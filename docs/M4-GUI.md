# M4 —— GUI（Tauri 2 桌面应用）

> 状态：**已实现并真机验收**（M4：`gui-matrix.sh` 41/41）。
> ★ **M8.1 已改造界面结构**（顶部 tab → 左侧图标栏 + 页面），验收矩阵扩到 **86/86**；
> ★ **M8.4 补齐设置中心（8 个分区）、文件页三态与右键菜单、冲突策略与待裁决队列、托盘/通知/选择器**，
> 见 §4.1、§4.3 与 [`M8-向Qsync-Client-6靠拢.md`](M8-向Qsync-Client-6靠拢.md)。
> 里程碑定位见 [`开发规划.md`](开发规划.md) §2：GUI 在双向同步（M2b/M2c）与脱水（M3）可用后进场，
> 首版只做四件事——**登录/连接配置、挂载管理、状态与进度面板、pin 管理**。

---

## 1. 边界：GUI 是 daemon 的图形客户端，不是第二个同步引擎

```
crates/qxync-gui (Tauri 2)
├── ui/                  零依赖静态前端（index.html / app.js / style.css，无 npm、无打包器）
└── src/
    ├── main.rs          入口：窗口模式 / --self-test / --self-test-login
    ├── lib.rs           tauri::Builder + invoke_handler + 自检
    ├── commands.rs      ★ 全部 Tauri 命令（前端的唯一入口）
    └── ipc.rs           unix socket + 一行一个 JSON → qxyncd
                                        │
                              qxyncd（唯一持有 NAS 会话与 FUSE 的进程）
```

* GUI **不发任何 HTTP 到 NAS**，也**不自己挂 FUSE**。所有动作都是 `Request`
  （契约：`qxync-core/src/ipc.rs`，说明：`M1.5-设计.md`），与 CLI `qxync` 走同一条线。
* 依赖方向仍然是「只允许向下」：`gui → core`（+ 经 IPC 访问 daemon），
  没有引入 `gui → client/fuse` 的边。

## 2. 为什么前端不用 npm / 打包器

Tauri 2 的 `withGlobalTauri: true` 会把 `window.__TAURI__.core.invoke` 直接挂到全局，
所以 `ui/` 三个静态文件就够了：`tauri.conf.json` 里 `build.frontendDist = "ui"`，
编译期由 `tauri-codegen` 嵌进二进制（`cargo build` 即可，`tauri dev` 不是必须的）。

好处：没有 `node_modules`、没有 Vite 构建步骤、没有版本漂移；代价是没有组件框架，
所以 `app.js` 用最朴素的 DOM API（也顺手避开了「把文件名拼进 innerHTML」这类注入问题）。

> 所有来自 daemon 的字符串（文件名/路径/错误）一律走 `textContent` / `createElement`。

## 3. 命令面（前端唯一入口）

| 命令 | 作用 |
|---|---|
| `app_info()` | 版本、HOME、socket、三个 XDG 目录、daemon 二进制、前端资源大小、初始 tab |
| `daemon_status()` | `ping` + `status` 快照（前端每 2s 轮询一次） |
| `ipc_call(req)` | **通用 IPC**：入参 `{"v":1,"method":"ls","path":"/home"}`，永远返回响应信封 `{v,ok,data,error}` |
| `link_read(linkId?)` | 读 `links/<id>.json`（不含口令） |
| `link_save(input)` | 写 `links/<id>.json` |
| `credential_save(input)` | 写 `credentials.json`（0600，临时文件 + rename） |
| `credential_present()` | 只回「有没有 + host/user」，**绝不回口令** |
| `daemon_start(linkId?)` | 找到 `qxyncd` 并拉起，等 socket 就绪后 ping |
| `daemon_stop()` | `shutdown` + 等 socket 消失 |
| `login_flow(input)` | 一条龙：写 link → 写凭据 → 连接参数变了就重启 daemon → IPC 登录 |

`ipc_call` 覆盖 `ping/status/logout/ls/stat/get/put/mkdir/pin/mount/umount/mounts/sync/rm/dehydrate/shutdown`，
所以 M2c/M3 的能力（立即同步、放行删除、脱水、限额）不需要为 GUI 再加命令。

### 3.1 两个容易踩的接口细节

