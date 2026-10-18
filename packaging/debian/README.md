# Debian / Ubuntu 打包（packaging/debian）

这个目录 + `xtask/release/{apt-mirror.sh,package-deb.sh}` 负责把 GitHub Release 的
tar.gz **重打包**成**一个**通用 `.deb`。**不重新编译**：三个二进制就是 Release 资产里那一份
（与 Arch 包 `packaging/arch/PKGBUILD` 的 `qxync-bin`、以及直接下载的裸二进制同源）。

```
GitHub Actions (ubuntu-22.04, cargo build --release)
        │  xtask/release/package-linux.sh
        ▼
dist/qxync-<ver>-x86_64-unknown-linux-gnu.tar.gz   ← 唯一的输入
        │  xtask/release/package-deb.sh（在 ubuntu:22.04 或 debian:12 基线容器里跑）
        ▼
dist/qxync_<ver>-1_amd64.deb                       ← 唯一的产物
```

一个包同时适用于 **Debian 12 / 13** 与 **Ubuntu 22.04 / 24.04 / 26.04**（见下「支持矩阵」）。

## 为什么只有一个包

早先按发行版出过两个包（`…-1~deb12`、`…-1~ubuntu22.04`）。实测对比两个包的内容：

```bash
# 两个旧包分别解包后逐文件比对（dpkg-deb -x）
$ diff -rq old-deb12 old-ubuntu2204
Files .../usr/share/doc/qxync/changelog.Debian.gz differ     # ← 唯一差异
```

* payload 共 **16 个文件**（3 个二进制、1 个 desktop、4 张图标、2 个 systemd user 单元、
  6 个文档），除 `changelog.Debian.gz`（内含版本串/发行版代号/构建时间）外**逐字节相同**；
  三个二进制的 sha256 在两个包里完全一致（与 Release 裸二进制也一致）。
* 两个包 `control` 里的 `Depends` 字符串**逐字相同**，`Installed-Size` 也相同（191907）。
* 原因：`dpkg-shlibdeps` 在 Debian 12 与 Ubuntu 22.04 两个基线里算出的版本下限恰好一致
  （两个发行版的 glibc/GTK/WebKitGTK 下限没有分叉）。

既然 payload 与依赖都一样，就没有理由维护两个几乎相同的包 —— 合并成
`qxync_<ver>-1_amd64.deb`，一个文件对所有支持的发行版通用。

## 支持矩阵

上游二进制是在 Ubuntu 22.04 上构建的，`objdump -T` 实测符号版本上限是 **`GLIBC_2.34`**：

| 发行版 | 版本 | 安装 + 运行 | 说明 |
|---|---|---|---|
| Debian 12 (bookworm) | glibc 2.36 | ✅ 实装验证 | 打包基线之一 |
| Debian 13 (trixie) | glibc 2.41 | ✅ 实装验证 | `libgtk-3-0t64` `Provides: libgtk-3-0` |
| Ubuntu 22.04 (jammy) | glibc 2.35 | ✅ 实装验证 | 打包基线之一 |
| Ubuntu 24.04 (noble) | glibc 2.39 | ✅ 实装验证 | 同上 t64 Provides |
| Ubuntu 26.04 | glibc 2.41+ | ✅ 实装验证 | 同上 t64 Provides |

* 更老的发行版（Debian 11 / Ubuntu 20.04，glibc 2.31）**跑不了**：二进制需要 glibc ≥ 2.34。
* 更新的发行版能装同一份包，是因为 GTK 3 的包改名后 `Provides: libgtk-3-0 (= 3.24.x)`，
  带版本下限的 `Depends` 仍可满足。如果某天某个发行版不再提供这个 Provides，装的时候会报
  `libgtk-3-0 : Depends: ... but it is not installable` 之类的错 —— 那是发行版改名，
  不是包坏了（见下「什么情况下才需要再拆成两个包」）。

## 目录内容

