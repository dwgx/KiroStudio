# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> 面向 AI 助手的项目导航文件。每次开始新任务前先读此文件。

---

## 🔴 第 0 步：先读 `STATUS.md`，接手执行再读 `docs/TAKEOVER.md`

**`STATUS.md`（仓根）是当前状态快照的入口。** `docs/TAKEOVER.md` 是本次审计后的**执行层交接**。
本文件是**长期约束**（推哪里、怎么构建、
硬约束、历史事故依据），`STATUS.md` 是**当下状态**（树里有什么没上线、线上跑什么、待做什么）。
两者分工不同，都要读，但顺序是 `STATUS.md` 先。

**不要**用本目录的 `HANDOFF-*` / `PLAN-*` / `TRACKING-*` / `OPEN-ISSUES-*` 判断当前状态 ——
它们是**过程记录**，已统一加了「历史档案」抬头。它们的价值在依据与推导过程，
**不在结论**（已确证含多条过期断言）。

### ⚠️ 本文件自己也会过期，尤其是配置值

下面「线上配置」那张表里的数字**记错过多次**（`credentialRpmLimit` 记过 85、记过 200、
现读 100；`inboundTargetRpm` 由 autotune 每 2 分钟自动调，同一天内从 65 变到 500）。
**用前一律现读** `ssh skiapi 'grep ... /opt/kirostudio/data/config.json'`。

这不是洁癖 —— 2026-08-06 夜实测发现，本节此前拿来当例证的那条断言**它自己就是写反的**：

> ❌ 原文写「写 `adminApiKey`、实际 `adminKey`」——**方向恰好相反，`adminApiKey` 才是对的。**

三处独立证据（2026-08-06 22:0x 实读）：

```
线上 /opt/kirostudio/data/config.json   adminApiKey = <set,41 chars>，且无 adminKey
src/model/config.rs:94                  pub admin_api_key + #[serde(rename_all="camelCase")]
/usr/local/bin/gateway-status:23        读 ['adminApiKey'] —— 且它工作正常（返回了号池数据）
/usr/local/bin/throttle-autotune:24     同样读 ['adminApiKey'] —— 每 2 分钟成功调用一次
```

**这条错误断言的教训比原例证更值钱**：它是"更正"本身引入的错误 —— 有人凭记忆把一条正确的
事实改成了错的，还写进了导航文件当依据。⇒ **更正一条断言时，要像验证原断言那样验证你的更正。**

同轮实测还证否了配套的那条归因：`kiro-ratelimit-monitor` 当时**连续跑了 2 天 1 小时、
`NRestarts=0`、`summary.json` 新鲜** —— 它**没有崩**。真实情况是那次重写把「写
`minutely.jsonl`」这个能力**删掉了**（脚本里 `minutely` 零命中），而 `minutely.jsonl` 的 mtime
恰好停在该进程的**启动时刻**，看起来像"崩在那一刻"。

⇒ 真正成立的教训只有一条，但它仍然重要：**`systemctl is-active` 只证明进程活着，不证明它在
产出数据。** 当时三层全绿（timer 在触发、`gateway-status` 只查进程不查数据新鲜度、面板读陈旧
快照），而分钟级趋势已经断了两天没人发现。**先修度量，再谈调参**，否则是在算空气。

---

## 🔴 先读这一节：代码推哪里

**`origin` = `dwgx/KiroStudio-skiapi`（private），是唯一开发仓库。**
**`public` = `dwgx/KiroStudio`（PUBLIC），已冻结，不要再推。**

```
origin  https://github.com/dwgx/KiroStudio-skiapi.git   ← 推这里
public  https://github.com/dwgx/KiroStudio.git          ← 不要推
```

`git push` 默认走 `origin`，即默认正确。**但凡显式写 remote 名，务必确认不是 `public`。**
公开仓库里已有历史代码，往它推新东西等于把私有改动公开出去——这一步不可逆。

### 工作区常有其他 AI 的未提交改动

这个仓库经常同时有多个会话在改，工作树里长期存在几十个未提交文件。因此：

- **禁止** `git checkout` / `git switch` / `git stash` / `git reset` / `git commit` / `git add`
- **禁止** 全仓 `cargo fmt`（历史事故：有会话跑了全仓 fmt，把别人整树的改动回退冲掉）
- 要提交/推送时，用 **git plumbing 在临时 index 里做快照**，不碰真实 index：

```bash
export GIT_INDEX_FILE=/tmp/snap.index && rm -f "$GIT_INDEX_FILE"
git read-tree HEAD
git add -A -- src admin-ui/src          # 只加你关心的路径
TREE=$(git write-tree)
C=$(git commit-tree $TREE -p origin/deploy/vps -m "...")
git branch -f deploy/vps $C
unset GIT_INDEX_FILE
git push -f origin deploy/vps
```

