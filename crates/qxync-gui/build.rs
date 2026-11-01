fn main() {
    // ★ P0-17：UI 是**编译期嵌入**的（tauri `frontendDist: "ui"`），而 proc-macro 读文件
    // 不在 cargo 的依赖图里 —— 只改 ui/*.js|css|html 时二进制不会更新，截图验收会看到
    // 上一版的界面（真踩过：改了左栏文字开关，截出来还是旧壳）。
    // 这里显式登记，保证 `cargo build -p qxync-gui` 一定重新嵌入。
    for f in [
        "ui/index.html",
        "ui/app.js",
        "ui/style.css",
        "ui/i18n.js",
        "ui/task-defaults.js",
    ] {
        println!("cargo:rerun-if-changed={f}");
    }
    tauri_build::build()
}
