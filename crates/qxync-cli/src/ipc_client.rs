//! CLI 侧的 IPC 客户端：一行 JSON 请求 → 一行 JSON 响应。
//!
//! 不引入额外依赖（tokio 的 UnixStream + 手动行帧），超时按请求类型区分。
//! ★ M26：[`call_with_progress`] 在此之上支持「进度行混发」—— 请求信封声明
//! `progress: true` 后，daemon 会在长任务期间往同一条 socket 推进度行，
//! 最终响应仍是最后一行（老 daemon 不推进度行，这条路自动退化为普通调用）。

use qxync_core::ipc::{
    conn_line_from_line, decode_line, encode_line, ErrorKind, IpcError, ProgressData, Request,
    RequestEnvelope, Response,
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

/// ★ M26：带进度行的调用（CLI 的 `get` 用）。
///
/// `want_progress` 为 true 时请求信封带 `progress: true`，读行循环把进度行喂给
/// `on_progress`，读到最终 [`Response`] 才返回；为 false 时发普通信封（daemon
/// 不推进度行，等价于 [`call`]，但读循环同样兼容万一混进来的进度行）。
/// **超时按行重置** —— 有进度行流动就说明 daemon 活着在干活，不该被 600s 总闸掐死。
pub async fn call_with_progress<T: serde::de::DeserializeOwned>(
    socket: &Path,
    req: Request,
    want_progress: bool,
    mut on_progress: impl FnMut(ProgressData),
) -> Result<T, IpcError> {
    let timeout = timeout_for(&req);
    let stream = UnixStream::connect(socket).await.map_err(|e| {
        IpcError::new(
            ErrorKind::NotRunning,
            format!("连不上 daemon（{}）: {e}", socket.display()),
        )
    })?;
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    let envelope = if want_progress {
        RequestEnvelope::with_progress(req)
    } else {
        RequestEnvelope::new(req)
    };
    let payload =
        encode_line(&envelope).map_err(|e| IpcError::new(ErrorKind::BadRequest, e.to_string()))?;
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
        let resp: Response = decode_line(line.as_bytes())
            .map_err(|e| IpcError::new(ErrorKind::Parse, format!("响应解析失败: {e}")))?;
        return resp.into_result::<T>();
    }
}
