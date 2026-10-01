//! # M8.4：释放空间（`statvfs` + 自动触发）
//!
//! 对应 Qsync 设置页「释放空间」tab：
//!
//! ```text
//! ( ) 不自动释放
//! (•) 自动释放空间
//!       ( ) 当本地可用空间少于  [10]%
//!       (•) 按频率             [每天 ▾]
//! [立即释放空间]
//! ```
//!
//! 本模块只做两件事：**量空间**（`statvfs`，带验收注入点）和**算要不要跑**（纯函数）。
//! **真正脱水仍然走 `qxyncd` 里既有的 `run_dehydrate`** —— 也就是说 M3 的安全检查链
//! （dirty / pinned / open / mapped / in-flight … 分类拦下）**一行都没绕过**，
//! 这是方案 §8 风险 5 的硬约束。
//!
//! ## 验收注入点
//!
//! `QSYNC_TEST_FAKE_STATVFS` 让验收脚本把「剩余空间」打到 5% 而不必真的塞满磁盘：
//!
//! ```text
//! QSYNC_TEST_FAKE_STATVFS="avail_pct=5"
//! QSYNC_TEST_FAKE_STATVFS="total=100G,avail=5G,free=5G"
//! QSYNC_TEST_FAKE_STATVFS="used_pct=95"
//! ```
//!
//! 只影响 `probe()`；注入值同样会进 `status`/日志，**不静默**。

use crate::error::{Error, Result};
use crate::settings::{FreeSpaceSettings, FREE_BELOW_PCT, FREE_FREQUENCY};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// 一次 `statvfs` 结果（字节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsSpace {
    /// 文件系统总大小。
    pub total: u64,
    /// 非特权用户可用字节。
    pub avail: u64,
    /// 全部空闲字节（含 root 保留）。
    pub free: u64,
}

impl FsSpace {
    /// 可用百分比（向下取整，0..=100）。
    pub fn avail_pct(&self) -> u8 {
        if self.total == 0 {
            return 100;
        }
        ((self.avail.min(self.total) as u128 * 100) / self.total as u128).min(100) as u8
    }

    /// 已用百分比（= 100 - 可用）。
    pub fn used_pct(&self) -> u8 {
        100u8.saturating_sub(self.avail_pct())
    }

    /// 从 `used_pct`/`avail_pct` 造一个自洽的空间快照（注入用）。
    fn from_pct(avail_pct: u8, total: u64) -> Self {
        let pct = avail_pct.min(100) as u64;
        let avail = total / 100 * pct;
        Self {
            total,
            avail,
            free: avail,
        }
    }
}

/// `statvfs(3)`：拿 `path` 所在文件系统的空间。
#[cfg(unix)]
pub fn statvfs(path: &Path) -> Result<FsSpace> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::Io(format!("路径含 NUL，无法 statvfs: {}", path.display())))?;
    // SAFETY: `statvfs` 只写我们提供的结构体；失败时返回非 0，我们不读未初始化内存。
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    if rc != 0 {
        return Err(Error::Io(format!(
            "statvfs({}) 失败: {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    let frsize = if st.f_frsize > 0 {
        st.f_frsize as u64
    } else {
        st.f_bsize as u64
    };
    Ok(FsSpace {
        total: (st.f_blocks as u64).saturating_mul(frsize),
        avail: (st.f_bavail as u64).saturating_mul(frsize),
        free: (st.f_bfree as u64).saturating_mul(frsize),
    })
}

#[cfg(not(unix))]
pub fn statvfs(_path: &Path) -> Result<FsSpace> {
    Err(Error::Unsupported("statvfs 只在 unix 上实现".into()))
}

/// 解析 `QSYNC_TEST_FAKE_STATVFS` 的注入值；不在验收里时返回 `None`。
pub fn fake_from_str(spec: &str) -> Option<FsSpace> {
    let mut total = 100u64 * 1024 * 1024 * 1024; // 默认 100 GiB
    let mut avail: Option<u64> = None;
    let mut free: Option<u64> = None;
    let mut seeded = false;
    for part in spec.split(',') {
        let (k, v) = part.split_once('=')?;
        let k = k.trim();
        let v = v.trim();
        match k {
            "total" => {
                total = parse_size(v)?;
                seeded = true;
            }
            "avail" => {
                avail = Some(parse_size(v)?);
                seeded = true;
            }
            "free" => {
                free = Some(parse_size(v)?);
                seeded = true;
            }
            "avail_pct" => {
                let p: u8 = v.parse().ok()?;
                let s = FsSpace::from_pct(p, total);
                avail = Some(s.avail);
                free = Some(s.free);
                seeded = true;
            }
            "used_pct" => {
                let p: u8 = v.parse().ok()?;
                let s = FsSpace::from_pct(100u8.saturating_sub(p), total);
                avail = Some(s.avail);
                free = Some(s.free);
                seeded = true;
            }
            _ => return None,
        }
    }
    if !seeded {
        return None;
    }
    let avail = avail.unwrap_or(total);
    Some(FsSpace {
        total,
        avail,
        free: free.unwrap_or(avail),
    })
}

/// 量空间：**优先用注入值**（`QSYNC_TEST_FAKE_STATVFS`），否则真 `statvfs`。
pub fn probe(path: &Path) -> Result<FsSpace> {
    if let Ok(spec) = std::env::var("QSYNC_TEST_FAKE_STATVFS") {
        if !spec.trim().is_empty() {
            if let Some(s) = fake_from_str(&spec) {
                return Ok(s);
            }
            return Err(Error::Io(format!(
                "QSYNC_TEST_FAKE_STATVFS 写法无效: {spec:?}（例：avail_pct=5 / total=100G,avail=5G）"
            )));
        }
    }
    statvfs(path)
}

/// `k` / `512M` / `1.5G` / `2T`（纯字节数字也接受）。
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
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
    if v < 0.0 {
        return None;
    }
    Some((v * mult as f64) as u64)
}

/// 「自动释放空间」这一轮该不该跑（纯函数，可单测）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoFreeDecision {
    /// 要不要脱水。
    pub run: bool,
    /// 为什么跑 / 为什么不跑（进日志与 `status`，不静默）。
    pub reason: String,
    /// 传给 `run_dehydrate` 的 `cache_limit` 字节数（`None` = 不按限额，按闲置）。
    pub cache_limit_bytes: Option<u64>,
    /// 传给 `run_dehydrate` 的闲置阈值秒数。
    pub idle_secs: u64,
    /// 触发时的可用百分比（用于日志/验收断言）。
    pub avail_pct: u8,
}

