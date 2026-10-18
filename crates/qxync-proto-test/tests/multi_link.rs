//! ★ M16：两条连接 = 两个独立 daemon 的端到端隔离测试。
//!
//! 不需要真机 NAS：两条 link 都指向 `127.0.0.1:1`（连接必然失败），但 daemon
//! 仍会正常启动并服务 IPC —— 本测试验证的正是「多连接」的进程/端口/任务边界，
//! 不是 NAS 同步本身。
//!
//! ```bash
//! cargo build --workspace --bins
//! cargo test -p qxync-proto-test --test multi_link -- --nocapture
//! ```

use qxync_core::ipc::{
    decode_line, encode_line, PingData, Request, RequestEnvelope, Response, StatusData, TasksData,
};
use qxync_core::tasks::Task;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

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
async fn two_links_run_as_two_isolated_daemons() {
    let bin = daemon_bin();
    if !bin.exists() {
        eprintln!(
            "跳过：{} 不存在（先 cargo build --workspace --bins）",
            bin.display()
        );
        return;
    }

    let root = std::env::temp_dir().join(format!("qxync-multi-link-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (cfg, run, state, data) = (
        root.join("config"),
        root.join("run"),
        root.join("state"),
        root.join("data"),
    );
    std::fs::create_dir_all(cfg.join("qxync/links")).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&data).unwrap();

    for (id, host) in [("default", "127.0.0.1"), ("nas2", "127.0.0.1")] {
        std::fs::write(
            cfg.join(format!("qxync/links/{id}.json")),
            serde_json::json!({
                "id": id, "host": host, "port": 1, "https": false, "insecure": true,
                "user": "u", "ipv4_only": true,
            })
            .to_string(),
        )
        .unwrap();
    }

    let sock_default = run.join("qxync/default.sock");
    let sock_nas2 = run.join("qxync/nas2.sock");
    std::fs::create_dir_all(sock_default.parent().unwrap()).unwrap();

    let spawn = |link: &str, socket: &Path| {
        std::process::Command::new(&bin)
            .args(["--foreground", "--link", link, "--socket"])
            .arg(socket)
            .env("XDG_CONFIG_HOME", &cfg)
            .env("XDG_DATA_HOME", &data)
            .env("XDG_RUNTIME_DIR", &run)
            .env("XDG_STATE_HOME", &state)
            .env("QXNYC_TASK_RESTORE", "0")
            .env("RUST_LOG", "warn")
            .spawn()
            .expect("启动 qxyncd")
    };
    let _a = KillOnDrop(spawn("default", &sock_default));
    let _b = KillOnDrop(spawn("nas2", &sock_nas2));

    assert!(
        wait_socket(&sock_default, 10).await,
        "default socket 未就绪"
    );
    assert!(wait_socket(&sock_nas2, 10).await, "nas2 socket 未就绪");
    assert_ne!(sock_default, sock_nas2);

    // 各自报自己的 link
    let sa: StatusData = call(&sock_default, Request::Status)
        .await
        .into_result()
        .unwrap();
    let sb: StatusData = call(&sock_nas2, Request::Status)
        .await
        .into_result()
        .unwrap();
    assert_eq!(sa.link.as_ref().map(|l| l.id.as_str()), Some("default"));
    assert_eq!(sb.link.as_ref().map(|l| l.id.as_str()), Some("nas2"));
    let pa: PingData = call(&sock_default, Request::Ping)
        .await
        .into_result()
        .unwrap();
    let pb: PingData = call(&sock_nas2, Request::Ping).await.into_result().unwrap();
    assert_ne!(pa.pid, pb.pid, "必须是两个进程");

    // 在 default 上登记任务
    let mp = root.join("mnt-default");
    std::fs::create_dir_all(&mp).unwrap();
    let mut task = Task::from_mount(
        Some("t1".into()),
        mp.clone(),
        Some("/home".into()),
        false,
        None,
        None,
        None,
        None,
        None,
    );
    task.normalize().unwrap();
    let saved: Response = call(
        &sock_default,
        Request::Tasks {
            action: "save".into(),
            id: None,
            task: Some(task.clone()),
        },
    )
    .await;
    assert!(saved.ok, "default 保存任务失败：{:?}", saved.error);

    // nas2 看不到 default 的任务
    let l2: TasksData = call(
        &sock_nas2,
        Request::Tasks {
            action: "list".into(),
            id: None,
            task: None,
        },
    )
    .await
    .into_result()
    .unwrap();
    assert!(
        l2.tasks.is_empty(),
        "nas2 不该看到 default 的任务：{:?}",
        l2.tasks.iter().map(|t| &t.task.id).collect::<Vec<_>>()
    );

    // 同 id 从 nas2 覆盖必须被拒
    let overwrite = call(
        &sock_nas2,
        Request::Tasks {
            action: "save".into(),
            id: None,
            task: Some(task),
        },
    )
    .await;
    assert!(
        !overwrite.ok,
        "同 id 任务跨连接覆盖必须被拒绝，实际：{overwrite:?}"
    );

    // 停 default，nas2 仍活着
    let _: serde_json::Value = call(&sock_default, Request::Shutdown)
        .await
        .into_result()
        .unwrap();
    for _ in 0..50 {
        if !sock_default.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!sock_default.exists(), "default shutdown 后 socket 应删除");
    let pb2: PingData = call(&sock_nas2, Request::Ping).await.into_result().unwrap();
    assert_eq!(pb2.pid, pb.pid, "default 退出不该影响 nas2");

    let _: serde_json::Value = call(&sock_nas2, Request::Shutdown)
        .await
        .into_result()
        .unwrap();
    for _ in 0..50 {
        if !sock_nas2.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!sock_nas2.exists(), "nas2 shutdown 后 socket 应删除");
    // pid 文件按连接分开，且都删掉了
    assert!(!state.join("qxync/qxyncd.pid").exists());
    assert!(!state.join("qxync/qxyncd-nas2.pid").exists());

    let _ = std::fs::remove_dir_all(&root);
}
