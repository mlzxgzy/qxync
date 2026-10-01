#!/usr/bin/env bash
# m82-matrix.sh —— M8.2（同步任务模型 + 持久化）验收矩阵
#
# 用法:
#   xtask/tests/m82-matrix.sh                 # 全量（需要 /dev/fuse + 真机凭据，~1min）
#   xtask/tests/m82-matrix.sh --no-fuse       # 跳过真挂载项（只验登记/校验/CLI/JSON）
#
# 依赖:
#   * cargo build --workspace（target/debug/{qxyncd,qsync}）
#   * /dev/fuse（真挂载项）；没有时自动跳过并说明
#   * 真机凭据：$XDG_CONFIG_HOME/qsync/{links/<id>.json,credentials.json}
#
# 验的是什么（对应 docs/M8-向Qsync-Client-6靠拢.md 的 M8.2 验收 ①–④）:
#   ① 迁移/兼容：**不带 task 参数的挂载不登记任务**（= M7 行为一字不变）；
#   ② daemon 重启后 `--restore-tasks` 能把启用任务自动恢复挂载；
#      负向对照：**不开 flag 时不恢复**（默认关是刻意的，防止旧挂载复活）；
#   ③ 暂停一个任务只影响它自己：t1 卸载、t2 仍然挂着；
#   ④ `qsync task list/add/rm/pause/resume/mount --json` 全可用；
#   ⑤ 安全：非法 id（`../x`）被拒；`rm` 只删登记、不动挂载点里的数据。
#
# 本矩阵**只用自己私有的 XDG 目录**（$RUNDIR/m82），不碰其它矩阵的状态。
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DAEMON="$REPO/target/debug/qxyncd"
QS="$REPO/target/debug/qsync"
RUNDIR="${QSYNC_TEST_RUNDIR:-$REPO/.local-run}/m82"
SOCK="$RUNDIR/s.sock"
LOG="$RUNDIR/daemon.log"
MNT1="$RUNDIR/mnt1"
MNT2="$RUNDIR/mnt2"

NO_FUSE=0
for a in "$@"; do
  case "$a" in
    --no-fuse) NO_FUSE=1 ;;
    *) echo "未知参数: $a"; exit 2 ;;
  esac
done

PASS=0; FAIL=0
ok()   { echo "  ✅ $1"; PASS=$((PASS+1)); }
bad()  { echo "  ❌ $1"; FAIL=$((FAIL+1)); }
check(){ if [ "$1" = "0" ]; then ok "$2"; else bad "$2"; fi; }
skip() { echo "  ⏭️  $1"; }

rm -rf "$RUNDIR"
mkdir -p "$RUNDIR/config/qsync/links" "$MNT1" "$MNT2"

# 凭据从默认 XDG 目录借（不改动它）
SRC_XDG="${QSYNC_SRC_XDG_CONFIG:-$REPO/.local-run/config}"
if [ ! -f "$SRC_XDG/qsync/links/${QSYNC_TEST_LINK:-default}.json" ]; then
  echo "❌ 找不到连接配置：$SRC_XDG/qsync/links/${QSYNC_TEST_LINK:-default}.json"
  echo "   先跑一次：XDG_CONFIG_HOME=$SRC_XDG cargo run -p qxync-cli -- --host <NAS> ... login"
  exit 2
fi
cp "$SRC_XDG/qsync/links/${QSYNC_TEST_LINK:-default}.json" "$RUNDIR/config/qsync/links/"
[ -f "$SRC_XDG/qsync/credentials.json" ] && cp "$SRC_XDG/qsync/credentials.json" "$RUNDIR/config/qsync/"

export XDG_CONFIG_HOME="$RUNDIR/config"
export XDG_DATA_HOME="$RUNDIR/data"
export XDG_STATE_HOME="$RUNDIR/state"
export QSYNC_SOCKET="$SOCK"
export RUST_LOG="${RUST_LOG:-warn}"
DAEMON_PID=""

