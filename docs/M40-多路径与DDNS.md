# M40 —— 多路径连接（内网 / 公网 / DDNS）与失败自诊断

> **状态**：已发布（v0.7.13）。本文是给后续开发者的设计说明：
> 一条 NAS 连接为什么可以有多条路径、怎么选、什么时候切、连不上时怎么查，
> 以及哪些边界**故意**没做。

起因是用户的一句话：

> 「做一下多路径，可能从内网或者公网访问过来，然后有些时候域名可能是 ddns 的，
> 这个也要考虑到，连接不到的时候都检测一下多路径的问题看着来。」

翻译成产品行为：

1. 同一条 NAS 连接可以配**多条访问路径**（例如家里 LAN `192.168.1.10:9834`、
   公网 DDNS `https://qnap.example.com:9834`、备用域名……）；
2. 按配置顺序自动挑可达的那条；一条挂了自动换下一条，**不重启 daemon、不丢会话**；
3. 优先路径恢复后自动切回（在家走 LAN、出门走 DDNS）；
4. **连不上时，每条路径的 DNS 解析、可达性、失败原因都能一眼看到**，
   而不是只有一句「网络错误」。

---

## 1. 配置模型

### 1.1 `LinkConfig` 新增字段

`~/.config/qxync/links/<id>.json`：

```json
{
  "id": "default",
  "host": "qnap.example.com",
  "port": 9834,
  "https": true,
  "insecure": true,
  "user": "test1",
  "endpoints": [
    { "label": "家里 LAN", "host": "192.168.1.10", "port": 9834 },
    { "label": "备用 DDNS", "host": "nas2.example.com", "https": true, "port": 9834 }
  ],
  "local_namespace": "qnap.example.com"
}
```

* `endpoints`：**额外路径**，按书写顺序**先于主地址**尝试；主地址自动追加到最后兜底。
  * 列表为空 → 只有主地址，行为与 v0.7.12 完全一致（旧配置零迁移）。
  * 列表里显式写了主地址 → 尊重它出现的位置（想「LAN 主、DDNS 备」就把它写第一行）。
  * 同 `(协议, 主机, 端口)` 去重，保留第一次出现。
  * 最多 **8 条**（含主地址）。
  * `port` / `https` 省略时**继承主地址**（不猜 8080/443，避免把 `http://` 悄悄指到错误端口）。
* `local_namespace`：第一次保存多路径时冻结的本地命名空间（`sync.db` / 缓存目录名）。
  之后即使主地址改了，本地状态目录也不会跟着变 —— 否则用户会看到「同步状态全丢」。
  旧配置没有该字段，继续走旧的 `sanitize(host)` 派生规则。

### 1.2 文本行语法（GUI textarea / 文档）

```
[标签 = ][http:// | https://] 主机 [:端口]
```

* 例：`家里 LAN = 192.168.1.10:9834`、`公网 DDNS = https://qnap.example.com:9834/`；
* 空行与 `#` 开头的行是注释；
* 允许尾部 `/`，拒绝路径 / 查询串 / 片段；
* IPv6 带端口必须写 `[fe80::1]:9834`；裸 IPv6 视为无端口；
* 解析在 `qxync-core`（`EndpointConfig::parse_line` / `parse_endpoint_lines`），
  GUI 与 CLI 共用同一套；保存时坏行**带行号报错**，不静默丢弃。

### 1.3 命名空间稳定性

主地址（`host`）继续承担两个角色：**凭据匹配锚点**与**旧命名空间派生源**。
启用多路径后不建议改主地址（GUI 有提示）；`local_namespace` 只兜住缓存/状态目录不被拆散。
真换了 NAS 请新建连接，而不是把旧连接的路径指过去 —— 后者会被身份守卫拦下（见 §4）。

---

## 2. 运行期路由规则

`qxync-client::route::Router`（进程内共享，daemon 的主客户端 / 引擎客户端 / 每个挂载点
共用同一个 `Arc`）。

