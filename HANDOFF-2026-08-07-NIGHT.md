# 接手文档 · 2026-08-07 夜

> **本文件是当前唯一派单依据。** 状态真相源仍是仓根 `STATUS.md`。
> 置信度标注贯穿全文：`[实测]` 我跑过命令 · `[代码]` 读码确认 · `[未验]` 推断，没测。

---

## 0. 🔴 先读这一节：根因已定，但止血靠改配置不靠改代码

**429 的根因是记账单位错配，不是端点、不是 region、不是协议形状。**

| | 记账单位 | 数字 |
|---|---|---|
| KiroStudio | 17 个凭据 × (`credentialRpmLimit` 100 × headroom 85%) | 以为有 **1445 RPM** |
| Kiro 上游 | **1 把 key = 1 个账号** | 实际 **1 个桶** |

`[实测]` owner 确认：那 17 个 `api_key` 号**同一把 key、同一 cloneGroup**（克隆分身）。
`[代码]` `family_key()` 对 `api_key` 返回 `cred:{id}` ⇒ 17 份各自独立成族，
健康分 / 冷却 / 熔断 / RPM **全部按份算，没有任何一层知道它们是同一个账号**。

⇒ 「十几个号，每号 10 RPM 也 429」= **170 RPM 砸一个桶**。
⇒ **加分身让它更糟**：每加一份，网关"容量"涨 85，上游桶一点没变。
⇒ 「以前 EU 500 RPM 不限流」= 那时是真的多个不同账号，N 个 key 就是 N 个桶。

### 三道闸全关，放大器全开 `[实测]`

```
inboundThrottleEnabled = False   ← 入站不削峰
cooldownEnabled        = False   ← 被限流的号立刻还能被选中
rateLimitEnabled       = False   ← 每号最小间隔不生效
```

`cooldownEnabled=False` 在这个场景下**特别致命**：17 份共用一个上游桶 ⇒ 一份被限流
意味着全部会被限流，而冷却关了 ⇒ 重试挨个试遍 17 份，**每次都撞同一个桶**。

---

## 1. 立刻止血（改面板/配置，不用 hotswap）

按见效速度排。**这三步不需要任何代码改动。**

| # | 动作 | 依据 |
|---|---|---|
| **①** | **分身数 17 → 1~2 份** | 17 份不提供任何真实容量，只提供 17 倍放大。**唯一一步就止血的动作** |
| **②** | `credentialRpmLimit` 按每账号设 ≈ `100 / N份` | `[实测]` kiro-rs 单号 103 RPM / 100% 成功 ⇒ 单账号舒适区 ≈ 100 RPM。当前 17×85=1445 是它的 14 倍 |
| **③** | `cooldownEnabled` 开回来 | 唯一能阻止「重试挨个撞同一个桶」的机制 |

⚠️ **第 ③ 条需 owner 拍板**：CLAUDE.md 里找不到关掉它的依据
（`rateLimitEnabled` 与 `inboundRpmAuto` 关掉都有明确记载的理由，**冷却没有**）。
可能有未记录的原因。

⚠️ `throttle-autotune.timer` 每 2 分钟按可用号数重算 `inboundTargetRpm`。
降分身后它会算出更低的值 —— 那是**正确**的方向，但要知道它会动。

### 验证方法（几分钟内能拿到因果）

降到 2 份后观察 5 分钟。**若 429 率大幅下降 ⇒ 下面 A/B 两个改动的方向被实测确认。**
这比再派多少 agent 都硬。

```bash
ssh ws-vps 'python3 - <<PY
import sqlite3,time,collections
con=sqlite3.connect("file:/opt/kirostudio/data/usage/traces.db?mode=ro",uri=True)
cut=int((time.time()-300)*1000)
rows=con.execute("select outcome,retries from traces where ts_ms>=?",(cut,)).fetchall()
print("近5min",len(rows),dict(collections.Counter(r[0] for r in rows)))
print("retries",dict(sorted(collections.Counter(r[1] or 0 for r in rows).items())))
PY'
```

---

## 2. 代码改动 A：`family_key` 对分身按 cloneGroup 收族

**状态：未做。** 这是止血之后的第一件代码事。

### 为什么选 cloneGroup 而不是解析上游 User ID

`cloneGroup` 已经在凭据里 ⇒ 不需要等号被封才学到身份、不需要网络调用。
而同组分身**定义上**就是同一把 key ⇒ 同一个上游账号。

