//! # M3：脱水（dehydrate）的**纯策略层**
//!
//! 脱水 = 把本地已水合的缓存内容丢掉、只留占位符（远端数据不动），用来把磁盘占用降下来。
//! 报告 12 §8 说得最重：**「整个系统最危险的操作」** —— 做错的表现是应用读到 0 或崩溃。
//!
//! 所以这里不碰 IO，只做两件事，且全部可离线单测：
//!
//! 1. [`eligible`]：脱水前的**完整安全检查链**（报告 12 §8.1）
//!    * pin = `pinned` / `excluded` → 不脱水
//!    * 有本地未上传改动 / 队列里还有作业 → 不脱水
//!    * 有打开的 fd（`open_count > 0`）→ 不脱水
//!    * 有 mmap（`/proc/*/maps` 扫描结果）→ 不脱水
//!    * 正在水合/正在读写（在途）→ 不脱水
//!    * 刚刚访问过（默认 300s 内）→ 不脱水（避免「刚 cat 完就被清」）
//!    * 目录 / 没有缓存内容 → 无事可做
//! 2. [`plan`]：批量脱水的**顺序与额度**（LRU：最久没访问的先清；`disk_percent`/限额算到不超限为止）。
//!
//! **顺序铁则（本模块只保证「谁能脱水」，顺序在 fuse 侧执行）**：
//! `inval_inode(0,0)` → 再清内容 → 再更新占位符状态。反了 = 内核 page cache 里的旧页
//! 会让应用读到假数据（报告 12 §8.2）。

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// 一个**可脱水候选**（fuse 侧从节点表导出）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub remote: String,
    pub is_dir: bool,
    /// 文件真实大小（占位符仍显示这个大小）。
    pub size: u64,
    /// 本地已缓存字节数（= 已就绪区间之和）。
    pub hydrated_bytes: u64,
    /// 本地有未上传改动。
    pub dirty: bool,
    /// 上传队列里还有该路径的作业（含在途）。
    pub pending_upload: bool,
    /// 当前打开的 fd 数。
    pub open_count: u32,
    /// 正在水合/读写（节点操作锁被占用）。
    pub in_flight: bool,
    /// pin 状态：`pinned` / `unpinned` / `unspecified` / `excluded`。
    pub pin: String,
    /// 最后一次读/写的时间（epoch 秒）。
    pub last_access: u64,
}

/// 不能脱水的原因（也是 `status` / `--dry-run` 的解释）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Block {
    NotAFile,
    NoContent,
    Dirty,
    PendingUpload,
    Pinned,
    Excluded,
    Open,
    Mapped,
    InFlight,
    Recent,
}

impl Block {
    pub fn reason(self) -> &'static str {
        match self {
            Block::NotAFile => "不是普通文件",
            Block::NoContent => "没有本地缓存内容（本来就是占位符）",
            Block::Dirty => "有未上传的本地改动",
            Block::PendingUpload => "上传队列里还有该文件的作业",
            Block::Pinned => "pin=pinned（钉住不释放）",
            Block::Excluded => "pin=excluded（永不脱水）",
            Block::Open => "还有打开的 fd",
            Block::Mapped => "有进程 mmap 了它",
            Block::InFlight => "正在水合/读写",
            Block::Recent => "刚访问过（保护窗口内）",
        }
    }
}

/// 脱水策略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// 只脱水「闲置 ≥ N 秒」的文件（0 = 不限制）。
    pub idle_secs: u64,
    /// 缓存限额（字节）：超过就按 LRU 清到不超限（`None` = 不按限额）。
    pub cache_limit: Option<u64>,
    /// 「刚访问过」的保护窗口（秒）：默认 300（报告 12 §8.1 的 `recently_modified(300)`）。
    pub recent_secs: u64,
    /// 当前时间（epoch 秒）。
    pub now: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            idle_secs: 0,
            cache_limit: None,
            recent_secs: 300,
            now: 0,
        }
    }
}

impl Policy {
    /// 常用组合：手动清一个文件（不限闲置、不看限额，但仍受安全检查约束）。
    pub fn manual(now: u64) -> Self {
        Self {
            idle_secs: 0,
            cache_limit: None,
            recent_secs: 0,
            now,
        }
    }

    /// 后台按「闲置时间 + 限额」扫。
    pub fn sweeper(idle_secs: u64, cache_limit: Option<u64>, now: u64) -> Self {
        Self {
            idle_secs,
            cache_limit,
            recent_secs: 300,
            now,
        }
    }
}

