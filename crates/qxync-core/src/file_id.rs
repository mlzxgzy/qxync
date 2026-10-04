//! ★ M15/T9：稳定文件身份 `file_id`（**纯函数、无 IO、好测**）。
//!
//! ## 为什么需要它
//!
//! 服务端 `get_list` / `stat` 返回的 `DirEntry` 只有
//! `filename / filesize / epochmt / isfolder / have_child` —— **没有任何 id**
//! （见 `qxync-core/src/model.rs`）。而 M2c 之后所有状态表
//! （`baseline` / `uploads` / `deletes` / `pins` / `decisions`）都以 **path 为主键**，
//! 内存索引 `Inner.by_remote: HashMap<String, INodeNo>` 也是按远端路径串。
//! 于是「远端把 `a.txt` 改名成 `b.txt`」这件事在本地看是
//! **「`a.txt` 消失 + `b.txt` 凭空出现」**，两者毫无关系 —— `decide` 只能把它
//! 判成「删除 + 新增」，于是凭空生成一个冲突副本、白白多占一份空间。
//!
//! `file_id` 就是给这一层补一个**跨改名/移动稳定的身份**：文件内容不变、
//! mtime 不变时，它是同一个值。对账时用它把「远端删除」和「远端新增」配成
//! 一对 rename（见 [`crate::sync::pair_renames`]），而不是两个互不相干的事件。
//!
//! ## 算法（v1）
//!
//! `file_id = xxh64(输入, SEED_A) || xxh64(输入, SEED_B)`（两个不同 seed 拼 16 字节）。
//! 选 xxhash64 而不是 sha256 的理由与 T3 的 chunk 校验和一致：快一个数量级，
//! 这里要挡的是「同名同大小不同内容」这种意外，不是恶意碰撞（能改 `sync.db`
//! 的人不需要伪造 file_id）。
//!
//! 输入串按「能不能跨改名」分成两档：
//!
//! | 情形 | 输入 | 跨改名稳定 |
//! |---|---|---|
//! | 文件，`mtime > 0` | `f \0 <mtime> \0 <size>` | ✅ |
//! | 文件，`mtime <= 0`（退化） | `f \0 <规范化名字> \0 <规范化父目录> \0 0` | ❌ |
//! | 目录 | `d \0 <规范化名字> \0 <规范化父目录>` | ❌ |
//!
//! **★ 为什么名字/目录不进主算法**（这是 T9 与「把规范化名字一起哈希」那一版
//! 设计的**刻意分歧**，理由是改名场景本身）：
//!
//! 一旦把 `filename` 混进 id，`a.txt → b.txt` 改名后算出的 id 就变了，
//! 配对**永远配不上**，T9 的核心价值（不产生冲突副本）直接归零。而
//! `mtime + size` 在「纯改名/移动」时是不变的 —— 这正是我们要的稳定性来源。
//! 名字只在**元数据不可用**（`mtime <= 0`）时作为兜底，保证退化路径下
//! 仍然有一个确定、非全零、可复算的值。
//!
//! 代价要写清楚：`(mtime, size)` 完全相同的两个文件会拿到**同一个 id**。
//! 这是**有意接受**的 —— 配对层（[`crate::sync::pair_renames`]）用
//! 「歧义就退回旧行为」兜住（宁可多冲突，绝不错配），而缓存认领层
//! （`qxync-fuse` 的 `cache_file_for`）还有 `size`/`mtime`/长度/逐区间校验和
//! 四道更严的关卡。
//!
//! ## 不变量（`tests` 里有断言守着）
//!
//! 1. 同一输入永远同一输出（纯函数、可复算 —— 跨进程重算必须一致）。
//! 2. `size` 或 `mtime` 变了 → id 变。
//! 3. 目录与同名文件**不撞**（kind 前缀不同）。
//! 4. **绝不返回全零** —— 全零是「没有身份」的哨兵
//!    （[`ZERO_FILE_ID`]），返回一个全零等于谎称「这是个还没有身份的新文件」。
//!    万一哈希真的撞上全零（概率 2⁻¹²⁸），把最后一字节钉成 1 —— 那是一个
//!    「几乎不可能撞上但确定不等于零」的值。

