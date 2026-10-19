#!/usr/bin/env bash
# m20-real-nas.sh —— ★ M20「离线可用 / 累计缓冲区」的**真机**验收（需要真 NAS + FUSE）
#
# 你正在看的是仓库里的可复现版本：不含任何口令，先设环境变量再跑（见下面 SKIP 段）。
#   QXNYC_REAL_HOST=... QXNYC_REAL_USER=... QXNYC_REAL_PASSWORD=... \
#     xtask/tests/m20-real-nas.sh
#
# 验的链路（全部在真机 + 真挂载点上）：
#   真 NAS ←TCP 中继← daemon/FUSE；杀中继 = 拔网线（服务端协议不变，只断传输层）。
#   断网后：mkdir / rename / 建文件 / touch / ls / rm 全部在挂载点上照常；
#   改动进 outbox + 上传/删除队列；daemon 重启不丢；恢复网络后按 seq 回放，
#   远端出现对应的目录/改名/内容/删除/mtime，缓冲区清零。
#
# 所有状态库 / 缓存 / 挂载点都在 .local-run/m20-real-nas 下，不碰真实用户数据；
# 远端只动 $QXNYC_REAL_ROOT（默认 /home/qxync-m20-e2e，跑完删除）。
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ROOT="$REPO/.local-run/m20-real-nas"
QX="$REPO/target/debug/qxync"
QD="$REPO/target/debug/qxyncd"
# ★ 仓库版本只放参数，**不含任何口令**。跑之前设：
#   QXNYC_REAL_HOST / QXNYC_REAL_USER / QXNYC_REAL_PASSWORD（必填）
#   QXNYC_REAL_PORT（默认 9834）、QXNYC_REAL_ROOT（默认 /home/qxync-m20-e2e）
#   QXNYC_RELAY_PORT（本地中继端口，默认 18434）
# 缺参数 / 没有 /dev/fuse 时直接 SKIP（CI 与普通开发机不会误跑）。
if [ ! -e /dev/fuse ] || [ -z "${QXNYC_REAL_HOST:-}" ] || [ -z "${QXNYC_REAL_USER:-}" ] || [ -z "${QXNYC_REAL_PASSWORD:-}" ]; then
  echo "SKIP：需要 /dev/fuse + QXNYC_REAL_HOST / QXNYC_REAL_USER / QXNYC_REAL_PASSWORD"
  exit 0
fi
REAL_HOST="$QXNYC_REAL_HOST"
REAL_PORT="${QXNYC_REAL_PORT:-9834}"
REAL_USER="$QXNYC_REAL_USER"
REAL_PASSWORD="$QXNYC_REAL_PASSWORD"
REMOTE="${QXNYC_REAL_ROOT:-/home/qxync-m20-e2e}"
REMOTE_PARENT="${REMOTE%/*}"
REMOTE_NAME="${REMOTE##*/}"
RELAY_PORT="${QXNYC_RELAY_PORT:-18434}"
MNT="$ROOT/mnt"
MTIME=1767294245   # 2026-01-02 03:04:05 +08:00
PASS=0; FAIL=0; RELAY_PID=""; DAEMON=0

pass() { echo "PASS $1"; PASS=$((PASS + 1)); }
fail() { echo "FAIL $1"; FAIL=$((FAIL + 1)); }
ok() { if [ "$1" = "0" ]; then pass "$2"; else fail "$2"; fi; }

export XDG_CONFIG_HOME="$ROOT/config" XDG_DATA_HOME="$ROOT/data" \
       XDG_STATE_HOME="$ROOT/state" XDG_RUNTIME_DIR="$ROOT/run"
unset QXNYC_HOST QXNYC_USER QXNYC_PASSWORD QXNYC_OTP_SECRET QXNYC_OTP_CODE QXNYC_SOCKET
mkdir -p "$ROOT/run"; chmod 700 "$ROOT/run"

