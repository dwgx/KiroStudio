# WebSearch 回灌路径流式化改造设计文档

> 状态：**设计稿，未实施**（上号实测前不动任何生产代码）
> 作者：只读研究员（证据型输出，每条论断带 `文件:行` 锚点）
> 关联审计：websearch 混合请求（tools 带 web_search）从真流式退化为「整轮缓冲 + 一次性渲染」

---

## 1. 现状全景

### 1.1 触发条件（什么请求走哪条路）

`/v1/messages`（handlers.rs:1968-1995）与 `/cc/v1/messages`（handlers.rs:3308-3336）两个端点共用同一套分流：

```
请求 → has_web_search_tool(payload)?
  ├─ true 且 should_handle_websearch_request  → 本地 MCP 快路径（websearch.rs:960 handle_websearch_request）
  │     条件：tool_choice 强制 web_search / tools 只有 web_search 单工具 / Claude Code 前缀
  │     （websearch.rs:378-385）
  ├─ true（其余情况，即混合工具）              → dispatch_web_search_loop（handlers.rs:443）→ agentic 回灌循环
  └─ false                                     → 常规转发（真流式 / buffered / 非流式）
```

- `has_web_search_tool` 是 any 语义（websearch.rs:296-300）：只要 tools 里**出现** web_search 就命中，不要求唯一工具。
- **关键事实：混合工具请求无条件进回灌循环，与模型是否真的调用搜索无关。** Claude Code 常态（tools 常驻 web_search + exec/bash/edit）下，一个普通问答请求也会走 `run_web_search_loop`。审计痛点「即使模型根本没搜」由此而来。

### 1.2 回灌链路与每一层的缓冲行为

```
dispatch_web_search_loop（handlers.rs:443-504）
  └─ run_web_search_loop（websearch.rs:1350-1473）       ← 整轮循环，await 完整才返回
       └─ run_round（websearch.rs:1277-1342）
            ├─ convert_request（转换 payload → Kiro 请求体）
            ├─ build_kiro_request_body_for_websearch（handlers.rs:416，带输入压缩，一次性）
            └─ call_api_stream（打上游）→ decode_round（websearch.rs:1095-1255）
                 └─ bytes_stream 逐 chunk 解码，但全部累积进 RoundOutcome（text/reasoning/
                    web_search/client_tool_use/context_input_tokens/credits）——整轮缓冲
       每轮判定 should_replay_round（websearch.rs:66-70）：
         - 纯 web_search tool_use 且未超上限（MAX_WEB_SEARCH_ROUNDS=5，websearch.rs:39）
           → 调 MCP（call_mcp_api）→ append_search_round 回灌进 payload（websearch.rs:92-151）→ continue
         - 否则收尾：presentation（各轮搜索结果块）+ 末轮 thinking/text/client_tool_use
           → WebSearchLoopSuccess（websearch.rs:1449-1458）
  dispatch 拿到 success 后（handlers.rs:470-491）：
    - stream=true → build_loop_sse_events（websearch.rs:1479-1628）一次性构造全部 SSE 事件，
      Body::from_stream(stream::iter(bytes)) 整包发出
    - stream=false → build_loop_json_body（websearch.rs:1631-1645）单条 JSON
```

**每一层的缓冲点（为什么）：**

| 层 | 缓冲行为 | 原因 |
|---|---|---|
| decode_round | 整轮上游流累积成 RoundOutcome | 必须等 tool_use 收完（工具参数 JSON 是分片到达的，`Event::ToolUse` 带 `stop` 标志，websearch.rs:1168）才知道「这轮是纯 web_search 还是要收尾」 |
| run_web_search_loop | presentation 跨轮累积 | 回灌轮的文本/思考**不展示给客户端**（见 1.4），只有搜索结果块进 presentation；最终回答在收尾轮 |
| dispatch_web_search_loop | **整个循环结束才构造 Response** | 这就是 TTFB 问题的根因 |
| build_loop_sse_events | 事件一次性预构造 | 上游已结束，无增量可发 |

