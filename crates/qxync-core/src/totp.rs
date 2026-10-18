//! # 两步验证（2SV）的 TOTP —— RFC 6238
//!
//! QNAP 账号开启「两步验证」后，`authLogin.cgi` 会回 `need_2_step_verification=1`，
//! 必须再带一个 `security_code=<6 位验证码>` 才能拿到 `authSid`。QTS Web UI
//! （`/cgi-bin/js/qos-core-login.js`）用的就是标准 TOTP：**SHA-1、30 秒、6 位**，
//! 字段名 `security_code`。
//!
//! ## 为什么自己实现，而不拉 `totp` / `hmac` / `sha1` crate
//!
//! 这是 qxync 里唯一的密码学原语，且要跨 core/client/daemon/CLI/GUI 复用；
//! 引三个新依赖（含为过 MSRV 锁版本的负担）不划算，而且供应链面积比一段
//! 带 RFC 测试向量的 200 行代码更大。实现严格照 RFC 3174（SHA-1）/ RFC 2104
//! （HMAC）/ RFC 4648 §6（Base32）/ RFC 6238（TOTP），单测里钉死了官方向量。
//!
//! ## 对外只做两件事
//!
//! * [`normalize_otp`]：把用户输入（Base32 密钥 / `otpauth://` URI / 6 位验证码）
//!   归一化成 [`Otp`]；密钥会顺带校验 Base32 与位数。
//! * [`resolve_code`]：真正登录时把它变成要提交的 6 位 `security_code`。
//!   密钥形式**每次调用现算**，绝不缓存验证码（30 秒就过期了）。

use crate::error::{Error, Result};
use std::time::{SystemTime, UNIX_EPOCH};

/// 时间步长：RFC 6238 默认 30 秒（QNAP 也是）。
pub const STEP_SECS: u64 = 30;
/// 验证码位数：QNAP 的 `security_code_length` 实测为 6。
pub const DIGITS: u32 = 6;

/// 用户提供的两步验证凭据。
///
/// * [`Otp::Secret`] 是 Base32 密钥（已归一化成大写、去空格/连字符/补位），
///   适合**保存**下来，之后每次登录现算验证码。
/// * [`Otp::Code`] 是一次性 6 位验证码，**只用于本次登录**，不落盘。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Otp {
    Code(String),
    Secret(String),
}

impl Otp {
    /// 是否是可长期保存的密钥（口令同一级别的敏感信息）。
    pub fn is_secret(&self) -> bool {
        matches!(self, Otp::Secret(_))
    }

    /// 归一化后的值（密钥是大写 Base32；验证码是 6 位数字）。
    pub fn value(&self) -> &str {
        match self {
            Otp::Code(v) | Otp::Secret(v) => v,
        }
    }
}

/// 6 位纯数字验证码？
pub fn is_code(input: &str) -> bool {
    let s = input.trim();
    s.len() == DIGITS as usize && s.bytes().all(|b| b.is_ascii_digit())
}

/// 把用户输入归一化：
///
/// * 6 位纯数字 → [`Otp::Code`]（一次性验证码）；
/// * `otpauth://totp/...?secret=...` → 取出并校验 `secret`，返回 [`Otp::Secret`]；
/// * 其它 → 当作 Base32 密钥，去空格/连字符/补位、转大写后校验，返回 [`Otp::Secret`]。
///
/// 注意：**看起来像 6 位验证码的输入永远不会被当成密钥**。这是有意的 ——
/// 密钥短于 8 个 Base32 字符本来就不合法，而 6 位数字更像用户手输的验证码。
pub fn normalize_otp(input: &str) -> Result<Otp> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(Error::Auth("两步验证：密钥/验证码为空".into()));
    }
    if is_code(raw) {
        return Ok(Otp::Code(raw.to_string()));
    }
    let (secret, from_uri) = if raw
        .get(..8)
        .is_some_and(|p| p.eq_ignore_ascii_case("otpauth:"))
    {
        (secret_from_uri(raw)?, true)
    } else {
        (raw.to_string(), false)
    };
    let canonical = canonical_secret(&secret)?;
    // URI 里可能还带着算法/位数/周期；只认默认值，免得「密钥对了、算出来的码却不对」。
    if from_uri {
        // 上面 secret_from_uri 已校验参数；这里保留 from_uri 只为可读性。
    }
    Ok(Otp::Secret(canonical))
}

/// 把一份 2SV 输入解析成**本次登录**要提交的 6 位验证码。
///
/// 密钥形式每次调用都按当前时间现算 —— daemon 长时间挂载、定时重登时，
/// 拿到的一定是当下的有效码，而不是登录那一刻缓存的旧码。
pub fn resolve_code(input: &str) -> Result<String> {
    match normalize_otp(input)? {
        Otp::Code(code) => Ok(code),
        Otp::Secret(secret) => code_now(&secret),
    }
}

/// 当前时间对应的 6 位验证码。
pub fn code_now(secret: &str) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Error::Auth(format!("两步验证：系统时间早于 Unix epoch（{e}）")))?;
    code_at(secret, now.as_secs())
}

