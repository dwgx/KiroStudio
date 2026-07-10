# KiroStudio 系统架构文档

> **定位**：KiroStudio 是一个 Anthropic API 兼容的反向代理网关（约 35,800 行 Rust / Axum 0.8 / Tokio），
> 把下游客户端的标准 Anthropic 格式请求透明转换为 Kiro 上游的 AWS event-stream 二进制协议，
> 并把响应实时流式转换回 Anthropic SSE。聚合多个 Kiro 凭据做多号调度、限流熔断、防关联、上号管理。
> 上游已随 Kiro 迁移到 `runtime.{region}.kiro.dev`（旧的 `q.{region}.amazonaws.com` 已停用）。
> 当前 **v0.4.0**。本文档按当前代码校准（用 codegraph 索引 + 源码逐一取证）。

---

## 一、系统总览

```
下游客户端 (Claude Code / Cursor / 任何 Anthropic SDK)
  │  POST /v1/messages 或 /cc/v1/messages (Anthropic JSON)
  ▼
┌────────────────────────────────────────────────────────────────┐
│               KiroStudio 单静态二进制进程（单端口）             │
├────────────────────────────────────────────────────────────────┤
│ 0. 入口安全层   IP白名单 · 每-IP限流 · (XFF最右段) 可选中间件   │ ← common/security.rs
│ 1. CORS + Body限制   CorsLayer + DefaultBodyLimit(默认256MiB)   │ ← anthropic/router.rs
│ 2. 认证层       constant_time_eq 验证 x-api-key / Bearer        │ ← anthropic/middleware.rs
│ 3. 转换层       Anthropic → Kiro 格式转换（状态机）             │ ← anthropic/converter.rs
│ 4. 压缩层       ConversationState 多层压缩（规避上游~5MiB上限） │ ← anthropic/compressor.rs
│ 5. 调度层       亲和 · balanced 8键选号 · 重试/故障转移         │ ← kiro/provider.rs + token_manager.rs
│ 6. 健康/限流    熔断器(AIMD) · 冷却 · per-cred RPM · 族级连坐   │ ← kiro/{health,cooldown,rate_limiter,scheduling}.rs
│ 7. Token管理    多凭据池 · Social/IDC/ExternalIdP 刷新 · 预刷新 │ ← kiro/token_manager.rs
│ 8. 协议层       AWS event-stream 二进制帧解码（双CRC）          │ ← kiro/parser/*
│ 9. 回转层       Kiro events → Anthropic SSE 实时转换            │ ← anthropic/stream.rs
│ 10.用量层       异步Pipeline(专用OS线程) → SQLite + JSONL + 内存│ ← usage/*
├────────────────────────────────────────────────────────────────┤
│ /api/admin/*  凭据CRUD · 余额 · 三种上号 · 配置 · 用量 · OTA    │ ← admin/*   (nest 进主端口)
│ /admin        rust-embed 内嵌 React SPA                         │ ← admin_ui/*(nest 进主端口)
└────────────────────────────────────────────────────────────────┘
  │  HTTPS + AWS event-stream 二进制流（每凭据可走独立代理）
  ▼
Kiro 上游  对话 runtime.{region}.kiro.dev/generateAssistantResponse
           余额 app.kiro.dev（Web Portal, rpc-v2-cbor）
```

> **端口**：单端口（默认 `port=8080`，`host=0.0.0.0`）。Admin API 与 Admin UI 通过
> `.nest("/api/admin", ...)` / `.nest("/admin", ...)` 挂在同一端口，**不再是独立的 :8992**。
> 仅当 `admin_api_key` 非空时才挂载 Admin。

---

## 二、启动流程（main.rs 时序）

