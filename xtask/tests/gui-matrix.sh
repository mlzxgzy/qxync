#!/usr/bin/env bash
# gui-matrix.sh —— M4（GUI：Tauri 2 桌面应用）验收矩阵
#
# 用法:
#   xtask/tests/gui-matrix.sh                 # 全量：无窗口自检 + 5 个 tab 真实截图（~1min）
#   xtask/tests/gui-matrix.sh --no-window     # 只跑无窗口自检（无 DISPLAY / 无人值守机器）
#   xtask/tests/gui-matrix.sh --keep-open     # 结束时保留 GUI 窗口（手动看）
#
# 依赖:
#   * cargo build --workspace（target/debug/{qxync-gui,qxyncd,qsync}）
#   * 真机凭据：先 `qsync login`（本脚本只读 $XDG_CONFIG_HOME/qsync/ 下的 link + credentials）
#   * 窗口项额外需要：DISPLAY + xdotool + ImageMagick（import/identify/compare）
#
# 验的是什么:
#   1. `qxync-gui --self-test`：前端资源真的嵌进二进制了 + GUI 自己的 IPC 通道能拿到
#      daemon 状态、真实 NAS 的 ls、挂载表、同步状态（这四条就是 GUI 四个面板的数据源）；
#   2. daemon 不在时自检**必须失败**（负向对照，防止自检变成橡皮图章）；
#   3. 真窗口：5 个 tab 各起一次，窗口标题正确、截图非空白、tab 之间画面确实不同。
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GUI="$REPO/target/debug/qxync-gui"
DAEMON="$REPO/target/debug/qxyncd"
QS="$REPO/target/debug/qsync"
RUNDIR="${QSYNC_TEST_RUNDIR:-$REPO/.local-run}"
SOCK="$RUNDIR/gui-matrix.sock"
SHOTS="$RUNDIR/gui-shots"
LINK="${QSYNC_TEST_LINK:-default}"
LOG="$RUNDIR/gui-matrix.log"
TABS="status connect mounts files sync"

NO_WINDOW=0; KEEP_OPEN=0
for a in "$@"; do
  case "$a" in
    --no-window) NO_WINDOW=1 ;;
    --keep-open) KEEP_OPEN=1 ;;
    *) echo "未知参数: $a"; exit 2 ;;
  esac
done

export XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-$RUNDIR/config}"
export XDG_DATA_HOME="${XDG_DATA_HOME:-$RUNDIR/data}"
export XDG_STATE_HOME="${XDG_STATE_HOME:-$RUNDIR/state}"
export QSYNC_SOCKET="$SOCK"
export RUST_LOG="${RUST_LOG:-warn}"
DAEMON_PID=""; GUI_PID=""

PASS=0; FAIL=0
ok()   { echo "  ✅ $1"; PASS=$((PASS+1)); }
bad()  { echo "  ❌ $1"; FAIL=$((FAIL+1)); }
check(){ if [ "$1" = "0" ]; then ok "$2"; else bad "$2"; fi }
skip() { echo "  ⏭️  $1"; }

mkdir -p "$RUNDIR" "$SHOTS"

cleanup() {
  [ -n "$GUI_PID" ] && kill "$GUI_PID" 2>/dev/null
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null
  pkill -x qxync-gui 2>/dev/null
  # --self-test-login 里的 login_flow 会自己拉起 daemon（pid 不在脚本手里）→ 一并收掉。
  # 本脚本约定独占本机 qxyncd（第 1 步已经 pkill 过一次），所以这里是安全的。
  pkill -x qxyncd 2>/dev/null
  sleep 0.3
  rm -f "$SOCK"
  return 0
}
trap 'if [ "$KEEP_OPEN" != "1" ]; then cleanup; else [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null; fi' EXIT

# ---------------------------------------------------------------- 前置检查
echo "== 0. 前置 =="
miss=0
for b in "$GUI" "$DAEMON" "$QS"; do
  [ -x "$b" ] || { echo "  ❌ 缺少可执行文件 $b（先 CARGO_HOME=\$PWD/.cargo-home cargo build --workspace）"; miss=1; }
done
check "$miss" "三个二进制都在（qxync-gui / qxyncd / qsync）"
if [ ! -f "$XDG_CONFIG_HOME/qsync/links/$LINK.json" ]; then
  echo "  ❌ 没有连接配置 $XDG_CONFIG_HOME/qsync/links/$LINK.json —— 先跑一次："
  echo "     XDG_CONFIG_HOME=$XDG_CONFIG_HOME cargo run -p qxync-cli -- --host <NAS> --port 9834 --insecure --user <用户> --password '<口令>' login"
  exit 2
