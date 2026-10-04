//! ★ M15/T5：挂载状态落盘 —— daemon 重启后自动恢复挂载，用户无感。
//!
//! ## 为什么不用 `tasks/`
//!
//! M8.2 的 [`crate::tasks::Task`] 是**用户意图**（「这个共享文件夹该自动挂」），
//! 存在 `~/.config/qxync/tasks/<id>.json`。但 `qxync mount` 手工挂载、没登记
//! `save_task` 的挂载点**不在任务表里**，daemon 一重启就凭空消失 —— 这正是
//! T5 要消灭的现象。所以两份数据职责不同、都要有：
//!
//! | 文件 | 记什么 | 位置 |
//! | --- | --- | --- |
//! | `tasks/<id>.json` | 用户登记了哪些共享文件夹（意图） | config_dir |
//! | `mounts.json` | **上一次实际挂载了什么**（现场） | state_dir |
//!
//! 恢复时**两份都读**，现场优先（它记的是真正跑过的参数，任务表可能早就改了）。
//!
//! ## 故意不存的字段
//!
//! * **`sid`** —— 会话每次挂载重新登录（daemon 侧 `mount()` 本就 `set_sid`）。
//!   存下来等于把过期令牌写进磁盘。
//! * **`Client` / 节点表** —— 靠懒加载重建（`lookup` 三级回退），落盘反而会过期。
//! * **`cache_dir`** —— 存的是**父目录**，daemon 会再拼一层 NAS 主机名；
//!   存主机名进去等于把「当前连的是哪台 NAS」也固化下来，换 link 就错位。
//!
//! ## 损坏的降级原则
//!
//! **解析失败一律当「没有 mounts.json」**（[`MountsFile::load`] 返回 `None`），
//! 绝不让 daemon 起不来（跟 [`crate::config`] 里 `adopt_legacy_dir` 迁移失败
//! 只打 WARN 是同一个态度）。用户大不了重启后重新 `qxync mount`，比 daemon
//! 直接起不来划算得多。警告文案由 `load` 一并返回 —— core 本身不引 `tracing`。

use crate::config::ConfigPaths;
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 落盘格式版本。将来字段变了 +1，旧文件读不出来时按「不恢复」处理（见模块文档）。
pub const MOUNTS_VERSION: u32 = 1;

/// 一个挂载点的**可复现参数**（够 `mount()` 再挂一次，够了）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountRecord {
    /// 本地挂载点（绝对路径，落盘前 canonicalize 过）。
    pub mountpoint: PathBuf,
    /// 这个挂载点对应的 NAS 文件夹（一对一），如 `/home`、`/Public`。
    pub remote: String,
    /// 读写挂载（`false` = 只读）。
    #[serde(default)]
    pub read_write: bool,
    /// `pagecache` / `direct`。
    #[serde(default)]
    pub cache_mode: String,
    /// 冲突策略（5 选项，见 [`crate::tasks::CONFLICTS`]）。
    #[serde(default)]
    pub conflict: String,
    /// FUSE 读线程数。
    #[serde(default)]
    pub threads: usize,
    /// 单个文件的水合超时（秒）。
    #[serde(default)]
    pub hydrate_timeout_secs: u64,
    /// 删除熔断上限（`None` = 不限）。
    #[serde(default)]
    pub delete_limit: Option<usize>,
    /// FUSE 会话结束时不自动卸载。
    #[serde(default = "yes")]
    pub auto_unmount: bool,
}

fn yes() -> bool {
    true
}

/// `mounts.json` 的顶层结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountsFile {
    pub version: u32,
    #[serde(default)]
    pub mounts: Vec<MountRecord>,
}

impl Default for MountsFile {
    fn default() -> Self {
        Self {
            version: MOUNTS_VERSION,
            mounts: Vec::new(),
        }
    }
}

impl MountsFile {
    /// 落盘位置：`$XDG_STATE_HOME/qxync/mounts.json`。
    ///
    /// ⚠️ `qxync` 这一层**不能省** —— `state_dir` 是 XDG 规范里 daemon 的公共地盘，
    /// 与 `link.json` / `credentials.json` 那套「config_dir 下按类型分子目录」
    /// 的布局一致（见 [`ConfigPaths::log_dir`]）。`load` 与 `save` 都必须走这里，
    /// 两边路径写死不一致的话，`save` 成功而 `load` 永远读不到（静默失效）。
    pub fn file(paths: &ConfigPaths) -> PathBuf {
        paths.state_dir.join("qxync").join("mounts.json")
    }

