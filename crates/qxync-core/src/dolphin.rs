//! ★ M25：Dolphin 角标插件（`qxync-dolphin-overlay`）的安装检测
//!
//! 架构（M24，见 `docs/M23-Dolphin集成.md` §8）：主程序是状态的**源头** ——
//! daemon/FUSE 按开关暴露 `user.qxync.overlay` xattr；插件只是**渲染器** ——
//! `KOverlayIconPlugin` 把 xattr 翻译成图标名，Dolphin 画在文件图标角上。
//! 渲染器是**可选**且**发行版绑定**的（KF6 的 C++ 插件，不随 Rust 主程序发布），
//! 所以主程序要能回答「角标到底会不会显示」，而不是让开关悄悄变成无效操作：
//!
//! * `qxync baloo-status` / GUI 设置页显示插件状态
//!   （已装 / 未装 / 装了但不会被加载）；
//! * 未装时角标开关**仍可打开**（xattr 无害，只是没人画），状态行如实说明。
//!
//! ## 检测规则（与 Qt 的插件查找对齐）
//!
//! Dolphin（经 KIO）加载 `kf6/overlayicon/` 下的插件，搜索路径 = Qt 编译期
//! 默认目录 + `QT_PLUGIN_PATH`。本模块按同样顺序找 `libqxync_overlay.so`：
//!
//! 1. `QT_PLUGIN_PATH` 的每个条目（用户级安装**只有**这样才能生效）；
//! 2. 系统目录：`/usr/lib/qt6/plugins`（Arch）、`/usr/lib64/qt6/plugins`
//!    （openSUSE 系）、`/usr/lib/<triplet>/qt6/plugins`（Debian/Ubuntu multiarch）；
//! 3. `~/.local/lib/qt6/plugins` —— 找到也只算「装了但**不会**被加载」：
//!    Qt 不默认扫家目录下的插件目录，必须设 `QT_PLUGIN_PATH` 才认
//!    （走到第 3 步说明没设、或设了但没包含它）。
//!
//! Tags 列（`user.xdg.tags`）**不需要**任何插件 —— 那是 Dolphin 内置列，
//! 直接读 xattr，所以本模块只关心角标渲染器。

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// 检测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayPlugin {
    /// 找到的 `libqxync_overlay.so`。
    pub path: PathBuf,
    /// Dolphin 真的会加载它（系统插件目录或 `QT_PLUGIN_PATH` 命中）。
    ///
    /// `false` = 装在 `~/.local` 但 `QT_PLUGIN_PATH` 没包含该目录 ——
    /// Dolphin 不会加载，角标不会显示（建议改用系统级安装）。
    pub effective: bool,
}

/// 插件文件名（Qt 在 Linux 上按这个名字加载 QPluginLoader 目标）。
const SO_NAME: &str = "libqxync_overlay.so";

/// 系统级 Qt6 插件目录（发行版差异全覆盖；存在即 effective）。
const SYSTEM_PLUGIN_DIRS: &[&str] = &[
    // Arch / CachyOS（qt6-base 装到 /usr/lib/qt6/plugins）
    "/usr/lib/qt6/plugins",
    // openSUSE 系（lib64）
    "/usr/lib64/qt6/plugins",
    // Debian / Ubuntu multiarch
    "/usr/lib/x86_64-linux-gnu/qt6/plugins",
];

/// 检测本机是否装了角标插件（读 `HOME` / `QT_PLUGIN_PATH` 环境变量）。
pub fn detect_overlay_plugin() -> Option<OverlayPlugin> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    detect_overlay_plugin_with(&home, std::env::var_os("QT_PLUGIN_PATH").as_deref())
}

/// [`detect_overlay_plugin`] 的可单测版本（不碰进程环境）。
pub fn detect_overlay_plugin_with(
    home: &Path,
    qt_plugin_path: Option<&OsStr>,
) -> Option<OverlayPlugin> {
    let system: Vec<PathBuf> = SYSTEM_PLUGIN_DIRS.iter().map(PathBuf::from).collect();
    detect_overlay_plugin_impl(home, qt_plugin_path, &system)
}

