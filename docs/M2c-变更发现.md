# M2c：变更发现（三游标轮询 + baseline 对账 + 冲突副本 + 删除保护）

> 目标：**另一台设备改了 NAS 上的文件，挂载点要能发现**；双方都改 → **冲突副本**（谁都不丢）；
> 大批删除 → **熔断**（不把误删同步到对面）。
> 状态（2026-09-30）：已实现并真机验收（`cargo test -p qxync-daemon -- --ignored`，
> 另见 `xtask/tests/fuse-matrix.sh` 的 M2c 段）。

---

## 1. 真机实测契约（2026-09-30，QTS 5.2.9 / Qsync QPKG 5.0.0.7 build 20260723）

| 事实 | 实测结果 | 对实现的影响 |
|---|---|---|
| `qbox_get_sync_log` 的 `lower` | **闭区间下界**：`lower=30` 会返回 `log_id=30` | 游标推进到「最后一个 `log_id` + 1」 |
| 没有事件时 | `status:-17`（旧文档里的谜团），**不是协议错** | 不推进游标；转 baseline 对账 |
| 事件结构 | `{log_id,user,old_filepath,filepath,action,isfolder,device,device_uid,exist,mtime,size}` | 直接反序列化；`size` 是字符串 |
| `isfolder` | `1`=目录、`2`=文件、`0`=删除项（不是布尔 0/1！） | 按值判断，别当 bool |
| 删除事件 | `filepath` 实测**为空字符串**，`exist=0` | 删除**不能靠事件**，只能靠对账差集 |
| 事件路径 | 真实路径 `/share/homes/<user>/...`，需映射到视图 `/home/...` | `map_event_path()` + 归属校验 |
| 我们自己的写操作 | `upload.php` / `rename` / `move` / `delete` **不产生** sync log 事件 | **对账是主路径，事件只是快路径** |
| 事件可用性 | 同一个 sid 下时好时坏（`lower=0` 有时返回 32 条，有时 `-17`） | 两种都按正常处理，绝不因 `-17` 报错 |
| `max_log` | 我们调 `qbox_write_log` 后会涨（44），但区间内取不到事件 | 游标按 `max_log` 观测、按事件实际推进 |
| config / global notify | `qbox_get_device_config_list` / `qbox_query_notify` 可调，均为 0 项 | M2c 只取 + 计数 + 推进游标，不消费 |

---

## 2. 三游标（照报告 08 §8.3 / 01 §7.3）

```
Cursors { config, notify, global_notify, max_log_seen, log_missing_count }
```

* **notify**（文件变更，M2c 的主战场）：`qbox_get_sync_log&lower=<游标>&number=200`。
* **config**：`qbox_get_device_config_list&user=&lower=&upper=`（同步文件夹/设备策略）。
* **global_notify**：`qbox_query_notify&lower=&upper=`（共享邀请、团队文件夹）。
* 三条铁律（报告 08 §8.3）：
  1. **先处理事件、再推进游标**：每处理完一批就落盘，崩溃宁可重放。
  2. **游标原子持久化**：临时文件 + `fsync` + `rename` + `fsync` 父目录
     （`qxync_core::sync::atomic_write`）。
  3. **`max_log` 回退 → 游标归零 + 全量重扫**（`Cursors::should_reset`）。
     `status:-17` 时**不推进**，只记账（`log_missing_count`）。

落盘位置：`~/.local/share/qsync/sync/<host>/{cursors.json,baseline.json}`（按 NAS 隔离）。

---

## 3. baseline 与三向决策表

baseline = **上次同步成功时远端的样子**（`path → {exists,is_dir,size,mtime}`）。
每轮对账把「本地视图 / baseline / 远端列举」三方比较：

| 远端 | baseline | 本地 | 结论 |
|---|---|---|---|
| 不存在 | 不存在 | — | `Noop` |
| 不存在 | 有 | 本地有改动 | `RecreateRemote`（重新上传，否决远端删除） |
| 不存在 | 有 | 本地没改 | `DeleteLocal`（**受删除保护**） |
| 有 | 不存在 | 本地有改动 | `UploadLocal` |
| 有 | 不存在 | 本地与远端一致 | `AdoptBaseline`（上传已落地/占位符） |
| 有 | 不存在 | 其它 | `RefreshRemote`（登记占位符） |
| 有 | 有 | 远端==baseline、本地没改 | `Noop` |
| 有 | 有 | 远端==baseline、本地改了 | `UploadLocal` |
| 有 | 有 | 远端变了、本地没改 | `RefreshRemote`（刷新元数据 + **失效缓存**） |
| 有 | 有 | 双方都改，远端==本地 | `AdoptBaseline`（上传已落地） |
| 有 | 有 | 双方都改，且内容不同 | **`Conflict`** |

要点：

