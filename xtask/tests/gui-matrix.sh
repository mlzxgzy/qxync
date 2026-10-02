#!/usr/bin/env bash
# gui-matrix.sh —— M4 + M8.1/M8.4 + M8.6（GUI：Tauri 2 桌面应用 + Qsync 风格外壳）验收矩阵
#
# 用法:
#   xtask/tests/gui-matrix.sh                 # 全量：无窗口自检 + 9 个目的地真实截图（~2min）
#   xtask/tests/gui-matrix.sh --no-window     # 只跑无窗口自检（无 DISPLAY / 无人值守机器）
#   xtask/tests/gui-matrix.sh --keep-open     # 结束时保留 GUI 窗口（手动看）
#
# 依赖:
#   * cargo build --workspace（target/debug/{qxync-gui,qxyncd,qxync}）
#   * 真机凭据：先 `qxync login`（本脚本只读 $XDG_CONFIG_HOME/qxync/ 下的 link + credentials）
#   * 窗口项额外需要：DISPLAY + xdotool + ImageMagick（import/identify/compare）
#
# 验的是什么:
#   1. `qxync-gui --self-test`：前端资源真的嵌进二进制了 + GUI 自己的 IPC 通道能拿到
#      daemon 状态、真实 NAS 的 ls、挂载表、同步状态（这四条就是 GUI 的数据源）；
#   2. daemon 不在时自检**必须失败**（负向对照，防止自检变成橡皮图章）；
#   3. ★ M8.1：**9 个目的地**各起一次真窗口 —— 左侧图标栏的 6 个一级页面
#      （home/tasks/files/journal/errors/settings）+ 诊断页的 3 个子页
#      （`diag:status` / `diag:mounts` / `diag:sync`），逐个断言窗口标题/尺寸/非空白/与主页不同；
#   4. ★ M8.4：设置页的 **8 个分区**（连接/代理/同步与筛选/个人/高级/释放空间/LAN/关于）
#      也能用 `settings:<分区>` 直达并逐个出图（`QXNYC_GUI_TAB=settings:proxy`）；
#   5. ★ M8.1：**旧 `QXNYC_GUI_TAB` 取值必须继续可用且落点不变** ——
#      status/mounts/sync/connect/files 五个旧值各起一次，用截图 AE 证明它们
#      落在与对应的新目的地**完全相同**的页面上（AE 很小 = 同页，落错页会极大）；
#   6. ★ M8.6：**UI 静态合规性**（`ui_spec`，随 `--self-test` 一起产出，不用开窗口）：
#      文案表键齐全（T() 与 data-i18n 用到的 key 都在 zh-CN 表里、en 预留）、
#      无 innerHTML 类拼接、键盘焦点环 / skip-link / tablist / dialog / aria-busy /
#      深色 token / reduced-motion 齐全、空错加载四态统一入口 + data-state 标记；
#      另加一条**运行时**断言：把 locale 切到预留的 en（空表）必须整条回落到 zh-CN；
#   7. ★ M8.6：**真窗口键盘可达性** —— 窗口起来后发一次 Tab，画面必须有变化
#      （第一个落点是「跳到主内容」skip-link，焦点环同时显形）。
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GUI="$REPO/target/debug/qxync-gui"
DAEMON="$REPO/target/debug/qxyncd"
QS="$REPO/target/debug/qxync"
RUNDIR="${QXNYC_TEST_RUNDIR:-$REPO/.local-run}"
SOCK="$RUNDIR/gui-matrix.sock"
SHOTS="$RUNDIR/gui-shots"
LINK="${QXNYC_TEST_LINK:-default}"
LOG="$RUNDIR/gui-matrix.log"
TABS="home tasks files journal errors settings diag:status diag:mounts diag:sync"
# ★ M8.4：设置页各分区（每个分区一张图，判据同其它目的地：非空白 + 与主页不同）
SECS="settings:proxy settings:sync settings:personal settings:advanced settings:free settings:lan settings:about"

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
export QXNYC_SOCKET="$SOCK"
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
check "$miss" "三个二进制都在（qxync-gui / qxyncd / qxync）"
if [ ! -f "$XDG_CONFIG_HOME/qxync/links/$LINK.json" ]; then
  echo "  ❌ 没有连接配置 $XDG_CONFIG_HOME/qxync/links/$LINK.json —— 先跑一次："
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
# ★ M8.4：设置 / 释放空间 / 冲突队列三条新数据源也要能拿到（GUI 的那几个页面全靠它们）
ST4=$("$QS" --socket "$SOCK" settings --json 2>/dev/null)
check "$([ "$(echo "$ST4" | jq -r '.settings.proxy.mode' 2>/dev/null)" != "null" ] && echo 0 || echo 1)" "M8.4 数据源：settings（代理模式 $(echo "$ST4" | jq -r '.settings.proxy.mode' 2>/dev/null)）"
SP4=$("$QS" --socket "$SOCK" space --json 2>/dev/null)
check "$([ "$(echo "$SP4" | jq -r '.fs_avail_pct' 2>/dev/null)" != "null" ] && echo 0 || echo 1)" "M8.4 数据源：space（可用 $(echo "$SP4" | jq -r '.fs_avail_pct' 2>/dev/null)%）"
DC4=$("$QS" --socket "$SOCK" file-states /home --json 2>/dev/null)
check "$([ "$(echo "$DC4" | jq -r '.path' 2>/dev/null)" = "/home" ] && echo 0 || echo 1)" "M8.4 数据源：file_states（三态：仅在线 $(echo "$DC4" | jq -r '.online' 2>/dev/null) / 本地可用 $(echo "$DC4" | jq -r '.local' 2>/dev/null) / 始终可用 $(echo "$DC4" | jq -r '.always' 2>/dev/null)）"
CF4=$("$QS" --socket "$SOCK" conflicts --json 2>/dev/null)
check "$([ "$(echo "$CF4" | jq -r '.pending' 2>/dev/null)" != "null" ] && echo 0 || echo 1)" "M8.4 数据源：conflicts（待裁决 $(echo "$CF4" | jq -r '.pending' 2>/dev/null)）"
cp -f "$ST" "$SHOTS/self-test.json"