做完必须验证工作区没变：分支名与 `git status --porcelain | wc -l` 应与开始时一致。

### 分支用途

| 分支 | 用途 |
|---|---|
| `master` | 主线历史。`deploy-build.yml` 也放这里一份（`workflow_dispatch` 要求 workflow 存在于默认分支） |
| `fix/macos-support-and-critical-bugs` | 当前开发分支 |
| `deploy/vps` | 部署构建用。推这里 + 触发 Actions 即产出 VPS 二进制 |
| `backup/worktree-snapshot` | 工作区快照备份 |

### 部署到 VPS

🔴 **2026-08-08 起 KiroStudio 已容器化，部署方式在过渡中。** 新机（`143.20.230.62`）上
KiroStudio 跑在 Docker 容器 `skiapi-kirostudio`（镜像 `skiapi/kirostudio:local-0.7.46`，
数据在 `/opt/skiapi/data/kirostudio/`），sub2api 用 compose 服务名 `kirostudio:8990` 寻址。
旧的「Actions 出二进制 → scp → `kirostudio-update` 热替换」流程**已不适用**（那套是 systemd
二进制时代）。新机的 `kirostudio-update` 脚本仍是旧流程、会 `systemctl restart kirostudio`，
对容器无效——**容器化部署的正确流程（镜像 build+push+rollback）尚未定型**，改镜像前先核对
`/opt/skiapi/docker-compose.yml` 与重建规格 `<运维仓>/docs/11-rebuild-new62.md`
（运维仓有两份，见「线上部署信息」节）。

```bash
# 巡检（新机，SSH 端口 673，密钥登录；本机 ssh config 尚无别名，直接 -p 673）
ssh -p 673 root@143.20.230.62 'gateway-status'     # 全链路巡检：服务/号池容量/链路/证书/负载
ssh -p 673 root@143.20.230.62 'gateway-status brief'
# 旧机 .248 已停 KiroStudio；gw.skiapi.dev 的 DNS 尚未切到新机（见下「线上部署信息」）
```

### 线上部署信息

🔴 **2026-08-08 已迁移到 `143.20.230.62`（SSH 端口 673，root 密钥登录；本机别名
`ssh skiapi` 已建好可直接用）。** 旧机 `143.20.230.248` 的 KiroStudio 已停，
sub2api 仍在跑但不应再承载流量。**迁移尚未完全切完**：`gw.skiapi.dev` 的 DNS 仍指向旧机
`.248`，而 `api.skiapi.dev`（CDN）已回源到新机——切 DNS 前 `gw` 直连入口打到的是停机了的旧机。
新机 KiroStudio 号池当前 **0/4 可用（4 个凭据全禁用：2 manual / 1 passthroughFailed /
1 quotaExceeded）**，是所有请求必失败的根源，需重新上号或启用。

运维仓库：**`~/Documents/WorkSpace/ws-vps` 这个路径不存在**（旧文档遗留）。实际有
两份工作树内容一致、但 git 已分叉的仓库：`~/Documents/WorkSpace/Project/wsvps` 和
`~/Documents/WorkSpace/skiapi-server`。⚠️ **改服务器配置前先确认改哪一份**，否则会
出现「改了一份、推的是另一份」。两边都有 `docs/02-tuning.md`（调优决策与踩坑）、
`docs/10-migration-143.20.230.62.md`、`docs/11-rebuild-new62.md`、`secrets/`（gitignored）。

**所有服务器改动先改仓库再 scp 推送并提交**（compose 栈 `/opt/skiapi/docker-compose.yml`，
KiroStudio 数据 `/opt/skiapi/data/kirostudio/`）。

面板 `https://k1ro.skiapi.dev/admin`（Caddy basic_auth + KiroStudio `adminApiKey` 两道）。

### 线上配置参考：这些值曾有依据，使用前必须重读

> 本表不是当前线上快照；它保留运维背景和历史证据。当前线上只读结果、版本、hash 和
> `gateway-status brief` 以仓根 `STATUS.md` 为准，具体配置使用前必须从 VPS 现读。

生产上有几项刻意偏离代码默认值，改回去会造成真实故障。完整依据在
`<运维仓>/docs/02-tuning.md`（`Project/wsvps` 或 `skiapi-server`），这里只列结论：

