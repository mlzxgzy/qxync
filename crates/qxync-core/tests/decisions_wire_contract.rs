//! ★ `decisions` 的 **wire 契约锁定测试**（`action="diff"`）。
//!
//! ## 为什么这个文件存在
//!
//! 踩过一次的坑：`DiffOutcome` 是 `#[serde(tag = "status")]` 的 **newtype 变体**，
//! serde 会把内层 `DiffResult` 的字段**平铺**到与 `status` 同级：
//!
//! ```jsonc
//! // ✅ wire 上真实的样子（平级）
//! {"status":"diff","lines":[{"kind":"+","local_no":3,"remote_no":0,"text":"x"}],
//!  "added":1,"removed":0,"truncated":false}
//!
//! // ❌ 曾经以为的样子（嵌套）—— 前端照这个读，于是永远读不到东西
//! {"status":"diff","diff":{"lines":[…],"added":1,…}}
//! ```
//!
//! 前端 `buildDiffView` 当时写的是 `var r = isObj(o.diff) ? o.diff : {}`。
//! 因为 `{}` 这个兜底**不报错**，界面会安安静静显示
//! 「本地多 +0 / -0」+「两边内容逐行相同。」——
//! 那正是这段代码注释里反复强调要避免的「危险的假象」：
//! 没比过和比出零差异，在界面上长得一模一样。
//!
//! ## 这个测试的作用
//!
//! 把「平级」这件事**钉在编译期测试里**。serde 的形状变了、
//! 或者有人给 `DiffOutcome` 加了自定义 `Serialize`，这里就会红。
//!
//! 只断言 `v.get(key)` 而**不按字符串精确匹配** —— `Cargo.toml` 没开
//! `serde_json` 的 `preserve_order`，`Value::Object` 底层是 `BTreeMap`，
//! 走 IPC 的真实字节里键是字母序的。按顺序比对会假失败。

use qxync_core::decisions::{DiffOutcome, DiffResult, DiffSkip};
use qxync_core::ipc::DiffData;

fn sample_lines() -> Vec<qxync_core::decisions::DiffLine> {
    vec![
        qxync_core::decisions::DiffLine {
            kind: ' ',
            local_no: 1,
            remote_no: 1,
            text: "# qxync 部署配置".into(),
        },
        // ★ 「-」= 只有**本地**有 → 本地行号 > 0，NAS 行号给 0
        //    （前端要显示成空白，不能显示 0）。方向与 `diff 本地 NAS` 一致。
        qxync_core::decisions::DiffLine {
            kind: '-',
            local_no: 2,
            remote_no: 0,
            text: "workers = 8".into(),
        },
        // ★ 「+」= 只有 **NAS** 有 → NAS 行号 > 0，本地行号给 0
        qxync_core::decisions::DiffLine {
            kind: '+',
            local_no: 0,
            remote_no: 2,
            text: "workers = 4".into(),
        },
    ]
}

// ============================================================ status="diff"

/// ★ 核心断言：`DiffResult` 的字段与 `status` **平级**，不在 `diff` 键下。
#[test]
fn diff_outcome_fields_are_flat_next_to_status() {
    let v = serde_json::to_value(DiffOutcome::Diff(DiffResult {
        lines: sample_lines(),
        added: 1,
        removed: 1,
        truncated: false,
    }))
    .expect("序列化 DiffOutcome");

    assert_eq!(v.get("status").and_then(|x| x.as_str()), Some("diff"));

    // 平级字段都在
    assert_eq!(v.get("added").and_then(|x| x.as_u64()), Some(1));
    assert_eq!(v.get("removed").and_then(|x| x.as_u64()), Some(1));
    assert_eq!(v.get("truncated").and_then(|x| x.as_bool()), Some(false));
    assert_eq!(
        v.get("lines").and_then(|x| x.as_array()).map(|a| a.len()),
        Some(3)
    );

    // ★ 决定性的反断言：没有 `diff` 这一层。
    //   前端就是读错了这个键才恒显零差异；一旦这里有值，测试立刻失败。
    assert!(
        v.get("diff").is_none(),
        "DiffOutcome::Diff 的字段被包进了 `diff` 键 —— 前端 buildDiffView \
         读 o.diff 会重新对上，但那是巧合。确认形状改了，并同步改 app.js:3761"
    );
}

