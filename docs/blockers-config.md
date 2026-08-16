# 配置/序列化层绊脚石清单（docs/blockers-config.md）

> 2026-08-15 研究落盘。目标：找出导致「配置不生效、误读、前后端契约漂移」的绊脚石。
> 全部结论带源码行号（工作树当前状态）。只研究不改代码。
> 六项研究清单逐项产出；重点 = 「配置在说谎字段清单」。
> **闭环状态（2026-08-16 核验）**：本文档列出的待修项已全部修复——错误码可配置化（W7-W8，42 key
> 表 + 热加载 + 校验）、otaAutoCheck 后端补接线（W10-W12 #2，restart-only + 契约测试）、
> 其余说谎字段均已在对应波次修复。正文保留为审计快照。

## 0. 一句话结论

- **Config 层「无 rename 被静默忽略」的现役字段 = 0 个**：`Config` 有 struct 级
  `#[serde(rename_all = "camelCase")]`（src/model/config.rs:107），全部字段自动接受
  camelCase；嵌套结构（ModelPrice/CompressionConfig/ErrorMessageOverride/
  DeepseekNormalizeConfig）也全部有 camelCase rename。`prompt_cache_enabled` 的历史
  事故已被 `rename_all` 兜住，并有默认值守卫测试钉死。**用户案例 2 的字段级问题
  已不存在。**
- **同款「配置在说谎」转移到了三个新位置**：① API 层静默丢字段（`otaAutoCheck`
  现役实锤）；② 文档承诺 vs 实现漂移（errorMessages「传 {} 清空全部」、example.json
  白名单注释、`customApiFirst` 热更注释）；③ 死配置（`promptCacheTtlSeconds` 无读取点）。
- **restore 表漏 5 项**：`corsAllowedOrigins/ipAllowlist/trustForwardedHeader/
  ingressRateLimitPerMin/maxBodyBytes` 在 restart_fields 但不在 reload_config 的
  restore 表 —— 混改触发 reload 时快照说谎（proxy split-brain 同型，历史踩过）。
- **后端契约整体健康**：UpdateConfigRequest/ConfigSnapshotResponse 全 camelCase、
  前端 api.ts 全 camelCase、error_messages per-key merge 前后端逐字对齐（好先例）。
  漂移集中在「前端有 UI 后端不接」「注释与实现不符」「面板摸不到的字段」三类。

---

## 1. 配置在说谎字段清单（重点）

### 1.1 Config 层：现役 = 0，历史 = prompt_cache_enabled（已修，有守卫）

| 字段 | 状态 | 证据 |
|---|---|---|
| `prompt_cache_enabled` | **已修**。曾「全仓零读取点 + 配置在说谎」，现：默认值 true（default_prompt_cache_enabled，config.rs:1414-1427 自述病史）、TIER3 接线完整（service.rs:4331-4338, 5112-5115）、快照/请求双侧都有（types.rs:1136/1355）、前端有 UI | 守卫测试 `absorb_config_default_goes_through_default_fns`（config.rs:2010） |
| 其它 Config 字段 | 无同类问题：struct 级 `rename_all = "camelCase"`（config.rs:107）全覆盖；嵌套结构各带 rename（config.rs:89/1119、error_messages.rs:53、deepseek_normalize.rs:47） | 逐字段核对 100+ 字段 serde 属性 |

**残余风险（机制性）**：`Config` 反序列化**无 `deny_unknown_fields`** —— 未知键静默
忽略、不告警。线上 config.json 的 114 键全部合法（已逐一对照），但未来任何拼错
键名的配置都是「保存成功但不生效」，且无任何日志提示。建议加「未知键告警」。

### 1.2 API 层：现役 1 个实锤 —— `otaAutoCheck`

| 位置 | 证据 |
|---|---|
| 前端 UI | settings-page.tsx:2273-2285（OTA 自动检查 Card + Switch，可操作） |
| 前端 diff 提交 | settings-page.tsx:1975-1976（`d.otaAutoCheck = form.otaAutoCheck`） |
| 前端类型 | api.ts:586（快照必填 `otaAutoCheck: boolean`）+ api.ts:749（更新请求字段） |
| 前端注释 | settings-page.tsx:343-346 自述「后端两侧都已有该字段」——**假设错误** |
| **后端快照** | ConfigSnapshotResponse（types.rs:1072-1305）**无 otaAutoCheck**（rg 零命中） |
| **后端更新请求** | UpdateConfigRequest（types.rs:1314-1479）**无 otaAutoCheck**（rg 零命中） |
| 后端 Config | config.rs:1077-1083 有 `ota_auto_check`（落盘字段）+ example.json 有键 |

