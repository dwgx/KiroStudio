# KiroStudio 模块参考手册

> 按模块分组，列出每个文件的核心结构体/枚举、关键函数签名及用途、模块间调用关系。
> 行数为当前实测（约 35,800 行 Rust）。用 codegraph 索引 + 源码逐一核对。
> 架构总览与请求链路见 `docs/ARCHITECTURE.md`；变更史见 `CHANGELOG.md`。

---

## 1. 入口与配置

### main.rs (481)
**结构体**：`UsageHandles` — admin 查询侧共享的用量 sink 句柄（stats + trace_db）
**函数**
- `main()` — 应用入口，19 步初始化时序（见 ARCHITECTURE §二）
- `init_usage_pipeline(config) → Option<UsageHandles>` — 装配 TraceDb + UsageStats 双 sink，冷启动重放
- `shutdown_signal()` — 等待 Ctrl-C（全平台）/ SIGTERM（Unix），触发优雅停机 drain

### model/config.rs (674)
**结构体**：`Config`（40+ 字段：host/port/region/tls/proxy/refresh/usage/cooldown/ratelimit/
affinity/rpm/compression/login_background/collect_fingerprint/trust_forwarded 等）· `TlsBackend`(Rustls|NativeTls)
· `CompressionConfig`
**函数**
- `Config::load(path) → Result<Self>` / `Config::save()` — JSON 加载/回写
- `effective_auth_region()` / `effective_api_region()` — region 回退链
- 默认值：`default_port()=8080` · `default_region()="us-east-1"` · `default_endpoint()="ide"`
  · `default_max_body_bytes()=256MiB`

### model/arg.rs (14)
`Args`（clap）：`config: Option<String>`, `credentials: Option<String>`

### http_client.rs (220)
`ProxyConfig`（url + username + password，支持 `socks5://user:pass@host` 内嵌账密自动拆分）；
`build_client(proxy, timeout, tls_backend) → Client` 统一构建器。

### token.rs (250)
本地估算（CJK×4.5/4）+ 远程精确计算：`count_tokens` / `count_tokens_remote` /
`estimate_message_content_tokens` / `estimate_output_tokens`。

---

## 2. Kiro 核心：调度与 Token 管理 (kiro/)

### kiro/token_manager.rs (5239) — 项目最大文件
> ⚠️ 已知技术债（God Object），dwgx 拍板**不拆**。
**结构体**：`MultiTokenManager`（entries + cooldown + rate_limiters + rpm + health + affinity +
ArcSwap 镜像 + 受管刷新任务槽）· `CredentialEntry`（credentials/inflight/success_count/disabled）
· `CallContext`（选中凭据 + InflightGuard，随响应存活）
**核心函数**
- `acquire_context(model, user_id) → Result<CallContext>` — **选号入口**（旧名 next_available_credential 已废）
- `select_next_credential(...)` — balanced 8 键排序 / priority 模式（见 ARCHITECTURE §四）
- `is_entry_selectable(entry, is_opus)` — 硬门：!disabled && !cooldown && !rate_limited && opus订阅
  （inflight 不做硬门）
- `commit_selection(entry)` — 同锁内 inflight+1 + rpm.record（原子选号防惊群）
- `report_success` / `report_failure` / `report_rate_limited_with_retry_after` /
  `report_suspicious_activity` / `report_auth_cooldown` / `report_quota_exhausted` /
  `report_account_suspended` / `report_refresh_failure` / `report_refresh_token_invalid` — 结果派发闭环
- `respawn_refresh_task()` — TIER2 受管预刷新任务（abort+respawn）
- `force_refresh_token_for(id)` — 主动刷新；`add_credential` / `delete`(软删) / `restore` /
  `purge_credential` / `purge_trash_batch` / `purge_expired_trash(days)` — 凭据 CRUD + 回收站
- `set_load_balancing_mode(mode)` — balanced/priority 切换（回写配置）
- `query_usage_limits(id)` — 订阅限额；`cleanup_affinity` / `cleanup_scheduling` — 定时回收