use crate::model::DirEntry;

/// 本地持久化的文件身份（16 字节）。服务端没有 id，本地自建。
pub type FileId = [u8; 16];

/// 「没有身份」的哨兵。与任何真实 id 都不相等（见模块头不变量 4）。
pub const ZERO_FILE_ID: FileId = [0u8; 16];

/// 两个 64 位半体的 seed。取两个不同的固定常量是为了拿满 128 位，
/// 且**必须固定** —— id 要能跨进程/跨版本复算。
const SEED_A: u64 = 0x9E37_79B9_7F4A_7C15;
const SEED_B: u64 = 0xC2B2_AE3D_27D4_EB4F;

/// ★ T3 用的 xxhash64（seed = 0），下沉到 core 供 T9 复用。
///
/// 为什么下沉：file_id 与 chunk 校验和用**同一个**散列，
/// 就只需要在两个 crate 里各依赖一次 xxhash-rust，算法演进时不会走岔。
/// seed 保持 0 —— T3 已写进 `.qxsum` 的校验和**不能变**。
pub fn xxh64(bytes: &[u8]) -> u64 {
    xxhash_rust::xxh64::xxh64(bytes, 0)
}

/// 规范化一个路径分量：去首尾 ASCII 空白 + 去掉尾部的 `/`。
///
/// 只做这两个**无损于身份**的清理：QTS 有时会把 `filename` 带上尾随空格、
/// 本地拼路径时又可能多一个 `/`，不规范化就会算出两个 id。
/// **刻意不做大小写折叠、不做 Unicode 归一化**：那会真的把两个不同文件
/// 合并成同一个身份（Linux 上 `A` 和 `a` 是两个文件）。
fn normalize_component(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_ascii_whitespace())
        .trim_end_matches('/')
}

/// 算一个路径分量的 `file_id`（纯函数）。
///
/// * `is_dir` —— 目录与文件走不同前缀，保证同名不撞。
/// * `size` / `mtime` —— 远端给的字节数 / epoch 秒（本地与远端一致时才可比）。
/// * `name` / `parent` —— 分量名与父目录，**只在退化路径参与**（见模块头）。
#[allow(clippy::too_many_arguments)]
pub fn compute_file_id(is_dir: bool, size: u64, mtime: i64, name: &str, parent: &str) -> FileId {
    let mut input = String::with_capacity(96);
    let name = normalize_component(name);
    let parent = normalize_component(parent);
    if is_dir {
        // 目录：size 恒无意义、mtime 会被「增删子项」改动 → 只能靠名字+父目录。
        input.push_str("d\0");
        input.push_str(name);
        input.push('\0');
        input.push_str(parent);
    } else if mtime > 0 {
        // 主路径：纯内容身份，跨改名/跨目录移动都不变。
        input.push_str("f\0");
        push_num(&mut input, mtime as u64);
        input.push('\0');
        push_num(&mut input, size);
    } else {
        // 退化：远端没给 mtime（epochmt = 0）。只能退回「名字 + 父目录」，
        // 这种 id 改名后会变 → 配不上 rename，退回旧的增删判定（保守）。
        input.push_str("f\0");
        input.push_str(name);
        input.push('\0');
        input.push_str(parent);
        input.push_str("\0d");
        push_num(&mut input, size);
    }
    let a = xxhash_rust::xxh64::xxh64(input.as_bytes(), SEED_A);
    let b = xxhash_rust::xxh64::xxh64(input.as_bytes(), SEED_B);
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&a.to_le_bytes());
    id[8..].copy_from_slice(&b.to_le_bytes());
    if id == ZERO_FILE_ID {
        // 不变量 4：绝不返回全零。概率 2⁻¹²⁸，但要确定性地兜住。
        id[15] = 1;
    }
    id
}

/// 十进制追加（不用 `format!`：这条路径每个节点建的时候都要走）。
fn push_num(out: &mut String, v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let mut n = v;
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.push_str(std::str::from_utf8(&buf[i..]).unwrap_or("0"));
}

/// 由 `DirEntry` + 父目录算 `file_id`（对账侧、节点建表侧共用一个入口）。
pub fn file_id_for_entry(parent: &str, e: &DirEntry) -> FileId {
    compute_file_id(e.isfolder, e.filesize, e.epochmt, &e.filename, parent)
}