```
main()
 ├─ 1. Args::parse()                              -c config / --credentials
 ├─ 2. tracing_subscriber 初始化                  RUST_LOG，默认 info
 ├─ 3. Config::load(path)                         JSON 配置（ArcSwap 承载，支持热重载）
 ├─ 4. CredentialsConfig::load(path)              单对象或数组格式 → into_sorted_credentials()
 │      └─ KIRO_API_KEY 环境变量 → 自动插入最高优先级 API Key 凭据
 ├─ 5. api_key 空值校验                            空白 apiKey 拒绝启动（防 fail-open 匿名消耗）
 ├─ 6. 构建端点注册表                              目前仅 IdeEndpoint（name="ide"）
 ├─ 7. MultiTokenManager::new(config, creds, ...)  多凭据管理器（含缓存加载 + 每号 RateLimiter）
 ├─ 8. token_manager.respawn_refresh_task()        TIER2：受管的后台预刷新任务（改配置 abort+respawn）
 ├─ 9. spawn 亲和清理(5min) + 回收站清理(6h)        惰性 TTL 之外的主动回收
 ├─10. set_login_background_r18 + spawn_bg_prefetch 登录页背景图预取（R18 开关，见 PROJECT-SELF）
 ├─11. set_collect_client_fingerprint              指纹采集开关播种进热路径镜像
 ├─12. KiroProvider::with_proxy(...)               核心代理器（端点注册表 + client 缓存）
 ├─13. init_usage_pipeline (if usage_enabled)      TraceDb(SQLite) + UsageStats(JSONL+内存) 双 sink
 ├─14. token::init_config()                        count_tokens 远程/本地估算配置
 ├─15. create_router_with_provider(...)            /v1 + /cc/v1（TIER3 热开关播种）
 ├─16. (if admin_key 非空) nest /api/admin + /admin AdminService + respawn_balance_task()
 ├─17. SecurityState::from_config(...)             IP白名单/限流/XFF 三者都未配则零开销不挂中间件
 ├─18. TcpListener::bind → clear_boot_attempts()   bind 成功即清 OTA 启动计数（越过 crashloop 门）
 │      spawn_health_confirm()                     稳定 30s 后写 .health + 删 .bak 回滚点
 └─19. axum::serve(...).with_graceful_shutdown()   SIGTERM/Ctrl-C → drain 在途请求（含 SSE）
```

---

## 三、请求完整链路（/v1/messages）

```
客户端 POST /v1/messages
 ├─ [安全层] (可选) IP 白名单 → 每-IP 令牌桶限流   client_ip 取 XFF 最右段(trust_forwarded 时)
 ├─ [CORS + Body] CorsLayer → DefaultBodyLimit(默认 256MiB，可设 0 完全放开)
 ├─ [认证] auth_middleware: x-api-key / Authorization: Bearer (constant_time_eq)
 ├─ [解析] JsonExtractor<MessagesRequest>（buffered，一次性入内存）
 ├─ [WebSearch] 检测 web_search / MCP 搜索工具 → 走 websearch 分支
 ├─ [转换] converter::convert_request()
 │   ├─ map_model(): "sonnet"→claude-sonnet-4.5 / "opus 4.x"→claude-opus-4.x / "haiku"→4.5（contains 匹配 + 回退）
 │   ├─ normalize_tools(): 修复 MCP 工具 JSON Schema
 │   ├─ convert_messages_to_state(): 平铺消息 → Kiro 分层 ConversationState（三态状态机）
 │   └─ (strip_env_noise) 剥离 <env>/gitStatus 等每请求漂移噪音（省 token + 提缓存命中 + 降关联）
 ├─ [压缩] compressor: 空白折叠 + tool_result 头尾截断（规避上游 ~5MiB 400）
 ├─ [调度] provider.call_once_streaming / 重试循环:
 │   ├─ token_manager.acquire_context(model, user_id) —— 选号 + 占在途名额 + 确保 token
 │   │   ├─ 会话亲和：session_id 已绑定且未 RPM 饱和 → 复用；饱和则临时解绑走 balanced
 │   │   ├─ is_entry_selectable 硬门：!disabled && !cooldown && !rate_limited && opus订阅
 │   │   │   （⚠️ inflight 不做硬门槛，只进排序键，避免假性排队）
 │   │   ├─ balanced 8 键升序选号（见 §四）；priority 模式则取 priority 最小
 │   │   └─ commit_selection：同一 entries 锁内 inflight+1 + rpm.record（根治惊群）
 │   ├─ endpoint.decorate_api：Authorization Bearer + tokentype(API_KEY/EXTERNAL_IDP) + UA/host/machineId
 │   ├─ transform_api_body：注入 effective_profile_arn（external_idp 用动态解析的真实租户 ARN）
 │   └─ 响应分派：
 │       ├─ 2xx → 建立亲和 + health.on_success + reset_failure → 返回 bytes_stream
 │       ├─ 401 → token 过期，刷新后重试
 │       ├─ 429 → health.on_429 + 冷却(RateLimitExceeded 15s) + 换号；识别可疑活动风控→族级退避
 │       ├─ 截断即成功 → 上游流被截断但已产出内容，视为成功透传（不重试放大）
 │       └─ 5xx/网络错误 → increment_failure + 退避重试；墙钟预算 45s 到即透传最后错误止损
 ├─ [流处理] stream.rs:
 │   ├─ EventStreamDecoder 解码 AWS event-stream 二进制帧（prelude + headers + payload + 双 CRC32）
 │   ├─ 逐帧 → Event(AssistantResponse/ToolUse/Metering/ContextUsage/Error/Exception)
 │   ├─ StreamContext 状态机 → Anthropic SSE:
 │   │   message_start → content_block_start → delta×N → block_stop → message_delta → message_stop
 │   └─ 定时 :ping 保活
 ├─ [用量] usage::record(RequestRecord) — try_send 非阻塞入队（满则丢弃 + 计数）
 └─ 客户端接收 SSE 流 (text/event-stream)
```