### kiro/provider.rs (911)
**结构体**：`KiroProvider`（token_manager + client_cache + endpoints + global_proxy + tls_backend）
· `CallMeta`（credential_id/model/session_id/is_streaming/retries/latency_ms + InflightGuard RAII）
**函数**
- `with_proxy(...)` — 构造（端点注册表 + 代理）
- `call_api` / `call_api_stream` — 非流式/流式调用（含完整重试循环 + 故障转移）
- `call_mcp(body)` — MCP 端点调用（WebSearch 用）
- `compute_max_retries(total, available)` — 动态重试预算（小池降为 1，下限=可用数，硬上限 64）
- 常量：`MAX_RETRIES_PER_CREDENTIAL=3` · `SMALL_POOL_THRESHOLD=3` · `ABSOLUTE_MAX_TOTAL_RETRIES=64`
  · `MAX_REQUEST_RETRY_BUDGET_SECS=45`（防雪崩墙钟）
**被调用**：anthropic/handlers.rs　**调用**：token_manager, endpoint, http_client

### kiro/health.rs (418)
**结构体**：`HealthTracker`（`Mutex<HashMap<family_key, HealthState>>`）· `Circuit`(Closed/Open/HalfOpen)
· `HealthState`（ewma_success/ewma_429/circuit/consecutive_429/admit_prob_seed/open_count…）· `HealthSnapshot`
**函数**
- `on_success(key)` / `on_429(key)` — EWMA 更新 + 熔断跳闸/恢复
- `report_family_suspicious(fam, backoff)` — 族级连坐退避
- `p_avail(key, rpm, inflight, rpm_limit) → f64` — balanced 选号权重（熔断门×健康×(1-RPM压)×(1-负载)）
- `tick_circuit` — 惰性推进 Open→HalfOpen（无定时器）；`snapshot` / `cleanup`
- 常量：TRIP_THRESHOLD=3 · RECOVERY_FULL=5 · BASE_OPEN_SECS=8 · MAX_OPEN_SECS=1800 · OPEN_GROWTH=1.6

### kiro/cooldown.rs (592)
**结构体**：`CooldownManager`（HashMap<u64, CooldownEntry>）· `CooldownReason`（8 种）
**函数**：`is_available(id)` / `set_cooldown(id, reason)` / `set_cooldown_with_duration` /
`remove_cooldown(id)` / `get_all_cooldowns()`
**冷却时长**（当前值）：RateLimitExceeded=15s · SuspiciousActivity=20s · ServerError=30s ·
TokenRefreshFailed=60s · ModelUnavailable=300s · AuthenticationFailed=3600s ·
AccountSuspended/QuotaExhausted=86400s（后三者不自动恢复）

### kiro/rate_limiter.rs (523)
**结构体**：`RateLimiter` · `RateLimitConfig`（daily_max/min_interval_ms/jitter/backoff_base）
· `FailureKind`（Transient 秒级退避 / Suspended 长冻，结构化枚举替旧字符串子串匹配，杜绝误冻）
**函数**：`check_rate_limit(id) → RateLimitResult` · `record_success/record_failure(id, kind)` ·
`calculate_backoff(failures)` · `calculate_wait_time(id)`

### kiro/scheduling.rs (190)
**结构体**：`InflightGuard`（RAII，直接持计数器 Arc，Drop 时 -1，与凭据生命周期解耦，**刻意不实现 Clone**）
· `RpmTracker`（60s 固定滚动窗，`record`/`count`/`prune`/`cleanup`）

### kiro/overage.rs (233)
超额真开关（移植 Foxfishc MIT，简化为同步）：调 web_portal 开/关单号按量付费。幂等 + 审计日志 +
仅响应显式单号请求 + 默认不动任何号（计费红线）。

### kiro/web_portal.rs (407)
app.kiro.dev Web Portal 客户端（**rpc-v2-cbor**，非 JSON）：`get_user_usage_and_limits`(只读) /
`update_billing_preferences`(写，需 CSRF) / `fetch_csrf_session`。承载 overage 真开关最小接口。

### kiro/machine_id.rs (336)
`generate_from_credentials(cred, config) → 64hex` · `normalize_machine_id` · `sha256_to_hex`；
进程级缓存 `FALLBACK_MACHINE_IDS` + `DERIVED_MACHINE_IDS`；入池撞车自动轮换（防关联）。

### kiro/affinity.rs (94)
`UserAffinityManager`（HashMap + TTL）：`get`(惰性过期) / `set` / `touch`(续期) /
`remove_by_credential` / `cleanup`。

### kiro/refresh_loop.rs (70)
受管后台预刷新：`spawn(manager, lead_minutes, interval_secs)` / `run_once`（由 respawn_refresh_task 挂载）。

---

