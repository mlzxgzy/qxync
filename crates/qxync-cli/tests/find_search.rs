//! ★ M42：`qxync find` 的隔离集成测试。
//!
//! 钉住用户可见的行为（全部**不挂载、不联网、不启动 daemon**）：
//!
//! * 子串命中 / 通配符 / `--under`（含本地路径换算）/ `--type` / 截断提示；
//! * `--local` 把远端路径换算成挂载点里的本地路径；`--json` 字段稳定；
//! * 状态库不存在、任务不存在 → 非零退出码 + 可读错误；
//! * 墓碑（`present=0`）不返回、本地新建（`nodes.local_only`）能搜到。
//!
//! 夹具直接写 SQLite（`qxync_core::Store`）与 `links/` / `tasks/` 配置，
//! 与 `xtask/tests/m42-search.sh`（python 造库 + 真实二进制）互补。

use qxync_core::{link_namespace, Baseline, ConfigPaths, LinkConfig, Sig, Store, Task, DB_FILE};
use std::path::PathBuf;
use std::process::{Command, Output};

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "qxync-find-{}-{}-{}",
        std::process::id(),
        tag,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Fixture {
    root: PathBuf,
    cfg: PathBuf,
    data: PathBuf,
    state: PathBuf,
    mount: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = tmp(tag);
        let cfg = root.join("config").join("qxync");
        let data = root.join("data").join("qxync");
        let state = root.join("state").join("qxync");
        let mount = root.join("mnt").join("qsync");
        for d in [
            cfg.join("links"),
            cfg.join("tasks"),
            data.clone(),
            state.join("log"),
            root.join("run"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(
            cfg.join("links").join("default.json"),
            r#"{"id":"default","host":"127.0.0.1","port":9,"https":false,"insecure":true,"user":"u","ipv4_only":true}"#,
        )
        .unwrap();
        std::fs::write(
            cfg.join("tasks").join("qsync.json"),
            format!(
                r#"{{"id":"qsync","link_id":"default","name":"qsync","enabled":true,
                    "mountpoint":"{}","root":"/home/.Qsync"}}"#,
                mount.display()
            ),
        )
        .unwrap();
        Self {
            root,
            cfg,
            data,
            state,
            mount,
        }
    }

    fn paths(&self) -> ConfigPaths {
        ConfigPaths {
            config_dir: self.cfg.clone(),
            data_dir: self.data.clone(),
            state_dir: self.state.clone(),
        }
    }

    fn link(&self) -> LinkConfig {
        LinkConfig::load(&self.paths(), "default").unwrap()
    }

    fn db(&self) -> PathBuf {
        self.paths().sync_dir_for(&self.link()).join(DB_FILE)
    }

    /// 造一份最小状态库：远端 baseline + 一条本地新建 + 一条墓碑。
    fn seed(&self) {
        let db = self.db();
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let store = Store::open(&db).unwrap();
        let mut b = Baseline::default();
        b.put(
            "/home/.Qsync/2026/29-四川赛/试题A.pdf",
            Sig::file(12218, 1792368403),
        );
        b.put(
            "/home/.Qsync/2026/29-四川赛/README.md",
            Sig::file(180, 1792368404),
        );
        b.put("/home/.Qsync/2026", Sig::dir());
        b.put("/home/.Qsync/2026/已删除.txt", Sig::MISSING);
        store.save_baseline(&b).unwrap();
        store
            .node_upsert(&qxync_core::store::NodeRow {
                file_id: qxync_core::compute_file_id(
                    false,
                    42,
                    1792368405,
                    "草稿.txt",
                    "/home/.Qsync/2026",
                ),
                path: "/home/.Qsync/2026/草稿.txt".into(),
                remote_path: "/home/.Qsync/2026/草稿.txt".into(),
                size: 42,
                mtime: 1792368405,
                is_folder: false,
                updated_at: 0,
                local_only: true,
            })
            .unwrap();
        drop(store);
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_qxync"));
        cmd.args(args)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("HOME", &self.root)
            .env_remove("QXNYC_SOCKET")
            .env_remove("QXNYC_HOST")
            .env_remove("QXNYC_USER")
            .env_remove("QXNYC_PASSWORD")
            .env_remove("QXNYC_OTP_SECRET")
            .env_remove("QXNYC_OTP_CODE")
            .env_remove("RUST_LOG");
        cmd.output().unwrap()
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn find_substring_and_local_mapping() {
    let f = Fixture::new("substring");
    f.seed();

    let o = f.run(&["find", "试题"]);
    assert!(o.status.success(), "stderr={}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("/home/.Qsync/2026/29-四川赛/试题A.pdf"),
        "远端路径应命中：{out}"
    );
    assert!(!out.contains("README"), "不该命中无关文件");

    // --local：换算成挂载点里的本地路径（挂载点此刻并不存在，纯词法换算）
    let o = f.run(&["find", "试题", "--local"]);
    let expected = f.mount.join("2026").join("29-四川赛").join("试题A.pdf");
    let out = stdout(&o);
    assert!(
        out.contains(expected.to_str().unwrap()),
        "--local 应输出 {}，实际：{out}",
        expected.display()
    );
}

#[test]
fn find_json_has_stable_fields() {
    let f = Fixture::new("json");
    f.seed();
    let o = f.run(&["find", "试题", "--json"]);
    assert!(o.status.success(), "stderr={}", stderr(&o));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["total"], 1);
    assert_eq!(v["truncated"], false);
    assert_eq!(v["pattern"], "试题");
    let hit = &v["hits"][0];
    assert_eq!(hit["origin"], "remote");
    assert_eq!(hit["size"], 12218);
    assert_eq!(hit["is_dir"], false);
    assert_eq!(hit["task_id"], "qsync");
    assert!(hit["mtime_text"].as_str().unwrap().contains("2026-10-"));
    assert!(hit["local_path"]
        .as_str()
        .unwrap()
        .ends_with("2026/29-四川赛/试题A.pdf"));
}

