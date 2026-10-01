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

# ★ 精确判断挂载点：`grep -F "$MNT"` 会把 mnt-rw / mnt-m3 也算进来（踩过：
#   上一次跑留下的挂载会让这一次的检查全跑在旧挂载上，而且旧挂载的缓存目录刚被 rm 掉）。
is_mounted() {
  awk -v mp="$1" '$2==mp { found=1 } END { exit !found }' /proc/mounts
}

# 上一次跑崩了可能留下挂载点/进程：开跑前先清干净（只碰本仓库 $RUNDIR 下的挂载点）
cleanup_stale() {
  local mp
  for mp in "$RUNDIR"/mnt*; do
    [ -d "$mp" ] || continue
    if is_mounted "$mp"; then
      echo "  ⚠️  清理上次残留的挂载: $mp"
      fusermount3 -u "$mp" >/dev/null 2>&1 || "$QS" umount "$mp" >/dev/null 2>&1
    fi
  done
  pkill -f "$REPO/target/debug/qsync mount" 2>/dev/null
  sleep 1
}
# ★ 必须在 cleanup_stale 定义之后再调用（以前写在定义前，每次跑都报「未找到命令」，
#   上一次崩溃留下的挂载点就清不掉了）
cleanup_stale

# 卸载带退避重试：FUSE 会话可能正卡在一次慢下载上，fusermount 偶尔会忙
unmount_retry() {
  local mp="$1" i
  for i in 1 2 3 4 5; do
    is_mounted "$mp" || return 0
    "$QS" umount "$mp" >/dev/null 2>&1 || fusermount3 -u "$mp" >/dev/null 2>&1
    sleep 1
  done
  if is_mounted "$mp"; then
    # 最后手段：杀掉本仓库里持有这个挂载点的 mount 进程（只匹配本仓库路径）
    echo "  ⚠️  $mp 卸载不掉，杀掉对应 mount 进程"
    pkill -f "$REPO/target/debug/qsync mount $mp" 2>/dev/null
    sleep 1
    fusermount3 -u "$mp" >/dev/null 2>&1
    sleep 1
  fi
  ! is_mounted "$mp"
}

cleanup() {
  if [ "$KEEP" = "0" ]; then unmount_retry "$MNT" >/dev/null 2>&1; fi
}
trap cleanup EXIT