## 3. 认证与上号 (kiro/auth/)

### kiro/auth/social.rs (349)
Social OAuth PKCE（本地回调服务器）：`start_callback_server` / `bind_available_port`(10 候选端口) /
`generate_pkce_pair`(S256) / `generate_social_login_url` / `exchange_code_for_token`。

### kiro/auth/idc.rs (258)
AWS SSO-OIDC 设备码：`register_client(region)` / `start_device_authorization` /
`poll_create_token`（Pending/Done/Expired/Error）。region 过白名单（M1）。

---

## 4. 上游端点 (kiro/endpoint/)

### kiro/endpoint/mod.rs (355)
`KiroEndpoint` trait（api_url/mcp_url/decorate_api/decorate_mcp/transform_api_body/错误检测）
· `RequestContext<'a>`（单次调用上下文引用）

### kiro/endpoint/ide.rs (181)
`IdeEndpoint`（`IDE_ENDPOINT_NAME="ide"`）。
- API：`https://runtime.{region}.kiro.dev/generateAssistantResponse`
- MCP：`https://runtime.{region}.kiro.dev/mcp`（旧 `q.{region}.amazonaws.com` 已停用）
- 请求头：Authorization Bearer + tokentype(API_KEY/EXTERNAL_IDP) + x-amzn-codewhisperer-optout +
  x-amzn-kiro-agent-mode + UA/host/amz-sdk-*
- `transform_api_body` 注入 `effective_profile_arn`（external_idp 用动态解析真实租户 ARN，缺则上游 400）

---

## 5. 协议解析器 (kiro/parser/) — AWS event-stream 二进制解码

### decoder.rs (461)
`EventStreamDecoder`（BytesMut 缓冲 + DecoderState: Ready/Parsing/Recovering/Stopped）：
`feed(data)` / `decode() → Option<Frame>` / `decode_iter()`。

### frame.rs (178)
常量 PRELUDE_SIZE=12 / MIN=16 / MAX=16MB；`parse_frame(buffer) → Option<(Frame, consumed)>`（双 CRC 校验）。

### header.rs (317)
`Headers`(HashMap) · `HeaderValue`(10 类型) · `HeaderValueType`(repr u8)；`parse_headers` +
`message_type()`/`event_type()`/`content_type()`。

### crc.rs (37) · error.rs (94)
`crc32(data)` ISO-HDLC；`ParseError`（11 种：Incomplete/CrcMismatch/TooLarge/… /TooManyErrors）。

---

## 6. 请求/事件模型 (kiro/model/)

### credentials.rs (1435)
`KiroCredentials`（tokens + region + proxy + health + usage + rpm_limit + profileArn）·
`CredentialsConfig`(Single/Multiple) · `HealthStatus`(Healthy/Degraded/Unhealthy)。
函数：`load_credentials_from_file` · `is_usable`/`is_expired` · `family_key(id)`（M365 `m365:{tenant}` /
`aws:{account}`，其余 `cred:{id}`）· `effective_region/auth_region/api_region`（过白名单）·
`effective_proxy(global)` · `effective_profile_arn()` · `is_api_key_credential`/`is_external_idp_credential`/
`supports_opus()`。Debug 已脱敏（refreshToken/clientSecret/kiroApiKey）。

### requests/ (conversation.rs 374 · tool.rs 192 · kiro.rs 68)
`KiroRequest`（conversation_state + profile_arn）· `ConversationState`（id + history + current_message +
agent_task_type）· `UserInputMessage`/`UserInputMessageContext`（tools + tool_results）·
`Tool`/`ToolSpecification`/`InputSchema` · `ToolResult`/`ToolUseEntry`。

### events/ (base.rs 189 · assistant/tool_use/metering/context_usage)
`Event`（AssistantResponse/ToolUse/Metering/ContextUsage/Unknown/Error/Exception）·
`AssistantResponseEvent{content}` · `ToolUseEvent{name,tool_use_id,input,stop}` ·
`MeteringEvent{unit,usage:f64}` · `ContextUsageEvent{context_usage_percentage}`。

### token_refresh.rs (83) · usage_limits.rs (365)
`RefreshRequest`/`AuthParameters`/`IdcRefreshRequest`；`UsageLimitsResponse`/`UsageLimit`。

---

## 7. Anthropic 兼容层 (anthropic/)

