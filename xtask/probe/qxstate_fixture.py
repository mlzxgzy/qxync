#!/usr/bin/env python3
"""造一份 qxync v2 缓存（.qxstate + .qxsum），用于端到端验证 `qxync verify`。

格式（与 crates/qxync-fuse/src/lib.rs 的 v2 定义严格一致）：
  .qxstate:
    magic "QXSTATE2" (8) | version=2 (4) | chunk_size (8) | size (8)
    | mtime (8) | nchunks (8) | file_id (16) | whole_xxhash (8)
    | bitmap ceil(nchunks/8)
  .qxsum:
    per-chunk xxhash64 LE，定长 8*nchunks

xxhash64 这里用一个**独立的 Python 实现**（seed 0），与 Rust 侧一致 ——
如果两边算法实现一致，就能证明「格式对得上」，而不是「用同一份代码自证」。
"""
import os
import struct
import sys

P1 = 0x9E3779B185EBCA87
P2 = 0xC2B2AE3D27D4EB4F
P3 = 0x165667B19E3779F9
P4 = 0x85EBCA77C2B2AE63
P5 = 0x27D4EB2F165667C5
M = (1 << 64) - 1


def rotl(x, r):
    return ((x << r) | (x >> (64 - r))) & M


def round64(acc, val):
    acc = (acc + (val * P2)) & M
    acc = rotl(acc, 31)
    return (acc * P1) & M


def merge_round(acc, val):
    # xxHash 规范：h ^= round(0, val); h = h * PRIME64_1 + PRIME64_4
    # ⚠️ 这里**没有** avalanche 步骤 —— 我第一版误加了 `h ^= h>>33` 之类，
    # 于是短输入（<32 字节，不走这条路径）的官方测试向量能过，长输入全错。
    val = round64(0, val)
    acc ^= val
    return ((acc * P1) + P4) & M


def xxh64(data, seed=0):
    n = len(data)
    if n >= 32:
        v1 = (seed + P1 + P2) & M
        v2 = (seed + P2) & M
        v3 = seed & M
        v4 = (seed - P1) & M
        i = 0
        while i + 32 <= n:
            v1 = round64(v1, struct.unpack_from("<Q", data, i)[0])
            v2 = round64(v2, struct.unpack_from("<Q", data, i + 8)[0])
            v3 = round64(v3, struct.unpack_from("<Q", data, i + 16)[0])
            v4 = round64(v4, struct.unpack_from("<Q", data, i + 24)[0])
            i += 32
        h = (rotl(v1, 1) + rotl(v2, 7) + rotl(v3, 12) + rotl(v4, 18)) & M
        for v in (v1, v2, v3, v4):
            h = merge_round(h, v)
    else:
        h = (seed + P5) & M
        i = 0
    h = (h + n) & M
    while i + 8 <= n:
        k1 = round64(0, struct.unpack_from("<Q", data, i)[0])
        h ^= k1
        h = (rotl(h, 27) * P1 + P4) & M
        i += 8
    if i + 4 <= n:
        h ^= (struct.unpack_from("<I", data, i)[0] * P1) & M
        h = (rotl(h, 23) * P2 + P3) & M
        i += 4
    while i < n:
        h ^= (data[i] * P5) & M
        h = (rotl(h, 11) * P1) & M
        i += 1
    h ^= h >> 33
    h = (h * P2) & M
    h ^= h >> 29
    h = (h * P3) & M
    h ^= h >> 32
    return h


CHUNK = 128 * 1024


def build(cache_dir, name, nchunks, break_chunks=(), whole_ok=True):
    """造一个 nchunks 区间、每区间 CHUNK 字节的缓存文件。"""
    size = nchunks * CHUNK
    data = bytearray()
    for i in range(nchunks):
        data += bytes(((i * 7 + j) % 251) for j in range(16)) * (CHUNK // 16)
    os.makedirs(cache_dir, exist_ok=True)
    cache = os.path.join(cache_dir, name)
    with open(cache, "wb") as f:
        f.write(data)
    # per-chunk 校验和
    sums = []
    for i in range(nchunks):
        sums.append(xxh64(bytes(data[i * CHUNK:(i + 1) * CHUNK])))
    with open(cache + ".qxsum", "wb") as f:
        for s in sums:
            f.write(struct.pack("<Q", s))
    # 位图（全就绪）
    bitmap = bytearray((nchunks + 7) // 8)
    for i in range(nchunks):
        bitmap[i // 8] |= 1 << (i % 8)
    whole = xxh64(bytes(data)) if whole_ok else (1 << 64) - 1
    head = b"QXSTATE2" + struct.pack("<I", 2) + struct.pack("<Q", CHUNK) \
        + struct.pack("<Q", size) + struct.pack("<q", 1700000000) \
        + struct.pack("<Q", nchunks) + b"\x00" * 16 \
        + struct.pack("<Q", whole)
    with open(cache + ".qxstate", "wb") as f:
        f.write(head + bytes(bitmap))
    # 按需破坏指定区间（长度不变 —— 只查长度查不出来的场景）
    for i in break_chunks:
        with open(cache, "r+b") as f:
            f.seek(i * CHUNK + 4096)
            f.write(b"\xDE\xAD\xBE\xEF" * 32)
    return cache, size


if __name__ == "__main__":
    d = sys.argv[1]
    os.makedirs(d, exist_ok=True)
    # 完好文件
    build(d, "aaaaaaaaaa_good.bin", 3)
    # 第 1 区间损坏
    build(d, "bbbbbbbbbb_bad1.bin", 3, break_chunks=[1])
    # 第 0 和第 2 区间都损坏
    build(d, "cccccccc_bad02.bin", 3, break_chunks=[0, 2])
    # 嵌套一层（模拟 daemon 的 <cache>/<nas host>/）
    build(os.path.join(d, "nas.example.com"), "dddddddddd_nested.bin", 2, break_chunks=[1])
    print("已在", d, "造好 4 个缓存文件（3 个有损坏）")