if [ ! -x "$DAEMON" ] || [ ! -x "$QS" ]; then echo "❌ 缺少二进制，先 cargo build --workspace"; exit 1; fi
[ "$NO_FUSE" = "0" ] && [ ! -e /dev/fuse ] && { echo "  ⚠️  没有 /dev/fuse → 自动跳过真挂载项"; NO_FUSE=1; }

Q() { "$QS" --socket "$SOCK" "$@"; }
jqv() { jq -r "$1" 2>/dev/null; }

start_daemon() {  # $1: 额外参数（如 --restore-tasks / --auto-login）
  pkill -x qxyncd 2>/dev/null; sleep 0.4; rm -f "$SOCK"
  # shellcheck disable=SC2086
  "$DAEMON" --link "${QSYNC_TEST_LINK:-default}" --socket "$SOCK" --foreground $1 >>"$LOG" 2>&1 &
  DAEMON_PID=$!
  for _ in $(seq 1 60); do [ -S "$SOCK" ] && return 0; sleep 0.25; done
  return 1
}
stop_daemon() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null
  pkill -x qxyncd 2>/dev/null
  sleep 0.6; rm -f "$SOCK"; DAEMON_PID=""
}
trap 'stop_daemon; for m in "$MNT1" "$MNT2"; do fusermount3 -u "$m" 2>/dev/null; done' EXIT

# ---------------------------------------------------------------- 0. 起 daemon
echo "== 0. 起 daemon + 登录 =="
start_daemon ""; check $? "qxyncd 起来（socket $SOCK）"
Q --via-daemon login >/dev/null 2>&1
check $? "登录成功（真机 NAS）"

# ---------------------------------------------------------------- 1. 空任务列表
echo "== 1. 空任务列表 =="
L0=$(Q task list --json 2>/dev/null)
check "$([ "$(echo "$L0" | jqv .empty)" = "true" ] && echo 0 || echo 1)" "初始 empty=true（还没有任务文件）"
check "$([ "$(echo "$L0" | jqv '.tasks|length')" = "0" ] && echo 0 || echo 1)" "初始任务数 0"

# ---------------------------------------------------------------- 2. add（登记 + 挂载）
echo "== 2. task add（登记 + 挂载） =="
if [ "$NO_FUSE" = "1" ]; then
  skip "真挂载项（--no-fuse）"
  Q task add --id t1 --mountpoint "$MNT1" --root /home --json >/dev/null 2>&1
  check $? "task add 成功（未挂载）"
else
  ADD=$(Q task add --id t1 --mountpoint "$MNT1" --root /home --json 2>/dev/null)
  check $? "task add 成功"
  check "$([ -f "$XDG_CONFIG_HOME/qsync/tasks/t1.json" ] && echo 0 || echo 1)" "任务文件落盘 tasks/t1.json"
  check "$([ "$(echo "$ADD" | jqv '.saved.saved')" = "true" ] && echo 0 || echo 1)" "响应 saved=true"
  ENTRY=$(ls "$MNT1" 2>/dev/null | head -1)
  check "$([ -n "$ENTRY" ] && echo 0 || echo 1)" "挂载点可读（看到 $ENTRY）"
fi

# ---------------------------------------------------------------- 3. list 反映状态
echo "== 3. task list --json =="
L1=$(Q task list --json 2>/dev/null)
check "$([ "$(echo "$L1" | jqv '.tasks|length')" = "1" ] && echo 0 || echo 1)" "任务数 1"
check "$([ "$(echo "$L1" | jqv '.tasks[0].task.id')" = "t1" ] && echo 0 || echo 1)" "id=t1"
check "$([ "$(echo "$L1" | jqv '.tasks[0].task.direction')" = "2way" ] && echo 0 || echo 1)" "方向默认 2way"
check "$([ "$(echo "$L1" | jqv '.tasks[0].task.cache_mode')" = "pagecache" ] && echo 0 || echo 1)" "cache_mode 落到文件里"
[ "$NO_FUSE" = "0" ] && check "$([ "$(echo "$L1" | jqv '.tasks[0].mounted')" = "true" ] && echo 0 || echo 1)" "mounted=true"

