#!/usr/bin/env bash
# 打 Dolphin 角标插件的 .deb —— **两个包**：kf5 与 kf6（ABI 绑 KIO 大版本）。
#
# 用法（必须在容器里跑，见下面的基线要求）：
#   xtask/release/package-overlay-deb.sh <version> [out-dir]
#
#   <version>    不带 v 前缀，例如 0.7.4（也可用环境变量 VERSION 传）
#   [out-dir]    默认 <repo>/dist
#
# 输出：
#   <out-dir>/qxync-dolphin-overlay-kf5_<version>-1_amd64.deb
#   <out-dir>/qxync-dolphin-overlay-kf6_<version>-1_amd64.deb
#
# 为什么是两个包而不是一个：插件的 .so 直接链 libKF5KIOCore / libKF6KIOCore，
# ABI 绑 KIO 大版本。Debian / Ubuntu 矩阵横跨两个世代：
#   * KF5 世代：debian:12（dolphin 22.12）、ubuntu:22.04（21.12）、ubuntu:24.04（23.08）
#     —— Dolphin 从 <Qt 插件目录>/kf5/overlayicon 加载（21.12 硬编码，22.12/23.08
#       用 QT_VERSION_MAJOR 拼出来，都是 kf5）；
#   * KF6 世代：debian:13（dolphin 25.04）、ubuntu:26.04（25.12）—— kf6/overlayicon。
# KF 系列内向后兼容（kf5 包在 5.92 基线编，覆盖 5.103/5.115；kf6 包在 6.13
# 基线编，覆盖 6.24），所以每个世代一个包就够。
#
# 为什么基线是 ubuntu:22.04（kf5）与 debian:13（kf6）：
#   * kf5：矩阵里最老的 KF5（5.92）—— 在更新的 KF5 上编会把 soname 依赖抬高；
#   * kf6：矩阵里最老的 KF6（6.13）—— debian:13 的 KIO 6.13；
#   * 与主 deb 的「按最老目标发行版算依赖」同一原则（见 package-deb.sh 头注释）。
#
# 源码：GitHub tag 归档（https://github.com/<repo>/archive/refs/tags/v<version>.tar.gz）
# —— 与 AUR 源码包（packaging/arch-dolphin-overlay/PKGBUILD）**同一份源**，
# CI 里由 workflow 下载并传进来（离线、可复现），本地跑则自己下载。
#
# 设计要点：
#   * 不重新发明打包：dpkg-deb --build 一个最小 stage（.so + 3 个 SVG + 文档 +
#     changelog），Depends 用 dpkg-shlibdeps 对 .so 实测（基线里算下限）；
#   * 插件目录按世代走：kf5 → usr/lib/qt5/plugins/kf5/overlayicon，
#     kf6 → usr/lib/qt6/plugins/kf6/overlayicon（Dolphin 源码的加载路径）；
#   * 图标两包都装（hicolor 是共享主题，装两份无害；两个包同时装也只占一份磁盘）；
#   * 幂等：重复跑覆盖同名 deb；HOST_UID/HOST_GID 交还宿主属主。
set -euo pipefail

die() { echo "❌ $*" >&2; exit 1; }

version="${1:-${VERSION:-}}"
[ -n "$version" ] || die "用法：$0 <version> [out-dir]"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
out_dir="${2:-$root/dist}"

case "$version" in
  v*) die "version 不要带 v 前缀：$version（tag 才带 v）" ;;
esac
case "$version" in
  *[!0-9A-Za-z.+~:]*) die "version 含非法字符：$version" ;;
esac

# ---- 1. 环境检查：Debian 系 + 构建依赖 -----------------------------------
os_field() {
  [ -r /etc/os-release ] || return 1
  sed -n "s/^$1=//p" /etc/os-release | head -n1 | tr -d '"'
}
cur_id="$(os_field ID || true)"
cur_like="$(os_field ID_LIKE || true)"
is_debian_family=0
case "$cur_id" in debian|ubuntu) is_debian_family=1 ;; esac
case " $cur_like " in *" debian "*) is_debian_family=1 ;; esac
[ "$is_debian_family" -eq 1 ] || die "必须在 Debian/Ubuntu 容器里跑（当前 ID=${cur_id:-?}）"

command -v dpkg >/dev/null || die "找不到 dpkg"
deb_arch="$(dpkg --print-architecture)"
[ "$deb_arch" = amd64 ] || die "本包只提供 amd64：当前是 $deb_arch"

