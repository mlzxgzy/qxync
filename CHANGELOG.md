# 变更日志

本文件格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)。

[English](CHANGELOG.en.md) · [README](README.md) · [验收记录](docs/验收记录.md)

## [Unreleased]

## [0.5.1] - 2026-10-04

**修两个 GUI 上「看着正常、其实功能不工作」的问题**：单文件脱水永远返回 0，
文件表格最右一列看不见。

### 修复

- **GUI 文件页点单行「脱水」永远返回「0 项，释放 0 B」**（一个文件都没清掉）。
  根因两层：
  1. `doDehydratePath` 只发 `{method:'dehydrate', path}`，没带 `force`。daemon 的
     `let recent = if opts.force { 0 } else { 300 }` 于是给它套上 300s「刚访问过」
     保护窗口 —— 而「刚下载 / 刚读完就点释放」恰恰是最想省空间的场景，等 5 分钟
     才能脱自己的水。CLI 早有 `--force`（M3 文档 §5 明写是「手动脱水的既有开关」），
     GUI 这条路径没有等价入口，所以测试矩阵全靠 `--force` 才跑得通、缺口一直没暴露。
     现在恒带 `force: true`；**只跳「刚访问过」这一项**，pin / 未上传改动 / 打开的 fd /
     mmap / 在途 / 上传队列一条都没松。
  2. 日志把原因吞了：只打 `dehydrated`/`freed_bytes`/`used_bytes`，完全没打响应里
     本就带了的 `blocked`（`DehydrateData.blocked`）。用户无从分辨是被保护窗口挡下、
     是 pinned，还是这个路径压根不在任何挂载点的远端根下（后者同样返回全 0）。
     现在追加「｜被挡下：<原因>」，并按结果分三档打点：成功走 `logOk`、
     0 项+有原因走 `logErr`、0 项+无原因走 `logInfo`。新增 `dehydrateReasons()`
     兼容两种 `blocked` 形状、原因去重、超 3 条折成「等 N 类」（目录级脱水上百条）。
- **操作日志的状态色一直是死的**：`.logline.ok/.err/.info` 的颜色挂在 `.res` 上，
  而 `logLine` 追加的 span 类是 `.t` / `.c`，DOM 里从来没有 `.res` 元素 —— 三种状态
  全渲染成同一个灰，「失败 / 被挡下」在视觉上完全看不出来。改成匹配 `.c`
  （specificity 更高，会覆盖前一条 `--fg-dim`），并给 `.c` 补
  `flex: 1 1 auto; min-width: 0` 让长文本正常换行。
- **GUI 文件页表格看不全，最右侧「pin 状态/操作」列整列被切掉**：默认 1200px 窗口下
  就要横向拖才能看到行尾，溢出部分**连滚动条都不出现**（被卡片裁掉了，静默丢内容）。
  根因三条叠加，都不是「窗口太小」：
  1. pin 列在**每一行**渲染 `<select>` + 4 个按钮（查 pin / 设 pin / 下载 / 脱水），
     单列约 370px —— 而这四个按钮在行右键菜单里**都已有等价项**，纯冗余。
  2. 表格没有横向滚动容器：`#main` 虽有 `overflow:auto`，但表格被卡片约束在卡片宽度内，
     多出来的部分直接消失。
  3. `have_child` 独立成列，而 QNAP 对目录几乎恒返回 0 → 该列长期全空，
     白占约 80px 还把文件名列挤窄。

  修法：pin 列收成「窄下拉 + ⋯ 按钮」，四个操作全部收进行右键菜单
  （并补上原本缺的三项：查询 pin 状态 / 设为 unpinned / 排除）；表格包一层
  `.tbl-scroll{overflow-x:auto}` 兜底；`.tbl-files` 改 `table-layout: fixed` +
  显式列宽，文件名列是唯一弹性列、长名单行省略并用 `title` 给全名；
  `have_child` 并进类型列（目录名后加 ▾），信息不丢且省一列。
  实测内容区逐级收窄的横向溢出量：1136→0 / 1000→0 / 900→0 / 800→0，
  700→66、600→166（此时才退化为滚动条）—— **最小窗口 960px 下完全不溢出**。
  `openRowMenu` 新增第 4 参把该行下拉带进菜单状态，改完 pin 会回填它，
  不会出现「设了 pinned、下拉还显示（未知）」。

### 不做

- **「全部脱水」按钮的 `force` 保持 `false`**：批量操作里刚访问过的文件正是最该保留的，
  与单文件按钮语义不同。它在面板里本来就带完整 `blocked` 列表，可观测性没问题。

## [0.5.0] - 2026-10-04

**同步引擎不再牵连挂载点 + 远端改名不再凭空多出冲突副本**：这一版把「同步」和「挂载」
在生命周期上拆开，并给每个文件安了一个本地稳定身份。

M15 清单的 T1–T9 到此收口（T4 验证后取消，理由见文末）。

### 新增

- **`systemctl --user reload qxyncd`（SIGHUP）：重载同步但挂载点一秒不中断**。
  新增 `qxync daemon reload` 走同一条路径。实现是给同步引擎发一个「代次令牌」，
  旧任务在下一个检查点自己退场 —— 不用 `abort()`，正在等网络 IO 的任务不会把退出拖住。
  ★ **这是「重启不掉挂载」真正能用的路径**：systemd 的 `restart` 是
  「stop → 等旧进程退出 → start」，一个不退出的守护进程会被 `TimeoutStopSec`
  之后的 SIGKILL 杀掉，挂载照样断 —— 这是 systemd 的语义，不是 qxync 能绕开的。
- **SIGTERM 保留挂载**：IPC `stop` / `SIGINT` 仍然真正卸载全部挂载（用户明确要「关掉」），
  但 SIGTERM 现在只停同步任务、写盘、转入挂载守护模式。守护进程有 4 个退出条件
  （IPC stop / SIGINT / 第二次 SIGTERM / 挂载表空）加一个可选的 `--mount-hold-secs`，
  不会把 systemd 卡死。
- **僵尸挂载自动清理**：真崩溃或 OOM 之后，挂载点会留在 `/proc/mounts` 里但 `ENOTCONN`，
  `cd` 进去报「Transport endpoint is not connected」。现在 daemon 启动早期会
  对**自己记录过的**挂载点写探针试活，确认已死才 `fusermount3 -uz` 懒卸载。
  安全边界：只清 `mounts.json` 里记过的路径（不扫全盘）、校验 subtype 是 `qxync`
  才动手、用 `-uz` 而非 `-u`、失败只 WARN 不阻塞启动。
- **稳定文件身份 `file_id`（16 字节，本地自建）**：服务端 `DirEntry` 没有 id 字段，
  所以身份由 `(mtime, size)` 推出（目录另算，**刻意不含文件名** —— 含了改名后身份就变了）。
  新增 `nodes` 表（schema v3 → v4），删除即清理索引。
  - **对账据此认出「远端改名」** → 本地节点跟着改名、baseline 迁移，
    **不再生成冲突副本**。这是 0.4.x 里最烦人的 phantom copy 的根因。
  - **上传队列排队期间远端改名**，作业落地到新名字，不再产生幽灵文件。
  - **配对是保守的**：身份未知、类型不符、一个身份对应多个路径（有歧义）一律不配，
    退回原有的增删判定。宁可多冲突，绝不错配或丢数据。