（解析上游 User ID 那条路已探明：`[实测]` **429 的 body 不带 User ID**，
只有 403 suspend 带 ⇒ 映射表只能等号被封才建得出来，可达性差。
第二来源 `getUserUsageAndLimits` 的 `user_info.user_id`（`web_portal.rs:60`）
`[实测]` **零生产读者**，且需网络调用。两条都比 cloneGroup 绕。）

### 改法

`src/kiro/model/credentials.rs` 的 `family_key(id)`，**第三分支之前**插入：

```rust
// api_key 分身：同 cloneGroup 定义上就是同一把 key ⇒ 同一个上游账号。
// 不按族收敛会让 N 份分身各自独立算健康分/冷却/熔断，而上游只有一个桶。
if self.is_api_key_credential() {
    if let Some(g) = self.clone_group.as_deref() {
        let g = g.trim();
        if !g.is_empty() {
            return format!("clone:{g}");
        }
    }
}
// ③ 非 M365 或解析失败：各自独立成族
format!("cred:{id}")
```

⚠️ **`clone_group` 这个字段名我没验过** `[未验]`。动手前先确认准确名字与类型：
```bash
git -C /Users/dwgx/Documents/Project/KiroStudio show 7d955b49:src/kiro/model/credentials.rs \
  | grep -nE "clone_group|cloneGroup|clone_seq|copies"
```
serde 是 camelCase ⇒ JSON 里 `cloneGroup` 对应 Rust `clone_group`，但**要看到才算**。

### 必须配的反向测试（OVER-REACH 控制）

1. `cloneGroup` 为 `None` ⇒ **仍回退 `cred:{id}`**
2. `cloneGroup` 为空串 / 全空白 ⇒ **仍回退 `cred:{id}`**
3. 非 `api_key`（social/idc）带 `cloneGroup` ⇒ **不受影响**
4. 同 group 两份 ⇒ 返回**相同**的 `clone:{g}`

**把修复故意改成「无 group 就并族」，确认第 1、2 条变红。** 否则所有无 group 的
`ksk_` 号会被并成一族 = **整池连坐**，比不修更糟。

### A 解决什么、不解决什么

✅ 健康分 / 冷却 / 熔断按账号收敛（三者**都读** `family_key`）
❌ **不解决 RPM 预算超发** —— `[代码]` `RpmTracker` 是 `HashMap<u64>` 按凭据 id
（`scheduling.rs:64`，`record`/`count`/`counts_for` 全按 id），**不读 `family_key`**

⇒ RPM 那层要单独做，**而它碰选号热路径**（`token_manager.rs` 的硬门附近）。
仓库明确把这类列为高风险区（`p_avail` 批量化就因此被否决）。
**建议等 A 上线观察后再决定，不要一次动两处。**

---

## 3. 代码改动 B：重试预算跨轮共享 —— **需先验证是否已修**

**状态：可能已修，未验证。**

`[代码]` 我 2026-08-07 已把 `ABSOLUTE_MAX_TOTAL_RETRIES` 从 **12 改到 4**，
且 `round_retry_quota` 的实现是：

```rust
let remaining = ABSOLUTE_MAX_TOTAL_RETRIES.saturating_sub(attempts_before as usize);
```

即跨轮共享同一份总额度 ⇒ 最坏 **4 次**而非 48。

⚠️ **但我没独立验证 `attempts_before` 真的跨轮累加** `[未验]` ——
它依赖 `upstream_calls` 在正确位置递增（`provider.rs` 里 `upstream_calls += 1`
在 `send()` **之后**）。验法：

```bash
cd /tmp/cc-kg/wt-us
grep -n "upstream_calls" src/kiro/provider.rs
grep -n -A15 "fn round_retry_quota" src/kiro/provider.rs
# 找守卫测试
grep -rn "total_upstream_attempts_are_capped_per_request_not_per_round" src/
```

若发现 `attempts_before` 每轮被重置 ⇒ B 仍要修，修法是 provider.rs 注释里
「未修问题 ②」自己写好的那个（本轮配额 = `min(基础配额, 总额度 − 已用)`）。

---

## 4. 清单：P0 / P1 / P2 / P3

### P0 —— 已完成