# ---------------------------------------------------------------- 4. 兼容：不带 task 的挂载不登记
echo "== 4. 兼容（M7 行为不变） =="
M3="$RUNDIR/mnt3"; mkdir -p "$M3"
if [ "$NO_FUSE" = "1" ]; then
  skip "不带 task 的挂载不登记（--no-fuse）"
else
  Q mount "$M3" --remote /home >/dev/null 2>&1
  check $? "普通 mount（不带 task）成功"
  BEFORE=$(ls "$XDG_CONFIG_HOME/qsync/tasks/" 2>/dev/null | wc -l)
  check "$([ "$BEFORE" = "1" ] && echo 0 || echo 1)" "任务文件仍是 1 个（普通 mount **不登记**任务）"
  Q umount "$M3" >/dev/null 2>&1
fi

# ---------------------------------------------------------------- 5. 默认不恢复（负向对照）
echo "== 5. 重启恢复：负向对照（不开 flag） =="
stop_daemon
start_daemon ""; check $? "daemon 重启（无 --restore-tasks）"
Q --via-daemon login >/dev/null 2>&1
sleep 1.5
if [ "$NO_FUSE" = "1" ]; then
  skip "默认不自动恢复（--no-fuse）"
else
  L2=$(Q task list --json 2>/dev/null)
  check "$([ "$(echo "$L2" | jqv '.tasks[0].mounted')" = "false" ] && echo 0 || echo 1)" \
    "默认**不**自动恢复挂载（mounted=false）—— 防止上次残余的挂载复活"
fi

# ---------------------------------------------------------------- 6. --restore-tasks 自动恢复
echo "== 6. 重启恢复：--restore-tasks =="
stop_daemon
start_daemon "--restore-tasks --auto-login"; check $? "daemon 重启（--restore-tasks --auto-login）"
if [ "$NO_FUSE" = "1" ]; then
  skip "自动恢复挂载（--no-fuse）"
else
  ROK=1
  for _ in $(seq 1 40); do
    if awk -v mp="$MNT1" '$2==mp{found=1} END{exit !found}' /proc/mounts; then ROK=0; break; fi
    sleep 0.5
  done
  check "$ROK" "★ 启用任务被自动恢复挂载（$MNT1 真的挂在 /proc/mounts 里）"
  check "$([ -n "$(ls "$MNT1" 2>/dev/null | head -1)" ] && echo 0 || echo 1)" "恢复后的挂载点可读"
  check "$([ "$(Q task list --json 2>/dev/null | jqv '.tasks[0].mounted')" = "true" ] && echo 0 || echo 1)" "list 里 mounted=true"
fi

# ---------------------------------------------------------------- 7. 暂停只影响自己
echo "== 7. 暂停一个任务不影响其它任务 =="
if [ "$NO_FUSE" = "1" ]; then
  Q task add --id t2 --mountpoint "$MNT2" --root /home --no-mount >/dev/null 2>&1
  Q task pause t2 >/dev/null 2>&1
  check $? "task pause 可用（--no-fuse）"
else
  Q task add --id t2 --mountpoint "$MNT2" --root /home >/dev/null 2>&1
  check $? "第二个任务 t2 已登记并挂载"
  Q task pause t1 >/dev/null 2>&1
  check $? "task pause t1 成功"
  sleep 0.8
  if awk -v mp="$MNT1" '$2==mp{found=1} END{exit !found}' /proc/mounts; then
    bad "t1 暂停后仍然挂着（应当已卸载）"
  else
    ok "★ t1 暂停后已卸载"
  fi
  if awk -v mp="$MNT2" '$2==mp{found=1} END{exit !found}' /proc/mounts; then
    ok "★ t2 仍然挂着（暂停 t1 不影响 t2）"
  else
    bad "t2 被误伤（暂停 t1 把 t2 也弄掉了）"
  fi
  L3=$(Q task list --json 2>/dev/null)
  T1EN=$(echo "$L3" | jq -r '.tasks[] | select(.task.id=="t1") | .task.enabled' 2>/dev/null)
  T2EN=$(echo "$L3" | jq -r '.tasks[] | select(.task.id=="t2") | .task.enabled' 2>/dev/null)
  check "$([ "$T1EN" = "false" ] && echo 0 || echo 1)" "t1 enabled=false（落盘）"
  check "$([ "$T2EN" = "true" ] && echo 0 || echo 1)" "t2 enabled=true（不受影响）"
