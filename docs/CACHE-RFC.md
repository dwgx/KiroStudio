# RFC: KiroStudio 缓存系统

> 状态：草案 · 2026-07-28 · 依据 `docs/CACHE-RESEARCH.md`
> 实施顺序：**Phase 0 实验 → L2 度量 → L0 前缀稳定 → L1 调度 → L3 本地缓存**
> 每一层都有独立的 kill switch，且都能单独验证收益。

## 核心原则

1. **先睁眼再动手。** 现网从未解析 `metadataEvent`，也从未度量 continuationId 的真实命中率。在拿到真值之前，任何"优化"都无法验证。
2. **绝不猜测计费。** 三份生态实测互相矛盾（读折扣 0% / 50%，首写溢价 1.89×），本仓凭据类型未知属哪档。全部靠自己的 A/B 定论。
3. **停止污染优先于新增功能。** 当前虚构的 `cache_read` 会让 `message_start.input_tokens` 塌成 0，并抬高 CC 上下文进度条提前触发 auto-compact。这是要先拆的炸弹。
4. **只追加、绝不改中段。** 这是 L0 唯一的不变量。无 KV 访问权时"历史中段被改动"在学术上无解（CacheBlend 类方法全需改 attention）。
5. **不做语义近似缓存。** 任何阈值都不做。理由见研究报告 §1.6。

---

# Phase 0 — 前置实验（不改一行业务代码）

目标：回答研究报告 §2 的 6 个未决问题。除 EXP-1 需要一个临时补丁，其余全部靠外部脚本。

## EXP-0：睁眼看上游到底发了什么（最高优先，零风险）

**动作**：在 `src/kiro/model/events/base.rs:105-128` 的 `parse_event` 里，把 `EventType::Unknown` 分支从静默丢弃改为 `tracing::warn!` 打印 `event_type` 字符串与 payload 前 512 字节；同时在 `EventType::from_str`（`base.rs:26-32`）补 `metadataEvent` 分支。

**为什么这是第一步**：AWS 官方 Smithy 模型里 `MetadataEvent.tokenUsage` 携带 `uncachedInputTokens` / `cacheReadInputTokens` / `cacheWriteInputTokens`。若上游在发，我们直接拿到真值，整个 L2 的影子模型都不必建。

**判定**：跑 20 个真实请求，覆盖 social / IDC / M365 三类凭据各若干。
- 若 `metadataEvent` 出现且带 `tokenUsage` → **未决 #1 = 有真值**，L2 直接用，跳过影子估算。
- 若只在部分凭据类型出现 → 按 entitlement 分档，L2 需真值 + 回落两条路。
- 若完全不出现 → 才走影子模型，且必须先做 EXP-2 判断 credits 能否当仪器。

## EXP-1：credit 函数标定（回答未决 #3）

**背景约束**：Kiro 官方明说"按请求不按 token"、credit 精度仅 **0.01**；先行者 kiro2cc-proxy 已于 2026-06-25 主动废弃 credits 反推（理由：大请求上 `baseline − input_credits` 趋零、反推发散）。所以本实验的**首要目的是判断这条路是否可行**，而不是假定可行去拟合。

**方法**：固定 model = Sonnet 5（倍率 1.3×，最常用），固定 `max_tokens=1` 且 prompt 末尾要求"只回一个字"以压制输出方差。自变量为输入 token 档位：`{1k, 2k, 4k, 8k, 16k, 32k, 64k, 128k}`，每档 5 次，全部用**全新随机 UUID 前缀**确保零缓存。

**观测**：`meteringEvent.usage`（credits）与本地字符数。

**判定**：
- 若 credit 在 8 个档位上取值数 ≤ 3（例如全是 0.13 / 0.26 / 0.39）→ **credit 是粗粒度阶梯，不可做命中率仪器**，EXP-2 改用 TTFT 作因变量。
- 若 credit 随 token 单调且能拟合出斜率（R² > 0.9）→ 可反推，记 `cpt(model) = credits / tokens`。

## EXP-2：上游是否真有缓存 + TTL + 隔离边界（回答未决 #2 #4）

方法照搬 "Don't Break the Cache"（arXiv 2601.06007）的 **UUID 强制边界法**：用随机 UUID 制造互不共享的前缀，靠对照组差值识别缓存。

固定条件：Sonnet 5、10k token 稳定系统前缀、`max_tokens=1`、单凭据、同 region。