### handlers.rs (1578) — 请求入口
- `get_models()` — 当前硬编码模型（Opus 4.7/4.8 + -thinking 变体等）
- `count_tokens(payload)` — token 估算
- `post_messages(payload)` — `/v1` 标准端点 → `handle_stream_request` / `handle_non_stream_request`
- `post_messages_cc(payload)` — `/cc/v1` 端点 → `handle_stream_request_buffered`（BufferedStreamContext）
- TIER3 热开关 setter：`set_extract_thinking` / `set_compression`
**被调用**：router　**调用**：converter, compressor, stream, provider, websearch

### converter.rs (2947) — 格式转换核心
- `convert_request(req) → Result<KiroRequest>` — Anthropic→Kiro 完整转换
- `map_model(model) → Option<String>` — contains 匹配：sonnet→4.5 / opus 4.x / haiku→4.5（无版本号回退）
- `normalize_tools(tools)` — 修复 MCP 工具 Schema
- `convert_messages_to_state(messages)` — 三态状态机（平铺→分层）
- `set_strip_env_noise(bool)` — TIER3 环境噪音剥离开关（进程级镜像）

### stream.rs (2599) — 流式处理核心
`StreamContext`（标准端点 SSE 状态机）· `BufferedStreamContext`（缓冲 message_start 等 contextUsage）
· `CompletionStatus` · `SseEvent`。二进制 event-stream → Anthropic SSE 实时转换 + :ping 保活 +
截断即成功处理 + resolved_usage()（与 CallMeta 合并出完整用量记录）。

### compressor.rs (584)
`ConversationState` 输入压缩（移植 Foxfishc MIT，分层增量吸收）：当前实现空白折叠（近无损）+
tool_result 智能截断（保留头 N 尾 M 行）。规避 Kiro 上游 ~5MiB 请求体 400。后续层（thinking/history
截断）风险高暂缓。

### websearch.rs (981)
MCP WebSearch：`handle_request(provider, messages)`。web_search 补 tool_use_id（v0.4.0 审计修复）。

### types.rs (311) · router.rs (96) · middleware.rs (64)
Anthropic API 类型（MessagesRequest/MessageResponse/ContentBlock/Usage/Tool/Thinking/CacheControl）；
`create_router_with_provider(...)`（/v1 + /cc/v1 + CORS + Body限制 + TIER3 播种）；
`AppState` + `auth_middleware`（constant_time_eq，空 key fail-closed）。

---

## 8. Admin 管理层 (admin/)

### service.rs (1990) — 业务逻辑核心
`AdminService`（token_manager + balance_cache + social/idc/external_idp login managers + 受管余额任务）
- `get_all_credentials()` — 全量快照（含哈希去重、健康、余额缓存）
- `add_credential` / `delete_credential`(软删) / `restore` / 回收站 · `set_credential_*`(disabled/priority/
  rpm-limit/name/proxy) / `reset_failure_count`
- `get_balance(id)`(缓存) / `get_cached_balances()` / `refresh_all_balances_gently(spacing)` /
  `respawn_balance_task()`（TIER2 受管余额刷新）
- `overage_status/enable_overage/disable_overage`(经 kiro::overage，计费红线) ·
  `deep_verify_credential`(真实 API 调用探活 suspended) · `export_credential`
- `get_config_snapshot()` / `update_config()`（含 TIER1/2/3 热重载派发）
- `restart_service()`（exit(0) 交 systemd 拉起）· `storage_stats/storage_cleanup`

### handlers.rs (706) · types.rs (697) · error.rs (64)
HTTP 处理器（薄封装转 service）；请求/响应类型；`AdminServiceError` → HTTP 映射。

### router.rs (125) · middleware.rs (64)
`create_admin_router(state)`（鉴权路由 + 公开 OAuth 回调 merge，端点清单见 ARCHITECTURE §十一）；
`AdminState` + `admin_auth_middleware`（x-api-key / Bearer）。

### social_login.rs (342) · idc_login.rs (221) · external_idp_login.rs (704)
`SocialLoginManager`（会话池 TTL，本地/远程模式，`start`/`poll`/`deliver_callback`）；
`IdcLoginManager`（设备码会话池）；`ExternalIdpLoginManager`（M365/Azure 双段 PKCE，
`start`/`leg1`/`leg2` URL 粘回引导，动态 ListAvailableProfiles 解析 profileArn）。

