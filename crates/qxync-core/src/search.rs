//! # M42：本地文件搜索（`qxync find`）—— 同步元数据库上的只读目录检索
//!
//! 起因（用户原话）：「有目录数据库的话是不是可以快速搜索仓库内的文件啊」。
//!
//! 答案是：**能，但能搜的只有元数据，不是文件内容**。qxync 的状态库
//! （`sync.db`）里本来就有两份天然的「目录数据库」：
//!
//! * `baseline`：上次同步成功时的**远端**路径 → 签名（`present / is_dir / size /
//!   mtime`）。一次全量对账后就是 NAS 同步树的完整清单（真机实测 28 136 个文件 +
//!   5 891 个目录）；
//! * `nodes`：文件身份 → 路径的索引；其中 `local_only = 1` 的行是**本地新建、
//!   远端还没有副本**的路径（M36），用来补上「还没上传成功的文件」。
//!
//! 所以搜索不需要挂载、不需要联网、不需要 daemon —— 只读打开 `sync.db` 即可。
//! 代价是**新鲜度**：`baseline` 每轮对账结束落盘、上传成功会立即落盘，远端侧的变化
//! 最多滞后一轮轮询（默认 30 s）；需要绝对新鲜时先跑一轮 `qxync sync --once`。
//!
//! ## 这一层只做纯函数
//!
//! [`run`] 不碰 IO：输入是 [`Baseline`] 与 `local_only` 的节点行，输出是命中的
//! [`SearchHit`]。这样单测不需要临时库/真实数据；装载由
//! [`Store::search`](crate::store::Store::search) 负责。
//!
//! ## 匹配语义（与 CLI 的 `--help` 一一对应）
//!
//! * 默认**子串**，大小写不敏感（Unicode 折叠；中文不受影响）；
//! * `--glob`：`*` 匹配任意序列、`?` 匹配单个字符；没有字符类（`[abc]` 按字面）。
//!   模式里**没有 `/`** 时只匹配文件名（`*.pdf` 能命中任意层级，不受路径前缀影响）；
//!   模式里**有 `/`** 时按整条路径匹配，此时 `*` 可跨 `/`；
//! * 模式为空 = 匹配全部；
//! * `--under` 按**路径段**判前缀：`/home/a` 不会吞掉 `/home/ab`。
//!
//! 为什么不用 SQL 的 `LIKE`：LIKE 的大小写折叠只对 ASCII 生效，而且模式里的
//! `%`/`_`/`\\` 要自己转义（转义漏一处就是「搜 `100%` 变成搜一切」）。3 万行量级
//! 在 Rust 侧过滤是毫秒级，正确性更值钱。

use crate::store::NodeRow;
use crate::sync::Baseline;

/// 模式串的匹配方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchMode {
    /// 子串（默认）。
    #[default]
    Substring,
    /// 通配符：`*` 任意序列、`?` 单字符。
    Glob,
}

/// 只要文件 / 只要目录。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KindFilter {
    /// 不筛（默认）。
    #[default]
    Any,
    /// 只要文件。
    File,
    /// 只要目录。
    Dir,
}

/// 排序键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    /// 路径升序（默认，与入库顺序一致，结果稳定）。
    #[default]
    Path,
    /// 大小降序（默认「大的在前」，`--reverse` 可翻）。
    Size,
    /// mtime 降序（默认「新的在前」，`--reverse` 可翻）。
    Mtime,
}

/// 一条命中来自哪张表。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `baseline`：远端已确认存在（上次同步成功的签名）。
    Remote,
    /// `nodes.local_only`：本地新建、远端还没有副本（还没上传成功）。
    LocalOnly,
}

impl Origin {
    /// `--json` 里的稳定字符串（不要随手改名：脚本按它判断）。
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Remote => "remote",
            Origin::LocalOnly => "local_only",
        }
    }
}

/// `--limit` 的缺省值（与 `qxync journal` 的 200 同一量级，够看又不刷屏）。
pub const DEFAULT_LIMIT: usize = 100;

