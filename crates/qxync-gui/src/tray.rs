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
//! * ★ **「建起来」≠「用户看得见」**。SNI 的宿主是 `org.kde.StatusNotifierWatcher`，
//!   但**有 watcher 不等于有 host**：裸 GNOME（没装 AppIndicator 扩展）、只跑了 watcher
//!   却没渲染托盘的面板、纯 XEmbed 桌面（IceWM/Fluxbox/老面板）都属于「名字注册成功、
//!   但没人画图标」。那种情况下把窗口藏进托盘 = 用户再也找不回窗口。
//!   所以这里在创建后**探测一次可用性**（watcher 存在 + `IsStatusNotifierHostRegistered`
//!   + 本进程的 item 出现在 `RegisteredStatusNotifierItems` 里），结果如实上报给
//!   `m84_info`，并作为「关窗是否隐藏」的判据。

use std::sync::{LazyLock, Mutex as StdMutex, OnceLock};
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

/// 托盘状态：`created`（对象建起来了）/ `probed`（探测跑完了）/ `visible`（有人会画它）
/// / `reason`（为什么是/不是可见 —— 日志与 `m84_info` 都靠它解释，不猜）。
#[derive(Debug, Clone, Default)]
pub struct TrayState {
    pub created: bool,
    pub probed: bool,
    pub visible: bool,
    pub reason: String,
    /// 探测到的 watcher 名字（没有 = 空）。
    pub watcher: String,
}

static TRAY_STATE: LazyLock<StdMutex<TrayState>> =
    LazyLock::new(|| StdMutex::new(TrayState::default()));

fn state() -> TrayState {
    TRAY_STATE.lock().unwrap().clone()
}

/// 供 `m84_info` 读取；`--self-test` 这类无窗口模式从没建过托盘，所以缺省是 `false`
/// （**不虚报**：没建就是没建）。
pub fn created() -> bool {
    *TRAY_CREATED.get().unwrap_or(&false)
}

/// **托盘是否真的会被画出来**（= 关窗隐藏的安全判据）。
///
/// 探测没跑完时返回 `false`：宁可「关窗真的关掉」，也不要藏一个用户可能找不回的窗口。
pub fn visible() -> bool {
    state().visible
}

/// 完整状态（`m84_info` 用）。
pub fn state_snapshot() -> TrayState {
    state()
}

/// 建托盘；无论成败都只记日志，绝不向上抛错（见模块头「托盘失败不能拖垮 GUI」）。
pub fn setup(app: &AppHandle) {
    let ok = match build(app) {
        Ok(()) => {
            tracing::info!("★ M8.4：系统托盘对象已创建（StatusNotifierItem 已交给 D-Bus）");
            true
        }
        Err(e) => {
            tracing::error!(
                "★ M8.4：系统托盘创建失败，降级为普通窗口（只是没有常驻图标，其它功能不受影响）: {e}"
            );
            false
        }
    };
    *TRAY_STATE.lock().unwrap() = TrayState {
        created: ok,
        probed: false,
        visible: false,
        reason: if ok {
            "托盘对象已创建，正在探测「有没有面板宿主」（StatusNotifierHost）".into()
        } else {
            "托盘对象创建失败".into()
        },
        watcher: String::new(),
    };
    // set 只可能失败一次（重复 setup），忽略即可。
    let _ = TRAY_CREATED.set(ok);
    if ok {
        // 探测放后台线程：ksni 是在自己的 worker 线程里注册名字的，
        // 注册完成可能比 `build()` 返回晚一点点，所以要带重试；
        // 而 setup 跑在 GTK 主线程上，**绝不能**在这里阻塞。
        std::thread::spawn(probe_visibility);
    }
}

