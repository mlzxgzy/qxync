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
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// `get_list` 的默认单页容量（对应服务端 `Max_File_List`）。
///
/// 只是**单页**大小，不是目录容量上限 —— `list` 会按 `start` 一直翻到取全量。
const LIST_LIMIT: usize = 200;

/// ★ M15/T2：流式上传的读取块大小。
///
/// 只影响**内存占用**（O(块大小)）与读写 syscall 次数，不影响协议 ——
/// multipart body 会被 reqwest 拼成同一个流。8 MB 是「syscall 少」与
/// 「别把内存吃太多」之间的折中。
const UPLOAD_CHUNK: usize = 8 * 1024 * 1024;

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

// ---------------------------------------------------------------- M30 错误文案与重试

/// 幂等 GET 的**最多尝试次数**（首次 + 2 次重试）。
///
/// 为什么要有重试：真机（2026-10-21）轮询每 30s 一轮，仍会偶发一条
/// `get_list` 失败 —— 探活同时是通的，说明只是这一条请求被对端丢了。
/// 最常见的成因是 **keep-alive 竞态**：NAS 的 nginx 回收空闲连接，
/// 而 reqwest 连接池里恰好复用了那条刚被关掉的连接。GET 是幂等的，
/// 重试一次就往往成功，用户不该为这种抖动看到一条「同步失败」。
///
/// 为什么只重试「对端提前关连接 / 协议中断」这一小类：连接超时、连接拒绝、
/// 请求超时是**离线**的第一现场 —— 把它们也重试，会把一次 20s 的 connect
/// timeout 拖成 60s，让断网检测反而变慢。
const GET_MAX_ATTEMPTS: u32 = 3;

/// 首次重试前的等待（之后每次 ×3：200ms、600ms）。
const GET_RETRY_BACKOFF: Duration = Duration::from_millis(200);

/// 把 `reqwest::Error` 压成一句**不含 URL / 远端路径**的人话。
///
/// 为什么必须压：`reqwest::Error` 的 `Display` 会带上完整请求 URL，例如
/// `error sending request for url (https://nas:9834/cgi-bin/qsync/qsyncsrv.cgi?func=get_list&sid=…&path=%2Fhome%2F…&…)`。
/// 那条 URL 同时泄露 **sid、远端路径和全部查询参数**；用户看到的却不只是
/// 「网络错误」，还有一长串本地/远程路径 —— 这正是 M30 要消掉的东西。
/// 这里只保留「哪一类失败 + 最底层原因」，路径由调用方自己的 context 决定要不要带。
fn http_error_text(e: reqwest::Error) -> String {
    http_error_text_ref(&e)
}

/// [`http_error_text`] 的借用版本（重试日志要在返回错误前先描述一次）。
fn http_error_text_ref(e: &reqwest::Error) -> String {
    let kind = if e.is_timeout() {
        "请求超时"
    } else if e.is_connect() {
        "连接失败"
    } else if e.is_body() || e.is_decode() {
        "响应读取失败"
    } else if e.is_request() {
        "请求发送失败"
    } else {
        "网络失败"
    };
    match io_cause(e) {
        // 底层 io 原因（如 `Connection reset by peer (os error 104)`）能一眼看出
        // 是「对端把连接关了」还是「本机发不出去」，且自身不含路径。
        Some(cause) => format!("{kind}：{cause}"),
        None => kind.to_string(),
    }
}

/// 从错误链里找最底层的 `io::Error` 文本（不含路径）。
fn io_cause(e: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut src = e.source();
    for _ in 0..8 {
        let s = src?;
        if let Some(io) = s.downcast_ref::<std::io::Error>() {
            let text = io.to_string();
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
        src = s.source();
    }
    None
}

/// 这次 GET 失败值不值得**立刻重试**（幂等，且不是「离线」的判据）。
fn is_transient_send_error(e: &reqwest::Error) -> bool {
    // 超时 / 连接失败（含 DNS、拒绝）是离线的第一现场：只试一次，别拖慢离线检测。
    if e.is_timeout() || e.is_connect() {
        return false;
    }
    e.is_request() || e.is_body() || e.is_decode()
}

/// 反代 / 服务端忙时值得再试一次的状态码（网关抖动常见，且 GET 幂等）。
fn is_transient_http_status(code: u16) -> bool {
    matches!(code, 502 | 503 | 504)
}

/// 登录后拿到的会话信息。
#[derive(Debug, Clone)]
pub struct Session {
    pub sid: String,
    pub username: String,
    pub uid: Option<String>,
    pub user_cuid: Option<String>,
    pub nas: NasUid,
    /// ★ M17：本次登录是否通过了两步验证（2SV）。
    ///
    /// 只是给 UI/日志展示用：`false` = 账号没开 2SV，或这次登录没用到；
    /// 不影响会话本身。
    pub two_step: bool,
}

impl Session {
    pub fn busy_reason(&self) -> Option<&'static str> {
        self.nas.busy_reason()
    }
}

