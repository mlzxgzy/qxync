//! ★ M40：NAS 身份指纹 —— 防止多路径把数据写到「另一台 NAS」上。
//!
//! 多路径的前提是「所有路径指向同一台 NAS」。如果用户把一台同名/同账号的备份 NAS
//! 的地址误加进路径列表，客户端会正常登录、正常同步 —— 然后把 A 的改动写进 B。
//! 这类错误一旦发生，用户很难立刻发现。
//!
//! 做法：第一次成功登录时记下 `qbox_get_nas_uid` 的 `UID`（拿不到就用 `MAC0`），
//! 之后每次登录（含切路后的重登）都比对；不一致 → 这条路径标记 `blocked` 并换路。
//!
//! * 指纹文件在 `<state>/link-identity/<id>.json`（**不是**配置目录：它是运行期
//!   事实，不该跟 link 配置一起备份/复制到另一台机器）；
//! * `subject`（主地址 host:port）变了 = 用户确实换了一台 NAS → 视为新设备覆盖；
//! * 拿不到 UID / MAC（老固件、接口失败）→ 跳过校验并告警，**不**误杀；
//! * 读写失败只告警，不阻塞登录（宁可少一层保护，也不能让 daemon 起不来）。

use qxync_core::config::sanitize_component;
use qxync_core::{ConfigPaths, LinkConfig, NasUid};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LinkIdentity {
    /// 主地址（`host:port`）—— 用来判断「用户是不是换了另一台 NAS」。
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub uid: Option<String>,
    #[serde(default)]
    pub mac0: Option<String>,
    #[serde(default)]
    pub updated_unix: i64,
}

/// 身份校验结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 第一次见到（或 subject 变了）—— 已记录，放行。
    Recorded,
    /// 与记录一致。
    Match,
    /// 指向另一台 NAS。
    Mismatch { expected: String, actual: String },
    /// 拿不到可比较的标识 —— 跳过校验（不误杀）。
    Unknown,
}

/// 指纹文件路径（按连接隔离）。
pub fn identity_path(paths: &ConfigPaths, link_id: &str) -> PathBuf {
    paths
        .state_dir
        .join("link-identity")
        .join(format!("{}.json", sanitize_component(link_id)))
}

/// 「这台连接自报的身份」。主地址大小写不敏感、端口算进去。
pub fn subject(link: &LinkConfig) -> String {
    format!("{}:{}", link.host.trim().to_ascii_lowercase(), link.port)
}

