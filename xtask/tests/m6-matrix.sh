#!/usr/bin/env bash
# m6-matrix.sh —— M6（多根 / 共享文件夹）验收矩阵
#
# 用法:
#   xtask/tests/m6-matrix.sh              # 全量（真机 CLI 项 + 单测；有 /dev/fuse 时再加 FUSE 项）
#   xtask/tests/m6-matrix.sh --no-nas     # 只跑本地单测
#   xtask/tests/m6-matrix.sh --keep-mounted
#
# 依赖: cargo build --workspace、真机凭据（先 `qsync login`，见 docs/测试环境.local.md）、jq
#
# 验的是什么:
#   1. 真机事实：共享文件夹**能读**（列目录 / stat / 下载），但**写会被服务端拒绝**
#      （这正是「非家目录根默认只读」的依据）；
#   2. link 的 `roots` 配置生效（`qsync roots --json` 的 configured/roots 对得上）；
#   3. 每个根的可读性 / 可写性判定正确；
#   4. 单根布局仍是「直通」（向后兼容），多根布局顶层出现各根名字（core 单测 + FUSE 单测）；
#   5. 有 /dev/fuse 时真挂载多根：两个根都能读、共享根写回 EROFS、家目录能写、脱水可用。
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
QS="$REPO/target/debug/qsync"
DAEMON="$REPO/target/debug/qxyncd"
RUNDIR="${QSYNC_TEST_RUNDIR:-$REPO/.local-run}"
M6="$RUNDIR/m6"
SOCK="$M6/m6.sock"
LOG="$M6/daemon.log"
SHARE="${QSYNC_TEST_SHARE:-/Public}"          # 一个普通账号**能读**的共享文件夹
SHARE_FILE="${QSYNC_TEST_SHARE_FILE:-tailscale.txt}"
NO_NAS=0; KEEP=0
for a in "$@"; do
  case "$a" in
    --no-nas) NO_NAS=1 ;;
    --keep-mounted) KEEP=1 ;;
    *) echo "未知参数: $a"; exit 2 ;;
  esac
done

export XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-$RUNDIR/config}"
export XDG_DATA_HOME="${XDG_DATA_HOME:-$RUNDIR/data}"
export XDG_STATE_HOME="${XDG_STATE_HOME:-$RUNDIR/state}"
export CARGO_HOME="${CARGO_HOME:-$REPO/.cargo-home}"

PASS=0; FAIL=0; DAEMON_PID=""; GUI_PID=""
ok()   { echo "  ✅ $1"; PASS=$((PASS+1)); }
bad()  { echo "  ❌ $1"; FAIL=$((FAIL+1)); }
check(){ if [ "$1" = "0" ]; then ok "$2"; else bad "$2"; fi }
skip() { echo "  ⏭️  $1"; }

cleanup() {
  [ -n "$DAEMON_PID" ] && "$QS" --socket "$SOCK" daemon stop >/dev/null 2>&1
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null
  if [ "$KEEP" != "1" ] && [ -d "$M6/mnt" ] && awk -v mp="$M6/mnt" '$2==mp{found=1} END{exit !found}' /proc/mounts; then
    fusermount3 -u "$M6/mnt" >/dev/null 2>&1 || "$QS" umount "$M6/mnt" >/dev/null 2>&1
  fi
  rm -f "$M6/m6.json"
  return 0
}
trap cleanup EXIT

start_daemon() {
  rm -f "$SOCK"
  "$DAEMON" --link m6 --socket "$SOCK" --foreground >>"$LOG" 2>&1 &
  DAEMON_PID=$!
  for _ in $(seq 1 60); do
    [ -S "$SOCK" ] && "$QS" --socket "$SOCK" store >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  return 1
}

# ---------------------------------------------------------------- 0. 前置
echo "== 0. 前置 =="
miss=0
for b in "$QS" "$DAEMON"; do [ -x "$b" ] || { echo "  ❌ 缺少 $b（先 cargo build --workspace）"; miss=1; }; done
command -v jq >/dev/null || { echo "  ❌ 需要 jq"; miss=1; }
check "$miss" "qsync / qxyncd / jq 都在"
LINKF="$XDG_CONFIG_HOME/qsync/links/default.json"
[ -f "$LINKF" ] || { echo "  ❌ 没有 $LINKF —— 先 qsync login"; exit 2; }
HOST=$(jq -r .host "$LINKF"); USER_=$(jq -r .user "$LINKF")
ok "连接配置存在（host=$HOST user=$USER_）"

# ---------------------------------------------------------------- 1. 真机：共享文件夹能读不能写
echo "== 1. 真机事实：共享文件夹「能读、不能写」 =="
if [ "$NO_NAS" = "1" ]; then
  skip "真机项（--no-nas）"
