# KiroStudio 模块参考手册

> 按模块分组，列出每个文件的核心结构体/枚举、关键函数签名及用途、模块间调用关系。

---

## 1. 入口与配置 (main.rs + model/)

### main.rs (401行)
**结构体**
- `UsageHandles` — admin 查询侧共享的用量 sink 句柄 (stats + trace_db)

**函数**
- `main()` — 应用入口，12步初始化时序
- `setup_usage_if_enabled(config, data_dir) → Option<UsageHandles>` — 用量子系统初始化

---

### model/arg.rs (16行)
**结构体**
- `Args` — clap 命令行参数: `config: Option<String>`, `credentials: Option<String>`

---

### model/config.rs (420行)
**结构体**
- `Config` — 全局配置（40+字段: host/port/region/tls/proxy/refresh/usage 等）
- `TlsBackend` — 枚举: Rustls | NativeTls

**函数**
- `Config::load(path) → Result<Self>` — 从 JSON 文件加载
- `Config::save() → Result<()>` — 回写当前配置到原始文件
- `Config::effective_auth_region() → &str` — auth_region 回退链
- `Config::effective_api_region() → &str` — api_region 回退链

---

### http_client.rs (117行)
**结构体**
- `ProxyConfig` — 代理配置 (url + username + password)

**函数**
- `build_client(proxy, timeout_secs, tls_backend) → Result<Client>` — 统一 HTTP Client 构建器

---

### token.rs (290行)
**函数**
- `init_config(config)` — 初始化全局 count_tokens 配置 (OnceLock)
- `count_tokens(text) → u64` — 本地估算 (CJK×4.5/4 rule)
- `count_tokens_remote(messages, system, tools) → Result<u64>` — 远程 API 精确计算
- `estimate_message_content_tokens(content) → u64` — 按 JSON content 结构估算
- `estimate_output_tokens(content) → i32` — 估算输出 token

---

## 2. Kiro 核心代理 (kiro/)

### kiro/provider.rs (662行)
**结构体**
- `KiroProvider` — 核心代理器 (token_manager + client_cache + endpoints + global_proxy)
- `CallMeta` — 单次调用元数据 (credential_id, model, session_id, latency_ms, retries)

**函数**
- `call_with_retry(body) → Result<(Response, CallMeta)>` — 完整重试循环(最多9次)
- `call_once(credential_id, body) → Result<Response>` — 单次HTTP调用(无重试)
- `get_or_create_client(proxy) → Client` — 按代理配置缓存 Client
- `retry_delay(attempt) → Duration` — 指数退避 200ms×2^n + 25%抖动

**被调用**: anthropic/handlers.rs  
**调用**: token_manager, endpoint, http_client

---

### kiro/token_manager.rs (3079行) — 项目最大文件
**结构体**
- `MultiTokenManager` — 多凭据管理器 (states + cooldown + rate_limiters + affinities)
- `TokenState` — 单凭据状态 (credentials, failure_count, disabled, last_refresh)

**核心函数**
- `new(config, credentials) → Self` — 创建并初始化所有凭据(含缓存加载)
- `next_available_credential() → Result<(u64, KiroCredentials)>` — 选择最优可用凭据
- `refresh_token_for(id) → Result<()>` — 按需同步刷新(请求路径)
- `prefetch_refresh_token_for(id, lead_minutes)` — 预刷新(后台路径)
- `refresh_social_token(id)` — Cognito InitiateAuth 刷新
- `refresh_idc_token(id)` — OIDC /token 端点刷新
- `query_usage_limits(id) → Result<UsageLimitsResponse>` — 查询订阅限额
- `increment_failure_count(id)` / `reset_failure_count(id)`
- `disable_credential(id)` / `enable_credential(id)`
- `put_credential_in_cooldown(id, reason)`
- `signal_shutdown()` — 停机信号

---

### kiro/refresh_loop.rs (62行)
**函数**
- `spawn(manager, lead_minutes, interval_secs)` — 启动后台预刷新 tokio task
- `run_once(manager, lead_minutes)` — 单轮扫描+刷新

---

### kiro/affinity.rs
**结构体**
- `UserAffinityManager` — 会话亲和性 (HashMap + 30min TTL)

