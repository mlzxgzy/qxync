# M23：Dolphin 集成（`user.xdg.tags`）+ Baloo 门禁

> 日期：2026-10-20
> 前置调研：[调研-xattr标签与Dolphin状态列.md](调研-xattr标签与Dolphin状态列.md)（结论：可行，改动最小）
> 环境：CachyOS / KDE Plasma 6 / Dolphin 26.08.2 / KF6 6.31 / baloo 6.31

## 0. 一句话

把每个文件的水合状态（**仅在线 / 部分本地 / 已水合**）动态暴露成 freedesktop 标准的
`user.xdg.tags` xattr —— Dolphin **自带的 Tags 列**直接显示，不用写任何插件、不用改
Dolphin；开关默认关，打开前 daemon 强制检查**每个挂载点都已排除出 Baloo 索引**
（否则内容索引器会把整个 NAS 拉到本地），GUI 里一键弹终端自动执行排除命令。

## 1. 用户视角

### 1.1 开启（GUI）

设置 → 高级 → 「在 Dolphin 里显示水合状态（Tags 列）」：

* 勾选 → 保存 → daemon 预检 Baloo：
  * 所有挂载点都不会被索引 → 保存成功，kv 表显示每个挂载点的检测结论；
  * 有挂载点会被索引 → **弹确认框**（说清楚为什么：Baloo 内容索引 = 读文件 = 水合），
    OK = 「打开终端，自动添加排除」→ 弹出 konsole，`balooctl6 config add excludeFolders
    <挂载点>` 逐条自动执行，末尾列出排除清单，窗口留在原地（`--hold`）；
    用户跑完后回来再点一次保存（或先点「重新检测 Baloo」核对）。
* 取消勾选 → 保存 → tags 立刻从所有挂载点消失（热切换，不用重挂载）。

### 1.2 使用（Dolphin）

1. 打开挂载点，右键列头 → 勾 **Tags**（一次性，Dolphin 按目录记忆）；
2. Tags 列显示**图标字符**：`✓`（已水合）/ `☁`（仅在线）/ `◐`（部分本地），
   目录无标签 —— 用字符而不是「已水合」这种词，是为了能在列里扫一眼而不是读字；
3. 想要 CFA / Windows 资源管理器那种**文件图标上的彩色角标**（绿对钩 = 已水合、
   蓝云 = 仅在线、半满圆 = 部分本地），装 `contrib/dolphin/overlay` 的
   KOverlayIconPlugin（见 §8）——装上后不用勾 Tags 列也能看到状态；
4. F11 信息面板里也有同一行；
5. 状态变化（读了文件 / 脱水了）→ **F5 或重进目录**才刷新（FUSE 服务端改 xattr
   不触发 inotify，这是内核行为，调研文档 §5.1）。

### 1.3 CLI

```bash
qxync baloo-status                 # 检测：Baloo 装没装 / 索引开关 / 每个挂载点的结论 / 排除命令
qxync settings --set dolphin_tags=true   # 直连写文件（daemon 重启后生效；会打警告）
qxync settings                     # 回读（显示 Dolphin 集成开/关）
```

CLI 的 `settings --set` 是**直连写文件**（既有行为，所有设置键都这样），绕过 daemon
门禁 —— 所以它会打一条明确的警告（前置条件 + 排除命令）。真正经 daemon 的
`settings_save`（GUI 的路径）才被门禁拦。

## 2. 语义与不变量

### 2.1 开关的三层语义

| 层 | 位置 | 语义 |
|---|---|---|
| 设置 | `settings.json` 的 `dolphin_tags` | 用户意图（默认 false） |
| 门禁 | daemon `settings_save`（false→true 边沿） | 有挂载点会被 Baloo 索引 → **拒绝保存** |
| 生效 | 每个挂载点一个 `Arc<AtomicBool>` | `设置开了 && 该挂载点不会被索引` 才点亮 |

第三层是关键：**安全是按挂载点算的**。设置开了但某个挂载点没排除 → 那个挂载点的
tags 静默不暴露（挂载本身照常），其它已排除的挂载点不受牵连。

### 2.2 为什么门禁只在「关→开」边沿拦

