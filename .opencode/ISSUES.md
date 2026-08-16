# KiroStudio 问题总清单（a-e 五类，与 STATUS.md 同步）

> 更新：2026-08-16（W13 收尾同步：W11/W13 已完成项标记、websearch 结构性缺陷新增、DONE 闭环核对）。
> 权威状态入口：仓根 STATUS.md。历史档案不承载当前结论。
> 研究依据：docs/ref-ZyphrZero-kiro.rs.md、docs/ref-kiro2cc-proxy.md（8 路分析 + 3 路评审）。
> W2-W13 全部波次修复记录见 .opencode/state.md；会话全景见 docs/session-report-2026-08-15-16.md。

## (a) 我们以前有的问题（历史，含已解决背景）

| 问题 | 状态 |
|---|---|
| 透传选号错误：gpt 请求被 fallback 改写进 deepseek 链（交叉请求白付一跳） | 已修（严格语义 1e100a2） |
| 503 model_not_found 不冷却（每次重选撞同一坏号） | 已修（模型黑名单 1e100a2） |
| 中转站没配 allowed_models（None=不限）导致选错号 | 已修（线上配置钉死） |
| ksk 分身缺 region 恒 403（2026-08-05 实测 4 分身 TooManyFailures） | 已修（region 继承 + 探测） |
| absorb 层 14 个死配置旋钮 | 已修（throttleProfile 三档） |
| shield 放大链按配置推算 480× 实际 5.6× | 已修（认知纠正） |
| 压缩重试目标公式反向 | 已修（compress_retry_target） |
| 吸收循环吞 429 | 已修（429 上抛） |
| 透传层 DSML 标记泄漏 | 已修（三处修复 + 门控拆分） |
| Failed to parse JSON（gzip 链路） | 已修（accept-encoding 处理） |
| 1439 误选（白名单感知失效） | 已修（effective_model 带白名单） |
| 排序键注释/守卫漂移（13→12 键） | 已修 |
| 前端 i18n 插值 {{n}}→{n}（66 处） | 已修 |
| config 值记错（现读纪律） | 已修 |
| minutely.jsonl 断更 2 天无人发现 | 已修（度量纪律） |
| **自动禁用原因重启后变成手动禁用**（token_manager.rs:1572-1575 历史注释描述；2026-08-15 对比研究确认已修复：加载路径 :2406-2414 读回 disabled_reason，persist_credentials :5359-5365 全量写盘，7 条自动禁用路径全部落盘，DisabledReason 有 serde(other) 兜底） | 已修（评审验证）；附注：k2cc 的分层方案（credentials.json 只写 Manual + kiro_stats.json 写自动）**不需要移植**——我们单文件全量写已解决同问题且更简单 |

## (b) 已经修补的问题（2026-08-15 波次 2 全量，CI 1845/0 + 前端 48/0 + nbus 部署验证）

### 协议/正确性
- websearch 快路径非流式请求返回 SSE（协议违规）→ 改 JSON（M1，nbus 实测返回 JSON 验证）
- websearch 快路径零用量埋点 → 补 emit_websearch_fast_path_usage（M2）
- subscription_unsupported=1 误译 502 可重试+误导文案 → 404 not_found_error（M3）
- 非流式无空响应兜底 → 400/429 + 近空判据两路径共用 near_empty_response（M4 + review M2）
- thinking 双重计数回归（嗅探路径 1.8 倍虚高）→ 撤销冗余计数（review M1）
- 流式/非流式 thinking output_tokens 口径对齐
- flush_tool_input 失败路径提前累计 → 移到实际下发前
- 回灌路径非法 JSON 静默降级空对象 → repair_tool_json 修不好整轮报错
- emit_websearch_loop_usage 恒 is_streaming=false → 传 wants_stream
- 快路径 title 未 normalize_html_text → 对齐
- compressor 带 images 压空不修复 → 一律补占位符
- 非流式 thinking-only 缺占位块 → 补空格 text 块
- OpenAI chat id 用上游 msg_xxx → 恒自生成
- 非 2xx 错误体 gzip 字节被 lossy 破坏 → 原字节透传（M7）
- 透传 URL contains("/v1/") 过宽 → ends_with 收紧
- 透传池排序键缺 ramp_tier → 补齐（M11）

