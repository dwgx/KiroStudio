# P2 两项设计细化：家族限流 scope（A5）+ 响应模型观察（A3）

> 状态：设计细化稿（2026-08-15，只研究 + 产设计，不改代码）。
> 依据：`docs/model-compat-plan.md`（P2 §6.1/§6.2 + P1 §5.2）、sub2api A5/A6 移植候选、
> 现状代码逐行核实（行号见各节）。
> 两份结论相反：**A5 不做**（无上游场景），**A3 做观察层**（零成本高价值），计费开关留 P2 A6。

---

## 设计 1：上游 429 按模型家族聚合限流 scope（sub2api A5）

### 1.1 现状盘点（已核实，行号为证）

**RpmTracker 模型维度（scheduling.rs:69-173）**：

- `model_hits: Mutex<HashMap<(u64, String), VecDeque<Instant>>>` —— 键 = `(凭据 id, 模型名)`，
  其中模型名 = **选号时的原始客户端模型名**（映射发生在选号之后，选号侧只有原始名，
  scheduling.rs:66-67 模块注释明示）。
- 60s 滚动窗口，`prune` 惰性逐出，**无独立 TTL**（窗口即 TTL）。
- `record_model`（:104）调用点全仓仅 2 处：`select_custom_api_inner` commit 分支
  （token_manager.rs:3272）与 Kiro 主路径 `commit_selection`。token_manager.rs:3370 有纪律注释：
  **只在这两处记，任何别处补记 = 双记**（RPM 翻倍虚高，排序键与饱和判定同时被污染）。
- `model_counts_for`（scheduling.rs:159）批量读，消费于排序键第 4 位
  （token_manager.rs:3211-3244：`priority → ramp_tier → rpm → model_calls → inflight`）。
- **模型级只是分流计数，不参与饱和判定**：scheduling.rs:68 明示「阈值/上限刻意不新增：
  模型级只是分流计数，饱和判定仍复用每凭据 rpm_limit」。rpm_limit 是每凭据配置
  （credentials.rs:97-102，全局默认 `credential_rpm_limit` config.rs:629），
  无模型级/家族级 RPM 阈值存在。

**429 处理路径（provider.rs:1953-2063，已核实）**：

- 429 ∈ `should_failover`（换号）+ `cooldown_custom_api(id, 5)`：**号级 5s 调度级跳过**。
- dwgx 定的语义（provider.rs:2009-2018 注释）：代挂号是付费中转站，429 只代表「它现在忙」，
  偶尔 429 只该 failover 不该留痕。429 不进 health、不计失败、不进 rate_limiter
  （`record_passthrough_result` 三态：429 不计成功也不计失败，token_manager.rs:3356-3361）。
- 模型黑名单（`mark_model_unsupported`，token_manager.rs:3317）**只记 model_not_found 类**
  （稳定属性，30min TTL），429 **从不进黑名单**（provider.rs:1962-1978 仅 `model_not_found` /
  `no available channel` / `model not found` / `unknown model` 四个关键词）。

**家族概念澄清（两个容易混的族，都是凭据维度）**：

- `family_key`（credentials.rs:947）= **凭据族**：`m365:{tenant}` / `aws:{account}` /
  `clone:{group}` / `cred:{id}`，用于 health/circuit 的**账号级连坐**。与模型家族完全无关。
- `model_catalog::Family`（Opus/Sonnet/Haiku/DeepSeek/Glm/Qwen/Minimax/Auto）是 **Kiro 生态
  解析**：`resolve` 对未知名（`gemini-*`、`claude-fable-5`、`gpt-*` 等透传池常态名）直接返回
  None（resolve_inner:823 strict 拒绝）。**不能直接用作透传池模型家族推导**。

### 1.2 家族推导分析

任务例子的「家族」形态：`claude-fable-5` 变体（claude-fable-5 / claude-fable-5-xxx 归一族）、
`gemini-*`（前缀族）。这两个例子本身就暗示两种粒度：

| 粒度 | 规则 | claude-fable-5 | claude-opus-5 | gemini-2.5-pro | deepseek-v4-flash |
|---|---|---|---|---|---|
| 一段（第一 `-` 前段） | `claude` | claude | claude | gemini | deepseek |
| 两段（前两段） | `claude-fable` | claude-opus | gemini-2.5 | deepseek-v4 |

