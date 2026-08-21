# 错误响应完整清单（网关回复下游）

> 用途：「错误码/提示词可配置化」的功能需求清单（key 集合）。
> 枚举基线：当前工作树（2026-08-15，`src/anthropic/handlers.rs` 等，行号以工作树为准）。
> 约束：只研究不改代码。本文件是独立文档，守卫的 include_str! 只读 `src/`，写 marker 字面量安全。

## 0. 枚举方法

对照代码逐分支走查：

1. `map_provider_error`（`src/anthropic/handlers.rs:1539-1876`）主干全部分支
2. `translate_upstream_error` 翻译链（`:1079-1379`，quota_subscription / context_input / network 三个子链）
3. 错误串**产生点**（`token_manager.rs` 选号 bail、`provider.rs` MCP/主路径 bail、透传 failover）
4. 两条 HTTP 入口（`/v1/messages` `:1965`、`/cc/v1/messages` `:3456`）逐出口
5. 透传池 `passthrough.rs forward`（`:178`）逐出口 + `try_custom_api_passthrough`（`provider.rs:1430`）语义
6. WebSearch 快路径 + 回灌循环（`websearch.rs`）
7. OpenAI 兼容层（`openai/handlers.rs`）
8. 非 messages 端点（`/v1/models`、`/v1/messages/count_tokens`、`/healthz`）
9. 流式 in-band 错误（`stream.rs` CompletionStatus / SSE error 事件）

完整性自查：`ErrorResponse::new(` 全部调用点逐一核对（handlers 26 处、websearch 12 处、openai 6 处、passthrough err_response 8 处）；`StatusCode::` 构造全部核对；catch-all 兜底分支（A12 未识别 502、B6 裸 subscription、translate 链 None 透传）已确认在列。

---

## 1. 完整清单

### A. map_provider_error 主干（`handlers.rs:1539-1876`，分支顺序承重）

| # | 触发条件（判据） | 状态码 | error.type | 文案（现状全文） | Retry-After | 标记 | 位置 |
|---|---|---|---|---|---|---|---|
| A1 | `shared_budget_exhausted=1`（每请求跨层共享上游预算耗尽） | 503 | api_error | `网关已就该请求打满上游调用预算（每请求上限），上游仍不可用（等容量）。这是可重试的瞬态状态，请按 Retry-After 退避后重试。` | 8s（`ABSORB_EXHAUSTED_RETRY_AFTER_SECS`=UPSTREAM_RATE_LIMIT=8，clamp 1-300） | shared_budget_exhausted=1 | handlers.rs:1564 |
| A2 | `absorb_budget_exhausted=1`（内置吸收层已尽力仍失败） | 503 | api_error | `网关已就该请求重试至预算上限，上游仍不可用（等容量）。这是可重试的瞬态状态，请按 Retry-After 退避后重试。若持续出现：①面板『限流健康』查看号池容量与冷却分布；②补充凭据分摊上游压力；③必要时调高 upstreamRetryAbsorb* 预算。` | 号池真值 `retry_after_secs=N` 优先 → 风控类 20s → 兜底 8s（clamp 1-300） | absorb_budget_exhausted=1 | handlers.rs:1596 |
| A3 | `inbound_admission_timeout=1`（入站整形排队超时，网关背压） | 429 | rate_limit_error | `Gateway inbound rate shaping is at capacity (request admission timed out). This is gateway-side backpressure, not an upstream cooldown; retrying immediately will not help.` | 串内真值或 1（clamp 1-300） | inbound_admission_timeout=1 | handlers.rs:1635 |
| A4 | `upstream_gate_full=1`（上游并发闸满，网关背压） | 429 | rate_limit_error | `Gateway upstream concurrency gate is full (too many in-flight upstream calls). This is gateway-side backpressure, not an upstream cooldown; retrying immediately will not help.` | 串内真值或 2（clamp 1-300） | upstream_gate_full=1 | handlers.rs:1657 |
| A5 | `retry_after_secs=N`（全池冷却/池耗尽/RPM 饱和等一切带退避真值的串） | 429 | rate_limit_error | `All credentials are temporarily cooling down. Please retry after the indicated delay.` | N（clamp 1-300） | — | handlers.rs:1678 |
| A6 | `model_unsupported_by_pool=1`（号池对该模型永久不可用） | 404 | not_found_error | `请求的模型不被当前号池支持（所有凭据的订阅档位或成本白名单均不含该模型）。这不是临时故障，重试无效：请换用号池支持的模型，或为凭据开通/放开该模型。` | 无（永久态） | model_unsupported_by_pool=1 | handlers.rs:1702 |
| A7 | 上游账户级速率限流（`USER_REQUEST_RATE_EXCEEDED` / `INSUFFICIENT_THROUGHPUT` / `Too many requests`） | 429 | rate_limit_error | `上游账户级速率限流（请求过于密集）。这是可重试的临时状态，请按 Retry-After 退避后重试。若持续出现：①降低客户端并发；②为号池补充更多凭据分摊速率；③面板『限流健康』确认是否单号承载了全部流量。` | 8s（固定常数） | — | handlers.rs:1730 |
| A8 | 上游 403 临时风控（`temporarily is suspended` / `TEMPORARILY_SUSPENDED`） | 429 | rate_limit_error | `上游账户级临时风控（账号被暂时限制，非永久封禁）。这是可恢复的限时状态，请按 Retry-After 退避后重试。若持续出现：①降低并发与请求密度；②为号池补充更多凭据分摊风控压力；③面板『限流健康』查看是否单号承载了全部流量。` | 20s（固定常数） | — | handlers.rs:1759 |
| A9 | 上游 403 region 错配（bearer token invalid + 403 语境 + 无 401 + 无瞬态标记） | 403 | permission_error | `上游拒绝该凭据的授权（bearer token 对目标 region 无效）。这不是服务端故障，重试无效：ksk_ 类 token 按 region 授权，打错 region 恒被拒。排障：①面板查看该凭据的 region 是否与签发 region 一致；②对该凭据手动改 region（或等网关 region 探测自动重选）；③若整池同区，确认推号来源给的 region 正确。` | 无（配置错误，重试无效） | — | handlers.rs:1794 |
| A10 | `translate_upstream_error` 命中（见 B 表，14 个子分支） | 各异 | 各异 | 各异 | 各异 | 各异 | handlers.rs:1812 |
| A11 | 上游 5xx/传输层瞬态（`500 internal server error` / `502 bad gateway` / `503 service unavailable` / `504 gateway timeout` / `: 524` / `524 a timeout occurred` / `internalserverexception` / 传输层串） | 503 | api_error | `上游服务暂时不可用（5xx 或连接失败），这是可重试的瞬态错误。请按 Retry-After 退避后重试；若持续出现，请查看网关日志。` | 3s（固定常数） | — | handlers.rs:1839 |
| A12 | 兜底：未识别任何分支 | 502 | api_error | `上游 API 调用失败（未识别错误）。请查看网关日志获取详情。`（原文只进服务端日志） | 无 | — | handlers.rs:1864 |

