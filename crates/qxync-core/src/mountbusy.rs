//! # 挂载点「谁在用」—— `daemon stop` / `umount` 之前的占用检查
//!
//! 问题：FUSE 挂载点被别的进程占着（在那儿打开了文件、或者干脆 `cd` 进去了）时，
//! `fusermount3 -u` 会回 `Device or resource busy`。旧行为是：daemon 记一条 WARN 就
//! 退出，挂载点留在内核里变成「连不上」的死挂载；`qxync daemon stop` 表面上成功，
//! 用户却看着挂载点还在、再启一次也挂不上。用户当时能看到的只是一句
//! `卸载失败: ... Device or resource busy` —— 不知道**是谁**。
//!
//! 所以停止/卸载之前先按 `/proc` 扫一遍，把「谁在用」如实列出来。判据（缺一不可，
//! 都是内核视角、不调用 `lsof`/`fuser` 这些外部命令）：
//!
//! | 占用形式 | 怎么发现 | 为什么这也算 busy |
//! | --- | --- | --- |
//! | 工作目录 / 根目录在挂载点里 | `/proc/<pid>/cwd`、`root` 的 readlink | 目录被引用，umount 回 EBUSY |
//! | 打开了挂载点里的文件 | `/proc/<pid>/fd/*` 的 readlink | 打开的 inode 被引用 |
//! | mmap 了挂载点里的文件（fd 已经关了也算） | `/proc/<pid>/maps` 的 pathname | 映射还在，内核仍算 busy |
//! | 挂载点**下面**还挂着文件系统 | `/proc/self/mountinfo` | 子挂载没卸，父挂载卸不掉 |
//!
//! ## 刻意保守、如实告知
//!
//! * **只报读得到的进程**：别的用户的 `/proc/<pid>/fd` 普通用户读不到（EACCES），
//!   这类进程直接跳过 —— 不猜、不编。root 跑时能看到全部。
//! * 判定用**路径包含**（`p == mp` 或 `p.starts_with(mp)`），不按 `st_dev`：
//!   拿普通目录做测试时 `st_dev` 会把同分区里所有进程都算进来；FUSE 挂载点本身
//!   路径是唯一的，路径包含已经足够，而且不会误伤。
//! * 已删除的文件/目录在 readlink 里带 `" (deleted)"` 后缀，比较前会剥掉。
//! * 同一个进程对同一个路径既开 fd 又 mmap，只报一次（去重，见 [`MountUser`]）。
//! * 每个挂载点最多列 [`MAX_LISTED_PER_MOUNT`] 条，剩下的只报个数 ——
//!   否则 `updatedb` / 索引器打开几千个文件时输出会淹没真正有用的信息。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// 每个挂载点最多列出的占用条数（超出的只报个数）。
pub const MAX_LISTED_PER_MOUNT: usize = 12;

/// 一条「进程怎么占着挂载点」的记录。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MountUser {
    pub pid: u32,
    /// 进程名（`/proc/<pid>/comm`，内核最多存 15 个字符；读不到时是 `?`）。
    pub comm: String,
    pub kind: UserKind,
    /// 进程里的那个路径：fd 指向的文件 / 工作目录 / mmap 的文件。
    /// 可能是 `… (deleted)`，也可能（root 目录）是 `/`。
    pub path: String,
}

/// 占用形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UserKind {
    /// 工作目录在挂载点里（`cd` 进去了）。
    Cwd,
    /// 根目录在挂载点里（chroot 进去的，极罕见）。
    Root,
    /// 打开了挂载点里的文件/目录。
    Fd,
    /// mmap 了挂载点里的文件。
    Mmap,
}

impl UserKind {
    /// 给用户看的短标签。
    pub fn label(self) -> &'static str {
        match self {
            UserKind::Cwd => "工作目录",
            UserKind::Root => "根目录  ",
            UserKind::Fd => "打开文件",
            UserKind::Mmap => "内存映射",
        }
    }
}