| 组 | 构造 | 重复 | 预期观测（若有缓存） |
|---|---|---|---|
| A 基线 | 每次全新 UUID 前缀 | 10 | credits 恒定 = 全价 |
| B 同前缀连打 | 同一前缀，间隔 5s | 10 | 第 1 次全价，第 2 次起下降 |
| C TTL 探测 | 同一前缀，间隔 `{30s, 2min, 4min, 6min, 10min, 30min}` | 每档 3 | 找到 credit 回升的拐点 = 真实 TTL |
| D 改 1 字节 | B 之后把前缀末尾改 1 字节 | 5 | 若回升 = 确认字节级敏感 |
| E 换凭据 | B 之后同前缀换另一个号 | 5 | 若回升 = 确认按账号隔离 |
| F 换 region | B 之后同前缀换 region | 5 | 若回升 = 确认按 region 隔离 |
| G 显式 cachePoint | B 组 + `Tool[].cachePoint` | 10 | **关键**：对比 B 组判断省钱还是亏钱 |
| H continuationId | 固定 vs 每次随机 continuationId | 各 10 | 验证 `converter.rs:747` 记录的 47% 折扣 |

**因变量**：credits（若 EXP-1 判定可用）+ TTFT（`first_token_ms`，始终可用）。两者取其一显著即可定论。

**G 组的判定尤为关键**：kiro-claude-bridge 实测 CACHE-ON = `0.371, 0.196, 0.196, 0.196` vs CACHE-OFF 全 `0.196` → 读折扣 0、首写溢价 1.89×、**净亏**。若本仓复现该结果，则**放弃显式 cachePoint**，L0 退化为纯"稳前缀蹭隐式缓存"。

**安全护栏（必须遵守）**：
- 单凭据 QPS ≤ 0.2（每 5s 一发），总计 ≤ 350 次请求
- 只用 2 个专用测试凭据，不动生产池
- 遇到任何 429 / `USER_REQUEST_RATE_EXCEEDED` 立即停 5 分钟（依据 `handlers.rs:459-461` 已实测的 2 分钟状态型惩罚窗口）
- 预算估算：350 次 × Sonnet 5 约 0.2 credit ≈ **70 credits**，加 TTL 组的等待时长约需 3 小时挂机

## EXP-3：MetadataEvent 补充字段探测（回答未决 #5）

在 EXP-0 已打开日志的基础上，一次请求里带 `clientCacheConfig: {useClientCachingOnly: true}`，看上游是否 400 / 行为是否变化。低优先，纯探索。

---
# L2 — 真实度量层

**目标**：让"缓存命中"第一次成为可测量的事实。这一层做完之前，L0/L1/L3 的任何改动都无法验证收益。

## L2-1 止损：停止注入虚构值（先做，独立可发）

**改动**：
- `handlers.rs:901-914` 与 `1813-1826`：`cache_breakdown` 恒 `None`。
- 结果：`billed_input_tokens`（`stream.rs:159-168`）退化为恒等，`message_start.usage` 不再出现 cache 字段，buffered 路径回填的 `contextUsageEvent` 准确值不再被污染。
- 前端：`usage-page.tsx` 的 `cacheHitPct` 卡片与 `overview-page.tsx:641-642` 的 24h 命中率**下线**（不是显示 0，是移除），`cache_read/creation` 列复用已有文案 `zh.json:1392`"上游不提供"。
- `record.rs` 新增 `cache_source: CacheSource`（`Upstream` / `Estimated` / `None`），落库 + 前端据此区分，**避免历史数据无法回溯甄别**。

**为什么选"不注入"而非"注入 0"或"注入并标注"**：
1. 官方明说"两项皆 0 即未缓存"，0 是语义合法的诚实表达；当前值是语义**非法**的（creation 恒 0 + read 恒 >0 这个组合在官方语义下不可能出现）。
2. Anthropic usage 结构里**没有** `estimated` 字段，"标注估算"在协议层无处可标，客户端与 OpenAI 兼容层照旧被污染。
3. 最重要：消除 `message_start.input_tokens` 塌成 0 的缺陷，以及 CC 上下文进度条被抬高提前触发 auto-compact 的行为缺陷。

**风险**：CC 的 `usage.input_tokens` 缺失会崩（issue 46932），但本改动**只移除 cache_* 字段、保留 input_tokens**，不触发该路径。回滚开关：`promptCacheEnabled` 复活为真开关（见下）。

## L2-2 上游真值接入（EXP-0 判定为"有真值"时）

