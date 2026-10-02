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
    parse_sync_log, DirEntry, Error, LinkConfig, Listing, MaxLog, NasUid, ProxySettings, ProxySpec,
    Result, ServerStatus, Settings, SyncLogBatch,
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

/// ★ M7：LAN 对等协议（设备配对 / 事件快路径 / 直传）。
pub mod peer;

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
    /// 不带代理覆盖（= reqwest 默认行为：读 `http_proxy` 等环境变量）。
    ///
    /// 「无代理」与「手动代理」必须用 [`Client::new_with_proxy`]，因为 reqwest
    /// 默认会自己读环境变量 —— 不显式 `no_proxy()` 就关不掉。
    pub fn new(link: &LinkConfig) -> Result<Self> {
        Self::build(link, None)
    }

    /// ★ M8.4：按全局设置里的代理策略构造。
    pub fn new_with_settings(link: &LinkConfig, settings: &Settings) -> Result<Self> {
        Self::new_with_proxy(link, Some(&settings.proxy))
    }

    /// ★ M8.4：按 `ProxySettings` 构造（`None` = 不覆盖环境变量）。
    pub fn new_with_proxy(link: &LinkConfig, proxy: Option<&ProxySettings>) -> Result<Self> {
        Self::build(link, proxy)
    }

    fn build(link: &LinkConfig, proxy: Option<&ProxySettings>) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .danger_accept_invalid_certs(link.insecure)
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(20))
            .user_agent(concat!(
                "qxync/",
                env!("CARGO_PKG_VERSION"),
                " (qxync-client)"
            ));
        if let Some(p) = proxy {
            match p.resolve()? {
                // 显式关掉：reqwest 默认会读环境变量，不显式关就关不掉
                ProxySpec::None => builder = builder.no_proxy(),
                // 自动检测 = reqwest 默认（环境变量）
                ProxySpec::Auto => {}
                ProxySpec::Manual { url, auth } => {
                    let mut pr = reqwest::Proxy::all(&url)
                        .map_err(|e| Error::Transport(format!("代理地址无效 {url:?}: {e}")))?;
                    if let Some((user, pass)) = auth {
                        pr = pr.basic_auth(&user, &pass);
                    }
                    builder = builder.proxy(pr);
                }
            }
        }
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
            ("client_agent", concat!("qxync/", env!("CARGO_PKG_VERSION"))),
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
        let tmp = std::path::PathBuf::from(format!("{}.qxync-part", dest.display()));
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

// ------------------------------------------------- M5 版本化 / 增量 delta
//
// 真机探测原文见 `xtask/probe/probe-out/m5-versioning/`（只读参考）。要点：
// * **只有新命名空间 `cgi-bin/qsync/qsyncsrv.cgi` 认 `versioning_*`**；
//   旧命名空间 `cgi-bin/filemanager/utilRequest.cgi` 对全部 `versioning_*` 回 `status:20`（未知 func）。
// * `versioning_lock&create_version=1` 真机可用（回 `lockid` + `version_id`），
//   但这台 NAS 的 `versioning_support` 全为 0、`versioning_stat_delta` 恒 `{"exist":0,"size":"---"}`，
//   所以 [`Client::delta_gate`] 目前必然判 `Unavailable` —— 这是**服务端能力**问题，不是解析问题。

/// `func=versioning_probe` 的能力探测结果。
///
/// 真机（QTS 5.2.9 / Qsync QPKG 20260723）响应原文：
/// `{"versioning_version": "1.0.0", "qbox_versioning_enable": 1, "qbox_user_versioning_enable": 1, "versioning_enable": 1}`。
///
/// 三个开关**都要为真**才算「服务端开放了版本化」：`versioning_enable` 是 QTS 全局版本化，
/// `qbox_versioning_enable` 是 Qsync 侧开关，`qbox_user_versioning_enable` 是当前用户开关。
#[derive(Debug, Clone)]
pub struct VersioningProbe {
    /// 版本化 CGI 版本号（反汇编里 `WFMQsyncGetVersioningProbe` 要求非空）。
    pub versioning_version: Option<String>,
    pub versioning_enable: bool,
    pub qbox_versioning_enable: bool,
    pub qbox_user_versioning_enable: bool,
    /// 原始 JSON（服务端以后改键名时仍能自查）。
    pub raw: serde_json::Value,
}

