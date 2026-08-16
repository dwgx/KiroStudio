# 深度推理会话体系研究：会话亲和 / 粘滞 / 标识

> 2026-08-16 调度研究员产出。范围：conversationId 的提取与操纵面、UserAffinityManager 亲和机制、
> usage 会话维度统计（by_session / by_client / by_machine）、垃圾/低质量会话 id、根治方案。
> 代码基线：工作树 HEAD 1e100a2（W13 未提交改动之上，行号以读码时刻为准）。
> 对照：docs/ref-kiro2cc-proxy.md（sticky cache 已评估无需移植）、docs/ref-ZyphrZero-kiro.rs.md（OpenAI 会话标识）。

## 1. 会话标识来源链路

### 1.1 conversationId 的三级回落（converter.rs:1046-1058）

```
客户端 metadata.user_id（Anthropic metadata 自由 JSON，客户端完全可控）
  └─ L1: extract_session_id(user_id)              converter.rs:857-881
       ├─ JSON 格式: {"device_id":...,"account_uuid":...,"session_id":"UUID"}
       └─ 字符串格式: "user_xxx_account__session_0b4445e1-..."
       校验 is_valid_uuid: 仅形状校验（36 字符 + 4 个连字符）  converter.rs:884-886
  └─ L2: derive_conversation_id_from_context(req)  converter.rs:930-982
       SHA256("derived-conversation:" + canonicalize_system_text(system) + 排序工具名)
       → UUID 形状；system 与 tools 双双为空 → None
  └─ L3: Uuid::new_v4() 随机                            converter.rs:1058

agentContinuationId = SHA256(conversationId) 确定性派生         converter.rs:830-854, 1066
```

- 客户端路径：Claude Code 发 `metadata.user_id`（内含 session UUID）；python/curl/opencode 不发
  → 落到 L2/L3（08-03 全天 38.8% 的请求是这类，见 converter.rs:896-898 实测注释）。
- L2 用 system + 排序工具名做键：同一工作上下文的连续请求稳定，拿到上游 prefix 缓存收益；
  代价是**跨用户撞键**（见 §5.5）。
- L3 随机 UUID：每请求一个全新会话键 → 亲和永不命中、by_session 全是垃圾键（见 §5.1）。

### 1.2 各消费路径拿到的会话键（存在口径分裂）

| 路径 | 会话键 | 证据 |
|---|---|---|
| Kiro 主路径（选号/亲和） | conversationId（converter 提取或派生后） | provider.rs:4527-4530 从请求体 conversationState.conversationId 提取 → acquire_context(:4357) |
| Kiro 主路径（usage 埋点） | 同上（CallMeta.session_id） | provider.rs:3350/:4397 → handlers.rs:3023 |
| custom_api 透传路径（usage 埋点） | **原始 user_id 字符串**（未提取、未派生） | provider.rs:1914/:2023 → handlers.rs:2737 |
| 入站整形超时（usage 埋点） | 固定字面量 `"admission-timeout"` | handlers.rs:2592 |
| OpenAI /cc 路径 | 无会话概念（session_id 恒 None） | openai 目录无 session 引用；by_session 不统计 |

**结论：同一真实会话横跨 Kiro 与透传两条路径时，usage 的 by_session 会拆成两个 key**
（一个是被提取后的 UUID，一个是完整 `user_xxx_account__session_UUID`）。这是会话维度
统计失真的第一实锤，见 §5.2。

### 1.3 客户端操纵面

- `metadata.user_id` 客户端可控，只过形状校验（36 字符 + 4 连字符，converter.rs:884-886）。
  36 字符任意串（非 hex 也能过）即可在以下三处制造任意键：
  1. affinity map 的 key（30 分钟 TTL + 5 分钟清理，有界，低危）；
  2. usage by_session 的 key（10 分钟窗口 + 5 分钟清理，有界，低危）；
  3. **traces.db 的 session_id 列（永久保留到 retention_days）** —— 无界污染 + 无法按会话回溯。
- 操纵用途分析：客户端看不到号 id，无法刻意把会话粘到特定号；但可以用噪声 user_id
  制造大量唯一会话键，稀释 by_session/active_sessions 的审计价值。威胁等级：低-中。
- zyphr 对照：参考仓 openai 路径做「四来源提取 UUID」（prompt_cache_key →
  x-session-affinity → x-client-request-id → session_id，ref-ZyphrZero-kiro.rs.md:17），
  我们 anthropic 路径只认 metadata.user_id、openai 路径完全免疫会话头 —— 比 zyphr 保守，
  不存在头部伪造面，代价是 openai 路径零会话收益（无亲和、无 by_session）。