后果：UI 开关保存 → toast「已保存」→ serde 静默忽略未知字段 → 永不生效；且快照
不下发 → 前端 `(c as ConfigWithCache).otaAutoCheck ?? false` 恒 false → 开关恒关、
重启后回弹。**与用户案例 2（promptCacheEnabled）完全同型，现役未修。**

修法（选一）：① 后端补接线（快照 + UpdateConfigRequest + service.rs merge 分支 +
`restart_fields`——OTA 检查是 main.rs 启动期 spawn，需重启，注释见 config.rs:1075）；
② 或前端移除该 Card。推荐 ①，config 侧字段与 example 注释都已存在。

### 1.3 文档承诺 vs 实现：3 个「配置在说谎」变体

| 项 | 文档说 | 实现做 | 位置 |
|---|---|---|---|
| errorMessages「传 {} 清空全部」 | 设计文档 §六 + example.json:86 注释 + UpdateConfigRequest 注释（types.rs:1474）承诺「传 {} = 清空全部覆盖回落到内置默认」 | **`{}` 是 no-op**：merge 分支遍历空 map，`merged == config.error_messages` → 不置 changed 标志 → 回「无改动」（service.rs:4959-4977） | 用户案例 5 根因 |
| `custom_api_first` 热更承诺 | config.rs:718 注释「TIER1 热重载即时生效」 | 不在快照/更新请求（面板摸不到）；手改 config.json **不触发 reload**（reload 只由面板保存触发）→ 实际「手改 + 重启（或碰巧面板保存）」才生效 | config.rs:719-720 |
| `prompt_cache_ttl_seconds` 死配置 | config.rs:744-748 注释诚实声明「无实际读取点，改了不影响行为」 | 改配置无效果，UI 无提示、example 无提及 | config.rs:749-750 |

### 1.4 面板摸不到的 Config 字段（UpdateConfigRequest 缺失，~17 个）

只能手改 config.json；其中部分热更（有 reload 触发时）、部分重启：

| 字段 | 备注 |
|---|---|
| `ua_version_fetch` | 启动期读取，重启生效（合理） |
| `auth_region` / `api_region` | 无 UI |
| `count_tokens_api_url/api_key/auth_type` | 无 UI |
| `endpoints` | 无 UI（快照有 endpointNames 只读） |
| `deepseek_normalize`（全局） | 凭据级有 UI，全局无 |
| `prompt_cache_ttl_seconds` | 死配置（见 1.3） |
| `custom_api_first`（全局） | 凭据级有 UI，全局无 + 热更注释说谎（见 1.3） |
| `usage_enabled/data_dir/retention_days` | 无 UI |
| `upstream_trace_enabled/path/max_bytes` | 无 UI（诊断期开关，合理但不可达） |
| `alert_webhook_url` / `alert_cooldown_secs` | example 有注释，面板无 |
| `ota_auto_check` / `ota_auto_check_interval_hours` | **前端有 UI（见 1.2 实锤）** |
| `upstream_concurrency_limit` | 快照也没有（types.rs 无） |
| `overload_fallback_model` | 无 UI |
| `trash_retention_days` | 无 UI |
| `model_pricing` | example 有，面板无 UI（成本估算展示用） |

## 2. 默认值 vs 线上实际（10 个关键字段，2026-08-15 ssh nbus 现读）