1. **Tauri 命令的参数名是 camelCase**（Rust 侧 `link_id` → JS 传 `{ linkId }`），
   但**对象内部字段保持 serde 的 snake_case 原样**（`ipv4_only` / `read_write` /
   `force_deletes` / `hydrate_timeout_secs` / `cache_mode`…）。
2. `login_flow` 会**重启 daemon**：`qxyncd` 只在启动时读 `links/<id>.json`，
   所以「换 NAS / 换账号」必须重启；只有连接参数（host/port/https/user/ipv4_only）没变才直接登录。

## 4. 界面

> ★ **M8.1 起，界面从「顶部 5 个 tab」改成「左侧图标栏 + 页面」**（对齐 Qsync Client 6 的信息架构，
> 见 [`M8-向Qsync-Client-6靠拢.md`](M8-向Qsync-Client-6靠拢.md)）。下面 §4.1 是新结构，§4.2 保留
> 各页原来的字段说明（**元素 id 与字段全部未变**，只是搬了位置）。

### 4.1 结构（M8.1）

```
┌────┬──────────────────────────────────────────────────────────────┐
│ Q  │  页面标题        [daemon][连接][登录]      启动/停止/立即登录   │
│    │  daemon: pid … · uptime … · socket …                          │
│ 主页│                                                              │
│ 任务│   内容区（页面容器）                                          │
│ 文件│                                                              │
│ 更新│                                                              │
│ 错误│                                                              │
│ 设置│                                                              │
│ ── │                                                              │
│ 诊断│                                                              │
├────┴──────────────────────────────────────────────────────────────┤
│ 操作日志（每次 invoke 一行，保留 200 条，可收起/清空）              │
└───────────────────────────────────────────────────────────────────┘
```

| 目的地 | 内容 | 状态 |
|---|---|---|
| **主页**（home） | 连接行（`user@host:port · 已连接 · Qsync 版本`）+ **任务卡片列表**（M8.2 起**优先按任务登记展示**，没有任务登记时退回挂载点）+ 最近一次同步摘要 + 快捷动作（连接设置 / 立即同步 / ＋添加任务） | ✅ M8.1/M8.2 |
| **任务**（tasks） | ★ M8.2 已实现：同步任务列表（每张卡：状态 / 本地路径 ⇄ NAS 路径 / 只读·缓存模式·**冲突策略** / `Sync`+已挂载徽章 + 设置·管理·挂载·暂停·继续·**打开目录**·删除登记·立即同步）+ **文件夹对设置**表单（保存到 `~/.config/qxync/tasks/<id>.json`，★ M8.4 起含**冲突策略下拉（5 选项）**/ 同步方向 / 节省空间模式 / 智能删除；★ 2026-10-02 起 **NAS 文件夹是一对一下拉**——候选来自 NAS 上登记的 Qsync 同步文件夹 + 已配置的 NAS 目录，另有「浏览…」逐层挑与手动输入，提交时检查目的地冲突）+ **冲突待裁决卡片**（「每个文件都问我」的策略下逐条裁决） | ✅ M8.2 / M8.4 |
| **文件**（files） | 远端目录浏览 + pin；★ M8.4：**三态列**（仅在线 / 本地可用 / 始终可用，来自 `file_states`）+ **行右键菜单**（始终保留在此设备 / 取消固定 / 释放空间 / 下载 / 复制路径 / 删除） | ✅ M8.4 |
| **更新**（journal） | ★ M8.3 已实现：同步日志表格（时间 / 活动 / 路径 / 说明 / 字节）+ 按文件名·说明搜索 + `全部/成功/失败/被挡下` 过滤 + 条数 + 清空 | ✅ M8.3 |
| **错误**（errors） | ★ M8.3 已实现：`journal` 里 `status='error'` 的失败项，每条可「复制路径」 | ✅ M8.3 |
| **设置**（settings） | ★ M8.4 已补齐 **8 个分区**：连接 / 代理 / 同步与筛选 / 个人 / 高级 / 释放空间 / LAN 加速 / 关于（每个分区可用 `QXNYC_GUI_TAB=settings:<分区>` 直达） | ✅ M8.4 |
| **诊断**（diag） | **专家模式**：原「状态 / 进度」「挂载」「同步 / 缓存」三个 tab 收纳为**子 tab**，字段与 id 全保留 | ✅ |

