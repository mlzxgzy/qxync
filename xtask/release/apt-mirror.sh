#!/usr/bin/env bash
# 把 Debian / Ubuntu 容器里的 apt 源换成国内镜像（默认清华 TUNA）。
#
# 用法：
#   xtask/release/apt-mirror.sh [镜像URL]
#
#   [镜像URL]     缺省取环境变量 $QXNYC_APT_MIRROR，再缺省 http://mirrors.tuna.tsinghua.edu.cn
#
# 例子：
#   bash xtask/release/apt-mirror.sh                     # 清华（默认）
#   QXNYC_APT_MIRROR=http://mirrors.aliyun.com bash xtask/release/apt-mirror.sh
#   bash xtask/release/apt-mirror.sh https://mirrors.ustc.edu.cn   # 参数优先级最高
#
# 为什么默认用 http：debian:12 / ubuntu:22.04 这类基础镜像里**还没装 ca-certificates**，
# 直接配 https 源会让 apt 报证书错误（而且 apt 自己也下不了 ca-certificates）。镜像站
# 都支持 http，等 apt 能装了想换回 https 再自行改掉即可。
#
# 处理两种 apt 源格式（实测：各发行版之间并不统一，别按版本号想当然）：
#   * 经典格式   /etc/apt/sources.list 与 /etc/apt/sources.list.d/*.list
#                实测：ubuntu:22.04 就是经典格式
#   * deb822 格式 /etc/apt/sources.list.d/*.sources 里的 URIs: 行
#                实测：debian:12（debian.sources）、debian:13、ubuntu:24.04
#                （改的就是 URIs: 行，一行可以有多个 URL）
#
# 幂等：已经指向目标镜像的 URL 不会再被改（第二次跑只会打印「未改动」）。
# 只动 apt 源文件，不碰 sources.list.d 里第三方仓库（它们的 URL 路径不是 /debian 或
# /ubuntu，识别不出来就原样保留）。
set -euo pipefail

mirror="${1:-${QXNYC_APT_MIRROR:-http://mirrors.tuna.tsinghua.edu.cn}}"
# 去掉结尾斜杠，方便后面拼 /debian、/ubuntu
while [ "${mirror%/}" != "$mirror" ]; do mirror="${mirror%/}"; done
case "$mirror" in
  http://*|https://*) : ;;
  *)
    echo "❌ 镜像地址必须以 http:// 或 https:// 开头：$mirror" >&2
    exit 2
    ;;
esac

os_release="${QXNYC_OS_RELEASE:-/etc/os-release}"
if [ ! -r "$os_release" ]; then
  echo "❌ 读不到 $os_release —— 这不是 Debian/Ubuntu 系统？" >&2
  exit 1
fi

# 只解析我们需要的三个字段，不 source（sources.list 形式的 shell 代码不可信）。
os_field() {
  sed -n "s/^$1=//p" "$os_release" | head -n1 | tr -d '"'
}
distro_id="$(os_field ID)"
distro_like="$(os_field ID_LIKE)"
distro_ver="$(os_field VERSION_ID)"

# 按发行版决定镜像站上的目录名：tuna 上是 /debian 与 /ubuntu。
case "$distro_id" in
  debian) deb_family=debian ;;
  ubuntu) deb_family=ubuntu ;;
  *)
    case " $distro_like " in
      *" debian "*) deb_family=debian ;;
      *" ubuntu "*) deb_family=ubuntu ;;
      *)
        echo "❌ 识别不出发行版（ID=$distro_id ID_LIKE=$distro_like），不知道该用 /debian 还是 /ubuntu" >&2
        exit 1
        ;;
    esac
    ;;
esac

# 把一个旧 URL 映射成「目标镜像 + 对应目录」。
#   http://deb.debian.org/debian            → $mirror/debian
#   http://deb.debian.org/debian-security   → $mirror/debian-security
#   http://security.ubuntu.com/ubuntu       → $mirror/ubuntu
#   http://ports.ubuntu.com/ubuntu-ports    → $mirror/ubuntu-ports
# 认不出路径的（第三方仓库）→ 空字符串 = 不改。
rewrite_url() {
  local url="$1" rest path seg
  rest="${url#*://}"                 # 去掉 scheme → host/path...
  if [ "$rest" = "${rest#*/}" ]; then
    path=""                          # 只有 host，没有路径
  else
    path="/${rest#*/}"
  fi
  seg="${path#/}"; seg="${seg%%/*}"  # 第一层路径
  case "$seg" in
    debian|debian-security|ubuntu|ubuntu-ports|ubuntu-security)
      printf '%s/%s' "$mirror" "$seg"
      ;;
    *)
      # 没有可识别的发行版路径：退回本机发行版的默认目录；
      # 但第三方仓库（路径是自己的名字）不该被改，返回空表示「不动」。
      # 判断依据：旧 host 里带 debian/ubuntu 字样，或者根本没路径。
      case "$url" in
        *debian*|*ubuntu*) printf '%s/%s' "$mirror" "$deb_family" ;;
        *) return 1 ;;
      esac
      ;;
  esac
}

