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

# 合并执行计划（2026-08-06）

> 来源：另一个 AI 的「KiroStudio 总账」+ `HANDOFF-2026-08-06.md` + `docs/TASK-CANVAS-IPPOOL-SHIELD.md`
> + `PLAN-openai-and-core-fixes.md` + 我对 34 个会话 452 条用户发言的提取。
> **本文件是唯一派单依据。** 每条都附我亲自核实的结果，行号会漂 —— 按符号名重新定位。
> 置信度：`[实测]` 我跑过命令 · `[代码]` 读码确认 · `[未验]` 来自报告、我没独立验证。

---

## §0 我核对那份总账的结果：三处要更正

**这三条不改会直接误导实现。**

### 🔴 先读：本轮反复出现的**单一**错误模式（六次实例）

> **把「grep 命中数」或「旧时刻的验证结果」当成当前结论。**

grep 只能定位**去哪读**，不能判定**是什么**。六次实例，两个方向的错都犯过：

| # | 我的断言 | 实际 | 我错在哪 |
|---|---|---|---|
| 1 | 「删除整个账号组没做」 | 完整实现（`pendingGroupDelete` + 20+ 三语键） | 只 grep 了 `deleteGroup`/`force: true`，**实现叫别的名字** |
| 2 | 「K4 未动」 | 那行加源码守卫都在 | grep 模式匹配的是**旧行号区间**（块已漂 80 行） |
| 3 | 「A8 完成」 | `handlers.rs` 那处裸奔 | **只查了两处实现中的一处** |
| 4 | 「画布 tsc 0 error」 | 编译失败（缺 `Move` 导入） | 那次 tsc 跑在加画布按钮**之前**，我没重跑 |
| 5 | 「A7/T1 已落地」 | 确实落地了 | 但当时**只数了命中数没读代码**，结论对是运气 |
| 6 | 「字段名是 `adminApiKey`」 | 是 `adminKey` | 凭记忆写进文档，**还当依据派了单** |

**第 6 条造成了真实损失**：`kiro-monitor` 按错键名取值 → `KeyError` + `set -euo pipefail`
→ 脚本第 23 行即退出 → **监控崩两天**，而 `systemctl is-active` 报 active、`gateway-status`
报正常，所以无人发现。agent 没照抄我的错值、自己查了线上才证否我。

**这个仓库有个结构特性会放大这个错**：东西成对出现 —— A8 两处实现、K9 后端+两份前端类型、
`restart_fields` 与 restore 表两份手工名单、`cleanup_verdict` 与 `batch-delete` 两条删除路径、
`credential-card` 与 `credential-row` 两套右键。**查一处就下结论必然错。**

⇒ 判定任何「是否已完成」时的正确做法：**找那个用户动作/数据流对应的代码去读**
（调用点、挂载点、测试断言内容），不要猜它会叫什么符号名。

### 我写的两条源码守卫本身也是纸面测试（同一类，两个变体）

写守卫时**实测踩到两次**，两次都是「回退后守卫仍报绿」：

1. **注释变体**：`include_str!` 读的是原始文本**含注释**，把实现注释掉，`contains` 匹配到
   被注释掉的那行 ⇒ 绿。修法：先剔掉 `//` 开头的行。
2. **换行变体**：剔注释后仍绿 —— rustfmt 把那句乘算断成三行，含换行的 needle 匹配不上。
   修法：**把连续空白归一成单空格**，断言与排版无关。

⇒ 写源码守卫的必备两步：**剔注释行 + 归一空白**，然后**必须做回退验证**看它真的会红。

---

### 更正 1 🔴 测试基线不是 `1234 passed / 2 failed`，是 `1314 passed / 0 failed`

`[实测]` 我刚跑完 `cargo test --no-default-features`，42.37s，**1314 passed / 0 failed / 0 ignored**。

