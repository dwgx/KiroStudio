# KiroStudio 缓存系统深度研究报告

> 调研日期 2026-07-28。置信度标签：【官确】官方文档确证 · 【实测】第三方实测报告 · 【推测】推导未验证 · 【本仓】已在本仓代码核对（附 file:line）。
> 本文只做研究与结论，不含实现。落地方案见 `docs/CACHE-RFC.md`。

---

## 0. 一句话结论

**上游 Kiro（CodeWhisperer/Q）的 Smithy 模型里存在 `CachePoint` 与 `MetadataEvent.tokenUsage.cacheReadInputTokens` 两个字段，我们既没发过前者、也没解析过后者。** 所谓"Bedrock 缓存不透明"这个前提是错的 —— 真信号很可能一直在线上，被 `events/base.rs:127` 当成 `Unknown {}` 丢掉了。同时现网在向客户端**注入一个虚构的 `cache_read_input_tokens`**，且这个虚构值会反向扣减上游唯一准确的 input_tokens。

所以本项目的正确顺序不是"造缓存"，而是：**先睁眼（L2 度量）→ 再止损（停止污染 + 稳前缀 L0）→ 再调度（L1）→ 最后才考虑本地响应缓存（L3）**。

---

## 1. 硬事实层

### 1.1 Anthropic 直连语义（决定我们对客户端的契约）

