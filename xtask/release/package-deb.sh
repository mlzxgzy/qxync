#!/usr/bin/env bash
# 从 Release 的 tar.gz 重打成**一个** Debian / Ubuntu 通用的 .deb（**不重新编译**）。
#
# 用法（必须在**打包基线**发行版的容器里跑：ubuntu 22.04 或 debian 12）：
#   xtask/release/package-deb.sh <version> [out-dir]
#
#   <version>    不带 v 前缀，例如 0.6.4（也可用环境变量 VERSION 传）
#   [out-dir]    默认 <repo>/dist
#
# 输入（必须先有，由 xtask/release/package-linux.sh 产出）：
#   <repo>/dist/qxync-<version>-x86_64-unknown-linux-gnu.tar.gz
#
# 输出（单包，不再区分发行版变体）：
#   <out-dir>/qxync_<version>-1_amd64.deb
#   例：qxync_0.6.4-1_amd64.deb
#
# 为什么只有一个包：三个二进制来自同一份 Release tar.gz，`dpkg-shlibdeps` 在
# debian:12 与 ubuntu:22.04 两个基线里算出的 Depends **逐字相同**，两个包的 payload
# 也逐字节相同（除 Version 串与 changelog）。既然内容一样，就没必要出两个包。
#
# 为什么只能在 ubuntu 22.04 / debian 12 里打：Depends 必须按**最老的目标发行版**算。
# 在更新的发行版（24.04 / debian 13）里算，版本下限会被抬高（例如 libc6 (>= 2.39)），
# 包就装不进老发行版了。更新的发行版靠 t64 的 Provides 满足这些 ≥ 下限，不需要单独打。
#
# 设计要点：
#   * 包里就是 Release tar.gz 里的那**三个二进制**（与 Arch 包 / 下载的裸二进制同源），
#     不 strip（与 Cargo.toml 的 release profile、packaging/arch/PKGBUILD 一致），
#     代价是装完约 190 MB。
#   * Depends 在打包基线里用 dpkg-shlibdeps 对三个二进制实测得出，再加手工的 fuse3
#     （挂载/卸载要调 fusermount3，而 qxync 自身不链接 libfuse3，dpkg-shlibdeps
#     看不出这个运行时依赖）。
#   * changelog.Debian 的发行版字段取 /etc/os-release 的 VERSION_CODENAME
#     （为空时按基线映射：ubuntu 22.04 → jammy、debian 12 → bookworm）。
#   * 幂等：重复跑覆盖同名 deb；临时目录用 mktemp，退出即删，不碰任何被 git 跟踪的文件。
#   * HOST_UID / HOST_GID：容器里以 root 跑时，把生成的 deb chown 回宿主 uid/gid
#     （与 .github/workflows/release.yml 里 Arch 那步同样的做法），否则 dist/ 里会出现
#     root 属主的文件。
#
# 容器里完整用法（国内镜像先换源，再打包；基线用 ubuntu:22.04 或 debian:12）：
#   docker run --rm -v "$PWD:/work" -w /work \
#     --env HOST_UID=$(id -u) --env HOST_GID=$(id -g) ubuntu:22.04 \
#     bash -lc 'bash xtask/release/apt-mirror.sh && bash xtask/release/package-deb.sh 0.6.4'
#
# 本地（宿主必须已经是 ubuntu 22.04 或 debian 12）：
#   bash xtask/release/package-deb.sh 0.6.4
set -euo pipefail

die() { echo "❌ $*" >&2; exit 1; }

usage() {
  # 打印文件头注释块（跳过 shebang，到第一个非注释行停）
  awk 'NR==1 { next } /^#/ { print; next } { exit }' "${BASH_SOURCE[0]}" \
    | sed 's/^#\{1,\} \{0,1\}//' >&2
  exit 2
}

version="${1:-${VERSION:-}}"
[ -n "$version" ] || usage
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
out_dir="${2:-$root/dist}"

case "$version" in
  v*) die "version 不要带 v 前缀：$version（tag 才带 v）" ;;
