# KiroStudio 系统架构文档

> **定位**：KiroStudio 是一个 Anthropic API 兼容的反向代理网关（22,796 行 Rust / Axum），将下游客户端的标准 Anthropic 格式请求透明地转换为 Kiro IDE 上游的 AWS event-stream 二进制协议，并将响应实时流式转换回 Anthropic SSE 格式。

---

## 一、系统总览

```
下游客户端 (Cursor / VSCode / Claude Code / 任何 Anthropic SDK)
  │  POST /v1/messages (Anthropic JSON)
  ▼
┌───────────────────────────────────────────────────────────┐
│               KiroStudio 单二进制进程                       │
├───────────────────────────────────────────────────────────┤
│ 1. 入口安全层   CORS · IP白名单 · 入口限流 · Body限制      │ ← common/security.rs
│ 2. 认证层       constant_time_eq 验证 x-api-key            │ ← anthropic/middleware.rs
│ 3. 转换层       Anthropic → Kiro 格式转换                  │ ← anthropic/converter.rs
│ 4. 调度层       亲和性 · 凭据选择 · 重试/故障转移 · 冷却    │ ← kiro/provider.rs
│ 5. Token管理    多凭据池 · Social/IDC刷新 · 预刷新循环      │ ← kiro/token_manager.rs
│ 6. 协议层       AWS event-stream 二进制帧解码               │ ← kiro/parser/*
│ 7. 回转层       Kiro events → Anthropic SSE 实时转换       │ ← anthropic/stream.rs
│ 8. 用量层       异步Pipeline → SQLite + JSONL + 内存聚合   │ ← usage/*
├───────────────────────────────────────────────────────────┤
│ Admin API (:8992)  凭据CRUD · 余额 · 上号 · 配置 · 用量查询│ ← admin/*
│ Admin UI           rust-embed 内嵌 React SPA              │ ← admin_ui/*
└───────────────────────────────────────────────────────────┘
  │  AWS event-stream 二进制流
  ▼
Kiro 上游 (q.{region}.amazonaws.com/generateAssistantResponse)
```

---

## 二、启动流程（main.rs 时序）

```
main()
 │
 ├─ 1. Args::parse()                         命令行参数（-c config, --credentials）
 ├─ 2. tracing_subscriber::fmt().init()       日志（RUST_LOG 环境变量，默认 info）
 ├─ 3. Config::load(config_path)              JSON 配置文件加载
 ├─ 4. CredentialsConfig::load(path)          凭据文件（单对象或数组格式）
 │      └─ into_sorted_credentials()          按 priority 排序
 │      └─ KIRO_API_KEY 环境变量检查          自动插入最高优先级 API Key 凭据
 ├─ 5. token::init_config()                   初始化全局 count_tokens 配置
 ├─ 6. MultiTokenManager::new(config, creds)  创建多凭据管理器
 │      └─ 为每个凭据计算 ID (SHA256)
 │      └─ 从磁盘缓存加载已有 Token
 │      └─ 初始化每凭据 RateLimiter
 ├─ 7. refresh_loop::spawn(manager)           启动预刷新后台任务（如果 enable_refresh）
 │      └─ startup_refresh 初始刷新
 │      └─ 定期扫描 + 预刷新即将过期 Token
 ├─ 8. KiroProvider::with_proxy(manager)      创建核心代理器（含端点注册表）
 ├─ 9. setup_usage_if_enabled()               用量统计初始化
 │      ├─ TraceDb::open(sqlite)              打开 SQLite
 │      ├─ UsageStats::new()                  内存预聚合器
 │      ├─ init_pipeline([trace_db, stats])   启动 OS 线程 worker
 │      └─ 定时清理任务（6h 清理过期明细）
 ├─10. CancellationToken::new()               优雅停机令牌
 ├─11. 启动 HTTP 服务器
 │      ├─ Anthropic 端点 (:port)             /v1/messages, /cc/v1/messages 等
 │      └─ Admin 端点 (:admin_port)           /api/admin/* + /admin (UI)
 └─12. shutdown_token.cancelled().await        等待 SIGTERM/Ctrl-C → 优雅停机
```

---

## 三、请求完整链路

