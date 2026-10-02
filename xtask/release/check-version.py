#!/usr/bin/env python3
"""发版版本一致性守卫：tag / Cargo.toml(workspace) / tauri.conf.json / AUR PKGBUILD。

用法：
    python3 xtask/release/check-version.py v0.1.0

退出码：0 = 一致；1 = 不一致或取值失败（并把各处实际值打印出来）。

为什么要有这条守卫：`Cargo.toml` 的 workspace 版本是 **crate 与二进制自报的版本**
（`qxync --version` 之类），`tauri.conf.json` 的 `version` 会进 `.deb` / 应用元数据，
`packaging/arch/PKGBUILD` 的 `pkgver` 决定 AUR 包去下哪个 Release 产物，tag 是第四处。
任何一处漂移，用户拿到的东西就会与 tag 对不上。发版流水线里把 tag 当**唯一真值**。

另外会**提醒**（不拦）`packaging/arch/PKGBUILD` 的 `sha256sums` 是否还是占位值 ——
Release 产物由 CI 构建，只有发完之后才能 `updpkgsums` 填真值。
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CARGO_TOML = ROOT / "Cargo.toml"
TAURI_CONF = ROOT / "crates" / "qxync-gui" / "tauri.conf.json"
PKGBUILD = ROOT / "packaging" / "arch" / "PKGBUILD"


def fail(msg: str) -> None:
    print(f"❌ {msg}", file=sys.stderr)
    sys.exit(1)


def warn(msg: str) -> None:
    print(f"⚠️  {msg}", file=sys.stderr)


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


def pkgbuild_text() -> str | None:
    """AUR PKGBUILD 的正文；文件不存在返回 None（只警告，不拦发版）。"""
    if not PKGBUILD.is_file():
        return None
    return PKGBUILD.read_text(encoding="utf-8")


def pkgbuild_version(text: str) -> str | None:
    m = re.search(r"(?m)^pkgver=(\S+)\s*$", text)
    return m.group(1).strip("'\"") if m else None


def pkgbuild_sums_are_placeholder(text: str) -> bool:
    """sha256sums 是否还是「一串 0」的占位值。"""
    m = re.search(r"(?ms)^sha256sums=\((.*?)\)", text)
    if not m:
        return False
    body = m.group(1)
    return bool(re.fullmatch(r"[\s'\"0]*", body)) and "0" in body


def main() -> int:
    if len(sys.argv) != 2:
        fail("用法：check-version.py <tag>（例如 v0.1.0）")

    tag = sys.argv[1].strip()
    if not re.fullmatch(r"v\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.\-]+)?", tag):
        fail(f"tag 不是 `v<语义化版本>` 形态：{tag!r}（期望例如 v0.1.0、v0.2.0-rc.1）")
    tag_version = tag[1:]

    cargo_v = workspace_version()
    tauri_v = tauri_version()
    pkg_text = pkgbuild_text()
    pkg_v = pkgbuild_version(pkg_text) if pkg_text else None

    print(f"tag                     = {tag}  →  {tag_version}")
    print(f"Cargo.toml (workspace)  = {cargo_v}")
    print(f"tauri.conf.json         = {tauri_v}")
    print(f"arch/PKGBUILD pkgver    = {pkg_v if pkg_v else '（没有这个文件 / 没有 pkgver）'}")

    bad = []
    if cargo_v != tag_version:
        bad.append(f"Cargo.toml 的 workspace version 是 {cargo_v}，与 tag {tag} 不符")
    if tauri_v != tag_version:
        bad.append(f"tauri.conf.json 的 version 是 {tauri_v}，与 tag {tag} 不符")
    if cargo_v != tauri_v:
        bad.append(f"Cargo.toml（{cargo_v}）与 tauri.conf.json（{tauri_v}）两处本身就不一致")
    if pkg_text is None:
        warn(f"{PKGBUILD.relative_to(ROOT)} 不存在，跳过 AUR 包版本检查")
    elif pkg_v is None:
        warn(f"{PKGBUILD.relative_to(ROOT)} 里找不到 pkgver，跳过 AUR 包版本检查")
    elif pkg_v != tag_version:
        bad.append(f"packaging/arch/PKGBUILD 的 pkgver 是 {pkg_v}，与 tag {tag} 不符")

    if bad:
        for b in bad:
            print(f"❌ {b}", file=sys.stderr)
        print(
            "   发版前先把各处对齐（tag 是唯一真值）：Cargo.toml 的 "
            "[workspace.package].version、crates/qxync-gui/tauri.conf.json 的 version、"
            "packaging/arch/PKGBUILD 的 pkgver。",
            file=sys.stderr,
        )
        return 1

    if pkg_text is not None and pkgbuild_sums_are_placeholder(pkg_text):
        warn(
            "packaging/arch/PKGBUILD 的 sha256sums 还是占位值 —— Release 发出来之后在 "
            "packaging/arch/ 下跑 `updpkgsums`（再 `makepkg --printsrcinfo > .SRCINFO`）"
            "才能推 AUR。"
        )

    print(f"✅ 各处版本一致：{tag_version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
