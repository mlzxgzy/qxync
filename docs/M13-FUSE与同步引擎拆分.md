# M13：FUSE 与同步引擎拆分 —— 让挂载活过 daemon 重启

> **【已收敛为 `M15-待办任务清单.md`——本方案结论是「不做」】**
> 本文是背景调研档案，**不是执行入口**。
>
> **决策**：不拆分 FUSE 与同步进程。理由见 M15 §0.3——
> 需 16 个 `LocalView` 方法跨进程 RPC，且 `reconcile_view` 每 30s 全量拉 `nodes()`，
> 节点多时性能不可接受；而 `M14` 的思路用 200 行就拿到了 90% 的收益（T5/T6/T7）。
> 本文保留作为**将来若真需要双进程时**的成本估算参考。

> 问题：`systemctl --user restart qxyncd` 之后 `/mnt/qxync` 失效，用户侧的
> IDE 索引、文件管理器、正在跑的脚本全部报 `transport endpoint is not connected`。
>
> 结论先行：**挂载失效不是 bug，是 FUSE 的进程绑定特性**。`fuser::spawn_mount2`
> 返回的 `BackgroundSession` 持有 `/dev/fuse` 的 fd，进程一死内核就关闭该 fd，
> 挂载点立刻变成"传输端点未连接"。**只要不拆进程，换任何写法都救不了。**
> 真正能解决的是把 FUSE 挪进一个**永不重启**的独立进程。

---

## 0. 先定性：为什么"拆代码"没用

`crates/qxync-daemon/src/main.rs:2` 的模块文档已经写明了这个约束：

> qxyncd —— qxync 守护进程（M1.5：本地 IPC；**FUSE 由本进程持有**）。

挂载的建立与持有链路（已逐行核实）：

| 环节 | 位置 | 说明 |
| --- | --- | --- |
| 建挂载 | `daemon.rs:1731` | `qxync_fuse::spawn(fs, &mp, ...)` |
| 拿会话 | `lib.rs:3709-3721` | `fuser::spawn_mount2` → `BackgroundSession` |
| 会话存哪 | `daemon.rs:1782` | `MountEntry.session: Option<BackgroundSession>` |
| 谁持有 | `daemon.rs:106` | `State.mounts: StdMutex<HashMap<PathBuf, MountEntry>>` |
| 退出清理 | `daemon.rs:1914` `shutdown_all_mounts` | 收到 SIGTERM → 逐个 `umount` |
| 兜底 | `packaging/systemd/qxyncd.service` | `TimeoutStopSec=30`，超时 SIGKILL |

所以：
- `systemctl restart` → SIGTERM → `shutdown_all_mounts`（`daemon.rs:1914`）**主动卸载**；
- `kill -9` / OOM → fd 由内核关闭 → 挂载点残留但不可用；
- 两条路都指向同一句：**`State` 所在的进程死了，挂载就没了。**

**因此"把 fuse 部分和 sync 部分拆开"如果理解成"拆成两个 crate/模块"，收益是零。**
必须拆成**两个进程**，且 FUSE 那个进程要在`qxyncd` 重启时保持不动。

---

## 1. 拆分的真实约束（先摸清再动手）

### 1.1 好消息：`LocalView` 已经是干净的接缝

`crates/qxync-fuse/src/lib.rs:645` 定义的 `LocalView` trait 就是同步引擎与FUSE 之间的**全部**接口：

```rust
pub trait LocalView: Send + Sync {
    fn remote_root(&self) -> &str;
    fn node(&self, remote: &str) -> Option<LocalNode>;
    fn nodes(&self) -> Vec<LocalNode>;
    fn known_dirs(&self) -> Vec<String>;
    fn has_pending(&self, remote: &str) -> bool;
    fn apply_remote_meta(&self, remote: &str, is_dir: bool, size: u64, mtime: i64) -> bool;
    fn invalidate_content(&self, remote: &str) -> bool;
    fn remove_remote(&self, remote: &str) -> bool;
    fn mark_dirty(&self, remote: &str) -> std::io::Result<()>;
    fn stash_conflict(&self, remote: &str, conflict_name: &str) -> std::io::Result<PathBuf>;
    fn enqueue_upload(&self, ...) -> std::io::Result<()>;
    // M9 新增，默认空实现
    fn apply_listing(&self, _dir: &str, _entries: &[DirEntry]) {}
    fn drop_listing(&self, _dir: &str) {}
    fn pending_refresh(&self, _remote: &str) -> bool { false }
    fn spawn_content_refresh(&self, _remote: &str) {}
}
```

