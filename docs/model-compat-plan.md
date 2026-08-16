# 模型兼容层总体规划设计

> 状态：**规划稿 + 分期进度**（2026-08-15 定稿；**P0 已实现（W6，CI 1921/0）**，P1/P2 未实施）。
> 依据：`.opencode/ISSUES.md`（a-e 五类，含 sub2api/NewAPI 研究结论）、`.opencode/state.md`（W6 模型名链路审计）、`docs/ref-ZyphrZero-kiro.rs.md`、`docs/ref-kiro2cc-proxy.md`。
> 现状代码锚点（已核实）：`predict_passthrough_upstream_model`（provider.rs:1381，消费点 :1607）、`mark_model_unsupported`（token_manager.rs:3317，键 = 客户端原始名）、`is_model_blacklisted`（token_manager.rs:3328，消费于 select_custom_api_inner :3125）、`fetch_upstream_models`（passthrough.rs:579，智能候选路径已支持 /anthropic 剥离）、`effective_model`（deepseek_normalize.rs:328，选号层与改写层共用）、`model_catalog`（src/anthropic/model_catalog.rs，Family/ModelSpec/CATALOG 权威目录）、`MODEL_BLACKLIST_TTL_SECS = 30*60`（token_manager.rs:1062）。

## 1. 愿景

模型兼容层解决一个根本矛盾：**同一个模型名在网关内承担三种相互冲突的语义**。

- **客户端侧要宽容**：客户端发什么名都尽量接住、尽量找到能服务的号，别因为名字写得不规范而白付一跳或直接报错。宽容体现在：别名/通配符/路径段匹配、`/v1/models` 动态聚合、错误回显带模型名。
- **选号侧要语义严格**：选号判据必须基于「改写后的真实上游名」，不能基于客户端原始名猜。严格体现在：`effective_model` 共用判定（已落地）、正向路由缓存、黑名单键与改写链同源。
- **统计侧要如实反映**：`requested_model`（客户端原始名）、`mapped_model`（实际服务名）各归其位，`by_model` 成本归因不张冠李戴。如实体现在：mapped_model 预判（P0 已修）、登记 A/B/C 记录层修复（P0）、响应模型观察与回写（P1/P2）。

三条语义各有一个**唯一权威函数**，其余消费点一律复用，从结构上消灭口径分叉：

| 语义 | 权威函数 | 消费点 |
|---|---|---|
| 宽容匹配（名→名） | `model_catalog::resolve`（别名表） | `/v1/models` 聚合、`supports_thinking`、改写链映射 |
| 严格改写（名→上游名） | `effective_model` + `predict_passthrough_upstream_model`（P0 已统一） | 选号预判、白名单判定、黑名单键（P2 切换）、正向路由（P1 复用） |
| 如实记录（名→统计字段） | `record.rs` 契约（requested_model 恒原始名，mapped_model 恒服务名） | 成功/失败埋点、usage 归因 |

分期原则：**每期可独立交付、独立验证、不依赖后续期**。P0 修口径（已在收尾），P1 加正向证据（巡检 + 观察 + 归一化），P2 做策略升级（scope/回写/effort/回显/键切换）。

## 2. 现状盘点（设计基线）

| 机制 | 位置 | 现状 |
|---|---|---|
| 模型黑名单 | token_manager.rs:3317 | (id, **原始名**) 30min TTL；负向证据唯一来源；**键与改写链不对齐**（登记 D，P2 修） |
| 严格语义 | deepseek_normalize.rs:328 | fallback 只对 claude-* 生效；选号层与改写层共用 `effective_model` 本体 |
| mapped_model 预判 | provider.rs:1381 | 完整链预判（映射→normalize→白名单→per-凭据覆盖），:1607 接线，13+ 测试 |
| model_catalog | src/anthropic/model_catalog.rs | 权威别名表，带 Family/版本/倍率/[1m]/thinking 语义 |
| fetch_upstream_models | passthrough.rs:579 | 现成数据源；智能候选路径（剥 /anthropic、/v1）；pinned client SSRF 防护；目前只被 admin 测试接口调用 |
| 12 位排序键 | token_manager.rs | 含 model_calls；白名单感知选号（allowed_models 硬门）；**首位前插 support_rank 是 P1 的有意行为变化** |
| 模型级限流/族级连坐 | token_manager.rs | cooldown 层已有族级连坐；RPM 桶按模型（A5 家族 scope 待议，P2） |

