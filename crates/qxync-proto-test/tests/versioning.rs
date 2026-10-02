//! M5 真机集成测试：`versioning_*`（增量 delta）能力探测 + [`qxync_client::Client::delta_gate`]。
//!
//! **默认 `#[ignore]`**（需要一台真实 NAS 与账号）。跑法：
//!
//! ```bash
//! export QXNYC_TEST_HOST=qnap.example.com
//! export QXNYC_TEST_PORT=9834
//! export QXNYC_TEST_USER=test1
//! export QXNYC_TEST_PASSWORD='...'
//! export QXNYC_TEST_FIXTURE=/home/qxync-test
//! cargo test -p qxync-proto-test --test versioning -- --ignored --nocapture
//! ```
//!
//! 安全性（故意收窄，避免污染真机）：
//! * **只**调 `versioning_probe` / `versioning_lock` / `versioning_unlock` / `versioning_stat_delta`
//!   （`delta_gate` 内部也只调这四条）；
//! * **不**调 `versioning_upload_file` / `versioning_commit_upload` / `versioning_gen_delta` /
//!   `versioning_clean_version`，**不改** fixture 内容；
//! * 自己拿的锁一定 `unlock`（尽力释放，失败只打印）；
//! * 断言只保证「调用成功且能解析」，**不**断言「一定不可用」——将来 NAS 开了版本化会变。

use qxync_client::{Client, DeltaGate};
use qxync_core::LinkConfig;

fn env_creds() -> Option<(LinkConfig, String)> {
    let host = std::env::var("QXNYC_TEST_HOST").ok()?;
    let user = std::env::var("QXNYC_TEST_USER").ok()?;
    let password = std::env::var("QXNYC_TEST_PASSWORD").ok()?;
    let port = std::env::var("QXNYC_TEST_PORT")
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
            ipv4_only: std::env::var("QXNYC_TEST_IPV4").is_ok(),
            exclude: Vec::new(),
            filter_temp: true,
            peer_listen: None,
            peer_name: None,
        },
        password,
    ))
}

fn fixture_root() -> String {
    std::env::var("QXNYC_TEST_FIXTURE").unwrap_or_else(|_| "/home/qxync-test".to_string())
}

async fn logged_in() -> Option<Client> {
    let (link, pw) = env_creds()?;
    let mut c = Client::new(&link).expect("构造 client");
    c.login(&link.user, &pw).await.expect("登录失败");
    Some(c)
}

/// 探测 → 对一个真实文件 lock → stat_delta → unlock → `delta_gate` 判定。
///
/// 这台 NAS（QTS 5.2.9 / Qsync QPKG 20260723）的实测结论是 `versioning_support` 全 0、
/// `stat_delta` 恒 `exist:0`，所以 gate 预期 `Unavailable` —— 但测试**只打印**，不断言不可用。
#[tokio::test]
#[ignore = "需要真机 NAS"]
async fn versioning_probe_lock_stat_delta_and_gate() {
    let Some(client) = logged_in().await else {
        return;
    };
    let root = fixture_root();

    // 目标文件：fixture 里真实存在的第一个文件（不写、只读元数据）
    let entries = client.list(&root).await.expect("get_list fixture");
    let before = entries
        .iter()
        .find(|e| e.exist && !e.isfolder && !e.filename.is_empty())
        .cloned()
        .expect("fixture 目录里至少应有一个文件");
    let name = before.filename.clone();
    println!(
        "fixture={root} 目标文件={name} size={} epochmt={} versioning_support={}",
        before.filesize, before.epochmt, before.versioning_support
    );

    // 1) versioning_probe（能力位）
    let probe = client
        .versioning_probe()
        .await
        .expect("versioning_probe 必须能解析");
    println!(
        "probe: versioning_version={:?} versioning_enable={} qbox_versioning_enable={} \
         qbox_user_versioning_enable={} enabled={} raw={}",
        probe.versioning_version,
        probe.versioning_enable,
        probe.qbox_versioning_enable,
        probe.qbox_user_versioning_enable,
        probe.enabled(),
        probe.raw
    );
    if let Some(r) = probe.disabled_reason() {
        println!("probe: 服务端版本化未开放 → {r}");
    }

    // 2) 新建锁（create_version=1）；失败也算结论，不 panic
    let locked = client.versioning_lock(&root, &name, true, None, None).await;
    match &locked {
        Ok(l) => println!(
            "lock: status={} lockid={} version_id={}",
            l.status, l.lockid, l.version_id
        ),
        Err(e) => println!("lock: 失败（可容忍，状态码本身就是结论）: {e}"),
    }

    // 3) stat_delta（不带 lockid，见报告 §6.4）
    match &locked {
        Ok(l) => match client
            .versioning_stat_delta(&root, &name, &l.version_id)
            .await
        {
            Ok(d) => println!(
                "stat_delta: version_id={} exist={} size={:?} raw={}",
                l.version_id, d.exist, d.size, d.raw
            ),
            Err(e) => println!("stat_delta: 失败（可容忍）: {e}"),
        },
        Err(_) => println!("stat_delta: 跳过（没有 version_id）"),
    }

    // 4) ★ 收尾：先把自己拿的锁释放掉（再走 delta_gate，避免同时持两把锁）
    if let Ok(l) = &locked {
        match client.versioning_unlock(&root, &name, &l.lockid).await {
            Ok(true) => println!("unlock: ok (lockid={})", l.lockid),
            Ok(false) => println!("unlock: 服务端回未成功（lockid={}）", l.lockid),
            Err(e) => println!("unlock: 失败（lockid={}）: {e}", l.lockid),
        }
    }

    // 5) 能力门：内部会自己 lock/stat_delta/unlock
    match client.delta_gate(&root, &name).await {
        DeltaGate::Available {
            version_id,
            delta_size,
        } => println!("gate: Available version_id={version_id} delta_size={delta_size:?}"),
        DeltaGate::Unavailable { reason } => println!("gate: Unavailable reason={reason}"),
    }

    // 6) fixture 文件没被动过（只比大小；mtime 只打印）
    let after = client
        .stat(&root, &name)
        .await
        .expect("stat")
        .expect("文件仍在");
    println!(
        "fixture 复核: size {} -> {} / epochmt {} -> {}",
        before.filesize, after.filesize, before.epochmt, after.epochmt
    );
    assert_eq!(
        after.filesize, before.filesize,
        "本测试不应改动 fixture 文件内容"
    );
}
