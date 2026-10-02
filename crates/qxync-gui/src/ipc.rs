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
            format!(
                "连不上 daemon（{}）: {e}；先 `qxync daemon start`",
                socket.display()
            ),
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