**`QXNYC_GUI_TAB` 取值**（验收矩阵与排障用）：
新值 `home|tasks|files|journal|errors|settings|diag`，并支持 **`diag:<status|mounts|sync>`** 与
**`settings:<connect|proxy|sync|personal|advanced|free|lan|about>`**（M8.4）直达子页/分区；
**旧值 `status|mounts|sync|connect|files` 必须继续可用**，分别落到 `diag:status`/`diag:mounts`/`diag:sync`/`settings`/`files`
（`gui-matrix.sh` §3b 用截图 AE 断言「落点等价」）。

底部固定「操作日志」面板：每次 invoke 记一行 `HH:MM:SS 命令 参数 → ok/error`，保留 200 条
（排障 + 验收时肉眼可见）。

### 4.1.1 任务卡的「打开目录」（用系统默认目录工具）

主页与任务页的每张任务卡上都有一个「**打开目录**」按钮，点一下就用**系统里用户自己配的
默认目录工具**（KDE Dolphin / GNOME Files / Thunar…）打开该任务的**本地挂载点**。

**为什么不用 opener 插件的默认行为**：`tauri-plugin-opener` 底层是
`open::that_detached`，在 Unix 上是 `xdg-open` → `gio open` → `gnome-open` → `kde-open`
挨个试。而 `xdg-open` 把「目录」当成一种 mime 类型去查默认处理程序 —— 不少桌面
**没有为 `inode/directory` 登记** Desktop Entry，于是它解析出空、**返回成功却一个窗口
都不弹**，也不会去试后面的 `gio`。表现就是「点了没反应」。

**因此后端 `open_path` 对目录走这条链**（`crates/qxync-gui/src/commands.rs`）：

1. `xdg-mime query default inode/directory`（`gio mime` 兜底）→ 拿到**桌面项 ID**；
2. 找该 `.desktop`（`XDG_DATA_HOME` 的 `applications/` 优先，再依次查 `XDG_DATA_DIRS`），
   读 `[Desktop Entry]` 组的 `Exec=` → 按 XDG 规范解析出**真正的程序名**；
3. 确认程序在 `PATH` 里，`setsid` 脱离后拉起，把挂载点路径作为**唯一**参数交给它。

⚠️ 第 1 步返回的是**桌面项 ID**（`org.kde.dolphin.desktop`），**不是可执行文件名** ——
真程序是 `Exec=` 里写的 `dolphin`。直接 spawn ID 必然失败（开发机实测 **127**）。
所以第 2 步不能省。

**行为约定**（都是有意这样，不是没做）：

| 情况 | 表现 |
|---|---|
| 查到默认程序且拉起成功 | 打开该目录；返回值带 `via=xdg-mime\|gio` + `program`（真正执行的程序名） |
| 没配默认目录工具 / 程序不在 `PATH` / 起不来 | 回落到插件 opener，并在返回值里如实标 `via=plugin` / `plugin-fallback` + 原因 |
| 任务没有本地挂载点 | 按钮**禁用**并说明原因，不做「点了没反应」 |
| 挂载点不存在（NAS 掉线、停用后自动卸载） | **如实报错**；**绝不 `mkdir` 造空目录** —— 造出来会让用户以为文件真在本地，正好掩盖最该看见的故障 |
| `setsid()` 失败（进程已是进程组组长，`EPERM`） | **仍照常拉起**（退化成与 GUI 同会话，与 opener 插件一致）；「脱离会话」只是锦上添花，不该决定功能可用性 |

**开的是本地那一侧**：NAS 侧在网络上，没有可交给本地目录工具的路径。

### 4.2 各页字段（元素 id 与字段名全部沿用 M4，未改动）

