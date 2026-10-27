#!/usr/bin/env bash
# m40-real-nas.sh —— ★ M40「多路径 / DDNS」的**真机只读**验收
#
# 验的是什么：
#   1. 真 DDNS 域名 + 真内网地址同时配置时，`routes --probe` 能解析出两条路径的
#      IP 并分类（公网 / 内网），选一条可达的作为 active；
#   2. 不可达的那条给出「请求超时」而不是把整个命令拖死；
#   3. （可选）给账号口令时，登录会走可达路径并成功 —— 不需要改任何现有 daemon。
#
# 本脚本**默认只读**：不登录、不写 NAS、不动用户正在跑的 qxyncd；所有配置都落在
# `.local-run/m40-real-nas` 的隔离 XDG 目录里，跑完即弃。
#
# 用法（把值换成你自己的）：
#   QXNYC_REAL_HOST=qnap.example.com \
#   QXNYC_REAL_LAN=192.168.1.100 \
#   QXNYC_REAL_PORT=9834 QXNYC_REAL_INSECURE=1 \
#     xtask/tests/m40-real-nas.sh
#
# 可选（会真的登录一次测试账号；口令只经环境变量传递，不写盘到仓库）：
#   QXNYC_REAL_USER=test1 QXNYC_REAL_PASSWORD='...'
#
# 缺 QXNYC_REAL_HOST 时直接 SKIP（CI / 普通开发机不会误连真实设备）。
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
QX="$REPO/target/debug/qxync"
ROOT="$REPO/.local-run/m40-real-nas"

if [ -z "${QXNYC_REAL_HOST:-}" ]; then
  echo "SKIP：需要 QXNYC_REAL_HOST / QXNYC_REAL_LAN（可选）/ QXNYC_REAL_PORT"
  exit 0
fi
if [ ! -x "$QX" ]; then
  echo "SKIP：没有 $QX（先 cargo build --workspace）"
  exit 0
fi
if ! command -v python3 >/dev/null; then
  echo "SKIP：需要 python3"
  exit 0
fi

HOST="${QXNYC_REAL_HOST}"
PORT="${QXNYC_REAL_PORT:-9834}"
HTTPS="${QXNYC_REAL_HTTPS:-1}"
INSECURE="${QXNYC_REAL_INSECURE:-1}"
LAN="${QXNYC_REAL_LAN:-}"
USER_NAME="${QXNYC_REAL_USER:-}"
PASSWORD="${QXNYC_REAL_PASSWORD:-}"

rm -rf "$ROOT"
mkdir -p "$ROOT/config/qxync/links" "$ROOT/run"
export XDG_CONFIG_HOME="$ROOT/config"
export XDG_DATA_HOME="$ROOT/data"
export XDG_STATE_HOME="$ROOT/state"
export XDG_RUNTIME_DIR="$ROOT/run"
unset QXNYC_SOCKET QXNYC_HOST QXNYC_USER QXNYC_PASSWORD QXNYC_OTP_SECRET QXNYC_OTP_CODE
chmod 700 "$ROOT/run"

json_escape() { python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$1"; }

EXTRAS="[]"
if [ -n "$LAN" ]; then
  EXTRAS="[{\"label\":\"内网 LAN\",\"host\":$(json_escape "$LAN"),\"port\":$PORT}]"
fi
cat >"$ROOT/config/qxync/links/default.json" <<EOF
{"id":"default","host":$(json_escape "$HOST"),"port":$PORT,
 "https":$([ "$HTTPS" = "1" ] && echo true || echo false),
 "insecure":$([ "$INSECURE" = "1" ] && echo true || echo false),
 "user":$(json_escape "${USER_NAME:-probe-only}"),"ipv4_only":false,
 "endpoints":$EXTRAS}
EOF

echo "== 直连探测（只读，不登录） =="
"$QX" routes --probe --json >"$ROOT/routes.json" 2>"$ROOT/probe.err"
PROBE_RC=$?
[ -s "$ROOT/probe.err" ] && sed -n '1,5p' "$ROOT/probe.err"
if [ "$PROBE_RC" != "0" ]; then
  echo "FAIL：routes --probe 退出码 $PROBE_RC"
  exit 1
fi

python3 - "$ROOT/routes.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
print("active =", d["active"], "| probed =", d["probed"])
for e in d["endpoints"]:
    dns = ", ".join(
        "%s(%s)" % (ip, scope)
        for ip, scope in zip(e["resolved"], e["resolved_scopes"])
    ) or "-"
    role = "主地址" if e["primary"] else "备选"
    print("- %-12s %s  %-8s %5s ms  DNS: %s  %s" % (
        e["label"], role, e["state"], e["latency_ms"] or "-", dns, e["last_error"] or ""))
ok = [e for e in d["endpoints"] if e["state"] == "ok"]
assert ok, "没有任何一条路径可达"
assert d["active"], "active 为空"
PY
if [ "$?" != "0" ]; then
  echo "FAIL：真实多路径探测未通过"
  exit 1
fi
echo "PASS：至少一条路径可达，active 已选定"

if [ -n "$LAN" ]; then
python3 - "$ROOT/routes.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
lan = [e for e in d["endpoints"] if e["label"] == "内网 LAN"][0]
assert lan["resolved"], "内网 LAN 没有解析结果"
assert any(s == "内网" for s in lan["resolved_scopes"]), \
    "内网地址分类不对: %s" % (lan["resolved_scopes"],)
print("PASS：内网路径解析并分类为「内网」")
PY
  [ "$?" = "0" ] || exit 1
fi

if [ -n "$USER_NAME" ] && [ -n "$PASSWORD" ]; then
  echo "== 真机登录（走上面选出的路径） =="
  QXNYC_USER="$USER_NAME" QXNYC_PASSWORD="$PASSWORD" \
    "$QX" login >"$ROOT/login.out" 2>"$ROOT/login.err"
  if [ "$?" != "0" ]; then
    echo "FAIL：登录失败"
    sed -n '1,20p' "$ROOT/login.err"
    exit 1
  fi
  if grep -q "路径" "$ROOT/login.out"; then
    echo "PASS：登录成功，且报告了所用路径"
  else
    echo "FAIL：登录输出里没有路径行"
    exit 1
  fi
  "$QX" status >"$ROOT/status.out" 2>&1
  if grep -q "已登录" "$ROOT/status.out"; then
    echo "PASS：status 显示已登录"
  else
    echo "FAIL：status 未显示已登录"
    exit 1
  fi
  grep -E "^(路径|连通性|登录状态)" "$ROOT/status.out" || true
else
  echo "（未提供 QXNYC_REAL_USER / QXNYC_REAL_PASSWORD，跳过登录验证）"
fi

echo
echo "m40-real-nas: 全部通过（隔离目录 $ROOT，可随时删除）"
