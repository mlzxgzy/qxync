#!/usr/bin/env bash
# m7-matrix.sh —— M7（选择性同步 + 设备配对 / LAN 直连）验收矩阵
#
# 用法:
#   xtask/tests/m7-matrix.sh              # 全量（单测 + LAN loopback + 真机；有 /dev/fuse 再加 FUSE 段）
#   xtask/tests/m7-matrix.sh --no-nas     # 单测 + LAN loopback（完全不需要 NAS）
#   xtask/tests/m7-matrix.sh --keep-mounted
#
# 依赖: cargo build --workspace、jq；真机项需要先 `qsync login`（见 docs/测试环境.local.md）
#
# 验的是什么（对应 docs/M7-选择性同步与LAN直连.md §3）:
#   1. 规则引擎：锚定/任意层级/**/尾斜杠目录剪枝/!/内置临时文件/坏规则不 panic（core 单测）；
#   2. FUSE 层：排除路径 lookup→ENOENT、readdir 剔除、脱水候选永不含排除路径、
#      未配置规则时行为与 M6 一致（fuse 单测；真挂载段在有 /dev/fuse 时再验一遍）；
#   3. 同步引擎：排除路径不列/不对账/不登记 baseline、事件跳过（daemon 单测）；
#   4. LAN 协议：配对码/token/路径越权/部分水合不可服务/head/get(Range)/事件（client 单测）；
#   5. LAN 水合：对端元数据一致 → 走 LAN；不一致 → 回落 NAS（fuse 单测 + 真挂载段）；
#   6. 两个真 daemon：配对（双向登记）、ping、错码被拒、事件快路径唤醒轮询、
#      上传后自动广播；没有挂载时对端没有任何内容可服务（负向对照）。
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
QS="$REPO/target/debug/qsync"
DAEMON="$REPO/target/debug/qxyncd"
RUNDIR="${QSYNC_TEST_RUNDIR:-$REPO/.local-run}"
M7="$RUNDIR/m7"
A="$M7/a"; B="$M7/b"
SOCKA="$A/qxyncd.sock"; SOCKB="$B/qxyncd.sock"
LOGA="$A/daemon.log"; LOGB="$B/daemon.log"
PORT_A="${QSYNC_TEST_PEER_A:-19870}"
PORT_B="${QSYNC_TEST_PEER_B:-19871}"
SHARE_DIR="${QSYNC_TEST_DIR:-/home/qxync-test}"
HIDDEN_FILE="${QSYNC_TEST_HIDDEN:-1k.bin}"      # 被规则排除的真实文件
VISIBLE_FILE="${QSYNC_TEST_VISIBLE:-hello.txt}" # 正常文件（LAN 直传也用它）
NO_NAS=0; KEEP=0
for a in "$@"; do
  case "$a" in
    --no-nas) NO_NAS=1 ;;
    --keep-mounted) KEEP=1 ;;
    *) echo "未知参数: $a"; exit 2 ;;
  esac
done

export CARGO_HOME="${CARGO_HOME:-$REPO/.cargo-home}"

PASS=0; FAIL=0; PID_A=""; PID_B=""
ok()   { echo "  ✅ $1"; PASS=$((PASS+1)); }
bad()  { echo "  ❌ $1"; FAIL=$((FAIL+1)); }
check(){ if [ "$1" = "0" ]; then ok "$2"; else bad "$2"; fi }
skip() { echo "  ⏭️  $1"; }

cleanup() {
  [ -n "$PID_A" ] && XDG_CONFIG_HOME="$A/config" XDG_DATA_HOME="$A/data" XDG_STATE_HOME="$A/state" \
    "$QS" --socket "$SOCKA" daemon stop >/dev/null 2>&1
  [ -n "$PID_B" ] && XDG_CONFIG_HOME="$B/config" XDG_DATA_HOME="$B/data" XDG_STATE_HOME="$B/state" \
    "$QS" --socket "$SOCKB" daemon stop >/dev/null 2>&1
  [ -n "$PID_A" ] && kill "$PID_A" 2>/dev/null
  [ -n "$PID_B" ] && kill "$PID_B" 2>/dev/null
  if [ "$KEEP" != "1" ]; then
    for mp in "$A/mnt" "$B/mnt"; do
      if [ -d "$mp" ] && awk -v m="$mp" '$2==m{found=1} END{exit !found}' /proc/mounts; then
        fusermount3 -u "$mp" >/dev/null 2>&1 || true
      fi
    done
  fi
  return 0
}
trap cleanup EXIT

