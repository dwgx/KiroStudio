# Changelog

本项目版本变更记录。遵循语义化版本(SemVer)。

## [0.7.17] - 2026-07-14

### GPT-5.6 三变体接入（Kiro 2026-07 新增）
- `model_catalog.rs` 新增 `Family::Gpt` + 三个并列变体 `gpt-5.6-sol` / `gpt-5.6-luna` /
  `gpt-5.6-terra`（各带连字符/无点/大小写别名）。走**精确别名命中**范式（与国产模型一致，无语义
  版本号）。故意不做 `contains("gpt")` 子串探测——`gpt-5.6` / `gpt-4` 这类无变体名会被 strict 拒绝，
  逼客户端指明 sol/luna/terra（与 `auto` 同理，防误命中 + 乱计费）。`/v1/models` 自动派生广告，
  前端 `PROBE_MODEL_CATALOG` 补三条（测活/号池白名单可选）。
- **诚实边界**：credit_mult / context_window 暂用保守占位（1.0x / 200K），待 Kiro 官方权威值校正；
  这两个字段只影响计费展示与 max_tokens 广告，不影响模型能否用。上游是否真接受该 modelId 需真机验证。

### 运维可观测三件套（对比并行项目 WindsurfAPI 调研后借鉴）
- **自愈机器可观测（recovery-metrics）**：新增 `common/recovery_metrics.rs` 进程级原子计数器
  （刷新 ok/fail、failover 换号跳数/耗尽、自动禁用死号、风控冷却触发、region 重探 ok/fail、
  泄漏 token 清洗/整段退化）。各处自愈事件埋点，`GET /admin/recovery-metrics` 一次性导出快照
  （含 uptimeMs）。**不持久化**（自进程启动的健康信号，重启归零）。把刷新/failover/清洗机器从
  黑箱变成可查。前端「运维」页自愈计数卡片（warn 类计数 >0 标琥珀色）。
- **实时日志流 + 一键导出**：新增 `common/log_buffer.rs` 自定义 tracing Layer → 有界环形缓冲
  （1000 条）+ broadcast 实时推送，与 fmt 层共享同一 EnvFilter。端点 `/admin/logs`（增量拉取 +
  级别过滤）、`/admin/logs/stream`（SSE 回放 + 实时直播）、`/admin/logs/export`（JSONL 下载，
  直接附 bug 报告）。前端运维页日志查看器：级别过滤 / 实时暂停（断连自动重连 + 如实反映连接态）/
  一键导出。自托管场景下运维不必 SSH/grep。

### 原子写重构（config.json 崩溃截断防护）
- 抽 `common/fs_atomic.rs` 共享单一真相源：temp → fsync → rename（创建即 0600，无 rename 后设权的
  短 world-readable 窗口）+ Windows 句柄占用（杀软/索引器）的 rename 退避重试（PermissionDenied /
  os error 5,32）。`config.rs Config::save()` 从裸 `fs::write` 改用它——修 adminApiKey / proxyPassword
  明文配置的**崩溃截断丢配置**（写一半崩溃 → 面板密钥丢失锁死管理入口）+ 短权限窗口双风险。
  token_manager 原内联 `write_atomic` 提取到共享模块复用（凭据/回收站持久化行为不变）。

### 诚实边界（实测推翻的假设）
- 「客户端断开→取消贯穿 failover」经旁挂实测**证明现状已正确**：axum/hyper 在客户端断开时会主动
  drop 正在执行的 handler future（即便还在 failover 的 `.await` 点），Rust 协作式取消让 failover
  循环 / inflight 守卫 / 上游 reqwest 级联取消。故**未改代码**（drop 级联已在做正确的事）。

双特性各 635 绿（+8 新测）+ 前端 build 绿 + 对抗性 review（5 维 + 复核）。

## [0.7.16] - 2026-07-14

### region 选择器统一复用（设置页那套下拉铺到三处上号 region 输入）
此前三处 region 输入各不相同：设置页「区域 region」已是带搜索/键盘选择/自由输入的
`RegionSelect`，而凭据卡片自定义 region、IdC 上号 Region 还是裸 `Input`，微软 SSO 干脆
没有 region 字段。本版把 `RegionSelect` 统一铺到三处，并加「最近使用区域」智能复用。
- **最近使用区域（跨入口全局共享）**：`lib/regions.ts` 新增 `getRecentRegions`/
  `pushRecentRegion`（localStorage 存最近 5 个，去重 + 最新置顶，正则形状校验防脏值，
  坏 JSON 安全降级）。`RegionSelect` 下拉打开且无搜索词时顶部展示「最近使用」分组，
  采用任一 region 自动写回历史——设置页 / IdC / 微软 SSO / 凭据卡片自定义切换，任一入口
  填过的 region，下次在任何入口都能一键复用。
- **三处接入**：凭据卡片自定义 region 切换、IdC 上号 Region、微软 SSO 新增「优先探测区域
  （可选）」。三处外观统一（凭据卡片紧凑场景经新增 `triggerClassName` 与邻接按钮齐平）。
- **微软 SSO region 后端真生效（优先探测）**：微软号 region 是授权后按候选表探测发现的。
  本版把用户填的优先区域从 `start` 贯穿到 leg2 探测（`StartExternalIdpLoginRequest.region`
  → session → `list_all_available_profiles` → 新增 `merge_probe_regions`：优先区域排头、
  并入 `PROFILE_PROBE_REGIONS` 去重），让只在冷门 region（如 eu-central-1）开通的账号也能
  被探到，不再漏掉。**安全**：优先区域在 `start` 时先经 `is_supported_region` 白名单过滤
  （非法值忽略退回默认候选），`merge_probe_regions` 仅决定探测顺序，出站 host 仍由白名单
  region + ARN 严格解析构造，SSRF 铁律未破。

### 交互回归修复（对抗性 review 查出）
- **commit-on-close（防丢字）**：`RegionSelect` 是提交式 combobox，搜索框逐键只更新内部
  状态、仅在回车/点选时才回写外部值。裸 `Input` 换成它后暴露一处回归：用户在自定义 region
  框键入 `eu-central-1` 后不回车、直接点框外或「切到此区域」按钮 → 已键入内容被丢弃、外部值
  仍是旧值 → 表现为「明明填了 region，切换按钮却是灰的」。现改为关闭下拉时提交已键入的
  **合法形状 region code**（点框外 / 点触发按钮均生效），搜索关键词（如「东京」未解析成 code）
  不回写以免污染。
- 自由输入 region 归一化小写（`EU-CENTRAL-1` → `eu-central-1`），对齐后端白名单精确匹配。

诚实边界：微软 SSO「优先探测区域」仅影响探测顺序/覆盖面，最终 region 仍以授权后验活可用的
profile ARN 为准（账号未在该 region 开通则探不到，符合预期）。
双特性各 624 绿（+3 merge_probe_regions 测：无 preferred / 冷门排头 / 大小写去重）+ 前端 build 绿。

## [0.7.15] - 2026-07-14

### 泄漏 token 清洗诊断（清洗不再是黑箱）
此前 `clean_leaked_tokens` 剥掉 #70544 幻觉 token（court/course/count/care/card/call/課/课）后
**直接返回，零计数、零日志、零诊断**——剥了多少、命中什么，用户和运维都看不到。本版补上可观测：
- `StreamContext` 加泄漏计数器：`leaked_stripped`（真剥掉数）+ `leaked_saturation_lines`（整行就是纯
  泄漏词的行数，#70544 整段退化信号）。`strip_leaked_prefix` 返回命中信息，`clean_leaked_tokens` 累加。
