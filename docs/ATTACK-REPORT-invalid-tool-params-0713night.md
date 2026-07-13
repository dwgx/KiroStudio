# 攻坚报告 · Invalid tool parameters / 幻觉token / 空回复 / mid-response

> 0713night ultracode 并发攻坚(12 agent，10 完成，全 Opus 4.8 high effort）+ 客户端源码逐行坐实 + 两语料实测。
> **本报告是方案，未改任何代码。等 dwgx 审后再实现。**
> 铁律贯穿全篇：**最坏情况 == 不开该修复，绝不破坏正常流。**

---

## 一、症状与真因总表

| 症状 | 真因分类 | 网关能修? | 一句话 |
|---|---|---|---|
| **Invalid tool parameters**（JSON_PARSE 路径） | A：参数内容坏 | ✅ 能修 | 客户端对 `partial_json` 直接 `JSON.parse`，失败即钉 `__unparsedToolInput`。网关在发给客户端前把坏 JSON 修好即根治。 |
| Invalid tool parameters（ZOD_VALIDATION 路径） | B：JSON 合法但字段不符 schema | ❌ 碰不到 | 模型少给/给错字段，客户端 zod 校验失败。网关拿到的是合法 JSON，改字段=伪造参数。 |
| Invalid tool parameters（schema 没下发） | A：请求侧 | ✅ 能修 | 工具 schema 被裁剪→模型盲吐字符串化参数→客户端拒。网关审计 tools 载荷完整性。 |
| **幻觉/泄漏 token**（court/card/call 进正文） | B：模型侧 | ⚠️ 可缓解 | 高多字节密度紧邻工具标签，模型吐控制 token。网关只能行首保守清洗**文本 delta**里的，且误删风险高。 |
| **工具调用被当文本**（`<invoke>` 丢 antml 前缀） | B：信封坏 | ❌ 碰不到（可检测重试） | 模型丢命名空间前缀，无 tool_use 帧。网关无从重构，但可**检测+触发重试**。 |
| **空回复**（output_tokens=0 + end_turn） | C：空产出 | ✅ 可缓解 | 上游只发 metering 帧、无内容。网关当前**静默当成功**，应加检测+可选重试。 |
| **mid-response server error** | A/基础设施 | ✅ 已修+可硬化 | reqwest 总超时硬掐长流（已改 read_timeout）；解码中断已补发 SSE error。 |

**关键认知（客户端源码 `claude-main.pretty.js` 坐实）**：
- 用户看到的字面 **"Invalid tool parameters" 是 UI 折叠标签**（`:2582`）——JSON_PARSE / ZOD_VALIDATION / schema-未下发 三种截然不同的根因在 UI 上长成同一句话。排障必须看底层 `errorCode`，不能只看这句话就判类型。
- 唯一的 JSON 解析点在 **`content_block_stop`**（`lJr` @ `:12034`），流式期间只做字符串拼接不解析。**网关的介入面 100% 精确**：缓冲 tool_use 的全部 `input_json_delta`，在发 `content_block_stop` 前校验+修复+重发单条 delta。这正是 KiroStudio「缓冲到 stop 发单 delta」的现有形态。

---

## 二、可落地修复清单（按 收益×安全 排序，只留通过对抗复核的）

### 🟢 P0-1　补 court/card/call 到泄漏 token 清单 —— 但**必须同时收严+加 Claude 门控**（否则是净负）

**这是最高频症状（court 在两语料里 202 次全部独占整行），但对抗复核揪出两个被严重低估的前提，不处理就会误删线上 Claude 正文：**

1. **清洗开关默认是 `true`（不是之前以为的默认关）** —— `handlers.rs:112`。误删是**线上默认行为**。
2. **`clean_leaked_tokens` 没有模型门控** —— `stream.rs:1236` 对所有模型无条件跑，**主力 Claude 路径正文也在被行首剥词**（对比 DSML 剥离在 `:1090` 明确排除 claude/opus/sonnet）。

**落地（收严优先于扩词）：**
- **先加 Claude 系模型门控**（复用 `dsml_filter_applicable` 的排除逻辑）——Claude 不产生这类泄漏 token，对 Claude 跑清洗是纯误删面。这一步比扩词更重要。
- court/card/call 加入清单后，判据要比现状更严：**仅当词后紧邻 CJK 码点**才剥；**排除冒号/数字/加号/点号/大写跳变**（这几类在正常英文/代码里高发：`count++`、`care.method()`、`Card:`、`count:5` 都会被现判据误删成 `++`/`.method()`/`:`/`：5`）。
- **court 独占整行的特例**：现 `strip_leaked_prefix` 遇「词独占整行、后面 None」走保守 `return line` **不剥**——所以 202 次独占行的主场景当前逻辑抓不到。可为 **court 单独**开「独占整行即剥」（court 独占一行几乎必是幻觉，英文 court 极少独占成行）；但 **call/card/count 独占行不可这样**（可能是正常内容/变量名）。
- 位置：`stream.rs:1183` (LEAKED_CONTROL_TOKENS) + `:1211` (strip_leaked_prefix) + `:1236` (加门控) + `handlers.rs:112`（评估默认值）。
- 单测：新增 court 独占行、`Card:`/`count++`/`care.method()` 不误删、Claude 门控生效。
- 默认：清洗保持开，**但加 Claude 门控**后 Claude 路径实际不跑。

**对抗复核否决项（不要做）：**
- ❌ **case-insensitive / 加大写变体（Court/Card/Call）** —— 大写首词恰是正常英文句首/标题高发位，泄漏 token 实测全小写。加了误删面爆炸。
- ❌ **递归剥多层 / compound re-scan** —— 会啃穿 `cardCount`、`courtcase` 这类正常驼峰/复合词。compound 只按整词精确加。

---

### 🟢 P0-2　尾逗号（trailing comma）修复 —— LLM 最高频非法 JSON，当前完全漏网

`{"a":1,}` / `[1,2,]` 被 serde 严拒，是 LLM 生成 JSON 的**头号**非法模式（高于 Windows 路径），但现有 char 层（只碰字符串内部）+ struct 层（只补闭合符）**双双不碰**，坏 JSON 原样发客户端。

- 落地：在 `repair_tool_json`（`stream.rs:2243`，char_fixed 之后 struct_fixed 之前）插一层 **string-aware `strip_trailing_commas`**：`in_string=false` 且遇 `,` 后跳空白，下一非空白是 `}`/`]` 则删该逗号。复用 `scan_tool_json` 的 in_string 判据保证字符串内逗号不动。
- 风险：极低。尾逗号在 JSON 里本就无语义，零语义损失。守铁律（只在 from_str 已失败时进、复验通过才用）。
- 单测：`{"a":1,}`、`[1,2,]`、`{"csv":"a,}"}`（字符串内逗号不动）。
- 默认：开（纯增益）。

---

### 🟢 P1-1　空回复检测 —— 当前被静默当成功（流式+非流式双命中）

上游只发 metering/contextUsage 帧、无 assistantResponse 也无 toolUse 时：`output_tokens=0`、stop_reason 回退 `end_turn`、completion 仍 Ok → **记 Success**。客户端把「模型没说话」当正常完成，不重试。与症状 `output_tokens=0 + end_turn` 逐字吻合。

- 落地（两档，遵默认纪律）：
  - **A 档（默认开，纯观测不改流程）**：加 `is_empty_success` 判据（`completion.is_ok() && output_tokens==0 && !has_tool_use && !has_non_thinking_blocks() && stop_reason=="end_turn"`），命中则 `tracing::warn` + 新增 `RequestOutcome::EmptyResponse` 分类（**只进 stats 不驱动 cooldown**，核实 `on_record` 不碰冷却）。行为完全不变，只是记账不再把空回复混进成功率。
  - **B 档（新开关 `empty_response_as_error` 默认关，改流程）**：收尾改发 SSE error/502 让客户端退避重试。**绝不 report_failure 连坐号**（空回复≠号坏）；**绝不在网关内自动重发上游**（双计费+可能循环）。
- 判据必须排除：thinking-only（`stream.rs:1871` 已补空格，不算空）、`max_tokens`、`model_context_window_exceeded`。
- 位置：`stream.rs:1775` generate_final_events + `handlers.rs:1224` 非流式收尾 + `usage/record.rs` 加 outcome。
- 客户端侧佐证（源码）：空 content + end_turn 客户端**不报错**，渲染成 `(no content)`；真正致命的是**信封不完整**（缺 message_start 或缺 stop_reason）会触发非流式回退。所以 B 档若走 error 一定要发完整信封。

---

### 🟡 P1-2　检测「正文疑似含未执行工具调用」→ 触发重试（不重构，默认关）

Bug B（`<invoke>` 当文本）**网关不能重构**（见第三节），但可以**检测+重试**，复用现成的 truncation-recovery collar-safe 管线。

- 落地（三档，务必按序）：
  1. **纯观测**（仿 `classify_tool_json_defect`，只打日志不进控制流）：加 phantom-tool-call 计数/trace，先用真实日志量化误报率，零行为改动。
  2. **可选重试闸门**（新 AtomicBool 默认关 + **仅 Claude 系** + 复用 `should_recover_truncation` 同款 `UpstreamError{INVALID_TOOL_INPUT}` + 收尾补发 SSE error）。
  3. **多信号保守判据**（单独任一都不触发）：裸 `<invoke name=`/`<function_calls>` 作为 text delta 出现 **且** 同窗口伴随 phantom token 或高 CJK 密度或截断标签。
- 位置：`stream.rs:1232` process_assistant_response（DSML 剥离之后），跨 chunk 缓冲仿 `dsml_tail_buffer`。
- 风险：误报（正文合法讨论 `<invoke>` 被判失败）→ 故**默认关+先观测量化**。根因不解（重试同上下文可能复现，需退避/换号配合）。绝不连坐号。

---

### 🟡 P2-1　`unwrap_double_encoded` 从 `tool_repair_json` 开关摘出

现在双重编码解包（洞1）整段裹在 `if tool_repair_json_enabled()`（`stream.rs:1752`）。用户为排查关掉 repair 时，会**连带关掉本可独立生效的双重编码解包**——而它不改语义、纯剥一层误加编码。

- 落地：给 unwrap 独立开关或默认恒开（对合法 object/array 是 no-op，`as_str()` 返回 None 即 early return，零回归）。
- 风险：零（已核实对正常输入 no-op）。

---

### 🟡 P2-2　②开③关配置陷阱 —— 失败态与「不发坏 JSON」绑定

②（align_failure）只置失败态不 return，③（expose_error）才 `return Vec::new()` 拦坏 JSON。两个独立 AtomicBool 可被 admin 热更成「②开③关」→**置了失败态却仍把坏 JSON 发出去**，记账与实际发送矛盾。

- 落地：把③的 return 条件从 `expose_error_enabled` 改为 `completion.is_err()`——失败态即不发坏 JSON，消除矛盾组合。需确认收尾 `generate_final_events` 会据失败态补发 SSE error（否则变空响应）。
- 位置：`stream.rs:1735-1746`。

---

### 🟡 P2-3　glued（`}{` 粘连）专门处置 + merge step6 半截+完整重写

`scan_tool_json` 已算出 `glued` 信号却**从不进控制流**（只打日志）。粘连其实是最可修的一类（保留最后一个完整对象即可），却落进 Malformed→无针对性修复→③暴露 error。

- 落地：对 `glued=true` 加一层 repair——从右往左找最后一个平衡完整 `{...}`，复验通过取之。**必须复用 `scan_tool_json` 的 string-aware in_string 逻辑**（绝不用裸 `str.contains("}{")`，否则误判字符串值里的 `}{`）。挂在 struct 层之后、复验之前。
- merge step6：当 buf 是半截碎片、frame 是完整重写对象时，现逻辑无脑 append 成粘连。可加「frame 单独完整且 buf 不是 frame 合法前驱则丢 buf 取 frame」。**边界极窄**（`{"outer":{"inner":1}` 内层完整不能误丢），必须配足单测。
- 风险：中。不实施则维持现状（粘连→repair 兜底→修不好失败态），无误删只是修复率略低。**建议配足单测后再上，否则先不动。**

---

### 🟢 P3　对照 kiro-gateway 做入站信封防呆（若未覆盖）

竞品 jwadow/kiro-gateway 逐版本修的 Kiro 专属信封坑，可对照吸收（都是入站规范化，不碰 Kiro 主路径输出）：工具名 >64 字符拦截、JSON Schema 递归消毒（剥空 `required:[]`/`additionalProperties:false`/空 description）、孤儿 tool_result 优雅降级、合并相邻 assistant 保留 tool_calls、首条非 user 前置合成 user。需先核对 KiroStudio 是否已覆盖。

---

## 三、砍掉的候选（KILL）—— 诚实记录死路，避免下个 AI 重走

| 候选 | 为什么砍 |
|---|---|
| **把正文 `<invoke>` 重构成 tool_use** | 死路。①输入已降级（antml 前缀丢失、标签截断、court 噪音替代参数）无从还原；②`<invoke>` 是常见 ASCII，Claude Code 会话高频讨论工具语法/XML，无罕见哨兵可门控（对比 DSML 靠 U+FF5C 全角竖线才安全）；③等于在网关重实现 harness 解析器还喂残缺输入。**只能检测+重试（P1-2），不能重构。** |
| **清洗 tool_use input 里的幻觉 token** | 违反不臆测铁律。token 在合法 JSON 字符串值内=可能是用户真想传的 `court` 字面（如搜索关键词），删了破坏语义。 |
| **case-insensitive / 大写变体清洗** | 大写首词是正常英文句首高发位，泄漏 token 实测全小写。误删面爆炸。 |
| **递归/多层 compound 剥离** | 啃穿 `cardCount`/`courtcase` 正常驼峰复合词。 |
| **调 max_tokens 修空回复** | max_tokens **根本不转发给上游**（Kiro CodeWhisperer 协议只吃 conversationState），到不了上游，调了无效。别浪费真号验证。 |
| **单引号→双引号 / 裸键 / NaN·Infinity 映射** | 需近乎完整的 JSON5 解析器，任何简化启发都改语义。除非日志证明高频，不做。 |
| **字符串内未转义裸双引号修复** | 唯一「修了可能比不修更糟」的类别——无法区分「字符串提前结束」与「值内嵌引号」，错切可能产出合法但语义全错的 JSON。归入碰不到清单。 |
| **括号类型错配修复**（`{"a":[1,2}`） | 罕见，且强改易把该判 None 的畸形串错「修」成语义漂移。复验兜底下仅漏修不产错，保持不动。 |
| **请求侧插分隔符隔开工具与中文 / 限制工具数 / 重排工具顺序** | 工具与中文在 Kiro 信封里已是并列独立 JSON 字段，无「相邻文本」可插分隔；限制工具数=砍客户端能力；重排顺序无依据。全无代码/issue/实测支撑。 |
| **thinking budget clamp** | 唯一有代码路径的请求侧空回复杠杆（budget 过大→只思考不输出），但真实相关性未坐实。**先加观测抓数据**，证实前不改。 |

---

## 四、验证方案（严格区分测 Bug A 还是 Bug B）

**绝不能用「让 AI 在中文对话里调工具看抽不抽风」验证** —— 那测的是 Bug B（#70544），与网关无关。本轮两个分析 agent 自己就是被这个 bug 打挂的（prompt 里嵌了字面 court/card 触发），是活体证据。

- **单元测试**（`cargo test --bin kirostudio` + `--no-default-features` 双特性）：
  - 尾逗号：`{"a":1,}`、`[1,2,]`、`{"csv":"a,}"}`（字符串内不动）。
  - court 清洗：court 独占行剥、`Card:`/`count++`/`care.method()` 不误删、Claude 门控生效。
  - 空回复：构造只发 metering 帧的流 → 断言记 EmptyResponse、A 档行为不变、B 档发 error。
  - glued：`{...}{...}` 取最后完整对象、字符串内 `}{` 不误判。
  - unwrap 摘开关后对合法 object no-op。
- **端到端**：真 Claude Code 把 base_url 指向 KiroStudio，触发参数含 `\U` 或尾逗号的工具调用，观察**不再报** Invalid tool parameters。
- **旁挂观测**：P1-2 / thinking budget 的判据先用 `KIRO_TOOL_TRACE` 抓真实日志量化误报率，再决定是否开重试闸门。
- **发版铁律**：bump 后同步 Cargo.lock（`cargo update -p kirostudio --precise <版本>`），`cargo test --no-default-features --locked` 复刻 CI 门禁。

---

## 五、诚实边界（网关根治不了的）

1. **ZOD_VALIDATION 路径**（JSON 合法但字段不符 schema）——模型少给/给错参数，网关拿到合法 JSON 无从判断某工具 schema 期望，改字段=伪造参数。纯模型侧。
2. **工具调用信封坏 Bug B**（`<invoke>` 丢 antml 前缀当文本）——无 tool_use 帧可介入，网关只能检测+重试（P1-2），不能重构。根因是 #70544 模型解码侧缺陷（telemetry：opus-4.8 0.40% vs sonnet-5/fable-5 0.00%），且有 in-context 自我模仿级联（坏块留上下文里被后续照抄，单会话可连坏几千次）。
3. **tool_use input 内的幻觉 token**——在合法 JSON 值里，清洗=臆测语义，不碰。
4. **上游真 server error / 真截断丢失的 unicode 精确值**——网关只能干净透传/字面化兜底，无法造出内容。
5. **官方 issue 全线 Open/not-planned 无 changelog 修复**（#70544/#69522/#67765/#66247/#75629）。客户端连 parse-failure hook 都没有、malformed 只重试 1 次就放弃——**网关就是唯一能补位的那层**：这正是 KiroStudio 修复层的价值。

### 证据级低成本缓解（运营/UX，非协议修复）
- 面板/文档提示：重工具循环、长会话、多字节密集场景下 **opus-4.8 坏帧率显著高于 sonnet-5**，建议用户自行选 sonnet-5 规避。**仅作证据提示，勿硬编码强制切换**（样本是社区 telemetry 非官方，且违背用户模型选择权）。
- court 一出现立刻 `/clear` 开新会话（阻断自我模仿级联，官方/社区一致推荐）——这是**端用户侧**动作，网关帮不上，但可写进文档。
