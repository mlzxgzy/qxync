#!/usr/bin/env bash
# m21-real-nas.sh —— ★ M21「离线可重启 daemon」的**真机**验收（需要真 NAS + FUSE）
#
# 你正在看的是仓库里的可复现版本：不含任何口令，先设环境变量再跑（见下面 SKIP 段）。
#   QXNYC_REAL_HOST=... QXNYC_REAL_USER=... QXNYC_REAL_PASSWORD=... \
#     xtask/tests/m21-real-nas.sh
#
# 验的链路（M20 验收的「已知限制」正是这里要消灭的）：
#   真 NAS ←TCP 中继← daemon/FUSE；杀中继 = 拔网线。
#   在线：写文件、上传、水合 remote-only.bin（清单 + 缓存落盘）。
#   断网：离线 mkdir + 写文件（进缓冲区）。
#   ★ 离线重启 daemon（M20 时挂载点要等联网；M21 起必须立即恢复）：
#     · 挂载点从 mounts.json 恢复（无会话挂载）；
#     · `ls` 吃 sync.db 里持久化的清单；
#     · `cat` 已水合文件走缓存认领（含自定义 cache-dir 的恢复修复）；
#     · 离线继续写（进缓冲区）；
#     · 再重启一次仍然幂等。
#   恢复网络：自动重登（sid 热推到恢复的挂载点）→ 回放 outbox → 上传 → 远端状态正确。
#
# 所有状态库 / 缓存 / 挂载点都在 .local-run/m21-real-nas 下，不碰真实用户数据；
# 远端只动 $QXNYC_REAL_ROOT（默认 /home/qxync-m21-e2e，跑完删除）。
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ROOT="$REPO/.local-run/m21-real-nas"
QX="$REPO/target/debug/qxync"
QD="$REPO/target/debug/qxyncd"
if [ ! -e /dev/fuse ] || [ -z "${QXNYC_REAL_HOST:-}" ] || [ -z "${QXNYC_REAL_USER:-}" ] || [ -z "${QXNYC_REAL_PASSWORD:-}" ]; then
  echo "SKIP：需要 /dev/fuse + QXNYC_REAL_HOST / QXNYC_REAL_USER / QXNYC_REAL_PASSWORD"
  exit 0