impl AutoFreeDecision {
    fn skip(reason: impl Into<String>, avail_pct: u8) -> Self {
        Self {
            run: false,
            reason: reason.into(),
            cache_limit_bytes: None,
            idle_secs: 0,
            avail_pct,
        }
    }
}

/// 判定逻辑：
///
/// * `auto=false` → 不跑；
/// * `below_pct`：`avail_pct < below_pct` 才跑，目标是「把可用空间抬回 `below_pct`」，
///   换算成 `cache_limit = 当前缓存占用 - 需要腾出的字节`（腾不出来就清空缓存，尽力而为）；
/// * `frequency`：距上次触发 ≥ `every_hours` 才跑，按「闲置 ≥ `idle_secs`」脱水。
pub fn decide(
    space: &FsSpace,
    cache_used: u64,
    cfg: &FreeSpaceSettings,
    last_run_unix: u64,
    now_unix: u64,
    idle_secs: u64,
) -> AutoFreeDecision {
    let avail_pct = space.avail_pct();
    if !cfg.auto {
        return AutoFreeDecision::skip("自动释放空间未开启", avail_pct);
    }
    match cfg.mode.as_str() {
        FREE_BELOW_PCT => {
            if avail_pct >= cfg.below_pct {
                return AutoFreeDecision::skip(
                    format!("可用空间 {avail_pct}% ≥ 阈值 {}%，无需释放", cfg.below_pct),
                    avail_pct,
                );
            }
            // 目标：可用空间回到阈值百分比
            let want_avail = space.total / 100 * cfg.below_pct as u64;
            let need = want_avail.saturating_sub(space.avail);
            let limit = cache_used.saturating_sub(need);
            AutoFreeDecision {
                run: true,
                reason: format!(
                    "可用空间 {avail_pct}% < 阈值 {}% → 目标腾出 {} 字节",
                    cfg.below_pct, need
                ),
                cache_limit_bytes: Some(limit),
                idle_secs: 0,
                avail_pct,
            }
        }
        FREE_FREQUENCY => {
            let period = cfg.every_hours.saturating_mul(3600).max(60);
            let elapsed = now_unix.saturating_sub(last_run_unix);
            if last_run_unix != 0 && elapsed < period {
                return AutoFreeDecision::skip(
                    format!("按频率：距上次释放 {}s < {}s", elapsed, period),
                    avail_pct,
                );
            }
            AutoFreeDecision {
                run: true,
                reason: format!(
                    "按频率：每 {} 小时一次（上次 {}）",
                    cfg.every_hours,
                    if last_run_unix == 0 {
                        "从未".to_string()
                    } else {
                        format!("{elapsed}s 前")
                    }
                ),
                cache_limit_bytes: None,
                idle_secs,
                avail_pct,
            }
        }
        other => AutoFreeDecision::skip(format!("未知释放模式 {other:?}"), avail_pct),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(total_g: u64, avail_g: u64) -> FsSpace {
        let g = 1024 * 1024 * 1024;
        FsSpace {
            total: total_g * g,
            avail: avail_g * g,
            free: avail_g * g,
        }
    }

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("1K"), Some(1024));
        assert_eq!(parse_size("1.5G"), Some(1610612736));
        assert_eq!(parse_size("2T"), Some(2 * 1024 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("x"), None);
    }

    #[test]
    fn fake_statvfs_specs() {
        let s = fake_from_str("avail_pct=5").unwrap();
        assert_eq!(s.avail_pct(), 5);
        let s = fake_from_str("used_pct=95").unwrap();
        assert_eq!(s.used_pct(), 95);
        let s = fake_from_str("total=100G,avail=5G,free=6G").unwrap();
        assert_eq!(s.total, 100 * 1024 * 1024 * 1024);
        assert_eq!(s.avail, 5 * 1024 * 1024 * 1024);
        assert_eq!(s.avail_pct(), 5);
        assert!(fake_from_str("nonsense").is_none());
        assert!(fake_from_str("avail_pct=abc").is_none());
    }

    #[test]
    fn probe_uses_injection() {
        // 这个测试独占环境变量：串行跑（cargo test 默认多线程）→ 用互斥锁
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap();
        let key = "QSYNC_TEST_FAKE_STATVFS";
        let old = std::env::var(key).ok();
        std::env::set_var(key, "avail_pct=3");
        let s = probe(Path::new("/")).unwrap();
        assert_eq!(s.avail_pct(), 3);
        std::env::set_var(key, "bad-spec");
        assert!(probe(Path::new("/")).is_err(), "坏注入值必须报错而不是静默真量");
        match old {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn real_statvfs_smoke() {
        let s = statvfs(Path::new("/")).unwrap();
        assert!(s.total > 0, "根文件系统总有大小");
        assert!(s.avail <= s.total);
        assert!(s.avail_pct() <= 100);
    }

    #[test]
    fn below_pct_triggers_only_when_low_and_computes_limit() {
        let mut cfg = FreeSpaceSettings {
            auto: true,
            mode: FREE_BELOW_PCT.into(),
            below_pct: 10,
            every_hours: 24,
        };
        // 20% 可用 → 不跑
        let d = decide(&space(100, 20), 5 * 1024 * 1024 * 1024, &cfg, 0, 1000, 0);
        assert!(!d.run);
        assert_eq!(d.avail_pct, 20);

        // 5% 可用、缓存占用 20 GiB → 目标抬回 10%（需腾 5 GiB）→ 限额 = 15 GiB
        let g = 1024 * 1024 * 1024;
        let d = decide(&space(100, 5), 20 * g, &cfg, 0, 1000, 0);
        assert!(d.run, "{}", d.reason);
        assert_eq!(d.cache_limit_bytes, Some(15 * g));
        assert_eq!(d.avail_pct, 5);

        // 缓存比需要腾出的还少 → 限额 0（能清多少清多少，不 panic）
        let d = decide(&space(100, 5), 1 * g, &cfg, 0, 1000, 0);
        assert!(d.run);
        assert_eq!(d.cache_limit_bytes, Some(0));

        cfg.auto = false;
        let d = decide(&space(100, 1), 20 * g, &cfg, 0, 1000, 0);
        assert!(!d.run);
        assert!(d.reason.contains("未开启"));
    }

    #[test]
    fn frequency_respects_period() {
        let cfg = FreeSpaceSettings {
            auto: true,
            mode: FREE_FREQUENCY.into(),
            below_pct: 10,
            every_hours: 6,
        };
        // 空间很充足也会按时跑（「按频率」的语义就是与剩余空间无关）
        let d = decide(&space(100, 90), 0, &cfg, 0, 10_000, 3600);
        assert!(d.run, "首次（last=0）应当跑: {}", d.reason);
        assert_eq!(d.idle_secs, 3600);
        assert_eq!(d.cache_limit_bytes, None);

        let now = 100_000u64;
        let d = decide(&space(100, 90), 0, &cfg, now - 3600, now, 3600);
        assert!(!d.run, "距上次 1h < 6h 不该跑");
        let d = decide(&space(100, 90), 0, &cfg, now - 6 * 3600, now, 3600);
        assert!(d.run, "满 6h 该跑");
        assert!(d.reason.contains("6 小时"));
    }
}