stop_relay() {
  if [ -n "$RELAY_PID" ]; then
    kill "$RELAY_PID" 2>/dev/null || true
    wait "$RELAY_PID" 2>/dev/null || true
    RELAY_PID=""
  fi
}
start_relay() {
  python3 "$ROOT/relay.py" "$RELAY_PORT" "$REAL_HOST" "$REAL_PORT" >"$ROOT/relay.log" 2>&1 &
  RELAY_PID=$!
  for _ in $(seq 1 50); do
    python3 - "$RELAY_PORT" <<'PY' && return 0
import socket, sys
s = socket.socket(); s.settimeout(0.3)
try:
    s.connect(("127.0.0.1", int(sys.argv[1]))); print("relay up")
except Exception:
    sys.exit(1)
PY
    sleep 0.2
  done
  echo "relay 启动失败"; return 1
}
cleanup() {
  stop_relay
  if [ "$DAEMON" = "1" ]; then timeout 120 "$QX" daemon stop --force >/dev/null 2>&1 || true; fi
}
trap cleanup EXIT

remote_ls() { timeout 40 "$QX" --direct ls "$REMOTE" 2>&1; }
wait_remote() { # wait_remote <grep-pattern> <timeout-sec>
  local pat="$1" t="$2" i
  for i in $(seq 1 "$t"); do remote_ls | grep -q "$pat" && return 0; sleep 1; done
  return 1
}

echo "=== 0) 准备隔离环境 ==="
rm -rf "$ROOT"
mkdir -p "$ROOT"/{config/qxync/links,data,state,run,local,mnt,cache}
chmod 700 "$ROOT/run"
cat > "$ROOT/relay.py" <<'PY'
import asyncio, sys
LISTEN_PORT, TARGET_HOST, TARGET_PORT = int(sys.argv[1]), sys.argv[2], int(sys.argv[3])

async def pipe(r, w):
    try:
        while (data := await r.read(65536)):
            w.write(data); await w.drain()
    except Exception:
        pass
    finally:
        try: w.close()
        except Exception: pass

async def handle(cr, cw):
    try:
        ur, uw = await asyncio.open_connection(TARGET_HOST, TARGET_PORT)
    except Exception:
        cw.close(); return
    await asyncio.gather(pipe(cr, uw), pipe(ur, cw))

async def main():
    server = await asyncio.start_server(handle, "127.0.0.1", LISTEN_PORT)
    print(f"relay 127.0.0.1:{LISTEN_PORT} -> {TARGET_HOST}:{TARGET_PORT}", flush=True)
    async with server:
        await server.serve_forever()
asyncio.run(main())
PY
python3 - "$ROOT/config/qxync/links/default.json" "$RELAY_PORT" "$REAL_USER" <<'PYjson'
import json, sys
path, port, user = sys.argv[1], int(sys.argv[2]), sys.argv[3]
json.dump({"id":"default","host":"127.0.0.1","port":port,"https":True,"insecure":True,
           "user":user,"ipv4_only":True}, open(path, "w"))
PYjson
python3 - "$ROOT/config/qxync/credentials.json" "$REAL_USER" "$REAL_PASSWORD" "${QXNYC_REAL_OTP_SECRET:-}" <<'PYjson'
import json, sys
path, user, password, otp = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
creds = {"host":"127.0.0.1","user":user,"password":password}
if otp:
    creds["otp_secret"] = otp   # ★ M21：账号开了 2SV 时必须带上（与 m21-real-nas.sh 同源）
json.dump(creds, open(path, "w"))
PYjson
chmod 600 "$ROOT/config/qxync/credentials.json"

echo "=== 1) 起中继 + 登录真 NAS ==="
start_relay; ok $? "TCP 中继已监听 127.0.0.1:$RELAY_PORT → qnap.kdajv.com:9834"
timeout 60 "$QX" --direct login >/dev/null 2>&1; ok $? "经中继登录真 NAS 成功"

echo "=== 2) 在线基线：远端预置一个「未水合」文件 ==="
timeout 40 "$QX" --direct rm "$REMOTE_PARENT" "$REMOTE_NAME" >/dev/null 2>&1 || true
timeout 40 "$QX" --direct mkdir "$REMOTE_PARENT" "$REMOTE_NAME" >/dev/null 2>&1; ok $? "远端测试目录已建 $REMOTE"
head -c 4096 /dev/urandom > "$ROOT/remote-only.bin"
REMOTE_ONLY_SHA=$(sha256sum "$ROOT/remote-only.bin" | cut -d' ' -f1)
timeout 60 "$QX" --direct put "$ROOT/remote-only.bin" "$REMOTE" >/dev/null 2>&1; ok $? "远端预置 remote-only.bin（本地不下载）"

