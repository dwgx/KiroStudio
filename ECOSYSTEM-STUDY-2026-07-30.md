# KiroStudio 生态对标研究 + 实施计划 — 2026-07-30

> 对 11 个同类项目的源码精读结论，以及由此产出的改动计划。
> **每条结论都带双侧行号**（竞品的 + 我们的），未亲自读到行的一律标注。
> 配套：`HANDOFF-NEXT.md`（批 A–E 的既有计划，本文不重复其内容，只在交叉处引用）。

---

## 0. 本轮做了什么 / 没做什么

**做了**：把生态 11 个仓库拉到 `/tmp/kirosrc`，逐层对比核心算法。
补齐了此前交接文档点名的最大缺口 **`hank9999/kiro.rs`**（1836★，2026-07-27 仍活跃，15306 行）
—— 它是 KiroStudio 的**上游祖先**，同语言同架构，是唯一能直接复用代码的对标对象。

**没做**：8 路并行侦察 fleet 跑到一半被上游 502 打死（5 个 agent 返回
`API Error: 502 上游 API 调用失败（未识别错误）`，与交接文档里记录的两次 fleet 失败同因）。
**本文所有结论均由主线亲自读源码得出**，未采用任何 agent 的未复核输出。
因此覆盖面小于原计划，但每条都可追溯到具体行号。

**克隆快照**（`/tmp/kirosrc`，最后提交日期）：

| 仓库 | 语言 | ★ | 最后提交 | 本轮用途 |
|---|---|---|---|---|
| `hank9999_kiro.rs` | Rust | 1836 | 2026-07-27 | **上游祖先**，本轮新增 |
| `Quorinex_Kiro-Go` | Go | 1032 | 2026-07-27 | 端点表 / 事件分派 / 错误分类 |
| `TsinHzl_kiro2cc-proxy` | Rust | 73 | 2026-07-29 | 池调度 / token 类型 |
| `d-kuro_kirocc` | Go | 38 | 2026-07-01 | Gate Writer 重试 |
| `caidaoli_kiro2api` | Go | 630 | 2026-06-16 | 刷新 Host 头 |
| `jwadow_kiro-gateway` | Python | 2165 | 2026-05-18（停更） | 可配 api_host |
| 其余 4 个 | — | — | — | 对照用 |

---

## 1. 结论摘要

生态对比的净结果是：**我们在 8 个层面里有 6 个领先或持平，真正该改的是 3 件事。**

### 值得做（按性价比排序）

| # | 改什么 | 为什么 | 严重度 | 工作量 |
|---|---|---|---|---|
| **E1** | `reasoningContentEvent` 接进结构化 thinking 流 | 上游**有**结构化思考流，我们把它当"未识别事件"丢弃，改用文本嗅探 `<thinking>` 标签重新解析——**在做多余且易错的工作** | 高 | M |
| **E2** | MCP 路径补齐 403 风控分类 | MCP 路径 403 一律 `report_failure`，缺 `TEMPORARILY_SUSPENDED` 判定 → 临时态被贴永久标签 → 累计 3 次禁用号。**正是历史 88 次误禁事故的同一形态**，只是这条路径没跟上修复 | 🔴致命 | S |
| **E3** | 全池自愈补落盘 | 自愈只改内存，磁盘仍 `disabled=true`。批 2 的 `persist_disabled_state` 上线后，这个洞从"重启即恢复"恶化成"重启回死态" | 🟠高 | S |

### 不要做（已确认我们已有或更好）

