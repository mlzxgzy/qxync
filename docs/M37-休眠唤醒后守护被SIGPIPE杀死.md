# M37 · 休眠唤醒后守护被 SIGPIPE 杀死（「服务自己停了，得手动启动」）

真机故障：笔记本**盒盖休眠一段时间再开盖**，qxyncd 就没了 —— 没有日志、没有卸载提示，
`systemctl --user status qxyncd` 只剩一行 `inactive (dead)`，用户必须手动 start。

本文记录现场证据、两层根因（为什么死 / 为什么没被 systemd 拉起来）、修复与验证方式。

---

## 1. 现场证据（2026-10-26，tb14 / CachyOS，systemd 262）

### 1.1 时间线（`journalctl -b`）

| 时刻（CST） | 事件 |
|---|---|
| 12:06:32 | `systemd[986]: Started qxync 常驻同步守护（qxyncd）` —— 本次开机由 **user 单元**拉起（PID 1025） |
| 15:54:41 | `systemd-logind: Lid closed.` |
| 15:54:48 | `PM: suspend entry (s2idle)` |
| 15:57:08 | `Lid opened.` / `PM: suspend exit` —— 休眠 **2m20s** |
| 15:57:14 | `user-1000.slice: Unit now thawed.`（唤醒后 user 管理器恢复） |
| **15:57:28** | `qxyncd.service: Killing process 2011 (fusermount3) with signal SIGKILL.` + `Consumed 2min 47s CPU … memory peak 1.2G` —— **进程已死，systemd 正在收尸** |
| 16:38→16:58、17:08→19:59 | 又两次休眠/唤醒；守护早在 15:57 就没了，所以这两次什么都没发生 |
| 20:02:41 | 用户手动起了一个 daemon（`qxyncd 启动 pid=368191`）—— 这就是「还得手动启动」 |

★ 死亡时刻 = **唤醒后第 20 秒**，正好是唤醒后第一轮「变更轮询（30s）/ 会话保活（120s）」
第一次真正去用网络的时候。

### 1.2 进程是怎么死的

```
$ systemctl --user status qxyncd
     Active: inactive (dead) since Mon 2026-10-26 15:57:28 CST; 4h 8min ago
   Duration: 3h 50min 55.758s
    Process: 1025 ExecStart=/usr/bin/qxyncd --foreground (code=killed, signal=PIPE)
```

`ExecMainCode=2`（CLD_KILLED）+ `ExecMainStatus=13` = 被 **SIGPIPE** 杀死。
不是 panic（panic 是 `code=exited`），不是 OOM（那是 `signal=KILL`），也不是正常退出。

守护自己的文件日志 `~/.local/state/qxync/log/qxyncd.log.2026-10-26` **断在 07:53:57Z
（= 15:53:57 CST）**，而 journal（stderr）里最后一条是 15:54:14 —— 死亡前约 3 分钟的
日志停在 `tracing_appender::non_blocking` 的缓冲区里没落盘。**这正是「被信号瞬杀、
没机会跑任何清理」的指纹**：正常退出/panic 展开都会让 `WorkerGuard` drop 并 flush。

### 1.3 为什么 systemd 没有自动重启

```
$ systemctl --user show qxyncd.service -p Restart -p Result -p NRestarts -p ExecMainStatus
Restart=on-failure
Result=success          ← ★ 信号死亡，却记成 success
NRestarts=0             ← 一次都没重启过
ExecMainStatus=13
```

**这是 systemd 的既定语义，不是日志异常**：`systemd.service(5)` 的 `SuccessExitStatus=`
一节写明 —— 除 `Type=oneshot` 外，**`SIGHUP` / `SIGINT` / `SIGTERM` / `SIGPIPE` 这 4 个
信号被杀都算「干净退出」**。于是：

