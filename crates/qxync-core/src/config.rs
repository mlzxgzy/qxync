//! 本地配置布局（照报告 09 §9.6 蓝本）。
//!
//! ```text
//! ~/.config/qsync/config.json             全局设置
//! ~/.config/qsync/links/<link_id>.json    NAS 连接
//! ~/.config/qsync/credentials.json        口令（0600，永不出现在 --password 之外的输出里）
//! ~/.local/share/qsync/sync.db            SQLite（baseline/游标/队列，M1 起用）
//! ~/.local/state/qsync/log/               日志
//! ```
//! 目录前缀遵循 `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_STATE_HOME`。

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ConfigPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Error::Io("环境变量 HOME 未设置".into()))
}

fn xdg(var: &str, fallback: PathBuf) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or(fallback)
}

impl ConfigPaths {
    /// 按 XDG 约定推导（不创建目录）。
    pub fn discover() -> Result<Self> {
        let h = home()?;
        Ok(Self {
            config_dir: xdg("XDG_CONFIG_HOME", h.join(".config")).join("qsync"),
            data_dir: xdg("XDG_DATA_HOME", h.join(".local/share")).join("qsync"),
            state_dir: xdg("XDG_STATE_HOME", h.join(".local/state")).join("qsync"),
        })
    }

    pub fn link_file(&self, link_id: &str) -> PathBuf {
        self.config_dir
            .join("links")
            .join(format!("{link_id}.json"))
    }
    pub fn credentials_file(&self) -> PathBuf {
        self.config_dir.join("credentials.json")
    }
    /// ★ M7：LAN 对等设备（含 token，0600）—— 与 link 分开存，避免把密钥混进可分享的 link JSON。
    pub fn peers_file(&self, link_id: &str) -> PathBuf {
        self.config_dir
            .join("links")
            .join(format!("{link_id}.peers.json"))
    }
    pub fn db_file(&self) -> PathBuf {
        self.data_dir.join("sync.db")
    }
    pub fn log_dir(&self) -> PathBuf {
        self.state_dir.join("log")
    }

    /// 建好所有需要写的目录，并确保配置目录是 0700。
    pub fn ensure_dirs(&self) -> Result<()> {
        for d in [
            self.config_dir.clone(),
            self.config_dir.join("links"),
            self.data_dir.clone(),
            self.log_dir(),
        ] {
            std::fs::create_dir_all(&d)?;
        }
        restrict_perms(&self.config_dir, 0o700)?;
        Ok(())
    }
}

