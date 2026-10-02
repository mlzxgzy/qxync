//! ★ M6：多根 / 共享文件夹的**视图布局**（纯逻辑，方便单测）。
//!
//! Qsync 的同步范围不止主目录：普通用户的家目录是 `/home`，但 NAS 上的共享文件夹
//! （`/Public`、`/Multimedia`…）只要账号有权限也能读写。M6 把「一个挂载点 = 一个远端根」
//! 扩展成「一个挂载点 = N 个远端根」。
//!
//! 两种布局（**向后兼容是硬要求**）：
//!
//! ```text
//! 单根（Passthrough，M1–M5 的行为，一字不改）:
//!     mount ~/mnt --remote /home        ~/mnt/qxync-test/hello.txt  →  /home/qxync-test/hello.txt
//!
//! 多根（Multi，M6 新增）:
//!     mount ~/mnt --remote /home --remote /Public
//!     ~/mnt/home/qxync-test/hello.txt   →  /home/qxync-test/hello.txt
//!     ~/mnt/Public/tailscale.txt        →  /Public/tailscale.txt
//! ```
//!
//! 设计约束：
//!
//! * **远端路径是唯一真值**：缓存文件名、baseline、pin、xattr 全都用远端路径做键
//!   （M2a 的教训：不能拿 ino 当键）。多根只是在**挂载点这一层**加了一次名字映射，
//!   下面所有层（水合/缓存/上传队列/对账）拿到的仍是远端路径，不用改。
//! * **可写性只有家目录**：实测（2026-10-01）普通账号向 `/Public` 上传会被服务端拒绝
//!   （`status:20`，非 Qsync 同步文件夹没有写权限），而 `qbox_get_syncing_folder_list`
//!   在该账号上是空的。所以非家目录的根**默认只读**，写操作直接回 `EROFS`，
//!   而不是把服务端那句含糊的 status 20 抛给用户。

use std::collections::BTreeSet;

/// 普通用户家目录根（`/home`），也是唯一的默认根。
pub const DEFAULT_ROOT: &str = "/home";

/// 一个远端根在挂载点里的表示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootSpec {
    /// 远端绝对路径，如 `/Public`。
    pub remote: String,
    /// 挂载点里的目录名（单根时用不到；多根时是顶层目录名）。
    pub view_name: String,
    /// 是否允许写（当前规则：只有家目录根可写）。
    pub writable: bool,
}

/// 挂载点布局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewLayout {
    /// 单根：挂载点**就是**那个远端根（M1–M5 的行为）。
    Passthrough { root: String },
    /// 多根：挂载点是虚拟根，下面是各根的目录。
    Multi { entries: Vec<RootSpec> },
}

impl ViewLayout {
    pub fn roots(&self) -> Vec<String> {
        match self {
            ViewLayout::Passthrough { root } => vec![root.clone()],
            ViewLayout::Multi { entries } => entries.iter().map(|e| e.remote.clone()).collect(),
        }
    }
    pub fn is_multi(&self) -> bool {
        matches!(self, ViewLayout::Multi { .. })
    }
}

/// 归一化一组根：去空白、补前导 `/`、去掉结尾 `/`、去重、保持顺序。
///
/// 空输入 → `["/home"]`（默认根）。
pub fn normalize_roots(roots: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in roots {
        let r = r.trim();
        if r.is_empty() {
            continue;
        }
        let mut s = if r.starts_with('/') {
            r.to_string()
        } else {
            format!("/{r}")
        };
        while s.len() > 1 && s.ends_with('/') {
            s.pop();
        }
        if !out.contains(&s) {
            out.push(s);
        }
    }
    if out.is_empty() {
        out.push(DEFAULT_ROOT.to_string());
    }
    out
}