| 生态点名的"最佳实践" | 我们的状态 | 证据 |
|---|---|---|
| 双 CRC32 校验 + 半包处理 | **已有且失败即 bail**（非静默 continue） | `parser/frame.rs:110-131`，`MAX_MESSAGE_SIZE=16MB` @:30 |
| 绝不做内容级去重 | **全仓零去重逻辑** | `stream.rs` / `parser/` grep `dedup`/去重 = 0 命中 |
| 重试须有 `emitted` 守卫 | **结构性免疫**，不需要标志 | `provider.rs:583` 返回未消费的 `(Response, CallMeta)`，重试边界在读 body **之前** |
| 模型映射不得静默降级 | **未知模型直接拒绝** | `model_catalog.rs:584` 返 `Option`；测试 `test_unknown_model_rejected` @:825 |
| opus 订阅等级预过滤 | **已有，且比竞品多一个调用点** | `credentials.rs:610-620` 与竞品 `:323-333` 逐行等价；我们在 `token_manager.rs:2484` **和** `:2546` 两处过滤 |
| 刷新加锁 + 二次检查防惊群 | **已有，且阈值与热路径对齐** | `token_manager.rs:3116` 传 `Some(10)`，与 :3103 同阈值 |
| 刷新请求显式设 `Host` 头 | **已有** | `token_manager.rs:499`（与 caidaoli `refresh.go:93` 同款防御） |
| `tool_result` 配对校验 | **双向清理，比竞品多一向** | `converter.rs:767-774`（竞品 `translator.go:279-288` 只清单向） |
| `x-amzn-codewhisperer-optout` | **两个端点都发** | `ide.rs:96`、`cli.rs:91` |
| 主动限流（令牌桶） | **生态唯一实现者是我们** | 我们 `throttle.rs`（桶+AIMD）；kiro2cc-proxy 的 `Semaphore`（`provider.rs:53-55`）只是并发上限 |

**这张表比上面那张更重要**：它是"不要为了学而改"的依据。
生态调研最大的风险是把"竞品有个我们没有的名词"误判成缺口。

### 明确不做

| 不做 | 理由 |
|---|---|
| **四端点 429 fallback** | 我们端点**按凭据类型绑定**（`credentials.rs:711-725`）：`ksk_` 号必须走 CLI（IDE 端点实测 403），social/idc 必须走 IDE。两者**不可互换**，"429 换端点"对我们是 bug 不是特性 |
| 任何指纹伪装强化 | 超出"实现正确性"范畴 |
| 内容级去重 | 见 Kiro-Go `kiro.go:612-618` 的血泪注释（把 `"6666666666"` 变成 `"666"`）。我们本来就没有，保持 |

---

## 2. E1 — `reasoningContentEvent` 结构化 thinking 流

### 现状（双侧确证）

**上游确实发这个事件**，而且是纯增量 delta。Kiro-Go 的注释是实测结论
（`Quorinex_Kiro-Go/proxy/kiro.go:608-612`）：

> Kiro sends `assistantResponseEvent` and `reasoningContentEvent` as pure incremental deltas
> (verified against real upstream traffic), never as cumulative snapshots

**他们怎么处理**（`kiro.go:630-636`）：当作一等公民的第二条文本流，
`callback.OnText(text, /*isReasoning=*/true)`，并置 `sawOutput = true`。
文本抽取只作**兜底**（`handler.go:2011`：`if thinking && reasoningOutput == "" && extractedReasoning != ""`）。

**我们怎么处理**：`EventType` 枚举只认 4 种事件
（`src/kiro/model/events/base.rs:53-56`：`assistantResponseEvent` / `toolUseEvent` /
`meteringEvent` / `contextUsageEvent`）。`reasoningContentEvent` 落进
`EventType::Unknown` 分支（`base.rs:153`）→ **payload 被丢弃**，只按类型 warn 一次。

而我们的 thinking 是**从正文文本里嗅探 `<thinking>` 标签重新解析出来的**
（`src/anthropic/stream.rs:1425-1476`，配套 `invoke_sniff_buffer`）。
这条路径的脆弱性有代码自证：
- `:211-227` 要处理"模型在思考里提到 `</thinking>`"（用反引号包裹的情形）
- `:271-274` 要求"`</thinking>` 之后全是空白"才认作结束标签
- `:1469` 注释"避免 4.6 模型中 `<thinking>` 标签跨事件分割"
- 已知致命缺陷 **#14**（`invoke_sniff_buffer` 无界持有导致整条流停摆）就出在这套嗅探上

**即：上游给了结构化边界，我们扔掉后用启发式规则把边界猜回来。**

### 为什么值得做