esac
# 版本串里除了数字/字母/点/加号/波浪号/冒号，别的都可能弄坏文件名或 dpkg 解析
case "$version" in
  *[!0-9A-Za-z.+~:]*) die "version 含非法字符：$version" ;;
esac

# ---- 1. 校验运行环境：必须是 Debian 系，且是打包基线 ----------------------
# 不在基线容器里跑的话，dpkg-shlibdeps 算出来的依赖下限是错的（要么装不进老发行版，
# 要么把新库版本当成必需项）。这里只放行 ubuntu 22.04 与 debian 12。
os_field() {
  [ -r /etc/os-release ] || return 1
  sed -n "s/^$1=//p" /etc/os-release | head -n1 | tr -d '"'
}
cur_id="$(os_field ID || true)"
cur_like="$(os_field ID_LIKE || true)"
cur_ver="$(os_field VERSION_ID || true)"
cur_codename="$(os_field VERSION_CODENAME || true)"

is_debian_family=0
case "$cur_id" in
  debian|ubuntu) is_debian_family=1 ;;
esac
# 派生发行版（ID_LIKE 里含 debian，例如 LMDE）也算 Debian 系，但仍要过基线检查
case " $cur_like " in
  *" debian "*) is_debian_family=1 ;;
esac
if [ "$is_debian_family" -ne 1 ]; then
  die "当前系统不是 Debian / Ubuntu（/etc/os-release: ID=${cur_id:-?} ID_LIKE=${cur_like:-?}）。
   这个脚本只能在 Debian 系容器里跑（dpkg-shlibdeps 需要 dpkg / shlibs 数据）。"
fi

case "$cur_id:$cur_ver" in
  debian:12)
    baseline_codename=bookworm
    ;;
  ubuntu:22.04)
    baseline_codename=jammy
    ;;
  *)
    die "打包基线只允许 **ubuntu 22.04 或 debian 12**（收到：ID=${cur_id:-?} VERSION_ID=${cur_ver:-?}）。
   为什么必须用最老的目标基线：Depends 是在这个环境里用 dpkg-shlibdeps 实测出来的，
   在更新的发行版（ubuntu 24.04 / debian 13）里算，版本下限会被抬高（如 libc6 (>= 2.39)），
   打出来的包就装不进 debian 12 / ubuntu 22.04 这些老发行版了。
   更新的发行版不需要单独打包：它们用 t64 的 Provides 满足同一份 Depends。

   用基线容器打：
     docker run --rm -v \"\$PWD:/work\" -w /work \\
       --env HOST_UID=\$(id -u) --env HOST_GID=\$(id -g) ubuntu:22.04 \\
       bash -lc 'bash xtask/release/apt-mirror.sh && bash xtask/release/package-deb.sh $version'
   或把 ubuntu:22.04 换成 debian:12。"
    ;;
esac
# changelog 的发行版字段：优先 os-release 里的代号，缺了（某些精简镜像会没有）就按基线映射
deb_dist="${cur_codename:-$baseline_codename}"

# ---- 2. 检查架构与输入 tar.gz --------------------------------------------
command -v dpkg >/dev/null || die "找不到 dpkg —— 这不是 Debian/Ubuntu 系统？"
deb_arch="$(dpkg --print-architecture)"
[ "$deb_arch" = amd64 ] || die "本包只提供 amd64（x86_64）：当前 dpkg 架构是 $deb_arch"

triple=x86_64-unknown-linux-gnu
stem="qxync-${version}-${triple}"
tarball="$root/dist/${stem}.tar.gz"
if [ ! -f "$tarball" ]; then
  die "找不到输入：$tarball
   先打 Release 产物（tar.gz 里就是本包要装的那三个二进制）：
     bash xtask/release/package-linux.sh $version $triple"
fi

created_out_dir=0
[ -d "$out_dir" ] || created_out_dir=1
mkdir -p "$out_dir"
out_dir="$(cd "$out_dir" && pwd)"
debversion="${version}-1"
deb_file="${out_dir}/qxync_${debversion}_${deb_arch}.deb"