### 1.3 TTFB 影响的具体位置

- **客户端第一个字节 = 循环全部完成之后**：`dispatch_web_search_loop` 在 `run_web_search_loop(...).await` 返回前不构造任何响应（handlers.rs:463-491）。HTTP 响应头 + 首个 SSE 事件都在 `WebSearchLoopSuccess` 到手之后才写。
- TTFB ≈ **所有轮的上游流时长 + 所有 MCP 搜索时长之和**（最多 5 轮）。
- 对比基线：
  - 主路径真流式（ccAutoBuffer=false）：`handle_stream_request`（handlers.rs:2190-2245）在 `call_api_stream` 返回后立即构造 SSE 响应，`create_sse_stream` 边收边转（handlers.rs:2233），TTFB ≈ 上游首个 chunk。
  - 主路径 buffered（ccAutoBuffer=true）：先返回 SSE 响应、每 25 秒发 ping 保活（handlers.rs:2347-2496），内容憋到流末。**TTFB（首字节）小，内容 TTFB 大**。
  - **回灌路径：连 buffered 都不如** —— 首字节 = 整轮结束。无 ping、无任何中间信号。

### 1.4 「刻意不做首字节握手」的注释位置与理由

websearch.rs:1057-1059（机制总注释块内）：

```rust
// **刻意没照抄参考仓的 1269 行**（本轮范围控制）：不做 thinking/redacted_thinking 回灌、
// 不做 metering/cache 精算、不做首字节 marker 的 SSE 提前握手。这些都是既有 StreamContext
// 已覆盖或本轮不必要的能力，先让机制跑通。
```

理由自述：「先让机制跑通」（当时范围控制）。审计已判定该理由不再成立——机制已跑通并稳定，现在 TTFB 代价不可接受。

### 1.5 回灌轮文本被丢弃（内容语义边界，方案设计的硬约束）

- `append_search_round`（websearch.rs:92-151）：回灌轮的 `assistant_text` 写回 payload 历史（给上游看，websearch.rs:101-103），但**不进** presentation（面向客户端的块只有 `server_tool_use` + `web_search_tool_result`，websearch.rs:129-144）。
- 收尾渲染的 content = presentation（各轮搜索结果）+ **末轮** thinking/text + 末轮 tool_use（websearch.rs:1418-1445）。
- **推论：回灌轮的文本/思考对客户端最终不可见。** 任何「轮内实时转发」方案在回灌轮上都会向客户端发布「幽灵内容」（发了 thinking_delta/text_delta，最终响应里却没有对应块）——协议层面是错的。**内容可发布性的边界是「轮」，不是 token。** 这是参考仓也只做首字节握手、不做真 token 级流式的原因（见 §2）。

---

## 2. 参考仓方案（/tmp/ref-grey @795b9ca）

### 2.1 它怎么处理「websearch 轮次 + 真流式」

参考仓的入口分流（ref handlers.rs:650-714）：纯单工具 web_search → MCP 快路径；`has_web_search_tool || has_web_search_among_tools`（含混合工具）→ `websearch_loop::run_web_search_loop(provider, payload, hook, payload_stream, ...)`——**流式/非流式的分流在 loop 内部**（ref websearch_loop.rs:689-732）。

流式分支走 **`render_deferred_sse`**（ref websearch_loop.rs:734-815），核心机制：

```
1. 建 mpsc channel（SseBytes）+ oneshot channel（StreamStartup）
2. spawn 后台任务跑 run_web_search_loop_inner，携带 StreamFirstByteMarker（ref :49-95）
3. decode_round 每收到上游一个 chunk，marker 首次触发（ref :212-214）：
     - 立即向 mpsc 发 `event: ping`（ref :97-99 create_ping_sse）
     - oneshot 发送 StreamStartup::Started
4. 主任务 await startup_rx（ref :792-814）：
     - Started → 立即返回 200 + SSE 响应（Body::from_stream(unfold(rx))）
     - Failed(resp) → 返回错误响应（首字节前失败，错误码尚可表达）
     - 超时/断 → 502
5. 后台任务循环结束后 mark_started_before_final_flush（ref :760），把整轮事件灌进 mpsc
   首字节后失败 → 只能发 error SSE（ref :780-787），因为 200 已发出
```