| 配置 | 线上值 | 为什么不能改回 |
|---|---|---|
| `rateLimitEnabled` | **false** | 它的每号最小间隔（1000ms）会在 241ms 处踢开亲和绑定 → 每次换号 → prompt cache 全丢。而 `handlers.rs` 那条 5339 样本实测显示速率与 429 率相关性仅 +0.09，即它防不住风控却让缓存失效 |
| `inboundRpmAuto` | **false** | 内置 AIMD 是单向棘轮：429 就砍半，回升要 20s 静默 ×N，而实测每 6.4s 就有一次 429 → 单调下滑锁死在下限。实测卡在 30 RPM 而号池能跑 216 |
| `inboundTargetRpm` | 由脚本管（现 **133**） | `throttle-autotune.timer` 每 2 分钟按可用号数自动调，补号后无需人工干预。⚠️ 它算容量的口径是**假的**，见下节 |
| `rpmHardGateOverloadWait` | **true**（2026-08-03 实测线上值） | 与代码默认 `false` 相反。本条原先记的 false 已过期；改动前先查 `ws-vps/docs/02-tuning.md` 的依据 |
| `credentialRpmLimit` | **100**（2026-08-06 21:5x 实读 `config.json`） | 🔴 **这条记过 85、记过 200、现读到 100 —— 别信本表，用前现读。** 有效阈值 = 100 × headroom 85% = 85。⚠️ 由它算出的「池容量」是配置自乘的数、**不是实测**，见下节 |
| `inboundTargetRpm` | **500**（同上时点实读；由 `throttle-autotune` 每 2 分钟自动调） | 同日曾观测到 **65**（当时可用号跌到 1）—— 它随可用号数浮动，**任何写死的数都是快照**。autotune 工作正常，别手动覆盖 |
| `inboundThrottleEnabled` | **true** | ⚠️ 本条原写「整形设在 133 而实测单号峰值 144 ⇒ 什么都没限住」—— **那个 133 已过期**。2026-08-06 实测：整形 500 而池容量算出 1722，`None` 桶（未分配到号即被挡）占全池 **16.2%**、其中 95.6% 是 rate_limited ⇒ **整形确实在限，且是当前最大的单一挡量来源**。结论与原文相反 |
| 七项 `tool*` 容错 | 全开 | 含默认关的 `toolTruncationRecovery`（宁可整轮重试也不下发半截参数） |

`trustForwardedHeader` 保持 **false** 也是刻意的：sub2api 的透传白名单
（`gateway_service.go` 的 `allowedHeaders`）里没有 `X-Forwarded-For` 且不转发它，
所以开了也拿不到真实用户 IP；而 KiroStudio 看到的 client_ip 恒为服务器自身地址，
若按它配 IP 黑名单会一封封掉全部流量。

### 🔴 容量口径是假的（读任何限流配置前先看这条，2026-08-03 实测）

`throttle-autotune` 日志：「target=133 已接近建议值 133（池容量 **167**，可用 1 个）」。
那个 167 = `credentialRpmLimit` 200 × `rpmHeadroomFactor` 85%，**是配置自乘出来的数，
不是测出来的**。实测单号 RPM 峰值 144，其中 **17.2% 是 rate_limited**；文档里"干净吞吐
25~30"指的是「429 率为 0 时的 RPM」，两个口径不能混用。

后果：整形阈值 133 < 实测峰值 144 → 整形层**没限住任何东西**，而所有依赖 167 的自动调节
都在算空气。用户抱怨"配置根本没法调"的根因不是选项多，而是**一个关键数字是假的**。
改 `credentialRpmLimit` 前必须做控制实验，不要直接按 25~30 改（会把吞吐掐死一个数量级）。

### 线上真实链路里还有一个外挂（不在本仓库）

```
客户端 → Caddy(:443) → kiro_shield.py(:8993, 239 行 Python) → KiroStudio(:8990) → Kiro 上游
```

`kiro_shield.py` 是别人加的重试外挂：RETRYABLE={429,500,502,503,504}，4xx 不重试；
MAX_BUDGET=600s / MAX_ATTEMPTS=60；MIN_DELAY=**1.0**（号池 50ms 就能恢复它也睡满 1s）；
预算耗尽返 **503 而非 429**（Cursor 见 429 会掐会话）。它每次重试是**整请求重打 KiroStudio**，
而网关内部还有 12 次换号 → 真实放大上限 60×12。

累计实测：requests 22448 / retries 19226 / absorbed 1657 / gave_up 325 →
**11.6 次重试才救回 1 个请求**，且统计只在它自己进程内，面板完全看不见。
**不要擅自停它或改 Caddy 指向**（正在保护生产），切换顺序见 `TASK-BUILTIN-RETRY.md` 任务 C。

### SSH 登录（2026-08-10 复核：别名已建好，直接用）

「SSH 只能用密码」那节**已作废**，密钥登录早已恢复。

- **用 `ssh skiapi`**（`new-vps` 是同一台的别名，等价）。`ssh -G skiapi` 实测解析为
  `root@143.20.230.62:673`，密钥 `~/.ssh/id_ed25519` + `~/.ssh/id_mig_skiapi`。
  **不要用 `ws-vps`** —— 本机 ssh config 里没有这个别名，会直接连接失败。
  **不要引用 `id_ed25519_pcs_root`** —— 该文件不存在。