### 3.1 /v1 与 /cc/v1 的差异

| 端点 | 上下文 | 行为差异 |
|------|--------|---------|
| `/v1/messages` | `StreamContext` | 立即发 `message_start`，`input_tokens` 为估算值 |
| `/cc/v1/messages` | `BufferedStreamContext` | **缓冲** `message_start`，等 `contextUsageEvent` 拿到精确 `input_tokens` 再发 |

原因：Claude Code 依赖 `message_start.usage.input_tokens` 显示上下文进度条，需精确值。

---

## 四、调度：balanced 选号算法（token_manager::select_next_credential）

历史文档写的 `(priority, consecutive_failures)` 双键**已过时**。当前 balanced 模式是 **8 键升序** `min_by_key`，
在同一把 `entries` 锁临界区内完成"读候选(含 inflight/rpm) → 选中 → inflight+1 → rpm.record"，
保证并发选号原子性（根治惊群/Top5 热点）：

```
排序键（升序，越小越先选）：
 ① unusable        —— 真不可用(熔断 Open→p_avail=0 或 RPM 已饱和)沉底，优雅溢出到下一层
 ② prio_key        —— priorityInBalanced 开关开：按 priority 分层；关：恒 0（纯健康均衡）
 ③ neg_p_bucket    —— ⭐核心：-(p_avail×100)，p_avail 高排前（健康分档首要键）
 ④ saturated       —— RPM 软/硬上限饱和兜底
 ⑤ inflight        —— 当前在飞请求数（少者优先，分摊并发）
 ⑥ rpm(近60s)      —— 滚动窗口 RPM
 ⑦ success_count   —— 终身成功数
 ⑧ priority        —— 末位兜底（开关关时唯一 priority 参与点）
```

- `p_avail ∈ [0,1] = 熔断门 × 健康分 × (1 - RPM压力) × (1 - 负载)`（health.rs::p_avail）。
- **per-cred RPM 容量**：号有自己的 `rpm_limit`（>0）则用它，否则回退全局 `rpm_limit`。
- **priority 模式**（默认 load_balancing_mode）：直接 `min_by_key(priority)`，固定主号。

### 4.1 健康熔断器（health.rs）

族/号同表同算法，键 = `family_key`：M365 同租户 → `m365:{tenant}` / `aws:{account}`（**整族连坐**，
一个号被账户级风控整族一起退避）；IdC/social/api_key → `cred:{id}`（各自独立，坚强兜底不受连坐）。

```
熔断状态机（AIMD，惰性推进无定时器）：
  Closed ──连续429≥3──→ Open{until}
    ▲                      │ 墙钟到期(tick_circuit)
    │ 连续成功≥5           ▼
    └───────── HalfOpen{admit_prob} ──失败──→ Open（admit_prob_seed *=0.5 收缩）
                （每次成功 admit_prob += 0.2）
```
- `ewma_success`(α=0.3 慢升) / `ewma_429`(α=0.5 快升敏感)；`health = ewma_success×(1-0.6×ewma_429)`。
- Open 退避：`base 8s × 1.6^open_count`，上限 30min（对齐 SuspiciousActivity）。
- 与 CooldownManager 分工：Cooldown = 硬退场布尔门（is_available，硬跳过）；Health = 到期后
  的软放回 + 连续权重（half-open 概率放行只进 balanced 排序，不进 is_entry_selectable 硬门）。

