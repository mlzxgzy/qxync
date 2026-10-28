#!/usr/bin/env python3
"""★ M41：真机日志「体量重放」估算 —— 按实现后的输出规则离线重算一份历史日志的体积。

为什么需要它：M41 的验收不能只靠「改了代码」——用户问的是「日志数据量太大，砍一些」，
必须给出**同一批真机日志**在旧规则 / 新规则下的行数与字节对比。真机日志是只读输入，
本脚本按已实现的降噪规则（见 docs/M41-日志降噪.md §3 站点表）重放一遍：

  * 逐路径重复型（EMFILE 家族 / 上传暂停 / 删除熔断 / 离线入缓冲）：同签名 60s（或 30s）
    窗口只留第一条，窗口内被压掉的条数写进保留那条的「另省略 N 条」后缀；
  * 逐目录对账失败：60s 窗口内最多 3 条样本 + 1 条汇总（每轮 3 条 + 每轮 1 条 WARN 的近似）；
  * 空转轮询 INFO：只保留「有实际动作」的行 + 每 10 分钟一条心跳；
  * 轮询异常 / 会话失效：按签名 60s 去重，消息从「整串 errors 数组」改成「条数 + 首条截断」；
  * 逐文件上传成功：每 60s 一条汇总（近一分钟 N 项 / B 字节 + 累计）；
  * 逐条离线回放：降为 debug（只在 RUST_LOG=debug 时出现），文件里不再有；
  * fuser 的 `[Not Implemented]`：同 opcode 60s 一条；
  * **其余类别原样保留**（不降级用户可见的一次性日志）。

口径（写进文档，别当精确账）：窗口用 60s 桶近似真实「每轮/每窗口」；被压掉的逐条在
`RUST_LOG=debug` 时仍会写在文件里（本估算按默认 INFO 级别 → 0 字节）；汇总行/后缀的
字节数按 tracing 实际前缀（时间戳 + 级别 + target）加消息长度算，误差在个位数百分比。

用法：
  python3 xtask/probe/m41_log_volume.py                 # 重放 ~/.local/state/qxync/log/qxyncd.log.*
  python3 xtask/probe/m41_log_volume.py f1.log f2.log   # 重放指定文件
  python3 xtask/probe/m41_log_volume.py --md            # 输出 markdown 表格（贴文档用）
"""

from __future__ import annotations

import argparse
import glob
import os
import re
import sys
from collections import defaultdict
from datetime import datetime, timezone

TS_RE = re.compile(
    r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z)\s+(\w+)\s+([\w:]+):\s?(.*)$", re.S
)
# 合成汇总行用的「每条日志固定前缀」样本（tracing 的默认格式，真机可查）。
PREFIX_HINT = {
    ("WARN", "qxyncd::sync"): "2026-10-26T00:00:00.000000Z  WARN qxyncd::sync: ",
    ("ERROR", "qxyncd::sync"): "2026-10-26T00:00:00.000000Z ERROR qxyncd::sync: ",
    ("INFO", "qxyncd::sync"): "2026-10-26T00:00:00.000000Z  INFO qxyncd::sync: ",
    ("WARN", "qxyncd::daemon"): "2026-10-26T00:00:00.000000Z  WARN qxyncd::daemon: ",
    ("INFO", "qxyncd::daemon"): "2026-10-26T00:00:00.000000Z  INFO qxyncd::daemon: ",
    ("INFO", "qxync_fuse"): "2026-10-26T00:00:00.000000Z  INFO qxync_fuse: ",
    ("INFO", "qxync_fuse::upload"): "2026-10-26T00:00:00.000000Z  INFO qxync_fuse::upload: ",
    ("INFO", "qxync_fuse::delete"): "2026-10-26T00:00:00.000000Z  INFO qxync_fuse::delete: ",
    ("WARN", "qxync_fuse"): "2026-10-26T00:00:00.000000Z  WARN qxync_fuse: ",
    ("WARN", "fuser"): "2026-10-26T00:00:00.000000Z  WARN fuser: ",
}
NOTE = "（过去一个窗口内另省略 {} 条同类）"


class Entry:
    __slots__ = ("ts", "level", "target", "msg", "raw", "size")

    def __init__(self, ts, level, target, msg, raw_lines):
        self.ts = ts
        self.level = level
        self.target = target
        self.msg = msg
        self.raw = join_lines(raw_lines)
        self.size = len(self.raw.encode())


