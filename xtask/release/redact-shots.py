#!/usr/bin/env python3
"""发布前截图脱敏：OCR 定位敏感串并涂掉（Release 发布说明/README 嵌图前跑）。

用法：
    python3 xtask/release/redact-shots.py [--dry-run] \
        --pattern 'my-nas.example.com' --pattern 'myuser' docs/images/*.png

设计：**敏感串走命令行参数**，不写进仓库（写进仓库就等于又泄露一次）。
命中判定是大小写不敏感的子串，OCR 用 tesseract(eng) 的 TSV，psm 3 + 6 两种版面都试，
同一行的命中词合并成段、左右各扩 28px，把 `user@` / `:9834` 这类相邻碎片一起盖住；
填充色取框上方的背景色（斑马纹表格/黄条都能融进去），取不到用白。

退出码：--dry-run 下发现命中 → 1（适合放进发版清单当闸门）；否则 0。
依赖：tesseract（eng 语言包）、python3-Pillow。
"""
from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

try:
    from PIL import Image, ImageDraw, ImageFilter
except ImportError:  # pragma: no cover
    sys.exit("缺少 Pillow：sudo pacman -S python-pillow / apt install python3-pil")


def ocr_hits(path: Path, pattern: re.Pattern[str]) -> list[tuple[int, int, int, int, str]]:
    hits = []
    for psm in (3, 6):
        try:
            out = subprocess.run(
                ["tesseract", str(path), "stdout", "--psm", str(psm), "tsv"],
                capture_output=True, text=True, timeout=120,
            ).stdout
        except (OSError, subprocess.TimeoutExpired):
            continue
        for line in out.splitlines()[1:]:
            f = line.split("\t")
            if len(f) < 12 or not f[11].strip() or not pattern.search(f[11]):
                continue
            try:
                x, y, w, h = int(f[6]), int(f[7]), int(f[8]), int(f[9])
            except ValueError:
                continue
            hits.append((x, y, w, h, f[11]))
    return hits


def mask_lines(img: Image.Image, hits: list[tuple[int, int, int, int, str]],
               blur: bool = False, pad: int = 28) -> int:
    """按 y 中线聚行，整行段涂掉；返回涂改的行数。"""
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
        x1, x2 = max(0, min(xs) - pad), min(img.width, max(xe) + pad)
        y1, y2 = max(0, min(ys) - 4), min(img.height, max(ye) + 4)
        if blur:
            box = (x1, y1, max(x1 + 1, x2), max(y1 + 1, y2))
            img.paste(img.crop(box).filter(ImageFilter.GaussianBlur(6)), box)
            continue
        fill = (255, 255, 255)
        for sx in (x1 - 10, x2 + 10, (x1 + x2) // 2):
            if 0 <= sx < img.width:
                c = img.getpixel((sx, max(0, y1 - 8)))
                if sum(c) > 400:
                    fill = c
                    break
        draw.rectangle([x1, y1, x2, y2], fill=fill)
    return len(lines)


def main() -> int:
    ap = argparse.ArgumentParser(description="OCR 截图脱敏（发布前闸门）")
    ap.add_argument("images", nargs="+", type=Path)
    ap.add_argument("--pattern", action="append", required=True,
                    help="敏感子串（可多次）；请勿把真实值提交进仓库")
    ap.add_argument("--dry-run", action="store_true", help="只报告命中，不改图")
    ap.add_argument("--blur", action="store_true",
                    help="命中区域用高斯模糊代替纯色块（截图里更自然，不会留深色印子）")
    ap.add_argument("--pad", type=int, default=28, help="命中框左右外扩像素（默认 28）")
    args = ap.parse_args()
    pat = re.compile("|".join(re.escape(p) for p in args.pattern if p), re.IGNORECASE)
    dirty = 0
    for path in args.images:
        if not path.is_file():
            print(f"⚠️  跳过（不存在）：{path}", file=sys.stderr)
            continue
        hits = ocr_hits(path, pat)
        if not hits:
            continue
        dirty += 1
        if args.dry_run:
            print(f"❌ {path}: {len(hits)} 处命中 —— {', '.join(h[4] for h in hits[:6])}")
            continue
        img = Image.open(path).convert("RGB")
        n = mask_lines(img, hits, blur=args.blur, pad=args.pad)
        img.save(path)
        print(f"✅ 已涂改 {path}（{n} 行）")
    if args.dry_run and dirty:
        print(f"\n❌ {dirty} 张图仍含敏感串，先脱敏再发版", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
