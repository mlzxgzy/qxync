//! 真机协议集成测试。**默认全部 `#[ignore]`**，因为需要一台真实 NAS 与账号。
//!
//! 跑法（不要把口令写进任何会提交的文件）：
//!
//! ```bash
//! export QSYNC_TEST_HOST=qnap.example.com
//! export QSYNC_TEST_PORT=9834
//! export QSYNC_TEST_USER=test1
//! export QSYNC_TEST_PASSWORD='...'
//! export QSYNC_TEST_FIXTURE=/home/qxync-test      # 由 report/probe/qs_fixture.py 造好
//! cargo test -p qxync-proto-test -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! 没有设置环境变量时测试直接返回（视为跳过），不会失败。

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
            roots: Vec::new(),
            ipv4_only: std::env::var("QSYNC_TEST_IPV4").is_ok(),
            exclude: Vec::new(),
            filter_temp: true,
            peer_listen: None,
            peer_name: None,
        },
        password,
    ))
}

fn fixture_root() -> String {
    std::env::var("QSYNC_TEST_FIXTURE").unwrap_or_else(|_| "/home/qxync-test".to_string())
}

async fn logged_in() -> Option<Client> {
    let (link, pw) = env_creds()?;
    let mut c = Client::new(&link).expect("构造 client");
    c.login(&link.user, &pw).await.expect("登录失败");
    Some(c)
}

/// 登录 + 能力探测：sid、NAS UID、max_log 都能拿到。
#[tokio::test]
#[ignore = "需要真机 NAS"]
async fn login_and_capabilities() {
    let Some(client) = logged_in().await else {
        return;
    };
    let nas = client.nas_uid().await.expect("qbox_get_nas_uid");
    println!(
        "Qsync {:?} QPKG {:?} UID {:?}",
        nas.qsync_version, nas.qpkg_version, nas.uid
    );
    assert!(nas.qsync_version.is_some(), "服务端应返回 Qsync_version");
    let max_log = client.max_log().await.expect("qbox_get_max_log");
    println!(
        "max_log={} global_notify={}",
        max_log.max_log, max_log.global_notify
    );
    assert!(client.check_alive().await.expect("check_alive"));
}

/// 列目录：家目录根可列，且测试数据目录存在。
#[tokio::test]
#[ignore = "需要真机 NAS"]
async fn list_home_and_fixture() {
    let Some(client) = logged_in().await else {
        return;
    };
    let home = client.list(HOME_ROOT).await.expect("get_list /home");
    println!(
        "/home -> {:?}",
        home.iter().map(|e| &e.filename).collect::<Vec<_>>()
    );
    assert!(!home.is_empty());

    let fix = client
        .list(&fixture_root())
        .await
        .expect("get_list fixture");
    let names: Vec<&str> = fix.iter().map(|e| e.filename.as_str()).collect();
    println!("fixture -> {names:?}");
    assert!(
        names.contains(&"hello.txt"),
        "测试数据里应有 hello.txt：{names:?}"
    );
}

/// stat 的契约：`目录 + 文件名`，不是全路径。
#[tokio::test]
#[ignore = "需要真机 NAS"]
async fn stat_contract() {
    let Some(client) = logged_in().await else {
        return;
    };
    let e = client
        .stat(&fixture_root(), "hello.txt")
        .await
        .expect("stat")
        .expect("hello.txt 应存在");
    assert_eq!(e.filename, "hello.txt");
    assert!(e.filesize > 0);
    assert!(e.epochmt > 0, "epochmt 是 FUSE getattr 的 mtime 来源");
    assert!(!e.isfolder);
}

