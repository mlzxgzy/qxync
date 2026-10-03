//! 服务端返回结构体 + 宽容反序列化。
//!
//! NAS 的字段类型不统一：`filesize` 有时是字符串（64 位）、`privilege` 可能是
//! 字符串/数字/空串、`isfolder`/`exist` 是 0/1 整数。这里统一做宽容处理，
//! 遇到未知字段一律忽略（服务端加字段不应让客户端解析失败）。

use crate::error::{Error, Result};
use crate::status::ServerStatus;
use serde::{Deserialize, Deserializer, Serialize};

// ---------------------------------------------------------------- 反序列化helper

fn de_u64_flex<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<u64, D::Error> {
    use serde::de::Error as _;
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Number(n) => Ok(n.as_u64().unwrap_or(0)),
        serde_json::Value::String(s) => Ok(s.parse().unwrap_or(0)),
        serde_json::Value::Null => Ok(0),
        other => Err(D::Error::custom(format!("无法当 u64 解析: {other}"))),
    }
}

fn de_i64_flex<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<i64, D::Error> {
    use serde::de::Error as _;
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Number(n) => Ok(n.as_i64().unwrap_or(0)),
        serde_json::Value::String(s) => Ok(s.parse().unwrap_or(0)),
        serde_json::Value::Null => Ok(0),
        other => Err(D::Error::custom(format!("无法当 i64 解析: {other}"))),
    }
}

fn de_opt_i64_flex<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<i64>, D::Error> {
    Ok(Some(de_i64_flex(d)?))
}

fn de_opt_u64_flex<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<u64>, D::Error> {
    Ok(Some(de_u64_flex(d)?))
}

fn de_bool_int<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<bool, D::Error> {
    use serde::de::Error as _;
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Bool(b) => Ok(b),
        serde_json::Value::Number(n) => Ok(n.as_i64().unwrap_or(0) != 0),
        serde_json::Value::String(s) => Ok(matches!(s.as_str(), "1" | "true" | "True")),
        serde_json::Value::Null => Ok(false),
        other => Err(D::Error::custom(format!("无法当 bool 解析: {other}"))),
    }
}

fn de_opt_string_flex<'de, D: Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<String>, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        other => Some(other.to_string()),
    })
}

// ---------------------------------------------------------------- 数据模型

/// `get_list` / `stat` 返回的单个条目（`CGetFolderInfoJson` / `CGetPathInfoJson`）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DirEntry {
    #[serde(default)]
    pub filename: String,
    #[serde(default, deserialize_with = "de_u64_flex")]
    pub filesize: u64,
    #[serde(default, deserialize_with = "de_bool_int")]
    pub isfolder: bool,
    #[serde(default, deserialize_with = "de_i64_flex")]
    pub epochmt: i64,
    #[serde(default, deserialize_with = "de_bool_int")]
    pub have_child: bool,
    #[serde(default, deserialize_with = "de_bool_int")]
    pub exist: bool,
    #[serde(default, deserialize_with = "de_opt_string_flex")]
    pub privilege: Option<String>,
    #[serde(default, deserialize_with = "de_bool_int")]
    pub versioning_support: bool,
    /// 人类可读时间（服务端给的 `mt`，形如 `2023/01/17 00:59:01`）。
    #[serde(default, rename = "mt")]
    pub mtime_text: Option<String>,
}

impl DirEntry {
    /// 本地新建的文件/目录（M2b 写路径用）。
    pub fn local(filename: impl Into<String>, is_folder: bool, size: u64, epochmt: i64) -> Self {
        Self {
            filename: filename.into(),
            filesize: size,
            isfolder: is_folder,
            epochmt,
            have_child: false,
            exist: true,
            privilege: None,
            versioning_support: false,
            mtime_text: None,
        }
    }

    /// 占位符要展示的大小：目录不关心，文件用真实字节数。
    pub fn display_size(&self) -> u64 {
        if self.isfolder {
            0
        } else {
            self.filesize
        }
    }
}

