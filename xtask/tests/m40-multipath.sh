#!/usr/bin/env bash
# m40-multipath.sh —— ★ M40「多路径连接（内网 / 公网 / DDNS）」的隔离端到端验收
#
# 验的是什么（用户原话：「做一下多路径，可能从内网或者公网访问过来，然后有些时候
# 域名可能是 ddns 的……连接不到的时候都检测一下多路径的问题」）：
#   1. 额外路径在前、主地址兜底：先走可达的那条，不碰死路径；
#   2. 一条路径挂掉后**自动切到另一条**（不重启 daemon、不丢会话）；
#   3. 优先路径恢复后**自动切回**；
#   4. `qxync routes` 能列出每条路径的 DNS 解析 / 内网-公网分类 / 延迟 / 失败原因；
#   5. DDNS 风格的主机名（localhost）能正常解析并被标注「回环」；
#   6. 路径指向另一台 NAS（UID 不同）时**身份守卫拦下并换路**，绝不把数据写错设备；
#   7. 旧配置（没有 endpoints）行为与以前一致；
#   8. daemon 不在时 `qxync routes --probe` 回落到直连探测。
#
# 不需要真实 NAS、不需要 /dev/fuse：全部用 Python 假 NAS + 隔离 XDG 目录。
# 用法：xtask/tests/m40-multipath.sh
# 依赖：cargo build --workspace（target/debug/{qxync,qxyncd}）、python3。
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
QX="$REPO/target/debug/qxync"
QD="$REPO/target/debug/qxyncd"
ROOT="$REPO/.local-run/m40-multipath"

PASS=0
FAIL=0
pass() { echo "PASS $1"; PASS=$((PASS + 1)); }
fail() { echo "FAIL $1"; FAIL=$((FAIL + 1)); }
check() { if [ "$1" = "0" ]; then pass "$2"; else fail "$2"; fi; }

if [ ! -x "$QX" ] || [ ! -x "$QD" ]; then
  echo "SKIP：没有 $QX / $QD（先 cargo build --workspace）"
  exit 0
fi
if ! command -v python3 >/dev/null; then
  echo "SKIP：需要 python3"
  exit 0
fi

rm -rf "$ROOT"
mkdir -p "$ROOT"
export XDG_CONFIG_HOME="$ROOT/config"
export XDG_DATA_HOME="$ROOT/data"
export XDG_STATE_HOME="$ROOT/state"
export XDG_RUNTIME_DIR="$ROOT/run"
unset QXNYC_SOCKET QXNYC_HOST QXNYC_USER QXNYC_PASSWORD QXNYC_OTP_SECRET QXNYC_OTP_CODE
# 探测 127.0.0.1 时不要让 CI/开发机的代理环境变量插一脚（localhost 通常有 no_proxy，
# 但不要依赖它；本测试验的是网络路径选择，不是代理）。
unset http_proxy https_proxy all_proxy HTTP_PROXY HTTPS_PROXY ALL_PROXY
unset QXNYC_POLL_INTERVAL
export QXNYC_ROUTE_PROBE_SECS=2
export QXNYC_SESSION_KEEPALIVE=0

CFG="$XDG_CONFIG_HOME/qxync"
STATE="$XDG_STATE_HOME/qxync"
mkdir -p "$CFG/links" "$XDG_RUNTIME_DIR/qxync" "$ROOT/logs"
chmod 700 "$XDG_RUNTIME_DIR/qxync"

