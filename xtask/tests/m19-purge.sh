#!/usr/bin/env bash
# m19-purge.sh —— ★ M19「删除相关数据」（task / link / all）的隔离端到端验收
#
# 验的是什么（用户原话：「再添加一个删除所有相关数据的功能」）：
#   1. 任务级 purge：计划里列出「任务登记 + 水合缓存 + 同步状态行」，
#      --yes 后全部消失；**挂载点里的用户文件一个都不少**；前缀不误伤（/homepage ≠ /home）；
#   2. 连接级 purge：sync/<ns> / cache/<ns> / upload-queue/<ns> / 日志 / link / 凭据都清掉，
#      挂载点文件仍然保留；
#   3. daemon 在跑时走 IPC（真正会卸载、会同步清内存 baseline）；
#   4. 全局 purge：config / data / state 三个根清空，挂载点保留；
#   5. 预览是确定性的：同一份输入 `--json` 跑两次必须逐字节一致。
#
# 不需要 NAS、不需要真窗口、不需要真实用户数据：所有 XDG 目录都指向 .local-run 下。
# 用法：xtask/tests/m19-purge.sh
# 依赖：cargo build --workspace（target/debug/{qxync,qxyncd}）；sqlite3 可选（缺了就跳过行数断言）。
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
QX="$REPO/target/debug/qxync"
QD="$REPO/target/debug/qxyncd"
ROOT="$REPO/.local-run/m19-purge-test"

PASS=0
FAIL=0
pass() { echo "PASS $1"; PASS=$((PASS + 1)); }
fail() { echo "FAIL $1"; FAIL=$((FAIL + 1)); }
check() { if [ "$1" = "0" ]; then pass "$2"; else fail "$2"; fi; }

if [ ! -x "$QX" ]; then
  echo "SKIP：没有 $QX（先 cargo build --workspace）"
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
MNT="$ROOT/mnt/home"
NS="nas.local"

mkdir -p "$CFG/links" "$CFG/tasks" "$DATA" "$STATE/log" "$XDG_RUNTIME_DIR/qxync" "$MNT"
echo "user data" >"$MNT/keep.txt"
DAEMON_STARTED=0

