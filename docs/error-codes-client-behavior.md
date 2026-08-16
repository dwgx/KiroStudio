# 下游客户端对错误码的行为（Claude Code / opencode / sub2api / 官方规范）

> 状态：**研究完成（2026-08-15）**，只研究不改代码。
> 用途：为「错误码可配置化设计」提供客户端行为事实与语义硬约束。
> 证据等级：本机源码（Claude Code 2.1.232 二进制内嵌 JS + anthropic-sdk 0.104.1）+ GitHub 源码（sst/opencode）+ 本机源码（sub2api）+ 官方文档（platform.claude.com/docs/en/api/errors）+ 参考仓源码（k2cc）。
> 关联：`docs/quota-402-design.md`（§2.2/§2.3 两处结论被本文档**修正**，见 §6）。

## 0. 一句话结论

- **状态码 + error.type + Retry-After 的组合是客户端的「决策输入」，必须锁死**；`message` 文案是「展示输入」，可自由配置。
- **Claude Code 2.1.232 对 402 + `billing_error` 会退避重试约 7 次（1 分钟）**——`quota-402-design.md` §2.2 的「402 不重试」结论在最新版 Claude Code 上**不成立**。k2cc 用非官方 `quota_exceeded_error` 恰好规避了这一点。
- 官方 gateway 契约（Claude Code 内嵌文档）推荐的「用户配额耗尽」姿势是 **429 + `billing_error` + `x-should-retry: false` + `anthropic-ratelimit-unified-*` 头**，但该姿势在 Claude Code CLI 层仍会被 Rzb={401,407,429,404,403,413} 强制重试——「停手」效果不如 402 + 非 billing_error type。
- 有一个**非标准但全客户端共识**的「客户端重试总开关」：`x-should-retry: true/false` 响应头（Claude Code SDK、opencode 不认，见 §2.3 的说明）。

---

## 1. Claude Code / anthropic-sdk（主要客户端，本机源码级证据）

### 1.1 证据源

| 来源 | 路径 | 内容 |
|---|---|---|
| Claude Code CLI 2.1.232（本机） | `/Users/dwgx/.local/share/mise/installs/node/24.19.0/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe`（Bun 编译二进制，JS 明文内嵌，已提取分析） | CLI 重试循环、错误分类（XLr/_Br/NhS）、429 显示、gateway 契约文档 |
| anthropic-sdk 0.104.1（Claude Code 内嵌） | `/Users/dwgx/Library/Application Support/Code/agent-host/sdk-cache/claude/0.3.220/darwin-arm64/node_modules/@anthropic-ai/sdk/client.js` | shouldRetry（:688-719）、retryRequest（:721-763）、APIError.generate（core/error.js） |
| Claude Code 2.0.15（npm 历史版） | `/tmp/cc-2.0.15/package/cli.js` | 旧版对照（无独立错误分类，行为随 SDK） |

### 1.2 双层重试结构（关键认知）

Claude Code 2.1.232 有**两层独立重试**：

1. **SDK 层**（内嵌 anthropic-sdk 的 `shouldRetry`）：`maxRetries` 默认 **2**，退避 `0.5s × 2^n` cap 8s + 25% jitter；`Retry-After` / `retry-after-ms` 头精确等待。
2. **CLI 层**（Claude Code 自己的重试生成器，二进制 0x1070ded0 区域）：`maxRetries` 默认 **10**（`pzb=10`；`CLAUDE_CODE_MAX_RETRIES` 环境变量可覆盖，clamp 15），退避 `pFe` = `500ms × 2^(attempt-1)` cap 300s + 25% jitter，**Retry-After > 60s（vzb=60000）直接放弃**（`api_request_retry_after_too_long`）。

**判定链**（CLI 层，`Dzb` 前置豁免 + `D` 判定）：

```
Dzb(e) 返回 true（SDK 判定可重试）→ 直接进入退避重试
Dzb(e) 返回 false → D 判定：
  D = status ∈ Rzb={401,407,429,404,403,413}（404 非流式请求除外）
    || error.type === "billing_error"
    || 消息匹配 xzb = [prompt_too_long, max_tokens_context_overflow,
         "credit balance is too low", "organization has been disabled",
         "Fast mode is not enabled", TDn]
  D=false → api_request_non_retryable，直接抛错（不重试）
```