# ---- 3. 确保 dpkg-shlibdeps 与它要查 shlibs 的库都在 ---------------------
# 只装**运行库**（不装 -dev）：dpkg-shlibdeps 要的是 /var/lib/dpkg/info/*.shlibs，
# 装了运行库就能拿到带版本下限的依赖，不需要头文件。
shlib_pkgs=(
  dpkg-dev
  libwebkit2gtk-4.1-0
  libgtk-3-0
  libgdk-pixbuf-2.0-0
  libcairo2
  libdbus-1-3
  libsoup-3.0-0
  libfuse3-3
)
missing_pkgs=()
for p in "${shlib_pkgs[@]}"; do
  dpkg -s "$p" >/dev/null 2>&1 || missing_pkgs+=("$p")
done
if [ "${#missing_pkgs[@]}" -gt 0 ]; then
  echo "==> 安装打包依赖（dpkg-shlibdeps 需要它们算 Depends）：${missing_pkgs[*]}"
  if [ "$(id -u)" -ne 0 ]; then
    die "缺少 ${missing_pkgs[*]}，且当前不是 root，无法自动安装。
   请先跑：bash xtask/release/apt-mirror.sh && apt-get update && apt-get install -y --no-install-recommends ${missing_pkgs[*]}"
  fi
  # 源已经换过/没换过都无所谓：没换源也能装（只是慢）；换源由 apt-mirror.sh 负责。
  apt-get update -qq
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends "${missing_pkgs[@]}"
fi
command -v dpkg-shlibdeps >/dev/null || die "装完仍然找不到 dpkg-shlibdeps（dpkg-dev 没装上？）"

# ---- 4. 解包 + 暂存目录树 ------------------------------------------------
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
tar -xzf "$tarball" -C "$work"
src="$work/$stem"
[ -d "$src" ] || die "tar.gz 里没有预期的 $stem/ 目录：$tarball"
for b in qxync qxyncd qxync-gui; do
  [ -f "$src/$b" ] || die "tar.gz 里缺二进制：$stem/$b"
done

stage="$work/root"
bin_dst="$stage/usr/bin"
install -d -m 0755 "$bin_dst" \
  "$stage/usr/share/applications" \
  "$stage/usr/share/icons/hicolor/32x32/apps" \
  "$stage/usr/share/icons/hicolor/128x128/apps" \
  "$stage/usr/share/icons/hicolor/256x256/apps" \
  "$stage/usr/share/icons/hicolor/512x512/apps" \
  "$stage/usr/lib/systemd/user" \
  "$stage/usr/share/doc/qxync" \
  "$stage/DEBIAN"

# 三个二进制装 /usr/bin（GUI 先找「自己旁边的 qxyncd」，同目录安装最省事）。
install -m 0755 "$src/qxync"      "$bin_dst/qxync"
install -m 0755 "$src/qxyncd"     "$bin_dst/qxyncd"
install -m 0755 "$src/qxync-gui"  "$bin_dst/qxync-gui"

# 桌面项与图标（tar.gz 里带着，避免打包器各抄一份）
install -m 0644 "$src/qxync.desktop" "$stage/usr/share/applications/qxync.desktop"
# PNG 实际尺寸：32x32.png=32²、128x128.png=128²、128x128@2x.png=256²、icon.png=512²
install -m 0644 "$src/icons/32x32.png"    "$stage/usr/share/icons/hicolor/32x32/apps/qxync.png"
install -m 0644 "$src/icons/128x128.png"  "$stage/usr/share/icons/hicolor/128x128/apps/qxync.png"
install -m 0644 "$src/icons/128x128@2x.png" "$stage/usr/share/icons/hicolor/256x256/apps/qxync.png"
install -m 0644 "$src/icons/icon.png"     "$stage/usr/share/icons/hicolor/512x512/apps/qxync.png"

# systemd **user** 单元：qxyncd 是某个用户的同步客户端（$HOME / $XDG_RUNTIME_DIR /
# 用户自己的 FUSE 挂载点），所以装 /usr/lib/systemd/user/，由用户自己 enable。
install -m 0644 "$src/qxyncd.service"  "$stage/usr/lib/systemd/user/qxyncd.service"
install -m 0644 "$src/qxyncd@.service" "$stage/usr/lib/systemd/user/qxyncd@.service"