echo "=== 挂载（只读 + on-demand，60s 水合超时）==="
rm -f "$CACHE"/*
"$QS" mount "$MNT" --remote "${QSYNC_REMOTE_ROOT:-/home}" --cache-dir "$CACHE" \
      --threads 4 --auto-unmount ${QSYNC_MOUNT_EXTRA:-} >"$LOG" 2>&1 &
MOUNT_PID=$!
for _ in $(seq 1 30); do is_mounted "$MNT" && break; sleep 0.5; done
is_mounted "$MNT" || { echo "挂载失败，日志："; cat "$LOG"; exit 1; }
echo "  mounted: $(awk -v mp="$MNT" '$2==mp' /proc/mounts | head -1)"

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
echo "=== M2 区间水合：head -c 100 只下 1 个 128 KiB 区间 ==="
head -c 100 "$M/big.bin" >"$RUNDIR/head100.bin" 2>/dev/null
if cmp -s "$RUNDIR/head100.bin" <(head -c 100 "$LOCAL_FIXTURE/big.bin"); then
  ok "head -c 100 数据正确"
else
  bad "head -c 100 数据不正确"
fi
bigcache=$(ls -S "$CACHE" 2>/dev/null | head -1)
if [ -n "$bigcache" ]; then
  apparent=$(stat -c %s "$CACHE/$bigcache")
  alloc_kib=$(( $(stat -c %b "$CACHE/$bigcache") / 2 ))
  echo "  缓存 $bigcache: apparent=${apparent}B allocated=${alloc_kib}KiB"
  check "$([ "$apparent" = "134217728" ] && echo 0 || echo 1)" "稀疏缓存 apparent size = 文件大小（128 MiB）"
  # 128 KiB 区间 + 文件系统开销，宽限到 300 KiB；M1 的整文件水合会是 128 MiB
  check "$([ "$alloc_kib" -le 300 ] && echo 0 || echo 1)" "只下载了 1 个区间（allocated ≤ 300 KiB，M1 整文件会是 131072 KiB）"
else
  bad "缓存目录里没有 big.bin 的缓存文件"
fi

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
sys.exit(0 if 'user.qsync.state' in names and 'user.qsync.vsize' in names and 'user.qsync.chunks' in names else 1)
" && echo 0 || echo 1)" "listxattr 返回全部键（含末尾 NUL 校验）"
chunks=$(python3 -c "
import os
print(os.getxattr('$M/big.bin','user.qsync.chunks').decode())
")
echo "  big.bin 区间: $chunks（head -c 100 之后应是 1/1024）"
check "$([ "$chunks" = "1/1024" ] && echo 0 || echo 1)" "区间计数 = 1/1024（M2 粒度）"

echo
echo "=== #11 水合失败 → EIO，不挂死、不零填充 ==="
# 用 0 秒水合超时模拟「水合不可用」。
# 注意：M2 之后单区间只有 128 KiB，1 秒足够下完（所以老版用 1s 已拦不住），
# 0 秒是确定性的失败注入。
MNT2="$RUNDIR/mnt-timeout"
mkdir -p "$MNT2"
rm -f "$CACHE"/*          # 别让上一个挂载的缓存把这步短路
"$QS" mount "$MNT2" --remote "${QSYNC_REMOTE_ROOT:-/home}" --cache-dir "$CACHE" \
      --threads 2 --auto-unmount --hydrate-timeout 0 >>"$LOG" 2>&1 &
for _ in $(seq 1 30); do is_mounted "$MNT2" && break; sleep 0.5; done
M2="$MNT2$( echo "$FIXTURE" | sed 's#^/home##' )"
timeout 30 cat "$M2/big.bin" >"$RUNDIR/eio.out" 2>"$RUNDIR/eio.err"
rc=$?
n=$(wc -c <"$RUNDIR/eio.out")
echo "  cat 退出码=$rc 输出字节=$n  stderr=$(cat "$RUNDIR/eio.err")"
check "$([ "$rc" = "1" ] && echo 0 || echo 1)" "返回 EIO（退出码 1，而非挂死 124）"
check "$([ "$n" = "0" ] && echo 0 || echo 1)" "失败时不吐零填充假数据"
check "$(ls "$M2" >/dev/null 2>&1 && echo 0 || echo 1)" "失败后文件系统仍存活"
check "$([ -z "$(ls -A "$CACHE" | grep qsync-part)" ] && echo 0 || echo 1)" "不残留半截文件"
unmount_retry "$MNT2"

if [ "$BIG" = "1" ]; then
  echo
  echo "=== 128 MiB 整文件水合 + md5（水合超时 ${HYD}s）==="
  unmount_retry "$MNT"
  rm -f "$CACHE"/*; : >"$LOG"
  RUST_LOG=debug "$QS" mount "$MNT" --remote "${QSYNC_REMOTE_ROOT:-/home}" --cache-dir "$CACHE" \
        --threads 4 --auto-unmount --hydrate-timeout "$HYD" >>"$LOG" 2>&1 &
  for _ in $(seq 1 30); do is_mounted "$MNT" && break; sleep 0.5; done
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
  unmount_retry "$MNT"
  # 重新挂载以清掉内核 page cache，否则读到的是内核缓存，测不出并发
  rm -f "$CACHE"/* "$RUNDIR"/dedup-*.txt
  : >"$LOG"
  RUST_LOG=debug "$QS" mount "$MNT" --remote "${QSYNC_REMOTE_ROOT:-/home}" --cache-dir "$CACHE" \
        --threads 4 --auto-unmount --hydrate-timeout "$HYD" >>"$LOG" 2>&1 &
  for _ in $(seq 1 30); do is_mounted "$MNT" && break; sleep 0.5; done
  # 注意：不能裸用 wait —— 后台还挂着 FUSE 挂载进程，wait 会一直等它。
  pids=()
  for i in 1 2 3 4; do (md5sum "$M/big.bin" >"$RUNDIR/dedup-$i.txt" 2>&1) & pids+=($!); done
  for pid in "${pids[@]}"; do wait "$pid"; done
  n_ok=$(grep -c "$h1" "$RUNDIR"/dedup-*.txt | grep -c ':1$')
  n_chunks=$(grep -c "区间就绪" "$LOG")
  n_files=$(ls "$CACHE" | wc -l)
  chunks_total=$(( 134217728 / 131072 ))
  echo "  4 路 md5 一致数=$n_ok  区间下载=$n_chunks（文件共 $chunks_total 个区间）  缓存文件数=$n_files"
  check "$([ "$n_ok" = "4" ] && echo 0 || echo 1)" "4 路读取结果一致"
  # 没有去重的话 4 个读者会把 1024 个区间各取 4 次（≈4096）；留 20% 余量
  check "$([ "$n_chunks" -le $(( chunks_total * 12 / 10 )) ] && echo 0 || echo 1)" "并发读没有重复下载（区间下载 ≤ 1.2×$chunks_total）"
fi

echo
echo
echo "=== M2b 写路径（读写挂载：create / read-modify-write / mkdir / rename / move / unlink）==="
MNTW="$RUNDIR/mnt-rw"; CACHEW="$RUNDIR/cache-rw"
mkdir -p "$MNTW"; rm -rf "$CACHEW"
"$QS" mount "$MNTW" --remote "${QSYNC_REMOTE_ROOT:-/home}" --cache-dir "$CACHEW" \
      --threads 4 --rw >>"$LOG" 2>&1 &
for _ in $(seq 1 40); do is_mounted "$MNTW" && break; sleep 0.5; done
if ! is_mounted "$MNTW"; then
  bad "读写挂载失败（跳过写路径检查）"
else
  MW="$MNTW$(echo "$FIXTURE" | sed 's#^/home##')"
  # $1=dir $2=name $3=期望(1=存在,0=不存在)，最多等 30s
  wait_remote() {
    local i
    for i in $(seq 1 60); do
      if "$QS" --direct stat "$1" "$2" >/dev/null 2>&1; then
        [ "$3" = "1" ] && return 0
      else
        [ "$3" = "0" ] && return 0
      fi
      sleep 0.5
    done
    return 1
  }

  # 1) 新文件 create+write → 远端内容一致
  printf 'matrix rw test\nline2\n' >"$MW/mx-new.txt"
  wait_remote "$FIXTURE" mx-new.txt 1
  check "$([ $? = 0 ] && echo 0 || echo 1)" "新文件已上传"
  "$QS" --direct get "$FIXTURE" mx-new.txt -o "$RUNDIR/mx-new.out" >/dev/null 2>&1
  if cmp -s <(printf 'matrix rw test\nline2\n') "$RUNDIR/mx-new.out"; then
    ok "新文件内容一致"
  else
    bad "新文件内容不一致"
  fi

  # 2) ★ read-modify-write：对已存在的 10 KB 远端文件做尾部追加 + 中间改写
  python3 -c "open('$RUNDIR/rmw-src.bin','wb').write(bytes((i*37+11)&0xFF for i in range(10240)))"
  "$QS" --direct put "$RUNDIR/rmw-src.bin" "$FIXTURE" --name mx-rmw.bin >/dev/null 2>&1
  printf 'APPENDED\n' >>"$MW/mx-rmw.bin"
  python3 -c "
d=open('$MW/mx-rmw.bin','r+b'); d.seek(5000); d.write(b'MIDDLE-PATCH'); d.close()"
  wait_remote "$FIXTURE" mx-rmw.bin 1
  "$QS" --direct get "$FIXTURE" mx-rmw.bin -o "$RUNDIR/mx-rmw.out" >/dev/null 2>&1
  if python3 -c "
import sys
src=open('$RUNDIR/rmw-src.bin','rb').read()
exp=bytearray(src+b'APPENDED\n'); exp[5000:5012]=b'MIDDLE-PATCH'
got=open('$RUNDIR/mx-rmw.out','rb').read()
sys.exit(0 if got==bytes(exp) else 1)"; then
    ok "read-modify-write 正确（原内容未被清零）"
  else
    bad "read-modify-write 损坏了原内容"
  fi

  # 3) mkdir / rmdir
  mkdir "$MW/mx-dir" 2>/dev/null
  wait_remote "$FIXTURE" mx-dir 1
  check "$([ $? = 0 ] && echo 0 || echo 1)" "远端出现 mx-dir"
  rmdir "$MW/mx-dir"
  wait_remote "$FIXTURE" mx-dir 0
  check "$([ $? = 0 ] && echo 0 || echo 1)" "远端删除 mx-dir"

  # 4) 同目录改名 + 只改大小写
  mv "$MW/mx-new.txt" "$MW/mx-renamed.txt"
  wait_remote "$FIXTURE" mx-renamed.txt 1
  check "$([ $? = 0 ] && echo 0 || echo 1)" "同目录改名生效"
  mv "$MW/mx-renamed.txt" "$MW/MX-RENAMED.TXT"
  wait_remote "$FIXTURE" MX-RENAMED.TXT 1
  check "$([ $? = 0 ] && echo 0 || echo 1)" "只改大小写生效"

  # 5) 跨目录 move（实现是 move + rename 两步：FileStation move 会忽略 dest_file）
  mkdir -p "$MW/mx-a" "$MW/mx-b"; wait_remote "$FIXTURE" mx-a 1
  printf 'movable\n' >"$MW/mx-a/m.txt"
  wait_remote "$FIXTURE/mx-a" m.txt 1
  if mv "$MW/mx-a/m.txt" "$MW/mx-b/m2.txt" 2>/dev/null; then
    wait_remote "$FIXTURE/mx-b" m2.txt 1
    check "$([ $? = 0 ] && echo 0 || echo 1)" "跨目录 move + 改名生效"
    "$QS" --direct get "$FIXTURE/mx-b" m2.txt -o "$RUNDIR/mx-mv.out" >/dev/null 2>&1
    if cmp -s <(printf 'movable\n') "$RUNDIR/mx-mv.out"; then
      ok "移动后内容一致"
    else
      bad "移动后内容不一致"
    fi
  else
    bad "跨目录 move 失败"
  fi

  # 6) 清理产物
  rm -f "$MW/mx-b/m2.txt" "$MW/MX-RENAMED.TXT" "$MW/mx-rmw.bin"
  rmdir "$MW/mx-a" "$MW/mx-b" 2>/dev/null
  wait_remote "$FIXTURE" mx-rmw.bin 0
  check "$([ $? = 0 ] && echo 0 || echo 1)" "测试产物已清理"
  unmount_retry "$MNTW"
fi

echo
echo "=== M2c 变更发现（daemon 三游标轮询 + baseline 对账 + 冲突副本 + 删除保护）==="
MNTC="$RUNDIR/mnt-m2c"; CACHEC="$RUNDIR/cache-m2c"
mkdir -p "$MNTC"; rm -rf "$CACHEC"
M2C="$FIXTURE/mx2"                       # 远端沙盒目录
export QSYNC_POLL_INTERVAL=2             # 快轮询，验收更快
"$QS" daemon stop >/dev/null 2>&1
"$QS" daemon start >/dev/null 2>&1
"$QS" --direct mkdir "$FIXTURE" mx2 >/dev/null 2>&1
MC="$MNTC$(echo "$FIXTURE" | sed 's#^/home##')/mx2"

xattr_state() {  # $1=挂载点内路径 → 打印 user.qsync.state
  python3 -c "
import os
try: print(os.getxattr('$1','user.qsync.state').decode())
except OSError as e: print('ERR%d'%e.errno)"
}

if ! "$QS" mount "$MNTC" --remote "${QSYNC_REMOTE_ROOT:-/home}" --cache-dir "$CACHEC" \
        --threads 4 --rw >/dev/null 2>&1; then
  bad "M2c：daemon 读写挂载失败（跳过 M2c 检查）"
else
  # ---- M2c-1 远端改动 → 元数据刷新 + 缓存失效（下一次读拿到新内容）
  python3 -c "open('$RUNDIR/m2c-r1.bin','wb').write(b'R1-ORIGINAL\n')"
  if ! "$QS" --direct put "$RUNDIR/m2c-r1.bin" "$M2C" --name r.txt >/dev/null 2>&1; then
    echo "  ⚠️  M2c-1 前置 put 失败（真机/网络抖动），后面的断言可能连带失败"
  fi
  n1=$(wc -c <"$RUNDIR/m2c-r1.bin")
  ok1=0
  for _ in $(seq 1 60); do
    [ "$(stat -c %s "$MC/r.txt" 2>/dev/null)" = "$n1" ] && { ok1=1; break; }
    sleep 0.5
  done
  check "$([ "$ok1" = 1 ] && echo 0 || echo 1)" "M2c-1 远端新文件对挂载点可见（$n1 字节）"
  cat "$MC/r.txt" >"$RUNDIR/m2c-r1.out" 2>/dev/null     # 先水合，制造本地缓存
  python3 -c "open('$RUNDIR/m2c-r2.bin','wb').write(b'R2-REMOTE-CHANGED-0123456789\n')"
  if ! "$QS" --direct put "$RUNDIR/m2c-r2.bin" "$M2C" --name r.txt >/dev/null 2>&1; then
    echo "  ⚠️  M2c-1b 前置 put 失败（真机/网络抖动）：远端没变，刷新断言必然失败"
  fi
  n2=$(wc -c <"$RUNDIR/m2c-r2.bin")
  ok2=0
  # ★ 30s 预算：一轮对账要列 ~29 个目录（真机 WAN 下 3–7s/轮），
  #   原来的 15s 在 NAS 抖动时会假失败（M7 期间实测到过一次）。
  for _ in $(seq 1 60); do
    [ "$(stat -c %s "$MC/r.txt" 2>/dev/null)" = "$n2" ] && { ok2=1; break; }
    sleep 0.5
  done
  check "$([ "$ok2" = 1 ] && echo 0 || echo 1)" "M2c-1b 远端改动后挂载点元数据已刷新（$n1 → $n2）"
  cat "$MC/r.txt" >"$RUNDIR/m2c-r2.out" 2>/dev/null
  if cmp -s "$RUNDIR/m2c-r2.out" "$RUNDIR/m2c-r2.bin"; then
    ok "M2c-1c 缓存已失效（读到的是远端新内容，不是本地旧缓存）"
  else
    bad "M2c-1c 读到的仍是旧缓存内容"
  fi
  st=$(xattr_state "$MC/r.txt")
  check "$([ "$st" = "hydrated" ] && echo 0 || echo 1)" "M2c-1d 刷新后重新读 = hydrated（当前 $st）"

  # ---- M2c-2 远端删除 → 本地节点消失（单条删除不触发保护）
  "$QS" --direct rm "$M2C" r.txt >/dev/null 2>&1
  ok3=0
  for _ in $(seq 1 30); do
    [ ! -e "$MC/r.txt" ] && { ok3=1; break; }
    sleep 0.5
  done
  check "$([ "$ok3" = 1 ] && echo 0 || echo 1)" "M2c-2 远端删除后挂载点里节点消失"

  # ---- M2c-3 删除保护：一次对账删 5 项 > 阈值 2 → 整批挡住；--force-deletes 才放行
  "$QS" sync --interval 0 >/dev/null 2>&1        # 暂停自动轮询，手工控制节奏
  for i in 1 2 3 4 5 6; do
    printf 'keep%s\n' "$i" >"$RUNDIR/m2c-keep.bin"
    "$QS" --direct put "$RUNDIR/m2c-keep.bin" "$M2C" --name "keep$i.txt" >/dev/null 2>&1
  done
  ls "$MC" >/dev/null 2>&1                       # 让挂载点建立本地节点
  cat "$MC/keep1.txt" >/dev/null 2>&1            # keep1 水合（有本地缓存）
  "$QS" sync --once >/dev/null 2>&1              # 采纳 baseline
  for i in 1 2 3 4 5; do
    "$QS" --direct rm "$M2C" "keep$i.txt" >/dev/null 2>&1
  done
  "$QS" sync --max-deletes 2 --once >"$RUNDIR/m2c-blocked.txt" 2>&1
  echo "  删除保护：keep1 存在=$([ -e "$MC/keep1.txt" ] && echo yes || echo no) 状态=$(xattr_state "$MC/keep1.txt") 输出=$(grep -c '删除被挡' "$RUNDIR/m2c-blocked.txt")"
  check "$([ -e "$MC/keep1.txt" ] && echo 0 || echo 1)" "M2c-3 批量删除被熔断（被删的本地节点保留）"
  check "$([ "$(xattr_state "$MC/keep1.txt")" = "hydrated" ] && echo 0 || echo 1)" "M2c-3b 熔断期间本地缓存内容未被清掉"
  check "$(grep -q '删除被挡 [1-9]' "$RUNDIR/m2c-blocked.txt" && echo 0 || echo 1)" "M2c-3c 状态显示「删除被挡」"
  "$QS" sync --force-deletes --once >/dev/null 2>&1
  ok4=0
  for _ in $(seq 1 30); do
    if [ ! -e "$MC/keep1.txt" ] && [ -e "$MC/keep6.txt" ]; then ok4=1; break; fi
    sleep 0.5
  done
  check "$([ "$ok4" = 1 ] && echo 0 || echo 1)" "M2c-3d --force-deletes 放行后删掉 5 项、保留未删的 keep6"
  "$QS" sync --max-deletes 50 >/dev/null 2>&1

  # ---- M2c-4 冲突副本：远端/本地都改 → 远端占原名，本地内容另存副本并上传
  printf 'BASE-CONTENT\n' >"$RUNDIR/m2c-cf-base.bin"
  "$QS" --direct put "$RUNDIR/m2c-cf-base.bin" "$M2C" --name cf.txt >/dev/null 2>&1
  "$QS" sync --interval 0 --once >/dev/null 2>&1     # baseline = BASE
  cat "$MC/cf.txt" >/dev/null 2>&1                    # 水合（本地有节点）
  head -c 4194304 /dev/zero | tr '\0' 'L' >"$RUNDIR/m2c-cf-local.bin"   # 4 MiB 本地改动
  cp "$RUNDIR/m2c-cf-local.bin" "$MC/cf.txt"          # 脏 + 入队上传
  wait_upload=0
  for _ in $(seq 1 240); do
    "$QS" status >"$RUNDIR/m2c-up.txt" 2>&1
    if grep -q '上传队列.*待上传 0｜上传中 no｜完成 [1-9]' "$RUNDIR/m2c-up.txt"; then wait_upload=1; break; fi
    sleep 0.5
  done
  printf 'REMOTE-AFTER\n' >"$RUNDIR/m2c-cf-remote.bin"
  "$QS" --direct put "$RUNDIR/m2c-cf-remote.bin" "$M2C" --name cf.txt >/dev/null 2>&1
  "$QS" sync --once >"$RUNDIR/m2c-conflict.txt" 2>&1
  echo "  冲突：本地已上传=$wait_upload  $(grep -o '冲突 [1-9][0-9]*' "$RUNDIR/m2c-conflict.txt" | head -1)"
  check "$([ "$wait_upload" = 1 ] && echo 0 || echo 1)" "M2c-4 本地 4 MiB 改动已上传（冲突前置）"
  check "$(grep -qE '冲突 [1-9]' "$RUNDIR/m2c-conflict.txt" && echo 0 || echo 1)" "M2c-4b 引擎识别冲突并生成副本（冲突计数 ≥ 1）"
  "$QS" --direct get "$M2C" cf.txt -o "$RUNDIR/m2c-cf-orig.out" >/dev/null 2>&1
  if cmp -s "$RUNDIR/m2c-cf-orig.out" "$RUNDIR/m2c-cf-remote.bin"; then
    ok "M2c-4c 原名保留远端内容"
  else
    bad "M2c-4c 原名内容不是远端版本"
  fi
  copy_ok=0; copy_name=""
  for _ in $(seq 1 60); do
    [ -n "$copy_name" ] || copy_name=$(ls "$MC" 2>/dev/null | grep 'conflicted copy' | head -1)
    if [ -n "$copy_name" ] && cat "$MC/$copy_name" >"$RUNDIR/m2c-cf-copy.out" 2>/dev/null \
       && cmp -s "$RUNDIR/m2c-cf-copy.out" "$RUNDIR/m2c-cf-local.bin"; then
      copy_ok=1; break
    fi
    sleep 1
  done
  check "$([ "$copy_ok" = 1 ] && echo 0 || echo 1)" "M2c-4d 冲突副本内容 = 本地改动（$copy_name）"

  # ---- M2c-5 三游标 + baseline 落盘可见
  "$QS" status >"$RUNDIR/m2c-status.txt" 2>&1
  check "$(grep -q '变更发现' "$RUNDIR/m2c-status.txt" && echo 0 || echo 1)" "M2c-5 status 显示变更发现状态"
  check "$(grep -q '游标' "$RUNDIR/m2c-status.txt" && echo 0 || echo 1)" "M2c-5b status 显示三游标"
  # ★ M5 起状态从 JSON 搬到了 SQLite（<data>/sync/<host>/sync.db），这里改查状态库
  dbfile=$(find "$RUNDIR/data" -name sync.db 2>/dev/null | head -1)
  echo "  状态库：$dbfile"
  if [ -n "$dbfile" ] && [ -f "$dbfile" ]; then
    ST=$("$QS" store --integrity --json 2>/dev/null)
    MAXLOG=$(echo "$ST" | jq -r '.cursors.max_log_seen // 0')
    NBASE=$(echo "$ST" | jq -r '.baseline_entries // 0')
    INTEG=$(echo "$ST" | jq -r '.integrity // "?"')
    if [ "${MAXLOG:-0}" -ge 1 ] && [ "${NBASE:-0}" -ge 1 ] && [ "$INTEG" = "ok" ]; then
      ok "M2c-5c 状态已落进 sync.db（max_log_seen=$MAXLOG，baseline=$NBASE 项，integrity=$INTEG）"
    else
      bad "M2c-5c 状态库内容不对（max_log_seen=$MAXLOG baseline=$NBASE integrity=$INTEG）"
    fi
    if [ -n "$(find "$RUNDIR/data" -name 'baseline.json' 2>/dev/null | head -1)" ]; then
      bad "M2c-5d 旧 baseline.json 还在（M5 起状态只写 sync.db）"
    else
      ok "M2c-5d 不再写 baseline.json（状态只在 sync.db）"
    fi
  else
    bad "M2c-5c 找不到状态库 sync.db"
  fi

  # ---- M2c 收尾：恢复轮询 + 清远端沙盒 + 卸载
  "$QS" sync --interval 2 >/dev/null 2>&1
  for n in $(ls "$MC" 2>/dev/null); do "$QS" --direct rm "$M2C" "$n" >/dev/null 2>&1; done
  "$QS" --direct rm "$FIXTURE" mx2 >/dev/null 2>&1
  unmount_retry "$MNTC"
fi
"$QS" daemon stop >/dev/null 2>&1

echo
echo "=== M3 脱水（先 inval_inode 再清内容 + 安全检查链 + 闲置/限额 + direct 模式）==="
MNT3="$RUNDIR/mnt-m3"; CACHE3="$RUNDIR/cache-m3"
mkdir -p "$MNT3"; rm -rf "$CACHE3"
unset QSYNC_CACHE_LIMIT
export QSYNC_DEHYDRATE_IDLE=0            # 先关掉自动扫描，测试要可控
export QSYNC_DEHYDRATE_INTERVAL=5
"$QS" daemon stop >/dev/null 2>&1
"$QS" daemon start >/dev/null 2>&1
M3="$MNT3$(echo "$FIXTURE" | sed 's#^/home##')"
DAEMONLOG=$(ls -t "$XDG_STATE_HOME"/qsync/log/qxyncd.log.* 2>/dev/null | head -1)

# 注意：缓存是稀疏文件，`du -sb`（apparent size）会把 128 MiB 的稀疏缓存算成 128 MiB；
# 这里要的是**真实磁盘占用**，所以用 --block-size=1 的默认（allocated）口径。
cache_bytes() { du -s --block-size=1 "$1" 2>/dev/null | cut -f1; }

if "$QS" mount "$MNT3" --remote "${QSYNC_REMOTE_ROOT:-/home}" --cache-dir "$CACHE3" \
        --threads 4 --rw >/dev/null 2>&1; then

  # ---- M3-1 水合 → 脱水 → 占位符仍显示真实大小 → 再读数据正确
  cat "$M3/hello.txt" >"$RUNDIR/m3-hello.out" 2>/dev/null
  check "$(cmp -s "$RUNDIR/m3-hello.out" "$LOCAL_FIXTURE/hello.txt" && echo 0 || echo 1)" "M3-1 读之前状态正常（水合内容一致）"
  st_before=$(xattr_state "$M3/hello.txt")
  c_before=$(cache_bytes "$CACHE3")
  "$QS" dehydrate --path "$FIXTURE/hello.txt" --force >"$RUNDIR/m3-d1.txt" 2>&1
  st_after=$(xattr_state "$M3/hello.txt")
  c_after=$(cache_bytes "$CACHE3")
  echo "  $st_before → $st_after，缓存 ${c_before}B → ${c_after}B"
  check "$([ "$st_before" = "hydrated" ] && [ "$st_after" = "placeholder" ] && echo 0 || echo 1)" "M3-1b 脱水后状态 = placeholder"
  check "$([ "$(stat -c %s "$M3/hello.txt")" = "24" ] && echo 0 || echo 1)" "M3-1c 占位符仍显示真实大小 24"
  check "$([ "${c_after:-0}" -lt "${c_before:-0}" ] && echo 0 || echo 1)" "M3-1d 本地缓存字节数下降（$c_before → $c_after）"
  cat "$M3/hello.txt" >"$RUNDIR/m3-hello2.out" 2>/dev/null
  check "$(cmp -s "$RUNDIR/m3-hello2.out" "$LOCAL_FIXTURE/hello.txt" && echo 0 || echo 1)" "M3-1e 脱水后重新读数据正确（按需水合回来了）"

  # ---- M3-2 ★ inval_inode 隔离验证：脱水后远端改成「同样长度、不同内容」，
  #          cat 必须拿到新内容 —— 若内核 page cache 没被失效，就会读到旧内容
  cat "$M3/hello.txt" >/dev/null 2>&1                     # 先水合，让 page cache 里有旧内容
  "$QS" dehydrate --path "$FIXTURE/hello.txt" --force >/dev/null 2>&1
  python3 -c "open('$RUNDIR/m3-z.bin','wb').write(b'Z'*24)"
  "$QS" --direct put "$RUNDIR/m3-z.bin" "$FIXTURE" --name hello.txt >/dev/null 2>&1
  got=$(cat "$M3/hello.txt" 2>/dev/null | tr -d '\n')
  check "$([ "$got" = "ZZZZZZZZZZZZZZZZZZZZZZZZ" ] && echo 0 || echo 1)" "M3-2 脱水让内核失效了 page cache（读到远端新内容，不是旧缓存）"
  "$QS" --direct put "$LOCAL_FIXTURE/hello.txt" "$FIXTURE" --name hello.txt >/dev/null 2>&1  # 还原 fixture
  cat "$M3/hello.txt" >/dev/null 2>&1

  # ---- M3-3 安全检查链：打开的 fd / mmap / pin
  cat "$M3/1k.bin" >/dev/null 2>&1                        # 水合
  python3 -c "import time; f=open('$M3/1k.bin','rb'); time.sleep(10)" &
  FDPID=$!
  sleep 1
  "$QS" dehydrate --path "$FIXTURE/1k.bin" --force >"$RUNDIR/m3-fd.txt" 2>&1
  kill "$FDPID" 2>/dev/null
  check "$(grep -q '打开的 fd' "$RUNDIR/m3-fd.txt" && echo 0 || echo 1)" "M3-3 有打开的 fd → 跳过脱水"
  # ★ mmap 保护：Linux 下 mmap 会给映射保留 struct file，就算进程 close(fd)，
  #   FUSE 的 release 也不会触发 → 我们的 open 计数仍然 >0（这层已经拦住了）；
  #   /proc/*/maps 扫描是第二道防线。这里验收「结果」：绝不能把 mmap 中的文件脱水。
  python3 -c "
