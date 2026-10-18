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

落盘位置：`~/.local/share/qxync/sync/<host>/{cursors.json,baseline.json}`（按 NAS 隔离）。

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
| 远端大批删除 → 不要批量清本地 | 引擎一轮对账里 `DeleteLocal` 计数超限 → **整批挡住** | >50 项，或 >25%（baseline≥20 项时） | GUI：提示行/任务卡上的「放行删除」<br>CLI：`qxync sync --force-deletes` |
| 本地大批删除 → 不要清空远端 | FUSE `unlink/rmdir` 的滑动窗口熔断（`DeleteGuard`） | 60 秒内 >100 次 | 同上（一次放行同时解两侧），或重新挂载 |

* 远端删除判定**只认** `status 4/5/6`（不存在/无权限）**且必须由 `stat` 正向确认
  `exist=0`** —— 4/5 同时是权限拒绝 / 会话失效的码，光凭列举错误就删会误删本地；
  网络错误、超时、HTTP 层失败**绝不**当成删除，否则一次抖动就会清空本地。
  `stat` 说仍存在、或 `stat` 也失败 → 该目录本轮不判定；4/5 同时置 `auth_failed` 触发重登。
* 熔断期间被删掉的远端文件**本地节点与缓存都保留**（`ls` 是实时的，但节点/缓存不清），
  避免「远端误删 → 本地缓存也没了」的双重损失。

### 5.1 GUI 侧：出口必须挂在提示自己身上

> 用户反馈（2026-10-04）：「有删除被熔断挡住」这条提示**没地方处理**。

根因不是缺功能 —— 出口一直有（诊断 → 同步 / 缓存 的按钮），而是**提示与出口错位**：
提示里写「待确认」，按钮却叫「强制放行删除」，两者对不上号，还隔着两层导航，
用户只能全局搜。于是定下一条通则：

> **凡是「看到了还得做点什么」的状态，出口就挂在提示自己身上，不让用户自己去找。**

落到实现（`crates/qxync-gui/ui/`）：

| 决策 | 做法 |
|---|---|
| 告警行可带按钮 | `addAlert(containerId, text, severity, action)` 增加可选 `action={label, onClick}`；DOM 上落成 `.alert-act` + `span.alert-text`（`flex-wrap`，长原因下按钮换行而不溢出） |
| 三个提示都带出口 | 主页「最近一次同步」、诊断→状态、诊断→同步/缓存，熔断时都内嵌「放行删除」 |
| 任务卡也给 | 熔断时操作区多一个「放行删除」，由 `taskState()` 的 `blocked:true` 驱动 |
| **按钮不得与状态自相矛盾** | 卡片的「已暂停 / 未挂载」盖掉全局状态时同步清掉 `blocked` —— 否则出现「状态说已暂停却挂着个真会删东西的按钮」 |
| 文案去掉「待确认」 | 改说「**这轮没有真的删除任何文件**」：熔断挡下的是*打算执行*的删除，用户最怕的「文件已删」从不发生，以前界面从没说 |
| 三个入口收敛 | 统一走 `doForceDeletes()`：二次确认，且**确认框里摆 daemon 的原始原因**（「一次对账要删除 812 项，超过上限 50」比「确认放行？」有信息量得多），并提示误清空时别放行。删除不可逆，这步不能省 |

`ui_spec`（`cargo test -p qxync-gui`）会校验新增 i18n 键都在 ZH 表里 —— 改这类
「提示 + 按钮」时最容易漏的就是文案键。

### 5.2 引擎侧：熔断告警只报「变化」+ 恢复后要能消失（2026-10-18 补充）

真机现场（用户机器，v0.5.1）：

```text
每 30s 一条：WARN 删除保护熔断（526 项）：一次对账要删除 526 项，超过上限 50
qxync status：删除被挡 692742          ← 13 小时 × 每 30s × 526 项 = 累计值
journal：3046 条里 3023 条是同一句话  ← 1 万条上限几天就被冲掉
GUI：三处「有删除被熔断挡住」告警一直亮着（问题恢复/放行后也不会消失）
```

根因不在熔断本身（它挡住删除是对的），而在**呈现层三件事**：

