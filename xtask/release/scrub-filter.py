#!/usr/bin/env python3
"""`scrub-history.sh` 的过滤器实现（tree 与 msg 两种模式）。

为什么要单独一个文件：`git filter-branch` 用 sh 执行过滤器，把多行 heredoc 塞进
`--tree-filter '...'` 会被引号层吃掉一层，行为难预测。这里只认两个入口：

    SCRUB_PATTERNS_FILE=/path/patterns scrub-filter.py tree   # cwd = 临时检出树，原地替换
    SCRUB_PATTERNS_FILE=/path/patterns scrub-filter.py msg    # stdin = 提交信息，stdout = 替换后

patterns 文件每行 `old=>new`（不含 `=>` 的行整行当 old，new 置空）。
"""
from __future__ import annotations

import os
import pathlib
import sys

SKIP_DIRS = {".git", "target", "node_modules", "dist", ".local-run"}
MAX_BYTES = 8 * 1024 * 1024  # 超过 8 MiB 的文件不当文本处理（产物/夹具，且几乎不可能是文档）


def load_patterns() -> list[tuple[str, str]]:
    path = os.environ.get("SCRUB_PATTERNS_FILE", "")
    if not path:
        sys.exit("SCRUB_PATTERNS_FILE 未设置")
    out = []
    for line in pathlib.Path(path).read_text(encoding="utf-8").splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if "=>" in line:
            old, new = line.split("=>", 1)
        else:
            old, new = line, ""
        if old:
            out.append((old, new))
    return out


def replace_text(s: str, pats: list[tuple[str, str]]) -> tuple[str, int]:
    n = 0
    for old, new in pats:
        if old in s:
            n += s.count(old)
            s = s.replace(old, new)
    return s, n


def do_tree(pats: list[tuple[str, str]]) -> int:
    changed = 0
    for root, dirs, files in os.walk("."):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
        for name in files:
            p = pathlib.Path(root) / name
            try:
                if p.stat().st_size > MAX_BYTES or p.is_symlink():
                    continue
                raw = p.read_bytes()
            except OSError:
                continue
            if b"\0" in raw[:4096]:  # 二进制不碰
                continue
            try:
                s = raw.decode("utf-8")
            except UnicodeDecodeError:
                continue
            s2, n = replace_text(s, pats)
            if n:
                p.write_text(s2, encoding="utf-8")
                changed += n
    if changed:
        print(f"  [tree] {changed} 处替换", file=sys.stderr)
    return 0


def do_msg(pats: list[tuple[str, str]]) -> int:
    s = sys.stdin.read()
    s2, _ = replace_text(s, pats)
    sys.stdout.write(s2)
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2 or sys.argv[1] not in ("tree", "msg"):
        sys.exit("用法: scrub-filter.py tree|msg")
    pats = load_patterns()
    sys.exit(do_tree(pats) if sys.argv[1] == "tree" else do_msg(pats))