#[cfg(unix)]
pub fn restrict_perms(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
pub fn restrict_perms(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// 一个 NAS 连接。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkConfig {
    pub id: String,
    pub host: String,
    pub port: u16,
    #[serde(default = "yes")]
    pub https: bool,
    /// 自签证书：跳过 TLS 校验（对应 Windows 客户端的证书例外）。
    #[serde(default)]
    pub insecure: bool,
    pub user: String,
    /// Qsync 家目录根：普通用户固定 `/home`。
    #[serde(default = "default_home_root")]
    pub home_root: String,
    /// ★ M6：要暴露 / 同步的远端根（多根挂载、共享文件夹）。
    /// 空 = 只用 `home_root`（与 M1–M5 完全一致）。
    #[serde(default)]
    pub roots: Vec<String>,
    /// 强制只用 IPv4：对端同时发布 AAAA 但 IPv6 路由不通时非常有用
    /// （实测遇到过：`Network is unreachable` / 传输中途 body 解码失败）。
    #[serde(default)]
    pub ipv4_only: bool,
    /// ★ M7：选择性同步 —— 排除规则（gitignore 风味，见 [`crate::rules`]）。
    /// 空 = 全部同步（M1–M6 行为一字不改）。
    #[serde(default)]
    pub exclude: Vec<String>,
    /// ★ M7：内置临时文件过滤（`*.crdownload` / `~$*` / `.goutputstream-*` / `.upload_cache*`
    /// / `*.qsync-part`）。默认开：这些文件同步出去只会给对端制造垃圾。
    #[serde(default = "yes")]
    pub filter_temp: bool,
    /// ★ M7：LAN 对等监听地址（`"127.0.0.1:9840"` / `"0.0.0.0:9840"`）。
    /// **缺省不监听**（LAN 服务默认关闭，要显式打开）。
    #[serde(default)]
    pub peer_listen: Option<String>,
    /// ★ M7：对等身份名（缺省用主机名）。
    #[serde(default)]
    pub peer_name: Option<String>,
}

fn yes() -> bool {
    true
}
fn default_home_root() -> String {
    crate::HOME_ROOT.to_string()
}

impl LinkConfig {
    pub fn base_url(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        format!("{}://{}:{}", scheme, self.host, self.port)
    }

    /// ★ M6：实际生效的远端根（归一化 + 去重）；没配 `roots` 就退回家目录。
    pub fn roots(&self) -> Vec<String> {
        if self.roots.is_empty() {
            crate::roots::normalize_roots(&[self.home_root.clone()])
        } else {
            crate::roots::normalize_roots(&self.roots)
        }
    }

    /// ★ M7：编译好的选择性同步规则（坏规则由调用方打日志，不静默）。
    pub fn rules(&self) -> crate::rules::RuleParse {
        crate::rules::Rules::parse(&self.exclude, self.filter_temp)
    }

    pub fn load(paths: &ConfigPaths, link_id: &str) -> Result<Self> {
        let p = paths.link_file(link_id);
        let raw = std::fs::read(&p).map_err(|e| Error::Io(format!("读取 {}: {e}", p.display())))?;
        serde_json::from_slice(&raw).map_err(|e| Error::Parse(format!("解析 {}: {e}", p.display())))
    }

    pub fn save(&self, paths: &ConfigPaths) -> Result<PathBuf> {
        let p = paths.link_file(&self.id);
        let body = serde_json::to_vec_pretty(self)?;
        std::fs::write(&p, body)?;
        Ok(p)
    }
}

/// ★ M7：一台已配对的对等设备（qxync ↔ qxync 的 LAN 直连）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerConfig {
    /// 对端自称的名字（`peer_name`，缺省 = 主机名）。
    pub name: String,
    /// `host:port`。
    pub addr: String,
    /// 配对时交换的共享令牌（明文 TCP 的唯一凭据）。
    pub token: String,
}

/// ★ M7：对等设备登记表（`links/<id>.peers.json`，0600）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRegistry {
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
}

impl PeerRegistry {
    pub fn load(paths: &ConfigPaths, link_id: &str) -> Result<Self> {
        let p = paths.peers_file(link_id);
        if !p.exists() {
            return Ok(Self::default());
        }
        let raw =
            std::fs::read(&p).map_err(|e| Error::Io(format!("读取 {}: {e}", p.display())))?;
        serde_json::from_slice(&raw).map_err(|e| Error::Parse(format!("解析 {}: {e}", p.display())))
    }

    /// 写 0600：临时文件 + rename（和凭据一样的写法，不出现半截文件）。
    pub fn save(&self, paths: &ConfigPaths, link_id: &str) -> Result<PathBuf> {
        paths.ensure_dirs()?;
        let target = paths.peers_file(link_id);
        let tmp = target.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(self)?;
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

    pub fn get(&self, name_or_addr: &str) -> Option<&PeerConfig> {
        self.peers
            .iter()
            .find(|p| p.name == name_or_addr || p.addr == name_or_addr)
    }

    /// 同名/同地址视为同一台设备 → 覆盖（重复配对不会长出一堆僵尸 peer）。
    pub fn upsert(&mut self, peer: PeerConfig) {
        if let Some(slot) = self
            .peers
            .iter_mut()
            .find(|p| p.name == peer.name || p.addr == peer.addr)
        {
            *slot = peer;
        } else {
            self.peers.push(peer);
        }
    }
}

/// 凭据（首版就是明文口令，文件权限 0600；后续换 keyring）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub host: String,
    pub user: String,
    pub password: String,
}

impl Credentials {
    pub fn load(paths: &ConfigPaths) -> Result<Self> {
        let p = paths.credentials_file();
        let raw = std::fs::read(&p).map_err(|e| Error::Io(format!("读取 {}: {e}", p.display())))?;
        serde_json::from_slice(&raw).map_err(|e| Error::Parse(format!("解析 {}: {e}", p.display())))
    }

