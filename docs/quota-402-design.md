# QUOTA_EXHAUSTED_ALL → 402 改造设计（研究结论，未改代码）

> 状态：**研究完成（2026-08-15），结论 = 条件做（推荐做，小改）**。本文档只研究 + 产方案。
> **实施状态：未实施**（跨月恢复已做 W5；402 配套随 ISSUES (d) 排序，待 owner 排期）。
> 关联：`.opencode/ISSUES.md (d)`「QUOTA_EXHAUSTED_ALL→402（k2cc）」条目——该条目原登记「绑定跨月恢复决策」，
> 跨月恢复已做（W5，token_manager.rs:6445-6500），本文件评估 402 配套改造。
> 判据体系同构声明（ISSUES 原话）：我们 `quota_exhausted_all=1` 与 k2cc `QUOTA_EXHAUSTED_ALL_MARKER` 语义同构
> （都是「scope 内 100% 确认配额耗尽」的机器标记）。

## 1. 现状：现在 quota 耗尽映射成什么

### 1.1 判据链（从上游到客户端，四个环节）

**① endpoint 层判据**（`src/kiro/endpoint/mod.rs:325-347`）

```rust
const QUOTA_EXHAUSTED_REASONS: &[&str] =
    &["MONTHLY_REQUEST_COUNT", "OVERAGE_REQUEST_LIMIT_EXCEEDED"];
```

`default_is_monthly_request_limit` 认顶层 `reason` 与嵌套 `error.reason` 两个字面量 + 子串兜底。
**刻意不门控状态码**：上游实测（2026-08-05，6h 窗口）`402` 出现 **0 次**、`400 + OVERAGE_REQUEST_LIMIT_EXCEEDED` 出现 **564 次**——
上游已从 402 改用 400 表达额度耗尽。源码守卫 `quota_exhausted_must_not_be_gated_on_status_code`
（provider.rs:5089）钉死「不得用 `status == 402` 门控额度判定」。

**② provider 层处置**（`src/kiro/provider.rs:3501-3542`，位置在通用 400 分支之前）

- `is_monthly_request_limit` 命中 → `report_quota_exhausted`（禁号 + 落 `quota_exhausted_at` + 落盘）→ 换号 continue
- **最后一个号也耗尽、`!has_available`** → bail 带显式标记：

```rust
"{} API 请求失败（所有凭据已用尽）quota_exhausted_all=1: {} {}"
```

（provider.rs:3528。标记只信 `has_available == false` 才打——2026-08-10 收口，裸串不得冒充，测试
`quota_exhausted_all_marker_distinguishes_pool_wide_from_single_credential` 钉住）

**③ 吸收层分类**（`src/anthropic/handlers.rs:1466 absorb_class_of`）

`quota_exhausted_all=1` 串**天然不可吸收**：该 bail 串不带 `retry_after_secs=`、不带
`USER_REQUEST_RATE_EXCEEDED`/`Too many requests`（判据大小写敏感，status Display 的 `Too Many Requests`
不匹配小写 m）、不带 `temporarily is suspended` → `absorb_class_of` 落 `None` → 上抛。
配额耗尽重试无意义，现有行为已正确，**改造不需要动吸收层**（详见 §3.3）。

**④ HTTP 层翻译**（`src/anthropic/handlers.rs:1086-1182 translate_quota_subscription`，
经 `translate_upstream_error` 链，最终由 `map_provider_error:1539` 的第 10 分支调用）

| 分支 | 判据 | 现状状态码 | error_type | Retry-After |
|---|---|---|---|---|
| 全池配额耗尽 | `quota_exhausted_all=1`（handlers.rs:1110-1117） | **429** | `rate_limit_error` | 无 |
| 单号/未知范围配额 | 裸串 `MONTHLY_REQUEST_COUNT`/`QUOTA`（handlers.rs:1128-1135） | **429** | `rate_limit_error` | 无 |
| 订阅不覆盖 | `subscription_unsupported=1`（handlers.rs:1093） | 404 | `not_found_error` | 无 |
| 模型容量 | `MODEL_TEMPORARILY_UNAVAILABLE`/`INSUFFICIENT_MODEL_CAPACITY` | 503 | `overloaded_error` | 无 |

