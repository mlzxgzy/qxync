# 变更日志

本文件格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)。

[English](CHANGELOG.en.md) · [README](README.md) · [验收记录](docs/验收记录.md)

## [Unreleased]

### 新增

- **本地文件搜索（[M42](docs/M42-本地文件搜索.md)）**：新增 `qxync find`，**只读**同步
  状态库（`baseline` + `nodes.local_only`），不挂载、不联网、daemon 不在也能按路径/
  文件名子串或通配符搜 NAS 同步树；支持按子树（`--under`，可给本地路径）、大小
  （`--min-size`/`--max-size`）、类型（`--type f|d`）过滤，按路径/大小/mtime 排序，
  `--local` 把命中换算成挂载点里的本地路径，`--json` 供脚本使用；`--limit 0` 全量。
  真实库（28 136 文件 + 5 891 目录）实测 `qxync find 防火墙` 约 **0.1 秒**。
  不动 schema / IPC / daemon，纯只读；新鲜度最多滞后一轮轮询（上传成功会立即落盘）。
- **GUI 搜索同步文件（[M42 §8](docs/M42-本地文件搜索.md) / M43）**：文件页新增
  「搜索同步文件」卡片 —— 关键词（回车即搜）/ 通配符 / 限定目录（NAS 路径或挂载点路径
  都行）/ 类型 / 排序 / 条数上限；结果表带来源（已同步 / 本地新建）与「定位 / 打开 /
  复制路径」三个动作，命中能直接映射到挂载点里的本地路径。与 CLI 共用
  `qxync_core::search`，**不挂载、不联网、daemon 不在也能搜**；切连接自动清结果，
  4 万级条目照样毫秒级返回。

## [0.7.14] - 2026-10-28

### 变更

- **日志降噪（[M41](docs/M41-日志降噪.md)）**。M39 管住了日志文件的体积（切段 / 保留），
  但每天**写进去的内容**仍以重复为主：真机 10-25 单日 11.19 MB 里 **10.33 MB** 是同一条
  `创建缓存文件失败 … Too many open files`（48 983 行，每条只差一个路径），10-26 的
  4.46 MB 里逐目录 `对账列举失败` 3 677 行、断网时逐文件 `上传暂停` 941 行。M41 在
  **源头**降噪、不放松诊断：
  - 新增 `qxync_core::noise`：同一 `(站点, 签名)` 只放行「首次 + 每窗口一条」，放行那条
    带「过去一个窗口内另省略 N 条同类」；逐条明细仍写 `debug!`（`RUST_LOG=debug` 一条
    不少）。签名不含路径/数字（否则等于没限流）、状态有界（512 键）、锁中毒也继续记日志。
  - **状态迁移式**告警：轮询异常 / 会话失效 / 上传暂停 / outbox 回放中断只在「变坏 /
    变化 / 恢复」时写一条，持续期间静默；恢复后再坏会重新报（事件不丢）。
  - 逐目录对账失败：每轮最多 3 条样本 + 1 条「另有 N 条」汇总；`SyncReport.errors`
    的语义与 20 条上限不变，IPC / GUI 错误列表 / journal 照旧。
  - 空转 `同步轮询` INFO → 有实际动作才写 + 10 分钟心跳；逐文件「已上传」→ `debug!` +
    每 60s「近一分钟上传 D 项 / B 字节」心跳；逐项离线入缓冲 / 删除熔断 / 水合位图
    / EMFILE 家族 / fuser「未实现」→ 按签名限流；`已删除` 名单截断到 120 字符；
    `accept 失败`（M39 的 fd 耗尽洪水类）按 errno 60s 限流 —— 即使 fd 再泄漏，最坏
    1 条/分钟（≈ 1.5 MB/天），不再可能复现「一天 5 GB」。
  - 实测（同批真机日志按新规则离线重放，`xtask/probe/m41_log_volume.py`）：三天
    16.35 MB → 0.58 MB（**-96.5%**），单日降幅 -87% ~ -99%；fd 泄漏（EMFILE）根因
    **未修**，但不再刷屏（M39 已保证刷不爆盘）。

## [0.7.13] - 2026-10-27

### 新增

- **多路径连接（内网 / 公网 / DDNS）与失败自诊断（[M40](docs/M40-多路径与DDNS.md)）**。
  一条 NAS 连接现在可以配置多条访问路径（家里 LAN、公网 DDNS、备用域名），
  按顺序自动挑可达的那条：
  - 额外路径按书写顺序**先于**主地址尝试，主地址自动兜底；列表为空 = 旧的单地址行为
    一字不变。端口/协议省略时继承主地址（不猜 8080/443），最多 8 条。
  - 一条路径失败 → 标记冷却（30s 起、5 分钟封顶）并**立即切到下一条**；一次失败只切
    一条（并发失败不会跳路）。LAN 恢复后连续 2 次探测成功才切回，避免抖动。
  - 登录、列目录、stat、下载、上传都会跨路径重试；mkdir / rename / move / delete 只在
    「连接都没建立」时重试（避免重复应用）。DDNS 换 IP 后自动重建连接池重新解析。
  - **连不上时逐条诊断**：`qxync routes [--probe]`（daemon 不在也能跑）列出每条路径的
    DNS 解析（A/AAAA）、内网/公网/运营商 NAT 分类、延迟、冷却与最近错误；
    `qxync status` 多一行路径摘要；`qxync login` 失败时自动打印每条路径现场。
    GUI 设置→连接可编辑备选路径，诊断页新增「多路径」子页。
  - **数据安全**：切换路径后用 NAS UID 指纹核对是不是同一台 NAS；指向另一台时
    `blocked` 并换路，绝不把数据写到错误设备。
  - 兼容：老 link 文件无需迁移（新字段空值不落盘）；IPC 只加字段/方法，不 bump 版本；
    改路径配置仍需重启 daemon 才生效（GUI「保存并登录」会自动重启），运行期切路不需要。
  - 验收：`xtask/tests/m40-multipath.sh` 隔离 e2e **27/27**（假 NAS，无真机依赖）；
    真机只读探测实测「内网路径解析为内网但超时、DDNS 解析为公网且可达、active 选中
    DDNS」（`xtask/tests/m40-real-nas.sh`，结果记在验收记录）。

## [0.7.12] - 2026-10-27

### 新增

- **日志轮转与保留策略（[M39](docs/M39-日志轮转与保留.md)）**。真机
  `~/.local/state/qxync/log/qxyncd.log.2026-10-23` **单文件 5.17 GB**（5 174 393 555 字节 /
  52 266 068 行，目录合计 4.9 GiB）。旧实现只有 `tracing_appender::rolling::daily`，两个缺陷：
  **同一天内没有大小上限**（只在跨天切段）、**旧文件从不删除**（只增不减）。现在：
  - **写路径换成自己的 `RotatingWriter`**（`qxync-core/src/logcfg.rs`）：按天 **且按大小** 切段 ——
    写满 `log_max_file_mb`（默认 **32 MiB**）当场改名成 `<前缀>.log.<日期>.<n>` 再开新段；
    文件名规则与旧 `daily` 逐字一致（**UTC 日期**，本地 08:00 换段），历史文件仍排得进序列；
    启动时发现当天段已超限会**直接切段**（正是「昨天写爆、今天重启」的现场）。
  - **按天 / 按总占用清理**：`log_keep_days`（默认 **14 天**）与 `log_max_total_mb`
    （默认 **256 MiB**）删旧文件，按 **mtime** 从最旧删起（不解析文件名，旧文件、外部拷贝来的
    文件也能正确处理）。「正在写的文件不删」用 **`flock` 探测**（写入器持 `LOCK_EX`，清理前试加锁；
    加不上就跳过并记进 `PruneReport.in_use`，再经 IPC `LogsData.in_use` 透传给
    CLI / GUI —— 会明确显示「跳过（正被写入）: …」而不是静默不动）—— daemon 与 GUI 是两个进程、
    GUI 可能几天不写日志，
    按 mtime /「族内最新」猜会把活动文件误判成死文件；反过来，40 天前已退出的孤儿日志现在会被
    正常删掉。
  - **策略进 `settings.json`，改完不用重启**：`log_max_file_mb` / `log_keep_days` /
    `log_max_total_mb`（三者 `0` 各有语义：不切段 / 不按天删 / 不限总量；范围上限在
    `Settings::normalize` 夹住）。写路径每次现读进程内策略；`settings_save` 热应用并**立即清一次**，
    保留天数从 14 改成 3 会马上看到空间回来。
  - **定时清理独立于写日志**：daemon 每 **3600s**（`QXNYC_LOG_PRUNE_SECS` 可调，最小 5）按策略跑
    一次，所以「不写日志也会按时删旧文件」；空转待命的 daemon 同样生效。
  - **可见 / 可操作**：IPC `Request::Logs { prune }` → `LogsData`（目录、占用、策略、清理结果、
    失败项、按大小降序最多 50 条文件）；CLI `qxync log` / `qxync log --prune` / `--json`；
    GUI 设置 →「日志」分区（占用、三项策略、保存、立即清理、文件表）。清理**不做网络/同步动作**，
    空转待命也能看。
  - 边界如实记在文档里：删除是尽力而为（失败项直接报出来）；`flock` 是启发式 —— **升级后旧二进制
    还在跑的那段时间它不持锁**，`qxync log --prune` 可能删掉旧 daemon 正在写的文件（只丢诊断信息，
    不丢数据），所以升级后尽快重启 daemon；总量上限不是硬上限（活动文件受保护优先）。
  - ⚠ **M39 管「文件不再无限长大 + 自动清」，不修刷屏根因**：那次 5.17 GB 里
    **99.998% 的行是同一条** `WARN qxyncd::daemon: accept 失败: Too many open files (os error 24)`
    （52 265 459 / 52 266 068 行）—— 是 `accept` 失败后的**紧循环**，不是「每轮轮询打一行」。
    写文档当天（2026-10-27 08:34–08:37）同形状又复发一次：**2 分半写出 674 MB**。
    **fd 被耗光的根因未定位，单独记一笔待查。**

### 修复

- **上传队列把「本地内容源缺失」误判成断网（M38）**。真机 2026-10-26：编辑器（WPS）
  原子保存产生的临时文件被清理后，上传队列里留下一条「内容源指向已删除 `.tmp`」的作业。
  旧实现把本地 `stat` / `open` 失败包成 `Error::Transport`，上传队列据此判成「网络不可用」
  —— 作业永不放弃、每轮重试还把连通性标成**离线**：界面一直「网络断开」而 NAS 正常，
  点「立即同步」永远看不到恢复（同步其实跑了，随即又被僵尸作业翻回离线）。修法三条：
  ①`qxync-client` 本地 `stat` / `open` 失败改报 `Error::Io`（不是 `Transport`）；
  ②`qxync-fuse` 内容源不存在 → **立刻放弃并删掉队列行**（`error` 日志留痕），不再无限
  重试；③按身份改名前先确认内容源还在，避免顶掉新名字上能传的作业。回归测试 3 条
  （`upload_missing_local_is_io_not_transport` /
  `missing_content_source_is_given_up_not_offline` /
  `identity_retarget_skips_dead_content_source`）。现场用缓存副本补回 35 043 字节暂存
  文件后上传成功、队列清零、远端 `exist=true`。
- **`accept` 失败不再空转刷屏（M39 伴生防御）**。错误分支原来直接落回 `select!`，下一次
  `accept` 立刻又失败 —— 纯烧 CPU + 刷日志（真机两次 EMFILE 紧循环就是它）。现在失败后
  `sleep(100ms)` 再重试（日志文案 `accept 失败（100ms 后重试）`，三处 `accept` 分支统一），
  一天上限约 86 万行，再被单文件上限切段 + 总量上限兜住。**这是止血，不是根治**：fd 为什么被
  耗光仍待查。

## [0.7.11] - 2026-10-26

### 修复

- **休眠唤醒后守护进程被 SIGPIPE 杀死（M37）**。`qxyncd` 的 `main` 从 M1.5 起照搬了 CLI 的
  「恢复 SIGPIPE 默认处置」（`libc::signal(SIGPIPE, SIG_DFL)`），于是「写到一个对端已关闭的
  socket/pipe」不再返回 `EPIPE` 错误，而是**内核立刻杀掉整个进程**。真机形状：笔记本盒盖
  s2idle 2 分钟后唤醒，NAS 的长连接 / 会话保活连接全部失效，唤醒后第 20 秒第一轮
  轮询·保活去写旧连接即死亡（`code=killed, signal=PIPE`）；tracing 的 non_blocking 缓冲
  来不及落盘（日志断在死前几分钟），FUSE 挂载点与上传队列停在半路。现在改为**忽略
  SIGPIPE**（`SIG_IGN`，即 Rust 运行时默认处置），写坏连接退化成普通的 `EPIPE`，
  由各调用点按「这条连接坏了 → 重连」消化，进程继续活着。CLI 侧保持 `SIG_DFL` 不变
  （`qxync ls | head` 的惯例）。见 [docs/M37](docs/M37-休眠唤醒后守护被SIGPIPE杀死.md)。
