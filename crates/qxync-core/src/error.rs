//! 统一错误类型（与具体 HTTP 库解耦，方便 core 保持零运行时依赖）。

use crate::status::ServerStatus;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 网络层失败（连接、TLS、超时）。
    #[error("网络错误: {0}")]
    Transport(String),

    /// 服务端返回了非成功 status。
    #[error("{context}: {status}")]
    Status {
        status: ServerStatus,
        context: String,
    },

    /// 响应不是预期的 JSON/XML。
    #[error("响应解析失败: {0}")]
    Parse(String),

    /// 登录/会话失败。
    #[error("鉴权失败: {0}")]
    Auth(String),

    /// 本地文件系统。
    #[error("本地 IO 失败: {0}")]
    Io(String),

    /// 本地状态库（SQLite）。
    #[error("状态库失败: {0}")]
    Db(String),

    /// 协议/端点在本机不支持。
    #[error("协议不支持: {0}")]
    Unsupported(String),
}

impl Error {
    pub fn status(status: i64, context: impl Into<String>) -> Self {
        Error::Status {
            status: ServerStatus(status),
            context: context.into(),
        }
    }

    /// ★ M10：是不是「会话没了」这一类错误（登录失败 / HTTP 鉴权失败 / 服务端回 4、5 号状态）。
    ///
    /// 调用方据此决定要不要**重登一次再重试**。判据原先是 daemon 里的一个私有函数，
    /// 现在收到 core：client、daemon、FUSE 三层用的是同一套判定。
    pub fn is_auth(&self) -> bool {
        match self {
            Error::Auth(_) => true,
            Error::Status { status, .. } => matches!(status.0, 4 | 5),
            _ => false,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Parse(e.to_string())
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ M10：三层（client / daemon / FUSE）判断「会话失效」用的是同一个判据。
    #[test]
    fn auth_classification_covers_status_4_and_5() {
        assert!(Error::Auth("口令不对".into()).is_auth());
        assert!(Error::status(4, "get_list").is_auth());
        assert!(Error::status(5, "stat").is_auth());
        assert!(!Error::status(-17, "qbox_get_sync_log").is_auth());
        assert!(!Error::Transport("连接超时".into()).is_auth());
        assert!(!Error::Parse("坏 JSON".into()).is_auth());
    }
}
