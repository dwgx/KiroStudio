# kirostudio 全仓深度审计 + 修复（2026-08-15）

## 目标
用户要求：全仓深度挖掘缺陷/不足（每个角落都要看），主线读代码找问题进清单，派 subagent 按清单修复 + 自行找问题 + 修补旧问题，可少量实测网关消耗。并发放开（探查类 5-6 一波，执行类 ≤5）。

## 验收标准
- 问题清单落盘（本文件 + 每 agent 报告）
- 每个修复过 CI（skiapi Docker 验证循环，1766 基线）
- 修复前 review，BLOCKER/MAJOR 修掉再交付
- 实测消耗（可选，少量 token）

## 环境事实（已确认）
- 线上 nbus = 38.244.34.15:52535（root），systemd kirostudio.service，0.0.0.0:8990，v1.1.1 + 严格语义修复 build，/healthz 全绿 pool=4
- HEAD = 1e100a2（master=origin，已推）；工作树 124 个未提交文件（多会话并发，禁止 git add/commit/checkout）
- CI 基线 1766 passed / 0 failed（skiapi Docker 验证循环，命令见 CLAUDE.md）
- 本机 8GB 编不过 Rust，验证必须走 skiapi 服务器
- 前端 admin-ui 用 pnpm；改前端需单独同步 dist

## 问题清单（波次 1 汇总，6 探查 agent + 主线交叉验证）

### MAJOR（11 项，已验证）
| # | 位置 | 问题 | 验证 |
|---|---|---|---|
| M1 | websearch.rs:974-1037 | WebSearch 快路径忽略 stream，非流式请求收到 SSE 流（协议违规） | 主线读过源码确认 |
| M2 | websearch.rs 全文件 | 快路径零 usage 埋点（RequestRecord/emit_record 零引用） | 主线确认 |
| M3 | handlers.rs:1104-1113 | subscription_unsupported=1 被宽匹配译成 502 可重试+误导文案 | 待修时确认 |
| M4 | handlers.rs 非流式收尾 | 非流式无空响应兜底（流式有） | 待修时确认 |
| M5 | provider.rs:2034 | MCP 路径 call_mcp_with_retry 无墙钟预算 | 待修时确认 |
| M6 | token_manager.rs:1339 等 | consecutive_passthrough_failures 死字段：注释承诺自动禁用，实现只有清零点（provider.rs:1938 注释与 token_manager.rs:3256 注释矛盾） | 主线 rg 确认零累加/零读取 |
| M7 | passthrough.rs:431-435 | 非 2xx 错误体 from_utf8_lossy 破坏 gzip 字节，客户端解压必失败+分类失效 | 待修时确认 |
| M8 | passthrough.rs forward/fetch_upstream_models | custom_api 透传运行时无 SSRF 复验，DNS rebinding 可绕过（ssrf.rs:419 注释的"禁重定向兜底"无效）。注意：fuckopencode 是 127.0.0.1 本机代挂，修复必须允许显式内网配置，只固化域名解析 | 待修时确认 |
| M9 | admin/service.rs:2784-2812 | region 探测窗口：新号以启用态入池，探测期间真实流量打错区恒 403（注释声称已修实际没修；import_one_key/clone 两路径都是先禁后建） | 主线 codegraph 确认两路径实践 |
| M10 | upstream_trace.rs 整文件 953 行 | 不在编译树（kiro/mod.rs 无声明，main.rs 无 mod user_id 同样孤儿），config.rs 三个配置字段静默无效 | 主线 rg 确认零声明 |
| M11 | token_manager.rs 透传排序键 | 透传池排序键缺 ramp_tier 维度（主路径有） | 待修时确认 |

### MINOR 精选（40+ 项，分组）
- **kiro 核心**：模型黑名单 TTL 注释/日志说 60s 实现 30min；refresh_token_locked 裸子串重试判定；透传冷却依赖全局 cooldown_enabled；set_overage 提交后轮询失败误报错；request_limit 字段文档与实现矛盾；死字段 passthrough_overload_since/last_passthrough_429_at；排序键注释编号错位（⑫重复/第⑥位误标）；排序键 ⑨⑩⑪ 位无守卫；三个 refresh 函数 refresh_token 直接 unwrap
- **anthropic**：流式 thinking tokens 不计 output_tokens（口径分叉）；flush_tool_input 失败态提前累计；回灌路径非法 JSON 静默降级空对象；emit_websearch_loop_usage 恒 is_streaming=false；set_tool_compat_mapping 零调用半接线；快路径 title 未 normalize_html_text；compressor 带 images 压空不修复；非流式 thinking 缺占位块
- **admin/openai**：external_idp leg1 state fail-open；export_credential 缺 no-store；usage_handlers 500 泄漏内部错误；OpenAI chat id 用上游 msg_xxx；submit_leg2_select 静默替换 profile；perform_update 显式版本仍依赖 tags 拉取；alerting client expect panic + 跟随重定向；告警 webhook 无 redirect none
- **透传/usage**：count_tokens 注释 4.5 vs 实现 4.0；acc_token 截断 vs 四舍五入注释；透传 URL contains("/v1/") 过宽；IngressRateLimiter 每请求全表 retain；cache_fingerprint LRU 每次全表排序；不观测 anthropic-ratelimit-*；extract_client_ip 死代码+注释指向；count_tokens 远程失败可拖 300s
- **前端**：上号弹窗关闭后轮询链不终止（MAJOR）；微软 SSO+conn 页无 i18n；cleanupDisabled 失败误导文案；api-region 注释过期；用后端中文文案做判定（cooldownReason==='速率限制'，本期不动避免跨端冲突）；硬编码中文散点；4 个死组件文件 ~1700 行；add-credential 错误中文；设置弹框轮询后不同步；storage.getApiKey 每次同步写
- **docs**：ARCHITECTURE 13 键已过期（实际 12）；冷却时长表缺 AuthTransient；compressor 两层 vs 实现 4 层；config.rs 注释编号 1→3 跳跃

### 决策项
- M6/M10 需先看代码再定方向（接线 or 删注释/删文件）
- M8 修复必须允许显式内网配置（fuckopencode 127.0.0.1 是合法用例）
- 前端中文判定耦合（cooldownReason）本期不做，避免与后端改动冲突