- **`qxync file-states <dir>`**：逐文件看同步状态。`in_sync` 是**派生值**
  （`dirty` / 待传作业 / 待裁决冲突三个信息源汇总），不新建状态表。
- **传输进度**：下载每收完一个分片（128 KiB）回调一次，上传直接用 T2 那个
  「真正发出的字节数」推进。进度上报**只读**且**绝不冒泡**到读/上传路径
  （回调 panic 用 `catch_unwind` 隔离）。

### 修复

- **`rename_remote` 漏迁移身份索引**（本版内部抓到并修复）：对账认定的远端改名迁移了
  本地节点，却没把 `nodes` 表那一行一起搬。于是同一个 `file_id` 出现两行 →
  按身份反查恒定返回「有歧义」→ **上传队列的按身份重定向永久失效**，
  且每次远端改名泄漏一行索引。已修，并补上能抓住它的回归测试
  （用变异测试确认过：撤掉修复，该测试立刻报红）。
- **`progress_percent` 整数溢出**：`done > total` 时 `saturating_mul(100)` 会输出
  `18446744073709551615`。已改用 `u128` 中间量并夹到 100。
- 为此把 `Store` 的 SQLite 连接改成 `Arc<Mutex<Connection>>`：FUSE 侧现在也要拿
  一份 `Store`（做身份索引），必须是**同一个连接**而不是各开一个抢写锁。

### 变更

- **默认的「重启不掉挂载」路径从 `systemctl restart` 换成了 `systemctl reload`。
  两者语义不同，别混用**：`restart` 仍会短暂断开挂载（由 T5 的自动重挂恢复，
  通常 3 秒内）；要的是**一秒都不中断**就用 `reload`。systemd 单元的注释已同步更正
  ——原先那句「qxyncd 收到 SIGTERM 会卸载全部 FUSE 挂载点」在 0.5.0 之后已不准确。
- 停止守护进程请用 `qxync daemon stop`（走 IPC，真正卸载挂载）或
  `qxync umount`（只卸挂载点）。

### 不做

- **T4 上传分片续传 —— 验证后取消**。原以为「服务端不支持分片」，实测是**错的**：
  `qsyncsrv.cgi` 里分片上传实现得很完整（5 个 func、参数表齐全）。真正的障碍是
  **分片通道只服务 Qbox 空间**（`upload_root_dir` 认 `/remote:` 前缀），
  而 `upload.php` 的 `dest_path` 用的是普通家目录路径 —— **两套路径空间不通**，
  我们够不到。探针留档在 `xtask/probe/upload_chunk_probe.py`。
  顺带纠正一个隐蔽的坑：该 CGI 的响应是**两段 JSON**，第一段 `"status": 0` 是框架头，
  只抓第一个会误判成「所有 func 都无反应」。

### 验证状态

- `cargo check --workspace --all-targets` 零 error 零 warning。
- `cargo test --workspace --lib --bins --tests` **290 passed / 0 failed**
  （基线 265 + 本批新增 25）。被 `#[ignore]` 的 19 条全部在 `qxync-proto-test/`，
  需要真实 NAS —— 本批没有新增任何 ignore。
- ⚠️ **本机没有 `/dev/fuse`**，所以凡是「真的挂上去再操作」的验收项
  （reload 期间 `ls` 是否正常、`kill -9` 后能否自愈、SIGHUP 期间 FUSE 会话是否真没断）
  **都只有逻辑层单测覆盖，没有真机验证**。T6/T7/T9 各节里逐条标了「未验」。

## [0.4.3] - 2026-10-04

**挂载点不再有回收站 + 删除不再干等**：删文件时弹进回收站、删完要等半天，
这两个问题来自两处，都在这一版解决。

### 修复

- **挂载点里的回收站被拒之门外**。回收站不是 qxync 建的 —— 代码里从来没有回收站
  逻辑（KDE Dolphin 走 FDO 的卷内回收站协议，把 `.Trash-$UID/` 造在挂载点里，
  内容最终落在 NAS 上）。现在 FUSE 写入口对回收站命名（`.Trash` / `.Trash-<uid>` /
  `@Recycle`）一律回 `EPERM`：**回收站目录建不出来，KIO 就无法启用卷内回收站，
  只能走它既定的「直接删除」回退路径**。守卫挂在 `mkdir` / `create` / `rename` /
  `unlink` / `rmdir` 五个入口上，判定是**段级精确匹配** —— 不会误伤 `.recent`、
  `.trash`、`a.Trash-1000.txt` 这类正常名字。
  （KIO 6.30 已删除 `DisabledFor` 键，配置层面本就无解，只能在文件系统层挡。）
- **`rm -rf` 父子删除冲突**：内核会同时把 `rmdir(dir)` 和 `unlink(dir/子)` 交给
  删除队列。父目录删掉后再删子项必然报「不存在」，白重试到放弃、让失败计数虚高。
  现在发请求前剔掉「父目录同批被删」的子项，前缀匹配按**完整路径段**判断
  （所以 `victim` 被删不会影响 `victim2.txt`）。
- **删除重试计数被重置**：原先重试时拿批内**最大**次数覆盖每项计数，已重试 4 次的项
  会被打回 1 次，永远达不到放弃阈值。现在逐项 `+1`、各自判定放弃。

### 新增

- **删除队列**（`qxync-fuse/src/delete.rs`）：`unlink`/`rmdir` **立刻返回**，
  远端删除由后台 worker 攒批推送。之前是 `rt.block_on(delete_entry(..))` ——
  unlink 要等 NAS 回包才返回，删 N 个文件就是 N 次串行往返。
  - **同目录合并成一次请求**：`qsyncsrv.cgi?func=delete` 的 `file_total` 本就是
    「本批条目数」（`stat`/`set_mtime` 硬编码成 `1` 只因那些调用每次一个文件），
    现在按目录分组、单批上限 50、攒批窗口 200ms。
  - **崩溃恢复**：作业落 `deletes` 状态库（与上传队列同一个 `queue.db`），
    语义一致 —— **表 = 未完成作业**，重启自动重新入队。
  - `status` 新增一行「删除队列」；卸载时排空 120s。
  - **对齐官方语义**：Qsync 官方客户端的删除（Smart Delete）是
    *"deleted files on the local device are retained on the NAS"* —— 本地立即生效、
    服务端侧异步推进。本队列就是照这个形态做的。

### 安全性

- **删除熔断仍在入队之前**：`DeleteGuard`（100 项 / 60s）照旧生效，超阈回 `EACCES`。
  大批量误删不会因为改成异步就悄悄溜进队列。
- 异步删除的取舍是「本地是权威」：入队即返回，远端失败时本地文件已消失。
  失败重试到超限会记入 `status` 的失败计数与 daemon 日志，运维可据此人工补删。

## [0.4.2] - 2026-10-04