### B. translate_upstream_error 翻译链（`handlers.rs:1079-1379`，经 A10 渲染）

| # | 触发条件（判据） | 状态码 | error.type | 文案 | Retry-After | 额外头 | 位置 |
|---|---|---|---|---|---|---|---|
| B1 | `subscription_unsupported=1`（订阅档位不含，永久） | 404 | not_found_error | `当前凭据的订阅档位不支持该应用/模型（永久条件，非临时故障）。换区或重试均无效：请更换为订阅覆盖该应用/模型的凭据，或联系账号管理员开通对应档位。` | 无 | — | handlers.rs:1093 |
| B2 | `quota_exhausted_all=1`（全池月度配额耗尽，provider 确认过 has_available==false） | 429 | rate_limit_error | `月度请求配额已耗尽（号池内所有凭据）。排障：①面板查看各凭据用量；②等待配额周期重置；③为号池补充新凭据。` | **无**（等下个计费周期） | — | handlers.rs:1110 |
| B3 | 裸 `MONTHLY_REQUEST_COUNT` / `QUOTA`（单号/未知范围，MCP/透传冒泡） | 429 | rate_limit_error | `请求配额已耗尽。排障：①面板查看各凭据用量，切到仍有额度的账号；②等待配额周期重置；③为号池补充新凭据。` | **无** | — | handlers.rs:1128 |
| B4 | `MODEL_TEMPORARILY_UNAVAILABLE` / `INSUFFICIENT_MODEL_CAPACITY`（容量紧张，可重试） | 503 | overloaded_error | `上游模型暂时不可用（负载过高），请稍后重试。若持续出现：①换用同族其他版本（如 claude-opus-4.8）；②新发布模型发布初期容量有限，属正常现象，等待 1~2 小时后通常恢复。` | **无**（有争议，见 §3） | — | handlers.rs:1146 |
| B5 | `FEATURE_NOT_SUPPORTED`（region 未开通功能） | 502 | api_error | `当前凭据所在 region 未开通该功能（profile 未激活）。排障：①网关会在刷新时自动验活重选可用 region；②如持续，右键该凭据切换 Profile ARN 到已开通 region（如 eu-central-1）；③确认该账号确在某 region 开通了 Kiro。` | 无 | — | handlers.rs:1156 |
| B6 | 裸 `Invalid token` / `subscription`（未带标记的凭据类文案） | 502 | api_error | `上游拒绝凭据（订阅失效或 token 无效）。排障：①面板对该凭据点『刷新 Token』；②若为 Enterprise/IdC 号，确认 profileArn 已正确解析；③测活确认订阅有效，失效则更换凭据。` | 无 | — | handlers.rs:1171 |
| B7 | `IMAGE_MIME_MISMATCH`（图片声明格式与实际字节不符） | 400 | invalid_request_error | `图片声明的 media_type 与实际字节格式不符（上游 IMAGE_MIME_MISMATCH）。这是请求构造问题，重试无效。排障：①按图片真实格式填写 media_type（如 JPEG 字节不要声明 image/png）；②不要在改扩展名后沿用旧的 media_type；③重新读取并重新编码该图片后再发。` | 无 | — | handlers.rs:1228 |
| B8 | `REQUEST_BODY_INVALID` / `Invalid tool use format`（请求体校验失败） | 400 | invalid_request_error | `请求体校验失败（上游 REQUEST_BODY_INVALID）。这是请求构造问题，重试无效。排障：①检查工具调用与工具结果的配对（上游对 tool 配对较严，截断/重排序会产生孤儿 tool_use）；②检查消息 role 与内容字段合法性；③重新构造请求后再发。` | 无 | — | handlers.rs:1243 |
| B9 | `CONTENT_LENGTH_EXCEEDS_THRESHOLD`（上下文超限，可压缩重试） | 400 | invalid_request_error | `prompt is too long: 上下文窗口已满（对话历史累积超出模型上下文上限）。排障：①精简对话历史或开新会话；②缩短 system prompt；③减少同时挂载的工具数量。` | 无 | `x-kirostudio-compress-retry: 1`（内部标记，出口前摘除，客户端不可见） | handlers.rs:1255 |
| B10 | `Input is too long`（单次输入超限，可压缩重试） | 400 | invalid_request_error | `prompt is too long: 单次输入过长（请求体本身超出上游限制）。排障：①拆分过大的消息或附件；②减少一次性粘贴的文件内容；③对超大工具结果先做摘要。` | 无 | 同上 | handlers.rs:1265 |
| B11 | 传输层 DNS 类（`dns` / `resolve` / `name resolution` / `failed to lookup`） | 502 | api_error | `DNS 解析失败（无法解析上游域名）。排障：①检查本机/容器 DNS 配置；②若走代理，确认代理能解析 kiro.dev；③确认网络出口正常。` | 无 | — | handlers.rs:1342 |
| B12 | 传输层超时（`timed out` / `timeout`） | 504 | api_error | `连接上游超时。排障：①上游或代理可能拥塞，稍后重试；②检查代理延迟；③大请求可拆小以缩短单次耗时。` | 无 | — | handlers.rs:1354 |
| B13 | 传输层 TLS/证书（`certificate` / `ssl` / `tls`） | 502 | api_error | `TLS/证书握手失败。排障：①检查系统时间是否准确；②若走中间人代理，确认其证书受信；③确认未误用被拦截的代理。` | 无 | — | handlers.rs:1362 |
| B14 | 传输层代理（`proxy`） | 502 | api_error | `代理连接失败。排障：①检查代理地址/账密是否正确；②确认代理在线可达；③面板核对该凭据绑定的代理配置。` | 无 | — | handlers.rs:1370 |