- **旧机** `143.20.230.248`：端口 30964，KiroStudio 已停，仅供迁移审计。本机无别名，
  要连就写全 `ssh -p 30964 root@143.20.230.248`。
- ufw 对 SSH 有限速（30s 6 连接），批量命令用 `ControlMaster`/`ControlPersist` 复用单条连接。

### VPS 上的运维脚本（优先用它们，别手写流程）

```bash
SSH_NEW='ssh -p 673 root@143.20.230.62'
$SSH_NEW 'gateway-status'          # 全链路巡检：服务/号池容量/五条链路/证书/负载（新机可用）
$SSH_NEW 'gateway-status brief'    # 一行汇总，可 diff
$SSH_NEW 'gateway-backup'          # 全量备份（PG + Redis + 凭据 + traces.db 一致性快照）
$SSH_NEW 'gateway-backup verify'   # 校验最新备份可恢复
# ⚠️ kirostudio-update 仍是旧的 systemd 二进制流程，容器化后无效，别用它
```

新机自动任务需在 `.62` 上重新核对（旧机 .248 的 timer 不再管理新栈）：
`gateway-backup` / `gateway-check` / `throttle-autotune` / `selfheal` / `cert-guard`。

### OTA「检查更新」按钮的前置条件

`update.rs` 已支持私有仓库（环境变量 `KIROSTUDIO_UPDATE_REPO` /
`KIROSTUDIO_UPDATE_TOKEN`，检测到令牌时**只走 GitHub 直连**、剔除所有第三方镜像，
避免把 PAT 交给 gh-proxy 之类的中间人）。

但线上 `/etc/kirostudio/update.env` 里的 token **目前为空**，所以面板按钮必然失败。
需要在 GitHub 网页手工建 fine-grained PAT（只勾 `dwgx/KiroStudio-skiapi`，
权限 Contents:read + Actions:read）填进去并重启服务。在此之前用
`kirostudio-update` 脚本更新，功能等价。

---

## 🔵 读代码先走 codegraph，不要一上来就 grep

**默认用全局 codegraph**（已为本仓建好索引，MCP `codegraph_explore` 或 CLI）：

```bash
codegraph explore "acquire_context"     # 一次调用给出相关符号逐字源码 + 调用路径
```

⚠️ **本仓另有一套自带索引 `tools/codegraph/`（2026-08-07 建），现在和全局工具冲突：**
`build_codegraph.py:31` 的输出目录 `OUT` 默认是 `.codegraph/`，而那里现在放的是全局
codegraph 的 `codegraph.db`（34 MB，daemon 正在读写）。**直接跑
`python3 tools/codegraph/build_codegraph.py` 会往运行中的 daemon 目录里倒文件。**

要用自带那套就先改道，别让它写 `.codegraph/`：

```bash
CODEGRAPH_DIR=.codegraph-legacy python3 tools/codegraph/build_codegraph.py
CODEGRAPH_DIR=.codegraph-legacy python3 tools/codegraph/cg.py stat
```

下面那套 `cg.py` 子命令（`sym` / `callers` / `path` / `tests` / `str`）都要带同样的
`CODEGRAPH_DIR=` 前缀。若不需要它独有的 `tests`（谁覆盖了这个符号）和诚实度标签，
用全局 `codegraph explore` 就够，不必碰这套。

**它比 grep 强的地方**：`callers/calls` 是解析过的调用边（不是名字字面匹配）、`tests` 直接
答"谁覆盖了它"、`stat` 给当前真实行数（本仓文档里的行数**全部过期**，见下）。

⚠️ **每条边都带诚实度标签，`[ambig]` 与 `[extern]` 不是结论**：

- `[exact]` 唯一解析，可采信
- `[ambig N]` 同名 N 处，**列出全部候选**，典型来源是 `dyn Trait` 派发
  （`endpoint.decorate_api` 到底走 Cli 还是 Ide 取决于凭据类型，索引给不出）
- `[extern]` 目标在仓外（std / crate / npm），**不等于"没人调用"**

完整边界（看不见宏展开 / 反射式分派 / 已修掉的两个假阳性）在
**`tools/codegraph/README.md`**，下结论前读它。那两个假阳性都是拿 grep 交叉验证才暴露的
（`tokio::spawn` 曾被解析成本仓 `refresh_loop::spawn`，凭空造出一条
`main -> spawn -> run_once`）—— **索引跑通 ≠ 结论对，关键结论至少交叉核对一条边。**

### 🔴 文档里的行数与「唯一端点」全部过期（2026-08-07 实测）

