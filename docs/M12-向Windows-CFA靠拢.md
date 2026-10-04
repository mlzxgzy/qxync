# M12：向 Windows CFA（Cloud Files API）设计靠拢

> **【已收敛为 `M15-待办任务清单.md`】**
> 本文是背景调研档案，**不是执行入口**。要做什么看 M15。
> 其中「不做」的结论已整理进 M15 §0.3，不要再捡回来。

> 参考：[云同步引擎](https://learn.microsoft.com/zh-cn/windows/win32/cfapi/build-a-cloud-file-sync-engine)、
> [云筛选器参考](https://learn.microsoft.com/zh-cn/windows/win32/cfapi/cloud-filter-reference)、
> [CloudMirror 示例](https://github.com/Microsoft/Windows-classic-samples/tree/master/Samples/CloudMirror)。
>
> 本文只做「**设计借鉴**」，不做 Windows 移植 —— 见 §6 为什么大部分 CFA 机制不能照搬。
> 结论先行：**CFA 值得学的不是 API，而是三样东西 —— 稳定文件身份、显式状态机、可中断的拉取协议。**

---

## 0. 结论先行

当前 qxync 已经有「按需水合 + 区间位图 + 三向合并」的骨架，架构方向和 CFA 是一致的
（`crates/qxync-fuse/src/lib.rs:48` 明确写了「与 Qsync 的 CfAPI `FETCH_DATA` 对齐」）。
但和三向合并配套的地基缺了三块，缺一块就补一块：

| 优先级 | 缺什么 | 对应 CFA 概念 | 真实风险 |
| --- | --- | --- | --- |
| **P0** | 稳定文件身份 | `CF_IDENTITY` / `CF_FILE_INFO` | 改名/移动即断链，三向合并退化为路径猜测 |
| **P0** | 内容校验 | `CF_HYDRATION_POLICY_VALIDATION_REQUIRED` | 静默数据损坏，且**无法被发现** |
| **P0** | 大文件上传 | `CF_FILE_RANGE` / 增量水合 | 整文件读进内存（`upload.rs:522`），4 GB 文件直接 OOM |
| **P0** | readdir 分页 | `FETCH_PLACEHOLDERS` 分批 | 硬截断 200 项（`lib.rs:59`），**大目录永远读不全** |
| **P1** | per-file 状态机 | `CF_IN_SYNC_STATE` + `CfReportProviderProgress` | 用户看不到「这个文件到底同步完没有」 |
| **P1** | 拉取可中断 | `CANCEL_FETCH_DATA` | 大文件下载中途关不掉，只能等 60 s 超时 |
| **P1** | 删除可撤销 | `NOTIFY_DELETE` 双向确认 | `rm` 即硬删，误删无法找回（`delete.rs:14`） |

一句话总结：**qxync 现在「能用」，但用户看不见状态、工程师没法定位坏数据。**
CFA 的价值恰好在这两处 —— 它把「文件现在处于什么状态」显式建模成可查询、可上报的一等公民。

---

## 1. 概念映射：CFA ↔ qxync

CFA 的调用链与 qxync 的调用链几乎同构，这说明当初的架构判断是对的：

```
用户进程 (cat/Explorer)
   ↓                                    用户进程 (cat/vim/ls)
cldflt.sys (NTFS minifilter)                  FUSE 内核模块
   ↓ CfExecute / 回调                            ↓ FUSE 回调
同步引擎 (占位符 + 回调注册)                    qxync-fuse（占位符位图 + 水合）
   ↓ CfConnectSyncRoot 通道                       ↓ IPC
Cloud Files 服务端                                Qsync 服务端
```

逐概念对照：

| CFA 概念 | 作用 | qxync 对应物 | 差距 |
| --- | --- | --- | --- |
| `cldflt.sys` | 内核边界代理，隐藏 reparse point | FUSE 内核 + `fuser` | 等价 ✅ |
| `CfRegisterSyncRoot` | 声明「这棵树归我管」+ 注册策略 | 挂载点（`daemon.rs:2009` 单挂载点 = 单远端根） | 策略缺失 ❌ |
| `CfConnectSyncRoot` | 引擎 ↔ 平台双向通道 | daemon ↔ FUSE 会话（`ipc.rs`） | 等价 ✅ |
| `CF_CALLBACK_TYPE_FETCH_DATA` | 平台请求文件数据 | `ensure_range` / `ensure_chunk`（`lib.rs:2685`） | 等价 ✅ |
| `CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS` | 平台请求目录条目 | `DirListing` 快照（`lib.rs:320`） | **截断 200 ❌** |
| `CF_CALLBACK_TYPE_CANCEL_FETCH_DATA` | 取消拉取 | 无 | **缺失 ❌** |
| `CF_CALLBACK_TYPE_NOTIFY_DELETE/RENAME` | 通知 + 阻断 | `remove_entry`（`lib.rs:2368`） | 单向，无阻断确认 ❌ |
| `CF_PLACEHOLDER_STATE_*` | 显式状态标志位 | `chunks_done` 位图推导（`lib.rs:292`） | 语义等价，实现方式不同 ⚠️ |
| `CF_IDENTITY` / `CF_FILE_INFO.FileId` | 稳定文件身份 | `remote` 路径字符串（`lib.rs:215`） | **缺失 ❌** |
| `CF_PIN_STATE` | 用户意图：钉住 | `pins` 表（`store.rs:61`） | 等价 ✅ |
| `CF_IN_SYNC_STATE` | 是否已同步 | 无 | **缺失 ❌** |
| `CfReportProviderProgress` | 带外进度上报 | `HydroStats` 累计计数（`ipc.rs:505`） | 仅聚合，无 per-file ❌ |
| `CF_HYDRATION_POLICY_*` | 水合策略（4 档） | 无（固定 128 KiB 区间粒度） | **缺失 ❌** |
| `VALIDATION_REQUIRED` | 数据必须落盘+校验 | 只校验长度（`lib.rs:2751`） | **缺失 ❌** |
| `CANCEL_*` 回调 | 可取消 | 60 s 硬超时 | 部分 ⚠️ |

**读法**：✅ 9 项、⚠️ 3 项、❌ 8 项。骨架对了，地基缺了 8 块。

---

## 2. P0：健壮性（数据正确性）

### 2.1 稳定文件身份 —— `file_id`（地基，最该先做）

**现状问题**

`Inner.by_remote: HashMap<String, INodeNo>`（`lib.rs:305`）是唯一索引，SQLite 里
`baseline` / `uploads` / `deletes` / `pins` / `decisions` 五张表**全部以 `path TEXT PRIMARY KEY` 为主键**
（`store.rs:54,61,65,77,102`）。全 workspace grep 无 `file_id` / `etag` / `inode` 远端标识。

后果链条很硬：
1. 远端改名 → 路径变 → `baseline` 查不到 → 三向合并（`qxync-core/src/sync.rs:349`）判成
   「本地新增 + 远端删除」，走 `rename_local`，**平白生成一份冲突副本**。
2. 远端移动（`dir1/a.txt` → `dir2/a.txt`）同理。
3. 离线队列 `uploads` 按 `remote_path` 排队（`upload.rs:256`），排队期间远端改名 → 上传写到旧名字，
   生成幽灵文件。

**CFA 怎么做**

`CF_PLACEHOLDER_CREATE_INFO` 要求每个占位符必填 `FileIdentity` blob（≤ 4 KB），
这个 blob **在所有回调中原样回传给同步引擎**（`CfGetPlaceholderInfo` + `CF_PLACEHOLDER_INFO_CLASS_FILE_IDENTITY`）。
平台认这个 blob，不认路径。改名时 blob 不变，所以引擎能立刻认出「这是同一个文件」。

**qxync 怎么改**

服务端 `get_list` / `stat` 返回的 `DirEntry` 里**没有** id 字段（`qxync-core/src/model.rs:68`），
这是硬约束 —— 服务端给的只有 `filename / filesize / epochmt / isfolder / have_child`。

所以只能在本地造，但可以造得很稳：

```rust
// qxync-core/src/model.rs —— DirEntry 增加本地身份
pub struct DirEntry {
    // ... 现有字段不动
    /// 本地持久化身份：优先 "<mtime>-<size>"，与服务端无关，稳定可复算
    pub file_id: FileId,   // 见下
}

/// 文件身份 = (父目录 file_id, 文件名) 的哈希 + 内容签名。
/// 改名 → 变（可由 rename 回调迁移）；内容变 → 不变（这才是我们要的稳定性）。
/// 取不到父目录时退化为纯文件名哈希。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileId([u8; 16]);
```

落库：新增 `nodes` 表（`file_id` 主键，`path` 唯一索引，`remote_path`、`size`、`mtime`、`file_id`）。
`rename` 回调（`lib.rs:3411`）里同步迁移 `nodes` 行 —— 这一步现在是手工迁移
`by_remote` 和缓存文件（`lib.rs:3411-3436`），加一行 SQL 就够。

**收益**：改名/移动不再误判冲突；上传队列抗改名；为后续 checksum、增量备份铺路。
**成本**：中等（新增一张表 + `DirEntry` 构造点全量补字段 + 一次迁移）。建议作为 M12.1 单独做。

### 2.2 内容校验和 —— 静默损坏的唯一克星

**现状问题**

全workspace **无 `etag` / `checksum` / `sha256` / `md5`**。下载只校验长度：

```rust
// lib.rs:2751 —— 「铁则 1」只保证不短读，不保证内容对
if data.len() != want { return Err(...) }   // 长度不符 → EIO
```

更危险的是：网络中间设备截断/改写、`set_len` 后稀疏空洞被当成真数据、位图与内容写序颠倒
（`lib.rs:114-115` 已用「先内容后位图」规避了一半）—— 这些**长度都对，内容错了**，而用户只在
几个月后 md5 对不上时才会发现。`delta.rs` 里明明有 `md4` 实现（`delta.rs:381`），但只服务于未接线的 delta。

**CFA 怎么做**

`CF_HYDRATION_POLICY_VALIDATION_REQUIRED` 修饰符：平台保证「引擎返回的数据在交给用户应用之前
必然已落盘」，并且引擎可以事后取回同一批数据重新校验，只有校验通过平台才完成用户的 I/O 请求。
代价是额外磁盘 I/O，所以是 opt-in。

**qxync 怎么改**

服务端同样没有 etag 字段（`model.rs:68` 确认），所以**自建**：下载时算 `xxhash64`（比 sha256 快 10 倍，
足够做损坏检测，不需要抗篡改），连同 `size` 一起存进 `.qxstate` 文件头。

```
QXSTATE1(8) + version(4) + chunk_size(8) + size(8) + mtime(8) + nchunks(8)
  →  改为 + file_id(16) + per-chunk xxhash(8 × nchunks) + whole-file xxhash(8)
```

- 下载每个区间时算一次，落在位图旁边；
- 启动恢复时全量校验一次（`cache_file_for` 已有「签名不符即整份作废」逻辑，`lib.rs:2598-2612`，
  只需把校验加进去）；
- `--verify` CLI 子命令 + GUI 一键校验，对应 CFA 的 `VALIDATE_DATA` 回调。

**收益**：把「数据坏了但没人知道」变成「启动就发现 + 明确告诉用户哪个文件哪个区间坏了」。
**成本**：低（`xxhash-rust` 一个依赖 + 位图格式升版）。**性价比最高的一项，建议 M12.0 先做。**

### 2.3 大文件上传 —— 现在整文件进内存

**现状问题**

```rust
// upload.rs:522 —— 整个文件读进内存再发
let bytes = std::fs::read(&job.local)?;
```

`upload_bytes` 是单次 multipart POST（`qxync-client/src/lib.rs:627`）。所以：

- 上传 4 GB 视频 → 进程直接吃 4 GB 内存；并发几个就 OOM；
- 传到 90% 断网 → 从 0 重来，`max_attempts = 5`（`upload.rs:231`）用尽后放弃；
- 无进度，用户只见 `pending` 计数不见百分比。

注意下载侧**已经**做对了：区间续传（`Range: bytes=start-end`，`client/lib.rs:543-580`）+
单区间独立重试（`lib.rs:2681-2785`）。上传侧是明显的短板。

**CFA 怎么做**

`CF_FILE_RANGE` / `CF_HYDRATION_POLICY_PROGRESSIVE`：水合按块推进，块可以独立重试，
`CfReportProviderProgress` 汇报「已到第几块」。平台不关心引擎内部怎么搬，只要求
「已落盘的部分必须准确，未完成的部分保持 partial 状态」。

**qxync 怎么改**

1. `upload_one` 改为**流式 multipart**：读 8 MB 块 → 追加到 multipart body → 释放。
   内存占用从 O(filesize) 降到 O(8 MB)，单文件改动约 60 行。
2. 断点续传：服务端 `upload.php` 若支持 chunk 索引则接入；不支持则**先落本地分片**，
   失败重传只重传缺失分片。
3. 进度：`UploadJob` 加 `bytes_sent` / `bytes_total`，经 IPC 报给 GUI（对应
   `CfReportProviderProgress`）。`UploadSnapshot`（`upload.rs`）已经在对外暴露快照，
   扩展点现成。

**收益**：去掉 OOM 风险，大文件失败率大幅下降，GUI 可显示进度。
**成本**：中。流式化必做（M12.2），分片续传视服务端能力排后。

### 2.4 readdir 硬截断 200 项

**现状问题**

```rust
const LIST_LIMIT: usize = 200;                    // lib.rs:59
// filter_visible ... .take(LIST_LIMIT)            // lib.rs:2231
```

`LIST_LIMIT` 对应服务端 `Max_File_List`，但服务端返回的是**这一页 200 条**，
qxync 直接把它当作「这个目录的全部子项」。后果：任何超过 200 项的目录，
`ls` 永远只能看到前 200 个，且**没有报错、没有提示**。这是最隐蔽的一个 bug。

`lookup` 的三级回退（`lib.rs:2086-2133`）能缓解 —— 逐个冷查 NAS `stat` 可以命中第 201 项 ——
但 `ls` 不做逐项 stat，所以用户看到的目录就是残缺的。

**CFA 怎么做**

`CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS` 的结果通过 `CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS`
分批传输，`EntriesProcessed` 显式告诉你处理了多少条，还有 `CF_CALLBACK_TYPE_CANCEL_FETCH_PLACEHOLDERS`
允许中途放弃。**分页是协议的一部分，不是调用方的可选优化。**

**qxync 怎么改**

`get_list` 加 `offset`/`limit` 参数（先抓包确认服务端是否接受分页，不接受就用 `filename > last`
的游标式翻页）。`DirListing`（`lib.rs:320`）从「快照」升级为「快照 + `complete` 标志 + 游标」：

```rust
struct DirListing {
    entries: Arc<Vec<DirEntry>>,
    fetched: Instant,
    /// false = 还有更多页没拉（readdir 触底时继续拉，而不是假装到底了）
    complete: bool,
    cursor: Option<String>,   // 下一页起点
}
```

`filter_visible` 去掉 `.take(LIST_LIMIT)`，改为「返回已知条目 + 若 `!complete` 则后台续拉」。

**收益**：大目录正确性。这是**必须修**的，不修的话其他优化都是沙上建塔。
**成本**：低（若有分页）或中（若需游标翻页）。排 M12.3。

### 2.5 删除可撤销

**现状问题**

```rust
// delete.rs:14-17 —— 本地立即摘除，异步入队硬删
// delete.rs:31-35 —— 自述：「失败只留 stats.failed + journal」
```

而回收站还被**主动阻止**（`lib.rs:2828-2837` 的 `is_trash_segment` 拦 `.Trash` / `.Trash-<uid>` /
`@Recycle` 三种段）—— 那是防NAS 端 trash 协议导致每文件 2–3 次往返，和本地回收站是两件事。
用户在本地 `rm` 之后，节点立刻从 `by_remote` 和快照里消失，缓存文件立刻删除
（`lib.rs:2368-2471`），**没有任何窗口可以反悔**。

**CFA 怎么做**

`NOTIFY_DELETE` 回调**会阻断执行删除的用户应用程序**，且同步提供程序可以响应。
`NOTIFY_DELETE_COMPLETION` 再通知最终结果。这是一个显式的「两阶段确认」。

**qxync 怎么改**

删除改为两阶段：

1. `unlink` → 把文件**移到** `<挂载点>/.qxync-trash/<时间戳>-<原名>`，节点标记 `deleting`；
2. 入 `deletes` 队列（`delete.rs:132` 已有的队列直接复用）；
3. 队列成功 → 删掉 trash 副本；失败或超时（比如 7 天）→ 移回原位（撤销）。

回收站不占额外空间的关键是：trash 里存的是**占位符**（`.qxstate` 位图跟着走），
脱水的文件占位符只有 1 KB 块。

**收益**：删除是最高风险的不可逆操作，误删一次就失去用户信任。
**成本**：中（`remove_entry` 改造 + 定时清理 + GUI 一条「清空回收站」）。
注意 `deny_trash`（`lib.rs:2062`）对 `.Trash*` 段的拦截要放行自己的 `.qxync-trash`。

---

## 3. P1：好用性（用户看得见的状态）

### 3.1 per-file 同步状态机 —— CFA 的 `CF_IN_SYNC_STATE`

**现状问题**

用户能看到的状态**全是聚合数字**（`daemon.rs:1159-1218` 的 `status`）：

- `SyncInfo`（`ipc.rs:562-584`）：polls / refreshed / conflicts / uploaded / last_error ...
- `CacheInfo`（`ipc.rs:589`）：used_bytes / total_files / hydrated_files ...
- `HydroStats`（`ipc.rs:505`）：累计 count / bytes

文件级只有 `SpaceState` 三态（`ipc.rs:948-995`：仅在线 / 本地可用 / 始终可用），
且**只覆盖空间维度**，不覆盖「同步维度」。也就是说用户无法回答
「我上周改的那个文件上传成功了吗」—— 唯一的错误明细在 `journal` 表（`store.rs:87-101`），
而 journal 是环形裁剪的（1 万条 / 30 天，`daemon.rs:37-58`）。

`tasks.rs` 是**任务配置模型**（JSON 持久化，`tasks.rs:434`），不是执行器 —— 容易误读。

**CFA 怎么做**

`CF_IN_SYNC_STATE`：每个占位符都带「已同步 / 未同步」标志，由 `CfSetInSyncState` 设置。
Explorer 用它画图标，用户一眼看出哪些文件有问题。配套 `CF_SYNC_STATUS` 报告
「本轮同步做了什么」。`CF_INSYNC_POLICY` 甚至控制平台何时可以自动清除这个标志
（比如允许平台自己清 `ReadOnly` / `Hidden` / 时间戳的变更）。

**qxync 怎么改**

`Node`（`lib.rs:210`）加两个字段，`state_str`（`lib.rs:292`）从三态扩成二维：

```rust
struct Node {
    // ... 现有字段
    /// 同步维度：本地与远端是否一致（CFA: CF_IN_SYNC_STATE）
    in_sync: bool,
    /// 上传/下载进度 0.0~1.0（CFA: CfReportProviderProgress）
    progress: Option<(u64, u64)>,   // (done_bytes, total_bytes)
}
```

`SpaceState` 保持不变（那是空间维度），CLI / GUI 组合展示成
`同步中 42% · 仅在线` / `已同步 · 始终可用` / `未同步（1 个冲突待处理）`。

三个信息源已经现成：`dirty`（`lib.rs:222`）、`uploads.pending`（`ipc.rs`）、
`decisions` 待裁决数（`store.rs:102`）。加个 `file_states` 扩展字段即可。

**收益**：这是「好用」的最大落差。CFA 的用户体感大部分来自这一项。
**成本**：低-中（`Node` 加字段 + `file_states` 扩展 + GUI 展示）。**建议 M12.4。**

### 3.2 拉取可中断 —— `CANCEL_FETCH_DATA`

**现状问题**

下载中的 `read()` 是**阻塞的**（`lib.rs:2957-2996`）：持 `op_lock` 同步等 `ensure_range`。
超时 60 s（`HYDRATE_TIMEOUT`，`lib.rs:47`）。用户 `cat` 一个 10 GB 文件然后按 Ctrl-C，
内核会给 FUSE 发 `INTERRUPT`，但 qxync 侧的下载任务**不知道自己被取消**，会跑满 60 s 或跑完整个文件。
per-chunk single-flight（`lib.rs:2685-2697`）保证了不重复下载，但也让取消变得无处下手 ——
没有任何地方记录「这次下载是为了哪个 fd、哪个请求」。

**CFA 怎么做**

`CF_CALLBACK_TYPE_CANCEL_FETCH_DATA`：平台告诉引擎「不再需要这批数据，通常是原始请求被取消」，
引擎可以停止未完成的网络请求。`CF_CALLBACK_PARAMETERS` 里带 `FileId` + `ProcessInfo`，
引擎能精确知道是谁在请求。

**qxync 怎么改**

1. `inflight_chunks` 的 value 从 `Arc<Mutex<()>>` 换成结构体：
   ```rust
   struct ChunkFlight {
       lock: Mutex<()>,
       /// 引用计数：每个正在等待的 fd 持一份
       waiters: AtomicU32,
       /// 被取消则置位，下载循环每轮检查
       cancelled: AtomicBool,
   }
   ```
2. `read()` 入口注册 waiter，fuser 的 `Request` 拿到 `unique`（fuser 提供
   `Reply::interrupt` 语义），断连时 `cancelled.store(true)`；
3. `fetch_chunk_bytes`（`lib.rs:445`）每收一个 16 KB 分片查一次标志，取消则提前返回
   `ECANCELED`，**已下载的部分保留在位图里**（这本身就是有价值的部分结果）。

**收益**：大文件读取可中断；顺带解决「用户关了窗口但流量还在跑」的资源浪费。
**成本**：中（要改 fuser 调用链的取消传播）。排 M12.5，可与 3.1 合并做。

### 3.3 水合策略分级 —— `CF_HYDRATION_POLICY`

**现状问题**

水合粒度写死 128 KiB（`DEFAULT_CHUNK_SIZE`，`lib.rs:48`），**用户无选择权**。
对比 CFA 的四档策略，取 `max(app_policy, provider_policy)`：

| CFA 策略 | 行为 | 适合 |
| --- | --- | --- |
| `ALWAYS_FULL` | 平台拒绝任何导致未完全水合的操作 | 关键目录 |
| `FULL` | 平台保证整文件就绪后才完成 I/O | 传统应用兼容 |
| `PROGRESSIVE` | 够用即返回，后台继续拉 | 媒体播放（默认） |
| `PARTIAL` | 无后台续拉，按需拉 | 纯占位符 |

外加三个修饰符：`VALIDATION_REQUIRED`（强制校验）、`STREAMING_ALLOWED`（免落盘）、
`AUTO_DEHYDRATION_ALLOWED`（允许平台自动脱水）。

**qxync 怎么改**

配置文件加 `hydrate_policy`（默认 `progressive`）+ 修饰符开关：

- `full` → `ensure_range` 之前先 `hydrate_all`（`lib.rs:2268` 现成的函数），
  对应「传统应用（如某些老工具）必须读到完整文件」；
- `streaming` → 边下载边回给调用方，跳过落盘（配合 §2.2 的校验策略，收益大但要小心）；
- `auto_dehydrate` → 允许系统在不询问引擎的情况下脱水（对应现有脱水器的自动化）。

当前固定 128 KiB 的行为**等价于 `PROGRESSIVE`**，所以默认值不用改，只是把选择权交出去。

**收益**：兼容性（`full` 救老应用）+ 灵活性。**成本：低**（配置项 + 一个分支）。
建议并入 M12.4 一起做。

### 3.4 进度上报 —— `CfReportProviderProgress`

**现状问题**

`HydroStats`（`ipc.rs:505-508`）只有累计 `count` / `bytes`。单个 4 GB 文件下载时，
用户看到的是「正在下载 4 GB」，不知道进度是 5% 还是 95%。上传同理（§2.3）。

**CFA 怎么做**

`CfReportProviderProgress` 专门上报带外进度，Explorer 在文件旁边画进度条。
CFA 明确强调这一点：进度是**同步引擎的义务**，不是可选的 courtesy。

**qxync 怎么改**

`FetchCtx`（`lib.rs:445` 一带）加 `on_progress: Box<dyn Fn(u64, u64)>`，
每收一个分片调一次；经 IPC 汇总到 daemon，由 `status` 增量暴露，GUI 轮询渲染。
`UploadJob` 同理。

**成本**：低。**收益**：体感提升明显。建议并入 M12.4（和 3.1 一起做，共用 IPC 通道）。

---

## 4. P2：架构演进（暂不实施，记录备案）

| 主题 | CFA 机制 | qxync 若要做的理由 | 为什么先不做 |
| --- | --- | --- | --- |
| 回调注册表 | `CF_CALLBACK_REGISTRATION` 数组 | 现状 FUSE 回调是静态分发，已够用 | 收益低 |
| 平台版本协商 | `CF_PLATFORM_INFO` | 单平台 Linux，无版本碎片 | 不适用 |
| oplock / 租约 | `CfOpenFileWithOplock` | 解决并发写冲突，比 `op_lock` 强 | FUSE 已有 `op_lock`（`lib.rs:229`），冲突率低 |
| 硬链接策略 | `CF_HARDLINK_POLICY` | 占位符 + 硬链接语义复杂 | 会引入 §2.1 的 identity 难题 |
| Shell 集成 | 导航窗格 / 上下文菜单 / Toast | 需 GUI 深度改造 | 依赖 GUI 成熟度（`M4-GUI.md`） |
| 缩略图 | `IStorageProviderThumbnailProvider` | 体验加分项 | 不解决正确性问题 |

---

## 5. 落地路线

按「先地基后表层、先正确后体验」排序。每项都是独立可验证的，不做捆绑。

| 里程碑 | 内容 | 对应 § | 依赖 | 预估 |
| --- | --- | --- | --- | --- |
| **M12.0** | xxhash 校验和 + `.qxstate` 升版 + `--verify` | 2.2 | 无 | 0.5 天 |
| **M12.1** | `file_id` + `nodes` 表 + rename 联动 | 2.1 | 无 | 2 天 |
| **M12.2** | 上传流式化（去 OOM） | 2.3 | 无 | 1 天 |
| **M12.3** | readdir 分页 + `DirListing.complete` | 2.4 | 无 | 1 天 |
| **M12.4** | per-file 状态机 + 进度上报 | 3.1 / 3.4 | M12.1（要 file_id） | 2 天 |
| **M12.5** | 水合策略分级配置 | 3.3 | 无 | 0.5 天 |
| **M12.6** | 拉取取消传播 | 3.2 | M12.0（`ChunkFlight` 结构） | 1.5 天 |
| **M12.7** | 回收站 / 删除可撤销 | 2.5 | M12.1 | 3 天 |

**建议节奏**：M12.0 → M12.3 → M12.2 → M12.1 → M12.4 → M12.5 → M12.6 → M12.7。
理由：M12.3 是当前唯一**功能性 bug**（目录读不全），M12.2 是唯一**崩溃风险**（OOM），
两者都不依赖新设计，先做；M12.1 是后面所有项的地基。

**顺手要修的现存小问题**（不改也能过，但既然在动这块）：

- `docs/开发规划.md:220-236` 的状态模型描述已与实现不符（写的是 SQLite 存节点/元数据，
  实际存的是 cursor/baseline/pin/队列/journal/decisions，节点在内存 + `.qxstate`）。文档需同步。
- `upload.rs:522` 的 `std::fs::read` 与 `Cargo.toml` 里刻意保留的 `panic = "abort"` 注释
  （"为了让用户报 bug 时贴出来的回溯是可读的"）风格一致 —— 流式化后回溯可读性不受影响。

---

## 6. 什么不该照搬（重要）

CFA 是 **Windows 特有的 minifilter 方案**，很多机制的前提在 Linux FUSE 上不存在。
盲目移植会做出一堆没有收益的抽象。

| CFA 机制 | 为什么在 qxync 上不适用 |
| --- | --- |
| `cldflt.sys`（内核 minifilter） | 只支持 NTFS（文档明说依赖 NTFS 特性）。Linux 侧 FUSE 就是内核边界，无需自建 |
| reparse point 隐藏 | CFA 需要「对所有应用隐藏 reparse point」来兼容老应用。FUSE 的稀疏文件对用户透明，**这层兼容负担不存在** |
| 60 s 回调固定超时 | Windows 上层约束（Explorer 会挂起）。FUSE 内核对 read 的超时语义不同，硬对齐反而有害 |
| 桌面桥 / UWP | Windows 特有。qxync 是纯 CLI + GTK GUI（`qxync-gui`），不涉及应用打包 |
| `CF_CALLBACK_TYPE_NOTIFY_*_COMPLETION` 全套 | Windows 要求 provider 响应每个生命周期事件做审计。qxync 的 `dirty` + `uploads` 队列已经覆盖了同样的信息，且不需要用户应用感知 |
| `CF_PLACEHOLDER_STATE_*` 标志位 | CFA 需要「Shell 从 `FileAttributes` + `ReparseTag` 直接读出状态」—— 因为 Explorer 要在没有 provider 的情况下画图标。FUSE 下 `getattr` 由 qxync 应答，**状态可以从内存直接返回，不需要编码进文件属性**。qxync 现有的位图推导（`lib.rs:292`）是对的，不要改成标志位 |
| `CF_HARDLINK_POLICY` | 占位符 + 硬链接的 identity 语义极其麻烦。CFA 明确说「平台会强制水合」，等于绕开问题。qxync 暂不支持更诚实 |
| 命名空间重叠注册 | 文档明说「不允许两个同步根重叠」。qxync 现阶段是「一个挂载点 = 一个远端根」（`daemon.rs:2009`），比它更严格，无需这层校验 |

**一句话**：学 CFA 的**状态建模方法论**，不学它的**平台实现**。CFA 花大力气解决
「状态必须编码进 NTFS 属性」的问题，Linux FUSE 上这个根本问题不存在。

---

## 7. 附录：核心 API 速查

只列本文引用到的，完整清单见
[Cloud Filter Functions](https://learn.microsoft.com/zh-cn/windows/desktop/cfApi/cloud-files-functions)。

**注册与连接**
```
CfRegisterSyncRoot / CfRegisterSyncRootWithProvider   注册同步根 + 策略
CfConnectSyncRoot                                     建立双向通道
CfDisconnectSyncRoot                                  断开
CfUnregisterSyncRoot                                  注销
CfReportSyncStatus                                    无需连接即可报告状态
```

**占位符生命周期**
```
CfCreatePlaceholders            批量创建（数组 + EntriesProcessed 逐项报告）
CfUpdatePlaceholder             更新特征
CfConvertToPlaceholder          普通文件 → 占位符
CfRevertPlaceholder             占位符 → 普通文件（去掉全部特殊属性）
CfSetInSyncState                标记已同步
CfSetPinState                   用户意图（任何应用可调）
CfExecute                       响应平台操作的主要入口
```

**查询**
```
CfGetPlaceholderInfo / CfGetPlaceholderRangeInfo
CfGetPlaceholderStateFromFileInfo / FromFindData / FromAttributeTag
CfGetSyncRootInfoByPath
CfGetPlatformInfo               版本协商
```

**进度与错误**
```
CfReportProviderProgress        带外进度上报
CfUpdateSyncProviderStatus      更新提供程序状态
CfQuerySyncProviderStatus
```

**回调类型**（`CF_CALLBACK_TYPE`，qxync 重点参考前 4 个）
```
FETCH_DATA                     → ensure_range / ensure_chunk        ✅ 已有
FETCH_PLACEHOLDERS             → DirListing 快照                   ⚠️ 需分页
CANCEL_FETCH_DATA              → （无）                            ❌ 缺失
VALIDATE_DATA                  → （无，仅长度校验）                ❌ 缺失
NOTIFY_DELETE / RENAME         → remove_entry / rename            ⚠️ 单向
NOTIFY_DEHYDRATE               → dehydrate_now                    ✅ 已有
```

**关键状态位**（`CF_PLACEHOLDER_STATE`，供语义对照，**不要照搬实现**）
```
0x01 PLACEHOLDER                是占位符
0x02 SYNC_ROOT                  是同步根
0x04 ESSENTIAL_PROP_PRESENT     基本属性已存
0x08 IN_SYNC                    与云端一致
0x10 PARTIAL                    内容未就绪（可能全部在本地，也可能不全）
0x20 PARTIALLY_ON_DISK          内容未完全在本地（设了它必须同时设 PARTIAL）
```

**水合策略**
```
主要：ALWAYS_FULL > FULL > PROGRESSIVE > PARTIAL    （取 max(app, provider)）
修饰：VALIDATION_REQUIRED  STREAMING_ALLOWED  AUTO_DEHYDRATION_ALLOWED
      （VALIDATION_REQUIRED 与 STREAMING_ALLOWED 互斥）
填充：ALWAYS_FULL / FULL / PARTIAL
硬链：默认禁止，可声明 ALLOWED
```

---

## 8. 一页总结

- **架构方向已经对了** —— qxync 的 FUSE + 区间水合 + 三向合并与 CFA 同构，9 个核心概念已等价实现。
- **最该补的三块地基**：稳定 file_id（§2.1）、内容校验和（§2.2）、readdir 分页（§2.4）。
- **最该补的一块体验**：per-file 同步状态机 + 进度上报（§3.1 / §3.4）—— 这是 CFA 用户体感的来源。
- **两个现存硬伤**：上传整文件进内存（OOM，§2.3）、目录超 200 项读不全（§2.4）。
- **最重要的判断**：CFA 的价值是方法论，不是 API。它花了大力气解决「状态如何编码进 NTFS 属性」，
  而这个问题在 FUSE 上不存在 —— 照搬会做出一堆无用抽象（§6 列了 8 项）。