/// 指定 Unix 时间戳对应的 6 位验证码（单测用）。
///
/// `secret` 既接受已归一化的 Base32，也接受用户原始输入（内部会再归一化一次）。
pub fn code_at(secret: &str, unix_secs: u64) -> Result<String> {
    let canonical = match normalize_otp(secret)? {
        Otp::Secret(s) => s,
        Otp::Code(_) => {
            return Err(Error::Auth(
                "两步验证：6 位验证码不能当密钥用（这里需要 Base32 密钥）".into(),
            ))
        }
    };
    let key = base32_decode(&canonical)?;
    let counter = unix_secs / STEP_SECS;
    let mac = hmac_sha1(&key, &counter.to_be_bytes());
    // RFC 4226 §5.3 dynamic truncation
    let offset = (mac[19] & 0x0f) as usize;
    let bin = u32::from_be_bytes([
        mac[offset],
        mac[offset + 1],
        mac[offset + 2],
        mac[offset + 3],
    ]) & 0x7fff_ffff;
    let code = bin % 10u32.pow(DIGITS);
    Ok(format!("{code:0width$}", width = DIGITS as usize))
}

/// 归一化 Base32 密钥：去空白/连字符、转大写、去 `=` 补位，并做合法性校验。
fn canonical_secret(input: &str) -> Result<String> {
    let mut s = String::with_capacity(input.len());
    for c in input.chars() {
        if c.is_whitespace() || c == '-' {
            continue;
        }
        s.push(c.to_ascii_uppercase());
    }
    let trimmed = s.trim_end_matches('=').to_string();
    if trimmed.len() < 8 {
        return Err(Error::Auth(format!(
            "两步验证：TOTP 密钥太短（{} 个 Base32 字符，至少 8 个）",
            trimmed.len()
        )));
    }
    // 长度取模的合法值：0/2/4/5/7（RFC 4648 §6），其余必然是输入漏字符。
    match trimmed.len() % 8 {
        0 | 2 | 4 | 5 | 7 => {}
        _ => {
            return Err(Error::Auth(format!(
                "两步验证：TOTP 密钥长度不合法（{} 个 Base32 字符）",
                trimmed.len()
            )))
        }
    }
    // 真解码一次：既校验字符集，也校验尾部填充位。
    base32_decode(&trimmed)?;
    Ok(trimmed)
}

/// `otpauth://totp/Issuer:account?secret=...&algorithm=SHA1&digits=6&period=30`
///
/// 只支持 QNAP/标准 TOTP 默认参数；显式写了别的参数就报错 —— 与其算出一串
/// 永远对不上的码，不如明确告诉用户「这个 URI 用的不是默认参数」。
fn secret_from_uri(uri: &str) -> Result<String> {
    let (_, query) = uri
        .split_once('?')
        .ok_or_else(|| Error::Auth("两步验证：otpauth URI 里没有查询参数（缺 secret）".into()))?;
    let mut secret = None;
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let key = k.to_ascii_lowercase();
        let val = percent_decode(v);
        match key.as_str() {
            "secret" => secret = Some(val),
            "algorithm" => {
                if !val.eq_ignore_ascii_case("SHA1") {
                    return Err(Error::Auth(format!(
                        "两步验证：otpauth URI 的算法是 {val}，只支持 SHA1（QNAP 用它）"
                    )));
                }
            }
            "digits" => {
                if val != DIGITS.to_string() {
                    return Err(Error::Auth(format!(
                        "两步验证：otpauth URI 是 {val} 位验证码，只支持 6 位"
                    )));
                }
            }
            "period" => {
                if val != STEP_SECS.to_string() {
                    return Err(Error::Auth(format!(
                        "两步验证：otpauth URI 的周期是 {val} 秒，只支持 30 秒"
                    )));
                }
            }
            _ => {}
        }
    }
    secret.ok_or_else(|| Error::Auth("两步验证：otpauth URI 里没有 secret 参数".into()))
}

/// 最小 percent-decode（Base32 里不会出现 `%`，但兼容 URI 的 `secret=%41...` 写法）。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// RFC 4648 §6 Base32 解码（大小写不敏感，容忍空格/连字符/`=` 补位）。
pub fn base32_decode(input: &str) -> Result<Vec<u8>> {
    let mut acc: u32 = 0;
    let mut nbits = 0;
    let mut out = Vec::with_capacity(input.len() * 5 / 8);
    for c in input.chars() {
        if c.is_whitespace() || c == '-' || c == '=' {
            continue;
        }
        let v = match c.to_ascii_uppercase() {
            'A'..='Z' => c.to_ascii_uppercase() as u32 - 'A' as u32,
            '2'..='7' => c.to_ascii_uppercase() as u32 - '2' as u32 + 26,
            _ => {
                return Err(Error::Auth(format!(
                    "两步验证：TOTP 密钥里有非法字符 {c:?}（Base32 只允许 A-Z2-7）"
                )))
            }
        };
        acc = (acc << 5) | v;
        nbits += 5;
        if nbits >= 8 {
            nbits -= 8;
            out.push(((acc >> nbits) & 0xff) as u8);
        }
    }
    if nbits > 0 && (acc & ((1 << nbits) - 1)) != 0 {
        return Err(Error::Auth(
            "两步验证：TOTP 密钥尾部有非零填充位（可能抄漏/抄多了字符）".into(),
        ));
    }
    if out.is_empty() {
        return Err(Error::Auth("两步验证：TOTP 密钥为空".into()));
    }
    Ok(out)
}

