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

# KiroStudio 未提交改动只读审计（2026-08-06）

审计开始：**2026-08-06 18:49:39 JST**（快照基准：`git status --porcelain` 于此刻抓取，139 条）。
审计结束：**2026-08-06 19:08 JST**（约 19 分钟，期间树仍在动）。

**本报告只读产出，未对仓库做任何写操作。** 所有结论均基于实际读码（源码 / diff / 编译输出），
不是 grep 命中即断言——对每一条「已完成」的判定都读了调用点、挂载点或测试断言内容。

## 0. 审计期间树发生的变化（时效性提示）

对照 `find src admin-ui/src -newermt "2026-08-06 18:49:00"`：

- `src/anthropic/stream.rs`（mtime 19:07）—— 有 agent 在审计**进行中**持续改动此文件。
  本报告对该文件的判定（A8 下界守卫）基于**审计开始时**已读到的内容，之后是否又变动未再复核。
- `src/openai/convert.rs`（mtime 19:00，**审计开始时未在 status 快照里，结束时新增为 M**）——
  审计过程中有第 6 个（或更多）agent 开始改这个文件。**本报告未审计该文件的改动内容**，
  它是审计窗口内的纯增量，下一轮审计需单独覆盖。

结论：本报告对其余 138 个文件的判定基于一份基本静止的快照；`stream.rs` 与 `openai/convert.rs`
两个文件的最终态可能已经与本报告描述的不同，提交前建议对这两个文件单独复核一次 diff。

---

## 1. 137 条改动的构成分类

