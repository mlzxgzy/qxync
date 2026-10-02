//! # M8.2：同步任务（Task）—— 把「挂载点 + 远端根 + 策略」持久化成一等公民
//!
//! 在 M8.2 之前，「同步范围」只存在于**内存**里：`qxyncd` 的 `mounts` 是个
//! `HashMap`，daemon 一重启就全没了（`docs/M8-向Qsync-Client-6靠拢.md` §2.2）。
//! 本模块把每个挂载点登记成一条**持久化任务**，于是：
//!
//! * 任务可以在 daemon 重启后**显式恢复**（`--restore-tasks`，见 daemon 侧）；
//! * 每个任务可以单独**暂停 / 继续**，不再只能全局暂停轮询；
//! * 界面有了 Qsync 那样的「任务列表 → 任务详情」两层结构；
//! * Qsync 的策略字段（同步方向 / 节省空间模式 / 智能删除 / 选择性同步）**先落盘不丢**，
//!   行为在后续里程碑接上（M8.4）。
//!
//! 存储：`~/.config/qxync/tasks/<id>.json`（一个任务一个文件，原子写）。
//!
//! ## 兼容性铁律
//!
//! **任务层是「外壳」**：它只记录参数并驱动既有的 `mount` / `umount` 代码路径，
//! **不触碰 FUSE 内部**。所以「没有任务文件时，daemon 行为与 M7 一字不变」。

use crate::config::{restrict_perms, ConfigPaths};
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 同步方向（对齐 Qsync 6.0 的 1-way / 2-way sync rules）。
pub const DIR_2WAY: &str = "2way";
pub const DIR_UP: &str = "1way-up";
pub const DIR_DOWN: &str = "1way-down";
pub const DIRECTIONS: [&str; 3] = [DIR_2WAY, DIR_UP, DIR_DOWN];

/// 缓存模式（与 `Request::Mount.cache_mode` 同义）。
pub const CACHE_PAGECACHE: &str = "pagecache";
pub const CACHE_DIRECT: &str = "direct";
pub const CACHE_MODES: [&str; 2] = [CACHE_PAGECACHE, CACHE_DIRECT];

// ---------------------------------------------------------------- ★ M8.4 冲突策略
//
// 五选项**逐字照抄** Qsync Client 6（见 `docs/M8-向Qsync-Client-6靠拢.md` §1.7）：
//
//   1. Let me decide for each file              → ask
//   2. Rename files on the NAS                  → rename_remote
//   3. Rename local files                       → rename_local   ← 默认（= M2c 现有行为）
//   4. Replace files on the NAS with local files→ replace_remote
//   5. Replace local files with files on the NAS→ replace_local
//
// 默认刻意选 `rename_local`：M2c 的硬编码行为就是「远端占原名、本地内容另存
// `xxx (conflicted copy from …)`」，也就是「重命名本地文件」。于是**没配过策略的
// 任务行为一字不变**（`sync.rs` 的单测 `decision_table` 与 m7/m82 矩阵都依赖这点）。

/// 每个文件都问我（写待裁决队列，GUI 逐个裁决）。
pub const CONFLICT_ASK: &str = "ask";
/// 重命名 NAS 上的文件（本地内容占原名）。
pub const CONFLICT_RENAME_REMOTE: &str = "rename_remote";
/// 重命名本地文件（远端内容占原名）—— **默认**。
pub const CONFLICT_RENAME_LOCAL: &str = "rename_local";
/// 用本地文件替换 NAS 上的文件（远端那份会丢）。
pub const CONFLICT_REPLACE_REMOTE: &str = "replace_remote";
/// 用 NAS 上的文件替换本地文件（本地那份会丢）。
pub const CONFLICT_REPLACE_LOCAL: &str = "replace_local";
/// 五个取值，顺序 = 界面顺序（照抄 Qsync 下拉框）。
pub const CONFLICTS: [&str; 5] = [
    CONFLICT_ASK,
    CONFLICT_RENAME_REMOTE,
    CONFLICT_RENAME_LOCAL,
    CONFLICT_REPLACE_REMOTE,
    CONFLICT_REPLACE_LOCAL,
];
/// 默认冲突策略（= M2c 现有行为）。
pub const DEFAULT_CONFLICT: &str = CONFLICT_RENAME_LOCAL;

fn default_conflict() -> String {
    DEFAULT_CONFLICT.to_string()
}

/// 策略 → GUI 文案（照抄 §1.7 的中文原文）。
pub fn conflict_label(v: &str) -> &'static str {
    match v {
        CONFLICT_ASK => "每个文件都问我",
        CONFLICT_RENAME_REMOTE => "重命名 NAS 上的文件",
        CONFLICT_RENAME_LOCAL => "重命名本地文件",
        CONFLICT_REPLACE_REMOTE => "用本地文件替换 NAS 上的文件",
        CONFLICT_REPLACE_LOCAL => "用 NAS 上的文件替换本地文件",
        _ => "未知策略",
    }
}