# 文档：README / 免责声明来自 tar.gz（与下载版逐字一致），
# rest 来自 packaging/debian/（维护者写的）。
doc_dst="$stage/usr/share/doc/qxync"
install -m 0644 "$src/README.md"     "$doc_dst/README.md"
install -m 0644 "$src/README.en.md"  "$doc_dst/README.en.md"
install -m 0644 "$src/DISCLAIMER.md" "$doc_dst/DISCLAIMER.md"
install -m 0644 "$root/packaging/debian/README.Debian" "$doc_dst/README.Debian"
install -m 0644 "$root/packaging/debian/copyright"     "$doc_dst/copyright"

# Debian changelog：一条 = 本次打包。日期用现在的时间（构建可复现不在本包目标内；
# 需要可复现时用 SOURCE_DATE_EPOCH 控制 date）。
if [ -n "${SOURCE_DATE_EPOCH:-}" ]; then
  build_date="$(LC_ALL=C date -R -d "@${SOURCE_DATE_EPOCH}")"
else
  build_date="$(LC_ALL=C date -R)"
fi
{
  printf '%s (%s) %s; urgency=medium\n' qxync "$debversion" "$deb_dist"
  printf '\n'
  printf '  * Prebuilt binaries from GitHub Release v%s (x86_64-unknown-linux-gnu).\n' "$version"
  printf '\n'
  printf ' -- Kami <mlzxgzy@gmail.com>  %s\n' "$build_date"
} > "$doc_dst/changelog.Debian"
gzip -9n "$doc_dst/changelog.Debian"

# ---- 5. Depends：打包基线里实测的三个二进制 DT_NEEDED + 手工 fuse3 ---------
echo "==> dpkg-shlibdeps（在 $cur_id $cur_ver 里实测 Depends）"
shlib_log="$work/dpkg-shlibdeps.log"
# dpkg-shlibdeps 必须在一个「有 debian/control 的源码包目录」里跑（它要靠 control 决定
# 依赖记到哪个包上），所以给它一个一次性的最小 control，别在 repo 里建 debian/。
sd_dir="$work/shlibdeps"
mkdir -p "$sd_dir/debian"
cat > "$sd_dir/debian/control" <<'EOF'
Source: qxync
Maintainer: Kami <mlzxgzy@gmail.com>
Section: net
Priority: optional

Package: qxync
Architecture: any
Description: qxync
EOF
if ! shlib_out="$(cd "$sd_dir" && dpkg-shlibdeps -O \
      "$bin_dst/qxync" "$bin_dst/qxyncd" "$bin_dst/qxync-gui" 2>"$shlib_log")"; then
  echo "--- dpkg-shlibdeps 输出 ---" >&2
  cat "$shlib_log" >&2 || true
  die "dpkg-shlibdeps 失败（完整输出见上）"
fi
# 输出形如：shlibs:Depends=libc6 (>= 2.34), libcairo2 (>= 1.10.0), ...
# 多个二进制可能产出多行 —— 合并去重，保持首次出现的顺序。
shlibs_dep="$(printf '%s\n' "$shlib_out" | sed -n 's/^shlibs:Depends=//p' \
              | tr ',' '\n' | sed 's/^[[:space:]]*//; s/[[:space:]]*$//' \
              | grep -v '^$' | awk '!seen[$0]++' | paste -sd ',' - | sed 's/,/, /g')"
[ -n "$shlibs_dep" ] || die "dpkg-shlibdeps 没算出来任何依赖（shlibs 数据不全？）"
# fusermount3 来自 fuse3 包；qxync 走命令行而不是链接 libfuse3，dpkg-shlibdeps 看不见它。
depends="$shlibs_dep, fuse3"
echo "    Depends: $depends"
if grep -q 'warning:' "$shlib_log" 2>/dev/null; then
  echo "    （dpkg-shlibdeps 有 warning，按设计忽略：$(grep -c 'warning:' "$shlib_log") 条）"