### 4.2 冷却系统（cooldown.rs）——差异化时长（当前值）

| 原因 | 时长 | 自动恢复 | 说明 |
|------|------|---------|------|
| RateLimitExceeded | 15s | ✓ | 上游 429 + 普通 throttle |
| SuspiciousActivity | 20s | ✓ | 可疑活动软风控（族级连坐地基） |
| ServerError | 30s | ✓ | 上游 5xx |
| TokenRefreshFailed | 60s | ✓ | 刷新失败 |
| ModelUnavailable | 300s | ✓ | 模型不可用 |
| AuthenticationFailed | 3600s | ✗ | 认证失败（需手动/长冻） |
| AccountSuspended | 86400s | ✗ | 账户暂停 |
| QuotaExhausted | 86400s | ✗ | 配额耗尽 |

### 4.3 重试预算（provider.rs::compute_max_retries）

写死 9 次的旧逻辑**已废**。现按凭据数动态算：每号 `MAX_RETRIES_PER_CREDENTIAL=3`，
**小号池（total ≤ 3）降为每号 1 次**（小池反复砸只会加重冷却，不如各摸一次即透传，让客户端自身退避）。
下限 = 可用凭据数（保证每个可用号至少摸一次），绝对硬上限 64。单请求墙钟预算 **45s**（防雪崩闸门：
超时停止重试、把最后错误透传，不让一个卡住的请求扫冷全池）。

---

## 五、Token 刷新机制

### 5.1 三种上号 + 刷新路径

| 方式 | 上号 | 刷新端点 | 备注 |
|------|------|---------|------|
| social | app.kiro.dev OAuth PKCE（本地回调服务器 / 远程粘贴） | Cognito `InitiateAuth` | ZyphrZero PKCE 移植 |
| idc | AWS IAM Identity Center 设备码 | OIDC `/token`（clientId+clientSecret+refreshToken） | region 过白名单 |
| external_idp | M365/Azure 双段 PKCE，地址栏 URL 粘回引导 | 同 idc OIDC 口径 | 必须带真实租户 profileArn（kiro.dev 迁移后缺则 400） |

### 5.2 预刷新 vs 按需刷新（双路径）

| 维度 | 预刷新（受管后台任务） | 按需刷新（请求热路径） |
|------|----------------------|----------------------|
| 触发 | 后台定时扫描（默认间隔可配） | 请求路径发现 token 过期 |
| 阻塞 | 不阻塞请求 | 阻塞当前请求直到完成 |
| 提前量 | lead_minutes（默认 15min） | 5min |
| 失败处置 | 累积 failure_count | 返回错误 → 重试其他凭据 |
| 热重载 | TIER2：改 proactive/lead/interval 后 abort+respawn 即时生效不重启 | — |

两路径共用 `refresh_lock`；进入刷新前二次确认是否仍需刷新，避免重复刷。刷新回写前比对 refresh_token
快照，防并发覆盖（陈旧守卫）。

---

## 六、协议转换层（converter.rs / stream.rs）

### 6.1 Anthropic → Kiro 映射

| Anthropic | Kiro |
|-----------|------|
| messages: [{role, content}] 平铺 | conversationState.history[] + currentMessage 分层 |
| tool_use 在 assistant.content[] | tool_uses 独立字段 |
| tool_result 在 user.content[] | UserInputMessageContext.tool_results |
| model: "claude-sonnet-4-*" | map_model → "claude-sonnet-4.5" 等模型串（contains 匹配 + 无版本号回退） |
| stream: true/false | 上游始终 event-stream；非流式由我方聚合 |
| conversationId | continuationId 确定性派生（命中上游 prefix 缓存，实测省 ~47% credit） |

### 6.2 对话状态机（三态）

`ExpectingUser / ExpectingAssistant / InToolUse{...}` 三态间转换，按 role + 内容类型
(text/tool_use/tool_result) 精确追踪，最终构造 Kiro `ConversationState`。