fi

# ---------------------------------------------------------------- 8. resume
echo "== 8. task resume =="
Q task resume t1 >/dev/null 2>&1
check $? "task resume t1 成功"
if [ "$NO_FUSE" = "0" ]; then
  ROK=1
  for _ in $(seq 1 40); do
    if awk -v mp="$MNT1" '$2==mp{found=1} END{exit !found}' /proc/mounts; then ROK=0; break; fi
    sleep 0.5
  done
  check "$ROK" "★ resume 后重新挂载（$MNT1 回到 /proc/mounts）"
fi

# ---------------------------------------------------------------- 9. 安全
echo "== 9. 安全（非法 id / rm 只删登记） =="
Q task rm '../evil' >/dev/null 2>&1
check "$([ $? -ne 0 ] && echo 0 || echo 1)" "非法 id（../evil）被拒"
if [ ! -e "$RUNDIR/evil.json" ] && [ ! -e "$REPO/evil.json" ] && [ ! -e "$(dirname "$REPO")/evil.json" ]; then
  ok "拒绝后没有在磁盘上乱建文件"
else
  bad "非法 id 竟然在磁盘上建了文件"
fi
STATE_BEFORE=$(Q task list --json 2>/dev/null | jqv '.tasks|length')
Q task rm t2 >/dev/null 2>&1
check $? "task rm t2 成功"
STATE_AFTER=$(Q task list --json 2>/dev/null | jqv '.tasks|length')
check "$([ "$STATE_BEFORE" = "2" ] && [ "$STATE_AFTER" = "1" ] && echo 0 || echo 1)" "rm 后任务数 2 → 1（只删登记）"
if [ "$NO_FUSE" = "0" ]; then
  check "$([ -n "$(ls "$MNT1" 2>/dev/null | head -1)" ] && echo 0 || echo 1)" "rm t2 后 t1 的挂载点数据完好"
fi

# ---------------------------------------------------------------- 10. 缓存目录可配置
echo "== 10. 缓存目录（--cache-dir） =="
MYCACHE="$RUNDIR/mycache"; mkdir -p "$MYCACHE"
Q task add --id c1 --mountpoint "$RUNDIR/mntc" --root /home --cache-dir "$MYCACHE" --no-mount >/dev/null 2>&1
check $? "task add --cache-dir 成功"
CD=$(jq -r '.cache_dir // "null"' "$XDG_CONFIG_HOME/qsync/tasks/c1.json" 2>/dev/null)
check "$([ "$CD" = "$MYCACHE" ] && echo 0 || echo 1)" "缓存目录落进任务文件（$CD）"
# 相对路径必须被拒
Q task add --id bad --mountpoint "$RUNDIR/mntb" --cache-dir "relative/cache" --no-mount >/dev/null 2>&1
check "$([ $? -ne 0 ] && echo 0 || echo 1)" "相对缓存目录被拒"

# ---------------------------------------------------------------- 汇总
echo
echo "== 汇总 =="
echo "  通过 $PASS / 失败 $FAIL"
if [ "$FAIL" = "0" ]; then echo "  🎉 M8.2 验收矩阵全过"; else echo "  ❌ 有失败项（日志 $LOG）"; fi
exit $([ "$FAIL" = "0" ] && echo 0 || echo 1)