def parse_ts(s: str) -> float:
    dt = datetime.strptime(s, "%Y-%m-%dT%H:%M:%S.%fZ").replace(tzinfo=timezone.utc)
    return dt.timestamp()


def join_lines(lines: list[str]) -> str:
    """物理行（各自带换行符）→ 一条记录文本（恰好保留原有换行，不重复）。"""
    return "".join(x if x.endswith("\n") else x + "\n" for x in lines)


def load(path: str) -> list[Entry]:
    entries: list[Entry] = []
    cur: list[str] | None = None

    def flush():
        if cur is None:
            return
        m = TS_RE.match(cur[0].rstrip("\n"))
        if not m:  # 极少数无时间戳的裸行 → 并入上一条
            if entries:
                entries[-1].raw += join_lines(cur)
                entries[-1].size = len(entries[-1].raw.encode())
            return
        ts, level, target, msg = m.groups()
        entries.append(Entry(parse_ts(ts), level, target, msg, cur))

    for line in open(path, "r", encoding="utf-8", errors="replace"):
        if TS_RE.match(line):
            flush()
            cur = [line]
        else:
            if cur is None:
                cur = [line]
            else:
                cur.append(line)
    flush()
    return entries


# ---------------------------------------------------------------- 规则实现

OS_ERR = {
    2: "ENOENT",
    24: "EMFILE",
    28: "ENOSPC",
    13: "EACCES",
}


def err_sig(msg: str) -> str:
    m = re.search(r"\(os error (\d+)\)", msg)
    if m:
        return OS_ERR.get(int(m.group(1)), f"os{m.group(1)}")
    tail = msg.rsplit(": ", 1)[-1]
    return tail[:40]


def prefix_for(level: str, target: str, sample: Entry | None) -> str:
    if (level, target) in PREFIX_HINT:
        return PREFIX_HINT[(level, target)]
    if sample is not None and sample.raw.startswith(sample.msg):
        idx = sample.raw.find(sample.msg)
        return sample.raw[:idx]
    return ""


