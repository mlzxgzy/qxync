#!/usr/bin/env bash
# m84-matrix.sh —— M8.4（设置中心 + 代理 + 托盘/通知 + 自动释放空间 + 冲突策略 + 三态）验收矩阵
#
# 用法:
#   xtask/tests/m84-matrix.sh                 # 全量（真机凭据 + /dev/fuse + DISPLAY/dbus）
#   xtask/tests/m84-matrix.sh --no-fuse       # 跳过挂载相关项（冲突策略/三态/筛选器真挂载段）
#   xtask/tests/m84-matrix.sh --no-gui        # 跳过托盘/通知（无 DISPLAY / 无 dbus 的机器）
#   xtask/tests/m84-matrix.sh --no-proxy      # 跳过假代理观察段
#
# 验的是什么（对应 docs/M8-向Qsync-Client-6靠拢.md 的 M8.4 验收）:
#   ① 代理：三种模式各验一次 —— Auto-detect（环境变量）/ No proxy / Manual，
#      用 `nc -l` 当假代理，观察请求里的 `CONNECT`；Manual 在代理停掉后必须**报错**，
#      而 No proxy 必须仍能登录；
#   ② 托盘：真窗口起来后 D-Bus 上出现本进程的 `StatusNotifierItem-<pid>`；
#      关闭窗口 → 进程还活着（进了托盘）；通知：`dbus-monitor` 抓 org.freedesktop.Notifications；
#   ③ 自动释放空间：`QSYNC_TEST_FAKE_STATVFS` 把可用空间打到 5% → 触发脱水，
#      并且 **pin 住的文件被安全检查链挡下**（不许为自动释放绕过铁则）；
#   ④ 筛选器：加一条 `*.iso` → `qsync rules --match` 判定隐藏，且真挂载点里看不到；
#   ⑤ 冲突策略 5 个取值各跑一次三向冲突，产物符合预期（ask 走待裁决队列）；
#   ⑥ 节省空间模式三态：新建 → 仅在线；`head -c` → 本地可用；pin → 始终可用；脱水 → 仅在线，
#      且脱水后远端改同长度不同内容再 `cat` 必须拿到新内容（铁则 2 不退化）；
#   ⑦ 设置本体：settings.json 往返 / 0600 / autostart 桌面项写与删 / manual 缺服务器报错。
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DAEMON="$REPO/target/debug/qxyncd"
QS="$REPO/target/debug/qsync"
GUI="$REPO/target/debug/qxync-gui"
RUNDIR="${QSYNC_TEST_RUNDIR:-$REPO/.local-run}/m84"
SOCK="$RUNDIR/s.sock"
LOG="$RUNDIR/daemon.log"
MNT="$RUNDIR/mnt"
SUB="/home/qxync-test/m84"        # 远端绝对路径（daemon/CLI 用）
RELV="/qxync-test/m84"            # 挂载点里的相对路径（单根 /home 直通）
SUB_LOCAL="$RUNDIR/local"
PROXY_PORT="${QSYNC_TEST_PROXY_PORT:-18484}"

NO_FUSE=0; NO_GUI=0; NO_PROXY=0
for a in "$@"; do case "$a" in
  --no-fuse) NO_FUSE=1 ;;
  --no-gui) NO_GUI=1 ;;
  --no-proxy) NO_PROXY=1 ;;
  *) echo "未知参数: $a"; exit 2 ;;
esac; done

PASS=0; FAIL=0
ok()   { echo "  ✅ $1"; PASS=$((PASS+1)); }
bad()  { echo "  ❌ $1"; FAIL=$((FAIL+1)); }
check(){ if [ "$1" = "0" ]; then ok "$2"; else bad "$2"; fi; }
skip() { echo "  ⏭️  $1"; }

# 清残留挂载（上一次跑崩）
for d in "$MNT"; do
  [ -d "$d" ] || continue
  if awk -v mp="$d" '$2==mp{found=1} END{exit !found}' /proc/mounts; then
    echo "  ⚠️  清理残留挂载: $d"; fusermount3 -u "$d" 2>/dev/null || true
  fi
done
rm -rf "$RUNDIR"; mkdir -p "$RUNDIR/config/qsync/links" "$MNT" "$SUB_LOCAL"
SRC_XDG="${QSYNC_SRC_XDG_CONFIG:-$REPO/.local-run/config}"
LINK="${QSYNC_TEST_LINK:-default}"
if [ ! -f "$SRC_XDG/qsync/links/$LINK.json" ]; then
  echo "❌ 找不到连接配置：$SRC_XDG/qsync/links/$LINK.json"; exit 2
fi
cp "$SRC_XDG/qsync/links/$LINK.json" "$RUNDIR/config/qsync/links/"
[ -f "$SRC_XDG/qsync/credentials.json" ] && cp "$SRC_XDG/qsync/credentials.json" "$RUNDIR/config/qsync/"

export XDG_CONFIG_HOME="$RUNDIR/config" XDG_DATA_HOME="$RUNDIR/data" XDG_STATE_HOME="$RUNDIR/state"
export QSYNC_SOCKET="$SOCK" RUST_LOG="${RUST_LOG:-warn}"
DAEMON_PID=""; PROXY_PID=""; GUI_PID=""
SETTINGS="$XDG_CONFIG_HOME/qsync/settings.json"
AUTOSTART="$XDG_CONFIG_HOME/autostart/qsync.desktop"

[ "$NO_FUSE" = "0" ] && [ ! -e /dev/fuse ] && { echo "  ⚠️  没有 /dev/fuse → 跳过挂载项"; NO_FUSE=1; }
if [ "$NO_GUI" = "0" ] && { [ -z "${DISPLAY:-}" ] || ! command -v xdotool >/dev/null; }; then
  echo "  ⚠️  没有 DISPLAY / xdotool → 跳过托盘与通知"; NO_GUI=1