## 3. 分期路线总览

| 期 | 主题 | 内容 | 交付判据 |
|---|---|---|---|
| **P0** | 口径修复（进行中） | mapped_model 预判修复（已完）、统计口径对齐（登记 A/B/C）、通配符匹配（A1，已移植）、路径段匹配（A2，已移植）、测试矩阵 | CI 全绿 + 守卫在位；登记 A/B/C 记录层字段各归其位 |
| **P1** | 正向证据 | 上游模型巡检 + 正向路由（与 NewAPI 巡检合并设计）、响应模型观察（先记录后计费）、通用归一化层（FormatMatchingModelName 式单函数多消费） | 三态缓存行为测试全绿；观察路径与 billed 路径分离守卫；归一化单函数守卫 |
| **P2** | 策略升级 | 家族限流 scope（A5）、响应体模型名回写（A6）、模型名内嵌 effort（A7）、错误回显增强（分组+模型名+ErrorCode）、黑名单键改映射后名（B1 + 登记 D） | 每项独立开关/独立测试；错误回显 ErrorCode 枚举与文案分离守卫 |

## 4. P0：口径修复（本期已完成/进行中）

### 4.1 mapped_model 预判修复（已完成）

**目标**：统计层在请求发出前就能预测「实际会服务该请求的上游模型名」，使失败记录/成功埋点的 mapped_model 不为 None。

**方案**（已落地）：`predict_passthrough_upstream_model`（provider.rs:1381）完整复刻改写链：映射 → deepseek normalize（白名单感知 + per-凭据 fallback_model 覆盖 + model_mapping_exempt）→ gpt-* 保持原名。:1607 接线到失败记录路径。测试 13+ 用例（provider.rs:7852-8004 测试矩阵：normalize fallback / 白名单保持 / 无 normalize 仅映射 / 先映射后归一化 / 映射到 deepseek / exempt 仍归一化 / gpt 不改写 / per-凭据覆盖 / 空名与 None）。

**风险与缓解**：预判函数与改写链是两处代码，分支漂移会让预判失准。缓解 = 测试矩阵逐分支钉死「预判 == 改写」的对应关系（已做），并升级为源码级守卫（见 §8-1）。**不会导致问题加重**：预判是增量字段（mapped_model 由 None 变 Some），不改写任何请求字节；即便预判出错也只是统计字段不准，且会被 P0 测试矩阵拦住。

**验收**：provider.rs 预判测试矩阵 13+ 全绿；守卫钉死「改写链新增分支必须同步预判」。

### 4.2 统计口径对齐（登记 A/B/C，进行中）

**目标**：让 `requested_model` 与 `mapped_model` 各归其位，消除 Kiro 主路径与透传路径的口径分叉（state.md W6 审计结论）。

**方案**：
- **登记 A（MAJOR）**：Kiro 主路径 `requested_model` 目前记 converter 归一化后的 Kiro id，违反 record.rs 契约「客户端原始名」。修法：`call_api_stream`/`call_api_with_retry` 加 `client_model` 参数（失败记录用）；handlers.rs 4 处成功埋点（2383/3163/3291/3980）`requested_model = payload.model`。**两处必须联动**，否则成功/失败混合口径比现在更糟。
- **登记 B（MAJOR）**：`overload_fallback_model` 成功路径（provider.rs:4350）`CallMeta.model = fallback 名`，违反「model 恒为客户端原始名」契约。修法：model 保持原始名，`mapped_model = Some(fallback 名)`。
- **登记 C（MAJOR）**：websearch 回灌路径 `upstream_model` 恒 None（websearch.rs:1569 run_round 丢弃 CallMeta.mapped_model）。修法：`WebSearchLoopSuccess` 加 mapped_model 字段，`emit_websearch_loop_usage`（handlers.rs:531）写入。

