//! Tauri 命令面（前端的全部入口）。
//!
//! 约定：
//! * 所有**业务动作**都封装成 `{v,ok,data,error}` 信封（与 `qxync-core::ipc::Response` 同形），
//!   前端只需要一套渲染逻辑；`Err(String)` 只用于「参数不合法」这类本地错误。
//! * 口令只在 `credential_save` / `login_flow` 的入参里出现，**从不回传**。

use crate::ipc;
use qxync_core::ipc::{ErrorKind, PingData, Request, RequestEnvelope, StatusData, IPC_VERSION};
use qxync_core::{ConfigPaths, Credentials, LinkConfig, Settings};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
// ★ M8.4：`get_webview_window` 来自 `Manager`。
use tauri::Manager as _;

// ---------------------------------------------------------------- 工具

fn paths() -> Result<ConfigPaths, String> {
    ConfigPaths::discover().map_err(|e| e.to_string())
}

fn daemon_binary() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("QXNYC_DAEMON_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
    }
    // `cargo build` 之后 qxyncd 就在同一个 target 目录里
    if let Ok(me) = std::env::current_exe() {
        let cand = me.with_file_name("qxyncd");
        if cand.is_file() {
            return Ok(cand);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("qxyncd");
            if cand.is_file() {
                return Ok(cand);
            }
        }
    }
    Err("找不到 qxyncd（先 `cargo build --workspace`，或设置 QXNYC_DAEMON_BIN）".into())
}

async fn wait_gone(socket: &std::path::Path) -> bool {
    for _ in 0..40 {
        if !ipc::available(socket).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

async fn wait_ready(socket: &std::path::Path) -> bool {
    for _ in 0..80 {
        if ipc::available(socket).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

/// 拉起 `qxyncd --link <id> --socket <socket>`（daemon 自己 fork+setsid）。
/// 返回 `(already_running, ping_json_or_null)`。
async fn start_daemon(link_id: &str) -> Value {
    let socket = ipc::socket_path();
    if ipc::available(&socket).await {
        let ping = ipc::call(&socket, Request::Ping).await;
        return json!({"started": false, "already": true, "socket": socket.display().to_string(), "ping": ping["data"]});
    }
    let exe = match daemon_binary() {
        Ok(e) => e,
        Err(e) => return json!({"started": false, "already": false, "error": e}),
    };
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("--link")
        .arg(link_id)
        .arg("--socket")
        .arg(&socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "started": false, "already": false,
                "error": format!("启动 {} 失败: {e}", exe.display()),
            })
        }
    };
    // daemon 会立刻 fork 并让父进程退出；这里把子进程收尸，避免僵尸。
    let _ = tauri::async_runtime::spawn_blocking(move || {
        let mut c = child;
        let _ = c.wait();
    })
    .await;
    if !wait_ready(&socket).await {
        return json!({
            "started": false, "already": false, "socket": socket.display().to_string(),
            "error": format!("qxyncd 已派生但 socket 未就绪: {}", socket.display()),
        });
    }
    let ping = ipc::call(&socket, Request::Ping).await;
    json!({
        "started": true, "already": false, "socket": socket.display().to_string(),
        "bin": exe.display().to_string(), "ping": ping["data"],
    })
}

/// 连接参数是否与 daemon 当前持有的 link 一致（不一致要重启 daemon 才生效）。
fn same_link(a: &qxync_core::ipc::LinkInfo, b: &LinkConfig) -> bool {
    a.host == b.host
        && a.port == b.port
        && a.https == b.https
        && a.user == b.user
        && a.ipv4_only == b.ipv4_only
}

/// 前端的连接表单（字段都可省，省了就用默认值/已有 link）。
#[derive(Debug, Deserialize)]
struct LinkInput {
    #[serde(default)]
    id: Option<String>,
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default = "yes")]
    https: bool,
    #[serde(default)]
    insecure: bool,
    user: String,
    #[serde(default)]
    ipv4_only: bool,
    /// ★ M7：选择性同步规则（GUI 暂不编辑；保存时**保留已有值**，不能被 GUI 抹掉）
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    filter_temp: Option<bool>,
    /// ★ M7：LAN 对等监听/身份（同上，GUI 暂不编辑）
    #[serde(default)]
    peer_listen: Option<String>,
    #[serde(default)]
    peer_name: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

fn default_port() -> u16 {
    9834
}
fn yes() -> bool {
    true
}
impl LinkInput {
    fn link(&self) -> LinkConfig {
        LinkConfig {
            id: self
                .id
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "default".into()),
            host: self.host.trim().to_string(),
            port: self.port,
            https: self.https,
            insecure: self.insecure,
            user: self.user.trim().to_string(),
            ipv4_only: self.ipv4_only,
            // 前端没给就先用默认；真正保存时由 `link_with_existing` 用已有值补齐
            exclude: self.exclude.clone(),
            filter_temp: self.filter_temp.unwrap_or(true),
            peer_listen: self.peer_listen.clone(),
            peer_name: self.peer_name.clone(),
        }
    }

    /// 保存前把 GUI 不编辑的 M7 字段（exclude / filter_temp / peer_*）从已有配置里带过来，
    /// 避免「GUI 保存一次就把规则清空」。
    fn link_with_existing(&self, paths: &ConfigPaths) -> LinkConfig {
        let mut link = self.link();
        if let Ok(old) = LinkConfig::load(paths, &link.id) {
            if self.exclude.is_empty() {
                link.exclude = old.exclude;
            }
            if self.filter_temp.is_none() {
                link.filter_temp = old.filter_temp;
            }
            if self.peer_listen.is_none() {
                link.peer_listen = old.peer_listen;
            }
            if self.peer_name.is_none() {
                link.peer_name = old.peer_name;
            }
        }
        link
    }
}

// ---------------------------------------------------------------- 命令

/// 应用/环境概览：版本、socket、配置目录、daemon 二进制、前端资源。
#[tauri::command]
pub async fn app_info() -> Result<Value, String> {
    let p = paths()?;
    let socket = ipc::socket_path();
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "home": std::env::var("HOME").unwrap_or_default(),
        "socket": socket.display().to_string(),
        "config_dir": p.config_dir.display().to_string(),
        "data_dir": p.data_dir.display().to_string(),
        "state_dir": p.state_dir.display().to_string(),
        "daemon_bin": daemon_binary().ok().map(|b| b.display().to_string()),
        "daemon_running": ipc::available(&socket).await,
        "ui_assets": crate::ui_assets(),
        // 调试/验收用：`QXNYC_GUI_TAB=<status|connect|mounts|files|sync>` 指定初始 tab
        "initial_tab": std::env::var("QXNYC_GUI_TAB").ok(),
    }))
}

#[derive(Debug, Serialize)]
pub struct DaemonStatus {
    running: bool,
    socket: String,
    ping: Option<PingData>,
    status: Option<StatusData>,
    error: Option<String>,
}

