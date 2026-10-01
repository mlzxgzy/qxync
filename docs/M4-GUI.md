# M4 —— GUI（Tauri 2 桌面应用）

> 状态：**已实现并真机验收**（`xtask/tests/gui-matrix.sh` 41/41）。
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
  （契约：`qxync-core/src/ipc.rs`，说明：`M1.5-设计.md`），与 CLI `qsync` 走同一条线。
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
   但**对象内部字段保持 serde 的 snake_case 原样**（`home_root` / `ipv4_only` / `read_write` /
   `force_deletes` / `hydrate_timeout_secs` / `cache_mode`…）。
2. `login_flow` 会**重启 daemon**：`qxyncd` 只在启动时读 `links/<id>.json`，
   所以「换 NAS / 换账号」必须重启；只有连接参数（host/port/https/user/ipv4_only）没变才直接登录。

## 4. 界面（5 个 tab）

| tab | 内容 |
|---|---|
| **状态 / 进度** | 服务端信息（Qsync 版本/QPKG/build/busy_reason）、会话、三游标、水合统计、上传队列（pending/active/done/failed/retries/bytes）、缓存限额进度条、`blocked_*` 分类、挂载列表、最近一轮同步摘要 |
| **连接 / 登录** | host/port/https/insecure/user/password/home_root/ipv4_only 表单（打开时预填）；保存配置 / **保存并登录** / 启停 daemon；三个 XDG 目录与 socket 路径 |
| **挂载** | 当前挂载表（可卸载）+ 新建挂载（挂载点默认 `$HOME/qsync-mnt`、读写开关、`cache_mode`、线程数、水合超时、删除熔断阈值、auto_unmount） |
| **文件 / pin** | 远端目录浏览（真机 `ls`，目录优先）、每行 pin 查询/设置（`unspecified/pinned/unpinned/excluded`）、下载（`get`）、脱水（`dehydrate`）、新建目录、删除 |
| **同步 / 缓存** | `SyncInfo` 全量（含 `devices`、`last_error`、`delete_block_reason`）+ 立即同步 / 强制放行删除 / 暂停轮询 / 设间隔；`CacheInfo` 全量 + 脱水预演 / 全部脱水 / 按限额 / 释放闲置 |

底部固定「操作日志」面板：每次 invoke 记一行 `HH:MM:SS 命令 参数 → ok/error`，保留 200 条
（排障 + 验收时肉眼可见，见 §6 截图）。

## 5. 自检与验收

```bash
# 无窗口自检（要在 daemon 跑着的时候）：前端资源嵌入 + status/ls/mounts/sync 四条数据源
cargo run -p qxync-gui -- --self-test

# 登录链自检：daemon_stop → login_flow（写 link/凭据 + 拉起 daemon + 登录）→ 复核
cargo run -p qxync-gui -- --self-test-login

# 完整验收矩阵（41 项）
xtask/tests/gui-matrix.sh              # 自检 + 登录链 + 5 个 tab 真窗口截图
xtask/tests/gui-matrix.sh --no-window  # 无 DISPLAY 的机器只跑自检
xtask/tests/gui-matrix.sh --keep-open  # 结束时保留窗口，手动玩
```

矩阵覆盖（41 项）：

1. 三个二进制 + 连接配置前置检查；
2. `--self-test`：资源嵌入、`status_ok`、`logged_in`、真机 `ls /home`、`mounts`、`sync`，
   以及**负向对照**——daemon 停掉后 `--self-test` 必须非 0 退出（防止自检变成橡皮图章）；
3. `--self-test-login`：GUI 自己的「保存并登录」链路真的能把 daemon 拉起来并登录；
4. 真窗口：5 个 tab 各起一次，窗口标题正确、截图非空白（stddev > 1500）、
   与 status 页画面确有差异（AE > 4000 px）。

> 截图落在 `$QSYNC_TEST_RUNDIR/gui-shots/`（默认 `.local-run/gui-shots/`，已 gitignore），
> 里面还有 `self-test.json` / `self-test-login.json` 两份原始自检输出。

已知环境限制：矩阵里窗口/截图项会 `export GDK_BACKEND=x11`——`xdotool`/`import`
看不见原生 Wayland 窗口。**只影响验收脚本**，用户正常启动仍走 Wayland。

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
  默认给 `$HOME/qsync-mnt`。
* `pin` 沿用 daemon 现状：**内存态**（M1.5 起就只登记，重启丢），GUI 只是它的可视化。
* 文件表格在窄窗口下横向滚动；行内 pin 的「当前值」要手动点一次「查 pin」才知道（显示「未知」）。
* 不做托盘图标、开机自启、通知；不做多 link 切换（`daemon_start` 已支持传 `linkId`，界面暂只发默认 link）。
* 下载/上传仍是「弹窗填本地路径」，没有文件选择器（同上，等引入 dialog 插件）。
