# 测试类绊脚石清单（测试全绿但线上失效的根源）

> 2026-08-15 研究产出。5 路并行探查 + 关键发现人工复核。
> 目的：找出「测试全绿但线上失效」的测试类绊脚石——夹具结构与生产不符、弱断言、
> 守卫自证绿、测试间耦合、盲区、卫生问题。每条给出位置、真实问题、严重度、修法。
> 背景案例（本会话已确认）：message_start 顶层 usage 夹具、守卫 needle 缺空格 ×2、
> 守卫注释字面量、new() 启动逻辑抢先、duplicated attribute ×4、B1 校验绕过。
> **闭环状态（2026-08-16 核验）**：本文档列出的待修项已全部闭环——B1 buffered 路径补 2 测试
> （W10-W12 #5）、H1 duplicated attribute（W11 已修）、测试锁纪律（W10-W12 websearch 测试锁修复）、
> 守卫自证绿（W10-W12 #4 mock_cache 守卫修复 + W13 核验无自证绿）。正文保留为审计快照。

## 一、测试与生产结构不符的盲区

### B1. [HIGH] buffered 路径 message_start usage 更正零测试（同构事故高发位）
- **位置**：生产 `src/anthropic/stream.rs:4279-4293`（buffered 收尾更正 message_start 的
  input_tokens，读 `event.data["message"]["usage"]` 嵌套路径）；测试：**不存在**。
- **真实问题**：这是全仓除 openai/convert.rs 外**唯一**读 message_start.usage 字段的消费点，
  且是 ccAutoBuffer=true buffered 路径的功能基石（handlers.rs:4224-4225 注释明说该路径
  存在理由就是「换取 message_start 即精确 input_tokens」）。但两个调用方测试
  （stream.rs:6708-6728、6787-6809）只断言推理文本兜底，从不断言更正后的 usage。
  若此处被改成读顶层 `data["usage"]`（message_start 注入事故的同类误改），**全部 376 个
  stream.rs 测试依然全绿**，线上 buffered 路径退回本地估算。
- **修法**：补行为测试——构造含嵌套 `{"message":{"usage":{...}}}` 的 event_buffer 调
  buffered 收尾，断言 input_tokens 被更正 + 顶层 usage 被忽略 + cache 字段补齐。

### B2. [MED] passthrough_think_filter 对无 `event:` 行的 data-only SSE 零覆盖（静默 fail-open）
- **位置**：测试辅助 `event()` `src/kiro/passthrough_think_filter.rs:890-892`（全部 41 个
  SSE 测试夹具必带 `event:` 行）；生产 `process_block` `:449-462`——`event_type.is_empty()`
  时**原样透传**（不滤 thinking、不剥 DSML、不扣 usage、不注入 mock cache），且无日志。
- **真实问题**：真实上游若吐 OpenAI 风格 data-only SSE（无 event: 行），过滤器整体静默
  失效。透传池 #4 pigcode 是 gpt 链，其 Anthropic 兼容端点格式未在测试/文档坐实。
  与已确认事故同构：全绿但线上不生效，且无日志可查。
- **修法**：补 data-only 输入测试，验证 fail-open 是**显式**行为（+ 日志）；或坐实
  4 个上游的 SSE 形态，决定是否需支持。

### B3. [MED] converter prefill 分支（末尾 assistant 静默丢弃）零测试
- **位置**：生产 `src/anthropic/converter.rs:1032-1044`——请求末尾 role != "user" 时
  静默丢弃并截断到最后一条 user；测试：零命中（全部 84 个夹具以 user 结尾）。
- **真实问题**：真实 Claude Code **高频发末尾 assistant prefill**，此分支是真实热路径。
  静默丢弃+截断逻辑无回归保护；误截有效历史无测试兜底。
- **修法**：补 prefill 请求测试（末尾 assistant 块被正确截断/保留语义钉死）。

### B4. [MED] tool_result `is_error` 字段零测试
- **位置**：生产 `converter.rs:1270-1278` 消费 `block.is_error` 决定
  `ToolResult::error/success` 与 status；测试：全部 ~39 处 tool_result 夹具均无
  `is_error` 字段（如 converter.rs:4942）。
- **真实问题**：真实 Claude Code 工具报错必带 `"is_error": true`。错误工具结果路径
  （error 状态标记）全绿但从未被断言；若 is_error 被忽略、错误结果被当成功下发，
  无测试兜底。
- **修法**：补 is_error=true 夹具的转换测试。