### 6.3 AWS event-stream 解码（parser/*）

自实现流式零拷贝解码器：`prelude(12B) + headers + payload + 双 CRC32(prelude + message)`。
`EventStreamDecoder` 状态机（Ready/Parsing/Recovering/Stopped）增量 feed → decode 单帧。
帧上限 16MB。10 种 header 值类型、11 种 ParseError。

---

## 七、安全层（common/security.rs + ssrf.rs）

执行顺序（Axum 中间件外→内）：

```
请求进入
 ├─ 0. (可选) SecurityState 中间件 —— IP白名单/限流/XFF 三者全未配则不挂载(零开销)
 │      client_ip: trust_forwarded=true 时取 X-Forwarded-For 最右段（H2 修复：最左可伪造）
 │      IP 白名单(CIDR) 不匹配 → 403；每-IP 令牌桶超限 → 429
 ├─ 1. CorsLayer —— cors_allowed_origins 白名单
 ├─ 2. DefaultBodyLimit —— 默认 256MiB 软上限（buffered，可设 0 完全放开）
 └─ 3. auth_middleware —— constant_time_eq 验证 x-api-key / Bearer（空 key fail-closed）
```

- **SSRF 防护**（ssrf.rs）：登录页背景图代理 `/admin/api/bg-img?url=` 匿名可达且回显响应体，
  故统一防线：只允许 http/https（背景图仅 https）→ 解析所有候选 IP 逐个校验拒绝私有/环回/
  链路本地/云元数据段（含 IPv4-mapped IPv6）→ `resolve_to_addrs` 把域名固定到已校验 IP（防
  DNS rebinding TOCTOU）→ 禁用重定向（防 302 跳内网绕过）。
- **region 白名单**（H3/M1）：凭据 region/auth_region/api_region + idc 上号 region 过
  `SUPPORTED_KIRO_REGIONS`，污染值不再拼进上游 host（否则 refresh_token 可能被 POST 到攻击者域）。

---

## 八、用量统计 Pipeline（usage/*）

```
请求路径 (tokio task)               专用 OS 线程 worker
┌──────────────┐   try_send      ┌────────────────┐
│  record()    │ ──────────────→ │ SyncSender 容量 │→ TraceDb  (SQLite 逐条)
└──────────────┘  满则丢弃+计数   │ 10,000 · panic  │→ UsageStats(JSONL+内存预聚合)
                                  │ catch_unwind    │
                                  └────────────────┘
```

- **为何 std::thread 而非 tokio task**：sink 做同步阻塞 IO（SQLite execute / writeln!），
  跑在 tokio worker 上会被慢盘/fsync 抖动侵蚀线程池、延迟传导回请求路径。独立 OS 线程隔离，
  请求路径只做一次非阻塞 `try_send`。
- 容错：通道满丢弃 + AtomicU64 计数；sink panic 用 `catch_unwind` 捕获不影响其他 sink；`OnceLock` 单次初始化。
- 内存聚合：hourly 桶 + model/credential/session/client 分组，按 session_id/client_ip 定时回收
  防无界增长（5min tick）；trace_db 保留期清理（6h tick）。

---

## 九、配置热重载三部曲 + OTA

- **TIER1（原子镜像）**：`Config` 存 `ArcSwap<Config>`，冷却/限流/亲和/RPM/负载均衡/快失败/自动禁用
  等改配置即时生效不重启（6 个 Atomic 镜像 + reload_config）。
- **TIER2（后台任务 abort+respawn）**：token 预刷新（`respawn_refresh_task`）、余额刷新
  （`respawn_balance_task`）改间隔/开关后重挂即时生效。
- **TIER3（AppState 进程级镜像）**：`extract_thinking` / `compression` / `strip_env_noise` 播种进
  进程级镜像，handler 读镜像而非固化 state，admin 改后即时生效。
