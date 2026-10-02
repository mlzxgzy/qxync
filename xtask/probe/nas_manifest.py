#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
nas_manifest.py —— NAS 数据基线快照 / 差异比对（**只读**）

为什么需要它：
    M8 的执行硬约束之一是「绝不损坏 NAS 上已有数据」。任何写操作之前先把当前
    远端树（路径 + 大小 + mtime + 类型）落盘成 manifest，改完之后 `--diff` 比对，
    就能**用证据证明**我们没动过不该动的字节。

用法：
  # 1) 立基线（只读）
  python3 nas_manifest.py --host nas.example.com --port 9834 --https --insecure \
          --user test1 --password '***' --roots /home --out .local-run/nas-baseline.json

  # 2) 事后比对（只读）—— 退出码 0 = 完全一致；1 = 有差异（会逐条列出）
  python3 nas_manifest.py ... --roots /home --out /tmp/after.json --diff .local-run/nas-baseline.json

判定：
  * `--diff` 只报告 **新增 / 删除 / 大小变化 / mtime 变化** 四类；
  * 「mtime 变化」单独分类，因为同步引擎回写文件会改 mtime（预期内）；
  * 「删除」是**最危险**的一类，任何未预期的删除都要当成事故处理。

依赖：标准库 + 同目录 qs_probe.py。
"""

import argparse, json, os, sys, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from qs_probe import Client, do_login  # noqa: E402

QSYNC = "cgi-bin/qsync/qsyncsrv.cgi"
# 永不下钻的目录（避免把缓存/回收站等噪声算进基线）
PRUNE = {".Qsync", "@Recycle", "#recycle", ".@__thumb", ".wdmc", ".AppleDouble"}


def list_dir(c, sid, path, limit=500):
    """列一个目录，返回 datas 数组（失败返回 []）。"""
    p = {"func": "get_list", "sid": sid, "is_iso": "0", "list_mode": "all",
         "path": path, "dir": "ASC", "limit": str(limit), "sort": "filename",
         "start": "0", "no_sort": "0", "hidden_file": "1"}
    code, text, _ = c.request(QSYNC, p, tag=None)
    if code != 200:
        return None
    try:
        d = json.loads(text)
    except Exception:
        return None
    if not isinstance(d, dict):
        return None
    datas = d.get("datas")
    if datas is None:
        return None
    return datas


def walk(c, sid, root, max_depth, max_entries, verbose=False):
    """广度优先递归；返回 {abs_path: {size, mtime, folder}}。"""
    out = {}
    queue = [(root, 0)]
    errors = []
    while queue:
        path, depth = queue.pop(0)
        datas = list_dir(c, sid, path)
        if datas is None:
            errors.append(path)
            continue
        for e in datas:
            if not isinstance(e, dict):
                continue
            name = e.get("filename") or e.get("name")
            if not name or name in (".", ".."):
                continue
            full = path.rstrip("/") + "/" + name
            is_folder = bool(e.get("isfolder")) or e.get("isfolder") == 1
            try:
                size = int(e.get("filesize") or 0)
            except Exception:
                size = 0
            mt = e.get("mt") or e.get("epochmt") or ""
            out[full] = {"size": size, "mtime": str(mt), "folder": is_folder}
            if len(out) >= max_entries:
                queue = []
                break
            if is_folder and depth + 1 <= max_depth and name not in PRUNE:
                queue.append((full, depth + 1))
        if verbose:
            print(f"    … {path}  (累计 {len(out)} 项, 待列 {len(queue)})", file=sys.stderr)
    return out, errors


def diff(base, now):
    """返回四类差异。"""
    bk, nk = set(base), set(now)
    added = sorted(nk - bk)
    removed = sorted(bk - nk)
    resized, retimed = [], []
    for k in sorted(bk & nk):
        b, n = base[k], now[k]
        if b.get("size") != n.get("size"):
            resized.append({"path": k, "was": b.get("size"), "now": n.get("size"),
                            "was_mtime": b.get("mtime"), "now_mtime": n.get("mtime")})
        elif str(b.get("mtime")) != str(n.get("mtime")):
            retimed.append({"path": k, "was": b.get("mtime"), "now": n.get("mtime")})
    return {"added": added, "removed": removed, "resized": resized, "retimed": retimed}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", required=True)
    ap.add_argument("--port", type=int, default=9834)
    ap.add_argument("--https", action="store_true")
    ap.add_argument("--insecure", action="store_true")
    ap.add_argument("--prefix", default="")
    ap.add_argument("--user", default="test1")
    ap.add_argument("--password", default="")
    ap.add_argument("--roots", nargs="+", required=True, help="要快照的远端根（绝对路径）")
    ap.add_argument("--max-depth", type=int, default=8)
    ap.add_argument("--max-entries", type=int, default=20000)
    ap.add_argument("--out", required=True)
    ap.add_argument("--diff", default=None, help="与已有 manifest 比对")
    ap.add_argument("--verbose", action="store_true")
    a = ap.parse_args()

    c = Client(a.host, a.port, a.https, a.insecure, a.prefix)
    c.verbose = False
    sid, _ = do_login(c, a.user, a.password, {})
    if not sid:
        print("❌ 登录失败")
        return 2

    entries, errors = {}, {}
    for root in a.roots:
        got, errs = walk(c, sid, root, a.max_depth, a.max_entries, a.verbose)
        entries.update(got)
        if errs:
            errors[root] = errs
        print(f"  {root}: {len(got)} 项" + (f"（{len(errs)} 个目录列不出）" if errs else ""))

    now = {"ts": time.strftime("%Y-%m-%dT%H:%M:%S"), "user": a.user,
           "roots": a.roots, "entries": entries, "errors": errors}
    os.makedirs(os.path.dirname(os.path.abspath(a.out)) or ".", exist_ok=True)
    with open(a.out, "w", encoding="utf-8") as f:
        json.dump(now, f, ensure_ascii=False, indent=1, sort_keys=True)
    print(f"✅ manifest 落盘：{a.out}（{len(entries)} 项）")

    if a.diff:
        with open(a.diff, encoding="utf-8") as f:
            base = json.load(f)
        d = diff(base.get("entries", {}), entries)
        n = sum(len(v) for v in d.values())
        print(f"\n==== 与基线比对（{a.diff}，基线 {len(base.get('entries', {}))} 项）====")
        for kind in ("added", "removed", "resized", "retimed"):
            items = d[kind]
            print(f"  {kind:8s}: {len(items)}")
            for it in items[:40]:
                print(f"      {it}")
            if len(items) > 40:
                print(f"      … 还有 {len(items) - 40} 条")
        if n == 0:
            print("\n✅ 完全一致：没有新增 / 删除 / 大小变化 / mtime 变化。")
            return 0
        print(f"\n⚠️  共 {n} 处差异（removed 最危险，请逐条确认是否预期）。")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
