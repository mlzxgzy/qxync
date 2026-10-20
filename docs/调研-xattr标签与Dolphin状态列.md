# xattr 标签（user.xdg.tags）与 Dolphin 状态显示：调研

> 日期：2026-10-20
> 状态：**调研完成，结论可行，待定是否实施**
> 环境：CachyOS / KDE Plasma 6 / Dolphin 26.08.2 / KF6 6.31 / Qt 6.12 / baloo 6.31
>
> 核心问题：把水合状态（仅在线 / 部分本地 / 已水合）写进 `user.xdg.tags` xattr，
> Dolphin 的 **Tags 列**能不能直接显示出来——**不用写任何 Dolphin 插件、不改 Dolphin**。

---

## 0. 一句话结论

**可行，而且比之前调研的"右键插件"方案更轻**：

- Dolphin 的 **Tags 列**读的就是 `user.xdg.tags` xattr（freedesktop.org 标准），**直接读文件系统，不经过 Baloo 索引**；
- qxync 的 FUSE 只要在 `getxattr` / `listxattr` 里多暴露一个 `user.xdg.tags`，Dolphin 就能把水合状态显示成 Tags 列里的文字；
- **不需要写 C++ 插件、不需要 KIO、不需要改 Dolphin**，改动全部落在 Rust FUSE 层；
- 代价：状态变化**不会实时刷新**（要 F5 / 重新进入目录才更新）；用户不能在挂载点上加自己的标签（`setxattr` 不实现）。

---

## 1. user.xdg.tags 是什么