- `events/base.rs` 新增 `EventType::Metadata` + `MetadataEvent{ token_usage: Option<TokenUsage> }`，`TokenUsage{ uncached_input_tokens, cache_read_input_tokens, cache_write_input_tokens, output_tokens, total_tokens }`。
- `stream.rs` / `handlers.rs` 的 usage 解析优先级：**`metadataEvent.tokenUsage` 真值 → `contextUsageEvent` 反推 → 本地估算**。三级降级，每级在 `RequestRecord.cache_source` 留痕。
- 映射到 Anthropic 口径（严格满足官方划分等式）：
  ```
  cache_read_input_tokens     = tokenUsage.cacheReadInputTokens
  cache_creation_input_tokens = tokenUsage.cacheWriteInputTokens
  input_tokens                = tokenUsage.uncachedInputTokens
  ```
  三者直接对应，**不需要任何减法**，这也是为什么真值路径天然消除 L2-1 的所有问题。
- `Unknown` 事件分支永久保留 warn 日志（EXP-0 的产物转正），未来上游新增事件类型不再静默丢弃。

## L2-3 影子模型（仅当 EXP-0 判定"无真值"时才做）

若上游确实不发 `tokenUsage`，才建影子估算。**门槛远高于现状** —— 以下八条缺一即不注入：

1. `continuationId` 命中：同一派生值（`converter.rs:751`）在本进程有过成功请求
2. 同凭据：`meta.credential_id` 与上次一致（上游按账号隔离）
3. 同 region
4. TTL 内：距上次成功 < `promptCacheTtlSeconds`（默认改 300s，对齐上游 5m）
5. 前缀逐字未变：对规范化后的 `tools + system + messages[..n-1]` 做 SHA256 比对，任一变更 → 记 `creation` 而非 `read`
6. 前缀估算 ≥ 该模型最小可缓存长度（Opus 5=512 / Sonnet 5=1024 / Opus 4.5,4.6,Haiku 4.5=4096，从 `model_catalog` 取）
7. 首次建缓存必报 `cache_creation` 而非 0
8. 估算器修好（见 L2-4）

`promptCacheEnabled` 从死配置变成真开关，控制这条路径，**默认 false**。

## L2-4 估算器修正（L2-3 的前置，也独立有价值）

`token.rs` 现状误差符号在两类流量上相反（中文高估 55%，英文/代码低估 23–41%，单工具低估 3.6 倍），且有结构性漏计。修正：

- 注释与代码对齐（`token.rs:75` 的 4.5 vs `:84` 的 4.0）
- **去掉 <800 的分档系数**（`token.rs:88-98`）：它导致"同文本拆块比整块多 20%"，CC 的几百条小消息全落 <100 档被整体 ×1.5
- 按脚本分别标定 bytes-per-token：ASCII 散文 0.363、代码 0.324–0.427、CJK 0.643（依据实测 gist）
- **补齐漏计**：`count_all_tokens_local`（`token.rs:208-213`）除 `"text"` 外增加 `tool_result.content` / `tool_use.input` / `image.source`（按 base64 长度折算）/ `thinking`
- **修口径不对称**：`count_prefix_tokens`（`token.rs:249-250`）把 `tools` 计入前缀（现在两次调用都传 `None`，而分母含 tools）
- 单工具开销校准到 ≈389 token 量级（官方 count_tokens 示例实测），而非按 JSON 字符数

## L2-5 前缀指纹落库（度量基线）

按 vLLM 块哈希链做**离线可复算的命中率基线**：
```
块 = 512 字符（不用 vLLM 的 16 token —— 上游最小可缓存前缀是 512–4096 token，更细无收益）
h_i = xxh3(h_{i-1} ‖ block_i)   // xxh3 31.5 GB/s vs SHA256 0.3 GB/s，200KB prompt 全量 ~6µs
只有满块入链，尾部残块不参与
salt = credential_id            // 等价 vLLM 的 cache_salt，不同号缓存不相通
```
`trace_db` 新增列：`prefix_fp`（末块哈希）、`prefix_blocks`（块数）、`prev_same_fp_ms`（距上一次同前缀请求的毫秒差）、`cred_switched`（是否换号）。有了这四列，**任何历史数据都能离线复算"本来能命中多少"**，这是 L0/L1 每次改动的验证仪器。

## L2 验收指标

- `cache_source` 分布可见：真值占比 / 估算占比 / 无
- 面板不再显示任何未标注来源的命中率
- `Unknown` 上游事件类型 100% 有日志
- 离线可对任意时间窗复算前缀可命中率