* `Result=success`（`code=killed, signal=PIPE` 被映射成成功）；
* `Restart=on-failure` 只对「失败」生效 ⇒ **永远轮不到重启**；
* 用户看到的就是 `inactive (dead)` + 「得手动启动」。

换句话说：`SIG_DFL` 的 SIGPIPE 不只杀进程，还**恰好落在 systemd 的 clean-exit 白名单里**，
让「崩溃自动重启」这条兜底彻底失效 —— 两处错误正好互相掩盖，所以这个故障才那么难查。

★ 顺带更正一条常见误解：`Restart=always` 才是这类死亡的解药（`on-failure` 无论信号是
SIGPIPE 还是别的 clean-exit 信号都不会重启），这也是 §3.2 的依据。

---

## 2. 根因：daemon 把 SIGPIPE 设回了 SIG_DFL

`crates/qxync-daemon/src/main.rs`（0.7.10 及以前）：

```rust
fn main() -> Result<()> {
    // 同 CLI：别让 EPIPE 变成 panic
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
```

这行从 M1.5（第一个 daemon 提交 `f8f3633`）就存在，注释说明意图是「别让 EPIPE 变成
panic」—— 这**对 CLI 是对的**（`qxync ls | head` 里 stdout 被提前关掉，按 shell 惯例该
安静地死；`crates/qxync-cli/src/main.rs:749` 保留这个行为），但 daemon 是**长驻多连接**
进程：

| daemon 里的写端 | 休眠唤醒后会发生什么 |
|---|---|
| NAS HTTPS 长连接 / 会话保活（每 120s） | 连接在对端早已失效（TCP RST / NAT 超时），第一次 `write()` 就 EPIPE |
| 变更轮询（每 30s） | 同上；唤醒后立刻到期的定时器正好触发 |
| 本地 IPC socket（CLI / GUI 客户端） | 客户端断开后写它就 EPIPE |
| journald 的 stderr 管道 / FUSE 会话 | 极端情况下也会 |

Rust 运行时默认把 SIGPIPE 设成 `SIG_IGN`，所以这些写入本来只会**返回 `EPIPE` 错误**，
由各调用点的错误处理消化（重连 / 记一笔离线）。被改成 `SIG_DFL` 之后，**内核在第一次
这样的写入时直接 SIGPIPE 杀掉整个进程** —— 这就是唤醒后第 20 秒死亡的全部机制。

---

## 3. 修复

### 3.1 忽略 SIGPIPE（`crates/qxync-daemon/src/main.rs`）

```rust
// ★ M37：守护进程必须忽略 SIGPIPE（Rust 运行时默认处置），绝不能再照搬 CLI 的 SIG_DFL
unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
```

写坏连接从此变成普通的 `EPIPE` 错误。daemon 里唯一会在 EPIPE 上 panic 的是
`println!/print!`，而它只在一次性的 `--print-config` 路径里用，长驻循环不碰。

### 3.2 单元加固（`packaging/systemd/qxyncd.service`、`qxyncd@.service`）

```ini
[Unit]
StartLimitIntervalSec=0        # 重启永不放弃（默认 5 次/10s 后 systemd 躺平）

[Service]
Restart=always                 # ★ 关键：on-failure 永远不覆盖 SIGPIPE 这种 clean-exit 信号
RestartSec=5
RestartMode=direct             # 自动重启不再走 stop 阶段
RestartPreventExitStatus=3     # 「已有 daemon 占着 socket」不是失败，别重试
```

* `Restart=always` 是**唯一**能覆盖 SIGPIPE/SIGTERM 这类「clean exit」的策略
  （见 §1.3）。除**用户明确 `systemctl stop`** 外，任何死亡都会 5 秒内被拉起来。
* ★ 语义变化：`qxync daemon stop`（IPC，干净退出 0）与 GUI 的「停止」也会被拉回来。
  装了 unit 就只用 `systemctl --user stop|start|restart|reload` 管它。
