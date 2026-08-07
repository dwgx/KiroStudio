<!-- HISTORICAL-ARCHIVE-MARK -->
> ⚠️ **这是过程记录，不是当前状态。** 当前状态看仓根 `STATUS.md`（唯一真相源）。
>
> 本文件已确证含**过期断言**。历史上多次出现「后来的会话把过期断言当约束」而做出错误决策
> ——最严重的一次：一句无依据的「`q.*` 已停用」注释直接导致了一次错误的架构迁移，
> 改坏 region 探测让 US 号恒 403，上线后才发现并回滚。
>
> 读本文件时：**任何数字（测试数 / 配置值 / 池容量 / 行号）一律现读现验**，
> 结论性断言先按 `STATUS.md` 核一遍。本文件的价值在于**依据与推导过程**，不在于它的结论。

---

# 实施计划 — 核心缺陷修复（供实现 agent 执行）

> 来源：2026-08-06 只读审计（5 个 agent + 主线复核）。**本文件只描述"做什么/为什么/怎么验"，不含实现代码。**
> 每条的证据都已独立复核；标注「未复现」的条目已明确区分。
> 行号会漂（工作树有其它会话未提交改动），**定位以符号名为准，行号仅作参考**。

---

## 0. 硬约束（违反会造成真实损失，先读完再动手）

1. **禁止 `cargo fmt` / `rustfmt` 任何已存在文件。** 仓库 HEAD 不是 rustfmt-clean。实测：对 `src/openai/handlers.rs` 跑 `rustfmt --edition 2024` 产生 861 插入/212 删除的重排，把 30 行真实改动埋掉，且可能冲掉其它会话的未提交改动。新增代码手工匹配周边风格。
2. **禁止 `git checkout` / `switch` / `stash` / `reset` / `commit` / `add`。** 工作树长期有多个会话的未提交改动（审计时 119 个条目）。
3. **`src/anthropic/converter.rs` 审计时有 +594/−57 未提交改动** —— 任务 A1 就在该文件。动手前先确认该文件当前状态，必要时与用户确认是否等其它会话落地。
4. 构建前置：`admin-ui/dist` 必须存在（rust-embed 编译期嵌入，缺则报 E0599）。
5. 一律 `cargo test --no-default-features` / `cargo clippy --no-default-features`（`Cargo.toml` 的 `default=["native-tls"]` 与出厂配置相反）。
6. **已知的 pre-existing 测试失败**（不是你造成的，不要"顺手修"）：
   `admin::service::multi_open_copies_tests::api_region_setter_endpoint_is_wired`。
   原因：它用 `include_str!` 对 `admin/router.rs` 做**单行**字符串匹配，而该 `.route(...)` 调用已被换行（`admin/router.rs` 的 `/credentials/{id}/api-region`）。路由注册正常、功能没坏。基线是 **1234 passed / 2 failed**。

---

## 任务 A1 🔴 `$ref` 展开指数爆炸（DoS 级，单独一批）

### 位置
`src/anthropic/converter.rs`：`resolve_schema_refs`（约 :57-110），入口 `normalize_json_schema`（约 :31-37，唯一调用点 `resolve_schema_refs(schema, &defs, 0)`）。

### 根因（已逐行核实）
`depth` 只在 `$ref` 跳转时 `+1`（约 :78 `resolve_schema_refs(target.clone(), defs, depth + 1)`），
而**同级子节点递归复用同一 `depth`**（约 :99 `new_obj.insert(k, resolve_schema_refs(v, defs, depth))`，数组arm 约 :105 同）。
于是 `MAX_REF_DEPTH = 16`（约 :62）约束的是 **$ref 链长**，不是**节点数**。分叉因子 b 的自引用 schema 展开出 ≈b^16 个节点，且 :63 的兜底在分叉已乘开之后才生效。

### 实测（我复刻同一递归结构验证，非推测）
| 分叉 b | 节点数 | 结论 |
|---|---|---|
| 1 | 508 | 正常 |
| 2 | >5,000,000（我的脚本上限）| 爆炸 |
| 3 | >5,000,000 | 爆炸 |

理论值 b=2 → 2^16=65,536；b=3 → 3^16=43,046,721。实测更差，因为每次展开还会把 target 子树 merge 一份再让同级递归。

### 触发条件
一个自引用且有 **≥2 个兄弟属性指向自身**的 JSON Schema。**这是 pydantic / zod / MCP 生成递归结构的常规产物**（树节点 `left`/`right`、文件树 `children`/`siblings`、`JSONValue`），不需要恶意构造。输入约 **219 字节**。