**连着保存两次不再凭空出冲突副本 + 保存提速**：这一版修的是一个用户一眼就能撞上的
假冲突（连着保存两次同一个文件会多出冲突副本），顺带把「一次保存」的等待时间从
**随文件大小线性增长**压到约 1/8。

### 修复

- **冲突副本的真正原因（不是「两次保存该合并」，是 baseline 漏了一步）**：
  baseline 记的是「上次同步成功时的**远端**签名」，之前只有同步引擎在一轮轮询里
  观察到远端后才会推进它。而我们自己上传成功后，success hook 只清了 `dirty`，
  没动 baseline。于是从「上传落地」到「下一轮轮询」之间有**最长 30s**的窗口，
  窗口里本地 == 远端 == 新版本、baseline 还是老版本：`decide` 看到
  「远端变了 + 本地没标脏」，又发现本地签名 ≠ baseline，判成 **Conflict**。
  也就是说**连着保存两次，第二次落地后等一轮轮询就会长出一个冲突副本**。
  现在上传成功时就把 baseline 推到刚写上去的那个签名（`note_uploaded`），
  这个窗口不存在了，两次保存各自就是各自的增量。
  真正的双方都改仍然照常判冲突（`decide` 决策表一行没动）。
- **上一条还有个反向的坑，一并修了**：一轮轮询是「克隆一份 baseline 快照 → 跑
  很久的网络 IO → 落盘」，落盘时原本是**整份盖回**这份开轮询时的旧快照 ——
  轮询期间 hook 刚推进上去的签名会被抹掉，正好把上面的修复自己撤销掉。
  现在落盘改为**合并**：只接受比内存里更新的条目（mtime 更大者为新）；
  mtime 相等时保留内存里那份（epoch 秒粒度下同秒连改两次很常见）。
- **保存慢：写之前要把整个文件的缺块取齐，原本是一段一段串行取的**。
  read-modify-write 要求整份内容本地齐了才敢落笔（否则未取回的区间是 0，
  整文件上传会把远端内容清零），所以一次保存的开销 = 全部缺块 × RTT，
  **随文件大小线性增长**（128 KiB 一块：1 MB 文件 8 个往返、4 MB 文件 32 个）。
  这些块彼此独立，现在按 8 路并发取，墙钟从 O(块数×RTT) 降到 O(⌈块数/8⌉×RTT)，
  1 MB 约 8 倍提速。并发取值 `QXYNC_HYDRATE_FANOUT`（1–64，默认 8，设 1 即退回串行）。
  正确性不受影响：每块各自有去重锁，且内容先于位图落盘，不会出现「位图说有、
  内容却是半截」。
- 顺带修掉上传回调的一个竞态：上传完成后统计字节数时读的是**那一刻磁盘上**的
  大小，而两次保存挨得近时作业可能早就取走了，读到的是**下一版**的大小 ——
  那个数字会被用来推 baseline，等于把 baseline 停在一个远端并不拥有的签名上。
  现在回调拿到的是**这次真正发出去的字节数**（`upload_one` 返回值）。
  另外，如果上传期间本地又变了，就**不**推进 baseline，等下一版落地再推一次。
- **「打开目录」按 `xdg-mime` 查到的桌面项 ID 直接 spawn 必然失败**：
  `xdg-mime` 返回的是 `org.kde.dolphin.desktop` 这类**桌面项 ID**，而真正能跑的
  程序是它的 `Exec=` 里写的 `dolphin`（`org.kde.dolphin` 在 `PATH` 里根本不存在）。
  开发机实测：直接 spawn ID 得到 **127「没有那个文件或目录」**，解析 `Exec` 后正常启动。
- **`setsid` 失败会让「打开目录」整个不可用**：`setsid()` 在**已是进程组组长**的进程里
  返回 `EPERM`，而 fork 出的子进程有可能仍然是组长。原本把它当致命错误，于是在这类环境里
  按钮**必然点不开**。现在改为**尽力而为**：脱离会话只是锦上添花，失败也照样把程序拉起
  （退化成与 GUI 同会话，与 opener 插件行为一致）。

### 新增

- `QXYNC_HYDRATE_FANOUT`：并发取块的扇出（1–64，默认 8）。
- **任务卡「打开目录」按钮**：每张任务卡（主页与任务页）多一个「打开目录」，
  点一下就用**系统里用户自己配的默认目录工具**打开该任务的**本地挂载点**。
  按 `xdg-mime query default inode/directory` 查用户配的默认目录工具，按 XDG 规范
  找到对应 `.desktop`、读 `Exec=` 取出**真正可执行的程序名**，`setsid` 脱离后拉起
  并把路径交给它。查不到 / 起不来时回落到原来的插件 opener（`xdg-open` 优先），
  并把实际走了哪条路回报给前端（`via`），点开没反应时能看出原因。
- 任务卡没有本地挂载点时按钮**禁用**并说明原因，而不是点了没反应；挂载点不存在
  （NAS 掉线、任务停用后自动卸载）时如实报错，**不会** `mkdir` 造一个空目录
  ——造出来会让用户以为文件真的在本地，正好掩盖最该看见的故障。

## [0.4.0] - 2026-10-03

**缓存优先 + 映射本地化**：磁盘上已经缓存好的内容，重建节点后**认领即用**
（水合位图随内容落盘），缓存命中的 `cat` 不再有 NAS 往返；`ls`/`lookup` 改吃
**NAS 文件列表的本地快照**（由变更轮询定时刷新），不再每次等网；远端内容变了时，
本地**有水**的文件留着旧内容继续可读、后台整份拉新版本**原子换上**（换上前 `cat`
拿到的是完整旧版本、`stat` 也是旧大小），**脱水**的文件只更元数据。
设计与真机延时对比见 [`docs/M9-缓存优先与映射.md`](docs/M9-缓存优先与映射.md)。

### 修复

- **磁盘上已缓存的内容被当成「没缓存」**：区间位图（`chunks_done`）只活在内存里，
  daemon 一重启或节点被重新 `lookup`，`cache_file_for` 就把区间表清成全 `false` ——
  于是**明明本地有内容**，`cat` 还是逐区间重新问 NAS。实测（4 MiB 文件、真机 NAS、
  重启 daemon 后）：**23.75 s → 0.115 s**。
- **`ls` 每次都是一次 NAS 往返**（约 0.3–1 s）：`readdir`/`lookup` 没有本地清单。
  实测同一会话第二次 `ls`：**0.50 s → 0.001 s**；重启后第二次 `ls`：**0.32 s → 0.001 s**。
- **远端一改就删本地缓存**：`apply_remote_meta` 直接丢掉内容，下次 `read()` 只能整份重下
  （阻塞在读路径上）。现在有水文件旧内容继续可读、后台换新；baseline 等刷新落地再推进，
  避免把「基于旧版本的本地修改」误判成覆盖远端。

### 新增

- **水合位图落盘** `<cache>.qxstate`（magic/version/chunk_size/size/mtime/区间数 + 位集）：
  先写内容后写位图、位图 `.tmp` + `rename` 原子替换；`release()` 时按最终 size/mtime 兜底落盘；
  脱水 / 失效 / 远端删除 / 本地 unlink / 改名都同步处理。
