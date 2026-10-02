#!/usr/bin/env bash
# m83-matrix.sh —— M8.3（同步日志 journal + 文件更新中心 + 错误列表）验收矩阵
#
# 用法:
#   xtask/tests/m83-matrix.sh              # 全量（需要真机凭据；有 /dev/fuse 时多跑「无挂载不写日志」的对照）
#   xtask/tests/m83-matrix.sh --no-fuse    # 跳过挂载相关项
#
# 验的是什么（对应 docs/M8-向Qsync-Client-6靠拢.md 的 M8.3 验收）:
#   ① schema v1 → v3：**老库打开后自动补 journal / decisions 表，已有数据一行不动**；
#   ② 挂载后跑一轮对账 → journal 里出现记录；**没挂载就不写**（没有对账，凭据见 README）；
#   ③ `--level / --query / --limit` 三种过滤正确；
#   ④ `--clear` **只清日志**：游标 / baseline / pin / 上传队列不受影响；
#   ⑤ 轮转：`QSYNC_JOURNAL_MAX_ROWS` 生效，日志不会无限增长。
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DAEMON="$REPO/target/debug/qxyncd"
QS="$REPO/target/debug/qsync"
RUNDIR="${QSYNC_TEST_RUNDIR:-$REPO/.local-run}/m83"
SOCK="$RUNDIR/s.sock"
LOG="$RUNDIR/daemon.log"
MNT="$RUNDIR/mnt"

NO_FUSE=0
for a in "$@"; do case "$a" in --no-fuse) NO_FUSE=1 ;; *) echo "未知参数: $a"; exit 2 ;; esac; done

PASS=0; FAIL=0
ok()   { echo "  ✅ $1"; PASS=$((PASS+1)); }
bad()  { echo "  ❌ $1"; FAIL=$((FAIL+1)); }
check(){ if [ "$1" = "0" ]; then ok "$2"; else bad "$2"; fi; }
skip() { echo "  ⏭️  $1"; }

