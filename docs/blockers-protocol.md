# 协议/魔法值层绊脚石清单（blockers-protocol）

> 状态：**研究完成（2026-08-15 深夜）**，只研究不改代码。
> 用途：找出协议/魔法值层的绊脚石（文案与逻辑耦合、硬编码数字、隐式契约），
> 为「错误码可配置化」与「i18n 本地化」提供绊脚石级事实与修法。
> 方法：3 路并行探查（Rust marker 链 / 子串匹配+magic number / 前端语言耦合）
> + 线上实测（skiapi kiro_shield.py 判据全文核对，2026-08-15 现读）。
> 关联：`docs/error-codes-inventory.md`（错误清单）、`docs/error-codes-client-behavior.md`
> （客户端判据）、`docs/cooldown-reason-i18n-design.md`（语言耦合改造设计稿）。
> **本文件修正 inventory §3.1 的 1 处错误**（`等容量` 不是 shield 判据，见 §1.4）。
> **闭环状态（2026-08-16 核验）**：本文档列出的待修项已全部修复——错误码可配置化（W7-W8，
> 42 key + 承重串告警 + 矛盾修复 B4/D10/F3）、幽灵承重串勘误（W11 #12，shield 真实判据 3 词）、
> 前端语言耦合改枚举（W11 #13，cooldownCode 9 码 + duplicate_credential）、子串匹配结构化
> （W10-W12 #14）。正文保留为审计快照。

## 0. 一句话结论

- 网关内部已经做到「marker 只进内部错误串、对外 message 是固定文案」——分层方向正确；
  但 marker 以「字符串内嵌 + 子串提取」跨组件传输，同一字面量多处裸写、改一处漏一处不报错。
- 真正承重的是**给外部机器看的英文/中文哨兵串**（shield 4 个判据词、Claude Code 1 个压缩判据）；
  而其中最响的一个「承重串」`等容量` **实测是幽灵**——三层代码/文档基于错误前提锁死了它。
- 前端有 **5 处**后端中文文案判定（ISSUES (c) MAJOR 只登记了 3 处），另 2 处在改造时必漏。
- 裸数字/宽词子串判据是已踩过事故的形态（524 误吞、`All credentials` 挂错表 1753 次），仍有残存。

---

## 1. 承重字符串全集（外部/内部判据三维清单）

### 1.1 外挂判据：kiro_shield.py（skiapi 线上实测，2026-08-15 现读）

shield 是 Caddy 与网关之间的重试外挂（`/opt/skiapi/services/kiro_shield.py`），
`classify()` **按 body 文案分类，不按状态码**。与我们的响应文案形成硬契约：

| shield 判据串 | 命中我们的文案 | 出口 | 判据类型 |
|---|---|---|---|
| `temporarily cooling down` | A5「All credentials are temporarily cooling down. Please retry after the indicated delay.」 | 429 | COOLING_MARKERS → cool（听 Retry-After + 升档） |
| `All credentials are temporarily` | 同上 | 429 | 同上 |
| `inbound rate shaping` | A3「Gateway inbound rate shaping is at capacity...」 | 429 | 同上 |
| `请求体解析失败` | D4「请求体解析失败: {e}」 | 400 | PERMANENT → 不透传直接 pass |
| `凭据不支持刷新` / `不支持刷新 Token` | refresh 端点文案 | 400 | PERMANENT |
| `不被本号池支持` | C10 中间串（渲染后的 A6 文案是「不被当前号池支持」，不含该子串——命中面待核对） | 404 | PERMANENT |
| `Context window is full` 等上游措辞 | 透传上游 body 原文 | — | PERMANENT（防 74 分钟重试事故） |

**脆弱性（MAJOR）**：
- 这些串是**单点事实源**：改 A5 文案（如 i18n 化翻译成中文）→ shield 的 COOLING_MARKERS 全部失配 →
  classify 兜底 `503∈RETRYABLE→retry` 或 429→retry 恒定退避 → 丢掉「cool 升档」节奏，
  1753 次「所有凭据均已禁用」事故形态重演（CLAUDE.md 记录）。
- shield 的 retry 路径**也读 Retry-After**（`backoff_delay` cap 15s），真正「丢弃 Retry-After 走 20→60s」
  只发生在 swap 路径——那是 2026-08-04 已修复的旧形态（`All credentials` 曾挂 SWAP 表）。
- 判据在**仓外**（skiapi 脚本），本仓只能靠守卫测试间接钉住，仓库内无任何编译期保障。