pub struct Client {
    http: reqwest::Client,
    link: LinkConfig,
    /// ★ M10：sid 放在 `Arc<RwLock<..>>` 里，**可热更新**。
    ///
    /// 挂载点持有的是同一个 [`Client`]（`Arc`）；daemon 重登之后把新 sid 直接塞回来，
    /// 挂载点就不用重挂。以前 sid 是普通字段，挂载时拷贝一份、过期后无人更新 ——
    /// 整个挂载点会一直 `EIO` 到重新挂载为止。
    sid: Arc<RwLock<Option<String>>>,
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
            .map_err(|e| Error::Transport(http_error_text(e)))?;
        Ok(Self {
            http,
            link: link.clone(),
            sid: Arc::new(RwLock::new(None)),
        })
    }

    pub fn link(&self) -> &LinkConfig {
        &self.link
    }

    /// 当前 sid（**克隆**出去，因为它是热更新的）。
    pub fn sid(&self) -> Option<String> {
        self.sid.read().unwrap().clone()
    }

    /// 用已有 sid 续接会话（例如从 daemon 传来的）。`&self`：可以在 `Arc<Client>` 上热更新。
    pub fn set_sid(&self, sid: impl Into<String>) {
        *self.sid.write().unwrap() = Some(sid.into());
    }

    /// 丢掉当前 sid（登出）。
    pub fn clear_sid(&self) {
        *self.sid.write().unwrap() = None;
    }

    fn require_sid(&self) -> Result<String> {
        self.sid
            .read()
            .unwrap()
            .clone()
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
    ///
    /// 账号开了两步验证（2SV）时会明确报错并提示改用 [`Client::login_with_otp`]。
    pub async fn login(&mut self, user: &str, password: &str) -> Result<Session> {
        self.login_with_otp(user, password, None).await
    }

    /// ★ M17：阶段 3 —— `authLogin.cgi`，支持两步验证（2SV / TOTP）。
    ///
    /// `otp` 接受三种输入（见 [`qxync_core::totp::normalize_otp`]）：
    /// * 6 位一次性验证码；
    /// * Base32 密钥（`GHH5...`，大小写/空格/连字符都容忍）；
    /// * `otpauth://totp/...?secret=...` URI。
    ///
    /// 流程与 QTS Web UI（`qos-core-login.js`）一致：
    ///
    /// 1. 先发**不带** `security_code` 的登录请求；
    /// 2. 若响应 `authPassed=1` → 账号没开 2SV，正常返回；
    /// 3. 若响应 `need_2_step_verification=1` / `need_2sv=1`，再发一次带
    ///    `security_code=<6 位验证码>` 的请求。
    ///
    /// 之所以不把 `security_code` 塞进第一次请求：万一 NAS 在未开 2SV 时对未知
    /// 字段的处理将来变了，两步流程对两种账号都成立，而 Web UI 走的就是这条路径。
    /// 传密钥时**每次调用现算**验证码 —— daemon 定时重登也走这里，绝不会拿到
    /// 30 秒前的旧码。
    pub async fn login_with_otp(
        &mut self,
        user: &str,
        password: &str,
        otp: Option<&str>,
    ) -> Result<Session> {
        // 先把输入归一化：密钥格式不对就不必白跑一趟 NAS。
        let otp = match otp {
            Some(raw) => Some(qxync_core::totp::normalize_otp(raw)?),
            None => None,
        };
        let first = self.auth_login(user, password, None).await?;
        let passed = xml_text(&first, "authPassed").unwrap_or("0");
        let need_2sv = xml_text(&first, "need_2_step_verification")
            .or_else(|| xml_text(&first, "need_2sv"))
            .unwrap_or("0");

        let (text, two_step) = if passed == "1" {
            (first, false)
        } else if need_2sv == "1" {
            let code = match &otp {
                Some(qxync_core::totp::Otp::Code(code)) => code.clone(),
                Some(qxync_core::totp::Otp::Secret(secret)) => qxync_core::totp::code_now(secret)?,
                None => {
                    return Err(Error::Auth(format!(
                        "该账号已开启两步验证（2SV），但这次没有提供验证码或 TOTP 密钥\
                         （need_2sv=1 / authPassed=0）：用 `--otp-secret <Base32密钥>` \
                         保存密钥，或 `--otp-code <6位验证码>` 只用于本次登录。\
                         NAS 响应：{}",
                        self.auth_failure_summary(&first)
                    )))
                }
            };
            let second = self.auth_login(user, password, Some(&code)).await?;
            let passed = xml_text(&second, "authPassed").unwrap_or("0");
            if passed != "1" {
                let still = xml_text(&second, "need_2_step_verification")
                    .or_else(|| xml_text(&second, "need_2sv"))
                    .unwrap_or("0");
                let hint = if still == "1" {
                    "验证码被 NAS 拒绝：检查 TOTP 密钥是否正确、验证码是否已过期\
                     （30 秒一换）、本机与 NAS 的时钟是否一致"
                } else {
                    "两步验证后 NAS 仍未放行"
                };
                return Err(Error::Auth(format!(
                    "两步验证失败：{hint}。NAS 响应：{}",
                    self.auth_failure_summary(&second)
                )));
            }
            (second, true)
        } else {
            let err = xml_text(&first, "errorValue").unwrap_or("?");
            return Err(Error::Auth(format!(
                "authPassed=0 errorValue={err} need_2sv={need_2sv}\
                 （检查用户名/口令；口令编码必须是 base64）"
            )));
        };

        let sid = xml_text(&text, "authSid")
            .ok_or_else(|| Error::Auth("authPassed=1 但响应里没有 authSid".into()))?
            .to_string();
        self.set_sid(sid.clone());

        let username = xml_text(&text, "username").unwrap_or(user).to_string();

        // 阶段 4：NAS UID（替代已 404 的 qsyncsrvPrepare.cgi）
        let nas = self.nas_uid().await.unwrap_or_else(|_| parse_uid_stub());
        Ok(Session {
            sid,
            username,
            uid: nas.uid.clone(),
            user_cuid: nas.user_cuid.clone(),
            nas,
            two_step,
        })
    }

    /// 发一次 `authLogin.cgi`，返回响应 XML 原文。
    ///
    /// `security_code` 只在两步验证的第二次请求里出现；其余字段与 0.6.1 一致
    /// （真机验证过：`serviceKey=1` + base64 口令是必需的）。
    async fn auth_login(
        &self,
        user: &str,
        password: &str,
        security_code: Option<&str>,
    ) -> Result<String> {
        use base64::Engine as _;
        let pwd = base64::engine::general_purpose::STANDARD.encode(password.as_bytes());
        let mut body: Vec<(&str, String)> = vec![
            ("user", user.to_string()),
            ("serviceKey", "1".to_string()),
            ("client_app", "Qsync".to_string()),
            (
                "client_agent",
                concat!("qxync/", env!("CARGO_PKG_VERSION")).to_string(),
            ),
            ("gen_client_id", "1".to_string()),
            ("remme", "1".to_string()),
            ("dont_verify_2sv_again", "0".to_string()),
            ("pwd", pwd),
        ];
        if let Some(code) = security_code {
            body.push(("security_code", code.to_string()));
        }
        let url = self.url("cgi-bin/authLogin.cgi", &[]);
        let resp = self
            .http
            .post(&url)
            .header("X-Forwarded-For", "127.0.0.1")
            .form(&body)
            .send()
            .await
            .map_err(|e| Error::Transport(format!("authLogin.cgi: {}", http_error_text(e))))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Transport(http_error_text(e)))?;
        if !status.is_success() {
            return Err(Error::Auth(format!("authLogin.cgi HTTP {status}")));
        }
        Ok(text)
    }

    /// 把 authLogin 失败响应的关键字段拼成一句可读摘要（**不含口令**）。
    fn auth_failure_summary(&self, text: &str) -> String {
        let field = |name: &str| xml_text(text, name).unwrap_or("-");
        format!(
            "authPassed={} errorValue={} need_2sv={}",
            field("authPassed"),
            field("errorValue"),
            field("need_2sv")
        )
    }

    /// 登出（失败不视为错误）。
    pub async fn logout(&mut self) -> Result<()> {
        // 先把 sid 摘掉（不持锁跨 await），再通知服务端
        let taken = self.sid.write().unwrap().take();
        if let Some(sid) = taken {
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
                // ★ 服务端统一错误出口带 `msg`（status=8 时是
                // "Qsync Central is initializing..."）—— 原样透传，别再丢掉。
                return Err(match m.msg.clone() {
                    Some(msg) => Error::status_msg(s, "qbox_get_max_log", msg),
                    None => Error::Status {
                        status: st,
                        context: "qbox_get_max_log".into(),
                        msg: None,
                    },
                });
            }
        }
        Ok(m)
    }

    /// 会话保活：`qbox_get_max_log` 能用就说明 sid 还有效。
    ///
    /// 三种结果由 [`alive_from_max_log`] 分类，**不能**把「服务端未就绪」判成会话失效：
    /// status=8（`Qsync Central is initializing…`）是 NAS 还在启动/迁移，
    /// 重登没有任何用（见 [`qxync_core::error::Error::is_server_busy`] 与
    /// `status.rs` 的说明）；以前这里对**所有** `Error::Status` 都回 `Ok(false)`，
    /// daemon 的会话保活会据此每轮重登一次，日志里还会写「sid 已失效」（诊断是错的）。
    pub async fn check_alive(&self) -> Result<bool> {
        alive_from_max_log(self.max_log().await)
    }

    /// ★ M20：**不依赖会话**的网络探活 —— 对 `base_url` 发一个 GET，任何 HTTP 回应
    /// 都算「网络是通的」（4xx/5xx 也是服务端在回话）。
    ///
    /// 为什么单列一个方法：离线恢复检测不能先登录 —— 断网时登录本身要等
    /// connect timeout（20s），每 3 秒一轮的探活会被它拖成 20+ 秒一轮，恢复延迟
    /// 反而比旧版还差。先探网络，通了再登录/flush。
    pub async fn probe_network(&self, timeout: std::time::Duration) -> bool {
        matches!(
            self.http
                .get(self.link.base_url())
                .timeout(timeout)
                .send()
                .await,
            Ok(_)
        )
    }

    /// ★ M20：**离线时的短探活** —— 用给定的短超时打一次 `qbox_get_max_log`。
    ///
    /// 为什么需要它：客户端全局 `connect_timeout` 是 20 秒，而离线恢复检测如果
    /// 沿用普通请求，网络断着的时候每次都要白等 20 秒；恢复后也要等满一个轮询间隔
    /// （默认 30s，真机有人设 600s）才会发现。轮询器在离线分支用 5 秒超时探活，
    /// 恢复延迟就压到了几秒。
    ///
    /// 返回 `Ok(true)` = 连上了且拿到合法响应；`Ok(false)` = 连上了但服务端明确
    /// 不 OK（例如 sid 失效的 4/5）—— 那说明**网络通了**，应该走正常轮询去重登，
    /// 不该继续当离线；`Err` = 网络层失败（仍是离线）。
    pub async fn probe_reachable(&self, timeout: std::time::Duration) -> Result<bool> {
        let sid = self.require_sid()?.to_string();
        let url = self.url(
            "cgi-bin/qsync/qsyncsrv.cgi",
            &[("func", "qbox_get_max_log"), ("sid", sid.as_str())],
        );
        let resp = self
            .http
            .get(&url)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| Error::Transport(format!("探活: {}", http_error_text(e))))?;
        if !resp.status().is_success() {
            // HTTP 层能回包 = 网络是通的（哪怕 500 也是服务端在回话）
            return Ok(true);
        }
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(format!("探活读响应: {}", http_error_text(e))))?;
        let m = parse_max_log(&body)?;
        Ok(m.status
            .is_none_or(|s| ServerStatus(s).is_success() || s == 8))
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

    /// 探针用：按**绝对 URL** 发一次 GET，返回 `(HTTP 状态码, 响应体)`。
    ///
    /// 用途是试那些"不确定该打哪个 CGI 入口"的接口（如 `get_meta`）——
    /// 同一个 func 在不同入口下行为可能完全不同（实测 `get_meta` 在
    /// `qsyncsrv.cgi` 上恒 500，需要换入口试）。
    ///
    /// 只做传输层的事：不注入 sid、不解析、不判 status。
    /// 正常业务代码请用上面那些有语义的封装。
    pub async fn raw_get(&self, url: &str) -> Result<(u16, Vec<u8>)> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| Error::Transport(format!("GET 失败: {}", http_error_text(e))))?;
        let code = resp.status().as_u16();
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(http_error_text(e)))?;
        Ok((code, body.to_vec()))
    }

    /// 把 query 拼到 `base_url()` 下某个 CGI 路径上（**不加** sid，调用方自己拼）。
    pub fn cgi_url(&self, cgi: &str, query: &[(&str, &str)]) -> String {
        self.url(cgi, query)
    }

    /// 发一次**幂等** GET：对端提前关连接 / 反代 502 这类抖动自动重试
    /// （见 [`GET_MAX_ATTEMPTS`] 与 [`is_transient_send_error`]）。
    ///
    /// 传 `what` 而不是完整 URL：错误文案里只留「哪一跳失败」，
    /// URL 里的 sid 与远端路径不外泄（见 [`http_error_text`]）。
    async fn send_get(&self, what: &str, url: &str) -> Result<reqwest::Response> {
        let mut backoff = GET_RETRY_BACKOFF;
        for attempt in 1..=GET_MAX_ATTEMPTS {
            let last = attempt == GET_MAX_ATTEMPTS;
            match self.http.get(url).send().await {
                Ok(resp) if !last && is_transient_http_status(resp.status().as_u16()) => {
                    tracing::debug!(
                        "{what}: HTTP {}（反代/服务端忙），{backoff:?} 后重试（第 {attempt}/{GET_MAX_ATTEMPTS} 次尝试）",
                        resp.status()
                    );
                }
                Ok(resp) => return Ok(resp),
                Err(e) if !last && is_transient_send_error(&e) => {
                    tracing::debug!(
                        "{what}: {}，{backoff:?} 后重试（第 {attempt}/{GET_MAX_ATTEMPTS} 次尝试）",
                        http_error_text_ref(&e)
                    );
                }
                Err(e) => {
                    return Err(Error::Transport(format!("{what}: {}", http_error_text(e))));
                }
            }
            tokio::time::sleep(backoff).await;
            backoff *= 3;
        }
        unreachable!("send_get 的每个分支都会 return")
    }

    /// 调 `qsyncsrv.cgi?func=…`，按给定键值原样发出去。
    async fn qsync_func(&self, func: &str, extra: &[(&str, &str)]) -> Result<Vec<u8>> {
        let sid = self.require_sid()?.to_string();
        let mut q: Vec<(&str, &str)> = vec![("func", func), ("sid", sid.as_str())];
        q.extend_from_slice(extra);
        let url = self.url("cgi-bin/qsync/qsyncsrv.cgi", &q);
        let resp = self.send_get(func, &url).await?;
        let code = resp.status();
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(http_error_text(e)))?;
        if !code.is_success() {
            return Err(Error::Transport(format!("{func}: HTTP {code}")));
        }
        Ok(body.to_vec())
    }

    /// ★ 探针用：按**原始键值**打一次 `qsyncsrv.cgi`，不做任何解析或 status 判定。
    ///
    /// 用途是实测那些签名未知的私有接口（如 `get_meta` / `get_meta_profile`）——
    /// 参数名要靠实机试出来，先把原文拿回来再决定怎么封装。
    /// 正常业务代码请用上面那些有语义的封装，别直接用这个。
    pub async fn qsync_probe(&self, query: &[(&str, &str)]) -> Result<String> {
        let sid = self.require_sid()?.to_string();
        let mut q: Vec<(&str, &str)> = vec![("sid", sid.as_str())];
        q.extend_from_slice(query);
        let url = self.url("cgi-bin/qsync/qsyncsrv.cgi", &q);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Transport(format!("probe: {}", http_error_text(e))))?;
        let code = resp.status();
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(http_error_text(e)))?;
        if !code.is_success() {
            return Err(Error::Transport(format!("probe: HTTP {code}")));
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    /// `func=get_list`，自动翻页（`start += len(datas)` 直到 `len < limit` 或已取满 `total`）。
    ///
    /// ★ M15/T1 真机实测（2026-10-04，QTS 5.2.9 / Qsync QPKG build 20260723）：
    /// 服务端**确实支持 `start`/`limit` 分页**，且 `limit` 不被 `Max_File_List=200`
    /// 夹住（实测 `limit=5000` 生效，单页拿回 501 项）。`total` / `real_total` 是
    /// **目录总项数**（`limit=1` 时 `total` 仍是 3，不是本页的 1），所以下面
    /// `out.len() >= total` 的退出条件是按「目录总数」写的，实测 501 项目录
    /// 翻 3 页拼回 501 条、不重不漏。
    ///
    /// 也就是说：调用方拿到的 `entries` 已经是**全量**，下游**不要**再按 200 截断
    /// （`qxync-fuse` 的 `filter_visible` 曾经这么干，导致 >200 项目录 `ls` 读不全）。
    pub async fn list(&self, path: &str) -> Result<Vec<DirEntry>> {
        self.list_limit(path, LIST_LIMIT).await
    }

    /// 同 [`Client::list`]，但用**自定义单页容量** —— 仍然会翻页取全量。
    ///
    /// 服务端实测不把 `limit` 夹在 `Max_File_List=200`（`limit=5000` 生效，单页
    /// 拿回 501 项），所以拿一个超大 `limit` 就能**一次**取全，作为分页结果的
    /// 对照基线（见 `qxync-proto-test` 的 `list_paginates_past_200`）。
    pub async fn list_limit(&self, path: &str, limit: usize) -> Result<Vec<DirEntry>> {
        let mut out: Vec<DirEntry> = Vec::new();
        let mut start = 0usize;
        loop {
            let listing = self.list_page(path, limit, start).await?;
            let got = listing.datas.len();
            let total = listing.total;
            out.extend(listing.datas);
            // ★ 只有 `total` **明确给了正数**才用它提前收尾：`total` 缺省是 0，
            //   拿 0 当「总数 0」会在第一页就 break，把 200+ 项的目录静默截断。
            //   没有 total 时靠分页本身收敛（got < limit / 空页）。
            if got == 0 || got < limit || (total > 0 && out.len() as i64 >= total) {
                break;
            }
            start += got;
            if start > 1_000_000 {
                return Err(Error::Parse("get_list 翻页异常".to_string()));
            }
        }
        Ok(out)
    }

    /// 单页 `get_list`（不翻页）。`list` / `list_limit` 的公共一跳。
    async fn list_page(&self, path: &str, limit: usize, start: usize) -> Result<Listing> {
        let sid = self.require_sid()?.to_string();
        let start_s = start.to_string();
        let limit_s = limit.to_string();
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
        let resp = self.send_get("get_list", &url).await?;
        // ★ HTTP 层失败**不能**当「目录是空的」：反代 502/504 的 JSON 错误体
        //   （如 `{"error":...}`）能通过 `Listing` 的全默认反序列化 + `ensure_ok`，
        //   于是被读成「0 项」→ 对账把该目录所有条目当成「远端已删」。
        if !resp.status().is_success() {
            return Err(Error::Transport(format!(
                "get_list: HTTP {}",
                resp.status()
            )));
        }
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(http_error_text(e)))?;
        Self::ensure_listing_shape(&body, "get_list")?;
        let listing: Listing = parse_listing(&body)?;
        listing.ensure_ok("get_list")?;
        Ok(listing)
    }

    /// 校验响应体至少「长得像」`get_list` / `stat` 的 JSON。
    ///
    /// 为什么需要：HTTP 200 + `{"error":…}`（某些反代 / WAF）能通过 `Listing` 的
    /// 全默认反序列化，被读成「0 项 / 不存在」，对账会据此删本地。合法响应至少带
    /// `status` / `datas` / `total` / `real_total` / `exist` 之一。
    fn ensure_listing_shape(body: &[u8], context: &str) -> Result<()> {
        let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| {
            Error::Parse(format!(
                "{context} 响应不是合法 JSON: {e}；原文前 200B: {}",
                String::from_utf8_lossy(&body[..body.len().min(200)])
            ))
        })?;
        let shape_ok = ["status", "datas", "total", "real_total", "exist"]
            .iter()
            .any(|k| v.get(*k).is_some());
        if !shape_ok {
            return Err(Error::Parse(format!(
                "{context} 响应不像列表/stat JSON（缺 status/datas/total/real_total/exist）: 原文前 200B: {}",
                String::from_utf8_lossy(&body[..body.len().min(200)])
            )));
        }
        Ok(())
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
        let resp = self.send_get("stat", &url).await?;
        // ★ stat 的 `Ok(None)` 会被对账当成「确认不存在」并据此删本地 ——
        //   HTTP 层失败绝不能长成「不存在」。
        if !resp.status().is_success() {
            return Err(Error::Transport(format!("stat: HTTP {}", resp.status())));
        }
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(http_error_text(e)))?;
        Self::ensure_listing_shape(&body, "stat")?;
        let listing: Listing = parse_listing(&body)?;
        listing.ensure_ok("stat")?;
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
            .map_err(|e| Error::Transport(format!("createdir: {}", http_error_text(e))))?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(http_error_text(e)))?;
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
    ///
    /// 两个边界都在发请求前/后显式挡住：
    /// * `start > end` 是**非法区间**（`Range: bytes=100-50` 会被服务端忽略，
    ///   回**整个文件**，调用方按「区间字节」用就会把整文件写进缓存）；
    /// * 200 + 服务端 JSON 错误信封要当错误（见 [`download_json_error`]）。
    pub async fn download_range(
        &self,
        dir: &str,
        name: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>> {
        checked_range(start, end)?;
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
            .map_err(|e| Error::Transport(format!("download {name}: {}", http_error_text(e))))?;
        let code = resp.status().as_u16();
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(http_error_text(e)))?;
        if code != 200 && code != 206 {
            return Err(Error::Transport(format!(
                "download {dir}/{name}: HTTP {code}, body={}",
                String::from_utf8_lossy(&body[..body.len().min(160)])
            )));
        }
        // 服务端忽略 Range 时会回 200 + **整个文件**。长度不等于请求长度就说明拿到的
        // 不是那一段 —— 绝不能当区间字节返回（FUSE 会把整文件写进区间缓存）。
        if range_was_ignored(code, start, end, body.len()) {
            return Err(Error::Transport(format!(
                "download {dir}/{name}: 服务端忽略了 Range（请求 bytes={start}-{end}，\
                 实际回 {} 字节，看起来是整文件）",
                body.len()
            )));
        }
        // 服务端出错时会用 200 + JSON 错误体，这里按内容再兜一层。
        if let Some(e) = download_json_error(&body, &format!("download {dir}/{name}")) {
            return Err(e);
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
        self.download_to_file_with_progress(dir, name, dest, None)
            .await
    }

    /// [`Client::download_to_file`] 的进度版：每收到一个网络分片就回调
    /// `(已写入字节, 总字节)`。`total` 取自 `Content-Length` 响应头，
    /// 服务端走 chunked 编码（无该头）时为 `None` —— 展示侧要能处理「未知总量」。
    ///
    /// 回调在下载循环里**同步**调用，必须廉价（采样/渲染级别，不做 IO）。
    pub async fn download_to_file_with_progress(
        &self,
        dir: &str,
        name: &str,
        dest: &std::path::Path,
        progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
    ) -> Result<u64> {
        let sid = self.require_sid()?.to_string();
        let url = self.url(
            "cgi-bin/filemanager/utilRequest.cgi",
            &self.filetool_query("download", dir, name, &sid),
        );
        let resp =
            self.http.get(&url).send().await.map_err(|e| {
                Error::Transport(format!("download {name}: {}", http_error_text(e)))
            })?;
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
        let total = resp.content_length();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| Error::Transport(http_error_text(e)))?;
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
            written += chunk.len() as u64;
            if let Some(cb) = progress {
                cb(written, total);
            }
        }
        tokio::io::AsyncWriteExt::flush(&mut file).await?;
        file.sync_all().await?;
        drop(file);
        // 出错残留的 JSON 错误体会先落到临时文件里；调用方比对长度后再 rename。
        tokio::fs::rename(&tmp, dest).await?;
        Ok(written)
    }

    /// 上传一个文件：`POST /cgi-bin/qsync/upload.php`，multipart 字段名必须是 `files[]`。
    ///
    /// ⚠️ **整个文件已经在内存里**。大文件（4 GB）会把进程内存吃满。
    /// 能走文件路径就用 [`Client::upload_file`]（流式，内存 O(块大小)）。
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
            .map_err(|e| Error::Transport(format!("upload {filename}: {}", http_error_text(e))))?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(http_error_text(e)))?;
        parse_upload_result(&body, filename)
    }

    /// ★ M15/T2：**流式**上传一个本地文件 —— 内存占用 O(块大小)，与文件大小无关。
    ///
    /// 为什么需要：`upload_bytes` 要调用方先把整个文件 `fs::read` 进内存，
    /// 传 4 GB 文件就是 4 GB 内存，并发几个直接 OOM。这里改成按块读、
    /// 读一块发一块（`unfold` 泵 + `Body::wrap_stream`）。
    ///
    /// ⚠️ **服务端不接受 chunked 编码**（真机实测，2026-10-04）：
    /// 不带 `Content-Length` 的流式 body 会被 `upload.php` 拒成
    /// `HTTP 411 Length Required`（回一个 HTML 错误页，不是 JSON）。
    /// 所以**必须**用 `stream_with_length` 把长度告诉服务端 ——
    /// 这正是 reqwest `Part::file` 内部做的事（`file.metadata()` 取 len）。
    /// 代价是上传期间文件被截短/改名会让长度对不上（服务端要么拒绝、
    /// 要么按错误长度收），这在「本地文件正被自己改」的场景下是既有风险，
    /// 不是本次引入的。
    ///
    /// 与 `upload_bytes` 的**唯一**区别是数据来源；multipart 字段名、URL、
    /// 返回值解析完全一致（真机实测服务端不关心 body 是怎么来的）。
    ///
    /// 返回**真正发出的字节数**。注意这不是「文件大小」：上传期间用户可能还在改
    /// 同一个文件，流式读到的是**当时那部分内容**。调用方要用它推进 baseline
    /// （见 `UploadQueue::upload_one` 的 success hook），所以必须精确。
    ///
    /// 注意：**不具备断点续传**。传到 90% 断网仍是从 0 重来
    /// （重试语义由 `UploadQueue` 的 `max_attempts` 兜着）。
    ///
    /// ★ M15/T4 已验证（2026-10-04）：**服务端确实实现了分片上传**
    /// （`start_chunked_upload` / `get_chunked_upload_status` /
    /// `delete_chunked_upload_file` + 5 个 `upload.c` 内部符号，
    /// `op_chunked_upload` 读 `offset`/`filesize`/`upload_id`），
    /// 但**那条通道只服务 Qbox 空间**（`upload_root_dir` 需 `/remote:` 前缀 +
    /// `name@uid`），普通家目录路径过不去（handler status 12/46）。
    /// 本函数用的 `dest_path` 是普通绝对路径 —— **两套路径空间不通，够不到**。
    /// 探针与完整证据见 `xtask/probe/upload_chunk_probe.py` 与 M15 §2 T4。
    ///
    /// 所以「大文件断网从 0 重来」在当前架构下**没有协议层的解**，
    /// 只能靠退避调参 + 进度上报（`UploadQueue` 侧，属 T8 范围）。
    pub async fn upload_file(&self, dest_path: &str, local: &Path, filename: &str) -> Result<u64> {
        // ★ M38：**本地**读不到内容源是「本地 IO 失败」，不是网络错误。
        //
        // 旧实现把 stat/open 的失败也包成 `Error::Transport` → `is_offline()` 为真
        // → 上传队列按「断网」处理：作业永远留在队尾、永远不放弃，每轮重试还把
        // 连通性标成离线。真机 2026-10-26 就卡在这个坑里：一个临时文件的缓存被清理后
        // 队列里剩了个指不到内容的作业，界面一直显示「网络断开」，而 NAS 明明通着
        // （登录、对账都正常），用户点「立即同步」也永远看不到恢复。
        //
        // 长度必须**发之前**拿到：reqwest 据此决定发 Content-Length 还是 chunked。
        let len = tokio::fs::metadata(local)
            .await
            .map_err(|e| Error::Io(format!("upload {filename}: stat {}: {e}", local.display())))?
            .len();
        let file = tokio::fs::File::open(local)
            .await
            .map_err(|e| Error::Io(format!("upload {filename}: 打开 {}: {e}", local.display())))?;
        self.upload_stream_sized(dest_path, filename, file, len)
            .await
    }

    /// 同 [`Client::upload_file`]，但数据源是任意 [`AsyncRead`]，长度由调用方给。
    ///
    /// `expected_len` 必须是**实际会发出的字节数**：服务端不接受 chunked
    /// （411Length Required），长度不对会导致上传失败或落地大小错。
    /// 用 `filename` 作为 multipart 里的文件名（**不是**源文件的末段）。
    pub async fn upload_stream_sized<R>(
        &self,
        dest_path: &str,
        filename: &str,
        reader: R,
        expected_len: u64,
    ) -> Result<u64>
    where
        R: tokio::io::AsyncRead + Send + Unpin + 'static,
    {
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
        // 通用 `R: AsyncRead` → `Stream<Item = Result<Vec<u8>, io::Error>>` 的适配。
        // `Body::wrap_stream` 要的是 `TryStream`（`Bytes: From<Item>`，`Vec<u8>` 满足），
        // 而 `tokio::fs::File` 只实现 `AsyncRead` —— tokio 的 `File` **不**实现
        // `Stream`，所以用 [`read_chunk_stream`] 手动泵：每次 poll 读一块
        // （`UPLOAD_CHUNK`），读满即产出一块，内存占用因此是 O(块大小) 而非 O(文件大小)。
        //
        // `sent` 累加**真正读走**的字节数：上传期间用户可能还在改同一个文件，
        // 调用方（`UploadQueue`）要用这个数把 baseline 推到准确签名，
        // 所以不能用 `expected_len` 代替。
        let sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let stream = read_chunk_stream(reader, sent.clone());
        // ★ 必须是 `stream_with_length`：QNAP 的 upload.php 对 chunked（无
        // Content-Length）直接回 `411 Length Required`。见 upload_file 的注释。
        let part = reqwest::multipart::Part::stream_with_length(
            reqwest::Body::wrap_stream(stream),
            expected_len,
        )
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
            .map_err(|e| Error::Transport(format!("upload {filename}: {}", http_error_text(e))))?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(http_error_text(e)))?;
        parse_upload_result(&body, filename)?;
        Ok(sent.load(Ordering::Relaxed))
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
        let resp = self.send_get("settime", &url).await?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(http_error_text(e)))?;
        parse_listing(&body)?.ensure_ok(format!("stat&settime {dir}/{name}"))?;
        Ok(())
    }
}

