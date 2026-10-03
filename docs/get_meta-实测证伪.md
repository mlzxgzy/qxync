# `get_meta` 实测证伪 —— 逆向报告里「最有价值的可抄资产」站不住

> 日期：2026-10-03　　对象：一台真实 NAS（域名/账号见本地 `docs/测试环境.local.md`，**不入库**）
> 自报版本：Qsync **5.1.1** / QPKG **5.0.0.7** / build **20260723**（File Station 6.0.5.7994 build 20260914）
> 结论一句话：**`get_meta` 与 `get_meta_profile` 都不可用，按需同步的元数据方案只能回退 `get_list` 递归。**

---

## 1. 被证伪的是哪条结论

逆向报告把这两个 func 列为本次解包**最有价值的可抄资产**：

> `get_meta` / `get_meta_profile` 是服务端已实现、但官方 File Station v5 文档中查不到的私有接口
> —— 这是本项目最有价值的可抄资产（证据强度：已确证）
> —— `Qsync_ELF逆向分析报告.md` §0 第 3 条、§11.2

并据此建议元数据层改用 `get_meta`（§12.1「修订方案（★优先实测）」）。

**实机结果：这两个接口不存在可用的调用方式。**

---

## 2. 实测矩阵

| func | `qsyncsrv.cgi` | `filemanager/utilRequest.cgi` | `qboxRequest.cgi` |
|---|---|---|---|
| `get_meta` | **HTTP 500**（Apache HTML 错误页） | `status: 20` | XML `authPassed` |
| `get_meta_profile` | **`status: 19`**（无数据字段） | `status: 20` | XML `authPassed` |
| 对照：`get_tree` | `200` 正常数据 | — | — |
| 对照：`get_list` | `200` 正常数据 | `status: 20` | — |
| 对照：**瞎编的** `func=zzz_not_a_func` | **`200` 空体** | **`status: 20`** | — |

---

## 3. 三条判据

### 3.1 `get_meta` 的 500 是 **CGI 自身崩溃**，不是参数名猜错

这是最容易被误判的一点，所以单独说明。**如果 500 是「参数不对」，正确做法是继续猜参数名**；
但这里不是，所以继续猜是浪费时间。

判据是**同一个端点上的对照组**：

| 调用 | 结果 | 说明 |
|---|---|---|
| `func=zzz_not_a_func`（**瞎编**） | `200`，响应体为空 | 未知 func 被正常消化，**没有 500** |
| `func=get_tree` | `200`，正常数据 | 分发到 handler 后能跑完 |
| `func=get_list&path=…` | `200`，正常数据 | 同上 |
| `func=get_meta`（各种参数） | **`500`** | 被匹配到了 handler，handler 内部崩了 |

→ 分发链路是**通的**，`get_meta` 确实存在于 func 表并被匹配；崩的是 handler 本身
（典型的「前置状态缺失就直接解引用空指针」）。

### 3.2 参数侧已穷举，不是猜漏了

16 种形态，**全部 500**：

```
无参
path= / folder= / folder_path= / share_path= / source_path= / dest_path= / share=
folder_number=1&folder0=<path>            （max_log 的逐目录查询风格）
path=&recursive=1  /  is_recursive=1  /  r=1  /  all=1  /  is_dir=1
path=&limit=50&start=0                   （get_list 的分页风格）
path=&get_detail=1  /  syncing_folder=1  /  all_log=1
path=/home（家目录根）
```

### 3.3 `get_meta_profile` 的 `status: 19` 是「未启用」，不是成功

响应体只有 `{"version":"", "build":"20260723", "status":19, "success":"true"}` ——
**没有任何数据字段**（无 `datas`、无 `meta`），且换任何参数都是同一个结果。
判为「不支持」而非「成功但结果为空」。

> 补充：服务端 `qbox_*` 私有路径上**唯一经逆向确证的业务 status 是 `8`**（未就绪）。
> `19` 不在任何已知码表里，无法进一步解释，只能确定它不是成功。

### 3.4 在 File Station 命名空间同样不存在

关键对照：`filemanager/utilRequest.cgi` 对**瞎编的 func 也回 `status: 20`**。
既然瞎编 func 也是 `status: 20`，那 `get_meta` 在这里回 `status: 20` 就**不携带任何信息**
—— 它和「不存在」无法区分，同样判为不存在。

