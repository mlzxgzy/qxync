//! ★ 单根：**一个挂载点 = 一个 NAS 文件夹**（一对一）。
//!
//! 历史（2026-10-02 之前的 M6）：曾支持「一个挂载点覆盖多个远端根（家目录 + 共享文件夹）」，
//! 多根时挂载点顶层会多出每个根的名字（`home/`、`Public/`）。用户实测反馈这不对 ——
//! 本地 `/home/user/qsync` 配 NAS `/home` 之后，挂载点里又冒出一层 `home/` ——
//! 于是**一对多整体删除**：任务、挂载、FUSE 视图都只对应一个 NAS 文件夹。
//!
//! 要同步多个 NAS 文件夹 → 建多个任务（每个任务一个本地文件夹 + 一个 NAS 文件夹）。
//! 非家目录的 NAS 文件夹（共享文件夹）仍然可以配，只是**默认只读**：实测普通账号往
//! 非 Qsync 同步文件夹上传会被服务端拒绝（`status:20`）。

/// 归一化**一个**远端路径：去空白、补前导 `/`、去尾 `/`。空输入 → 家目录（[`crate::HOME_ROOT`]）。
pub fn normalize_root(root: &str) -> String {
    let r = root.trim();
    if r.is_empty() {
        return crate::HOME_ROOT.to_string();
    }
    let mut s = if r.starts_with('/') {
        r.to_string()
    } else {
        format!("/{r}")
    };
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    s
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_root_trims_and_defaults() {
        assert_eq!(normalize_root(""), "/home");
        assert_eq!(normalize_root("   "), "/home");
        assert_eq!(normalize_root("/Public/"), "/Public");
        assert_eq!(normalize_root("Public"), "/Public");
        assert_eq!(normalize_root("  /home  "), "/home");
        assert_eq!(normalize_root("/"), "/");
    }

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
            client_path_from_share("/share/homes/test1/Qsync", None, "/home/test1/", "test1")
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
