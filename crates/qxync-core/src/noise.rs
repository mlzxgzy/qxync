//! ★ M41：日志降噪 —— 同一来源的重复日志按「签名」限流。
//!
//! ## 为什么要这一层
//!
//! M39 把「日志文件」管住了（按大小切段、按天/总量删旧），但**写进去的内容**没管：
//! 2026-10-25 的真机日志 11.2 MB / 56 645 行里，**10.3 MB** 是同一个 fd 泄漏导致的
//! `创建缓存文件失败 … Too many open files`（48 983 行，每行带一个不同路径）；
//! 10-26 又有 4.5 MB，其中 `对账列举失败（第 N/M 个目录）` 3 677 行、`上传因网络不可用
//! 暂停: <路径>` 941 行。**信息量只有一句话，体积是几 MB。**
//!
//! 这一层只做一件事：让调用方把「同一条消息的重复」压成「首次 + 每窗口一条（带省略计数）」，
//! 逐条明细仍然写 `debug!`（`RUST_LOG=debug` 时一条不少）。
//!
//! ## 语义（刻意简单，可单测）
//!
//! 对 `(site, signature)` 这个键：
//!
//! 1. **第一次** → 放行（`suppressed = 0`）；
//! 2. **窗口内**再出现 → 压掉（计数 +1），调用方通常写 `debug!`；
//! 3. **窗口到期后的第一次** → 放行，且 `suppressed = 窗口内压掉的条数`，
//!    调用方把它写进消息（「过去 60s 另省略 N 条同类」）；
//! 4. `reset` → 删掉这个键，下一次立刻放行（用于「故障已恢复」后的首条）。
//!
//! **签名里绝不能带路径/数字**（那样每条都是新键，等于没限流）；路径/具体值只出现在
//! 放行那条消息里。签名一般是错误种类：`EMFILE`、`status=5`、`offline`、`io:ENOENT`。
//!
//! ## 边界
//!
//! * 状态有界：最多 [`MAX_KEYS`] 个键，超了淘汰「窗口开始最早」的（极端刷屏也不会把内存吃穿）；
//! * 全进程共享一把 `Mutex`；锁内只有 `HashMap` 操作，不碰 IO —— FUSE 回调路径多这一下
//!   可忽略（对比一次日志格式化 + 文件写入）。**日志路径不能因为别的线程 panic 就停摆**，
//!   所以锁中毒按「继续用」处理（`unwrap_or_else(|e| e.into_inner())`，与 `logcfg` 同口径）；
//! * 窗口按 `Instant`（单调时钟），系统时间跳变不影响；
//! * 这是**进程内**限流：daemon 与 GUI 各有一份状态，互不干扰（它们本来就各写各的日志文件）。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 默认限流窗口。60s 是「一条运维消息最多每分钟一条」：既不吵，又不会让现场断档。
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(60);

/// 最多同时跟踪多少个签名。典型站点十几个签名，512 足够；超了按「窗口开始最早」淘汰。
pub const MAX_KEYS: usize = 512;

/// 限流判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admission {
    /// 是否应输出这条日志。
    pub emit: bool,
    /// 本次放行时，**过去一个窗口内被压掉**的同类条数（首次放行为 0）。
    /// 调用方应把它写进消息，别让信息静默消失。
    pub suppressed: u64,
}

impl Admission {
    /// 省略计数的后缀文案（`suppressed == 0` → 空串）。方便调用方拼消息。
    pub fn note(&self) -> String {
        if self.suppressed == 0 {
            String::new()
        } else {
            format!("（过去一个窗口内另省略 {} 条同类）", self.suppressed)
        }
    }
}

/// 键 = (站点, 签名)。站点用 `&'static str` 的场景占多数，但调用方也可能传 `String`，
/// 统一存 `String` 换实现简单；日志路径上每次两条小分配，可接受。
type Key = (String, String);

struct Entry {
    /// 当前窗口的起点（单调时钟）。
    window_start: Instant,
    /// 本窗口内被压掉的条数。
    suppressed: u64,
}

struct Gate {
    map: HashMap<Key, Entry>,
}

fn gate() -> &'static Mutex<Gate> {
    static GATE: OnceLock<Mutex<Gate>> = OnceLock::new();
    GATE.get_or_init(|| {
        Mutex::new(Gate {
            map: HashMap::new(),
        })
    })
}

/// 加锁；中毒也继续用（见模块文档「边界」）。
fn lock() -> std::sync::MutexGuard<'static, Gate> {
    gate().lock().unwrap_or_else(|e| e.into_inner())
}

/// 按默认窗口（[`DEFAULT_WINDOW`]）判定是否放行。
pub fn dedupe(site: &str, signature: &str) -> Admission {
    dedupe_in(site, signature, DEFAULT_WINDOW)
}