已经开着的人保存别的设置（代理等）时，如果这时新挂了一个没排除的点，不该被一个
无关的挂载点卡死整个保存 —— 那个点的 tags 本来就不会被点亮（挂载时按安全条件算）。
新挂载点的风险由 `mount` 的**非阻塞警告**兜底（见 §2.4）。

### 2.3 Baloo 判定（`qxync-core::baloo`，纯函数可单测）

读 `~/.config/baloofilerc`：

* `[Basic Settings] Indexing-Enabled` —— **缺省视为 true**（本机实测：文件里没有
  这一行，`balooctl6 status` 却在跑）；
* `[General] exclude folders` —— 逗号分隔的排除列表；
* `[General] folders` —— include 列表（非空 = 只索引这些）。

判定链（`would_index`）：

```text
没装 balooctl（= Baloo 没装）        → 不会索引（安全）
索引全局关                          → 不会索引（安全，但 Tags 列也不显示）
路径或任一祖先在 exclude folders    → 不会索引（安全）
include 列表非空 → 与任一项有交集    → 会索引（危险）
include 空（默认索引整个家目录）→ 与家目录有交集 → 会索引（危险）
```

「有交集」取**双向祖先**：include 项在挂载点内部（如 include=/home/u/docs 而挂载
/home/u）同样会让子树被索引，不能漏。排除项在路径**内部**（排除 /m/sub 而问 /m）
不算排除 —— 根仍会被索引。

### 2.4 挂载时的非阻塞警告

`mount` 响应新增 `baloo_warning`（`Option<String>`）：

* 挂载点会被 Baloo 索引 → `Some("…建议排除：<命令>")`，**挂载照常成功**；
  CLI 原样打印，GUI 显示在挂载结果里；
* 这与 Dolphin 集成开关**无关** —— Baloo 的内容索引器自己就会读文件（= 水合），
  挂载点没排除就该提醒，哪怕用户从没开过 tags。

### 2.5 xattr 契约

| 名字 | 值 | 说明 |
|---|---|---|
| `user.xdg.tags` | `✓` / `☁` / `◐` / 空串 | **动态算的**，不落盘；目录 = 空串。字符与语义的映射见评论区（`✓`=hydrated、`☁`=placeholder、`◐`=partial） |

* 开关关 → `getxattr` 回 `ENODATA`、`listxattr` 名单不含它 —— Dolphin 的
  `queryAttributes` 当这个文件没有 Tags 属性，**M23 之前的行为一字不变**；
* 开关开 → 名单含它、值按 `user.qxync.state`（placeholder/partial/hydrated）映射；
* `setxattr` 不实现（有意）：用户在挂载点上加自己的标签会报「不支持」——
  挂载点上的标签是系统生成的只读状态，脱水后也不该跟着走。

## 3. 实现

| 层 | 文件 | 内容 |
|---|---|---|
| core | `src/baloo.rs`（新） | `BalooConfig::parse/load`、`is_excluded`、`would_index`、`find_balooctl`、`exclude_cmd`、`shell_quote` + 12 个单测（含 balooctl 实写格式 `[$e]`/尾斜杠、`config_path` 的 XDG 语义） |
| core | `src/settings.rs` | `Settings.dolphin_tags`（`#[serde(default)]`，旧文件零迁移）+ `set_kv("dolphin_tags")` |
| core | `src/ipc.rs` | `Request::BalooStatus`（`baloo_status`）、`BalooStatusData`/`BalooMountInfo`、`MountInfo.baloo_warning`（`skip_serializing_if=None`，老客户端无感） |
| fuse | `src/lib.rs` | `QxyncFs.dolphin_tags: Arc<AtomicBool>` + `with_dolphin_tags`；`getxattr`/`listxattr` 拆出可单测的 `xattr_value`/`xattr_names`；`user.xdg.tags` 条件暴露 |
| daemon | `src/daemon.rs` | `BalooProbe`（一次探测共用）；`settings_save` 门禁 + `refresh_dolphin_flags` 热刷新；`mount` 注入开关 + `baloo_warning`；`baloo_status_cmd`（同步模式 + 空转待命都答） |
| gui | `src/commands.rs` | `open_baloo_exclude`：konsole（`--hold -e bash -c`）→ xdg-terminal-exec → xterm 逐个试，`setsid` 脱离会话；找不到终端 → 返回可复制的命令文本 |
| gui | `ui/app.js` | 高级区开关 + `refreshBalooStatus`（kv 表）+ `doAdvancedSave` 预检 + `promptBalooExclude` 弹窗 + `openConfirm`（复用输入弹窗 DOM、藏输入框） |
| cli | `src/main.rs` | `baloo-status` 命令（人话 + `--json`）；`settings` 显示 Dolphin 行；直连 `--set dolphin_tags=true` 打警告；`mount` 打印 `baloo_warning` |
| e2e | `xtask/tests/fuse-matrix.sh` | M23 段：默认关 ENODATA/名单 → 门禁拒绝 → balooctl 排除后通过 → 三态值 → 目录空串 → 热切换消失（见 §5） |