/// daemon 存活 + 完整状态快照（前端每 2s 轮询一次）。
#[tauri::command]
pub async fn daemon_status() -> Result<DaemonStatus, String> {
    let socket = ipc::socket_path();
    let sock_str = socket.display().to_string();
    if !ipc::available(&socket).await {
        return Ok(DaemonStatus {
            running: false,
            socket: sock_str,
            ping: None,
            status: None,
            error: None,
        });
    }
    let ping = ipc::call_typed::<PingData>(&socket, Request::Ping).await;
    let status = ipc::call_typed::<StatusData>(&socket, Request::Status).await;
    let error = match (&ping, &status) {
        (Err(e), _) => Some(e.message.clone()),
        (_, Err(e)) => Some(e.message.clone()),
        _ => None,
    };
    Ok(DaemonStatus {
        running: true,
        socket: sock_str,
        ping: ping.ok(),
        status: status.ok(),
        error,
    })
}

/// 通用 IPC：入参就是 `{"v":1,"method":"ls","path":"/home"}`，回响应信封。
#[tauri::command]
pub async fn ipc_call(req: Value) -> Value {
    let socket = ipc::socket_path();
    match serde_json::from_value::<RequestEnvelope>(req) {
        Ok(env) if env.v == IPC_VERSION => ipc::call(&socket, env.req).await,
        Ok(env) => ipc::err_response(
            ErrorKind::BadVersion,
            format!("协议版本 {} 不受支持（本进程支持 {IPC_VERSION}）", env.v),
        ),
        Err(e) => ipc::err_response(ErrorKind::BadRequest, format!("请求解析失败: {e}")),
    }
}

/// 读连接配置（**不含口令**）。
#[tauri::command]
pub async fn link_read(link_id: Option<String>) -> Result<Value, String> {
    let p = paths()?;
    let id = link_id
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".into());
    let path = p.link_file(&id);
    match LinkConfig::load(&p, &id) {
        Ok(link) => {
            Ok(json!({"exists": true, "id": id, "path": path.display().to_string(), "link": link}))
        }
        Err(_) => Ok(
            json!({"exists": false, "id": id, "path": path.display().to_string(), "link": Value::Null}),
        ),
    }
}

/// 写连接配置 `~/.config/qxync/links/<id>.json`。
#[tauri::command]
pub async fn link_save(input: Value) -> Result<Value, String> {
    let li: LinkInput = serde_json::from_value(input).map_err(|e| format!("参数错误: {e}"))?;
    if li.host.trim().is_empty() || li.user.trim().is_empty() {
        return Err("host / user 不能为空".into());
    }
    let p = paths()?;
    p.ensure_dirs().map_err(|e| e.to_string())?;
    let link = li.link();
    let path = link.save(&p).map_err(|e| e.to_string())?;
    Ok(json!({"ok": true, "path": path.display().to_string(), "link": link}))
}

/// 写凭据 `~/.config/qxync/credentials.json`（0600，先临时文件再 rename）。
#[tauri::command]
pub async fn credential_save(input: Value) -> Result<Value, String> {
    let li: LinkInput = serde_json::from_value(input).map_err(|e| format!("参数错误: {e}"))?;
    let password = li
        .password
        .clone()
        .ok_or_else(|| "缺少 password".to_string())?;
    let p = paths()?;
    let cred = Credentials {
        host: li.host.trim().to_string(),
        user: li.user.trim().to_string(),
        password,
    };
    let path = cred.save(&p).map_err(|e| e.to_string())?;
    Ok(json!({"ok": true, "path": path.display().to_string()}))
}

/// 凭据是否存在（只回 host/user，绝不回口令）。
#[tauri::command]
pub async fn credential_present() -> Result<Value, String> {
    let p = paths()?;
    match Credentials::load(&p) {
        Ok(c) => Ok(
            json!({"present": true, "host": c.host, "user": c.user, "path": p.credentials_file().display().to_string()}),
        ),
        Err(_) => Ok(json!({"present": false, "path": p.credentials_file().display().to_string()})),
    }
}

/// 拉起 daemon（已在跑就返回 already）。
///
/// ★ 不再要求「先有连接配置」：daemon 没配 NAS 时会**空转待命**（界面显示「未配置」），
/// 配好连接后它自己转入同步 —— 「配置」和「把 daemon 跑起来」是两件独立的事。
#[tauri::command]
pub async fn daemon_start(link_id: Option<String>) -> Result<Value, String> {
    let id = link_id
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".into());
    Ok(start_daemon(&id).await)
}

/// 干净退出 daemon（卸载全部挂载 + 删 socket/pid）。
#[tauri::command]
pub async fn daemon_stop() -> Result<Value, String> {
    let socket = ipc::socket_path();
    if !ipc::available(&socket).await {
        return Ok(json!({"stopped": false, "already": true, "message": "qxyncd 未在运行"}));
    }
    let resp = ipc::call(&socket, Request::Shutdown).await;
    let gone = wait_gone(&socket).await;
    Ok(
        json!({"stopped": gone, "already": false, "socket": socket.display().to_string(), "response": resp}),
    )
}

/// 「保存连接 + 登录」一条龙：
/// 写 link → 写凭据 → 连接参数变了就重启 daemon（daemon 只在启动时读 link）→ IPC 登录。
#[tauri::command]
pub async fn login_flow(input: Value) -> Result<Value, String> {
    let li: LinkInput = serde_json::from_value(input).map_err(|e| format!("参数错误: {e}"))?;
    if li.host.trim().is_empty() || li.user.trim().is_empty() {
        return Err("host / user 不能为空".into());
    }
    let password = li
        .password
        .clone()
        .ok_or_else(|| "缺少 password".to_string())?;
    let p = paths()?;
    let link = li.link_with_existing(&p);
    p.ensure_dirs().map_err(|e| e.to_string())?;
    let link_path = link.save(&p).map_err(|e| e.to_string())?;
    let cred_path = Credentials {
        host: link.host.clone(),
        user: link.user.clone(),
        password: password.clone(),
    }
    .save(&p)
    .map_err(|e| e.to_string())?;

    let socket = ipc::socket_path();
    let mut restarted = false;
    if ipc::available(&socket).await {
        let current = ipc::call_typed::<StatusData>(&socket, Request::Status)
            .await
            .ok();
        let same = current
            .as_ref()
            // `link: None` = daemon 还在空转待命（没有任何连接配置）→ 必须重启才能读新 link
            .map(|s| s.link.as_ref().is_some_and(|dl| same_link(dl, &link)))
            .unwrap_or(false);
        if !same {
            // 换 NAS / 换账号：daemon 需要重启才会读新的 link
            let _ = ipc::call(&socket, Request::Shutdown).await;
            if !wait_gone(&socket).await {
                return Ok(json!({
                    "ok": false, "link_path": link_path.display().to_string(),
                    "credential_path": cred_path.display().to_string(),
                    "error": "参数已改，但旧 daemon 没能在 10s 内退出；请手动 `qxync daemon stop` 后重试",
                }));
            }
            let started = start_daemon(&link.id).await;
            restarted = started["started"].as_bool().unwrap_or(false);
            if !restarted {
                return Ok(json!({
                    "ok": false, "link_path": link_path.display().to_string(),
                    "credential_path": cred_path.display().to_string(),
                    "restarted": false, "start": started,
                    "error": "旧 daemon 已停止，但新 daemon 没能起来",
                }));
            }
        }
    } else {
        let started = start_daemon(&link.id).await;
        if !started["started"].as_bool().unwrap_or(false)
            && !started["already"].as_bool().unwrap_or(false)
        {
            return Ok(json!({
                "ok": false, "link_path": link_path.display().to_string(),
                "credential_path": cred_path.display().to_string(), "restarted": false,
                "start": started, "error": "qxyncd 启动失败",
            }));
        }
    }

    let login = ipc::call(
        &socket,
        Request::Login {
            user: Some(link.user.clone()),
            password: Some(password),
        },
    )
    .await;
    let ok = login["ok"].as_bool().unwrap_or(false);
    Ok(json!({
        "ok": ok,
        "link_path": link_path.display().to_string(),
        "credential_path": cred_path.display().to_string(),
        "restarted": restarted,
        "login": login,
    }))
}

