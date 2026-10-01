//! librsync 兼容的签名 / delta / patch（M5）。
//!
//! 格式与参数来自逆向报告 [`report/06-增量传输与版本化.md`](../../../report/06-增量传输与版本化.md)
//! §6.1–6.3（逐条有反汇编证据），要点：
//!
//! * 全部多字节整数 **大端**；signature magic `0x72730136`，delta magic `0x72730236`；
//! * **block_len 必须是 1 MiB**（librsync 默认 2048，Qsync 显式覆盖）；
//! * **strong_sum_len 必须是 16**（librsync 默认 8），强校验 = **MD4** 前 16 字节；
//! * 弱校验：`CHAR_OFFSET = 31`，`A = Σ(b[i]+31)`、`B = Σ(n-i)*(b[i]+31)`、
//!   `weak = (B << 16) | (A & 0xffff)`；
//! * 命令字节 `0x41 + 4*where_code + len_code`（`1→0, 2→1, 4→2, 8→3`），
//!   `0x41..0x44` 是 LITERAL，`0x45..0x54` 是 COPY，`0x00` 是 END。
//!
//! ## 为什么本地也要有这一层（实测结论）
//!
//! M5 真机探测（见 `docs/M5-SQLite与delta.md` §3）：这台 NAS `versioning_lock` 能用，
//! 但 `versioning_support` 全为 0、`versioning_stat_delta` 恒 `exist:0`、
//! `versioning_gen_sig` 恒 `status:33` —— **没有历史版本，服务端算不了 delta**。
//! 所以本模块负责「可验证的那一半」：格式正确性、round-trip、体积收益，
//! 并让上层在服务端能力可用时能直接接上（能力探测在 `qxync-client`）。

use crate::error::{Error, Result};

/// signature 魔数（大端写出来是 `72 73 01 36`）。
pub const SIG_MAGIC: u32 = 0x7273_0136;
/// delta 魔数（大端 `72 73 02 36`）。
pub const DELTA_MAGIC: u32 = 0x7273_0236;
/// Qsync 实测使用的块大小（**必须显式覆盖 librsync 的 2048 默认值**）。
pub const DEFAULT_BLOCK_LEN: u32 = 1024 * 1024;
/// Qsync 实测使用的强校验长度（librsync 默认 8）。
pub const DEFAULT_STRONG_LEN: u8 = 16;
/// 弱校验里的字符偏移（rsync/librsync 的 CHAR_OFFSET）。
pub const CHAR_OFFSET: u32 = 31;

/// 一个块的弱校验 + 强校验。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigBlock {
    pub weak: u32,
    pub strong: Vec<u8>,
}

impl SigBlock {
    pub fn strong_hex(&self) -> String {
        self.strong.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// 整个 basis 文件的签名（signature 文件的内存表示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub block_len: u32,
    pub strong_len: u8,
    pub blocks: Vec<SigBlock>,
}

impl Signature {
    /// 按 Qsync 的实测参数（1 MiB / 16 字节）造签名。
    pub fn compute_default(data: &[u8]) -> Self {
        Self::compute(data, DEFAULT_BLOCK_LEN, DEFAULT_STRONG_LEN)
    }

    /// 造签名。`block_len` 为 0 表示「slack 模式」：不切块，退化成空签名
    /// （对应 librsync 里 `block_len == 0` → delta 全是 literal）。
    pub fn compute(data: &[u8], block_len: u32, strong_len: u8) -> Self {
        if block_len == 0 {
            return Self {
                block_len: 0,
                strong_len,
                blocks: Vec::new(),
            };
        }
        let bs = block_len as usize;
        let mut blocks = Vec::with_capacity(data.len() / bs + 1);
        let mut off = 0usize;
        while off < data.len() {
            let end = (off + bs).min(data.len());
            let chunk = &data[off..end];
            blocks.push(SigBlock {
                weak: weak_sum(chunk),
                strong: md4(chunk)[..(strong_len as usize).min(16)].to_vec(),
            });
            off = end;
        }
        Self {
            block_len,
            strong_len,
            blocks,
        }
    }

