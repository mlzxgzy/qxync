//! # M8.4：全局设置（`settings.json`）
//!
//! 在 M8.4 之前，所有「设置」都挤在 **每个 link** 的 `links/<id>.json` 里
//! （连接参数 + 规则），而 Qsync 6 的设置是**全局四分区**：代理 / 个人 / 高级 / 释放空间。
//! 本模块提供那份全局设置：
//!
//! ```text
//! ~/.config/qxync/settings.json          全局设置（0600，可能含代路口令）
//! ~/.config/autostart/qxync.desktop      开机自启（XDG autostart 规范）
//! ```
//!
//! 设计约束（与 `tasks.rs` 同一套）：
//!
//! * **字段全部 `#[serde(default)]`** —— 旧文件缺字段能读、新字段不丢，加设置不用写迁移；
//! * **写盘是「临时文件 + rename」且 0600** —— 不出现半截文件，口令不落到组/他人可读的权限；
//! * **默认值 = 现在的行为** —— 没有 `settings.json` 时，代理=无、不自动释放、不开机自启，
//!   于是 M1–M8.3 的行为一字不变（验收矩阵的前提）。

use crate::config::{restrict_perms, ConfigPaths};
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- 代理

/// 不使用代理。**注意**：reqwest 默认会自动读取 `http_proxy` 环境变量，
/// 所以「无代理」必须显式 `no_proxy()`，否则用户会以为关掉了其实没关。
pub const PROXY_NONE: &str = "none";
/// 自动检测（**默认**）：跟随环境变量
/// （`http_proxy` / `https_proxy` / `all_proxy` / `no_proxy`）。
///
/// 默认选它有两个理由：① Qsync 官方也是把 Auto-detect 当推荐值；
/// ② reqwest 本来就会读这些环境变量，选它 = **M1–M8.3 的行为一字不变**。
pub const PROXY_AUTO: &str = "auto";
/// 手动指定服务器 + 端口（可选认证）。
pub const PROXY_MANUAL: &str = "manual";
pub const PROXY_MODES: [&str; 3] = [PROXY_NONE, PROXY_AUTO, PROXY_MANUAL];

fn default_proxy_mode() -> String {
    PROXY_AUTO.to_string()
}

/// 代理设置（对齐 Qsync 设置页「代理」tab）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxySettings {
    /// `none` / `auto` / `manual`。
    #[serde(default = "default_proxy_mode")]
    pub mode: String,
    /// 手动模式：服务器 IP 或 URL（`proxy.corp` / `http://proxy.corp` / `socks5://…`）。
    #[serde(default)]
    pub server: String,
    /// 手动模式端口；服务器 URL 里已带端口时可以留空。
    #[serde(default)]
    pub port: Option<u16>,
    /// `Proxy server requires a password`。
    #[serde(default)]
    pub auth: bool,
    #[serde(default)]
    pub user: String,
    /// ⚠ 明文存 `settings.json`（与 `credentials.json` 同级别：0600，目录 0700）。
    #[serde(default)]
    pub password: String,
}

impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            mode: default_proxy_mode(),
            server: String::new(),
            port: None,
            auth: false,
            user: String::new(),
            password: String::new(),
        }
    }
}

/// 解析后的代理规格（`qxync-client` 按它配置 `reqwest`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxySpec {
    /// 显式不用代理（`Client::builder().no_proxy()`）。
    None,
    /// 跟随环境变量（reqwest 默认行为）。
    Auto,
    /// 手动代理；`auth` 为 `Some((user, password))` 时带上 Basic 认证。
    Manual {
        url: String,
        auth: Option<(String, String)>,
    },
}

impl ProxySettings {
    /// 归一化（宽松：非法 mode 落回默认；`auth=false` 时清掉用户名口令，避免脏数据）。
    pub fn normalize(&mut self) {
        if !PROXY_MODES.contains(&self.mode.as_str()) {
            self.mode = default_proxy_mode();
        }
        self.server = self.server.trim().to_string();
        self.user = self.user.trim().to_string();
        // 不变量：只有「手动 + 需要认证」才保留认证信息。
        // 服务器/端口**不清**：清了会让 `--set proxy.server=… --set proxy.mode=manual`
        // 这类顺序敏感的操作莫名其妙丢值（值本身也不敏感）。
        if self.mode != PROXY_MANUAL {
            self.auth = false;
        }
        if !self.auth {
            self.user.clear();
            self.password.clear();
        }
    }

