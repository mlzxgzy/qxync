//! ★ M17：两步验证（2SV / TOTP）真机协议测试。
//!
//! **默认全部 `#[ignore]`**，因为需要一台真实 NAS + 一个**已开启两步验证**的账号。
//!
//! ```bash
//! export QXNYC_TEST_HOST=nas.example.com
//! export QXNYC_TEST_PORT=9834
//! export QXNYC_TEST_USER=test2
//! export QXNYC_TEST_PASSWORD='...'
//! export QXNYC_TEST_TOTP_SECRET='BASE32SECRET'   # 认证器里的密钥
//! cargo test -p qxync-proto-test --test two_step -- --ignored --nocapture
//! ```
//!
//! 没有设置环境变量时测试直接返回（视为跳过），不会失败。
//! 想让 `daemon_ipc` 那组老用例也能登 2SV 账号，同样设 `QXNYC_TEST_TOTP_SECRET`。

use qxync_client::Client;
use qxync_core::{LinkConfig, Otp};

fn env_link() -> Option<(LinkConfig, String, String)> {
    let host = std::env::var("QXNYC_TEST_HOST").ok()?;
    let user = std::env::var("QXNYC_TEST_USER").ok()?;
    let password = std::env::var("QXNYC_TEST_PASSWORD").ok()?;
    let secret = std::env::var("QXNYC_TEST_TOTP_SECRET").ok()?;
    let port = std::env::var("QXNYC_TEST_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(9834);
    Some((
        LinkConfig {
            id: "2sv-test".into(),
            host,
            port,
            https: true,
            insecure: true,
            user,
            ipv4_only: std::env::var("QXNYC_TEST_IPV4").is_ok(),
            exclude: Vec::new(),
            filter_temp: true,
            peer_listen: None,
            peer_name: None,
        },
        password,
        secret,
    ))
}

/// 真机：用 TOTP 密钥登录 → 会话可用；再登一次（重新现算验证码）仍然成功。
///
/// 这条用例同时证明「daemon 重登不会拿到 30 秒前的旧码」：两次登录之间的验证码
/// 由 `login_with_otp` 各自现算。
#[tokio::test]
#[ignore = "需要真机 NAS + 已开启两步验证的账号"]
async fn m17_totp_login_roundtrip() {
    let Some((link, password, secret)) = env_link() else {
        return;
    };

    let mut c = Client::new(&link).expect("构造 client");
    let s1 = c
        .login_with_otp(&link.user, &password, Some(&secret))
        .await
        .expect("TOTP 登录失败");
    assert!(!s1.sid.is_empty());
    assert!(s1.two_step, "开了 2SV 的账号登录后 two_step 必须为 true");
    assert_eq!(s1.username, link.user);
    assert!(c.check_alive().await.unwrap_or(false), "会话必须可用");

    // 第二次：全新 client，验证码在登录时重新生成（不是复用上一次的）
    let mut c2 = Client::new(&link).expect("构造 client");
    let s2 = c2
        .login_with_otp(&link.user, &password, Some(&secret))
        .await
        .expect("第二次 TOTP 登录失败");
    assert!(s2.two_step);
    assert!(c2.check_alive().await.unwrap_or(false));
    println!("TOTP 登录成功两次：sid={} / {}", s1.sid.len(), s2.sid.len());
}

/// 真机：不提供第二因素时必须**明确报 2SV**，而不是只回一个 `authPassed=0`。
#[tokio::test]
#[ignore = "需要真机 NAS + 已开启两步验证的账号"]
async fn m17_totp_missing_second_factor_is_reported() {
    let Some((link, password, _secret)) = env_link() else {
        return;
    };
    let mut c = Client::new(&link).expect("构造 client");
    match c.login(&link.user, &password).await {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("两步验证") && msg.contains("2SV"),
                "错误信息必须告诉用户这是两步验证，实际：{msg}"
            );
            println!("预期失败：{msg}");
        }
        Ok(_) => {
            // 账号其实没开 2SV（或已关闭）：这条用例不适用，跳过而不是误报失败。
            println!("账号未开启两步验证，跳过");
        }
    }
}

/// 离线：密钥格式错误必须在**发请求之前**就被拒绝（不依赖网络）。
#[tokio::test]
async fn m17_bad_totp_secret_fails_before_network() {
    let link = LinkConfig {
        id: "offline".into(),
        // 一个必然连不上的本地端口；如果实现先把请求发出去，这里会是 Transport 错误
        host: "127.0.0.1".into(),
        port: 1,
        https: false,
        insecure: true,
        user: "u".into(),
        ipv4_only: true,
        exclude: Vec::new(),
        filter_temp: true,
        peer_listen: None,
        peer_name: None,
    };
    let mut c = Client::new(&link).expect("构造 client");
    let err = c
        .login_with_otp("u", "p", Some("not a base32 secret!"))
        .await
        .expect_err("非法密钥必须报错");
    assert!(
        matches!(err, qxync_core::Error::Auth(_)),
        "必须是鉴权/参数错误（本地校验），实际：{err:?}"
    );
    // 6 位验证码形式在本地不会当场失败（要交给 NAS 判定），这里只校验归一化结果
    assert_eq!(
        qxync_core::totp::normalize_otp("123456").unwrap(),
        Otp::Code("123456".into())
    );
}