**风险与缓解**：
- 联动风险（登记 A）：成功埋点 4 处与失败记录路径分处两文件，只修一半会制造混合口径。缓解：验收测试同时断言成功与失败两条路径的 requested_model。
- 行为变化风险（登记 B）：requested_model 从 fallback 名变原始名，依赖旧口径的查询（前端用量页按模型分组）会看到新的键。缓解：这是契约修正（旧值本就是 bug），发布时在变更说明中列出。
- **不会导致问题加重**：三处都是「把错字段改成对字段」，键值都在现有 schema 内，不新增字段不新增口径。

**验收**：守卫断言 handlers.rs 4 处成功埋点与失败路径共用 `payload.model` 来源；登记 B 测试（fallback 成功路径 CallMeta.model 保持原始名）；登记 C 测试（websearch 回灌 usage 的 mapped_model 非 None）。

### 4.3 通配符匹配（A1，已移植）

**目标**：模型映射支持通配符形态（如 `gpt-*` → 特定号），减少逐名枚举。

**方案**（已落地）：model_mapping 匹配规则支持通配符，与白名单/预判链共用判定。

**风险**：通配符过宽会扩大改写范围。缓解：改写判定仍受严格语义约束（gpt-* 不改写进 deepseek 链），通配符只作用于映射命中，不改变 `effective_model` 的生态闸。**不会导致问题加重**：A1 是纯匹配能力扩展，判定链不变。

**验收**：通配符命中/不命中/边界（空、全星）测试。

### 4.4 路径段匹配（A2，已移植）

**目标**：模型名按路径段匹配（如 `deepseek-v4-*` 段匹配），覆盖版本号抖动。

**方案**（已落地）：模型名按 `-` 分段匹配，段级通配。

**风险**：段通配与精确别名优先级冲突。缓解：精确命中优先于段通配（沿用 model_catalog 的 MatchKind 分级语义）。**不会导致问题加重**：匹配失败路径不变（不命中即原名透传 + 白名单按原名判定）。

**验收**：段匹配优先级测试（精确 > 段通配 > 未命中）。

### 4.5 测试矩阵

**目标**：把「预判 == 改写 == 记录」三条链路的所有分支对应关系钉死在测试里。

**方案**：以 provider.rs 预判测试矩阵为基座，补齐登记 A/B/C 的联动测试，形成兼容层基线矩阵（名称、分支、断言三栏）。后续 P1/P2 每引入一个新消费点，先在矩阵中加行再实现。

**风险**：矩阵膨胀为仪式。缓解：每行必须对应一个真实分支（改写链一个分支 = 矩阵一行），不接受「为测试而测试」的行；守卫纪律沿用 CURRENT.md 的 needle 纪律（不写被守卫代码的完整字面量）。

## 5. P1：正向证据（巡检 + 观察 + 归一化）

### 5.1 上游模型巡检自动同步 + 模型感知正向路由（合并设计）

**目标**：把「负向黑名单」升级为「正向目录 + 负向证据」双轨：上游 `/models` 目录确认含目标模型的号优先选，明确不含的号压后，无目录数据的号维持现状。与 NewAPI `channel_upstream_update`（1106 行）的「上游模型巡检自动同步」天然合并（ISSUES.md (d) 模型路由条目 + 评审补强全部采纳）。

**方案要点**（文件/函数级）：

1. **数据源**：复用 `fetch_upstream_models`（passthrough.rs:579）。已具备：智能候选路径（剥 `/anthropic`/`/v1` 后按最贴近 base 排序尝试）、pinned client SSRF 防护、read_json_capped 限流、`{base}/models` 与 `{base}/v1/models` 双形态、`data[].id` / `models[]` / 纯数组三形态解析、排序去重。**零新增网络代码**。
2. **三态缓存**（zyphr `cached_model_support` 移植）：新模块 `src/kiro/model_support_cache.rs`（建议），`HashMap<(credential_id, normalized_model), SupportState>`，`SupportState ∈ {Confirmed, Unsupported, Unknown}`。
   - Confirmed：目录包含目标模型（大小写不敏感）。
   - Unsupported：目录明确不含（zyphr 语义）；**TTL 30min-1h**（评审补强：目录变更后不能永久跳过），与黑名单 TTL 同量级。
   - Unknown：无目录数据（未巡检/巡检失败/空列表），**不允许写缓存**（评审补强：空列表可能是上游暂时故障，写进缓存会把「暂时空」固化成「无模型」）。
   - 键用 §5.3 的通用归一化函数（大小写不敏感，与 `effective_model` 同语义）。