### 安全
- custom_api 透传 DNS rebinding 可绕过 SSRF → pinned_streaming_client 固化（M8）
- 告警 webhook 跟随重定向 / 构建失败 panic → redirect none + 降级
- export_credential 缺 no-store → 补
- usage 500 泄漏内部错误 → 通用文案
- external_idp leg1 state fail-open → fail-closed

### 健壮性/死代码
- MCP 路径无墙钟 → MCP_WALL_SECS = 720×2+30（M5）
- consecutive_passthrough_failures 死字段注释矛盾 → 统一「绝不自动禁用」+ 守卫
- refresh 裸子串重试判定 → 结构化错误
- 3 处 unwrap → ok_or_else；死字段/死函数清理
- count_tokens 300s → 10s；token 注释/round 修正
- IngressRateLimiter 全表 retain → 抽样；LRU 全表排序 → 阈值
- 观测补 anthropic-ratelimit-*；黑名单 TTL 注释对齐 30min
- 排序键注释 12 位重写 + ⑨⑩⑪ 守卫
- upstream_trace 953 行不在编译树 → 完整接线（M10）
- region 探测窗口先禁后探（M9）；eidp select 回显 arn/region
- perform_update 显式 tag 跳过 tags 拉取
- 前端：轮询链泄漏（PollGuard bump 含 countdown 路径）、快照覆盖修复、SSO/conn i18n、死组件删除、storage 优化
- docs：ARCHITECTURE 12 键、AuthTransient、压缩 4 层

## (c) 还有的问题（现存未修，按严重度；含 2026-08-15 对比研究结论）

### 后端
- **MAJOR**：web_search 工具判定 OR 过宽 —— **✅ 已修（2026-08-15 W5）**：改 AND（name && type starts_with），tool_choice 闸同步收紧，测试钉死（自定义同名工具/type-only 不触发）；converter 归一化补名修正为内置表 key "WebSearch"（原小写 key 错过内置 schema 的缺陷顺带修复）；type-only + 强制 tool_choice 的能力降级为有意决策（非官方形态，线上 0 流量）
- **MAJOR**：跨自然月配额无自动恢复 —— **✅ 已修（2026-08-15 W5）**：quota_exhausted_at 字段（serde camelCase 持久化）+ recover_expired_quota_disables（12h 月初缓冲覆盖偏西时区 + 缺失时间戳可恢复 + 幂等 + clear_transient_counters + 清冷却/限流 + 落盘）+ 懒触发（启动 + acquire_context 无候选，recovery_attempted 标志）。评审的「恢复前查余额缓存挡探针」**书面放弃**（balance_cache 在 AdminService 私有字段，token_manager 反向够不着；退化为幂等 + 402 再禁用兜底，注释 6431-6440 写明取舍）
- **MINOR**：input_schema HashMap（types.rs:248）——**动机修正**（评审验证）：转发链路已由 converter normalize_json_schema 兜底（serde_json 无 preserve_order，Value::Object=BTreeMap 恒字典序），**上游字节序抖动不成立**；真实收益 = token.rs:257 payload_hash 直接序列化 HashMap 的缓存确定性 + 类型收口。**待验证**：先查哪条转发路径不过 normalize_json_schema（OpenAI/透传路径），查不到收益点就砍
- MINOR：cache 指纹无持久化（重启后计费短暂退化）——8GB 约束下暂不做（zyphr CacheMeter 方案可参考）
- MINOR：upstream_trace DROPPED/WRITTEN 计数器无消费点
- MINOR：set_tool_compat_mapping 零调用半接线
- MINOR：count_tokens 口径 4.0 vs fuckopencode 4.5 待统一
- MINOR：token_manager 计数器清零不对称（consecutive_pool_unavailable）
- MINOR：cli_ua_align_real_client 半接线
- MINOR：A-5/A-6 低危项（429 换区 403 绕圈；select_endpoint 备区只取 PROBE_ORDER 第一项）
- MINOR：D 类阈值无真实故障分布数据
- MINOR：provider.rs DEBUG 级打印完整请求头风险自查
- MINOR：524 终态渲染缺口 —— **✅ 已修（2026-08-15 W5）**：is_upstream_transient_5xx 加 `: 524` 状态行形态 + `"524 a timeout occurred"` 连续形态（不裸匹配数字纪律），终态落 503+Retry-After；反例测试（524 数字+干扰词不误判）+ 三形态正向测试
- MINOR：update_refresh_token 路径缺截断检测 + 跨凭据重复检测 —— **✅ 已修（2026-08-15 W5）**：截断检测 + 跨凭据 sha256 去重（排除自身）+ api_key 类型闸 400 + 服务端 trim，5 个测试
- MINOR：token_manager.rs:5861 过时注释（「Single 对象格式 persist 是 no-op」与 :5339-5344 已修复实现矛盾）——评审附带发现 —— **✅ 已清理（W10-W12 波次，现为族级清零承重注释）**

