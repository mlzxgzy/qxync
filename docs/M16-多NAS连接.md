# M16：多 NAS 连接

> **状态**：已实现，随 **v0.6.0** 发布。本文是给后续开发者的设计说明：
> 为什么是「一个连接一个 daemon」、数据落在哪、旧数据怎么迁、哪些边界不能碰。
>
> 涉及的三层：`qxync-core`（路径 / 数据模型 / 命名空间）、`qxyncd`（进程与迁移）、
> `qxync` CLI 与 `qxync-gui`（入口）。**default 连接的行为与路径保持不变**，
> 这是整个 M16 的硬约束。

---

## 1. 目标与产品行为

一句话：**同时连多台 NAS，每台 NAS 下挂多个任务。**

| 概念 | 含义 |
|---|---|
| **连接（link）** | 一台 NAS + 一个账号 = `host` / `port` / `https` / `user`（+ 选择性同步、LAN 对等配置）。配置在 `~/.config/qxync/links/<id>.json` |
| **任务（task）** | 一个**本地文件夹 ⇄ 一个 NAS 文件夹**（一对一）。任务登记在 `~/.config/qxync/tasks/<id>.json`，可带 `link_id` 归属 |
| **默认连接** | `id = "default"`。旧版本唯一存在的那条连接，**升级后路径、socket、pid、日志、凭据全部不变** |

于是产品形态是：

```
NAS-A (default)  ── qxyncd --link default ── 任务 t1、t2、t3
NAS-B (nas2)     ── qxyncd --link nas2    ── 任务 t4、t5
NAS-C (work)     ── qxyncd --link work    ── 任务 t6
```

* 每条连接**独立**的进程、socket、pid、日志、状态库、缓存、上传队列；
* 一个连接里的任务互不影响，**停掉 A 的 daemon 不会动 B 的任务和挂载点**；
* 连接 id 只允许 `A-Z a-z 0-9 . _ -`（≤ 64 字符，不能是 `.` / `..`）——它同时是
  文件名与 socket 名，必须能安全地当路径组件（`LinkConfig::is_valid_id`）。

---

## 2. 架构：一连接一 daemon 进程

### 2.1 为什么不是「单 daemon 多 link」

早期草案考虑过单进程管全部连接。否决理由（都是真机上会咬人的）：

1. **FUSE 挂载与 NAS 会话是绑定在一个进程里的**：换 NAS / 换账号要重建客户端与全部
   会话状态。单进程里「只重启一条连接」做不到，只能全量重启——另一条连接上正传的
   文件会被打断。
2. **故障隔离**：一条连接登录失败 / NAS 掉线 / 进程崩，不应该让所有挂载点一起消失。
3. **迁移与兼容更简单**：default 连接仍旧是「旧的那个 qxyncd」，不需要在进程内部
   给旧代码加一层多路复用。
4. **代价可控**：每连接多一个常驻进程（几 MB RSS），相比上面三条可以接受。

### 2.2 进程与路径派生

qxyncd 启动参数里带 `--link <id>`（缺省 `default`），由此派生全部运行期路径：

| 东西 | default（旧路径，不变） | 非 default |
|---|---|---|
| IPC socket | `$XDG_RUNTIME_DIR/qxync/qxyncd.sock`（无 `XDG_RUNTIME_DIR` 时退到 `<state>/qxyncd.sock`） | 同目录 `qxyncd-<id>.sock` |
| pid 文件 | `<state>/qxyncd.pid` | `<state>/qxyncd-<id>.pid` |
| 滚动日志 | `<state>/log/qxyncd.log`（按天滚动） | `<state>/log/qxyncd-<id>.log` |
| 配置文件 | `links/default.json` | `links/<id>.json` |

细节：

* **socket / pid / 日志的名字** 走 `qxync_core::ipc::{socket_path_for, pid_path_for,
  log_prefix_for}`，id 先经 `sanitize_component` 再拼进文件名。
