//! 本地配置布局（照报告 09 §9.6 蓝本）。
//!
//! ```text
//! ~/.config/qxync/config.json             全局设置
//! ~/.config/qxync/links/<link_id>.json    NAS 连接
//! ~/.config/qxync/credentials.json        口令（0600，永不出现在 --password 之外的输出里）
//! ~/.local/share/qxync/sync.db            SQLite（baseline/游标/队列，M1 起用）
//! ~/.local/state/qxync/log/               日志
//! ```
//! 目录前缀遵循 `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_STATE_HOME`。
//!
//! ★ 0.2.0：目录名从 `qsync` 改成 `qxync`（旧名与 QNAP 官方 Qsync 客户端撞名 ——
//! 官方客户端的本地数据在 `~/.local/share/QNAP/Qsync`，和这里不冲突）。
//! 改名前的老目录由 [`adopt_legacy_dir`] 搬过来：只有老目录就整体改名，
//! 两个都在就只补缺（绝不覆盖），用户不必重新登录；见 README「从 0.1.x 升级」。

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

/// ★ 0.2.0：把老目录 `<base>/qsync` 的内容搬到 `<base>/qxync`，返回新目录。
///
/// 三种情况，**都不丢数据、都不覆盖**：
/// 1. 只有老目录（0.1.x 用户的常态）→ 整体 `rename`（同分区原子；跨设备退回复制 + 删老目录）。
/// 2. 只有新目录（全新机器）→ 直接用。
/// 3. **两个都在** → 只补缺：把老目录里新目录没有的条目搬进来，已存在的一律不动。
///    这一支是给「0.1.0 的原型目录 `qxync/` 还留在机器上」那种情况准备的 ——
///    0.1.0 用的就是 `qxync`，改名成 `qsync` 之后又改回来，于是两个都在。
///    补缺而不是整体替换，是为了「活的那份永远不被旧的那份盖掉」。
///
/// 迁移本身失败 → 打一行 WARN 就继续用新目录：宁可让用户重新 `login` /
/// 重新对账一轮，也不能因为迁移不了就让 CLI/daemon 起不来。
fn adopt_legacy_dir(base: PathBuf) -> PathBuf {
    let new = base.join("qxync");
    let old = base.join("qsync");
    if !old.exists() {
        return new; // 情况 2（或全新机器）
    }
    if !new.exists() {
        match std::fs::rename(&old, &new) {
            Ok(()) => eprintln!(
                "ℹ️  已把旧目录迁移过来：{} → {}",
                old.display(),
                new.display()
            ),
            Err(e_rename) => match copy_dir_all(&old, &new) {
                Ok(()) => {
                    let _ = std::fs::remove_dir_all(&old);
                    eprintln!(
                        "ℹ️  已把旧目录复制过来：{} → {}",
                        old.display(),
                        new.display()
                    );
                }
                Err(e_copy) => eprintln!(
                    "⚠️  旧目录 {} 迁移失败（rename: {e_rename}；copy: {e_copy}），\
                     本次使用 {}；需要的话请手动搬运",
                    old.display(),
                    new.display()
                ),
            },
        }
        return new;
    }
    // 情况 3：两个都在 —— 补缺。每个进程只做一次，`discover()` 会被反复调用。
    merge_missing_once(&old, &new);
    new
}

/// 情况 3 的入口：同一个 `new` 每个进程只走一次，避免 `discover()` 被反复调用时重复扫盘。
fn merge_missing_once(old: &Path, new: &Path) {
    use std::sync::{Mutex, OnceLock};
    static DONE: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
    let seen = DONE.get_or_init(|| Mutex::new(Vec::new()));
    {
        let mut seen = seen.lock().unwrap_or_else(|e| e.into_inner());
        if seen.iter().any(|p| p == new) {
            return;
        }
        seen.push(new.to_path_buf());
    }
    match merge_missing(old, new) {
        Ok(0) => {}
        Ok(n) => eprintln!(
            "ℹ️  检测到新旧目录并存：已把 {} 里缺的 {n} 个条目补进 {}（已有的一律没动）。\
             核对无误后可以把老目录删掉。",
            old.display(),
            new.display()
        ),
        Err(e) => eprintln!(
            "⚠️  {} 与 {} 并存，补缺失败：{e}；本次只用 {}，需要的话请手动搬运",
            old.display(),
            new.display(),
            new.display()
        ),
    }
}

