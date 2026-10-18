//! # M19：删除相关数据（purge）
//!
//! 旧版只有「删除任务登记」和「移除连接」两个动作，而且两者都**刻意不碰任何数据**：
//!
//! * `Task::delete()` 只删 `tasks/<id>.json`；
//! * `LinkConfig::delete()` 只删 link / 凭据 / peers；
//! * `link_delete` 的注释里写死了「不删本地文件、不删 NAS 数据、不动 cache/state」。
//!
//! 这个边界本身是对的（删登记 ≠ 删数据），但它没有**出口**：缓存、
//! `sync/<ns>/sync.db`（baseline / 日志 / 待裁决）、`upload-queue/<ns>/`、按天滚动的
//! 运行日志会一直留在本机。用户重加同名任务时还会读到旧 baseline，
//! 「删干净重来」这件事在界面上做不出来。
//!
//! 本模块就是那个出口，分三个作用域：
//!
//! | 作用域 | 删什么 | 明确不删 |
//! |---|---|---|
//! | `task` | 任务登记、该任务的水合缓存、`baseline` / `journal` / `decisions` 里落在该 NAS 文件夹子树的行 | 挂载点目录与其中文件、NAS 数据、按连接共享的上传/删除队列 |
//! | `link` | 该连接的任务登记、link / 凭据 / peers、`sync/<ns>`、`cache/<ns>`、`upload-queue/<ns>`、挂载记录、该连接的日志 / socket / pid | 挂载点目录与其中文件、NAS 数据 |
//! | `all` | 本机 qxync 的配置目录、数据目录、状态目录，以及各连接的 socket / pid | 挂载点目录、NAS 数据、系统 autostart 项 |
//!
//! ## 三条安全约束
//!
//! 1. **先出计划、再执行**（[`PurgePlan`]）：GUI 把「将删除哪些路径、多少字节、
//!    多少状态库行」摊开让用户确认；计划里每一项都带一个**允许根**
//!    （[`PurgeItem::base`]，不序列化给前端），执行时逐项校验目标仍在允许根内。
//! 2. **不跟随符号链接**：`symlink_metadata` 判类型，符号链接只删链接本身；
//!    `remove_dir_all` 也不会穿过子目录里的符号链接。
//! 3. **共享数据不猜**：任务的水合缓存目录 / NAS 文件夹如果和本连接的另一条任务
//!    共用，对应项**不删**并给出警告 —— 宁可少删，也不替另一条任务做决定。
//!
//! ## 与 daemon 的分工
//!
//! 任务级 purge 必须由 daemon 执行（它持有挂载表与 `SyncState` 内存副本）：
//! 先卸载、再删库行、最后把内存里的 baseline 条目摘掉并 `save_all()`，
//! 否则下一轮 `persist` 会把刚删的 baseline 原样写回来。
//! 连接级 / 全局 purge 需要先停 daemon，由调用方停完之后直接走本模块。

use crate::config::{link_namespace, ConfigPaths, LinkConfig};
use crate::error::{Error, Result};
use crate::store::Store;
use crate::tasks::Task;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// 作用域：一条任务。
pub const SCOPE_TASK: &str = "task";
/// 作用域：一条连接。
pub const SCOPE_LINK: &str = "link";
/// 作用域：本机全部 qxync 数据。
pub const SCOPE_ALL: &str = "all";

/// 这三个 `kind` 的允许根**可以等于路径本身**（全局 purge 就是要删整个根目录）。
const GLOBAL_KINDS: [&str; 3] = ["config", "data", "state"];

/// 状态库里「按远端路径删」的行数（任务级 purge 的预览 / 结果）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeDbRows {
    pub baseline: u64,
    pub journal: u64,
    pub decisions: u64,
}

impl PurgeDbRows {
    pub fn total(&self) -> u64 {
        self.baseline + self.journal + self.decisions
    }
}

/// 计划里的一条路径。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeItem {
    /// 绝对路径（展示 + 删除对象）。
    pub path: String,
    /// `task` / `cache` / `sync` / `queue` / `mounts` / `log` / `socket` / `pid` /
    /// `link` / `credentials` / `peers` / `config` / `data` / `state`。
    pub kind: String,
    /// 给界面看的中文名。
    pub label: String,
    pub is_dir: bool,
    /// 计划时的体积（删除后因文件变化可能有偏差，只用于展示）。
    pub bytes: u64,
    pub files: u64,
    /// `true` = **只有勾选「同时删除相关数据」才删**；`false` = 这个动作本来就会删。
    pub optional: bool,
    /// 执行时允许删除的根（安全边界）。**不序列化给前端**。
    #[serde(skip)]
    pub base: PathBuf,
}

