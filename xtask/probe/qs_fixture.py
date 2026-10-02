#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""qs_fixture.py —— M0 真机测试数据 + 下载/上传验证（只对自己授权的 NAS 使用）

2026-09-30 实测结论（QTS 5.2.9 / Qsync QPKG build 20260723）：
  * 登录      : POST /cgi-bin/authLogin.cgi, serviceKey=1 + pwd=base64(口令)     → authSid
  * 建目录    : POST /cgi-bin/qsync/qsyncsrv.cgi?func=createdir  body:sid,dest_path,dest_folder
  * 上传      : POST /cgi-bin/qsync/upload.php?sid=&dest_path=&overwrite=1&type=standard
                multipart 字段名必须是 `files[]`（blueimp jQuery-File-Upload 风格）
  * 下载      : GET  /cgi-bin/qsync/qsyncsrv.cgi?func=download&sid=&path=&file_name=
  * 普通用户的 /home == /share/homes/<user>（upload.php 的 fullPath 会回真实路径）

用法:
  python3 qs_fixture.py fixture            # 建 qxync-test/ 全套测试数据（含 >100MB 大文件）
  python3 qs_fixture.py fixture --small    # 同上但大文件只 16 MiB（省流量）
  python3 qs_fixture.py download           # 门1：下载 + Range + md5 校验
  python3 qs_fixture.py ls                 # 列 /home/qxync-test