/// 下载字节与 `Range`（M2 的 128 KiB 水合前提）。
#[tokio::test]
#[ignore = "需要真机 NAS"]
async fn download_full_and_range() {
    let Some(client) = logged_in().await else {
        return;
    };
    let root = fixture_root();
    let full = client
        .download_range(&root, "1k.bin", 0, 1023)
        .await
        .expect("download");
    assert_eq!(full.len(), 1024);
    // 内容是我们造的确定性序列：bytes(range(256)) * 4
    assert_eq!(full[0], 0);
    assert_eq!(full[255], 255);
    assert_eq!(full[256], 0);

    let head = client
        .download_range(&root, "1k.bin", 0, 99)
        .await
        .expect("range");
    assert_eq!(head.len(), 100);
    assert_eq!(head, full[..100]);
}

/// 需要写权限：建目录 → 上传 → stat 校验 → （不删除）。
#[tokio::test]
#[ignore = "需要真机 NAS，且会写入 NAS"]
async fn upload_roundtrip() {
    let Some(client) = logged_in().await else {
        return;
    };
    let dir = format!("{}/rust-proto", fixture_root());
    let _ = client.mkdir(&fixture_root(), "rust-proto").await; // 已存在时服务端回成功变体
    let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    client
        .upload_bytes(&dir, "rust-upload.bin", payload.clone())
        .await
        .expect("upload.php");
    let e = client
        .stat(&dir, "rust-upload.bin")
        .await
        .expect("stat")
        .expect("存在");
    assert_eq!(e.filesize, payload.len() as u64);
    let back = client
        .download_range(&dir, "rust-upload.bin", 0, payload.len() as u64 - 1)
        .await
        .expect("download 回读");
    assert_eq!(back, payload, "上传后回读必须逐字节一致");
    // 清理：别在 NAS 的 fixture 里留产物（M0 起这条测试一直漏了清理）
    let _ = client.delete_entry(&dir, "rust-upload.bin").await;
    let _ = client.delete_entry(&fixture_root(), "rust-proto").await;
}

/// M2b 写接口契约：rename/move/delete + `stat` 的 `exist` 语义。
///
/// 会写入真机（在 `<fixture>/rust-write-api/` 下），跑完自己清理。
#[tokio::test]
#[ignore = "需要真机 NAS，且会写入 NAS"]
async fn write_api_contract() {
    let Some(client) = logged_in().await else {
        return;
    };
    let root = fixture_root();
    let dir = format!("{root}/rust-write-api");
    let sub = format!("{dir}/sub");
    let _ = client.mkdir(&root, "rust-write-api").await; // 已存在时服务端回成功变体
    let _ = client.mkdir(&dir, "sub").await;

    client
        .upload_bytes(&dir, "a.txt", b"AAA\n".to_vec())
        .await
        .expect("upload");

    // 1) 不存在的路径必须回 None（不能靠「文件名非空」判存在）
    assert!(
        client
            .stat(&dir, "definitely-missing")
            .await
            .unwrap()
            .is_none(),
        "stat 必须用 exist 判存在，缺失路径应返回 None"
    );

    // 2) 同目录改名（含只改大小写）
    client.rename(&dir, "a.txt", "b.txt").await.expect("rename");
    assert!(client.stat(&dir, "b.txt").await.unwrap().is_some());
    assert!(client.stat(&dir, "a.txt").await.unwrap().is_none());
    client
        .rename(&dir, "b.txt", "B.txt")
        .await
        .expect("case rename");

    // 3) 跨目录移动：FileStation move 会忽略 dest_file（保持原名），
    //    所以实现是 move_into + rename 两步
    client.move_into(&dir, "B.txt", &sub).await.expect("move");
    assert!(client.stat(&sub, "B.txt").await.unwrap().is_some());
    assert!(client.stat(&dir, "B.txt").await.unwrap().is_none());
    client
        .rename(&sub, "B.txt", "c.txt")
        .await
        .expect("rename after move");
    let e = client.stat(&sub, "c.txt").await.unwrap().expect("c.txt");
    assert_eq!(e.filesize, 4);

    // 4) 内容回读 + 删除
    let back = client
        .download_range(&sub, "c.txt", 0, 3)
        .await
        .expect("download");
    assert_eq!(back, b"AAA\n");
    client
        .delete_entry(&sub, "c.txt")
        .await
        .expect("delete file");
    assert!(client.stat(&sub, "c.txt").await.unwrap().is_none());
    client.delete_entry(&dir, "sub").await.expect("delete dir");
    client
        .delete_entry(&root, "rust-write-api")
        .await
        .expect("cleanup dir");
    assert!(client
        .stat(&root, "rust-write-api")
        .await
        .unwrap()
        .is_none());
}