| 字段 | 代码默认 | 线上实际 | 结论 |
|---|---|---|---|
| `cooldownEnabled` | true | **false** | 语义陷阱组合（config.rs:36-40 有记录）；新装实例与线上行为不同 |
| `upstreamRetryAbsorbEnabled` | false | **true** | 线上主动开（正常）；排障时「默认关」注释会误导 |
| `inboundRpmAuto` | true | **true** | ⚠️ **config.rs:1870-1873 测试注释写「线上刻意设成 false」——注释过期**，线上已是 true。读那条注释做排障会得到错误前提 |
| `promptCacheEnabled` | true | true | 一致（历史说谎字段已修） |
| `otaAutoCheck` | false | false | 一致，但 UI 改了不生效（1.2） |
| `uaVersionFetch` | true | true | 一致 |
| `mockCacheEnabled` | false | false | 一致 |
| `stripEnvNoise` | true | true | 一致 |
| `throttleProfile` | manual | **direct** | 用户切档（正常） |
| `loginBackgroundR18` | false | **true** | 用户开的（正常） |

**「默认值误导」候选**：`upstreamRetryAbsorbEnabled` 代码默认 false 而线上开——
若有人按代码默认推断线上行为会错；`inboundRpmAuto` 的过期测试注释是更直接的误导
源。其余 8 项默认值与线上一致或差异有据。

## 3. 热重载分类一致性

### 3.1 TIER1/TIER2/TIER3 接线完整性：全部闭合

- TIER1（ArcSwap）：冷却/限流/亲和/RPM/吸收层/模型映射/错误码表/CLI 三开关/
  load_balancing_mode 等 —— reload_config（token_manager.rs:2731-2782）的原子镜像
  store 与 service.rs 的 `hot_changed` 分支覆盖一致 ✓
- TIER2（respawn）：proactive_token_refresh 三件套 + balance_refresh_interval_secs
  （service.rs:4873-4899 → 5064-5070）✓
- TIER3（handlers 镜像）：extract_thinking/cc_auto_buffer/prompt_cache/mock_cache/
  strip_env_noise/native_effort/tool_* 全部有「main.rs 或 router.rs 播种 + service.rs
  热更」双调用点 ✓（播种分散在 main.rs 与 anthropic/router.rs:60-72 两处，风格问题
  非功能问题）
- `set_error_messages`：main.rs:593（播种）+ token_manager.rs:2743（reload）= 2 个
  调用点 ✓（用户案例 4 的「只接一半」已修复）

### 3.2 ⭐ restore 表漏 5 项（split-brain 同型，P1）

`restart_fields`（service.rs:4142-4871 push 的 18 项）与 reload_config 的 restore 表
（token_manager.rs:2697-2729，14 项）差集：

| 在 restart_fields | 在 restore 表 | 消费点 |
|---|---|---|
| host/port/region/kiroVersion/systemVersion/nodeVersion/tlsBackend/defaultEndpoint/proxyUrl/proxyUsername/proxyPassword/callbackBaseUrl/apiKey | ✓（14 项） | — |
| **corsAllowedOrigins** | **✗** | anthropic/router.rs:115 build_cors_layer（router 构造时固化） |
| **ipAllowlist** | **✗** | common/security.rs:362（SecurityState 构造时固化） |
| **trustForwardedHeader** | **✗** | security 中间件（启动固化）+ 业务层镜像（main.rs:533 播种） |
| **ingressRateLimitPerMin** | **✗** | common/security.rs:364 IngressRateLimiter（启动固化） |
| **maxBodyBytes** | **✗** | anthropic/router.rs:106-109 DefaultBodyLimit（router 构造时固化） |

后果：这 5 项与任一热字段**同批提交** → reload_config 触发 → ArcSwap 换入新值 →
快照/导出/前端基线显示新值，而真实运行态（tower layer / security 中间件）仍是
旧值直到重启。前端虽提示「需重启」，但**快照已经说谎**——与 proxy split-brain
（token_manager.rs:2690-2696 注释）完全同型，正是 restore 表要防的形态。
修法：reload_config 的 restore 块补 5 行（同 `new.proxy_url = old.proxy_url.clone()` 样板）。

### 3.3 热更语义不对称（P3）

- `ip_blocklist`/`machine_code_blocklist` 立即生效（业务层镜像，service.rs:4826/4850），
  但同组 `ip_allowlist` 需重启（仅中间件固化，service.rs:4808）——同一「反代安全」组
  两套热更语义，注释自述是架构遗留（service.rs:4824-4826）。