- 一段粒度满足 gemini-* 例，但 claude 全族（opus/sonnet/fable/mythos）同桶 → 误伤面大；
- 两段粒度满足 claude-fable-5 例（变体形态归族），但 gemini-* 无法归到一族（gemini-2.5 vs
  gemini-3 分族）。
- 两种粒度互相矛盾，没有天然答案；若要覆盖两例需配置段数（`family_depth` 1 或 2），
  为一个收益未证实的场景引入配置面。

### 1.3 设计（若做：方案骨架，供否决后留档）

- **key 形态**：`family_hits: HashMap<(u64, String), VecDeque<Instant>>`，键值
  `family:claude-fable`（前缀推导，段数可配）。
- **记录环节**：`record_model` 同点加 `record_family`（同锁同窗口 60s，零新增锁）。
- **排序键**：家族计数插到 `model_calls` 之后第 5 位（只降权，不跳过、不参与饱和判定——
  守住 scheduling.rs:68「阈值刻意不新增」纪律）。
- **冷却判定**：家族级 429 计数器（新结构）达阈值 → 该号×家族排序降权（soft）。
  429 哲学（provider.rs:2009「偶尔 429 只 failover 不留痕」）下**绝不做家族级跳过/冷却**，
  否则家族过宽时一个健康模型被兄弟模型拖入 5s+ 调度跳过，违反号池容量守恒。
- **TTL**：RPM 窗口（60s 滚动）即可；家族 429 计数若做独立退避需新 TTL（30-60s），
  与号级 5s 冷却同量级。
- **与精确键关系**：精确模型计数（现状）保留为第一维度，家族是叠加维度，只影响排序降权。

### 1.4 风险与收益场景

**风险**：
- 家族过宽误伤：一段粒度下 claude-opus（健康）被 claude-fable（429）拖累降权 → 请求被
  分流到更贵的号，白付成本。
- 与 429 哲学冲突：任何「家族级跳过」都违反「429 不留痕」的既定语义（provider.rs 注释是
  用户拍板过的），只能做降权，降权收益又很弱（ramp_tier 已在 rpm 之前管住了速率跃升）。

**收益场景核查（我们的上游形态）**：
- #1 fuckopencode（deepseek 链）：429 = Console 周额度 / IP 日窗（200/天）——**账号级**；
- #2 deepseekapi（api.deepseek.com/anthropic）：账号级；
- #3 cursorapi（本机 8008 池）：池内号级；
- #4 pigcode（gpt 链）：账号级。
- **全部是账号级/凭据级 429，没有任何「上游对家族内多模型连续 429」的形态**。sub2api 的
  A5 场景（上游按模型家族限流，如 OpenRouter 对同一 family 的聚合配额）在我们 4 个上游
  都不存在。

### 1.5 结论：不做

1. **无场景**：线上 4 上游 429 全账号级，家族聚合限流没有对应真实问题；
2. **已有覆盖**：429 failover + 号级 5s 冷却 + ramp_tier 防跃升 + 模型级分流
   （model_hits 已存在）已经覆盖「连续 429 退避」的可用形态；
3. **家族推导粒度无解**：任务两例（gemini-* 一段 vs claude-fable-5 两段）互相矛盾，
   需配置面，为无场景功能引入配置不划算；
4. **误伤风险不对称**：降权（安全）无收益，跳过（有收益）违反 429 哲学。

**保留项**：`model_hits` 模型级分流维持现状；若未来接入按家族限流的上游
（OpenRouter 形态），届时再按 §1.3 骨架实现，配置默认关零迁移。

---

## 设计 2：上游响应模型观察（sub2api A3，P1 §5.2 细化）

### 2.1 现状盘点（已核实）

**透传路径响应解析（passthrough_think_filter.rs）**：

- 流式：`process_block`（:444-622）对**每个** SSE 事件无条件
  `serde_json::from_str::<serde_json::Value>(&data)`（:464）——`message_start` 分支
  （:597-612）在 mock_cache 关闭时拿到 `v` 后直接 `block.to_vec()` 返回原始字节，
  **v 已在手，读 `v["model"]` 是纯读字段，零额外解析**。