---

# L0 — 前缀稳定引擎

**目标**：把"请求字节前缀"当缓存键来工程化。**唯一不变量：只追加，绝不改中段。**

## L0-1 修致命项：thinking 前缀内联 budget（🔴）

`converter.rs:1257-1277` 把 `budget_tokens` / `effort` 内联进字符串，`converter.rs:1338-1341` 又把它拼到 system 注入的**最前面**。CC 的 adaptive budget 随轮次变化 → **从第一个字节起全废**，比官方语义的"messages 层失效"严重得多。

**修法**：
- 把 thinking 前缀从 system **头部**移到 system **尾部**（保住前面的静态 system 与 tools 前缀）
- budget 数值**量化分桶**（如 `{4k, 8k, 16k, 32k, 64k}` 就近取整）而非原样内联，减少漂移频率
- effort 保持原样（它本来就该进 cache key，官方也是这么定的）

## L0-2 修致命项：压缩阈值悬崖（🔴）

`handlers.rs:433-445` 在请求体跨过 `trigger_bytes`（默认 4MiB）那一刻调 `compressor.rs` 改写历史中段 —— 4MiB−1B 正常、4MiB+1B 全前缀失效。LMCache 实测同类滑窗截断把命中率从 ~85% 打到 ~45%。

**修法**：
- 触发点后移到**尽可能接近上游硬限**（实测约 5MiB），`trigger_bytes` 默认从 4MiB 提到 4.8MiB
- 压缩顺序改为**从最旧的历史开始**而非全量扫（最旧的中段已经在 20-block 回溯窗口外，改动它的边际损失最小）
- 触发时打 `warn` 日志并在 `RequestRecord` 标记 `prefix_broken_by=compression`，让度量层能看见

## L0-3 修高危项：历史中段原地删改（🟠）

`converter.rs:1129-1152 remove_orphaned_tool_uses` 用 `retain` 原地删历史中段的 tool_use（CC 中断/取消工具调用即触发）。这是为满足"Kiro 要求 tool_use 必须配对"的硬约束，不能直接不做。

**修法**：改为**补一个合成 tool_result** 而非删 tool_use。合成内容固定为字节稳定串（如 `[tool call cancelled]`），这样历史前缀是**追加**而非删除，同时满足上游配对要求。同理适用 `converter.rs:1390-1401` 的自动 `"OK"` assistant 配对 —— 改成幂等的固定串且下一轮真回复到位时不重排。

## L0-4 修高危项：图片跨历史去重（🟠）

`converter.rs:1362` + `1410-1418` 的 `image_dedup` 跨整个历史去重，同一张图第二次出现被替换为占位符 → 历史中段字节随后续轮次变化。

**修法**：去重集合的作用域从"整个历史"收窄为"仅当前轮"，历史中的重复图片保留原样。代价是 token 多花，但 `MAX_TOTAL_IMAGES=20` 已有上限兜底，且换回的是前缀稳定。**若实测证明 token 代价过大，则改为：去重决策只依赖该图片在历史中的首次位置（前缀单调），不依赖后续轮次。**

## L0-5 修中危项：conversationId 随机降级（🟡）

`converter.rs:637-641`：`extract_session_id` 拿不到 `metadata.user_id` → `Uuid::new_v4()` → continuationId 每请求全新 → **该客户端永久零命中**（hank9999/kiro.rs 正是这个反面对照）。

**修法**：照 kiro2cc-proxy 的做法，无 metadata 时退化为 `SHA256(截断后的 system prompt + 排序后的工具名集合)` 派生稳定 UUID。这样即使客户端不给 session_id，同一"工作上下文"仍归到同一会话键。

## L0-6 规范化增强

- **attribution 块**：`canonicalize_billing_header`（`converter.rs:383-389`）现在整块折叠为占位符，方向正确（**不要删除** —— LiteLLM 无差别删除导致 CC 工具安全分类器被 429、plan mode 全废）。增强：把 `cc_version=X.Y.Z.<buildhash>` 归一到 `X.Y.Z`，避免 CC 每次小版本升级全前缀失效。
- **消费客户端的 cache_control**：CC 下发的 marker 是**客户端亲口声明的"我认为到这里是稳定前缀"**，是免费的分段提示。当前 `converter.rs` 12 处全部写 `None` 丢弃。改为：读取并作为 L2-5 前缀指纹的**分段点**，同时（若 EXP-2 G 组判定 cachePoint 划算）映射为上游 `cachePoint`。
- **绝不做的事**：任何消息搬移/重排/上提。LiteLLM v1.91.0 把中途 system 上提到顶层，CC warm 命中率 90% → 25–45%，日花费涨 2–3 倍。
- **空 text block 清理**：`{"type":"text","text":"","cache_control":{...}}` 会让 Anthropic 端 400 且因进了 JSONL 而 `--resume` 永久复现。入口过滤掉。

