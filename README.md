# qxync —— Qsync for Linux（带 on-demand 按需同步）

基于对 QNAP **Qsync for Windows v6.1.0.0831** 的静态逆向报告（[`report/`](report/)）实现的第三方 Linux 客户端。
目标：**on-demand 按需同步**（占位符 + 按需水合），Rust + FUSE，后续 Tauri GUI。

> ⚠️ 仅用于**你自己拥有或已获授权**的 QNAP NAS。不分发 QNAP 二进制，不绕过授权。

## 现状（2026-09-30）

| 里程碑 | 状态 |
|---|---|
| **M0.1 协议定论**（登录 / 列举 / stat / 下载 / 上传全部对真机跑通） | ✅ 完成 |
| **M0 代码**（`core` + `client` + `cli`，`qsync login/ls/stat/get/put/mkdir`） | ✅ 完成 |
| `qxync-proto-test`（真机集成测试） | ✅ 完成（`#[ignore]` 手动跑，5/5 通过） |
| **M1 只读 FUSE + on-demand 整文件水合** | ✅ **真机挂载验收通过**（`fuse-matrix.sh` 快测 16/16；`--big` 含 128 MiB 水合与并发去重 **20/20**） |
| **M1.5 daemon（`qxyncd`）+ 本地 IPC + CLI 完善 + 滚动日志** | ✅ **真机验收通过**（IPC 端到端测试 + 16/16 FUSE 矩阵在 daemon 持有挂载下复跑） |
| **M2a 区间水合（128 KiB）** | ✅ **已实现并验收**：`head -c 100 big.bin` 只下载 1 个 128 KiB 区间（`chunks=1/1024`） |
| **M2b 写路径**（FUSE 写操作 + 上传队列 + dirty 标记崩溃恢复） | ✅ **已实现并验收**（矩阵 30/30，其中写路径 10 项） |
| **M2c 变更发现**（三游标轮询 + baseline 三向对账 + 冲突副本 + 删除保护） | ✅ **已实现并真机验收**（矩阵 46/46，其中 M2c 16 项） |
| **M3 脱水**（安全检查链 + 先 `inval_inode` 再清内容 + 闲置/限额 LRU + `--cache-mode`） | ✅ **已实现并真机验收**（矩阵 **67/67**，其中 M3 21 项） |
| M4 GUI / M5 增量 delta + SQLite 元数据 | ⏳ 下一步 |

真机验证对象：`TS-464C` / `QTS 5.2.9` / Qsync QPKG `5.0.0.7`（build `20260723`）。

M1 实现说明（`crates/qxync-fuse`）：
**元数据不走数据面**（`ls -l` 直接答 NAS 元数据，真实大小、零下载）；`read()` 首次触发**整文件水合**到
`~/.local/share/qsync/cache`（single-flight 去重 + 可配置超时，默认 60s）；读不满即 `EIO`（铁则 1）；
`user.qsync.*` xattr 可观测（`placeholder`/`hydrated`）；mount 参数 `ro,default_permissions,noatime`。
**M2a 已把「整文件水合」换成 128 KiB 区间水合**：缓存是「apparent size = 文件大小」的稀疏文件，
只把读到的区间 `pwrite` 进去；`user.qsync.state` 会显示 `placeholder`/`partial`/`hydrated`，
`user.qsync.chunks` 显示 `已就绪/总数`。缓存文件名用**远端路径的稳定哈希**（不能用 ino，
否则两次挂载里同一个 ino 可能对应不同文件 → 读到错的缓存）。

**M3 让磁盘占用可控**（`crates/qxync-core/src/dehydrate.rs` + `qxync-fuse`/`qxyncd`）：
脱水前先过**完整安全检查链**（pin=pinned/excluded、未上传改动/队列在途、打开的 fd、
被 mmap（扫 `/proc/*/maps`）、正在水合、刚访问过），再按**铁则 2** 执行
`inval_inode(0,0)` → 清缓存内容 → 更新占位符状态；`inval_inode` 失败就**什么都不清**。
默认**不**自动脱水：`QSYNC_DEHYDRATE_IDLE=600`（闲置）/ `QSYNC_CACHE_LIMIT=2G|25%`（限额，LRU）
或手动 `qsync dehydrate --path | --all | --cache-limit`；`--cache-mode direct` 用
`FOPEN_DIRECT_IO` 绕过 page cache（脱水天然安全，代价是没 readahead、mmap 不可用）。