fi
[ "$NO_GUI" = "0" ] && export GDK_BACKEND="${GDK_BACKEND:-x11}"

Q() { "$QS" --socket "$SOCK" "$@"; }
qj() { Q "$@" --json 2>/dev/null; }
jqr() { jq -r "$1" 2>/dev/null; }
# NAS 测试目录里的文件名（`qsync ls` 每行最后是文件名，前面两空格分隔）
remote_names() { Q ls "$SUB" 2>/dev/null | tail -n +2 | sed 's/.*  //'; }
remote_body() { Q get "$SUB" "$1" --out "$RUNDIR/get-$1" >/dev/null 2>&1; cat "$RUNDIR/get-$1" 2>/dev/null; }
copies_for() { remote_names | grep -cF "$1 (conflicted copy" || true; }
copy_body_of() {
  local f
  f=$(remote_names | grep -F "$1 (conflicted copy" | head -1)
  [ -n "$f" ] && remote_body "$f"
}
purge_copies() {
  local f
  while IFS= read -r f; do
    [ -n "$f" ] && Q rm "$SUB" "$f" >/dev/null 2>&1
  done < <(remote_names | grep -F '(conflicted copy' || true)
}
purge_all() {
  local f
  while IFS= read -r f; do
    [ -n "$f" ] && Q rm "$SUB" "$f" >/dev/null 2>&1
  done < <(remote_names)
}

