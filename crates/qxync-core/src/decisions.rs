//! # 冲突待裁决队列的**生命周期巡检** + **本地/NAS 文本对比**
//!
//! 这个模块解决两件事，都刻意做成**纯函数**（不碰网络、不碰挂载），便于离线单测：
//!
//! ## 1. 巡检（[`sweep_decision`]）
//!
//! 队列里的行原本**只有一条删除路径**：引擎执行完裁决后调 `finish_decision` 删行。
//! 于是三个常见情况会让行永久残留 —— 冲突自己消失了、任务策略从 `ask` 改成了别的、
//! 任务被删了。行留着不会丢数据（裁决本来也只是「下次同步时按它执行」），但用户看到
//! 的是「我明明决定好了，怎么还在」—— **看着像卡住，实际无法处理**。
//!
//! [`sweep_decision`] 给每行一个明确结论：留下（`Keep`）还是出队（`Drop` + 原因）。
//! 判据只用**已经知道的事实**（远端签名、本地签名、baseline、任务是否还在、策略是什么），
//! 不做任何猜测。**关键约束：宁可留，不可误删** —— 误删 = 用户的裁决意图被静默吞掉。
//!
//! ⚠️ **「当前没有挂载」不等于「任务被删」**：daemon 刚启动 / 用户卸载 / 恢复挂载失败时
//! 本地根本没有视图可看。这种情况必须走 [`DecisionFacts::observed`] = `false` 的分支：
//! 只用「任务还在不在 / 策略还是不是 ask」判断，**绝不**拿占位的本地/远端签名判
//! 「NAS 上的文件已不存在」—— 那正是曾经把整队待裁决静默删光的路径。
//!
//! ⚠️ **「远端 stat 到了」也不等于「本地看过了」**：本地视图的节点表是进程内的，
//! daemon 重启 / 重挂之后没访问过的路径取不到节点（`local_now` 只是占位值）。
//! 所以「两边已不冲突」这条判据额外要求 [`DecisionFacts::local_observed`]，否则
//! 会把「已裁决待执行」的行误判成「冲突已自行消解」清掉。
//!
//! ## 2. 文本对比（[`text_diff`]）
//!
//! 裁决「保留本地还是保留 NAS」需要看**内容差在哪**，光看 size/mtime 没法决定。
//! 这里做行级 LCS diff，输出单栏 unified 形态。
//!
//! **方向与 `diff 本地 NAS` 的 unified 约定一致**（本地是「旧」、NAS 是「新」）：
//!
//! * `-` / `removed` = **只有本地有**的行（保留 NAS 会覆盖掉它们）；
//! * `+` / `added`   = **只有 NAS 有**的行（保留本地会推到 NAS 去）；
//! * ` ` = 两边相同（只出现在变更块前后各 [`CONTEXT`] 行的上下文里）。
//!
//! 注意行号字段的方向：`-` 行有本地行号、`remote_no == 0`；`+` 行有 NAS 行号、
//! `local_no == 0`。界面据此渲染空白而不是 0。
//!
//! **不做**的事：不做语法高亮、不做二进制 diff、不做三方合并。超限 / 二进制一律
//! 走 [`DiffOutcome::Skipped`]，**由调用方显示原因** —— 静默返回空 diff 会让用户
//! 以为「两边一样」，那是危险的假象。对比区域过大时**按行数截断输入**（仍然做
//! 真正的 LCS，只是只对比前 [`MAX_OUT_LINES`] 行），并把 [`DiffResult::truncated`]
//! 置真，绝不假装比完了整个文件。

use crate::sync::{Decision, LocalSig, Sig};
use crate::tasks::{CONFLICT_ASK, DEFAULT_CONFLICT};

/// 文本对比的大小上限：超过就只给摘要。
///
/// 512 KiB 覆盖绝大多数源码 / 配置 / 日志类冲突；再大则一次 diff 要从 NAS 拉
/// 十几兆，界面会明显卡住，而那种文件肉眼本来也不适合逐行看。
pub const DIFF_MAX_BYTES: u64 = 512 * 1024;

// ==================================================================== 巡检

/// 一行待裁决条目的巡检结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SweepVerdict {
    /// 留在队列里。
    Keep,
    /// 出队，并带上**人话原因**（直接显示给用户，所以要具体，不能是「已失效」）。
    Drop(String),
}