### 前端
- **MAJOR**：cooldownReason 用后端中文文案做判定（'速率限制'/'可疑'）——语言耦合 —— **✅ 已修（2026-08-15 W11）**：后端 `CooldownReason::code()` 9 变体 snake_case + `cooldownCode` 字段下发；前端 lib/cooldown.ts 纯函数 helper（翻译注入不 import i18n）+ 9 key ×3 语 + 5 处判定改枚举（credential-card:228 / credential-row:255 / overview-page:84 / use-pool-notifications:208 / settings-page:1566）+ 2 处展示走 i18n label + 5 测试（admin-ui/tests/cooldown.test.ts）。重复类错误独立判别 `duplicate_credential`（DuplicateCredential 错误类型，W13 补 Display 文案分清凭据重复/无效）
- **MAJOR**：缺「更新 refreshToken」前端入口 —— **✅ 已修（2026-08-15 W5）**：api/credentials.ts updateRefreshToken + credential-card「更新 Token」按钮/对话框/InvalidRefreshToken 琥珀横幅引导 + useUpdateRefreshToken mutation + 三语 11 键。zyphr 源文件重载确认不适用我们架构（加密存储 + 服务器网关）
- MINOR：dashboard marquee 未滤 disabled（历史审计项）
- MINOR：modelMapping 非法 JSON 仍提交（历史审计项）
- MINOR：contextmenu 未 preventDefault（历史审计项）
- MINOR：指纹 creation 低估、system 动态头不跳等（审计 MINOR 全量见 archive）

### 配置/运维
- opencode 用户侧配置仍指向 k1ro.skiapi.dev（冷却中）——建议切 api.dwgx.top + deepseek-v4-pro（需用户 key）
- OTA token 未填；OTA 自动检查默认关（restart-only 语义已接线，W10-W12 #2）
- release 产物落后线上全部 W2-W13 修复——发 v1.1.2 决策待定

## (d) 还没做的问题（有意未做/待决策；含移植候选研究结论）