那份总账说「基线 1234 passed / 2 failed，那 2 条是 pre-existing 的
`api_region_setter_endpoint_is_wired`（`include_str!` 单行匹配被 rustfmt 换行打断），别去修」——
**该测试现在是绿的**（`service.rs:5353`）。它的基线是上一轮的、已过期。

⚠️ 后果：若照它写的「有 2 条 pre-existing failed，别去修」派单，实现 AI 会**忽略自己引入的真失败**
（以为那就是那 2 条）。派单里必须写死 **0 failed，任何红灯都是你弄坏的**。

### 更正 2 🔴 K4 的依据引错了（结论对，依据是另一个已修的缺陷）

总账写「`provider.rs:605-610` 注释记载这个 split 已烧过号一次」。
`[代码]` 那个区间是 `with_proxy` 的**参数文档**，没有烧号记录。

真正的烧号注释在 **`fn endpoint_for`**（`provider.rs`）与守卫测试
**`endpoint_for_must_use_effective_endpoint_not_raw_field`** —— ⚠️ **按符号名 grep，不要按行号**：
本条原写的 `:654` / `:3010` 已漂（守卫测试实际在 `:3018`），而**按行号找不到东西正是这条误引
被反复怀疑、又反复传抄的原因**。它讲的是**另一个已修的缺陷**：
`endpoint_for` 曾直读 `credentials.endpoint` 原始字段、漏了 `ksk_` 号自动路由 CLI ⇒ 健康号打 IDE
端点 403 ⇒ 连续 6 次判死号自动禁用。**那条已修且有源码守卫钉死。**

K4 自己的真实机制（我查到的，比总账更具体）：`token_manager.rs` 的 `reload_config` 为治
proxy split-brain，把一批 restart-only 字段用**旧值覆盖回 new**（`proxy_url`/`tls_backend`/
`host`/`port`/`region`/`callback_base_url`/`admin_api_key`/`api_key` 等）。`default_endpoint`
此前**不在这张表里**，而 `service.rs` 又把它 push 进 `restart_fields`。于是改配置后只要同批动了
任何热字段触发 reload，ArcSwap 拿到新值、而 `KiroProvider` 仍持构造时拷贝
⇒ 对话走旧端点，`region_probe.rs` 与 `token_manager.rs` 的余额/验活/region 探测走新端点。

⇒ 修法是那张表加一行 `new.default_endpoint = old.default_endpoint.clone();`，与既有范式完全一致。
✅ **该修法已落地**（2026-08-06 复核）：restore 块已含那一行，并有两个守卫测试
`reload_config_must_restore_default_endpoint_to_startup_value` /
`reload_config_restore_list_must_contain_default_endpoint`。**别再当待办派单。**

⚠️ **两条依据不可互换引用**（这是本条误引的根源）：
「`default_endpoint` 漏出 restore 白名单」的依据是 **`reload_config` 的 restore 块**；
「已经烧过号一次」的依据是 **`fn endpoint_for`**。混引会让下一个人去 `:605` 找不到东西。

### 更正 3 🟠 `api_region/auth_region 绕过 region 钉死` 我倾向判为误读

`[代码]` `credentials.rs:453` 明写「凭据的 region/auth_region/api_region 字段来自不可信来源」，
`:482-483` `effective_api_region` 的优先级注释是「凭据.api_region（**过白名单**）> config.api_region
> config.region」。即它**有白名单校验**，且「面板手改能立刻生效」正是 08-06 那轮 region 修复
刻意做的（`OPEN-ISSUES` §四 第②条）。

⇒ **列为「需先证实再动」，不要当确认项修**。改错方向会把刚修好的手改 region 能力废掉。

---

## §1 UI 账单：绿格子密集恐惧（用户当面提的，本轮最高优先）

### 根因（不是 bug，是刻意设计）

`[代码]` `admin-ui/src/components/overview/GlowGrid.tsx` —— 它的文档注释原话：

> GlowGrid —— GPU / CUDA 核心阵列（算力墙点阵）。
> 阵列：**小而密的方块核心**规整排布，像 GPU die 上的 SM / CUDA 核心墙，**一格 = 一个号**。