| ID | 项 | 状态 | 证据 |
|---|---|---|---|
| P0-1 | CLI `origin` 由开关门控改为**无条件 `KIRO_CLI`** | ✅ 已 hotswap | `[实测]` 二进制断言 `KIRO_CLI`×2 |
| P0-2 | `optout` true→false、`amz-sdk-request` max=1→3、移除 `x-amzn-kiro-agent-mode` | ✅ 已 hotswap | `[实测]` 断言 `attempt=1; max=3` |
| P0-3 | `ABSOLUTE_MAX_TOTAL_RETRIES` 12→4、429 专属退避 1s→8s、`Connection: close` | ✅ 已 hotswap | `[代码]` |
| P0-4 | 那个「一次 429 → 冷却 1800s」的补丁 | ✅ 已回退（owner 做的） | `[实测]` `/proc/<pid>/exe` 逐字节不含 |
| P0-5 | **上游 trace 埋点**（`upstream_trace.rs`，43KB） | ⚠️ 代码在 worktree，**未部署** | `[代码]` 文件已建 |
| P0-6 | **User ID 解析器**（`user_id.rs`，16KB，11 测试全绿） | ⚠️ 同上 | `[实测]` 测试绿 |

### P1 —— 代码在 worktree，未部署未验收

| ID | 项 | 状态 | 备注 |
|---|---|---|---|
| P1-1 | 请求体 debug 日志脱敏（`handlers.rs:1512/2577`） | ⚠️ 已改+守卫测试 | 原来会把用户 prompt 落进**面板可读**的内存 ring |
| P1-2 | D2 `supports_thinking` 死字段 | ⚠️ 已改 | `openai/convert.rs` + `model_catalog.rs` |
| P1-3 | D3 未识别 tool type 日志 `debug!`→`info!` + 去重 | ⚠️ 已改 | 原来生产上一行不产出 |
| P1-4 | `_ => {}` 静默丢弃加日志 | ⚠️ 已改 | 原来连收到什么都不知道 |
| P1-5 | 用量管道丢弃计数器接出口 | ⚠️ 已改 | `dropped_count`/`written_count` 原**零读者** |
| P1-6 | 图片入站硬上限 | ⚠️ 已改 | GreyGunG 有 8 个上限，我们**一个都没有** |
| P1-7 | i18n `ZyphrZero` → `kiro.rs`（中/英/日） | ⚠️ 已改 | 需 `pnpm build` 才进二进制 |
| P1-8 | `cli.rs` 补 7 条测试 | ✅ verify agent 做了 | 🔴 见下「最该被抓的疏漏」 |

### P2 —— 未做，有明确修法

| ID | 项 | 依据 |
|---|---|---|
| **P2-A** | **`family_key` 按 cloneGroup 收族** | 本文档 §2。**止血后第一件** |
| P2-B | 验证/修 重试预算跨轮共享 | 本文档 §3 |
| P2-C | 客户端断连丢整条记录（`impl Drop` 兜底） | `[代码]` `emit_record`/`report_credits` 全在 `stream::unfold` 闭包内（流式 1687/1684、缓冲 2911/2908），`handlers.rs` 无 `impl Drop`。设计已出（`/tmp/cc-fix-20260807/B-disconnect-guard.md`），**实现被 503 打断** |
| P2-D | 0.7.46 未打 tag | `[实测]` 最新 tag `v0.7.45` ⇒ OTA 升不到当前版 |
| P2-E | `cli_origin_kiro_cli` 死字段清理 | 去掉门控后**无生产读者**。⚠️ 直接删要验旧 JSON 反序列化兼容 |
| P2-F | RPM 预算按族收口 | **碰选号热路径，高风险**。等 P2-A 上线观察后再决定 |

### P3 —— 需实验/需 owner 拍板

| ID | 项 | 卡在哪 |
|---|---|---|
| P3-1 | **D1 reasoning item 入站丢弃** | 🔴 **三家给出三个互斥答案且无一有抓包证据**：freebattle 说必须回传 signature / greygung 说必须全丢（回塞会 400）/ 我们现状是无条件 `<thinking>` 回塞。**选错会弄坏正在跑的流量** ⇒ 必须等 P0-5 trace 落地后用真实上游响应判定 |
| P3-2 | cache_read 口径 | 需 owner 拍板：停掉估算 / 保留+标注 / 移植 CacheMeter |
| P3-3 | IDE 端点是否迁 `q.*` | 我改过又**撤回**了（见下）。要重做需先补「IDE 协议在 `q.*` 上的实测」 |
| P3-4 | GreyGunG 的 `image_resize.rs`（763 行）完整移植 | 需引入 `image` crate（新依赖）+ CPU 密集操作进热路径。本轮只做了上限校验不做 resize |
| P3-5 | RPM 真实放大倍数 | 唯一算出的 2.9x **无效**（monitor 读的就是 traces，分子分母同源）⇒ 需独立计数源，P0-5 trace 可提供 |