/// 一行队列条目在当前事实下的全部已知状态。
///
/// **刻意不 derive `Default`** —— `Sig` 没有 `Default`，而且「什么都不存在的
/// 事实」本身没有意义（`baseline` 缺失要用 `Sig::MISSING` 明确表达）。
#[derive(Debug, Clone)]
pub struct DecisionFacts {
    /// **本轮的*远端*现状是否真的观测到了**。
    ///
    /// `false` = 没有挂载视图（daemon 刚启动 / 用户卸载了 / 恢复挂载失败），
    /// 此时 [`Self::remote_now`] 只是**占位值**，**任何依赖它的判据都不许用**。
    /// 把它显式建模出来是为了修一个真实的事故面：以前「没有视图」会被伪造成
    /// `remote_now = Sig::MISSING`，于是巡检第 1 条（远端已不存在）命中 →
    /// 把整队待裁决**静默删光**，理由还显示成「NAS 上的文件已不存在」。
    /// 没看到 ≠ 不存在。
    ///
    /// ⚠️ 本地侧有**独立的** [`Self::local_observed`]：远端 stat 到了不代表
    /// 本地视图里也有这个节点（见那里的说明）。
    pub observed: bool,
    /// **本轮的*本地*现状是否真的观测到了**（本地视图的节点表里有这条路径）。
    ///
    /// 为什么不能只看 [`Self::observed`]：本地视图的节点表是**进程内**的
    /// （`LocalView::nodes()` 只返回本次进程已物化的节点；对账时推给视图的远端
    /// 清单只是快照、**不物化节点**；持久化的节点索引里也没有「脏」位）。
    /// 于是 **daemon 重启 / 用户重挂 / 恢复任务之后**，任何还没被访问过的路径
    /// 都取不到节点 → [`Self::local_now`] 退回占位 `Sig::MISSING`、
    /// `local_dirty=false`。那不是「本地没有」，是「本进程还没看过」。
    ///
    /// 拿它跑三向决策的后果（真机事故面）：已裁决（`resolution` 非空）的行会被
    /// 判成「冲突已自行消解（Noop / RefreshRemote）」**静默出队** —— 用户的
    /// 保留意图从未执行就被吞掉了。所以第 4 条判据要求本地侧也真的观测到。
    /// **不知道就留**（GUI 有「忽略这条」出口），绝不拿没看过的状态下结论。
    pub local_observed: bool,
    /// 队列行里记的**本地**签名（发现冲突那一刻）。
    pub recorded_local: Sig,
    /// 队列行里记的**远端**签名（发现冲突那一刻）。
    pub recorded_remote: Sig,
    /// 已裁决？（`resolution` 非空）
    pub resolved: bool,
    /// 所属任务是否还存在（任务被删 → 没人会执行这条裁决）。
    pub task_alive: bool,
    /// 所属任务当前的冲突策略。
    pub policy: String,
    /// **现在**的本地签名（`Sig::MISSING` = 本地已不存在；
    /// 仅 [`Self::local_observed`] 时有意义，否则是占位值）。
    pub local_now: Sig,
    /// 本地当前有没有未上传的改动（仅 [`Self::local_observed`] 时有意义）。
    pub local_dirty: bool,
    /// **现在**的远端签名（`Sig::MISSING` = NAS 上已不存在；仅 [`Self::observed`] 时有意义）。
    pub remote_now: Sig,
    /// baseline 里记的签名。
    pub baseline: Sig,
}

/// 巡检一行队列条目：还留不留。
///
/// ## 判据的顺序（顺序本身是设计的一部分）
///
/// 从**最确定**的往**最不确定**的排，先命中先返回：
///
/// 0. **本轮根本没观测到现状**（没有挂载视图）→ 完全跳过第 1、4 条判据
///    （它们依赖「现在」的本地/远端签名，而那只是占位值）。此时只按
///    「任务还在不在 / 策略还是不是 ask」判断 —— 任务在就留（挂载回来还要执行），
///    任务确实没了才出队。这条是「宁可留，不可误删」的第一道闸。
/// 1. **远端已不存在** → 冲突对象没了，裁决失去意义 → 出队。
/// 2. **任务已不存在** → 没人会执行这条裁决 → 出队（`resolved` 的更要清：
///    它永远等不到执行了，留着只是让人以为还有事没做）。
/// 3. **策略已不是 `ask`** → 引擎不会再走 `CONFLICT_ASK` 分支，这条行永远读不到
///    → 出队。**这条是最常见的残留原因**（用户把任务改成 `rename_local` 就中招）。
/// 4. **两边已经不冲突**（用当前事实跑一遍 [`decide`]，结论不是 `Conflict`）
///    → 冲突自己消解了（用户手动改一致 / 上传已落地 / 远端回退了）
///    → 出队。**只对已裁决的生效**：未裁决的还冲突着就要留着问用户。
/// 5. 其余 → 留。
///
/// ## 为什么第 4 条只对已裁决的生效
///
/// 未裁决的条目如果此刻恰好不冲突（比如远端刚好被回滚），那它**本来就不该问**了，
/// 一样该出队。但「恰好不冲突」和「稳定不冲突」在单轮里分不清 —— 远端可能下一轮
/// 又变。所以未裁决的保持保守：**除非远端真的没了或任务/策略不再适用，否则一律留着**，
/// 让用户自己点（GUI 有「忽略这条」）。宁可多留一条问用户，也不要静默吞掉一个
/// 真实的冲突。
pub fn sweep_decision(f: &DecisionFacts) -> SweepVerdict {
    // 1. 冲突对象（NAS 上那份）已经没了 —— 裁决它没有意义。
    //    ★ 只有**真的看过**远端才许判：没挂载时 `remote_now` 是占位值。
    if f.observed && !f.remote_now.exists && f.recorded_remote.exists {
        return SweepVerdict::Drop("NAS 上的文件已不存在，冲突自动作废".into());
    }

    // 2. 任务没了 → 没人执行这条裁决
    if !f.task_alive {
        return SweepVerdict::Drop(if f.resolved {
            "所属任务已删除，这条裁决不会再被执行".into()
        } else {
            "所属任务已删除".into()
        });
    }

    // 3. 策略不再是「每个文件都问我」→ 引擎不再读这张表
    let policy = if f.policy.is_empty() {
        DEFAULT_CONFLICT
    } else {
        f.policy.as_str()
    };
    if policy != CONFLICT_ASK {
        return SweepVerdict::Drop(format!(
            "任务策略已改为「{}」，不再走待裁决队列",
            crate::tasks::conflict_label(policy)
        ));
    }

    // 4. 已裁决的：两边已经不冲突 → 裁决已经「事实上」落地或已无意义。
    //    ★ 要求**远端**（`observed`）与**本地**（`local_observed`）两侧都真的观测过：
    //      - 没挂载时 `remote_now` 是占位值；
    //      - 挂载着但本地节点还没物化时（daemon 刚重启 / 路径没被访问过）
    //        `local_now` 同样是占位值 —— 拿它跑三向决策会把「本进程还没看过」
    //        当成「本地没有」，把已裁决的行误判成「已消解」清掉（吞用户裁决意图）。
    if f.observed && f.local_observed && f.resolved {
        let d = decide_now(f);
        if d != Decision::Conflict {
            return SweepVerdict::Drop(format!("冲突已自行消解（{d:?}），不再需要裁决"));
        }
    }

    SweepVerdict::Keep
}