| 规则 | 值 / 行为 |
|---|---|
| 顺序 | 配置顺序 = 优先级；额外路径在前、主地址兜底 |
| 初始选路 | 有多条路径时，登录前并行探测一次，选顺序上第一条可达的 |
| 失败切路 | 当前路径失败 → 标记失败并**立即**切到顺序上的下一条（不在冷却期的） |
| 并发失败 | 只有「失败的那条仍是 active」才切路；多个线程同时失败不会一次跳两条 |
| 冷却 | `30s × 2^(连续失败-1)`，上限 5 分钟；成功即清零 |
| 全部冷却 | 选「冷却最早到期」的那条，不把自己锁死 |
| 切回优先路径 | 在线巡检发现更靠前的路径**连续 2 次**探测成功才切回（防 LAN/WAN 抖动） |
| 巡检间隔 | 默认 60s（`QXNYC_ROUTE_PROBE_SECS` 可调，`0` 关闭）；离线时由 poller 短探活负责 |
| 探测超时 | 单条路径 3s 上限；探测专用 client 连接 2s / 总 3s |
| 业务连接超时 | 多路径时 8s（旧单路径仍是 20s）—— 一条被黑洞的路径不该拖 20 秒 |

一次成功（任意请求或探测）只更新该路径健康度，不会把 active 从别处拽回来；
切回只由巡检的「连续成功」规则完成。

---

## 3. 请求层：跨路径重试策略

所有请求的 URL 都来自 `Router::base_url()`（当前 active 路径）。不同操作的重试策略：

| 操作 | 跨路径重试 | 说明 |
|---|---|---|
| 登录（含 2SV） | 是，每路径一次 | 只对 `Transport` 重试；`Auth` / 业务 status 立即停（换路也没用） |
| 幂等 GET（get_list / stat / max_log / settime / probe） | 是，每路径一次 | 保留原有「对端提前关连接 / 反代 502」的同路径抖动重试 |
| 区间下载 / 整文件下载 | 是 | 整文件下载失败删 `.qxync-part` 从头来（不拼接残留分片） |
| `upload_bytes` / `upload_file` | 是 | 每次重开文件 / 克隆字节；服务端 `overwrite=1`，重放安全 |
| `upload_stream_sized` | 否 | 调用方给的 reader 不能重放；失败交给上传队列下一次尝试（届时已在新路径） |
| mkdir / rename / move / delete | 仅 `is_connect()` | 请求确定没发出去才重试，避免重复建目录 / 重复删除 |
| logout | 否 | 尽力而为 |

**DDNS 连接池刷新**：所有路径都失败后，如果目标是域名（不是 IP 字面量），重建一次
`reqwest::Client`（丢弃旧 DNS / 连接池）再试一次。原因：reqwest 的连接池按 `host:port`
复用，DDNS 换 IP 后池里通往旧 IP 的死连接会被一直取出 —— 不换 client 就永远解析不到新地址。
每次操作最多刷新一次。

---

## 4. NAS 身份守卫（数据安全）

多路径的前提是「所有路径指向同一台 NAS」。如果用户把另一台 NAS 的地址误加进路径列表，
客户端会正常登录、正常同步 —— 然后把 A 的改动写进 B。

* 第一次成功登录时把 `qbox_get_nas_uid` 的 `UID`（拿不到则 `MAC0`）记到
  `<state>/link-identity/<id>.json`（**运行期事实**，不进配置目录）；
* 之后每次登录（含切路后的重登）比对；不一致 → 该路径标记 `blocked` 并换下一条重登；
  全部不一致才报错（错误信息里给出期望/实际 UID，并提示「换 NAS 请新建连接」）；
* `subject`（主地址 `host:port`）变了 = 用户确实换了 NAS → 视为新设备覆盖；
* 接口拿不到 UID / MAC（老固件、字段缺失）→ 跳过校验并 WARN，**不误杀**；
* 指纹文件读写失败只告警，不阻塞登录。

`blocked` 的路径不会被探测「复活」，也不会被自动切回去。

---

## 5. 连通性与离线语义

* `Connectivity` 仍是「整台 NAS 通不通」的唯一判定；多路径的好坏由 `Router` 管；
* 离线探活从「只探当前路径」改成**并行探全部路径**：任一条通 → 在线，并选中它；
* 所有路径都失败才进离线（`Error::Transport`），恢复检测仍走原有短探活 + 退避；
* 探活结果写回路由表，`qxync routes` 不改一行代码就能看到最近一次每条路径的现场；
* 手动 `qxync login` 失败也会进连通性状态（否则 `status` 只能显示「未检测」）。