    /// 序列化成 signature 文件（librsync 原生格式）。
    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + self.blocks.len() * (4 + self.strong_len as usize));
        out.extend_from_slice(&SIG_MAGIC.to_be_bytes());
        out.extend_from_slice(&self.block_len.to_be_bytes());
        out.extend_from_slice(&(self.strong_len as u32).to_be_bytes());
        for b in &self.blocks {
            out.extend_from_slice(&b.weak.to_be_bytes());
            out.extend_from_slice(&b.strong);
        }
        out
    }

    /// 解析 signature 文件。
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 12 {
            return Err(Error::Parse("signature 太短（<12 字节）".into()));
        }
        let magic = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if magic != SIG_MAGIC {
            return Err(Error::Parse(format!(
                "wrong magic number {magic:#010x} for signature（应为 {SIG_MAGIC:#010x}）"
            )));
        }
        let block_len = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let strong_len = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        if !(1..=16).contains(&strong_len) {
            return Err(Error::Parse(format!(
                "strong sum length {strong_len} is implausible（必须在 1..=16）"
            )));
        }
        let rec = 4 + strong_len as usize;
        let body = &bytes[12..];
        if body.len() % rec != 0 {
            return Err(Error::Parse(format!(
                "signature 正文长度 {} 不是 {} 的整数倍",
                body.len(),
                rec
            )));
        }
        let mut blocks = Vec::with_capacity(body.len() / rec);
        for chunk in body.chunks(rec) {
            blocks.push(SigBlock {
                weak: u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
                strong: chunk[4..].to_vec(),
            });
        }
        Ok(Self {
            block_len,
            strong_len: strong_len as u8,
            blocks,
        })
    }
}

/// 弱校验（rsync 滚动校验和，CHAR_OFFSET = 31）。
pub fn weak_sum(block: &[u8]) -> u32 {
    let mut a: u32 = 0;
    let mut b: u32 = 0;
    let n = block.len() as u32;
    for (i, &byte) in block.iter().enumerate() {
        let v = byte as u32 + CHAR_OFFSET;
        a = a.wrapping_add(v);
        b = b.wrapping_add((n - i as u32).wrapping_mul(v));
    }
    ((b & 0xffff) << 16) | (a & 0xffff)
}

/// 滚动弱校验：去掉窗口最老的字节、加入最新字节（O(1)）。
#[derive(Debug, Clone)]
pub struct RollingWeak {
    a: u32,
    b: u32,
    len: u32,
}

impl RollingWeak {
    pub fn new(window: &[u8]) -> Self {
        let mut a: u32 = 0;
        let mut b: u32 = 0;
        let n = window.len() as u32;
        for (i, &byte) in window.iter().enumerate() {
            let v = byte as u32 + CHAR_OFFSET;
            a = a.wrapping_add(v);
            b = b.wrapping_add((n - i as u32).wrapping_mul(v));
        }
        Self { a, b, len: n }
    }

    pub fn sum(&self) -> u32 {
        ((self.b & 0xffff) << 16) | (self.a & 0xffff)
    }

    /// 滚动一格：`old` 是离开窗口的字节，`new` 是进入窗口的字节。
    pub fn roll(&mut self, old: u8, new: u8) {
        let n = self.len;
        let ov = old as u32 + CHAR_OFFSET;
        let nv = new as u32 + CHAR_OFFSET;
        self.a = self.a.wrapping_sub(ov).wrapping_add(nv);
        self.b = self
            .b
            .wrapping_sub(n.wrapping_mul(ov))
            .wrapping_add(self.a);
    }
}

// ---------------------------------------------------------------- delta

fn len_code(n: u64) -> (u8, usize) {
    if n <= u8::MAX as u64 {
        (0, 1)
    } else if n <= u16::MAX as u64 {
        (1, 2)
    } else if n <= u32::MAX as u64 {
        (2, 4)
    } else {
        (3, 8)
    }
}

fn write_len(out: &mut Vec<u8>, n: u64, bytes: usize) {
    out.extend_from_slice(&n.to_be_bytes()[8 - bytes..]);
}

fn push_literal(out: &mut Vec<u8>, lit: &[u8]) {
    if lit.is_empty() {
        return;
    }
    let (code, bytes) = len_code(lit.len() as u64);
    out.push(0x41 + code);
    write_len(out, lit.len() as u64, bytes);
    out.extend_from_slice(lit);
}