cleanup() {
  if [ "$DAEMON_STARTED" = "1" ]; then
    "$QX" daemon stop >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

write_link() {
  cat >"$CFG/links/default.json" <<'EOF'
{"id":"default","host":"nas.local","port":1,"https":false,"insecure":true,"user":"u","ipv4_only":true}
EOF
}
write_task() {
  cat >"$CFG/tasks/home.json" <<EOF
{"id":"home","link_id":"default","name":"home","enabled":true,
 "mountpoint":"$MNT","cache_dir":"$DATA/cache/tasks/default/home",
 "root":"/home","read_write":false,"cache_mode":"pagecache",
 "conflict":"rename_local","direction":"2way"}
EOF
}
seed_cache() {
  mkdir -p "$DATA/cache/tasks/default/home/$NS/sub"
  echo hello >"$DATA/cache/tasks/default/home/$NS/a.bin"
  echo x >"$DATA/cache/tasks/default/home/$NS/sub/b.bin"
}

# ---------------------------------------------------------------- ① 任务级（无 daemon）
echo "== ① 任务级 purge（无 daemon，含状态库行） =="
write_link
write_task
seed_cache
DB="$DATA/sync/$NS/sync.db"
mkdir -p "$(dirname "$DB")"
if command -v sqlite3 >/dev/null; then
  sqlite3 "$DB" <<'SQL'
CREATE TABLE IF NOT EXISTS baseline(path TEXT PRIMARY KEY, present INTEGER NOT NULL, is_dir INTEGER NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS journal(id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, task_id TEXT NOT NULL DEFAULT '', kind TEXT NOT NULL, path TEXT NOT NULL DEFAULT '', detail TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'ok', bytes INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS decisions(id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE, task_id TEXT NOT NULL DEFAULT '', is_dir INTEGER NOT NULL DEFAULT 0, local_size INTEGER NOT NULL DEFAULT 0, local_mtime INTEGER NOT NULL DEFAULT 0, remote_size INTEGER NOT NULL DEFAULT 0, remote_mtime INTEGER NOT NULL DEFAULT 0, created_unix INTEGER NOT NULL DEFAULT 0, resolution TEXT);
INSERT OR REPLACE INTO baseline VALUES('/home/a',1,0,6,1),('/home/b',1,0,1,1),('/homepage/d',1,0,1,1);
INSERT INTO journal(ts,kind,path) VALUES(1,'upload','/home/a'),(1,'upload','/homepage/d');
INSERT OR REPLACE INTO decisions(id,path) VALUES('home_a','/home/a'),('homepage_d','/homepage/d');
SQL
  HAVE_SQLITE=1
else
  HAVE_SQLITE=0
fi
P1="$("$QX" purge --task home --json 2>&1)"
P2="$("$QX" purge --task home --json 2>&1)"
if [ -n "$P1" ] && [ "$P1" = "$P2" ]; then
  pass "预览确定性：--json 跑两次逐字节一致"
else
  fail "预览确定性：两次输出不一致"
fi
if command -v jq >/dev/null; then
  check "$([ "$(echo "$P1" | jq -r '.items | map(.kind) | join(",")')" = "task,cache" ] && echo 0 || echo 1)" \
    "计划包含任务登记 + 水合缓存（kind=$(echo "$P1" | jq -r '.items | map(.kind) | join(",")')）"
  if [ "$HAVE_SQLITE" = "1" ]; then
    check "$([ "$(echo "$P1" | jq -r '.db_rows.baseline')" = "2" ] && [ "$(echo "$P1" | jq -r '.db_rows.journal')" = "1" ] && [ "$(echo "$P1" | jq -r '.db_rows.decisions')" = "1" ] && echo 0 || echo 1)" \
      "计划统计状态库行（baseline 2 / journal 1 / decisions 1）"
  fi
fi
"$QX" purge --task home --yes >/dev/null 2>&1
check $? "任务级 purge 执行成功"
check "$([ ! -e "$CFG/tasks/home.json" ] && echo 0 || echo 1)" "任务登记已删除"
check "$([ ! -e "$DATA/cache/tasks/default/home/$NS" ] && echo 0 || echo 1)" "水合缓存子树已删除"
check "$([ -f "$MNT/keep.txt" ] && echo 0 || echo 1)" "挂载点里的用户文件保留（安全底线）"
if [ "$HAVE_SQLITE" = "1" ]; then
  LEFT="$(sqlite3 "$DB" "SELECT group_concat(path,' ') FROM baseline;")"
  check "$([ "$LEFT" = "/homepage/d" ] && echo 0 || echo 1)" \
    "前缀不误伤：/home 删干净、/homepage 与 /Public 保留（剩余=$LEFT）"
  # /home/a 的待裁决要删，/homepage/d 的那条必须留着（前缀安全）
  LEFT_D="$(sqlite3 "$DB" "SELECT group_concat(path,' ') FROM decisions;")"
  check "$([ "$LEFT_D" = "/homepage/d" ] && echo 0 || echo 1)" \
    "待裁决只清 /home 子树（剩余=$LEFT_D）"
fi

# ---------------------------------------------------------------- ② 连接级
echo "== ② 连接级 purge =="
write_link
write_task
seed_cache
mkdir -p "$DATA/sync/$NS" "$DATA/cache/$NS" "$DATA/upload-queue/$NS"
echo db >"$DATA/sync/$NS/sync.db"
echo cache >"$DATA/cache/$NS/x.bin"
echo q >"$DATA/upload-queue/$NS/queue.db"
echo log >"$STATE/log/qxyncd.log.1"
LP="$("$QX" --link default purge --connection --json 2>&1)"
if command -v jq >/dev/null; then
  check "$(echo "$LP" | jq -e '.items | map(.kind) | (index("sync") and index("cache") and index("queue") and index("log"))' >/dev/null 2>&1 && echo 0 || echo 1)" \
    "计划包含该连接的全部本地数据（sync/cache/queue/log）"
fi
"$QX" --link default purge --connection --yes >/dev/null 2>&1
check $? "连接级 purge 执行成功"
check "$([ ! -e "$CFG/links/default.json" ] && echo 0 || echo 1)" "link 配置已删除"
check "$([ ! -e "$DATA/sync/$NS" ] && echo 0 || echo 1)" "同步状态库目录已删除"
check "$([ ! -e "$DATA/cache/$NS" ] && echo 0 || echo 1)" "按连接的水合缓存已删除"
check "$([ ! -e "$DATA/upload-queue/$NS" ] && echo 0 || echo 1)" "上传队列已删除"
check "$([ ! -e "$STATE/log/qxyncd.log.1" ] && echo 0 || echo 1)" "该连接的日志已删除"
check "$([ -f "$MNT/keep.txt" ] && echo 0 || echo 1)" "挂载点里的用户文件保留（连接级）"

# ---------------------------------------------------------------- ③ daemon 在跑时走 IPC
if [ -x "$QD" ]; then
  echo "== ③ 任务级 purge（daemon 在跑 → IPC） =="
  write_link
  write_task
  seed_cache
  # 先把 baseline 写进库、再起 daemon：daemon 启动时会把 baseline 读进内存，
  # 「内存副本为真」是 M5 起的约定 —— 起了再写库相当于绕过它，purge 后会被内存覆盖，
  # 那样测的就不是真实行为（真实用户的数据本来就在 daemon 内存里）。
  DBP="$DATA/sync/$NS/sync.db"
  mkdir -p "$(dirname "$DBP")"
  if [ "$HAVE_SQLITE" = "1" ]; then
    sqlite3 "$DBP" <<'SQL'
CREATE TABLE IF NOT EXISTS baseline(path TEXT PRIMARY KEY, present INTEGER NOT NULL, is_dir INTEGER NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL);
INSERT OR REPLACE INTO baseline VALUES('/home/a',1,0,1,1),('/Public/c',1,0,1,1);
SQL
  fi
  "$QX" daemon start >/dev/null 2>&1
  DAEMON_STARTED=1
  for _ in $(seq 1 40); do
    "$QX" daemon status >/dev/null 2>&1 && break
    sleep 0.25
  done
  "$QX" purge --task home --yes >/dev/null 2>&1
  check $? "daemon 路径 purge 成功"
  check "$([ ! -e "$DATA/cache/tasks/default/home/$NS" ] && echo 0 || echo 1)" "daemon 路径缓存已删除"
  check "$([ ! -e "$CFG/tasks/home.json" ] && echo 0 || echo 1)" "daemon 路径任务登记已删除"
  check "$("$QX" daemon status >/dev/null 2>&1 && echo 0 || echo 1)" "purge 没有把 daemon 弄挂"
  if [ "$HAVE_SQLITE" = "1" ] && [ -n "$DBP" ]; then
    check "$([ "$(sqlite3 "$DBP" "SELECT group_concat(path,' ') FROM baseline;")" = "/Public/c" ] && echo 0 || echo 1)" \
      "daemon 路径 baseline 清理正确（/home 删、/Public 留）"
  fi
else
  echo "SKIP：没有 qxyncd，跳过 daemon 路径"
fi

# ---------------------------------------------------------------- ④ 全局
echo "== ④ 全局 purge（--all） =="
write_link
write_task
seed_cache
mkdir -p "$STATE"
echo x >"$STATE/mounts-probe"
"$QX" purge --all --yes >/dev/null 2>&1
check $? "全局 purge 执行成功"
check "$([ ! -e "$CFG" ] && echo 0 || echo 1)" "配置目录已清空"
check "$([ ! -e "$DATA" ] && echo 0 || echo 1)" "数据目录已清空"
check "$([ ! -e "$STATE" ] && echo 0 || echo 1)" "状态目录已清空"
check "$([ -f "$MNT/keep.txt" ] && echo 0 || echo 1)" "挂载点里的用户文件保留（全局）"

# ---------------------------------------------------------------- 汇总
echo
echo "== 汇总 =="
echo "  $PASS passed / $FAIL failed"
exit $([ "$FAIL" = "0" ] && echo 0 || echo 1)