* `RestartPreventExitStatus=3`：`bind_socket` 发现「另一个 daemon 已经占着 socket」时
  改为返回专用错误 `daemon::AlreadyRunning`，`main` 映射成**退出码 3**（日志文案不变）。
  没有它的话，`Restart=always` + 不限次数会让「你手动起过一个 daemon」变成每 5 秒一次的
  永久重启风暴（2026-10-26 20:08 现场就抓到过这个循环：`daemon 已在运行` 每 3 秒一次）。
* ⚠ `RestartMode=direct` **只作用于自动重启**：显式 `systemctl restart|stop` 仍走
  「SIGTERM → 挂载守护模式不退 → 等满 `TimeoutStopSec=30` → SIGKILL」这条 M15/T6 的
  老路径（挂载点靠 `auto_unmount` + 启动僵尸清理兜底）。想要「重启也秒级」另有机会，
  见 §6。
* ⚠ `StartLimitIntervalSec=0` 的代价：除退出码 3 以外，任何**启动期**失败（配置坏、
  socket 无权限…）都会每 5s 重试一次并一直刷日志。这是刻意取舍 —— 对「同步客户端」来说
  「可能吵」远好过「静默躺平」；真要收窄，把 §6 的 P1 条做了再考虑给启动失败配专用码。

---

## 4. 验证

### 4.1 单元文件

```
$ systemd-analyze verify packaging/systemd/qxyncd.service packaging/systemd/qxyncd@.service
（无输出 = 通过）
```

### 4.2 信号处置：旧二进制死、新二进制活（隔离 XDG 目录，不碰真实配置/挂载）

```
$ (隔离环境起 /usr/bin/qxyncd)   → kill -PIPE → pid 已死      # 0.7.10 行为
$ (隔离环境起 target/release/qxyncd) → kill -PIPE → pid 存活   # 修复后
```

### 4.3 回归测试（`crates/qxync-daemon/tests/sigpipe_survival.rs`）

```
$ cargo test -p qxync-daemon --test sigpipe_survival
test sigpipe_must_not_kill_the_daemon ... ok
```

测试内容：隔离 XDG 目录 + `--socket` 起一个「空转待命」的 qxyncd → 等 IPC `ping` 通 →
`kill(SIGPIPE)` → 断言进程仍活着 **且 IPC 仍应答** → `SIGTERM` 收尾。

### 4.4 退出码

```
$ (起一个 daemon 占住 socket) ; 再用同一 socket 起第二个
第二个实例退出码=3 (期望 3)
ERROR qxyncd: qxyncd 启动失败: daemon 已在运行（socket …）
```

### 4.5 真机验收（需要人 + 真 NAS，见 §5 安装后执行）

1. `systemctl --user status qxyncd` 记下 `Main PID`；
2. 盒盖休眠 ≥ 3 分钟再开盖；
3. 期望：`Main PID` 不变（没死），`journalctl --user -u qxyncd -b | tail` 在唤醒后
   出现新一轮「同步轮询」，`ls ~/qxync/default/qsync` 正常（挂载没断）。
4. 若还是死了：`systemctl --user status qxyncd` 应显示最近一次自动重启的时间与
   `NRestarts`（`systemctl --user show qxyncd -p NRestarts -p Result`），而不是
   `inactive (dead)` —— 至少不会回到「必须手动 start」。

真机验收里还值得顺手确认一条**与休眠无关的第二条 SIGPIPE 通道**：`qxync get <大文件>`
跑到一半 Ctrl-C，旧版本会因为 IPC 写端断开而把 daemon 一起杀掉；修复后 daemon 只会在
日志里留一条写失败，`qxync status` 照常。

---

## 5. 安装到真机（需要 root，仓库里只改源码/单元文件）