| 项 | 状态（2026-08-15 研究后） |
|---|---|
| **模型感知正向路由**（zyphr P0） | **做轻量版，改方案**（评审）：透传池三态缓存（Confirmed/Unknown/Unsupported）+ 黑名单负向兜底；数据源现成 fetch_upstream_models。补充：① Unsupported 带 TTL（30min-1h，否则目录变更后永久跳过）；② 预热限频（单号单飞、长周期、失败退避，绝不放选号热路径同步 fetch）；③ 空列表不写缓存；④ 全候选 Unsupported 退化放行；⑤ deepseek 归一化凭据跳过；⑥ 排序键首位前插 support_rank 是**有意行为变化**（非零回归，均衡被打乱是目的）；⑦ 工时 2-3 人日（非 0.5-1），先出设计再动手 |
| **客户端 Key 分发**（P3-35 立项） | **做，分两期**（研究）：一期轻量分享 4-5 天（ClientKeyManager + sync_system_key 收编主 key 零迁移 + middleware KeyContext + usage 归因 client_key_id + admin API + Key 管理页）；二期商业闭环 6-8 天（bound_credential_ids 绑定账号（k2cc 路线，比 zyphr 分组更适合我们分身机制）+ spendingLimit/durationDays + user-ui）。规避：fs_atomic 原子写（不学裸 fs::write）、constant_time_eq（k2cc 用普通 == 是弱点）、mask 展示 |
| **跨月配额自动恢复** | **✅ 已做（2026-08-15 W5）**：见 (c) MAJOR 条目——quota_exhausted_at 字段 + 12h 月初缓冲 + 懒触发 + 幂等 + 闭环测试 8 个。评审的「余额缓存挡探针」书面放弃（balance_cache 在 AdminService 私有字段够不着，退化为幂等 + 402 再禁用兜底） |
| **524 终态渲染** | **✅ 已做（2026-08-15 W5）**：见 (c) 条目（`: 524` 状态行 + `"524 a timeout occurred"` 连续形态 + 反例测试） |
| **前端更新 refreshToken 入口** | **✅ 已做（2026-08-15 W5）**：方案 A+D，见 (c) 条目 |
| **模型兼容波 P0（2026-08-15）** | **✅ 已做**：mapped_model 预判缺口（predict_passthrough_upstream_model 与 forward 逐位对齐 + 8 测试）；登记缺口 A/B/C（client_model 参数链路 / overload fallback 成功+失败 / websearch 回灌 mapped_model）；通配符移植（map_target + allows_model 末尾星号 + 最长优先，13 测试）；路径段解析（resolve 剥 models/ 前缀 + 末段，含 [1m] 顺序）；测试矩阵补齐（effective_model 白名单分支 / select 门白名单 / trace_db 双口径 / 黑名单键，7+ 测试）；docs/model-compat-plan.md 分期设计（P1：巡检+正向路由合并 / P2：家族限流等）。F1 通配×normalize 陷阱已测试钉住 + 文档警示；F2 fallback 失败路径已修；F5 空名通配已钉住。F3（通配符放大黑名单漏判面 → 登记 D 优先级上调）待 P2；F4b 透传白名单按原始名（含路径段）匹配待文档标注 |
| **websearch 快路径结构性缺陷**（W7-W8 smoke 诊断，新增） | **待 owner 决策**：快路径 MCP 硬依赖 Kiro 池号，纯透传池时透传拦截（静默失效）或无号 502。选项 D 补 Kiro 号（零代码，推荐）/ A 判定前移 / B 无号降级转发 / C 大工程 |
| **RPM release_index 精确化** | **✅ 已做（2026-08-15 W5）**：RpmTracker kth_oldest_age（O(1) get）+ RpmRecovery 等待第 fresh-limit+1 个时间戳过期；k=1 与旧行为一致（容差测试）；2 个测试 |
| **自定义模型测试 /api/admin/models/test**（zyphr P2） | 待做（上号实测工具） |
| **OpenAI 显式会话标识亲和**（zyphr P2） | 待做（四来源 UUID） |
| **GPT reasoning 双通道**（zyphr P2） | 待做（pigcode gpt 号需要时） |
| **缓存测试方法论**（k2cc test-cache.sh 真实账单反推） | 待做 |
| **continuationId sticky**（k2cc P1） | **无需移植**（评审修正论据）：我们 UserAffinityManager 条件钉住已覆盖核心语义，但粒度不同——我们主会话级（conversationId），k2cc 子代理级（客户端 agentContinuationId，我们 converter 不读入站字段），TTL 30min vs 60min。未来需子代理级粘合再重做 |
| **h[0] 冻结三件套**（k2cc P1） | **大部分已覆盖**（评审验证）：cch 归一已有（canonicalize_billing_header 整块折叠，更强）；PREV_H0 冻结不需要（我们剥离路线已把字节不稳定源核实为 0）；唯一缺口是 **system-reminder 指纹层剥除** —— **✅ 已做（2026-08-15 W5）**（历史 user 消息 reminders 漂移 → 指纹段 miss；指纹层剥除零行为风险，转发字节不动遵守 RFC 禁令；2 个测试） |
| **rotation_bias**（k2cc P0） | **降级/砍**（评审）：与 5s 调度冷却 + ramp_tier 三重重叠，小号池（1-3 号）无边际价值；增量仅「成功清零快速恢复」。除非实测 ramp_tier 持续 429 场景不足 |
| **fingerprint 首 miss break + 85% cap** | **砍**（评审否决，2026-08-15 验证修正论据）：对累积哈希链而言，浅段 miss ⟹ 深段 hash 不可能匹配（「首 false 即断」误杀真命中」的论据自相矛盾，撤回）；真正否决理由是 **85% cap 无依据**——L3 精确哈希链无高估问题（read ≤ covered ≤ total 天然成立），cap 是 k2cc 配三角分布模拟的设计；break 语义改动无收益。唯一可做：查询时惰性删除过期条目（内存清理，不影响正确性） |
| **子 API Key 体系 + user-ui** | 二期（随客户端 Key 分发） |
| **customModels 配置化映射**（zyphr P1） | 待做 |
| **Token 源文件重载**（zyphr P1） | **不适用我们架构**（无共享明文文件 + 加密存储 + 服务器网关）——等价物是前端更新入口（见 (c) 方案 A） |
| **must_wait_for_upstream**（zyphr） | **不移植**（研究结论）：我们多号多账号模型下换号真实有效（吸收层存在意义），与 zyphr 单账号模型前提不同；同 IP 风控已有 MAX_SUSPICIOUS_FAILOVERS_PER_CALL 防御 |
| **QUOTA_EXHAUSTED_ALL→402**（k2cc） | **评估完成（2026-08-15，docs/quota-402-design.md）**：跨月恢复已做（W5）；402 配套为独立小改（判据同构，402 让客户端停手 → 跨月自动回池），**结论 = 条件做，未实施**，随 websearch 结构性缺陷后排序 |
| **BTreeMap input_schema** | **待证**：先查 OpenAI/透传路径是否直接序列化 HashMap 不过 normalize_json_schema，查不到收益点就砍 |
| 发 v1.1.2 | 待用户决策 |
| native_thinking_effort_enabled 开启 | 待上号实测 |
| opencode 配置切 api.dwgx.top | 待用户 key |
| gpt-5.6-sol 请求来源排查 | 待排查 |
| websearch 整轮缓冲改造 | 等上号实测 |
| P0 上号实测清单（native effort/指纹命中率/error_message 盲区/websearch TTFB/401 判据） | 基础 smoketest 已过，长周期项待观察 |