- **systemd 单元不再「死了不拉起来」（M37）**。按 `systemd.service(5)`，`SIGHUP/SIGINT/
  SIGTERM/SIGPIPE` 被杀都算「干净退出」⇒ 那次死亡被记成 `Result=success`（`NRestarts=0`），
  `Restart=on-failure` 永远轮不到重启 —— 用户看到的就是「服务自己停了、得手动启动」。
  单元改为 `Restart=always`（唯一能覆盖 clean-exit 信号死亡的策略）+ `RestartSec=5` +
  `StartLimitIntervalSec=0`（重启永不放弃，默认 5 次/10s 后就躺平）+ `RestartMode=direct`
  （自动重启不白等 SIGTERM「挂载守护模式」那 30s）。⚠ 语义变化：`qxync daemon stop` /
  GUI「停止」（干净退出）也会被拉起来 —— 装了单元就只用 `systemctl --user` 管它。
- **「已有 daemon 占着 socket」不再被当成启动失败无限重启（M37）**。`bind_socket` 改为返回
  专用错误 `daemon::AlreadyRunning`，`main` 把它映射成退出码 **3**，单元用
  `RestartPreventExitStatus=3` 拦住 —— 否则 `Restart=always` 会把「你手动起过一个 daemon」
  变成每 5 秒一次的永久重启风暴（现场抓到过每 3 秒一轮的循环）。
- 新增回归测试 `crates/qxync-daemon/tests/sigpipe_survival.rs`：隔离 XDG 目录起一个
  「空转待命」的 qxyncd → `kill(SIGPIPE)` → 断言进程仍活着**且 IPC 仍应答**。

## [0.7.10] - 2026-10-25

### 修复

- **回放顺序不再依赖隐式时序：outbox 前缀改写把行重排到队尾（M36）**。旧实现
  `outbox_rewrite_prefix` 只改 `path`、保留 `seq`，而改写会把行指向**新的父目录**——
  新父目录的 `mkdir` 行可能排在原 seq 之后（真机形状：先建 `/p/a`、后建 `/b`、
  再建 `/p/a/sub`，最后把本地新建的 `/p/a` 改名到 `/b/a2`），回放顺序就变成
  「子项排在新父目录前面」，只能靠「等结构操作」兜底一轮轮烧 `stat`。现在改写行
  在**同一个事务**里删除、按原相对顺序重插到队尾（`attempts` / `created_unix` /
  `target` 原样保留），未被改写的行 seq 不变 →「祖先先落地」由 seq 直接保证。
  回放侧的 `Pending` 兜底保留（旧版本写下的行序仍要能解开）。
  见 [docs/M36](docs/M36-回放顺序与远端副本证据.md)。
- **「远端有没有副本」不再用 `dirty` 猜：改成持久化证据（M36）**。改名决策里的
  `unwrap_or(!dirty)` 把「本地有没有未上传改动」当成了「NAS 上有没有这个名字」——
  两者是不同事实。重启后节点按持久化清单懒物化，而清单分不清「NAS 条目」与
  `create()` 写进去的本地新建条目：一个本地新建、还没上传的 KIO `.part` 会被当成
  远端已有，紧接着的改名（`.part` → 正式名）发一条**永远不成立**的远端 rename
  （在线 `EIO`、离线重试到放弃）。现在 `nodes` 表新增 `local_only` 列（schema v7，
  v6 老库自动补列）：本地新建置 1、上传成功清 0、改名/删除跟着行搬；按清单物化
  节点与改名决策都读它，`dirty` 不再参与。顺带补上 M34 清单叠加的跨重启来源——
  一次远端清单刷新不再把本地新建的文件从 `ls` 里抹掉。
  见 [docs/M36](docs/M36-回放顺序与远端副本证据.md)。
- **目录列表快照不再被远端列举「整份换新」→ 本地新建、还没同步上去的条目
  不会再从 `ls` 里消失（M34）**。真机事故（2026-10-25）：离线建的 `取证1` 目录还在
  重放队列里，NAS 那次列举里当然没有它，而快照被整份覆盖成空 → Dolphin 里目录
  凭空消失；再建同名目录又拿到「已存在」（节点表里还有它，内核 `lookup` 命中 →
  `mkdir` 直接 `EEXIST`）—— 既看不见、又建不了。现在所有「快照 = 远端清单」的
  写入点（daemon 轮询推送 / TTL 后台补拉 / 冷路径列举 / 挂载预热）都先合并
  `远端全集 ∪ 本地未同步新建 − 本地待删`，并且写穿持久化的就是合并后的视图
  （离线重启同样可见）。本地新建的**文件**也覆盖（`create` 时父目录还没有快照的
  场景）；远端真删除仍然生效。
- **离线缓冲区回放不再把 `status=5`（路径不存在）当成会话失效去重登**。真机
  2026-10-25：子目录的 `stat` 因为父目录还没回放而回 5，旧分类器判成会话失效 →
  每 3 秒真登录一次（6 分钟 95 次「登录成功」，会撞上 NAS 的失败登录封锁），20 次
  后把 5 条离线操作整行删掉。现在：父级结构操作还没落地时判为「等待」—— 不计数
  重登、不中断本轮（父目录的 `mkdir` 往往就排在后面几条，同一轮就能建出来）。
  另外 `setmtime` 的目标还在上传队列里时直接跳过（上传落地会自己对齐 mtime）——
  这是旧实现的死锁：上传队列在 outbox 清空前被冻结，`set_mtime` 又只在文件已上传
  后才可能成功，只能靠「20 次假重登后放弃删行」解开。
- **M36 独立复核补的四个洞**：① 重启后清单叠加原来会拿 `create()` 时的 `size=0`
  `nodes` 行盖掉正确条目（`read` 静默 EOF、追加写按陈旧 size 截断缓存）→ release /
  setattr 定稿时把 `nodes` 行刷到最终签名；② 目录改名只搬目录自己一行 `nodes`
  （清单搬整棵子树）→ 新增 `node_rename_subtree`，子项的 `local_only` 证据跟着搬；
  ③ 删除本地新建的目录不撤销 outbox 的 `mkdir` 行 → 回放/清单叠加把它「复活」，
  现在删除时按同一套证据跳过远端删除并撤销结构行；④ `outbox_rewrite_prefix`
  并发回放时会把已 `outbox_done` 的行重插（复活/重复）→ 按 DELETE 影响行数跳过。
  详见 [docs/M36](docs/M36-回放顺序与远端副本证据.md) §2.1。
- **M36 二轮复核打磨**：`nodes`/`outbox` 子树前缀匹配从 `LIKE`（默认大小写不敏感）
  改成 `substr` 比较 —— `/home/a` 不再误删/误搬 `/home/A/...` 的行与回放意图；
  `node_rename_subtree` 对「目标嵌套在源里 / 目标是源的祖先」显式拒绝；
  `refresh_listing_entry` 改为**先**写 `nodes` 行且写前重读当前路径（并发
  rename/unlink 不会把行写回旧 path）；`link()` 的 O_TMPFILE 物化补上
  `local_only` 记账；`node_local_only_under` 去掉 `ORDER BY`，让局部索引真正生效
  （3 万行实测 1.125ms → 0.006ms）。详见
  [docs/M36](docs/M36-回放顺序与远端副本证据.md) §2.2。

## [0.7.9] - 2026-10-23

### 变更

- **GUI「下载到本地」的进度提示改为右下角浮动卡片（[M33](docs/M33-右下角下载进度卡片.md)）**。
  用户反馈「下载大文件（>3 秒）右下角没有弹出下载进度提示框」：M26 的进度链路本身是通的
  （daemon 推进度行 → GUI emit → 前端监听），但展示挂在文件页工具栏的「进行中…」小徽标上
  ——字号小、位置不显眼、切到别的页就看不见，真机上等于没有进度提示。现在：
  - 下载超过 3 秒（静默期，与 CLI 同一套规则）在**窗口右下角**浮一张卡片：文件名 +
    进度条 + 百分比/已下载/速度/剩余时间；多笔并发各自一张、纵向堆叠；
  - 容器 `pointer-events: none`（只有卡片本身可点、点击即关），不挡下面的页面交互；
    `position: fixed`，任何页面都可见；
  - 未知总量（服务端 chunked）进度条走 indeterminate 滑动动画；
  - 成功 → 绿色「✅ 下载完成 · N」停留 2.5s 自动消失；失败 → 红色 + 错误摘要（完整
    错误在 title）停留 6s；快下载（<3 秒）从头到尾无痕；
  - 徽标 `#files-busy` 保留为通用「进行中…」指示，不再覆写进度文本（进度只有卡片
    一个真相源）；Rust/daemon 侧零改动。
  - 验证：源文本级单测 9/9（静默期/节流/滑窗/未知总量/finish 语义）+ 无头 chromium
    集成测试 17/17（真实走「文件页 → 右键 → 下载到本地… → 弹窗确定」用户路径，
    断言卡片浮出、fixed 贴右下角、pointer-events、done 态、自动移除）；
    `cargo test --workspace` 540/0/22，`ui_spec` 自检通过。

### 修复

- **重启 / 重新挂载后「缓存好像没了、水合过的文件又变回白云」
  （[M31](docs/M31-重启后缓存状态与签名自愈.md)）**。真机取证：缓存目录里 392 份
  `.qxstate`、386 份全量就绪、约 495 MiB 内容**一份没丢**，但重挂后
  `qxync status` 显示「已缓存 7.6 KiB / 水合文件 1」、Dolphin/角标全变 placeholder，
  读一个 5.26 MiB 已缓存文件还真的回源下载了 41 个区间。两个独立缺陷：
  - **水合状态只在第一次 `read` 时读回**：位图落盘了，但读回它的唯一入口是
    `cache_file_for`。daemon 重启后节点表重建、`chunks_done` 全空，而
    `user.qxync.state` / `user.qxync.overlay` / `user.xdg.tags` / `status` 的缓存统计 /
    GUI 文件页三态全看内存节点 → 整列已缓存文件变成占位符。
    修：挂载时 `scan_cache_index()` 扫 `<cache>/*.qxstate`（**只读头 + 位图，不读内容**）
    重建水合索引；节点物化时按签名预填区间表（零网络）；`cache_stats` 改从索引统计，
    「共 N」分母用 baseline；GUI 文件页三态在节点未物化时用 `cached_bytes_for` 兜底。
  - **`mtime` 被改后位图签名不更新，重启后认领必然失配 → 整份重下**：
    `cp -a` / `rsync -a` 拷入挂载点时时序是「写完 → `release` 落位图（记写盘时刻）→
    工具再 `utimensat` 把源文件时间戳设回」，`setattr` 更新了节点/清单/远端却**没有
    回写 `.qxstate` 头** → 位图和清单签名永久分叉（真机 326/392 命中）。
    修：① `setattr` 只改 mtime 时 `resync_cache_signature()` 同步回写位图头，
    在线路径同步补上 `update_pending_mtime`（与离线路径一致）；② 认领新增第二种容差：
    清单 mtime **更旧** + 全量就绪 + 逐区间 `xxhash64` 全过 → 认领并把签名对齐清单；
    远端更新方向仍然拒绝、校验和不过仍然回源 —— 「绝不返回错误内容」的红线不动。
  - 验证：新增 3 个单测（重启未读即 `hydrated`、签名更旧认领/更新拒绝、mtime 同步）；
    `cargo test --workspace` **537 passed / 0 failed / 22 ignored**；`cargo fmt --check` 干净。

- **本地写入的校验和与内容分叉 + 改名丢校验和表（[M32](docs/M32-写入校验和与缓存搬运.md)）**。
  真机 `qxync verify` 报 403 个已水合文件里 **198 个「内容损坏」**，但抽查文件内容与 NAS
  重新下载的字节 **sha256 完全一致**——内容没坏，坏的是 `.qxsum`（逐区间 xxhash64 表）。
  两个独立缺陷：
  - **未对齐写入被当成整区间写完**：`mark_written_chunks()` 用 `min(区间尾, EOF)` 当区间尾，
    新建/追加文件第一次写进区间的几 KB 就被当成整区间内容算了 hash，之后追加/补写不再
    重算 → `.qxsum` 永久停在第一版。真机证据：19 个单区间损坏样本全部满足
    「盘上 hash == `xxh64(内容前 N 字节)`」，N 正是那个程序第一次写的长度（1 / 4096 /
    16000 字节）。
    修：`Node` 新增 `stale_sums`，部分覆盖的区间只记脏、不拿半截内容算 hash；新增
    `finalize_written_chunks()` 在 **`release` / `flush` / `fsync`** 时按最终内容一次补齐
    （只读脏区间，整块对齐的 `cp` 零额外开销）。
  - **改名 / O_TMPFILE 物化只搬「内容 + 位图」，漏了 `.qxsum`**：新名字于是「有位图、
    没基准」（T3 判损坏、白重下）或「新内容 + 旧文件的基准」（每次都判损坏）。真机
    `22 份孤儿 .qxsum` 全挂在 `.part` / `.tmp` / `wpsdiag_*` 这类改名前的临时名上。
    修：`move_cache_triplet()` 三件套一起搬（先清目标位置残留的位图/校验和，宁可没基准
    也不能拿错基准）；硬链接（复制语义）分支同时复制 `.qxsum`。
  - 验证：新增 3 个单测（未对齐写 → `finalize` 后校验和与内容一致、对齐写零脏区间、
    改名搬三件套并覆盖目标位置旧基准）；已损坏的 198 份 `.qxsum` 首次读时按 T3 重下
    一遍即自愈（内容本来就是对的），或用这些程序再保存一次会自动补算。