---

## 6. 可观测性

### 6.1 CLI

```bash
qxync routes                 # 只读路由表（零网络、离线也能看）
qxync routes --probe         # 立即并行探测全部路径（DNS + HTTP，≤3s）
qxync routes --json          # 脚本/验收用
qxync status                 # 多一行「路径：N 条（当前 …；x/y 可用）」
qxync login                  # 成功时报告所用路径；失败时自动打印每条路径的现场
```

输出示例（脱敏）：

```text
多路径    : 2 条（按顺序尝试；✓ = 当前，本次已探测）
   1     家里 LAN      http://192.168.1.10:9834      192.168.1.10〔内网〕         - ms  冷却中
        └ 冷却中：27 秒后可再试
        └ 最近错误：请求超时：deadline has elapsed
   2  ✓  主地址        https://qnap.example.com:9834  203.0.113.7〔公网〕      53 ms  正常
最近切换  : 12 秒前 从 192.168.1.10:9834 到 qnap.example.com:9834（原因 failure）
```

* **daemon 没在跑 / 老 daemon 不认识 `routes`** → 自动回落**直连探测**（不需要登录），
  这是「连不上时先查多路径」在没有 daemon 的机器上仍然可用的原因。
* 错误文案仍遵守 M30：不含完整 URL / sid / 远端路径；路径诊断里只出现 `host:port`
  与 DNS 结果。

### 6.2 IPC

* `Request::Routes { probe: bool }` → `RoutesData`（`active` / `endpoints[]` /
  `switches` / `last_switch` / `probed`）；
* `StatusData.routes: Option<RouteSummary>`（`active` / `healthy` / `total` /
  `last_switch`）；老 daemon 没有该字段 → `None`，界面不显示路径行；
* `LinkInfo.endpoints: Vec<EndpointInfo>`：GUI 的 `same_link` 用它判断「路径改了
  要不要重启 daemon」；老 daemon 缺字段按「只有主地址」处理。
* **不 bump `IPC_VERSION`**：只加字段/方法，老客户端忽略新字段。

### 6.3 GUI

* 设置 → 连接：新增「备选路径」textarea（语法见 §1.2），保存时按行提交；
* 诊断 → 新增「多路径」子页：路径表 + 刷新 + 「检测全部路径」+ 上次切换原因；
* `QXNYC_GUI_TAB=diag:routes` 可直达。

---

## 7. 兼容、边界与已知限制

| 场景 | 行为 |
|---|---|
| 旧配置（无 `endpoints`） | 路径、缓存命名空间、socket、凭据、错误文案全部不变 |
| 新字段序列化 | 空 `endpoints` / `None` 的 `local_namespace` **不写进 JSON**，旧文件不产生噪音 |
| 新 CLI + 老 daemon | `routes` 回落直连并提示；`status` 缺字段不显示路径行 |
| 新 daemon + 老 GUI | 不影响运行；`same_link` 最坏多重启一次 |
| 改路径配置 | 仍需重启 daemon 才生效（与现在改 host 一致）；GUI「保存并登录」会自动重启 |
| 运行期切路径 | 不需要重启，会话 sid 继续用；若服务端把 sid 绑接口，现有「auth 失败 → 重登」自愈 |
| 代理 | 仍作用于**全部**路径（含 LAN）；手动代理可能让 LAN 探测失真，建议 `Auto` + `NO_PROXY` |
| 每路径 TLS `insecure` | v1 不做，沿用 link 全局开关 |
| 系统 DNS 缓存 | 我们只能做到「失败后重建连接重新解析」；systemd-resolved/nscd 按 TTL 缓存，无法绕过 |
| DDNS 指向内网 / CGNAT | 诊断里标出「内网 / 运营商 NAT（100.64/10）」及提示；不自动禁用该路径 |
| 只有 AAAA 但开了「仅 IPv4」 | 诊断可见 IPv6 解析结果；连接按 `ipv4_only` 走，失败即冷却 |
| 路径指向另一台 NAS | UID 指纹拦截并 `blocked` |

