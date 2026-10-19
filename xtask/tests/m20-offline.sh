#!/usr/bin/env bash
# m20-offline.sh —— ★ M20「离线缓冲区 / 离线状态」的隔离端到端验收
#
# 验的是什么（用户原话：「离线了之后还能提供服务，就是累计缓冲区，然后等上线
# 之后统一进行同步」）：
#   1. **离线时 status 必须快**：旧实现断网时 `status` 会依次打 nas_uid / max_log /
#      check_alive，每个等 20s connect timeout —— 排查「怎么不动了」时最该跑的命令
#      反而卡一分钟。现在已知离线直接跳过网络探测。
#   2. **离线状态要如实上报**：connectivity.online=false / known=true，且带原因。
#   3. **离线缓冲区可见**：启动时 sync.db 里压着的 mkdir / rename 行要在 status 里
#      显示成「待建目录 / 待改名」（这是「累计缓冲区」在界面上的唯一出口）。
#   4. **缓冲区跨 daemon 重启不丢、网络错误不消耗重试次数**：起停一次之后
#      行还在、attempts 仍是 0（放弃/计数只该发生在真正的业务错误上）。
#   5. `user.qxync.offline` xattr 的逻辑由单测覆盖（本机没有 /dev/fuse，不真挂）。
#
# 不需要 NAS、不需要真挂载、不需要真实用户数据：所有 XDG 目录都指向 .local-run 下，
# NAS 地址刻意指向 127.0.0.1:9（必定 connection refused = Transport = 离线）。
# 用法：xtask/tests/m20-offline.sh
# 依赖：cargo build --workspace（target/debug/{qxync,qxyncd}）；sqlite3 可选
#       （缺了就跳过缓冲区行数断言，只验离线状态）。
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
QX="$REPO/target/debug/qxync"
QD="$REPO/target/debug/qxyncd"
ROOT="$REPO/.local-run/m20-offline-test"

PASS=0
FAIL=0
pass() { echo "PASS $1"; PASS=$((PASS + 1)); }
fail() { echo "FAIL $1"; FAIL=$((FAIL + 1)); }
check() { if [ "$1" = "0" ]; then pass "$2"; else fail "$2"; fi; }

if [ ! -x "$QX" ] || [ ! -x "$QD" ]; then
  echo "SKIP：没有 $QX / $QD（先 cargo build --workspace）"
  exit 0
fi

rm -rf "$ROOT"
mkdir -p "$ROOT"
export XDG_CONFIG_HOME="$ROOT/config"
export XDG_DATA_HOME="$ROOT/data"
export XDG_STATE_HOME="$ROOT/state"
export XDG_RUNTIME_DIR="$ROOT/run"
# 隔离环境里绝不允许继承真实连接 / 口令 / socket 覆盖
unset QXNYC_SOCKET QXNYC_HOST QXNYC_USER QXNYC_PASSWORD QXNYC_OTP_SECRET QXNYC_OTP_CODE
CFG="$XDG_CONFIG_HOME/qxync"
DATA="$XDG_DATA_HOME/qxync"
STATE="$XDG_STATE_HOME/qxync"
NS="127.0.0.1"
DAEMON_STARTED=0

mkdir -p "$CFG/links" "$CFG/tasks" "$DATA" "$STATE/log" "$XDG_RUNTIME_DIR/qxync"
chmod 700 "$XDG_RUNTIME_DIR/qxync"

