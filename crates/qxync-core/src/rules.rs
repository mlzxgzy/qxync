//! ★ M7：选择性同步 / 临时文件过滤的**路径规则引擎**（纯逻辑，方便单测）。
//!
//! 设计见 `docs/M7-选择性同步与LAN直连.md` §1。要点：
//!
//! * 匹配输入是**根相对路径**（`/qxync-test/secret`，相对于命中它的那个远端根），
//!   与 baseline / pin / 缓存键 / 上传队列用的是同一套键。
//! * 语法是 gitignore 风味的子集：`#` 注释、`!` 反向包含、`/` 锚定、
//!   尾 `/` 只匹配目录（匹配上就**整棵子树**一起排除）、`*`（不跨 `/`）、`**`（跨层级）、`?`。
//! * 不含 `/` 的规则匹配**任意层级**的单个名字（`*.iso` 到处都算）。
//! * 祖先目录被排除 → 后代一起排除（逐级求值，`!` 可以在后代上翻回来）。
//!
//! 铁律：排除 ≠ 删除。规则只决定「看不见 / 不同步 / 不水合 / 不脱水」，
//! 绝不删除任何本地或远端内容（见文档 §0）。

use std::fmt;

/// 内置临时文件规则（报告 03 §3.7 / 12 §6；`*.qxync-part` 是我们自己的下载临时文件）。
pub const TEMP_PATTERNS: &[&str] = &[
    "*.crdownload",
    "~$*",
    ".goutputstream-*",
    ".upload_cache",
    ".upload_cache*",
    "*.qxync-part",
    "*.qxync-tmp",
];

/// 为什么一个路径在挂载点里不可见。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HideReason {
    /// `exclude` 规则命中（选择性同步：根本不管）。
    Excluded,
    /// 临时文件规则命中（写到一半的下载/Office 锁文件…）。
    Temp,
}

impl HideReason {
    pub fn as_str(self) -> &'static str {
        match self {
            HideReason::Excluded => "excluded",
            HideReason::Temp => "temp",
        }
    }
}

impl fmt::Display for HideReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条编译好的规则。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pattern {
    /// 原始文本（诊断/回显用）。
    raw: String,
    /// `!` 前缀：重新包含。
    negated: bool,
    /// 尾 `/`：只匹配目录。
    dir_only: bool,
    /// 去掉前导 `/` 与尾部 `/` 的 glob。
    glob: String,
    /// glob 里含 `/`（= 从根算起匹配整条路径）。
    has_slash: bool,
}

fn parse_pattern(line: &str) -> Option<Pattern> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    let (negated, rest) = match t.strip_prefix('!') {
        Some(r) => (true, r.trim()),
        None => (false, t),
    };
    if rest.is_empty() {
        return None;
    }
    let dir_only = rest.ends_with('/');
    let body = rest.trim_matches('/');
    if body.is_empty() {
        return None;
    }
    Some(Pattern {
        raw: t.to_string(),
        negated,
        dir_only,
        glob: body.to_string(),
        has_slash: body.contains('/'),
    })
}

impl Pattern {
    /// 单条规则是否命中某条**根相对路径**（形如 `/a/b`）。
    fn matches_path(&self, path: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        let path = path.trim_matches('/');
        if path.is_empty() {
            return false;
        }
        if self.has_slash {
            glob_match(self.glob.as_bytes(), path.as_bytes())
        } else {
            path.split('/')
                .any(|seg| glob_match(self.glob.as_bytes(), seg.as_bytes()))
        }
    }
}

