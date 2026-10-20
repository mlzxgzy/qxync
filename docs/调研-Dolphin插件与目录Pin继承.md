# Dolphin 插件与目录 Pin 继承：调研与方案归档

> 日期：2026-10-18
> 状态：**调研归档，暂不实施**（2026-10-20 更新：应「Tags 列看不清楚、要图标」的需求，
> 已按 §2.2.4 排除的 `KOverlayIconPlugin` 做出角标插件，见 [M23 §8](M23-Dolphin集成.md#8-角标插件koverlayiconplugin2026-10-20-追加)。本文其余结论不变。）
> 环境：CachyOS / KDE Plasma 6 / Dolphin 26.08.2 / KF6 6.31 / Qt 6.12
>
> 结论：技术上可行，但“常驻状态列 / 角标 / 实时推送”等体验受 Dolphin 和 KIO 的限制；
> 在“不改 Dolphin、不做角标”的前提下，插件只剩右键菜单，收益不足以抵消维护成本，
> 因此本轮只保存调研与方案，不改代码。

---

## 0. 一句话结论

- **Dolphin 右键插件能做**：在 `fuse.qxync` 挂载点内显示水合/Pin 状态、控制 Pin；
  非同步目录返回空菜单，零 xattr 探测。
- **原版 Dolphin 不能加自定义列 / 信息面板 / tooltip**：没有对应插件 API；
  要做只能改 Dolphin 源码，已明确不做。
- **目录 Pin 不能只靠插件做完整**：插件可以枚举现有文件，但新文件不会继承，
  脱水安全检查也只看精确路径。正确做法是 core 增加“有效 Pin”继承。
- **最终取舍**：不改 Dolphin、不做角标；目录 Pin 继承方案已设计但本轮不实施。

---

## 1. 需求

最初的目标：

1. 在 Dolphin 里直接看到某个文件的**水合状态**（仅在线 / 部分本地 / 已水合）；
2. 看到并控制 **Pin 状态**（固定 / 取消固定 / 排除）；
3. 只对 qxync 同步目录生效，普通目录不显示；
4. 支持当前系统已有的 Dolphin；
5. 后来明确：**不要角标**；**不要自定义列，不改 Dolphin**；
6. 目录 Pin 采用**继承**语义（Pin 目录后，子树和以后新增文件都受保护）。

---

## 2. 调研结论

### 2.1 能做的

| 需求 | 机制 | 证据 |
|---|---|---|
| Dolphin 里加右键动作 | KIO `KAbstractFileItemActionPlugin` | `KFileItemActions` 从 `kf6/kfileitemaction` 加载；已用最小 CMake 工程在本机编译通过 |
| 读取水合状态 | FUSE xattr `user.qxync.state` | 见 [`qxync-fuse/src/lib.rs`](../crates/qxync-fuse/src/lib.rs) 的 `getxattr` |
| 读取 Pin 状态 | xattr `user.qxync.pin` | 同上；daemon 与 FUSE 共享同一份 pin map |
| 设置 Pin | IPC `{"v":1,"method":"pin","path":"...","state":"..."}` | [`qxync-core/src/ipc.rs`](../crates/qxync-core/src/ipc.rs) 的 `Request::Pin`；daemon 的 `pin_cmd` 同步写内存和 SQLite |
| 找 daemon 和挂载点 | IPC `mounts` | daemon 返回 `{mountpoint, remote}` 列表 |
| 区分同步目录 | 挂载 fstype `fuse.qxync` | [`qxync-fuse/src/lib.rs`](../crates/qxync-fuse/src/lib.rs) 的 `Subtype("qxync")` |
| 非同步目录不显示 | `actions()` 先查挂载表，不命中直接返回空 | 插件层即可实现，不依赖 Dolphin 改动 |

### 2.2 做不到 / 做不干净的

#### 2.2.1 自定义“水合”列：原版 Dolphin 没有插件 API

Dolphin 的列由 `KFileItemModel::rolesInfoMap()` 固定定义：Name、Size、Modified、
Created、Accessed、Type、Rating、Tags、Comment、Title、Author 等。
`KFileItemModelRolesUpdater` 只解析这张表 + Baloo 提供的角色，**没有“插件注册一列”的接口**。

- `Comment` 列看起来能放文字，但它要求 Baloo 索引，数据来自 Baloo 的 `userComment`，
  不是普通 xattr，也不是给插件用的；
- 右侧信息面板（F11）使用 `Baloo::FileMetaDataWidget`，同样依赖 Baloo 索引库；
- 底部状态栏使用 `KFileItem::getStatusBarInfo()`，没有插件钩子；
- tooltip 也来自 Baloo。

结论：**不修改 Dolphin 源码，就没有常驻的水合状态列/信息面板/tooltip。**

#### 2.2.2 Dolphin 的 VCS 插件不能替代

`KVersionControlPlugin` 只返回固定的 `ItemVersion` 枚举
（Normal / Modified / Added / Conflicting / ...），Dolphin 把它渲染为：
- 文件名文字颜色；
- 左上角一个固定 VCS 图标。

它**不能自定义文字，不能加列**，而且按 `.git` / `.svn` 等目录标记决定是否启用，
会和真正的 git/svn 插件抢目录。不适合水合/Pin 语义。

#### 2.2.3 非同步目录里“完全不加载插件”：KIO 架构不支持

Dolphin 的 KIO 插件是**进程级全局加载**：

- action 插件在每次弹出菜单时从 `kf6/kfileitemaction` 加载；
- overlay 插件（本方案不用）在视图初始化时从 `kf6/overlayicon` 加载；
- 没有“只在某些路径加载”的机制。

能做到的是：非同步路径下 `actions()` 返回空，**不显示菜单、不读 xattr**；
但插件 `.so` 仍在 Dolphin 进程里，右键动作插件也会出现在
“配置 Dolphin → 上下文菜单”的插件列表中。这一点无法通过插件隐藏。

#### 2.2.4 角标也不是理想方案

- `KOverlayIconPlugin` 只给图标加角标，不能带文字/tooltip；
- Dolphin 只留三个角给插件（左下、右上、右下），左上角固定给 VCS；
- 用户已明确：**角标没有意义，不做**。

#### 2.2.5 只支持本地 `file://`

不处理 `sftp://`、`fish://`、`smb://`、`trash:`、`recentdocuments:` 等虚拟/远程路径。
插件只对本地文件生效。

#### 2.2.6 混合“文件 + 目录”多选时右键菜单不出现

KIO 的 action 插件按**公共 MIME** 过滤：

- 选文件：公共 MIME 是 `application/octet-stream`（或继承它的具体类型）；
- 选目录：`inode/directory`；
- 文件和目录混选：公共 MIME 为空，KIO 不调用插件。

规避方式：文件、目录分开选。多选多个文件、多选多个目录都正常。

#### 2.2.7 插件只能“按需”显示状态

没有列、没有角标、没有 tooltip 后，状态只能：

- 右键菜单顶部状态行；
- 状态详情对话框；
- 可选的属性页（`KPropertiesDialogPlugin`，本轮未做）。

如果要在列表中“扫一眼就看到”，只能改 Dolphin，已明确不做。

#### 2.2.8 Pin 依赖 daemon，且 FUSE 没有 setxattr

- 设置 Pin 必须通过 `qxyncd` 的 unix socket；
- daemon 停止时只能看状态（如果挂载还活着），不能改 Pin；
- FUSE 只实现了 `getxattr` / `listxattr`，没有 `setxattr`，
  所以 `setfattr`、其他文件管理器、普通脚本都不能直接写 `user.qxync.pin`；
- 要支持“任意工具改 Pin”，需要给 FUSE 增加 `setxattr`，属于 core 改动。

#### 2.2.9 状态刷新不是事件推送

qxync 目前没有文件状态变化订阅接口：

- `status` / `file_states` 都是请求-响应；
- xattr 变化不会触发 inotify；
- 所以插件只能在**打开菜单时**读取最新 xattr，不能实时刷新已打开的界面。

如果以后要实时，需要给 daemon 增加事件订阅通道。

#### 2.2.10 多连接/自定义 socket 的边缘情况

插件通过扫描 `$XDG_RUNTIME_DIR/qxync/qxyncd*.sock`（以及 `QXNYC_SOCKET`）发现 daemon，
再对每个 daemon 发 `mounts` 建立“挂载点 → socket”映射。以下情况可能失效：

- daemon 用 `--socket` 启动在任意目录；
- 挂载点由另一个 `qxyncd` 进程“外部托管”（M15/T6），其 `mounts` 表里没有它；
- 多个 daemon 返回异常。

结果：状态仍能通过 xattr 读到，但 Pin 动作可能找不到 daemon，只能弹错误。
彻底解决需要给 FUSE 增加 `user.qxync.socket` 之类的 xattr。

#### 2.2.11 挂载失联只能显示“不可用”

daemon 挂了/挂载点变成 `Transport endpoint is not connected` 时：

- `getxattr` 直接失败，拿不到每个文件的状态；
- 只能靠 `/proc/self/mounts` 知道“这曾经是 qxync 挂载点”，显示统一的离线状态；
- Pin 也做不了。

#### 2.2.12 其他限制

- 只支持原生 Plasma 6 / KF6 Dolphin；KF5、Flatpak Dolphin 不支持；
- Nautilus / Thunar 等使用不同扩展机制，不通用；
- 看不到同步维度（已同步 / 待上传 / 冲突）：现有 xattr 只有水合和 Pin，
  这些数据在 `file_states` 里，需要额外查询/新增 xattr；
- Pin 落库失败时 daemon 仍返回成功（`pin_cmd` 只打 WARN），插件无法识别；
  这是既有 core 行为，不是插件能修的。

---

## 3. 最终取舍（2026-10-18）

| 议题 | 决定 |
|---|---|
| 自定义列 | **不做**；不改 Dolphin |
| 角标 | **不做**；没有意义 |
| Dolphin 版本 | 只支持当前系统 26.08.2 / KF6 6.31 |
| 插件形态 | 如果实施，只做 `KAbstractFileItemActionPlugin` 右键菜单 |
| 目录 Pin | 如果实施，core 做“有效 Pin 继承”，不是插件枚举 |
| 当前动作 | **只归档本文档，不改代码** |

暂不实施的原因：在不改 Dolphin、不做角标的前提下，插件只能提供右键菜单，
而 core 的目录 Pin 继承还需要改 FUSE/daemon/IPC/GUI/CLI 多处；
当前收益不足以抵消维护成本。先把调研和设计留档，以后需要时可直接按第 4、5 节实施。

---

## 4. 若以后实施：目录 Pin 继承（core）

### 4.1 语义

`pins` 表继续只存**精确路径**；新增“有效 Pin”：

```text
自身 pinned / excluded        → 用自身
自身 unpinned                  → 用自身（显式不钉，阻断继承）
自身 unspecified / 没有记录     → 向上找最近的 pinned / excluded
一直找到根仍没有                → unspecified
```

效果：

- `pin /home/dir pinned`：
  - `/home/dir/a.txt` 的 `pin_effective = pinned`；
  - 以后新建 `/home/dir/b.txt` 自动继承；
  - 脱水安全检查对子树返回 `Pinned`；
- 子文件想单独放掉：设 `unpinned`，阻断目录继承；
- 想恢复继承：设 `unspecified`；
- `excluded` 同样继承；
- 只 Pin 文件时行为不变。

### 4.2 core 改动

新增 `crates/qxync-core/src/pins.rs`：

```rust
pub fn normalize_pin_path(path: &str) -> String;       // 去尾斜杠（保留 "/"）
pub fn parent_pin_path(path: &str) -> Option<&str>;    // "/a/b" → "/a"；"/a" → "/"；"/" → None
pub fn effective_pin<'a, F>(lookup: F, path: &str) -> Option<&'a str>
where
    F: FnMut(&str) -> Option<&'a str>;
```

在 `qxync-core/src/lib.rs` 注册 `pub mod pins;`。

### 4.3 脱水

[`qxync-core/src/dehydrate.rs`](../crates/qxync-core/src/dehydrate.rs)：

```rust
pub struct Candidate {
    // ...
    pub pin: String,                 // 精确值，保持原语义
    #[serde(default)]
    pub pin_effective: String,       // 继承后的有效值
}
```

- `eligible()` 改用 `pin_effective`；为空时回退 `pin`，兼容老 Candidate/测试；
- `Block::Pinned` / `Block::Excluded` 的语义不变，只是判断源改为有效值；
- 测试新增：祖先 pinned 时子文件被挡；祖先 excluded 时子文件被挡；
  子文件 unpinned 可放行。

### 4.4 FUSE / daemon / IPC

**FUSE** [`qxync-fuse/src/lib.rs`](../crates/qxync-fuse/src/lib.rs)：

- `candidate_of()` 一次性锁 `self.pins`，同时计算 `pin` 和 `pin_effective`；
- `getxattr()`：
  - `user.qxync.pin` 保持精确值（不破坏既有消费者）；
  - 新增 `user.qxync.pin_effective`；
- `listxattr()` 加入新名字；
- `log_dehydrate_block()` 同时打印两个值。

**daemon** [`qxync-daemon/src/daemon.rs`](../crates/qxync-daemon/src/daemon.rs)：

- `pin_cmd()` 写之前规范化路径；仍只写这一条精确路径，不递归写子路径；
- Pin 查询/设置响应增加 `pin_effective`；
- `file_states_cmd()` 的 `FileStateInfo.pin` 保持精确值，新增 `pin_effective`；
- 三态 `always` 判断改用 `pin_effective == "pinned"`。

**IPC** [`qxync-core/src/ipc.rs`](../crates/qxync-core/src/ipc.rs)：

```rust
pub struct FileStateInfo {
    // ...
    pub pin: String,
    #[serde(default)]
    pub pin_effective: String,
    // ...
}
```

只加字段，不改已有字段含义，**不 bump `IPC_VERSION`**：

- 老 daemon → 新 GUI：`pin_effective` 为空，前端回退 `pin`；
- 新 daemon → 老 GUI：未知字段忽略；
- 更新 `t8_file_states_json_field_names_are_stable` 的字段清单。

### 4.5 CLI / GUI

**CLI** [`qxync-cli/src/main.rs`](../crates/qxync-cli/src/main.rs)：

- `qxync pin <path>`（查询）同时输出精确值和有效值；
- 设置输出也带 effective；
- `qxync state` 打印两个值；
- `print_file_states` 打印 `pin_effective`；
- 调整 `xtask/tests/m5-matrix.sh` 里对 pin 查询输出的断言。

**GUI** [`qxync-gui/ui/app.js`](../crates/qxync-gui/ui/app.js) /
[`index.html`](../crates/qxync-gui/ui/index.html) /
[`i18n.js`](../crates/qxync-gui/ui/i18n.js)：

- `PIN_LABEL` 更新：
  - `unspecified`：未指定（继承父目录）
  - `pinned`：已固定
  - `unpinned`：不固定（覆盖继承）
  - `excluded`：已排除（永不脱水）
- 行内 pin 单元格显示 `pin_effective`，tooltip 给“自身值 / 继承值”；
- 右键菜单：
  - `取消固定` 改为写 `unpinned`（显式覆盖目录继承）；
  - 新增 `恢复继承` 写 `unspecified`；
  - 原 `不保留在此设备` 与 `unpinned` 合并，避免同义项；
- 老 daemon 没有 `pin_effective` 时回退 `pin`。

### 4.6 兼容与迁移

- `pins` 表结构不变，无 SQLite migration；
- 已存在的目录 Pin 从这一版起开始对子树生效（行为变化，需在 CHANGELOG 说明）；
- 已存在的 `unpinned` 子文件自然成为“覆盖继承”的例外；
- 带尾斜杠的 pin 在写入/查询时规范化，不做破坏性批量重写。

### 4.7 测试

- `pins::effective_pin`：
  - 目录 pinned → 子/孙路径 effective pinned；
  - 子路径 unpinned → 阻断继承；
  - 子路径 unspecified / 无记录 → 继续向上；
  - 祖先 excluded → 继承 excluded；
  - 根 `/`、尾斜杠、深层路径。
- `dehydrate::eligible`：
  - `pin_effective=pinned` → `Block::Pinned`；
  - `pin_effective=excluded` → `Block::Excluded`；
  - `unpinned` 放行；
  - 空 `pin_effective` 回退 `pin`。
- FUSE：`candidate()` 对目录 Pin 子文件给出有效值；xattr/listxattr 包含新名字。
- IPC：新字段序列化；旧 JSON 仍能解析。
- daemon/CLI：`file_states --json` 含 `pin_effective`；pin 查询两个值。
- e2e：新增 `xtask/tests/m20-pin-inherit.sh`：
  1. 建目录/文件；
  2. pin 目录；
  3. 子文件 `user.qxync.pin_effective=pinned`；
  4. `dehydrate` 被挡；
  5. 新建文件自动继承；
  6. 子文件 `unpinned` 可脱水；
  7. 子文件 `unspecified` 恢复继承；
  8. daemon 重启后继承仍在。

---

## 5. 若以后实施：Dolphin 右键插件

### 5.1 范围

只做 `KAbstractFileItemActionPlugin`：

```text
contrib/dolphin/
├── CMakeLists.txt
├── README.md
├── common/
│   ├── qxync_mounts.{h,cpp}    # /proc/self/mounts + fuse.qxync 判定
│   ├── qxync_status.{h,cpp}    # 读 xattr
│   └── qxync_ipc.{h,cpp}       # socket 发现 + mounts/pin
├── action/
│   ├── qxync_action.{h,cpp}
│   └── qxync_action.json
└── tests/
```

- 不实现 `KOverlayIconPlugin`；
- 不安装角标 SVG；
- 不修改 Dolphin 源码；
- 只安装 `kf6/kfileitemaction/qxync_action.so`。

### 5.2 菜单只在 qxync 挂载点出现

`actions()` 顺序：

1. 过滤非本地 URL；
2. 解析 `/proc/self/mounts`，命中 `fstype == fuse.qxync` 且路径在该挂载点下；
3. 任何一项不命中 → `return {}`，**不读 xattr**；
4. 全部命中 → 读 `user.qxync.state` / `.pin` / `.pin_effective` / `.remote`；
5. 构造菜单。

普通目录、`/tmp`、非 qxync 的 FUSE 挂载（例如本机 `/home/tester/Qsync`）完全没有 qxync 菜单项。

### 5.3 菜单内容

```text
qxync：部分本地 · Pin=已固定（继承）        （禁用状态行）
────────────────────────────────
固定到本地（子目录继承）
取消固定（覆盖继承）
恢复继承
排除自动释放（永不脱水）
────────────────────────────────
查看 qxync 状态…
```

- 状态行：
  - 水合：仅在线 / 部分本地 / 已水合 / 挂载不可用
  - Pin：未指定 / 已固定 / 已固定（继承） / 不固定（覆盖继承） / 已排除
- 动作语义：
  - 固定到本地 → `pin=pinned`（目录则子树继承）；
  - 取消固定 → `pin=unpinned`（覆盖父目录继承）；
  - 恢复继承 → `pin=unspecified`；
  - 排除自动释放 → `pin=excluded`；
- 多选时状态显示“混合”，动作应用到全部选中项；
- 目录 Pin 不遍历子项、不发 N 次 IPC，只写目录这一条，继承由 core 完成；
- 失败弹错误对话框；成功不弹窗；
- daemon 未运行但挂载点还在：显示禁用的“同步服务未运行”，不提供 Pin；
- 混合文件+目录的多选受 KIO MIME 过滤限制，文档说明，分别选择即可。

### 5.4 IPC

- 扫描 `$XDG_RUNTIME_DIR/qxync/qxyncd*.sock` + `QXNYC_SOCKET`；
- 对每个 socket 发 `{"v":1,"method":"mounts"}`，取最长挂载点前缀定位 daemon；
- 发 `{"v":1,"method":"pin","path":"<远端路径>","state":"..."}`；
- 读 `pin_effective` 校验结果；
- 全部在工作线程，连接 300ms / 读 1.5s 超时，不阻塞 Dolphin；
- 多连接按 socket 分组；找不到 daemon 弹错误。

### 5.5 状态详情

`查看 qxync 状态…` 显示：

```text
本地：/mnt/qxync/home/dir/a.bin
远端：/home/dir/a.bin
水合：部分本地（3/10 区间）
Pin：未指定（有效：已固定，继承）
大小：本地 1.2 MiB / 原始 4.0 MiB
```

只读 xattr，不发 `file_states`，不触发 NAS 目录扫描。

### 5.6 构建

- C++17 / Qt6 / KF6，CMake 独立工程；
- 依赖：`cmake`、`qt6-base`、`kcoreaddons`、`kio`（本机已具备）；
- 安装路径：`${CMAKE_INSTALL_LIBDIR}/qt6/plugins/kf6/kfileitemaction/`；
- 元数据：

  ```json
  {
    "KPlugin": {
      "Name": "qxync",
      "MimeTypes": ["application/octet-stream", "inode/directory"]
    }
  }
  ```

- 构建：
  ```bash
  cmake -S contrib/dolphin -B build/dolphin -DCMAKE_BUILD_TYPE=Release
  cmake --build build/dolphin -j
  sudo cmake --install build/dolphin
  ```
- 重启 Dolphin 生效；`QT_DEBUG_PLUGINS=1 dolphin` 可排查加载；
- 只支持当前 KF6 Dolphin，不做 KF5/Flatpak。

---

## 6. 明确不做的

- 自定义 Dolphin 列 / 信息面板 / tooltip / 底部状态栏；
- 改 Dolphin 源码或做 Dolphin 补丁；
- 角标（`KOverlayIconPlugin`）；
- 属性页（`KPropertiesDialogPlugin`，可作为以后可选项，本轮不做）；
- 同步维度（已同步 / 待上传 / 冲突）状态；
- 递归写子文件 Pin（用 core 继承替代）；
- Flatpak Dolphin、KF5 Dolphin、Nautilus/Thunar；
- 通过 FUSE `setxattr` 让任意工具改 Pin（core 改动，未做）。

---

## 7. 验收清单（若实施）

### core

- [ ] `pin /home/dir pinned` 后，子文件 `user.qxync.pin_effective=pinned`；
- [ ] 子文件 `pin=unpinned` 阻断继承，可脱水；
- [ ] 子文件 `pin=unspecified` 恢复继承；
- [ ] 新建文件自动继承；
- [ ] 脱水安全检查对继承 Pin 生效；
- [ ] daemon 重启后继承仍在；
- [ ] `file_states --json` 含 `pin_effective`；
- [ ] 旧 JSON / 旧 GUI 兼容；
- [ ] `cargo test --workspace`、`clippy`、`fmt --check` 通过；
- [ ] 现有 pin/脱水/三态 e2e 全部通过。

### Dolphin 插件

- [ ] 挂载点内右键有 qxync 菜单；
- [ ] 状态行显示水合 + Pin（含继承提示）；
- [ ] 固定/取消固定/恢复继承/排除动作正确；
- [ ] Pin 目录后子文件显示“已固定（继承）”；
- [ ] 非 qxync 目录无菜单、无 xattr 探测；
- [ ] daemon 停止时显示“同步服务未运行”，Dolphin 不崩溃；
- [ ] 多连接分别 Pin 不串；
- [ ] 混合文件+目录多选无菜单（已知限制）；
- [ ] 无角标、无自定义列、Dolphin 为系统原版。

---

## 8. 参考

- Dolphin 源码：
  - `src/kitemviews/kfileitemmodelrolesupdater.cpp`（加载 `kf6/overlayicon`）；
  - `src/kitemviews/kfileitemmodel.cpp`（`rolesInfoMap()` 固定列）；
  - `src/kitemviews/kstandarditemlistwidget.cpp`（角标绘制 `QIcon::fromTheme`）；
  - `src/views/dolphinfileitemlistwidget.cpp`（VCS 状态映射）；
  - `src/views/versioncontrol/versioncontrolobserver.cpp`（VCS 插件加载）；
  - `src/panels/information/informationpanelcontent.cpp`（Baloo 信息面板）。
- KIO：
  - `KOverlayIconPlugin`（`KIOCore`）；
  - `KAbstractFileItemActionPlugin`、`KFileItemActions`（`KIOWidgets`）；
  - `KFileItemActionsPrivate::addPluginActionsTo()` 的 MIME 过滤逻辑。
- qxync：
  - [`crates/qxync-fuse/src/lib.rs`](../crates/qxync-fuse/src/lib.rs)：xattr、Candidate、dehydrate；
  - [`crates/qxync-core/src/ipc.rs`](../crates/qxync-core/src/ipc.rs)：Pin、FileStates、MountInfo；
  - [`crates/qxync-core/src/dehydrate.rs`](../crates/qxync-core/src/dehydrate.rs)：安全检查链；
  - [`crates/qxync-daemon/src/daemon.rs`](../crates/qxync-daemon/src/daemon.rs)：`pin_cmd`、`file_states_cmd`、`mounts`；
  - [`crates/qxync-core/src/store.rs`](../crates/qxync-core/src/store.rs)：`pins` 表读写。
