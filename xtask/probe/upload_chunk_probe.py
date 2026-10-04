"""M15/T4 前置验证：Qsync 服务端**到底支不支持分片续传**。

结论（2026-10-04，两条独立证据）：

  1. **静态分析**：`QsyncServer_5.0.0.8_20260916` 的 `cgi/qsyncsrv.cgi`（ELF x86-64）
     里完整实现了分片上传，`objdump -d` 逐条对上：

     | func | 处理函数 | 说明 |
     | --- | --- | --- |
     | `start_chunked_upload` | `0x6973c` | 申请 upload_id，参数 `ssid` + `upload_root_dir` |
     | `get_chunked_upload_status` | `0x6cae8` | 查已收分片 |
     | `delete_chunked_upload_file` | `0x6d1c9` | 清分片缓存 |

     另有 `op_chunked_upload` / `op_combine_upload` / `op_get_chunked_upload_id` /
     `op_repository_upload` / `op_versioning_list_chunk` 五个内部符号（`upload.c`），
     其中 `op_chunked_upload` 读 `offset` / `filesize` / `upload_id` / `upload_root_dir` /
     `upload_name` / `overwrite` / `check_sum`，分片落盘到
     `snprintf(buf, 0x400, "%s/%s", upload_root_dir, ".@upload_cache")`。

     `start_chunked_upload` 成功时回
     `{"status":1,"home_folder_available":1,"upload_id":"tmp-XXXXXXXX"}`
     （`tmp-` + `CGI_Get_Upload_ID` 生成的 8 位随机，12 字符）。
     `upload_id` 长度约束 `[2,32]`（`sub rax,0x2; cmp rax,0x1e; jbe`）+ `Is_Alphabet_Num`。

  2. **真机实测**：`start_chunked_upload` 走 `func=` 分发表（`strcmp` 链），
     `op_*` 那几个**不在**分发表里（打过去 HTTP 200 空 body）。

     ⚠️ **响应有两段 JSON**，第一段 `{"status":0,...}` 是框架头，
     第二段才是 handler 结果 —— 只抓第一个 `status` 会误判。

     | upload_root_dir | 结果 |
     | --- | --- |
     | `/home/qxync-t4probe`（存在的普通目录） | `status 12`（路径未通过 Qbox 校验） |
     | `/remote:qxync-t4probe`（Qbox 形态） | `status 46` |

     普通路径走 `0x6927c` → `0x9d02a`（realpath + UID 校验），
     `/remote:` 前缀走另一分支。**两者都没到返回 upload_id 那一步**：
     `start_chunked_upload` 要求 `upload_root_dir` 是 **Qbox 空间路径**
     （`/remote:` 前缀 + `name@uid`，见 `0x9d02a` 里的 `/remote:` 与 `0x118daf="@"`），
     而 `func=upload.php` 的 `dest_path` 用的是**普通绝对路径**。
     这两套路径空间不通，**普通家目录上传走不了分片通道**。

结论落点：**T4 在这台 NAS / 这条路径空间上做不了**。
`upload.php`（普通路径）没有分片入口，`qsyncsrv.cgi` 的分片入口只服务 Qbox 空间。
Qsync 官方客户端能续传，是因为它对 Qbox 空间用分片、对普通家目录直接整传。

## 用法

```bash
python3 upload_chunk_probe.py --host nas.example.com --port 9834 \
    --user test1 --password 'xxx' --dir /home/qxync-t4probe
```

⚠️ 只往指定目录写，跑完可手工 `rm -rf`。
"""
import argparse
import base64
import re
import ssl
import sys
import urllib.error
import urllib.parse
import urllib.request

CTX = ssl.create_default_context()
CTX.check_hostname = False
CTX.verify_mode = ssl.CERT_NONE


def get(url, timeout=60):
    with urllib.request.urlopen(url, context=CTX, timeout=timeout) as r:
        return r.status, r.read()


def login(base, user, password):
    q = urllib.parse.urlencode(
        {
            "user": user, "serviceKey": "1", "client_app": "Qsync",
            "client_agent": "QsyncChunkProbe", "gen_client_id": "1",
            "remme": "1", "pwd": base64.b64encode(password.encode()).decode(),
        }
    )
    _, body = get(f"{base}/cgi-bin/authLogin.cgi?{q}")
    txt = body.decode("utf-8", "replace")
    m = re.search(r"<authSid><!\[CDATA\[([^\]]+)\]\]></authSid>", txt)
    if not m:
        print("登录失败:", txt[:300])
        sys.exit(1)
    return m.group(1)


