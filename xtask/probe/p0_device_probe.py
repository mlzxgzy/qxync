#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
p0_device_probe.py —— P0 探针线：设备注册 / 计算机名称（见 docs/M8-向Qsync-Client-6靠拢.md §11）

目的：验证 `qbox_save_device_config` 能不能把本机注册成 Qsync 设备，以及**注册后
      我们自己的写操作是否终于会产生 sync log 事件**（M2c 的事件快路径一直不工作，
      README 记录的怀疑原因是「本机未做设备配对」）。

⚠️ 分阶段、默认只读：
    --phase a   （默认）只读侦察：登录 + 列出现有设备/配置列表 + 收集候选 device_uid
    --phase b   影子写入：注册一次，然后立刻读回对比（**会改 NAS 上的设备列表**）
    --phase c   事件验证：本地写一个文件，看 sync log 是否出现本机事件

用法：
  python3 p0_device_probe.py --host nas.example.com --port 9834 --https --insecure \
          --user test1 --password '***' --phase a

依赖：标准库 + 同目录 qs_probe.py（复用 Client / do_login）。
"""

import argparse, json, os, sys, time, urllib.parse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from qs_probe import Client, do_login, xml_pick  # noqa: E402

QSYNC = "cgi-bin/qsync/qsyncsrv.cgi"
UTIL = "cgi-bin/filemanager/utilRequest.cgi"

# device_uid 的候选来源：登录响应字段 / nas_uid 响应字段
UID_FIELDS = ["uid", "suid", "cuid", "user_cuid", "nas_uid", "device_uid", "duid", "gid"]


def jget(text, *keys):
    """从 JSON 文本里逐个 key 取第一个非空值（浅层 + data 内一层）。"""
    try:
        doc = json.loads(text)
    except Exception:
        return None
    cands = [doc]
    if isinstance(doc, dict) and isinstance(doc.get("data"), (dict, list)):
        cands.append(doc["data"])
    for c in cands:
        if isinstance(c, dict):
            for k in keys:
                v = c.get(k)
                if v not in (None, "", 0):
                    return v
        if isinstance(c, list):
            for item in c:
                if isinstance(item, dict):
                    for k in keys:
                        v = item.get(k)
                        if v not in (None, "", 0):
                            return v
    return None


def dump(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)


def call(c, path, params, tag):
    """发一次请求，返回 (code, text)；同时把原始响应落盘。"""
    code, text = c.retry(path, params, None, tag=tag)
    return code, text


def phase_a(c, sid, user, outdir):
    """只读侦察。"""
    print("\n" + "#" * 78)
    print("# P0.1 只读侦察（不动 NAS）")
    print("#" * 78)
    facts = {}

    # --- 1. 现有设备 / 配置列表（基线）
    baseline = {}
    for lower, upper, label in [(0, 500, "0-500"), (0, 50, "0-50")]:
        p = {"func": "qbox_get_device_config_list", "sid": sid, "user": user,
             "lower": str(lower), "upper": str(upper)}
        code, text = call(c, QSYNC, p, tag=f"p0_device_config_list_{label}")
        dump(os.path.join(outdir, f"device_config_list_{label}.txt"), text)
        baseline[label] = text
        print(f"  → device_config_list[{label}] code={code} len={len(text)}")

    # --- 2. NAS UID / 候选 device_uid
    code, text = call(c, QSYNC, {"func": "qbox_get_nas_uid", "sid": sid}, tag="p0_nas_uid")
    dump(os.path.join(outdir, "nas_uid.txt"), text)
    facts["nas_uid_raw"] = text
    for f in UID_FIELDS:
        v = jget(text, f)
        if v:
            facts[f"nas_uid.{f}"] = v

    # --- 3. max_log（事件基线的水位）
    code, text = call(c, QSYNC, {"func": "qbox_get_max_log", "sid": sid}, tag="p0_max_log")
    dump(os.path.join(outdir, "max_log.txt"), text)
    facts["max_log_raw"] = text

    # --- 4. 设备配置的 utilRequest 变体（存在两套命名空间）
    code, text = call(c, UTIL, {"func": "qbox_get_device_config_list", "sid": sid,
                                "user": user, "lower": "0", "upper": "500"},
                      tag="p0_device_config_list_util")
    dump(os.path.join(outdir, "device_config_list_util.txt"), text)

    # --- 5. 本机身份相关（用于猜 device_uid 的形态）
    for func in ["qbox_get_qbox_info", "qbox_get_static_information"]:
        code, text = call(c, QSYNC, {"func": func, "sid": sid}, tag=f"p0_{func}")
        dump(os.path.join(outdir, f"{func}.txt"), text)

    print("\n  ---- 候选 device_uid ----")
    for k, v in facts.items():
        if k.endswith("_raw"):
            continue
        print(f"    {k} = {v}")

    with open(os.path.join(outdir, "facts_a.json"), "w", encoding="utf-8") as f:
        json.dump(facts, f, ensure_ascii=False, indent=1)
    return facts


def phase_b(c, sid, user, outdir, device_uid, config_number):
    """影子写入：注册一次并立刻读回。"""
    print("\n" + "#" * 78)
    print("# P0.2 影子写入（qbox_save_device_config）")
    print("#" * 78)
    print(f"  device_uid = {device_uid}   config_number = {config_number}")

    p = {"func": "qbox_save_device_config", "sid": sid, "device_uid": device_uid,
         "user": user, "apply": "1"}
    if config_number is not None:
        p["config_number"] = str(config_number)
    code, text = call(c, QSYNC, p, tag="p0_save_device_config")
    dump(os.path.join(outdir, "save_device_config.txt"), text)
    print(f"  → save_device_config code={code} len={len(text)}")

    # 立刻读回
    for lower, upper, label in [(0, 500, "after-0-500")]:
        code, text = call(c, QSYNC, {"func": "qbox_get_device_config_list", "sid": sid,
                                     "user": user, "lower": str(lower), "upper": str(upper)},
                          tag=f"p0_device_config_list_{label}")
        dump(os.path.join(outdir, f"device_config_list_{label}.txt"), text)
        print(f"  → 读回 device_config_list[{label}] code={code} len={len(text)}")

    # utilRequest 变体也读一次
    code, text = call(c, UTIL, {"func": "qbox_get_device_config_list", "sid": sid,
                                "user": user, "lower": "0", "upper": "500"},
                      tag="p0_device_config_list_util_after")
    dump(os.path.join(outdir, "device_config_list_util_after.txt"), text)
    print(f"  → 读回 device_config_list[util] code={code} len={len(text)}")


def phase_c(c, sid, user, outdir, remote_marker):
    """事件验证：读一次 max_log 基线 → 提示做一次本地写 → 再读事件。"""
    print("\n" + "#" * 78)
    print("# P0.3 事件验证")
    print("#" * 78)
    code, text = call(c, QSYNC, {"func": "qbox_get_max_log", "sid": sid}, tag="p0c_max_log_before")
    dump(os.path.join(outdir, "max_log_before.txt"), text)
    print(f"  max_log(before) = {text.strip()[:300]}")

    code, text = call(c, QSYNC, {"func": "qbox_get_sync_log", "sid": sid,
                                 "lower": "0", "number": "50"}, tag="p0c_sync_log_before")
    dump(os.path.join(outdir, "sync_log_before.txt"), text)
    print(f"  sync_log(before) = {text.strip()[:300]}")

    print(f"""
  ---- 现在请做一次本地写操作，然后重跑 --phase c 复看 ----
  例：qsync put <本地小文件> {remote_marker}      （或 qsync sync --once）
  目标远端路径：{remote_marker}