* **「本地有改动」= 脏 + 签名 != baseline**。FUSE 的 `dirty` 标志上传成功后不会自动清，
  用签名一起判定才不会把「上传完的老脏标记」误判成冲突。
* 目录只比「存在性 + 类型」，文件比 `size + mtime`（服务端 epoch 秒，无亚秒）。
* 只在**已知路径**上做决策（挂载视图里的节点 + baseline 条目）；远端新出现的文件由
  `readdir`/`lookup` 实时发现（on-demand 的本性），对账时顺手把它们的 baseline 登记好。

---

## 4. 冲突副本

```
本地改 + 远端改，且两边内容不同
  → 远端内容占**原名**（本地缓存失效，下次读按需水合远端新内容）
  → 本地内容复制到 stash，用冲突名上传：
     notes.txt → notes (conflicted copy from <本机名> 2026-09-30).txt
  → 原路径 baseline = 远端签名；副本作为新远端文件被后续对账登记
```

* 重名时补序号：`... 2026-09-30 2).txt`（最多试 20 次）。
* 顺序很重要：**先 stash（复制本地缓存）再失效原始缓存**，否则本地内容就丢了。
* 冲突副本的上传走 M2b 的上传队列（临时文件 `ephemeral`，成功后自动清理 stash）。
* ★ **真机踩到的竞态**：本地大改动还在上传（队列 `pending=0` 但 `active=1`）时判定冲突，
  冲突副本还没传完，**在途的「原名上传」就把远端改回了本地内容** → 冲突副本白做、远端变更丢失。
  修法：判定冲突前 `cancel(path)` + `drain(30s)` 等在途作业结束，再重新 `stat` 重判
  （若此时远端已等于本地 → 冲突自然消解，只补 baseline）。
  配套：上传队列快照新增 `active`（`status` 显示「上传中 yes/no」），
  等待「上传完成」必须同时看 `pending=0 && active=no`。

---

## 5. 删除保护（两个方向）

| 方向 | 机制 | 阈值（默认） | 解除方式 |
|---|---|---|---|
| 远端大批删除 → 不要批量清本地 | 引擎一轮对账里 `DeleteLocal` 计数超限 → **整批挡住** | >50 项，或 >25%（baseline≥20 项时） | `qsync sync --force-deletes` |
| 本地大批删除 → 不要清空远端 | FUSE `unlink/rmdir` 的滑动窗口熔断（`DeleteGuard`） | 60 秒内 >100 次 | `qsync sync --force-deletes` 或重新挂载 |

* 远端删除判定**只认** `status 4/5/6`（不存在/无权限）；网络错误、超时**绝不**当成删除
  ——否则一次抖动就会清空本地。
* 熔断期间被删掉的远端文件**本地节点与缓存都保留**（`ls` 是实时的，但节点/缓存不清），
  避免「远端误删 → 本地缓存也没了」的双重损失。

---

## 6. 远端变更如何作用到 FUSE

`QxyncFs` 的节点表改成 `Arc<Mutex<Inner>>`，挂载前用 `fs.handle()` 取一个
**`FsHandle`**（实现 `LocalView`）交给守护进程的引擎；FUSE 实例移进挂载线程后引擎仍可操作：

| 操作 | 作用 |
|---|---|
| `apply_remote_meta` | 刷新 `size/mtime/kind`；**内容变了就丢掉稀疏缓存 + 清空区间表** |
| `invalidate_content` | 只丢缓存内容（冲突解决后以远端为准） |
| `remove_remote` | 删除节点（目录连后代）+ 缓存文件 |
| `mark_dirty` | 标脏 + 入上传队列（`RecreateRemote`/`UploadLocal`） |
| `stash_conflict` / `enqueue_upload` | 冲突副本 |

两条铁则不变：`read()` 要么给真实字节、要么 `EIO`（失效后按区间重新水合，
不会把旧缓存当新数据）；脱水（M3）依然要「先 `inval_inode` 再清内容」。

---

## 7. 引擎与接口

* **引擎**（`qxync-daemon/src/sync.rs`）：`poll_once()` = ① `max_log` → ② notify 事件
  （按 `device_uid` / `user` / 挂载根过滤后逐条 `stat` + 决策）→ ③ config/global 游标 →
  ④ 对账所有挂载视图（列已知目录 + 差集）→ 删除保护 → 落盘。
* **轮询**：守护进程后台任务，默认 30s；`QSYNC_POLL_INTERVAL=0` 暂停。
* **IPC**（`qxync_core::ipc`）：
  * `sync {once, force_deletes, max_deletes, interval_secs}`；
  * `rm {dir,name}`（测试/脚本用）；
  * `mount {…, delete_limit}`（本地删除熔断阈值，0 = 关闭）；
  * `status.sync` = `SyncInfo`（游标 / baseline 条目数 / 计数 / 事件设备 / 熔断原因）。