#[test]
fn find_glob_under_type_and_truncation() {
    let f = Fixture::new("filters");
    f.seed();

    // 无 `/` 的 glob 只匹配文件名
    let o = f.run(&["find", "*.pdf", "--glob"]);
    assert!(o.status.success(), "stderr={}", stderr(&o));
    assert_eq!(stdout(&o).lines().count(), 1);

    // --under 接受挂载点下的本地路径（自动换算成远端）
    let under = f.mount.join("2026").join("29-四川赛");
    let o = f.run(&["find", "--under", under.to_str().unwrap(), "--type", "f"]);
    let out = stdout(&o);
    assert!(out.contains("试题A.pdf"));
    assert!(
        !out.contains("/home/.Qsync/2026\n"),
        "目录不该出现在 --type f"
    );

    // 截断提示在 stderr，stdout 仍是干净的一行一条
    let o = f.run(&["find", "Qsync", "--limit", "1"]);
    assert_eq!(stdout(&o).lines().count(), 1);
    assert!(stderr(&o).contains("已显示 1 条"), "stderr={}", stderr(&o));
}

/// 状态库正被写连接持有（≈ daemon 正在跑）时，只读搜索必须照常可用 ——
/// 走的是 WAL 并发读；这比「库空闲时能读」更接近真实使用场景。
#[test]
fn find_reads_while_state_db_writer_is_open() {
    let f = Fixture::new("concurrent");
    f.seed();
    let writer = Store::open(&f.db()).unwrap();
    let mut b = writer.baseline().unwrap();
    b.put("/home/.Qsync/2026/并发新增.txt", Sig::file(9, 1792368406));
    writer.save_baseline(&b).unwrap();

    let o = f.run(&["find", "并发新增"]);
    assert!(o.status.success(), "stderr={}", stderr(&o));
    assert!(stdout(&o).contains("并发新增.txt"), "{}", stdout(&o));
    drop(writer);
}

#[test]
fn find_returns_local_only_and_skips_tombstone() {
    let f = Fixture::new("local-only");
    f.seed();

    let o = f.run(&["find", "草稿", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["total"], 1);
    assert_eq!(v["hits"][0]["origin"], "local_only");
    assert_eq!(v["hits"][0]["size"], 42);

    let o = f.run(&["find", "已删除"]);
    assert!(o.status.success());
    assert!(stdout(&o).is_empty(), "present=0 的墓碑不能出现在结果里");
}

#[test]
fn find_reports_missing_db_and_unknown_task() {
    // 没有状态库：明确报错 + 非零退出
    let f = Fixture::new("missing-db");
    let o = f.run(&["find", "x"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("状态库不存在"), "stderr={}", stderr(&o));

    // 状态库在、任务不存在：报错里带任务 id
    f.seed();
    let o = f.run(&["find", "--task", "nope"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("nope"), "stderr={}", stderr(&o));
}

#[test]
fn find_prefers_existing_link_namespace() {
    let f = Fixture::new("namespace");
    f.seed();
    // 状态库按连接命名空间放置：路径应当与 link_namespace 一致
    let db = f.db();
    assert!(
        db.ends_with(
            PathBuf::from("sync")
                .join(link_namespace(&f.link()))
                .join(DB_FILE)
        ),
        "{}",
        db.display()
    );
    assert!(Task::load(&f.paths(), "qsync").is_ok());
}