### 后果
- `convert_request` 在 async handler 里**同步**调用（`anthropic/handlers.rs` 约 :1260 与 :2308），**全仓无 `spawn_blocking`**（唯一命中是 `admin/usage_handlers.rs:117` 的一句注释）→ 直接占死一个 tokio worker，并发几个即打瘫 runtime。
- `max_body_bytes` 默认 256MiB 拦不住 219 字节输入。
- b=2 那条更隐蔽：它会"成功"，然后把约 17MB schema 发上游 → 必然撞 Kiro 的 ~5MiB 限制回 400，而 `compressor.rs` 不碰 `input_schema`，压不下去。
- **两个协议入口全中**：`/v1/messages` 直连，以及 `/v1/chat/completions`、`/v1/responses`（`openai/convert.rs` 把 `function.parameters` 原样搬过来；OpenAI 生态用 pydantic/zod 出递归 schema 更普遍）。

### 要求的修法：全局节点预算（**不要**改成同级也 `depth+1`）
- 引入一个贯穿整次展开的节点计数上界（建议常量名 `MAX_SCHEMA_NODES`，量级 10_000；需在注释里写清这个数怎么来的）。
- 计数需在**整棵树**共享（不是每层重置）。达到上界即停止展开，把当前子树降级为 `{"type":"object","additionalProperties":true}`（与 :63 现有兜底同形），并记一条 `tracing::warn!`（要能在线上分辨"真的有人发了递归 schema"）。
- **为什么不用"同级也 depth+1"**：那会改变正常深嵌套 schema 的展开行为（一个 5 层嵌套的普通 schema 会被误判超深而降级），属于把正确输入也弄坏。节点预算只惩罚真正的规模爆炸。

### 验收
1. 新增测试：b=2 与 b=3 的自引用 schema，断言**返回**（不挂）且节点数/输出字节有上界。b=1 的现有测试 `test_normalize_schema_ref_cycle_safe`（约 :2798，其 `Node.properties` 只有一个 `child` —— **恰好是唯一安全的分叉因子**，这就是它漏掉此 bug 的原因）必须保持绿。
2. 新增测试断言超预算时降级为宽松 object 而非 panic/空壳。
3. 现有 `test_normalize_schema_unresolvable_ref_degrades`（约 :2817）保持绿。

---

## 任务 A2 🔴 `merge_tool_input` 第 5 步吞掉单独成帧的 `{`

### 位置
`src/anthropic/stream.rs`：`merge_tool_input`（约 :4730），第 5 步约 :4748-4750。

### 根因（已实测复现）
第 5 步 `buf.len() > frame.len() && buf.starts_with(frame)` → 丢弃本帧。
工具 input 顶层必是 object ⇒ **buf 永远以 `{` 开头** ⇒ 任何单独到达的 `{` 帧必然命中此条被吞。

### 实测（复刻 7 步决策表逐场景验证）
| 帧序列 | 结果 |
|---|---|
| `{"edits":[` / **`{`** / `"old":"a"` / `}` / `]}` | `{"edits":["old":"a"}]}` ❌ 非法 |
| 嵌套但 `{` 与后续同帧 | ✅ 合法 |
| 平坦 input（无嵌套） | ✅ 合法 |

**只有 `{` 单独成帧才触发** ⇒ 依赖上游分帧位置 ⇒ 线上是间歇性的。

### 后果链（全部已核实）
`scan_tool_json` 对该串算出 `depth=-1` ⇒ 归因落 `Malformed`/`missing_comma`（约 :3596-3603、:3621-3648）→ **日志指向模型侧，把排查带偏**。
`repair_json_structure`（约 :3962）对它是 **no-op**（我实测输入输出逐字相同）：它只从栈里 append **闭**括号，而这里缺的是**开**括号，且只在末尾追加。
⇒ `tool_stream_align_failure`（默认 true）置失败态 ⇒ `flush_tool_input` 返回空 ⇒ 客户端拿到 `input:{}` 的 tool_use + 一条 SSE error。
注：约 :4715 的注释把这类"网关侧合并出错"称为**类型 C、已根治** —— 这是一个未被根治的类型 C。

### 要求的修法：第 5 步增加「`buf` 必须是完整 JSON」前置条件
判据依据（来自第 5 步自己的设计意图，约 :4718 决策表注释"丢弃迟到的旧短快照"）：
**"旧快照"只有在 buf 已持有完成值时才成立** —— 累积一旦完整，之后到达的更短前缀必然过期。
若 buf 仍不完整（累积进行中），更短的前缀帧是**增量碎片**，不是过期快照。