### 2.2 它的权衡

| 维度 | 参考仓 | 本仓现状 |
|---|---|---|
| 响应首字节 | **上游首个 chunk 到达即回**（ping 握手） | 整个循环完成后才回 |
| 内容粒度 | **轮级**：每轮收完后 flush 整轮事件（decode_round 仍是整轮缓冲，ref :182-287） | 一次性（比轮级更粗：全部轮完成后才发） |
| 中间保活 | 仅首 chunk 一个 ping，**轮间无保活**（2-5 轮长轮时无信号） | 无任何信号 |
| 错误表达 | 首字节前失败 → 非 200 正确表达；首字节后失败 → 只能 error SSE（200 已发） | 失败一律非 200（因为响应还没发） |
| message_start | 收尾时发，input_tokens 用末轮精确值 | 同左 |

**参考仓有的而本仓没有的机制 = 首字节握手（ping + oneshot 竞争 200/错误码）。** 参考仓的 TTFB（首字节）≈ 上游首 chunk 延迟，本仓 TTFB = 循环总时长。但注意：**参考仓内容 TTFB 与本仓同数量级**（都是整轮/整循环缓冲，参考仓只是粒度细到「轮」）。

### 2.3 参考仓没解决的（改造时不要神化它）

- 轮间无保活：一轮 60s+ 时客户端只看到开头一个 ping。
- 回灌轮文本同样丢弃（append_search_round ref :418 同语义），它也没做 token 级流式。
- `render_deferred_sse` 无单元测试（ref websearch_loop.rs 11 个测试全是纯函数级：should_search_round / build_result_block / map_provider_error）。

---

## 3. 约束与测试

### 3.1 现有测试（websearch.rs 内 26 个 `fn test_`，加 converter/stream 相关）

**改造后不受影响的（纯函数，签名不变）**：
- `loop_sse_events_render_each_block_type_in_order`（websearch.rs:2349）、`loop_sse_events_render_thinking_block`（:2424）、`loop_json_body_carries_content_and_usage`（:2468）——测 `build_loop_sse_events`/`build_loop_json_body` 输入输出，只要这两个函数保留就绿。
- `should_replay_only_when_round_is_pure_web_search`（:2307）、`append_search_round_*`（:2200/:2281）、`build_result_block_*`（:2335）、快路径全套（:1680-2131）。

**受改造影响的（结构守卫，改了结构会故意红）**：
- `replay_loop_must_not_swallow_mcp_failure`（websearch.rs:2495-2519）：用 `include_str!` + 源码文本 split，断言 `run_web_search_loop` 函数体含 `BAD_GATEWAY` 和 `should_replay_round`。**若重构时把循环主体改名（如拆出 `_inner`）必须同步改守卫的 split 锚点。**
- `replay_round_must_reject_truncated_upstream_round`（websearch.rs:2525-2549）：断言 `run_round` 函数体含 `outcome.stream_error` 与 `outcome.upstream_error`。`run_round` 加参数不影响（锚点是函数名字面量）。

**没有的测试（改造时应补）**：无任何测试覆盖「响应何时返回/首字节时序/轮间信号」。参考仓也没有，需自行设计。

### 3.2 守卫对改造的结构约束（结论）

- `run_web_search_loop` 的**函数名与主体结构**被文本级守卫锚定：改造时要么保留该函数名并把新逻辑放内部/下游，要么同步改守卫。
- 干净路径：**保留 `run_web_search_loop` 名字**，在其下方新增 `render_deferred_sse` 风格的分发函数（参考仓结构），`dispatch_web_search_loop`（handlers.rs:443）只改调用点。
- `BAD_GATEWAY` 必须继续出现在循环主体里（MCP 失败 502 语义不可变）。

