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
git add -A -- src admin-ui build.rs Dockerfile                    # 统一快照命令：后端 + 前端（含未跟踪新文件）
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

🔴 **2026-08-16 最新线上：nbus（`38.244.34.15:8990`，systemd `kirostudio.service`）跑
W13 最终 build（sha256 88270616，build_sha=final，v1.1.1 + W2-W13 全部修复），`/healthz` pool=4 全绿，
详情以仓根 `STATUS.md` 为准。** skiapi（`143.20.230.62:673`）现在是**验证机**（Docker 验证循环），
不再是生产。更早的容器化过渡（skiapi-kirostudio 镜像 0.7.46 时代）与更早的
`kirostudio-update` 热替换流程**均已过期**，勿引用。

**当前部署流程（nbus，2026-08-15 定型）**：
🔴 **部署前先跑 `scripts/verify-snapshot.sh`**（快照完整核验：src/、admin-ui/ 下未跟踪新文件
必须已进快照；git archive 只含快照分支内容，untracked 漏进部署 = 前端组件缺失 BLOCKER，历史 ×2）。
快照命令统一为 `git read-tree HEAD && git add -A -- src admin-ui build.rs Dockerfile`（含 build.rs；
三份文档逐字一致，见 WORKFLOW §5）。
```bash
# 0) 快照 commit 短 sha：与验证循环的 C 同值（git archive 解包环境无 .git，必须显式传）
SHA=$(git rev-parse --short ci/verify-adaptive)
# 1) 验证机构建 release 二进制（含前端，Dockerfile frontend-builder 自动 build dist 内嵌）
#    --build-arg KIRO_BUILD_SHA=$SHA：healthz 的 build_sha 字段（B11），不传显示 "unknown"
ssh skiapi "cd /tmp/kiro-verify && docker build -f Dockerfile --target builder --build-arg KIRO_BUILD_SHA=$SHA -t kv:x . && \
  docker create --name kv-tmp kv:x && docker cp kv-tmp:/app/target/release/kirostudio /tmp/kirostudio-release && \
  docker rm kv-tmp"
# 2) 本机中转（skiapi 不能直连 nbus）
scp skiapi:/tmp/kirostudio-release /tmp/ && scp /tmp/kirostudio-release nbus:/tmp/kirostudio-new
# 3) nbus 备份 + 替换 + 重启 + 验证
ssh nbus 'cp /opt/kirostudio/kirostudio /opt/kirostudio/kirostudio.bak-<tag> && \
  mv /tmp/kirostudio-new /opt/kirostudio/kirostudio && chmod +x /opt/kirostudio/kirostudio && \
  systemctl restart kirostudio && sleep 5 && systemctl is-active kirostudio && \
  curl -s http://127.0.0.1:8990/healthz'
# 判定：sha256sum 一致 + active + /healthz ok:true pool_count=4 且 build_sha == $SHA + 4 通道 smoketest
```

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

### 🔴 34 个限流旋钮里只有 4 个真在限（2026-08-11 实测线上 config + 逐个核对代码路径）

用户问「外部 30 RPM 怎么被打成上游 1000 RPM」，逐层实读后的分类：

| 类别 | 数量 | 实情 |
|---|---|---|
| **死配置** | 14 | `absorb ×10`（线上 `Enabled=false`，且**吸收层不覆盖透传路径** —— 实测透传调用点 absorb 命中 **0**，而线上 100% 流量走透传 ⇒ 这 10 个旋钮全程无效）；`rate_limit ×4`（有实测依据不能开） |
| **真在限** | 4 | `ABSOLUTE_MAX_TOTAL_RETRIES=4`、`MAX_PASSTHROUGH_FAILOVER_HOPS=6`（**都是代码常量不是配置**）、`upstreamConcurrencyLimit=16`、`upstreamPerCredentialLimit=8` |
| 容量假数 | 3 | 见上一节 |
| 其余 | ~13 | 冷却细分、reserve、安全限流等 |

**三个语义陷阱**（名字与真实后果不对应，已加守卫
`throttle_semantic_traps_defaults_are_documented` 钉住）：