配额 429 **刻意不带 Retry-After**（`is_upstream_rate_limited` 判据注释 handlers.rs:932-933：「虽同为 429 但不该带
Retry-After（要等下个计费周期，给秒数会诱导客户端反复砸死号）」；测试 `test_quota_exhausted_stays_429_without_retry_after`
handlers.rs:5339 钉住）。

### 1.2 现状形态（改造要覆盖的两种出口）

- **形态 a（主路径）**：请求在池内有号可选 → 逐号尝试 → 全灭 → `quota_exhausted_all=1` bail → **429 无 Retry-After**
- **形态 b（次路径）**：请求进来时号已全禁（月末持续期）→ `acquire_context` 懒恢复（W5，当月内必失败）→
  NoCandidate bail（token_manager.rs:5103）`"所有凭据均已禁用（0/N）pool_permanently_exhausted=1 retry_after_secs=10"`
  → `map_provider_error` 全池冷却分支（handlers.rs:1678）→ **429 + Retry-After=10** → 客户端每 10s 退避重试直到下月。

**形态 b 的 bail 串不带 quota 特征**——只做形态 a 改造，月末的持续请求仍走 429 循环，402 的「让客户端停手」
目标只达成一半。k2cc 用 `describe_unavailable`（原因拆解串 + 全 quota 时打标记）覆盖了等价场景。

### 1.3 文案现状

- 全池：`"月度请求配额已耗尽（号池内所有凭据）。排障：①面板查看各凭据用量；②等待配额周期重置；③为号池补充新凭据。"`
- 单号兜底：`"请求配额已耗尽。排障：①面板查看各凭据用量，切到仍有额度的账号；②等待配额周期重置；③为号池补充新凭据。"`

## 2. 402 对客户端的行为（实测/源码级证据）

### 2.1 Anthropic 官方语义：402 = billing_error（重要）