/// `DiffLine` 的字段名也是平级的：`kind` / `local_no` / `remote_no` / `text`。
///
/// ★ `kind` 是 `char`，serde_json 把它写成**单字符串** `"+"` 而不是数字 `43` ——
///   前端 `str(x.kind) === '+'` 靠的就是这一点。
#[test]
fn diff_line_field_names_and_char_encoding() {
    let v = serde_json::to_value(DiffOutcome::Diff(DiffResult {
        lines: sample_lines(),
        added: 1,
        removed: 1,
        truncated: false,
    }))
    .expect("序列化");
    let lines = v.get("lines").and_then(|x| x.as_array()).expect("lines");

    let first = &lines[0];
    assert_eq!(first.get("kind").and_then(|x| x.as_str()), Some(" "));
    assert_eq!(first.get("local_no").and_then(|x| x.as_u64()), Some(1));
    assert_eq!(first.get("remote_no").and_then(|x| x.as_u64()), Some(1));
    assert_eq!(
        first.get("text").and_then(|x| x.as_str()),
        Some("# qxync 部署配置")
    );

    let del = &lines[1];
    assert_eq!(del.get("kind").and_then(|x| x.as_str()), Some("-"));
    assert!(
        del.get("local_no").and_then(|x| x.as_u64()).unwrap_or(0) > 0,
        "「-」行 = 只有本地有，必须带本地行号（前端据此渲染左边一栏）"
    );
    assert_eq!(
        del.get("remote_no").and_then(|x| x.as_u64()),
        Some(0),
        "「-」行 NAS 没有对应行 → remote_no=0，前端渲染成空白而不是 0"
    );

    let add = &lines[2];
    assert_eq!(add.get("kind").and_then(|x| x.as_str()), Some("+"));
    assert!(
        add.get("remote_no").and_then(|x| x.as_u64()).unwrap_or(0) > 0,
        "「+」行 = 只有 NAS 有，必须带 NAS 行号"
    );
    assert_eq!(
        add.get("local_no").and_then(|x| x.as_u64()),
        Some(0),
        "「+」行本地没有对应行 → local_no=0"
    );
}

/// ★ 方向契约（曾写反过一次）：`-` = 只有本地有、`+` = 只有 NAS 有。
///
/// 这条用**生产函数**的真实输出钉住，防止"注释/夹具"再次跑偏：
/// 本地有 `b`、NAS 有 `x` → `-` 行必须是 `b` 且 local_no>0，`+` 行必须是 `x` 且 remote_no>0。
#[test]
fn diff_direction_is_local_minus_remote_plus() {
    use qxync_core::decisions::text_diff;

    let d = text_diff("a\nb\nc\n", "a\nx\nc\n");
    assert_eq!((d.added, d.removed), (1, 1));

    let del = d.lines.iter().find(|l| l.kind == '-').expect("'-' 行");
    assert_eq!(del.text, "b", "'-' = 只有本地有 → 文本必须是本地内容");
    assert!(del.local_no > 0, "'-' 行本地行号必须 >0");
    assert_eq!(del.remote_no, 0, "'-' 行 NAS 行号为 0");

    let add = d.lines.iter().find(|l| l.kind == '+').expect("'+' 行");
    assert_eq!(add.text, "x", "'+' = 只有 NAS 有 → 文本必须是 NAS 内容");
    assert!(add.remote_no > 0, "'+' 行 NAS 行号必须 >0");
    assert_eq!(add.local_no, 0, "'+' 行本地行号为 0");

    // 整文件为空 vs 非空时不 panic（daemon 的空文件分支依赖它）
    let d = text_diff("a\nb\n", "");
    assert_eq!(
        (d.added, d.removed),
        (0, 2),
        "NAS 为空 → 本地两行都只有本地有"
    );
}

// ============================================================ status="skipped"

/// ★ `DiffSkip` 也带 `#[serde(tag="reason")]` → `reason` 与 `status` **平级**。
///   前端 `diffSkipText` 读的正是 `o.reason`，这一半本来就是对的。
#[test]
fn skipped_outcome_flattens_reason_with_status() {
    let cases: Vec<(DiffOutcome, &str, Vec<(&str, serde_json::Value)>)> = vec![
        (DiffOutcome::Skipped(DiffSkip::Binary), "binary", vec![]),
        (DiffOutcome::Skipped(DiffSkip::IsDir), "is_dir", vec![]),
        (
            DiffOutcome::Skipped(DiffSkip::TooLarge {
                size: 5242880,
                limit: 524288,
            }),
            "too_large",
            vec![
                ("size", serde_json::json!(5242880u64)),
                ("limit", serde_json::json!(524288u64)),
            ],
        ),
        (
            DiffOutcome::Skipped(DiffSkip::NoContent {
                which: "local".into(),
                detail: "本地尚未水合".into(),
            }),
            "no_content",
            vec![
                ("which", serde_json::json!("local")),
                ("detail", serde_json::json!("本地尚未水合")),
            ],
        ),
    ];

    for (outcome, reason, extra) in cases {
        let v = serde_json::to_value(outcome).expect("序列化");
        assert_eq!(
            v.get("status").and_then(|x| x.as_str()),
            Some("skipped"),
            "skipped 侧 status 必须是 skipped"
        );
        assert_eq!(
            v.get("reason").and_then(|x| x.as_str()),
            Some(reason),
            "reason 必须与 status 平级，前端 diffSkipText 读的就是 o.reason"
        );
        for (k, want) in extra {
            assert_eq!(v.get(k), Some(&want), "字段 {k} 缺失或值不对");
        }
        // skipped 侧绝不能出现 diff 行 —— 界面据此走 buildDiffSkip 分支
        assert!(
            v.get("lines").is_none(),
            "{reason} 不该带 lines（带了界面就会去画空 diff 面板）"
        );
    }
}

