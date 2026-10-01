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

/// 启动 GUI 事件循环（阻塞直到窗口关闭）。
pub fn run() {
    init_logging();
    let result = tauri::Builder::default()
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
        ])
        .run(tauri::generate_context!());
    if let Err(e) = result {
        tracing::error!("GUI 退出: {e}");
        eprintln!("❌ QSync GUI 启动失败: {e}");
        std::process::exit(1);
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