### 1.2 客户端判据（Claude Code / opencode，见 error-codes-client-behavior.md）

| 判据 | 命中我们的文案 | 客户端行为 | 严重度 |
|---|---|---|---|
| `prompt is too long`（小写子串） | B9/B10 文案前缀 | Claude Code compact-and-retry 自动压缩 | MAJOR：删/译后客户端撞满上下文直接报错 |
| `"model: ` 字样 | 无（A6/B1 文案不含） | 404 触发模型切换提示 | 低（约束未来文案） |
| H7 决策词（`credit balance is too low` / `organization has been disabled` / `usage credits are required` / `extra usage is required` / `OAuth token has been revoked` / `overloaded_error` 字样 / 429/500/502/503/504 数字字样） | 无 | 各客户端把 message 当重试决策输入 | 低（当前文案安全，约束未来） |

### 1.3 内部 marker（网关内部协议，11 个 + 1 头）

| marker | 产生点 | 消费点 | 状态 |
|---|---|---|---|
| `retry_after_secs=N` | token_manager.rs:5085/5094/5101/5107/5128、handlers.rs:2282 | map_provider_error :1954、absorb_class_of :1725（parse_retry_after_secs 单一实现 handlers.rs:1623-1629） | 结构化提取，最稳 |
| `shared_budget_exhausted=1` | provider.rs:2655（MCP）/4409（主路径） | map :1802、websearch.rs:1121/1702 | 裸字面量 3 份 |
| `absorb_budget_exhausted=1` | provider.rs:4437 | map :1846 | **const**（ABSORB_BUDGET_EXHAUSTED_MARKER handlers.rs:1765）✓ |
| `inbound_admission_timeout=1` | handlers.rs:2282 | map :1896、absorb :1703 | 裸字面量 2 份 |
| `upstream_gate_full=1` | provider.rs:3110 | map :1926、absorb :1722 | 裸字面量 2 份 |
| `model_unsupported_by_pool=1` | token_manager.rs:5121 | map :1987、absorb :1706 | 裸字面量 2 份 |
| `subscription_unsupported=1` | provider.rs:2459/3394 | translate :1172 | 裸字面量 |
| `quota_exhausted_all=1` | provider.rs:3528 | translate :1202 | 裸字面量 |
| `pool_permanently_exhausted=1` | token_manager.rs:5085/5107/5203 | absorb :1714 | 裸字面量 |
| `consecutive_pool_unavailable=N` | token_manager.rs:5094 | **无消费点**（纯诊断串） | 死协议面 |
| `bearer_invalid_transient=1` | provider.rs:3828 | handlers.rs:1064/1142 | **const**（BEARER_INVALID_TRANSIENT_MARKER）✓ |
| `x-kirostudio-compress-retry: 1`（头） | map_provider_error :2134-2138 | 压缩重试循环 :2731/:4280、websearch :1548；出口前摘除（:2741/:4290/handlers.rs:585-591/websearch.rs:1557） | 响应头当信号通道，有漏摘事故史（注释自承） |

**脆弱性（MAJOR）**：见 §2。

### 1.4 幽灵承重串：`等容量`（本报告最重要的发现）

**线上实测（2026-08-15）**：`ssh skiapi 'grep -n "等容量" /opt/skiapi/services/kiro_shield.py'`
只在 **337 行注释**出现（「准入超时是"等容量"信号而非"请求非法"」），
**不在 COOLING_MARKERS 判据里**。COOLING_MARKERS 实测只有 3 项英文串
（`temporarily cooling down` / `All credentials are temporarily` / `inbound rate shaping`）。

但本仓三层代码/文档全部基于「`等容量` 是 shield 判据」的错误前提：

| 层 | 位置 | 现状 |
|---|---|---|
| 守卫测试 | handlers.rs `shield_cooling_markers_stay_in_production_text`（原 `absorb_503_body_must_carry_shield_cooling_marker`，2026-08-15 已按本报告修正） | 原断言 A1/A2 的 503 文案必须含「等容量」，注释声称「shield COOLING_MARKERS 里我们实际使用的那个词（2026-08-11 线上实读核对）」——**核对错误**；已改为钉真实判据串（A5 双哨兵 + A3 背压哨兵） |
| 配置校验 | src/model/error_messages.rs:502 `check_load_bearing_message` | 把「等容量」列为承重串，配置化保存时 warn「kiro_shield COOLING_MARKERS 分类判据」 |
| 文档 | docs/error-codes-inventory.md §3.1 | 「等容量 = kiro_shield COOLING_MARKERS（按 body 文案分类）」 |