**M2c 让「另一台设备改了 NAS」能被发现**（`crates/qxync-daemon/src/sync.rs`）：守护进程每 30s
（`QSYNC_POLL_INTERVAL` 可调）跑一轮「三游标 + baseline 对账」——
`qbox_get_max_log` / `qbox_get_sync_log` 拉事件（`lower` 是闭区间、无事件时 `status:-17` 不是错误），
再按「已知目录列举 + baseline 差集」兜底。远端改动 → 刷新元数据并**失效本地缓存**（下次读按需水合新内容）；
双方都改 → **冲突副本**（远端占原名，本地内容存 `xxx (conflicted copy from <设备> <日期>).txt` 并上传）；
远端批量删除 → **熔断**（`qsync sync --force-deletes` 才放行）；本地批量删除也有滑动窗口熔断。

## 目录结构

```
crates/
├── qxync-core/        共享类型：状态码、错误、URL 编码规则、数据模型、配置布局（零运行时依赖）
├── qxync-client/      NAS HTTP API 封装（登录 / 元数据 / 上传下载）
├── qxync-fuse/        FUSE 只读 + on-demand 水合（M1：Filesystem 实现 + 挂载参数）
├── qxync-daemon/      二进制 `qxyncd`（M1.5：常驻进程 + unix socket JSON IPC + 持有 FUSE；
│                       M2c：`sync.rs` 三游标轮询 + baseline 对账 + 冲突/删除保护）
├── qxync-cli/         二进制 `qsync`（login/status/ls/stat/get/put/mkdir/mount/umount/pin/state/daemon）
└── qxync-proto-test/  真机集成测试（5 个协议测试 + 1 个 IPC 端到端，均 #[ignore] 手动跑）
xtask/tests/
└── fuse-matrix.sh     ★ M1 验收矩阵（挂载 → 16 项检查 → 卸载；--big 追加 128 MiB + 并发去重）
docs/
├── 开发规划.md             第一版（MVP）规划
├── M1.5-设计.md           daemon/IPC 契约、生命周期、pin 语义、验收标准
├── M2b-写路径.md          ★ 写路径：真机写接口契约、read-modify-write 铁则、上传队列、已知限制
├── M2c-变更发现.md        ★ 变更发现：三游标/事件契约、三向决策表、冲突副本、删除保护、已知限制
├── M3-脱水.md             ★ 脱水：安全检查链、inval_inode 顺序铁则、闲置/限额、cache-mode
├── 执行方案-M0M1.md        ★ 真机验证后的修正版：实测事实 + 修正项 + 执行顺序 + 风险门
└── 测试环境.local.md       测试 NAS 与账号（已 gitignore，禁止提交）
report/                 逆向报告 + probe 工具（qs_probe.py / qs_fixture.py）
```

依赖方向（只允许向下）：`cli → core`（+ 经 IPC 访问 daemon）；`daemon → fuse/client → core`。

**进程模型（M1.5 起）**：`qxyncd` 是唯一持有 FUSE 与 NAS 会话的进程；`qsync` 默认**自动路由**——
socket 可连就走 IPC（`--via-daemon` 强制、`--direct` 跳过），所以 `qsync ls /home/x` 对用户是无感的。

## 构建与运行