## 波次记录
- W1（完成）：主线 codegraph 读关键路径 + 6 探查 agent 分模块审计，清单落盘（11 MAJOR + 40 MINOR，0 BLOCKER）
- W2（完成）：7 个修复 agent（A-G）+ 主线 token.rs，全部修复过 CI
- W2.5（完成）：对抗 review（2 reviewer）→ 3 MAJOR + 8 MINOR → 2 修复 agent（H 后端 7 项 / I 前端 3 项）+ 主线 3 处守卫同步 → 全绿

## 最终验证（2026-08-15 实测）
- 后端：**1845 passed / 0 failed**（基线 1766，+79 测试），服务器 Docker 验证循环
- 前端：tsc --noEmit 干净 + **48 pass / 0 fail** + pnpm build 成功
- 关键新增测试单跑全部确认执行（thinking 双计守卫 ×2、近空判据共用、refresh 结构化、pinned 缓存 port、MCP 墙钟、websearch 守卫、provider 埋点守卫）

## 修复成果汇总（W2 全量）
- **协议/正确性**：websearch 快路径非流式返回 JSON（原协议违规）+ 补用量埋点；subscription_unsupported → 404（原 502 误导）；非流式空响应兜底 + 近空判据两路径共用；thinking 双重计数回归修复；流式/非流式 thinking output_tokens 口径对齐；OpenAI id 自生成
- **安全**：custom_api 透传 DNS 固化防 rebinding（保留本机代挂合法配置）；非 2xx gzip 错误体不再被 lossy 破坏；告警 webhook 禁重定向 + 构建失败降级；export_credential no-store；usage 500 去内部细节；external_idp leg1 state fail-closed
- **健壮性**：MCP 墙钟（720×2+30s 推导）；透传池排序键加 ramp_tier；refresh 结构化错误替代裸子串；3 处 unwrap→ok_or_else；死字段/死代码清理（consecutive 保留为观测、passthrough_overload_since 等删除）；count_tokens 超时 300→10s；LRU/限流抽样清理；模型黑名单注释对齐 30min
- **接线**：upstream_trace 完整接线（mod + main.rs + provider 两路径埋点 17 分类，默认关零开销）；region 探测窗口保护（先禁后探）；eidp select 回显 arn/region（后端 handler 补齐）
- **前端**：上号轮询链泄漏修复（PollGuard bump，含 countdown 超时路径）；微软 SSO + conn 页 i18n 接线；快照覆盖远端新值修复；4 死组件删除；storage 读优化
- **文档**：ARCHITECTURE 12 键、AuthTransient、压缩 4 层、排序键注释 12 位

## 遗留（有意未做 / 待决策）
- 前端 cooldownReason 中文耦合判定（跨端改动，下期）
- config.example.json 未加 upstream_trace 三字段（serde default 兜底）
- upstream_trace 默认关（未开）；OTA 未启用
- 线上实测：改动未部署！部署需用户批准（nbus deploy 流程）
- 用户决策项照旧：opencode 配置切 api.dwgx.top、gpt-5.6-sol 来源排查、v1.1.2 发版
- leg1 state 依赖上游 portal 回传——上线后实测一次微软 SSO 全流程（reviewer o1）

## W3（2026-08-15 下午）：上线 smoketest + 参考仓库吃透（已完成）
- **nbus 部署成功**：2026-08-15 修复 build（sha 52748cf2，19.5MB），备份 .bak-1.1.1-fixes，systemd active，/healthz 全绿 pool=4
- **smoketest 4 通道全通过**：#1 fuckopencode（deepseek-v4-flash）200/1.1s；#2 deepseekapi（deepseek-v4-pro）200/2.6s；#3 cursorapi（claude-sonnet-4-6）**已启用** 200/1.1s；#4 pigcode（gpt-5.6-sol）200/4.7s
- **M1 修复线上验证**：websearch 非流式返回 JSON（不再 SSE）
- **微软 SSO**：start 端点正常（signinUrl + PKCE），chrome 打开登录页可达（4 种登录方式）——完整登录流程待用户操作
- **参考仓库克隆**：/tmp/ref-zyphr（ZyphrZero/kiro.rs @ d3cec44 v0.7.6，47193 行）+ /tmp/ref-k2cc（TsinHzl/kiro2cc-proxy @ 252b5ee v2.9.6，28657 行），codegraph 索引已建
- **8 路总结完成**，落盘：docs/ref-ZyphrZero-kiro.rs.md + docs/ref-kiro2cc-proxy.md
- **问题总清单**：.opencode/ISSUES.md（a-e 五类，与 STATUS.md 同步）
- 高价值移植候选：模型感知正向路由（P0）、跨月配额恢复+禁用持久化分层（P0）、rotation_bias（P0）、token 源文件重载、customModels、客户端 Key 分组、h[0] 冻结
- 新发现我们的潜在缺陷：web_search 判定 OR 过宽（MAJOR 待确认）、input_schema HashMap 序列化顺序不稳、自动禁用原因重启变手动（已知）

## W4（2026-08-15 晚）：深度对比研究 + 双路评审（已完成）
- 8 路对比研究（我们 vs zyphr vs k2cc：模型路由/禁用生命周期/选号调度/token 刷新/缓存体系/web_search+序列化/客户端 Key/错误翻译）+ token 刷新重跑
- 2 路对抗评审（认知更新验证 + 移植候选可行性）——评审抓出关键修正：
  - **fingerprint 首 miss break + 85% cap：否决**（哈希链铁律证明方向反了，read→creation 转移多收钱）
  - **跨月配额恢复：改设计**（quota_exhausted_at 独立字段与 reason 解耦 + 余额缓存挡探针 402 + 清缓存 + UTC，200+ 行非 100）
  - **模型路由：改方案**（Unsupported TTL + 预热限频单飞 + 工时 2-3 人日）
  - **rotation_bias：降级/砍**（与 5s 冷却 + ramp_tier 三重重叠）
- 认知更新（评审验证）：自动禁用重启变手动**已修复**（ISSUES (a)）；0.6657 双轨**已实现**；sticky **无需移植**（粒度差异：主会话 vs 子代理）；input_schema 字节序**已被 converter 兜底**（动机修正）；客户端格式错误防风暴**已具备**
- token 刷新研究：zyphr 源文件重载**不适用**（无共享文件+加密存储）；真实缺口 = **前端更新 refreshToken 入口**（后端已就绪前端零调用）+ update 路径补校验
- 结论落盘：ISSUES.md 全面更新（(a)(c)(d)(e) 含研究结论）
- 干净小修复待做：524 终态（~1 行）、RPM release_index 精确化（~15 行）、web_search AND（先验证）、前端 refreshToken 入口、system-reminder 指纹剥除

