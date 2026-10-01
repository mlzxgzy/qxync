//! `qxync-gui` —— QSync for Linux 桌面应用（Tauri 2）。
//!
//! 定位（见 `docs/开发规划.md` §2「M4」）：**daemon 的图形客户端**，
//! 首版只做四件事——登录/连接配置、挂载管理、状态与进度面板、pin 管理。
//! 所有 NAS 操作都经 `qxyncd` 的 unix socket（`crate::ipc`），GUI 自己不发任何 HTTP。
//!
//! 前端是**零依赖静态资源**（`ui/`，无打包器、无 node_modules），
//! 通过 `withGlobalTauri` 暴露的 `window.__TAURI__.core.invoke` 调下面的命令。

pub mod commands;
pub mod ipc;
pub mod tray;

use serde_json::{json, Value};
use std::sync::OnceLock;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// 编译期嵌入的前端资源大小（`--self-test` 用它证明「UI 真的进包了」）。
pub fn ui_assets() -> Value {
    json!({
        "index_html": include_str!("../ui/index.html").len(),
        "app_js": include_str!("../ui/app.js").len(),
        "style_css": include_str!("../ui/style.css").len(),
    })
}

/// 启动 GUI 事件循环（阻塞直到窗口关闭 / 托盘「退出」）。
pub fn run() {
    init_logging();
    let result = tauri::Builder::default()
        // ★ M8.4：三个官方插件都只被**本进程的 Rust 命令**调用（见 commands.rs），
        // JS 侧不直接使用它们的 API —— 所以 capabilities/default.json 不必为它们开口子。
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            commands::app_info,
            commands::daemon_status,
            commands::ipc_call,
            commands::link_read,
            commands::link_save,
            commands::credential_save,
            commands::credential_present,
            commands::daemon_start,
            commands::daemon_stop,
            commands::login_flow,
            // ★ M8.4：托盘 / 通知 / 选择器 / opener / 窗口控制
            commands::m84_info,
            commands::notify_show,
            commands::pick_folder,
            commands::pick_file,
            commands::open_path,
            commands::open_url,
            commands::app_exe_path,
            commands::window_hide,
            commands::window_show,
            commands::app_quit,
            commands::tray_emit,
        ])
        // 托盘放在 setup：配置里的主窗口已经建好，`emit_to("main", ...)` 一定有目标。
        .setup(|app| {
            tray::setup(app.handle());
            Ok(())
        })
        .on_window_event(|window, event| {
            // ★ M8.4：关闭主窗口 = 收进托盘（Qsync 的默认行为）。
            let tauri::WindowEvent::CloseRequested { api, .. } = event else {
                return;
            };
            // 只管主窗口：以后若加「关于/设置」等独立窗口，关掉它们不该被拦下来。
            if window.label() != "main" {
                return;
            }
            let want_tray = close_to_tray_now();
            // 托盘没建起来（缺 appindicator / 当时没有 StatusNotifierHost）时必须**真的关闭**：
            // 窗口一隐藏就再也没人能把它叫回来，进程会变成用户够不着的僵尸 ——
            // 那不叫「降级为普通窗口」，叫挖坑。这里正好落回普通窗口的语义。
            if want_tray && !tray::created() {
                tracing::warn!(
                    "★ M8.4：close_to_tray=true 但托盘未创建成功，直接关闭窗口（藏起来就找不回来了）"
                );
                return;
            }
            if want_tray {
                // prevent_close + hide：窗口对象还在，事件循环因此不会退出，
                // 托盘菜单仍能把窗口叫回来（真正销毁窗口的话就只能重启进程了）。
                api.prevent_close();
                if let Err(e) = window.hide() {
                    tracing::warn!("★ M8.4：隐藏主窗口失败（按关闭处理）: {e}");
                } else {
                    tracing::info!("★ M8.4：主窗口已收进托盘（close_to_tray=true）");
                }
            } else {
                tracing::info!("★ M8.4：close_to_tray=false，主窗口正常关闭并退出");
            }
        })
        .run(tauri::generate_context!());
    if let Err(e) = result {
        tracing::error!("GUI 退出: {e}");
        eprintln!("❌ QSync GUI 启动失败: {e}");
        std::process::exit(1);
    }
}