| 断言出处 | 文档写的 | 实测 |
|---|---|---|
| `ARCHITECTURE.md` / `MODULES.md` 抬头 | 约 35,800 行 Rust | **90,032**（差 2.5 倍） |
| `MODULES.md` token_manager.rs | 5239 | **14927** |
| `MODULES.md` stream.rs | 2599 | **9205** |
| `MODULES.md` service.rs | 1990 | **9163** |
| `MODULES.md` main.rs | 481 | **1139** |
| 本文件 + `ARCHITECTURE.md` §二 | ide 是「唯一已注册端点」 | **ide + cli 两个都注册**（`endpoint/mod.rs::build()`） |
| `ARCHITECTURE.md` 抬头 | v0.4.0 | **v0.7.46**（`Cargo.toml`） |

两份 docs 已就地加警示抬头。`MODULES.md` 的**结构性**描述（谁调谁、职责划分）大体仍成立，
过期的是数字与个别断言。另外两份 docs 都**缺 `src/openai/`** 一节（4 个文件确实存在）。

---

## 项目定位

KiroStudio 是一个 **Anthropic Messages API 兼容的反向代理网关**，Rust / Axum 编写。
接收标准 Anthropic 格式请求 → 转换为 Kiro/AWS Q 的 AWS event-stream 二进制协议 → 把响应翻译回 Anthropic SSE。

- 单端口单二进制（react 前端 rust-embed 内嵌）
- 上游：`runtime.{region}.kiro.dev/generateAssistantResponse`
- 管理面板：`/admin`（React SPA），Admin API：`/api/admin/*`
- 当前版本：v0.7.46（Cargo.toml）。⚠️ 0.7.46 **还没打 tag** → OTA 升不到这一版

## 仓库结构

```
src/
├── main.rs                   入口：19步初始化 + 优雅停机
├── model/
│   ├── config.rs             全局配置结构体（Config，serde camelCase）
│   └── arg.rs                CLI 参数（--config / --credentials）
├── common/
│   ├── security.rs           IP白名单/黑名单/每-IP限流/XFF最右段
│   ├── ssrf.rs               出站URL SSRF防护 + DNS固定（⚠ 见已知问题#1）
│   ├── auth.rs               API Key提取 + constant_time_eq
│   ├── fs_atomic.rs          原子写文件（temp→fsync→rename）
│   ├── health_marker.rs      OTA健康标记/crashloop回滚
│   ├── log_buffer.rs         内存环形日志缓冲（面板实时日志）
│   ├── recovery_metrics.rs   自愈可观测性计数器
│   └── secret_store.rs       credentials at-rest XChaCha20-Poly1305加密
├── kiro/
│   ├── token_manager.rs      ★ 多凭据管理+选号+刷新+亲和（~5200行，最大文件）
│   ├── provider.rs           核心代理：重试/故障转移/Client缓存/动态重试预算
│   ├── health.rs             AIMD熔断器 + EWMA健康分 + 族级连坐（⚠ 见已知问题#7）
│   ├── cooldown.rs           8种冷却原因 + 差异化时长
│   ├── rate_limiter.rs       每日/最小间隔/退避/抖动
│   ├── scheduling.rs         InflightGuard(RAII) + RpmTracker(60s滑窗)
│   ├── affinity.rs           会话亲和（session_id→credential_id，TTL）
│   ├── machine_id.rs         machineId生成/派生/撞车轮换
│   ├── overage.rs            超额开关（幂等+审计，单号显式请求）
│   ├── web_portal.rs         app.kiro.dev Web Portal客户端（rpc-v2-cbor）
│   ├── refresh_loop.rs       受管后台预刷新任务
│   ├── throttle.rs           入站整形令牌桶（AIMD RPM自动挡）
│   ├── regions.rs            AWS region白名单
│   ├── diagnosis.rs          凭据诊断
│   ├── passthrough.rs        透传模式
│   ├── auth/
│   │   ├── social.rs         OAuth PKCE（Cognito）
│   │   ├── idc.rs            AWS IAM Identity Center设备码
│   │   └── mod.rs
│   ├── endpoint/
│   │   ├── ide.rs            IdeEndpoint（kiro.dev）⚠️ 原写「唯一已注册端点」已证否
│   │   ├── cli.rs            CliEndpoint（q.{region}.amazonaws.com，ksk_ API Key 号）
│   │   └── mod.rs            KiroEndpoint trait；build() 里 ide + cli **两个**都注册
│   │                         （2026-08-07 实测 endpoint/mod.rs:27-31，与 PROTOCOL.md §3 一致）
│   ├── parser/               AWS event-stream二进制解码（双CRC32）
│   └── model/                凭据/请求/事件/用量限制数据结构
├── anthropic/
│   ├── converter.rs          ★ Anthropic→Kiro格式转换 + 环境噪音剥离（~2900行）
│   ├── stream.rs             ★ Kiro events→Anthropic SSE（流式+缓冲双ctx，~2600行）
│   ├── handlers.rs           请求入口+流式/非流式+WebSearch分派（⚠ 见已知问题#6）
│   ├── websearch.rs          MCP WebSearch
│   ├── compressor.rs         输入压缩（空白折叠+tool_result头尾截断）
│   ├── model_catalog.rs      模型列表
│   ├── middleware.rs         认证中间件
│   ├── router.rs             路由装配
│   └── types.rs              Anthropic API数据类型
├── admin/
│   ├── service.rs            业务逻辑核心（凭据CRUD/余额/配置/OTA任务）
│   ├── handlers.rs           HTTP处理器
│   ├── router.rs             Admin路由装配
│   ├── update.rs             OTA自更新（⚠ 见已知问题#2#3#4）
│   ├── usage_handlers.rs     用量查询+insights+SSE stream/live
│   ├── social_login.rs       Social OAuth上号
│   ├── idc_login.rs          IDC设备码上号
│   ├── external_idp_login.rs M365双段PKCE上号
│   ├── middleware.rs         Admin鉴权中间件
│   ├── error.rs              错误类型
│   └── types.rs              Admin API数据类型
├── usage/
│   ├── usage_stats.rs        JSONL+内存预聚合+设备/客户端识别（~1800行）
│   ├── record.rs             RequestRecord数据契约
│   ├── trace_db.rs           SQLite逐条持久化（rusqlite bundled）
│   └── pipeline.rs           异步管道（专用OS线程+SyncSender）
├── admin_ui/
│   ├── router.rs             rust-embed React SPA + 登录背景图代理（含SSRF防护）
│   └── mod.rs
├── openai/                   OpenAI兼容层
├── token.rs                  Token计算（本地估算+远程API）
├── http_client.rs            ProxyConfig + reqwest Client构建
├── tray.rs                   Windows系统托盘（仅Windows编译）
├── debug.rs                  调试工具
└── test.rs                   集成测试

admin-ui/                     React + Vite + Tailwind前端（编译期embed进二进制）
docs/                         架构/协议/部署文档
deploy/                       systemd/bluegreen/rollback脚本
```