## W6（2026-08-15 深夜）：模型名链路全链路一致性审计（已完成，只读+登记，0 改动）

审计「统计口径 vs 实际服务模型」：9 项清单逐项查证（含并行 agent 对 provider.rs mapped_model 预判缺口的修复确认）。结论：**范围内（passthrough.rs/usage/handlers.rs）零修改**——主要缺口全在 provider.rs / websearch.rs（范围外），全部登记：

- **已修（并行 agent，已确认）**：provider.rs `predict_passthrough_upstream_model`（1381-1416）完整链预判（映射→deepseek effective_model，白名单感知+per-凭据覆盖），:1607 接线，测试 13+ 用例（7852-8000+）
- **登记 A（MAJOR）**：Kiro 主路径 requested_model 记的是 converter 归一化后的 Kiro id（claude-sonnet-4-5-20250929 → claude-sonnet-4.5），≠ record.rs 契约"客户端原始名"，且与透传路径（原始名）口径分叉。修法：provider.rs call_api_stream/call_api_with_retry 加 client_model 参数（失败记录用）+ handlers.rs 4 处成功埋点（2383/3163/3291/3980）requested_model = payload.model，两处必须联动否则成功/失败混合口径更糟
- **登记 B（MAJOR）**：overload_fallback_model 成功路径 provider.rs:4350 `CallMeta.model = fallback 名`，违反 CallMeta 契约"model 恒为客户端原始名"→ requested_model 失真。修法：model 保持原始名，mapped_model = Some(fallback 名)
- **登记 C（MAJOR）**：websearch 回灌路径 upstream_model 恒 None（websearch.rs:1569 run_round 丢弃 CallMeta.mapped_model）→ 主路径映射命中时 by_model 失真。修法：WebSearchLoopSuccess 加 mapped_model 字段，emit_websearch_loop_usage（handlers.rs:531）写入
- **登记 D（MINOR）**：模型黑名单键用客户端原始名（provider.rs:1916），与改写链（mapping+deepseek）不对齐 → 同改写目标的其它原始名漏判（不误伤）。修法：mark 用 predict 结果（改写后名）作键，token_manager.rs:3160 同步换键
- **一致项**：websearch 快路径 model=payload.model（无改写）；MCP 埋点 "mcp" 常量（设计）；失败记录 model（主路径 fail_record/透传 PassthroughMeta/admission）口径各自一致；by_model 回落 r.model 是总量守恒设计；成本随 by_model key 传导（源头=登记 A/B/C）；白名单三处规则逐位一致（选号层与改写层共用 effective_model 本体；映射不进选号预判为设计接受）

## W5（2026-08-15 深夜）：模拟缓存 + 修复清单全量执行（已完成，CI 1886/0 + 前端 48/0）
- **模拟缓存功能**（用户要求，做完美）：config mockCacheEnabled/mockCacheReadRatio（默认关/0.7，可配 0-100% 含 100%）+ TIER3 热重载全链（config→main 播种→admin PUT→镜像→passthrough 每请求读）+ 透传池响应注入（流式 message_start/message_delta + 非流式 usage，read=round(input×ratio)、creation=0、clamp 不变量）+ Kiro 池四层链隔离 + 前端设置面板（开关+比例 stepper+三语）+ 守卫测试全套。**注意**：注入的是伪造值（用户明确要模拟），sub2api 等下游可见
- **修复清单全量执行**：524 终态渲染（状态行形态 `: 524` + `"524 a timeout occurred"`，反例测试防误伤）、RPM release_index 精确化（RpmTracker kth_oldest_age）、update_refresh_token 补校验（截断+跨凭据去重+api_key 类型闸+trim）、前端「更新 Token」入口（按钮+对话框+InvalidRefreshToken 横幅+三语）、web_search OR→AND（含 tool_choice 闸、type-only 归一化补 WebSearch 大写 key 缺陷修复、线上 0 流量确认）、跨月配额恢复（quota_exhausted_at 字段+12h 月初缓冲+懒触发+闭环测试）、system-reminder 指纹剥除
- **避开评审否决项**：fingerprint 首 miss break / 85% cap / rotation_bias 未做
- **双路对抗 review**：2 MAJOR + 15 MINOR 全部修复（时区缓冲、注入门控、524 收窄、类型闸、trim、torn read 顺序、sanitize 写盘、恢复标志、前端空串/toast）
- **参考仓库问题深挖**：zyphr 16 项（MAJOR 7：日志泄漏 Authorization、websearch_loop 静默 {} 回退、缓存断点复活、会话亲和被客户端操纵、count_all_tokens 同步阻塞 300s、解码器静默截断、720s 流式超时）+ k2cc 20 项（MAJOR 10：fingerprint 生产死代码、ephemeral 配置不生效、L2 首请求 read 失真、子 key 并发超卖、activate_key 全局写锁、续期清空 expires_at、前端 0.72 硬编码、5m/1h 落库恒 0、RPM 门控超时放行、文档与实现背离）——对我们的启示已记录，待下一轮消化
- **CI 过程**：跨 agent 协作抓出 3 轮编译错误（chrono trait、借用、守卫 needle 尾逗号）+ 2 轮测试修复（new() 内启动恢复抢先、Duration::abs）全部修完