fn push_copy(out: &mut Vec<u8>, where_: u64, len: u64) {
    let (wcode, wbytes) = len_code(where_);
    let (lcode, lbytes) = len_code(len);
    // COPY 的 where_code 不能是 0（0 是 LITERAL），所以 1 字节 where 用 code=1
    let wcode = wcode + 1;
    out.push(0x41 + 4 * wcode + lcode);
    write_len(out, where_, wbytes);
    write_len(out, len, lbytes);
}

/// 生成 delta（含 magic）。
///
/// 用「按弱校验分桶 + 滚动窗口」的经典 rsync 匹配：命中就发 COPY（块长固定），
/// 没命中就把当前字节并进 literal，窗口滚一格。**不追求最小 delta，但保证正确**。
pub fn delta(sig: &Signature, new_data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&DELTA_MAGIC.to_be_bytes());
    if sig.block_len == 0 || sig.blocks.is_empty() {
        // slack 模式：整段 literal
        push_literal(&mut out, new_data);
        out.push(0x00);
        return out;
    }
    let bs = sig.block_len as usize;
    if new_data.len() < bs {
        push_literal(&mut out, new_data);
        out.push(0x00);
        return out;
    }

    use std::collections::HashMap;
    let mut by_weak: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, b) in sig.blocks.iter().enumerate() {
        by_weak.entry(b.weak).or_default().push(i);
    }

    let mut literal: Vec<u8> = Vec::new();
    let mut pos = 0usize;
    let mut rolling = RollingWeak::new(&new_data[pos..pos + bs]);

    loop {
        let remaining = new_data.len() - pos;
        if remaining < bs {
            // 尾巴（不足一块）：全部当 literal
            literal.extend_from_slice(&new_data[pos..]);
            break;
        }
        let sum = rolling.sum();
        let mut matched: Option<(usize, Vec<u8>)> = None;
        if let Some(cands) = by_weak.get(&sum) {
            let strong_full = md4(&new_data[pos..pos + bs]);
            let strong = &strong_full[..(sig.strong_len as usize).min(16)];
            for &ci in cands {
                if sig.blocks[ci].strong.as_slice() == strong {
                    matched = Some((ci, strong.to_vec()));
                    break;
                }
            }
        }
        match matched {
            Some((ci, _)) => {
                push_literal(&mut out, &literal);
                literal.clear();
                push_copy(&mut out, (ci as u64) * (bs as u64), bs as u64);
                pos += bs;
                if pos + bs <= new_data.len() {
                    rolling = RollingWeak::new(&new_data[pos..pos + bs]);
                }
            }
            None => {
                literal.push(new_data[pos]);
                pos += 1;
                if pos + bs <= new_data.len() {
                    rolling.roll(new_data[pos - 1], new_data[pos + bs - 1]);
                }
            }
        }
    }
    push_literal(&mut out, &literal);
    out.push(0x00);
    out
}

/// 应用 delta（`basis` 是旧文件内容）。
pub fn patch(basis: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    if delta.len() < 4 {
        return Err(Error::Parse("delta 太短（<4 字节）".into()));
    }
    let magic = u32::from_be_bytes([delta[0], delta[1], delta[2], delta[3]]);
    if magic != DELTA_MAGIC {
        return Err(Error::Parse(format!(
            "wrong magic number {magic:#010x} for delta（应为 {DELTA_MAGIC:#010x}）"
        )));
    }
    let mut out = Vec::new();
    let mut i = 4usize;
    loop {
        if i >= delta.len() {
            return Err(Error::Parse("delta 在 END 之前就结束了".into()));
        }
        let cmd = delta[i];
        i += 1;
        if cmd == 0x00 {
            return Ok(out);
        }
        if !(0x41..=0x54).contains(&cmd) {
            return Err(Error::Parse(format!("bogus command {cmd:#04x}")));
        }
        let v = (cmd - 0x41) as usize;
        let where_code = v / 4;
        let len_bytes_code = v % 4;
        let len_bytes = [1usize, 2, 4, 8][len_bytes_code];
        if (cmd as usize) < 0x45 {
            // LITERAL
            let len = read_be(&delta, &mut i, len_bytes)?;
            let end = i
                .checked_add(len as usize)
                .ok_or_else(|| Error::Parse("literal 长度溢出".into()))?;
            if end > delta.len() {
                return Err(Error::Parse("literal 超出 delta 末尾".into()));
            }
            out.extend_from_slice(&delta[i..end]);
            i = end;
        } else {
            // COPY
            let where_bytes = [0usize, 1, 2, 4, 8][where_code];
            let where_ = read_be(&delta, &mut i, where_bytes)?;
            let len = read_be(&delta, &mut i, len_bytes)?;
            let start = where_ as usize;
            let end = start
                .checked_add(len as usize)
                .ok_or_else(|| Error::Parse("copy 长度溢出".into()))?;
            if end > basis.len() {
                return Err(Error::Parse(format!(
                    "copy 越界：basis 只有 {} 字节，要求 [{}..{})",
                    basis.len(),
                    start,
                    end
                )));
            }
            out.extend_from_slice(&basis[start..end]);
        }
    }
}