`is_complete_json` 已存在（约 :3538），第 6 步已在用，无需新增辅助函数。

### 我已验证该修法不破坏任何现有用例
| 现有决策表测试 | 旧 | 新 |
|---|---|---|
| `test_merge_cumulative_snapshots` | PASS | PASS |
| `test_merge_pure_increments` | PASS | PASS |
| `test_merge_duplicate_final_frame` | PASS | PASS |
| `test_merge_nonprefix_double_object_keeps_latest` | PASS | PASS |
| `test_merge_full_then_shorter_prefix_kept`（**第 5 步本体**）| PASS | PASS |
| `test_merge_illegal_fragments_append` | PASS | PASS |
| `test_merge_empty_edges` | PASS | PASS |
| D1 坏场景 | **FAIL** | **PASS** |

`test_merge_full_then_shorter_prefix_kept` 之所以仍绿：它的 buf 是 `{"path":"a.txt","content":"hi"}`（完整 JSON），新前置条件满足，第 5 步照常生效。

### 验收
1. 新增测试：`{"edits":[` / `{` / `"old":"a"` / `}` / `]}` → 断言合并结果为 `{"edits":[{"old":"a"}]}` 且 `serde_json::from_str` 成功。
2. 上表 7 条现有测试全绿。
3. 同步更新约 :4718 的决策表注释（第 5 步补上新前置条件与理由）。
4. **顺带修归因**：确认修复后这类不再落 `Malformed`。若归因逻辑仍可能把网关侧问题标成模型侧，在 `scan_tool_json` 的归因注释里说明边界。

### 线上取证（可选，不需要新代码）
`KIRO_TOOL_TRACE=1`（约 :4010 的常驻探针，注释写明就是为区分类型 C 与类型 A 而建）可抓 `toolUseEvent.input` 逐帧原文 + 合并轨迹，用于确认线上实际频率。

---

## 任务 A7 🟠 OpenAI 错误出口丢弃全部响应头（含 `Retry-After`）

### 位置
`src/openai/handlers.rs`：`translate_error_response`（约 :296-311）。

### 根因
该函数用 `status` + `body` **重建**响应（末尾 `openai_error(status, &typ, &msg)`），原响应的**全部头被丢弃**。

### 后果
`src/anthropic/handlers.rs` 为 `Retry-After` 下了很大功夫且有 7 个 `error_translation_tests` 钉住映射顺序：
- `UPSTREAM_RATE_LIMIT_RETRY_AFTER_SECS = 8`（约 :515）
- `UPSTREAM_SUSPENDED_RETRY_AFTER_SECS = 20`（约 :571）
- 全池冷却/真耗尽带号池算出的**真实**恢复秒数
- 且有承重顺序：准入超时必须最先判、`model_unsupported_by_pool=1` 必须排在 `retry_after_secs=` 之前

这些值全部算好，然后在 OpenAI 出口扔掉 ⇒ 走 `/v1/chat/completions` 与 `/v1/responses` 的客户端（Codex / Cline / Roo）退回自身默认退避。线上 `kiro_shield.py` 的 `MIN_DELAY=1.0` 让这个浪费更贵。
已有 7 个测试测的是 `map_provider_error`（**产生**头的那一侧），测不到出口**重建**这一层 —— 这就是缺口能长期存活的原因。

### 要求的修法
在 `into_body()` **之前**取出 `Retry-After`（`resp.headers()`），重建响应后写回。
**只透传 `Retry-After`**，不要无脑透传全部头（`Content-Type` 等必须由新响应决定，否则会声明成 Anthropic 的类型）。
上游未给该头时**不得凭空构造**（等于替上游编造恢复时刻）。

### 验收
`src/openai/handlers.rs` 当前 **0 测试**（全仓唯一真零，487 行），本任务需建立该文件的首个 `#[cfg(test)]`：
1. 造一个带 `Retry-After: 8` 的 Anthropic 错误响应（429 + `{"type":"error","error":{...}}`），断言翻译后 status=429、`Retry-After` 仍为 `8`、body 已是 OpenAI 形状（`error.type` / `error.message`）。
2. 不带该头的（如 502），断言翻译后**没有** `Retry-After`。

---

## 任务 A8 🟠 `contextUsagePercentage` 无下界守卫 → input_tokens 归零