### 1.3 SDK 层 shouldRetry 全集（0.104.1 + CC 2.1.232 内嵌增强版）

| 条件 | 行为 |
|---|---|
| `x-should-retry: true` | 重试（CC：非订阅或 enterprise 或无 unified 头时） |
| `x-should-retry: false` | **不重试**（CC 内嵌版：5xx 也返回 false——注意与 opencode 的「5xx 例外」不同） |
| 401（用了 token cache） | 失效缓存 + **重试一次**（触发重新认证的机器路径） |
| 403 + 消息含 "OAuth token has been revoked" | 重试（**消息内容影响重试决策**的例子） |
| 408 / 409 | 重试 |
| 429 | 重试，**除非**响应带 `anthropic-ratelimit-unified-representative-claim` / `-overage-status` / `-overage-disabled-reason` 头（`Nhp`/`Shp`，CC 2.1.232）→ 不重试 |
| 429 + 消息含 "usage credits are required"/"extra usage is required"（且 disabled-reason 非 fetch_error） | 不重试 |
| status ≥ 500 | 重试 |
| 其余（含 **402**） | **不重试** |

### 1.4 CLI 层重试全集（CC 2.1.232）

| 输入 | CLI 层行为 |
|---|---|
| 401/403（remote 模式） | 固定 1s 等待重试（`SDn`） |
| 401（登录态/OAuth） | 重试（token refresh 路径） |
| **429** | 必重试（Rzb 强制）：Retry-After 有值 → `max(Retry-After, 退避)`；>60s → 放弃；`unified-reset` 头 → 等到重置（cap 6h）；无头 → 指数退避 |
| **402 + `billing_error`** | **重试**（D 判定 `type==="billing_error"`）：约 7 次（0.5s 指数，第 8 次起 >60s 触发放弃），共约 1 分钟 |
| **402 + 非 billing_error type**（如 k2cc 的 `quota_exceeded_error`） | **不重试**（D=false → non_retryable） |
| 404（流式请求） | 重试（Rzb）；404 非流式 + message 含 '"model: ' → 模型切换提示（Mhp） |
| 403 | 重试（Rzb；revoked 场景 VUe 另计） |
| 413 | 重试（Rzb） |
| 529 / message 含 overloaded_error | 重试（xye） |
| 400 上下文超限（message 含 "input length and `max_tokens` exceed context limit"） | 重试（自动下调 max_tokens，Uhp） |
| 其他 4xx | 不重试 |

### 1.5 用户可见行为（显示）