/// 目录列举响应。
#[derive(Debug, Clone, Deserialize)]
pub struct Listing {
    #[serde(default, deserialize_with = "de_opt_i64_flex")]
    pub status: Option<i64>,
    #[serde(default, deserialize_with = "de_i64_flex")]
    pub total: i64,
    #[serde(default, deserialize_with = "de_opt_i64_flex")]
    pub real_total: Option<i64>,
    #[serde(default)]
    pub datas: Vec<DirEntry>,
    #[serde(default, deserialize_with = "de_opt_i64_flex")]
    pub error_code: Option<i64>,
    #[serde(default, deserialize_with = "de_opt_string_flex")]
    pub version: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string_flex")]
    pub build: Option<String>,
    /// 单文件 `stat` 时的存在性。
    #[serde(default, deserialize_with = "de_opt_i64_flex")]
    pub exist: Option<i64>,
    /// 服务端统一错误出口的原文（仅失败时出现）。
    #[serde(default, deserialize_with = "de_opt_string_flex")]
    pub msg: Option<String>,
}

impl Listing {
    /// 若响应带 `status` 且非成功 → 返回错误（带服务端 `msg` 原文）。
    pub fn ensure_ok(&self, context: impl Into<String>) -> Result<&Self> {
        if let Some(s) = self.status {
            let status = ServerStatus(s);
            if !status.is_success() {
                return Err(match self.msg.clone() {
                    Some(msg) => Error::status_msg(s, context, msg),
                    None => Error::Status {
                        status,
                        context: context.into(),
                        msg: None,
                    },
                });
            }
        }
        Ok(self)
    }

    /// `stat` 的便利取值：取 datas 第 0 项。
    pub fn single(&self) -> Option<&DirEntry> {
        self.datas.first()
    }
}

/// `qbox_get_max_log` 响应（轮询入口）。
///
/// ★ 成功响应**没有 `status` 键** —— 判据是 `sync_signal` / `max_log`。
/// 只有走服务端统一错误出口时才会带 `status`（当前唯一确认值：8 = 未就绪）。
/// 逆向见 `.research/new/Qsync_API_返回码逆向.md` §1.1（handler 0xef6eb–0xf0f94）。
#[derive(Debug, Clone, Deserialize)]
pub struct MaxLog {
    #[serde(default, deserialize_with = "de_u64_flex")]
    pub max_log: u64,
    #[serde(default, deserialize_with = "de_u64_flex")]
    pub notify: u64,
    #[serde(default, deserialize_with = "de_u64_flex")]
    pub global_notify: u64,
    /// ★ 真正的判据：`1` = 有变更，`2` = 降速（附 `slowdown_seconds`）。
    #[serde(default, deserialize_with = "de_i64_flex")]
    pub sync_signal: i64,
    /// `sync_signal == 2` 时服务端给出的建议等待秒数（= n*5）。
    #[serde(default, deserialize_with = "de_u64_flex")]
    pub slowdown_seconds: u64,
    /// 服务端单次 `qbox_get_sync_log` 的条数上限（真机 256）。
    /// 缺字段时为 `None` —— 表示"服务端没告诉 us"，别当成 0。
    #[serde(default, deserialize_with = "de_opt_u64_flex")]
    pub server_limit: Option<u64>,
    /// CGI 并发数，服务端给客户端的压测信息。
    #[serde(default, deserialize_with = "de_opt_u64_flex")]
    pub cgi_number: Option<u64>,
    // ---- 服务端就绪状态（逆向 §1.1 的四个自检字段）----
    /// QPKG 仍在安装（`/etc/config/qpkg_job.conf` 里 internal_name != "null"）。
    #[serde(default, deserialize_with = "de_bool_int")]
    pub is_booting: bool,
    /// 升级/迁移未完成（`<mount>/.QsyncServer_required` 仍存在）。
    #[serde(default, deserialize_with = "de_bool_int")]
    pub is_migrating: bool,
    #[serde(default, deserialize_with = "de_bool_int")]
    pub is_recovering: bool,
    #[serde(default, deserialize_with = "de_bool_int")]
    pub is_backuping_restoring: bool,
    #[serde(default, deserialize_with = "de_opt_i64_flex")]
    pub status: Option<i64>,
    /// 服务端统一错误出口的原文（仅失败时出现）。
    #[serde(default, deserialize_with = "de_opt_string_flex")]
    pub msg: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string_flex")]
    pub version: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string_flex")]
    pub build: Option<String>,
}