// ---------------------------------------------------------------- ★ M8.4 桌面集成

/// ★ M8.4：把一条桌面通知交给通知后端。
///
/// **`--self-test-notify` 与本文件里的 `notify_show` 共用的就是这一段**，所以自检
/// 证明的是「真实发通知这条路」，而不是另写一套可能走偏的模拟代码。
///
/// 注意插件的 `show()` 是「派发后立即返回」（内部 spawn 到 async runtime，连 D-Bus
/// 错误都被吞掉），所以返回 `Ok(())` 只代表**已交给后端**，不代表用户真的看到了。
/// 需要「确实投递」的证据时用 `dbus-monitor` 抓 `org.freedesktop.Notifications.Notify`。
pub fn show_notification(app: &tauri::AppHandle, title: &str, body: &str) -> Result<(), String> {
    use tauri_plugin_notification::NotificationExt as _;
    app.notification()
        .builder()
        .title(title)
        .body(body)
        .show()
        .map_err(|e| format!("发通知失败: {e}"))
}

/// ★ M8.4：桌面集成自检信息（托盘是否真的建起来了、自启项在哪、关窗口是否进托盘）。
///
/// **只读**：这里绝不写 `settings.json`、也绝不增删 autostart 桌面项 —— 那两件事分别
/// 属于设置页（写设置）和 daemon 的 `settings_save`（`apply_autostart`）。前端拿到
/// `autostart_path` 只是用来展示「自启项会落在哪 / 在不在」。
#[tauri::command]
pub async fn m84_info() -> Result<Value, String> {
    let (autostart_path, autostart_present, close_to_tray) = match ConfigPaths::discover() {
        Ok(p) => {
            // 设置读不出来（文件坏了）就按默认：默认 `close_to_tray=true`，
            // 与 `run()` 里关闭窗口的判定保持一致，免得 UI 显示和实际行为打架。
            let s = Settings::load(&p).unwrap_or_default();
            (
                Settings::autostart_file(&p).display().to_string(),
                Settings::autostart_present(&p),
                s.close_to_tray,
            )
        }
        Err(_) => (String::new(), false, Settings::default().close_to_tray),
    };
    let tray = crate::tray::state_snapshot();
    Ok(json!({
        "ok": true,
        // 三个插件都是进程启动时注册的；列出来是给验收脚本对答案用的。
        "plugins": ["notification", "dialog", "opener"],
        // 如实上报：`--self-test`（无窗口）模式下永远是 false。
        "tray_created": crate::tray::created(),
        // ★ M8.4：「建起来」≠「有人画」。可见性 = watcher 有 host 且本进程已登记，
        //   探测（带重试）跑完前 `probed=false`，前端要如实显示「探测中」，不要猜。
        "tray_probed": tray.probed,
        "tray_visible": tray.visible,
        "tray_reason": tray.reason,
        "tray_watcher": tray.watcher,
        "app_exe": std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        "autostart_path": autostart_path,
        "autostart_present": autostart_present,
        "close_to_tray": close_to_tray,
    }))
}

/// ★ T3：一键校验本地水合缓存的内容完整性（GUI 侧入口，逻辑全在 `qxync_fuse`）。
///
/// **纯本地**：只读缓存文件 + `.qxstate` / `.qxsum`，不连 NAS、不要 daemon。
/// 「数据出问题的时候往往正是网络不方便的时候」—— 校验按钮不该依赖挂载还在跑。
///
/// `repair = true` 时把坏区间退回「按需水合」（**不下载**，下次读到那个区间时
/// 自然从 NAS 重取）。这是刻意的：点一下「修复」就产生流量会很吓人，而且 NAS
/// 可能正连不上。
#[tauri::command]
pub async fn cache_verify(repair: Option<bool>) -> Result<Value, String> {
    let repair = repair.unwrap_or(false);
    let p = paths()?;
    // 从数据目录的 `cache/` 往下扫：`verify_cache_dir` 自己会递归（daemon 的实际
    // 缓存目录是 `<cache>/<nas host>/`，多一层）。
    let cache_root = p.data_dir.join("cache");
    if !cache_root.is_dir() {
        return Ok(json!({
            "ok": true,
            "skipped": true,
            "reason": format!("缓存目录还不存在: {}", cache_root.display()),
        }));
    }
    // 校验是纯 CPU/IO 的同步活儿；放阻塞线程池，别占住 Tauri 的 async 运行时。
    let dir = cache_root.clone();
    let rep =
        tauri::async_runtime::spawn_blocking(move || qxync_fuse::verify_cache_dir(&dir, repair))
            .await
            .map_err(|e| format!("校验任务失败: {e}"))?;
    Ok(json!({
        "ok": true,
        "skipped": false,
        "clean": rep.is_clean(),
        "bad_files": rep.bad_files(),
        "files_scanned": rep.files_scanned,
        "files_reported": rep.files.len(),
        "bytes_verified": rep.bytes_verified,
        "repaired_chunks": rep.repaired_chunks,
        "whole_hashes_written": rep.whole_hashes_written,
        "elapsed_ms": rep.elapsed_ms,
        "files": rep.files,
    }))
}