| 文件 | 作用 | 装进包吗 |
|---|---|---|
| `control.in` | 控制文件模板，占位符 `@VERSION@` `@ARCH@` `@INSTALLED_SIZE@` `@DEPENDS@` 由脚本替换 | 是（→ `DEBIAN/control`） |
| `copyright` | DEP-5 机器可读版权；MIT 全文内联，Apache-2.0 引 `/usr/share/common-licenses/Apache-2.0` | 是（→ `/usr/share/doc/qxync/copyright`） |
| `README.Debian` | 给**用户**看：systemd user 单元、enable-linger、多 NAS、卸载 | 是（→ `/usr/share/doc/qxync/README.Debian`） |
| `README.md` | 本文件，给**维护者**看 | 否 |

包里的其余文件都从 tar.gz 直接取（`README.md` / `README.en.md` / `DISCLAIMER.md` /
`qxync.desktop` / 四张图标 / 两个 systemd user 单元），不在打包器里另抄一份，避免
「图标换了只改一处」的问题。

## 包内容（`dpkg -L qxync` 应包含）

```
/usr/bin/qxync                              0755
/usr/bin/qxyncd                             0755
/usr/bin/qxync-gui                          0755
/usr/share/applications/qxync.desktop       0644
/usr/share/icons/hicolor/32x32/apps/qxync.png      ← icons/32x32.png
/usr/share/icons/hicolor/128x128/apps/qxync.png    ← icons/128x128.png
/usr/share/icons/hicolor/256x256/apps/qxync.png    ← icons/128x128@2x.png（实际 256×256）
/usr/share/icons/hicolor/512x512/apps/qxync.png    ← icons/icon.png（实际 512×512）
/usr/lib/systemd/user/qxyncd.service                systemd **user** 单元（不是 system 单元）
/usr/lib/systemd/user/qxyncd@.service               多连接模板单元
/usr/share/doc/qxync/{README.md,README.en.md,DISCLAIMER.md,README.Debian,copyright,changelog.Debian.gz}
```

不装 `/usr/share/licenses/`（那是 Arch 的习惯），许可证统一由 `copyright` 承载。
不装 man page（上游没写），`--help` 有完整用法。

## 版本号约定

```
<上游版本>-<打包修订号>
0.6.4-1          ← 单包，不再带 ~deb12 / ~ubuntu22.04 这样的发行版后缀
```

* `-1` 是**打包修订号**：只改打包方式（control / 依赖 / 文件布局）而不换上游时递增到 `-2`。
* changelog 的**发行版字段**取打包环境 `/etc/os-release` 的 `VERSION_CODENAME`
  （ubuntu:22.04 → `jammy`、debian:12 → `bookworm`）；某些精简镜像没有该字段时按基线映射兜底。
  这个字段只影响 changelog 的可读性，不影响安装（本地 deb 不校验发行版代号）。
* 需要可复现构建时设 `SOURCE_DATE_EPOCH`，脚本用它写 changelog 日期。

### 从旧的双包升级

旧包 `0.6.4-1~deb12` / `0.6.4-1~ubuntu22.04` 与新的单包是同一个包名（`qxync`），
且 `~` 在 dpkg 版本比较里排在「什么都没有」之前：

```bash
$ dpkg --compare-versions '0.6.4-1~deb12' lt '0.6.4-1' && echo yes
yes
$ dpkg --compare-versions '0.6.4-1~ubuntu22.04' lt '0.6.4-1' && echo yes
yes
```

所以老用户 `apt update && apt upgrade`（或 `apt-get install -y ./qxync_0.6.4-1_amd64.deb`）
会直接把旧的变体包升成单包，**不需要**先卸载。

## 构建

### 容器里打（推荐；宿主不必是 Debian）

**基线只能是 `ubuntu:22.04` 或 `debian:12`**，脚本会校验并拒绝其它环境。
推荐 `ubuntu:22.04`：它是最老的 Ubuntu 目标，也是 CI 编译 Release 二进制的同一个环境，
依赖下限与二进制的代际最匹配；`debian:12` 打出来的 `Depends` 与它**逐字相同**（已实测）。