/// 一次搜索的查询条件。
///
/// 所有字段都是「省略即不筛」，构造后不随搜索过程变化。
#[derive(Debug, Clone)]
pub struct SearchQuery {
    /// 模式串；`None` / 空串 = 匹配全部。
    pub pattern: Option<String>,
    /// 子串还是通配符。
    pub mode: MatchMode,
    /// 大小写敏感（默认敏感度与 `--case-sensitive` 一致：默认**不**敏感）。
    pub case_sensitive: bool,
    /// 只搜这棵子树（远端路径，已去掉末尾 `/`；`None` = 全部）。
    pub under: Option<String>,
    /// 只要文件 / 只要目录。
    pub kind: KindFilter,
    /// 文件大小的下界（含）。
    pub min_size: Option<u64>,
    /// 文件大小的上界（含）。
    pub max_size: Option<u64>,
    /// 排序键。
    pub sort: SortKey,
    /// 反向排序。
    pub reverse: bool,
    /// 最多返回多少条；`None` = 不截断。
    pub limit: Option<usize>,
}

impl Default for SearchQuery {
    fn default() -> Self {
        Self {
            pattern: None,
            mode: MatchMode::Substring,
            case_sensitive: false,
            under: None,
            kind: KindFilter::Any,
            min_size: None,
            max_size: None,
            sort: SortKey::Path,
            reverse: false,
            limit: Some(DEFAULT_LIMIT),
        }
    }
}

/// 一条命中。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// 远端路径（与 `baseline` / `nodes.path` 的字面值一致）。
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub is_dir: bool,
    pub origin: Origin,
}

/// 一次搜索的结果（`total` 是**截断前**的命中数，用来告诉用户「还有多少没显示」）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchPage {
    pub total: usize,
    pub hits: Vec<SearchHit>,
}

impl SearchPage {
    /// 是否因为 `limit` 截掉了结果。
    pub fn truncated(&self) -> bool {
        self.total > self.hits.len()
    }
}

/// 在同步元数据上跑一次搜索（纯函数：不碰文件系统、不碰数据库）。
///
/// 数据源与去重：
/// * `baseline` 里 `present = 1` 的行 → [`Origin::Remote`]；
/// * `local_only`（`nodes.local_only <> 0`）里**没有**以 `present = 1` 出现在
///   baseline 的路径 → [`Origin::LocalOnly`]；
/// * `present = 0` 的 baseline 行是墓碑，不返回。
pub fn run(baseline: &Baseline, local_only: &[NodeRow], query: &SearchQuery) -> SearchPage {
    let under = query.under.as_deref().map(|u| u.trim_end_matches('/'));
    let mut hits: Vec<SearchHit> = Vec::new();

    // baseline 是 BTreeMap（按 path 有序），遍历顺序确定。
    for (path, sig) in &baseline.entries {
        if !sig.exists {
            continue; // 墓碑
        }
        if !under_matches(path, under) {
            continue;
        }
        if !pattern_matches(query, path) {
            continue;
        }
        if !passes_filters(query, sig.is_dir, sig.size) {
            continue;
        }
        hits.push(SearchHit {
            path: path.clone(),
            size: sig.size,
            mtime: sig.mtime,
            is_dir: sig.is_dir,
            origin: Origin::Remote,
        });
    }

    // 本地新建、还没上传成功的路径（通常很少，但搜「我刚建的文件」全靠它）。
    for row in local_only {
        if baseline.get(&row.path).exists {
            continue; // 以远端签名为准，避免同一路径出两条
        }
        if !under_matches(&row.path, under) {
            continue;
        }
        if !pattern_matches(query, &row.path) {
            continue;
        }
        if !passes_filters(query, row.is_folder, row.size) {
            continue;
        }
        hits.push(SearchHit {
            path: row.path.clone(),
            size: row.size,
            mtime: row.mtime,
            is_dir: row.is_folder,
            origin: Origin::LocalOnly,
        });
    }

    sort_hits(&mut hits, query.sort);
    if query.reverse {
        hits.reverse();
    }

    let total = hits.len();
    if let Some(limit) = query.limit {
        hits.truncate(limit);
    }
    SearchPage { total, hits }
}

/// 路径是否落在 `under` 这棵子树里（按路径段判：`/a` 不匹配 `/ab`）。
///
/// `under` 为空 / `None` → 全部路径都算命中（根）。
pub fn under_matches(path: &str, under: Option<&str>) -> bool {
    let Some(under) = under
        .map(|u| u.trim_end_matches('/'))
        .filter(|u| !u.is_empty())
    else {
        return true;
    };
    if path == under {
        return true;
    }
    path.len() > under.len()
        && path.starts_with(under)
        && path.as_bytes().get(under.len()).copied() == Some(b'/')
}