# ---------------------------------------------------------------- 2c. ★ M8.6 UI 静态合规性
# 判据来源：`ui_spec` —— Rust 侧扫 include_str! 进来的**这一份** UI 资源（不是 grep 工作区），
# 所以这里绿 = 「打进二进制的那份界面」合规。见 crates/qxync-gui/src/lib.rs::ui_spec。
echo "== 2c. M8.6 UI 静态合规性（ui_spec） =="
jqs() { jq -r "$1" "$ST" 2>/dev/null; }
check "$([ "$(jqs .ui_spec_ok)" = "true" ] && echo 0 || echo 1)" "ui_spec.ok（UI 静态合规性总闸）"
check "$([ "$(jqs .ui_assets.i18n_js)" -gt 256 ] && echo 0 || echo 1)" "文案表已嵌入（i18n.js $(jqs .ui_assets.i18n_js) B）"
check "$([ "$(jqs .ui_spec.i18n.ok)" = "true" ] && echo 0 || echo 1)" \
  "文案表：zh-CN $(jqs .ui_spec.i18n.zh_keys) 条 / en 预留 $(jqs .ui_spec.i18n.en_keys) 条 / T() 用 $(jqs .ui_spec.i18n.used_keys) 条 / DOM 挂 $(jqs .ui_spec.i18n.dom_keys) 条；缺词 T=$(jqs '.ui_spec.i18n.missing_used|length') DOM=$(jqs '.ui_spec.i18n.missing_dom|length')"
check "$([ "$(jqs '.ui_spec.html_sinks|length')" = "0" ] && echo 0 || echo 1)" \
  "无 HTML 拼接（innerHTML/outerHTML/insertAdjacentHTML/document.write）"
for kv in focus_visible hidden_rule skip_link aria_current tablist dialog aria_busy reduced_motion dark_tokens; do
  check "$([ "$(jqs ".ui_spec.a11y.$kv")" = "true" ] && echo 0 || echo 1)" "无障碍/视觉规范：$kv"
done
check "$([ "$(jqs .ui_spec.states.helper)" = "true" ] && [ "$(jqs .ui_spec.states.markers)" -ge 8 ] && [ "$(jqs .ui_spec.states.screens)" -ge 8 ] && echo 0 || echo 1)" \
  "空/错/加载四态：setListState $(jqs .ui_spec.states.screens) 处调用，页面 data-state 标记 $(jqs .ui_spec.states.markers) 个"