# 上一次跑崩可能留下挂载：先卸干净（否则 rm -rf 会报「是一个目录」，后面挂载也会撞上）
for d in "$MNT" "$RUNDIR"/*/; do
  [ -d "$d" ] || continue
  if awk -v mp="${d%/}" '$2==mp{found=1} END{exit !found}' /proc/mounts; then
    echo "  ⚠️  清理残留挂载: ${d%/}"
    fusermount3 -u "${d%/}" 2>/dev/null || true
  fi
done
rm -rf "$RUNDIR"; mkdir -p "$RUNDIR/config/qsync/links" "$MNT"
SRC_XDG="${QSYNC_SRC_XDG_CONFIG:-$REPO/.local-run/config}"
LINK="${QSYNC_TEST_LINK:-default}"
if [ ! -f "$SRC_XDG/qsync/links/$LINK.json" ]; then
  echo "❌ 找不到连接配置：$SRC_XDG/qsync/links/$LINK.json"; exit 2
fi
cp "$SRC_XDG/qsync/links/$LINK.json" "$RUNDIR/config/qsync/links/"
[ -f "$SRC_XDG/qsync/credentials.json" ] && cp "$SRC_XDG/qsync/credentials.json" "$RUNDIR/config/qsync/"

export XDG_CONFIG_HOME="$RUNDIR/config" XDG_DATA_HOME="$RUNDIR/data" XDG_STATE_HOME="$RUNDIR/state"
export QSYNC_SOCKET="$SOCK" RUST_LOG="${RUST_LOG:-warn}"
DAEMON_PID=""
[ "$NO_FUSE" = "0" ] && [ ! -e /dev/fuse ] && { echo "  ⚠️  没有 /dev/fuse → 跳过挂载项"; NO_FUSE=1; }

Q() { "$QS" --socket "$SOCK" "$@"; }
jqr() { jq -r "$1" 2>/dev/null; }
start_daemon() {
  pkill -x qxyncd 2>/dev/null; sleep 0.4; rm -f "$SOCK"
  # shellcheck disable=SC2086
  "$DAEMON" --link "$LINK" --socket "$SOCK" --foreground $1 >>"$LOG" 2>&1 &
  DAEMON_PID=$!
  for _ in $(seq 1 60); do [ -S "$SOCK" ] && return 0; sleep 0.25; done
  return 1
}
stop_daemon(){ [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null; pkill -x qxyncd 2>/dev/null; sleep 0.6; rm -f "$SOCK"; DAEMON_PID=""; }
trap 'stop_daemon; fusermount3 -u "$MNT" 2>/dev/null' EXIT

if [ ! -x "$DAEMON" ] || [ ! -x "$QS" ]; then echo "❌ 缺少二进制，先 cargo build --workspace"; exit 1; fi

# ---------------------------------------------------------------- 0. schema v1 → v3
echo "== 0. schema v1 → v3（老库自动补表，数据不动） =="
# 状态库路径由 **link 里的 host** 决定（和 m5-matrix.sh 一样从配置推导，
# 不要把任何具体 NAS 地址写进脚本 —— 这既让脚本对任意用户可用，也避免泄漏）。
DB_HOST="$(jq -r .host "$RUNDIR/config/qsync/links/$LINK.json")"
[ -n "$DB_HOST" ] && [ "$DB_HOST" != "null" ] || { echo "❌ link 里读不到 host"; exit 2; }
DB="$XDG_DATA_HOME/qsync/sync/$DB_HOST/sync.db"
mkdir -p "$(dirname "$DB")"
python3 - "$DB" <<'PY'
import sqlite3, sys
db = sys.argv[1]
c = sqlite3.connect(db)
c.execute("CREATE TABLE IF NOT EXISTS pins (path TEXT PRIMARY KEY, state TEXT NOT NULL)")
c.execute("INSERT OR REPLACE INTO pins VALUES ('/home/qxync-test/hello.txt','pinned')")
c.execute("PRAGMA user_version=1")
c.commit(); c.close()
print("已造一个 v1 老库（含 1 条 pin）")
PY
check "$([ "$(python3 -c "import sqlite3,sys;print(sqlite3.connect(sys.argv[1]).execute('PRAGMA user_version').fetchone()[0])" "$DB")" = "1" ] && echo 0 || echo 1)" "造出来的老库确实是 user_version=1"

start_daemon ""; check $? "qxyncd 起来"
Q --via-daemon login >/dev/null 2>&1
VER=$(python3 -c "import sqlite3,sys;print(sqlite3.connect(sys.argv[1]).execute('PRAGMA user_version').fetchone()[0])" "$DB")
check "$([ "$VER" = "3" ] && echo 0 || echo 1)" "打开后 schema 升到 v3（实际 $VER）"
HAS=$(python3 -c "
import sqlite3,sys
c=sqlite3.connect(sys.argv[1])
print(1 if c.execute(\"SELECT name FROM sqlite_master WHERE type='table' AND name='journal'\").fetchone() else 0)
" "$DB")
check "$([ "$HAS" = "1" ] && echo 0 || echo 1)" "journal 表已自动创建"
PIN=$(Q store 2>/dev/null | grep -c 'hello.txt' || true)
check "$([ "${PIN:-0}" -ge 1 ] && echo 0 || echo 1)" "★ v1 老库里的 pin 原样保留（没被迁移破坏）"

# ---------------------------------------------------------------- 1. 空日志
echo "== 1. 初始日志为空 =="
J0=$(Q journal --json 2>/dev/null)
check "$([ "$(echo "$J0" | jqr .total)" = "0" ] && echo 0 || echo 1)" "初始 total=0"
check "$([ "$(echo "$J0" | jqr .limit_rows)" = "10000" ] && echo 0 || echo 1)" "默认上限 10000 条"
check "$([ "$(echo "$J0" | jqr .max_age_days)" = "30" ] && echo 0 || echo 1)" "默认保留 30 天"

# ---------------------------------------------------------------- 2. 没挂载 → 不写日志
echo "== 2. 没有挂载点 → 对账无事可做 → 不写日志 =="
Q sync --once >/dev/null 2>&1
sleep 1.2
N_NOMOUNT=$(Q journal --json 2>/dev/null | jqr .total)
check "$([ "${N_NOMOUNT:-x}" = "0" ] && echo 0 || echo 1)" "★ 没挂载时跑 sync 不产生日志（实际 $N_NOMOUNT 条）"

# ---------------------------------------------------------------- 3. 挂载后产生日志
echo "== 3. 挂载后跑一轮 → 有记录 =="
if [ "$NO_FUSE" = "1" ]; then
  skip "挂载相关项（--no-fuse）"
else
  Q mount "$MNT" --remote /home >/dev/null 2>&1; check $? "挂载成功"
  ls "$MNT" >/dev/null 2>&1
  Q sync --once >/dev/null 2>&1
  sleep 1.2
  J1=$(Q journal --json 2>/dev/null)
  N1=$(echo "$J1" | jqr .total)
  check "$([ "${N1:-0}" -ge 1 ] && echo 0 || echo 1)" "★ 挂载后产生日志（$N1 条）"
  K=$(echo "$J1" | jqr '.entries[0].kind')
  check "$([ -n "$K" ] && [ "$K" != "null" ] && echo 0 || echo 1)" "条目有 kind（$K）"
fi

# ---------------------------------------------------------------- 4. 过滤
echo "== 4. level / query / limit 过滤 =="
# 手工灌几条确定性的记录，避免依赖真实同步事件
sqlite3 "$DB" "INSERT INTO journal (ts, task_id, kind, path, detail, status, bytes) VALUES
 (strftime('%s','now'),'t','upload','/home/qxync-test/a.iso','上传完成 a.iso','ok',1024),
 (strftime('%s','now'),'t','upload','/home/qxync-test/b.iso','上传失败：连接被重置','error',0),
 (strftime('%s','now'),'t','dehydrate','/home/qxync-test/c.bin','被 pin 挡下','blocked',0);" 2>/dev/null
check $? "手工灌入 3 条（ok/error/blocked 各一）"
E=$(Q journal --level error --json 2>/dev/null)
check "$([ "$(echo "$E" | jqr '[.entries[]|select(.status!="error")]|length')" = "0" ] && echo 0 || echo 1)" "--level error 只返回 error"
check "$([ "$(echo "$E" | jqr '.entries|length')" = "1" ] && echo 0 || echo 1)" "--level error 返回 1 条"
B=$(Q journal --level blocked --json 2>/dev/null)
check "$([ "$(echo "$B" | jqr '.entries[0].kind')" = "dehydrate" ] && echo 0 || echo 1)" "--level blocked 命中脱水那条"
QY=$(Q journal --query b.iso --json 2>/dev/null)
check "$([ "$(echo "$QY" | jqr '.entries|length')" = "1" ] && echo 0 || echo 1)" "--query 按路径子串命中"
QD=$(Q journal --query '连接被重置' --json 2>/dev/null)
check "$([ "$(echo "$QD" | jqr '.entries|length')" = "1" ] && echo 0 || echo 1)" "--query 也匹配说明文本"
LM=$(Q journal --limit 2 --json 2>/dev/null)
check "$([ "$(echo "$LM" | jqr '.entries|length')" = "2" ] && echo 0 || echo 1)" "--limit 生效"

# ---------------------------------------------------------------- 5. clear 只清日志
echo "== 5. --clear 只清日志（同步状态不受影响） =="
CUR_BEFORE=$(Q store --json 2>/dev/null | jqr '.cursors.max_log_seen')
BASE_BEFORE=$(Q store --json 2>/dev/null | jqr '(.baseline|length)')
CL=$(Q journal --clear --json 2>/dev/null)
check "$([ "$(echo "$CL" | jqr .cleared)" = "true" ] && echo 0 || echo 1)" "clear 返回 cleared=true"
check "$([ "$(echo "$CL" | jqr .total)" = "0" ] && echo 0 || echo 1)" "清空后 total=0"
CUR_AFTER=$(Q store --json 2>/dev/null | jqr '.cursors.max_log_seen')
BASE_AFTER=$(Q store --json 2>/dev/null | jqr '(.baseline|length)')
check "$([ "$CUR_BEFORE" = "$CUR_AFTER" ] && echo 0 || echo 1)" "★ 游标没变（$CUR_BEFORE → $CUR_AFTER）"
check "$([ "$BASE_BEFORE" = "$BASE_AFTER" ] && echo 0 || echo 1)" "★ baseline 没变（$BASE_BEFORE → $BASE_AFTER）"
PIN_AFTER=$(Q store 2>/dev/null | grep -c 'hello.txt' || true)
check "$([ "${PIN_AFTER:-0}" -ge 1 ] && echo 0 || echo 1)" "★ pin 没被清掉"

# ---------------------------------------------------------------- 6. 轮转
echo "== 6. 轮转（QSYNC_JOURNAL_MAX_ROWS） =="
stop_daemon
QSYNC_JOURNAL_MAX_ROWS=5 QSYNC_JOURNAL_TRIM_SECS=1 start_daemon ""
check $? "以 QSYNC_JOURNAL_MAX_ROWS=5 / TRIM_SECS=1 起 daemon"
Q --via-daemon login >/dev/null 2>&1
sqlite3 "$DB" "$(python3 - <<'PY'
rows = []
for i in range(40):
    rows.append(f"(strftime('%s','now'),'t','upload','/home/f{i}.txt','第 {i} 条','ok',{i})")
print("INSERT INTO journal (ts, task_id, kind, path, detail, status, bytes) VALUES " + ",".join(rows) + ";")
PY
)" 2>/dev/null
N40=$(Q journal --json 2>/dev/null | jqr .total)
check "$([ "${N40:-0}" -ge 40 ] && echo 0 || echo 1)" "先灌到 $N40 条"
# 轮转由后台 flusher 做（测试里把间隔调成 1s），等它压到上限内
AFTER=999
for _ in $(seq 1 60); do
  AFTER=$(Q journal --json 2>/dev/null | jqr .total)
  [ "${AFTER:-999}" -le 5 ] && break
  sleep 0.5
done
check "$([ "${AFTER:-999}" -le 5 ] && echo 0 || echo 1)" "★ 轮转把日志压到上限内（$AFTER ≤ 5）"

# ---------------------------------------------------------------- 汇总
echo
echo "== 汇总 =="
echo "  通过 $PASS / 失败 $FAIL"
if [ "$FAIL" = "0" ]; then echo "  🎉 M8.3 验收矩阵全过"; else echo "  ❌ 有失败项（日志 $LOG）"; fi
exit $([ "$FAIL" = "0" ] && echo 0 || echo 1)
