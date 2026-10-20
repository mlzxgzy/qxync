# qxync 的 Dolphin 角标插件

在 Dolphin 的**文件图标**上显示 qxync 的水合状态，和 Windows 资源管理器 / CFA
（Cloud Files API）那套角标一个意思：

| 状态 | 角标 | 含义 |
|---|---|---|
| 已水合 | 绿色对钩 | 内容已在本地缓存，离线可读 |
| 仅在线 | 蓝色云 | 占位符，没有本地内容（打开时才下载） |
| 部分本地 | 半满圆 | 下了一部分区间 |

## 为什么需要插件

Dolphin **没有**「插件往列表里加一列」的接口（列是 `KFileItemModel::rolesInfoMap()`
写死的），Tags 列只能显示文字（M23 的 `user.xdg.tags` 走的就是那条路，见
[`docs/M23-Dolphin集成.md`](../../../docs/M23-Dolphin集成.md)）。但 Dolphin 支持
`KOverlayIconPlugin`：插件按 URL 返回图标名，Dolphin 把最多 3 个画在文件图标的
左下 / 右上 / 右下角（左上角留给 VCS 插件）。这就是这里的实现。

## 实现要点

* 只对 `fstype == fuse.qxync` 的挂载点里的本地文件生效（`/proc/self/mounts`，
  2 秒缓存）；其它路径**一次 xattr 都不读**，普通目录零开销。
* 开关开/关是**热切换**（FUSE 现算属性），关掉后角标立刻消失，不用重挂载；
  但已打开的 Dolphin 窗口要 F5 / 重进目录才刷新。
* **独立开关**：读 `user.qxync.overlay`（不是 Tags 列用的 `user.xdg.tags`）。
  属性不存在（ENODATA）= 角标开关关着（或该挂载点会被 Baloo 索引）→ 不画角标。
  打开方式：GUI 设置 → 高级 →「在文件图标上显示状态角标」（默认关，与 Tags 列
  是两个独立开关，可只开一个；需要先把挂载点排除出 Baloo 索引）。
* 属性值就是三态本身（`placeholder` / `partial` / `hydrated`）；目录是空串 →
  目录不画角标。插件只读、无状态。
* 图标走 hicolor 主题（`qxync-hydrated` 等），Dolphin 用 `QIcon::fromTheme` 解析。
* `getOverlays` 在 Dolphin 主线程调用：只做字符串比较 + 2 次 `getxattr`（本地
  daemon 应答），不联网、不枚举目录。大目录下每个可见条目 2 次 FUSE 往返，
  实测无感（见 M23 文档 §5）。

## 构建与安装

依赖：Qt6 Core、KF6 KIO（Arch：`qt6-base kio cmake`）。

### Arch Linux：装 AUR 包（最省事）

```bash
yay -S qxync-dolphin-overlay        # 源码包：在本机对着系统 KF6 现编，ABI 不会错
```

包定义在 [`packaging/arch-dolphin-overlay/`](../../../packaging/arch-dolphin-overlay)，
装的位置与下面的「系统级」完全相同；装完重启 Dolphin 即可。

### 系统级（推荐 —— Dolphin 直接加载，不需要任何环境变量）

```bash
cd contrib/dolphin/overlay
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build -j
sudo cmake --install build --prefix /usr
```

装到 `/usr/lib/qt6/plugins/kf6/overlayicon/libqxync_overlay.so` 和
`/usr/share/icons/hicolor/scalable/status/qxync-*.svg`。

### 用户级（不用 root，但要让 Dolphin 找得到插件）

```bash
cmake --install build --prefix ~/.local
```

`~/.local/lib/qt6/plugins` **不在** Qt 的默认插件搜索路径里，必须给 Dolphin
设 `QT_PLUGIN_PATH`。持久做法（重新登录后生效）：

```bash
mkdir -p ~/.config/environment.d
echo 'QT_PLUGIN_PATH=$HOME/.local/lib/qt6/plugins' > ~/.config/environment.d/50-qxync-overlay.conf
```

临时验证（先退出所有 Dolphin 窗口）：

```bash
QT_PLUGIN_PATH=$HOME/.local/lib/qt6/plugins dolphin
```

## 生效与排查

* Dolphin 在**创建视图时**加载插件：已经开着的窗口不会热加载，**重启 Dolphin**
  即可。
* 图标来自主题缓存：换过 SVG 后若没更新，`gtk-update-icon-cache ~/.local/share/icons/hicolor`
  或重登一次；Dolphin 进程内的 `QIcon` 缓存按进程存活。
* `QT_DEBUG_PLUGINS=1 dolphin` 可以看到插件是否被加载。
* 查看 xattr 原文：`getfattr -d <文件>`（`user.qxync.overlay` / `user.qxync.state`）。

## 已知限制

* 只支持本地 `file://`（Dolphin 里的 `sftp://`、`smb://` 等不画）。
* 角标大小跟 Dolphin 的图标尺寸走：详情视图默认 16 px 图标 → 8 px 角标（对钩/云
  还能认出来，但确实小）；想更明显就把图标尺寸调大（22/32 px → 16/22 px 角标）。
* 最多 3 个角标；状态变化不会实时推送（F5 / 重进目录后更新，和 Tags 列一致）。
* 只支持 KF6 / Qt6 的 Dolphin（本仓库的目标环境）。

## 卸载

```bash
sudo rm /usr/lib/qt6/plugins/kf6/overlayicon/libqxync_overlay.so \
        /usr/share/icons/hicolor/scalable/status/qxync-{hydrated,placeholder,partial}.svg
```