LINKS=(default ddns ident legacy down)
cleanup() {
  for id in "${LINKS[@]}"; do
    timeout 30 "$QX" --link "$id" daemon stop --force >/dev/null 2>&1 || true
  done
  if [ -f "$ROOT/pids" ]; then
    while read -r pid; do
      [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    done <"$ROOT/pids"
  fi
}
trap cleanup EXIT

# ---------------------------------------------------------------- 假 NAS

cat >"$ROOT/mock_nas.py" <<'PY'
import sys, json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs

PORT = int(sys.argv[1])
UID = sys.argv[2]
SID = "sid-%d" % PORT

class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a):
        pass
    def _send(self, code, body, ctype="application/json"):
        b = body.encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(b)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(b)
    def do_GET(self):
        u = urlparse(self.path)
        q = parse_qs(u.query)
        func = (q.get("func") or [""])[0]
        if func == "qbox_get_nas_uid":
            self._send(200, json.dumps({
                "UID": UID, "MAC0": "aabbccddeeff", "SUID": "s-" + UID,
                "Qsync_version": "5.1.1", "Qsync_qpkg_version": "5.0.0.7",
                "Build": "20260723", "qbox_cgi": 1, "fcgi": 1,
            }))
        elif func == "qbox_get_max_log":
            self._send(200, json.dumps({
                "max_log": "1", "notify": "1", "global_notify": "1",
                "sync_signal": 1, "server_limit": "256", "cgi_number": "1",
            }))
        elif func in ("qbox_get_sync_log", "qbox_query_notify"):
            self._send(200, json.dumps({"status": 1, "datas": []}))
        elif func in ("get_list", "stat", "qbox_get_syncing_folder_list"):
            body = {"status": 1, "total": 0, "datas": [], "folder": []}
            self._send(200, json.dumps(body))
        else:
            self._send(200, json.dumps({"status": 1}))
    def do_POST(self):
        ln = int(self.headers.get("Content-Length") or 0)
        if ln:
            self.rfile.read(ln)
        if self.path.startswith("/cgi-bin/authLogin.cgi"):
            self._send(200,
                "<QDocRoot><authPassed>1</authPassed><authSid>%s</authSid>"
                "<username>u</username></QDocRoot>" % SID,
                "text/xml")
        elif self.path.startswith("/cgi-bin/qsync/upload.php"):
            self._send(200, json.dumps({"status": 1, "files": [{"status": 1}]}))
        else:
            self._send(200, json.dumps({"status": 1}))

ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
PY

free_port() {
  python3 -c "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1]); s.close()"
}

start_nas() { # port uid name
  local port="$1" uid="$2" name="$3"
  python3 "$ROOT/mock_nas.py" "$port" "$uid" >"$ROOT/logs/$name.log" 2>&1 &
  local pid=$!
  echo "$pid" >>"$ROOT/pids"
  for _ in $(seq 1 50); do
    if python3 - "$port" <<'PY'
import socket, sys
s = socket.socket(); s.settimeout(0.2)
try:
    s.connect(("127.0.0.1", int(sys.argv[1]))); sys.exit(0)
except Exception:
    sys.exit(1)
PY
    then return 0; fi
    sleep 0.1
  done
  echo "假 NAS 启动失败（port=$port）" >&2
  return 1
}

stop_nas() { # pid
  kill "$1" 2>/dev/null || true
  for _ in $(seq 1 30); do
    kill -0 "$1" 2>/dev/null || return 0
    sleep 0.1
  done
}

write_link() { # id primary_port extras_json [user]
  local id="$1" pport="$2" extras="$3"
  local user="${4:-u}"
  cat >"$CFG/links/$id.json" <<EOF
{"id":"$id","host":"127.0.0.1","port":$pport,"https":false,"insecure":true,
 "user":"$user","ipv4_only":true,"endpoints":$extras}
EOF
  # default 沿用旧的全局 credentials.json；其它连接用 links/<id>.credentials.json
  local cred="$CFG/links/$id.credentials.json"
  if [ "$id" = "default" ]; then cred="$CFG/credentials.json"; fi
  cat >"$cred" <<EOF
{"host":"127.0.0.1","user":"$user","password":"x"}
EOF
  chmod 600 "$cred"
}

routes_json() { # id
  "$QX" --link "$1" routes --json 2>/dev/null
}
jget() { # json expr（用 argv 传表达式，避免引号/逗号被 shell 吃掉）
  python3 -c 'import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))' "$1" 2>/dev/null
}
wait_active() { # id port timeout_secs
  local id="$1" port="$2" limit="${3:-20}" i act
  for i in $(seq 1 "$limit"); do
    act="$(routes_json "$id" | jget "d['active'] or ''")"
    if [ "$act" = "127.0.0.1:$port" ]; then return 0; fi
    sleep 1
  done
  echo "等待 active=127.0.0.1:$port 超时（最后 $act）" >&2
  return 1
}

# ---------------------------------------------------------------- 端口