**函数**
- `get(user_id) → Option<u64>` — 查询绑定(惰性过期)
- `set(user_id, credential_id)` — 建立绑定
- `touch(user_id)` — 续期
- `remove_by_credential(id)` — 按凭据清理
- `cleanup()` — 定时批量清理

---

### kiro/cooldown.rs (388行)
**结构体**
- `CooldownManager` — HashMap<u64, CooldownEntry>
- `CooldownReason` — 7种枚举(RateLimitExceeded/AccountSuspended/QuotaExhausted/TokenRefreshFailed/AuthenticationFailed/ServerError/ModelUnavailable)

**函数**
- `is_in_cooldown(id) → bool`
- `set_cooldown(id, reason) → Duration`
- `set_cooldown_with_duration(id, reason, duration)`
- `remove_cooldown(id) → bool`
- `get_all_cooldowns() → Vec<(u64, CooldownInfo)>`

---

### kiro/rate_limiter.rs (484行)
**结构体**
- `RateLimiter` — 限流器
- `RateLimitConfig` — (daily_max=500, min_interval_ms=1000, jitter=0.3, backoff_base=30s)

**函数**
- `check_rate_limit(id) → RateLimitResult` — Allow/TooManyRequests/MinInterval/Backoff
- `calculate_wait_time(id) → Option<Duration>`
- `record_success(id)` / `record_failure(id, msg)`
- `calculate_backoff(failures) → Duration` — base×multiplier^n

---

### kiro/machine_id.rs (327行)
**函数**
- `generate_from_credentials(cred, config) → String` — 64字符hex
- `normalize_machine_id(str) → Option<String>` — UUID/hex 标准化
- `sha256_to_hex(data) → String`

**缓存**: `FALLBACK_MACHINE_IDS` + `DERIVED_MACHINE_IDS` (进程级)

---

### kiro/endpoint/mod.rs (254行)
**Trait**
- `KiroEndpoint` — 端点抽象接口 (api_url, mcp_url, decorate_api, transform_body, 错误检测)

**结构体**
- `RequestContext<'a>` — 单次调用上下文引用

---

### kiro/endpoint/ide.rs (169行)
**结构体**
- `IdeEndpoint` — IDE 端点实现

**URL**: `https://q.{region}.amazonaws.com/generateAssistantResponse`

---

## 3. 认证层 (kiro/auth/)

### kiro/auth/social.rs (349行)
**函数**
- `start_callback_server(tx) → (port, ServerHandle)` — 本地 OAuth 回调服务器
- `bind_available_port() → (port, TcpListener)` — 10个候选端口
- `generate_pkce_pair() → (verifier, challenge)` — S256
- `generate_social_login_url(challenge, port, state) → String`
- `exchange_code_for_token(code, verifier, redirect_uri, ...) → Result<Response>`

---

### kiro/auth/idc.rs (258行)
**函数**
- `register_client(region) → Result<OidcClient>` — RegisterClient (clientName="Kiro IDE")
- `start_device_authorization(region, client_id) → Result<DeviceAuth>` — 获取 device_code + user_code
- `poll_create_token(region, client, device_code) → PollTokenResult` — Pending/Done/Expired/Error

---

## 4. 协议解析器 (kiro/parser/)

### kiro/parser/decoder.rs (337行)
**结构体**
- `EventStreamDecoder` — 流式解码状态机 (buffer: BytesMut, state: DecoderState)
- `DecoderState` — Ready | Parsing | Recovering | Stopped

**函数**
- `feed(data) → Result<()>` — 追加数据到缓冲区
- `decode() → Result<Option<Frame>>` — 尝试解析单帧
- `decode_iter() → DecodeIter` — 迭代器，解析所有可用帧

---

### kiro/parser/frame.rs (178行)
**常量**: PRELUDE_SIZE=12, MIN_MESSAGE_SIZE=16, MAX_MESSAGE_SIZE=16MB

**函数**
- `parse_frame(buffer) → Result<Option<(Frame, consumed)>>` — 完整帧解析(双CRC校验)

---

### kiro/parser/header.rs (317行)
**结构体**
- `Headers` — HashMap<String, HeaderValue>
- `HeaderValue` — 10种类型 (Bool/Byte/Short/Int/Long/ByteArray/String/Timestamp/Uuid)
- `HeaderValueType` — repr(u8) 类型标识

