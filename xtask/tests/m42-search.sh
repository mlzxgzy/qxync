#!/usr/bin/env bash
# m42-search.sh —— ★ M42「本地文件搜索（qxync find）」的隔离端到端验收
#
# 验的是什么（用户原话：「有目录数据库的话是不是可以快速搜索仓库内的文件啊」）：
#   1. **不挂载、不联网、不启动 daemon**，只读状态库就能按文件名/路径搜到远端文件
#      （数据源 = `baseline` 里 present=1 的行）；
#   2. 本地新建、还没上传的文件（`nodes.local_only`）也能搜到，且标成 local_only；
#   3. 墓碑（present=0）不返回；
#   4. `--local` 把远端路径换算成挂载点里的本地路径（纯词法，不需要挂载点存在）；
#   5. `--json` 字段稳定（path/local_path/size/mtime/mtime_text/is_dir/origin/task_id）；
#   6. `--glob` / `--under`（含本地路径换算）/ `--type` / `--limit` 截断提示；
#   7. 状态库不存在 → 非零退出 + 可读错误；`--via-daemon` 被明确拒绝。
#
# 不需要 NAS、不需要真挂载、不需要真实用户数据：所有 XDG 目录都指向 .local-run 下，
# 状态库由 python3 按《当前 schema 的最小列集》直接造（不依赖 sqlite3 CLI）。
# 用法：xtask/tests/m42-search.sh
# 依赖：cargo build -p qxync-cli（target/debug/qxync）+ python3
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
QX="$REPO/target/debug/qxync"
ROOT="$REPO/.local-run/m42-search-test"

PASS=0
FAIL=0
pass() { echo "PASS $1"; PASS=$((PASS + 1)); }
fail() { echo "FAIL $1"; FAIL=$((FAIL + 1)); }
check() { if [ "$1" = "0" ]; then pass "$2"; else fail "$2"; fi; }

if [ ! -x "$QX" ]; then
  echo "SKIP：没有 $QX（先 cargo build -p qxync-cli）"
  exit 0
fi

rm -rf "$ROOT"
mkdir -p "$ROOT"
export XDG_CONFIG_HOME="$ROOT/config"
export XDG_DATA_HOME="$ROOT/data"
export XDG_STATE_HOME="$ROOT/state"
export XDG_RUNTIME_DIR="$ROOT/run"
export HOME="$ROOT/home"
unset QXNYC_SOCKET QXNYC_HOST QXNYC_USER QXNYC_PASSWORD QXNYC_OTP_SECRET QXNYC_OTP_CODE 2>/dev/null || true

CFG="$XDG_CONFIG_HOME/qxync"
DATA="$XDG_DATA_HOME/qxync"
STATE="$XDG_STATE_HOME/qxync"
MNT="$ROOT/mnt/qsync"
mkdir -p "$CFG/links" "$CFG/tasks" "$DATA/sync/127.0.0.1" "$STATE/log" "$XDG_RUNTIME_DIR" "$MNT" "$HOME"

cat >"$CFG/links/default.json" <<'EOF'
{"id":"default","host":"127.0.0.1","port":9,"https":false,"insecure":true,"user":"u","ipv4_only":true}
EOF
cat >"$CFG/tasks/qsync.json" <<EOF
{"id":"qsync","link_id":"default","name":"qsync","enabled":true,"mountpoint":"$MNT","root":"/home/.Qsync"}
EOF

DB="$DATA/sync/127.0.0.1/sync.db"
python3 - "$DB" <<'PY'
import sqlite3, sys
db = sys.argv[1]
c = sqlite3.connect(db)
c.executescript("""
CREATE TABLE baseline (
  path    TEXT PRIMARY KEY,
  present INTEGER NOT NULL,
  is_dir  INTEGER NOT NULL,
  size    INTEGER NOT NULL,
  mtime   INTEGER NOT NULL
);
CREATE TABLE nodes (
  file_id     BLOB    NOT NULL,
  path        TEXT    NOT NULL,
  remote_path TEXT    NOT NULL DEFAULT '',
  size        INTEGER NOT NULL DEFAULT 0,
  mtime       INTEGER NOT NULL DEFAULT 0,
  isfolder    INTEGER NOT NULL DEFAULT 0,
  updated_at  INTEGER NOT NULL DEFAULT 0,
  local_only  INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (file_id, path)
);
CREATE UNIQUE INDEX nodes_path ON nodes(path);
""")
rows = [
    ("/home/.Qsync/2026/29-四川赛/试题A.pdf", 1, 0, 12218, 1792368403),
    ("/home/.Qsync/2026/29-四川赛/README.md", 1, 0, 180, 1792368404),
    ("/home/.Qsync/2026", 1, 1, 0, 0),
    # 墓碑：远端已删除的路径（present=0）不该出现在搜索结果里
    ("/home/.Qsync/2026/已删除.txt", 0, 0, 5, 1),
]
c.executemany("INSERT INTO baseline(path,present,is_dir,size,mtime) VALUES(?,?,?,?,?)", rows)
c.execute(
    "INSERT INTO nodes(file_id,path,remote_path,size,mtime,isfolder,updated_at,local_only)"
    " VALUES(?,?,?,?,?,?,?,?)",
    (b"\0" * 16, "/home/.Qsync/2026/草稿.txt", "/home/.Qsync/2026/草稿.txt", 42, 1792368405, 0, 0, 1),
)
c.commit()
c.close()
PY