### 位置
- `src/anthropic/stream.rs` 约 :1528-1545（`Event::ContextUsage` 分支）
- `src/anthropic/handlers.rs` 约 :1911-1926（非流式同一份逻辑，**同缺口**）
- 字段定义：`src/kiro/model/events/context_usage.rs` 约 :15-21，`context_usage_percentage` 带 `#[serde(default)]`

### 根因（已核实）
`#[serde(default)]` ⇒ 上游不带该字段时为 `0.0`。
`(0.0 * window_size / 100.0) as i32` = 0 ⇒ `self.context_input_tokens = Some(0)`。
下游 `unwrap_or(input_tokens)` 因为是 `Some(0)` 而非 `None`，**回退永不触发**。

### 后果
客户端 usage 与 `RequestRecord.input_tokens` 双双归零，`clamp_cache_to_input` 再把 cache 两项夹成 0 ⇒ 该请求在用量库里 input=0、cache=0。

### 要求的修法
`context_usage_percentage <= 0.0` 时**不要**落 `Some(...)`（保持 `None` 让下游回退到本地估算）。
两处都要改（`stream.rs` 与 `handlers.rs`）。**注意**：这两份是同一逻辑的重复实现，除窗口取值外还包含 100% 判定；本任务只加下界守卫，**不要**顺手合并这两份（合并是独立重构，风险另算）。

### 验收
1. 全仓当前**没有任何测试断言 `context_input_tokens`**（约 :5266 那条只断言它不触发 TTFB 打点）。需新增：字段缺失 / 值为 0 两种输入，断言 `context_input_tokens` 为 `None` 且最终 input_tokens 走本地估算（非 0）。
2. 正常值（如 42.0）仍正确换算。
3. `stream.rs` 与 `handlers.rs` 两条路径各一组。

---

## 任务 A3 🔴 客户端断连 → 整条流式记录消失

> 顺序建议：**先做度量再动手**。

### 位置
四个 emit 点全在 `stream::unfold` 闭包内：
- `src/anthropic/handlers.rs` 约 :1592 / :1617（真流式，`emit_stream_usage`，定义约 :1429）
- 约 :2600 / :2623（buffered，`emit_buffered_usage`，定义约 :2636）
- unfold 起点约 :1496 / :2503

### 根因（已核实）
客户端断连 ⇒ axum/hyper drop response body ⇒ drop unfold future ⇒ **两个 emit 点都到不了**。
全仓 6 个 `impl Drop` 无一保护 usage：`admin/update.rs:744`(测试)、`usage/trace_db.rs:464`(测试)、`kiro/token_manager.rs:7872`+`:8247`、`kiro/scheduling.rs:46`、`anthropic/converter.rs:1922`(测试)。

### 后果
该请求**分子分母都没有**（不是记成失败，是不存在）⇒ 面板成功率**偏乐观**。
`provider.report_credits` 只在 emit 函数体内调用（约 :1462 / :2669）⇒ **真实 credit 永久丢失**，凭据生命周期花费少算。
线上 `ccAutoBuffer=true` 使 buffered 成为主路径，它把整轮憋到流末才吐 ⇒ **暴露窗口 = 整轮时长**，比真流式大一个量级。

### 关键参照：项目已有 RAII 范式
`src/kiro/scheduling.rs:46` `InflightGuard` 是精心设计的 RAII 守卫，还在注释里说明**为何刻意不实现 `Clone`**（clone 会 +0/-1 导致 inflight 低估，反而破坏防惊群目标）。
即：**项目理解 RAII-for-correctness，并已用于 inflight 计数，却没用在计费信号上。** 这是本任务的设计依据。

### 第 1 步（先做）：量化实际损失
`src/kiro/provider.rs` 约 :756-758 的注释显示这类对账**此前已做过一次**，实测 `report_success` 2070 vs 用量库 951。
按同口径再对一次：`sum(report_success)` vs `SELECT count(*) FROM traces WHERE outcome='success'`。
**拿到比例再决定这条的优先级**（我未复现实际断连频率，它取决于 CC 取消习惯与 shield 放弃在途流的频率）。

### 第 2 步：加 RAII 守卫
- 守卫持有 emit 所需的最小状态，在 `Drop` 里补发"未正常收尾"的记录。
- **必须防 double-emit**：正常路径已 emit 后，守卫需被显式解除（`mem::forget` / 内部 flag / `Option::take`），否则每个成功请求变两条。
- 两条路径（真流式 / buffered）都要覆盖。
- 补发的记录要能与正常记录**区分**（建议 outcome 上标注断连，否则面板无法分辨"客户端取消"与"上游失败"）。

