//! # qxync-client
//!
//! NAS HTTP API 封装。命名空间分工是**实测**结论，不是推断：
//!
//! | 用途 | 端点 |
//! |---|---|
//! | 登录 | `POST /cgi-bin/authLogin.cgi`（`serviceKey=1` + `pwd=base64`） |
//! | 元数据 | `GET /cgi-bin/qsync/qsyncsrv.cgi?func=…` |
//! | 下载（字节流） | `GET /cgi-bin/filemanager/utilRequest.cgi?func=download&source_path=&source_file=&isfolder=0&source_total=1` |
//! | 上传 | `POST /cgi-bin/qsync/upload.php`（multipart 字段名 `files[]`） |
//!
//! `qsyncsrv.cgi?func=download` 在真机上恒返回 `status:20`（需要 Qsync 同步文件夹会话），
//! 因此数据面一律走 FileStation；这也意味着**只读 M0 不依赖 `q_token`**。

use futures_util::StreamExt;
use qxync_core::{
    build_query, encode_query_value, model::parse_listing, parse_max_log, parse_nas_uid,
    parse_sync_log, DirEntry, Error, LinkConfig, Listing, MaxLog, NasUid, Result, ServerStatus,
    SyncLogBatch,
};
use std::time::Duration;

/// `qbox_write_log` 的 action 码。
///
/// **推断值**：报告未确认枚举；这里的取值来自真机 sync log 里观察到的真实事件
/// （另一台已配对设备的操作日志）：`12` = 新建目录、`14` = 文件新增/修改、`1` = 删除。
/// 服务端对 action **不做校验**（0..5 全回 status 1），所以这些值只影响对端语义。
pub mod write_action {
    pub const DELETE: i64 = 1;
    pub const CREATE_DIR: i64 = 12;
    pub const UPSERT_FILE: i64 = 14;
}

/// 登录后拿到的会话信息。
#[derive(Debug, Clone)]
pub struct Session {
    pub sid: String,
    pub username: String,
    pub uid: Option<String>,
    pub user_cuid: Option<String>,
    pub nas: NasUid,
}

impl Session {
    pub fn busy_reason(&self) -> Option<&'static str> {
        self.nas.busy_reason()
    }
}

pub struct Client {
    http: reqwest::Client,
    link: LinkConfig,
    sid: Option<String>,
}