```
客户端 POST /v1/messages
 │
 ├─ [安全层] CORS 检查 → IP 白名单 → 入口限流(令牌桶) → Body 大小限制
 ├─ [认证] auth_middleware: x-api-key / Authorization: Bearer (constant_time_eq)
 ├─ [解析] JsonExtractor<MessagesRequest> 反序列化
 ├─ [WebSearch] 检测 computer_20241022 工具 + WebSearch → 走 MCP 搜索
 ├─ [转换] converter::convert_request()
 │   ├─ map_model(): claude-sonnet-4-20250514 → Kiro ID "14"
 │   ├─ normalize_tools(): 修复 MCP 工具 JSON Schema (null → 合法值)
 │   └─ convert_messages_to_state(): 平铺消息序列 → Kiro 分层 ConversationState
 │
 ├─ [调度] provider.call_with_retry(body):
 │   ├─ 检查会话亲和性 (session_id → credential_id)
 │   └─ 循环 (最多 MAX_TOTAL_RETRIES=9 次):
 │       ├─ token_manager.next_available_credential()
 │       │   └─ 过滤: !disabled && !in_cooldown && !expired
 │       │   └─ 选择: min_by_key(priority, consecutive_failures)
 │       ├─ provider.call_once(credential_id, body):
 │       │   ├─ 过期检查 → 触发 refresh_token_for() 同步刷新
 │       │   ├─ 限流检查 → wait_for_rate_limit()
 │       │   ├─ 获取 HTTP Client (按 proxy 配置缓存)
 │       │   ├─ endpoint.decorate_api(): 构建请求头 (Authorization, UA, machineId)
 │       │   └─ client.execute(request)
 │       └─ 响应分派:
 │           ├─ 2xx → 设置亲和性, reset_failure, 返回 bytes_stream
 │           ├─ 401 → TokenExpired (触发刷新, 重试)
 │           ├─ 429 → 触发冷却, 切换凭据重试
 │           ├─ 403/500 → 不可重试, 直接返回错误
 │           └─ 网络错误 → increment_failure_count, 退避后重试
 │
 ├─ [流处理] stream.rs:
 │   ├─ EventStreamDecoder 解码 AWS event-stream 二进制帧
 │   ├─ 逐帧解析为 Event (AssistantResponse/ToolUse/Metering/ContextUsage)
 │   ├─ StreamContext 状态机: 实时转换为 Anthropic SSE
 │   │   ├─ message_start → content_block_start → content_block_delta (×N)
 │   │   → content_block_stop → message_delta → message_stop
 │   └─ 每 15 秒发送 :ping 保活
 │
 ├─ [用量] usage::emit_record(RequestRecord) — try_send 非阻塞
 │
 └─ 客户端接收 SSE 流 (text/event-stream)
```

---

## 四、凭据管理机制

### 4.1 数据结构

```rust
struct MultiTokenManager {
    states: Mutex<HashMap<u64, TokenState>>,  // parking_lot Mutex
    cooldown_manager: CooldownManager,
    rate_limiters: HashMap<u64, RateLimiter>,
    user_affinities: UserAffinityManager,
    refresh_lock: TokioMutex<()>,             // 防并发刷新
}

struct TokenState {
    credentials: KiroCredentials,
    last_refresh: Instant,
    failure_count: u32,
    disabled: bool,
}
```

### 4.2 健康状态机

```
              成功任何一次
Unhealthy ──────────────────→ Healthy
    ↑                            │
    │ 连续失败≥3                  │ 连续失败1-2
    │                            ▼
    └──────────────────── Degraded
```

- **Healthy**: 正常参与调度
- **Degraded**: 仍可用，但选择优先级降低（consecutive_failures 作为次排序键）
- **Unhealthy**: 跳过不选（等待手动重置或自动恢复）

### 4.3 凭据选择算法

```rust
pool.iter()
    .filter(|c| !c.disabled && !in_cooldown(c.id) && !c.is_expired())
    .min_by_key(|c| (c.priority, c.consecutive_failures))
```

1. 过滤不可用凭据
2. 按 `(priority, consecutive_failures)` 双键排序
3. 取最优凭据
4. 轮询计数器确保多凭据间负载均衡

### 4.4 冷却系统 (cooldown.rs)

7 种冷却原因，差异化时长：

