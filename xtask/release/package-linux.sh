#!/usr/bin/env bash
# 打包 Linux 发版产物：把已构建好的三个二进制（qsync / qxyncd / qxync-gui）连同
# 桌面项、图标、许可证与免责声明打成 tar.gz，并把三个裸二进制复制到输出目录。
#
# 用法：
#   xtask/release/package-linux.sh <version> <target-triple> [bin-dir] [out-dir]
#
#   <version>        不带 v 前缀，例如 0.1.0
#   <target-triple>  例如 x86_64-unknown-linux-gnu（**只用于命名**，不做交叉编译）
#   bin-dir          默认 <repo>/target/release
#   out-dir          默认 <repo>/dist
#
# 产物（out-dir 下）：
#   qxync-<version>-<triple>.tar.gz   解包后是 <stem>/{三个二进制, qxync.desktop,
#                                     icons/, README*, LICENSE-*, DISCLAIMER.md}
#                                     —— 发行版打包（packaging/arch/）直接从这里取
#   qsync / qxyncd / qxync-gui        三个裸二进制（供直接下载）
#
# 可复现性：tar 内 owner/group 归零、顺序按名字排序、mtime 取 HEAD 提交时间
# （或 SOURCE_DATE_EPOCH），gzip 用 `-n` 不写时间戳 —— 同一份输入 → 同一个 sha256。
#
# 本地复跑（发版前自查）：
#   CARGO_HOME=$PWD/.cargo-home cargo build --release --locked \
#     -p qxync-cli -p qxync-daemon -p qxync-gui
#   bash xtask/release/package-linux.sh 0.1.0 x86_64-unknown-linux-gnu
set -euo pipefail

if [ "$#" -lt 2 ]; then
  sed -n '2,22p' "${BASH_SOURCE[0]}" | sed 's/^#\{1,\} \{0,1\}//'
  exit 2
fi

version="$1"
triple="$2"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
bin_dir="${3:-$root/target/release}"
out_dir="${4:-$root/dist}"

case "$version" in
  v*) echo "❌ version 不要带 v 前缀：$version（tag 才带 v）" >&2; exit 1 ;;
  "") echo "❌ version 不能为空" >&2; exit 1 ;;
esac

binaries=(qsync qxyncd qxync-gui)
for b in "${binaries[@]}"; do
  if [ ! -x "$bin_dir/$b" ]; then
    echo "❌ 找不到可执行文件：$bin_dir/$b" >&2
    echo "   先跑：cargo build --release --locked -p qxync-cli -p qxync-daemon -p qxync-gui" >&2
    exit 1
  fi
done

stem="qxync-${version}-${triple}"
tarball="$out_dir/${stem}.tar.gz"

# tar 的 mtime：优先 SOURCE_DATE_EPOCH，其次 HEAD 提交时间，最后回落到当前时间。
mtime_args=()
if [ -n "${SOURCE_DATE_EPOCH:-}" ]; then
  mtime_args=(--mtime="@${SOURCE_DATE_EPOCH}")
elif head_date="$(git -C "$root" log -1 --format=%cI 2>/dev/null)" && [ -n "$head_date" ]; then
  mtime_args=(--mtime="$head_date")
fi

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

dest="$stage/$stem"
mkdir -p "$dest" "$out_dir"

for b in "${binaries[@]}"; do
  install -m 0755 "$bin_dir/$b" "$dest/$b"
done
for f in README.md README.en.md LICENSE-MIT LICENSE-APACHE DISCLAIMER.md; do
  if [ -f "$root/$f" ]; then
    install -m 0644 "$root/$f" "$dest/$f"
  else
    echo "⚠️  跳过缺失的文档：$f" >&2
  fi
done

# 发行版打包要用的桌面项与图标（packaging/arch/ 的 PKGBUILD 直接从这里取，
# 不在打包器里另抄一份，免得图标换了只改一处）。
install -m 0644 "$root/packaging/qxync.desktop" "$dest/qxync.desktop"
mkdir -p "$dest/icons"
for i in 32x32 128x128 128x128@2x icon; do
  install -m 0644 "$root/crates/qxync-gui/icons/$i.png" "$dest/icons/$i.png"
done

# 先写 gzip（-n 不写时间戳），再原子替换，避免留下半个包。
tmp_tar="$(mktemp "$out_dir/.${stem}.XXXXXX.tar.gz")"
tar --sort=name --owner=0 --group=0 --numeric-owner "${mtime_args[@]}" \
    -C "$stage" -cf - "$stem" | gzip -9n > "$tmp_tar"
mv -f "$tmp_tar" "$tarball"
chmod 0644 "$tarball"

for b in "${binaries[@]}"; do
  install -m 0755 "$bin_dir/$b" "$out_dir/$b"
done

echo "✅ 打包完成：$tarball"
echo "   裸二进制：$(printf '%s ' "${binaries[@]}")（在 $out_dir 下）"
echo "   解包内容："
tar -tzf "$tarball" | sed 's/^/     /'
echo "   sha256: $(sha256sum "$tarball" | cut -d' ' -f1)"