* **显式 `--socket`（CLI 的环境变量是 `QXNYC_SOCKET`）是「整套覆盖」**：CLI 解析完
  参数后，给了 `--socket` 就用它、不再看 `--link` 的派生结果（对非 default 连接也一样）。
  这是测试 / 多实例的逃生口，**多连接日常使用不要设** —— 设了 `--link nas2` 也会
  连到 default 的 socket 上。GUI 不读这个环境变量，一律按连接派生。
* **`qxync daemon --all` 与显式 `--socket` 互斥**：每个连接有自己的 socket，
  一个显式路径没有意义（CLI 会直接报错，而不是猜）。
* 日志前缀按连接分文件，`journalctl --user -u qxyncd@<id>` 与磁盘日志都对得上。

### 2.3 启动与停止

```bash
# default（旧命令，行为不变）
qxync daemon start
qxync daemon status

# 非 default：全局 --link 决定路由
qxync --link nas2 daemon start
qxync --link nas2 daemon status
qxync --link nas2 daemon reload

# 全部连接（逐条串行，失败汇总）
qxync daemon start --all
qxync daemon stop --all
qxync daemon status --all
qxync daemon reload --all
```

`daemon start --all` 只遍历**配置里存在的连接**（`LinkConfig::list`），坏文件跳过并
打警告；任意一条失败最后以 `failed/total` 汇总返回非 0，不静默。

> 没有连接配置时 qxyncd 依旧「空转待命」：它只服务 ping/status/shutdown，配置一出现
> 就自己转入同步。所以「先起 daemon、后配 NAS」仍然成立（旧行为未变）。

---

## 3. 存储布局

### 3.1 总表

| 数据 | default | 非 default（`ns = link_namespace(link)`） |
|---|---|---|
| 连接 | `~/.config/qxync/links/default.json` | `~/.config/qxync/links/<id>.json` |
| 凭据（0600） | `~/.config/qxync/credentials.json`（旧路径） | `~/.config/qxync/links/<id>.credentials.json` |
| LAN 对等设备 | `~/.config/qxync/links/default.peers.json` | `~/.config/qxync/links/<id>.peers.json` |
| 任务 | `~/.config/qxync/tasks/<task>.json`（全局一个目录，靠 `link_id` 归属） | 同左 |
| 挂载现场 | `~/.local/state/qxync/qxync/mounts.json` | `~/.local/state/qxync/qxync/mounts/<ns>.json` |
| 同步状态库 | `~/.local/share/qxync/sync/<host>/sync.db` | `~/.local/share/qxync/sync/<ns>/sync.db` |
| 水合缓存 | `~/.local/share/qxync/cache/<host>/` | `~/.local/share/qxync/cache/<ns>/` |
| 上传/删除队列 | `~/.local/share/qxync/upload-queue/<host>/`（M16 起） | `~/.local/share/qxync/upload-queue/<ns>/` |
| socket | 见 §2.2 | 见 §2.2 |
| pid | 见 §2.2 | 见 §2.2 |
| 日志 | `<state>/log/qxyncd.log` | `<state>/log/qxyncd-<id>.log` |

> 表中 `<state>` 指 `ConfigPaths::state_dir` = `~/.local/state/qxync`（`$XDG_STATE_HOME`
> 是 `~/.local/state` 时）；`<data>`/`data/` 指 `~/.local/share/qxync`（`$XDG_DATA_HOME`）。
> 挂载现场那一行的 `state/qxync/mounts.json` 完整写出来是
> `~/.local/state/qxync/qxync/mounts.json`（多一层 `qxync`，见 §3.3 的警告）。

> 「default 的 sync/cache 目录名就是 `sanitize(host)`」不是巧合：这**正是旧版本**
> `sync/<host>` / `cache/<host>` 的目录名，所以升级后 default 的数据原地可读，
> 零迁移（§5）。