    /// 读 `mounts.json`。**任何失败都当「没有」**（返回 `(None, [])`），由调用方决定降级。
    ///
    /// 刻意不让失败变成 `Err`：这个文件的失败模式全是「磁盘上那份读不懂」，
    /// 而对它的正确反应永远只有一个（不恢复），没有让调用方处理错误的余地。
    ///
    /// 第二个返回值是**给人看的警告文案**（core 本身不引 `tracing`，与
    /// [`crate::tasks::Task::list`] 同风格）：解析失败不能静默 —— 用户会以为
    /// 自己的挂载配置凭空丢了。文件不存在不算警告（第一次用而已）。
    pub fn load(paths: &ConfigPaths) -> (Option<Self>, Vec<String>) {
        let p = Self::file(paths);
        let raw = match std::fs::read(&p) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (None, Vec::new()),
            Err(e) => {
                return (
                    None,
                    vec![format!("读挂载记录失败（本次不恢复挂载）{}: {e}", p.display())],
                )
            }
        };
        match serde_json::from_slice::<Self>(&raw) {
            Ok(f) if f.version == MOUNTS_VERSION => (Some(f), Vec::new()),
            Ok(f) => (
                None,
                vec![format!(
                    "挂载记录版本是 {}（本机认 {MOUNTS_VERSION}），本次不恢复: {}",
                    f.version,
                    p.display()
                )],
            ),
            Err(e) => (
                None,
                vec![format!(
                    "挂载记录解析失败（本次不恢复挂载）{}: {e}",
                    p.display()
                )],
            ),
        }
    }

    /// 原子写（临时文件 + `fsync` + `rename`），与 `tasks` / `link` 同一套写法。
    ///
    /// 临时文件用 `.json.tmp` 后缀（`with_extension`）—— 与 `Task::save` 一致，
    /// 这样中途崩了留下的临时文件也一眼认得出是什么。
    pub fn save(&self, paths: &ConfigPaths) -> Result<PathBuf> {
        // 目标路径一律问 `file()`，不自己拼 —— 两边各拼一次迟早会对不上
        let target = Self::file(paths);
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = target.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(self)
            .map_err(|e| Error::Io(format!("序列化挂载记录失败: {e}")))?;
        {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &target)?;
        Ok(target)
    }

    /// 按挂载点 upsert（`mount()` 成功后调用）。
    pub fn put(&mut self, r: MountRecord) {
        match self
            .mounts
            .iter_mut()
            .find(|m| m.mountpoint == r.mountpoint)
        {
            Some(slot) => *slot = r,
            None => self.mounts.push(r),
        }
    }

    /// 移除一个挂载点（`umount()` 成功后调用）。
    ///
    /// 返回是否真的移掉了 —— 卸载成功但记录里没有，说明这份记录本来就不一致
    /// （比如别人手改了文件），调用方可以顺手把整个文件重写一遍归正。
    pub fn remove(&mut self, mountpoint: &Path) -> bool {
        let n = self.mounts.len();
        self.mounts.retain(|m| m.mountpoint != mountpoint);
        self.mounts.len() != n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{CACHE_PAGECACHE, CONFLICT_RENAME_LOCAL};

    fn tmpdir(tag: &str) -> ConfigPaths {
        // 每个用例一个独立目录；`target/tmp` 下（`/tmp` 只有 10M tmpfs，见 M12 记忆）
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp/mounts-test")
            .join(tag);
        let _ = std::fs::remove_dir_all(&base);
        // `MountsFile::file` 落在 `state_dir/qxync/`（照 XDG 约定），先把两级都建出来
        std::fs::create_dir_all(base.join("state/qxync")).unwrap();
        ConfigPaths {
            config_dir: base.join("config"),
            data_dir: base.join("data"),
            state_dir: base.join("state"),
        }
    }

    /// `load` 的便捷包装：只要「读到了没有」，警告文案在需要时再单独取。
    fn load_ok(p: &ConfigPaths) -> Option<MountsFile> {
        p_load(p).0
    }

    fn p_load(p: &ConfigPaths) -> (Option<MountsFile>, Vec<String>) {
        MountsFile::load(p)
    }

    fn rec(mp: &str) -> MountRecord {
        MountRecord {
            mountpoint: mp.into(),
            remote: "/home".into(),
            read_write: true,
            cache_mode: CACHE_PAGECACHE.into(),
            conflict: CONFLICT_RENAME_LOCAL.into(),
            threads: 4,
            hydrate_timeout_secs: 60,
            delete_limit: None,
            auto_unmount: true,
        }
    }

    #[test]
    fn roundtrip_keeps_every_field() {
        let p = tmpdir("roundtrip");
        let mut f = MountsFile::default();
        let mut r = rec("/home/kami/qxync");
        r.delete_limit = Some(3);
        f.put(r.clone());
        f.save(&p).unwrap();

        let back = load_ok(&p).expect("应能读回");
        assert_eq!(back.version, MOUNTS_VERSION);
        assert_eq!(back.mounts, vec![r]);
    }

    #[test]
    fn put_replaces_same_mountpoint_instead_of_appending() {
        let mut f = MountsFile::default();
        f.put(rec("/a"));
        let mut r = rec("/a");
        r.remote = "/Public".into();
        f.put(r);
        assert_eq!(f.mounts.len(), 1, "同一挂载点不该出现两条");
        assert_eq!(f.mounts[0].remote, "/Public", "应是后者覆盖前者");
    }

    #[test]
    fn put_different_mountpoints_both_kept() {
        let mut f = MountsFile::default();
        f.put(rec("/a"));
        f.put(rec("/b"));
        assert_eq!(f.mounts.len(), 2);
    }

    #[test]
    fn remove_reports_whether_it_removed_something() {
        let mut f = MountsFile::default();
        f.put(rec("/a"));
        assert!(f.remove(Path::new("/a")), "存在的应报告移掉了");
        assert!(!f.remove(Path::new("/a")), "已不在的不该报告移掉了");
        assert!(f.mounts.is_empty());
    }

    /// ★ 验收第 5 条：`mounts.json` 损坏时 daemon 仍能正常启动（降级为不恢复）。
    #[test]
    fn corrupt_file_degrades_to_none() {
        let p = tmpdir("corrupt");
        std::fs::write(MountsFile::file(&p), b"{ not json at all ").unwrap();
        let (f, warns) = MountsFile::load(&p);
        assert!(f.is_none(), "损坏文件必须降级为不恢复");
        assert_eq!(warns.len(), 1, "且必须给出警告（不能静默丢用户的挂载配置）");
    }

    /// 版本对不上也是「不恢复」，而不是按老字段硬读 —— 那样会挂出参数不对的挂载点。
    #[test]
    fn unknown_version_degrades_to_none() {
        let p = tmpdir("badver");
        let mut f = MountsFile::default();
        f.put(rec("/a"));
        f.save(&p).unwrap();
        let mut v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(MountsFile::file(&p)).unwrap()).unwrap();
        v["version"] = serde_json::json!(MOUNTS_VERSION + 1);
        std::fs::write(MountsFile::file(&p), serde_json::to_vec(&v).unwrap()).unwrap();
        let (f, warns) = MountsFile::load(&p);
        assert!(f.is_none());
        assert_eq!(warns.len(), 1);
    }

    /// 文件不存在 = 第一次用，不是错误（`load` 返回 `(None, [])`，调用方按「没挂过」处理）。
    #[test]
    fn missing_file_is_not_an_error() {
        let p = tmpdir("missing");
        let (f, warns) = MountsFile::load(&p);
        assert!(f.is_none());
        assert!(warns.is_empty(), "第一次用不该报警告");
    }

    /// 只读的旧文件也得能读出来 —— `read_write` 等字段都有 `#[serde(default)]`。
    #[test]
    fn minimal_record_gets_defaults() {
        let p = tmpdir("minimal");
        let body = br#"{"version":1,"mounts":[{"mountpoint":"/a","remote":"/home"}]}"#;
        std::fs::write(MountsFile::file(&p), body).unwrap();
        let (f, warns) = MountsFile::load(&p);
        assert!(warns.is_empty(), "最小记录不该警告: {warns:?}");
        let f = f.expect("最小记录应能读");
        let m = &f.mounts[0];
        assert!(!m.read_write, "缺省应是只读（与 M1–M7 一致）");
        assert!(m.auto_unmount, "缺省应自动卸载");
        assert_eq!(m.threads, 0, "缺省 0 = 让 daemon 用自己的默认（mount_task 那侧会兜）");
    }

    /// 写盘是「先 tmp 再 rename」：崩在中间不会让主文件变成半截 JSON。
    #[test]
    fn save_is_atomic_via_tmp_then_rename() {
        let p = tmpdir("atomic");
        let mut f = MountsFile::default();
        f.put(rec("/a"));
        let target = f.save(&p).unwrap();
        assert!(target.exists(), "目标文件应在");
        let tmp = target.with_extension("json.tmp");
        assert!(!tmp.exists(), "rename 成功后临时文件不该留下");
    }
}