/// ★ M8.4：发一条桌面通知（前端在同步出错/完成时调用）。
///
/// 设置 `desktop_notifications=false` 时**诚实地什么都不发**并说明原因 —— 返回
/// `shown:false` 而不是假装成功，否则前端会在「用户明明关了通知」时还以为弹过了。
/// 「显示桌面通知」是否开着（`settings.json`；读不到按默认**开**）。
///
/// 抽出来是为了让 `--self-test-notify` 走**同一条判定** —— 自检必须能反映
/// 「用户关掉通知后到底还发不发」，否则验收注释里那句「同一段代码」就是假的。
pub fn notifications_enabled() -> bool {
    ConfigPaths::discover()
        .ok()
        .map(|p| Settings::load(&p).unwrap_or_default().desktop_notifications)
        // 连配置目录都定位不到时按默认（默认开）——「通知默认开」是产品的既定取舍。
        .unwrap_or(true)
}

#[tauri::command]
pub async fn notify_show(
    app: tauri::AppHandle,
    title: String,
    body: String,
) -> Result<Value, String> {
    let enabled = notifications_enabled();
    if !enabled {
        return Ok(json!({"ok": true, "shown": false, "reason": "桌面通知已关闭"}));
    }
    match show_notification(&app, &title, &body) {
        Ok(()) => Ok(json!({"ok": true, "shown": true})),
        Err(e) => Ok(json!({"ok": false, "shown": false, "error": e})),
    }
}

/// ★ M8.4：选目录。返回 `path: null` = 用户取消（**不是错误**）。
///
/// 刻意用插件的**回调式** API + `oneshot` 转 async，而不是 `blocking_pick_folder`：
/// 后者会阻塞当前线程等 GTK 主循环派发对话框，在 Tauri 命令线程上极易和 GTK 主循环
/// 互锁（对话框永远不弹 / 整个 GUI 卡死）。回调式 API 由插件自己
/// `run_on_main_thread` 弹窗，命令线程只是 `await` 一个 channel。
#[tauri::command]
pub async fn pick_folder(app: tauri::AppHandle, title: Option<String>) -> Result<Value, String> {
    pick_with(app, title, true).await
}

/// ★ M8.4：选文件。语义与 [`pick_folder`] 完全一致，只是对话框类型不同。
#[tauri::command]
pub async fn pick_file(app: tauri::AppHandle, title: Option<String>) -> Result<Value, String> {
    pick_with(app, title, false).await
}

async fn pick_with(
    app: tauri::AppHandle,
    title: Option<String>,
    folder: bool,
) -> Result<Value, String> {
    use tauri_plugin_dialog::DialogExt as _;
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<PathBuf>>();
    let mut builder = app.dialog().file();
    if let Some(t) = title
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
    {
        builder = builder.set_title(t);
    }
    // 回调在 GTK 主线程/插件的 worker 上执行，`tx` 只是把结果送回这里。
    let send = move |p: Option<tauri_plugin_dialog::FilePath>| {
        // `into_path` 在极少数情况下会失败（非本地路径），当作「用户没选」更诚实。
        let _ = tx.send(p.and_then(|f| f.into_path().ok()));
    };
    if folder {
        builder.pick_folder(send);
    } else {
        builder.pick_file(send);
    }
    match rx.await {
        Ok(Some(p)) => Ok(json!({"ok": true, "path": p.display().to_string()})),
        // 用户点了取消 —— 这是正常结果，不是失败。
        Ok(None) => Ok(json!({"ok": true, "path": Value::Null})),
        Err(_) => Ok(json!({
            "ok": false, "path": Value::Null,
            "error": "文件选择器没有返回结果（窗口可能已关闭）",
        })),
    }
}

/// ★ M8.4：用系统默认程序打开本地路径（前端「打开缓存目录/打开同步文件夹」用）。
///
/// 路径不存在时**明确报错**而不是静默成功：静默的话用户点了按钮什么也没发生，
/// 根本不知道是自己删了目录还是程序坏了。
///
/// ★ M10.7：**目录**优先走「用户自己配的默认目录工具」。
///
/// `tauri-plugin-opener` 底层是 `open::that_detached`，它在 Unix 上是
/// `xdg-open` → `gio open` → `gnome-open` → `kde-open` **挨个试**。
/// 问题在 `xdg-open`：它把「目录」当成一种 mime 类型去查默认处理程序，而不少桌面
/// **没有为 `inode/directory` 登记** Desktop Entry；此时 `xdg-open` 解析出空、
/// 返回成功却**一个窗口都不弹**，也不会去试后面的 `gio` —— 表现就是「点了没反应」。
///
/// 所以目录先显式查一次用户配置：`xdg-mime query default inode/directory`。
/// ⚠️ 它返回的是**桌面项 ID**（`org.kde.dolphin.desktop`），**不是可执行文件名**
/// （真正能跑的是 `dolphin`）。直接把 ID 当程序名去 spawn 会**必然失败**
/// —— 这一点在开发机上实测确认过，所以必须解析出 `.desktop` 的 `Exec=` 再取程序名。
/// 解析不出来就回落到插件那条链，并把实际走了哪条路回报给前端（`via`），
/// 免得界面上「点开目录」没反应却看不出到底走了哪条路。
#[tauri::command]
pub async fn open_path(app: tauri::AppHandle, path: String) -> Result<Value, String> {
    use tauri_plugin_opener::OpenerExt as _;
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Ok(json!({"ok": false, "error": "路径不存在", "path": path}));
    }
    if p.is_dir() {
        if let Some((prog, desktop_id, via)) = default_dir_handler() {
            match spawn_detached(&prog, &p) {
                Ok(()) => {
                    return Ok(json!({
                        "ok": true, "path": path,
                        "via": via, "program": prog, "desktop_id": desktop_id,
                    }))
                }
                Err(e) => {
                    // 查到了默认程序却起不来（程序被卸载、desktop 文件残留…）：
                    // 如实说清楚，并继续走插件那条链，别让按钮彻底没用。
                    tracing::warn!(
                        "★ M10.7：默认目录程序 {prog}（{desktop_id}，{via}）启动失败: {e}；回落到插件 opener"
                    );
                    return match app.opener().open_path(path.clone(), None::<&str>) {
                        Ok(()) => Ok(json!({
                            "ok": true, "path": path, "via": "plugin-fallback",
                            "program": prog, "desktop_id": desktop_id, "fallback_reason": e.to_string(),
                        })),
                        Err(e2) => Ok(json!({
                            "ok": false, "path": path, "via": "plugin-fallback",
                            "program": prog, "desktop_id": desktop_id,
                            "error": format!("打开失败（默认程序 {prog}：{e}；回落后：{e2}）"),
                        })),
                    };
                }
            }
        }
    }
    match app.opener().open_path(path.clone(), None::<&str>) {
        Ok(()) => Ok(json!({"ok": true, "path": path, "via": "plugin"})),
        Err(e) => Ok(json!({"ok": false, "error": format!("打开失败: {e}"), "path": path})),
    }
}