### 3.2 命名空间：`sanitize_component` 与 `link_namespace`

```rust
sanitize_component(s): 只保留 A-Z a-z 0-9 . _ -，其余替换为 '_'；
                       空 → "_"；"." / ".." → 加前缀 '_'
link_namespace(link):  default      → sanitize(host)
                       非 default   → "#<sanitize(id)>@<sanitize(host)>"
```

* `sanitize_component` **永远不会**产出 `#` 或 `@` —— 这两个字符专门用来把
  「多连接命名空间」与「旧 host 命名空间」区分开。看到目录名以 `#` 开头，
  一定是 M16 之后的非 default 连接。
* 带上 `id` 而不是只有 host：同一台 NAS 上的两个账号（比如个人号 + 工作号）
  各自有独立缓存与 baseline，不会互相踩。
* 含 `#` / `@` 的目录名对用户不友好，但它是**内部布局**；GUI / CLI 展示用
  `id / host / user`，不用目录名。

### 3.3 任务与挂载记录

* **任务**仍然是**全局一个目录**（`tasks/<id>.json`），因为同一个任务可能被多个
  进程看到；归属靠字段 `link_id` 判断，不靠目录（§4）。
* **挂载现场**按连接分文件：`mounts.json` 的语义是「上一次实际挂了什么」，
  多个 daemon 各写各的，不再有「全量重写把别人的记录抹掉」的竞态。
  `MountsFile::{file_for, load_for, save_for}` 是唯一入口；`mounts_persist` 还会
  把「仍挂着、但不由本进程托管」的记录带上（与 M15/T6 的挂载守护语义一致）。
  > ⚠️ 路径里的 `qxync/qxync` 不是笔误：`ConfigPaths::state_dir` 已经是
  > `~/.local/state/qxync`，而 `MountsFile::file` 为了「与 log 同层」又拼了一层
  > `qxync`。**改代码/脚本请一律走 `mounts_file_for()`，不要手拼路径** ——
  > 手拼少一层就会「save 成功但 load 读不到」，静默失效。

---

## 4. 数据模型

### 4.1 `Task.link_id` 可选

```rust
pub struct Task {
    // ... 既有字段 ...
    /// 这条任务属于哪个 NAS 连接（links/<id>.json 的 id）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_id: Option<String>,
}
```

* 序列化时 `None` **不落盘**，旧任务 JSON 读进来仍是 `None` → 文件格式向后兼容。
* 非空时必须能安全地当文件路径组件（`Task::normalize` 会拒绝非法 id）。
* daemon 保存任务时**一律写成显式的 `Some(link.id)`**：归属由 daemon 说了算，
  客户端传什么都覆盖掉。任务从挂载点推导出来时（`Task::from_mount`）不知道
  调用方是哪个连接，落盘前由 daemon 补上。

### 4.2 旧任务的归属规则

旧版本的任务文件没有 `link_id`。它们归谁？规则必须**确定**——两个 daemon 同时启动，
不能让它们都认为旧数据归自己：

1. 显式写了 `link_id` → 就用它（哪怕对应连接文件暂时不在，也不能被别的 daemon 抢走）；
2. 没有 `link_id` → **legacy owner**：
   - 有 `default` 连接 → `default`；
   - 没有 `default` → **字典序最小**的连接 id。

对应实现：

| 函数 | 作用 |
|---|---|
| `Task::effective_link_id(&all_ids)` | 按上面规则算出这条任务实际归谁 |
| `Task::belongs_to(link_id, &all_ids)` | 当前 daemon 该不该管这条任务 |
| `Task::list_for_link(paths, link_id, &all_ids)` | 只列本连接的任务（daemon `tasks list` 用） |
| `LinkConfig::legacy_owner(&ids)` | legacy owner 规则本身（default 优先 → 字典序最小） |

### 4.3 位置冲突检查

任务提交时（daemon `tasks save`）：