/// ★ M8.4：关闭窗口时是否收进托盘。
///
/// **每次关闭都重新读盘**：`close_to_tray` 是运行时可改的设置（前端设置页会写
/// `settings.json`），缓存住的话用户改完得重启才生效。读失败（文件损坏/没权限）
/// 按 `true` 处理 —— 那正好是 `Settings::default()` 的取值，也和 QSync 一致。
fn close_to_tray_now() -> bool {
    match qxync_core::ConfigPaths::discover() {
        Ok(p) => qxync_core::Settings::load(&p).map(|s| s.close_to_tray).unwrap_or(true),
        Err(_) => true,
    }
}

/// 无窗口自检（`qxync-gui --self-test`）：给脚本/验收矩阵用。
///
/// 打印一行 JSON，返回是否通过。判定标准：
/// 1. 前端资源已嵌入且体积合理（>0）；
/// 2. daemon 在跑且 `status` 能解析（GUI 的全部价值都依赖这一条）；
/// 3. 若已登录，再打一次 `ls`（证明 GUI 的 IPC 通道能拿到真实 NAS 数据）。
pub fn self_test() -> bool {
    init_logging();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            println!("{}", json!({"ok": false, "error": format!("建 tokio 运行时失败: {e}")}));
            return false;
        }
    };
    let out = rt.block_on(self_test_inner());
    let ok = out["ok"].as_bool().unwrap_or(false);
    println!(
        "{}",
        serde_json::to_string_pretty(&out).unwrap_or_else(|_| "{}".into())
    );
    ok
}

/// 登录链自检（`qxync-gui --self-test-login`）：把 GUI「连接 / 登录」页真正会走的
/// 那条链路跑一遍 —— `daemon_stop` → `login_flow`（写 link + 写凭据 + 重新拉起 daemon
/// + IPC 登录）→ `daemon_status` 复核已登录。
///
/// 运行前 daemon 可以在跑也可以不在跑；跑完 daemon 一定在跑且已登录。
/// **口令只从 `credentials.json` 读，绝不打印**。
pub fn self_test_login() -> bool {
    init_logging();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            println!("{}", json!({"ok": false, "error": format!("建 tokio 运行时失败: {e}")}));
            return false;
        }
    };
    let out = rt.block_on(self_test_login_inner());
    let ok = out["ok"].as_bool().unwrap_or(false);
    println!(
        "{}",
        serde_json::to_string_pretty(&out).unwrap_or_else(|_| "{}".into())
    );
    ok
}

