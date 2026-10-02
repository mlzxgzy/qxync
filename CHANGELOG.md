# 变更日志

本文件格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)。

[English](CHANGELOG.en.md) · [README](README.md) · [验收记录](docs/验收记录.md)

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

[0.3.0]: https://github.com/mlzxgzy/qxync/compare/v0.2.3...v0.3.0
[0.2.3]: https://github.com/mlzxgzy/qxync/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/mlzxgzy/qxync/releases/tag/v0.2.2
[0.1.1]: https://github.com/mlzxgzy/qxync/releases/tag/v0.1.1
[0.1.0]: https://github.com/mlzxgzy/qxync/releases/tag/v0.1.0
