//! M6 真机集成测试：`func=qbox_get_syncing_folder_list`（NAS 上报的 Qsync 同步文件夹）。
//!
//! **默认 `#[ignore]`**（需要一台真实 NAS 与账号）。跑法：
//!
//! ```bash
//! export QSYNC_TEST_HOST=qnap.example.com
//! export QSYNC_TEST_PORT=9834
//! export QSYNC_TEST_USER=test1
//! export QSYNC_TEST_PASSWORD='...'
//! cargo test -p qxync-proto-test --test syncing_folders -- --ignored --nocapture
//! ```
//!
//! 没有设置环境变量时直接返回（视为跳过），不会失败。
//!
//! 安全性：
//! * **只读**——只登录 + 调 `syncing_folders()`，不 list/上传/删除任何东西；
//! * 断言只有「调用成功」：普通账号实测回 `{"total":0,…,"folder":[]}`，
//!   空列表是**正常状态**，不断言非空（该 NAS 的 test1 就是空的）。

use qxync_client::Client;
use qxync_core::{LinkConfig, HOME_ROOT};

fn env_creds() -> Option<(LinkConfig, String)> {
    let host = std::env::var("QSYNC_TEST_HOST").ok()?;
    let user = std::env::var("QSYNC_TEST_USER").ok()?;
    let password = std::env::var("QSYNC_TEST_PASSWORD").ok()?;
    let port = std::env::var("QSYNC_TEST_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(9834);
    Some((
        LinkConfig {
            id: "proto-test".into(),
            host,
            port,
            https: true,
            insecure: true,
            user,
            home_root: HOME_ROOT.to_string(),
            // 本测试只看 NAS 上报的同步文件夹，不配置任何额外根（空 = 只用家目录）。
            roots: Vec::new(),
            ipv4_only: std::env::var("QSYNC_TEST_IPV4").is_ok(),
        },
        password,
    ))
}

async fn logged_in() -> Option<Client> {
    let (link, pw) = env_creds()?;
    let mut c = Client::new(&link).expect("构造 client");
    c.login(&link.user, &pw).await.expect("登录失败");
    Some(c)
}

/// 登录 → `syncing_folders()` → 打印 total 与每一项（folder/permission/realpath）。
///
/// ★ 只断言「调用成功」。真机实测（test1）响应是
/// `{"total": 0, "client_key": "754879e7…", "folder" :[]}` —— 空就是预期结果。
#[tokio::test]
#[ignore = "需要真机 NAS"]
async fn syncing_folder_list_is_readable() {
    let Some(mut client) = logged_in().await else {
        return;
    };

    let folders = client
        .syncing_folders()
        .await
        .expect("qbox_get_syncing_folder_list 必须调用成功（空列表也是成功）");

    // 解析后的数组长度（= 服务端 `total` 的可用值）
    println!("syncing_folders: 共 {} 项", folders.len());
    for (i, f) in folders.iter().enumerate() {
        println!(
            "  [{i}] folder={} permission={} read_deletable={} realpath={:?} volume_id={:?}",
            f.folder, f.permission, f.read_deletable, f.realpath, f.volume_id
        );
    }
    if folders.is_empty() {
        println!("（该账号没有登记任何 Qsync 同步文件夹 —— 实测如此，不是错误）");
    }

    // 撤销登录会话（只读操作，失败不影响结论）
    let _ = client.logout().await;
}
