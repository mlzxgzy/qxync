//! `get` 的下载进度显示（M26）。
//!
//! 设计一句话：**下载超过 [`SHOW_AFTER`]（3 秒）才亮进度行** —— 快的下载一个字
//! 都不多打（输出与之前完全一致），慢的下载 3 秒后在 stderr 原地刷新
//! 「百分比 + 进度条 + 已下载/总量 + 速度 + 剩余时间」。
//!
//! * 写 **stderr**：stdout 保持纯净（`qxync get > log` 不混进度）；
//! * 只在 stderr 是终端时启用（重定向/管道里刷 `\r` 只会污染日志）；
//! * 重绘节流 10 Hz；速度用最近 3 秒滑窗（比「从头平均」更贴近当前带宽）；
//! * 结束时（成功/失败都）清掉进度行，让 `✅ …` / 错误消息独占一行。
//!
//! 直连与经 daemon 两条路径共用这一个结构：前者由客户端分片回调喂，
//! 后者由 daemon 推的进度行喂（见 `ipc_client::call_with_progress`）。
//!
//! 已知化妆级局限：行宽按「1 字符 = 1 列」补空格，CJK 文件名会占两列，
//! 极端情况下清行可能留一点残影 —— 不值得为它引 unicode-width 依赖。

use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 进度行亮出的延迟：短于它的下载不显示任何进度（需求：超过 3 秒才显示）。
pub const SHOW_AFTER: Duration = Duration::from_secs(3);
/// 重绘节流：再快的分片流也最多 10 帧/秒（防闪烁、省无谓 IO）。
const REDRAW_EVERY: Duration = Duration::from_millis(100);
/// 速度滑窗：用最近 3 秒的采样算「当前速度」。
const SPEED_WINDOW: Duration = Duration::from_secs(3);
const BAR_WIDTH: usize = 20;
/// 进度行固定宽度：补空格防残影，超长按字符截断。
const LINE_WIDTH: usize = 76;
const NAME_MAX: usize = 24;

/// 一次下载的进度显示。`Clone` 后把副本挪进回调闭包，原实例留在主流程 `finish()`。
#[derive(Clone)]
pub struct DownloadProgress {
    inner: Option<Arc<Mutex<Inner>>>,
}

struct Inner {
    name: String,
    start: Instant,
    /// `stat` 先行拿到的远端大小：服务端 chunked（无 Content-Length）时的兜底总量。
    total_hint: Option<u64>,
    written: u64,
    total: Option<u64>,
    samples: VecDeque<(Instant, u64)>,
    shown: bool,
    last_draw: Instant,
}

impl DownloadProgress {
    /// stderr 不是终端 → 恒禁用（`update`/`finish` 全是 no-op）。
    pub fn new(name: &str) -> Self {
        let inner = std::io::stderr().is_terminal().then(|| {
            Arc::new(Mutex::new(Inner {
                name: truncate_name(name),
                start: Instant::now(),
                total_hint: None,
                written: 0,
                total: None,
                samples: VecDeque::new(),
                shown: false,
                last_draw: Instant::now(),
            }))
        });
        Self { inner }
    }

    /// 直连路径的总量兜底：`get` 本来就先 `stat` 拿远端大小做长度校验，
    /// 顺手喂给进度条（daemon 路径没有这个值，靠 Content-Length）。
    pub fn with_total_hint(self, total: Option<u64>) -> Self {
        if let Some(i) = &self.inner {
            i.lock().unwrap().total_hint = total;
        }
        self
    }

    /// stderr 是否终端（即进度会不会真的画）。经 daemon 下载时用它决定要不要
    /// 在信封里声明 `progress:true` —— 禁用时 daemon 不建通道不推行，白省。
    pub fn enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// 喂一次采样（客户端分片回调 / daemon 进度行都走这里）。必须廉价：下载循环里同步调。
    pub fn update(&self, written: u64, total: Option<u64>) {
        let Some(inner) = &self.inner else { return };
        let mut g = inner.lock().unwrap();
        let now = Instant::now();
        g.written = written;
        g.total = total.or(g.total_hint);
        g.samples.push_back((now, written));
        while let Some(&(at, _)) = g.samples.front() {
            if now.duration_since(at) > SPEED_WINDOW {
                g.samples.pop_front();
            } else {
                break;
            }
        }
        // 3 秒静默期：没过就一个字不画（下载随后结束 → finish 也是 no-op，输出零变化）
        if !past_grace(g.start, now) {
            return;
        }
        // 节流：首帧必画（shown=false），之后最多 10 Hz
        if g.shown && now.duration_since(g.last_draw) < REDRAW_EVERY {
            return;
        }
        let line = render(&g, now);
        g.shown = true;
        g.last_draw = now;
        drop(g);
        draw(&line);
    }