## L0-7 前缀连续性断言

在 `debug_assert` 级别校验：同一 conversationId 的连续请求，其前缀指纹链（L2-5）必须是**单调延长**关系（新请求的块哈希链前 N 项等于上次的全部）。任何一次违反都打 `warn` 并记录破坏原因，使"将来再引入中段改写"立即可见而不是静默掉命中率。

## L0 验收指标

- 同 conversationId 连续请求的前缀指纹链单调延长率 ≥ 99%（离线由 L2-5 数据复算）
- `prefix_broken_by` 各原因的分布可见，压缩触发率 < 0.1%
- credit 折扣（EXP-2 H 组基线的 47%）在 A/B 中不退化

---

# L1 — 缓存感知调度

**目标**：让选号决策带上"缓存温度成本"。核心洞察（来自 DualMap 消融）：**不要每请求重算最优号，要粘住直到越过阈值。** min-cost 每请求重算会在 cache-aware 与 load-aware 间振荡并自己搅乱缓存。

## L1-1 亲和键升级

现状 `affinity.rs` 是 `conversationId → credential_id` 平坦映射、TTL 固定 30 分钟。改为三级：

- **L1 精确键** = `agentContinuationId`（不是 session_id —— 要对齐**真正发给上游的**那个键）
- **L2 前缀指纹** = L2-5 的块哈希链。收益：CC 换 session 但 system+tools 不变时仍能粘回同号
- **L3 兜底** = `family_key`（同租户天然共享风控与 region）

## L1-2 TTL 对齐（现状是 6 倍超配）

上游 5m TTL 下，30 分钟 affinity 里**有 25 分钟是零缓存收益的纯负载倾斜** —— 既拿不到折扣，又放弃了健康号的选择自由。

改为**滑动 `last_hit` TTL** 而非固定 `first_bind` TTL（依据：5m 缓存每次命中免费续期，活跃会话可无限续期，静默会话应快速释放）。分档：

| 档 | 条件 | temp | 行为 |
|---|---|---|---|
| hot | `age < 4min` | 1.0 | 亲和权重满额，允许短暂等待 |
| warm | `4–8min` | 0.35 | 存在但可能已失效，期望值折算 |
| cold | `> 8min` | 0 | 亲和解除，退回纯健康/负载选号 |

`affinityTtlSeconds` 默认 360（6min，替代现 1800）。参照实现：LiteLLM `PromptCachingDeploymentCheck` 用 `sha256(可缓存前缀) → model_id`、**ttl=300s**，是与本场景最接近的可抄实现。

**EXP-2 C 组测出的真实 TTL 若与 5m 不同，以实测为准调整这三档。**

## L1-3 硬门 + 二选一（不做连续加权）

四家独立实现（Preble / SGLang / Ray Serve / DualMap）**全部是硬门结构，没人做连续加权**。照此：

```
// 硬门（沿用现有）：selectable / circuit != Open / cooldown 未生效 / model 未 block
// 负载倾斜门（SGLang 口径）
if (max_inflight - min_inflight) > BALANCE_ABS && max_inflight > min_inflight * BALANCE_REL {
    → 走最短队列（放弃缓存亲和）
}
// 否则：缓存优先（Preble E2）
if matched_prefix_tokens >= 0.5 * total_prompt_tokens && temp(bound_cred) > 0 {
    → exploit：粘住 bound_cred
} else {
    → explore：回落现有 8 键 balanced 打分
}
```

默认参数（取 Preble 与 SGLang 两处独立收敛的同一值）：`cacheThreshold=0.5`、`balanceAbs=32`、`balanceRel=1.1`、`capFactor=1.5`（Envoy `hash_balance_factor` 建议区间 120–200 的中位）。

## L1-4 failover 时"是否值得等"

三条**全满足**才等：
1. `wait_secs × BETA < cache_loss(换号)`
2. `wait_secs ≤ hardWaitCap`（默认 **3s**）
3. 等待时间**有界且已知**（`retry_after` 头存在，或 RPM 桶残量可算）

`BETA` = 延迟的 credit 等价，默认 `0.02 credit/s`，做成配置项。