# 2c-2. ★ M8.6：文案表的**运行时**回落语义（ui_spec 只能查键齐不齐，查不了行为）
#   判据：zh-CN 取到中文；切到预留的 en（空表）必须整条回落到 zh-CN（不是空白、不是 key）；
#   未知 key 返回 key 本身（便于在界面上看出漏配）。node 不在就如实跳过。
if command -v node >/dev/null; then
  I18N_OUT=$(node -e '
    const fs = require("fs");
    global.window = global;
    eval(fs.readFileSync(process.argv[1], "utf8"));
    const I = global.QXNYC_I18N;
    const zh = I.t("nav.home");
    I.setLocale("en");
    const en = I.t("nav.home");
    const missing = I.t("no.such.key.at.all");
    I.setLocale("zh-CN");
    process.stdout.write(JSON.stringify({ zh: zh, en: en, missing: missing }));
  ' "$REPO/crates/qxync-gui/ui/i18n.js" 2>/dev/null)
  check "$([ "$(echo "$I18N_OUT" | jq -r '.zh' 2>/dev/null)" = "主页" ] && [ "$(echo "$I18N_OUT" | jq -r '.en' 2>/dev/null)" = "主页" ] && [ "$(echo "$I18N_OUT" | jq -r '.missing' 2>/dev/null)" = "no.such.key.at.all" ] && echo 0 || echo 1)" \
    "i18n 回落：en（预留空表）→ zh-CN（zh=$(echo "$I18N_OUT" | jq -r '.zh' 2>/dev/null) / en=$(echo "$I18N_OUT" | jq -r '.en' 2>/dev/null)）；未知 key 原样返回"
else
  skip "i18n 回落语义（没有 node，跳过）"
fi

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

# ---------------------------------------------------------------- 3. 真窗口 + 逐目的地截图
# 目的地取值：一级页面（home/tasks/files/journal/errors/settings）+ `diag:<子页>`。
# 旧的 status/mounts/sync/connect/files 在 3b 单独验「仍然可用且落点不变」。
echo "== 3. 真窗口（$(echo $TABS | wc -w) 个目的地） =="

# 起一次 GUI、等窗口、截图、做尺寸/非空白断言。$1=QXNYC_GUI_TAB 值 $2=输出 png $3=日志/文件名 slug
shot_page() {
  local t="$1" out="$2" slug="$3" wid=""
  pkill -x qxync-gui 2>/dev/null; sleep 0.6
  QXNYC_GUI_TAB="$t" "$GUI" >"$RUNDIR/gui-window-$slug.log" 2>&1 &
  GUI_PID=$!
  for _ in $(seq 1 40); do
    wid=$(xdotool search --name "qxync" 2>/dev/null | head -1)
    [ -n "$wid" ] && break
    sleep 0.5
  done
  if [ -z "$wid" ]; then
    bad "page=$t：20s 内没找到 qxync 窗口"
    kill "$GUI_PID" 2>/dev/null; GUI_PID=""
    return 1
  fi
  local title; title=$(xdotool getwindowname "$wid" 2>/dev/null)
  case "$title" in *qxync*) ok "page=$t：窗口标题正确（$title）";; *) bad "page=$t：窗口标题异常（$title）";; esac
  sleep 5   # 等首轮 status + 该页自己的数据（files 要打真机 ls）
  import -window "$wid" "$out" 2>/dev/null
  [ -s "$out" ]; check $? "page=$t：截图产出（$(basename "$out")）"
  local dim; dim=$(identify -format "%wx%h" "$out" 2>/dev/null)
  check "$([ "$dim" = "1200x800" ] && echo 0 || echo 1)" "page=$t：窗口尺寸 $dim"
  local sd; sd=$(identify -format "%[standard-deviation]" "$out" 2>/dev/null | cut -d. -f1)
  check "$([ "${sd:-0}" -gt 1500 ] && echo 0 || echo 1)" "page=$t：画面非空白（stddev $sd > 1500）"
  kill "$GUI_PID" 2>/dev/null; wait "$GUI_PID" 2>/dev/null; GUI_PID=""
  return 0
}

ae_of() {  # $1 $2 → 输出 AE 像素数
  compare -metric AE -fuzz 2% "$1" "$2" null: 2>&1 | cut -d' ' -f1 | cut -d. -f1
}

if [ "$NO_WINDOW" = "1" ]; then
  skip "窗口检查与截图（--no-window / 无 DISPLAY）"