echo "=== 3) 起 daemon + 真挂载（rw） ==="
timeout 60 "$QX" daemon start >/dev/null 2>&1; DAEMON=1
timeout 120 "$QX" task add --id m20 --mountpoint "$MNT" --root "$REMOTE" \
  --read-write --cache-dir "$ROOT/cache" >/dev/null 2>&1; ok $? "task add 挂载成功（真 FUSE）"
mount | grep -q "on $MNT type fuse" && pass "mount 表里可见 fuse.qxync（真挂载）" || fail "mount 表里可见 fuse.qxync（真挂载）"

echo "=== 4) 在线写：文件经挂载点上传 ==="
printf 'hello-offline-buffer\n' > "$MNT/hello.txt"
mkdir "$MNT/keepdir"; printf 'cached-content-abc\n' > "$MNT/keepdir/cached.txt"
printf 'delete-me-later\n' > "$MNT/deleteme.txt"
sync
wait_remote 'hello.txt' 40; ok $? "在线：hello.txt 已上传到 NAS"
wait_remote 'keepdir' 40; ok $? "在线：keepdir 已上传到 NAS"
wait_remote 'deleteme.txt' 40; ok $? "在线：deleteme.txt 已上传到 NAS"

# 等一轮轮询，让挂载点看到远端预置的 remote-only.bin（不水合内容）
for _ in $(seq 1 45); do [ -e "$MNT/remote-only.bin" ] && break; sleep 1; done
[ -e "$MNT/remote-only.bin" ] && pass "挂载点已看到远端预置文件（未水合）" || fail "挂载点已看到远端预置文件（未水合）"
getfattr -d -m - "$MNT/remote-only.bin" 2>/dev/null | grep -q 'user.qxync.state="placeholder"' \
  && pass "remote-only.bin 是 placeholder（内容不在本地）" || fail "remote-only.bin 是 placeholder（内容不在本地）"

echo "=== 5) 断网（杀中继，等价拔网线） ==="
stop_relay
T0=$(date +%s)
for _ in $(seq 1 60); do
  S=$(date +%s); OUT=$(timeout 20 "$QX" daemon status 2>&1); EL=$(( $(date +%s) - S ))
  echo "$OUT" | grep -q '离线' && break
  sleep 1
done
DET=$(( $(date +%s) - T0 ))
echo "（离线判定耗时 ${DET}s；status 单次 ${EL:-?}s）"
[ "$DET" -lt 60 ] && pass "daemon 在 ${DET}s 内判定离线" || fail "daemon 在 60s 内判定离线"
[ "${EL:-99}" -lt 5 ] && pass "离线时 status 仍然秒回（${EL}s < 5s）" || fail "离线时 status 快速返回（实际 ${EL}s）"

echo "=== 6) 离线操作（真挂载点上） ==="
mkdir "$MNT/offline-dir"; ok $? "离线 mkdir offline-dir"
mv "$MNT/hello.txt" "$MNT/hello-renamed.txt"; ok $? "离线 rename hello.txt → hello-renamed.txt"
printf 'written-while-offline\n' > "$MNT/offline-dir/newfile.txt"; ok $? "离线在新建目录里写文件（旧版回 EIO）"
LS_OUT=$(ls -la "$MNT/offline-dir" 2>&1); [ $? = 0 ] && pass "离线 ls 新建目录（旧版回 EIO）" || fail "离线 ls 新建目录：$LS_OUT"
mkdir "$MNT/offline-dir/sub"; ok $? "离线建二级目录 offline-dir/sub"
printf 'nested\n' > "$MNT/offline-dir/sub/nested.txt"; ok $? "离线往二级目录里写文件"
mv "$MNT/offline-dir" "$MNT/offline-dir-moved"; ok $? "离线改名目录 offline-dir → offline-dir-moved"
LS2=$(ls -la "$MNT/offline-dir-moved" 2>&1); [ $? = 0 ] && pass "离线改名后 ls 新目录名（清单快照跟着搬）" || fail "离线改名后 ls 新目录名：$LS2"
printf 'after-move\n' > "$MNT/offline-dir-moved/moved.txt"; ok $? "离线往改名后的目录里写文件"
rm "$MNT/deleteme.txt"; ok $? "离线删除文件 deleteme.txt"
touch -m -d '@'"$MTIME" "$MNT/keepdir/cached.txt"; ok $? "离线 touch 修改 mtime"
[ "$(stat -c %Y "$MNT/keepdir/cached.txt")" = "$MTIME" ] && pass "离线 mtime 本地生效" || fail "离线 mtime 本地生效"
GOT=$(cat "$MNT/hello-renamed.txt" 2>/dev/null); [ "$GOT" = "hello-offline-buffer" ] \
  && pass "离线读已水合文件内容正确" || fail "离线读已水合文件：$GOT"