### C. token_manager 池状态 bail 串（产生点；渲染后并入 A5/A6，非独立出口）

这些串带 `retry_after_secs=` 或 marker，由 map_provider_error 统一渲染。文案**进日志与埋点**，客户端看到的是 A5/A6 的固定文案。列出以确认「池状态」类的触发面（`token_manager.rs:4640-5219`）：

| # | 触发条件 | 串形态（中文前缀） | 渲染去向 |
|---|---|---|---|
| C1 | 选号重试耗尽 | `所有凭据均无法获取有效 Token（可用: x/N）retry_after_secs={POOL_EXHAUSTED_RETRY_AFTER_SECS}` | A5 (429) | 
| C2 | 全池冷却 fast-fail（all_cooling_fast_fail 开） | `所有凭据均在冷却（x/N）retry_after_secs={最短恢复秒}` | A5 (429) |
| C3 | 全池冷却等待超总预算 | `所有凭据均在冷却，等待超时（0/N）retry_after_secs={}` | A5 (429) |
| C4 | 整池 RPM 饱和等待超时（L4 背压） | `整池 RPM 已饱和，等待恢复超时（N/N）retry_after_secs={}` | A5 (429) |
| C5 | 选号竞态无法收敛（逻辑 bug 兜底） | `选号竞态无法收敛（可用: x/N），已中止以避免忙等` — **无 marker** | **A12 (502 兜底)** ⚠️ |
| C6 | 纯代挂池连续 N 轮全败 ≥20 次升级终态 | `Kiro 路径无可用凭据（池中 N 个号均为 custom_api 代挂号，其上游已连续 N 轮全部失败，判定为持续故障；代挂号本身未被禁用——请检查中转站余额/可用性）pool_permanently_exhausted=1 retry_after_secs={}` | A5 (429)；吸收层拒收 |
| C7 | 纯代挂池全败（未达阈值） | `Kiro 路径无可用凭据（池中 N 个号均为 custom_api 代挂号，其上游此刻全部失败；代挂号本身未被禁用）consecutive_pool_unavailable=N retry_after_secs={}` | A5 (429) |
| C8 | 全池禁用且无自愈希望 | `所有凭据均已禁用（0/N）retry_after_secs={}` | A5 (429) |
| C9 | 全池禁用且无可自愈号（永久耗尽） | `所有凭据均已禁用（0/N）pool_permanently_exhausted=1 retry_after_secs={}` | A5 (429)；吸收层拒收 |
| C10 | 模型级硬门无 TTL（白名单不含/不支持 opus，永久） | `模型 "{model}" 不被本号池支持（x/N 个号均因订阅档位或成本白名单不含该模型而被过滤，非号池耗尽，重试无效）model_unsupported_by_pool=1` | A6 (404) |
| C11 | 模型级硬门有 TTL（blocklist 命中，限时） | `模型 "{model}" 当前无可用凭据（x/N 个号均被模型级过滤，非号池耗尽）retry_after_secs={TTL 剩余}` | A5 (429) |
| C12 | 取 token 失败把最后可用号也禁掉（无自愈希望） | `所有凭据均已禁用（0/N）pool_permanently_exhausted=1 retry_after_secs={}` | A5 (429)；吸收层拒收 |