这不是"多一个功能"，是**移除一整类缺陷的来源**。
`base.rs:10-13` 的注释自己记录了这个事件的流量特征：
「一次带思考的响应就有几十帧，逐帧 warn 实测 30 分钟刷出 22939 条、占全部日志 91.5%」
—— 说明这个事件在生产上**高频稳定出现**，不是边缘情况。

### 改法

1. `src/kiro/model/events/base.rs`：`EventType` 加 `ReasoningContent` 变体
   （`:53-56` 的 `from_str` 与 `:64-67` 的 `as_str` 成对加）。
2. 新增 `src/kiro/model/events/reasoning.rs`：`ReasoningContentEvent { text: String }`
   （字段名依 Kiro-Go `kiro.go:631` 的 `event["text"]`，**落地前需实际抓一帧确认**）。
3. `src/anthropic/stream.rs`：新增分派分支，直接产出 `thinking_delta`，
   复用现有 `create_signature_delta_event`（:1878）收尾。
4. **文本嗅探保留作兜底**，不删——上游可能对某些模型仍走内联标签。
   两条路径都置同一个"已进入 thinking 块"状态，避免重复开块。

### 回归测试

| 测试 | 断言 | 旧代码为何 FAIL |
|---|---|---|
| `should_emit_thinking_delta_from_reasoning_content_event` | 喂一帧 `reasoningContentEvent` → 产出 `content_block_delta` 且 `delta.type == "thinking_delta"` | 旧代码走 `EventType::Unknown` → payload 丢弃 → 零事件产出 |
| `should_not_double_open_thinking_block_when_both_paths_fire` | 先喂 `reasoningContentEvent` 再喂含 `<thinking>` 的正文 → 只有一个 `content_block_start(thinking)` | 新增守卫，旧代码无此路径 |

### 前置未知（落地前必须确认）

- `reasoningContentEvent` 的 payload 字段名。Kiro-Go 读 `event["text"]`，
  而 `assistantResponseEvent` 读 `event["content"]` —— **两个事件字段名不同**，别想当然。
  确认手段：临时把 `base.rs:175` 的 unknown payload 日志调到 info，抓一次真实带思考的响应。

---

## 3. E2 — MCP 路径的 403 风控分类缺口 🔴

### 现状（双侧行号）

**对话路径**（`src/kiro/provider.rs:805-809`）：
403 → 判 `TEMPORARILY_SUSPENDED` → `report_suspicious_activity`（分钟级退避，**临时态**）。

**MCP 路径**（`src/kiro/provider.rs:517-537`）：
403 → 先给一次 force-refresh 机会（:520-530）→ 然后**无条件** `report_failure(ctx.id)`（:532）。
**完全没有 suspend 判定，也没有临时风控分类。**

### 为什么是致命

`report_failure` 累加 `failure_count`，达 `MAX_FAILURES_PER_CREDENTIAL` 即以
`TooManyFailures` 禁用（`token_manager.rs:3630-3634`）。所以：

一个只是**被临时限流**的号，走 WebSearch/MCP 请求被打 3 次 403 → 被永久型标签禁用。

而这正是本项目历史事故的形态——交接文档记录：
「403 `TEMPORARILY_SUSPENDED` 曾被当永久封禁处理 → 12 小时 88 次误禁 +
51 次『所有凭据已用尽』+ 36 次全池自愈活锁，逐小时拒绝率升到 100%」。
对话路径修了，**MCP 路径没跟**。

**并且批 2 让它恶化了**：`persist_disabled_state` 上线后，
这个误禁**重启也回不来**（此前重启会以 enabled 回池）。

### 改法

把对话路径的判定顺序搬到 MCP 分支（`provider.rs:517` 那个 `if matches!(status.as_u16(), 401 | 403)` 内）：
force-refresh 尝试后，先判 `TEMPORARILY_SUSPENDED` → `report_suspicious_activity`，
**只有非风控 403 才落 `report_failure`**。

> **更好的收口**（MASTERPLAN G4 已提过，本轮证实其必要性）：
> 抽 `classify_upstream_failure(status, &body, endpoint, cred) -> FailureVerdict`，
> 两条路径共用。否则下次新增信号还会分叉第三次。
> 建议**先用最小改动修 E2**（S 工作量、可立即上线），收口作为独立 PR。