**这意味着跨进程 RPC 只需要实现这16 个方法**，同步引擎（`qxync-daemon/src/sync.rs`）
一行都不用改——它只认 trait。这是整个拆分最省力的地方，务必保持。

### 1.2 坏消息：FUSE 侧持有的状态分三类

| 类别 | 内容 | 位置 | 跨进程后怎么办 |
| --- | --- | --- | --- |
| **可重建（低成本）** | `Inner.nodes` 节点表 | `lib.rs:303` | 冷路径懒加载：`lookup` 已有「节点表 → 父目录快照 → NAS stat」三级回退（`lib.rs:2086-2133`），重启后首次访问慢一点，之后回到正轨 |
| **可重建（靠落盘）** | 水合位图 `chunks_done` | `.qxstate`（`lib.rs:78-89`） | 已落盘，天然活过重启——**这正是当前设计的最大优势** |
| **必须共享（不可重建）** | `PinMap` | `daemon.rs:1639` `with_pins(state.pins.clone())` | 共享 `Arc` 断了→ 要走 IPC 或落盘 |
| |上传/删除队列 worker | `upload.rs:379`、`delete.rs:255` | 各是独立 OS 线程，跟着挂载进程走最自然 |
| | `Client`（含 sid） | `lib.rs:715` | 挂载点本来就持有**自己那份** Client（`daemon.rs:1632-1633`），靠 `SidRefresher` 热更新（`daemon.rs:1744`）——**这个设计天生适合拆分** |
| | `DeleteGuard` | `lib.rs:1428` | 内存计数，丢了不影响正确性 |

### 1.3 最需要小心的：`LocalView::nodes()` 的调用频率

`reconcile_view`（`qxync-daemon/src/sync.rs:598`）每轮对账开头就调`view.view.nodes()`（`sync.rs:637`）
和 `view.view.known_dirs()`（`sync.rs:620`），把结果灌进 `BTreeSet`：

```rust
for n in view.view.nodes() { ... }        // sync.rs:637  全部节点
for d in view.view.known_dirs() { ... }   // sync.rs:620  全部目录
```

这是**每轮轮询（默认 30s，`daemon.rs` 的 `QXNYC_POLL_INTERVAL`）一次全量拉取**。
跨进程后如果每次都走JSON over unix socket，10 万节点时每轮要传几 MB —— 不可接受。

**解法**：加一个批量接口，把 `nodes()` / `known_dirs()` 合成**流式分页**，
或者在挂载进程侧加一个「节点表快照缓存」，由 `apply_listing` 等写操作驱动版本号递增，
`nodes()` 请求带上版本号——版本没变就直接复用上次的反序列化结果。

**建议**：先做最笨的版本（每轮全量 JSON），实测节点规模后再优化。
过早优化是这里最容易犯的错。

---

## 2. 三个方案对比

|维度 | A：拆 crate 不拆进程 | **B：挂载独立进程** | C：内核态/特权方案 |
| --- | --- | --- | --- |
| 解决重启失效 | ❌ | ✅ | ✅ |
| 改动量 | 极小（挪文件） | 中（新增一个 bin + RPC） | 极大（写 C 内核模块） |
| 需要 root | 否 | 否 | **是** |
| systemd 复杂度 | 不变 | 两个 unit | 一个 unit + 装模块 |
| 崩溃影响面 | 全挂 |挂载挂载进程仍活，同步独立崩| 需内核模块稳定性 |
| 端口/权限 | — | 额外一个 unix socket | — |
| 推荐度 | 纯可读性收益 | **推荐** | 不推荐 |

方案 C 指Linux 的 `fuse-overlayfs` / 自写 `cldflt` 等价物（Windows 才是 minifilter）。
Linux 上FUSE 挂载**必须**由用户态进程持有 fd，没有内核态捷径。所以这条路排除。

**选定方案 B。**

---

## 3. 方案 B 的架构