| 原 tab（现位置） | 内容 |
|---|---|
| **状态 / 进度**（诊断 → 状态 / 进度） | 服务端信息（Qsync 版本/QPKG/build/busy_reason）、会话、三游标、水合统计、上传队列（pending/active/done/failed/retries/bytes）、缓存限额进度条、`blocked_*` 分类、挂载列表、**NAS 同步文件夹**面板、最近一轮同步摘要 |
| **连接 / 登录**（设置） | host/port/https/insecure/user/password/ipv4_only 表单（打开时预填）；保存配置 / **保存并登录** / 启停 daemon；三个 XDG 目录与 socket 路径 |
| **挂载**（诊断 → 挂载） | 当前挂载表（可卸载）+ 新建挂载（挂载点默认 `$HOME/qxync-mnt`、**一个** NAS 目录、读写开关、`cache_mode`、线程数、水合超时、删除熔断阈值、auto_unmount） |
| **文件 / pin**（文件） | 远端目录浏览（真机 `ls`，目录优先）、每行 pin 查询/设置（`unspecified/pinned/unpinned/excluded`）、下载（`get`）、脱水（`dehydrate`，**恒带 `force: true`**）、新建目录、删除 |
| **同步 / 缓存**（诊断 → 同步 / 缓存） | `SyncInfo` 全量（含 `devices`、`last_error`、`delete_block_reason`）+ 立即同步 / 强制放行删除 / 暂停轮询 / 设间隔；`CacheInfo` 全量 + 脱水预演 / 全部脱水（`force: false`）/ 按限额 / 释放闲置 |

> ★ **两个脱水入口的 `force` 与呈现不同，是有意为之**：
> 单行「脱水」= 用户点名释放这一个路径，恒带 `force: true`（跳过 300s「刚访问过」，
> 否则「刚读完就点释放」永远脱不掉）；「全部脱水」是批量操作，`force: false` 保留保护
> 窗口（刚读过的文件正是最该留在本地的）。两者都**必须**把返回体的 `blocked` 原因
> 显示出来 —— 只打「0 项，释放 0 B」等于静默失败，详见 [`M3-脱水.md`](M3-脱水.md) §4/§6。


## 5. 自检与验收

```bash
# 无窗口自检（要在 daemon 跑着的时候）：前端资源嵌入 + status/ls/mounts/sync 四条数据源
cargo run -p qxync-gui -- --self-test

# 登录链自检：daemon_stop → login_flow（写 link/凭据 + 拉起 daemon + 登录）→ 复核
cargo run -p qxync-gui -- --self-test-login

### 4.3 设置中心与桌面集成（M8.4）

**设置页的 8 个分区**（对应 Qsync 的四个 tab + qxync 自己的连接/筛选/LAN/关于）：

| 分区 | 数据源 | 能做什么 |
|---|---|---|
| 连接 | `link_read` / `link_save` / `credential_*` / `app_info` | host/port/https/insecure/ipv4_only…；保存并登录；启停 daemon |
| 代理 | `settings` / `settings_save` | `No proxy` / `Auto-detect` / `Manual`（+认证）；显示实际环境变量与解析出的代理 URL |
| 同步与筛选 | `link_read` / `link_save` / `rules` | 编辑 `exclude`（每行一条）+ `filter_temp`；`--match` 实时预览；显示每个任务的冲突策略 |
| 个人 | `settings` | 开机自启（写 XDG autostart 桌面项）/ 语言 / 地区 / 关闭进托盘 |
| 高级 | `settings` | 调试日志位 / 桌面通知开关 + 测试通知；「明确不做」的三个按钮点了给原因 |
| 释放空间 | `settings` / `space` | `Free up space automatically`（当空间少于 X% / 按频率）+ 立即释放空间 + 用量条 + 被挡下列表 |
| LAN 加速 | `peer` / `link_save` | 监听地址 / 设备名 / 配对 / 已配对设备 / 最近事件 |
| 关于 | `app_info` / `m84_info` | 版本与路径、打开日志/配置目录、File Station 深链、托盘与插件状态、明确不做的清单 |

**桌面集成**（Rust 侧命令，前端一律经 `window.__TAURI__.core.invoke`）：

| 命令 | 用途 |
|---|---|
| `m84_info` | 托盘是否建起来 / 插件清单 / autostart 路径与存在性 / `close_to_tray`（**只读**） |
| `notify_show(title, body)` | 发桌面通知；设置里关了就回 `shown:false`（**不假装发**） |
| `pick_folder` / `pick_file` | 文件选择器（回调式 API + oneshot，避免与 GTK 主循环互锁） |
| `open_path` / `open_url` | 打开目录 / URL（`tauri-plugin-opener`） |
| `window_show` / `window_hide` / `app_quit` / `app_exe_path` / `tray_emit` | 托盘动作与自启路径 |

**托盘**：ksni（纯 Rust StatusNotifierItem）→ D-Bus 名字 `org.kde.StatusNotifierItem-<pid>-1`，
菜单 4 项（打开主窗口 / 立即与 NAS 同步 / 暂停 / 退出）；前三项 emit `tray://action`，动作由前端执行。