- **诚实边界**：`prompt_cache_ttl` / proxy / tls / 端口 / adminKey 仍需重启。
- **OTA 自更新**（admin/update.rs + common/health_marker.rs）：面板一键检查/升级，多镜像回退拉
  GitHub，下载的二进制**必须过 sha256 校验**（校验文件从 github.com 直连取，切同源投毒 RCE 链），
  原子 rename 覆盖 exe → systemd 拉起新版；替换前备份 `.bak`，配合 systemd `ExecStartPre` 守卫脚本
  实现「新版启动即崩 → 自动回滚旧版」闭环（回滚决策放 systemd 层，不放可能已崩的进程自己）。

---

## 十、关键设计决策表

| # | 决策 | 原因 |
|---|------|------|
| 1 | 用量 Pipeline 用 `std::thread` 不用 tokio task | SQLite/fsync 阻塞 IO 会侵蚀 tokio 线程池 |
| 2 | `parking_lot::Mutex` 保护凭据状态 | 极低锁开销，持有微秒级，无需 async |
| 3 | `TokioMutex` 保护 token 刷新 | 刷新涉网络 IO（数秒），需 async 友好 |
| 4 | inflight 只进排序键、不做硬门槛 | 硬门槛会造成假性排队（多客户端体感极慢） |
| 5 | 选号 + inflight+1 + rpm.record 同一锁临界区 | 原子选号，根治并发惊群/Top5 热点 |
| 6 | 健康熔断器与冷却分层（软放回 vs 硬退场） | 冷却到期不全量涌回把缓过来的号又打进风控 |
| 7 | M365 族级连坐（family_key） | 账户级风控整族退避，不逐个砸 |
| 8 | continuationId 确定性派生 | 命中上游 prefix 缓存，实测省 ~47% credit |
| 9 | 每号独立 machineId（撞车自动轮换）+ 环境噪音剥离 | 防关联 + 省 token + 提缓存 |
| 10 | 自实现 AWS event-stream 解码器 | 无现成 Rust 库支持流式零拷贝解码 |
| 11 | 双端点 /v1 和 /cc/v1 | Claude Code 依赖精确 input_tokens，需缓冲等 contextUsageEvent |
| 12 | 小号池降重试 + 单请求 45s 墙钟预算 | 防「没入站却一直 429」的重试放大雪崩 |
| 13 | Config 存 ArcSwap + 进程级镜像 | 配置热重载，读端无锁近零成本 |

---

## 十一、API 端点总览

**Anthropic 兼容（需 x-api-key / Bearer）**
- `GET /v1/models`（当前硬编码 Opus 4.7/4.8 等，含 -thinking 变体）
- `POST /v1/messages` · `POST /v1/messages/count_tokens`
- `POST /cc/v1/messages` · `POST /cc/v1/messages/count_tokens`（Claude Code 缓冲变体）

**Admin API（`/api/admin/*`，需 admin key；OAuth 回调 `/auth/callback` 公开）**
- 凭据：`GET/POST /credentials`、`DELETE /credentials/{id}`、回收站 `trash`(list/purge/restore)、
  `{id}/disabled|priority|rpm-limit|name|proxy|reset|refresh|verify|balance|export`、
  `{id}/overage`(status/enable/disable)、`balances/cached`
- 配置：`GET/PUT /config`、`GET/PUT /config/load-balancing`
- 上号：`auth/social/{start,poll}`、`auth/idc/{start,poll}`、`auth/external-idp/{start,leg1,leg2}`
- 用量：`usage/{overview,timeseries,by-model,by-credential,recent,rate,clients,machines,throughput}`、
  `ratelimit/insights`、`stream/live`(SSE)
- 运维：`service/restart`、`storage/{stats,cleanup}`、OTA `update/{check,perform,status}`

**Admin UI**：`GET /admin`（rust-embed React SPA + SPA fallback）

---

## 十二、源码目录结构（约 35,800 行）

