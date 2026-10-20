//! # M23：Baloo 索引配置检测（Dolphin 集成的前置条件）
//!
//! 背景（见 `docs/调研-xattr标签与Dolphin状态列.md`）：把水合状态暴露成
//! `user.xdg.tags` 之后，Dolphin 的 Tags 列能直接显示「仅在线 / 部分本地 / 已水合」。
//! 但 **Baloo 内容索引器会把挂载点里的占位符整个拉下来**（索引 = 读内容 =
//! 水合），所以开启 Dolphin 集成之前必须把每个挂载点排除出 Baloo 索引。
//!
//! 本模块只做**纯读取与判定**（不写任何配置）：
//!
//! * [`BalooConfig::load`]：读 baloofilerc（`$XDG_CONFIG_HOME` 优先，缺省
//!   `~/.config` —— 与 balooctl/KConfig 的写入位置对齐）；
//! * [`BalooConfig::is_excluded`]：路径（或其祖先）是否在排除列表里；
//! * [`BalooConfig::would_index`]：Baloo 开着时**会不会真的索引这个路径**
//!   （= 开启 Dolphin 集成的风险判定）；
//! * [`find_balooctl`] / [`find_balooctl_in`]：找 `balooctl6` / `balooctl`
//!   （排除命令由调用方在终端里执行，见 GUI 的 `open_baloo_exclude`）。
//!
//! ## baloofilerc 实测格式（CachyOS / baloo 6.31）
//!
//! ```ini
//! [Basic Settings]
//! Indexing-Enabled=true          ← 缺省时视为 true（本机实测：文件里没有这一行，
//!                                   balooctl6 status 却报「正在运行」）
//!
//! [General]
//! dbVersion=2
//! exclude filters=…              ← 文件名 glob，与目录排除无关
//! exclude folders[$e]=/mnt/a/,/mnt/b/   ← 逗号分隔的绝对路径。balooctl 实写
//!                                   带键标记 [$e] 与尾斜杠（实测）；手写无标记
//!                                   也常见，两种都解析
//! folders=/home/u/docs           ← 非空 = 只索引这些目录（include 列表）
//! ```
//!
//! 语义（对齐 Baloo 自己的判定，宁可保守）：
//!
//! * **排除**：路径本身或任一**祖先**在 `exclude folders` 里 → 不索引；
//!   排除项在路径**内部**（如排除 `/m/sub` 而问 `/m`）→ 根仍会被索引，**不算排除**；
//! * **include 列表非空**：路径与任一 include 项有**交集**（互为祖先）才可能被索引；
//! * **include 列表为空**：Baloo 默认索引整个家目录 → 路径与家目录有交集就可能被索引
//!   （挂载点在家目录**外**且从未登记 → 不索引）；
//! * `Indexing-Enabled=false`：整个索引器关了 → 什么都不会被索引
//!   （代价：Dolphin 的 Tags 列也不显示，那是 KDE 的设计）。

use std::path::{Path, PathBuf};

/// baloofilerc 的路径：KConfig 语义 —— `$XDG_CONFIG_HOME/baloofilerc` 优先
/// （非空且为绝对路径时），缺省 `~/.config/baloofilerc`。
///
/// 必须与 balooctl 对齐：balooctl 写配置走 KConfig，实测（CachyOS /
/// baloo 6.31，`XDG_CONFIG_HOME=/tmp/x balooctl6 config add excludeFolders …`）
/// 写进的是 `$XDG_CONFIG_HOME/baloofilerc`。读侧若写死 `~/.config`，
/// 在设了 `XDG_CONFIG_HOME` 的环境（如 fuse-matrix 为隔离凭据设的
/// `$RUNDIR/config`）里两边看的就不是同一个文件 —— 门禁永远看不到
/// balooctl 刚写入的排除项。
pub fn config_path(home: &Path) -> PathBuf {
    config_path_with(home, std::env::var_os("XDG_CONFIG_HOME").as_deref())
}

/// [`config_path`] 的可单测版本（不碰进程环境）。
pub fn config_path_with(home: &Path, xdg_config_home: Option<&std::ffi::OsStr>) -> PathBuf {
    if let Some(x) = xdg_config_home {
        let p = PathBuf::from(x);
        if p.is_absolute() {
            return p.join("baloofilerc");
        }
    }
    home.join(".config").join("baloofilerc")
}