| 事实 | 出处 |
|---|---|
| 断点语义是**前缀缓存**，不是块缓存：写只发生在断点处（`hash(prefix ending at that block)`），读向后回溯找**以前写过的条目**（"looking for prior writes, not for stable content"） | 【官确】[prompt-caching](https://platform.claude.com/docs/en/docs/build-with-claude/prompt-caching) |
| 渲染顺序固定 `tools → system → messages`；每级改动作废本级与其后所有级 | 【官确】同上 |
| 最多 **4 个断点**；每个断点**只回溯 20 个 content block**。官方示例：35 block 的请求检查 35→16，"one position outside the window, so there is no cache hit" | 【官确】同上 |
| `total_input = cache_read + cache_creation + input`，三者**互斥构成划分**；`input_tokens` 定义为"最后一个断点之后的 token"，**不是总输入** | 【官确】同上 |
| 官方判据："creation 与 read 皆为 0 即未命中"；首次建缓存**必报** `cache_creation_input_tokens` | 【官确】同上 |
| 最小可缓存长度按模型且**非单调**：Opus 5=512、Sonnet 5/Opus 4.8=1024、Opus 4.5/4.6/Haiku 4.5=4096。低于门槛**不报错、静默不缓存** | 【官确】同上 |
| TTL 只有 `5m`/`1h`，默认 5m；**读命中免费续期**；1h 已 GA 不需 beta header（旧的 `extended-cache-ttl-2025-04-11` 现在会 400） | 【官确】同上 + [release-notes](https://platform.claude.com/docs/en/release-notes/api) |
| 倍率：5m write 1.25× / 1h write 2× / read **0.1×**。5m 一次读回本 | 【官确】[pricing](https://platform.claude.com/docs/en/docs/about-claude/pricing) |
| `cache_read` **不计入 ITPM** 限流（Haiku 3.5 例外） | 【官确】[rate-limits](https://platform.claude.com/docs/en/api/rate-limits) |
| 隔离：Claude API 为 **workspace 级**，Bedrock/GCP 为 **organization 级**；"Different organizations never share caches, even if they use identical prompts" | 【官确】同 prompt-caching |
| 字节级敏感："Cache hits require 100% identical prompt segments"。官方点名 Go/Swift 的 map 序列化随机化 key 序会打破缓存 | 【官确】同上 |
| 存在官方排障 beta `cache-diagnosis-2026-04-07`，返回 `cache_miss_reason.type` ∈ {model_changed, system_changed, tools_changed, messages_changed, ...}。**仅 Claude API，Bedrock 不支持** | 【官确】[cache-diagnostics](https://platform.claude.com/docs/en/docs/build-with-claude/cache-diagnostics) |

### 1.2 上游现实（Kiro / CodeWhisperer / Bedrock）

这一节是本次调研最重要的收获，它**推翻了本仓 `converter.rs:747` 注释隐含的假设**（"Bedrock prefix cache 不透明、只能靠 continuationId 蹭"）。

| 事实 | 出处 |
|---|---|
| **AWS 官方 Smithy 模型里有 `CachePoint`**，挂载点三处：`Tool`（union 成员，与 `toolSpecification` 并列）、`UserInputMessage.cachePoint`、`AssistantResponseMessage.cachePoint` | 【官确】[amazon-q-developer-cli `_cache_point.rs`](https://github.com/aws/amazon-q-developer-cli/blob/main/crates/amzn-codewhisperer-streaming-client/src/types/_cache_point.rs) · [CodeWhisperer Smithy 模型](https://github.com/aws/aws-toolkit-vscode/blob/master/packages/core/src/codewhisperer/client/user-service-2.json) |
| `CachePointType` 枚举**只有 `default`，没有 ttl 参数** → 拿不到 1h，只能 5m | 【官确】同上 |
| **`MetadataEvent.tokenUsage` 含 `uncachedInputTokens` / `cacheReadInputTokens` / `cacheWriteInputTokens` / `outputTokens` / `totalTokens`** | 【官确】[shape_token_usage.rs](https://github.com/aws/amazon-q-developer-cli/blob/main/crates/amzn-codewhisperer-streaming-client/src/protocol_serde/shape_token_usage.rs) |
| `MeteringEvent` 只有 `usage: f64` + `unit: String`（本仓已解析） | 【官确】[_metering_event.rs](https://github.com/aws/amazon-q-developer-cli/blob/main/crates/amzn-codewhisperer-streaming-client/src/types/_metering_event.rs) |
| `UserInputMessage` 另有 `clientCacheConfig: { useClientCachingOnly: Boolean }`，语义未公开 | 【官确】[_client_cache_config.rs](https://github.com/aws/amazon-q-developer-cli/blob/main/crates/amzn-codewhisperer-streaming-client/src/types/_client_cache_config.rs) |
| 官方 `chat-cli` 自身**从不设置** cachePoint，也不读 cacheRead → Q CLI 走纯隐式路径 | 【官确】GitHub code search 于 `crates/chat-cli` 命中数 0 |
| Bedrock 侧：cache checkpoint 前缀必须逐字节稳定；TTL 每次命中重置，多数模型 5 分钟；最多 4 个 checkpoint；顺序 `tools→system→messages` | 【官确】[Bedrock prompt-caching](https://docs.aws.amazon.com/bedrock/latest/userguide/prompt-caching.html) |
| Bedrock 缓存"**specific to your account**"（按 AWS 账号隔离）；与 CRIS 兼容但高负载路由切区会**增加 cache write** | 【官确】[产品页](https://aws.amazon.com/bedrock/prompt-caching/) + 上述用户指南 |
| Bedrock 明确点名有**自动/隐式**缓存的只有 Amazon Nova 与 OpenAI GPT-5.5 及更早。**Claude 系列没有任何隐式缓存表述** | 【官确】同用户指南 |
| Kiro credit 计量是"**按请求不按 token**"，精度到 0.01；倍率 Opus 5 = 2.2×、Sonnet 5 = 1.3×、Haiku 4.5 = 0.4× | 【官确】[Kiro billing FAQ](https://kiro.dev/docs/cli/billing/related-questions/) · [Kiro models](https://kiro.dev/docs/models/) |
| Kiro 官方把 prompt caching 定位为**它自己的降本手段**，"reuses context where possible"，收益体现在定价设计而非用户 credit | 【官确】同 billing FAQ + [定价博客](https://kiro.dev/blog/new-pricing-plans-and-auto/) |

#### 生态实测严重冲突 —— 这是最大未决点

三份独立实测互相矛盾，且都不是官方：

- **A（kiro2cc-proxy，2026-06-20）**：Kiro 有跨会话前缀缓存，命中按**全价 50%** 计费（≠ Anthropic 的 10%），靠稳定 `agentContinuationId` + 冻结 `history[0]` 达 89–97% 命中。**但该项目已于 2026-06-25 主动废弃 credits 反推**（大请求会被 clamp 恒显示 100% 命中），改用本地字符估算，误差 ±15–30%。【实测】[文档](https://github.com/TsinHzl/kiro2cc-proxy/blob/main/docs/Kiro缓存与状态管理解析.md)
- **B（kiro-claude-bridge，企业 IDC entitlement）**：`cachePoint` 被上游接受（无 400），但同一 36k 前缀重复打，CACHE-ON credits = `0.371, 0.196, 0.196, 0.196` vs CACHE-OFF 全 `0.196` → **读折扣 = 0，首写溢价 ≈1.89×，净亏**，故默认关闭。且该 entitlement 的 `metadataEvent` **完全不发 `tokenUsage`**。【实测】[CONTEXT.md](https://github.com/AlqattanDev/kiro-claude-bridge/blob/main/CONTEXT.md)
- **C（kirocc）**：只给 `tools` 数组插 cachePoint，并**能正常读到** `metadataEvent.tokenUsage` 的 cacheRead/cacheWrite。【实测】[cache_points.go](https://github.com/d-kuro/kirocc/blob/main/internal/reqconv/cache_points.go)

**唯一能让 A/B/C 自洽的解释**：折扣幅度与 `tokenUsage` 可见性**按 entitlement 分档**（social/Pro vs IDC/企业）。B 自己也说其 cacheRead 映射"是给 Kiro Pro/social 后端用的，它确实会发"。【推测】

> **未决 #1**：本仓各类凭据（social / IDC / M365）分别属于哪一档？`metadataEvent` 发不发？cachePoint 是省钱还是亏钱？**必须自己实测，不能采信任何一方。**

### 1.3 Claude Code 客户端行为（决定命中率天花板）

| 事实 | 出处 |
|---|---|
| CC 分层顺序与 Anthropic 一致，四层稳定度：静态 system+tools（全局） → CLAUDE.md（项目） → 会话上下文 → 消息。官方自认造过三次事故：静态 prompt 塞时间戳、工具排序非确定、改工具参数 | 【官确】[官方博客 2026-04-30](https://claude.com/blog/lessons-from-building-claude-code-prompt-caching-is-everything) |
| `system[0]` 是 `x-anthropic-billing-header: cc_version=...; cch=<NONCE>;`，**`cch` 每请求重生**。对不认 cache_control 的第三方 provider = 命中率 0%。剥掉这一个 block 实测 **0% → 99.7%** | 【实测】[issue 68900](https://github.com/anthropics/claude-code/issues/68900) |
| **分水岭**：自 CC v2.1.181 起该 block 在自定义 base URL 下"每会话稳定"，网关可按整 body 做键 | 【官确】[LLM gateway protocol](https://claude-code.mintlify.app/en/llm-gateway-protocol) |
| 交互式会话下 cwd/platform/git 快照/CLAUDE.md **仅会话启动读一次**；每轮真正追加的只有 `<system-reminder>` 等。官方明确"无时间戳注入" | 【官确】[CC prompt-caching](https://code.claude.com/docs/en/prompt-caching) |
| **headless `claude -p --resume` 每次重生 git 段** → 全前缀塌陷。语料实测 16,058 次调用中 8.8% 呈塌陷特征，183 会话共重建 1.61 亿 tokens | 【实测】[issue 78720](https://github.com/anthropics/claude-code/issues/78720) |
| **CC 的 `context_window.used_percentage` 把 `cache_read_input_tokens` 计入**（反编译 `BqA()` = input + creation + read + output）。实测 `cache_read:339155` → 1M 窗口显示 34% | 【实测】[issue 42646](https://github.com/anthropics/claude-code/issues/42646) · [13997](https://github.com/anthropics/claude-code/issues/13997) |
| **没有** CC 校验 input_tokens **数值**的证据；但有"字段缺失就崩"的证据（`usage.input_tokens` undefined → TypeError 会话卡死） | 【实测】[issue 46932](https://github.com/anthropics/claude-code/issues/46932) |
| 不破前缀：subagent（独立冷缓存）、fork、`/rewind`、tool search。必然全变：切模型、effort 变更、CC 升级后 resume、worktree 切换、**MCP 连断** | 【官确】同 CC prompt-caching |
| CC 直连命中率被独立代理测为 **97–99%**（cc-relay，45,884 请求/320 会话）；但**同数据集下 143 会话中 79% 首轮 `cache_read=0`**，Haiku 子代理仅 58.1% | 【实测】[claude-code-cache-analysis](https://github.com/ArkNill/claude-code-cache-analysis) |
| 网关**不能对未知 cache_control marker 报 400**：CC 会不带它重试，并在该对话剩余时间放弃缓存该块 | 【官确】同 CC prompt-caching |

> **推论（对本仓直接适用）**：注入一个大的假 `cache_read` 不只是"显示不准"，它会**抬高 CC 的上下文进度条、提前触发 auto-compact**（阈值 = 窗口 − 13K）。这是行为缺陷，不是展示缺陷。【推测】

### 1.4 本仓现状（全部已核对到行号）

**① `metadataEvent` 从未被解析 —— 头号发现**

`src/kiro/model/events/base.rs:26-32` 的 `EventType::from_str` 只认 4 种字符串（`assistantResponseEvent` / `toolUseEvent` / `meteringEvent` / `contextUsageEvent`），其余一律 `Unknown`；`base.rs:127` 把 `Unknown` 变成 `Self::Unknown {}` 静默丢弃，**连事件类型名都不打日志**。全仓 grep `metadataEvent|tokenUsage|cacheReadInputTokens|cachePoint` 命中数 **0**。

含义：AWS 官方模型里带 `cacheReadInputTokens` 的 `MetadataEvent` 若上游真的在发，我们从未看过一眼。**这是零风险、零成本、可能直接给出真值的一步。**

**② `cache_read_input_tokens` 是虚构值，且无条件注入**

- `token.rs:236-252` `count_prefix_tokens`：把 `[system + messages[..n-1]]` 的字符数经 `count_tokens` 估算当作命中量。
- `handlers.rs:901-914`（/v1）与 `1813-1826`（/cc/v1）：唯一门槛是 `messages.len() > 1`，**无条件计算并注入**。
- `config.rs:290-295` / `730-744` / `885-886` 定义了 `prompt_cache_enabled`（默认 false）与 `prompt_cache_ttl_seconds`，**全仓零读取点** → 死配置，且 `config.rs:731-739` 的注释（"默认关以砍掉 build_profile CPU 开销"）描述的代码路径已不存在（`token.rs:250-253` 自承 cache_tracker 已删）。**注释与实际行为相反。**

**③ 估算器本身误差极大且符号不定**

`token.rs:79-103` `count_tokens`：非西文字符记 4.0 字符单位、西文 1.0，÷4 得基数，再按基数分档乘系数（<100→×1.5、<200→×1.3、<300→×1.25、<800→×1.2、≥800→×1.0）。

- 注释与代码不一致：`token.rs:75` 写"非西文每个计 4.5"，`token.rs:84` 是 `4.0`。【本仓】
- **分档在 ≥800 处断崖**：同一段文本拆块分别计数会被逐块乘 1.5，整块则 ×1.0。CC 历史是几百条小消息 → 每块都落 <100 档 → 前缀估算整体被 ×1.5。【实测复刻】
- 对照真实 tokenizer：英文 0.363 tok/char（估算器 0.25 → **低估 31%**）；代码 0.324–0.427（**低估 23–41%**）；中文 0.643（估算器 1.0 → **高估 55%**）；单个极简工具真实 ≈389 token 而估算给 108（**低估 3.6 倍**）。【实测】[tokenizer gist](https://gist.github.com/cometkim/f5b382e9f69b3a35513ce66725b0e42e) + 【官确】[token-counting](https://platform.claude.com/docs/en/docs/build-with-claude/token-counting)
- **结构性漏计比系数误差更严重**：`token.rs:208-213` 的 `count_all_tokens_local` 只读 content 块的 `"text"` 字段 —— `tool_result`、`tool_use.input`、`image.source`、`thinking` 全计 0。CC 会话里 tool_result 常占历史 70%+。【本仓】
- **口径不对称**：`count_prefix_tokens`（`token.rs:249-250`）两次调用都传 `tools=None`，而分母 `input_tokens` 走的 `count_all_tokens` **含** tools。CC 的 15–30k tools 定义只进分母不进分子。【本仓】

**④ 虚构值反向污染上游唯一准确的数字**

链路：`contextUsageEvent` → `stream.rs:983-999` 用 `pct × window / 100` 得 input → buffered 收尾 `stream.rs:2516-2547` 回填 `message_start`，但先经 `stream.rs:159-168` `billed_input_tokens` 减掉 `cache_read`。

- clamp 在 `stream.rs:167 .max(0)`；埋点侧第二道在 `record.rs:176-193 clamp_cache_to_input`。**不会负数，但会静默出现 0。**
- 定量：真实总输入 100k、真实前缀 95k，估算高 30% → cache_read ≈123k → min 后仍 ≥ 反推的 100k → **`message_start.input_tokens = 0`、cache_read 显示 ≥100k**，客户端看到"本轮零新增输入、全部命中"。纯中文多轮会话是**必然触发**场景。
- 反向场景（英文 + 大量 tool_result）估算严重低估 → CC 显示"从不命中缓存"。**同一网关在两类流量上误差符号相反。**
- `ccAutoBuffer` 默认 true（`config.rs:629-636`），`/v1`（`handlers.rs:930`）与 `/cc/v1`（`handlers.rs:1853`）都走 buffered → **默认配置下就是"用准确值减估算值"**。而 buffered 路径存在的全部理由就是让 CC 拿到准确的 input_tokens。

**⑤ 污染面（全量下游消费点）**

- 源：`stream.rs:1071-1082 ResolvedUsage` → `record.rs:92-103`（流式 `handlers.rs:1025-1028`、非流式 `handlers.rs:1584`）
- 预聚合：`usage_stats.rs:95-97`（字段）、`116-117`（on_record）、`131-132`（merge）、`658-660`/`714-716`/`737-739`（响应）、`1072-1073`/`1108-1109`（时序）
- 落库：`trace_db.rs:174-175`（迁移）、`222-223`（建表）、`244`/`265-266`（插入）、`411-412`（读取）
- 派生：`record.rs:159-164 billed_input_tokens()` —— 面板"计费输入"直接由虚构值反算
- 前端：`usage-page.tsx:75-83`（billedInput/cacheHitPct）、`293-294`、`329`、`347-372`、`405`、`454`、`756-763`、`805-817`、`892`、`1043-1044`、`1104`；`overview-page.tsx:641-642`（24h 命中率）；`ops-detail-dialogs.tsx:595`、`656-702`、`914-923`、`1010-1015`；`overview/AreaTrendChart.tsx:468-473`、`701`
- **OpenAI 兼容层**：`convert.rs:853-881` `UsageTokens::openai()` → `prompt = input + creation + read`、`cached = read`；输出到 `1021-1027`/`1131-1141`/`1362-1372`/`1542-1550`。billed 被 clamp 成 0 时 `prompt` 退化为本地膨胀估算，OpenAI 客户端连总输入都拿不到准确数。
- 唯一诚实标注只有 `ops-detail-dialogs.tsx:595` 一个 tooltip（`zh.json:689`）。

**⑥ 违反的官方 usage 语义（逐条）**

1. `input_tokens` 官方定义是"最后一个断点之后"，网关无断点概念，减的是"历史前缀估算"，产出的数既不是断点后余量也不是全量。
2. 官方"creation 与 read 皆 0 = 未命中"、"首次建缓存必报 creation"。网关 **creation 恒 0、read 恒 >0** —— 这个组合在官方语义下**不可能出现**。
3. 最小可缓存长度（512/1024/4096）完全未判，20 token 的前缀也报命中。
4. TTL 完全未判，隔 3 小时续聊仍报满额 cache_read。
5. 失效规则完全未判。更矛盾的是 `strip_env_noise`（默认 true）的存在本身就说明 CC 的 system 每请求在漂移 —— 按官方规则这恰恰**应当**判 miss。
6. `stream.rs:663-671` 的 `ephemeral_5m/1h` 明细恒为 0 且无条件下发，声明了一个不存在的 TTL 拆分。
7. **口径错位**：`contextUsageEvent` 是**上下文窗口占用率**（`events/context_usage.rs:17-21`），本质是 gross 体积量，不是计费量。拿它反推的值去减 cache_read，是把体积量当计费量用。

**⑦ 唯一真实的缓存收益（且无任何度量）**

`converter.rs:631-641 derive_agent_continuation_id` 从 conversationId 确定性派生 SHA256，`converter.rs:746-751` 注释记录实测 credits 0.141 → 0.075（≈47% 折扣）。配合 `converter.rs:463-476 canonicalize_system_text`（折叠 `cch=` 归因头 + 剥 `<env>` 噪音）稳前缀。**这是真钱，但没人知道命中率是多少、哪次 miss、为什么 miss。**

**⑧ 前缀稳定性审计（我实测复核，纠正了两个常见误判）**

按破坏范围排序：

| 等级 | 位置 | 触发条件 | 破坏范围 |
|---|---|---|---|
| 🔴 致命 | `converter.rs:1257-1277` + `1338-1341` | `generate_thinking_prefix` 把 `budget_tokens` / `effort` **内联进 system 注入的最前面**。CC 的 adaptive budget 随轮次变化 | **从第一个字节起全废**（比官方语义的"messages 层失效"严重得多） |
| 🔴 致命 | `handlers.rs:433-445` + `compressor.rs` | 请求体跨过 `trigger_bytes`（默认 4MiB）阈值那一刻，`compress_tool_results_pass` 改写**历史中段** | 全前缀失效，且是**悬崖式**（4MiB−1B 正常、4MiB+1B 全废）。LMCache 实测同类滑窗截断把命中率从 ~85% 打到 ~45% |
| 🟠 高危 | `converter.rs:679-691` + `1129-1152` | `remove_orphaned_tool_uses` 用 `retain` **原地删历史中段**的 tool_use | 从被删位置起后续全废。CC 中断/取消工具调用即触发 |
| 🟠 高危 | `converter.rs:1362`+`1410-1418` | `image_dedup` 跨整个历史去重（`MAX_TOTAL_IMAGES=20`），同一张图第二次出现被替换为占位符 | 历史中段字节随后续轮次变化；截图多的会话必踩 |
| 🟡 中危 | `converter.rs:637-641` fallback | `extract_session_id` 拿不到 `metadata.user_id` → `Uuid::new_v4()` 随机 conversationId → continuationId 每请求全新 | 该客户端**永久零命中**（hank9999/kiro.rs 就是这个反面对照） |
| 🟡 中危 | `converter.rs:1390-1401` | 结尾孤立 user 消息被自动配一条 `"OK"` assistant | 下一轮真 assistant 回复到位时该位置字节变化 |

**已复核为不成立的怀疑**（避免后续误改）：
- `convert_tools`（`converter.rs:1213`）用 `iter().map()` **保序**；`collect_history_tool_names`（`675-691`）用 `Vec` + `contains` 也保序 → tools 数组顺序稳定，**没有 HashMap 迭代顺序泄漏**。
- `map_tool_name` / `shorten_tool_name`（`1159-1205`）纯 SHA256，**确定性**。
- `create_placeholder_tool` 的追加是 **append 语义**，前缀不变，可接受。
- `conversation.rs` 全部结构体是 serde derive 固定字段序，`Vec` 有序；`tool.rs:37/125` 的 `serde_json::Value` 有隐患**但仅当客户端传来的 tool schema / tool_use input 里的 JSON key 序不稳定时**，而 serde_json 默认 `Map = BTreeMap`（未启用 `preserve_order` feature，已核 `Cargo.toml:20`）→ **key 自动有序，稳定**。

### 1.5 学术界能借与不能借的

**不能借**（全部需要读写 KV 张量）：Prompt Cache 的模块化位置编码、CacheBlend/EPIC 的选择性重算、LMCache/Mooncake 的 KV 池化传输。

**能借的三类**：

1. **块哈希链**（vLLM APC）：`h_i = H(h_{i-1} ‖ block_i)`，只有满块参与。"若某块哈希匹配，则其所有前缀块必然匹配"。【官确】[vLLM prefix_caching](https://docs.vllm.ai/en/latest/design/prefix_caching/)。`cache_salt` 注入首块哈希做多租户隔离 —— 我们的等价物是 credential_id。
2. **cache-aware 路由的决策规则**：四家独立实现**全部是"硬门 + 二选一"，没人做连续加权**。Preble E2：`missed_len < cached_len` → exploit 否则 explore【论文】[2407.00023](https://arxiv.org/html/2407.00023v2)；SGLang：`cache_threshold=0.5`、`balance_abs=32`、`balance_rel=1.1`【源码】[cache_aware.rs](https://github.com/sgl-project/sglang/blob/main/sgl-model-gateway/src/policies/cache_aware.rs)；Ray Serve PrefixCacheAffinityRouter 同构；DualMap（ICLR'26）"默认选缓存高的，直到预测 TTFT 破 SLO 才换"。
   **DualMap 的消融明确否证了"每请求算最小成本"这条直觉路线** —— min-TTFT 会在 cache-aware 与 load-aware 间振荡并自己搅乱缓存，SLO 门控法 P50 好 23.5%、P90 好 18.5%。【论文】[2602.06502](https://arxiv.org/html/2602.06502) → **不要每请求重算最优号，要粘住直到越过阈值。**
3. **驱逐策略**：纯 Leaf-LRU 最坏 Θ(n) 竞争比，**随机化叶驱逐 Θ(log(B−L)) 且有匹配下界**，实测开销 0.71–1.05ms vs LRU 0.09–0.13ms。【论文】[2601.18999](https://arxiv.org/html/2601.18999v1)

**收益上限锚点**：Mooncake 生产 trace 即使假设无限存储、无限 TTFT 预算，可复用 KVCache **也只到 ~50%**（分工作负载 0%–90%）。【论文】[2407.00079](https://arxiv.org/html/2407.00079v1) **不要按 90% 立 KPI。**

**度量方法学（可直接复用）**："Don't Break the Cache"（PwC，2026-01）用 **UUID 强制制造缓存边界**做四组对照，每组每模型 40 session、10k token 系统提示、组间等待 >24h 让条目过期。结论：成本降 41–80%；**Claude Sonnet 4.5 最优策略是"仅缓存系统提示"**（成本 −78.5%、TTFT −22.9%）；成本节省在各策略间只差 2–4 点 → **几乎全部收益来自系统提示**。【论文】[2601.06007](https://arxiv.org/html/2601.06007v2)

### 1.6 L3（本地响应缓存）的行业与安全事实

- **四家主流网关（LiteLLM / Portkey / Cloudflare AI Gateway / Helicone）全部只做整 body 逐字节精确匹配，且全部默认关闭。**【官确】[CF caching](https://developers.cloudflare.com/ai-gateway/features/caching/) 等
- CF 明确承认其缓存是 volatile，**两个并发相同请求会双双打到上游**（无 request coalescing）。【官确】同上
- **`temperature=0 ≠ 确定性**：Thinking Machines 实测 temp=0 同 prompt 1000 次 → 80 个不同结果，根因是 kernel 缺 batch invariance。【实测】[thinkingmachines.ai](https://www.thinkingmachines.ai/blog/defeating-nondeterminism-in-llm-inference/) → 网关缓存给出的"同问同答"其实**比上游更确定**，这是行为变更，必须明示。
- **语义近似缓存：任何阈值都不要做。** vCache 实测静态阈值 GPTCache 在 150k query 上、阈值 **0.99 时仍 1.7% 错误率且随样本量持续上升不收敛**【论文】[2502.03771](https://arxiv.org/html/2502.03771)；agent 场景 **44.3% unsafe hit**，"check email" vs "send email" 余弦 0.91 但需完全不同工具序列【论文】[2602.18922](https://arxiv.org/html/2602.18922)；最优离线语义缓存 **NP-hard**，P≠NP 下不可优于 (1−1/e)【论文】[2603.03301](https://arxiv.org/html/2603.03301v1)。coding agent 场景一次误命中 = 执行错误的文件编辑，代价不可逆。
- **哈希必须全量密码学摘要，禁止截断**：两个真实 CVE —— vLLM 用 Python 内置 `hash()` 可构造碰撞返回**别人内容生成的 KV**（CVE-2025-25183）；LiteLLM 用 `token[:20]` 做 key 导致**攻击者继承他人身份**（CVE-2026-35030）。【官确】[GHSA](https://github.com/vllm-project/vllm/security/advisories/GHSA-rm76-4mrf-v9r8) · [LiteLLM 安全公告](https://docs.litellm.ai/blog/security-hardening-april-2026)
- **共享凭据池是已被论文点名的高危结构**：CacheProbe（SAGAI'26@S&P）实测经 OpenRouter 默认路由（共享组织凭据）时 Groq/Fireworks/OpenAI 三家全部出现**跨账号 prompt cache 共享**，直连与 BYOK 无泄漏；根因是**凭据池化而非路由层**。【论文】[2605.30613](https://arxiv.org/abs/2605.30613) —— KiroStudio 的多凭据池正落在这个威胁模型里。另一篇 17 家 provider 审计在 8 家检出缓存、**7 家检出全局跨组织共享**，结论"只允许 per-user 缓存"。【论文】[2502.07776](https://arxiv.org/html/2502.07776)
- **RFC 9111 的可迁移条款**：§3.3 允许存不完整响应但**必须标记 incomplete**，§3.4 明确 **MUST NOT 用它应答请求**；§2 允许负缓存；POST 属 unsafe method，"cache MUST write through" → 整个 L3 是在**违背 HTTP 默认语义**下运行，唯一正当性来自显式 opt-in。【官确】[RFC 9111](https://www.rfc-editor.org/rfc/rfc9111.html)
- **响应头用 RFC 9211 `Cache-Status`**（Standards Track），它已定义好 `hit` / `fwd` / `ttl` / `stored` / **`collapsed`（正是 single-flight 合并的标准上报位）**，不要自造 `X-Cache`。【官确】[RFC 9211](https://www.rfc-editor.org/rfc/rfc9211.html)
- **single-flight 的两个真坑**：① moka `try_get_with` 用 `Arc<E>` 共享错误 → follower 拿到 leader 的失败，但我们的失败是**凭据级**的（429/402/熔断），leader 的号挂了不代表 follower 该失败，必须让 follower 独立重走选号；② LLM 请求 P99 可达数十秒，follower 必须有独立超时。【官确】[moka 文档](https://docs.rs/moka/latest/moka/future/struct.Cache.html) + 【推测】
- **反面教材（务必记住）**：LiteLLM v1.91.0 把 `role:"system"` 的中途消息上提到顶层 system，CC 每约 3 轮发一条 → warm 命中率 **90% 掉到 25–45%**，团队日花费涨 2–3 倍。**代理任何"看起来无害"的消息搬移/重排都会摧毁前缀。**【官确】[事故报告](https://docs.litellm.ai/blog/bedrock-invoke-prompt-caching-incident)
- 另一条：LiteLLM 曾为兼容 Bedrock 无差别剥掉 `x-anthropic-billing-header` block，结果 CC 的工具安全分类器（仅带该一个识别信号）被上游 429，plan mode 全不可用。【实测】[issue 29572](https://github.com/BerriAI/litellm/issues/29572) → 本仓 `canonicalize_billing_header` 是**折叠为占位符**而非删除，方向正确。

---

## 2. 未决问题（必须靠实验回答，不能靠调研）

| # | 问题 | 为什么调研答不了 | 影响 |
|---|---|---|---|
| 1 | 本仓各 entitlement（social / IDC / M365）的 `metadataEvent` 是否携带 `tokenUsage`？ | 三份实测互相矛盾，B 说企业 IDC 完全不发、C 说能读到 | 决定 L2 是"直接拿真值"还是"必须建影子模型" |
| 2 | 显式发 `cachePoint` 是省钱还是亏钱？ | B 实测读折扣 = 0、首写溢价 1.89× **净亏**；A 实测有 50% 折扣 | 决定 L0 是否要主动插 cachePoint |
| 3 | Kiro credit 与 token 是否有可辨识函数关系？ | 官方明说"按请求不按 token"、精度仅 0.01；先行者 kiro2cc-proxy 已因反推发散而废弃该路 | 决定 credits 能否当命中率的度量仪 |
| 4 | 上游缓存真实 TTL 与隔离边界（是否按账号、是否跨 region） | Bedrock 文档说 account-specific + 5m，但 Kiro 是否代设 checkpoint、TTL 是否一致均无文档 | 决定 affinity TTL 与 failover 判据 |
| 5 | `clientCacheConfig.useClientCachingOnly` 的语义 | AWS Smithy 模型里只有 "Client cache config" 一句文档串 | 可能是个开关，也可能无关 |
| 6 | `contextUsageEvent.context_usage_percentage` 能否反推 warm prefix 长度 | 无任何公开资料 | 若能则可省掉自维护块索引 |

---

## 3. 收益上限的诚实估计

叠加三个独立来源的打折项：

- Mooncake 生产 trace 的可复用 KV 上限 ~50%（各工作负载 0–90%）
- CC 直连稳态命中 97–99%，但**首轮 79% 是 `cache_read=0`**、Haiku 子代理仅 58.1%
- "Don't Break the Cache" 结论：**几乎全部收益来自系统提示**，各策略间只差 2–4 点

**结论**：稳态轮次可逼近 95–99%（前提：attribution nonce 与动态段规范化 + 按 CC marker 切段），但"新会话首轮"与"子代理首调"结构性冷启且占比可观。**不要把它们算进命中率目标。** 若未决 #2 的答案是"净亏"，则 L0 的全部价值退化为"稳前缀以蹭上游隐式缓存"，收益锚点应按 `converter.rs:747` 已实测的 47% credit 折扣估。

---

## 4. 参考资料索引

**官方规范**：[Anthropic prompt-caching](https://platform.claude.com/docs/en/docs/build-with-claude/prompt-caching) · [pricing](https://platform.claude.com/docs/en/docs/about-claude/pricing) · [rate-limits](https://platform.claude.com/docs/en/api/rate-limits) · [messages API](https://platform.claude.com/docs/en/api/messages) · [cache-diagnostics](https://platform.claude.com/docs/en/docs/build-with-claude/cache-diagnostics) · [token-counting](https://platform.claude.com/docs/en/docs/build-with-claude/token-counting) · [CC prompt-caching](https://code.claude.com/docs/en/prompt-caching) · [CC LLM gateway protocol](https://claude-code.mintlify.app/en/llm-gateway-protocol) · [Bedrock prompt-caching](https://docs.aws.amazon.com/bedrock/latest/userguide/prompt-caching.html) · [Kiro billing FAQ](https://kiro.dev/docs/cli/billing/related-questions/) · [Kiro models](https://kiro.dev/docs/models/) · [RFC 9111](https://www.rfc-editor.org/rfc/rfc9111.html) · [RFC 9211](https://www.rfc-editor.org/rfc/rfc9211.html) · [RFC 8785 JCS](https://www.rfc-editor.org/rfc/rfc8785.html)

**上游 Smithy 模型（最关键）**：[CachePoint](https://github.com/aws/amazon-q-developer-cli/blob/main/crates/amzn-codewhisperer-streaming-client/src/types/_cache_point.rs) · [TokenUsage serde](https://github.com/aws/amazon-q-developer-cli/blob/main/crates/amzn-codewhisperer-streaming-client/src/protocol_serde/shape_token_usage.rs) · [ClientCacheConfig](https://github.com/aws/amazon-q-developer-cli/blob/main/crates/amzn-codewhisperer-streaming-client/src/types/_client_cache_config.rs) · [user-service-2.json](https://github.com/aws/aws-toolkit-vscode/blob/master/packages/core/src/codewhisperer/client/user-service-2.json)

**论文**：[Preble 2407.00023](https://arxiv.org/html/2407.00023v2) · [RadixAttention 2312.07104](https://arxiv.org/html/2312.07104v1) · [Mooncake 2407.00079](https://arxiv.org/html/2407.00079v1) · [Randomized KV eviction 2601.18999](https://arxiv.org/html/2601.18999v1) · [DualMap 2602.06502](https://arxiv.org/html/2602.06502) · [LMCache 2510.09665](https://arxiv.org/html/2510.09665) · [Don't Break the Cache 2601.06007](https://arxiv.org/html/2601.06007v2) · [vCache 2502.03771](https://arxiv.org/html/2502.03771) · [语义缓存 NP-hard 2603.03301](https://arxiv.org/html/2603.03301v1) · [CacheProbe 2605.30613](https://arxiv.org/abs/2605.30613) · [17-provider 审计 2502.07776](https://arxiv.org/html/2502.07776)

**生态实现**：[kiro2cc-proxy cache 模块](https://github.com/TsinHzl/kiro2cc-proxy/blob/main/docs/Kiro缓存与状态管理解析.md) · [kirocc cache_points](https://github.com/d-kuro/kirocc/blob/main/internal/reqconv/cache_points.go) · [kiro-claude-bridge 实测](https://github.com/AlqattanDev/kiro-claude-bridge/blob/main/CONTEXT.md) · [LiteLLM caching](https://github.com/BerriAI/litellm/blob/main/litellm/caching/caching.py) · [LiteLLM 缓存事故报告](https://docs.litellm.ai/blog/bedrock-invoke-prompt-caching-incident) · [LiteLLM prompt_caching_cache.py](https://github.com/BerriAI/litellm/blob/main/litellm/router_utils/prompt_caching_cache.py) · [SGLang cache_aware.rs](https://github.com/sgl-project/sglang/blob/main/sgl-model-gateway/src/policies/cache_aware.rs) · [vLLM prefix caching 设计](https://docs.vllm.ai/en/latest/design/prefix_caching/) · [claude-code-cache-analysis](https://github.com/ArkNill/claude-code-cache-analysis) · [claude-code-cache-fix](https://github.com/cnighswonger/claude-code-cache-fix)