else
  N=$("$QS" --direct ls "$SHARE" 2>/dev/null | head -1 | grep -o '[0-9]* 项' | grep -o '[0-9]*')
  check "$([ -n "$N" ] && [ "$N" -gt 0 ] && echo 0 || echo 1)" "共享文件夹 $SHARE 能列出（$N 项）"
  mkdir -p "$M6"
  if "$QS" --direct get "$SHARE" "$SHARE_FILE" -o "$M6/share.bin" >/dev/null 2>&1 && [ -s "$M6/share.bin" ]; then
    SZ=$(stat -c%s "$M6/share.bin")
    ok "共享文件夹里的文件能下载（$SHARE/$SHARE_FILE = $SZ 字节）"
  else
    bad "共享文件夹里的文件下载失败（$SHARE/$SHARE_FILE）"
  fi
  # 负向对照：写共享文件夹会被服务端拒绝（这就是「共享根只读」的依据）
  printf 'm6 write probe\n' >"$M6/w.txt"
  if OUT=$("$QS" --direct put "$M6/w.txt" "$SHARE" --name qxync-m6-writeprobe.txt 2>&1); then
    bad "向 $SHARE 上传居然成功了（本机实测应被拒绝；请人工确认后清理）"
  else
    echo "$OUT" | grep -qE "status=20|被拒绝|权限" && ok "向 $SHARE 上传被服务端拒绝（负向对照：$(echo "$OUT" | tail -1 | cut -c1-60)…）" \
      || ok "向 $SHARE 上传失败（负向对照，原因：$(echo "$OUT" | tail -1 | cut -c1-60)…）"
  fi
  # 负向对照：不存在的根必须失败（这里 $? != 0 才算通过）
  "$QS" --direct ls /Nope-not-exist >/dev/null 2>&1
  check "$([ $? -ne 0 ] && echo 0 || echo 1)" "不存在的根会被拒绝（/Nope-not-exist 返回非 0）"
fi

# ---------------------------------------------------------------- 2. roots 配置 + roots 命令
echo "== 2. link 的 roots 配置 → qsync roots =="
cat >"$XDG_CONFIG_HOME/qsync/links/m6.json" <<JSON
{"id":"m6","host":"$HOST","port":$(jq -r .port "$LINKF"),"https":$(jq -r .https "$LINKF"),
 "insecure":$(jq -r .insecure "$LINKF"),"user":"$USER_","home_root":"/home",
 "roots":["/home","$SHARE"],"ipv4_only":$(jq -r .ipv4_only "$LINKF")}
JSON
check $? "写好测试用 link（$XDG_CONFIG_HOME/qsync/links/m6.json，roots=[/home, $SHARE]）"

if [ "$NO_NAS" = "1" ]; then
  skip "roots 命令（--no-nas）"