**实际行为**：A1/A2 的 503 不命中 shield 任何 marker → classify 兜底 → `retry`（指数退避，**也读 Retry-After**）。
也就是说：文案里删掉「等容量」，shield 行为**完全不变**。它是「虚惊型承重串」：
- 正向风险为零（删了没事）；
- 反向风险真实存在——**真正的承重词**（A5 英文句、`inbound rate shaping`）万一被改，
  check_load_bearing 只拦得住 A5（有词条），拦不住 A3 的 `inbound rate shaping` 被替换
  （A3 文案里 `gateway-side backpressure` 还在，校验就放行，但 shield 判据 `inbound rate shaping` 已失配）。
  这就是守卫盲区：**校验词表 ≠ shield 真实判据词表**。

**修法（MINOR，认知纠错）**：
1. 守卫测试注释改写（去掉「shield COOLING_MARKERS 词」的错误理由，改为「锁 A1/A2 文案形态」或直接放宽）；守卫断言是否保留取决于「等容量」是否还有保留价值（纯文案，可自由改）。
2. `check_load_bearing_message` 词表修正：删「等容量」，补 shield 真实判据词 `inbound rate shaping` 与 `temporarily cooling down`（A5 的 `all credentials...` 词条保留但描述改为实测判据）。
3. inventory §3.1 表格修正。
4. 顺带补：shield 的 PERMANENT 中文判据（`请求体解析失败` / `凭据不支持刷新` / `不支持刷新 Token`）也应收进「改文案先查仓外消费者」的清单（CLAUDE.md:609 纪律的落地）。

---

## 2. marker 与 message 的耦合

### 2.1 现状（已核实，方向正确）

- **marker 不会到客户端**：map_provider_error 全部 12 分支的 message 来自 `resolve_msg`
  （静态默认文案或配置表），**从不插值 err_str**。A5 分支是范例：`retry_after_secs=N` 结构化提取后
  只进 Retry-After 头，message 是固定英文句（handlers.rs:1963/:1974）。C5 竞态兜底落 A12，
  原文只进服务端 tracing（:2199-2202）。
- **例外路径**：E5 透传（上游 status + 原始字节逐字节回客户端，passthrough.rs:730-752，网关零构造——
  若上游 body 恰好含 `retry_after_secs=` 字样，客户端能看到，但不进 map_provider_error，不会误判）；
  透传本地错误 E1-E4/E8/E9 把 reqwest 错误文本拼进 message（唯一「错误文本进 message」的路径，无 marker）。
- `x-kirostudio-compress-retry` 头：**客户端不可见**（所有运行时出口都在摘除之后，守卫测试
  handlers.rs:6650-6669 钉死），脆弱点在于「新增 map_provider_error 直接调用点而漏 strip」的
  事故形态（:2741-2742、:585-588 注释自承）。

### 2.2 脆弱性（MAJOR）

载体是「字符串内嵌 + 子串提取」，四个弱点：

1. **同一字面量多处裸写**：`inbound_admission_timeout=1`/`upstream_gate_full=1`/`model_unsupported_by_pool=1`
   在 map 与 absorb_class_of 各写一份（handlers.rs:1703/1722/1706 vs :1896/1926/1987），
   `shared_budget_exhausted=1` 在 websearch.rs 再写一份（:1121/:1702）。改一处漏一处 → 分类分叉，
   **编译不报错**（:3825-3826 注释自承此类事故）。已 const 化的只有 2 个（ABSORB_BUDGET_EXHAUSTED_MARKER、BEARER_INVALID_TRANSIENT_MARKER）。
2. **子串提取误判**：`retry_after_secs=N` 是 contains+split 数字前缀（handlers.rs:1623-1629）。
   主路径错误串**内嵌上游 body 原文**（provider.rs:3476 `"{api_type} API 请求失败: {status} {body}"`），
   上游 body 若含 `retry_after_secs=数字` 字样即静默误判为 PoolCooldown。
   同类误判刚修过（`is_upstream_transient_5xx` 524 宽判据误吞 4xx，handlers.rs:1515-1518）——「body 子串碰巧命中」在本仓是已发生过的现实。