| 字段 | 名字看起来 | 真实后果 |
|---|---|---|
| `cooldownEnabled=false` | 「不用冷却功能」 | 429 过的号**不被跳过、立刻可重选** ⇒ 换号 = 原地打转 |
| `inboundQueueTimeoutPassthrough=true` | 「排队超时别拒绝」 | 整形层退化成**延迟器**（排队 5s 后放行）；前面有重试外挂时"放行"等于鼓励它立刻重发 ⇒ 整形反而是放大器的润滑剂 |
| `inboundRpmAuto`（代码默认 **true**，线上刻意 **false**） | 「自动调挺好」 | 内置 AIMD 是单向棘轮，锁死在下限（详见上文）。线上关掉是对的 |

**放大链**（用实读参数算，非估算）：

**⚠️ 先看这条：配置值上限 ≠ 实际放大。** 2026-08-11 曾按配置值推算「最坏 480×」
（`SWAP_MAX_ATTEMPTS=60` × 客户端 2× × 网关 4×），**当天实测 shield 日志（84261 行）推翻**：

| | 按配置推算 | 实测 |
|---|---|---|
| shield 每请求尝试 | 最坏 60 次 | **最大 7 次**（成功请求均 4.27、放弃的均 5.84） |
| 总放大 | 480× | **约 5.6×** |

原因：19579 次判定**全部落 `[passthrough]` 分支，零 `swap`/`cool`/`retry`**
⇒ `SWAP_MAX_ATTEMPTS=60` 从未被触及。真正生效的是 `MAX_ATTEMPTS=10` 配合
`MAX_BUDGET_SECS=30`，而最大只到 7 次说明**30s 预算先耗尽、次数上限用不到**。

⇒ **改 `SWAP_MAX_ATTEMPTS` 不会有任何效果**（死配置）。要压 shield 的放大，
动的应该是 `MAX_BUDGET_SECS`（当前 30s）。

复核命令（每次下结论前跑，别信本表）：
```bash
ssh skiapi 'docker logs skiapi-shield-k2cc 2>&1 | sed "s/\x1b\[[0-9;]*m//g" \
  | grep -oE "after [0-9]+ attempts" | grep -oE "[0-9]+" \
  | awk "{n++;s+=\$1;if(\$1>m)m=\$1} END{printf \"n=%d avg=%.2f max=%d\\n\",n,s/n,m}"'
ssh skiapi 'docker logs skiapi-shield-k2cc 2>&1 | grep -oE "\[(cool|swap|retry|auth|perm|passthrough)\]" | sort | uniq -c'
```

线上那两个陷阱的组合（`passthrough=true` + `cooldown=false`）**方向相同、都放开**，
所以外层放大能完整穿透（幅度见上表实测值，不是推算的 480×）。⇒ 2026-08-11 新增 `throttleProfile` 三档
（`shielded` / `direct` / `manual`）把这几个关键开关成组管住，
**默认 `manual` 不覆盖任何字段**（线上那 7 个受管字段全部显式写过，
无条件覆盖会改写生产配置；两条应用路径的差异见 `ThrottleProfile` 文档）。

### 🔴 改客户端可见的状态码或文案前，先 grep 仓外消费者（2026-08-11 实测）

`kiro_shield.py` 的 `classify()` **按 body 文案分类，不按状态码**，且只有
`verdict ∈ {cool, auth}` 才读我们的 `Retry-After`：

```python
if verdict in ("cool","auth"): delay = cool_delay(attempt, Retry-After)  # 听真值
else:                          delay = swap_delay(attempt)               # 本地阶梯
```

⇒ 吸收层/预算耗尽的 503 文案**必须含 `COOLING_MARKERS` 词**（现用「等容量」），
否则落 `retry` 兜底、我们算的 `Retry-After` 被整个丢弃、改走 20→60s 阶梯。
已加守卫 `absorb_503_body_must_carry_shield_cooling_marker`（实测删 marker 必红）。

核对命令：`ssh skiapi 'grep -A12 COOLING_MARKERS /opt/skiapi/services/kiro_shield.py'`

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
| `ARCHITECTURE.md` 抬头 | v0.4.0 | **v1.1.0**（`Cargo.toml`，2026-08-14 复核） |

两份 docs 已就地加警示抬头。`MODULES.md` 的**结构性**描述（谁调谁、职责划分）大体仍成立，
过期的是数字与个别断言。另外两份 docs 都**缺 `src/openai/`** 一节（4 个文件确实存在）。

---

## 项目定位

KiroStudio 是一个 **Anthropic Messages API 兼容的反向代理网关**，Rust / Axum 编写。
接收标准 Anthropic 格式请求 → 转换为 Kiro/AWS Q 的 AWS event-stream 二进制协议 → 把响应翻译回 Anthropic SSE。

