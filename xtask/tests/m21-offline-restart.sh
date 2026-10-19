#!/usr/bin/env bash
# m21-offline-restart.sh —— ★ M21「离线可重启 daemon」的隔离端到端验收
#
# 验的是什么（用户原话：「做一下离线可重启 daemon，否则急用会出问题」）：
#   1. **离线时重启 daemon，挂载点必须恢复**：mounts.json 里的记录 + 无会话挂载
#      （M21 之前：恢复挂载要先登录，登录要网络 —— 离线重启 = 挂载点消失到联网为止）。
#   2. **恢复的挂载点离线可用**：`ls` 吃 sync.db 里持久化的目录清单（M21 listings 表），
#      不碰 NAS；离线建的目录（空清单）也跨重启可见。
#   3. **离线写操作照常缓冲**：mkdir / 建文件进 outbox + 上传队列，重启不丢。
#   4. **再重启一次仍然幂等**：第二次离线重启，清单里已经有上一轮离线写的东西。
#
# 环境：NAS 指向 127.0.0.1:9（必拒绝 = Transport = 离线）；需要 /dev/fuse 真挂载
# （没有就 SKIP —— CI 与普通开发机不会误跑）。所有 XDG 目录都在 .local-run 下。
# 用法：xtask/tests/m21-offline-restart.sh
# 依赖：cargo build --workspace（target/debug/{qxync,qxyncd}）；python3。
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
QX="$REPO/target/debug/qxync"
QD="$REPO/target/debug/qxyncd"
ROOT="$REPO/.local-run/m21-offline-restart"

PASS=0
FAIL=0
pass() { echo "PASS $1"; PASS=$((PASS + 1)); }
fail() { echo "FAIL $1"; FAIL=$((FAIL + 1)); }
ok() { if [ "$1" = "0" ]; then pass "$2"; else fail "$2"; fi; }

if [ ! -x "$QX" ] || [ ! -x "$QD" ]; then
  echo "SKIP：没有 $QX / $QD（先 cargo build --workspace）"
  exit 0
fi
if [ ! -e /dev/fuse ]; then
  echo "SKIP：需要 /dev/fuse（真挂载验收；无 FUSE 环境跑 m20-offline.sh 覆盖无挂载部分）"
  exit 0
fi
if ! command -v python3 >/dev/null; then
  echo "SKIP：需要 python3（造 sync.db / mounts.json fixture）"
  exit 0
fi

rm -rf "$ROOT"
mkdir -p "$ROOT"
export XDG_CONFIG_HOME="$ROOT/config"
export XDG_DATA_HOME="$ROOT/data"
export XDG_STATE_HOME="$ROOT/state"
export XDG_RUNTIME_DIR="$ROOT/run"
unset QXNYC_SOCKET QXNYC_HOST QXNYC_USER QXNYC_PASSWORD QXNYC_OTP_SECRET QXNYC_OTP_CODE
CFG="$XDG_CONFIG_HOME/qxync"
DATA="$XDG_DATA_HOME/qxync"
STATE="$XDG_STATE_HOME/qxync"
NS="127.0.0.1"
MNT="$ROOT/mnt"
DAEMON_STARTED=0

mkdir -p "$CFG/links" "$DATA" "$STATE/log" "$XDG_RUNTIME_DIR/qxync" "$(dirname "$MNT")"
chmod 700 "$XDG_RUNTIME_DIR/qxync"

