# xtask/probe/ —— 协议探测工具

`qs_probe.py` 用来对**你自己的** QNAP NAS 复现本项目已实现的 API，把原始响应落盘。
它的存在是为了解决**静态分析无法确定的参数** —— 凡是二进制里查不到、只有真实请求才能回答的问题，
都先用它探一遍再写代码。

⚠️ 仅用于你拥有或获授权管理的设备。

## 为什么需要它

| 问题 | 脚本怎么帮你 |
|---|---|
| `authLogin.cgi` 的密码字段到底叫什么（`pwd`? `pass`? `auth_pass`? 还是 AES 封装） | `login` 会依次尝试 4 个候选字段并报告哪个通过 |
| 响应是 XML 还是 JSON、字段名与嵌套 | 原始响应全文落盘 |
| `qbox_get_max_log` 返回结构 | `probe` 直接打印 |
| `qbox_get_sync_log` 的事件格式与 `number` 批量 | `probe` 会带 `get_detail=1&sub_folder=/` 调一次 |
| 地址候选集字段（DDNS / WAN / LAN / 端口） | `probe` 调 `get_domain_ip_list` |
| 版本化能力 | `probe` 调 `versioning_probe` |
| `%ls` URL 前缀到底填什么 | `--prefix` 参数，直连留空 |

## 脚本一览

| 脚本 | 用途 |
|---|---|
| `qs_probe.py` | 通用探测：登录 / 批量只读端点 / 自定义查询，原始响应落盘 |
| `qs_fixture.py` | 在 NAS 上造/删测试数据（`/home/qxync-test/`），并做下载 + Range + md5 校验 |
| `nas_manifest.py` | **只读**清单：列出某个根的完整树，`--diff` 比对两次快照（验收时的「NAS 数据基线」） |
| `p0_device_probe.py` | 设备注册相关端点的探针（Phase A/B/C），结论是**决策不实现设备注册** |
| `paging_probe.py` | `get_list` 的 `start`/`limit` 分页与 `total` 语义（小目录速查版） |
| `bigdir_probe.py` | **造 500 项目录**并验证翻页不重不漏（M15/T1 的前置，结论见下） |
| `ls_probe.py` | 汇总 `probe-out/` 里所有 `get_list` 抓包的 `limit/start/total/datas` 关系 |

连接目标一律走环境变量或 `--host`，**默认值是指不到任何真实设备的占位符**：

```bash
export QXNYC_TEST_HOST=your-nas.example.com
export QXNYC_TEST_PORT=9834
```

## 用法

```bash
# 1) 只验证登录（会打印 authLogin 的原始响应，并试出正确密码字段名）
python3 qs_probe.py --host 192.168.1.10 --port 8080 --user admin --password 'xxx' login

# 2) HTTPS + 自签证书
python3 qs_probe.py --host nas.example.com --port 443 --https --insecure \
                   --user admin --password 'xxx' login

# 3) 登录 + 批量调用只读端点
python3 qs_probe.py --host 192.168.1.10 --user admin --password 'xxx' probe

# 4) 自定义查询（需要已有 sid）
python3 qs_probe.py --host 192.168.1.10 raw 'func=qbox_get_qbox_info&sid=XXXX'

# 5) 造测试夹具（含 >100 MiB 大文件）/ 只造 16 MiB
python3 qs_fixture.py fixture
python3 qs_fixture.py fixture --small

# 6) 验证 get_list 分页（★ 会往 /home/bigdir-probe 写 500 个小文件，约 4~5 分钟）
python3 bigdir_probe.py --host nas.example.com --port 9834 --user test1 --password 'xxx' --n 500

# 7) 只看结论不重建数据（目录已存在时）
python3 bigdir_probe.py --host nas.example.com --port 9834 --user test1 --password 'xxx' --skip-build
```

输出：stdout 同时写入 `probe-out/<时间戳>/*.http`（含完整响应头与正文）。

> ⚠️ `probe-out/` 里有**完整原始响应（含 sid、账号、主机名）**，已被 `.gitignore` 排除，
> **永远不要提交**。

## ★ `get_list` 分页的实测定论（2026-10-04，M15/T1）

`bigdir_probe.py` 在真机上造了 501 项目录后测出来的，**改分页相关代码前先看这段**：

| 验证项 | 结论 |
|---|---|
| `limit=200&start=0` | 只返回 **200** 条，`total=501` → 服务端**确实按页返回** |
| 按 `start` 翻页 | 3 页拼回 **501** 条，**重复 0、漏 0** → 分页可用 |
| `limit=5000` | 生效并一次返回 501 条 → 服务端**不**把 limit 夹在 `Max_File_List=200` |
| `total` / `real_total` | **目录总项数**（`limit=1` 时 `total` 仍是 3，不是本页的 1） |

推论：`Client::list` 的翻页循环本来就是对的，而下游 `qxync-fuse::filter_visible`
曾经又 `.take(200)` 了一次 —— 那是「>200 项目录 `ls` 读不全」的真根因（M15/T1 已修）。

## 建议的验证顺序

1. **登录**：`login`。**2026-09-30 已实测定论（QTS 5.2.9）**：服务端只认
   `serviceKey=1` + `pwd=base64(口令)`（QNAP 前端的 `ezEncode`=标准 base64）；
   明文口令、或早期笔记里写的 `service=Qsync`，都会得到 `authPassed=0 / errorValue=-1`。
   脚本已按「`serviceKey+b64` → `serviceKey+plain` → 旧写法」顺序尝试并打印命中变体
   （详见 [`docs/执行方案-M0M1.md`](../../docs/执行方案-M0M1.md) §1.1）。
   若仍失败，再看响应里的 `error_code` / `need_2sv`：
   - `need_2sv=1` → 你的账号开了二步验证，用 `--param` 传 `code` / `security_answer` 等
   - `error_code` 指向账号密码错 → 换账号
2. **拿 SID 后**：`probe`，重点看 `qbox_get_max_log`、`qbox_get_syncing_folder_list`、`get_domain_ip_list`
3. **变更事件**：在 NAS 上改一个文件，再跑一次 `probe`，对比两次 `qbox_get_sync_log` 的差异
4. **传输参数**：用 NAS 自带工具或 `tcpdump` 抓官方客户端的下载/上传，量出分片大小与轮询间隔

## 相关文档

- 实测出来的协议结论汇总在 [`README.md` 的「协议要点」](../../README.md#协议要点踩过的坑)
- 真机验证的执行记录见 [`docs/执行方案-M0M1.md`](../../docs/执行方案-M0M1.md)
- 各里程碑的设计与决策见 [`docs/`](../../docs/)
- 法律边界与使用者责任见 [`DISCLAIMER.md`](../../DISCLAIMER.md)