A_LIVE=$(free_port); A_DEAD=$(free_port)
B_PRIMARY=$(free_port); B_HOST=$(free_port)
I_A=$(free_port); I_B=$(free_port)
L_PORT=$(free_port)
D_A=$(free_port); D_B=$(free_port)

echo "== 1. 额外路径优先 + 自动切换 + 自动切回 =="
start_nas "$A_LIVE" "UID-A" "a-live"
write_link default "$A_DEAD" "[{\"label\":\"live\",\"host\":\"127.0.0.1\",\"port\":$A_LIVE}]"
"$QX" daemon start >/dev/null 2>&1 || fail "default daemon 启动"
sleep 1
"$QX" login >/dev/null 2>&1; check $? "default 经额外路径登录成功"

ACT="$(routes_json default | jget "d['active']")"
[ "$ACT" = "127.0.0.1:$A_LIVE" ]; check $? "初始走可达的额外路径（$ACT）"
STATE_LIVE="$(routes_json default | jget "d['endpoints'][0]['state']")"
[ "$STATE_LIVE" = "ok" ]; check $? "额外路径状态 ok"
STATE_DEAD="$(routes_json default | jget "d['endpoints'][1]['state']")"
[ "$STATE_DEAD" = "cooling" ] || [ "$STATE_DEAD" = "failed" ]; check $? "死路径进入冷却/失败"

# 杀掉 live，在原「主地址（死）」端口起同 UID 的 NAS → 巡检应切过去
A_PID="$(tail -1 "$ROOT/pids")"
stop_nas "$A_PID"
start_nas "$A_DEAD" "UID-A" "a-primary"
if wait_active default "$A_DEAD" 25; then
  pass "live 挂掉后自动切到原主地址（不重启 daemon）"
else
  fail "live 挂掉后自动切到原主地址（不重启 daemon）"
fi
SW="$(routes_json default | jget "d['switches']")"
[ "${SW:-0}" -ge 1 ]; check $? "routes 记录切换次数（switches=$SW）"
REASON="$(routes_json default | jget "(d.get('last_switch') or {}).get('reason','')")"
[ -n "$REASON" ]; check $? "routes 记录切换原因（$REASON）"

# 再把 live 起回来（同 UID）→ 连续 2 次探测成功后应切回优先路径
start_nas "$A_LIVE" "UID-A" "a-live-back"
if wait_active default "$A_LIVE" 30; then
  pass "优先路径恢复后自动切回"
else
  fail "优先路径恢复后自动切回"
fi
"$QX" status >/dev/null 2>&1; check $? "切路后 status 仍正常"
"$QX" --link default daemon stop --force >/dev/null 2>&1 || true

echo "== 2. 主机名 / DDNS 解析与诊断 =="
start_nas "$B_PRIMARY" "UID-B" "b-primary"
start_nas "$B_HOST" "UID-B" "b-host"
write_link ddns "$B_PRIMARY" "[{\"label\":\"ddns\",\"host\":\"localhost\",\"port\":$B_HOST}]"
"$QX" --link ddns daemon start >/dev/null 2>&1 || fail "ddns daemon 启动"
sleep 1
"$QX" --link ddns login >/dev/null 2>&1; check $? "DDNS 风格主机名路径登录成功"
DNS="$(routes_json ddns | jget "'_'.join(d['endpoints'][0]['resolved'])")"
case "$DNS" in *127.*) check 0 "域名解析结果可见（$DNS）";; *) check 1 "域名解析结果可见（$DNS）";; esac
SCOPE="$(routes_json ddns | jget "'_'.join(d['endpoints'][0]['resolved_scopes'])")"
case "$SCOPE" in *回环*) check 0 "解析结果分类正确（$SCOPE）";; *) check 1 "解析结果分类正确（$SCOPE）";; esac
"$QX" --link ddns routes >/dev/null 2>&1; check $? "人类可读的 routes 输出"
"$QX" --link ddns daemon stop --force >/dev/null 2>&1 || true