3. **marker 与文案混编**：同一串同时服务人（日志可读）与机器（marker 提取），
   信息面泄漏到日志/面板/透传路径；`consecutive_pool_unavailable=N` 更是无消费者的纯诊断串。
4. **分支顺序承重**：absorb_class_of 十条判据的顺序（:1702-1756）与 map_provider_error 的
   分支顺序（A1 必须第一、A3 在 A5 前）靠注释 + 守卫测试维持，无结构性保障。

### 2.3 设计判断与建议

- **不建议**把 marker 迁出 message 放进新响应头/字段——外部消费者（Claude Code、shield）的判据
  已建立在现有文案上，新增第三个判据面只会更乱。对外的正确姿势 = A5 现状（固定文案 + 标准
  Retry-After/status 契约），marker 只活在网关内部。
- **短期（MINOR，低风险）**：裸字面量全部收敛为 const（2 个先例照抄），产生侧经 const 引用、
  消费侧统一引用；`consecutive_pool_unavailable=N` 补消费点或降级为结构化字段；`retry_after_secs=2`
  从 bail 文案（provider.rs:3110）抽出来与 handlers.rs:1938 的 `unwrap_or(2)` 共用常量。
- **中期（设计待做）**：marker 语义迁出错误串——错误串保留给人读，另加结构化通道
  （错误枚举/内部字段），map_provider_error 按枚举分发而非子串匹配。会动热路径与守卫测试，
  需要服务器验证循环配合。

---

## 3. 子串匹配清单（错误分类判定）

输入面：handlers.rs 全部谓词的输入 = provider 错误 Display =
`"{api_type} API 请求失败: {status} {body}"`（provider.rs:3476）——**上游 body 原文透传其中**；
provider.rs:1943/1962 直接对上游原文 lower 匹配。除机器标记（`xxx=1`）外，
所有裸串判据都暴露在上游原文的误伤面上。

### 3.1 高风险（MAJOR，误伤面真实存在）

| 位置 | 匹配串 | 判定 | 误伤风险 |
|---|---|---|---|
| stream.rs:128-136 | `throttl` / `toomanyrequests` / `ratelimit` / `rate limit` / 裸 `429` / `overload` / `quota` / `exhaust` | 流内限流信号→429 | **前缀级超宽词族**：上游 code/message 任意含 `quota`/`exhaust`/`overload` 即判限流；裸 `429` 命中 requestId/时间戳 |
| token_manager.rs:1757-1768 | 裸 `400`/`401`/`403`/`404`/`410`/`422` + `invalid_grant` 等 | 刷新失败是否凭据级 | **裸数字六连**：err.to_string() 含 URL/端口（`:4000`）即误判，无注释自认 |
| provider.rs:1943-1952 | `too long` / `content_length_exceeds` / `usage limit` / `quota` / `insufficient balance` / `insufficient_quota` | 400/404 是否值得换号 | `quota` 宽词：body 任意含 quota 的 400 被判「换号无益」直返 |
| endpoint/mod.rs:391-429 | 8 串 + 裸 `suspended` 且非 `temporar` | 永久封禁 | 裸词判据，注释记录过 `locked` 裸词误禁事故 |
| handlers.rs:1309-1310 | `Invalid token` / `subscription` | 凭据失效→502 | `subscription` 是**宽词**，注释 :1302-1308 已为此收过一次 |
| handlers.rs:1231 | `MONTHLY_REQUEST_COUNT` / `QUOTA` | 单号配额兜底 | 上游原文裸串，`QUOTA` 任意命中即误判（注释自认降级保留） |

### 3.2 中风险（MINOR，有闸门/方向安全侧）

| 位置 | 匹配串 | 风险 |
|---|---|---|
| handlers.rs:1136-1153、region_probe.rs:139/189 | 裸 `401`/`403` + `认证失败`/`权限不足` | 裸数字 + 中文字串；requestId 含 `401` 漏判（注释 :1111-1115 自认，方向安全侧） |
| region_probe.rs:135 | 裸 `429` / `too many requests` / `throttling` | 裸数字，方向安全侧（少判死） |
| handlers.rs:1476-1488、:1534-1537、:1557、:1576、:1595 | 传输层词族（`dns`/`timeout`/`certificate`/`ssl`/`tls`/`proxy`） | 有 is_transport_error 闸门收窄，但词本身超宽 |
| endpoint/mod.rs:439-507 | `temporarily is suspended` / `temporarily suspended` / `temporarily_suspended` 三形态 + `suspicious activity` + `unusual`∧`activity` + TEMPORARY_SIGNALS 7 串 | 上游措辞每多/少一个词就漏判；注释 :453 记录过整池 429 风暴事故 |
| endpoint/mod.rs:351 | `The bearer token included in the request is invalid` 整句（**不 lower**） | 大小写/标点变化即漏判（刻意窄） |
| provider.rs:2149/2938 | `retry_after_secs=` 或 **`冷却`（中文）** | 中文字串判据：`冷却` 出现在其他文案即误归限流度量桶（仅影响度量） |