### 回归测试

| 测试 | 断言 | 旧代码为何 FAIL |
|---|---|---|
| `mcp_403_temporarily_suspended_does_not_count_as_failure` | mock MCP 返 403 + body 含 `TEMPORARILY_SUSPENDED`，连打 3 次 → 号 `disabled == false` | 旧代码 3 次 `report_failure` → `TooManyFailures` 禁用 → `true` |
| `mcp_403_non_suspend_still_counts_as_failure` | 403 + 无关 body → 仍计失败（对照组，防止修过头把真失败也放过） | 新增，守住边界 |

---

## 4. E3 — 全池自愈不落盘 🟠

### 现状

`src/kiro/token_manager.rs:2884-2903`：全部号因 `TooManyFailures` 禁用时，
一次性 `disabled = false` / `disabled_reason = None` / `failure_count = 0`。

**只改内存**：没有 `persist_disabled_state`，没有 `persist_credentials`。
且漏清 `consecutive_suspicious` / `cooldown` / `rate_limiter` / `refresh_failure_count`。

### 为什么现在才要紧

批 2 之前，自愈不落盘无所谓——重启会把所有号以 enabled 读回来。
批 2 的 `persist_disabled_state`（9 处命中）让自动禁用**持久化**了，于是：

自愈后内存 enabled、磁盘 `disabled=true` → 面板与磁盘长期背离 → **重启回死态**。

### 改法

自愈块内补 `persist_disabled_state` 批量落盘 + 清那四个瞬态计数。

**顺带修一个同类漏项**（本轮独立发现，MASTERPLAN 批 2 列了但未做）：
`reset_and_enable`（`token_manager.rs:4520`）清了 `failure_count` /
`refresh_failure_count` / `request_count`，**没清 `consecutive_suspicious`**
（该字段唯一清零点是 `report_success` @:3471）。
所以被 `SuspiciousActivityAuto` 禁用的号（计数已达阈值 6）人工「重置并启用」后，
**下一次 403 立即再禁**，且同样重启不恢复。

→ 两处一起修，落地 MASTERPLAN 的 `clear_transient_counters()` 收口
（当前全仓 0 命中，三处复活路径各自手写清零列表）。

### 回归测试

| 测试 | 断言 | 旧代码为何 FAIL |
|---|---|---|
| `should_persist_pool_wide_selfheal` | 全池打到 `TooManyFailures` → 触发自愈 → **重新加载文件** → `disabled == false` | 旧代码自愈只改内存，文件读回 `true` |
| `should_clear_suspicious_counter_on_reset_and_enable` | 连打 6 次 `report_suspicious_activity` 致自动禁用 → `reset_and_enable` → 再打 1 次 → `disabled == false` | 旧代码计数仍为 6，第 7 次立即再禁 |

---

## 5. 与既有计划（`HANDOFF-NEXT.md`）的关系

本文**不取代**那份，两者互补：

| 来源 | 内容 | 状态 |
|---|---|---|
| `HANDOFF-NEXT.md` 批 A–E | 后端读路径 / 多选删除 / OTA / 用量埋点 / 缓存 | 仍有效，未开工（已核实：`spawn_blocking` / `live_snapshot` / `p_avail_batch` / `batch_delete` / `read_body_capped` / `busy_timeout` 全仓 **0 命中**） |
| 本文 E1–E3 | 生态对标得出的三条 | 新增 |

**建议顺序**：**E2 → E3 → 批 A → E1 → 批 B/C/D/E**。

理由：E2 是致命且 S 工作量，E3 与它同文件同主题（都是禁用/自愈语义）可一并提交并共享测试脚手架；
E1 是 M 工作量且有一个前置未知（payload 字段名）需先抓帧确认；
批 A 与已上线的前端韧性改动配对，用户能直接感知。

### 本轮顺带核实的既有计划项