- 非流式：`filter_json_bytes_with`（:95-188）整体 `serde_json::from_slice`（:101），
  顶层 `v.get("model")` 顺手可读，同样零额外解析。
- **协议事实**：Anthropic SSE 流中 `model` 字段只在 `message_start` 声明一次；
  `message_delta` 无 model。所以流式路径的「首声明」就是唯一声明；「终声明」的实际含义 =
  非流式响应体顶层 model（唯一声明）与 message_start.model 的跨形态对比，或上游非标准
  地在 message_delta 带 model 的扩展读取。**首 ≠ 终在规范流里不会发生**（除非上游重发
  message_start 的违规流）——conflict 判定的重心是「响应自报名 vs 我们下发的名」。

**主路径（Kiro 池）——无数据源**：

- 上游是 AWS event-stream（Kiro 协议），`src/kiro/model/events/*` 事件结构**无 model 字段**
  （已 rg 全目录核实）。
- `message_start` 是网关本地生成（stream.rs:1639-1669），`"model": self.model`（:1669）
  即请求侧模型名（StreamContext.model）。
- **结论：响应模型观察只在透传池有数据源；Kiro 主路径无源可观察，设计上排除。**

**record emit 时机（关键约束）**：

- 透传成功路径在**响应开始前**就 emit：handlers.rs:2067-2087 —— input 本地估算、
  output=0、credits=None，`emit_record` 后 `return resp`。响应流的 model 观察值
  拿到时 record 已经落库。
- 因此观察结果**无法写入已 emit 的 RequestRecord**，观察落点只能是 upstream_trace 风格
  的日志/独立观测（P1 §5.2 + 守卫 #4 已定：观察只写日志/观测字段，usage 埋点函数签名不动）。

### 2.2 设计

**观察器挂点（复用现有解析，零新增解析）**：

1. 流式：`SseFilterState` 加两个字段：
   - `observed_model: Option<String>`（首声明，读 message_start 的 `v["model"]`）；
   - `sent_model: Option<String>`（我们下发的改写后名，构造 filter 时传入）。
   `message_start` 分支读 model 后**仍返回原始字节**（mock 关闭路径 block.to_vec() 不变）。
2. 非流式：`filter_json_bytes_with` 读顶层 model，同样不触发额外重序列化
   （content 早退分支 mock 关闭时仍 `copy_from_slice` 原字节）。
3. `filter_sse_stream_with` / `filter_json_bytes_with` 各加一个参数
   `observed_sent_model: Option<String>`（有 `mock_cache` 参数先例，签名风格一致）；
   passthrough.rs forward 两处调用点（:521 / :531）传映射改写后的名
   （`predict_passthrough_upstream_model` 或 forward 手头已算的 mapped_model）。

**记录（观察与 billed 结构性分离，守守卫 #4）**：

- conflict 判定（三态）：
  - 响应有 model 且 == 下发名 → 一致，仅计数；
  - 响应有 model 且 != 下发名 → **conflict**：限频 tracing::warn
    （credential_id / sent_model / response_model / is_streaming）+ 计数器；
  - 响应无 model（非标准上游）→ 不记录。
- 计数落点：模块级 `AtomicU64`（或复用 upstream_trace 通道，默认关时零开销）。
- **不进 RequestRecord、不改 usage 埋点签名、不改 billed 路径**——守卫 #4 直接适用。

**billing_model_source=response_model（计费开关）**：

- 本设计**只做观察层，不做计费开关**。理由：
  1. 透传 record 在响应前 emit，响应名要进 by_model 必须引入「观察槽 + 补写/延迟 emit」
     通道，与现有 emit 时机架构冲突，是 P2 A6（model-compat-plan §6.2）的完整课题；
  2. A6 依赖登记 A/B/C 完成（软依赖），且需要观察数据先验证响应名可信度（首终一致 +
     可解析门）——顺序是：观察（本期）→ 验证 → 回写（P2 A6）。
- 默认关 = 现状字节不变，可独立回滚。

### 2.3 性能与语义风险