## [0.7.8] - 2026-10-21

### 新增

- **首次同步 / 太久没同步 → 按「全量列举」处理（[M29](docs/M29-首次同步与全量重扫.md)）**：
  变更发现的主路径是「列目录 + baseline 差集」（真机 `qbox_get_sync_log` 恒返回 `-17`），
  而旧实现把**第一次同步**也塞进常规轮转窗口 —— 真机 4964 个目录 × 每轮约 171 个 =
  29 轮（约 24 分钟），期间所有条目都在「本轮暂不判定」，用户看到的是永远同步不完。
  现在按挂载点记状态，三种情况走全量：
  - **首次同步**（没有 `reconcile_full_done:<mountpoint>`）→ **一轮 BFS 列完整棵树**
    （每个目录只列一次；不受轮转游标、常规单轮预算、2000 个目录硬顶的截断），
    完成后落完成标记，自动**恢复常规轮转与用户配置的轮询间隔**；
  - **太久没同步**（距上次**完整跑完**一轮 ≥ 1 小时；`sync_last_ok_unix` 只在
    「事件 + 对账 + 巡检」完整跑完时更新，断网/休眠不会把它拨到现在）→ 按首次同步处理；
  - **上次没列完**（崩溃 / 断网 / 预算用完）→ `reconcile_full_pending` 标记，下轮继续；
    预算用完时轮询循环 **2 秒**后就继续，不等 30s/600s 的常规间隔。
  - 全量单轮预算 1800s（`QXNYC_FULL_SCAN_BUDGET_SECS`，`0` = 不限）；
    `QXNYC_FULL_SYNC_AFTER_SECS` 调「太久」阈值（`0` = 不因超时触发）；
    `QXNYC_FULL_SYNC=0` 退回旧行为（排障对比用）。
  - 状态可见：`SyncInfo` 新增 `full_sync_pending` / `last_sync_ok_age_secs`；
    `qxync status` / `qxync sync` 和 GUI 同步摘要显示「全量同步：进行中（首次 / 太久没同步）」
    与「上次成功同步 N 前」。
  - 真机只读实测（5362 目录账号）：约 10.9 目录/秒（92ms/目录），一轮全量约 8~9 分钟
    —— 而不是旧行为的 24 分钟轮转一圈且期间无法判定删除。

### 修复

- **网络失败不再刷屏、不再带路径/URL/sid：一次闪断由重试吸收，真断网按「已缓冲，等联网」
  表达（[M30](docs/M30-网络失败缓冲与错误降噪.md)）**。真机用户的三句话：「咋还在报网络错误」
  「本地路径和远程路径都不要显示」「放到缓冲区去等在线」。
  - **幂等 GET 有限重试**（`get_list` / `stat` / `qsyncsrv.cgi` 的 GET / `settime`）：
    最多 3 次尝试（200ms → 600ms 退避），只重试「对端提前关连接 / 协议中断 / 反代
    502/503/504」；连接超时、连接拒绝、DNS 失败**不**重试（这些是离线判据，重试只会把
    20s 的 connect timeout 拖成 60s）。被吸收的闪断轮次**零错误**，用户看不到任何东西。
  - **文案去掉 URL / sid / 本地与远端路径**：client 把 `reqwest::Error` 压成
    「哪类失败 + 最底层 io 原因」（例如 `请求发送失败：Connection reset by peer (os error 104)`），
    `reqwest` 自带的完整 URL（含 `sid=` 与 URL-encoded 远端路径）不再外泄；`get_list`/`stat`
    的错误上下文不再拼路径；对账失败文案改为「同步「任务名」对账列举失败（第 N/M 个目录）：…；
    已缓冲本轮进度，网络恢复后自动继续」，「哪一路同步」用挂载点目录名（任务名），路径只进
    `tracing::debug` 日志。
  - **网络失败进「说明」、不进「最近错误」**：`absorb_round` 只把**非网络**错误写
    `last_error`；网络失败写 `last_note`（`qxync status` / `qxync sync` / GUI 同步页与诊断页
    都显示「说明：…已缓冲…」）。断网不是「同步坏了」，不该在界面常驻红字。
  - **journal 按断网签名去重**：同一次断网只写一条 `warn`（重复轮次降 debug），恢复时补一条
    `ok`「网络已恢复：同步继续」，更新中心不再每 30s 多一条。
  - 数据安全不变：列举失败的目录仍**不判定**、不按缺失删本地、不删缓存。

### 验证

- `cargo test --workspace --locked` 全绿（**534 通过 / 0 失败 / 22 忽略**；新增 5 个全量同步
  用例 + 1 个 `#[ignore]` 真机只读冒烟；M30 新增 3 个用例：闪断重试吸收 / 文案不含路径
  URL sid / 网络失败只进说明）；
- `cargo fmt --all --check` 干净；`python3 xtask/release/check-version.py v0.7.8` 通过。

## [0.7.7] - 2026-10-21

### 修复

- **v0.7.6 的 daemon 二进制启动即 panic：`log` 桥与 `LogTracer` 抢全局
  logger（CLI / GUI 本身正常，但没有 daemon 一切功能不可用）**。v0.7.6 为把
  fuser 的诊断日志接进 daemon 日志，在 `init_logging()` 里先
  `log::set_logger(&FuserLogBridge)`，随后
  `tracing_subscriber::registry().init()` —— 而 `init()`（默认 feature 含
  `tracing-log`）内部还会调 `LogTracer::init()` 去占 `log` 全局 logger，
  两者撞车 → `SetLoggerError` → `.expect()` panic。daemon 化后 fd 0/1/2 已
  指向 `/dev/null`，panic 无声无息：用户只看到 `qxync daemon start` 报
  「qxyncd 已派生但 socket 未就绪」、日志零线索。
  - 改用 `tracing::subscriber::set_global_default()`，绕开 `init()` 内置的
    `LogTracer`；`log` → `tracing` 桥继续由 `FuserLogBridge` 负责，fuser 的
    诊断日志照常可见；
  - **日志文件初始化失败不再 panic**：`tracing_appender::rolling::daily()`
    内部 `.expect()` 在日志文件建不出来时同样会 panic（同样被 `/dev/null`
    吞掉）—— 改用 builder 拿 `Result`，失败降级为只写 stderr（并在 stderr
    上留一条警告）；
  - 新增 `crates/qxync-daemon/tests/logging_init.rs` 防复发：`--print-config`
    存活回归（启动含日志初始化并正常退出）+ 日志目录不可写时降级 stderr。
- **v0.7.6 的 Release 资产（`qxyncd`）作废，本版重新构建全部资产**：同一
  缺陷使 v0.7.6 的 `qxyncd` 无法启动，Release 包里的整套程序不可用；已装
  v0.7.6 的用户请直接升级到 v0.7.7。

## [0.7.6] - 2026-10-21

### 修复

- **WPS 表格保存报「在替换目标文件时发生错误 (0x80000008)」—— FUSE 层补上
  O_TMPFILE 与 link**：WPS（以及新版 LibreOffice、Qt 5.14+ 的 `QSaveFile`）的
  原子保存是「`open(dir, O_TMPFILE)` 写内容 → `linkat(fd, "", dir, name,
  AT_EMPTY_PATH)` 原子替换」，而 qxync 两个操作都不支持：fuser 0.17 的 opcode
  枚举到 47 为止（内核协议 7.37 的 `FUSE_TMPFILE`=51 解析失败 → 回 ENOSYS →
  内核转 `EOPNOTSUPP`），`link` 则压根没实现（fuser 默认回 `EPERM`）。真机诊断
  （`wps_save_diag`）实测：`open(O_TMPFILE)` → errno 95、`link()` → errno 1，
  而普通 rename（LibreOffice 的保存方式）一直正常。修复分三层：
  - **vendor/fuser（本地补丁，`[patch.crates-io]`）**：opcode 枚举补
    `FUSE_TMPFILE = 51`（顺带 `FUSE_STATX = 52` 显式回 ENOSYS，保持原状），
    新增 `Operation::TmpFile` 解析（请求体 = `fuse_create_in`、无名字参数，
    与 FUSE_CREATE 同构）与 `Filesystem::tmpfile()` 回调（默认 ENOSYS，
    内核据此回 `EOPNOTSUPP` —— 与上游未打补丁时的行为一致）；
  - **`tmpfile()` 实现**：建「未命名」节点（`Node.unnamed`：空 remote、不进
    `by_remote`、不进目录清单、`mark_dirty` 只置脏不入队 —— 上传发生在 link
    物化之后），缓存文件用 `tmpfile-<ino>.cache`；`release()` 时仍未命名 →
    整个丢弃（节点 + 缓存）；
  - **`link()` 实现**：对未命名节点 = **物化**（搬到目标名字、迁移缓存、进清单、
    标脏上传 —— 正是 `linkat(AT_EMPTY_PATH)` 的语义）；对普通节点 = **复制语义**
    （NAS 协议没有硬链接，与 CIFS 等网络文件系统同例：新节点 + 拷贝缓存 + 上传）。
    目标已存在 → `EEXIST`（内核 VFS 通常已挡）。
- **renameat2 的 flags 不再静默忽略**：原先 `RENAME_EXCHANGE` 被当普通 rename
  执行（源名字的内容被覆盖而不是交换 = 静默数据丢失）。NAS 协议做不了原子交换
  （3 步换名有中间态），按网络文件系统惯例（sshfs/CIFS 同样不支持）明确回
  `EINVAL`；`RENAME_NOREPLACE` 目标已存在时回 `EEXIST`（内核通常已挡，兜底）。
- **fuser 的诊断日志不再蒸发**：fuser 用 `log` crate 输出（「[Not Implemented]
  link(...)」「Unknown FUSE opcode」等），而 daemon 只初始化 `tracing` ——
  两者之间没有桥，**所有 fuser 层错误全部静默丢弃**（本次故障查了整轮日志零
  线索的直接原因）。daemon 启动时装 `log` → `tracing` 桥（target=`fuser`），
  以后这类问题日志里直接可见。
- **Dolphin 复制报「磁盘上没有足够的空间写入 …」—— 补上 `statfs`，回缓存盘
  真实空间**：挂载点此前从未实现 `statfs`，走 fuser 默认值（blocks/bfree/bavail
  全 0），`df` 显示 0 B 可用；而 Dolphin/KIO 在复制**前**会查目标挂载的可用空间
  （`FileSystemFreeSpaceJob`，KDE bug 492108 里没回真实 statfs 的 davfs2 挂载
  同症状），看到 0 直接判 `ERR_DISK_FULL`。写入实际落在本地缓存盘（空间充足），
  `cp` / qxync put 不做 statvfs，所以「命令行可以、Dolphin 不行」。现在
  `Filesystem::statfs` 对 cache_dir 跑 `statvfs(3)` 并映射到 FUSE 字段：
  缓存目录尚未创建时沿祖先目录找第一个存在的（挂载早于建目录的竞态）；块粒度
  优先 `f_frsize`（df 也按它算），为 0 时退回 `f_bsize`；`f_namemax` 为 0 时按
  惯例兜底 255。**报缓存盘而不是 NAS**：写入最终落进 cache_dir，「往这个挂载点
  写还剩多少」就是缓存盘剩余；NAS 侧配额不在 statfs 语义里，由上传失败回报。
  量不到时维持旧行为（全 0）并留一条 warn，不编造数字。注意：挂载线程在
  qxyncd 里，daemon reload（SIGHUP）只重载同步引擎，此修复要 stop/start
  daemon（或重启）才生效。

## [0.7.5] - 2026-10-21

### 新增

- **托盘三色状态点 + 网络错误降噪 + 冲突弹窗（M28）**：断网/波动不再弹窗、不再进
  错误列表 —— 网络类错误在 journal 里降级为 `warn`（`kind=network`，全量视图仍可查），
  桌面通知与错误列表只保留真正的同步错误；连接状态改由**托盘图标右下角的状态点**
  表达：🟢 全部正常 / 🔴 探活确认网络断 / 🟡 网络通着但有错误或删除熔断；没有
  daemon 在跑时保持原图标（不误报）。tooltip 同步更新（「在线」/「离线：连接X」/
  「注意：…」），且只在状态变化时才动 SNI 图标（不刷 D-Bus）。同步产生新冲突时
  发一条桌面通知（「双方都改」的条目才算新冲突，队列清理不弹；尊重「桌面通知」
  设置）。首页/任务卡在离线时不再叠一条「同步出错」红字（横幅已说「离线 + 攒着
  N 项」），改为「网络断开 —— 本地改动先攒着，联网后自动同步」。判据走结构化标记
  （`SyncReport.error_is_network`，与 errors 逐条对齐含 20 条截断），不是事后从
  文本猜；老 daemon 由前端按文案兜底识别。详见 `docs/M28-托盘状态与错误降噪.md`。