fn passes_filters(query: &SearchQuery, is_dir: bool, size: u64) -> bool {
    match query.kind {
        KindFilter::Any => {}
        KindFilter::File if is_dir => return false,
        KindFilter::Dir if !is_dir => return false,
        _ => {}
    }
    if let Some(min) = query.min_size {
        if size < min {
            return false;
        }
    }
    if let Some(max) = query.max_size {
        if size > max {
            return false;
        }
    }
    true
}

fn pattern_matches(query: &SearchQuery, path: &str) -> bool {
    let Some(pattern) = query
        .pattern
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    else {
        return true;
    };
    match query.mode {
        MatchMode::Substring => {
            if query.case_sensitive {
                path.contains(pattern)
            } else {
                path.to_lowercase().contains(&pattern.to_lowercase())
            }
        }
        MatchMode::Glob => glob_match(pattern, glob_target(pattern, path), query.case_sensitive),
    }
}

/// `--glob` 的匹配对象：模式里**没有 `/`** 时只匹配文件名（`fd` 的习惯，
/// `*.pdf` 不该因为路径前缀而失败）；模式里**有 `/`** 时按整条路径匹配
/// （此时 `*` 可跨 `/`，用来写 `*2026/29-四川赛/*.pdf` 这类子路径模式）。
fn glob_target<'a>(pattern: &str, path: &'a str) -> &'a str {
    if pattern.contains('/') {
        path
    } else {
        path.rsplit('/').next().unwrap_or(path)
    }
}

fn sort_hits(hits: &mut [SearchHit], key: SortKey) {
    match key {
        SortKey::Path => hits.sort_by(|a, b| a.path.cmp(&b.path)),
        SortKey::Size => hits.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path))),
        SortKey::Mtime => {
            hits.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.path.cmp(&b.path)))
        }
    }
}

/// `--glob` 的匹配器：`*` 任意序列（含 `/`）、`?` 单字符，其余按字面。
///
/// 经典的「双指针 + 回溯到最后一个 `*`」，最坏 O(n·m)、额外空间 O(1)。
pub fn glob_match(pattern: &str, text: &str, case_sensitive: bool) -> bool {
    if case_sensitive {
        let p: Vec<char> = pattern.chars().collect();
        let t: Vec<char> = text.chars().collect();
        glob_chars(&p, &t)
    } else {
        let p: Vec<char> = pattern.to_lowercase().chars().collect();
        let t: Vec<char> = text.to_lowercase().chars().collect();
        glob_chars(&p, &t)
    }
}

