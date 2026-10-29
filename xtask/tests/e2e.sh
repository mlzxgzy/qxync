#!/usr/bin/env bash
# e2e.sh —— 端到端验收入口：**合并「跑法」，不合并断言**
#
# 以前每个里程碑一个脚本（19 个），发版前要一个个人肉跑、顺序也靠记；这套入口把它们
# 登记成一张表，按「档位」取子集、能并行的并行、已被大矩阵覆盖的不重复跑，最后出一张
# 汇总表。**脚本本体一行没少**：断言、夹具、真机链路都还在原文件里（谁改哪块就看那个
# 文件），这里只负责「什么时候跑谁、怎么跑、跑完怎么报」。
#
# 用法:
#   xtask/tests/e2e.sh                     # 默认 gate 档 = 单测 + 隔离 e2e（发版必跑，~30s）
#   xtask/tests/e2e.sh --tier fast         # 只跑隔离 e2e（CI 用；不含 cargo test）
#   xtask/tests/e2e.sh --tier gui          # GUI 矩阵（chromium / 真窗口 / 真机凭据）
#   xtask/tests/e2e.sh --tier fuse         # 真 FUSE 挂载矩阵（需要 /dev/fuse）
#   xtask/tests/e2e.sh --tier nas          # 真机 NAS 矩阵（读 .local-run/config 里的测试凭据）
#   xtask/tests/e2e.sh --tier all          # 全部能跑的都跑（按能力自动 SKIP 缺依赖的）
#   xtask/tests/e2e.sh --only m42-search,gui-purge   # 点名跑
#   xtask/tests/e2e.sh --list              # 列出所有套件（档位 / 依赖 / 超时 / 说明）
#
# 选项:
#   --tier a[,b]     档位，可重复：gate(默认) | unit | fast | gui | fuse | nas | all
#   --only a[,b]     只跑这些套件（忽略档位）
#   --skip a[,b]     从选择里去掉这些套件
#   --jobs N         并行度（只作用于 fast 档；默认 4）
#   --timeout S      覆盖单套件超时（秒）
#   --fail-fast      第一个失败就停（默认跑完全部，出完整画面）
#   --allow-ambient-stop  允许测试期间停掉你桌面上的 daemon（默认视为事故，退出码 3）
#   --dry-run        只打印将要跑什么
#   --list
#
# 环境隔离约定（本入口只做「保护」，不替套件做隔离）：
#   * 不导出任何 QXNYC_* 连接覆盖；只有 real-nas 套件会用 QXNYC_REAL_*（你自己给）。
#   * 真机类套件统一吃 `$REPO/.local-run/config` 里的**测试连接**（不是你的 ~/.config），
#     所以不会拿你的主账号去写 NAS；那个目录没配好就自动 SKIP。
#   * 跑完会核对「你的 daemon 还在不在」：任何一个套件把它带走了 → 退出码 3 并指名凶手
#     （历史上 gui-matrix 的 `pkill -x qxyncd` 就这么干过，M45 修掉）。
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TESTS="$REPO/xtask/tests"
QS="$REPO/target/debug/qxync"
QD="$REPO/target/debug/qxyncd"
QG="$REPO/target/debug/qxync-gui"
TEST_CFG="${QXNYC_SRC_CONFIG:-$REPO/.local-run/config}"
TEST_LINK="${QXNYC_TEST_LINK:-default}"