changed_files=()
unchanged_files=()

# 经典格式：一行 = 类型 + 选项 + URL + 套件 + 组件。只替换 URL 那个 token，
# 其余 token 原样保留（用单空格重新拼接；sources.list 里空白不承载语义）。
rewrite_classic_file() {
  local file="$1" line main comment out tok rewritten newline
  newline=""
  out="$(mktemp)"
  while IFS= read -r line || [ -n "$line" ]; do
    # 整行注释 / 空行 / 非 deb 行：原样输出
    if [[ "$line" =~ ^[[:space:]]*# ]] || [[ "$line" =~ ^[[:space:]]*$ ]] \
       || [[ ! "$line" =~ ^[[:space:]]*(deb|deb-src)([[:space:]]|$) ]]; then
      printf '%s\n' "$line" >> "$out"
      continue
    fi
    # 把行尾注释摘出来（URL 里不会有 #），只重写正文
    main="$line"
    comment=""
    if [[ "$line" == *"#"* ]]; then
      main="${line%%#*}"
      comment=" #${line#*#}"
    fi
    local rebuilt=""
    read -r -a toks <<< "$main"
    for tok in "${toks[@]}"; do
      if [[ "$tok" =~ ^https?:// ]]; then
        if rewritten="$(rewrite_url "$tok")"; then
          tok="$rewritten"
        fi
      fi
      rebuilt="${rebuilt:+$rebuilt }$tok"
    done
    printf '%s%s\n' "$rebuilt" "$comment" >> "$out"
  done < "$file"
  if cmp -s "$file" "$out"; then
    rm -f "$out"
    unchanged_files+=("$file")
    return 0
  fi
  cat "$out" > "$file"
  rm -f "$out"
  changed_files+=("$file")
}

# deb822 格式：只改 URIs: 行，一行可以有多个 URL（空格/制表符分隔）。
rewrite_deb822_file() {
  local file="$1" line out tok rebuilt url
  out="$(mktemp)"
  while IFS= read -r line || [ -n "$line" ]; do
    if [[ "$line" =~ ^URIs:[[:space:]] ]]; then
      local urls="${line#URIs:}"
      rebuilt=""
      read -r -a toks <<< "$urls"
      for tok in "${toks[@]}"; do
        url="$tok"
        if [[ "$url" =~ ^https?:// ]]; then
          if new_url="$(rewrite_url "$url")"; then url="$new_url"; fi
        fi
        rebuilt="${rebuilt:+$rebuilt }$url"
      done
      printf 'URIs: %s\n' "$rebuilt" >> "$out"
    else
      printf '%s\n' "$line" >> "$out"
    fi
  done < "$file"
  if cmp -s "$file" "$out"; then
    rm -f "$out"
    unchanged_files+=("$file")
    return 0
  fi
  cat "$out" > "$file"
  rm -f "$out"
  changed_files+=("$file")
}

found=0
# 1) 经典 sources.list
if [ -f /etc/apt/sources.list ]; then
  found=$((found + 1))
  rewrite_classic_file /etc/apt/sources.list
fi
# 2) sources.list.d 下的两种格式（某些镜像/第三方层会混放）
shopt -s nullglob
for f in /etc/apt/sources.list.d/*.list; do
  found=$((found + 1))
  rewrite_classic_file "$f"
done
for f in /etc/apt/sources.list.d/*.sources; do
  found=$((found + 1))
  rewrite_deb822_file "$f"
done

echo "==> apt 镜像：$mirror（发行版 $distro_id ${distro_ver:-?} → /$deb_family）"
if [ "$found" -eq 0 ]; then
  echo "⚠️  没找到任何 apt 源文件（/etc/apt/sources.list 或 /etc/apt/sources.list.d/*.sources）——什么都没改" >&2
  exit 0
fi
if [ "${#changed_files[@]}" -gt 0 ]; then
  for f in "${changed_files[@]}"; do
    echo "    改：$f"
  done
else
  echo "    已经是目标源，未改动（幂等）："
  for f in "${unchanged_files[@]}"; do
    echo "    跳过：$f"
  done
fi