**必须立刻换号不许等**：熔断 Open（等待无界）、配额耗尽 / 账号封禁 / refreshToken 失效（期望收益 0）、`model_blocklist` 命中。

**degraded 与 Open 要分开**：健康分低但未跳闸 → **保亲和**（宁可用降级号保缓存）；跳闸 → 立即失效亲和。这与 Envoy `stateful_session_filter` 的语义一致（host 标记 degraded 但已有 session 继续路由到它）。

## L1-5 驱逐用随机化叶驱逐

前缀索引的驱逐**不要用纯 LRU**：Leaf-LRU 最坏 Θ(n) 竞争比，随机化叶驱逐 Θ(log(B−L)) 且有匹配下界。实测开销 0.71–1.05ms vs LRU 0.09–0.13ms —— 用微秒级代价换指数级最坏情况改善。索引上限 `10000` 节点 / `30s` 定期驱逐（SGLang 默认）。

## L1 验收指标

- sticky 命中率（`sticky_hits / (hits+misses)`）可观测
- 换号导致的缓存重建量（token · 次）可观测
- A/B：开启 L1 后同等流量的 credits 总量下降，且 p99 延迟不劣化超过 10%

---

# L3 — 本地响应缓存

**推荐档位（唯一，不提供选项）**：**默认关闭 + 严格字节精确 + 白名单准入 + 单飞合并 + 只缓存已完成的非工具流。**

明确**不做**：语义近似缓存（任何阈值）、负缓存（除确定性 400）、缓存任何含 `tool_result` 的请求。

## L3-1 准入白名单（全部满足才可缓存）

- ✅ 无 `tool_result`（**最硬红线**：agent 循环中同前缀+同 tool_result 再现意味着 agent 在原地打转，命中缓存会把它锁进死循环。Agentic Plan Caching 实测指纹层 69.3% 命中率但**精确率仅 48.1%**）
- ✅ 无 `thinking`（`signature_delta` 是完整性签名，回放跨会话签名不可控）
- ✅ 未显式携带采样参数（注意：`temperature=0` 判据**已过时** —— Claude 4.7+ 传非默认值直接 400，所以判据是"是否携带"而非"是否为 0"）
- ✅ 上游返回了完整流（有 `message_stop`、`stop_reason` 非 null）。RFC 9111 §3.4 明确 MUST NOT 用不完整响应应答请求
- ⚠️ 带 `tools` 的请求技术上可缓存但**收益趋近 0**（coding agent 流量几乎全是长且各不相同的前缀），默认排除

## L3-2 cache key 规范

```
key = sha256(inbound_api_key) ‖ sha256(JCS(canonical_body))
```

- **入站 key 哈希作强制前缀，不提供关闭开关。** 依据：CacheProbe 论文实测经共享凭据池代理时 Groq/Fireworks/OpenAI 三家全部出现跨账号缓存共享，根因是**凭据池化本身**；另一篇 17-provider 审计在 7 家检出全局跨组织共享，结论"只允许 per-user 缓存"。KiroStudio 的多凭据池正落在这个威胁模型里。
- Portkey 的 `cache-namespace` / Helicone 的 `Cache-Seed` 是同一思路但**由调用方自愿提供** —— 这正是不该照抄的地方（自愿即等于不隔离）。
- **必须进 key**：model、system、messages 全文、tools 全文、max_tokens、thinking(mode+budget)、tool_choice、beta headers
- **必须排除**：`metadata.user_id`、request_id、`<system-reminder>` 时间戳、每请求漂移的 env 块
- **JCS（RFC 8785）只用于算 key，绝不用于转发。** 它按 UTF-16 code unit 排序对象键，恰好解决官方点名的"Go/Swift map 序列化随机化 key 序打破缓存"问题；但它会重排属性，与上游看到的原始字节序不同 → canonical 形式只喂哈希，转发仍用原始 body。
- **哈希禁止截断。** 两个真实 CVE：vLLM 用 Python 内置 `hash()` 可构造碰撞返回别人内容生成的 KV（CVE-2025-25183）；LiteLLM 用 `token[:20]` 导致攻击者继承他人身份（CVE-2026-35030）。

## L3-3 流式缓存与回放

**存语义事件，不存原始 SSE 字节。** 理由：Anthropic SSE 有严格结构且 `message_delta.usage` 是累积值，新事件类型会持续新增；存字节则版本升级后无法改写 usage/id。

