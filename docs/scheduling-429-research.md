# 429 深度推理研究：分类体系、垃圾/低质量、根治方案

> 状态：**研究完成（2026-08-16）**，只研究不改代码。
> 用途：429 处理全链路现状 + 问题清单 + 根治方案表，供调度优化排期。
> 证据等级：工作树代码逐段精读（行号以工作树为准，多会话并发会漂移）+ 参考仓源码
> （`/tmp/ref-zyphr`，codegraph 已建）+ 客户端行为文档（`docs/error-codes-client-behavior.md`）。
> 关联：`docs/error-codes-inventory.md`（A/B/C/D/E/F/G 表）、`docs/blockers-protocol.md`
> （子串匹配/魔法值清单）、`.opencode/ISSUES.md`（(e) 429 链研究结论）。
> 范围：只覆盖 **429 的判定、吸收、冷却、换号、端点封禁、Retry-After 决议、客户端回显**。
> 不覆盖：配额类（B2/B3）语义、403 临时风控（A8）细节（仅在与 429 纠缠时提及）。

---

## 1. 429 全链路现状图

### 1.1 上游 429 在网关内的分类路径总览

```
上游返回 429
   │
   ├─[A] 入站整形闸门（handler 层，所有请求先过）────────────┐
   │     try_inbound_admission_gate（handlers.rs:2574）
   │     acquire_admission 超时 → inbound_admission_timeout=1 retry_after_secs={闸门真值}
   │     → map A3（handlers.rs:2197）429 + RA=串内真值（配置兜底，兜底 1s）
   │     → absorb_class_of 显式 None（handlers.rs:2008）→ 不吸收、不上抛
   │
   ├─[B] 透传池（custom_api，try_custom_api_passthrough，provider.rs:1460）──┐
   │     429 → 同号吸收？（passthrough_absorb_should_retry，:1854，默认关）
   │            │ 是 → sleep（passthrough_absorb_delay_ms，:1871；⚠️不读上游 RA，:1823）
   │            │ 否 → 5s 调度级跳过冷却（:2068/:2080）+ excluded 换下一号
   │            └ 全部失败 → None → 落 Kiro 主路径 [C]
   │     注：透传 429 本身**不直接回客户端**（should_failover 恒真，:1983）；
   │     只有「换号无益」类 4xx（hopeless 400/404 等）才 E5 原样透传（含上游 Retry-After 白名单头）
   │
   └─[C] Kiro 主路径（call_api_* 的 'absorb loop，provider.rs:2900）──┐
        每跳（for attempt in 0..max_retries，:2926）：
        ① 选号（acquire_context_excluding，:2954）——冷却中的号被调度结构性避开
           全池冷却 bail（C2，token_manager.rs:4960-4976）→ retry_after_secs={最短恢复}
        ② select_endpoint（:3007）——端点桶被封 → None → 30s 冷却换号（:3017-3022）
        ③ 发上游，429（:4013-4119）：
           - retry_after = 响应头 Retry-After（u64 解析，:3364-3371；HTTP-date 被丢）
                          or body resets_*（endpoint/mod.rs:615-639）
           - 多端点凭据：封当前桶 30s（ENDPOINT_BUCKET_THROTTLE，:174/:4053）+ 换端点（:4057-4069）
           - 单端点凭据：凭据级冷却（report_rate_limited_with_retry_after，:4080-4083）
             ——上游 RA 秒数进冷却（clamp MAX_RETRY_AFTER_COOLDOWN_SECS，token_manager.rs:6134-6146）；
             无 RA → 固定基线冷却（不指数升级，:6142-6146）
           - 429 专用长退避 sleep 1s→2s→4s→8s（retry_delay_throttle，:4114/:4552）
           - 本请求链内同号不重复惩罚（rate_limited_this_call 去重，:2736-2741/:4070-4089）
        轮末：
           absorb_class_of(last_error)（handlers.rs:2007）
             PoolCooldown(秒) → 按真值 sleep 再打一轮（:399/:2900 循环）
             UpstreamRateLimit → 指数吸收（:402）
             非可吸收类（inbound/永久耗尽/配额等）→ None 上抛
        终态：
           吸收层真跑过并放弃 → absorb_budget_exhausted=1 → map A2（handlers.rs:2147）
             503 + RA = 串内真值 → 配置 → 风控 20s → 兜底 8s（:2161-2168）
           没开吸收（默认关，config.rs:1559 false）→ last_error 直接上抛 → map_provider_error
             A7（is_upstream_rate_limited）→ 429 + RA = 配置 → 固定 8s（:2336-2338）
             A5（retry_after_secs= 串）→ 429 + RA = 号池真值（:2255-2278）
             A3/A4（背压）→ 429 + RA = 串内真值 → 配置 → 1/2s（:2210/:2239）
             B2/B3（配额）→ 429 **无 RA**（等下个计费周期，:1485-1540）
```

