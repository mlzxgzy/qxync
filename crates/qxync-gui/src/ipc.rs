//! GUI ↔ `qxyncd` 的本地 IPC 客户端（unix socket + 一行一个 JSON）。
//!
//! GUI **不直连 NAS**：和 CLI 一样走 daemon（见 `docs/M1.5-设计.md`）。
//! 这里只做「请求 → 响应信封」的搬运，契约类型全部复用 `qxync-core::ipc`。

use qxync_core::ipc::{
    conn_line_from_line, decode_line, encode_line, ErrorKind, IpcError, ProgressData, Request,
    RequestEnvelope, Response,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// 默认连接的 socket 路径（兼容旧调用）。
pub fn socket_path() -> PathBuf {
    socket_path_for("default")
}

/// ★ M16：某个连接的 socket 路径（default 保持旧行为，其它连接同目录派生）。
pub fn socket_path_for(link_id: &str) -> PathBuf {
    qxync_core::ipc::socket_path_for(link_id)
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

/// ★ M26：带进度行的调用（GUI 的「下载到本地」用）。
///
/// 信封声明 `progress: true`，daemon 在长任务期间往同一条 socket 推进度行；
/// 每行喂给 `on_progress`，读到最终 [`Response`] 才返回。**超时按行重置** ——
/// 有进度行流动就说明 daemon 活着在干活，不该被 600s 总闸掐死。
/// 老 daemon 不认识 `progress` 字段（serde 宽容忽略）→ 一个进度行都不发，
/// 这条路自动退化成普通调用。
pub async fn call_with_progress(
    socket: &Path,
    req: Request,
    mut on_progress: impl FnMut(ProgressData),
) -> Result<Response, IpcError> {
    let timeout = timeout_for(&req);
    let stream = UnixStream::connect(socket).await.map_err(|e| {
        IpcError::new(
            ErrorKind::NotRunning,
            not_running_message(socket, &e.to_string()),
        )
    })?;
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    let payload = encode_line(&RequestEnvelope::with_progress(req))
        .map_err(|e| IpcError::new(ErrorKind::BadRequest, e.to_string()))?;
    wr.write_all(&payload)
        .await
        .map_err(|e| IpcError::new(ErrorKind::Transport, format!("写 socket 失败: {e}")))?;
    loop {
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
        // 进度行 → 喂回调继续读；最终响应 → 解出返回
        if let Some(p) = conn_line_from_line(line.as_bytes())
            .map_err(|e| IpcError::new(ErrorKind::Parse, format!("响应解析失败: {e}")))?
        {
            on_progress(p);
            continue;
        }
        return decode_line(line.as_bytes())
            .map_err(|e| IpcError::new(ErrorKind::Parse, format!("响应解析失败: {e}")));
    }
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

    /// ★ M26：`call_with_progress` 的读循环 —— 进度行按序喂给回调、最终响应收尾，
    /// 且信封必须声明 `progress:true`（daemon 靠这个字段决定推不推，这里守新客户端
    /// 的声明义务；漏了它 GUI 就永远收不到进度）。
    #[tokio::test]
    async fn progress_lines_flow_and_envelope_declares_progress() {
        use qxync_core::ipc::{ProgressLine, IPC_VERSION};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let socket =
            std::env::temp_dir().join(format!("qxync-gui-m26-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();

        // 假 daemon：读请求 → 断言声明了 progress → 推两条进度行 → 回最终响应
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (rd, mut wr) = stream.into_split();
            let mut lines = BufReader::new(rd).lines();
            let req_line = lines.next_line().await.unwrap().unwrap();
            assert!(
                req_line.contains("\"progress\":true"),
                "信封未声明 progress:true：{req_line}"
            );
            let p1 = ProgressLine {
                v: IPC_VERSION,
                progress: ProgressData {
                    written: 100,
                    total: Some(400),
                },
            };
            let p2 = ProgressLine {
                v: IPC_VERSION,
                progress: ProgressData {
                    written: 300,
                    total: Some(400),
                },
            };
            wr.write_all(&encode_line(&p1).unwrap()).await.unwrap();
            wr.write_all(&encode_line(&p2).unwrap()).await.unwrap();
            let resp = Response::ok(serde_json::json!({ "bytes": 400 }));
            wr.write_all(&encode_line(&resp).unwrap()).await.unwrap();
        });

        let mut seen: Vec<(u64, Option<u64>)> = Vec::new();
        let resp = call_with_progress(&socket, Request::Ping, |p| {
            seen.push((p.written, p.total));
        })
        .await
        .expect("call_with_progress 应成功");
        server.await.unwrap();

        assert_eq!(
            seen,
            vec![(100, Some(400)), (300, Some(400))],
            "进度行应按序到达回调"
        );
        assert!(resp.ok, "最终响应应成功：{resp:?}");
        let _ = std::fs::remove_file(&socket);
    }
}
