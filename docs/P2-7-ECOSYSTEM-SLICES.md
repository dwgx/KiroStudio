# P2-7 生态四点 — 切片设计（2026-08-21）

对照 ZyphrZero/kiro.rs v0.7.6 / ISSUES (d)。本页是设计，不是发版授权。
12 键选号元组、absorb 循环顺序、AIMD、sticky 语义本切片不改。

## 1. OpenAI 入站会话亲和（本波可做）

**缺口：** `/v1/chat/completions` 不把客户端会话头写入 Anthropic
`metadata.user_id`，Kiro `conversationId` 走 converter 派生/随机，上游
prefix cache 吃亏。

**锁定：** 按顺序取第一个像 UUID 的值：
`prompt_cache_key` JSON 字段 → 头 `x-session-affinity` →
`x-client-request-id` → JSON `session_id`。写入翻译后的 Anthropic
`metadata.user_id`（现有 `extract_session_id` 已认 UUID）。未命中则保持
现状派生。具名测试四来源 + 非法值忽略。

**不做：** 改 Kiro 主路径 conversationId 算法。

## 2. 模型感知正向路由（轻量，另波）

ISSUES (d) 已锁：透传池三态 Confirmed/Unknown/Unsupported + TTL +
`fetch_upstream_models` 预热不进选号热路径。排序键首位 `support_rank`
是**有意行为变化**。本波只出设计，不改 12 键。

## 3. 客户端 Key（另波，要 Owner 点名）

ISSUES (d) 一期：`ClientKeyManager` + 收编主 key + `KeyContext` + usage
`client_key_id`。Key 前缀名 **不在本页发明**（grill：命名必须问）。
二期 bound_credential_ids / spendingLimit 不做。

## 4. GPT 族 reasoning effort（另波）

Zyphr：GPT-5.6 走 `additionalModelRequestFields.reasoning.effort`。
我们 converter 只有 Claude `output_config.effort`。切片：catalog 标
GPT 族时改写 effort 通道。不改 absorb。