其他 429 相关出口（不经过上面主链）：

| 出口 | 位置 | 形态 |
|---|---|---|
| 流式 in-band 上游 error 事件 | stream.rs:72-93 + `is_rate_limit_signal`（:126-137） | 限流信号 → SSE `error` + `overloaded_error`；非流式完成态 → 429（http_status_u16 :88-93） |
| 空/近空响应（D10） | handlers.rs:3084-3132 / :4001-4022 | 429 + `overloaded_error` + RA（默认 3s，非流式 HTTP 挂头；**流式 SSE 事件丢弃 RA**，:3124） |
| WebSearch 快路径共享预算耗尽（F3） | websearch.rs:400-411 | 503 + RA（配置 → 8） |
| OpenAI 兼容层（G5） | openai/handlers.rs:359-422 | 透传内层 status/type/Retry-After（内层没给不自造，:590-596） |
| MCP 调用 429 | provider.rs:2596-2644 | 封桶（多端点）+ 凭据冷却 + continue（不上抛） |

### 1.2 每环节 Retry-After 来源与优先级（现状）

| 分支 | RA 决议链（优先级从高到低） | 位置 |
|---|---|---|
| A5 全池冷却 | 号池真值 `retry_after_secs=N`（token_manager 算出的最短恢复秒）——**唯一来源，配置不可覆盖** | handlers.rs:2255-2268（`_cfg_ra` 不读） |
| A2 吸收耗尽 | 串内真值 → 配置 → 风控类 20s → 兜底 8s（clamp 1-300） | handlers.rs:2161-2168 |
| A3 入站背压 | 串内真值 → 配置 → 兜底 1s | handlers.rs:2210；准入闸门处同 key 但**不读配置**（handlers.rs:2599 `_cfg_ra`） |
| A4 并发闸满 | 串内真值（provider 打 2s）→ 配置 → 兜底 2s | handlers.rs:2239 |
| A7 上游限流 | **配置 → 固定 8s**——上游 Retry-After 不进这里 | handlers.rs:2336-2338 |
| A8 临时风控 | 配置 → 固定 20s | handlers.rs:2372-2374 |
| A1 共享预算耗尽 | 配置 → 8s | handlers.rs:2121-2123 |
| B4 容量 503 | 配置 → 3s | handlers.rs:1556-1564 |
| D10 空响应 | 配置 → 3s | handlers.rs:3102-3113 |
| F3 websearch 预算 | 配置 → 8s | websearch.rs:400-411 |
| 透传 E5 原样透传 | 上游 Retry-After 原样（白名单头） | passthrough.rs（inventory E5） |
| OpenAI G5 | 内层算好的秒数原样 | openai/handlers.rs:370-389 |
| 凭据级冷却（内部） | 上游响应头 u64 → body resets_* → 固定基线（无 RA 时） | provider.rs:4035-4037 + token_manager.rs:6129-6147 |

### 1.3 优先级正确性评估