1. **每轮都当成新告警**：`reconcile_view` 每轮 WARN + `journal_record_sync` 每轮记一条
   blocked。现在 `SyncReport` 带「数量 + 原因」签名，`absorb_round()` 只在**签名变化**时
   置 `delete_block_changed`；WARN/journal 只认这个标志，重复轮次降为 debug。
   熔断解除的那一轮补记一条「删除保护熔断已解除」，否则更新中心最后一条永远停在熔断上。
2. **「最近错误 / 熔断告警」只写不清**：`last_error`、`last_note`、`delete_block_reason`
   以前只在「有」的时候写。现在统一由 `absorb_round(stats, report, completed)` 结算：
   **完整跑完**的轮次（`completed=true`）零错误才清 `last_error`、`deletes_blocked==0`
   才清熔断告警；在 `max_log` 阶段提前返回的轮次只更新当前错误、不清历史告警
   （没检查过 ≠ 检查通过）。`--force-deletes` 手动放行时同时清 reason 与签名，
   下次再熔断能重新告警。
3. **累计值被当成「当前」显示**：`deletes_blocked` 是 `fetch_add` 的累计计数（只增不清），
   CLI/GUI 标签改为「删除被挡（累计）」；「当前是否熔断」只看 `delete_block_reason`。
   「引擎活着」的 scan journal 条目也从每 30s 一条降到 600s 一条。

同批修的两个**数据安全**问题（都属于「远端缺失」判定的前提）：

* **列举失败不能当远端删除**：旧实现里目录 `client.list()` 网络失败只记一条 error，
  `remote_map` 因此缺项 → 这些路径被判 `DeleteLocal` → 本地节点与缓存被删。
  现在 `reconcile_view` 统计非 404 的列举失败，**该目录下的条目不判定**并在 note 里
  说明，同一视图里其它已列全的目录照常判定。这就是上面 §5 那条「网络错误绝不当成删除」
  的显式实现。
* **4/5 不能直接当「已删」，必须 `stat` 正向确认**（2026-10-18 复核补修）：真机
  `/Download`（存在但当前会话不可见）回 `status 5`、`/homes` 回 `status 4`，而 4/5
  同时是会话失效码。旧实现把 4/5 当「确定不存在」→ 权限/ACL 变更或 sid 中途失效时
  会把该目录所有候选判 `DeleteLocal`（本地节点 + 缓存 + baseline），且不会触发重登。
  现在：列举返回 4/5/6 时先用 `stat` 确认，只有 `exist=0` 才进「已确认不存在」集合；
  stat 说存在 → **目录条目本身记进 `remote_map`**（防止「父目录列举漏了它」时目录自身被
  判 `DeleteLocal`、连整棵子树 + 缓存一起删），目录内容本轮不判定；stat 也失败 → 目录
  存在性未知，目录自身进 `unresolved_dirs`（候选循环与改名配对都跳过），内容也不判定。
  stat 成功本身证明会话有效，所以**不**置 `auth_failed`（否则永久 ACL 隐藏目录会每轮
  重登 + 刷 WARN）；只有 stat 也 auth 失败或根 4/5 才置。远端根例外：挂载根不可能凭空
  消失，根上的 4/5/6 一律按权限 / 会话 / 卸载处理（不做 stat 猜测、不删根下条目）。
  覆盖：`listing_status_4_or_5_never_deletes_without_stat_confirmation`、
  `listing_root_status_4_or_5_defers_instead_of_deleting`、
  `directory_missing_from_parent_listing_but_stat_exists_is_not_deleted`、
  `stat_failure_keeps_directory_and_subtree`。
* **HTTP 层失败不再被读成「空目录」**：`get_list` / `stat` 先检查 HTTP 状态，
  反代 502/504 的 JSON 错误体（`{"error":…}`）不再能通过 `Listing` 的全默认反序列化
  被当成「0 项 / 不存在」；HTTP 200 但形状不是列表/stat JSON（缺 `status`/`datas`/
  `total`/`real_total`/`exist`）同样按解析失败处理；`list_limit` 也不会再把缺失的
  `total` 当 0 在第一页就 break（目录 >200 项时不再静默截断）。