| 冲突 | 范围 | 处理 |
|---|---|---|
| **本地挂载点**重复 / 嵌套 | **跨连接**（所有任务） | **直接拒绝**（`error`）：两个进程往同一个本地目录挂东西，内核里只会剩一个，另一个静默失效 |
| **NAS 文件夹**重叠 | **只同一连接内** | 仅 `warning` 回给界面：不同 NAS 上的 `/home` 不是同一个目录，跨连接比较没有意义；同一连接内只读挂同一个目录是合法用法 |

---

## 5. 迁移

原则：**default 零迁移；非 default 只在它是 legacy owner 时迁移旧数据。**

### 5.1 default：路径一字不动，只迁上传队列

default 的 link / 凭据 / peers / mounts / sync / cache / socket / pid / 日志**保持旧路径**，
升级后直接可用。唯一变化的是上传队列目录：

```
data/upload-queue/queue.db          （旧：全局一个队列）
data/upload-queue/<host>/queue.db   （新：per-link；default 的 ns = sanitize(host)）
```

首次启动时 `migrate_legacy_queue` 把 `queue.db` / `queue.db-wal` / `queue.db-shm` /
`*.dirty` 移进新目录；目标已有 `queue.db` 就跳过（幂等）。**迁移失败拒绝启动**：
队列里是「已落盘但还没推回 NAS 的改动」，静默换一个空队列等于丢改动。

> 其他连接（非 legacy owner）启动时**不会**碰旧全局队列，避免两个进程抢同一批脏标记。

### 5.2 非 default：legacy owner 迁移五项

只有 legacy owner 会迁：`sync` / `cache` / `upload-queue` / `mounts` / `credentials`。

| 数据 | 源（旧） | 目标（新） | 失败策略 |
|---|---|---|---|
| 上传队列 | `data/upload-queue/{queue.db,*.dirty}` | `data/upload-queue/<ns>/` | **拒绝启动**（丢未上传改动） |
| 同步状态 | `data/sync/<sanitize(host)>/` | `data/sync/<ns>/` | **拒绝启动**（空 baseline 重新对账可能误删） |
| 水合缓存 | `data/cache/<sanitize(host)>/` | `data/cache/<ns>/` | **只告警**（可重新下载，不影响正确性） |
| 挂载记录 | `<state>/qxync/mounts.json` | `<state>/qxync/mounts/<ns>.json` | **只告警**（复制，旧文件保留） |
| 凭据 | `credentials.json` | `links/<id>.credentials.json` | **只告警**（host/user 不匹配就不复制） |

补充语义：

* `sync` / `cache` 迁移是 **move**（同分区 rename，跨分区退回复制后删源）；
* `mounts` 是 **copy**，旧 `mounts.json` 保留 —— default 可能还要继续用它；
* `credentials` 只复制 host/user 与连接匹配的那份（`Credentials::load_for` 会校验）；
* 目标已存在就跳过并 WARN（「新目录与旧目录同时存在，按新目录继续，旧目录保留」）。
* 迁移在 daemon 启动早期执行一次，**幂等**，失败信息写日志并（对 sync/queue）直接退出。

### 5.3 旧版本回退

非 default 连接的数据在旧版本里**不存在对应路径**（旧版本只认 default）。
回退旧版本 = 非 default 连接失效（default 不受影响）。见 §10。

---

## 6. CLI

### 6.1 `qxync links [--json]`

只读汇总，**不触发登录**：

```bash
$ qxync links
- default    alice@nas-a:9834  daemon=运行  login=已登录  credential=有  socket=/run/user/1000/qxync/qxyncd.sock
- nas2       alice@nas-b:9834  daemon=停止  login=未登录  credential=有
```

（`id` 左对齐 10 列；socket 只在 daemon 运行时显示；坏连接文件以 `⚠️ …` 打到 stderr。）

`--json` 输出（脚本 / 验收用）：