```bash
cd ~/项目/qxync
CARGO_HOME=$PWD/.cargo-home cargo build --release -p qxync-daemon

# 1) 二进制（SIGPIPE 修复在这一步生效）
sudo install -m755 target/release/qxyncd /usr/bin/qxyncd

# 2) 单元（Restart=always / StartLimitIntervalSec=0 / RestartPreventExitStatus=3）
sudo install -m644 packaging/systemd/qxyncd.service  /usr/lib/systemd/user/qxyncd.service
sudo install -m644 packaging/systemd/qxyncd@.service /usr/lib/systemd/user/qxyncd@.service
systemctl --user daemon-reload

# 3) 收掉「不受 systemd 管」的那个 daemon，再交给 systemd
qxync daemon stop            # 如果当前 daemon 是 CLI/GUI 起的（20:02 那个）
systemctl --user restart qxyncd
systemctl --user status qxyncd
```

不想覆盖 `/usr/lib` 里的单元时，可以用 **user drop-in**（无需 root）：

```bash
mkdir -p ~/.config/systemd/user/qxyncd.service.d
cat > ~/.config/systemd/user/qxyncd.service.d/50-m37-always-restart.conf <<'EOF'
[Unit]
StartLimitIntervalSec=0

[Service]
Restart=always
RestartSec=5
RestartMode=direct
RestartPreventExitStatus=3
EOF
systemctl --user daemon-reload && systemctl --user restart qxyncd
```

★ **别两边同时管**：`qxync daemon start/stop`、GUI 的「启动/停止」、systemd 三者只能选一个。
现在能顺便确认一件事：`systemctl --user status qxyncd` 是 `inactive` 而
`qxync status` 却能连上 —— 说明此刻有一个**不受 systemd 管**的 daemon 在跑（正是
「手动启动」留下的），装完上面三步就统一了。

---

## 6. 已知但本次**没有**做的加固（审计结论，按优先级）

唤醒场景本身已经由 §3 收口；以下是同一轮只读审计（`resume-audit`）发现的、**不是本次
事故根因**但值得排期的洞：

| 优先级 | 问题 | 位置 | 建议 |
|---|---|---|---|
| P1 | 显式 `systemctl restart/stop` 仍要等 30s + SIGKILL（M15/T6 的「SIGTERM 保留挂载」副作用），`RestartMode=direct` 救不了 | `daemon.rs` `hold_mounts`（`mount_hold_secs=0` 无限） | systemd 下首次 SIGTERM 直接 clean unmount（用 `INVOCATION_ID` 判断），或 unit 加 `ExecStop=/usr/bin/qxync daemon stop` |
| P1 | 唤醒后 30s~300s 内可能「活着但卡住」：`State.client` 是 tokio Mutex，多处**持锁跨网络 await**（单请求超时 300s） | `daemon.rs` login/check_alive/with_client/probe_online | 探活/保活用独立 client 或更短 per-request timeout |
| P2 | 上传 / 删除 worker 是裸 `std::thread`，无 `catch_unwind`、无监督 ⇒ 一次 panic = 队列永久卡住 | `qxync-fuse/src/upload.rs`、`delete.rs` worker_loop | 加 catch_unwind + 存活检测/重建 |
| P2 | 5 个后台任务（poller/脱水/journal/保活…）handle 只在停止时 timeout 轮询且**忽略 Err** ⇒ 某任务 panic 后无人知晓 | `daemon.rs` `stop_sync_engine` 附近 | 监督任务存活，异常时重建或进离线态 |
| P2 | 运行期不检测 FUSE 会话死亡（进程活着、挂载点 ENOTCONN），只有启动期僵尸清理 | `qxync-fuse` session 线程 | 运行期探活 + 自动重挂 |
| P2 | 无 `logind PrepareForSleep` 监听、无 `sd_notify` + `WatchdogSec` | daemon 全仓无 dbus/notify 依赖 | 唤醒时主动丢连接池 + 重登（把恢复从 30s 缩到秒级）；watchdog 能发现「活着但死锁」（成本：新依赖） |