# ------------------------------------------------------------------ 套件注册表
# name|tier|needs|timeout|说明
#   tier : unit(单测/格式) | fast(隔离 e2e，CI 同级) | gui | fuse | nas
#   needs: 逗号分隔的能力名（缺任何一个 → SKIP，并说明缺什么）
#          fuse / chromium / display / node / python3 / sqlite3 / jq / creds / realnas / fixture
# 新增套件：加一行 + 在 suite_exec 里加一个分支即可，别再往 CI 里堆一堆 step。
SUITES=(
  "cargo-test|unit||900|cargo test --workspace --locked（660 项单测 + 集成测试）"
  "cargo-fmt|unit||120|cargo fmt --all --check（格式闸门）"
  "m19-purge|fast|python3|120|M19 删除相关数据（任务/连接/全局，隔离 XDG，不碰 NAS）"
  "m20-offline|fast|python3|180|M20 离线缓冲区 / 离线状态（隔离 XDG + 不可达 NAS）"
  "m21-offline-restart|fast|fuse,python3|180|M21 离线可重启 daemon（隔离 XDG + 真 FUSE）"
  "m40-multipath|fast|python3,jq|240|M40 多路径 / DDNS 选路（Python 假 NAS + 隔离 XDG）"
  "m42-search|fast|python3,sqlite3|120|M42 本地文件搜索 qxync find（只读状态库，不联网）"
  "pair-1to1|fast|jq|120|配对文件夹 = 一对一（私有 daemon + 假 link，不碰 NAS）"
  "node-task-defaults|fast|node|60|M18 任务默认路径纯函数单测（源文本级）"
  "node-m33-dl-toast|fast|node|60|M33 下载进度卡片纯函数单测（源文本级 + 假时钟）"
  "node-m28-tray-status|fast|node|60|M28 托盘状态聚合纯函数单测（源文本级）"
  "gui-task-defaults|gui|chromium,node|180|M18 任务默认路径无头集成（真 app.js + 打桩后端）"
  "gui-purge|gui|chromium,node|180|M19 删除相关数据无头集成（真 app.js + 打桩后端）"
  "m33-dl-toast-gui|gui|chromium,node|240|M33 下载进度卡片无头浏览器集成（真 app.js）"
  "gui-matrix|gui|creds|900|M4/M8.1/M8.4/M8.6 GUI 矩阵（自检 + 9 目的地真窗口截图）"
  "fuse-matrix|fuse|fuse,creds,fixture|1800|M1 FUSE 只读 + on-demand（真挂载；--big 需手动）"
  "m82-matrix|nas|creds|1200|M8.2 同步任务模型 + 持久化（真机 + 可选真挂载）"
  "m83-matrix|nas|creds|1200|M8.3 journal / 文件更新中心 / 错误列表（真机 + 可选真挂载）"
  "m84-matrix|nas|creds,python3,jq|1500|M8.4 设置中心 / 代理 / 托盘 / 冲突 / 三态（真机）"
  "m5-matrix|nas|creds,jq|1200|M5 SQLite 元数据 + librsync delta（真机，可 --no-nas）"
  "m7-matrix|nas|creds|1500|M7 选择性同步 + 设备配对 / LAN 直连（真机 + 可选真挂载）"
  "m20-real-nas|nas|fuse,realnas,python3|900|M20 离线可用 / 缓冲区的真机版（会写 QXNYC_REAL_ROOT）"
  "m21-real-nas|nas|fuse,realnas,python3|1200|M21 离线可重启的真机版（真挂载 + 真 NAS）"
  "m40-real-nas|nas|realnas,python3|600|M40 多路径 / DDNS 真机只读探测（不写 NAS）"
)

TIER_GATE="unit fast"
ALL_TIERS="unit fast gui fuse nas"
STAGE_ORDER="unit fast gui fuse nas"

# 大矩阵内部已经跑过的套件：跑到它时不再重复跑（只在同一轮选择里生效）
suite_covers() {
  case "$1" in
    gui-matrix) echo "node-task-defaults gui-task-defaults gui-purge m19-purge" ;;
    m84-matrix) echo "node-m28-tray-status" ;;
    *) echo "" ;;
  esac
}

# ------------------------------------------------------------------ 能力探测
CAP_FUSE=0; [ -e /dev/fuse ] && CAP_FUSE=1
CAP_NODE=0; command -v node >/dev/null 2>&1 && CAP_NODE=1
CAP_PY=0; command -v python3 >/dev/null 2>&1 && CAP_PY=1
CAP_SQLITE=0; command -v sqlite3 >/dev/null 2>&1 && CAP_SQLITE=1
CAP_JQ=0; command -v jq >/dev/null 2>&1 && CAP_JQ=1
CAP_CHROMIUM=0; command -v chromium >/dev/null 2>&1 && CAP_CHROMIUM=1
CAP_DISPLAY=0; { [ -n "${DISPLAY:-}" ] || [ -n "${WAYLAND_DISPLAY:-}" ]; } && CAP_DISPLAY=1
CAP_CREDS=0; [ -f "$TEST_CFG/qxync/links/$TEST_LINK.json" ] && CAP_CREDS=1
CAP_REALNAS=0
{ [ -n "${QXNYC_REAL_HOST:-}" ] && [ -n "${QXNYC_REAL_USER:-}" ] && [ -n "${QXNYC_REAL_PASSWORD:-}" ]; } && CAP_REALNAS=1
CAP_FIXTURE=0; [ -d "$REPO/xtask/probe/probe-out/fixture/local" ] && CAP_FIXTURE=1