fi
ok "连接配置存在（$LINK.json）"
[ "$NO_WINDOW" = "1" ] && skip "窗口/截图项（--no-window）" || true
if [ "$NO_WINDOW" = "0" ] && { [ -z "${DISPLAY:-}" ] || ! command -v xdotool >/dev/null || ! command -v import >/dev/null; }; then
  echo "  ⚠️  没有 DISPLAY / xdotool / import → 自动跳过窗口与截图项"
  NO_WINDOW=1
fi
# 截图/窗口检查走 X11（XWayland）：xdotool/import 看不见原生 Wayland 窗口。
# 只影响本脚本；用户正常启动仍走 Wayland。
[ "$NO_WINDOW" = "0" ] && export GDK_BACKEND="${GDK_BACKEND:-x11}"

# ---------------------------------------------------------------- 起 daemon + 登录
echo "== 1. daemon + 登录 =="
pkill -x qxyncd 2>/dev/null; rm -f "$SOCK"; sleep 0.5
"$DAEMON" --link "$LINK" --socket "$SOCK" --foreground >"$LOG" 2>&1 &
DAEMON_PID=$!
ready=1
for _ in $(seq 1 60); do
  if [ -S "$SOCK" ] && "$QS" --socket "$SOCK" daemon status >/dev/null 2>&1; then ready=0; break; fi
  sleep 0.5
done
check "$ready" "qxyncd 起来了（socket $SOCK，pid $DAEMON_PID）"
if [ "$ready" != "0" ]; then echo "见 $LOG"; exit 1; fi

"$QS" --socket "$SOCK" --via-daemon login >/dev/null 2>&1
check $? "登录成功（真机 NAS，凭据来自 credentials.json）"

# ---------------------------------------------------------------- 2. 无窗口自检
echo "== 2. qxync-gui --self-test（无窗口） =="
ST="$RUNDIR/gui-self-test.json"
"$GUI" --self-test >"$ST" 2>"$RUNDIR/gui-self-test.err"
SELF_RC=$?
jqv() { jq -r "$1" "$ST" 2>/dev/null; }

check "$SELF_RC" "self-test 退出码 0（自检 ok）"
check "$([ "$(jqv .ui_assets_ok)" = "true" ] && echo 0 || echo 1)" "前端资源已嵌入（index_html $(jqv .ui_assets.index_html) B / app_js $(jqv .ui_assets.app_js) B / style_css $(jqv .ui_assets.style_css) B）"
check "$([ "$(jqv .daemon_running)" = "true" ] && echo 0 || echo 1)" "app_info 看到 daemon 在跑（socket $(jqv .socket)）"
check "$([ "$(jqv .status_ok)" = "true" ] && echo 0 || echo 1)" "status 快照解析成功（GUI 顶栏数据源）"
check "$([ "$(jqv .logged_in)" = "true" ] && echo 0 || echo 1)" "会话已登录"
check "$([ "$(jqv .ls.ok)" = "true" ] && [ "$(jqv .ls.total)" -gt 0 ] && echo 0 || echo 1)" "ipc_call ls /home 成功（$(jqv .ls.total) 项，文件面板数据源）"
check "$([ "$(jqv .mounts_ok)" = "true" ] && echo 0 || echo 1)" "ipc_call mounts 成功（$(jqv .mounts_count) 个挂载，挂载面板数据源）"
check "$([ "$(jqv .sync_ok)" = "true" ] && echo 0 || echo 1)" "ipc_call sync 成功（同步/缓存面板数据源）"
cp -f "$ST" "$SHOTS/self-test.json"

# 负向对照：daemon 停了必须以非 0 退出（否则自检没有意义）
kill "$DAEMON_PID" 2>/dev/null; DAEMON_PID=""; rm -f "$SOCK"; sleep 0.8
"$GUI" --self-test >/dev/null 2>&1
NEG=$?
[ "$NEG" != "0" ]; check $? "daemon 不在时 self-test 退出码非 0（负向对照，实际 $NEG）"

"$DAEMON" --link "$LINK" --socket "$SOCK" --foreground >>"$LOG" 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 60); do [ -S "$SOCK" ] && break; sleep 0.5; done
"$QS" --socket "$SOCK" --via-daemon login >/dev/null 2>&1

