# M7 —— 选择性同步 + 设备配对 / LAN 直连

> 状态：**已实现并真机验收**（`xtask/tests/m7-matrix.sh` **60/60**，含真挂载段：
> 排除路径不可见/不可写、临时文件隐藏、LAN 直传命中）。
>
> 一句话：M7 补上「同步范围」与「传输路径」这两块 ——
> **选择性同步**（路径规则引擎，排除的子树在挂载点里根本不存在，同步/水合/脱水都不碰它）与
> **设备配对 / LAN 直连**（同一账号下两台 qxync **自研**对等协议：配对 → 事件快路径 → 局域网直传），
> NAS 始终是**主路径与兜底**（对端不可用 / 元数据不一致 → 回落 NAS，语义与 M1–M6 完全一致）。

---

## 0. 范围、取舍与不做的

《开发规划》给 M7 列了三件事，本章逐项定论（2026-10-01）：

| 项 | M7 决定 | 依据 |
|---|---|---|
| 选择性同步 | ✅ 做：`exclude` 路径规则引擎，贯通 FUSE / 同步引擎 / 脱水 / 上传队列 | 报告 03 §3.7：这是**纯客户端**行为（服务端 `get_list` 照样能看到全部内容） |
| 设备配对（事件快路径） | ✅ 做：qxync 实例之间配对 + 事件推送；收到事件立刻唤醒一轮对账 | M2c 的既定哲学：**事件是快路径，对账是主路径** |
| LAN 直连 | ✅ 做，但**是自家协议**（配对 token + 行 JSON + 裸字节），**不追求与官方 Windows 客户端互通** | 官方通道是 WebSocket 二进制命令 `Auth1`/`Auth2`/`LANDownloadFile`（报告 01 §(d)/03 §3.10），线格式未还原，逆向成本远超收益 |
| 分支 B（服务端算 delta 的上传增量） | ❌ 不做，保持 `DeltaGate` 能力门控 | M5 真机实测：这台 NAS `versioning_support` 全 0、`versioning_stat_delta` 恒 `exist:0` → 服务端不可用（见 `M5-SQLite与delta.md`） |

两条硬约束（整个 M7 不许违反）：

1. **排除 ≠ 删除**：排除只影响「同步/水合/脱水」，本地已有内容与已入队上传**一个字节都不动**（丢数据的风险远大于省流量）。
2. **LAN 只是快路径**：任何一步（配对缺失、对端没水合、元数据不一致、超时）都必须**静默回落 NAS**，结果与不走 LAN 时逐字节一致。

---

## 1. 选择性同步：`exclude` 路径规则引擎

### 1.1 配置

```json
// ~/.config/qxync/links/default.json
{
  "id": "default", "host": "nas.local", "port": 9834, "https": true,
  "user": "test1", "home_root": "/home", "roots": ["/home", "/Public"],
  "exclude": ["/qxync-test/大目录", "*.iso", "!重要.iso", "/Public/迅雷/"],
  "filter_temp": true
}
```

* `exclude`：排除规则（空/缺省 = M1–M6 行为**一字不改**）。
* `filter_temp`：内置临时文件过滤（默认 `true`），对应报告 03 §3.7 的
  `*.crdownload` / `.upload_cache` / `~$*` / `.goutputstream-*`（并补上我们自己的 `*.qxync-part`）。
* `LinkConfig::rules()` 解析成 [`qxync_core::rules::Rules`]；解析失败 **不静默**：
  `qxync rules` 会把坏规则标出来，daemon 启动日志 WARN 并**忽略那一条**（其余照常生效）。

### 1.2 规则语法（gitignore 风味，但只保留够用的子集）

| 写法 | 含义 |
|---|---|
| `# 注释`、空行 | 忽略 |
| `/a/b` | 锚定根相对路径（`/` 开头 = 从根算起） |
| `a/b` | 含 `/` → 从根算起匹配整条相对路径 |
| `*.iso` | 不含 `/` → **任意层级**的文件/目录名匹配（`*` 不跨 `/`） |
| `/cache/` | 尾 `/` = 只匹配目录；匹配上后**整棵子树**一起排除 |
| `!名字` | 反向包含（后出现的规则优先级更高） |
| `**` | 跨层级通配 |