- 单端口单二进制（react 前端 rust-embed 内嵌）
- 上游：`runtime.{region}.kiro.dev/generateAssistantResponse`
- 管理面板：`/admin`（React SPA），Admin API：`/api/admin/*`
- 当前版本：v1.1.1（Cargo.toml；前端 admin-ui 独立线 0.7.44）。**v1.1.1 已于 2026-08-15 发版**
  （origin + public，发版例外，OTA 目标即 public release）；OTA 面板仍未启用（`update.env`
  token 未填、自动检查默认关）。**注意**：release 产物落后工作树全部修复（W2-W13），
  线上 nbus 跑的是修复后 build（sha 88270616，build_sha=final）——发 v1.1.2 让 release 追上需 owner 决策。

## 工作流

**工作流规范在 `.opencode/WORKFLOW.md`**（2026-08-15 定稿，从 W1-W6 实战提炼）：
任务生命周期 9 步（需求澄清→目标→计划→执行→自 review→落实核验→主线 review→CI→文档同步）、
派发纪律（文件边界/并行上限/空返回重派）、守卫纪律（needle 拼接/注释不写字面量/删目标必红）、
落实核验机制（每波三态核对进 DONE.md）、文档六件套职责、参考仓使用纪律（只参考不照搬定义）。
所有会话（主会话 + subagent）按它执行。

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
- ⇒ **想确认代码能编译/测试通过，必须在 `skiapi` 服务器上用 Docker**。

**验证循环（2026-08-11 实测，当前唯一可用流程）**：仓库里**已没有
`Dockerfile.verify`**（旧文档记载已过期）。现用 `Dockerfile` 的 `builder`
target + 持久化 target-cache 卷：build 只编译、测试用 `docker run` 显式跑，
退出码来自 cargo 本身，不存在「测试被构建缓存吞掉」的坑。build 层缓存命中时
秒级出结果（依赖只编一次，增量改动 = 重编改动文件）。

```bash
# 1) 本地：快照（临时 index，绝不 git add/commit/checkout）→ scp
cd /Users/dwgx/Documents/WorkSpace/Project/kirostudio
export GIT_INDEX_FILE=/tmp/ci.index && rm -f "$GIT_INDEX_FILE"
git read-tree HEAD && git add -A -- src admin-ui build.rs Dockerfile    # 统一快照命令：后端 + 前端 + 构建文件（B11 起含 build.rs）
TREE=$(git write-tree)
C=$(git commit-tree "$TREE" -p HEAD -m ci)
git branch -f ci/verify-adaptive "$C"
unset GIT_INDEX_FILE
git archive --format=tar ci/verify-adaptive -o /tmp/kv.tar
# 1.5) 部署前新文件核验：src/、admin-ui/ 下未跟踪（??）文件必须已进快照（git archive 只含
#      快照分支内容，untracked 漏进部署 = 组件缺失 BLOCKER，历史 ×2）。缺失即失败退出。
scripts/verify-snapshot.sh /tmp/kv.tar
SHA=${C:0:7}
scp -q /tmp/kv.tar skiapi:/tmp/kv.tar

# 2) 服务器：解包 → 确认关键新文件在包内 → build → 显式跑全量测试
#    --build-arg KIRO_BUILD_SHA=$SHA：git archive 解包环境无 .git，build.rs 拿不到 commit，
#    必须显式传快照 commit 短 sha（不传则 healthz 显示 "unknown"）
ssh skiapi "cd /tmp/kiro-verify && rm -rf src && tar -xf /tmp/kv.tar && \
  ls admin-ui/src/components/error-messages-dialog.tsx >/dev/null && echo KEYFILE=ok && \
  docker build -f Dockerfile --target builder --build-arg KIRO_BUILD_SHA=$SHA -t kv:x . > /tmp/b.log 2>&1 && echo BUILD=ok && \
  docker run --rm -w /app -v /tmp/kiro-verify/target-cache:/app/target kv:x sh -c \"cargo test --no-default-features 2>&1 | tail -6\""

# 3) 新增/改过的测试必须按名单独再跑一次，确认真的执行了（而不是被 filter 吞掉）
ssh skiapi 'docker run --rm -w /app -v /tmp/kiro-verify/target-cache:/app/target kv:x sh -c \
  "cargo test --no-default-features <测试名> 2>&1 | tail -4"'
```