## W6（2026-08-15 深夜）：模型兼容波（已完成，CI 1921/0，未部署）
用户指令：修 mapped_model 统计缺口 + 以 sub2api/NewAPI 为参考全面扩张模型兼容层 + 大量派发 + 双 review（subagent 自 review + 主线对抗 review 均完成）。
- **根因修复**：透传 mapped_model 预判只算 model_mapping 漏 deepseek fallback → `predict_passthrough_upstream_model`（provider.rs:1385，与 forward 改写链逐位对齐：豁免/顺序/cfg merge/白名单）+ 8 测试
- **全链路审计**（9 项）：登记缺口 A/B/C 全部修复——A 主路径 requested_model 契约（call_api 系加 client_model 参数，成功/失败同源）；B overload fallback model/mapped_model 契约（成功+失败路径，F2 已补）；C websearch 回灌 mapped_model 带出（run_round 三元组 + 埋点）
- **sub2api 移植**：model_mapping 通配符（末尾 `*` + 最长优先 + tie-break）+ allows_model 通配（共享 wildcard_matches）+ 路径段解析（resolve 剥 models/ + 末段，唯一入口一次剥除，含 [1m] 顺序）+ 14 测试
- **NewAPI 学习**：巡检自动同步 + 模型归一化单函数多消费 + 错误回显——进 docs/model-compat-plan.md P1/P2 分期
- **测试矩阵**：effective_model 白名单分支（1439 核心，零覆盖→5 分支）、select 门白名单（含 1439 修复态 + F1 通配×normalize 钉住）、trace_db 双口径、黑名单键自洽 + 源码守卫（select 必须调 effective_model）
- **主线对抗 review**：可交付（无 BLOCKER/MAJOR）；F1 已测试钉住+文档警示；F2 已修；F5 已钉住；F3（通配放大黑名单漏判→登记 D 优先级上调）/F4b 待办
- **设计文档**：docs/model-compat-plan.md（P0 已完成 / P1 巡检+正向路由合并 / P2 家族限流等 + 不做清单 + 8 守卫规划）
- 线上实测背景：opus-5 成功出话（strict 链路改写为 flash 发给 fuckopencode，正常设计）；v4-flash 首次 429 是 fuckopencode 周额度瞬态；trace 失真根因=统计预判缺口（本轮修复）

## W7（2026-08-15）：错误码/提示词可配置化——翻译接入层（进行中，未编译未部署）
任务：错误翻译接入层 + 3 处矛盾修复（设计 docs/error-codes-config-design.md §四/§五）。
**只允许改**：src/anthropic/handlers.rs、src/kiro/passthrough.rs、src/anthropic/websearch.rs（openai/ 确认无需改）。

### 接口契约（与并行 config 结构层对齐——合并时关键）
- config 字段 `error_messages: HashMap<String, ErrorMessageOverride>`（默认空表），
  `ErrorMessageOverride { status: Option<u16>, r#type: Option<String>, message: Option<String>, retry_after_secs: Option<u64> }`（全 Optional，None=内置默认）
- handlers.rs 提供：`pub fn set_error_messages(ErrorMessagesTable)`（main/reload_config 接线点）、
  `pub(crate) fn current_error_messages() -> Arc<ErrorMessagesTable>`、`pub(crate) fn resolve_msg(cfg, key, default:(StatusCode,&str,&str,Option<u64>)) -> (StatusCode,String,String,Option<u64>)`
- **热更接线待并行 agent**：reload_config（admin PUT /config）需调用 `set_error_messages`，否则配置不生效（镜像空表=现状行为）
- 镜像范式：COMPRESSION 同款（ArcSwap + OnceLock，handlers.rs:430 附近）

### 接入点（key 清单）
- map_provider_error 12 分支：shared_budget_exhausted / absorb_exhausted / gate_timeout(A3) / upstream_gate_full / rate_limited_pool(A5，RA 真值优先配置不可覆盖) / model_unsupported(A6，永久态忽略配置 RA) / rate_limited_credential(A7) / account_throttled(A8) / permission_denied(A9，永久态忽略 RA) / upstream_5xx(A11) / unrecognized_upstream(A12)
- translate 链 B1-B14：subscription_unsupported(忽略 RA) / quota_exhausted / quota_subscription / overloaded_capacity(**默认 RA=3，B4 修复**) / feature_not_supported / invalid_credential / image_mime_mismatch / request_body_invalid / context_too_large / input_too_long / upstream_dns / upstream_timeout / upstream_tls / upstream_proxy（400/永久态分支忽略配置 RA；可重试分支 RA 可配经 TranslatedError.retry_after_override 挂头；B9/B10 的 compress-retry 标记不受配置影响）
- TranslatedError.error_type 改为 String（配置值非静态）
- 入站闸门 try_inbound_admission_gate：gate_timeout（与 A3 同 key 双渲染点）
- D 类本地：request_parse_failed(D4) / provider_not_configured(D5，双入口同 key) / unsupported_model+empty_messages+tool_mapping_failed(D6) / request_serialization_failed(D7，4 处) / response_read_failed(D8) / empty_response(D9/D10，**默认 RA=3，D10 修复**；status 不可配——双形态判据承重)
- 透传池：err_response 统一读 passthrough_failed（E1-E4/E8/E9；E5 上游原文透传不改造；E7 空流因 guard_empty_stream 要求 &'static str 豁免）
- websearch：websearch_query_missing(F1) / mcp_failed(F2/F3/F8，**F3 RA 默认 8 走配置，修复硬编码"8"**) / websearch_failed(F4-F7/F9)
- OpenAI 层：零改动（G5/G6 透传内层 status+Retry-After+body，配置 anthropic 层即覆盖）

### 测试（handlers.rs + websearch.rs 内联）
configured_message_renders_marker_stays_pool_truth_wins / configured_status_and_type_override_render / b4_capacity_503_carries_default_retry_after / d10_empty_response_429_carries_retry_after / both_entries_read_same_provider_not_configured_key / f3_budget_exhausted_retry_after_defaults_and_overridable
（全局镜像测试用 ERROR_MESSAGES_TEST_LOCK 串行 + 复位空表，范式同 BLOCKLIST_TEST_LOCK）

### 状态
- 语法验证：rustfmt --check 通过（无 parse error，仅格式 diff 未采用）
- **未跑 cargo**（本机 8GB 编不过 + 任务约束）：编译推演完成，关键点 = crate::model::config::ErrorMessageOverride 依赖并行 agent 落地
- 未 git add/commit