# ---------------------------------------------------------------- 1. 子串 / 大小写
OUT="$("$QX" find 试题 2>/dev/null)"
RC=$?
check "$RC" "子串搜索退出码 0"
case "$OUT" in *"试题A.pdf"*) pass "子串命中远端文件";; *) fail "子串命中远端文件（实际：$OUT）";; esac
case "$OUT" in *README*) fail "不该命中无关文件";; *) pass "不命中无关文件";; esac

OUT="$("$QX" find readme 2>/dev/null)"
case "$OUT" in *README.md*) pass "默认大小写不敏感";; *) fail "默认大小写不敏感（实际：$OUT）";; esac

# ---------------------------------------------------------------- 2. --local 映射
OUT="$("$QX" find 试题 --local 2>/dev/null)"
case "$OUT" in *"$MNT/2026/29-四川赛/试题A.pdf"*) pass "--local 换算成挂载点路径";; *) fail "--local 换算（实际：$OUT）";; esac

# ---------------------------------------------------------------- 3. --json 字段
OUT="$("$QX" find 试题 --json 2>/dev/null)"
printf '%s' "$OUT" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d["total"] == 1, d
assert d["truncated"] is False, d
h = d["hits"][0]
for k in ("path", "local_path", "size", "mtime", "mtime_text", "is_dir", "origin", "task_id"):
    assert k in h, (k, h)
assert h["origin"] == "remote", h
assert h["task_id"] == "qsync", h
assert h["size"] == 12218, h
' >/dev/null 2>&1
check "$?" "--json 字段稳定且可解析"

# ---------------------------------------------------------------- 4. glob / under / type
OUT="$("$QX" find '*.pdf' --glob --limit 0 2>/dev/null)"
[ "$(printf '%s\n' "$OUT" | grep -c '试题A.pdf')" = "1" ]
check "$?" "--glob 只匹配文件名"

OUT="$("$QX" find --under "$MNT/2026/29-四川赛" --type f --limit 0 2>/dev/null)"
case "$OUT" in *"试题A.pdf"*) pass "--under 接受本地路径并换算";; *) fail "--under 本地路径换算（实际：$OUT）";; esac
case "$OUT" in *"/home/.Qsync/2026"$'\n'*) fail "--type f 不该出现目录";; *) pass "--type f 过滤目录";; esac

OUT="$("$QX" find --type d --limit 0 2>/dev/null)"
[ "$OUT" = "/home/.Qsync/2026" ]
check "$?" "--type d 只返回目录"

# ---------------------------------------------------------------- 5. 截断 / 本地新建 / 墓碑
OUT="$("$QX" find Qsync --limit 1 2>"$ROOT/err")"
[ "$(printf '%s\n' "$OUT" | wc -l)" = "1" ] && grep -q "已显示 1 条" "$ROOT/err"
check "$?" "--limit 截断且提示写 stderr"

OUT="$("$QX" find 草稿 --json 2>/dev/null)"
printf '%s' "$OUT" | python3 -c '
import json, sys
h = json.load(sys.stdin)["hits"][0]
assert h["origin"] == "local_only", h
assert h["size"] == 42, h
' >/dev/null 2>&1
check "$?" "本地新建（local_only）能搜到"

OUT="$("$QX" find 已删除 2>/dev/null)"
[ -z "$OUT" ]
check "$?" "墓碑（present=0）不返回"

# ---------------------------------------------------------------- 6. 离线 / 缺库 / 拒绝 IPC
"$QX" find 试题 --limit 1 >/dev/null 2>&1
check "$?" "daemon 不在、无挂载、无网络也能搜（离线可用）"

"$QX" find 试题 --via-daemon >/dev/null 2>&1
[ $? -ne 0 ]
check "$?" "--via-daemon 被拒绝（纯本地命令）"

EMPTY="$ROOT/empty"
mkdir -p "$EMPTY/config/qxync/links" "$EMPTY/data" "$EMPTY/state" "$EMPTY/run" "$EMPTY/home"
cp "$CFG/links/default.json" "$EMPTY/config/qxync/links/default.json"
ERR="$(XDG_CONFIG_HOME="$EMPTY/config" XDG_DATA_HOME="$EMPTY/data" \
       XDG_STATE_HOME="$EMPTY/state" XDG_RUNTIME_DIR="$EMPTY/run" HOME="$EMPTY/home" \
       "$QX" find x 2>&1 >/dev/null)"
RC=$?
[ "$RC" -ne 0 ] && case "$ERR" in *状态库不存在*) true;; *) false;; esac
check "$?" "状态库不存在 → 非零退出 + 明确错误"

echo "── m42-search: ${PASS} passed, ${FAIL} failed"
[ "$FAIL" = "0" ]