- **目录清单快照**（「映射」）：daemon 轮询每列完一个目录就推给挂载视图
  （`apply_listing` / `drop_listing`），`readdir`/`lookup` 吃快照；快照过期只做后台补拉；
  本地 `create`/`mkdir`/`unlink`/`rename` 立刻同步快照（防止幽灵节点 / 新文件不可见）。
- **后台内容刷新**：整份下到 `<cache>.refresh` 后 `rename` 原子换上，`attr`/区间表/位图一次切换，
  并让内核丢掉旧 page cache（daemon 注入 `Notifier::inval_inode`）；连续失败 3 次退回按需水合。
- `sync --once` 与 journal 增加「本地有水 → 后台更新内容」的计数。

### 测试

- `qxync-fuse` 单测 **27 → 34**：位图编解码与损坏防护、重建节点后认领缓存、
  快照服务 `readdir`/`lookup`（用 `nas.invalid` 当 NAS，**只要还问一次网络就必然失败**）、
  有水时的远端变更语义、原子换上的元数据一致性、换上前被改动则作废、刷新失败退化。

**会话热更新（M10）**：sid 过期后挂载点不再一直 `EIO` 到重新挂载 —— sid 可热更新，
挂载点遇鉴权失败会**让 daemon 重登一次、拿到新 sid 原地重试**；重登成功后新 sid 会
**推给所有同账号的挂载点**（含读写挂载的上传队列），另有定时保活兜底。
设计与真机验收见 [`docs/M10-会话热更新.md`](docs/M10-会话热更新.md)。

### 修复（会话热更新）

- **sid 过期 = 挂载点永久 EIO**：`Client.sid` 是普通字段，挂载时拷一份给 FUSE 用的 client，
  过期后没有任何路径更新它；「遇鉴权失败重登」只写在 IPC 命令的宏里，挂载点与同步引擎都不走。
  真机验收：`logout` 之后挂载点冷 `ls` 0.50 s 成功、`cat` 1024 字节成功，
  日志出现「挂载点会话失效 → 会话已热更新到 1 个挂载点 → 登录成功」。
- **变更轮询遇会话失效每轮都失败**：`map_err` 把服务端 4/5 号 status 泛化成
  `ErrorKind::Status`，调用方分不出「会话失效」；现在 4/5 归 `ErrorKind::Auth`，轮询据此重登。

### 新增（会话热更新）

- `Client` 的 sid 改成 `Arc<RwLock<..>>` + `set_sid(&self)`/`clear_sid()`，跨 `Arc<Client>` 热更新；
  `qxync_core::Error::is_auth()` 收口三层共用的「会话失效」判定。
- FUSE 注入 `SidRefresher`：`readdir` / `lookup` / 区间水合 / `mkdir` / `unlink` / `set_mtime` /
  `rename` 遇鉴权失败 → 重登 → **原地重试一次**（只一次）。
- 会话经纪人：FUSE 线程同步要新 sid、daemon 侧串行重登（3 s 内去重，避免多个挂载点同时打登录接口）。
- 会话保活：每 `QXNYC_SESSION_KEEPALIVE` 秒（默认 120，`0` = 关）探一次，失效就重登并推挂载点；
  探测失败（断网）只等下一轮，不反复打登录接口。
- `login_internal` 成功后把新 sid 推给**同账号**的挂载点（换账号只告警，避免张冠李戴）；
  `logout` 顺手清掉挂载点的 sid。

## [0.4.1] - 2026-10-03

**服务端错误可读 + 就绪状态显式化**：把服务端明明说了却被丢掉的 `msg` 透传出来
（`Qsync Central is initializing. Please wait a few minutes and try again.`），
把 `qbox_get_max_log` 自带的四个就绪字段与限流信息解析并接线。

### 修复

- **服务端说了原因，用户只看到「未知状态码」**：`Error::Status` 现在带 `msg`，
  原样进 `Display`（`— 服务端 msg: …`）。`get_list` / `stat` / `qbox_get_max_log`
  的失败响应都能读到服务端原文。
- **服务端未就绪被当成普通错误**：逆向确证服务端唯一的业务 status 是 `8`
  （`qbox_*` 私有路径，msg 为 `Qsync Central is initializing…`）。现在由
  `Error::is_server_busy()` 识别，**归入「等一等」而非「出错」**——
  不计入同步错误、不触发重登（重登无用，只能等 `qsyncsrv_metad` / `qsyncsrvd` 就绪）。
  判据按 `msg` 文本而非写死码值，服务端将来换码仍认得出。
- **NAS 迁移 / 恢复 / 备份还原时白白全量重扫**：这些状态下 `max_log` 可能短暂偏小，
  过去会被 `should_reset` 判成「游标回退」而触发一次全量重扫。现在读
  `is_migrating` / `is_recovering` / `is_backuping_restoring` / `is_booting`
  前置短路，本轮跳过。

### 新增

- `qbox_get_max_log` 解析 `is_booting` / `is_migrating` / `is_recovering` /
  `is_backuping_restoring` 与 `server_limit` / `cgi_number`，并提供
  `MaxLog::busy_reason()` / `advised_interval_secs()`。
- **批量大小改按服务端自报的 `server_limit` 夹住**（真机实测 256），不再写死 200。
  写死会在上限较低的机型上被服务端截断，导致「以为拉完了其实没拉完」。
  `sync_signal == 2`（降速）时按 `slowdown_seconds` 给出退避建议。
- `qxync status` 增加限流 / 降速 / 就绪状态输出。
- `Client::qsync_probe()` / `raw_get()`：按原始键值打任意 CGI，不做解析或 status 判定，
  供真机探针使用（业务代码请走有语义的封装）。

### 实测结论（推翻既有逆向结论）

- **`get_meta` / `get_meta_profile` 不可用**，路线 B 的元数据方案回退到 `get_list` 递归。
  真机（QPKG 5.0.0.7 build 20260723）实测：`get_meta` 在 `qsyncsrv.cgi` 上
  **恒 HTTP 500**（Apache HTML 错误页），16 种参数形态无一例外；`get_meta_profile`
  恒 `status: 19` 且无任何数据字段。在 `filemanager/utilRequest.cgi` 上两者均为
  `status: 20`，而该入口对**瞎编的** func 也回 `status: 20`，说明在 File Station
  命名空间同样不存在。
  500 是 CGI 自身崩溃而非参数名不对——同端点上瞎编的 func 回 `200` 空体，
  `get_tree` / `get_list` 也正常，证明分发链路通畅。详见
  [`docs/get_meta-实测证伪.md`](docs/get_meta-实测证伪.md)。
- `get_list` 递归成本实测：11 次请求 / 17 个条目，单次往返约 157–430 ms，
  **耗时几乎全在往返次数上**——提速方向是减少往返（缓存 / 映射）而非换接口。
- 按需同步的可行性**未被推翻**：服务端 `qsyncsrv_metad` 生成的 `{share}/.qsync/meta/`
  仍然真实存在，只是没有可用的 CGI 出口。