impl VersioningProbe {
    /// 三个开关都打开才算「服务端启用了版本化」。
    pub fn enabled(&self) -> bool {
        self.versioning_enable && self.qbox_versioning_enable && self.qbox_user_versioning_enable
    }

    /// 给 [`Client::delta_gate`] 用的**可读原因**：三个开关全开时返回 `None`。
    pub fn disabled_reason(&self) -> Option<String> {
        if !self.versioning_enable {
            return Some("版本化未启用（versioning_enable=0）".into());
        }
        if !self.qbox_versioning_enable {
            return Some("Qsync 版本化未启用（qbox_versioning_enable=0）".into());
        }
        if !self.qbox_user_versioning_enable {
            return Some("当前用户未启用版本化（qbox_user_versioning_enable=0）".into());
        }
        None
    }
}

/// `func=versioning_lock` 的结果。
///
/// 真机 `lockid` / `version_id` 都是**字符串**（`"1790834873-5654"` / `"1790834873"`，前者是
/// `<version_id>-<序号>`），失败时是 `"---"`，所以这里**不要**当数字解析。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersioningLock {
    /// 原始 `status`：成功是 `1`；`4` / `12` 表示 `source_path` 非法（相对路径被拒）。
    pub status: i64,
    pub lockid: String,
    pub version_id: String,
}

/// `func=versioning_stat_delta` 的结果。真机没有历史版本时回 `{"exist": 0, "size": "---"}`。
#[derive(Debug, Clone)]
pub struct DeltaInfo {
    /// 服务端是否存在可算 delta 的旧版本（`exist:0` → `false`）。
    pub exist: bool,
    /// `size` 是数字或数字字符串；`"---"`（无 delta）→ `None`。
    pub size: Option<u64>,
    pub raw: serde_json::Value,
}

/// 某个远端文件「能不能走增量」的判定结果（给上层与验收用）。
///
/// **判定规则一句话**：`stat` 条目的 `versioning_support` 为真、`versioning_probe` 三个开关全开、
/// 能拿到锁并且 `versioning_stat_delta` 回 `exist=1` → `Available`，任何一步失败/不满足 → `Unavailable`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeltaGate {
    /// 服务端可用：`stat_delta` 说 `exist=1`（旧版本存在、可算 delta）。
    Available {
        version_id: String,
        delta_size: Option<u64>,
    },
    /// 不可用（附**可读原因**）：例如 `versioning_support=0（该目录没有历史版本）` /
    /// `stat_delta exist=0` / `版本化未启用`。
    Unavailable { reason: String },
}

impl DeltaGate {
    pub fn is_available(&self) -> bool {
        matches!(self, DeltaGate::Available { .. })
    }

    /// `Unavailable` 时给出原因。
    pub fn reason(&self) -> Option<&str> {
        match self {
            DeltaGate::Available { .. } => None,
            DeltaGate::Unavailable { reason } => Some(reason),
        }
    }
}

/// 解析 `versioning_probe` 的 JSON。
///
/// 带 `status` 且非成功（例如把 `versioning_probe` 打到旧命名空间会回 `status:20`）时**报错**，
/// 而不是假装「能力全 false」——否则 [`Client::delta_gate`] 会给出误导性的原因。
pub fn parse_versioning_probe(body: &[u8]) -> Result<VersioningProbe> {
    let v: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| Error::Parse(format!("versioning_probe 非 JSON: {e}")))?;
    if let Some(s) = v.get("status").and_then(json_i64) {
        if !ServerStatus(s).is_success() {
            return Err(Error::status(s, "versioning_probe"));
        }
    }
    Ok(VersioningProbe {
        versioning_version: v
            .get("versioning_version")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        versioning_enable: v.get("versioning_enable").map(json_bool).unwrap_or(false),
        qbox_versioning_enable: v
            .get("qbox_versioning_enable")
            .map(json_bool)
            .unwrap_or(false),
        qbox_user_versioning_enable: v
            .get("qbox_user_versioning_enable")
            .map(json_bool)
            .unwrap_or(false),
        raw: v,
    })
}

/// 解析 `versioning_lock` 的 JSON（**不求成功**：`status` 原样带出来，由调用方判定）。
pub fn parse_versioning_lock(body: &[u8]) -> Result<VersioningLock> {
    let v: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| Error::Parse(format!("versioning_lock 非 JSON: {e}")))?;
    let s = |k: &str| -> String {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or("---")
            .to_string()
    };
    Ok(VersioningLock {
        // 没有 status 视为失败（别把畸形响应当成功）
        status: v.get("status").and_then(json_i64).unwrap_or(-1),
        lockid: s("lockid"),
        version_id: s("version_id"),
    })
}

