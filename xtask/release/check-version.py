#!/usr/bin/env python3
"""发版版本一致性守卫：tag / Cargo.toml(workspace) / tauri.conf.json 三处必须一致。

用法：
    python3 xtask/release/check-version.py v0.1.0

退出码：0 = 一致；1 = 不一致或取值失败（并把三处实际值打印出来）。

为什么要有这条守卫：`Cargo.toml` 的 workspace 版本是 **crate 与二进制自报的版本**
（`qsync --version` 之类），`tauri.conf.json` 的 `version` 又会进 `.deb` / 应用元数据，
Release 的 tag 是第三处。三者漂移过一次，用户拿到的二进制就会报出与 tag 不符的版本。
发版流水线里把 tag 当**唯一真值**，另两处必须跟着它走。
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CARGO_TOML = ROOT / "Cargo.toml"
TAURI_CONF = ROOT / "crates" / "qxync-gui" / "tauri.conf.json"


def fail(msg: str) -> None:
    print(f"❌ {msg}", file=sys.stderr)
    sys.exit(1)


def workspace_version() -> str:
    text = CARGO_TOML.read_text(encoding="utf-8")
    lines = text.splitlines()
    try:
        start = next(i for i, ln in enumerate(lines) if ln.strip() == "[workspace.package]")
    except StopIteration:
        fail(f"{CARGO_TOML.relative_to(ROOT)} 里找不到 [workspace.package]")
    for ln in lines[start + 1:]:
        if ln.startswith("["):  # 下一个 section，说明 [workspace.package] 里没有 version
            break
        m = re.match(r'\s*version\s*=\s*"([^"]+)"', ln)
        if m:
            return m.group(1)
    fail(f"{CARGO_TOML.relative_to(ROOT)} 的 [workspace.package] 里找不到 version")


def tauri_version() -> str:
    try:
        data = json.loads(TAURI_CONF.read_text(encoding="utf-8"))
    except json.JSONDecodeError as e:
        fail(f"{TAURI_CONF.relative_to(ROOT)} 不是合法 JSON：{e}")
    v = data.get("version")
    if not isinstance(v, str) or not v:
        fail(f"{TAURI_CONF.relative_to(ROOT)} 里没有 version")
    return v


def main() -> int:
    if len(sys.argv) != 2:
        fail("用法：check-version.py <tag>（例如 v0.1.0）")

    tag = sys.argv[1].strip()
    if not re.fullmatch(r"v\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.\-]+)?", tag):
        fail(f"tag 不是 `v<语义化版本>` 形态：{tag!r}（期望例如 v0.1.0、v0.2.0-rc.1）")
    tag_version = tag[1:]

    cargo_v = workspace_version()
    tauri_v = tauri_version()

    print(f"tag                = {tag}  →  {tag_version}")
    print(f"Cargo.toml (ws)    = {cargo_v}")
    print(f"tauri.conf.json    = {tauri_v}")

    bad = []
    if cargo_v != tag_version:
        bad.append(f"Cargo.toml 的 workspace version 是 {cargo_v}，与 tag {tag} 不符")
    if tauri_v != tag_version:
        bad.append(f"tauri.conf.json 的 version 是 {tauri_v}，与 tag {tag} 不符")
    if cargo_v != tauri_v:
        bad.append(
            f"Cargo.toml（{cargo_v}）与 tauri.conf.json（{tauri_v}）两处本身就不一致"
        )

    if bad:
        for b in bad:
            print(f"❌ {b}", file=sys.stderr)
        print(
            "   发版前先把三处对齐（tag 是唯一真值）：改 Cargo.toml 的 "
            "[workspace.package].version 与 crates/qxync-gui/tauri.conf.json 的 version。",
            file=sys.stderr,
        )
        return 1

    print(f"✅ 三处版本一致：{tag_version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