/// 指定窗口判定是否放行（窗口 = 0 等价于每次都放行，不做抑制）。
pub fn dedupe_in(site: &str, signature: &str, window: Duration) -> Admission {
    let mut g = lock();
    admit(&mut g.map, site, signature, Instant::now(), window)
}

/// 心跳式放行：同一 `site` 每 `period` 最多放行一次（首次立即放行）。
///
/// 用途：把「每轮都打」的 INFO 行（例如 `同步轮询: …`）改成「有变化才打 + 定时一条」，
/// 空闲时日志不再按轮询节奏膨胀。
pub fn every(site: &str, period: Duration) -> Admission {
    dedupe_in(site, "__every__", period)
}

/// 清掉一个签名的状态：故障恢复后的第一条应立即放行，不等窗口到期。
pub fn reset(site: &str, signature: &str) {
    let mut g = lock();
    g.map.remove(&key_of(site, signature));
}

/// 清掉一个站点下所有签名（站点整体恢复时用）。
pub fn reset_site(site: &str) {
    let mut g = lock();
    g.map.retain(|(s, _), _| s != site);
}

/// 当前跟踪的签名数（诊断 / 单测用）。
pub fn tracked() -> usize {
    lock().map.len()
}

fn key_of(site: &str, signature: &str) -> Key {
    (site.to_string(), signature.to_string())
}

/// 限流核心（私有的「时钟注入版」，单测用合成时刻，不 sleep）。
///
/// `now` 必须单调不倒退；调用方保证这一点（`Instant::now()` 天然满足）。
fn admit(
    map: &mut HashMap<Key, Entry>,
    site: &str,
    signature: &str,
    now: Instant,
    window: Duration,
) -> Admission {
    let key = key_of(site, signature);
    if window.is_zero() {
        return Admission {
            emit: true,
            suppressed: 0,
        };
    }
    if let Some(e) = map.get_mut(&key) {
        if now.saturating_duration_since(e.window_start) >= window {
            let suppressed = e.suppressed;
            e.window_start = now;
            e.suppressed = 0;
            Admission {
                emit: true,
                suppressed,
            }
        } else {
            e.suppressed = e.suppressed.saturating_add(1);
            Admission {
                emit: false,
                suppressed: 0,
            }
        }
    } else {
        if map.len() >= MAX_KEYS {
            evict_oldest(map);
        }
        map.insert(
            key,
            Entry {
                window_start: now,
                suppressed: 0,
            },
        );
        Admission {
            emit: true,
            suppressed: 0,
        }
    }
}

/// 淘汰「窗口开始最早」的一个键（只在超上限时调用，O(n) 可接受）。
fn evict_oldest(map: &mut HashMap<Key, Entry>) {
    if let Some(k) = map
        .iter()
        .min_by_key(|(_, e)| e.window_start)
        .map(|(k, _)| k.clone())
    {
        map.remove(&k);
    }
}

// ---------------------------------------------------------------- 消息瘦身