### 3.3 低风险（稳定/机器标记/已修）

- 机器标记 `xxx=1` 系列（handlers.rs:1172/1202/1703-1722/1802/1846/1896/1926/1987、websearch.rs:1121/1702）：网关自创词，稳。
- `invalid_grant`（token_manager.rs:374/535/642）：**同一判据复制三份**（Social/External/IdC 刷新路径），形态固定，风险在复制不同步。
- `is_upstream_transient_5xx`（handlers.rs:1506-1521）：W5 已修（不裸匹配数字、`: 524` + `524 a timeout occurred` 连续形态 + 反例测试）；`internalserverexception` 依赖无空格形态（漏判面，低）。
- 大小写敏感残留：`Too many requests`（:1016）、`Invalid token`（:1309）、`Input is too long`（:1448）——上游变体漏判落兜底（行为偏安全侧）。
- endpoint 侧 JSON 精确比对（`/error/reason` 字段）与裸串兜底双通道：较稳。

### 3.4 共性修法建议

- 裸数字判据（`429`/`401`/`403`/`400` 等）一律改为「结构化字段 + 状态码」判定（JSON 解析 error 结构
  或按 status 而非 body 子串），或至少注释显式声明「已知误伤面，方向安全侧」。
- 宽词（`quota`/`subscription`/`suspended`/`冷却`）改窄：加限定上下文（如 status 组合）或精确短语。
- 中文判据（`认证失败`/`权限不足`/`冷却`/`API Key 凭据不支持刷新`）全部枚举化（本仓已有 CooldownReason 枚举先例）。

---

## 4. magic number 清单（抽 20 项）