### 3.3 ccAutoBuffer / 压缩重试循环与 websearch 的交互

- **websearch 回灌在压缩重试循环之外**：`dispatch_web_search_loop` 在 handlers.rs:1993（/v1）与 :3334（/cc/v1）**早退**，位于 `'compress_retry: loop`（handlers.rs:2092）之前。回灌路径的压缩只有 `build_kiro_request_body_for_websearch`（handlers.rs:416-428）的一次性压缩，**无 CONTENT_LENGTH_EXCEEDS 重试**（dispatch 里明确注释「回灌路径无压缩重试循环，标记无消费者」，handlers.rs:497-499，并负责剥离 `x-kirostudio-compress-retry` 头）。
- **websearch 回灌不受 ccAutoBuffer 影响**：ccAutoBuffer 判定在 handlers.rs:2125（buffered 分发），同样在回灌早退之后。即：**无论 ccAutoBuffer 开/关，带 web_search 的混合请求永远走「纯憋」回灌路径**——开缓冲的用户以为自己在用 ping 保活版，实际上比 buffered 更差（无 ping）。
- 回灌路径的 `emit_websearch_loop_usage`（handlers.rs:513-538）记 `is_streaming=false` 单条记录（N 轮上游往返只记 1 条，`retries = rounds-1`）。改造流式通道后**不建议**改这个记账语义（与 MCP 路径同源，且面板已习惯该口径）——但 `record.is_streaming` 对客户端拿到 SSE 的请求置 false 略不诚实，可作为上号时顺带评估项。

---

## 4. 方案设计

### 方案 A：整轮缓冲 + 提前发 message_start（TTFB 不变，感知改善）

**机制**：在循环开始前（或第一轮第一个上游 chunk 到达时）先返回 SSE 响应，发 `message_start`（input_tokens 用本地估算 fallback）；循环结束后继续发 content 块 + message_delta + message_stop。

**改动范围**：
- handlers.rs:443-504（dispatch）：改成参考仓 render_deferred_sse 的双通道结构，但 startup 信号改为「循环开始即 Started」。
- websearch.rs:1479（build_loop_sse_events）：拆成「头部事件（message_start）」与「主体事件」两部分，头部提前发。

**TTFB**：首字节 = 循环开始（≈0 延迟），但**内容 TTFB 不变**（仍等整循环）。

**风险**：
- `message_start` 的 input_tokens 从「末轮精确值」降级为「本地估算」——主路径真流式（ccAutoBuffer=false）本来就是估算值（handlers.rs:2226 generate_initial_events 用传入的 input_tokens），CC 流式主路径无抱怨记录 → **风险低，与既有基线同口径**。
- 提前发 message_start 后若循环失败：200 已发，只能 error SSE（语义劣于现在的非 200）。需要「首字节前失败仍返回错误码」的竞争逻辑（即方案 C 的 oneshot 机制）——**A 不引入该机制时，必须等第一轮上游 chunk 后才发 message_start**，与 C 的握手时机相同，A 实际退化为 C 的变体。

**实测点**：无 TTFB 内容提升，只验感知（不建议单独上，作为 C 的退路）。

### 方案 B：边收边转（每轮收完立即转 SSE，轮间保活）

**机制**：`run_web_search_loop` 内每轮 decode 完成后，**立即**把该轮可发布内容转成 SSE 发出去：
- 回灌轮：可发布 = 搜索结果块（server_tool_use + web_search_tool_result），一轮一发；
- 收尾轮：可发布 = 末轮 thinking/text/tool_use + message_delta + message_stop；
- message_start 在首轮前发；轮间发 ping 保活。

**改动范围**：run_web_search_loop（websearch.rs:1350）改造成带发送端口的循环；dispatch（handlers.rs:443）改双通道；build_loop_sse_events 拆段。

**TTFB**：内容首字节 = 第一轮上游流结束。比现状好（一轮 vs 多轮），但**不是 token 级**。