## 4. 关键取舍

* **弹终端而不是 GUI 静默改 baloofilerc**：改的是用户桌面环境的配置，应该全程看得见；
  走 `balooctl` = 与 KDE 的读写路径完全一致；命令失败用户当场看到错误。
  `--hold` 让窗口留在原地，用户能核对 `excludeFolders:` 的最终列表。
* **门禁拒绝整个 settings_save**（而不是静默把 dolphin_tags 压回 false）：
  GUI 的流程是「预检 → 弹窗 → 排除 → 重试」，用户重试前其它设置本来就没保存过，
  不存在「卡住别的设置」的问题；静默改值会让用户以为开了其实没开。
* **`Indexing-Enabled` 缺省 = true**：宁可误报危险（多弹一次确认），不可漏报
  （静默让索引器把 NAS 拉下来）。
* **没装 Baloo → `can_enable=true`**：没有索引风险，开关可以开（xattr 照样暴露，
  `getfattr` 能看）；只是 Dolphin 的 Tags 列不显示 —— GUI 的 kv 表如实标注。

## 5. 验收

### 单测（全绿）

* `qxync-core::baloo`：12 例 —— 缺省视为开 / 只有字面 false 才关 / 排除与 include
  列表解析 / `~` 与 `$HOME` 展开 / **balooctl 实写格式（`[$e]` 标记 + 尾斜杠 +
  逗号多项）** / **`config_path` 的 XDG 语义（未设/绝对/相对回落）** / 排除覆盖
  自身与后代但不覆盖内部 / 双向祖先交集 / 家目录外默认不索引 / `find_balooctl`
  KF6 优先 / shell 引用 / 命令拼装；
* `qxync-fuse`：`xdg_tags_reflect_hydration_state_only_when_enabled` —— 开关关
  ENODATA+名单不含 → 开：三态逐一对上 + 目录空串 + 名单含 + 末尾 NUL → 再关：
  立刻消失（`Arc<AtomicBool>` 热切换）。

### 实测（本机，无 NAS 的空转/假连接 daemon）

* 空转待命：`baloo_status` 正常答（GUI 设置页在没配连接时也要能用）；
* 同步模式：经 socket 的 `settings_save`（GUI 路径）读-改-写成功、`dolphin_tags`
  热生效；`baloo-status --json` 结构与文档一致；
* 直连 `settings --set dolphin_tags=true`：警告正确打出。

### e2e（fuse-matrix M23 段，需真机 /dev/fuse + NAS）

1. 默认关：`getxattr` = ENODATA、`listxattr` 名单不含；
2. 门禁：装了 balooctl 且挂载点会被索引 → `settings_save`（经 daemon socket，
   GUI 的路径）被拒 → `balooctl config add excludeFolders` 后通过；
   （RUNDIR 在家目录外 / 没装 Baloo / 索引全局关 → 门禁不拦，脚本自动跳过该断言）
3. 三态：占位符 `仅在线` → `cat` 后 `已水合`；目录空串；名单包含；
4. 热切换：关回去 → ENODATA 立刻出现（无重挂载）；
5. 净退出：设置还原 + 门禁段加的 Baloo 排除项移除。

**真机验证（2026-10-20，CachyOS / baloo 6.31 / KDE，9/9 全绿）**：上述 1–5
全部通过。真机首跑暴露了两个实现 bug（当时单测/假连接实测都没覆盖到），
已修复并补了单测：