### 文档

- 新增 [`docs/get_meta-实测证伪.md`](docs/get_meta-实测证伪.md)：
  `get_meta` 的实机证伪过程、判据与回归护栏。
- 脱敏：已入库文档里残留的真实 NAS 域名改为指向本地（不入库）的测试环境文档。

## [0.3.0] - 2026-10-02

**一对多整体删除，只留一对一**：一个挂载点 = 一个 NAS 文件夹，挂载点里**直接**就是那个
文件夹的内容（配 `/home` 就看到家目录，不再多套一层 `home/`）。NAS 侧改成下拉选择
（候选 = NAS 上登记的 Qsync 同步文件夹 + 家目录），选不到还能逐层浏览或手输；
提交时会检查目的地冲突。**连接页的「高级」栏整个删掉了**：`roots` 与 `home_root` 两个
字段都不再存在。**不保留任何兼容**。

### 删除（破坏性）

- **一对多（多根）能力整体移除**，涉及这些曾经存在的入口：
  * link 配置里的 `roots` **和 `home_root`**（连接页「高级」栏整个删除，表单只剩
    host / port / user / password / https / insecure / ipv4-only）；家目录在 Qsync 协议里
    固定叫 `/home`（`qxync_core::HOME_ROOT`），不是配置项；
  * `qxync mount --remote A --remote B` —— 现在 `--remote` 只能给一次，`--root` 同理；
  * FUSE 的**虚拟根**（`ViewLayout::Multi` / `RootSpec` / `QxyncFs::new_multi` /
    `multi_root` 分支）与 `MountInfo.roots`、`RootsData.configured` / `roots`；
  * 任务里的 `roots: Vec<String>` → `root: Option<String>`（一个任务一个 NAS 文件夹）。
- **旧的多根任务文件**：没有兼容保留。文件里只有一个 `roots` 条目 → 自动迁移成 `root`；
  有多个条目 → 在任务列表里报成 `bad_file`（错误信息写明「旧的多根格式，请为每个 NAS
  文件夹各建一个任务」）。**不会静默缩小同步范围**。
- `xtask/tests/m6-matrix.sh`（29 项多根矩阵）随功能删除；`docs/M6-多根与共享文件夹.md`
  标注为历史文档（仍有效的结论「共享文件夹能读不能写」保留在 README §踩坑 30–33）。

### 新增

- **「NAS 文件夹」下拉 + 「浏览…」选择器**。以前是一格 textarea（每行一个 NAS 目录），
  既看不出 NAS 上有哪些目录、也不该让人手打路径。现在：
  * 候选：NAS 上登记的 Qsync 同步文件夹（`qbox_get_syncing_folder_list&detail=1`），
    并把共享路径映射成客户端路径（`/share/homes/<user>/x` → `/home/x`，真机 HAR 有直接证据）
    + 家目录；
  * 「浏览…」= 逐层点目录挑（复用文件页的 `ls`），「手动输入」兜底任意路径。
- **GUI 静态合规性单测**（`cargo test -p qxync-gui` 的 `ui_spec_is_green`）：文案键齐全 /
  无 HTML 拼接 / 无障碍与四态标记，改界面忘了补 i18n 会直接测试失败。
- **`xtask/tests/pair-1to1.sh`（17 项，不需要 NAS / 不需要 FUSE）**：起一个私有 daemon
  （假 link，只走 `tasks save`）验一对一登记、`--root` 重复被拒、目的地冲突拦/提示的分工、
  旧文件单个 `roots` 迁移 / 多个 `roots` 报错。

### 变更

- **可写性不再由客户端预判**：以前守卫按「是不是家目录」一刀切，那是错的判据 ——
  能写的判据在 NAS 侧（目录有没有被登记成 Qsync 同步文件夹，见
  `qbox_get_syncing_folder_list`），家目录之外登记过的目录照样能写。现在勾了「读写」就按
  读写挂载；真被服务端拒绝（`status:20`）由上传队列与「更新 / 错误」页如实报出来。
  GUI 的下拉会把「NAS 同步文件夹」这类候选标出来，并在说明里讲清这条边界。
- `qxync roots` 现在只列「家目录 + NAS 上登记的同步文件夹」（不再逐根探测可读性，也不再有
  「配置的根」）；`qxync roots` 的「NAS 同步文件夹」一行打印**客户端路径**（`client_path`），
  映射不出来时显示共享路径并标注。

### 修复

- **`qbox_get_syncing_folder_list` 的真机字段一直解析错了**。真机（2026-10-02 HAR，
  `detail=1`）回的是 `name` / `path` / `privilege`，而解析按项内 `folder` / `permission`
  去读 —— 于是只要 NAS 上真登记了同步文件夹，界面就会显示成「空名字 + 0 权限」，
  把「能列举」误判成「列不出来」。现在两种拼法都认（真机字段优先），并补了 HAR 原文的
  回归测试（`syncing_folders_real_machine_response_is_not_lost`）。
- **「没写 NAS 文件夹」的默认值统一了**。以前 GUI 结果行与 `Task` 的注释都写「由 link 的
  `home_root` 决定」，但 daemon 落的是编译期常量 `/home`（`mount()` 的默认值）—— 改过
  `home_root` 的用户会静默挂到 `/home`。现在两边都走 `Task::effective_root()` =
  协议常量 `/home`（`home_root` 字段本身也删掉了）。
- **提交时检查目的地冲突**：本地文件夹与别的任务重复或互相嵌套 → **拒绝保存**并说明是哪个
  任务（嵌套挂载会互相遮挡）；同一个 NAS 文件夹被别的任务用了 → **只提示**（只读挂同一个
  文件夹是合法用法，`m82-matrix.sh` 的 t1/t2 就靠它），两个任务都读写时会明确写出
  「双向写会打架」。

## [0.2.3] - 2026-10-02

**界面看得懂了**：术语去黑话（界面上不再出现「远端根」，统一叫「NAS 目录」），
连接页那两个平时不用动的字段收进默认折叠的「高级」栏；主页「＋ 添加任务」也不再
把你送去「诊断 → 挂载」。无破坏性变更，配置与协议一字未动，升级即用。

### 新增

- **连接页「高级：同步范围」折叠栏**（`roots` + `home_root` 两格）。这两格普通账号
  根本不用填（默认就只同步家目录），原来裸放在表单里，用户既看不懂、又容易被误填。
  现在收进原生 `<details>` 默认折叠，输入框 `id` 不变、收起状态下照样回填与提交；
  展开后是两段大白话说明（含「配置文件里这个字段叫 `roots` / `home_root`」的对照）。

### 修复

- **主页「＋ 添加任务」现在真的去「添加配对文件夹」**。这颗按钮的事件处理还是 M8.2
  之前的老写法（`switchPage('diag', 'mounts')`）—— 那时任务登记还没进 GUI，只能把人
  送去挂载页。M8.2 加了任务页与 `openTaskForm()`、M8.4 又把冲突策略/方向/节省空间并进
  同一张表单之后，主页快捷动作没跟着改，于是和任务页 `#btn-tasks-add` 走了两条不同的路：
  用户按字面理解要填「本地文件夹 ↔ NAS 文件夹」，看到的却是挂载点 / 远端根 / 线程数。
  现在两者完全同路（`switchPage('tasks')` + `openTaskForm()`）。任务卡片上的「管理」
  按钮仍去「诊断 → 挂载」（那里才是查看/卸载已建挂载的正确落点），未改。