/// 真正的实现（系统目录可注入，单测不用碰 `/usr`）。
fn detect_overlay_plugin_impl(
    home: &Path,
    qt_plugin_path: Option<&OsStr>,
    system_dirs: &[PathBuf],
) -> Option<OverlayPlugin> {
    // 1. QT_PLUGIN_PATH（冒号分隔；Qt 允许空条目，split_paths 会给出 ""，join 后不是文件，自然跳过）
    if let Some(v) = qt_plugin_path {
        for entry in std::env::split_paths(v) {
            let cand = entry.join("kf6").join("overlayicon").join(SO_NAME);
            if cand.is_file() {
                return Some(OverlayPlugin {
                    path: cand,
                    effective: true,
                });
            }
        }
    }
    // 2. 系统插件目录（发行版包 / `sudo cmake --install --prefix /usr` 装到这里）
    for dir in system_dirs {
        let cand = dir.join("kf6").join("overlayicon").join(SO_NAME);
        if cand.is_file() {
            return Some(OverlayPlugin {
                path: cand,
                effective: true,
            });
        }
    }
    // 3. 用户级 ~/.local —— 只有 QT_PLUGIN_PATH 包含它才会被加载；
    //    走到这里说明第 1 步没命中（没设 / 设了但没包含），如实报「不会加载」。
    let cand = home
        .join(".local/lib/qt6/plugins/kf6/overlayicon")
        .join(SO_NAME);
    if cand.is_file() {
        return Some(OverlayPlugin {
            path: cand,
            effective: false,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在临时目录里造一个假的 `libqxync_overlay.so`，返回其完整路径。
    fn touch_so(root: &Path, sub: &str) -> PathBuf {
        let dir = root.join(sub).join("kf6").join("overlayicon");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(SO_NAME);
        std::fs::write(&p, b"fake so").unwrap();
        p
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("qxync-dolphin-{}-{}", tag, std::process::id()));
        std::fs::remove_dir_all(&d).ok();
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn absent_when_nowhere_installed() {
        let d = tmp("absent");
        assert_eq!(detect_overlay_plugin_impl(&d, None, &[d.join("usr")]), None);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn qt_plugin_path_hit_is_effective() {
        let d = tmp("qtpp");
        let so = touch_so(&d, "custom/plugins");
        let v = std::env::join_paths([&d.join("custom/plugins")]).unwrap();
        let got = detect_overlay_plugin_impl(&d, Some(v.as_os_str()), &[d.join("usr")]);
        assert_eq!(
            got,
            Some(OverlayPlugin {
                path: so,
                effective: true
            })
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn system_dir_hit_is_effective() {
        let d = tmp("system");
        let so = touch_so(&d, "usr/lib/qt6/plugins");
        let got = detect_overlay_plugin_impl(&d, None, &[d.join("usr/lib/qt6/plugins")]);
        assert_eq!(
            got,
            Some(OverlayPlugin {
                path: so,
                effective: true
            })
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn user_local_without_qt_plugin_path_is_ineffective() {
        let d = tmp("local");
        // ~/.local 里有，但 QT_PLUGIN_PATH 没设 → 装了但 Dolphin 不会加载
        let so = touch_so(&d, ".local/lib/qt6/plugins");
        let got = detect_overlay_plugin_impl(&d, None, &[d.join("usr")]);
        assert_eq!(
            got,
            Some(OverlayPlugin {
                path: so,
                effective: false
            })
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn user_local_with_qt_plugin_path_is_effective() {
        let d = tmp("local2");
        // 同一个 ~/.local，但 QT_PLUGIN_PATH 包含它 → 会加载
        let so = touch_so(&d, ".local/lib/qt6/plugins");
        let v = std::env::join_paths([&d.join(".local/lib/qt6/plugins")]).unwrap();
        let got = detect_overlay_plugin_impl(&d, Some(v.as_os_str()), &[d.join("usr")]);
        assert_eq!(
            got,
            Some(OverlayPlugin {
                path: so,
                effective: true
            })
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn qt_plugin_path_takes_precedence_over_user_local() {
        let d = tmp("precedence");
        touch_so(&d, ".local/lib/qt6/plugins");
        let so = touch_so(&d, "custom/plugins");
        let v = std::env::join_paths([&d.join("custom/plugins")]).unwrap();
        let got = detect_overlay_plugin_impl(&d, Some(v.as_os_str()), &[d.join("usr")]);
        // 两处都装了：QT_PLUGIN_PATH 先命中，且是 effective
        assert_eq!(
            got,
            Some(OverlayPlugin {
                path: so,
                effective: true
            })
        );
        std::fs::remove_dir_all(&d).ok();
    }
}