async fn self_test_login_inner() -> Value {
    let paths = match qxync_core::ConfigPaths::discover() {
        Ok(p) => p,
        Err(e) => return json!({"ok": false, "error": format!("定位配置目录失败: {e}")}),
    };
    let link = match qxync_core::LinkConfig::load(&paths, "default") {
        Ok(l) => l,
        Err(e) => return json!({"ok": false, "error": format!("读连接配置失败: {e}")}),
    };
    let cred = match qxync_core::Credentials::load(&paths) {
        Ok(c) => c,
        Err(e) => {
            return json!({"ok": false, "error": format!("读凭据失败（先 `qsync login --password`）: {e}")})
        }
    };
    if cred.host != link.host || cred.user != link.user {
        return json!({
            "ok": false,
            "error": format!("凭据与连接配置不匹配：凭据 {}@{} vs 配置 {}@{}", cred.user, cred.host, link.user, link.host),
        });
    }

    // 1) 停掉（可能本来就没跑）
    let stopped = commands::daemon_stop().await.unwrap_or(Value::Null);
    let stop_ok = stopped["stopped"].as_bool().unwrap_or(false)
        || stopped["already"].as_bool().unwrap_or(false);

    // 2) 走 GUI 的「保存并登录」整条链（含 daemon_start）
    let login = commands::login_flow(json!({
        "id": link.id,
        "host": link.host,
        "port": link.port,
        "https": link.https,
        "insecure": link.insecure,
        "user": link.user,
        "home_root": link.home_root,
        "ipv4_only": link.ipv4_only,
        "password": cred.password,
    }))
    .await
    .unwrap_or(Value::Null);
    let login_ok = login["ok"].as_bool().unwrap_or(false);

    // 3) 复核
    let st = commands::daemon_status().await.ok();
    let st_json = st.and_then(|s| serde_json::to_value(s).ok()).unwrap_or(Value::Null);
    let logged_in = st_json["status"]["logged_in"].as_bool().unwrap_or(false);
    let running = st_json["running"].as_bool().unwrap_or(false);

    json!({
        "ok": stop_ok && login_ok && running && logged_in,
        "stop_ok": stop_ok,
        "stopped": stopped["stopped"],
        "login_ok": login_ok,
        "login_error": login["login"]["error"]["message"],
        "login_data": login["login"]["data"],
        "link_path": login["link_path"],
        "credential_path": login["credential_path"],
        "restarted": login["restarted"],
        "daemon_running": running,
        "logged_in": logged_in,
        "session": st_json["status"]["session"],
    })
}