""")


def phase_d(c, sid, user, outdir, max_log):
    """只读：把 sync log / notify 区间扫一遍，看事件到底有没有、是谁产生的。"""
    print("\n" + "#" * 78)
    print("# P0.4 sync log 区间扫描（只读）")
    print("#" * 78)

    # 1. 全量区间
    for lower, number, tag in [(0, 400, "full"), (max(0, max_log - 60), 60, "tail")]:
        p = {"func": "qbox_get_sync_log", "sid": sid, "lower": str(lower), "number": str(number)}
        code, text = call(c, QSYNC, p, tag=f"p0d_sync_log_{tag}")
        dump(os.path.join(outdir, f"sync_log_{tag}.txt"), text)
        print(f"  → sync_log[{tag}] lower={lower} code={code} len={len(text)}")

    # 2. get_detail 变体
    code, text = call(c, QSYNC, {"func": "qbox_get_sync_log", "sid": sid, "lower": "0",
                                 "number": "400", "get_detail": "1", "sub_folder": "/"},
                      tag="p0d_sync_log_detail")
    dump(os.path.join(outdir, "sync_log_detail.txt"), text)
    print(f"  → sync_log[detail] code={code} len={len(text)}")

    # 3. notify 两条游标
    for func, lo, hi, tag in [("qbox_query_notify", 0, 400, "query_notify"),
                              ("qbox_get_device_config_list", 0, 400, "device_config_again")]:
        p = {"func": func, "sid": sid, "lower": str(lo), "upper": str(hi)}
        if func == "qbox_query_notify":
            p["user"] = user
        code, text = call(c, QSYNC, p, tag=f"p0d_{tag}")
        dump(os.path.join(outdir, f"{tag}.txt"), text)
        print(f"  → {func}[{tag}] code={code} len={len(text)}")

    print("""
  ---- 判读要点 ----
  · sync_log 里每条的 device_uid 是不是 01234567…567（= NAS 上登记的 win-pc）？
  · 有没有任何一条是「我们客户端刚刚产生的」？
  · status:-17 表示该区间内没有事件（不是错误，见 README 踩坑 #18）
