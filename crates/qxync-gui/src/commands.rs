//! Tauri 命令面（前端的全部入口）。
//!
//! 约定：
//! * 所有**业务动作**都封装成 `{v,ok,data,error}` 信封（与 `qxync-core::ipc::Response` 同形），
//!   前端只需要一套渲染逻辑；`Err(String)` 只用于「参数不合法」这类本地错误。
//! * 口令只在 `credential_save` / `login_flow` 的入参里出现，**从不回传**。

use crate::ipc;
use qxync_core::ipc::{ErrorKind, PingData, Request, RequestEnvelope, StatusData, IPC_VERSION};
use qxync_core::{ConfigPaths, Credentials, LinkConfig, HOME_ROOT};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

// ---------------------------------------------------------------- 工具

fn paths() -> Result<ConfigPaths, String> {
    ConfigPaths::discover().map_err(|e| e.to_string())
}

fn daemon_binary() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("QSYNC_DAEMON_BIN") {
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
    Err("找不到 qxyncd（先 `cargo build --workspace`，或设置 QSYNC_DAEMON_BIN）".into())
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
    #[serde(default = "default_home_root")]
    home_root: String,
    #[serde(default)]
    ipv4_only: bool,
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
            id: self.id.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "default".into()),
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
            ipv4_only: self.ipv4_only,
        }
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
        // 调试/验收用：`QSYNC_GUI_TAB=<status|connect|mounts|files|sync>` 指定初始 tab
        "initial_tab": std::env::var("QSYNC_GUI_TAB").ok(),
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
    let id = link_id.filter(|s| !s.is_empty()).unwrap_or_else(|| "default".into());
    let path = p.link_file(&id);
    match LinkConfig::load(&p, &id) {
        Ok(link) => Ok(json!({"exists": true, "id": id, "path": path.display().to_string(), "link": link})),
        Err(_) => Ok(json!({"exists": false, "id": id, "path": path.display().to_string(), "link": Value::Null})),
    }
}

/// 写连接配置 `~/.config/qsync/links/<id>.json`。
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

/// 写凭据 `~/.config/qsync/credentials.json`（0600，先临时文件再 rename）。
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
        Ok(c) => Ok(json!({"present": true, "host": c.host, "user": c.user, "path": p.credentials_file().display().to_string()})),
        Err(_) => Ok(json!({"present": false, "path": p.credentials_file().display().to_string()})),
    }
}

/// 拉起 daemon（已在跑就返回 already）。
#[tauri::command]
pub async fn daemon_start(link_id: Option<String>) -> Result<Value, String> {
    let id = link_id.filter(|s| !s.is_empty()).unwrap_or_else(|| "default".into());
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
    Ok(json!({"stopped": gone, "already": false, "socket": socket.display().to_string(), "response": resp}))
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
    let link = li.link();
    let p = paths()?;
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
        let current = ipc::call_typed::<StatusData>(&socket, Request::Status).await.ok();
        let same = current.as_ref().map(|s| same_link(&s.link, &link)).unwrap_or(false);
        if !same {
            // 换 NAS / 换账号：daemon 需要重启才会读新的 link
            let _ = ipc::call(&socket, Request::Shutdown).await;
            if !wait_gone(&socket).await {
                return Ok(json!({
                    "ok": false, "link_path": link_path.display().to_string(),
                    "credential_path": cred_path.display().to_string(),
                    "error": "参数已改，但旧 daemon 没能在 10s 内退出；请手动 `qsync daemon stop` 后重试",
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
        if !started["started"].as_bool().unwrap_or(false) && !started["already"].as_bool().unwrap_or(false) {
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
            home_root: "/home".into(),
            ipv4_only: false,
        };
        assert!(same_link(&info, &link));
        link.user = "other".into();
        assert!(!same_link(&info, &link));
    }

    #[test]
    fn bad_request_becomes_error_envelope() {
        let v = ipc::err_response(ErrorKind::BadRequest, "x");
        assert_eq!(v["ok"], json!(false));
        assert_eq!(v["error"]["kind"], json!("bad_request"));
    }
}