/// 探测「托盘到底有没有人画」。
///
/// 判据三连（缺一不可）：
/// 1. 会话总线上有 `org.kde.StatusNotifierWatcher`（面板/DE 提供的规范 watcher）；
/// 2. 它的 `IsStatusNotifierHostRegistered` 为真（**有 watcher ≠ 有 host**：
///    裸 GNOME、只装了 watcher 没渲染托盘的面板都栽在这一条上）；
/// 3. 本进程的 item 出现在 `RegisteredStatusNotifierItems` 里（ksni 注册的名字形如
///    `org.kde.StatusNotifierItem-<pid>-<n>`；用 pid 匹配，不写死后缀）。
///
/// 最多重试 `TRIES` 次、每次间隔 `RETRY_MS`：注册是异步的，第一次读不到很正常。
fn probe_visibility() {
    const TRIES: u32 = 8;
    const RETRY_MS: u64 = 250;

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            finish(false, "", format!("建探测运行时失败: {e}"));
            return;
        }
    };
    let out = rt.block_on(async {
        let conn = match zbus::Connection::session().await {
            Ok(c) => c,
            Err(e) => return Probe::NoBus(format!("连不上会话总线: {e}")),
        };
        let pid_mark = format!("StatusNotifierItem-{}-", std::process::id());
        let mut last = Probe::NoWatcher;
        for _ in 0..TRIES {
            let proxy = match zbus::Proxy::new(
                &conn,
                "org.kde.StatusNotifierWatcher",
                "/StatusNotifierWatcher",
                "org.kde.StatusNotifierWatcher",
            )
            .await
            {
                Ok(p) => p,
                Err(e) => {
                    last = Probe::NoWatcher;
                    tracing::debug!("★ M8.4：托盘可见性探测：拿不到 watcher 代理（{e}）");
                    tokio::time::sleep(std::time::Duration::from_millis(RETRY_MS)).await;
                    continue;
                }
            };
            let host: bool = proxy
                .get_property("IsStatusNotifierHostRegistered")
                .await
                .unwrap_or(false);
            if !host {
                last = Probe::NoHost;
                tokio::time::sleep(std::time::Duration::from_millis(RETRY_MS)).await;
                continue;
            }
            let items: Vec<String> = proxy
                .get_property("RegisteredStatusNotifierItems")
                .await
                .unwrap_or_default();
            if items.iter().any(|i| i.contains(&pid_mark)) {
                return Probe::Visible;
            }
            last = Probe::NotRegistered(items.len());
            tokio::time::sleep(std::time::Duration::from_millis(RETRY_MS)).await;
        }
        last
    });

    match out {
        Probe::Visible => finish(
            true,
            "org.kde.StatusNotifierWatcher",
            "watcher + StatusNotifierHost 都在，且本进程的 item 已登记 → 面板会画这个图标".into(),
        ),
        Probe::NoBus(msg) => finish(false, "", msg),
        Probe::NoWatcher => finish(
            false,
            "",
            "会话总线上没有 org.kde.StatusNotifierWatcher：纯 XEmbed 桌面（IceWM/Fluxbox/老面板）             或裸 GNOME（没装 AppIndicator 扩展）—— 托盘图标不会出现"
                .into(),
        ),
        Probe::NoHost => finish(
            false,
            "org.kde.StatusNotifierWatcher",
            "watcher 在，但 IsStatusNotifierHostRegistered=false：没有面板在渲染托盘".into(),
        ),
        Probe::NotRegistered(n) => finish(
            false,
            "org.kde.StatusNotifierWatcher",
            format!("watcher 与 host 都在，但本进程的 item 没出现在已登记列表里（列表共 {n} 项）"),
        ),
    }
}

enum Probe {
    Visible,
    NoBus(String),
    NoWatcher,
    NoHost,
    NotRegistered(usize),
}

fn finish(visible: bool, watcher: &str, reason: String) {
    {
        let mut st = TRAY_STATE.lock().unwrap();
        st.probed = true;
        st.visible = visible;
        st.reason = reason.clone();
        st.watcher = watcher.to_string();
    }
    if visible {
        tracing::info!("★ M8.4：托盘可用（{reason}）");
    } else {
        tracing::warn!(
            "★ M8.4：托盘**不可见**（{reason}）→ 关闭主窗口时会真的关闭（不隐藏），             避免窗口藏起来之后用户找不回"
        );
    }
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