def call(base, sid, func, **params):
    q = {"func": func, "sid": sid}
    q.update({k: v for k, v in params.items() if v is not None})
    url = f"{base}/cgi-bin/qsync/qsyncsrv.cgi?" + urllib.parse.urlencode(q)
    try:
        code, body = get(url)
        return code, body.decode("utf-8", "replace").replace("\n", " ").strip()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace").replace("\n", " ").strip()
    except Exception as e:  # noqa: BLE001
        return -1, f"{type(e).__name__}: {e}"


def statuses(txt):
    """⚠️ 响应有**两段** JSON：框架头 status 0 + handler 的真实 status。
    只抓第一个会误判成「全都不支持」。"""
    return [int(x) for x in re.findall(r'"status":\s*(\d+)', txt)]


def handler_status(txt):
    """handler 的真实 status = 最后一个；status 0 是框架头。"""
    st = statuses(txt)
    real = [x for x in st if x != 0]
    return real[-1] if real else (st[-1] if st else None)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", required=True)
    ap.add_argument("--port", type=int, default=9834)
    ap.add_argument("--user", required=True)
    ap.add_argument("--password", required=True)
    ap.add_argument("--dir", default="/home/qxync-t4probe")
    a = ap.parse_args()

    base = f"https://{a.host}:{a.port}"
    sid = login(base, a.user, a.password)
    print(f"登录 OK  sid={sid[:6]}…  目录={a.dir}\n")

    # ---------- 1) func 分发表：哪些名字真的被 dispatch ----------
    print("=== 1) 各 func 的真实 handler status（0=框架头，不是结果） ===")
    for f in [
        "start_chunked_upload", "get_chunked_upload_status", "delete_chunked_upload_file",
        "op_chunked_upload", "op_get_chunked_upload_id", "op_get_chunked_upload_status",
        "op_combine_upload", "op_upload", "op_repository_upload",
    ]:
        code, txt = call(base, sid, f, ssid=sid, upload_root_dir=a.dir)
        hst = handler_status(txt)
        got_id = "upload_id" in txt
        print(f"  {f:32s} statuses={statuses(txt)!s:18s} handler={hst!s:5s} "
              f"{'← 拿到 upload_id' if got_id else ''}")

    # ---------- 2) upload_root_dir 路径形态 ----------
    print("\n=== 2) start_chunked_upload 的 upload_root_dir 形态 ===")
    print("    （普通绝对路径走 0x9d02a realpath+UID 校验；`/remote:` 是 Qbox 空间）")
    cands = [
        a.dir, a.dir + "/", "/home", "/",
        "/remote:" + a.dir, "/remote:home", "/remote:home@1000",
    ]
    for p in cands:
        code, txt = call(base, sid, "start_chunked_upload", ssid=sid, upload_root_dir=p)
        st = statuses(txt)
        print(f"  {p!r:34} statuses={st!s:18s} "
              f"{'← 拿到 upload_id' if 'upload_id' in txt else ''}")

    # ---------- 3) 结论 ----------
    print("\n=== 3) 结论 ===")
    code, txt = call(base, sid, "start_chunked_upload", ssid=sid, upload_root_dir=a.dir)
    if "upload_id" in txt:
        print("  ✅ 服务端给了 upload_id —— 分片通道可用，T4 可以做")
        print("  完整响应:", txt[:300])
    else:
        print("  ❌ 普通家目录路径拿不到 upload_id —— 分片通道只服务 Qbox 空间")
        print(f"     start_chunked_upload({a.dir}) → {txt[:200]}")
        print("  → M15/T4 在这条路径空间上做不了。详见本文件顶部的结论段。")
        print("  → 可选替代：不改协议，改本地策略（失败后整文件重传 + 指数退避），")
        print("     这是 T2 现有 `max_attempts` 已经在做的事，只需调参 + 补进度上报（T8）。")


if __name__ == "__main__":
    main()