## 关键设计原则

1. **单二进制** — 前端 rust-embed 内嵌，拷贝即跑
2. **原子选号** — 选号+inflight+1+rpm.record 在同一把 `parking_lot::Mutex` 临界区内完成，根治并发惊群
3. **双路径刷新** — 预刷新（后台定时）+ 按需刷新（请求热路径），每凭据独立刷新锁防重复刷（v0.7.43 起，原全局 `refresh_lock` 已拆分）
4. **三层热重载** — TIER1(ArcSwap原子镜像) / TIER2(后台任务abort+respawn) / TIER3(进程级Atomic镜像)
5. **用量管道用OS线程** — SQLite/fsync阻塞IO不跑在tokio worker上，try_send非阻塞入队
6. **族级连坐** — M365/AWS同租户账号共享 `family_key`，一号风控整族退避

## 配置文件

- `config.json` — 主配置（serde_json camelCase，首次启动自动生成）
- `credentials.json` — 凭据（单对象或数组格式，可选at-rest加密）
- 默认端口：`8080`（config.example.json 里是 `8990`）

## 开发约定

### Rust 风格
- 注释密度：中文注释为主，关键算法行内注释
- 错误处理：`anyhow::Result` for 业务逻辑，自定义 error types for HTTP响应
- 异步：`tokio::spawn` + `Arc<Mutex>` (parking_lot for sync, tokio::Mutex for async IO)
- 配置热重载字段必须同步更新三层热重载镜像

### 前端（admin-ui）
- React + TypeScript + Tailwind CSS + Vite
- `pnpm` 包管理
- 编译后通过 `rust-embed` 嵌入二进制

### 构建与测试
```bash
# ⚠️ 必须先构建前端：rust-embed 是**编译期**嵌入 admin-ui/dist，缺 dist 时 cargo 直接报
#    E0599 "no associated function named `get` found for struct `Asset`"（不是友好报错）。
#    dist 在 .gitignore 里，所以 fresh clone 后第一件事就是这句。
cd admin-ui && pnpm install --frozen-lockfile && pnpm build && cd ..

# ⚠️ 一律加 --no-default-features：Cargo.toml 的 default = ["native-tls"] 与出厂配置**相反**。
#    CI/Docker/Windows 出厂构建全部用 --no-default-features（纯 rustls）。
#    不加则本地测的是发布版根本走不到的那条 TLS 分支（历史测试盲区，release.yml 已加门禁）。
cargo build --release --no-default-features   # 生产构建（单二进制）
cargo test --no-default-features              # 全部测试（**仅**内联 #[cfg(test)]，见下）
cargo test --no-default-features <name>       # 运行单个测试（按名称过滤）
cargo clippy --no-default-features && cargo fmt   # 提交前静态检查 + 格式化
cargo run -- -c config.json --credentials credentials.json   # 本地运行
cd admin-ui && pnpm dev                       # 前端热更开发（改前端后需 pnpm build 才进二进制）

# macOS 交叉构建（CI 同参数）
cargo build --release --no-default-features --target aarch64-apple-darwin  # Apple Silicon
cargo build --release --no-default-features --target x86_64-apple-darwin   # Intel
```

