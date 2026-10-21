//! ★ 回归：`init_logging` 里「自定义 log 桥 + tracing-subscriber 的 LogTracer」冲突
//!   会让 qxyncd 启动即 panic —— daemon 化后 stderr 指向 /dev/null，用户只能看到
//!   `qxync daemon start` 报「socket 未就绪」，日志里零线索（真机排查了一整轮）。
//!
//!   最小可复现路径就是 `--print-config`：它不 daemon 化、不建 tokio 运行时，
//!   但照常走 `init_logging` —— 修复前这里必 panic（退出码 101）。
//!
//!   历史：WPS 保存修复（bc81a4f）给 daemon 加了 log→tracing 桥（`log::set_logger`），
//!   同一函数里又用 `SubscriberInitExt::init()` 装 subscriber —— tracing-subscriber
//!   的默认 feature 带 `tracing-log`，`init()` 内部还会调 `LogTracer::init()` 去占
//!   `log` 全局 logger，于是 SetLoggerError → panic。修法见 main.rs：绕开 `init()`，
//!   直接 `tracing::subscriber::set_global_default(...)`。

use std::path::PathBuf;

fn unique_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qxync-daemon-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

#[test]
fn print_config_survives_logging_init() {
    let root = unique_dir("logging-init");
    let state = root.join("state");
    let config = root.join("config");
    let data = root.join("data");

    // XDG 三个目录全部重定向到临时目录：既不碰用户的真实配置/状态，
    // 也保证 adopt_legacy_dir 的迁移逻辑（base/qsync → base/qxync）跑在隔离区里。
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_qxyncd"))
        .args(["--print-config", "--link", "default"])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_DATA_HOME", &data)
        .env("RUST_LOG", "error")
        .output()
        .expect("启动 qxyncd");

    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "qxyncd --print-config 非零退出（logging 初始化 panic？）：status={:?} stderr={stderr}",
        out.status
    );
    assert!(
        stdout.contains("\"link\":\"default\""),
        "--print-config 输出缺少 link 字段：{stdout}"
    );

    // 文件日志层确实建出来了（不只是 stderr 一路）
    let log_dir = state.join("qxync").join("log");
    let has_log = std::fs::read_dir(&log_dir)
        .map(|rd| {
            rd.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("qxyncd.log."))
        })
        .unwrap_or(false);
    assert!(has_log, "滚动日志文件没有建出来：{}", log_dir.display());

    let _ = std::fs::remove_dir_all(&root);
}

/// 日志目录建不出来时必须**降级到 stderr** 而不是 panic —— 启动期 panic 在
/// daemon 化后完全不可见（用户只看到「socket 未就绪」）。
#[test]
fn unwritable_log_dir_falls_back_to_stderr() {
    let root = unique_dir("logging-init-fallback");
    let blocker = root.join("blocker");
    std::fs::write(&blocker, b"not a directory").expect("写阻塞文件");

    // XDG_STATE_HOME 指向一个「文件」→ <state>/qxync/log 建不出来。
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_qxyncd"))
        .args(["--print-config", "--link", "default"])
        .env("XDG_STATE_HOME", &blocker)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("RUST_LOG", "error")
        .output()
        .expect("启动 qxyncd");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "日志目录不可写时不应 panic：status={:?} stderr={stderr}",
        out.status
    );
    assert!(stderr.contains("日志目录不可写"), "缺少降级提示：{stderr}");
    let _ = std::fs::remove_dir_all(&root);
}