```bash
# ubuntu:22.04（推荐基线）
docker run --rm -v "$PWD:/work" -w /work \
  --env HOST_UID=$(id -u) --env HOST_GID=$(id -g) \
  ubuntu:22.04 bash -lc \
  'bash xtask/release/apt-mirror.sh && bash xtask/release/package-deb.sh 0.6.4'

# debian:12（等价基线，产物 payload 相同、Depends 相同）
docker run --rm -v "$PWD:/work" -w /work \
  --env HOST_UID=$(id -u) --env HOST_GID=$(id -g) \
  debian:12 bash -lc \
  'bash xtask/release/apt-mirror.sh && bash xtask/release/package-deb.sh 0.6.4'
```

* 用法：`xtask/release/package-deb.sh <version> [out-dir]`（`<version>` 不带 `v`；
  也可以只给环境变量 `VERSION`）。`[out-dir]` 默认 `<repo>/dist`。
* `HOST_UID`/`HOST_GID`：容器里以 root 打包，产物会 chown 回宿主 uid/gid，
  否则 `dist/` 里会冒出 root 属主的文件（与 release.yml 里 Arch 那步同样的处理）。
* `package-deb.sh` **自己装打包依赖**（`dpkg-dev` + 算依赖要查 shlibs 的运行库，见下），
  调用方只要先换源；重复跑会跳过已装的包。
* 输出文件名固定：`dist/qxync_<ver>-1_amd64.deb`（架构取自 `dpkg --print-architecture`）。

### ⚠️ 输入必须是 CI 构建的 Release 原件

`package-deb.sh` 只认 `<repo>/dist/qxync-<ver>-x86_64-unknown-linux-gnu.tar.gz`，但**这个 tar.gz
必须来自 CI**（`.github/workflows/release.yml` 在 `ubuntu-22.04` 上 `cargo build --release`）。
用开发机（例如 Arch，glibc 2.44）自己编的二进制来打包，依赖解析和安装都没问题，但
**在 Debian 12 / Ubuntu 22.04 里跑不起来**：

```bash
$ objdump -T qxync | grep -oE 'GLIBC_[0-9.]+' | sort -uV | tail -1
GLIBC_2.39     # 开发机（Arch, glibc 2.44）本地编的 qxync / qxync-gui
GLIBC_2.34     # GitHub Release 原件（CI 在 ubuntu-22.04 上编）的 qxync / qxync-gui
```

老发行版里的运行时报错是
`weak version 'GLIBC_2.39' not found (required by /usr/bin/qxync)`，
`ldd` 也因此会显示 `not found`。这是**输入二进制的限制，不是打包错误**：要验证老发行版，
用 Release 原件（或按 CI 的方式在 ubuntu-22.04 容器里重编）。本仓库的验证全部使用
Release 原件（sha256 与 Release 公布的 `SHA256SUMS` 对得上）。

### 本地打（宿主必须已经是 ubuntu 22.04 或 debian 12）

脚本会先校验 `/etc/os-release`：必须是 Debian 系（`ID` 或 `ID_LIKE` 含 debian），
且**版本恰好是 `ubuntu 22.04` 或 `debian 12`**；否则直接报错退出（防止在 Arch/更新发行版上
「顺手打一个」导致 `Depends` 下限被抬高）。

```bash
# 需要 root（要装 dpkg-dev 等）
bash xtask/release/package-deb.sh 0.6.4
```

### 国内镜像

```bash
bash xtask/release/apt-mirror.sh                              # 默认清华 http
QXNYC_APT_MIRROR=http://mirrors.aliyun.com bash xtask/release/apt-mirror.sh
bash xtask/release/apt-mirror.sh https://mirrors.ustc.edu.cn  # 位置参数优先级最高
```