| 状态码 / type | errorClass | 显示 | 我们的 message 是否展示 |
|---|---|---|---|
| 429 `rate_limit_error` | rate_limit | 订阅用户：优先解析响应体 `error.message` 显示；带 unified 头 → "You've hit your limit" 类文案（**丢弃 message**，除非带 `-overage-disabled-reason` 头）；非订阅用户 → "Request rejected (429) … temporary capacity issue" | 部分场景被丢弃 |
| **402 `billing_error`** | unknown | `API error: <message>`（KLr 取 message 原样显示） | **是** |
| 401 `authentication_error` | authentication_failed | "Please run /login · <message>"（非 gateway）；"Failed to authenticate. <message>"；OAuth refresh 失败 → "OAuth refresh token is no longer valid; run /login"；**不自动弹登录、不掐会话** | 是 |
| 403 `permission_error` | authentication_failed | 同 401 模板 + `<message>`；403+"api key authentication is disabled" 走专用分支；**不掐会话**（该轮请求失败，会话保留） | 是 |
| 404 `not_found_error` | model_not_found | 固定模板 "The model X is not available on your deployment…"（**丢弃 message**，除非 message 含 `"model: `） | 多数被丢弃 |
| 5xx `api_error` | server_error | `<message>. This is a server-side issue, usually temporary…" | 是 |
| 529 `overloaded_error` | overloaded | "The API is at capacity — this is usually temporary…" | message 部分展示 |
| 413 `request_too_large` | invalid_request | 专用处理（提示 compact/rewind） | 否 |
| 400 `invalid_request_error` | invalid_request | 按 message 内容细分（concurrency/duplicate tool_use/thinking 等） | 是 |

Hook `StopFailure`（`error` 字段）消费的 errorClass 枚举：`rate_limit / overloaded / authentication_failed / oauth_org_not_allowed / billing_error / invalid_request / model_not_found / server_error / max_output_tokens / unknown`。**Claude Code 自身的 errorClass 与 error.type 并不一一对应**：`_Br()` 只按状态码粗分（529/429/401/403/≥408 → 对应类），402 落 `unknown`——`billing_error` errorClass 只出现在 429+unified 头场景。

### 1.6 官方 Gateway 契约（CC 2.1.232 二进制内嵌文档，面向网关开发者的权威契约）

```
| 429 | rate_limit_error | Throttling; include Retry-After |
| 429 | billing_error    | The user's own cap on your gateway is reached; see Usage-limit headers |
| 401 | authentication_error | client prompts re-login |
| 529 | overloaded_error | Upstream at capacity; client backs off and retries |
| 5xx | api_error | Anything else |
```

**「用户配额耗尽」官方推荐姿势**（原样摘录）：

```
HTTP/1.1 429
retry-after: 37800
x-should-retry: false
anthropic-ratelimit-unified-status: rejected
anthropic-ratelimit-unified-reset: <unix 秒>
anthropic-ratelimit-unified-overage-reset: <unix 秒>
anthropic-ratelimit-unified-overage-utilization: 1
anthropic-ratelimit-unified-overage-surpassed-threshold: 1
anthropic-ratelimit-unified-overage-period: daily
anthropic-ratelimit-unified-overage-disabled-reason: org_spend_cap_reached
{"type":"error","error":{"type":"billing_error","message":"spend limit reached (daily; resets …) — request an increase at …"}}
```

要点（文档原文语义）：
- **带** `representative-claim`/`overage-status` → 客户端自行组合 "You've hit your limit" 并**丢弃你的 message**；**不带** → 原样打印 `error.message`（旧客户端加 "API Error" 前缀）。
- `retry-after` 秒数 = 到重置的时间；`x-should-retry: false` 阻止 SDK 重试。
- `anthropic-ratelimit-unified-*` 头（成功响应上）：75%/95% 阈值触发 "You've used NN% of your usage credits · resets …" 通知（CC 2.1.225+，gateway 登录态）。
- **注意**：该契约针对「网关 + 订阅用户」形态；我们网关的 API-key 用户走 429 时显示 "Request rejected (429)"（§1.5）。

### 1.7 其他观察

- `CLAUDE_CODE_MAX_RETRIES` 环境变量可调重试次数（clamp 15）。
- 429 的 `Retry-After` 数值**双向风险**：太大（>60s）→ CLI 层放弃；太小 → 高频重试。`unified-reset` 头则无 60s 限制（cap 6h）。
- 流式请求的 404 例外：`!(C.status===404&&r.isNonStreamingRequest)` —— 流式 404 也重试。

---

## 2. opencode（sst/opencode 当前版，GitHub 源码）

### 2.1 证据源

- `packages/opencode/src/session/retry.ts`（retryable/delay/policy）
- `packages/opencode/src/provider/error.ts`（parseAPICallError/message）

### 2.2 retryable 判定（retry.ts `retryable()`）

```
ContextOverflowError → 不重试
APIError：
  不重试，当且仅当：
    !isRetryable && status < 500
    && message 不匹配 RETRYABLE_MESSAGE_PATTERNS
    && responseBody 不匹配 RETRYABLE_MESSAGE_PATTERNS
  其中 isRetryable 来自 provider SDK（Anthropic 系 = SDK shouldRetry；OpenAI 系 = status===404 || isRetryable）
  RETRYABLE_MESSAGE_PATTERNS = /429|500|502|503|504|524/、rate limit 类、overloaded、
    timeout 类、network 类、"resource exhausted" 等