    /// 下载结束（成功或失败都要调）：画过就清行，让后续消息独占一行。
    pub fn finish(&self) {
        let Some(inner) = &self.inner else { return };
        let mut g = inner.lock().unwrap();
        if g.shown {
            g.shown = false;
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "\r{}\r", " ".repeat(LINE_WIDTH));
            let _ = err.flush();
        }
    }
}

/// 静默期判定（独立出来是为了能单测「3 秒」这条线）。
fn past_grace(start: Instant, now: Instant) -> bool {
    now.duration_since(start) >= SHOW_AFTER
}

fn draw(line: &str) {
    let mut err = std::io::stderr().lock();
    let _ = write!(err, "\r{line}");
    let _ = err.flush();
}

fn truncate_name(name: &str) -> String {
    if name.chars().count() <= NAME_MAX {
        name.to_string()
    } else {
        let head: String = name.chars().take(NAME_MAX - 1).collect();
        format!("{head}…")
    }
}

/// 滑窗速度（字节/秒）。窗口不足 1 ms 时返回 0（除法会爆）。
fn speed_of(samples: &VecDeque<(Instant, u64)>, now: Instant) -> u64 {
    match (samples.front(), samples.back()) {
        (Some(&(at0, w0)), Some(&(_, w1))) => {
            let dt = now.duration_since(at0).as_secs_f64();
            if dt < 0.001 {
                0
            } else {
                ((w1.saturating_sub(w0)) as f64 / dt) as u64
            }
        }
        _ => 0,
    }
}

fn render(g: &Inner, now: Instant) -> String {
    let speed = speed_of(&g.samples, now);
    let body = match g.total {
        Some(t) if t > 0 => {
            let pct = (g.written as f64 / t as f64).clamp(0.0, 1.0);
            let filled = (pct * BAR_WIDTH as f64).round() as usize;
            let bar: String = "█".repeat(filled) + &"░".repeat(BAR_WIDTH - filled);
            let eta = if speed > 0 && g.written < t {
                format!(" · 剩余 {}", fmt_eta((t - g.written) / speed))
            } else {
                String::new()
            };
            format!(
                "[{bar}] {:>5.1}% {}/{} · {}{eta}",
                pct * 100.0,
                human_size(g.written),
                human_size(t),
                human_speed(speed)
            )
        }
        // 未知总量（chunked 且没有 stat 兜底）：只给「已下载多少 + 速度」
        _ => format!("已下载 {} · {}", human_size(g.written), human_speed(speed)),
    };
    pad_or_trunc(&format!("⬇ {} {body}", g.name))
}