回放三条硬规则：
1. `message.id` **必须重新生成**（复用旧 id 破坏客户端按 id 去重/落库）
2. `usage` **必须改写**：命中时真实 credit = 0，`RequestRecord.credits_used = 0` 且打 `cache_hit` 标记，否则用量统计与余额预测全部失真
3. **不要模拟 token 节奏**。LiteLLM 把文本切成 5 字符片段 + `sleep 0.02s` 伪装流式，纯装饰且引入延迟。一次性发全量 delta 更好。

语义目标（LangChain 的表述最准）："warm cache 必须产生与 cold call 相同的事件流，消费者不应观察到依赖缓存状态的行为。"

## L3-4 single-flight

用 `moka::future::Cache::try_get_with`（并发调用合并为一次 init future，其余等待；`Err` 时不插入）。两个必须自己处理的坑：

1. **不要透传 leader 的错误。** moka 用 `Arc<E>` 共享错误，但我们的失败是**凭据级**的（429/402/熔断）—— leader 用的号挂了不代表 follower 该失败。follower 在 leader `Err` 时必须**独立重走选号路径**。
2. **follower 必须有独立超时。** LLM 请求 P99 可达数十秒。Caffeine FAQ 记录同族问题：并发计算数逼近容量时阻塞 map 扩容，表现为"不同 key 的线程阻塞在同一把锁上"。

注：Cloudflare AI Gateway 直言其缓存 volatile、**两个并发相同请求会双双打到上游**（根本没做合并）。若我们要做就要做对。

## L3-5 可观测性

- 响应头用 **RFC 9211 `Cache-Status`**（Standards Track）而非自造 `X-Cache`。它已定义好 `hit` / `fwd` / `fwd-status` / `ttl` / `stored` / **`collapsed`（single-flight 合并的标准上报位）** / `key` / `detail`。安全注记同样有用：该头可泄露缓存行为用于时序攻击，`key` 参数应限授权客户端。另加 `X-KiroStudio-Cache-Hit` 兼容前端。
- 旁路三件套（照搬 LiteLLM 语义）：`no-cache`（跳读）/ `no-store`（跳写）/ `s-maxage`（只接受 N 秒内）
- **影子抽检**：命中时按 1% 比例仍真打上游，丢弃结果只记差异。注意因 batch-invariance 问题，比较基准只能是"语义/工具调用是否一致"，**不能是逐字节相等**。

## L3-6 为什么默认关闭不可让步

RFC 9111 §4：POST 属 unsafe method，"a cache MUST write through requests with methods that are unsafe to the origin server"。**整个 L3 是在违背 HTTP 默认语义下运行的**，唯一正当性来自用户显式 opt-in + 严格准入白名单。四家主流网关全部 opt-in，不是巧合。

## L3 验收指标

- 命中率、`collapsed` 合并率、影子抽检不一致率（目标 < 1%，超过则关闭）
- 命中请求的 `credits_used = 0` 且不进健康统计
- 零跨租户串话（单测覆盖：两个不同 inbound key 的相同 body 必须 miss）

---

# 新增配置项汇总

| 名称（camelCase） | 默认 | 层 | 理由 |
|---|---|---|---|
| `promptCacheEnabled` | `false` | L2 | 从死配置复活为真开关，控制影子估算路径 |
| `promptCacheTtlSeconds` | `300` | L2 | 从 3600 改为 300，对齐上游 5m（EXP-2 C 组实测后修正） |
| `upstreamCacheSignalEnabled` | `true` | L2 | 解析 `metadataEvent.tokenUsage`；零风险故默认开 |
| `prefixFingerprintEnabled` | `true` | L2 | xxh3 块哈希链，200KB prompt ~6µs，开销可忽略 |
| `explicitCachePointEnabled` | `false` | L0 | **必须等 EXP-2 G 组判定** —— 生态实测可能净亏 1.89× |
| `compressionTriggerBytes` | `4.8MiB` | L0 | 从 4MiB 提高，把前缀悬崖推到尽可能靠后 |
| `affinityTtlSeconds` | `360` | L1 | 从 1800 改为滑动 6min，消除 6 倍超配 |
| `cacheAwareSchedulingEnabled` | `true` | L1 | Preble/SGLang 双重收敛的成熟策略 |
| `cacheThreshold` | `0.5` | L1 | Preble E2 与 SGLang 独立收敛的同一值 |
| `balanceAbs` / `balanceRel` | `32` / `1.1` | L1 | SGLang 生产默认 |
| `hardWaitCapMs` | `3000` | L1 | DualMap"到 SLO 门才换"的等价物 |
| `betaCreditPerSecond` | `0.02` | L1 | 延迟的 credit 等价，用于统一打分 |
| `responseCacheEnabled` | `false` | L3 | 违背 HTTP 默认语义，必须显式 opt-in |
| `responseCacheTtlSeconds` | `300` | L3 | 与上游 TTL 同量级 |
| `responseCacheShadowSampleRate` | `0.01` | L3 | 影子抽检比例 |

