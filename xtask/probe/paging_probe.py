"""验证 get_list 的 start/limit 分页是否真生效（M15/T1 的强制前置）。

要回答两个问题：
  Q1: start>0 能否翻到第 2 页（返回与第一页不同的条目）？
  Q2: total / real_total 是「目录总项数」还是「本页条数」？
     —— 这决定客户端分页循环的退出条件怎么写（qxync-client list()）。

用法：
  python3 paging_probe.py --host qnap.kdajv.com --port 9834 \
      --user test1 --password '***' --path /home
"""
import argparse
import json
import re
import ssl
import sys
import urllib.parse
import urllib.request

CTX = ssl.create_default_context()
CTX.check_hostname = False
CTX.verify_mode = ssl.CERT_NONE


def get(url):
    with urllib.request.urlopen(url, context=CTX, timeout=30) as r:
        return r.read().decode("utf-8", "replace")


def listing(base, params):
    q = urllib.parse.urlencode(params)
    url = f"{base}/cgi-bin/qsync/qsyncsrv.cgi?{q}"
    body = get(url)
    i = body.find("{")
    if i < 0:
        raise RuntimeError("响应里没有 JSON: " + body[:200])
    return json.loads(body[i:])


def summarize(tag, d):
    names = [x.get("filename") for x in d.get("datas", [])]
    print(f"--- {tag}")
    print(
        f"    status={d.get('status')} total={d.get('total')} "
        f"real_total={d.get('real_total')} datas_len={len(names)}"
    )
    if names:
        print(f"    first={names[0]!r} last={names[-1]!r}")
    return names


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", required=True)
    ap.add_argument("--port", type=int, default=9834)
    ap.add_argument("--user", required=True)
    ap.add_argument("--password", required=True)
    ap.add_argument("--path", default="/home")
    ap.add_argument("--limit", type=int, default=200)
    a = ap.parse_args()
    base = f"https://{a.host}:{a.port}"

    # 登录：照 qs_probe.py 的实测变体 —— serviceKey=1 + pwd=base64(口令)。
    # （QTS 5.x 前端写法；明文口令会返回 authPassed=0。）
    import base64

    body = get(
        f"{base}/cgi-bin/authLogin.cgi?"
        + urllib.parse.urlencode(
            {
                "user": a.user,
                "serviceKey": "1",
                "client_app": "Qsync",
                "client_agent": "QsyncPagingProbe",
                "gen_client_id": "1",
                "remme": "1",
                "pwd": base64.b64encode(a.password.encode()).decode(),
            }
        )
    )
    # authLogin.cgi 回的是 **XML**（QDocRoot），不是 JSON —— sid 在 <authSid> 里。
    m = re.search(r"<authSid><!\[CDATA\[([^\]]+)\]\]></authSid>", body) or re.search(
        r'"sid"\s*:\s*"([^"]+)"', body
    )
    if not m:
        print("登录失败:", body[:400])
        sys.exit(1)
    sid = m.group(1)
    print(f"sid={sid}  path={a.path}")

    common = {
        "func": "get_list", "sid": sid, "is_iso": "0", "list_mode": "all",
        "path": a.path, "dir": "ASC", "sort": "filename",
        "no_sort": "0", "hidden_file": "1",
    }

    n0 = summarize("start=0", listing(base, dict(common, limit=str(a.limit), start="0")))
    n1 = summarize(
        f"start={a.limit}", listing(base, dict(common, limit=str(a.limit), start=str(a.limit)))
    )
    ns = summarize("limit=1 start=0", listing(base, dict(common, limit="1", start="0")))
    summarize("limit=1 start=1", listing(base, dict(common, limit="1", start="1")))

    print("\n=== 结论")
    if n0 and not n1:
        print("Q1 start>0 翻页: 否 —— 第二页为空，目录项数 <= limit")
    elif set(n1) & set(n0):
        print(f"Q1 start>0 翻页: 否 —— 返回了与第一页重复的条目 {sorted(set(n1) & set(n0))[:3]}")
    else:
        print("Q1 start>0 翻页: 是 —— 第二页是不同条目")
    if ns:
        verdict = "total=目录总项数" if len(ns) > 1 else "total=本页条数（不是目录总数！）"
        print(f"Q2 limit=1 时 total={len(ns)} → {verdict}")


if __name__ == "__main__":
    main()