cap_ok() {
  case "$1" in
    fuse) [ "$CAP_FUSE" = 1 ] ;;
    node) [ "$CAP_NODE" = 1 ] ;;
    python3) [ "$CAP_PY" = 1 ] ;;
    sqlite3) [ "$CAP_SQLITE" = 1 ] ;;
    jq) [ "$CAP_JQ" = 1 ] ;;
    chromium) [ "$CAP_CHROMIUM" = 1 ] ;;
    display) [ "$CAP_DISPLAY" = 1 ] ;;
    creds) [ "$CAP_CREDS" = 1 ] ;;
    realnas) [ "$CAP_REALNAS" = 1 ] ;;
    fixture) [ "$CAP_FIXTURE" = 1 ] ;;
    *) return 0 ;;
  esac
}

cap_hint() {
  case "$1" in
    fuse) echo "/dev/fuse" ;;
    node) echo "node" ;;
    python3) echo "python3" ;;
    sqlite3) echo "sqlite3" ;;
    jq) echo "jq" ;;
    chromium) echo "chromium" ;;
    display) echo "DISPLAY/WAYLAND_DISPLAY" ;;
    creds) echo "测试凭据 $TEST_CFG/qxync/links/$TEST_LINK.json（见 docs/测试环境.local.md）" ;;
    realnas) echo "QXNYC_REAL_HOST/USER/PASSWORD" ;;
    fixture) echo "本地夹具 $REPO/xtask/probe/probe-out/fixture/local" ;;
    *) echo "$1" ;;
  esac
}

# ------------------------------------------------------------------ 套件命令
suite_exec() {
  case "$1" in
    cargo-test) cargo test --workspace --locked ;;
    cargo-fmt) cargo fmt --all --check ;;
    m19-purge) bash "$TESTS/m19-purge.sh" ;;
    m20-offline) bash "$TESTS/m20-offline.sh" ;;
    m21-offline-restart) bash "$TESTS/m21-offline-restart.sh" ;;
    m40-multipath) bash "$TESTS/m40-multipath.sh" ;;
    m42-search) bash "$TESTS/m42-search.sh" ;;
    pair-1to1) bash "$TESTS/pair-1to1.sh" ;;
    node-task-defaults) node "$TESTS/task-defaults.test.mjs" ;;
    node-m33-dl-toast) node "$TESTS/m33-dl-toast.test.mjs" ;;
    node-m28-tray-status) node "$TESTS/m28-tray-status.test.mjs" ;;
    gui-task-defaults) bash "$TESTS/gui-task-defaults.sh" ;;
    gui-purge) bash "$TESTS/gui-purge.sh" ;;
    m33-dl-toast-gui) bash "$TESTS/m33-dl-toast-gui.sh" ;;
    gui-matrix)
      if [ "$CAP_DISPLAY" = 1 ] && command -v xdotool >/dev/null 2>&1 && command -v import >/dev/null 2>&1; then
        bash "$TESTS/gui-matrix.sh"
      else
        echo "（没有 DISPLAY / xdotool / import → 只跑无窗口自检）"
        bash "$TESTS/gui-matrix.sh" --no-window
      fi ;;
    fuse-matrix) bash "$TESTS/fuse-matrix.sh" ;;
    m82-matrix) if [ "$CAP_FUSE" = 1 ]; then bash "$TESTS/m82-matrix.sh"; else bash "$TESTS/m82-matrix.sh" --no-fuse; fi ;;
    m83-matrix) if [ "$CAP_FUSE" = 1 ]; then bash "$TESTS/m83-matrix.sh"; else bash "$TESTS/m83-matrix.sh" --no-fuse; fi ;;
    m84-matrix)
      local a=()
      [ "$CAP_FUSE" = 1 ] || a+=(--no-fuse)
      if [ "$CAP_DISPLAY" != 1 ] || ! command -v xdotool >/dev/null 2>&1; then a+=(--no-gui); fi
      bash "$TESTS/m84-matrix.sh" "${a[@]}" ;;
    m5-matrix) if [ "$CAP_CREDS" = 1 ]; then bash "$TESTS/m5-matrix.sh"; else bash "$TESTS/m5-matrix.sh" --no-nas; fi ;;
    m7-matrix) if [ "$CAP_CREDS" = 1 ]; then bash "$TESTS/m7-matrix.sh"; else bash "$TESTS/m7-matrix.sh" --no-nas; fi ;;
    m20-real-nas) bash "$TESTS/m20-real-nas.sh" ;;
    m21-real-nas) bash "$TESTS/m21-real-nas.sh" ;;
    m40-real-nas) bash "$TESTS/m40-real-nas.sh" ;;
    *) echo "❌ 未注册的套件: $1" >&2; return 2 ;;
  esac
}