**函数**
- `parse_headers(data, length) → Headers`
- `Headers::message_type()` / `event_type()` / `content_type()`

---

### kiro/parser/crc.rs
**函数**
- `crc32(data) → u32` — CRC32 ISO-HDLC 校验

---

### kiro/parser/error.rs
**枚举**
- `ParseError` — 11种错误 (Incomplete/PreludeCrcMismatch/MessageCrcMismatch/InvalidHeaderType/HeaderParseFailed/MessageTooLarge/MessageTooSmall/InvalidMessageType/PayloadDeserialize/Io/TooManyErrors/BufferOverflow)

---

## 5. 请求/事件模型 (kiro/model/)

### kiro/model/credentials.rs (873行)
**结构体**
- `KiroCredentials` — 完整凭据 (tokens + region + proxy + health + usage)
- `CredentialsConfig` — 文件格式(Single/Multiple)
- `HealthStatus` — Healthy | Degraded | Unhealthy

**函数**
- `load_credentials_from_file(path) → Vec<KiroCredentials>`
- `select_healthy_credential(pool) → Option<&KiroCredentials>`
- `is_usable()` / `is_expired()` / `mark_failure()` / `mark_success()`
- `effective_region()` / `effective_auth_region()` / `effective_api_region()`
- `effective_proxy(global) → Option<ProxyConfig>`

---

### kiro/model/requests/ (conversation.rs 374行 + kiro.rs + tool.rs 192行)
**结构体**
- `KiroRequest` — 顶层请求 (conversation_state + profile_arn)
- `ConversationState` — (conversation_id, history, current_message, agent_task_type)
- `CurrentMessage` → `UserInputMessage` → `UserInputMessageContext` (tools, tool_results)
- `Tool` / `ToolSpecification` / `InputSchema`
- `ToolResult` / `ToolUseEntry`

---

### kiro/model/events/ (base.rs 189行 + assistant/tool_use/metering/context_usage)
**枚举**
- `Event` — AssistantResponse | ToolUse | Metering | ContextUsage | Unknown | Error | Exception

**结构体**
- `AssistantResponseEvent` — {content: String}
- `ToolUseEvent` — {name, tool_use_id, input, stop}
- `MeteringEvent` — {unit, unit_plural, usage: f64}
- `ContextUsageEvent` — {context_usage_percentage}

---

### kiro/model/token_refresh.rs
**结构体**: `RefreshRequest`, `AuthParameters`, `IdcRefreshRequest`

### kiro/model/usage_limits.rs (202行)
**结构体**: `UsageLimitsResponse`, `UsageLimit`

---

## 6. Anthropic 兼容层 (anthropic/)

### anthropic/handlers.rs (1121行) — 请求入口
**函数**
- `get_models()` — 返回4个模型定义
- `count_tokens(payload)` — 本地token估算
- `post_messages(payload)` — 标准端点
- `post_messages_cc(payload)` — Claude Code 端点(BufferedStreamContext)

**被调用**: router  
**调用**: converter, stream, provider, cache_tracker, websearch

---

### anthropic/converter.rs (1798行) — 格式转换核心
**函数**
- `convert_request(req) → Result<KiroRequest>` — 完整 Anthropic→Kiro 转换
- `map_model(model) → Option<String>` — 模型名→ID映射
- `normalize_tools(tools)` — 修复 MCP 工具 Schema
- `convert_messages_to_state(messages)` — 对话状态机(平铺→分层)

---

### anthropic/stream.rs (2131行) — 流式处理核心
**结构体**
- `StreamContext` — SSE 状态管理器(标准端点)
- `BufferedStreamContext` — 缓冲版(等待 contextUsage 后再发 message_start)

**函数**
- `process_streaming_response(stream, model, ...) → impl Stream<Item=Bytes>` — 二进制流→SSE

---

### anthropic/cache_tracker.rs (824行)
**结构体**
- `CacheTracker` — 本地影子 prompt 缓存记账

**函数**
- `build_cache_profile(messages, system, tools) → CacheProfile`
- `resolve(credential_id) → CacheAccounting` — 计算缓存命中

---