匹配输入是**根相对路径**（`/qxync-test/secret`，相对 `home_root`/`roots` 中命中它的那个根），
不是用户视角的挂载路径（多根时挂载点多一层 `home/`、`Public/`）。原因：baseline、pin、
缓存键、上传队列全部以远端路径为键，规则跟着同一套键走，多根/单根语义才一致。
`qxync rules --match /home/qxync-test/secret` 可以直接在 bash 里验规则。

⚠️ 由此带来一个取舍：**同一条规则对每个根都生效**（`/tailscale.txt` 同时作用于 `/home` 与 `/Public`），
没有「只排除某个共享文件夹」的写法；要彻底不同步某个根，直接从 `roots` 里去掉它（M6 的能力）。
整个根**不能**被规则隐藏（根目录是挂载视图本身）——`qxync rules --match /Public` 会回 `visible`。

### 1.3 生效点（一个都不能漏）

| 层 | 行为 | 位置 |
|---|---|---|
| FUSE `lookup` | 排除路径 → `ENOENT`（**挂载点里根本看不到**，不是「看得到拉不下来」） | `qxync-fuse/src/lib.rs` `lookup_child` |
| FUSE `readdir` | 列目录时直接剔除（临时文件同样剔除） | `load_children` |
| FUSE 写路径 | **所有写入口**都过规则闸门 → `ENOENT`：`open(O_CREAT)` 走的是 `create`（**不经过 `lookup`**），`mkdir`/`write`/`setattr`（截断/改 mtime）/`rename`（两端）/`remove_entry` 同样要挡 | `deny_hidden` + `create`/`mkdir`/`write`/`setattr`/`rename`/`remove_entry` |
| FUSE 水合 | 兜底防御：即使拿到 ino 也拒绝下载（`EACCES`） | `hydrate_all`/`ensure_chunk` |
| 同步对账 | 排除的目录**不列**、候选路径跳过、远端新条目不登记 baseline | `qxync-daemon/src/sync.rs` `reconcile_view` |
| 事件快路径 | 排除路径的事件直接跳过（计入 `events_skipped`） | `apply_event` |
| 脱水 | 排除路径永不作为脱水候选（防止「本地唯一副本被当缓存清掉」） | `FsHandle::dehydrate_candidates` / daemon 候选过滤 |
| 上传队列 | **不过滤**：已入队的上传照旧推回 NAS（硬约束 1：不丢改动） | `qxync-fuse/src/upload.rs` |

> 排除是「没有本地副本」的语义：远端文件仍在 NAS 上，所以脱水排除它**不等于**数据会丢；
> 但反过来，脱水把排除路径的本地内容清掉会让用户意外（他以为自己在本地留着），
> 因此一律不碰 —— 需要空间请自己删。

### 1.4 为什么不是「显示但不下内容」

那是 on-demand（M1 起就有）。选择性同步的语义是**根本不管**：
两种都做成「看得见、打不开」只会让用户分不清「这个文件是占位符还是没被选中」。
所以：**排除 = 挂载点里不存在**，`qxync rules` 负责把「为什么看不到」讲清楚。

---

## 2. 设备配对 / 事件快路径 / LAN 直连

### 2.1 配对模型

```
daemon A（peer_listen=0.0.0.0:9840）        daemon B（peer_listen=0.0.0.0:9840）
   │  qxync peer pair 192.168.1.5:9840 --code 4821
   ├────────── pair(code, name, ver) ──────────►
   │                                            校验 code → 生成 token
   │◄───────── ok(token, identity) ─────────────┤
   双方把 {name, addr, token} 存进 links/<id>.peers.json（0600）
```

* 配对码：daemon 启动/开启监听时随机 6 位数字，`qxync peer status` 显示；
  配对成功后**立即轮换**；同一来源每分钟最多 10 次尝试（超过直接拒绝，不泄露码）。
* token：32 位十六进制（`/dev/urandom`，无该文件时退化为「时间+pid+计数器」的 FNV-1a 混合，
  只用于测试环境）。token 不走 NAS，只在本机 `peers.json`（0600）与对端内存里。
* 配置：`peer_listen`（`"IP:PORT"`；**缺省不监听** —— LAN 服务默认关闭，要显式打开）、
  `peer_name`（缺省用主机名）。

### 2.2 线协议（自研，TCP + 行 JSON + 裸字节）

跟本机 IPC（M1.5）同一套风格，便于用 `nc`/测试脚本手工验证：