- `trust_forwarded_header` **已有镜像 setter**（handlers.rs:66 + main.rs:533 播种 +
  测试），但 service.rs:4854-4858 热更分支只 push restart_fields、不调 setter——
  接线只接一半：镜像存在却不用，改它必须重启（行为没错，浪费已有机制）。

### 3.4 「需要重启但 UI 没说」

- restart_fields 全部经 UpdateConfigResponse 下发、前端 toast 提示（settings-page.tsx:2125-2127）✓
- 但 UpdateConfigRequest 的 doc 注释（types.rs:1311）「除 load_balancing_mode 立即生效外，
  其余字段需重启进程后生效」**全面过期**——几十个字段早已热更。读代码的人会被误导。
- 面板摸不到的字段（1.4 清单）无提示义务，但其中 `ota_auto_check` 前端有 UI 却
  无任何「需重启」提示（因为后端根本不接）。

## 4. 镜像同步点（set_* 调用点核对）

| setter | 播种 | 热更 | 状态 |
|---|---|---|---|
| set_error_messages | main.rs:593 | token_manager.rs:2743 | ✓（历史「只有测试调用」已修） |
| set_mock_cache_config | main.rs:584-587 | service.rs:5120-5124 | ✓ |
| set_extract_thinking / set_cc_auto_buffer / set_strip_env_noise / tool_*（9 个） | anthropic/router.rs:60-72 | service.rs:5104-5153 | ✓ |
| set_prompt_cache_enabled / set_native_thinking_effort_enabled / set_collect_client_fingerprint / set_login_background_* | main.rs:496/527/575/579 | service.rs:5074-5115 | ✓ |
| set_ip_blocklist / set_machine_code_blocklist | main.rs:536/538 | service.rs:4826/4850 | ✓（merge 分支内联调，风格不同） |
| set_tool_reclaim_textified_invoke / set_tool_stray_repeat_guard | main.rs:566/567 | service.rs:4400/4407（**merge 分支内联调**） | ✓ 但接法与其它 tool_*（changed 标志 + 尾部统一调）**两种风格并存**，加新字段时易抄错样板 |
| set_trust_forwarded_header | main.rs:533 | **无**（只 restart_fields） | ⚠️ 只接一半（见 3.3） |

## 5. 前端契约（api.ts 类型 vs 后端响应结构，10+ 端点抽查）

字段名全部 camelCase 且前后端一致 ✓（ConfigSnapshotResponse / UpdateConfigRequest /
UpdateConfigResponse / credentials / usage 已核对）。**没有「前端用错字段名」的
camelCase 问题**。发现的是以下 5 类：

1. **otaAutoCheck 前端有、后端无**（1.2 实锤）——前端类型补丁（settings-page.tsx:347-358
   `ConfigWithCache`/`UpdateWithCache`）基于「后端已有」的错误假设写成，且补丁未清理：
   api.ts 已补 promptCacheEnabled（api.ts:530/710），临时别名仍在用 → **双份类型漂移风险**。
2. **注释错位**：`selfHealBaseBackoffSecs` 挂着「prompt cache 记账下发」的注释——
   后端 types.rs:1130-1131 + 前端 api.ts:523-524 **两处同款错位**（prompt_cache_enabled
   的真正注释被挤到 1136/530）。读代码者会被误导成「self_heal 是缓存开关」。
3. **命名与逻辑倒置**：`upstreamRetryAbsorbExhausted503`（settings-page.tsx:409）取值
   `=== 429`，diff 时 `? 429 : 503`（1928）——字段名说「503」、逻辑判「429」，语义反着读。
   行为正确（双向对称），但读代码的人必踩。
4. **白名单集不一致（文档侧）**：前端 TYPE_OPTIONS（8 类，error-messages-dialog.tsx:50-59）
   与 STATUS_OPTIONS（10 项含 504，:62）**与后端一致** ✓；但 example.json:93-95 注释写
   「status 白名单 [400,401,403,404,413,429,500,502,503]」缺 504、「type 白名单（官方
   9 类 + quota_exceeded_error）」——与实现（service.rs:538 ERROR_STATUS_WHITELIST 含
   504；:554 ERROR_TYPE_WHITELIST 仅 8 类，billing/quota_exceeded 均不可配）不符。
   **用户按 example 注释配置 quota_exceeded_error → 400 整表拒绝**。