> **`src/test.rs` 与 `src/debug.rs` 不参与编译**：`main.rs` 的 mod 列表里没有 `mod test;`/`mod debug;`，
> 两个文件都是孤儿，且已与现有 API 脱节（引用了不存在的 `TokenManager`、`decoder.frames_decoded()`）。
> 全部测试都是各源文件内联的 `#[cfg(test)]` 模块；“当前 982 个”是历史快照，不能作为当前测试数或全绿证明。
> 本次审计实际运行的是 `cargo test --no-default-features`，结果见 `STATUS.md`。改测试请改对应源文件，别动 `src/test.rs`。
Rust 2024 edition。提交遵循 Conventional Commits（中文描述，动词开头，无句号），禁止直推 `master`，详见 `CONTRIBUTING.md`。

### 本机编译与线上验证（2026-08-09 实测，很重要）

**本机（MacBook Air M2 / 8GB）编不过、也编不了**：
- `cargo build/test` 缺 `admin-ui/dist` → rust-embed E0599（与代码质量无关）
- `admin-ui` 无 `node_modules`（网络受限装不上 pnpm 依赖）
- ⇒ **想确认代码能编译/测试通过，必须在 `skiapi` 服务器上用 Docker**：

```bash
# 本地打快照 → scp → 服务器容器内编译验证（Dockerfile.verify 的 verify target = check+test+build）
export GIT_INDEX_FILE=/tmp/ci.index && rm -f "$GIT_INDEX_FILE"
git read-tree HEAD && git add -A -- src admin-ui/src admin-ui/tests
TREE=$(git write-tree)
C=$(git commit-tree "$TREE" -p HEAD -m "ci")
git branch -f ci/verify-adaptive "$C"
unset GIT_INDEX_FILE
git archive --format=tar ci/verify-adaptive -o /tmp/kv.tar
scp -q /tmp/kv.tar skiapi:/tmp/kv.tar
ssh skiapi 'mkdir -p /tmp/kiro-verify && cd /tmp/kiro-verify && rm -rf src admin-ui && tar -xf /tmp/kv.tar && docker build --no-cache -f Dockerfile.verify --target verify -t kv:x . > /tmp/b.log 2>&1; echo exit=$?; grep -E "test result|error\[E" /tmp/b.log | head'
```

⚠️ **必须 `--no-cache`**：缓存命中时不会真跑测试，`exit=0` 不代表通过
（Dockerfile 里 `cargo test | tail` 让退出码来自 `tail`）。一定要看到
`test result: ok. NNNN passed` 才算绿。

**部署**（后端）：
```bash
git push ssh://root@143.20.230.62:673/srv/git/KiroStudio-skiapi.git ci/verify-adaptive:refs/heads/deploy/adaptive -f
ssh skiapi 'cd /opt/kirostudio-src && git fetch origin && kirostudio-hotswap deploy origin/deploy/adaptive'
```

**改了前端还要单独同步 dist**（前端 dist 是 **bind mount**：
`/opt/kirostudio-src/admin-ui/dist:/app/ui-dist:ro`，二进制从磁盘读、优先于 rust-embed，
`hotswap deploy` 只换后端）：
```bash
# 服务器上用 node:22-alpine 构建 admin-ui 后 docker cp 出 dist 覆盖
```

**线上健康核实**：
```bash
ssh skiapi 'docker ps --filter name=kirostudio --format "{{.Image}} {{.Status}}"'
ssh skiapi 'DB=/opt/skiapi/data/kirostudio/usage/traces.db; sqlite3 "$DB" "SELECT outcome,COUNT(*) FROM (SELECT outcome FROM traces ORDER BY rowid DESC LIMIT 200) GROUP BY outcome;"'
```

### 排查时的坑（每条都踩过，别重蹈）

1. **`strings` 查编译产物验证改动是否上线 —— 不可靠**（编译优化掉字面量）。
   要验证改动是否在线，查**已部署快照源码**：`git show <tag>:<file> | grep`。
2. **本机 python 数括号平衡找语法错 —— 不可靠**（char 字面量/生命周期误判，
   15k 行文件上两种启发式给出互相矛盾的结果）。直接用服务器 CI 编译让 rustc 报行号。
3. **`git diff --stat <snapshot>` 对未跟踪文件显示为"删除"**（`endpoint_health.rs`
   `marquee-geometry.ts` 这类）。文件在本地且被引用就正常，别当真删除去"恢复"。
4. **测 KiroStudio 的 Kiro 路径会被 custom_api 代挂优先分流**（`should_try_custom_api_first`）。
   打 `/v1/messages` 测 WebSearch/Kiro 行为时请求实际走代挂（trace 的 `credential_id`
   是 `authMethod=custom_api` 的号），Kiro 路径日志零命中。要测 Kiro 路径须先禁用代挂。
