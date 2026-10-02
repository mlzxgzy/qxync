# M5 —— SQLite 元数据 + librsync 兼容 delta

> 状态：**已实现并真机验收**（`xtask/tests/m5-matrix.sh` **28/28**）。
> 里程碑定义见 [`开发规划.md`](开发规划.md) §2：「M5 = 增量 delta + SQLite 元数据」。
>
> 一句话结论：**SQLite 那一半完整落地**（游标/baseline/pin/上传队列全部进 `sync.db`，含 JSON 迁移与
> 崩溃语义）；**delta 那一半在服务端走不通** —— 真机探测（§3）证明这台 NAS 没有历史版本
> （`versioning_support` 全 0、`versioning_stat_delta` 恒 `exist:0`、`versioning_gen_sig` 恒 `status:33`），
> 所以本地把 **librsync 格式的 sign/delta/patch 完整实现并做了字节级验收**，
> 上层用 **DeltaGate** 做能力判定：探测到服务端可用才走增量，否则维持整文件传输（当前真实行为）。

---

## 1. 为什么要换成 SQLite（不是「为了用 SQLite」）

M2c 的状态是两份 JSON：`cursors.json` + `baseline.json`，各自「临时文件 + rename」原子落盘。
问题不在单个文件，而在**两个文件之间**：

```
poll_once:  处理事件 → 写游标(commit 1) → 写 baseline(commit 2)
                              ↑ 崩在这里：游标已推到 N，baseline 还停在 N-1
```

下一轮会把已处理过的事件**再处理一遍**（或反过来漏事件）。报告 09 §9.6 把三个游标称为
「同步正确性的核心状态」，要求原子持久化 —— SQLite 的**单事务**才是这个语义。

M5 顺带解决的两个老窟窿：

| 窟窿 | 之前 | 现在 |
|---|---|---|
| `pin` 只在内存 | daemon 一重启全丢 → M3 脱水安全检查（`pinned`/`excluded` 不脱水）**静默失守** | 落库，重启恢复（实测 2 条 pin 活过重启） |
| 上传队列 = 一堆 `.dirty` JSON | 能用，但元数据散落两处 | 入库（`uploads` 表），老标记文件自动迁移归档 |

## 2. 状态库设计

路径：`<data>/sync/<host>/sync.db`（沿用 M2c 的「按 NAS 分目录」，多台 NAS 互不污染；
`ConfigPaths::db_file()` 的全局 `sync.db` 未被使用，实际用的是这个按 host 隔离的库）。

```sql
PRAGMA journal_mode = WAL;      -- 读写不互斥
PRAGMA synchronous  = FULL;     -- 游标/事务提交必须真落盘（正确性 > 吞吐）
PRAGMA user_version = 1;        -- schema 版本

CREATE TABLE cursors  (id INTEGER PRIMARY KEY CHECK (id=1),
                       config, notify, global_notify, max_log_seen, log_missing_count);
CREATE TABLE baseline (path TEXT PRIMARY KEY, present, is_dir, size, mtime);
CREATE TABLE pins     (path TEXT PRIMARY KEY, state TEXT NOT NULL);
CREATE TABLE uploads  (remote_path TEXT PRIMARY KEY, remote_dir, remote_name,
                       local, mtime, attempts, ephemeral);
CREATE TABLE meta     (key TEXT PRIMARY KEY, value TEXT NOT NULL);
```

实现位置：`crates/qxync-core/src/store.rs`（`Store`，`rusqlite` 用 `bundled` 特性自带 SQLite，
不依赖系统 `libsqlite3`）。

**关键 API 与取舍**

* `Store::save_state(&cursors, &baseline)` —— ★ 游标 + baseline **同一事务**。
  `qxync-daemon` 的 `persist()` 已从「两次 save」改成调它。
* **内存模型不动**：`Baseline` / `Cursors` 仍是工作副本，持久层从 JSON 换成 SQLite。
  这样 1300 行的对账逻辑（`has_children_of` / `children_of` / `descendants_of` …）零改动，
  风险最小；代价是 baseline 落盘是「事务内全量替换」（实测 225 项，毫秒级；
  极端规模下的增量 diff 留待以后）。
* `Store::migrate_legacy(dir)` —— 把 M2c 的 JSON 迁进来：
  * **只在库里对应数据为空时才导入**（避免过期 JSON 覆盖新库）；
  * 旧文件一律改名 `*.json.migrated` **保留备份**，不删除；
  * 幂等：第二次启动什么都不做（矩阵里有专项断言）。
* `Store::integrity_check()` —— `PRAGMA integrity_check`，把「库是不是好的」变成可验收项
  （`qsync store --integrity`）。

## 3. delta 那一半：先探测，再决定

### 3.1 真机探测结论（QTS 5.2.9 / Qsync QPKG build 20260723，用户 `test1`）

原始响应存 `xtask/probe/probe-out/m5-versioning/`（已 gitignore）。