**风险（关键）**：
- **回灌轮文本/思考不能发**（§1.5）：decode_round 已把它们累积进 RoundOutcome，本轮决定回灌时它们对客户端不可见。若误发 → 客户端收到「幽灵内容」。实现上必须区分「回灌轮」与「收尾轮」的发布内容，**边界恰好是 should_replay_round 的判定时机**——而该判定必须在整轮收完后才能做（tool_use 分片到达）。所以 B 的发布粒度天然是「轮」，无法到 token。
- 「模型根本没搜」的首轮即收尾场景：B 的收益 = 第一轮上游流结束即发（≈主路径真流式的 TTFB 减去最后一小段）。这类请求是 Claude Code 常态，B 收益可观。
- 结构守卫：run_web_search_loop 签名改动最小化（见 §3.2）。
- **index 语义**：content block index 必须在发布时确定且不回退——轮级发布天然满足（每轮追加）。

**实测点**：首轮即收尾场景的 TTFB vs 主路径真流式；多轮场景下客户端是否看到「搜索结果块先到、最终文本后到」（对 SDK 合法，但 UX 上是否可接受需实测）。

### 方案 C：参考仓式首字节握手（先发 ping 再轮级批量）★ 参考仓实证

**机制**：照搬 ref websearch_loop.rs:734-815 的 `render_deferred_sse`：
- 后台任务跑循环，`StreamFirstByteMarker` 在上游**首个 chunk** 触发：发 ping + oneshot Started；
- 主任务等 oneshot：Started → 200 + SSE 流；Failed → 错误码；其余 → 502；
- 循环结束 → mark_started_before_final_flush → 整批事件灌 mpsc；首字节后失败 → error SSE。

**改动范围**：
- websearch.rs 新增 `render_deferred_sse` + `StreamFirstByteMarker` + `create_ping_sse`（拷贝 ref :44-115，约 70 行）+ `run_web_search_loop_inner`（把现有循环主体搬进去，加 `first_byte_marker: Option<&mut StreamFirstByteMarker>` 参数贯穿 decode_round）；
- handlers.rs:443-504 dispatch 的 stream 分支改调 `render_deferred_sse`（或把分流移进 websearch.rs，对齐参考仓）；
- decode_round（websearch.rs:1095）加 marker 触发点（ref :212-214）。
- **不需要**动 build_loop_sse_events（事件序列不变）。

**TTFB**：首字节 = 上游首个 chunk（**真流式的 TTFB 水平**）。内容仍整循环批量（与现状同）。

**风险**：
- 首字节后失败只能 error SSE（200 已发）——与主路径真流式同语义（上游中途断流本来就是 error SSE），客户端已惯用。
- 结构守卫：`run_web_search_loop` 名字保留在 inner 入口，守卫锚点兼容（函数名仍在，函数体里 BAD_GATEWAY/should_replay_round 还在）。
- ping 事件是 `event: ping`，与主路径 buffered 的 ping（handlers.rs:2300-2301 同款字符串）一致，客户端已知处理。
- 轮间无保活（参考仓同款弱点）：可选加「每轮结束发 ping」，一行代价，建议加上。

**实测点**：TTFB 是否 = 上游首 chunk；首字节后失败的表现（error SSE 是否被 SDK 正确显示）；ping 不被客户端误当成内容。

### 方案 D：混合（按是否真命中 web_search 决定）

**机制**：C 的握手 + 在**首轮结束后**决定内容粒度：
- 首轮即收尾（模型没搜，Claude Code 常态）→ **走主路径真流式**（复用 StreamContext/create_sse_stream，边收边转，token 级）；
- 首轮纯 web_search（真搜了）→ 继续 C 的轮级批量（回灌轮内容不可发布，token 级无意义）。

**改动范围**：C 全部 + run_round 改造为「可旁路到 StreamContext」的流式调用；handlers 的 dispatch 需要拿到「首轮判定」后才定分发形态。改动最大。

**TTFB**：没搜场景 = 真流式（token 级，最优）；搜了场景 = C 的水平。