| # | 位置 | 值 | 常量名 | 重复情况 | 风险 |
|---|---|---|---|---|---|
| 1 | handlers.rs:1002；cooldown.rs:177；handlers.rs:1772 | 8s | `UPSTREAM_RATE_LIMIT_RETRY_AFTER_SECS` / `RATE_LIMIT_FLOOR_SECS` / 派生 `ABSORB_EXHAUSTED_RETRY_AFTER_SECS` | 3 处，前两处已联动，cooldown 的 8 未联动 | 中 |
| 2 | handlers.rs:1058；cooldown.rs:111/119；token_manager.rs:2259；provider.rs:184/234 | 20s | 有（语义各异） | **5 处**，「账户级风控 20s」散 4 文件 | **高** |
| 3 | handlers.rs:2886；:1272 | 3s | `EMPTY_RESPONSE_RETRY_AFTER_SECS` / 字面量 `Some(3)` | 2 处，注释说同档但未引用 | 中 |
| 4 | provider.rs:3110（**文案内嵌**）；handlers.rs:1938 | 2s | **无**（`retry_after_secs=2` 埋在 bail 串里） | 2 处 | **高**：改文案忘改数字 |
| 5 | handlers.rs:1909 | 1s | 无（`.unwrap_or(1)`） | 1 处 | 中 |
| 6 | provider.rs:147；config.rs:1241-1243 | 45s | `MAX_REQUEST_RETRY_BUDGET_SECS` / `default_absorb_budget_secs()` | 2 处，注释声明同源未引用 | 中 |
| 7 | passthrough.rs:176；handlers.rs:2636/4164；http_client.rs:793 | 90s | `FIRST_BYTE_TIMEOUT_SECS` / `MAX_COMPRESS_RETRY_BUDGET_SECS`(×2) / `pool_idle_timeout` | 4 处，handlers 内部已复制两份 | 中高 |
| 8 | provider.rs:154；http_client.rs:985 | 720s | `MCP_CLIENT_READ_TIMEOUT_SECS` / **裸字面量** `read_timeout(720)` | 2 处 | **高**：http_client 未引用常量 |
| 9 | provider.rs:167；:1478-1479 | 1470s / 210s | `MCP_WALL_SECS`=720×2+30 / `PASSTHROUGH_WALL_SECS`=90×2+30 | `×2+30` 公式两处各写一遍 | 中 |
| 10 | affinity.rs:37；token_manager.rs:1062；:2307；cooldown.rs:625；health.rs:78 | 1800s | `ttl` / `MODEL_BLACKLIST_TTL_SECS` / `MODEL_BLOCK_TTL` / `SUSPICIOUS_MAX_SECS` / `MAX_OPEN_SECS` | 5 处，**token_manager 同文件两个 1800** | **高**：同文件同值异名 |
| 11 | token.rs:161；cooldown.rs:112；token_manager.rs:1057；config.rs:1328；security.rs:228；update.rs:366；scheduling.rs:85 | 60s | 7 个命名/字面量 | 7 处（TTL/刷新锁/自愈/限流窗等异义） | 中 |
| 12 | version_mask.rs:35；token_manager.rs:1053；main.rs:484/1158 | 12h / 6h | `REFRESH_INTERVAL` / `REPROBE_ALL_BAD_COOLDOWN` / 裸字面量 | 4 处 | 中 |
| 13 | cooldown.rs:127-129；rate_limiter.rs:111/158/207/270 | 86400s / 3600s | match 字面量 / `from_secs(86400)`×3 / Suspended=>3600 | 6 处 | **高**：同为「封禁退避」，Suspended 1h vs AccountSuspended 24h 两套时长 |
| 14 | main.rs:182；config.example.json:7；deploy/*.sh ×6 | 8990 | 无 | **≥8 处** | 中：改端口要动脚本+模板 |
| 15 | external_idp_login.rs:41；social.rs:90 | 3128 / 8008 | `SOCIAL_REDIRECT_URI`（OAuth 契约值） | 2 处 | 低（上游硬锁，不应动） |
| 16 | provider.rs:47 | 4 | `ABSOLUTE_MAX_TOTAL_RETRIES`（曾 64） | 1 处 | 低 |
| 17 | config.rs:1241-1273；provider.rs:189 | 45s/3 轮/150ms/15s/503/50ms | 全部有函数名 | 一组 | 中：150ms 与 50ms 易混 |
| 18 | cooldown.rs:107-129 | 15/20/60/20/30/300/86400×3 | **整个冷却基线表 9 个裸字面量** | 9 个无常量名 | **高** |
| 19 | config.rs:1350-1362；rate_limiter.rs:13-31 | RPM 100/20/300、burst 2s、queue 30s、daily 500 | 全部有函数名 | 各自 1 处 | 低（已命名） |
| 20 | token_manager.rs:2206；websearch.rs:39；handlers.rs:2630/4163；websearch.rs:1536 | 6 / 5 / 3×3 | `MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE` / `MAX_WEB_SEARCH_ROUNDS` / `MAX_COMPRESS_RETRIES` | `MAX_COMPRESS_RETRIES=3` 复制三处 | 中 |

**勘误**（与用户背景案例的出入，以实测为准）：`MAX_PEEK_BYTES 4096` 在 src/ 不存在
（shield 侧是 4096，Rust 侧最近似 upstream_trace.rs:53 `CHANNEL_CAPACITY=4096`）；
「IP 日窗 200」不存在（每-IP 限流是 60s 窗口，security.rs:196-251；每日窗是 rate_limiter.rs:13
`DEFAULT_DAILY_MAX_REQUESTS=500`）；端口 8993/52535 不在 src/（52535 是 nbus ssh 端口，8787/8788 仅注释/测试）。

**最值得抽常量的 5 个**：
1. cooldown.rs:107-129 冷却时长表（9 个裸字面量，且 20s 与 handlers.rs:1058 语义绑定）
2. token_manager.rs:1062 `MODEL_BLACKLIST_TTL_SECS` 与 :2307 `MODEL_BLOCK_TTL` 合并（同文件同值 1800s 同语义）
3. provider.rs:3110 文案内嵌的 `retry_after_secs=2` 抽出，与 handlers.rs:1938 共用
4. http_client.rs:985 裸 `read_timeout(720)` 引用 `MCP_CLIENT_READ_TIMEOUT_SECS`
5. 45s 双源 + `×2+30` 墙钟公式抽派生函数 `wall_secs(read_timeout)`

---

## 5. 语言耦合（前端拿后端中文文案做判定）

### 5.1 判定点全集：5 处，全部 MAJOR

| # | 位置 | 判定 | 消费字段 | 风险 | 设计文档覆盖 |
|---|---|---|---|---|---|
| 1 | credential-card.tsx:228 | `=== '速率限制'` | 列表 `cooldownReason` | 改文案→冷却 pill 全走红分支 | ✓ 已列 |
| 2 | credential-row.tsx:255 | `=== '速率限制'` | 同上 | 状态点/DetailItem tone 全走 red/bad | ✓ 已列 |
| 3 | overview-page.tsx:84 | `includes('可疑')` | **insights** `CooldownDetail.reason`（第二条下发路径） | 风险等级从 suspicious 掉 generic | ✓ 已列 |
| 4 | hooks/use-pool-notifications.ts:208 | `includes('可疑活动')` | insights `CooldownDetail.reason`（与 #3 同字段同出口） | 可疑风控 toast **静默不再弹**（最痛点功能） | ✗ **遗漏** |
| 5 | settings-page.tsx:1566 | `msg.includes('重复')` | restore 端点错误 message（后端 `refreshToken 与其他凭据重复` service.rs:1312 / `凭据已存在（kiroApiKey 重复）` :3042） | 强制恢复自动重试静默失效 | ✗ **遗漏（范围外）** |

设计文档「除 3 处判定 + 2 处展示外无 cooldown_reason 消费」的结论**不成立**：
#4 与 #3 同字段同出口，漏改则「可疑活动」通知仍耦合（文档改造后仍是语言耦合残留）；
#5 是跨主题新类别（错误消息文案判定），且现成抓手已有——`lib/utils.ts parseError`
已解析出后端 `error.type`（结构化，error.rs:80-85）但**只存不消费**，settings-page 本可用它。

### 5.2 展示层缺口（en/ja 界面显示后端中文，非判定）

- `StoragePartition.label`（types/api.ts:957 注释自认「展示名（中文）」）、`SuccessResponse.message`、
  `BatchDeleteItemResult.error`、`RequestRecord.error_message`、clone 管理 toast 的 `r.message`
  等后端中文直显点（文档仅登记了 cooldownReason/insightText，且 insightText 行号记偏）。
- `CleanupSkippedItem.reason` 是英文稳定码（service.rs:139-147）但未走 i18n，toast 裸拼英文码。
- 英文枚举判定（`disabledReason`、`RequestOutcome`、`CleanupSkippedItem.reason` 动态 key）是**正确模式**，照抄即可。

### 5.3 修法（沿用 cooldown-reason-i18n-design.md 方案，补 2 处）

1. 设计稿 2.3 的 `lib/cooldown.ts` helper 的 `isSuspiciousCooldown` **同时接到**
   use-pool-notifications.ts:208（两处 suspicious 判定共用，防第二次遗漏）。
2. settings-page.tsx:1566 改消费 `error.type`（后端补结构化 code 或复用现有 type），与文案脱钩。
3. 展示缺口按「后端枚举/稳定码 + 前端 i18n key」模式补（StoragePartition 需要后端加 code 字段，
   单独排期）。

---

## 6. 文案与逻辑耦合的架构建议（i18n 双轨）

用户提过「本地化完美」目标。错误文案要支持 i18n，**判据串必须独立于展示文案**，双轨：

### 6.1 三条轨道的身份（先分清，再谈配置）

| 轨道 | 例子 | 能否 i18n / 可配 | 机制 |
|---|---|---|---|
| **A. 内部 marker**（网关内组件间协议） | `retry_after_secs=N`、`*_exhausted=1` | **永不 i18n、永不进配置**（配置键名而非值） | 短期收敛 const，中期结构化字段（§2.3） |
| **B. 外部判据哨兵**（客户端/外挂的机器判据） | A5 英文句、`prompt is too long`、`inbound rate shaping`、shield PERMANENT 中文词 | **哨兵本体永不 i18n**；人读部分可配但哨兵子串必须保留 | 单一事实源：`check_load_bearing_message` 词表 = 实测哨兵全集（修正后），配置校验命中即拦/告警（现状已实现：命中只告警不硬拒，管理员显式要改仍允许——这是对的取舍） |
| **C. 纯展示文案** | 中文排障步骤、面板指引 | **自由 i18n/可配** | 配置系统（error-codes-config 已上线）+ i18n 表 |

### 6.2 落地原则

1. **哨兵串单点定义**：B 轨哨兵全部收进一个模块（如 error_messages.rs 的常量表），
   `check_load_bearing_message` 从这张表生成（而不是手写词表）——消除「校验词表 ≠ 真实判据」的守卫盲区（§1.4）。
2. **仓外判据进文档 + 守卫**：shield 判据（skiapi）是仓外事实源，改 A5/A3 类文案前必须 grep 仓外
   （CLAUDE.md:609 纪律）；把「shield 判据词表」做成仓库内常量注释 + 守卫锚点（现状只锚了 `等容量` 一处，
   修正后应锚真判据词）。
3. **i18n 化顺序**：先 C 轨（展示文案），再碰 B 轨（哨兵保留子串、人读部分翻译），A 轨不碰。
   B 轨 i18n 的形态 = 文案模板参数化（哨兵子串为模板常量，翻译只动其余部分）。
4. **新文案准入检查**：任何新错误文案自动过两道闸——`check_load_bearing`（是否误伤哨兵）+
   H7 决策词黑名单（客户端行为文档 §6.1 的约束表，前端/后端共用一张表）。

---

## 7. 最值得先动的 3 个协议绊脚石

### 1. 幽灵承重串纠错 + 校验词表对齐实测（MINOR，认知纠错，半天）

- **内容**：修正 handlers.rs:6008 守卫注释、`check_load_bearing_message` 词表（删 `等容量`，
  补 `inbound rate shaping`/`temporarily cooling down`）、inventory §3.1。
- **为什么先动**：错误码配置系统已上线（W8），`等容量` 词条正在**错误地限制**管理员改 A1/A2 文案，
  而真正的 shield 判据词 `inbound rate shaping` 却在**错误地放行**（改掉它配置校验不拦）。方向性错误，
  越晚修越多人被误导。改动是文档+注释+词表，零逻辑风险。

### 2. 前端语言耦合 5 处判定改枚举（MAJOR，已有设计稿，半天~1 天）

- **内容**：按 cooldown-reason-i18n-design.md 执行（后端 `code()` + 双出口加 `cooldownCode`），
  **必须补 2 处遗漏**：use-pool-notifications.ts:208（同字段同出口，漏改则通知功能仍耦合）、
  settings-page.tsx:1566（改消费 `error.type`）。
- **为什么先动**：ISSUES (c) MAJOR 已立项、设计稿现成、工作量小；遗漏点正是「设计文档说无其他消费」
  的结论被实证推翻的坑——不修报告，改造做一半。

### 3. marker 字面量收敛 const + 数字出文案（MINOR，防静默漂移，半天）

- **内容**：`inbound_admission_timeout=1`/`upstream_gate_full=1`/`model_unsupported_by_pool=1`/
  `shared_budget_exhausted=1` 收敛为 const（2 个先例照抄）；`retry_after_secs=2` 从 bail 文案抽出
  与 `unwrap_or(2)` 共用常量；`consecutive_pool_unavailable=N` 补消费点或降级。
- **为什么先动**：注释自承此类事故已踩过（:3825-3826），编译不报错、只靠守卫测试兜。
  改动是纯重构 + 守卫跑 CI 验证，风险低，为后续「marker 结构化迁移」铺路。

---

## 8. 证据与可证伪性

- **shield 判据**：`ssh skiapi 'grep -n "COOLING_MARKERS\|PERMANENT_BODY_MARKERS\|等容量" /opt/skiapi/services/kiro_shield.py'`
  —— COOLING_MARKERS 仅 3 项英文；`等容量` 仅 337 行注释；classify() 兜底 503→retry、403→auth、其余 4xx→pass；
  `backoff_delay`/`cool_delay` 均读 Retry-After（:767-775/:650-668）。
- **marker 不进 message**：读 map_provider_error（handlers.rs:1775-2218）全分支 message 均来自 `resolve_msg`。
- **前端 5 处判定**：读 admin-ui 各文件行号（工作树 2026-08-15 核对，与设计文档一致 + 2 处新增）。
- **子串/magic number 行号**：3 路探查均逐段读码核实，行号以工作树为准（工作树大量未提交，行号
  会随波次漂移，改代码前以当下为准重核）。
- **本报告修正的既有断言**：inventory §3.1「等容量=shield COOLING_MARKERS」→ 实测不成立；
  cooldown-reason-i18n-design.md「除 3 处判定外无 cooldown_reason 消费」→ 遗漏 2 处；
  用户背景案例「MAX_PEEK_BYTES 4096 / IP 日窗 200」→ src/ 不存在对应物。