/// 通知自检（`qxync-gui --self-test-notify`）：**无窗口**起一个最小 Tauri app，
/// 用与 [`commands::notify_show`] 完全相同的代码发一条桌面通知，打印一行 JSON 后退出。
///
/// 为什么要有这条：`--self-test` 是纯 IPC 自检，碰不到 GTK/D-Bus；而通知链路的坑
/// 恰恰都在那里（没有通知守护、`dbus` 会话不对、插件没注册）。验收脚本用
/// `dbus-monitor --session "interface='org.freedesktop.Notifications'"` 抓 `Notify`
/// 方法调用来证明「真的发出去了」，所以这里必须是**真发一条**，不能只是自报成功。
///
/// 注意：本自检**不看 `desktop_notifications` 设置**。那条设置是「用户想不想收通知」
/// 的产品开关（由 `notify_show` 命令负责遵守），而这里是「通知链路通不通」的探针，
/// 关了开关也仍然要证明链路可用。
pub fn self_test_notify() -> bool {
    init_logging();

    // 兜底：无窗口的 Tauri 事件循环如果因为环境问题没能退出，别把验收脚本挂死。
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(10));
        eprintln!(
            "{}",
            json!({"ok": false, "shown": false, "error": "10s 超时：Tauri 事件循环没有退出"})
        );
        std::process::exit(3);
    });

    let mut ctx = tauri::generate_context!();
    // 无窗口：把配置里的窗口清单清空。否则 `setup()` 会照 tauri.conf.json 建出主窗口，
    // 自检就会在验收脚本里闪一个真窗口出来。
    ctx.config_mut().app.windows.clear();

    let outcome: std::sync::Arc<std::sync::Mutex<Option<Value>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let slot = outcome.clone();

    let app = match tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .build(ctx)
    {
        Ok(a) => a,
        Err(e) => {
            println!(
                "{}",
                json!({"ok": false, "shown": false, "error": format!("建 Tauri app 失败: {e}")})
            );
            return false;
        }
    };

    // 放在 `RunEvent::Ready` 而不是 `setup`：Ready 时插件已经 `initialize_plugins` 完毕，
    // 且事件循环已经跑起来，`exit(0)` 一定被处理（在 setup 里 exit 有可能被吞掉）。
    //
    // ⚠ 这里必须用 `run_return` 而不是 `run`：Linux 上 `run` 最终落到 tao 的
    // `EventLoop::run`，它**永不返回**（循环结束时直接 `process::exit(code)`），
    // 于是在 `run` 之后打印 JSON 是打印不出来的（这个坑踩过一次）。
    let loop_code = app.run_return(move |handle, event| {
        if !matches!(event, tauri::RunEvent::Ready) {
            return;
        }
        // ★ 尊重设置里的「显示桌面通知」：关掉时必须**如实**报 shown=false，
        //   而不是自检里偷偷发一条（那会让验收的「关掉后一个 Notify 都没有」变成假绿）。
        let result = if commands::notifications_enabled() {
            commands::show_notification(
                handle,
                "QSync 桌面通知自检",
                "如果你看到这条通知，说明 qxync-gui 的通知链路是通的。",
            )
        } else {
            Err("桌面通知已关闭".to_string())
        };
        let payload = match &result {
            Err(_) if !commands::notifications_enabled() => json!({
                "ok": true,
                "shown": false,
                "reason": "桌面通知已关闭",
                "title": "QSync 桌面通知自检",
                "note": "settings.json 里 desktop_notifications=false：按设置**不发**通知",
            }),
            Ok(()) => json!({
                "ok": true,
                "shown": true,
                "title": "QSync 桌面通知自检",
                // `shown=true` 的准确含义：已交给通知后端**异步**派发（插件内部 spawn 后
                // 立即返回，连 D-Bus 错误都被它吞掉了），不代表已经确认投递到托盘区。
                "dispatch": "async",
                "note": "shown=true 表示已交给通知后端派发；是否真正弹到桌面请看 dbus-monitor 抓到的 Notify 调用",
            }),
            Err(e) => json!({"ok": false, "shown": false, "error": e}),
        };
        // 事件循环的退出码与 JSON 保持一致（脚本两个都可能看）。
        let code = if payload["ok"].as_bool().unwrap_or(false) { 0 } else { 1 };
        *slot.lock().unwrap() = Some(payload);
        // 通知是 spawn 出去异步发的：立刻 exit 会让它随进程一起消失。
        // 给 D-Bus 往返留 ~700ms（正常几毫秒就够，这里只是别把自己坑了）。
        let handle = handle.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(700));
            handle.exit(code);
        });
    });

    let payload = outcome
        .lock()
        .unwrap()
        .take()
        .unwrap_or_else(|| json!({"ok": false, "shown": false, "error": "事件循环结束了但没跑过通知分支"}));
    // 事件循环在子线程里 request_exit 之后才返回，这里才轮到我们说话。
    {
        use std::io::Write as _;
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into())
        );
        let _ = std::io::stdout().flush();
    }
    if loop_code != 0 {
        tracing::warn!("★ M8.4：通知自检的事件循环退出码异常: {loop_code}");
    }
    // 成败以 JSON 为准（main 用它决定进程退出码）；`loop_code` 只用来核对 ——
    // 万一事件循环是被别的东西结束的，两者会不一致，下面那条 warning 就是线索。
    payload["ok"].as_bool().unwrap_or(false)
}