5. **errorMessages 弹窗是好先例**：buildDiff 只提交脏 key、空对象 = 删 key、全量非空
   字段提交——与后端 per-key merge（service.rs:4959-4977）逐字对齐 ✓（含 1.3 的
   「{} 整体清空」语义缺口——弹窗从不发整体 {}，只影响外部 API 调用者）。

## 6. config.example.json vs Config 字段差集

example 17 键。**缺失但线上实际在用**（新装用户从 example 出发不知道存在）：
`throttleProfile`（线上 direct）、吸收层十项（线上开）、`modelMapping`、
`promptCacheEnabled`、`stripEnvNoise`、冷却相关、`ipAllowlist/ipBlocklist`、
usage 三件套等。example 的 `errorMessages` 注释（:93-95）白名单过期（见 5.4），
且注释承诺「传 {} = 全部用内置默认」与实现 no-op 矛盾（见 1.3）。

---

## 7. 最值得先动的 3 个绊脚石

### ⭐ 1. `otaAutoCheck` 后端接线缺失（P0，现役「配置在说谎」）

UI 有开关 → 保存成功 → 后端静默丢弃 → 永不生效、刷新回弹。与已修的
promptCacheEnabled 事故完全同型，且前端注释自以为「后端已有」。
**修法**：后端补 3 处（ConfigSnapshotResponse + UpdateConfigRequest +
service.rs merge 分支进 restart_fields）；前端删临时补丁（api.ts 已含字段）。
改动 ~20 行，有完整先例可抄。

### ⭐ 2. reload_config restore 表漏 5 项（P1，split-brain 同型）

`corsAllowedOrigins/ipAllowlist/trustForwardedHeader/ingressRateLimitPerMin/
maxBodyBytes` 混改触发 reload 时 ArcSwap 显示新值、运行态旧值——快照说谎，与
proxy split-brain（token_manager.rs:2690-2696 已根治的历史）同型，restore 表
正是为防它而建的。
**修法**：token_manager.rs restore 块补 5 行 `new.X = old.X.clone()`。

### ⭐ 3. errorMessages「{} 清空全部」文档-实现漂移（P1）

设计文档 §六 + example.json + UpdateConfigRequest 注释承诺「传 {} = 清空全部
覆盖」，实现是 no-op。外部脚本/API 调用者按文档做「一键清空」会得到「无改动」。
**修法（二选一）**：① 实现侧补语义——`error_messages: Some(empty_map)` 时整表置空
（注意与「空条目 = 删单 key」区分，per-key merge 已定义空条目删 key，整体 {} 清空
不冲突）；② 或改三处文档注释。推荐 ① + 同步 example.json 白名单注释（8 type /
10 status 含 504）。

---

## 8. 自 review（可验证性）

| 结论 | 证据 | 验证方式 |
|---|---|---|
| Config 层无 rename 缺失字段 | config.rs:107 rename_all + 100+ 字段逐一核对 serde 属性 | 已读 config.rs 全文件（2366 行） |
| otaAutoCheck 后端无字段 | rg "otaAutoCheck" src/admin/types.rs src/admin/service.rs 零命中 | 已跑 |
| 线上 114 键全部合法 | ssh nbus 读全键列表 vs Config 字段清单 | 已逐一对照 |
| restore 表 5 项缺失 | token_manager.rs:2697-2729（14 项）vs service.rs restart_fields（18 项） | 已读两处 |
| 前端白名单与后端一致 | error-messages-dialog.tsx:50-62 vs service.rs:538/554 | 已读两处 |
| errorMessages {} 是 no-op | service.rs:4959-4977（空 map 遍历后 merged==config） | 已读 |
| set_error_messages 双调用点 | main.rs:593 + token_manager.rs:2743 | rg 全仓 |

未验证项（诚实披露）：
- 未跑任何测试/构建（只读任务）；行号基于当前工作树，改动后需重核。
- 「混改触发 reload 后 CORS/body limit 旧值」是按代码路径推演（消费点均为构造时
  固化），未做运行时验证。
- 线上 config.json 的 `alertWebhookUrl` 键不存在（未配置），未列入默认值对比。