/// 截断到最多 `max_chars` 个字符（按 `char` 边界，不会切坏 UTF-8）。
///
/// 被截断时末尾加 `…`。用途：`errors` / `deleted` 这类可能很长的数组拼进一行日志前
/// 先截一刀 —— 真机 `轮询有异常: … errors=["同步…第 47/5245 个目录…", …]` 单行 1.8 KB，
/// 其中 99% 是同一个错误的重复。
pub fn truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_chars.saturating_mul(4)));
    for (n, ch) in s.chars().enumerate() {
        if n >= max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

/// 把一组错误压成一行摘要：`共 N 条；首条「…」`（`N == 0` → `无`）。
///
/// 逐条全文仍在 `debug!` 里（或 `SyncReport.errors` 里），用户可见的那一行只留
/// 「有几条 + 第一条像什么」—— 现场排查看的是「是不是同一类错」，不是 20 份全文。
pub fn summarize_errors(errs: &[String], max_chars: usize) -> String {
    match errs.first() {
        None => "无".to_string(),
        Some(first) => format!(
            "共 {} 条；首条「{}」",
            errs.len(),
            truncate(first, max_chars)
        ),
    }
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    /// 窗口内：首次放行、后续压掉；窗口到期后的第一条放行并报出省略数。
    #[test]
    fn first_then_suppress_then_report_count() {
        let mut map = HashMap::new();
        let win = Duration::from_secs(10);
        let now = t0();

        let a = admit(&mut map, "t-first", "EMFILE", now, win);
        assert!(a.emit, "首次必须放行");
        assert_eq!(a.suppressed, 0);
        assert_eq!(a.note(), "");

        let a = admit(
            &mut map,
            "t-first",
            "EMFILE",
            now + Duration::from_secs(1),
            win,
        );
        assert!(!a.emit, "窗口内必须压掉");
        let a = admit(
            &mut map,
            "t-first",
            "EMFILE",
            now + Duration::from_secs(9),
            win,
        );
        assert!(!a.emit);

        // 刚好到期：放行，并如实报出前面压掉了 2 条
        let a = admit(
            &mut map,
            "t-first",
            "EMFILE",
            now + Duration::from_secs(10),
            win,
        );
        assert!(a.emit, "窗口到期必须放行");
        assert_eq!(a.suppressed, 2);
        assert!(a.note().contains('2'));
    }

    /// 不同站点 / 不同签名互不影响（这是「签名不能带路径」的前提：同签名才聚合）。
    #[test]
    fn site_and_signature_are_independent() {
        let mut map = HashMap::new();
        let win = Duration::from_secs(60);
        let now = t0();
        assert!(admit(&mut map, "t-ind", "a", now, win).emit);
        assert!(
            admit(&mut map, "t-ind", "b", now, win).emit,
            "不同签名各自首放"
        );
        assert!(
            admit(&mut map, "t-ind2", "a", now, win).emit,
            "不同站点各自首放"
        );
        assert!(!admit(&mut map, "t-ind", "a", now + Duration::from_secs(1), win).emit);
    }

    /// 窗口为 0 = 每次放行（想彻底关掉限流的站点用）。
    #[test]
    fn zero_window_always_emits() {
        let mut map = HashMap::new();
        let now = t0();
        for _ in 0..3 {
            let a = admit(&mut map, "t-zero", "x", now, Duration::ZERO);
            assert!(a.emit);
            assert_eq!(a.suppressed, 0);
        }
        assert!(map.is_empty(), "零窗口不应留下状态");
    }

    /// 状态有界：超过 MAX_KEYS 时淘汰最旧的一个，长度不再涨。
    #[test]
    fn map_is_bounded() {
        let mut map = HashMap::new();
        let win = Duration::from_secs(60);
        let now = t0();
        for i in 0..(MAX_KEYS + 50) {
            let site = format!("t-bound-{i}");
            let _ = admit(
                &mut map,
                &site,
                "sig",
                now + Duration::from_millis(i as u64),
                win,
            );
        }
        assert_eq!(map.len(), MAX_KEYS, "键数必须被夹在 MAX_KEYS");
        // 淘汰的是「窗口开始最早」的：最早的 t-bound-0 应已不在
        assert!(!map.contains_key(&key_of("t-bound-0", "sig")));
    }

    /// reset 之后下一次立刻放行（故障恢复场景：下一秒又坏了，不该被窗口吞掉）。
    #[test]
    fn reset_lets_next_event_through() {
        let mut map = HashMap::new();
        let win = Duration::from_secs(60);
        let now = t0();
        assert!(admit(&mut map, "t-reset", "offline", now, win).emit);
        assert!(
            !admit(
                &mut map,
                "t-reset",
                "offline",
                now + Duration::from_secs(1),
                win
            )
            .emit
        );
        map.remove(&key_of("t-reset", "offline"));
        assert!(
            admit(
                &mut map,
                "t-reset",
                "offline",
                now + Duration::from_secs(2),
                win
            )
            .emit
        );
    }

    /// 截断按字符边界走，中文/emoji 不会切坏 UTF-8。
    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("中文路径/文件名.xlsx", 4), "中文路径…");
        assert_eq!(truncate("", 3), "");
        // emoji 占 4 字节，但算 1 个字符
        assert_eq!(truncate("🙂🙂🙂", 2), "🙂🙂…");
    }

    /// 错误摘要：没有 → 「无」；有 → 条数 + 首条（截断）。
    #[test]
    fn summarize_errors_is_one_line() {
        assert_eq!(summarize_errors(&[], 100), "无");
        let errs = vec!["第一条错误".to_string(), "第二条".to_string()];
        let s = summarize_errors(&errs, 100);
        assert!(s.contains("共 2 条"), "{s}");
        assert!(s.contains("第一条错误"), "{s}");

        let long = vec!["x".repeat(500)];
        let s = summarize_errors(&long, 10);
        assert!(s.contains('…'), "长错误必须截断: {s}");
        assert!(s.chars().count() < 60, "摘要必须是一行小串: {s}");
    }

    /// 全局入口可正常调用、reset 生效（其余测试用独立 site，互不干扰）。
    #[test]
    fn global_entry_points_work() {
        reset_site("t-global");
        let a = dedupe("t-global", "sig");
        assert!(a.emit);
        assert!(!dedupe("t-global", "sig").emit);
        reset("t-global", "sig");
        assert!(dedupe("t-global", "sig").emit);
        reset_site("t-global");
        assert!(
            every("t-global-every", Duration::from_secs(60)).emit,
            "心跳首次放行"
        );
        assert!(
            !every("t-global-every", Duration::from_secs(60)).emit,
            "同一窗口内心跳只放一次"
        );
    }
}
