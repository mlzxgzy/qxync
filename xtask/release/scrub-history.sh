#!/usr/bin/env bash
# 历史清洗：把敏感串从**全部提交**里替换成占位符（发布前自检 / 事故补救）。
#
# 为什么单独做一个脚本：敏感串**不能写进仓库**（写进仓库就等于又泄露一次），
# 所以模式表从命令行或本地未跟踪文件传进来，脚本本身保持通用。
#
# 用法：
#   bash xtask/release/scrub-history.sh --pattern 'my-nas.example.com' --pattern 'myuser'
#   bash xtask/release/scrub-history.sh --patterns-file .scrub-patterns      # 每行 key=value
#   bash xtask/release/scrub-history.sh --pattern ... --apply               # 真跑（默认只演练）
#
# 行为：
#   - 默认 **dry-run**：只统计各模式命中多少次、涉及多少提交，不碰仓库；
#   - --apply：在**当前仓库**上重写历史（git filter-branch），会改所有提交 hash，
#     已发布 tag 必须由你自己决定要不要重打（脚本会打印需要处理的 tag 清单，不自动动 tag）；
#   - 跑完自动 `git log` 校验残留，并提示 `git push --force-with-lease` 与 tag 处理。
#
# 建议：先在克隆里跑
#   git clone --no-local . /tmp/scrub-check && cd /tmp/scrub-check
#   bash <repo>/xtask/release/scrub-history.sh --pattern '...' --apply
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || exit 1

PATTERNS=()
FILE=""
APPLY=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --pattern)      PATTERNS+=("$2"); shift 2 ;;
    --pattern=*)    PATTERNS+=("${1#*=}"); shift ;;
    --patterns-file) FILE="$2"; shift 2 ;;
    --patterns-file=*) FILE="${1#*=}"; shift ;;
    --apply)        APPLY=1; shift ;;
    -h|--help)      sed -n '2,26p' "$0" | sed 's/^#\{1,\} \{0,1\}//'; exit 0 ;;
    *) echo "未知参数：$1" >&2; exit 2 ;;
  esac
done

if [ -n "$FILE" ]; then
  [ -f "$FILE" ] || { echo "❌ 模式文件不存在：$FILE" >&2; exit 2; }
  while IFS='=' read -r k v; do
    [ -z "${k// }" ] && continue
    case "$k" in \#*) continue ;; esac
    PATTERNS+=("$k=$v")
  done < "$FILE"
fi

[ "${#PATTERNS[@]}" -gt 0 ] || { echo "❌ 至少给一个 --pattern 或 --patterns-file" >&2; exit 2; }

# git filter-branch 的 --tree-filter/--msg-filter 需要「旧串=>新串」；只有 key 没 value 的模式
# 表示「替换成占位符」，占位符由调用方给，避免脚本瞎猜。
# 统一成 old=>new 一种形态（--pattern 既接受 "old=>new"，也接受老的 "old=new"）
REPL=""
for p in "${PATTERNS[@]}"; do
  if [[ "$p" == *"=>"* ]]; then
    REPL+="$p"$'\n'
  elif [[ "$p" == *=* ]]; then
    REPL+="${p%%=*}=>${p#*=}"$'\n'
  else
    REPL+="$p=>$(printf '%s' "$p" | tr 'a-z0-9' 'X')"$'\n'
  fi
done

TMPREPL="$(mktemp)"
printf '%s' "$REPL" > "$TMPREPL"
trap 'rm -f "$TMPREPL"' EXIT

echo "== 命中统计（全历史，仅工作树，不改仓库） =="
total=0
while IFS= read -r line; do
  [ -z "$line" ] && continue
  key="${line%%=>*}"
  n_files=$(git -c core.quotepath=false grep -l -F "$key" $(git rev-list --all) -- 2>/dev/null | wc -l)
  echo "  $(printf '%-40s' "$key") 命中对象 $n_files 处"
  total=$((total + n_files))
done < "$TMPREPL"

echo
echo "== 含敏感串的 tag =="
while IFS= read -r line; do
  [ -z "$line" ] && continue
  key="${line%%=>*}"
  for t in $(git tag); do
    git -c core.quotepath=false grep -qF "$key" "$t" -- 2>/dev/null && echo "  $t  ($key)"
  done
done < "$TMPREPL" | sort -u

if [ "$APPLY" = 0 ]; then
  cat <<'EOT'

（演练结束，未改动任何东西。确认上面的清单后加 --apply 真跑。）

注意 --apply 之后：
  1. 所有提交 hash 会变 —— 需要 `git push --force-with-lease origin master`；
  2. 已发布 tag 指向旧 hash，Release 会失联：要么重打 tag 重跑发版，要么接受旧 Release 保留；
  3. GitHub 上旧对象仍可能通过 SHA 直达，彻底清除需联系 GitHub Support。
EOT
  exit 0
fi

command -v git >/dev/null || exit 1
if [ -n "$(git status --porcelain)" ]; then
  echo "❌ 工作区不干净，先 commit/stash 再重写历史" >&2
  exit 1
fi

FILTER="$ROOT/xtask/release/scrub-filter.py"
export SCRUB_PATTERNS_FILE="$TMPREPL"

echo "== 开始重写（filter-branch，可能需要几分钟） =="
FILTER_BRANCH_SQUELCH_WARNING=1 git filter-branch -f --prune-empty \
  --tree-filter "python3 '$FILTER' tree" \
  --msg-filter "python3 '$FILTER' msg" \
  --tag-name-filter cat -- --all

echo "== 残留校验 =="
# 注意：filter-branch 会把旧历史备份在 refs/original/ 下，`--all` 会把它算进来，
# 不排除就会把「已经改干净的新历史」误判成残留。
left=0
while IFS= read -r line; do
  [ -z "$line" ] && continue
  key="${line%%=>*}"
  if git -c core.quotepath=false grep -qF "$key" \
       $(git rev-list --exclude='refs/original/*' --all) -- 2>/dev/null; then
    echo "  ❌ 仍有残留：$key"; left=1
  fi
done < "$TMPREPL"
if [ "$left" = 0 ]; then
  echo "  ✅ 新历史已无命中（旧对象仍留在 refs/original/ 与 reflog，按下面的命令清）"
  echo "  提交数：$(git rev-list --exclude='refs/original/*' --all --count)"
fi

cat <<'EOT'

下一步（手动确认后再执行）：
  git for-each-ref --format='%(refname)' refs/original/ | xargs -n1 git update-ref -d
  git reflog expire --expire=now --all && git gc --prune=now --aggressive
  git push --force-with-lease origin master
  git push --force origin <需要重打的 tag>
EOT