响应体含 "FreeUsageLimitError"/"GoUsageLimitError" → 订阅引导弹窗（GO_UPSELL / account_rate_limit）
```

→ **402 + 我们的文案（无上述字样）→ 不重试**（与 quota-402-design §2.3 结论一致）。文案若含 "exhausted"/"unavailable"（`lower.includes("exhausted")` 走非 APIError 分支）需注意——APIError 分支不受该子句影响，但保守起见仍建议避开。

### 2.3 delay（退避）

- `retry-after-ms` 头（毫秒）→ 精确等待（无 cap 上限值 2^31-1 ms）
- `retry-after` 头（秒 / HTTP-date）→ 精确等待
- 无头 → 指数退避 2s × 2^(n-1) + 25% jitter，cap 30s
- `RETRY_MAX_RETRIES = 5`

### 2.4 显示

- `error.ts message()`：优先响应体 `body.message || body.error || body.error?.message` → 我们的 `error.message` 原样显示（拼接为 "<status>: <errMsg>"）。
- 401/403 + HTML 响应体 → 专用提示（gateway/proxy 场景）。
- 5xx 永远重试（即使 SDK 未标 isRetryable）；402 不重试直接显示。

### 2.5 401 行为

- 不自动重试、不自动重新认证（provider 层 `isRetryable(401)=false`）；错误显示为 "Unauthorized" 类文案并提示 `opencode auth login <provider URL>`（error.ts 的 401 HTML 分支文案同款提示）。
- OAuth 刷新只发生在登录/授权流程（provider/auth.ts），不因 API 401 触发。

---

## 3. sub2api（下游计费网关，本机源码）

### 3.1 证据源

- `backend/internal/service/openai_gateway_upstream_errors.go:212-229`（shouldFailoverUpstreamError / shouldFailoverOpenAIUpstreamResponse）
- `backend/internal/handler/openai_gateway_credential_failover_loop_test.go:597-621`（TestResponsesGrok402FailoverCooldown）
- `backend/internal/handler/gateway_handler_responses.go:363-386`（handleResponsesFailoverExhausted）
- `backend/internal/handler/gateway_handler.go`（billingErrorDetails：下游自己的配额映射）

### 3.2 对上游错误码的行为

| 上游状态码 | sub2api 行为 |
|---|---|
| **401/402/403**/405/429/529 / ≥500 | **failover（换下一个健康账号）+ 账号 cooldown**（402 账号进冷却排除，后续请求跳过；测试断言 hits=[801,802,802]） |
| 400 / 404 / 408 / 422 | 不换号 |
| 413 | 换号（request body too large 专用路径） |
| 400/503 + 特定瞬态消息 | 换号（isOpenAITransientProcessingError） |
| 上下文超限错误 | 不换号（isOpenAIContextWindowError） |

### 3.3 全败终态

- 透传最后一个上游状态码（`lastErr.StatusCode`）+ OpenAI 格式 `"server_error"` + "All available accounts exhausted"；`copyFailoverRetryAfter` 透传上游 `Retry-After` 头。
- **sub2api 自己的用户配额** → `429 rate_limit_exceeded + Retry-After`（短窗口语义，与 Kiro 月度配额模型不同——quota-402-design §2.5 已结论「不需要向 sub2api 对齐」）。
- 对 Kiro 的结论：**收到我们的 402 → 换号 + 冷却，行为正确**；全败后把 402 透传给它的下游（OpenAI 兼容客户端对 402 通常不重试）。

---

## 4. OpenClaw / 其他 Claude Code 变体

- OpenClaw 无公开统一源码（openclaw.ai 生态、多 fork），客户端侧行为继承 anthropic-sdk：402 不重试（SDK shouldRetry false）、显示错误 message。
- **修正**：`quota-402-design.md` §4 引用的 "openclaw issue #30484" 实为 `anthropics/claude-code#30484`（skill 调用功能请求，与 402 无关）——该引用无效，OpenClaw 的 402 显示行为以「继承 SDK」为准，无独立证据。
- kiro_shield.py（线上链路）：`RETRYABLE={429,500,502,503,504}`，4xx 不重试 → 402 透传不重试（quota-402-design §2.6，保持）。

---

## 5. Anthropic 官方错误规范

来源：https://platform.claude.com/docs/en/api/errors （2026-08 抓取）

