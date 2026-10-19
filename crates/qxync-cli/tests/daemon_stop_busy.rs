//! 集成测试：`qxync daemon stop` 停止前的「挂载点被谁占用」检查。
//!
//! 用一个**假 daemon**（本测试进程里的 Unix socket 服务端）回答 `ping` / `mounts` /
//! `shutdown`，这样不用真挂 FUSE 也能钉住两条用户可见的行为：
//!
//! * 挂载点里有进程在用（cwd 在里面）→ **拒绝停止**，stderr 里报出 pid / 工作目录；
//! * `--force` → 照停（且 CLI 会等「daemon 进程」真的退出才说「已停止」）。
//!
//! 真机行为（真 FUSE 挂载 + 真 daemon）由 `cargo test` 之外的验收矩阵覆盖；这里
//! 只钉 CLI 侧协议与决策，不依赖 NAS。

use qxync_core::ipc::{
    decode_line, encode_line, ErrorKind, MountInfo, PingData, Request, RequestEnvelope, Response,
    ShutdownData,
};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "qxync-cli-stop-{}-{}-{}",
        std::process::id(),
        tag,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

/// 测试进程的子进程兜底清理。
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 假 daemon 的「进程」：Ping 报这个 `sleep` 的 pid；收到 Shutdown 就把它杀掉，
/// 让 CLI 的「等进程真的消失」能收尾。
struct DaemonProc {
    child: Arc<Mutex<Option<Child>>>,
}

impl DaemonProc {
    fn new() -> Self {
        let child = Command::new("sleep")
            .arg("120")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("测试需要 `sleep` 命令（Linux coreutils）");
        Self {
            child: Arc::new(Mutex::new(Some(child))),
        }
    }

    fn pid(&self) -> u32 {
        self.child.lock().unwrap().as_ref().unwrap().id()
    }

    fn holder(&self) -> Arc<Mutex<Option<Child>>> {
        self.child.clone()
    }
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// 起一个假 daemon：`ping` 回 `daemon_pid`，`mounts` 回 `mountpoint`，`shutdown`
/// 回成功并杀掉 `daemon_pid` 对应的进程。
fn spawn_stub(
    socket: &Path,
    mountpoint: PathBuf,
    daemon_pid: Arc<Mutex<Option<Child>>>,
) -> std::thread::JoinHandle<()> {
    let listener = UnixListener::bind(socket).unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { break };
            let Ok(reader_stream) = stream.try_clone() else {
                continue;
            };
            let mut reader = BufReader::new(reader_stream);
            let mut line = String::new();
            match reader.read_line(&mut line) {
                // `ipc::available()` 只连一下就关（探活）→ 没有请求，跳过。
                Ok(0) => continue,
                Ok(_) => {}
                Err(_) => continue,
            }
            let resp = match decode_line::<RequestEnvelope>(line.as_bytes()) {
                Ok(env) => match env.req {
                    Request::Ping => {
                        let pid = daemon_pid.lock().unwrap().as_ref().map(|c| c.id());
                        Response::ok(PingData {
                            pong: true,
                            daemon_version: "stub".into(),
                            pid: pid.unwrap_or(0),
                            uptime_secs: 1,
                        })
                    }
                    Request::Mounts => Response::ok(vec![MountInfo {
                        mountpoint: mountpoint.clone(),
                        remote: "/home".into(),
                        readonly: false,
                    }]),
                    Request::Shutdown => {
                        // 模拟「收到停止请求 → 进程退出」：先杀掉假 daemon 进程，
                        // CLI 的 `wait_pid_gone` 才能立刻看到它消失。
                        if let Some(mut c) = daemon_pid.lock().unwrap().take() {
                            let _ = c.kill();
                            let _ = c.wait();
                        }
                        Response::ok(ShutdownData {
                            unmounted: 1,
                            ..Default::default()
                        })
                    }
                    other => Response::err(
                        ErrorKind::BadRequest,
                        format!("stub 不支持 {}", other.method()),
                    ),
                },
                Err(e) => Response::err(ErrorKind::BadRequest, format!("请求解析失败: {e}")),
            };
            let _ = stream.write_all(&encode_line(&resp).unwrap());
        }
    })
}

fn qxync_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qxync")
}

fn run_stop(socket: &Path, force: bool) -> std::process::Output {
    let mut cmd = Command::new(qxync_bin());
    cmd.arg("--socket").arg(socket).arg("daemon").arg("stop");
    if force {
        cmd.arg("--force");
    }
    cmd.output().expect("跑 qxync 失败")
}

#[test]
fn stop_refuses_when_mount_is_in_use() {
    let dir = tmp("busy");
    let socket = dir.join("qxyncd.sock");
    let mount = dir.join("mnt");
    std::fs::create_dir_all(&mount).unwrap();
    let daemon = DaemonProc::new();
    let _stub = spawn_stub(&socket, mount.clone(), daemon.holder());

    // 占用者：cwd 在挂载点里。
    let _occupier = Guard(
        Command::new("sh")
            .arg("-c")
            .arg("sleep 60")
            .current_dir(&mount)
            .spawn()
            .unwrap(),
    );
    // 等进程真的起来、cwd 生效（/proc/<pid>/cwd 可用）。
    std::thread::sleep(Duration::from_millis(300));

    let out = run_stop(&socket, false);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "挂载点被占用时必须拒绝停止；stdout={} stderr={stderr}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(stderr.contains("没有停止 qxyncd"), "stderr={stderr}");
    assert!(
        stderr.contains(&format!("pid {}", _occupier.0.id())),
        "应该报出占用者的 pid；stderr={stderr}"
    );
    assert!(stderr.contains("工作目录"), "stderr={stderr}");
    assert!(
        stderr.contains(mount.to_str().unwrap()),
        "应该报出挂载点；stderr={stderr}"
    );
    assert!(
        stderr.contains("--force"),
        "应该提示 --force；stderr={stderr}"
    );

    // 拒绝时**不能**发 shutdown：假 daemon 进程还活着。
    let _ = daemon.pid(); // holder 还在 = 没被杀
}

#[test]
fn stop_force_proceeds_and_waits_for_process_exit() {
    let dir = tmp("force");
    let socket = dir.join("qxyncd.sock");
    let mount = dir.join("mnt");
    std::fs::create_dir_all(&mount).unwrap();
    let daemon = DaemonProc::new();
    let _stub = spawn_stub(&socket, mount.clone(), daemon.holder());

    // 占用者（force 时不会拦）。
    let _occupier = Guard(
        Command::new("sh")
            .arg("-c")
            .arg("sleep 60")
            .current_dir(&mount)
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(Duration::from_millis(300));

    let out = run_stop(&socket, true);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout={stdout} stderr={stderr}");
    assert!(stdout.contains("已停止"), "stdout={stdout}");
    // 强制停止仍然把「谁在用」写出来（stderr），别悄悄硬停。
    assert!(
        stderr.contains("--force") || stderr.contains("挂载点仍被占用"),
        "stderr={stderr}"
    );
}

#[test]
fn stop_says_not_running_when_socket_is_gone() {
    let dir = tmp("gone");
    let socket = dir.join("qxyncd.sock");
    let out = run_stop(&socket, false);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout={stdout}");
    assert!(stdout.contains("未在运行"), "stdout={stdout}");
}