5. **`hotswap status` 的 `FAIL docker compose up 失败` 是误报**（探测不存在的 `/health`）。
   核实看 `docker ps` 实际状态 + Admin API 是否 200。
6. **`kirostudio-hotswap deploy` 只换后端**，前端 dist 是 bind mount 必须单独同步
   （同前）。Vite 内容哈希相同说明源码没变，不是构建失败。
7. **workflow agent 的代码必须过 CI 才能信**。报告自称"逻辑自洽"的代码实际有
   5 类真实缺陷被 CI 抓出：`r#"..."#` 内容以引号结尾导致 raw string 提前闭合、
   `///` 文档注释用在函数参数（Rust 不允许）、cap 触发后重入死循环、
   截断结果超出契约、测试 helper 喂裸字符串给需要 JSON 对象的函数。

### 关键架构结论（三方对比 + 对抗评审确立，别推翻）

- **ksk region/端点**：OURS 已是最强（单一 region 真相源 `credentials.rs:544` +
  按区纠错自愈 + 含 region 的 bucket_key），**不要做 K2CC 式物化重写**（会亲手制造
  恒 403 + 桶 key 双斜杠漂移）。完整论证在 `docs/ARCHITECTURE-DECISIONS-2026-08-09.md`。
- **deepseek 归一化**：白名单感知 —— 选号与改写共用 `effective_model(raw, cfg, whitelist)`，
  原模型在白名单就保持原名，否则 fallback。改任何一处都要同步另一处，否则
  "选中了但改写后必失败"。
- **模型映射**（进行中，用户已拍板）：全局扁平 map + 每凭据豁免；用量记
  原始名 + 映射后名两维度；映射后不再判白名单（生态主流，豁免是安全阀）；
  先映射再 deepseek 归一化。

## 相关文档

- `docs/ARCHITECTURE.md` — 完整系统架构（推荐先读）
- `docs/PROTOCOL.md` — Kiro上游协议细节
- `docs/MODULES.md` — 模块说明
- `docs/INVALID-TOOL-PARAMETERS.md` — 工具参数修复原理
- `CHANGELOG.md` — 版本历史

### 缓存专题（三份配套，**从 EXP0-RESULT 开始读**）

- **`docs/CACHE-EXP0-RESULT.md`** — 实测结论 + 交接状态。**先读这份**：
  RFC 里每一层都依赖 EXP-0 的结论，而结论已经有了。已确证「上游不发 prompt cache 真值」
  （`metadataEvent` 只有 `stopReason`），并记录了一个更值钱的待办发现
  （`reasoningContentEvent` 被静默丢弃 = 结构化 thinking 流没被用）及其
  「为什么现在不做」的完整理由。
- `docs/CACHE-RESEARCH.md` — 生态调研（kiro2cc-proxy / kirocc / kiro-claude-bridge
  三家实测互相矛盾，按 entitlement 分档；vCache 语义缓存误差率；CacheProbe 的
  跨账号缓存共享威胁模型）。注意它对 `MetadataEvent.tokenUsage`
  「真信号可能一直在线上」的推断**已被 EXP-0 证否**。
- `docs/CACHE-RFC.md` — 分层落地方案（L0 前缀稳定 / L1 缓存感知调度 / L2 度量层 /
  L3 响应缓存），含 8 组实验矩阵、新增配置项汇总、三种分叉预案、风险登记册。
  ⚠️ 它文内多处仍把 EXP-1/EXP-2 与「47% 折扣」当待验前提，**那部分已过期**（见下条）。
- **`docs/prefix-stability-2026-08-06.md`** — ✅ **EXP-1/EXP-2 的核心问题已回答**
  （2026-08-05/06 用线上 **46 万条 traces** 判的，不是探针）。结论：
  **上游没有可观测的隐式前缀 credit 折扣 ⇒ 分叉 C 成立**。
  两条"矛盾"证据都收口了：「无隐式缓存」结论成立但原方法学是错的；
  **「47% 折扣」是不可复核的孤例观测**（单次、无样本数、落在正常噪声内 ——
  固定 model+input+output 后 credit 组内极差平均 49%）。
  ⇒ 保留估算的 `cache_read` 是对的（已有 `x-kirostudio-cache-estimated` 标注），
  但**别再用「47% 折扣」当任何决策依据**，也别再花探针预算重问「折扣是否存在」。

## 历史档案

已知问题的历史记录、与其他 kiro.rs 分支的对比、逐条 changelog、从生态学到的经验
都移到了 [`docs/HISTORY.md`](docs/HISTORY.md)（2026-08-10 从本文件拆出，331 行）。
那些是查档资料，不是当前待办 —— 需要追溯某个决定的来由时再读。
