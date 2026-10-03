//! ★ 实测结论固化：`func=get_meta` / `get_meta_profile` **不可用**。
//!
//! # 结论（2026-10-03 实机，QPKG 5.0.0.7 build 20260723）
//!
//! 逆向报告把 `get_meta` / `get_meta_profile` 列为「最有价值的可抄资产」
//! （`.research/new/Qsync_ELF逆向分析报告.md` §11.2），**实机证伪**：
//!
//! | func | `qsyncsrv.cgi` | `filemanager/utilRequest.cgi` | `qboxRequest.cgi` |
//! |---|---|---|---|
//! | `get_meta` | **HTTP 500**（Apache HTML 错误页） | `status: 20` | XML `authPassed` |
//! | `get_meta_profile` | **`status: 19`** | `status: 20` | XML `authPassed` |
//!
//! 判读：
//!
//! 1. **`get_meta` 的 500 是 CGI 自身崩溃，不是参数名不对。**
//!    证据：同一个端点上**瞎编的** `func=zzz_not_a_func` 回的是 `200` 空体，
//!    `get_tree` / `get_list` 也正常 → 分发是通的，`get_meta` 确实被匹配到 handler，
//!    是 handler 内部炸了。
//!    参数侧也已穷举：`path` / `folder` / `folder_path` / `share_path` /
//!    `source_path` / `dest_path` / `share` / `folder_number+folder0` /
//!    `recursive` / `is_recursive` / `r` / `all` / `limit+start` /
//!    `get_detail` / `syncing_folder` / `all_log` / 无参 —— **全部 500**。
//!
//! 2. **`get_meta_profile` 的 `status: 19` 是「未启用」，不是成功。**
//!    响应没有任何数据字段（仅 `version`/`build`/`status`/`success`），
//!    且换任何参数都一样 → 判为不支持。
//!    （`19` 不在服务端 `qbox_*` 私有路径的任何已知码表里；报告 §1.3 穷举后
//!    唯一确认的业务 status 是 `8`。）
//!
//! 3. **这两个 func 在 `filemanager` 上是 `status: 20`，而 `filemanager`
//!    对**瞎编 func** 也回 `status: 20`** → 在 File Station 命名空间同样不存在。
//!
//! # 影响：路线 B 的元数据方案回退到 `get_list`
//!
//! 报告 §12.1 曾建议「元数据优先 `get_meta`，降级 `get_list` 递归」。
//! 实测后 **`get_meta` 这一档必须删掉** —— 它不是「更快的路」，是死路，
//! 留着只会让人以为有捷径而反复尝试。
//!
//! 但**按需同步本身的可行性没有被推翻**：服务端 `qsyncsrv_metad` 生成的
//! `{share}/.qsync/meta/` 是**真实存在**的元数据层，只是没有可用的 CGI 出口。
//! 想用就得直读那个目录（需要 NAS 本地路径访问，非纯 HTTP 客户端能覆盖）。
//!
//! # `get_list` 递归的真实成本（真机实测）
//!
//! ```text
//! get_list 递归: 11 次请求 / 17 个条目 / 11 个目录
//! 单次往返 ≈ 157 ~ 430 ms（同一台 NAS，两次跑法波动较大）
//! ```
//! 目录 `/home/qxync-test`（7 项，含 4 个子目录）。
//! **耗时几乎全在往返次数上，与条目数关系不大** —— 11 个目录 17 个条目，
//! 每个目录只列一次（`get_list` 一页就够，没触发翻页）。
//! 推论：缓存优先 / 本地映射（v0.4.0 M9 做的事）比换接口更有效；
//! 真要提速，方向是**减少往返次数**（合并请求 / 复用映射），
//! 而不是减少返回的字节数。
//!
//! # 附带确认
//!
//! * `server_limit=256`、`cgi_number=1` —— v0.4.0 用 `server_limit`
//!   夹批量（而非写死 200）的改动方向正确，真机值已确认。
//! * 唯一登记的同步文件夹是 `/home/.Qsync`（`permission=2`），
//!   而 `/home/qxync-test` **不在同步登记内**。metad 只覆盖共享文件夹，
//!   这很可能就是 `get_meta` 在家目录路径上一律 500 的原因。
//!
//! # 跑法（保留为可复跑证据；日常开发不设置环境变量就不会跑它）
//!
//! ```bash
//! export QXNYC_TEST_HOST=<你的 NAS>
//! export QXNYC_TEST_PORT=9834
//! export QXNYC_TEST_USER=<账号>
//! export QXNYC_TEST_PASSWORD='<口令>'
//! export QXNYC_TEST_FIXTURE=/home/qxync-test
//! cargo test -p qxync-proto-test --test get_meta -- --ignored --nocapture --test-threads=1
//! ```

use qxync_client::Client;
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

async fn logged_in() -> Option<Client> {
    let (link, pw) = env_creds()?;
    let mut c = Client::new(&link).expect("构造 client");
    c.login(&link.user, &pw).await.expect("登录失败");
    Some(c)
}

fn fixture_root() -> String {
    std::env::var("QXNYC_TEST_FIXTURE").unwrap_or_else(|_| "/home/qxync-test".to_string())
}