```
┌─ systemd --user ────────────────────────────────────────┐
│qxcync-mountd.service   Restart=on-failure（崩了才重启）    │
│    └── 持有 BackgroundSession /节点表 / 上传删除队列       │
│         └── unix socket: $XDG_RUNTIME_DIR/qxync/mount.sock│
│                                                          │
│  qxyncd.service       Restart=always  ← 随便重启          │
│    └── 登录 / 会话保活 / 轮询 / 对账 / 脱水调度             │
│         └── unix client → mount.sock                     │
└──────────────────────────────────────────────────────────┘
```

### 3.1 职责切分

| 能力 | 归属 | 理由 |
| --- | --- | --- |
| FUSE 会话、inode 分配、节点表 | **mountd** | 挂载生命周期的一部分 |
| 水合、脱水执行、`inval_inode` | **mountd** | 需要 `Notifier`（`lib.rs:3693`），且和节点表同处 |
| 上传/删除队列 worker | **mountd** | 作业对象是缓存文件，跨进程传递反而复杂 |
| `PinMap` | **mountd**（权威） | 脱水要读它；qxyncd 改动走 RPC |
| NAS 登录、sid 保活重登 | **qxyncd** | 逻辑上属同步；通过 `SidRefresher` 推给 mountd |
| 轮询、对账、baseline | **qxyncd** | 纯逻辑，无 FUSE 依赖 |
| 脱水**决策**（选谁脱水） | **qxyncd** → mountd 执行 | `dehydrate::plan` 是纯函数（`dehydrate.rs:190`），可留在 qxyncd |
| journal 落库 | **qxyncd** | SQLite 单写者原则 |

> 脱水决策的归属值得单独说：`dehydrate::plan`（`dehydrate.rs:190`）是**纯函数**，
> 输入 `Vec<Candidate>` 输出 `Vec<Decision>`，完全不碰 FUSE。
> 拆开后可以让 qxyncd 决策、mountd 执行——这样 `QXNYC_CACHE_LIMIT` 之类策略改动
> 不需要重启挂载进程。

### 3.2 现有 M10 设计是天然的拆分基础

值得强调：`daemon.rs:1632-1633` 让**每个挂载点持有自己的 `Client`副本**，
sid 靠 `SidRefresher` 闭包热更新（`daemon.rs:1744` → `lib.rs:780`）。
这个设计当初是为了"换账号登录时不给旧挂载点推错 sid"（`daemon.rs:86-88` 注释），
但**副作用正好是让挂载点对 daemon 的依赖只剩「给我一个新 sid」这一个回调**。

拆分后这个回调变成一次 IPC，语义不变。

---

## 4. 实施步骤

### M13.0：先做「快速重挂载」（不拆进程，1 天）

**这一步独立有价值，且是拆分的兜底预案。**

现在重启后要手动 `qxync mount`。加一个 `mount_state.json`（落盘到
`$XDG_STATE/qxync/`），记录每个挂载点的 `mountpoint` / `remote` / `read_write` / `cache_mode` / `conflict`，
daemon 启动时读它并自动重挂。

配合已有的 `restore_tasks` 开关（`main.rs:52`，默认关闭）——改成**默认开启**
并让它也能恢复「手动挂载」而不只是「任务里的挂载」。

> 注意 `main.rs:53-56` 的顾虑是刻意的：「恢复会凭空挂载，可能让上一次跑崩留下的挂载复活」。
> 缓解办法：挂载前先 `is_mounted()` 检查（`daemon.rs:1927` 已有该函数），
> 已挂载且远端一致就跳过；不一致就先 `fusermount3 -u`。

收益：重启后 3~5 秒自动恢复，用户基本无感。**但挂载点仍会短暂消失**，
在挂载点上有长跑进程（IDE 索引、rsync、数据库）的场景下仍会中断。

### M13.1：抽出 mountd 骨架（2 天）

1. 新 crate `qxync-mountd`，依赖 `qxync-fuse` + `qxync-client` + `qxync-core`。
2. 从 `daemon.rs` 搬`do_mount`（`daemon.rs:1578-1800`）的挂载部分，
   去掉 sync 相关依赖，只留 `FsHandle` + `BackgroundSession` + 队列。
