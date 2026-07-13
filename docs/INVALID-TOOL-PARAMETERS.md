# 根治 Claude Code 的 `Invalid tool parameters`

> KiroStudio 的招牌修复之一。竞品（kiro2api / kiro-gateway）都没做到这一层。

Claude Code 在调用工具时经常报 **`Invalid tool parameters`** 或把工具参数渲染成
`{"__unparsedToolInput": …}`，导致工具调用失败。KiroStudio 在网关层**根治**了其中
一大类，并对另一类做了明确的降级与可观测。本文说清楚**两层不同的病根**——它们长得像、
一直被混淆，但一个网关能修、一个网关架构上碰不到。

---

## 先分清两层（关键）

| | **Bug A：参数内容坏（网关能修 ✅）** | **Bug B：调用信封坏（网关碰不到 ❌）** |
| --- | --- | --- |
| 坏在哪 | `tool_use` 事件里的 `partial_json` **内容**非法 | 工具调用的**信封**（`antml:` 前缀 / 标签）本身被模型吐坏 |
| 典型成因 | `\U` 等 JSON 非法转义、裸控制符、上游截断、帧丢失 | 高多字节（中文/日文）密度紧邻工具标签，模型丢 `antml:` 前缀 |
| 现象 | 下游报 `Invalid tool parameters` | 整段工具调用被当**纯文本**显示、根本不执行 |
| 有没有 tool_use 事件 | **有**——网关能拿到内容去修 | **没有**——模型把 `<invoke>` 当文本吐了，无从修起 |
| 官方 issue | #20015 / #29715 / #69522 等 | **#70544**（not-planned） |
| 谁来修 | **KiroStudio 已根治** | 模型 / 客户端侧，网关架构上修不了 |

一句话：**网关只能修「已经产生了 tool_use 块、但块里 JSON 内容坏了」的情况（Bug A）。**
如果模型连 tool_use 块都没吐、把整个工具调用当文本输出（Bug B），网关没有任何工具事件可
介入——那是模型/客户端的缺陷。