/// 递归补缺：只复制 `to` 里**不存在**的东西（文件按名字判存），已存在的一律不碰。
fn merge_missing(from: &Path, to: &Path) -> std::io::Result<usize> {
    std::fs::create_dir_all(to)?;
    let mut copied = 0;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copied += merge_missing(&src, &dst)?;
        } else if !dst.exists() {
            std::fs::copy(&src, &dst)?;
            copied += 1;
        }
    }
    Ok(copied)
}

/// 递归复制（跨文件系统时 `rename` 会失败，用它兜底）。
fn copy_dir_all(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

impl ConfigPaths {
    /// 按 XDG 约定推导（不创建目录）。
    ///
    /// ★ 0.2.0：顺手做 `qsync` → `qxync` 的一次性目录迁移（见 [`adopt_legacy_dir`]）。
    pub fn discover() -> Result<Self> {
        let h = home()?;
        Ok(Self {
            config_dir: adopt_legacy_dir(xdg("XDG_CONFIG_HOME", h.join(".config"))),
            data_dir: adopt_legacy_dir(xdg("XDG_DATA_HOME", h.join(".local/share"))),
            state_dir: adopt_legacy_dir(xdg("XDG_STATE_HOME", h.join(".local/state"))),
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

    /// ★ M16：某个连接的凭据文件。
    ///
    /// `default` 继续用旧的全局 `credentials.json`（升级零迁移）；其它连接用
    /// `links/<id>.credentials.json`，避免 A 的密码被拿去登 B。
    pub fn link_credentials_file(&self, link_id: &str) -> PathBuf {
        if link_id == "default" {
            self.credentials_file()
        } else {
            self.config_dir
                .join("links")
                .join(format!("{link_id}.credentials.json"))
        }
    }

    /// ★ M16：某个连接的挂载现场记录。
    ///
    /// `default` 继续用旧的 `state/qxync/mounts.json`；其它连接一瓶一个文件，
    /// 多 daemon 各写各的，不再有「全量重写把别人的记录抹掉」的竞态。
    pub fn mounts_file_for(&self, link: &LinkConfig) -> PathBuf {
        if link.id == "default" {
            self.state_dir.join("qxync").join("mounts.json")
        } else {
            self.state_dir
                .join("qxync")
                .join("mounts")
                .join(format!("{}.json", link_namespace(link)))
        }
    }

    /// ★ M16：某个连接的同步状态目录（游标 / baseline / sync.db / journal）。
    pub fn sync_dir_for(&self, link: &LinkConfig) -> PathBuf {
        self.data_dir.join("sync").join(link_namespace(link))
    }

    /// ★ M16：某个连接的水合缓存目录（实际缓存文件还会再拼一层 `conflicts/` 之类）。
    pub fn cache_dir_for(&self, link: &LinkConfig) -> PathBuf {
        self.data_dir.join("cache").join(link_namespace(link))
    }

    /// ★ M16：某个连接的上传/删除队列目录。
    pub fn queue_dir_for(&self, link: &LinkConfig) -> PathBuf {
        self.data_dir
            .join("upload-queue")
            .join(link_namespace(link))
    }

    /// ★ M16：旧版全局队列目录（`data/upload-queue`）—— **只给迁移用**。
    pub fn legacy_queue_dir(&self) -> PathBuf {
        self.data_dir.join("upload-queue")
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

/// ★ M16：把任意字符串变成**安全的单层路径组件**。
///
/// 只保留 `A-Z a-z 0-9 . _ -`，其余全换成 `_`。注意：这个函数**永远不会**
/// 产出 `#` 或 `@` —— 多连接的命名空间就靠这两个字符与「旧 host 命名空间」区分开。
pub fn sanitize_component(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        "_".into()
    } else if out == "." || out == ".." {
        format!("_{out}")
    } else {
        out
    }
}

/// ★ M16：一个连接的数据命名空间。
///
/// * `default` → `sanitize(host)`（= 旧版 `sync/<host>` / `cache/<host>` 的目录名，
///   升级后路径一字不变）；
/// * 其它 → `#<id>@<sanitize(host)>`。`#` / `@` 不可能出现在 [`sanitize_component`]
///   的输出里，所以同一台 NAS 上的多个账号也不会互相踩缓存/队列/状态库。
pub fn link_namespace(link: &LinkConfig) -> String {
    let host = sanitize_component(&link.host);
    if link.id == "default" {
        host
    } else {
        format!("#{}@{}", sanitize_component(&link.id), host)
    }
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
    /// 强制只用 IPv4：对端同时发布 AAAA 但 IPv6 路由不通时非常有用
    /// （实测遇到过：`Network is unreachable` / 传输中途 body 解码失败）。
    #[serde(default)]
    pub ipv4_only: bool,
    /// ★ M7：选择性同步 —— 排除规则（gitignore 风味，见 [`crate::rules`]）。
    /// 空 = 全部同步（M1–M6 行为一字不改）。
    #[serde(default)]
    pub exclude: Vec<String>,
    /// ★ M7：内置临时文件过滤（`*.crdownload` / `~$*` / `.goutputstream-*` / `.upload_cache*`
    /// / `*.qxync-part`）。默认开：这些文件同步出去只会给对端制造垃圾。
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
impl LinkConfig {
    pub fn base_url(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        format!("{}://{}:{}", scheme, self.host, self.port)
    }

    /// ★ M7：编译好的选择性同步规则（坏规则由调用方打日志，不静默）。
    pub fn rules(&self) -> crate::rules::RuleParse {
        crate::rules::Rules::parse(&self.exclude, self.filter_temp)
    }

    /// ★ M16：连接 id 必须能安全地当文件名 / socket 名（防路径穿越）。
    pub fn is_valid_id(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= 64
            && id != "."
            && id != ".."
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    }

    fn validate_id(&self) -> Result<()> {
        if !Self::is_valid_id(&self.id) {
            return Err(Error::Io(format!(
                "连接 id 不合法：{:?}（只允许 A-Za-z0-9 . _ -，且不能是 . / ..）",
                self.id
            )));
        }
        Ok(())
    }

    pub fn load(paths: &ConfigPaths, link_id: &str) -> Result<Self> {
        if !Self::is_valid_id(link_id) {
            return Err(Error::Io(format!("连接 id 不合法：{link_id:?}")));
        }
        let p = paths.link_file(link_id);
        let raw = std::fs::read(&p).map_err(|e| Error::Io(format!("读取 {}: {e}", p.display())))?;
        let link: Self = serde_json::from_slice(&raw)
            .map_err(|e| Error::Parse(format!("解析 {}: {e}", p.display())))?;
        // 文件名与文件里的 id 必须一致：否则 save() 会写到另一个文件，
        // 用户会看到「改了一个连接，另一个连接变了」这种极难排查的现象。
        if link.id != link_id {
            return Err(Error::Parse(format!(
                "{} 里的 id 是 {:?}，与文件名 {:?} 不一致",
                p.display(),
                link.id,
                link_id
            )));
        }
        Ok(link)
    }

    pub fn save(&self, paths: &ConfigPaths) -> Result<PathBuf> {
        self.validate_id()?;
        paths.ensure_dirs()?;
        let p = paths.link_file(&self.id);
        let tmp = p.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(self)?;
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &p)?;
        Ok(p)
    }

    /// ★ M16：列出全部连接（按 id 排序）。
    ///
    /// 与 [`crate::tasks::Task::list`] 同风格：单个文件坏掉**不整体失败**，
    /// 坏文件进第二个返回值，让 GUI/CLI 能如实报出来。
    pub fn list(paths: &ConfigPaths) -> (Vec<Self>, Vec<(PathBuf, String)>) {
        let dir = paths.config_dir.join("links");
        let mut out = Vec::new();
        let mut bad = Vec::new();
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => return (out, bad),
        };
        for e in rd.flatten() {
            let p = e.path();
            let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            // peers / 凭据 / 临时文件都不是连接配置
            if name.ends_with(".peers.json")
                || name.ends_with(".credentials.json")
                || name.ends_with(".json.tmp")
            {
                continue;
            }
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = p.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            match Self::load(paths, id) {
                Ok(l) => out.push(l),
                Err(err) => bad.push((p.clone(), err.to_string())),
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        (out, bad)
    }

    /// ★ M16：删除连接配置（link / 凭据 / peers）。**不删任务、不删缓存/状态**。
    ///
    /// 任务与缓存由 GUI 的「移除连接」按用户确认结果处理；core 只做最保守的
    /// 配置清理，避免一个底层函数悄悄删掉用户数据。
    pub fn delete(paths: &ConfigPaths, link_id: &str) -> Result<bool> {
        if !Self::is_valid_id(link_id) {
            return Err(Error::Io(format!("连接 id 不合法：{link_id:?}")));
        }
        let link = paths.link_file(link_id);
        let existed = link.exists();
        for p in [
            link,
            paths.link_credentials_file(link_id),
            paths.peers_file(link_id),
        ] {
            if p.exists() {
                std::fs::remove_file(&p)?;
            }
        }
        Ok(existed)
    }

    /// ★ M16：旧数据（没有 link_id 的任务、旧 `mounts.json`、旧全局 queue）归谁。
    ///
    /// `default` 优先；没有 `default` 时归字典序最小的连接。这个规则必须**确定**：
    /// 两个 daemon 同时启动时，不能让它们都认为旧数据归自己。
    pub fn legacy_owner(ids: &[String]) -> Option<String> {
        if ids.iter().any(|id| id == "default") {
            return Some("default".into());
        }
        ids.iter().min().cloned()
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
        let raw = std::fs::read(&p).map_err(|e| Error::Io(format!("读取 {}: {e}", p.display())))?;
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

    /// ★ M16：按连接读凭据。
    ///
    /// 优先 `links/<id>.credentials.json`（`default` 是旧的 `credentials.json`）。
    /// 旧版只有一个全局凭据文件：非 default 连接首次读取时会回落到它，但必须
    /// **host + user 都对上**才认 —— 否则就会出现「拿 A 的密码去登 B」。
    pub fn load_for(paths: &ConfigPaths, link: &LinkConfig) -> Result<Self> {
        let target = paths.link_credentials_file(&link.id);
        let read = |p: &Path| -> Result<Self> {
            let raw =
                std::fs::read(p).map_err(|e| Error::Io(format!("读取 {}: {e}", p.display())))?;
            serde_json::from_slice(&raw)
                .map_err(|e| Error::Parse(format!("解析 {}: {e}", p.display())))
        };
        let mut c = match std::fs::read(&target) {
            Ok(_) => read(&target)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // 回落旧全局凭据（只认 host/user 完全匹配的那一份）
                let legacy = paths.credentials_file();
                let c = read(&legacy)?;
                if !same_account(&c, link) {
                    return Err(Error::Io(format!(
                        "{} 里的凭据是 {}@{}，与连接「{}」的 {}@{} 不匹配",
                        legacy.display(),
                        c.user,
                        c.host,
                        link.id,
                        link.user,
                        link.host
                    )));
                }
                c
            }
            Err(e) => return Err(Error::Io(format!("读取 {}: {e}", target.display()))),
        };
        if !same_account(&c, link) {
            return Err(Error::Io(format!(
                "{} 里的凭据是 {}@{}，与连接「{}」的 {}@{} 不匹配",
                target.display(),
                c.user,
                c.host,
                link.id,
                link.user,
                link.host
            )));
        }
        // 统一成连接上的写法，免得大小写差异把后续比较搞乱
        c.host = link.host.clone();
        c.user = link.user.clone();
        Ok(c)
    }

    /// ★ M16：按连接写凭据（0600）。`default` 继续写旧的 `credentials.json`。
    pub fn save_for(&self, paths: &ConfigPaths, link: &LinkConfig) -> Result<PathBuf> {
        let mut c = self.clone();
        c.host = link.host.clone();
        c.user = link.user.clone();
        c.save_to(paths, &paths.link_credentials_file(&link.id))
    }

    /// ★ M16：凭据是否存在（GUI 用；**绝不回传口令**）。
    pub fn present_for(
        paths: &ConfigPaths,
        link: &LinkConfig,
    ) -> Option<(String, String, PathBuf)> {
        match Self::load_for(paths, link) {
            Ok(c) => {
                // load_for 可能回落到旧文件；这里如实报「实际可用的那份」
                let target = paths.link_credentials_file(&link.id);
                let p = if target.exists() {
                    target
                } else {
                    paths.credentials_file()
                };
                Some((c.host, c.user, p))
            }
            Err(_) => None,
        }
    }

    /// 写 0600：先 build 到临时文件再 rename，避免出现半截的凭据文件。
    pub fn save(&self, paths: &ConfigPaths) -> Result<PathBuf> {
        let target = paths.credentials_file();
        self.save_to(paths, &target)
    }

    fn save_to(&self, paths: &ConfigPaths, target: &Path) -> Result<PathBuf> {
        paths.ensure_dirs()?;
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = target.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(self)?;
        {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&tmp)?;
            restrict_perms(&tmp, 0o600)?;
            f.write_all(&body)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, target)?;
        Ok(target.to_path_buf())
    }
}

/// 凭据是不是这个连接账号的（host 大小写不敏感，user 精确）。
fn same_account(c: &Credentials, link: &LinkConfig) -> bool {
    c.host.eq_ignore_ascii_case(&link.host) && c.user == link.user
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 0.2.0：老的 `qsync/` 目录要迁到 `qxync/`，三种情况都不丢数据、都不覆盖。
    #[test]
    fn legacy_dir_is_adopted_once_and_never_clobbers_new() {
        let base = std::env::temp_dir().join(format!("qxync-adopt-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();

        // ① 只有老目录 → 整体改名过来，内容跟着走，老目录不再留着（否则会有两份状态）。
        let old = base.join("qsync");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("credentials.json"), b"{\"user\":\"test1\"}").unwrap();
        let new = adopt_legacy_dir(base.clone());
        assert_eq!(new, base.join("qxync"));
        assert!(new.join("credentials.json").exists(), "老凭据要跟着搬过来");
        assert!(!old.exists(), "整体迁移后不该再有 qsync/ 目录");

        // ② 两个都在（0.1.0 的原型目录 + 0.1.1 的活目录）→ **只补缺**：
        //    老目录独有的搬进来，新目录已有的一个字都不改。
        let old2 = base.join("qsync");
        std::fs::create_dir_all(old2.join("sync/nas")).unwrap();
        std::fs::write(old2.join("sync/nas/sync.db"), b"live-state").unwrap();
        std::fs::write(old2.join("credentials.json"), b"STALE").unwrap();
        std::fs::write(new.join("credentials.json"), b"KEEP").unwrap();
        let again = adopt_legacy_dir(base.clone());
        assert_eq!(again, base.join("qxync"));
        assert_eq!(
            std::fs::read(again.join("credentials.json")).unwrap(),
            b"KEEP",
            "新目录已有的文件绝不能被旧目录盖掉"
        );
        assert_eq!(
            std::fs::read(again.join("sync/nas/sync.db")).unwrap(),
            b"live-state",
            "老目录独有的活状态要补进来"
        );
        assert!(
            old2.join("sync/nas/sync.db").exists(),
            "老目录原样留着，不删"
        );

        // ③ 可重入：再跑一次不报错、也不重复搬运。
        assert!(adopt_legacy_dir(base.clone()).exists());

        std::fs::remove_dir_all(&base).ok();
    }

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
            ipv4_only: false,
            exclude: vec!["/secret".into()],
            filter_temp: true,
            peer_listen: Some("127.0.0.1:9849".into()),
            peer_name: Some("unit-test".into()),
        };
        link.save(&paths).unwrap();
        let back = LinkConfig::load(&paths, "default").unwrap();
        assert_eq!(back.base_url(), "https://nas.local:9834");
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

    /// 兼容性：老的 link JSON（含已删除的 `home_root` / `roots`）也必须能读。
    #[test]
    fn m7_fields_default_for_old_config() {
        let raw = br#"{"id":"old","host":"nas","port":9834,"https":true,"insecure":false,"user":"u","home_root":"/home","roots":["/home"]}"#;
        let link: LinkConfig = serde_json::from_slice(raw).unwrap();
        assert!(link.exclude.is_empty());
        assert!(link.filter_temp, "临时文件过滤默认开");
        assert!(link.peer_listen.is_none(), "LAN 监听默认关");
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
        assert!(PeerRegistry::load(&paths, "default")
            .unwrap()
            .peers
            .is_empty());
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

    // ------------------------------------------------------------ ★ M16 多连接

    fn m16_paths(tag: &str) -> ConfigPaths {
        let dir = std::env::temp_dir().join(format!(
            "qxync-m16-cfg-{}-{}-{}",
            process_marker(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let p = ConfigPaths {
            config_dir: dir.join("config"),
            data_dir: dir.join("data"),
            state_dir: dir.join("state"),
        };
        p.ensure_dirs().unwrap();
        p
    }

    fn process_marker() -> u32 {
        std::process::id()
    }

    fn mk_link(id: &str, host: &str, user: &str) -> LinkConfig {
        LinkConfig {
            id: id.into(),
            host: host.into(),
            port: 9834,
            https: true,
            insecure: false,
            user: user.into(),
            ipv4_only: false,
            exclude: Vec::new(),
            filter_temp: true,
            peer_listen: None,
            peer_name: None,
        }
    }

    #[test]
    fn m16_default_namespace_is_exactly_the_legacy_host_path() {
        let paths = m16_paths("ns-default");
        let link = mk_link("default", "nas.local", "u");
        assert_eq!(link_namespace(&link), "nas.local");
        assert_eq!(
            paths.sync_dir_for(&link),
            paths.data_dir.join("sync/nas.local")
        );
        assert_eq!(
            paths.cache_dir_for(&link),
            paths.data_dir.join("cache/nas.local")
        );
        assert_eq!(
            paths.queue_dir_for(&link),
            paths.data_dir.join("upload-queue/nas.local")
        );
        assert_eq!(
            paths.mounts_file_for(&link),
            paths.state_dir.join("qxync/mounts.json")
        );
        // host 里的非法字符仍走旧的 `_` 替换
        assert_eq!(
            link_namespace(&mk_link("default", "nas:9834", "u")),
            "nas_9834"
        );
        std::fs::remove_dir_all(paths.data_dir.parent().unwrap()).ok();
    }

    #[test]
    fn m16_non_default_namespace_cannot_collide_with_host_namespace() {
        let paths = m16_paths("ns-other");
        let link = mk_link("nas2", "nas.local", "u");
        assert_eq!(link_namespace(&link), "#nas2@nas.local");
        assert!(paths.sync_dir_for(&link).ends_with("#nas2@nas.local"));
        assert!(paths.cache_dir_for(&link).ends_with("#nas2@nas.local"));
        assert!(paths.queue_dir_for(&link).ends_with("#nas2@nas.local"));
        assert!(paths
            .mounts_file_for(&link)
            .ends_with("mounts/#nas2@nas.local.json"));
        // 同一台 NAS 的另一个账号 → 不同命名空间
        assert_ne!(
            link_namespace(&link),
            link_namespace(&mk_link("nas3", "nas.local", "u"))
        );
        std::fs::remove_dir_all(paths.data_dir.parent().unwrap()).ok();
    }

    #[test]
    fn m16_link_id_validation_and_list() {
        assert!(LinkConfig::is_valid_id("default"));
        assert!(LinkConfig::is_valid_id("nas-2.local_1"));
        for bad in ["", ".", "..", "a/b", "../x", "a b", "a#b"] {
            assert!(!LinkConfig::is_valid_id(bad), "{bad:?} 不该合法");
        }
        let paths = m16_paths("list");
        let a = mk_link("default", "a.local", "u");
        let b = mk_link("nas2", "b.local", "u");
        a.save(&paths).unwrap();
        b.save(&paths).unwrap();
        // 辅助文件不能出现在连接列表里
        std::fs::write(
            paths.link_file("default").with_extension("peers.json"),
            b"{}",
        )
        .unwrap();
        std::fs::write(
            paths
                .link_file("default")
                .with_extension("credentials.json"),
            b"{}",
        )
        .unwrap();
        let (list, bad) = LinkConfig::list(&paths);
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!(
            list.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(),
            vec!["default", "nas2"]
        );
        std::fs::remove_dir_all(paths.data_dir.parent().unwrap()).ok();
    }

    #[test]
    fn m16_legacy_owner_is_default_then_lexicographically_smallest() {
        assert_eq!(
            LinkConfig::legacy_owner(&["nas2".into(), "default".into()]),
            Some("default".into())
        );
        assert_eq!(
            LinkConfig::legacy_owner(&["nas3".into(), "nas2".into()]),
            Some("nas2".into())
        );
        assert_eq!(LinkConfig::legacy_owner(&[]), None);
    }

    #[test]
    fn m16_credentials_are_per_link_and_default_stays_legacy() {
        let paths = m16_paths("cred");
        let d = mk_link("default", "a.local", "u1");
        let n = mk_link("nas2", "b.local", "u2");

        let cd = Credentials {
            host: "a.local".into(),
            user: "u1".into(),
            password: "pw-default".into(),
        };
        let p = cd.save_for(&paths, &d).unwrap();
        assert_eq!(p, paths.credentials_file(), "default 必须继续用旧文件");
        assert_eq!(
            Credentials::load_for(&paths, &d).unwrap().password,
            "pw-default"
        );

        let cn = Credentials {
            host: "b.local".into(),
            user: "u2".into(),
            password: "pw-nas2".into(),
        };
        let pn = cn.save_for(&paths, &n).unwrap();
        assert_eq!(pn, paths.link_credentials_file("nas2"));
        assert_eq!(
            Credentials::load_for(&paths, &n).unwrap().password,
            "pw-nas2"
        );
        // 非 default 的文件存在时**绝不**回落全局凭据
        assert_eq!(
            Credentials::load_for(&paths, &d).unwrap().password,
            "pw-default"
        );

        // host/user 不匹配 → 明确报错，不能拿 A 的密码去登 B
        let wrong = mk_link("nas2", "other.local", "u2");
        assert!(Credentials::load_for(&paths, &wrong).is_err());
        std::fs::remove_dir_all(paths.data_dir.parent().unwrap()).ok();
    }
}
