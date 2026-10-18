//! # 回收站路径判定（段级精确匹配）
//!
//! 同步引擎和 FUSE 层用同一套「什么名字算回收站」的语义。这里把它收到 core，
//! 是因为**两条路径必须一致**（缺一边就会出真实问题）：
//!
//! * FUSE 侧（`qxync-fuse` 的 `deny_trash`）已经把这些段当硬约束：不允许经挂载点
//!   在回收站里创建/删除/改名 —— 否则 KDE 的 FDO 回收站会把文件「搬」进挂载点，
//!   内容最终落到 NAS 上。
//! * 同步引擎侧**原先没有任何排除**：`/home/@Recycle` 子树被当成用户数据逐目录
//!   reconcile、进 baseline。真机取证：某用户 baseline 1991 条里 **1425 条**在
//!   `@Recycle` 下、13 条在 `.Trash-1000` 下；QTS 清理回收站时这些条目会变成批量
//!   「远端删除」候选（被删除熔断挡住 = 每轮刷屏），而且 FDO 的 `.trashinfo`
//!   会被反向同步上 NAS。
//!
//! ## 认哪些名字（刻意保守）
//!
//! **段级精确匹配**，不做前缀/子串/大小写模糊：
//!
//! * `.Trash` —— FDO 规范的卷内共享回收站；
//! * `.Trash-<uid>` —— FDO 规范的用户回收站，**只认调用方传入的那个 uid**
//!   （FUSE 侧传的是本进程 uid；同步引擎也传本机 uid，两边口径一致）；
//! * `@Recycle` —— QNAP 自家回收站目录（`@` 是 QTS 的隐藏标记前缀）。
//!
//! 反例（必须**不**命中）：`.recent`、`.trash`（小写）、`Trash`、`@Recycled`、
//! `a.Trash-1000.txt`、`.Trash-`、`.Trash-abc`、别人的 `.Trash-1001`。
//!
//! 为什么不做子串匹配：回收站排除是「跳过整个子树」的粗粒度操作，误伤一个正常
//! 文件 = 它从此不再同步（用户看到的是「我的文件不见了」），比漏排除危险得多。
//! 漏掉一个自定义命名的回收站只是回到旧行为（多同步一点垃圾）。

/// 这个路径**段**是不是回收站目录名。
///
/// `uid` 是调用方认为「当前用户」的数字 uid（与 FUSE 侧 `libc::getuid()` 同口径）。
pub fn is_trash_segment(seg: &str, uid: u32) -> bool {
    if seg == ".Trash" || seg == "@Recycle" {
        return true;
    }
    if let Some(rest) = seg.strip_prefix(".Trash-") {
        // `.Trash-1000` / `.Trash-0`：纯数字且等于当前 uid（别人的回收站不碰）
        return rest.parse::<u32>().map(|u| u == uid).unwrap_or(false);
    }
    false
}

/// 路径的**任一段**是回收站 → 整条路径都在回收站子树里。
///
/// 绝对路径开头会出现空段，跳过；`/home/@Recycle2` 这类不做前缀匹配（见模块文档）。
pub fn is_trash_path(path: &str, uid: u32) -> bool {
    path.split('/')
        .filter(|s| !s.is_empty())
        .any(|seg| is_trash_segment(seg, uid))
}

#[cfg(test)]
mod tests {
    use super::*;

    const UID: u32 = 1000;

    /// 段级判定的正例/反例表 —— 反例比正例重要：误伤一个正常目录 = 它不再同步。
    #[test]
    fn trash_segment_recognition() {
        // 正例
        assert!(is_trash_segment(".Trash", UID));
        assert!(is_trash_segment(&format!(".Trash-{UID}"), UID));
        assert!(is_trash_segment("@Recycle", UID));
        assert!(is_trash_segment(".Trash-0", 0), "uid 0 的回收站也要认");

        // 反例（不能误伤正常名字）
        assert!(!is_trash_segment(".recent", UID), ".recent 是正常目录");
        assert!(!is_trash_segment(".trash", UID), "大小写敏感");
        assert!(!is_trash_segment("Trash", UID));
        assert!(!is_trash_segment("@Recycled", UID), "不做前缀匹配");
        assert!(!is_trash_segment("a.Trash-1000", UID), "不做子串匹配");
        assert!(!is_trash_segment("my.Trash-1000.txt", UID));
        assert!(!is_trash_segment(".Trash-", UID), "空后缀不是数字");
        assert!(!is_trash_segment(".Trash-abc", UID), "非数字后缀");
        assert!(
            !is_trash_segment(&format!(".Trash-{}", UID + 1), UID),
            "别人的 uid 不管"
        );
        assert!(!is_trash_segment("", UID));
        assert!(!is_trash_segment("/", UID));
    }

    /// 整条路径：**任一段**命中即回收站子树；根与正常子树不受影响。
    #[test]
    fn trash_path_walks_every_segment() {
        // 正例
        assert!(is_trash_path(&format!("/home/@Recycle"), UID));
        assert!(is_trash_path(&format!("/home/@Recycle/files/x.txt"), UID));
        assert!(is_trash_path(&format!("/home/.Trash-{UID}/files/a"), UID));
        assert!(is_trash_path(
            &format!("/home/.Trash/info/a.trashinfo"),
            UID
        ));
        assert!(is_trash_path(&format!("@Recycle/x"), UID), "相对路径也认");

        // 反例
        assert!(!is_trash_path("/home", UID));
        assert!(!is_trash_path("/home/qxync-test/hello.txt", UID));
        assert!(!is_trash_path("/home/.recent", UID));
        assert!(!is_trash_path("/home/@Recycled", UID));
        assert!(!is_trash_path("/home/a.Trash-1000.txt", UID));
        assert!(
            !is_trash_path(&format!("/home/.Trash-{}", UID + 1), UID),
            "别人的回收站不管（与 FUSE 的 deny_trash 口径一致）"
        );
        assert!(!is_trash_path("", UID));
    }
}
