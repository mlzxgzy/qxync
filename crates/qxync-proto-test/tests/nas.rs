//! 真机协议集成测试。**默认全部 `#[ignore]`**，因为需要一台真实 NAS 与账号。
//!
//! 跑法（不要把口令写进任何会提交的文件）：
//!
//! ```bash
//! export QXNYC_TEST_HOST=qnap.example.com
//! export QXNYC_TEST_PORT=9834
//! export QXNYC_TEST_USER=test1
//! export QXNYC_TEST_PASSWORD='...'
//! export QXNYC_TEST_FIXTURE=/home/qxync-test      # 由 xtask/probe/qs_fixture.py 造好
//! cargo test -p qxync-proto-test -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! 没有设置环境变量时测试直接返回（视为跳过），不会失败。

use qxync_client::Client;
use qxync_core::{LinkConfig, HOME_ROOT};

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

/// ★ M15/T2 核心验收：流式上传大文件时**进程内存不随文件大小增长**。
///
/// 旧实现 `fs::read` 整个文件 → 传 N 字节就吃 N 字节内存（4 GB 文件 = 4 GB RSS，
/// 并发几个直接 OOM）。这里量的是「传 64 MiB 时 RSS 峰值增量」：
/// 流式实现应该在几十 MiB 以内（一个 8 MiB 块 + 协议缓冲），
/// 而非 +64 MiB。
///
/// 判据故意留了余量：不做精确断言（reqwest/分配器行为随平台变），
/// 只在「内存增量远超文件大小」时失败 —— 那才是回归。
#[tokio::test]
#[ignore = "需要真机 NAS，且会写入 NAS"]
async fn stream_upload_memory_stays_flat() {
    let Some(client) = logged_in().await else {
        return;
    };
    let dir = format!("{}/rust-mem", fixture_root());
    let _ = client.mkdir(&fixture_root(), "rust-mem").await;

    let size = 64 * 1024 * 1024u64;
    let tmpdir = std::env::var("QXYNC_TEST_TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let local = tmpdir.join("qxync-mem-probe.bin");
    // 分块写磁盘，不在测试进程里造 64 MiB 的 Vec
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&local).expect("建临时文件");
        let chunk = vec![7u8; 1024 * 1024];
        for _ in 0..(size / chunk.len() as u64) {
            f.write_all(&chunk).expect("写临时文件");
        }
    }

    // 采样 RSS：/proc/self/status 的 VmHWM 是**峰值**，VmRSS 是当前值
    let read_status = |key: &str| -> Option<u64> {
        std::fs::read_to_string("/proc/self/status")
            .ok()?
            .lines()
            .find(|l| l.starts_with(key))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    let rss_before = read_status("VmRSS:").unwrap_or(0);
    let hwm_before = read_status("VmHWM:").unwrap_or(0);

    let sent = client
        .upload_file(&dir, &local, "mem-probe.bin")
        .await
        .expect("流式上传 64 MiB");
    assert_eq!(sent, size, "应发出全部 {size} 字节");

    let rss_after = read_status("VmRSS:").unwrap_or(0);
    let hwm_after = read_status("VmHWM:").unwrap_or(0);
    let grew = hwm_after.saturating_sub(hwm_before);
    let mib = |kb: u64| kb / 1024;
    println!(
        "64 MiB 流式上传：VmHWM {hwm_before}→{hwm_after} kB（峰值增量 {} MiB）、\
         VmRSS {rss_before}→{rss_after} kB",
        mib(grew)
    );

    // 峰值增量不应接近文件大小（64 MiB）。留 2 倍余量仍判失败，说明是回归。
    assert!(
        grew < size / 2,
        "峰值 RSS 增量 {} MiB 逼近文件大小（{} MiB）——流式没生效？",
        mib(grew),
        mib(size)
    );

    let e = client.stat(&dir, "mem-probe.bin").await.unwrap();
    assert_eq!(e.map(|e| e.filesize), Some(size), "服务端落盘大小不对");

    let _ = std::fs::remove_file(&local);
    let _ = client.delete_entry(&dir, "mem-probe.bin").await;
    let _ = client.delete_entry(&fixture_root(), "rust-mem").await;
}