impl Client {
    pub fn new(link: &LinkConfig) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .danger_accept_invalid_certs(link.insecure)
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(20))
            .user_agent("QSyncLinux/0.1 (qxync-client)");
        if link.ipv4_only {
            // 绑定 IPv4 源地址 → 只走 IPv4（Happy Eyeballs 在对端 IPv6 不通时会先失败）
            builder = builder.local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        }
        let http = builder
            .build()
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(Self {
            http,
            link: link.clone(),
            sid: None,
        })
    }

    pub fn link(&self) -> &LinkConfig {
        &self.link
    }

    pub fn sid(&self) -> Option<&str> {
        self.sid.as_deref()
    }

    /// 用已有 sid 续接会话（例如从 daemon 传来的）。
    pub fn set_sid(&mut self, sid: impl Into<String>) {
        self.sid = Some(sid.into());
    }

    fn require_sid(&self) -> Result<&str> {
        self.sid
            .as_deref()
            .ok_or_else(|| Error::Auth("尚未登录（没有 sid）".into()))
    }

    /// 拼 URL：查询串自己编码（空格必须是 `%20`）。
    fn url(&self, path: &str, query: &[(&str, &str)]) -> String {
        let mut u = format!("{}/{}", self.link.base_url(), path.trim_start_matches('/'));
        if !query.is_empty() {
            u.push('?');
            u.push_str(&build_query(query));
        }
        u
    }

    // ---------------------------------------------------------------- 登录

    /// 阶段 3：`authLogin.cgi`。
    pub async fn login(&mut self, user: &str, password: &str) -> Result<Session> {
        use base64::Engine as _;
        let pwd = base64::engine::general_purpose::STANDARD.encode(password.as_bytes());
        let body = [
            ("user", user),
            ("serviceKey", "1"),
            ("client_app", "Qsync"),
            ("client_agent", "QSyncLinux/0.1"),
            ("gen_client_id", "1"),
            ("remme", "1"),
            ("dont_verify_2sv_again", "0"),
            ("pwd", pwd.as_str()),
        ];
        let url = self.url("cgi-bin/authLogin.cgi", &[]);
        let resp = self
            .http
            .post(&url)
            .header("X-Forwarded-For", "127.0.0.1")
            .form(&body)
            .send()
            .await
            .map_err(|e| Error::Transport(format!("authLogin.cgi: {e}")))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(Error::Auth(format!("authLogin.cgi HTTP {status}")));
        }

        let passed = xml_text(&text, "authPassed").unwrap_or("0");
        if passed != "1" {
            let err = xml_text(&text, "errorValue").unwrap_or("?");
            let need2sv = xml_text(&text, "need_2sv").unwrap_or("0");
            return Err(Error::Auth(format!(
                "authPassed={passed} errorValue={err} need_2sv={need2sv}（检查口令编码：必须是 base64）"
            )));
        }
        let sid = xml_text(&text, "authSid")
            .ok_or_else(|| Error::Auth("authPassed=1 但响应里没有 authSid".into()))?
            .to_string();
        self.sid = Some(sid.clone());

        let username = xml_text(&text, "username").unwrap_or(user).to_string();

        // 阶段 4：NAS UID（替代已 404 的 qsyncsrvPrepare.cgi）
        let nas = self.nas_uid().await.unwrap_or_else(|_| parse_uid_stub());
        Ok(Session {
            sid,
            username,
            uid: nas.uid.clone(),
            user_cuid: nas.user_cuid.clone(),
            nas,
        })
    }

    /// 登出（失败不视为错误）。
    pub async fn logout(&mut self) -> Result<()> {
        if let Some(sid) = self.sid.take() {
            let url = self.url(
                "cgi-bin/qsync/qsyncsrv_logout.cgi",
                &[("sid", sid.as_str()), ("logout", "1")],
            );
            let _ = self.http.get(&url).send().await;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- 能力/元数据

    pub async fn nas_uid(&self) -> Result<NasUid> {
        let body = self.qsync_func("qbox_get_nas_uid", &[]).await?;
        parse_nas_uid(&body)
    }

    pub async fn max_log(&self) -> Result<MaxLog> {
        let body = self.qsync_func("qbox_get_max_log", &[]).await?;
        let m = parse_max_log(&body)?;
        if let Some(s) = m.status {
            let st = ServerStatus(s);
            if !st.is_success() {
                return Err(Error::Status {
                    status: st,
                    context: "qbox_get_max_log".into(),
                });
            }
        }
        Ok(m)
    }

    /// 会话保活：`qbox_get_max_log` 能用就说明 sid 还有效。
    pub async fn check_alive(&self) -> Result<bool> {
        match self.max_log().await {
            Ok(_) => Ok(true),
            Err(Error::Status { .. }) | Err(Error::Auth(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    // ---------------------------------------------------------- M2c 变更发现

    /// `func=qbox_get_sync_log&lower=&number=`：拉一批文件变更事件。
    ///
    /// 真机要点：
    /// * `lower` 是**闭区间下界**（`lower=30` 会包含 `log_id=30`）；
    /// * 没有任何事件时回 `status:-17`（[`qxync_core::sync::is_log_missing`] 可判定），
    ///   这不是错误，调用方**不要推进游标**、转而对账 baseline 即可。
    pub async fn sync_log(
        &self,
        lower: i64,
        number: usize,
        sub_folder: Option<&str>,
    ) -> Result<SyncLogBatch> {
        let lower_s = lower.to_string();
        let number_s = number.to_string();
        let mut extra: Vec<(&str, &str)> = vec![
            ("lower", lower_s.as_str()),
            ("number", number_s.as_str()),
            ("get_detail", "1"),
        ];
        if let Some(f) = sub_folder {
            extra.push(("sub_folder", f));
        }
        let body = self.qsync_func("qbox_get_sync_log", &extra).await?;
        parse_sync_log(&body)
    }

    /// `func=qbox_query_notify&lower=&upper=`：config log / global notify log 的区间查询。
    ///
    /// M2c 只用来**推进游标 + 统计**（共享邀请、团队文件夹等事件不在 M2c 范围内）。
    pub async fn query_notify(&self, lower: u64, upper: u64) -> Result<NotifyBatch> {
        let lower_s = lower.to_string();
        let upper_s = upper.to_string();
        let body = self
            .qsync_func(
                "qbox_query_notify",
                &[("lower", lower_s.as_str()), ("upper", upper_s.as_str())],
            )
            .await?;
        parse_notify(&body, "qbox_query_notify")
    }

    /// `func=qbox_get_device_config_list&user=&lower=&upper=`：设备/同步文件夹配置日志区间。
    pub async fn device_config_list(
        &self,
        user: &str,
        lower: u64,
        upper: u64,
    ) -> Result<NotifyBatch> {
        let lower_s = lower.to_string();
        let upper_s = upper.to_string();
        let body = self
            .qsync_func(
                "qbox_get_device_config_list",
                &[
                    ("user", user),
                    ("lower", lower_s.as_str()),
                    ("upper", upper_s.as_str()),
                ],
            )
            .await?;
        parse_notify(&body, "qbox_get_device_config_list")
    }

    /// 调一个不带额外参数的 `qsyncsrv.cgi?func=…`。
    async fn qsync_func(&self, func: &str, extra: &[(&str, &str)]) -> Result<Vec<u8>> {
        let sid = self.require_sid()?.to_string();
        let mut q: Vec<(&str, &str)> = vec![("func", func), ("sid", sid.as_str())];
        q.extend_from_slice(extra);
        let url = self.url("cgi-bin/qsync/qsyncsrv.cgi", &q);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Transport(format!("{func}: {e}")))?;
        let code = resp.status();
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        if !code.is_success() {
            return Err(Error::Transport(format!("{func}: HTTP {code}")));
        }
        Ok(body.to_vec())
    }

    /// `func=get_list`，自动翻页（`start += len(datas)` 直到 `len < limit` 或 `start >= total`）。
    pub async fn list(&self, path: &str) -> Result<Vec<DirEntry>> {
        const LIMIT: usize = 200;
        let mut out: Vec<DirEntry> = Vec::new();
        let mut start = 0usize;
        loop {
            let sid = self.require_sid()?.to_string();
            let start_s = start.to_string();
            let limit_s = LIMIT.to_string();
            let url = self.url(
                "cgi-bin/qsync/qsyncsrv.cgi",
                &[
                    ("func", "get_list"),
                    ("sid", sid.as_str()),
                    ("is_iso", "0"),
                    ("list_mode", "all"),
                    ("path", path),
                    ("dir", "ASC"),
                    ("limit", limit_s.as_str()),
                    ("sort", "filename"),
                    ("start", start_s.as_str()),
                    ("no_sort", "0"),
                    ("hidden_file", "1"),
                ],
            );
            let resp = self
                .http
                .get(&url)
                .send()
                .await
                .map_err(|e| Error::Transport(format!("get_list {path}: {e}")))?;
            let body = resp
                .bytes()
                .await
                .map_err(|e| Error::Transport(e.to_string()))?;
            let listing: Listing = parse_listing(&body)?;
            listing.ensure_ok(format!("get_list {path}"))?;

            let got = listing.datas.len();
            out.extend(listing.datas);
            if got < LIMIT || (listing.total >= 0 && out.len() as i64 >= listing.total) || got == 0
            {
                break;
            }
            start += got;
            if start > 1_000_000 {
                return Err(Error::Parse(format!("get_list {path} 翻页异常")));
            }
        }
        Ok(out)
    }

    /// `func=stat`：**必须**是 `path=<所在目录>&file_name=<名字>&file_total=1`。
    pub async fn stat(&self, dir: &str, file_name: &str) -> Result<Option<DirEntry>> {
        let sid = self.require_sid()?.to_string();
        let url = self.url(
            "cgi-bin/qsync/qsyncsrv.cgi",
            &[
                ("func", "stat"),
                ("sid", sid.as_str()),
                ("path", dir),
                ("file_name", file_name),
                ("file_total", "1"),
            ],
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Transport(format!("stat {dir}/{file_name}: {e}")))?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let listing: Listing = parse_listing(&body)?;
        listing.ensure_ok(format!("stat {dir}/{file_name}"))?;
        // ★ 只能靠 `exist` 判定：不存在的路径也会返回「占位条目」
        //   （filename 是请求的名字、filesize=0、owner/privilege 为空），但 `exist=0`。
        //   早期用 `!filename.is_empty()` 兜底 → 把不存在的文件当存在，FUSE lookup 误报正项
        //   → `mkdir` 直接 EEXIST（实测踩过）。
        Ok(listing.single().cloned().filter(|e| e.exist))
    }

    pub async fn mkdir(&self, parent: &str, name: &str) -> Result<()> {
        let sid = self.require_sid()?.to_string();
        let url = self.url(
            "cgi-bin/qsync/qsyncsrv.cgi",
            &[("func", "createdir"), ("sid", sid.as_str())],
        );
        // 注意：body 用 form 编码没问题（目录名里的空格在 body 里由 form 编码器处理，
        // 真机实测 `dest_folder` 可直接接受 UTF-8/空格）。
        let resp = self
            .http
            .post(&url)
            .form(&[("dest_path", parent), ("dest_folder", name)])
            .send()
            .await
            .map_err(|e| Error::Transport(format!("createdir: {e}")))?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let listing: Listing = parse_listing(&body)?;
        listing.ensure_ok(format!("createdir {parent}/{name}"))?;
        Ok(())
    }

    // ---------------------------------------------------------------- 数据面

    fn filetool_query<'a>(
        &'a self,
        func: &'a str,
        dir: &'a str,
        name: &'a str,
        sid: &'a str,
    ) -> Vec<(&'a str, &'a str)> {
        vec![
            ("func", func),
            ("sid", sid),
            ("source_path", dir),
            ("source_file", name),
            ("isfolder", "0"),
            ("source_total", "1"),
        ]
    }

    /// 区间下载（M2 的 128 KiB 水合就靠它）。`end` 含端点，返回 200/206 的原始字节。
    pub async fn download_range(
        &self,
        dir: &str,
        name: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>> {
        let sid = self.require_sid()?.to_string();
        let url = self.url(
            "cgi-bin/filemanager/utilRequest.cgi",
            &self.filetool_query("download", dir, name, &sid),
        );
        let resp = self
            .http
            .get(&url)
            .header("Range", format!("bytes={start}-{end}"))
            .send()
            .await
            .map_err(|e| Error::Transport(format!("download {name}: {e}")))?;
        let code = resp.status().as_u16();
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        if code != 200 && code != 206 {
            return Err(Error::Transport(format!(
                "download {dir}/{name}: HTTP {code}, body={}",
                String::from_utf8_lossy(&body[..body.len().min(160)])
            )));
        }
        // 服务端出错时会用 200 + JSON 错误体，这里按内容再兜一层。
        if body.starts_with(b"{") && body.windows(9).any(|w| w == b"\"status\"") {
            if let Ok(l) = parse_listing(&body) {
                l.ensure_ok(format!("download {dir}/{name}"))?;
            }
        }
        Ok(body.to_vec())
    }

    /// 整文件下载到本地（流式；返回写入字节数）。
    pub async fn download_to_file(
        &self,
        dir: &str,
        name: &str,
        dest: &std::path::Path,
    ) -> Result<u64> {
        let sid = self.require_sid()?.to_string();
        let url = self.url(
            "cgi-bin/filemanager/utilRequest.cgi",
            &self.filetool_query("download", dir, name, &sid),
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Transport(format!("download {name}: {e}")))?;
        let code = resp.status().as_u16();
        if code != 200 && code != 206 {
            return Err(Error::Transport(format!(
                "download {dir}/{name}: HTTP {code}"
            )));
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = std::path::PathBuf::from(format!("{}.qsync-part", dest.display()));
        let mut file = tokio::fs::File::create(&tmp).await?;
        let mut written: u64 = 0;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| Error::Transport(e.to_string()))?;
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
            written += chunk.len() as u64;
        }
        tokio::io::AsyncWriteExt::flush(&mut file).await?;
        file.sync_all().await?;
        drop(file);
        // 出错残留的 JSON 错误体会先落到临时文件里；调用方比对长度后再 rename。
        tokio::fs::rename(&tmp, dest).await?;
        Ok(written)
    }

    /// 上传一个文件：`POST /cgi-bin/qsync/upload.php`，multipart 字段名必须是 `files[]`。
    pub async fn upload_bytes(
        &self,
        dest_path: &str,
        filename: &str,
        bytes: Vec<u8>,
    ) -> Result<()> {
        let sid = self.require_sid()?.to_string();
        let url = self.url(
            "cgi-bin/qsync/upload.php",
            &[
                ("sid", sid.as_str()),
                ("dest_path", dest_path),
                ("overwrite", "1"),
                ("type", "standard"),
            ],
        );
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_string())
            .mime_str("application/octet-stream")
            .map_err(|e| Error::Parse(e.to_string()))?;
        let form = reqwest::multipart::Form::new().part("files[]", part);
        let resp = self
            .http
            .post(&url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| Error::Transport(format!("upload {filename}: {e}")))?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        parse_upload_result(&body, filename)
    }

    /// 上传后对齐 mtime（`stat&settime=1`），否则服务端会判定「未同步」。
    pub async fn set_mtime(&self, dir: &str, name: &str, mtime: i64) -> Result<()> {
        let sid = self.require_sid()?.to_string();
        let mtime_s = mtime.to_string();
        let url = self.url(
            "cgi-bin/qsync/qsyncsrv.cgi",
            &[
                ("func", "stat"),
                ("sid", sid.as_str()),
                ("settime", "1"),
                ("mtime", mtime_s.as_str()),
                ("path", dir),
                ("file_total", "1"),
                ("file_name", name),
            ],
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Transport(format!("settime: {e}")))?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        parse_listing(&body)?.ensure_ok(format!("stat&settime {dir}/{name}"))?;
        Ok(())
    }
}

impl Client {
    /// 同目录重命名：FileStation `func=rename`。
    /// 实测 body 字段是 `{path, source_name, dest_name}`（`filename/dest_name` 会被 status 20 拒）。
    /// 大小写改名实测可直接成功（本机 NAS 不需要两阶段）。
    pub async fn rename(&self, dir: &str, from: &str, to: &str) -> Result<()> {
        let sid = self.require_sid()?.to_string();
        let url = self.url(
            "cgi-bin/filemanager/utilRequest.cgi",
            &[("func", "rename"), ("sid", sid.as_str())],
        );
        let resp = self
            .http
            .post(&url)
            .form(&[("path", dir), ("source_name", from), ("dest_name", to)])
            .send()
            .await
            .map_err(|e| Error::Transport(format!("rename: {e}")))?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        parse_listing(&body)?.ensure_ok(format!("rename {dir}/{from} -> {to}"))?;
        Ok(())
    }

    /// 把条目移动到另一个目录：FileStation `func=move`。
    ///
    /// 实测两个坑：
    /// 1. 必须带 **`source_total=1`**（不带会静默不动）；
    /// 2. **`dest_file` 会被忽略** —— 文件在目标目录里保持原文件名，
    ///    所以「跨目录 + 改名」必须拆成 `move_into` + `rename` 两步。
    ///
    /// 服务端异步执行并回 `{"status":1,"pid":N}`，这里轮询 `to_dir/from_name` 出现为止。
    pub async fn move_into(&self, from_dir: &str, from_name: &str, to_dir: &str) -> Result<()> {
        let sid = self.require_sid()?.to_string();
        let url = self.url(
            "cgi-bin/filemanager/utilRequest.cgi",
            &[("func", "move"), ("sid", sid.as_str()), ("no_fork", "1")],
        );
        let resp = self
            .http
            .post(&url)
            .form(&[
                ("source_path", from_dir),
                ("source_file", from_name),
                ("dest_path", to_dir),
                ("dest_file", from_name),
                ("source_total", "1"),
            ])
            .send()
            .await
            .map_err(|e| Error::Transport(format!("move: {e}")))?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        parse_listing(&body)?.ensure_ok(format!("move {from_dir}/{from_name} -> {to_dir}"))?;

        for _ in 0..40 {
            if self.stat(to_dir, from_name).await?.is_some() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err(Error::Transport(format!(
            "move 超时：{to_dir}/{from_name} 未出现（服务端可能仍在后台执行）"
        )))
    }

    /// 删除文件或目录：`qsyncsrv.cgi?func=delete`，body `{path, file_name, file_total=1}`（实测）。
    pub async fn delete_entry(&self, dir: &str, name: &str) -> Result<()> {
        let sid = self.require_sid()?.to_string();
        let url = self.url(
            "cgi-bin/qsync/qsyncsrv.cgi",
            &[("func", "delete"), ("sid", sid.as_str())],
        );
        let resp = self
            .http
            .post(&url)
            .form(&[("path", dir), ("file_name", name), ("file_total", "1")])
            .send()
            .await
            .map_err(|e| Error::Transport(format!("delete: {e}")))?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        parse_listing(&body)?.ensure_ok(format!("delete {dir}/{name}"))?;
        Ok(())
    }

    /// `qbox_write_log`：把本机改动记进服务端日志（让其它设备能发现）。
    ///
    /// ⚠️ 实测：服务端接受任何 action 并回 status 1，但**只有路径落在已注册的同步文件夹里**
    /// 才会真正出现在 `qbox_get_sync_log`（本机测试账号没有注册同步对，所以看不到）。
    /// 设备注册（`qbox_save_device_config`）不在 M2b 范围内，这里做「尽力而为」。
    pub async fn write_log(&self, filepath: &str, action: i64) -> Result<()> {
        let action_s = action.to_string();
        let body = self
            .qsync_func(
                "qbox_write_log",
                &[("filepath", filepath), ("action", action_s.as_str())],
            )
            .await?;
        parse_listing(&body)?.ensure_ok(format!("qbox_write_log {filepath}"))?;
        Ok(())
    }
}

/// 上传响应的判定：`{"status":"1","files":[{"status":"1",...}]}`。
pub fn parse_upload_result(body: &[u8], filename: &str) -> Result<()> {
    let v: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| Error::Parse(format!("upload.php 非 JSON: {e}")))?;
    let file = v.get("files").and_then(|f| f.get(0));
    let ok = file
        .and_then(|f| f.get("status"))
        .map(|s| matches!(s.as_str(), Some("1")) || s.as_i64() == Some(1))
        .unwrap_or(false);
    if !ok {
        let err = file
            .and_then(|f| f.get("error"))
            .and_then(|e| e.as_str())
            .unwrap_or("unknown");
        return Err(Error::Status {
            status: ServerStatus(20),
            context: format!("upload {filename} 被拒绝（error={err}）"),
        });
    }
    Ok(())
}

/// `qbox_query_notify` / `qbox_get_device_config_list` 的通用返回（M2c）。
///
/// 这两个端点的事件结构在报告里标为「△ 未确认」，M2c 只做「取到 + 计数 + 推进游标」，
/// 因此这里保留原始 JSON，不强行反序列化成可能错的字段。
#[derive(Debug, Clone, Default)]
pub struct NotifyBatch {
    pub items: Vec<serde_json::Value>,
}

impl NotifyBatch {
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    /// 事件里出现过的 `device_uid`（去重），用于诊断「这些事件是哪台设备产生的」。
    pub fn device_uids(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for it in &self.items {
            for k in ["device_uid", "duid"] {
                if let Some(s) = it.get(k).and_then(|v| v.as_str()) {
                    if !s.is_empty() && !out.iter().any(|x| x == s) {
                        out.push(s.to_string());
                    }
                }
            }
        }
        out
    }
}

/// 解析 `qbox_query_notify` / `qbox_get_device_config_list` 的 JSON。
/// `status:-17` 与 sync log 一样表示「区间内没有事件」。
pub fn parse_notify(body: &[u8], context: &str) -> Result<NotifyBatch> {
    let v: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| Error::Parse(format!("{context} 解析失败: {e}")))?;
    if let Some(s) = v.get("status").and_then(|x| {
        x.as_i64()
            .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
    }) {
        if s != 0 && s != 1 {
            return Err(Error::status(s, context));
        }
    }
    let items = v
        .get("data")
        .or_else(|| v.get("datas"))
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(NotifyBatch { items })
}

/// 极简 XML 取值（`authLogin.cgi` 的响应是 XML，只有登录需要它）。
fn xml_text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let close = format!("</{tag}>");
    let end = start + xml[start..].find(&close)?;
    let raw = xml[start..end].trim();
    let raw = raw.strip_prefix("<![CDATA[").unwrap_or(raw);
    let raw = raw.strip_suffix("]]>").unwrap_or(raw);
    Some(raw.trim())
}