/// 一个挂载点的占用情况。
#[derive(Debug, Clone, Default)]
pub struct MountBusy {
    /// 挂载点（尽量 canonicalize；挂载点不存在/连不上时原样保留）。
    pub mountpoint: PathBuf,
    /// 占用进程（去重后；最多 `MAX_LISTED_PER_MOUNT` 条）。
    pub users: Vec<MountUser>,
    /// 被截掉没列出来的条数。
    pub omitted: usize,
    /// 挂载点**下面**的子挂载（不卸它们，父挂载卸不掉）。
    pub nested_mounts: Vec<PathBuf>,
}

impl MountBusy {
    /// 有没有人占着（进程或子挂载，任一都算）。
    pub fn is_in_use(&self) -> bool {
        !self.users.is_empty() || self.omitted > 0 || !self.nested_mounts.is_empty()
    }
}

/// 扫一个挂载点（自动排除**本进程**）。
pub fn scan(mountpoint: &Path) -> MountBusy {
    scan_excluding(mountpoint, &[])
}

/// 扫一个挂载点，并额外排除 `skip_pids` 里的进程（例如 daemon 自己）。
///
/// 为什么必须能排除：调用方可能是「检查后马上就要退出」的进程 —— 如果把
/// `qxync daemon stop` 自己算成占用者，用户会被自己的命令永久拒绝。
pub fn scan_excluding(mountpoint: &Path, skip_pids: &[u32]) -> MountBusy {
    let mp = normalize(mountpoint);
    let mut out = MountBusy {
        mountpoint: mp.clone(),
        ..Default::default()
    };
    // 空路径 / 根路径不能扫：「任何路径都以空路径开头」会把整机所有进程都算进来。
    if mp.as_os_str().is_empty() || mp == Path::new("/") {
        return out;
    }

    let me = std::process::id();
    let mut seen: BTreeSet<(u32, String)> = BTreeSet::new();
    let mut all: Vec<MountUser> = Vec::new();

    for (pid, dir) in proc_entries() {
        if pid == me || skip_pids.contains(&pid) {
            continue;
        }
        let comm = read_comm(&dir);
        let mut push = |kind: UserKind, path: PathBuf| {
            // 同一进程同一路径只报一次（fd + mmap 常见；cwd 与目录 fd 也是同一个）。
            let key = (pid, path.to_string_lossy().into_owned());
            if seen.insert(key) {
                all.push(MountUser {
                    pid,
                    comm: comm.clone(),
                    kind,
                    path: path.to_string_lossy().into_owned(),
                });
            }
        };

        // ① cwd / root
        for (kind, name) in [(UserKind::Cwd, "cwd"), (UserKind::Root, "root")] {
            if let Ok(p) = std::fs::read_link(dir.join(name)) {
                if in_mount(&p, &mp) {
                    push(kind, p);
                }
            }
        }
        // ② 打开的文件 / 目录
        if let Ok(rd) = std::fs::read_dir(dir.join("fd")) {
            for e in rd.flatten() {
                if let Ok(p) = std::fs::read_link(e.path()) {
                    if in_mount(&p, &mp) {
                        push(UserKind::Fd, p);
                    }
                }
            }
        }
        // ③ mmap（fd 已关掉的也在这里）
        if let Ok(txt) = std::fs::read_to_string(dir.join("maps")) {
            for line in txt.lines() {
                if let Some(raw) = map_path(line) {
                    let p = PathBuf::from(unescape(raw));
                    if in_mount(&p, &mp) {
                        push(UserKind::Mmap, p);
                    }
                }
            }
        }
    }

    all.sort();
    out.omitted = all.len().saturating_sub(MAX_LISTED_PER_MOUNT);
    all.truncate(MAX_LISTED_PER_MOUNT);
    out.users = all;
    out.nested_mounts = parse_nested_mounts(
        &std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default(),
        &mp,
    );
    out
}