// ------------------------------------------------- 会话保活判定

/// 把 `qbox_get_max_log` 的探测结果分成三类（[`Client::check_alive`] 的判定核心）：
///
/// * `Ok(true)`  —— 探测成功，sid 有效；
/// * `Ok(false)` —— **会话确定失效**（[`Error::is_auth`]：登录失败 / status 4 / 5），该重登；
/// * `Err(e)`    —— 探测本身失败（断网 / 解析失败 / 服务端未就绪），调用方应稍后再试，
///   **不要**重登。
///
/// status=8（`Qsync Central is initializing…`）属于第三类：那是 NAS 正在安装/迁移/
/// 还原，`authLogin.cgi` 可能照样能过，但 Qsync 侧一直回未就绪 —— 重登只会每轮白跑
/// 一次，还会在日志里写出「sid 已失效」这种**错误诊断**。码值与 `msg` 文本两种
/// 判据都认（见 `qxync_core::error::Error::is_server_busy`，它按 msg 判；这里补一个
/// 按 `ServerStatus` 码值判的兜底，防止服务端某次没带 msg）。
fn alive_from_max_log(probe: Result<MaxLog>) -> Result<bool> {
    match probe {
        Ok(_) => Ok(true),
        Err(e) if e.is_auth() => Ok(false),
        Err(e) if e.is_server_busy() => Err(e),
        Err(Error::Status { status, .. }) if status.is_server_busy() => Err(Error::status(
            status.0,
            "qbox_get_max_log（服务端未就绪：Qsync Central is initializing）",
        )),
        // 其它业务 status：无法确认存活，但也不是「确定失效」→ 不触发重登。
        Err(Error::Status { .. }) => Ok(false),
        Err(e) => Err(e),
    }
}