> ⚠️ 常见误判：在**满是中文的对话**里看到「AI 的工具调用变成文本、没执行」，那是 Bug B
> (#70544)，**不是 KiroStudio 修复失败**。它发生在模型/客户端侧，KiroStudio 在不在链路上
> 都一样。验证 KiroStudio 的修复要看 Bug A（见下方"如何验证"）。

---

## Bug A：为什么客户端一坏就报错，而不自我修复

对着 Claude Code 客户端源码（2.1.207）逐行坐实：客户端拿到累积的 `partial_json` 后
**直接 `JSON.parse`，不做任何修复**（仅剥 BOM）。parse 失败即包成
`{__unparsedToolInput:{raw,len}}`，渲染成 `Invalid tool parameters`。客户端源码里明列三类成因：

1. **未转义反斜杠**——如 Windows 路径 `C:\Users`（`\U` 是 JSON 非法转义）。
2. **未转义控制符**——真实换行/制表符混进字符串值。
3. **截断输出**——流被中途切断，缺尾部 `"` / `}` / `]`，或 `\uXXXX` 只到一半。

对应的官方 issue（#20015 / #29715 / #69522 …）全部 **Open / not-planned**——官方不修。
这些请求经过 KiroStudio 时，我们在**发给客户端之前**把坏 JSON 修好，客户端就能 parse 成功。

---

## KiroStudio 的修复层（`src/anthropic/stream.rs`）

四道处理，从根治到可观测，逐层兜底：

### ④ JSON 修复层 `repair_tool_json`（默认**开**，纯增益）

两层修复，**只在 `serde_json::from_str` 已失败时才介入**：

1. **字符级** `repair_json_char_level`：状态机扫描，只修**字符串字面量内部**的非法转义
   （`\U`/`\x` 等降级成字面 `\\`）和裸控制符（转义成 `\n`/`\t`/`\uXXXX`），结构字符原样不动。
2. **结构级** `repair_json_structure`：补全截断——未闭合的 `"` / `{` / `[` 按栈逆序补齐。

**铁律**（安全契约，保证"最坏情况 == 不开修复"）：

- 只在 JSON 已非法时调用——合法 JSON 永不进入，对正常流零影响。
- 修复后**强制复验** `from_str`：通过才用，修不好返回 `None` 退回原样透传。
- 只修字符级噪声/结构截断，**绝不臆测语义**、不碰合法转义（`\t`/`\n` 即使可能不符模型
  本意也不动，碰了会破坏正常场景）。

### ② 拼装非法对齐失败态（默认**开**）

流式工具参数拼成非法 JSON 且修复层也修不好时，把本次置为失败态（与非流式一致，不再静默
记成功）。**绝不 `report_failure` 连坐号**（工具非法 ≠ 号坏）。

### ③ 工具错误如实暴露客户端（默认**开**，与 ② 配对）

修复层修不好时**不发坏 JSON**，改发明确的 SSE `error` 让客户端退避重试，而不是让它拿坏
参数报 `Invalid tool parameters`。

> ②③ 必须配对：② 只负责"标失败态"（记账正确），③ 才负责"不把坏 JSON 发出去"。
> 单开 ② 留 ③ 关，坏 JSON 照样发给客户端。

### ⑤ 截断跨轮恢复（默认**关**——改变对话流程）

仅当修复层④也补不回（真截断，缺整段值）且归因为截断时：不发半截参数（半截会被客户端当
完整调用执行，更危险），改置失败态让客户端**重试整轮**。默认关，因为它把"发半截"变成
"整轮失败重试"，改变对话流程，需按需开启。

### 附：截断诊断归因（纯可观测）

`classify_tool_json_defect` 对每个修不好的串按责任方归因——`truncated`（帧丢失/上游截断）
/ `illegal_chars`（模型侧非法转义或裸控制符）/ `truncated_and_illegal` / `malformed`——
只写日志（`warn` + `KIRO_TOOL_TRACE`），**绝不进控制流**，服务于"修不好的残留到底是谁的
责任"定位真因。

---

## 架构天然优势

KiroStudio「缓冲到 stop 一次性发单个 delta」（0.6.7 起）的设计，正是官方 issue #69085
报告者建议的修法（don't forward truncated/partial），**天然规避了客户端 accumulator
shear**。修复层再补"上游发来就坏"的字符级噪声，形成完整防线。

---

## 如何验证（测的是 Bug A，不是 Bug B）

**不能**用"让某个 AI 助手在中文对话里调工具、看它抽不抽风"来验证——那测的是 Bug B
(#70544)，与 KiroStudio 无关。正确的验证：

1. **单元测试**（`cargo test --bin kirostudio`，已随发布跑绿）：
   - `test_repair_windows_path_backslash`——`C:\Users\…` 反斜杠路径修成合法 JSON。
   - `test_repair_bare_control_chars`——裸换行/制表符转义成 `\n`/`\t`。
   - `test_repair_truncated_structure`——截断补全 `"` / `}`。
   - `test_repair_truncated_unicode_escape`——半截 `\uXXXX` 降级修复。
   - `test_repair_noop_on_valid_json`——合法 JSON 往返语义不变（幂等安全网）。
   - `test_classify_defect_*` / `test_should_recover_truncation_decision`——归因与恢复判据。
2. **端到端**：真实 Claude Code 客户端把 `base_url` 指向 KiroStudio，触发一次
   参数含 `\U` 的工具调用，观察它**不再报** `Invalid tool parameters`。

---

## 开关一览（设置页 → 基础 → 工具调用容错，均热更即时生效）

| 开关 | 默认 | 作用 |
| --- | --- | --- |
| JSON 修复层 | **开** | 根治：坏 JSON 修成合法再发（Bug A 主力） |
| 拼装非法对齐失败态 | **开** | 修不好时标失败态，不静默记成功 |
| 工具错误如实暴露客户端 | **开** | 修不好时不发坏 JSON，改发 SSE error 让客户端重试 |
| 清洗泄漏控制 token | **开** | 剥离模型泄漏进文本行首的控制 token（course/課 粘连） |
| 截断跨轮恢复 | **关** | 真截断且修不回时置失败态让客户端重试整轮（改对话流程） |
| 工具描述字符上限 | 10000 | 入站工具 description 超长按字符边界安全截断 |