- **Dolphin 角标插件的 deb 分发（kf5 / kf6 两个包）**：Debian / Ubuntu 矩阵横跨
  KF5 / KF6 两个世代（Debian 12 / Ubuntu 22.04 / 24.04 的 Dolphin 是 KF5，
  Debian 13 / Ubuntu 26.04 是 KF6），插件 `.so` 的 ABI 绑 KIO 大版本，一个包
  盖不住 —— 发版流水线新增两个资产：`qxync-dolphin-overlay-kf5_<版本>-1_amd64.deb`
  （ubuntu:22.04 基线编）与 `qxync-dolphin-overlay-kf6_<版本>-1_amd64.deb`
  （debian:13 基线编），KF 系列内向后兼容，合盖五个发行版，CI 每次发版在
  debian:12 / 13、ubuntu:22.04 / 24.04 / 26.04 容器里实装校验。插件源码改为
  **Qt5/Qt6 双兼容**（`KOverlayIconPlugin` 是同一接口；KF5 的头文件在
  `KF5/KIOWidgets/` 下、接口实体也在 KIOWidgets —— CMake 的 KF5 分支链
  `KF5::KIOWidgets`，`dpkg-shlibdeps` 才能算出 `libkf5kiowidgets5` 依赖）；
  `contrib/dolphin/overlay` 的 CMake 加 `-DQXYNC_KF_MAJOR=5|6`（缺省 6，
  Arch / KF6 发行版行为不变）。本地已实测双世代 QPluginLoader 加载
  （`instance()` + `qobject_cast<KOverlayIconPlugin*>`）与五发行版实装全绿。

### 修复

- **断网时不再「一遇到问题就报错」：先探活确认，再只报一条并立刻收手**：真机日志
  里一次断网会在一轮轮询内被记成 `qbox_get_sync_log` + `qbox_get_device_config_list`
  + `qbox_query_notify` + 对账的**每个目录**各一条 WARN / journal 错误 —— 每个失败
  还要各自等一次 connect timeout，用户看到的是「程序到处报错」，而事实只有一句
  「网断了」。现在任何一步遇到**网络层**错误都先花 5 秒探一次 NAS
  （`Client::probe_network`，不需要会话）：探活也失败 = 整台不可达 → **立刻中止
  本轮**，只记一条带 NAS 地址、「已做到哪一步」、「恢复后自动继续」的错误，并跳过
  其余挂载视图与待裁决巡检；探活通过 = 单条请求被丢（闪断）→ 继续，但网络类
  错误每轮最多记 3 条（事件与对账共用一份配额），其余降为 debug。中止的轮次按「未完成」并入长期状态（不清
  上一轮的真实告警）；事件游标只推进到**已完整应用的那一批**——正在处理的那批下一轮
  会整批重放，安全性靠 `apply_event` / `apply_decision` 的幂等性（回归用例
  `poll_once_abort_mid_batch_then_replay_is_idempotent` 钉住；见下一条的复审修正）。
  离线恢复探活同时加了退避：
  刚断网仍是 3 秒一轮（拔网线 5 秒仍秒级恢复），离线 30 秒后 10 秒、5 分钟后 30 秒
  （不再拿关机一整夜的 NAS 做压力测试）。
- **复审修正：探活结论不再「整轮一锤定音」，探活=可达的闪断不再把 daemon 钉成离线**：
  * **探活结论带失效期**（10 秒）并且**连续 3 次网络失败后强制重探**。旧实现一次
    探活成功就整轮复用 `true`，弱网下 5 秒侥幸通过、随后链路彻底恶化时，剩下的事件
    会逐条 stat、逐条等 connect timeout（一轮可能挂几十分钟，还一直握着
    `sync_run` 锁）。现在最坏一分钟内就能重新确认并中止。事件阶段的网络失败与对账
    阶段共用**同一份**每轮 3 条的错误配额（以前事件阶段没有上限，且配额是每视图
    3 条）；对账里「列举 4/5/6 且 stat 也失败」、冲突处理中重新 stat 失败也按同一
    口径收敛（后者上游刚成功 stat 过，按闪断处理、下轮重试）。
  * **连通性按探活结论判定**：`qsync_func` 把任何 HTTP 非 2xx 都映射成 `Transport`，
    而探活只要求「base_url 有回应」——CGI 持续 500 / WAF 403 / NAS 封客户端 IP 时，
    旧逻辑会把 daemon **长期钉在离线态、上传删除长期冻结**。现在探活说可达就保持
    在线（照常记错误），只有探活失败才置离线、冻结队列；离线原因单独存放
    （`offline_reason`），不再被网络错误之后记下的本地错误顶掉。
  * 冲突处理里的网络调用失败（在途上传结束后重新 stat、冲突副本取名、远端改名）也接上
    `NetGuard`：网络层失败会探活、确认不可达即中止，不再「每个冲突各等一次
    connect timeout、最后还判在线」；单次失败仍按闪断处理（debug + 去重提示）。
    ACL 隐藏目录（列举说不存在但 stat 说存在）的告警改为**整轮 3 条**上限；
    `stats.events` 改为整批应用完之后才累加（中止的批下一轮会重放，避免同一个
    事件被计两次）。新增两个回归：`poll_once` 级「批中途断网 → 中止 → 同游标重放整批 → 全部
    Noop、不删任何东西、不产生第二个冲突副本」，以及「探活侥幸通过后连续失败会
    重新探活并中止」。

## [0.7.4] - 2026-10-20

### 修复

- **Dolphin 删除报「无法写入文件 …trashinfo」（M27）**：NAS 上遗留了
  `.Trash-1000` 目录时（旧版挂载时代 Dolphin 建出来的），KIO 的卷内回收站探测
  `stat(".Trash-$UID")` 通过、写 `.trashinfo` 才被 `EPERM` 拒 —— 用户看到的是
  报错弹窗（真机 2026-10-19/20 日志三次复现）。修法：回收站路径**读侧也藏掉**
  （`lookup` 回 `ENOENT`、`readdir` 不列出），探测在第一步就拿到「不存在」→
  mkdir 被拒 → KIO 走它自己的既定回退：**「回收站在此项目的位置不可用。改为
  彻底删除它吗？」确认框**（KIO 6.21+ 的 `ERR_TRASH_NOT_AVAILABLE` 路径，
  任何不提供回收站的文件系统在 KDE 里都是这个体验）。写入口的 `EPERM`
  纵深防御原样保留；判定沿用 v0.4.3 的段级精确匹配（`.Trash` / `.Trash-$UID` /
  `@Recycle`），正常名字不受影响。A/B 对照实测：旧行为在真实生产挂载上精确
  复现报错（`KIO::DeleteOrTrashJob` 路径），新行为弹确认框。

## [0.7.3] - 2026-10-20

### 新增

- **`get` 下载进度：超过 3 秒才亮，快下载输出零变化（M26）**：下载超过 3 秒时在
  stderr 原地刷新「百分比 + 进度条 + 已下载/总量 + 速度（3 秒滑窗）+ 剩余时间」，
  3 秒内完成的下载一个字都不多打（输出与之前完全一致）。直连与经 daemon 两条路
  都有：daemon 侧由请求信封的 `progress:true` **能力声明**触发（老客户端不声明 →
  一个进度行都不发，行为与之前一字不变；新客户端打老 daemon 也安全退化），进度行
  与最终响应在同一条 socket 上混发、**超时按行重置**（有进度流动就不掐 600s 总闸）。
  GUI「下载到本地」同步接入：进度亮在文件页的「进行中…」徽标上（不弹窗挡操作），
  老版本 daemon 下徽标保持原样。stderr 非终端（重定向/管道）时 CLI 不声明进度、
  daemon 不建通道不推行，`qxync get > log` 保持纯净。服务端 chunked（无
  Content-Length）时退化为「已下载 + 速度」，CLI 直连另用 `stat` 的远端大小兜底总量。
- **角标插件安装检测（M25）**：主程序现在能回答「角标到底会不会显示」——
  `qxync baloo_status`（IPC）新增 `overlay_plugin` 字段、`qxync baloo-status`
  多一行「角标插件」（已装+路径 / 装在 `~/.local` 但 `QT_PLUGIN_PATH` 没包含
  → Dolphin 不会加载 / 未装+装法提示）、GUI 设置 → 高级的检测表同步显示。
  检测规则与 Qt 的插件查找对齐（`QT_PLUGIN_PATH` → 系统目录 → `~/.local`），
  CLI 侧是**本地文件检查**，不依赖 daemon 版本；GUI 对老 daemon 的
  「字段缺失」不显示该行（不误报「未安装」）。检测只提示不拦开关（xattr 无害）。
- **发版流水线产出 `qxync-dolphin-overlay-<版本>-1-x86_64.pkg.tar.zst`**：
  Dolphin 角标插件的 Arch 包（AUR 账号注册好之前的分发渠道，`pacman -U` 装）。
  在 `archlinux:base-devel` 容器里对着**当次 tag 的源码归档**现编（与
  `packaging/arch-dolphin-overlay/PKGBUILD` 同一份配方，CI 先替用户验一遍），
  逐项校验包内容（.so + 三个 SVG）后挂到 Release。`check-version.py` 同时守
  该 PKGBUILD 的 `pkgver` 与 tag 对齐。

### 修复

- **`qxync settings --set dolphin_overlay=...` 报「未知设置键」**：M24 新增字段时
  漏了 `Settings::set_kv` 的键表 —— GUI 勾选与 IPC `settings_save` 都正常，只有
  CLI 直连设置被挡在 core 的键校验上。补上这条 arm 并加回归测试，报错的「可用键」
  清单也同步列出 `dolphin_overlay`。

## [0.7.2] - 2026-10-20

### 新增

- **Dolphin 角标：文件图标上的彩色水合状态（M24，可选插件 + 独立开关）**：
  绿对钩 = 已水合、蓝云 = 仅在线、半满圆 = 部分本地，和 Windows 资源管理器 /
  CFA（Cloud Files API）同一种表达。`contrib/dolphin/overlay/` 是独立 CMake 工程
  （Qt6 Core + KF6 KIO，**不参与 cargo 构建**、不进 release 产物），在 Dolphin
  的文件图标左下角画 `KOverlayIconPlugin` 角标；只对 `fuse.qxync` 挂载点里的
  本地文件读 xattr（其它路径一次都不读）。这种角标机制**只有 Dolphin / KDE 支持**，
  所以做成与 Tags 列**相互独立**的开关 `settings.dolphin_overlay`（默认关，
  GUI 设置 → 高级）：开关开 → FUSE 暴露 `user.qxync.overlay`
  （placeholder / partial / hydrated，目录为空串），插件据此选图标；开关关 →
  属性不存在（ENODATA），插件不画角标。两个开关共用 Baloo 前置门禁与「按挂载点
  判定」，热切换不重挂载；`baloo_status` 顺带热刷新挂载点标志（在终端手动排除
  Baloo 后点「重新检测」即生效）。详情视图 16px 图标下角标 8px（可辨认，放大
  图标尺寸会更大）。

### 变更

- **Tags 列的值从文字改成图标字符**：`user.xdg.tags` 现在写 `✓` / `☁` / `◐`
  （原为 `已水合` / `仅在线` / `部分本地`）。Tags 列只能显示文字、字号与正文相同、
  没有颜色 —— 中文词在满屏文件名里不显眼。机器可读状态仍看 `user.qxync.state`
  （值没变）；有脚本解析过 tags 的需要跟着改（角标插件已改读专用的
  `user.qxync.overlay`，不受影响）。

## [0.7.1] - 2026-10-20

### 新增

- **Dolphin 集成：水合状态进 Tags 列（M23）**：挂载点里每个文件把水合状态
  （仅在线 / 部分本地 / 已水合）动态写进 freedesktop 标准的 `user.xdg.tags` xattr，
  Dolphin **自带的 Tags 列**直接显示 —— 不用写插件、不用改 Dolphin。分四块：
  * **开关（默认关）**：`settings.json` 新增 `dolphin_tags`（`qxync settings --set
    dolphin_tags=true` 或 GUI 设置 → 高级）；关着时 xattr 名单里根本没有它，
    M23 之前的行为一字不变；
  * **Baloo 门禁**：开启前 daemon 强制检查**每个挂载点都已排除出 Baloo 索引**
    （内容索引器读文件 = 水合，没排除就开 tags 等于邀请它把整个 NAS 拉到本地）。
    新增 `qxync baloo-status`：Baloo 装没装 / 索引开关 / 每个挂载点的结论与
    排除命令。判定对齐 Baloo 自己的语义：`Indexing-Enabled` 缺省视为开、
    排除覆盖自身与后代、include 列表与挂载点取双向祖先交集；
  * **GUI 一键排除**：预检不过 → 弹确认框 → 「打开终端，自动添加排除」→
    konsole（`--hold`）里逐条自动执行 `balooctl6 config add excludeFolders
    <挂载点>` 并列出最终排除清单，跑完回来重试保存即可；
  * **热切换 + 按挂载点生效**：开关与 FUSE 共享 `Arc<AtomicBool>`，改设置不用
    重挂载；每个挂载点独立判定「设置开了 && 不会被索引」，一个没排除的点不会
    牵连其它挂载点。`mount` 响应新增非阻塞的 `baloo_warning`（挂载点会被索引就
    提醒，与开关无关 —— Baloo 自己就会读文件）。
  限制（调研文档 §5）：状态不实时刷新（F5 / 重进目录）；依赖 Baloo 全局开关；
  进不了 `tags:` 虚拟目录；挂载点上加不了用户自己的标签（`setxattr` 有意不实现）。