**明确不做**：

* 链路聚合 / 负载均衡 / 同时多路传一个文件（始终单活路径）；
* 自动发现局域网地址、mDNS/UDP 扫描、自动生成 LAN endpoint；
* 公网 IP 查询 / DDNS 服务商 API 更新；
* 每路径独立代理、独立凭据、独立 TLS 校验；
* 与官方 Qsync LAN 二进制通道相关的任何改动（M7 自研 peer 不受影响）。

---

## 8. 测试

### 8.1 单测

* `qxync-core`：`EndpointConfig` 解析 / 继承 / 去重 / IPv6 方括号 / 8 条上限 /
  命名空间冻结 / 旧 JSON 兼容；`netpath` 的 IPv4/IPv6 作用域分类（含 CGNAT）。
* `qxync-client`：路由选择、并发失败只切一条、全部冷却取最早、连续 2 次成功切回、
  探测结果与 DNS 变化、blocked 不复活；假 NAS 上的登录 / GET / 上传 / 区间下载
  跨路径重试。
* `qxync-daemon`：NAS 身份指纹（首次记录 / 一致 / 不一致 / subject 变化 / 缺字段跳过）。
* `qxync-gui`：`m40_multipath_page_is_wired`（表单 textarea / 诊断 tab / 表格 /
  两个按钮 / `refreshRoutes` 接线）、`m40_link_input_endpoints_parse_preserve_clear_and_freeze_namespace`
  （解析 / 保留 / 清空 / 命名空间冻结 / `same_link` 感知路径变化）。

### 8.2 隔离 e2e

`xtask/tests/m40-multipath.sh`（进 CI，不需要真 NAS、不需要 `/dev/fuse`）：
用 Python 假 NAS + 隔离 XDG 验 6 组场景 —— 额外路径优先、自动切路、自动切回、
DDNS 风格主机名解析与分类、身份守卫拦下错误 NAS、旧配置行为不变、全路径不可达离线、
daemon 不在时直连回落。2026-10-27 实跑 **27/27**。

### 8.3 GUI

* 连接页：`#f-endpoints` 回填 / 保存（与 Rust 行语法逐字对齐；空 = 清空、缺省字段 =
  保留旧值）；保存结果里回显「访问路径 N 条」。
* 诊断 →「多路径」：摘要（当前路径 / 切换次数 / 上次切换原因）+ 七列表格
  （角色 / 标签 / 地址 / 状态 / DNS 解析〔分类〕 / 延迟 / 说明），当前路径行高亮；
  「刷新」零网络，「检测全部路径」= `probe:true`。
* `QXNYC_GUI_TAB=diag:routes` 直达；`xtask/tests/gui-matrix.sh` 的 TABS 已加入该目的地。

### 8.3 真机只读

`xtask/tests/m40-real-nas.sh`（env 门控，默认只读）：
真 DDNS + 真内网地址同时配置时，实测「内网路径解析为内网并超时、DDNS 解析为公网并可达、
active 选中 DDNS」；给测试账号时可再验一次登录。**实测结果记在
[`验收记录.md`](验收记录.md)**（不把真实公网 IP / 口令写进仓库）。

---

## 9. 涉及文件

| 层 | 文件 |
|---|---|
| core | `config.rs`（`EndpointConfig`/`Endpoint`/`link_namespace`）、`netpath.rs`（新）、`ipc.rs` |
| client | `route.rs`（新）、`lib.rs`（共享路由 / 探测 / 重试 / 登录逐路径 / DDNS 换池） |
| daemon | `daemon.rs`（共享路由 / 探活 / 登录守卫 / `routes` IPC / 在线巡检）、`identity.rs`（新） |
| CLI | `main.rs`（`qxync routes`、直连回落、status/login/links 展示） |
| GUI | `commands.rs`（`LinkInput.endpoints` / 命名空间 / `same_link`）、`ui/*` |
| 验收 | `xtask/tests/m40-multipath.sh`、`xtask/tests/m40-real-nas.sh`、`.github/workflows/ci.yml` |