## W7-W8（2026-08-15 晚）：错误码配置系统 + 大规模 smoke test（已完成，CI 1955/0，已部署 sha a3ae8874）
- **错误码/提示词可配置化**（用户需求 5 点全实现）：config errorMessages 表（42 key，per-key merge，热加载 TIER1+镜像）、校验（status 白名单 10 码含 504 / type 白名单 8 类 / 组合约束 / 决策词黑名单 / 承重串告警 / 整表拒绝）、7 处接入点（map_provider_error 12 分支 + 翻译链 + 透传 + websearch + 双入口）、矛盾修复（B4/D10/F3 补 RA）、前端弹窗（分页/搜索/编辑/恢复默认/校验回显/默认值预览 + defaults 接口）、守卫（key 集双向一致 + 全套接线）
- **对抗 review 三轮修复**：B1 组合校验 merged 值、B2 billing_error 移除（CC 重试风暴）、B3 key 集对齐（61→42，死 key 全删）、M1 启动校验、M2 语义错位（region_mismatch 拆 key）、M3 拆 key、M4 前端全表渲染、M5 流内声明不可配
- **大规模 smoke test（7 路 agent）**：4 通道（#1-#4 全 200）、透传 vs sub2api 对比、错误码系统端到端（配置→热加载→生效→恢复）、模拟缓存闭环（0.5/1.0 注入验证）、SSE 完整性、前端弹窗三轮回合（前两轮 BLOCKER：快照漏 admin-ui → 第三轮全 PASS）、OpenAI 层、边界矩阵
- **smoke 抓出的 bug 全修**：message_start 注入失效（测试结构错误掩盖——真实结构 message.usage 嵌套）、OpenAI web_search 静默丢弃（保留透传+warn）、max_tokens 超限误判 failover（本地 400 直返，校验上移到透传前）
- **websearch 结构性缺陷（诊断结论）**：快路径 MCP 硬依赖 Kiro 池号，纯透传池时透传拦截（静默失效）或无号 502。修复选项：D 补 Kiro 号（零代码推荐）/ A 判定前移 / B 无号降级转发 / C 大工程——待 owner 决策

## W9（2026-08-15 深夜）：前端 a11y 修复（已完成，tsc 干净 + 48/0，未部署）
- 用户要求：smoke test 发现 console issues 323 条（表单字段缺 label 关联/id-name），只改 admin-ui/
- **结构性修复**：settings-page `Field` 外层 div→`<label>`（隐式关联覆盖 ~40 个 Switch/textarea/select/Input/ComboInput/NumberStepper，布局类名不变）
- **label 关联补全**（4 路并行 agent + 主线）：login-dialog 8 控件（htmlFor+id+readOnly aria-label）、login-page（id+htmlFor+autoComplete）、usage-page 搜索、add-credential-dialog 5 处（region/proxy/paste 组 htmlFor+id、proxyUser/Pass aria-label）、credential-card updateToken textarea + 设置框 6 输入、clone-management 4 处（bulk Switch/节点行 Switch/取消按钮/标签输入）、ops-page 6 处（proxy/name/日志搜索/折叠/清搜索/复制）、ops-detail-dialogs 9 处（trace 筛选/缩略图/lightbox）、error-messages-dialog message textarea（`${key} 提示文案` 模式）、help-page 2、credential-canvas 重命名、region-select 搜索框（硬编码中文跟随文件惯例）、dashboard 5 个图标按钮
- **id/name 补全**（Chrome issue「form field 缺 id/name」）：NumberStepper/ComboInput/RegionSelect 内部 useId 自动 id（全局消灭 stepper/combo 类告警）；settings-page 12 输入 + 5 textarea 显式 id；usage/ops 搜索、error-messages-dialog 行字段 `${key}-message`/`${key}-retryAfter`、credential-card 6 输入、clone-management 8 输入（map 内模板化 id）、ops-detail DateTimeField useId
- **Dialog a11y**：6 个无 DialogDescription 的 DialogContent 补 `aria-describedby={undefined}`（Radix 警告消除，无视觉变化；credential-canvas 删除确认框有描述故未动）
- **图标按钮**：ops-page 2 个刷新钮补 aria-label（新增三语键 `opspage.common.refresh`）
- **新增 i18n 键 14 个 ×3 语**（无占位符插值，测试通过）：signinUrlAria/authorizeUrlAria/portalUrlAria/verificationUrlAria/searchAria/proxyUsername.label/proxyPassword.label/updateToken.aria/cancel/enableNode{label}/tagAria{id}/clientIpLabel/darkMode/common.refresh
- **验证**：tsc --noEmit 干净 + node 48/48 + 浏览器实测（dev server 代理线上 api.dwgx.top）：登录页/设置页/错误提示词弹窗/上号弹窗/用量/运维 全部 0 表单字段无 label、0 缺 id/name、console 无 React a11y 告警（仅剩 password-in-form [verbose] 提示，属既有类）
- **已知取舍**：Field 行内同时含控件+齿轮按钮的少数行（~6 处），label 关联首个可标注后代（控件），齿轮按钮自带 aria-label——浏览器/AT 行为正确，技术上 label 内容模型略非合规，接受
- 未部署（部署需 owner 批准）；vite.config.ts 已还原（dev 代理为临时验证改）
- **已知问题**：#3 cursorapi 上游号池空（502 持续，sonnet fallback 到 #1）；#1 fuckopencode 周配额（Go 限额，24h 恢复）；#1 端口已固化 8787；生图三层不通（路由 404 + catalog 400 + 上游 key 无 image 权限）；PUT {} 清空语义（per-key 需空对象）；poolHealth 观测盲区

## W9（2026-08-15 晚）：i18n 完美化 + a11y + token 交换修复（已完成，CI 1961/0 + 前端 48/0，未部署）
- **I18N 完美化**（用户要求"本地化完美做完"）：全站硬编码中文扫尾——528 行中文 → 335 处理 + 193 豁免（注释/后端比较/兜底），**真·UI 残留 0**；309 新键 ×3 语（知识库 262 字段键化、region 22 键组、帮助按钮等）；三语键集 2309=2309=2309 一致；tsc + 48 pass + vite build 全过
- **a11y 修复**（console 323 条 → 0）：~80 处控件修复（Field div→label 隐式关联覆盖 ~40 控件、useId 基元层、htmlFor+id 补全、Dialog aria-describedby）+ 14 键 ×3 语；浏览器实测 0 a11y 告警
- **Token 交换修复**（线上用户报「Token 交换失败 500 Oops」）：根因=上游 Kiro auth 服务 500（我们请求与官方同源，非我们问题）；修复排障短板——服务端 warn 日志（status+body 截断+code 脱敏只记长度/前 4 位）、5xx 重试 1 次（500ms）、4xx/5xx 文案区分；6 测试（含本地 TCP mock 重试验证）
- **遗留**：#1 fuckopencode 周配额（24h 恢复）；#3 cursorapi 号池空；websearch 快路径结构性缺陷（待 owner 决策 D/A/B/C）；生图三层不通；前端 i18n 后「速率限制/可疑活动」等后端文案比较点仍耦合（ISSUES (c) MAJOR 待后端枚举化）