/// 一次「删除相关数据」的完整计划（先预览、再执行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurgePlan {
    /// `task` / `link` / `all`。
    pub scope: String,
    /// 任务 id / 连接 id；`all` 时为 `all`。
    pub target: String,
    /// 所属连接 id（`all` 时为空串）。
    pub link_id: String,
    /// 给界面看的标题（如「任务「home」」）。
    pub title: String,
    pub items: Vec<PurgeItem>,
    /// 任务级才有：状态库将清理的行数；连接级 / 全局为 `null`（整个库目录一起删）。
    pub db_rows: Option<PurgeDbRows>,
    pub total_bytes: u64,
    pub total_files: u64,
    /// 其中 `optional = true` 的部分（勾选框上显示的「约 X MB」用它）。
    pub optional_bytes: u64,
    pub optional_files: u64,
    pub warnings: Vec<String>,
    pub notes: Vec<String>,
}

/// 执行结果（`purged` 之外的数字都是**实际删掉**的）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurgeOutcome {
    pub scope: String,
    pub target: String,
    pub purged: bool,
    pub deleted_paths: Vec<String>,
    pub bytes: u64,
    pub files: u64,
    pub db_rows: Option<PurgeDbRows>,
    pub warnings: Vec<String>,
    pub notes: Vec<String>,
}

impl PurgePlan {
    /// 供调用方（daemon / GUI 命令）判断「这是不是空计划」。
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.db_rows.is_none()
    }
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "task" => "任务登记",
        "cache" => "水合缓存",
        "sync" => "同步状态库",
        "queue" => "上传 / 删除队列",
        "mounts" => "挂载记录",
        "log" => "运行日志",
        "socket" => "IPC socket",
        "pid" => "pid 文件",
        "link" => "连接配置",
        "credentials" => "登录凭据",
        "peers" => "LAN 配对凭据",
        "config" => "配置目录",
        "data" => "数据目录",
        "state" => "状态目录",
        _ => "文件",
    }
}

/// `paths` 是否就是本机真实的配置目录（而不是测试 / 隔离环境里手搭的临时目录）。
///
/// 只有真实环境才把 `XDG_RUNTIME_DIR/qxync/` 下的 socket 与 pid 文件算进计划 ——
/// 那两条路径是 [`crate::ipc`] 按**进程环境**推导的，与传进来的 `paths` 无关；
/// 在临时目录里跑单测/验收时把它们算进来会去统计（甚至删除）本机正在跑的 daemon
/// 的运行态文件，这正是「测试去动用户真实数据」那类事故。
fn real_runtime(paths: &ConfigPaths) -> bool {
    ConfigPaths::discover()
        .map(|d| {
            d.config_dir == paths.config_dir
                && d.data_dir == paths.data_dir
                && d.state_dir == paths.state_dir
        })
        .unwrap_or(false)
}

fn plan_new(scope: &str, target: &str, link_id: &str, title: String) -> PurgePlan {
    PurgePlan {
        scope: scope.into(),
        target: target.into(),
        link_id: link_id.into(),
        title,
        items: Vec::new(),
        db_rows: None,
        total_bytes: 0,
        total_files: 0,
        optional_bytes: 0,
        optional_files: 0,
        warnings: Vec::new(),
        notes: Vec::new(),
    }
}

fn finish(mut p: PurgePlan) -> PurgePlan {
    p.total_bytes = p.items.iter().map(|i| i.bytes).sum();
    p.total_files = p.items.iter().map(|i| i.files).sum();
    p.optional_bytes = p.items.iter().filter(|i| i.optional).map(|i| i.bytes).sum();
    p.optional_files = p.items.iter().filter(|i| i.optional).map(|i| i.files).sum();
    p
}