**做对的**：
- A5 号池真值不可被配置覆盖（handlers.rs:2256 注释「号池算出的剩余秒数比任何常数都准」）——与 A2 注释同一论证，方向正确。
- A2 的真值→配置→类别→兜底四层链是全场最完整的决议（handlers.rs:2160-2168）。
- 凭据级冷却吃上游 RA 并 clamp 上界（token_manager.rs:6131-6140），防「本月配额 resets_at」把号冻几天。
- B2/B3 配额 429 刻意不带 RA（H2 契约，inventory §2a）。

**做错的 / 缺口（详见 §3 问题清单）**：
1. **上游显式 Retry-After 只进凭据冷却、不进客户端响应**——provider.rs:4037 解析出的
   `retry_after` 仅用于 `report_rate_limited_with_retry_after`（:4073/:4083），错误串
   （:4105-4110）不带任何 marker → 客户端拿到的 A7 恒 8s。上游明说「30s 后恢复」时
   客户端 8s 就重打，白打一轮又被冷却。
2. **HTTP-date 格式 Retry-After 静默丢失**（upstream_trace.rs:102-105 自认）→ 冷却回退
   固定基线。
3. **透传同号吸收不读上游 RA**（provider.rs:1823 自认，退避被 max_delay_secs 夹住最坏 15s）。
4. **D10 流式形态丢 RA**（handlers.rs:3124 `_retry_after` 丢弃）——SSE error 事件无
   Retry-After 通道，流式空响应 429 客户端只能指数退避（H1 契约在流内形态不成立）。
5. **A5 分支注释与行为不一致**（error_messages.rs:156-157 说「配置只是兜底」，实际
   handlers.rs:2258 的 `_cfg_ra` 完全不读——配置了 `rate_limited_pool.retryAfterSecs`
   的管理员会发现它静默无效）。
6. **gate_timeout 同 key 两个调用点行为分叉**：map 分支读配置兜底（handlers.rs:2210），
   准入闸门不读（:2599）——M2 合一 key 后同一 key 两种语义。

---

## 2. 分类质量

### 2.1 子串匹配判定（残存清单，已对照 blockers-protocol §3 现状核实）

**W10-W12 已修（#14 结构化）**：quota/subscription 宽词（handlers.rs:1522 收口到
`default_is_monthly_request_limit` 精确 reason 词表；:1601-1615 收窄到订阅失效连续形态）、
524 裸数字（handlers.rs:1824-1825 `: 524` 形态）、token_manager 裸数字六连、`is_hopeless_upstream_400`
连续形态词表（provider.rs:967-987，不再认裸 `quota`）。

**仍残存的 429 相关子串判据**：

| # | 位置 | 判据 | 判定 | 误伤风险 |
|---|---|---|---|---|
| R1 | stream.rs:126-137 `is_rate_limit_signal` | `throttl`/`toomanyrequests`/`ratelimit`/`rate limit`/裸 `429`/`overload`/`quota`/`exhaust` | 流内限流 → SSE `overloaded_error` + 非流式 429 | **前缀级超宽词族 + 裸数字**：上游 code/message 任意含 `quota`/`exhaust`/`overload` 即判限流；裸 `429` 命中 requestId/时间戳。误判方向 = 把永久错误标成可重试（客户端无限退避）。**这是 429 误判的头号残存** |
| R2 | handlers.rs:1299 | `Too many requests`（大小写敏感） | A7 兜底文案匹配 | 漏判落兜底（安全侧），上游变体 `too many requests` 不命中 |
| R3 | handlers.rs:1522 | `default_is_monthly_request_limit` 子串兜底（body 含 `MONTHLY_REQUEST_COUNT` 字样即命中） | B3 配额 429 无 RA | 上游 body 恰好含该串即误判配额（注释自认「降级保留」，面已缩小） |
| R4 | provider.rs:2968 | `es.contains("retry_after_secs=") \|\| es.contains("冷却")` | 度量桶 RateLimited | 中文宽词「冷却」出现在其它文案即误归限流桶（仅影响度量，低危） |
| R5 | handlers.rs:1430-1435 | 裸 `403` 辅助（bearer-invalid 语境） | region 错配排除 | 注释自认 requestId 含 `401` 漏判（安全侧） |
| R6 | endpoint/mod.rs:426-428 | 裸 `suspended` 且非 `temporar` 前缀 | 永久封禁 | 裸词判据，已删 `locked` 裸词（:419-422 注释），残留 `suspended` 裸词；`Account locked...temporarily` 类不命中（已排除 temporar 族） |