## W10（2026-08-15 深夜）：结构绊脚石 #11 —— 前 5 大函数行为测试补写（已完成，未验证 CI，未提交）
- **侦察结论**（codegraph「零覆盖」标注部分过时）：add_credential_with_intent 已有几十个走真实入口的行为测试（非零覆盖）；acquire_context_excluding 有 acquire_context 行为测试间接覆盖（token_manager 不可改文件，产出=标注）；**真实零覆盖 = call_api_with_retry/call_api_stream/try_custom_api_passthrough 行为链（provider.rs 仅纯函数+源码守卫）与 update_config_locked 行为（service.rs:7710 旧注释自称不可测——实际可测）**
- **新增 8 测试**（零生产改动）：
  - provider.rs（端到端 mock 上游：本地 TCP 假上游 + MockEndpoint 注入 `with_proxy` 注册表，经 `cred.endpoint=Some("mock")` 钉死选号链）3 个：首次 200 成功（retries=0/hits=1）、429→冷却→换号→200（retries=1/hits=2）、单号池预算 1 恒 500→Err 且只打 1 次（不风暴）
  - service.rs（tmp 磁盘 config + Config::load 构造真实更新链路）5 个：restart_fields 累积+未提交字段保持、四类非法值整单拒绝零写盘、TIER1/TIER3 立即生效文案、同值提交回「无改动。」、error_messages per-key merge（未提交 key 保持 + 整表拒绝保旧表）
- **对抗 review 修复 2 BLOCKER**（reviewer 抓出，均为测试侧）：CallMeta 无 Debug → .expect 编不过（改 match 解构+panic）；api_key 号被 effective_endpoint_order 自动路由到 cli/cli-runtime 候选链 → mock 端点零命中（补 cred.endpoint=Some("mock")）
- **标注不可测**：call_api_with_retry 的 AWS 签名/流式帧/吸收层 sleep 时序分支（需真实上游语义）；try_custom_api_passthrough 网络循环；acquire_context_excluding 内部（token_manager.rs 不在本次可改范围）——seam 建议：若将来要测，给 KiroProvider 注入 client 工厂（替代 client_cache 的 reqwest::Client）
- **验证状态**：未跑 cargo（本机 8GB 编不过 + 任务约束）；已做静态推演 + 对抗 review（A/B/C 三类核验）；需走 skiapi Docker 验证循环真跑
- 未 git add/commit

## W11（2026-08-15 深夜）：协议绊脚石 #12/#13（已完成实现，后端 CI 被并发 WIP 阻塞未验证）
- **#12 幽灵承重串「等容量」认知纠错**（docs/blockers-protocol.md §1.4）：shield COOLING_MARKERS 实测只有 3 个英文串，`等容量` 仅注释出现非判据。
  - error_messages.rs：`check_load_bearing_message` 词表 = shield 判据逐条镜像（`all credentials are temporarily` / `temporarily cooling down` / `inbound rate shaping`）+ prompt is too long + 背压哨兵；`等容量` 移除；模块/表注释勘误
  - handlers.rs：守卫重写为 `shield_cooling_markers_stay_in_production_text`（A5 双哨兵恰好 1 处 + `inbound rate shaping` 恰好 2 处：A3 分支 + 入站闸门）；A1/A2 错误注释勘误；顺带修 H1 重复 #[test]（错位 doc 移回 permanently_exhausted 测试）
  - 测试：error_messages `load_bearing_detection_covers_inventory_markers` 翻转（等容量 is_none + 三哨兵 is_some）；service.rs `validate_accepts_load_bearing_message_with_warning` 词表同步
  - 文档：inventory §3.1 勘误 + blockers-protocol §1.4 守卫行更新 + blockers-testing H1 标记已修
- **#13 前端语言耦合 5 处改枚举**（设计稿 cooldown-reason-i18n-design.md 未实现过 → 按文档实现 + 补 2 处遗漏）：
  - 后端：cooldown.rs `CooldownReason::code()` 9 变体 snake_case + 测试 `test_cooldown_reason_code_covers_all_variants`（code↔description 一一对应）；types.rs CredentialResponse + service.rs CooldownDetail/两出口加 `cooldownCode`；重复类错误独立判别 `DuplicateCredential`（error.type=`duplicate_credential`，classify_trash_error/classify_add_error/update_refresh_token 三处接线）
  - 前端：types/api.ts 加 cooldownCode/code；lib/cooldown.ts 纯函数 helper（isRateLimitCooldown/isSuspiciousCooldown/cooldownReasonKey/cooldownReasonLabel，翻译注入不 import i18n）；i18n 9 key ×3 语；5 处判定改枚举（credential-card:228 / credential-row:255 / overview-page:84 / use-pool-notifications:208 / settings-page:1566 用 parseError.type==='duplicate_credential'）；2 处展示走 i18n label
  - 测试：新增 admin-ui/tests/cooldown.test.ts 5 个（判定只认码/9 码映射/三语字典齐全/label fallback）
- **验证状态**：前端全绿（tsc -b + pnpm build + node --test 53/53，含新 5 个）；**后端 CI 未验证**——skiapi Docker 两次构建被**并发会话 WIP 的 3 个编译错误阻塞**（handlers.rs:2627/4100 `payload.max_tokens` i32↔u32、cooldown.rs:743 save_lock 重构丢类型注解，均为他人工作树改动，行号核验非本次改动）；我的后端改动区域无编译错误报告
- 未 git add/commit；未部署