* **读侧不认 `XDG_CONFIG_HOME`**：balooctl 写配置走 KConfig 语义
  （`$XDG_CONFIG_HOME/baloofilerc` 优先），而 `config_path()` 写死了
  `$HOME/.config`。真实用户环境两者恰好一致（XDG 未设）所以没炸；
  fuse-matrix 为隔离凭据设了 `XDG_CONFIG_HOME` → 门禁永远看不到
  balooctl 刚写的排除项。修复：`config_path` 对齐 KConfig 语义（拆出
  可单测的 `config_path_with`）。
* **不认 balooctl 实写的键格式**：balooctl 写的是
  `exclude folders[$e]=$HOME/项目/.../mnt-m23/` —— 键带 KConfig 扩展标记
  `[$e]`、值是 `$HOME` 变量形式 + 尾斜杠。解析器只精确匹配
  `exclude folders` → 一个条目都读不到。修复：键匹配前剥 `[...]` 后缀
  （`$HOME` 展开和尾斜杠的组件比较原本就有，恰好兜住）。

顺带修了 fuse-matrix 自身的两个测试缺陷（见 §5.1）。

### 手工（Dolphin，真机）

1. 排除挂载点 → 开开关 → Dolphin 右键列头勾 Tags → 三态可见；
2. F5 / 重进目录刷新语义符合预期；
3. 挂载点上加标签 → 报「不支持」（`setxattr` 未实现，预期行为）；
4. `tags://` 虚拟目录搜不到这些标签（走 Baloo 索引库，预期行为）。

### 5.1 真机验证顺带修掉的 fuse-matrix 测试缺陷

* **M23 段不重置起始状态**：门禁只在「关→开」边沿拦（产品语义，见
  `settings_save_cmd` 注释）。上一轮跑挂在中途 / 外部手动验证把
  `dolphin_tags=true` 留在 settings.json 里，下一轮的门禁断言就被边沿
  条件静默吞掉（实测：手动验证 save true 后没还原 → 下一轮「门禁未拦」
  + M23-3 全红，且日志线索极具误导性）。修复：M23 段开跑先
  `save_dolphin false` 强制回到已知状态。
* **缓存清理不认 host 子目录**（M9 布局）：`rm -f "$CACHE"/*` 删不掉
  `qnap.kdajv.com/` 目录 → 残留缓存让「读元数据不触发下载」「#11 不吐
  假数据」「稀疏缓存 apparent size」三个断言假失败（cat 命中上轮已下载
  的区间，EIO 前吐了 128 KiB 真数据）。修复：`rm -rf` + 稀疏缓存断言
  改用 `find` 拿完整路径（`ls -S` 对子目录布局输出 basename，stat 找
  不到文件）。

基线对照（`dbc02cb`，M23 之前的代码）确认：fuse-matrix 剩余失败
（M2b 改名三项、M2c 删除熔断/冲突副本等）在基线上同样失败 —— 是
WAN 环境下 NAS 端 rename/删除判定的固有问题，与 M23 无关。

## 6. 已知限制（调研文档 §5 全部继承）

* **不实时刷新**：F5 / 重进目录才更新（FUSE 服务端改 xattr 不触发 inotify）；
* **依赖 Baloo 全局开关**：用户 `balooctl6 disable` → Tags 列整个消失（KDE 设计）；
* **进不了 `tags:` 虚拟目录 / Baloo 标签搜索**（那走索引库）；
* **标签是纯文本**：没有颜色、图标、排序权重；
* **多连接 fanout**：GUI 保存设置广播到所有 daemon，预检只查当前连接 ——
  其它连接的挂载点若未排除，那台 daemon 的门禁会拒（fanout 结果里如实显示失败）。

## 8. 角标插件（KOverlayIconPlugin，2026-10-20 追加）

### 8.1 为什么又做插件

Tags 列**只能显示文字/字符**：字号跟正文一样、没有颜色。真机上「已水合」这行字
在满屏文件名里并不显眼（用户实测反馈：「看也看不太出来」）。要 CFA（Windows
Cloud Files API）/ 资源管理器那种**文件图标上的彩色角标**，Dolphin 唯一开放的
机制是 `KOverlayIconPlugin`：

* Dolphin 源码 `src/kitemviews/kfileitemmodelrolesupdater.cpp` 从
  **`<Qt 插件目录>/kf6/overlayicon`** 加载插件，`getOverlays(QUrl)` 返回图标名，
  `src/kitemviews/kstandarditemlistwidget.cpp` 的 `drawOverlays()` 用
  `QIcon::fromTheme(name)` 画在文件图标左下/右上/右下（最多 3 个，左上角留给
  VCS 插件）；