```json
{
  "links": [{
    "id": "nas2", "host": "nas-b", "port": 9834,
    "https": true, "insecure": false, "user": "alice",
    "socket": "/run/user/1000/qxync/qxyncd-nas2.sock",
    "credential_present": true,
    "daemon": {"running": true, "pid": 2345, "logged_in": true, "error": null}
  }],
  "bad": [{"path": "...", "error": "..."}]
}
```

* 坏连接文件进 `bad[]`，**不整体失败**（与 `Task::list` 同一风格）；
* `credential_present` 只判断凭据文件在不在（`Credentials::present_for`），不读口令；
* `daemon.logged_in` 来自 daemon 的 `Status`，daemon 没跑就是 `false`。

### 6.2 全局 `--link`

```bash
qxync --link nas2 ls /home
qxync --link nas2 task list
qxync --link nas2 mount /tmp/nas2-home --remote /home --rw
qxync --link nas2 sync --once
```

`--link` 是**全局参数**：所有需要「连接」的子命令都走它，包含直连路径（不走 daemon）
时的凭据选择。缺省 `default`，所以旧命令一字不改。

### 6.3 `daemon` 生命周期

`qxync daemon start|stop|status|reload [--all]`：

* 不带 `--all`：只作用于 `--link` 指定的那一条连接（`daemon_action_one`）；
* 带 `--all`：遍历 `LinkConfig::list`，串行处理，逐条打印，最后汇总失败数；
* `--all` 与显式 `--socket` 互斥（`--all` 的每个连接有自己的 socket）；
* `reload` 走 SIGHUP（M15/T6 语义）：只重载同步引擎，**不中断 FUSE 挂载**；
* `start` 不检查是否有 link 配置（空转待命是合法状态）。

---

## 7. GUI

> GUI 侧的连接管理全部经 daemon 的 IPC / `commands.rs`，前端不直接读写配置文件。

| 界面 | 行为 |
|---|---|
| **设置 → 连接** | 列出全部连接（`links_status`），每条显示 host/user/daemon 状态/是否已登录；可新增（自动生成 `nas2`、`nas3`… id）、编辑、移除。新增连接另有顶栏 / 主页 / 任务表单三处快捷入口，见 §7.1 |
| **当前连接选择器** | 顶栏 / 任务页选择一个「当前连接」；之后任务、文件、诊断等页默认作用在该连接上（`ipc_call(linkId, …)` 路由） |
| **任务列表** | **聚合**显示所有连接的任务，每条带连接 id 角标；操作（暂停 / 继续 / 删除 / 挂载）按该任务的 `linkId` 路由到对应 daemon |
| **文件页** | 跟随当前连接：列目录、下载、脱水、pin 都发到该连接的 daemon；切换连接即切换视图 |
| **设置广播** | 全局设置（代理 / 自动释放空间 / 冲突策略 / 文件三态 / 通知）保存后**广播到所有连接**，避免「改了设置只有一半 daemon 生效」 |
| **托盘 / 连接列表** | 托盘菜单：打开主窗口 / 立即同步 / 暂停；「立即同步 / 暂停」遍历**所有运行中的连接**；「启动全部 / 停止全部」在「设置 → 连接」的连接列表里（`daemon_start_all` / `daemon_stop_all`） |
| **空转 daemon** | 没有 link 文件、但 default socket 在跑时，`links_status` 会补一条 `link_exists=false` 的条目，让用户至少能把它停掉 |

GUI 后端命令（`lib.rs` 注册）：

