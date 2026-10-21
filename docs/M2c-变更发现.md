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
| 事件路径 | 真实路径 `/share/homes/<user>/...`，需映射到视图 `/home/...`。视图根是家目录**子目录**时尤其注意：事件路径先换算到 `/home` 命名空间、再按视图根收窄（见 §10.1） | `map_event_path()` + 归属校验 |
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
  * 覆盖测试：`scan_window_rotates_and_covers_every_directory`、
    `scan_window_survives_a_deleted_cursor_directory`、
    `reconcile_rotates_scan_window_instead_of_starving_deletions`、
    `listing_failure_only_defers_its_own_directory`、
    `listing_status_4_or_5_never_deletes_without_stat_confirmation`。
* **时间预算驱动对账扫描**（v0.6.8，2026-10-19 真机反馈后重设计）：轮转只保证「最终
  会轮到」，但**一轮该列多少目录**以前写死成 200（`max_dirs_per_poll`），跟轮询间隔
  完全脱钩 —— 真机 1439 目录 × 600s 间隔，全量列举只要 1~5 分钟，却硬掰成 8 轮轮转，
  远端删除最坏要 **80 分钟**才落地（而该账号事件快路径是死的：`qbox_get_sync_log`
  无新事件，对账就是主路径）。现在一轮能列多少由「这轮有多少时间」决定：
  * **预算** = 轮询间隔 × 50%（`SCAN_BUDGET_RATIO_OF_INTERVAL`），每轮推导（间隔
    运行时可调）；`QXNYC_SCAN_BUDGET_SECS` 可显式覆盖（`0` = 不限）；
    `sync --once` / GUI「立即同步」给足 600s（`SCAN_BUDGET_ONCE`）—— 手动场景就是想全量；
  * 预算没用完 → 窗口列完即**全量**（删除判定延迟 = 一个间隔）；预算耗尽 → 到点即收，
    游标 = 最后一个列过的目录，下轮续扫（退化为轮转，但每轮耗时可控）；
  * **进度保证**：每轮至少列一个轮转目录（预算检查从第二个开始）—— 否则「预算 <
    一次列举耗时」的极端配置会让根吃光预算、游标存不下，饿死换个马甲回来；
  * 游标存在就进轮转（不再要求「目录数 > 硬顶」）：时间预算耗尽留下的游标若被忽略，
  排序在后的目录在时间预算下永远轮不到；列完全部时**清游标**（不清的话下轮带着游标
    白转一圈）；
  * `max_dirs_per_poll` 降级为**硬顶兜底**（防目录数爆炸拖死单轮），默认 200 → 2000；
  * 回绕补齐不超过全集（预算 ≥ 全集时窗口 = 全集的旋转序，不重复列目录）；
  * 覆盖测试：`generous_time_budget_scans_everything_in_one_round`（预算充足一轮
    全量、游标清空）、`tiny_time_budget_still_makes_progress`（1ns 预算下每轮推进
    一个、游标逐轮前进、未轮到的不误删）。
* **首次同步 / 太久没同步 → 全量列举**（2026-10-21 真机反馈后补；详见
  [M29](M29-首次同步与全量重扫.md)）：轮转窗口解决的是「稳态下每轮耗时可控」，
  但它不该用在**第一次同步**上 —— 4964 个目录 × 每轮 ~171 个 = 29 轮 ≈ 24 分钟，
  期间所有删除判定都被推迟，用户看到的是「永远在暂不判定」。
  现在按挂载点记状态（`reconcile_full_done:<mountpoint>` / `reconcile_full_pending:<mountpoint>`）：
  * **首次同步**（无 done 标记）→ 一轮 BFS 列完整棵树（每目录只列一次，不受轮转游标 /
    常规单轮预算 / 2000 硬顶限制），完成后落 done，**恢复常规轮转与用户配置的间隔**；
  * **太久没同步**（距上次**完整跑完**一轮 ≥ 1h，`sync_last_ok_unix` 判据）→ 按首次同步处理；
  * **上次没列完**（崩溃 / 断网 / 预算用完）→ pending 标记，下轮继续；
  * 全量单轮预算 1800s（`QXNYC_FULL_SCAN_BUDGET_SECS`），用完则轮询循环 2 秒后继续；
    完成状态在**删除判定之前**落盘（删除熔断不该导致每轮重扫整棵树）；
  * `QXNYC_FULL_SYNC=0` 可整体关掉（退回 v0.7.7 纯轮转，排障对比用）。
  * 覆盖测试：`full_scan_plan_triggers_on_first_pending_or_stale`、
    `full_scan_lists_whole_tree_in_one_round`、
    `full_scan_budget_exhaustion_keeps_pending_and_retries`、
    `full_scan_after_long_gap`、
    `poll_once_records_last_success_and_resumes_normal_polling`。
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