/// 解析 `versioning_unlock`：`status` 成功且 `success` 不是 false/0 才算释放成功。
pub fn parse_versioning_unlock(body: &[u8]) -> Result<bool> {
    let v: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| Error::Parse(format!("versioning_unlock 非 JSON: {e}")))?;
    let ok = v
        .get("status")
        .and_then(json_i64)
        .map(|s| ServerStatus(s).is_success())
        .unwrap_or(false);
    // 真机成功响应：{ "status": 1, "success": "true" }
    let success = v.get("success").map(json_bool).unwrap_or(true);
    Ok(ok && success)
}

/// 解析 `versioning_stat_delta`。`size` 是字符串 `"---"`（或缺失）→ `None`。
pub fn parse_delta_info(body: &[u8]) -> Result<DeltaInfo> {
    let v: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| Error::Parse(format!("versioning_stat_delta 非 JSON: {e}")))?;
    if let Some(s) = v.get("status").and_then(json_i64) {
        if !ServerStatus(s).is_success() {
            return Err(Error::status(s, "versioning_stat_delta"));
        }
    }
    Ok(DeltaInfo {
        exist: v.get("exist").map(json_bool).unwrap_or(false),
        size: v.get("size").and_then(json_u64),
        raw: v,
    })
}

/// `versioning_gen_sig` 只把原始 JSON 交出去：真机回 `{"status": 33, "pid": 5703}`
/// （`33` = 目标不可写/参数不适用，说明这台 NAS 没有可供造签名的旧版本），
/// 具体语义由上层按报告 §6.5 判断。
pub fn parse_gen_sig(body: &[u8]) -> Result<serde_json::Value> {
    serde_json::from_slice(body)
        .map_err(|e| Error::Parse(format!("versioning_gen_sig 非 JSON: {e}")))
}

/// `versioning_support=0` 的判定（[`Client::delta_gate`] 第 1 步）。
fn unsupported_gate(dir: &str, name: &str) -> DeltaGate {
    DeltaGate::Unavailable {
        reason: format!("versioning_support=0（{dir}/{name} 没有可用的历史版本）"),
    }
}

/// `stat_delta` 之后的判定（[`Client::delta_gate`] 第 3 步的收尾）。
fn verdict_from_delta_info(version_id: &str, info: &DeltaInfo) -> DeltaGate {
    if info.exist {
        DeltaGate::Available {
            version_id: version_id.to_string(),
            delta_size: info.size,
        }
    } else {
        DeltaGate::Unavailable {
            reason: format!(
                "stat_delta exist=0（version_id={version_id} 没有可算 delta 的旧版本）"
            ),
        }
    }
}

fn json_i64(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.parse().ok(),
        serde_json::Value::Bool(b) => Some(*b as i64),
        _ => None,
    }
}

fn json_u64(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => n.as_u64(),
        // 真机是字符串："---" 解析失败 → None
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn json_bool(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
        serde_json::Value::String(s) => matches!(s.as_str(), "1" | "true" | "True"),
        _ => false,
    }
}

impl Client {
    /// `func=versioning_probe`：读服务端版本化能力（响应字段见 [`VersioningProbe`]）。
    ///
    /// ⚠️ 只有新命名空间 `qsyncsrv.cgi` 认它；旧命名空间会回 `status:20`，这里会转成 `Err`。
    pub async fn versioning_probe(&self) -> Result<VersioningProbe> {
        let body = self.qsync_func("versioning_probe", &[]).await?;
        parse_versioning_probe(&body)
    }