fi

# ---- 6. control / md5sums / Installed-Size ------------------------------
# Installed-Size 是 KiB，与 dh_gencontrol 的口径一致（du -k -s，含目录块的近似）。
installed_size="$(du -sk --apparent-size --exclude=DEBIAN "$stage" | cut -f1)"
sed -e "s/@VERSION@/${debversion}/g" \
    -e "s/@ARCH@/${deb_arch}/g" \
    -e "s/@INSTALLED_SIZE@/${installed_size}/g" \
    -e "s/@DEPENDS@/${depends}/g" \
    "$root/packaging/debian/control.in" > "$stage/DEBIAN/control"
chmod 0644 "$stage/DEBIAN/control"
# control 里不该留下没被替换的占位符
if grep -q '@[A-Z_]*@' "$stage/DEBIAN/control"; then
  die "control 里还有未替换的占位符：$(grep -o '@[A-Z_]*@' "$stage/DEBIAN/control" | sort -u | tr '\n' ' ')"
fi

# md5sums：相对路径、不带 ./，按字节序排序（dpkg-deb 只把它当数据文件，不做校验逻辑）
( cd "$stage" && find . -type f ! -path './DEBIAN/*' -printf '%P\n' \
    | LC_ALL=C sort | while IFS= read -r f; do md5sum "$f"; done ) > "$stage/DEBIAN/md5sums"
chmod 0644 "$stage/DEBIAN/md5sums"

# ---- 7. 打 deb ----------------------------------------------------------
# -Zxz：与 Arch 包（zstd）不同，xz 是 Debian 的默认压缩，老 apt 也都认。
echo "==> dpkg-deb --build"
tmp_deb="${deb_file}.tmp.$$"
dpkg-deb --root-owner-group -Zxz --build "$stage" "$tmp_deb" >/dev/null
mv -f "$tmp_deb" "$deb_file"

# 自检：版本 / 架构 / 包内容的关键文件必须都在
dpkg-deb -f "$deb_file" Version | grep -qx "$debversion" \
  || die "自检失败：deb 里的 Version 不是 $debversion"
for path in \
  ./usr/bin/qxync ./usr/bin/qxyncd ./usr/bin/qxync-gui \
  ./usr/share/applications/qxync.desktop \
  ./usr/share/icons/hicolor/{32x32,128x128,256x256,512x512}/apps/qxync.png \
  ./usr/lib/systemd/user/qxyncd.service ./usr/lib/systemd/user/qxyncd@.service \
  ./usr/share/doc/qxync/{README.md,README.en.md,DISCLAIMER.md,README.Debian,copyright,changelog.Debian.gz} ; do
  dpkg-deb -c "$deb_file" | awk '{print $6}' | grep -qx "$path" \
    || die "自检失败：deb 里缺少 $path"
done

# 容器里以 root 打包时，把产物交回宿主用户（dist/ 是 bind mount 进来的宿主目录）
if [ -n "${HOST_UID:-}" ] && [ -n "${HOST_GID:-}" ] && [ "$(id -u)" -eq 0 ]; then
  chown "$HOST_UID:$HOST_GID" "$deb_file" || true
  # 若 out-dir 是本次在容器里新建的，它的属主也是 root，一并交回宿主
  if [ "$created_out_dir" -eq 1 ]; then
    chown "$HOST_UID:$HOST_GID" "$out_dir" || true
  fi
fi

echo
echo "✅ 打包完成：$deb_file"
echo "   版本：$debversion    架构：$deb_arch    打包基线：$cur_id $cur_ver（changelog 发行版：$deb_dist）"
echo "   大小：$(du -h "$deb_file" | cut -f1)    安装后：$((installed_size / 1024)) MiB（Installed-Size: ${installed_size} KiB）"
echo "   sha256: $(sha256sum "$deb_file" | cut -d' ' -f1)"
echo "   Depends: $depends"
echo
echo "   装它：apt-get install -y ./$(basename "$deb_file")"