fn fmt_eta(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn human_size(n: u64) -> String {
    const K: u64 = 1024;
    match n {
        0..=1023 => format!("{n} B"),
        n if n < K * K => format!("{:.1} KiB", n as f64 / K as f64),
        n if n < K * K * K => format!("{:.1} MiB", n as f64 / (K * K) as f64),
        n => format!("{:.2} GiB", n as f64 / (K * K * K) as f64),
    }
}

fn human_speed(bps: u64) -> String {
    if bps == 0 {
        "…/s".into()
    } else {
        format!("{}/s", human_size(bps))
    }
}

/// 补空格到 [`LINE_WIDTH`]（清残影），超长按字符截断（窄终端下换行比残影更糟）。
fn pad_or_trunc(s: &str) -> String {
    let width = s.chars().count();
    if width >= LINE_WIDTH {
        s.chars().take(LINE_WIDTH).collect()
    } else {
        format!("{s}{}", " ".repeat(LINE_WIDTH - width))
    }
}

// ---------------------------------------------------------------- 单测

#[cfg(test)]
mod tests {
    use super::*;

    fn inner_at(
        start: Instant,
        written: u64,
        total: Option<u64>,
        samples: &[(Duration, u64)],
    ) -> Inner {
        let now = Instant::now();
        Inner {
            name: truncate_name("big.bin"),
            start,
            total_hint: None,
            written,
            total,
            samples: samples.iter().map(|&(ago, w)| (now - ago, w)).collect(),
            shown: false,
            last_draw: start,
        }
    }

    #[test]
    fn grace_period_is_three_seconds() {
        let now = Instant::now();
        assert!(!past_grace(now - Duration::from_secs(2), now), "2 秒：静默");
        assert!(
            !past_grace(now - Duration::from_millis(2999), now),
            "差 1ms：静默"
        );
        assert!(past_grace(now - Duration::from_secs(3), now), "满 3 秒：亮");
        assert!(past_grace(now - Duration::from_secs(30), now), "30 秒：亮");
    }

    #[test]
    fn render_shows_bar_percent_speed_eta() {
        let now = Instant::now();
        let mib = 1024 * 1024;
        // 3 秒滑窗：0 → 25 MiB，速度 ≈ 25/3 MiB/s；总量 100 MiB
        let g = inner_at(
            now - Duration::from_secs(3),
            25 * mib,
            Some(100 * mib),
            &[(Duration::from_secs(3), 0), (Duration::ZERO, 25 * mib)],
        );
        let line = render(&g, now);
        assert!(line.contains("25.0%"), "{line}");
        assert!(line.contains("25.0 MiB/100.0 MiB"), "{line}");
        assert!(line.contains("MiB/s"), "{line}");
        assert!(line.contains("剩余"), "{line}");
        assert!(
            line.contains('█') && line.contains('░'),
            "进度条两端都要有：{line}"
        );
        assert!(line.starts_with("⬇ big.bin"), "{line}");
    }

    #[test]
    fn render_without_total_drops_bar_and_percent() {
        let now = Instant::now();
        let g = inner_at(
            now - Duration::from_secs(5),
            4096,
            None,
            &[(Duration::from_secs(2), 0), (Duration::ZERO, 4096)],
        );
        let line = render(&g, now);
        assert!(line.contains("已下载 4.0 KiB"), "{line}");
        assert!(!line.contains('%'), "未知总量不该有百分比：{line}");
        assert!(!line.contains('█'), "未知总量不该有进度条：{line}");
    }

    #[test]
    fn speed_uses_sliding_window_not_since_start() {
        let now = Instant::now();
        // 窗口里只有最近 3 秒的样本：0 → 30 MiB ⇒ ≈10 MiB/s。
        // （update 的裁剪保证喂进来的就是这 3 秒；这里直接构造裁剪后的形态）
        let samples: VecDeque<(Instant, u64)> = vec![
            (now - Duration::from_secs(3), 0),
            (now - Duration::from_secs(1), 10 * 1024 * 1024),
            (now - Duration::from_millis(100), 30 * 1024 * 1024),
        ]
        .into();
        let speed = speed_of(&samples, now);
        assert!((9..=11).contains(&(speed / (1024 * 1024))), "速度 {speed}");
    }

    #[test]
    fn speed_with_single_sample_or_tiny_window_is_zero() {
        let now = Instant::now();
        let one: VecDeque<(Instant, u64)> = vec![(now, 1000)].into();
        assert_eq!(speed_of(&one, now), 0);
        let tight: VecDeque<(Instant, u64)> = vec![(now, 0), (now, 999)].into();
        assert_eq!(speed_of(&tight, now), 0, "dt < 1ms 必须挡住除法");
    }

    #[test]
    fn line_is_padded_to_fixed_width() {
        let now = Instant::now();
        let g = inner_at(
            now - Duration::from_secs(4),
            10,
            Some(20),
            &[(Duration::ZERO, 10)],
        );
        let line = render(&g, now);
        assert_eq!(line.chars().count(), LINE_WIDTH, "固定宽度清残影：{line}");
    }

    #[test]
    fn long_names_are_truncated_not_wrapped() {
        let n = truncate_name(&"a".repeat(60));
        assert_eq!(n.chars().count(), NAME_MAX);
        assert!(n.ends_with('…'));
        assert_eq!(truncate_name("short.bin"), "short.bin");
    }

    #[test]
    fn eta_formats_humanly() {
        assert_eq!(fmt_eta(45), "45s");
        assert_eq!(fmt_eta(65), "1m05s");
        assert_eq!(fmt_eta(3725), "1h02m");
    }

    #[test]
    fn disabled_progress_is_noop() {
        // stderr 重定向时 new() 得到禁用实例 —— 这里只能构造等价形态验证 no-op 语义
        let p = DownloadProgress { inner: None };
        assert!(!p.enabled());
        p.update(1, Some(2)); // 不 panic、不写终端
        p.finish();
    }
}
