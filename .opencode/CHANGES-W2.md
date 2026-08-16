# 波次 2 改动清单（供 reviewer 审查用）

CI 基线：1766 → **1841 passed / 0 failed**（+75 测试，2026-08-15 服务器 Docker 实测）
审查对象 = 当前工作树代码（src/ + admin-ui/），本文件列出本次改动点。HEAD=1e100a2。

## 后端核心（anthropic 主路径）
- websearch.rs：handle_websearch_request 按 payload.stream 分支（非流式返回 JSON body，新函数 build_fast_path_json_body）；成功路径补 usage 埋点（emit_websearch_fast_path_usage，handlers.rs 新函数，带 client 参数）；回灌路径 tool_use input 非法 JSON 复用 stream::repair_tool_json，修不好置 stream_error 整轮报错；快路径 title 走 normalize_html_text。新守卫：fast_path_respects_stream_flag_and_emits_usage / replay_tool_input_repairs_json_or_fails_round
- handlers.rs：translate_quota_subscription 最前识别 subscription_unsupported=1 → 404 not_found_error（裸 "subscription" 兜底保留 502）；非流式空响应兜底（400 大输入 / 429 偶发，阈值共用 stream.rs empty_response_oversized_threshold，文案抽 empty_response_error_shape 共用）；非流式 thinking-only 补空格 text 块 + max_tokens（对齐流式 generate_final_events）；emit_websearch_loop_usage 加 wants_stream 参数写 is_streaming；删除死代码 extract_client_ip + 修正 3 处注释引用
- stream.rs：thinking 块文本计 output_tokens（6 处：process_reasoning_content + 各处 flush）；flush_tool_input 的 output_tokens 累加从函数入口移到实际下发前；empty_response_oversized_threshold 改 pub(crate)
- compressor.rs：repair_non_empty_content_pass 删 has_payload 门，content trim 空一律补占位符

## 后端核心（kiro）
- provider.rs：call_mcp_with_retry 加墙钟闸门（MAX_REQUEST_RETRY_BUDGET_SECS=45，首试豁免，复用对话路径常量）；upstream_trace 完整埋点：MCP+对话两路径各 1 个 FailureTraceGuard（初值 VERDICT_UNCLASSIFIED）+ 网络错误/成功独立 emit，verdict 分类表 17 分支；Retry-After 原始串+解析值并存；模型黑名单日志 60s→30min；删透传自动禁用假承诺注释（consecutive_passthrough_failures 保持观测性，新守卫 passthrough_failure_counter_must_stay_observational 钉死只清零不累加）
- token_manager.rs：透传池排序键加 ramp_tier（主路径同款公式，纯 RPM 派生）；refresh_error_retryable 结构化错误替代裸子串匹配（新类型 RefreshHttpError/RefreshNotSupportedError）；3 处 refresh_token unwrap→ok_or_else；删死字段 passthrough_overload_since/last_passthrough_429_at；排序键注释 12 位重写；排序键守卫补 ⑨⑩⑪ 三条位置断言；consecutive_passthrough_failures 注释统一「绝不自动禁用」
- rate_limiter.rs：删死函数 reset_all

## 透传/HTTP/安全
- passthrough.rs：非 2xx 错误体保留原始字节（不 lossy），诊断串另用 lossy 副本（build_error_passthrough_response）；messages_endpoint 只 ends_with("/v1")；quota 观测补 anthropic-ratelimit-* 三键
- http_client.rs：新增 pinned_streaming_client（一次 lookup → 逐 IP 过 validate_outbound_url_with + AdminConfigured 复验 → resolve_to_addrs 固化，按 (host,proxy,tls) 缓存，DNS 失败 fail-closed）；passthrough forward/fetch_upstream_models 接入，允许显式本机/内网配置（fuckopencode 127.0.0.1 是合法用例）
- token.rs：注释 4.5→4.0；.round() 替代截断；远程 count_tokens 超时 300s→10s（失败照旧 fallback 本地）；estimate_output_tokens 计 thinking 块
- common/alerting.rs：webhook client redirect::Policy::none()；client() 构建失败降级 no-op（不再 panic）
- common/security.rs：IngressRateLimiter::check 每 256 次抽样清理替代每请求全表 retain
- cache_fingerprint.rs：LRU 每 128 条超限插入才淘汰

## admin/openai/横切
- admin/service.rs：add_credential_with_intent 探测窗口保护（会探测的号以临时禁用态入池，探测成功后恢复启用，判据镜像 needs_api_region_probe）；新守卫 probe_window_keeps_credential_unselectable
- admin/handlers.rs：export_credential 补 Cache-Control: no-store
- admin/usage_handlers.rs：三处 500 响应体去内部错误细节（只留通用文案，{e} 进 tracing::warn!）
- admin/update.rs：perform_update 显式 tag 跳过 tags 拉取（check_for_updates 只在 None 分支）
- admin/external_idp_login.rs：leg1 state 校验 fail-closed（共享 callback_state_mismatch）；submit_leg2_select 响应回显实际 arn/region（ExternalIdpSelectResult 加必填字段）
- openai/convert.rs：chat/completions/responses 三处 id 恒自生成（不用上游 msg_xxx）
- kiro/mod.rs + main.rs：upstream_trace/user_id 模块接线（sync_from_config 启动期一次性读取，默认关零开销）
- kiro/diagnosis.rs：删死函数 diagnose_feature_not_supported
- model/config.rs：压缩实现注释补第 4 层
- docs/ARCHITECTURE.md：排序键 13→12 键；冷却表补 AuthTransient

## 前端 admin-ui
- lib/poll-guard.ts（新）：PollGuard 代次守卫；login-dialog.tsx pollWeb/pollIdc 关闭后自尽、重开不叠加
- i18n：login-dialog 微软 SSO 向导 ~30 处 + conn-page 自建字典删除 + login-page 按钮接线（键已存在）；散点（复制 Key 4 处、导出 2 处、dialog close、add-credential JSON 错误）；补 exportKamFailed 三语
- 死代码删除：idc-login-dialog/social-login-dialog/kam-import-dialog/batch-import-dialog 4 文件
- lib/storage.ts：getApiKey 仅当键存在才 removeItem
- credential-card.tsx：打开弹框捕获快照 ref，保存前比对（防旧值覆盖远端新值）；api/credentials.ts JSDoc 更新

## 测试修复（CI 抓出）
- update.rs：注释绕开守卫 needle 字面量（本仓经典坑）
- external_idp_login.rs：守卫 needle 分号→逗号
- token.rs：thinking 计数测试断言修正

## 已知未做（有意）
- 前端 cooldownReason 中文耦合判定（跨端改动，本期不做）
- config.example.json 未加 upstream_trace 三字段（有 serde default，行为不变）
- upstream_trace 默认关（未开）