3. **巡检触发**：后台任务（tokio spawn），启动时 + 周期触发；**绝不放选号热路径同步 fetch**（评审补强）。singleflight：每号同一时刻至多一个在途巡检（号级 `AtomicBool` 或正在巡检集合）。
4. **失败退避**（评审补强）：连续失败指数退避（1min → 2min → 4min → 上限 30min），成功重置；退避状态不进排序判定（失败 = Unknown，维持现状排序）。
5. **选号接入**：`select_custom_api_inner`（token_manager.rs:3125）排序键**首位前插 support_rank**：Confirmed=0（优先）、Unknown=1（维持现状）、Unsupported=2（压到最末，**不跳过**——目录信息不全可能误判，压后比跳过安全）。「全候选 Unsupported 退化放行」（评审补强）：排序后仍有候选（最后兜底 Unsupported 号也会被尝试），不因全 Unsupported 返回无号错误。
6. **范围**：只对 custom_api 透传号巡检（Kiro/ksk 号无 `/models` 语义）。**deepseek 归一化凭据跳过**（评审补强 ISSUES (d) ⑤）：其 OpenAI 形态 `/models` 列表不等于 Anthropic 兼容层支持的模型集，目录数据会误导判定。
7. **与黑名单共存**：黑名单（负向，运行时事实）优先级高于正向缓存——黑名单内直接跳过（现有逻辑不变）；正向缓存只做排序，不做硬过滤（Unsupported 是压后不是跳过）。
8. **与 NewAPI 巡检合并点**（channel_upstream_update 设计吸收）：
   - **diff/通知**：巡检成功后与上次目录对比，新增/移除模型记日志；目录变化可选择触发通知（复用 alerting 通道，默认关）。移除模型不主动清黑名单——黑名单 30min TTL 自愈已覆盖。
   - **租约**：NewAPI 用租约防多实例重复巡检；我们单实例，singleflight 即等价物，不引入租约复杂度。
   - **同步语义**：一次巡检同时刷新目录缓存 + 记录 diff，无第二条链路（避免多路不同步）。

**风险与缓解**：
- **目录不全导致误判 Unsupported**：上游 `/models` 是广告位不是承诺，漏列 = 真支持但被压后。缓解：① Unsupported 只压后不跳过；② 30min-1h TTL 自动重查；③ 黑名单运行时证据仍优先。**不会导致问题加重**：压后 ≠ 不可用，最坏情况是「该号的请求被多试几次」。
- **巡检放大上游负载**：4 号池 × 周期巡检是低频（分钟级）；singleflight + 退避 + 不在热路径。**不会导致问题加重**。
- **排序键首位前插打乱均衡**：这是**有意行为变化**（评审确认：均衡被打乱正是目的——目录确认的号就该优先）。风险是 Confirmed 号被过度集中。缓解：support_rank 只做首位维度，后续 12 位键全部保留原语义；观察阶段可加「Confirmed 号成功/失败比例」度量，若 Confirmed 号失败率异常升高说明目录失真，靠黑名单兜底。
- **与新机制冲突自查**：白名单硬门（allowed_models）在 `is_entry_selectable` 层，先于排序——巡检缓存不改变白名单语义；严格语义（effective_model）是改写层，巡检是选号层，正交；吸收层不参与选号，无交互。无冲突。
- **口径分叉自查**：巡检键 = 通用归一化（§5.3）后的**改写后名**（predict 结果），与 P2 黑名单键切换（§7-5）同源——提前在 P1 定键空间，P2 切换零迁移。