/// 用**当前**事实跑一遍三向决策（巡检内部用；也让调用方能拿到同样的结论）。
pub fn decide_now(f: &DecisionFacts) -> Decision {
    let local = LocalSig {
        sig: f.local_now,
        dirty: f.local_dirty,
    };
    crate::sync::decide(&local, &f.baseline, &f.remote_now)
}

/// 巡检一批条目，返回**该删的 id**（`Keep` 的不在结果里）。
///
/// 逐行独立判断：某行的事实查不到（`facts_for` 返回 `None`）就**跳过那行**，
/// 绝不当成「可以删」—— 查不到 ≠ 失效。
pub fn sweep_batch<F>(rows: &[crate::store::DecisionRow], mut facts_for: F) -> Vec<(String, String)>
where
    F: FnMut(&crate::store::DecisionRow) -> Option<DecisionFacts>,
{
    let mut out = Vec::new();
    for r in rows {
        let Some(f) = facts_for(r) else { continue };
        if let SweepVerdict::Drop(why) = sweep_decision(&f) {
            out.push((r.id.clone(), why));
        }
    }
    out
}

// ==================================================================== 文本对比

/// 一行 diff。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiffLine {
    /// ` `=两边相同 / `-`=只有本地有 / `+`=只有 NAS 有。
    ///
    /// 方向与 `diff 本地 NAS` 一致（本地是「旧」、NAS 是「新」）：
    /// `-` 是本地独有的行，`+` 是 NAS 独有的行。
    pub kind: char,
    /// 本地行号（1 起；`+` 行表示本地没有对应行 → 0）。
    pub local_no: u32,
    /// NAS 行号（1 起；`-` 行表示 NAS 没有对应行 → 0）。
    pub remote_no: u32,
    pub text: String,
}

/// diff 结果。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiffResult {
    /// 单栏 diff 行（本地侧视角）。
    pub lines: Vec<DiffLine>,
    /// **只有 NAS 有**的行数（保留本地会推到 NAS 的那些）。
    ///
    /// ★ 若 [`Self::truncated`] 是因**输入被截断**而置真，这里也只统计已对比的部分
    /// （不是整文件）—— 界面不能把它当成「全部差异」。
    pub added: usize,
    /// **只有本地有**的行数（保留 NAS 会覆盖掉的那些）。
    ///
    /// ★ 同 [`Self::added`]：输入被截断时只是已对比部分的统计。
    pub removed: usize,
    /// 是否有截断。**两种来源，语义不同**：
    ///
    /// 1. **输入被截断**：差异区域的行数超过 [`MAX_OUT_LINES`]（如 512 KiB 文本
    ///    被切成几十万行短行）→ 只取前 [`MAX_OUT_LINES`] 行做精确 LCS，
    ///    此时 `added` / `removed` **只覆盖已对比的那部分**；
    /// 2. **输出被截断**：变更行数超过 [`MAX_OUT_LINES`] → `lines` 只保留前
    ///    [`MAX_OUT_LINES`] 行，但 `added` / `removed` 仍是**已对比范围内**的完整统计。
    ///
    /// 无论哪种，界面都必须把 `truncated=true` 明确展示出来：它不是「只有这些差异」。
    pub truncated: bool,
}

/// 对比为什么没做出来。
///
/// **刻意带上原因并可序列化** —— 这个类型要经 IPC 回传 GUI，界面必须能把
/// 「文件 4.2MB，超出 512 KiB 对比上限」这句话显示出来。静默返回空 diff
/// 会让用户以为「两边一样」，那是危险的假象。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum DiffSkip {
    /// 超过 [`DIFF_MAX_BYTES`]。
    TooLarge { size: u64, limit: u64 },
    /// 二进制（含 NUL 字节或不是合法 UTF-8）。
    Binary,
    /// 目录。
    IsDir,
    /// 有一侧的内容拿不到（没缓存 / 拉不下来）—— **要显示出来，不能当「无差异」**。
    NoContent {
        /// `"local"` 或 `"remote"`。
        which: String,
        detail: String,
    },
}

/// 对比的完整结果：**要么有 diff，要么明说为什么没有**。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DiffOutcome {
    /// 成功。
    Diff(DiffResult),
    /// 不做（附原因）。
    Skipped(DiffSkip),
}

/// `Default` = 「没做对比，且原因未知」。
///
/// **不能默认成 `Diff(空)`** —— 那会让界面显示「零差异」，用户以为两边一样，
/// 而真相是我们根本还没比。默认必须落在「跳过」这一侧。
impl Default for DiffOutcome {
    fn default() -> Self {
        DiffOutcome::Skipped(DiffSkip::NoContent {
            which: "unknown".into(),
            detail: "尚未对比".into(),
        })
    }
}

