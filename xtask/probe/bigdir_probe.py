"""M15/T1 决定性验证：造一个 500 项的目录，实测 get_list 分页。

要回答三个问题（决定 T1 的实现方式）：
  Q1: limit=200 时，服务端返回的是「一页 200」还是「全部 500」？
  Q2: start>0 能否真的翻到后续页？翻页会不会重复/漏项？
  Q3: total / real_total 是「目录总项数」还是「本页条数」？
      → 这决定 qxync-client/src/lib.rs list() 的分页退出条件是否正确。

实测（2026-10-04，QTS 5.2.9 / Qsync QPKG build 20260723，账号 test1）：
  /home 只有 3 项，无法触发翻页 —— 必须造大目录才能验证。
  limit 可放大到 5000（服务端不强制 Max_File_List=200）。

用法：
  python3 bigdir_probe.py --host qnap.kdajv.com --port 9834 --user test1 --password '***'
"""
import argparse
import base64
import json
import re
import ssl
import sys
import urllib.parse
import urllib.request

CTX = ssl.create_default_context()
CTX.check_hostname = False
CTX.verify_mode = ssl.CERT_NONE

BIGDIR = "/home/bigdir-probe"
N_FILES = 500


def get(url, timeout=60):
    with urllib.request.urlopen(url, context=CTX, timeout=timeout) as r:
        return r.read().decode("utf-8", "replace")


def post(url, data, ctype, timeout=120):
    req = urllib.request.Request(url, data=data, headers={"Content-Type": ctype})
    with urllib.request.urlopen(req, context=CTX, timeout=timeout) as r:
        return r.read().decode("utf-8", "replace")


def login(base, user, password):
    q = urllib.parse.urlencode(
        {
            "user": user, "serviceKey": "1", "client_app": "Qsync",
            "client_agent": "QsyncBigdirProbe", "gen_client_id": "1",
            "remme": "1", "pwd": base64.b64encode(password.encode()).decode(),
        }
    )
    body = get(f"{base}/cgi-bin/authLogin.cgi?{q}")
    m = re.search(r"<authSid><!\[CDATA\[([^\]]+)\]\]></authSid>", body)
    if not m:
        print("登录失败:", body[:300])
        sys.exit(1)
    return m.group(1)


class Nas:
    def __init__(self, base, sid):
        self.base, self.sid = base, sid
        self.cgi = f"{base}/cgi-bin/qsync/qsyncsrv.cgi"

    def _json(self, url):
        body = get(url)
        i = body.find("{")
        return json.loads(body[i:]) if i >= 0 else {}

    def list(self, path, limit=200, start=0):
        p = {
            "func": "get_list", "sid": self.sid, "is_iso": "0", "list_mode": "all",
            "path": path, "dir": "ASC", "sort": "filename", "no_sort": "0",
            "hidden_file": "1", "limit": str(limit), "start": str(start),
        }
        return self._json(self.cgi + "?" + urllib.parse.urlencode(p))

    def mkdir(self, parent, name):
        data = urllib.parse.urlencode(
            {"sid": self.sid, "dest_path": parent, "dest_folder": name}
        ).encode()
        body = post(
            f"{self.cgi}?func=createdir", data,
            "application/x-www-form-urlencoded",
        )
        i = body.find("{")
        return json.loads(body[i:]) if i >= 0 else {}

    def upload(self, dest_path, filename, content):
        import uuid

        # 字段名必须是 files[]（写 file 会被服务端以 acceptFileTypes 拒绝，实测踩过）
        bd = "----qx" + uuid.uuid4().hex
        body = (
            f"--{bd}\r\n"
            f'Content-Disposition: form-data; name="files[]"; filename="{filename}"\r\n'
            f"Content-Type: application/octet-stream\r\n\r\n"
        ).encode() + content + f"\r\n--{bd}--\r\n".encode()
        url = (
            f"{self.base}/cgi-bin/qsync/upload.php?"
            + urllib.parse.urlencode(
                {"sid": self.sid, "dest_path": dest_path, "overwrite": "1", "type": "standard"}
            )
        )
        body = post(url, body, f"multipart/form-data; boundary={bd}")
        i = body.find("{")
        return json.loads(body[i:]) if i >= 0 else {}


def build(nas, n):
    print(f"[1/3] 建目录 {BIGDIR}")
    names = [x["filename"] for x in nas.list("/home").get("datas", [])]
    if "bigdir-probe" not in names:
        print("   ", nas.mkdir("/home", "bigdir-probe"))
    print(f"[2/3] 传 {n} 个小文件（并行度受限，逐个传）")
    existing = {x["filename"] for x in nas.list(BIGDIR, limit=5000).get("datas", [])}
    todo = [f"f{i:04d}.txt" for i in range(n) if f"f{i:04d}.txt" not in existing]
    print(f"    已有 {len(existing)}，待传 {len(todo)}")
    for i, name in enumerate(todo):
        r = nas.upload(BIGDIR, name, f"content {i}\n".encode())
        if i % 100 == 0:
            print(f"    {i}/{len(todo)} status={r.get('status')}")
    print("[3/3] 完成")


def analyze(nas, n):
    print("\n=== 验证 ===")
    d0 = nas.list(BIGDIR, limit=200, start=0)
    total = d0.get("total")
    print(f"Q1 limit=200 start=0 : total={total} real_total={d0.get('real_total')} "
          f"datas={len(d0.get('datas', []))} → "
          f"{'服务端只给一页' if len(d0.get('datas', [])) < n else '服务端一次给全'}")

    big = nas.list(BIGDIR, limit=5000, start=0)
    allnames = [x["filename"] for x in big.get("datas", [])]
    print(f"   limit=5000 单页拿到 {len(allnames)} 项，total={big.get('total')}")
    print(f"Q3 total 语义: limit=200 时 total={total}，实际项数={len(allnames)} → "
          f"{'total=目录总项数' if total == len(allnames) else 'total=本页条数'}")

    # 用 start 翻页拼回全量，看是否不重不漏
    seen, dup, pages = [], 0, 0
    start = 0
    while True:
        p = nas.list(BIGDIR, limit=200, start=start)
        datas = p.get("datas", [])
        if not datas:
            break
        pages += 1
        for x in datas:
            nm = x["filename"]
            if nm in seen:
                dup += 1
            seen.append(nm)
        start += len(datas)
        if pages > 20:
            break
    print(f"Q2 start 翻页 {pages} 页：共 {len(seen)} 项，重复 {dup} 项，"
          f"漏 {len(set(allnames) - set(seen))} 项，多 {len(set(seen) - set(allnames))} 项")

    ok = len(seen) == len(allnames) and dup == 0
    print(f"\n=== 结论: start/limit 翻页{'可用，不重不漏' if ok else '有问题，不可直接依赖'}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", required=True)
    ap.add_argument("--port", type=int, default=9834)
    ap.add_argument("--user", required=True)
    ap.add_argument("--password", required=True)
    ap.add_argument("--n", type=int, default=N_FILES)
    ap.add_argument("--skip-build", action="store_true")
    a = ap.parse_args()
    base = f"https://{a.host}:{a.port}"
    nas = Nas(base, login(base, a.user, a.password))
    if not a.skip_build:
        build(nas, a.n)
    analyze(nas, a.n)


if __name__ == "__main__":
    main()