---

# 分叉预案

RFC 的每一层都依赖 Phase 0 的实验结论。三种分叉：

**分叉 A — EXP-0 发现上游发 `tokenUsage`（最好情况）**
L2-3 影子模型整个不做，L2-4 估算器修正降为可选。直接拿真值填 usage，L0/L1 的验证仪器从"离线复算"升级为"在线真值"，整个计划提速一倍。

**分叉 B — EXP-2 G 组判定显式 cachePoint 净亏（生态实测已有先例）**
`explicitCachePointEnabled` 永久保持 false，L0 退化为纯"稳前缀蹭上游隐式缓存"。收益锚点按 `converter.rs:747` 已实测的 47% credit 折扣估，L0-1～L0-7 的价值不变（它们本来就是为了不打破隐式缓存）。

**分叉 C — EXP-2 B 组显示上游根本没有任何前缀缓存（最坏情况）**
即同前缀连打 credits 恒定不降。则：
- L0 的价值退化为纯 token 节省（省的是 `strip_env_noise` 那部分字节），仍值得做但优先级降低
- L1 缓存亲和整个失去意义 → 只保留 TTL 对齐这一项（30min 超配本身就是纯负载倾斜，无论有无缓存都该修）
- **L3 反而升为最高优先** —— 上游既然不给缓存折扣，网关自己缓存就是唯一的省钱手段
- 同时需要解释 `converter.rs:747` 那次 0.141→0.075 的观测是什么（可能是 credit 阶梯取整的假象，这也是 EXP-1 要先判断 credit 粒度的原因）

---

# 风险登记册

| 类型 | 风险 | 缓解 |
|---|---|---|
| 技术 | L2-1 移除 cache 字段后某客户端崩 | CC 只在 `usage.input_tokens` 缺失时崩（issue 46932），本改动保留该字段；`promptCacheEnabled` 作回滚开关 |
| 技术 | L0-3 合成 tool_result 被上游拒 | 先在测试凭据上验证合成串被接受，再上线；失败则回落现有 retain 行为并记录 `prefix_broken_by` |
| 技术 | L0-4 取消图片跨历史去重导致请求体变大撞 5MiB | `MAX_TOTAL_IMAGES=20` 已有上限；L0-2 的压缩兜底仍在 |
| 技术 | L1 亲和过粘导致单号被限流打爆 | 硬负载倾斜门（`balanceAbs/balanceRel`）+ `capFactor=1.5` 上限；Preble 的 prefix autoscaling 思路：热前缀主动复制到 2–3 个号 |
| 技术 | 多副本部署时前缀索引不同步 | 反例警示：SGLang issue #12700 两个 router 树不同步导致**发到模型层的并发直接翻倍**（不只是命中率下降）。推荐方案 A：亲和不存状态，用 CHBL(conv_id) 在凭据列表上排序，各副本独立算出同一顺序 |
| 语义 | L3 命中改变模型不确定性语义 | 默认关闭 + `Cache-Status` 头明示 + 1% 影子抽检；文档明写"这是行为变更不是无损优化"（temp=0 本身就不确定，见 Thinking Machines 实测） |
| 语义 | L2-4 估算器修正改变历史数据可比性 | `cache_source` 字段留痕 + 修正版本号，面板按版本分段展示 |
| 合规/风控 | Phase 0 实验触发上游风控 | 专用测试凭据、QPS ≤ 0.2、遇 429 停 5min、总量 ≤ 350 请求 ≈ 70 credits |
| 安全 | L3 跨租户串话 | `sha256(inbound_api_key)` 强制前缀无开关；单测覆盖；CacheProbe 论文点名的正是我们这种共享凭据池结构 |
| 安全 | 哈希碰撞 | 全量 sha256（key）/ xxh3（前缀指纹，非安全边界）；禁止任何截断 —— 两个真实 CVE 都是截断哈希 |
| 运营 | 一次改太多导致无法归因 | 每层独立 kill switch；L2 先于一切（没有仪器就没有结论）；每层单独 A/B |