# ---- 2. 源码归档 ----------------------------------------------------------
# CI 里 workflow 已下载到 dist/.overlay-src/（离线复用）；本地跑则现下载。
srcdir="$root/dist/.overlay-src"
src="$srcdir/qxync-dolphin-overlay-src-$version.tar.gz"
if [ ! -f "$src" ]; then
  mkdir -p "$srcdir"
  repo_slug="$(git -C "$root" remote get-url origin 2>/dev/null \
    | sed -E 's#.*github\.com[:/]##; s#\.git$##' || true)"
  [ -n "$repo_slug" ] || repo_slug="mlzxgzy/qxync"
  echo "==> 下载源码归档：${repo_slug} v$version"
  # 基础容器里没有 curl；先装（ca-certificates 同理，最小镜像不带）
  command -v curl >/dev/null || {
    apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends curl ca-certificates
  }
  curl -fsSL -o "$src" "https://github.com/${repo_slug}/archive/refs/tags/v${version}.tar.gz"
fi
sum="$(sha256sum "$src" | cut -d' ' -f1)"
echo "==> 源码归档：$src（sha256 ${sum:0:12}…）"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
tar -xzf "$src" -C "$work"
src_tree="$work/qxync-$version"
[ -d "$src_tree/contrib/dolphin/overlay" ] || die "归档里没有 contrib/dolphin/overlay"

mkdir -p "$out_dir"
out_dir="$(cd "$out_dir" && pwd)"
debversion="${version}-1"

# ---- 3. 每个世代：编译 + 打包 ----------------------------------------------
# 世代 → (KF 大版本, 基线代号, 构建依赖)
build_overlay_deb() {
  local kf="$1" baseline="$2" build_deps="$3"
  local pkg="qxync-dolphin-overlay-kf${kf}"
  local deb_file="${out_dir}/${pkg}_${debversion}_${deb_arch}.deb"

  echo
  echo "════════ ${pkg}（基线 ${baseline}）════════"

  echo "==> 安装构建依赖：${build_deps}"
  apt-get update -qq
  # shellcheck disable=SC2086
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
    dpkg-dev $build_deps

  echo "==> cmake（-DQXYNC_KF_MAJOR=${kf}）"
  cmake -S "$src_tree/contrib/dolphin/overlay" -B "$work/build-kf${kf}" \
    -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr \
    -DQXYNC_KF_MAJOR="$kf"
  cmake --build "$work/build-kf${kf}" -j"$(nproc)"

  # stage：DESTDIR 安装 + 文档 + DEBIAN/
  local stage="$work/root-kf${kf}"
  DESTDIR="$stage" cmake --install "$work/build-kf${kf}"
  install -d -m 0755 "$stage/usr/share/doc/$pkg" "$stage/DEBIAN"
  install -m 0644 "$src_tree/LICENSE-MIT" "$stage/usr/share/doc/$pkg/LICENSE-MIT"
  install -m 0644 "$src_tree/LICENSE-APACHE" "$stage/usr/share/doc/$pkg/LICENSE-APACHE"

  local build_date
  if [ -n "${SOURCE_DATE_EPOCH:-}" ]; then
    build_date="$(LC_ALL=C date -R -d "@${SOURCE_DATE_EPOCH}")"
  else
    build_date="$(LC_ALL=C date -R)"
  fi
  {
    printf '%s (%s) %s; urgency=medium\n' "$pkg" "$debversion" "$baseline"
    printf '\n'
    printf '  * Dolphin overlay icon plugin (KOverlayIconPlugin), built against KF%s\n' "$kf"
    printf '    from the v%s source tarball.\n' "$version"
    printf '\n'
    printf ' -- Kami <mlzxgzy@gmail.com>  %s\n' "$build_date"
  } > "$stage/usr/share/doc/$pkg/changelog.Debian"
  gzip -9n "$stage/usr/share/doc/$pkg/changelog.Debian"

  # Depends：dpkg-shlibdeps 对 .so 实测（在基线里算 → 下限正确）
  local so
  so="$(find "$stage" -name 'libqxync_overlay.so' -print -quit)"
  [ -n "$so" ] || die "kf${kf}：安装树里没有 libqxync_overlay.so"
  local sd_dir="$work/shlibdeps-kf${kf}"
  mkdir -p "$sd_dir/debian"
  cat > "$sd_dir/debian/control" <<EOF
Source: $pkg
Maintainer: Kami <mlzxgzy@gmail.com>
Section: net
Priority: optional

Package: $pkg
Architecture: any
Description: qxync Dolphin overlay icon plugin (KF${kf})
EOF
  local shlib_out
  shlib_out="$(cd "$sd_dir" && dpkg-shlibdeps -O "$so" 2>"$work/shlib-kf${kf}.log")" \
    || { cat "$work/shlib-kf${kf}.log" >&2; die "kf${kf}：dpkg-shlibdeps 失败"; }
  local depends
  depends="$(printf '%s\n' "$shlib_out" | sed -n 's/^shlibs:Depends=//p' \
              | tr ',' '\n' | sed 's/^[[:space:]]*//; s/[[:space:]]*$//' \
              | grep -v '^$' | awk '!seen[$0]++' | paste -sd ',' - | sed 's/,/, /g')"
  [ -n "$depends" ] || die "kf${kf}：dpkg-shlibdeps 没算出依赖"
  # 插件由 Dolphin 在运行时 dlopen，dpkg-shlibdeps 看不见这层关系；
  # 把宿主包写进 Recommends（装了就最好有，没装插件也没害处）。
  echo "    Depends: $depends"

  local installed_size
  installed_size="$(du -sk --apparent-size --exclude=DEBIAN "$stage" | cut -f1)"

  cat > "$stage/DEBIAN/control" <<EOF
Package: $pkg
Version: ${debversion}
Architecture: ${deb_arch}
Maintainer: Kami <mlzxgzy@gmail.com>
Installed-Size: ${installed_size}
Depends: ${depends}
Recommends: qxync
Section: net
Priority: optional
Homepage: https://github.com/mlzxgzy/qxync
Description: Dolphin overlay icon plugin for qxync (KF${kf} build)
 Shows the qxync hydration state as overlay icons on file icons in
 Dolphin (green check = hydrated, cloud = online-only, half-filled
 circle = partially local). This is the KF${kf} build of the plugin;
 the .so links against libKF${kf}KIOCore, so it only loads on
 Dolphin/KF${kf} systems.
 .
 Requires the qxync main package to provide the user.qxync.overlay
 extended attribute (GUI: Settings -> Advanced -> overlay icons).
 Restart Dolphin after installing (plugins are only loaded when a
 view is created).
EOF
  chmod 0644 "$stage/DEBIAN/control"

  ( cd "$stage" && find . -type f ! -path './DEBIAN/*' -printf '%P\n' \
      | LC_ALL=C sort | while IFS= read -r f; do md5sum "$f"; done ) \
    > "$stage/DEBIAN/md5sums"
  chmod 0644 "$stage/DEBIAN/md5sums"

  echo "==> dpkg-deb --build"
  local tmp_deb="${deb_file}.tmp.$$"
  dpkg-deb --root-owner-group -Zxz --build "$stage" "$tmp_deb" >/dev/null
  mv -f "$tmp_deb" "$deb_file"

  # 自检：Debian 的 libdir 带 triplet（usr/lib/x86_64-linux-gnu），Arch 是 usr/lib；
  # 用 grep -E 的通配匹配两种布局都认。
  dpkg-deb -f "$deb_file" Version | grep -qx "$debversion" \
    || die "kf${kf}：deb 里 Version 不是 $debversion"
  dpkg-deb -c "$deb_file" | awk '{print $6}' \
    | grep -Eq "^\./usr/lib/[^/]+/qt${kf}/plugins/kf${kf}/overlayicon/libqxync_overlay\.so$" \
    || die "kf${kf}：deb 里没有 qt${kf}/plugins/kf${kf}/overlayicon/libqxync_overlay.so"
  for icon in qxync-hydrated qxync-placeholder qxync-partial; do
    dpkg-deb -c "$deb_file" | awk '{print $6}' \
      | grep -qx "./usr/share/icons/hicolor/scalable/status/${icon}.svg" \
      || die "kf${kf}：deb 里缺 ${icon}.svg"
  done

  if [ -n "${HOST_UID:-}" ] && [ -n "${HOST_GID:-}" ] && [ "$(id -u)" -eq 0 ]; then
    chown "$HOST_UID:$HOST_GID" "$deb_file" || true
  fi
  echo "✅ ${deb_file}（$(du -h "$deb_file" | cut -f1)）"
}

