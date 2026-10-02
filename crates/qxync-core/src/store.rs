//! SQLite 状态库（M5）——`<data>/sync/<host>/sync.db`
//!
//! 承载**同步正确性的核心状态**（报告 09 §9.6/§9.7）：
//!
//! | 表 | 内容 | M5 之前在哪 |
//! |---|---|---|
//! | `cursors` | 三个事件游标 + `max_log_seen` + `log_missing_count` | `cursors.json` |
//! | `baseline` | 路径 → 上次同步成功时的签名（`exists/is_dir/size/mtime`） | `baseline.json` |
//! | `pins` | 路径 → pin 状态 | **只在内存**（daemon 重启就丢） |
//!
//! 为什么值得换掉 JSON：
//!
//! 1. **游标与 baseline 必须在同一次提交里落盘**：两者分两次 `rename` 时，崩在中间会出现
//!    「游标推到了 N，baseline 还停在 N-1」——下一轮会把已处理过的事件再处理一遍，
//!    或者反过来漏事件。SQLite 用**一个事务**覆盖 [`Store::save_state`]。
//! 2. **pin 必须活过重启**：M3 的脱水安全检查依赖 `pin`（`pinned`/`excluded` 不脱水），
//!    以前 daemon 一重启 pin 全丢 → 安全检查静默失守。
//! 3. 顺带拿到 `PRAGMA integrity_check` 这种可验收的自证手段。
//!
//! 注意：这里**不做**「内存模型改成按需查询」——`Baseline`/`Cursors` 仍是工作副本，
//! 只是持久层从 JSON 换成 SQLite（调用方代码几乎不动，风险最低）。

use crate::error::{Error, Result};
use crate::sync::{Baseline, Cursors, Sig, BASELINE_FILE, CURSORS_FILE};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 状态库文件名（每个 NAS 一个：`<data>/sync/<host>/sync.db`）。
pub const DB_FILE: &str = "sync.db";