cleanup() {
  if [ "$DAEMON_STARTED" = "1" ]; then
    "$QX" daemon stop --force >/dev/null 2>&1 || true
  fi
  # 兜底卸载（daemon 没起来 / 崩了的时候）
  fusermount3 -u "$MNT" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# 不可达的 NAS：127.0.0.1:9 立刻连接拒绝 → Transport（离线），且拒绝得快（不等超时）
cat >"$CFG/links/default.json" <<'EOF'
{"id":"default","host":"127.0.0.1","port":9,"https":false,"insecure":true,"user":"u","ipv4_only":true}
EOF
cat >"$CFG/credentials.json" <<'EOF'
{"host":"127.0.0.1","user":"u","password":"x"}
EOF
chmod 600 "$CFG/credentials.json"

# ---------------------------------------------------------------- ① 造「上次离线退出」的现场
# sync.db：listings（根清单 / 子目录清单 / 离线建的目录空清单）+ outbox（1 行待回放 mkdir）
DB="$DATA/sync/$NS/sync.db"
python3 - "$DB" <<'PY'
import json
import os
import sqlite3
import sys

db = sys.argv[1]
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
CREATE TABLE IF NOT EXISTS listings (
  dir          TEXT PRIMARY KEY,
  entries      TEXT    NOT NULL,
  updated_unix INTEGER NOT NULL DEFAULT 0
);
""")

def ent(name, folder, size, mt):
    return {
        "filename": name, "filesize": size, "isfolder": folder, "epochmt": mt,
        "have_child": folder, "exist": True, "privilege": None,
        "versioning_support": False, "mt": None,
        "file_id": [0] * 16,
    }

# 根清单：一个子目录 + 一个（未水合的）文件
c.execute(
    "INSERT INTO listings(dir, entries, updated_unix) VALUES('/home', ?, 1)",
    (json.dumps([ent("docs", True, 0, 100), ent("dry.txt", False, 10, 100)]),),
)
# 子目录清单：里面有一个文件 + 上次离线建的目录（真实流程里 mkdir 会 upsert 进父清单）
c.execute(
    "INSERT INTO listings(dir, entries, updated_unix) VALUES('/home/docs', ?, 1)",
    (json.dumps([ent("readme.txt", False, 5, 100), ent("offline-dir", True, 0, 100)]),),
)
# 上次离线建的目录：空清单（M20 落的，M21 起跨重启保留）
c.execute(
    "INSERT INTO listings(dir, entries, updated_unix) VALUES('/home/docs/offline-dir', ?, 1)",
    (json.dumps([]),),
)
# 上次离线攒下的结构操作
c.execute(
    "INSERT INTO outbox(kind, path, target, is_dir, attempts, created_unix)"
    " VALUES('mkdir', '/home/docs/offline-dir', '', 1, 0, 1)"
)
c.commit()
c.close()
PY

# mounts.json：上次退出前的挂载现场（daemon 重启后按它恢复）
# 注意路径是 state/qxync/qxync/mounts.json —— state_dir 本身已含一层 qxync，
# MountsFile::file_for 会再拼一层（与真机 ~/.local/state/qxync/qxync/mounts.json 一致）。
mkdir -p "$STATE/qxync"
cat >"$STATE/qxync/mounts.json" <<EOF
{
  "version": 1,
  "mounts": [
    {
      "mountpoint": "$MNT",
      "remote": "/home",
      "read_write": true,
      "cache_mode": "pagecache",
      "conflict": "rename_local",
      "threads": 2,
      "hydrate_timeout_secs": 20,
      "delete_limit": null,
      "auto_unmount": true
    }
  ]
}
EOF

# ---------------------------------------------------------------- ② 离线启动：挂载点必须恢复
echo "== ① 离线启动 daemon：挂载点恢复（无 NAS / 无会话） =="
"$QX" daemon start >/dev/null 2>&1
DAEMON_STARTED=1
MOUNTED=1
for _ in $(seq 1 60); do
  if "$QX" daemon status >/dev/null 2>&1 && grep -qs " $(echo "$MNT" | sed 's/[\/&]/\\&/g') fuse" /proc/self/mounts; then
    MOUNTED=0
    break
  fi
  sleep 0.25
done
ok "$MOUNTED" "离线重启后挂载点恢复（mounts.json + 无会话挂载）"

# ---------------------------------------------------------------- ③ 恢复的挂载点离线可用
echo "== ② 恢复的挂载点离线可用（清单来自 sync.db，不碰 NAS） =="
LS_OUT="$(ls -1 "$MNT" 2>&1)"
ok "$([ $? = 0 ] && echo 0 || echo 1)" "离线 ls 挂载根目录成功"
echo "$LS_OUT" | grep -q "docs" && pass "根清单装回：docs 可见" || fail "根清单没装回：$LS_OUT"
echo "$LS_OUT" | grep -q "dry.txt" && pass "根清单装回：dry.txt 可见" || fail "根清单缺 dry.txt"

LS_DOCS="$(ls -1 "$MNT/docs" 2>&1)"
echo "$LS_DOCS" | grep -q "readme.txt" && pass "子目录清单装回：docs/readme.txt 可见" || fail "子目录清单缺失：$LS_DOCS"
echo "$LS_DOCS" | grep -q "offline-dir" && pass "上次离线建的目录跨重启可见" || fail "离线建的目录丢了：$LS_DOCS"

LS_OD="$(ls -1 "$MNT/docs/offline-dir" 2>&1)"
ok "$([ -z "$LS_OD" ] && echo 0 || echo 1)" "离线建的目录能 ls（空清单跨重启保留）"

# ---------------------------------------------------------------- ④ 离线写操作照常缓冲
echo "== ③ 离线写操作：mkdir / 建文件（进缓冲区，不丢） =="
mkdir "$MNT/restart-dir" 2>/dev/null
ok "$?" "离线 mkdir 成功（本地视图 + outbox）"
echo "hello-m21" > "$MNT/restart-file.txt" 2>/dev/null
ok "$?" "离线写文件成功（缓存 + 上传队列）"
ls -1 "$MNT" | grep -q "restart-dir" && pass "新目录立刻可见" || fail "新目录不可见"
ls -1 "$MNT" | grep -q "restart-file.txt" && pass "新文件立刻可见" || fail "新文件不可见"

# ---------------------------------------------------------------- ⑤ status 如实
OUT="$("$QX" status 2>&1)"
if echo "$OUT" | grep -q "连通性    : 离线"; then
  pass "status 显示离线"
else
  fail "status 没显示离线：$(echo "$OUT" | grep 连通性 | head -1)"
fi
if echo "$OUT" | grep -q "待建目录"; then
  pass "status 显示待建目录计数"
else
  fail "status 缺待建目录计数"
fi

# ---------------------------------------------------------------- ⑥ 第二次离线重启（幂等）
echo "== ④ 第二次离线重启：上一轮离线写的东西也要回来 =="
"$QX" daemon stop >/dev/null 2>&1 || "$QX" daemon stop --force >/dev/null 2>&1
DAEMON_STARTED=0
sleep 1
fusermount3 -u "$MNT" >/dev/null 2>&1 || true
"$QX" daemon start >/dev/null 2>&1
DAEMON_STARTED=1
MOUNTED=1
for _ in $(seq 1 60); do
  if "$QX" daemon status >/dev/null 2>&1 && mountpoint -q "$MNT" 2>/dev/null; then
    MOUNTED=0
    break
  fi
  sleep 0.25
done
ok "$MOUNTED" "第二次离线重启后挂载点再次恢复"

LS2="$(ls -1 "$MNT" 2>&1)"
echo "$LS2" | grep -q "restart-dir" && pass "上一轮离线建的目录跨重启可见（清单写穿生效）" || fail "restart-dir 丢了：$LS2"
echo "$LS2" | grep -q "restart-file.txt" && pass "上一轮离线建的文件跨重启可见" || fail "restart-file.txt 丢了：$LS2"
echo "$LS2" | grep -q "docs" && pass "原始清单仍在" || fail "原始清单丢了：$LS2"

# outbox 行数：fixture 1 行 + 本轮 mkdir 1 行 = 2（离线不消耗、不丢）
LEFT="$(python3 -c 'import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute("SELECT COUNT(*) FROM outbox").fetchone()[0])' "$DB")"
check_count() { if [ "$1" = "$2" ]; then pass "$3（$1）"; else fail "$3（实际 $1 / 期望 $2）"; fi; }
check_count "$LEFT" "2" "outbox 行跨两次离线重启保留"

# ---------------------------------------------------------------- ⑦ 收尾
"$QX" daemon stop >/dev/null 2>&1 || "$QX" daemon stop --force >/dev/null 2>&1
DAEMON_STARTED=0
sleep 0.5
if mountpoint -q "$MNT" 2>/dev/null; then
  fail "daemon 退出后挂载点应已卸载"
else
  pass "daemon 退出后挂载点已卸载"
fi

echo
echo "m21-offline-restart: PASS=$PASS FAIL=$FAIL"
[ "$FAIL" = "0" ]