具体是四个因素叠加成「密集绿墙」：

| 因素 | 位置 | 效果 |
|---|---|---|
| 格子 22px 起、`auto-fill` 铺满整行 | `:65` `repeat(auto-fill, minmax(22px, 1fr))` | 号越多铺得越满，无上限、无分组、无留白 |
| 健康色 = emerald 绿 | `credViz.tsx:27` `healthy: '16 185 129'` | 池子健康时**全绿**，无色相变化 |
| 每格常驻呼吸动画 | `:96-106` + `glow-grid.css:13` `gg-breathe` | N 个格子各自错相位脉动 = 视觉噪音 ×N |
| 每格再叠 3 层 | `:108` 白色高光斑 / `:112` hover 辉光 / `:119` 活跃环 | 单格视觉密度高，密集时糊成一片 |

⚠️ **`GlowGrid.tsx` 不在未提交改动里**（`git status` 只有 `overview-page.tsx`/`FireCanvas.tsx`/
`StatusBars.tsx`）。最后一次改它是 `cc727b7`。⇒ **「回退」回退不到更好的版本** ——
它一直是这个样子，是号池变大后才显出问题。所以只能改，不能退。

### 三条修法（我建议全做，成本都很低）

1. **格子变大 + 密度上限**：22px → 32~40px，并给容器设最大列数（如 12 列）。
   超出的号不再无限铺，改成分页或滚动。**这条单独就能解决密集感。**
2. **同色改为按分组着色**：一个 `cloneGroup`/`apiKeyHash` 一个色相（复用任务 A2 的
   `slotOf` 分组键思路），健康态只决定明度/描边。绿墙变成可读的分组带。
3. **呼吸降噪**：常驻呼吸只保留给**在途 > 0** 的号（`busy`），健康静默号改静态。
   `:96` 的 `lit &&` 改成 `busy &&` 即可，动画量从 N 降到实际在途数。

⚠️ 三条都碰视觉，**必须截图验**（CLAUDE.md：涉及视觉的结论必须截图看）。
`tsc --noEmit` 只能证明不崩，证明不了好看。

---

## §2 已核实、可直接开工（按我的验证结果重排）

**总账那张表我逐条回源码查了，结论一致的照收，不一致的已在 §0 更正。**

