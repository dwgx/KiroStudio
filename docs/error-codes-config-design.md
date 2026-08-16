# 错误码/提示词可配置化设计（docs/error-codes-config-design.md）

> 2026-08-15 设计定稿。依据三份研究：error-codes-inventory.md（77 条目 7 层清单）、
> error-codes-client-behavior.md（客户端源码级行为 + 12 条硬约束）、error-codes-config-mechanism.md（热重载机制）。
> **实现状态：已按本设计落地（2026-08-15 W7-W8）**：42 key 表 + per-key merge + TIER1 热加载 +
> 校验 + 7 处接入点 + 矛盾修复（B4/D10/F3）+ 前端弹窗；三轮对抗 review 全修；部署 a3ae8874。

## 目标

用户需求：① 所有错误码和提示词后台可配置；② 高级设置点击弹出新页面展示全部可编辑提示词；
③ 翻页 + 页面直接修改；④ 热加载；⑤ 对提示词规划优化。

## 一、配置结构（config.json 新字段）

```jsonc
"errorMessages": {
  // key = 错误形态标识（见下 key 集）；未配置的 key 用内置默认值（= 现状文案）
  "quota_exhausted":       { "status": 429, "type": "rate_limit_error",       "message": "…", "retryAfterSecs": null },
  "overloaded_capacity":   { "status": 503, "type": "overloaded_error",        "message": "…", "retryAfterSecs": 3 },
  "model_unsupported":     { "status": 404, "type": "not_found_error",         "message": "…", "retryAfterSecs": null },
  // 所有字段 Optional：None = 用内置默认（只改 message 时 status/type 不填）
}
```

- serde：`#[serde(default, skip_serializing_if = "Option::is_none")]`，字段 camelCase
- **默认值 = 现状**（从 error-codes-inventory.md 提取，A/B/D/E/F/G 七层全量，~61 key）
- key 集：`quota_exhausted` / `quota_subscription` / `overloaded_capacity` / `model_unsupported` /
  `request_body_invalid` / `context_too_large` / `upstream_timeout` / `upstream_5xx` / `auth_invalid` /
  `auth_expired` / `permission_denied` / `rate_limited_pool` / `rate_limited_credential` /
  `gate_timeout` / `absorb_exhausted` / `mcp_failed` / `websearch_failed` / `no_usable_pool` /
  `account_throttled` / `empty_response` …（完整清单由实现 agent 从 inventory 提取，命名按此风格）

## 二、语义不变量（锁死项，配置校验强制）

研究（client-behavior）的 12 条硬约束 H1-H12 落实为校验规则：

1. **status 白名单**：`[400, 401, 403, 404, 413, 429, 500, 502, 503]`（对齐 exhausted_status 先例）
2. **type 枚举白名单**：`invalid_request_error / authentication_error / permission_error / not_found_error / request_too_large / rate_limit_error / api_error / overloaded_error / billing_error`（Anthropic 官方 9 类）
3. **组合约束**：429 必须配 rate_limit_error 或 overloaded_error；401→authentication_error；403→permission_error；404→not_found_error；400/413→invalid_request_error 或 request_too_large
4. **Retry-After 范围**：0-3600；**号池真值 `retry_after_secs=N` 永远优先于配置**（代码层强制，配置只是兜底值）
5. **message 决策词黑名单**（研究确认会改变客户端决策的词，配置拒绝）：`credit balance is too low`、`organization has been disabled`、`overloaded_error` 字样、`quota`+`exhausted` 组合（除非 type 是 billing_error）、`billing` 组合（防 Claude Code CLI 层 7 次重试误触发）
6. **marker 内部协议不可配**：`subscription_unsupported=1` 等 11 个标记串永远由代码注入，不暴露给配置
7. **承重字符串保护**（6 处，改默认值时告警但允许）：`等容量`（kiro_shield 分类词）、`prompt is too long`（Claude Code 压缩判据）、英文背压哨兵等——校验时提示，不硬拒
8. **失败整表拒绝**：任一 key 校验失败 → 整表不生效（保持旧表），返回 400 + 具体错误