S=$(date +%s%N); timeout 25 cat "$MNT/remote-only.bin" >/dev/null 2>&1; RC=$?; E=$(( ($(date +%s%N)-S)/1000000 ))
[ "$RC" != "0" ] && [ "$E" -lt 2000 ] && pass "离线读未水合文件立即失败（${E}ms < 2s，旧版卡 20s）" \
  || fail "离线读未水合文件应快速失败（rc=$RC ${E}ms）"
X=$(getfattr -d -m - "$MNT/hello-renamed.txt" 2>/dev/null)
echo "$X" | grep -q 'user.qxync.offline="1"' && pass "离线 xattr user.qxync.offline=1" || fail "离线 xattr user.qxync.offline=1"

echo "=== 7) 离线缓冲区可见性 ==="
OUT=$(timeout 20 "$QX" daemon status 2>&1)
echo "$OUT" | grep -q '离线' && pass "status 显示离线" || fail "status 显示离线"
echo "$OUT" | grep -E '离线缓冲' | head -2
echo "$OUT" | grep -qE '待建目录 [1-9]' && pass "status 显示待建目录计数" || fail "status 显示待建目录计数"
echo "$OUT" | grep -qE '待改名 [1-9]' && pass "status 显示待改名计数" || fail "status 显示待改名计数"
echo "$OUT" | grep -qE '待改 mtime [1-9]' && pass "status 显示待改 mtime 计数" || fail "status 显示待改 mtime 计数"
echo "$OUT" | grep -qE '待上传 [1-9]' && pass "status 显示待上传计数" || fail "status 显示待上传计数"

DB="$ROOT/data/qxync/sync/127.0.0.1/sync.db"
ROWS=$(python3 - "$DB" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
rows = c.execute("select kind, path, attempts from outbox order by seq").fetchall()
print("|".join(f"{k}:{p}:{a}" for k, p, a in rows))
PY
)
echo "（outbox: $ROWS）"
echo "$ROWS" | grep -q 'mkdir:.*offline-dir-moved:' && pass "outbox 落库 mkdir 且目录改名后路径已回写（offline-dir-moved）" || fail "outbox 落库 mkdir offline-dir-moved"
echo "$ROWS" | grep -q 'rename:.*hello.txt' && pass "outbox 落库 rename hello.txt" || fail "outbox 落库 rename hello.txt"
echo "$ROWS" | grep -q 'setmtime:.*cached.txt' && pass "outbox 落库 setmtime cached.txt" || fail "outbox 落库 setmtime cached.txt"
echo "$OUT" | grep -qE '待删 [1-9]' && pass "status 显示待删除 1（删除队列离线冻结）" || fail "status 显示待删除计数"
echo "$ROWS" | grep -qE ':[1-9]' && fail "网络错误不应消耗重试次数（attempts>0）" || pass "网络错误没有消耗重试次数（attempts 全 0）"

echo "=== 8) 离线重启 daemon：缓冲区不丢 ==="
timeout 120 "$QX" daemon stop --force >/dev/null 2>&1
timeout 60 "$QX" daemon start >/dev/null 2>&1; DAEMON=1
sleep 4
S=$(date +%s); OUT=$(timeout 20 "$QX" daemon status 2>&1); EL=$(( $(date +%s) - S ))
echo "$OUT" | grep -q '离线' && pass "重启后仍判定离线" || fail "重启后仍判定离线"
if mount | grep -q "$MNT"; then
  echo "（离线重启后挂载点竟然恢复了：超出当前版本能力，记下来）"
else
  echo "（已知限制：离线重启后挂载点要等联网恢复；缓冲区本身不丢）"
fi
[ "$EL" -lt 5 ] && pass "重启后离线 status 仍然秒回（${EL}s）" || fail "重启后离线 status 快速返回"
ROWS2=$(python3 - "$DB" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
print(len(c.execute("select 1 from outbox").fetchall()))
PY
)
N1=$(echo "$ROWS" | awk -F'|' '{print NF}')
[ "$ROWS2" = "$N1" ] && pass "重启后 outbox 行数不变（$ROWS2 行）" || fail "重启后 outbox 行数：$ROWS2 vs $N1"
python3 - "$DB" <<'PY' && [ $? = 0 ] && pass "重启后 outbox attempts 仍全为 0" || fail "重启后 outbox attempts 仍全为 0"
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
assert all(a == 0 for (a,) in c.execute("select attempts from outbox")), "attempts != 0"
PY