```
src/
├── main.rs                   (481)   入口：19 步初始化 + 优雅停机
├── token.rs                  (250)   token 计算（本地估算 + 远程 API）
├── http_client.rs            (220)   ProxyConfig + reqwest Client 构建
├── debug.rs                  (210)   调试工具（hex dump / CRC / 事件打印）
├── model/  config.rs (674) · arg.rs (14)                全局配置 + CLI 参数
├── common/                          通用
│   ├── security.rs (454)   CORS/IP白名单/每-IP限流/XFF最右段/Body限制
│   ├── ssrf.rs     (307)   出站 URL SSRF 防护 + DNS 固定
│   ├── health_marker.rs (189)  OTA 启动健康标记 / crashloop 回滚兜底
│   └── auth.rs      (41)   API Key 提取 + constant_time_eq
├── kiro/                            上游 Kiro 协议 + 调度
│   ├── token_manager.rs   (5239)  ★ 多凭据管理 + 选号 + 刷新 + 亲和（最大文件）
│   ├── provider.rs         (911)  核心代理：重试/故障转移/Client 缓存/动态重试预算
│   ├── health.rs           (418)  AIMD 熔断器 + EWMA 健康分 + 族级连坐
│   ├── cooldown.rs         (592)  8 种冷却原因 + 差异化时长
│   ├── rate_limiter.rs     (523)  每日/最小间隔/退避/抖动（结构化 FailureKind）
│   ├── scheduling.rs       (190)  InflightGuard(RAII) + RpmTracker(60s 滚动窗)
│   ├── overage.rs          (233)  超额真开关（幂等 + 审计，单号显式请求）
│   ├── web_portal.rs       (407)  app.kiro.dev Web Portal 客户端（rpc-v2-cbor）
│   ├── machine_id.rs       (336)  machineId 生成/派生/撞车轮换
│   ├── affinity.rs          (94)  会话亲和（session_id → credential_id, TTL）
│   ├── refresh_loop.rs      (70)  受管后台预刷新
│   ├── auth/  social.rs (349) · idc.rs (258)            OAuth PKCE / SSO 设备码
│   ├── endpoint/  mod.rs (355) · ide.rs (181)           KiroEndpoint trait + IDE(kiro.dev)
│   ├── parser/  decoder/frame/header/crc/error (~1090)  AWS event-stream 解码
│   └── model/  credentials.rs (1435) · requests/* · events/* · usage_limits.rs (365) …
├── anthropic/                       Anthropic 兼容层
│   ├── converter.rs       (2947)  ★ Anthropic → Kiro 格式转换 + 环境噪音剥离
│   ├── stream.rs          (2599)  ★ Kiro event-stream → Anthropic SSE（Stream/Buffered 双 ctx）
│   ├── handlers.rs        (1578)  请求入口 + 流式/非流式 + WebSearch 分派
│   ├── websearch.rs        (981)  MCP WebSearch
│   ├── compressor.rs       (584)  ConversationState 输入压缩（规避上游 ~5MiB）
│   ├── types.rs (311) · router.rs (96) · middleware.rs (64)
├── admin/                           Admin 管理 API
│   ├── service.rs         (1990)  业务逻辑核心（凭据 CRUD/余额/配置/受管余额任务）
│   ├── handlers.rs         (706)  HTTP 处理器 · types.rs (697)
│   ├── external_idp_login.rs (704)  M365 双段 PKCE 上号
│   ├── update.rs           (405)  GitHub 版本检查 + 二进制 OTA
│   ├── usage_handlers.rs   (302)  用量查询 + insights + SSE stream/live
│   ├── social_login.rs (342) · idc_login.rs (221) · router.rs (125) · middleware.rs (64) · error.rs (64)
├── usage/                           用量统计
│   ├── usage_stats.rs     (1812)  JSONL + 内存预聚合 + 设备/客户端识别
│   ├── record.rs           (604)  RequestRecord 数据契约
│   ├── trace_db.rs         (514)  SQLite 逐条持久化（rusqlite）
│   └── pipeline.rs         (165)  异步管道（专用 OS 线程 + SyncSender）
└── admin_ui/  router.rs (544) · mod.rs (10)             rust-embed React SPA + 登录背景图代理
```

> 说明：v0.4.0 删除了旧的 `anthropic/cache_tracker.rs`（影子 prompt 缓存记账，在大请求热路径同步
> 跑 SHA256 是固定开销且不省钱，真正省 credit 的是 continuationId 派生，未受影响）。

---

## 附录：端口

| 配置 | 用途 | 默认值 |
|------|------|--------|
| `host` / `port` | 单端口：Anthropic API + `/api/admin` + `/admin`（全部 nest 同端口） | `0.0.0.0` / `8080` |

> 部署侧（自用 homecloud）实际对外端口以 systemd / 反代配置为准（现网走 8990），与代码默认值 8080 无关。



