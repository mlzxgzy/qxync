#!/usr/bin/env bash
# pair-1to1.sh —— 「配对文件夹 = 一对一」验收（**不需要 NAS，也不需要 /dev/fuse**）
#
# 用法:
#   xtask/tests/pair-1to1.sh
#
# 依赖:
#   * cargo build -p qxync-cli -p qxync-daemon（target/debug/{qxyncd,qxync}）
#   * 什么都不需要：起一个**私有** daemon（自己的 socket + XDG_CONFIG_HOME），
#     link 是一份指向 `nas.invalid` 的假配置 —— `tasks save` 不碰 NAS。
#
# 验的是什么（对应 2026-10-02 的用户反馈 + CHANGELOG「未发布」）:
#   ① 一对一：一个任务只接受**一个** NAS 目录，多根被拒且**不落盘**；
#   ② 目的地冲突（本地文件夹相同 / 互相嵌套）→ 提交即拒，错误里点名冲突任务；
#   ③ 同一个 NAS 目录被别的任务用了 → **只警告不拦**（只读挂同一目录是合法用法，
#      `m82-matrix.sh` 的 t1/t2 就靠这条）；
#   ④ 旧的多根任务文件仍能读、仍能列（升级不会把已有任务变成坏文件）。
#
# 本矩阵**只用自己私有的 XDG 目录**（$RUNDIR），不碰其它矩阵与用户的真实配置。
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DAEMON="$REPO/target/debug/qxyncd"
QS="$REPO/target/debug/qxync"
RUNDIR="${QXNYC_TEST_RUNDIR:-$REPO/.local-run}/pair-1to1"
SOCK="$RUNDIR/s.sock"
LOG="$RUNDIR/daemon.log"

for bin in "$DAEMON" "$QS"; do
  [ -x "$bin" ] || { echo "❌ 缺可执行文件 $bin —— 先 cargo build -p qxync-cli -p qxync-daemon"; exit 2; }
done
command -v jq >/dev/null || { echo "❌ 需要 jq"; exit 2; }

PASS=0; FAIL=0
ok()   { echo "  ✅ $1"; PASS=$((PASS+1)); }
bad()  { echo "  ❌ $1"; FAIL=$((FAIL+1)); }
check(){ if [ "$1" = "0" ]; then ok "$2"; else bad "$2"; fi; }

rm -rf "$RUNDIR"
mkdir -p "$RUNDIR/config/qxync/links" "$RUNDIR/run"
export XDG_CONFIG_HOME="$RUNDIR/config"
export XDG_RUNTIME_DIR="$RUNDIR/run"
export XDG_DATA_HOME="$RUNDIR/data"
# ⚠️ 假 link：daemon 要有 link 才进「同步模式」（没 link 时它空转待命，只服务 ping/status）。
cat > "$XDG_CONFIG_HOME/qxync/links/default.json" <<'JSON'
{"id":"default","host":"nas.invalid","port":9834,"https":true,"insecure":true,
 "user":"test1","home_root":"/home","roots":["/home"]}
JSON

"$DAEMON" --socket "$SOCK" --foreground >"$LOG" 2>&1 &
DPID=$!
trap 'kill "$DPID" 2>/dev/null; wait "$DPID" 2>/dev/null' EXIT
ready=1
for _ in $(seq 1 60); do
  if [ -S "$SOCK" ] && "$QS" --socket "$SOCK" daemon status >/dev/null 2>&1; then ready=0; break; fi
  sleep 0.5
done
if [ "$ready" != "0" ]; then echo "❌ daemon 没起来，见 $LOG"; exit 1; fi

taskadd() { "$QS" --socket "$SOCK" task add "$@" --json 2>&1; }

echo "== 1. 一对一：正常登记 =="
OUT="$(taskadd --id t1 --mountpoint "$RUNDIR/m1" --root /home --no-mount)"
check $? "t1 登记成功"
echo "$OUT" | jq -e '.saved.saved == true and (.saved.task.roots == ["/home"])' >/dev/null
check $? "roots 落盘为单元素 [\"/home\"]"

echo "== 2. 一对一：多根被拒（且不落盘） =="
OUT="$(taskadd --id tmany --mountpoint "$RUNDIR/mmany" --root /home --root /Public --no-mount)"
RC=$?
check "$([ "$RC" -ne 0 ] && echo 0 || echo 1)" "多根登记被拒（rc=$RC）"
echo "$OUT" | grep -q "只能有一个 NAS 目录"
check $? "错误信息说明「只能有一个 NAS 目录」"
[ ! -f "$XDG_CONFIG_HOME/qxync/tasks/tmany.json" ]
check $? "被拒后没有落盘 tmany.json"

echo "== 3. 目的地冲突：本地文件夹相同 =="
OUT="$(taskadd --id t2 --mountpoint "$RUNDIR/m1" --root /Public --no-mount)"
RC=$?
check "$([ "$RC" -ne 0 ] && echo 0 || echo 1)" "同一个本地文件夹被拒（rc=$RC）"
echo "$OUT" | grep -q "目的地冲突"
check $? "错误里带「目的地冲突」"
echo "$OUT" | grep -q "t1"
check $? "错误里点名冲突的任务 id（t1）"

echo "== 4. 目的地冲突：本地文件夹嵌套 =="
OUT="$(taskadd --id t3 --mountpoint "$RUNDIR/m1/inner" --root /Public --no-mount)"
RC=$?
check "$([ "$RC" -ne 0 ] && echo 0 || echo 1)" "嵌套的本地文件夹被拒（rc=$RC）"
echo "$OUT" | grep -q "嵌套"
check $? "错误里说明「嵌套」"

echo "== 5. NAS 目录相同：只提示不拦 =="
OUT="$(taskadd --id t4 --mountpoint "$RUNDIR/m4" --root /home --no-mount)"
check $? "同 NAS 目录、不同本地文件夹 → 允许登记"
echo "$OUT" | jq -e '.saved.warnings | length >= 1' >/dev/null
check $? "响应里带 warnings"
echo "$OUT" | jq -r '.saved.warnings[0]' | grep -q "t1"
check $? "warnings 里点名 t1"

echo "== 6. 旧的多根任务文件：仍可读（不变成坏文件） =="
mkdir -p "$XDG_CONFIG_HOME/qxync/tasks"
cat > "$XDG_CONFIG_HOME/qxync/tasks/legacy.json" <<'JSON'
{"id":"legacy","name":"legacy","enabled":false,"mountpoint":"/tmp/qxync-legacy-mnt","roots":["/home","/Public"]}
JSON
OUT="$("$QS" --socket "$SOCK" task list --json 2>&1)"
echo "$OUT" | jq -e '.tasks[] | select(.task.id=="legacy") | .task.roots == ["/home","/Public"]' >/dev/null
check $? "legacy 多根任务仍能列出、roots 原样保留"
echo "$OUT" | jq -e '.bad_files | length == 0' >/dev/null
check $? "legacy 没有被算成 bad_files"

echo
echo "结果：通过 $PASS / 失败 $FAIL"
[ "$FAIL" = "0" ]