""")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", required=True)
    ap.add_argument("--port", type=int, default=9834)
    ap.add_argument("--https", action="store_true")
    ap.add_argument("--insecure", action="store_true")
    ap.add_argument("--prefix", default="")
    ap.add_argument("--user", default="test1")
    ap.add_argument("--password", default="")
    ap.add_argument("--phase", default="a", choices=["a", "b", "c", "d"])
    ap.add_argument("--device-uid", default=None, help="phase b：写入用的 device_uid")
    ap.add_argument("--config-number", type=int, default=None, help="phase b：可选")
    ap.add_argument("--marker", default="/home/qxync-test", help="phase c：本地写操作的目标远端目录")
    ap.add_argument("--max-log", type=int, default=400, help="phase d：当前 max_log 水位")
    ap.add_argument("--outdir", default=None)
    a = ap.parse_args()

    outdir = a.outdir or os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "probe-out",
        "p0-" + time.strftime("%Y%m%d-%H%M%S"))
    os.makedirs(outdir, exist_ok=True)
    print(f"输出目录: {outdir}")

    c = Client(a.host, a.port, a.https, a.insecure, a.prefix)
    c.outdir = outdir

    sid, qtoken = do_login(c, a.user, a.password, {})
    if not sid:
        print("\n❌ 登录失败，终止。")
        return 2
    print(f"\n✅ 登录成功 sid={sid}")

    # 把登录响应里的候选 uid 也记下来
    dump(os.path.join(outdir, "sid_note.json"),
         json.dumps({"sid": sid, "qtoken": qtoken, "user": a.user}, ensure_ascii=False, indent=1))

    if a.phase == "a":
        phase_a(c, sid, a.user, outdir)
    elif a.phase == "b":
        if not a.device_uid:
            print("❌ phase b 需要 --device-uid")
            return 1
        phase_b(c, sid, a.user, outdir, a.device_uid, a.config_number)
    elif a.phase == "d":
        phase_d(c, sid, a.user, outdir, a.max_log)
    else:
        phase_c(c, sid, a.user, outdir, a.marker)

    print(f"\n完成。原始响应在 {outdir}/")
    return 0


if __name__ == "__main__":
    sys.exit(main())