/// schema 版本（写进 `PRAGMA user_version`；将来加表要迁移时用它判断）。
pub const SCHEMA_VERSION: i64 = 3;
// ★ M8.3：v1 → v2 只是**新增 journal 表与索引**。
// ★ M8.4：v2 → v3 只是**新增 decisions 表（冲突待裁决队列）**。
// 因为整份 SCHEMA_SQL 都是
// `CREATE TABLE/INDEX IF NOT EXISTS`，老库在下次 `Store::open()` 时会被自动补齐，
// **不需要写迁移代码、也不会碰已有表里的数据**（迁移幂等由 m5-matrix 断言覆盖）。

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS cursors (
  id                INTEGER PRIMARY KEY CHECK (id = 1),
  config            INTEGER NOT NULL DEFAULT 0,
  notify            INTEGER NOT NULL DEFAULT 0,
  global_notify     INTEGER NOT NULL DEFAULT 0,
  max_log_seen      INTEGER NOT NULL DEFAULT 0,
  log_missing_count INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS baseline (
  path    TEXT PRIMARY KEY,
  present INTEGER NOT NULL,
  is_dir  INTEGER NOT NULL,
  size    INTEGER NOT NULL,
  mtime   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS pins (
  path  TEXT PRIMARY KEY,
  state TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS uploads (
  remote_path TEXT PRIMARY KEY,
  remote_dir  TEXT NOT NULL,
  remote_name TEXT NOT NULL,
  local       TEXT NOT NULL,
  mtime       INTEGER NOT NULL DEFAULT 0,
  attempts    INTEGER NOT NULL DEFAULT 0,
  ephemeral   INTEGER NOT NULL DEFAULT 0
);
-- ★ M8.3：同步活动日志。GUI 的「文件更新中心 / 错误列表」读的就是它。
-- 写入侧**必须批量**（见 Store::journal_add_batch）：同步/上传热路径不许逐条事务。
CREATE TABLE IF NOT EXISTS journal (
  id      INTEGER PRIMARY KEY AUTOINCREMENT,
  ts      INTEGER NOT NULL,
  task_id TEXT    NOT NULL DEFAULT '',
  kind    TEXT    NOT NULL,
  path    TEXT    NOT NULL DEFAULT '',
  detail  TEXT    NOT NULL DEFAULT '',
  status  TEXT    NOT NULL DEFAULT 'ok',
  bytes   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS journal_ts     ON journal(ts DESC);
CREATE INDEX IF NOT EXISTS journal_status ON journal(status);
CREATE INDEX IF NOT EXISTS journal_kind   ON journal(kind);
-- ★ M8.4：冲突策略 =「每个文件都问我」时的待裁决队列。
-- 只有这一个策略会往里写；裁决完（引擎执行后）删行。
CREATE TABLE IF NOT EXISTS decisions (
  id            TEXT PRIMARY KEY,
  path          TEXT NOT NULL UNIQUE,
  task_id       TEXT NOT NULL DEFAULT '',
  is_dir        INTEGER NOT NULL DEFAULT 0,
  local_size    INTEGER NOT NULL DEFAULT 0,
  local_mtime   INTEGER NOT NULL DEFAULT 0,
  remote_size   INTEGER NOT NULL DEFAULT 0,
  remote_mtime  INTEGER NOT NULL DEFAULT 0,
  created_unix  INTEGER NOT NULL DEFAULT 0,
  resolution    TEXT
);
CREATE INDEX IF NOT EXISTS decisions_created ON decisions(created_unix DESC);
CREATE INDEX IF NOT EXISTS decisions_path    ON decisions(path);
"#;

/// 上传队列里的一行（M5 起队列也进状态库）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadRow {
    pub remote_dir: String,
    pub remote_name: String,
    pub local: PathBuf,
    pub mtime: i64,
    pub attempts: u32,
    pub ephemeral: bool,
}

impl UploadRow {
    pub fn remote_path(&self) -> String {
        format!(
            "{}/{}",
            self.remote_dir.trim_end_matches('/'),
            self.remote_name
        )
    }
}

/// ★ M8.4：冲突待裁决队列的一行（`decisions` 表）。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DecisionRow {
    pub id: String,
    /// 远端绝对路径。
    pub path: String,
    #[serde(default)]
    pub task_id: String,
    #[serde(default)]
    pub is_dir: bool,
    #[serde(default)]
    pub local_size: u64,
    #[serde(default)]
    pub local_mtime: i64,
    #[serde(default)]
    pub remote_size: u64,
    #[serde(default)]
    pub remote_mtime: i64,
    #[serde(default)]
    pub created_unix: i64,
    /// `None` = 待裁决；`keep_local` / `keep_remote` / `keep_both`。
    #[serde(default)]
    pub resolution: Option<String>,
}

impl DecisionRow {
    /// 稳定 id：**只用路径**（同一路径只应有一条待裁决）。
    ///
    /// 不用散列是为了让 `qsync conflicts --json` 的输出可读、可 diff；
    /// 路径里可能有 `/`，所以做一层转义。
    pub fn id_for(path: &str) -> String {
        path.trim_start_matches('/')
            .replace('/', "_")
            .chars()
            .take(120)
            .collect()
    }
    /// 已裁决且待执行。
    pub fn is_resolved(&self) -> bool {
        self.resolution.is_some()
    }
}

/// ★ M8.3：一条同步活动记录（`journal` 表的一行）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JournalEntry {
    /// unix 秒。
    pub ts: i64,
    /// 关联的任务 id（M8.2 起有任务；旧调用点填空字符串）。
    #[serde(default)]
    pub task_id: String,
    pub kind: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub detail: String,
    /// `ok` / `error` / `blocked`。
    #[serde(default = "journal_ok")]
    pub status: String,
    #[serde(default)]
    pub bytes: i64,
}

fn journal_ok() -> String {
    "ok".to_string()
}

impl JournalEntry {
    pub fn new(
        kind: &str,
        path: impl Into<String>,
        detail: impl Into<String>,
        status: &str,
    ) -> Self {
        Self {
            ts: now_unix(),
            task_id: String::new(),
            kind: kind.to_string(),
            path: path.into(),
            detail: detail.into(),
            status: status.to_string(),
            bytes: 0,
        }
    }

    pub fn ok(kind: &str, path: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(kind, path, detail, "ok")
    }

    pub fn error(kind: &str, path: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(kind, path, detail, "error")
    }

    pub fn blocked(kind: &str, path: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(kind, path, detail, "blocked")
    }

    pub fn with_bytes(mut self, b: i64) -> Self {
        self.bytes = b;
        self
    }

    pub fn with_task(mut self, id: impl Into<String>) -> Self {
        self.task_id = id.into();
        self
    }
}

/// 当前 unix 秒（失败回 0，不 panic）。
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 从 JSON 迁移的结果（验收脚本会读它）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MigrateReport {
    /// 是否导入了 `cursors.json`。
    pub cursors: bool,
    /// 从 `baseline.json` 导入了多少条。
    pub baseline: usize,
    /// 被改名为 `*.migrated` 的旧文件（保留备份，不删）。
    pub archived: Vec<PathBuf>,
}

impl MigrateReport {
    pub fn did_something(&self) -> bool {
        self.cursors || self.baseline > 0 || !self.archived.is_empty()
    }
}

/// 一个 NAS 的状态库。
pub struct Store {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl Store {
    /// 打开（必要时创建）状态库；父目录会自动创建。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path).map_err(Error::from)?;
        Self::init(conn, path)
    }

    /// 内存库（单测用）。
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(Error::from)?;
        Self::init(conn, PathBuf::from(":memory:"))
    }

    fn init(conn: Connection, path: PathBuf) -> Result<Self> {
        // WAL：读写不互斥；FULL：游标/事务提交必须真的落盘（同步正确性 > 吞吐）。
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(Error::from)?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(Error::from)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(Error::from)?;
        conn.execute_batch(SCHEMA_SQL).map_err(Error::from)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(Error::from)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 句柄（中毒也继续用：状态库没有「半改坏」的概念，事务保证了一致性）。
    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn schema_version(&self) -> Result<i64> {
        let v: i64 = self
            .conn()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(Error::from)?;
        Ok(v)
    }

    /// 自证手段：`PRAGMA integrity_check` 返回 `ok` 才算库是好的。
    pub fn integrity_check(&self) -> Result<String> {
        let v: String = self
            .conn()
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(Error::from)?;
        Ok(v)
    }

    // ------------------------------------------------------------ 游标

    pub fn cursors(&self) -> Result<Cursors> {
        let row = self
            .conn()
            .query_row(
                "SELECT config, notify, global_notify, max_log_seen, log_missing_count
                   FROM cursors WHERE id = 1",
                [],
                |r| {
                    Ok(Cursors {
                        config: r.get(0)?,
                        notify: r.get(1)?,
                        global_notify: r.get(2)?,
                        max_log_seen: r.get::<_, i64>(3)? as u64,
                        log_missing_count: r.get::<_, i64>(4)? as u64,
                    })
                },
            )
            .optional()
            .map_err(Error::from)?;
        Ok(row.unwrap_or_default())
    }

    pub fn save_cursors(&self, c: &Cursors) -> Result<()> {
        write_cursors(&self.conn(), c)
    }

    // ------------------------------------------------------------ baseline

    pub fn baseline(&self) -> Result<Baseline> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT path, present, is_dir, size, mtime FROM baseline")
            .map_err(Error::from)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Sig {
                        exists: r.get::<_, i64>(1)? != 0,
                        is_dir: r.get::<_, i64>(2)? != 0,
                        size: r.get::<_, i64>(3)? as u64,
                        mtime: r.get(4)?,
                    },
                ))
            })
            .map_err(Error::from)?;
        let mut b = Baseline::default();
        for row in rows {
            let (path, sig) = row.map_err(Error::from)?;
            b.put(path, sig);
        }
        Ok(b)
    }

    pub fn baseline_len(&self) -> Result<u64> {
        let n: i64 = self
            .conn()
            .query_row("SELECT COUNT(*) FROM baseline", [], |r| r.get(0))
            .map_err(Error::from)?;
        Ok(n as u64)
    }

    pub fn save_baseline(&self, b: &Baseline) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(Error::from)?;
        write_baseline(&tx, b)?;
        tx.commit().map_err(Error::from)?;
        Ok(())
    }

    /// ★ 游标 + baseline **同一个事务**落盘：崩溃后要么都旧、要么都新。
    pub fn save_state(&self, c: &Cursors, b: &Baseline) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(Error::from)?;
        write_cursors(&tx, c)?;
        write_baseline(&tx, b)?;
        tx.commit().map_err(Error::from)?;
        Ok(())
    }

    // ------------------------------------------------------------ pin

    pub fn pins(&self) -> Result<BTreeMap<String, String>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT path, state FROM pins")
            .map_err(Error::from)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(Error::from)?;
        let mut m = BTreeMap::new();
        for row in rows {
            let (p, s) = row.map_err(Error::from)?;
            m.insert(p, s);
        }
        Ok(m)
    }

    // ------------------------------------------------------------ ★ M8.3 同步日志（journal）

    /// 一条同步活动记录。
    ///
    /// `status`：`ok`（成功）/ `error`（失败，错误列表读的就是它）/ `blocked`（被安全策略挡下）。
    /// `kind` 是自由字符串，当前用到：`remote_change` / `upload` / `download` / `conflict`
    /// / `delete` / `dehydrate` / `scan`。
    pub fn journal_add_batch(&self, entries: &[JournalEntry]) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(Error::from)?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO journal (ts, task_id, kind, path, detail, status, bytes)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .map_err(Error::from)?;
            for e in entries {
                stmt.execute(rusqlite::params![
                    e.ts, e.task_id, e.kind, e.path, e.detail, e.status, e.bytes
                ])
                .map_err(Error::from)?;
            }
        }
        tx.commit().map_err(Error::from)?;
        Ok(entries.len())
    }

    /// 查日志。`since` 是 unix 秒下界（闭区间）；`query` 在 path/detail 上做子串匹配；
    /// `level = Some("error")` 只返回失败项（GUI 的「错误列表」用）。
    pub fn journal_list(
        &self,
        limit: usize,
        since: Option<i64>,
        query: Option<&str>,
        level: Option<&str>,
    ) -> Result<Vec<JournalEntry>> {
        let conn = self.conn();
        let mut sql = String::from(
            "SELECT ts, task_id, kind, path, detail, status, bytes FROM journal WHERE 1=1",
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(s) = since {
            sql.push_str(" AND ts >= ?");
            args.push(Box::new(s));
        }
        if let Some(l) = level {
            if l == "error" {
                sql.push_str(" AND status = 'error'");
            } else if l == "blocked" {
                sql.push_str(" AND status = 'blocked'");
            } else if l == "ok" {
                sql.push_str(" AND status = 'ok'");
            }
        }
        if let Some(q) = query {
            if !q.trim().is_empty() {
                let like = format!("%{}%", q.trim());
                sql.push_str(" AND (path LIKE ? OR detail LIKE ?)");
                args.push(Box::new(like.clone()));
                args.push(Box::new(like));
            }
        }
        sql.push_str(" ORDER BY id DESC LIMIT ?");
        args.push(Box::new(limit.clamp(1, 10_000) as i64));

        let mut stmt = conn.prepare(&sql).map_err(Error::from)?;
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let rows = stmt
            .query_map(refs.as_slice(), |r| {
                Ok(JournalEntry {
                    ts: r.get(0)?,
                    task_id: r.get(1)?,
                    kind: r.get(2)?,
                    path: r.get(3)?,
                    detail: r.get(4)?,
                    status: r.get(5)?,
                    bytes: r.get(6)?,
                })
            })
            .map_err(Error::from)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(Error::from)?);
        }
        Ok(out)
    }

    pub fn journal_count(&self) -> Result<i64> {
        self.conn()
            .query_row("SELECT COUNT(*) FROM journal", [], |r| r.get(0))
            .map_err(Error::from)
    }

    /// 按状态计数（`ok` / `error` / `blocked`），给 GUI 的角标用。
    pub fn journal_counts(&self) -> Result<std::collections::BTreeMap<String, i64>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT status, COUNT(*) FROM journal GROUP BY status")
            .map_err(Error::from)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(Error::from)?;
        let mut m = std::collections::BTreeMap::new();
        for row in rows {
            let (k, v) = row.map_err(Error::from)?;
            m.insert(k, v);
        }
        Ok(m)
    }

    /// 清空日志（**只清日志，不碰游标/baseline/pin/队列**）。
    pub fn journal_clear(&self) -> Result<usize> {
        let n = self
            .conn()
            .execute("DELETE FROM journal", [])
            .map_err(Error::from)?;
        Ok(n)
    }

    /// 轮转：先按天数删，再按条数删。返回删掉的行数。
    ///
    /// **必须有上限**：同步日志是无限增长型数据，不加约束会把 `sync.db` 撑大。
    pub fn journal_trim(&self, max_rows: i64, max_age_days: i64) -> Result<usize> {
        let (max_rows, max_age_days) = (max_rows.max(0), max_age_days.max(0));
        let mut n = 0usize;
        if max_age_days > 0 {
            let cutoff = crate::store::now_unix() - max_age_days * 86_400;
            n += self
                .conn()
                .execute(
                    "DELETE FROM journal WHERE ts < ?1",
                    rusqlite::params![cutoff],
                )
                .map_err(Error::from)?;
        }
        if max_rows > 0 {
            // 保留最新 max_rows 条
            n += self
                .conn()
                .execute(
                    "DELETE FROM journal WHERE id NOT IN (
                        SELECT id FROM journal ORDER BY id DESC LIMIT ?1
                     )",
                    rusqlite::params![max_rows],
                )
                .map_err(Error::from)?;
        }
        Ok(n)
    }

    pub fn pin(&self, path: &str) -> Result<Option<String>> {
        self.conn()
            .query_row(
                "SELECT state FROM pins WHERE path = ?1",
                params![path],
                |r| r.get(0),
            )
            .optional()
            .map_err(Error::from)
    }

    pub fn set_pin(&self, path: &str, state: &str) -> Result<()> {
        self.conn()
            .execute(
                "INSERT INTO pins(path, state) VALUES(?1, ?2)
                 ON CONFLICT(path) DO UPDATE SET state = excluded.state",
                params![path, state],
            )
            .map_err(Error::from)?;
        Ok(())
    }

    pub fn remove_pin(&self, path: &str) -> Result<bool> {
        let n = self
            .conn()
            .execute("DELETE FROM pins WHERE path = ?1", params![path])
            .map_err(Error::from)?;
        Ok(n > 0)
    }

    // ------------------------------------------------------------ ★ M8.4 冲突待裁决队列

    /// 记一条待裁决冲突。
    ///
    /// **同一个路径只保留一条**（`ON CONFLICT(path)`）—— 否则每一轮同步都会把
    /// 同一个冲突再插一遍，队列会被噪声撑爆。`id` 取路径的稳定散列。
    /// 已裁决（`resolution` 非空）的条目再次冲突时**保留裁决**（用户已经表过态）。
    pub fn decision_upsert(&self, d: &DecisionRow) -> Result<()> {
        self.conn()
            .execute(
                "INSERT INTO decisions
                   (id, path, task_id, is_dir, local_size, local_mtime,
                    remote_size, remote_mtime, created_unix, resolution)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                 ON CONFLICT(path) DO UPDATE SET
                    task_id      = excluded.task_id,
                    is_dir       = excluded.is_dir,
                    local_size   = excluded.local_size,
                    local_mtime  = excluded.local_mtime,
                    remote_size  = excluded.remote_size,
                    remote_mtime = excluded.remote_mtime",
                params![
                    d.id,
                    d.path,
                    d.task_id,
                    d.is_dir as i64,
                    d.local_size as i64,
                    d.local_mtime,
                    d.remote_size as i64,
                    d.remote_mtime,
                    d.created_unix,
                    d.resolution,
                ],
            )
            .map_err(Error::from)?;
        Ok(())
    }

    /// 队列全量（新的在前）。
    pub fn decisions(&self) -> Result<Vec<DecisionRow>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, path, task_id, is_dir, local_size, local_mtime,
                        remote_size, remote_mtime, created_unix, resolution
                   FROM decisions ORDER BY created_unix DESC, path",
            )
            .map_err(Error::from)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(DecisionRow {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    task_id: r.get(2)?,
                    is_dir: r.get::<_, i64>(3)? != 0,
                    local_size: r.get::<_, i64>(4)? as u64,
                    local_mtime: r.get(5)?,
                    remote_size: r.get::<_, i64>(6)? as u64,
                    remote_mtime: r.get(7)?,
                    created_unix: r.get(8)?,
                    resolution: r.get(9)?,
                })
            })
            .map_err(Error::from)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(Error::from)?);
        }
        Ok(out)
    }

    /// 按路径取一条（引擎每轮查一次）。
    pub fn decision_by_path(&self, path: &str) -> Result<Option<DecisionRow>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, path, task_id, is_dir, local_size, local_mtime,
                        remote_size, remote_mtime, created_unix, resolution
                   FROM decisions WHERE path = ?1",
            )
            .map_err(Error::from)?;
        let mut rows = stmt
            .query_map(params![path], |r| {
                Ok(DecisionRow {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    task_id: r.get(2)?,
                    is_dir: r.get::<_, i64>(3)? != 0,
                    local_size: r.get::<_, i64>(4)? as u64,
                    local_mtime: r.get(5)?,
                    remote_size: r.get::<_, i64>(6)? as u64,
                    remote_mtime: r.get(7)?,
                    created_unix: r.get(8)?,
                    resolution: r.get(9)?,
                })
            })
            .map_err(Error::from)?;
        match rows.next() {
            Some(r) => Ok(Some(r.map_err(Error::from)?)),
            None => Ok(None),
        }
    }

    /// 裁决一条；返回是否命中（`resolution` 为 `None` = 退回未裁决）。
    pub fn decision_resolve(&self, id: &str, resolution: Option<&str>) -> Result<bool> {
        let n = self
            .conn()
            .execute(
                "UPDATE decisions SET resolution = ?2 WHERE id = ?1",
                params![id, resolution],
            )
            .map_err(Error::from)?;
        Ok(n > 0)
    }

    /// 执行完（或用户清空）后删一条。
    pub fn decision_delete(&self, id: &str) -> Result<bool> {
        let n = self
            .conn()
            .execute("DELETE FROM decisions WHERE id = ?1", params![id])
            .map_err(Error::from)?;
        Ok(n > 0)
    }

    /// 清空队列（**只清队列，不动任何文件**）。
    pub fn decisions_clear(&self) -> Result<usize> {
        let n = self
            .conn()
            .execute("DELETE FROM decisions", [])
            .map_err(Error::from)?;
        Ok(n)
    }

    // ------------------------------------------------------------ 上传队列

    pub fn uploads(&self) -> Result<Vec<UploadRow>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT remote_dir, remote_name, local, mtime, attempts, ephemeral
                   FROM uploads ORDER BY remote_path",
            )
            .map_err(Error::from)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(UploadRow {
                    remote_dir: r.get(0)?,
                    remote_name: r.get(1)?,
                    local: PathBuf::from(r.get::<_, String>(2)?),
                    mtime: r.get(3)?,
                    attempts: r.get::<_, i64>(4)? as u32,
                    ephemeral: r.get::<_, i64>(5)? != 0,
                })
            })
            .map_err(Error::from)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(Error::from)?);
        }
        Ok(out)
    }

    pub fn put_upload(&self, r: &UploadRow) -> Result<()> {
        self.conn()
            .execute(
                "INSERT INTO uploads(remote_path, remote_dir, remote_name, local, mtime, attempts, ephemeral)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT(remote_path) DO UPDATE SET
                   remote_dir = excluded.remote_dir,
                   remote_name = excluded.remote_name,
                   local = excluded.local,
                   mtime = excluded.mtime,
                   attempts = excluded.attempts,
                   ephemeral = excluded.ephemeral",
                params![
                    r.remote_path(),
                    r.remote_dir,
                    r.remote_name,
                    r.local.to_string_lossy(),
                    r.mtime,
                    r.attempts as i64,
                    r.ephemeral as i64,
                ],
            )
            .map_err(Error::from)?;
        Ok(())
    }

    pub fn bump_upload_attempts(&self, remote_path: &str, attempts: u32) -> Result<()> {
        self.conn()
            .execute(
                "UPDATE uploads SET attempts = ?2 WHERE remote_path = ?1",
                params![remote_path, attempts as i64],
            )
            .map_err(Error::from)?;
        Ok(())
    }

    pub fn delete_upload(&self, remote_path: &str) -> Result<bool> {
        let n = self
            .conn()
            .execute(
                "DELETE FROM uploads WHERE remote_path = ?1",
                params![remote_path],
            )
            .map_err(Error::from)?;
        Ok(n > 0)
    }

    pub fn clear_uploads(&self) -> Result<usize> {
        let n = self
            .conn()
            .execute("DELETE FROM uploads", [])
            .map_err(Error::from)?;
        Ok(n)
    }

    // ------------------------------------------------------------ 迁移 / 元数据

    /// 把 M2c 的 `cursors.json` / `baseline.json` 迁进库里（幂等）。
    ///
    /// 规则：
    /// * 只有「库里对应数据还是空的」才导入（避免把过期 JSON 又灌回已迁移的库）；
    /// * 旧文件一律改名成 `*.json.migrated` **保留备份**，不删除；
    /// * 第二次调用是 no-op（文件已不在原位）。
    pub fn migrate_legacy(&self, dir: &Path) -> Result<MigrateReport> {
        let mut rep = MigrateReport::default();
        let cpath = dir.join(CURSORS_FILE);
        let bpath = dir.join(BASELINE_FILE);

        if cpath.exists() && self.cursors()? == Cursors::default() {
            let c = Cursors::load(&cpath)?;
            self.save_cursors(&c)?;
            rep.cursors = true;
        }
        if bpath.exists() && self.baseline_len()? == 0 {
            let b = Baseline::load(&bpath)?;
            rep.baseline = b.len();
            self.save_baseline(&b)?;
        }
        for p in [cpath, bpath] {
            if p.exists() {
                let to = p.with_extension("json.migrated");
                if std::fs::rename(&p, &to).is_ok() {
                    rep.archived.push(to);
                }
            }
        }
        Ok(rep)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn()
            .execute(
                "INSERT INTO meta(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(Error::from)?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        self.conn()
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()
            .map_err(Error::from)
    }
}