**性能**：
- 解析成本：零（v 已在手，纯字段读取）。
- 序列化成本：零（观察只读，message_start mock 关闭路径仍返回原始字节；
  非流式 content 早退分支仍 copy_from_slice）。
- 唯一新增：每流一次字符串比较 + 每请求至多一条 warn 日志（限频）。

**语义风险（网关改写时响应回什么名）**：
- 透传零转换铁律下，响应是上游**原样**——上游回的名 = 它实际服务的模型名
  （deepseek 系中转回 deepseek-v4-flash，即使客户端发 claude-opus-5 被我们改写）。
- 所以「响应名 != 客户端原始名」是**常态**（改写链生效即如此），**不是 conflict**；
  conflict 只当「响应名 != 我们下发的改写后名」时成立（上游偷换模型 / 上游别名解析
  覆盖我们的名 / 上游多模型负载均衡）。判定必须用下发名做基准，不能用客户端原始名。
- 该判定恰好是 A6 回写的前置证据：响应自报名与改写链预测名长期不一致 = 成本归因失真
  的真源头（fuckopencode 类上游实测关注点）。

### 2.4 结论：做（观察层，零行为变化）

- **收益**：A6 回写的前置证据；发现上游替换模型（成本归因失真根因之一）；与 P1 §5.2
  目标完全一致，属 P1 范围而非 P2（观察先于计费）。
- **成本**：~50 行 + 测试（流式/非流式各 3 态 + 字节不变守卫）。
- **主路径不做**：无数据源（Kiro 协议事件无 model 字段），文档写明即可。
- **计费开关不做**：留 P2 A6，依赖登记 A/B/C + 观察可信度验证。

**改动面清单**：

| 文件 | 改动 |
|---|---|
| `src/kiro/passthrough_think_filter.rs` | SseFilterState 加 observed/sent 字段；message_start 分支读 model；filter_json_bytes_with 读顶层 model；两函数加 sent_model 参数；conflict 限频日志 + 计数 |
| `src/kiro/passthrough.rs` | forward 两处调用点（:521/:531）传改写后名 |
| 测试 | 流式一致/不一致/缺 model；非流式一致/不一致；**观察后字节与未观察逐字节相同**（mock 关闭路径）；conflict 判定用下发名不用客户端名 |

---

## 自 review

**与现有机制兼容论证**：

1. **RpmTracker / 排序键**：A5 否决 → 零触碰 `model_hits`、零触碰 12/5 位排序键
   （token_manager.rs:3211-3244 与 :3370 双记纪律不受影响）。A3 观察器在响应侧，
   选号侧（RpmTracker 记的是选号时点）完全不相关，无交集。
2. **透传零转换铁律**：A3 观察**只读不写**——message_start mock 关闭路径返回原始字节
   （block.to_vec()，passthrough_think_filter.rs:611），非流式 content 早退分支
   copy_from_slice（:136），conflict 只打日志。观察改造后 bytes 逐字节不变由测试钉死。
3. **观察与 billed 分离（守卫 #4）**：观察落点 = 日志 + 计数，RequestRecord/emit_record
   签名零改动；record 先 emit 的时机（handlers.rs:2067）正是结构性隔离的现成墙——
   观察值物理上到不了 billed 路径。计费开关留 P2 A6（§6.2 软依赖登记 A/B/C）。
4. **与黑名单键空间（P2 §6.5）**：A3 的 conflict 用「下发名」做基准，与黑名单键
   （映射后名）同口径同源（`predict_passthrough_upstream_model`），不引入第三套键空间。
5. **家族 scope 与凭据 family_key**：A5 不做后无新增家族概念；凭据族（credentials.rs:947）
   保持唯一「族」语义，无命名混淆。

**诚实披露**：
- A3 的「首 vs 终」在 Anthropic 规范流里通常无差异（model 只在 message_start 声明），
  跨形态对比（流式 vs 非流式同请求不可能共存）也不成立——实际 conflict 只剩「响应名 vs
  下发名」一态；文档已把判定重心放对，避免为不存在的「首≠终」造机制。
- A5 结论依赖上游形态判断（4 号池 429 全账号级）；若未来接入按模型家族限流的上游
  （OpenRouter 形态），§1.3 骨架可复用，默认关零迁移。