★ **「建起来」≠「有人画」**：创建后会探测
① watcher 在不在 ② `IsStatusNotifierHostRegistered` ③ 本进程 item 是否已在
`RegisteredStatusNotifierItems` 里（最多 8×250ms 重试），三条都过才算**托盘可见**，
结果在 `m84_info` 的 `tray_visible` / `tray_reason` 与「设置 → 关于」里如实显示。
关闭主窗口按 `close_to_tray` 隐藏进托盘 —— 但**托盘不可见时照常关闭**，
否则窗口会藏进一个没人渲染的托盘、用户再也找不回来。
XEmbed-only 桌面（IceWM/Fluxbox 等）与裸 GNOME 属于「不可见」，可装 `snixembed` 桥接。

# 完整验收矩阵（M4 自检/登录链 + M8.1 的 9 个目的地 + 旧 tab 值落点等价性 + M8.4 的 7 个设置分区）
xtask/tests/gui-matrix.sh              # 自检 + 登录链 + 9 个目的地真窗口截图
xtask/tests/gui-matrix.sh --no-window  # 无 DISPLAY 的机器只跑自检
xtask/tests/gui-matrix.sh --keep-open  # 结束时保留窗口，手动玩
```

矩阵覆盖（86 项）：

1. 三个二进制 + 连接配置前置检查；
2. `--self-test`：资源嵌入、`status_ok`、`logged_in`、真机 `ls /home`、`mounts`、`sync`，
   以及**负向对照**——daemon 停掉后 `--self-test` 必须非 0 退出（防止自检变成橡皮图章）；
3. `--self-test-login`：GUI 自己的「保存并登录」链路真的能把 daemon 拉起来并登录；
4. 真窗口：9 个目的地各起一次，窗口标题正确、截图非空白（stddev > 1500）、
   与主页画面确有差异（AE > 4000 px）；
5. ★ M8.1：旧 `QXNYC_GUI_TAB` 的 5 个取值各起一次，用截图 AE 断言**落点等价**
   （status→diag:status、mounts→diag:mounts、sync→diag:sync、connect→settings、files→files；
   实测 AE ≈ 2000 px，只差时钟/uptime）。

> 截图落在 `$QXNYC_TEST_RUNDIR/gui-shots/`（默认 `.local-run/gui-shots/`，已 gitignore），
> 里面还有 `self-test.json` / `self-test-login.json` 两份原始自检输出。

已知环境限制：矩阵里窗口/截图项会 `export GDK_BACKEND=x11`——`xdotool`/`import`
看不见原生 Wayland 窗口。**只影响验收脚本**，用户正常启动仍走 Wayland。

> ★ **M8.6（打磨与收口）在本文基础上又加了两段**：**2c** —— `ui_spec` 静态合规性
> （随 `qxync-gui --self-test` 一起产出，扫的是**编译期嵌进二进制的那份 UI**：无 `innerHTML`
> 类拼接、文案表键齐全、焦点环 / `aria-*` / 深色 token / reduced-motion、四态入口与标记）
> 外加一条**文案表运行时回落**断言（预留的 `en` 空表必须回落到 zh-CN，未知 key 原样返回）；
> **3c** —— 真窗口按一次 Tab，断言「跳到主内容」skip-link 与焦点环真的显形（AE > 100 px）。
> 矩阵合计 **148 项**。同时 §6 的踩坑 #1（`[hidden]` 压不住 `display`）与 #2（首轮 status 未到
> 就渲染空态）在 M8 新页面上**复查过一遍**：现在统一走 `setListState()`
> （loading / error / empty / ready + `data-state` 标记），不再把「还没加载完」写成
> 「不可用（daemon 未运行？）」。详见 [`M8-向Qsync-Client-6靠拢.md`](M8-向Qsync-Client-6靠拢.md) §M8.6。

## 6. 实现时踩到的坑（都是实测）

1. ★ **`hidden` 属性压不住作者样式里的 `display`**：`.env-banner { display: flex }` 让
   `<div hidden>` 在 Tauri 里**永远显示**，表现是「明明 IPC 都通，却顶着一条
   『未在 Tauri 中运行』的红条」。修法是全局 `[hidden] { display: none !important; }`。
   （同类问题也适用于 loading / 空态 / 结果框这些用 `hidden` 切换的元素。）
2. **首轮 `status` 到位前切 tab 会渲染空态**：`requireLogin()` 依赖 `state.lastStatus`，
   页面刚起来时它还是 `null`，于是「文件 / pin」页的 `ls` 直接不发了（表现为「共 0 项」，
   但操作日志里连一条 `ipc_call ls` 都没有）。修法：首轮 status 回来后补一次当前 tab 的刷新。
3. **Tauri 2 自定义命令不受插件权限系统管辖**，但仍需要 `capabilities/default.json`
   放行 `core:default`（窗口/事件）；`withGlobalTauri` 与它无关。
4. `frontendDist` 指向的目录在**编译期**被嵌入：改 `ui/` 必须重新 `cargo build`，
   否则 `--self-test` 里 `ui_assets` 的大小还是旧值（这也是判断资源是否真的进包的手段）。
5. `login_flow` 里「旧 daemon 让位」要**等 socket 真的消失**再拉新的，否则会撞上
   `daemon 已在运行（socket …）`。

## 7. 已知限制（首版故意不做）

* **没有 GUI 侧挂载点选择对话框**（需要 `tauri-plugin-dialog`）——表单里直接填路径，
  默认给 `$HOME/qxync-mnt`。
* `pin` 沿用 daemon 现状：**内存态**（M1.5 起就只登记，重启丢），GUI 只是它的可视化。
* ~~文件表格在窄窗口下横向滚动~~ **已修（2026-10-04）**：此前不是「窄窗口才滚」，而是
  默认 1200px 窗口下总宽就超出容器，且溢出部分**被卡片静默裁掉**（连滚动条都没有），
  最右侧的 pin 操作列整列看不见。现改为 `table-layout: fixed` + 固定列宽 + 文件名列
  弹性，实测最小窗口 960px 下横向溢出为 0；再窄才由 `.tbl-scroll` 退化为滚动条。
  行内 pin 的「当前值」仍需手动查一次才知道（显示「（未知）」，查完回填到该行下拉）。
* 不做托盘图标、开机自启、通知；不做多 link 切换（`daemon_start` 已支持传 `linkId`，界面暂只发默认 link）。
* 下载/上传仍是「弹窗填本地路径」，没有文件选择器（同上，等引入 dialog 插件）。

### 7.1 文件页表格的列（2026-10-04 调整）

`buildFileRow` 原来在**每一行**渲染 `<select>` + 4 个按钮（查 pin / 设 pin / 下载 / 脱水），
单列约 370px —— 正是它把整张表撑出容器。而这四个按钮**在行右键菜单里都已有等价项**，
属纯冗余。现在收成「窄下拉 + ⋯ 按钮」，操作全部收进菜单（并补上原本没有的三项：
查询 pin 状态 / 设为 unpinned / 排除）。列构成：

| 列 | 宽 | 说明 |
|---|---|---|
| 选 | 34px | checkbox |
| 类型 | 46px | 📁 / 📄；`have_child` 已并入（目录后加 ▾），不再单列 |
| 文件名 | 弹性 | 单行省略，悬停 `title` 给全名 |
| 大小 | 84px | 右对齐 |
| 修改时间 | 128px | 等宽字体 |
| 状态 | 156px | 空间三态 + 同步维度徽章，放不下时换行 |
| pin / 操作 | 150px | 窄下拉 + ⋯ 按钮 |

`openRowMenu` 新增第 4 参 `selectEl`，把该行的下拉带进 `state.rowMenu`；
菜单里改完 pin 会回填它 —— 否则会出现「设了 pinned、下拉还显示（未知）」。

**给后续接行内操作的人**：往表格里加按钮前先问一句「右键菜单里有没有等价项」。
有就收进菜单；确实要留在行内，就用窄下拉 + 单个 ⋯ 按钮，别再堆一排。
新增按钮务必同步 `i18n.js`（`cargo test -p qxync-gui` 的 `ui_spec_is_green` 会拦）。