/// 该策略是否可能**丢数据**（`replace_*` 会覆盖一方；UI 要显式警告）。
pub fn conflict_is_destructive(v: &str) -> bool {
    matches!(v, CONFLICT_REPLACE_REMOTE | CONFLICT_REPLACE_LOCAL)
}

fn yes() -> bool {
    true
}
fn default_cache_mode() -> String {
    CACHE_PAGECACHE.to_string()
}
fn default_direction() -> String {
    DIR_2WAY.to_string()
}

/// 一个同步任务。
///
/// 字段刻意宽松（全部 `#[serde(default)]`）：**旧文件缺字段能读、新字段不丢**，
/// 这样以后加策略不用写迁移。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    /// 任务 id，同时是文件名（`tasks/<id>.json`）。只允许 `[A-Za-z0-9._-]`。
    pub id: String,
    /// 展示名（缺省用 id）。
    #[serde(default)]
    pub name: String,
    /// 是否启用。`true` = 该任务应当处于挂载 + 同步状态。
    #[serde(default = "yes")]
    pub enabled: bool,
    /// 本地挂载点（绝对路径）。
    pub mountpoint: PathBuf,
    /// ★ 水合缓存目录（**父目录**；实际缓存会再拼一层 NAS 主机名做隔离）。
    /// `None` = 用默认 `$XDG_DATA_HOME/qxync/cache`。
    /// ⚠️ 这是**父目录**：daemon 会 `cache_dir.join(<nas host>)`，见 `daemon.rs::mount()`。
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,
    /// ★ 这一个任务的 NAS 文件夹（一对一）：`/home`、`/Public`…
    /// `None` = 没指定 → 用 Qsync 家目录（[`crate::HOME_ROOT`]）。
    #[serde(default)]
    pub root: Option<String>,
    /// ⚠️ **只读的旧格式探测**：多根时代文件里的 `roots` 数组。
    ///
    /// 不参与任何同步逻辑，只在 [`Task::normalize`] 里用来**报错**：一对多已删除，
    /// 直接忽略会让用户以为多个目录还在同步（静默缩小同步范围）。保存时不写回。
    #[serde(default, rename = "roots", skip_serializing)]
    pub legacy_roots: Vec<String>,
    /// 读写挂载（默认只读，与 M1–M7 一致）。
    #[serde(default)]
    pub read_write: bool,
    #[serde(default = "default_cache_mode")]
    pub cache_mode: String,
    #[serde(default)]
    pub threads: Option<usize>,
    #[serde(default)]
    pub hydrate_timeout_secs: Option<u64>,
    #[serde(default)]
    pub delete_limit: Option<usize>,
    #[serde(default = "yes")]
    pub auto_unmount: bool,

    // ---- 以下是 Qsync 的策略字段：M8.2 只负责「存得住、不丢」，行为后续里程碑接 ----
    /// 同步方向：`2way`（默认）/ `1way-up` / `1way-down`。
    #[serde(default = "default_direction")]
    pub direction: String,
    /// 节省空间模式（≈ qxync 的按需同步 / 脱水）。
    #[serde(default)]
    pub space_saving: bool,
    /// 智能删除（≈ 删除熔断 / 待确认删除）。**与 `space_saving` 互斥**。
    #[serde(default)]
    pub smart_delete: bool,
    /// 选择性同步：要同步到本机的子文件夹（根相对）。
    #[serde(default)]
    pub selective: Vec<String>,
    /// 筛选器规则（复用 M7 的 gitignore 风味语法）。
    #[serde(default)]
    pub exclude: Vec<String>,
    /// ★ M8.4：冲突策略（5 个取值见 [`CONFLICTS`]；默认 = M2c 的硬编码行为）。
    #[serde(default = "default_conflict")]
    pub conflict: String,
}

impl Default for Task {
    /// **占位用**：只为 `TaskInfo` / `TasksData` 的 serde 默认值服务。
    ///
    /// 刻意做成「不可保存」的形态（`mountpoint` 为空、`enabled=false`）：这样万一
    /// 它被误当成真任务，`normalize()` 会直接拒绝（挂载点必须是绝对路径），
    /// 不会在磁盘上留一条没有挂载点的垃圾任务。见 `default_task_is_unsavable` 单测。
    fn default() -> Self {
        Self {
            id: "default".into(),
            name: String::new(),
            enabled: false,
            mountpoint: PathBuf::new(),
            cache_dir: None,
            root: None,
            legacy_roots: Vec::new(),
            read_write: false,
            cache_mode: default_cache_mode(),
            threads: None,
            hydrate_timeout_secs: None,
            delete_limit: None,
            auto_unmount: true,
            direction: default_direction(),
            space_saving: false,
            smart_delete: false,
            selective: Vec::new(),
            exclude: Vec::new(),
            conflict: default_conflict(),
        }
    }
}

