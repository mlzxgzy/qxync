#!/usr/bin/env bash
# m5-matrix.sh —— M5（SQLite 元数据 + librsync 兼容 delta）验收矩阵
#
# 用法:
#   xtask/tests/m5-matrix.sh                 # 全量（含真机 delta_gate 判定）
#   xtask/tests/m5-matrix.sh --no-nas        # 跳过真机项（只跑本地状态库/编解码）
#
# 依赖: cargo build --workspace、真机凭据（先 `qxync login`，见 docs/测试环境.local.md）、jq
#
# 验的是什么（每一项都可复现）:
#   1. M2c 的 cursors.json / baseline.json 能被迁进 sync.db，且旧文件归档成 *.json.migrated；
#   2. 迁移是幂等的（重启不会重复导入 / 不会丢数据）；
#   3. **不再双写 JSON**：daemon 跑过一轮轮询（30s）后，旧的 baseline.json/cursors.json 不会复活；
#   4. 游标 + baseline 同一个事务落盘，`store --integrity` 报 ok；
#   5. pin 落库并活过 daemon 重启（M5 之前 pin 只在内存，重启就丢）；
#   6. delta 编解码（sign/delta/patch，1 MiB 块 / 16 字节 MD4）单测全绿；
#   7. 客户端 DeltaGate 在真机上的判定（这台 NAS：versioning_support=0 → Unavailable）。
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DAEMON="$REPO/target/debug/qxyncd"
QS="$REPO/target/debug/qxync"
RUNDIR="${QXNYC_TEST_RUNDIR:-$REPO/.local-run}"
M5="$RUNDIR/m5"
SOCK="$M5/m5.sock"
DBG="$M5/daemon.log"
LINK="${QXNYC_TEST_LINK:-default}"
export CARGO_HOME="${CARGO_HOME:-$REPO/.cargo-home}"

NO_NAS=0
for a in "$@"; do
  case "$a" in
    --no-nas) NO_NAS=1 ;;
    *) echo "未知参数: $a"; exit 2 ;;
  esac
done

# 状态库放**全新的**目录，避免和别的验收矩阵互相踩
export XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-$RUNDIR/config}"
export XDG_DATA_HOME="$M5/data"
export XDG_STATE_HOME="$M5/state"
export QXNYC_SOCKET="$SOCK"
export RUST_LOG="${RUST_LOG:-info}"

PASS=0; FAIL=0; DAEMON_PID=""
ok()   { echo "  ✅ $1"; PASS=$((PASS+1)); }
bad()  { echo "  ❌ $1"; FAIL=$((FAIL+1)); }
check(){ if [ "$1" = "0" ]; then ok "$2"; else bad "$2"; fi }
skip() { echo "  ⏭️  $1"; }

cleanup() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null
  sleep 0.3
  rm -f "$SOCK"
  return 0
}
trap cleanup EXIT

start_daemon() {
  rm -f "$SOCK"
  "$DAEMON" --link "$LINK" --socket "$SOCK" --foreground >>"$DBG" 2>&1 &
  DAEMON_PID=$!
  for _ in $(seq 1 60); do
    [ -S "$SOCK" ] && "$QS" --socket "$SOCK" store >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  return 1
}
stop_daemon() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null
  DAEMON_PID=""
  for _ in $(seq 1 40); do [ -S "$SOCK" ] || return 0; sleep 0.25; done
  return 0
}
sj() { jq -r "$1" "$M5/store.json" 2>/dev/null; }

# ---------------------------------------------------------------- 0. 前置
echo "== 0. 前置 =="
miss=0
for b in "$DAEMON" "$QS"; do [ -x "$b" ] || { echo "  ❌ 缺少 $b（先 cargo build --workspace）"; miss=1; }; done
command -v jq >/dev/null || { echo "  ❌ 需要 jq"; miss=1; }
check "$miss" "qxyncd / qxync / jq 都在"
LINKF="$XDG_CONFIG_HOME/qxync/links/$LINK.json"
if [ ! -f "$LINKF" ]; then
  echo "  ❌ 没有连接配置 $LINKF —— 先跑一次 qxync login"; exit 2
fi
HOST=$(jq -r .host "$LINKF")
ok "连接配置存在（host=$HOST）"

rm -rf "$M5"; mkdir -p "$M5" "$XDG_DATA_HOME/qxync/sync/$HOST"