freedesktop.org 的 [Common Extended Attributes](https://www.freedesktop.org/wiki/CommonExtendedAttributes/) 约定：

| xattr | 含义 | 格式 |
|---|---|---|
| `user.xdg.tags` | 文件标签 | 逗号分隔的字符串列表 |
| `user.xdg.comment` | 用户备注 | 字符串 |
| `user.xdg.origin.url` | 下载来源 URL | 字符串 |

KDE 侧由 `KFileMetaData::UserMetaData`（kfilemetadata 库）统一读写：

```cpp
// kfilemetadata/src/usermetadata.cpp
QStringList UserMetaData::tags() const {
    QString value;
    k_getxattr(d->filePath, QStringLiteral("xdg.tags"), &value);  // → getxattr(path, "user.xdg.tags")
    return value.split(QLatin1Char(','), Qt::SkipEmptyParts);
}
```

Linux 上 `k_getxattr` 自动拼 `user.` 前缀（[xattr_p.h:98](https://invent.kde.org/frameworks/kfilemetadata/-/blob/master/src/xattr_p.h#L98)），
所以**磁盘上的真实名字是 `user.xdg.tags`**，值是 `tag1,tag2` 这种逗号分隔串。

---

## 2. Dolphin 怎么显示 Tags 列（关键证据链）

这是本次调研最重要的发现：**Tags 列的值直接来自 xattr，不经过 Baloo 索引库**。

### 2.1 角色定义

Dolphin 的列（role）表里，`tags` 标着 `requiresBaloo=true, requiresIndexer=false`：

```cpp
// dolphin/src/kitemviews/kfileitemmodel.cpp:3220
{ "tags", TagsRole, kli18nc("@label", "Tags"), ..., /*requiresBaloo=*/true, /*requiresIndexer=*/false },
```

`requiresIndexer=false` 意味着**不要求 Baloo 索引过这个文件**——只要 Baloo 库在、
全局索引开关开着就行。

### 2.2 值的来源：直接读 xattr

`KFileItemModelRolesUpdater` → `KBalooRolesProvider::roleValues()`：

```cpp
// dolphin/src/kitemviews/private/kbaloorolesprovider.cpp:161-166
KFileMetaData::UserMetaData md(file.path());          // file.path() 就是本地路径
attributes = md.queryAttributes(attributes);
if (attributes & KFileMetaData::UserMetaData::Tags) {
    values.insert("tags", tagsFromValues(md.tags())); // ← 直接 getxattr("user.xdg.tags")
}
```

`md.tags()` 就是 §1 的那段代码——`getxattr(path, "user.xdg.tags")` 后按逗号切开。
**全程没有查 Baloo 的 Xapian 数据库。**

`Baloo::File` 在这里只当"路径壳"用（`file.path()`）；`file.load()` 加载的是
标题/作者等**内容元数据**（那些才走索引），tags 不在其中。

### 2.3 前提条件

`KFileItemModelRolesUpdater::setRoles()`（[kfileitemmodelrolesupdater.cpp:332](https://invent.kde.org/system/dolphin/-/blob/master/src/kitemviews/kfileitemmodelrolesupdater.cpp#L332)）：

```cpp
if (hasBalooRole && m_balooConfig.fileIndexingEnabled() && !m_balooFileMonitor) {
    m_balooFileMonitor = new Baloo::FileMonitor(this);
    ...
}
```

要 Tags 列工作，需要：

1. **编译时 `HAVE_BALOO`**（发行版 Dolphin 都带）；
2. **Baloo 全局索引开关开着**（`balooctl6 config` 里的 fileIndexingEnabled；
   本机实测为开）；
3. 用户在 Dolphin 里**手动把 Tags 列加出来**（右键列头 → 勾 Tags）。

**不需要** qxync 挂载点被 Baloo 索引——甚至应该把挂载点加进 Baloo 排除列表，
免得索引占位符文件把整个 NAS 拖下来（见 §5.2）。

### 2.4 xattr 探测怎么确认"有 tags"

`UserMetaData::queryAttributes()` 走 `listxattr`（[xattr_p.h:240](https://invent.kde.org/frameworks/kfilemetadata/-/blob/master/src/xattr_p.h#L240)）：

```cpp
const ssize_t size = listxattr(encodedPath, nullptr, 0);   // 先拿总长度
// ...再 listxattr 拿全名单，按 "user." 前缀过滤
// _mapAttribute: "xdg.tags" → Attribute::Tags
```

所以 FUSE 的 **`listxattr` 必须把 `user.xdg.tags` 列进名单**，`queryAttributes`
才会认为这个文件有 Tags 属性，Dolphin 才会接着调 `getxattr` 拿值。

### 2.5 信息面板（右侧栏）也走同一条路

Dolphin 信息面板用 `Baloo::FileMetaDataWidget` → `FileFetchJob`：

```cpp
// baloo-widgets/src/filefetchjob.cpp:53-58
KFileMetaData::UserMetaData umd(filePath);
if (umd.isSupported()) {
    auto umdData = Baloo::Private::fetchUserMetaData(umd, Attribute::Tags | ...);
    prop = Baloo::Private::convertUserMetaData(umdData);   // "tags" → 显示
}
```

同样是**直接读 xattr**。所以做了 `user.xdg.tags` 之后，选中文件按 F11 的
信息面板里也会出现 Tags 一行。

### 2.6 `tags:` 虚拟目录不走这条路

`tags://` KIO slave 从 Baloo **索引库**里查标签（`kio_tags.cpp`），我们的
xattr 标签**不会**出现在 `tags:` 虚拟文件夹里，也不能用 Baloo 的标签搜索搜到。
要进 `tags:` 得让 Baloo 真去索引挂载点——那会把占位符全部水合，不可接受（§5.2）。

---

## 3. qxync 侧需要改什么

改动全部在 Rust FUSE 层，量很小：

### 3.1 `getxattr` 多回一个名字

现状（[crates/qxync-fuse/src/lib.rs:5095-5106](../crates/qxync-fuse/src/lib.rs)）只认
`user.qxync.*` 五个名字。加一条：

```rust
"user.xdg.tags" => {
    // 水合状态映射成标签文字；目录不水合，回空
    let tag = if is_dir { "" } else {
        match state {
            "placeholder" => "仅在线",
            "partial"     => "部分本地",
            "hydrated"    => "已水合",
            _             => "",
        }
    };
    Some(tag.into())
}
```

值就是**动态算出来的**——不落盘、不占空间，每次 `getxattr` 现查节点表，
和现有 `user.qxync.state` 同一份数据源。

### 3.2 `listxattr` 名单加一项

```rust
// 现状
"user.qxync.state\0user.qxync.pin\0user.qxync.remote\0user.qxync.vsize\0user.qxync.chunks\0"
// 加上
"user.xdg.tags\0"
```

不加进名单，`queryAttributes` 就探测不到（§2.4），Tags 列直接空。

### 3.3 `setxattr` 不实现（有意为之）

用户在挂载点上右键加自己的标签 → Dolphin 调 `setxattr("user.xdg.tags", ...)`
→ FUSE 回 `ENOSYS`/`EPERM` → Dolphin 报"不支持"。这是**预期行为**：

- 挂载点上的标签本来就不该落盘（脱水后标签跟谁走？）；
- 避免和我们的动态值打架（写进去了下次 `getxattr` 也不该回它）。

文档里写清楚即可。

### 3.4 要不要同时暴露 pin 状态

可以顺手把 pin 也做成标签（`已固定` / `已排除`），和 `user.qxync.pin` 同源。
但注意 `user.xdg.tags` 是**一个** xattr，多个标签要拼在同一条逗号串里：

```text
已水合,已固定
```

建议 MVP 只放水合状态一个维度，pin 继续走 `user.qxync.pin`（CLI/GUI 已有），
免得 Tags 列文字太长。

---

## 4. 用户体验流程

1. 用户打开 Dolphin，进入 qxync 挂载点；
2. 右键列头 → 勾 **Tags**（一次性设置，Dolphin 按目录记忆）；
3. Tags 列显示每个文件的：`仅在线` / `部分本地` / `已水合`；
4. 选中文件按 F11，信息面板里也有 Tags 一行；
5. 状态变了（读了文件 / 脱水了）→ **按 F5 或重新进入目录**才更新。

非 KDE 桌面 / 无 Dolphin：xattr 照样在（`getfattr` 能看），只是没人显示；
不产生任何开销。

---

## 5. 限制与风险

### 5.1 不实时刷新（最大的限制）

Dolphin 的 Tags 列变化通知走 `Baloo::FileMonitor` → D-Bus → `baloo_file` 守护 →
inotify `IN_ATTRIB`。**FUSE 服务端改 xattr 不会触发内核 inotify 事件**（inotify
只看通过内核 VFS 进来的操作），所以：

- 后台脱水、读文件导致的水合 → Tags 列**不会自动变**；
- F5 / 退出重进目录 / 改视图设置 → 全量重读 xattr → 更新。

对"扫一眼看状态"的场景够用；对"盯着它变"的场景不行。

### 5.2 必须把挂载点排除出 Baloo 索引

Baloo 索引会**读文件内容**（内容索引）——在 qxync 挂载点上等于把每个占位符
整个拉下来。所以上线前必须：

```bash
balooctl6 config add excludeFolders ~/qxync-mnt
```

排除索引**不影响** Tags 列工作（§2.3 只看全局开关，不看单目录是否在索引里），
两者互不干扰。

### 5.3 用户不能在挂载点上加自己的标签

`setxattr` 不实现（§3.3）。Dolphin 的"添加标签"入口会报错。文档说明：
挂载点上的标签是系统生成的只读状态，不是用户标签。

### 5.4 标签文字是纯文本

没有颜色、没有图标、没有排序权重——就是 Tags 列里一段文字。
比 Windows Cloud Files 的"Status"列（有系统图标）朴素，但比什么都没有强。

### 5.5 依赖 Baloo 全局开关

用户如果把 Baloo 索引整个关了（`balooctl6 disable`），Tags 列也会跟着消失
（Dolphin 的 `m_balooFileMonitor` 不会创建）。这是 KDE 的设计，不是我们能控的。
本机默认开着，一般用户不会动。

### 5.6 `tags:` 虚拟目录和标签搜索不可用

见 §2.6。标签只在文件列表/信息面板可见，不能反向搜索。

---

## 6. 与之前两轮调研的关系

| 轮次 | 方案 | 结论 |
|---|---|---|
| 2026-10-18 [第一轮](调研-Dolphin插件与目录Pin继承.md) | KIO 右键插件 + overlay 角标 | 可行但收益低；角标被否；归档未实施 |
| 2026-10-18 同轮 | 自定义列 / 改 Dolphin | 明确不做 |
| **本轮（2026-10-20）** | **`user.xdg.tags` xattr → Dolphin 自带 Tags 列** | **可行，改动最小（纯 FUSE 层），推荐** |

本轮方案的本质：**借用 Dolphin 已经有的 Tags 列当"状态列"用**——不需要任何
插件，只是让 FUSE 多回答一个标准问题（"这个文件的 xdg.tags 是什么"）。

---

## 7. 若实施：改动清单与验收

### 改动

| 文件 | 内容 |
|---|---|
| `crates/qxync-fuse/src/lib.rs` | `getxattr` 加 `user.xdg.tags` 分支；`listxattr` 名单加项；单测 |
| `docs/M23-xattr标签.md`（或并入现有文档） | 契约：值域、目录行为、setxattr 拒绝、Baloo 排除建议 |
| `README.md` / `CHANGELOG.md` | 功能行 |
| `xtask/tests/fuse-matrix.sh` | 断言 `getfattr -n user.xdg.tags` 三态 + `getfattr -m -` 名单含它 |

### 验收

1. `getfattr -n user.xdg.tags <挂载点>/file` → `仅在线` / `部分本地` / `已水合`；
2. `getfattr -m - <挂载点>/file` → 名单里有 `user.xdg.tags`；
3. `setfattr -n user.xdg.tags -v x <挂载点>/file` → 失败（ENOTSUP/EPERM）；
4. Dolphin：加 Tags 列 → 三态文字正确；F11 信息面板有 Tags；
5. `head -c` 读文件 → F5 → Tags 从 `仅在线` 变 `部分本地`/`已水合`；
6. `dehydrate` → F5 → Tags 变回 `仅在线`；
7. 目录行的 Tags 为空（目录不水合）；
8. 非 qxync 目录完全不受影响（xattr 只在挂载点内）；
9. Baloo 排除挂载点后 Tags 列仍然工作。

---

## 8. 参考源码

| 组件 | 文件 | 关键行 |
|---|---|---|
| kfilemetadata | `src/usermetadata.cpp` | `tags()` = getxattr("user.xdg.tags") 按逗号切 |
| kfilemetadata | `src/xattr_p.h` | `k_getxattr` 拼 `user.` 前缀；`k_queryAttributes` 走 `listxattr`；`_mapAttribute` 认 `xdg.tags` |
| dolphin | `src/kitemviews/kfileitemmodel.cpp:3220` | tags role：requiresBaloo=true, requiresIndexer=false |
| dolphin | `src/kitemviews/private/kbaloorolesprovider.cpp:161` | Tags 值 = `UserMetaData::tags()`（直接 xattr） |
| dolphin | `src/kitemviews/kfileitemmodelrolesupdater.cpp:332` | Baloo 全局开关 → 创建 FileMonitor |
| baloo-widgets | `src/filefetchjob.cpp:53` | 信息面板同样直接读 UserMetaData |
| baloo | `src/file/basicindexingjob.cpp` | 索引时把 xattr tags 写进索引（`tags:` 搜索用，与本方案无关） |
| baloo | `src/file/filewatch.cpp:143` | IN_ATTRIB → XAttrIndexer（实时刷新链路，FUSE 上不生效） |