    /// 手动模式的代理 URL（`scheme://host:port`；没写 scheme 时补 `http://`）。
    pub fn manual_url(&self) -> Option<String> {
        let s = self.server.trim();
        if s.is_empty() {
            return None;
        }
        let base = if s.contains("://") {
            s.trim_end_matches('/').to_string()
        } else {
            format!("http://{}", s.trim_end_matches('/'))
        };
        match self.port {
            // URL 里已经写了端口就不再追加
            Some(p) if !has_explicit_port(&base) => Some(format!("{base}:{p}")),
            _ => Some(base),
        }
    }

    /// 解析成 [`ProxySpec`]；手动模式缺服务器 → 报错（**不静默退回无代理**，
    /// 否则用户以为在用代理、实际直连，这是最危险的一种「静默」）。
    pub fn resolve(&self) -> Result<ProxySpec> {
        match self.mode.as_str() {
            PROXY_NONE => Ok(ProxySpec::None),
            PROXY_AUTO => Ok(ProxySpec::Auto),
            PROXY_MANUAL => {
                let url = self.manual_url().ok_or_else(|| {
                    Error::Io("代理模式为 manual，但服务器为空（应填 IP 或 URL）".into())
                })?;
                let auth = if self.auth {
                    if self.user.is_empty() {
                        return Err(Error::Io("勾选了「代理服务器需要口令」但用户名为空".into()));
                    }
                    Some((self.user.clone(), self.password.clone()))
                } else {
                    None
                };
                Ok(ProxySpec::Manual { url, auth })
            }
            // normalize() 之后不该到达；真到了也按默认处理
            _ => Ok(ProxySpec::Auto),
        }
    }
}

/// URL 里是否已经带端口（`host:8080` / `[::1]:8080`）。只看最后一个 `:` 之后是不是纯数字。
fn has_explicit_port(url: &str) -> bool {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let hostport = after_scheme.split('/').next().unwrap_or("");
    match hostport.rsplit_once(':') {
        Some((_, p)) => !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

// ---------------------------------------------------------------- 释放空间

/// `Free up space automatically` 的触发条件。
pub const FREE_BELOW_PCT: &str = "below_pct";
/// `By frequency`。
pub const FREE_FREQUENCY: &str = "frequency";
pub const FREE_MODES: [&str; 2] = [FREE_BELOW_PCT, FREE_FREQUENCY];

fn default_free_mode() -> String {
    FREE_BELOW_PCT.to_string()
}
fn default_below_pct() -> u8 {
    10
}
fn default_every_hours() -> u64 {
    24
}

/// 释放空间设置（对齐 Qsync 设置页「释放空间」tab）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FreeSpaceSettings {
    /// `Free up space automatically` 总开关（默认关 = 现有行为）。
    #[serde(default)]
    pub auto: bool,
    /// `below_pct`（当空间少于 X%）/ `frequency`（按频率）。
    #[serde(default = "default_free_mode")]
    pub mode: String,
    /// `When space is less than [10]%`。
    #[serde(default = "default_below_pct")]
    pub below_pct: u8,
    /// `By frequency`：每 N 小时跑一次。
    #[serde(default = "default_every_hours")]
    pub every_hours: u64,
}

impl Default for FreeSpaceSettings {
    fn default() -> Self {
        Self {
            auto: false,
            mode: default_free_mode(),
            below_pct: default_below_pct(),
            every_hours: default_every_hours(),
        }
    }
}

impl FreeSpaceSettings {
    pub fn normalize(&mut self) {
        if !FREE_MODES.contains(&self.mode.as_str()) {
            self.mode = default_free_mode();
        }
        // 1..=99：0 会让「少于 0%」永不触发，100 会永远触发 —— 都不是用户想要的
        self.below_pct = self.below_pct.clamp(1, 99);
        self.every_hours = self.every_hours.clamp(1, 24 * 30);
    }
}

// ---------------------------------------------------------------- 设置本体

fn yes() -> bool {
    true
}

/// 全局设置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// schema 版本（预留给以后的迁移；现在恒为 1）。
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub proxy: ProxySettings,
    /// `Launch Qsync at startup` —— 记录开关状态；
    /// 真正写 `~/.config/autostart/qxync.desktop` 的是 [`Settings::apply_autostart`]。
    #[serde(default)]
    pub launch_at_startup: bool,
    /// `Show desktop notifications`（默认开：同步出错要让人看见）。
    #[serde(default = "yes")]
    pub desktop_notifications: bool,
    /// `Enable debug log`（⚠ 影响性能；只影响 daemon/GUI 的日志级别）。
    #[serde(default)]
    pub debug_log: bool,
    /// 语言（`""` = 跟随系统；本轮只记录，文案仍只有 zh-CN）。
    #[serde(default)]
    pub language: String,
    /// 地区（`Select the correct region to ensure better connectivity`；只记录）。
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub free_space: FreeSpaceSettings,
    /// ★ M8.4：托盘行为 —— 关闭主窗口时最小化到托盘（默认开）。
    #[serde(default = "yes")]
    pub close_to_tray: bool,
}