/// 批量扫：只返回**确实被占用**的挂载点（干净的挂载点不返回）。
///
/// `skip_pids` 见 [`scan_excluding`]；调用方一般传 `[daemon_pid]`（daemon 自己
/// 不算占用者，但它在别的 pid 上时也要排除）。
pub fn scan_many<'a, I>(mountpoints: I, skip_pids: &[u32]) -> Vec<MountBusy>
where
    I: IntoIterator<Item = &'a Path>,
{
    let mut out: Vec<MountBusy> = mountpoints
        .into_iter()
        .filter(|mp| !mp.as_os_str().is_empty())
        .map(|mp| scan_excluding(mp, skip_pids))
        .filter(|b| b.is_in_use())
        .collect();
    out.sort_by(|a, b| a.mountpoint.cmp(&b.mountpoint));
    out
}

/// 把 [`scan_many`] 的结果拼成给人看的多行文本（CLI 报错 / GUI 弹窗 / 日志共用）。
pub fn report(busy: &[MountBusy]) -> String {
    let pids: BTreeSet<u32> = busy
        .iter()
        .flat_map(|b| b.users.iter().map(|u| u.pid))
        .collect();
    let nested: usize = busy.iter().map(|b| b.nested_mounts.len()).sum();
    let mut out = format!(
        "{} 个挂载点正在被使用（{} 个进程{}）：",
        busy.len(),
        pids.len(),
        if nested > 0 {
            format!("、{nested} 个子挂载")
        } else {
            String::new()
        }
    );
    for b in busy {
        out.push_str(&format!("\n挂载点 {}：", b.mountpoint.display()));
        for u in &b.users {
            out.push_str(&format!(
                "\n  pid {:<7} {:<16} {} {}",
                u.pid,
                u.comm,
                u.kind.label(),
                u.path
            ));
        }
        if b.omitted > 0 {
            out.push_str(&format!(
                "\n  … 另有 {} 条占用未列出（同一进程可能打开了很多文件）",
                b.omitted
            ));
        }
        for n in &b.nested_mounts {
            out.push_str(&format!(
                "\n  子挂载 {}（要卸父挂载点，得先卸它）",
                n.display()
            ));
        }
    }
    out
}

// ---------------------------------------------------------------- 内部实现

/// 挂载点尽量规范化（解析符号链接）；连不上 / 不存在时原样返回。
fn normalize(mp: &Path) -> PathBuf {
    std::fs::canonicalize(mp).unwrap_or_else(|_| mp.to_path_buf())
}

/// `/proc` 下的数字进程目录。
fn proc_entries() -> Vec<(u32, PathBuf)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        out.push((pid, e.path()));
    }
    out
}

fn read_comm(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "?".into())
}

/// `p` 是不是就在挂载点 `mp` 里（含挂载点本身）。
///
/// 用字节比较 + 组件边界，避免 `/a/bc` 被 `/a/b` 前缀命中；readlink 的
/// `" (deleted)"` 后缀先剥掉。刻意不用 `st_dev`（同分区普通目录会误判，见模块文档）。
fn in_mount(p: &Path, mp: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let mut pb = p.as_os_str().as_bytes();
    if let Some(rest) = strip_deleted(pb) {
        pb = rest;
    }
    let mb = mp.as_os_str().as_bytes();
    if pb == mb {
        return true;
    }
    pb.len() > mb.len() && pb.starts_with(mb) && (mb.ends_with(b"/") || pb[mb.len()] == b'/')
}

fn strip_deleted(mut b: &[u8]) -> Option<&[u8]> {
    const SUFFIX: &[u8] = b" (deleted)";
    if b.ends_with(SUFFIX) {
        b = &b[..b.len() - SUFFIX.len()];
        Some(b)
    } else {
        None
    }
}