- **主页不再把引擎的「说明」当报错标红**。`qbox_get_sync_log` 恒返回 `status:-17`
  （本账号从未登记同步文件夹，区间内没有事件）时，引擎在
  `crates/qxync-daemon/src/sync.rs` 里走的本来就是 `report.note(...)` 而不是
  `report.error(...)`；但 GUI 主页用同一个 `addAlert()` 渲染 `sy.note`，而
  `addAlert()` 没有严重度参数、硬编码 `div.alert`，`.alert` 的配色写死
  `--err-soft` / `--err` —— 于是一条正常提示在主页常驻成红框，而且计数每轮 +1、
  红框永远不消失。同一个 `note` 在「状态」页走的是中性的 `<p class="note">`，
  两处表现不一致，可见红色并非有意。
  现在 `addAlert()` 增加 `severity` 参数（默认 `'error'`，真错误仍为红），`note`
  传 `'warn'` 挂 `.alert-warn`（`--warn-soft` / `--warn`）；错误与删除熔断的红色不变。

### 变更

- **界面术语统一**：`远端根` → **`NAS 目录`**，`多根` → `多个目录`，`视图名 X` →
  `挂载点里叫 X`，`归属根` / `根相对路径` → `属于哪个目录` / `目录内相对路径`；
  文件页「远端目录」/ 右键「复制远端路径」/ 冲突策略里的「远端占原名」等单说「远端」的
  地方也统一成「NAS」（原文案混用「NAS」与「远端」，同屏看着别扭）。
  原始字段名（`roots` / `home_root`）保留在提示与诊断面板的 kv 标签里，方便对着配置
  文件或 `--json` 排障；`docs/`、配置字段与 CLI 继续用「远端根」这套术语。
- daemon 的 `roots` note 措辞跟着改（同时出现在 `qxync roots` 输出里，m6 矩阵只断言它非空）。
- 版本号 → 0.2.3；发布说明 `docs/发布说明-v0.2.3.md`。

## [0.2.2] - 2026-10-02

**给 daemon 配了 systemd user 单元**，并修掉一个会让 `systemctl stop` 留下 FUSE 挂载的问题。

### 新增

- **systemd user 单元 `qxyncd.service`**（`packaging/systemd/qxyncd.service`，装到
  `/usr/lib/systemd/user/`）：`systemctl --user enable --now qxyncd` 就是「登录即启动 +
  现在启动」，`Restart=on-failure` 崩了自动拉起；想不登录也常驻用
  `sudo loginctl enable-linger "$USER"`。
  刻意是 **user** 单元而不是 system 单元：qxyncd 是「某个用户的同步客户端」（用 `$HOME` /
  `$XDG_RUNTIME_DIR`，FUSE 挂载点也属于该用户），所以包**不会**替用户启用它 ——
  附 `qxync-bin.install` 在装完打印怎么开、以及「开了单元就别再用 `qxync daemon start/stop`」。
- **Arch 包与 Release tarball 都带上这个单元**；CI 的包内容检查也把它列进必查清单。

### 修复

- **`qxyncd` 现在处理 SIGTERM**。`systemctl --user stop/restart`（以及 systemd-logind 注销）
  发的就是它，之前没处理 → 默认动作是**立刻终止**：FUSE 挂载点会留在 `/proc/mounts` 里、
  socket/pid 也不删。现在 SIGTERM 与 SIGINT、IPC `shutdown` 走同一条优雅退出路径
  （卸载全部挂载点 → 删 socket/pid），单元里 `TimeoutStopSec=30` 给足卸载时间。
  注册信号处理器失败时**不 panic**（daemon 化之后 panic 信息会掉进 `/dev/null`），
  退化成「没有这条优雅退出路径」并记一条 WARN。

### 变更

- 版本号 → 0.2.2；发布说明改名 `docs/发布说明-v0.2.2.md`。

## [0.2.1] - 2026-10-02

> **这一版也没有单独发出去**（同样没有打 tag）。第一个真正发布的版本是 **0.2.2**。

**daemon 现在「没配 NAS 也能一直跑着」了。** 之前 `qxyncd` 启动时硬性要求一份连接配置
（`~/.config/qxync/links/<id>.json`），读不到就**直接退出** —— 于是「还没登录 / 还没配连接」
等同于「daemon 用不了」，和「daemon 常驻后台」的用法直接冲突；GUI 还会把你引到
`qxync daemon start` 那句提示上（照做依然报错）。

### 变更

- **新增「空转待命」态**：一份连接配置都没有时，daemon 照样启动、照样常驻，只服务
  `ping` / `status` / `shutdown` 与**纯本地文件**的设置读写。`status` 里 `link` 为 `null`，
  GUI 直接显示「未配置」（用的是既有的 `top.conn_none` / `home.conn_no_link` 文案）。
  需要 NAS 的请求被**明确拒绝**（「还没有配置 NAS 连接：先 `qxync login`…」），
  而不是拿着空 host 去发请求。
- **配好连接后自动生效，不用重启、不用再敲命令**：空转的 daemon 每 2 秒看一眼 link 文件，
  一出现就**原地**转入同步模式（同一进程、同一个 pid，不 re-exec），随后自动开始同步。
- **`qxync daemon start` / GUI 的「启动 daemon」不再因缺连接配置而拒绝启动**：
  启动成功但还没配连接时，CLI 会明说「空转待命」，GUI 主页显示「未配置」。
- **空转时设置照常可读可写**：GUI 的设置页（登录表单就在那一页）靠它渲染，
  之前会先弹一个错误。`settings_save` 的落盘逻辑抽成共用函数，避免空转态与同步态漂移。

### 测试

- 新增**不需要 NAS 的回归测试** `idle_daemon_serves_without_any_link_config`：真起
  `qxyncd`、真走 unix socket，验「无 link 也能 ping/status/设置读写/干净退出」，
  并验「依赖 NAS 的请求被拒且理由可读」。这个用例在 CI 里就能跑
  （原有那条要真机，只能常年 `#[ignore]`）。

## [0.2.0] - 2026-10-02

> **这一版没有单独发出去**（没有打 tag）。第一个真正发布的版本是 **0.2.2**，
> 它包含下面全部改名内容 —— 这里的记录保留，是因为改名本身值得单独成一节。

**改名版本：命令行从 `qsync` 改成 `qxync`。** 旧名 `qsync` 会和 **QNAP 官方 Qsync 客户端**
抢同一个 `/usr/bin/qsync`，而 `~/.config/qsync`、`QSYNC_*` 环境变量、`user.qsync.*` xattr
其实都是同一个来历的旧名。这一版把这些**我们自己的**标识统一收进 `qxync`；
**QNAP 侧的产品名与协议串一字未动**（边界见下）。