### 2.2 吸收层的 429 语义（absorb_class_of 怎么分 429）

`absorb_class_of`（handlers.rs:2007-2061）对 429 相关错误的分界：

- **吸收**（在单请求预算内重试）：
  - `PoolCooldown(secs)`：带 `retry_after_secs=` 真值的一切（全池冷却/池耗尽/RPM 饱和/模型 TTL）→ 按真值 sleep（provider.rs:399）
  - `UpstreamRateLimit`：`is_upstream_rate_limited`（USER_REQUEST_RATE_EXCEEDED 等）→ 指数吸收（:402）
- **上抛（None）**：
  - `inbound_admission_timeout=1`（网关背压，重试无意义，:2008）
  - `model_unsupported_by_pool=1`（永久态，:2011）
  - `pool_permanently_exhausted=1`（45s 预算内等多久都不会变，:2019）
  - `upstream_gate_full=1`（网关背压，带 retry_after_secs=2 会被 PoolCooldown 抢走，:2027）
  - region 错配 403、配额类、网络/TLS、其它 4xx（:2058-2060）

**边界评价**：分界正确——「网关自己背压」与「永久态」三类（inbound/gate-full/perm-exhausted）
都带 `retry_after_secs=` 却刻意排除在 PoolCooldown 之前（:1987-2029 顺序注释 + 守卫测试
:6031-6069/:6144-6162 钉死），这是本仓踩过坑后的正确收口。守卫测试完备（
`absorb_class_of` 顺序守卫 + 10 条分类用例）。

### 2.3 全池冷却快速失败 vs zyphr「最早类型化 429 保留」

**现状（我们）**：全池冷却 bail（C2，token_manager.rs:4960-4976）带的
`retry_after_secs={wait}` 是 `transient_wait_outcome` 算出的**池内最短恢复秒**（真实值，
不是常数）；C4 RPM 饱和走 `RpmTracker.kth_oldest_age` 时间戳精确推导（ISSUES (d) 已做
「RPM release_index 精确化」）；C11 模型 TTL 剩余同理。**即「全池冷却快速失败」的
RA 精度并不粗**——zyphr 的 #6/#8（按滑动窗口时间戳精确推导剩余秒数）我们等价已覆盖。

**真正缺失的是 zyphr #8 的另一半**：重试链内**最早的类型化 429 不被后续 generic 错误覆盖**。
我们的 `last_error` 每跳覆盖（provider.rs:2734/:4105），终态按**最后一个**错误分类。
场景：多号池 attempt1 = A 号 429（上游 RA 30s，冷却 30s）、attempt2 = B 号 429、
attempt3 = C 号 5xx → 终态 = 5xx → 客户端拿 503+3s，而不是 429+30s。丢失的是
「429 语义 + 上游精确 RA」。

**ISSUES (e) 的旧结论重估**（当时记「两级退避已兜住，中改排后」）：
- 吸收层**默认关**（config.rs:1559 `upstream_retry_absorb_enabled: false`）——「吸收层把
  429 兜住」的前提在默认配置下不成立；主路径 failover 换号是唯一兜底。
- 但丢失的只是**信号精度**（503+3s vs 429+30s），不是可重试性——CC 对 503 也退避重试
  （error-codes-client-behavior.md §1.4），opencode 对 5xx 强制重试。行为面损失有限。
- 唯一的行为级损失：429 触发 CC 的「精确等待 Retry-After」路径（§1.4：`max(Retry-After,
  退避)`），503 只能指数退避——对上游给了长 RA（30-60s）的场景，503 会让客户端更早重打。
- **结论：值得做，但必须与 §3.1 的「上游 RA 进客户端」合并做**（同一通道，见 §5 方案 2）；
  单独做保留逻辑不动 RA 没意义。

---

## 3. 客户端视角

