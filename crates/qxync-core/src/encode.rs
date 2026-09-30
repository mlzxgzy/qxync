//! URL 编码。
//!
//! **实测教训（2026-09-30）**：`/cgi-bin/filemanager/utilRequest.cgi?func=download`
//! 收 `source_file=空 格 中文名.txt` 时，如果空格被编码成 `+`（`form_urlencoded` /
//! `serde_urlencoded` 的默认行为）会返回 **404**；必须编码成 `%20`。
//! 中文的 UTF-8 百分号编码是正常的。
//!
//! 所以本项目**不使用** `reqwest::Client::query()` / `form_urlencoded`，
//! 一律用 [`build_query`] 自己拼查询串。

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

/// 需要百分号编码的 ASCII 字符集合：除 unreserved（字母/数字/`-._~`）之外全部编码。
const QUERY_VALUE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'$')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// 把单个值编码成可以安全放进查询串的形式（空格 → `%20`）。
pub fn encode_query_value(value: &str) -> String {
    utf8_percent_encode(value, QUERY_VALUE).to_string()
}

/// 按出现顺序拼查询串（不含前导 `?`）。
pub fn build_query(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(k);
        out.push('=');
        out.push_str(&encode_query_value(v));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_becomes_percent_20_not_plus() {
        assert_eq!(
            encode_query_value("空 格 中文名.txt"),
            "%E7%A9%BA%20%E6%A0%BC%20%E4%B8%AD%E6%96%87%E5%90%8D.txt"
        );
        assert!(!encode_query_value("a b").contains('+'));
    }

    #[test]
    fn slash_and_plus_are_encoded() {
        assert_eq!(
            encode_query_value("/home/qxync-test"),
            "%2Fhome%2Fqxync-test"
        );
        assert_eq!(encode_query_value("a+b"), "a%2Bb");
    }

    #[test]
    fn unreserved_kept() {
        assert_eq!(encode_query_value("hello.txt-1_2~3"), "hello.txt-1_2~3");
    }

    #[test]
    fn build_query_joins_in_order() {
        let q = build_query(&[
            ("func", "download"),
            ("sid", "abc"),
            ("source_path", "/home"),
        ]);
        assert_eq!(q, "func=download&sid=abc&source_path=%2Fhome");
    }
}
