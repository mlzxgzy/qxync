# 贡献指南 / Contributing

[中文](#中文) · [English](#english)

先读这两个：

- **[`README.md`](README.md) 的「两条铁则」** —— 整个项目不许违反，改动 FUSE 读路径或脱水路径时尤其注意。
- **[`DISCLAIMER.md`](DISCLAIMER.md)** —— 本项目的性质与法律边界。

---

## 中文

### 1. 这个项目接受什么

**欢迎**：

- 🐛 **Bug 报告**：请用 [Bug 报告模板](.github/ISSUE_TEMPLATE/bug_report.yml)，把复现步骤、日志、环境写全。
- 📄 **协议事实更正**：你在**你自己拥有或已获授权**的 NAS 上实测出与 README 或代码注释
  不符的行为 —— 这类 Issue 价值最高，请附上**原始响应**（记得先掩码你的域名 / 账号 / sid）。
- 🔧 **代码 PR**：修 bug、补测试、改进文档、增加发行版适配。
- 🌍 **翻译**：`ui/i18n.js` 里预留了 `en` 空表；把界面文案补齐是很好的切入点。

**不接受**（会被直接关闭）：

- ❌ 绕过授权、破解、批量抓取他人 NAS 的内容
- ❌ 要求实现「自动跳过证书校验之外的安全措施」或任何规避保护手段的功能
- ❌ 分发 QNAP 的二进制、安装包、反编译产物或大段原始字符串转储
- ❌ 在 Issue / PR 里粘贴**未经掩码**的真实域名、账号、口令、sid、设备指纹

### 2. 开发环境

```bash
git clone https://github.com/mlzxgzy/qxync.git
cd qxync
cargo build --workspace
cargo test --workspace        # 不需要 NAS
```

- 需要 **Rust 1.90+**、**FUSE 3**（`/dev/fuse` + `fusermount3`）；
  构建 GUI 还需要 WebKitGTK 4.1 / GTK 3 的开发包。
- 前端在 `crates/qxync-gui/ui/`，是**零依赖静态三件套**：改完必须重新 `cargo build`
  （`frontendDist` 是编译期嵌入的），没有 npm、没有打包器。

### 3. 提交前必须跑

```bash
cargo fmt --all                 # 格式化（见下方「已知格式债」）
cargo clippy --workspace --all-targets
cargo test --workspace
```

**改了这些路径，必须附上对应验收矩阵的复跑结果**（脚本在 `xtask/tests/`）：

| 改了什么 | 必须跑 |
|---|---|
| FUSE 读 / 写 / 水合 / 脱水 | `xtask/tests/fuse-matrix.sh` |
| 同步引擎 / 游标 / 冲突 / 删除保护 | `xtask/tests/fuse-matrix.sh`（含 M2c 段） |
| 状态库 schema / 迁移 | `xtask/tests/m5-matrix.sh`、`xtask/tests/m83-matrix.sh` |
| 排除规则 / LAN 对等 | `xtask/tests/m7-matrix.sh`（`--no-nas` 可先自测） |
| 任务登记 / 恢复 | `xtask/tests/m82-matrix.sh` |
| 一对一配对 / 目的地冲突 | `xtask/tests/pair-1to1.sh`（不需要 NAS） |
| 设置 / 代理 / 托盘 / 释放空间 | `xtask/tests/m84-matrix.sh` |
| `ui/` 任意文件 | `xtask/tests/gui-matrix.sh`（无 DISPLAY 用 `--no-window`） |

矩阵需要真机凭据（`QXNYC_TEST_HOST` / `QXNYC_TEST_USER` / `QXNYC_TEST_PASSWORD`）。
**如果某条你跑不了，请在 PR 里明说**，不要假装跑过 —— 这是比「测试没跑」严重得多的问题。

### 4. 两条铁则（复审必查）

1. **每次 `read()` 必须返回真实数据或明确 `EIO`，绝不短读。** 短读 = 内核零填充 = 数据静默损坏。
2. **每次脱水必须先 `inval_inode` 让内核失效缓存，再清内容。** 顺序反了 = 数据错乱。

任何可能让这两条失守的改动，请**在 PR 描述里显式论证**它为什么安全。

### 5. 文档与语言约定

- **面向读者的长文档用「双文件」**：`X.md`（中文，权威）+ `X.en.md`（英文镜像），
  两者必须同步更新 —— 改了中文忘了英文，PR 会被要求补上。
- **短小的政策类文档用「单文件双语」**：像本文件与 `SECURITY.md`，以及既有的
  `DISCLAIMER.md`。
- **`docs/` 下的设计与执行文档目前只有中文**，这是有意的（它们是开发过程记录）；
  新增这类文档也按中文写即可。
- **代码注释用中文**，与现有风格保持一致。

### 6. 提交信息

参考现有历史（Conventional Commits 风格 + 中文正文）：

```
fix(fuse): 挂载失败不再把 daemon 打成 panic（tokio Runtime 在 async 上下文里析构）
feat(m8.6): 打磨与验收收口 —— 视觉规范 / 键盘可达性 / 空错加载四态 / i18n / ui_spec
```

`type(scope): 一句话说清结果`。正文写**为什么**，以及**怎么验证的**（贴矩阵结果）。

### 7. 已知格式债

仓库目前的 Rust 代码**尚未整体 `cargo fmt` 过**（约 21 个文件 / 200 余处差异），
CI 里的 `fmt` 任务是**非阻断**的。我们**不希望**在一个功能 PR 里夹带全库格式化 ——
格式化会单独作为一个 commit 收口。**新写 / 新改的代码请保持 `cargo fmt` 干净。**

### 8. 许可

提交即表示你同意以 **MIT OR Apache-2.0** 双许可授权你的贡献（与仓库一致）。
请勿提交任何你无权授权的第三方代码或素材。

---

## English

### 1. What this project accepts

**Welcome**:

- 🐛 **Bug reports** — use the [Bug report template](.github/ISSUE_TEMPLATE/bug_report.yml)
  and include reproduction steps, logs, and environment.
- 📄 **Protocol corrections** — a behaviour you measured on a NAS **you own or are authorized to
  administer** that differs from the README or the code comments. These are the most valuable issues:
  attach the **raw response** (mask your hostname / account / sid first).
- 🔧 **Code PRs** — bug fixes, tests, docs, distro packaging.
- 🌍 **Translations** — `ui/i18n.js` ships an empty `en` table; filling in the UI strings is a
  great first contribution.

**Not accepted** (will be closed):

- ❌ Circumventing authorization, cracking, or bulk-scraping other people's NAS content
- ❌ Requests to bypass protection measures or weaken security beyond accepting a self-signed cert
- ❌ Distributing QNAP binaries, installers, decompilation output, or bulk raw string dumps
- ❌ Pasting **unmasked** real hostnames, accounts, passwords, sids, or device fingerprints
  into issues or PRs

### 2. Development setup

```bash
git clone https://github.com/mlzxgzy/qxync.git
cd qxync
cargo build --workspace
cargo test --workspace        # no NAS required
```

- Requires **Rust 1.90+** and **FUSE 3** (`/dev/fuse` + `fusermount3`);
  building the GUI additionally needs the WebKitGTK 4.1 / GTK 3 dev packages.
- The frontend lives in `crates/qxync-gui/ui/` and is a **dependency-free static trio**:
  after editing it you must re-run `cargo build` (`frontendDist` is embedded at compile time).
  There is no npm and no bundler.

### 3. Before you push

```bash
cargo fmt --all                 # see "Known formatting debt" below
cargo clippy --workspace --all-targets
cargo test --workspace
```

**If you touched any of the paths below, attach the corresponding acceptance-matrix run**
(scripts in `xtask/tests/`):

| You changed | You must run |
|---|---|
| FUSE read / write / hydrate / dehydrate | `xtask/tests/fuse-matrix.sh` |
| Sync engine / cursors / conflicts / delete protection | `xtask/tests/fuse-matrix.sh` (M2c section) |
| State store schema / migration | `xtask/tests/m5-matrix.sh`, `xtask/tests/m83-matrix.sh` |
| Exclude rules / LAN peer | `xtask/tests/m7-matrix.sh` (`--no-nas` for a quick local pass) |
| Task registry / restore | `xtask/tests/m82-matrix.sh` |
| Settings / proxy / tray / free-up-space | `xtask/tests/m84-matrix.sh` |
| Anything under `ui/` | `xtask/tests/gui-matrix.sh` (`--no-window` without a DISPLAY) |

The matrices need real-NAS credentials (`QXNYC_TEST_HOST` / `QXNYC_TEST_USER` /
`QXNYC_TEST_PASSWORD`). **If you could not run one, say so in the PR.** Pretending a matrix
passed is a far more serious problem than not running it.

### 4. The two hard rules (reviewers check these)

1. **Every `read()` must return real data or an explicit `EIO` — never a short read.**
   A short read means the kernel zero-fills, i.e. silent data corruption.
2. **Every dehydration must call `inval_inode` before clearing content.**
   Reversed order means data corruption.

Any change that could break either rule must **explicitly argue in the PR description** why it
is safe.

### 5. Documentation and language conventions

- **Reader-facing long docs use two files**: `X.md` (Chinese, authoritative) + `X.en.md`
  (English mirror). They must be updated together — a PR that updates only one will be asked to
  add the other.
- **Short policy docs use one bilingual file**: like this one, `SECURITY.md`, and the existing
  `DISCLAIMER.md`.
- **Design/execution docs under `docs/` are Chinese-only on purpose** (they are development
  logs); new ones can follow that.
- **Code comments are in Chinese**, matching the existing style.

### 6. Commit messages

Follow the existing history (Conventional Commits + Chinese body):

```
fix(fuse): 挂载失败不再把 daemon 打成 panic（tokio Runtime 在 async 上下文里析构）
feat(m8.6): 打磨与验收收口 —— 视觉规范 / 键盘可达性 / 空错加载四态 / i18n / ui_spec
```

`type(scope): one line stating the outcome`. In the body, explain **why** and **how you
verified it** (paste the matrix result).

### 7. Known formatting debt

The Rust code has **not** been run through a repo-wide `cargo fmt` yet (about 21 files /
200+ diffs). The CI `fmt` job is **non-blocking** on purpose. We do **not** want a whole-repo
reformat smuggled into a feature PR — that will land as its own commit.
**New or modified code should be `cargo fmt` clean.**

### 8. License

By contributing you agree to license your contribution under **MIT OR Apache-2.0**, matching the
repository. Do not submit third-party code or assets you are not entitled to license.