## 2. 亲和机制现状

### 2.1 UserAffinityManager（src/kiro/affinity.rs，全 94 行）

- 结构：`Mutex<HashMap<String, AffinityEntry{credential_id, last_used: Instant}>>`
- **TTL 30 分钟硬编码**（affinity.rs:37），config 只有开关 `affinity_enabled`（config.rs:639-640）。
- 惰性清理（get 过期即删）+ 后台 5 分钟 retain（main.rs:526-541）。
- 凭据禁用/删除时 `remove_by_credential` 全量解绑（token_manager.rs:6106/:6312/:6535/:6691/:6824/:6883/:7075/:8629，8 处）。

### 2.2 接线语义（token_manager.rs:3646-3693）

- 亲和命中是一条 `return` 旁路（跳过全部排序键），前提三件套：
  1. `!excluded.contains(id)`（本请求排除集也生效，排除即「本跳临时解绑」）；
  2. 绑定号仍在 `available`（可选）集合内；
  3. **`is_sticky_reuse_healthy`（:4514-4533）——完整语义**：
     - RPM 未饱和：`rpm.count(id) >= effective_saturation_limit` 才饱和。阈值 =
       per-cred `rpm_limit`(>0) > 全局 `credential_rpm_limit`(>0) > **兜底 30**
       （token_manager.rs:4444-4452），再乘 headroom 因子扣 reserve 名额（:4458-4468）。
     - 熔断未 Open；半开期按 `admit_prob` 概率放行（给恢复留通路）。
- 饱和/熔断/不可用 → 落回 `effective_scheduling` 归一化后的 balanced 排序键分流。
- **选中后无条件 `affinity.set` 重绑**（token_manager.rs:4079-4083）——包括刚才因饱和
  解绑的那个会话：直接绑到新选中的号。

### 2.3 亲和 vs 平摊的冲突（剩余项）

已修的部分（历史上有真实事故，注释有完整前因后果）：
- 亲和命中不查熔断 → 熔断 Open 的号被死粘（现由 is_sticky_reuse_healthy 封堵，:3663-3668）；
- priority 模式下亲和解绑后重选又选回同一饱和号 → 解绑白做活锁（现由调度归一化封堵，:3675-3677）；
- 默认无饱和阈值时 affinity 死粘单号打爆（现由 SATURATION_FALLBACK_RPM=30 兜底 + headroom 折扣封堵，:4430-4433）。

**剩余冲突：重绑无滞后**（token_manager.rs:4081）。绑定号在饱和阈值附近波动时：
- 请求 N 命中亲和 → 复用；
- 请求 N+1 判定饱和 → 解绑 → balanced 选空闲号 → **无条件重绑到新号**；
- 请求 N+2 起粘新号；旧号恢复后不再粘回（亲和表已改写）。
净效果：高频单会话在阈值附近的号之间反复横跳，每跳都打断上游 prefix 缓存，
「粘住省 token」的收益在饱和边缘被清空。判据（阈值、headroom）全是常量，不可调参。

### 2.4 亲和与 RPM 的交互

- 设计意图正确：单会话高频顶到 headroom 阈值（如 25 而非硬限 30）就让路，防死粘打爆
  （token_manager.rs:3660-3662 注释）。headroom 折扣的 L3 语义已读（:4461-4468）。
- 但「让路」的实现是**解绑 + 立即重绑**（§2.3），不是「暂时不粘旧号、会话结束后再粘回」。
  注释声称的「会话下次仍可能粘回」（:3658-3659）与实际代码（4081 无条件 set）不符 ——
  注释漂移（见 §5.8）。
- 会话粘到高负载号的风险已被 is_sticky_reuse_healthy 封住（饱和即不复用），
  但存在一个不对称：**饱和判定是瞬时的**，一个 rpm=29/30（阈值 30）的号对会话来说
  「粘不稳」，而对 balanced 分流来说「还能接」——两个口径用同一个阈值，没有
  亲和专用余量（headroom 折扣作用在选定 base 之后，两处共用，见 :4439-4440）。

## 3. usage 会话维度

### 3.1 ClientAgg 结构（usage_stats.rs:449-467）

| map | key 来源 | 证据 |
|---|---|---|
| by_session | record.session_id 原样 | :521-525 |
| by_client | client_ip 优先 → device 兜底 → "unknown" | client_key_of :471-476 |
| by_machine | derive_machine_key：IP 主键 → device → "unknown" | :50-62, :489-493 |
| session_machine | session_id → machine_key 粘滞（**只锚真实 IP**，防 unknown 黑洞） | :544-609 |