    /// `func=versioning_lock`：`create=true` → `create_version=1` 新建锁；
    /// `create=false` → `check_version=1` + 已持有的 `version_id`/`lockid` 复核（报告 §6.4）。
    ///
    /// 成功时服务端回 `status:1`，这里返回 `Ok(VersioningLock)`；`status` 非成功（如 `4`/`12`
    /// 表示路径非法）→ `Err(Error::Status{..})`，`lockid`/`version_id` 在这种响应里是 `"---"`。
    ///
    /// **用完必须 [`Client::versioning_unlock`]**（[`Client::delta_gate`] 会自己保证释放）。
    pub async fn versioning_lock(
        &self,
        source_path: &str,
        source_file: &str,
        create: bool,
        version_id: Option<&str>,
        lockid: Option<&str>,
    ) -> Result<VersioningLock> {
        let mut extra: Vec<(&str, &str)> = Vec::new();
        if create {
            extra.push(("create_version", "1"));
        } else {
            extra.push(("check_version", "1"));
        }
        if let Some(v) = version_id {
            extra.push(("version_id", v));
        }
        if let Some(l) = lockid {
            extra.push(("lockid", l));
        }
        extra.push(("source_path", source_path));
        extra.push(("source_file", source_file));
        let body = self.qsync_func("versioning_lock", &extra).await?;
        let lock = parse_versioning_lock(&body)?;
        if !ServerStatus(lock.status).is_success() {
            return Err(Error::Status {
                status: ServerStatus(lock.status),
                context: format!("versioning_lock {source_path}/{source_file}"),
            });
        }
        Ok(lock)
    }

    /// `func=versioning_unlock`：释放版本锁。`true` = 服务端确认释放。
    ///
    /// 真机成功响应：`{ "status": 1, "success": "true" }`。
    pub async fn versioning_unlock(
        &self,
        source_path: &str,
        source_file: &str,
        lockid: &str,
    ) -> Result<bool> {
        let body = self
            .qsync_func(
                "versioning_unlock",
                &[
                    ("lockid", lockid),
                    ("source_path", source_path),
                    ("source_file", source_file),
                ],
            )
            .await?;
        parse_versioning_unlock(&body)
    }

    /// `func=versioning_stat_delta`：查询 `version_id` 对应的旧版本能否算 delta、多大。
    ///
    /// **注意不带 `lockid`**（报告 §6.4）。真机无历史版本时回 `{"exist":0,"size":"---"}`。
    pub async fn versioning_stat_delta(
        &self,
        source_path: &str,
        source_file: &str,
        version_id: &str,
    ) -> Result<DeltaInfo> {
        let body = self
            .qsync_func(
                "versioning_stat_delta",
                &[
                    ("version_id", version_id),
                    ("source_path", source_path),
                    ("source_file", source_file),
                ],
            )
            .await?;
        parse_delta_info(&body)
    }

    /// `func=versioning_gen_sig`：让服务端为旧版本生成 signature（报告 §6.5 分支 B 第 1 步）。
    ///
    /// 只返回原始 JSON：真机回 `{"status": 33, "pid": 5703}`（`33` = 目标不可写/参数不适用），
    /// 后续可接 `versioning_get_sig` 下载签名（M5 未实现）。
    pub async fn versioning_gen_sig(
        &self,
        source_path: &str,
        source_file: &str,
        lockid: &str,
        version_id: &str,
    ) -> Result<serde_json::Value> {
        let body = self
            .qsync_func(
                "versioning_gen_sig",
                &[
                    ("lockid", lockid),
                    ("version_id", version_id),
                    ("source_path", source_path),
                    ("source_file", source_file),
                ],
            )
            .await?;
        parse_gen_sig(&body)
    }