### anthropic/websearch.rs (764行)
**函数**
- `handle_request(provider, messages) → Result<SearchResults>` — MCP WebSearch

---

### anthropic/types.rs (311行)
**结构体**: `MessagesRequest`, `MessageResponse`, `Message`, `ContentBlock`, `Usage`, `Tool`, `Thinking`, `SystemMessage`, `CacheControl`

---

### anthropic/middleware.rs
**函数**
- `auth_middleware(state, request, next) → Response` — constant_time_eq 验证 API Key

---

## 7. Admin 管理层 (admin/)

### admin/service.rs (855行) — 业务逻辑核心
**结构体**
- `AdminService` — (token_manager + balance_cache + social_login + idc_login)

**函数**
- `get_all_credentials() → CredentialsStatusResponse` — 全量快照(含哈希去重)
- `add_credential(req) → Result<AddCredentialResponse>` — 添加+验证
- `delete_credential(id)` / `set_disabled(id)` / `set_priority(id)`
- `force_refresh(id)` / `get_balance(id)` (5min缓存)
- `get_config_snapshot()` / `update_config(req)` — 配置CRUD

---

### admin/social_login.rs (305行)
**结构体**
- `SocialLoginManager` — 网页上号会话池 (sessions HashMap, 600s TTL)

**函数**
- `start(priority, proxy) → StartResult` — 发起OAuth(本地/远程模式)
- `poll(session_id) → PollResult` — 轮询状态(Pending/Done/Error)
- `deliver_callback(data)` — 远程模式投递回调

---

### admin/idc_login.rs (201行)
**结构体**
- `IdcLoginManager` — IDC 上号会话池 (900s TTL)

**函数**
- `start(start_url, region, priority) → IdcStartResult`
- `poll(session_id) → IdcPollResult`

---

### admin/usage_handlers.rs (129行)
**端点**: `usage_overview`, `usage_timeseries`, `usage_by_model`, `usage_by_credential`, `usage_recent`, `usage_rate`

---

## 8. 用量统计 (usage/)

### usage/pipeline.rs (165行)
**函数**
- `init(sinks: Vec<Box<dyn UsageSink>>)` — OnceLock + 启动专用OS线程worker
- `record(record: RequestRecord)` — try_send 非阻塞入队(满则丢弃+计数)

**Trait**
- `UsageSink` — `on_record(&self, record: &RequestRecord)` + `name()`

---

### usage/trace_db.rs (383行)
**结构体**
- `TraceDb` — SQLite 明细存储(rusqlite, Mutex)

**函数**
- `open(path) → Result<Self>` — 创建表+索引
- `insert(record)` — 逐条落账
- `query_recent(limit)` / `query_timeseries(from, to, bucket)` / `query_by_model()` / `query_by_credential()`
- `delete_before(ts)` — 保留期清理

---

### usage/usage_stats.rs (887行)
**结构体**
- `UsageStats` — 内存预聚合 (hourly buckets + model/credential 分组 + JSONL)

**函数**
- `on_record(record)` — 合并到内存桶 + 追加JSONL
- `overview()` / `timeseries(from, to)` / `by_model()` / `by_credential()` / `recent()` / `rate()`

---

### usage/record.rs (144行)
**结构体**
- `RequestRecord` — 数据契约(ts_ms, credential_id, model, input/output_tokens, latency_ms, outcome, session_id)
- `RequestOutcome` — Success | UpstreamError | ClientError | Timeout

---

## 9. 安全与UI (common/ + admin_ui/)

### common/security.rs (449行)
**函数**
- `build_cors_layer(origins) → CorsLayer` — CORS 白名单
- `build_ip_allowlist_layer(allowlist) → middleware` — IP 过滤
- `build_rate_limit_layer(rps, burst) → middleware` — 入口令牌桶限流
- `build_body_limit_layer(max_bytes) → DefaultBodyLimit`

---

### common/auth.rs
**函数**
- `extract_api_key(request) → Option<String>` — 从 x-api-key 或 Authorization 提取
- `constant_time_eq(a, b) → bool` — 恒定时间比较

---

### admin_ui/router.rs
**函数**
- `create_admin_ui_router() → Router` — rust-embed 静态文件服务 + SPA fallback(所有非文件路径返回index.html)
