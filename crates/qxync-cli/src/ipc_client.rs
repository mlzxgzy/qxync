//! CLI 侧的 IPC 客户端：一行 JSON 请求 → 一行 JSON 响应。
//!
//! 不引入额外依赖（tokio 的 UnixStream + 手动行帧），超时按请求类型区分。

use qxync_core::ipc::{
    decode_line, encode_line, ErrorKind, IpcError, Request, RequestEnvelope, Response,
};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// 默认超时：普通请求 30s，长任务（get/put/mount）600s。
pub fn timeout_for(req: &Request) -> Duration {
    if req.is_long_running() {
        Duration::from_secs(600)
    } else {
        Duration::from_secs(30)
    }
}

/// socket 是否可连（用于「自动走 daemon」的探测，快速失败）。
pub async fn available(socket: &Path) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_millis(500), UnixStream::connect(socket)).await,
        Ok(Ok(_))
    )
}

/// 发一个请求，拿回响应信封。
pub async fn send(socket: &Path, req: Request) -> Result<Response, IpcError> {
    let timeout = timeout_for(&req);
    let stream = UnixStream::connect(socket).await.map_err(|e| {
        IpcError::new(
            ErrorKind::NotRunning,
            format!("连不上 daemon（{}）: {e}", socket.display()),
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

/// 发请求并解出 `data`。
pub async fn call<T: serde::de::DeserializeOwned>(
    socket: &Path,
    req: Request,
) -> Result<T, IpcError> {
    send(socket, req).await?.into_result::<T>()
}

/// 只是想要成功/失败，不关心 data。
pub async fn call_ok(socket: &Path, req: Request) -> Result<(), IpcError> {
    let r = send(socket, req).await?;
    if r.ok {
        Ok(())
    } else {
        Err(r
            .error
            .unwrap_or_else(|| IpcError::new(ErrorKind::BadRequest, "未知错误")))
    }
}