**风险**：
- 首轮判定发生在**上游流结束**时——「没搜」判定出来时流已经结束，此时转真流式已无增量可发，**D 的「没搜 → 真流式」分支实际无法兑现 token 级收益**（除非首轮中途就能判定模型不搜——不可行，tool_use 可能最后才出现）。
- 结构性最复杂，改造面最大，收益落空。**不建议。**

---

## 5. 方案对比与推荐

| 维度 | A 提前 message_start | B 边收边转 | C 首字节握手 | D 混合 |
|---|---|---|---|---|
| 首字节 TTFB | ≈0（循环开始即发） | 第一轮流结束 | 上游首 chunk | 同 C |
| 内容 TTFB | 整循环（不变） | 第一轮结束（没搜场景优） | 整循环（不变） | 没搜=整循环（无增量） |
| 内容粒度 | 一次性 | 轮级 | 轮级 | 轮级 |
| 正确性风险 | 低 | **中（幽灵内容边界）** | 低 | 低 |
| 改动量 | 小 | 中 | **中（参考仓实证模板）** | 大 |
| 结构守卫影响 | 低 | 中 | 低（保留函数名） | 高 |
| 参考仓实证 | 无 | 无 | **有（生产运行）** | 无 |

### 推荐：方案 C（参考仓式首字节握手）+ 两个小增强

1. **C 是参考仓在生产运行的同款机制**（ref websearch_loop.rs:734-815），风险已被另一条代码基线实证，本仓可逐行对照移植，改动面集中在 websearch.rs 单文件 + dispatch 一个调用点。
2. **B 的 token 级收益在回灌路径上不存在**（§1.5：回灌轮内容不可发布；§4-D：没搜判定出来时流已结束）。B 的唯一增量收益是「每轮结束发该轮搜索结果块」——这属于 UX 优化而非 TTFB 修复，且引入「幽灵内容」边界风险，不值得为它承担正确性风险。
3. **D 无法兑现宣称收益**（首轮判定时机在流末），且改动最大。
4. 增强①：**每轮结束发一次 ping**（参考仓只在首 chunk 发一次，2-5 轮长轮间无信号；本仓主路径 buffered 已有 25s ping 先例，handlers.rs:2347-2496）。一行代价，消除长轮连接被中间设备掐断的风险。
5. 增强②：message_start 保持在**收尾时发**（与参考仓一致），input_tokens 用末轮精确值——不降级准确性，不做方案 A 的估算值妥协。

**不推荐把改造目标定成「token 级真流式」**：回灌链路的语义决定了内容只能轮级发布，这是协议/正确性约束，不是实现偷懒。用户感知的「从真流式退化」实质是 **TTFB 从上游首 chunk 恶化到整循环结束**，C 恰好把 TTFB 恢复原水平，且不引入任何内容语义风险。

---

## 6. 上号实测清单（改造后、上线前必做）

用真实上游（Kiro）验证，重点测「模型根本没搜」的混合工具请求（Claude Code 常态，也是用户感知最强的场景）：

| # | 场景 | 指标 | 通过标准 |
|---|---|---|---|
| 1 | 混合工具 + 普通问答（模型不搜） | 首字节 TTFB | ≈ 主路径真流式水平（上游首 chunk 即回），不再 ≈ 整轮时长 |
| 2 | 同上 | 首个 content 事件（message_start）到达时距 | message_start 在循环结束后立即出现（不应有额外延迟） |
| 3 | 混合工具 + 真搜索（1 轮） | 全程事件序列 | message_start → server_tool_use → web_search_tool_result → 最终 text → message_stop，顺序与现状完全一致（回归） |
| 4 | 混合工具 + 真搜索（2-5 轮） | 轮间信号 | 每轮结束有 ping，客户端连接不被掐 |
| 5 | 首字节前上游失败（如 401/400） | 状态码 | 非 200 错误响应（oneshot Failed 分支生效） |
| 6 | 首字节后上游断流 | 错误表现 | error SSE 到达，客户端显示错误而非挂死 |
| 7 | CC 客户端（stream=true）+ ccAutoBuffer 开/关 | 行为一致性 | 两种配置下 TTFB/事件序一致（回灌路径不受 ccAutoBuffer 影响） |
| 8 | 非流式请求（stream=false） | 回归 | 单条 JSON，与现状完全一致 |
| 9 | 内存 | 长回答内存占用 | 无新增全量缓冲（现状已有；确认改造未加重） |
| 10 | 无 web_search 的普通请求 | 回归 | 不进回灌路径，行为零变化 |

