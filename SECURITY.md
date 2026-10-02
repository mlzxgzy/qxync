# 安全策略 / Security Policy

[中文](#中文) · [English](#english)

---

## 中文

### 报告安全问题

**请不要开公开 Issue。** 请用 GitHub 的
[私密漏洞报告](https://github.com/mlzxgzy/qxync/security/advisories/new)
（仓库 → Security → Report a vulnerability）。

如果该入口不可用，可以开一个**不含任何细节**的 Issue，只说「我有一个安全问题要私下报告」，
我会回一个联系方式。

### 我会怎么处理

| 阶段 | 目标 |
|---|---|
| 确认收到 | 7 天内 |
| 初步判定（是否成立 / 影响面 / 严重性） | 14 天内 |
| 修复或缓解 | 视严重性，尽力尽快；修好后发布带说明的新版本 |

这是一个**个人业余项目**，没有安全团队、没有 SLA、没有漏洞赏金 —— 请据此调整预期。
但**数据安全类问题我会优先处理**：这个项目的全部价值都建立在「不会悄悄弄坏你的数据」上。

### 什么算安全问题

- **数据完整性 / 数据丢失**（最高优先级）：短读、写回把远端内容清零、脱水顺序错误导致读到旧数据、
  冲突处理把两边改动都丢掉 —— 也就是 README 里**两条铁则**的任何一条失守。
- **凭据泄漏**：口令落进日志 / 进程参数 / shell 历史；`credentials.json` 权限不对；
  IPC socket 可被其它用户连接。
- **越权**：非家目录根被写出、`exclude` 规则被绕过、LAN 对端 token 获得了写 / 删远端的能力。
- **依赖漏洞**：`cargo audit` 报出的、影响本项目的 RUSTSEC 条目。

### 什么**不算**（请走普通 Issue）

- NAS 自身或 QTS / Qsync 官方客户端的漏洞（请报给 QNAP）
- 需要你先在 NAS 上执行任意命令 / 关闭认证才能触发的「问题」
- 自签证书需要 `--insecure` —— 这是**已知且有意的**设计（README 有说明），不是漏洞

### 我们不会做的事

- 不采集任何遥测
- 不连接除你配置的 NAS 与（可选的）LAN 对端之外的任何主机
- 不写 NAS 的设备列表、不修改 NAS 侧任何配置

---

## English

### Reporting a vulnerability

**Please do not open a public issue.** Use GitHub's
[private vulnerability reporting](https://github.com/mlzxgzy/qxync/security/advisories/new)
(repo → Security → Report a vulnerability).

If that is unavailable, open an issue containing **no details** other than "I have a security
issue to report privately" and I will reply with a contact channel.

### What to expect

| Stage | Target |
|---|---|
| Acknowledgement | within 7 days |
| Initial assessment (validity / impact / severity) | within 14 days |
| Fix or mitigation | as fast as severity warrants; a new release with notes follows |

This is a **personal, part-time project**: no security team, no SLA, no bug bounty — please set
expectations accordingly. That said, **data-safety issues get priority**: the entire value of
this project rests on not silently corrupting your data.

### What counts as a security issue

- **Data integrity / data loss** (highest priority): short reads, a write-back that zeroes remote
  content, out-of-order dehydration causing stale reads, conflict handling that loses both sides —
  i.e. any breach of the **two hard rules** in the README.
- **Credential exposure**: passwords in logs / argv / shell history, wrong
  `credentials.json` permissions, an IPC socket other users can connect to.
- **Privilege escalation**: writes into a non-home root, an `exclude` rule being bypassed, a LAN
  peer token gaining the ability to write to or delete remote files.
- **Dependency vulnerabilities**: RUSTSEC advisories from `cargo audit` that affect this project.

### What does **not** count (use a normal issue)

- Vulnerabilities in the NAS itself or in QTS / the official Qsync client (report those to QNAP)
- "Issues" that require you to already run arbitrary commands on the NAS or disable authentication
- Self-signed certificates needing `--insecure` — this is **known and intentional** (documented in
  the README), not a vulnerability

### What this project will never do

- Collect telemetry
- Connect to anything other than the NAS you configured and (optionally) a LAN peer
- Write to the NAS device list or modify any NAS-side configuration