| # | 问题 | 我的核实 | 严重度 |
|---|---|---|---|
| A1 | `$ref` 展开指数爆炸 | ✅ `converter.rs` 只有 `MAX_REF_DEPTH: usize = 16`（限**链长**），**「节点预算」这个概念根本不存在**（`MAX_SCHEMA_NODES` / `node_budget` / `nodes_visited` 全零命中 —— ⚠️ 这不是「常量存在但未生效」，而是**符号本身不存在**，`MAX_SCHEMA_NODES` 是下方 B1 建议**新增**的名字）⇒ 同级递归复用同 depth 可指数膨胀 | 🔴 DoS |
| A2 | `merge_tool_input` 吞掉单独成帧的 `{` | ✅ `stream.rs:4780`（不是 :4798，已漂）。第 5 步缺「`buf` 必须是完整 JSON」前置 | 🔴 |
| K3 | 刷新写回整体替换 | ✅ `token_manager.rs:8208` 守卫只比 `refresh_token`，`:8216` `entry.credentials = new_creds` 覆盖全部字段并**落盘** | 🔴 |
| K4 | `default_endpoint` 漏出 restore 白名单 | ✅ 成立，机制见 §0 更正 2。**一行修复** | 🔴 |
| A8 | `contextUsagePercentage` 无**下界**守卫 | ⚠️ **本行原判定不完整，已更正**：这个判据有**两处**实现（`stream.rs` 流式 + `handlers.rs` 非流式缓冲聚合），我当时只查了 `stream.rs` 就宣布「A8 已完成」。实际 `handlers.rs` 那份**裸奔**（直接乘、只有 `>= 100.0` 上界）。✅ 已于 2026-08-06 晚修：判据抽成 `stream.rs::context_input_tokens_from_pct` 由两处**物理共用**，并加源码守卫 `context_usage_predicate_must_be_shared` 钉死（断言必须调共享函数 + 本文件不得出现自己的乘算） | 🟠 |
| A7 | OpenAI 错误出口丢弃 `Retry-After` | ✅ `src/openai/` 全目录 `RETRY_AFTER`/`retry-after` **零命中** | 🟠 |
| C1 | Codex 请求侧 `custom` 工具声明被丢弃 | ✅ `openai/convert.rs:690` 只认 `Some("function")`，无 `else` 无日志。而入站侧 `:639` 支持 `custom_tool_call` 且有测试 `:2007` ⇒ **不对称成立** | 🟠 |
| C2 | `previous_response_id` 零实现 | ✅ `openai/handlers.rs:88` 只有一行注释说"忽略" | 🟠 |
| A6/K9 | retries 聚合层已建、缺 API 出口 | ✅ `usage_stats.rs:120/123` 有字段；`usage_handlers.rs` **零命中**（时间戳 03:50，从未被碰）；`api.ts:621-634` 前端类型已就位并注明「后端不下发」 | 🟠 |
| K5 | 客户端断连 ⇒ 整条流式记录消失 | ✅ 5 个 `emit_record` 全在 unfold 内（`:1444/:1680/:2250/:2350/:2887`，非总账给的 :1592 等）。`report_credits` 在 emit 体内 ⇒ 真实 credit 永久丢 | 🔴 但**先对账量化再动手** |
| T1 | `AuthTransient` 建好未接线 | ✅ `cooldown.rs:72` 变体在、`:100` = 20s，注释 `:29` 自称「等 provider 的 401/403 路径接入」。`provider.rs` 四个调用点（`:994/:1840/:1871/:1902`）仍走 86400s | 🟠 |
| U1 | 绿格子密集恐惧 | ✅ 见 §1 | 🟠 用户当面提 |
| U2 | 分身导入默认关闭 | ✅ **零实现**（`config.rs` 无任何 clone 配置项） | 🟠 用户 #342 |
| U3 | 删除整个账号组 | ✅ 后端 `batch-delete` 端点有（`router.rs:66`），**前端零实现**（`deleteGroup`/`force:true` 零命中）。⚠️ `TRACKING` §E 记该 workflow「已完成」—— **产出没落地** | 🟠 用户 #342 |
| U4 | 创建组时勾选「删除主份」 | ✅ 未见实现 | 🟡 用户 #368 |

### 一条需先证实的（不要当确认项）

| # | 问题 | 为什么先证实 |
|---|---|---|
| V1 | 上游不回 `expires_in` ⇒ 每请求刷一次 token | `[代码]` `token_manager.rs:314` 与 `:463` 都是 `if let Some(expires_in)`，**无 else** ⇒ 上游不回时 `expires_at` 保持旧值。若旧值已过期则每请求都判过期 → 刷新。**机制成立**，但「上游是否真的不回」未实测。⚠️ 会直接放大风控，值得优先取证 |

---

## §3 派单批次（按文件冲突分组，不是按优先级）

**分批的唯一依据是「同一文件只能有一个 agent」** —— 本仓历史事故全部来自并发改同一文件。
本机 8 核 ⇒ workflow 并发上限 6（`min(16, ncpu-2)`），每批不超过 6 个。

⚠️ **每个 agent 必须显式传 `model: 'opus'`**（`~/CLAUDE.md` 实测：会话模型 `claude-opus-5[1m]`
的 `[1m]` 后缀在子 agent 侧解析不了，一次 workflow 12 个 agent 死了 11 个，烧掉约 100 万 token）。

### 批次 1（6 个，零文件重叠）