class Window:
    """按 (签名, 时间桶) 记录放行；返回 (kept_bytes, summary_lines)。"""

    def __init__(self, window: float):
        self.window = window
        self.seen: dict[str, int] = {}
        self.kept_bytes = 0
        self.kept = 0
        self.dropped = 0
        self.prefix = None

    def admit(self, sig: str, ts: float, size: int, prefix: str) -> bool:
        b = int(ts // self.window)
        if self.seen.get(sig) == b:
            self.dropped += 1
            return False
        first = sig not in self.seen
        self.seen[sig] = b
        self.kept += 1
        note = 0 if first else len(NOTE.format(0).encode())
        self.kept_bytes += size + note
        self.prefix = prefix
        return True


def bucket_num(ts: float, window: float) -> int:
    return int(ts // window)


def estimate(entries: list[Entry]) -> tuple[dict, dict]:
    """返回 (before, after)：每个类别的 {lines, bytes}。"""
    cats: dict[str, dict] = defaultdict(lambda: {"lines": 0, "bytes": 0})
    after: dict[str, dict] = defaultdict(lambda: {"lines": 0, "bytes": 0})

    def add(d, name, lines, size):
        d[name]["lines"] += lines
        d[name]["bytes"] += size

    # 各类状态（按类别 + 签名 / 时间桶）
    wins = {
        "fuse-cache-create": Window(60),
        "fuse-state-load": Window(60),
        "fuse-state-save": Window(60),
        "fuse-delete-breaker": Window(60),
        "fuser-bridge": Window(600),  # 「未实现」类诊断：10 分钟一条足够
        "fuse-outbox-add": Window(30),
    }
    upload_bucket: dict[int, list[int]] = defaultdict(lambda: [0, 0])  # 60s 桶 -> [n, 字节]
    upload_done = [0, 0]  # 累计 [n, 字节]（汇总行里的「累计」字段）
    list_fail_bucket: dict[int, list[Entry]] = defaultdict(list)  # 60s 桶 -> 逐目录失败行
    poll_last_kept: float | None = None
    # 过渡式告警状态：只有「进入异常」那一刻才打，持续期间静默（与实现里的 latch 对齐）
    offline_warned = False  # 上传暂停：成功一次后复位
    anomaly_latch: tuple[str, float] | None = None  # (签名, 首次放行时刻)
    auth_latch: tuple[str, float] | None = None
    recover_upload: dict[int, int] = defaultdict(int)  # 300s 桶 -> 条数
    replay_interrupt: tuple[str, float] | None = None   # (中断种类, 首次放行时刻)

    for e in entries:
        msg, size, ts = e.msg, e.size, e.ts
        target, level = e.target, e.level
        name = "other"

        # ---- fuse：缓存文件 / 水合位图（EMFILE 家族）
        if msg.startswith(("创建缓存文件失败", "设置缓存文件大小失败")):
            name = "fuse-cache-create"
        elif msg.startswith("读取水合位图失败"):
            name = "fuse-state-load"
        elif msg.startswith("水合位图落盘失败"):
            name = "fuse-state-save"
        # ---- fuse：上传成功 / 网络暂停 / 恢复未完成 / 离线入缓冲 / 删除熔断
        elif msg.startswith("已上传 "):
            name = "fuse-upload-done"
        elif msg.startswith("上传因网络不可用暂停"):
            name = "fuse-upload-pause"
        elif msg.startswith("恢复未完成的上传"):
            name = "fuse-recover-upload"
        elif msg.startswith("离线操作已进入缓冲区"):
            name = "fuse-outbox-add"
        elif msg.startswith("本地删除被熔断"):
            name = "fuse-delete-breaker"
        elif msg.startswith("已取消 ") and "待上传作业" in msg:
            name = "fuse-upload-cancel"
        # ---- fuser log 桥
        elif target == "fuser" and "[Not Implemented]" in msg:
            name = "fuser-bridge"
        # ---- daemon：轮询行 / 异常 / 会话失效 / 逐目录对账失败 / 离线回放
        elif msg.startswith("同步轮询: "):
            name = "sync-poll"
        elif msg.startswith("轮询有异常"):
            name = "daemon-anomaly"
        elif msg.startswith("轮询遇会话失效（本轮已记录）"):
            name = "daemon-auth"
        elif "对账列举失败" in msg:
            name = "sync-list-fail"
        elif msg.startswith(("离线操作已回放", "离线操作回放命中幂等")):
            name = "sync-replay"
        elif msg.startswith("离线缓冲回放中断"):
            name = "sync-replay-interrupt"
        elif msg.startswith(("离线缓冲区回放中断", "离线缓冲区回放遇会话失效")):
            # daemon 侧同一次中断的第二层上报（M41 已降 debug → 默认不落盘）
            name = "daemon-replay-dupe"

        add(cats, name, 1, size)

        if name in (
            "fuse-cache-create",
            "fuse-state-load",
            "fuse-state-save",
            "fuse-delete-breaker",
            "fuser-bridge",
        ):
            win = wins[name]
            if name == "fuse-delete-breaker":
                sig = re.sub(r"\d+", "N", msg.split("）:", 1)[0])
            elif name == "fuser-bridge":
                sig = re.sub(r"\d+", "N", e.raw.split("\n", 1)[0])[:80]
            else:
                sig = err_sig(msg)
            if win.admit(sig, ts, size, prefix_for(level, target, e)):
                add(after, name, 1, size + (len(NOTE.format(0).encode()) if win.kept > 1 else 0))

        elif name == "fuse-upload-pause":
            # 故障迁移式：一次断网只报第一条；上传成功一次后复位
            if not offline_warned:
                offline_warned = True
                add(after, name, 1, size)

        elif name == "fuse-upload-done":
            offline_warned = False
            m = re.search(r"\((\d+) 字节\)$", msg)
            nbytes = int(m.group(1)) if m else 0
            upload_done[0] += 1
            upload_done[1] += nbytes
            b = bucket_num(ts, 60)
            upload_bucket[b][0] += 1
            upload_bucket[b][1] += nbytes

        elif name == "fuse-recover-upload":
            recover_upload[bucket_num(ts, 300)] += 1

        elif name == "fuse-outbox-add":
            kind = msg.split("）: ", 1)[-1].split(" ", 1)[0]
            b = bucket_num(ts, 30)
            sig = f"{kind}"
            win = wins[name]
            if win.admit(sig, ts, size, prefix_for(level, target, e)):
                add(after, name, 1, size + (len(NOTE.format(0).encode()) if win.kept > 1 else 0))

        elif name == "sync-list-fail":
            list_fail_bucket[bucket_num(ts, 60)].append(e)

        elif name == "sync-poll":
            ca = dict(
                (k, int(v))
                for k, v in re.findall(
                    r"(max_log|events|refreshed|uploaded|conflicts|deleted|blocked|baseline|dirs|full_dirs)=(\d+)",
                    msg,
                )
            )
            fs = "full_scan=false" not in msg
            active = any(
                ca.get(k, 0) > 0
                for k in ("events", "refreshed", "uploaded", "conflicts", "deleted", "blocked")
            ) or fs
            keep = active or poll_last_kept is None or (ts - poll_last_kept) >= 600
            if keep:
                poll_last_kept = ts
                add(after, name, 1, size)

        elif name == "daemon-anomaly":
            m = re.search(r"conflicts=(\d+) blocked=(\d+) errors=\[(.*)\]\s*$", msg, re.S)
            if not m:
                add(after, name, 1, size)
                continue
            c, b, arr = int(m.group(1)), int(m.group(2)), m.group(3)
            nerr = arr.count('", "') + 1 if '"' in arr else 0
            sig = f"c{c}b{b}n{nerr}"
            # 过渡式：签名变化 → 打；同一签名持续 → 静默；同类故障间隔 >5min 视为新一次
            if (
                anomaly_latch is not None
                and anomaly_latch[0] == sig
                and (ts - anomaly_latch[1]) <= 300
            ):
                continue
            anomaly_latch = (sig, ts)
            first = re.search(r'"(.*?)"(?:, |\])', arr, re.S)
            firsts = re.sub(r"\s+", " ", first.group(1)) if first else ""
            summary = f"轮询有异常: conflicts={c} blocked={b} errors=共 {nerr} 条；首条「{firsts[:120]}」"
            add(after, name, 1, len(prefix_for(level, target, e).encode()) + len(summary.encode()))

        elif name == "daemon-auth":
            if (
                auth_latch is not None
                and auth_latch[0] == "auth"
                and (ts - auth_latch[1]) <= 300
            ):
                continue
            auth_latch = ("auth", ts)
            summary = "轮询遇会话失效（本轮已记录），立即重登后下一轮继续: 共 20 条；首条「…」"
            add(after, name, 1, len(prefix_for(level, target, e).encode()) + len(summary.encode()))

        elif name == "sync-replay":
            # 逐条降 debug：默认 INFO 级别下不落盘（daemon 已有每轮回放汇总）
            pass

        elif name == "sync-replay-interrupt":
            # 门闩：一次回放打断只留第一条（同种类持续 → debug）；回放清空才复位，
            # 这里用 300s 无同类打断近似「一次打断事件」。
            sig = msg.split("：", 1)[0]
            if (
                replay_interrupt is not None
                and replay_interrupt[0] == sig
                and (ts - replay_interrupt[1]) <= 300
            ):
                continue
            replay_interrupt = (sig, ts)
            add(after, name, 1, size)

        elif name == "daemon-replay-dupe":
            # 与 sync-replay-interrupt 是同一次中断的两层上报，M41 已降 debug → 0
            pass

        else:  # 其余不变
            add(after, name, 1, size)

    # ---- 收尾：逐桶聚类的类别
    for b, es in list_fail_bucket.items():
        keep = min(3, len(es))
        for e in es[:keep]:
            add(after, "sync-list-fail", 1, e.size)
        if len(es) > keep:
            msg = (
                f"同步引擎: 本轮另有 {len(es) - keep} 条同类同步错误未逐条列出"
                "（每轮最多 3 条样本，明细见 debug 日志）"
            )
            add(
                after,
                "sync-list-fail",
                1,
                len(PREFIX_HINT[("WARN", "qxyncd::sync")].encode()) + len(msg.encode()),
            )

    for b, (n, nb) in upload_bucket.items():
        msg = (
            f"上传完成：近一分钟 {n} 项 / {nb} 字节"
            f"（累计 {upload_done[0]} 项 / {upload_done[1]} 字节）"
        )
        add(after, "fuse-upload-done", 1, len(PREFIX_HINT[("INFO", "qxync_fuse::upload")].encode()) + len(msg.encode()))

    for b, n in recover_upload.items():
        msg = f"恢复未完成的上传 {n} 项（逐条明细见 debug 日志）"
        add(after, "fuse-recover-upload", 1, len(PREFIX_HINT[("INFO", "qxync_fuse::upload")].encode()) + len(msg.encode()))

    return cats, after


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("files", nargs="*")
    ap.add_argument("--md", action="store_true", help="输出 markdown 表格")
    args = ap.parse_args()

    files = args.files or sorted(glob.glob(os.path.expanduser("~/.local/state/qxync/log/qxyncd.log.*")))
    if not files:
        print("没有日志文件可重放", file=sys.stderr)
        return 2

    before_total = {"lines": 0, "bytes": 0}
    after_total = {"lines": 0, "bytes": 0}
    agg_b: dict[str, dict] = defaultdict(lambda: {"lines": 0, "bytes": 0})
    agg_a: dict[str, dict] = defaultdict(lambda: {"lines": 0, "bytes": 0})

    print("M41 日志体量重放估算（旧规则 → 新规则；默认 INFO 级别，被压明细在 debug 不落盘）")
    for f in files:
        entries = load(f)
        b, a = estimate(entries)
        bt = {"lines": sum(v["lines"] for v in b.values()), "bytes": sum(v["bytes"] for v in b.values())}
        at = {"lines": sum(v["lines"] for v in a.values()), "bytes": sum(v["bytes"] for v in a.values())}
        for k in set(b) | set(a):
            for src, dst in ((b, agg_b), (a, agg_a)):
                dst[k]["lines"] += src.get(k, {}).get("lines", 0)
                dst[k]["bytes"] += src.get(k, {}).get("bytes", 0)
        for key in before_total:
            before_total[key] += bt[key]
            after_total[key] += at[key]
        drop = (1 - at["bytes"] / bt["bytes"]) * 100 if bt["bytes"] else 0
        print(f"  {os.path.basename(f):>26}: {bt['lines']:>7} 行 / {bt['bytes']:>9} B → "
              f"{at['lines']:>6} 行 / {at['bytes']:>8} B  (-{drop:.0f}%)")

    names = {
        "sync-list-fail": "对账逐目录失败 WARN",
        "sync-poll": "空转同步轮询 INFO",
        "sync-replay": "逐条离线回放 INFO",
        "sync-replay-interrupt": "outbox 回放中断（逐行 WARN）",
        "daemon-replay-dupe": "回放中断第二层上报（daemon）",
        "daemon-anomaly": "轮询有异常（抄 errors 数组）",
        "daemon-auth": "轮询会话失效（抄 errors 数组）",
        "fuse-cache-create": "创建缓存文件失败（EMFILE 等）",
        "fuse-state-load": "读取水合位图失败",
        "fuse-state-save": "水合位图落盘失败",
        "fuse-upload-done": "逐文件上传成功 INFO",
        "fuse-upload-pause": "上传因网络不可用暂停",
        "fuse-recover-upload": "恢复未完成的上传",
        "fuse-outbox-add": "离线操作进入缓冲区",
        "fuse-delete-breaker": "本地删除被熔断（逐项）",
        "fuse-upload-cancel": "取消子树待上传作业",
        "fuser-bridge": "fuser [Not Implemented]",
        "other": "其它（原样保留）",
    }
    rows = []
    for k in set(list(agg_b) + list(agg_a)):
        bb, aa = agg_b[k], agg_a[k]
        drop = (1 - aa["bytes"] / bb["bytes"]) * 100 if bb["bytes"] else 0
        rows.append((names.get(k, k), bb["lines"], bb["bytes"], aa["lines"], aa["bytes"], drop))
    rows.sort(key=lambda r: -r[2])

    if args.md:
        print("\n| 类别 | 旧 行 | 旧 字节 | 新 行 | 新 字节 | 降幅 |")
        print("|---|---:|---:|---:|---:|---:|")
        for r in rows:
            print(f"| {r[0]} | {r[1]} | {r[2]} | {r[3]} | {r[4]} | {r[5]:.0f}% |")
    else:
        print(f"\n{'类别':<32}{'旧行':>8}{'旧字节':>11}{'新行':>8}{'新字节':>10}{'降幅':>7}")
        for r in rows:
            print(f"{r[0]:<32}{r[1]:>8}{r[2]:>11}{r[3]:>8}{r[4]:>10}{r[5]:>6.0f}%")

    drop = (1 - after_total["bytes"] / before_total["bytes"]) * 100 if before_total["bytes"] else 0
    dlines = (1 - after_total["lines"] / before_total["lines"]) * 100 if before_total["lines"] else 0
    print(
        f"\n合计：{before_total['lines']} 行 / {before_total['bytes']} B"
        f" → {after_total['lines']} 行 / {after_total['bytes']} B"
        f"（行 -{dlines:.0f}%，字节 -{drop:.0f}%）"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