---

## 10. 修正（2026-10-19，真机事故复盘）

一次真机操作把**三个互相独立**的问题同时暴露出来：08:05–08:06 用户连续保存
`…/28-比赛项目/01配置备份/防火墙配置.txt`（事件里可见编辑器交换文件），08:06:47 该文件被判冲突并生成了副本；删掉副本之后，
后续事件仍每轮报 `事件 stat … 失败: status=5 (路径不存在 / 无权限)`。
取证来源：daemon 日志（`$XDG_STATE_HOME/qxync/log/qxyncd.log.2026-10-18` / `…-10-19`）、
`sync.db` 的 `journal` 表（`$XDG_DATA_HOME/qxync/sync/<host>/sync.db`）、
`mounts.json` 与任务配置（`root=/home/.Qsync`，任务写 `conflict=ask`）。

### 10.1 事件路径双写：`/home/.Qsync/.Qsync/…`（事件快路径整体失效）

* **症状**：每条事件 stat 都 `status=5`（journal 每轮最多保留 20 条，看到的全是它）；
  事件快路径等于不存在，远端变更只能等 baseline 对账。真机取证两条：`journal` 里
  2026-10-18 17:31 那一轮**拉到 87 580 条事件、跳过 8 017 条**（`remote_change` 条目
  原文）；当天 daemon 日志（80 479 行）里 `事件 stat` 共 **79 670 条，100% 命中双写
  前缀** `/home/.Qsync/.Qsync/`，其中 79 667 条 `status=5`、3 条网络错误。
* **根因**：`map_event_path(real, user, view_root)` 把「剥掉 `/share/homes/<user>` 后
  剩下的部分」直接拼到 `view_root` 上。视图根**就是**家目录（`/home`）时这个巧合成立；
  视图根是子目录时（`/home/.Qsync`，NAS 上就是 `~/.Qsync`），剩下的部分本身以
  `/.Qsync` 开头 → 拼出 `/home/.Qsync/.Qsync/2026/…`（`.Qsync` 两遍），NAS 上当然没有
  这个路径。
* **修法**：先按 `qxync_core::roots::client_path_from_share` 把真实路径换算到客户端
  命名空间（前缀恒为 `HOME_ROOT` = `/home`，同时认 `/homes/<user>` 与卷内
  `realpath` 形态、拒绝他人路径），再要求结果落在本视图根之下；视图外/归属不符 → `None`
  （交对账兜底）。**不要**在事件侧另造一套前缀逻辑，两处前缀规则分叉就是这次 bug 的来源。
* **顺带**：事件 `stat` 的 4/5/6 改为 debug + `events_skipped`，不再 `report.error` ——
  这三个码同时是权限拒绝/会话失效，真机上不该按硬错误刷屏；并且**不置 `auth_failed`**
  （`is_auth()` 把 4/5 当会话失效，给已删文件的事件置它会每轮重登，真机会撞 NAS 的
  失败登录封锁）。跳过事件现在按原因计数，写进 `remote_change` 的 journal 详情
  （删除事件无路径 / 本机回声 / 非本账号 / 被规则隐藏 / 远端不可见 / 映射失败）——
  4/5/6 降级后 `events_skipped` 是唯一痕迹，不归因的话「事件快路径整体失效」会变成
  无声故障。