（这条修正了报告 §3.1「三个入口共用同一套 `func=` 命名空间，入口可互换」的说法：
实际上 `.fcgi` 比 `.cgi` 少 101 个 func token，连 `get_tree` 都没有。
不过 qxync 打的是 `.cgi`，两者都是活入口，**不影响本项目**。）

---

## 4. 最可能的根因

- `qbox_get_syncing_folder_list` 实测：本机唯一登记的同步文件夹是 `/home/.Qsync`
  （`permission=2`），而测试目录 `/home/qxync-test` **不在同步登记内**。
- `qsyncsrv_metad` 只为**共享文件夹**生成 `{share}/.qsync/meta/`。
- → handler 找不到对应的 meta 目录就崩（500）。

这条推测能解释「为什么家目录路径上一律 500」，但**未经证实**（要在 NAS 上翻
`/var/log/log.qmeta` 才能确认）。因此不写进结论，只作为后续排查方向。

---

## 5. 对路线 B 的影响

报告 §12.1 的方案是：

```
元数据：func=get_meta（服务端已实现，未文档化）→ 优先尝试
        func=get_list&limit=&start=               → 降级方案
```

**实测后第一档必须删掉。** `get_meta` 不是「更快的路」，是死路 ——
留着只会让人以为有捷径而反复尝试。

**但按需同步本身的可行性没有被推翻**：

- 服务端 `qsyncsrv_metad` 生成的 `{share}/.qsync/meta/` **真实存在**
  （格式 `%s/:%llu:%llu` = 相对路径 : size : mtime，是"只同步元数据不传内容"的现成形态）；
- 只是**没有可用的 CGI 出口**。想用就得直读那个目录 ——
  需要 NAS 本地路径访问，不是纯 HTTP 客户端能覆盖的能力。
  这一点要等真正要做按需同步时再评估，本版本不动。

---

## 6. `get_list` 递归的真实成本

同一目录树（`/home/qxync-test`，7 项 / 4 个子目录）：

```text
get_list 递归: 11 次请求 / 17 个条目 / 11 个目录
单次往返 ≈ 157 ~ 430 ms（同一台 NAS，两次跑法波动较大）
```

**耗时几乎全在往返次数上，与条目数关系不大** —— 每个目录只列一次
（`get_list` 一页 200 就够，没触发翻页）。

推论：提速方向是**减少往返次数**（合并请求 / 复用映射 / 长轮询），
而不是减少返回的字节数。v0.4.0 的 M9「缓存优先 + 映射本地化」正是这个方向，
实测已把热 `ls` 从 0.50 s 降到 0.001 s。

---

## 7. 回归护栏

`crates/qxync-proto-test/tests/get_meta.rs` 固化了上述结论：

| 测试 | 作用 |
|---|---|
| `get_meta_is_not_usable` | 断言 `get_meta` 当前**不可用**。若哪天 NAS 升级后它可用了，测试会**红掉**并提示重写封装 —— 而不是让人悄悄继续走 `get_list` |
| `get_meta_profile_is_not_usable` | 固化 `status: 19` |
| `get_list_recursion_cost` | 成本基线，用于观察后续优化效果 |

> 注意：护栏断言的是「**不可用**」，所以刻意**没有**用 `#[should_panic]` ——
> 「可用」才是 bug，应当让测试红掉而不是悄悄通过。

复跑（需要真机）：

```bash
export QXNYC_TEST_HOST=<你的 NAS>
export QXNYC_TEST_PORT=9834
export QXNYC_TEST_USER=<账号>
export QXNYC_TEST_PASSWORD='<口令>'
export QXNYC_TEST_FIXTURE=/home/qxync-test
cargo test -p qxync-proto-test --test get_meta -- --ignored --nocapture --test-threads=1
```

---

## 8. 方法论备注

这次能证伪，靠的是一个很便宜的对照：**故意打一个不存在的 func**。

未知 func 返回空 `200`，而 `get_meta` 返回 `500` —— 这一个对比就区分了
「参数名错了」和「handler 崩了」两种完全不同的失败，
避免了把时间浪费在穷举参数上。

新加的 `Client::qsync_probe()` / `raw_get()` 就是为这类探针准备的：
按原始键值发请求、不做解析与 status 判定，先拿回原文再决定怎么封装。

---

*本文档所有数据来自真机实测，未使用任何估算值。域名、账号与口令均不在本文档内。*