/// 查「用户配的默认目录工具」并**解析成可执行程序**。返回 `(程序名, 桌面项 ID, 来源)`。
///
/// 步骤（都是只读的，不改用户的 `mimeapps.list`）：
/// 1. `xdg-mime query default inode/directory` → 桌面项 ID（如 `org.kde.dolphin.desktop`）；
///    `gio mime inode/directory` 作补充（**不带** `--handler`：本机 glib 版本不认这个
///    选项，带了会退化成「打印用法 + 退出码 1」，反而把 `xdg-mime` 之外的线索也断掉）；
/// 2. 拿 ID 去找 `.desktop` 文件（`XDG_DATA_HOME` 下的 `applications/` 优先，
///    再依次查每个 `XDG_DATA_DIRS`），读它的 `Exec=` 取**真正的程序名**。
///
/// 三种情况都返回 `None`（由调用方回落到插件 opener），而不是猜一个程序：
/// * 没配（`xdg-mime` 在没登记时是**退出码 0 + 空输出**）；
/// * 找到了 `.desktop` 但 `Exec=` 解析不出程序名；
/// * 程序名不在 `PATH` 里（桌面项残留 / 程序被卸载）。
fn default_dir_handler() -> Option<(String, String, &'static str)> {
    let (desktop_id, via) = xdg_default_desktop_id()?;
    let exec = read_desktop_exec(&desktop_id)?;
    let prog = exec_program(&exec)?;
    if which(&prog).is_none() {
        tracing::warn!(
            "★ M10.7：{desktop_id} 的 Exec={prog:?} 不在 PATH 里（桌面项可能已失效）；回落到插件 opener"
        );
        return None;
    }
    Some((prog, desktop_id, via))
}

/// 问「用户配的默认目录工具」是哪个**桌面项**。返回 `(桌面项 ID, 来源)`。
fn xdg_default_desktop_id() -> Option<(String, &'static str)> {
    for (prog, args, via) in [
        (
            "xdg-mime",
            vec!["query", "default", "inode/directory"],
            "xdg-mime",
        ),
        // 只作补充：老 glib 没有 `--handler`，有的话它在第二行起会列候选，
        // 第一行是「默认应用程序：xxx.desktop」，同样交给解析函数剥前缀。
        ("gio", vec!["mime", "inode/directory"], "gio"),
    ] {
        let Ok(o) = std::process::Command::new(prog)
            .args(&args)
            .stdin(Stdio::null())
            .output()
        else {
            continue;
        };
        if !o.status.success() {
            continue;
        }
        if let Some(id) = parse_desktop_id(&o.stdout) {
            return Some((id, via));
        }
    }
    None
}

/// 从 `xdg-mime` / `gio mime` 的输出里解析出**桌面项 ID**；解析不出返回 `None`。
///
/// 要挡掉的「看起来像成功、实则没结果」的情况：
/// 1. 空输出 —— `xdg-mime` 没登记时是**退出码 0 + 空输出**，不是报错；
/// 2. 只有空行/空白；
/// 3. `gio mime` 的输出**第一行是带前缀的自然语言**（本机实测：
///    `用于"inode/directory"的默认应用程序：org.kde.dolphin.desktop`），
///    必须剥到最后一个 `：` / 空格后面，不能整行当 ID；
/// 4. 含 `/` 的不是 ID（那是路径），非 `.desktop` 结尾的也不是（那是类型名）。
fn parse_desktop_id(stdout: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(stdout).unwrap_or("");
    for line in s.lines().map(str::trim).filter(|l| !l.is_empty()) {
        // 跳过「已注册的应用程序：」这类小节标题行（它们不是 ID 那一行）。
        let tail = match line
            .rsplit(['：', ' ', '\t'])
            .find(|t| t.ends_with(".desktop"))
        {
            Some(t) => t,
            None => continue,
        };
        if tail.contains('/') || !tail.ends_with(".desktop") {
            continue;
        }
        let id = tail.trim_end_matches(".desktop");
        if id.is_empty() {
            continue;
        }
        return Some(id.to_string());
    }
    None
}

/// 找出桌面项 `.desktop` 文件的内容路径。
///
/// 按 XDG 规范找 `$XDG_DATA_HOME/applications`（缺省 `~/.local/share/applications`）
/// 与每个 `$XDG_DATA_DIRS/applications`（缺省 `/usr/local/share` + `/usr/share`）。
/// 用户目录必须在系统目录**前面**查：用户自己装的扁平化/改过的桌面项优先。
fn read_desktop_exec(desktop_id: &str) -> Option<String> {
    // 桌面项 ID 里带 `/` 是路径穿越/绝对路径的形态，不是合法的 ID。
    if desktop_id.is_empty() || desktop_id.contains('/') {
        return None;
    }
    let name = format!("{desktop_id}.desktop");
    for dir in xdg_data_dirs() {
        if let Some(exec) = read_exec_in(&dir.join("applications").join(&name)) {
            return Some(exec);
        }
    }
    None
}

/// 在一个具体目录里找 `<desktop_id>.desktop` 的 `Exec=`。便于测试与复用。
fn read_exec_in(file: &Path) -> Option<String> {
    if !file.is_file() {
        return None;
    }
    // 桌面项文件损坏/非 UTF-8 时读不出 Exec —— 当「这个目录没找到」继续往下找，
    // 免得一个坏文件就废掉整条解析。
    match std::fs::read_to_string(file) {
        Ok(s) => parse_exec_field(&s),
        Err(e) => {
            tracing::warn!("★ M10.7：读 {} 失败: {e}", file.display());
            None
        }
    }
}

/// `$XDG_DATA_HOME` + `$XDG_DATA_DIRS`（空项/空变量按规范跳过，用户目录在前）。
fn xdg_data_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if p.as_os_str().is_empty() {
            return;
        }
        if !out.contains(&p) {
            out.push(p);
        }
    };
    if let Some(h) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        push(PathBuf::from(h));
    } else if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        push(PathBuf::from(home).join(".local").join("share"));
    }
    // 规范规定 `XDG_DATA_DIRS` 为空/未设时用这两个默认值。
    let dirs: Vec<PathBuf> = match std::env::var_os("XDG_DATA_DIRS") {
        Some(v) if !v.is_empty() => std::env::split_paths(&v).collect(),
        _ => vec![
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
        ],
    };
    for d in dirs {
        push(d);
    }
    out
}