/// 解析后的 Baloo 配置（只取 Dolphin 集成关心的三样）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BalooConfig {
    /// `[Basic Settings] Indexing-Enabled`；**缺省 = true**（Baloo 默认开）。
    pub indexing_enabled: bool,
    /// `[General] exclude folders`（逗号分隔，已展开 `~` / `$HOME`）。
    pub exclude_folders: Vec<PathBuf>,
    /// `[General] folders`（include 列表；空 = 索引整个家目录）。
    pub include_folders: Vec<PathBuf>,
}

impl BalooConfig {
    /// 读 `~/.config/baloofilerc`。
    ///
    /// 返回 `None` = 文件不存在（Baloo 没装或从没跑过 → 不会索引任何东西）。
    /// 文件存在但解析不出已知键 → 全默认值（`indexing_enabled=true`，两个列表空）。
    pub fn load(home: &Path) -> Option<Self> {
        let p = config_path(home);
        let text = std::fs::read_to_string(p).ok()?;
        Some(Self::parse(&text, home))
    }

    /// 解析 baloofilerc 文本（`home` 用来展开 `~` / `$HOME` 前缀）。
    pub fn parse(text: &str, home: &Path) -> Self {
        let mut cfg = Self {
            indexing_enabled: true, // 缺省 = 开（见模块注释的实测依据）
            exclude_folders: Vec::new(),
            include_folders: Vec::new(),
        };
        let mut section = String::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                section = line[1..line.len() - 1].trim().to_string();
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            // KConfig 键可带扩展标记后缀：balooctl 实写 `exclude folders[$e]=…`
            // （实测 CachyOS / baloo 6.31；`$e` = 值里展开变量）。剥掉 `[...]`
            // 再匹配，否则 balooctl 写入的排除项一个都读不到。
            let key = key.split('[').next().unwrap_or(key).trim();
            let value = value.trim();
            match (section.as_str(), key) {
                ("Basic Settings", "Indexing-Enabled") => {
                    // Baloo 用 KConfig：只有字面 "false" 才算关（"true"/缺省都开）
                    cfg.indexing_enabled = !value.eq_ignore_ascii_case("false");
                }
                ("General", "exclude folders") => {
                    cfg.exclude_folders = split_paths(value, home);
                }
                ("General", "folders") => {
                    cfg.include_folders = split_paths(value, home);
                }
                _ => {}
            }
        }
        cfg
    }

    /// 路径（或其任一祖先）在排除列表里 → 不会被索引。
    ///
    /// 排除项在路径**内部**不算：问 `/m` 而排除的是 `/m/sub` 时，`/m` 根
    /// 仍会被索引（Baloo 按目录粒度排除，不递归向上）。
    pub fn is_excluded(&self, path: &Path) -> bool {
        self.exclude_folders.iter().any(|e| path.starts_with(e))
    }

    /// Baloo 开着时**会不会真的索引这个路径**（= Dolphin 集成的风险判定）。
    ///
    /// 判定链：索引器关了 → 否；被排除 → 否；include 列表非空 → 与任一
    /// include 项有交集才可能；include 空 → 与家目录有交集就可能
    /// （挂载点在家目录外且没登记过 → 否）。「有交集」取**双向祖先**
    /// （include 项在挂载点内部同样会让子树被索引，不能漏）。
    pub fn would_index(&self, path: &Path, home: &Path) -> bool {
        if !self.indexing_enabled {
            return false;
        }
        if self.is_excluded(path) {
            return false;
        }
        if self.include_folders.is_empty() {
            overlaps(path, home)
        } else {
            self.include_folders.iter().any(|f| overlaps(path, f))
        }
    }
}

/// 逗号分隔的路径列表（Baloo 自己就这么存；路径里带逗号属于自找的，不处理）。
/// 顺手展开 `~` / `$HOME` 前缀 —— Baloo 写的是绝对路径，这里只是防御性兼容。
fn split_paths(value: &str, home: &Path) -> Vec<PathBuf> {
    value
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| expand_home(s, home))
        .collect()
}

fn expand_home(s: &str, home: &Path) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        home.join(rest)
    } else if let Some(rest) = s.strip_prefix("$HOME/") {
        home.join(rest)
    } else if s == "~" || s == "$HOME" {
        home.to_path_buf()
    } else {
        PathBuf::from(s)
    }
}