/// glob 匹配：`*` 不跨 `/`，`**` 跨层级，`?` 匹配一个非 `/` 字符。
fn glob_match(pat: &[u8], txt: &[u8]) -> bool {
    if pat.is_empty() {
        return txt.is_empty();
    }
    match pat[0] {
        b'*' => {
            if pat.len() >= 2 && pat[1] == b'*' {
                let rest = &pat[2..];
                // `**/` 也匹配「零层目录」：`**/b` 命中 `b`。
                if rest.first() == Some(&b'/') && glob_match(&rest[1..], txt) {
                    return true;
                }
                for i in 0..=txt.len() {
                    if glob_match(rest, &txt[i..]) {
                        return true;
                    }
                }
                false
            } else {
                let rest = &pat[1..];
                let mut i = 0;
                loop {
                    if glob_match(rest, &txt[i..]) {
                        return true;
                    }
                    if i >= txt.len() || txt[i] == b'/' {
                        return false;
                    }
                    i += 1;
                }
            }
        }
        b'?' => {
            if txt.is_empty() || txt[0] == b'/' {
                false
            } else {
                glob_match(&pat[1..], &txt[1..])
            }
        }
        c => {
            if txt.first() == Some(&c) {
                glob_match(&pat[1..], &txt[1..])
            } else {
                false
            }
        }
    }
}

/// 编译好的规则集。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rules {
    patterns: Vec<Pattern>,
    filter_temp: bool,
    /// 解析失败的原文（不静默：`qxync rules` 会展示，调用方负责 WARN）。
    bad: Vec<String>,
}

/// `Rules::parse` 的结果（坏规则单独回给调用方打日志）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleParse {
    pub rules: Rules,
    pub bad: Vec<String>,
}

impl Default for Rules {
    /// 缺省 = 没有 `exclude`，但**开**内置临时文件过滤（与 `filter_temp` 缺省一致）。
    fn default() -> Self {
        Self {
            patterns: Vec::new(),
            filter_temp: true,
            bad: Vec::new(),
        }
    }
}

impl Rules {
    /// 从 `exclude` 行解析。空/注释行忽略；解析不了的进 `bad`（不影响其它规则）。
    pub fn parse(lines: &[String], filter_temp: bool) -> RuleParse {
        let mut patterns = Vec::new();
        let mut bad = Vec::new();
        for line in lines {
            // 一条规则 = 一个 JSON 数组项；额外允许用 `;` 在一项里写多条
            // （**不**用逗号：文件名里带逗号是合法的，不能自作聪明拆开）。
            for piece in line.split(';') {
                let piece = piece.trim();
                if piece.is_empty() {
                    continue;
                }
                match parse_pattern(piece) {
                    Some(p) => patterns.push(p),
                    None => {
                        if !piece.starts_with('#') {
                            bad.push(piece.to_string());
                        }
                    }
                }
            }
        }
        RuleParse {
            rules: Rules {
                patterns,
                filter_temp,
                bad: bad.clone(),
            },
            bad,
        }
    }