# ---- 4. 入口：一次只打一个世代（由第 4 个参数或 QXYNC_OVERLAY_KF 指定）----
# workflow 里 kf5 包在 ubuntu:22.04 容器里跑、kf6 包在 debian:13 容器里跑
# （各自的基线算 Depends 下限）；本地复现也是各起一个容器。
kf="${QXYNC_OVERLAY_KF:-}"
if [ -z "$kf" ]; then
  # 没指定就按当前系统猜：有 libkf6kio-dev 的打 kf6，否则 kf5
  if apt-cache show libkf6kio-dev >/dev/null 2>&1 \
     && [ -n "$(apt-cache policy libkf6kio-dev 2>/dev/null | sed -n 's/^  Candidate: //p' | head -1)" ]; then
    kf=6
  else
    kf=5
  fi
  echo "==> 未指定世代，按当前系统选了 kf${kf}（可用 QXYNC_OVERLAY_KF=5|6 显式指定）"
fi

case "$kf" in
  5) build_overlay_deb 5 jammy "build-essential cmake qtbase5-dev libkf5kio-dev" ;;
  6) build_overlay_deb 6 trixie "build-essential cmake qt6-base-dev libkf6kio-dev" ;;
  *) die "KF 世代只能是 5 或 6：$kf" ;;
esac

if [ -n "${HOST_UID:-}" ] && [ -n "${HOST_GID:-}" ] && [ "$(id -u)" -eq 0 ]; then
  [ -d "$out_dir" ] && chown "$HOST_UID:$HOST_GID" "$out_dir" 2>/dev/null || true
fi
echo
echo "✅ 完成（kf${kf}）"