* 默认用 **http**：`debian:12` / `ubuntu:22.04` 基础镜像里没有 `ca-certificates`，
  https 源会直接证书失败（而 apt 又没法自己下 ca-certificates 来救）。
* 两种格式都处理：经典 `/etc/apt/sources.list`（+ `sources.list.d/*.list`）与
  deb822 `*.sources` 的 `URIs:` 行（一行多 URL 也处理）。实测 `debian:12` 是 deb822、
  `ubuntu:22.04` 是经典格式，别按版本号想当然。
* 按 `ID` / `ID_LIKE` 决定镜像站路径 `/debian` 或 `/ubuntu`；`debian-security`、
  `ubuntu-ports` 等路径保留原样；认不出的第三方源不动。
* 幂等：已经指向目标镜像的文件不会再改，第二次跑只打印「未改动」。

## Depends 从哪来

```bash
# dpkg-shlibdeps 必须在一个「有 debian/control」的目录里跑（它靠 control 决定依赖记到
# 哪个包上）。脚本会给它一个一次性的最小 control，不在 repo 里建 debian/：
(cd "$tmp/debian-pkg" && dpkg-shlibdeps -O "$stage/usr/bin"/{qxync,qxyncd,qxync-gui})
# → shlibs:Depends=libc6 (>= 2.34), libcairo2 (>= 1.10.0), ...
```

* 在**打包基线**（最老的目标发行版）里对三个二进制实测，取各自 `DT_NEEDED` 对应包的
  shlibs 版本下限，合并去重（多行 `shlibs:Depends=` 合并、去重、保序）。
  只在最老的基线上算，更新的发行版才能装：若在 ubuntu:24.04 里算，会得到
  `libc6 (>= 2.39)` 这种下限，包就装不进 Debian 12 / Ubuntu 22.04 了。
* 实测两个基线（debian:12 / ubuntu:22.04）算出的 `Depends` **逐字相同**，所以只出一个包。
  当前实际值：
  `libc6 (>= 2.34), libcairo2 (>= 1.10.0), libdbus-1-3 (>= 1.9.14), libgcc-s1 (>= 4.2),
  libgdk-pixbuf-2.0-0 (>= 2.36.9), libglib2.0-0 (>= 2.65.1), libgtk-3-0 (>= 3.21.5),
  libjavascriptcoregtk-4.1-0, libsoup-3.0-0 (>= 3.0.3), libwebkit2gtk-4.1-0 (>= 2.41.90),
  fuse3`。
* 额外**手工加 `fuse3`**：挂载 / 卸载要调 `fusermount3`（fuse3 包里的命令行工具），
  但 `qxync` 自身**不链接 libfuse**（FUSE 协议走内核字符设备），`dpkg-shlibdeps` 看不出这个
  运行时依赖 —— 不加的话装完能跑但一挂载就报「找不到 fusermount3」。
* `dpkg-shlibdeps` 的 warning（例如某个库缺少 shlibs 信息）按设计忽略，脚本只统计条数。
* `Installed-Size` = 暂存树 `du -sk --apparent-size`（不含 `DEBIAN/`），单位 KiB。

## 为什么不 strip

与 Arch 包的口径一致，**有意不 strip**：

* `Cargo.toml` 的 `[profile.release]` 是 `debug = "line-tables-only"`：
  保留行号表、去掉类型/变量信息，体积代价可控，但用户报 bug 贴出来的回溯**有行号**。
* `packaging/arch/PKGBUILD` 对应 `options=('!strip' '!debug')`（`!debug` 防止 makepkg
  把调试信息切进单独的 `-debug` 包，那就白留了）。
* 代价：deb 约 **36 MiB**，装完约 **187 MiB**（`Installed-Size: 191908`）；
  `lintian` 会报 `unstripped-binary-or-object`（预期保留，见下）。
* **不要**为了消 lintian 告警去 strip：回溯变不可读，与发版口径冲突。

## lintian 预期告警

