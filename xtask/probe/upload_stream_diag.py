"""诊断 upload.php 对「流式 multipart」的真实反应（M15/T2）。

背景：`Client::upload_bytes`（Part::bytes，整块 Vec）能上传成功，
`upload_stream`（Body::wrap_stream）返回了非 JSON 响应。
要确认是**协议不兼容**（chunked/无 Content-Length 被服务端拒），
还是编码/字段名之类的其它问题。

做法：同一个文件、同一套表单字段，分别用
  A) 整块body（有 Content-Length）
  B) chunked body（无 Content-Length，流式）
打到 upload.php，把两边的**原始响应**都打出来对比。
"""
import argparse
import base64
import json
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
            "client_agent": "QsyncUploadDiag", "gen_client_id": "1",
            "remme": "1", "pwd": base64.b64encode(password.encode()).decode(),
        }
    )
    body = get(f"{base}/cgi-bin/authLogin.cgi?{q}").decode("utf-8", "replace")
    m = re.search(r"<authSid><!\[CDATA\[([^\]]+)\]\]></authSid>", body)
    if not m:
        print("登录失败:", body[:300])
        sys.exit(1)
    return m.group(1)


def multipart_body(name, content):
    bd = "----qx" + uuid.uuid4().hex
    body = (
        f"--{bd}\r\n"
        f'Content-Disposition: form-data; name="files[]"; filename="{name}"\r\n'
        f"Content-Type: application/octet-stream\r\n\r\n"
    ).encode() + content + f"\r\n--{bd}--\r\n".encode()
    return body, f"multipart/form-data; boundary={bd}"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", required=True)
    ap.add_argument("--port", type=int, default=9834)
    ap.add_argument("--user", required=True)
    ap.add_argument("--password", required=True)
    ap.add_argument("--dir", required=True, help="目标目录，如 /home/qxync-test")
    a = ap.parse_args()
    base = f"https://{a.host}:{a.port}"
    sid = login(base, a.user, a.password)
    up = (
        f"{base}/cgi-bin/qsync/upload.php?"
        + urllib.parse.urlencode(
            {"sid": sid, "dest_path": a.dir, "overwrite": "1", "type": "standard"}
        )
    )

    content = b"stream upload diagnostic payload\n"
    body, ctype = multipart_body("diag-a-plain.bin", content)

    # A) 有 Content-Length（reqwest 对 Part::bytes 就是这么发的）
    req = urllib.request.Request(up, data=body, headers={"Content-Type": ctype})
    try:
        with urllib.request.urlopen(req, context=CTX, timeout=120) as r:
            code, txt = r.status, r.read().decode("utf-8", "replace")
        print(f"A) 整块 body（有 Content-Length）: HTTP {code}")
        print(f"   响应: {txt[:400]}")
    except Exception as e:
        print(f"A) 整块 body 失败: {type(e).__name__}: {e}")

    # B) chunked（无 Content-Length）—— 流式上传走的就是这条
    req = urllib.request.Request(up, data=body, headers={"Content-Type": ctype})
    # urllib 默认对 data 设Content-Length；显式删掉并声明 chunked
    print(f"   （reqwest 流式会用 Transfer-Encoding: chunked）")


if __name__ == "__main__":
    main()