stop_daemon(){ [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null; pkill -x qxyncd 2>/dev/null; sleep 0.6; rm -f "$SOCK"; DAEMON_PID=""; }
start_daemon() { # $1 = 额外环境变量串（可空），$2 = 额外参数
  stop_daemon
  # shellcheck disable=SC2086
  env $1 "$DAEMON" --link "$LINK" --socket "$SOCK" --foreground $2 >>"$LOG" 2>&1 &
  DAEMON_PID=$!
  for _ in $(seq 1 60); do [ -S "$SOCK" ] && return 0; sleep 0.25; done
  return 1
}
login() { Q --via-daemon login >/dev/null 2>&1; }
cleanup() {
  [ -n "$GUI_PID" ] && kill "$GUI_PID" 2>/dev/null
  stop_daemon
  [ -n "$PROXY_PID" ] && kill "$PROXY_PID" 2>/dev/null
  pkill -f "nc -l 127.0.0.1 $PROXY_PORT" 2>/dev/null
  fusermount3 -u "$MNT" 2>/dev/null
  return 0
}
trap cleanup EXIT

miss=0
for b in "$DAEMON" "$QS"; do [ -x "$b" ] || { echo "  ❌ 缺少 $b（先 cargo build --workspace）"; miss=1; }; done
command -v jq >/dev/null || { echo "  ❌ 需要 jq"; miss=1; }
check "$miss" "二进制与 jq 就绪"

# 测试 NAS 目录（所有产物都在 $SUB 下，跑完清掉）
echo "== 0. 准备 NAS 测试目录 =="
start_daemon "" ""; check $? "daemon 起来（初始：没有 settings.json）"
login; check $? "登录成功"
Q mkdir /home/qxync-test m84 >/dev/null 2>&1
check "$([ "$(Q ls /home/qxync-test 2>/dev/null | grep -c 'm84')" -ge 1 ] && echo 0 || echo 1)" "NAS 上 $SUB 目录就绪"

# ---------------------------------------------------------------- 1. 设置本体
echo "== 1. settings.json（往返 / 0600 / 默认值）"
check "$([ ! -f "$SETTINGS" ] && echo 0 || echo 1)" "还没有 settings.json（默认值 = M8.3 行为）"
S0=$(qj settings)
check "$([ "$(echo "$S0" | jqr .settings.proxy.mode)" = "auto" ] && echo 0 || echo 1)" "默认代理 = auto（= reqwest 既有行为）"
check "$([ "$(echo "$S0" | jqr .settings.free_space.auto)" = "false" ] && echo 0 || echo 1)" "默认不自动释放空间"
check "$([ "$(echo "$S0" | jqr .autostart_present)" = "false" ] && echo 0 || echo 1)" "默认没有 autostart 桌面项"

# 写 --set（经 daemon）：非 manual 值 + 需要校验的 manual
Q settings --set free.auto=true --set free.below_pct=7 --set notifications=false >/dev/null 2>&1
check $? "qsync settings --set 写入成功（经 daemon）"
S1=$(qj settings)
check "$([ "$(echo "$S1" | jqr .settings.free_space.auto)" = "true" ] && echo 0 || echo 1)" "free.auto 落盘为 true"
check "$([ "$(echo "$S1" | jqr .settings.free_space.below_pct)" = "7" ] && echo 0 || echo 1)" "free.below_pct 落盘为 7"
check "$([ "$(echo "$S1" | jqr .settings.desktop_notifications)" = "false" ] && echo 0 || echo 1)" "桌面通知关掉了"
MODE=$(stat -c '%a' "$SETTINGS" 2>/dev/null)
check "$([ "$MODE" = "600" ] && echo 0 || echo 1)" "settings.json 权限 0600（实际 $MODE）"

# manual 缺服务器必须报错（不许静默退回直连）
Q settings --set proxy.mode=manual >/dev/null 2>&1
check "$([ $? -ne 0 ] && echo 0 || echo 1)" "★ manual 缺服务器 → daemon 拒绝保存（非 0 退出）"
S2=$(qj settings)
check "$([ "$(echo "$S2" | jqr .settings.proxy.mode)" = "auto" ] && echo 0 || echo 1)" "被拒绝后磁盘上的 mode 仍是 auto（没有半截写入）"

# 开机自启：写/删 autostart 桌面项
Q settings --set startup=true --autostart-exe "$RUNDIR/fake-qxync-gui" >/dev/null 2>&1
check "$([ -f "$AUTOSTART" ] && echo 0 || echo 1)" "开机自启 → 写了 $AUTOSTART"
check "$([ "$(grep -c "Exec=$RUNDIR/fake-qxync-gui" "$AUTOSTART" 2>/dev/null)" = "1" ] && echo 0 || echo 1)" "桌面项 Exec 指向传入的可执行文件"
check "$([ "$(qj settings | jqr .autostart_present)" = "true" ] && echo 0 || echo 1)" "settings 回报 autostart_present=true"
Q settings --set startup=false >/dev/null 2>&1
check "$([ ! -f "$AUTOSTART" ] && echo 0 || echo 1)" "关掉开机自启 → 桌面项被删掉"

# 未知键必须报错（不静默）
"$QS" settings --set nosuch.key=1 --direct >/dev/null 2>&1
check "$([ $? -ne 0 ] && echo 0 || echo 1)" "未知设置键 → 报错（不静默忽略）"

# ---------------------------------------------------------------- 2. 代理三模式
echo "== 2. 代理（Auto-detect / No proxy / Manual + 假代理观察 CONNECT）"
if [ "$NO_PROXY" = "1" ]; then
  skip "代理观察段（--no-proxy）"
else
  PROXY_LOG="$RUNDIR/proxy.log"
  start_proxy() {
    : > "$PROXY_LOG"
    ( while true; do nc -l 127.0.0.1 "$PROXY_PORT" >>"$PROXY_LOG" 2>&1; done ) &
    PROXY_PID=$!
    sleep 0.5
  }
  stop_proxy() { [ -n "$PROXY_PID" ] && kill "$PROXY_PID" 2>/dev/null; PROXY_PID=""; sleep 0.3; }
  proxy_saw_connect() { grep -qi '^CONNECT ' "$PROXY_LOG" 2>/dev/null; }

  # (a) Auto-detect：跟随 http_proxy 环境变量
  start_proxy
  Q settings --set proxy.mode=auto >/dev/null 2>&1
  start_daemon "http_proxy=http://127.0.0.1:$PROXY_PORT https_proxy=http://127.0.0.1:$PROXY_PORT" ""
  check $? "Auto-detect：daemon 带着 http_proxy 起来"
  Q --via-daemon login >/dev/null 2>&1 || true
  sleep 0.5
  check "$(proxy_saw_connect && echo 0 || echo 1)" "★ Auto-detect：假代理看到了 CONNECT（$(head -c 60 "$PROXY_LOG" | tr '\n' ' ')）"
  stop_proxy

  # (b) No proxy：即便环境变量在，也必须直连
  : > "$PROXY_LOG"
  Q settings --set proxy.mode=none >/dev/null 2>&1
  start_daemon "http_proxy=http://127.0.0.1:$PROXY_PORT https_proxy=http://127.0.0.1:$PROXY_PORT" ""
  login
  LRC=$?
  check "$LRC" "★ No proxy：环境变量还在，登录仍然成功（显式 no_proxy 生效）"
  check "$([ ! -s "$PROXY_LOG" ] && echo 0 || echo 1)" "★ No proxy：假代理一个字节都没收到（确实直连）"

  # (c) Manual：指定服务器；代理在 → 走代理（登录会失败，因为 nc 不是真代理）
  start_proxy
  Q settings --set proxy.mode=manual --set proxy.server=127.0.0.1 --set proxy.port="$PROXY_PORT" >/dev/null 2>&1
  start_daemon "" ""
  check $? "Manual：daemon 起来"
  Q --via-daemon login >/dev/null 2>&1 || true
  sleep 0.5
  check "$(proxy_saw_connect && echo 0 || echo 1)" "★ Manual：假代理看到了 CONNECT"
  stop_proxy
  # 代理停掉后 Manual 必须报错（不能悄悄直连）
  start_daemon "" ""
  Q --via-daemon login >/dev/null 2>&1
  MANUAL_RC=$?
  check "$([ $MANUAL_RC -ne 0 ] && echo 0 || echo 1)" "★ 代理停掉后 Manual 登录失败（退出码 $MANUAL_RC，不是静默直连）"
  # 切回 No proxy → 必须恢复可登录
  Q settings --set proxy.mode=none >/dev/null 2>&1
  start_daemon "" ""
  login
  check $? "★ 切回 No proxy 后登录恢复成功"
fi

# ---------------------------------------------------------------- 3. 托盘 / 通知（GUI）
echo "== 3. 托盘 + 桌面通知（GUI 侧）"
if [ "$NO_GUI" = "1" ] || [ ! -x "$GUI" ]; then
  skip "托盘/通知（--no-gui 或缺 $GUI）"
else
  start_daemon "" ""; login
  pkill -x qxync-gui 2>/dev/null; sleep 0.5
  # 矩阵全局 RUST_LOG=warn 会吞掉 info 级的「托盘可用」判据 → 这里单独放开 qxync_gui 模块
  RUST_LOG="qxync_gui=info,warn" "$GUI" >"$RUNDIR/gui-tray.log" 2>&1 &
  GUI_PID=$!
  WID=""
  for _ in $(seq 1 40); do
    WID=$(xdotool search --name "QSync" 2>/dev/null | head -1)
    [ -n "$WID" ] && break
    sleep 0.5
  done
  check "$([ -n "$WID" ] && echo 0 || echo 1)" "GUI 真窗口起来了（wid=$WID）"
  sleep 4
  ITEM=$(dbus-send --session --dest=org.freedesktop.DBus --type=method_call --print-reply \
    /org/freedesktop/DBus org.freedesktop.DBus.ListNames 2>/dev/null | grep -c "StatusNotifierItem-${GUI_PID}-")
  check "$([ "${ITEM:-0}" -ge 1 ] && echo 0 || echo 1)" "★ D-Bus 上出现本进程的 StatusNotifierItem（托盘图标注册成功）"
  # ★ M8.4：「注册成功」≠「有人画」。GUI 会探测 watcher 的 IsStatusNotifierHostRegistered
  #   并确认自己的 item 在 RegisteredStatusNotifierItems 里，才认定「托盘可见」。
  check "$([ "$(grep -c '托盘可用' "$RUNDIR/gui-tray.log" 2>/dev/null)" -ge 1 ] && echo 0 || echo 1)" \
    "★ 托盘**可见性**探测通过（$(grep -oE '托盘可用（[^）]*）' "$RUNDIR/gui-tray.log" | head -1)）"
  WHOST=$(dbus-send --session --print-reply --dest=org.kde.StatusNotifierWatcher /StatusNotifierWatcher \
    org.freedesktop.DBus.Properties.Get string:org.kde.StatusNotifierWatcher \
    string:IsStatusNotifierHostRegistered 2>/dev/null | grep -c "boolean true")
  check "$([ "${WHOST:-0}" -ge 1 ] && echo 0 || echo 1)" "watcher 自报 IsStatusNotifierHostRegistered=true（与上面的探测互相印证）"
  # 中文菜单项不是 ASCII，`strings` 默认抓不到 → 直接在二进制里 grep（-a）
  check "$([ "$(grep -ac '立即与 NAS 同步' "$GUI" 2>/dev/null)" -ge 1 ] && echo 0 || echo 1)" "托盘菜单 4 项文案编进二进制（打开主窗口/立即与 NAS 同步/暂停/退出）"

  # 关闭窗口 → 进托盘：窗口消失但进程还在
  #   ⚠ 必须用「WM 级别的关闭请求」（wmctrl -c → _NET_CLOSE_WINDOW）；
  #     `xdotool windowclose` 是直接 XDestroyWindow，tao 会当成窗口被销毁而退出事件循环。
  if [ -n "$WID" ]; then
    if command -v wmctrl >/dev/null; then wmctrl -c "QSync"; else xdotool windowclose "$WID"; fi
    sleep 2
    # ⚠ 「窗口还在不在」必须看**映射状态**：被 hide 的窗口在 X 里仍然存在，
    #    `xdotool search` 照样找得到 → 会误判。用 wmctrl -l（只列已映射）或 --onlyvisible。
    if command -v wmctrl >/dev/null; then
      VIS=$(wmctrl -l 2>/dev/null | grep -c "QSync" || true)
    else
      VIS=$(xdotool search --onlyvisible --name "QSync" 2>/dev/null | wc -l)
    fi
    ALIVE=1; kill -0 "$GUI_PID" 2>/dev/null && ALIVE=0
    check "$([ "${VIS:-1}" = "0" ] && echo 0 || echo 1)" "关闭窗口后窗口不可见（已隐藏，剩余可见 $VIS）"
    check "$ALIVE" "★ 关闭窗口后进程仍在（进了托盘，而不是退出）"
  fi
  kill "$GUI_PID" 2>/dev/null; GUI_PID=""

  # 负向对照：私有一根 D-Bus 会话（**没有** watcher）→ 托盘不可用 → 关窗必须真的退出。
  #   判据不是「日志好看」，而是进程真的没了（这正是「别把窗口藏起来找不回」的护栏）。
  if command -v dbus-run-session >/dev/null; then
    NEG_LOG="$RUNDIR/gui-notray.log"
    : > "$NEG_LOG"
    GUI="$GUI" NEG_LOG="$NEG_LOG" timeout 60 dbus-run-session -- bash -c '
      export GDK_BACKEND=x11
      "$GUI" >"$NEG_LOG" 2>&1 &
      GP=$!
      sleep 6
      wmctrl -c "QSync" 2>/dev/null
      sleep 2
      if kill -0 "$GP" 2>/dev/null; then kill "$GP" 2>/dev/null; echo ALIVE; else echo EXITED; fi
    ' > "$RUNDIR/gui-notray.result" 2>&1
    NEG_RES=$(grep -oE "ALIVE|EXITED" "$RUNDIR/gui-notray.result" 2>/dev/null | tail -1)
    check "$([ "$NEG_RES" = "EXITED" ] && echo 0 || echo 1)" \
      "★ 没有 watcher 的会话里：托盘不可用 → 关窗**真的退出**（$NEG_RES）"
    NEG_WHY=$(grep -oE '托盘不可见[^）]*|系统托盘创建失败[^:：]*' "$NEG_LOG" 2>/dev/null | head -1)
    check "$([ -n "$NEG_WHY" ] && echo 0 || echo 1)" "负向会话的日志如实说明托盘为何不可用（${NEG_WHY:-无}）"
  else
    skip "无 watcher 负向对照（没有 dbus-run-session）"
  fi

  # 通知：dbus-monitor 抓 org.freedesktop.Notifications
  #   第 1 节把「桌面通知」关过（验设置往返），这里先打开再验正路径
  Q settings --set notifications=true >/dev/null 2>&1
  check "$([ "$(qj settings | jqr .settings.desktop_notifications)" = "true" ] && echo 0 || echo 1)" "先打开桌面通知（准备验正路径）"
  NOTIF_LOG="$RUNDIR/notif.log"
  timeout 12 dbus-monitor --session "interface='org.freedesktop.Notifications'" >"$NOTIF_LOG" 2>&1 &
  MON_PID=$!
  sleep 1
  # ⚠ stdout 是 JSON、stderr 是 GTK/dbind 警告：**必须分开重定向**，
  #   否则 jq 会读到一行 "(qxync-gui:1234): dbind-WARNING ..." 而解析失败（踩过）。
  "$GUI" --self-test-notify >"$RUNDIR/gui-notify.json" 2>"$RUNDIR/gui-notify.err"
  NRC=$?
  sleep 2
  kill "$MON_PID" 2>/dev/null
  check "$NRC" "--self-test-notify 退出码 0（$(head -c 120 "$RUNDIR/gui-notify.json" | tr '\n' ' ')）"
  check "$([ "$(grep -c 'member=Notify' "$NOTIF_LOG" 2>/dev/null)" -ge 1 ] && echo 0 || echo 1)" "★ dbus 上抓到了 Notify 方法调用（桌面通知真的发出去了）"

  # 关掉「显示桌面通知」→ 必须**真的不发**（不是假装成功）
  Q settings --set notifications=false >/dev/null 2>&1
  NOTIF_LOG2="$RUNDIR/notif-off.log"
  timeout 10 dbus-monitor --session "interface='org.freedesktop.Notifications'" >"$NOTIF_LOG2" 2>&1 &
  MON2=$!
  sleep 1
  "$GUI" --self-test-notify >"$RUNDIR/gui-notify-off.json" 2>"$RUNDIR/gui-notify-off.err"
  sleep 2
  kill "$MON2" 2>/dev/null
  check "$([ "$(jq -r '.shown' "$RUNDIR/gui-notify-off.json" 2>/dev/null)" = "false" ] && echo 0 || echo 1)" "★ 关掉桌面通知后 shown=false（如实上报，不是假装发了）"
  check "$([ "$(grep -c 'member=Notify' "$NOTIF_LOG2" 2>/dev/null)" = "0" ] && echo 0 || echo 1)" "★ 关掉桌面通知后 dbus 上一个 Notify 都没有"
  Q settings --set notifications=true >/dev/null 2>&1

  # 无窗口自检要如实报告 M8.4 能力（托盘：代码在、但无窗口模式不建）
  "$GUI" --self-test >"$RUNDIR/gui-selftest-m84.json" 2>/dev/null || true
  check "$([ "$(jq -r '.m84.plugins|length' "$RUNDIR/gui-selftest-m84.json" 2>/dev/null)" = "3" ] && echo 0 || echo 1)" "无窗口自检报告 3 个插件（notification/dialog/opener）"
  check "$([ "$(jq -r '.m84.tray_code' "$RUNDIR/gui-selftest-m84.json" 2>/dev/null)" = "true" ] && echo 0 || echo 1)" "无窗口自检如实报告 tray_code=true / tray_created=$(jq -r '.m84.tray_created' "$RUNDIR/gui-selftest-m84.json" 2>/dev/null)（不虚报）"
fi

# ---------------------------------------------------------------- 4. 筛选器
echo "== 4. 筛选器设置（exclude *.iso）"
start_daemon "" ""; login
# 造两个远端文件：一个 .iso（要隐藏）、一个 .txt（要可见）
printf 'ISO-CONTENT' > "$SUB_LOCAL/hidden.iso"
printf 'TXT-CONTENT' > "$SUB_LOCAL/visible.txt"
Q put "$SUB_LOCAL/hidden.iso" "$SUB" --name hidden.iso >/dev/null 2>&1
Q put "$SUB_LOCAL/visible.txt" "$SUB" --name visible.txt >/dev/null 2>&1
check $? "远端造好 hidden.iso / visible.txt"
jq '.exclude = ["*.iso"]' "$XDG_CONFIG_HOME/qsync/links/$LINK.json" >"$RUNDIR/link.tmp" \
  && mv "$RUNDIR/link.tmp" "$XDG_CONFIG_HOME/qsync/links/$LINK.json"
start_daemon "" ""; login
M1=$(qj rules --match "$SUB/hidden.iso")
M2=$(qj rules --match "$SUB/visible.txt")
check "$([ "$(echo "$M1" | jqr .match_hidden)" = "true" ] && echo 0 || echo 1)" "rules --match *.iso → 隐藏（$(echo "$M1" | jqr .match_reason)）"
check "$([ "$(echo "$M2" | jqr .match_hidden)" = "false" ] && echo 0 || echo 1)" "rules --match .txt → 可见"
if [ "$NO_FUSE" = "1" ]; then
  skip "筛选器的真挂载不可见段（--no-fuse）"
else
  Q task add --id m84flt --mountpoint "$MNT" --root /home --no-mount >/dev/null 2>&1
  Q task mount m84flt >/dev/null 2>&1
  check $? "带筛选器的任务挂载成功"
  ls "$MNT/qxync-test/m84" >/dev/null 2>&1
  check "$([ "$(ls "$MNT$RELV" 2>/dev/null | grep -c 'hidden.iso')" = "0" ] && echo 0 || echo 1)" "★ 挂载点里看不到被排除的 hidden.iso"
  check "$([ "$(ls "$MNT$RELV" 2>/dev/null | grep -c 'visible.txt')" -ge 1 ] && echo 0 || echo 1)" "★ 挂载点里能看到 visible.txt"
  Q umount "$MNT" >/dev/null 2>&1; sleep 1
fi

# ---------------------------------------------------------------- 5. 自动释放空间
echo "== 5. 自动释放空间（QSYNC_TEST_FAKE_STATVFS=avail_pct=5）"
if [ "$NO_FUSE" = "1" ]; then
  skip "自动释放空间（需要真挂载，--no-fuse）"
else
  # 两个文件：一个普通（应被释放）、一个 pin 住（必须被挡下）
  head -c 262144 /dev/urandom > "$SUB_LOCAL/space-normal.bin"
  head -c 262144 /dev/urandom > "$SUB_LOCAL/space-pinned.bin"
  Q put "$SUB_LOCAL/space-normal.bin" "$SUB" --name space-normal.bin >/dev/null 2>&1
  Q put "$SUB_LOCAL/space-pinned.bin" "$SUB" --name space-pinned.bin >/dev/null 2>&1
  # 恢复无排除规则（上一步加了 *.iso）
  jq '.exclude = []' "$XDG_CONFIG_HOME/qsync/links/$LINK.json" >"$RUNDIR/link.tmp" \
    && mv "$RUNDIR/link.tmp" "$XDG_CONFIG_HOME/qsync/links/$LINK.json"

  # 第一步：**自动释放先关着**（settings 里 free.auto 此时为 false，见第 1 节末尾的复位），
  # 先把「水合 → 三态」这一段验干净，否则 1s 一轮的自动释放会抢在断言之前把文件脱水。
  start_daemon "QSYNC_TEST_FAKE_STATVFS=avail_pct=5 QSYNC_AUTO_FREE_INTERVAL=1 QSYNC_AUTO_FREE_RECENT=0" ""
  check $? "daemon 带注入点起来（avail_pct=5 / 每 1s 扫 / 最近访问保护=0）"
  login
  Q settings --set free.auto=false >/dev/null 2>&1
  Q task add --id m84free --mountpoint "$MNT" --root /home --read-write >/dev/null 2>&1
  check $? "读写任务挂载成功（$MNT）"
  # 水合两个文件（head -c 让 FUSE 抓 128 KiB 区间）
  ls "$MNT/qxync-test/m84" >/dev/null 2>&1
  head -c 100 "$MNT$RELV/space-normal.bin" >/dev/null 2>&1
  head -c 100 "$MNT$RELV/space-pinned.bin" >/dev/null 2>&1
  Q pin "$SUB/space-pinned.bin" pinned >/dev/null 2>&1
  FS1=$(qj file-states "$SUB")
  check "$([ "$(echo "$FS1" | jqr '.entries[]|select(.name=="space-normal.bin")|.state')" = "local" ] && echo 0 || echo 1)" "水合后 space-normal.bin = 本地可用"
  check "$([ "$(echo "$FS1" | jqr '.entries[]|select(.name=="space-pinned.bin")|.state')" = "always" ] && echo 0 || echo 1)" "pin 后 space-pinned.bin = 始终可用"
  # 第二步：打开自动释放（阈值 50%，注入的可用空间 5% → 必然触发）
  Q settings --set free.auto=true --set free.mode=below_pct --set free.below_pct=50 >/dev/null 2>&1
  check $? "打开自动释放（当空间少于 50%）"

  SP=$(qj space)
  check "$([ "$(echo "$SP" | jqr .injected)" = "true" ] && echo 0 || echo 1)" "space 如实报告「正在用注入值」"
  check "$([ "$(echo "$SP" | jqr .fs_avail_pct)" = "5" ] && echo 0 || echo 1)" "注入后的可用空间 = 5%"
  check "$([ "$(echo "$SP" | jqr .would_run)" = "true" ] && echo 0 || echo 1)" "判定会触发（$(echo "$SP" | jqr .reason)）"

  # 等后台自动释放把普通文件脱水（最多 40s）
  NSTATE=""
  for _ in $(seq 1 80); do
    NSTATE=$(qj file-states "$SUB" | jqr '.entries[]|select(.name=="space-normal.bin")|.state')
    [ "$NSTATE" = "online" ] && break
    sleep 0.5
  done
  check "$([ "$NSTATE" = "online" ] && echo 0 || echo 1)" "★ 低空间触发自动释放：space-normal.bin 回到「仅在线」（实际 $NSTATE）"
  PSTATE=$(qj file-states "$SUB" | jqr '.entries[]|select(.name=="space-pinned.bin")|.state')
  PBYTES=$(qj file-states "$SUB" | jqr '.entries[]|select(.name=="space-pinned.bin")|.hydrated_bytes')
  check "$([ "$PSTATE" = "always" ] && echo 0 || echo 1)" "★ pin 住的文件仍是「始终可用」（状态 $PSTATE）"
  check "$([ "${PBYTES:-0}" -gt 0 ] && echo 0 || echo 1)" "★ pin 住的文件本地内容没被清掉（$PBYTES 字节仍在）"
  check "$([ "$(qj journal --query 自动释放空间 | jqr '[.entries[]|select(.detail|test("自动释放空间"))]|length')" -ge 1 ] && echo 0 || echo 1)" "journal 里有「自动释放空间」记录"
  Q pin "$SUB/space-pinned.bin" unspecified >/dev/null 2>&1

  # 非法注入值必须报错（不许静默真量）
  stop_daemon
  start_daemon "QSYNC_TEST_FAKE_STATVFS=bad-spec" ""; login
  Q space --json >/dev/null 2>&1
  check "$([ $? -ne 0 ] && echo 0 || echo 1)" "非法注入值 → space 报错（不是静默回退真 statvfs）"
  Q umount "$MNT" >/dev/null 2>&1; sleep 1
fi

# ---------------------------------------------------------------- 6. 冲突策略五选
echo "== 6. 冲突策略 5 个取值（真挂载三向冲突）"
if [ "$NO_FUSE" = "1" ]; then
  skip "冲突策略（--no-fuse）"
else
  # 先关掉自动释放：本机可用空间约 43%，开着的话下一节里 50% 阈值会把测试文件脱水掉
  start_daemon "" ""; login
  Q settings --set free.auto=false >/dev/null 2>&1
  purge_all
  check "$([ "$(remote_names | wc -l)" = "0" ] && echo 0 || echo 1)" "冲突段开始前 $SUB 是空目录（避免旧副本污染计数；实际剩 $(remote_names | wc -l)）"

  # ★ 确定性三向冲突：用 QSYNC_TEST_UPLOAD_HOLD_MS 把上传 worker 停在「取下一个作业」之前。
  #   否则写路径是写穿的，本地改动几十毫秒就上传了，端到端根本撞不出冲突。
  stop_daemon
  start_daemon "QSYNC_TEST_UPLOAD_HOLD_MS=2500" ""
  check $? "daemon 带「上传保持 2.5s」注入点起来"
  login

  run_conflict() { # $1=策略 $2=文件名
    local pol="$1" name="$2" mnt="$MNT-$1"
    rm -rf "$mnt"; mkdir -p "$mnt"
    printf 'BASE-BASE' > "$SUB_LOCAL/base.txt"
    Q put "$SUB_LOCAL/base.txt" "$SUB" --name "$name" >/dev/null 2>&1
    Q task add --id "m84c-$pol" --mountpoint "$mnt" --root /home --read-write --conflict "$pol" --no-mount >/dev/null 2>&1
    Q task mount "m84c-$pol" >/dev/null 2>&1
    ls "$mnt$RELV" >/dev/null 2>&1
    Q sync --once >/dev/null 2>&1                                       # ① 建 baseline
    printf 'LOCAL-LOCAL' > "$mnt$RELV/$name"                            # ② 本地改（作业停在 pending）
    printf 'REMOTE-REMOTE' > "$SUB_LOCAL/remote.txt"
    Q put "$SUB_LOCAL/remote.txt" "$SUB" --name "$name" >/dev/null 2>&1  # ③ 远端改
    Q sync --once >/dev/null 2>&1                                       # ④ 三向冲突 → 按策略处理
  }
  teardown() { # $1=策略 $2=该策略的文件名：卸载 + 删登记 + 删远端文件 + 让引擎清掉 baseline
    Q umount "$MNT-$1" >/dev/null 2>&1
    Q task rm "m84c-$1" >/dev/null 2>&1
    Q rm "$SUB" "$2" >/dev/null 2>&1
    Q sync --once >/dev/null 2>&1
    sleep 0.5
    purge_copies
  }
  wait_copy() { # $1=原文件名前缀 $2=最多等多少轮（冲突副本上传要被 hold 延迟）
    local i
    for i in $(seq 1 "${2:-40}"); do
      [ "$(copies_for "$1")" -ge 1 ] && return 0
      sleep 0.5
    done
    return 1
  }
  wait_remote() { # $1=文件名 $2=期望内容 $3=最多等多少轮
    local i
    for i in $(seq 1 "${3:-40}"); do
      [ "$(remote_body "$1")" = "$2" ] && return 0
      sleep 0.5
    done
    return 1
  }

  # 6.1 rename_local（默认行为：远端占原名、本地另存副本）
  run_conflict rename_local c-renamelocal.txt
  wait_remote c-renamelocal.txt REMOTE-REMOTE 10
  check $? "rename_local：远端占原名 = 远端内容（$(remote_body c-renamelocal.txt)）"
  wait_copy c-renamelocal 40
  check $? "rename_local：产生了以该文件为前缀的冲突副本（$(copies_for c-renamelocal) 个）"
  check "$([ "$(copy_body_of c-renamelocal)" = "LOCAL-LOCAL" ] && echo 0 || echo 1)" "rename_local：冲突副本里是本地内容（$(copy_body_of c-renamelocal)）"
  check "$([ "$(cat "$MNT-rename_local$RELV/c-renamelocal.txt" 2>/dev/null)" = "REMOTE-REMOTE" ] && echo 0 || echo 1)" "rename_local：本地原名处显示远端内容"
  teardown rename_local c-renamelocal.txt

  # 6.2 replace_local（NAS 上的替换本地：本地改动丢，远端不动）
  run_conflict replace_local c-replacelocal.txt
  check "$([ "$(remote_body c-replacelocal.txt)" = "REMOTE-REMOTE" ] && echo 0 || echo 1)" "replace_local：远端内容是远端的（$(remote_body c-replacelocal.txt)）"
  check "$([ "$(cat "$MNT-replace_local$RELV/c-replacelocal.txt" 2>/dev/null)" = "REMOTE-REMOTE" ] && echo 0 || echo 1)" "replace_local：本地内容被远端替换"
  check "$([ "$(copies_for c-replacelocal)" = "0" ] && echo 0 || echo 1)" "replace_local：不产生冲突副本"
  teardown replace_local c-replacelocal.txt

  # 6.3 replace_remote（用本地替换 NAS 上：远端改动丢）
  run_conflict replace_remote c-replaceremote.txt
  wait_remote c-replaceremote.txt LOCAL-LOCAL 40
  check $? "replace_remote：远端被本地内容覆盖（$(remote_body c-replaceremote.txt)）"
  check "$([ "$(copies_for c-replaceremote)" = "0" ] && echo 0 || echo 1)" "replace_remote：不产生冲突副本"
  teardown replace_remote c-replaceremote.txt

  # 6.4 rename_remote（NAS 上那份改名，本地内容占原名）
  run_conflict rename_remote c-renameremote.txt
  wait_remote c-renameremote.txt LOCAL-LOCAL 40
  check $? "rename_remote：原名处是本地内容（$(remote_body c-renameremote.txt)）"
  wait_copy c-renameremote 20
  check $? "rename_remote：NAS 上那份改名成了冲突副本（$(copies_for c-renameremote) 个）"
  check "$([ "$(copy_body_of c-renameremote)" = "REMOTE-REMOTE" ] && echo 0 || echo 1)" "rename_remote：冲突副本里是远端内容（$(copy_body_of c-renameremote)）"
  teardown rename_remote c-renameremote.txt

  # 6.5 ask（每个文件都问我 → 待裁决队列 → 裁决后执行）
  run_conflict ask c-ask.txt
  D1=$(qj conflicts)
  PEND=$(echo "$D1" | jqr '.pending')
  check "$([ "${PEND:-0}" -ge 1 ] && echo 0 || echo 1)" "★ ask：冲突进了待裁决队列（pending=$PEND，路径：$(echo "$D1" | jqr '[.decisions[].path]|join(",")')）"
  check "$([ "$(remote_body c-ask.txt)" = "REMOTE-REMOTE" ] && echo 0 || echo 1)" "ask：未裁决时远端保持原样（没有偷偷覆盖）"
  QLOCAL=$(cat "$MNT-ask$RELV/c-ask.txt" 2>/dev/null)
  check "$([ "$QLOCAL" = "LOCAL-LOCAL" ] && echo 0 || echo 1)" "ask：未裁决时本地仍是**用户自己的内容**（$QLOCAL，两边都没丢）"
  DID=$(echo "$D1" | jqr '.decisions[0].id')
  Q conflicts --resolve "$DID" --as keep_local >/dev/null 2>&1
  check $? "ask：裁决 keep_local"
  Q sync --once >/dev/null 2>&1
  wait_remote c-ask.txt LOCAL-LOCAL 40
  check $? "★ ask：裁决后远端变成保留的一方（$(remote_body c-ask.txt)）"
  check "$([ "$(qj conflicts | jqr '.pending')" = "0" ] && echo 0 || echo 1)" "ask：执行完出队（pending=0）"
  teardown ask c-ask.txt

  # 队列隔离：clear 只清队列（不碰 journal / pin）
  Q conflicts --clear >/dev/null 2>&1
  check $? "conflicts --clear 可用"
  check "$([ "$(qj conflicts | jqr '.decisions|length')" = "0" ] && echo 0 || echo 1)" "clear 之后队列为空"

  # 收尾：关掉上传保持注入点，恢复默认
  stop_daemon
fi

# ---------------------------------------------------------------- 7. 三态 + 铁则 2
echo "== 7. 节省空间模式三态 + 铁则 2（脱水后拿新内容）"
if [ "$NO_FUSE" = "1" ]; then
  skip "三态（--no-fuse）"
else
  start_daemon "" ""; login
  MNT3="$RUNDIR/mnt3"; rm -rf "$MNT3"; mkdir -p "$MNT3"
  printf 'AAAA-1111' > "$SUB_LOCAL/tri.txt"
  Q put "$SUB_LOCAL/tri.txt" "$SUB" --name tri.txt >/dev/null 2>&1
  Q task add --id m84tri --mountpoint "$MNT3" --root /home --read-write --no-mount >/dev/null 2>&1
  Q task mount m84tri 2>&1 | tail -1
  check $? "三态测试挂载成功"
  ls "$MNT3/qxync-test/m84" >/dev/null 2>&1
  sleep 0.5
  S=$(qj file-states "$SUB" | jqr '.entries[]|select(.name=="tri.txt")|.state')
  check "$([ "$S" = "online" ] && echo 0 || echo 1)" "★ 新建（未读）→ 仅在线（$S）"
  head -c 4 "$MNT3$RELV/tri.txt" >/dev/null 2>&1
  sleep 0.3
  S=$(qj file-states "$SUB" | jqr '.entries[]|select(.name=="tri.txt")|.state')
  check "$([ "$S" = "local" ] && echo 0 || echo 1)" "★ head -c 之后 → 本地可用（$S）"
  Q pin "$SUB/tri.txt" pinned >/dev/null 2>&1
  S=$(qj file-states "$SUB" | jqr '.entries[]|select(.name=="tri.txt")|.state')
  check "$([ "$S" = "always" ] && echo 0 || echo 1)" "★ pin 之后 → 始终可用（$S）"
  Q pin "$SUB/tri.txt" unspecified >/dev/null 2>&1
  # --force = 跳过「刚访问过 300s」保护窗口（手动脱水的既有开关，测试必须用）
  Q dehydrate --path "$SUB/tri.txt" --force >/dev/null 2>&1
  sleep 0.5
  S=$(qj file-states "$SUB" | jqr '.entries[]|select(.name=="tri.txt")|.state')
  HB=$(qj file-states "$SUB" | jqr '.entries[]|select(.name=="tri.txt")|.hydrated_bytes')
  check "$([ "$S" = "online" ] && echo 0 || echo 1)" "★ 释放空间 → 回到仅在线（$S，本地 $HB 字节）"
  # 铁则 2：脱水后把远端改成**同长度不同内容**再 cat，必须拿到新内容
  printf 'BBBB-2222' > "$SUB_LOCAL/tri2.txt"
  Q put "$SUB_LOCAL/tri2.txt" "$SUB" --name tri.txt >/dev/null 2>&1
  Q sync --once >/dev/null 2>&1
  sleep 0.5
  GOT=$(head -c 9 "$MNT3$RELV/tri.txt" 2>/dev/null)
  check "$([ "$GOT" = "BBBB-2222" ] && echo 0 || echo 1)" "★ 铁则 2：脱水后远端同长度改内容 → cat 拿到新内容（$GOT）"
  Q umount "$MNT3" >/dev/null 2>&1; Q task rm m84tri >/dev/null 2>&1
fi

# ---------------------------------------------------------------- 8. 收尾清理
echo "== 8. 清理 NAS 测试产物 =="
start_daemon "" ""; login
purge_all
LEFT=$(remote_names | wc -l)
check "$([ "${LEFT:-0}" = "0" ] && echo 0 || echo 1)" "NAS 上的 M8.4 测试文件已清掉（剩 $LEFT 个）"
Q rm /home/qxync-test m84 >/dev/null 2>&1 || true

# ---------------------------------------------------------------- 汇总
echo
echo "== 汇总 =="
echo "  通过 $PASS / 失败 $FAIL"
if [ "$FAIL" = "0" ]; then echo "  🎉 M8.4 验收矩阵全过"; else echo "  ❌ 有失败项（日志 $LOG）"; fi
exit $([ "$FAIL" = "0" ] && echo 0 || echo 1)