///
/// 改动的性质决定了验证方式：流式与非流式的**唯一**区别是 body 怎么来的，
/// 服务端不该关心 —— 所以这条测试比的是「流式传的东西与原字节逐字节相同」，
/// 而不只是「传上去了」。
///
/// 覆盖三种尺寸（跨块边界是重点）+ 中文/空格文件名（multipart 边界最容易出问题的地方）。
/// 会写入真机，跑完自己清理。
#[tokio::test]
#[ignore = "需要真机 NAS，且会写入 NAS"]
async fn stream_upload_matches_bytes_upload() {
    let Some(client) = logged_in().await else {
        return;
    };
    let dir = format!("{}/rust-stream", fixture_root());
    let _ = client.mkdir(&fixture_root(), "rust-stream").await;

    // 8 MB 恰好等于 UPLOAD_CHUNK；再加 1 字节迫使服务端跨块收
    let sizes = [1u64, 4096, 8 * 1024 * 1024 + 1, 20 * 1024 * 1024];
    for size in sizes {
        // 内容用可复现的伪随机：全 0 / 全 0xFF 会让某些边角问题测不出来
        let payload: Vec<u8> = (0..size).map(|i| ((i * 31 + 7) % 251) as u8).collect();
        let name = if size == sizes[0] {
            "空 格 流式.bin".to_string()
        } else {
            format!("stream-{size}.bin")
        };

        // 临时目录可能是个只有几 MB 的 tmpfs（写大文件会 ENOSPC），
        // 所以优先用环境变量指定的目录，其次退到当前目录。
        let tmpdir = std::env::var("QXYNC_TEST_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        let local = tmpdir.join(format!("qxync-stream-{size}.bin"));
        std::fs::write(&local, &payload).unwrap_or_else(|e| {
            panic!(
                "写本地临时文件 {}（{size} 字节）失败: {e}；\
                 可用 QXYNC_TEST_TMPDIR 指定空间充足的目录",
                local.display()
            )
        });

        let sent = client
            .upload_file(&dir, &local, &name)
            .await
            .unwrap_or_else(|e| panic!("流式上传 {name}（{size} 字节）失败: {e}"));
        assert_eq!(
            sent, size,
            "{name}: upload_file 应返回真正发出的字节数 {size}"
        );

        let e = client.stat(&dir, &name).await.expect("stat").expect("存在");
        assert_eq!(e.filesize, size, "{name}: 服务端落盘大小不对");

        // 逐字节回读（小文件全量；大文件抽头尾 + 中段，避免测试本身跑很久）
        let back = if size <= 4096 {
            client
                .download_range(&dir, &name, 0, size - 1)
                .await
                .expect("回读")
        } else {
            let head = client
                .download_range(&dir, &name, 0, 4095)
                .await
                .expect("回读头");
            let mid = client
                .download_range(&dir, &name, size / 2, size / 2 + 4095)
                .await
                .expect("回读中段");
            let tail = client
                .download_range(&dir, &name, size - 4096, size - 1)
                .await
                .expect("回读尾");
            assert_eq!(head, payload[..4096], "{name}: 头部不一致");
            assert_eq!(tail, payload[size as usize - 4096..], "{name}: 尾部不一致");
            let m = (size / 2) as usize;
            assert_eq!(mid, payload[m..m + 4096], "{name}: 中段不一致");
            Vec::new()
        };
        if size <= 4096 {
            assert_eq!(back, payload, "{name}: 小文件必须逐字节一致");
        }

        let _ = std::fs::remove_file(&local);
        let _ = client.delete_entry(&dir, &name).await;
        println!("  ✓ {name}: {size} 字节流式上传并校验通过");
    }
    let _ = client.delete_entry(&fixture_root(), "rust-stream").await;
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

/// ★ M11：批量删除契约 —— `file_total=N` + N 个 `file_name` 能一次删掉同目录多项。
///
/// 这条是「删除异步化 + 攒批」收益的**前提**：如果服务端只认前 `file_total` 个，
/// 或者只处理第一个 `file_name`，那批量删除就会**静默漏删**（用户以为删了，
/// NAS 上还在），所以必须真机验证「N 个全部消失」。
///
/// 会在 `<fixture>/rust-batch-delete/` 下写入，跑完自己清理。
#[tokio::test]
#[ignore = "需要真机 NAS，且会写入 NAS"]
async fn m11_batch_delete_contract() {
    let Some(client) = logged_in().await else {
        return;
    };
    let root = fixture_root();
    let dir = format!("{root}/rust-batch-delete");
    let _ = client.delete_entries(&root, &["rust-batch-delete"]).await; // 清掉上次残留
    let _ = client.mkdir(&root, "rust-batch-delete").await;

    // 造 5 个文件（数量刻意不取 1，才能证明「多」这条路径）
    let names: Vec<String> = (0..5).map(|i| format!("b{i}.txt")).collect();
    for n in &names {
        client
            .upload_bytes(&dir, n, b"batch\n".to_vec())
            .await
            .expect("upload");
    }
    // 前置断言：5 个都在
    for n in &names {
        assert!(
            client.stat(&dir, n).await.unwrap().is_some(),
            "前置条件：{n} 应当存在"
        );
    }

    // 一次请求删全部
    let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    client
        .delete_entries(&dir, &refs)
        .await
        .expect("batch delete");

    // 核心验收：N 个**全部**消失
    for n in &names {
        assert!(
            client.stat(&dir, n).await.unwrap().is_none(),
            "{n} 必须已被删除（批量删除不能漏项）"
        );
    }

    // 空批次是合法的 no-op（worker 不该因空批白发一次请求）
    client.delete_entries(&dir, &[]).await.expect("empty no-op");

    // 混合：文件 + 目录同批。目录应一并消失。
    let _ = client.mkdir(&dir, "bdir").await;
    client
        .upload_bytes(&dir, "keep1.txt", b"x\n".to_vec())
        .await
        .unwrap();
    client
        .delete_entries(&dir, &["keep1.txt", "bdir"])
        .await
        .expect("mixed batch");
    assert!(client.stat(&dir, "keep1.txt").await.unwrap().is_none());
    assert!(client.stat(&dir, "bdir").await.unwrap().is_none());

    // 清理
    let _ = client.delete_entries(&root, &["rust-batch-delete"]).await;
    assert!(client
        .stat(&root, "rust-batch-delete")
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

/// ★ M15/T1：`Client::list` 的 `start`/`limit` 翻页能取回**超过单页 200** 的全量清单。
///
/// 这条守的是「>200 项的目录读不全」那个静默 bug 的**上游**：`list` 自己翻页，
/// 下游 `qxync-fuse` 的 `filter_visible` 才能拿到全量（它历史上多截断了一次）。
///
/// 依赖 `xtask/probe/bigdir_probe.py` 造的 501 项目录：
/// ```bash
/// export QXYNC_TEST_BIGDIR=/home/bigdir-probe
/// ```
/// 未设置该变量 → 跳过。
#[tokio::test]
#[ignore = "需要真机 NAS + 500 项目录"]
async fn list_paginates_past_200() {
    let Some(client) = logged_in().await else {
        return;
    };
    let Ok(dir) = std::env::var("QXYNC_TEST_BIGDIR") else {
        println!("跳过：未设置 QXYNC_TEST_BIGDIR");
        return;
    };

    let entries = client.list(&dir).await.expect("get_list 大目录");
    let names: Vec<&str> = entries.iter().map(|e| e.filename.as_str()).collect();
    println!("{dir} -> {} 项", names.len());

    assert!(
        names.len() > 200,
        "必须超过单页 200 条，否则这条测试没意义（实际 {}）",
        names.len()
    );

    // 不重：同名只应出现一次
    let mut uniq = names.clone();
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(uniq.len(), names.len(), "翻页结果里有重复项");

    // 不漏：拿单页大 limit 一次性取全量作对照基线
    let all = client
        .list_limit(&dir, 5000)
        .await
        .expect("get_list limit=5000");
    println!("对照 limit=5000 -> {} 项", all.len());
    assert_eq!(names.len(), all.len(), "分页结果与单页全量应一致（不漏项）");
    for n in &names {
        assert!(all.iter().any(|e| e.filename == *n), "缺项 {n}");
    }
}

// ---------------------------------------------------------------- ★ T3：区间内容可复现性
//
// qxync 的内容校验和（\`qxsync verify\`）建立在一条前提上：**同一区间两次下载
// 拿到的字节必须完全一样**。这条测试就是验这个前提 —— 它在真机上跑，因为如果
// Qsync 服务端对同一个 Range 请求返回的内容不稳定（比如负载均衡到不同后端、
// 或者做了某种内容变换），那所有校验和都会误报损坏。
//
// 顺带确认「Range 响应长度 == 请求长度」，这是 \`ensure_chunk\` 唯一已有的把关，
// 校验和是它的加强而不是替代。

/// ★ T3：同一区间重复下载必须逐字节一致（校验和方案的前提）。
#[tokio::test]
#[ignore = "需要真机 NAS（只读，不写入）"]
async fn ranged_download_is_byte_stable() {
    let Some(client) = logged_in().await else {
        return;
    };
    let dir = fixture_root();
    let entries = match client.list(&dir).await {
        Ok(v) if !v.is_empty() => v,
        _ => {
            println!("跳过（{dir} 为空，先跑 xtask/probe/qs_fixture.py）");
            return;
        }
    };
    // 挑一个大一点的普通文件，区间才够有意思
    let Some(e) = entries
        .iter()
        .filter(|e| !e.isfolder && e.filesize > 400 * 1024)
        .max_by_key(|e| e.filesize)
    else {
        println!("跳过（{dir} 里没有 > 400 KiB 的文件）");
        return;
    };
    println!("验证 {}/{}（{} 字节）", dir, e.filename, e.filesize);

    // 取头、中、尾三个 128 KiB 区间（与 qxync 的区间大小一致）
    let cs = 128 * 1024u64;
    for (label, start) in [
        ("首区间", 0u64),
        ("中间", (e.filesize / 2) / cs * cs),
        ("末区间", e.filesize.saturating_sub(cs) / cs * cs),
    ] {
        let end = (start + cs - 1).min(e.filesize.saturating_sub(1));
        let a = client
            .download_range(&dir, &e.filename, start, end)
            .await
            .expect("第一次区间下载");
        let b = client
            .download_range(&dir, &e.filename, start, end)
            .await
            .expect("第二次区间下载");

        // 长度必须与请求一致（ensure_chunk 的既有把关）
        assert_eq!(
            a.len() as u64,
            end - start + 1,
            "{label} [{start}..={end}] 返回长度与请求不符"
        );
        // ★ 核心：两次必须逐字节一致，否则校验和会一直误报
        assert_eq!(
            a, b,
            "{label} [{start}..={end}] 两次下载内容不同 —— 校验和方案的前提不成立"
        );

        // 相邻区间不能重叠（错位会让 per-chunk 校验和互相矛盾）
        if start > 0 {
            let prev_end = start - 1;
            let prev = client
                .download_range(&dir, &e.filename, start - cs, prev_end)
                .await
                .expect("前一区间");
            assert_eq!(
                prev.len() as u64,
                cs,
                "前一区间长度异常（末区间不完整时应为 start..=end）"
            );
        }
        println!("  {label} [{start}..={end}] {} 字节，两次一致 ✓", a.len());
    }
}