else
  first_shot=""
  for t in $TABS; do
    slug=$(echo "$t" | tr ':' '-')
    SHOT="$SHOTS/tab-$slug.png"
    shot_page "$t" "$SHOT" "$slug" || continue
    [ -n "$first_shot" ] && {
      AE=$(ae_of "$first_shot" "$SHOT")
      check "$([ "${AE:-0}" -gt 4000 ] && echo 0 || echo 1)" "page=$t：与主页画面不同（AE $AE px）"
    }
    first_shot="$SHOT"
  done

  # 3a. ★ M8.4：设置页的每个分区各出一张图（同一目的地内的不同分区必须**互不相同**）
  echo "== 3a. M8.4 设置分区截图（$(echo $SECS | wc -w) 个） =="
  prev_sec=""
  for t in $SECS; do
    slug=$(echo "$t" | tr ':' '-')
    SHOT="$SHOTS/tab-$slug.png"
    shot_page "$t" "$SHOT" "$slug" || continue
    AE=$(ae_of "$SHOTS/tab-settings.png" "$SHOT")
    check "$([ "${AE:-0}" -gt 4000 ] && echo 0 || echo 1)" "sec=$t：与「连接」分区画面不同（AE $AE px）"
    if [ -n "$prev_sec" ]; then
      AE2=$(ae_of "$prev_sec" "$SHOT")
      check "$([ "${AE2:-0}" -gt 4000 ] && echo 0 || echo 1)" "sec=$t：与上一个分区也不同（AE $AE2 px）"
    fi
    prev_sec="$SHOT"
  done

  # 3c. ★ M8.6：键盘可达性 —— Tab 一次，skip-link/焦点环必须显形（画面有变化）
  echo "== 3c. M8.6 键盘可达性（真窗口 Tab） =="
  pkill -x qxync-gui 2>/dev/null; sleep 0.6
  QXNYC_GUI_TAB=home "$GUI" >"$RUNDIR/gui-window-kbd.log" 2>&1 &
  GUI_PID=$!
  KWID=""
  for _ in $(seq 1 40); do
    KWID=$(xdotool search --name "qxync" 2>/dev/null | head -1)
    [ -n "$KWID" ] && break
    sleep 0.5
  done
  if [ -z "$KWID" ]; then
    bad "kbd：20s 内没找到 qxync 窗口"
  else
    sleep 5
    import -window "$KWID" "$SHOTS/kbd-before.png" 2>/dev/null
    check "$([ -s "$SHOTS/kbd-before.png" ] && echo 0 || echo 1)" "kbd：Tab 前截图产出"
    # 把窗口激活再发键（XTEST 只送到有输入焦点的窗口）；拿不到输入焦点就如实跳过，不假装通过
    if xdotool windowactivate --sync "$KWID" 2>/dev/null; then
      sleep 0.5
      xdotool key --clearmodifiers Tab 2>/dev/null
      sleep 1
      import -window "$KWID" "$SHOTS/kbd-after.png" 2>/dev/null
      KAE=$(ae_of "$SHOTS/kbd-before.png" "$SHOTS/kbd-after.png")
      check "$([ "${KAE:-0}" -gt 100 ] && echo 0 || echo 1)" \
        "kbd：Tab 后 skip-link/焦点环显形（AE $KAE px > 100）"
    else
      skip "kbd：拿不到窗口输入焦点（无 WM / Wayland 转发）→ 只保留 Tab 前截图"
    fi
    kill "$GUI_PID" 2>/dev/null; wait "$GUI_PID" 2>/dev/null; GUI_PID=""
  fi

  # 3b. ★ M8.1：旧的 QXNYC_GUI_TAB 取值必须继续可用，而且落到**同一个目的地**
  #     判据 = 截图 AE 很小（同一目的地）而不是极大（落错页）。
  echo "== 3b. 旧 QXNYC_GUI_TAB 值兼容（落点等价性） =="
  for pair in "status:diag-status" "mounts:diag-mounts" "sync:diag-sync" "connect:settings" "files:files"; do
    old="${pair%%:*}"; want="${pair##*:}"
    ref="$SHOTS/tab-$want.png"
    cur="$SHOTS/legacy-$old.png"
    if [ ! -s "$ref" ]; then bad "legacy=$old：缺少参照截图 tab-$want.png"; continue; fi
    shot_page "$old" "$cur" "legacy-$old" || continue
    AE=$(ae_of "$ref" "$cur")
    check "$([ "${AE:-999999}" -lt 30000 ] && echo 0 || echo 1)" \
      "legacy=$old → 落在 $want（AE $AE < 30000）"
  done
fi

# ---------------------------------------------------------------- 汇总
echo
echo "== 汇总 =="
echo "  通过 $PASS / 失败 $FAIL"
if [ "$FAIL" = "0" ]; then
  echo "  🎉 GUI 验收矩阵全过（M4 自检/登录链 + M8.1 目的地 + M8.4 设置分区 + M8.6 打磨收口）"
else
  echo "  ❌ 有失败项，见上面输出；截图在 $SHOTS"
fi
exit $([ "$FAIL" = "0" ] && echo 0 || echo 1)