### B5. [LOW] message_start 骨架夹具（缺 message 对象）
- **位置**：stream.rs:6070/6215/8037、passthrough_think_filter.rs:920/966/1134/1154——
  夹具全是 `{"type":"message_start"}`，无真实上游必带的 message 对象
  （docs/PROTOCOL.md:419 坐实）。
- **真实问题**：与背景事故同构的**入口形态**：夹具缺字段/层级错 → 生产读不到 →
  测试照样绿。任何未来开始读 message_start 字段的改动在这些夹具上读到 Null 静默通过。
- **修法**：夹具统一升级为完整形态（含 message.usage 嵌套），并在 B1 的测试中坐实。

### B6. [LOW] 类型化 Event 夹具绕过事件名匹配层
- **位置**：stream.rs 全部测试用 `Event::AssistantResponse(...)` 类型化构造
  （如 stream.rs:6360-6371），从不走 `Event::from_frame`（kiro/model/events/base.rs:126-161）
  与 `EventType::from_str`（base.rs:53-62）的名字映射。
- **真实问题**：上游事件名 → 变体映射只在 base.rs 自身单测覆盖。若上游改发不带
  `Event` 后缀的事件名（如 `"assistantResponse"`），stream.rs 全部测试依然绿，
  线上事件全部落 Unknown 被丢弃。
- **修法**：在 stream.rs 层加一条从原始 `event: xxx` 字符串走 from_frame 的端到端用例。

### 已闭环（排除项，别重复报）
- passthrough_think_filter.rs:602-627 + 测试 1764-1813：message_start usage 嵌套事故
  已修复（生产先探 message.usage、顶层回退；测试双层覆盖）。
- openai/convert.rs、websearch.rs：夹具层级与生产消费路径一致（agent 逐项核对）。

## 二、弱断言模式

统计口径：行级 rg，`assert!(` 与 `assert_eq!`/`assert_ne!` 互斥计数；总测试 = `#[test]`+`#[tokio::test]`。

| 文件 | assert! | assert_eq! | contains | is_ok | 测试数 |
|---|---:|---:|---:|---:|---:|
| anthropic/stream.rs | 251 | 324 | 6 | 8 | 220 |
| anthropic/converter.rs | 122 | 170 | 41 | 0 | 102 |
| kiro/provider.rs | 153 | 152 | 1 | 0 | 101 |
| kiro/token_manager.rs | 299 | 303 | 9 | 4 | 237 |
| kiro/passthrough_think_filter.rs | 58 | 43 | 29 | 0 | 41 |
| admin/service.rs | 235 | 219 | 14 | 4 | 120 |

总体判断：断言质量比预期好（assert_eq! 占比高、大量全文锚定 + 兄弟断言双保险），
真弱断言集中在少数点：

### W1. [MED] serde_json parse 后 `is_ok()` 不验值
- **位置**：stream.rs:9617/9628/9677——拼帧测试只验「能解析」，不验解析出的结构/字段值。
- **真实问题**：拼出的 JSON 结构对但字段值错（丢字段、值被改）照样绿。
- **修法**：解析后 `assert_eq!(v["..."], expected)` 验关键字段。

### W2. [MED] token_manager.rs:9788 验证不出 refresh_token 长度校验真实存在
- **位置**：`test_validate_refresh_token_valid`（150 字符 token 只验 is_ok）。
- **真实问题**：兄弟用例只有 missing→is_err，没有「短 token 应 err」边界用例。若实现
  只查缺失不查长度，此测试依然绿（背景案例「B1 校验绕过」同型：单字段配置绕过）。
- **修法**：补短 token → is_err 用例。

### W3. [MED] passthrough_think_filter.rs:1085 OR 条件弱化 + 1304-1305 单字符 contains
- **位置**：`assert!(filtered.contains("text_delta") || filtered.contains("\"type\":\"text\""))`
  ——两个信号丢一个照样绿；`contains("A")`+`contains("B")` 单字符断言验不出顺序与位置。
- **修法**：OR 改 AND 或拆两条断言；单字符改完整块锚定（`assert_eq!` 全文）。

### W4. [LOW] converter.rs:2821 默认文案 contains
- **位置**：`assert!(sys_a.contains("You are a helpful assistant."))`——主断言
  `assert_eq!(sys_a, sys_b)` 只验两请求字节一致；若归一化 bug 把两请求稳定正文
  **同构地**改掉，两个断言同时绿。
- **修法**：加一条「与已知正确输出逐字节 equal」的黄金用例。