/// 统计路径的字节数 / 文件数。**不跟随符号链接**（链接本身算 1 项、0 字节）。
///
/// 目录项按文件名排序后遍历——计划是确定性的：同样输入跑两次输出一致
/// （验收脚本会对比两次 `--dry-run --json`）。
fn measure(path: &Path, warnings: &mut Vec<String>) -> (u64, u64) {
    let md = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (0, 0),
        Err(e) => {
            warnings.push(format!("无法读取 {}：{e}", path.display()));
            return (0, 0);
        }
    };
    let ft = md.file_type();
    if ft.is_symlink() {
        return (0, 1);
    }
    if ft.is_file() {
        return (md.len(), 1);
    }
    if !ft.is_dir() {
        return (0, 1);
    }
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(path) {
        Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
        Err(e) => {
            warnings.push(format!("无法读取目录 {}：{e}", path.display()));
            return (0, 0);
        }
    };
    entries.sort();
    let (mut bytes, mut files) = (0u64, 0u64);
    for child in entries {
        let (b, f) = measure(&child, warnings);
        bytes += b;
        files += f;
    }
    (bytes, files)
}

fn push_item(p: &mut PurgePlan, base: &Path, path: PathBuf, kind: &str, optional: bool) {
    let md = std::fs::symlink_metadata(&path);
    let is_dir = md
        .as_ref()
        .map(|m| m.is_dir() && !m.file_type().is_symlink())
        .unwrap_or(false);
    let (bytes, files) = measure(&path, &mut p.warnings);
    p.items.push(PurgeItem {
        path: path.display().to_string(),
        kind: kind.into(),
        label: kind_label(kind).into(),
        is_dir,
        bytes,
        files,
        optional,
        base: base.to_path_buf(),
    });
}

/// `path` 是否落在 `root` 子树内（含 `root` 自身，纯字面比较，不解析符号链接）。
pub fn under_root(path: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        return path.starts_with('/');
    }
    path == root || path.starts_with(&format!("{root}/"))
}

/// 纯字面归一化：解析 `.` / `..`，不碰文件系统（目标可能已经不存在）。
///
/// 为什么不做 `canonicalize`：它要求路径存在，而「删除」的路径本就是为了让它
/// 不存在；而且 `canonicalize` 会解析符号链接，那正是这里**不**想要的。
fn normalize_lexical(p: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::Normal(x) => out.push(x),
        }
    }
    out.is_absolute().then_some(out)
}

/// 路径是否在允许根内；拒绝非绝对路径、`..` 逃逸与 `/`。
fn inside(base: &Path, path: &Path) -> bool {
    let (Some(base), Some(path)) = (normalize_lexical(base), normalize_lexical(path)) else {
        return false;
    };
    if path == Path::new("/") {
        return false;
    }
    path == base || path.starts_with(base)
}

// ---------------------------------------------------------------- 计划

/// 任务级计划。
///
/// `db` 是**这个连接的**状态库（daemon 传它自己持有的 `Store`；GUI/CLI 用
/// [`Store::open_readonly`] 打开）。传 `None` 表示状态库不存在 —— 此时
/// `db_rows` 也是 `None`（没有库就没有行可删）。
pub fn plan_task(
    paths: &ConfigPaths,
    link: &LinkConfig,
    task: &Task,
    other_tasks: &[Task],
    db: Option<&Store>,
) -> PurgePlan {
    let mut p = plan_new(
        SCOPE_TASK,
        &task.id,
        &link.id,
        format!("任务「{}」", task.id),
    );
    let ns = link_namespace(link);

    // ① 任务登记：无论勾不勾选都删（这就是「删除任务」本身）。
    push_item(
        &mut p,
        &paths.config_dir,
        Task::file(paths, &task.id),
        "task",
        false,
    );

    // ② 水合缓存：M18 起新建任务默认独占；老任务 / 手改过的可能与其他任务共用。
    match &task.cache_dir {
        Some(parent) => {
            let dir = parent.join(&ns);
            let shared = other_tasks
                .iter()
                .filter(|o| o.id != task.id)
                .any(|o| o.cache_dir.as_ref().map(|q| q.join(&ns)) == Some(dir.clone()));
            if shared {
                p.warnings.push(format!(
                    "水合缓存 {} 与本连接的另一条任务共用，未列入删除",
                    dir.display()
                ));
            } else {
                // 缓存目录允许用户自定义到数据目录之外：允许根取「它自己所在目录」。
                let base = if inside(&paths.data_dir, &dir) {
                    paths.data_dir.clone()
                } else {
                    dir.parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| dir.clone())
                };
                push_item(&mut p, &base, dir, "cache", true);
            }
        }
        None => p
            .warnings
            .push("这条任务没有单独设置水合缓存（按连接共享），未列入删除".into()),
    }

    // ③ 状态库里落在该 NAS 文件夹子树的行。
    let root = task.effective_root();
    let shared_root = other_tasks
        .iter()
        .filter(|o| o.id != task.id)
        .any(|o| o.effective_root() == root);
    if shared_root {
        p.warnings.push(format!(
            "NAS 文件夹 {root} 同时被本连接的另一条任务使用：baseline / 日志 / 待裁决未列入删除"
        ));
    } else if let Some(db) = db {
        match task_rows(db, &root) {
            Ok(rows) => {
                if rows.total() > 0 {
                    p.db_rows = Some(rows);
                }
            }
            Err(e) => p
                .warnings
                .push(format!("状态库读取失败，未统计可清理的行：{e}")),
        }
    }

    p.notes.push("挂载点目录与其中的文件保留".into());
    p.notes.push("NAS 上的数据不受影响".into());
    p.notes.push("上传 / 删除队列按连接共享，未删除".into());
    finish(p)
}