### 3.1 我们回给客户端的 429 形态 vs Claude Code 重试行为

| 我们的出口 | status/type/RA | CC 行为（error-codes-client-behavior.md §1.4） | 匹配度 |
|---|---|---|---|
| A3/A4 背压 | 429 `rate_limit_error` + RA（真值/1-2s） | 必重试，`max(RA, 退避)` | ✓ |
| A5 全池冷却 | 429 `rate_limit_error` + RA（真值，clamp 1-300） | RA ≤60s 精确等待；**>60s 直接放弃** | ✓（放弃 = 别白等语义正确） |
| A7 上游限流 | 429 `rate_limit_error` + RA（8s 固定） | 精确等待 8s | ⚠️ 见 §3.2 问题 1（上游 RA 被吞） |
| A8 临时风控 | 429 `rate_limit_error` + RA（20s） | 精确等待 20s | ✓ |
| B2/B3 配额 | 429 `rate_limit_error` + **无 RA** | 无头 → SDK 指数退避（默认 2 次）→ CLI 指数退避（500ms×2^n cap 300s） | ✓（H2：配额不给秒数是刻意） |
| D10 空响应（非流式） | 429 `overloaded_error` + RA（3s） | 529/overloaded 语义退避重试 | ✓ |
| D10 空响应（流式） | SSE `error` + `overloaded_error`，**无 RA** | 流内 error → 客户端按 overloaded 退避（无精确等待） | ⚠️ 协议无 RA 通道（§3.2 问题 4） |
| 流内 in-band 429 | SSE `error` + `overloaded_error`（is_rate_limit_signal） | 同上 | ✓（但判据过宽，§2.1 R1） |
| A2 吸收耗尽 | 503 `api_error` + RA（8s 兜底） | 503 自行退避重试，频率受 RA 控制 | ✓（刻意：Cursor 见 429 掐会话，见 :2096-2099） |

**与 CC 60s 放弃线的交互**：A5 的 clamp 1-300 允许 >60s（模型 TTL 剩余、30min 级冷却）。
CC 见 >60s 直接放弃该请求（不重试）——语义上正确（等真实恢复太久），但注意这与
「我们主动吸收 + 客户端退避」的目标冲突：若客户端放弃，请求实际失败。对 C2 全池冷却
（最短恢复恒 ≤30min 递增曲线）而言，>60s 只出现在全池几乎死透的场景，放弃可接受。

**显示**：我们不带 unified 头 → CC 429 原样显示 `error.message`（§1.5 订阅场景带 unified
头才丢弃）✓。message 无 H7 决策词 ✓（blockers §1.2 核对过）。

### 3.2 errorMessages 42 key 中 429 相关 key 的默认值质量

| key（error_messages.rs） | 默认 (status/type/RA) | 质量评价 |
|---|---|---|
| `gate_timeout`（:137-143） | 429/rate_limit_error/1 | ✓ 承重英文背压哨兵在案（:126-136）；⚠️ 双调用点配置语义分叉（§1.3 问题 6） |
| `upstream_gate_full`（:146-152） | 429/rate_limit_error/2 | ✓ 同上 |
| `rate_limited_pool`（:157-163） | 429/rate_limit_error/None | ✓ 文案 = shield 双判据承重串（:153-155）；⚠️ 配置 RA 注释与行为不符（§1.3 问题 5） |
| `rate_limited_credential`（:173-179） | 429/rate_limit_error/8 | ✓；⚠️ 8s 是常数兜底而非上游真值（§3.1 问题 1） |
| `account_throttled`（:181-187） | 429/rate_limit_error/20 | ✓ 与 cooldown.rs SuspiciousActivity 20s 同源（注释） |
| `quota_exhausted`（:224-230） | 429/rate_limit_error/None | ✓ 无 RA 正确（H2） |
| `quota_subscription`（:232-238） | 429/rate_limit_error/None | ✓ 同上 |
| `empty_response`（:425-431） | 429/overloaded_error/3 | ✓ D10 矛盾已修（表与调用点一致）；流式形态丢 RA 属协议限制 |
| `overloaded_capacity`（:242-248） | 503/overloaded_error/3 | ✓ B4 矛盾已修 |