/// `file_id` 的短十六进制串（日志/调试用；不是身份的权威表示）。
pub fn file_id_hex(id: &FileId) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "/home/docs";

    #[test]
    fn same_input_gives_same_id() {
        let a = compute_file_id(false, 1024, 1_700_000_000, "a.txt", P);
        let b = compute_file_id(false, 1024, 1_700_000_000, "a.txt", P);
        assert_eq!(a, b, "纯函数：同输入必须同输出（要能跨进程复算）");
        assert_ne!(a, ZERO_FILE_ID);
    }

    #[test]
    fn changing_size_or_mtime_changes_id() {
        let base = compute_file_id(false, 1024, 1_700_000_000, "a.txt", P);
        assert_ne!(
            base,
            compute_file_id(false, 1025, 1_700_000_000, "a.txt", P),
            "改了 size 必须是另一个身份"
        );
        assert_ne!(
            base,
            compute_file_id(false, 1024, 1_700_000_001, "a.txt", P),
            "改了 mtime 必须是另一个身份"
        );
    }

    #[test]
    fn rename_keeps_id_but_directory_name_matters() {
        // ★ T9 的核心不变量：纯改名（内容与 mtime 不变）身份不变 —— 配对靠的就是它。
        let old = compute_file_id(false, 1024, 1_700_000_000, "a.txt", P);
        let new = compute_file_id(false, 1024, 1_700_000_000, "b.txt", P);
        assert_eq!(
            old, new,
            "跨改名必须保持同一身份，否则 rename 配对永远配不上"
        );
        // 换目录（移动）同样不变
        let moved = compute_file_id(false, 1024, 1_700_000_000, "a.txt", "/home/other");
        assert_eq!(old, moved, "跨目录移动也必须保持身份");
    }

    #[test]
    fn dir_never_collides_with_same_named_file() {
        let d = compute_file_id(true, 0, 1_700_000_000, "a.txt", P);
        let f = compute_file_id(false, 0, 1_700_000_000, "a.txt", P);
        assert_ne!(d, f, "同名目录与文件不能撞");
        assert_ne!(d, ZERO_FILE_ID);
        // 目录的 id 只由名字+父目录决定（size/mtime 无意义）
        assert_eq!(d, compute_file_id(true, 999, 1, "a.txt", P));
        assert_ne!(d, compute_file_id(true, 0, 0, "a.txt", "/home/other"));
    }

    #[test]
    fn never_returns_zero_even_when_metadata_missing() {
        // 退化路径（mtime <= 0）也必须有确定、非全零的值
        let a = compute_file_id(false, 0, 0, "weird name.txt", P);
        let b = compute_file_id(false, 0, 0, "weird name.txt", P);
        assert_eq!(a, b);
        assert_ne!(a, ZERO_FILE_ID);
        assert_ne!(a, compute_file_id(false, 0, 0, "other.txt", P));
    }

    #[test]
    fn trailing_space_and_slash_are_normalized_away() {
        assert_eq!(
            compute_file_id(false, 10, 5, "a.txt", "/home/docs/"),
            compute_file_id(false, 10, 5, " a.txt ", "/home/docs"),
            "QTS 的尾随空格/多余斜杠不该造出第二个身份"
        );
        assert_ne!(
            compute_file_id(false, 0, 0, "a.txt", P),
            compute_file_id(false, 0, 0, "A.txt", P),
            "大小写折叠会把两个真实文件合并 → 刻意不做"
        );
    }

    #[test]
    fn entry_helper_matches_manual_call() {
        let e = DirEntry {
            filename: "a.txt".into(),
            filesize: 7,
            isfolder: false,
            epochmt: 99,
            ..DirEntry::local("a.txt", false, 7, 99)
        };
        assert_eq!(
            file_id_for_entry(P, &e),
            compute_file_id(false, 7, 99, "a.txt", P)
        );
    }

    #[test]
    fn hex_round_trips_to_32_chars() {
        let id = compute_file_id(false, 1, 1, "x", "/");
        assert_eq!(file_id_hex(&id).len(), 32);
    }
}