| Agent | 任务 | 独占文件 |
|---|---|---|
| B1-converter | **A1** 节点预算（**新增**常量 `MAX_SCHEMA_NODES` —— 现有代码里只有 `MAX_REF_DEPTH`，别拿这名字去 grep 锚点；**不要**改成同级也 depth+1） | `anthropic/converter.rs` |
| B1-stream | **A2** 第 5 步加完整-JSON 前置 + **A8** 下界守卫（同文件，合并给一个 agent） | `anthropic/stream.rs` |
| B1-token | **K4** restore 表加一行 + **K3** 陈旧守卫改逐字段合并 + **V1** `expires_in` 缺失取证 | `kiro/token_manager.rs` |
| B1-openai | **A7** `Retry-After` 透传 + **C1** custom 工具声明 + 丢弃时加日志 | `openai/handlers.rs`、`openai/convert.rs` |
| B1-usage | **A6/K9** 三个输出结构 + `usage_handlers.rs` 出口 + 前端**两份**类型定义 | `usage/usage_stats.rs`、`admin/usage_handlers.rs`、`types/api.ts`、`api/ops.ts` |
| B1-glowgrid | **U1** 绿格子三条修法（§1） | `overview/GlowGrid.tsx`、`glow-grid.css` |

### 批次 2（5 个，依赖批次 1）

| Agent | 任务 | 依赖 | 独占文件 |
|---|---|---|---|
| B2-authtransient | **T1** 接线：`token_manager` 加 transient 入口 + `provider.rs` 四点改用 | B1-token | `kiro/provider.rs` |
| B2-clone | **U2/U3/U4** 三条分身遗漏 | — | `admin/service.rs`、`admin/types.rs`、`model/config.rs`、`clone-management-card.tsx` |
| B2-k5-measure | **K5 第 1 步**：先对账量化（`report_success` vs `traces` success 计数），**拿到比例再决定是否加 RAII** | — | 只读 + 写报告 |
| B2-codex-store | **C2** `previous_response_id` 有状态链（参照 WindsurfAPI 的 `response-store.js`，**避开 W3–W7 五条缺陷**） | B1-openai | `openai/handlers.rs`（B1 完成后） |
| B2-docs | **§0 三处更正** + `PROTOCOL.md:196` `q.*`→`runtime.*` + `ide.rs:4`「已停用」+ `CLAUDE.md` #20–22 + **C7 过期条款** | — | 纯文档 |

⚠️ **C7 必须改**：`TASK-CANVAS-IPPOOL-SHIELD.md` C7 写「不给预算耗尽换 503」，
而用户 #428（08-05 21:28）明确要求「把 Cursor 对 429 立刻停会话改掉，改为重试，我们作为缓冲」，
实际落地的是用户的要求（`handlers.rs:1029` `ABSORB_BUDGET_EXHAUSTED_MARKER` + `:1057` 503 覆盖）。
⇒ **代码是对的，C7 那条已过期。不改的话下一个 agent 会照 C7 把它删掉。**

### 批次 3（前端大件，依赖批次 1/2）

| Agent | 任务 | 依赖 |
|---|---|---|
| B3-selection | 任务 A 的 **S2** 共享选区 store + 迁移 `dashboard.tsx` 6 组批量处理器 | — |
| B3-canvas | 任务 A 的 **S4+S5** 选择代数 + 框选 + 画布视图 | B3-selection |
| B3-ippool | 任务 B 的 **B1 `proxy_node_id` 5 跳** + B4 UI 入口 | B2-clone |

---

## §4 硬约束（逐条来自实测事故，违反造成真实损失）

1. 🔴 **判据是 `0 failed`，不是某个具体的通过数**。⚠️ 本文档写作时是 1314，当晚随 agent
   陆续落地涨到 **1349** —— **任何写死的通过数都会过期**（这正是本文件 §0 那个错误模式的
   一个实例：把旧时刻的数当当前状态）。派单时只写「0 failed」+「跑前先记下当前数」。
   任何红灯都是本轮弄坏的，
   **不存在 pre-existing failed**。总账说的那 2 条已过期，见 §0 更正 1。