## W11.5（2026-08-15）：CI 失败 6 测试修复（已完成，未验证 CI）
- 任务：修复 #11 update_config 5 测试（helper 构造 panic）+ #13 duplicate_token 1 测试（错误类型断言）
- **update_config ×5（helper 错，非实现错）**：`svc_with_disk_config` 写 `dir.0/config.json` 但 TempDir 只登记路径不建目录 → `fs::write` NotFound panic（旧 :12384:76 = fs::write 的 unwrap，非 serde 行）——对比 `rotate_config_backup_keeps_three_generations` 有 create_dir_all 而 5 个 update_config 测试没有。修复：helper 内补 `create_dir_all`（一处修 5 测试）。已推演全部断言过（restart_fields 顺序/文案 2 个字段、四类非法值 pre-save 拒绝、热字段立即生效文案、同值「无改动。」、error_messages per-key merge + 418 整表拒绝保旧表）
- **duplicate_token ×1（测试断言错，实现是 #13 有意变更）**：update_refresh_token:1316 跨凭据重复返回 `DuplicateCredential`（前端 duplicate_credential 判别依据），测试仍断言 InvalidCredential → 已改断言；status 400 / 文案含「重复」/ hash 不变三条断言在 DuplicateCredential 下仍成立（error.rs:70 BAD_REQUEST）
- 约束遵守：只改 src/admin/service.rs 测试模块 + helper；未跑 cargo（本机编不过）；未 git add/commit

## W10-W12（2026-08-16 凌晨）：绊脚石全量修复（16 项 + 并发工程类，CI 2020/0，已部署 edf27204）
- **上号问题（redirect_uri/region/截断）**：移植用户调试修复（full_redirect_uri 消费 callback.path / auth_endpoint_for_region / 回调读取截断 / host 头 / Accept / 重试 / 脱敏日志）+ 3 测试，已部署
- **现役 bug 级 1-8 全修**：#1 set_compression 接线（reload 播报 + 守卫）、#2 otaAutoCheck 后端补接线（restart-only + 契约测试）、#3 restore 表补 5 项 + 通用守卫 18/18、#4 mock_cache 守卫自证绿修复（needle 拼接 + rg 验证）、#5 buffered 路径补 2 测试、#6 快照命令三文档统一 + verify-snapshot.sh、#7 双层日志 filter（面板 INFO 可见）、#8 cooldown save 串行化 + 版本守卫（4 测试）
- **结构设计类 9-16 全修**：#9 model_blocklist/blacklist 合并（顺带修删号清理漏覆盖）、#10 禁用四字段双份（3 测试 + 守卫 + 真源注释 + 修 set_disabled 漏清 disabled_at）、#11 超大函数补 8 行为测试（TCP mock 端到端）、#12 幽灵承重串纠错（shield 真实判据 3 词）、#13 前端语言耦合 5 处改枚举（cooldownCode 9 码 + duplicate_credential）+ 前端 53 测试、#14 子串匹配 3 处结构化（quota/subscription/裸词 → 连续形态）+ 行为对照、#15 双入口提取 6 公共函数（427/365→323/257 行）、#16 调用环标注纪律
- **并发工程类**：count_tokens 可注入 + 6 测试（真打网）、websearch 测试锁修复、真并发测试 4 个（选号/刷新/禁用）、启动播种自检（21 镜像标记 + verify）、healthz build_sha（build.rs 注入）、告警扩展（pool_exhausted 5 埋点 + quota_exhausted + stats_stale watchdog）、refresh_loop/select_highest_priority 补测、**alerting poison 修复**（生产级：锁 poison 恢复 + 无 runtime 降级——32 测试连锁崩根因）
- **部署**：sha edf27204（static-pie 正常形态——首次传输/缓存构建出动态链接坏产物，回滚 + 强制重建 + 校验 sha 后成功；healthz 显示 build_sha）
- **模拟测试**：4 通道（#4 pigcode 上游 502 非网关）、错误码配置/枚举/热加载全链路、镜像自检 21 全接线、otaAutoCheck 落盘、mockCache 注入 43、max_tokens 400 直返、前端 cooldownCode 判定脱离中文 + INFO 日志面板可见 + console 无 error
- **MINOR 记录**：build_sha 部署时传 commit 真值（本次传的 blockers 标签）；OTA 回显旧值（restart-only 语义）；并发会话踩踏配置（错峰建议）

## W13（2026-08-16）：最终收尾四线（已完成，CI 2020/0 + 前端 53/53，已部署 88270616）
- **核验（blockers 修复真实性）**：14/16 真实有效（含全部高危项 needle 展开验证——#4/#11/#12/#13/#14 无自证绿无幻觉）+ 2 瑕疵 + 4 MINOR 全修：#6 快照命令彻底统一（build.rs 入 KEY_FILES）、#16 标注勘误（写/读引用分清）、stats_stale 接线（main.rs 60s 周期任务，10min 断更告警）、#13 Display 文案（凭据重复/无效分清）；#8 纳秒级残余为观察项
- **性能**：docs/PERFORMANCE.md——nbus 实测（2 核 Xeon E5-2680 v4 / RSS 32.5MB / p50 1185ms / 5 并发 100% 成功 / CPU 0.5%）；联网对比（new-api/sub2api 无公开硬基准——我们是稀缺硬证据）；README 性能声明草案 §8；P1 建议（UUIDv4→v7、RPM intern、EndpointHealth 单遍历）记录待做
- **性能仪表盘**：overview-page 加 PerfDashboard（6 指标卡 + 延迟分布 p50/p90/p99 条 + 错误分布 + uptime/RSS 元信息）；设置页「显示性能仪表盘」开关（localStorage uiLayoutPrefs，默认显示，隐藏即停轮询实测）；三语 21 键；无新依赖；浏览器实测
- **I18N 彻底干净**：浏览器核对抓 4 组件 bug（region-select 中文残留/useMemo 缺 t/storage 分区 5 处后端 label 直显——新增 storagePartitionLabel 枚举映射）+ ja 精修 34 处（份→件/透传→透過/风控→リスク管理等）+ 术语统一 31 处（資格情報→認証情報 AWS 惯例等）+ 联网核对（レート制限/クールダウン/認証情報/利用状況 均符合主流）+ 三语 2344 键一致
- **最终部署**：sha 88270616（build_sha=final），healthz 全绿

---

## 调度机制真实链路 smoketest（2026-08-16 22:1x-22:3x UTC，线上 sha 88270616）

> 执行：nbus 真实流量 40+2 次（max_tokens=8）+ 历史日志审计。证据：usage/recent + traces.db + journalctl + /api/admin 全端点。