---

## 7. 代码准备清单

### 现在就做（行为完全不变，只铺路）

1. **抽取 `render_deferred_sse` 骨架为纯新增函数**（websearch.rs 新增，未被调用 → 无行为影响）：`StreamFirstByteMarker`、`create_ping_sse`、双通道组装逻辑，逐行对照 ref websearch_loop.rs:44-115/734-815。上号时只需接线 dispatch + 让 decode_round 触发 marker。
2. **在 websearch.rs:1057-1059 注释处追加改造指针**：注明「首字节握手已列为待办，机制模板见 ref websearch_loop.rs:734」，避免后来者误以为「刻意不做」是永久决定。
3. **把 `build_loop_sse_events` 拆成纯函数段**（message_start 头 / content 主体 / message_delta+stop 尾）：三个私有纯函数，输出拼接与现状逐字节一致（现状就是顺序 push，拆分零风险），上号时可直接复用任意段。现有 3 个相关测试继续全绿。
4. **补一个纯函数测试**：`build_loop_sse_events` 事件序列 = 拆段后拼接的事件序列（锁定拆分不改变行为）——这是拆分动作的回归网。
5. **准备实测脚本/记录格式**（docs/ 下，不碰代码）：curl + 时间戳命令模板，用于 §6 的 TTFB 测量；不写进仓库也行，但格式先定好。

### 必须等上号（行为变更本身）

6. decode_round（websearch.rs:1095）加 first_byte_marker 触发点（ref :212-214）——行为变更。
7. run_web_search_loop（websearch.rs:1350）改名/包一层 inner 并贯穿 marker——行为变更 + 需同步守卫（§3.2）。
8. dispatch_web_search_loop（handlers.rs:443-504）stream 分支接 render_deferred_sse——行为变更。
9. 轮间 ping（增强①）——行为变更。
10. `x-kirostudio-compress-retry` 剥离逻辑（handlers.rs:498-499）位置确认不因改造移动——纯检查项。

---

## 8. 验证状态声明（诚实边界）

- **已用 Read/rg 逐文件核实的**：本仓全部链路与行号锚点；参考仓 render_deferred_sse/StreamFirstByteMarker 机制全文；触发条件语义（any vs 单工具）；回灌轮文本丢弃行为（append_search_round）；测试清单与两个结构守卫的断言文本；ccAutoBuffer/压缩重试循环与回灌路径的先后关系（handlers.rs:1993 < 2125 < 2092 循环体）。
- **推断而非实测的**：①「回灌轮文本对客户端不可见」→ 由 append_search_round 的 presentation 构造推断（websearch.rs:117-150），未跑运行验证；②「首轮判定时机在流末」→ 由 tool_use stop 分片语义推断（websearch.rs:1168）；③ 方案 C 在本仓的移植兼容性（守卫文本 split 的脆弱性）→ 已核对守卫源码但未实际运行改造后的测试。
- **未做的**：未跑构建、未跑测试（本机 8GB 编不过，且任务明确禁止）；未读 .claude/state/CURRENT.md 的完整守卫清单（本任务不涉及文案/位置类守卫，若上号实施前应补读）。

**给实施者的最后一句话**：改造地图 = 在 websearch.rs 新增 render_deferred_sse（参考仓同款），dispatch 的 stream 分支改调它，decode_round 加 marker 触发，run_web_search_loop 主体包成 inner 并保留原函数名（守卫锚点），实测 §6 的 10 项。