# ---------------------------------------------------------------- 1. 造一份「M2c 时代」的 JSON
echo "== 1. M2c 的 JSON → sync.db 迁移 =="
LEG="$XDG_DATA_HOME/qxync/sync/$HOST"
cat >"$LEG/cursors.json" <<'JSON'
{"config":188,"notify":37,"global_notify":177,"max_log_seen":188,"log_missing_count":111}
JSON
cat >"$LEG/baseline.json" <<'JSON'
{"version":1,"entries":{
  "/home/qxync-test/hello.txt":{"exists":true,"is_dir":false,"size":24,"mtime":1790769708},
  "/home/qxync-test":{"exists":true,"is_dir":true,"size":0,"mtime":1790769000},
  "/home/qxync-test/gone.txt":{"exists":false,"is_dir":false,"size":0,"mtime":0}
}}
JSON
check 0 "已铺好旧 JSON（cursors + 3 条 baseline，含 1 条 MISSING）"

start_daemon; check $? "qxyncd 起来了（状态目录 $XDG_DATA_HOME/qxync/sync/$HOST）"
grep -aq "M5 状态迁移" "$DBG"; check $? "启动日志里有「M5 状态迁移」"
grep -aq "baseline=3 条" "$DBG"; check $? "日志显示导入了 3 条 baseline"