在容器里 `apt-get install -y lintian && lintian dist/qxync_<ver>-1_amd64.deb`。
**不加 `--fail-on`**（不卡等级）。实测（Debian 12 容器 lintian `2.116.3+deb12u1` 与
Ubuntu 22.04 容器 lintian `2.114.0ubuntu1.8` 跑同一个单包，两边完全一致）：

| 告警 | 级别 | 条数 | 判定 | 说明 |
|---|---|---|---|---|
| `unstripped-binary-or-object`（`usr/bin/qxync`、`qxyncd`、`qxync-gui`） | E | 3 | **预期保留** | 见上「为什么不 strip」；消掉只能 strip，与发版口径冲突 |
| `no-manual-page`（三个二进制各一条） | W | 3 | **预期保留** | 上游没有 man page，`--help` 有完整用法 |
| `initial-upload-closes-no-bugs` | W | 1 | **预期保留** | 版本串从 `-1~deb12` 变成干净的 `-1` 后，lintian 把这当成「首次上传」启发式；本包不上传 Debian 官方仓库，无 bug 号可关。要消掉只能在 changelog 里编一个假 `Closes:`，不做 |
| 其它任何告警 | — | **0** | 无此告警 | 没有 changelog / md5sums / copyright / desktop / description / 文档位置 之类的问题 |

在容器里以 root 跑会多一行 `running with root privileges is not recommended!`（忽略）。

## 干净容器实装验证

用**真实 Release 资产**（`v0.6.4` 的 tar.gz，sha256 `7a3c69fe…` 与 Release 公布的 `SHA256SUMS`
一致）打的单包，在 5 个干净容器里 `apt-get install ./dist/qxync_0.6.4-1_amd64.deb` 并跑运行时自检。
本次实测全部 **ALL PASS**（`dist/qxync_0.6.4-1_amd64.deb`，sha256 `384f9b04…`）：

| 干净容器 | 安装（依赖解析） | `qxync/qxyncd --version` | `ldd` 无缺库 | `--self-test` 出 JSON | `dpkg -s` Version |
|---|---|---|---|---|---|
| `debian:12`（glibc 2.36） | ✅ | ✅ `0.6.4` | ✅ | ✅（`ui_spec_ok: true`） | ✅ `0.6.4-1` |
| `debian:13`（glibc 2.41） | ✅ | ✅ `0.6.4` | ✅ | ✅ | ✅ `0.6.4-1` |
| `ubuntu:22.04`（glibc 2.35） | ✅ | ✅ `0.6.4` | ✅ | ✅ | ✅ `0.6.4-1` |
| `ubuntu:24.04`（glibc 2.39） | ✅ | ✅ `0.6.4` | ✅ | ✅ | ✅ `0.6.4-1` |
| `ubuntu:26.04`（glibc 2.41+） | ✅ | ✅ `0.6.4` | ✅ | ✅ | ✅ `0.6.4-1` |

断言命令（每个容器都跑一遍）：

```bash
# 干净容器里：先换源（国内），再装
bash xtask/release/apt-mirror.sh
apt-get update
apt-get install -y ./dist/qxync_0.6.4-1_amd64.deb    # 用 apt 而不是 dpkg -i：会按 Depends 拉库
qxync --version        # → qxync 0.6.4
qxyncd --version       # → qxyncd 0.6.4
ldd /usr/bin/qxync-gui | grep 'not found' && echo '❌ 缺库' || echo '✅ 无缺库'
qxync-gui --self-test  # 无头容器里退出码非 0 属预期，但必须能跑出 JSON
dpkg -L qxync | grep -E 'usr/(bin|share|lib)'    # 文件齐全（含两个 systemd 单元与 4 张图标）
dpkg -s qxync | grep -E '^(Version|Depends|Installed-Size)'
systemd-analyze verify /usr/lib/systemd/user/qxyncd.service   # 单元语法 OK
```

注意（踩过的坑，改验证脚本时别踩第二次）：

