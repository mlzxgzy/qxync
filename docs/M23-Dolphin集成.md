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
2. Tags 列显示：`仅在线` / `部分本地` / `已水合`；目录无标签；
3. F11 信息面板里也有同一行；
4. 状态变化（读了文件 / 脱水了）→ **F5 或重进目录**才刷新（FUSE 服务端改 xattr
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
| `user.xdg.tags` | `仅在线` / `部分本地` / `已水合` / 空串 | **动态算的**，不落盘；目录 = 空串 |

* 开关关 → `getxattr` 回 `ENODATA`、`listxattr` 名单不含它 —— Dolphin 的
  `queryAttributes` 当这个文件没有 Tags 属性，**M23 之前的行为一字不变**；
* 开关开 → 名单含它、值按 `user.qxync.state`（placeholder/partial/hydrated）映射；
* `setxattr` 不实现（有意）：用户在挂载点上加自己的标签会报「不支持」——
  挂载点上的标签是系统生成的只读状态，脱水后也不该跟着走。

## 3. 实现

| 层 | 文件 | 内容 |
|---|---|---|
| core | `src/baloo.rs`（新） | `BalooConfig::parse/load`、`is_excluded`、`would_index`、`find_balooctl`、`exclude_cmd`、`shell_quote` + 10 个单测 |
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

* `qxync-core::baloo`：10 例 —— 缺省视为开 / 只有字面 false 才关 / 排除与 include
  列表解析 / `~` 与 `$HOME` 展开 / 排除覆盖自身与后代但不覆盖内部 / 双向祖先交集 /
  家目录外默认不索引 / `find_balooctl` KF6 优先 / shell 引用 / 命令拼装；
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

### 手工（Dolphin，真机）

1. 排除挂载点 → 开开关 → Dolphin 右键列头勾 Tags → 三态可见；
2. F5 / 重进目录刷新语义符合预期；
3. 挂载点上加标签 → 报「不支持」（`setxattr` 未实现，预期行为）；
4. `tags://` 虚拟目录搜不到这些标签（走 Baloo 索引库，预期行为）。

## 6. 已知限制（调研文档 §5 全部继承）

* **不实时刷新**：F5 / 重进目录才更新（FUSE 服务端改 xattr 不触发 inotify）；
* **依赖 Baloo 全局开关**：用户 `balooctl6 disable` → Tags 列整个消失（KDE 设计）；
* **进不了 `tags:` 虚拟目录 / Baloo 标签搜索**（那走索引库）；
* **标签是纯文本**：没有颜色、图标、排序权重；
* **多连接 fanout**：GUI 保存设置广播到所有 daemon，预检只查当前连接 ——
  其它连接的挂载点若未排除，那台 daemon 的门禁会拒（fanout 结果里如实显示失败）。

## 7. 与既有行为的关系

* 开关默认关 → **M23 之前的行为一字不变**（xattr 名单、`getfattr`、Dolphin 全部照旧）；
* `settings.json` 加字段是 `#[serde(default)]` → 旧文件零迁移；
* `MountInfo.baloo_warning` 是 `Option` + `skip_serializing_if` → 老 CLI/GUI 反序列化
  不受影响（未知字段忽略）；
* `IPC_VERSION` 不动：`baloo_status` 是新方法名，老 daemon 对未知方法本来就会回
  可读错误，GUI 的预检失败时**不拦保存**（让 daemon 侧门禁自己说话）。
