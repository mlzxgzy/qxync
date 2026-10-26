//! ★ M37 回归：守护进程**绝不能**被 SIGPIPE 杀死。
//!
//! 0.7.10 及以前 `main` 照搬了 CLI 的做法 `libc::signal(SIGPIPE, SIG_DFL)`，于是
//! 「写到一个对端已关闭的 socket/pipe」= 内核**立刻杀掉整个 daemon**：
//!
//! * 真机形状（2026-10-26）：笔记本盒盖 s2idle 约 2 分钟后唤醒，NAS 的 HTTPS
//!   长连接 / 会话保活连接全部失效；唤醒后第一轮变更轮询去写那条旧连接 →
//!   `code=killed, signal=PIPE`。没有清理机会（FUSE 挂载点、上传队列停在半路），
//!   tracing 的 non_blocking 缓冲来不及落盘（日志断在死前几分钟），systemd 还把
//!   这一次记成 `Result=success`（`NRestarts=0`）→ 不自动重启 → 用户必须手动 start。
//!
//! 这里不需要真的去写一个坏 socket：进程级**信号处置**的差异，一条 `kill(SIGPIPE)`
//! 就够钉住了 —— `SIG_DFL` 会立刻死，`SIG_IGN`（修复后）什么都不会发生。
//! 「唤醒后写坏连接 → EPIPE 被当成错误处理 → 重连」那条链路由
//! `docs/M37-休眠唤醒后守护被SIGPIPE杀死.md` 的人工验收矩阵覆盖（要真挂 FUSE + 真 NAS）。
//!
//! 顺便把「`SIGPIPE` 之后 IPC 仍然可用」也钉住：进程活着但 listener 卡死同样是故障。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn unique_dir(tag: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("qxync-daemon-{tag}-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

/// 测试进程的子进程兜底清理（断言失败也别把 daemon 留在机器上）。
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGKILL) };
        let _ = self.0.wait();
    }
}

fn wait_until<F: FnMut() -> bool>(timeout: Duration, mut f: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    f()
}

/// 问 daemon 一句 `ping`：证明它不光是「进程还在」，IPC 线程也活着。
fn ping_ok(socket: &Path) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let Ok(req) = qxync_core::ipc::encode_line(&qxync_core::ipc::RequestEnvelope::new(
        qxync_core::ipc::Request::Ping,
    )) else {
        return false;
    };
    if stream.write_all(&req).is_err() {
        return false;
    }
    let mut line = String::new();
    if BufReader::new(stream).read_line(&mut line).is_err() {
        return false;
    }
    serde_json::from_str::<qxync_core::ipc::Response>(&line)
        .map(|r| r.ok)
        .unwrap_or(false)
}

#[test]
fn sigpipe_must_not_kill_the_daemon() {
    let root = unique_dir("sigpipe");
    let socket = root.join("run").join("qxyncd.sock");
    std::fs::create_dir_all(socket.parent().expect("socket 父目录")).expect("建 socket 目录");

    let child = Command::new(env!("CARGO_BIN_EXE_qxyncd"))
        .args(["--foreground", "--link", "default"])
        .arg("--socket")
        .arg(&socket)
        // XDG 三个目录全部重定向：不碰用户真实配置/状态/缓存。
        // 没有 link 配置 → daemon 进「空转待命」，不挂 FUSE、不连 NAS。
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("RUST_LOG", "error")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动 qxyncd");

    let mut guard = Guard(child);
    let pid = guard.0.id() as libc::pid_t;

    // 1) 等「空转待命」起来（socket 可连）
    let ready = wait_until(Duration::from_secs(20), || {
        if let Some(status) = guard.0.try_wait().expect("wait 子进程") {
            panic!("qxyncd 过早退出（{status}）");
        }
        ping_ok(&socket)
    });
    assert!(
        ready,
        "qxyncd 20s 内没有就绪（socket {}）",
        socket.display()
    );

    // 2) ★ 关键断言：SIGPIPE 不能让进程死。
    //    修复前（SIG_DFL）这里直接退出，退出原因是 signal 13。
    assert_eq!(
        unsafe { libc::kill(pid, libc::SIGPIPE) },
        0,
        "给 qxyncd 发 SIGPIPE 失败"
    );
    std::thread::sleep(Duration::from_millis(500));
    if let Some(status) = guard.0.try_wait().expect("wait 子进程") {
        panic!(
            "收到 SIGPIPE 后 qxyncd 退出了（{status}）—— \
             说明 SIGPIPE 又被设回了 SIG_DFL：唤醒后写一条已失效的 NAS 连接就会这样死掉"
        );
    }

    // 3) 不只是「活着」：SIGPIPE 之后 IPC 必须照常应答
    assert!(
        ping_ok(&socket),
        "SIGPIPE 之后 IPC 不再应答（listener 卡死？）"
    );

    // 4) 干净收尾：SIGTERM → 空转待命模式应自己退出（超时就交给 Guard 强杀，不算失败）
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let _ = wait_until(Duration::from_secs(10), || {
        guard.0.try_wait().map(|s| s.is_some()).unwrap_or(true)
    });
}

/// ★ M37：`bind_socket` 发现「另一个 daemon 已经占着 socket」时必须以**退出码 3** 结束。
///
/// 这个码是 `qxyncd.service` 里 `RestartPreventExitStatus=3` 的另一半：少了它，
/// `Restart=always` 会把「已经有人在同步」当成启动失败，每 5 秒重试一次、永远起不来
/// （2026-10-26 20:08 现场抓到过每 3 秒一轮的循环）。
#[test]
fn already_running_exits_with_code_three() {
    let root = unique_dir("already-running");
    let socket = root.join("run").join("qxyncd.sock");
    std::fs::create_dir_all(socket.parent().expect("socket 父目录")).expect("建 socket 目录");

    let spawn = || {
        Command::new(env!("CARGO_BIN_EXE_qxyncd"))
            .args(["--foreground", "--link", "default"])
            .arg("--socket")
            .arg(&socket)
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("RUST_LOG", "error")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("启动 qxyncd")
    };

    let mut first = Guard(spawn());
    let ready = wait_until(Duration::from_secs(20), || {
        if let Some(status) = first.0.try_wait().expect("wait 子进程") {
            panic!("第一个 qxyncd 过早退出（{status}）");
        }
        ping_ok(&socket)
    });
    assert!(
        ready,
        "第一个 qxyncd 20s 内没有就绪（socket {}）",
        socket.display()
    );

    let second = spawn().wait_with_output().expect("等第二个 qxyncd 退出");
    assert_eq!(
        second.status.code(),
        Some(3),
        "「daemon 已在运行」必须退出码 3（unit 的 RestartPreventExitStatus=3 认它），实际 {:?}",
        second.status
    );
}