（C8 与 C9 文案相同前缀、标记不同；`POOL_EXHAUSTED_RETRY_AFTER_SECS`=10 的常数在 token_manager 侧。）

### D. 本地构造错误（非上游语义，两条 HTTP 入口 + 流式收尾）

| # | 触发条件 | 状态码 | error.type | 文案 | 位置 |
|---|---|---|---|---|---|
| D1 | API key 不匹配（中间件，全部 /v1、/cc/v1、/v1/chat/completions、/v1/responses 先过） | 401 | authentication_error | `Invalid API key` | middleware.rs:58 |
| D2 | IP 黑名单命中 | 403 | permission_error | `来源 IP 已被封禁` | handlers.rs:180 |
| D3 | 机器码黑名单命中 | 403 | permission_error | `sbsbsb！` | handlers.rs:197 |
| D4 | 请求体 JSON 解析失败（/v1 入口，`raw_body` 裸字节解析） | 400 | invalid_request_error | `请求体解析失败: {e}` | handlers.rs:1979 |
| D5 | KiroProvider 未配置（/v1 与 /cc/v1 各一份同构代码） | 503 | service_unavailable | `Kiro API provider not configured` | handlers.rs:2008 / 3481 |
| D6 | 请求转换失败（3 种：UnsupportedModel / EmptyMessages / UnsupportedToolMapping；/v1 与 /cc/v1 各一份） | 400 | invalid_request_error | `模型不支持: {model}` / `消息列表为空` / `工具参数无法映射: {tool} — {reason}` | handlers.rs:2128 / 3541 |
| D7 | Kiro 请求体序列化失败（含压缩重试轮内；/v1 与 /cc/v1 各一份） | 500 | internal_error | `序列化请求失败: {e}` | handlers.rs:2161 / 2234 / 3575 / 3650 |
| D8 | 非流式读上游响应体失败 | 502 | api_error | `读取响应失败: {e}` | handlers.rs:2860 |
| D9 | 空/近空响应 + 大输入（疑似上下文超限） | 400 | invalid_request_error | `上游返回了空响应，疑似上下文已接近窗口上限。请精简对话历史（如 /compact）、缩短 system prompt 或减少工具数量后重试。` | handlers.rs:2442 |
| D10 | 空/近空响应 + 小输入（疑似偶发） | 429 | overloaded_error | `上游返回了空响应，请重试。` — ⚠️ **不带 Retry-After** | handlers.rs:2449 |
| D11 | 流式 in-band 上游 error 事件（非限流类） | 200 + SSE `error` 事件 | api_error | `上游返回错误: {code} - {message}` | stream.rs:1829 / :96 |
| D12 | 流式 in-band 上游 error 事件（限流类：throttl/toomanyrequests/ratelimit/429/overload/quota/exhaust） | 200 + SSE `error` 事件 | overloaded_error | 同上 | stream.rs:72 |
| D13 | 流式传输中断（reqwest 层） | 200 + SSE `error` 事件 | api_error | `上游响应流中断: {message}` | stream.rs:106 |
| D14 | 流式解码中断 | 200 + SSE `error` 事件 | api_error | `上游响应解析中断: {message}` | stream.rs:109 |