// ------------------------------------------------- 上传流泵 / 下载正文的错误体判定

/// 把任意 [`tokio::io::AsyncRead`] 泵成 `Stream<Item = io::Result<Vec<u8>>>`，
/// 同时累计**真正读走**的字节数（`sent`）。
///
/// 这是 [`Client::upload_stream_sized`] 的数据来源：内存 O(块大小)，与文件大小无关。
///
/// ★ 读错误必须**传出去**，不能当作 EOF：早期实现是 `reader.read(...).await.ok()?`
/// —— 一次本地 IO 错误（磁盘掉线 / 读 EIO）被 `ok()` 吞成「流正常结束」，
/// 于是 reqwest 按已声明的 `Content-Length` 发了一个**短 body**，调用方最后只看到
/// 一个泛泛的网络/服务端错误（根因丢失）；若服务端宽松接受短 body，更会静默落盘
/// 一个截断文件。这里用 `Some(Err(e))` 让 `Body::wrap_stream` 把它当传输错误抛出，
/// 并在产出错误后终止流（下一次 poll 返回 `None`）。
fn read_chunk_stream<R>(
    reader: R,
    sent: Arc<std::sync::atomic::AtomicU64>,
) -> impl futures_util::Stream<Item = std::io::Result<Vec<u8>>>
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
{
    futures_util::stream::unfold(
        (Some(reader), vec![0u8; UPLOAD_CHUNK], sent),
        |(mut reader, mut buf, sent)| async move {
            use tokio::io::AsyncReadExt as _;
            // `None` = 上一块读失败后已终止（reqwest 通常不会再 poll，这里兜底）。
            let mut r = reader.take()?;
            match r.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    sent.fetch_add(n as u64, Ordering::Relaxed);
                    Some((Ok(buf[..n].to_vec()), (Some(r), buf, sent)))
                }
                Err(e) => Some((Err(e), (None, buf, sent))),
            }
        },
    )
}