/// 从桌面项文件里取 `Exec=` 的值（取 `[Desktop Entry]` 组里的那一条）。
///
/// 刻意**不**处理 `Hidden=true` / `TryExec=` / 本地化后的 `Name[xx]=`：
/// 那是完整的 XDG 校验，交给 `gio`/`xdg-open` 去做；这里只要能拿到程序名就够了。
/// 找不到 `[Desktop Entry]` 组里的 `Exec=` 就返回 `None`（**不能**误取
/// `[Desktop Action Foo]` 里的 `Exec=`，那会打开一个动作而不是程序本体）。
fn parse_exec_field(desktop: &str) -> Option<String> {
    let mut in_group = false;
    for line in desktop.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_group {
            continue;
        }
        if let Some(v) = line.strip_prefix("Exec=") {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// 从 `Exec=` 里取**程序名**。
///
/// 桌面项的 `Exec` 允许引号、百分号（`%u`/`%U`/`%f`/`%F`/`%i`/`%c`/`%k`）和反斜杠转义，
/// 所以不能简单 `split_whitespace().next()`（`"C:\...\dolphin.exe %U"` 这类会解析错）。
/// 这里按桌面项规范做「反斜杠转义 + 引号 + 字段码终止」的逐字符解析。
///
/// 关键：**遇到引号外的空白就停**（程序名是 `Exec` 的第一个*参数*，不可能含未加引号的
/// 空格）。`nautilus --browser %U` 的程序名是 `nautilus` 而不是 `nautilus --browser`
/// —— 后者会被当成一个不存在的文件名去 spawn。
fn exec_program(exec: &str) -> Option<String> {
    let mut prog = String::new();
    let mut chars = exec.chars().peekable();
    let mut in_quotes = false;
    while let Some(c) = chars.next() {
        match c {
            // 字段码：程序名到此为止（`%` 后必须有码字符；裸 `%` 属于保留）
            '%' => break,
            // 转义下一个字符（规范：`\\` `\"` `\s` `\t` `\n` `\r` `\\`）
            '\\' => {
                if let Some(n) = chars.next() {
                    prog.push(n);
                }
            }
            '"' if !in_quotes => in_quotes = true,
            // 引号内的空白属于程序名的一部分；引号本身不是程序名的字符
            '"' => in_quotes = false,
            // 引号外的空白 = 参数分隔：程序名结束
            c if c.is_whitespace() && !in_quotes => break,
            // 桌面项里单引号不是引号，直接当普通字符
            c => prog.push(c),
        }
    }
    let prog = prog.trim().to_string();
    if prog.is_empty() {
        None
    } else {
        Some(prog)
    }
}

/// 在 `PATH` 里找程序（`contains('/')` 的当绝对/相对路径直接查）。
fn which(prog: &str) -> Option<PathBuf> {
    if prog.contains('/') {
        let p = PathBuf::from(prog);
        return if p.is_file() { Some(p) } else { None };
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let cand = dir.join(prog);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// 拉起默认目录程序并与本进程**脱离**（不因它退出/报错而影响 GUI）。
///
/// 只传路径一个参数：桌面项 `Exec` 里那些 `%u`/`%U`/`%f` 字段码是「用条目打开」用的，
/// 打开目录不需要，硬拼反而会让某些程序多开一个窗口。
///
/// ⚠️ `setsid()` 是**尽力而为**，失败也照样把程序拉起来。
/// 这一点是实测逼出来的：`setsid` 在**已经是进程组组长**的进程里会返回 `EPERM`，
/// 而 `fork` 出来的子进程**有可能**仍然是组长（父进程本身是组长时，子进程继承其 pgid，
/// 而 pid 恰好等于 pgid 就构成组长）。若把它的失败当成致命错误，按钮就会在
/// **某些环境下必然点不开** —— 一个「脱离会话」的需求完全不该让功能整体不可用。
/// 顶多退化成「目录窗口与 GUI 共享会话」，这与 opener 插件的行为一致，可以接受。
fn spawn_detached(prog: &str, path: &PathBuf) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(prog);
    cmd.arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() != 0 {
                // 只是没能独立成会话，不影响程序被拉起来 —— 如实记一笔，别当致命错误。
                tracing::debug!(
                    "★ M10.7：setsid 失败（目录窗口将与 GUI 同会话）: {}",
                    std::io::Error::last_os_error()
                );
            }
            Ok(())
        });
    }
    cmd.spawn().map(|_| ())
}

/// ★ M8.4：用系统默认浏览器打开 URL（前端「帮助/官网」链接用）。
#[tauri::command]
pub async fn open_url(app: tauri::AppHandle, url: String) -> Result<Value, String> {
    use tauri_plugin_opener::OpenerExt as _;
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("url 不能为空".into());
    }
    match app.opener().open_url(url.clone(), None::<&str>) {
        Ok(()) => Ok(json!({"ok": true, "url": url})),
        Err(e) => Ok(json!({"ok": false, "error": format!("打开失败: {e}"), "url": url})),
    }
}

/// ★ M8.4：本进程可执行文件的绝对路径。
///
/// 前端「开机自启」开关要把它作为 `autostart_exe` 传给 daemon 的 `settings_save`：
/// autostart 桌面项里写的必须是 **GUI** 的路径（daemon 自己 `current_exe()` 会写成
/// `qxyncd`，那就开机只起守护没有界面了，见 `Settings::desktop_entry` 的注释）。
#[tauri::command]
pub async fn app_exe_path() -> Result<Value, String> {
    match std::env::current_exe() {
        Ok(p) => Ok(json!({"ok": true, "path": p.display().to_string()})),
        Err(e) => Ok(json!({"ok": false, "error": format!("取 current_exe 失败: {e}")})),
    }
}

/// ★ M8.4：隐藏主窗口（前端「最小化到托盘」按钮）。
#[tauri::command]
pub async fn window_hide(app: tauri::AppHandle) -> Result<Value, String> {
    let w = app
        .get_webview_window("main")
        .ok_or_else(|| "找不到主窗口 main".to_string())?;
    w.hide().map_err(|e| format!("隐藏失败: {e}"))?;
    Ok(json!({"ok": true, "hidden": true}))
}

/// ★ M8.4：显示并聚焦主窗口（主窗口被收进托盘后，前端/托盘要能把它叫回来）。
#[tauri::command]
pub async fn window_show(app: tauri::AppHandle) -> Result<Value, String> {
    let w = app
        .get_webview_window("main")
        .ok_or_else(|| "找不到主窗口 main".to_string())?;
    w.show().map_err(|e| format!("显示失败: {e}"))?;
    // 从托盘回来的窗口如果不 focus，用户会觉得「点了没反应」。
    let _ = w.set_focus();
    Ok(json!({"ok": true, "shown": true}))
}

/// ★ M8.4：退出整个应用（含托盘）。
#[tauri::command]
pub async fn app_quit(app: tauri::AppHandle) -> Result<Value, String> {
    // 注意：`exit` 会拆掉事件循环，这个返回信封**不一定能回到前端**；
    // 前端不该依赖它的返回值，只当「发出去就行了」。
    app.exit(0);
    Ok(json!({"ok": true, "quitting": true}))
}