/// 单个候选的安全检查（`mapped` = 该路径被某个进程 mmap 了）。
pub fn eligible(c: &Candidate, policy: &Policy, mapped: bool) -> Result<(), Block> {
    if c.is_dir {
        return Err(Block::NotAFile);
    }
    if c.hydrated_bytes == 0 {
        return Err(Block::NoContent);
    }
    match c.pin.as_str() {
        "pinned" => return Err(Block::Pinned),
        "excluded" => return Err(Block::Excluded),
        _ => {}
    }
    if c.dirty {
        return Err(Block::Dirty);
    }
    if c.pending_upload {
        return Err(Block::PendingUpload);
    }
    if c.open_count > 0 {
        return Err(Block::Open);
    }
    if mapped {
        return Err(Block::Mapped);
    }
    if c.in_flight {
        return Err(Block::InFlight);
    }
    // 「刚访问过」保护窗口
    if policy.recent_secs > 0 && policy.now.saturating_sub(c.last_access) < policy.recent_secs {
        return Err(Block::Recent);
    }
    if policy.idle_secs > 0 && policy.now.saturating_sub(c.last_access) < policy.idle_secs {
        return Err(Block::Recent);
    }
    Ok(())
}

/// 一次批量脱水的计划。
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// 要脱水的候选（按「最久没访问」优先排序）。
    pub targets: Vec<Candidate>,
    /// 被安全检查挡下的（路径 → 原因），方便 `--dry-run` / `status` 解释。
    pub blocked: Vec<(String, Block)>,
    /// 本地缓存总字节（全部候选之和）。
    pub used_bytes: u64,
    /// 计划释放的字节。
    pub freed_bytes: u64,
    /// 限额（若设了）。
    pub limit_bytes: Option<u64>,
}

impl Plan {
    pub fn blocked_count(&self, b: Block) -> usize {
        self.blocked.iter().filter(|(_, x)| *x == b).count()
    }
}

/// 生成计划：
/// * 先过安全检查；
/// * 设了 `cache_limit` → 只清到 `used - limit` 为止（LRU）；
/// * 没设限额 → 所有通过的都清（手动 `--all` 语义）。
pub fn plan(cands: &[Candidate], policy: &Policy, mapped: &BTreeSet<String>) -> Plan {
    let mut out = Plan {
        limit_bytes: policy.cache_limit,
        ..Default::default()
    };
    let mut ok: Vec<Candidate> = Vec::new();
    for c in cands {
        out.used_bytes += c.hydrated_bytes;
        match eligible(c, policy, mapped.contains(&c.remote)) {
            Ok(()) => ok.push(c.clone()),
            Err(b) => out.blocked.push((c.remote.clone(), b)),
        }
    }
    // LRU：最久没访问的先清
    ok.sort_by_key(|c| (c.last_access, c.remote.clone()));

    let need = match policy.cache_limit {
        Some(limit) => out.used_bytes.saturating_sub(limit),
        None => u64::MAX,
    };
    for c in ok {
        if out.freed_bytes >= need {
            break;
        }
        out.freed_bytes += c.hydrated_bytes;
        out.targets.push(c);
    }
    out
}

/// 缓存限额的写法：`10G` / `512M` / `1.5G` / `25%`（占缓存所在文件系统的百分比）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CacheLimit {
    Bytes(u64),
    Percent(u8),
}