（D11-D14 的完成态在**非流式**路径渲染为 HTTP 错误：429（限流类）或 502（其余），文案同 client_message()，见 handlers.rs:3192。）

### E. 透传池（custom_api，`passthrough.rs` + `provider.rs:1430`）

| # | 触发条件 | 状态码 | error.type | 文案 | 备注 |
|---|---|---|---|---|---|
| E1 | 凭据缺 base_url | 502 | api_error | `自定义 API 凭据缺少 base_url` | 本地构造 |
| E2 | 出站目标校验失败（SSRF 复验/建连） | 502 | api_error | `透传出站目标校验失败: {e}` | 本地构造 |
| E3 | 90s 首字节超时 | 502 | api_error | `透传上游 90s 未返回响应头` | 本地构造 |
| E4 | 连接层失败（不可达/DNS/TLS） | 502 | api_error | `透传上游请求失败: {e}` | 本地构造 |
| E5 | **上游非 2xx** | **上游 status 原样** | **上游 error 原文**（原始字节，lossy 只进诊断串） | 上游 body 原文逐字节透传 | 头白名单透传：`retry-after` / `request-id` / `content-encoding` / `x-ratelimit-*` / `anthropic-ratelimit-*` / `x-request-id*`。**429 的 Retry-After 由此到达客户端** |
| E6 | 上游流读取中断（成功路径中道崩） | 无 HTTP 响应（连接错误终止） | — | — | 客户端按「提前结束的流」判失败 |
| E7 | SSE 空流兜底（thinking 滤光/真空响应） | 200 + SSE `error` 事件 | api_error | `上游返回空响应（未收到任何正文内容），请重试` | passthrough.rs:522 |
| E8 | 非流式响应体读取失败（超 32MiB 上限/网络） | 502 | api_error | `透传非流式响应读取失败` | 本地构造 |
| E9 | 构建透传响应失败（builder 错误） | 502 | api_error | `构建透传响应失败` | 本地构造 |

failover 语义（provider.rs:1430-1640）：4xx（非 403）直接返 E5 给客户端不换号；403（额度满）/429/5xx/超时 → 冷却换下一个 custom_api；全部不可用 / 墙钟耗尽 / 预算耗尽 / 并发闸空转超限 → 返 `None` 落 Kiro 主路径（错误由 A 表渲染）。

### F. WebSearch 快路径与回灌（`websearch.rs`）

| # | 触发条件 | 状态码 | error.type | 文案 | Retry-After | 位置 |
|---|---|---|---|---|---|---|
| F1 | 无法从消息提取搜索查询 | 400 | invalid_request_error | `无法从消息中提取搜索查询` | 无 | websearch.rs:1070 |
| F2 | 快路径 MCP 调用失败（非预算） | 502 | upstream_error | `WebSearch 上游调用失败: {e}` | 无 | websearch.rs:1104 |
| F3 | 快路径 MCP 共享预算耗尽 | 503 | api_error | `网关已就该请求打满上游调用预算（每请求上限），上游仍不可用。这是可重试的瞬态状态，请按 Retry-After 退避后重试。` | 8s（硬编码 `"8"`） | websearch.rs:399 |
| F4 | 回灌请求转换失败 | 400 | invalid_request_error | `WebSearch 回灌请求转换失败: {e}` | 无 | websearch.rs:1450 |
| F5 | 回灌序列化失败（含压缩重试轮） | 500 | internal_error | `序列化请求失败: {e}` | 无 | websearch.rs:1472 / 1531 |
| F6 | 回灌上游 in-band 错误（不把半截当成功） | 502 | upstream_error | `WebSearch 回灌上游返回错误: {err}` | 无 | websearch.rs:1555 |
| F7 | 回灌流中断 | 502 | upstream_error | `WebSearch 回灌期间上游响应流意外中断（内容不完整，未回灌）` | 无 | websearch.rs:1565 |
| F8 | 回灌 MCP 调用失败（非预算） | 502 | upstream_error | `WebSearch 上游调用失败: {e}` | 无 | websearch.rs:1635 |
| F9 | 回灌循环异常退出（理论不可达兜底） | 500 | internal_error | `WebSearch 回灌循环异常退出` | 无 | websearch.rs:1729 |