# 每个实例一套独立 XDG（同一台机器上的两个 qxync 设备）
env_a() { XDG_CONFIG_HOME="$A/config" XDG_DATA_HOME="$A/data" XDG_STATE_HOME="$A/state" "$@"; }
env_b() { XDG_CONFIG_HOME="$B/config" XDG_DATA_HOME="$B/data" XDG_STATE_HOME="$B/state" "$@"; }
qa() { env_a "$QS" --socket "$SOCKA" "$@"; }
qb() { env_b "$QS" --socket "$SOCKB" "$@"; }

setup_instance() { # dir name port  (exclude 规则另加)
  local dir="$1" name="$2" port="$3"
  mkdir -p "$dir/config/qsync/links" "$dir/data" "$dir/state"
  jq --arg n "$name" --arg l "127.0.0.1:$port" \
     '.peer_name=$n | .peer_listen=$l' "$LINKF" >"$dir/config/qsync/links/default.json"
  [ -f "$CREDF" ] && cp "$CREDF" "$dir/config/qsync/credentials.json"
}

start_daemon() { # dir socket log poll  → 回显 pid
  local dir="$1" sock="$2" log="$3" poll="$4"
  rm -f "$sock"
  XDG_CONFIG_HOME="$dir/config" XDG_DATA_HOME="$dir/data" XDG_STATE_HOME="$dir/state" \
  QSYNC_POLL_INTERVAL="$poll" \
    "$DAEMON" --link default --socket "$sock" --foreground >>"$log" 2>&1 &
  echo $!
}
wait_daemon() { # dir socket
  local dir="$1" sock="$2"
  for _ in $(seq 1 60); do
    [ -S "$sock" ] && XDG_CONFIG_HOME="$dir/config" XDG_DATA_HOME="$dir/data" XDG_STATE_HOME="$dir/state" \
      "$QS" --socket "$sock" peer status >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  return 1
}

# ---------------------------------------------------------------- 0. 前置
echo "== 0. 前置 =="
mkdir -p "$M7"
miss=0
for b in "$QS" "$DAEMON"; do [ -x "$b" ] || { echo "  ❌ 缺少 $b（先 cargo build --workspace）"; miss=1; }; done
command -v jq >/dev/null || { echo "  ❌ 需要 jq"; miss=1; }
check "$miss" "qsync / qxyncd / jq 都在"
LINKF="${XDG_CONFIG_HOME:-$RUNDIR/config}/qsync/links/default.json"
CREDF="${XDG_CONFIG_HOME:-$RUNDIR/config}/qsync/credentials.json"
if [ ! -f "$LINKF" ]; then echo "  ❌ 没有 $LINKF —— 先 qsync login"; exit 2; fi
HOST=$(jq -r .host "$LINKF"); USER_=$(jq -r .user "$LINKF")
ok "连接配置存在（host=$HOST user=$USER_）"

# ---------------------------------------------------------------- 1. 单测
echo "== 1. 单测（规则引擎 / FUSE 过滤 / 同步引擎 / LAN 协议） =="
run_test() { # name pkg filter target
  local name="$1" pkg="$2" filter="${3:-}" target="${4:---lib}"
  ( cd "$REPO" && cargo test -p "$pkg" $target $filter 2>&1 | tail -40 >"$M7/t-$name.log" )
  if grep -aq "test result: ok" "$M7/t-$name.log"; then
    ok "cargo test -p $pkg $target ${filter:-} 全绿（$(grep -ao '[0-9]* passed' "$M7/t-$name.log" | head -1)）"
  else
    bad "cargo test -p $pkg $target ${filter:-} 失败（见 $M7/t-$name.log）"
  fi
}
run_test core-rules qxync-core "rules"
run_test fuse-m7 qxync-fuse "m7_"
run_test daemon-m7 qxync-daemon "m7_" "--bins"
run_test client-peer qxync-client "peer"