---

## 5. 我这轮犯的错（写下来避免重犯）

### 🔴 最大的错：没查 keyhash 分布

我查了 `family_key` 的代码、端点、UA、重试放大、region 优先级链，还派了 20 个 agent
读四个仓库 —— **却没查「这些号是不是同一把 key」，而那是一条命令的事。**

更糟：我手里一直有那个数据。我几次查 `credentials.json` 都只看
`authMethod`/`apiRegion`/`disabled`，**从没聚合过 keyhash**。
而 `balance_cache_key` 就是按 `sha256_hex(kiro_api_key)` 分组的，
我读过它、还引用过它，却没想到拿它去聚合现有池子。

⇒ **教训：查"为什么容量不够"之前，先查"容量的分母是不是真的"。**

以后遇到号池类问题，第一条命令应该是：
```bash
ssh ws-vps 'python3 - <<PY
import json,hashlib,collections
d=json.load(open("/opt/kirostudio/data/credentials.json"))
i=d if isinstance(d,list) else d.get("credentials",[])
c=collections.Counter()
for x in i:
    k=x.get("kiroApiKey") or ""
    c[hashlib.sha256(k.encode()).hexdigest()[:12] if k else "no-key"]+=1
print("keyhash 分布:",dict(c))
print("cloneGroup 分布:",dict(collections.Counter(x.get("cloneGroup") for x in i)))
PY'
```

### 我做了件没依据的事，被 verify agent 抓到并已撤回

我把 `ide.rs` 的 host 从 `runtime.*.kiro.dev` 改成 `q.*.amazonaws.com`，
而**同一文件的注释正是在论证不要做这件事**，前置条件（「IDE 协议在 `q.*` 上的实测」）
至今未满足。

它对 US 问题**毫无帮助**（`ksk_` 走 CLI 端点，根本不经过 `ide.rs`），纯属顺手改动，
却埋了个「加 OAuth 号就爆」的雷。已撤回三处，注释改成事实记录。

⇒ **教训：改一个文件前先问「它在我要解决的那条链路上吗」。**

### verify agent 抓到的更该被抓的疏漏

`cli.rs` 的 `#[test]` 从基线 **15 个掉到 1 个** —— 我那 4 项 CLI 协议改动
（`optami`/`max=3`/去 `agent-mode`/`origin` 无条件化）**全仓零断言**，
改回任一项 1410 个测试照样全绿。

⇒ **教训：改了协议形状，就要检查守卫它的测试还在不在。**

### 一条假红守卫（本仓第 6 次同型）

`call_sites_must_not_log_raw_body` 红过一次，但**修复是完整的，守卫本身写错了**：
它数 `redact_request_body(&request_body` 的出现次数期望 2，
而**测试自己的 `format!` 里那段字面量也被数进去** ⇒ 实际 3。

它注释里自己写着「这个 needle 也必须运行时拼，写成字面量时会数到自己」——
**然后还是踩了**，因为 `redact_request_body(&` 这半截仍是字面量。

⇒ 本仓 111 处 `include_str!` 只有 14 处剔了注释/自身。**加源码级守卫必须剔两样：
注释行 + 测试自身的字面量。**

---

## 6. 工程环境的硬约束（逐条来自本轮实测）

1. **工作目录**：`/tmp/cc-kg/wt-us`（detached `7d955b4`）。
   ⚠️ 该基点是「2026-08-07 00:58 JST 的工作树快照」，**不是 master 祖先**，
   且主仓那 159 个未提交条目一直在变 ⇒ **worktree 四门绿不代表主仓绿**。
2. ⛔ 绝不改 `/Users/dwgx/Documents/Project/KiroStudio` 主树（别人的未提交改动）。
3. **禁止 `cargo fmt` / `rustfmt` 已存在文件** —— `[实测]` 单文件 861 插入/212 删除。
   且 `cargo fmt` **忽略路径参数扫全树**，要用 `rustfmt --check <单文件>`。