- **离线可重启 daemon（M21）**：断网时重启 daemon（或断电后开机），挂载点**立即**
  恢复（真机实测 1 秒）且离线可用 —— 不再「等联网登录成功才恢复」。分四块：
  * **无会话挂载**：`mount()` 不再要求已登录。挂载本身不需要 NAS（需要网络的是
    水合与上传，而这两样离线时本来就有各自的诚实降级）；已知离线时连登录都不试
    （省 20s connect timeout）。登录成功后 sid 由 `push_sid_to_mounts` 热推进
    恢复的挂载点，无需重挂；
  * **目录清单持久化**（`sync.db` schema v5 → v6，新增 `listings` 表）：M9 的
    清单快照从「只在内存」改为写穿状态库、挂载时整份装回 —— 离线重启后 `ls`
    吃持久化清单、已水合文件靠缓存 + `.qxstate` 位图认领（离线可读）、写操作
    照旧进缓冲区。清单是「最后一次已知内容」：联网后轮询整份换新，丢行只是
    少看见、不丢数据；
  * **清单签名一致性**：create 的占位条目（size=0）在 release / setattr 时按
    最终签名刷新；上传成功钩子把「刚传上去的那版」刷进清单（关掉 create 与挂载
    预热的竞态）；认领遇清单 mtime 偏 1 秒（NAS 上 `set_mtime` 可见性窗口）时查
    baseline —— 证实就是最后上传的那版就认领，否则仍拒绝（「绝不返回错误内容」
    红线不动）；
  * **`mounts.json` 补 `cache_dir`**（存父目录，恢复时按当前 link 重拼命名空间）：
    自定义 `--cache-dir` 的挂载恢复后认领得到旧缓存 —— 旧版不存，在线重启白白
    重新水合、离线重启直接读不了已水合内容。
  * 目录改名（本地与远端）现在**整棵子树**搬清单（旧版只搬目录自身，子目录在新
    路径下「无快照」，离线 `ls` EIO）；远端删除子树时清单一并作废（防幽灵目录）。

- **离线可用 + 累计缓冲区（M20）**：断网不再等于「本地改动等 10 秒后丢失」。
  离线时挂载点继续提供服务：已缓存内容照常读写；本地改动（写文件、新建、删除、改名、
  `touch`）落进**持久化缓冲区**，联网后按用户操作顺序统一回放。分六块：
  * **连通性状态机**（`qxync-core::connectivity`）：全进程唯一判定「现在连不连得上」，
    三态（未检测 / 在线 / 离线），只有 `Error::Transport` 才算离线 —— 业务错误
    （不存在 / 无权限 / 会话失效 / 服务端忙）继续走原来的重试/放弃逻辑；
  * **离线结构操作 outbox**（`sync.db` schema v4 → v5，新增 `outbox` 表）：离线期间的
    `mkdir` / `rename` / 只改 mtime 按 `seq` 持久化，daemon 跨重启不丢；
  * **上传 / 删除队列离线感知**：网络错误**不计重试次数、不删作业、不放弃**，
    worker 停在「等联网」闸门上；daemon 恢复后统一唤醒。旧实现断网约 10 秒就会把
    作业当失败删库 —— 本地文件还在，但远端永远停在旧版（删除则会在联网后「复活」）；
  * **本地改名会带着缓冲区一起走**：离线新建的目录再改名时回写 outbox 里的路径前缀；
    删除队列也按改名 rekey（`a/x` → `b/x`），不会出现「离线删掉的文件联网后复活」；
  * **FUSE 写路径离线化**：已知离线时不再打 NAS（也就不再白等 20s connect timeout），
    `mkdir` / `rename` / `touch` 先改本地视图再进 outbox；未水合的区间立即 EIO，
    已缓存内容不受影响；新增 `user.qxync.offline` xattr，脚本可直接判断当前能不能联网；
  * **重连统一同步**：轮询器离线时每 3 秒短探活（不再等 30s/600s 的完整间隔）；
    恢复后**先按 seq 回放 outbox、再解冻上传/删除队列**，最后补一轮对账 —— 顺序反了会
    让上传抢在「改名 / 建目录」之前落到旧路径上。回放幂等（远端已是目标状态就只删行）；
    改名目标被占用时把 NAS 上那份改成冲突副本（两边数据都保留，不覆盖）；会话失效会
    自动重登后继续，网络错误 / 服务端未就绪都不消耗重试次数；
  * **状态可见**：`status` / CLI / GUI 显示离线（含已离线时长与原因）、缓冲区分类计数
    （待上传 / 待删除 / 待建目录 / 待改名 / 待改 mtime）与回放状态。

### 修复

- 断网后上传 / 删除作业被误判为「重试超限」并删库 → 离线改动永远不会同步。
- 离线时 `mkdir` / `rename` 直接 EIO（文件管理器里建文件夹、改名都失败）。
- **`qxync status` 断网时最长卡一分钟**：旧实现在登录状态下依次请求
  `nas_uid` / `max_log` / `check_alive`，每个都要等 20s connect timeout。
  现在已知离线直接跳过网络探测（真机脚本实测 6ms 返回）。
- 离线读未水合文件要白等 connect timeout 才 EIO；列过目录的 `readdir` 仍吃本地快照。
- 离线 `touch` 后，排队中的上传会把 mtime 覆盖回旧值（现在同步更新队列作业的 mtime）。
- **离线建出来的目录「能建不能用」**（真机 FUSE 验收抓到）：内核在 `mkdir` /
  `open(O_CREAT)` 之前会先对目标名字发一次 FUSE `LOOKUP`；新目录在远端还不存在、
  本地又没有清单快照 → 「无快照 + 已知离线」直接 EIO，`ls` 和往里写文件全部失败。
  修复：离线 `mkdir` 在本地视图里给新目录放一份**空清单快照**；目录改名时清单快照
  跟着搬到新路径（否则改名后的目录同样进不去）。
- **断网后挂载根目录建不了东西**（用户从没在挂载点里 `ls` 过根时）：挂载成功后主动
  **预热挂载根清单**；预热用 `seed_listing`（只在还没有快照时灌入），避免「挂载时抓的
  旧清单」迟到落地把刚新建的文件从 `ls` 里抹掉。
- **排队中途断网时 `daemon stop` / `umount` 卡两分钟**（M22）：已知离线时 M20 就跳过
  排空，但「刚断网、连通性还没判定离线」的窗口里，drain 会陪等满 120s 超时（队列在
  闸门上不可能推进）。现在上传 / 删除队列的 `drain` 在闸门（离线 / 回放未完）落下时
  **提前返回** —— 作业都在持久化库里，联网后继续；最坏耗时 = 一次在途请求的
  connect timeout。
- **离线时 `status` 把会话误报成「失活」**（M22）：离线跳过 `check_alive` 后
  `alive=false` 只是「不知道」。`SessionInfo` 新增 `alive_known`（serde 默认值，
  老 daemon 兼容），CLI / GUI 对「没探测过」显示**「未知（离线未探测）」**——
  不再把断网误导成「会话失效了去重登」。
- **GUI 诊断页的颜色标注从未生效**（M22，顺手抓的既有 bug）：`kvText` 把调用方传的
  第 4 参（`val-dim` / `val-ok` / `val-warn` / `val-err`）静默丢弃，约 8 处调用的
  颜色分类一直没渲染出来。

### 验证

```text
cargo build --workspace                         ✅
cargo test --workspace                          ✅ 476 passed / 0 failed
  qxync-core   244（含 connectivity / outbox / listings 持久化 / alive_known 兼容）
  qxync-daemon  68（含回放计划 / 网络错误不丢行 / 不计次数）
  qxync-fuse   107（含队列离线闸门 / drain 闸门提前返回 / 离线重启装回视图 /
                  认领 mtime 偏斜容差 / 真机验收后补的离线 FUSE 回归）
  qxync-client 32 / cli 7 / gui 15 / proto-test 3
cargo fmt --all --check                         ✅
node xtask/tests/task-defaults.test.mjs         ✅ 24 passed
xtask/tests/m20-offline.sh                      ✅ 9 passed（隔离 XDG，NAS 指向 127.0.0.1:9）
xtask/tests/m21-offline-restart.sh              ✅ 19 passed（假 NAS + 真 FUSE）
  · 离线时 status 6ms 返回（< 5000ms 断言）
  · 缓冲区分类计数正确（待建目录 1 / 待改名 1）
  · 跨 daemon 重启行不丢、attempts 仍为 0
  · 离线重启后挂载点恢复、ls 吃持久化清单、二次重启幂等
```

**真机 FUSE 验收（2026-10-19 补做）**：

```text
xtask/tests/m20-real-nas.sh                     ✅ 57 passed（真 NAS + 真挂载 + TCP 中继断网）
  · 在线写 → 断网 → 离线 mkdir / 建二级目录 / 写文件 / rename 文件与目录 / rm / touch
  · 离线 ls 新建目录、读缓存、读未水合（2ms EIO）；status 缓冲区计数、outbox attempts=0
  · daemon 离线重启：outbox 4 行不丢、attempts 仍 0
  · 恢复网络 3s 内：远端出现改名后的目录/文件与内容、旧名消失、删除生效、mtime 生效、清零
xtask/tests/m21-real-nas.sh                     ✅ 43 passed（真 NAS + 真挂载 + 断网）
  · 离线重启 daemon：挂载点 1 秒恢复（无 NAS / 无会话）
  · ls 根/子目录/离线建的目录（清单来自 sync.db）；cat 水合认领 / 偏斜容差 / 自定义 cache-dir
  · 第二次离线重启幂等；恢复网络后回放 + 上传全对、缓冲区清零
```

验收中抓到并修复的真机问题见上面「修复」与 M20/M21 里程碑文档；仍存在的限制见下方「已知限制」。

### 已知限制

- 离线新建/改名的文件在同步落地前不会出现在「重新打开挂载点」的目录列表里
  （本地节点表是进程内状态）；但内容与操作都在持久化队列/outbox 里，不会丢。
  同一进程内它们是立即可见、可继续写入的（空清单快照 + 目录清单随改名搬迁）。
  ★ M21 起已列过的目录、本地建的目录跨重启也可见（清单持久化）；只有「从没列过
  也没本地建过」的目录要联网后由轮询补上。
- ~~离线重启 daemon 时，挂载点要等网络恢复后才恢复~~ —— **M21 已修**（无会话挂载 +
  持久化清单，离线重启 1 秒恢复）。
- 回放改名的冲突处理固定走 `rename_remote` 语义（NAS 那份改名成冲突副本），
  暂不读取任务的 `conflict` 策略（离线时无法保证策略在回放时仍有效）。
- 任务级 purge 暂不清理 outbox / listings 行（与上传/删除队列一样按连接共享；
  连接级 purge 会整体删掉 `sync.db`）。

### 修复（M23 真机验证暴露）

- **Baloo 配置读取不认 `XDG_CONFIG_HOME`**：balooctl 写配置走 KConfig 语义
  （`$XDG_CONFIG_HOME/baloofilerc` 优先），而读侧写死了 `$HOME/.config` ——
  设了 `XDG_CONFIG_HOME` 的环境里门禁永远看不到刚写入的排除项。真实用户
  环境两者恰好一致所以此前未炸。现在读侧对齐 KConfig 语义（fuse-matrix
  真机 9/9 全绿）；
- **不认 balooctl 实写的键格式**：balooctl 写的是 `exclude folders[$e]=$HOME/…/`
  （KConfig 扩展标记 + `$HOME` 变量形式 + 尾斜杠），精确匹配 `exclude folders`
  一个条目都读不到。现在键匹配前剥 `[...]` 后缀（`$HOME` 展开与尾斜杠的组件
  比较原本就有）。两个修复各带单测（baloo 12 例）；
- **fuse-matrix 两处测试缺陷**（假失败的来源）：M23 段开跑不重置 `dolphin_tags`
  → 上轮残留 true 时门禁断言被「只在关→开边沿拦」的产品语义静默吞掉；缓存
  清理 `rm -f` 删不掉 host 子目录（M9 布局）→ 残留缓存让三个断言假失败。
  基线对照（M23 之前的代码）确认其余失败为 WAN 环境固有，与本版本无关。

## [0.6.8] - 2026-10-19

### 重设计