```
C→S  {"v":1,"op":"ping"}\n
S→C  {"v":1,"ok":true,"peer":{"name":"qxync-a","version":"0.1.0","addr":"10.0.0.2:9840","roots":["/home"]}}\n

C→S  {"v":1,"op":"pair","code":"4821","name":"qxync-b"}\n
S→C  {"v":1,"ok":true,"token":"…","peer":{"name":"qxync-a","version":"0.1.0","addr":"10.0.0.2:9840","roots":["/home"]}}\n

C→S  {"v":1,"op":"hello","token":"…","name":"qxync-b","addr":"10.0.0.9:9840"}\n
S→C  {"v":1,"ok":true,"peer":{"name":"qxync-a",…}}\n

C→S  {"v":1,"op":"head","token":"…","path":"/home/qxync-test/big.bin"}\n
S→C  {"v":1,"ok":true,"exists":true,"size":1048576,"mtime":1696000000,"hydrated":true}\n

C→S  {"v":1,"op":"get","token":"…","path":"…","offset":0,"len":131072}\n
S→C  {"v":1,"ok":true,"len":131072,"size":1048576,"mtime":1696000000}\n
     <131072 字节裸数据>

C→S  {"v":1,"op":"event","token":"…","event":{"path":"…","size":…,"mtime":…,"ts":…}}\n
S→C  {"v":1,"ok":true,"accepted":true}\n
```

* `ping` 不需要 token（发现/连通性探测）；其余全部要 token。
* **只服务「完整水合 + 未脏 + 无待上传」的文件**：部分水合的文件对端返回 `hydrated:false`，
  调用方直接回落 NAS（绝不能把稀疏文件里的 0 当数据发出去 —— 铁则 1 的 LAN 版）。
* `head` 的 `size`+`mtime` 必须与调用方从 NAS `stat` 拿到的签名一致才接受对端数据；
  一致才算「同一份内容」，不一致（对端改过、还没上传完）→ 回落 NAS。
* 服务端单连接顺序处理（LAN 场景够用），单次 `get` 上限 8 MiB，超出请分片。

### 2.3 事件快路径

```
A 本地写入 → 上传队列落地 → A 组装事件 → 广播给所有已配对 peer
                                              ↓
B 收到事件 → 记入 peer_events（环形，最近 N 条）→ Notify 唤醒轮询线程
                                              ↓
B 立刻跑一轮 poll_once（三游标 + baseline 对账，M2c 原封不动）
```

* 事件内容只是「某路径可能变了」的**提示**，不携带内容、也不改变任何状态 ——
  即使全部丢失，下一轮轮询（默认 5 s）照样能靠对账发现（M2c 的铁则：事件是快路径）。
* 触发点：IPC `put` 成功后立即广播；每轮对账结束后对 `SyncReport.events_out` 里的路径广播。
* 防抖：事件触发的额外对账最多 1 秒一次；收到事件但 NAS 不可达时只记日志，不阻塞。

### 2.4 下载快路径（LAN 直传）

`ensure_chunk()` 在打 NAS 之前先问对端：

```
peers 逐个 head(path) → size/mtime 与本地 node 的远端签名一致 且 hydrated
   → get(offset, len) → 长度校验 → 写缓存（与 NAS 路径共用同一段落盘代码）
   任何一步失败/超时（head 1 s / get 5 s）→ 继续下一个 peer → 都不行 → NAS（原逻辑）
```

* FUSE 只依赖「一组 `PeerConfig` + `qxync_client::peer::fetch_range`」，
  因此**不挂 FUSE 也能测**（单测用 `DirContent` 起真 TCP 对端，见 `qxync-fuse` 的 `m7_lan_*`）。
* 计数：每个挂载的 LAN 命中数/字节会汇总到 `qxync peer status` 的 `lan_hits` / `lan_bytes`。

### 2.5 安全边界（明确写死）

* 默认**不监听**；打开监听是显式配置。
* 明文 TCP（无 TLS）—— 与官方 LAN 通道同级；文档明确「只在可信局域网里开」。
* token 只授权 **已水合文件的读取** 与 **事件提交**；没有任何写/删/改远端的能力。
* 路径越权防护：只允许请求 `roots` 之内的远端路径（`..`、空路径、绝对路径越界一律拒绝）。
* 对端数量上限 32，单连接 4 MiB 读缓冲上限，粘包/超长行直接断开。

---