**验收**（测试/守卫）：
- 三态转换测试：目录含 → Confirmed；不含 → Unsupported；无数据 → Unknown。
- TTL 测试：Unsupported 过期后重新巡检。
- 空列表不写缓存测试（上游返回空数组 → 状态保持 Unknown，不写 Unsupported）。
- 全候选 Unsupported 退化放行测试（全部 Unsupported 时仍能选号，不返回无号错误）。
- 退避测试（连续失败 → 退避增长；成功 → 重置）。
- singleflight 测试（同号并发触发只发一次网络请求——可用 mock fetch 计数）。
- support_rank 排序测试（Confirmed < Unknown < Unsupported；黑名单命中优先于一切）。
- deepseek 归一化凭据不巡检测试。
- 守卫：排序键首位 support_rank 的守卫注释 + 测试（对齐 CURRENT.md 排序键守卫纪律）。

**工时**：2-3 人日（评审修正，非 0.5-1）。

### 5.2 响应模型观察（首/终声明 + conflict 日志）

**目标**：收集「上游实际服务模型名」证据：流式 `message_start.model`（首声明）与终态模型名（终声明）不一致时记 conflict 日志。**先记录，后计费**——P1 只观察不改任何计费路径。

**方案要点**：
- 观察点：透传池流式路径（`message_start` 解析处）与非流式路径（响应体 model 字段）。目前这两个点已有解析（流式 message_start 是既有事件分支，非流式 usage 解析处）。
- 记录：`(credential_id, 我们下发的改写后名, 上游首声明名, 上游终声明名, conflict)` 落日志 + 可选结构化字段（upstream_trace 已接线，观察写入其模型名分类或独立计数器）。
- conflict 判定：首 ≠ 终（上游中途换模型）或首 ≠ 我们下发的改写后名（上游覆盖了我们的名）。
- **计费隔离**：观察路径只写日志/观测字段，**不进入 usage 真值路径**。P2 §7-2 才决定是否采纳进统计。

**风险与缓解**：
- **观察改计费**（本项最大风险）：若实现时顺手把观察值写进 usage，就是新口径分叉。缓解：结构性分离——观察写入只调 `upstream_trace`/日志，usage 埋点函数签名不动；守卫（§8-4）钉死「观察与 billed 路径分离」。
- **日志噪声**：每请求两行观察日志在高峰是噪声。缓解：conflict 才打完整日志，无 conflict 仅计数（或走 upstream_trace 默认关）。
- **不会导致问题加重**：纯新增观测，不改请求字节、不改 usage 字段、不改错误响应。

**验收**：首/终一致静默计数测试；conflict 日志测试（构造首 ≠ 终的流）；守卫：观察写入路径与 billed 路径分离。

### 5.3 通用归一化层（FormatMatchingModelName 式单函数多消费）

**目标**：NewAPI `FormatMatchingModelName` 是「单函数多消费」范式（一处归一化，黑名单/巡检/限流/路由全消费）。我们移植该范式，但**保留我们的大小写不敏感语义**（`eq_ignore_ascii_case`，`effective_model` 已是）。消灭「同一模型名在不同层用不同键」的分叉。

**方案要点**：
- 新函数 `normalize_model_key(name) -> String`（建议放 `src/anthropic/model_catalog.rs` 或 model_mapping 模块）：小写 + trim。**不剥语义后缀**（`[1m]`/`-thinking` 是语义差异，合并冷却会误伤——与 `model_catalog::resolve` 区分：resolve 是解析成规范 kiro_id，normalize 只是键规范化，二者职责不同、互不调用）。
- P1 消费点：巡检缓存键（§5.1）。
- P2 消费点：黑名单键（§7-5）、限流桶键（§7-1 若涉及模型维度）。
- 白名单判定（`effective_model`）保持现状（已大小写不敏感），P2 用 normalize 包装以统一入口。

**风险与缓解**：
- **新函数引入的回归**：多消费点切换 = 多点回归面。缓解：P1 只用于新链路（巡检缓存，零迁移）；黑名单/白名单切换放 P2，与键改映射后名（§7-5）同批，一次切换一次验证。
- **语义后缀误合并**：normalize 不剥后缀，从设计上排除。
- **与 model_catalog 混淆**：两名（normalize vs resolve）职责注释写死，守卫断言 resolve 不经过 normalize（防有人把键规范化塞进别名解析破坏 MatchKind 分级）。
- **不会导致问题加重**：P1 阶段纯新增函数 + 新链路消费，现有路径零改动。