/// 上下文行数：每个变更块前后各保留几行。
const CONTEXT: usize = 3;

/// 上限：返回给界面的最大行数（超出截断，防止几万行 diff 撑爆界面）。
///
/// 同一个值也是 **LCS 输入的行数上限**（见 [`text_diff`]）：差异区域两侧都超过它时，
/// 只取前 `MAX_OUT_LINES` 行做精确 LCS，并置 `truncated=true`。两处共用是为了让
/// 内存上界只有一个数字：最坏情况 DP 表 `(MAX_OUT_LINES+1)² × 4B ≈ 16 MiB`，
/// 而不是「行数平方」级的无边分配（曾经 512 KiB 的 1 字符短行文本 = 26 万行/侧
/// → 26万×26万×4B ≈ 275 GB）。
const MAX_OUT_LINES: usize = 2000;

/// 行级 LCS diff（本地 vs NAS），**本地侧视角**。
///
/// * `-` = 只有本地有（保留 NAS 会覆盖掉的那部分）
/// * `+` = 只有 NAS 有（保留本地会推到 NAS 的内容）
///
/// ## 为什么用 LCS 而不是 Myers
///
/// Myers 的 O(ND) 在「两边差异大」时 D 很大，退化成 O(N·M)；LCS 的 O(N·M) 表大小
/// 只由行数决定，`u32` 表够用，而且**实现简单**、行为可预测。
///
/// ## 有界回退（必须保留的硬约束）
///
/// 差异区域的行数可能很大（512 KiB 上限内，1 字符一行的文本就有 26 万行/侧），
/// 直接建 `(n+1)×(m+1)` 的 DP 表会要几百 GB。所以**输入行数有界**：
/// 若 `(n+1)·(m+1)` 超过 `(MAX_OUT_LINES+1)²`，就把两侧差异区域各截到前
/// [`MAX_OUT_LINES`] 行再做精确 LCS，并置 [`DiffResult::truncated`] 为真。
///
/// 这条回退**不允许**退化成「逐行相同」的假象：公共前后缀已经掐掉，差异区域的第一行
/// 一定不同，所以截断后至少产生一条 `-`/`+`；同时 `truncated=true` 会由界面明确展示。
/// 小输入（差异区域 ≤ `MAX_OUT_LINES`²）仍然得到完整的 LCS 质量。
pub fn text_diff(local: &str, remote: &str) -> DiffResult {
    let a: Vec<&str> = local.lines().collect();
    let b: Vec<&str> = remote.lines().collect();

    // 掐掉公共前后缀 —— 真实冲突文件通常只有中间几行不同，这一步能把
    // O(N·M) 的规模压到实际差异的量级。
    let mut start = 0usize;
    while start < a.len() && start < b.len() && a[start] == b[start] {
        start += 1;
    }
    let mut end_a = a.len();
    let mut end_b = b.len();
    while end_a > start && end_b > start && a[end_a - 1] == b[end_b - 1] {
        end_a -= 1;
        end_b -= 1;
    }

    let mid_a = &a[start..end_a];
    let mid_b = &b[start..end_b];

    // ★ 有界回退：差异区域过大时**按行数截断输入**，绝不为「行数的平方」分配内存。
    //
    //   判定用真实的 (n+1)*(m+1) 而不是 n*m：单侧很小、另一侧很大时（例如本地 3 行
    //   vs NAS 30 万行），表其实很小，不该白白截断；只有两侧都大（真·平方爆炸）
    //   才截。截断后仍然跑同一套精确 LCS（只是范围小了），并置 truncated=true。
    let (n_full, m_full) = (mid_a.len(), mid_b.len());
    let mut input_truncated = false;
    let (n, m) =
        if (n_full + 1).saturating_mul(m_full + 1) > (MAX_OUT_LINES + 1) * (MAX_OUT_LINES + 1) {
            input_truncated = true;
            (n_full.min(MAX_OUT_LINES), m_full.min(MAX_OUT_LINES))
        } else {
            (n_full, m_full)
        };
    let mid_a = &mid_a[..n];
    let mid_b = &mid_b[..m];

    // LCS 长度表（(n+1) × (m+1)），u32 足够（n ≤ MAX_OUT_LINES << u32::MAX）
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * (m + 1) + j] = if mid_a[i] == mid_b[j] {
                dp[(i + 1) * (m + 1) + j + 1] + 1
            } else {
                dp[(i + 1) * (m + 1) + j].max(dp[i * (m + 1) + j + 1])
            };
        }
    }

    // 回溯成编辑序列：(' ', 本地下标, NAS下标) / ('-', 本地下标, None) / ('+', None, NAS下标)
    let mut ops: Vec<(char, Option<usize>, Option<usize>)> = Vec::new();
    {
        let (mut i, mut j) = (0usize, 0usize);
        while i < n && j < m {
            if mid_a[i] == mid_b[j] {
                ops.push((' ', Some(start + i), Some(start + j)));
                i += 1;
                j += 1;
            } else if dp[(i + 1) * (m + 1) + j] >= dp[i * (m + 1) + j + 1] {
                ops.push(('-', Some(start + i), None));
                i += 1;
            } else {
                ops.push(('+', None, Some(start + j)));
                j += 1;
            }
        }
        while i < n {
            ops.push(('-', Some(start + i), None));
            i += 1;
        }
        while j < m {
            ops.push(('+', None, Some(start + j)));
            j += 1;
        }
    }

    let mut added = 0usize;
    let mut removed = 0usize;
    // 中段（掐掉公共前后缀之后）逐行映射
    let mut full: Vec<(char, String, u32, u32)> = ops
        .into_iter()
        .map(|(k, ia, ib)| {
            let (text, lno, rno) = match (k, ia, ib) {
                (' ', Some(x), Some(y)) => (a[x].to_string(), (x + 1) as u32, (y + 1) as u32),
                ('-', Some(x), _) => {
                    removed += 1;
                    (a[x].to_string(), (x + 1) as u32, 0)
                }
                ('+', _, Some(y)) => {
                    added += 1;
                    (b[y].to_string(), 0, (y + 1) as u32)
                }
                // 上面的回溯保证不会落空；保底取空串（不该发生）
                _ => (String::new(), 0, 0),
            };
            (k, text, lno, rno)
        })
        .collect();

    // ★ 公共前缀 / 后缀也要能当上下文用。
    //
    //   上面为了把 O(N·M) 的规模压到实际差异量级，把公共前后缀从 LCS 里掐掉了，
    //   所以它们**不在 `full` 里**。若直接对 `full` 做上下文裁剪，变更块贴着文件
    //   头/尾时就会丢掉本该显示的那几行上下文（实测：3 行文件里改第 2 行，
    //   首行上下文消失，行号对不上）。
    //
    //   这里只回填**变更附近**的前后缀行（各 CONTEXT 行），不做全量重建 ——
    //   既补上上下文，又不会把 20 万行文件重新塞回内存。
    let prefix_start = start.saturating_sub(CONTEXT);
    let prefix_ctx: Vec<(char, String, u32, u32)> = (prefix_start..start)
        .map(|x| {
            let y = x; // 公共前缀下标两侧相同
            (' ', a[x].to_string(), (x + 1) as u32, (y + 1) as u32)
        })
        .collect();
    let suffix_end = (end_a + CONTEXT).min(a.len());
    // ★ 输入被截断时**不回填尾部上下文**：我们只对比了差异区域的前 2000 行，
    //   尾部上下文属于「还没比到」的部分，硬塞进来会让界面出现一段行号大跳跃的
    //   上下文（2000 → 250000），比没有更让人困惑。truncated=true 已明确说明。
    let suffix_ctx: Vec<(char, String, u32, u32)> = if input_truncated {
        Vec::new()
    } else {
        (end_a..suffix_end)
            .map(|x| {
                // 后缀里本地下标 x 对应 NAS 下标 x - (a.len()-b.len())
                let y = (x + b.len()) - a.len();
                (' ', a[x].to_string(), (x + 1) as u32, (y + 1) as u32)
            })
            .collect()
    };
    let mut merged: Vec<(char, String, u32, u32)> =
        Vec::with_capacity(prefix_ctx.len() + full.len() + suffix_ctx.len());
    merged.extend(prefix_ctx);
    merged.append(&mut full);
    merged.extend(suffix_ctx);
    let full = merged;

    // 加上下文：只保留变更块附近 ±CONTEXT 行
    let keep = context_mask(&full);
    let mut lines = Vec::new();
    for (i, (k, text, lno, rno)) in full.iter().enumerate() {
        if !keep[i] {
            continue;
        }
        lines.push(DiffLine {
            kind: *k,
            local_no: *lno,
            remote_no: *rno,
            text: text.clone(),
        });
    }

    let mut truncated = input_truncated;
    if lines.len() > MAX_OUT_LINES {
        lines.truncate(MAX_OUT_LINES);
        truncated = true;
    }

    DiffResult {
        lines,
        added,
        removed,
        truncated,
    }
}

