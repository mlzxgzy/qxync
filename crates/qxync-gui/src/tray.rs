//! ★ M8.4：系统托盘（Linux 上走 D-Bus 的 `org.kde.StatusNotifierItem`）。
//!
//! 设计取舍：
//! * **托盘只负责「转发意图」，不直接干活**。菜单里的「打开主窗口 / 立即与 NAS 同步 /
//!   暂停」都只是往主窗口 emit 一条 `tray://action`（payload 是 `"open"|"sync"|"pause"`），
//!   真正的动作由前端做 —— 前端才持有界面状态与 IPC 封装；如果 Rust 侧绕开前端直接发
//!   IPC，「暂停」按钮的 UI 状态就会和 daemon 实际状态对不上（QSync 也有同样的坑）。
//!   唯一的例外是「退出」：那是纯粹的进程生命周期，直接 `app.exit(0)`。
//! * **托盘失败不能拖垮 GUI**。缺 `libayatana-appindicator3` / 没有 StatusNotifierHost /
//!   纯 X11 环境下建托盘都会失败，但那只是「少了个图标」，登录/同步/挂载全都还能用。
//!   所以这里把错误吞掉记日志，用一个进程级标志把结果留给 `m84_info` 如实上报。
//! * **菜单文案用中文**：与 GUI 其余部分（zh-CN）保持一致，且照 QSync 的四项。

use std::sync::OnceLock;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter};

/// 事件名：托盘 → 主窗口。`://` 在 Tauri 的事件名白名单里（字母数字 + `-` `/` `:` `_`）。
pub const ACTION_EVENT: &str = "tray://action";

/// 托盘图标 id（仅用于日志/调试；同进程只有一个托盘）。
const TRAY_ID: &str = "qsync-tray";
const MENU_OPEN: &str = "tray-open";
const MENU_SYNC: &str = "tray-sync";
const MENU_PAUSE: &str = "tray-pause";
const MENU_QUIT: &str = "tray-quit";

/// 托盘是否**真的**建起来了。`OnceLock` 是刻意的：一个进程只有一个托盘，
/// 且 `m84_info` 可能在任何时刻被前端调用（那时 setup 早已跑完）。
static TRAY_CREATED: OnceLock<bool> = OnceLock::new();

/// 供 `m84_info` 读取；`--self-test` 这类无窗口模式从没建过托盘，所以缺省是 `false`
/// （**不虚报**：没建就是没建）。
pub fn created() -> bool {
    *TRAY_CREATED.get().unwrap_or(&false)
}

/// 建托盘；无论成败都只记日志，绝不向上抛错（见模块头「托盘失败不能拖垮 GUI」）。
pub fn setup(app: &AppHandle) {
    let ok = match build(app) {
        Ok(()) => {
            tracing::info!("★ M8.4：系统托盘创建成功（StatusNotifierItem 已注册到 D-Bus）");
            true
        }
        Err(e) => {
            tracing::error!(
                "★ M8.4：系统托盘创建失败，降级为普通窗口（只是没有常驻图标，其它功能不受影响）: {e}"
            );
            false
        }
    };
    // set 只可能失败一次（重复 setup），忽略即可。
    let _ = TRAY_CREATED.set(ok);
}

fn build(app: &AppHandle) -> tauri::Result<()> {
    // `include_bytes!` 在编译期把图标焊进二进制：托盘图标必须随时可用，
    // 不能依赖运行时 cwd 或安装路径下恰好有 icons/ 目录。
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))?;

    let open = MenuItemBuilder::with_id(MENU_OPEN, "打开主窗口").build(app)?;
    let sync = MenuItemBuilder::with_id(MENU_SYNC, "立即与 NAS 同步").build(app)?;
    let pause = MenuItemBuilder::with_id(MENU_PAUSE, "暂停").build(app)?;
    let quit = MenuItemBuilder::with_id(MENU_QUIT, "退出").build(app)?;
    // 顺序照 QSync：三项动作 + 分隔线 + 退出（退出隔开，避免误点）
    let menu = MenuBuilder::new(app)
        .items(&[&open, &sync, &pause])
        .separator()
        .item(&quit)
        .build()?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .tooltip("QSync — QNAP 按需同步")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            MENU_OPEN => emit_action(app, "open"),
            MENU_SYNC => emit_action(app, "sync"),
            MENU_PAUSE => emit_action(app, "pause"),
            // 退出是这里唯一自己动手的动作：前端可能已经卡住/窗口已隐藏，
            // 再走一遍 IPC 反而可能退不掉。
            MENU_QUIT => {
                tracing::info!("★ M8.4：托盘「退出」被点击，正在结束进程");
                app.exit(0);
            }
            other => tracing::warn!("★ M8.4：收到未知托盘菜单 id: {other}"),
        })
        .build(app)?;
    Ok(())
}

/// 把托盘意图发给主窗口；托盘菜单与 `tray_emit` 命令共用这一条路径
/// （验收脚本用 `tray_emit` 打的就是真实链路，不是另一套模拟）。
pub fn emit_action(app: &AppHandle, action: &str) {
    match app.emit_to("main", ACTION_EVENT, action.to_string()) {
        Ok(()) => tracing::info!("★ M8.4：托盘动作 {action:?} 已 emit 到主窗口（{ACTION_EVENT}）"),
        // 主窗口还没建/已销毁不算致命：托盘仍可用，前端下次起来也能自己拉状态。
        Err(e) => tracing::warn!("★ M8.4：托盘动作 {action:?} emit 失败: {e}"),
    }
}