总体质量高：RA 语义方向（退避 vs 配额停手 vs 永久态忽略）全部正确，承重哨兵都有
`check_load_bearing_message` 告警（error_messages.rs:521-542，W11 #12 已对齐 shield
实测判据词表）。瑕疵只有 §1.3 的问题 5/6 两处注释-行为不一致。

---

## 4. 垃圾/低质量清单

### 4.1 重复判定（同一错误多处 contains，改一处漏一处）

| 判据 | 出现次数 | 位置 | 状态 |
|---|---|---|---|
| `inbound_admission_timeout=1` | 2 | handlers.rs:2008 / :2197 | 裸字面量（blockers §2.2 登记） |
| `upstream_gate_full=1` | 2 | handlers.rs:2027 / :2227 | 裸字面量 |
| `model_unsupported_by_pool=1` | 2 | handlers.rs:2011 / :2288 | 裸字面量 |
| `shared_budget_exhausted=1` | 3+2 | provider.rs:2655/4409 + handlers.rs:2107 + websearch.rs:1121/1702 | 裸字面量 |
| `retry_after_secs=` 解析 | 3 调用点 1 实现 | parse_retry_after_secs（handlers.rs:1928-1934） | ✓ 已收敛 |
| `temporarily* suspended` 判据 | 2 套 | handlers.rs:1332-1334（2 串） vs endpoint/mod.rs:470-475（3 串+信号表） | **同一 403 文案两套字面量**，注释互相引用但未共用；漂移风险真实（endpoint 侧曾漏 `temporarily is suspended` 变体造成生产事故，:452-457 注释自述） |
| `Too many requests` | 1 | handlers.rs:1299 | 与 endpoint 侧 TEMPORARY_SIGNALS 无交集（不同语义，低危） |
| `429` 判定 | 3 种语义 | provider status==429（结构化）/ stream is_rate_limit_signal（子串）/ map A7（子串） | 各有职责，但「同一错误三类判据」的维护面大（R1 是其中最弱一环） |

### 4.2 marker 与文案耦合

blockers-protocol §2 已详述，本报告确认现状未变：
- 载体 = 「字符串内嵌 + 子串提取」，已 const 化仅 2 个（`ABSORB_BUDGET_EXHAUSTED_MARKER`
  handlers.rs:2070、`BEARER_INVALID_TRANSIENT_MARKER` :1347），其余 4 个 429 相关 marker
  裸字面量（§4.1 表）——**编译不报错、只靠守卫测试兜**（blockers :3825-3826 注释自承
  此类事故已踩过）。
- 一个意外收获：marker 语义**不进客户端 message**（blockers §2.1 核实）——429 相关
  marker 全部在 map_provider_error 渲染前被结构化消费，客户端看到的恒是 resolve_msg
  固定文案。这条方向正确，不需要动。

### 4.3 Retry-After 多源混乱

1. **常数多源**（blockers magic #1/#2）：8s 三处（handlers.rs:1285 + cooldown.rs:177 +
   派生 :2077，cooldown 的 8 未联动）；20s 五处（magic #2，含 cooldown.rs 基线表）。
2. **上游 RA 与客户端 RA 两套通道互不相通**：上游 RA（头/body）→ 凭据冷却
   （token_manager.rs:6134-6146）；客户端 RA ← 独立通道（marker/配置/常数，
   map_provider_error 内联）。§3.1 问题 1 即此断层的表现。
3. **HTTP-date 解析缺失**（upstream_trace.rs:102-105 自认）。
4. **透传吸收不读上游 RA**（provider.rs:1823 自认）。
5. **F3 websearch RA 8 与 A1 同源但字面量各写**（websearch.rs:400-411 vs handlers.rs:2077；
   inventory §2c 矛盾点 3 已修——现在走配置 + 调用点默认，但默认值仍是各写一份 8）。

---

## 5. 根治方案表