2. 🔴 **禁止对已存在文件跑 `rustfmt`/全仓 `cargo fmt`** —— 历史事故：把别人整树改动冲掉；
   另一次 861 插入/212 删除全文件重排。只 fmt 自己新增的段落。
3. 🔴 **禁止 git 写操作**（`checkout`/`switch`/`stash`/`reset`/`commit`/`add`）—— 工作树 127 条未提交。
4. 🔴 **禁止 heredoc 写文件**（`cat > f <<'EOF'`）—— 实测使整个会话的 Bash **永久静默且转录零记录**。
   写文件一律 Write/Edit。
5. 🔴 **大块改代码用唯一锚点 replace，不要按行号 splice** —— 历史事故：481 行连同整个 IIFE 被复制。
6. **配置项新增必须带 `#[serde(default)]`** —— 线上 config.json 是既有文件，缺 default → 服务起不来。
7. **一律 `--no-default-features`**（`default = ["native-tls"]` 与出厂配置相反）。
8. **行号已漂，按符号名重新定位**。本文件里 A2 是 `:4780`（总账说 :4798）、K5 是
   `:1444/:1680/:2250/:2350/:2887`（总账说 :1592 等）—— 都以符号为准。
9. **每个 agent 显式 `model: 'opus'`**，不要依赖继承。
10. macOS/zsh：无 `timeout`（是 `gtimeout`）；`--include='*.rs'` 必须加引号。

### 「回退即 FAIL」是唯一可接受的验证

每条修复：把它改回旧行为 → 跑 → **必须 FAILED** → 还原 → **grep 确认无 `TEMP-REVERT-CHECK` 残留**。
报告里贴**实际观察到的输出**，不是「我加了测试」。

本仓已记录 **9 种「纸面测试」形态**，最容易踩的三种：
① 漏 struct 级 `#[serde(rename_all)]`；② 中文注释使字节偏移落在多字节字符中间 ⇒ 断言恒真；
③ **测了分支内部、没测分支顺序**（真实事故：四条测试三次回退全过而修复无效）。

---

## §5 明确不做（有证据，重做即回归）

- **`wait_for_capacity` 丢唤醒竞态** —— 三个符号全仓零命中，真实机制无 `Notify` 原语，结构上不可能。
- **不给预算耗尽换回 429** —— 用户 #428 明确要 503 缓冲，见 §3 批次 2 的 C7 说明。
- **不把吸收循环移出 `call_api_with_retry`** —— 有源码守卫测试钉死。
- **不逐轮上报 AIMD** —— 四个上报点的 `absorb_round == 0` 门去掉任一个会触发 RPM 单调滑到 floor 锁死。
- **不碰线上限流配置**（`credentialRpmLimit`/`inboundTargetRpm`）—— 依据在 `ws-vps/docs/02-tuning.md`，
  且「容量口径是假的」那节说明按实测改小会把吞吐掐死一个数量级。
- **不擅自停 shield 或改 Caddy 指向** —— 正在保护生产。
- **IDE 端点 host 保持 `runtime.*`** —— 已用 DNS 实测 + 4360 请求/99.9% 成功判定不改。
  ⚠️ 但用户 #227/#266 两次粘过一段注释说 `runtime.*` 比 `q.*` 限流狠 25–40% ——
  **那是第三方项目的注释，与本仓 4360 请求实测冲突，列为待取证，不据它改 host。**

---

## §6 一句话

本轮真正的新增信息有三条：**测试基线是 1314/0 不是 1234/2**（派单据此才不会误判红灯）、
**K4 的依据引错但结论对**（真实机制是 reload restore 表漏一行）、
**绿格子是刻意设计而非回归**（回退不到更好的版本，只能改）。
其余 15 条已核实项按 §3 三批派单，文件零重叠。