- 收尾 `generate_final_events` 若本请求清洗过泄漏 token / 命中 saturation → `tracing::warn` 如实记一条
  （含 model + 清洗数 + saturation 行数 + 归因提示：saturation=模型侧整段退化，网关仅能缓解不能根治，
  建议该模型高多字节上下文场景 /clear 或换 sonnet）。
- 新增 `KIRO_LEAK_TRACE` 环境变量探针（仿 `KIRO_TOOL_TRACE`），开启时收尾打印本请求泄漏清洗全貌，
  平时零开销。
- **剥离判据完全不变**（0.7.14 已收严：CJK/全角粘连才剥、正常英文 count:42/countDown() 绝不误删、
  court/課/课 独占整行特例真删）——本版纯加观测，不改任何已发内容。
- **诚实边界**：清洗只作用于**流经网关的上游文本**（Bug B 模型侧现象，网关能缓解流经它的下游泄漏，
  但对不经过网关的场景物理上无能为力）。

## [0.7.14] - 2026-07-14

### 上号智能诊断系统（无论谁的错都给正确引导）
上号是本项目最关键路径，历来反复翻车的共性不是某个 bug，而是**出错时说不清"是账号的问题
还是网关的问题"，用户只看到裸 502 / 裸报错，不知道该干什么**。本版建立结构化诊断，把"出错了"
升级成 **(哪一步 + 谁的错 + 具体 code + 该干什么)**。

- **新增 `src/kiro/diagnosis.rs`**：`OnboardingDiagnosis`（stage / fault 归因 / code / summary /
  有序 guidance / raw 折叠 / retriable）+ 纯函数 `diagnose_*` 规则库，收录本轮**真实 token 实测**
  坐实的错误模式。归因分五类：`user_input`（用户填错）/ `account_state`（账号问题）/ `upstream`
  （AWS 侧）/ `gateway`（网关未覆盖，诚实标注 + 附原文 + 提示反馈）/ `transient`（瞬时）。