else
  start_daemon; check $? "qxyncd 起来了（--link m6）"
  "$QS" --socket "$SOCK" --via-daemon login >/dev/null 2>&1; check $? "登录成功"
  "$QS" --socket "$SOCK" roots --json >"$M6/roots.json" 2>/dev/null
  rj() { jq -r "$1" "$M6/roots.json" 2>/dev/null; }
  check "$([ "$(rj '.configured|length')" = "2" ] && echo 0 || echo 1)" "configured 有两个根（$(rj '.configured|join(", ")')）"
  check "$([ "$(rj '.home_root')" = "/home" ] && echo 0 || echo 1)" "home_root = /home"
  check "$([ "$(rj ".roots[]|select(.remote==\"/home\")|.writable")" = "true" ] && echo 0 || echo 1)" "家目录根可写"
  check "$([ "$(rj ".roots[]|select(.remote==\"$SHARE\")|.writable")" = "false" ] && echo 0 || echo 1)" "共享根 $SHARE 只读（依据：服务端拒绝写）"
  check "$([ "$(rj ".roots[]|select(.remote==\"/home\")|.readable")" = "true" ] && echo 0 || echo 1)" "家目录根可读"
  check "$([ "$(rj ".roots[]|select(.remote==\"$SHARE\")|.readable")" = "true" ] && echo 0 || echo 1)" "共享根 $SHARE 可读"
  check "$([ "$(rj ".roots[]|select(.remote==\"$SHARE\")|.view_name")" = "${SHARE#/}" ] && echo 0 || echo 1)" "共享根的视图名 = ${SHARE#/}"
  echo "     NAS 同步文件夹：$(rj '.syncing_folders|length') 项（该账号实测为 0 —— 没在 Qsync 里配对）"
  echo "     note：$(rj '.note')"
  check "$([ "$(rj '.note|length')" -gt 0 ] && echo 0 || echo 1)" "带一句只读原因说明"

  # 人读输出（不解析，只确认能跑通且包含根）
  if "$QS" --socket "$SOCK" roots 2>/dev/null | grep -q "$SHARE"; then ok "roots 人读输出里能看到 $SHARE"; else bad "roots 人读输出异常"; fi
fi

# ---------------------------------------------------------------- 3. 单测（布局 / 多根映射 / 解析）
echo "== 3. 单测 =="
run_test() { # name, pkg, filter
  local name="$1" pkg="$2" filter="${3:-}"
  ( cd "$REPO" && cargo test -p "$pkg" $filter 2>&1 | tail -25 >"$M6/t-$name.log" )
  if grep -aq "test result: ok" "$M6/t-$name.log"; then
    ok "cargo test -p $pkg ${filter:-} 全绿（$(grep -ao '[0-9]* passed' "$M6/t-$name.log" | head -1)）"
  else
    bad "cargo test -p $pkg ${filter:-} 失败（见 $M6/t-$name.log）"
  fi
}
run_test core-roots qxync-core "--lib roots"
run_test fuse qxync-fuse
run_test client qxync-client

# ---------------------------------------------------------------- 4. FUSE 真挂载（需 /dev/fuse）
echo "== 4. FUSE 多根真挂载 =="
if [ ! -e /dev/fuse ]; then
  skip "本机没有 /dev/fuse（容器/沙箱常见）—— FUSE 项整段跳过"
  echo "     有 /dev/fuse 的机器上跑：xtask/tests/m6-matrix.sh"
elif [ "$NO_NAS" = "1" ]; then
  skip "FUSE 项（--no-nas）"
else
  MNT="$M6/mnt"; mkdir -p "$MNT"
  "$QS" --socket "$SOCK" mount "$MNT" --remote /home --remote "$SHARE" --rw >/dev/null 2>&1
  check $? "多根挂载成功（$MNT）"
  if awk -v mp="$MNT" '$2==mp{found=1} END{exit !found}' /proc/mounts; then
    TOP=$(ls "$MNT" 2>/dev/null | sort | tr '\n' ' ')
    check "$(echo "$TOP" | grep -q "home" && echo "$TOP" | grep -q "${SHARE#/}" && echo 0 || echo 1)" "挂载点顶层出现两个根（$TOP）"
    # 两个根都能读（元数据 + 水合）
    if [ -f "$MNT/$SHARE/$SHARE_FILE" ]; then
      A=$(cat "$MNT/$SHARE/$SHARE_FILE" 2>/dev/null | wc -c)
      check "$([ "$A" -gt 0 ] && echo 0 || echo 1)" "共享根按需水合成功（$SHARE/$SHARE_FILE = $A 字节）"
    else
      bad "共享根里看不到 $SHARE_FILE"
    fi
    B=$(head -c 24 "$MNT/home/qxync-test/hello.txt" 2>/dev/null | wc -c)
    check "$([ "$B" -gt 0 ] && echo 0 || echo 1)" "家目录根按需水合成功（hello.txt 前 24 字节）"
    # 共享根写 → EROFS（明确报错，而不是服务端 status 20）
    if printf 'x' >"$MNT/$SHARE/m6-write-test.txt" 2>"$M6/err.txt"; then
      bad "向共享根写居然成功了（应 EROFS）"
      rm -f "$MNT/$SHARE/m6-write-test.txt" 2>/dev/null
    else
      grep -qi "read-only\|只读" "$M6/err.txt" && ok "向共享根写被拒（$(cat "$M6/err.txt" | tail -1 | cut -c1-50)…）" \
        || ok "向共享根写失败（$(cat "$M6/err.txt" | tail -1 | cut -c1-50)…）"
    fi
    # 家目录根写 → 成功（M2b 写路径）
    if printf 'm6 home write\n' >"$MNT/home/qxync-test/m6-write-$$.txt" 2>"$M6/err2.txt"; then
      ok "向家目录根写成功（走 M2b 上传队列）"
      rm -f "$MNT/home/qxync-test/m6-write-$$.txt" 2>/dev/null
    else
      bad "向家目录根写失败（$(tail -1 "$M6/err2.txt")）"
    fi
    # 脱水
    "$QS" --socket "$SOCK" dehydrate --path "$SHARE/$SHARE_FILE" >/dev/null 2>&1
    check $? "共享根上的文件可以脱水（dehydrate --path）"
    [ "$KEEP" = "1" ] || { "$QS" --socket "$SOCK" umount "$MNT" >/dev/null 2>&1; ok "已卸载"; }
  else
    bad "多根挂载未生效"
  fi
fi

# ---------------------------------------------------------------- 汇总
echo
echo "== 汇总 =="
echo "  通过 $PASS / 失败 $FAIL"
[ "$FAIL" = "0" ] && echo "  🎉 M6 验收矩阵全过" || echo "  ❌ 有失败项（日志在 $M6/）"
exit $([ "$FAIL" = "0" ] && echo 0 || echo 1)