* **回归**：`qxync-core` 单测 `event_path_under_home_subdir_is_not_double_prefixed`
  （真机原样中文路径、视图根自身、视图外路径、realpath/短形态、他人路径、空 user、
  `/share/Public` 共享路径、`/home/.Qsync` vs `/home/.Qsync2` 前缀撞车）+
  `qxync-daemon` 的事件级端到端测试
  `apply_event_stats_single_prefixed_path_for_home_subdir_view`（真 `Client` + 真 HTTP +
  mock NAS：断言实际请求是单前缀 `stat:/home/.Qsync/…`、永不出现双写路径、三方一致
  时 Noop 且节点仍是文件、4/5 只跳过不记错误不重登、跳过归因到
  `events_skipped_reasons["远端不可见(4/5/6)"]`）。注意这个测试覆盖的是**本节**的
  路径映射与 4/5/6 降级；hook 推进时序的回归见 §10.2。

### 10.2 凭空冲突副本：本轮上传 hook 推进了 baseline，判定却用开轮快照

* **症状**：连续保存（每次保存都触发一次上传落地）之后凭空出现
  `防火墙配置 (conflicted copy from <本机名> 2026-10-19).txt`，且**与原文件字节完全相同**
  （12218 = 12218）。副本名里的主机名就是本机 —— 是自己跟自己打出冲突。
* **根因**：`poll_once` 开轮时克隆 baseline 快照，整轮（几十秒、要列几百个目录）都用它
  判定；上传成功 hook 却在**本轮进行中**就把权威 baseline 推进到刚落上去的签名
  （`SyncState::note_uploaded`）并 `clear_dirty`。于是判这个文件时：
  * 快照 base = 上一版（旧 mtime）；
  * remote = 自己刚传的新版（与本地相同）；
  * local = 新版、`dirty=false`（hook 已清）；
  → `remote != base` 成立、`local != base` 也成立 → `decide` 的 `(false, true)` 分支判
  **Conflict**。旧修复（`note_uploaded` 推进 baseline + `merge_round_baseline` 落盘合并）
  只堵了「上传落地 → 下一轮开始」这个窗口，**漏了一轮进行中**这个窗口；而且它自己的
  落盘合并也有洞（见下面「复审修正」第一条）。
* **修法**：`SyncState` 新增本轮覆盖表 `round_hook_advances`
  （路径 → **本轮按时间顺序的全部推进签名**，同签名去重，每路径封顶 16 个 ——
  `QXNYC_POLL_INTERVAL=0` 禁用轮询时表永不清，不设上限会无界增长），
  `begin_round()` 在**取快照的同一把锁**里清空它；对账、事件、冲突重判统一走
  `decide_with_state()/base_for()`：base 规则如下（窗口都要照顾）：
  1. 远端列到的 == 开轮快照，或 == 本轮任一推进 → `base = remote`（`rc = false`）。
     目录列举可能落在本轮两次上传**之间**，所以必须匹配整张列表，不能只比最新那次；
  2. 远端比本轮最后一次推进还旧（mtime 更小 / 不存在）→ 也是陈旧列表，`base = remote`
     （本轮 `Noop`，下一轮用权威 baseline 收敛），**不能**拿来 `RefreshRemote` 把本地
     元数据/内容刷回旧版；
  3. 「远端比最后一次推进还旧」这条捷径**只在本地干净时生效**（真机事故就是
     clean）：本地带未上传改动时退回 `base = 最新推进`，让 `(true, true)` 走
     Conflict —— 否则别处设备「mtime 偏小的新写入」（时钟慢 / `rsync -t` /
     备份恢复）会被 `UploadLocal` 直接覆盖，丢数据；
  4. 其余（别处设备改的新版）→ `base = 本轮最后一次推进`（没有推进就是快照），
     正常 `RefreshRemote`。

  另有**两处复审修正**（第一版修法自己的洞）：
  * `merge_round_baseline` 不再在第一次合并后清 `hook_touched`：一轮内 persist 会发生
    多次（事件阶段每批一次 + 轮末一次），清掉后第二次走整份替换，用「hook 推进前
    克隆的旧快照」把刚写上去的签名在盘上抹掉 —— 本轮内存里看着修好了，下一轮开轮
    快照又回到旧签名，**同一个凭空冲突晚一轮重现**。现在标记只在 `begin_round`
    复位（本轮有 hook 写入时删除/改名迁移晚一轮落盘，取舍已在 persist 文档声明）。
  * 上传 success hook 的顺序改为**先 `note_uploaded` 再 `clear_dirty`**：反过来的话
    中间有个窗口（local 已 clean、base 还是旧快照、覆盖表还没记），对账恰好判到
    就是凭空 Conflict；先推进则窗口内是 `dirty=true + base=新签名` → AdoptBaseline，
    安全。