fi
REAL_HOST="$QXNYC_REAL_HOST"
REAL_PORT="${QXNYC_REAL_PORT:-9834}"
REAL_USER="$QXNYC_REAL_USER"
REAL_PASSWORD="$QXNYC_REAL_PASSWORD"
REMOTE="${QXNYC_REAL_ROOT:-/home/qxync-m21-e2e}"
REMOTE_PARENT="${REMOTE%/*}"
REMOTE_NAME="${REMOTE##*/}"
RELAY_PORT="${QXNYC_RELAY_PORT:-18435}"
MNT="$ROOT/mnt"
CACHE="$ROOT/cache"
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
wait_mount() { # 挂载点出现在 /proc/mounts（恢复完成的标志）
  local i
  for i in $(seq 1 60); do
    mount | grep -q "on $MNT type fuse" && return 0
    sleep 0.5
  done
  return 1
}
wait_unmount() { # 挂载点从 /proc/mounts 消失（stop 的完成标志）
  local i
  for i in $(seq 1 60); do
    mount | grep -q "on $MNT type fuse" || return 0
    sleep 1
  done
  return 1
}
stop_daemon() {
  # ★ 离线时上传队列冻结着、排空要等满 120s 超时 —— 用 --force 跳过占用检查，
  #   并给足 CLI 等进程退出的时间（CLI 自身上限 260s）。
  timeout 200 "$QX" daemon stop --force >/dev/null 2>&1 || true
  DAEMON=0
  wait_unmount || fusermount3 -u "$MNT" >/dev/null 2>&1 || true
}
# 失败时转储现场：cat 的 stderr、xattr、清单行、baseline 行、.qxstate 签名
diag() { # diag <label> <remote-path> <cache-glob>
  local label="$1" rp="$2" glob="$3"
  echo "DIAG[$label] remote=$rp"
  python3 - "$DB" "$rp" "$glob" <<'PYD'
import sqlite3, json, struct, sys
db, rp, glob = sys.argv[1], sys.argv[2], sys.argv[3]
c = sqlite3.connect(db)
for d, raw in c.execute("SELECT dir, entries FROM listings"):
    for e in json.loads(raw):
        if f"{d}/{e['filename']}" == rp:
            print(f"  LISTING {rp} size={e['filesize']} mtime={e['epochmt']}")
for p, present, size, mtime in c.execute("SELECT path, present, size, mtime FROM baseline"):
    if p == rp:
        print(f"  BASELINE {rp} present={present} size={size} mtime={mtime}")
import glob as g
for p in g.glob(glob):
    d = open(p, "rb").read()
    if len(d) >= 60 and d[:8] == b"QXSTATE2":
        size = struct.unpack_from("<Q", d, 20)[0]
        mtime = struct.unpack_from("<q", d, 28)[0]
        nch = struct.unpack_from("<Q", d, 36)[0]
        done = d[60:60+nch]
        print(f"  QXSTATE {p.split('/')[-1]} size={size} mtime={mtime} done={list(done)}")
PYD
}
wait_offline() {
  local i
  for i in $(seq 1 60); do
    timeout 20 "$QX" daemon status 2>&1 | grep -q '离线' && return 0
    sleep 1
  done
  return 1
}
wait_online() {
  local i
  for i in $(seq 1 90); do
    timeout 20 "$QX" daemon status 2>&1 | grep -q '在线' && return 0
    sleep 1
  done
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
    creds["otp_secret"] = otp   # 账号开了 2SV 时必须带上（Base32 密钥，登录时现算验证码）
json.dump(creds, open(path, "w"))
PYjson
chmod 600 "$ROOT/config/qxync/credentials.json"

echo "=== 1) 起中继 + 登录真 NAS ==="
start_relay; ok $? "TCP 中继已监听 127.0.0.1:$RELAY_PORT → $REAL_HOST:$REAL_PORT"
timeout 60 "$QX" --direct login >/dev/null 2>&1; ok $? "经中继登录真 NAS 成功"

echo "=== 2) 在线基线：远端预置一个「未水合」文件 ==="
timeout 40 "$QX" --direct rm "$REMOTE_PARENT" "$REMOTE_NAME" >/dev/null 2>&1 || true
timeout 40 "$QX" --direct mkdir "$REMOTE_PARENT" "$REMOTE_NAME" >/dev/null 2>&1; ok $? "远端测试目录已建 $REMOTE"
head -c 4096 /dev/urandom > "$ROOT/remote-only.bin"
REMOTE_SHA=$(sha256sum "$ROOT/remote-only.bin" | cut -d' ' -f1)
timeout 60 "$QX" --direct put "$ROOT/remote-only.bin" "$REMOTE" >/dev/null 2>&1; ok $? "远端预置 remote-only.bin"

echo "=== 3) 起 daemon + 真挂载（rw，自定义 cache-dir —— 顺带验恢复修复） ==="
timeout 60 "$QX" daemon start >/dev/null 2>&1; DAEMON=1
timeout 120 "$QX" task add --id m21 --mountpoint "$MNT" --root "$REMOTE" \
  --read-write --cache-dir "$CACHE" >/dev/null 2>&1; ok $? "task add 挂载成功（真 FUSE）"
mount | grep -q "on $MNT type fuse" && pass "mount 表里可见 fuse.qxync（真挂载）" || fail "mount 表里可见 fuse.qxync（真挂载）"

echo "=== 4) 在线：写文件 + 水合 remote-only.bin（清单与缓存落盘） ==="
printf 'hello-m21\n' > "$MNT/hello.txt"
mkdir "$MNT/keepdir"; printf 'cached-content-xyz\n' > "$MNT/keepdir/cached.txt"
sync
wait_remote 'hello.txt' 40; ok $? "在线：hello.txt 已上传"
wait_remote 'keepdir' 40; ok $? "在线：keepdir 已上传"
for _ in $(seq 1 45); do [ -e "$MNT/remote-only.bin" ] && break; sleep 1; done
cat "$MNT/remote-only.bin" > "$ROOT/hydrated.bin" 2>/dev/null; ok $? "在线：完整读取 remote-only.bin（水合）"
sha256sum "$ROOT/hydrated.bin" | grep -q "$REMOTE_SHA" && pass "水合内容 sha256 一致" || fail "水合内容不对"
# 等一轮轮询：清单（listings 表）写穿
sleep 35
DB="$ROOT/data/qxync/sync/127.0.0.1/sync.db"
LROWS=$(python3 -c 'import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute("SELECT COUNT(*) FROM listings").fetchone()[0])' "$DB" 2>/dev/null)
[ "${LROWS:-0}" -ge 1 ] && pass "清单已持久化（listings 表 $LROWS 行）" || fail "listings 表为空（写穿没生效）"

echo "=== 5) 断网（杀中继）+ 离线操作 ==="
stop_relay
wait_offline; ok $? "daemon 已判定离线"
mkdir "$MNT/offline-dir"; ok $? "离线 mkdir offline-dir"
printf 'written-before-restart\n' > "$MNT/offline-dir/pre-restart.txt"; ok $? "离线写 pre-restart.txt（进缓冲区）"

echo "=== 6) ★ 离线重启 daemon：挂载点必须恢复（M21 核心） ==="
T0=$(date +%s)
stop_daemon
mount | grep -q "on $MNT type fuse" && fail "stop 后挂载点应已卸载" || pass "stop 后挂载点已卸载"
timeout 60 "$QX" daemon start >/dev/null 2>&1; DAEMON=1
wait_mount; RC=$?
T1=$(date +%s)
ok "$RC" "离线重启后挂载点恢复（无 NAS / 无会话）"
echo "（离线重启到挂载恢复耗时 $((T1 - T0))s）"

echo "=== 7) 恢复的挂载点离线可用（清单 + 缓存都来自磁盘） ==="
LS=$(ls -1 "$MNT" 2>&1); RC=$?
ok "$RC" "离线 ls 挂载根目录成功（清单来自 sync.db）"
echo "$LS" | grep -q 'hello.txt' && pass "根清单：hello.txt 可见" || fail "根清单缺 hello.txt：$LS"
echo "$LS" | grep -q 'keepdir' && pass "根清单：keepdir 可见" || fail "根清单缺 keepdir：$LS"
echo "$LS" | grep -q 'remote-only.bin' && pass "根清单：remote-only.bin 可见" || fail "根清单缺 remote-only.bin"
echo "$LS" | grep -q 'offline-dir' && pass "重启前离线建的目录可见" || fail "重启前离线建的目录丢了：$LS"
LSK=$(ls -1 "$MNT/keepdir" 2>&1)
echo "$LSK" | grep -q 'cached.txt' && pass "子目录清单：keepdir/cached.txt 可见" || fail "子目录清单缺失：$LSK"
GOT=$(cat "$MNT/keepdir/cached.txt" 2>/tmp/m21-cat1.err)
[ "$GOT" = "cached-content-xyz" ] && pass "读已水合文件（缓存认领，自定义 cache-dir 恢复）" || { fail "读已水合文件失败：$GOT"; echo "  cat-err: $(cat /tmp/m21-cat1.err)"; getfattr -d -m - "$MNT/keepdir/cached.txt" 2>&1 | sed 's/^/  ATTR: /'; diag "cached.txt" "$REMOTE/keepdir/cached.txt" "$CACHE/127.0.0.1/*cached.txt.qxstate"; }
GOTH=$(cat "$MNT/hello.txt" 2>/tmp/m21-cat2.err)
[ "$GOTH" = "hello-m21" ] && pass "读本地写后上传的文件（mtime 偏斜容差认领）" || { fail "读 hello.txt 失败：$GOTH"; echo "  cat-err: $(cat /tmp/m21-cat2.err)"; getfattr -d -m - "$MNT/hello.txt" 2>&1 | sed 's/^/  ATTR: /'; diag "hello.txt" "$REMOTE/hello.txt" "$CACHE/127.0.0.1/*hello.txt.qxstate"; }
cat "$MNT/remote-only.bin" > "$ROOT/after-restart.bin" 2>/dev/null; ok $? "离线重启后读 remote-only.bin 成功（水合缓存认领）"
sha256sum "$ROOT/after-restart.bin" 2>/dev/null | grep -q "$REMOTE_SHA" && pass "重启后读到的内容与原文件一致" || fail "重启后内容不对"
LSO=$(ls -1 "$MNT/offline-dir" 2>&1)
echo "$LSO" | grep -q 'pre-restart.txt' && pass "离线建的目录里重启前写的文件可见" || fail "pre-restart.txt 丢了：$LSO"
printf 'written-after-restart\n' > "$MNT/offline-dir/post-restart.txt"; ok $? "离线重启后继续写文件（进缓冲区）"
OUT=$(timeout 20 "$QX" daemon status 2>&1)
echo "$OUT" | grep -q '离线' && pass "重启后 status 显示离线" || fail "重启后 status 没显示离线"

echo "=== 8) 第二次离线重启（幂等） ==="
stop_daemon
timeout 60 "$QX" daemon start >/dev/null 2>&1; DAEMON=1
wait_mount; ok $? "第二次离线重启后挂载点再次恢复"
LS2=$(ls -1 "$MNT/offline-dir" 2>&1)
echo "$LS2" | grep -q 'post-restart.txt' && pass "两次重启间离线写的文件可见（清单写穿）" || fail "post-restart.txt 丢了：$LS2"
echo "$LS2" | grep -q 'pre-restart.txt' && pass "更早的离线文件仍在" || fail "pre-restart.txt 丢了：$LS2"
GOT2=$(cat "$MNT/hello.txt" 2>/tmp/m21-cat3.err)
[ "$GOT2" = "hello-m21" ] && pass "第二次重启后已水合文件仍可读" || { fail "第二次重启后读失败：$GOT2"; echo "  cat-err: $(cat /tmp/m21-cat3.err)"; diag "hello.txt-2nd" "$REMOTE/hello.txt" "$CACHE/127.0.0.1/*hello.txt.qxstate"; }

echo "=== 9) 恢复网络：重登（sid 热推）→ 回放 → 上传 ==="
start_relay; ok $? "中继已恢复"
wait_online; ok $? "daemon 恢复在线（探活 + 自动重登）"
wait_remote 'offline-dir' 90; ok $? "回放：远端出现 offline-dir"
# pre/post-restart.txt 在 offline-dir 里 —— ls 子目录，不是根目录
wait_remote_sub() { # wait_remote_sub <dir> <name> <timeout-sec>
  local d="$1" n="$2" t="$3" i
  for i in $(seq 1 "$t"); do
    timeout 40 "$QX" --direct ls "$REMOTE/$d" 2>/dev/null | grep -q "$n" && return 0
    sleep 1
  done
  return 1
}
wait_remote_sub offline-dir pre-restart.txt 90; ok $? "上传：pre-restart.txt 到达远端"
wait_remote_sub offline-dir post-restart.txt 90; ok $? "上传：post-restart.txt 到达远端"
GOTP=$(timeout 40 "$QX" --direct get "$REMOTE/offline-dir" pre-restart.txt -o "$ROOT/got-pre.txt" >/dev/null 2>&1; cat "$ROOT/got-pre.txt" 2>/dev/null)
[ "$GOTP" = "written-before-restart" ] && pass "pre-restart.txt 内容正确" || fail "pre-restart.txt 内容=$GOTP"
remote_ls | grep -q 'remote-only' && pass "远端原有文件未被破坏" || fail "远端原有文件被破坏"
OUTBOX_LEFT=$(python3 -c 'import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute("SELECT COUNT(*) FROM outbox").fetchone()[0])' "$DB")
[ "$OUTBOX_LEFT" = "0" ] && pass "回放完成：outbox 清空" || fail "outbox 还有 $OUTBOX_LEFT 行"
OUT=$(timeout 20 "$QX" daemon status 2>&1)
echo "$OUT" | grep -qE '待上传 [1-9]' && fail "上传队列仍有待上传" || pass "上传队列已清空"

echo "=== 10) 清理 ==="
timeout 40 "$QX" --direct rm "$REMOTE_PARENT" "$REMOTE_NAME" >/dev/null 2>&1; ok $? "远端测试目录已清理"
timeout 120 "$QX" daemon stop --force >/dev/null 2>&1; DAEMON=0
stop_relay

echo
echo "================ M21 真机 FUSE 验收（离线可重启） ================"
echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" = "0" ] && echo "结果：全部通过" || echo "结果：有失败项"
exit $([ "$FAIL" = "0" ] && echo 0 || echo 1)