* **Ubuntu 官方 docker 镜像默认丢弃 `/usr/share/doc/*`**（`/etc/dpkg/dpkg.cfg.d/excludes`，
  镜像瘦身用），只保留 `copyright` 与 `changelog.*`。所以「`/usr/share/doc/qxync/README.md`
  是否存在」这类断言在 Ubuntu 容器里会**假失败**，而 `dpkg -L` 明明列出了它。验证文档落盘前
  先把那个 excludes 文件临时移走（`mv /etc/dpkg/dpkg.cfg.d/excludes /tmp/`），只影响验证容器。
  Debian 官方镜像没有这个策略。
* **别多容器共享同一个 apt archives 卷并发跑**：`apt-get install` 会互相抢
  `/var/cache/apt/archives/lock`，或者报 `cannot stat .../tmp/apt-dpkg-install-*`。并发验证
  就每个容器一个独立缓存卷。
* `qxync-gui --self-test` 在无头容器里因为连不上 daemon / 没有登录会话，会输出
  `"ok": false` 并返回非 0，这是**允许**的（`ui_spec_ok` 等内部检查仍是 true）；
  要求是「有 JSON 输出」+ `ldd` 无 `not found`（GUI 的库依赖齐全）。
* 用 `apt-get install ./dist/qxync_*.deb` 而不是 `dpkg -i`：前者按 `Depends` 自动装库，
  正好验证依赖写得对。

## 什么情况下才需要再拆成两个包

合并的前提是「一份 `Depends` + 一份 payload 对所有目标发行版都成立」。出现下面任一情况，
才需要恢复 `~debXX` / `~ubuntuXX.YY` 变体（并恢复脚本的 distro-id 参数与 CI matrix）：

1. **依赖真的分叉**：某个库在两个基线里的 shlibs 版本下限不同，且取最老下限会装不进另一个
   基线（例如未来某发行版要求更高的 `libc6`/`libwebkit2gtk` 下限，而老发行版又满足不了）。
2. **关键包改名且没有 Provides**：像 `libgtk-3-0` → `libgtk-3-0t64` 这种事再发生一次、
   而新包不再 `Provides` 旧名，同一份带版本下限的依赖就无法两边都满足。
3. **需要按发行版做 maintainer script / 补丁**：例如某个发行版要额外的 systemd 依赖、
   不同的文件布局，或要跳过某段安装逻辑。

届时用 `dpkg --compare-versions` 确认升级路径（`~` 后缀的变体包 < 同版本无后缀包，
老用户能自动升级），并把支持矩阵与验证表一起更新。

## CI 接入

发版流水线在构建完 tar.gz 之后打**一个** deb，并在干净容器里做实装校验。本目录的脚本就是
流水线调用的东西，本地复跑与 CI 是同一套命令；workflow 文件由
`.github/workflows/release.yml` 维护，不在本目录。

CI 在五个干净容器里各 `apt-get install ./deb` + 跑版本/自检：`debian:12` / `debian:13` /
`ubuntu:22.04` / `ubuntu:24.04` / `ubuntu:26.04`，覆盖两个发行版家族的新旧两端；
实测结果见上「干净容器实装验证」表。

## 已知限制

* 只出 **amd64**（上游 tar.gz 只有 `x86_64-unknown-linux-gnu`）。
* 只出**二进制包**，没有 `debian/rules` / source package，因此**不能**直接进 Debian 官方仓库
  （官方要求可源码重建）。要从源码构建见仓库根 README。
* 没有 APT 仓库与签名：用户需从 GitHub Release 下载 `.deb` 后手动 `apt-get install ./xxx.deb`。
* 未 strip（见上）：deb 压缩后约 **36 MiB**（实测 37,543,792 字节），
  装完约 **187 MiB**（`Installed-Size: 191908 KiB`）。
* 包里的 `changelog.Debian.gz` 含构建时间，**重复构建的 sha256 会变**；要可复现就设
  `SOURCE_DATE_EPOCH`。