    /// 判定某个远端文件**能不能走增量**（M5 的能力门）。
    ///
    /// 顺序（任一步失败/不满足都返回 [`DeltaGate::Unavailable`]）：
    /// 1. `stat(dir,name)` 的条目必须是存在文件且 `versioning_support=true`；
    /// 2. `versioning_probe` 的三个 enable 位必须全开；
    /// 3. `versioning_lock(create=1)` 拿 `version_id` → `versioning_stat_delta` 必须 `exist=1`；
    ///    **锁无论如何都会尽力 unlock**（包括中间失败），不会把锁留在服务端。
    ///
    /// 网络/协议错误只当 `Unavailable`（附原因），**绝不会**把错误当 `Available`。
    pub async fn delta_gate(&self, dir: &str, name: &str) -> DeltaGate {
        // 1) 条目元数据：存在 + 是文件 + versioning_support
        let entry = match self.stat(dir, name).await {
            Ok(Some(e)) => e,
            Ok(None) => {
                return DeltaGate::Unavailable {
                    reason: format!("stat {dir}/{name} 不存在（或不可见）"),
                }
            }
            Err(e) => {
                return DeltaGate::Unavailable {
                    reason: format!("stat {dir}/{name} 失败: {e}"),
                }
            }
        };
        if entry.isfolder {
            return DeltaGate::Unavailable {
                reason: format!("{dir}/{name} 是目录，delta 只对文件有意义"),
            };
        }
        if !entry.versioning_support {
            return unsupported_gate(dir, name);
        }

        // 2) 服务端版本化能力
        let probe = match self.versioning_probe().await {
            Ok(p) => p,
            Err(e) => {
                return DeltaGate::Unavailable {
                    reason: format!("versioning_probe 失败: {e}"),
                }
            }
        };
        if let Some(reason) = probe.disabled_reason() {
            return DeltaGate::Unavailable { reason };
        }

        // 3) 加锁 → stat_delta；从这里起无论走哪条分支都必须 unlock
        let lock = match self.versioning_lock(dir, name, true, None, None).await {
            Ok(l) => l,
            Err(e) => {
                return DeltaGate::Unavailable {
                    reason: format!("versioning_lock 失败: {e}"),
                }
            }
        };
        if lock.lockid.is_empty() || lock.lockid == "---" {
            let gate = DeltaGate::Unavailable {
                reason: format!(
                    "versioning_lock 没返回有效 lockid（lockid={:?}）",
                    lock.lockid
                ),
            };
            let _ = self.versioning_unlock(dir, name, &lock.lockid).await;
            return gate;
        }

        let gate = match self
            .versioning_stat_delta(dir, name, &lock.version_id)
            .await
        {
            Ok(d) => verdict_from_delta_info(&lock.version_id, &d),
            Err(e) => DeltaGate::Unavailable {
                reason: format!("stat_delta 失败: {e}"),
            },
        };
        // ★ 尽力释放：即使 stat_delta 失败/网络断了也要让服务端解锁
        match (gate, self.versioning_unlock(dir, name, &lock.lockid).await) {
            (DeltaGate::Unavailable { reason }, Err(e)) => DeltaGate::Unavailable {
                reason: format!("{reason}；另外 unlock 失败: {e}"),
            },
            (g, _) => g,
        }
    }
}

// ------------------------------------------------------- M6 同步文件夹列表
//
// `GET /cgi-bin/qsync/qsyncsrv.cgi?func=qbox_get_syncing_folder_list&sid=…`（新命名空间）。
// 真机实测（用户 test1）响应原文：
// `{"total": 0, "client_key": "754879e7…", "folder" :[]}` —— **端点可用，但该账号没有登记
// 任何 Qsync 同步文件夹**。所以「空列表」是正常状态，绝不报错。

impl Client {
    /// ★ M6：NAS 上报的 Qsync 同步文件夹（= 用户/设备在 Qsync 里配了同步的共享文件夹）。
    ///
    /// 普通账号没配对时返回空 `Vec`（实测如此），**不报错**；只有网络/HTTP、非 JSON、
    /// 或带上非成功 `status` 时才回 `Err`。复用 core 的
    /// [`qxync_core::ipc::SyncingFolderInfo`]，不另造类型。
    pub async fn syncing_folders(&self) -> Result<Vec<qxync_core::ipc::SyncingFolderInfo>> {
        let body = self.qsync_func("qbox_get_syncing_folder_list", &[]).await?;
        let v: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| Error::Parse(format!("qbox_get_syncing_folder_list 非 JSON: {e}")))?;
        // 真机正常响应里没有 `status`；一旦出现且非成功就按协议错误处理。
        if let Some(s) = v.get("status").and_then(json_i64) {
            if !ServerStatus(s).is_success() {
                return Err(Error::status(s, "qbox_get_syncing_folder_list"));
            }
        }
        Ok(parse_syncing_folders(&v))
    }
}

