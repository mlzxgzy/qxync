//! # qxync-core
//!
//! QSync-Linux 的共享层：错误码、数据模型、URL 编码规则、本地配置布局。
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

pub mod config;
pub mod encode;
pub mod error;
pub mod ipc;
pub mod model;
pub mod status;

pub use config::{ConfigPaths, Credentials, LinkConfig};
pub use encode::{build_query, encode_query_value};
pub use error::{Error, Result};
pub use model::{parse_listing, parse_max_log, parse_nas_uid, DirEntry, Listing, MaxLog, NasUid};
pub use status::ServerStatus;

/// 普通用户的 Qsync 家目录根（不是 `/home/<user>`）。
pub const HOME_ROOT: &str = "/home";