### 排除项（不弱）
admin/service.rs:8061/8176/8269/8299 的 `validate_error_messages(...).is_ok()`
（Ok 是 `()`，is_ok 即完整验证）；token_manager 的 is_ok 后均有 unwrap+assert_eq 验值。

## 三、守卫自弱化模式（15 条全查，14 有效）

### G1. [HIGH] service.rs:7923 mock_cache_config_is_fully_wired —— 快照/更新 4 个断言自证绿
- **位置**：`src/admin/service.rs:7923-7940`（清单写 7655/7670，工作树漂移 +260）。
- **真实问题**：注释声称「needle 运行时拼接」，但只有 setter（7943-7946）与 OR 链真拼了，
  快照/更新 4 个 needle（7929/7930/7935/7936）是**完整字面量且测试段自身含同款**：
  - `"mock_cache_enabled: config.mock_cache_enabled,"` → `src.contains(...)` 命中测试段自身
    7929 恒真（生产 3931 被删仍绿）
  - `"mock_cache_read_ratio: config.mock_cache_read_ratio,"` 同理
  - `"req.mock_cache_enabled"` / `"req.mock_cache_read_ratio"` 同理（生产 4341 被删仍绿）
  - `types.matches("mock_cache_enabled").count() >= 2` —— types.rs 测试段 2125/2129/2460/2727
    有同名字段垫底，生产 1138/1358 被删后 count 仍 ≥2 恒真
- **真实问题**：守卫「面板改配置生效链路」**最核心的 4/6 接线断言是摆设**——面板改了不
  生效且回「无改动」的回归无人拦截（正是该守卫注释声称要防的）。这是「守卫自弱化模式」
  在本仓现存唯一实锤，且**上一轮 5 路核验没抓到**（核验只查守卫存在，不展开 needle）。
- **修法**：4 个 needle 改运行时拼接（如 `"mock_cache_enabled"+": config.mock_cache_"+ "enabled,"`）
  或先 `split("#[cfg(test)]")` 截断测试段。setter/OR 链的写法就是现成样板。

### 其余 14 条：✅ 有效（证据摘要）
- provider.rs 六条（6761/6850/6889/6924/6970/5012/5178/4619）：needle 拼接 + `#[cfg(test)]`
  截断 + 剔注释行，断言精确计数/位置比较，测试段字面量全部被截断排除。
- handlers.rs:6531：函数体切片锚定（顶格 `}` 找边界），切片外测试段不干扰。
- handlers.rs:7011：真行为测试（构造 bail 串调 translate_upstream_error 断言 404）。
- converter.rs:5328：运行时数据守卫（读 CATALOG 断言白名单模型存在），无自证可能。
- token_manager.rs:14912/11309：`#[cfg(test)]` 截断 + 元组窗口锚定，字段顺序逐一验证。
- websearch.rs:3107、cli.rs:414（剔注释行双保险）、cooldown.rs:1198/1231（真实磁盘
  round-trip）。
- ⚠️ 行号漂移已确认：handlers.rs:6531/7011、service.rs:7923、websearch.rs:3107、
  token_manager.rs:14912/11309 相对 CURRENT.md 清单漂移（+60~+260），下次核验刷新。

### 守卫流程建议（答任务问题：WORKFLOW 是否加步骤？）
**建议在 WORKFLOW §3 或 §6 加「守卫 needle 本地验证」**：核验 agent 抽守卫时，不只是
「守卫存在」，而是对每条抽到的守卫**实际展开 needle 最终形态 → rg 到源码 → 确认唯一
命中生产段且测试段/注释无同款字面量**（本次 5 路核验没抓到 G1 的教训）。成本低
（15 条守卫 rg 几分钟），收益是守卫防回退能力可信。

## 四、测试间耦合

### C1. [MED] websearch.rs:3168 改全局 ERROR_MESSAGES 镜像但无锁（跨模块污染）
- **位置**：`src/anthropic/websearch.rs:3168-3204`（`f3_budget_exhausted_retry_after_...`）
  直接 `set_error_messages(table)` 改写进程级全局表，复位用空表。
- **真实问题**：handlers.rs:7121-7124 自证「ERROR_MESSAGES 是进程级全局，测试并行读写
  互相污染」，故用模块私有 `ERROR_MESSAGES_TEST_LOCK` 串行 4 个测试。websearch 的测试
  **取不到跨模块锁**，只能自 set 自复位。竞态窗口：websearch set `mcp_failed=15` 后、
  复位前，若 handlers 受锁测试写入自己的表 → websearch 的 `Retry-After: 15` 断言（3197）
  随机红（反向同理）。单跑绿、全量随机红的风险现役存在。