# ---------------------------------------------------------------- 2. 两个实例 + 规则命令
echo "== 2. 选择性同步规则（qsync rules） =="
setup_instance "$A" "qxync-a" "$PORT_A"
setup_instance "$B" "qxync-b" "$PORT_B"
# 给 A 加排除规则（真实文件 + 目录 + 一条不存在的）
jq --arg h "/qxync-test/$HIDDEN_FILE" '.exclude=[$h, "/qxync-test/nope-dir"]' \
   "$A/config/qsync/links/default.json" >"$A/config/qsync/links/default.json.tmp" \
   && mv "$A/config/qsync/links/default.json.tmp" "$A/config/qsync/links/default.json"
check $? "写好 A/B 两个实例的 link（A 带 2 条 exclude，peer_listen=127.0.0.1:$PORT_A/$PORT_B）"
# 故意塞一条坏规则：必须被点名而不是静默
jq '.exclude += ["///"]' "$A/config/qsync/links/default.json" >"$A/config/qsync/links/default.json.tmp" \
   && mv "$A/config/qsync/links/default.json.tmp" "$A/config/qsync/links/default.json"

PID_A=$(start_daemon "$A" "$SOCKA" "$LOGA" 5)
wait_daemon "$A" "$SOCKA"; check $? "qxyncd A 起来了（--link default）"

qa rules --json >"$M7/rules.json" 2>/dev/null
rj() { jq -r "$1" "$M7/rules.json" 2>/dev/null; }
check "$([ "$(rj '.patterns|length')" = "2" ] && echo 0 || echo 1)" "规则编译出 2 条（坏规则不进 patterns）"
check "$([ "$(rj '.bad|length')" = "1" ] && echo 0 || echo 1)" "坏规则被点名（bad=$(rj '.bad|join(",")')）"
check "$([ "$(rj '.filter_temp')" = "true" ] && echo 0 || echo 1)" "内置临时文件过滤默认开"
check "$([ "$(rj '.temp_patterns|length')" -ge 5 ] && echo 0 || echo 1)" "内置临时规则 $(rj '.temp_patterns|length') 条"
check "$([ "$(rj '.roots|join(",")')" = "/home" ] && echo 0 || echo 1)" "远端根 = /home"

qa rules --match "$SHARE_DIR/$HIDDEN_FILE" --json >"$M7/m1.json" 2>/dev/null
check "$([ "$(jq -r '.match_hidden' "$M7/m1.json")" = "true" ] && echo 0 || echo 1)" "--match 被排除文件 → hidden=true"
check "$([ "$(jq -r '.match_reason' "$M7/m1.json")" = "excluded" ] && echo 0 || echo 1)" "原因 = excluded"
qa rules --match "$SHARE_DIR/$VISIBLE_FILE" --json >"$M7/m2.json" 2>/dev/null
check "$([ "$(jq -r '.match_hidden' "$M7/m2.json")" = "false" ] && echo 0 || echo 1)" "--match 正常文件 → hidden=false"
qa rules --match "$SHARE_DIR/x/y.crdownload" --json >"$M7/m3.json" 2>/dev/null
check "$([ "$(jq -r '.match_reason' "$M7/m3.json")" = "temp" ] && echo 0 || echo 1)" "--match 临时文件 → reason=temp"
qa rules --match /etc/passwd --json >"$M7/m4.json" 2>/dev/null
check "$([ "$(jq -r '.match_reason' "$M7/m4.json")" = "outside-roots" ] && echo 0 || echo 1)" "--match 范围外路径 → outside-roots"
if qa rules 2>/dev/null | grep -q "规则条数"; then ok "rules 人读输出正常"; else bad "rules 人读输出异常"; fi