/// `/proc/<pid>/maps` 一行里的 pathname（第 6 段起，允许带空格）。
fn map_path(line: &str) -> Option<&str> {
    let mut it = line.splitn(6, ' ');
    for _ in 0..5 {
        it.next()?;
    }
    let path = it.next()?;
    path.starts_with('/').then_some(path)
}

/// 还原 `/proc` 里的转义：`\040`(空格) 这类八进制、`\n`、`\\`。
fn unescape(raw: &str) -> String {
    let b = raw.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            match b[i + 1] {
                b'n' => {
                    out.push(b'\n');
                    i += 2;
                    continue;
                }
                b't' => {
                    out.push(b'\t');
                    i += 2;
                    continue;
                }
                b'\\' => {
                    out.push(b'\\');
                    i += 2;
                    continue;
                }
                _ => {}
            }
            if i + 3 < b.len() {
                let oct = &b[i + 1..i + 4];
                if oct.iter().all(|c| (b'0'..=b'7').contains(c)) {
                    out.push((oct[0] - b'0') * 64 + (oct[1] - b'0') * 8 + (oct[2] - b'0'));
                    i += 4;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 从 `/proc/self/mountinfo` 里挑出挂在 `mp` **下面**的子挂载。
fn parse_nested_mounts(text: &str, mp: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut it = line.split(' ');
        // 格式：id parent major:minor root mountpoint optional... - fstype source superopts
        let (Some(_id), Some(_parent), Some(_majmin), Some(_root), Some(field)) =
            (it.next(), it.next(), it.next(), it.next(), it.next())
        else {
            continue;
        };
        if field.is_empty() {
            continue;
        }
        let p = PathBuf::from(unescape(field));
        if p != *mp && p.starts_with(mp) {
            out.push(p);
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "qxync-busy-{}-{}-{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    /// 子进程兜底清理（测试 panic 也不留 `sleep`）。
    struct Proc(Child);

    impl Drop for Proc {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn wait_users(dir: &Path) -> Vec<MountUser> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let users = scan(dir).users;
            if !users.is_empty() || Instant::now() >= deadline {
                return users;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn cwd_inside_mount_is_detected() {
        let dir = tmp("cwd");
        let child = Proc(
            Command::new("sh")
                .arg("-c")
                .arg("sleep 30")
                .current_dir(&dir)
                .spawn()
                .unwrap(),
        );
        let users = wait_users(&dir);
        assert!(
            users.iter().any(|u| u.pid == child.0.id()),
            "子进程的 cwd 在 {} 里，应该被报出来：{users:?}",
            dir.display()
        );
        assert!(users.iter().any(|u| u.kind == UserKind::Cwd));
    }

    #[test]
    fn open_fd_inside_mount_is_detected() {
        let dir = tmp("fd");
        let file = dir.join("打开的文件.bin");
        std::fs::write(&file, b"x").unwrap();
        let _child = Proc(
            Command::new("sh")
                .arg("-c")
                .arg(format!("exec 3<{}; sleep 30", file.display()))
                .spawn()
                .unwrap(),
        );
        let users = wait_users(&dir);
        assert!(
            users
                .iter()
                .any(|u| u.path.contains("打开的文件.bin") && u.kind == UserKind::Fd),
            "应该报出打开的文件：{users:?}"
        );
    }

    #[test]
    fn parent_directory_is_not_reported() {
        let dir = tmp("parent");
        // cwd 是父目录（同一分区、st_dev 相同）—— 用 st_dev 判定的实现会误报，
        // 这里钉住「路径包含」语义：它不属于这个挂载点。
        let _child = Proc(
            Command::new("sh")
                .arg("-c")
                .arg("sleep 30")
                .current_dir(dir.parent().unwrap())
                .spawn()
                .unwrap(),
        );
        // 等一会儿让进程真的起来，再断言「扫不到任何指向 dir 的占用」。
        std::thread::sleep(Duration::from_millis(200));
        let users = scan(&dir).users;
        assert!(users.is_empty(), "父目录不属于挂载点，不该报：{users:?}");
    }

    #[test]
    fn deleted_paths_are_compared_without_suffix() {
        assert!(in_mount(
            Path::new("/mnt/x/a (deleted)"),
            Path::new("/mnt/x")
        ));
        assert!(!in_mount(
            Path::new("/mnt/xy/a (deleted)"),
            Path::new("/mnt/x")
        ));
        assert!(in_mount(Path::new("/mnt/x"), Path::new("/mnt/x")));
        assert!(!in_mount(Path::new("/mnt/xy"), Path::new("/mnt/x")));
    }

    #[test]
    fn maps_path_parsing_and_unescape() {
        assert_eq!(
            map_path("7f00-7f01 r--p 00000000 00:1d 123 /path/a b"),
            Some("/path/a b")
        );
        assert_eq!(map_path("7f00-7f01 r--p 00000000 00:1d 123"), None);
        assert_eq!(map_path("7f00-7f01 r--p 00000000 00:1d 123 [heap]"), None);
        assert_eq!(unescape("a\\040b"), "a b");
        assert_eq!(unescape("a\\134b"), "a\\b");
        assert_eq!(unescape("plain"), "plain");
    }

    #[test]
    fn nested_mounts_are_parsed() {
        let text = "\
36 35 98:0 /mnt1 /mnt/x rw,noatime master:1 - ext3 /dev/root rw,errors=continue
37 36 0:42 / /mnt/x/scratch rw,nosuid - tmpfs tmpfs rw
38 36 0:43 / /mnt/xy rw,nosuid - tmpfs tmpfs rw
39 36 0:44 / /mnt/x/with\\040space rw,nosuid - tmpfs tmpfs rw
";
        let got = parse_nested_mounts(text, Path::new("/mnt/x"));
        assert_eq!(
            got,
            vec![
                PathBuf::from("/mnt/x/scratch"),
                PathBuf::from("/mnt/x/with space"),
            ]
        );
    }

    #[test]
    fn scan_many_reports_only_busy_mounts() {
        let busy_dir = tmp("many-busy");
        let clean_dir = tmp("many-clean");
        let _child = Proc(
            Command::new("sh")
                .arg("-c")
                .arg("sleep 30")
                .current_dir(&busy_dir)
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let got = scan_many([busy_dir.as_path(), clean_dir.as_path()], &[]);
            if got.len() == 1 && got[0].mountpoint == busy_dir {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "应该只报被占用的 {}，实际：{got:?}",
                busy_dir.display()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn report_lists_processes_and_counts() {
        let busy = MountBusy {
            mountpoint: PathBuf::from("/mnt/x"),
            users: vec![
                MountUser {
                    pid: 1234,
                    comm: "wps".into(),
                    kind: UserKind::Fd,
                    path: "/mnt/x/方案.docx".into(),
                },
                MountUser {
                    pid: 5678,
                    comm: "bash".into(),
                    kind: UserKind::Cwd,
                    path: "/mnt/x/sub".into(),
                },
            ],
            omitted: 0,
            nested_mounts: vec![PathBuf::from("/mnt/x/scratch")],
        };
        let text = report(std::slice::from_ref(&busy));
        assert!(text.contains("/mnt/x"), "{text}");
        assert!(text.contains("pid 1234"), "{text}");
        assert!(text.contains("wps"), "{text}");
        assert!(text.contains("打开文件"), "{text}");
        assert!(text.contains("子挂载 /mnt/x/scratch"), "{text}");
        assert!(text.contains("2 个进程"), "{text}");
    }

    #[test]
    fn empty_and_root_mountpoints_are_ignored() {
        assert!(!scan(Path::new("")).is_in_use());
        // 根路径不扫（否则整机所有进程都算「在根挂载点里」）。
        assert!(!scan(Path::new("/")).is_in_use());
    }
}
