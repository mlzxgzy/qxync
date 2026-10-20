# Arch Linux 打包（AUR：`qxync-dolphin-overlay`）

Dolphin 文件图标上的 qxync 水合状态角标（`KOverlayIconPlugin`）。源码在
[`contrib/dolphin/overlay/`](../../contrib/dolphin/overlay/)，这里只是一个小小的
AUR 包定义。

| 文件 | 作用 |
|---|---|
| `PKGBUILD` | 从 GitHub tag 的源码 tarball 取 `contrib/dolphin/overlay`，本机对着**系统 KF6** 现编，装到 `/usr/lib/qt6/plugins/kf6/overlayicon/libqxync_overlay.so` + `/usr/share/icons/hicolor/scalable/status/qxync-*.svg` |
| `.SRCINFO` | 由 `makepkg --printsrcinfo > .SRCINFO` 生成，**不要手写** |

## 为什么是源码包（不是 `-bin`）

插件是 KF6 的 C++ 共享库，直接链 `libKF6KIOCore`。预编译的 `.so` 换个 KF6 版本
就可能加载失败甚至把 Dolphin 带崩；而且 `qxync-bin` 那个 release tar.gz 里也
只有三个 Rust 二进制。所以这个包**必须**在用户机器上编译（编译时间几秒）。

## 装

| 方式 | 命令 |
|---|---|
| AUR（发布之后） | `yay -S qxync-dolphin-overlay` |
| 本机打（改完 PKGBUILD 后） | `makepkg -si` |
| 不装包、直接编 | 见 [`contrib/dolphin/overlay/README.md`](../../contrib/dolphin/overlay/README.md) |

装完要**重启 Dolphin**（Dolphin 只在创建视图时加载插件），然后在
GUI「设置 → 高级 → 在文件图标上显示状态角标」打开开关。前置条件：挂载点已
排除出 Baloo 索引（`qxync baloo-status`）。

## 发新版本

```bash
# pkgver 与 tag 对齐；源码 tarball 的 sha256 会变
updpkgsums
makepkg --printsrcinfo > .SRCINFO
git push aur master            # AUR 仓库（ssh://aur@aur.archlinux.org/qxync-dolphin-overlay.git）
```

> ⚠️ 源码 tarball 用的是 GitHub 的 `archive/refs/tags/v<版本>.tar.gz`。GitHub 的自动
> tarball 理论上由它按需生成，哈希对同一个 tag 长期稳定；若哪天 `updpkgsums` 报
> 不匹配，重新跑一次即可（真出问题就把源码 tarball 改成随 Release 发布的资产）。

## 依赖说明

* `depends=('qt6-base' 'kio')` —— `libQt6Core` + `libKF6KIOCore`（`namcap` 口径）；
* `optdepends=('qxync-bin: …')` —— 插件本身不依赖主程序，但没主程序就没有
  `user.qxync.overlay` 数据可显示。
