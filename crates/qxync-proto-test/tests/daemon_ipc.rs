//! qxyncd 的 IPC 端到端测试：**真的起进程、真的走 unix socket**。
//!
//! ```bash
//! export QXNYC_TEST_HOST=... QXNYC_TEST_USER=... QXNYC_TEST_PASSWORD='...'
//! cargo test -p qxync-proto-test --test daemon_ipc -- --ignored --nocapture
//! ```
//! 没设 `QXNYC_TEST_HOST` 时直接返回（视为跳过）。ping/status/shutdown 不需要 NAS。

use qxync_core::ipc::{
    decode_line, encode_line, DaemonInfo, ErrorKind, LsData, PingData, Request, RequestEnvelope,
    Response, SettingsData, StatusData,
};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// 断言失败时也要把 daemon 收掉，免得留下一个永远空转的进程。
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 定位 `qxyncd`：优先环境变量，其次 workspace 根的 `target/debug/`。
fn daemon_bin() -> PathBuf {
    if let Ok(p) = std::env::var("QXNYC_TEST_DAEMON_BIN") {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/qxyncd")
}

async fn call(socket: &Path, req: Request) -> Response {
    let stream = UnixStream::connect(socket).await.expect("连接 daemon");
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    let line = String::from_utf8(encode_line(&RequestEnvelope::new(req)).unwrap()).unwrap();
    wr.write_all(line.as_bytes()).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
        .await
        .expect("响应超时")
        .unwrap()
        .expect("连接被关闭");
    decode_line(reply.as_bytes()).expect("响应解码")
}

async fn wait_socket(socket: &Path, secs: u64) -> bool {
    for _ in 0..(secs * 10) {
        if UnixStream::connect(socket).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test]
#[ignore = "会拉起 qxyncd 进程（需要 NAS 才能测 login/ls）"]
async fn daemon_ipc_round_trip() {
    let Some(host) = std::env::var("QXNYC_TEST_HOST").ok() else {
        eprintln!("跳过：未设置 QXNYC_TEST_HOST");
        return;
    };
    let user = std::env::var("QXNYC_TEST_USER").unwrap_or_else(|_| "test1".into());
    let port = std::env::var("QXNYC_TEST_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(9834);
    let password = std::env::var("QXNYC_TEST_PASSWORD").ok();
    let fixture = std::env::var("QXNYC_TEST_FIXTURE").unwrap_or_else(|_| "/home/qxync-test".into());

    let bin = daemon_bin();
    if !bin.exists() {
        eprintln!(
            "跳过：{} 不存在（先 cargo build --workspace）",
            bin.display()
        );
        return;
    }

    // 独立的 XDG 目录，避免污染真实配置
    let root = std::env::temp_dir().join(format!("qxync-ipc-test-{}", std::process::id()));
    let (cfg, run, state) = (root.join("config"), root.join("run"), root.join("state"));
    std::fs::create_dir_all(cfg.join("qxync/links")).unwrap();
    std::fs::write(
        cfg.join("qxync/links/default.json"),
        serde_json::json!({
            "id":"default","host":host,"port":port,"https":true,"insecure":true,
            "user":user,"home_root":"/home","ipv4_only": true
        })
        .to_string(),
    )
    .unwrap();
    if let Some(pw) = &password {
        std::fs::write(
            cfg.join("qxync/credentials.json"),
            serde_json::json!({"host":host,"user":user,"password":pw}).to_string(),
        )
        .unwrap();
    }
    std::fs::create_dir_all(&run).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let socket = run.join("qxync/qxyncd.sock");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();

    let mut child = std::process::Command::new(&bin)
        .args(["--foreground", "--link", "default", "--socket"])
        .arg(&socket)
        .env("XDG_CONFIG_HOME", &cfg)
        .env("XDG_RUNTIME_DIR", &run)
        .env("XDG_STATE_HOME", &state)
        .env("RUST_LOG", "info")
        .spawn()
        .expect("启动 qxyncd");

    assert!(wait_socket(&socket, 10).await, "socket 未在 10s 内就绪");

    // 1) ping
    let ping: PingData = call(&socket, Request::Ping).await.into_result().unwrap();
    assert!(ping.pong);
    assert_eq!(ping.pid, child.id());
    println!("ping ok: v{} pid={}", ping.daemon_version, ping.pid);

    // 2) status：未登录（daemon 不强制登录）
    let st: StatusData = call(&socket, Request::Status).await.into_result().unwrap();
    assert!(!st.logged_in, "status 不应触发登录");
    // 这个用例先写好 link 配置再起 daemon，所以必须报出连接（`None` 只出现在空转待命）
    assert_eq!(st.link.as_ref().map(|l| l.user.clone()), Some(user));
    assert!(st.mounts.is_empty());
    let DaemonInfo { socket: s, .. } = st.daemon.clone();
    assert!(s.ends_with("qxyncd.sock"), "{s}");
    println!("status ok: socket={s}");

    // 3) login + ls + 水合统计（需要凭据）
    if password.is_some() {
        let _: serde_json::Value = call(
            &socket,
            Request::Login {
                user: None,
                password: None,
            },
        )
        .await
        .into_result()
        .expect("login");

        let st: StatusData = call(&socket, Request::Status).await.into_result().unwrap();
        assert!(st.logged_in, "登录后 status 应为已登录");
        assert!(st.session.is_some());

        let ls: LsData = call(
            &socket,
            Request::Ls {
                path: fixture.clone(),
            },
        )
        .await
        .into_result()
        .expect("ls");
        assert!(ls.total > 0, "{} 应有内容", fixture);
        assert!(
            ls.entries.iter().any(|e| e.filename == "hello.txt"),
            "应有 hello.txt：{:?}",
            ls.entries.iter().map(|e| &e.filename).collect::<Vec<_>>()
        );
        println!("login+ls ok: {} 项", ls.total);
    } else {
        println!("跳过 login/ls（未设置 QXNYC_TEST_PASSWORD）");
    }

    // 4) shutdown → socket 消失、pid 文件消失
    let _: serde_json::Value = call(&socket, Request::Shutdown)
        .await
        .into_result()
        .unwrap();
    for _ in 0..50 {
        if !socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!socket.exists(), "shutdown 后 socket 应被删除");
    let pid_file = state.join("qxync/qxyncd.pid");
    assert!(!pid_file.exists(), "shutdown 后 pid 文件应被删除");
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&root);
    println!("shutdown ok");
}

/// ★ 回归测试：**一份连接配置都没有时，daemon 也必须能起来、能常驻、能服务**。
///
/// 这正是用户报的那个问题：daemon 曾经在 `LinkConfig::load` 失败时直接退出，于是
/// 「没配 NAS」=「daemon 起不来」，和「daemon 一直跑在后台」的计划直接冲突。
///
/// 不需要 NAS、不需要凭据 —— 只要 `target/debug/qxyncd` 在（`cargo build --workspace` 后即有）。
#[tokio::test]
async fn idle_daemon_serves_without_any_link_config() {
    let bin = daemon_bin();
    if !bin.exists() {
        eprintln!(
            "跳过：{} 不存在（先 cargo build --workspace）",
            bin.display()
        );
        return;
    }

    let root = std::env::temp_dir().join(format!("qxync-idle-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (cfg, run, state) = (root.join("config"), root.join("run"), root.join("state"));
    // 关键：**故意不建** `qxync/links/default.json`
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let socket = run.join("qxync/qxyncd.sock");

    let child = std::process::Command::new(&bin)
        .args(["--foreground", "--link", "default", "--socket"])
        .arg(&socket)
        .env("XDG_CONFIG_HOME", &cfg)
        .env("XDG_RUNTIME_DIR", &run)
        .env("XDG_STATE_HOME", &state)
        .env("RUST_LOG", "info")
        .spawn()
        .expect("启动 qxyncd");
    let mut child = KillOnDrop(child);

    assert!(
        wait_socket(&socket, 10).await,
        "空转待命的 daemon 也必须在 10s 内就绪"
    );

    // 1) 活着
    let ping: PingData = call(&socket, Request::Ping).await.into_result().unwrap();
    assert!(ping.pong);

    // 2) status 如实报「在跑但未配置」：`link: None`（前端据此显示「未配置」）
    let st: StatusData = call(&socket, Request::Status).await.into_result().unwrap();
    assert!(
        st.link.is_none(),
        "没有 link 配置必须报 None，而不是空 host"
    );
    assert!(!st.logged_in);
    assert!(st.mounts.is_empty());

    // 3) 要 NAS 的请求被**明确拒绝**，而不是拿着空 host 去发请求
    let err = call(
        &socket,
        Request::Ls {
            path: "/home".into(),
        },
    )
    .await
    .into_result::<LsData>()
    .expect_err("没有连接配置时 ls 必须失败");
    assert_eq!(err.kind, ErrorKind::NotLoggedIn);
    assert!(
        err.message.contains("还没有配置 NAS 连接"),
        "拒绝理由要能看懂：{}",
        err.message
    );

    // 4) 纯本地文件的设置读写照常可用（GUI 的设置页 / 登录页就在那一页）
    let d: SettingsData = call(&socket, Request::Settings)
        .await
        .into_result()
        .unwrap();
    assert!(d.path.ends_with("settings.json"), "{}", d.path);
    let mut s = d.settings.clone();
    s.desktop_notifications = false;
    let d2: SettingsData = call(
        &socket,
        Request::SettingsSave {
            settings: s,
            autostart_exe: None,
        },
    )
    .await
    .into_result()
    .unwrap();
    assert!(d2.saved, "空转时也要能写设置");
    assert!(!d2.settings.desktop_notifications);
    assert!(
        cfg.join("qxync/settings.json").exists(),
        "设置要真的落盘到 {}",
        cfg.display()
    );

    // 5) shutdown：干净退出（socket / pid 都收掉）
    let _: serde_json::Value = call(&socket, Request::Shutdown)
        .await
        .into_result()
        .unwrap();
    for _ in 0..50 {
        if !socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!socket.exists(), "shutdown 后 socket 应被删除");
    assert!(
        !state.join("qxync/qxyncd.pid").exists(),
        "shutdown 后 pid 文件应被删除"
    );
    let _ = child.0.wait();
    let _ = std::fs::remove_dir_all(&root);
    println!("空转待命 ok：无 link 也能 ping/status/设置读写/退出");
}