| 状态码 | error.type | 官方语义 | 客户端契约 |
|---|---|---|---|
| 400 | `invalid_request_error` | 请求格式/内容问题（**也可用于其他未列出的 4XX**） | 永久错误，不重试 |
| 401 | `authentication_error` | API key 问题 | 触发重新认证流程 |
| **402** | **`billing_error`** | **billing/支付信息问题** | 永久错误（官方 SDK 重试集合不含 402） |
| 403 | `permission_error` | 无权限 | 永久错误 |
| 404 | `not_found_error` | 资源不存在 | 永久错误 |
| 409 | `conflict_error` | 状态冲突 | 官方 SDK 重试（408/409） |
| 413 | `request_too_large` | 请求超限 | 永久错误（SDK 层不重试；CC CLI 层重试） |
| 429 | `rate_limit_error` | 命中限流 | 瞬态，SDK 指数退避重试（默认 2 次），honor `retry-after` |
| 500 | `api_error` | 服务端内部错误 | 瞬态，退避重试 |
| 504 | `timeout_error` | 处理超时 | 瞬态，退避重试 |
| 529 | `overloaded_error` | API 过载 | 瞬态，退避重试 |

官方 SDK 声明：「automatically retry transient failures (connection errors, rate limits, 5xx) with exponential backoff, **twice by default**, honoring the retry-after header」。

错误体形状：`{"type":"error","error":{"type":"...","message":"..."},"request_id":"req_..."}`。官方声明 type 值**可能随版本增长**（向后兼容扩展，不删除）。

---

## 6. 语义硬约束清单（绝对不能改 / 可以改）

### 6.1 锁死（改了客户端行为崩坏）

| # | 约束 | 理由（客户端行为） | 涉及分支 |
|---|---|---|---|
| H1 | **429 必须带 `rate_limit_error`；Retry-After 数值决定客户端行为**：≤60s → 精确等待重试；>60s → Claude Code 放弃（其余客户端照等）；无 → SDK 指数退避 | CC CLI 层 `max(Retry-After,退避)` + vzb=60s 放弃（§1.4）；SDK honor retry-after（§1.3） | 限流分支 |
| H2 | **429 配额分支不得带 Retry-After**（现状正确）或带「到重置的秒数」+ `x-should-retry: false` + unified 头（官方姿势）——二选一，不得给「几秒级」RA | 配额是分钟/月级窗口，给秒数诱导高频重试砸号（handlers.rs:932 注释同款论证） | quota 429 分支 |
| H3 | **503 必须带 `overloaded_error`**（含 `MODEL_TEMPORARILY_UNAVAILABLE` 等模型容量场景） | 客户端对 overloaded_error 必重试（xye/529 语义）；改成别的 type 会变成 non_retryable 硬失败 | 模型容量分支 |
| H4 | **401 必须是 `authentication_error`，绝不能变成 200/4xx 其他码** | SDK 401 → token cache invalidate + 重试一次 + 触发重新认证（§1.3）；改成别的码 → 客户端无法触发重新认证 | 认证分支 |
| H5 | **500/502/504 类必须 `api_error` 且 5xx** | 全客户端对 5xx 必重试（CC Rzb≥500 / SDK ≥500 / opencode 5xx 强制）；改成 4xx → 变永久失败 | 上游 5xx 分支 |
| H6 | **402（若采用）必须用非 `billing_error` 的 error.type**（k2cc 用 `quota_exceeded_error`），文案不得含 `credit balance is too low` / `organization has been disabled` 等 xzb 词 | CC CLI 层 `type==="billing_error"` → 重试 ~7 次（§1.4）；xzb 消息匹配 → 重试；二者都避开才停手 | 402 分支（改造待做） |
| H7 | **`message` 不得包含客户端「决策词」**：`credit balance is too low`、`organization has been disabled`、`usage credits are required`、`extra usage is required`、`OAuth token has been revoked`（403 场景）、`overloaded_error` 字样（非 529 时）、429/500/502/503/504 等数字字样 | CC xzb 匹配 / Dzb 豁免 / SDK VUe / opencode 模式匹配都会把 message 变成重试决策输入（§1.4、§2.2） | 所有文案 |
| H8 | **404 必须 `not_found_error`；文案不要含 `"model: ` 字样**（含了会触发 CC 模型切换提示） | CC 404 → model_not_found 模板（§1.5）；Mhp 检查 message 含 '"model: ' | 404 分支 |
| H9 | **吸收层（absorb_class_of）语义不动**：quota/限流类 bail 串不得携带可吸收特征词 | 吸收层判据大小写敏感、按串匹配；串特征变了吸收行为就变（handlers.rs:1466） | 吸收层 |
| H10 | **错误响应必须 JSON 形状 `{"type":"error","error":{...}}` 且恒有 `error.message`** | 全客户端解析该形状提取 message；SDK `APIError.makeMessage` 依赖 `error.message` | 所有错误 |
| H11 | **403 不得用于「配额/限流」语义**（sub2api 对 403 换号冷却，OpenAI 客户端 403=权限永久错误）；配额耗尽 429/402 语义不能被 403 顶替 | sub2api failover 集含 403（§3.2）；CC 403 → authentication_failed 提示 /login（§1.5） | 错误分类 |
| H12 | **SSE 流内 error event 的 error.type 与 HTTP 层一致** | CC 流式 404 例外（§1.7）；流内类型不一致会导致客户端分类错乱 | 流式错误 |