impl MaxLog {
    /// 服务端是否处于"忙但没坏"的状态 → 本轮应当**安静等待**，不要重登也不要全量重扫。
    ///
    /// 与 [`NasUid::busy_reason`] 同义，但走的是轮询响应自带的那份，
    /// 省掉一次额外的 `qbox_get_nas_uid` 往返。
    pub fn busy_reason(&self) -> Option<&'static str> {
        if self.is_migrating {
            Some("NAS 正在迁移")
        } else if self.is_recovering {
            Some("NAS 正在恢复")
        } else if self.is_backuping_restoring {
            Some("NAS 正在备份还原")
        } else if self.is_booting {
            Some("Qsync 服务仍在启动")
        } else {
            None
        }
    }

    /// 服务端建议的轮询间隔（秒）。
    ///
    /// 逆向结论：`slowdown_seconds` 只在 `sync_signal == 2`（降速）时给出，
    /// 值是 `n * 5`。这里按 `n*5` 直接当秒用；没有就返回 `None` 让调用方用默认值。
    pub fn advised_interval_secs(&self) -> Option<u64> {
        if self.sync_signal == 2 && self.slowdown_seconds > 0 {
            Some(self.slowdown_seconds)
        } else {
            None
        }
    }
}

/// `qbox_get_nas_uid` 响应（替代已 404 的 `qsyncsrvPrepare.cgi`）。
#[derive(Debug, Clone, Deserialize)]
pub struct NasUid {
    #[serde(default, rename = "UID", deserialize_with = "de_opt_string_flex")]
    pub uid: Option<String>,
    #[serde(default, rename = "SUID", deserialize_with = "de_opt_string_flex")]
    pub suid: Option<String>,
    #[serde(default, rename = "CUID", deserialize_with = "de_opt_string_flex")]
    pub cuid: Option<String>,
    #[serde(default, rename = "USER_CUID", deserialize_with = "de_opt_string_flex")]
    pub user_cuid: Option<String>,
    #[serde(default, rename = "MAC0", deserialize_with = "de_opt_string_flex")]
    pub mac0: Option<String>,
    #[serde(
        default,
        rename = "Qsync_version",
        deserialize_with = "de_opt_string_flex"
    )]
    pub qsync_version: Option<String>,
    #[serde(
        default,
        rename = "Qsync_qpkg_version",
        deserialize_with = "de_opt_string_flex"
    )]
    pub qpkg_version: Option<String>,
    #[serde(default, rename = "Build", deserialize_with = "de_opt_string_flex")]
    pub build: Option<String>,
    #[serde(default, deserialize_with = "de_bool_int")]
    pub qbox_cgi: bool,
    #[serde(default, deserialize_with = "de_bool_int")]
    pub fcgi: bool,
    #[serde(default, deserialize_with = "de_bool_int")]
    pub is_migrating: bool,
    #[serde(default, deserialize_with = "de_bool_int")]
    pub is_recovering: bool,
    #[serde(default, deserialize_with = "de_bool_int")]
    pub is_backuping_restoring: bool,
}