### 实测结论速览
1. **选号分布：恒选 #3 cursorapi**（30/30 @1s + 10/10 @100ms + opus/gpt 各 1）——但 usage 只记最终成功号 #2，真实首选是 #3 且每请求先吃一跳 502（上游号池空）。**master plan S1（starved）预期"恒选第一个号"证实，但实际是死号恒选，比 starved 更严重**。
2. **429 链路实测**（pigcode #4 现场 + 历史 #1 周额度）：透传 429/5xx → 吸收层（**线上开**）同号退避 3 轮 → failover → 全失败落 Kiro → consecutive_pool_unavailable → **503+RA10**（非 429，刻意）。
3. **冷却机制线上全关**（cooldownEnabled=false）：`cooldown_custom_api` 被门控（token_manager.rs:3434-3442）→ 401/429 的 5s/180s 冷却从未生效，但日志打印"该号冷却 180s"（误导性日志）。
4. **RPM 计数准确**（insights rpm=4 ↔ 60s 内 4 请求），但 rpmLimit=25 是全局默认（credentialRpmLimit=0）。
5. **会话聚合正确**：同 user_id → session_id 原样透传（无脱敏），usage/clients+machines 归组正确；无会话头 → session_id=None（透传路径无随机 UUID 兜底，与 S6 研究"随机兜底 38.8%"矛盾——那是 Kiro 主路径行为）。
6. **outcome 分布（1565 条）**：success 1381 / rate_limited 154 / other_error 30——无聚合端点，S7 缺口确认。
7. **镜像自检通过**：启动日志"21 个进程镜像全部接线"（seq 13，info 级）；healthz build_sha=final。
8. **配置**：absorb 线上 ON（upstreamRetryAbsorbEnabled=true，研究文档"默认关"前提不成立→S3 优先级重估）；cooldownEnabled=false；customApiFirst=false；inboundRpmAuto=true。

### 新发现（相对 master plan 增量）
- **N1（MAJOR）**：透传池无白名单的号（#3 cursorapi，credentials.json 无 allowedModels 字段）恒被选中且上游恒 502 → 每请求白打一跳。根因链：5xx 不冷却（provider.rs `_ => 0`）→ 无 starved 位 → rpm 双记（#3+#2 每请求各 record 一次）排序恒平局 → min_by_key 恒选 Vec 第一个（#3 在 entries 顺序 [3,2,1,4] 的头部）。**补 starved 救不了**（每请求都更新 last_selected_at）。
- **N2（MAJOR）**：日志-行为不一致：`cooldown_custom_api` 被 cooldownEnabled=false 门控，但 provider.rs:2074-2077 打印"该号冷却 180s 并 failover"——线上实际零冷却，401 死号每请求重撞。
- **N3（MINOR）**：gpt-5.6-sol/opus-5 请求会撞上无白名单的 #3（上游号池空 502），验证了 #3 不限模型。
- **N4（MINOR）**：透传失败链路的 usage 记录 credential_id=null（面板看不出首选号），掩盖 N1。
- **N5（MINOR）**：#2 计数 success_count(477) > request_count(396)（kiro_stats.json）——success 含 failover 后成功？口径需核对。

## W14（2026-08-16）：调度体系实测+修复+简化（CI 2032/0，已部署 ff4ab41e）
- **真实 smoketest 42+ 次**：发现 N1 死号恒选（#3 恒 502 每请求白打一跳——starved 方案救不了）、N2 冷却日志撒谎、N4 透传失败链首选号缺失、N5 reset_count 不对称清零
- **N1 两轮根治**：排序位余温方案复测失败（高频 RPM 维度压过）→ 过滤级硬排除（60s 余温不参与选号）→ 复测达标（高频 15 次 0 撞，探测复活 3 循环实测）
- **N2**：冷却日志诚实三档（真冷却/未启用明说/5xx 记余温）+ cooldown_custom_api 返回 bool
- **N4**：first_attempted_credential_id 全链（透传首跳→SharedRetryBudget→fail_record+成功链）+ trace_db 迁移 + 8 测试
- **N5**：reset_count 只清 request_count 不对称 → 成对清零 + 测试
- **调度模式三按钮**：schedulingMode（smart/stable/manual）映射 ThrottleProfile Direct/Shielded/Manual + 33 字段矩阵 + 前端按钮/确认框 + 兼容策略（旧配置不重写旋钮）+ 3 测试
- **研究 6 份**：scheduling-rpm/cooldown/429/balance/session/audit-research.md + master-plan.md（S1 starved/S2 RA 断层/S3 最早 429 等 8 项真实缺口待后续批次）
- **实测验证**：S1 偏斜 ✓（对象 #3 非 #2）、S2 RA 断层 ✓、S3 前提失效（吸收层线上已开）、S7 outcome 缺口 ✓、RPM 计数准确、镜像自检通过

## W14-2（2026-08-16）：S2 上游 RA 透传 + S3 最早类型化 429 保留（master-plan 批次 C，未部署未 CI）
- **S2**：上游显式 Retry-After 进客户端响应（A7 决议链：`upstream_retry_after=N` 真值 > 配置 > 8s）。
  provider 429 分支把解析出的 RA 打成网关自己的 marker 拼进错误串（provider.rs:4205）；
  handlers A7 分支读 marker（handlers.rs:2365-2386）+ A5 `_cfg_ra` 改为读（:2298，注释-行为矛盾修复）。
- **S3**：重试链首个上游 429 的 RA 保留（`first_upstream_429_retry_after`，.or() 首个带值胜出），
  终态经 `assemble_final_error`（provider.rs:4735 纯函数）并入 marker；限定：吸收耗尽 503/永久态/
  配额/背压/已带真值不转换，仅 last_outcome ∈ ServerError/OtherError/RateLimited 的 generic 终态转换。
- **新增测试 9 个**：handlers 4（真值>配置>默认、5xx 终态→429、配额 H2 不破、parse 边界）+ provider 5（assemble 行为集）。
- **S2 统计结论**：nbus 无上游 RA 分布数据（upstream_trace.jsonl 从未写出——线上 4 号全透传池，Kiro 路径不打真实上游；
  traces.db 不存 RA 字段；usage 158 条 rate_limited 全是池不可用 retry_after_secs=10，非上游 RA）。
  沿用既有 clamp：A7 clamp(1,300) 兜底，无需新 cap。HTTP-date RA 解析（研究 S3 提及）未做，留待后续。
- ⚠️ 未跑 cargo（任务约束），未部署。验证走 skiapi 验证循环；文件独占：provider.rs / handlers.rs / error_messages.rs 本次被改。
