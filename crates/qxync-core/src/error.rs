//! 统一错误类型（与具体 HTTP 库解耦，方便 core 保持零运行时依赖）。

use crate::status::ServerStatus;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 网络层失败（连接、TLS、超时）。
    #[error("网络错误: {0}")]
    Transport(String),

    /// 服务端返回了非成功 status。
    ///
    /// ★ `msg` 是服务端在**统一错误出口**里给的原文（例如 status=8 时的
    /// `"Qsync Central is initializing. Please wait a few minutes and try again."`）。
    /// 服务端明明说了原因，客户端过去把它整个丢掉 → 用户只看到「未知状态码」。
    /// 现在原样透传，缺省为 `None`（File Station 一侧的响应多不带 `msg`）。
    #[error("{context}: {status}{}", render_msg(.msg))]
    Status {
        status: ServerStatus,
        context: String,
        msg: Option<String>,
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
            msg: None,
        }
    }

    /// 带服务端 `msg` 原文的错误。
    pub fn status_msg(status: i64, context: impl Into<String>, msg: impl Into<String>) -> Self {
        let msg = msg.into();
        Error::Status {
            status: ServerStatus(status),
            context: context.into(),
            msg: if msg.trim().is_empty() {
                None
            } else {
                Some(msg)
            },
        }
    }

    /// 服务端给的 `msg` 原文（没有则 `None`）。
    pub fn server_msg(&self) -> Option<&str> {
        match self {
            Error::Status { msg, .. } => msg.as_deref(),
            _ => None,
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

    /// ★ 服务端**未就绪**（status=8）。
    ///
    /// 逆向结论（服务端 `qsyncsrv.cgi` 统一错误出口 @0x15853，全库唯一带 `msg` 的模板
    /// `{"version":"%s","build":"%s","status":%d,"success":"true","msg":"%s"}`）：
    /// ```text
    /// read readiness fail: qbox.enable missing, user=%s
    /// msg = "Qsync Central is initializing. Please wait a few minutes and try again."
    /// ```
    /// 触发条件是 `Is_Qbox_File_Flag_Enabled()` 为假 —— 即 `/var/qfunc/qbox.enable`
    /// 未就绪：正在安装 / 迁移 / 恢复 / 备份还原 / 守护进程未起齐。
    ///
    /// 这**不是**错误，重登 sid 也没用，只能等。按 `msg` 文本判定而不是写死 8，
    /// 是为了将来服务端换码值时依然认得出（8 是当前唯一确认的业务 status）。
    pub fn is_server_busy(&self) -> bool {
        match self {
            Error::Status { msg, .. } => msg.as_deref().is_some_and(|m| {
                let m = m.to_ascii_lowercase();
                m.contains("initializing") || m.contains("please wait")
            }),
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

/// `thiserror` 的 `#[error]` 里用来拼 `msg` 后缀（没有 msg 就不产生任何字符）。
fn render_msg(msg: &Option<String>) -> String {
    match msg {
        Some(m) if !m.trim().is_empty() => format!(" — 服务端 msg: {m}"),
        _ => String::new(),
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

    /// ★ 服务端未就绪（status=8）既不是登录失败、也不该按普通错误重试。
    #[test]
    fn server_busy_is_distinguished_from_auth() {
        let e = Error::status_msg(
            8,
            "qbox_get_max_log",
            "Qsync Central is initializing. Please wait a few minutes and try again.",
        );
        assert!(e.is_server_busy());
        // 关键：status=8 不能被判成「会话失效」，否则会做无用重登
        assert!(!e.is_auth());
        // msg 必须出现在 Display 里，否则排错时看不到根因
        assert!(e.to_string().contains("initializing"), "{e}");

        // 判据基于 msg 文本而非硬编码码值 —— 服务端换码仍认得出
        let moved = Error::status_msg(77, "x", "Qsync Central is initializing.");
        assert!(moved.is_server_busy());

        // 普通错误不能误判成「服务端忙」
        assert!(!Error::status(5, "get_list").is_server_busy());
        assert!(!Error::Transport("超时".into()).is_server_busy());
        assert!(!Error::Auth("口令不对".into()).is_server_busy());
    }

    /// 没有 msg 时 Display 不留空后缀，且 `server_msg()` 返回 None。
    #[test]
    fn missing_msg_is_clean() {
        let e = Error::status(5, "get_list");
        assert_eq!(e.server_msg(), None);
        assert_eq!(e.to_string(), "get_list: status=5 (路径不存在 / 无权限)");

        // 空串 msg 归一成 None，不产生「— 服务端 msg: 」这种尾巴
        let blank = Error::status_msg(5, "get_list", "   ");
        assert_eq!(blank.server_msg(), None);
        assert_eq!(blank.to_string(), e.to_string());
    }
}