（回灌轮内的**上游**错误走 `map_provider_error_for_websearch` = 主映射同口径，即 A/B 表；`x-kirostudio-compress-retry` 内部标记出口前摘除，websearch.rs:1507。）

### G. OpenAI 兼容层（`openai/handlers.rs`，/v1/chat/completions + /v1/responses）

| # | 触发条件 | 状态码 | error.type | 文案 | 备注 |
|---|---|---|---|---|---|
| G1 | 请求体 JSON 解析失败 | 400 | invalid_request_error | `请求体解析失败: {e}` | chat 与 responses 同款 |
| G2 | 缺必填字段 | 400 | invalid_request_error | `缺少必填字段: {e}`（chat）/ `缺少必填字段 model`（responses） | |
| G3 | 请求翻译失败（serde 序列化） | 500 | internal_error | `请求翻译失败: {e}` | |
| G4 | 非流式读上游响应体失败 | 502 | api_error | `读取上游响应失败: {e}` | |
| G5 | **内层 Anthropic 错误翻译** | **透传内层 status** | **透传内层 error.type**（解析失败兜底 `api_error`） | **透传内层 message** | ⭐ **Retry-After 透传**（A1/A2/A3/A4/A5/A7/A8/A11 算好的秒数原样带上；内层没给就不自造） |
| G6 | 流式 in-band error chunk | 200 SSE | 透传 Anthropic error.type（缺省 api_error） | 透传 message | convert.rs:1314 |

**OpenAI 层与 Anthropic 层不是独立错误体系**：G1-G4 是本地构造，G5/G6 完全复用内层（与 anthropic 层同源）。「可配置化」只需配置 Anthropic 层 + 本地构造的 4 条，OpenAI 层自动继承。

### H. 非 messages 端点（待定，先枚举）

| 端点 | 错误路径 | 说明 |
|---|---|---|
| GET /v1/models | 无业务错误（仅中间件 D1 401） | 从模型目录派生，恒 200 |
| POST /v1/messages/count_tokens | 无业务错误（JsonExtractor 框架级 400） | 纯本地计数 |
| GET /healthz | 无业务错误（未鉴权） | 部署探针 |
| /cc/v1/messages | 与 /v1 逐一同构（D3-D7、A 表全分支共用） | **同构代码两份**，改一处必须改另一处（漂移风险点，见 §4） |
| /cc/v1/messages/count_tokens | 同 /v1 | |

---

## 2. 分组建议

### 2a. 客户端语义必须保持的码（status_code 不可自由改）

| 语义 | 条目 | 理由 |
|---|---|---|
| **退避语义 429**（带 Retry-After） | A3、A4、A5、A7、A8、B2、B3、D10 | Claude Code/Cursor 见 429 掐会话或退避；Retry-After 控制节奏 |
| **退避语义 503**（带 Retry-After） | A1、A2、A11 | Cursor 见 429 掐会话、见 503 自行退避重试（2026-08-11 实测依据）；kiro_shield.py 的 RETRYABLE 集含 503 |
| **不可重试 404** | A6、B1 | 永久条件；带 Retry-After 会诱导「每 5 分钟重试直到永远」 |
| **认证/授权 401/403** | D1、D2、D3、A9 | 401/403 触发客户端凭据处置（换 key/停会话）；A9 是配置错误不能伪装成可重试 |
| **请求错误 400** | D4、D6、B7、B8、B9、B10、D9、F1、F4、G1、G2 | 重试原请求无意义；客户端需改请求 |
| **网关内部故障 500/502/504** | D7、D8、D5、A12、B5、B6、B11-B14、F2、F5-F9、G3、G4、E1-E4、E8、E9 | 502 在外挂 RETRYABLE 集内会重试——**对不可重试类给 502 是已知事故形态**（本仓反复踩） |

### 2b. 文案可自由改的（message 是给人看的）

除 §3 列出的**承重字符串**外，所有 message 均可改。A 表/A10 链的 message 同时服务两个受众：