impl NasUid {
    /// 迁移/恢复/备份还原中 → 同步应暂停（报告 02 §3.4）。
    pub fn busy_reason(&self) -> Option<&'static str> {
        if self.is_migrating {
            Some("NAS 正在迁移")
        } else if self.is_recovering {
            Some("NAS 正在恢复")
        } else if self.is_backuping_restoring {
            Some("NAS 正在备份还原")
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------- 解析入口

pub fn parse_listing(body: &[u8]) -> Result<Listing> {
    serde_json::from_slice(body).map_err(|e| {
        Error::Parse(format!(
            "get_list 响应不是合法 JSON: {e}；原文前 200B: {}",
            String::from_utf8_lossy(&body[..body.len().min(200)])
        ))
    })
}

pub fn parse_max_log(body: &[u8]) -> Result<MaxLog> {
    serde_json::from_slice(body)
        .map_err(|e| Error::Parse(format!("qbox_get_max_log 解析失败: {e}")))
}

pub fn parse_nas_uid(body: &[u8]) -> Result<NasUid> {
    serde_json::from_slice(body)
        .map_err(|e| Error::Parse(format!("qbox_get_nas_uid 解析失败: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机（2026-09-30）`get_list&path=/home/qxync-test` 的裁剪响应。
    const REAL_LISTING: &str = r#"{
      "medialib": 1, "total": 2, "real_total": 2, "acl": 7, "is_acl_enable": 1,
      "datas": [
        { "filename": "l1", "isfolder": 1, "filesize": "4096", "group": "everyone",
          "owner": "test1", "iscommpressed": 0, "privilege": "777", "privilege_ex": 0,
          "filetype": 0, "mt": "2026/09/30 20:01:47", "epochmt": 1790769707, "qbox_type": 0,
          "have_child": 0, "exist": 1, "versioning_support": 0 },
        { "filename": "空 格 中文名.txt", "isfolder": 0, "filesize": "29", "privilege": "670",
          "epochmt": 1790769710, "exist": 1, "have_child": 0 }
      ] }"#;

    /// 真机 `qbox_get_max_log` 响应。
    const REAL_MAX_LOG: &str = r#"{ "notify": "0", "max_log": "37", "global_notify": "177",
        "server_limit": "256", "cgi_number": "1", "sync_signal": 1 }"#;

    /// 真机 `qbox_get_nas_uid` 响应（裁剪）。
    const REAL_NAS_UID: &str = r#"{"qbox_cgi": 1, "UID": "684966bbc039b0187fcb37e5a7d47241",
        "USER_CUID": "aa4a20cc762671c2c53cf21a6e04bd21", "Qsync_version": "5.1.1",
        "Qsync_qpkg_version": "5.0.0.7", "Build": "20260723", "SUID": "45caa2ab",
        "CUID": "a698c436682b0f29e9a9fd65ba290a56", "MAC0": "245ebe69307d",
        "versioning_version": "1.0.0", "user_type": 0, "fcgi": 1, "is_booting": 0,
        "is_migrating": 0, "is_recovering": 0, "is_backuping_restoring": 0}"#;

    #[test]
    fn parses_real_listing_with_chinese_and_string_sizes() {
        let l = parse_listing(REAL_LISTING.as_bytes()).unwrap();
        assert_eq!(l.total, 2);
        assert_eq!(l.datas.len(), 2);
        assert!(l.datas[0].isfolder);
        assert_eq!(l.datas[0].filesize, 4096);
        assert_eq!(l.datas[1].filename, "空 格 中文名.txt");
        assert_eq!(l.datas[1].filesize, 29);
        assert_eq!(l.datas[1].epochmt, 1790769710);
        assert!(!l.datas[1].isfolder);
        assert_eq!(l.datas[1].display_size(), 29);
        // 没有 status 字段时视为成功
        l.ensure_ok("get_list").unwrap();
    }

    #[test]
    fn status_error_is_surfaced() {
        let l = parse_listing(
            br#"{ "version": "", "build": "20260723", "status": 5, "success": "true" }"#,
        )
        .unwrap();
        let e = l.ensure_ok("get_list /home/test1").unwrap_err();
        assert!(e.to_string().contains("status=5"), "{e}");
        assert_eq!(e.server_msg(), None);
    }

    /// 服务端说了原因就要透传出来，别让用户只看到「未知状态码」。
    #[test]
    fn status_error_includes_server_msg() {
        let l = parse_listing(
            br#"{"status":8,"success":"true",
                 "msg":"Qsync Central is initializing. Please wait a few minutes and try again."}"#,
        )
        .unwrap();
        let e = l.ensure_ok("get_list /home/test1").unwrap_err();
        assert!(e.is_server_busy());
        assert!(!e.is_auth(), "未就绪不能被判成会话失效");
        let s = e.to_string();
        assert!(s.contains("status=8"), "{s}");
        assert!(s.contains("initializing"), "msg 必须在 Display 里: {s}");
    }

    #[test]
    fn parses_max_log_with_string_numbers() {
        let m = parse_max_log(REAL_MAX_LOG.as_bytes()).unwrap();
        assert_eq!(m.max_log, 37);
        assert_eq!(m.global_notify, 177);
        assert_eq!(m.sync_signal, 1);
    }

    /// 逆向 §1.1：就绪字段与限流信息都从 `max_log` 响应自带，不必额外往返。
    #[test]
    fn max_log_carries_limit_and_readiness_fields() {
        let m = parse_max_log(REAL_MAX_LOG.as_bytes()).unwrap();
        assert_eq!(m.server_limit, Some(256));
        assert_eq!(m.cgi_number, Some(1));
        // 真机这次是正常态
        assert!(m.busy_reason().is_none());
        // sync_signal=1 不是降速
        assert_eq!(m.advised_interval_secs(), None);

        let busy = parse_max_log(
            br#"{"max_log":"0","sync_signal":2,"slowdown_seconds":"15",
                 "is_booting":0,"is_migrating":1}"#,
        )
        .unwrap();
        assert_eq!(busy.busy_reason(), Some("NAS 正在迁移"));
        // sync_signal=2 时按服务端给的秒数退避
        assert_eq!(busy.advised_interval_secs(), Some(15));

        let booting = parse_max_log(br#"{"max_log":"0","is_booting":1}"#).unwrap();
        assert_eq!(booting.busy_reason(), Some("Qsync 服务仍在启动"));
    }

    /// 老 NAS 不回这些字段时不能崩，也不能把「没给」当成 0 上限。
    #[test]
    fn max_log_absent_fields_are_none_not_zero() {
        let m = parse_max_log(br#"{"max_log":"37","sync_signal":1}"#).unwrap();
        assert_eq!(m.server_limit, None);
        assert_eq!(m.cgi_number, None);
        assert!(m.busy_reason().is_none());
    }

    /// 失败响应里的 `msg` 必须能被读出来（status=8 排错全靠它）。
    #[test]
    fn max_log_error_carries_server_msg() {
        let m = parse_max_log(
            br#"{"version":"","build":"20260916","status":8,"success":"true",
                 "msg":"Qsync Central is initializing. Please wait a few minutes and try again."}"#,
        )
        .unwrap();
        assert_eq!(m.status, Some(8));
        assert_eq!(m.server_limit, None);
        assert!(ServerStatus(8).is_server_busy());
        assert!(m.msg.as_deref().unwrap().contains("initializing"));
    }

    #[test]
    fn parses_nas_uid_and_busy_flags() {
        let n = parse_nas_uid(REAL_NAS_UID.as_bytes()).unwrap();
        assert_eq!(n.qsync_version.as_deref(), Some("5.1.1"));
        assert_eq!(n.qpkg_version.as_deref(), Some("5.0.0.7"));
        assert!(n.qbox_cgi);
        assert!(n.busy_reason().is_none());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let l = parse_listing(br#"{"total":0,"datas":[],"brand_new_field":{"a":1}}"#).unwrap();
        assert_eq!(l.total, 0);
    }
}