| 端点 | 结果 | 判定 |
|---|---|---|
| `versioning_probe` | `{"versioning_version":"1.0.0","versioning_enable":1,"qbox_versioning_enable":1,"qbox_user_versioning_enable":1}` | ✅ 端点存在，能力位全开 |
| `versioning_lock&create_version=1` | `{"status":1,"lockid":"1790834873-5654","version_id":"1790834873"}` | ✅ 加锁可用 |
| `versioning_lock&check_version=1` | `status:1`（原样回 lockid/version_id） | ✅ 可复用锁 |
| `versioning_stat_delta` | `{"exist":0,"size":"---"}`（`hello.txt` / `big.bin` 都一样） | ❌ **没有可用旧版本 → 算不了 delta** |
| `versioning_gen_sig` | `{"status":33,"pid":…}` | ❌ 造不出签名 |
| `versioning_unlock` | `{"status":1,"success":"true"}` | ✅ 收尾正常 |
| `get_list` 每项 | `versioning_support = 0`（`/home/qxync-test` 6 项、`/home` 3 项全 0） | ❌ **该用户/目录没有任何历史版本** |
| `versioning_stat` | `version_id:"---"`、`datas:[]`、`number:0` | ❌ 无版本记录 |
| 旧命名空间 `utilRequest.cgi` | 全部 `versioning_*` 回 `status:20`（未知 func） | ❌ 只有新命名空间实现 |
| 相对路径 | `.` → `status:4`、`..` → `status:12` | 路径非法会被拒 |

**阻塞点不是鉴权、不是命名空间、不是 404，而是「NAS 侧没有为这个目录建立历史版本」**
（`versioning_support=0` + `version_id="---"`）。要打开需要 NAS 管理侧操作（版本化/快照策略），
不是客户端能解决的。

### 3.2 本地 delta 编解码（`crates/qxync-core/src/delta.rs`）

格式与参数照报告 06 §6.1–6.3（逐条有反汇编证据）：

| 项 | 值 |
|---|---|
| magic | signature `0x72730136`、delta `0x72730236`（大端） |
| block_len | **1 MiB**（librsync 默认 2048，Qsync 显式覆盖） |
| strong_sum_len | **16**（librsync 默认 8），强校验 = **MD4** 前 16 字节 |
| 弱校验 | `CHAR_OFFSET=31`；`A=Σ(b[i]+31)`、`B=Σ(n-i)*(b[i]+31)`、`weak=(B<<16)\|(A&0xffff)` |
| 命令 | `0x41+4*where_code+len_code`（`1→0,2→1,4→2,8→3`）；`0x41..0x44` LITERAL、`0x45..0x54` COPY、`0x00` END |

对外 API：`Signature::{compute,compute_default,write,parse}`、`delta(&sig, new)`、`patch(basis, delta)`、
`weak_sum`、`RollingWeak`（O(1) 滚动）。

匹配算法用「按弱校验分桶 + 滚动窗口」（rsync 经典做法）：命中就发 COPY、没命中把当前字节并进
literal。**不追求最小 delta，但保证正确**（矩阵与单测断言 round-trip 与体积收益）。

### 3.3 能力门控（`qxync-client::DeltaGate`）

```rust
pub enum DeltaGate {
    Available  { version_id: String, delta_size: Option<u64> },
    Unavailable { reason: String },
}
// Client::delta_gate(dir, name)：
//   1) stat 条目存在、非目录、versioning_support=true   ← 这台 NAS 卡在这里
//   2) versioning_probe 三个 enable 位全开
//   3) versioning_lock(create=1) 拿到 lockid/version_id（此后无论成败都尽力 unlock）
//   4) versioning_stat_delta exist=1  → Available
```

真机输出（`cargo test -p qxync-proto-test --test versioning -- --ignored --nocapture`）：

```
probe: versioning_version=Some("1.0.0") versioning_enable=true qbox_versioning_enable=true enabled=true
lock:  status=1 lockid=1790835515-2591 version_id=1790835515
stat_delta: version_id=1790835515 exist=false size=None raw={"exist":0,"size":"---"}
gate:  Unavailable reason=versioning_support=0（/home/qxync-test/hello.txt 没有可用的历史版本）
unlock: ok
```

**上传/下载路径仍走整文件 + 分片**（M0/M2b 的既有实现，未变）；DeltaGate 只做判定与上报，
不产生「假装能走增量」的死代码。等 NAS 侧开了版本化，取消门控即可接上分支 B（上传增量）：
`lock → gen_sig → get_sig → rdiff_delta → upload_file(offset 分片) → commit_upload → unlock`
（报告 06 §6.5 的时序，客户端方法已在 `qxync-client` 备好）。

## 4. 上传队列入库

`crates/qxync-fuse/src/upload.rs`：队列工作副本仍是内存 `VecDeque`（单 worker 串行 + 条件变量 +
退避重试），持久化换成 `marker_dir/queue.db`（同一套 `Store`）：

* `enqueue`：先 `put_upload` 再改内存（保留 M2b 的「先落盘再改内容」铁律）；
* worker：成功 `delete_upload`；重试 `bump_upload_attempts`；达到 `max_attempts` 放弃时也删行
  （语义 = 「库里只有未完成作业」，与当年删 `.dirty` 一致）；