⚠️ **判定标准**：必须看到 `test result: ok. NNNN passed; 0 failed` 才算绿；
build 失败看 `/tmp/b.log`（`grep -E "error\[E|^error"` 拿行号）。
⚠️ `/tmp/kiro-verify` 在 VPS 重启后清空；重建依赖层约 5 分钟，之后走缓存。
⚠️ 前端（admin-ui）改动**不需要单独同步 dist**——Dockerfile 的 frontend-builder 阶段自动
构建并内嵌进二进制（nbus 是纯二进制部署）；验证时快照要 `git add -A -- src admin-ui build.rs Dockerfile`（B11 起含构建文件）。
⚠️ **面板日志级别独立于控制台**：LogBufferLayer（面板实时日志环形缓冲）固定 INFO，控制台
由 RUST_LOG 环境变量控制（nbus unit 写 `RUST_LOG=warn` 时终端精简、面板仍可见 info+）。
⚠️ **部署后验证**：二进制内嵌前端无法直接 ls 检查组件，以快照核验
（`scripts/verify-snapshot.sh`）+ 面板打开关键页/curl 前端资源为准。

**部署**（nbus 流程见上「部署到 VPS」节，skiapi hotswap 流程已废弃）：

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
8. **测试绿 ≠ 守卫有效。写完守卫必须实测"删掉目标它会不会红"**（2026-08-11 实测）。
   本轮靠这一步抓到全轮最隐蔽的 bug：函数内 `macro_rules!` 靠**捕获**外层局部变量
   （`if !explicit.contains(...)`），而宏卫生性让标识符在**定义处**语境解析、
   解析不到那个绑定 ⇒ **检查形同不存在** ⇒ 它唯一要守的契约（不覆盖用户显式配置）
   静默失效。全套测试当时是**绿的**，守卫在、被守护的逻辑是空的。
   ⇒ 函数内宏要用的外层变量**显式当参数传进去**，别靠捕获。
   ⇒ 破坏实验要**类型等价**（改成 `let x = ...; if let Some(_) = x` 这类），
   直接删字段会引入编译错误，测不到守卫本身。
9. **守卫用「找某标记第一次出现的位置」切分生产代码区时，注释里不能出现该标记的字面量**
   （2026-08-11 踩两次：`upstream_hops += 1` 与 `#[cfg(test)]`）。
   注释命中会让切分点提前、生产区被截断，守卫**静默变绿**。描述某个代码标记时刻意绕开它。
10. **改客户端可见的状态码/文案前，先 grep 仓外消费者**（见上文 shield `COOLING_MARKERS` 那节）。
    仓库内做对了不等于链路上做对了。
11. **配置值上限 ≠ 实际放大：读到一个上限值就推算后果 = 算空气**（2026-08-11 实测）。
    当天按 `SWAP_MAX_ATTEMPTS=60` 推算 shield「最坏 480×」并据此把它定为"整条链最大的
    单一放大源"、列成待改项；实测日志后推翻 —— 19579 次判定**全部落 `[passthrough]`
    分支、零 `swap`**，那个 60 从未被触及，真实放大**约 5.6×**（每请求最大 7 次）。
    ⇒ 判断一个配置的影响，先确认**它所在的那条分支实际有没有被走到**，再谈它的值。
    这与「容量口径是假的」是同一类错误的两种形态：一个是自乘出来的假数，
    一个是根本没生效的上限。两者都会让人对着不存在的瓶颈调参。

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
- **P1 移植甄别结论（2026-08-11，双向对比 Kiro-RS-Tool @795b9ca）**：6 项本仓已是超集
  （工具适配 / Write-Edit-Read / 半截 JSON / XML 泄漏过滤 / profileArn / 缓存方向），
  参考仓有吞字 bug 等，**不移植**。2 项已移植：
  - **native effort 映射**（`native_thinking_effort_enabled`，**默认关**）：往上游注
    `additionalModelRequestFields.output_config.effort`（参考仓实测只有它能触发
    reasoningContentEvent）。白名单 4 模型（opus-4.8/4.7 五档含 xhigh；4.6/sonnet-4.6
    四档），守卫钉住白名单⊆model_catalog。默认关是刻意的（未线上实测）。
  - **缓存 fingerprint 模拟器**（cache 链 Layer 3）：`anthropic/cache_fingerprint.rs`，
    纯内存（无持久化/后台线程），最长公共前缀命中 + 会话隔离（种子=完整 user_id）。
    签名剔除工具块漂移 id + JSON canonicalize（否则工具对话 read 恒 0）。

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