// ------------------------------------------------- 下载正文的错误体判定 / 区间校验

/// 校验 [`Client::download_range`] 的闭区间。
///
/// `start > end` 必须报错而不是把 `Range: bytes=100-50` 发出去：那是非法 Range，
/// 服务端会**忽略 Range 直接回整个文件**（200），调用方拿到的就不再是请求的那一段
/// —— FUSE 的区间水合会把它当区间内容写进缓存（铁则 1：绝不短读/错位）。
fn checked_range(start: u64, end: u64) -> Result<()> {
    if start > end {
        return Err(Error::Parse(format!(
            "download_range 非法区间: start={start} > end={end}"
        )));
    }
    Ok(())
}

/// 服务端是否**忽略了 Range**（对「从中间取」的请求回了 200 + 整个文件）。
///
/// 判据刻意宽松：`start == 0` 时 200 与 206 本来就难分（整个文件可能就是这段），
/// 长度恰好等于请求长度的 200 也放过（有些实现会直接回切片 + 200）；
/// 只有「200 + 从中间取 + 长度不等于请求长度」才是确定的整文件 —— 这种绝不能
/// 当区间字节返回。
fn range_was_ignored(code: u16, start: u64, end: u64, body_len: usize) -> bool {
    code == 200 && start > 0 && body_len as u64 != end - start + 1
}