import mmap,time
f=open('$M3/1k.bin','rb'); m=mmap.mmap(f.fileno(),0,prot=mmap.PROT_READ); f.close(); time.sleep(10)" &
MPID=$!
  sleep 1
  "$QS" dehydrate --path "$FIXTURE/1k.bin" --force >"$RUNDIR/m3-mmap.txt" 2>&1
  st_mmap=$(xattr_state "$M3/1k.bin")
  kill "$MPID" 2>/dev/null
  echo "  mmap 中脱水结果: $st_mmap｜$(grep -oE '(还有打开的 fd|有进程 mmap 了它)' "$RUNDIR/m3-mmap.txt" | head -1)"
  check "$([ "$st_mmap" = "hydrated" ] && echo 0 || echo 1)" "M3-3b 被 mmap 的文件不会被脱水（fd 计数 / /proc 扫描兜底）"
  "$QS" pin "$FIXTURE/1k.bin" pinned >/dev/null 2>&1
  "$QS" dehydrate --path "$FIXTURE/1k.bin" --force >"$RUNDIR/m3-pin.txt" 2>&1
  check "$(grep -q 'pinned' "$RUNDIR/m3-pin.txt" && echo 0 || echo 1)" "M3-3c pin=pinned → 跳过脱水"
  "$QS" pin "$FIXTURE/1k.bin" unpinned >/dev/null 2>&1
  "$QS" dehydrate --path "$FIXTURE/1k.bin" --force >"$RUNDIR/m3-pin2.txt" 2>&1
  check "$(grep -qE '脱水 1 个|脱水 [1-9]' "$RUNDIR/m3-pin2.txt" && echo 0 || echo 1)" "M3-3d unpin 后可以脱水"

  # ---- M3-4 dirty / 在途上传 → 跳过；上传落地（success hook 清 dirty）后可以脱水
  head -c 16777216 /dev/zero | tr '\0' 'D' >"$RUNDIR/m3-16m.bin"
  cp "$RUNDIR/m3-16m.bin" "$M3/m3-big.bin"                   # 16 MiB 本地新文件 → 入队上传
  "$QS" dehydrate --path "$FIXTURE/m3-big.bin" --force >"$RUNDIR/m3-dirty.txt" 2>&1
  echo "  立即脱水（上传在途）: $(grep -oE '跳过 [0-9]+ 个' "$RUNDIR/m3-dirty.txt" | head -1) $(grep -oE '(有未上传的本地改动|上传队列里还有该文件的作业)' "$RUNDIR/m3-dirty.txt" | head -1)"
  check "$(grep -qE '未上传的本地改动|上传队列里还有该文件的作业' "$RUNDIR/m3-dirty.txt" && echo 0 || echo 1)" "M3-4 有未上传改动/在途上传 → 跳过脱水"
  upok=0
  for _ in $(seq 1 240); do
    if "$QS" status 2>/dev/null | grep -q '上传队列.*待上传 0｜上传中 no'; then upok=1; break; fi
    sleep 0.5
  done
  check "$([ "$upok" = 1 ] && echo 0 || echo 1)" "M3-4b 16 MiB 上传完成（上传队列排空）"
  "$QS" dehydrate --path "$FIXTURE/m3-big.bin" --force >"$RUNDIR/m3-dirty2.txt" 2>&1
  check "$(grep -qE '脱水 [1-9]' "$RUNDIR/m3-dirty2.txt" && echo 0 || echo 1)" "M3-4c 上传成功后 dirty 已清 → 可以脱水（success hook 生效）"
  "$QS" --direct rm "$FIXTURE" m3-big.bin >/dev/null 2>&1

  # ---- M3-5 限额：按 LRU 清到不超限
  head -c 1048576 "$M3/big.bin" >/dev/null 2>&1              # 水合 1 MiB（8 个区间）
  cat "$M3/1k.bin" >/dev/null 2>&1
  cat "$M3/hello.txt" >/dev/null 2>&1
  sleep 2   # 让刚读完的 readahead / 内核 file 引用落定（真实场景不会毫秒级紧接着脱水）
  c_before=$(cache_bytes "$CACHE3")
  "$QS" dehydrate --cache-limit 256K --force >"$RUNDIR/m3-limit.txt" 2>&1
  c_after=$(cache_bytes "$CACHE3")
  echo "  限额 256K：缓存 ${c_before}B → ${c_after}B｜$(grep -oE '释放 [^；]*' "$RUNDIR/m3-limit.txt" | head -1)"
  check "$([ "${c_after:-0}" -lt "${c_before:-0}" ] && echo 0 || echo 1)" "M3-5 限额触发批量脱水（$c_before → $c_after）"
  check "$([ "${c_after:-0}" -le 400000 ] && echo 0 || echo 1)" "M3-5b 清到了限额附近（≤ 400 KiB 含开销）"

  # ---- M3-6 后台自动扫描（动态开启 idle 脱水）
  cat "$M3/hello.txt" >/dev/null 2>&1                        # 重新水合
  "$QS" dehydrate --idle-secs 1 >/dev/null 2>&1              # 动态开启后台扫描（闲置 ≥1s）
  sleep 9
  st_auto=$(xattr_state "$M3/hello.txt")
  check "$([ "$st_auto" = "placeholder" ] && echo 0 || echo 1)" "M3-6 后台定时脱水生效（闲置 ≥1s 后被自动清成占位符）"
  check "$(grep -qh '自动脱水：' "$XDG_STATE_HOME"/qsync/log/qxyncd.log.* && echo 0 || echo 1)" "M3-6b daemon 日志有自动脱水记录"
  "$QS" dehydrate --idle-secs 0 >/dev/null 2>&1              # 关掉，别影响 Direct 模式测试

  # ---- M3-7 status 可观测
  "$QS" status >"$RUNDIR/m3-status.txt" 2>&1
  check "$(grep -q '本地缓存' "$RUNDIR/m3-status.txt" && echo 0 || echo 1)" "M3-7 status 显示本地缓存/脱水信息"
  check "$(grep -qE '累计脱水 [1-9]' "$RUNDIR/m3-status.txt" && echo 0 || echo 1)" "M3-7b status 显示累计脱水次数"
  unmount_retry "$MNT3"