- **IP 变化不拆分**：同一 session 换 IP（DHCP/漫游）靠 session_machine 粘滞仍归原机器
  （:552-562）；无 IP 请求绝不建立粘滞（:605-608），防止互不相干的缺 IP 机器并入
  "unknown" 黑洞（:480-485 有 2026-07-08 修错的完整前因后果）。
- 单一归属不变量：session 任一时刻只属一台机器/一个客户端，迁移时从旧组移除
  （:533-537, :592-600）——防 RPM 双计。
- **prune 保留策略**：`RATE_BUCKETS=20 × 30s = 10 分钟窗口`（usage_stats.rs:29-31, :613-643），
  查询时惰性 prune + 后台 5 分钟主动回收（main.rs:1260-1278）。`by_model` 不在此列
  （有独立的 MODEL_KEY_CAP 上限，:674-682）。

### 3.2 审计角度的问题

1. **长期会话统计失真**：by_session 是 10 分钟滚动 RPM 视图（SessionRpm 只有
   session_id + rpm 两个字段，:1006-1014），会话暂停 10 分钟以上再继续 = 新会话。
   这不是 bug（设计如此），但「活跃窗口数」不能当「会话数」审计——没有任何
   跨窗口的会话总量/用量维度（无 tokens 按会话聚合，只有 RPM 计数）。
2. **session_id 缺失/噪声无观测**：没有 session_id 缺失率、一次性率指标。
3. **透传双 key + 敏感信息**（§1.2/§5.2）：trace 里明文存完整 `user_xxx_account__session_UUID`，
   含 account_uuid 片段。
4. **admission-timeout 混入真实会话**（handlers.rs:2592）：所有入站超时聚合到同一个
   假 session，在 by_session 视图里以最高 RPM 占据榜首，污染「谁在用」的判断。

## 4. 对照参考（为什么 sticky 无需移植）

- kiro2cc 的 Sticky cache = agentContinuationId → 账号绑定 60min TTL
  （docs/ref-kiro2cc-proxy.md:24）。我们的 conversationId 已按会话恒定（L1/L2），
  agentContinuationId 由它确定性派生（converter.rs:830-854），亲和键与其等价，
  机制已具备（affinity_enabled，默认开）；「无需移植」结论与 W13 认知更新一致
  （docs/session-report-2026-08-15-16.md:68）。
- k2cc 是 60min TTL，我们是 30min —— 数值差异，无结构差异。

## 5. 垃圾/低质量清单（按严重度）

| # | 问题 | 证据 | 影响 |
|---|---|---|---|
| 5.1 | **L3 随机 UUID 兜底**：无 metadata 且 system+tools 空的请求每请求一个随机会话键 | converter.rs:1058（含 896-898 的 38.8% 实测注释） | by_session 一次性键堆积；traces.db 会话键永久不可回溯；亲和零收益（provider.rs:4505-4507 注释自认） |
| 5.2 | **透传路径 session_id = 原始 user_id**（未提取/未派生） | provider.rs:1914, 2023 | 与 Kiro 路径同会话双 key；account_uuid 明文入 trace；超长/非 UUID 键进 by_session 与 traces |
| 5.3 | **admission-timeout 假会话** | handlers.rs:2592 | by_session 榜首假象，审计「谁在用」被污染 |
| 5.4 | **is_valid_uuid 只校验形状**（36 字符 4 连字符，不校验 hex） | converter.rs:884-886 | 客户端任意 36 字符串可伪造会话键（操纵面见 §1.3） |
| 5.5 | **L2 派生键跨用户撞键**：system+tools 相同的不同用户共用一个 conversationId | converter.rs:920-929（注释自认安全的前提是「上游不按 continuationId 存历史」） | 异用户共享上游会话键 + 共享亲和绑定（A 用户高频可让 B 用户同号饱和让路）+ by_session 合并异用户；加盐需要给 convert_request 传租户标识，注释判定「不值得」——前提成立则安全，但该前提无自动化验证 |
| 5.6 | **亲和 TTL/饱和阈值硬编码**：TTL 30min（affinity.rs:37）、兜底 30（token_manager.rs:4445）、headroom 因子/余量 | 同上 | 运维不可调参；面板无亲和观测（命中率/解绑率） |
| 5.7 | **亲和重绑无滞后** | token_manager.rs:4081 | 饱和边缘高频会话号间横跳，prefix 缓存收益被清空（§2.3） |
| 5.8 | **注释漂移**：「会话下次仍可能粘回」与实际无条件重绑不符 | token_manager.rs:3658-3659 vs 4081 | 后续维护者按注释修代码会引入新行为 |
| 5.9 | **by_session 10 分钟窗口**：长期会话暂停后续接被拆成新会话 | usage_stats.rs:29-31, 613-616 | 活跃窗口数 ≠ 会话数；无会话级 tokens/用量维度 |
| 5.10 | **session_id 数据质量零观测**：缺失率/一次性率/非法形状率无指标 | 全仓无统计 | 无法量化 §5.1-5.4 的实际占比，修与不修都缺依据 |
| 5.11 | openai 路径零会话收益（非缺陷，对照项） | ref-ZyphrZero-kiro.rs.md:17 | 该路径无亲和/无 by_session；若要会话级审计需移植 zyphr 式四来源提取，P2 级 |