# ---------------------------------------------------------------- 3. LAN 对等（A ↔ B，不需要 NAS）
echo "== 3. LAN 设备配对 / 事件快路径（两个真 daemon） =="
PID_B=$(start_daemon "$B" "$SOCKB" "$LOGB" 3600)
wait_daemon "$B" "$SOCKB"; check $? "qxyncd B 起来了（轮询 3600s：只有事件唤醒才会跑）"

qa peer status --json >"$M7/pa.json" 2>/dev/null
qb peer status --json >"$M7/pb.json" 2>/dev/null
check "$([ "$(jq -r '.enabled' "$M7/pa.json")" = "true" ] && echo 0 || echo 1)" "A 的 LAN 监听从配置生效（$(jq -r .listen "$M7/pa.json")）"
CODE_B=$(jq -r '.pairing_code' "$M7/pb.json")
check "$([ -n "$CODE_B" ] && [ "$CODE_B" != "null" ] && echo 0 || echo 1)" "B 给出了配对码（$CODE_B）"
CODE_A=$(jq -r '.pairing_code' "$M7/pa.json")
check "$([ -n "$CODE_A" ] && [ "$CODE_A" != "$CODE_B" ] && echo 0 || echo 1)" "两个实例的配对码不同（不共用全局码）"

# 负向：错配对码必须失败
if qa peer pair "127.0.0.1:$PORT_B" --code 000000 >/dev/null 2>&1; then
  bad "错配对码居然配对成功"
else
  ok "错配对码被拒（负向对照）"
fi
# 正向：用 B 的码配对
if OUT=$(qa peer pair "127.0.0.1:$PORT_B" --code "$CODE_B" 2>&1); then
  ok "配对成功（$(echo "$OUT" | head -1 | cut -c1-70)）"
else
  bad "配对失败：$(echo "$OUT" | tail -1)"
fi
qa peer list --json >"$M7/la.json" 2>/dev/null
MA=$(jq -r '.devices[]|select(.name=="qxync-b")|.token_masked' "$M7/la.json" 2>/dev/null)
check "$([ -n "$MA" ] && [ "$MA" != "null" ] && echo 0 || echo 1)" "A 的登记表里有 qxync-b（token 掩码 $MA）"
check "$(echo "$MA" | grep -q '…' && echo 0 || echo 1)" "token 只回掩码（绝不回全量）"

# 双向：hello 让 B 也认识 A（事件快路径需要这一半）
sleep 0.5
qb peer list --json >"$M7/lb.json" 2>/dev/null
check "$([ "$(jq -r '.devices|length' "$M7/lb.json")" = "1" ] && echo 0 || echo 1)" "B 也登记了 A（hello 双向信任）"
if qb peer ping qxync-a >/dev/null 2>&1; then ok "B 能按名字 ping 通 A"; else bad "B ping A 失败"; fi
if qa peer ping qxync-b >/dev/null 2>&1; then ok "A 能按名字 ping 通 B"; else bad "A ping B 失败"; fi
if qa peer ping 127.0.0.1:1 >/dev/null 2>&1; then bad "不存在的对端居然 ping 通了"; else ok "ping 不存在的地址会失败（负向对照）"; fi

# 事件快路径：notify → B 的轮询被唤醒
POLLS0=$(qb sync --json 2>/dev/null | jq -r '.polls // 0')
[ -z "$POLLS0" ] && POLLS0=0
qa peer notify "$SHARE_DIR/$VISIBLE_FILE" >/dev/null 2>&1
check $? "A 广播事件（peer notify）"
WOKE=1
for _ in $(seq 1 20); do
  sleep 0.5
  POLLS1=$(qb sync --json 2>/dev/null | jq -r '.polls // 0')
  [ -z "$POLLS1" ] && POLLS1=0
  [ "${POLLS1:-0}" -gt "${POLLS0:-0}" ] && { WOKE=0; break; }