> 每条：问题 → 根治 → 风险 → 工作量。排序按「同一通道优先」：方案 1 是方案 2 的地基，
> 方案 3/4 是纯重构。

| # | 问题（证据） | 根治 | 风险 | 工作量 |
|---|---|---|---|---|
| S1 | **上游显式 Retry-After 不进客户端响应**（provider.rs:4035-4037 只喂冷却；A7 恒 8s，handlers.rs:2336-2338） | ① 主路径 429 时把解析出的上游 RA 写进错误串 marker（如 `upstream_retry_after=N`，复用 `retry_after_secs=` 的 parse 通道或新 marker）；② map_provider_error A7 分支优先读该真值（`parse.or(cfg).unwrap_or(8)`），与 A2 的四层链同构；③ 顺带补 HTTP-date 解析（httpdate crate 或 RFC7231 手写，~20 行） | 中低：行为变化 = 客户端 RA 从 8s 变上游真值；上游给 >60s 时 CC 放弃（符合语义）；给超大值（resets_at 到月底）必须 clamp 1-300（A7 已 clamp）；需要防「上游 body 里被注入 retry_after 字样」——marker 必须是**网关自己**打的串（与 `retry_after_secs=` 同款误伤面，blockers §2.2.2 已记录该形态，用结构化字段而非子串判定可规避） | 小-中：marker 通道 + A7 决议 + HTTP-date 解析 + 测试 ≈ **0.5-1 天** |
| S2 | **最早类型化 429 被后续 generic 错误覆盖**（last_error 每跳覆盖，provider.rs:4105；ISSUES (e) 记录过） | 在 S1 通道上扩展：failover 循环维护 `first_upstream_429: Option<(u64 RA)>`（首个 429 的 RA），终态 last_error 非 429 且 first_upstream_429 存在时，把 RA marker 并入上抛串 → 客户端拿到 429+上游 RA 而非 503/502。**限定**：仅「上游 429」适用；吸收层耗尽路径（absorb_budget_exhausted=1）保持 503 不转换（Cursor 掐会话兼容，:2096-2099 论证不破） | 中：行为变化 = 某些混合失败链从 503 变 429；429 对 Cursor 是掐会话信号——但这是**上游真 429**（本来就该退避），与 A2 的「网关尽力了」503 语义不同；需守卫测试钉「不转换 absorb 耗尽」 | 中：provider 循环 + 终态合并 + 测试 ≈ **1-1.5 天**（与 S1 合并做，单独做无价值） |
| S3 | **上游明确 RA 时的换号策略**（ISSUES (e)「维持现状」） | **重估结论 = 维持换号，不移植 zyphr「禁换号」**：我们多号多账号模型下换号真实有效（must_wait_for_upstream 已论证，ISSUES (d)）；上游 RA 已进凭据冷却（token_manager.rs:6134）——「等」的语义由冷却时长表达。唯一要补的是 S1 的 HTTP-date 解析（否则上游给了 HTTP-date RA 时冷却退化固定基线） | 无（维持现状） | 0（HTTP-date 已并入 S1） |
| S4 | **Retry-After 多源优先级显式化**（§4.3：两套通道 + 常数多源 + 注释-行为不一致） | ① 建一张「RA 决议表」文档 + helper：`真值(retry_after_secs= / 上游 RA) > 配置 > 类别常数 > 兜底`，四个分支（A2/A3/A4/A5/A7）统一走一个 `resolve_retry_after(err_str, cfg_ra, class_default)`；② 修两处注释-行为不一致（A5 的 `_cfg_ra` 与 error_messages.rs:156-157 注释二选一对齐；gate_timeout 双调用点统一为「真值恒优先，配置不适用」并改注释）；③ 8s/20s 常数收敛（blockers magic #1/#2，cooldown.rs 的 8 引用 handlers 常量） | 低：纯重构 + 行为测试钉现状；唯一行为变化是 A5/gate_timeout 的配置兜底从「静默无效」变「明确忽略（注释）」——不改变任何实际输出 | 小：决议 helper + 注释对齐 + 常数收敛 ≈ **0.5-1 天** |
| S5 | **流内 429 判据过宽**（stream.rs:126-137 裸 `429`/`quota`/`exhaust`/`overload` 前缀词族） | 收窄 `is_rate_limit_signal`：删裸 `429`（改「带状态语境的三位码形态」，同 `: 524` 先例）；`quota`/`exhaust` 改连续形态（`quota exhausted`/`quota_exceeded` 等，对齐 is_hopeless_upstream_400 词表风格）；先采集流内 error 样本（D 类阈值纪律：先修度量再调参）确认无真限流漏判 | 中：收窄可能漏判真限流 → 落 api_error/502（仍可重试，安全侧）；需要样本支撑 + 反例测试（同 524 修复范式） | 中：样本采集 + 收窄 + 测试 ≈ **0.5-1 天** |
| S6 | **marker 裸字面量**（§4.1 表，4 个 429 相关 marker 各 2-3 份） | 收敛 const（2 个先例照抄：ABSORB_BUDGET_EXHAUSTED_MARKER / BEARER_INVALID_TRANSIENT_MARKER）；`consecutive_pool_unavailable=N` 补消费点或降级 | 低：纯重构 + 守卫 CI | 小 ≈ **0.5 天** |
| S7 | **D10 流式形态丢 RA**（handlers.rs:3124） | **不修，记录为协议限制**：SSE error 事件无 Retry-After 通道，把 RA 塞 message 会污染 H7 决策词空间；CC 对 overloaded_error 有自身退避，行为可接受。文档标注即可 | 无 | 0 |

