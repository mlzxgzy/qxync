#!/usr/bin/env python3
"""`scrub-history.sh --images` 的图片清洗过滤器：把历史里的截图也脱敏。

为什么需要它：文本替换改不了 PNG 像素。截图里的顶栏连接行/家目录一旦入库，
`git log` 翻到那次提交就能看见 —— 只清文本等于只挡一半。

做法：tree-filter 里对 `docs/` 下的 PNG 逐个算内容哈希，命中缓存就直接写回
脱敏版，未命中才 OCR 一次再入缓存。**缓存是关键**：191 个提交里有 40 张图，
不做缓存就是 40×191 次 OCR。

环境变量：
    SCRUB_PATTERNS_FILE  每行 `old=>new`（只用 old 做 OCR 命中判定）
    SCRUB_IMG_CACHE      缓存目录（默认 <repo>/.local-run/img-cache）
"""
from __future__ import annotations

import hashlib
import os
import pathlib
import re
import shutil
import subprocess
import sys

MAX_BYTES = 8 * 1024 * 1024


def load_patterns() -> re.Pattern[str]:
    path = os.environ.get("SCRUB_PATTERNS_FILE", "")
    if not path:
        sys.exit("SCRUB_PATTERNS_FILE 未设置")
    keys = []
    for line in pathlib.Path(path).read_text(encoding="utf-8").splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        old = line.split("=>", 1)[0]
        if old:
            keys.append(re.escape(old))
    return re.compile("|".join(keys), re.IGNORECASE) if keys else re.compile(r"(?!x)x")


def ocr_hits(p: pathlib.Path, pat: re.Pattern[str]):
    hits = []
    for psm in (3, 6):
        try:
            out = subprocess.run(
                ["tesseract", str(p), "stdout", "--psm", str(psm), "tsv"],
                capture_output=True, text=True, timeout=180,
            ).stdout
        except (OSError, subprocess.TimeoutExpired):
            continue
        for line in out.splitlines()[1:]:
            f = line.split("\t")
            if len(f) < 12 or not f[11].strip() or not pat.search(f[11]):
                continue
            try:
                hits.append((int(f[6]), int(f[7]), int(f[8]), int(f[9])))
            except ValueError:
                continue
    return hits


def mask(path: pathlib.Path, hits) -> None:
    from PIL import Image, ImageDraw

    img = Image.open(path).convert("RGB")
    draw = ImageDraw.Draw(img)
    hits = sorted(hits, key=lambda w: (w[1] + w[3] / 2, w[0]))
    lines: list[dict] = []
    for w in hits:
        cy = w[1] + w[3] / 2
        for ln in lines:
            if abs(ln["cy"] - cy) <= 8:
                ln["items"].append(w)
                ln["cy"] = (ln["cy"] + cy) / 2
                break
        else:
            lines.append({"cy": cy, "items": [w]})
    for ln in lines:
        xs = [w[0] for w in ln["items"]]
        xe = [w[0] + w[2] for w in ln["items"]]
        ys = [w[1] for w in ln["items"]]
        ye = [w[1] + w[3] for w in ln["items"]]
        x1, x2 = max(0, min(xs) - 28), min(img.width, max(xe) + 28)
        y1, y2 = max(0, min(ys) - 4), min(img.height, max(ye) + 4)
        # 背景色：在框四周多点采样取最亮的那个（避免采到图标/文字本身）
        cands = []
        for sy in (max(0, y1 - 8), min(img.height - 1, y2 + 8)):
            for sx in (max(0, x1 - 12), (x1 + x2) // 2, min(img.width - 1, x2 + 12)):
                cands.append(img.getpixel((sx, sy)))
        fill = max(cands, key=sum) if cands else (255, 255, 255)
        draw.rectangle([x1, y1, x2, y2], fill=fill)
    img.save(path)


def main() -> int:
    pat = load_patterns()
    cache = pathlib.Path(os.environ.get("SCRUB_IMG_CACHE") or ".local-run/img-cache")
    cache.mkdir(parents=True, exist_ok=True)
    touched = 0
    for p in pathlib.Path("docs").rglob("*.png") if pathlib.Path("docs").is_dir() else []:
        try:
            if p.is_symlink() or p.stat().st_size > MAX_BYTES:
                continue
            raw = p.read_bytes()
        except OSError:
            continue
        key = hashlib.sha1(raw).hexdigest()
        cached = cache / f"{key}.png"
        if cached.is_file():
            if cached.read_bytes() != raw:
                p.write_bytes(cached.read_bytes())
                touched += 1
            continue
        tmp = cache / f".{key}.tmp.png"
        tmp.write_bytes(raw)
        hits = ocr_hits(tmp, pat)
        if hits:
            try:
                mask(tmp, hits)
            except ImportError:
                tmp.unlink(missing_ok=True)
                print("缺少 Pillow，图片清洗跳过（文本仍会替换）", file=sys.stderr)
                continue
            p.write_bytes(tmp.read_bytes())
            touched += 1
        tmp.replace(cached)
    if touched:
        print(f"  [img] {touched} 张截图已脱敏", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