* **CLI**：`qsync sync [--once] [--force-deletes] [--max-deletes N] [--interval S]`、
  `qsync rm <dir> <name>`；`qsync status` / `qsync mount` 打印上述信息。

---

## 8. 验收

**真机集成测试**（`#[ignore]`，需环境变量）：

```bash
export QSYNC_TEST_HOST=... QSYNC_TEST_USER=... QSYNC_TEST_PASSWORD=...
export QSYNC_TEST_FIXTURE=/home/qxync-test
cargo test -p qxync-daemon -- --ignored --test-threads=1 --nocapture   # 引擎：刷新/冲突/删除保护/游标
cargo test -p qxync-proto-test -- --ignored --test-threads=1 --nocapture  # 协议：sync log / notify 端点
```

| 检查 | 判据 |
|---|---|
| 远端刷新 | 远端改大小/mtime → 视图元数据刷新、缓存失效、下次读拿到新内容 |
| 冲突副本 | 原名=远端内容，副本=本地内容（上传到 NAS，真机逐字节比对） |
| 删除保护 | 5 项 > 阈值 2 → 整批挡住（节点与缓存保留）；`--force-deletes` 才删 |
| 三游标 | `cursors.json` 原子落盘，`max_log_seen ≤ 服务端 max_log`，notify 按 log_id 推进 |
| 事件解析 | `-17` 与事件两种返回都当正常；`lower` 闭区间；`isfolder 1/2/0` |

**FUSE 验收矩阵**（`xtask/tests/fuse-matrix.sh` 的 M2c 段，daemon 轮询 2s）：

1. 远端新文件可见；远端改动 → 元数据刷新 + 缓存失效（读到新内容）；
2. 远端删除 → 挂载点节点消失；
3. 批量删除熔断（6 个 `keep*`，阈值 2 → 全保留）→ `--force-deletes` 后删 5 留 1；
4. 冲突副本（4 MiB 本地改动 vs 远端改动）→ 原名=远端、副本=本地（挂载点读出比对）；
5. `status` 显示变更发现/三游标，`cursors.json`/`baseline.json` 落盘可解析。

---

## 9. 已知限制（留给后续）

* **我们的写操作不产生事件** —— 原因**不是**「本机没做设备配对」，而是「账号下没有已登记的同步文件夹」。
  **2026-10-01 真机复核（P0 探针，`report/probe/p0_device_probe.py`）推翻了旧结论**：
  1. NAS 上**确有一台已注册设备** `win-pc`（`qbox_get_device_config_list` 可见，`modify_time`=2026-09-30 11:46，
     `device_uid=01234567…567`）—— 那是**官方 Qsync 客户端**之前注册的，不是 qxync；
  2. 但这台设备的配置是**空的**（`qbox_get_device_config` → `total: 0, config: []`），
     即**没有任何配对文件夹**；
  3. `qbox_get_syncing_folder_list` → `total: 0`，本账号（test1）**从未登记过同步文件夹**；
  4. `qbox_get_sync_log` 对**全部区间**（0–400 / 75–331 / 331）与**全部参数变体**
     （`device_uid` / `duid` / `uid` / `user` / `get_detail` / `sub_folder`）**恒返回 `status:-17`**；
     `qbox_query_notify` 恒 `count:0`。
  → **结论：设备注册不是缺失的那一环**（设备早就注册了，事件照样拿不到）。
  代码注释里的判据才是对的：**只有路径落在「已注册的同步文件夹」里才会真正出现在 `qbox_get_sync_log`**。
  而当前 208 条端点清单里**根本没有「注册同步文件夹」的端点** —— 那是官方客户端在 Qsync Central 里
  做配对时才创建的。所以这条路在现有逆向成果下走不通，**baseline 对账作为主路径的设计必须保持**。
  详见 `docs/M8-向Qsync-Client-6靠拢.md` §11。
* 事件回声过滤目前只有两种手段：`user`/挂载根归属校验 + 「远端==baseline → 无动作」的幂等。
  （原计划「设备配对完成后把本机 `device_uid` 填进 `SyncConfig::own_devices`」已按上面的结论**撤销**。）
* **删除事件无路径**：删除只能等到下一轮对账（默认 30s）才发现。
* 对账按「已知目录」列举：用户没浏览过的深目录不会主动扫（on-demand 的取舍）；
  单轮目录数上限 200。
* config log / global notify **只推进游标不消费**（共享文件夹、团队文件夹语义留给后续）。
* baseline 是 JSON 文件（原子 rename），不是规划里的 SQLite；文件数极大时应迁移到 SQLite
  （M3 一起做元数据持久化时）。
* 冲突只做「远端占原名 + 本地存副本」，不做三方内容合并；目录级冲突按存在性处理。