async fn self_test_inner() -> Value {
    let assets = ui_assets();
    let assets_ok = assets["index_html"].as_u64().unwrap_or(0) > 64
        && assets["app_js"].as_u64().unwrap_or(0) > 256
        && assets["style_css"].as_u64().unwrap_or(0) > 64;

    let info = commands::app_info().await.unwrap_or(Value::Null);
    let sock = ipc::socket_path();
    let running = ipc::available(&sock).await;

    let mut status_ok = false;
    let mut logged_in = false;
    let mut ls: Value = Value::Null;
    let mut ping: Value = Value::Null;
    if running {
        if let Ok(p) = ipc::call_typed::<qxync_core::ipc::PingData>(&sock, qxync_core::ipc::Request::Ping).await {
            ping = json!({"pid": p.pid, "version": p.daemon_version, "uptime_secs": p.uptime_secs});
        }
        if let Ok(st) = ipc::call_typed::<qxync_core::ipc::StatusData>(&sock, qxync_core::ipc::Request::Status).await {
            status_ok = true;
            logged_in = st.logged_in;
            if st.logged_in {
                // LinkInfo 不含 home_root（那是 link 文件的字段）；普通用户就是 /home。
                let home = qxync_core::HOME_ROOT.to_string();
                let resp = ipc::call(&sock, qxync_core::ipc::Request::Ls { path: home }).await;
                ls = json!({
                    "ok": resp["ok"],
                    "total": resp["data"]["total"],
                    "error": resp["error"]["message"],
                });
            }
        }
    }

    let ls_ok = !logged_in || ls["ok"].as_bool().unwrap_or(false);

    // ★ M8.4：桌面集成（托盘/通知/选择器/自启开关）也要能被脚本看见。
    // 这里复用 `m84_info` 命令本身，保证「脚本看到的」和「前端看到的」是同一份数据。
    let m84 = commands::m84_info().await.unwrap_or(Value::Null);

    // 挂载面板 / 同步面板用的两条**只读**请求也在这里打一遍：
    // 验收脚本据此确认「GUI 按钮会发的请求」在真实 daemon 上都能拿到数据。
    let mut mounts_ok = false;
    let mut mounts_count = 0u64;
    let mut sync_ok = false;
    if running {
        let m = ipc::call(&sock, qxync_core::ipc::Request::Mounts).await;
        mounts_ok = m["ok"].as_bool().unwrap_or(false);
        mounts_count = m["data"].as_array().map(|a| a.len() as u64).unwrap_or(0);
        let s = ipc::call(
            &sock,
            qxync_core::ipc::Request::Sync {
                once: Some(false),
                force_deletes: None,
                max_deletes: None,
                interval_secs: None,
            },
        )
        .await;
        sync_ok = s["ok"].as_bool().unwrap_or(false);
    }

    json!({
        "ok": assets_ok && running && status_ok && ls_ok && mounts_ok && sync_ok,
        "ui_assets": assets,
        "ui_assets_ok": assets_ok,
        "app_info": info,
        "daemon_running": running,
        "socket": sock.display().to_string(),
        "ping": ping,
        "status_ok": status_ok,
        "logged_in": logged_in,
        "ls": ls,
        "mounts_ok": mounts_ok,
        "mounts_count": mounts_count,
        "sync_ok": sync_ok,
        // ★ M8.4：桌面集成（托盘/通知/选择器/自启）自检。
        // `tray_code=true` 是**编译期**事实（托盘代码在这个二进制里，且 tauri 开了
        // `tray-icon` feature）；`tray_created=false` 是**运行时**事实 —— 本模式不开窗口，
        // 也就没有 `setup`，托盘从未被创建。两者不能混为一谈。
        "m84": {
            "plugins": m84["plugins"],
            "tray_code": true,
            "tray_created": false,
            "note": "无窗口自检不建托盘",
            "app_exe": m84["app_exe"],
            "autostart_path": m84["autostart_path"],
            "autostart_present": m84["autostart_present"],
            "close_to_tray": m84["close_to_tray"],
        },
    })
}

/// 日志：stderr + 按天滚动文件 `<state>/log/qsync-gui.log.YYYY-MM-DD`（与 daemon 同目录）。
fn init_logging() {
    static GUARD: OnceLock<tracing_appender::non_blocking::WorkerGuard> = OnceLock::new();
    static INIT: OnceLock<()> = OnceLock::new();
    if INIT.set(()).is_err() {
        return;
    }
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    let file_layer = qxync_core::ConfigPaths::discover().ok().and_then(|p| {
        let dir = p.log_dir();
        if std::fs::create_dir_all(&dir).is_err() {
            return None;
        }
        let appender = tracing_appender::rolling::daily(&dir, "qsync-gui.log");
        let (nb, guard) = tracing_appender::non_blocking(appender);
        let _ = GUARD.set(guard);
        Some(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(nb))
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();
}
