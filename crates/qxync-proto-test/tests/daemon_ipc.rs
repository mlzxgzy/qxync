//! qxyncd 的 IPC 端到端测试：**真的起进程、真的走 unix socket**。
//!
//! ```bash
//! export QXNYC_TEST_HOST=... QXNYC_TEST_USER=... QXNYC_TEST_PASSWORD='...'
//! cargo test -p qxync-proto-test --test daemon_ipc -- --ignored --nocapture
//! ```
//! 没设 `QXNYC_TEST_HOST` 时直接返回（视为跳过）。ping/status/shutdown 不需要 NAS。

use qxync_core::ipc::{
    decode_line, encode_line, DaemonInfo, LsData, PingData, Request, RequestEnvelope, Response,
    StatusData,
};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

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
    assert_eq!(st.link.user, user);
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