```bash
cargo build --workspace

# 首次登录（凭据写入 ~/.config/qsync/credentials.json，权限 0600）
cargo run -p qxync-cli -- --host <NAS> --port 9834 --insecure --user <用户> --password '<口令>' login

cargo run -p qxync-cli -- status
cargo run -p qxync-cli -- ls /home
cargo run -p qxync-cli -- stat /home/qxync-test hello.txt
cargo run -p qxync-cli -- get /home/qxync-test hello.txt -o /tmp/hello.txt
cargo run -p qxync-cli -- put ./local.txt /home/qxync-test

# M1：挂载只读 on-demand 视图（另开一个终端；或加 & 后台跑）
mkdir -p ~/qsync-mnt
cargo run -p qxync-cli -- mount ~/qsync-mnt --remote /home --threads 4 --auto-unmount
ls -l ~/qsync-mnt/qxync-test      # 真实大小，尚未下载
cat ~/qsync-mnt/qxync-test/hello.txt   # 首次读触发水合
getfattr -n user.qsync.state ~/qsync-mnt/qxync-test/hello.txt   # placeholder -> hydrated
cargo run -p qxync-cli -- umount ~/qsync-mnt
```

`mount` 常用开关：`--threads N`（FUSE 事件循环线程，默认 4）、`--hydrate-timeout N`（秒，默认 60）、
`--auto-unmount`（需 `/etc/fuse.conf` 里 `user_allow_other`）、`--ipv4`（对端 IPv6 路由不通时用）、
`--rw`（读写挂载）、`--delete-limit N`（M2c 本地批量删除熔断阈值，60 秒窗口，0 = 关闭，默认 100）、
`--cache-mode pagecache|direct`（M3：默认走内核 page cache；`direct` 用 `FOPEN_DIRECT_IO` 绕过它，
脱水天然安全但没有 readahead、mmap 不可用）。

守护进程（M1.5）：

```bash
qsync daemon start          # 拉起 qxyncd（自动 fork+setsid；幂等）
qsync daemon status         # ping + 状态快照（daemon/link/会话/服务端/游标/水合统计/挂载）
qsync status                # 同上（自动路由到 daemon）
qsync ls /home/qxync-test   # 所有命令默认走 daemon
qsync pin /home/qxync-test/hello.txt pinned      # 设 pin（getfattr -n user.qsync.pin 可读）
qsync state /home/qxync-test/hello.txt           # 占位符状态 + pin
qsync mount ~/qsync-mnt --remote /home           # FUSE 由 daemon 持有（默认只读）
qsync mount ~/qsync-mnt --remote /home --rw      # M2b：读写挂载（本地改动经队列推回 NAS）
qsync sync                  # M2c：变更发现状态（三游标 / baseline / 冲突 / 删除保护）
qsync sync --once           # 立刻跑一轮（拉事件 + 对账）；--force-deletes 放行批量删除
qsync dehydrate --path /home/qxync-test/big.bin   # M3：脱水（只留占位符，释放本地缓存）
qsync dehydrate --cache-limit 2G --dry-run        # 按限额预演（LRU 该清哪些）；去掉 --dry-run 真清
qsync rm /home/qxync-test a.txt                  # 删远端条目（测试/脚本用；挂载点里 rm 走 FUSE）
qsync umount ~/qsync-mnt
qsync daemon stop           # 干净退出：卸载全部挂载 + 删 socket/pid
```

IPC 契约见 [`docs/M1.5-设计.md`](docs/M1.5-设计.md)：unix socket + **一行一个 JSON**
（`$XDG_RUNTIME_DIR/qxync/qxyncd.sock`，目录 0700 / socket 0600），手测可用
`socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/qxync/qxyncd.sock`。日志在 `~/.local/state/qsync/log/qxyncd.log.YYYY-MM-DD`。

M1 验收矩阵（挂载 → 16 项检查 → 卸载）：

```bash
xtask/tests/fuse-matrix.sh          # 快测 67 项（M1 + M2a 区间水合 + M2b 写路径 + M2c 变更发现 + M3 脱水），~12min
xtask/tests/fuse-matrix.sh --big    # 追加 128 MiB 全量读 + 并发去重（~5min，取决于带宽）
```

M2c 的真机引擎测试（冲突副本 / 删除保护 / 游标落盘，`#[ignore]` 手动跑）：

```bash
export QSYNC_TEST_HOST=... QSYNC_TEST_USER=... QSYNC_TEST_PASSWORD=...
export QSYNC_TEST_FIXTURE=/home/qxync-test
cargo test -p qxync-daemon -- --ignored --test-threads=1 --nocapture
```