## 3. 验收

```bash
xtask/tests/m7-matrix.sh              # 全量（单测 + LAN loopback + 真机；有 /dev/fuse 再加 FUSE 段）
xtask/tests/m7-matrix.sh --no-nas     # 不需要 NAS
qxync rules [--json] [--match PATH]   # 规则一览 / 单路径判定
qxync peer status|list|pair|ping|events|notify|fetch
```

矩阵覆盖（**60 项**，本机全绿，含真挂载段）：

1. **规则引擎单测**（core 10 项）：锚定/任意层级/`**`/尾 `/` 目录剪枝/`!` 反向包含/
   内置临时文件与开关/根相对判定/最长根前缀/坏规则不 panic/逗号文件名不误拆；
2. **FUSE 层单测**（5 项）：排除路径 `hide_reason` → `Excluded`、临时文件 → `Temp`、
   `filter_visible` 剔除、**写入口闸门 `deny_hidden` → `ENOENT`**、脱水候选永不含排除路径、
   多根时根目录不被隐藏、默认规则与 M6 一致、**LAN 水合命中与「元数据不符 → 回落 NAS」**；
3. **同步引擎单测**（1 项）：`MountView::hidden` 语义（根目录永不 hidden；默认规则不误伤）；
4. **LAN 协议单测**（client 7 项）：配对码轮换/尝试限流、错 token 拒绝、路径越权(`/etc/passwd`、
   `..`)、`hello` 双向登记、部分水合不可服务、`get` 全量/Range、长度上限、事件送达、
   `fetch_range` 元数据一致才命中；
5. **两个真 daemon（本机 loopback）**：A/B 独立 XDG 与 socket → 各自 `peer_listen`；
   配对码不同、错码被拒、配对成功、A 登记 B、**B 通过 hello 也登记 A**（双向）、
   双向 `ping`、不存在地址 ping 失败（负向）、`peer notify` → **B 的轮询被唤醒**
   （B 轮询间隔 3600 s，polls 0 → 1）、B 事件日志与 `events_in`；
6. **真机（NAS）**：`qxync ls` 仍能看到被排除文件（选择性同步**不动远端**）、
   无挂载时 `peer fetch` 被拒（没有完整水合就不服务）、`qxync put` 上传后 A 自动广播且 B 收到；
7. **FUSE 真挂载段**：被排除文件在挂载点里不可见、**写它失败且不产生上传作业、
   远端内容逐字节不变（`cmp` 基线对比）**、`mkdir` 被排除目录失败且 NAS 上不会凭空出现、
   `*.crdownload` 既不可写也不可见、正常文件可读、B 读同一文件时 **LAN 直传命中**
   （`peer status.lan_hits ≥ 1` + 日志「LAN 直传命中」）；
8. **回归**：`fuse-matrix.sh` **68/68**（M1–M5 的 68 项在 M7 之后全绿；顺手把 M2c-1b 的等待预算
   从 15s 提到 30s 并让 `--direct put` 的失败可见 —— 真机 WAN 下一轮对账要列 ~29 个目录，
   3–7s/轮，15s 会在 NAS 抖动时假失败）。

**实测输出（2026-10-01，`xtask/tests/m7-matrix.sh`）**：

```
== 5. FUSE 真挂载：隐藏 / 写保护 / LAN 直传 ==
  ✅ 被排除文件在挂载点里不可见（1k.bin）
  ✅ 被排除路径 lookup → ENOENT
  ✅ 向被排除路径写失败
  ✅ 被排除路径的写入没有产生上传作业（uploads=0）
  ✅ 远端文件内容原封不动（1024 字节，排除 ≠ 删除）
  ✅ 被排除路径 mkdir 失败
  ✅ NAS 上没有出现被排除目录（写入口挡在客户端）
  ✅ 临时文件路径（*.crdownload）写失败
  ✅ B 的这次水合走了 LAN 直传（lan_hits=1）

== 汇总 ==
  通过 60 / 失败 0
  🎉 M7 验收矩阵全过
```

## 4. 已知限制

* **与官方 Windows 客户端不互通**：官方 LAN 通道是未还原的 WebSocket 二进制协议；
  我们的协议只服务 qxync ↔ qxync。同一台机器上装官方客户端不会互相干扰（端口默认不开）。