fn glob_chars(pattern: &[char], text: &[char]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    // 最近一次遇到的 `*` 在模式里的位置，以及它当时匹配到的文本位置。
    let mut star_p: Option<usize> = None;
    let mut star_t = 0usize;
    while ti < text.len() {
        if pi < pattern.len() && pattern[pi] == '*' {
            star_p = Some(pi);
            star_t = ti;
            pi += 1;
        } else if pi < pattern.len() && (pattern[pi] == '?' || pattern[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if let Some(sp) = star_p {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::Sig;

    fn baseline_of(entries: &[(&str, Sig)]) -> Baseline {
        let mut b = Baseline::default();
        for (path, sig) in entries {
            b.put(*path, *sig);
        }
        b
    }

    fn node(path: &str, size: u64, mtime: i64, is_folder: bool) -> NodeRow {
        NodeRow {
            // file_id 只要求 16 字节；搜索不关心身份，用路径散列填充即可。
            file_id: {
                let mut id = [0u8; 16];
                for (i, b) in path.as_bytes().iter().take(16).enumerate() {
                    id[i] = *b;
                }
                id
            },
            path: path.into(),
            remote_path: path.into(),
            size,
            mtime,
            is_folder,
            updated_at: 0,
            local_only: true,
        }
    }

    fn q(pattern: &str) -> SearchQuery {
        SearchQuery {
            pattern: Some(pattern.into()),
            ..SearchQuery::default()
        }
    }

    fn paths(page: &SearchPage) -> Vec<String> {
        page.hits.iter().map(|h| h.path.clone()).collect()
    }

    fn sample() -> (Baseline, Vec<NodeRow>) {
        let b = baseline_of(&[
            (
                "/home/.Qsync/2026/29-四川赛/试题A.pdf",
                Sig::file(12218, 1000),
            ),
            (
                "/home/.Qsync/2026/29-四川赛/README.md",
                Sig::file(180, 2000),
            ),
            ("/home/.Qsync/docs/旧文件.txt", Sig::file(1, 3000)),
            ("/home/.Qsync/2026", Sig::dir()),
            ("/home/.Qsync/deleted.txt", Sig::MISSING),
        ]);
        let n = vec![
            node("/home/.Qsync/2026/刚建的草稿.txt", 42, 4000, false),
            // 与 baseline 重复的本地节点：必须以远端为准（不算 local_only）
            node("/home/.Qsync/2026/29-四川赛/试题A.pdf", 12218, 1000, false),
        ];
        (b, n)
    }

    #[test]
    fn substring_matches_middle_and_default_is_case_insensitive() {
        let (b, n) = sample();
        let page = run(&b, &n, &q("readme"));
        assert_eq!(
            paths(&page),
            vec!["/home/.Qsync/2026/29-四川赛/README.md".to_string()],
            "默认大小写不敏感、能命中路径中段"
        );
        // 子串不是前缀：命中「赛/试题A」
        let page = run(&b, &n, &q("赛/试题"));
        assert_eq!(page.hits.len(), 1);
        assert_eq!(page.hits[0].origin, Origin::Remote);
    }

    #[test]
    fn substring_unicode_and_case_sensitive() {
        let (b, n) = sample();
        assert_eq!(run(&b, &n, &q("试题")).hits.len(), 1);
        let mut cs = q("readme");
        cs.case_sensitive = true;
        assert!(
            run(&b, &n, &cs).hits.is_empty(),
            "大小写敏感时 README 不该命中"
        );
        let mut cs = q("README");
        cs.case_sensitive = true;
        assert_eq!(run(&b, &n, &cs).hits.len(), 1);
    }

    #[test]
    fn glob_star_crosses_slash_and_question_matches_one_char() {
        let (b, n) = sample();
        let mut qq = q("*.pdf");
        qq.mode = MatchMode::Glob;
        assert_eq!(run(&b, &n, &qq).hits.len(), 1, "无 `/` 的模式匹配文件名");

        // 有 `/` 的模式按整条路径匹配，`*` 可跨 `/`
        let mut qq = q("*2026/29-四川赛/试题?.pdf");
        qq.mode = MatchMode::Glob;
        assert_eq!(run(&b, &n, &qq).hits.len(), 1);
        let mut qq = q("*2026/29-四川赛/试题??.pdf");
        qq.mode = MatchMode::Glob;
        assert!(run(&b, &n, &qq).hits.is_empty());

        // 无 `/` 时 `?` 对文件名生效
        let mut qq = q("试题?.pdf");
        qq.mode = MatchMode::Glob;
        assert_eq!(run(&b, &n, &qq).hits.len(), 1);
    }

    #[test]
    fn glob_treats_specials_literally_and_handles_empty_pattern() {
        let mut b = Baseline::default();
        b.put("/home/a[1].txt", Sig::file(5, 1));
        b.put("/home/100%.txt", Sig::file(6, 2));
        let n = Vec::new();
        let mut qq = q("a[1].txt");
        qq.mode = MatchMode::Glob;
        assert_eq!(run(&b, &n, &qq).hits.len(), 1, "字符类不支持，`[]` 按字面");
        let mut qq = q("100%.txt");
        qq.mode = MatchMode::Glob;
        assert_eq!(run(&b, &n, &qq).hits.len(), 1, "`%` 在 glob 模式下是字面");
        // 空模式 = 全部（与子串模式一致）
        let page = run(&b, &n, &SearchQuery::default());
        assert_eq!(page.hits.len(), 2);
    }

    #[test]
    fn under_is_component_aware() {
        let mut b = Baseline::default();
        b.put("/home/a/x.txt", Sig::file(1, 1));
        b.put("/home/ab/y.txt", Sig::file(2, 2));
        let n = Vec::new();
        let mut qq = SearchQuery::default();
        qq.under = Some("/home/a".into());
        assert_eq!(paths(&run(&b, &n, &qq)), vec!["/home/a/x.txt".to_string()]);
        // 末尾斜杠容忍；根 = 全部
        qq.under = Some("/home/a/".into());
        assert_eq!(paths(&run(&b, &n, &qq)), vec!["/home/a/x.txt".to_string()]);
        qq.under = Some("/".into());
        assert_eq!(run(&b, &n, &qq).hits.len(), 2);
    }

    #[test]
    fn kind_and_size_filters() {
        let (b, n) = sample();
        let mut qq = SearchQuery::default();
        qq.kind = KindFilter::Dir;
        assert_eq!(
            paths(&run(&b, &n, &qq)),
            vec!["/home/.Qsync/2026".to_string()]
        );

        let mut qq = SearchQuery::default();
        qq.kind = KindFilter::File;
        qq.min_size = Some(1000);
        qq.max_size = Some(20000);
        let got = run(&b, &n, &qq);
        assert!(got
            .hits
            .iter()
            .all(|h| !h.is_dir && (1000..=20000).contains(&h.size)));
        assert_eq!(got.hits.len(), 1, "仅试题A.pdf 落在区间内");

        let mut qq = SearchQuery::default();
        qq.min_size = Some(0);
        qq.max_size = Some(0);
        let got = run(&b, &n, &qq);
        assert_eq!(
            paths(&got),
            vec!["/home/.Qsync/2026".to_string()],
            "目录 size=0"
        );
    }

    #[test]
    fn sorting_and_reverse() {
        let (b, n) = sample();
        let mut qq = SearchQuery::default();
        qq.sort = SortKey::Mtime;
        let got = paths(&run(&b, &n, &qq));
        // mtime 降序：本地新建(4000) > 旧文件(3000) > README(2000) > 试题A(1000) > 目录(0)
        assert_eq!(got[0], "/home/.Qsync/2026/刚建的草稿.txt");
        qq.reverse = true;
        let got = paths(&run(&b, &n, &qq));
        assert_eq!(got[0], "/home/.Qsync/2026");

        let mut qq = SearchQuery::default();
        qq.sort = SortKey::Size;
        let got = paths(&run(&b, &n, &qq));
        assert_eq!(got[0], "/home/.Qsync/2026/29-四川赛/试题A.pdf");
    }

    #[test]
    fn limit_truncates_but_total_counts_everything() {
        let (b, n) = sample();
        let mut qq = SearchQuery::default();
        qq.limit = Some(2);
        let page = run(&b, &n, &qq);
        assert_eq!(page.hits.len(), 2);
        assert!(page.truncated());
        assert_eq!(page.total, 5, "墓碑不算；5 = 4 baseline + 1 local_only");
        let mut qq = SearchQuery::default();
        qq.limit = None;
        let page = run(&b, &n, &qq);
        assert_eq!(page.hits.len(), 5);
        assert!(!page.truncated());
    }

    #[test]
    fn local_only_rows_are_included_and_deduped() {
        let (b, n) = sample();
        let page = run(&b, &n, &q("试卷"));
        assert!(page.hits.is_empty());
        let page = run(&b, &n, &q("草稿"));
        assert_eq!(page.hits.len(), 1);
        assert_eq!(page.hits[0].origin, Origin::LocalOnly);
        assert_eq!(page.hits[0].size, 42);

        // 与 baseline 同路径的 local_only 行不重复出现
        let page = run(&b, &n, &q("试题A.pdf"));
        assert_eq!(page.hits.len(), 1);
        assert_eq!(page.hits[0].origin, Origin::Remote);
    }

    #[test]
    fn tombstone_rows_are_not_returned() {
        let (b, n) = sample();
        let page = run(&b, &n, &q("deleted"));
        assert!(
            page.hits.is_empty(),
            "present=0 是墓碑，不能出现在搜索结果里"
        );
        assert_eq!(Origin::Remote.as_str(), "remote");
        assert_eq!(Origin::LocalOnly.as_str(), "local_only");
    }

    #[test]
    fn glob_matcher_edge_cases() {
        assert!(glob_match("*", "/任何/路径", false));
        assert!(glob_match("", "", false));
        assert!(!glob_match("", "x", false));
        assert!(glob_match("a*b*c", "aXbYc", true));
        assert!(glob_match("AB", "ab", false));
        assert!(!glob_match("AB", "ab", true));
        assert!(glob_match("?", "中", true), "? 匹配一个 Unicode 字符");
        assert!(!glob_match("?", "中文", true));
        assert!(glob_match("*试题*", "/x/试题/1", true));
        // 回溯场景：`*` 需要让出字符才能整体匹配
        assert!(glob_match("*aab", "aaab", true));
        assert!(!glob_match("*aab", "aaa", true));
    }
}
