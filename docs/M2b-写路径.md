# M2b：写路径（FUSE 写操作 → 上传队列 → 服务端）

> 目标：让挂载点可读写——本地建/改/删/改名都要落到 NAS，且**不损坏远端内容**。
> 状态（2026-09-30）：已实现并真机验收（`xtask/tests/fuse-matrix.sh` 共 30 项全过，其中写路径 10 项）。

---

## 1. 真机写接口契约（都是实测出来的）

| 操作 | 端点 | 参数 | 备注 |
|---|---|---|---|
| 上传 | `POST /cgi-bin/qsync/upload.php` | query `sid/dest_path/overwrite=1/type=standard`，multipart 字段名 **`files[]`** | M0 已验证 |
| 对齐 mtime | `GET qsyncsrv.cgi?func=stat&settime=1&mtime=` | `path`+`file_name`+`file_total=1` | 不对齐会被判「未同步」 |
| 建目录 | `POST qsyncsrv.cgi?func=createdir` | body `dest_path`/`dest_folder` | 成功回 `status:1` |
| 删除 | `POST qsyncsrv.cgi?func=delete` | body `path`/`file_name`/`file_total=1` | 文件与目录同一个调用 |
| 同目录改名 | `POST utilRequest.cgi?func=rename` | body **`path`/`source_name`/`dest_name`** | ⚠️ 在 `qsyncsrv` 上做 rename 一律 `status:20`；只改大小写可直接成功 |
| 跨目录移动 | `POST utilRequest.cgi?func=move&no_fork=1` | body `source_path`/`source_file`/`dest_path`/`dest_file`/**`source_total=1`** | ⚠️ 两个坑见下 |
| 记变更日志 | `GET qsyncsrv.cgi?func=qbox_write_log` | `filepath`/`action` | 服务端不校验 action |

**跨目录移动的两个坑**：
1. **必须带 `source_total=1`**，否则静默不动（回 `status:1` 但没搬）。
2. **`dest_file` 被忽略**：文件在目标目录里保持原名。所以「跨目录 + 改名」必须拆成
   `move`（保持原名）→ `rename`（在目标目录里改）。客户端 `move_into()` 只负责搬，改名由调用方补一刀。

**`stat` 的坑**：不存在的文件**不会报错**，而是回一个占位条目（`filename` 是请求的名字、
`filesize=0`、`owner/privilege` 为空），只有 **`exist=0`** 能区分。
早期用「文件名非空」判存在 → FUSE `lookup` 误报正项 → `mkdir` 直接 `EEXIST`（已修）。

---

## 2. FUSE 写路径设计

```
write()/setattr(size) ──► ① 确保本地内容完整（read-modify-write / 本地权威则跳过）
                          ├─► ② pwrite 到稀疏缓存 + 更新 size/mtime
                          └─► ③ mark_dirty：写 .dirty 标记（刷盘）→ 入上传队列
create()/unlink()/rename()/mkdir()/rmdir() ──► 直接调服务端 API（删/改名不需要队列）
```

### 2.1 ★ 铁则：read-modify-write

写一个还没取全的占位符文件时，**未取回的区间在本地是 0**；如果直接写 + 整文件上传，
远端内容会被清零。所以写之前必须把「不会被完整覆盖」的区间补齐：

```rust
// 只有「写范围完整覆盖该区间」才能跳过
fully_covered = c_start >= write_start && c_end <= write_end
```

> 实测踩过：10 KB 文件尾部追加 9 字节时，按「写到哪个区间就跳过哪个区间」处理，
> 结果前 10 KB 全变成 0 上传 —— 这是最危险的一类静默损坏，已修并进验收矩阵。

### 2.2 本地权威：有未上传改动时不从远端拉

本地新建的文件远端还不存在，第二次 `write` 若还去 `hydrate_all` 会 **HTTP 404**（实测）。
规则：**节点 dirty（或队列里还有它的作业）时，本地缓存就是权威内容**，一律跳过远端拉取。
崩溃重启后，`.dirty` 标记会让队列重新入队，`has_pending()` 也会让该路径继续保持「本地权威」。

### 2.3 上传队列

* 每个改动先写 `<data>/upload-queue/<fnv1a(远端路径)>.dirty`（JSON：目录/名字/本地文件/mtime），
  **先落标记再改数据** → 崩溃/断电后重启扫描标记重新入队，不丢改动。
* 单 worker 线程串行消费；同一路径的新作业会替换旧的（写十次只传最后一次）。
* 失败指数退避重试（最多 5 次），超过记为 failed 并通过 `status` 暴露；标记文件保留。
* 成功 = 上传 → `settime` 对齐 mtime → `qbox_write_log`（尽力而为）→ 删标记。
* 卸载/改名/删除前会 `drain()` 排空相关作业，避免「队列还指着旧名字」。

### 2.4 只读 vs 读写

* 默认仍是**只读**挂载（M1 行为不变）；`qxync mount --rw` 才开写路径并创建上传队列。
* 只读时任何写意图回 `EROFS`（`open` 阶段就拦）。
* `status` 会显示上传队列：`待上传 / 完成 / 失败（重试次数、字节数）`。

---

## 3. 验收（`fuse-matrix.sh` 的 M2b 段，10 项）

| 检查 | 说明 |
|---|---|
| 新文件已上传 / 内容一致 | `printf > mnt/.../mx-new.txt` → 远端字节一致 |
| **read-modify-write 正确** | 对 10 KB 远端文件做尾部追加 + 中间改写，逐字节比对期望值（原内容未被清零） |
| 远端出现 / 删除 mx-dir | `mkdir` / `rmdir` |
| 同目录改名生效 / 只改大小写生效 | `mv`；NAS 大小写不敏感，实测可直接改 |
| 跨目录 move + 改名生效 / 移动后内容一致 | 走 `move` + `rename` 两步 |
| 测试产物已清理 | `rm`/`rmdir` 后远端干净 |

---

## 4. 已知限制（留给后续）

* **`qbox_write_log` 的效果未证实**：服务端接受任意 action（0..5 都回 status 1），
  但只有路径落在**已注册的同步文件夹**里才会进 `qbox_get_sync_log`——本机测试账号没有注册同步对
  （`qbox_save_device_config` / 设备配对不在 M2b 范围）。action 取值按真机日志推断：
  `1`=删除、`12`=新建目录、`14`=文件新增/修改。
* **整文件上传**：写一个 128 MiB 占位符仍会上传整个文件（局部改动不划算）；增量 delta 是 M6。
* **改大小写/跨目录改名前的 drain 是粗粒度同步**（最多等 120s），大文件场景会显得慢。
* 远端变更的刷新（另一台设备改了同一文件）要等 M2c 的三游标轮询 + baseline。
* 删除保护（本地大批删除 vs 远端）也是 M2c。

---

## 5. 顺带解开的旧谜团

* **`qbox_get_sync_log` 的 `status:-17`**：不是协议不对，而是**该用户当时没有任何日志事件**。
  现在库里有了事件，同样的调用返回 `{"status":0,...,"data":[...]}`。
* 事件结构（M2c 直接用）：`{log_id, user, old_filepath, filepath, action, isfolder, device,
  device_uid, exist, mtime, size}`；`filepath` 是**真实路径** `/share/homes/<user>/...`
  （需要和 Qsync 的 `/home/...` 做映射），并且**带设备归属**（M2c 必须按 device_uid 过滤，
  否则会处理别的设备的事件）。
* 我们的 CGI 写操作（upload/rename/move/delete）**不产生** sync log 事件；
  现有事件都来自另一台已配对设备 `win-pc`。