fn parse_uid_stub() -> NasUid {
    serde_json::from_str("{}").expect("空对象可解析")
}

/// 供 CLI 打印用：把相对路径拼成 NAS 绝对路径。
pub fn join_nas_path(dir: &str, name: &str) -> String {
    format!("{}/{}", dir.trim_end_matches('/'), name)
}

/// 便于外部构造查询串（调试/日志）。
pub fn debug_query(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", encode_query_value(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_extraction_handles_cdata_and_plain() {
        let xml = r#"<QDocRoot><authPassed><![CDATA[1]]></authPassed>
            <authSid><![CDATA[abc123]]></authSid><errorValue>-1</errorValue></QDocRoot>"#;
        assert_eq!(xml_text(xml, "authPassed"), Some("1"));
        assert_eq!(xml_text(xml, "authSid"), Some("abc123"));
        assert_eq!(xml_text(xml, "errorValue"), Some("-1"));
        assert_eq!(xml_text(xml, "missing"), None);
    }

    #[test]
    fn upload_result_accepts_string_and_number_status() {
        let ok = br#"{"status":"1","files":[{"status":"1","name":"a.txt","size":3}]}"#;
        parse_upload_result(ok, "a.txt").unwrap();
        let ok_num = br#"{"status":1,"files":[{"status":1}]}"#;
        parse_upload_result(ok_num, "a.txt").unwrap();
        let bad = br#"{"status":"1","files":[{"status":"-1","error":"acceptFileTypes"}]}"#;
        let e = parse_upload_result(bad, "a.txt").unwrap_err();
        assert!(e.to_string().contains("acceptFileTypes"), "{e}");
    }

    #[test]
    fn debug_query_percent_encodes_space() {
        assert_eq!(
            debug_query(&[("source_file", "空 格.txt")]),
            "source_file=%E7%A9%BA%20%E6%A0%BC.txt"
        );
    }

    #[test]
    fn nas_path_join() {
        assert_eq!(
            join_nas_path("/home/qxync-test/", "a.txt"),
            "/home/qxync-test/a.txt"
        );
    }

    #[test]
    fn notify_batch_parses_and_surfaces_minus_17() {
        let ok = br#"{"status":0,"data":[{"device_uid":"abc","event":"config"},{"duid":"abc"}]}"#;
        let b = parse_notify(ok, "qbox_query_notify").unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b.device_uids(), vec!["abc".to_string()]);
        let empty = br#"{"version":"","build":"","status":-17,"success":"true"}"#;
        let e = parse_notify(empty, "qbox_query_notify").unwrap_err();
        assert!(qxync_core::sync::is_log_missing(&e), "{e}");
    }
}