/// ★ M2c：变更发现三端点（`qbox_get_sync_log` / `qbox_query_notify` / `qbox_get_device_config_list`）。
///
/// 验收点（真机）：
/// 1. `qbox_get_sync_log&lower=0&number=N` 要么返回**按 log_id 递增**的事件，要么回 `status:-17`
///    （区间无事件）——两者都算通过，`-17` 不是协议错（旧谜团）。
/// 2. 事件字段：`log_id/action/isfolder/filepath/device/device_uid/exist/mtime/size` 都能解析。
/// 3. `lower` 是**闭区间下界**：用上一批最后一个 log_id 再拉一次，应能重新拿到它。
/// 4. `qbox_query_notify` / `qbox_get_device_config_list` 不 panic、错误可判定。
#[tokio::test]
#[ignore = "需要真机 NAS"]
async fn m2c_sync_log_and_notify_endpoints() {
    use qxync_core::sync::is_log_missing;

    let Some(client) = logged_in().await else {
        return;
    };
    let max = client.max_log().await.expect("qbox_get_max_log");
    println!(
        "max_log={} global_notify={}",
        max.max_log, max.global_notify
    );

    match client.sync_log(0, 20, None).await {
        Ok(b) => {
            println!(
                "sync_log: {} 条（end={} number={}）",
                b.events.len(),
                b.end,
                b.number
            );
            for e in b.events.iter().take(5) {
                println!(
                    "  log_id={} action={} isfolder={} exist={} size={} device={:?} path={}",
                    e.log_id, e.action, e.isfolder, e.exist, e.size, e.device, e.filepath
                );
            }
            for w in b.events.windows(2) {
                assert!(
                    w[0].log_id <= w[1].log_id,
                    "事件必须按 log_id 递增: {} > {}",
                    w[0].log_id,
                    w[1].log_id
                );
            }
            // `lower` 闭区间：拿最后一个 log_id 再拉，必须还能看到它
            if let Some(last) = b.events.last().map(|e| e.log_id) {
                match client.sync_log(last, 5, None).await {
                    Ok(b2) => assert!(
                        b2.events.iter().any(|e| e.log_id == last),
                        "lower={last} 应包含 log_id={last}（闭区间）"
                    ),
                    Err(e) if is_log_missing(&e) => {
                        println!("  lower={last} → -17（日志被滚动，符合预期）")
                    }
                    Err(e) => panic!("sync_log(lower={last}) 失败: {e}"),
                }
            }
        }
        Err(e) if is_log_missing(&e) => println!("sync_log lower=0 → status:-17（区间内没有事件）"),
        Err(e) => panic!("qbox_get_sync_log 失败: {e}"),
    }

    // 另外两个游标端点：能判定即可（M2c 不消费它们的事件）
    match client.query_notify(0, max.global_notify).await {
        Ok(b) => println!("query_notify: {} 项 device={:?}", b.len(), b.device_uids()),
        Err(e) if is_log_missing(&e) => println!("query_notify → -17"),
        Err(e) => println!("query_notify 出错（可容忍）: {e}"),
    }
    let user = client.link().user.clone();
    match client.device_config_list(&user, 0, max.max_log).await {
        Ok(b) => println!("device_config_list: {} 项", b.len()),
        Err(e) if is_log_missing(&e) => println!("device_config_list → -17"),
        Err(e) => println!("device_config_list 出错（可容忍）: {e}"),
    }
}