* **规则改动要重启 daemon**（规则在挂载/同步引擎构造时注入）。`qxync rules` 能看到当前生效值。
* **LAN 直传只在「对端已完整水合且未修改」时命中**：冷启动后第一份内容仍要各自从 NAS 拿
  （没有做「对端代拉」，那会把 NAS 流量变成 LAN 流量但延迟更差）。
* **没有内容哈希**：用「大小 + mtime 与 NAS `stat` 一致」做一致性判据（与 M2c 的 baseline 同源）。
  恶意对端仍可投毒 —— 因此 token 与「只在可信局域网」是前提。
* 多网卡/广播发现不做：对端地址在配对时写死（`qxync peer pair <addr>`），
  没有 UDP 广播自动发现（报告 03 §3.10 的 `UpdateUDPInfo` 不在 M7 范围）。

## 5. 踩到的坑

0. **`open(O_CREAT)` 不经过 `lookup` —— 这是本里程碑唯一一个「会毁数据」的 bug**。
   第一版只在 `lookup_child`/`load_children` 上过滤，真挂载段一跑就发现：
   `printf x > mnt/qxync-test/1k.bin`（被排除的文件）**居然成功**，而且因为 `create`
   先把节点建成 0 字节、`mark_dirty` 直接入队上传 → **NAS 上那个 1024 字节的文件被覆盖成 0 字节**。
   修复：写入口统一过 `deny_hidden()` → `ENOENT`（`create`/`mkdir`/`write`/`setattr`/
   `rename` 两端/`remove_entry`），并在矩阵里加了三条硬断言（写失败 + `uploads=0` +
   **下载前后 `cmp` 远端内容不变**）。教训：**「读路径挡住了」不等于「写路径挡住了」**，
   内核的 atomic_open 会绕过 lookup，必须逐个写入口过闸门。
0b. **回调跑在哪个线程上，决定你能不能 `tokio::spawn`**。事件快路径最初在「上传成功回调」里
   `tokio::spawn` 一个广播任务 —— 那个回调跑在**上传 worker 的 OS 线程**里（不是 tokio worker），
   于是 spawn 直接 panic，把 worker 打死：表现是「上传队列永远卡住、`drain` 超时、
   `qxync status` 一直显示上传中」，整个 daemon 像挂了（fuse-matrix 的冲突段卡了 3 分钟才被发现）。
   修复两层：① 事件广播改成**同步 send + 常驻 task 消费**（发送端不需要 runtime 上下文）；
   ② 回调调用点用 `catch_unwind` 兜住 —— 回调只能坏它自己，队列必须继续跑。
   教训：**任何会从 FUSE/上传线程调用的代码，都不能假设自己在 tokio 上下文里**。

1. **「排除」的命名空间只能选一个**：一开始想在挂载视图上匹配（`home/qxync-test/x`），
   但 baseline/pin/缓存键/上传队列全用远端路径，且多根时视图名是布局推导出来的 ——
   最后锚定「根相对路径」，并在文档里把「规则对所有根生效、整个根靠 root 列表控制」写死。
2. **配对必须是双向的**：只做 A→B 的 `pair` 只能让 A 拉 B，B 推不了事件给 A。
   加了 `hello`（token 认证）让 A 把自己的监听地址交给 B，一次配对建立双向信任；
   没开监听时明确回 ⚠️「单向配对」，不假装成功。
3. **测试共享临时目录会互相删**：`qxync-client` 的 loopback 测试并行跑，两个测试用同一个
   `tmpdir("src")` 导致 `head` 时而 `exists:false`（另一个测试把目录删了）。改成目录名带原子序号。
4. **GUI 保存会抹掉规则**：M4 的 `login_flow` 用表单重建 link JSON。M7 给 GUI 的输入结构补了
   `exclude/filter_temp/peer_listen/peer_name` 四个字段并在保存时**从已有配置补齐**，
   否则「GUI 里点一下保存」就会把选择性同步规则清空。
5. **`--lib` 对二进制 crate 无效**：`qxync-daemon` 的单测在 `src/main.rs` 里，
   矩阵里必须用 `cargo test -p qxync-daemon --bins`（M6 矩阵没踩到，因为只跑 core/fuse/client）。
6. **`qxync sync` 原本没有 `--json`**：验收要断言「事件唤醒后轮询计数 +1」，
   读人读文本太脆，顺手给 `qxync sync` 补了 `--json`（和 roots/store/rules 一致）。