* `cancel` 删行；`new()` 从 `store.uploads()` 恢复；
* **迁移**：残留 `*.dirty` → 导入 → 改名 `*.dirty.migrated`（入库失败不改名，下次重试）；
* **降级取舍**：`Store::open` 失败只 `warn!`、不返回 Err（挂载可用性优先），退化成无持久化队列。

> `sync.db`（每 host，同步状态）与 `queue.db`（全局，上传队列）刻意分库：队列在
> FUSE 挂载层构造、生命周期与 host 无关，硬塞进同一个库反而把两层耦合起来。

## 5. 验收

```bash
xtask/tests/m5-matrix.sh          # 28 项：迁移 / 幂等 / 不双写 / pin 存活 / 单测 / 真机 gate
xtask/tests/m5-matrix.sh --no-nas # 不连真机
qsync store [--integrity] [--json]  # 状态库快照（JSON 形态给脚本用）
```

矩阵覆盖（28 项，全绿）：

1. **迁移**：造一份「M2c 时代」的 `cursors.json` + `baseline.json`（含一条 `exists=false` 的
   MISSING 条目）→ 启动 daemon → 断言游标五个字段、baseline 行数、schema 版本、`integrity_check=ok`、
   库路径、旧文件已归档且不再在原位；
2. **幂等 + 不双写**：重启后迁移日志只出现一次、行数不变、**daemon 跑过一轮 30s 轮询后旧的
   `baseline.json`/`cursors.json` 不会复活**（这条专门盯「JSON/SQLite 双写」回归）；
3. **pin 持久化**：写 pin → 重启 → 库里还有 → IPC 查 pin 一致；
4. **单测**：`cargo test -p qxync-core --lib delta`（11）、`--lib store`（10）、
   `cargo test -p qxync-fuse`（19，含队列入库/恢复/迁移）、`cargo test -p qxync-client`（11，含 DeltaGate 三路径）；
5. **真机**：`cargo test -p qxync-proto-test --test versioning -- --ignored` 通过并打印 gate 判定。

单元测试里最能说明问题的两条：

* **MD4 过 RFC 1320 全部官方测试向量**（含 80 字节跨块那条）——强校验的正确性根基；
* **4 MiB 相同文件的 delta < 128 字节**（4 条 COPY + magic + END）——滚动匹配真的在工作，
  而 `patch(basis, delta) == new` 覆盖了相同/追加/前置/中间改/空文件/短于一块/1 MiB 默认块等场景。

真机实测的额外证据：主状态目录从 M2c 升级时迁移了 **225 条 baseline**（cursors
`config=188 notify=37 global_notify=177 max_log_seen=188`），旧文件归档成 `.json.migrated`；
`qsync sync --once` 在 SQLite 状态上照常跑完对账（baseline 225 项、0 冲突 0 删除）。

## 6. 已知限制 / 后续

* **服务端增量目前不可用**（§3.1），这是 NAS 侧策略问题；门控已备好，开了就能接。
* baseline 落盘是事务内全量替换；10 万级条目下需要改成「差集 upsert + 删缺失」（现在是毫秒级，
  但值得写进 TODO）。
* 状态库没有 schema 迁移框架：`user_version=1` 已写入，将来加表要写 v1→v2 迁移。
* 占位符/区间状态（`chunks_done`）仍在内存、由稀疏文件与稀疏区间现推；真要落库要等有明确收益的场景。
* `queue.db` 与 `sync.db` 分库（§4），`store` 命令只报 `sync.db` 的 baseline/游标/pin 与
  `uploads` 表行数（`uploads` 表在 `sync.db` 里目前恒为 0，队列真实数据在 `queue.db`）。

## 7. 踩到的坑

1. **旧 daemon 会和新 daemon 抢同一个状态目录**：本机调试时发现「迁移归档后 `cursors.json`
   又冒出来」——是上一轮会话遗留的**旧二进制 daemon** 还在按 30s 轮询、用旧代码写 JSON。
   判据是文件 mtime 恰好落在轮询节拍上。这提醒我们：**改状态持久化时必须确认没有老进程在跑**
   （矩阵用全新状态目录就是为隔离这类污染）。
2. **`rusqlite` 选 `bundled`**：自带 SQLite 源码，不要求系统 `libsqlite3-dev`（代价是首次编译多花
   一点时间）；WAL 会在数据目录留下 `-wal`/`-shm` 两个文件，属正常现象（不要当成垃圾清掉）。
3. **迁移只能单向**：`migrate_legacy` 只在目标表为空时导入；旧 JSON 改名保留是为了「回滚时还有
   原始数据」，但**回滚后新数据在 SQLite 里**，反向迁移没做。
4. `Store` 内部用 `Mutex<Connection>`：daemon 的写路径本来就是串行的（`persist` 持锁），
   但**不要在持锁期间做网络 IO**，否则会拖住整个同步循环。