* **回归**：`hook_advance_during_round_prevents_phantom_conflict`（复刻真机时序：
  旧逻辑必现 Conflict、新逻辑 Noop；并逐条锁住上面的窗口 1/2/3/4 —— 只认最新推进
  会误判 `RefreshRemote`、陈旧列表不能回刷、**本地脏 + 远端旧钟新写入必须判
  Conflict**、别处设备的新版必须照常刷新）、
  `reconcile_uses_in_round_hook_advance_not_the_stale_snapshot`（穿透真
  `reconcile_view`：不出冲突、不入待裁决队列、权威 baseline 不回退）、
  `persist_keeps_hook_advances_across_repeated_persists_in_one_round`（同轮多次
  persist + 重新 load 仍是 hook 的值）、
  `begin_round_folds_previous_advances_into_snapshot_and_resets_table`（生命周期 +
  多次推进去重）、`round_hook_advances_are_capped_per_path`（封顶）。

### 10.3 冲突策略与实际不符：任务写 `ask`，实际一直按 `rename_local` 走

* **症状**：`decisions` 表是空的、也没有「等待裁决」日志，但副本确实生成了 ——
  说明生效策略不是任务里的 `ask`。
* **根因**：冲突策略由同步引擎按 **`MountEntry.conflict`** 分派（`build_views` 直接取
  mount 的 `conflict`，视图不读任务文件），来源有两处：
  * 任务挂载时 `mount_task` 传的是 `t.conflict`；
  * daemon 启动恢复时 `restore_mounts_on_start` 传的是 **`mounts.json` 里的记录值**
    （设计上「现场优先」）。
  而保存任务（`tasks_cmd` 的 `save`）**不回写已挂载视图、也不更新记录**。真机现场因此是：
  任务 10-18 17:27 改成 `ask`，`mounts.json` 仍是更早的 `rename_local`；daemon 10-19
  00:05 重启后按记录恢复 → 08:06:47 的冲突落 `match policy` 的 `_` 兜底分支
  `rename_local_copy`，生成了副本（`08:06:46.970` 的 WARN 与 `rename_local_copy`
  的唯一格式串逐字一致；副本上传在 `08:06:47.662`，距冲突 0.69s）。
* **修法**：① `tasks_cmd` 保存任务后，若同路径已挂载，立刻把新策略写进
  `MountEntry.conflict` 与 `rec.conflict` 并落盘（免重启生效、重启后不回退）；
  ② `restore_mounts_on_start` 的**冲突策略例外**：任务表里有同名任务就用任务里的值
  （其余挂载参数保持「现场优先」不变）——兜住「直接改任务文件 / 保存时没挂载」的情况。
* **未改的边界**：若用户手工 `qxync mount --conflict X` 挂在一个**已登记任务**的
  挂载点上，下次重启会按任务里的策略恢复（任务 = 意图）；不想要这个行为就先改任务。

### 10.4 仍保留的相关限制（未在本次修改）

* 副本名里的设备名是**本机 hostname**，不是「文件来自哪台设备」；多端同名时无法区分。
* 编辑器临时文件（`.kate-swp` / `*.swp` 等）是否进同步由 `filter_temp` 规则决定，
  本次未改（双写路径修好后它们不再刷错误，但仍会被当成普通文件同步一次）。
* 共享文件夹（`root=/Public`）的事件仍映射为 `None`（`client_path_from_share` 只认
  `/homes/<user>` 标记；旧实现同样 `None`，不是本次回归）。NAS 对共享文件夹事件发
  `/share/Public/…` 还是 `/Public/…` 尚未实测，确认后补映射与测试。