impl CacheLimit {
    /// 解析人类可读的限额。`None` = 无效写法。
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        if let Some(p) = s.strip_suffix('%') {
            let v: u8 = p.trim().parse().ok()?;
            return (v > 0 && v <= 100).then_some(CacheLimit::Percent(v));
        }
        let upper = s.to_ascii_uppercase();
        let split = upper
            .find(|c: char| c.is_ascii_alphabetic())
            .unwrap_or(upper.len());
        let (num, unit) = upper.split_at(split);
        let mult: u64 = match unit.trim() {
            "" | "B" => 1,
            "K" | "KB" | "KIB" => 1024,
            "M" | "MB" | "MIB" => 1024 * 1024,
            "G" | "GB" | "GIB" => 1024 * 1024 * 1024,
            "T" | "TB" | "TIB" => 1024 * 1024 * 1024 * 1024,
            _ => return None,
        };
        let v: f64 = num.trim().parse().ok()?;
        if v <= 0.0 {
            return None;
        }
        Some(CacheLimit::Bytes((v * mult as f64) as u64))
    }

    /// 换算成字节（百分比需要文件系统总大小）。
    pub fn bytes(self, fs_total: u64) -> u64 {
        match self {
            CacheLimit::Bytes(b) => b,
            CacheLimit::Percent(p) => fs_total / 100 * p as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(remote: &str, bytes: u64, last: u64) -> Candidate {
        Candidate {
            remote: remote.into(),
            is_dir: false,
            size: bytes,
            hydrated_bytes: bytes,
            dirty: false,
            pending_upload: false,
            open_count: 0,
            in_flight: false,
            pin: "unspecified".into(),
            last_access: last,
        }
    }

    #[test]
    fn safety_chain_blocks_every_dangerous_case() {
        let p = Policy {
            now: 10_000,
            recent_secs: 300,
            ..Default::default()
        };
        let ok = cand("/home/a", 100, 0);
        assert_eq!(eligible(&ok, &p, false), Ok(()));

        let mut c = ok.clone();
        c.pin = "pinned".into();
        assert_eq!(eligible(&c, &p, false), Err(Block::Pinned));
        c.pin = "excluded".into();
        assert_eq!(eligible(&c, &p, false), Err(Block::Excluded));
        c.pin = "unpinned".into();
        assert_eq!(eligible(&c, &p, false), Ok(()));

        let mut c = ok.clone();
        c.dirty = true;
        assert_eq!(eligible(&c, &p, false), Err(Block::Dirty));
        let mut c = ok.clone();
        c.pending_upload = true;
        assert_eq!(eligible(&c, &p, false), Err(Block::PendingUpload));
        let mut c = ok.clone();
        c.open_count = 1;
        assert_eq!(eligible(&c, &p, false), Err(Block::Open));
        let mut c = ok.clone();
        c.in_flight = true;
        assert_eq!(eligible(&c, &p, false), Err(Block::InFlight));
        // mmap
        assert_eq!(eligible(&ok, &p, true), Err(Block::Mapped));
        // 目录 / 没内容
        let mut c = ok.clone();
        c.is_dir = true;
        assert_eq!(eligible(&c, &p, false), Err(Block::NotAFile));
        let mut c = ok.clone();
        c.hydrated_bytes = 0;
        assert_eq!(eligible(&c, &p, false), Err(Block::NoContent));
        // 刚访问过（保护窗口 300s）
        let fresh = cand("/home/fresh", 100, 9_900);
        assert_eq!(eligible(&fresh, &p, false), Err(Block::Recent));
        // 手动模式不看保护窗口
        let manual = Policy::manual(10_000);
        assert_eq!(eligible(&fresh, &manual, false), Ok(()));
    }

    #[test]
    fn idle_policy_only_takes_older_files() {
        let p = Policy::sweeper(600, None, 10_000);
        let old = cand("/home/old", 100, 9_000);
        let fresh = cand("/home/fresh", 100, 9_800);
        let got = plan(&[old, fresh], &p, &BTreeSet::new());
        assert_eq!(got.targets.len(), 1);
        assert_eq!(got.targets[0].remote, "/home/old");
        assert_eq!(got.blocked.len(), 1);
        assert_eq!(got.blocked[0].1, Block::Recent);
    }

    #[test]
    fn limit_evicts_lru_until_under_limit() {
        let cands = vec![
            cand("/home/a", 400, 1000),
            cand("/home/b", 400, 2000),
            cand("/home/c", 400, 3000),
        ];
        let p = Policy {
            idle_secs: 0,
            cache_limit: Some(500),
            recent_secs: 0,
            now: 10_000,
        };
        let got = plan(&cands, &p, &BTreeSet::new());
        assert_eq!(got.used_bytes, 1200);
        // 需要释放 700 → 先清 a(400) 再清 b(400)，c 保留
        assert_eq!(
            got.targets
                .iter()
                .map(|c| c.remote.as_str())
                .collect::<Vec<_>>(),
            vec!["/home/a", "/home/b"]
        );
        assert_eq!(got.freed_bytes, 800);
        assert!(got.freed_bytes >= 700);
        // 已经低于限额 → 什么都不做
        let p2 = Policy {
            cache_limit: Some(4096),
            ..p
        };
        assert!(plan(&cands, &p2, &BTreeSet::new()).targets.is_empty());
    }

    #[test]
    fn mapped_paths_are_reported_not_taken() {
        let cands = vec![cand("/home/a", 100, 0), cand("/home/b", 100, 0)];
        let mapped: BTreeSet<String> = ["/home/b".to_string()].into_iter().collect();
        let p = Policy::manual(10_000);
        let got = plan(&cands, &p, &mapped);
        assert_eq!(got.targets.len(), 1);
        assert_eq!(got.targets[0].remote, "/home/a");
        assert_eq!(got.blocked_count(Block::Mapped), 1);
    }

    #[test]
    fn cache_limit_parsing() {
        assert_eq!(
            CacheLimit::parse("512M"),
            Some(CacheLimit::Bytes(512 * 1024 * 1024))
        );
        assert_eq!(
            CacheLimit::parse("1.5G"),
            Some(CacheLimit::Bytes(1610612736))
        );
        assert_eq!(
            CacheLimit::parse("10GiB"),
            Some(CacheLimit::Bytes(10 * 1024 * 1024 * 1024))
        );
        assert_eq!(CacheLimit::parse("2048"), Some(CacheLimit::Bytes(2048)));
        assert_eq!(CacheLimit::parse("25%"), Some(CacheLimit::Percent(25)));
        assert_eq!(CacheLimit::parse("0%"), None);
        assert_eq!(CacheLimit::parse("120%"), None);
        assert_eq!(CacheLimit::parse("abc"), None);
        assert_eq!(CacheLimit::parse(""), None);
        assert_eq!(CacheLimit::Percent(25).bytes(1000), 250);
        assert_eq!(CacheLimit::Bytes(42).bytes(1000), 42);
    }
}