// ============================================================ 整包 DiffData

/// 整包形状（daemon 的 `decision_diff_cmd` 直接 `to_value(DiffData{..})`）。
#[test]
fn diff_data_wraps_outcome_under_outcome_key() {
    let data = DiffData {
        path: "/home/user/deploy.conf".into(),
        local_size: 4821,
        remote_size: 4790,
        outcome: DiffOutcome::Diff(DiffResult {
            lines: sample_lines(),
            added: 1,
            removed: 1,
            truncated: false,
        }),
    };
    let v = serde_json::to_value(&data).expect("序列化");

    assert_eq!(
        v.get("path").and_then(|x| x.as_str()),
        Some("/home/user/deploy.conf")
    );
    assert_eq!(v.get("local_size").and_then(|x| x.as_u64()), Some(4821));
    assert_eq!(v.get("remote_size").and_then(|x| x.as_u64()), Some(4790));

    let o = v
        .get("outcome")
        .and_then(|x| x.as_object())
        .expect("outcome 必须是对象");
    assert_eq!(o.get("status").and_then(|x| x.as_str()), Some("diff"));
    assert!(o.get("diff").is_none(), "outcome 里不该再包一层 `diff`");
    assert_eq!(o.get("added").and_then(|x| x.as_u64()), Some(1));
}

/// 往返：wire → Rust 必须能解回来（GUI 侧将来若改走 `call_typed::<DiffData>`，
/// 这里的形状就是它依赖的契约）。
#[test]
fn diff_data_round_trips_through_json() {
    let data = DiffData {
        path: "/home/user/notes.md".into(),
        local_size: 1200,
        remote_size: 1180,
        outcome: DiffOutcome::Skipped(DiffSkip::TooLarge {
            size: 5_242_880,
            limit: 524_288,
        }),
    };
    let text = serde_json::to_string(&data).expect("序列化");
    let back: DiffData = serde_json::from_str(&text).expect("反序列化");
    assert_eq!(back.path, data.path);
    assert_eq!(back.local_size, data.local_size);
    assert_eq!(back.remote_size, data.remote_size);
    assert_eq!(back.outcome, data.outcome);
}

/// `DiffData` 是 `#[serde(default)]`，且 `DiffOutcome` 手写了 `Default`
/// 落在 `Skipped(NoContent{which:"unknown"})` ——
/// **绝不能默认成 `Diff(空)`**，那会让界面显示「两边一样」。
#[test]
fn missing_outcome_falls_back_to_skipped_never_to_empty_diff() {
    let back: DiffData =
        serde_json::from_str(r#"{"path":"/x","local_size":1,"remote_size":2}"#).expect("反序列化");
    match back.outcome {
        DiffOutcome::Skipped(DiffSkip::NoContent { which, detail }) => {
            assert_eq!(which, "unknown");
            assert_eq!(detail, "尚未对比");
        }
        other => panic!(
            "缺 outcome 时必须回落到 Skipped，实际 {:?} —— \
             回落成 Diff(空) 会让界面显示假的「两边内容逐行相同」",
            other
        ),
    }
}

/// 缺 `status` 时**不该**悄悄变零差异：内部 tag enum 反序列化要求这个键。
/// （`#[serde(default)]` 在容器级，不覆盖 enum 内部 tag。）
#[test]
fn missing_status_tag_is_rejected_not_silently_zero_diff() {
    let r = serde_json::from_str::<DiffData>(
        r#"{"path":"/x","local_size":1,"remote_size":2,"outcome":{"lines":[],"added":0,"removed":0}}"#,
    );
    assert!(
        r.is_err(),
        "outcome 缺 status tag 必须报错（不能被当成 Diff(空) 静默接受）"
    );
}