- **对账扫描改为时间预算驱动**：一轮能列多少目录由「这轮有多少时间」决定，不再写死
  200 个。动机（真机 2026-10-19）：1439 个目录 × 600s 轮询间隔，全量列举只要
  1~5 分钟，旧实现却按目录数硬掰成 8 轮轮转 —— 远端删除最坏要 **80 分钟**才落地；
  而该账号事件快路径是死的（`qbox_get_sync_log` 无新事件、从未登记同步文件夹），
  baseline 对账就是变更发现的主路径。现在：
  * 预算 = 轮询间隔 × 50%（每轮推导，间隔运行时可调）；`QXNYC_SCAN_BUDGET_SECS`
    显式覆盖（`0` = 不限）；`sync --once` / GUI「立即同步」给足 600s —— 手动场景
    就是要全量；
  * 预算没用完 → 一轮**全量**（删除判定延迟 = 一个间隔）；预算耗尽 → 到点即收、
    游标续扫（退化为轮转，每轮耗时可控）；
  * **进度保证**：每轮至少列一个轮转目录，杜绝「预算小于一次列举耗时」时根吃光
    预算、游标存不下的新型饿死；
  * 游标存在即进轮转（不再要求目录数超硬顶）；列完全部时**清游标**；
    回绕补齐不超过全集（预算 ≥ 全集时不再重复列目录）；
  * `max_dirs_per_poll` 降级为硬顶兜底，默认 200 → 2000；
  * note 口径升级：预算耗尽会写明「本轮列了 X/Y 个目录，下轮从游标续扫」。
  覆盖：`generous_time_budget_scans_everything_in_one_round`、
  `tiny_time_budget_still_makes_progress`（1ns 预算下逐轮推进）。

## [0.6.7] - 2026-10-19

### 修复

- **`qxync daemon stop` 在挂载点被占用时「看起来停不下来」**：停止前要先排空上传 / 删除
  队列再卸载挂载点（队列卡住时各等 120 秒），而旧 CLI 只看 socket 连不连得上、10 秒就
  打印「已停止」，进程其实还在；`fusermount -u` 撞上「有进程在用」时又只回一句
  `Device or resource busy`，用户不知道是谁占着。真机取证（2026-10-19 12:48:40 →
  12:50:40）：挂载点里还压着一个未上传作业（`HTML 文件.html`），`daemon stop` 卡了
  整整 120 秒才退出，CLI 却早已报成功。现在：
  * 停止前扫 `/proc`（进程 cwd / root / 打开的文件 / mmap，以及挂载点下的子挂载），
    有进程占用就**列出 pid / 程序 / 路径并拒绝停止**，关掉这些程序后再停；
    `qxync daemon stop --force` 可强制停止（挂载点里打开着的文件会立刻失效）；
  * 等**进程真正退出**（不再只等 socket），退出前排空队列时明确说明「在等什么」；
  * `qxync umount` 走同一套判据：占用中直接返回「谁在用」，不再只回
    `Device or resource busy`；daemon 侧卸载失败时也会把占用者写进日志。

## [0.6.6] - 2026-10-19

### 修复

- **事件快路径在「视图根是家目录子目录」时整体失效（每条事件都 `status=5`）**：
  任务把 NAS 文件夹选成家目录里的子目录（真机 `root=/home/.Qsync`，对应 NAS 的
  `~/.Qsync`）时，`map_event_path` 把「剥掉 `/share/homes/<user>` 后剩下的
  `/.Qsync/…`」又拼到了视图根 `/home/.Qsync` 后面 → `/home/.Qsync/.Qsync/…`
  （`.Qsync` 两遍）。于是**每一条事件**的 `stat` 都回 `status=5 (路径不存在 / 无权限)`：
  事件快路径完全失效（全靠 baseline 对账兜底，远端变更要等一轮轮询）、
  `最近错误` 被同一类噪音刷满。真机取证：`journal` 里 2026-10-18 17:31 那一轮**拉到
  87 580 条事件、跳过 8 017 条**；当天 daemon 日志里 `事件 stat` 共 **79 670 条，
  100% 是 `/home/.Qsync/.Qsync/…` 双写路径**（79 667 条 `status=5`，3 条网络错误）。
  现在事件路径先经 `client_path_from_share` 换算到
  客户端命名空间（前缀恒为 `/home`），再按视图根收窄 —— 不属于本视图的事件返回
  `None`（交对账兜底），不再拼出一个不存在的路径。顺带：事件 `stat` 的 4/5/6 不再
  记成硬错误刷屏（那三个码同时是「权限拒绝 / 会话失效」，本来就不该当错误）。
- **凭空多出冲突副本：一轮进行中自己上传成功，本轮判定却拿开轮快照当 base**：
  `poll_once` 在开轮时克隆 baseline 快照、整轮（几十秒）都用它判定；而上传成功 hook
  会在本轮**进行中**就把权威 baseline 推进到刚落上去的签名、并 `clear_dirty`。
  于是判某个刚保存过的文件时：快照里还是旧签名、远端已经是自己刚传的新版、本地已
  标干净 → `decide` 走进 `(dirty=false, remote≠base)` 分支，又因本地签名 ≠ 旧 base
  被判 **冲突**，再按当时的冲突策略生成了一个与原文件**字节完全相同**的副本
  （真机 2026-10-19 08:06：`防火墙配置 (conflicted copy from <本机名> 2026-10-19).txt`，
  12218 = 12218 字节）。现在开轮时同时清空一张「本轮 hook 推进」表，判定时
  （对账 / 事件 / 冲突重判）都用「快照 ∪ 本轮 hook 推进」当 base；同一轮内多次上传按
  **列表**匹配（目录列举可能落在两次上传之间）；若本轮列举早于上传落地（远端还是
  快照里的旧签名），则仍以快照为准，避免把本地元数据刷回旧版。复审又补了两处：
  ① `persist` 在同一轮内发生多次（事件阶段每批一次 + 轮末一次）时，第一次合并后
  不再清 `hook_touched`，否则第二次整份替换会把 hook 刚推进的签名在盘上抹掉 ——
  同一个凭空冲突**晚一轮重现**；② 「远端比本轮最后一次推进还旧」的捷径只在
  **本地干净**时生效，本地带未上传改动时退回正常判定，否则别处设备「mtime 偏小的
  新写入」（时钟慢 / `rsync -t` / 备份恢复）会被本地直接覆盖 —— 丢数据。
  覆盖：复刻真机时序的回归单测（旧逻辑必现 Conflict、新逻辑 Noop）、同轮多次
  persist 的落盘回归、穿透 `reconcile_view` 的端到端回归、开轮生命周期单测。
- **任务页改的冲突策略对已挂载视图不生效、重启后又被 `mounts.json` 盖回旧值**：
  冲突策略由同步引擎按 `MountEntry.conflict` 分派（视图不读任务文件），而
  `mounts.json` 的语义是「上次实际挂了什么」、重启时现场优先。真机上因此出现过
  任务写 `ask`、实际一直按 `rename_local` 走 —— 本该进「待裁决队列」的冲突直接生成了
  冲突副本。现在：① 保存任务时把新策略同步到**已挂载**的同路径视图并落进记录；
  ② 启动恢复时任务表里有同名任务就以任务里的冲突策略为准（其余挂载参数仍现场优先）。
  另外事件跳过现在按原因计数并写进 `remote_change` journal 详情（删除事件无路径 /
  本机回声 / 非本账号 / 被规则隐藏 / 远端不可见 / 映射失败），免得「事件快路径整体
  失效」再次变成无声故障。

## [0.6.5] - 2026-10-18

### 新增

- **Debian / Ubuntu 安装包**：Release 新增 `qxync_<版本>-1_amd64.deb`，**一个包同时适用**
  Debian 12 / 13 与 Ubuntu 22.04 / 24.04 / 26.04（amd64）。三个二进制与本 Release 的
  tar.gz **逐字节同源**（从 tar.gz 重打包、不重新编译）；`Depends` 由 `dpkg-shlibdeps`
  对三个二进制实测后得出、再手工补 `fuse3`（共 11 项，全是官方仓库里的标准包）。
  发布流水线在干净容器矩阵里把 Debian 12 / 13、Ubuntu 22.04 / 24.04 / 26.04
  逐个 `apt install` 实装、跑通断言才发布。
- **打包脚本**：`xtask/release/package-deb.sh`（在 `ubuntu:22.04` / `debian:12` 基线容器里
  从 Release tar.gz 打 `.deb`）、`xtask/release/apt-mirror.sh`（容器内 apt 源切国内镜像，
  默认清华）。维护者说明见 `packaging/debian/README.md`。
- **`docs/协议要点.md`**：把原本堆在 README 里的协议级「踩过的坑」集中成开发文档，
  条目编号保持不变，代码注释里的引用仍然对得上。

### 变化

- **README 重写为面向用户的说明**（中文 / English 同步）：是什么、功能、截图、安装
  （Arch、Debian/Ubuntu、tar.gz、源码构建）、登录、常驻 daemon、多 NAS、挂载与脱水、
  常用命令、升级、已知限制、安全隐私、贡献。维护者向的架构说明、实现要点、验收矩阵等
  从 README 移到 `docs/` 与 `CONTRIBUTING.md`。
- **发版流水线**：Release 资产 6 → 7 个（新增通用 `.deb`）；Arch 包校验之后新增
  「打 `.deb`」与「五发行版干净容器实装校验」两步，`.deb` 同时进 `SHA256SUMS`。

### 说明

- **本版没有同步逻辑变更**：`git diff v0.6.4..v0.6.5 -- crates/` 为空，0.6.4 → 0.6.5
  不涉及数据格式、配置项或 CLI 参数变化。已在 0.6.4 上的用户升级是可选的。

## [0.6.4] - 2026-10-18

### 修复

- **目录数超过单轮上限时，删除判定被永久推迟**：真机反馈「目录数 228 超过单轮上限 200…
  本轮跳过删除判定」每轮重复出现（连续 87 轮）。根因是对账每轮都从排序开头只列前 200 个
  目录，排序在后的目录**永远**轮不到；而「远端视图不完整就不按缺失删本地」这条安全阀
  因此永久生效 —— 那些目录里的远端删除、上传、刷新判定永远不会执行（数据安全没有被破坏，
  但同步的「删」这一半等于停摆）。现在改为**轮转扫描 + 按目录判定**：
  - 目录数超上限时每轮从上一轮停下的位置接着列、到尾回绕（远端根恒定每轮都列）；
    轮转游标落在 `sync.db` 的 `meta` 表，`qxync sync --once` 跨进程也接得上；
  - 「视图不完整」从整视图收窄到**按目录**：只有本轮真正列过（列举成功，或 4/5/6 经
    `stat` 确认 `exist=0`）的目录里的候选才允许判定，没列到的目录里的条目一律暂不判定
    （不按缺失删本地），同一视图里其它已列全的目录**不陪绑**；
  - 改名配对也只用「父目录本轮列过」的已删侧候选 —— 不把「没看到」配成一次改名；
  - 覆盖：纯函数轮转窗口单测 + 无真机 mock NAS 的 `reconcile_view` 两轮回归
    （228 个目录，d000 / d210 各有一条远端删除：第 1 轮只删窗口内、第 2 轮轮转到
    排序在后的目录，未扫目录零误删，两轮窗口覆盖全部目录），以及「单个目录失败
    只推迟它自己」单测。
- **`status 4/5` 不再直接当成「远端已删」**：4/5 同时是权限拒绝 / 会话失效的码
  （真机 `/Download` 回 5、`/homes` 回 4，但路径其实存在）。旧实现把 4/5 当「确定
  不存在」，权限 / ACL 变更或 sid 中途失效时会把该目录所有候选判 `DeleteLocal`
  （本地节点 + 缓存 + baseline 清掉）且不重登。现在列举返回 4/5/6 时**先用 `stat`
  正向确认**：只有 `exist=0` 才允许按「远端已删」处理；stat 说仍存在 → 目录条目记进
  远端视图（防止「父目录列举漏掉它」时目录自身被判删、连整棵子树 + 缓存一起清），
  其内容本轮不判定；stat 也失败 → 目录存在性未知，目录自身与内容都不判定。stat 成功
  本身证明会话有效，所以**不**置 `auth_failed`（否则永久 ACL 隐藏的目录会每轮重登 +
  刷 WARN）；只有 stat 也 auth 失败或远端根 4/5 才触发重登。远端根例外：挂载根不可能
  凭空消失，4/5/6 一律按权限 / 会话 / 卸载处理（不 stat、不删）。新增 mock NAS 回归
  `listing_status_4_or_5_never_deletes_without_stat_confirmation`、
  `listing_root_status_4_or_5_defers_instead_of_deleting`、
  `directory_missing_from_parent_listing_but_stat_exists_is_not_deleted`、
  `stat_failure_keeps_directory_and_subtree`。
- **HTTP 层失败不再被读成「空目录 / 不存在」**：`get_list` / `stat` 先检查 HTTP 状态码
  —— 反代 502/504 的 JSON 错误体（`{"error":…}`）以前能通过 `Listing` 的全默认反序列化
  被当成「0 项」，对账据此删本地；即使 HTTP 200，非列表形状的 JSON（缺 `status`/`datas`/
  `total`/`real_total`/`exist`）现在也按解析失败处理。`list_limit` 也不再拿缺省的
  `total=0` 在第一页就 break（服务端不给 `total` 时 >200 项的目录会被静默截断）；
  无可信 `total` 时按分页自然收敛。

## [0.6.3] - 2026-10-18

### 新增