/// 把 NAS 上报的同步文件夹**共享路径**映射成**客户端可见路径**。
///
/// 真机依据（2026-10-02 HAR，`qbox_get_syncing_folder_list&detail=1`）：
/// NAS 回的是 `path = /share/homes/test1/.Qsync`，紧接着客户端就去 `stat`
/// `path=/home` + `file_name=.Qsync` —— 也就是 `/home/.Qsync`。所以映射规则是
/// 「共享路径里的家目录前缀 `/share/homes/<user>` ⇒ `home_root`」。
///
/// 认这几种前缀（大小写敏感，NAS 路径就是大小写敏感的）：
/// * `/share/homes/<user>`（真机）
/// * `/share/CACHEDEVn_DATA/homes/<user>`（`realpath` 形态，从 `realpath` 里再剥一层）
/// * `/homes/<user>`（短形态）
///
/// 已经在 `home_root` 之内的路径原样返回；对不上任何前缀 → `None`
/// （**宁可不给候选项，也不猜一个错的 NAS 路径**）。
pub fn client_path_from_share(
    path: &str,
    realpath: Option<&str>,
    home_root: &str,
    user: &str,
) -> Option<String> {
    let home = home_root.trim_end_matches('/');
    let home = if home.is_empty() { "/" } else { home };
    let user = user.trim();
    if user.is_empty() {
        return None;
    }
    let join = |rest: &str| -> String {
        let rest = rest.trim_matches('/');
        if rest.is_empty() {
            home.to_string()
        } else if home == "/" {
            format!("/{rest}")
        } else {
            format!("{home}/{rest}")
        }
    };

    // 先看原样给的 path；对不上就试 realpath（有些固件 path 给的是卷内真实路径）。
    for raw in [Some(path), realpath].into_iter().flatten() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let raw = raw.trim_end_matches('/');
        // 已经在客户端路径里了（例如用户自己填的就是 /home/xxx）
        if home != "/" && (raw == home || raw.starts_with(&format!("{home}/"))) {
            return Some(raw.to_string());
        }
        // 从右往左找 `/homes/<user>`，它左边整段（`/share`、`/share/CACHEDEV1_DATA`…）都当挂载前缀
        let marker = format!("/homes/{user}");
        if let Some(pos) = raw.find(&marker) {
            let after = &raw[pos + marker.len()..];
            if after.is_empty() || after.starts_with('/') {
                return Some(join(after));
            }
        }
    }
    None
}

/// 给一个远端起视图名（basename；同名时用「父目录_名字」消歧）。
fn view_name(remote: &str, taken: &BTreeSet<String>) -> String {
    let base = remote
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string();
    let base = if base.is_empty() {
        "root".to_string()
    } else {
        base
    };
    if !taken.contains(&base) {
        return base;
    }
    // `/a/b` 与 `/c/b` → `a_b` / `c_b`
    let parts: Vec<&str> = remote.trim_matches('/').split('/').collect();
    let joined = parts.join("_");
    let joined = if joined.is_empty() {
        "root".to_string()
    } else {
        joined
    };
    if !taken.contains(&joined) {
        return joined;
    }
    let mut i = 2;
    loop {
        let cand = format!("{joined}_{i}");
        if !taken.contains(&cand) {
            return cand;
        }
        i += 1;
    }
}