### 6.2 可自由改（客户端只展示不决策）

| # | 字段 | 说明 |
|---|---|---|
| S1 | `error.message` 文案 | 全客户端展示（CC 402/5xx/401 原样显示；429 订阅场景被丢弃属例外）；仅需避开 §6.1 H7 的决策词 |
| S2 | `request_id` / `request-id` 头 | 仅透传展示，无决策 |
| S3 | 429 响应中的 `x-ratelimit-*`（非 unified）头 | CC 仅读取展示（x-ratelimit-limit/remaining 等），不决策 |
| S4 | 500/503 的详细排障文案（含 `MODEL_TEMPORARILY_UNAVAILABLE` 原因串） | 客户端仅展示 |
| S5 | 404 的 message（**避开 `"model: ` 与 H7 词**） | CC 多数场景用模板丢弃 message，但避开决策词仍是硬前提 |
| S6 | 403 的 message 排障说明（**避开 "OAuth token has been revoked"**） | 同上 |

---

## 7. 对可配置化设计的约束（语义不变量 + 校验规则建议）

### 7.1 语义不变量（配置系统必须保证，任何配置组合不得破坏）

1. **码-class 绑定**：status ↔ error.type 只能在该 class 的合法集合内配：
   - `429 ∈ {rate_limit_error, billing_error}`（CC 官方契约两者都放 429 下；`billing_error` 配 429 时建议强制带 `x-should-retry: false`）
   - `402 ∈ {billing_error, 自定义类型（如 quota_exceeded_error）}` —— 配置 402 时必须警告「billing_error 会被 CC 重试约 7 次」
   - `503/529 ∈ {overloaded_error}`；`500/502/504 ∈ {api_error}`；`401 ∈ {authentication_error}`；`403 ∈ {permission_error}`；`404 ∈ {not_found_error}`；`400 ∈ {invalid_request_error}`
2. **Retry-After 域校验**：
   - 配了 429 且意图「退避重试」→ 必须可带 Retry-After ∈ [1,60]s（CC CLI 60s 放弃线）或省略（SDK 指数退避）
   - 配了 429 且意图「配额/停手」→ **禁止 Retry-After**（或强制 `x-should-retry: false` + unified 头 + RA=到重置）
   - 非 429 状态码配 Retry-After 是**语义污染**（SDK 只在重试路径读它）→ 校验拒绝或忽略
3. **5xx 不可配置为 4xx，反之亦然**（重试类别边界；H5）
4. **文案决策词黑名单**：任何 message 模板字段不得包含 H7 词表（校验时静态拒绝）
5. **SSE 流内 error 的 type 必须与 HTTP 错误同表校验**（H12）
6. **吸收层 bail 串标记不得可配**（`retry_after_secs=`、`quota_exhausted_all=1`、`pool_permanently_exhausted=1` 等内部标记是代码契约，不是用户配置）

### 7.2 建议的配置面（安全可配）