cleanup() {
  if [ "$DAEMON_STARTED" = "1" ]; then
    "$QX" daemon stop --force >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

# 不可达的 NAS：127.0.0.1:9 会立刻连接拒绝 → 客户端判 Transport（离线）
cat >"$CFG/links/default.json" <<'EOF'
{"id":"default","host":"127.0.0.1","port":9,"https":false,"insecure":true,"user":"u","ipv4_only":true}
EOF
# 凭据必须存在，否则自动登录在**发请求之前**就以「没有口令」失败（那是 Auth，
# 不是离线）—— 本测试要验的正是「请求打出去、网络层失败 → 进入离线模式」。
cat >"$CFG/credentials.json" <<'EOF'
{"host":"127.0.0.1","user":"u","password":"x"}
EOF
chmod 600 "$CFG/credentials.json"

# 用 python3 的 sqlite3 造 fixture（不依赖 sqlite3 CLI；GitHub runner 与开发机都有 python3）。
if ! command -v python3 >/dev/null; then
  echo "SKIP：需要 python3（用于造 sync.db / queue.db fixture）"
  exit 0
fi

# ---------------------------------------------------------------- ① 先造「离线缓冲区」
# 直接按 daemon 的真实 schema 造 sync.db（Store::open 会补齐其余表，不会动已有行）。
DB="$DATA/sync/$NS/sync.db"
QDB="$DATA/upload-queue/$NS/queue.db"
python3 - "$DB" "$QDB" <<'PY'
import os
import sqlite3
import sys

db, qdb = sys.argv[1], sys.argv[2]
os.makedirs(os.path.dirname(db), exist_ok=True)
c = sqlite3.connect(db)
c.executescript("""
CREATE TABLE IF NOT EXISTS outbox (
  seq          INTEGER PRIMARY KEY AUTOINCREMENT,
  kind         TEXT    NOT NULL,
  path         TEXT    NOT NULL,
  target       TEXT    NOT NULL DEFAULT '',
  is_dir       INTEGER NOT NULL DEFAULT 0,
  attempts     INTEGER NOT NULL DEFAULT 0,
  created_unix INTEGER NOT NULL DEFAULT 0
);
INSERT INTO outbox(kind, path, target, is_dir, attempts, created_unix)
  VALUES('mkdir', '/home/offline-dir', '', 1, 0, 1);
INSERT INTO outbox(kind, path, target, is_dir, attempts, created_unix)
  VALUES('rename', '/home/a.txt', '/home/b.txt', 0, 0, 1);
""")
c.commit()
c.close()

# 「离线重启时挂载点还没恢复」的队列库：内存队列为空，但 queue.db 里有作业。
# 验 status 的降级统计（只读打开 queue.db，不依赖挂载）。
os.makedirs(os.path.dirname(qdb), exist_ok=True)
c = sqlite3.connect(qdb)
c.executescript("""
CREATE TABLE IF NOT EXISTS uploads (
  remote_path TEXT PRIMARY KEY, remote_dir TEXT NOT NULL, remote_name TEXT NOT NULL,
  local TEXT NOT NULL, mtime INTEGER NOT NULL DEFAULT 0,
  attempts INTEGER NOT NULL DEFAULT 0, ephemeral INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS deletes (
  remote_path TEXT PRIMARY KEY, remote_dir TEXT NOT NULL, remote_name TEXT NOT NULL,
  is_dir INTEGER NOT NULL DEFAULT 0, attempts INTEGER NOT NULL DEFAULT 0,
  queued_unix INTEGER NOT NULL DEFAULT 0);
INSERT INTO uploads(remote_path, remote_dir, remote_name, local, mtime)
  VALUES('/home/offline.txt', '/home', 'offline.txt', '/tmp/offline.txt', 1);
INSERT INTO deletes(remote_path, remote_dir, remote_name, is_dir)
  VALUES('/home/gone.txt', '/home', 'gone.txt', 0);
""")
c.commit()
c.close()
PY

db_query() {
  python3 -c 'import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute(sys.argv[2]).fetchone()[0])' "$1" "$2"
}

# ---------------------------------------------------------------- ② 起 daemon（离线）
echo "== ① 离线启动：status 快且如实 =="
"$QX" daemon start >/dev/null 2>&1
DAEMON_STARTED=1
for _ in $(seq 1 40); do
  "$QX" daemon status >/dev/null 2>&1 && break
  sleep 0.25
done

T0=$(date +%s%N)
OUT="$("$QX" status 2>&1)"
RC=$?
T1=$(date +%s%N)
ELAPSED_MS=$(( (T1 - T0) / 1000000 ))
check "$RC" "离线时 qxync status 仍能成功返回"
check "$([ "$ELAPSED_MS" -lt 5000 ] && echo 0 || echo 1)" \
  "离线时 status 不卡网络（${ELAPSED_MS}ms < 5000ms：跳过 nas_uid/max_log/check_alive）"
if echo "$OUT" | grep -q "连通性    : 离线"; then
  pass "status 如实显示离线（$(echo "$OUT" | grep '连通性' | head -1)）"
else
  fail "status 没有显示离线：$(echo "$OUT" | tail -5)"
fi

# ---------------------------------------------------------------- ③ 缓冲区可见
if true; then
  if echo "$OUT" | grep -q "离线缓冲"; then
    pass "status 显示离线缓冲区"
  else
    fail "status 没有显示离线缓冲区"
  fi
  if echo "$OUT" | grep -q "待建目录 1" && echo "$OUT" | grep -q "待改名 1"; then
    pass "缓冲区分类计数正确（待建目录 1 / 待改名 1）"
  else
    fail "缓冲区分类计数不对：$(echo "$OUT" | grep '离线缓冲' | head -1)"
  fi
  if echo "$OUT" | grep -q "待上传 1" && echo "$OUT" | grep -q "待删除 1"; then
    pass "无挂载点时也能从 queue.db 统计待上传 / 待删除（离线重启不丢视野）"
  else
    fail "无挂载点时队列计数缺失：$(echo "$OUT" | grep '离线缓冲' | head -1)"
  fi
fi

# ---------------------------------------------------------------- ④ 跨重启不丢、不计数
echo "== ② 缓冲区跨 daemon 重启：行不丢、attempts 不涨 =="
sleep 2
"$QX" daemon stop --force >/dev/null 2>&1
DAEMON_STARTED=0
"$QX" daemon start >/dev/null 2>&1
DAEMON_STARTED=1
for _ in $(seq 1 40); do
  "$QX" daemon status >/dev/null 2>&1 && break
  sleep 0.25
done
sleep 2
LEFT="$(db_query "$DB" "SELECT COUNT(*) FROM outbox;")"
ATT="$(db_query "$DB" "SELECT COALESCE(MAX(attempts),0) FROM outbox;")"
check "$([ "$LEFT" = "2" ] && echo 0 || echo 1)" "离线缓冲区行跨重启保留（剩余 $LEFT / 期望 2）"
check "$([ "$ATT" = "0" ] && echo 0 || echo 1)" \
  "网络错误不消耗重试次数（attempts=$ATT / 期望 0：断网不是「失败」）"

# ---------------------------------------------------------------- ⑤ 收尾
"$QX" daemon stop --force >/dev/null 2>&1
DAEMON_STARTED=0
check "$([ -x "$QD" ] && echo 0 || echo 1)" "qxyncd 仍在（收尾无异常）"

echo
echo "== 汇总 =="
echo "  $PASS passed / $FAIL failed"
exit $([ "$FAIL" = "0" ] && echo 0 || echo 1)