# 套件私有的环境（真机类统一吃测试配置目录，绝不碰 ~/.config）
suite_env() {
  local name="$1"
  # 连接覆盖类环境一律清掉：测试绝不能因为 shell 里恰好有 QXNYC_HOST/SOCKET 就跑偏
  unset QXNYC_SOCKET QXNYC_HOST QXNYC_PORT QXNYC_USER QXNYC_PASSWORD QXNYC_OTP_SECRET QXNYC_OTP_CODE 2>/dev/null || true
  case "$name" in
    cargo-test|cargo-fmt|node-*) return 0 ;;
    m20-real-nas|m21-real-nas|m40-real-nas) return 0 ;;   # 真机套件自己造隔离配置
    *) export XDG_CONFIG_HOME="$TEST_CFG" ;;              # 其余统一吃测试连接（不是 ~/.config）
  esac
  return 0
}

# 套件在自己进程组里跑（见 run_one 的 setsid），所以这两个函数和它们用到的变量要导出。
export REPO TESTS QS QD QG TEST_CFG TEST_LINK
export CAP_FUSE CAP_NODE CAP_PY CAP_SQLITE CAP_JQ CAP_CHROMIUM CAP_DISPLAY CAP_CREDS CAP_REALNAS CAP_FIXTURE
export -f suite_env suite_exec

# ------------------------------------------------------------------ 参数
TIERS_ARG=(); ONLY_ARG=""; SKIP_ARG=""; JOBS=4; TIMEOUT_OVERRIDE=""
FAIL_FAST=0; ALLOW_AMBIENT_STOP=0; DRY_RUN=0; LIST_ONLY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --tier) TIERS_ARG+=("${2:-}"); shift 2 ;;
    --tier=*) TIERS_ARG+=("${1#*=}"); shift ;;
    --only) ONLY_ARG="${2:-}"; shift 2 ;;
    --only=*) ONLY_ARG="${1#*=}"; shift ;;
    --skip) SKIP_ARG="${2:-}"; shift 2 ;;
    --skip=*) SKIP_ARG="${1#*=}"; shift ;;
    --jobs) JOBS="${2:-4}"; shift 2 ;;
    --jobs=*) JOBS="${1#*=}"; shift ;;
    --timeout) TIMEOUT_OVERRIDE="${2:-}"; shift 2 ;;
    --timeout=*) TIMEOUT_OVERRIDE="${1#*=}"; shift ;;
    --fail-fast) FAIL_FAST=1; shift ;;
    --allow-ambient-stop) ALLOW_AMBIENT_STOP=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    --list) LIST_ONLY=1; shift ;;
    -h|--help) sed -n '2,45p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "未知参数: $1（--help 看用法）" >&2; exit 2 ;;
  esac
done
case "$JOBS" in ''|*[!0-9]*) echo "--jobs 需要正整数" >&2; exit 2 ;; esac
[ "$JOBS" -ge 1 ] || { echo "--jobs 需要 >= 1" >&2; exit 2; }

