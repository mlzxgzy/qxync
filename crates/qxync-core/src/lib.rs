//! # qxync-core
//!
//! qxync 的共享层：错误码、数据模型、URL 编码规则、本地配置布局、
//! **M5 起**还包含本地状态库（SQLite）与 librsync 兼容的 delta 编解码。
//! 不依赖任何 HTTP 运行时，方便被 client / daemon / fuse / cli 复用，也方便单测。
//!
//! ## 已实测的协议要点（2026-09-30，QTS 5.2.9 / TS-464C）
//!
//! * 登录：`POST /cgi-bin/authLogin.cgi`，body 必须 `serviceKey=1` + `pwd=base64(口令)`；
//!   用 `service=Qsync` 或明文口令都会得到 `authPassed=0 / errorValue=-1`。
//! * 元数据：`/cgi-bin/qsync/qsyncsrv.cgi`（`func=get_list|stat|createdir|qbox_*`）。
//! * 字节流：下载 `GET /cgi-bin/filemanager/utilRequest.cgi?func=download`，
//!   上传 `POST /cgi-bin/qsync/upload.php`（multipart 字段名必须是 `files[]`）。
//! * **查询串里的空格必须编码成 `%20`** —— 用 `+` 会让含空格/中文的文件名 404。
//! * 普通用户的家目录根是 `/home`（对应真实路径 `/share/homes/<user>`）。
//! * `versioning_lock` 可用，但这台 NAS 的 `versioning_support` 全为 0、
//!   `versioning_stat_delta` 恒 `exist:0` → **增量链路服务端不可用**（M5 实测，见 `M5-SQLite与delta.md`）。

pub mod config;
pub mod dehydrate;
pub mod delta;
pub mod encode;
pub mod error;
pub mod freespace;
pub mod ipc;
pub mod model;
pub mod roots;
pub mod rules;
pub mod settings;
pub mod status;
pub mod store;
pub mod sync;
pub mod tasks;

pub use config::{ConfigPaths, Credentials, LinkConfig, PeerConfig, PeerRegistry};
pub use dehydrate::{Block, CacheLimit, Candidate, Policy};
pub use delta::Signature;
pub use encode::{build_query, encode_query_value};
pub use error::{Error, Result};
pub use freespace::{statvfs, AutoFreeDecision, FsSpace};
pub use model::{parse_listing, parse_max_log, parse_nas_uid, DirEntry, Listing, MaxLog, NasUid};
pub use roots::{client_path_from_share, normalize_root};
pub use rules::{HideReason, RuleParse, Rules, TEMP_PATTERNS};
pub use settings::{ProxySettings, ProxySpec, Settings};
pub use status::ServerStatus;
pub use store::{JournalEntry, MigrateReport, Store, DB_FILE};
pub use sync::{
    parse_sync_log, Baseline, Cursors, Decision, DeleteProtection, LocalSig, Sig, SyncEvent,
    SyncLogBatch,
};
pub use tasks::{Task, DIR_2WAY, DIR_DOWN, DIR_UP};

/// 普通用户的 Qsync 家目录根（不是 `/home/<user>`）。
pub const HOME_ROOT: &str = "/home";