done
check "$WOKE" "B 被事件唤醒并跑了一轮对账（polls $POLLS0 → ${POLLS1:-?}；B 的轮询间隔是 3600s）"
qb peer events --json >"$M7/evb.json" 2>/dev/null
check "$([ "$(jq -r --arg p "$SHARE_DIR/$VISIBLE_FILE" '[.events[].path]|index($p)!=null' "$M7/evb.json")" = "true" ] && echo 0 || echo 1)" "B 的事件日志里有这条路径"
check "$([ "$(qb peer status --json 2>/dev/null | jq -r '.events_in')" -ge 1 ] && echo 0 || echo 1)" "B 的 events_in 计数 ≥ 1"

# ---------------------------------------------------------------- 4. 真机：规则不影响远端 + 上传广播 + 没有挂载就没有内容可直传
echo "== 4. 真机（NAS）：远端不受规则影响 / 上传后自动广播 =="
if [ "$NO_NAS" = "1" ]; then
  skip "真机项（--no-nas）"
else
  qa login >/dev/null 2>&1; check $? "A 登录成功"
  if qa ls "$SHARE_DIR" 2>/dev/null | grep -q "$HIDDEN_FILE"; then
    ok "远端目录里 $HIDDEN_FILE 仍在（选择性同步=客户端行为，不动 NAS）"
  else
    bad "远端看不到 $HIDDEN_FILE（不应该：规则只影响本地）"
  fi

  # 没有挂载 → 对端没有任何完整水合的文件 → 直传必须失败（负向对照：绝不拿半份内容糊弄）
  if qa peer fetch qxync-b "$SHARE_DIR/$VISIBLE_FILE" "$M7/fetch-should-fail.bin" >/dev/null 2>&1; then
    bad "无挂载时对端居然能直传内容"
  else
    ok "无挂载时对端拒绝直传（负向对照：没有完整水合就不服务）"
  fi

  # 上传 → A 广播 → B 收到（端到端事件快路径）
  OUT_A0=$(qa peer status --json 2>/dev/null | jq -r '.events_out')
  OUT_B0=$(qb peer status --json 2>/dev/null | jq -r '.events_in')
  printf 'm7 event probe %s\n' "$$" >"$M7/probe.txt"
  NAME="qxync-m7-event-$$.txt"
  if qa put "$M7/probe.txt" "$SHARE_DIR" --name "$NAME" >/dev/null 2>&1; then
    ok "上传 $NAME 成功"
    sleep 2
    OUT_A1=$(qa peer status --json 2>/dev/null | jq -r '.events_out')
    OUT_B1=$(qb peer status --json 2>/dev/null | jq -r '.events_in')
    check "$([ "${OUT_A1:-0}" -gt "${OUT_A0:-0}" ] && echo 0 || echo 1)" "A 上传后自动广播（events_out $OUT_A0 → ${OUT_A1:-?}）"
    check "$([ "${OUT_B1:-0}" -gt "${OUT_B0:-0}" ] && echo 0 || echo 1)" "B 收到对端事件（events_in $OUT_B0 → ${OUT_B1:-?}）"
    qa rm "$SHARE_DIR" "$NAME" >/dev/null 2>&1 && ok "清理远端测试文件"
  else
    bad "上传测试文件失败"
  fi
fi

# ---------------------------------------------------------------- 5. FUSE 真挂载（需 /dev/fuse）
echo "== 5. FUSE 真挂载：隐藏 / 写保护 / LAN 直传 =="
if [ ! -e /dev/fuse ]; then
  skip "本机没有 /dev/fuse（容器/沙箱常见）—— FUSE 段整段跳过（有设备的机器上重跑本矩阵）"
elif [ "$NO_NAS" = "1" ]; then
  skip "FUSE 项（--no-nas）"