    /// 只有内置临时过滤、没有任何用户规则的规则集。
    pub fn temp_only(filter_temp: bool) -> Self {
        Rules {
            patterns: Vec::new(),
            filter_temp,
            bad: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    pub fn filter_temp(&self) -> bool {
        self.filter_temp
    }

    pub fn bad(&self) -> &[String] {
        &self.bad
    }

    /// 规则原文（按生效顺序），给 `qxync rules` 回显。
    pub fn pattern_list(&self) -> Vec<String> {
        self.patterns.iter().map(|p| p.raw.clone()).collect()
    }

    /// 是否命中 `exclude`（不含临时文件规则）。
    pub fn is_excluded(&self, rel: &str, is_dir: bool) -> bool {
        let norm = normalize_rel(rel);
        let segs: Vec<&str> = norm.split('/').filter(|s| !s.is_empty()).collect();
        let mut excluded = false;
        for i in 0..segs.len() {
            let prefix = format!("/{}", segs[..=i].join("/"));
            // 前缀一定是目录；只有最后一个片段可能是文件。
            let prefix_is_dir = i + 1 < segs.len() || is_dir;
            for p in &self.patterns {
                if p.matches_path(&prefix, prefix_is_dir) {
                    excluded = !p.negated;
                }
            }
        }
        excluded
    }

    /// 是否命中内置临时文件规则（任何一层目录名/文件名）。
    pub fn is_temp(&self, rel: &str) -> bool {
        if !self.filter_temp {
            return false;
        }
        let norm = normalize_rel(rel);
        norm.split('/').filter(|s| !s.is_empty()).any(|seg| {
            TEMP_PATTERNS
                .iter()
                .any(|p| glob_match(p.as_bytes(), seg.as_bytes()))
        })
    }

    /// 综合判定：不可见（排除或临时文件）。
    pub fn hides(&self, rel: &str, is_dir: bool) -> Option<HideReason> {
        if self.is_temp(rel) {
            return Some(HideReason::Temp);
        }
        if self.is_excluded(rel, is_dir) {
            return Some(HideReason::Excluded);
        }
        None
    }

    /// 便捷入口：远端绝对路径 + 该路径所属的根。
    pub fn hides_remote(&self, root: &str, remote: &str, is_dir: bool) -> Option<HideReason> {
        let rel = rel_under(root, remote)?;
        self.hides(&rel, is_dir)
    }

    /// 便捷入口：在 `roots` 里找归属根后判定（找不到归属 = 不隐藏）。
    pub fn hides_in_roots(
        &self,
        roots: &[String],
        remote: &str,
        is_dir: bool,
    ) -> Option<HideReason> {
        let root = containing_root(roots, remote)?;
        self.hides_remote(root, remote, is_dir)
    }
}

/// 归一根相对路径：补前导 `/`、去尾 `/`、去重复斜杠。
pub fn normalize_rel(rel: &str) -> String {
    let mut out = String::with_capacity(rel.len() + 1);
    for seg in rel.split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        out.push_str(seg);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// `rel` 里哪一个根包含 `remote`（最长前缀优先）。
pub fn containing_root<'a>(roots: &'a [String], remote: &str) -> Option<&'a str> {
    let mut best: Option<&str> = None;
    for r in roots {
        let r = r.trim_end_matches('/');
        if r.is_empty() {
            continue;
        }
        if remote == r || remote.starts_with(&format!("{r}/")) {
            if best.map(|b| r.len() > b.len()).unwrap_or(true) {
                best = Some(r);
            }
        }
    }
    best
}

/// `remote` 相对于 `root` 的路径（`/a/b`）；不属于该根 → `None`。
pub fn rel_under(root: &str, remote: &str) -> Option<String> {
    let root = root.trim_end_matches('/');
    if remote == root {
        return Some("/".to_string());
    }
    remote
        .strip_prefix(&format!("{root}/"))
        .map(|s| format!("/{}", s.trim_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(lines: &[&str]) -> Rules {
        Rules::parse(
            &lines.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            true,
        )
        .rules
    }

    fn roots() -> Vec<String> {
        vec!["/home".to_string(), "/Public".to_string()]
    }

    #[test]
    fn anchored_and_any_level() {
        let r = rules(&["/qxync-test/secret", "*.iso"]);
        assert!(r.is_excluded("/qxync-test/secret", true));
        assert!(r.is_excluded("/qxync-test/secret/deep/file.txt", false)); // 祖先剪枝
        assert!(!r.is_excluded("/qxync-test/other/secret", false)); // 锚定只认根下的那条
        assert!(r.is_excluded("/a/b/x.iso", false)); // 无斜杠 = 任意层级
        assert!(r.is_excluded("/Ubuntu.iso", false));
        assert!(!r.is_excluded("/a/b/x.iso.txt", false));
    }

    #[test]
    fn dir_only_prunes_subtree_but_keeps_same_named_file() {
        let r = rules(&["cache/"]);
        assert!(r.is_excluded("/cache", true));
        assert!(r.is_excluded("/cache/x.bin", false));
        assert!(r.is_excluded("/a/cache/y.bin", false));
        assert!(!r.is_excluded("/cache", false)); // 同名**文件**不受尾 / 影响
    }

    #[test]
    fn double_star_and_question() {
        let r = rules(&["/docs/**/tmp", "file?.txt"]);
        assert!(r.is_excluded("/docs/a/b/tmp", true));
        assert!(r.is_excluded("/docs/tmp", true)); // **/ 匹配零层
        assert!(!r.is_excluded("/docs/a/b/other", true));
        assert!(r.is_excluded("/x/file1.txt", false));
        assert!(!r.is_excluded("/x/file12.txt", false));
    }

    #[test]
    fn negation_reincludes_child() {
        let r = rules(&["/data/", "!/data/keep.txt"]);
        assert!(r.is_excluded("/data", true));
        assert!(r.is_excluded("/data/drop.bin", false));
        assert!(!r.is_excluded("/data/keep.txt", false));
    }

    #[test]
    fn temp_patterns_and_switch() {
        let r = rules(&[]);
        assert_eq!(r.hides("/a/b.crdownload", false), Some(HideReason::Temp));
        assert_eq!(r.hides("/a/~$doc.docx", false), Some(HideReason::Temp));
        assert_eq!(
            r.hides("/a/.goutputstream-1A2B", false),
            Some(HideReason::Temp)
        );
        assert_eq!(r.hides("/.upload_cache", true), Some(HideReason::Temp));
        assert_eq!(r.hides("/a/x.qxync-part", false), Some(HideReason::Temp));
        assert_eq!(r.hides("/a/normal.txt", false), None);

        let off = Rules::parse(&[], false).rules;
        assert_eq!(off.hides("/a/b.crdownload", false), None);
    }

    #[test]
    fn root_relative_matching_and_containing_root() {
        let r = rules(&["/qxync-test/secret"]);
        let roots = roots();
        // /home 下命中；/Public 下同名的路径不命中（相对路径不同）
        assert_eq!(
            r.hides_in_roots(&roots, "/home/qxync-test/secret", true),
            Some(HideReason::Excluded)
        );
        assert_eq!(
            r.hides_in_roots(&roots, "/Public/other/secret", false),
            None
        );
        // 最长前缀：/home/test1 与 /home 同时给时，属于 /home/test1 的根
        let long = vec!["/home".to_string(), "/home/test1".to_string()];
        assert_eq!(
            containing_root(&long, "/home/test1/a.txt"),
            Some("/home/test1")
        );
        assert_eq!(rel_under("/home", "/home/a/b").as_deref(), Some("/a/b"));
        assert_eq!(rel_under("/home", "/home").as_deref(), Some("/"));
        assert_eq!(rel_under("/home", "/Public/a"), None);
    }

    #[test]
    fn bad_rules_are_reported_not_panicked() {
        // 只有分隔符/空白/注释的规则 → 忽略；其余进 bad
        let parsed = Rules::parse(
            &[
                "/ok".into(),
                "# 注释".into(),
                " ; ".into(),
                "/".into(),
                "///".into(),
            ],
            true,
        );
        assert_eq!(parsed.rules.pattern_list(), vec!["/ok".to_string()]);
        assert_eq!(parsed.bad, vec!["/".to_string(), "///".to_string()]);
        // 文件名里带逗号是合法的，不能被当成多条规则拆开
        let comma = Rules::parse(&["a,b.txt".into()], true);
        assert_eq!(comma.rules.pattern_list(), vec!["a,b.txt".to_string()]);
        assert!(comma.rules.is_excluded("/x/a,b.txt", false));
    }

    #[test]
    fn single_root_backward_compat() {
        // 没有任何 exclude 规则 + 临时文件不命中 → 什么都不隐藏（M1–M6 行为）
        let r = Rules::temp_only(true);
        for p in ["/qxync-test/a.bin", "/a/b/c.txt", "/Public/x"] {
            assert_eq!(r.hides(p, false), None, "{p} 不该被隐藏");
        }
    }

    #[test]
    fn rel_normalization() {
        assert_eq!(normalize_rel("a//b/"), "/a/b");
        assert_eq!(normalize_rel("/"), "/");
        assert_eq!(normalize_rel(""), "/");
    }
}