| 命令 | 作用 |
|---|---|
| `links_status` | 全部连接 + 各自 daemon / 凭据状态（前端每 2s 轮询；并发探测，不串行拖慢） |
| `link_save` / `login_flow` | 写连接 + 写凭据 +（参数变了才）重启该连接 daemon + 登录 |
| `link_delete` | 移除连接：有本连接任务时先返回 `needs_confirm`（此阶段不停 daemon、不删任何东西）；用户确认后 `delete_tasks=true` 才停 daemon → 删任务登记 → 删 link / 凭据 / peers |
| `daemon_start_all` / `daemon_stop_all` | 遍历配置里的连接逐条起停 |
| `tasks_config` | daemon 没在跑时直接从磁盘读该连接的任务登记（主页/任务页仍能看到「有哪些任务，但当前不会同步」） |
| `ipc_call` / `daemon_status` | 按 `linkId` 路由的通用 IPC 与状态 |

> ⚠ **同一连接不要同时由 GUI 和 systemd 管**：GUI 起停走的是 socket/IPC，systemd 起停
> 走的是它自己拉起的进程。两边同时管同一条连接会互相打架（与旧版 qxyncd.service
> 文档里的警告同一条道理）。

### 7.1 「新增连接」的三个入口（v0.6.0 之后补的）

v0.6.0 里新建连接只有「设置 → 连接」卡片右上角那一个「＋ 添加 NAS」（`#btn-link-add`）。
主页连接卡片、顶栏「当前连接」下拉旁边、任务表单的「NAS 连接」下拉**都没有出口**，
而任务表单的下拉只列**已有**连接 —— 于是「想加一台还没配的 NAS」这个动作在界面上
没有落点：用户会盯着「连接」找，哪里都找不到（v0.6.0 发布后的真实反馈）。

现在三个入口全部收敛到同一个函数 `openNewLink()`（`crates/qxync-gui/ui/app.js`）：
`switchPage('settings', null, 'connect')` → 清空表单 → 把「连接配置」卡片
`scrollIntoView` → 聚焦「主机 / NAS 地址」（`#f-host`）→ 留一行日志。

| 入口 | 位置 | 触发 |
|---|---|---|
| 顶栏「＋」 | 「当前连接」下拉右侧（`#btn-top-add-link`，样式 `.conn-add`） | 点击 → `openNewLink()` |
| 主页「＋ 添加连接」 | 主页连接卡片操作行（`#btn-home-add-link`） | 点击 → `openNewLink()` |
| 任务表单「＋ 新建连接…」 | 新建任务的「NAS 连接」下拉末项（`#t-link`） | 选中伪选项 `__new__` → 下拉先回退 → `openNewLink()` + 提示「配好后回任务页继续」 |

三条实现约束（都是踩出来的，改之前先读）：

1. **`state.editingLinkId === ''` 有两种含义**：「展示当前连接」和「正在新建」。
   v0.6.0 只凭它判断，于是点「＋ 添加 NAS」清空表单后，`loadConnect()` 的异步回包
   （读的是**当前连接**）会把上一台 NAS 的 host / path 填回表单 —— 标题写着
   「添加 NAS 连接」，地址栏却是别的连接，保存下去就是一条错的连接。现在用
   `state.creatingLink` 区分：新建态下这次回包只写「尚无连接配置」提示，不碰表单；
   编辑 / 保存 / 登录成功 / 移除 / 取消编辑都会把它复位。
2. **`__new__` 是伪选项，不是连接 id**：选中后必须先 `renderTaskLinkSelect()` 回退，
   否则 `state.taskFormLink` 会停在一个不存在的 id 上（下一次渲染取不到名字）。
   编辑态（`taskFormEditing === true`）下拉是只读的，不挂这一项。
3. **主页「连接设置」固定落 `settings:connect` 分区**：它原来是 `switchPage('settings')`，
   会沿用「上次看过的设置分区」；上次停在「关于」时点它就落到关于页，连接列表
   根本不出现。与 `settings:<分区>` 直达（M8.4）保持一致。

**没有**做成「任务表单里内联建连接」：任务保存要求连接已存在（保存 → 登录 → 建任务
三步），内联会把 `login_flow`（写配置 + 重启该连接 daemon + 登录）的副作用塞进任务
表单的错误处理里。所以入口只管「跳过去建」，日志里明说回来继续 —— 不接管任务表单
那半条流程。