| 原因 | 时长 | 自动恢复 |
|------|------|---------|
| RateLimitExceeded | 60s | ✓ |
| TokenRefreshFailed | 60s | ✓ |
| ServerError | 120s | ✓ |
| ModelUnavailable | 300s | ✓ |
| AuthenticationFailed | 3600s | ✗ |
| AccountSuspended | 86400s | ✗ |
| QuotaExhausted | 86400s | ✗ |

---

## 五、Token 刷新机制

### 5.1 双路径设计

| 维度 | 预刷新 (refresh_loop.rs) | 按需刷新 (call_once 路径) |
|------|--------------------------|--------------------------|
| 触发时机 | 后台定时扫描(默认60s一轮) | 请求到来时发现 Token 过期 |
| 阻塞性 | 不阻塞请求 | 阻塞当前请求直到完成 |
| 提前量 | lead_minutes 参数(默认15分钟) | 5 分钟(硬编码) |
| 失败处置 | 累积 failure_count，不影响请求 | 返回错误，触发重试其他凭据 |
| 持锁 | refresh_lock（短暂，仅在确认需要后） | refresh_lock（整个刷新过程） |

### 5.2 二次确认（防止重复刷新）

两个路径共用 `refresh_lock: TokioMutex<()>`。进入刷新前二次检查 Token 是否仍需刷新，避免：
- 预刷新刚完成 → 请求路径又刷一次
- 两个请求同时触发按需刷新

### 5.3 Social vs IDC 刷新差异

| 维度 | Social (Cognito) | IDC (OIDC) |
|------|------------------|------------|
| 端点 | `cognito-idp.{region}.amazonaws.com` | `oidc.{region}.amazonaws.com/token` |
| 请求头 | `X-Amz-Target: InitiateAuth` | `Content-Type: x-www-form-urlencoded` |
| 请求体 | JSON (authFlow + clientId + refreshToken) | URL-encoded (clientId + clientSecret + refreshToken) |
| 响应 | `idToken` + `accessToken` + `expiresIn` | `access_token` + `refresh_token` + `expires_in` |

---

## 六、协议转换层

### 6.1 Anthropic → Kiro 映射

| Anthropic | Kiro |
|-----------|------|
| messages: [{role, content}] 平铺序列 | conversationState.history[] + currentMessage 分层 |
| tool_use 在 assistant.content[] 中 | tool_uses 独立字段 |
| tool_result 在 user.content[] 中 | UserInputMessageContext.tool_results |
| model: "claude-sonnet-4-20250514" | modelId: "14" |
| stream: true | 始终流式（event-stream 响应） |

### 6.2 对话状态机 (converter.rs)

```
enum ParsingState {
    ExpectingUser,       // 期待 user 消息
    ExpectingAssistant,  // 期待 assistant 消息
    InToolUse { ... },   // 工具调用中，等待 tool_result
}
```

遍历 messages[]，根据 role + 内容类型(text/tool_use/tool_result) 在三态之间转换，最终构造 Kiro 的 `ConversationState`。

### 6.3 /v1 vs /cc/v1 差异

| 端点 | 上下文 | 行为差异 |
|------|--------|---------|
| `/v1/messages` | `StreamContext` | 立即发送 `message_start`，`input_tokens` 为估算值 |
| `/cc/v1/messages` | `BufferedStreamContext` | **缓冲** `message_start`，等待 `contextUsageEvent` 获得精确 `input_tokens` 后再发送 |

原因：Claude Code 依赖 `message_start.usage.input_tokens` 显示上下文用量进度条，需要精确值。

---

## 七、安全层 (common/security.rs)

执行顺序（Axum 中间件从外到内）：

```
请求进入
  │
  ├─ 1. CORS Layer (tower-http)
  │      允许的 origins 从 config.cors_allowed_origins 读取
  │      未配置时默认允许所有
  │
  ├─ 2. IP 白名单
  │      从 X-Forwarded-For / X-Real-IP / peer addr 提取客户端 IP
  │      config.ip_allowlist 为空时跳过检查
  │      不在白名单 → 403
  │
  ├─ 3. 入口限流 (令牌桶)
  │      config.global_rate_limit (默认 0 = 不限)
  │      超限 → 429 + Retry-After
  │
  └─ 4. Body 大小限制
         config.max_body_bytes (默认 10MB)
         DefaultBodyLimit::max()
```

---

## 八、用量统计 Pipeline

### 8.1 架构