- **人**（用户/管理员排障）：中文排障步骤、面板指引
- **机器**（外挂 kiro_shield.py 按 body 文案分类；Claude Code 按 message 子串触发压缩）：见 §3 承重清单

### 2c. 现状矛盾点（可配置化时一并解决）

- **B4（容量 503 overloaded_error）不带 Retry-After**：同是可重试 503，A11 带 3s、B4 不带。客户端对 overloaded_error 有自身退避，但网关侧口径不统一。
- **D10（空响应 429 overloaded_error）不带 Retry-After**：429 语义「该退避」却无秒数，客户端只能瞎重试。
- **F3 的 Retry-After 是硬编码 `"8"`**：与 A1 的 8s 同值但**字面量写死**（非引用常量），改 A1 不会同步 F3（已知漂移形态）。

---

## 3. 可配置项粒度建议（按客户端行为敏感性）

| 字段 | 建议 | 理由 |
|---|---|---|
| **message** | ✅ **可配**（默认值保留现文案） | 人读的排障文案，唯一受众是人。唯一例外：§3.1 承重字符串（5 处）必须禁止改或改后必须保持子串 |
| **Retry-After 秒数** | ⚠️ 可配但需分级 | 4 个常数（8/20/3/2/1）+ 号池真值优先。秒数决定客户端退避节奏，与上游风控窗口匹配（8s 有实测曲线依据 :919-923）。建议：常数可配、**号池真值 `retry_after_secs=N` 永远优先**（不可被配置覆盖——它比任何常数都准，A2 注释原话） |
| **error.type** | ⚠️ 可配但白名单限定 | 客户端按 type 分派行为：`overloaded_error`/`rate_limit_error` 触发退避、`authentication_error`/`permission_error` 触发凭据处置、`not_found_error` 不可重试。**同语义码之间可换**（如 rate_limit↔overloaded），跨语义组不可换 |
| **status_code** | ❌ 不可配（或极窄白名单） | 唯一已存在的先例：`upstream_retry_absorb_exhausted_status`（429↔503 二选一，服务特定客户端兼容）。除此之外任何 status 改动都会打破 A 表注释里反复钉死的客户端行为契约（429 掐会话 / 502 盲退避 / 404 无限重试 / 403 处置凭据） |
| **标记串（marker）** | ❌ 不可配（内部协议） | `*_exhausted=1` / `*_unsupported=1` / `retry_after_secs=` 是**网关内部组件间协议**（provider→handlers→吸收层），与客户端无关；改它等于改协议。若要做成 key，这些是「配置键名」而非「配置值」 |

### 3.1 承重字符串清单（改 message 前必须保留；删除=静默失效）

> ⚠️ **勘误（2026-08-15 线上实测）**：旧表把 `等容量` 列为承重（「kiro_shield
> COOLING_MARKERS 判据」）——**实测不成立**：`等容量` 只出现在 shield **注释**
> （kiro_shield.py:337）里，不在 `COOLING_MARKERS` 判据表（仅 3 个英文串）。
> A1/A2 的 503 文案不承载任何判据，**可自由改**（删掉不影响 shield 行为）。
> 真正承重的是下表 3 个英文哨兵。

| 字符串 | 出现在 | 谁在依赖 | 失效后果 |
|---|---|---|---|
| `temporarily cooling down` | A5「All credentials are temporarily cooling down. Please retry...」 | 外挂 `kiro_shield.py` `COOLING_MARKERS`（按 body 文案分类，不看状态码） | shield 丢弃网关 Retry-After、改走 20→60s 本地阶梯，等真实恢复时间 2~6 倍（CLAUDE.md 记录的 1753 次失败事故） |
| `All credentials are temporarily` | A5（同上） | 同上 | 同上 |
| `inbound rate shaping` | A3「Gateway inbound rate shaping is at capacity...」 | 同上 | 同上 |
| `prompt is too long`（英文前缀） | B9、B10 文案头部 | Claude Code compact-and-retry 的 message 小写子串判据 | 客户端自动压缩静默失效，撞满上下文直接报错 |
| `Gateway inbound rate shaping is at capacity` + `gateway-side backpressure` | A3、A4（英文） | 内置吸收层/外挂按 body 区分「网关背压，不该重试」 | 重试层把网关自己的背压信号当上游故障重试 |
| `retrying immediately will not help` | A3、A4 | 同上（语义承载） | 同上 |
| `sbsbsb！` | D3 机器码黑名单 | 无外部依赖（疑似刻意文案） | 无已知风险，可改 |