/// 「HTTP 200 但 body 其实是服务端 JSON 错误信封」的判定（命中 → 返回可读错误）。
///
/// 真机记录（`xtask/probe/probe-out/webui/chain4/dl_missing.http`）：
///
/// ```text
/// 200 ... Content-Type: application/json
/// { "version": "", "build": "20260723", "status": 20, "success": "true" }
/// ```
///
/// ★ 早期实现写的是 `body.windows(9).any(|w| w == b"\"status\"")` ——
/// `b"\"status\""` 只有 **8** 字节，任何 9 字节窗口都不可能等于它，条件**恒为 false**，
/// 于是这道保险从来没生效过：服务端错误体被当成文件内容静默返回。
///
/// 但也不能简单改成 `windows(8)`：用户同步的 **JSON 文件**同样可能以 `{` 开头、
/// 带一个非 0/1 的 `status` 字段，那样会把真文件误判成服务端错误（读它必然 EIO）。
/// 所以除了 `status` 非成功，还要求它**像服务端信封** —— `version` / `build` /
/// `success` / `msg` 里至少出现两个，且没有列表响应才有的 `datas` 键。
fn download_json_error(body: &[u8], context: &str) -> Option<Error> {
    if !body.starts_with(b"{") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let obj = v.as_object()?;
    // 列表响应（正常 get_list）不是「单个文件的错误体」。
    if obj.contains_key("datas") {
        return None;
    }
    let status = match v.get("status")? {
        serde_json::Value::Number(n) => n.as_i64()?,
        serde_json::Value::String(s) => s.parse().ok()?,
        _ => return None,
    };
    if ServerStatus(status).is_success() {
        return None;
    }
    let envelope_markers = ["version", "build", "success", "msg"]
        .iter()
        .filter(|k| obj.contains_key(**k))
        .count();
    if envelope_markers < 2 {
        return None;
    }
    Some(match v.get("msg").and_then(|m| m.as_str()) {
        Some(msg) if !msg.trim().is_empty() => Error::status_msg(status, context, msg),
        _ => Error::status(status, context),
    })
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
            .map_err(|e| Error::Transport(format!("rename: {}", http_error_text(e))))?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(http_error_text(e)))?;
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
            .map_err(|e| Error::Transport(format!("move: {}", http_error_text(e))))?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(http_error_text(e)))?;
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
        self.delete_entries(dir, &[name]).await
    }

    /// ★ M11：同一目录下的**批量删除**。
    ///
    /// `file_total` 本来就是「本批条目数」——`stat`/`set_mtime` 把它硬编码成 `1`
    /// 只是因为那些调用每次只处理一个文件。删 N 个文件时把 N 个 `file_name`
    /// 连同 `file_total=N` 一次发出去，N 次串行 RTT 塌缩成 1 次。
    ///
    /// 限制：**只能同目录**。`path` 只有一个，跨目录必须分组。
    pub async fn delete_entries(&self, dir: &str, names: &[&str]) -> Result<()> {
        if names.is_empty() {
            return Ok(());
        }
        let sid = self.require_sid()?.to_string();
        let url = self.url(
            "cgi-bin/qsync/qsyncsrv.cgi",
            &[("func", "delete"), ("sid", sid.as_str())],
        );
        // `file_total` 必须等于本批实际条目数，否则服务端只处理前 N 个。
        let mut form: Vec<(&str, String)> = vec![
            ("path", dir.to_string()),
            ("file_total", names.len().to_string()),
        ];
        for n in names {
            form.push(("file_name", n.to_string()));
        }
        let resp = self
            .http
            .post(&url)
            .form(&form)
            .send()
            .await
            .map_err(|e| Error::Transport(format!("delete: {}", http_error_text(e))))?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(http_error_text(e)))?;
        // 部分成功也要能看出是哪几个：ensure_ok 只报首个失败项。
        let listing = parse_listing(&body)?;
        listing.ensure_ok(format!("delete {dir}/{}", names.join(",")))?;
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
                msg: None,
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
// 真机实测（用户 test1）响应原文（`client_key` 已脱敏）：
// `{"total": 0, "client_key": "<redacted>", "folder" :[]}` —— **端点可用，但该账号没有登记
// 任何 Qsync 同步文件夹**。所以「空列表」是正常状态，绝不报错。

impl Client {
    /// ★ M6：NAS 上报的 Qsync 同步文件夹（= 用户/设备在 Qsync 里配了同步的共享文件夹）。
    ///
    /// 普通账号没配对时返回空 `Vec`（实测如此），**不报错**；只有网络/HTTP、非 JSON、
    /// 或带上非成功 `status` 时才回 `Err`。复用 core 的
    /// [`qxync_core::ipc::SyncingFolderInfo`]，不另造类型。
    pub async fn syncing_folders(&self) -> Result<Vec<qxync_core::ipc::SyncingFolderInfo>> {
        // ★ `detail=1` 与官方客户端一致（2026-10-02 HAR 抓到的就是这个请求）。
        let body = self
            .qsync_func("qbox_get_syncing_folder_list", &[("detail", "1")])
            .await?;
        let v: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| Error::Parse(format!("qbox_get_syncing_folder_list 非 JSON: {e}")))?;
        // 真机正常响应里没有 `status`；一旦出现且非成功就按协议错误处理。
        if let Some(s) = v.get("status").and_then(json_i64) {
            if !ServerStatus(s).is_success() {
                return Err(Error::status(s, "qbox_get_syncing_folder_list"));
            }
        }
        let mut items = parse_syncing_folders(&v);
        // ★ 把 NAS 共享路径换算成客户端路径（`/share/homes/test1/.Qsync` → `/home/.Qsync`）：
        //   选 NAS 文件夹时要用的是**客户端能 get_list 的路径**，不是共享路径。
        //   家目录在 Qsync 协议里固定叫 `/home`（[`qxync_core::HOME_ROOT`]），不是配置项。
        let home_root = qxync_core::HOME_ROOT;
        let user = self.link.user.clone();
        for it in items.iter_mut() {
            it.client_path = qxync_core::client_path_from_share(
                it.path.as_deref().unwrap_or(""),
                it.realpath.as_deref(),
                &home_root,
                &user,
            );
        }
        Ok(items)
    }
}

