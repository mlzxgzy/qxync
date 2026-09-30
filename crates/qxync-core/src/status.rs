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

    /// 人类可读的语义（未知值给中性描述）。
    pub fn meaning(self) -> &'static str {
        match self.0 {
            0 => "成功",
            1 => "成功（该端点的成功变体）",
            4 => "路径不存在 / 无权限",
            5 => "路径不存在 / 无权限",
            6 => "路径不存在",
            20 => "被拒绝：qsyncsrv 下载需要 Qsync 同步文件夹会话；FileStation 上传/下载权限不足",
            33 => "目标不可写 / 参数不适用",
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
    }
}