"$QS" --socket "$SOCK" store --integrity --json >"$M5/store.json" 2>/dev/null
check "$([ "$(sj .cursors.config)" = "188" ] && [ "$(sj .cursors.notify)" = "37" ] && [ "$(sj .cursors.global_notify)" = "177" ] && [ "$(sj .cursors.max_log_seen)" = "188" ] && [ "$(sj .cursors.log_missing_count)" = "111" ] && echo 0 || echo 1)" "三个游标 + max_log_seen + log_missing_count 原样迁移"
check "$([ "$(sj .baseline_entries)" = "3" ] && echo 0 || echo 1)" "baseline 3 条进库"
# ★ M8.3/M8.4：schema 升到 v3（v2 新增 journal 表，v3 新增 decisions 表）。
#   老库在打开时**自动补表**（CREATE TABLE IF NOT EXISTS），不写迁移代码。
#   已有数据一行不动 —— 这正是下面几条要断言的。
check "$([ "$(sj .schema_version)" = "3" ] && echo 0 || echo 1)" "schema 版本 v3（M8.4 起：+decisions 冲突待裁决队列）"
HASJ=$(python3 -c "
import sqlite3,sys
c=sqlite3.connect(sys.argv[1])
print(1 if c.execute(\"SELECT name FROM sqlite_master WHERE type='table' AND name='journal'\").fetchone() else 0)
" "$LEG/sync.db")
check "$([ "$HASJ" = "1" ] && echo 0 || echo 1)" "★ journal 表已自动创建（老库补表，不需要迁移脚本）"
JROWS=$(python3 -c "
import sqlite3,sys
print(sqlite3.connect(sys.argv[1]).execute('SELECT COUNT(*) FROM journal').fetchone()[0])
" "$LEG/sync.db")
check "$([ "$JROWS" = "0" ] && echo 0 || echo 1)" "新补的 journal 表是空的（$JROWS 行）"
check "$([ "$(sj .integrity)" = "ok" ] && echo 0 || echo 1)" "PRAGMA integrity_check = ok"
check "$([ "$(sj .path)" = "$LEG/sync.db" ] && echo 0 || echo 1)" "状态库路径正确（$(sj .path)）"
check "$([ -f "$LEG/baseline.json.migrated" ] && [ -f "$LEG/cursors.json.migrated" ] && echo 0 || echo 1)" "旧 JSON 已归档成 *.json.migrated（保留备份）"
check "$([ ! -f "$LEG/baseline.json" ] && [ ! -f "$LEG/cursors.json" ] && echo 0 || echo 1)" "旧 JSON 不再在原来的位置上"

# ---------------------------------------------------------------- 2. 幂等 + 不双写
echo "== 2. 幂等 + 不双写 JSON =="
stop_daemon; start_daemon; check $? "重启 daemon 成功"
MCOUNT=$(grep -ac "M5 状态迁移" "$DBG")
check "$([ "$MCOUNT" = "1" ] && echo 0 || echo 1)" "迁移只发生一次（日志里出现 $MCOUNT 次）"
"$QS" --socket "$SOCK" store --json >"$M5/store.json" 2>/dev/null
check "$([ "$(sj .baseline_entries)" = "3" ] && echo 0 || echo 1)" "重启后 baseline 仍是 3 条（没被清空/没翻倍）"

echo "  …等一轮后台轮询（30s）确认不再双写 JSON"
sleep 32
NEWJSON=0
[ -f "$LEG/baseline.json" ] && NEWJSON=1
[ -f "$LEG/cursors.json" ] && NEWJSON=1
check "$([ "$NEWJSON" = "0" ] && echo 0 || echo 1)" "轮询一轮后旧 JSON 没有复活（M5 起只写 SQLite）"
"$QS" --socket "$SOCK" store --json >"$M5/store.json" 2>/dev/null
check "$([ "$(sj .baseline_entries)" = "3" ] && echo 0 || echo 1)" "轮询后 baseline 行数不变"

# ---------------------------------------------------------------- 3. pin 持久化
echo "== 3. pin 落库 + 活过重启 =="
"$QS" --socket "$SOCK" pin /home/qxync-test/hello.txt pinned >/dev/null 2>&1
"$QS" --socket "$SOCK" pin /home/qxync-test/big.bin excluded >/dev/null 2>&1
"$QS" --socket "$SOCK" store --json >"$M5/store.json" 2>/dev/null
check "$([ "$(sj .pins.\"/home/qxync-test/hello.txt\")" = "pinned" ] && echo 0 || echo 1)" "pin 写进状态库（hello.txt=pinned）"
stop_daemon; start_daemon; check $? "再次重启 daemon"
grep -aq "从状态库恢复 2 条 pin" "$DBG"; check $? "启动时从状态库恢复 pin"
"$QS" --socket "$SOCK" store --json >"$M5/store.json" 2>/dev/null
check "$([ "$(sj .pins.\"/home/qxync-test/big.bin\")" = "excluded" ] && echo 0 || echo 1)" "重启后 pin 还在（big.bin=excluded）"
P=$("$QS" --socket "$SOCK" pin /home/qxync-test/hello.txt 2>/dev/null | tail -1)
check "$([ "$P" = "pinned" ] && echo 0 || echo 1)" "IPC 查 pin 也一致（$P）"

# ---------------------------------------------------------------- 4. 编解码 / 队列单测
echo "== 4. delta 编解码 + 上传队列 SQLite 单测 =="
( cd "$REPO" && cargo test -p qxync-core --lib delta 2>&1 | tail -20 >"$M5/t-delta.log" )
grep -aq "test result: ok" "$M5/t-delta.log"; check $? "cargo test -p qxync-core --lib delta 全绿（$(grep -ao '[0-9]* passed' "$M5/t-delta.log" | head -1)）"
( cd "$REPO" && cargo test -p qxync-core --lib store 2>&1 | tail -20 >"$M5/t-store.log" )
grep -aq "test result: ok" "$M5/t-store.log"; check $? "cargo test -p qxync-core --lib store 全绿（$(grep -ao '[0-9]* passed' "$M5/t-store.log" | head -1)）"
( cd "$REPO" && cargo test -p qxync-fuse 2>&1 | tail -20 >"$M5/t-fuse.log" )
grep -aq "test result: ok" "$M5/t-fuse.log"; check $? "cargo test -p qxync-fuse 全绿（上传队列入库；$(grep -ao '[0-9]* passed' "$M5/t-fuse.log" | head -1)）"
( cd "$REPO" && cargo test -p qxync-client 2>&1 | tail -20 >"$M5/t-client.log" )
grep -aq "test result: ok" "$M5/t-client.log"; check $? "cargo test -p qxync-client 全绿（DeltaGate 离线三路径；$(grep -ao '[0-9]* passed' "$M5/t-client.log" | head -1)）"

# ---------------------------------------------------------------- 5. 真机：delta 能力判定
echo "== 5. 真机 delta_gate（versioning_* 能力判定） =="
if [ "$NO_NAS" = "1" ]; then
  skip "真机项（--no-nas）"
else
  CRED="$XDG_CONFIG_HOME/qxync/credentials.json"
  if [ ! -f "$CRED" ]; then
    bad "缺凭据 $CRED（先 qxync login）"
  else
    export QXNYC_TEST_HOST="$HOST"
    export QXNYC_TEST_PORT="$(jq -r .port "$LINKF")"
    export QXNYC_TEST_USER="$(jq -r .user "$LINKF")"
    export QXNYC_TEST_PASSWORD="$(jq -r .password "$CRED")"
    export QXNYC_TEST_FIXTURE="${QXNYC_TEST_FIXTURE:-/home/qxync-test}"
    ( cd "$REPO" && cargo test -p qxync-proto-test --test versioning -- --ignored --nocapture 2>&1 | tail -40 >"$M5/t-versioning.log" )
    grep -aq "test result: ok. 1 passed" "$M5/t-versioning.log"; check $? "真机 versioning 探测测试通过（1 passed）"
    grep -ao "gate: .*" "$M5/t-versioning.log" | head -1 | sed 's/^/     /'
    grep -ao "lock: .*" "$M5/t-versioning.log" | head -1 | sed 's/^/     /'
  fi
fi

# ---------------------------------------------------------------- 汇总
echo
echo "== 汇总 =="
echo "  通过 $PASS / 失败 $FAIL"
[ "$FAIL" = "0" ] && echo "  🎉 M5 验收矩阵全过" || echo "  ❌ 有失败项（日志在 $M5/）"
exit $([ "$FAIL" = "0" ] && echo 0 || echo 1)