pub fn load(paths: &ConfigPaths, link: &LinkConfig) -> std::io::Result<Option<LinkIdentity>> {
    let p = identity_path(paths, &link.id);
    match std::fs::read(&p) {
        Ok(raw) => serde_json::from_slice(&raw)
            .map(Some)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn save(paths: &ConfigPaths, link: &LinkConfig, identity: &LinkIdentity) -> std::io::Result<()> {
    let p = identity_path(paths, &link.id);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
        let _ = qxync_core::config::restrict_perms(dir, 0o700);
    }
    let tmp = p.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(identity)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

/// 校验（必要时记录）一次登录拿到的 NAS 身份。
pub fn check_or_record(
    paths: &ConfigPaths,
    link: &LinkConfig,
    nas: &NasUid,
) -> std::io::Result<Verdict> {
    let uid = nas
        .uid
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mac0 = nas
        .mac0
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase);
    if uid.is_none() && mac0.is_none() {
        return Ok(Verdict::Unknown);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let subj = subject(link);
    let existing = load(paths, link)?;
    let Some(mut existing) = existing else {
        let fresh = LinkIdentity {
            subject: subj,
            uid,
            mac0,
            updated_unix: now,
        };
        save(paths, link, &fresh)?;
        return Ok(Verdict::Recorded);
    };
    if existing.subject != subj {
        // 主地址变了：用户明确换了 NAS（或换了连接锚点）→ 重新记录，不做误杀。
        existing.subject = subj;
        existing.uid = uid;
        existing.mac0 = mac0;
        existing.updated_unix = now;
        save(paths, link, &existing)?;
        return Ok(Verdict::Recorded);
    }
    // 优先比 UID；记录里没有 UID 时比 MAC0；两边都缺 → 跳过。
    if let (Some(expected), Some(actual)) = (&existing.uid, &uid) {
        if expected != actual {
            return Ok(Verdict::Mismatch {
                expected: expected.clone(),
                actual: actual.clone(),
            });
        }
        return Ok(Verdict::Match);
    }
    if let (Some(expected), Some(actual)) = (&existing.mac0, &mac0) {
        if expected != actual {
            return Ok(Verdict::Mismatch {
                expected: expected.clone(),
                actual: actual.clone(),
            });
        }
        return Ok(Verdict::Match);
    }
    // 记录里有 UID、这次只拿到 MAC（或反过来）：补全后按一致处理。
    if existing.uid.is_none() {
        existing.uid = uid;
    }
    if existing.mac0.is_none() {
        existing.mac0 = mac0;
    }
    existing.updated_unix = now;
    save(paths, link, &existing)?;
    Ok(Verdict::Match)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(tag: &str) -> ConfigPaths {
        let base =
            std::env::temp_dir().join(format!("qxync-m40-identity-{tag}-{}", std::process::id()));
        let p = ConfigPaths {
            config_dir: base.join("config"),
            data_dir: base.join("data"),
            state_dir: base.join("state"),
        };
        p.ensure_dirs().unwrap();
        p
    }

    fn link(host: &str) -> LinkConfig {
        LinkConfig {
            id: "default".into(),
            host: host.into(),
            port: 9834,
            https: true,
            insecure: false,
            user: "u".into(),
            ipv4_only: false,
            endpoints: Vec::new(),
            local_namespace: None,
            exclude: Vec::new(),
            filter_temp: true,
            peer_listen: None,
            peer_name: None,
        }
    }

    fn uid(s: &str) -> NasUid {
        serde_json::from_value(serde_json::json!({"UID": s, "MAC0": "aabbccddeeff"})).unwrap()
    }

    #[test]
    fn m40_first_login_records_then_matches() {
        let p = paths("match");
        let l = link("nas.local");
        assert_eq!(
            check_or_record(&p, &l, &uid("UID-A")).unwrap(),
            Verdict::Recorded
        );
        assert_eq!(
            check_or_record(&p, &l, &uid("UID-A")).unwrap(),
            Verdict::Match
        );
        std::fs::remove_dir_all(p.state_dir.parent().unwrap()).ok();
    }

    #[test]
    fn m40_different_nas_is_a_mismatch() {
        let p = paths("mismatch");
        let l = link("nas.local");
        check_or_record(&p, &l, &uid("UID-A")).unwrap();
        match check_or_record(&p, &l, &uid("UID-B")).unwrap() {
            Verdict::Mismatch { expected, actual } => {
                assert_eq!(expected, "UID-A");
                assert_eq!(actual, "UID-B");
            }
            other => panic!("必须是 Mismatch，实际 {other:?}"),
        }
        // 指纹不能被一次错误登录覆盖（否则下一次就「合法」了）
        assert_eq!(
            check_or_record(&p, &l, &uid("UID-A")).unwrap(),
            Verdict::Match
        );
        std::fs::remove_dir_all(p.state_dir.parent().unwrap()).ok();
    }

    #[test]
    fn m40_subject_change_and_missing_ids_are_not_fatal() {
        let p = paths("subject");
        let a = link("a.local");
        check_or_record(&p, &a, &uid("UID-A")).unwrap();
        // 主地址变了 = 换 NAS，按新设备记录
        let b = link("b.local");
        assert_eq!(
            check_or_record(&p, &b, &uid("UID-B")).unwrap(),
            Verdict::Recorded
        );
        // 拿不到任何标识 → Unknown（跳过校验，不误杀）
        let empty: NasUid = serde_json::from_str("{}").unwrap();
        assert_eq!(check_or_record(&p, &b, &empty).unwrap(), Verdict::Unknown);
        std::fs::remove_dir_all(p.state_dir.parent().unwrap()).ok();
    }
}