echo "== 3. NAS 身份守卫（路径指向另一台 NAS） =="
start_nas "$I_A" "UID-OTHER" "i-a"
start_nas "$I_B" "UID-EXPECTED" "i-b"
write_link ident "$I_B" "[{\"label\":\"wrong\",\"host\":\"127.0.0.1\",\"port\":$I_A}]"
mkdir -p "$STATE/link-identity"
cat >"$STATE/link-identity/ident.json" <<EOF
{"subject":"127.0.0.1:$I_B","uid":"UID-EXPECTED","mac0":"aabbccddeeff","updated_unix":0}
EOF
"$QX" --link ident daemon start >/dev/null 2>&1 || fail "ident daemon 启动"
sleep 1
"$QX" --link ident login >/dev/null 2>&1; check $? "身份不符时自动换到正确路径登录"
ACT_I="$(routes_json ident | jget "d['active']")"
[ "$ACT_I" = "127.0.0.1:$I_B" ]; check $? "最终使用的是正确 NAS 的路径（$ACT_I）"
STATE_I="$(routes_json ident | jget "d['endpoints'][0]['state']")"
[ "$STATE_I" = "blocked" ]; check $? "错误 NAS 路径被标记 blocked（$STATE_I）"
NOTE_I="$(routes_json ident | jget "d['endpoints'][0].get('note') or ''")"
case "$NOTE_I" in *身份*) check 0 "blocked 原因可读（$NOTE_I）";; *) check 1 "blocked 原因可读（$NOTE_I）";; esac
"$QX" --link ident daemon stop --force >/dev/null 2>&1 || true

echo "== 4. 旧配置（无 endpoints）行为不变 =="
start_nas "$L_PORT" "UID-L" "l"
write_link legacy "$L_PORT" "[]"
"$QX" --link legacy daemon start >/dev/null 2>&1 || fail "legacy daemon 启动"
sleep 1
"$QX" --link legacy login >/dev/null 2>&1; check $? "旧配置登录成功"
LEGACY_N="$(routes_json legacy | jget "len(d['endpoints'])")"
[ "$LEGACY_N" = "1" ]; check $? "旧配置只有主地址一条路径（$LEGACY_N）"
LEGACY_ACTIVE="$(routes_json legacy | jget "d['active']")"
[ "$LEGACY_ACTIVE" = "127.0.0.1:$L_PORT" ]; check $? "旧配置 active = 主地址"
"$QX" --link legacy routes >/dev/null 2>&1; check $? "旧配置 routes 命令可用"
"$QX" --link legacy daemon stop --force >/dev/null 2>&1 || true

echo "== 5. 全部路径不可达 → 离线 + 每路原因 =="
write_link down "$D_A" "[{\"label\":\"backup\",\"host\":\"127.0.0.1\",\"port\":$D_B}]"
"$QX" --link down daemon start >/dev/null 2>&1 || fail "down daemon 启动"
sleep 1
"$QX" --link down login >/dev/null 2>&1 && fail "全路径不可达时 login 不应成功" || pass "全路径不可达时 login 明确失败"
OUT="$("$QX" --link down status 2>&1)"
case "$OUT" in *离线*) check 0 "status 如实显示离线";; *) check 1 "status 如实显示离线";; esac
case "$OUT" in *"路径      : 2 条"*) check 0 "status 显示路径摘要";; *) check 1 "status 显示路径摘要";; esac
DOWN_FAILED="$(routes_json down | jget "sum(1 for e in d['endpoints'] if e['state'] in ('failed','cooling'))")"
[ "${DOWN_FAILED:-0}" -ge 1 ]; check $? "routes 保留每条路径的失败状态（$DOWN_FAILED）"
"$QX" --link down daemon stop --force >/dev/null 2>&1 || true

echo "== 6. daemon 不在时 routes --probe 回落到直连探测 =="
OUT="$("$QX" --link default routes --probe --json 2>/dev/null)"
printf '%s' "$OUT" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['probed'] is True; assert len(d['endpoints']) == 2" 2>/dev/null
check $? "直连模式 routes --probe 输出合法 JSON 且 probed=true"
case "$OUT" in *'"resolved"'*) check 0 "直连探测包含 DNS 解析结果";; *) check 1 "直连探测包含 DNS 解析结果";; esac

echo
echo "m40-multipath: PASS=$PASS FAIL=$FAIL"
[ "$FAIL" = "0" ] || exit 1