* 调研文档 §2.2.4 当时以「角标没有意义」为由排除 —— 那是针对「右键菜单看到状态」
  的方案；现在需求变成「图标化显示」，它正是唯一可行解。

### 8.2 实现

`contrib/dolphin/overlay/`（独立 CMake 工程，不参与 Rust workspace，不进 cargo build）：

| 文件 | 内容 |
|---|---|
| `qxync_overlay.cpp` | `KOverlayIconPlugin` 实现：`/proc/self/mounts` 找 `fuse.qxync` 挂载点（2 秒缓存）→ 只对挂载点内文件读 xattr → 返回图标名 |
| `qxync_overlay.json` | 插件元数据（Id / Name / Version） |
| `CMakeLists.txt` | Qt6 Core + KF6 KIO，装到 `lib/qt6/plugins/kf6/overlayicon` + hicolor 图标目录 |
| `icons/qxync-*.svg` | 三个 16px 网格图标：绿圆白对钩 / 蓝云 / 半满圆 |
| `README.md` | 构建、系统级/用户级安装、重启 Dolphin、排查、限制、卸载 |

判定顺序（`getOverlays`）：

```text
URL 不是 file://                              → {}
不在 fuse.qxync 挂载点下                      → {}   （一次 xattr 都不读）
user.xdg.tags 不存在（= 开关关）或为空串（目录）→ {}
user.qxync.state = hydrated                   → qxync-hydrated
                  partial                     → qxync-partial
                  placeholder                 → qxync-placeholder
```

**为什么读 `user.qxync.state` 而不是认 tags 字符**：语义与展示解耦 —— 以后改
Tags 列的字面（或做本地化）不会让角标跟着坏；开关仍由 `user.xdg.tags` 是否
存在表达（和 FUSE 同源，关掉开关角标立刻消失，不需要重装插件）。

### 8.3 实测（本机，Dolphin 26.08.2 / KF6 6.31 / Wayland）

* 插件被 Dolphin 进程实际加载，`getOverlays` 对打开目录里的两个文件各调用多次
  （roles updater 会重解析），返回 `qxync-hydrated`；
* **截图可见**：两个已水合文件左下角绿色对钩，目录与普通文件无角标；
* 非挂载点路径（`/etc/hostname`）不读 xattr、直接返回空（日志验证）；
* 图标在 8 / 16 / 22 / 32 / 64 px 下都渲染正常（`QIcon::fromTheme` 逐尺寸渲染检查）；
* 开关关（`user.xdg.tags` ENODATA）→ 角标消失（插件只读、无状态）。

### 8.4 安装与限制

* 系统级：`sudo cmake --install build --prefix /usr` —— Dolphin 直接加载，**不需要
  任何环境变量**（推荐，后续可进发行包）；
* 用户级：装 `~/.local` 后要给 Dolphin 设 `QT_PLUGIN_PATH`（Qt 默认不搜
  `~/.local/lib/qt6/plugins`，实测 `KPluginMetaData::findPlugins` 只认
  `QCoreApplication::libraryPaths()`）；
* Dolphin **在创建视图时**加载插件：已经开着的窗口不会热加载，重启 Dolphin 即可；
* 详情视图默认 16px 图标 → 角标只有 8px（能认，但小）；把图标尺寸调到 22/32px
  会得到 16/22px 角标；
* 只支持本地 `file://`、KF6 Dolphin；最多 3 个角标；状态仍不实时推送（F5 刷新）。

## 7. 与既有行为的关系

* 开关默认关 → **M23 之前的行为一字不变**（xattr 名单、`getfattr`、Dolphin 全部照旧）；
* `settings.json` 加字段是 `#[serde(default)]` → 旧文件零迁移；
* `MountInfo.baloo_warning` 是 `Option` + `skip_serializing_if` → 老 CLI/GUI 反序列化
  不受影响（未知字段忽略）；
* `IPC_VERSION` 不动：`baloo_status` 是新方法名，老 daemon 对未知方法本来就会回
  可读错误，GUI 的预检失败时**不拦保存**（让 daemon 侧门禁自己说话）。