# ---------------------------------------------------------------- 2b. 「保存并登录」整条链
echo "== 2b. qxync-gui --self-test-login（GUI 自己的登录链） =="
LT="$RUNDIR/gui-self-test-login.json"
"$GUI" --self-test-login >"$LT" 2>"$RUNDIR/gui-self-test-login.err"
LRC=$?
ljqv() { jq -r "$1" "$LT" 2>/dev/null; }
check "$LRC" "self-test-login 退出码 0（停 daemon → login_flow → 复核）"
check "$([ "$(ljqv .stopped)" = "true" ] && echo 0 || echo 1)" "daemon_stop 生效"
check "$([ "$(ljqv .login_ok)" = "true" ] && echo 0 || echo 1)" "login_flow 成功（写 link/凭据 + 拉起 daemon + IPC 登录）"
check "$([ "$(ljqv .daemon_running)" = "true" ] && [ "$(ljqv .logged_in)" = "true" ] && echo 0 || echo 1)" "复核：daemon 在跑且已登录（user=$(ljqv .login_data.user) sid=$(ljqv .login_data.sid_masked)）"
cp -f "$LT" "$SHOTS/self-test-login.json"
# 后面窗口项要用 daemon；本步结束后 daemon 由 GUI 拉起（pid 不在 $DAEMON_PID 里）
DAEMON_PID=""

# ---------------------------------------------------------------- 3. 真窗口 + 逐 tab 截图
echo "== 3. 真窗口（5 个 tab） =="
if [ "$NO_WINDOW" = "1" ]; then
  skip "窗口检查与截图（--no-window / 无 DISPLAY）"
else
  first_shot=""
  for t in $TABS; do
    pkill -x qxync-gui 2>/dev/null; sleep 0.6
    QSYNC_GUI_TAB="$t" "$GUI" >"$RUNDIR/gui-window-$t.log" 2>&1 &
    GUI_PID=$!
    WID=""
    for _ in $(seq 1 40); do
      WID=$(xdotool search --name "QSync" 2>/dev/null | head -1)
      [ -n "$WID" ] && break
      sleep 0.5
    done
    if [ -z "$WID" ]; then bad "tab=$t：20s 内没找到 QSync 窗口"; kill "$GUI_PID" 2>/dev/null; GUI_PID=""; continue; fi
    TITLE=$(xdotool getwindowname "$WID" 2>/dev/null)
    case "$TITLE" in *QSync*) check 0 "tab=$t：窗口标题正确（$TITLE）";; *) check 1 "tab=$t：窗口标题异常（$TITLE）";; esac
    sleep 5   # 等首轮 status + 该 tab 自己的数据（files 要打真机 ls）
    SHOT="$SHOTS/tab-$t.png"
    import -window "$WID" "$SHOT" 2>/dev/null
    [ -s "$SHOT" ]; check $? "tab=$t：截图产出（$SHOT）"
    DIM=$(identify -format "%wx%h" "$SHOT" 2>/dev/null)
    check "$([ "$DIM" = "1200x800" ] && echo 0 || echo 1)" "tab=$t：窗口尺寸 $DIM"
    SD=$(identify -format "%[standard-deviation]" "$SHOT" 2>/dev/null | cut -d. -f1)
    check "$([ "${SD:-0}" -gt 1500 ] && echo 0 || echo 1)" "tab=$t：画面非空白（stddev $SD > 1500）"
    [ -n "$first_shot" ] && {
      AE=$(compare -metric AE -fuzz 2% "$first_shot" "$SHOT" null: 2>&1 | cut -d' ' -f1 | cut -d. -f1)
      check "$([ "${AE:-0}" -gt 4000 ] && echo 0 || echo 1)" "tab=$t：与 status 页画面不同（AE $AE px）"
    }
    first_shot="$SHOT"
    kill "$GUI_PID" 2>/dev/null; wait "$GUI_PID" 2>/dev/null; GUI_PID=""
  done
fi

# ---------------------------------------------------------------- 汇总
echo
echo "== 汇总 =="
echo "  通过 $PASS / 失败 $FAIL"
if [ "$FAIL" = "0" ]; then
  echo "  🎉 M4 GUI 验收矩阵全过"
else
  echo "  ❌ 有失败项，见上面输出；截图在 $SHOTS"
fi
exit $([ "$FAIL" = "0" ] && echo 0 || echo 1)