### 变更（破坏性）

- **CLI 二进制 `qsync` → `qxync`**：`/usr/bin/qsync` 不再存在，PATH 里不会再和官方
  Qsync 客户端撞名。子命令、参数、输出格式一律不变。
- **配置 / 数据 / 状态目录 `…/qsync` → `…/qxync`**：`~/.config/qxync`、
  `~/.local/share/qxync`、`~/.local/state/qxync`。**首次运行自动迁移**，三种情况都不丢数据：
  只有旧目录 → 整体改名（跨文件系统时退回复制 + 删除）；两个都在（例如 0.1.0 的原型目录
  `qxync/` 还留着）→ **只补缺**，把旧目录里新目录没有的条目搬进来，**已有的一律不覆盖**；
  只有新目录 → 直接用。所以凭据与状态库都跟着走，不用重新 `login`。
- **环境变量前缀 `QSYNC_` → `QXNYC_`**：`QXNYC_HOST` / `QXNYC_USER` / `QXNYC_PASSWORD` /
  `QXNYC_SOCKET`，以及全部 `QXNYC_*` 调参与验收开关（原 `QSYNC_*`）。旧名不再识别。
- **FUSE xattr `user.qsync.*` → `user.qxync.*`**：`state` / `pin` / `remote` / `vsize` /
  `chunks`；脚本里的 `getfattr -n user.qsync.state` 要跟着改。
- **下载临时文件后缀 `*.qsync-part` → `*.qxync-part`**（`*.qsync-tmp` → `*.qxync-tmp`），
  内置临时文件过滤规则同步更新。
- **GUI 显示名 `QSync` → `qxync`**：`productName`、窗口标题、托盘 tooltip、通知标题、
  自启桌面项 `Name=` 全部统一；Tauri `identifier` 随之改为 `org.qxync.qxync-gui`。
- **日志与运行时名**：`qsync-gui.log` → `qxync-gui.log`、托盘 ID `qsync-tray` →
  `qxync-tray`、IPC socket 目录 `$XDG_RUNTIME_DIR/qxync/qxyncd.sock`。
- **自启项** `autostart/qsync.desktop` → `qxync.desktop`：切换「开机自启」开关时会顺手删掉
  旧的那一个（旧文件里的 `Exec=` 其实还能用，只是 `Name=` 是旧名），
  `autostart_present()` 也把旧文件算作「已生效」，免得界面显示错误。

### 改名边界（什么**没**改）

只改**我们自己的**标识；QNAP 的产品名与 NAS 协议面原样保留：
`cgi-bin/qsync/qsyncsrv.cgi` / `qsyncsrv_login.cgi` / `upload.php`、
`qsync_version` / `Qsync_qpkg_version` / `Qsync_client_version` 等返回字段、
登录体里的 `client_app=Qsync`、`Qsync QPKG` / `Qsync Client 6` / `.Qsync`、
错误码 `WFM_QSYNC_DISABLED` / `QFILE_ERROR_QSYNC_QPKG_NOT_EXIST`、
NAS 设置名 `QSYNC_FOLDERPAIR_USE_SPACE_SAVING`、Windows 客户端注册表键
`QSYNC_PROCESSED_MAX_*`、探测脚本的 `QSYNC` 常量与 myQNAPcloud 路径前缀 `qsync/`。
`.desktop` 的 `GenericName=QNAP Qsync client`、crates.io 关键词里的 `qsync`
也保留 —— 它们描述的是「跟什么互通」，不是本项目的名字。

### 修复

- **`qxyncd` 启动失败不再静默**。daemon 默认 daemon 化（`fork` 之后 fd 0/1/2 全指向
  `/dev/null`），启动期的致命错误以前只随 anyhow 打进 `/dev/null` —— 于是
  `qxync daemon start` 永远只报一句「socket 未就绪」，用户拿不到任何原因（本次就是被这个
  坑住的）。现在三处一起补：
  ① daemon 把启动失败写进 `<state>/log/qxyncd.log*`；
  ② `qxync daemon start` 派生后 socket 起不来时，把日志尾部带回前台；
  ③ CLI 在派生**之前**先核对 link 配置，缺了就直接说「还没有配置 NAS 连接」并给出
  `login` 命令（GUI 的 `daemon_start` 早就有这道前置检查，CLI 这边补齐）。
- **GUI 空态提示指向真正缺的那一步**：连不上 daemon 时不再一律说「先 `qxync daemon start`」
  —— 还没有任何连接配置时改说「还没有配置 NAS 连接，先在「设置 → 连接」里保存
  （或跑 `qxync --host … login`）」。全新安装下照旧提示只会让人再撞一次墙。
- daemon 的「创建配置 / 数据 / 日志目录」与「创建 / 授权 socket 目录」补上 anyhow 上下文，
  日志里能一眼看出卡在哪一步。

### 其它

- **UA 与登录体**：`client_agent` 从硬编码的 `QSyncLinux/0.1` 改成 `qxync/<真实版本>`
  （取自 `CARGO_PKG_VERSION`，不会再随版本漂移）。
- 版本号固定的**历史存档**保留旧名：`docs/发布说明-v0.1.0.md`、
  `docs/发布说明-v0.1.1.md`、`docs/发布清单-v0.1.0.md` 与 0.1.x 的条目记录的是当时
  **真发出去**的产物，不做改写；发版清单顶部加了一段「照它操作时请自行翻译旧名」的说明。
  同理，文档里**指向 QNAP 的外链**（教程 / 公告 / 产品页）与本仓库外的文件名一字未动。
- 新增 `adopt_legacy_dir` 与旧 autostart 清理的单元测试（整体迁移 / 只补缺不覆盖 / 可重入 /
  旧桌面项被清掉）。
- 升级步骤速查见 README「从 0.1.x 升级」。

## [0.1.1] - 2026-10-02

**只改了「怎么把 qxync 交到用户手里」，客户端行为与 v0.1.0 完全一致** ——
`v0.1.0..v0.1.1` 之间**没有任何 Rust 代码改动**（`git diff --stat v0.1.0 HEAD -- '*.rs'` 为空）。

### 新增

- **发版产物自动化**：推 `v*` tag（或手工触发 `Release` 工作流）即构建并发布
  `qsync` / `qxyncd` / `qxync-gui` 三个二进制 —— 一个
  `qxync-<版本>-x86_64-unknown-linux-gnu.tar.gz`（含桌面项与图标）、三个裸二进制、
  `SHA256SUMS`，全部挂到该 tag 的 Release。发版前有**版本一致性守卫**（tag 与 `Cargo.toml` /
  `tauri.conf.json` / AUR `PKGBUILD` 的 `pkgver` 必须一致，否则拒绝发版）；打包脚本
  `xtask/release/package-linux.sh` 可本地复跑，产物可复现（tar 内 owner/group 归零、
  顺序固定、gzip 不写时间戳）。
- **Release 里也带 Arch 包**：`qxync-bin-<版本>-1-x86_64.pkg.tar.zst` 由 CI 在
  `archlinux:base-devel` 容器里用**真 `makepkg`** 从上面那个 tar.gz 打出来，包里三个二进制
  与 Release 资产**逐字节一致**；`sudo pacman -U` 直接装。