### 验收
1. 构造 unfold 被提前 drop 的测试，断言恰好落 1 条记录且标注为断连。
2. 正常完成路径断言仍恰好 1 条（不是 2 条）—— 这条是防 double-emit 的护栏。
3. 两条路径各一组。

---

## 任务 A6 🟠 `#21` 的 API 出口（纯机械，零风险）

### 先更正项目文档
`CLAUDE.md` 的 #21 写「`Aggregate` 没有任何 retry 字段、`add` 不读 `r.retries`」—— **已过期**。
聚合层其实已建好：`src/usage/usage_stats.rs` 约 :120 `retries_sum`、约 :123 `retried_requests`，并有注释说明两个分母各自用途（`retries_sum/requests` 是整池放大倍数，`retries_sum/retried_requests` 是"真重试时重试几次"）。
**缺的只是 API 出口。** 完成本任务后请同步修正 CLAUDE.md 该条。

### 缺口（已 grep 确认）
- `WindowSummary` / `SeriesPoint` / `GroupStat` 三个输出结构 retries **零命中**
- `avg_retries_per_request` / `avg_retries_when_retried` / `avg_first_token_ms` 的**唯一调用方是单元测试**
- `admin-ui/src` 里 `avg_retries` / `avgRetries` **零命中**

### 要求
1. 三个输出结构补字段 + 各自的 `From`/构造点（`GroupStat::from`、两处 `SeriesPoint` 构造）。
2. 前端**两份独立类型定义**都要同步（`admin-ui/src/types/api.ts` 与 `admin-ui/src/api/ops.ts` —— 这是 #21 已记录的坑）。
3. 面板加聚合视图（当前只有 2 处逐条详情）。
4. 前端改动后必须 `cd admin-ui && pnpm build`，否则二进制里还是旧的。

### 验收
`cargo test --no-default-features`、`cd admin-ui && npx tsc --noEmit`、`pnpm build` 全绿；端点返回值含新字段。

---

## 明确暂缓（缺前置事实，不要凭猜动手）

### B1 `cooldownEnabled` 的职责拆分 🟠
**缺的前置**：线上该值的真值。代码默认 `true`（`config.rs` 约 :869），但 `provider.rs` 约 :989 与 `token_manager.rs` 约 :3931 两处注释断言**线上是 `false`**（后者追溯到一次真实事故：一个真实 429 被放大成连环 429）。`ws-vps/docs/02-tuning.md` 里**没有这一项**。

两组缺陷**互斥生效**，改错方向等于白做：
- **线上（false）失效的**：`report_suspicious_activity` 第 2 段整块在门内（`token_manager.rs` 约 :5303-5321），含注释写着"让**整族**进熔断 Open"的 `report_family_suspicious` ⇒ docs 宣称的"一号风控整族退避"在生产上是空的。第 3 段又被 `rate_limit_enabled` 门控（线上也是 false）⇒ 线上遇账户级风控**只有计数器在工作**（第 1 段刻意在门外）。
- **仅默认部署（true）踩的**：`report_auth_cooldown`（约 :5332）→ `AuthenticationFailed` = 86400s 且 `is_auto_recoverable=false`（`cooldown.rs` 约 :77、:92）。而三个调用点注释都以为自己设的是"短冷却"（`provider.rs` 约 :1607 明写"处置与 `is_temporary_rate_limit` 同款：设短冷却"）⇒ **一次瞬态 403 冻号 24 小时**。
  ⚠️ 这个缺陷**在你的生产环境看不见（开关关着），只会打到按默认配置部署的人**。

**拆分原则**（真值确认后再动）：`report_suspicious_activity` 已把"计数"拆出门外，健康惩罚与 5xx 处置应按同一原则拆 —— 冷却是冷却，健康降权是调度信号，不该共用一个开关。

### B2 SSE keepalive 🟠
**缺的前置**：`kiro_shield.py`（239 行，在 KiroStudio 前面，**不在本仓库**）是否透传 SSE comment 帧（`:` 开头的行）。若它按行解析只转发 `data:`，ping 到不了客户端，做了白做。
参考实现有此能力（kiro.rs `handlers.rs:43` `PING_INTERVAL_SECS=15` + `:723-725` 在 `select!` 里发 comment 帧）。KiroStudio 两个协议层**全无**（已 grep 确认）。
真实风险：上游首 token 可能几十秒（Opus 高 effort），链路是 Caddy → kiro_shield.py → KiroStudio，任一段空闲超时即断。
移植代价：你用 `Body::from_stream` 而非 axum `Sse`，`KeepAlive` 不适用，须在 `async_stream::stream!` 里手写 `tokio::select!` 竞速 data 与 ticker（约 15 行）。