/// 解析 `qbox_get_syncing_folder_list` 的 JSON（纯函数，便于离线单测）。
///
/// ★ 真机形状（2026-10-02 HAR，`detail=1`，用户 test1）：
///
/// ```json
/// { "total": 1, "folder": [ { "name": "Qsync", "privilege": 2,
///     "path": "/share/homes/test1/.Qsync",
///     "realpath": "/share/CACHEDEV1_DATA/homes/test1/.Qsync", … } ] }
/// ```
///
/// 也就是说**外层**数组叫 `folder`，而每一项里的名字字段叫 `name`、权限叫 `privilege`、
/// 路径叫 `path` —— 早期实现按 `folder` / `permission` 去读项内字段，遇到真有内容的
/// 响应会解析出「空名字 + 0 权限」，正好把「能列举」判成了「列举不出来」。
/// 现在两种拼法都认（真机字段优先）。
///
/// 其他容错要点（真机实测 + 报告 §4.2 的宽松字段表）：
/// * 外层 `folder` 可能**缺失 / 是 `null` / 类型不对** → 一律当**空列表**，
///   「没配对」是正常状态，不报错；`"folder" :[]`（冒号前空格）由 `serde_json` 自行容错；
/// * `privilege` / `permission` / `read_deletable` 可能是数字或字符串 → 宽松解析
///   （`"3"` / `"1"` 都认）；
/// * `path` / `realpath` / `volume_id`（别名 `vol_id`）的**空串当作 `None`**；
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
            let i = |k: &str| -> Option<i64> { obj.get(k).and_then(json_i64) };
            Some(qxync_core::ipc::SyncingFolderInfo {
                // 真机 `name`（展示名，如 `Qsync`）；老拼法 `folder` 兼容
                folder: s("name").or_else(|| s("folder")).unwrap_or_default(),
                path: s("path"),
                // `client_path` 由 `syncing_folders()` 按 link 的 home_root/user 填
                client_path: None,
                // 真机 `privilege`；老拼法 `permission` 兼容
                permission: i("privilege").or_else(|| i("permission")).unwrap_or(0),
                read_deletable: obj.get("read_deletable").map(json_bool).unwrap_or(false),
                realpath: s("realpath"),
                // 报告 §4.2 里同一字段有 `volume_id` / `vol_id` 两种拼法；
                // ★ 真机是**数字** `1`（卷 id），所以要再兜一层数字→字符串
                volume_id: s("volume_id").or_else(|| s("vol_id")).or_else(|| {
                    i("volume_id")
                        .or_else(|| i("vol_id"))
                        .map(|n| n.to_string())
                }),
            })
        })
        .collect()
}

