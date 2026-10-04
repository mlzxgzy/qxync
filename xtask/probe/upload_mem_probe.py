"""M15/T2 验收：量「流式上传」与「整文件读入内存」的实际内存占用差。

验收标准是「上传 4 GB 文件，进程 RSS 峰值 < 200 MB」。本机/tmp 只有 10 MB、
造不出 4 GB 文件，所以退一步做**同规模对照**：同一份 N 字节文件，
分别用两条路径上传，采样进程 RSS 峰值。

对照组的非流式路径可以直接用 `upload_bytes`（它要求调用方先 `fs::read` 整文件），
本脚本用 /proc/<pid>/status 的 VmHWM（峰值 RSS）取数。

用法：
  python3 upload_mem_probe.py --host nas --port 9834 --user test1 --password '***' \
      --dir /home/qxync-test --size-mb 64
"""
import argparse
import base64
import os
import re
import ssl
import sys
import urllib.parse
import urllib.request
import uuid

CTX = ssl.create_default_context()
CTX.check_hostname = False
CTX.verify_mode = ssl.CERT_NONE


def get(url, timeout=60):
    with urllib.request.urlopen(url, context=CTX, timeout=timeout) as r:
        return r.read()


def login(base, user, password):
    q = urllib.parse.urlencode(
        {
            "user": user, "serviceKey": "1", "client_app": "Qsync",
            "client_agent": "QsyncMemProbe", "gen_client_id": "1",
            "remme": "1", "pwd": base64.b64encode(password.encode()).decode(),
        }
    )
    body = get(f"{base}/cgi-bin/authLogin.cgi?{q}").decode("utf-8", "replace")
    m = re.search(r"<authSid><!\[CDATA\[([^\]]+)\]\]></authSid>", body)
    if not m:
        print("登录失败:", body[:300])
        sys.exit(1)
    return m.group(1)


def peak_rss_kb(pid):
    """/proc/<pid>/status 的 VmHWM = 峰值 RSS（VmRSS 是当前值）。"""
    try:
        with open(f"/proc/{pid}/status", encoding="utf-8") as f:
            for line in f:
                if line.startswith("VmHWM:"):
                    return int(line.split()[1])
    except OSError:
        pass
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", required=True)
    ap.add_argument("--port", type=int, default=9834)
    ap.add_argument("--user", required=True)
    ap.add_argument("--password", required=True)
    ap.add_argument("--dir", required=True)
    ap.add_argument("--size-mb", type=int, default=64)
    a = ap.parse_args()

    base = f"https://{a.host}:{a.port}"
    sid = login(base, a.user, a.password)
    n = a.size_mb * 1024 * 1024

    # 造 N 字节文件在磁盘上（分块写，不占内存）
    tmpdir = os.environ.get("QXYNC_TEST_TMPDIR", "/home/kami/项目/qxync/target/tmp")
    os.makedirs(tmpdir, exist_ok=True)
    path = os.path.join(tmpdir, f"qxync-mem-{n}.bin")
    print(f"造 {n >> 20} MiB 文件 {path} ...")
    chunk = bytes(range(256)) * 4096  # 1 MiB
    with open(path, "wb") as f:
        for _ in range(n // len(chunk)):
            f.write(chunk)

    up = (
        f"{base}/cgi-bin/qsync/upload.php?"
        + urllib.parse.urlencode(
            {"sid": sid, "dest_path": a.dir, "overwrite": "1", "type": "standard"}
        )
    )

    #流式：边读边发（每块 1 MiB），不把整个文件放进内存
    pid = os.getpid()
    before = peak_rss_kb(pid)
    print(f"基线 VmHWM = {before} kB")
    name = f"memprobe-stream-{a.size_mb}mb.bin"
    sent = stream_upload(up, path, name)
    peak = peak_rss_kb(pid)
    print(
        f"流式上传 {n >> 20} MiB：发出 {sent >> 20} MiB，"
        f"VmHWM 峰值 {peak} kB（增量 {None if (peak is None or before is None) else peak - before} kB）"
    )
    os.remove(path)

    # 对照：整文件读进内存再发（=旧 upload_bytes 的行为）
    name2 = f"memprobe-bytes-{a.size_mb}mb.bin"
    before2 = peak_rss_kb(pid)
    with open(path, "rb") as f:
        data = f.read()
    print(f"整块读入后 VmHWM = {peak_rss_kb(pid)} kB")
    post(up, name2, data)
    peak2 = peak_rss_kb(pid)
    print(f"非流式上传 {n >> 20} MiB：VmHWM 峰值 {peak2} kB")
    os.remove(path)

    print("\n=== 结论")
    print(f"流式峰值 {peak} kB vs 非流式峰值 {peak2} kB")
    if peak and peak2:
        print(f"非流式比流式多占 {(peak2 - peak) / 1024:.1f} MiB")


def _multipart(name, content_iter, total):
    """返回 (headers, body_iterator)。content_iter 逐块产出 bytes。"""
    bd = "----qx" + uuid.uuid4().hex
    head = (
        f"--{bd}\r\n"
        f'Content-Disposition: form-data; name="files[]"; filename="{name}"\r\n'
        f"Content-Type: application/octet-stream\r\n\r\n"
    ).encode()
    tail = f"\r\n--{bd}--\r\n".encode()

    def body_iter():
        yield head
        for c in content_iter:
            yield c
        yield tail

    return {"Content-Type": f"multipart/form-data; boundary={bd}"}, body_iter, total + len(head) + len(tail)


def stream_upload(url, path, name):
    """按块读文件发出去（不把整个文件放进内存）。"""
    total = os.path.getsize(path)

    def gen():
        with open(path, "rb") as f:
            while True:
                b = f.read(1024 * 1024)
                if not b:
                    return
                yield b

    headers, body_iter, length = _multipart(name, gen(), total)
    headers["Content-Length"] = str(length)
    req = urllib.request.Request(url, data=None, headers=headers, method="POST")
    # urllib 不接受生成器 body，这里手动分块发送
    h = urllib.request.HTTPSConnection(
        url.split("/")[2].split(":")[0], int(url.split(":")[2].split("/")[0]),
        context=CTX, timeout=600,
    )
    path_q = "/" + url.split("/", 3)[3]
    h.putrequest("POST", path_q, skip_accept_encoding=True)
    for k, v in headers.items():
        h.putheader(k, v)
    h.endheaders()
    sent = 0
    for chunk in body_iter():
        h.send(chunk)
        sent += len(chunk)
    resp = h.getresponse()
    body = resp.read().decode("utf-8", "replace")
    print(f"  服务端: HTTP {resp.status} {body[:160]}")
    return sent - total if sent > total else sent


def post(url, name, data):
    bd = "----qx" + uuid.uuid4().hex
    body = (
        f"--{bd}\r\n"
        f'Content-Disposition: form-data; name="files[]"; filename="{name}"\r\n'
        f"Content-Type: application/octet-stream\r\n\r\n"
    ).encode() + data + f"\r\n--{bd}--\r\n".encode()
    req = urllib.request.Request(
        url, data=body, headers={"Content-Type": f"multipart/form-data; boundary={bd}"}
    )
    with urllib.request.urlopen(req, context=CTX, timeout=600) as r:
        print(f"  服务端: HTTP {r.status} {r.read().decode('utf-8','replace')[:160]}")


if __name__ == "__main__":
    main()
