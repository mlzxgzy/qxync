//! 服务端业务状态码。
//!
//! 报告里没有码表，以下语义是 2026-09-30 对真机（QTS 5.2.9 / Qsync QPKG 20260723）
//! 逐个端点实测得到的；未知值一律按「失败」处理但保留原值，方便边跑边补。

use serde::{Deserialize, Serialize};

/// JSON/XML 顶层 `status` 字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ServerStatus(pub i64);

impl ServerStatus {
    /// 常规成功。
    pub const OK: i64 = 0;
    /// 部分写操作（`createdir`、`qbox_check_permission`）成功时回 1。
    pub const OK_VARIANT: i64 = 1;

    pub fn is_success(self) -> bool {
        matches!(self.0, Self::OK | Self::OK_VARIANT)
    }

    /// ★ 服务端「未就绪」码。
    ///
    /// 逆向证据（`qsyncsrv.cgi` 统一错误出口 @0x15853，全库唯一带 `msg` 的
    /// JSON 模板 `{"version":"%s","build":"%s","status":%d,"success":"true","msg":"%s"}`）：
    /// ```text
    /// readiness fail: qbox.enable missing, user=%s
    /// msg = "Qsync Central is initializing. Please wait a few minutes and try again."
    /// ```
    /// → 这是服务端 `qbox_*` 私有路径上**唯一确认**的业务 status。
    /// 不是会话问题（重登无用），只能等 `qsyncsrv_metad` / `qsyncsrvd` 就绪。
    pub const SERVER_BUSY: i64 = 8;

    /// 这个 status 是否代表「服务端还在初始化，等一会儿就好」。
    ///
    /// 注意与 [`Error::is_server_busy`](crate::error::Error::is_server_busy) 的分工：
    /// 这里按**码值**判（`status.rs` 自己的语义），那里按 `msg` **文本**判
    /// （因为 `max_log` 成功时压根没有 `status` 字段，只有失败出口才有）。
    /// 两个都返回 false 时不等于"服务端一定就绪"，只表示"没有未就绪的信号"。
    pub fn is_server_busy(self) -> bool {
        self.0 == Self::SERVER_BUSY
    }

    /// 人类可读的语义（未知值给中性描述）。
    pub fn meaning(self) -> &'static str {
        match self.0 {
            0 => "成功",
            1 => "成功（该端点的成功变体）",
            2 => "源不存在 / 参数不适用（实测：rename 源缺失、FileStation rename 拒绝）",
            4 => "路径不存在 / 无权限",
            5 => "路径不存在 / 无权限",
            6 => "路径不存在",
            8 => "服务端未就绪（Qsync Central 正在初始化，等几分钟再试；重登无用）",
            20 => "被拒绝：qsyncsrv 下载需要 Qsync 同步文件夹会话；FileStation 上传/下载权限不足",
            33 => "目标不可写 / 参数不适用（实测：utilRequest createdir 落到了错误的命名空间）",
            -17 => "同步日志区间无效或日志库为空",
            -50 => "Qsync 层登录失败（本机实测恒出现；只读端点可直接用 QTS sid）",
            _ => "未知状态码（按失败处理）",
        }
    }
}

impl std::fmt::Display for ServerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "status={} ({})", self.0, self.meaning())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_variants() {
        assert!(ServerStatus(0).is_success());
        assert!(ServerStatus(1).is_success());
        assert!(!ServerStatus(20).is_success());
        assert!(!ServerStatus(-50).is_success());
    }

    #[test]
    fn display_contains_meaning() {
        assert!(ServerStatus(20).to_string().contains("同步文件夹会话"));
        assert!(ServerStatus(8).to_string().contains("未就绪"));
    }

    /// status=8 走的是「等一等」，绝不能混进登录失败的重登路径。
    #[test]
    fn server_busy_is_not_a_failure_to_relogin() {
        assert!(ServerStatus(8).is_server_busy());
        assert!(!ServerStatus(8).is_success());
        assert!(!ServerStatus(5).is_server_busy());
    }
}