## 6. 根治方案表

> 排序按「问题严重度 × 修复性价比」。每条：问题 → 根治 → 风险 → 工作量。
> 工作量为估算（单人，按现有代码惯例，验证走 skiapi Docker 循环）。

### P1-1 透传路径 session_id 归一化（修 5.2）

- **问题**：透传埋点把原始 user_id 当 session_id，同会话双 key + 敏感信息入 trace。
- **根治**：把 converter 的 `extract_session_id`（converter.rs:857）提升为 pub，透传埋点
  落 record 前先提取（provider.rs:1914/:2023 处）；提取不到则 None（不再回落原始串）。
- **风险**：低。纯埋点口径修正；trace 过滤按旧 user_id 串的查询会失效（可接受的破坏）。
- **工作量**：0.5 人日（两处调用 + 单测）。

### P1-2 随机 UUID 兜底改为 None + usage 侧归桶（修 5.1）

- **问题**：L3 随机键让 usage 会话维度出现大量一次性键，traces 无法按会话回溯。
- **根治**：converter 的 L3 回落改为 `Option::None`（conversationId 仍需要
  ConversationState 字段，用随机 UUID 填充但**打标**）——更干净的做法是 converter
  返回 conversationId 同时带 `is_derived: bool`（L1 真会话 / L2 上下文键 / L3 随机），
  provider 提取 session_id 时把 L3 置 None；usage 的 by_session 对 None 归入
  `"(no-session)"` 单桶（对齐 MODEL_KEY_OTHER 先例，usage_stats.rs:682），总量守恒。
- **风险**：中。`is_derived` 标记要穿透 converter → request_body → provider 提取全链路
  （或者 provider 对非 UUID/空 conversationId 判 None）。affinity 对 None 天然跳过
  （user_id: Option，已有）。上游会话键仍是随机 UUID，不影响缓存语义。
- **工作量**：1-1.5 人日（标记穿透 + usage 归桶 + 测试）。

### P1-3 admission-timeout 归桶（修 5.3）

- **问题**：假会话在 by_session 榜首。
- **根治**：session_id 保持 None 或固定 `"(gate)"` 桶，与真实会话隔离；查询视图不展示。
- **风险**：低。纯埋点字段改动。
- **工作量**：0.25 人日。

### P1-4 session_id 数据质量观测（修 5.10）

- **问题**：缺失率/一次性率/非法形状率零指标。
- **根治**：在 ClientAgg 或 UsageStats 加轻量计数器：`session_none` / `session_once`
  （10 分钟窗口内只出现 1 次的键数）/ `session_invalid`（非 UUID 形状），暴露到
  admin 面板或日志（cleanup_client_stats 返回里已带 session 存活数，顺手扩展）。
- **风险**：无。
- **工作量**：0.5-1 人日（计数 + 端点字段 + 前端展示可选）。

### P2-1 亲和参数化 + 重绑滞后（修 5.6、5.7）

- **问题**：TTL/阈值常量不可调；饱和边缘会话横跳。
- **根治**：① `affinity_ttl_secs` 进 config（默认 1800，对齐 affinity.rs:37）；
  ② 亲和解绑记 `last_unbound`，`set` 时对刚解绑的 (session, credential) 组合在
  滞后窗口（如 60s）内不重绑回旧号（若新选中的就是旧号，允许——那是全池只有
  它可用）；③ 加亲和命中率/解绑率观测。
- **风险**：中。②改动的是选号热路径的核心旁路逻辑（token_manager.rs:3646-3693 +
  4079-4083），需要并发测试护住（现有 affinity 测试在 :16423-16429 只有基本 get/set）。
- **工作量**：1.5-2 人日（config 接线 + 滞后表 + 测试）。若线上实测亲和收益可忽略，
  也可以只做①③，②降级为注释修正（见 5.8）。