## 三、热加载（TIER1 ArcSwap，model_mapping 先例）

- `Config.error_messages: HashMap<String, ErrorMessageOverride>`（默认空表）
- **不新增 TIER3 镜像**：错误翻译处（map_provider_error 等）已持有 `Arc<Config>`（absorb_cfg 快照），
  每次请求从 config 快照查表（HashMap get O(1)，未命中用内置默认）——model_mapping 同款范式
- 热重载走现有 TIER1：admin PUT /config → 字段级 merge → reload_config 换 ArcSwap → 下次请求生效
- 前端保存后 toast + 即时生效（无需重启）

## 四、错误翻译接入点（~6 处，全部改读表）

1. handlers.rs `map_provider_error` 主干 + `translate_upstream_error` 链（quota/context/network 子链）
2. 吸收层 AbsorbBudgetExhausted（429/503）
3. 容量 400 / 空响应 429 / 入站闸门 503
4. 透传池错误（build_error_passthrough_response 的构造部分——**上游错误原文透传的保持原样**）
5. websearch 快路径 MCP 失败 502 + 回灌错误
6. OpenAI 层（G5/G6 透传内层同源，配置一层即覆盖）
7. **/v1 与 /cc/v1 双入口同读**（D 类本地错误两份复制，读同一配置）

每个接入点签名：`resolve_error_message(config, key, default) -> (status, type, message, retry_after)`
（未配置 key → 内置默认，零行为变化）

## 五、优化规划（并做，研究发现的现状问题）

1. **B4 容量 503 补 Retry-After**（现状无 RA，客户端不退避）
2. **D10 空响应 429 补 Retry-After**（现状无 RA）
3. **F3 websearch RA 硬编码 "8" 与 A1 常量不同源** → 统一走配置
4. **文案规范化**：口语化文案（"上游账号没了"类）→ 规范结构（原因 + 动作提示 + 可重试语义），
   作为默认表的新文案（用户可在后台再改）
5. **402 姿势**：client-behavior 研究修正——Claude Code CLI 层对 billing_error 会重试 7 次，
   k2cc 的 `quota_exceeded_error` 非官方 type 恰好 non_retryable。**本次只把「quota 类错误」默认
   type 可选改为 quota_exceeded_error（配置可设）**，完整 402 改造仍按 quota-402-design 单独评估

## 六、前端（高级设置 → 新页面弹窗）

- settings-page 高级设置区加「错误提示词」卡片入口（CARD_INDEX_DEFS 注册，SectionGate 分组）
- 点击打开**独立 Dialog 页面**（ops-detail-dialogs 的 TraceDetailDialog 页面化先例）
- **分页**：每页 10 条（翻页按钮 + 页码 + 总数），支持搜索过滤（按 key/status/文案）
- **直接编辑**：每行 status/type/message/RA 可编辑（输入框/下拉），改动本地暂存（脏标记），
  保存批量提交（PUT config 字段级 merge）→ toast + 热加载生效
- 只显示**已配置 + 默认值预览**（未配置 key 显示默认文案，可编辑保存后写入配置）
- i18n 三语（页面标题/提示词/按钮/校验错误提示）
- 配置校验错误回显（后端 400 的逐 key 错误）

## 七、交付顺序

1. A1 后端：config 结构 + 默认表 + 校验 + TIER1 接线（config.rs + service.rs + types.rs + example）
2. A2 错误接入：7 个接入点改读表 + 3 处矛盾修复 + 优化文案（handlers.rs + passthrough.rs + websearch.rs + openai/）
3. A3 前端：入口 + 弹窗 + 分页 + 编辑 + 三语（admin-ui/）
4. CI → 双 review（对抗）→ 落实核验 → 文档同步