（英文哨兵不是「翻译问题」——它们是给外部机器判据用的，与给人读的中文并存是刻意设计。
shield 的 PERMANENT 中文判据（`请求体解析失败` / `凭据不支持刷新` / `不支持刷新 Token`）
是「改文案先查仓外消费者」的另一张清单，见 blockers-protocol.md §1.1。）

---

## 4. 可配置化实现的额外注意点（从枚举中暴露的结构性问题）

1. **/v1 与 /cc/v1 同构代码两份**（D4-D7、A 表全分支共用函数，但 D 类本地错误是复制粘贴的）：配置 key 必须两份同读，否则两入口文案漂移。已踩过（守卫清单多处记录「同一逻辑各写一份」事故）。
2. **map_provider_error 分支顺序是承重的**（A1 必须第一、A3 必须在 A5 之前等，吸收层 `absorb_class_of` 有守卫钉顺序）：配置化只改「渲染值」，**不许改判据顺序**。
3. **C 表 12 条中间串不直接面对客户端**（客户端看到的是 A5/A6 固定文案）：若要「池状态」文案可配，配 A5/A6 即可；C 表文案进日志/埋点，另配或不动。
4. **透传 E5 的错误体是上游原文**（网关零构造）：可配置化对 E5 无效，除非引入「透传错误体改写」功能（超出本次范围，标记为待定）。
5. **F3 Retry-After 硬编码 `"8"`** 与 A1 的常量应合并为同一配置源。

---

## 5. 数量统计

| 层 | 类别数 | 说明 |
|---|---|---|
| A. map_provider_error 主干 | 12 | 含 1 条 catch-all 兜底（A12） |
| B. translate_upstream_error 翻译链 | 14 | 3 子链（quota 6 / context 4 / network 4） |
| C. token_manager 池状态中间串 | 12 | 非独立出口，渲染并入 A5/A6（其中 C5 落 A12 兜底） |
| D. 本地构造（HTTP + 流式收尾） | 14 | 含流式 in-band 4 种（D11-D14） |
| E. 透传池 | 9 | 其中 E5 = 上游原文透传（网关零构造） |
| F. WebSearch 快路径/回灌 | 9 | 另共用 A/B 表（map_provider_error_for_websearch） |
| G. OpenAI 兼容层 | 7 | 4 本地 + 2 透传内层 + 1 流式透传 |
| **合计（最终响应形态）** | **≈77 条目，去重后约 50+ 独立形态** | 可配置化的最小 key 集合建议 = A(12) + B(14) + D(14) + E(8 本地) + F(9) + G(4 本地) = **61 个「触发条件 → 渲染值」条目** |

各层关系：G5/G6 透传 Anthropic 层 → 配置 Anthropic 层即覆盖 OpenAI 层；F 的轮内上游错误也走 A/B → 同覆盖。

---

## 6. 自 review 结论

- **分支完整性**：`map_provider_error` 12 分支逐条核对（含 `shared_budget_exhausted` 前置分支、`inbound_admission_timeout` 在 `retry_after_secs` 之前的顺序敏感分支）；`translate_upstream_error` 三链 14 分支全列（含 `Invalid tool use format` 2026-08-11 新增、M3 对照组）；catch-all 兜底 A12 与裸串 B3/B6 在列。
- **marker 标记串**：`subscription_unsupported=1` / `model_unsupported_by_pool=1` / `quota_exhausted_all=1` / `pool_permanently_exhausted=1` / `bearer_invalid_transient=1` / `shared_budget_exhausted=1` / `absorb_budget_exhausted=1` / `inbound_admission_timeout=1` / `upstream_gate_full=1` / `consecutive_pool_unavailable=N` / `retry_after_secs=N` / `x-kirostudio-compress-retry` 全部登记（§1 各表 + §3）。
- **吸收层**：AbsorbBudgetExhausted → A2（503）；容量 400 → B4（503 overloaded）；压缩失败 → D7/B9/B10（500/400 + 内部重试头）。
- **透传**：E5 上游原文透传（status 保留 + 原始字节 + 白名单头含 Retry-After）。
- **闸门**：入站超时 A3（429）、并发闸 A4（429）、RPM 全池冷却 C4→A5（429 + 真值）。
- **websearch/MCP**：MCP 失败 F2/F8（502 upstream_error）、快路径 F1-F3。
- **非 messages 端点**：H 表已枚举（无业务错误，仅中间件 401）。
- **OpenAI 层**：与 anthropic 层同源（G5/G6 透传），非独立体系。
- **已知未覆盖**：admin 面板 API（非「下游客户端」，不在范围）；JsonExtractor 框架级 400（axum 默认形态，非网关构造）。