> **沙箱/受限环境注意**：若 `~/.cargo` / `~/.config` 不可写，用工作区内的路径：
> `CARGO_HOME=$PWD/.cargo-home cargo build --workspace`、
> `XDG_CONFIG_HOME=$PWD/.local-run/config XDG_DATA_HOME=$PWD/.local-run/data`。
> （`.cargo/`、`.cargo-home/`、`.local-run/` 已在 `.gitignore` 中；`.cargo/config.toml` 配了 USTC 镜像，按需删改。）

真机集成测试：

```bash
export QSYNC_TEST_HOST=... QSYNC_TEST_PORT=9834 QSYNC_TEST_USER=... QSYNC_TEST_PASSWORD='...'
export QSYNC_TEST_FIXTURE=/home/qxync-test
cargo test -p qxync-proto-test -- --ignored --test-threads=1 --nocapture   # 协议 5 项 + IPC 端到端 1 项
```

## 已实测的协议要点（踩过的坑）

完整证据与探索过程见 [`docs/执行方案-M0M1.md`](docs/执行方案-M0M1.md)；写代码时最容易踩的几条：

1. **登录**：`POST /cgi-bin/authLogin.cgi`，body 必须 `serviceKey=1` + `pwd=base64(口令)`。
   明文口令、或报告里写的 `service=Qsync`，都只会得到 `authPassed=0 / errorValue=-1`。
   （登录协议的权威参考是 NAS 自带前端 `/cgi-bin/js/qos-core-login.js`，不是 Windows 二进制。）
2. **`q_token` 不是必需的**：真机 `qsyncsrv_login.cgi` 恒返回 `status:-50`，但只读端点用 QTS `sid` 直接可用。
3. **命名空间分工**：元数据走 `/cgi-bin/qsync/qsyncsrv.cgi`；
   **字节流走 `/cgi-bin/filemanager/utilRequest.cgi?func=download`（下载）与 `/cgi-bin/qsync/upload.php`（上传）**。
   `qsyncsrv.cgi?func=download` 恒返回 `status:20`，别用它。
4. **上传的 multipart 字段名必须是 `files[]`**（blueimp 风格），`upload_and_move` / `func=upload` 收不到文件体。
5. **查询串里空格必须编码成 `%20`**：用 `+` 会让含空格/中文的文件名 404（所以本项目不用 `serde_urlencoded`）。
6. **`stat` 要 `path=<目录>&file_name=<名字>&file_total=1`**，不是全路径；`get_list` 要带 `hidden_file=1` 才见隐藏文件。
7. **`/home` 才是普通用户的家目录根**（真实路径 `/share/homes/<user>`），`/home/<user>` 会 `status:5`。
8. `Range: bytes=0-99` → **HTTP 206 + `Content-Range`**，M2 的 128 KiB 区间水合有原生支持。

写 FUSE 时踩到的（M1）：

9. **写操作命名空间分工**（实测）：`rename`/`move` 只能在 FileStation（`utilRequest.cgi`）做，
   `createdir`/`delete` 用 `qsyncsrv.cgi`；`move` 必须带 `source_total=1`，且 **`dest_file` 会被忽略**
   （跨目录改名 = move + rename 两步）。
10. **`stat` 用 `exist` 判存在**：不存在的路径也返回占位条目（名字是你请求的名字、`filesize=0`），
   只有 `exist=0` 能区分；判错会让 `lookup` 误报正项、`mkdir` 直接 `EEXIST`。
11. **写前必须 read-modify-write**：写占位符前要把「不会被完整覆盖」的区间补齐，否则未取回的区间是 0，
    整文件上传会把远端内容清零（实测踩过）。
12. **`listxattr` 的返回值必须以 NUL 结尾**：内核 `fuse_verify_xattr_list()` 会逐项 `strnlen`，
   最后一项少了终止符就**把整个 listxattr 判成 `-EIO`** —— 表现是 `ls -l` 全目录报「输入/输出错误」，
   而 `stat`/`cat` 都正常（coreutils 的 `ls -l` 会查 ACL 从而调 `listxattr`）。
   实测判据：`size<66` 回 `ERANGE`，`size>=66` 反而 `EIO`，就是这个校验触发的。