/// 上传响应的判定：`{"status":"1","files":[{"status":"1",...}]}`。
///
/// 响应不是 JSON 时把**原文**带进错误里（截断到 200B）—— 服务端对被拒的
/// 上传会回 HTML 错误页或空 body，只报「非 JSON」等于什么线索都没有。
pub fn parse_upload_result(body: &[u8], filename: &str) -> Result<()> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| {
        Error::Parse(format!(
            "upload.php 非 JSON: {e}；HTTP 响应前 200B: {}",
            String::from_utf8_lossy(&body[..body.len().min(200)])
        ))
    })?;
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
            msg: None,
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

    // ------------------------------------------------ 区间下载的边界 / 错误体判定

    /// ★ 回归：旧实现 `body.windows(9).any(|w| w == b"\"status\"")` 恒为 false
    /// （`"status"` 带引号只有 8 字节），所以「200 + 服务端 JSON 错误体」从来没被拦住。
    #[test]
    fn download_error_envelope_is_detected_but_real_json_files_are_not() {
        // 真机记录：200 + JSON 错误信封（xtask/probe/probe-out/webui/chain4/dl_missing.http）
        let real = br#"{ "version": "", "build": "20260723", "status": 20, "success": "true" }"#;
        let e = download_json_error(real, "download /home/nope.txt").expect("必须识别成错误");
        assert!(matches!(e, Error::Status { .. }), "{e:?}");
        assert!(e.to_string().contains("status=20"), "{e}");

        // 成功信封不是错误
        let ok = br#"{ "version": "", "build": "20260723", "status": 1, "success": "true" }"#;
        assert!(download_json_error(ok, "x").is_none());

        // 用户同步的 JSON 文件：有非 0/1 的 status，但不是服务端信封 → 不能误伤
        // （误判会让这个文件的每次读都 EIO）
        assert!(
            download_json_error(br#"{"status": 500, "message": "boom"}"#, "x").is_none(),
            "普通 JSON 文件不能被当成服务端错误"
        );
        assert!(
            download_json_error(br#"{"version": "1.0.0", "status": 4, "items": []}"#, "x")
                .is_none()
        );

        // 带 datas 的列表响应不是「单个文件的错误体」
        assert!(download_json_error(
            br#"{"status": 4, "version": "x", "build": "1", "datas": []}"#,
            "x"
        )
        .is_none());

        // 二进制 / 空 body / 非 JSON
        assert!(download_json_error(&[0u8, 1, 2, 3], "x").is_none());
        assert!(download_json_error(b"", "x").is_none());
        assert!(download_json_error(b"not json", "x").is_none());

        // 旧判据的窗口长度（`"status"` = 8 字节）—— 写死在这里防再次写错
        assert_eq!(b"\"status\"".len(), 8);
    }

    #[test]
    fn download_range_rejects_inverted_interval() {
        assert!(checked_range(0, 0).is_ok(), "1 字节区间必须合法");
        assert!(checked_range(0, 1023).is_ok());
        let e = checked_range(50, 10).expect_err("start > end 必须报错");
        assert!(e.to_string().contains("非法区间"), "{e}");
    }

    /// 服务端忽略 Range 的判定：只拦「200 + 从中间取 + 长度不对」这一种确定的整文件回包。
    #[test]
    fn download_range_detects_ignored_range() {
        // 206 = 服务端确实按 Range 回了那一段
        assert!(!range_was_ignored(206, 10, 19, 10));
        // 200 + start=0：整文件可能就是请求的那段，不拦
        assert!(!range_was_ignored(200, 0, 99, 100));
        // 200 + 长度恰好等于请求长度：宽松放过
        assert!(!range_was_ignored(200, 10, 19, 10));
        // 200 + 从中间取 + 回的长度不对 = 整文件，必须拦
        assert!(range_was_ignored(200, 10, 19, 100));
        assert!(range_was_ignored(200, 10, 19, 5000));
    }

    // ------------------------------------------------ 上传流泵：读错误不能被吞

    /// 读一块成功后立刻报 IO 错的数据源（模拟上传中途磁盘掉线 / EIO）。
    struct FailAfterFirstChunk {
        served: bool,
    }

    impl tokio::io::AsyncRead for FailAfterFirstChunk {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if !self.served {
                self.served = true;
                buf.put_slice(b"abc");
                std::task::Poll::Ready(Ok(()))
            } else {
                std::task::Poll::Ready(Err(std::io::Error::other("磁盘读失败")))
            }
        }
    }

    /// ★ 回归：旧实现 `reader.read(...).await.ok()?` 把读错误吞成 EOF，
    /// 上传会变成一次「短 body」，根因丢失。
    #[tokio::test]
    async fn upload_stream_propagates_read_error_instead_of_eof() {
        use futures_util::StreamExt as _;
        let sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let s = read_chunk_stream(FailAfterFirstChunk { served: false }, sent.clone());
        futures_util::pin_mut!(s);
        let first = s.next().await.expect("第一块必须有").expect("第一块应成功");
        assert_eq!(first.as_slice(), b"abc");
        let second = s.next().await.expect("第二个元素必须是错误");
        assert!(second.is_err(), "读错误不能变成 EOF：{second:?}");
        assert!(second.unwrap_err().to_string().contains("磁盘读失败"));
        assert!(s.next().await.is_none(), "出错后流必须终止（不能继续读）");
        assert_eq!(sent.load(Ordering::Relaxed), 3, "sent 只累计真正读走的字节");
    }

    #[tokio::test]
    async fn upload_stream_pumps_to_eof_and_counts_bytes() {
        use futures_util::StreamExt as _;
        let sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let s = read_chunk_stream(&b"hello world"[..], sent.clone());
        futures_util::pin_mut!(s);
        let mut all = Vec::new();
        while let Some(item) = s.next().await {
            all.extend(item.expect("读 &[u8] 不会失败"));
        }
        assert_eq!(all.as_slice(), b"hello world");
        assert_eq!(sent.load(Ordering::Relaxed), 11);
    }

    // ------------------------------------------------ 会话保活：未就绪 ≠ 会话失效

    #[test]
    fn alive_probe_does_not_treat_server_busy_as_dead_session() {
        // 探测成功 = 活着
        let ok = parse_max_log(b"{}").expect("空对象可解析");
        assert!(matches!(alive_from_max_log(Ok(ok)), Ok(true)));

        // 确定失效 → Ok(false)（调用方据此重登）
        assert!(matches!(
            alive_from_max_log(Err(Error::Auth("口令不对".into()))),
            Ok(false)
        ));
        assert!(matches!(
            alive_from_max_log(Err(Error::status(4, "qbox_get_max_log"))),
            Ok(false)
        ));

        // 服务端未就绪（status=8 + 真机 msg）→ 探测失败，**不是**会话失效
        let busy = Error::status_msg(
            8,
            "qbox_get_max_log",
            "Qsync Central is initializing. Please wait a few minutes and try again.",
        );
        assert!(
            matches!(alive_from_max_log(Err(busy)), Err(_)),
            "status=8 不能进重登路径（重登无用，日志还会写「sid 已失效」）"
        );
        // 码值兜底：即便服务端这次没带 msg，8 也按未就绪处理
        assert!(matches!(
            alive_from_max_log(Err(Error::status(8, "qbox_get_max_log"))),
            Err(_)
        ));

        // 网络/解析错误 → 探测失败
        assert!(matches!(
            alive_from_max_log(Err(Error::Transport("连接超时".into()))),
            Err(_)
        ));
        assert!(matches!(
            alive_from_max_log(Err(Error::Parse("坏 JSON".into()))),
            Err(_)
        ));
    }

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

    /// ★ M38 回归（真机 2026-10-27）：本地文件不存在必须报「本地 IO 失败」，
    /// 不能是 `Transport` —— 后者会被上传队列判成「网络断了」：作业永远不放弃，
    /// 每轮重试还把连通性标成离线（界面显示「网络断开」，而 NAS 一直是通的）。
    #[tokio::test]
    async fn upload_missing_local_is_io_not_transport() {
        let link = qxync_core::LinkConfig {
            id: "t".into(),
            host: "nas.invalid".into(),
            port: 9834,
            https: true,
            insecure: true,
            user: "test1".into(),
            ipv4_only: false,
            exclude: Vec::new(),
            filter_temp: true,
            peer_listen: None,
            peer_name: None,
        };
        let c = Client::new(&link).unwrap();
        let missing = std::env::temp_dir().join(format!(
            "qxync-client-m38-missing-{}.bin",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&missing);
        let e = c
            .upload_file("/home/qxync-test", &missing, "missing.bin")
            .await
            .expect_err("本地文件不存在必须报错");
        assert!(matches!(e, Error::Io(_)), "本地文件缺失必须是 Io: {e:?}");
        assert!(!e.is_offline(), "本地文件缺失不是网络错误: {e}");
        assert!(e.to_string().contains("本地 IO 失败"), "{e}");
    }

    #[test]
    fn debug_query_percent_encodes_space() {
        assert_eq!(
            debug_query(&[("source_file", "空 格.txt")]),
            "source_file=%E7%A9%BA%20%E6%A0%BC.txt"
        );
    }

    /// ★ M10：sid 必须**热更新** —— 挂载点与 daemon 拿到的是同一个 `Arc<Client>`，
    /// 一个地方 set_sid，另一个地方立刻看得到（否则重登后挂载点还在用旧 sid → 一直 EIO）。
    #[test]
    fn sid_hot_swap_is_visible_through_clones() {
        let link = qxync_core::LinkConfig {
            id: "t".into(),
            host: "nas.invalid".into(),
            port: 9834,
            https: true,
            insecure: true,
            user: "test1".into(),
            ipv4_only: false,
            exclude: Vec::new(),
            filter_temp: true,
            peer_listen: None,
            peer_name: None,
        };
        let c = Client::new(&link).unwrap();
        assert_eq!(c.sid(), None);
        c.set_sid("sid-1");
        assert_eq!(c.sid().as_deref(), Some("sid-1"));

        // 「挂载点」与「daemon」各持一份 handle，指向同一个 client
        let shared = Arc::new(c);
        let mount_side = shared.clone();
        let daemon_side = shared.clone();
        daemon_side.set_sid("sid-2");
        assert_eq!(
            mount_side.sid().as_deref(),
            Some("sid-2"),
            "热更新必须穿过 Arc 立刻可见"
        );
        daemon_side.clear_sid();
        assert_eq!(mount_side.sid(), None);
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
        // ★ 真机原文（`client_key` 已脱敏；省略号处是没抄进来的字段）。
        //   注意 `"folder" :[]` 冒号前有空格 —— serde_json 本身就容错。
        let raw = r#"{ "total": 0, "client_key": "<redacted>", "folder" :[]}"#;
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
        // 老拼法：项内 `folder` 当展示名
        assert_eq!(f[0].folder, "/share/Photos");
        assert_eq!(f[0].permission, 3);
        assert!(f[0].read_deletable, "字符串 \"1\" 也必须算真");
        assert_eq!(
            f[0].realpath.as_deref(),
            Some("/share/CACHEDEV1_DATA/Photos")
        );
        assert_eq!(f[0].volume_id.as_deref(), Some("ce_cachedev1"));
    }

    /// ★ 真机响应原文（2026-10-02 reqable HAR，`detail=1`）。
    ///
    /// **字段名与类型逐字照抄**（这正是回归点：真机是 `name`/`path`/`privilege`，不是
    /// `folder`/`permission`）；`client_key` 与卷容量这类跟这台 NAS 绑定的数值**已脱敏**，
    /// 换成中性值，类型不变。
    ///
    /// 这条测试是**回归闸门**：早期实现按项内 `folder`/`permission` 读，遇到这个响应
    /// 会得到「名字空 + 权限 0」，于是「NAS 明明登记了同步文件夹」在界面上显示成空。
    #[test]
    fn syncing_folders_real_machine_response_is_not_lost() {
        let raw = r#"{ "client_key": "<redacted>", "total": 1, "admingroup": 0, "folder": [ { "name": "Qsync", "fkey": "", "privilege": 2, "path": "/share/homes/test1/.Qsync", "realpath": "/share/CACHEDEV1_DATA/homes/test1/.Qsync", "read_deletable": 0, "iconCls": "folder", "enc": 0, "lock": 0, "volume_id": 1, "used_unit": "Byte", "used_size": "0", "capacity_unit": "GB", "capacity": 1, "free_size": 0, "volume_status": 0, "volume_encrypt": 0, "volume_lock": 0, "volume_name": "vol", "raid_level": 5, "raid_disk_cnt": 4, "pool_id": 1, "pool_name": "pool", "raid_count": 1, "raid_id1": 5, "pool_capacity": "1.00 TB", "pool_status": 0, "max_item_limit": 2000 } ] }"#;
        let v: serde_json::Value = serde_json::from_str(raw).unwrap();
        let f = parse_syncing_folders(&v);
        assert_eq!(f.len(), 1, "真机 total=1，必须解析出 1 条");
        assert_eq!(f[0].folder, "Qsync", "展示名取真机 `name`");
        assert_eq!(f[0].permission, 2, "权限取真机 `privilege`（数字）");
        assert_eq!(f[0].path.as_deref(), Some("/share/homes/test1/.Qsync"));
        assert_eq!(
            f[0].realpath.as_deref(),
            Some("/share/CACHEDEV1_DATA/homes/test1/.Qsync")
        );
        assert_eq!(
            f[0].volume_id.as_deref(),
            Some("1"),
            "数字型 volume_id 也认"
        );
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
