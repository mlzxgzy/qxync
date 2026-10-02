#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
qs_probe.py —— Qsync / WFM 协议动态探测器

用途：对【你自己的】QNAP NAS 逐条调用本文档还原出的 API，打印原始响应，
      用来填补静态分析无法确定的空白（密码字段名、响应字段、轮询批量大小等）。

⚠️ 仅用于你拥有或已获授权管理的设备。请勿对他人 NAS 使用。

用法:
  python3 qs_probe.py --host 192.168.1.10 --port 8080 --user admin --password 'xxx' login
  python3 qs_probe.py --host nas.myqnapcloud.com --port 443 --https --user u --password p login
  python3 qs_probe.py ... probe          # 登录 + 依次调用只读端点并落盘
  python3 qs_probe.py ... raw "func=qbox_get_qbox_info&sid={sid}"

依赖: 仅标准库 (urllib)
输出: 结果同时打印到 stdout 并写入 ./probe-out/<时间戳>/ 下
"""

import argparse, base64, json, os, re, ssl, sys, time, urllib.parse, urllib.request, urllib.error

# ---------------------------------------------------------------- 基础设施

def make_ctx(insecure: bool):
    if insecure:
        c = ssl.create_default_context()
        c.check_hostname = False
        c.verify_mode = ssl.CERT_NONE
        return c
    return None

class Client:
    def __init__(self, host, port, use_https, insecure, prefix="", verbose=True):
        self.host = host
        self.port = port
        self.scheme = "https" if use_https else "http"
        # 从二进制还原：prefix 为空 → 直连；非空时形如 "qsync/"
        self.prefix = prefix
        self.base = f"{self.scheme}://{host}:{port}"
        self.ctx = make_ctx(insecure)
        self.verbose = verbose
        self.outdir = None
        self.opener = urllib.request.build_opener(
            urllib.request.HTTPSHandler(context=self.ctx) if self.ctx else urllib.request.HTTPSHandler()
        )

    def url(self, path, params=None):
        u = f"{self.base}/{self.prefix}{path.lstrip('/')}"
        if params:
            u += ("&" if "?" in u else "?") + urllib.parse.urlencode(params)
        return u

    def request(self, path, params=None, data=None, headers=None, tag=None, timeout=30):
        u = self.url(path, params)
        body = urllib.parse.urlencode(data).encode() if data else None
        h = {
            "User-Agent": "QsyncProbe/1.0",
            "Accept": "*/*",
            "Cache-Control": "no-cache",
            "Connection": "Keep-Alive",
            "X-Forwarded-For": "127.0.0.1",
        }
        if body:
            h["Content-Type"] = "application/x-www-form-urlencoded; charset=UTF-8"
        if headers:
            h.update(headers)

        req = urllib.request.Request(u, data=body, headers=h,
                                     method="POST" if body else "GET")
        t0 = time.time()
        try:
            with self.opener.open(req, timeout=timeout) as r:
                raw = r.read()
                code, hdrs = r.status, dict(r.headers)
        except urllib.error.HTTPError as e:
            raw, code, hdrs = e.read(), e.code, dict(e.headers)
        except Exception as e:
            raw, code, hdrs = b"", -1, {"error": repr(e)}
        dt = int((time.time() - t0) * 1000)

        text = raw.decode("utf-8", "replace")
        if self.verbose:
            name = tag or path
            print(f"\n{'='*78}\n[{code}] {name}  ({dt} ms, {len(raw)} B)\n  {u}")
            if body:
                print(f"  BODY: {body.decode()}")
            print(f"{'-'*78}\n{text[:4000]}{'  …(截断)' if len(text) > 4000 else ''}")

        if self.outdir:
            fn = re.sub(r'[^A-Za-z0-9_.-]', '_', tag or path)[:80]
            with open(os.path.join(self.outdir, f"{fn}.http"), "w") as f:
                f.write(f"# {code} {u}\n# headers: {json.dumps(hdrs, ensure_ascii=False)}\n\n{text}")
        return code, text, hdrs

    def retry(self, path, params, data, tag):
        """与客户端一致的简单重试：连接类错误重试 3 次"""
        for i in range(3):
            code, text, h = self.request(path, params, data, tag=tag)
            if code > 0 or h.get("error"):
                if code > 0:
                    return code, text
            time.sleep(0.5 * (i + 1))
        return code, text


# ---------------------------------------------------------------- 解析

def xml_pick(text, tag):
    m = re.search(rf"<{tag}>(?:<!\[CDATA\[)?(.*?)(?:\]\]>)?</{tag}>", text, re.S)
    return m.group(1).strip() if m else None

def json_pick(text, key):
    try:
        return json.loads(text).get(key)
    except Exception:
        return None

def extract_sid(text):
    """authLogin.cgi / qsyncsrv_login.cgi 的 SID 有多种可能标签"""
    for tag in ("authSid", "sid", "auth_sid"):
        v = xml_pick(text, tag)
        if v:
            return v
    v = json_pick(text, "sid")
    if v:
        return v
    m = re.search(r'"sid"\s*:\s*"([^"]+)"', text)
    return m.group(1) if m else None

def extract_qtoken(text):
    for tag in ("q_token", "qtoken", "QToken"):
        v = xml_pick(text, tag)
        if v:
            return v
    for k in ("q_token", "qtoken", "QToken"):
        v = json_pick(text, k)
        if v:
            return v
    return None


# ---------------------------------------------------------------- 流程

def qnap_encode_pwd(pwd):
    """QNAP 前端 QNAPTool.ezEncode(QNAPTool.utf16to8(pwd))：ezEncode 就是标准 base64，
    等价于 base64(UTF-8 口令)。"""
    return base64.b64encode(pwd.encode("utf-8")).decode()


def do_login(c, user, pwd, extra):
    """阶段 3：authLogin.cgi。

    2026-09-30 对 QTS 5.2.9 / Qsync QPKG build 20260723 实测（见 docs/执行方案-M0M1.md §1.1）：
      - 服务端只认 `serviceKey=1` + `pwd=base64(口令)`；明文口令、或 `service=Qsync`
        都会返回 authPassed=0 / errorValue=-1。
      - `service=Qsync` 这个字段名来自 Windows 二进制的静态逆向，实测是错的
        （登录协议的权威参考是 NAS 自带的 /cgi-bin/js/qos-core-login.js）。
    顺序：实测通过的变体 → 旧 QTS 回退，并打印命中的是哪一个。
    """
    print("\n" + "#" * 78)
    print("# 阶段 3: authLogin.cgi —— 登录（含旧 QTS 回退）")
    print("#" * 78)

    modern = {"serviceKey": "1", "client_app": "Qsync", "client_agent": "QsyncProbe",
              "gen_client_id": "1", "remme": "1", "dont_verify_2sv_again": "0"}
    legacy = {"service": "Qsync", "client_app": "Qsync", "client_id": "",
              "client_agent": "QsyncProbe", "duration": "1440", "remme": "1"}

    variants = [
        ("serviceKey+b64",   modern, qnap_encode_pwd(pwd), "★ 实测通过（QTS 5.x 前端写法）"),
        ("serviceKey+plain", modern, pwd,                  "回退：未编码口令"),
        ("service+plain",    legacy, pwd,                  "回退：旧写法（早期笔记）"),
    ]

    best = None
    for name, fields, pwd_value, why in variants:
        body = {"user": user}
        body.update(fields)
        body.update(extra or {})
        if not (extra and "pwd" in extra):
            body["pwd"] = pwd_value
        code, text = c.retry("cgi-bin/authLogin.cgi", None, body,
                             tag=f"authLogin__{name.replace('+', '_')}")
        passed = xml_pick(text, "authPassed")
        sid = extract_sid(text)
        print(f"  → 变体 {name!r} ({why}): authPassed={passed} sid={'有' if sid else '无'}")
        if (passed in ("1", "true", "True")) or sid:
            best = (name, sid, text)
            print(f"  ✅ 命中：登录变体 {name!r}")
            break
    if not best:
        print("  ❌ 所有变体都未通过。可能：① 账号/密码错 ② 需要 2SV ③ 服务端版本差异")
        print("     请检查上面的原始响应（error_code / need_2sv / need_2_step_verification）")
        return None, None
    name, sid, text = best
    print(f"\n  SID = {sid}")
    for t in ("authPassed", "error_code", "permission_deny", "pw_expiry",
              "need_2sv", "force_2sv", "need_2_step_verification", "username", "cuid"):
        v = xml_pick(text, t)
        if v is not None:
            print(f"    {t} = {v}")

    # 阶段 5：Qsync 层登录，换 q_token
    print("\n" + "#" * 78)
    print("# 阶段 5: qsyncsrv_login.cgi / qsyncsrv.fcgi?func=login")
    print("#" * 78)
    qbody = {"user": user, "qbox_user": user, "qbox_computer": os.uname().nodename,
             "qbox_device_type": "3", "Qsync_client_version": "5.0.0"}
    qbody.update(extra or {})
    qtoken = None
    for path, tag in (("cgi-bin/qsync/qsyncsrv_login.cgi", "qsyncsrv_login"),
                      ("cgi-bin/qsync/qsyncsrv.fcgi?func=login", "qsyncsrv_fcgi_login"),
                      ("cgi-bin/filemanager/wfm2Login.cgi", "wfm2Login")):
        # 注意：模板本身不含 sid（?func=login 已是完整 query），不要把 sid 混进 query
        code, text = c.retry(path, None, qbody, tag=tag)
        qtoken = extract_qtoken(text)
        new_sid = extract_sid(text)
        if new_sid:
            sid = new_sid
        print(f"  → {path}: sid={'有' if sid else '无'} q_token={'有' if qtoken else '无'}")
        if qtoken or "authPassed" in text:
            break
    if not qtoken:
        # 实测：QTS 5.2.9 / Qsync QPKG 20260723 返回 {"status":-50}，拿不到 q_token；
        # 但 qsyncsrv.cgi 的只读 func 直接用 QTS sid 就能用 → q_token 不是只读链路的必要条件。
        print("  ⚠️ 没拿到 q_token（本机实测 status=-50）。若 status 为 -50，属已知现象："
              "只读端点用 QTS sid 即可，不必阻塞。")
    print(f"\n  SID={sid}\n  QToken={(qtoken or '')[:40]}{'…' if qtoken and len(qtoken)>40 else ''}")
    return sid, qtoken


READONLY = [
    # (路径, 参数模板, 说明)   {sid} / {qtoken} 会被替换
    ("cgi-bin/qsync/qsyncsrvPrepare.cgi", {"user": "{user}"}, "取 NAS UID（新版已 404 → 用 qbox_get_nas_uid）"),
    ("cgi-bin/qsync/qsyncsrv.cgi", {"func": "qbox_get_nas_uid", "sid": "{sid}"}, "★ NAS UID / SUID / CUID（替代 prepare）"),
    ("cgi-bin/qsync/qsyncsrv.fcgi", {"func": "qbox_get_max_log", "sid": "{sid}"}, "★ 同步日志最大号"),
    ("cgi-bin/qsync/qsyncsrv.cgi", {"func": "qbox_get_qbox_info", "sid": "{sid}"}, "Qsync 服务端信息"),
    ("cgi-bin/qsync/qsyncsrv.cgi", {"func": "qbox_get_static_information", "sid": "{sid}"}, "NAS 静态信息"),
    ("cgi-bin/qsync/qsyncsrv.cgi", {"func": "qbox_get_nas_status", "sid": "{sid}"}, "NAS 状态"),
    ("cgi-bin/qsync/qsyncsrv.cgi", {"func": "qbox_get_quota_info", "sid": "{sid}"}, "配额"),
    ("cgi-bin/qsync/qsyncsrv.cgi", {"func": "qbox_get_nas_uid", "sid": "{sid}"}, "NAS UID"),
    ("cgi-bin/qsync/qsyncsrv.cgi", {"func": "qbox_get_veto_files", "sid": "{sid}"}, "排除文件列表"),
    ("cgi-bin/qsync/qsyncsrv.cgi", {"func": "qbox_get_policy", "sid": "{sid}"}, "共享策略"),
    ("cgi-bin/qsync/qsyncsrv.cgi", {"func": "qbox_get_config_acl", "sid": "{sid}"}, "配置 ACL"),
    ("cgi-bin/qsync/qsyncsrv.cgi", {"func": "qbox_get_syncing_folder_list", "sid": "{sid}"}, "★ 正在同步的文件夹"),
    ("cgi-bin/qsync/qsyncsrv.cgi", {"func": "versioning_probe", "sid": "{sid}"}, "版本化能力探测"),
    ("cgi-bin/qsync/qsyncsrv.cgi", {"func": "versioning_stat", "sid": "{sid}"}, "版本化全局统计"),
    ("cgi-bin/qsync/qsyncsrv.cgi", {"func": "qbox_get_registry", "sid": "{sid}"}, "服务端注册表快照"),
    ("cgi-bin/qsync/qsyncsrv.cgi", {"func": "get_domain_ip_list", "sid": "{sid}"}, "★ 地址候选集"),
    ("cgi-bin/filemanager/utilRequest.cgi", {"func": "get_sys_setting", "sid": "{sid}"}, "系统设置"),
    ("cgi-bin/qsync/qsyncsrv.cgi", {"func": "get_list", "sid": "{sid}", "is_iso": "0",
        "list_mode": "all", "path": "/", "dir": "ASC", "limit": "50", "sort": "filename",
        "start": "0", "no_sort": "0"}, "★ 列根目录"),
]

def do_probe(c, sid, qtoken, user):
    print("\n" + "#" * 78)
    print("# 只读端点批量探测")
    print("#" * 78)
    for path, params, why in READONLY:
        p = {k: v.replace("{sid}", sid or "").replace("{user}", user).replace("{qtoken}", qtoken or "")
             for k, v in params.items()}
        c.request(path, p, tag=f"probe_{params.get('func', path.split('/')[-1])}")
    # 带 get_detail 的 sync log（新版引擎用的变体）
    c.request("cgi-bin/qsync/qsyncsrv.cgi",
              {"func": "qbox_get_sync_log", "sid": sid, "lower": "0", "number": "50",
               "get_detail": "1", "sub_folder": "/"}, tag="probe_qbox_get_sync_log_detail")
    c.request("cgi-bin/filemanager/utilRequest.cgi",
              {"func": "qbox_query_notify", "sid": sid, "lower": "0", "upper": "50"},
              tag="probe_qbox_query_notify")


def main():
    ap = argparse.ArgumentParser(description="Qsync/WFM 协议动态探测器")
    ap.add_argument("--host", required=True)
    ap.add_argument("--port", type=int, default=8080)
    ap.add_argument("--https", action="store_true")
    ap.add_argument("--insecure", action="store_true", help="跳过 TLS 校验（自签证书）")
    ap.add_argument("--prefix", default="", help="URL 前缀（直连留空；经 myQNAPcloud 时形如 qsync/）")
    ap.add_argument("--user", default="admin")
    ap.add_argument("--password", default="")
    ap.add_argument("--param", action="append", default=[],
                    help="追加/覆盖 authLogin 参数，形如 k=v（可多次）")
    ap.add_argument("--outdir", default=None)
    ap.add_argument("cmd", choices=["login", "probe", "raw"])
    ap.add_argument("rest", nargs="*")
    a = ap.parse_args()

    extra = {}
    for kv in a.param:
        if "=" in kv:
            k, v = kv.split("=", 1); extra[k] = v

    outdir = a.outdir or os.path.join("probe-out", time.strftime("%Y%m%d-%H%M%S"))
    os.makedirs(outdir, exist_ok=True)
    c = Client(a.host, a.port, a.https, a.insecure, a.prefix)
    c.outdir = outdir
    print(f"输出目录: {outdir}")
    print(f"基线 URL: {c.base}/{a.prefix}")

    if a.cmd == "raw":
        if not a.rest:
            print("raw 需要给出查询串，例如: raw 'func=qbox_get_qbox_info&sid=XXX'")
            return 1
        qs = a.rest[0]
        path, _, query = qs.partition("?")
        params = dict(urllib.parse.parse_qsl(query)) if query else None
        if "/" not in path:
            path = "cgi-bin/qsync/qsyncsrv.cgi"
        c.request(path, params, tag="raw")
        return 0

    sid, qtoken = do_login(c, a.user, a.password, extra)
    if not sid:
        print("\n登录失败，终止。")
        return 2
    if a.cmd == "probe":
        do_probe(c, sid, qtoken, a.user)

    print(f"\n\n完成。原始响应已保存到 {outdir}/")
    print("下一步建议：")
    print("  1) 打开 authLogin 的响应，确认 authPassed/sid 的字段名与格式")
    print("  2) 看 qbox_get_max_log 的返回结构")
    print("  3) 在 NAS 上改一个文件，再跑一次 probe，对比 qbox_get_sync_log 的事件结构")
    return 0


if __name__ == "__main__":
    sys.exit(main())