/// 连接级计划。调用方应**先停 daemon** 再执行（`purge` 的路径里包含它正在写的库）。
pub fn plan_link(paths: &ConfigPaths, link: &LinkConfig, tasks: &[Task]) -> PurgePlan {
    let mut p = plan_new(
        SCOPE_LINK,
        &link.id,
        &link.id,
        format!("连接「{}」", link.id),
    );

    // 这些是「移除连接」本来就会删的（与旧 link_delete 一致）。
    push_item(
        &mut p,
        &paths.config_dir,
        paths.link_file(&link.id),
        "link",
        false,
    );
    push_item(
        &mut p,
        &paths.config_dir,
        paths.link_credentials_file(&link.id),
        "credentials",
        false,
    );
    push_item(
        &mut p,
        &paths.config_dir,
        paths.peers_file(&link.id),
        "peers",
        false,
    );
    for t in tasks {
        push_item(
            &mut p,
            &paths.config_dir,
            Task::file(paths, &t.id),
            "task",
            false,
        );
    }

    // 这些是新增的「相关数据」。
    push_item(
        &mut p,
        &paths.data_dir,
        paths.sync_dir_for(link),
        "sync",
        true,
    );
    push_item(
        &mut p,
        &paths.data_dir,
        paths.cache_dir_for(link),
        "cache",
        true,
    );
    push_item(
        &mut p,
        &paths.data_dir,
        paths.queue_dir_for(link),
        "queue",
        true,
    );
    push_item(
        &mut p,
        &paths.state_dir,
        paths.mounts_file_for(link),
        "mounts",
        true,
    );

    // 按连接分文件的运行日志（`qxyncd-<id>.log.*`；default 是 `qxyncd.log.*`）。
    let prefix = crate::ipc::log_prefix_for(&link.id);
    let log_dir = paths.log_dir();
    if let Ok(rd) = std::fs::read_dir(&log_dir) {
        let mut logs: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix))
            })
            .collect();
        logs.sort();
        for log in logs {
            push_item(&mut p, &log_dir, log, "log", true);
        }
    }

    // 运行态文件：daemon 正常退出会自己收掉；这里兜底清理残留。
    // 只在真实环境里收集（见 real_runtime 的注释）。
    if real_runtime(paths) {
        for (kind, path) in [
            ("socket", crate::ipc::socket_path_for(&link.id)),
            ("pid", crate::ipc::pid_path_for(&link.id)),
        ] {
            let base = path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| paths.state_dir.clone());
            push_item(&mut p, &base, path, kind, true);
        }
    }

    if tasks.is_empty() {
        p.notes.push("这条连接没有任务登记".into());
    }
    p.notes.push("挂载点目录与其中的文件保留".into());
    p.notes.push("NAS 上的数据不受影响".into());
    finish(p)
}

/// 全局计划：本机所有 qxync 数据（恢复出厂）。调用方必须先停掉所有 daemon。
pub fn plan_all(paths: &ConfigPaths, links: &[LinkConfig]) -> PurgePlan {
    let mut p = plan_new(SCOPE_ALL, "all", "", "本机所有 qxync 数据".into());
    for (kind, path) in [
        ("config", paths.config_dir.clone()),
        ("data", paths.data_dir.clone()),
        ("state", paths.state_dir.clone()),
    ] {
        push_item(&mut p, &path, path.clone(), kind, true);
    }
    if real_runtime(paths) {
        for l in links {
            for (kind, path) in [
                ("socket", crate::ipc::socket_path_for(&l.id)),
                ("pid", crate::ipc::pid_path_for(&l.id)),
            ] {
                let base = path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| paths.state_dir.clone());
                push_item(&mut p, &base, path, kind, true);
            }
        }
    }
    p.warnings
        .push("删除后本机会变回「从未配置过」的状态；请在执行前确认已停止所有 daemon".into());
    p.notes.push("挂载点目录与其中的文件保留".into());
    p.notes.push("NAS 上的数据不受影响".into());
    p.notes
        .push("系统 autostart 项不删除（需要的话到「设置 → 高级」关掉）".into());
    finish(p)
}