echo "=== 9) 恢复网络：自动回放 ==="
start_relay; ok $? "中继已恢复"
S=$(date +%s)
for _ in $(seq 1 60); do echo "$(timeout 20 "$QX" daemon status 2>&1)" | grep -q '在线' && break; sleep 1; done
DET=$(( $(date +%s) - S )); echo "（恢复在线耗时 ${DET}s）"
wait_remote 'hello-renamed.txt' 90; ok $? "回放：远端出现 hello-renamed.txt"
wait_remote 'offline-dir-moved' 90; ok $? "回放：远端出现 offline-dir-moved（目录改名已回放）"
remote_ls | grep -q 'offline-dir[^-]' && fail "回放：远端旧目录名 offline-dir 应已消失" || pass "回放：远端旧目录名 offline-dir 已消失"
remote_ls | grep -q 'hello\.txt' && fail "回放：远端旧名 hello.txt 应已消失" || pass "回放：远端旧名 hello.txt 已消失"
GOT=$(timeout 40 "$QX" --direct get "$REMOTE/offline-dir-moved" newfile.txt -o "$ROOT/got-newfile.txt" >/dev/null 2>&1; cat "$ROOT/got-newfile.txt" 2>/dev/null)
[ "$GOT" = "written-while-offline" ] && pass "回放：offline-dir-moved/newfile.txt 内容正确" || fail "回放：newfile.txt 内容=$GOT"
GOT2=$(timeout 40 "$QX" --direct get "$REMOTE/offline-dir-moved/sub" nested.txt -o "$ROOT/got-nested.txt" >/dev/null 2>&1; cat "$ROOT/got-nested.txt" 2>/dev/null)
[ "$GOT2" = "nested" ] && pass "回放：二级目录 offline-dir-moved/sub/nested.txt 内容正确" || fail "回放：nested.txt 内容=$GOT2"
remote_ls | grep -q 'remote-only' && pass "远端原有文件未被破坏（remote-only.bin 仍在）" || fail "远端原有文件未被破坏"
remote_ls | grep -q 'deleteme.txt' && fail "回放：离线删除应已生效（远端 deleteme.txt 还在）" || pass "回放：离线删除已生效（远端 deleteme.txt 已消失）"
MT=$(timeout 40 "$QX" --direct ls "$REMOTE/keepdir" 2>&1 | grep cached.txt)
echo "$MT" | grep -q '2026/01/02 03:04:05' && pass "回放：远端 mtime 已按离线 touch 生效" || fail "回放：远端 mtime 未生效（$MT）"
OUT=$(timeout 20 "$QX" daemon status 2>&1)
OUTBOX_LEFT=$(python3 - "$DB" <<'PY2'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
print(len(c.execute("select 1 from outbox").fetchall()))
PY2
)
[ "$OUTBOX_LEFT" = "0" ] && pass "回放完成：outbox 清空" || fail "回放完成：outbox 还有 $OUTBOX_LEFT 行"
echo "$OUT" | grep -qE '待上传 [1-9]' && fail "回放完成：上传队列仍有待上传" || pass "回放完成：上传队列已清空"
LS=$(ls -la "$MNT" 2>&1)
echo "$LS" | grep -q 'offline-dir' && pass "本地视图：offline-dir 可见" || fail "本地视图：offline-dir 可见"
echo "$LS" | grep -q 'hello-renamed.txt' && pass "本地视图：hello-renamed.txt 可见" || fail "本地视图：hello-renamed.txt 可见"

echo "=== 10) 清理 ==="
timeout 40 "$QX" --direct rm "$REMOTE_PARENT" "$REMOTE_NAME" >/dev/null 2>&1; ok $? "远端测试目录已清理"
timeout 120 "$QX" daemon stop --force >/dev/null 2>&1; DAEMON=0
stop_relay

echo
echo "================ M20 真机 FUSE 验收 ================"
echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" = "0" ] && echo "结果：全部通过" || echo "结果：有失败项"
exit $([ "$FAIL" = "0" ] && echo 0 || echo 1)