- **修法**：把锁提升为 `pub(crate)` 共享，或 websearch 测试复用 handlers 的
  `with_error_messages(...)` 辅助（取锁 + set + 尾复位），删除自 set 自复位模式。

### C2. [MED] MultiTokenManager::new() 启动副作用抢先测试构造（跨月恢复）
- **位置**：`token_manager.rs:2371`（new 内 persist_credentials → load_stats →
  **recover_expired_quota_disables(None)**（用真实 Utc::now()）→ load_trash）；
  测试被迫用 `re_set_quota_disabled`（:12528）在 new 之后重置（12525-12527 注释自证）。
- **真实问题**：① new 的恢复副作用使「构造参数带旧时间戳」的测试必然被复活，
  测试只能靠 new 后手动重置——重置路径和真实路径不一致，掩埋接线错误；
  ② 副作用是**真实时钟驱动**，测试全部用固定 now 避开月初 12h 缓冲 → 见 B 盲区 #3。
- **修法**：new() 增加测试注入点（如 `#[cfg(test)]` 构造器接收 now/跳过恢复），
  让测试能验证「new 内恢复」本身而非绕开它。

### C3. [LOW] pipeline DROPPED 计数器差值断言已正确串行（样板）
- pipeline.rs:156-188 `with_drop_burst` 把 before 读数放进 `DROP_TEST_LOCK` 临界区
  （pub(crate) 供 usage_handlers 共用）。alerting.rs:226-244 用 TEST_LOCK 锁全局
  集成测试。这两个是**做对了的样板**，C1 应照此模式修。
- 7 把锁盘点：DROP_TEST_LOCK / ENV_NOISE_TEST_LOCK / NATIVE_EFFORT_TEST_LOCK /
  BLOCKLIST_TEST_LOCK / ERROR_MESSAGES_TEST_LOCK / TEST_LOCK(alerting) / ENV_LOCK(update.rs)。
  锁粒度都是单测试函数，模式统一（毒锁容忍）。

## 五、测试盲区地图

### M1. [HIGH] refresh_loop.rs 全文件零测试
- **位置**：`src/kiro/refresh_loop.rs`（run_once:60、spawn:29）。
- **真实问题**：token 预刷新是生产核心路径（main.rs:416 启动即 respawn_refresh_task），
  但刷新循环体（选哪些号、lead_minutes、失败语义）无任何直接测试；token_manager.rs
  的 4 个 respawn 测试只验证句柄存/abort，不触 run_once 一行逻辑。
- **修法**：给 run_once 补行为测试（构造待刷新集合 + 固定 now）。

### M2. [HIGH] select_highest_priority 零测试
- **位置**：`token_manager.rs:5231`（生产调用 6954/8506，决定恢复后 current_id 切到
  最高优先级号）。
- **真实问题**：自愈/恢复路径的核心动作，行为正确性直接影响线上流量分布，只有定义
  没有断言。
- **修法**：补行为测试（多号不同优先级 → 断言选中最高者）。

### M3. [HIGH] recover_expired_quota_disables 月初缓冲分支零覆盖
- **位置**：`token_manager.rs:6456-6458`（`hour_of_month < 12 → return 0`）。
- **真实问题**：所有测试用固定 `2026-08-15T12:00:00Z` 刻意避开月初 12h 缓冲窗口
  （注释 12502-12504 自证）。该分支是「上游重置时区未验证」的防御逻辑，一旦防御误判
  （该恢复时挡掉）会导致整月不恢复——恰好是「测试全绿但线上失效」的种子。
- **修法**：固定 now 参数化（2026-08-01T06:00:00Z 等月初时刻）补两条用例。

### M4. [MED] select_next_credential 仅 1 个行为测试
- **位置**：`token_manager.rs:3493`（生产 4680/4692/4810 换号路径核心），唯一行为测试
  13973 只覆盖 RPM 饱和硬门；14099 是源码文本守卫非行为测试。冷却跳过、排除集合、
  降级语义几乎无覆盖。

### M5. [MED] usage_stats 面板聚合层无直接测试
- **位置**：`usage_stats.rs:1243/1249/1327/1332/1365/1370`（overview/timeseries_hourly/
  timeseries_daily）——`/api/admin/usage/overview` 与 `/timeseries` 的响应数据只被底层
  bucket 测试间接覆盖，聚合正确性（窗口对齐、空数据、跨窗口边界）零断言。