13. **`attr_timeout`/`entry_timeout`/`max_read` 不是 fusermount 挂载选项**，传给 `-o` 会直接
    `unknown option` 挂载失败；TTL 应通过每次 `reply.entry/attr(&ttl, ..)` 传，`max_readahead` 在 `init()` 里设。
14. **fuser 0.17 的 `AutoUnmount` 要求 `SessionACL != Owner`**（即 `allow_other`，非特权挂载还需
    `/etc/fuse.conf` 的 `user_allow_other`），否则 mount 报 `auto_unmount requires acl != Owner`。
12. **整文件水合 + 60s 超时在大文件/慢链路上必然失败**：实测对端 ~1.1 MB/s 时 128 MiB 要 116s。
    M1 的应对是 `--hydrate-timeout` 可调；根治是 M2 的 128 KiB 区间水合（只取需要的分片）。
16. FUSE 调用里 `block_on` 要用**独立运行时**，且挂载线程别用 `tokio::spawn_blocking`
    （blocking 线程带 runtime 上下文，再 `block_on` 另一个 runtime 会 panic）。
17. 对端同时发布 AAAA 但 IPv6 路由不通时，会出现 `Network is unreachable` 或传输中途 body 解码失败 →
    用 `--ipv4`（客户端 `local_address` 绑 IPv4 源地址）规避。

写变更发现时踩到的（M2c）：

18. **`qbox_get_sync_log` 的 `lower` 是闭区间下界**（`lower=30` 会返回 `log_id=30`）→ 游标推进到
    「最后一条 `log_id` + 1」；区间内没有事件时返回 **`status:-17`**，这不是协议错。
19. **事件里 `isfolder` 是 `1`=目录 / `2`=文件 / `0`=删除项**（不是布尔）；`size` 是字符串。
20. **删除事件的 `filepath` 实测为空**，而且**我们自己的 CGI 写操作（upload/rename/move/delete）
    不产生 sync log 事件**（本机未做设备配对）→ 变更发现必须**以 baseline 对账为主路径**，事件只是快路径。
21. `qbox_write_log` 会让 `max_log` 上涨但区间内取不到事件 → 游标**只按实际返回的事件推进**，
    `-17` 时不推进、只记账，避免「推进了游标但事件丢了」。

脱水时踩到的（M3）：

22. ★ **顺序铁则**：`Notifier::inval_inode(ino, 0, 0)` 必须在清内容**之前** —— 否则内核 page cache
    里的旧页会让应用读到旧数据。验收方式：脱水后把远端改成**同长度不同内容**再 `cat`，必须拿到新内容。
23. `fuser` 的 `mount2()` **拿不到 `Notifier`** → 改用 `fuser::spawn_mount2()`（daemon 侧），
    拿到 `BackgroundSession`（join/卸载）+ `Notifier`（`inval_inode`）。
24. **mmap 挡不住**：Linux 的 `flock` 不阻止 mmap → 只能自己扫 `/proc/*/maps`。
    实测补充：mmap 会给映射保留 `struct file`，进程 `close(fd)` 后 FUSE `release` 也不触发，
    所以「打开的 fd」计数本身就是第一道防线（`/proc` 扫描是兜底）。
25. 本地写入的区间必须记进区间表（`chunks_done`），否则本地新建/改过的文件会被判成
    「没有缓存内容」（实测：16 MiB 本地文件脱水被报「本来就是占位符」）。
26. 稀疏缓存别用 `du -sb`（apparent size）量占用 —— 128 MiB 的稀疏文件会算成 128 MiB；
    要 `du -s --block-size=1`（allocated）。

## 两条铁则（整个项目不许违反）

1. 每次 `read()` 必须返回真实数据或明确 `EIO`，**绝不短读**（短读 = 内核零填充 → 数据静默损坏）。
   M1 已实测：水合失败/超时 → `cat` 拿 `EIO` 且**输出 0 字节**，不挂死、不吐假数据。
2. 每次脱水必须**先 `inval_inode` 让内核失效缓存，再清内容**，顺序反了 = 数据错乱。