[platform.claude.com/docs/en/api/errors](https://platform.claude.com/docs/en/api/errors) 官方错误表：
**402 - `billing_error`: There's an issue with your billing or payment information。**
→ 402 是 Anthropic 协议**合法**状态码，Claude Code / SDK 认识它，不是未知码。语义借用（k2cc 用法）：
「上游账号配额耗尽」≈「billing 问题」——客户端停手不重试，这正是目的。

### 2.2 anthropic-sdk-typescript（Claude Code 内嵌，源码证据）

`src/client.ts shouldRetry`（GitHub anthropics/anthropic-sdk-typescript）：重试集合 = 408 / 409 / 429 / **status >= 500**；
**402 不重试**（fall through return false）。SDK 对 402 生成 `BillingError` 类型，message 取响应体 `error.message`
（我们可控文案）。Claude Code 行为：**收到 402 立即停止该请求、不自动退避重试**，显示错误消息。

### 2.3 opencode（源码证据）

`packages/opencode/src/session/retry.ts retryable()`：
- `!isRetryable && status < 500 && !matchesRetryableMessage(...)` → 不重试
- 匹配模式：`/429|500|502|503|504|524/i`、rate limit 类、overloaded、timeout 类——**402 不在任何模式**，且我们的 402 文案不含这些字样 → **不重试，直接显示错误**。
- 注意：opencode 对 responseBody 含 `FreeUsageLimitError`/`GoUsageLimitError` 会弹订阅引导——我们不用这些字样，不触发。

### 2.4 curl / 通用 HTTP 客户端

无自动重试（除非脚本显式写）；402 响应无缓存头（动态生成错误体），不会被 4xx 缓存。

### 2.5 我们的下游自定义客户端 sub2api（本机源码证据）

- `openai_gateway_grok_405_test.go:20`：failoverCodes = {401, **402**, 403, 405, 429, 529, 500, 502, 503, 504}——**402 是账号级 failover + cooldown 触发码**（`TestResponsesGrok402FailoverCooldown`：402 的账号进冷却排除）
- 若 sub2api 全部上游账号都 402 → 走它自己的终态映射（`billingErrorDetails` 是 sub2api 对**自己用户配额**的映射，与消费上游 402 无关；其语义方向是「quota → 429+RA 让 SDK 退避」，因为它的是短窗口配额，与 Kiro 月度配额模型不同，**不需要向 sub2api 对齐**）
- **对 sub2api 的结论**：收到我们的 402 → 换号 + 冷却，行为正确；最终用户看到什么取决于 sub2api 全败后的映射（sub2api 侧登记，不在本改造范围）

### 2.6 外挂 kiro_shield.py（线上链路 Caddy → shield → KiroStudio）

`RETRYABLE={429,500,502,503,504}`，**4xx 不重试**（CLAUDE.md:244）→ 402 透传不重试。✓ 与现有「吸收层 503 承重文案」互不干扰（402 不含 `COOLING_MARKERS` 词，也不会被 shield 误分类进 cool/auth——它本来就不是吸收层语义）。

### 2.7 k2cc 实测效果

k2cc v2.9.6：`QUOTA_EXHAUSTED_ALL_MARKER`（token_manager.rs:812，`describe_unavailable` 全 quota 时打）→
`map_provider_error_with_context` 顶部 402 分支（**排 429 之前**，handlers.rs:121-133，错误类型 `quota_exceeded_error`）+ 两个守卫测试：
- `test_map_provider_error_quota_marker_returns_402`（标记 → 402）
- `test_map_provider_error_mixed_marker_and_429_prefers_402`（**H2 回归：402 分支必须排在 429 分支之前，混合串不被 429 抢先**）
- `test_map_provider_error_bare_monthly_request_count_does_not_trigger_402`（H1 回归：裸串不 402）
k2cc 的跨月恢复文档称「代价仅一次 402」：402 让客户端停手 → 跨月后首个请求触发恢复 → 成功。
k2cc 未公开发布 402 的量化效果数据，行为证据以源码 + 测试为准。

## 3. 改造点（文件:行号级）

### 3.1 主路径：map_provider_error 新增 402 分支

**位置**：`src/anthropic/handlers.rs` `map_provider_error` 内、`ABSORB_BUDGET_EXHAUSTED_MARKER` 分支
（:1596）之后、`inbound_admission_timeout`（:1635）之前。

理由：
1. **必须排在全池冷却（`parse_retry_after_secs` :1678）之前**——形态 b 改造后（§3.3）quota bail 串**带** `retry_after_secs=10`，顺序反了会被 429+RA 接走，402 失效。
2. **必须排在 `is_upstream_rate_limited`（:1730）之前**——k2cc H2 教训：混合串（429 字样 + quota 标记）不能被限流分支抢先。当前形态 a 的串不含限流字样，但这是防御性契约，用守卫钉住。
3. **必须排在 `is_upstream_temporarily_suspended`（:1759）之前**——同理，防止将来混合串回归。
4. 放在 absorb 分支之后：quota bail 与 absorb 标记互斥（不同 bail 串），且 absorb 分支的既有守卫
   `absorb_exhausted_branch_is_first_in_map_provider_error`（:5843）语义保持不破坏。

**新分支内容**（只认显式标记，k2cc 同范式）：

```rust
if err_str.contains("quota_exhausted_all=1") {
    return (
        StatusCode::PAYMENT_REQUIRED,
        Json(ErrorResponse::new(
            "billing_error",
            "号池内所有凭据的月度请求配额均已耗尽（当月内不可恢复）。\
             排障：①面板查看各凭据用量；②等待配额周期重置——跨月后自动恢复，无需人工介入；\
             ③为号池补充新凭据可立即恢复。",
        )),
    ).into_response();
}
```

要点：
- error_type 用 **`billing_error`**（Anthropic 官方 402 语义，SDK 生成 BillingError；k2cc 的自定义 `quota_exceeded_error` 非官方类型，不采纳）
- **不带 Retry-After**（给了就诱导客户端退避重试，违背「停手」目标；与现 429 无 RA 的口径一致）
- 文案必须写明「**跨月后自动恢复**」——W5 恢复机制已上线，这是 402 与跨月恢复配套的告知面

**同步处理 translate_quota_subscription 的 quota_exhausted_all=1 分支**（handlers.rs:1110-1117）：
改为同样返回 402（保持两处一致，防将来有人把顶层分支挪走时静默退化）。裸串分支（:1128）**保持 429 不变**
——裸串语义是「单号/未知范围」，402 必须只用于「全池确认耗尽」（k2cc H1 同款约束）。

### 3.2 顺序守卫（新增 2 个测试，k2cc H2 同款）

- 源码级守卫：`quota_exhausted_all=1` 分支必须排在 `parse_retry_after_secs` / `is_upstream_rate_limited` /
  `is_upstream_temporarily_suspended` 之前（复用 `absorb_exhausted_branch_is_first_in_map_provider_error` 的写法，
  needle 运行时拼接防 include_str! 自匹配）
- 运行时守卫：混合错误串 `"429 Too Many Requests … quota_exhausted_all=1"` → 断言 402

### 3.3 形态 b：NoCandidate 全池 quota 标记（token_manager.rs）

**改造**：`src/kiro/token_manager.rs` 的 NoCandidate 全禁分支（:5103，`pool_permanently_exhausted=1` 那处）
在 bail 前检查「Kiro 路径全部禁用号的原因是否全为 QuotaExceeded」，是则追加 `quota_exhausted_all=1` 标记：

```rust
// 新增 helper（entries 锁内可用）：
fn all_kiro_quota_exhausted(&self) -> bool {
    let entries = self.entries.lock();
    let kiro: Vec<_> = entries.iter()
        .filter(|e| !e.credentials.is_custom_api_credential())
        .collect();
    !kiro.is_empty()
        && kiro.iter().all(|e| {
            e.disabled && e.disabled_reason == Some(DisabledReason::QuotaExceeded)
        })
}
// bail 串改为：
"所有凭据均已禁用（0/{}）quota_exhausted_all=1 pool_permanently_exhausted=1 retry_after_secs={}"
```

要点与边界：
- **只改 :5103 一处**。:5097（any_healable=true 分支）不涉及——全池 QuotaExceeded 时无未禁用 custom_api、
  QuotaExceeded 又不在 `is_self_healable_reason`（:1726，只认 TooManyFailures/SuspiciousActivityAuto/TooManyRefreshFailures）
  → any_healable 恒 false → 恒走 :5103。:5198（刷新失败路径）不涉及——刷新失败禁用的是
  TooManyRefreshFailures/InvalidRefreshToken，非 QuotaExceeded。:4642（Token 获取用尽）不涉及——全池禁时
  select 立即返回 None，不会跑到 attempt 用尽。
- custom_api 混合场景（:5088 分支，Kiro 号全 quota + 有未禁用代挂）：保持现状 429。那些请求通常已走透传路径
  （custom_api 优先分流），bail 意味着透传也失败，429 合理。
- 标记串同时保留 `pool_permanently_exhausted=1`：吸收层继续不可吸收（`absorb_class_of` 先认
  `pool_permanently_exhausted=1` 返回 None，顺序在 retry_after_secs 之前，:1478）——两个标记共存不冲突。

**吸收层零改动**（关键论证）：quota 类当前天然不可吸收（§1.1③），改造后形态 b 串带两个标记，
`absorb_class_of` 的 `pool_permanently_exhausted=1` 分支（:1478）仍先行拒收。402 不需要也不应该进吸收层
——配额耗尽在单请求 45s 预算内重试无意义，现有「不可吸收」行为即正确。建议补一个测试钉住：
`absorb_class_of(形态a串) == None`（当前无此测试，行为是结构性的，值得显式化）。

### 3.4 跨月恢复配套（零代码改动，验证关系）

- W5 `recover_expired_quota_disables`（token_manager.rs:6445-6503）懒触发点：启动 + `acquire_context` 无候选
  （:4686-4691）。**402 让客户端停手 → 当月内零请求 → 零探测成本**（现形态 b 下客户端每 10s 退避重试骚扰一整月）。
- 跨月后首个请求：acquire_context 无候选 → 懒恢复 → 号复活 → 请求成功。**「402 停手 → 跨月自动回池」配套成立**
  （k2cc「代价仅一次 402」同款）。
- 已知窗口：月初 12h 缓冲（:6453，覆盖偏西时区）内跨月恢复被挡 → 客户端拿到 402 → 停手 → 12h 后用户再试成功。
  这是刻意的保守设计（宁可延迟恢复不可提前撞墙重禁用盖当月时间戳），402 语义下无新增成本（本来 429 也是失败）。
- 402 分支**不带 Retry-After** 也与恢复节奏一致：12h 缓冲不是「等几秒就好」，给秒数反而误导。

### 3.5 透传路径（零改动）

透传池（custom_api）对上游 402 已按 QuotaExhausted 处置（provider.rs:1863），错误体原样透传（M7）——
**上游 402 已经以 402 形态到达客户端**。Kiro 路径改造后，两条路径的 402 语义一致化（「额度/余额耗尽」），是改造的额外收益，无需动透传。

## 4. 风险

| 风险 | 评估 | 处置 |
|---|---|---|
| 客户端把 402 当 4xx 永久错误缓存 | **不成立**：错误响应无缓存头（动态生成），curl/代理不缓存 4xx 错误体；anthropic-sdk 的 shouldRetry 对 402 返回 false 是「不自动重试」而非「缓存」 | 无需处置 |
| 客户端显示「billing 问题」吓到用户（OpenClaw 类行为：显示余额用尽警告并停请求，openclaw issue #30484） | **存在**，但我们的主要客户端（Claude Code / opencode / sub2api）均正确停手；Claude Code 显示我们的 message 文案 | 文案写明「号池配额耗尽 / 跨月自动恢复」，避免用户误以为是自己的账单问题 |
| opencode 的订阅引导（FreeUsageLimitError/GoUsageLimitError）误触发 | 不成立：响应体不含这些字样 | 无需处置 |
| 混合串被前置 429 分支抢先，402 静默失效 | 当前形态 a 串不含限流字样，但形态 b 改造后带 retry_after_secs | §3.1 位置 + §3.2 守卫钉住（k2cc H2 同款） |
| 裸串误判 402（单号耗尽被说成全池） | 判据只认 provider 打的 `quota_exhausted_all=1`（`!has_available` 确认后才打），裸串保持 429 | 已有收口测试 + H1 同款新测试 |
| sub2api 全账号 402 后的终态映射 | sub2api 侧行为，不在本仓；402 已在其 failoverCodes（行为正确） | 登记 sub2api 侧（全败终态映射核对） |
| 404/429 之外的「生僻码」让某些客户端困惑 | 402 是 Anthropic 官方错误表成员（billing_error），非生僻码 | 文档记录 |
| 402 让监控/告警误报 | 网关内部告警（alerting）不依赖 HTTP 码；quota 禁用已有 `credential_disabled` 告警（token_manager.rs:6379） | 无需处置 |
| 前端面板依赖 429 判定 | 面板消费 admin API（cooldownReason 等），不消费 messages API 错误码 | 无影响 |

## 5. 结论

**条件做（推荐）**：跨月恢复已上线（W5），402 是配套的「停手信号」——现形态（429 无 RA + 形态 b 429+RA）下
客户端当月内持续退避重试（月末形态 b 每 10s 一次），改造后当月内零骚扰、跨月自动回池。判据体系已同构
（`quota_exhausted_all=1` vs k2cc 标记），主要客户端（Claude Code SDK / opencode / sub2api / kiro_shield）全部
「不重试 402 或正确处理」，风险可控。**不做**则维持现状也可运行（客户端骚扰是网关零上游成本的，只是客户侧噪声）——
所以是「条件做」：做的前提是确认主要流量是 Claude Code 系 + opencode（它们正确停手），且接受「402 显示为
billing 错误」的用户认知成本。

### 改动清单（文件:行号级）

| # | 文件 | 改动 | 工作量 |
|---|---|---|---|
| 1 | src/anthropic/handlers.rs | `map_provider_error` 新增 402 分支（:1596 之后 :1635 之前），认 `quota_exhausted_all=1` → 402 `billing_error` 无 Retry-After；文案含「跨月自动恢复」 | ~15 行 |
| 2 | src/anthropic/handlers.rs | `translate_quota_subscription` 的 `quota_exhausted_all=1` 分支（:1110-1117）429 → 402（一致性兜底）；裸串分支（:1128）保持 429 | ~5 行 |
| 3 | src/kiro/token_manager.rs | NoCandidate 全禁分支（:5103）前加 `all_kiro_quota_exhausted` helper + bail 串追加标记 | ~25 行 |
| 4 | 测试 | 见下 | ~8 个测试 |

### 测试策略

1. `test_map_provider_error_quota_marker_returns_402`（k2cc 同款：形态 a 串 → 402）
2. `test_map_provider_error_mixed_marker_and_429_prefers_402`（k2cc H2 同款：混合串 → 402）
3. `test_bare_monthly_request_count_stays_429`（k2cc H1 同款：裸串不 402）
4. 402 响应断言：error_type=billing_error、无 Retry-After 头
5. 源码守卫：402 分支必须排在 `parse_retry_after_secs` / `is_upstream_rate_limited` / `is_upstream_temporarily_suspended` 之前
6. token_manager 级：全池 QuotaExceeded 的 NoCandidate bail 带 `quota_exhausted_all=1`（+ 反例：全池 AccountSuspended 不带）
7. `absorb_class_of(形态 a 串) == None`（钉住 quota 不可吸收）
8. **更新既有测试**：`test_quota_exhausted_stays_429_without_retry_after`（:5339）与
   `test_translate_quota_exhausted`（:6208）——quota_exhausted_all=1 语义从 429 变 402，断言必须跟着改；
   裸串 429 语义不变的部分保留

### 自 review（兼容性论证）

- **与跨月恢复兼容**：懒恢复在 acquire_context（HTTP 翻译层更早的环节），402 分支不影响其触发；402 停手 → 当月零请求 → 跨月首请求恢复。12h 缓冲窗口内 402 停手无新增成本。
- **与吸收层兼容**：quota 类不可吸收（结构性），形态 b 串保留 `pool_permanently_exhausted=1`，`absorb_class_of` 先行拒收；402 分支在 absorb 标记分支之后，不破坏 `absorb_exhausted_branch_is_first_in_map_provider_error` 守卫（该守卫只约束 absorb 与更后分支的相对顺序）。
- **与错误翻译守卫兼容**：新增守卫只约束「402 在限流/冷却判定之前」，与现有守卫（absorb 最前、quota 不门控状态码、限流不吞配额）无冲突；`quota_exhausted_must_not_be_gated_on_status_code`（provider.rs:5089）不受影响——那守卫约束的是**上游判定**（不能按 402 门控），本次改的是**网关输出**（确认全池耗尽后输出 402），方向不同、互不触碰。
- **与透传路径兼容**：透传 402 原样透传不变，Kiro 路径新 402 使两路语义一致化。
- **与 kiro_shield 外挂兼容**：4xx 不重试，402 直接透传。