**验收**：normalize 单元测试（大小写/trim/后缀保留）；守卫：各消费点调用同一函数（§8-3）；resolve 不经 normalize。

## 6. P2：策略升级（每项独立开关、独立验证）

### 6.1 家族限流 scope（sub2api A5）

**目标**：模型级限流桶支持按家族聚合（model → family），便宜家族与贵家族不共享桶、同家族共享桶。

**方案**：限流桶键从 model 扩展为可配 `(scope, key)`：`model`（现状，默认）或 `family`（聚合）。家族来源 = `model_catalog::resolve` 的 Family。配置默认保持 model（零行为变化），family 为可选开关。

**风险**：与现有「族级连坐」（cooldown 层）语义重叠——连坐是失败冷却，scope 是容量桶，层不同但名称相近易混。缓解：文档与注释明确「连坐=cooldown，scope=RPM 桶」；family 开关默认关，打开时观测 429 分布确认无异常。**不会导致问题加重**：默认关 = 现状字节不变。

**验收**：默认 model scope 行为不变测试；family scope 聚合测试（同家族两模型共享桶）；family 未知名回落 model scope 测试。

### 6.2 响应体模型名回写（sub2api A6）

**目标**：P1 观察的上游模型名在可信时回写 usage 的 `by_model`（成本归因用真实服务名）。

**方案**：P1 观察数据 + 可信判据：首终声明一致 **且** 能被 `model_catalog::resolve` 解析（可查倍率）才回写；conflict 或不可解析时保持 mapped_model（现状值）并打标记。默认关（可配）。

**风险**：
- **口径变化**：by_model 从 mapped_model 变上游观察名，成本核算键可能漂移。缓解：可解析门（catalog 内）挡住未知名；默认关；开关打开时 diff 统计「回写率」。
- 与登记 A/B/C 的关系：登记修的是「字段用错名」的 bug，回写是「用真实名覆盖预判名」的策略升级，正交。若登记未完成就开回写，会掩盖登记缺陷——**依赖顺序：登记 A/B/C 完成后再启用**（这是唯一一处跨期依赖，标记为软依赖：不开回写则无依赖）。
- **不会导致问题加重**：默认关 + 可解析门 + conflict 不回写。

**验收**：首终一致且可解析 → 回写测试；conflict → 不回写测试；未知名 → 不回写测试；默认关测试。

### 6.3 模型名内嵌 effort（sub2api A7）

**目标**：客户端通过模型名表达 effort 语义（如 `claude-sonnet-4-5-effort-high` 或既有 `-thinking` 变体），网关解析后注入 `output_config.effort`，替代/补充显式参数。

**方案**：模型名后缀解析（如 `-effort-{low|medium|high}`）→ 注入 effort 字段；与 `native_effort_whitelist_models`（converter.rs:5267 守卫，仅白名单内模型生效）一致；**显式 effort 参数优先于模型名后缀**（客户端显式传了就用显式的）；默认关。

**风险**：与现有 effort 传递冲突（显式参数 vs 后缀）；后缀误解析（模型名恰含 `-effort-`）。缓解：显式参数优先 + 白名单门 + 精确后缀匹配（不子串匹配）；默认关。**不会导致问题加重**：默认关 + 双门（白名单 + 显式优先）。

**验收**：后缀解析测试；显式参数优先测试；白名单外模型不解析测试；默认关测试。

### 6.4 错误回显增强（NewAPI ③：分组 + 模型名 + ErrorCode）

**目标**：错误响应结构化：稳定 ErrorCode 枚举（分组）+ 回显被拒模型名，客户端/前端可程序化判定，不再依赖文案。

**方案**：错误响应体**新增**字段 `error.code`（稳定枚举：`model_not_found` / `rate_limit` / `auth` / `context` / `upstream` 等，分组复用现有分类逻辑）+ `error.model`（回显客户端原始名）。**只增不改**：现有字段（type/message）字节不动。