// ---------------------------------------------------------------- 状态库行

/// 统计 `root` 子树在 baseline / journal / decisions 里的行数。
pub fn task_rows(store: &Store, root: &str) -> Result<PurgeDbRows> {
    Ok(PurgeDbRows {
        baseline: store.count_path_rows_under("baseline", root)?,
        journal: store.count_path_rows_under("journal", root)?,
        decisions: store.count_path_rows_under("decisions", root)?,
    })
}

/// 删除 `root` 子树在 baseline / journal / decisions 里的行。
///
/// ⚠️ daemon 调用后必须同步清理内存里的 `SyncState.baseline` 再 `save_all()`。
pub fn delete_task_rows(store: &Store, root: &str) -> Result<PurgeDbRows> {
    Ok(PurgeDbRows {
        baseline: store.delete_path_rows_under("baseline", root)? as u64,
        journal: store.delete_path_rows_under("journal", root)? as u64,
        decisions: store.delete_path_rows_under("decisions", root)? as u64,
    })
}

// ---------------------------------------------------------------- 执行

/// 执行计划：只删计划里列出、且仍落在各自允许根内的路径。
///
/// 单个路径失败**不中断**整体（记进 `warnings` 继续下一个）；不存在的路径
/// 直接跳过（「移除连接」的流程里任务登记可能已经被 `LinkConfig::delete` 先删掉）。
pub fn execute(plan: &PurgePlan) -> PurgeOutcome {
    let mut out = PurgeOutcome {
        scope: plan.scope.clone(),
        target: plan.target.clone(),
        purged: true,
        deleted_paths: Vec::new(),
        bytes: 0,
        files: 0,
        db_rows: plan.db_rows,
        warnings: Vec::new(),
        notes: plan.notes.clone(),
    };
    for item in &plan.items {
        let path = Path::new(&item.path);
        if !inside(&item.base, path) {
            out.warnings.push(format!(
                "拒绝删除 {}：不在允许根 {} 内",
                item.path,
                item.base.display()
            ));
            continue;
        }
        // 只有全局作用域允许直接删「允许根本身」（config/data/state 三个目录）。
        if path == item.base && !GLOBAL_KINDS.contains(&item.kind.as_str()) {
            out.warnings.push(format!(
                "拒绝删除 {}：它是允许根本身且不属于全局类别",
                item.path
            ));
            continue;
        }
        match remove_path(path) {
            Ok(true) => {
                out.deleted_paths.push(item.path.clone());
                out.bytes += item.bytes;
                out.files += item.files;
            }
            Ok(false) => {}
            Err(e) => out.warnings.push(format!("删除 {} 失败：{e}", item.path)),
        }
    }
    out
}

/// 删一个路径：不存在返回 `Ok(false)`；符号链接只删链接本身。
fn remove_path(path: &Path) -> std::io::Result<bool> {
    let md = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if md.file_type().is_symlink() || !md.is_dir() {
        std::fs::remove_file(path)?;
        return Ok(true);
    }
    std::fs::remove_dir_all(path)?;
    Ok(true)
}

/// 从挂载记录里移除一个挂载点（任务级 purge 兜底：任务没挂载时 `umount` 不会跑）。
pub fn forget_mount_record(paths: &ConfigPaths, link: &LinkConfig, mountpoint: &Path) -> bool {
    let (file, _warns) = crate::mounts::MountsFile::load_for(paths, link);
    let Some(mut f) = file else {
        return false;
    };
    // 记录里的路径是 canonicalize 过的；两种形态都试一次。
    let canon = std::fs::canonicalize(mountpoint).unwrap_or_else(|_| mountpoint.to_path_buf());
    let removed = f.remove(&canon) || f.remove(mountpoint);
    if removed {
        let _ = f.save_for(paths, link);
    }
    removed
}

