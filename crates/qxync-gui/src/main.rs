//! `qxync-gui` 入口。
//!
//! * `qxync-gui`                    → 打开窗口
//! * `qxync-gui --self-test`        → 不开窗口，打印 JSON 自检结果（脚本/CI 用）
//! * `qxync-gui --self-test-login`  → 额外把「保存并登录」整条链跑一遍（会重启 daemon）
//!
//! 见 `xtask/tests/gui-matrix.sh`。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--self-test") {
        std::process::exit(if qxync_gui::self_test() { 0 } else { 1 });
    }
    if args.iter().any(|a| a == "--self-test-login") {
        std::process::exit(if qxync_gui::self_test_login() { 0 } else { 1 });
    }
    qxync_gui::run();
}