**推荐执行顺序**：S1+S2 合并（同一通道，1.5-2.5 天）→ S4（0.5-1 天）→ S6（0.5 天）→
S5（0.5-1 天，等样本）。S3 维持现状、S7 不做。

---

## 6. 自 review

**覆盖检查**：任务清单 5 项全部落地——
① 429 分类全貌：§1 链路图 + §1.2/1.3 RA 来源优先级（每个环节的 RA 来源都给了文件:行号）；
② 分类质量：§2.1 残存子串（R1-R6 逐条现状核实，含 W10 修复确认）、§2.2 吸收层分界、
§2.3 zyphr 对比重估（含 ISSUES (e) 结论的「默认关吸收层」新证据）；
③ 客户端视角：§3.1 形态-行为对照表、§3.2 42 key 质量表；
④ 垃圾/低质量：§4 三张清单；
⑤ 根治方案：§5 七条方案表（问题→根治→风险→工作量）+ 执行顺序。

**证据核实记录**：
- `upstream_retry_absorb_enabled` 默认 false 是我现查的（config.rs:1559），否定了 ISSUES
  (e)「两级退避已兜住」的前提之一——这是本报告最重要的新事实。
- 「上游 RA 不进客户端」经代码确认：retry_after 变量（provider.rs:4037）的作用域只到
  report_rate_limited_with_retry_after 调用（:4073/:4083），错误串（:4105-4110）无 marker。
- A5 的 `_cfg_ra` 不读是现读代码确认（handlers.rs:2258），与 error_messages.rs:156-157
  注释矛盾，已列入 S4。
- stream.rs 裸 `429` 判据、透传不读 RA（:1823）、HTTP-date 丢失（upstream_trace.rs:102-105）
  均为注释+代码双重确认。

**局限性（诚实披露）**：
1. 行号以工作树为准（203 个未提交文件，多会话并发），改代码前必须重核。
2. S1/S2 的行为影响（上游 RA >60s 时 CC 放弃率）没有线上样本量化——上游显式给 RA 的
   频率、分布未知；建议 S1 落地前先跑一段 trace（upstream_trace 已存 retry_after_raw，
   字段现成，直接可统计）。
3. S5 的流内 error 样本同样缺失（trace 的 body 字段已有，可离线统计 is_rate_limit_signal
   误判率）。
4. 「吸收层默认关」是代码默认值，线上 config.json 实际值未现读（配置现读纪律）；若线上
   开了吸收，S2 优先级还要再降（吸收会把 429 吃掉）。