/// 回归护栏：`get_meta` 当前**任何参数形态都不可用**。
///
/// 如果哪天 NAS 升级后它突然可用了，这个测试会**红掉**并提示重写封装，
/// 而不是让上层悄悄继续走 `get_list` 而没人知道捷径出现了。
#[tokio::test]
#[ignore = "需要真机 NAS；固化「get_meta 当前不可用」这一实测结论"]
async fn get_meta_is_not_usable() {
    let Some(client) = logged_in().await else {
        return;
    };
    let root = fixture_root();

    // 对照组：分发现状正常，否则下面的结论不成立。
    let ok = client.list(&root).await.expect("对照组 get_list 必须成功");
    println!("对照组 get_list {root} → {} 项", ok.len());

    let attempts: Vec<(&str, Vec<(&str, &str)>)> = vec![
        ("无参", vec![("func", "get_meta")]),
        ("path", vec![("func", "get_meta"), ("path", root.as_str())]),
        (
            "folder",
            vec![("func", "get_meta"), ("folder", root.as_str())],
        ),
        (
            "folder_path",
            vec![("func", "get_meta"), ("folder_path", root.as_str())],
        ),
        (
            "share_path",
            vec![("func", "get_meta"), ("share_path", root.as_str())],
        ),
        (
            "folder_number+folder0",
            vec![
                ("func", "get_meta"),
                ("folder_number", "1"),
                ("folder0", root.as_str()),
            ],
        ),
        (
            "recursive=1",
            vec![
                ("func", "get_meta"),
                ("path", root.as_str()),
                ("recursive", "1"),
            ],
        ),
        (
            "limit/start",
            vec![
                ("func", "get_meta"),
                ("path", root.as_str()),
                ("limit", "50"),
                ("start", "0"),
            ],
        ),
        ("/home", vec![("func", "get_meta"), ("path", "/home")]),
    ];

    let mut any_usable = false;
    for (label, q) in attempts {
        match client.qsync_probe(&q).await {
            Ok(body) => {
                // 200 且带真数据 = 真的可用了。
                let has_data = !body.trim().is_empty()
                    && !body.to_ascii_lowercase().contains("<html")
                    && !body.contains("status");
                if has_data {
                    any_usable = true;
                    let n = body.len().min(200);
                    println!("⚠️  get_meta[{label}] 现在**可用**了: {}", &body[..n]);
                } else {
                    println!("  get_meta[{label}] → 200 但无数据: {}", body.trim());
                }
            }
            Err(e) => println!("  get_meta[{label}] → 失败: {e}"),
        }
    }

    assert!(
        !any_usable,
        "get_meta 似乎已可用 —— 请重新逆向其签名，并考虑用它替换 get_list 递归"
    );
}

/// `get_meta_profile` 恒回 `status: 19`（无数据字段），判为不支持。
#[tokio::test]
#[ignore = "需要真机 NAS；固化「get_meta_profile 恒 status:19」这一实测结论"]
async fn get_meta_profile_is_not_usable() {
    let Some(client) = logged_in().await else {
        return;
    };
    let root = fixture_root();

    for (label, q) in [
        ("无参", vec![("func", "get_meta_profile")]),
        (
            "path",
            vec![("func", "get_meta_profile"), ("path", root.as_str())],
        ),
        (
            "file_name",
            vec![
                ("func", "get_meta_profile"),
                ("path", root.as_str()),
                ("file_name", "*"),
                ("file_total", "1"),
            ],
        ),
    ] {
        let body = client
            .qsync_probe(&q)
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        println!("  get_meta_profile[{label}] → {}", body.trim());
        assert!(
            body.contains("\"status\""),
            "{label}: 期望 status 字段（不支持的信号），实得 {body}"
        );
        assert!(
            !body.contains("datas") && !body.contains("meta"),
            "{label}: get_meta_profile 竟返回了数据 —— 请重新评估"
        );
    }
}

/// `get_list` 递归的真实成本 —— 决定元数据层还要不要继续优化。
#[tokio::test]
#[ignore = "需要真机 NAS"]
async fn get_list_recursion_cost() {
    let Some(client) = logged_in().await else {
        return;
    };
    let root = fixture_root();

    let t = std::time::Instant::now();
    let mut reqs = 0usize;
    let mut entries = 0usize;
    let mut dirs = 0usize;
    let mut stack = vec![root.clone()];
    while let Some(d) = stack.pop() {
        match client.list(&d).await {
            Ok(v) => {
                reqs += 1;
                dirs += 1;
                entries += v.len();
                for e in &v {
                    if e.isfolder {
                        stack.push(format!("{}/{}", d.trim_end_matches('/'), e.filename));
                    }
                }
                if dirs > 500 {
                    println!("（目录数超 500，提前收手）");
                    break;
                }
            }
            Err(e) => {
                println!("  list {d} 失败: {e}");
                break;
            }
        }
    }
    let ms = t.elapsed().as_millis();
    println!("get_list 递归: {reqs} 次请求 / {entries} 个条目 / {dirs} 个目录 / {ms} ms");
    println!("单次往返 ≈ {} ms", ms / reqs.max(1) as u128);

    if let Ok(m) = client.max_log().await {
        println!(
            "server_limit={:?} cgi_number={:?} sync_signal={}",
            m.server_limit, m.cgi_number, m.sync_signal
        );
    }

    assert!(reqs > 0, "至少要能列一次");
}