### P2-2 会话 id 形状强化 + usage 侧上限（修 5.4）

- **问题**：形状校验弱，任意 36 字符串可伪造键；traces 无界污染。
- **根治**：① is_valid_uuid 加 hex 字符集校验（[0-9a-fA-F-]，converter.rs:884——注意
  L2 派生键是合法 UUID 形状，不受影响；客户端真实 UUID 也全 hex，无回归）；
  ② usage 侧对 session_id 长度/字符集做防御性过滤（超长截断或归桶，对齐
  MODEL_KEY_CAP 先例 usage_stats.rs:674-682）；③ traces.db 侧对超长 session_id
  截断（列是 TEXT，防超长串放大单条存储）。
- **风险**：低。①可能拒绝少数非标准客户端（无 hex 的 UUID 形状）——降级为不拒绝、
  仅打标观测即可。
- **工作量**：0.5 人日（①）+ 0.5 人日（②③）。

### P2-3 会话维度统计增强（修 5.9）

- **问题**：只有 10 分钟 RPM 窗口，无会话级 tokens/用量聚合。
- **根治**：SessionMeta 扩展（session 首见时间、累计请求/tokens/成功失败），
  prune 时随窗口清理（内存有界）；admin 增加「会话」视图（session_id、总量、
  活跃窗口时长）。或退一步：仅把 trace 查询的 session 过滤做强（已有
  trace_db.rs:44-45 session_id 精确匹配），靠 traces 反查会话，不动内存聚合。
- **风险**：中（内存聚合扩展涉及 on_record 热路径 + 前端新视图，改动面大）。
- **工作量**：全量 3-4 人日；退步方案（仅 trace 反查）1 人日。

### P3-1 L2 派生键加盐评估（修 5.5）

- **问题**：跨用户撞键，前提「上游不按 continuationId 存历史」无自动化验证。
- **根治**：给 convert_request 传租户标识加盐（converter.rs:927-929 注释自述代价：
  改全部调用点）；或降低优先级：在文档/守卫里钉死前提，待上游行为变化再改。
- **风险**：改造成本高、收益当前为零（注释判断成立）。**建议维持现状 + 文档钉死**。
- **工作量**：改动 1-2 人日；钉死 0.1 人日。

### P3-2 openai 路径会话标识（修 5.11，对照 zyphr）

- **问题**：openai 路径无会话概念。
- **根治**：zyphr 式四来源提取（prompt_cache_key → x-session-affinity →
  x-client-request-id → session_id，ref-ZyphrZero-kiro.rs.md:17），接入 affinity 与
  by_session。注意 zyphr 的顺序本身是「从最受控到最受控」——若移植，会话头在公网
  直连时不可信（XFF 伪造教训同源，handlers.rs:234-236 的 trust 逻辑）。
- **风险**：中。会话头伪造面是当前 openai 路径没有的；需按 trusted 原则决定
  是否只信 metadata.user_id。
- **工作量**：1.5-2 人日。非急迫（openai 路径当前流量小可确认）。

## 7. 自 review

- **范围核验**：任务清单四项（标识来源 / 亲和机制 / usage 会话维度 / 垃圾质量 +
  根治方案）全部覆盖；「is_sticky_reuse_healthy 条件钉住」读了完整实现
  （token_manager.rs:4514-4533）及其上游阈值链（:4444-4468）——不是只看表面布尔。
- **口径核验**：会话键双 key 问题（5.2）是本次最实的新发现，证据为
  provider.rs:1914/2023（透传原始 user_id）vs provider.rs:4527-4530（Kiro 提取后
  conversationId），两条路径埋点在同一文件内对读，可信。
- **已知限制**：
  - 未跑线上数据验证 5.1/5.2 的实际占比（需要 nbus traces.db 查询，本机无权直查；
    5.10 建议的观测指标正是为补齐这个依据）。
  - 5.6/5.7 的「横跳」是代码推演结论，无线上证据（需要 affinity 命中率观测）。
  - L2 派生键的实测数据在 converter.rs:896-898 注释（08-03 全天 38.8%），未独立复核。
- **方案一致性**：P1-2 与 P2-2 的「归桶/截断」对齐了 by_model 的 MODEL_KEY_CAP 先例
  （usage_stats.rs:674-682），不另造抽象。
- **不做的事**：不改 affinity 的核心旁路语义（P2-1 之外）；不移植 k2cc sticky
  （已确认等价）；不按 zyphr 给 openai 路径加会话头（P3-2 降级为可选项）。