### B3 A5 的两路错误分类统一 🟠
`stream.rs` 约 :117-128 `is_rate_limit_signal` 对**整段 payload** 裸子串匹配（`error_message = frame.payload_as_str()`，`events/base.rs` 约 :214），判据含 `"throttl"` / `"quota"` / `"429"` / `"overload"` / `"exhaust"`。
而 `handlers.rs` 约 :660 记录上游发 `400 ThrottlingException` + `reason:INSUFFICIENT_MODEL_CAPACITY`，**实测 24h 272 次**。该 payload 含 `Throttl` ⇒ **in-band 路径判成限流，HTTP 路径有精确分类链和回归测试（约 :3061）** ⇒ 两条路径对同一上游信号结论相反。
**为什么暂缓**：统一口径要动错误分类的承重顺序（`handlers.rs` 已有 4 个字面量哨兵且顺序承重）。建议单独一批 + 先补两路一致性测试。

---

## 未复现条目（agent 报告，我未逐条验证；实现前请自行核实）

| 条目 | 位置 | 我的判断 |
|---|---|---|
| prefill 截断判据是 `!= "user"` 而非 `== "assistant"`，且丢的是最后一条 user 之后**全部**消息 | `converter.rs` 约 :861-871 | 判据写法我看过，机制可信。对 OpenAI 入口是常规路径（`openai/convert.rs` 约 :552 只护首条不护末条）→ 以 assistant 结尾的合法 continuation 被静默丢弃、上一条 user 被重答一遍。但"重复计费"的实际频率取决于有无客户端这样发，未实测 |
| 历史里的孤儿 tool_result 从不清理（只清当前消息） | `converter.rs` 约 :1301-1319 | 反向的孤儿 tool_use 有专门清理（约 :1340）。主要打直连 `/v1/messages`；OpenAI 侧 `convert.rs` 约 :512 双向清了 |
| in-band Error/Exception 之后流不终止，收尾仍发正常 `message_delta` + `message_stop` | `stream.rs` 约 :1546-1598、:3250 | 未复现 |
| `tool_choice` 整体被丢弃（Kiro 的 `UserInputMessageContext` 确无该通道） | converter 全文不读 `req.tool_choice` | 上游无字段可映射是事实，但当前是**零日志**静默降级。最小改法是加一条 warn |
| `output_tokens` 随上游分片粒度膨胀（agent 离线实测 ASCII 3.8×） | `stream.rs` 约 :3533 的 `.max(1)` | 未复现 |
| thinking 块在 tool_use 块开始前不关闭（顺序违规） | `stream.rs` 约 :2751、:998 | 顺序违规是客观的；**客户端是否真报错未确认**（取决于 CC 对 `content_block_delta` 是按 index 写入还是校验活跃块，那份代码不在本仓） |

---

## 附：已作废的审计条目（不要照做）

- **"`/v1/responses` 请求侧完全不看 `tools[].type`、把所有工具都当 function"** —— **错误，已证否**。
  `src/openai/convert.rs` 唯一的 tools 处理点确实测试 `== Some("function")`，非 function **被丢弃**而非透传。该 agent 还臆造了不存在的函数名与行号。真实缺陷是"静默丢弃无日志"（见下）。

## 附：低成本可选项

`src/openai/convert.rs` 的 tools 处理点：非 `function` 声明（Codex 的 `type:custom` / `namespace` / `tool_search`，以及 `web_search` 等服务端工具）被**静默**丢弃、无日志。
丢弃本身是刻意的（服务端工具没有上游对应物），但静默会让现象变成"模型不听话、不调工具"，排查方向完全错。
**建议**：加一条 `tracing::debug!` 记录被丢弃的 `type` 与 `name`。先拿线上样本看真实客户端发的是哪几种，再决定要不要按形状支持 —— 不要先猜。

参考实现的分派结构可借（kiro.rs `converter.rs:378-397`：服务端工具显式丢弃并记日志 → 未知 type 有 name 才透传），约 25 行、纯请求侧、不碰状态机。
但**只取"按 type 分派 + 显式丢弃名单"**，不要它的 custom/function 双集合判别 —— 那会导致顶层同名 function 被误发成 `custom_tool_call`（带 `input` 无 `arguments`，客户端拿不到参数）。