---

## 8. 安全边界

1. **移除连接只删登记，不删数据。**
   `link_delete` 的顺序是：有任务则先返回 `needs_confirm`（不停 daemon、不删东西）
   → 用户显式 `delete_tasks=true` 后停 daemon（真卸载该连接挂载点）→ 删任务登记
   → 删 `link` / 凭据 / peers。
   **本地文件、NAS 数据、cache、状态库（sync.db / baseline）一律不删。**
   底层 `LinkConfig::delete` 只做最保守的配置清理，绝不替用户删业务数据。
2. **同一 NAS + 账号只保留一个连接**：`(host, port, user)` 重复（host 大小写不敏感）
   时拒绝新建/改成重复，并指出已有连接 id。
3. **跨连接任务 id 拒绝覆盖**：同 id 任务已属于别的连接时，`tasks save` 直接报错
   （「任务 id 已经属于连接 X，不能从连接 Y 覆盖；换个 id」）。两个 NAS 都用 `t1`
   是很自然的误操作，静默覆盖会直接换掉另一条任务的远端根。
   （M18 起 GUI 新建任务的默认 id 取 NAS 文件夹名并自动去重，从源头就不会两个连接都用
   同一个 id；见 [`M18-任务默认路径.md`](M18-任务默认路径.md)。）
4. **本地挂载点冲突跨连接直接拒**（§4.3）；卸载 / 挂载记录写盘只动本连接的文件，
   且「路径相同但挂载者是别人的东西」绝不覆盖、绝不卸载（M15/T7 的边界）。
5. **连接 id 做路径组件校验**：`is_valid_id` 挡路径穿越；`sanitize_component` 再兜一层。
6. **凭据按连接分文件**，A 的口令不会被拿去登 B。非 default 连接在
   `links/<id>.credentials.json` 不存在时**允许回落到旧全局 `credentials.json`**
   （迁移期的便利），但必须 `host + user` 完全匹配（`Credentials::same_account`），
   不匹配就报错 —— 绝不「既然只有一份就拿来用」。

---

## 9. 验收

| # | 验收项 | 判据 |
|---|---|---|
| 1 | **升级零迁移** | 旧版数据目录原样启动 default：路径不变、能登录、任务照常同步；旧全局 upload-queue 只发生一次迁移（幂等） |
| 2 | **两 daemon 隔离** | `default` 与 `nas2` 各自 socket/pid/日志/状态库；停 A 不影响 B 的挂载点与任务 |
| 3 | **任务隔离** | 每个 daemon 的 `tasks list` 只列本连接任务；两条连接各建同 id 任务时后者被拒 |
| 4 | **文件页切换** | GUI 切「当前连接」后，文件/任务/诊断请求打到对应 daemon |
| 5 | **停 A 不影响 B** | `qxync --link default daemon stop` 后 `nas2` 仍 running、挂载点仍在 |
| 6 | **CLI `links`** | `qxync links` / `--json` 如实列出全部连接与各自 daemon 状态；坏文件进 `bad[]` |
| 7 | **`--self-test-multi`** | `qxync-gui --self-test-multi`：隔离 XDG 目录 + 两个假连接，证 `links_status` 列出两条、`daemon_start_all` 起两个独立 socket、任务按连接隔离且跨连接同 id 覆盖被拒、`daemon_stop_all` 收干净（全部退出码 0） |
| 8 | **加连接入口（§7.1）** | 顶栏「＋」/ 主页「＋ 添加连接」/ 任务表单「＋ 新建连接…」三处都落到「设置 → 连接」的**空表单**：地址栏为空、路径提示是「新建连接」、焦点在「主机 / NAS 地址」；新建态下当前连接的异步回包不把 host/path 填进来。静态侧 `ui_spec.ok` 仍为 true（`cargo test -p qxync-gui --lib` 全过） |