- **Arch Linux 包（AUR `qxync-bin`）**：`packaging/arch/` 里带 AUR 包定义 —— 直接取 Release
  上的预编译 `tar.gz`，把 `qsync` / `qxyncd` / `qxync-gui` 装进 `/usr/bin`（带桌面项与图标）。
  `makepkg -si` 本地即可装，推到 AUR 后是 `yay -S qxync-bin`。包**有意不 strip**（与 Release
  产物一致，为了回溯可读），装完约 190 MB。

## [0.1.0] - 2026-10-02

**首个公开版本。** 这不是「能跑起来」的程度：每个里程碑都在真机上跑过完整验收，
最近一次全量口径 **491 项全过**（`fuse 68 · gui 148 · m5 30 · m6 29 · m7 60 · m82 37 · m83 27 · m84 92`），
明细见 [`docs/验收记录.md`](docs/验收记录.md)。

真机验证对象：**QNAP TS-464C / QTS 5.2.9 / Qsync QPKG 5.0.0.7（build 20260723）**。

### 新增

- **协议客户端与 CLI（M0）**：登录 / 列举 / stat / 下载 / 上传 / 建目录五条路全部对真机跑通；
  二进制 `qsync` 提供 `login · status · ls · stat · get · put · mkdir`。
  协议结论来自对 Qsync for Windows v6.1.0.0831 的静态逆向，且每一条都在真机上复验过。
- **只读 FUSE + on-demand 按需水合（M1）**：`ls -l` 显示真实大小却零下载，首次 `read()` 才取数据；
  `user.qsync.*` xattr 暴露占位符状态；读不满即 `EIO`，绝不短读。
- **守护进程与本地 IPC（M1.5）**：二进制 `qxyncd` 成为**唯一**持有 FUSE 与 NAS 会话的进程；
  unix socket + 一行一个 JSON 的 IPC；`qsync` 默认自动路由（socket 可连就走 IPC）。
- **128 KiB 区间水合（M2a）**：`head -c 100 big.bin` 只下载 1 个区间，不再整文件下载。
- **写路径（M2b）**：`--rw` 读写挂载；上传队列 + dirty 标记崩溃恢复；
  **写前 read-modify-write**，绝不把没取回的区间当 0 上传。
- **变更发现（M2c）**：三游标轮询 + baseline 三向对账（事件只是快路径）；
  双方都改用**冲突副本**，远端批量删除用**熔断**保护。
- **脱水 / 释放空间（M3）**：完整安全检查链（pin、未上传改动、打开的 fd、被 mmap、正在水合、
  刚访问过）后才清本地内容；`inval_inode` 顺序铁则；闲置 / 限额 LRU；`--cache-mode pagecache|direct`。
- **桌面 GUI（M4）**：Tauri 2 应用，前端是**零依赖静态三件套**（无 npm）；
  登录配置 / 挂载管理 / 状态与进度 / pin 管理。
- **SQLite 状态库 + delta（M5）**：`sync.db` 承载游标 / baseline / pin / 上传队列，
  **同一事务**落盘；老 JSON 状态自动迁移并归档；librsync 原生格式的 sign/delta/patch + 能力门控。
- **多根 / 共享文件夹（M6）**：link 配 `roots`，挂载点顶层出现每个根的名字；
  家目录根可读写，非家目录根在 FUSE 层直接 `EROFS`。
- **选择性同步（M7）**：gitignore 风味的 `exclude` 规则引擎（锚定 / `*.iso` 任意层级 /
  子树剪枝 / 反向包含 / `**`）+ 内置临时文件过滤，贯通 FUSE、同步引擎与脱水候选。
- **LAN 直连（M7）**：qxync↔qxync 自研对等协议 —— 设备配对、事件快路径、本地区间直传；
  默认不监听，任何失败 / 不一致 / 超时都静默回落 NAS。
- **同步任务（M8.2）**：`tasks/<id>.json` 持久化登记，逐任务暂停 / 继续，
  `qxyncd --restore-tasks` 重启恢复（默认关闭）。
- **同步日志（M8.3）**：`sync.db` schema v3 增加 `journal` 表，后台批量落库 + 轮转，
  驱动 GUI 的「文件更新中心 / 错误列表」。
- **设置中心（M8.4）**：代理三模式（Auto-detect / No proxy / Manual，接 reqwest 与
  `CONNECT` 实测）、开机自启、桌面通知、ksni 托盘（含「真可见」探测）、
  `statvfs` 自动释放空间（复用脱水安全链）、冲突策略五选、文件三态。
- **打磨与可访问性（M8.6）**：视觉规范 token 化（浅 / 深两份）、键盘可达性与焦点环、
  空 / 错 / 加载四态全库复查、i18n 文案表（zh-CN 163 条 + en 预留）、
  `qxync-gui --self-test` 的 `ui_spec` 静态自检。

### 修复

- 挂载失败不再把 daemon 的 IPC worker 打成 panic：`QxyncFs` 的 tokio `Runtime` 在 async 上下文里
  析构会触发 `Cannot drop a runtime in a context where blocking is not allowed`。
  改为按上下文选路的 `FsRuntime`（tokio 里用 `shutdown_background()`），
  并新增回归单测 `dropping_fs_inside_async_context_does_not_panic`。

### 安全

- **仓库内不含任何真实主机名、账号、本机路径或设备指纹** —— 发版前做过统一清洗，
  映射与验证方法见 [`docs/发布清单-v0.1.0.md`](docs/发布清单-v0.1.0.md)。
- 仓库**不分发** QNAP 二进制 / 安装包 / 反编译产物；报告中的第三方凭据已掩码。
- 凭据文件 `0600`，IPC socket 目录 `0700` / socket `0600`。
- **无任何遥测**；除你配置的 NAS 与可选 LAN 对端外不连接任何主机。

### 文档

- 中英双语 `README.md` / `README.en.md`。
- 新增 [`CONTRIBUTING.md`](CONTRIBUTING.md)、[`SECURITY.md`](SECURITY.md)、
  本变更日志（中英双语）。
- 验收结论从 README 抽出为 [`docs/验收记录.md`](docs/验收记录.md)。
- 采用 **MIT OR Apache-2.0** 双许可（`LICENSE-MIT` / `LICENSE-APACHE`）。

[0.5.1]: https://github.com/mlzxgzy/qxync/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/mlzxgzy/qxync/compare/v0.4.3...v0.5.0
[0.4.3]: https://github.com/mlzxgzy/qxync/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/mlzxgzy/qxync/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/mlzxgzy/qxync/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/mlzxgzy/qxync/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/mlzxgzy/qxync/compare/v0.2.3...v0.3.0
[0.2.3]: https://github.com/mlzxgzy/qxync/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/mlzxgzy/qxync/releases/tag/v0.2.2
[0.1.1]: https://github.com/mlzxgzy/qxync/releases/tag/v0.1.1
[0.1.0]: https://github.com/mlzxgzy/qxync/releases/tag/v0.1.0