* **目录数超 `max_dirs_per_poll` 时的永久推迟**（2026-10-18 真机反馈后补修）：
  旧实现每轮都从排序开头 `take(max)`，目录数一旦超过上限，排序在后的目录**每轮都
  轮不到**；而「未扫到的目录本轮不判定」这条安全阀因此永久生效 —— 那些目录里的远端
  删除（以及上传 / 刷新判定）永远不会执行（真机 228 个目录时每轮都在重复同一条说明）。
  现在：
  * 每轮**轮转**：远端根恒定参与，其余目录从上一轮停下的目录之后接着列、到尾回绕；
    游标落在 `sync.db` 的 `meta`（`reconcile_scan_cursor:<mountpoint>`），
    `qxync sync --once` 跨进程也接得上；
  * 「视图完整」按**目录**成立：只有本轮列过（列举成功或确定 404）的目录里的候选才判定，
    其余一律暂不判定 —— 不再「一个目录抖动挡住整个视图的删除」；
  * 改名配对的已删侧同样过滤「父目录本轮列过」，不把「没看到」配成一次改名；
  * **固有代价**：目录树远超单轮上限时，每个目录要隔若干轮才轮到一次（上限越小 / 目录
    越多，删除与上传/刷新判定的延迟越大）；轮转只保证「最终会轮到、不会永久漏」。要更快
    就提高上限（`SyncConfig::max_dirs_per_poll`）或缩小同步范围；note 里会写明本轮轮转
    覆盖了多少个目录。
  * 覆盖测试：`scan_window_rotates_and_covers_every_directory`、
    `scan_window_survives_a_deleted_cursor_directory`、
    `reconcile_rotates_scan_window_instead_of_starving_deletions`、
    `listing_failure_only_defers_its_own_directory`、
    `listing_status_4_or_5_never_deletes_without_stat_confirmation`。
* **baseline 的删除/改名永远落不了盘**（pre-existing）：`persist()` 只按 mtime 取较新
  签名做增量合并，从**本轮快照**里删掉的条目会被内存里旧 `g.baseline` 复活 → 下一轮
  又写回库。于是「规则排除清理」「回收站清理」「已执行删除的 baseline」「改名迁移的
  旧路径」全部每轮重复出现。现在改为「本轮没有并发 hook 写 → **整份替换**（删除/改名
  真正落盘）；有 hook 并发写 → 退回逐条合并」，并用重开库的测试钉住落盘。
* **回收站不进 baseline/对账**：真机 baseline 1991 条里 1425 条在 `/home/@Recycle` 下、
  13 条在 `.Trash-1000` 下（还有 FDO `.trashinfo` 被反向同步到 NAS）。
  `@Recycle` / `.Trash` / `.Trash-<uid>` 现在从对账、baseline、改名配对、事件处理里
  全部排除（判定在 `qxync-core::trash`，与 FUSE `deny_trash` 同一套段级语义）；
  baseline 里已有的回收站条目做一次性清理（**只清状态，不动文件**）。
  QTS 清理自己的回收站时，不会再在本端产生成批「远端删除」候选。

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
* **轮询**：守护进程后台任务，默认 30s；`QXNYC_POLL_INTERVAL=0` 暂停。
* **IPC**（`qxync_core::ipc`）：
  * `sync {once, force_deletes, max_deletes, interval_secs}`；
  * `rm {dir,name}`（测试/脚本用）；
  * `mount {…, delete_limit}`（本地删除熔断阈值，0 = 关闭）；
  * `status.sync` = `SyncInfo`（游标 / baseline 条目数 / 计数 / 事件设备 / 熔断原因）。
* **CLI**：`qxync sync [--once] [--force-deletes] [--max-deletes N] [--interval S]`、
  `qxync rm <dir> <name>`；`qxync status` / `qxync mount` 打印上述信息。

---

## 8. 验收

**真机集成测试**（`#[ignore]`，需环境变量）：

```bash
export QXNYC_TEST_HOST=... QXNYC_TEST_USER=... QXNYC_TEST_PASSWORD=...
export QXNYC_TEST_FIXTURE=/home/qxync-test
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
  **2026-10-01 真机复核（P0 探针，`xtask/probe/p0_device_probe.py`）推翻了旧结论**：
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