fn one() -> u32 {
    1
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            proxy: ProxySettings::default(),
            launch_at_startup: false,
            desktop_notifications: true,
            debug_log: false,
            language: String::new(),
            region: String::new(),
            free_space: FreeSpaceSettings::default(),
            close_to_tray: true,
        }
    }
}

impl Settings {
    pub fn file(paths: &ConfigPaths) -> PathBuf {
        paths.config_dir.join("settings.json")
    }

    /// XDG autostart 目录里的桌面项（`$XDG_CONFIG_HOME/autostart/qxync.desktop`）。
    pub fn autostart_file(paths: &ConfigPaths) -> PathBuf {
        Self::autostart_dir(paths).join("qxync.desktop")
    }

    /// ★ 0.2.0：改名前的老桌面项（`autostart/qsync.desktop`）。
    ///
    /// 0.1.x 写下的那个文件里的 `Exec=` 指向 `qxync-gui`（**名字没变**），所以它其实
    /// 还能用，只是 `Name=` 是旧名。这里留着只为「切换开关时顺手清掉」，不做静默删除。
    fn legacy_autostart_file(paths: &ConfigPaths) -> PathBuf {
        Self::autostart_dir(paths).join("qsync.desktop")
    }

    fn autostart_dir(paths: &ConfigPaths) -> PathBuf {
        let base = paths
            .config_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| paths.config_dir.clone());
        base.join("autostart")
    }

    pub fn normalize(&mut self) {
        self.proxy.normalize();
        self.free_space.normalize();
        if !matches!(self.version, 1..=1) {
            self.version = 1;
        }
    }

    /// 读设置；**文件不存在 = 全默认**（不是错误）。
    pub fn load(paths: &ConfigPaths) -> Result<Self> {
        let p = Self::file(paths);
        if !p.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read(&p).map_err(|e| Error::Io(format!("读取 {}: {e}", p.display())))?;
        let mut s: Self = serde_json::from_slice(&raw)
            .map_err(|e| Error::Parse(format!("解析 {}: {e}", p.display())))?;
        s.normalize();
        Ok(s)
    }

    /// 原子写 + 0600（先写 `.tmp` 再 rename）。
    pub fn save(&self, paths: &ConfigPaths) -> Result<PathBuf> {
        let mut s = self.clone();
        s.normalize();
        paths.ensure_dirs()?;
        let target = Self::file(paths);
        let tmp = target.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(&s)?;
        {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&tmp)?;
            restrict_perms(&tmp, 0o600)?;
            f.write_all(&body)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &target)?;
        Ok(target)
    }

    /// 生成 autostart 桌面项内容。
    ///
    /// `exe` 必须是**要自启的可执行文件**：GUI 自启时是 `qxync-gui` 的绝对路径。
    /// 这里刻意不自己 `current_exe()` —— daemon 调用时会写成 `qxyncd`，那是错的。
    pub fn desktop_entry(exe: &Path, comment: &str) -> String {
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=qxync\n\
             Comment={comment}\n\
             Exec={exe}\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n",
            exe = exe.display()
        )
    }

    /// 按 `launch_at_startup` 落盘/删除 autostart 项。返回实际路径。
    ///
    /// ⚠ 只改这一个桌面项文件，**不动任何同步状态**。
    /// ★ 0.2.0：无论开还是关，都顺手清掉改名前留下的 `qsync.desktop`
    /// （开着的时候留着它，用户会在菜单里看到两个自启项）。
    pub fn apply_autostart(&self, paths: &ConfigPaths, exe: &Path) -> Result<PathBuf> {
        let target = Self::autostart_file(paths);
        if self.launch_at_startup {
            if let Some(dir) = target.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let body = Self::desktop_entry(exe, "QNAP Qsync 按需同步（qxync）");
            let tmp = target.with_extension("desktop.tmp");
            std::fs::write(&tmp, body)?;
            std::fs::rename(&tmp, &target)?;
        } else if target.exists() {
            std::fs::remove_file(&target)?;
        }
        let legacy = Self::legacy_autostart_file(paths);
        if legacy.exists() {
            std::fs::remove_file(&legacy)?;
        }
        Ok(target)
    }

    /// autostart 项此刻是否真的存在（用来在 UI 里显示「已生效 / 未生效」）。
    ///
    /// ★ 0.2.0：老的 `qsync.desktop` 也算「已生效」—— 它确实还在开机拉起 GUI，
    /// 只是名字是旧名。下一次切换开关时会被 [`Self::apply_autostart`] 清掉。
    pub fn autostart_present(paths: &ConfigPaths) -> bool {
        Self::autostart_file(paths).exists() || Self::legacy_autostart_file(paths).exists()
    }

    /// 点号键赋值（CLI `qxync settings --set key=value`）。
    ///
    /// **未知键直接报错**，不静默忽略 —— 脚本里拼错一个键却「看起来成功了」，
    /// 比报错危险得多。布尔值接受 `true/false/1/0/on/off/yes/no`。
    pub fn set_kv(&mut self, key: &str, value: &str) -> Result<()> {
        let v = value.trim();
        let b = |s: &str| -> Result<bool> {
            match s.to_ascii_lowercase().as_str() {
                "true" | "1" | "on" | "yes" | "y" => Ok(true),
                "false" | "0" | "off" | "no" | "n" => Ok(false),
                other => Err(Error::Io(format!("{key} 需要布尔值，收到 {other:?}"))),
            }
        };
        match key {
            "proxy.mode" => {
                if !PROXY_MODES.contains(&v) {
                    return Err(Error::Io(format!(
                        "proxy.mode 只能是 {}，收到 {v:?}",
                        PROXY_MODES.join(" / ")
                    )));
                }
                self.proxy.mode = v.to_string();
            }
            "proxy.server" => self.proxy.server = v.to_string(),
            "proxy.port" => {
                self.proxy.port = if v.is_empty() || v.eq_ignore_ascii_case("none") {
                    None
                } else {
                    Some(
                        v.parse()
                            .map_err(|_| Error::Io(format!("proxy.port 不是端口号: {v:?}")))?,
                    )
                }
            }
            "proxy.auth" => self.proxy.auth = b(v)?,
            "proxy.user" => self.proxy.user = v.to_string(),
            "proxy.password" => self.proxy.password = v.to_string(),
            "free.auto" => self.free_space.auto = b(v)?,
            "free.mode" => {
                if !FREE_MODES.contains(&v) {
                    return Err(Error::Io(format!(
                        "free.mode 只能是 {}，收到 {v:?}",
                        FREE_MODES.join(" / ")
                    )));
                }
                self.free_space.mode = v.to_string();
            }
            "free.below_pct" => {
                self.free_space.below_pct = v
                    .parse()
                    .map_err(|_| Error::Io(format!("free.below_pct 不是数字: {v:?}")))?
            }
            "free.every_hours" => {
                self.free_space.every_hours = v
                    .parse()
                    .map_err(|_| Error::Io(format!("free.every_hours 不是数字: {v:?}")))?
            }
            "startup" => self.launch_at_startup = b(v)?,
            "notifications" => self.desktop_notifications = b(v)?,
            "debug_log" => self.debug_log = b(v)?,
            "close_to_tray" => self.close_to_tray = b(v)?,
            "language" => self.language = v.to_string(),
            "region" => self.region = v.to_string(),
            other => {
                return Err(Error::Io(format!(
                    "未知设置键 {other:?}（可用：proxy.mode/server/port/auth/user/password、\
                     free.auto/mode/below_pct/every_hours、startup、notifications、debug_log、\
                     close_to_tray、language、region）"
                )))
            }
        }
        // 刻意**不在这里 normalize**：多键赋值是一次批处理，
        // 中途归一化会让「先 server 后 mode=manual」这种顺序丢值。
        // 调用方（CLI/GUI）在 save() 时统一归一化 + 校验。
        Ok(())
    }

    /// 当前环境里的代理相关环境变量（「自动检测」实际会读到什么，UI 要如实显示）。
    pub fn proxy_env() -> std::collections::BTreeMap<String, String> {
        let mut out = std::collections::BTreeMap::new();
        for k in [
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
        ] {
            if let Ok(v) = std::env::var(k) {
                if !v.is_empty() {
                    out.insert(k.to_string(), v);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_for(tag: &str) -> (ConfigPaths, PathBuf) {
        let dir = std::env::temp_dir().join(format!("qxync-settings-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let paths = ConfigPaths {
            config_dir: dir.join("config").join("qxync"),
            data_dir: dir.join("data").join("qxync"),
            state_dir: dir.join("state").join("qxync"),
        };
        paths.ensure_dirs().unwrap();
        (paths, dir)
    }

    #[test]
    fn missing_file_means_defaults() {
        let (paths, dir) = paths_for("default");
        let s = Settings::load(&paths).unwrap();
        assert_eq!(
            s.proxy.mode, PROXY_AUTO,
            "默认 = 自动检测（= reqwest 既有行为）"
        );
        assert!(!s.launch_at_startup);
        assert!(s.desktop_notifications, "桌面通知默认开");
        assert!(!s.free_space.auto, "自动释放默认关（= M8.3 行为）");
        assert_eq!(s.free_space.below_pct, 10);
        assert_eq!(s.proxy.resolve().unwrap(), ProxySpec::Auto);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn roundtrip_is_0600_and_atomic() {
        let (paths, dir) = paths_for("roundtrip");
        let mut s = Settings::default();
        s.proxy = ProxySettings {
            mode: PROXY_MANUAL.into(),
            server: "proxy.corp".into(),
            port: Some(3128),
            auth: true,
            user: "u".into(),
            password: "p".into(),
        };
        s.free_space = FreeSpaceSettings {
            auto: true,
            mode: FREE_BELOW_PCT.into(),
            below_pct: 5,
            every_hours: 6,
        };
        let p = s.save(&paths).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "settings.json 可能含代路口令 → 必须 0600");
        }
        let back = Settings::load(&paths).unwrap();
        assert_eq!(back, s);
        assert_eq!(
            back.proxy.resolve().unwrap(),
            ProxySpec::Manual {
                url: "http://proxy.corp:3128".into(),
                auth: Some(("u".into(), "p".into())),
            }
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 旧文件（没有 M8.4 字段）必须能读，且新字段取默认值。
    #[test]
    fn old_settings_file_gets_new_defaults() {
        let (paths, dir) = paths_for("old");
        std::fs::write(Settings::file(&paths), br#"{"proxy":{"mode":"none"}}"#).unwrap();
        let s = Settings::load(&paths).unwrap();
        assert!(s.desktop_notifications);
        assert!(s.close_to_tray);
        assert!(!s.free_space.auto);
        assert_eq!(s.version, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn manual_without_server_is_an_error_not_silent_direct() {
        let p = ProxySettings {
            mode: PROXY_MANUAL.into(),
            ..Default::default()
        };
        let e = p.resolve().unwrap_err().to_string();
        assert!(e.contains("服务器为空"), "实际: {e}");

        let p = ProxySettings {
            mode: PROXY_MANUAL.into(),
            server: "http://proxy:8080".into(),
            port: Some(9999), // URL 已带端口 → 不重复追加
            auth: false,
            user: String::new(),
            password: String::new(),
        };
        assert_eq!(
            p.resolve().unwrap(),
            ProxySpec::Manual {
                url: "http://proxy:8080".into(),
                auth: None
            }
        );

        // 勾了「需要口令」却没填用户名 → 报错，不静默匿名连
        let p = ProxySettings {
            mode: PROXY_MANUAL.into(),
            server: "proxy".into(),
            port: Some(3128),
            auth: true,
            user: String::new(),
            password: "x".into(),
        };
        assert!(p.resolve().unwrap_err().to_string().contains("用户名为空"));
    }

    #[test]
    fn normalize_clears_stale_proxy_fields() {
        let mut p = ProxySettings {
            mode: PROXY_NONE.into(),
            server: "old.proxy".into(),
            port: Some(8080),
            auth: true,
            user: "u".into(),
            password: "p".into(),
        };
        p.normalize();
        // 服务器/端口保留（顺序无关），但**口令必须清掉**（已经切回无代理了）
        assert_eq!(p.server, "old.proxy");
        assert_eq!(p.port, Some(8080));
        assert!(p.user.is_empty() && p.password.is_empty());

        let mut fs = FreeSpaceSettings {
            auto: true,
            mode: "nonsense".into(),
            below_pct: 0,
            every_hours: 0,
        };
        fs.normalize();
        assert_eq!(fs.mode, FREE_BELOW_PCT);
        assert_eq!(fs.below_pct, 1);
        assert_eq!(fs.every_hours, 1);
        assert!(fs.auto, "normalize 不该动总开关");
    }

    #[test]
    fn autostart_written_and_removed() {
        let (paths, dir) = paths_for("autostart");
        let t = Settings::autostart_file(&paths);
        assert!(
            t.ends_with("autostart/qxync.desktop"),
            "路径: {}",
            t.display()
        );
        assert!(!Settings::autostart_present(&paths));

        let mut s = Settings::default();
        s.launch_at_startup = true;
        let p = s
            .apply_autostart(&paths, Path::new("/usr/bin/qxync-gui"))
            .unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.contains("Exec=/usr/bin/qxync-gui"));
        assert!(body.contains("Type=Application"));
        assert!(Settings::autostart_present(&paths));

        s.launch_at_startup = false;
        s.apply_autostart(&paths, Path::new("/usr/bin/qxync-gui"))
            .unwrap();
        assert!(!t.exists(), "关掉自启要把桌面项删掉");

        // ★ 0.2.0：改名前留下的 `autostart/qsync.desktop` 也要被顺手清掉，
        // 而且在那之前要算作「已生效」（它确实还在开机拉起 GUI）。
        let legacy = Settings::legacy_autostart_file(&paths);
        std::fs::write(&legacy, "老的桌面项").unwrap();
        assert!(Settings::autostart_present(&paths), "旧名桌面项也算已生效");

        s.launch_at_startup = true;
        s.apply_autostart(&paths, Path::new("/usr/bin/qxync-gui"))
            .unwrap();
        assert!(!legacy.exists(), "开自启时要把旧名桌面项清掉");
        assert!(t.exists());

        s.launch_at_startup = false;
        s.apply_autostart(&paths, Path::new("/usr/bin/qxync-gui"))
            .unwrap();
        assert!(!t.exists() && !legacy.exists());

        std::fs::remove_dir_all(&dir).ok();
    }
}