```
请求路径 (tokio task)               专用 OS 线程
┌──────────────┐    try_send     ┌────────────────┐
│ emit_record()│ ──────────────→ │ pipeline worker│
└──────────────┘  SyncSender     │                │
                  容量 10,000     │  for sink in   │
                  满则丢弃+计数   │    sinks:      │
                                  │    catch_unwind│
                                  │      sink()    │
                                  └────────────────┘
                                    ↓           ↓
                              TraceDb       UsageStats
                              (SQLite)      (JSONL+内存)
```

### 8.2 为什么用 std::thread 而非 tokio task

sink 内部做同步阻塞 IO（SQLite execute、文件 writeln!）。若跑在 tokio worker 线程上：
- 慢盘/fsync 抖动会阻塞该 worker
- 侵蚀 tokio 线程池（默认 CPU 核数个）
- 延迟传导回请求路径

独立 OS 线程彻底隔离，请求路径只做一次非阻塞 `try_send`。

### 8.3 容错设计

- **通道满**: 丢弃记录，`AtomicU64` 计数器递增（可通过 `dropped_count()` 查询）
- **sink panic**: `catch_unwind(AssertUnwindSafe(...))` 捕获，warn 日志，不影响其他 sink 和 worker
- **单次初始化**: `OnceLock` 确保 `init()` 只能调用一次

---

## 九、关键设计决策表

| # | 决策 | 原因 |
|---|------|------|
| 1 | 用量 Pipeline 用 `std::thread` 不用 tokio task | SQLite/fsync 阻塞 IO 会侵蚀 tokio 线程池 |
| 2 | `parking_lot::Mutex` 保护凭据状态 | 极低锁开销，持有时间短（微秒级），不需要 async |
| 3 | `TokioMutex` 保护 token 刷新 | 刷新涉及网络 IO (数秒)，需要 async 友好的 Mutex |
| 4 | machineId 用 SHA256 从 refreshToken 派生 | 单向不可逆 + refreshToken 轮换时自动更新 |
| 5 | 每凭据独立 proxy 配置 | 企业凭据走内网代理，个人凭据直连 |
| 6 | 自实现 AWS event-stream 解码器 | 无现成 Rust 库支持流式零拷贝解码 |
| 7 | converter.rs 用状态机解析对话 | Anthropic 平铺 ↔ Kiro 分层需精确状态追踪 |
| 8 | 双端点 /v1 和 /cc/v1 | Claude Code 依赖精确 input_tokens，需缓冲等 contextUsageEvent |
| 9 | 预刷新 + 按需刷新双路径 | 预刷新消除请求热路径阻塞，按需刷新兜底保证可用性 |
| 10 | HTTP Client 按 proxy 配置缓存 | 避免每次请求创建新连接，支持连接池复用 |

---

## 十、文件目录结构

