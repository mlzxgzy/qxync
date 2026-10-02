# Arch Linux 打包（AUR：`qxync-bin`）

这里有三个需要维护的文件：

| 文件 | 作用 |
|---|---|
| `PKGBUILD` | AUR 包定义：直接取 GitHub Release 上的预编译 `qxync-<版本>-x86_64-unknown-linux-gnu.tar.gz`，把 `qxync` / `qxyncd` / `qxync-gui` 装进 `/usr/bin`，桌面项 / 图标装进 `/usr/share`，systemd **user** 单元装进 `/usr/lib/systemd/user/` |
| `qxync-bin.install` | 装完打印的提示（`post_install`）：怎么 `systemctl --user enable --now qxyncd`、怎么看日志、以及「开了单元就别再用 `qxync daemon start/stop`」 |
| `.SRCINFO` | AUR 要求的元数据，由 `makepkg --printsrcinfo > .SRCINFO` 生成，**不要手写** |

> ⚠️ **不会**自动启用 `qxyncd.service`：它是 user 单元，得在**每个用户自己的会话**里
> `systemctl --user enable --now qxyncd`（包不能替某个用户做这件事）。想「不登录也常驻」
> 再加 `sudo loginctl enable-linger "$USER"`。

> ⚠️ `sha256sums` 在仓库里是**占位值**（一串 `0`）：Release 产物由 CI 在 ubuntu-22.04 上构建，
> 提交代码时算不出来。发版之后**必须**更新（见下面「发新版本」），否则用户装的时候
> checksum 会失败 —— 是**响亮地失败**，不会静默装错东西。

## 装

三条路，随便挑：

| 方式 | 命令 |
|---|---|
| **Release 里 CI 打好的包**（最快，推荐给用户） | `sudo pacman -U qxync-bin-<版本>-1-x86_64.pkg.tar.zst` |
| 本机自己打（改完 PKGBUILD 后 `updpkgsums` + `makepkg -si`） | 见下面 |
| 发布到 AUR 之后 | `yay -S qxync-bin`（或 `paru` / `pamac`） |

Release 里那份是 CI 用 Arch 官方镜像（`archlinux:base-devel`）里的**真 `makepkg`**、
从**同一个 tar.gz** 打的 —— 包里三个二进制与 Release 资产**逐字节一致**，所以
「下 Release 那份」和「自己 `makepkg`」装出来的东西相同，区别只是校验和由谁填。

自己打（在 `packaging/arch/` 下，需要 Release 已发布）：

```bash
updpkgsums      # 从 Release 下载产物并回填 sha256sums
makepkg -si     # 构建并安装（三个二进制都进 /usr/bin，GUI 会在旁边找到 qxyncd）
```

## 只想先验证打包流程（Release 还没发）

拿本机 `dist/` 里的产物当源就行 —— `makepkg` 会先在 `SRCDEST` 里找同名文件，不会去下载：

```bash
cd packaging/arch
sum=$(sha256sum ../../dist/qxync-0.2.2-x86_64-unknown-linux-gnu.tar.gz | cut -d' ' -f1)
sed -i "s/^sha256sums=.*/sha256sums=('$sum')/" PKGBUILD    # 验证完记得改回占位值再提交
# BUILDDIR 指到临时目录：否则 makepkg 会在本目录留 src/ 与 pkg/（各有 ~180 MB，已在 .gitignore 里）
BUILDDIR=$(mktemp -d) SRCDEST=../../dist makepkg -f
namcap PKGBUILD ./*.pkg.tar.zst
sudo pacman -U qxync-bin-0.2.2-1-x86_64.pkg.tar.zst
```

## 发新版本要做的三件事

```bash
cd packaging/arch
# 1) pkgver 与 tag 对齐（发版流水线的版本守卫会校验这一行，不一致直接拒绝发版）
$EDITOR PKGBUILD
# 2) 用新 Release 的产物回填 sha256sums
updpkgsums
# 3) 重新生成 .SRCINFO
makepkg --printsrcinfo > .SRCINFO
```

然后把 `PKGBUILD` / `.SRCINFO` 推到 AUR（首次要先在 AUR 建 `qxync-bin` 这个包，
并把自己的 SSH 公钥加到 AUR 账号里）：

```bash
git clone ssh://aur@aur.archlinux.org/qxync-bin.git
cp PKGBUILD .SRCINFO qxync-bin.install qxync-bin/
cd qxync-bin && git add PKGBUILD .SRCINFO qxync-bin.install \
  && git commit -m "upgpkg: qxync-bin 0.2.2-1" && git push
```

## 两个有意为之的选择（别「顺手修掉」）

1. **`options=('!strip' '!debug')`**：Release 产物带 `line-tables-only` 调试信息，是为了用户
   报 bug 时贴出来的回溯可读（见 `Cargo.toml` 的 `[profile.release]` 注释）。
   代价是装完约 190 MB，`namcap` 会报 "ELF file is unstripped" —— **这是有意的**。
   `!debug` 同样必要：makepkg 默认会把调试信息切进单独的 `-debug` 包，那就白留了。
2. **`depends` 里有 `fuse3`**：`fuser 0.17` 不链接 libfuse，挂载 / 卸载走的是
   `/usr/bin/fusermount3`（`fuse3` 提供），所以 `ldd` 看不出来但运行必需。
   `namcap` 报的 "Dependency included, but may not be needed ('fuse3')" 是误报。

`namcap` 另外两条告警同样是噪声：三个二进制都带一条
`NEEDED /usr/lib64/ld-linux-x86-64.so.2`（发行版工具链加上的），以及
`gcc-libs` 在本机被 `libgcc` 满足 —— 在 Arch 上游，`libgcc_s.so.1` 正是 `gcc-libs` 提供的。
`namcap PKGBUILD` 还会提示 `arch=('x86_64')` 建议写成 `$CARCH`：AUR 上的惯例就是写死
`x86_64`（`$CARCH` 来自本机 makepkg.conf），这条忽略即可。