- **删除相关数据（M19）**：以前「删除任务登记」「移除连接」都刻意不碰数据 —— 水合缓存、
  `sync/<ns>/sync.db`（baseline / 日志 / 待裁决）、`upload-queue/<ns>/`、日志会永远留在本机，
  界面上没有任何出口。现在三个作用域都能把相关数据一起删：
  - **任务**：登记 + 该任务水合缓存 + `baseline` / `journal` / `decisions` 里落在该 NAS
    文件夹子树的**行**（先卸载；按连接共享的队列 / pins / nodes 不动）；
  - **连接**：任务登记 + link / 凭据 / peers + `sync/<ns>` + `cache/<ns>` +
    `upload-queue/<ns>` + 挂载记录 + 该连接日志 + socket / pid；
  - **本机全部**：配置 / 数据 / 状态三个根（恢复出厂），会先停掉所有 daemon。
  - **任何作用域都不删 NAS 数据、不删挂载点里的文件**；共用缓存 / 共用 NAS 文件夹时不猜，
    跳过并警告；符号链接只删链接本身。
  - GUI：任务卡「删除…」、连接行「移除」、设置 → 高级 → **危险区**（必须勾选才可确认）；
    三处共用一个确认弹窗，先展示「会删什么 / 多大 / 多少状态库行」，复选框默认不勾选。
  - CLI：`qxync purge --task <id>` / `--connection` / `--all`，**默认只预览**，
    `--yes` 才执行，`--json` 可脚本化。
  - 新增 `qxync-gui --self-test-purge`、`xtask/tests/m19-purge.sh`（隔离 XDG 的 e2e，
    CI 已接入）与 core 单测；`link_delete` 新增可选 `purgeData` 参数，不传时行为与旧版一致。
  - 设计取舍与边界见 [M19-删除相关数据.md](docs/M19-删除相关数据.md)。

- **新建任务的默认路径按「连接 + id」派生（M18）**：以前新建任务时，任务 id / 本地文件夹 /
  水合缓存三处默认值对所有任务都一样（`default` / `~/qxync-mnt` / 按连接共享的缓存根），
  第二条任务一提交必然撞 id 或撞挂载点，只能手工改三格。现在：
  - 任务 id 取 NAS 文件夹名（`/home` → `home`，与已有任务重名自动 `-2`）；
  - 本地文件夹 = `~/qxync/<连接 id>/<任务 id>`；
  - 水合缓存 = `<data_dir>/cache/tasks/<连接 id>/<任务 id>`（**父目录**；实际落盘还会
    再拼一层 M16 的连接命名空间）；
  - 表单先选 NAS 文件夹；换文件夹 / 换连接会跟着重算，**手改过的字段不再被自动覆盖**
    （点「重置为默认路径」可恢复）；编辑已有任务不做派生；
  - 提交前拦下「id 为空 / 非法 / 已被别的任务占用」，不再静默覆盖另一条任务；
  - 已保存任务的路径**一个都不动**，任务文件格式**没有新增字段**（回退旧版本无兼容问题）。
  - 设计取舍与验证见 [M18-任务默认路径.md](docs/M18-任务默认路径.md)。

### 修复

- **新建任务用同一个 id 会静默覆盖另一条任务**：`tasks/<id>.json` 是全局唯一文件名，
  而 GUI 新建表单的默认 id 固定是 `default` —— 同连接下建第二条任务会把第一条的登记
  悄悄换掉，挂载表里却还挂着旧任务的根（记录与挂载对不上，界面上看不出来）。
  现在 GUI 新建任务在提交前查重并拒绝（编辑任务不受影响）。

## [0.6.2] - 2026-10-18

### 新增

- **两步验证（2SV / TOTP）支持（M17）**：账号在 QTS 里开了两步验证后，qxync 以前**永远登不上**
  —— `authLogin.cgi` 的第一份表单就决定给不给 sid，而我们只发 `user + pwd(base64)`，服务端回
  `authPassed=0 / need_2sv=1`，后续所有端点都拿不到 sid。真机实测（2026-10-18）的结论是：
  **同一份表单多带 `security_code=<6 位 TOTP>` 就能拿到 `authPassed=1` + `authSid`**，
  其余字段一个不动。实现照 QTS Web UI 走两步：**第一次请求与 v0.6.1 逐字节相同**，
  只有 NAS 回 `need_2sv=1` 时才发带 `security_code` 的第二次（所以「未开 2SV 行为一字不改」
  是结构性的，不靠服务端忽略未知字段）。实现：
  - 纯 Rust 内置 TOTP（RFC 6238：SHA-1 / 30 秒 / 6 位；Base32 密钥；直接粘贴 `otpauth://`
    URI 也认），**不新增依赖**；
  - 凭据新增可选 `otp_secret`（与口令同文件、`0600`、按连接一份；旧文件**零迁移**，
    缺字段 = 未配置 2SV）；
  - CLI 全局 `--otp-secret`（或 `QXNYC_OTP_SECRET`，会随凭据保存）与 `--otp-code`
    （或 `QXNYC_OTP_CODE`，一次性、不保存）；
  - daemon **每次重登录现算验证码**（只存密钥、不缓存验证码）——长时间挂载不会因为验证码
    过期掉线；`Request::Login` 支持一次性 `otp`；
  - GUI「设置 → 连接」新增「两步验证（可选）」字段（填密钥 = 保存，填 6 位码 = 一次性），
    编辑连接时显示「已配置」，登录结果显示两步验证是否通过，**密钥不回显**；
  - 设计与协议证据见 [M17-两步验证.md](docs/M17-两步验证.md)。

### 修复

- **2SV 账号的登录报错不再误导**：以前 `authPassed=0` 一律附加「检查口令编码：必须是 base64」，
  账号开了两步验证时这句话是错的。现在区分两种分支：「账号要 2SV 但没提供第二因素」→
  提示用 `--otp-code` / `--otp-secret` 或 GUI 里的位置；「提供了但被拒」→ 提示验证码可能
  已过期（30 秒一换）或本机时钟偏移。提示与日志都**不含**密钥 / 验证码。

### 变更

- **未开 2SV 的账号行为一字不改**：登录请求体不带 `security_code`，凭据文件不新增字段。
- 旧凭据文件直接可读（无 `otp_secret` = 未配置）；新凭据给**旧版本**读只多一个被忽略的字段。
- `IPC_VERSION` **不 bump**：`Request::Login.otp` 与 `LoginData.two_step` 都是 `serde(default)`，
  新旧进程可混跑（与 M16 同一口径）。
- 回退 v0.6.1：2SV 账号重新登不上，其余功能不受影响。

### 已知问题（本轮未修）

- **只支持 TOTP** 一种第二因素；QNAP 还有的安全问题 / 邮件验证码 / 二维码批准 / 设备批准
  **均未实现**。
- 依赖本机时钟；`otp_secret` 与口令一样明文存 `0600` 文件（keyring 是后续工作）。
- 协议侧未测：`dont_verify_2sv_again=1`（记住设备）、**连续**输错验证码的锁定 / 限流行为
  （只测了单次错码：被拒、未锁定）。

## [0.6.1] - 2026-10-18

### 新增

- **「添加连接」不再只有一个入口**：顶栏「当前连接」右边的「＋」、主页连接卡片的
  「＋ 添加连接」、任务表单「NAS 连接」下拉的「＋ 新建连接…」——三处都直达
  「设置 → 连接」的新建表单（切到该分区、把表单滚进视野、焦点落在「主机 / NAS 地址」，
  并留一行日志）。之前只有「设置 → 连接」卡片右上角那一个「＋ 添加 NAS」；主页、
  任务页都没有出口，而任务表单的下拉只列**已有**连接 —— 想连一台还没配的 NAS 时，
  界面上根本没有地方可以点。见 [M16 §7.1](docs/M16-多NAS连接.md)。

### 修复

- **改名/删除不再「先排空整个上传队列」**（编辑器保存大文件/文本从 1~2 秒回到几十毫秒）。
  之前 FUSE `rename`/`unlink` 只要目标有本地改动就 `drain()` 等**整个**上传队列清空
  （最多 60s / 30s）再打服务端，编辑器（KIO/Kate）的「原子保存 = 写 `.part` → rename」
  正好撞上：等队列把 `.part`、swap 文件、同目录其它上传全部传完才返回（实测一次保存
  1.5~1.9s）。现在：
  - 只冻结并等待**相关路径**上的作业；排队中的作业直接改写到新名字
    （`UploadQueue::retarget_pending_under`，状态库主键同步 rekey），不再先传一版老名字；
  - 节点新增 `remote_known`（远端到底有没有这个副本）：本地新建后立刻改名/删除
    （KIO 的 `.part`、临时文件）**不打服务端**，纯本地处理，上传直接落到最终名字；
    远端有老副本时行为不变（先远端改名、再更新本地视图，失败回 EIO 且队列原样保留）；
  - 上传队列新增 250ms「入队去抖」：同一路径的连续写合并成一版上传，
    编辑器的「写临时文件 → 改名」不会再把 0 字节 / 中间版本推上 NAS；
  - 删除一个从没上传过的文件（关文档时删 swap）同样不再「先上传再删除」。
  - 回归：新增 6 条单测（路径级等待/改写/去抖/暂停恢复/`remote_known` 生命周期），
    `cargo test --workspace` 全过。

- **新建连接的表单不再被「当前连接」的异步回包填回**：点「＋ 添加 NAS」清空表单后，
  `loadConnect()` 读回的是**当前连接**，于是「添加 NAS 连接」标题下挂着上一台 NAS 的
  host / path，保存下去就是一条错的连接。现在用 `creatingLink` 区分「展示当前连接」
  与「正在新建」，新建态下这次回包不碰表单。
- **主页「连接设置」不再落到上次看过的设置分区**：上次停在「关于」时点它会落到关于页，
  连接列表根本不出现；现在固定落在「连接」。

## [0.6.0] - 2026-10-18

### 新增

- **多 NAS 连接（M16）**：一条连接一个 `qxyncd` 进程，可以同时连多台 NAS，
  每条连接下再建多个任务（本地文件夹 ⇄ NAS 文件夹）。要点：
  - 每个连接**独立** socket / pid / 日志 / 状态库 / 缓存 / 上传队列；**停掉一条连接
    不影响其它连接**；`default` 连接保持旧路径、**升级零迁移**；
  - 数据按连接命名空间隔离：`default` 用 `<host>`（旧目录名），其它连接用
    `#<id>@<host>`（`sync/`、`cache/`、`upload-queue/`、`mounts/` 同一个规则）；
    非 default 凭据在 `links/<id>.credentials.json`；
  - 任务新增可选 `link_id`；旧任务归属**确定**（`default` 优先，否则字典序最小的连接）；
    同 id 任务跨连接**拒绝覆盖**；本地挂载点冲突**跨连接直接拒**，NAS 根重叠只同连接告警；
  - 旧数据迁移：`default` 只迁上传队列；非 default 的 legacy owner 迁
    sync / cache / queue / mounts / credentials（sync / queue 失败**拒绝启动**，
    cache 失败降级只告警）；
  - CLI：`qxync links [--json]`（所有连接 + 各自 daemon / 凭据状态）、全局 `--link`
    路由、`qxync daemon start|stop|status|reload --all`；
  - GUI：连接列表（新增 / 编辑 / 移除）、「当前连接」选择器、任务聚合、文件页跟随
    当前连接、设置广播到所有连接、托盘「启动全部 / 停止全部」；
  - 移除连接**只删登记**（link / 凭据 / peers / 任务登记），**不删**本地文件、NAS 数据、
    cache 与状态库；同一 `(host, port, user)` 拒绝重复添加；
  - 新增 `qxync-gui --self-test-multi` 自检（两个假连接、两个独立 socket、任务隔离、
    跨连接覆盖被拒、收干净全部退出码 0），文档见
    [M16-多NAS连接.md](docs/M16-多NAS连接.md)。
- **冲突待裁决队列现在「能看清、能处置」**：「每个文件都问我」策略攒下的队列，以前只有
  三个裁决按钮（保留本地 / 保留 NAS / 两份都留），用户**看不到差在哪**、**也没有
  「这条我不想管」的出口**。本批完成：
  - GUI / CLI 新增「**对比本地/NAS**」：行级文本 diff（`-` = 只有本地有、`+` = 只有 NAS 有），
    超 512 KiB / 二进制 / 目录 / 一边内容读不到时**明确显示原因**，绝不静默显示「无差异」；
  - GUI / CLI 新增「**忽略这条**」：只删队列行、不动任何文件；
  - GUI 新增「**清空全部**」，CLI 补 `--clear`；
  - 引擎新增**队列巡检**：远端已删 / 任务已删 / 策略不再是 `ask` / 冲突自行消解的条目
    自动出队（只删队列行、不动文件），并说明原因。
- **同步引擎不再管理回收站子树**：`@Recycle`、`.Trash`、`.Trash-<uid>` 从对账、baseline、
  改名配对、事件处理、待裁决队列、上传队列里全部排除（判定收进
  `qxync-core::trash`，与 FUSE 既有的 `deny_trash` 同一套段级语义）。真机取证：
  baseline 1991 条里 **1425 条在 `/home/@Recycle` 下**、13 条在 `.Trash-1000` 下
  （FDO trashinfo 还曾被反向同步到 NAS）——回收站被 QTS 清理时会产生成批「幽灵删除」。

### 修复