### M6. [MED] load_stats / persist_credentials 无直接测试
- **位置**：token_manager.rs:5382/5556——new() 内部执行，字段覆盖语义（success_count/
  request_count/last_used_at 覆盖、disabled 状态同步）完全靠生产验证；
  save_stats 有测试但 load/persist 方向没有。

### M7. [MED] cooldown.rs:581 cleanup_expired 无测试且全仓无调用方
- **位置**：`src/kiro/cooldown.rs:581`——双重信号：死代码或漏接线。若设计上该在
  flush/save 前清理过期条目而无人调用，冷却表随 trigger_count 累积无限膨胀。
- **修法**：先确认调用意图——接线或删除，然后补测试。

### 排除项（覆盖良好，不报）
错误翻译层（translate_upstream_error 端到端 10+ 测试，handlers.rs:6756-6950）；
select_custom_api（15+ 测试）；cooldown 持久化 round-trip（1198/1231）；
estimate_cost（usage_stats.rs:1934-1944）；rebuild_from_logs（2842/2880/2905）。

## 六、测试卫生

### H1. [LOW] handlers.rs duplicated attribute（已修，2026-08-15）
- 原 `#[test]` 后夹 doc 注释再 `#[test]`，作用于同一 fn
  （原 `absorb_503_body_must_carry_shield_cooling_marker`）。编译器警告但测试照跑，
  掩盖「两个 attribute 含义不同」的问题（背景案例 ×4 的残余形态）。
- **已修**：守卫重写为 `shield_cooling_markers_stay_in_production_text` 时，错位的
  doc 注释移回其归属测试（`permanently_exhausted_pool_is_never_absorbable`），
  游离 `#[test]` 删除，警告消除。

### H2. [LOW] 4 处临时目录泄漏（无 Drop 清理）
- admin/service.rs:8531（`kiro_bal_mig_{pid}` 无随机化）、8905、9258（helper 返回
  PathBuf 无 Drop）、admin/usage_handlers.rs:801。全部在 temp_dir() 下，不污染生产目录，
  但 CI 机器会累积。
- **样板**：admin/service.rs:12120 的 TempDir 带 impl Drop——其余几处应套用。

### H3. [LOW] model_catalog.rs:648 生产代码读 KIRO_ALLOW_UNKNOWN_VERSION 环境变量
- 测试行为随宿主环境漂移（CI 若设了该变量，模型识别测试结果改变）。无测试主动 set。

### 排除项
无测试读真实生产 config.json（12202 的 config.json 是临时目录自造）；无测试依赖真实
网络（多处注释「零网络」）；include_str! 契约守卫是项目特色做法，已用拼接规避自匹配。

## 七、最值得先动的 3 个测试绊脚石

1. **G1：service.rs:7923 mock_cache 守卫自证绿（HIGH）**——模拟缓存接线防回退的 4/6
   断言是摆设，面板改配置不生效的回归无人拦截；且证明核验流程有洞（守卫存在 ≠ 守卫
   有效）。修法 10 分钟（4 个 needle 改拼接 + 截断），样板就在同函数里。
2. **B1：stream.rs buffered 路径 message_start usage 更正零测试（HIGH）**——全仓唯一
   读 message.usage 的消费点，是 message_start 注入事故的同类高发位；误改成顶层 usage
   时 376 个测试全绿。补一个嵌套 usage 夹具行为测试即钉死。
3. **B2 + B3：结构盲区实测件（MED 但同构事故）**——passthrough data-only SSE 静默
   fail-open 零覆盖 + converter prefill 热路径零测试。两条都是「夹具形态没覆盖真实
   上游/真实客户端形态」的直接实例，补夹具即可。

## 附：修法优先级与 WORKFLOW 建议

- **守卫流程**（答任务问题）：**建议在 WORKFLOW §3 加「守卫 needle 本地验证」**——
  新守卫交付前：展开 needle 最终形态 → rg 确认唯一命中生产段 + 测试段/注释无同款
  字面量；§6 核验 agent 抽守卫时同样执行（本次 G1 就是核验只查存在没抓到的）。
- 先修 G1/B1（HIGH），再补 B2/B3/M1/M2/M3 测试；C1 锁提升顺手做（低风险高收益）。
- 所有修法都是补测试/改测试，不动生产逻辑——适合在下一波直接并入。