## (e) 还在研究的问题

| 项 | 现状 |
|---|---|
| 0.6657 双轨记账 | **已实现**（评审验证）：stream.rs:171-188 CLIENT_TOKEN_DISPLAY_SCALE + scale_for_client，message_delta 缩放、resolved_usage 入库真值；**差异**：我们 output_tokens 刻意不缩放（比 k2cc 克制），message_start 保持 billed 未缩放（有意不同步，stream.rs:1650-1653） |
| 客户端格式错误防 503 风暴 | **已具备**（评审验证）：provider.rs:3342-3364 认 TOOL_USE_RESULT_MISMATCH + TOOL_SCHEMA_INVALID（reason 集合与 zyphr 逐字一致），break 不重试 + 透传路径格式 400 不在重试集。可选增强：JSON 结构化确认 + message 级兜底（zyphr 有，我们裸子串——误伤概率低，择机补） |
| 429 链 | 我们整体强于两个参考仓（端点桶隔离 + 凭据级 RA 冷却 + 吸收层 + 全池冷却快速失败带真实秒数）；zyphr「最早类型化 429 保留」确认我们缺失（429→generic 覆盖丢退避指令），但两级退避已兜住，中改排后 |
| 上游明确 Retry-After 时禁换号 | **维持现状**（研究结论）：换号有收益；HTTP-date 格式 Retry-After 解析缺失（upstream_trace.rs:103-105 自认，zyphr 认双格式）——低-中，按需补 |
| web_search type-only 流量验证 | **已确认 0 流量（2026-08-15 W5）**：AND 收紧前后 nbus 日志占比确认，无 type-only 请求；维持收紧语义（能力降级为有意决策，非官方形态） |
| BTreeMap 收益验证 | 待做（查绕过 normalize 的路径） |
| invalid_grant 延迟禁用（方案 B） | 待上号实测上游语义（access_token 是否连坐吊销） |
| D 类阈值 | 先修度量再调参 |
| 指纹 creation 低估、system 动态头不跳（审计 MINOR 全量） | 登记待决策 |
| usage by_model/by_requested_model 双口径复制品 | 登记待决策 |
| 微软 SSO leg1 state 依赖上游 portal 回传 | **用户确认流程一直可用**，后续继续用；无需额外验证 |
| 模型黑名单 30min TTL 长周期表现 | 观察中 |
| 透传池 ramp_tier / pinned client 固化解析上线后表现 | 观察中 |
| k2cc openspec WHEN/THEN 规范驱动是否引入 | 待决策（可作为我们复杂模块的 spec 化参考） |
| k2cc「ephemeral 配置不生效」（问题深挖 MAJOR#2） | **待复查**（2026-08-15 验证）：代码层面字段接线完整（config.rs:34-35 + fingerprint.rs:226/:244 + main.rs），与「不生效」论断矛盾；可疑点在 admin PUT 覆盖路径或部署配置缺失，需复查后定级，当前不采信 MAJOR |