/// 互为祖先（含相等）：`a` 在 `b` 之下，或 `b` 在 `a` 之下。
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// 在 `PATH` 环境变量字符串里找 `balooctl6` / `balooctl`（KF6 优先）。
///
/// 拆出带参版本是为了可单测（不用改进程环境）。
pub fn find_balooctl_in(path_var: &str) -> Option<PathBuf> {
    for name in ["balooctl6", "balooctl"] {
        for dir in path_var.split(':') {
            if dir.is_empty() {
                continue;
            }
            let cand = Path::new(dir).join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// 在当前进程的 `PATH` 里找 `balooctl6` / `balooctl`。
pub fn find_balooctl() -> Option<PathBuf> {
    find_balooctl_in(&std::env::var("PATH").unwrap_or_default())
}

/// 拼一条「把路径排除出 Baloo 索引」的命令（给终端执行 / 展示用）。
///
/// 注意**不执行**：真正跑它的是 GUI 弹出的终端（用户看得见每一条命令），
/// 这里只负责生成文本。
pub fn exclude_cmd(balooctl: &Path, path: &Path) -> String {
    format!(
        "{} config add excludeFolders {}",
        balooctl.display(),
        shell_quote(&path.display().to_string())
    )
}

/// 单引号包裹（内部 `'` 转成 `'\''`）—— 生成的片段能安全拼进 `bash -c` 脚本。
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '/' | '.' | '_' | '-' | '+' | '=' | ':' | '@' | '%')
        })
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/u";

    fn cfg(text: &str) -> BalooConfig {
        BalooConfig::parse(text, Path::new(HOME))
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn missing_indexing_enabled_means_on() {
        // 本机实测形状：没有 [Basic Settings]，balooctl6 status 却在跑
        let c = cfg("[General]\ndbVersion=2\n");
        assert!(c.indexing_enabled);
        // 只有字面 false 才算关
        assert!(!cfg("[Basic Settings]\nIndexing-Enabled=false\n").indexing_enabled);
        assert!(cfg("[Basic Settings]\nIndexing-Enabled=true\n").indexing_enabled);
    }

    #[test]
    fn parses_exclude_and_include_lists() {
        let c = cfg("[General]\nexclude folders=/home/u/a, /home/u/b\nfolders=/home/u/docs\n");
        assert_eq!(c.exclude_folders, vec![p("/home/u/a"), p("/home/u/b")]);
        assert_eq!(c.include_folders, vec![p("/home/u/docs")]);
        // 空值 / 只有逗号 → 空列表（本机实测：excludeFolders: 后面什么都没有）
        let c = cfg("[General]\nexclude folders=\n");
        assert!(c.exclude_folders.is_empty());
    }

    #[test]
    fn tilde_and_home_var_are_expanded() {
        let c = cfg("[General]\nexclude folders=~/mnt,$HOME/mnt2\n");
        assert_eq!(c.exclude_folders, vec![p("/home/u/mnt"), p("/home/u/mnt2")]);
    }

    #[test]
    fn parses_balooctl_written_format() {
        // balooctl6 config add excludeFolders /tmp/x 实际写出的形状
        // （CachyOS / baloo 6.31 实测）：键带 KConfig 扩展标记 [$e]、
        // 值带尾部斜杠、多项逗号分隔。尾斜杠不产生 Path 组件，判定不受影响。
        let c = cfg("[General]\nexclude folders[$e]=/tmp/x/,/home/u/mnt/\nfolders[$e]=/home/u/docs/\n");
        assert_eq!(c.exclude_folders, vec![p("/tmp/x"), p("/home/u/mnt")]);
        assert_eq!(c.include_folders, vec![p("/home/u/docs")]);
        // 手写（无标记）格式照旧可读
        let c = cfg("[General]\nexclude folders=/tmp/x\n");
        assert_eq!(c.exclude_folders, vec![p("/tmp/x")]);
    }

    #[test]
    fn config_path_respects_xdg_config_home() {
        use std::ffi::OsStr;
        let home = Path::new("/home/u");
        // 未设 → ~/.config（真实用户环境的常态）
        assert_eq!(
            config_path_with(home, None),
            p("/home/u/.config/baloofilerc")
        );
        // 设了且绝对 → $XDG_CONFIG_HOME 下（balooctl/KConfig 的写入位置）
        assert_eq!(
            config_path_with(home, Some(OsStr::new("/run/isolated/config"))),
            p("/run/isolated/config/baloofilerc")
        );
        // 相对路径 / 空串 → KConfig 规定忽略，回落 ~/.config
        assert_eq!(
            config_path_with(home, Some(OsStr::new(""))),
            p("/home/u/.config/baloofilerc")
        );
        assert_eq!(
            config_path_with(home, Some(OsStr::new("relative/paths"))),
            p("/home/u/.config/baloofilerc")
        );
    }

    #[test]
    fn exclusion_covers_self_and_descendants_not_inside() {
        let c = BalooConfig {
            indexing_enabled: true,
            exclude_folders: vec![p("/home/u/mnt")],
            include_folders: vec![],
        };
        assert!(c.is_excluded(&p("/home/u/mnt")));
        assert!(c.is_excluded(&p("/home/u/mnt/sub/deep")));
        // 排除项在路径内部：根没被排除
        assert!(!c.is_excluded(&p("/home/u")));
        assert!(!c.is_excluded(&p("/home/u/other")));
    }

    #[test]
    fn would_index_respects_disabled_and_excluded() {
        let mut c = BalooConfig {
            indexing_enabled: true,
            exclude_folders: vec![p("/home/u/mnt")],
            include_folders: vec![],
        };
        // 家目录下的挂载点：默认会被索引（危险）
        assert!(c.would_index(&p("/home/u/mnt2"), Path::new(HOME)));
        // 排除了 → 安全
        assert!(!c.would_index(&p("/home/u/mnt"), Path::new(HOME)));
        // 索引器整体关了 → 全部安全
        c.indexing_enabled = false;
        assert!(!c.would_index(&p("/home/u/mnt2"), Path::new(HOME)));
    }

    #[test]
    fn would_index_outside_home_is_not_indexed_by_default() {
        let c = BalooConfig {
            indexing_enabled: true,
            exclude_folders: vec![],
            include_folders: vec![],
        };
        // /mnt/q 不在家目录里、也没登记进 include → Baloo 不会碰它
        assert!(!c.would_index(&p("/mnt/q"), Path::new(HOME)));
        assert!(c.would_index(&p("/home/u/q"), Path::new(HOME)));
    }

    #[test]
    fn include_list_gates_indexing_both_directions() {
        let c = BalooConfig {
            indexing_enabled: true,
            exclude_folders: vec![],
            include_folders: vec![p("/home/u/docs")],
        };
        // 挂载点在 include 项之下 → 索引
        assert!(c.would_index(&p("/home/u/docs/mnt"), Path::new(HOME)));
        // include 项在挂载点内部（子树会被索引）→ 也算危险，不能漏
        assert!(c.would_index(&p("/home/u"), Path::new(HOME)));
        // 完全不相干 → 安全
        assert!(!c.would_index(&p("/home/u/other"), Path::new(HOME)));
    }

    #[test]
    fn find_balooctl_prefers_kf6() {
        let tmp = std::env::temp_dir().join(format!("qxync-baloo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let bin = tmp.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path_var = format!("{}", bin.display());
        // 什么都没有 → None
        assert!(find_balooctl_in(&path_var).is_none());
        // 只有老 balooctl → 用它
        std::fs::write(bin.join("balooctl"), b"#!/bin/sh\n").unwrap();
        assert_eq!(find_balooctl_in(&path_var), Some(bin.join("balooctl")));
        // 两个都有 → balooctl6 优先
        std::fs::write(bin.join("balooctl6"), b"#!/bin/sh\n").unwrap();
        assert_eq!(find_balooctl_in(&path_var), Some(bin.join("balooctl6")));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn shell_quote_only_quotes_when_needed() {
        assert_eq!(shell_quote("/mnt/q"), "/mnt/q");
        assert_eq!(shell_quote("/mnt/my dir"), "'/mnt/my dir'");
        assert_eq!(shell_quote("/mnt/it's"), "'/mnt/it'\\''s'");
    }

    #[test]
    fn exclude_cmd_is_well_formed() {
        let cmd = exclude_cmd(Path::new("/usr/bin/balooctl6"), &p("/mnt/my dir"));
        assert_eq!(
            cmd,
            "/usr/bin/balooctl6 config add excludeFolders '/mnt/my dir'"
        );
    }
}