// ---------------------------------------------------------------- SHA-1 / HMAC

/// RFC 3174 SHA-1。只给 TOTP 用，不追求通用性。
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    // padding：0x80 + 0x00... + 64 位大端 bit 长度
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(data.len() + 72);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for block in padded.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, chunk) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// RFC 2104 HMAC-SHA1。
fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; 20] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..20].copy_from_slice(&sha1(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(BLOCK + msg.len());
    inner.extend(k.iter().map(|b| b ^ 0x36));
    inner.extend_from_slice(msg);
    let inner_hash = sha1(&inner);
    let mut outer = Vec::with_capacity(BLOCK + 20);
    outer.extend(k.iter().map(|b| b ^ 0x5c));
    outer.extend_from_slice(&inner_hash);
    sha1(&outer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    // RFC 3174 §7.3 官方向量
    #[test]
    fn sha1_rfc3174_vectors() {
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        // 多块（> 64 字节）+ 长度跨块
        let million_a = vec![b'a'; 1_000_000];
        assert_eq!(
            hex(&sha1(&million_a)),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    // RFC 2202 §2 官方向量
    #[test]
    fn hmac_sha1_rfc2202_vectors() {
        assert_eq!(
            hex(&hmac_sha1(&[0x0b; 20], b"Hi There")),
            "b617318655057264e28bc0b6fb378c8ef146be00"
        );
        assert_eq!(
            hex(&hmac_sha1(b"Jefe", b"what do ya want for nothing?")),
            "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"
        );
        assert_eq!(
            hex(&hmac_sha1(&[0xaa; 20], &[0xdd; 50])),
            "125d7342b9ac11cd91a39af48aa17b4f63f175d3"
        );
    }

    // RFC 4648 §10 Base32 向量
    #[test]
    fn base32_rfc4648_vectors() {
        assert_eq!(base32_decode("MZXW6===").unwrap(), b"foo");
        assert_eq!(base32_decode("MZXW6YTBOI======").unwrap(), b"foobar");
        assert_eq!(base32_decode("mzxw6ytboi").unwrap(), b"foobar");
        assert!(base32_decode("MZXW6!").is_err());
    }

    // RFC 6238 Appendix B（SHA-1 seed），8 位向量取后 6 位
    #[test]
    fn totp_rfc6238_sha1_vectors() {
        const SEED: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"; // "12345678901234567890"
        let cases = [
            (59u64, "287082"),
            (1111111109, "081804"),
            (1111111111, "050471"),
            (1234567890, "005924"),
            (2000000000, "279037"),
            (20000000000, "353130"),
        ];
        for (t, want) in cases {
            assert_eq!(code_at(SEED, t).unwrap(), want, "t={t}");
        }
    }

    #[test]
    fn normalize_accepts_user_forms() {
        // 小写 + 空格 + 连字符 + 补位
        assert_eq!(
            normalize_otp("jbswy3dp ehpk-3pxp").unwrap(),
            Otp::Secret("JBSWY3DPEHPK3PXP".into())
        );
        assert_eq!(
            normalize_otp("JBSWY3DPEHPK3PXP====").unwrap(),
            Otp::Secret("JBSWY3DPEHPK3PXP".into())
        );
        // otpauth URI
        assert_eq!(
            normalize_otp("otpauth://totp/QNAP:test2?secret=JBSWY3DPEHPK3PXP&issuer=QNAP&algorithm=SHA1&digits=6&period=30").unwrap(),
            Otp::Secret("JBSWY3DPEHPK3PXP".into())
        );
        // 6 位数字 → 一次性验证码
        assert_eq!(normalize_otp("123456").unwrap(), Otp::Code("123456".into()));
        // 非默认参数 → 明确拒绝
        assert!(normalize_otp("otpauth://totp/x?secret=JBSWY3DPEHPK3PXP&digits=8").is_err());
        assert!(normalize_otp("otpauth://totp/x?secret=JBSWY3DPEHPK3PXP&period=60").is_err());
        assert!(
            normalize_otp("otpauth://totp/x?secret=JBSWY3DPEHPK3PXP&algorithm=SHA256").is_err()
        );
        // 非法密钥
        assert!(normalize_otp("not a base32 secret!").is_err());
        assert!(normalize_otp("ABC").is_err());
        assert!(normalize_otp("").is_err());
    }

    #[test]
    fn resolve_code_returns_code_or_computed() {
        assert_eq!(resolve_code("000123").unwrap(), "000123");
        let code = resolve_code("JBSWY3DPEHPK3PXP").unwrap();
        assert_eq!(code.len(), 6);
        assert!(code.bytes().all(|b| b.is_ascii_digit()));
    }

    #[test]
    fn code_rejects_secret_passed_to_code_at() {
        // code_at 只接受密钥
        assert!(code_at("123456", 59).is_err());
    }
}