**风险**：
- **前端语言耦合重演**（CURRENT.md 刚修 cooldownReason 中文判定）：ErrorCode 是枚举不下发文案，从结构上排除；前端消费改判 `error.code`。
- 客户端解析兼容：新增字段对老客户端无害（Anthropic 错误 schema 允许额外字段）；**文案不动**则依赖文案判定的第三方不受影响。
- **不会导致问题加重**：只增字段，不改现有字段与文案。

**验收**：各分组 ErrorCode 映射测试；现有错误字段逐字节不变测试；守卫：ErrorCode 枚举与文案分离（§8-5）。

### 6.5 黑名单键改映射后名（sub2api B1 + 登记 D）

**目标**：黑名单键从客户端原始名改为**改写后名**（`predict_passthrough_upstream_model` 同源），让同改写目标的多个原始名共享冷却（如 `claude-opus-5` 与 `claude-opus-4-6` 都被改写为 `deepseek-v4-flash`，后者被上游拒后前者也跳过——少付白跳一跳）。

**方案**：
- mark 端：provider.rs:1916 调用 `mark_model_unsupported` 时传**实际下发的上游名**（改写链最终输出，调用点手头就有，不重复计算）。
- select 端：`select_custom_api` 调用点（provider 侧）先跑 `predict_passthrough_upstream_model` 得改写后名，作为黑名单查询键传入；`is_model_blacklisted`（token_manager.rs:3328）与 `mark_model_unsupported`（:3317）本身不改签名，改的是**调用点传入的键**。
- 键空间一致性由守卫保证：mark 端实际下发名 == select 端 predict 名（同一改写链，P0 测试矩阵已钉死「predict == 改写」）。
- 白名单判定保持 `effective_model`（映射不进选号预判是既有设计，注释已写明）——黑名单与白名单键空间不同是有意为之：黑名单=负向冷却（用最终名最准），白名单=授权过滤（用生态名最直观），各自一致即可。

**风险**：
- **行为变化**：同改写目标的原始名共享冷却——这是目的（少付跳），但代价是某个原始名可能因兄弟名的失败而暂缓。缓解：TTL 30min 自动解禁；这是负向冷却，只可能少打错号，不可能多打错号。
- **键不匹配回退**：若 predict 与改写链漂移，黑名单查询将全部 miss（退化为「无黑名单」行为——比现状更宽容，不会更严）。缓解：P0 测试矩阵 + §8-1 守卫双保险。
- **与新机制冲突自查**：不影响白名单硬门（层不同）；不影响严格语义（改写判定不变，只改键）；不影响吸收层（无交互）。
- **不会导致问题加重**：最坏情况 = 黑名单失效退回现状（30min 内多付几跳），不是永久加重；且守卫测试拦住漂移。

**验收**：同一改写目标两原始名共享冷却测试（mark 用 A 名 → select 用 B 名（同改写目标）命中）；不同改写目标不误伤测试；守卫：mark 与 select 键空间一致性（§8-2）。

## 7. 不做清单（有理由）

| 项 | 来源 | 不做理由 |
|---|---|---|
| 链式映射 + 循环检测 | NewAPI | 我们 model_mapping 是**单跳拍板**（A→B 一步，无多跳展开）。链式映射引入循环检测复杂度，现实用例（直连名→代理名）单跳全覆盖。真出现多跳需求时再加，届时检测成本不变。 |
| 四轨计费 | sub2api | sub2api 的渠道概念（按渠道定价/限流/记账四轨）与我们架构不符——我们无渠道实体，凭据池 + model_catalog 倍率已覆盖成本归因。硬移植 = 引入伪渠道层，纯负担。 |
| OAuth 厂商前缀黑名单 | NewAPI | 无 OAuth Codex 上游（线上 4 凭据全 custom_api 透传 + ksk 未来）。前缀黑名单是防「OAuth 厂商把模型名加前缀」的场景，我们没有该上游形态。 |
| composite 别名路由 | NewAPI/sub2api | ModelSpec 虽有 owned_by/family，但我们不做「按 family 组合路由」——与白名单硬门语义冲突（白名单是显式授权，composite 是隐式推断），且选号已有 12 位键 + 正向路由（P1），composite 无独立价值。 |

