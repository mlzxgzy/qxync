# 免责声明 / Disclaimer

[中文](#中文) · [English](#english)

---

## 中文

### 1. 性质：非官方、无关联、无利益

**qxync** 是一个**第三方、非官方的 QNAP Qsync Linux 客户端**，由个人出于兴趣开发。

- 与 **QNAP Systems, Inc.（威联通科技股份有限公司）** 及其关联公司**没有任何关系**：
  未获其授权、未受其委托、未与其合作、未受其资助。
- **无任何商业目的、无任何经济利益**：不销售、不含广告、不提供付费服务、
  不接收与本项目相关的赞助或报酬。
- 本项目**不是** QNAP 官方软件，也不代表 QNAP 的任何立场或观点。

### 2. 目的：为了在 Linux 上自用

作者的动机单一且已公开说明：在 Linux 上使用 QNAP NAS，希望有一个支持
「按需同步（on-demand）」的第三方客户端。本项目的目标就是这个客户端本身。

### 3. 实现方式与分析边界

本项目为**互操作性**目的而实现了一套与 QNAP 官方 Windows 客户端等价的客户端协议交互。
协议细节来自对作者**自行安装、合法获得**的软件副本所做的**静态阅读**
（PE 结构解析、导入/导出表读取、字符串提取、反汇编交叉引用）。

明确声明**未做**以下事情：

- ❌ 未运行该官方客户端程序
- ❌ 未进行任何网络抓包、代理拦截或流量解密
- ❌ 未绕过、未破解、未禁用任何技术保护措施（无许可证校验破解、无 DRM 规避、无签名伪造）
- ❌ 未修改、未重新打包、未重新分发 QNAP 的任何二进制文件
- ❌ 未获取、未使用任何他人的账号、凭据或私密数据

**本仓库不分发** QNAP 的二进制文件、安装包、反编译产物、大段原始字符串转储，
也**不包含**任何逆向分析报告的正文。仓库内的接口信息均为**事实性技术信息**
（URL 路径、参数名、状态码等），用于说明互操作性，且每一条都在真机上验证过。

### 4. 知识产权归属

- **QNAP**、**Qsync**、**myQNAPcloud**、**QID** 等名称与标识是 QNAP Systems, Inc.
  的商标或注册商标。
- 被分析软件的**全部著作权、商标权及其他一切权利，均归 QNAP Systems, Inc. 及其许可方所有**。
- 本项目**不主张**对被分析软件的任何权利，也**不授予**任何人对该软件的任何许可。
- 本项目中属于作者原创的部分（客户端源码、文档、验收脚本）按 **MIT OR Apache-2.0**
  双许可发布，详见 [`LICENSE-MIT`](LICENSE-MIT) / [`LICENSE-APACHE`](LICENSE-APACHE)。
  **该许可只覆盖本仓库的原创部分**，不代表对上述任何权利的放弃或让渡。

### 5. 凭据

本项目**不包含、不分发**任何第三方客户端凭据。所有凭据都只存在于**你自己的机器上**：
NAS 账号口令写在 `~/.config/qsync/credentials.json`（权限 `0600`），
且该文件不会被本项目上传到任何地方。

### 6. 无担保

本项目按「**现状**」提供，**不提供任何形式的明示或默示担保**，包括但不限于
准确性、完整性、适销性、特定用途适用性与不侵权担保。
**任何人依据本项目所作的任何决定与行为的风险，由其自行承担。**

> ⚠️ 这是一个会**读写你 NAS 上真实文件**的工具。作者已尽最大努力保证数据安全
> （见 README 的[两条铁则](README.md#两条铁则)，以及 [`docs/验收记录.md`](docs/验收记录.md) 中的真机验收），
> 但**请务必先在测试账号/测试目录上跑通再用于重要数据**。

### 7. 使用者的责任

- 请**仅对你拥有所有权或已获明确授权管理的设备与软件**使用本项目。
- 使用本项目时，请**遵守当地法律法规**、**QNAP 的服务条款与许可协议**，
  以及你所处司法辖区关于逆向工程、互操作性、计算机软件保护的相关规定。
- **请勿**将本项目用于未授权访问、绕过授权、批量侵权或其他违法用途。
- 各方对逆向工程的合法边界规定不同，**在分发本项目或基于它开发软件之前，
  请自行咨询专业法律意见**。

### 8. 侵权处理：通知即删

作者**无意侵犯任何人的合法权利**。若你认为本项目内容侵犯了你的权利，请通过
GitHub Issue 或仓库中提供的联系方式告知，并说明具体内容与权利依据。

**收到有效通知后，作者将立即删除相关内容或整个仓库，不寻求任何形式的法律对抗。**
本项目纯属兴趣学习，没有任何需要捍卫的经济利益。

---

## English

### 1. Nature: unofficial, unaffiliated, non-commercial

**qxync** is a **third-party, unofficial QNAP Qsync client for Linux**, built by an individual
as a hobby.

- It is **not affiliated with, authorized by, sponsored by, endorsed by, or connected to
  QNAP Systems, Inc.** in any way.
- It has **no commercial purpose and no financial interest**: nothing is sold, there is no
  advertising, no paid service, and no sponsorship or compensation of any kind is received.
- This is **not** official QNAP software and does not represent QNAP's positions.

### 2. Purpose: personal use on Linux

The motivation is narrow and disclosed: the author uses a QNAP NAS from Linux and wants a
third-party client with **on-demand sync**. That client is what this project is.

### 3. How it was built, and the boundaries of the analysis

To achieve **interoperability**, this project implements a client protocol equivalent to that
used by QNAP's official Windows client. The protocol details come from **static reading** of a
software copy the author installed and legitimately obtained (PE structure parsing,
import/export table reading, string extraction, disassembly cross-referencing).

Explicitly **NOT** done:

- ❌ The official client program was never executed
- ❌ No packet capture, proxy interception, or traffic decryption
- ❌ No technical protection measure was circumvented, cracked, or disabled
      (no license-check bypass, no DRM circumvention, no signature forgery)
- ❌ No QNAP binary was modified, repackaged, or redistributed
- ❌ No third-party credentials or private data were obtained or used

**This repository does not distribute** QNAP binaries, installers, decompilation output, or bulk
raw string dumps, and it **contains no reverse-engineering report text**. The interface
information in it is **factual technical information** (URL paths, parameter names, status codes)
provided to describe interoperability — and every item was verified against real hardware.

### 4. Intellectual property

- **QNAP**, **Qsync**, **myQNAPcloud**, and **QID** are trademarks or registered trademarks of
  QNAP Systems, Inc.
- **All copyright, trademark, and other rights in the analysed software belong to
  QNAP Systems, Inc. and its licensors.**
- This project claims **no rights** in that software and grants **no licence** to it.
- Original portions of this project (client source, documentation, acceptance scripts) are
  released under **MIT OR Apache-2.0** — see [`LICENSE-MIT`](LICENSE-MIT) /
  [`LICENSE-APACHE`](LICENSE-APACHE). **That licence covers only the original parts of this
  repository** and waives or transfers none of the rights above.

### 5. Credentials

This project **contains and distributes no third-party client credentials**. All credentials live
**only on your own machine**: your NAS account password is written to
`~/.config/qsync/credentials.json` (mode `0600`), and this project never uploads it anywhere.

### 6. No warranty

Provided **"AS IS"**, **without warranty of any kind**, express or implied, including but not
limited to accuracy, completeness, merchantability, fitness for a particular purpose, and
non-infringement. **You bear all risk for any decision or action taken based on this project.**

> ⚠️ This tool **reads and writes real files on your NAS**. The author has gone to considerable
> lengths to protect your data (see the [two iron rules](README.en.md#two-iron-rules) in the README
> and the real-hardware acceptance results in [`docs/验收记录.md`](docs/验收记录.md)), but
> **please try it on a test account/directory before trusting it with important data**.

### 7. Your responsibility

- Use this project **only for devices and software you own or are explicitly authorized to administer**.
- Comply with **applicable law**, **QNAP's terms of service and licence agreements**, and the rules
  on reverse engineering, interoperability, and software protection in your jurisdiction.
- **Do not** use this project for unauthorized access, circumvention of authorization, mass
  infringement, or any other unlawful purpose.
- The legal boundaries of reverse engineering differ by jurisdiction. **Consult a qualified lawyer
  before distributing this project or building software on top of it.**

### 8. Takedown on notice

The author **has no intention of infringing anyone's rights**. If you believe this project
infringes your rights, please open a GitHub Issue or use the contact information in the repository,
identifying the specific content and the basis of your claim.

**Upon a valid notice, the author will remove the relevant content — or the entire repository —
promptly, and will not mount any legal defence.** This is a hobby project; there is no economic
interest worth defending.