else
  bad "M3：daemon 读写挂载失败（跳过脱水检查）"
fi

# ---- M3-8 direct 模式（绕过 page cache）：数据正确 + mmap 不可用
MNT3D="$RUNDIR/mnt-m3-direct"; CACHE3D="$RUNDIR/cache-m3-direct"
mkdir -p "$MNT3D"; rm -rf "$CACHE3D"
if "$QS" mount "$MNT3D" --remote "${QSYNC_REMOTE_ROOT:-/home}" --cache-dir "$CACHE3D" \
        --threads 2 --cache-mode direct >/dev/null 2>&1; then
  M3D="$MNT3D$(echo "$FIXTURE" | sed 's#^/home##')"
  check "$(cmp -s <(cat "$M3D/1k.bin" 2>/dev/null) "$LOCAL_FIXTURE/1k.bin" && echo 0 || echo 1)" "M3-8 direct 模式读取正确"
  check "$(python3 -c "
import mmap,sys
f=open('$M3D/1k.bin','rb')
try:
    mmap.mmap(f.fileno(),0,prot=mmap.PROT_READ); sys.exit(1)
except OSError:
    sys.exit(0)
" && echo 0 || echo 1)" "M3-8b direct 模式 mmap 不可用（FOPEN_DIRECT_IO 生效）"
  unmount_retry "$MNT3D"
else
  bad "M3：direct 模式挂载失败"
fi

"$QS" daemon stop >/dev/null 2>&1

echo "=== 卸载干净（无残留）==="
if [ "$KEEP" = "0" ]; then
  unmount_retry "$MNT"
  check "$(is_mounted "$MNT" && echo 1 || echo 0)" "卸载后无残留挂载"
  grep -E "unmount: 水合" "$LOG" | tail -1 | sed 's/^/  /'
fi

echo
echo "================ 结果: 通过 $PASS / 失败 $FAIL ================"
[ "$FAIL" = "0" ]