/// 把 `link_delete` 结果里「实际删了哪些文件」汇总成 JSON 友好的对象。
pub fn outcome_json(out: &PurgeOutcome) -> serde_json::Value {
    serde_json::json!({
        "bytes": out.bytes,
        "files": out.files,
        "deleted_paths": out.deleted_paths,
        "db_rows": out.db_rows,
        "warnings": out.warnings,
    })
}

/// 计划里 `optional = true` 的项（GUI 用它渲染「仅勾选后删除」的清单）。
pub fn optional_items(plan: &PurgePlan) -> Vec<&PurgeItem> {
    plan.items.iter().filter(|i| i.optional).collect()
}

// ---------------------------------------------------------------- 校验

/// 计划 / 结果里是否包含「不允许的路径」（供调用方在删除前再自查一次）。
pub fn validate_plan(plan: &PurgePlan) -> Result<()> {
    for item in &plan.items {
        let path = Path::new(&item.path);
        if !inside(&item.base, path) {
            return Err(Error::Io(format!(
                "计划项 {} 不在允许根 {} 内",
                item.path,
                item.base.display()
            )));
        }
        if path == item.base && !GLOBAL_KINDS.contains(&item.kind.as_str()) {
            return Err(Error::Io(format!(
                "计划项 {} 是允许根本身，且不属于全局类别",
                item.path
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::link_namespace;
    use crate::store::{DecisionRow, JournalEntry};
    use crate::sync::Baseline;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn tmp_paths(tag: &str) -> ConfigPaths {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp/purge-test")
            .join(format!("{tag}-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        ConfigPaths {
            config_dir: base.join("config"),
            data_dir: base.join("data"),
            state_dir: base.join("state"),
        }
    }

    fn link(id: &str, host: &str) -> LinkConfig {
        LinkConfig {
            id: id.into(),
            host: host.into(),
            port: 9834,
            https: true,
            insecure: false,
            user: "u".into(),
            ipv4_only: false,
            exclude: vec![],
            filter_temp: true,
            peer_listen: None,
            peer_name: None,
        }
    }

    fn task(id: &str, root: &str, cache: Option<PathBuf>) -> Task {
        Task {
            id: id.into(),
            link_id: None,
            name: id.into(),
            enabled: true,
            mountpoint: PathBuf::from(format!("/home/u/qxync/{id}")),
            cache_dir: cache,
            root: Some(root.into()),
            legacy_roots: vec![],
            read_write: false,
            cache_mode: "pagecache".into(),
            threads: None,
            hydrate_timeout_secs: None,
            delete_limit: None,
            auto_unmount: true,
            direction: "2way".into(),
            space_saving: false,
            smart_delete: false,
            selective: vec![],
            exclude: vec![],
            conflict: "rename_local".into(),
        }
    }

    fn write_file(p: &Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn plan_task_covers_cache_and_db_rows_but_keeps_queue_out() {
        let paths = tmp_paths("plan-task");
        let l = link("default", "nas.local");
        let ns = link_namespace(&l);
        let cache_parent = paths.data_dir.join("cache/tasks/default/home");
        let t = task("home", "/home", Some(cache_parent.clone()));
        let other = task("public", "/Public", None);
        write_file(
            &Task::file(&paths, "home"),
            r#"{"id":"home","mountpoint":"/x"}"#,
        );
        write_file(&cache_parent.join(&ns).join("a.bin"), "123456");
        write_file(&cache_parent.join(&ns).join("dir/b.bin"), "12");

        let store = Store::open_in_memory().unwrap();
        let mut b = Baseline::default();
        b.put("/home/a.bin", crate::sync::Sig::file(6, 1));
        b.put("/home/dir/b.bin", crate::sync::Sig::file(2, 1));
        b.put("/Public/c.bin", crate::sync::Sig::file(1, 1));
        store.save_baseline(&b).unwrap();
        store
            .journal_add_batch(&[
                JournalEntry::ok("upload", "/home/a.bin", "ok"),
                JournalEntry::ok("upload", "/Public/c.bin", "ok"),
            ])
            .unwrap();
        let mut d = DecisionRow {
            path: "/home/a.bin".into(),
            ..Default::default()
        };
        d.id = DecisionRow::id_for("/home/a.bin");
        store.decision_upsert(&d).unwrap();

        let plan = plan_task(&paths, &l, &t, std::slice::from_ref(&other), Some(&store));
        let kinds: Vec<&str> = plan.items.iter().map(|i| i.kind.as_str()).collect();
        assert_eq!(kinds, vec!["task", "cache"]);
        assert!(!plan.items[0].optional, "任务登记是「删除任务」本身要删的");
        assert!(plan.items[1].optional, "缓存是勾选后才删的");
        assert_eq!(
            plan.items[1].path,
            cache_parent.join(&ns).display().to_string()
        );
        // total 含任务登记文件本身；勾选框只按 optional 部分显示
        assert_eq!(plan.optional_bytes, 8);
        assert_eq!(
            plan.total_bytes,
            8 + std::fs::metadata(Task::file(&paths, "home")).unwrap().len()
        );
        let rows = plan.db_rows.unwrap();
        assert_eq!(
            rows,
            PurgeDbRows {
                baseline: 2,
                journal: 1,
                decisions: 1
            }
        );
        assert!(plan.notes.iter().any(|n| n.contains("挂载点")));
    }

    #[test]
    fn plan_task_skips_shared_cache_and_shared_root() {
        let paths = tmp_paths("plan-shared");
        let l = link("default", "nas.local");
        let ns = link_namespace(&l);
        let cache = paths.data_dir.join("cache/shared");
        let t = task("a", "/home", Some(cache.clone()));
        let other = task("b", "/home", Some(cache.clone()));
        let plan = plan_task(&paths, &l, &t, std::slice::from_ref(&other), None);
        assert!(
            plan.items.iter().all(|i| i.kind != "cache"),
            "共用缓存不能删"
        );
        assert!(plan.db_rows.is_none(), "共用 NAS 根时不能删状态库行");
        assert!(plan.warnings.iter().any(|w| w.contains("共用")));
        assert!(plan.warnings.iter().any(|w| w.contains("同时被")));
        // 缓存目录确实没进计划
        assert!(!plan
            .items
            .iter()
            .any(|i| i.path.contains(&format!("cache/shared/{ns}"))));
    }

    #[test]
    fn execute_removes_planned_paths_and_leaves_others() {
        let paths = tmp_paths("exec");
        let l = link("default", "nas.local");
        let ns = link_namespace(&l);
        let cache_parent = paths.data_dir.join("cache/tasks/default/home");
        let t = task("home", "/home", Some(cache_parent.clone()));
        let keep = paths.data_dir.join("cache/other.bin");
        write_file(&Task::file(&paths, "home"), "{}");
        write_file(&cache_parent.join(&ns).join("a.bin"), "123456");
        write_file(&keep, "keep");

        let plan = plan_task(&paths, &l, &t, &[], None);
        let out = execute(&plan);
        assert!(out.purged);
        assert!(!Task::file(&paths, "home").exists());
        assert!(!cache_parent.join(&ns).exists());
        assert!(keep.exists(), "计划外的路径一个都不许动");
        assert_eq!(out.files, 2, "任务登记 + 缓存里的 1 个文件");
        assert_eq!(out.bytes, 8);
    }

    #[test]
    fn execute_refuses_paths_outside_allow_root() {
        let paths = tmp_paths("escape");
        let l = link("default", "nas.local");
        let t = task("home", "/home", None);
        let mut plan = plan_task(&paths, &l, &t, &[], None);
        let victim = paths.config_dir.parent().unwrap().join("victim.txt");
        write_file(&victim, "must survive");
        plan.items.push(PurgeItem {
            path: victim.display().to_string(),
            kind: "cache".into(),
            label: "伪造".into(),
            is_dir: false,
            bytes: 1,
            files: 1,
            optional: true,
            base: paths.data_dir.clone(),
        });
        let out = execute(&plan);
        assert!(victim.exists(), "计划项越界时拒绝删除");
        assert!(out.warnings.iter().any(|w| w.contains("不在允许根")));
        assert!(validate_plan(&plan).is_err());
    }

    #[test]
    fn execute_does_not_follow_symlinks() {
        let paths = tmp_paths("symlink");
        let l = link("default", "nas.local");
        let ns = link_namespace(&l);
        let outside = paths.data_dir.parent().unwrap().join("outside.txt");
        write_file(&outside, "outside");
        let cache_parent = paths.data_dir.join("cache/tasks/default/home");
        let dir = cache_parent.join(&ns);
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("link-to-outside")).unwrap();
        let t = task("home", "/home", Some(cache_parent));
        let plan = plan_task(&paths, &l, &t, &[], None);
        execute(&plan);
        assert!(outside.exists(), "符号链接的目标绝不能被删");
    }

    #[test]
    fn plan_link_lists_all_per_link_data() {
        let paths = tmp_paths("plan-link");
        let l = link("default", "nas.local");
        let ns = link_namespace(&l);
        write_file(&paths.link_file("default"), "{}");
        write_file(&paths.sync_dir_for(&l).join("sync.db"), "db");
        write_file(&paths.cache_dir_for(&l).join("x.bin"), "123");
        write_file(&paths.queue_dir_for(&l).join("queue.db"), "q");
        write_file(&paths.log_dir().join("qxyncd.log.2026-10-18"), "log");
        let t = task("home", "/home", None);
        write_file(&Task::file(&paths, "home"), "{}");
        let plan = plan_link(&paths, &l, std::slice::from_ref(&t));
        let kinds: Vec<&str> = plan.items.iter().map(|i| i.kind.as_str()).collect();
        for want in [
            "link",
            "credentials",
            "peers",
            "task",
            "sync",
            "cache",
            "queue",
            "mounts",
            "log",
        ] {
            assert!(kinds.contains(&want), "缺少 {want}：{kinds:?}");
        }
        let opt: Vec<&str> = plan
            .items
            .iter()
            .filter(|i| i.optional)
            .map(|i| i.kind.as_str())
            .collect();
        assert!(!opt.contains(&"link"), "连接配置本来就要删");
        assert!(opt.contains(&"sync") && opt.contains(&"cache") && opt.contains(&"queue"));
        // sync.db=2 + cache=3 + queue=1 + log=3（mounts/socket/pid 不存在时为 0）
        assert_eq!(plan.optional_bytes, 2 + 3 + 1 + 3);
        assert!(
            plan.items
                .iter()
                .any(|i| i.kind == "log" && i.path.contains("qxyncd.log")),
            "日志按连接前缀收集"
        );
        // 计划是确定性的：跑两次完全一致
        let again = plan_link(&paths, &l, std::slice::from_ref(&t));
        assert_eq!(plan.items, again.items);
        let _ = ns;
    }

    #[test]
    fn plan_all_covers_three_roots_and_executes() {
        let paths = tmp_paths("plan-all");
        let l = link("nas2", "nas2.local");
        write_file(&paths.config_dir.join("settings.json"), "s");
        write_file(&paths.data_dir.join("sync/x/sync.db"), "db");
        write_file(&paths.state_dir.join("log/x.log"), "log");
        let plan = plan_all(&paths, std::slice::from_ref(&l));
        let kinds: Vec<&str> = plan.items.iter().map(|i| i.kind.as_str()).collect();
        assert!(kinds.contains(&"config") && kinds.contains(&"data") && kinds.contains(&"state"));
        assert!(plan.items.iter().all(|i| i.optional));
        let out = execute(&plan);
        assert!(!paths.config_dir.exists());
        assert!(!paths.data_dir.exists());
        assert!(!paths.state_dir.exists());
        assert_eq!(out.files, 3);
    }

    #[test]
    fn delete_task_rows_only_touches_the_root_subtree() {
        let store = Store::open_in_memory().unwrap();
        let mut b = Baseline::default();
        for p in ["/home/a", "/home/b", "/homepage/c", "/Public/d"] {
            b.put(p, crate::sync::Sig::file(1, 1));
        }
        store.save_baseline(&b).unwrap();
        store
            .journal_add_batch(&[
                JournalEntry::ok("upload", "/home/a", "ok"),
                JournalEntry::ok("upload", "/Public/d", "ok"),
            ])
            .unwrap();
        assert_eq!(task_rows(&store, "/home").unwrap().baseline, 2);
        assert_eq!(task_rows(&store, "/home").unwrap().journal, 1);
        let rows = delete_task_rows(&store, "/home").unwrap();
        assert_eq!(
            rows,
            PurgeDbRows {
                baseline: 2,
                journal: 1,
                decisions: 0
            }
        );
        let left = store.baseline().unwrap();
        assert_eq!(left.len(), 2, "homepage / Public 不能被 /home 前缀误删");
        assert!(left.get("/homepage/c").exists);
        assert_eq!(store.journal_count().unwrap(), 1);
    }

    #[test]
    fn open_readonly_does_not_create_missing_db() {
        let paths = tmp_paths("readonly");
        let missing = paths.data_dir.join("sync/none/sync.db");
        assert!(Store::open_readonly(&missing).unwrap().is_none());
        assert!(!missing.exists(), "预览不许在磁盘上造一个空库");
    }
}