# ------------------------------------------------------------------ 列表
if [ "$LIST_ONLY" = "1" ]; then
  printf '%-22s %-6s %-5s %-30s %s\n' 套件 档位 超时 依赖 说明
  for row in "${SUITES[@]}"; do
    IFS='|' read -r name tier needs tmo desc <<<"$row"
    printf '%-22s %-6s %-5s %-30s %s\n' "$name" "$tier" "${tmo}s" "${needs:-—}" "$desc"
  done
  echo
  echo "档位: gate=($TIER_GATE)  all=${ALL_TIERS// /,}"
  exit 0
fi

# ------------------------------------------------------------------ 选择套件
declare -A TIER_SET=() CHOSEN=()
if [ -n "$ONLY_ARG" ]; then
  IFS=',' read -r -a _only <<<"$ONLY_ARG"
  for n in "${_only[@]}"; do CHOSEN["$n"]=1; done
else
  [ "${#TIERS_ARG[@]}" -eq 0 ] && TIERS_ARG=("gate")
  IFS=',' read -r -a _flat <<<"$(printf '%s,' "${TIERS_ARG[@]}")"
  for t in "${_flat[@]}"; do
    [ -z "$t" ] && continue
    case "$t" in
      gate) for x in $TIER_GATE; do TIER_SET["$x"]=1; done ;;
      all) for x in $ALL_TIERS; do TIER_SET["$x"]=1; done ;;
      unit|fast|gui|fuse|nas) TIER_SET["$t"]=1 ;;
      *) echo "未知档位: $t（可选 gate|unit|fast|gui|fuse|nas|all）" >&2; exit 2 ;;
    esac
  done
  for row in "${SUITES[@]}"; do
    IFS='|' read -r name tier _ _ _ <<<"$row"
    [ -n "${TIER_SET[$tier]:-}" ] && CHOSEN["$name"]=1
  done
fi
if [ -n "${ONLY_ARG}${SKIP_ARG}" ]; then
  for n in "${!CHOSEN[@]}"; do
    if [ -n "$SKIP_ARG" ]; then
      case ",$SKIP_ARG," in *",$n,"*) unset 'CHOSEN[$n]' ;; esac
    fi
  done
  if [ -n "$ONLY_ARG" ]; then
    : # --only 已经精确选择
  fi
fi
if [ "${#CHOSEN[@]}" -eq 0 ]; then echo "没有选中任何套件（--list 看名字）" >&2; exit 2; fi
# 名字必须都在注册表里（拼错时别悄悄什么都不跑）
declare -A KNOWN=()
for row in "${SUITES[@]}"; do IFS='|' read -r _k _ _ _ _ <<<"$row"; KNOWN["$_k"]=1; done
for n in "${!CHOSEN[@]}"; do
  [ -n "${KNOWN[$n]:-}" ] || { echo "未知套件: $n（--list 看名字）" >&2; exit 2; }
done

# 去重：大矩阵已覆盖的不重复跑
declare -A COVERED=()
for n in "${!CHOSEN[@]}"; do
  for c in $(suite_covers "$n"); do
    [ -n "${CHOSEN[$c]:-}" ] && COVERED["$c"]="$n"
  done
done
for c in "${!COVERED[@]}"; do
  unset 'CHOSEN[$c]'
done