## 8. 守卫规划（源码级）

| # | 守卫 | 内容 | 防什么 |
|---|---|---|---|
| 1 | 预判与改写共用同一函数 | `predict_passthrough_upstream_model` 与 forward 改写链逐分支对应（测试矩阵钉死 + 注释守卫）；改写链新增分支必须同步预判 | 预判漂移 → 统计失真 / P2 黑名单键 miss |
| 2 | 巡检与黑名单同源 | 巡检缓存键、黑名单键（P2）、白名单判定共用同一「改写后名 + normalize_model_key」键空间；mark 端实际下发名 == select 端 predict 名 | 两层证据各说各话 → 选号判据分叉 |
| 3 | 归一化单函数多消费 | 各消费点（巡检缓存/黑名单/限流桶）调用同一 `normalize_model_key`；`model_catalog::resolve` 不经 normalize | 各层各写一个「小写化」→ 键空间漂移 |
| 4 | 观察不改变计费 | 响应模型观察只写日志/观测字段；usage 埋点函数签名不动 | 观察顺手进 usage → 新口径分叉（本设计头号风险） |
| 5 | ErrorCode 枚举与文案分离 | 错误回显 `error.code` 是枚举常量，与 message 文案解耦；前端判定只认枚举 | 语言耦合重演（cooldownReason 教训） |
| 6 | 空列表不写缓存 | 巡检空列表 → 状态保持 Unknown | 上游暂时故障被固化为「无模型」 |
| 7 | 全候选 Unsupported 退化放行 | 全 Unsupported 时选号不返回无号错误 | 目录误判导致服务中断 |
| 8 | 排序键首位 support_rank | support_rank 首前插，后续 12 位键语义不动（对齐 CURRENT.md 排序键守卫纪律） | 正向路由悄悄改掉既有排序语义 |

守卫纪律沿用 CURRENT.md：needle 运行时拼接、测试段不写被守卫代码的完整字面量、include_str 用 `#[cfg(test)]` 截断。

## 9. 自 review 结论

**与现有机制冲突检查**：
- 黑名单：P1 正向缓存与黑名单共存（黑名单优先）；P2 改键不改造型。无冲突。
- 白名单硬门：`is_entry_selectable` 层先于排序，巡检缓存/P1/P2 都不触碰该层。无冲突。
- 严格语义：`effective_model` 本体不动，所有新链路以它或它的同源函数（predict）为键来源。无冲突。
- 吸收层：不参与选号与模型名链路，所有设计零交互。无冲突。

**新口径分叉检查**：三条语义各一个权威函数（§1 表格），新增消费点（巡检缓存、观察记录、错误回显、黑名单键）全部指向既有权威函数；唯一新函数 `normalize_model_key` 是纯键规范化且 P1 只在新链路使用，P2 切换时一次验证。观察路径（P1-2）与计费路径结构性分离（守卫 #4），是分叉风险最高处，已上双保险（函数签名不动 + 守卫）。

**可验证性检查**：每项验收都有测试/守卫（§5/§6 逐项列出）；关键行为变化（support_rank 前插、黑名单键共享）均为有意且标注「不会导致问题加重」的论证；P2 每项默认关或默认字节不变，可独立开关独立回滚。

**跨期依赖**：仅一处软依赖——§6.2 回写依赖登记 A/B/C 完成（否则掩盖字段 bug）；其余各期完全独立。软依赖通过「默认关 + 依赖方检查」化解，不构成硬阻塞。

**遗留风险（诚实披露）**：① P1 巡检目录是广告位不是承诺，Unsupported 压后语义依赖上游列表完整性，线上观察期需盯 Confirmed 号失败率；② support_rank 打乱均衡是有意变化，需要真实流量验证均衡收益；③ A7 effort 后缀依赖上游实测（native_thinking_effort_enabled 尚未上号验证），默认关待测。