impl Task {
    /// 从一次 `mount` 请求派生任务（id 缺省 `default`）。
    #[allow(clippy::too_many_arguments)]
    pub fn from_mount(
        id: Option<String>,
        mountpoint: PathBuf,
        root: Option<String>,
        read_write: bool,
        cache_mode: Option<String>,
        threads: Option<usize>,
        hydrate_timeout_secs: Option<u64>,
        delete_limit: Option<usize>,
        auto_unmount: Option<bool>,
    ) -> Self {
        let id = id
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "default".into());
        Self {
            name: id.clone(),
            id,
            enabled: true,
            mountpoint,
            cache_dir: None,
            root: root.filter(|r| !r.trim().is_empty()),
            legacy_roots: Vec::new(),
            read_write,
            cache_mode: cache_mode.unwrap_or_else(default_cache_mode),
            threads,
            hydrate_timeout_secs,
            delete_limit,
            auto_unmount: auto_unmount.unwrap_or(true),
            direction: default_direction(),
            space_saving: false,
            smart_delete: false,
            selective: Vec::new(),
            exclude: Vec::new(),
            conflict: default_conflict(),
        }
    }

    /// 设置缓存目录（**父目录**）。链式写法，便于 CLI/GUI 构造。
    pub fn with_cache_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.cache_dir = dir.filter(|p| !p.as_os_str().is_empty());
        self
    }

    /// ★ M8.4：设置冲突策略（空/未知 → 保持默认，不报错：旧文件读得回来更重要）。
    pub fn with_conflict(mut self, v: Option<String>) -> Self {
        if let Some(v) = v.filter(|s| CONFLICTS.contains(&s.as_str())) {
            self.conflict = v;
        }
        self
    }

    /// 任务 id 是否可安全用作文件名（**防路径穿越**：`../x`、`a/b` 一律拒绝）。
    pub fn is_valid_id(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= 64
            && id != "."
            && id != ".."
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    }

    pub fn tasks_dir(paths: &ConfigPaths) -> PathBuf {
        paths.config_dir.join("tasks")
    }

    pub fn file(paths: &ConfigPaths, id: &str) -> PathBuf {
        Self::tasks_dir(paths).join(format!("{id}.json"))
    }

    /// 归一化 + 校验。
    ///
    /// * `id` 非法 → 报错（不静默改写，否则用户找不到自己的任务）；
    /// * `name` 空 → 回填 id；
    /// * `cache_mode` / `direction` 非法 → 落回默认值（宽松，避免旧文件读不出来）；
    /// * **`space_saving` 为真时强制 `smart_delete = false`**
    ///   —— 这是 Qsync 原文语义（"This function is not available when Space-Saving Mode is enabled."），
    ///   不是我们的发明。
    pub fn normalize(&mut self) -> Result<()> {
        if !Self::is_valid_id(&self.id) {
            return Err(Error::Io(format!(
                "任务 id 不合法：{:?}（只允许 A-Za-z0-9 . _ -，且不能是 . / ..）",
                self.id
            )));
        }
        if self.name.trim().is_empty() {
            self.name = self.id.clone();
        }
        if let Some(c) = &self.cache_dir {
            if !c.is_absolute() {
                return Err(Error::Io(format!(
                    "缓存目录必须是绝对路径：{}",
                    c.display()
                )));
            }
        }
        if !self.mountpoint.is_absolute() {
            return Err(Error::Io(format!(
                "挂载点必须是绝对路径：{}",
                self.mountpoint.display()
            )));
        }
        if !CACHE_MODES.contains(&self.cache_mode.as_str()) {
            self.cache_mode = default_cache_mode();
        }
        if !DIRECTIONS.contains(&self.direction.as_str()) {
            self.direction = default_direction();
        }
        // ★ M8.4：未知冲突策略落回默认（宽松，避免旧文件/手工编辑读不出来）
        if !CONFLICTS.contains(&self.conflict.as_str()) {
            self.conflict = default_conflict();
        }
        if self.space_saving {
            self.smart_delete = false;
        }
        // ★ 旧格式（多根时代的 `roots` 数组）：一对一已删除，**不静默忽略** ——
        //   一个根就迁移成 `root`；多个根直接报错，让用户自己拆成多个任务。
        if !self.legacy_roots.is_empty() {
            if self.legacy_roots.len() > 1 {
                return Err(Error::Io(format!(
                    "任务「{}」是旧的多根格式（roots = {}）：一对多同步已删除，\
                     请为每个 NAS 文件夹各建一个任务（一个任务 = 一个本地文件夹 + 一个 NAS 文件夹）",
                    self.id,
                    self.legacy_roots.join("、")
                )));
            }
            if self.root.is_none() {
                self.root = Some(crate::roots::normalize_root(&self.legacy_roots[0]));
            }
            // 迁移是一次性的：读进来处理完就丢掉（`skip_serializing` 也保证不写回）
            self.legacy_roots.clear();
        }
        // 归一化这一个根：补前导 /、去尾斜杠；空 = 没指定（用 Qsync 家目录 /home）
        self.root = self
            .root
            .take()
            .map(|r| crate::roots::normalize_root(&r))
            .filter(|r| !r.is_empty());
        Ok(())
    }

    // ------------------------------------------------------------ ★ 一对一（1:1）配对
    //
    // 用户反馈（2026-10-02）：本地 `/home/user/qsync` 配 NAS `/home` 之后，里面又出现了
    // 一层 `home/` —— 那是 M6 的**多根视图**（挂载点当虚拟根，每个远端根一个目录）。
    // 按「一对多直接删除、只留一对一」的要求，任务层现在只有**一个** NAS 文件夹，
    // 挂载点里直接就是它；`mount --remote A --remote B` 这种多根挂载也一并删掉了。

    /// 这一个任务实际生效的 NAS 文件夹：没写 → Qsync 家目录（[`crate::HOME_ROOT`]）。
    pub fn effective_root(&self) -> String {
        self.root
            .as_deref()
            .map(crate::roots::normalize_root)
            .unwrap_or_else(|| crate::HOME_ROOT.to_string())
    }

    /// 本地文件夹的**比较键**：只做词法规范化（`.`/`..`/多余分隔符），
    /// 不碰文件系统 —— 挂载点可能还不存在，`canonicalize` 会失败。
    pub fn local_key(&self) -> PathBuf {
        normalize_local(&self.mountpoint)
    }

    /// ★ 提交前的「目的地冲突」检查（**会拦保存**）：本地这一侧撞车。
    ///
    /// 两类：
    /// 1. **本地文件夹相同** —— 两个任务挂同一个挂载点，必然互相踩；
    /// 2. **本地文件夹嵌套** —— 外层挂载会把内层盖在下面（表现为内层目录消失）。
    ///
    /// 这是硬冲突：一提交就该被拒，别等挂上去才发现。
    pub fn destination_conflicts_with(&self, other: &Task) -> Vec<String> {
        let mut out = Vec::new();
        if other.id == self.id {
            return out;
        }
        let a = self.local_key();
        let b = other.local_key();
        if a == b {
            out.push(format!(
                "本地文件夹 {} 已经分配给任务「{}」（一个本地文件夹只能配一个 NAS 目录）",
                a.display(),
                other.id
            ));
        } else if a.starts_with(&b) || b.starts_with(&a) {
            out.push(format!(
                "本地文件夹 {} 与任务「{}」的 {} 互相嵌套（嵌套挂载会互相遮挡）",
                a.display(),
                other.id,
                b.display()
            ));
        }
        out
    }

    /// NAS 文件夹与别的任务重复 —— **只提示、不拦**。
    ///
    /// 为什么不当错误：只读地把同一个 NAS 文件夹挂到两个本地文件夹是完全合理的用法
    /// （`xtask/tests/m82-matrix.sh` 就靠这个建 t1/t2）。真正的风险是**两边都读写**，
    /// 那由调用方决定怎么呈现；core 只负责把事实说清楚。
    pub fn nas_overlaps_with(&self, other: &Task) -> Vec<String> {
        let mut out = Vec::new();
        if other.id == self.id {
            return out;
        }
        let mine = self.effective_root();
        let theirs = other.effective_root();
        if mine == theirs {
            out.push(format!(
                "NAS 文件夹 {mine} 也配给了任务「{}」{}",
                other.id,
                if self.read_write && other.read_write {
                    "（两个任务都是读写：同一个 NAS 文件夹被双向写会打架）"
                } else {
                    "（只是提示；只读挂载通常没问题）"
                }
            ));
        }
        out
    }

    /// 与一组已有任务比对：返回（**会拦保存的错误**，**只提示的警告**）。
    ///
    /// 调用方负责把「自己」从 `others` 里排除，或依赖 id 去重（同 id 直接跳过）。
    pub fn conflict_report(&self, others: &[Task]) -> (Vec<String>, Vec<String>) {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        for o in others {
            errors.extend(self.destination_conflicts_with(o));
            warnings.extend(self.nas_overlaps_with(o));
        }
        (errors, warnings)
    }

    pub fn load(paths: &ConfigPaths, id: &str) -> Result<Self> {
        if !Self::is_valid_id(id) {
            return Err(Error::Io(format!("任务 id 不合法：{id:?}")));
        }
        let p = Self::file(paths, id);
        let raw = std::fs::read(&p).map_err(|e| Error::Io(format!("读取 {}: {e}", p.display())))?;
        let mut t: Self = serde_json::from_slice(&raw)
            .map_err(|e| Error::Parse(format!("解析 {}: {e}", p.display())))?;
        t.normalize()?;
        Ok(t)
    }

    /// 原子写（临时文件 + fsync + rename），与 link / peers 同一套写法。
    pub fn save(&mut self, paths: &ConfigPaths) -> Result<PathBuf> {
        self.normalize()?;
        let dir = Self::tasks_dir(paths);
        std::fs::create_dir_all(&dir)?;
        restrict_perms(&paths.config_dir, 0o700)?;
        let target = Self::file(paths, &self.id);
        let tmp = target.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(self)?;
        {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &target)?;
        Ok(target)
    }

    /// 列出全部任务（按 id 排序）。解析失败的文件**跳过并返回错误清单**，不整体失败。
    pub fn list(paths: &ConfigPaths) -> (Vec<Task>, Vec<(PathBuf, String)>) {
        let dir = Self::tasks_dir(paths);
        let mut out = Vec::new();
        let mut bad = Vec::new();
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => return (out, bad), // 目录不存在 = 还没有任务
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = p.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            match Self::load(paths, id) {
                Ok(t) => out.push(t),
                Err(err) => bad.push((p.clone(), err.to_string())),
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        (out, bad)
    }

    /// 删除任务文件。**只删登记，不动挂载点里的任何数据。**
    pub fn delete(paths: &ConfigPaths, id: &str) -> Result<bool> {
        if !Self::is_valid_id(id) {
            return Err(Error::Io(format!("任务 id 不合法：{id:?}")));
        }
        let p = Self::file(paths, id);
        if !p.exists() {
            return Ok(false);
        }
        std::fs::remove_file(&p)?;
        Ok(true)
    }

    /// 由 link 合成一个「默认任务」（**迁移用**，不落盘）。
    ///
    /// 仅在**完全没有任务文件**时使用，代表「M8.2 之前那个隐式的同步范围」，
    /// 让界面有东西可展示；`enabled = false` —— 因为它对应的是
    /// 「daemon 起来但还没挂载」的历史状态，**不能**凭空触发一次挂载。
    pub fn legacy_from_link(link: &crate::config::LinkConfig) -> Self {
        Self {
            id: link.id.clone(),
            name: format!("{}（从连接配置推导）", link.id),
            enabled: false,
            mountpoint: PathBuf::new(),
            cache_dir: None,
            // 没显式写 NAS 文件夹 → 用 Qsync 家目录 /home（见 effective_root）
            root: None,
            legacy_roots: Vec::new(),
            read_write: false,
            cache_mode: default_cache_mode(),
            threads: None,
            hydrate_timeout_secs: None,
            delete_limit: None,
            auto_unmount: true,
            direction: default_direction(),
            space_saving: false,
            smart_delete: false,
            selective: Vec::new(),
            exclude: link.exclude.clone(),
            conflict: default_conflict(),
        }
    }

    /// 汇总成一个可读的一行（CLI / 日志用）。
    pub fn summary(&self) -> String {
        format!(
            "{} [{}] {} -> {} · {}",
            self.id,
            if self.enabled { "启用" } else { "停用" },
            self.mountpoint.display(),
            match &self.root {
                Some(r) => r.clone(),
                None => "(默认：Qsync 家目录 /home)".to_string(),
            },
            self.cache_mode
        ) + &match &self.cache_dir {
            Some(c) => format!(" · 缓存 {}", c.display()),
            None => String::new(),
        }
    }
}