- **GUI 提示「有删除被熔断挡住」，却没地方处理**（用户反馈）。这是一条**只有出口提示、
  没有出口本身**的状态：任务卡状态行写着「有删除被熔断挡住（待确认）」，主页红框补了句
  「到「诊断 → 同步 / 缓存」可强制放行一轮」—— 但那里唯一的按钮叫「**强制放行删除**」，
  和提示里的「待确认」毫无对应关系，看到的人根本不知道该点哪个，只能全局搜。
  1. `addAlert()` 增加可选的 `action` 参数：告警行右侧可以直接挂一个按钮。
     **凡是「看到了还得做点什么」的状态，出口就挂在提示自己身上**，不再让用户去找。
  2. 三处提示（主页「最近一次同步」、诊断→状态、诊断→同步/缓存）都内嵌了「放行删除」。
  3. 任务卡的操作区在熔断时多出一个「放行删除」——只在状态行**真的**在讲熔断时出现；
     卡片自己的「已暂停 / 未挂载」盖掉全局状态时按钮同步让位，避免「状态说已暂停、
     却挂着个放行删除」的矛盾画面（而它确实会真删东西）。
  4. 文案去掉含糊的「待确认」，改成如实说明「**这轮没有真的删除任何文件**」——
     熔断挡下的是**打算执行的删除**，用户最怕的「文件已经被删了」从来不会发生，
     这一点以前从没在界面上说过。
  5. 三个入口的放行动作收敛到同一个 `doForceDeletes()`：二次确认，且**确认框里摆出
     daemon 给的原始原因**（「一次对账要删除 812 项，超过上限 50」比「确认放行？」
     有信息量得多），并提示「如果是误清空就别放行」。删除不可逆，这一步不能省。

#### GUI

- **冲突对比面板恒显「本地多 +0 / -0 / 两边内容逐行相同」**（危险假象）：`DiffOutcome`
  是 serde newtype 变体，wire 上字段与 `status` **平级**，前端却多读了一层 `o.diff`，
  而兜底 `{}` 不报错 → 没比过和比出零差异长得一模一样。改为读平级 + 校验每行形状，
  形状不对显示错误说明（**绝不落成「逐行相同」**）。
- **diff 语义方向反了**（GUI 图例 / CLI 输出）：真实 `text_diff` 里 `-` = 只有本地有
  （`removed`）、`+` = 只有 NAS 有（`added`），UI 与 CLI 都按反方向标注。方向看反会
  直接导致选错「保留哪边」且不可撤销，已把 core 注释、契约夹具、GUI 图例、CLI 输出
  四处统一（并用生产函数写测试钉死）。
- **冲突卡片「发现于」永远显示当前时刻**：`clock()` 把数字时间戳当成非对象丢弃、
  退化成 `new Date()`。同类问题还影响「上次触发 / LAN 事件时间」。
- **动态按钮的「忙 / 禁用」完全没生效**：`withBusy` 用 `document.getElementById(btn.id)`
  找按钮，而卡片按钮是动态建的无 id → 连点会重复发裁决/忽略/对比请求。改为同时支持
  元素引用；diff 展开还加了飞行中禁用与去重，避免同一张卡挂出两个对比面板。
- **诊断页把累计值当「当前」**：`deletes_blocked` 是累计计数（真机 69 万），
  标签改为「删除被挡（累计）」且不再标红；「当前是否熔断」只看 `delete_block_reason`。
- **连接页保存会静默抹掉选择性同步 / LAN 对等配置**：`link_save` 用 `LinkInfo::link()`
  直接落盘，前端只提交 host/port/https/insecure/user/ipv4；`exclude` / `filter_temp` /
  `peer_listen` / `peer_name` 被默认值覆盖。改走 `link_with_existing()`（`login_flow`
  本来就这么做）。
- **`login_flow` 重启路径忽略 `already`**：另一个进程抢先拉起 daemon 时
  （`{started:false, already:true}`）被判成「新 daemon 没能起来」并**跳过登录**。
- **`LinkInfo` 缺 `insecure` 字段**：GUI 感知不到「跳过 TLS 校验」被改过，
  重存连接后 daemon 仍用旧配置。补字段并纳入 `same_link` 比较。

#### 同步引擎 / daemon

- **同一批 526 项删除被熔断挡住，每 30s 刷一条 WARN + 一条 journal，连续 13 小时**
  （真机 journal 3046 条里 3023 条是同一句话，1 万条上限几天就被冲掉）：改为按
  「数量 + 原因」签名去重，签名不变降为 debug；`ask` 策略的冲突也只在**首次入队**时
  计数/记日志（以前每轮都写「生成 N 个冲突副本」，实际一个副本都没有）；「引擎活着」
  的 scan 条目 600s 限频。
- **「最近错误 / 熔断告警」只写不清，问题恢复后界面永远红着**：真机上 08:30 就恢复
  的 `status=3` 错误下午还显示在首页。统一到 `absorb_round()`：**完整跑完**的轮次
  零错误才清 `last_error`，`deletes_blocked==0` 才清熔断告警；提前返回的轮次
  （会话失效 / 服务端未就绪）只更新当前错误、不清历史告警。脱水路径同款修复。
- **会话过期后要等 120s 保活才自愈**：`poll_once` 把 `qbox_get_max_log` 失败塞进
  `errors` 后仍返回 `Ok`，轮询循环的 `is_auth` 重登分支永远不触发。新增
  `SyncReport::auth_failed`（`status=3` 只在 max_log 端点识别，附真机证据）→
  轮询循环立即重登一次。
- **待裁决队列在「没有挂载视图」时被整队静默删光**（真机风险）：daemon 刚启动 /
  用户卸载 / 恢复失败时 `views` 为空，旧巡检验把每行都当成「任务已删 + 远端已不存在」
  → 用户裁决意图直接丢失。新增 `DecisionFacts::observed` 禁止用占位签名判定，并用
  磁盘任务配置区分「任务被删」与「只是没挂载」；配置读不全时保守保留。
  ★ 复核又补上一半：`observed` 只保证**远端** stat 到了；daemon 重启后本地节点表还是空
  的（`local_sig` 返回占位 `MISSING`），「已裁决待执行」的行会被 rule 4 误判「冲突已
  自行消解」清掉。新增 `DecisionFacts::local_observed`（本地节点真的存在才为真），
  rule 4 只在这两个观察都成立时才敢判「消解」。
- **对账列举失败被当成「远端已删」→ 可能真删本地节点/缓存**（数据安全）：
  网络超时/权限错误时 `remote_map` 不完整，旧逻辑据此判 `DeleteLocal`。现在本轮只要有
  非 404 列举失败就**暂缓该视图的删除判定**并说明原因，下一轮列全再删。
  ★ 同类漏网也补上：目录数超过 `max_dirs_per_poll`（默认 200）时只扫前 200 个，
  第 201 个之后的 baseline 条目同样会被判「远端已删」——**没有报错、没有失败计数，最
  容易漏判**，现在「目录被截断」与「列举失败」一样算远端视图不完整、同样暂缓删除判定。
- **baseline 的删除与改名永远落不了盘**（pre-existing，回收站清理「下一轮又回来」的
  根因）：`persist()` 只做「取较新签名」的增量合并，**删除方向被内存里旧快照复活**。
  改为「本轮无并发 hook 写 → 整份替换（删除/改名真正落盘）；有则逐条合并」。
  该 bug 同样影响规则排除清理、已执行删除的 baseline 与改名迁移。
  「整份替换」要求同一时刻只有一轮在跑（HEAD 的纯合并写对并发天然免疫），所以
  `run_sync_once` 加了 `State` 级异步 Mutex 串行化：后台轮询与 GUI「立即同步」/
  `qxync sync --once` 并发时，后者排队等本轮结束，不会用旧快照覆盖已落盘结果。
- **文本对比可能 OOM / 卡死**：LCS 的 DP 表按行数平方分配，512 KiB 的短行文本最坏
  26 万×26 万×4B ≈ 275 GB。加有界回退（差异区各截前 2000 行做精确 LCS，最坏 16 MiB，
  `truncated=true` 且绝不误报「逐行相同」）。
- **diff 边界**：空远端文件不再发可疑的 `bytes=0--1`；超限时报告**真实大小**
  （优先取 FUSE 节点元数据，而不是被截断的读取长度）。

#### 协议层 / CLI

- **`download_range` 的「200 + JSON 错误体」防护是死代码**：`body.windows(9)` 去匹配
  8 字节的 `"status"`，条件恒为 false，这道保险从上线起从未生效 → 服务端错误体被当
  文件内容静默返回。改为精确识别「服务端错误信封」（不误伤用户同步的 JSON 文件）。
- **`download_range` 区间边界**：`start > end` 会让服务端忽略 Range 回整个文件
  （FUSE 会把整文件写进区间缓存）；服务端忽略 Range 时也没有检测。两处都显式报错。
- **上传流把读错误吞成 EOF**：`reader.read(...).await.ok()?` 让本地 IO 错误变成
  「流正常结束」，按其声明长度发短 body —— 轻则错误根因丢失，重则服务端落盘截断文件。
- **会话保活把「服务端未就绪」判成「会话失效」**：`status=8`（Qsync Central is
  initializing）本该等待，旧逻辑每轮无意义重登并打出错误诊断「sid 已失效」。
- **LAN 对端两个真 bug**：`local_of` 用 `trim_start_matches` 会**反复**剥前缀
  （`/home/home/x` → `x`）→ 对端读到的可能是**另一个文件**；`path_allowed` 在 roots
  为空时 fail-open → 任意路径可被对端读取。改为单次 `strip_prefix` 与 fail-closed。
- **`read_cached` 会拿「半水合文件的 0 字节」当本地内容**做 diff（假差异，可能让用户
  选错裁决）：改为先校验所需区间都已就绪（内存位图 / 纯读 `.qxstate` 且核对
  size/mtime/file_id），未就绪返回明确原因而不是假数据。
- **CLI `conflicts` 参数静默优先**：`--clear`/`--diff`/`--forget`/`--resolve` 同时给时
  按代码顺序静默取一个（用户以为都执行了）。改为 clap 互斥报错（exit 2），
  `--as` 必须配 `--resolve`。
- **CLI 累计计数器标注**：「删除被挡 N」、上传/删除队列的完成/失败/重试、水合统计等
  补「（累计，自 daemon 启动）」；「最近错误」双重前缀修复。

### 变更

- 冲突策略为 `ask` 时，`conflicts` 计数现在是「**待裁决的文件数**」（首次入队才 +1），
  不再按轮次累计；`deletes_blocked` 仍是累计值（已标注）。
- 回收站排除是**硬约束**：即使用户写了 `!@Recycle` 之类反向规则也不会把它们放回同步
  （与 FUSE `deny_trash` 的硬约束一致）。

### 已知问题（本轮未修）

- diff 依赖 FUSE 节点：daemon 重启后、用户还没在挂载点里访问过该路径时点「对比」会得到
  「本进程还没物化这个文件」的说明（需要 FUSE 侧新增按路径物化节点的 API）。
- 差异区超过 2000 行/侧时走有界回退：界面脚注会明确说「结果已截断」，但 `added/removed`
  统计只覆盖**已对比的前缀**（差异极大时数字偏小）。
- FUSE 挂载点里**仍能看到**回收站目录（引擎已不管理；是否隐藏属产品决定）。
- `.Trash-<uid>` 只认本机 uid；若 NAS 侧 uid 不同则不会排除（真机当前一致）。
- 持续重复的同步 errors 仍每轮 WARN 一次（长时间断网时日志仍会增长；blocked 已去重）。
- GUI：删除队列（`StatusData.deletes`）尚未展示；文件页三态/LAN 面板查询失败仍静默
  降级；无单实例保护；`pick_with` 无超时；`link_read` / `credential_present` 吞错；
  `ui_spec` 的自检存在三处「声称比实际多」（screens 统计把定义算进去、states 只验
  数量、i18n 只扫单引号 `T('`）。
- 协议层：`download_to_file` 尚未复用「200 + JSON 错误体」防护；HTTP 401/403 归为
  Transport 不触发重登（真机鉴权失败走 200+status 4/5，风险低）；上传无取消接口；
  LAN 配对限流是全局的、对端可登记任意 addr（低危）。
- `status=3` 目前只在 `qbox_get_max_log` 端点识别为会话失效（其它端点证据不足）。

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

[0.7.14]: https://github.com/mlzxgzy/qxync/compare/v0.7.13...v0.7.14
[0.7.12]: https://github.com/mlzxgzy/qxync/compare/v0.7.11...v0.7.12
[0.7.4]: https://github.com/mlzxgzy/qxync/compare/v0.7.3...v0.7.4
[0.7.3]: https://github.com/mlzxgzy/qxync/compare/v0.7.2...v0.7.3
[0.7.2]: https://github.com/mlzxgzy/qxync/compare/v0.7.1...v0.7.2
[0.7.1]: https://github.com/mlzxgzy/qxync/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/mlzxgzy/qxync/compare/v0.6.8...v0.7.0
[0.6.4]: https://github.com/mlzxgzy/qxync/compare/v0.6.3...v0.6.4
[0.6.3]: https://github.com/mlzxgzy/qxync/compare/v0.6.2...v0.6.3
[0.6.2]: https://github.com/mlzxgzy/qxync/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/mlzxgzy/qxync/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/mlzxgzy/qxync/compare/v0.5.1...v0.6.0
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