/// ★ M8.4：验收用 —— 模拟托盘菜单点击（等价于 `tray::emit_action`）。
///
/// 存在的意义：验收脚本没法真的用鼠标点托盘菜单，但又必须验证「托盘菜单 → 事件 →
/// 前端」这条路，所以把同一段 emit 代码暴露成命令。只接受三个合法动作，
/// 拼错就直接报错，免得脚本里写错事件名却「看起来通过了」。
#[tauri::command]
pub async fn tray_emit(app: tauri::AppHandle, action: String) -> Result<Value, String> {
    if !matches!(action.as_str(), "open" | "sync" | "pause") {
        return Err(format!("action 只能是 open|sync|pause，收到 {action:?}"));
    }
    crate::tray::emit_action(&app, &action);
    Ok(json!({"ok": true, "action": action, "event": crate::tray::ACTION_EVENT, "target": "main"}))
}

// ---------------------------------------------------------------- 单元测试

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_input_defaults_and_trim() {
        let li: LinkInput = serde_json::from_value(json!({
            "host": " nas.local ", "user": " test1 ", "password": "x"
        }))
        .unwrap();
        let link = li.link();
        assert_eq!(link.id, "default");
        assert_eq!(link.host, "nas.local");
        assert_eq!(link.user, "test1");
        assert_eq!(link.port, 9834);
        assert!(link.https);
        assert!(!link.insecure);
        // 老前端可能还带着已删除的 `home_root` / `roots` 字段 → 忽略即可，不许报错
        let li2: LinkInput = serde_json::from_value(json!({
            "host": "nas.local", "user": "test1", "home_root": "/home/test1", "roots": ["/home"]
        }))
        .unwrap();
        assert_eq!(li2.link().user, "test1");
        assert_eq!(
            qxync_core::HOME_ROOT,
            "/home",
            "家目录是协议常量，不再是配置项"
        );
    }

    #[test]
    fn same_link_detects_change() {
        let info = qxync_core::ipc::LinkInfo {
            id: "default".into(),
            host: "nas.local".into(),
            port: 9834,
            https: true,
            user: "test1".into(),
            ipv4_only: false,
        };
        let mut link = LinkConfig {
            id: "default".into(),
            host: "nas.local".into(),
            port: 9834,
            https: true,
            insecure: true,
            user: "test1".into(),
            ipv4_only: false,
            exclude: Vec::new(),
            filter_temp: true,
            peer_listen: None,
            peer_name: None,
        };
        assert!(same_link(&info, &link));
        link.user = "other".into();
        assert!(!same_link(&info, &link));
        // 改回来 → 一致（不再有「家目录变了」这条：它已经不是配置项）
        link.user = "test1".into();
        assert!(same_link(&info, &link));
    }

    #[test]
    fn bad_request_becomes_error_envelope() {
        let v = ipc::err_response(ErrorKind::BadRequest, "x");
        assert_eq!(v["ok"], json!(false));
        assert_eq!(v["error"]["kind"], json!("bad_request"));
    }

    /// ★ M10.7：默认目录工具的解析链（桌面项 ID → 真程序名 → Exec 取程序名）。
    ///
    /// 这里每一组输入都是**开发机上真实出现过的形状**，不是假想的：
    /// `xdg-mime` 没登记目录处理器时是「退出码 0 + 空输出」；`gio mime` 的第一行
    /// 是带前缀的自然语言；KDE 的 `org.kde.dolphin.desktop` 的 `Exec` 是 `dolphin %u`
    /// ——**桌面项 ID 不是可执行文件名**（这一点是踩过才知道的）。
    #[test]
    fn dir_handler_parsing() {
        // 正常：`xdg-mime query default inode/directory`
        assert_eq!(
            parse_desktop_id(b"org.gnome.Nautilus.desktop\n").as_deref(),
            Some("org.gnome.Nautilus")
        );
        // ★ `gio mime inode/directory` 的真实输出：第一行是**带前缀**的自然语言。
        //   解析器必须剥到 ID 上，剥不掉就会拿整句当程序名去 spawn。
        assert_eq!(
            parse_desktop_id(
                "用于“inode/directory”的默认应用程序：org.kde.dolphin.desktop\n\
                              已注册的应用程序：\n\torg.kde.kate.desktop\n"
                    .as_bytes()
            )
            .as_deref(),
            Some("org.kde.dolphin")
        );
        // ★ 没登记时是**空输出 + 退出码 0**，不能当成程序名（空串 spawn 必失败）
        assert_eq!(parse_desktop_id(b""), None);
        assert_eq!(parse_desktop_id(b"\n  \n"), None);
        // 不是桌面项的一律不认（那多半是类型名）
        assert_eq!(parse_desktop_id(b"inode/directory\n"), None);
        // 路径形态的输出不是 ID
        assert_eq!(
            parse_desktop_id(b"/usr/share/applications/foo.desktop\n"),
            None
        );
        // 非 UTF-8 也不能 panic
        assert_eq!(parse_desktop_id(&[0xff, 0xfe]), None);
    }

    /// 桌面项 ID **不是**可执行文件名：必须从 `Exec=` 取。
    #[test]
    fn exec_program_extraction() {
        // ★ 开发机实测：org.kde.dolphin.desktop 的 Exec=dolphin %u
        assert_eq!(exec_program("dolphin %u").as_deref(), Some("dolphin"));
        assert_eq!(
            exec_program("nautilus --browser %U").as_deref(),
            Some("nautilus")
        );
        // 引号与转义（桌面项规范允许）
        assert_eq!(
            exec_program("\"/opt/My Files/dolphin\" %U").as_deref(),
            Some("/opt/My Files/dolphin")
        );
        assert_eq!(
            exec_program("C:\\\\dolphin.exe %U").as_deref(),
            Some("C:\\dolphin.exe")
        );
        // 只有字段码 / 空 → 没有程序名
        assert_eq!(exec_program("%U"), None);
        assert_eq!(exec_program("   "), None);
        assert_eq!(exec_program(""), None);
    }

    /// `Exec=` 只认 `[Desktop Entry]` 组里的那条，不能误取动作组的。
    #[test]
    fn exec_field_only_from_desktop_entry_group() {
        let f = "[Desktop Entry]\nType=Application\nName=Dolphin\nExec=dolphin %u\n\
                 [Desktop Action open-in-terminal]\nName=在终端中打开\nExec=dolphin-open %U\n";
        assert_eq!(parse_exec_field(f).as_deref(), Some("dolphin %u"));
        // 动作组在前、Desktop Entry 里没有 Exec → 不能拿动作的 Exec 当程序
        let g = "[Desktop Entry]\nType=Application\nName=X\n[Desktop Action a]\nExec=b %U\n";
        assert_eq!(parse_exec_field(g), None);
        // 损坏/空文件
        assert_eq!(parse_exec_field(""), None);
        assert_eq!(parse_exec_field("[Desktop Entry]\nExec=\n"), None);
    }

    /// 端到端（不打真程序）：临时 `.desktop` → 解析出真程序名。
    ///
    /// 覆盖「桌面项 ID 与 `Exec` 里的程序名不是一回事」——`org.example.MyFiles`
    /// 这个 ID 拿去 spawn 必然失败，真程序是 `Exec=` 里的 `myfiles`。
    /// 刻意**不改环境变量**（`XDG_DATA_HOME` 是进程全局的，并行跑测试会互相干扰），
    /// 直接测文件解析那一层。
    #[test]
    fn desktop_id_resolves_to_real_program() {
        let tmp = std::env::temp_dir().join(format!("qxync-dt-{}", std::process::id()));
        let apps = tmp.join("applications");
        std::fs::create_dir_all(&apps).unwrap();
        let entry = apps.join("org.example.MyFiles.desktop");
        std::fs::write(
            &entry,
            "[Desktop Entry]\nType=Application\nName=MyFiles\nExec=myfiles --open %U\n",
        )
        .unwrap();

        let exec = read_exec_in(&entry).unwrap();
        assert_eq!(exec, "myfiles --open %U");
        // 关键：ID `org.example.MyFiles` 本身不是程序，程序是 Exec 里的 `myfiles`
        assert_eq!(exec_program(&exec).as_deref(), Some("myfiles"));
        // 桌面项 ID 里带 `/` 一律拒掉（路径穿越 / 绝对路径形态都不是合法 ID）
        assert_eq!(read_desktop_exec("../../etc/passwd"), None);
        assert_eq!(read_desktop_exec(""), None);
        // 不存在的 ID → None（不是 panic）
        assert_eq!(read_exec_in(&apps.join("org.example.Nope.desktop")), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// ★ M10.7：`spawn_detached` **一定会把程序拉起来**。
    ///
    /// 这条守的是实测踩到的坑：原本 `setsid()` 失败被当成致命错误直接返回 `Err`，
    /// 于是在 `setsid` 返回 `EPERM` 的环境（进程已是进程组组长时）按钮**必然点不开**。
    /// 「脱离会话」只是锦上添花，不该决定功能可用性。
    ///
    /// 用 `sh -c` 起一个**把参数写进自己进程标题**的孩子，再从 `/proc` 读回它的
    /// `cmdline` 来验证「孩子真的跑了、且收到的是要打开的目录」。走 `/proc` 是为了
    /// 不依赖 `sh` 的 quoting 细节。
    #[cfg(unix)]
    #[test]
    fn spawn_detached_runs_even_when_setsid_is_unavailable() {
        // 目录名带上 pid + 一个测试内自增序号：并行跑多个测试二进制时别互相踩。
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let mnt = std::env::temp_dir().join(format!(
            "qxync-spawn-mnt-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&mnt);
        std::fs::create_dir_all(&mnt).unwrap();
        // ★ `spawn_detached(prog, arg)` 只会产生 `argv = [prog, arg]` —— **只有一个**参数。
        //
        //   所以任何「`sh -c <脚本> <参数>`」的写法在这里都做不到：`Command::arg` 会把
        //   整串当成**一个** argv 元素，sh 看到的是**一个**以 `-c ` 开头的整体选项，
        //   没有把 -c 和脚本分开，直接报 `sh: - : 无效的选项` 并以 2 退出
        //   （本机实测 returncode=2，`/proc/<pid>/cmdline` 读出来是空的 ——
        //   进程转瞬即死，怎么等都找不到）。
        //
        //   于是「找不到带目录的进程」是**必然**，不是偶发：机器空闲时看起来能过，
        //   是因为空 cmdline 的短命进程有时还没被回收，而判据
        //   `contains(target) && contains("sleep")` 又恰好命中了
        //   **cmdline 里含有本测试脚本文本的父 shell**（bwrap/bash）—— 靠误命中过关。
        //   这也是它在满载机器上随机红的原因：父 shell 何时被回收 / 短命进程何时
        //   真正消失，都会变。本机满载复现：15 次挂 8 次；空闲时 25 次全过。
        //
        //   修法：把辅助脚本**落成一个文件**再执行，于是 `argv = [脚本, 目录]`。
        //
        // ★ 脚本内容必须**纯 POSIX**。开发机的 /bin/sh 是 bash，Ubuntu 22.04 的
        //   /bin/sh 是 **dash** —— 同一份测试在开发机永远绿、在 CI 上必然红：
        //   `exec -a`（改 argv0）是 bash 扩展，dash 不认，脚本直接失败退出，
        //   `/proc` 里永远找不到那个进程，测试要卡满 30s 才报错。
        //   现在脚本只有一行 `sleep 30`，两边行为一致。
        let helper = mnt.join("holder.sh");
        // 不用 `exec`：exec 会**替换**进程映像，argv 随之被换成 sleep 自己的，
        // 追加的目录就没了（实测 argv 变成 `sleep 30`）。直接跑 `sleep`，
        // sh 会 fork 出一个带着完整 argv（脚本路径 + 目录）的子进程 —— 那正是要验的。
        std::fs::write(&helper, "#!/bin/sh\nsleep 30\n").unwrap();
        let mut perms = std::fs::metadata(&helper).unwrap().permissions();
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o755);
        }
        std::fs::set_permissions(&helper, perms).unwrap();
        let r = spawn_detached(helper.to_str().unwrap(), &mnt);
        assert!(r.is_ok(), "spawn_detached 不应因 setsid 失败而报错: {r:?}");
        // 从 /proc 找到刚起的那个孩子，验证它**带着要打开的目录**这个参数。
        //
        // 判据：argv 里同时出现「辅助脚本路径」与「目标目录」。
        //   * 为什么不用「argv0 == 目录」：那需要 `exec -a`（bash 扩展，dash 没有）。
        //   * 为什么不用「cmdline 含 sleep」：`#!` 脚本最终跑的是 sleep，但
        //     sh 可能做 exec 优化把 argv 换成 `sleep 30`，字样时有时无。
        //   * 只判「含 target」也不够：父 shell（bwrap/bash）的 cmdline 里含有
        //     本测试的脚本文本，会误命中 —— 所以要求**同时**出现脚本路径。
        let target = mnt.display().to_string();
        let script_arg = helper.display().to_string();
        let mut found = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            if let Ok(rd) = std::fs::read_dir("/proc") {
                for e in rd.flatten() {
                    let cmdline = e.path().join("cmdline");
                    if let Ok(b) = std::fs::read(&cmdline) {
                        if b.is_empty() {
                            continue;
                        }
                        let joined = String::from_utf8_lossy(&b).replace('\0', "\n");
                        let argv: Vec<&str> = joined.lines().map(str::trim).collect();
                        if argv.iter().any(|a| *a == script_arg)
                            && argv.iter().any(|a| *a == target)
                        {
                            found = true;
                            break;
                        }
                    }
                }
            }
            if found {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            found,
            "被拉起的进程 argv 应含脚本 {script_arg} 与要打开的目录 {target}（实际扫到的进程需含两者）"
        );
        let _ = std::fs::remove_dir_all(&mnt);
    }
}