// ---------------------------------------------------------------- SQL 小工具

fn write_cursors(conn: &Connection, c: &Cursors) -> Result<()> {
    conn.execute(
        "INSERT INTO cursors(id, config, notify, global_notify, max_log_seen, log_missing_count)
         VALUES(1, ?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET
           config = excluded.config,
           notify = excluded.notify,
           global_notify = excluded.global_notify,
           max_log_seen = excluded.max_log_seen,
           log_missing_count = excluded.log_missing_count",
        params![
            c.config,
            c.notify,
            c.global_notify,
            c.max_log_seen as i64,
            c.log_missing_count as i64,
        ],
    )
    .map_err(Error::from)?;
    Ok(())
}

/// 全量替换 baseline（事务由调用方开）。
///
/// 量级参考：实测单机 baseline ~225 项；即使到 10 万项，prepared statement + 单事务
/// 也是「毫秒~百毫秒」级，换来的是与游标同一个事务的强一致。
fn write_baseline(conn: &Connection, b: &Baseline) -> Result<()> {
    conn.execute("DELETE FROM baseline", [])
        .map_err(Error::from)?;
    {
        let mut stmt = conn
            .prepare(
                "INSERT INTO baseline(path, present, is_dir, size, mtime) VALUES(?1,?2,?3,?4,?5)",
            )
            .map_err(Error::from)?;
        for (path, sig) in &b.entries {
            stmt.execute(params![
                path,
                sig.exists as i64,
                sig.is_dir as i64,
                sig.size as i64,
                sig.mtime,
            ])
            .map_err(Error::from)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "qxync-store-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn cursors_roundtrip_and_single_row() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.cursors().unwrap(), Cursors::default());
        let c = Cursors {
            config: 188,
            notify: 37,
            global_notify: 177,
            max_log_seen: 188,
            log_missing_count: 111,
        };
        s.save_cursors(&c).unwrap();
        assert_eq!(s.cursors().unwrap(), c);
        // 再写一次不能变成两行
        s.save_cursors(&Cursors::default()).unwrap();
        let n: i64 = s
            .conn()
            .query_row("SELECT COUNT(*) FROM cursors", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn baseline_roundtrip_keeps_missing_semantics() {
        let s = Store::open_in_memory().unwrap();
        let mut b = Baseline::default();
        b.put("/home/a.txt", Sig::file(1024, 111));
        b.put("/home/dir", Sig::dir());
        b.put("/home/gone.txt", Sig::MISSING);
        s.save_baseline(&b).unwrap();
        assert_eq!(s.baseline_len().unwrap(), 3);

        let back = s.baseline().unwrap();
        assert_eq!(back.len(), 3);
        assert_eq!(back.get("/home/a.txt"), Sig::file(1024, 111));
        assert_eq!(back.get("/home/dir"), Sig::dir());
        assert!(!back.get("/home/gone.txt").exists, "MISSING 必须原样回来");
        assert_eq!(back.get("/nope"), Sig::MISSING);

        // 全量替换：写入更小的集合后旧行必须消失
        let mut b2 = Baseline::default();
        b2.put("/home/a.txt", Sig::file(2048, 333));
        s.save_baseline(&b2).unwrap();
        assert_eq!(s.baseline_len().unwrap(), 1);
        assert_eq!(
            s.baseline().unwrap().get("/home/a.txt"),
            Sig::file(2048, 333)
        );
    }

    #[test]
    fn save_state_is_one_transaction() {
        let s = Store::open_in_memory().unwrap();
        let c = Cursors {
            notify: 9,
            ..Cursors::default()
        };
        let mut b = Baseline::default();
        b.put("/home/x", Sig::file(1, 2));
        s.save_state(&c, &b).unwrap();
        assert_eq!(s.cursors().unwrap().notify, 9);
        assert_eq!(s.baseline_len().unwrap(), 1);
    }

    #[test]
    fn pins_survive_reopen() {
        let dir = tmpdir("pins");
        let db = dir.join(DB_FILE);
        {
            let s = Store::open(&db).unwrap();
            s.set_pin("/home/keep.bin", "pinned").unwrap();
            s.set_pin("/home/skip.bin", "excluded").unwrap();
            s.set_pin("/home/keep.bin", "unpinned").unwrap(); // 覆盖
            assert_eq!(
                s.pin("/home/keep.bin").unwrap().as_deref(),
                Some("unpinned")
            );
        }
        let s = Store::open(&db).unwrap();
        let m = s.pins().unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(
            m.get("/home/skip.bin").map(String::as_str),
            Some("excluded")
        );
        assert!(s.remove_pin("/home/keep.bin").unwrap());
        assert!(!s.remove_pin("/home/keep.bin").unwrap());
        assert_eq!(s.pins().unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn journal_batch_insert_list_filter_and_trim() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(
            s.schema_version().unwrap(),
            SCHEMA_VERSION,
            "v3 schema（M8.4 加 decisions 表）"
        );

        let mut rows = Vec::new();
        for i in 0..10 {
            rows.push(
                JournalEntry::ok("upload", format!("/home/f{i}.txt"), "上传完成").with_bytes(i),
            );
        }
        rows.push(JournalEntry::error(
            "upload",
            "/home/fail.txt",
            "上传失败：连接重置",
        ));
        rows.push(JournalEntry::blocked(
            "dehydrate",
            "/home/pin.bin",
            "被 pin 挡下",
        ));
        assert_eq!(s.journal_add_batch(&rows).unwrap(), 12);
        assert_eq!(s.journal_add_batch(&[]).unwrap(), 0, "空批次是 no-op");
        assert_eq!(s.journal_count().unwrap(), 12);

        // 倒序（最新在前）
        let all = s.journal_list(100, None, None, None).unwrap();
        assert_eq!(all.len(), 12);
        assert_eq!(all[0].kind, "dehydrate", "最新的在最前");

        // level 过滤
        let errs = s.journal_list(100, None, None, Some("error")).unwrap();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].path, "/home/fail.txt");
        let blk = s.journal_list(100, None, None, Some("blocked")).unwrap();
        assert_eq!(blk.len(), 1);

        // 子串查询（path 与 detail 都匹配）
        assert_eq!(
            s.journal_list(100, None, Some("f3.txt"), None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            s.journal_list(100, None, Some("连接重置"), None)
                .unwrap()
                .len(),
            1
        );

        // limit
        assert_eq!(s.journal_list(3, None, None, None).unwrap().len(), 3);

        // 计数分组
        let c = s.journal_counts().unwrap();
        assert_eq!(c.get("ok").copied().unwrap_or(0), 10);
        assert_eq!(c.get("error").copied().unwrap_or(0), 1);

        // 轮转：按条数只留 5 条
        let removed = s.journal_trim(5, 0).unwrap();
        assert_eq!(removed, 7);
        assert_eq!(s.journal_count().unwrap(), 5);
        assert!(
            s.journal_list(100, None, None, None)
                .unwrap()
                .iter()
                .all(|e| e.kind != "upload"
                    || e.path == "/home/f9.txt"
                    || e.path.starts_with("/home/f")),
            "留下的应该是最新的那些"
        );

        // 清空只清 journal
        assert_eq!(s.journal_clear().unwrap(), 5);
        assert_eq!(s.journal_count().unwrap(), 0);
    }

    #[test]
    fn journal_added_to_existing_v1_db_without_touching_data() {
        // 模拟老库：先按 v1 的 schema 建库并塞数据，再用 Store::open 打开 → 自动补 journal 表
        let dir = std::env::temp_dir().join(format!(
            "qxync-store-v1-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("sync.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS pins (path TEXT PRIMARY KEY, state TEXT NOT NULL);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO pins (path, state) VALUES ('/home/x', 'pinned')",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 1i64).unwrap();
        }
        let s = Store::open(&db).unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION, "打开时升到 v3");
        assert_eq!(
            s.pin("/home/x").unwrap().as_deref(),
            Some("pinned"),
            "老数据必须原样保留"
        );
        assert_eq!(s.journal_count().unwrap(), 0, "新表是空的");
        assert_eq!(
            s.journal_add_batch(&[JournalEntry::ok("scan", "/", "一轮")])
                .unwrap(),
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrates_legacy_json_once_and_archives_it() {
        let dir = tmpdir("migrate");
        let db = dir.join(DB_FILE);

        let mut b = Baseline::default();
        b.put("/home/a.txt", Sig::file(5, 6));
        b.put("/home/b.txt", Sig::file(7, 8));
        b.save(&dir.join(BASELINE_FILE)).unwrap();
        let c = Cursors {
            config: 1,
            notify: 2,
            global_notify: 3,
            max_log_seen: 4,
            log_missing_count: 5,
        };
        c.save(&dir.join(CURSORS_FILE)).unwrap();

        let s = Store::open(&db).unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
        let rep = s.migrate_legacy(&dir).unwrap();
        assert!(rep.cursors);
        assert_eq!(rep.baseline, 2);
        assert_eq!(rep.archived.len(), 2);
        assert_eq!(s.cursors().unwrap(), c);
        assert_eq!(s.baseline_len().unwrap(), 2);
        assert!(!dir.join(BASELINE_FILE).exists());
        assert!(dir.join("baseline.json.migrated").exists());

        // 幂等：再跑一次什么都不做，数据也不翻倍
        let rep2 = s.migrate_legacy(&dir).unwrap();
        assert!(!rep2.did_something());
        assert_eq!(s.baseline_len().unwrap(), 2);
        assert_eq!(s.integrity_check().unwrap(), "ok");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migrate_does_not_clobber_existing_rows() {
        let dir = tmpdir("no-clobber");
        let s = Store::open(dir.join(DB_FILE)).unwrap();
        let c = Cursors {
            notify: 42,
            ..Cursors::default()
        };
        s.save_cursors(&c).unwrap();
        let mut b = Baseline::default();
        b.put("/home/newer", Sig::file(1, 1));
        s.save_baseline(&b).unwrap();

        // 旧 JSON 还在（比如用户从旧版本回滚又升级回来）
        let mut old = Baseline::default();
        old.put("/home/older", Sig::file(9, 9));
        old.save(&dir.join(BASELINE_FILE)).unwrap();
        Cursors::default().save(&dir.join(CURSORS_FILE)).unwrap();

        let rep = s.migrate_legacy(&dir).unwrap();
        assert!(rep.archived.len() == 2, "旧文件仍应归档");
        assert!(!rep.cursors && rep.baseline == 0, "不能覆盖库里已有的数据");
        assert_eq!(s.cursors().unwrap().notify, 42);
        assert_eq!(s.baseline().unwrap().get("/home/newer"), Sig::file(1, 1));
        assert_eq!(s.baseline_len().unwrap(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wal_files_and_reopen_persist() {
        let dir = tmpdir("wal");
        let db = dir.join(DB_FILE);
        {
            let s = Store::open(&db).unwrap();
            let c = Cursors {
                notify: 7,
                ..Cursors::default()
            };
            let mut b = Baseline::default();
            b.put("/home/z", Sig::file(3, 4));
            s.save_state(&c, &b).unwrap();
            s.set_meta("host", "nas.local").unwrap();
        }
        let s = Store::open(&db).unwrap();
        assert_eq!(s.cursors().unwrap().notify, 7);
        assert_eq!(s.baseline().unwrap().get("/home/z"), Sig::file(3, 4));
        assert_eq!(s.meta("host").unwrap().as_deref(), Some("nas.local"));
        assert_eq!(s.meta("nope").unwrap(), None);
        assert_eq!(s.integrity_check().unwrap(), "ok");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn uploads_roundtrip_upsert_delete() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.uploads().unwrap().is_empty());
        let a = UploadRow {
            remote_dir: "/home/qxync-test".into(),
            remote_name: "a.txt".into(),
            local: PathBuf::from("/cache/aaa"),
            mtime: 1000,
            attempts: 0,
            ephemeral: false,
        };
        let b = UploadRow {
            remote_dir: "/home/qxync-test".into(),
            remote_name: "b b.txt".into(),
            local: PathBuf::from("/cache/bbb"),
            mtime: 2000,
            attempts: 3,
            ephemeral: true,
        };
        s.put_upload(&a).unwrap();
        s.put_upload(&b).unwrap();
        assert_eq!(s.uploads().unwrap().len(), 2);

        // 同一个远端路径 → 覆盖，不是新增
        let a2 = UploadRow {
            mtime: 1500,
            ..a.clone()
        };
        s.put_upload(&a2).unwrap();
        let rows = s.uploads().unwrap();
        assert_eq!(rows.len(), 2);
        let got = rows.iter().find(|r| r.remote_name == "a.txt").unwrap();
        assert_eq!(got.mtime, 1500);
        assert_eq!(got.remote_path(), "/home/qxync-test/a.txt");
        let got_b = rows.iter().find(|r| r.remote_name == "b b.txt").unwrap();
        assert!(got_b.ephemeral && got_b.attempts == 3);

        s.bump_upload_attempts("/home/qxync-test/a.txt", 7).unwrap();
        assert_eq!(
            s.uploads()
                .unwrap()
                .iter()
                .find(|r| r.remote_name == "a.txt")
                .unwrap()
                .attempts,
            7
        );
        assert!(s.delete_upload("/home/qxync-test/a.txt").unwrap());
        assert!(!s.delete_upload("/home/qxync-test/a.txt").unwrap());
        assert_eq!(s.clear_uploads().unwrap(), 1);
        assert!(s.uploads().unwrap().is_empty());
    }

    #[test]
    fn sig_exists_is_dir_helpers_are_consistent() {
        // 保证 store 里用的 Sig 构造器与 sync.rs 的语义一致
        assert!(Sig::file(1, 2).exists && !Sig::file(1, 2).is_dir);
        assert!(Sig::dir().exists && Sig::dir().is_dir);
        assert!(!Sig::MISSING.exists);
    }

    /// ★ M8.4：待裁决队列 —— 同路径去重、裁决、删行、清空，且**不碰 pins/journal**。
    #[test]
    fn decisions_queue_upsert_resolve_and_isolation() {
        let s = Store::open_in_memory().unwrap();
        s.set_pin("/home/keep.bin", "pinned").unwrap();
        s.journal_add_batch(&[JournalEntry::error("sync", "/home/a.txt", "冲突")])
            .unwrap();

        let mk = |path: &str, ls: u64, rs: u64| DecisionRow {
            id: DecisionRow::id_for(path),
            path: path.to_string(),
            task_id: "t1".into(),
            is_dir: false,
            local_size: ls,
            local_mtime: 100,
            remote_size: rs,
            remote_mtime: 200,
            created_unix: 1_700_000_000,
            resolution: None,
        };
        s.decision_upsert(&mk("/home/a.txt", 10, 20)).unwrap();
        s.decision_upsert(&mk("/home/b.txt", 30, 40)).unwrap();
        assert_eq!(s.decisions().unwrap().len(), 2);

        // 同一路径再冲突：**不新增行**，只刷新签名（否则每轮同步都会灌一条）
        s.decision_upsert(&mk("/home/a.txt", 11, 22)).unwrap();
        let rows = s.decisions().unwrap();
        assert_eq!(rows.len(), 2, "同路径必须去重");
        let a = rows.iter().find(|r| r.path == "/home/a.txt").unwrap();
        assert_eq!(a.local_size, 11);
        assert!(!a.is_resolved());

        // 裁决
        assert!(s.decision_resolve(&a.id, Some("keep_local")).unwrap());
        let a2 = s.decision_by_path("/home/a.txt").unwrap().unwrap();
        assert_eq!(a2.resolution.as_deref(), Some("keep_local"));
        assert!(a2.is_resolved());
        assert!(!s.decision_resolve("不存在", Some("keep_local")).unwrap());

        // 再冲突时**保留已有裁决**（用户已经表过态）
        s.decision_upsert(&mk("/home/a.txt", 12, 24)).unwrap();
        assert_eq!(
            s.decision_by_path("/home/a.txt")
                .unwrap()
                .unwrap()
                .resolution
                .as_deref(),
            Some("keep_local")
        );

        // 删一条 / 清空
        assert!(s.decision_delete(&a.id).unwrap());
        assert!(s.decision_delete(&a.id).unwrap() == false);
        assert_eq!(s.decisions_clear().unwrap(), 1);
        assert!(s.decisions().unwrap().is_empty());

        // 隔离：pin 与 journal 一行没动
        assert_eq!(s.pin("/home/keep.bin").unwrap().as_deref(), Some("pinned"));
        assert_eq!(s.journal_count().unwrap(), 1);
    }

    /// ★ M8.4：v2 老库打开后自动补 decisions 表（不写迁移代码），旧数据原样保留。
    #[test]
    fn decisions_table_added_to_v2_db() {
        let dir = std::env::temp_dir().join(format!(
            "qxync-store-v2-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("sync.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS pins (path TEXT PRIMARY KEY, state TEXT NOT NULL);
                 CREATE TABLE IF NOT EXISTS journal (
                   id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL,
                   task_id TEXT NOT NULL DEFAULT '', kind TEXT NOT NULL,
                   path TEXT NOT NULL DEFAULT '', detail TEXT NOT NULL DEFAULT '',
                   status TEXT NOT NULL DEFAULT 'ok', bytes INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO pins (path, state) VALUES ('/home/x', 'pinned');
                 INSERT INTO journal (ts, kind, path) VALUES (1, 'scan', '/home');",
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 2i64).unwrap();
        }
        let s = Store::open(&db).unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(s.pin("/home/x").unwrap().as_deref(), Some("pinned"));
        assert_eq!(s.journal_count().unwrap(), 1);
        assert!(s.decisions().unwrap().is_empty());
        s.decision_upsert(&DecisionRow {
            id: DecisionRow::id_for("/home/c.txt"),
            path: "/home/c.txt".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(s.decisions().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