### update.rs (405)
GitHub 版本检查 + 二进制 OTA：`check_for_updates`（读本地版本 + 拉 tags + semver 比较，多镜像回退）/
`perform_update`（下载 → **sha256 校验**（github 直连取校验文件，切同源投毒 RCE）→ `.new` 原子 rename
覆盖 exe → 备份 `.bak` → 交 systemd 重启）/ `update_status`（读 .health/.bak/.failed 标记）。

### usage_handlers.rs (302)
只读查询端点：`usage_overview/timeseries/by_model/by_credential/recent/rate/clients/machines/throughput`
+ `ratelimit_insights`（每号限流健康快照，零上游）+ `stream_live`（SSE 每 ~1.5s 推轻量快照）。

---

## 9. 用量统计 (usage/)

### pipeline.rs (165)
`init(sinks)` — OnceLock + 专用 OS 线程 worker；`record(record)` — try_send 非阻塞（满则丢弃 + 计数）。
Trait `UsageSink`（`on_record` + `name`）。为何 OS 线程见 ARCHITECTURE §八。

### record.rs (604)
`RequestRecord` 数据契约（ts_ms/credential_id/model/input_output_tokens/latency_ms/outcome/session_id/
client 设备指纹）· `RequestOutcome`(Success/UpstreamError/ClientError/Timeout)。

### trace_db.rs (514)
`TraceDb`（SQLite/rusqlite/Mutex）：`open`(建表+索引+legacy 迁移) / `insert` /
`query_recent/timeseries/by_model/by_credential` / `retention_cleanup(days)`。

### usage_stats.rs (1812)
`UsageStats`（hourly 桶 + model/credential/session/client 分组 + JSONL）：`on_record` /
`overview/timeseries/by_model/by_credential/recent/rate/clients/machines/throughput` /
`rebuild_from_logs`(冷启动重放) / `cleanup_client_stats`(定时回收防无界增长)。含设备/客户端识别
（IP 为主键 + session 漫游合并 + 品牌识别）。

---

## 10. 安全与 UI (common/ + admin_ui/)

### common/security.rs (454)
`SecurityState`（allowlist + rate_limiter + trust_forwarded）· `IpAllowlist`(CIDR) · `RateLimiter`(每-IP 令牌桶)。
`client_ip(req, peer, trust_forwarded)` — trust 时取 **X-Forwarded-For 最右段**（H2 修复：最左可伪造）；
`security_middleware` · `build_cors_layer` · `validate_cidr`。三者全未配则不挂载中间件（零开销）。

### common/ssrf.rs (307)
出站 URL SSRF 防线（背景图代理用）：scheme 校验 → 解析所有候选 IP 拒绝私有/环回/链路本地/云元数据
（含 IPv4-mapped IPv6）→ `resolve_to_addrs` 固定 IP 防 DNS rebinding → 禁用重定向防 302 跳内网。

### common/health_marker.rs (189)
OTA 启动健康标记 / crashloop 回滚兜底：`clear_boot_attempts()`（bind 成功即清计数，越过 crashloop 门）·
`spawn_health_confirm(version)`（稳定 30s 后写 .health + 删 .bak）。配合 systemd ExecStartPre 守卫脚本。

### common/auth.rs (41)
`extract_api_key(req)`（x-api-key / Authorization）· `constant_time_eq(a, b)`（恒定时间比较）。

### admin_ui/router.rs (544) · mod.rs (10)
`create_admin_ui_router()` — rust-embed 内嵌 React SPA + SPA fallback（非文件路径返回 index.html）；
登录页背景图代理（`bg-img`，经 ssrf 防护）+ 内存池预取（`spawn_bg_prefetch` / `set_login_background_r18`
/ `set_login_background_enabled`）。

---

## 附录：调用关系速览

```
router → middleware(auth) → handlers → converter → compressor → provider.call_api(_stream)
                                                                   │
                              token_manager.acquire_context ───────┤ (选号: affinity/balanced/priority)
                                ├─ health.p_avail / on_success / on_429      (熔断权重)
                                ├─ cooldown.is_available / set_cooldown       (硬退场)
                                ├─ rate_limiter.check_rate_limit              (限流)
                                └─ scheduling: InflightGuard / RpmTracker     (在途/RPM)
                                                                   │
                              endpoint.decorate_api/transform_api_body → reqwest → Kiro (kiro.dev)
                                                                   │
                              stream.rs: parser 解码 → SSE 回转 → usage.record
```