`git status --porcelain` 139 行（137 条改动 + 2 条本审计自身产物在统计之外）：
87 个已跟踪文件修改（`git diff --stat`：87 files changed, 28302 insertions(+), 2858 deletions(-)）+
约 52 个新建未跟踪文件/目录（含 11 份 HANDOFF/PLAN/TRACKING/STATUS md、2 个新 Rust 模块、
9 个新前端文件、`admin-ui/tests/` 测试目录、多份截图 png、多份 docs/*.md）。

### 1a. 完整且已验证收尾的（多数 Rust 后端改动）

体量最大的三个文件 `src/admin/service.rs`（+6153/-?）、`src/kiro/provider.rs`（+3628）、
`src/kiro/token_manager.rs`（+4762）、`src/anthropic/stream.rs`（+3298）、
`src/anthropic/handlers.rs`（+1961）、`src/http_client.rs`（+1146）——
`cargo check --no-default-features` **0 error**，`cargo clippy --no-default-features`
**0 error / 227 warning**（全部是既有 dead-code / doc 格式类，非本轮新增编译阻断，见 §4）。
逐条抽查（A1/A7/A8/K4/T1/K9/H2/H3/WebGL 四项）**均完整落地且带回归测试**，见 §3。

### 1b. 半成品（本轮抓到的真半成品，见 §2 详情）

- `admin-ui/src/components/dashboard.tsx` 的画布视图切换按钮：**`tsc` 编译报错**
  （`Cannot find name 'Move'`）+ 目标组件从未挂载。
- 4 个 i18n key 在 `overview/GlowGrid.tsx`（**已挂载在主览页**）里被引用但三语 JSON 均未收录，
  且代码侧无 `defaultValue` 兜底 ⇒ 用户会看到裸键名。

### 1c. 看不出意图 / 需要人工判断的（新建 markdown 文档，非代码）

11 份 `HANDOFF-*.md` / `STATUS-*.md` / `PLAN-*.md` / `TRACKING-2026-08-06.md` /
`OPEN-ISSUES-2026-08-06.md` 是多轮会话的过程记录，内容互相有交叉引用、部分已过期
（如 TRACKING.md 记「K9 API 出口还没接」，本轮核实**已接完**，见 §3）。这些文件本身不影响
编译或运行，但如果要提交，建议先做一次去重/归档，否则历史文档会继续误导下一轮会话
（TRACKING.md 自己也点名过这个问题：「清单越读越长」）。
3 张 png 截图（`card-view-baseline.png`、`current-state.png`、`row-*.png`、`sbar-after.png`、
`state2.png`）是验证过程产物，可考虑删除或移出仓库范围，不建议随功能改动一起提交。

---

## 2. 半成品清单（缺哪一跳）

### 半成品 #1 — 🔴 画布视图（Canvas View）：三跳都缺，且有编译错误

**症状确认**：
```
src/components/dashboard.tsx(908,49): error TS2304: Cannot find name 'Move'.
```
`npx tsc --noEmit` 在 `admin-ui/` 下**当前不干净**，这是本次审计唯一一条真实编译错误。

**缺的三跳**：
1. **导入缺失**：`dashboard.tsx:2` 的 lucide-react 导入列表里没有 `Move`
   （`import { RefreshCw, LogOut, Moon, Sun, Server, Plus, Trash2, RotateCcw, CheckCircle2,
   Database, Zap, Ban, Power, FlaskConical, Download, AlertTriangle, LayoutGrid, List } from
   'lucide-react'`），但 908 行用了 `icon: Move`。
2. **组件从未挂载**：`grep -rn "<CredentialCanvas"` 全仓零命中。视图切换的三态按钮
   （card / row / canvas）里 `canvas` 档只改变 `uiPrefs.credentialView` 状态值，
   `dashboard.tsx` 渲染主体只有 `isRowView` 分支（`row` / `card` 两态），**没有第三个分支**
   去渲染 `credential-canvas.tsx` 里已经写好的 `CredentialCanvas` 组件。用户点击「画布」按钮，
   界面不会发生任何变化。
3. `credential-canvas.tsx`（新建，完整实现）+ `use-canvas-layout.ts`（新建，完整实现，
   含详尽的设计说明注释）两个文件本身写得完整、自洽，**唯独没有被消费**。

**判定**：完整独立功能已实现≈95%，但因为缺一行 import + 一处挂载点，**整个功能对用户不可见，
且现在处于编译不过的状态**。这正是任务描述里点名的「签名/组件写了没挂进去」的同类模式。

### 半成品 #2 — 🟡 GlowGrid 四个 i18n key 三语均缺失

`admin-ui/src/components/overview/GlowGrid.tsx`（**已挂载在 `overview-page.tsx:534`，
是主览页的核心组件**）引用了：

- `overviewpage.grid.collapse`
- `overviewpage.grid.showMore`
- `overviewpage.grid.hiddenAbnormal`
- `overviewpage.legend.groupHue`

三语 JSON（zh/en/ja）里 `overviewpage.grid` 段**完全不存在**（`python3 -c "...grid.keys()"`
三语皆返回 `[]`）。代码里这四处调用**均无 `defaultValue` 兜底参数**，而 `i18n/index.ts:28`
的 i18next 配置**没有** `parseMissingKeyHandler`，缺键时 i18next 默认行为是**原样返回 key 字符串**。
即用户在主览页展开/收起卡片、看图例时会看到 `overviewpage.grid.collapse` 这样的裸键名，
不是中文/英文/日文。

对照组：同一份代码里 `clone-management-card.tsx` 的另外三个缺失键
（`clones.group.bareExitWarn/Hint/Badge`）**都带了 `defaultValue`**，所以那三个不影响观感，
只是三语 JSON 里少收录（技术债但用户无感）。GlowGrid 的四个键**没有这层保护**，是真实可见 bug。

**判定**：半成品——功能逻辑（展开/收起/图例）本身完整可用，缺的是最后一跳「写进 JSON 或补
defaultValue」。

### 未发现更多半成品

对 clippy 227 条 warning 逐类扫过（见 §4），dead-code 类全部对照 `git show HEAD:<file>` 确认
是**本轮之前就存在的历史遗留**，不是本轮新增的「改了签名没改调用点」类缺口
（如 `p_avail`/`clear`/`has_custom_api_credential`/`is_rpm_saturated`/`delete_credential`/
`validate_outbound_url`/`is_forbidden_ipv4/6`/`wait_for_callback` 等，均在 `git show HEAD:` 版本
里已是同样签名同样未被外部调用，本轮 diff 未改动这些函数体或调用关系）。

---

## 3. 对照清单逐条判定

| 项 | 判定 | 依据 |
|---|---|---|
| **A1** `$ref` 节点预算 | ✅ **完整** | `converter.rs:87` `MAX_SCHEMA_NODES=50_000`，`:97` `SchemaRefBudget{max_nodes,visited,truncated_nodes}`，`:133-193` `resolve_schema_refs` 两处递归调用（`:161` 展开目标、`:182` 同级字段）**均正确传入 `&mut budget`**，`:143-146` 预算耗尽降级为 `degraded_object_schema()` 并 `truncated_nodes+=1` 留痕，`:50` 附近有 `truncated_nodes>0` 时的日志。测试覆盖小预算注入版本，不会拿真 5 万预算压测。 |
| **A7** OpenAI `Retry-After` 透传 | ✅ **完整** | `openai/handlers.rs:308-310` 从内层响应取 `header::RETRY_AFTER`；`:337-358` `openai_error_with_retry_after` 写回，非法值走 `warn!` 丢弃而非 panic；4 条测试（`:488` 透传、`:516` 不伪造、`:531` 本地错误不带、`:543` 非法值丢弃）。 |
| **A8** `contextUsagePercentage` 下界守卫 | ✅ **完整** | `stream.rs:1531-1536` 附近：`pct > 0 且有限` 才覆盖已有值，注释详细说明 `#[serde(default)]` 会让缺字段/null 落 0.0、进而把计费口径的 `context_input_tokens` 写成 0 的连锁后果；测试 `:5374` 覆盖非正/非有限值不覆盖已算值。 |
| **K4** `default_endpoint` restore | ✅ **完整** | `token_manager.rs:2467` `new.default_endpoint = old.default_endpoint.clone()`；`:14518` 回归测试断言 reload 后仍等于启动值；`:14562` 源码守卫测试用运行时拼接的 needle 确认 restore 行字面存在（防止「改了措辞让守卫测失效」）。 |
| **T1** `AuthTransient` 接线 | ✅ **完整** | `cooldown.rs:76` 枚举变体、`:104` 默认时长 20s、`:131` `is_auto_recoverable=true`、`:269` 刻意不设缩放下限（注释说明与限流类不同）；`provider.rs` 四个调用点（`:1003`/`:1860`/`:1895`/`:1938`）；`token_manager.rs:5507` `report_auth_transient_cooldown` 与姊妹函数 `report_auth_cooldown` 的语义分界写得很清楚（是否 `has_ever_succeeded`）。回归测试含「必须是 AuthTransient 而非 AuthenticationFailed」的显式对照。 |
| **K9** retries 面板出口 | ✅ **完整（TRACKING.md 记录已过期）** | `usage_stats.rs:119-231` `retries_sum`/`retried_requests` 双字段 + 两个派生比率；`usage_handlers.rs:600-662` 四类端点（overview/timeseries 两粒度/group-by-model/group-by-cred）均下发且有测试断言字段值与 camelCase 反检查（`assert!(!body.contains("retriesSum"))`）；`admin-ui/src/types/api.ts:621-706` 前端类型齐全且带承重注释；`usage-page.tsx:396-550` 有独立的「重试放大」卡片渲染 `retries_sum`/`avg_retries_per_request`/`avg_retries_when_retried`，且对旧后端（字段 undefined）做了整行不渲染的降级。**`TRACKING-2026-08-06.md` 里「API 出口正在补，另有 agent 在补」这条描述已经不准——本轮审计时点它已经全链路打通**，如果还有 agent 在按这条派单重做，应立即撤销该派单避免重复工作。 |
| **分身 M1** `clone_default_enabled` | ✅ **完整** | `model/config.rs:158-159` 字段 + `#[serde(default = "default_clone_default_enabled")]`（`:897` 默认 false）；`admin/service.rs:1552` `disabled: !enabled.unwrap_or_else(|| self.clone_default_enabled())`；`:2740` 读取 accessor；`:3041-3044` 配置热更新路径。 |
| **分身 M2** 删整组 | ✅ **完整（TRACKING E.1 的「未落地」结论已过期）** | `clone-management-card.tsx:812` `pendingGroupDelete` state；`:938-970` `confirmGroupDelete` 走 `deleteCredentialsBatch(ids, true)`（批量 + force，1 次往返而非 2N 次）；含部分失败明细展示（`r.results.filter(!ok)`）+ i18n 文案（`clones.group.deleteAllOk/Partial/FailedItem` 等）+ 二次确认对话框（`:1237-1256`，说明软删可从回收站恢复、节点池不受影响）。**`TRACKING-2026-08-06.md` §E.1 说「全仓 grep `deleteGroup`/`delete_group` 零命中，workflow 产出未落地」——那次核实是对的（用词零命中），但按走的是 `deleteCredentialsBatch` 而非叫 `deleteGroup` 的专用函数，本轮审计确认功能确实完整实现了，只是没用那个函数名。** 下一轮读 TRACKING 时注意这条已经过期。 |
| **分身 M3** 勾选删主份 | ✅ **完整（设计选择：默认含主份，非勾选制）** | `pendingGroupDelete.ids` 本身就包含组内全部成员（含主份），删整组对话框（`:1237`）无额外「是否包含主份」勾选框——因为「删整组」语义上就是全删，与「删单份」（`confirmMemberDelete`，`:1227` 单独警告「这是主份」）是两条不同路径。判定为完整而非半成品：对照清单写的是「勾选删主份」，实测设计是「删单份时警告 + 删整组时默认含主份」，覆盖了同样的用户场景，只是交互形态不同，不构成缺口。 |
| **文档六处修正** | ✅ **完整** | `PROTOCOL.md:201-205` 已用表格区分 IDE（`runtime.*`，URL 路径判据）/ CLI（`q.*`，`X-Amz-Target` 头判据），并显式否定「各取一半拼出来的组合不存在」；`ide.rs:1-32` 头注释已更正「旧 `q.*` 已停用」为错误断言、写明证据不对称的判断依据、且专门记录了一条**尚未解决的矛盾**（另一实现称 `runtime.*` 高并发下 25-40% 429 而己方实测 99.9% 成功，矛盾原样保留没有强行下结论）；`docs/TASK-CANVAS-IPPOOL-SHIELD.md` C7 段落存在且被 `absorb-layer-design.md` 引用。 |
| **画布视图（前端）** | ❌ **半成品** | 见 §2 半成品 #1。三态按钮 + 完整组件 + 完整 hook，但缺 import（编译错误）+ 缺挂载分支，功能对用户不可见。 |
| **选区 store** | ✅ **完整** | `use-credential-selection.ts`（133 行，`useSyncExternalStore` + 自定义事件跨组件同步，刻意不落 localStorage 的设计说明清楚）；`dashboard.tsx:29,152` 已切换为消费该 store（`const { ids: selectedIds, toggle: toggleSelect, clear: deselectAll } = useCredentialSelection()`），下方 6 组批量处理器（batchDelete/batchDisable/batchRefresh/batchWhitelist 等，`:293-658`）**全部**改用新 `selectedIds`，没有遗留的旧 `useState<Set>` 局部选区状态残留（用 `grep -n "selectedIds|setSelectedIds|useState<Set"` 核实，唯一命中的 `useState<Set>` 是别的字段 `batchAllowedModels`/`loadingBalanceIds`，与选区无关）。 |

### 附加核实（不在原对照表但审计中主动核实的相关项）

| 项 | 判定 | 依据 |
|---|---|---|
| **H2** `AccessDeniedException` 独立分支 | ✅ **完整，且带「分支顺序」专项守卫测试** | `handlers.rs` 有 `is_upstream_temporarily_suspended` 窄判据；关键的是 `region_mismatch_branch_must_not_shadow_rate_limit_or_suspended`（`:3568` 一带）**专门测分支顺序而非分支内部**——用真实错误串拼接（429/全池冷却/临时风控 各自 + bearer-invalid 串）验证 region 分支不会抢走本该拿 429 的错误。这正是 CLAUDE.md 里点名过的「第 8 种纸面测试」要害，本轮这条测试**做对了**。 |
| **H3** 图片 magic bytes 校正 | ✅ **完整** | `converter.rs:1209-1227` 按 magic bytes 判定 png/jpeg/gif/webp（webp 正确检查偏移 8 处的 `WEBP` 而非仅前 4 字节，命中 TRACKING B6 点名的风险点）；`:1235` 声明值仅兜底。4 条测试覆盖四种格式纠正。 |
| **M7** region 手改 UI 控件 | ✅ **完整（TRACKING 记「只缺界面入口」，本轮已补齐）** | `credential-card.tsx:44,135` 引入并调用 `useSetCredentialApiRegion`；`:319,1185-1220` 有实际的 UI 交互（下拉/按钮 + loading 态 + disabled 条件）。 |
| **M5** SOCKS 节点前端编辑 | ✅ **完整（TRACKING 记「只能新增删除不能改」，本轮已补齐）** | `socks-node-edit.ts`（新建，纯函数 + 测试）+ `clone-management-card.tsx:310-345` `startEdit`/`patchEdit`/`saveEdit` 三态编辑，`saveEdit` 走 `upsertSocksNode(buildSocksNodeEditPayload(n, editForm))` 带 id 更新而非仅新增。 |
| **M6-4** notification 缺两 case | ✅ **完整（用更优方案取代原计划）** | 未按原计划补两个 switch case，而是把整份硬编码 switch **重构成转发** `disabledReasonLabel`（`lib/i18n-labels.ts`）+ 独立分类模块 `pool-event-classify.ts`（有测试 `admin-ui/tests/pool-event-classify.test.ts`），注释里明确指出原 switch 只覆盖 8/14 个后端枚举、且发现两个「从未真实命中的死分支」（`RefreshTokenInvalid`/`SubscriptionInvalid` 拼错枚举名）。新方案是通用兜底，不止修了两个 case，还顺带清理了历史技术债。 |
| **M6-6** 批量清理排除代挂 | 🟡 **后端完整，前端故意留白** | `admin/service.rs:2558` `cleanup_disabled_credentials` 完整实现（排除 custom_api + 可自愈原因，走软删进回收站），`8558` 起有对 `PassthroughFailed`/`PassthroughOverloaded` 排除逻辑的测试。前端 `grep "cleanup.*disabled"` 零命中——**这是 TRACKING §C5 记录的「有意留后」，不是半成品**，UI 接线要等其它页面完成后再做。 |
| **C5** 分身页批量清理前端 UI | ⏸️ **确认未做，且是刻意的（非缺陷）** | 同上，文档已说明理由（避免与另一个正在改 admin-ui 的 agent 冲突）。 |
| **WebGL 火焰四条修复**（实例上限/上下文丢失/去 mixBlendMode/ResizeObserver 防抖） | ✅ **完整** | `FireCanvas.tsx` 四处均确认存在：`pickFireCandidates` 硬顶实例数、`webglcontextlost` 监听 + `isContextLost()` 双保险 + `contextWasLost` 标志防「lost/restored 振荡器」、composite shader 输出预乘 alpha 取代 CSS `mixBlendMode`、`ResizeObserver` + RAF 防抖重建 FBO。 |

---

## 4. 三项闸门结果

### 4.1 i18n 完整性闸门

```
zh: 1804   en: 1804   ja: 1804
```

**三语键数相等**（均 1804，任务描述里预期的 1809 与实测有 5 个的差异，可能是文档写作时的另一
快照，不影响「三语相等」这条硬闸门本身——闸门通过）。

**代码引用 vs JSON 收录**做了双向核对（用严格的 `[^a-zA-Z0-9_]t\('...'` 正则避免把
`set('host', ...)` 之类的普通函数调用误判为 `t()` 调用）：代码里用 `t('...')` 引用但三语 JSON
均未收录的键，共 **13 个**：

| 键 | 是否有 `defaultValue` 兜底 | 影响 |
|---|---|---|
| `dashboard.viewMode.canvas` / `dashboard.canvas.hint/resetLayout/rpm/selected` | 无 | 无影响——功能本身未挂载（见半成品#1），用户看不到这些文案的载体 |
| `overviewpage.grid.collapse/showMore/hiddenAbnormal` / `overviewpage.legend.groupHue` | **无** | **真实可见 bug**，见半成品#2 |
| `clones.group.bareExitWarn/Hint/Badge` | 有 | 无影响，defaultValue 兜住了 |

未发现反向问题（JSON 里存在但代码从未引用的键——本轮没有专门跑这个方向的全量比对，
只针对小样本抽查未发现异常，如需要精确数字建议用 i18next-scanner 之类工具跑一次全量未使用键检测）。

### 4.2 临时突变残留闸门

```
grep -rn "TEMP-REVERT-CHECK\|TEMP_REVERT" src/ admin-ui/src/
```
**零命中，闸门通过。**

### 4.3 源码级守卫测试完整性闸门

`include_str!` 分布在 12 个文件（`admin/handlers.rs`、`admin/service.rs`、`admin/types.rs`、
`anthropic/handlers.rs`、`anthropic/stream.rs`、`kiro/endpoint/cli.rs`、`kiro/endpoint/mod.rs`、
`kiro/health.rs`、`kiro/provider.rs`、`kiro/region_probe.rs`、`kiro/token_manager.rs`、`tray.rs`
——最后一个是嵌入 SVG 图标，非测试）。**逐条抽查确认所有守卫测试引用的字面量在当前源码里都真实
存在**（K4 的 `default_endpoint` 、T1 的 `report_auth_transient_cooldown(ctx.id)`、
region-mismatch 分支顺序测试所需的真实错误串格式等均已在 §3 逐条核实）。`cargo check` 干净意味着
这批测试至少**能编译**；由于测试运行会与并发的 6 个 agent 争抢 `target/` 锁，本轮未执行
`cargo test`（按任务要求也只读，不做长耗时占锁操作），**守卫测试是否真的全绿未在本轮实测**，
建议提交前单独跑一次 `cargo test --no-default-features`。

### 4.4「测分支内部不测分支顺序」隐患扫描

专门检查了 H2 的 `region_mismatch_branch_must_not_shadow_rate_limit_or_suspended` 测试
（本轮新增测试中唯一一处涉及多分支互斥优先级的场景）——**它做对了**：用拼接的真实错误串
显式验证「即便 region 判据命中，仍不能抢走更优先的 429/冷却/风控分支」，测的正是分支间顺序而
非分支内部逻辑。未在本轮新增代码中发现「回退测试仍全绿而修复无效」的同类隐患，但审计范围
所限（时间与并发树变动），**不代表全仓没有**，只能确认抽查到的这一处是安全的。

---

## 5. 提交前必须先修的项（按严重度排）

### 🔴 P0 — 会让 `tsc` 编译失败，必须先修

**画布视图缺 `Move` 图标导入**：`admin-ui/src/components/dashboard.tsx:2` 的 import 列表补
`Move`（从 `lucide-react`）。这是当前唯一一条真实编译错误，不修的话 CI 的 tsc 门禁必挂。

### 🟠 P1 — 用户可见的裸键名，且已挂载在主览页（默认打开即可见）

**GlowGrid 四个 i18n key**：在三语 JSON 的 `overviewpage` 段补 `grid.collapse` /
`grid.showMore`（带 `{{n}}` 插值）/ `grid.hiddenAbnormal`（带 `{{n}}` 插值）/
`legend.groupHue`，三语各补一次。这是主览页默认渲染路径上的可见文案缺失，比画布视图的
优先级更高（画布视图至少不影响现有视图，这个是主页面的显示缺陷）。

### 🟡 P2 — 决定画布视图这个功能要不要发

在修完 P0 让代码能编译之后，还要决定：**画布视图这次要不要一起发**。如果要发，还差最后一跳
（在 `dashboard.tsx` 渲染主体加第三个分支，`credentialView === 'canvas'` 时渲染
`<CredentialCanvas>`，并把对应的画布视图专属 i18n key 补进三语 JSON）；如果暂不发，
建议把切换按钮那一档（`{ v: 'canvas' as const, ... }`）连同 import 一起先删掉或用 feature flag
挡住，避免用户点了没反应。**不建议**只修 P0 让它编译过就直接提交——那样按钮能点但点了没用，
体验上比没有这个按钮更差。

### 🟡 P3 — 文档去重（不影响运行，影响下一轮协作效率）

11 份 HANDOFF/PLAN/STATUS/TRACKING md 里已有多条确认过期的结论（本报告 §3 点出的 K9、
分身 M2 两条），建议提交前更新 `TRACKING-2026-08-06.md` 对应行的状态，避免下一个 agent
或人工按过期清单重复派单——`TRACKING.md` 自己就写过「新开文件正是清单越读越长的成因」，
目前 11 份历史文档已经处在这个状态。这条不阻塞提交，但建议在合并前后各做一次。

### ℹ️ 其余（无需处理，仅记录）

- `cargo check`/`clippy` 的 227 条 warning 全部是本轮之前就存在的历史 dead-code / 文档格式类，
  非本轮引入，不阻塞提交。
- `src/openai/convert.rs` 在审计窗口内变为新的未提交改动（见 §0），**本报告未覆盖其内容**，
  需要单独复核。
- `src/anthropic/stream.rs` 在审计期间仍在被改动，本报告对它的判定（A8）基于审计开始时读到的
  版本，建议提交前重新读一次确认没有回退。