# ------------------------------------------------------------------ 准备
# 上一次跑到一半被中断留下的套件进程（孤儿）：会继续改状态、甚至执行 daemon stop
# 把用户桌面守护带走（实测事故根因）。开跑前按「argv[1]=bash，argv[2]=本目录下 *.sh」精确清掉，
# 不去 pkill 任何按名字模糊匹配的进程。
for _d in /proc/[0-9]*; do
  _p="${_d#/proc/}"
  [ "$_p" = "$$" ] && continue
  [ -r "$_d/cmdline" ] || continue
  mapfile -d '' -t _av <"$_d/cmdline" 2>/dev/null || continue
  [ "${#_av[@]}" -ge 2 ] || continue
  case "${_av[0]}" in *bash|*/bash) ;; *) continue ;; esac
  case "${_av[1]}" in "$TESTS"/*.sh) ;; *) continue ;; esac
  case "${_av[1]}" in *e2e.sh) continue ;; esac
  echo "⚠️  清理上次残留的套件进程 pid=$_p：${_av[*]}"
  kill -TERM "$_p" 2>/dev/null
done
unset _d _p _av

RUN_DIR="$REPO/.local-run/e2e/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$RUN_DIR/logs" "$RUN_DIR/status"
AMBIENT_DAEMON_BEFORE="$(pgrep -x qxyncd 2>/dev/null | sort | tr '\n' ' ')"
AMBIENT_GUI_BEFORE="$(pgrep -x qxync-gui 2>/dev/null | sort | tr '\n' ' ')"
START_TS=$(date +%s)

# 二进制不在就先构建（快档全都用 target/debug）
NEED_BIN=0
for n in "${!CHOSEN[@]}"; do case "$n" in cargo-test|cargo-fmt|node-*) ;; *) NEED_BIN=1 ;; esac; done
if [ "$NEED_BIN" = "1" ] && [ ! -x "$QS" ]; then
  echo "== 准备：target/debug 二进制缺失，先 cargo build --workspace --locked =="
  ( cd "$REPO" && cargo build --workspace --locked ) || { echo "❌ 构建失败" >&2; exit 1; }
fi

echo "== e2e 入口 =="
echo "  仓库    : $REPO"
echo "  记录    : $RUN_DIR"
echo "  选中    : $(for n in "${!CHOSEN[@]}"; do echo -n "$n "; done)"
[ "${#COVERED[@]}" -gt 0 ] && echo "  去重    : $(for c in "${!COVERED[@]}"; do echo -n "$c(→${COVERED[$c]}) "; done)"
echo "  环境    : fuse=$CAP_FUSE chromium=$CAP_CHROMIUM display=$CAP_DISPLAY 测试凭据=$CAP_CREDS realnas=$CAP_REALNAS"
[ -n "$AMBIENT_DAEMON_BEFORE" ] && echo "  保护    : 检测到你桌面上的 qxyncd（pid $AMBIENT_DAEMON_BEFORE），本入口不会停它；若哪个套件把它带走会报出来"
echo

# ------------------------------------------------------------------ 执行
run_one() {  # $1=name  → 结果写入 $RUN_DIR/status/<name>
  local name="$1" row tier needs tmo desc log rc t0 t1
  row="$(for r in "${SUITES[@]}"; do case "$r" in "$name|"*) echo "$r"; break ;; esac; done)"
  IFS='|' read -r _ tier needs tmo desc <<<"$row"
  [ -n "$TIMEOUT_OVERRIDE" ] && tmo="$TIMEOUT_OVERRIDE"
  log="$RUN_DIR/logs/$name.log"

  # 能力不满足 → SKIP
  local miss=""
  if [ -n "$needs" ]; then
    IFS=',' read -r -a _needs <<<"$needs"
    for cap in "${_needs[@]}"; do
      cap_ok "$cap" || miss+="$(cap_hint "$cap") "
    done
  fi
  if [ -n "$miss" ]; then
    printf 'SKIP\t0\t（缺 %s）\n' "$miss" >"$RUN_DIR/status/$name"
    echo "SKIP $name —— 缺 $miss"
    return 0
  fi
  if [ "$DRY_RUN" = "1" ]; then
    echo "DRY  $name   （$tier，超时 ${tmo}s）"
    return 0
  fi

  t0=$(date +%s)
  # 套件跑在自己的进程组里（setsid）：超时要杀的是**整棵树**，不是只有那层子 shell。
  # 教训（本次实测）：`kill $pid` 只杀子 shell，`bash fuse-matrix.sh` 会变成孤儿继续往下跑，
  # 十几分钟后自己执行到 `daemon stop` —— 用旧脚本的默认 socket 把用户桌面守护停掉。
  # setsid 之后 `kill -- -$pid` 一次带走套件 + 它派生的 CLI / daemon / fusermount。
  setsid bash -c 'suite_env "$1"; cd "$2" && suite_exec "$1"' _ "$name" "$REPO" >"$log" 2>&1 &
  local pid=$!
  local waited=0
  kill_tree() {  # 递归杀子孙再杀自己（兜底 setsid 之前派生的进程）
    local p="$1" c
    for c in $(pgrep -P "$p" 2>/dev/null); do kill_tree "$c"; done
    kill -TERM "$p" 2>/dev/null
  }
  while kill -0 "$pid" 2>/dev/null; do
    if [ "$waited" -ge "$tmo" ]; then
      kill -- -"$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null
      kill_tree "$pid"
      sleep 1
      kill -- -"$pid" 2>/dev/null; kill -KILL "$pid" 2>/dev/null
      wait "$pid" 2>/dev/null
      # 兜底：按脚本路径再扫一遍（防止 daemon 自己 setsid 逃出进程组）
      pkill -f "$TESTS/$name.sh" 2>/dev/null
      t1=$(date +%s)
      printf 'TIMEOUT\t%s\t超时 %ss > %ss\n' "$((t1-t0))" "$((t1-t0))" "$tmo" >"$RUN_DIR/status/$name"
      echo "FAIL $name —— 超时（${tmo}s），已杀整棵进程树，日志 $log"
      return 1
    fi
    sleep 1; waited=$((waited+1))
  done
  wait "$pid"; rc=$?
  t1=$(date +%s)

  # 结果判定：退出码优先；0 且日志里只有 SKIP 的记为 SKIP
  local verdict="PASS"
  if [ "$rc" != "0" ]; then
    verdict="FAIL"
  elif ! grep -qE '^(PASS |  ✅|ok )' "$log" 2>/dev/null && grep -qE '^SKIP' "$log" 2>/dev/null; then
    verdict="SKIP"
  fi
  local np=0 nf=0
  np=$(grep -cE '^(PASS |  ✅)' "$log" 2>/dev/null || true)
  nf=$(grep -cE '^(FAIL |  ❌)' "$log" 2>/dev/null || true)
  local detail=""
  [ "${np:-0}" -gt 0 ] || [ "${nf:-0}" -gt 0 ] && detail="通过 ${np} / 失败 ${nf}"
  [ "$verdict" = "SKIP" ] && detail="$(grep -m1 '^SKIP' "$log" 2>/dev/null | sed 's/^SKIP：\?//')"
  printf '%s\t%s\t%s\n' "$verdict" "$((t1-t0))" "$detail" >"$RUN_DIR/status/$name"
  if [ "$verdict" = "PASS" ]; then
    echo "PASS $name  ${detail}（$((t1-t0))s）"
  elif [ "$verdict" = "SKIP" ]; then
    echo "SKIP $name —— ${detail}"
  else
    echo "FAIL $name（exit $rc，$((t1-t0))s，日志 $log）"
  fi
  [ "$verdict" = "FAIL" ] && return 1
  return 0
}

run_parallel() {  # $@=names，按批并行（每批 <= JOBS）
  local -a names=("$@") pids=()
  local i=0 j p rc_total=0
  while [ "$i" -lt "${#names[@]}" ]; do
    pids=()
    for (( j=0; j<JOBS && i<${#names[@]}; j++,i++ )); do
      run_one "${names[$i]}" &
      pids+=($!)
    done
    for p in "${pids[@]}"; do
      wait "$p" || rc_total=1
    done
  done
  return "$rc_total"
}

FAILED=(); PASSED=(); SKIPPED=()
for stage in $STAGE_ORDER; do
  STAGE_NAMES=()
  for row in "${SUITES[@]}"; do
    IFS='|' read -r name tier _ _ _ <<<"$row"
    [ "$tier" = "$stage" ] || continue
    [ -n "${CHOSEN[$name]:-}" ] || continue
    STAGE_NAMES+=("$name")
  done
  [ "${#STAGE_NAMES[@]}" -eq 0 ] && continue
  echo "== 阶段 $stage（${#STAGE_NAMES[@]} 个套件$( [ "$stage" = fast ] && echo "，并行 $JOBS" )) =="
  rc=0
  if [ "$stage" = "fast" ] && [ "$JOBS" -gt 1 ]; then
    run_parallel "${STAGE_NAMES[@]}" || rc=1
  else
    for n in "${STAGE_NAMES[@]}"; do
      run_one "$n" || rc=1
      if [ "$rc" = "1" ] && [ "$FAIL_FAST" = "1" ]; then break; fi
    done
  fi
  echo
  if [ "$rc" = "1" ] && [ "$FAIL_FAST" = "1" ]; then break; fi
done

# ------------------------------------------------------------------ 汇总
END_TS=$(date +%s)
ALL_STATUS=( $(ls "$RUN_DIR/status" 2>/dev/null) )
for s in "${ALL_STATUS[@]}"; do
  IFS=$'\t' read -r v secs detail <"$RUN_DIR/status/$s"
  case "$v" in
    PASS) PASSED+=("$s") ;;
    SKIP) SKIPPED+=("$s") ;;
    *) FAILED+=("$s") ;;
  esac
done

echo "== 汇总（用时 $((END_TS-START_TS))s） =="
for s in "${ALL_STATUS[@]}"; do
  IFS=$'\t' read -r v secs detail <"$RUN_DIR/status/$s"
  case "$v" in
    PASS) mark="✅" ;;
    SKIP) mark="⏭️ " ;;
    *) mark="❌" ;;
  esac
  printf '  %s %-22s %4ss  %s\n' "$mark" "$s" "${secs:-0}" "$detail"
done
echo
echo "  通过 ${#PASSED[@]} / 跳过 ${#SKIPPED[@]} / 失败 ${#FAILED[@]}"

if [ "${#FAILED[@]}" -gt 0 ]; then
  echo
  echo "== 失败详情（各取日志尾部） =="
  for s in "${FAILED[@]}"; do
    echo "---- $s（$RUN_DIR/logs/$s.log） ----"
    tail -25 "$RUN_DIR/logs/$s.log" 2>/dev/null | sed 's/^/  /'
    echo
  done
fi

# 你桌面上的 daemon / GUI 有没有被测试带走（历史事故：gui-matrix 的 pkill -x qxyncd）
AMBIENT_DAEMON_AFTER="$(pgrep -x qxyncd 2>/dev/null | sort | tr '\n' ' ')"
LOST=()
for p in $AMBIENT_DAEMON_BEFORE; do
  case " $AMBIENT_DAEMON_AFTER " in *" $p "*) ;; *) LOST+=("$p") ;; esac
done
AMBIENT_KILLED=0
if [ "${#LOST[@]}" -gt 0 ]; then
  AMBIENT_KILLED=1
  echo
  echo "⚠️  测试期间你桌面上的 qxyncd 被停掉了（pid ${LOST[*]}）——这是测试套件的责任，不是你的操作。"
  for s in "${!CHOSEN[@]}"; do
    grep -lE 'pkill -x qxyncd' "$TESTS/$s.sh" >/dev/null 2>&1 && echo "    疑似加害者：$s.sh"
  done
  echo "    先用 \`qxync daemon start\` 把守护拉回来；若是新套件的锅，请修脚本（M45 已修 gui-matrix）。"
fi
if [ -n "$AMBIENT_GUI_BEFORE" ]; then
  for g in $AMBIENT_GUI_BEFORE; do
    kill -0 "$g" 2>/dev/null || echo "⚠️  你桌面上的 qxync-gui（pid $g）在测试期间退出了（可能是 GUI 矩阵的正常收尾，也可能是被 pkill 带走）。"
  done
fi

echo "$RUN_DIR" >"$REPO/.local-run/e2e/last-run"
{
  echo "tiers: ${TIERS_ARG[*]:-only:$ONLY_ARG}"
  echo "duration_s: $((END_TS-START_TS))"
  echo "passed: ${#PASSED[@]} skipped: ${#SKIPPED[@]} failed: ${#FAILED[@]}"
} >"$RUN_DIR/summary.txt"

[ "$DRY_RUN" = "1" ] && exit 0
if [ "$AMBIENT_KILLED" = "1" ] && [ "$ALLOW_AMBIENT_STOP" != "1" ]; then
  exit 3
fi
if [ "${#FAILED[@]}" -gt 0 ]; then exit 1; fi
if [ "${#PASSED[@]}" -eq 0 ] && [ "${#SKIPPED[@]}" -gt 0 ]; then
  echo "⚠️  本轮全部 SKIP（缺环境），没有任何套件真正跑过。" >&2
  exit 5
fi
exit 0