"""
import argparse, base64, hashlib, json, os, re, ssl, sys, time, urllib.error, urllib.parse, urllib.request, uuid

# 连接目标一律从环境变量取（默认值是占位符，指不到任何真实设备）。
#   export QXNYC_TEST_HOST=your-nas.example.com
HOST = os.environ.get("QXNYC_TEST_HOST", "nas.example.com")
PORT = int(os.environ.get("QXNYC_TEST_PORT", "9834"))
USER = "test1"
UA = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36"
ROOT = "/home/qxync-test"
OUTDIR = "probe-out/fixture"
LOCAL = "probe-out/fixture/local"


def ctx():
    c = ssl.create_default_context()
    c.check_hostname = False
    c.verify_mode = ssl.CERT_NONE
    return c


class Nas:
    def __init__(self, password):
        self.base = f"https://{HOST}:{PORT}"
        self.pwd = password
        self.sid = None
        self.ctx = ctx()
        os.makedirs(OUTDIR, exist_ok=True)

    # ---------------------------------------------------------------- 低层
    def _req(self, path, params=None, data=None, body=None, ctype=None, tag=None, raw=False, timeout=300):
        url = self.base + "/" + path.lstrip("/")
        if params:
            # ★ 空格必须编码成 %20（不能是 +），否则含空格/中文的文件名一律 404
            url += ("&" if "?" in url else "?") + urllib.parse.urlencode(params, quote_via=urllib.parse.quote)
        if body is None and data is not None:
            body = urllib.parse.urlencode(data).encode()
            ctype = ctype or "application/x-www-form-urlencoded; charset=UTF-8"
        h = {"User-Agent": UA, "X-Forwarded-For": "127.0.0.1", "Accept": "*/*", "Cache-Control": "no-cache"}
        if ctype:
            h["Content-Type"] = ctype
        req = urllib.request.Request(url, data=body, headers=h)
        t0 = time.time()
        try:
            with urllib.request.urlopen(req, context=self.ctx, timeout=timeout) as r:
                code, blob, hdr = r.status, r.read(), dict(r.headers)
        except urllib.error.HTTPError as e:
            code, blob, hdr = e.code, e.read(), dict(e.headers)
        except Exception as e:
            code, blob, hdr = -1, ("ERR " + repr(e)).encode(), {}
        dt = time.time() - t0
        if tag:
            with open(os.path.join(OUTDIR, tag + ".http"), "wb") as f:
                f.write(f"# {code} {url}  ({dt:.2f}s)\n\n".encode() + blob)
        return code, blob, hdr, dt

    # ---------------------------------------------------------------- 高层
    def login(self):
        code, blob, _, _ = self._req("cgi-bin/authLogin.cgi", tag="00_login", data={
            "user": USER, "serviceKey": "1", "client_app": "Qsync", "client_agent": UA,
            "gen_client_id": "1", "remme": "1", "dont_verify_2sv_again": "0",
            "pwd": base64.b64encode(self.pwd.encode()).decode()})
        m = re.search(r"<authSid>(?:<!\[CDATA\[)?(.*?)(?:\]\]>)?</authSid>", blob.decode("utf-8", "replace"), re.S)
        if not m:
            sys.exit(f"登录失败（HTTP {code}）：{blob[:300]!r}")
        self.sid = m.group(1).strip()
        print(f"  login ok, sid={self.sid}")
        return self.sid

    def mkdir(self, parent, name):
        code, blob, _, _ = self._req("cgi-bin/qsync/qsyncsrv.cgi",
                                     {"func": "createdir", "sid": self.sid},
                                     data={"dest_path": parent, "dest_folder": name},
                                     tag=f"mkdir_{name}")
        txt = blob.decode("utf-8", "replace")
        m = re.search(r'"status"\s*:\s*(-?\d+)', txt)
        st = int(m.group(1)) if m else None
        print(f"  mkdir {parent}/{name} -> status={st}")
        return st

    def upload(self, dest_path, filename, content, tag=None):
        bd = "----qs" + uuid.uuid4().hex
        body = (f"--{bd}\r\nContent-Disposition: form-data; name=\"files[]\"; filename=\"{filename}\"\r\n"
                f"Content-Type: application/octet-stream\r\n\r\n").encode() + content + f"\r\n--{bd}--\r\n".encode()
        code, blob, _, dt = self._req("cgi-bin/qsync/upload.php",
                                      {"sid": self.sid, "dest_path": dest_path, "overwrite": "1", "type": "standard"},
                                      body=body, ctype=f"multipart/form-data; boundary={bd}",
                                      tag=tag or ("up_" + re.sub(r"[^A-Za-z0-9_.-]", "_", filename)), timeout=900)
        txt = blob.decode("utf-8", "replace")
        try:
            j = json.loads(txt)
            f = (j.get("files") or [{}])[0]
            ok = str(f.get("status")) == "1"
            speed = f"{len(content)/max(dt,1e-6)/1048576:.2f} MB/s"
            print(f"  upload {filename:32s} {len(content):>10d} B  status={f.get('status')} err={f.get('error')}  {speed}")
            return ok, f
        except Exception:
            print(f"  upload {filename}: 非 JSON 响应 {txt[:200]!r}")
            return False, {}

    def list(self, path=ROOT, hidden=True):
        p = {"func": "get_list", "sid": self.sid, "is_iso": "0", "list_mode": "all", "path": path,
             "dir": "ASC", "limit": "200", "sort": "filename", "start": "0", "no_sort": "0"}
        if hidden:
            p["hidden_file"] = "1"
        code, blob, _, _ = self._req("cgi-bin/qsync/qsyncsrv.cgi", p, tag="ls_" + path.strip("/").replace("/", "_"))
        try:
            return json.loads(blob.decode("utf-8", "replace"))
        except Exception:
            return {"_raw": blob[:200].decode("utf-8", "replace")}

    def stat(self, path, file_name):
        code, blob, _, _ = self._req("cgi-bin/qsync/qsyncsrv.cgi",
                                     {"func": "stat", "sid": self.sid, "path": path,
                                      "file_name": file_name, "file_total": "1"},
                                     tag=f"stat_{file_name}")
        try:
            return json.loads(blob.decode("utf-8", "replace"))
        except Exception:
            return {"_raw": blob[:200].decode("utf-8", "replace")}

    def download(self, path, file_name, rng=None, tag=None):
        """实测：qsyncsrv.cgi?func=download 会返回 status:20（需要 Qsync 同步文件夹会话），
        可用的数据面是 FileStation 命名空间：utilRequest.cgi?func=download
        + source_path/source_file/isfolder=0/source_total=1。"""
        hdr = {"Range": rng} if rng else None
        url = self.base + "/cgi-bin/filemanager/utilRequest.cgi?" + urllib.parse.urlencode(
            {"func": "download", "sid": self.sid, "source_path": path,
             "source_file": file_name, "isfolder": "0", "source_total": "1"},
            quote_via=urllib.parse.quote)
        h = {"User-Agent": UA, "X-Forwarded-For": "127.0.0.1", "Accept": "*/*"}
        if hdr:
            h.update(hdr)
        req = urllib.request.Request(url, headers=h)
        t0 = time.time()
        try:
            with urllib.request.urlopen(req, context=self.ctx, timeout=900) as r:
                code, blob, hh = r.status, r.read(), dict(r.headers)
        except urllib.error.HTTPError as e:
            code, blob, hh = e.code, e.read(), dict(e.headers)
        except Exception as e:
            code, blob, hh = -1, ("ERR " + repr(e)).encode(), {}
        dt = time.time() - t0
        if tag:
            with open(os.path.join(OUTDIR, tag + ".bin"), "wb") as f:
                f.write(blob)
        return code, blob, hh, dt


# ---------------------------------------------------------------- 测试数据
def build_fixture(nas, big_mb):
    os.makedirs(LOCAL, exist_ok=True)
    print(f"\n[1/4] 建目录 {ROOT}")
    if nas.list("/home").get("total") is not None:
        names = [x["filename"] for x in nas.list("/home").get("datas", [])]
        if "qxync-test" not in names:
            nas.mkdir("/home", "qxync-test")
    # 深层嵌套 ≥10 层
    parent = ROOT
    for i in range(1, 11):
        nas.mkdir(parent, f"l{i}")
        parent += f"/l{i}"

    print(f"\n[2/4] 小文件（含中文/空格/空文件）")
    files = [
        ("hello.txt", "qxync hello\nline2\nline3\n".encode()),
        ("空 格 中文名.txt", "中文内容校验\n第二行\n".encode()),
        ("empty.txt", b""),
        ("1k.bin", bytes(range(256)) * 4),
    ]
    for name, content in files:
        nas.upload(ROOT, name, content)
        with open(os.path.join(LOCAL, name), "wb") as f:
            f.write(content)
    nas.upload(parent, "deep.txt", "deep 10 levels\n".encode())
    with open(os.path.join(LOCAL, "deep.txt"), "wb") as f:
        f.write("deep 10 levels\n".encode())

    print(f"\n[3/4] 大文件 {big_mb} MiB（分块生成，避免占内存）")
    big = os.path.join(LOCAL, "big.bin")
    chunk = bytes((i * 7 + 13) & 0xFF for i in range(1 << 20))
    md5 = hashlib.md5()
    with open(big, "wb") as f:
        for _ in range(big_mb):
            f.write(chunk)
            md5.update(chunk)
    print(f"  local big.bin md5={md5.hexdigest()} size={big_mb<<20}")
    with open(big, "rb") as f:
        nas.upload(ROOT, "big.bin", f.read(), tag="up_big")

    print(f"\n[4/4] 清单")
    d = nas.list()
    for x in d.get("datas", []):
        print(f"  {'DIR ' if x.get('isfolder') else 'FILE'} {x.get('filename'):34s} {x.get('filesize'):>12s} epochmt={x.get('epochmt')}")


def verify_download(nas):
    print("\n=== 门 1：download 验证 ===")
    failures = 0
    deep_path = ROOT + "".join(f"/l{i}" for i in range(1, 11))
    targets = [(ROOT, n) for n in sorted(os.listdir(LOCAL))
               if os.path.isfile(os.path.join(LOCAL, n)) and n != "deep.txt"]
    targets.append((deep_path, "deep.txt"))          # 10 层嵌套里的文件
    for path, name in targets:
        lp = os.path.join(LOCAL, name)
        local = open(lp, "rb").read()
        code, blob, hh, dt = nas.download(path, name, tag="dl_" + re.sub(r"[^A-Za-z0-9_.-]", "_", name))
        same = blob == local
        print(f"  [{code}] {name:30s} local={len(local):>10d} remote={len(blob):>10d} "
              f"match={same} ct={hh.get('Content-type') or hh.get('Content-Type')} {dt:.2f}s")
        if code != 200 or not same:
            failures += 1
            print(f"        !!! 失败：{blob[:200]!r}")
            continue
        if len(local) > 512:
            code2, part, hh2, _ = nas.download(path, name, rng="bytes=0-99", tag="range_" + name)
            ok = code2 in (200, 206) and part == local[:100]
            print(f"         Range bytes=0-99 -> {code2} {len(part)}B match={ok} "
                  f"Content-Range={hh2.get('Content-range') or hh2.get('Content-Range')}")
            if not ok:
                failures += 1
    s = nas.stat(ROOT, "hello.txt")
    d0 = (s.get("datas") or [{}])[0]
    print(f"  stat hello.txt -> datas[0].filename={d0.get('filename')!r} filesize={d0.get('filesize')} "
          f"epochmt={d0.get('epochmt')} isfolder={d0.get('isfolder')}")
    print(f"\n  结论：{'✅ 全部通过（含 Range 206）' if failures == 0 else f'❌ {failures} 项失败'}")
    return failures


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("cmd", choices=["fixture", "download", "ls"])
    ap.add_argument("--small", action="store_true", help="大文件用 16 MiB 而不是 128 MiB")
    ap.add_argument("--password", default=None)
    a = ap.parse_args()
    pw = a.password
    if not pw:
        for cand in ("docs/测试环境.local.md", "../../docs/测试环境.local.md"):
            if os.path.exists(cand):
                pw = open(cand, encoding="utf-8").read().split("| 密码 | `")[1].split("`")[0]
                break
    if not pw:
        sys.exit("需要 --password，或从 docs/测试环境.local.md 读取")
    nas = Nas(pw)
    nas.login()
    if a.cmd == "fixture":
        build_fixture(nas, 16 if a.small else 128)
    elif a.cmd == "download":
        sys.exit(1 if verify_download(nas) else 0)
    else:
        print(json.dumps(nas.list(), ensure_ascii=False, indent=1)[:4000])


if __name__ == "__main__":
    main()
