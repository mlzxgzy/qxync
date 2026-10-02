# qxync —— Qsync for Linux（带 on-demand 按需同步）

[![CI](https://github.com/mlzxgzy/qxync/actions/workflows/ci.yml/badge.svg)](https://github.com/mlzxgzy/qxync/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#许可证)
[![Rust](https://img.shields.io/badge/rust-1.90%2B-orange.svg)](Cargo.toml)
[![Platform](https://img.shields.io/badge/platform-Linux-lightgrey.svg)](#快速开始)

**中文** · [English](README.en.md) · [免责声明](DISCLAIMER.md) · [变更日志](CHANGELOG.md) · [验收记录](docs/验收记录.md)

用 Rust + FUSE 写的**第三方 QNAP Qsync Linux 客户端**，核心是 **on-demand 按需同步**：
NAS 上的文件在本地只是一个「占位符」，`ls -l` 显示真实大小却**零下载**，读到哪一块才取哪一块；
不想留的就**脱水**回占位符，本地磁盘占用随时可控。

> ⚠️ **只用于你自己拥有、或已获明确授权的 QNAP NAS。**
> 本项目**不分发**任何 QNAP 二进制/安装包/反编译产物，**不绕过**任何授权或技术保护措施，
> 与 **QNAP Systems, Inc. 无任何关联**。完整条款见 **[免责声明](DISCLAIMER.md)**。

---

## 这是什么

一个**跑在 Linux 上的 Qsync 客户端**，由三个二进制组成：

| 二进制 | 角色 |
|---|---|
| `qxync` | 命令行（登录 / 列举 / 上传下载 / 挂载 / 同步 / 脱水 / 任务 / 设置 / 日志 / LAN 对等） |
| `qxyncd` | 常驻守护进程：**唯一**持有 FUSE 挂载与 NAS 会话，对外提供本地 unix socket JSON IPC |
| `qxync-gui` | Tauri 2 桌面应用（零依赖静态前端，全部经 daemon 的 IPC） |

**它是什么**：对 QNAP 官方客户端所做行为的**互操作性实现** —— 协议细节来自对
**Qsync for Windows v6.1.0.0831** 的静态逆向分析，
每一处结论都对真机验证过。

**它不是什么**（明确的能力边界）：

- ❌ **不是**官方客户端，也不代表 QNAP 的立场（[免责声明](DISCLAIMER.md)）
- ❌ **不实现云账号 / QID / myQNAPcloud 登录**：只做「直连 NAS 的本地账号」这一条路
- ❌ **不与官方客户端的 WebSocket 二进制通道互通**（线格式未还原），LAN 加速是 qxync↔qxync 自研协议
- ❌ **不写 NAS 的设备列表**、不做设备注册，也不修改 NAS 侧任何配置
- ❌ 不支持 Windows / macOS（FUSE 与 `/proc` 语义是 Linux 专属）

## 现在能做什么

| 能力 | 说明 | 文档 |
|---|---|---|
| **按需水合** | 占位符 + **128 KiB 区间**按需下载（`head -c 100 big.bin` 只取 1 个区间，不是整个文件） | [M1.5](docs/M1.5-设计.md) |
| **读写挂载** | `--rw` 后本地改动经上传队列推回 NAS；写前 **read-modify-write**，绝不把没取回的区间当 0 上传 | [M2b](docs/M2b-写路径.md) |
| **变更发现** | 三游标轮询 + baseline 三向对账；冲突生成**冲突副本**，远端批量删除有**熔断**保护 | [M2c](docs/M2c-变更发现.md) |
| **脱水（释放空间）** | 完整安全检查链（pin / 未上传改动 / 打开的 fd / 被 mmap / 正在水合 / 刚访问过）后才清本地内容 | [M3](docs/M3-脱水.md) |
| **本地状态库** | SQLite 承载游标 / baseline / pin / 上传队列，**同一事务**落盘；librsync 兼容的 delta 编解码 + 能力门控 | [M5](docs/M5-SQLite与delta.md) |
| **多根 / 共享文件夹** | `roots` 配置多根；家目录可读写，非家目录根在 FUSE 层直接 `EROFS` | [M6](docs/M6-多根与共享文件夹.md) |
| **选择性同步** | gitignore 风味的 `exclude` 规则（锚定 / `**` / 反向包含 / 子树剪枝）+ 内置临时文件过滤 | [M7](docs/M7-选择性同步与LAN直连.md) |
| **LAN 直连** | qxync↔qxync 自研对等协议：设备配对、事件快路径、本地区间直传（失败一律静默回落 NAS） | [M7](docs/M7-选择性同步与LAN直连.md) |
| **同步任务** | `tasks/<id>.json` 持久化的挂载登记，逐任务暂停/继续，重启可 `--restore-tasks` 恢复 | [M8](docs/M8-向Qsync-Client-6靠拢.md) |
| **同步日志** | `sync.db` 的 `journal` 表 + 后台批量落库与轮转，驱动 GUI 的「文件更新中心 / 错误列表」 | [M8](docs/M8-向Qsync-Client-6靠拢.md) |
| **桌面 GUI** | 主页 / 任务 / 文件 / 更新 / 错误 / 设置 / 诊断；托盘 + 通知 + 开机自启 + 文件选择器 | [M4](docs/M4-GUI.md) · [M8](docs/M8-向Qsync-Client-6靠拢.md) |
| **设置中心** | 代理三模式（Auto-detect / No proxy / Manual）、自动释放空间、冲突策略五选、文件三态 | [M8](docs/M8-向Qsync-Client-6靠拢.md) |

## 截图

> 下列截图为**脱敏后**的界面（真实 NAS 地址与本机路径已涂抹）。

| 主页 | 文件 |
|---|---|
| ![主页](docs/images/gui-home.png) | ![文件](docs/images/gui-files.png) |

| 文件更新中心 | 设置 · 代理 |
|---|---|
| ![更新](docs/images/gui-journal.png) | ![代理](docs/images/gui-settings-proxy.png) |

## 快速开始

### 1. 依赖

- **Rust 1.90+**（`rust-version` 见 [`Cargo.toml`](Cargo.toml)）
- **FUSE 3**：内核 `/dev/fuse` + `fusermount3`（大多数发行版装 `fuse3` 即可）
- **GUI 额外需要** WebKitGTK 4.1 与 GTK 3 的开发包
  （如 Debian/Ubuntu 的 `libwebkit2gtk-4.1-dev` + `libgtk-3-dev`；
  各发行版的完整清单见 Tauri 2 官方文档的 Prerequisites 一节）
- 前端是 `crates/qxync-gui/ui/` 下的**零依赖静态三件套**，**不需要 npm / 打包器**

### 2. 构建

```bash
git clone https://github.com/mlzxgzy/qxync.git
cd qxync
cargo build --workspace           # debug
cargo build --workspace --release # release（已配 lto=thin；保留行号回溯便于报 bug）
```

如果要出 `.deb` / AppImage 安装包，`crates/qxync-gui/tauri.conf.json` 里已经配好
`bundle.targets = ["deb", "appimage"]`，用 Tauri CLI 打包即可。

**预编译产物**：推 `v*` tag（或手工触发 `Release` 工作流）后，由
[`.github/workflows/release.yml`](.github/workflows/release.yml) 自动构建并挂到对应
[Release](https://github.com/mlzxgzy/qxync/releases)：

| 资产 | 说明 |
|---|---|
| `qxync-<版本>-x86_64-unknown-linux-gnu.tar.gz` | 三个二进制 + 桌面项 / 图标 + 许可证 / 免责声明 |
| `qxync-bin-<版本>-1-x86_64.pkg.tar.zst` | **Arch 包**（CI 用 archlinux 镜像里的真 `makepkg` 从上面那个 tar.gz 打的） |
| `qxync` · `qxyncd` · `qxync-gui` | 三个裸二进制（同一份构建，供挑着下） |
| `SHA256SUMS` | 上述资产的校验和 |

三个二进制放在**同一个目录**即可：GUI 先找自己旁边的 `qxyncd`，找不到再退回 `PATH`。
运行 GUI 还要系统里有 WebKitGTK 4.1 / GTK 3 运行时（Debian/Ubuntu 是
`libwebkit2gtk-4.1-0` + `libgtk-3-0`）。**仍然建议优先从源码构建**（可复现、可审计）。

**Arch Linux**：Release 里直接带打好的 `qxync-bin-<版本>-1-x86_64.pkg.tar.zst`，
`sudo pacman -U` 就能装；仓库里还带一份 AUR 用的包定义
[`packaging/arch/PKGBUILD`](packaging/arch/)（包名 `qxync-bin`），在 `packaging/arch/` 下
`makepkg -si` 可以自己打，推到 AUR 之后就是 `yay -S qxync-bin`。
包**有意不 strip**（为了回溯可读，见上一节），所以装完约 190 MB —— 细节与发版后的
校验和更新见 [`packaging/arch/README.md`](packaging/arch/README.md)。

### 2.5 从 0.1.x 升级（改名了）

0.1.x 的命令行叫 `qsync`，会和 **QNAP 官方 Qsync 客户端**在 `PATH` 里抢同一个名字；
0.2.2 起统一叫 `qxync`。完整清单见 [CHANGELOG](CHANGELOG.md#022---2026-10-02)。

升级后第一次运行时，本地目录会**自动迁移**：`~/.config/qsync`、`~/.local/share/qsync`、
`~/.local/state/qsync` 会被直接改名成对应的 `qxync` 目录（跨文件系统时退回复制），
所以凭据、状态库、日志都跟着走，**不需要重新 `login`**。如果新旧目录同时存在
（例如早期原型留下的 `~/.local/share/qxync`），只把旧目录里**缺**的条目补进去，
**已有的一律不覆盖**，并打一行提示告诉你两个路径。唯一要动的是你自己的脚本：

| 旧写法（0.1.x） | 新写法（0.2.2+） |
|---|---|
| `qsync …` | `qxync …` |
| `QSYNC_PASSWORD` / `QSYNC_HOST` / `QSYNC_USER` / `QSYNC_SOCKET` | `QXNYC_PASSWORD` / `QXNYC_HOST` / `QXNYC_USER` / `QXNYC_SOCKET` |
| `QSYNC_TEST_*`（验收矩阵） | `QXNYC_TEST_*` |
| `getfattr -n user.qsync.state` | `getfattr -n user.qxync.state` |

> **名字边界**：只有**我们自己的**标识改了。NAS 协议面照旧 —— `cgi-bin/qsync/qsyncsrv.cgi`、
> `qsync_version`、`service=Qsync`、`WFM_QSYNC_DISABLED` 这些**一字未动**。

> **daemon 不用等登录也能起**：一份连接配置都没有时 `qxyncd` 会**空转待命**（界面显示
> 「未配置」，`qxync daemon status` 里连接是 `未配置`），配好连接后它自己转入同步 ——
> 所以「先把 daemon 挂后台」和「稍后再登录」不冲突。

### 3. 首次登录

凭据写入 `~/.config/qxync/credentials.json`（权限 `0600`）。

```bash
cargo run -p qxync-cli -- \
  --host <你的NAS> --port 9834 --insecure \
  --user <用户> --password '<口令>' login
```

> `--insecure` = 接受自签证书。**口令不要写进 shell 历史**：
> `--password` 也可以用 `QXNYC_PASSWORD` 环境变量代替。

### 4. 挂载按需同步视图

```bash
cargo run -p qxync-cli -- daemon start            # 拉起 qxyncd（幂等）
mkdir -p ~/qxync-mnt
qxync mount ~/qxync-mnt --remote /home            # FUSE 由 daemon 持有（默认只读）
qxync mount ~/qxync-mnt --remote /home --rw       # 需要写回时就加 --rw

ls -l ~/qxync-mnt/qxync-test          # 真实大小，尚未下载
cat ~/qxync-mnt/qxync-test/hello.txt  # 首次读触发按需水合（只取需要的区间）
getfattr -n user.qxync.state ~/qxync-mnt/qxync-test/hello.txt   # placeholder / partial / hydrated

qxync dehydrate --path /home/qxync-test/big.bin   # 脱水：丢本地内容、只留占位符
qxync umount ~/qxync-mnt
qxync daemon stop                                 # 干净退出：卸载全部挂载 + 删 socket/pid
```

> **没配 NAS 也能先把 daemon 挂后台**：一份连接配置都没有时它会「空转待命」（不做事但一直活着，
> 界面显示「未配置」），配好连接后自己转入同步 —— 所以「先挂后台、稍后登录」不冲突。

#### 4.1 用 systemd 让它常驻（推荐）

`packaging/systemd/qxyncd.service` 是一个 **user 单元**，装了包的话它在
`/usr/lib/systemd/user/qxyncd.service`：

```bash
systemctl --user enable --now qxyncd     # 登录即启动（现在也启动）
systemctl --user status qxyncd
journalctl --user -u qxyncd -f           # 日志（daemon 的 stderr 进 journald）
systemctl --user stop qxyncd             # 优雅停止：卸载 FUSE 挂载 + 删 socket/pid
sudo loginctl enable-linger "$USER"      # 想「不登录也常驻」（开机就起）再执行这条
```

> 用了 systemd 单元之后就**别再用** `qxync daemon start` / `qxync daemon stop` 管它 ——
> 那条路是给没有 systemd 的用法准备的，两边同时用只会互相打架。
> （`qxync daemon stop` 走 IPC 让进程自己退出，systemd 会看到主进程消失。）

### 5. GUI

```bash
cargo build -p qxync-gui
qxync daemon start
./target/debug/qxync-gui

# 无窗口自检（脚本 / CI 用；daemon 在跑时退出码 0）
./target/debug/qxync-gui --self-test
./target/debug/qxync-gui --self-test-login   # 额外跑一遍「保存并登录」整条链（会重启 daemon）
```

界面左侧是图标栏：**主页 / 任务 / 文件 / 更新 / 错误 / 设置 / 诊断**。
GUI 自己不发 HTTP，**全部经 daemon 的 IPC**。

### 6. 常用命令速查

```bash
qxync status                     # 会话 + 服务端 + 游标 + 水合统计 + 挂载
qxync ls /home                   # 列目录（自动翻页）
qxync store [--integrity|--json] # 状态库快照（游标 / baseline / pin / 上传队列）
qxync roots [--json]             # 远端根一览 + 可读/可写判定
qxync rules [--match <路径>]     # 选择性同步规则判定（visible / excluded / temp / outside-roots）
qxync sync [--once]              # 变更发现状态；--force-deletes 放行批量删除
qxync task list|add|pause|resume|rm
qxync journal [--level error]    # 同步活动日志（--level error 就是「错误列表」）
qxync settings [--set k=v]       # 代理 / 开机自启 / 通知 / 释放空间
qxync space [--now]              # 释放空间状态 / 立即释放
qxync conflicts --resolve <id> --as keep_local|keep_remote|keep_both
qxync file-states /home          # 文件三态：仅在线 / 本地可用 / 始终可用
qxync peer status|pair|ping|events|fetch    # LAN 对等（需先在 link 里配 peer_listen）
```

全部子命令见 `qxync --help`；IPC 契约（unix socket + 一行一个 JSON）见
[`docs/M1.5-设计.md`](docs/M1.5-设计.md)。

## 架构

**进程模型**：`qxyncd` 是**唯一**持有 FUSE 与 NAS 会话的进程。`qxync` 默认**自动路由** ——
socket 可连就走 IPC（`--via-daemon` 强制、`--direct` 跳过），所以 `qxync ls /home/x` 对用户无感。

```
qxync (CLI) ──┐
              ├─IPC(unix socket)──> qxyncd ──┬── FUSE 挂载（按需水合 / 写回 / 脱水）
qxync-gui ────┘                              ├── 同步引擎（轮询 + baseline 对账 + 冲突/删除保护）
                                             └── qxync-client ──HTTP──> NAS
                                                  └── LAN 对等（qxync↔qxync）
```

```
crates/
├── qxync-core/        共享类型 + 配置布局 + 状态库（SQLite）+ delta 编解码 + 多根布局 + 规则引擎
├── qxync-client/      NAS HTTP API 封装（登录 / 元数据 / 上传下载）+ LAN 对等协议
├── qxync-fuse/        FUSE 层：只读/读写挂载 + 区间水合 + 脱水（含上传队列）
├── qxync-daemon/      二进制 qxyncd：常驻进程 + IPC 服务端 + 同步引擎 + 对端监听
├── qxync-cli/         二进制 qxync：命令行
├── qxync-gui/         二进制 qxync-gui：Tauri 2 应用（ui/ 为零依赖静态前端）
└── qxync-proto-test/  真机集成测试（默认 #[ignore]，手动跑）
xtask/tests/           8 个验收矩阵脚本（见「验收与测试」）
xtask/probe/           协议探测工具（qs_probe / qs_fixture / nas_manifest / p0_device_probe）
docs/                  设计与执行文档
DISCLAIMER.md          免责声明与法律边界
```

依赖方向（只允许向下）：`cli → core`（+ 经 IPC 访问 daemon）；`daemon → fuse/client → core`。

## 实现要点

**元数据不走数据面**：`ls -l` 直接答 NAS 元数据（真实大小、零下载）。缓存是
「apparent size = 文件大小」的稀疏文件，只把读到的区间 `pwrite` 进去；
`user.qxync.state` 暴露 `placeholder`/`partial`/`hydrated`，`user.qxync.chunks` 暴露「已就绪/总数」。
缓存文件名用**远端路径的稳定哈希**（不能用 ino —— 两次挂载里同一个 ino 可能对应不同文件）。

**脱水前先过完整安全检查链**（`qxync-core/src/dehydrate.rs` + `qxync-fuse`/`qxyncd`）：
pin=pinned/excluded、未上传改动/队列在途、打开的 fd、被 mmap（扫 `/proc/*/maps`）、
正在水合、刚访问过。全部通过后按**铁则 2** 执行 `inval_inode(0,0)` → 清缓存内容 → 更新占位符；
`inval_inode` 失败就**什么都不清**。默认**不**自动脱水，
由 `QXNYC_DEHYDRATE_IDLE=600`（闲置）/ `QXNYC_CACHE_LIMIT=2G|25%`（限额，LRU）触发，
或手动 `qxync dehydrate`。`--cache-mode direct` 用 `FOPEN_DIRECT_IO` 绕过 page cache
（脱水天然安全，代价是没有 readahead、mmap 不可用）。

**变更发现以 baseline 对账为主路径**：daemon 每 30s（`QXNYC_POLL_INTERVAL` 可调）跑一轮
「三游标 + baseline 对账」—— 先拉事件快路径，再按「已知目录列举 + baseline 差集」兜底。
远端改动 → 刷新元数据并**失效本地缓存**（下次读按需水合新内容）；双方都改 → **冲突副本**
（远端占原名，本地内容存 `xxx (conflicted copy from <设备> <日期>).txt` 并上传）；
远端批量删除 → **熔断**（`qxync sync --force-deletes` 才放行）。

**状态搬进了 SQLite**（`qxync-core/src/store.rs`）：`<data>/sync/<host>/sync.db` 承载三个事件游标
+ baseline + **pin**（以前只在内存里，daemon 一重启就丢 → 脱水安全检查会静默失守）+ 上传队列。
**游标与 baseline 在同一个事务里落盘** —— JSON 时代两次 `rename` 之间崩溃会出现
「游标推了、baseline 没推」。老 `cursors.json`/`baseline.json` 首次启动自动迁移并归档成
`*.json.migrated`（保留备份、幂等）。

**多根只在挂载点这一层加名字映射**：link 配置 `roots`（默认 `["/home"]`），
多根时挂载点顶层出现每个根的名字（`home/`、`Public/`），**下面所有层（缓存/baseline/pin/xattr/
上传队列）仍用远端路径做键**。单根是**直通**（挂载点就是那个根），M1–M5 的行为一字不改 ——
`roots.rs` 里专门有一条「单根必须还是 Passthrough」的断言守着这件事。

## 已知限制

* **托盘需要 SNI 宿主**：qxync 走的是 `org.kde.StatusNotifierItem`（ksni 实现，
  不是 libappindicator）。它覆盖 Plasma / waybar / polybar / XFCE（`statusnotifier` 插件）/
  LXQt / Cinnamon / GNOME + AppIndicator 扩展；但 **IceWM / Fluxbox / Openbox+tray /
  老式 XFCE·MATE 面板只有 XEmbed**、**裸 GNOME 两个协议都不支持** —— 这两类环境里托盘图标不会出现
  （可装 [`snixembed`](https://sr.ht/~steef/snixembed/) 把 SNI 桥进老式托盘）。
  程序会**探测**自己是否真的可见（watcher 有 host + 本进程 item 已登记），
  **不可见时关闭窗口会真的关闭**，不会把窗口藏进一个没人画的托盘里。
* **「按频率」自动释放空间**的「上次触发时间」只在内存里，daemon 重启会重新计时
  （「当空间少于 X%」不受影响）。
* **i18n 只覆盖界面文案**：`ui/i18n.js` 是 zh-CN 文案表（163 条）+ **预留的 en 空表**
  （空表 = 整条回落到 zh-CN，不会出现空白）。诊断日志与 `index.html` 里带内联 `<code>` 的
  混合标记段落**有意不进表**。文案表由 `qxync-gui --self-test` 的 `ui_spec` 双向自检守住，
  **不引入任何 i18n 框架**。
* **直连（`--direct`）模式下改 `settings.json` 只写文件**，跑着的 daemon 要重启才读到新设置。
* **能力门控而非能力假设**：NAS 侧没有历史版本时，delta 走门控、真实路径仍是整文件传输
  （见 [`M5-SQLite与delta.md`](docs/M5-SQLite与delta.md)）。
* 本项目只在 **QNAP TS-464C / QTS 5.2.9 / Qsync QPKG 5.0.0.7（build 20260723）**
  上做过完整真机验证；其它型号/QTS 版本可能踩到未覆盖的行为差异。

## 协议要点（踩过的坑）

完整证据与探索过程见 [`docs/执行方案-M0M1.md`](docs/执行方案-M0M1.md)；
写代码时最容易踩的几条：

**登录与读路径**

1. **登录**：`POST /cgi-bin/authLogin.cgi`，body 必须 `serviceKey=1` + `pwd=base64(口令)`。
   明文口令、或报告里写的 `service=Qsync`，都只会得到 `authPassed=0 / errorValue=-1`。
   （登录协议的权威参考是 NAS 自带前端 `/cgi-bin/js/qos-core-login.js`，不是 Windows 二进制。）
2. **`q_token` 不是必需的**：真机 `qsyncsrv_login.cgi` 恒返回 `status:-50`，但只读端点用 QTS `sid` 直接可用。
3. **命名空间分工**：元数据走 `/cgi-bin/qsync/qsyncsrv.cgi`；
   **字节流走 `/cgi-bin/filemanager/utilRequest.cgi?func=download`（下载）与
   `/cgi-bin/qsync/upload.php`（上传）**。`qsyncsrv.cgi?func=download` 恒返回 `status:20`，别用它。
4. **上传的 multipart 字段名必须是 `files[]`**（blueimp 风格），
   `upload_and_move` / `func=upload` 收不到文件体。
5. **查询串里空格必须编码成 `%20`**：用 `+` 会让含空格/中文的文件名 404
   （所以本项目不用 `serde_urlencoded`）。
6. **`stat` 要 `path=<目录>&file_name=<名字>&file_total=1`**，不是全路径；
   `get_list` 要带 `hidden_file=1` 才见隐藏文件。
7. **`/home` 才是普通用户的家目录根**（真实路径 `/share/homes/<user>`），
   `/home/<user>` 会 `status:5`。
8. `Range: bytes=0-99` → **HTTP 206 + `Content-Range`**，区间水合有原生支持。
9. **`stat` 用 `exist` 判存在**：不存在的路径也返回占位条目（名字是你请求的名字、`filesize=0`），
   只有 `exist=0` 能区分；判错会让 `lookup` 误报正项、`mkdir` 直接 `EEXIST`。

**写路径与 FUSE**

10. **写操作命名空间分工**：`rename`/`move` 只能在 FileStation（`utilRequest.cgi`）做，
    `createdir`/`delete` 用 `qsyncsrv.cgi`；`move` 必须带 `source_total=1`，
    且 **`dest_file` 会被忽略**（跨目录改名 = move + rename 两步）。
11. **写前必须 read-modify-write**：写占位符前要把「不会被完整覆盖」的区间补齐，
    否则未取回的区间是 0，整文件上传会把远端内容清零（实测踩过）。
12. **`listxattr` 的返回值必须以 NUL 结尾**：内核 `fuse_verify_xattr_list()` 会逐项 `strnlen`，
    最后一项少了终止符就**把整个 listxattr 判成 `-EIO`** —— 表现是 `ls -l` 全目录报
    「输入/输出错误」，而 `stat`/`cat` 都正常。实测判据：`size<66` 回 `ERANGE`，
    `size>=66` 反而 `EIO`，就是这个校验触发的。
13. **`attr_timeout`/`entry_timeout`/`max_read` 不是 fusermount 挂载选项**，传给 `-o` 会直接
    `unknown option` 挂载失败；TTL 应通过每次 `reply.entry/attr(&ttl, ..)` 传，
    `max_readahead` 在 `init()` 里设。
14. **fuser 0.17 的 `AutoUnmount` 要求 `SessionACL != Owner`**（即 `allow_other`，
    非特权挂载还需 `/etc/fuse.conf` 的 `user_allow_other`），否则 mount 报
    `auto_unmount requires acl != Owner`。
15. FUSE 调用里 `block_on` 要用**独立运行时**，且挂载线程别用 `tokio::spawn_blocking`
    （blocking 线程带 runtime 上下文，再 `block_on` 另一个 runtime 会 panic）。
16. **fuser 的 `mount2()` 拿不到 `Notifier`** → 改用 `fuser::spawn_mount2()`（daemon 侧），
    拿到 `BackgroundSession`（join/卸载）+ `Notifier`（`inval_inode`）。
17. 对端同时发布 AAAA 但 IPv6 路由不通时，会出现 `Network is unreachable` 或传输中途
    body 解码失败 → 用 `--ipv4`（客户端 `local_address` 绑 IPv4 源地址）规避。

**变更发现**

18. **`qbox_get_sync_log` 的 `lower` 是闭区间下界**（`lower=30` 会返回 `log_id=30`）→
    游标推进到「最后一条 `log_id` + 1」；区间内没有事件时返回 **`status:-17`**，这不是协议错。
19. **事件里 `isfolder` 是 `1`=目录 / `2`=文件 / `0`=删除项**（不是布尔）；`size` 是字符串。
20. **删除事件的 `filepath` 实测为空**，而且**我们自己的 CGI 写操作（upload/rename/move/delete）
    不产生 sync log 事件** → 变更发现必须**以 baseline 对账为主路径**，事件只是快路径。
    P0 探针更正了归因：原因**不是**「本机未做设备配对」（NAS 上早有官方客户端注册的设备），
    而是该账号 `qbox_get_syncing_folder_list` 是 `total:0`（**从未登记过同步文件夹**）；
    真正的闸门是「路径落在已注册的同步文件夹里」，而现有端点清单里**没有注册同步文件夹的端点**
    → 这条路走不通，**baseline 对账主路径的结论不变、且证据更强**。
21. `qbox_write_log` 会让 `max_log` 上涨但区间内取不到事件 → 游标**只按实际返回的事件推进**，
    `-17` 时不推进、只记账，避免「推进了游标但事件丢了」。

**脱水**

22. ★ **顺序铁则**：`Notifier::inval_inode(ino, 0, 0)` 必须在清内容**之前** ——
    否则内核 page cache 里的旧页会让应用读到旧数据。验收方式：脱水后把远端改成
    **同长度不同内容**再 `cat`，必须拿到新内容。
23. **mmap 挡不住**：Linux 的 `flock` 不阻止 mmap → 只能自己扫 `/proc/*/maps`。
    实测补充：mmap 会给映射保留 `struct file`，进程 `close(fd)` 后 FUSE `release` 也不触发，
    所以「打开的 fd」计数本身就是第一道防线（`/proc` 扫描是兜底）。
24. 本地写入的区间必须记进区间表（`chunks_done`），否则本地新建/改过的文件会被判成
    「没有缓存内容」（实测：16 MiB 本地文件脱水被报「本来就是占位符」）。
25. 稀疏缓存别用 `du -sb`（apparent size）量占用 —— 128 MiB 的稀疏文件会算成 128 MiB；
    要 `du -s --block-size=1`（allocated）。

**状态库与 delta**

26. **换持久化时一定要确认没有老进程在跑**：调试中发现「迁移归档后 `cursors.json` 又冒出来」，
    查下来是上一轮会话遗留的**旧二进制 daemon** 还在按 30s 轮询、用旧代码写 JSON
    （mtime 恰好落在轮询节拍上是判据）。验收矩阵用**全新状态目录**就是为了隔离这类污染。
27. **`rusqlite` 用 `bundled`**：自带 SQLite 源码，不需要系统 `libsqlite3-dev`；
    WAL 会在状态目录留下 `-wal`/`-shm`，属正常现象，别当垃圾清掉。
28. **delta 的格式细节都是「必须显式覆盖」的默认值**：librsync 默认 block 2048 / strong 8，
    Qsync 用的是 **1 MiB / 16 字节 MD4**；magic 是 `0x72730136`(sig) / `0x72730236`(delta)，
    全部**大端**。弱校验的 `CHAR_OFFSET=31` 差一点就对不上。
29. **能力探测不能当成「有端点就是支持」**：这台 NAS 的 `versioning_probe` 三个 enable 位
    全是 1、`versioning_lock` 也能拿到 lockid，但 `versioning_stat_delta` 恒 `exist:0`、
    `versioning_support` 全 0 —— 真正的判据是「**有没有历史版本**」，
    只看端点存在会得出完全错误的结论。

**多根 / 共享文件夹**

30. **共享文件夹「能读不能写」**：只要账号有读权限，用普通 `sid` 就能列 / stat / 下载
    （`/Public`、`/Multimedia` 实测通过），**不需要** `auth_data` AES；但上传必须是 Qsync
    同步文件夹，否则服务端只回一句含糊的 `status:20`。所以非家目录根一律按只读处理，
    写操作在 FUSE 层直接回 `EROFS`。
31. **顶层共享列表枚举不出来**：`get_list /` 对普通用户是 `status:5`；
    `qbox_get_syncing_folder_list` 返回的是「NAS 上登记过同步的文件夹」
    → 根只能由用户配置，不能自动发现。端点 200 但空数组不是错误。
32. **多根绝不能动单根那条路**：多根逻辑全部以 `multi_root` 为开关；
    否则 M1–M5 的 FUSE 矩阵会整体失效。
33. **脱水候选与 mmap 映射必须按根展开**：否则同一个文件被每个根各算一遍
    （`freed_bytes` 翻倍），更糟的是 mmap 映射错了会让「被 mmap 的文件不脱水」
    这条安全检查失守 —— 直接违反脱水铁则。

**GUI**

34. **`hidden` 属性压不住作者样式里的 `display`**：`.env-banner { display: flex }` 让
    `<div hidden>` 在 Tauri 里**永远可见** —— 表现是 IPC 全通却顶着一条「未在 Tauri 中运行」红条。
    全局加 `[hidden] { display: none !important; }`（loading/空态/结果框同类元素一起受益）。
35. **首轮 `status` 没回来就切 tab → 空态**：`requireLogin()` 依赖 `state.lastStatus`，
    页面刚起来时它是 `null`，「文件 / pin」页的 `ls` 直接被跳过。
    修法：首轮 status 回来后补一次当前 tab 的刷新。
36. Tauri 2 的命令参数是 **camelCase**（Rust `link_id` → JS `{linkId}`），但传进去的**对象内部
    字段仍是 snake_case**（`home_root`/`force_deletes`/`cache_mode`/`hydrate_timeout_secs`…）；
    `frontendDist` 是**编译期**嵌入，改 `ui/` 必须重新 `cargo build`
    （`--self-test` 里的 `ui_assets` 字节数就是「资源有没有真的进包」的判据）。
37. **挂载失败会把 daemon 的 IPC worker 打成 panic**：`QxyncFs` 里的 tokio `Runtime`
    在 async 上下文里析构会触发
    `Cannot drop a runtime in a context where blocking is not allowed`。
    修法：包成 `FsRuntime`，按上下文选路（tokio 里用 `shutdown_background()`）。
    回归单测 `dropping_fs_inside_async_context_does_not_panic` 常驻 `cargo test`。

## 两条铁则

整个项目**不许违反**这两条：

1. **每次 `read()` 必须返回真实数据或明确 `EIO`，绝不短读。**
   短读 = 内核零填充 → 数据静默损坏。实测：水合失败/超时 → `cat` 拿 `EIO` 且**输出 0 字节**，
   不挂死、不吐假数据。
2. **每次脱水必须先 `inval_inode` 让内核失效缓存，再清内容。**
   顺序反了 = 数据错乱。

## 验收与测试

判据全部是真机 + 真挂载跑出来的，脚本在 `xtask/tests/`，**每一条都可以在你自己的 NAS 上复跑**。
里程碑级结果见 [`docs/验收记录.md`](docs/验收记录.md)。

```bash
cargo test --workspace                                   # 单测 + 文档测试（不需要 NAS）

xtask/tests/fuse-matrix.sh            # 68 项：区间水合 / 写路径 / 变更发现 / 脱水 / 状态库（~12min）
xtask/tests/fuse-matrix.sh --big      #    追加 128 MiB 全量读 + 并发去重
xtask/tests/m5-matrix.sh              # 30 项
xtask/tests/m6-matrix.sh              # 29 项（多根真挂载）
xtask/tests/m7-matrix.sh              # 60 项（规则 / FUSE 过滤 / LAN 配对·事件·直传）
xtask/tests/m7-matrix.sh --no-nas     #    不需要 NAS：单测 + 两个真 daemon 的 loopback
xtask/tests/m82-matrix.sh             # 37 项（任务登记 / 重启恢复）
xtask/tests/m83-matrix.sh             # 27 项（日志 schema 迁移 / 过滤 / 轮转）
xtask/tests/m84-matrix.sh             # 92 项（设置 / 代理 / 托盘 / 释放空间 / 冲突策略）
xtask/tests/gui-matrix.sh             # 148 项（9 个目的地真窗口截图 + ui_spec 静态自检）
xtask/tests/gui-matrix.sh --no-window #    无 DISPLAY 的机器只跑自检
```

**最近一次全量口径：491 项全过**（fuse 68 · gui 148 · m5 30 · m6 29 · m7 60 · m82 37 · m83 27 · m84 92）。

真机集成测试（`#[ignore]`，需要你自己的 NAS 凭据）：

```bash
export QXNYC_TEST_HOST=... QXNYC_TEST_PORT=9834
export QXNYC_TEST_USER=... QXNYC_TEST_PASSWORD='...'
export QXNYC_TEST_FIXTURE=/home/qxync-test
cargo test -p qxync-proto-test -- --ignored --test-threads=1 --nocapture  # 协议 5 项 + IPC 端到端 1 项
cargo test -p qxync-daemon -- --ignored --test-threads=1 --nocapture     # M2c 引擎（冲突副本 / 删除保护）
```

> **受限环境**：若 `~/.cargo` / `~/.config` 不可写，用工作区内的路径：
> `CARGO_HOME=$PWD/.cargo-home cargo build --workspace`、
> `XDG_CONFIG_HOME=$PWD/.local-run/config XDG_DATA_HOME=$PWD/.local-run/data`。

## 文档索引

| 文档 | 内容 |
|---|---|
| [`docs/开发规划.md`](docs/开发规划.md) | 第一版（MVP）规划 |
| [`docs/执行方案-M0M1.md`](docs/执行方案-M0M1.md) | 真机验证后的修正版：实测事实 + 修正项 + 执行顺序 + 风险门 |
| [`docs/M1.5-设计.md`](docs/M1.5-设计.md) | daemon / IPC 契约、生命周期、pin 语义、验收标准 |
| [`docs/M2b-写路径.md`](docs/M2b-写路径.md) | 写路径：真机写接口契约、read-modify-write 铁则、上传队列 |
| [`docs/M2c-变更发现.md`](docs/M2c-变更发现.md) | 三游标/事件契约、三向决策表、冲突副本、删除保护 |
| [`docs/M3-脱水.md`](docs/M3-脱水.md) | 安全检查链、`inval_inode` 顺序铁则、闲置/限额、cache-mode |
| [`docs/M4-GUI.md`](docs/M4-GUI.md) | GUI 边界、命令面、页面结构、验收与踩坑 |
| [`docs/M5-SQLite与delta.md`](docs/M5-SQLite与delta.md) | 状态库 schema/迁移/单事务、真机 versioning 探测、delta 能力门控 |
| [`docs/M6-多根与共享文件夹.md`](docs/M6-多根与共享文件夹.md) | 多根布局/只读规则、真机共享文件夹探测、FUSE 虚拟根 |
| [`docs/M7-选择性同步与LAN直连.md`](docs/M7-选择性同步与LAN直连.md) | exclude 规则引擎、对等协议线格式、事件快路径、直传 |
| [`docs/M8-向Qsync-Client-6靠拢.md`](docs/M8-向Qsync-Client-6靠拢.md) | GUI 改造研究 + M8.1–M8.6 执行方案与决策记录 |
| [`docs/验收记录.md`](docs/验收记录.md) | 里程碑级验收结论（跑了什么、结果是什么） |
| [`docs/发布清单-v0.1.0.md`](docs/发布清单-v0.1.0.md) | 发版前检查清单（维护者用） |
| [`CHANGELOG.md`](CHANGELOG.md) | 变更日志 |

## 安全与隐私

- **凭据**：`~/.config/qxync/credentials.json`（`0600`）；IPC socket 目录 `0700` / socket `0600`。
- **本项目不采集、不上报任何遥测数据**，也不连接除你配置的 NAS 与（可选的）LAN 对端以外的任何主机。
- **LAN 对等是明文 TCP**，只在可信局域网开启即可；token 只授权「读已水合文件 + 提交事件」，
  **没有任何写/删远端的能力**。
- **本项目不包含、不分发任何第三方客户端凭据**。协议探测工具
  （[`xtask/probe/`](xtask/probe/)）只使用**你自己**的 NAS 账号，
  原始响应落在 `xtask/probe/probe-out/`（已 gitignore，**含 sid 与账号，绝不提交**）。
- **仓库不含任何真实主机名、账号或设备指纹**（发版前已做统一清洗，见
  [`docs/发布清单-v0.1.0.md`](docs/发布清单-v0.1.0.md)）。

发现安全问题请**不要开公开 Issue**，见 [`SECURITY.md`](SECURITY.md)。

## 参与贡献

见 [`CONTRIBUTING.md`](CONTRIBUTING.md)。特别提醒：本项目有**两条铁则**（见上），
改动 FUSE 读路径或脱水路径时请务必附带对应矩阵的复跑结果。

## 许可证

本项目**原创代码**以 **MIT OR Apache-2.0** 双许可发布，任选其一：

- [`LICENSE-MIT`](LICENSE-MIT)
- [`LICENSE-APACHE`](LICENSE-APACHE)

**该许可只覆盖本仓库的原创部分。** QNAP、Qsync、myQNAPcloud、QID 等名称与标识是
QNAP Systems, Inc. 的商标或注册商标；被分析软件的全部权利归其及许可方所有 ——
详见 [`DISCLAIMER.md`](DISCLAIMER.md)。

第三方依赖各自遵循其自身许可（`cargo metadata` / `cargo deny` 可查）。