fn read_be(buf: &[u8], i: &mut usize, n: usize) -> Result<u64> {
    if *i + n > buf.len() {
        return Err(Error::Parse("delta 截断".into()));
    }
    let mut v: u64 = 0;
    for k in 0..n {
        v = (v << 8) | buf[*i + k] as u64;
    }
    *i += n;
    Ok(v)
}

// ---------------------------------------------------------------- MD4 (RFC 1320)

/// MD4（RFC 1320）。强校验用它；Qsync 取前 `strong_sum_len`（16）字节。
pub fn md4(data: &[u8]) -> [u8; 16] {
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    let (mut a0, mut b0, mut c0, mut d0) =
        (0x6745_2301u32, 0xefcd_ab89u32, 0x98ba_dcfeu32, 0x1032_5476u32);

    for chunk in msg.chunks(64) {
        let mut m = [0u32; 16];
        for (i, w) in m.iter_mut().enumerate() {
            *w = u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);

        // Round 1
        const S1: [u32; 4] = [3, 7, 11, 19];
        for i in 0..16 {
            let (x, y, z) = (b, c, d);
            let f = (x & y) | (!x & z);
            let t = a
                .wrapping_add(f)
                .wrapping_add(m[i])
                .rotate_left(S1[i % 4]);
            a = d;
            d = c;
            c = b;
            b = t;
        }
        // Round 2
        const S2: [u32; 4] = [3, 5, 9, 13];
        const K2: [usize; 16] = [0, 4, 8, 12, 1, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15];
        for i in 0..16 {
            let (x, y, z) = (b, c, d);
            let g = (x & y) | (x & z) | (y & z);
            let t = a
                .wrapping_add(g)
                .wrapping_add(m[K2[i]])
                .wrapping_add(0x5A82_7999)
                .rotate_left(S2[i % 4]);
            a = d;
            d = c;
            c = b;
            b = t;
        }
        // Round 3
        const S3: [u32; 4] = [3, 9, 11, 15];
        const K3: [usize; 16] = [0, 8, 4, 12, 2, 10, 6, 14, 1, 9, 5, 13, 3, 11, 7, 15];
        for i in 0..16 {
            let h = b ^ c ^ d;
            let t = a
                .wrapping_add(h)
                .wrapping_add(m[K3[i]])
                .wrapping_add(0x6ED9_EBA1)
                .rotate_left(S3[i % 4]);
            a = d;
            d = c;
            c = b;
            b = t;
        }

        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    // ---------------- MD4：RFC 1320 官方测试向量（强校验的正确性根基）
    #[test]
    fn md4_rfc1320_vectors() {
        assert_eq!(hex(&md4(b"")), "31d6cfe0d16ae931b73c59d7e0c089c0");
        assert_eq!(hex(&md4(b"a")), "bde52cb31de33e46245e05fbdbd6fb24");
        assert_eq!(hex(&md4(b"abc")), "a448017aaf21d8525fc10ae87aa6729d");
        assert_eq!(
            hex(&md4(b"message digest")),
            "d9130a8164549fe818874806e1c7014b"
        );
        assert_eq!(
            hex(&md4(b"abcdefghijklmnopqrstuvwxyz")),
            "d79e1c308aa5bbcdeea8ed63df412da9"
        );
        assert_eq!(
            hex(&md4(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789")),
            "043f8582f241db351ce627e153e7f0e4"
        );
        // 跨块（>64 字节）也要对
        let long = b"1234567890".repeat(8);
        assert_eq!(hex(&md4(&long)), "e33b4ddc9c38f2199c3e7b164fcc0536");
    }

    // ---------------- 弱校验：与报告里的公式逐项对照
    #[test]
    fn weak_sum_matches_formula() {
        let block = b"abc";
        // A = (97+31)+(98+31)+(99+31) = 387 → 0x0183
        // B = 3*128 + 2*129 + 1*130 = 384+258+130 = 772 = 0x0304
        assert_eq!(weak_sum(block), (0x0304u32 << 16) | 0x0183);
        assert_eq!(weak_sum(b""), 0);
        // 等长但内容不同 → 不同
        assert_ne!(weak_sum(b"abc"), weak_sum(b"abd"));
    }

    #[test]
    fn rolling_matches_recompute() {
        let data: Vec<u8> = (0..300u32).map(|i| (i * 7 % 251) as u8).collect();
        let bs = 64;
        let mut r = RollingWeak::new(&data[0..bs]);
        for pos in 0..(data.len() - bs - 1) {
            assert_eq!(
                r.sum(),
                weak_sum(&data[pos..pos + bs]),
                "滚动到 pos={pos} 时弱校验漂了"
            );
            r.roll(data[pos], data[pos + bs]);
        }
    }

    // ---------------- signature 文件格式：magic / 大端 / 往返
    #[test]
    fn signature_wire_format_is_librsync_native() {
        let data = b"hello world, this is a basis file for signature tests!!";
        let sig = Signature::compute(data, 16, 16);
        let bytes = sig.write();
        // magic 0x72730136 大端
        assert_eq!(&bytes[0..4], &[0x72, 0x73, 0x01, 0x36]);
        // block_len / strong_len 大端
        assert_eq!(&bytes[4..8], &16u32.to_be_bytes());
        assert_eq!(&bytes[8..12], &16u32.to_be_bytes());
        // 每块 4 + 16 字节
        assert_eq!(bytes.len(), 12 + sig.blocks.len() * 20);
        assert_eq!(sig.blocks[0].weak, weak_sum(&data[0..16]));
        assert_eq!(
            sig.blocks[0].strong.as_slice(),
            &md4(&data[0..16])[..16],
            "强校验必须是 MD4 前 16 字节"
        );
        let back = Signature::parse(&bytes).unwrap();
        assert_eq!(back, sig);
    }

    #[test]
    fn signature_parse_rejects_bad_input() {
        assert!(Signature::parse(&[]).is_err());
        assert!(Signature::parse(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
        // 正确的 magic 但 strong_len 不合法
        let mut bad = SIG_MAGIC.to_be_bytes().to_vec();
        bad.extend_from_slice(&1024u32.to_be_bytes());
        bad.extend_from_slice(&17u32.to_be_bytes());
        let err = Signature::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("implausible"), "{err}");
        // 正文长度不是 4+strong 的整数倍
        let mut trunc = SIG_MAGIC.to_be_bytes().to_vec();
        trunc.extend_from_slice(&1024u32.to_be_bytes());
        trunc.extend_from_slice(&16u32.to_be_bytes());
        trunc.extend_from_slice(&[1, 2, 3, 4, 5]);
        assert!(Signature::parse(&trunc).is_err());
    }

    #[test]
    fn digest_of_missing_data_is_zero_length_signature() {
        // 空文件：签名没有块（librsync 行为），delta 退化为全 literal
        let sig = Signature::compute_default(b"");
        assert!(sig.blocks.is_empty());
        let d = delta(&sig, b"brand new content");
        assert_eq!(patch(b"", &d).unwrap(), b"brand new content");
    }

    #[test]
    fn slack_mode_is_all_literal() {
        let sig = Signature {
            block_len: 0,
            strong_len: 16,
            blocks: vec![],
        };
        let d = delta(&sig, b"whatever");
        assert_eq!(u32::from_be_bytes([d[0], d[1], d[2], d[3]]), DELTA_MAGIC);
        assert_eq!(d[4], 0x41, "第一字节就是 LITERAL(1 字节长度)");
        assert_eq!(patch(b"", &d).unwrap(), b"whatever");
    }

    // ---------------- 核心：sign → delta → patch 往返
    fn roundtrip(basis: &[u8], new: &[u8], block: u32) {
        let sig = Signature::compute(basis, block, 16);
        let d = delta(&sig, new);
        let got = patch(basis, &d).unwrap();
        assert_eq!(got, new, "round-trip 不一致（block={block}）");
    }

    #[test]
    fn delta_patch_roundtrip_cases() {
        let basis: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        roundtrip(&basis, &basis, 512); // 完全相同
        roundtrip(&basis, b"", 512); // 变成空文件
        roundtrip(&basis, &basis[..1], 512);
        roundtrip(b"", &basis, 512); // 从空文件变出来
        roundtrip(b"short", b"another short thing", 512); // 比块还短

        let mut prepend = b"HEADER".to_vec();
        prepend.extend_from_slice(&basis);
        roundtrip(&basis, &prepend, 512);

        let mut append = basis.clone();
        append.extend_from_slice(b"TAIL");
        roundtrip(&basis, &append, 512);

        let mut middle = basis.clone();
        middle[2000] ^= 0xff;
        roundtrip(&basis, &middle, 512);

        let mut many = basis.clone();
        for i in (0..many.len()).step_by(97) {
            many[i] = many[i].wrapping_add(3);
        }
        roundtrip(&basis, &many, 512);

        // 1 MiB 默认块（Qsync 实测参数）：改几个字节
        let big: Vec<u8> = (0..(3 * 1024 * 1024 + 12345u32))
            .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
            .collect();
        let mut big2 = big.clone();
        for i in [0usize, 40, 1_500_000, big.len() - 3] {
            big2[i] ^= 0x5a;
        }
        roundtrip(&big, &big2, DEFAULT_BLOCK_LEN);
    }

    #[test]
    fn identical_file_produces_tiny_delta() {
        let data: Vec<u8> = (0..(4 * 1024 * 1024u32)).map(|i| (i % 253) as u8).collect();
        let sig = Signature::compute(&data, DEFAULT_BLOCK_LEN, 16);
        let d = delta(&sig, &data);
        // 4 MiB 全命中 → delta 应该只有「4 条 COPY + magic + END」
        assert!(
            d.len() < 128,
            "相同文件的 delta 竟然有 {} 字节（滚动匹配没生效？）",
            d.len()
        );
        assert_eq!(patch(&data, &d).unwrap(), data);
    }

    #[test]
    fn delta_end_and_commands_are_wellformed() {
        let basis = b"0123456789abcdef".repeat(64); // 1024 字节
        let mut new = basis.clone();
        new[100] = b'X';
        let sig = Signature::compute(&basis, 256, 16);
        let d = delta(&sig, &new);
        assert_eq!(*d.last().unwrap(), 0x00, "delta 必须以 END 结尾");
        // 走一遍解析，命令流必须完全合法
        assert_eq!(patch(&basis, &d).unwrap(), new);
    }

    #[test]
    fn patch_rejects_corrupt_delta() {
        let basis = b"0123456789abcdef".repeat(64);
        let sig = Signature::compute(&basis, 256, 16);
        let mut d = delta(&sig, &basis);
        assert!(patch(&basis, &[0, 0, 0, 0]).is_err(), "magic 不对要报错");
        assert!(patch(&basis, &[]).is_err(), "太短要报错");
        // 截断
        let cut = d.len() - 2;
        assert!(patch(&basis, &d[..cut]).is_err(), "截断要报错");
        // 造一个越界 COPY（where=1000 / len=250，而 basis 只有 1024 字节）
        let mut bad = DELTA_MAGIC.to_be_bytes().to_vec();
        bad.push(0x49); // COPY：where 用 2 字节、len 用 1 字节
        bad.extend_from_slice(&1000u16.to_be_bytes());
        bad.push(250);
        bad.push(0x00);
        let err = patch(&basis, &bad).unwrap_err().to_string();
        assert!(err.contains("越界"), "{err}");
        // 非法命令字节
        d.clear();
        d.extend_from_slice(&DELTA_MAGIC.to_be_bytes());
        d.push(0x7f);
        assert!(patch(&basis, &d).unwrap_err().to_string().contains("bogus"));
    }
}