/// 算出挂载布局。`writable_root` 是可写的那一个根（当前就是家目录；None = 全部只读）。
pub fn layout(roots: &[String], writable_root: Option<&str>) -> ViewLayout {
    let roots = normalize_roots(roots);
    let is_writable = |r: &str| writable_root.map(|w| w == r).unwrap_or(false);
    if roots.len() == 1 {
        return ViewLayout::Passthrough {
            root: roots[0].clone(),
        };
    }
    let mut taken = BTreeSet::new();
    let mut entries = Vec::with_capacity(roots.len());
    for remote in roots {
        let name = view_name(&remote, &taken);
        taken.insert(name.clone());
        entries.push(RootSpec {
            writable: is_writable(&remote),
            remote,
            view_name: name,
        });
    }
    ViewLayout::Multi { entries }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_falls_back_to_home() {
        assert_eq!(normalize_roots(&[]), vec!["/home".to_string()]);
        assert_eq!(normalize_roots(&v(&["  ", ""])), vec!["/home".to_string()]);
    }

    #[test]
    fn normalization_trims_and_dedupes() {
        assert_eq!(
            normalize_roots(&v(&[" /Public/ ", "Public", "/Public", "/Multimedia"])),
            v(&["/Public", "/Multimedia"])
        );
        assert_eq!(normalize_roots(&v(&["/home/"])), v(&["/home"]));
        assert_eq!(normalize_roots(&v(&["/"])), v(&["/"]));
    }

    #[test]
    fn single_root_stays_passthrough() {
        // ★ 向后兼容：单根必须还是「挂载点就是那个根」，否则 M1–M5 的验收矩阵全废
        assert_eq!(
            layout(&v(&["/home"]), Some("/home")),
            ViewLayout::Passthrough {
                root: "/home".into()
            }
        );
        assert_eq!(
            layout(&v(&["/home", "/home"]), Some("/home")),
            ViewLayout::Passthrough {
                root: "/home".into()
            }
        );
        // 只读的家目录（--ro 挂载）也是直通布局
        assert_eq!(
            layout(&v(&["/home"]), None),
            ViewLayout::Passthrough {
                root: "/home".into()
            }
        );
    }

    #[test]
    fn multi_root_names_and_writability() {
        let l = layout(&v(&["/home", "/Public", "/Multimedia"]), Some("/home"));
        let roots = l.roots();
        assert!(l.is_multi());
        match l {
            ViewLayout::Multi { entries } => {
                let names: Vec<&str> = entries.iter().map(|e| e.view_name.as_str()).collect();
                assert_eq!(names, vec!["home", "Public", "Multimedia"]);
                assert!(entries[0].writable, "家目录可写");
                assert!(
                    !entries[1].writable,
                    "共享文件夹默认只读（实测服务端拒绝写）"
                );
                assert!(!entries[2].writable);
                assert_eq!(roots, v(&["/home", "/Public", "/Multimedia"]));
                assert_eq!(entries.len(), 3);
            }
            other => panic!("应该是多根布局: {other:?}"),
        }
    }

    #[test]
    fn duplicate_basenames_get_distinct_view_names() {
        let l = layout(&v(&["/a/b", "/c/b", "/c/b/x"]), None);
        match l {
            ViewLayout::Multi { entries } => {
                let names: Vec<&str> = entries.iter().map(|e| e.view_name.as_str()).collect();
                assert_eq!(names, vec!["b", "c_b", "x"]);
                // 名字唯一是硬要求：否则顶层目录会撞在一起
                let uniq: BTreeSet<&String> = entries.iter().map(|e| &e.view_name).collect();
                assert_eq!(uniq.len(), entries.len());
            }
            other => panic!("应该是多根布局: {other:?}"),
        }
    }

    #[test]
    fn root_slash_gets_a_name() {
        let l = layout(&v(&["/", "/Public"]), None);
        match l {
            ViewLayout::Multi { entries } => assert_eq!(entries[0].view_name, "root"),
            other => panic!("{other:?}"),
        }
    }

    // ---------------------------------------------------------------- 共享路径 → 客户端路径

    /// ★ 真机原文（2026-10-02 HAR）：`path=/share/homes/test1/.Qsync` ⇒ `/home/.Qsync`。
    /// 紧接着客户端的 `stat` 就是 `path=/home&file_name=.Qsync`，这条映射有直接证据。
    #[test]
    fn share_path_maps_to_client_path_by_real_machine_evidence() {
        assert_eq!(
            client_path_from_share(
                "/share/homes/test1/.Qsync",
                Some("/share/CACHEDEV1_DATA/homes/test1/.Qsync"),
                "/home",
                "test1"
            )
            .as_deref(),
            Some("/home/.Qsync")
        );
        // 家目录本身（没有子目录）
        assert_eq!(
            client_path_from_share("/share/homes/test1", None, "/home", "test1").as_deref(),
            Some("/home")
        );
        // 用户自定义过 home_root 也一样按 home_root 拼
        assert_eq!(
            client_path_from_share(
                "/share/homes/test1/Qsync",
                None,
                "/home/test1/",
                "test1"
            )
            .as_deref(),
            Some("/home/test1/Qsync")
        );
    }

    #[test]
    fn share_path_mapping_accepts_volume_realpath_and_is_not_greedy() {
        // path 给的是卷内真实路径 → 退到 realpath 也能认
        assert_eq!(
            client_path_from_share(
                "/share/CACHEDEV1_DATA/homes/test1/.Qsync",
                None,
                "/home",
                "test1"
            )
            .as_deref(),
            Some("/home/.Qsync")
        );
        // 短形态 /homes/<user>
        assert_eq!(
            client_path_from_share("/homes/test1/x", None, "/home", "test1").as_deref(),
            Some("/home/x")
        );
        // 已经在 home_root 里 → 原样返回
        assert_eq!(
            client_path_from_share("/home/Public", None, "/home", "test1").as_deref(),
            Some("/home/Public")
        );
        // ★ 前缀相似但**不是**该用户（test1 vs test10）→ 宁可不给候选
        assert_eq!(
            client_path_from_share("/share/homes/test10/x", None, "/home", "test1"),
            None
        );
        // 别人的家目录 / 认不出的路径 → None
        assert_eq!(
            client_path_from_share("/share/Public", None, "/home", "test1"),
            None
        );
        assert_eq!(client_path_from_share("", None, "/home", "test1"), None);
        // 没有用户名的 link（未配置）→ 不猜
        assert_eq!(
            client_path_from_share("/share/homes//x", None, "/home", "  "),
            None
        );
    }
}