/// 哪些行要保留（变更行 + 前后 CONTEXT 行）。
fn context_mask(rows: &[(char, String, u32, u32)]) -> Vec<bool> {
    let mut keep = vec![false; rows.len()];
    for (i, (k, _, _, _)) in rows.iter().enumerate() {
        if *k == ' ' {
            continue;
        }
        let lo = i.saturating_sub(CONTEXT);
        let hi = (i + CONTEXT + 1).min(rows.len());
        for m in keep.iter_mut().take(hi).skip(lo) {
            *m = true;
        }
    }
    keep
}

/// 判断是不是可以逐行看的内容。**含 NUL 字节 → 二进制**（不做启发式猜测）。
pub fn looks_binary(bytes: &[u8]) -> bool {
    // 只看前 8 KiB：NUL 通常在文件头就有，且不必为整个文件付扫描代价
    let head = &bytes[..bytes.len().min(8192)];
    head.contains(&0)
}

/// 两份字节能不能做文本 diff；不能就给出**具体原因**。
pub fn prepare_bytes(local: &[u8], remote: &[u8]) -> Result<(String, String), DiffSkip> {
    if local.len() as u64 > DIFF_MAX_BYTES || remote.len() as u64 > DIFF_MAX_BYTES {
        let size = local.len().max(remote.len()) as u64;
        return Err(DiffSkip::TooLarge {
            size,
            limit: DIFF_MAX_BYTES,
        });
    }
    if looks_binary(local) || looks_binary(remote) {
        return Err(DiffSkip::Binary);
    }
    let l = String::from_utf8_lossy(local).into_owned();
    let r = String::from_utf8_lossy(remote).into_owned();
    Ok((l, r))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::DecisionRow;

    fn row(path: &str) -> DecisionRow {
        DecisionRow {
            id: DecisionRow::id_for(path),
            path: path.into(),
            task_id: "t1".into(),
            is_dir: false,
            local_size: 10,
            local_mtime: 100,
            remote_size: 20,
            remote_mtime: 200,
            created_unix: 1_700_000_000,
            resolution: None,
        }
    }

    /// 一个「一切正常、冲突仍在、等着用户裁决」的事实底座。
    fn live_facts() -> DecisionFacts {
        DecisionFacts {
            observed: true,
            local_observed: true,
            recorded_local: Sig::file(10, 100),
            recorded_remote: Sig::file(20, 200),
            resolved: false,
            task_alive: true,
            policy: CONFLICT_ASK.into(),
            local_now: Sig::file(11, 101),
            local_dirty: true,
            remote_now: Sig::file(20, 200),
            baseline: Sig::file(10, 100),
        }
    }

    #[test]
    fn sweep_keeps_a_live_unresolved_conflict() {
        assert_eq!(sweep_decision(&live_facts()), SweepVerdict::Keep);
    }

    #[test]
    fn sweep_drops_when_remote_file_gone() {
        let f = DecisionFacts {
            remote_now: Sig::MISSING,
            ..live_facts()
        };
        match sweep_decision(&f) {
            SweepVerdict::Drop(why) => assert!(why.contains("已不存在"), "{why}"),
            other => panic!("应出队，实际 {other:?}"),
        }
    }

    #[test]
    fn sweep_drops_when_task_deleted() {
        let f = DecisionFacts {
            task_alive: false,
            ..live_facts()
        };
        assert!(matches!(sweep_decision(&f), SweepVerdict::Drop(_)));
    }

    /// ★ 最常见的残留原因：用户把任务策略从 ask 改成别的。
    #[test]
    fn sweep_drops_when_policy_is_no_longer_ask() {
        for p in [
            "rename_local",
            "rename_remote",
            "replace_local",
            "replace_remote",
        ] {
            let f = DecisionFacts {
                policy: p.into(),
                ..live_facts()
            };
            match sweep_decision(&f) {
                SweepVerdict::Drop(why) => assert!(why.contains("策略已改为"), "{p}: {why}"),
                other => panic!("{p} 应出队，实际 {other:?}"),
            }
        }
    }

    /// 空策略按默认（rename_local）算 —— 同样不是 ask，该出队。
    #[test]
    fn sweep_treats_empty_policy_as_default() {
        let f = DecisionFacts {
            policy: String::new(),
            ..live_facts()
        };
        assert!(matches!(sweep_decision(&f), SweepVerdict::Drop(_)));
    }

    /// 已裁决 + 冲突已消解（远端回退到 baseline）→ 出队。
    #[test]
    fn sweep_drops_resolved_when_conflict_gone() {
        let f = DecisionFacts {
            resolved: true,
            remote_now: Sig::file(10, 100),
            local_dirty: false,
            local_now: Sig::file(10, 100),
            ..live_facts()
        };
        match sweep_decision(&f) {
            SweepVerdict::Drop(why) => assert!(why.contains("自行消解"), "{why}"),
            other => panic!("应出队，实际 {other:?}"),
        }
    }

    /// 已裁决但**仍然冲突** → 留着等引擎执行。这是「裁决还没落地」的正常态。
    #[test]
    fn sweep_keeps_resolved_while_still_conflicting() {
        let f = DecisionFacts {
            resolved: true,
            ..live_facts()
        };
        assert_eq!(sweep_decision(&f), SweepVerdict::Keep);
    }

    /// ★ 回归（真机事故面）：远端 stat 到了（`observed=true`），但**本地侧还没观测到**
    /// （daemon 刚重启 / 路径没被访问过 → 本地节点表里没有它，`local_now` 只是占位
    /// `Sig::MISSING`）。这时**绝不能**跑三向决策 —— 否则已裁决（`resolved=true`）的行
    /// 会被判成「冲突已自行消解（Noop / RefreshRemote）」清掉，用户的保留意图从未执行
    /// 就被静默吞掉了。
    #[test]
    fn sweep_keeps_resolved_rows_when_local_side_not_observed() {
        let f = DecisionFacts {
            observed: true,
            local_observed: false,
            resolved: true,
            local_now: Sig::MISSING,
            local_dirty: false,
            remote_now: Sig::file(20, 200),
            ..live_facts()
        };
        assert_eq!(
            sweep_decision(&f),
            SweepVerdict::Keep,
            "本地没被观测到 ≠ 冲突已消解：已裁决的行必须留到能真正执行/确认的那一轮"
        );

        // 反向对照：本地真的看过了（节点存在、内容是干净的）→ 才允许判「已消解」。
        let f = DecisionFacts {
            local_observed: true,
            local_now: Sig::file(10, 100),
            local_dirty: false,
            remote_now: Sig::file(10, 100),
            ..f
        };
        match sweep_decision(&f) {
            SweepVerdict::Drop(why) => assert!(why.contains("自行消解"), "{why}"),
            other => panic!("本地观测到两边一致时应出队，实际 {other:?}"),
        }
    }

    /// ★ 保守约束：未裁决的即便此刻恰好不冲突，也**不自动删** ——
    /// 单轮看不出「稳定不冲突」，宁可多留一条问用户。
    #[test]
    fn sweep_keeps_unresolved_even_if_not_conflicting_now() {
        let f = DecisionFacts {
            resolved: false,
            remote_now: Sig::file(10, 100),
            local_dirty: false,
            local_now: Sig::file(10, 100),
            ..live_facts()
        };
        assert_eq!(sweep_decision(&f), SweepVerdict::Keep);
    }

    /// ★ 事故面：没有挂载视图时（`observed=false`），**绝不能**把占位的
    /// `remote_now=MISSING` 当成「NAS 上的文件已不存在」—— 那会把整队待裁决静默删光。
    #[test]
    fn sweep_never_uses_placeholder_facts_when_nothing_observed() {
        let f = DecisionFacts {
            observed: false,
            remote_now: Sig::MISSING,
            local_now: Sig::MISSING,
            local_dirty: false,
            ..live_facts()
        };
        assert_eq!(
            sweep_decision(&f),
            SweepVerdict::Keep,
            "没挂载 ≠ NAS 文件已删：任务还在就留着，挂载回来还要执行裁决"
        );
    }

    /// 没观测到现状时，「任务确实删了」仍然要出队 —— 与「没挂载」是两回事。
    #[test]
    fn sweep_drops_when_unobserved_but_task_really_deleted() {
        let f = DecisionFacts {
            observed: false,
            remote_now: Sig::MISSING,
            task_alive: false,
            ..live_facts()
        };
        match sweep_decision(&f) {
            SweepVerdict::Drop(why) => assert!(why.contains("任务已删除"), "{why}"),
            other => panic!("任务真没了应出队，实际 {other:?}"),
        }
    }

    /// 没观测到现状时，已裁决的行也不能跑三向决策（占位值会把它误判成「已消解」）。
    #[test]
    fn sweep_does_not_run_decision_without_observation() {
        let f = DecisionFacts {
            observed: false,
            resolved: true,
            remote_now: Sig::MISSING,
            local_now: Sig::MISSING,
            local_dirty: false,
            ..live_facts()
        };
        assert_eq!(sweep_decision(&f), SweepVerdict::Keep);
    }

    #[test]
    fn sweep_batch_skips_rows_with_unknown_facts() {
        let rows = vec![row("/home/a.txt"), row("/home/b.txt"), row("/home/c.txt")];
        let out = sweep_batch(&rows, |r| {
            (r.path == "/home/a.txt").then(|| DecisionFacts {
                task_alive: false,
                ..live_facts()
            })
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, DecisionRow::id_for("/home/a.txt"));
    }

    #[test]
    fn sweep_batch_keeps_everything_when_all_fine() {
        let rows = vec![row("/home/a.txt"), row("/home/b.txt")];
        let out = sweep_batch(&rows, |_| Some(live_facts()));
        assert!(out.is_empty());
    }

    // ------------------------------------------------------------ diff

    /// ★ 方向锁定：`-` = 只有**本地**有（removed），`+` = 只有 **NAS** 有（added）。
    ///
    /// 与 `diff 本地 NAS` 的 unified 约定一致（本地是「旧」、NAS 是「新」）。
    /// 这条曾把注释/契约夹具写反过，所以这里对**内容 + 两个行号字段**全方向钉死。
    #[test]
    fn diff_marks_local_only_and_remote_only() {
        // 本地 a b c / NAS a x c：
        //   b → 只有本地有（'-'，removed）
        //   x → 只有 NAS 有（'+'，added）
        let d = text_diff("a\nb\nc\n", "a\nx\nc\n");
        assert_eq!(d.added, 1, "added = 只有 NAS 有的行数");
        assert_eq!(d.removed, 1, "removed = 只有本地有的行数");

        let del = d.lines.iter().find(|l| l.kind == '-').expect("应有 '-' 行");
        assert_eq!(del.text, "b", "'-' 行必须是**本地**内容");
        assert!(
            del.local_no > 0,
            "'-' 行必须有本地行号（实际 {}）",
            del.local_no
        );
        assert_eq!(del.remote_no, 0, "'-' 行 NAS 没有对应行");
        assert_eq!(del.local_no, 2);

        let add = d.lines.iter().find(|l| l.kind == '+').expect("应有 '+' 行");
        assert_eq!(add.text, "x", "'+' 行必须是 **NAS** 内容");
        assert!(
            add.remote_no > 0,
            "'+' 行必须有 NAS 行号（实际 {}）",
            add.remote_no
        );
        assert_eq!(add.local_no, 0, "'+' 行本地没有对应行");
        assert_eq!(add.remote_no, 2);
    }

    #[test]
    fn diff_reports_identical_as_no_changes() {
        let d = text_diff("a\nb\n", "a\nb\n");
        assert_eq!((d.added, d.removed), (0, 0));
    }

    #[test]
    fn diff_keeps_line_numbers_aligned() {
        let d = text_diff("1\n2\n3\n", "1\n9\n3\n");
        assert!(d
            .lines
            .iter()
            .any(|l| l.kind == ' ' && l.local_no == 1 && l.remote_no == 1));
        assert!(d
            .lines
            .iter()
            .any(|l| l.kind == ' ' && l.local_no == 3 && l.remote_no == 3));
    }

    /// 差异在很后面时，头部不该全带出来（上下文裁剪）。
    #[test]
    fn diff_trims_far_away_context() {
        let local: String = (1..=200).map(|i| format!("line{i}\n")).collect();
        let mut remote = local.clone();
        remote.push_str("extra\n");
        let d = text_diff(&local, &remote);
        assert_eq!(d.added, 1);
        assert!(
            d.lines.len() < 20,
            "200 行文件只差 1 行，不该回显 {} 行",
            d.lines.len()
        );
        assert!(d.lines.iter().any(|l| l.text == "extra"));
    }

    /// 变更行数超过**输出**上限 → `lines` 截断，但 `added/removed` 仍是已对比范围内的
    /// 完整统计（这是「输出截断」，与「输入截断」是两种不同的 `truncated` 来源）。
    ///
    /// 2000 行 × 2000 行 = 2001² = 4,004,001 个 DP 单元，恰好**没有**触发输入截断，
    /// 所以能验证「统计不受输出截断影响」。
    #[test]
    fn diff_truncates_huge_output_but_stats_stay_complete() {
        let a: String = (1..=2000).map(|i| format!("a{i}\n")).collect();
        let b: String = (1..=2000).map(|i| format!("b{i}\n")).collect();
        let d = text_diff(&a, &b);
        assert!(d.truncated, "2000 行变更必须标截断");
        assert!(d.lines.len() <= MAX_OUT_LINES);
        assert_eq!(
            (d.added, d.removed),
            (2000, 2000),
            "输出截断不影响统计：统计要报已对比范围内的全量"
        );
    }

    /// ★ 有界回退：行数平方爆炸的输入必须**快速返回**、**真的做对比**、
    /// 且**不得误报「逐行相同」**。
    ///
    /// 输入是 512 KiB 的 1 字符短行（26 万行/侧）—— 老实现要
    /// 26万×26万×4B ≈ 275 GB 的 DP 表，qxyncd 会直接 OOM/卡死。
    #[test]
    fn diff_large_quadratic_input_falls_back_to_bounded_lcs() {
        let a: String = std::iter::repeat("a\n").take(262_144).collect();
        let b: String = std::iter::repeat("b\n").take(262_144).collect();
        let t = std::time::Instant::now();
        let d = text_diff(&a, &b);
        let elapsed = t.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "有界回退必须快速返回，实际 {elapsed:?}"
        );
        assert!(d.truncated, "输入被截断必须显式标出");
        assert!(
            !d.lines.is_empty(),
            "不得静默返回空 diff —— 界面会把空 lines 显示成「两边内容逐行相同」"
        );
        assert!(d.lines.len() <= MAX_OUT_LINES);
        assert_eq!(
            (d.added, d.removed),
            (MAX_OUT_LINES, MAX_OUT_LINES),
            "统计只覆盖已对比的前 MAX_OUT_LINES 行（文档化的截断语义）"
        );
    }

    /// ★ 回退的统计语义：超过阈值时只对比前 `MAX_OUT_LINES` 行 —— 不假装比完了全文件。
    #[test]
    fn diff_fallback_stats_only_cover_compared_prefix() {
        let a: String = (0..3000).map(|i| format!("aa{i}\n")).collect();
        let b: String = (0..3000).map(|i| format!("bb{i}\n")).collect();
        let d = text_diff(&a, &b);
        assert!(d.truncated);
        assert_eq!(
            (d.added, d.removed),
            (MAX_OUT_LINES, MAX_OUT_LINES),
            "回退后只统计已对比的部分"
        );
    }

    /// 边界：差异区域在阈值内（1000×1000）时保持完整 LCS 质量，且不标截断。
    #[test]
    fn diff_below_threshold_keeps_full_lcs_quality() {
        let a: String = (0..1000).map(|i| format!("aa{i}\n")).collect();
        let b: String = (0..1000).map(|i| format!("bb{i}\n")).collect();
        let d = text_diff(&a, &b);
        assert_eq!((d.added, d.removed), (1000, 1000));
        assert!(!d.truncated, "1000×1000 远小于上限，不该标截断");
        assert_eq!(d.lines.len(), 2000, "1000 删 + 1000 增，全部在输出上限内");
    }

    /// 差异在很后面时，头部不该全带出来（上下文裁剪）。

    #[test]
    fn diff_handles_empty_sides() {
        let d = text_diff("", "a\nb\n");
        assert_eq!((d.added, d.removed), (2, 0));
        let d = text_diff("a\nb\n", "");
        assert_eq!((d.added, d.removed), (0, 2));
        let d = text_diff("", "");
        assert_eq!((d.added, d.removed), (0, 0));
    }

    #[test]
    fn diff_without_trailing_newline_is_fine() {
        let d = text_diff("a\nb", "a\nb");
        assert_eq!((d.added, d.removed), (0, 0));
    }

    #[test]
    fn nul_byte_means_binary() {
        assert!(looks_binary(b"abc\0def"));
        assert!(!looks_binary(b"abcdef"));
        assert!(!looks_binary("冲突内容".as_bytes()));
    }

    #[test]
    fn prepare_reports_too_large_with_numbers() {
        let big = vec![b'x'; (DIFF_MAX_BYTES + 1) as usize];
        match prepare_bytes(&big, b"small") {
            Err(DiffSkip::TooLarge { size, limit }) => {
                assert_eq!(limit, DIFF_MAX_BYTES);
                assert_eq!(size, DIFF_MAX_BYTES + 1);
            }
            other => panic!("应报超限，实际 {other:?}"),
        }
    }

    #[test]
    fn prepare_reports_binary_instead_of_fake_empty_diff() {
        match prepare_bytes(b"a\0b", b"text") {
            Err(DiffSkip::Binary) => {}
            other => panic!("应报二进制，实际 {other:?}"),
        }
    }

    /// 非法 UTF-8 走 lossy，不 panic。
    #[test]
    fn prepare_and_diff_tolerate_lossy_utf8() {
        let (l, r) = prepare_bytes(&[0xff, 0xfe], b"ok").unwrap();
        let d = text_diff(&l, &r);
        assert!(d.added + d.removed > 0);
    }
}
