# 变更日志

本文件格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)。

[English](CHANGELOG.en.md) · [README](README.md) · [验收记录](docs/验收记录.md)

## [未发布]

## [0.1.0] - 2026-10-02

**首个公开版本。** 这不是「能跑起来」的程度：每个里程碑都在真机上跑过完整验收，
最近一次全量口径 **491 项全过**（`fuse 68 · gui 148 · m5 30 · m6 29 · m7 60 · m82 37 · m83 27 · m84 92`），
明细见 [`docs/验收记录.md`](docs/验收记录.md)。

真机验证对象：**QNAP TS-464C / QTS 5.2.9 / Qsync QPKG 5.0.0.7（build 20260723）**。

### 新增

- **协议客户端与 CLI（M0）**：登录 / 列举 / stat / 下载 / 上传 / 建目录五条路全部对真机跑通；
  二进制 `qsync` 提供 `login · status · ls · stat · get · put · mkdir`。
  协议结论来自对 Qsync for Windows v6.1.0.0831 的静态逆向，且每一条都在真机上复验过。
- **只读 FUSE + on-demand 按需水合（M1）**：`ls -l` 显示真实大小却零下载，首次 `read()` 才取数据；
  `user.qsync.*` xattr 暴露占位符状态；读不满即 `EIO`，绝不短读。
- **守护进程与本地 IPC（M1.5）**：二进制 `qxyncd` 成为**唯一**持有 FUSE 与 NAS 会话的进程；
  unix socket + 一行一个 JSON 的 IPC；`qsync` 默认自动路由（socket 可连就走 IPC）。
- **128 KiB 区间水合（M2a）**：`head -c 100 big.bin` 只下载 1 个区间，不再整文件下载。
- **写路径（M2b）**：`--rw` 读写挂载；上传队列 + dirty 标记崩溃恢复；
  **写前 read-modify-write**，绝不把没取回的区间当 0 上传。
- **变更发现（M2c）**：三游标轮询 + baseline 三向对账（事件只是快路径）；
  双方都改用**冲突副本**，远端批量删除用**熔断**保护。
- **脱水 / 释放空间（M3）**：完整安全检查链（pin、未上传改动、打开的 fd、被 mmap、正在水合、
  刚访问过）后才清本地内容；`inval_inode` 顺序铁则；闲置 / 限额 LRU；`--cache-mode pagecache|direct`。
- **桌面 GUI（M4）**：Tauri 2 应用，前端是**零依赖静态三件套**（无 npm）；
  登录配置 / 挂载管理 / 状态与进度 / pin 管理。
- **SQLite 状态库 + delta（M5）**：`sync.db` 承载游标 / baseline / pin / 上传队列，
  **同一事务**落盘；老 JSON 状态自动迁移并归档；librsync 原生格式的 sign/delta/patch + 能力门控。
- **多根 / 共享文件夹（M6）**：link 配 `roots`，挂载点顶层出现每个根的名字；
  家目录根可读写，非家目录根在 FUSE 层直接 `EROFS`。
- **选择性同步（M7）**：gitignore 风味的 `exclude` 规则引擎（锚定 / `*.iso` 任意层级 /
  子树剪枝 / 反向包含 / `**`）+ 内置临时文件过滤，贯通 FUSE、同步引擎与脱水候选。
- **LAN 直连（M7）**：qxync↔qxync 自研对等协议 —— 设备配对、事件快路径、本地区间直传；
  默认不监听，任何失败 / 不一致 / 超时都静默回落 NAS。
- **同步任务（M8.2）**：`tasks/<id>.json` 持久化登记，逐任务暂停 / 继续，
  `qxyncd --restore-tasks` 重启恢复（默认关闭）。
- **同步日志（M8.3）**：`sync.db` schema v3 增加 `journal` 表，后台批量落库 + 轮转，
  驱动 GUI 的「文件更新中心 / 错误列表」。
- **设置中心（M8.4）**：代理三模式（Auto-detect / No proxy / Manual，接 reqwest 与
  `CONNECT` 实测）、开机自启、桌面通知、ksni 托盘（含「真可见」探测）、
  `statvfs` 自动释放空间（复用脱水安全链）、冲突策略五选、文件三态。
- **打磨与可访问性（M8.6）**：视觉规范 token 化（浅 / 深两份）、键盘可达性与焦点环、
  空 / 错 / 加载四态全库复查、i18n 文案表（zh-CN 163 条 + en 预留）、
  `qxync-gui --self-test` 的 `ui_spec` 静态自检。

### 修复

- 挂载失败不再把 daemon 的 IPC worker 打成 panic：`QxyncFs` 的 tokio `Runtime` 在 async 上下文里
  析构会触发 `Cannot drop a runtime in a context where blocking is not allowed`。
  改为按上下文选路的 `FsRuntime`（tokio 里用 `shutdown_background()`），
  并新增回归单测 `dropping_fs_inside_async_context_does_not_panic`。

### 安全

- **仓库内不含任何真实主机名、账号、本机路径或设备指纹** —— 发版前做过统一清洗，
  映射与验证方法见 [`docs/发布清单-v0.1.0.md`](docs/发布清单-v0.1.0.md)。
- 仓库**不分发** QNAP 二进制 / 安装包 / 反编译产物；报告中的第三方凭据已掩码。
- 凭据文件 `0600`，IPC socket 目录 `0700` / socket `0600`。
- **无任何遥测**；除你配置的 NAS 与可选 LAN 对端外不连接任何主机。

### 文档

- 中英双语 `README.md` / `README.en.md`。
- 新增 [`CONTRIBUTING.md`](CONTRIBUTING.md)、[`SECURITY.md`](SECURITY.md)、
  本变更日志（中英双语）。
- 验收结论从 README 抽出为 [`docs/验收记录.md`](docs/验收记录.md)。
- 采用 **MIT OR Apache-2.0** 双许可（`LICENSE-MIT` / `LICENSE-APACHE`）。

[未发布]: https://github.com/mlzxgzy/qxync/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/mlzxgzy/qxync/releases/tag/v0.1.0