3. 加一个极简 IPC server（手写 `serde_json` over unix socket 即可，
   参照 `qxync-core/src/ipc.rs` 的 `Request`/`Response` 风格，约 15 个 method）。
4. **此时仍然单进程**：qxyncd 同时持有 mountd 的逻辑，两者走**直接函数调用**。

这一步的目的是**先把跨进程接口设计好、跑通**，不改变部署形态。
若发现接口不够用，改起来便宜（还没有真实的两进程）。

### M13.2：实现 `RpcLocalView`（2 天）

在 `qxync-core`（或新的 `qxync-ipc` crate）实现 `LocalView` 的 RPC 客户端版本：

```rust
pub struct RpcLocalView { sock: Arc<Mutex<UnixStream>> }

impl LocalView for RpcLocalView {
    fn nodes(&self) -> Vec<LocalNode> { /* 一次请求，返回 JSON 数组 */ }
    fn known_dirs(&self) -> Vec<String> { /* 同上 */ }
    fn apply_remote_meta(&self, ...) -> bool { /* 一次请求 */ }
    // ... 其余 13 个方法
}
```

**注意**：trait 方法是同步的（`fn nodes(&self) -> Vec<LocalNode>`），
而 IPC 是异步的。两种做法：

- **(a) socket 设阻塞超时**（如 2s），`reconcile_view` 在 tokio worker 上
  用 `spawn_blocking` 包一层。改动最小，推荐。
- **(b) 把 trait 改成 async**。改动波及 FUSE 回调（`lib.rs:1298` 等），
  会污染热路径，**不推荐**。

选 (a)。`LocalView` 已经`Send + Sync`（`lib.rs:645`），
`spawn_blocking` 天然满足。

### M13.3：拆分部署（1 天）

1. 加 `packaging/systemd/qxync-mountd.service`：
   ```ini
   [Service]
   ExecStart=/usr/bin/qxync-mountd --foreground
   Restart=on-failure# 崩了才重启
   RestartSec=3
   TimeoutStopSec=30
   ```
   注意**不能**是 `Restart=always`——那会让 `systemctl stop` 也被拉起来。
2. 改 `qxyncd.service`：`After=qxync-mountd.service`、`Requires=`（可选）。
   `qxyncd` 启动时若mountd socket 不存在，则**只启动同步、不挂载**，
   等 mountd 起来后再通过 RPC 触发挂载。
3. `qxyncd` 的 SIGTERM 处理**不再卸载挂载**——
   `shutdown_all_mounts`（`daemon.rs:1914`）删掉，改成「通知 mountd 卸载」或干脆不管
   （挂载生命周期归mountd 管）。

### M13.4：补齐边角（1 天）

- `qxync umount` 命令改为转发给 mountd。
- `qxync status` 的挂载部分改为从 mountd 查询。
- mountd 崩溃后的挂载重建：`Restart=on-failure` 会重启进程，
  靠 M13.0 的 `mount_state.json` 自动重挂。
- **GUI 的托盘图标**：现在大概率调`qxync daemon start/stop`
  （`qxync-gui` 依赖 daemon 存活），拆分后要改成只管同步、挂载另管。

---

## 5. 风险与已知代价

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| **RPC 延迟进入轮询热路径** | `nodes()` 每 30s 一次全量拉取，节点多时变慢 | 先全量后优化；加版本号缓存 |
| **两份 `Client`各自登录** | mountd 和 qxyncd 各持一份 sid，可能不一致 | mountd **不自己登录**，sid 全靠 `SidRefresher` 从 qxyncd 拿（挂载点无网时也能工作） |
| **pin 状态双写** | 两边都改会打架 | mountd 权威，qxyncd 改动走 RPC；`pins` 表已在 SQLite（`store.rs:61`）可作恢复源 |
| **两个 systemd unit 的启停顺序** | 停机时 mountd 先死，qxyncd 还在 RPC 上 | `qxyncd.service` 加 `After=` + `PartOf=`；或让 qxyncd 容忍 socket 断开（全部方法返回默认值） |
| **调试复杂度上升** | 两个日志文件、两个 pid | 现有日志已经按天滚动（`main.rs:init_logging`），沿用；`status` 里同时报两个 pid |
| **FUSE 进程变成"僵尸"守护** | mountd 挂了但 systemd 没拉起 | `Restart=on-failure` + M13.0 的状态文件重建 |
| **升级要发两个二进制** | 打包脚本要改 | `packaging/` 已分平台目录（`packaging/arch`、`packaging/systemd`），加一个 bin 影响小 |