    /// 写 0600：先 build 到临时文件再 rename，避免出现半截的凭据文件。
    pub fn save(&self, paths: &ConfigPaths) -> Result<PathBuf> {
        paths.ensure_dirs()?;
        let target = paths.credentials_file();
        let tmp = target.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(self)?;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_config_roundtrip() {
        let dir = std::env::temp_dir().join(format!("qxync-cfg-{}", std::process::id()));
        let paths = ConfigPaths {
            config_dir: dir.join("config"),
            data_dir: dir.join("data"),
            state_dir: dir.join("state"),
        };
        paths.ensure_dirs().unwrap();
        let link = LinkConfig {
            id: "default".into(),
            host: "nas.local".into(),
            port: 9834,
            https: true,
            insecure: true,
            user: "test1".into(),
            home_root: "/home".into(),
            roots: vec![],
            ipv4_only: false,
            exclude: vec!["/secret".into()],
            filter_temp: true,
            peer_listen: Some("127.0.0.1:9849".into()),
            peer_name: Some("unit-test".into()),
        };
        link.save(&paths).unwrap();
        let back = LinkConfig::load(&paths, "default").unwrap();
        assert_eq!(back.base_url(), "https://nas.local:9834");
        assert_eq!(back.home_root, "/home");
        assert!(back.rules().rules.is_excluded("/secret/a", true));
        assert_eq!(back.peer_listen.as_deref(), Some("127.0.0.1:9849"));
        assert_eq!(back.peer_name.as_deref(), Some("unit-test"));

        let cred = Credentials {
            host: "nas.local".into(),
            user: "test1".into(),
            password: "p@ss".into(),
        };
        let p = cred.save(&paths).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "凭据文件必须是 0600");
        }
        assert_eq!(Credentials::load(&paths).unwrap().password, "p@ss");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// M7 兼容性：老的 link JSON 没有新字段也必须能读，且行为与 M1–M6 一致。
    #[test]
    fn m7_fields_default_for_old_config() {
        let raw = br#"{"id":"old","host":"nas","port":9834,"https":true,"insecure":false,"user":"u","home_root":"/home"}"#;
        let link: LinkConfig = serde_json::from_slice(raw).unwrap();
        assert!(link.exclude.is_empty());
        assert!(link.filter_temp, "临时文件过滤默认开");
        assert!(link.peer_listen.is_none(), "LAN 监听默认关");
        assert_eq!(link.roots(), vec!["/home".to_string()]);
        let parsed = link.rules();
        assert!(parsed.rules.is_empty());
        assert!(parsed.bad.is_empty());
    }

    /// M7：对等设备登记表 —— 0600、可达、同名覆盖（不长僵尸 peer）。
    #[test]
    fn peer_registry_roundtrip_and_upsert() {
        let dir = std::env::temp_dir().join(format!("qxync-peers-{}", std::process::id()));
        let paths = ConfigPaths {
            config_dir: dir.join("config"),
            data_dir: dir.join("data"),
            state_dir: dir.join("state"),
        };
        paths.ensure_dirs().unwrap();
        let mut reg = PeerRegistry::default();
        assert!(PeerRegistry::load(&paths, "default").unwrap().peers.is_empty());
        reg.upsert(PeerConfig {
            name: "laptop".into(),
            addr: "127.0.0.1:9840".into(),
            token: "t1".into(),
        });
        reg.upsert(PeerConfig {
            name: "laptop".into(),
            addr: "127.0.0.1:9841".into(),
            token: "t2".into(),
        });
        reg.upsert(PeerConfig {
            name: "desk".into(),
            addr: "127.0.0.1:9840".into(),
            token: "t3".into(),
        });
        assert_eq!(reg.peers.len(), 2, "同名/同地址都是同一台设备");
        assert_eq!(reg.get("laptop").unwrap().token, "t2");
        assert_eq!(reg.get("127.0.0.1:9840").unwrap().name, "desk");
        let p = reg.save(&paths, "default").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "peers.json 必须 0600");
        }
        assert_eq!(PeerRegistry::load(&paths, "default").unwrap(), reg);
        std::fs::remove_dir_all(&dir).ok();
    }
}