| 配置项 | 取值域 | 默认（现状） | 理由 |
|---|---|---|---|
| 各分支的 `error.message` 文案 | 自由文本（过 H7 黑名单） | 现有文案 | S1 |
| 429 是否带 Retry-After 及数值 | 无 / 1-60s / 「到重置」+unified 头 | 限流分支现状 | H1/H2 |
| 配额耗尽的输出形态 | `429 无RA`（现状）／`429+RA到重置+unified+x-should-retry:false`（官方姿势）／`402+quota_exceeded_error`（k2cc 姿势） | 429 无 RA | H6（选 402 必须非 billing_error） |
| 503 模型容量文案 | 自由文本 | 现有 | S4 |
| `x-should-retry` 头开关（对 429 配额分支） | true/false/省略 | 省略 | 官方契约成员（H2） |

### 7.3 必须锁死的字段（禁止进配置）

| 字段 | 原因 |
|---|---|
| status ↔ error.type 的**映射本身**（哪条分支出哪个码） | 客户端决策的核心输入；可配即崩坏（H1-H6、H8、H11） |
| Retry-After 的**语义方向**（数值=退避秒 vs 到重置时刻） | CC 对二者处理不同（Bzb vs Fzb，§1.4） |
| 401 语义（绝不 200 化） | H4 |
| 吸收层内部标记串 | §7.1.6 |
| 流内 error type | H12 |

---

## 8. 对 quota-402-design.md 的三处修正（自 review）

| # | 原文断言 | 修正 | 证据 |
|---|---|---|---|
| 1 | §2.2「SDK 对 402 生成 BillingError 类型」 | 0.104.1 中 402 落在 `APIError.generate` 末位 `new APIError(...)`（**无 BillingError 异常类**；BillingError 仅是 error.type 的字面量类型）。message 取自 `error.message` 的结论不变 | core/error.js generate() |
| 2 | §2.2「Claude Code 收到 402 立即停止该请求、不自动退避重试」 | **不成立（2.1.232）**：CLI 层 D 判定 `type==="billing_error"` → 重试约 7 次/1 分钟（§1.4）。SDK 层确实不重试，但 CLI 层接管。**§3.1 设计稿选 billing_error 会事与愿违**——应改用 k2cc 的 `quota_exceeded_error`（非官方 type，恰好触发 non_retryable）或保持 429 | 二进制 0x1070ded0 区域 D 判定 + Rzb/xzb |
| 3 | §4「openclaw issue #30484 显示余额用尽警告」 | 该 issue 实为 anthropics/claude-code#30484（skill 调用功能请求），与 402 无关；OpenClaw 无独立公开源码，行为以继承 SDK 为准 | GitHub issue 核对 |

补充修正：§2.3 对 opencode 的结论（402 不重试）**保持成立**（retry.ts 判定不含 402 与我们的文案词）；§2.5 对 sub2api 的结论（402 换号+冷却）**保持成立**（§3.2 源码验证）。

---

## 9. 可证伪性与验证方法

- 所有 CC 行为断言来自本机 2.1.232 二进制内嵌 JS（函数：Dzb/shouldRetry/pFe/Bzb/vzb/Rzb/xzb/NhS/XLr/_Br）。可复验：`strings /Users/dwgx/.local/share/mise/installs/node/24.19.0/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe | rg -n "billing_error|retry_after_too_long|x-should-retry"`。
- SDK 断言可复验：读 `client.js` shouldRetry/retryRequest + `core/error.js` APIError.generate。
- opencode 断言可复验：GitHub `sst/opencode` `packages/opencode/src/session/retry.ts` + `provider/error.ts`。
- sub2api 断言可复验：`backend/internal/service/openai_gateway_upstream_errors.go:212` + `openai_gateway_credential_failover_loop_test.go:597`。
- 「402+billing_error 被 CC 重试」的实测法：对任一 CC 2.1.232 会话配 402 响应，观察请求日志出现 ~7 次请求与 ~1 分钟间隔（本机 kirostudio 测试桩可复现）。
- 版本漂移警告：CC 2.1.232 行为可能随版本变化（二进制是当前线上版本）；旧版（2.0.15 时代）无 CLI 层独立重试循环，行为更贴近 SDK。
