#!/usr/bin/env bash
# fuse-matrix.sh —— M1(FUSE 只读 + on-demand) 验收矩阵
#
# 用法:
#   xtask/tests/fuse-matrix.sh                 # 快测（小文件/元数据/xattr/EIO），~30s
#   xtask/tests/fuse-matrix.sh --big           # 追加 128 MiB 水合 + 并发去重，~5min（取决于带宽）
#   xtask/tests/fuse-matrix.sh --keep-mounted  # 结束后不卸载（方便手动玩）
#
# 依赖：已 `cargo build`（target/debug/qsync）、/dev/fuse、fusermount3。
# 凭据：读 XDG_CONFIG_HOME 下的 qsync 配置；先用 `qsync login` 登录过一次。
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
QS="$REPO/target/debug/qsync"
RUNDIR="${QSYNC_TEST_RUNDIR:-$REPO/.local-run}"
MNT="$RUNDIR/mnt"
CACHE="$RUNDIR/cache"
FIXTURE="${QSYNC_TEST_FIXTURE:-/home/qxync-test}"     # NAS 上的路径
LOCAL_FIXTURE="$REPO/report/probe/probe-out/fixture/local"
BIG=0
KEEP=0
# 大文件整文件水合受带宽限制：对端 ~1.1MB/s 时 128MiB 要 ~116s，
# 所以 --big 段单独用一个宽松的水合超时（M2 换成区间水合后就不再敏感）。
HYD="${QSYNC_HYDRATE_TIMEOUT:-600}"
for a in "$@"; do
  case "$a" in
    --big) BIG=1 ;;
    --keep-mounted) KEEP=1 ;;
    *) echo "未知参数: $a"; exit 2 ;;
  esac
done

PASS=0; FAIL=0
ok()   { echo "  ✅ $1"; PASS=$((PASS+1)); }
bad()  { echo "  ❌ $1"; FAIL=$((FAIL+1)); }
check(){ if [ "$1" = "0" ]; then ok "$2"; else bad "$2"; fi }

mkdir -p "$MNT" "$CACHE"
export XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-$RUNDIR/config}"
export XDG_DATA_HOME="${XDG_DATA_HOME:-$RUNDIR/data}"
export XDG_STATE_HOME="${XDG_STATE_HOME:-$RUNDIR/state}"
export RUST_LOG="${RUST_LOG:-info}"
LOG="$RUNDIR/fuse-matrix.log"

if ! [ -x "$QS" ]; then echo "缺少 $QS，先 cargo build"; exit 1; fi

cleanup() {
  if [ "$KEEP" = "0" ]; then "$QS" umount "$MNT" >/dev/null 2>&1; fi
}
trap cleanup EXIT