/// 解析 `qbox_get_syncing_folder_list` 的 JSON（纯函数，便于离线单测）。
///
/// 容错要点（真机实测 + 报告 §4.2 的宽松字段表）：
/// * `folder` 可能是数组，也可能**缺失 / 是 `null` / 类型不对** → 一律当**空列表**，
///   「没配对」是正常状态，不报错；`"folder" :[]`（冒号前空格）由 `serde_json` 自行容错；
/// * `permission` / `read_deletable` 可能是数字或字符串 → 宽松解析（`"3"` / `"1"` 都认）；
/// * `realpath` / `volume_id`（别名 `vol_id`）的**空串当作 `None`**；
/// * `total` 与数组长度不一致**不报错**，以数组为准（`total` 只是提示）；
/// * 数组里的非对象条目直接跳过。
pub fn parse_syncing_folders(raw: &serde_json::Value) -> Vec<qxync_core::ipc::SyncingFolderInfo> {
    let Some(items) = raw.get("folder").and_then(|f| f.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|it| {
            let obj = it.as_object()?;
            let s = |k: &str| -> Option<String> {
                obj.get(k)
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            };
            Some(qxync_core::ipc::SyncingFolderInfo {
                folder: s("folder").unwrap_or_default(),
                permission: obj.get("permission").and_then(json_i64).unwrap_or(0),
                read_deletable: obj.get("read_deletable").map(json_bool).unwrap_or(false),
                realpath: s("realpath"),
                // 报告 §4.2 里同一字段有 `volume_id` / `vol_id` 两种拼法
                volume_id: s("volume_id").or_else(|| s("vol_id")),
            })
        })
        .collect()
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

    // ---- M5 版本化 / 增量 delta：输入全部是 `xtask/probe/probe-out/m5-versioning/` 的真机响应原文

    #[test]
    fn versioning_probe_parses_real_response() {
        // 20_versioning_probe_new.json
        let body = br#"{"versioning_version": "1.0.0", "qbox_versioning_enable": 1, "qbox_user_versioning_enable": 1, "versioning_enable": 1}"#;
        let p = parse_versioning_probe(body).unwrap();
        assert_eq!(p.versioning_version.as_deref(), Some("1.0.0"));
        assert!(p.versioning_enable);
        assert!(p.qbox_versioning_enable);
        assert!(p.qbox_user_versioning_enable);
        assert!(p.enabled());
        assert_eq!(p.disabled_reason(), None);
        assert_eq!(p.raw["versioning_version"], "1.0.0");
    }

    #[test]
    fn versioning_probe_disabled_flags_and_old_namespace() {
        // 全 0：必须给出可读原因（gate 第 2 步的输入）
        let off = br#"{"versioning_version":"1.0.0","versioning_enable":0,"qbox_versioning_enable":0,"qbox_user_versioning_enable":0}"#;
        let p = parse_versioning_probe(off).unwrap();
        assert!(!p.enabled());
        let r = p.disabled_reason().expect("全 0 必须给原因");
        assert!(r.contains("versioning_enable=0"), "{r}");
        let gate = DeltaGate::Unavailable { reason: r };
        assert!(!gate.is_available());

        // 只有用户级关掉时原因要指向用户开关
        let user_off = br#"{"versioning_version":"1.0.0","versioning_enable":1,"qbox_versioning_enable":1,"qbox_user_versioning_enable":0}"#;
        let p2 = parse_versioning_probe(user_off).unwrap();
        assert!(p2
            .disabled_reason()
            .unwrap()
            .contains("qbox_user_versioning_enable=0"));

        // 旧命名空间 utilRequest.cgi 把未知 func 当 status 20 拒（73_old_bogus_func.json 同形）
        let old =
            br#"{ "version": "6.0.5.7994", "build": "20260914", "status": 20, "success": "true" }"#;
        let e = parse_versioning_probe(old).unwrap_err();
        assert!(e.to_string().contains("status=20"), "{e}");
    }

    #[test]
    fn versioning_lock_keeps_string_ids() {
        // 30_lock_new_spvar_abs.json / 40_lock_check_version_new.json
        let ok = br#"{"status": 1, "lockid": "1790834873-5654", "version_id": "1790834873"}"#;
        let l = parse_versioning_lock(ok).unwrap();
        assert_eq!(l.status, 1);
        assert_eq!(
            l.lockid, "1790834873-5654",
            "lockid 必须保持 <vid>-<seq> 字符串"
        );
        assert_eq!(l.version_id, "1790834873");
        assert!(ServerStatus(l.status).is_success());

        // 相对路径被拒（30_lock_new_spvar_dot.json）：status 4，两个 id 都是 "---"
        let bad = br#"{"status": 4, "lockid": "---", "version_id": "---"}"#;
        let b = parse_versioning_lock(bad).unwrap();
        assert_eq!(b.status, 4);
        assert_eq!(b.lockid, "---");
        assert!(!ServerStatus(b.status).is_success());
        // 30_lock_new_spvar_dotdot.json：status 12
        assert_eq!(
            parse_versioning_lock(br#"{"status": 12, "lockid": "---", "version_id": "---"}"#)
                .unwrap()
                .status,
            12
        );
    }

    #[test]
    fn stat_delta_dash_size_is_none() {
        // 43_stat_delta_new_withvid.json / 65_stat_delta_new_bigbin.json
        let d = parse_delta_info(br#"{"exist": 0, "size": "---"}"#).unwrap();
        assert!(!d.exist);
        assert_eq!(d.size, None, "\"---\" 必须解析成 None 而不是数字");
        assert_eq!(d.raw["size"], "---");
        let d0 = parse_delta_info(br#"{"exist":0,"size":"---"}"#).unwrap();
        assert!(!d0.exist);

        // 不带 version_id 时（50_stat_delta_new_novid_spvar_abs.json）只回 status:0，没有 exist 字段
        let novid = br#"{ "version": "", "build": "20260723", "status": 0, "success": "true" }"#;
        let d2 = parse_delta_info(novid).unwrap();
        assert!(!d2.exist);
        assert_eq!(d2.size, None);

        // 真有 delta：数字字符串与纯数字都要能解析
        let d3 = parse_delta_info(br#"{"exist":1,"size":"4096"}"#).unwrap();
        assert!(d3.exist);
        assert_eq!(d3.size, Some(4096));
        assert_eq!(
            parse_delta_info(br#"{"exist":true,"size":4096}"#)
                .unwrap()
                .size,
            Some(4096)
        );
    }

    #[test]
    fn unlock_and_gen_sig_parse_real_responses() {
        // 90_unlock_new.json / 67_unlock_new_bigbin.json
        let ok = br#"{ "version": "", "build": "20260723", "status": 1, "success": "true" }"#;
        assert!(parse_versioning_unlock(ok).unwrap());
        // 非成功 status 不能当释放成功
        let rejected = br#"{ "version": "6.0.5.7994", "status": 20, "success": "true" }"#;
        assert!(!parse_versioning_unlock(rejected).unwrap());

        // 41_gen_sig_new.json：原样交出 JSON
        let sig = parse_gen_sig(br#"{"status": 33, "pid": 5703}"#).unwrap();
        assert_eq!(sig["status"], 33);
        assert_eq!(sig["pid"], 5703);
    }

    /// `DeltaGate` 的三条路径（用真机响应驱动，不联网）。
    #[test]
    fn delta_gate_three_paths() {
        // 路径 1：get_list 条目 versioning_support=0 → Unavailable
        let listing = br#"{"status":0,"total":2,"datas":[
            {"filename":"hello.txt","isfolder":0,"filesize":"24","versioning_support":0,"exist":1},
            {"filename":"v.txt","isfolder":0,"filesize":"24","versioning_support":1,"exist":1}]}"#;
        let l: Listing = parse_listing(listing).unwrap();
        assert!(
            !l.datas[0].versioning_support,
            "versioning_support:0 → false"
        );
        assert!(l.datas[1].versioning_support, "versioning_support:1 → true");
        let g1 = unsupported_gate("/home/qxync-test", "hello.txt");
        assert_eq!(
            g1,
            DeltaGate::Unavailable {
                reason: "versioning_support=0（/home/qxync-test/hello.txt 没有可用的历史版本）"
                    .into()
            }
        );
        assert!(g1.reason().unwrap().contains("versioning_support=0"));

        // 路径 2：enable 全 0 → Unavailable
        let probe = parse_versioning_probe(
            br#"{"versioning_version":"1.0.0","versioning_enable":0,"qbox_versioning_enable":0,"qbox_user_versioning_enable":0}"#,
        )
        .unwrap();
        let g2 = DeltaGate::Unavailable {
            reason: probe.disabled_reason().expect("必须给原因"),
        };
        assert!(!g2.is_available());

        // 路径 3：stat_delta exist=1 + size 正常 → Available
        let info = parse_delta_info(br#"{"exist":1,"size":"123456"}"#).unwrap();
        let g3 = verdict_from_delta_info("1790834873", &info);
        assert_eq!(
            g3,
            DeltaGate::Available {
                version_id: "1790834873".into(),
                delta_size: Some(123456)
            }
        );
        assert!(g3.is_available());
        assert_eq!(g3.reason(), None);

        // 路径 3 的否分支：exist=0 → Unavailable（真机当前就是这条）
        let none = parse_delta_info(br#"{"exist":0,"size":"---"}"#).unwrap();
        let g4 = verdict_from_delta_info("1790834873", &none);
        assert!(!g4.is_available());
        assert!(
            g4.reason().unwrap().contains("stat_delta exist=0"),
            "{g4:?}"
        );
    }

    // ------------------------- M6 同步文件夹列表
    // 输入含真机响应原文（xtask/probe/probe-out/webui/chain/09_qbox_get_syncing_folder_list.http，
    // 用户 test1）：`{"total": 0, …, "folder" :[]}`。

    #[test]
    fn syncing_folders_empty_real_response() {
        // ★ 真机原文。注意 `"folder" :[]` 冒号前有空格 —— serde_json 本身就容错。
        let raw = r#"{ "total": 0, "client_key": "754879e7da2632f22eef398e885d0467de50d9629cc1529b6c1dbe29cccb6999", "folder" :[]}"#;
        let v: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert!(
            parse_syncing_folders(&v).is_empty(),
            "空 folder 是「该账号没配对」的正常状态，不是错误"
        );
    }

    #[test]
    fn syncing_folders_parses_full_record_leniently() {
        let raw = r#"{"total": 1, "client_key": "ck", "folder": [{
            "folder": "/share/Photos", "permission": 3, "read_deletable": "1",
            "realpath": "/share/CACHEDEV1_DATA/Photos", "volume_id": "ce_cachedev1",
            "volume_lock": 0, "volume_encrypt": 0, "capacity": 0, "capacity_unit": "GB",
            "used_size": 0, "used_unit": "GB", "free_size": 0, "pid": 0 }]}"#;
        let v: serde_json::Value = serde_json::from_str(raw).unwrap();
        let f = parse_syncing_folders(&v);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].folder, "/share/Photos");
        assert_eq!(f[0].permission, 3);
        assert!(f[0].read_deletable, "字符串 \"1\" 也必须算真");
        assert_eq!(
            f[0].realpath.as_deref(),
            Some("/share/CACHEDEV1_DATA/Photos")
        );
        assert_eq!(f[0].volume_id.as_deref(), Some("ce_cachedev1"));
    }

    #[test]
    fn syncing_folders_missing_null_and_mismatch_are_not_errors() {
        // folder 缺失
        let v: serde_json::Value =
            serde_json::from_str(r#"{"total": 0, "client_key": "ck"}"#).unwrap();
        assert!(parse_syncing_folders(&v).is_empty());
        // folder 是 null
        let v: serde_json::Value = serde_json::from_str(r#"{"total": 0, "folder": null}"#).unwrap();
        assert!(parse_syncing_folders(&v).is_empty());
        // folder 类型不对（对象而非数组）
        let v: serde_json::Value = serde_json::from_str(r#"{"total": 0, "folder": {}}"#).unwrap();
        assert!(parse_syncing_folders(&v).is_empty());
        // total 与数组长度不一致 → 不 panic，以数组为准
        let v: serde_json::Value = serde_json::from_str(
            r#"{"total": 99, "folder": [{"folder": "/a"}, {"folder": "/b"}, {"folder": "/c"}]}"#,
        )
        .unwrap();
        assert_eq!(parse_syncing_folders(&v).len(), 3);
        let v: serde_json::Value = serde_json::from_str(r#"{"total": 7, "folder": []}"#).unwrap();
        assert!(parse_syncing_folders(&v).is_empty());
    }

    #[test]
    fn syncing_folders_lenient_types_and_empty_strings() {
        let raw = r#"{"folder": [
            {"folder": "/a", "permission": "2", "read_deletable": 0, "realpath": "", "vol_id": "v9"},
            {"folder": "/b", "permission": "abc", "read_deletable": true, "realpath": null, "volume_id": ""},
            "垃圾条目"]}"#;
        let v: serde_json::Value = serde_json::from_str(raw).unwrap();
        let f = parse_syncing_folders(&v);
        assert_eq!(f.len(), 2, "数组里的非对象条目应被跳过");
        assert_eq!(f[0].permission, 2, "字符串 \"2\" 也要能解析");
        assert!(!f[0].read_deletable);
        assert_eq!(f[0].realpath, None, "空串 realpath → None");
        assert_eq!(
            f[0].volume_id.as_deref(),
            Some("v9"),
            "vol_id 是 volume_id 的别名"
        );
        assert_eq!(f[1].permission, 0, "解析不了的 permission → 0");
        assert!(f[1].read_deletable);
        assert_eq!(f[1].volume_id, None, "空串 volume_id → None");
    }
}