```
src/                                  22,796 行
├── main.rs                   (401)   程序入口：初始化 + 启动 + 优雅停机
├── model/
│   ├── mod.rs                 (3)    模块导出
│   ├── arg.rs                (15)    命令行参数 (clap)
│   └── config.rs            (420)    全局配置模型 (JSON serde)
├── http_client.rs           (117)    ProxyConfig + reqwest Client 构建
├── token.rs                 (290)    Token 计算（本地估算 + 远程 API）
├── debug.rs                 (210)    调试工具（hex dump / CRC / 事件打印）
├── test.rs                   (96)    集成测试辅助
│
├── kiro/                             上游 Kiro 协议层
│   ├── mod.rs                (11)    模块声明
│   ├── provider.rs          (662)    核心代理：重试/故障转移/Client缓存
│   ├── token_manager.rs    (3079)    ★ 最大文件：多凭据管理全部逻辑
│   ├── refresh_loop.rs       (62)    后台预刷新循环
│   ├── affinity.rs          (105)    会话亲和性 (user_id → credential_id, 30min TTL)
│   ├── cooldown.rs          (388)    7种冷却原因 + 差异化时长
│   ├── rate_limiter.rs      (484)    每日限制 + 最小间隔 + 指数退避 + 抖动
│   ├── machine_id.rs        (327)    machineId 生成/派生/缓存
│   ├── auth/
│   │   ├── mod.rs             (3)    认证模块声明
│   │   ├── social.rs       (349)    Social OAuth PKCE (本地回调服务器)
│   │   └── idc.rs          (258)    AWS SSO Device Code Flow
│   ├── endpoint/
│   │   ├── mod.rs           (254)    KiroEndpoint trait + RequestContext
│   │   └── ide.rs          (169)    IDE 端点实现 (q.{region}.amazonaws.com)
│   ├── parser/
│   │   ├── mod.rs             (6)    解析器模块声明
│   │   ├── decoder.rs      (337)    流式解码器状态机 (BytesMut 缓冲)
│   │   ├── frame.rs        (178)    完整帧解析 (prelude + headers + payload + CRC)
│   │   ├── header.rs       (317)    10种头部值类型解析
│   │   ├── crc.rs           (42)    CRC32 ISO-HDLC
│   │   └── error.rs         (89)    11种解析错误
│   └── model/
│       ├── mod.rs             (5)    模型模块声明
│       ├── credentials.rs  (873)    凭据模型 + 选择算法 + 文件加载
│       ├── token_refresh.rs  (79)    刷新请求/响应类型
│       ├── usage_limits.rs (202)    订阅用量限制查询模型
│       ├── common/
│       │   └── mod.rs        (35)    通用模型
│       ├── requests/
│       │   ├── mod.rs        (12)    请求模型导出
│       │   ├── conversation.rs (374) ConversationState + History
│       │   ├── kiro.rs       (28)    KiroRequest 顶层
│       │   └── tool.rs     (192)    Tool/ToolResult/ToolUseEntry
│       └── events/
│           ├── mod.rs        (66)    Event 枚举 + 分派
│           ├── base.rs      (189)    EventType + EventPayload trait
│           ├── assistant.rs  (23)    AssistantResponseEvent
│           ├── tool_use.rs   (37)    ToolUseEvent
│           ├── metering.rs   (29)    MeteringEvent
│           └── context_usage.rs (19) ContextUsageEvent
│
├── anthropic/                        Anthropic 兼容 API 层
│   ├── mod.rs                (12)    模块声明
│   ├── router.rs             (68)    路由配置 (/v1 + /cc/v1)
│   ├── middleware.rs         (45)    AppState + auth_middleware
│   ├── handlers.rs        (1121)    请求处理 + 非流式转换 + WebSearch
│   ├── converter.rs       (1798)    ★ Anthropic → Kiro 格式转换核心
│   ├── stream.rs          (2131)    ★ Kiro event-stream → Anthropic SSE
│   ├── types.rs            (311)    Anthropic API 类型定义
│   ├── cache_tracker.rs    (824)    影子 prompt 缓存记账
│   └── websearch.rs        (764)    MCP WebSearch 处理
│
├── admin/                            Admin 管理 API
│   ├── mod.rs                 (9)    模块声明
│   ├── router.rs             (78)    路由注册 (认证+公开)
│   ├── middleware.rs         (52)    AdminState + admin_auth
│   ├── handlers.rs          (340)    HTTP 处理器
│   ├── service.rs           (855)    业务逻辑 (凭据CRUD/余额/配置)
│   ├── types.rs             (402)    请求/响应类型
│   ├── error.rs              (67)    错误类型 + HTTP 映射
│   ├── social_login.rs      (305)    网页 OAuth 上号会话管理
│   ├── idc_login.rs         (201)    IDC Device Code 上号会话管理
│   └── usage_handlers.rs   (129)    用量查询端点
│
├── usage/                            用量统计
│   ├── mod.rs                (19)    模块导出
│   ├── pipeline.rs          (165)    异步管道 (OS线程 + SyncSender)
│   ├── record.rs            (144)    RequestRecord 数据契约
│   ├── trace_db.rs          (383)    SQLite 逐条持久化
│   └── usage_stats.rs      (887)    JSONL 落盘 + 内存预聚合
│
├── common/                           通用工具
│   ├── mod.rs                 (3)    模块声明
│   ├── auth.rs               (56)    API Key 提取 + constant_time_eq
│   └── security.rs          (449)    CORS/IP白名单/入口限流/Body限制
│
└── admin_ui/                         管理界面
    ├── mod.rs                (10)    rust-embed Assets 声明
    └── router.rs             (45)    静态文件服务 + SPA fallback
```

---

## 附录：端口分配

| 端口 | 用途 | 默认值 |
|------|------|--------|
| `port` | Anthropic 兼容 API (下游客户端连接) | 8991 |
| `admin_port` | Admin API + Admin UI | 8992 |