/// 词法规范化本地路径（**不访问文件系统**）：去掉 `.`、就地消解 `..`、去掉结尾分隔符。
///
/// 只用于**比较**（目的地冲突检测）：挂载点在保存时可能还不存在，`canonicalize`
/// 会失败；符号链接也不该影响「是不是同一个文件夹」的判断。
pub fn normalize_local(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // 绝对路径到根就停（`/..` = `/`）；相对路径的 `..` 必须留着
                if !out.pop() && !p.is_absolute() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 任务目录里是否有任何任务文件（判断「用户是否已经用过任务模型」）。
pub fn has_any_task(paths: &ConfigPaths) -> bool {
    let dir = Task::tasks_dir(paths);
    match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .flatten()
            .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json")),
        Err(_) => false,
    }
}

/// 是否认为该路径是任务文件（内部工具）。
#[allow(dead_code)]
pub fn is_task_file(p: &Path) -> bool {
    p.extension().and_then(|s| s.to_str()) == Some("json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_paths(tag: &str) -> ConfigPaths {
        let base = std::env::temp_dir().join(format!(
            "qxync-tasks-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        ConfigPaths {
            config_dir: base.join("config"),
            data_dir: base.join("data"),
            state_dir: base.join("state"),
        }
    }

    #[test]
    fn id_validation_blocks_traversal() {
        assert!(Task::is_valid_id("default"));
        assert!(Task::is_valid_id("m6"));
        assert!(Task::is_valid_id("a.b-c_1"));
        for bad in ["", ".", "..", "../etc", "a/b", "a\\b", "有中文"] {
            assert!(!Task::is_valid_id(bad), "{bad:?} 不该通过");
        }
    }

    #[test]
    fn save_load_roundtrip_and_atomic() {
        let p = tmp_paths("rt");
        let mut t = Task::from_mount(
            None,
            PathBuf::from("/tmp/qxync-mnt"),
            Some("/home".into()),
            true,
            Some("direct".into()),
            Some(8),
            Some(120),
            Some(50),
            Some(false),
        );
        let path = t.save(&p).unwrap();
        assert!(path.exists(), "任务文件应落盘");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "临时文件必须已被 rename 掉"
        );

        let got = Task::load(&p, "default").unwrap();
        assert_eq!(got.id, "default");
        assert_eq!(got.mountpoint, PathBuf::from("/tmp/qxync-mnt"));
        assert_eq!(got.root.as_deref(), Some("/home"));
        assert!(got.read_write);
        assert_eq!(got.cache_mode, "direct");
        assert_eq!(got.threads, Some(8));
        assert_eq!(got.hydrate_timeout_secs, Some(120));
        assert_eq!(got.delete_limit, Some(50));
        assert!(!got.auto_unmount);
        assert!(got.enabled, "新建任务默认启用");
    }

    #[test]
    fn list_is_sorted_and_skips_broken_files() {
        let p = tmp_paths("list");
        for id in ["zeta", "alpha", "m6"] {
            let mut t = Task::from_mount(
                Some(id.into()),
                PathBuf::from("/tmp/m"),
                Some("/home".into()),
                false,
                None,
                None,
                None,
                None,
                None,
            );
            t.save(&p).unwrap();
        }
        // 塞一个坏文件：不能把整个 list 打挂
        std::fs::write(Task::file(&p, "broken"), b"{ this is not json").unwrap();
        let (list, bad) = Task::list(&p);
        let ids: Vec<_> = list.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "m6", "zeta"], "按 id 排序");
        assert_eq!(bad.len(), 1, "坏文件应被单独报出来");
    }

    #[test]
    fn delete_only_removes_registration() {
        let p = tmp_paths("del");
        let mut t = Task::from_mount(
            Some("t1".into()),
            PathBuf::from("/tmp/m"),
            Some("/home".into()),
            false,
            None,
            None,
            None,
            None,
            None,
        );
        t.save(&p).unwrap();
        assert!(Task::delete(&p, "t1").unwrap());
        assert!(!Task::delete(&p, "t1").unwrap(), "第二次应是「本来就没有」");
        // 非法 id 不许删
        assert!(Task::delete(&p, "../x").is_err());
    }

    #[test]
    fn space_saving_and_smart_delete_are_mutually_exclusive() {
        // Qsync 原文语义：节省空间模式开启时，智能删除不可用
        let mut t = Task::from_mount(
            None,
            PathBuf::from("/tmp/m"),
            Some("/home".into()),
            false,
            None,
            None,
            None,
            None,
            None,
        );
        t.space_saving = true;
        t.smart_delete = true;
        t.normalize().unwrap();
        assert!(t.space_saving);
        assert!(
            !t.smart_delete,
            "space_saving 开启时必须把 smart_delete 关掉"
        );
    }

    #[test]
    fn bad_enum_values_fall_back_to_defaults() {
        let mut t = Task::from_mount(
            None,
            PathBuf::from("/tmp/m"),
            None,
            false,
            Some("nonsense".into()),
            None,
            None,
            None,
            None,
        );
        t.direction = "sideways".into();
        t.normalize().unwrap();
        assert_eq!(t.cache_mode, CACHE_PAGECACHE);
        assert_eq!(t.direction, DIR_2WAY);
    }

    #[test]
    fn relative_mountpoint_is_rejected() {
        let mut t = Task::from_mount(
            None,
            PathBuf::from("relative/mnt"),
            None,
            false,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(t.normalize().is_err(), "相对路径挂载点必须被拒");
    }

    #[test]
    fn the_one_root_is_normalized() {
        let mut t = Task::from_mount(
            None,
            PathBuf::from("/mnt"),
            Some("  Public/  ".into()),
            false,
            None,
            None,
            None,
            None,
            None,
        );
        t.normalize().unwrap();
        assert_eq!(t.root.as_deref(), Some("/Public"));

        // 不写 NAS 文件夹 → 用默认的 /home
        let mut none = Task::from_mount(
            None,
            PathBuf::from("/mnt"),
            None,
            false,
            None,
            None,
            None,
            None,
            None,
        );
        none.normalize().unwrap();
        assert!(none.root.is_none());
        assert_eq!(none.effective_root(), "/home");
    }

    // ------------------------------------------------------------ ★ 一对一 + 目的地冲突

    /// 多根时代的 `roots` 数组：一个根 → 迁移成 `root`；多个根 → **报错**（不静默缩小范围）。
    #[test]
    fn legacy_roots_array_migrates_or_errors_loudly() {
        let p = tmp_paths("legacy-roots");
        std::fs::create_dir_all(Task::tasks_dir(&p)).unwrap();

        std::fs::write(
            Task::file(&p, "one"),
            r#"{"id":"one","mountpoint":"/tmp/m","roots":["/home/"]}"#,
        )
        .unwrap();
        let t = Task::load(&p, "one").unwrap();
        assert_eq!(t.root.as_deref(), Some("/home"), "单个旧根自动迁移");
        assert!(t.legacy_roots.is_empty(), "迁移后不该再留着旧字段");

        std::fs::write(
            Task::file(&p, "many"),
            r#"{"id":"many","mountpoint":"/tmp/m","roots":["/home","/Public"]}"#,
        )
        .unwrap();
        let e = Task::load(&p, "many").unwrap_err().to_string();
        assert!(e.contains("旧的多根格式"), "{e}");
        assert!(e.contains("/Public"), "错误里要说清是哪几个：{e}");

        // 存回去只写 `root`，不会再写出 `roots`
        let mut t = Task::load(&p, "one").unwrap();
        t.save(&p).unwrap();
        let raw = std::fs::read_to_string(Task::file(&p, "one")).unwrap();
        assert!(raw.contains("\"root\""), "{raw}");
        assert!(!raw.contains("\"roots\""), "旧字段不许再写回：{raw}");
    }

    fn mk(id: &str, mp: &str, root: &str) -> Task {
        Task::from_mount(
            Some(id.into()),
            PathBuf::from(mp),
            Some(root.into()),
            false,
            None,
            None,
            None,
            None,
            None,
        )
    }

    #[test]
    fn destination_conflicts_are_detected() {
        let mine = mk("a", "/home/user/qs", "/home");

        // ① 本地文件夹相同 → 硬冲突
        let same_local = mk("b", "/home/user/qs", "/Public");
        let c = mine.destination_conflicts_with(&same_local);
        assert_eq!(c.len(), 1, "{c:?}");
        assert!(
            c[0].contains("本地文件夹") && c[0].contains("「b」"),
            "{c:?}"
        );

        // ② 本地文件夹嵌套（外层盖内层）→ 硬冲突
        let nested = mk("b", "/home/user/qs/inner", "/Public");
        let c = mine.destination_conflicts_with(&nested);
        assert_eq!(c.len(), 1, "{c:?}");
        assert!(c[0].contains("嵌套"), "{c:?}");

        // ③ NAS 目录相同 → **只警告不拦**（m82 矩阵靠它建 t1/t2；只读挂载合法）
        let same_remote = mk("b", "/home/user/other", "/home");
        assert!(mine.destination_conflicts_with(&same_remote).is_empty());
        let w = mine.nas_overlaps_with(&same_remote);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].contains("NAS 文件夹 /home") && w[0].contains("「b」"),
            "{w:?}"
        );

        // 两个都读写时，警告里要说明「双写打架」
        let mut rw = same_remote.clone();
        rw.read_write = true;
        let mut me_rw = mine.clone();
        me_rw.read_write = true;
        assert!(me_rw.nas_overlaps_with(&rw)[0].contains("双向写"), "{w:?}");
        // 一读一写 → 只提示，不提双写
        assert!(me_rw.nas_overlaps_with(&same_remote)[0].contains("只读挂载"));

        // 自己跟自己不算冲突（编辑保存时最常见）
        assert!(mine.destination_conflicts_with(&mine).is_empty());
        assert!(mine.nas_overlaps_with(&mine).is_empty());
        let (e, w2) =
            mine.conflict_report(&[mine.clone(), same_local.clone(), same_remote.clone()]);
        assert_eq!(e.len(), 1, "两两比：只有 same_local 是硬冲突：{e:?}");
        assert_eq!(w2.len(), 1, "只有 same_remote 是警告：{w2:?}");

        // 互不打扰的另一个任务 → 干净
        let ok = mk("c", "/home/user/other", "/Public");
        let (e, w) = mine.conflict_report(&[ok]);
        assert!(e.is_empty() && w.is_empty(), "{e:?} {w:?}");
    }

    #[test]
    fn destination_conflicts_handle_dots_and_default_home() {
        // `/mnt/./x/..` 与 `/mnt` 是同一个文件夹（词法规范化，不碰文件系统）
        let a = mk("a", "/mnt", "/home");
        let b = mk("b", "/mnt/./x/..", "/Public");
        let c = a.destination_conflicts_with(&b);
        assert_eq!(c.len(), 1, "{c:?}");

        // 没写 NAS 文件夹（`root = None`）= /home → 与显式 /home 重叠
        let mut implicit = mk("b", "/srv/other", "/Public");
        implicit.root = None;
        let (e, w) = a.conflict_report(&[implicit]);
        assert!(e.is_empty(), "挂载点不嵌套 → 没有目的地冲突：{e:?}");
        assert_eq!(w.len(), 1, "没写 NAS 文件夹时语义是 /home，必须提示：{w:?}");

        // 显式配了别的 NAS 文件夹 → 与 /home 不重叠
        let explicit = mk("b", "/srv/other", "/Public");
        let (e, w) = a.conflict_report(&[explicit]);
        assert!(e.is_empty() && w.is_empty(), "{e:?} {w:?}");
    }

    #[test]
    fn normalize_local_is_lexical() {
        assert_eq!(
            normalize_local(Path::new("/a/b/../c")),
            PathBuf::from("/a/c")
        );
        assert_eq!(normalize_local(Path::new("/a/./b/")), PathBuf::from("/a/b"));
        assert_eq!(normalize_local(Path::new("/")), PathBuf::from("/"));
        assert_eq!(normalize_local(Path::new("a/../b")), PathBuf::from("b"));
        assert_eq!(normalize_local(Path::new("../x")), PathBuf::from("../x"));
    }

    #[test]
    fn unknown_fields_are_preserved_forward_compat() {
        // 旧版本写的文件里没有 direction/space_saving 等字段 → 必须能读出来（默认值）
        let p = tmp_paths("compat");
        std::fs::create_dir_all(Task::tasks_dir(&p)).unwrap();
        let legacy = r#"{"id":"old","mountpoint":"/tmp/m","roots":["/home"]}"#;
        std::fs::write(Task::file(&p, "old"), legacy).unwrap();
        let t = Task::load(&p, "old").unwrap();
        assert_eq!(
            t.root.as_deref(),
            Some("/home"),
            "旧的单个 roots 迁移成 root"
        );
        assert_eq!(t.name, "old", "name 缺省回填 id");
        assert!(t.enabled, "enabled 缺省 true");
        assert_eq!(t.cache_mode, CACHE_PAGECACHE);
        assert_eq!(t.direction, DIR_2WAY);
        assert!(!t.read_write);
    }

    #[test]
    fn legacy_from_link_is_disabled_and_carries_excludes() {
        let link = crate::config::LinkConfig {
            id: "default".into(),
            host: "nas".into(),
            port: 9834,
            https: true,
            insecure: true,
            user: "u".into(),
            ipv4_only: false,
            exclude: vec!["*.iso".into()],
            filter_temp: true,
            peer_listen: None,
            peer_name: None,
        };
        let t = Task::legacy_from_link(&link);
        assert_eq!(t.id, "default");
        assert!(!t.enabled, "推导出来的任务必须是停用的，不能凭空触发挂载");
        assert!(t.root.is_none(), "没显式 NAS 文件夹 → 用默认 /home");
        assert_eq!(t.effective_root(), "/home");
        assert_eq!(t.exclude, vec!["*.iso".to_string()]);
    }

    #[test]
    fn cache_dir_roundtrip_and_validation() {
        let p = tmp_paths("cache");
        let mut t = Task::from_mount(
            Some("c1".into()),
            PathBuf::from("/tmp/m"),
            Some("/home".into()),
            false,
            None,
            None,
            None,
            None,
            None,
        )
        .with_cache_dir(Some(PathBuf::from("/data/qxync-cache")));
        t.save(&p).unwrap();
        let got = Task::load(&p, "c1").unwrap();
        assert_eq!(
            got.cache_dir,
            Some(PathBuf::from("/data/qxync-cache")),
            "缓存目录必须能往返（否则任务方式挂载改不了缓存位置）"
        );

        // 相对路径必须被拒
        let mut bad = Task::from_mount(
            Some("c2".into()),
            PathBuf::from("/tmp/m"),
            None,
            false,
            None,
            None,
            None,
            None,
            None,
        )
        .with_cache_dir(Some(PathBuf::from("relative/cache")));
        assert!(bad.normalize().is_err(), "相对缓存目录必须被拒");

        // 默认 None
        let d = Task::from_mount(
            None,
            PathBuf::from("/tmp/m"),
            None,
            false,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(d.cache_dir, None);
    }

    #[test]
    fn default_task_is_unsavable() {
        // Task::default() 是 serde 占位值，**必须**过不了校验，防止被误存成真任务
        let mut t = Task::default();
        assert!(!t.enabled);
        assert!(t.normalize().is_err(), "空挂载点的占位任务不许通过校验");
    }

    #[test]
    fn has_any_task_detects_registration() {
        let p = tmp_paths("any");
        assert!(!has_any_task(&p), "目录还不存在时 = 没有任务");
        let mut t = Task::from_mount(
            None,
            PathBuf::from("/tmp/m"),
            None,
            false,
            None,
            None,
            None,
            None,
            None,
        );
        t.save(&p).unwrap();
        assert!(has_any_task(&p));
    }
}