| 项 | 结论 |
|---|---|
| **D8**（强制删 `inflight>0` 的号是否安全）—— MASTERPLAN 列为「落地前必须确认的三项」之一 | ✅ **安全**。`InflightGuard` 直接持 `Arc<AtomicU32>`，Drop 只对自己那个 Arc 做 `saturating_sub`（`scheduling.rs:46-53`），与 entry 生命周期解耦（模块注释 REF-1 明写）；`report_failure` entry 缺失时 early-return（`token_manager.rs:3610-3612`），不 panic 不误伤。→ 按 D8 推荐落地：不拒绝，确认框标红 + 后端 `warn!` 记 inflight |
| 批 D 的 `first_token_ms` 疑似从不写入 | ✅ **确证**。全仓生产赋值点为 **0**（`record.rs:143` 是 `None` 初始化，`trace_db.rs:402` 是读回，:483/:609 在测试里）。TTFB 数据不存在 → 所有延迟分析失效 |
| 批 E 的 EXP-1 自变量缺失 | ✅ **确证**。`docs/cache-probe-data/observations.jsonl` 8 档采样中 `cache_read_input_tokens` / `credits_used` / `first_token_ms` **全 null**，唯一有效信号是 `input_tokens`（随 filler 单调增长）。→ 批 E 确实被批 D 的埋点阻塞 |
| `credits_used` 写入链 | 仅 **1 处**生产写入（`src/anthropic/handlers.rs:1506`）。另外三条路径（非流式 / cc buffered / openai 兼容 / MCP）是否有等价累加**未核实**，是批 D 的待办 |

---

## 6. 值得记住的生态经验（不构成改动）

这几条不产生代码改动，但对将来判断有用：

1. **Kiro-Go `kiro.go:612-618` 的去重血泪注释**值得完整读一遍。
   要点：AWS event-stream 基础规范**不带 sequence number 或 message id**，
   所以字符串级"重放检测"只能猜，猜错就静默吃掉真实输出
   （实测把 `"6666666666"` 变 `"666"`、`"abababab"` 变 `"abab"`、`"1833"` 变 `"183"`）。
   → 将来任何人提"要不要防重复帧"，指向这条注释。

2. **`star` 数与可用性无关**。生态前四名 star 里两个已死一个闭源：
   `jwadow/kiro-gateway`（2165★）停更 2.4 个月，`FakeOAI/tokens`（411★）主体闭源，
   `aliom-v/KiroGate`（423★）已 archived。判活跃度要看最后提交。

3. **2026-05-15 端点迁移是硬淘汰线**。`caidaoli/kiro2api` 把旧端点写成常量
   （`config/config.go:25`）且无 fallback → 迁移后基本废。
   我们已在 `runtime.{region}.kiro.dev` + CLI 端点，这条线上是安全的。

4. **上游 `hank9999/kiro.rs` 仍活跃**（2026-07-27，最新提交是 Opus 5 支持）。
   我们 26595 行（8 个大文件）vs 它 15306 行（全仓），分叉已很深。
   但 `src/kiro/parser/` 那 5 个文件双方相对接近，**是将来对照上游 bugfix 的最佳切入点**
   ——本轮时间不够做这个逐函数对照，是最值得补的一路。

---

## 7. 未完成 / 需要重跑

| 项 | 为什么没做完 | 建议 |
|---|---|---|
| 8 路 fleet 侦察 | 上游 502 打死 5 个 agent（与交接文档记录的两次 fleet 失败同因） | 号池健康时重跑；或继续按本轮方式主线串行读 |
| 上游 `kiro.rs` 的 `fix(...)` 提交逐条比对 | 时间不够 | 最高价值的未做项。方法：`git -C /tmp/kirosrc/hank9999_kiro.rs log --oneline --grep='^fix'` → 逐个 `git show --stat` → 到我们代码找对应位置 |
| `reasoningContentEvent` payload 字段名 | 需抓真实帧 | E1 的前置，见 §2 |
| 其余三条路径的 `credits_used` 累加 | 属批 D 范围 | 与批 D 一起做 |

---

*本文所有行号均为 2026-07-30 工作树实读值。工作树有 56 个其它会话的未提交改动，行号可能随之漂移——动手前用函数名重新定位。*