**最大的代价说清楚**：进程内 `Arc<Mutex<Inner>>` 变成跨进程 socket 后，
**FUSE 热路径（`getattr`/`readdir`/`read`）不能走 RPC**——那会把每次 `ls` 拖慢几个数量级。
本方案的设计前提就是「FUSE 热路径全部留在 mountd 内部，RPC 只用于低频的同步侧操作」。
**如果将来有人在 `getattr` 里调 RPC，那就是把 10 万次 `ls` 变成 10 万次 socket 往返。**
建议在 `qxync-fuse` crate 顶部加一条显式约束注释（类似现有 `lib.rs:1-13` 的铁则风格）。

---

## 6. 一个更轻的替代：mdev 风格的自恢复挂载

如果拆分成本太高，有个**改动极小**的折中——

Linux 的 `mount -t fuse` 在 `/etc/fstab` 里可以写 `user` 选项，
配合 `allow_other`，用户态进程崩溃后内核可以按需重挂。但这要求 FUSE
自己实现 `fuse_operations` 的 `.init`/`.destroy` 之外的重连逻辑，
`fuser` **不暴露这个能力**。所以此路不通。

真正轻量的替代是 **M13.0 单独上**（快速重挂载）：
重启 daemon → 挂载点消失 → 3~5 秒后自动回来。
对大多数场景够用，只在「挂载点上有长跑进程」时才会被打断。

**建议：先上 M13.0 止血，再评估是否需要 M13.1~M13.4。**

---

## 7. 落地路线

| 里程碑 | 内容 | 解决什么 | 预估 |
| --- | --- | --- | --- |
| **M13.0** | `mount_state.json` + 启动自恢复 | 重启后自动重挂（3~5s） | 1 天 |
| **M13.1** | `qxync-mountd` crate 骨架 + IPC server | 接口设计跑通，仍单进程 | 2 天 |
| **M13.2** | `RpcLocalView` 实现 16 个方法 | 跨进程打通 | 2 天 |
| **M13.3** | 双 systemd unit + 启停顺序 | **真正解决重启失效** | 1 天 |
| **M13.4** | CLI/GUI 适配 + 崩溃恢复 | 收尾 | 1 天 |

M13.1~M13.4 总计约 6 天，是一次性投入。
之后换NAS 客户端、改同步策略、升级 daemon 都不再影响挂载。

---

## 8. 附：关键代码位置索引

改动时按这个清单对照：

| 关注点 | 文件:行 |
| --- | --- |
| 挂载建立（要搬走） | `qxync-daemon/src/daemon.rs:1578-1800` |
| FUSE spawn（不用改） | `qxync-fuse/src/lib.rs:3709-3721` |
| 挂载卸载（要删） | `qxync-daemon/src/daemon.rs:1914` `shutdown_all_mounts` |
| 挂载表（要搬） | `qxync-daemon/src/daemon.rs:106` `State.mounts` |
| `MountEntry`（要搬） | `qxync-daemon/src/daemon.rs:79-104` |
| 节点表（要搬） | `qxync-fuse/src/lib.rs:303-316` `Inner` |
| 冷路径懒加载（拆分后更关键） | `qxync-fuse/src/lib.rs:2086-2133` `lookup_child` |
| `LocalView`（要实现 RPC 版） | `qxync-fuse/src/lib.rs:645-694` |
| 对账消费 `LocalView`（不改） | `qxync-daemon/src/sync.rs:598-700` `reconcile_view` |
| sid 热更新（要改走 RPC） | `qxync-daemon/src/daemon.rs:1744` → `qxync-fuse/src/lib.rs:780` |
| 脱水决策纯函数（可留在 qxyncd） | `qxync-core/src/dehydrate.rs:190` `plan` |
| 队列 worker（跟着 mountd 走） | `qxync-fuse/src/upload.rs:379`、`delete.rs:255` |
| systemd unit（要加第二个） | `packaging/systemd/qxyncd.service` |
| 恢复开关（默认值要改） | `qxync-daemon/src/main.rs:52-58` |