echo "=== 挂载（只读 + on-demand，60s 水合超时）==="
rm -f "$CACHE"/*
"$QS" mount "$MNT" --remote "${QSYNC_REMOTE_ROOT:-/home}" --cache-dir "$CACHE" \
      --threads 4 --auto-unmount ${QSYNC_MOUNT_EXTRA:-} >"$LOG" 2>&1 &
MOUNT_PID=$!
for _ in $(seq 1 30); do grep -q "$MNT" /proc/mounts && break; sleep 0.5; done
grep -q "$MNT" /proc/mounts || { echo "挂载失败，日志："; cat "$LOG"; exit 1; }
echo "  mounted: $(grep "$MNT" /proc/mounts | head -1)"

M="$MNT$( echo "$FIXTURE" | sed 's#^/home##' )"   # /home -> 挂载点根

echo
echo "=== #1 占位符：ls 显示真实大小、且未下载 ==="
sz_hello=$(stat -c %s "$M/hello.txt" 2>/dev/null)
sz_big=$(stat -c %s "$M/big.bin" 2>/dev/null)
cache_before=$(du -sb "$CACHE" | cut -f1)
echo "  hello.txt=$sz_hello  big.bin=$sz_big  cache=${cache_before}B"
check "$([ "${sz_hello:-0}" = "24" ] && echo 0 || echo 1)" "hello.txt 大小 = 24（真实元数据）"
check "$([ "${sz_big:-0}" = "134217728" ] && echo 0 || echo 1)" "big.bin 大小 = 134217728（真实元数据）"
check "$([ "$cache_before" -lt 4096 ] && echo 0 || echo 1)" "读元数据不触发下载（缓存仍为空）"

echo
echo "=== #3/#4 读取正确性（cat / dd / cmp）==="
if cmp -s <(cat "$M/hello.txt") "$LOCAL_FIXTURE/hello.txt"; then ok "hello.txt 逐字节一致"; else bad "hello.txt 不一致"; fi
if cmp -s <(cat "$M/空 格 中文名.txt") "$LOCAL_FIXTURE/空 格 中文名.txt"; then ok "中文+空格文件名逐字节一致"; else bad "中文+空格文件名不一致"; fi
check "$([ "$(wc -c < "$M/empty.txt")" = "0" ] && echo 0 || echo 1)" "空文件 = 0 字节"
check "$([ "$(cat "$M/l1/l2/l3/l4/l5/l6/l7/l8/l9/l10/deep.txt")" = "deep 10 levels" ] && echo 0 || echo 1)" "10 层嵌套可读"
check "$([ "$(wc -c < "$M/1k.bin")" = "1024" ] && echo 0 || echo 1)" "1k.bin 字节数正确（无零填充）"

echo
echo "=== #5 mmap ==="
if python3 -c "
import mmap,sys
f=open('$M/1k.bin','rb'); m=mmap.mmap(f.fileno(),0,prot=mmap.PROT_READ)
sys.exit(0 if (len(m)==1024 and m[0]==0 and m[255]==255 and m[256]==0) else 1)
"; then ok "mmap 读取正确、无 SIGBUS"; else bad "mmap 失败"; fi

echo
echo "=== xattr 可观测（M1.7）==="
state=$(python3 -c "
import os
try: print(os.getxattr('$M/hello.txt','user.qsync.state').decode())
except OSError as e: print('ERR%d'%e.errno)
")
echo "  user.qsync.state = $state"
check "$([ "$state" = "hydrated" ] && echo 0 || echo 1)" "读取后状态 = hydrated"
check "$(python3 -c "
import os,sys
names=os.listxattr('$M/hello.txt')
sys.exit(0 if 'user.qsync.state' in names and 'user.qsync.vsize' in names else 1)
" && echo 0 || echo 1)" "listxattr 返回全部键（含末尾 NUL 校验）"

echo
echo "=== #11 水合失败 → EIO，不挂死、不零填充 ==="
# 用 1s 超时挂第二个只读视图来模拟「网络慢/断」时水合失败
MNT2="$RUNDIR/mnt-timeout"
mkdir -p "$MNT2"
"$QS" mount "$MNT2" --remote "${QSYNC_REMOTE_ROOT:-/home}" --cache-dir "$CACHE" \
      --threads 2 --auto-unmount --hydrate-timeout 1 >>"$LOG" 2>&1 &
for _ in $(seq 1 30); do grep -q "$MNT2" /proc/mounts && break; sleep 0.5; done
M2="$MNT2$( echo "$FIXTURE" | sed 's#^/home##' )"
timeout 30 cat "$M2/big.bin" >"$RUNDIR/eio.out" 2>"$RUNDIR/eio.err"
rc=$?
n=$(wc -c <"$RUNDIR/eio.out")
echo "  cat 退出码=$rc 输出字节=$n  stderr=$(cat "$RUNDIR/eio.err")"
check "$([ "$rc" = "1" ] && echo 0 || echo 1)" "返回 EIO（退出码 1，而非挂死 124）"
check "$([ "$n" = "0" ] && echo 0 || echo 1)" "失败时不吐零填充假数据"
check "$(ls "$M2" >/dev/null 2>&1 && echo 0 || echo 1)" "失败后文件系统仍存活"
check "$([ -z "$(ls -A "$CACHE" | grep qsync-part)" ] && echo 0 || echo 1)" "不残留 .qsync-part 半截文件"
"$QS" umount "$MNT2" >/dev/null 2>&1

if [ "$BIG" = "1" ]; then
  echo
  echo "=== 128 MiB 整文件水合 + md5（水合超时 ${HYD}s）==="
  "$QS" umount "$MNT" >/dev/null 2>&1
  rm -f "$CACHE"/*; : >"$LOG"
  "$QS" mount "$MNT" --remote "${QSYNC_REMOTE_ROOT:-/home}" --cache-dir "$CACHE" \
        --threads 4 --auto-unmount --hydrate-timeout "$HYD" >>"$LOG" 2>&1 &
  for _ in $(seq 1 30); do grep -q "$MNT" /proc/mounts && break; sleep 0.5; done
  read_big() {  # 输出 "字节数 md5"；失败输出 "0 -"
    local n h
    n=$(cat "$M/big.bin" | wc -c 2>/dev/null)
    if [ "$n" != "134217728" ]; then echo "0 -"; return; fi
    h=$(md5sum <"$M/big.bin" | cut -d' ' -f1)
    echo "$n $h"
  }
  t0=$(date +%s)
  out=$(read_big); t1=$(date +%s)
  if [ "${out%% *}" != "134217728" ]; then
    # 公网抖动会让整文件水合中途失败（此时按设计回 EIO）；重试一次再判定
    echo "  ⚠️  首次读取失败（可能是公网抖动导致水合中断，已按设计回 EIO）→ 重试一次"
    out=$(read_big); t1=$(date +%s)
  fi
  got=${out%% *}; h1=${out##* }
  h2=$(md5sum <"$LOCAL_FIXTURE/big.bin" | cut -d' ' -f1)
  echo "  cat | wc -c = $got  md5=$h1  ($((t1-t0))s)"
  check "$([ "$got" = "134217728" ] && echo 0 || echo 1)" "字节数 = 134217728"
  check "$([ -n "$h1" ] && [ "$h1" = "$h2" ] && echo 0 || echo 1)" "md5 与本地 fixture 一致 ($h2)"

  echo
  echo "=== 水合去重（4 并发读同一文件）==="
  "$QS" umount "$MNT" >/dev/null 2>&1
  # 重新挂载以清掉内核 page cache，否则读到的是内核缓存，测不出并发
  rm -f "$CACHE"/* "$RUNDIR"/dedup-*.txt
  : >"$LOG"
  "$QS" mount "$MNT" --remote "${QSYNC_REMOTE_ROOT:-/home}" --cache-dir "$CACHE" \
        --threads 4 --auto-unmount --hydrate-timeout "$HYD" >>"$LOG" 2>&1 &
  for _ in $(seq 1 30); do grep -q "$MNT" /proc/mounts && break; sleep 0.5; done
  # 注意：不能裸用 wait —— 后台还挂着 FUSE 挂载进程，wait 会一直等它。
  pids=()
  for i in 1 2 3 4; do (md5sum "$M/big.bin" >"$RUNDIR/dedup-$i.txt" 2>&1) & pids+=($!); done
  for pid in "${pids[@]}"; do wait "$pid"; done
  n_ok=$(grep -c "$h1" "$RUNDIR"/dedup-*.txt | grep -c ':1$')
  n_hyd=$(grep -c "水合完成" "$LOG")
  n_files=$(ls "$CACHE" | wc -l)
  echo "  4 路 md5 一致数=$n_ok  水合次数=$n_hyd  缓存文件数=$n_files"
  check "$([ "$n_ok" = "4" ] && echo 0 || echo 1)" "4 路读取结果一致"
  check "$([ "$n_hyd" = "1" ] && echo 0 || echo 1)" "并发读只下载一次（single-flight）"
fi

echo
echo "=== 卸载干净（无残留）==="
if [ "$KEEP" = "0" ]; then
  "$QS" umount "$MNT" >/dev/null 2>&1
  sleep 1
  check "$(grep -q "$MNT" /proc/mounts && echo 1 || echo 0)" "卸载后无残留挂载"
  grep -E "unmount: 水合" "$LOG" | tail -1 | sed 's/^/  /'
fi

echo
echo "================ 结果: 通过 $PASS / 失败 $FAIL ================"
[ "$FAIL" = "0" ]