回归底线（每批改动都要跑）：

```bash
cargo test -p qxync-core -p qxyncd
cargo build -p qxync -p qxync-gui
qxync-gui --self-test-multi
# 旧行为：default 的 --self-test / --self-test-login 仍全绿
```

---

## 10. 已知限制

* **同一台 NAS 上的两条连接，不要同时双向同步同一个远端目录**：两条连接各有独立
  baseline / 游标 / 上传队列，同一棵树被两个进程对账会互相判成对方的改动，
  冲突副本与误判删除都可能出现。需要多账号时，请让远端根不重叠。
* **非 default 连接回退旧版本会失效**：旧版本只认 `default`，看不到
  `links/<id>.json`，也不会派生 `#<id>@<host>` 命名空间。回退前请把非 default
  任务停掉（default 不受影响）。
* **连接 id 创建后不可改名**：id 同时是文件名、socket 名与数据命名空间的一部分。
  改名 = 换一个新连接（数据不会自动跟过去）；需要时请「新建 + 迁移任务」，不要手工
  重命名文件。
* **每连接一个进程**：连接数 × 常驻内存；挂载点很多时建议只对真正需要的连接起 daemon。
* **Windows 客户端风格的多 NAS 列表未做**：GUI 只做本地连接管理，不写 NAS 设备列表。

---

## 11. 关键代码位置

| 位置 | 内容 |
|---|---|
| `crates/qxync-core/src/config.rs` | `LinkConfig::is_valid_id` / `list` / `delete` / `legacy_owner`；`sanitize_component`、`link_namespace`；`ConfigPaths::link_credentials_file` / `mounts_file_for` / `sync_dir_for` / `cache_dir_for` / `queue_dir_for` / `legacy_queue_dir`；`Credentials::{load_for, save_for, present_for}` |
| `crates/qxync-core/src/ipc.rs` | `socket_path_for` / `pid_path_for` / `log_prefix_for`（default 保持旧路径，其余按 id 派生） |
| `crates/qxync-core/src/tasks.rs` | `Task::link_id`、`effective_link_id` / `belongs_to` / `list_for_link` / `has_any_task_for_link`、`conflict_report`（本地挂载点 vs NAS 根覆盖） |
| `crates/qxync-core/src/mounts.rs` | `MountsFile::{file_for, load_for, save_for}`（per-link 挂载记录） |
| `crates/qxync-daemon/src/daemon.rs` | `all_link_ids`、`migrate_legacy_data`（`migrate_legacy_{queue,sync,cache,mounts,credentials}`）、`tasks_cmd`（归属强制 / 跨连接拒绝覆盖 / 冲突检查）、`mounts_persist`、`restore_mounts_on_start` / `restore_tasks_on_start` |
| `crates/qxync-daemon/src/main.rs` | `--link` 参数、按连接派生 socket/pid/日志、id 合法性校验 |
| `crates/qxync-cli/src/main.rs` | `links_cmd`（`qxync links [--json]`）、`daemon_cmd`（`--all` 汇总）、`daemon_action_one`（单连接起停/状态/reload）、全局 `--link` |
| `crates/qxync-gui/src/commands.rs` | `links_status`、`tasks_config`、`link_delete`、`daemon_start_all` / `daemon_stop_all`、`link_save` / `login_flow`（按 id 写连接与凭据）、`duplicate_link`、`next_link_id`、`ipc_call(linkId, …)` |
| `crates/qxync-gui/src/lib.rs` | `self_test_multi` / `self_test_multi_inner`（`--self-test-multi`） |
| `crates/qxync-gui/src/tray.rs` | 托盘菜单（打开 / 立即同步 / 暂停；同步/暂停遍历所有运行中的连接，动作由前端 fanout） |
| `packaging/systemd/qxyncd@.service` | 多连接用的 user 单元模板（`--link %i`） |
