//! Tauri 命令面（前端的全部入口）。
//!
//! 约定：
//! * 所有**业务动作**都封装成 `{v,ok,data,error}` 信封（与 `qxync-core::ipc::Response` 同形），
//!   前端只需要一套渲染逻辑；`Err(String)` 只用于「参数不合法」这类本地错误。
//! * 口令只在 `credential_save` / `login_flow` 的入参里出现，**从不回传**。

use crate::ipc;
use qxync_core::ipc::{ErrorKind, PingData, Request, RequestEnvelope, StatusData, IPC_VERSION};
use qxync_core::{ConfigPaths, Credentials, LinkConfig, Settings, HOME_ROOT};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
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
        // ★ M6：roots 变了也必须重启 daemon（daemon 只在启动时读 link 文件）
        && a.roots == b.roots()
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
    #[serde(default = "default_home_root")]
    home_root: String,
    /// ★ M6：多根 / 共享文件夹（空 = 只用 home_root）
    #[serde(default)]
    roots: Vec<String>,
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
fn default_home_root() -> String {
    HOME_ROOT.to_string()
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
            home_root: if self.home_root.trim().is_empty() {
                HOME_ROOT.to_string()
            } else {
                self.home_root.trim().to_string()
            },
            // 空行 = 没配；有内容才归一化（归一化会把空输入变成 /home，那是「生效根」的语义）
            roots: if self.roots.iter().all(|r| r.trim().is_empty()) {
                Vec::new()
            } else {
                qxync_core::normalize_roots(&self.roots)
            },
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
#[tauri::command]
pub async fn daemon_start(link_id: Option<String>) -> Result<Value, String> {
    let id = link_id
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".into());
    // 没配置就直说，省得 daemon 起来又立刻退出
    let p = paths()?;
    if LinkConfig::load(&p, &id).is_err() {
        return Ok(json!({
            "started": false, "already": false,
            "error": format!("没有连接配置 {}（先保存连接配置）", p.link_file(&id).display()),
        }));
    }
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
            .map(|s| same_link(&s.link, &link))
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
#[tauri::command]
pub async fn open_path(app: tauri::AppHandle, path: String) -> Result<Value, String> {
    use tauri_plugin_opener::OpenerExt as _;
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Ok(json!({"ok": false, "error": "路径不存在", "path": path}));
    }
    match app.opener().open_path(path.clone(), None::<&str>) {
        Ok(()) => Ok(json!({"ok": true, "path": path})),
        Err(e) => Ok(json!({"ok": false, "error": format!("打开失败: {e}"), "path": path})),
    }
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
        assert_eq!(link.home_root, HOME_ROOT);
        assert!(link.roots.is_empty(), "没传 roots 就是空 = 只用 home_root");
    }

    #[test]
    fn link_input_roots_are_normalized() {
        // ★ M6：roots 传了就归一化（补前导 /、去尾斜杠、去重）
        let li: LinkInput = serde_json::from_value(json!({
            "host": "nas.local", "user": "test1",
            "roots": [" /home/ ", "Public", "/home", ""]
        }))
        .unwrap();
        assert_eq!(
            li.link().roots,
            vec!["/home".to_string(), "/Public".to_string()]
        );
        // 没传（或全是空白）= 空 = 只用 home_root，行为与 M5 之前一致
        let li2: LinkInput = serde_json::from_value(
            json!({"host": "nas.local", "user": "test1", "roots": ["  ", ""]}),
        )
        .unwrap();
        assert!(li2.link().roots.is_empty());
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
            roots: vec!["/home".into()],
        };
        let mut link = LinkConfig {
            id: "default".into(),
            host: "nas.local".into(),
            port: 9834,
            https: true,
            insecure: true,
            user: "test1".into(),
            home_root: "/home".into(),
            roots: Vec::new(),
            ipv4_only: false,
            exclude: Vec::new(),
            filter_temp: true,
            peer_listen: None,
            peer_name: None,
        };
        assert!(same_link(&info, &link));
        link.user = "other".into();
        assert!(!same_link(&info, &link));
        // ★ M6：只改 roots 也要能识别出来（否则「保存并登录」不会重启 daemon，roots 不生效）
        link.user = "test1".into();
        link.roots = vec!["/home".into(), "/Public".into()];
        assert!(!same_link(&info, &link), "roots 变了必须被判为不同");
    }

    #[test]
    fn bad_request_becomes_error_envelope() {
        let v = ipc::err_response(ErrorKind::BadRequest, "x");
        assert_eq!(v["ok"], json!(false));
        assert_eq!(v["error"]["kind"], json!("bad_request"));
    }
}