4. **禁止 heredoc 写文件** —— `[实测]` 让整个会话 Bash 永久静默且转录零记录。
5. macOS/zsh：无 `timeout`（是 `gtimeout`）；`--include='*.rs'` 必须加引号。
6. 新增配置项必须 `#[serde(default)]`（线上 config.json 是既有文件，缺 default 服务起不来）。
7. 四门一律 `--no-default-features`。基线**现读**（当前 1430 passed，每天涨）。
8. 前端测试**必须先 `cd admin-ui`** —— 在仓根跑会**静默 pass 0 个还报绿**。
9. **二进制身份只认 sha256 与 `/proc/<pid>/exe`**。查中文字面量用 Python 逐字节，
   `grep -a` 与 `strings | grep` **都会骗人**（`[实测]` 前者假阳性、后者切碎 UTF-8）。
10. 派 subagent **必须显式 `model: 'opus'`/`'sonnet'`** —— 继承会拿到带 `[1m]` 后缀的 ID。
11. 🔴 **subagent 必须边查边落盘**（第一步 Write 建文件，之后 Edit 追加）。
    `[实测]` 本轮 12 个 agent 里 **5 个被 503 打死**（全部来自 `k1ro.skiapi.dev`，
    即我们正在修的那个网关），共烧 **约 260 万 token**。
    **这条规矩救了全部三批** —— 代码与报告都在盘上。没它就是零产出。

---

## 7. 未部署的二进制

`[实测]` verify agent 出过一个 sha `205cc0bb...`，四门全绿
（`cargo test` 1430 / `clippy` 0 error / `tsc` exit 0 / 前端 37/37）。

⚠️ **但那个二进制已经过期** —— 我之后撤回了 `ide.rs` 三处 ⇒ **必须重建**。

⚠️ **零端到端实测**：四门证明不了 `origin=KIRO_CLI` 在真上游拿 200。
唯一的正面证据是早先 5 个 EU 号跑出的 1210/1214 成功、0 个 429。

部署流程用 `deploy/verified-deploy.sh`（本轮新写），它在上传前 / hotswap 后 /
`/proc/<pid>/exe` **三个时点各验一次内容断言** —— 补的正是「回退了但线上还在跑旧二进制」
那个缺口。

---

## 8. 落盘的研究资料（别重复劳动）

| 路径 | 内容 |
|---|---|
| `/tmp/cc-eco-20260807/repos/` | 6 份仓库审计（160KB）：ZyphrZero endpoint/rpm/anthropic/onboarding、Foxfishc endpoint/onboarding |
| `/tmp/cc-fix-20260807/D-greygung-harvest.md` | **49KB**，GreyGunG 逐项审计。含 `image_resize.rs` 8 个硬上限、工具名双向映射、防截断提示词注入 |
| `/tmp/cc-fix-20260807/VERIFY.md` | 304 行，四门全部原始输出 |
| `/tmp/cc-uid-20260807/step1-observe.md` | 20KB，User ID 解析器设计 + **429 body 不带 User ID** 这条关键局限 |
| `/tmp/cc-b2-20260807/` | 3 份实现报告（E/F/G） |
| `/tmp/cc-us-recon/{zyphrzero,foxfishc,mjyuan,greygung}` | 四个参考仓库的 clone |

⚠️ 这些在 `/tmp`，**重启会丢**。要长期保留得先拷出来。

---

## 9. 三条不要做（有依据）

1. **不要照抄 kiro.rs 的 region 链。** `[实测]` 它只看 `api_region` → 回退 `config.region`
   （= `us-east-1`），**完全不看 `auth_region`**。而线上 EU 号的字段是
   `apiRegion=None`/`region=None`/`authRegion=eu-central-1` ⇒ 抄了会把它们全打到 us-east-1。
   **我们多的那层 `auth_region` 回退正是它们能工作的原因。**
2. **不要抄 kiro.rs 把 `suspended` + `locked your account` 判永久封禁。**
   我们判临时限速（短冷却 + failover），有 2026-08-04 实测依据（两次把只是临时限速的号判死）。
   抄它，那个跑出 114 次成功的 US 号刚才就被永久禁用了。
3. **不要因为「429 多」就加号。** **加同账号的号会让它更糟** —— 这正是当前现象。