- **修 #98 刷新 502 的真因**：实测 #98 刷新返回 `400 invalid_request "Invalid token provided"`
  （不是我此前以为的 region 错配——#98 全是 us-east-1）。旧代码只识别 `invalid_grant/Invalid refresh
  token` 精确组合，#98 措辞不同 → 落兜底 → 裸 502。现在归 `CLIENT_OR_TOKEN_MISMATCH`（client 注册
  约 90 天过期或与 token 不匹配）+ 引导「重新上号」。**诚实说明**：这是"归因+引导"修复——refresh_token
  真失效只能重新上号，但用户会清楚知道原因，不再对着裸 502 懵。
- **诊断贯穿链路**：`refresh_idc_token` / IdC device flow 全 region 失败（`REGION_MISMATCH`）→
  `DiagnosedError` → `classify_balance_error`/`start_idc_login` **downcast 透传**（不再字符串关键词
  匹配丢结构）→ `AdminServiceError::Diagnosed` → API 响应 `error.diagnosis` 结构化字段。
- **前端诊断卡片**（`DiagnosisCard`）：归因徽标 + 一句话诊断 + 有序引导步骤 + 折叠原始信息 +
  按需「重试」/「重新上号」按钮。接入 IdC 上号对话框 + 凭据卡片刷新。

### 自定义 region（都要）
- 凭据卡片「Profile ARN 区域」块新增**手填 region 输入**：用已探测候选的 account + profile 名构造
  目标 region 的 ARN 直接切换（绕候选表，覆盖冷门 region），验活可用才真生效（后端 switch 只在
  Usable 写回）。空探测结果时提示可能未开通（24h 传播）或手填其它 region。

### 诚实修正（本轮实测推翻的假设）
- 用 #98 真实 token 直打 AWS `ListAvailableProfiles`：eu-central-1 返回 `{"profiles":[]}`（**EU 确实
  无 profile**），us-east-1 有。此账号目前只在 us-east-1 开通，探测无 bug。0.7.13 的 auth_region 错配
  修复对 #98 无效（它不存在错配），真因是 client 凭证——本版归因修对。

## [0.7.13] - 2026-07-13

### 修复 IdC 号刷新 Token 502（0.7.12 收口引入的回归）
- **根因**：IdC 号有两个物理不同的 region——`auth_region`=SSO-OIDC 实例所在区域（clientId/secret/
  refreshToken 在此注册，刷新 token 必须打 `oidc.{auth_region}.amazonaws.com`），与 profileArn 的
  region（对话 `runtime.{R}.kiro.dev` / 余额 `management.{R}.kiro.dev` 用）不同。0.7.12 加的
  `sync_region_from_arn` 收口铁律会把 `auth_region` 也一起同步成 ARN region → 刷新打到错的 OIDC 端点
  → clientId 跨 region 失配 → AWS 拒 → 网关映射成 502。表现为「余额/对话正常、唯独刷新 Token 502」，
  且 access_token 过期后会使该号变废（定时炸弹）。
- **修复**：`sync_region_from_arn` 对 IdC 号**豁免 auth_region 改写**（只同步 `region` 供对话/余额，
  `auth_region` 保留上号 device flow 探测的 SSO-OIDC 区域）。新增 `is_idc_credential()` 判据。
  external_idp 的 auth_region 不参与刷新（用微软 token_endpoint）、social 走 kiro.dev，故仅 IdC 需豁免。

### IdC 号放开 region 手动切换 / 自动探测
- `probe_regions_for` / `switch_profile_region_for` 从「仅 External IdP」放开到 **External IdP + IdC**
  （排除 social/api_key/custom_api）。底层探测对 IdC 用纯 Bearer（无特殊 TokenType），与刷新路径已
  在用的多 region ListAvailableProfiles 同源；切换只在验活 `Usable` 时才写回，坏 region 一律 bail
  不动状态，零误切风险。
- 前端凭据卡片设置区对 IdC 号显示「Profile ARN 区域」探测/切换。**诚实文案**：IdC 实例通常绑定单一
  区域，探测多用于确认/重新解析该号 profileArn（一般只返回一个区域），非多 region 选择器。

### region 探测实时反馈（notification）
- 探测过程实时 toast：开始「正在探测…」→ 找到「找到 N 个可用区域」/ 全不可用给明确提示 / 失败给
  **详细错误报告**（后端 bail 的具体原因透传到卡片红框 + toast，不再是裸失败）。

## [0.7.12] - 2026-07-13

### 上号统一治理（根治 region/ARN 反复修补）
- **region 三表归拢单一真相源**（新建 `src/kiro/regions.rs`）：此前 region 表散落三处（对话白名单 /
  profile 探测 / OIDC 探测），加一种认证源就各自新增或漏改 → 反复修补。现按**用途维度**归拢到一处并
  分层命名：`KIRO_DIALOG_REGIONS`（对话/profileArn 白名单）、`OIDC_PROBE_REGIONS`（SSO-OIDC 探测，
  16→25 补全）、`PROFILE_PROBE_REGIONS`（profile 探测，2→6 补全）。三者维度不同**不合并**，原三处
  const 改 re-export，调用点零改动。
- **`add_credential` 收口铁律**：任何号进池那一刻强制 `sync_region_from_arn`——无论来自哪条上号路径
  （external_idp / idc / social）、建号前 region 填得对不对，进池即 region↔ARN 自洽，杜绝错配 →
  400 Improperly formed。无 profileArn 的号（api_key/custom_api）是安全 no-op。

### IdC 登录自动探测 region（防呆）
- IdC start URL（`d-xxxxxxxxxx.awsapps.com`）是全局域名、**不含 region**，用户填错 →
  device_authorization 400 invalid_request。新增 `register_and_authorize_probing`：按「用户填的 region
  打头 + 候选表」顺次试，**第一个 device_authorization 成功的 region 即实例所在 region**，探到即用并
  贯穿 poll/建号/token 刷新。全不成才报可读中文错误（引导查 IAM Identity Center 设置），不再裸抛 400。
  走 AWS 公开 OIDC 端点，与号池无关不烧号。

### Invalid tool parameters 工具容错 5 缺陷修复（完整性核实查出）
- **缺陷1（默认配置即中·时序）**：无 stop 残留截断时，收尾先查 completion 决定补 error、后跑
  `generate_final_events`（残留 flush 才置失败态）→ error 事件漏发，客户端把 `input:{}` 的 tool 块当
  成功。修：收尾**先** flush、**后**据 completion 补 error（/v1 与 /cc/v1 两路径同修）。
- **缺陷2/3（②③开关拆开即漏）**：「②开③关」记账失败却发坏 JSON；「②关③开」吞坏 JSON 记成功。
  修：把「不发坏 JSON」的判据从③开关本身改为**失败态已置**（`!completion.is_ok()`），与失败态绑定，
  消除所有拆开组合的矛盾。
- **缺陷4/5（清洗）**：泄漏 token 清单补 `court`（实测最高频，独占行 202 次）/`card`/`call`，删死条目
  `coursecount`；判据**收严**为「仅 CJK/全角粘连才剥」（此前「非空格非小写即剥」会误删 `count: 42` /
  `countDown()` / `courseCatalog` 等正常英文）；`court`/`課`/`课` 独占整行特例（高置信泄漏）。
- **P2-1（unwrap 解耦）**：`unwrap_double_encoded`（双重编码解包）从 `tool_repair_json` 开关摘出、
  独立恒开——它不改语义、对合法输入 no-op，此前被绑在 repair 开关下，用户关 repair 排查时会连带关掉它。

## [0.7.11] - 2026-07-13

### 文档与注释(无逻辑改动)
- **1M `[1m]` 变体验证结论钉入代码注释**：0713 旁挂 8995 黑盒实测坐实——Kiro 上游(CodeWhisperer
  协议)本就给足远超「官方 200K」的窗口(`claude-opus-4-6` 不带 `[1m]`、不带任何 beta 头,64 万
  token 输入直接返回 200),**不依赖 `anthropic-beta: context-1m-2025-08-07` 头**。故该头注入是
  「保留但无害」,`[1m]` 后缀的实际价值 = 给只能传纯模型名的客户端一个显式 1M 变体名。结论写进
  `model_catalog.rs` `supports_1m` 与 `ide.rs` `BETA_1M` 注释,替换此前「待验证」措辞。
- **新增 Invalid tool parameters 攻坚报告**（`docs/ATTACK-REPORT-invalid-tool-params-0713night.md`）：
  并发攻坚四症状(Invalid tool params / 幻觉 token / 空回复 / mid-response)的可落地修复清单，含
  客户端源码逐行坐实、对抗复核裁决、KILL 死路清单、验证方案、诚实边界。属规划文档，本版未实现其中代码。
- gitignore 补挡 `kiro_stats.json` / `replay.tmp.json` / `scripts/` 三类运行时产物与本地临时脚本。

## [0.7.10] - 2026-07-13

### 安全
- **未知上游错误不再向客户端泄露内部细节**：`map_provider_error` 的未识别错误分支此前把原始错误链
  （`err.to_string()`）直接拼进返回给客户端的响应体，而错误链可能含上游响应体里的 profileArn /
  AWS 账号号 / region / 内部 URL 等敏感信息。现在**完整原文只进服务端日志**（便于排障），客户端只得
  通用提示 + 引导查网关日志，不泄露任何上游内部细节。加泄露回归测试（断言响应体不含 ARN/账号/region）。

## [0.7.9] - 2026-07-13

### region 自动纠正「一条龙」（对话路径补齐——此前只有导入/刷新/手动探测有）
- **对话请求撞 403 FEATURE_NOT_SUPPORTED 时自动纠正 region**：此前对话热路径把该错误当普通凭据
  错误 `report_failure` 冷却 + 换号，**误伤只是 region 配错的好号**（号本身可用，换个 region 就行）。
  现在特判：① 廉价本地纠正 `sync_region_from_arn`（纯字符串，无网络）；② 触发 **per-id 守卫的
  后台异步重探**（`trigger_background_reprobe`：`compare_exchange` 抢占，N 并发只 1 个真探测，
  6h 冷却双检，detached spawn，绝不阻塞当前对话请求）；③ 本地纠正生效则同号重试一次，否则认证冷却
  换号（**绝不 report_failure 连坐**）。非 external_idp 号短路，行为零变化。
- **对抗复核裁决**：昂贵的 `probe_all_usable_profiles`（一整轮 getUsageLimits）**绝不上同步对话
  热路径**（会阻塞客户端数十秒 + 并发打爆上游自造风控），改为后台异步 + 当前请求立即 failover。
- **右键手动切换 region 补「当前」标记**：`ProfileCandidate.current` 标出当前绑定的 profile，
  前端绿标 + 禁点，省一次冗余 switch。

### Invalid tool parameters 补三个漏过的洞
- **非流式路径补 JSON 修复**：此前 `repair_tool_json` 只在流式路径生效，非流式解析失败直接置失败态；
  现在非流式也先修复再复验，与流式对齐。
- **整包双重编码解包**：模型偶发把整个工具参数对象**再套一层字符串编码**（`from_str` 成功但得到
  `String`，漏过修复层），客户端按 object 消费即报 InputValidationError。新增 `unwrap_double_encoded`
  解一层还原（只解一层、复验必 object/array 才用），流式 + 非流式两处接入。
- **孤立/半截 UTF-16 代理对降级**（对应 #69522）：`\uD83D` 等孤立高/低代理会被判非法 JSON，
  修复层降级为字面；合法代理对（如 😀 = `😀`）原样保留不碰。
- **修 repair 成功路径绕过双重编码解包**：修复成功后不再提前返回，与「原本合法」路径汇合到同一
  解包 + 发送出口，消除路径不一致。

### 错误翻译层
- **修 `translate_network` 子串误匹配**：此前对上游错误串裸 `contains("tls"/"proxy"/"timeout"…)`，
  会把响应体里恰好含这些词的**普通上游错误**误判成网络故障（错状态码 + 误导排障）。现在加传输层
  闸门 `is_transport_error`（只认 reqwest 建连/发送阶段的稳定标志），非传输错误不在此翻译、诚实透传。

## [0.7.8] - 2026-07-13

### 1M 上下文变体 + beta header 注入
- **`[1m]` 后缀模型名可用**：客户端传 `claude-opus-4-6[1m]`（部分客户端只能传纯模型名、无法单独
  设置 beta 头）现在能成功。照 `-thinking` 后缀范式，在 `model_catalog::resolve` 最前面剥离
  `[1m]` → 映射到干净的 Kiro modelId（body 里仍是 `claude-opus-4.6`）+ 记 `is_1m` 标志。
- **自动注入 1M beta 头**：命中受支持的 `[1m]` 变体时，`IdeEndpoint::decorate_api` 给上游请求注入
  `anthropic-beta: context-1m-2025-08-07`，上游（若为 Anthropic 直连/透传）才会真启用 1M 窗口。
- **`/v1/models` 广告 1M 变体**：`supports_1m` 的模型（opus 4.6/4.7/4.8、sonnet 5/4.6）额外广告一条
  `<id>[1m]`（显示名带 `(1M)`），客户端可直接选。
- **宽容降级**：不支持 1M 的模型加 `[1m]`（如 `claude-opus-4-5[1m]`）→ 忽略后缀 + 告警，不拒绝；
  未知模型加 `[1m]` → 剥后仍未知即拒。信号经 `RequestContext.is_1m` 透传，Kiro 路径从零构造请求
  不与客户端 header 重复。
- **诚实边界**：Kiro 上游是 CodeWhisperer/Q 协议（非 Anthropic 直连），该 beta 头是否被上游识别
  并真放开 1M 窗口**待旁挂黑盒验证**。header 注入本身无害（上游不认最多忽略），故先落地、再验证。

## [0.7.7] - 2026-07-13

### 工具容错开关默认组合调优
- **①清洗泄漏 token / ②拼装非法对齐失败态 / ③工具错误如实暴露客户端 默认改为开启**（原默认关）。
  配合早前默认开的 ④JSON 修复层，构成完整的「修得好就修（④）、修不好给客户端干净失败信号让其重试
  （②+③）、顺带清洗模型泄漏 token（①）」组合。②③本就该配对——②只标失败态，③才真正不发坏 JSON；
  单开②留③关会导致「修不好的残留仍发坏 JSON、客户端照报 Invalid tool parameters」。均热更、绝不连坐号。
- ⑤截断跨轮恢复保持默认关（改变对话流程，需按需开启）。

### 设置页 UI
- **修长 hint 撑歪开关列**：`Field` 行布局从「标签固定 40% 最小宽 + 开关占剩余」改为「标签弹性占满
  + 开关固定右列」，超长说明（JSON 修复层 / 截断跨轮恢复）不再把开关挤到右边缘、各行开关恢复对齐。
- **拆分臃肿的「客户端伪装」卡**：原 12 项一坨的大卡按语义拆成三张——「客户端伪装」（版本号伪装）、
  「协议与转发」（提取 thinking / CC 自动切协议 / 剥离环境噪音）、「工具调用容错」（6 个工具错误
  处理项，卡头加一句说明）。三张卡仍在「基础」分区，搜索索引同步拆分。

## [0.7.6] - 2026-07-13

### 工具参数错误处理（承 0.7.5 JSON 修复层，补齐用户体验层）
- **上游错误翻译层**：`map_provider_error` 新增 `translate_upstream_error`（纯函数，可测），把已确证
  含义的上游错误翻译成**带排障步骤的中文提示**——覆盖月配额耗尽 / region 未开通
  （FEATURE_NOT_SUPPORTED）/ 订阅失效 / 上下文窗口满 / 输入过长 / DNS / 超时 / TLS / 代理故障，
  每类给「一句诊断 + 分步排障」。未知错误诚实透传原文（不臆造排障步骤误导）。
- **截断诊断归因标签**：工具参数拼装后非法 JSON 时，单遍 string-aware 扫描把非法串按责任方归因
  （`truncated` 帧丢失/上游截断 / `illegal_chars` 模型侧非法转义或裸控制符 / `truncated_and_illegal`
  / `malformed`），只写日志（warn + `KIRO_TOOL_TRACE` 带 `defect` 字段），**纯可观测、绝不进控制流**，
  服务于「修不好的残留到底是谁的责任」定位真因。
- **截断跨轮恢复**（开关 `tool_truncation_recovery`，默认**关**）：仅当 JSON 修复层已启用且也补不回
  （真截断，缺整段值）、且归因为截断时触发——不发半截参数（半截会被客户端当完整调用执行），改置
  失败态让客户端退避**重试整个请求**（下轮模型可能生成更小的调用）。**绝不连坐号**（工具截断≠号坏）。
  默认关：它把截断从「发半截」变成「整轮失败重试」，改变对话流程，需用户显式开启。
- **工具描述长度上限可配置**（`tool_description_max_chars`，默认 10000）：入站工具顶层 description 的
  硬编码截断（原 10000 / schema 内嵌 2000）提为配置项，schema 内嵌恒取顶层的 1/5，设 0 表示不截断；
  按字符边界安全截断防多字节切坏。热更即时生效。

### External IdP 验活（承 0.7.5，补队头阻塞与成本泄漏）
- **修 reprobe/ARN 解析的 refresh_lock 队头阻塞**：全坏 external_idp 号 reprobe 一整轮 getUsageLimits
  会把所有号的刷新堵在锁后；显式 `drop` refresh_lock 让 arn/reprobe 在锁外并发，写回 profile_arn
  时另用短锁，消除队头阻塞。
- **全坏号 reprobe 成本护栏**：所有候选 region 都未开通 Kiro 的号，两次全坏 reprobe 之间加 6 小时
  最小冷却（`last_full_reprobe_at`），稀释「每 token TTL 白跑一整轮 getUsageLimits」的成本泄漏；
  找到可用 profile 时清空冷却（恢复灵敏）。

### Windows
- **系统托盘「重启服务」接线**：抽 `spawn_windows_relaunch_process` 自由函数供托盘与面板一键重启
  共用，走优雅关闭（quit notify + exit 3）拉起新进程，避免双拉。

## [0.7.5] - 2026-07-12

### 模型识别（registry 重构）
- **模型目录改为单一声明式真相源**（`model_catalog.rs`）：一张 `CATALOG` 表，每个 Kiro 真实
  modelId 一行，携带别名/上下文窗口/计费倍率/能力。`map_model` / `get_context_window_size` /
  `/v1/models` 广告清单全部从此表派生，消灭「广告清单 vs 映射逻辑」漂移。对齐 Kiro 官方模型表
  （补全 Sonnet 5 / Sonnet 4.0 / Auto，DeepSeek 128K、Qwen 256K 窗口）。
- **修旧 `contains` 子串匹配的静默错档**：Claude 老名不再静默升到最贵档、高版本不再静默降级、
  未知模型/未知版本改为**显式拒绝**（strict，可用 `KIRO_ALLOW_UNKNOWN_VERSION=1` 回退最新档），
  所有非精确命中打 `warn` 日志（从静默变可观测）。
- **修含 "auto" 子串的未知名被静默映射到 Auto**：`gpt-4-auto` / `autopilot` 等不含真实族名但含
  `auto` 子串的名字，此前会静默命中 Kiro Auto（1.0x）真实发上游、既不拒也不告警。改为 Auto 只经
  精确别名（`auto` / `claude-auto`）命中，其余 strict 拒绝。

### 流式与国产模型
- **剥离 DeepSeek DSML 工具协议标记**：国产模型（deepseek/qwen/glm）调工具前会吐 `<｜DSML｜…>` /
  `<｜tool▁calls▁begin｜>` 家族标记，原样透传会让客户端看到乱码。新增跨 chunk 安全的剥离逻辑，
  白名单门控、**只对国产模型生效**，Claude 路径首行即原样返回（零风险跳过）。
- **修 thinking 模式下 DSML 残留导致的 SSE 块顺序交错**：流在 thinking 块内结束且末尾残留 `<` 时，
  把 DSML 尾巴 flush 移到 thinking 块 stop 之后，避免「新 text 块 start → 旧 thinking 块 stop」
  违反 Anthropic「先 stop 再 start」契约。
- **cc_auto_buffer 默认改真流式**：Claude Code 请求从整段缓冲改为边到边逐块转发，修 CC 卡顿
  （想要 message_start 即精确 input_tokens 的场景仍可将 ccAutoBuffer 设回 true，热更即时生效）。

### 号池与稳定性
- **根治 id 复用隐患**：进程内单调计数器（`AtomicU64`，`fetch_add` 取号永不回退/复用），删号后
  清 per-id 冷却 / RPM / 模型黑名单态，杀「删号→出回收站→再加号复用旧 id→静默继承死号内存态」。
- **custom_api 请求上限改为终身预算**：`request_count` 纳入持久化（`kiro_stats.json`），达上限时
  **立即落盘** `request_count` + 禁用状态，修「重启即额度归零、被禁号重新可用」的防超预算漏洞。

### External IdP 多 region profile（403 FEATURE_NOT_SUPPORTED 根治）
- **同一 External IdP 账号多 region profile「验活选 region」**：实测坐实同一微软账号在 us-east-1 /
  eu-central-1 各有独立 profile，但**只有部分 region 真正开通可用**（另一 region 打 getUsageLimits
  返回 403 FEATURE_NOT_SUPPORTED）。导入时逐 region 探测 + **验活**（试 getUsageLimits），把可用
  region 标出、默认选可用的（多个才让用户选）；导入 UI 从盲取第一个改为列出全部 profile 让选。
- **存量坏号自动纠正**：已入池号若当前 region profile 返回 FEATURE_NOT_SUPPORTED，刷新时自动
  reprobe 切到可用 region（`sync_region_from_arn` 保 region 与 ARN 物理绑定，杜绝错配 400）。
- 右键卡片设置支持切换 Profile ARN region（切 ARN 而非改 region 字段，带验活校验，不可用则拒绝写入）。

### 安全（Grok 审计修复）
- **清除源码内嵌真实代理账密**（C1）：`http_client.rs` / `credentials.rs` / `usage_stats.rs` 三处
  测试样例的真实 socks5 账密与 IP 全部改虚构样例（RFC5737 文档 IP）。
- **custom_api 出站 SSRF 防护**（H1）：写入 `base_url` 时校验最终透传 URL 目标 IP 不落私网/环回/
  链路本地/元数据段（复用 `ssrf` 现有 forbidden 逻辑，DNS 失败放行不误伤）；透传/测活出站禁重定向
  （`redirect::Policy::none()`），堵死「公网 302 → 内网/元数据」的绕过链。

### Windows
- **系统托盘**：Windows 裸跑在系统托盘显示图标，右键菜单：打开网页 / 复制面板密钥 / 重启服务 /
  版本号 / 退出。「退出」走优雅关闭（drain 在途请求、关 SQLite），不硬杀。专用线程跑 win32 消息
  循环，不占 tokio 主线程；非 Windows 编译时不含托盘。
- **数据隔离 + 首次开浏览器**：Windows 裸跑把 config.json / credentials.json / 用量库统一收进 exe
  同目录 `KiroStudio-data/`（兼容旧位置存量配置，不丢号）；首次启动（新生成 config 时）自动开
  浏览器到 /admin。Linux 与显式 `--config` 路径行为不变。
- **面板 OTA / 重启在 Windows 裸跑（双击 exe）下支持自重启**：升级/重启后进程自身 spawn 一个后台
  helper（`.bat`），等旧进程退出、端口释放后用原路径拉起新 exe——不再依赖 systemd/监督脚本。
  修复 detached helper 缺 `CREATE_BREAKAWAY_FROM_JOB` 导致主进程在 job object 下退出时连带杀掉
  重启脚本、新进程起不来的问题（带 fallback：job 禁止 breakaway 时回退）。
- 更正 `DEPLOY-WINDOWS.md` / `update.bat` 的陈旧描述（旧文档称 OTA 会下 Linux 包/不可用，
  实际 v0.6.6 起已下对平台包 + 绕文件锁 + 回滚）。

### 工具调用（Invalid tool parameters 类型C 根治）
- **修 tool_use input 多帧拼装的非前缀重写洞**：Kiro 上游同一 tool_use_id 逐帧到达的 input，旧
  merge 只有「前缀替换 / 否则追加」两步，遇到非前缀双完整对象（如 AskUserQuestion 深嵌套参数被
  重写）会拼成 `}{` 粘连的非法 JSON → 客户端报 Invalid tool parameters。抽出 `merge_tool_input`
  纯函数补全 7 步决策表（新增「丢迟到旧短快照」「非前缀双完整对象取最新」），流式/非流式共用
  同一实现消除漂移。保持「stop 前不发 delta、stop 时单个 input_json_delta」不变式。

### 前端
- **号池列表 FLIP 平滑重排动画**：排序模式切换 / 显隐变化时，列表项从旧位滑到新位（不瞬跳）。
- **UI 排版自定义**：号池排序模式 + 卡片尺寸档位（设置页新增「UI 排版」区，切换后统一走保存按钮）。
- **custom_api 专属卡片**：上游地址 / 请求用量 / 测活，隐藏 Kiro 订阅/余额/刷新 Token。
- **白名单（允许模型）/ 测活统一到勾选后批量操作**：去掉卡片正面重复的「测活」「允许模型」按钮，
  改为勾选凭据后由工具栏「批量验活」「允许模型」（弹窗）统一操作，卡片正面更清爽。
- 新号初始化翻牌 toast 通知。

## [0.7.4] - 2026-07-11

### 修复
- **透传号被 Kiro 选号误选致 403 冷却**：彻底隔离 custom_api 与 Kiro 两个选号池——
  `is_entry_selectable` 对 custom_api 直接返回 false（Kiro 永不选透传号），透传结果记账只动
  per-id RPM/计数，不碰 Kiro 的 health/family/token 状态。

## [0.7.3] - 2026-07-11

### 修复
- **添加自定义 API 报「refreshToken 为空」**：后端 `add_credential` 只认 api_key / OAuth 两类，
  custom_api 落进 OAuth 分支被要求 refreshToken。修为：custom_api 单独分支——只校验 base_url、
  去重按 base_url+api_key、跳过 Kiro 网络刷新验证（它不是 Kiro 号，没有 refresh token）。
  本地实测：只给 base_url+apiKey 添加成功，不再报 refreshToken 为空。
- **R18 图源开关关闭后缓存不清、刷新仍是旧图**：改 R18 / 背景开关保存后，只改了「下一轮预取参数」
  却没清已缓存的 20 张旧图（容量 20、每 12 分钟才补 6 张，旧图能服务很久）。修为：R18 或背景
  开关一变，**立即清空背景图内存池**（`clear_bg_pool`），下次 random-bg 按新参数即时重新拉取。

## [0.7.2] - 2026-07-11

### 修复（非 us-east-1 的 IdC/Enterprise 号对话 400 Improperly formed）
- **profileArn 动态解析固定打 us-east-1**：`resolve_profile_arn_via_management`（ListAvailableProfiles）
  此前用凭据 region 拼 management host，但 **Kiro 的 profile 全局注册在 us-east-1**，不随账号
  region 分布。服务器实测（eu-central-1 Enterprise 号）：打 `management.us-east-1.kiro.dev`
  返回真实 profile，打 `management.eu-central-1.kiro.dev` 返回空 `[]`。空 profiles → profileArn
  恒 None → 对话套 us-east-1 占位 ARN → region 与 profileArn 不符 → 400 Improperly formed。
  修为：**该解析函数固定 us-east-1**（对话/余额端点仍按凭据 region，解析到的真实 ARN 第 4 段自带
  正确 region，会被 `effective_upstream_region` 回正，自洽）。这是「以前 non-us-east-1 号偶发 400、
  一直没根治」的真根因——us-east-1 号巧合一致所以没暴露，eu/ap 等 region 的号才炸。

## [0.7.1] - 2026-07-11

### 修复
- **自定义 API 上号误报「请输入 Refresh Token」**：添加凭据选「自定义 API」时，提交校验的
  非-api_key 分支会先要求 Refresh Token，导致自定义 API（本不需要 refresh token）永远卡在
  这一步、走不到 base URL 校验。修为：custom_api 单独分支，只校验 base URL、不要 Refresh Token。

## [0.7.0] - 2026-07-11

### 新增（自定义 API 代挂透传）
- **自定义 API 凭据（Anthropic 兼容上游代挂）**：可在「添加凭据」里选「自定义 API」，填上游
  base URL + 密钥 + 请求上限。语义是**代挂透传**——Claude Code 打 `/v1/messages` 时，若选号
  命中自定义 API 凭据，就把原始请求体**原样透传**到该 base URL、换用它的密钥、响应流**原样回**
  （入口=出口=Anthropic，零协议转换，效果等同直接拿那个 key 用）。与 Kiro 号**混在同一池按
  优先级/负载均衡分流**。
  - **请求上限自动禁用**：累计请求数达到 `requestLimit` 自动禁用该凭据（防代挂 key 跑量超预算）。
  - 支持凭据级**代理 + 优先级**（复用现有 effective_proxy）。
  - **铁律：绝不污染 Kiro 主路径** —— 只在选号命中自定义 API 凭据时接管；选到 Kiro 号（或池中
    无自定义号）则原样走 Kiro 转发，行为字节级不变。透传响应独立流回，绝不进 Kiro 的 event-stream
    解码器/StreamContext。本地假上游实测透传通过（换 key + body 原样转发），505 测试双特性全绿。
  - 数据模型：`KiroCredentials` 加 `base_url`/`api_key`/`request_limit`（auth_method=custom_api），
    api_key 已加入 Debug 脱敏；自定义号在 `ensure_valid_token` 短路，不进 Kiro token 刷新/IdC 逻辑。

## [0.6.10] - 2026-07-11

### 修复（关键：Windows 裸双击 exe「点击没反应」）
- **exe 缺 config 时不再闪退，改为内置引导**：此前直接双击下载的单个 exe（当前目录无 `config.json`）
  会因缺 apiKey 立刻 `exit(1)`，控制台窗口一闪而过 = 用户看到「点击没反应」。现在 exe 启动时若
  配置缺失，**自动在 exe 同目录生成带强随机密钥的 config.json**（加密安全 RNG）、大字打印
  adminApiKey / apiKey / 面板地址，然后正常启动——裸双击开箱即用，无需先跑 start.bat。
  - 落盘路径：默认 `config.json` 时优先写 **exe 同目录**（双击时 cwd 常不是 exe 目录），
    但 cwd 已有 config 则沿用（兼容源码目录运行 / start.bat）；`--config` 显式路径原样尊重。
  - **幂等且绝不覆盖**：已有 config 完全不碰，二次运行不重新生成、密钥不变。
  - 排除了「缺 DLL」误因：核对线上 exe 导入表无 `VCRUNTIME140.dll`（crt-static 生效），
    「没反应」纯粹是缺 config 闪退，非运行库问题。

## [0.6.9] - 2026-07-11

### 改进（白名单 UI 补全）
- **凭据卡片直接管理「允许模型」白名单**：此前白名单只能在"测试可用模型"弹窗里设、且要先测出结果才出现，
  凭据卡片上既看不到也改不了。现在齿轮设置弹窗（优先级/RPM 同排）新增「允许模型（白名单）」勾选器——
  勾选即该号只接选中模型（成本安全硬门，全不选=不限制），一键保存；卡片主体在设了白名单时显示
  「白名单 N 项」徽标（悬停看具体模型）。
- **模板文案**：模型测试弹窗的快速勾选模板「仅国产便宜」改为「仅国产」。

## [0.6.8] - 2026-07-11

### 修复
- **侧边栏版本号硬编码**：侧边栏一直写死显示 `Admin Panel v0.6.4`，与后端真实版本脱节
  （设置页/OTA 显示正确，唯独侧边栏是死值）。改为读服务端真实版本：`/config` 响应新增
  `serverVersion`（编译期注入 `CARGO_PKG_VERSION`），侧边栏经共享的 `config-snapshot`
  查询取值（与设置页同一缓存键，零额外请求），取不到时不显示版本号而非显示过时值。

## [0.6.7] - 2026-07-11

### 新增（国产模型 + 成本安全）
- **国产模型可调用（GLM / DeepSeek / Qwen / MiniMax）**：Kiro 上游本身直收原生 modelId，
  `map_model` 加分支——`deepseek→deepseek-3.2`、`glm→glm-5`、`qwen→qwen3-coder-next`、
  `minimax→minimax-m2.5/m2.1`，并支持完整原生 id 直透；`/v1/models` 列出这些模型；上下文窗口
  默认 200k。计费按上游 meteringEvent 真实累加，不硬编码倍率。（能否用取决于该号订阅是否覆盖，
  不覆盖走 INVALID_MODEL_ID 模型级黑名单 + failover，不废号。）
- **每号「允许模型」白名单（成本安全硬门）**：凭据可设 `allowedModels`，选号在唯一收敛点
  `is_entry_selectable` + 平行 `transient_wait_duration` 两处硬过滤——设了白名单的号**只**接白名单内
  模型。用途：把便宜模型（国产）的流量锁死在指定便宜号上，**杜绝便宜请求溢出到贵号按贵号计费**。
  硬门语义：设太窄 + 号不够则该模型无号可用返错（防溢出优先于可用性，刻意如此）。新增
  `POST /credentials/{id}/allowed-models` 端点。
- **探测结果打标签持久化**：`probe_models` 完成后把「测试可用模型」结果（supported/unsupported/
  unknown + 时间）写入凭据、持久化，下次进测试页无需重测即可看到该号测过什么、结果如何。
- **白名单 UI**：模型测试弹窗加模板（仅国产便宜 / 仅 Claude / 全部）、测出 supported 一键设为白名单、
  展示历史测试结果。

### 修复 / 改进
- **`Invalid tool parameters` 根治**：根因是逐片透传 tool 参数 partial_json——上游帧非前缀单调时
  启发式重复拼接、或中间帧静默丢弃/截断，客户端拼接后的**总 JSON 非法**。改为 kiro2api 验证的
  范式：按 tool_use_id **缓冲到 content_block_stop 再一次性发单个 delta**（Anthropic 契约允许，
  客户端只在 stop 才 parse）。全程 String 级重组、删除字节切片（消除 char-boundary panic 面）；
  stop 时校验完整 JSON，非法则告警但原样发（绝不静默吞成空参数）；流截断时收尾 flush 残留缓冲 +
  关闭块。单点覆盖 /v1 流式、/cc/v1 缓冲、非流式三条路径。
- **tool 帧静默丢弃补盲（可观测性）**：`Event::from_frame` 失败此前无声吞帧。四处站点补 Err 分支——
  `toolUseEvent` 解析失败置 DecoderStopped 失败态（收尾补发 SSE error / 非流式返 502，客户端按
  api_error 重试，不再把截断当成功），非 tool 帧仅告警不置失败态（零误伤正常流）。
- **Claude Code 自动切协议**：识别到 CC 请求（`x-anthropic-billing-header` 或 UA 经
  `classify_device` 判为 claude-code）时，`/v1` 流式自动走 buffered 分发（等价 `/cc/v1`，
  input_tokens 用上游准确值），CC 无需手动改端点。可配置热更开关 `ccAutoBuffer`（默认开）。

## [0.6.6] - 2026-07-11

### 修复（v0.6.5 出厂构建随附的三处真实缺陷）
- **TLS 后端统一为 rustls，消除「切 native-tls 废网关」的雷**：v0.6.5 起出厂二进制一律
  `--no-default-features`（纯 rustls），不含 native-tls 后端；但设置页仍留着可点的「native-tls」
  按钮，用户点它保存并重启后，所有上游调用（刷 token / 转发）会命中 `bail!` 全部失败、网关直接废，
  只能手改 config.json 才能救回。三重根治：① 设置页移除 native-tls 按钮，TLS 后端改为只读展示
  `rustls`；② 后端 `http_client` 遇 `native-tls` 配置**静默回退 rustls**（不再 `bail`），兜底旧
  `config.json`；③ 保存配置时对任何非 rustls 值归一到 rustls，不再把死后端持久化。rustls 内置
  webpki + 系统根证书，功能等价，回退无副作用。
- **Windows 面板「OTA 在线更新」修好**：OTA 资产名此前硬编码 Linux（`kirostudio-linux-x86_64`），
  Windows 用户点面板升级会下载 Linux ELF（下错平台，即便 sha256 自洽也无法运行）、再试图覆盖
  运行中的 `.exe`（Windows 锁定，失败）。两处根治：① 资产名按运行平台编译期选择（Windows 取
  `kirostudio-windows-x86_64.exe`）；② 替换步骤按平台分流——Windows 用「rename 旧 exe→.bak（备份+
  腾路径）→ rename 新 exe→原路径」绕开文件锁，重启由 start.bat/run.bat 监督循环按原路径拉起新
  二进制；替换失败自动回滚，不留缺失的 exe。至此 Windows 面板一键升级真正可用。
- **CI 增加出厂构建测试门禁**：此前 `cargo test` 只跑默认特性（native-tls），从未覆盖真正发布的
  `--no-default-features`（纯 rustls）构建 = 出厂配置存在测试盲区。`release.yml` 新增 `test` 任务，
  在构建任何产物前先以出厂特性跑全量测试（492 通过），Linux/Windows 两个 build 任务均 `needs` 它，
  测试不过不发布。

## [0.6.5] - 2026-07-11

### 新增（Windows 本机部署，纯增量层，不改任何 `src/` 运行逻辑）
- **引导式启动器 `deploy/windows/start.bat`（双击即跑）**：检测配置 → 缺失/损坏则自动生成带强
  随机密钥的 `config.json`（无 BOM，避免后端 `serde_json` 报 `expected value at line 1 column 1`）
  → 大字打印 adminApiKey/apiKey/面板地址 → 拉起网关。首次零手工配置。
- **监督循环（等价 systemd `Restart=always`）**：`start.bat` / `run.bat` 内置守护循环，网关干净
  自退（exit 0）后自动重拉——**让 admin 面板「一键重启」/ OTA 后重启在 Windows 真正生效**（Windows
  前台无守护进程，此前点重启只会停服不自起）。按退出码区分：0=面板重启→重拉；非零=崩溃→退避重试，
  连续 5 次放弃并报错（不无限刷屏）；Ctrl-C / 关窗口=停服。已在 Windows 实机测试通过。
- **更新脚本 `deploy/windows/update.bat`**：`git pull` + 重建前端/exe，等价面板 OTA（面板 OTA 在
  Windows 不适用：它下载 Linux musl 二进制 + 依赖 rename 运行中 exe）。带防呆：已跟踪文件脏改动
  拒绝更新（不吞用户改动，untracked 文件不误伤）、检测到 exe 运行中拒绝重编（Windows 锁定运行中 .exe）。
- **零运行库依赖 `.cargo/config.toml`（`+crt-static`）**：仅对 `windows-msvc` 目标生效，静态链接 C
  运行时，消除对 `VCRUNTIME140.dll`（VC++ Redistributable）的依赖——任意 Win10+ x64 机器双击即跑，
  无需预装任何运行库。**不影响 Linux/macOS 构建**（cfg 条件不匹配，GitHub Actions Linux 产物不变）。
- **发布产物新增 Windows exe**：`release.yml` 增加 `kirostudio-windows-x86_64.exe`（纯 rustls，
  `--no-default-features`，前端已内嵌）+ sha256，Release 页可直接下载运行。
- **部署文档 `docs/DEPLOY-WINDOWS.md`**：兼容性矩阵、从零跑起、日常运维（停止/重启/更新）、
  与 Linux 版差异表、常见问题。

## [0.6.4] - 2026-07-11

### 修复（模型探测超时）
- **前端 axios 超时**：模型探测现在对每个模型发真实生成请求（可耗时数十秒~数分钟），却被全局
  15s 超时掐断，报 `timeout of 15000ms exceeded`。给探测请求单独放宽到 5 分钟（其它 admin 操作
  仍保留 15s 兜底不变）。
- **后端探测客户端超时**：探测要消费完整生成流，此前用 `.timeout(30s)` 总超时，慢模型生成中途被
  掐断→误判 unknown/失败（与 `Connection closed mid-response` 同类）。改用 `build_streaming_client`
  的 `read_timeout`（空闲间隔 60s）——只要上游在吐数据就不超时，真卡死 60s 无数据才放弃。

## [0.6.3] - 2026-07-11

### 修复（关键）
- **`Connection closed mid-response` 根治**：对话路径的 HTTP client 此前用 reqwest 的 `.timeout()`
  （**整个请求生命周期总超时**，720s），覆盖读响应体全过程——对流式是致命的：一个健康但耗时长
  的大请求（opus 大 prompt / 64k max_tokens，生成可超 12 分钟）会在**流中途被硬掐**，上游流没读完
  就断、我方 SSE 随之断裂，下游报 `Connection closed mid-response` 并疯狂重试。新增
  `build_streaming_client` 改用 **`read_timeout`（两次数据之间的空闲间隔上限）+ connect_timeout**，
  只要上游持续吐 token 流就永不被掐，只有真卡死才中断。仅换对话路径两个 client，其它一次性请求
  （auth/token/探测/count）保留总超时不变。
- **模型探测请求体修正**：探测此前用手搓的最小请求体（缺 chatTriggerType/origin 等必填字段），
  上游一律回通用 400（与模型权限无关）导致非全绿即全红、且拿不到 credits。改为复用 converter
  生成**与真实对话同构的合法请求体**、再覆盖 modelId，才能真正触发上游的模型权限判定 +
  消费流解析真实 meteringEvent 计费。

### UI / 默认值
- **模型测试改为独立弹窗**：可自选要测的模型（10 个候选带计费倍率）、结果保留在页可反复测、
  底部"返回"不清结果。每模型真实计费、逐号显示花费 + 总花费。
- **userKey badge 换行修复**：设置页 userKey 输入行的"已设置/未设置"标签不再被挤压换行。
- **R18 图源默认改为关闭**（全年龄 r18=0）：截图/演示/给别人看面板更安全，需要再手动开。

## [0.6.2] - 2026-07-11

### 功能 / 修复
- **模型测试重做**：从卡片按钮改为**勾选凭据后顶部批量栏的「测试可用模型」+ 独立弹窗**
  （仿批量验活）。修正此前只看 HTTP status 导致的**假阳性**（#82/#77 明明受限却全绿）——
  现**真正消费上游 event-stream**，流内出现 error/exception(含 INVALID_MODEL_ID)才判不支持，
  其它 400 也保守判不可用。
- **真实计费 + 花费提示**：每个候选模型发一个无提示词真实请求、解析 meteringEvent 累加**真实
  credit 消耗**；每号显示"花费 X credits"，整轮完成 toast 报"本轮共花费 X credits"。
- **候选模型清单**用真实 Kiro modelId（qwen3-coder-next / haiku-4.5 / sonnet-4.5/4.6 /
  opus-4.6/4.8，从便宜到贵），探测直发 modelId 不过映射，国产模型亦可测。
- 诚实边界：判定依赖上游"无权限模型才返回 INVALID_MODEL_ID"的行为，弹窗内已明确标注可能偏乐观。

## [0.6.1] - 2026-07-11

修正 0.6.0 INVALID_MODEL_ID 处置的**致命设计缺陷**（0.6.0 未部署上线即被发布前对抗性复核拦下）。

### 修复（关键）
- **INVALID_MODEL_ID 改为模型级处置**（原 0.6.0 是凭据级、模型盲）：此前把某号对某模型返回
  `INVALID_MODEL_ID` 当成"整个号坏了"——冷却该号 300s，反复命中还自动禁用整个号。后果：一个
  客户端请求一个订阅不含的模型（如 opus-4.8），几秒内就能把**能正常服务其它模型**（sonnet/haiku）
  的号乃至整池全部打下线，且被禁号不参与自愈、需手动重启。现改为只记"该号+该模型"短期黑名单
  （TTL 30min），选号**仅对该模型**跳过它，该号对其它模型照常调度；**绝不**冷却/禁用整个号。
- **failover 透传修正**：仅当所有未禁用号都已对**当前模型**返回 INVALID_MODEL_ID 时，才向客户端
  透传真实 400（模型无效）；此前因可用性判定忽略冷却态，永远走不到透传分支，客户端收到的是
  429/502 死循环而非干净的"模型不存在"。移除了会误伤的 `SubscriptionInvalid` 自动禁用整号逻辑。
- **模型探测健壮性**：`probe_available_models` 单模型遇上游 5xx/网络错误降级为 `unknown`（不再
  误判 supported，也不再因一个模型失败中止整轮）；结果区分 supported/unsupported/unknown 三态。
- **deep_verify 诚实化**：移除其永不触发的 INVALID_MODEL_ID 死分支（探测体不含 modelId），明确
  分工——deep_verify 只做认证/封禁验活，模型可用性由 probe_available_models 负责。

## [0.6.0] - 2026-07-11

本轮聚焦**订阅失效处置、账号可用性诊断与每账号花费统计**。

### 调度 / 韧性
- **INVALID_MODEL_ID 识别 + 故障转移**：此前上游返回 `400 INVALID_MODEL_ID`（多因某号订阅
  被取消/降级、原本能用的模型不再开放）时，请求当场失败透传给客户端、坏号还留在轮转里反复命中。
  现改为：命中时给该号冷却并 **failover 到订阅仍有效的号**（换个号往往能成功）；短时间内反复命中
  达阈值即判定订阅失效、**自动禁用**（新增 `DisabledReason::SubscriptionInvalid`，可人工/自愈恢复）；
  仅当**所有**号都返回该错误时才判定模型本身无效、透传给客户端。
- **深度验活修正**：`deep_verify` 此前把一切 400 当"凭据有效"，会把订阅已失效的号误判为"活着"。
  现识别 `INVALID_MODEL_ID` 并如实报出"订阅失效/降级"。

### 功能
- **每账号生命周期累计花费**：凭据卡片新增"累计花费"，按上游 meteringEvent 真实计费累加，
  持久化进 `kiro_stats.json`，**独立于用量保留期**（明细按 30 天滚动清理，此累计只增不清），
  软删/恢复无损保留。
- **选中令牌后探测可用模型**：新增 `GET /api/admin/credentials/{id}/models`，对候选模型逐个发极小
  探测请求，按 `INVALID_MODEL_ID` 与否判定该号支持哪些模型（Kiro 无原生列模型接口，仅手动触发、
  约 7 次轻量上游调用，绝不进请求热路径）。凭据卡片加"测可用模型"按钮 + 结果展示。
- **禁用的号也能刷新 Token**：刷新按钮去掉"已禁用则禁用"的前端门（后端本就支持），便于排查/恢复。

## [0.5.0] - 2026-07-11

本轮聚焦**通知系统重写**与**架构文档校准**。

### UI
- **通知系统重写**(弃用 sonner,改自研 `admin-ui/src/lib/toaster.tsx`):此前多条通知并发时,
  sonner 的折叠态需靠一堆 `!important` CSS 硬掰其内部堆叠状态机,导致闪烁 / 空白灰卡 / hover 才
  显现等问题。改为极简 pub/sub store + 自绘 Toaster,完全掌控堆叠:竖直平铺、硬上限 5 条(超出丢
  最旧防刷屏堆爆)、底部倒计时进度条、hover 暂停、常驻关闭叉叉,保留右下角纯实色去光污染视觉。
  经 Vite alias + tsconfig paths 把 `sonner` 重定向到自研模块,现有全部 `toast.*` 调用点零改动。
- **号池健康通知批量合并**:同类事件(ARN 缺失/号禁用/额度耗尽/可疑活动风控)≥3 条时合并为一条
  汇总通知(标题给数量、描述列出前几个),避免号池批量出事时刷屏;1-2 条仍逐条带详细描述。

### 文档
- **`docs/ARCHITECTURE.md` / `docs/MODULES.md` 按当前代码全面校准**(用 codegraph 索引 + 源码逐一
  取证):修正代码规模(约 35,800 行)、上游端点(`runtime.{region}.kiro.dev`)、单端口 nest
  (admin 不再独立 :8992)、balanced 8 键选号 + AIMD 熔断器 + 族级连坐、动态重试预算 + 45s 墙钟、
  冷却时长现值;补全 health/compressor/overage/web_portal/health_marker/ssrf/scheduling/
  external_idp_login/update 等新模块;删除已移除的 cache_tracker 记述。

## [0.4.0] - 2026-07-10

本轮聚焦**性能、安全、上号可用性与 UI 打磨**,并规整了发布与一键部署流程。

### 性能
- **删除影子 prompt 缓存记账**:该记账在 30-40 万 token 大请求热路径同步跑 SHA256 前缀
  指纹计算,是可观固定开销且并不省钱(真正省上游 credit 的是 continuationId 确定性派生,
  未受影响)。移除后大请求慢尾从 16-31s 降到 ~6s。`promptCacheEnabled` 默认关。

### 安全(审计修复)
- **H1 OTA 完整性**:`.sha256` 校验文件改从 github.com 直连获取(独立可信信道),不再与
  二进制共用第三方镜像 —— 恶意/被劫持镜像无法再"同源投毒"绕过校验(此前构成 RCE 面)。
- **H2 XFF 伪造**:`trust_forwarded` 开启时改取 `X-Forwarded-For` **最右**可信段(而非可被
  客户端伪造的最左段),堵住绕过 IP 白名单/每-IP 限流。默认 `trustForwardedHeader=false`。
- **H3 region 注入**:凭据的 `region/auth_region/api_region` 字段过 AWS region 白名单,污染值
  不再拼进上游 host(此前可致 refresh_token 被 POST 到攻击者域名)。
- **M1 idc SSRF**:idc 上号 `region` 参数白名单校验,非法拒绝。
- 附带:修客户端可触发的 UTF-8 切片 panic、social OAuth CSRF 改 fail-closed、web_search
  补 `tool_use_id`、前端最近请求表 key 修复。

### 上号 / 凭据
- **external_idp(M365/Azure)根治**:kiro.dev 迁移后 external_idp 号必须带自己租户的真实
  profileArn,动态 ListAvailableProfiles 解析补全;余额查询改用统一 profileArn 口径,修
  external_idp 号余额显示为空的问题。

### UI
- 全站蓝色转圈圈换成贴合内容形状的**骨架屏**。
- 新增**号池健康通知**(右下角 toast):ARN 缺失/号禁用/额度耗尽/账户风控,状态跃迁提醒。
- **toast 重写**为干净扁平风(去光污染、关闭按钮清晰可见)。
- 版本字段改为**可选预设 + 自定义**(combobox);KPI 大数字**线性滚动动画**;修 KIRO PRO MAX
  订阅标签截断。

### 发布 / 部署
- 提交历史按主题拆分;`install.sh` 一键部署(Docker + 预编译二进制两条路径)防呆加固。

## 早期版本

- **0.3.x** — 上游 endpoint 迁移 kiro.dev、动态 profileArn、配置热重载三部曲、429 自适应熔断、
  M365 族级限速、per-credential RPM、OTA 回滚兜底。
- **0.2.x** — 仓库公开、历史脱敏、部署脚本 + Docker + systemd。
- **0.1.x** — 初版:多凭据聚合、Anthropic 兼容网关、管理面板。