else
  qb login >/dev/null 2>&1
  MNT_A="$A/mnt"; MNT_B="$B/mnt"; mkdir -p "$MNT_A" "$MNT_B"
  qa mount "$MNT_A" --remote /home --rw >/dev/null 2>&1
  check $? "A 挂载成功（$MNT_A，带 exclude 规则）"
  if awk -v mp="$MNT_A" '$2==mp{found=1} END{exit !found}' /proc/mounts; then
    LS=$(ls "$MNT_A/qxync-test" 2>/dev/null | tr '\n' ' ')
    check "$(echo "$LS" | grep -q "$VISIBLE_FILE" && echo 0 || echo 1)" "正常文件可见（$VISIBLE_FILE）"
    check "$(echo "$LS" | grep -q "$HIDDEN_FILE" && echo 1 || echo 0)" "被排除文件在挂载点里不可见（$HIDDEN_FILE）"
    if [ -e "$MNT_A/qxync-test/$HIDDEN_FILE" ]; then bad "lookup 应 ENOENT"; else ok "被排除路径 lookup → ENOENT"; fi
    if ( printf 'x' >"$MNT_A/qxync-test/$HIDDEN_FILE" ) 2>"$M7/err.txt"; then
      bad "向被排除路径写居然成功了"
    else
      ok "向被排除路径写失败（$(tail -1 "$M7/err.txt" | cut -c1-50)）"
    fi
    # 临时文件过滤：远端放一个 .crdownload → 挂载点里看不到
    printf 'temp\n' >"$M7/tmp-probe.crdownload"
    qa put "$M7/tmp-probe.crdownload" "$SHARE_DIR" >/dev/null 2>&1
    sleep 0.5
    if ls "$MNT_A/qxync-test" 2>/dev/null | grep -q "crdownload"; then
      bad "临时文件在挂载点里可见（应被内置规则隐藏）"
    else
      ok "临时文件（*.crdownload）被内置规则隐藏"
    fi
    qa rm "$SHARE_DIR" "tmp-probe.crdownload" >/dev/null 2>&1

    # LAN 直传：A 先把 hello.txt 水合到本地；B 再读同一文件 → 应命中 A
    A_BYTES=$(cat "$MNT_A/qxync-test/$VISIBLE_FILE" 2>/dev/null | wc -c)
    check "$([ "${A_BYTES:-0}" -gt 0 ] && echo 0 || echo 1)" "A 水合 $VISIBLE_FILE（$A_BYTES 字节）"
    qb mount "$MNT_B" --remote /home --rw >/dev/null 2>&1
    check $? "B 挂载成功（$MNT_B）"
    if awk -v mp="$MNT_B" '$2==mp{found=1} END{exit !found}' /proc/mounts; then
      B_BYTES=$(cat "$MNT_B/qxync-test/$VISIBLE_FILE" 2>/dev/null | wc -c)
      check "$([ "${B_BYTES:-0}" = "${A_BYTES:-1}" ] && echo 0 || echo 1)" "B 读到同样的 $VISIBLE_FILE（$B_BYTES 字节）"
      LAN_HITS=$(qb peer status --json 2>/dev/null | jq -r '.lan_hits // 0')
      check "$([ "${LAN_HITS:-0}" -ge 1 ] && echo 0 || echo 1)" "B 的这次水合走了 LAN 直传（lan_hits=$LAN_HITS）"
      grep -q "LAN 直传命中" "$LOGB" && ok "B 的日志里有「LAN 直传命中」" || bad "B 的日志里没有 LAN 命中记录"
      [ "$KEEP" = "1" ] || { qb umount "$MNT_B" >/dev/null 2>&1; ok "B 已卸载"; }
    else
      bad "B 挂载未生效"
    fi
    [ "$KEEP" = "1" ] || { qa umount "$MNT_A" >/dev/null 2>&1; ok "A 已卸载"; }
  else
    bad "A 挂载未生效"
  fi
fi

# ---------------------------------------------------------------- 汇总
echo
echo "== 汇总 =="
echo "  通过 $PASS / 失败 $FAIL"
[ "$FAIL" = "0" ] && echo "  🎉 M7 验收矩阵全过" || echo "  ❌ 有失败项（日志在 $M7/）"
exit $([ "$FAIL" = "0" ] && echo 0 || echo 1)
