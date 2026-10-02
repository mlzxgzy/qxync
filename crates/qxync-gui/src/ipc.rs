//! GUI ↔ `qxyncd` 的本地 IPC 客户端（unix socket + 一行一个 JSON）。
//!
//! GUI **不直连 NAS**：和 CLI 一样走 daemon（见 `docs/M1.5-设计.md`）。
//! 这里只做「请求 → 响应信封」的搬运，契约类型全部复用 `qxync-core::ipc`。

use qxync_core::ipc::{
    decode_line, default_socket_path, encode_line, ErrorKind, IpcError, Request, RequestEnvelope,
    Response,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// socket 路径：`QXNYC_SOCKET` 优先（测试/多实例），否则 `$XDG_RUNTIME_DIR/qxync/qxyncd.sock`。
pub fn socket_path() -> PathBuf {
    std::env::var_os("QXNYC_SOCKET")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_socket_path)
}

/// 超时：普通请求 30s；长任务（mount/get/put/sync/dehydrate）600s（与 CLI 一致）。
pub fn timeout_for(req: &Request) -> Duration {
    if req.is_long_running() {
        Duration::from_secs(600)
    } else {
        Duration::from_secs(30)
    }
}

/// socket 是否可连（探测 daemon 是否在跑；快速失败）。
pub async fn available(socket: &Path) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_millis(500), UnixStream::connect(socket)).await,
        Ok(Ok(_))
    )
}

/// ★ 连不上 daemon 时该给的**下一步**。
///
/// 不能一律说「先 `qxync daemon start`」：全新安装下压根还没有连接配置，而 `qxyncd`
/// 会在后台直接退出（fork 之后 fd 0/1/2 → `/dev/null`，错误只进日志文件），用户照着
/// 这句提示跑一遍也只会再撞一次墙。所以先看一眼有没有 link 配置，把「缺的是哪一步」说准。
fn not_running_message(socket: &Path, err: &str) -> String {
    hint(socket, err, has_any_link())
}

/// 有没有任何一份连接配置（`<config>/links/*.json`）。
fn has_any_link() -> bool {
    qxync_core::ConfigPaths::discover()
        .ok()
        .map(|p| {
            std::fs::read_dir(p.config_dir.join("links"))
                .map(|rd| {
                    rd.flatten().any(|e| {
                        e.path()
                            .extension()
                            .map(|x| x.eq_ignore_ascii_case("json"))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// 提示正文。把「有没有 link」当参数传进来，是为了能不起进程、不动环境变量地测它。
fn hint(socket: &Path, err: &str, has_link: bool) -> String {
    let base = format!("连不上 daemon（{}）: {err}", socket.display());
    if has_link {
        format!("{base}；先 `qxync daemon start`（或点界面上的「启动」）")
    } else {
        format!(
            "{base}；还没有配置 NAS 连接 —— 先在「设置 → 连接」里填好并保存\
             （或跑 `qxync --host <NAS地址> --port 9834 --insecure --user <用户> login`），\
             再启动 daemon"
        )
    }
}

/// 把错误包装成**响应信封**（前端只需要处理一种形状：`{v,ok,data,error}`）。
pub fn err_response(kind: ErrorKind, message: impl Into<String>) -> Value {
    serde_json::to_value(Response::err(kind, message))
        .unwrap_or_else(|e| serde_json::json!({ "v": 1, "ok": false, "error": { "kind": "parse", "message": e.to_string() } }))
}

async fn call_inner(socket: &Path, req: Request) -> Result<Response, IpcError> {
    let timeout = timeout_for(&req);
    let stream = UnixStream::connect(socket).await.map_err(|e| {
        IpcError::new(
            ErrorKind::NotRunning,
            not_running_message(socket, &e.to_string()),
        )
    })?;
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    let payload = encode_line(&RequestEnvelope::new(req))
        .map_err(|e| IpcError::new(ErrorKind::BadRequest, e.to_string()))?;
    wr.write_all(&payload)
        .await
        .map_err(|e| IpcError::new(ErrorKind::Transport, format!("写 socket 失败: {e}")))?;
    let line = tokio::time::timeout(timeout, lines.next_line())
        .await
        .map_err(|_| {
            IpcError::new(
                ErrorKind::Transport,
                format!("daemon 响应超时（{timeout:?}）"),
            )
        })?
        .map_err(|e| IpcError::new(ErrorKind::Transport, e.to_string()))?
        .ok_or_else(|| IpcError::new(ErrorKind::Transport, "daemon 提前关闭了连接"))?;
    decode_line(line.as_bytes())
        .map_err(|e| IpcError::new(ErrorKind::Parse, format!("响应解析失败: {e}")))
}

/// 发一个请求，**永远返回响应信封**（连不上也包装成 `ok:false`）。
pub async fn call(socket: &Path, req: Request) -> Value {
    match call_inner(socket, req).await {
        Ok(resp) => serde_json::to_value(resp)
            .unwrap_or_else(|e| err_response(ErrorKind::Parse, format!("序列化响应失败: {e}"))),
        Err(e) => err_response(e.kind, e.message),
    }
}

/// 发请求并解出 `data`（前端不方便直接用的强类型调用留给 Rust 侧）。
pub async fn call_typed<T: DeserializeOwned>(socket: &Path, req: Request) -> Result<T, IpcError> {
    call_inner(socket, req).await?.into_result::<T>()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 连不上 daemon 时，「下一步」必须指向**真正缺的那一步** —— 全新安装缺的是连接配置，
    /// 照「先 daemon start」去跑只会再撞一次墙（daemon 会在后台静默退出）。
    #[test]
    fn hint_points_at_the_step_that_is_actually_missing() {
        let socket = Path::new("/run/user/1000/qxync/qxyncd.sock");

        let with_link = hint(socket, "No such file or directory", true);
        assert!(with_link.contains("qxync daemon start"), "{with_link}");
        assert!(!with_link.contains("还没有配置 NAS 连接"), "{with_link}");

        let without_link = hint(socket, "No such file or directory", false);
        assert!(
            without_link.contains("还没有配置 NAS 连接"),
            "{without_link}"
        );
        assert!(without_link.contains("qxync --host"), "{without_link}");
        assert!(
            !without_link.contains("daemon start"),
            "缺连接配置时不该再让人去 start：{without_link}"
        );
    }
}
