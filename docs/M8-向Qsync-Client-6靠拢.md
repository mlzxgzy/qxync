# M8 —— 界面与功能向 Qsync Client 6 靠拢（研究与执行方案）

> 状态：**方案已全部落地**（M8.0–M8.4 + **M8.6 打磨与收口** 已完成；M8.5 备份任务决策不做）。
> 本文保留原始研究与决策记录，逐条实测结论见各里程碑小节与 §12。
> 基准：QNAP 官方教程 [如何使用 Qsync Client 6？](https://www.qnap.com.cn/zh-cn/how-to/tutorial/article/%e5%a6%82%e4%bd%95%e4%bd%bf%e7%94%a8-qsync-client-6)
> （最后修订 2025-07-22，适用 Windows / macOS 的 Qsync Client 6.0.0+）。
> 现状基线：M0–M7 全部完成，`fuse-matrix.sh` 68/68、`gui-matrix.sh` 41/41。
> 本文所有「基准侧」结论都来自该教程正文与其中 44 张教程截图（本地留档在 `.research/`，已 gitignore）；
> 并由一份独立的补充调研报告交叉核对（`.research/qsync-client-6-UI调研报告.md`，含官方下载页/社区公告/旧版教程等来源）；
> 所有「现状侧」结论都来自本仓库源码逐文件核对。

---

## 0. 一页速览

**先明确一件事：QNAP 自家的 Linux 客户端早就停更了。**

QNAP 实用工具下载页上，Ubuntu 版仍是 **`QNAPQsyncClientUbuntux64-1.0.12.2202.deb` / `x86-1.0.2.0502.deb`**（
[utilities](https://www.qnap.com/en/utilities/essentials)）——**停在 1.0.x**；Windows 已到 **6.1.0.0831**；
macOS 公开版还是 **5.1.7.0923**，Mac 6.0 仅 Beta。也就是说：

> **6.x 的完整界面在 Linux 上根本不存在。** qxync 不是在「追平 Linux 版 Qsync」，而是在
> **用 Linux 的方式（FUSE 按需同步）重新提供 6.x 的体验**。因此对齐目标应当是
> 「**6.x 的信息架构与术语** + **qxync 自己的技术底座**」，而不是逐像素复刻 Windows 版。

**结论：能靠拢，而且收益最大的是「外壳 + 术语 + 信息架构」，不是「功能堆砌」。**

| 判断 | 内容 |
|---|---|
| 🏆 **性价比最高的一项** | **节省空间模式的 GUI 化**：qxync 的 `placeholder` / `partial` / `hydrated` + `pin` **已经实现**（M1–M3），只要换成 Qsync 的三状态文案（`Online-only` / `Locally available` / `Always available`）与右键菜单（`Always keep on this device` / `Free up space`）即可。**零后端改动**，却能把最硬的底层能力变成用户认得的语言。 |
| ✅ 强烈建议靠拢 | ① 导航范式（顶部 tab → **左侧 6 图标栏** + 页面）；② **「任务」一等概念**（同步任务 / 备份任务，统一任务列表）；③ 面向普通用户的**状态图标 + 进度 `finished/total (xx%)` + 最近时间**，替换现在的内部术语（游标 / baseline / blocked_*）；④ **文件更新中心 + 错误列表**（现在完全没有）；⑤ **设置中心**（代理 / 个人 / 高级 / 释放空间）；⑥ **托盘图标 + 桌面通知 + 开机自启**。 |
| 🟡 有条件靠拢 | **同步方向 1 向 / 2 向**（6.0 官方口径，qxync 现在只有双向）；**选择性同步的勾选式 UI**（建议先用「一任务多配对文件夹」近似）；多 NAS 连接列表（6.x 本身未证实）。 |
| 🚫 **已决策不做**（2026-10-01） | **① 备份任务（原 M8.5）** —— 不做，M8.5 整体移出本方案。**② 版本还原 / 以前版本** —— 不做（客户端版本桶、时间点还原、NAS 侧版本控制 re-probe 全部取消）。**③ QID 云账号发现**（需 QNAP 云 API，报告 07 篇首版即不做）。**④ 团队文件夹 / 分享链接**（需 Qsync Central 分享 API + `auth_data` AES 权限模型，线格式未还原）→ **降级为占位页 + File Station 深链**。**⑤ 创建缩略图**（NAS 媒体索引优化，要引入图像依赖）。**⑥ USB 导入**。**⑦ 与官方 Windows 客户端的 LAN 二进制通道互通**（M7 已明确不做）。 |
| ⚠️ 不该靠拢 | Qsync 是 **Windows CfAPI + 无挂载点**模型；qxync 是 **Linux FUSE + 挂载点 + 显式根**模型。xattr / 占位符 / 脱水安全检查链 / 多根只读判定这些**是 qxync 的护城河，必须保留**，只能换一层用户友好的皮，不能为了「长得像」而砍掉。**特别地：qxync 的筛选器能剪枝整棵子树，而 Qsync 6.x 的用户报告已不能过滤文件夹名 —— 对齐时不许把这条强项做退化。** |

**工作量估算**：M8.0–M8.4 + M8.6 = **14.5 人日**（备份任务已决策不做，不再计入）。

**另外有一条与 M8 解耦的 P0 探针线**（已批准、**已执行完毕**，见 §11）：**设备注册 / 计算机名称**。
结论是**假设被证伪** —— NAS 上早就有官方客户端注册的设备 `win-pc`，事件照样拿不到；
`qbox_get_sync_log` 对全区间 + 全参数变体恒返回 `status:-17`，而该账号从未登记过同步文件夹。
→ **M2c 的「baseline 对账是主路径」被强化**；设备注册**不再实现**，GUI 的「此设备名称」降级为只读展示。

**两个必须盯住的风险**：
1. M8.1 改导航会**打断 `gui-matrix.sh` 的 41 项验收**（它按 `QSYNC_GUI_TAB=status|connect|mounts|files|sync` 逐个 tab 截图）→ 导航改造与矩阵改造必须**同一里程碑、同一 PR**。
2. 「对齐」不得导致**已有强项退化**（筛选器子树剪枝 / 脱水 blocked 安全检查链 / `read()` 不短读）→ 每条都在验收里有独立断言（§8 风险 11）。

---

## 1. 基准：Qsync Client 6 长什么样

### 1.1 主窗口信息架构（从教程截图实测）

```
┌──────────────────────────────────────────────────────────────────────┐
│  [QNAP logo]  页面标题（Task / Sync task）        ⚙设置   ⋮更多菜单   │
├────┬─────────────────────────────────────────────────────────────────┤
│ ①  │  内容区（页面标题带 ← 返回箭头）                                 │
│ ②  │    ┌──────────────┐         ┌──────────────┐                    │
│ ③  │    │ Local  My-NB │   ⇄ /→  │ NAS  TVS-672XT│  ↗在 File Station │
│ ④  │    └──────────────┘         └──────────────┘                    │
│ ⑤  │    Paired Folder / Backup 表格                                   │
│ ⑥  │      本地文件夹 | ⇄ | NAS 文件夹 | 状态 | 操作(⏸/✎/🗑)           │
│    │                                                                  │
│ ⑦  │                                                                  │
└────┴─────────────────────────────────────────────────────────────────┘
```

左侧是一条**只有图标的竖直导航栏**（**截图中只有图标、没有文字标签**，名称来自教程章节与正文；编号即教程里的 1–6 图标位）：

| 图标位 | 名称（英 / 中） | 内容 |
|---|---|---|
| ① | **Task List / Task**（任务列表 / 主页） | 任务卡片列表（每张卡：状态图标 + 一句话状态 + 最近同步/备份时间 + `Sync`/`Backup` 角标 + 暂停/继续 + `>` 进详情）；顶部一行 `admin23@TVS-672XT ● Connected (LAN)`；右上 `Add` + NAS 连接设置图标 |
| ② | **File Update Center**（文件更新中心） | 所有 Qsync 日志；可按文件名搜索、按活动排序、清空 |
| ③ | **Error List**（错误列表） | 同步/备份**失败**的文件与文件夹清单 |
| ④ | **Sharing Center**（分享中心） | 子标签 `Team Folder`（团队文件夹）/ `Share Link`（分享链接） |
| ⑤ | **Settings**（设置） | 四个分区：代理设置 / 个人设置 / 高级设置 / 释放空间 |
| ⑥ | **Help / About**（帮助 / 关于） | `Check for Updates` / `About` / `What's New` / `Quick Start` / `Qsync Help` / `Open Source & Third-Party Software` |

> ⚠️ **6.x 是「统一任务列表」，不是「主页 = NAS 列表」**。4.x/5.x 的主界面是已添加的 NAS 设备列表
> （右键 NAS 出 `Remove NAS / Pause syncing / Sync with NAS now`），6.x 才改成「左侧图标导航 + 一个
> 统一的 Task List」，同步/备份任务靠 `Sync`/`Backup` 角标区分。**对齐时认准 6.x 这一版。**

### 1.2 首次启动：`Create Task` / `Choose Task Type`

弹出**「Create Task / 建立任务」**窗口，页头为 **"Choose Task Type / 选择任务类型"**，两张**大卡片**二选一
（这是 6.0 的关键新形态）：

* **Backup Task**（蓝色 `Backup` 角标）：*"Create a backup task to save important files on your QNAP NAS. You can recover these files by restoring from the backups of previous file versions."*
* **Sync Task**（绿色 `Sync` 角标）：*"Create a sync task to seamlessly synchronize files and folders between your QNAP NAS and this computer, ensuring your data is always up-to-date on both devices."*

> 前置条件：**创建备份任务需要 Qsync Central ≥ 5.0.0.0**（6.x 新增点）。

### 1.3 逐页控件清单（教程正文原文口径）

**任务列表（Task）**
1. 同步任务状态图标：`所有文件均处于最新状态` / `正在同步...已处理 {finished}/{total} 个事件 (xx%)` / `正在扫描` / `暂停` / `错误` / `警告`
2. 备份任务状态图标：`备份已完成` / `正在备份...已处理 {finished}/{total} 个事件 (xx%)` / `正在扫描` / `暂停` / `错误` / `尚未备份`
3. `Sync` / `Backup` 任务类型角标
4. 同步任务：暂停/继续
5. 同步任务：管理 → 打开**任务/同步任务页**
6. 备份任务：启动 / 停止
7. 备份任务：**恢复文件**（点日历选一个**日期 + 时间点**，然后选文件/文件夹 → `Restore`）
8. 备份任务：管理 → 打开**任务/备份任务页**

**「任务 / 同步任务」页**
* `Local` 卡（本机名）/ `NAS` 卡（NAS 名）`↗` 打开 File Station
* **Paired Folder** 表格：`Local Folder | ⇄ | NAS Folder | Status | Action`
* `Add paired folders` 按钮 → **文件夹对设置窗口**（见下）
* 右上 `⚙` 打开 **Setting** 窗口：**`Conflict policies`（冲突策略）** / **`Sharing policies`（分享策略）** / **`Filter settings`（筛选器设置）** / **`Create thumbnails`（创建缩略图，默认关闭且耗资源）**
* 右上 `⋮`：**`Sync with NAS now`（立即与 NAS 同步，全量扫描重新比对）** / **`Smart Delete File Management`（智能删除文件管理）** / **`Delete Sync Task`（删除同步任务）**

**★ 文件夹对设置窗口（`Folder Pair Settings`）—— 全方案最重要的一页，6.x 的能力都挂在这里**

| 区块 | 控件 | 说明 |
|---|---|---|
| 顶部 | `Local` / `NAS` 双栏（各带 `Sync` 角标） | `Local Folder` 占位符 **"Select local folder"**；`NAS Folder` 占位符 **"Select NAS folder"** |
| ① | **`Selective Synchronization`（选择性同步）** 区块 + **`Select`** 按钮 | 文案 *"Select the folder(s) to synchronize to this computer."* → 弹出 `Selective Synchronization` 窗口，「Select the folders you want to synchronize」→ `Apply`。**注意：入口在这里，不是右键菜单** |
| ② | **`Space-Saving Mode`（节省空间模式）** 开关 + `Learn more` | *"Space-Saving Mode allows you to view files without taking up space on the local device. We recommend you to enable this mode for synchronization."* ← **就是 qxync 的按需同步** |
| ③ | **`Enable Smart Delete`** 复选框 | *"Smart Delete Mode ensures that copies of deleted files on the local device are retained on the NAS…"* 并提示 **"This function is not available when Space-Saving Mode is enabled."** ← **两者互斥** |
| 底部 | `Apply` / `Cancel` | 创建向导另有 `Finish` / `Back` |

**「任务 / 备份任务」页**
* `Local folder` 卡（含已选文件夹数与**总大小**，如 `1 folder selected` / `Total size: 207 MB`）/ `NAS folder` 卡（如 `/home/QsyncBackup/User@User-NB`）
* `Backup mode: Real-time` + `Settings` → **`Frequency Settings`（频率设置）窗口**：
  `Backup Mode:` 三选一 **`Real-time` / `Manual` / `Scheduled`**（实时/手动/计划）；
  `Backup frequency:` 下拉（如 `Hourly`）+ **`Start at: 00 min`** + 说明 *"Run once every hour at 00 min."*；
  复选框 *"Run backup tasks immediately if there are any unprocessed tasks when starting Qsync"* → `Save` / `Cancel`
* `Advanced Settings` → **按名称或扩展名**配置备份排除
* `⋮` → `Delete Backup Task`（确认框）/ 打开 File Station 上的 Qsync 备份文件夹

**恢复文件（Restore Files）对话框**
* 顶部 `‹ 05/20 14:06 ›` 时间点选择器（**红框高亮**，点击出日历选年/日/时间 → `Apply`）
* 左侧 NAS 目录树（`user@TVS-672XT > Disk D > MyNB_Data`）；面包屑 `Root > Disk D > MyNB_Data`；搜索框 `Search file name`
* 右侧文件表 `File Name | Modified Date | Size | Action`；行动作 = 下载、**`View previous versions`**
* 底部：分页（`Page 1 /1`、`Display item:1 - 4, Total: 4`、`Show [100] Item(s)`）、`Show deleted files/folders` 开关、`Restore` / `Cancel`
* 底部提示 *"It takes a moment for Qsync Central to retrieve folder contents…"* ← **还原是 NAS 侧能力**

**文件更新中心**：全部日志 + 按文件名搜索 + 按活动排序 + 清除
**错误列表**：同步/备份失败项清单
**分享中心**：团队文件夹（`Sharing Status` / `Modify the Team Folder` / 取消分享 / 接受或拒绝别人的分享）；分享链接（复制 / `Share the files(s)` 编辑 / 删除）

### 1.4 概念模型：Task 是一等公民

```
NAS 连接（可多个）  →  Task（同步任务 / 备份任务）  →  Paired Folder（1..N 个本地↔NAS 文件夹对）
```

* **同步任务** = 双向（也支持 1 向 / 2 向规则），一个任务下可以有**多个配对文件夹**
* **备份任务** = 单向（本地 → NAS），有**频率**与**排除规则**，**支持时间点还原**
* 「释放空间」「筛选器」「冲突策略」都挂在**任务**上，而不是全局
* ★ **两个容易混的「范围控制」是两套东西**：
  * **选择性同步（Selective Synchronization）** = 在配对文件夹设置里**勾选要同步到本机的子文件夹**（粗粒度，决定「哪些文件夹存在」）
  * **筛选器设置（Filter settings）** = **通配符规则**（`thumbs.db` / `*.crdownload` / `.*`），决定「哪些文件被排除」
  qxync 目前只有后者（M7 `exclude`），前者要靠「多根 / 多任务」近似。
* ★ **节省空间模式与智能删除互斥**：原文 *"This function is not available when Space-Saving Mode is enabled."*
  —— 这条对 qxync 有直接映射意义：**按需同步开着的时候，「删除即保留在 NAS」的语义要重新定义**。

### 1.5 设置页四个 tab（教程原文）

| tab | 选项 |
|---|---|
| **代理**（`Proxy`） | **`No proxy`（无代理）/ `Auto-detect`（自动检测，官方推荐）/ `Manual`（手动）**（服务器 IP 或 URL + 端口 + `Proxy server requires a password` + 用户名口令）→ `Apply` → `OK` |
| **个人**（`Personal`） | **`Launch Qsync at startup`（在启动时，启动 Qsync）**；语言；地区 *"Select the correct region to ensure better connectivity"*；`连接 USB 外部设备时导入照片和视频` + 默认导入文件夹 |
| **高级**（`Advanced`） | **`Enable debug log`（启用调试日志）** + `Send Logs`（导出）/ `Clear Logs`，⚠ 影响同步性能；**`Show desktop notifications`（显示桌面通知，每个 Qsync 活动都发）**；`Automatically check for updates`；**`Enable LAN sync`（⚠ 仅 Windows/Mac，需 Qsync Central 3.0.3+ / QTS 4.3.4+）** |
| **释放空间**（`Free up space`） | **`Free up space automatically`（自动释放空间）** → **`When space is less than`（当空间少于 <百分比>）** 或 **`By frequency`（按频率）**；**`Free Up Space Now`（立即释放空间）** |

> 教程**没有**带宽限速项（**未证实**）；连接侧另有 `Secure login`（安全登录）与
> `Automatically select the best connection method`。

### 1.6 NAS 连接管理（右上角）

* `Use IP address` / 使用 IP 地址指定（默认端口 **8080**，端口变了要跟 IP 一起写）
* `Search via LAN` / 通过 LAN 搜索 → `Search NAS` 窗口 → 选中 → `Select`（教程注：目前只有 **Windows / Ubuntu** 版有）
* `Search via QID` / 通过 QID 搜索 → `Sign in to QNAP Account`（邮箱 + 口令）→ `Search NAS` 窗口 → `Select`
* `Computer name` / **计算机名称**：只允许 A-Z a-z 0-9 与连字符，**"此名称将显示在 Qsync Central 的设备列表中"** → `OK` → 确认对话框 → `Yes`
* 右上 `⋮`：`Connection Test` / `Edit Connection` / `Remove NAS`（⚠ 移除会**删除所有同步任务和备份任务**）
* **6.x 多 NAS 能力未证实**（5.x 文档明确「Windows 可加多台、macOS 只能一台」；6.x 教程只描述单一 NAS 连接管理）

### 1.7 术语表（**做 UI 文案对齐时按这张表，全部是一手来源原文**）

| Qsync Client 6（官方中文） | 英文原文 | qxync 内部现状 | 建议 GUI 文案 |
|---|---|---|---|
| 同步任务 | Sync Task | 无（= link + roots + 挂载点） | 同步任务 |
| 备份任务 | Backup Task | 无 | 备份任务 |
| 配对文件夹 | Paired Folder | `roots` + `mountpoint` | 配对文件夹 |
| 文件夹对设置 | Folder Pair Settings | 无（表单散落） | 文件夹对设置 |
| **选择性同步** | **Selective Synchronization** | 挂载的根（粗粒度，无勾选 UI） | 选择性同步 |
| **筛选器设置** | **Filter settings** | `exclude` 规则（M7）**已有** | 筛选器 |
| **节省空间模式** | **Space-Saving Mode** | **按需同步 / 脱水（M1–M3）已有** | 节省空间模式 |
| 仅在线 | Online-only | `placeholder` | 仅在线 |
| 本地可用 | Locally available | `partial` / 部分水合 | 本地可用 |
| 始终可用 | Always available | `hydrated` / `pin=pinned` | 始终可用 |
| 始终保留在此设备 | Always keep on this device | `pin=pinned` | 始终保留在此设备 |
| 释放空间 | Free up space | `dehydrate` | 释放空间 |
| 立即释放空间 | Free Up Space Now | `dehydrate --all` | 立即释放空间 |
| 智能删除 | Smart Delete | 删除熔断（M2c，形态不同） | 待确认删除 |
| 冲突策略 | Conflict policies | 硬编码「冲突副本」 | 冲突策略 |
| 文件更新中心 | File Update Center | 无（只有底部 IPC 操作日志） | 文件更新中心 |
| 错误列表 | Error List | 无（散落 `sync.last_error` / `uploads.failed`） | 错误列表 |
| 分享中心 | Sharing Center | 无 | —（不做） |
| 团队文件夹 / 分享链接 | Team Folder / Share Link | 无 | —（不做） |
| LAN 同步 | Enable LAN sync | `peer_listen`（M7 **自研**协议） | LAN 加速 |
| 计算机名称 | Computer name | NAS 侧已有官方客户端注册的设备名（只读可列） | 此设备名称（只读） |
| 立即与 NAS 同步 | Sync with NAS now | `sync --once` | 立即同步 |
| 单向 / 双向同步 | 1-way / 2-way sync | 仅双向 | 同步方向 |
| 调试日志 / 发送日志 | Enable debug log / Send Logs | `RUST_LOG` + 滚动日志文件 | 调试日志 |
| 开机自启 | Launch Qsync at startup | 无 | 开机自启 |

**冲突策略五选项（原文，实现时照抄）**：

1. `Let me decide for each file`（每个文件都问我）
2. `Rename files on the NAS`（重命名 NAS 上的文件）
3. `Rename local files`（重命名本地文件）
4. `Replace files on the NAS with local files`（用本地文件替换 NAS 上的文件）
5. `Replace local files with files on the NAS`（用 NAS 上的文件替换本地文件）

> qxync 现在的硬编码「冲突副本」≈ 选项 2/3 的中间态（远端占原名、本地内容另存 `xxx (conflicted copy from …)`）。
> **选项 1「每个文件都问我」是纯 GUI + 队列的交互**，正好能替代现在的「删除熔断 + 强制放行」粗粒度做法。

**筛选器格式（原文示例）**：`thumbs.db`、`*.crdownload`、`*.part*`、`.*`（支持 `*`）。
⚠ 官方社区有用户报告 **6.x 起筛选器不再匹配文件夹名**（**未证实**）——
而 qxync 的 `exclude` 是**能剪枝整棵子树**的（`/cache/`），这一点上 **qxync 更强，对齐时不要退化**。

**编码冲突命名**：Unicode NFC/NFD 冲突时在名称末尾追加 **`(Conflicted encode (x))`**（内容冲突的后缀官方未文档化，未证实）。

### 1.8 6.0 的定位（官方新闻稿口径，用于判断「哪些是必须对齐的旗舰功能」）

QNAP 在 [Qsync 6.0 Beta 新闻稿](https://www.storagenewsletter.com/2025/07/04/qnap-unveils-qsync-6-0-beta-one-stop-file-backup-and-synchronization-solution/)（2025-07-04）里把 6.0 定义为
**「一站式文件备份与同步」**，四大块：

1. **文件/文件夹备份**（6.0 新增）：Windows/macOS → NAS，**多版本 + 时间点还原（point-in-time recovery）**，
   可配置 **实时 / 计划 / 手动** 三种频率。
2. **跨设备同步**：明确支持 **1 向与 2 向同步规则**；离线同步；**省空间模式（Space-saving mode，即按访问下载）**。
3. **协作与分享**：团队文件夹 + 分享链接。
4. **私有云管控**：备份设备管理、远程设备擦除、文件策略下发（都在 Qsync Central 侧）。

> 对照意义：
> * 第 2 条的「省空间模式」正是 qxync 的**按需同步 / 脱水**，我们已经做到了 —— 这是我们**唯一已经领先或持平**的旗舰能力。
> * 第 2 条的「**1 向 / 2 向同步规则**」说明同步任务的方向是可配置的，qxync 目前只有双向（`read_write` 只控制能不能写）。
> * 第 1 条（备份 + 时间点还原）是 6.0 的**头号新功能**，也是我们缺口最大的地方（见 §4.3 的版本化约束）。
> * 第 3/4 条依赖 Qsync Central 的 NAS 侧服务，**不在本项目射程内**。

**版本事实（用于判断「对齐哪一版」）**：

| 平台 | 当前公开版本 | 备注 |
|---|---|---|
| Windows | **6.1.0.0831** | 历史：6.0.3.0703 / 6.0.2.0529 / 6.0.0.x |
| macOS | **5.1.7.0923** | 6.0 仍是 Beta（要求 macOS 12+，官方称 "Redesigned task interface"） |
| Ubuntu | **1.0.12.2202 / 1.0.2.0502** | **长期停在 1.0.x** |

**6.0.3 的增量**（值得对齐）：查看/还原历史版本新增两处入口（Windows 资源管理器右键 `Qsync` → `View previous versions`；
客户端 **File Changes 页 → `View versions`**），`View Previous Versions` 对话框显示每个版本的
backup date / modified date / size / **来源设备**，可一键恢复或下载；节省空间模式的云端占位文件在资源管理器显示缩略图
（需 NAS 端 File Station 缩略图服务）。

**未证实项**（对齐时不要当成事实）：托盘右键菜单原文；备份保留策略的具体 UI（官方只说 "multi-version + point-in-time recovery"）；
6.0.4 / 6.1.0 的 changelog（官方 release-notes 是 SPA，无稳定直链）；6.x 的多 NAS 能力；带宽限速（教程里没有）。

---

## 2. 现状：qxync GUI（M4）长什么样

### 2.1 信息架构（`crates/qxync-gui/ui/index.html`）

```
┌────────────────────────────────────────────────────────────────┐
│ QSync  QNAP 按需同步   [qxyncd:运行中][连接:…][登录:…]  启动/停止/立即登录 │
│ daemon：pid … · uptime … · socket …                            │
├────────────────────────────────────────────────────────────────┤
│ [状态/进度][连接/登录][挂载][文件/pin][同步/缓存]   ← 5 个顶部 tab      │
├────────────────────────────────────────────────────────────────┤
│ 卡片（KV 表 + 表格 + 进度条），2/3 列自适应网格                    │
├────────────────────────────────────────────────────────────────┤
│ 操作日志（每次 invoke 一行，保留 200 条，可收起/清空）              │
└────────────────────────────────────────────────────────────────┘
```

| tab | 内容（`docs/M4-GUI.md` §4） |
|---|---|
| 状态 / 进度 | 服务端信息、会话、三游标、水合统计、上传队列、缓存/脱水 + 限额进度条 + `blocked_*`、挂载点表、**远端根面板**、最近一次同步摘要 |
| 连接 / 登录 | host/port/https/insecure/user/password/home_root/roots 表单；保存配置 / 保存并登录 / 启停 daemon；本机路径与运行环境 |
| 挂载 | 挂载表 + 新建挂载（挂载点 / 远端根多行 / cache_mode / threads / 水合超时 / 删除熔断 / auto_unmount / 读写） |
| 文件 / pin | 远端目录浏览（目录优先）、每行 pin 查询/设置、下载、脱水、新建目录、删除 |
| 同步 / 缓存 | `SyncInfo` 全量 + 立即同步/强制放行/暂停/设间隔；`CacheInfo` 全量 + 脱水预演/全部/限额/闲置 |

### 2.2 后端已有能力 vs GUI 已暴露（`crates/qxync-core/src/ipc.rs`）

| IPC | 状态 | GUI 是否用上 |
|---|---|---|
| `ping` / `status` / `login` / `logout` | ✅ | ✅ |
| `ls` / `stat` / `get` / `put` / `mkdir` / `rm` | ✅ | `ls`/`get`/`mkdir`/`rm` ✅；`stat`/`put` ❌（未暴露） |
| `pin` | ✅ | ✅ |
| `mount` / `umount` / `mounts` | ✅ | ✅（**挂载点是内存态，重启即丢**） |
| `roots` | ✅ | ✅ |
| `rules`（M7 选择性同步） | ✅ | ❌ **GUI 完全没做**（只在保存时保留字段） |
| `peer`（M7 LAN 对等） | ✅ | ❌ **GUI 完全没做** |
| `sync`（M2c） | ✅ | ✅ |
| `store`（M5 状态库） | ✅ | ❌ |
| `dehydrate`（M3） | ✅ | ✅（但只有手动四连，没有「自动释放空间」开关） |
| `shutdown` | ✅ | ✅ |

**未实现（后端也没有）**：任务概念、任务持久化、同步日志表、失败项持久化、全局设置、代理、托盘、通知、开机自启、文件选择器、备份任务、设备注册、进度百分比、多 link 切换。

---

## 3. 差异分析

### 3.1 界面结构差异

| 维度 | Qsync Client 6 | qxync GUI（M4） | 差距性质 |
|---|---|---|---|
| 导航 | **左侧图标栏**（7 个目的地）+ 卡片式主页 | **顶部 5 个文本 tab** | 结构 |
| 页面层级 | 主页 → 任务详情页（带 ← 返回） | 平铺 5 个 tab，无层级 | 结构 |
| 一级信息 | **任务**（状态 + 时间 + 进度） | **子系统**（状态/连接/挂载/文件/同步） | **范式** |
| 状态表达 | 图标 + 一句话 + `finished/total (xx%)` + 时间戳 | KV 表 + 计数器 + 内部术语 | **受众** |
| 日志 | 文件更新中心（可搜索/排序/清空） | 底部 IPC 调用日志（200 条） | 功能 |
| 错误 | 独立「错误列表」页 | 散落在 `val-err` 单元格 + 操作日志 | 功能 |
| 设置 | 独立设置页（4 个 tab） | 无（散在各页表单） | 功能 |
| 常驻形态 | 托盘 + 桌面通知 + 开机自启 | 只能开着窗口 | 功能 |

### 3.2 交互范式差异（最本质的一条）

* Qsync 的第一屏回答的问题是：**「我的东西同步好了吗？」** → 绿色勾 + 「所有文件均处于最新状态」+ 「Last synchronized: 2025-04-07 12:13:34」。
* qxync 的第一屏回答的问题是：**「守护进程和协议栈现在什么情况？」** → `max_log` / `global_notify` / `sync_signal` 游标、`水合次数/字节`、`blocked_dirty/pinned/open/mapped/inflight`。

> 这不是「谁更好」，而是**受众不同**：M4 的 GUI 是**排障面板**（验收矩阵就是靠这些字段判定的），
> Qsync 的 GUI 是**消费级状态面板**。
> 靠拢的正确做法是**两层并存**：新增面向普通用户的「主页/任务/更新中心/错误/设置」，把现有的游标、
> baseline、blocked_* 收进「诊断（专家模式）」页 —— **不是删掉它们**（验收矩阵依赖它们）。

### 3.3 功能差异矩阵

图例：✅ 已有 · ⚠️ 部分 / 形态不同 · ❌ 没有 · 🚫 不做

| # | Qsync Client 6 功能 | qxync 现状 | 差距 | 可行性 |
|---|---|---|---|---|
| 1 | 任务类型选择页（同步 / 备份） | ❌ | ❌ | ✅ 纯 GUI |
| 2 | 同步任务（配对文件夹，一个任务多个对） | ⚠️ 一个 daemon 一个 link，`roots` 多根共享一个挂载点 | 概念 + 持久化 | ✅ 后端小改 |
| 2b | 同步方向：**1 向 / 2 向同步规则**（6.0 官方口径） | ⚠️ 只有双向（`read_write` 只控制「能不能写」） | 配置项 | 🟡 后端中等改动（单向只跑一侧的对账/上传） |
| 3 | 备份任务（单向 + 频率） | ❌ | 新功能 | 🚫 **已决策不做**（2026-10-01） |
| 4 | 备份频率：实时 / 手动 / 计划 | ❌ | 新功能 | 🚫 同上（随备份任务一并移出） |
| 5 | 备份排除（按名称 / 扩展名，`Advanced Settings`） | ⚠️ `exclude` 规则引擎已有（M7），但**没有 GUI** | GUI | ✅ |
| 6 | 冲突策略：**5 个选项**（见 §1.7 原文） | ⚠️ 硬编码「冲突副本」 | 配置项 + 「每个文件都问我」交互 | ✅ core 小改 + GUI 队列 |
| 7a | **选择性同步（Selective Synchronization，勾选子文件夹）** | ⚠️ 只能靠「多根 / 多任务」近似，无勾选 UI | 概念 + GUI | 🟡 建议先做「每任务多配对文件夹」（M8.2），勾选 UI 后置 |
| 7b | **筛选器设置（Filter settings，通配符排除）** | ⚠️ 规则引擎已有（`qsync rules`），**GUI 完全缺失** | **GUI** | ✅ |
| 8 | 立即与 NAS 同步（全扫描） | ✅ `sync --once` | GUI 已有 | ✅ |
| 9 | 智能删除文件管理（删除的副本保留在 NAS） | ⚠️ 删除熔断（M2c），只是「挡住 + 强制放行」 | 语义 + 形态 | ✅ 降级为「待确认删除」；★ 与节省空间模式**互斥** |
| 9b | **节省空间模式（Space-Saving Mode）三状态**：仅在线的 / 本地可用 / 始终可用 | ✅ **已有**（`placeholder` / `partial` / `hydrated` + `pin`） | **只是缺 GUI 与文案** | ✅ 纯 GUI，**本项目最划算的一项** |
| 9c | 右键 `Always keep on this device` / `Free up space` | ⚠️ 有 `pin=pinned` 与 `dehydrate`，但入口是表格里的按钮 | GUI | ✅ 文件页右键菜单 |
| 10 | 暂停 / 继续任务 | ⚠️ 只能暂停轮询（`interval_secs=0`），且是全局 | 粒度 | ✅ 后端小改 |
| 11 | 进度 `finished/total (xx%)` | ⚠️ 有计数器（pending/active/done/failed/bytes），无百分比 | 上报字段 | ✅ |
| 12 | 文件更新中心（日志 + 搜索） | ❌（只有 IPC 调用日志） | **新表 + 新页** | ✅ daemon 本地改动 |
| 13 | 错误列表 | ❌ | **新表 + 新页** | ✅ 同上 |
| 14 | 恢复文件 / 以前版本（`Restore Files` + `View previous versions`） | ❌ | — | 🚫 **已决策不做**（2026-10-01）。备份任务页随之取消，所以连「打开 File Station 看历史版本」的挂载点也没有了；文件页仍保留一个通用「在 File Station 打开」深链作为出口 |
| 14b | 编码冲突命名 `(Conflicted encode (x))` | ❌（现在只有 `(conflicted copy from …)`） | 命名规则 | ✅ 小改（NFC/NFD 检测） |
| 15 | 团队文件夹 | ❌ | — | 🚫 需 Qsync Central 分享 API + 权限模型 |
| 16 | 分享链接 | ❌ | — | 🚫 同上 |
| 17 | 打开 File Station / NAS 网页 | ❌ | 小 | ✅ 一行深链 |
| 18 | 代理设置（`No proxy` / `Auto-detect` 推荐 / `Manual` + 认证） | ❌（`reqwest` 未接代理） | 新配置 | ✅ |
| 19 | 个人：开机自启 | ❌ | 新 | ✅ autostart `.desktop` |
| 20 | 个人：语言 / 地区 | ❌（全中文硬编码） | 新 | 🟡 i18n 是可选项 |
| 21 | 个人：USB 导入照片视频 | ❌ | — | 🚫 与本项目目标无关 |
| 22 | 高级：调试日志开关 + 发送/清除 | ⚠️ `RUST_LOG` + 滚动日志文件已有，无 GUI | GUI | ✅ |
| 23 | 高级：桌面通知 | ❌ | 新 | ✅ Tauri 插件 |
| 24 | 高级：自动检查更新 | ❌ | — | 🟡 可降级为「查看版本 / 打开下载页」 |
| 25 | 高级：LAN 同步 | ⚠️ M7 自研 `peer`（默认不监听），**GUI 完全没做** | GUI | ✅ |
| 26 | 释放空间：`Free up space automatically`（`When space is less than` % / `By frequency`） | ⚠️ `QSYNC_DEHYDRATE_IDLE` + `QSYNC_CACHE_LIMIT`（额度式，**非剩余空间百分比**） | 条件式 | ✅ 需 `statvfs` |
| 27 | `Free Up Space Now`（立即释放空间） | ✅ | GUI 已有 | ✅ |
| 28 | NAS 连接：IP 指定 | ✅ | ✅ | ✅ |
| 29 | NAS 连接：LAN 搜索 | ❌ | 新 | 🟡 需 UDP 广播/mDNS 探测 |
| 30 | NAS 连接：QID 搜索 | ❌ | — | 🚫 需 QNAP 云账号 API（报告 07 篇首版不做） |
| 31 | 计算机名称（Qsync Central 设备列表可见） | ⚠️ **NAS 侧已有**官方客户端注册的设备 `win-pc`（可列出）；qxync 不写入 | UI 只读 | ✅ **降级为只读展示**（§11：`qbox_save_device_config` 已探针并决策不做） |
| 32 | 托盘图标 + 右键菜单 | ❌ | 新 | ✅ 托盘存在已被教程佐证；**右键菜单原文未证实**，条目按 4.x/5.x 同义项推测 |
| 33 | 移除 NAS（连带删任务） | ⚠️ 可停 daemon / 改 link，无「移除」语义 | 小 | ✅ |
| 34 | 关于 / 新增功能 / 快速入门 / 帮助 | ❌ | 小 | ✅ |
| 35 | 创建缩略图 | ❌ | — | 🚫 NAS 媒体索引优化，需图像依赖 |
| 36 | 多 NAS 连接 | ⚠️ 后端支持 `linkId`，GUI 只发 `default` | GUI | 🟡 **6.x 多 NAS 能力本身未证实**（5.x 才明确 Windows 可多台） |

---

## 4. 可行性研判

### 4.1 纯 GUI 就能靠拢（零协议改动、零 daemon 改动）

* 左侧图标栏外壳 + 页面路由 + 主页卡片（数据全部来自现有 `status` / `mounts` / `roots` / `sync`）
* 首次运行向导（`Create Task` → `Choose Task Type`）：**只提供 Sync Task 一张卡**；Backup Task 卡片显示为不可用，
  点击弹出说明「本客户端暂不支持备份任务」——**保留卡片位是为了让 Qsync 用户一眼看懂缺什么**，而不是悄悄没有。
* ★ **节省空间模式的三种状态与右键菜单**：qxync 的 `placeholder` / `partial` / `hydrated` + `pin`
  **已经完整实现**（M1–M3），只是缺 GUI 文案与入口。映射：
  `Online-only` ← `placeholder`；`Locally available` ← `partial`；`Always available` ← `hydrated`/`pinned`；
  `Always keep on this device` ← `pin=pinned`；`Free up space` ← `dehydrate`。
  **这是全方案性价比最高的一项：零后端改动，直接把已有的硬能力变成 Qsync 用户认得的语言。**
* 文件页右键/行内操作改用**菜单化**（节省空间模式 ▸ 始终保留在此设备 / 释放空间 / 下载 / 删除 / 打开位置）
* 托盘 + 桌面通知（Tauri `tray` + `tauri-plugin-notification`）
* 文件选择器（`tauri-plugin-dialog`，替掉现在的「弹窗填路径」）
* 打开 File Station / 日志目录 / 下载页（`tauri-plugin-opener`）
* 关于页、快速入门

### 4.2 需要新写 daemon 能力（**全在本机，不需要任何新 NAS 协议**）

| 能力 | 落地方式 | 影响面 |
|---|---|---|
| **任务（Task）持久化** | 新 `~/.config/qsync/tasks/<id>.json`；`mounts` 现在是 `HashMap` **内存态**，重启即丢 → 改为任务驱动 + 启动恢复 | `qxync-daemon/daemon.rs` 挂载生命周期 |
| **同步日志表 `journal`** | `sync.db` 新表 `journal(ts, task_id, path, action, direction, bytes, status, error)`；在 sync/upload/conflict/dehydrate 落一条；新 IPC `journal{limit,query,level}` / `journal_clear` | `qxync-core/store.rs`、`qxync-daemon/sync.rs` |
| **失败项持久化** | 同上表 `status='error'` 视图 | 同上 |
| **全局设置** | 新 `~/.config/qsync/settings.json` + IPC `settings` / `settings_save`（代理、通知、自启、释放空间阈值、并发、日志级别、设备名） | `qxync-core/config.rs` |
| **代理** | `reqwest::Proxy` 从 settings 读，`Client` 构造时应用 | `qxync-client/lib.rs` |
| **进度百分比** | `SyncInfo`/`BackupInfo` 加 `finished` / `total` / `current_path` | `qxync-core/ipc.rs`、`sync.rs` |
| **冲突策略** | 照抄 Qsync 的 **5 个选项**（§1.7 原文），映射到 `qxync-core/sync.rs` 的三向决策表：`ask`（每个文件都问我 → 写 `pending_decisions` 队列，GUI 逐个裁决）/ `rename_remote` / `rename_local` / `replace_remote` / `replace_local`；默认保持现有的「远端占原名 + 本地另存副本」 | `qxync-core/sync.rs`、`store.rs`（队列）、`ipc.rs` |
| **编码冲突命名** | Unicode NFC/NFD 冲突时在名称末尾追加 `(Conflicted encode (x))`（与内容冲突的 `(conflicted copy from …)` 区分开） | `qxync-core/sync.rs` |
| **释放空间条件式** | `statvfs` 取剩余空间百分比触发 + `By frequency` 定时触发，接到现有脱水安全检查链 | `qxync-daemon`（脱水候选已就绪） |
| **传输队列明细** | 现在只有计数（pending/active/done/failed/retries/bytes），加每项 `path/state/retry/error` 的 IPC `transfers` | `qxync-core/store.rs`（queue.db 已有） |
| ~~**备份任务**（大件）~~ | 🚫 **已决策不做**（2026-10-01），整条移出本方案 | — |
| **设备注册 / 计算机名称** | 🚫 **已探针并决策不做**（见 §11）—— `qbox_save_device_config` **不实现**；GUI 的「此设备名称」改为**只读展示** `qbox_get_device_config_list` 里已有的设备名 | — |

> ⚠️ **journal（上表第 2/3 行）是「文件更新中心 + 错误列表」的唯一前置**，**没有它两个页面只能是空壳**，
> 所以必须排在页面之前。
> ✅ **设备注册那条线已经跑完并关闭**（§11）：假设「注册设备就能拿到事件」**被证伪** ——
> 设备早由官方客户端注册，`qbox_get_sync_log` 仍对全区间全参数恒返回 `-17`。
> 结论是**保持 M2c 的 baseline 对账主路径**，不为事件快路径做任何投入。

### 4.3 需要 NAS 侧支持 / 协议未还原（本轮不做或降级）

| 功能 | 结论 | 依据 |
|---|---|---|
| **恢复文件 / 以前版本** | 🚫 **已决策不做**（2026-10-01）。技术背景仍记录在此：M5 真机实测 `versioning_support` 全 0、`versioning_stat_delta` 恒 `exist:0`、`versioning_gen_sig` 恒 `status:33`；NAS 侧虽有 `Enable version control` 设置，但**不再投入 re-probe 与实现**。<br>顺带说明：qxync 是 Linux，本就做不了 Qsync 那种资源管理器右键菜单集成（6.0.3 的入口之一）。 | M5 真机实测 + 决策 |
| **团队文件夹 / 分享链接** | 🚫 不做。需要 Qsync Central 的分享 API + `auth_data` AES 权限模型 + 用户目录解析，线格式未还原。 | 报告 07 / 12 篇范围外 |
| **QID 云账号设备发现** | 🚫 不做。需 QNAP 账号 OAuth + 云 API，报告 07 篇首版即列为不做。 | 报告 07 |
| **创建缩略图** | 🚫 不做。属 NAS 媒体索引优化，需引入 `image` 依赖 + 额外上传通道，与 Linux 客户端目标无关。 | 权衡 |
| **与官方客户端 LAN 通道互通** | 🚫 不做。`Auth1`/`Auth2`/`LANDownloadFile` 线格式未还原，成本高收益有限（M7 已决策）。 | `docs/M7-*.md` §已确认决策 5 |
| **USB 导入照片 / 视频** | 🚫 不做。 | 权衡 |

### 4.4 不该靠拢（Linux/FUSE 语义决定）

1. **没有「无挂载点」形态**：Qsync 靠 Windows CfAPI 让文件直接出现在 `C:\Users\...\Qsync`；
   qxync 必然有挂载点。→ 界面要**把挂载点讲清楚**（Qsync 用户不知道什么是挂载点），而不是藏起来。
2. **xattr 占位符 / 脱水安全检查链 / 两条铁则**：这是 qxync 的数据安全护城河（`user.qsync.state`、
   `inval_inode` 先于清内容、绝不短读）。UI 可以参考 Qsync 的「仅在线 / 保留在此设备」文案，
   但**不能**为了视觉一致删掉这些机制或其可观测性。
3. **多根只读判定**：共享文件夹只读是 NAS 侧的真实约束（实测写被拒 `status:20`）。
   Qsync 不会遇到这个（它只同步 Qsync 同步文件夹）。→ 界面上要**显式标「只读」并解释原因**。
4. **诊断维度**：游标 / baseline / blocked_* 是验收矩阵的判据，**只能收纳，不能删除**。

---

## 5. 目标形态（To-Be）

### 5.1 页面映射表

| 新导航（对齐 Qsync） | qxync 页面 | 主要数据源 | 对应 Qsync |
|---|---|---|---|
| **① 主页** | 概览：连接卡 + 任务卡列表 + 快捷入口 | `status` + `tasks` | 主页 / 任务 |
| **② 任务** | 任务列表 → 任务详情（**同步任务页**；备份任务页已决策不做） | `tasks` + `mounts` + `sync` | 任务列表 + 任务详情页 |
| **③ 文件** | 远端根浏览（现「文件/pin」升级，加右键菜单、只读标记、占位/已水合列） | `ls` / `pin` / `roots` / `rules` | Paired Folder 表格（弱对应） |
| **④ 更新中心** | 同步日志（搜索 / 排序 / 清空） | **`journal`（新）** | 文件更新中心 |
| **⑤ 错误** | 失败项列表（重试 / 复制路径 / 打开日志） | **`journal` where status=error（新）** | 错误列表 |
| **⑥ 设置** | 连接 / 同步与筛选 / 缓存与释放空间 / 网络与 LAN / 通知与启动 / 关于 | `settings`（新）+ `link` + `rules` + `peer` | 设置（代理/个人/高级/释放空间） |
| **⑦ 诊断（专家模式）** | 现有 状态/进度、挂载、同步/缓存 三页原样收纳 | 现有全部 | （Qsync 无对应） |
| — | 底部操作日志（IPC 调用日志）保留在诊断页内 | 前端本地 | — |
| — | **分享中心**：占位页，一句话说明「需 NAS 侧 Qsync Central 团队文件夹 / 分享链接，本客户端暂不支持」+ 打开 File Station 深链 | — | 分享中心（**降级为占位**） |

> 「诊断」默认收起在设置页底部或导航最下方；`QSYNC_GUI_TAB` 仍可直达，保证验收矩阵可继续按原字段判定。
> 分享中心做成**占位页**而不是直接不做：Qsync 用户的肌肉记忆在这里，留一个「说明 + 出口」比凭空消失更好。

### 5.2 任务模型映射

```
Task { id, kind: sync,                 // 备份任务已决策不做；kind 字段预留，暂只有 sync
       name, enabled,
       local_dir,            // FUSE 挂载点目录
       remote_dir,           // NAS 上的目标目录
       direction,            // 双向 2-way（默认）| 仅上传 1-way-up | 仅下载 1-way-down
                             //   ← 对齐 Qsync 6.0 的「1-way / 2-way sync rules」
       filter { exclude[], filter_temp },   // 复用 M7 规则引擎（≈ Filter settings）
       selective { subfolders[] },          // ≈ Selective Synchronization（勾选要同步的子文件夹）
       conflict_policy,      // 5 选项，见 §1.7；默认「远端占原名 + 本地另存副本」
       space_saving,         // 节省空间模式开关（≈ 按需同步；与 smart_delete 互斥）
       smart_delete,         // 智能删除（与 space_saving 互斥）
       cache { mode, auto_free: { on_low_space_pct, every_hours }, }
     }
```

* **同步任务** ⇒ 大致等于「一个挂载点 + 一个（或一组）远端根」，落到现有 `Request::Mount`。
  多个配对文件夹 = 任务内多条 `(local_dir, remote_dir)` 对 ⇒ 多个挂载点（daemon 已支持多挂载）。
* ~~**备份任务** ⇒ 新 `Request::Backup`~~ 🚫 **已决策不做**。
* **迁移**：现有「一个 link + roots」在首次启动时自动生成**一个同步任务**（`name = link.id`），
  行为与 M7 **一字不变**；用户不感知迁移。

### 5.3 关键界面草案

**主页**
```
[QNAP 图标]  任务                                    [+ 添加任务]  [⚙]
────────────────────────────────────────────────────────────────────
 ✓  所有文件均处于最新状态                        ⏸   ›
    Last synchronized: 2026-10-01 18:30:12
    [Sync]  本地 ~/qsync-mnt  ⇄  NAS /home
（备份任务已决策不做 —— 主页只列同步任务；空态给「+ 添加任务」引导）
```

**任务详情（同步任务）**
```
← 任务 / 同步任务                                     [⚙]  [⋮]
┌── 本地 ─────────────────┐   ⇄   ┌── NAS ──────────────────┐
│ 此设备（<hostname>）     │       │ test1@<NAS>   ↗File Station│
└─────────────────────────┘       └─────────────────────────┘
配对文件夹                                       [+ 添加配对文件夹]
本地文件夹          | 方向 | NAS 文件夹        | 状态                  | 操作
~/qsync-mnt         |  ⇄   | /home             | ✓ 所有文件均处于最新状态 | ⏸ ✎ 🗑
```
`⚙`（Setting）→ **冲突策略**（5 选项）/ **筛选器设置**（通配符规则 + 实时预览）/ 节省空间模式 / 缓存与释放空间 / 创建缩略图（灰掉并注明不支持）
`⋮` → 立即与 NAS 同步 / 智能删除文件管理 / 删除同步任务 / 在文件管理器中打开

**文件夹对设置（`Folder Pair Settings`）**
```
┌─ 本地 ────────────────┐        ┌─ NAS ─────────────────┐
│ [选择本地文件夹…]      │   ⇄    │ [选择 NAS 文件夹…]     │
└───────────────────────┘        └───────────────────────┘
选择性同步   选择要同步到这台电脑的文件夹              [选择…]
节省空间模式  仅在线查看、不占本地空间        [开关]  ⓘ了解更多
智能删除     删除的文件副本保留在 NAS 上      [ ]     （节省空间模式开启时不可用）
                                                          [应用] [取消]
```

**文件页右键菜单（对齐 Qsync 的 `Space-Saving Mode` 子菜单）**
```
节省空间模式 ▸
     ☑ 始终保留在此设备      （≈ pin=pinned → 状态「始终可用」）
     ⤓ 释放空间              （≈ dehydrate → 状态「仅在线」）
────────────────────────
     下载到本地…
     删除（仅在线文件删除前会二次确认）
     在文件管理器中打开
```

**释放空间（设置 → 释放空间）**
```
( ) 不自动释放
(•) 自动释放空间
      ( ) 当本地可用空间少于  [10]%
      (•) 按频率             [每天 ▾]
[立即释放空间]   已用 1.2 GiB / 限额 2 GiB  [========------]
被挡下：dirty 3 · 已保留 12 · 打开中 1 · 映射中 0 · 传输中 2
```
（术语与 Qsync 逐字对齐：`Free up space automatically` / `When space is less than` / `By frequency` / `Free Up Space Now`）

### 5.4 与现有验收的关系（**必须写死**）

* 现有 5 个 tab 的**全部字段与 id 保留**，只是移动位置 → `gui-matrix.sh` 的字段断言可复用。
* 新增页面**必须**纳入 `gui-matrix.sh`，逐页截图 + 非空白 + 页面间 AE 差异判定（沿用 M4 的做法）。
* `QSYNC_GUI_TAB` 的取值域从 5 个扩到 `home|tasks|files|journal|errors|settings|diag-*`，
  **旧值 `status|connect|mounts|files|sync` 必须继续可用**（映射到诊断页对应子页）。

---

## 6. 执行方案（里程碑）

> 全局约束：
> * 依赖方向不变：`gui → core`（+ 经 IPC 访问 daemon），**GUI 不发 HTTP、不自己挂 FUSE**。
> * 每条 IPC 改动都要**同时**改 `qxync-core/src/ipc.rs` 的 `Request`/`Response` 与 `IPC_VERSION` 兼容策略（现为 `v:1`，加字段用 `#[serde(default)]`）。
> * 每个里程碑**结束即跑**：`cargo test --workspace` + `fuse-matrix.sh` + `gui-matrix.sh`；涉及 M5/M6/M7 的再加对应矩阵。
> * 任一里程碑都**不许**触碰两条铁则（`read()` 不短读、脱水先 `inval_inode`）。

### M8.0 —— 设计与基线冻结（0.5 人日）

| 项 | 内容 |
|---|---|
| 交付物 | 本文档定稿 + `docs/M8-术语与文案.md`（§1.7 术语表 + 每页文案清单）+ `docs/M8-界面线框.md`（§5.3 扩展成逐控件线框） |
| 写域 | `docs/` |
| 验收 | 术语表覆盖 Qsync 教程里出现的**每一个** UI 字符串；线框里每个控件都标注了「数据来自哪个 IPC / 哪个字段」 |

### M8.1 —— 外壳：左侧导航 + 主页 + 诊断收纳（2 人日）—— ✅ **已完成并真机验收（2026-10-01）**

| 项 | 内容 |
|---|---|
| 交付物 | `ui/index.html` / `style.css` / `app.js` 重构为「左侧图标栏 + 页面容器」；新增「主页」；现有 3 个排障 tab 收进「诊断」的子 tab；未实现的目的地给**说明页**（不是空白） |
| 写域 | `crates/qxync-gui/ui/**`、**`xtask/tests/gui-matrix.sh`（同批改，已做到）** |
| 依赖 | 无（纯前端，**零 daemon 改动、零协议改动**） |

**实际落地的结构**（7 个一级目的地 + 诊断 3 子页）：

| 目的地 | 内容 | 状态 |
|---|---|---|
| 主页 home | 连接行 + **任务卡片列表**（现由挂载点派生，`taskState()` 做**诚实**归纳：拿不到证据就不说「已同步」）+ 最近一次同步摘要 + 快捷动作（连接设置 / 立即同步 / ＋添加任务） | ✅ |
| 任务 tasks | 同步任务列表 → 任务详情 | 🚧 说明页（M8.2） |
| 文件 files | 原「文件 / pin」页原样搬入（**元素 id 未变**） | ✅ |
| 更新 journal | 文件更新中心 | 🚧 说明页（M8.3） |
| 错误 errors | 错误列表 | 🚧 说明页（M8.3） |
| 设置 settings | 原「连接 / 登录」页搬入 | ✅ 部分（M8.4 补齐） |
| 诊断 diag | **专家模式**：原「状态/进度」「挂载」「同步/缓存」收纳为**子 tab**，**字段与 id 全部保留** | ✅ |

**与 Qsync 的**有意偏离**：左栏图标**保留了小号文字标签**（Qsync 6.x 只有图标）。
7 个目的地靠 tooltip 认不全，可发现性优先；已在 §1.1 与 `M4-GUI.md` §4.1 注明。

**验收结果（`xtask/tests/gui-matrix.sh`）**：

| 项 | M4 | M8.1 后 |
|---|---|---|
| 通过项 | 41/41 | **86/86** ✅ |
| 真窗口截图 | 5 个 tab | **9 个目的地**（home/tasks/files/journal/errors/settings + `diag:status`/`diag:mounts`/`diag:sync`），逐个断言标题/尺寸 1200x800/非空白（stddev 7333–8353 > 1500）/与主页差异（AE 15441–37235 > 4000） |
| **旧 `QSYNC_GUI_TAB` 值兼容（§3b 新增）** | — | status→`diag:status`、mounts→`diag:mounts`、sync→`diag:sync`、connect→`settings`、files→`files`，**截图 AE 2035–2318**（只差时钟/uptime）→ 落点等价 ✅ |

**同时新增 `diag:<status|mounts|sync>` 寻址语法**，让验收矩阵能直达诊断子页（旧值仍走 `LEGACY_TABS` 映射）。

**同步功能未被触及的证据（同批回归，全部绿）**：

```
fuse-matrix.sh  68/68      ← M1–M5 挂载/水合/写路径/变更发现/脱水/状态库
m5-matrix.sh    28/28      ← SQLite 状态库 + delta 编解码 + 能力门控
m6-matrix.sh    29/29      ← 多根 / 共享文件夹
m7-matrix.sh    60/60      ← 选择性同步 + LAN 配对/事件/直传
gui-matrix.sh   86/86      ← 本次改造的验收
```

**NAS 数据未被损坏的证据（见 §12）**：改造前后对 `/home` 全树做只读 manifest 比对 ——
**added 0 / removed 0 / resized 0**，只有 2 处 mtime 变化（`/home/.recent` 是 NAS 自己的索引，
`/home/qxync-test` 是目录 mtime 随测试子文件增删而变）。

| 风险 | 结论 |
|---|---|
| ~~这是全方案唯一会打破现有验收的里程碑~~ | ✅ **已化解**：矩阵与产品代码同一批改完，86/86 通过，且旧值落点等价 |

### M8.2 —— 任务模型 + 持久化（3 人日）—— ✅ **已完成并真机验收（2026-10-01）**

| 项 | 内容 |
|---|---|
| 交付物 | 新模块 `qxync-core/src/tasks.rs`（`Task` + 原子写 + 列表 + 校验，**11 项单测**）；`Request::Tasks{action,id,task}` 与 `Request::Mount{task,save_task}`；daemon 侧的登记/停用/恢复；`qsync task list\|add\|rm\|pause\|resume\|mount [--json]`；GUI「任务」页（列表 + 文件夹对设置 + 暂停/继续/挂载/删除登记）；主页任务卡改为**优先按任务展示** |
| 写域 | `crates/qxync-core/src/{tasks.rs(新),ipc.rs,lib.rs}`、`crates/qxync-daemon/src/{daemon.rs,main.rs}`、`crates/qxync-cli/src/main.rs`、`crates/qxync-gui/ui/**`、新 `xtask/tests/m82-matrix.sh` |

**关键设计决策（都是为了「不动同步」）**

| 决策 | 理由 |
|---|---|
| 任务层是**外壳**：只记录参数并驱动既有的 `mount()`/`umount()` | **FUSE 内部一行未改**；`mount()` 的函数体没动，只在 dispatch 分支加了「挂载成功后再落盘登记」 |
| `mount` **默认不登记任务**（要 `task` / `save_task` 才登记） | qsync CLI 与所有验收矩阵走的就是这条路 → **M7 行为一字不变**（矩阵里专门断言了这条） |
| daemon 恢复**默认关闭**，要 `--restore-tasks` / `QSYNC_TASK_RESTORE=1` | 恢复会「凭空挂载」；默认打开会让上次跑崩留下的挂载在重启时复活，打乱验收矩阵「开跑前环境干净」的前提 |
| 「暂停」= 停用登记 + **卸载该挂载点** | 同步引擎是**账号级**的（不是按挂载点循环），做「暂停但不卸载」必须改引擎 —— 那正是本次要避开的风险。卸载后该任务不再产生本地改动，且 `umount` 会先**排空已入队的上传**（不丢改动）；其它任务完全不受影响 |
| 验收/测试全用**私有 XDG 目录** | `m82-matrix.sh` 只碰 `.local-run/m82/`，不污染其它矩阵的状态 |
| `Task::default()` 刻意做成**不可保存** | 它是 serde 占位值；`normalize()` 会拒掉空挂载点，单测 `default_task_is_unsavable` 守住这条 |

**验收结果（新矩阵 `xtask/tests/m82-matrix.sh`，34/34）**：

| 验收项（计划里的 ①–④） | 实测 |
|---|---|
| ① 兼容：老配置行为与 M7 一致 | ✅ **普通 `mount`（不带 task）不登记任务**；`fuse-matrix.sh` 68/68 回归 |
| ② daemon 重启后自动恢复挂载 | ✅ `--restore-tasks --auto-login` 后 `$MNT1` 真的回到 `/proc/mounts`；**负向对照**：不开 flag 时 `mounted=false`（不复活） |
| ③ 暂停只影响自己 | ✅ 两个任务 t1/t2：`pause t1` → t1 卸载、**t2 仍然挂着**；`enabled` 分别落盘为 false/true |
| ④ CLI `--json` 可判定 | ✅ `task list/add/rm/pause/resume/mount --json` 全部可用 |
| 附加：安全 | ✅ 非法 id `../evil` 被拒且磁盘上没有乱建文件；`rm` 后任务数 2→1 且**挂载点数据完好** |

**同批回归（全部绿）**：

```
fuse-matrix.sh  68/68   ← ★ 挂载路径改过，这是最关键的回归
gui-matrix.sh   86/86
m5/m6/m7        28 / 29 / 60
cargo test      all passed
```

**NAS 数据零变化**（§12.3 同款比对）：added 0 / removed 0 / resized 0，仍是那 2 处可解释的目录 mtime。

| 风险（原计划） | 结论 |
|---|---|
| 挂载生命周期改动易碎 | ✅ **已化解**：`mount()`/`umount()` 函数体未改，只在 dispatch 层加登记；`fuse-matrix` 68/68 |

### M8.3 —— 同步日志（journal）+ 更新中心 + 错误列表（3 人日）—— ✅ **已完成并真机验收（2026-10-01）**

| 项 | 内容 |
|---|---|
| 交付物 | `sync.db` **schema v1 → v2**：新增 `journal` 表（3 个索引）；`JournalEntry` + `Store::{journal_add_batch, journal_list, journal_count, journal_counts, journal_clear, journal_trim}`；`Request::Journal{limit,since,query,level,clear}`；daemon 侧**内存缓冲 + 后台批量落库 + 周期轮转**；`qsync journal [--level] [--query] [--limit] [--since] [--clear] [--json]`；GUI「文件更新中心」（搜索/过滤/条数/清空）+「错误列表」（复制路径） |
| 写域 | `crates/qxync-core/src/{store.rs,ipc.rs,lib.rs}`、`crates/qxync-daemon/src/daemon.rs`、`crates/qxync-cli/src/main.rs`、`crates/qxync-gui/ui/**`、新 `xtask/tests/m83-matrix.sh`、**`xtask/tests/m5-matrix.sh`（同批改）** |

**几个关键决策**

| 决策 | 理由 |
|---|---|
| schema v1→v2 **不写迁移代码** | 整份 `SCHEMA_SQL` 都是 `CREATE TABLE/INDEX IF NOT EXISTS`，老库在下次 `Store::open()` 时自动补 `journal` 表；**已有表里的数据一行不动**（`m5-matrix` 与 `m83-matrix` 都有断言） |
| 热路径**只 push 到内存**，后台每 500ms 批量落库 | 同步轮 / 脱水都在热路径上；**绝不在那里开事务写库**（方案 §8 风险 3 的硬约束） |
| **必须有轮转**：默认 1 万条 / 30 天（`QSYNC_JOURNAL_MAX_ROWS` / `_MAX_AGE_DAYS` / `_TRIM_SECS` 可调） | 同步日志是无限增长型数据，不加约束会把 `sync.db` 撑大 |
| **只记有内容的项** | 默认 30 秒一轮 = 一天 2880 条；空轮询不写日志，否则日志被噪声淹没 |
| 错误列表 = `journal` 里 `status='error'` 的视图 | 不另建表，避免两份数据不同步 |

**验收结果（新矩阵 `xtask/tests/m83-matrix.sh`，27/27 + `m5-matrix` 扩到 30/30）**：

| 验收项 | 实测 |
|---|---|
| ① schema v1→v2 且不破坏数据 | ✅ 手工造一个 v1 老库（含 pin）→ 打开后 `user_version=2`、`journal` 表自动出现、**pin 原样保留** |
| ② 有活动才有日志 | ✅ **没挂载点跑 sync → 0 条**（对账无事可做）；挂载后跑一轮 → 出现 `scan` 记录 |
| ③ 过滤 | ✅ `--level error` 只回 error / `--level blocked` 命中脱水那条 / `--query` 同时匹配路径与说明 / `--limit` 生效 |
| ④ `--clear` 只清日志 | ✅ **游标 435→435 不变、baseline 不变、pin 没被清掉** |
| ⑤ 轮转 | ✅ `QSYNC_JOURNAL_MAX_ROWS=5` → 灌入 40 条后自动压回 **5 条** |

**同批回归（全部绿，共 337 项）**：

```
fuse-matrix 68 · gui-matrix 86 · m5-matrix 30 · m6-matrix 29
m7-matrix 60 · m82-matrix 37 · m83-matrix 27 · cargo test all passed
```

> ⚠️ **本次踩到的一个坑（值得写下来）**：中途有一次 `fuse-matrix` 报 66/68，
> 排查后确认是**我在矩阵运行期间并发执行了 `cargo build`**，把 `target/debug` 下的二进制换掉了。
> 复跑两次都是 68/68。**规矩：矩阵运行期间不许重新构建。**

**NAS 数据零变化**：added 0 / removed 0 / resized 0，仍是那 2 处可解释的目录 mtime。

### M8.4 —— 设置中心 + 托盘/通知 + 自动释放空间 + M7 面板补课（4 人日）—— ✅ **已完成并真机验收（2026-10-01）**

| 项 | 内容 |
|---|---|
| 交付物 | ① `settings.json` + `Request::Settings` / `SettingsSave`；② **代理**（`No proxy` / `Auto-detect` / `Manual` + 认证）接到 `reqwest`；③ **托盘图标**（4 项菜单）+ `tauri-plugin-notification` 桌面通知；④ **开机自启**（写 XDG autostart 桌面项）；⑤ **释放空间**页（`statvfs` + 定时器 + `Free Up Space Now`）；⑥ **筛选器设置 GUI**（编辑 `exclude` + `filter_temp` + `--match` 实时预览）；⑦ **LAN 加速页**（`peer_listen` / `peer_name` / 配对 / 已配对 / 最近事件）；⑧ **冲突策略下拉（5 选项）** + 「每个文件都问我」的待裁决队列；⑨ **文件选择器**（`tauri-plugin-dialog`）；⑩ 关于页 + 打开日志/配置目录 + File Station（`tauri-plugin-opener`）；⑪ ★ **文件页三态列 + 右键菜单**（`Always keep on this device` / `Free up space`） |
| 写域 | `crates/qxync-core/src/{settings.rs(新),freespace.rs(新),tasks.rs,store.rs,ipc.rs,lib.rs}`、`crates/qxync-client/src/lib.rs`、`crates/qxync-daemon/src/{daemon.rs,sync.rs}`、`crates/qxync-fuse/src/upload.rs`、`crates/qxync-cli/src/main.rs`、`crates/qxync-gui/{Cargo.toml,src/**}`、`crates/qxync-gui/ui/**`、新 `xtask/tests/m84-matrix.sh`、**`xtask/tests/{gui-matrix.sh,m5-matrix.sh,m83-matrix.sh}`（同批改）** |
| 依赖 | M8.1（页面壳）；⑥⑦ 依赖 M8.2（任务） |
| 验收 | ✅ 全部落地，见下方实测（新矩阵 `m84-matrix.sh` **88/88**） |

#### 落地形状

**① 全局设置（`~/.config/qsync/settings.json`，0600，原子写）**

```jsonc
{
  "version": 1,
  "proxy": { "mode": "auto|none|manual", "server": "", "port": null, "auth": false, "user": "", "password": "" },
  "launch_at_startup": false,          // → ~/.config/autostart/qsync.desktop
  "desktop_notifications": true,
  "debug_log": false,
  "language": "", "region": "",        // 记录位（文案仍只有 zh-CN）
  "free_space": { "auto": false, "mode": "below_pct|frequency", "below_pct": 10, "every_hours": 24 },
  "close_to_tray": true
}
```

* **默认值 = M8.3 的行为**：`proxy.mode=auto`（跟随 `http_proxy` 等环境变量 —— reqwest 本来就这么做）、
  不自动释放、不开机自启、`close_to_tray=true`。**没有 `settings.json` 时一切照旧**。
* 字段全部 `#[serde(default)]`：旧文件能读、新字段不丢，加设置不用写迁移。
* `qsync settings --set key=value`（可重复）走 `Settings::set_kv`：**未知键报错**，不静默忽略；
  多键赋值**顺序无关**（归一化只在 save 时做一次，否则「先 server 后 mode=manual」会把 server 抹掉）。

**② 代理：三种模式都接到 reqwest，并且「关得掉」**

* `Auto-detect` = 不设任何 proxy（reqwest 默认读环境变量）；
* `No proxy` = 显式 `.no_proxy()` —— 否则环境变量还在时根本关不掉（实测：同一台机器上
  `http_proxy` 还在，`No proxy` 必须**一个字节都不经过代理**）；
* `Manual` = `reqwest::Proxy::all(url)`（可选 `basic_auth`）；`server` 缺省或勾了认证却没填用户名 → **报错**，
  **绝不静默退回直连**（那是最危险的一种「静默」）。
* 生效范围：daemon 主客户端、FUSE 水合客户端、引擎客户端、CLI 直连，**全部**按设置构造。

**依赖（`Cargo.lock` 在本仓库是 gitignore 的，所以把 M8.4 新增的版本记在这里）**：
`tauri 2.12.1`（features `tray-icon` + `image-png`）、`tray-icon 0.25.1`（直接依赖，features `["ksni"]`）、
`ksni 0.3.6`、`tauri-plugin-notification 2.5.1`、`tauri-plugin-dialog 2.8.1`、`tauri-plugin-opener 2.7.0`。
⚠ `tray-icon` 那行是靠 Cargo feature 合并把 ksni 打开的（tauri 没有直通 feature）；
tauri 将来升到 0.26 时这行要跟着升，否则依赖图里会出现两份 `tray-icon`、feature 合并不上。

**③ 托盘 + 通知（`qxync-gui`）**

* 托盘用 **ksni**（纯 Rust StatusNotifierItem，不是 libappindicator）：名字是规范的
  `org.kde.StatusNotifierItem-<pid>-1`，D-Bus 上可被验收脚本直接断言；菜单 4 项
  （打开主窗口 / 立即与 NAS 同步 / 暂停 / 退出），前三项 emit `tray://action` 给前端做。
  `ldd target/debug/qxync-gui` 里**没有** `libayatana-appindicator` —— 走 ksni 后
  这条系统库依赖真的没有了（tray-icon 的 libappindicator 后端仍在依赖图里但被 cfg 排除）。
* ★ **「建起来」≠「用户看得见」**：SNI 的宿主是 `org.kde.StatusNotifierWatcher`，
  但「有 watcher」不等于「有 host」。创建后做一次探测（最多 8×250ms 重试）：
  ① watcher 在总线上；② `IsStatusNotifierHostRegistered == true`；
  ③ 本进程的 item 出现在 `RegisteredStatusNotifierItems` 里。三条都过才认定**托盘可见**，
  并写进 `m84_info` 的 `tray_visible` / `tray_reason`（GUI「关于」页如实显示三态：
  可见 / 不可见 / 探测中）。
* `CloseRequested` → 读设置里的 `close_to_tray`：为真**且托盘可见**才隐藏；
  否则照常关闭 —— 免得窗口藏进一个「没人画图标」的托盘后用户再也找不回来。
* 兼容性事实（写进 README 已知限制）：SNI 覆盖 Plasma / waybar / polybar / XFCE(statusnotifier) /
  LXQt / Cinnamon / GNOME+AppIndicator 扩展；**IceWM / Fluxbox / Openbox+tray / 老面板只有 XEmbed**，
  裸 GNOME 两个协议都不支持 —— 这两类环境下托盘不出现（可装
  [`snixembed`](https://sr.ht/~steef/snixembed/) 把 SNI 桥进 XEmbed 托盘），
  但 qxync 的窗口、通知、同步全都照常工作。
* 桌面通知尊重 `desktop_notifications`：关掉时 `notify_show` 与 `--self-test-notify` 都如实回 `shown=false`。
* 前端在轮询里每 ~6s 看一次 journal 的 `error` 视图，有新错误才发通知（成功项不发，避免噪声）。

**④ 自动释放空间：低空间触发，但**走的还是 M3 的安全检查链**

* `qxync-core::freespace`：`statvfs` + 纯函数判定（`below_pct` / `frequency`）。
* daemon 后台任务（默认 60s 一次，`QSYNC_AUTO_FREE_INTERVAL` 可调）判定该不该跑，
  **判定结果翻译成一次 `run_dehydrate_with_recent`** —— 也就是与手动脱水**同一条路**：
  dirty / 待上传 / `pin=pinned` / `excluded` / 打开中 / mmap / 传输中 一律跳过并计入「被挡下」。
  方案 §8 风险 5（自动释放绕过铁则）由此**闭合**：没有任何旁路可绕。
* 「按频率」也留了耐心值（闲置阈值下限 1 小时），不会把「能脱的全脱掉」。
* ⚠ **已知限制（不影响验收）**：「按频率」的「上次触发时间」只在内存里，
  daemon 重启会重新计时（也就是重启后可能立刻跑一次）。要跨重启记住它得把时间戳落进状态库，
  留给 M8.6；「当空间少于 X%」这一模式不受影响（它只看当前可用空间）。
* 踩到的坑：`below_pct` 模式下若「需要腾出的字节 > 缓存占用」，算出来的限额是 0，
  而 `CacheLimit::parse("0")` 判为无效写法 → **整轮变成空操作**（日志里 0/0/0）。
  已夹到最小 1 字节（语义 = 能清多少清多少），矩阵里有断言守着。

**⑤ 冲突策略（5 选项，逐字对齐 §1.7）**

| 策略 | 行为 | 谁丢数据 |
|---|---|---|
| `ask` 每个文件都问我 | 两边都不动，写 `decisions` 表排队，GUI 逐个裁决 | 都不丢 |
| `rename_remote` 重命名 NAS 上的文件 | 服务端 `rename` 成冲突副本，本地内容占原名 | 都不丢 |
| `rename_local` 重命名本地文件（**默认**） | 本地另存副本并上传，远端占原名 | 都不丢 |
| `replace_remote` 用本地替换 NAS 上的 | 本地内容覆盖远端 | **远端那份** |
| `replace_local` 用 NAS 上的替换本地 | 远端覆盖本地 | **本地那份** |

* 默认选 `rename_local`，因为它**就是 M2c 的硬编码行为**（远端占原名、本地另存
  `xxx (conflicted copy from …)`）→ 没配过策略的任务行为一字不变。
* 策略挂在**任务**上（`Task.conflict`），`mount` 也接受可选的 `conflict`；
  未知取值落回默认。
* `ask` 队列落 `sync.db` 的 `decisions` 表（**schema v2 → v3**，仍然只是
  `CREATE TABLE IF NOT EXISTS`，老库自动补表、不写迁移代码）；同一路径只留一条，
  已裁决的条目不因为再次冲突而丢掉用户的表态。裁决后由下一轮同步执行并出队。
* 顺手修掉一个真问题：`UploadQueue::cancel()` 原本只能取消「还没被 worker 取走」的作业，
  **已经取走但还没发出去**的那个取消不了。现在 worker 在发请求前查一次取消集合
  （`cancel` 会登记），M2c「cancel + drain + 重新 stat」的窄窗口被关掉。

**⑥⑦ 筛选器 / LAN 面板**：编辑的是 link 的 `exclude` + `filter_temp`、`peer_listen` + `peer_name`
（都复用既有 `link_save` 命令，M7 的后端一行未改）。筛选器有 `--match` 实时预览；
LAN 面板显示身份/监听/配对码/已配对设备/事件计数，并支持一次配对。

**⑪ 文件三态 + 右键菜单**：三态直接来自 FUSE 的 `FsHandle::candidate()`（已缓存字节 + pin），
不新增任何后端状态：`仅在线` ← `hydrated_bytes == 0`、`本地可用` ← 有缓存、`始终可用` ← `pin=pinned`。
右键菜单：始终保留在此设备 / 取消固定 / 释放空间 / 下载 / 复制路径 / 删除。

#### 验收结果（新矩阵 `xtask/tests/m84-matrix.sh`，88/88）

| 验收项（计划里的 ①–⑦） | 实测 |
|---|---|
| ① 代理三模式 | ✅ **假代理（`nc -l`）真的看到了 `CONNECT nas.example.com:9834`**（Auto-detect 走环境变量、Manual 走配置）；`No proxy` 时环境变量还在也**一个字节都没经过代理**且登录成功；**代理停掉后 Manual 登录失败（退出码 3）**，切回 `No proxy` 立刻恢复 |
| ② 托盘 | ✅ 真窗口起来后 D-Bus 上出现 `org.kde.StatusNotifierItem-<pid>-1` 且 watcher 已登记；**可见性探测通过**（watcher + `IsStatusNotifierHostRegistered=true` + 本进程 item 已在列表里，日志与 `m84_info` 一致）；dbusmenu 的 4 项文案与二进制一致；`wmctrl -c` 关窗后**窗口不可见、进程仍在**（进了托盘）；`close_to_tray=false` 时关窗即退出；**负向对照**：用 `dbus-run-session` 起一根没有 watcher 的私有会话 → 托盘创建失败 → 关窗**真的退出**（不会把窗口藏起来） |
| ③ 通知 | ✅ `dbus-monitor` 抓到 `member=Notify`（正路径）；**关掉设置后 `shown=false` 且 dbus 上一个 Notify 都没有**（负向对照） |
| ④ 自动释放空间 | ✅ `QSYNC_TEST_FAKE_STATVFS=avail_pct=5` → 判定触发 → 普通文件自动回到**仅在线**；**`pin` 住的文件仍是始终可用且 131072 字节内容原样**（安全链挡下，没被绕过）；journal 有「自动释放空间」记录；非法注入值**报错**不静默 |
| ⑤ 筛选器 | ✅ 加 `*.iso` → `rules --match` 判 `excluded`，且**真挂载点里看不到** hidden.iso、看得到 visible.txt |
| ⑥ 冲突策略五选 | ✅ 五个取值各跑一次真挂载三向冲突：`rename_local`（远端占原名 + 副本=本地内容）、`rename_remote`（原名=本地内容 + 副本=远端内容）、`replace_remote`（远端变成本地内容、无副本）、`replace_local`（远端不变、本地被替换、无副本）、`ask`（**远端原样、本地仍是用户自己的内容**；队列 pending=1 → 裁决 `keep_local` → 下一轮远端变成本地内容 → 出队） |
| ⑦ 节省空间模式三态 + 铁则 2 | ✅ 新建 → 仅在线；`head -c` → 本地可用；`pin` → 始终可用；释放空间 → 回到仅在线（0 字节）；**脱水后把远端改成同长度不同内容再 `cat` 拿到新内容**（`BBBB-2222`） |
| 附加：设置本体 | ✅ 默认值、往返、**0600**、`--set` 未知键报错、`manual` 缺服务器被 daemon 拒绝（磁盘上不留半截写入）、autostart 桌面项写/删 |

#### 确定性冲突怎么测出来的（值得记一笔）

端到端跑三向冲突本来是**测不稳**的：写路径是写穿的，「本地已改但还没上传」的窗口只有几十毫秒。
矩阵里加了 `QSYNC_TEST_UPLOAD_HOLD_MS`（**默认 0，仅验收用**）让上传 worker 在发请求前停一会儿；
配合上面那条「取消集合」的修复，`cancel + drain + 重新 stat` 就能稳定地把冲突判出来。
另一个坑：每个策略跑完必须把远端文件**删掉并跑一轮 sync** 清掉 baseline，
否则下一个策略的挂载会用陈旧 baseline 把上一个文件重新判成冲突（矩阵初版就被这个坑到，
表现为「ask 队列里凭空多一条别的文件的待裁决」）。

#### 与现有验收的关系

* **旧 `QSYNC_GUI_TAB` 取值一字未改**，新增 `settings:<连接|代理|同步与筛选|个人|高级|释放空间|LAN|关于>` 直达写法；
  `gui-matrix.sh` 追加 7 个设置分区截图（每个分区与「连接」分区、与上一个分区都要 AE > 4000），
  并在自检里加了 4 条 M8.4 数据源断言（`settings` / `space` / `file_states` / `conflicts`）。
* `m5-matrix.sh` 与 `m83-matrix.sh` 的 schema 断言随 v3 一起改（**只改了一个数字**，
  「老库自动补表且数据不动」的断言一条没动）。

| 风险（原计划） | 结论 |
|---|---|
| 自动释放空间绕开脱水安全检查链 | ✅ **已化解**：判定与执行分离，「执行」永远走 `run_dehydrate`；矩阵专门断言 pin 住的文件仍被挡下、内容原样 |
| 托盘/通知/自启在 Wayland 与 X11 行为不同 | 🟡 **部分化解**：托盘走 StatusNotifierItem（D-Bus），与 X11/Wayland 无关；截图/`wmctrl`/`xdotool` 仍在 `GDK_BACKEND=x11` 下做（**只影响验收脚本**）；`libappindicator` 的告警通过换 ksni 后端一并消掉 |

### ~~M8.5 —— 备份任务~~ 🚫 已决策不做（2026-10-01）

**整条移出本方案。** 决策理由与遗留记录：

* 它是 Qsync 6.0 的旗舰新功能，但也是最贵的一块（+5 人日 + 一个全新的单向引擎 + 队列隔离 + 调度器），
  而 qxync 的定位是 **Linux 按需同步**，备份场景已有更合适的工具（HBS 3 / rsync / restic）。
* **连带取消**：备份任务页、`Frequency Settings`、`Advanced Settings`（备份排除）、
  `Request::Backup`、`crates/qxync-daemon/src/backup.rs`、`xtask/tests/m8-backup-matrix.sh`、
  以及「恢复文件 / 时间点还原」这条依赖它的链路（见 §4.3）。
* **保留的动作只有一个**：首次运行向导里 **Backup Task 卡片保持可见但不可用**，
  点击给出「本客户端暂不支持备份任务」的说明 —— 让 Qsync 用户一眼看懂缺什么。

### M8.6 —— 打磨与验收收口（2 人日）✅ **已完成（2026-10-01）**

| 项 | 内容 |
|---|---|
| 交付物 | ① 视觉规范落地（浅色卡片 + 状态色 + 徽章 + 图标；深色模式同步维护）；② 键盘可达性与焦点环；③ 空态/错误态/加载态逐页检查（M4 踩坑 #1 `[hidden]` 与 #2 首轮 status 的同类问题全库复查）；④ **i18n（可选）**：抽出 zh-CN 文案表，预留 en；⑤ `gui-matrix.sh` 覆盖全部新页面；⑥ `README.md` / `docs/开发规划.md` 同步 |
| 写域 | `crates/qxync-gui/ui/**`、`xtask/tests/gui-matrix.sh`、`README.md`、`docs/` |
| 验收 | ① 全矩阵绿：`fuse-matrix.sh` 68/68、`gui-matrix.sh`（新项全绿）、`m5/m6/m7-matrix.sh`；② 每个新页面都有真窗口截图 + 非空白 + 两两 AE 差异；③ 无 `innerHTML` 拼接外部字符串（沿用 M4 的安全约定） |

#### 实到了什么（逐条对照上面的交付物）

| 交付物 | 落地 |
|---|---|
| ① 视觉规范 | `style.css` 顶部一组 token（层次 `--bg/--bg-sunken/--bg-elev/--bg-head`、文字三档、状态四色 + `-soft`、`--accent`、形状 `--radius/--radius-lg/--radius-pill`、浮层 `--shadow/--shadow-lg/--overlay`）；**新增 `--focus` / `--focus-halo` / `--overlay` / `--shadow-lg` / `--code-bg` 全部同时给了深色值**（`ui_spec.a11y.dark_tokens` 断言「浅色与深色两份都有」）；任务卡角标改用语义类 `badge-sync` / `badge-backup`；补 `prefers-reduced-motion` |
| ② 键盘可达性 | 「跳到主内容」skip-link（`:focus-visible` 才显形）；全局 `:focus-visible` 焦点环（按钮/tab/图标栏/菜单项/表格行 `:focus-within` 都有）；图标栏 ↑↓←→ + Home/End；设置分区 / 诊断子页 `role=tablist` + `aria-selected` + ←→ 跟随焦点；弹窗 `role=dialog aria-modal` + **Tab 焦点陷阱** + 关闭时**焦点归还**；文件行右键菜单键盘化（Shift+F10 / 菜单键打开、↑↓/Home/End、Escape/Tab 关闭并把焦点还给触发行）；日志面板头 `role=button tabindex=0 aria-expanded` + Enter/Space 折叠；`aria-current="page"` 标当前页 |
| ③ 空/错/加载四态 | 新增 `setListState()`（`loading`/`error`/`empty`/`ready`，写 `data-state` 供断言）+ `setBusyState()`（容器 `aria-busy`），**10 个列表全部接入**：主页任务、任务页、待裁决、文件、更新中心、错误列表、挂载（诊断 + 状态页两张表）、远端根、LAN 设备/事件、释放空间、设置读取失败。**修掉的就是 M4 踩坑 #2 的同类**：首轮数据没回来时不再写「不可用（daemon 未运行？）」，而是「正在读取…」；读取失败给出失败原因；文件页另外区分「daemon 没跑 / 没登录 / ls 真失败」三种情况 |
| ④ i18n | 新增 `ui/i18n.js`：**zh-CN 文案表 163 条**、`en` 预留（空表 = 整条回落到 zh-CN）、`{name}` 具名插值、`data-i18n` / `data-i18n-title` / `-placeholder` / `-aria-label` 四种静态挂钩 + 启动时 `apply()`；JS 侧动态文案走 `T(key)`。**不引入 i18n 框架**（方案 §8 风险 10 的口径）。范围划清：`logErr/logInfo` 的诊断日志与 `index.html` 里带内联 `<code>` 的混合标记段落**不进表**（写进 `i18n.js` 头注释） |
| ⑤ 矩阵覆盖 | `gui-matrix.sh`：新增 2c（`ui_spec` 静态合规性 14 条 + 文案表运行时回落 1 条）+ 3c（真窗口 Tab 焦点断言 2 条）→ **148/148**；9 个目的地 + 8 个设置分区 + 5 个旧 tab 值的截图与 AE 断言全部保留 |
| ⑥ 文档 | `README.md`（现状表 + M8.6 验收结论）、`docs/开发规划.md`（M8 进度块）、本文（§7 矩阵表、§9 排期、§12.7 数据安全） |

#### 验收结果

| 项 | 实测 |
|---|---|
| `gui-matrix.sh` | ✅ **148/148**（0 失败，含 2c 的 15 条静态断言与 3c 的 Tab 焦点 AE 2918 px） |
| `ui_spec`（随 `--self-test`，不开窗口） | ✅ 无 HTML 拼接（`innerHTML`/`outerHTML`/`insertAdjacentHTML`/`document.write` 全 0）；文案表 163 条、`T()` 用到 124 条、DOM 挂 58 条，**两边缺词都是 0**；焦点环/skip-link/tablist+tabpanel/dialog/aria-busy/aria-current/reduced-motion/深色 token 全绿；四态入口 + 13 个 `data-state` 标记 |
| 键盘可达性（真窗口） | ✅ 窗口起来后发一次 Tab → 画面变化 **AE 2918 px**（skip-link 显形 + 焦点环），截图 `.local-run/gui-shots/kbd-{before,after}.png` |
| 回归矩阵 | 见 §12.7 与 `README.md`「M8.6 验收结论」 |
| 负向对照 | ✅ daemon 停掉后 `--self-test` 仍**非 0 退出**（自检没被新增的静态断言稀释成橡皮图章） |

> ⚠️ 环境限制（如实记录）：本次验收所在沙箱**没有 `/dev/fuse`**（`nodev fuse` 在 `/proc/filesystems` 里，但设备节点缺失且无 `CAP_MKNOD`/`sudo`），
> 所以 `fuse-matrix.sh` 与各矩阵的**真挂载段**这次跑不了 —— 它们是「跳过」而不是「通过」。
> 这与 M6/M7/M8.4 当年「拿到 `/dev/fuse` 后补跑」的情况一样，属于环境而非代码；有 `/dev/fuse` 的机器上重跑即可。
> **M8.6 的写域只有前端资源 + 矩阵 + 文档，零后端/协议/FUSE 改动**，因此不动铁则 1（`read()` 不短读）与铁则 2（脱水先 `inval_inode`）。

---

## 7. 验收矩阵与回归清单

| 矩阵 | 现在 | M8 后 |
|---|---|---|
| `xtask/tests/fuse-matrix.sh` | 68/68 | ✅ **保持 68/68**（M8.2–M8.4 每次都复跑；M8.4 触碰了上传队列的取消语义，也在此回归里） |
| `xtask/tests/gui-matrix.sh` | 41/41（5 tab 截图） | ✅ **M8.1 后 86/86**（9 个目的地 + 旧 tab 落点等价）；**M8.4 后 131/131**（+7 个设置分区截图、两两 AE 差异 + 4 条 M8.4 数据源断言）；**M8.6 后 148/148**（+15 条 2c 静态合规性断言（`ui_spec` + 文案表运行时回落）、+2 条 3c 真窗口键盘断言；截图项一条未减） |
| `xtask/tests/m5-matrix.sh` | 28/28 | ✅ **30/30**（+schema 补表断言，M8.4 起断言 v3） |
| `xtask/tests/m6-matrix.sh` | 29/29 | 不变（多根语义不动） |
| `xtask/tests/m7-matrix.sh` | 60/60 | ✅ **保持 60/60**（后端规则一行未改；GUI 侧的规则编辑面板复用同一批命令） |
| `xtask/tests/m82-matrix.sh` | — | ✅ **新增 37/37**（M8.2：登记/兼容/重启恢复/暂停隔离/安全/缓存目录） |
| `xtask/tests/m83-matrix.sh` | — | ✅ **新增 27/27**（M8.3：schema 迁移/过滤/clear 隔离/轮转；M8.4 把 schema 断言改到 v3） |
| `xtask/tests/m84-matrix.sh` | — | ✅ **新增 92/92**（M8.4：设置/代理三模式+假代理/托盘可见性+通知/自动释放空间+安全链/筛选器/冲突策略五选/三态+铁则 2） |

**门（Gate）**：每个里程碑的 PR 必须在**同一 PR 内**同时更新受影响的矩阵脚本；只改产品代码不改矩阵 = 不允许合并。

> ★ M8.6 追加的判据不是截图而是 **`ui_spec`**：它随 `qxync-gui --self-test` 一起产出，
> 扫的是 `include_str!` **打进二进制的那份 UI**（不是工作区文件），因此「自检绿」= 交付物合规。
> 这一条同时守住了 M4 的安全约定（无 `innerHTML` 类拼接）——**新增界面代码时如果拼了 HTML，自检直接红**。

---

## 8. 风险清单

| # | 风险 | 等级 | 应对 |
|---|---|---|---|
| 1 | 改导航打破 `gui-matrix.sh`，回归失去判据 | 🔴 | M8.1 与矩阵改造同 PR；旧 `QSYNC_GUI_TAB` 值保持可用 |
| 2 | 挂载点从内存态改为任务持久化，引入「重启后挂载状态错乱」 | 🔴 | 只加外壳不改 FUSE；新增「daemon 重启后任务恢复」专项断言；`fuse-matrix.sh` 每次回归 |
| 3 | journal 写库拖慢热路径 / 撑大 `sync.db` | 🟠 | 异步批量落盘；条数 + 天数双重上限 + 轮转；绝不在 `read()` 路径同步写 |
| 4 | 备份任务与双向上传队列互相挤占 | 🟠 | 队列隔离或优先级，**先定再写** |
| 5 | 自动释放空间绕开脱水安全检查链 | 🔴 | 复用 M3 安全检查链，**不得新增旁路**；验收里专门断言 blocked 分类仍然生效 |
| 6 | ~~设备注册（`qbox_save_device_config`）协议未验证，写错可能污染 NAS 设备列表~~ | ✅ **已关闭** | P0 探针只读阶段即得出结论（§11）：假设证伪 → **不实现**，**未对 NAS 做任何写操作**，风险归零 |
| 7 | 版本还原期望落空（NAS 无历史版本） | 🟠 | 先 re-probe NAS 侧 `Enable version control`；仍不可用再走客户端版本桶 / File Station 深链；**待决策** |
| 8 | Qsync 截图/教程是版权内容 | 🟢 | 只做**功能与信息架构**对齐，不复制图标/位图/字体；自绘 SVG 或系统图标 |
| 9 | 多 NAS / 多 link 切换要重启 daemon，体验割裂 | 🟡 | 明说「切换连接会重启守护进程」；或 M9 做单 daemon 多 link |
| 10 | i18n 引入成本失控 | 🟡 | 标为可选项；先把文案集中到一个表，不引入 i18n 框架 |
| 11 | **「对齐」把 qxync 已有的强项做退化** | 🔴 | 三处必须守住：① 筛选器**能剪枝整棵子树**（Qsync 6.x 用户报告已不能过滤文件夹名）；② 脱水的 blocked 安全检查链；③ `read()` 不短读。每一条都在验收里有独立断言 |
| 12 | 照抄 Qsync 术语但语义对不上，反而更误导 | 🟠 | 术语表（§1.7）逐条标注「已实现 / 近似 / 不做」；对标不上的一律加一句说明（例：多功能文件夹只读 → 显式标「只读」并解释原因），**不做同名不同义的假对齐** |
| 13 | 「智能删除」与「节省空间模式」在 qxync 里并非真的互斥，UI 照抄会出错 | 🟠 | 先定义清楚两套语义在 FUSE 下的真实关系（节省空间模式 = 脱水；智能删除 = 删除熔断/回收），再决定 UI 是否照抄互斥提示 |

---

## 9. 工作量与排期

| 里程碑 | 人日 | 依赖 | 可并行 |
|---|---|---|---|
| M8.0 设计与基线 | 0.5 | — | — |
| M8.1 外壳 + 主页 + 诊断收纳 | 2 | M8.0 | — |
| M8.2 任务模型 + 持久化 | 3 | M8.1 | 与 M8.3 部分并行（后端/前端可拆） |
| M8.3 journal + 更新中心 + 错误列表 | 3 | M8.2 | 同上 |
| M8.4 设置中心 + 托盘/通知 + 自动释放 + M7 面板 | 4 | M8.1（⑥⑦ 还需 M8.2） | 内部可拆 4 条并行 |
| M8.6 打磨与收口 | 2 | 全部 | ✅ **已完成**（2026-10-01，`gui-matrix` 148/148 + `ui_spec` 全绿；见 §M8.6 与 §12.7） |
| **M8 合计** | **14.5** | | |
| **P0 探针线**（§11，与 M8 解耦） | ✅ **已完成**（只读阶段，0.5 人日） | — | 已关闭，无后续 |
| **M8.1 外壳** | ✅ **已完成**（2026-10-01，含矩阵改造与数据安全比对） | — | — |
| **M8.2 任务模型** | ✅ **已完成**（2026-10-01，`m82-matrix.sh` 37/37；fuse 回归 68/68） | — | — |
| **M8.3 同步日志** | ✅ **已完成**（2026-10-01，`m83-matrix.sh` 27/27；`m5-matrix` 扩到 30/30） | — | — |
| **M8.4 设置/托盘/释放空间/冲突策略** | ✅ **已完成**（2026-10-01，`m84-matrix.sh` 92/92；全量回归见 §7/§12） | — | — |
| **M8 剩余** | **0** | M8.0–M8.4 + M8.6 全部完成 | M8.5 已决策不做 |

---

## 10. 决策记录与剩余待决项

### 10.1 已拍板（2026-10-01）

| # | 议题 | 决策 | 对方案的影响 |
|---|---|---|---|
| 1 | 备份任务（原 M8.5） | 🚫 **不做** | M8.5 整条移除；连带取消备份任务页、`Frequency Settings`、`Advanced Settings`、`Request::Backup`、`backup.rs`、`m8-backup-matrix.sh`、「时间点还原」链路。**首次向导里 Backup Task 卡片保留但不可用**（给出说明，让用户看懂缺什么）。工作量 19.5 → **14.5 人日** |
| 2 | 版本还原 / 以前版本 | 🚫 **不做** | 客户端版本桶、`View previous versions`、NAS 侧 `Enable version control` re-probe **全部取消**。文件页只保留通用「在 File Station 打开」深链作为出口 |
| 3 | `qbox_save_device_config` 设备注册 probe | ✅ **可以（做）→ 已执行完毕** | P0 探针线（§11）**只读阶段即得出结论**：假设被证伪。**未写 NAS**；设备注册不实现；GUI「此设备名称」降级为只读。M2c 的 baseline 主路径被强化 |
| 4 | 「诊断（专家模式）」默认收起 | ✅ 采默认建议 | 默认收起，`QSYNC_DEBUG=1` 或设置里一个开关展开（写进 M8.1 验收） |

### 10.2 仍待决（不阻塞 M8.0 / M8.1 开工）

1. **备份任务取消后，首次向导还要不要保留两卡片形态？** 建议保留（Sync 可用 / Backup 灰置 + 说明）。
2. **i18n 做不做？** 建议 M8 之内不做，只把文案集中成表。
3. **「选择性同步」怎么落地？** Qsync 是「配对文件夹设置里勾选子文件夹」。qxync 的等价物是
   「多任务 / 多根」。建议：**M8.2 先做「一个任务多个配对文件夹」，勾选式 UI 后置到 M9**，
   不要为了抄这一个交互而把 roots 模型推倒重来。
4. **对齐到哪一版？** 建议：**以 Windows 6.1 的界面与术语为准**（唯一完整的 6.x 实现），
   但**不承诺任何 Qsync Central 侧能力**（团队文件夹 / 分享链接 / 版本还原 / QID）。
   同时在 README 里写清「macOS 官方仍是 5.1.x、Ubuntu 仍是 1.0.x」这一事实 —— 这本身就是 qxync 的存在理由。

---

## 11. P0 探针线：设备注册 —— **已执行，假设被证伪（2026-10-01）**

> 状态：**✅ 已完成（只读阶段即得出结论，未对 NAS 做任何写操作）**。
> 工具：`report/probe/p0_device_probe.py`（新增，复用 `qs_probe.py` 的登录链）。
> 原始响应：`report/probe/probe-out/p0-*/`（已 gitignore）。

### 11.1 原本的假设

M2c 的实测结论是「我们自己的 CGI 写操作不产生 sync log 事件」，当时的归因是
**「本机未做设备配对」**。于是假设是：

> 只要用 `qbox_save_device_config` 把本机注册成 Qsync 设备，我们写入产生的事件就会出现在
> `qbox_get_sync_log` 里 → 变更发现从「30s 轮询」升级到近实时。

### 11.2 实测结果（全部只读）

| # | 检查 | 结果 |
|---|---|---|
| 1 | `qbox_get_device_config_list&user=test1` | **已有一台注册设备**：`device_name="win-pc"`、`device_uid=01234567…567`、`modify_time`=2026-09-30 11:46 |
| 2 | 这台设备是谁注册的 | **官方 Qsync 客户端**（用户确认：是之前用官方客户端时注册的，不是 qxync） |
| 3 | `qbox_get_device_config&device_uid=01234567…` | `total: 0`、`config: []` —— 该设备**没有任何配对文件夹配置** |
| 4 | `qbox_get_syncing_folder_list` | **`total: 0`** —— 本账号**从未登记过同步文件夹** |
| 5 | `qbox_get_sync_log` 全区间（0–400 / 75–331 / 331） | **恒 `status:-17`**（区间内无事件） |
| 6 | `qbox_get_sync_log` 全参数变体（`device_uid`/`duid`/`uid`/`user`/`get_detail`/`sub_folder`） | **恒 `status:-17`** —— 参数对结果**没有任何影响** |
| 7 | `qbox_query_notify&lower=0&upper=177` | **`count:0, event:[]`** |
| 8 | `utilRequest.cgi` 命名空间下的同名 func | `status:20`（不支持）；只有 `qsyncsrv.cgi` 可用 |
| 9 | 208 条端点清单里有没有「注册同步文件夹」的端点 | **没有** |
| 10 | 回滚手段 | `qbox_reset_device_config(sid,user,device_uid,mode)` 存在；本次只读尝试 `mode=0` 返回 `status:-2`（**未修改任何东西**） |

### 11.3 结论

**假设被证伪：设备注册不是缺失的那一环。**

* 设备**早就注册了**（官方客户端留下的 `win-pc`），事件**照样拿不到**；
* `qbox_get_sync_log` 不是「没给对参数」——**换任何参数都是 `-17`**；
* 真正的闸门是代码注释里写对的那句：**「只有路径落在『已注册的同步文件夹』里才会真正出现在 sync_log」**，
  而 `qbox_get_syncing_folder_list` 是空的；
* 而**现有逆向成果里根本没有创建同步文件夹的端点**（那是官方客户端在 Qsync Central 里配对时才建的）。

→ **这条路在当前逆向成果下走不通。** 失败的探针同样有价值：它把一个反复出现的猜测**永久关掉**了。

### 11.4 对方案的影响

| 影响 | 说明 |
|---|---|
| **M2c 设计被强化** | 「**baseline 对账是主路径，事件只是快路径**」这个既有权衡现在有了更强的证据：事件快路径在这台 NAS 上**根本不工作**。**不要**为它做任何优化或预留。 |
| **不做设备注册** | `qbox_save_device_config` **不再实现**。它唯一的剩余价值（让本机出现在 Qsync Central 设备列表里）已由官方客户端注册的 `win-pc` 覆盖，收益不足以承担写 NAS 设备列表的风险。 |
| **§5.1 「计算机名称」降级** | GUI 的「此设备名称」改为**只读展示**（显示 `device_config_list` 里已有的设备名），不再提供编辑/注册。 |
| **已更正的文档** | `README.md` 踩坑 #20、`docs/M2c-变更发现.md` §9 —— 两处「本机未做设备配对」的归因**已改为上述实测事实**。 |
| **若将来仍想做** | 唯一路径是**逆向「同步文件夹配对」端点**（不在现有 208 条清单里）。成本高、收益不确定 → 建议列入「不做」。 |

### 11.5 复现方式

```bash
cd report/probe
# P0.1 只读侦察（设备列表 / nas_uid / max_log / qbox_info）
python3 p0_device_probe.py --host nas.example.com --port 9834 --https --insecure \
        --user test1 --password '***' --phase a
# P0.4 sync log 区间扫描（全区间 + 全参数变体）
python3 p0_device_probe.py --host nas.example.com --port 9834 --https --insecure \
        --user test1 --password '***' --phase d --max-log 331
# P0.2 影子写入（--phase b）**有意未执行**：决定性结论已在只读阶段得出。
```

---

## 12. 数据安全基线：怎么证明「没把 NAS 上的数据干坏」

> 这是 M8 全过程的**硬约束**（用户要求：「确定同步功能别出问题，别把现有的数据干坏」）。
> 做法不是「小心一点」，而是**可复现的证据链**。

### 12.1 工具

`report/probe/nas_manifest.py`（新增，**只读**）：

```bash
# 立基线（递归列 /home 全树：路径 + 大小 + mtime + 类型）
python3 nas_manifest.py --host <NAS> --port 9834 --https --insecure \
        --user test1 --password '***' --roots /home --out .local-run/nas-baseline.json

# 事后比对（退出码 0 = 完全一致；1 = 有差异并逐条列出）
python3 nas_manifest.py ... --roots /home --out .local-run/nas-after.json \
        --diff .local-run/nas-baseline.json
```

判据分四类，**危险程度从高到低**：`removed`（删除）> `resized`（内容变了）> `added` > `retimed`（只改 mtime）。

### 12.2 使用规则（每个里程碑都要走一遍）

1. **任何写操作/测试之前**先跑一次 manifest（立基线）；
2. 跑完回归/验收**之后**再跑一次并 `--diff`；
3. `removed` 必须为 0 —— **只要有一条未预期的删除，就当成事故处理**（停止、定位、恢复）；
4. `resized` 必须为 0（测试夹具的预期改动除外，且要在记录里点名）；
5. `retimed` 允许存在，但要能逐条解释；
6. 基线文件放 `.local-run/`（已 gitignore），不提交。

### 12.3 M8.1 的实测结果

| 项 | 结果 |
|---|---|
| 采集范围 | `/home` 全树 16 项（`qxync-test/` 夹具 + `big.bin` 128 MiB + 深层目录 + 中文/空格文件名 + `.recent` + `@Recycle`） |
| 改造期间跑过的测试 | `fuse-matrix` 68 + `gui-matrix` 86 + `m5` 28 + `m6` 29 + `m7` 60，**共 271 项**（矩阵本身会在夹具里建/改/删文件，然后自行清理） |
| **added** | **0** |
| **removed** | **0** ← 最关键的一条 |
| **resized** | **0** ← 没有任何已有文件的内容被改 |
| retimed | 2：`/home/.recent`（NAS 自己的「最近访问」索引）、`/home/qxync-test`（目录 mtime 随子文件增删而变）—— 均为预期且无害 |

**结论：16 项已有数据全部完好，没有任何删除、没有任何大小变化，只留下 2 个可解释的目录级 mtime 变化。**

**M8.2 / M8.3 后各再比对一次**（基线 = 上一个里程碑之后的 manifest）：两次都是 `added 0 / removed 0 / resized 0`，
仍是同样那 2 处 mtime。M8.2 会真实挂载/卸载 FUSE，但全程只用私有 XDG 目录与自建挂载点，
**没有对 NAS 产生任何写操作**。

### 12.4 为什么这次改动本身不可能伤到 NAS

M8.1 的写域是 `crates/qxync-gui/ui/**` + `xtask/tests/gui-matrix.sh`：

* **零 daemon 改动、零协议改动、零 FUSE 改动** —— GUI 只经 IPC 读状态，不改传输路径；
* 两条铁则（`read()` 不短读、脱水先 `inval_inode`）的实现代码**一行未动**；
* 所有写 NAS 的行为都只发生在 `qxync-test/` 夹具目录内，且由既有矩阵自己清理。

### 12.5 后续里程碑的额外约束

* **M8.2（任务持久化）会碰挂载生命周期** —— 这是第一个可能影响同步的里程碑。
  规则：**只加「任务外壳」，不改 `mount`/`umount` 与 FUSE 内部**；并入后必须复跑 `fuse-matrix` 68/68 + manifest 比对。
* **M8.3（journal 落库）会写 `sync.db`** —— 属于本地状态库，不碰 NAS；但**迁移必须幂等**（`m5-matrix` 已有断言），且不许在 `read()` 热路径同步写库。
* **M8.4（自动释放空间）会调脱水** —— 必须复用 M3 的安全检查链，**不许新增旁路**；验收里专门断言 `blocked_*` 分类仍生效。

### 12.6 M8.4 的实测结果

**采集范围与基线**：基线 = M8.3 之后的 `.local-run/nas-after-m83.json`（`/home` 全树 16 项）；
事后 = `.local-run/nas-after-m84.json`。

| 项 | 结果 |
|---|---|
| 期间跑过的验收 | `fuse-matrix` 68 + `m5` 30 + `m6` 29 + `m7` 60 + `m82` 37 + `m83` 27 + `gui-matrix` 131 + `m84-matrix` **92** = **474 项**（全部通过），外加 `cargo test --workspace` |
| **added** | **0** |
| **removed** | **0** ← 最关键的一条 |
| **resized** | **1**：`/home/.recent`（106496 → 110592 字节）—— **NAS 自己的「最近访问」索引**（QTS 维护，不是用户数据）；M8.4 第一次**大量真读写** NAS（上传/下载/水合/脱水夹具），索引随之增长属于预期。**复跑一次（基线改为第一次 manifest）：`resized 0`**，说明那只是索引一次性补齐、不是持续增长 |
| **retimed** | **1**：`/home/qxync-test` 目录 mtime（子文件增删导致，夹具目录自身） |

**结论**：16 项已有数据里**没有删除**；唯一「大小变化」的是 NAS 自己维护的 `.recent` 索引
（在 M8.1–M8.3 的记录里它一直只变 mtime，M8.4 起因为真读写变多而变大小，已在上面点名）。
`qxync-test/` 里的夹具文件由矩阵自己创建并清理，比对时已回到基线状态。

**两次全量回归都在最终产物上跑过**（第二次是为了验证「量空间路径容错」那处小改动之后的二进制）：
`fuse 68 · m5 30 · m6 29 · m7 60 · m82 37 · m83 27 · gui 131 · m84 88`，两次**都是 0 失败**；
第二次的 manifest 比对是 `added 0 / removed 0 / resized 0 / retimed 2`（`.recent` 与夹具目录 mtime）。

> ⚠️ 与 M8.1–M8.3 的差别要如实写明：那三次的 `resized` 是 0；这次是 1，且那 1 项是
> NAS 内部索引 `.recent`。**没有第二个文件被写过。**

### 12.7 M8.6 的实测结果

**采集范围与基线**：基线 = **M8.6 改动落地之后、回归矩阵开跑之前**的 `.local-run/nas-baseline-m86.json`；
事后 = `.local-run/nas-after-m86.json`（两者都是 `/home` 全树 16 项）。

| 项 | 结果 |
|---|---|
| 期间跑过的验收 | `gui-matrix` **148** + `m5` 30 + `m82` 18 + `m83` 24 + `m84` 48 + `m6` 21 + `m7` 42 = **331 项通过**；另 `cargo test --workspace` 全绿（6 个测试二进制，0 failed）；`fuse-matrix` 因无 `/dev/fuse` **未运行** |
| **added** | **0** |
| **removed** | **0** ← 最关键的一条 |
| **resized** | **0** ← 没有任何已有文件的内容被改（这一次连 `.recent` 都只动了 mtime） |
| retimed | **2**：`/home/.recent`、`/home/qxync-test` —— M8.1–M8.4 记录里的老面孔（NAS 自维护索引 + 夹具目录），属预期 |

**为什么这次改动本身不可能伤到 NAS**：M8.6 的写域只有
`crates/qxync-gui/ui/**`、`crates/qxync-gui/src/lib.rs`（新增 `ui_spec` 静态自检）、
`xtask/tests/gui-matrix.sh` 与文档 —— **零 daemon 改动、零协议改动、零 FUSE 改动**，
两条铁则（`read()` 不短读、脱水先 `inval_inode`）的实现代码**一行未动**。

**环境限制（如实记录）**：本沙箱**没有 `/dev/fuse`**（`/proc/filesystems` 里有 `nodev fuse`，
但没有设备节点，也没有 `CAP_MKNOD` / 可用 `sudo`）。`fuse-matrix.sh` 直接停在
`fusermount3: fuse device /dev/fuse not found. Kernel module not loaded?`；
`m6`/`m7`/`m82`/`m83`/`m84` 的**真挂载段按设计整段跳过** —— 它们是**「跳过」而不是「通过」**。
有 `/dev/fuse` 的机器上重跑这些矩阵即可（M6/M7/M8.4 当年也是这么补的）。

**顺带发现的既有问题（不在 M8.6 写域，未修）**：无 `/dev/fuse` 时 `qsync task add` / `task resume`
（默认要挂载）会让 daemon 的 worker panic 断连（`Cannot drop a runtime in a context where blocking is not allowed`）。
这解释了 `m82-matrix.sh` 在 `--no-fuse` 模式下的 2 条失败 —— 它们都要求真挂载，而 M8.6 没碰
`qsync`/`qxyncd` 一行。建议修法：把挂载失败包成 `Result` 经 IPC 返回，而不是在 blocking 任务里让运行时析构。

---

## 附：基准来源

* 教程正文与截图：[如何使用 Qsync Client 6？](https://www.qnap.com.cn/zh-cn/how-to/tutorial/article/%e5%a6%82%e4%bd%95%e4%bd%bf%e7%94%a8-qsync-client-6)（最后修订 2025-07-22）
* 英文版教程（含 44 张截图，逐张识读）：[How to use Qsync Client 6?](https://www.qnap.com/en/how-to/tutorial/article/how-to-use-qsync-client-6)
* 4.x/5.x 旧版界面（对照「主页 = NAS 列表 → 统一任务列表」的差异）：[如何使用 Qsync Client？（cid=388）](https://www.qnap.com.cn/go/how-to/tutorial/con_show.php?cid=388)
* 6.0 旗舰功能口径（1 向/2 向同步、节省空间模式、备份+时间点还原、团队文件夹/分享链接）：[QNAP Unveils Qsync 6.0 Beta](https://www.storagenewsletter.com/2025/07/04/qnap-unveils-qsync-6-0-beta-one-stop-file-backup-and-synchronization-solution/)（2025-07-04 新闻稿）、[Qsync 产品页](https://www.qnap.com.cn/zh-cn/software/qsync)
* 各平台版本与托盘形态（Ubuntu 停在 1.0.x）：[QNAP Utilities 下载页](https://www.qnap.com/en/utilities/essentials)、[winget 版本清单](https://wingetgui.com/apps/QNAP-Qsync)
* **节省空间模式三状态 / 右键菜单 / 释放空间页**术语：[How to save disk space with Space-Saving Mode](https://www.qnap.com/en/how-to/tutorial/article/how-to-save-disk-space-with-space-saving-mode-in-qsync-client-5-0-6-windows-5-0-5-macos-or-later-versions)
* **冲突策略 5 选项 / 筛选器格式 / NAS 侧版本控制设置**：[How to use Qsync to synchronize files](https://www.qnap.com/en/how-to/tutorial/article/how-to-use-qsync-to-synchronize-files-between-the-nas-and-my-other-devices)
* 6.0.3 增量（`View previous versions` 入口）、Mac 6.0 Beta：[6.0.3 公告](https://community.qnap.com/t/qsync-client-for-windows-6-0-3-is-here-view-restore-previous-versions-of-synced-files-plus-thumbnail-previews-for-cloud-space-saving-files/6452)、[Mac 6.0 Beta 公告](https://community.qnap.com/t/qsync-for-mac-6-0-beta-open-for-community-testing/6195)
* 编码冲突命名（`(Conflicted encode (x))`）：[Qsync FAQ](https://www.qnap.com.cn/zh-cn/how-to/faq/article/qsync-%E4%B8%BA%E4%BD%95%E5%9C%A8%E6%96%87%E4%BB%B6%E6%88%96%E6%96%87%E4%BB%B6%E5%A4%B9%E5%90%8D%E7%A7%B0%E6%9C%AB%E5%B0%BE%E5%8A%A0%E4%B8%8Aconflicted-encode-x%E5%AD%97%E6%A0%B7)
* 筛选器不匹配文件夹名（**用户报告，未证实**）：[QNAP Community: folder filter support](https://community.qnap.com/t/qsync-adding-folder-filter-support/5080)
* 现状：`README.md`、`docs/M4-GUI.md`、`docs/开发规划.md`、`crates/qxync-gui/**`、`crates/qxync-core/src/ipc.rs`
* 协议与能力边界：`docs/M5-SQLite与delta.md`（NAS 无历史版本）、`docs/M6-多根与共享文件夹.md`（共享根只读）、`docs/M7-选择性同步与LAN直连.md`（自研 LAN 协议、不做官方通道互通）
* 设备注册端点规格与探针结论：`report/out/api_endpoints.json` #121 `qbox_save_device_config`（**已探针，决策不实现**）、
  `report/probe/p0_device_probe.py`（P0 探针工具）、`report/probe/probe-out/p0-*/`（原始响应，gitignore）
* 补充调研留档（含原始 HTML / 正文提取件 / 44 张截图）：`.research/qsync-client-6-UI调研报告.md`、`.research/art.txt`、`.research/en.txt`、`.research/img/`
