# 任务：凭据画布视图 + IP 池闭环 + kiro_shield 完整内建

> 给执行 AI 的任务书。**先读第 0 节，它能省掉你一半的工作量** —— 本仓大量"待做"其实已落地，
> 重做的代价不是浪费时间，而是与既有实现冲突。
> 行号取自 2026-08-06 工作树（有其它会话的未提交改动），**每处改动前重新 grep 定位**。

## 阅读顺序（不要跳）

1. `OPEN-ISSUES-2026-08-06.md` —— 唯一经过源码复核的问题清单。§一 是"文档说待修实际已闭合"。
2. `TRACKING-2026-08-06.md` §A —— **有 8 个 agent 正在并行改代码**。开工前先跑
   `git status --porcelain | wc -l` 和 `git diff --stat`，确认你要改的文件没有别人在改。
   特别是 agent F 在做 M4（吸收层三条），与本文任务 C 直接重叠。
3. `docs/absorb-layer-design.md` —— 吸收层的**已实现**设计（行号级 + 突变验证过的守卫测试）。
4. `docs/shield-ui-plan.md` —— shield 前端与 41 键三语 JSON 全文，§4 是 shield_stats_url 完整方案。
5. `docs/clone-page-impl-plan.md` —— 分身页与 SOCKS 节点的实施清单，§3 有 SSRF 与密码三态的坑。
6. `CLAUDE.md`（项目根 + `~/CLAUDE.md`）—— 硬约束与实测事故记录。

---

## 0. 已落地，不要重做（本节是本文最值钱的部分）

开工前逐条 grep 验证，**不要相信旧 HANDOFF 说的"待修"**。

| 项 | 状态 | 证据 |
|---|---|---|
| 吸收层（内建 shield 核心） | ✅ **已完整落地** | `provider.rs` 含 `absorb` 145 处；`handlers.rs:867-918` `AbsorbClass`/`absorb_class_of`；`recovery_metrics.rs:110-118` 四个计数器；`admin/types.rs:891-896`+`:1023-1028`；`settings-page.tsx` 36 处；`ops-page.tsx:133-136` 四个 metric |
| 已知问题 #1–#22 | ✅ 全部已修 | `OPEN-ISSUES` §一 逐条核实 |
| `cloneGroup` / `cloneSeq` | ✅ 已上 wire | `types/api.ts:74-78` |
| 凭据多开 `copies` | ✅ 已有 | `service.rs:103` `effective_copies`，`MAX_CREDENTIAL_COPIES=16` |
| SOCKS 节点 CRUD + 批量导入 | ✅ 四端点齐 | `router.rs:175-181`；前端 `credentials.ts:590` `upsertSocksNode` |
| SOCKS 节点**编辑** | ✅ 已修（M5 已闭合） | `clone-management-card.tsx:298` `saveEdit` + `:310` `buildSocksNodeEditPayload` |
| 行视图 | ✅ 已有 | `credential-row.tsx`（1028 行） |
| 行视图的「出口 IP ▸」子菜单 | ✅ 已有 | `credential-row.tsx:777-805` |
| 代理链接解析 | ✅ 已有 | `lib/proxy-line-parse.ts`（515 行）+ 测试 |
| FLIP 动画 | ✅ 已有 | `hooks/use-flip.ts` |
| 可视化零件 | ✅ 已有 | `components/overview/`：GlowGrid / OrbitRing / StatusBars / StatusHeatmap / Sparkline / RadialGauge / FireCanvas / `credViz.tsx`（健康三态口径） |

**推论：你的任务不是"实现吸收层"，而是补它与 shield 的差集（任务 C）+ 两个真正缺的东西（任务 A/B）。**

---

## 1. 三个真实缺口（本文全部任务的根因）

开工前先把这三条看懂，否则会做出"精美但结论错误"的 UI。

### 缺口 1 — 凭据与节点之间没有 id 级绑定 🔴 任务 B 的硬前置

`SocksNode.boundCredentials` 是按 `凭据.proxyUrl == 节点.url` **字符串比对**算的，
`types/api.ts:964-974` 自己标注为「启发式」，并说明"手工填过代理的号可能因 scheme 未归一而漏算"。
后端 `kiro/model/socks_node.rs:70-78` 更直接写着「**将来若把节点 id 写进凭据**，绑定关系会静默错位」
—— 即这件事**还没做**。已 grep 确认：全仓 `proxy_node_id` / `proxyNodeId` **零命中**。

后果：改节点 URL 或删节点，已绑的分身静默指向失效出口，面板上看不出来。
**任何「在 IP 池里选 IP」「分身切换 IP」的 UI，画的都是一条猜出来的线。**

### 缺口 2 — 节点池没有自动健康探测

`last_test` 只由手工 `POST /socks/nodes/{id}/test` 写入。已 grep 确认 `main.rs` 里**没有任何**
socks 相关后台任务。于是死掉的出口保持 `enabled`，继续被分配给新分身。
凭据侧有 `health.rs`（AIMD + EWMA）、`cooldown.rs`（8 种冷却原因）—— **节点侧一个都没有**。

### 缺口 3 — 出口 IP 不随请求记录

`exit_ip` 只存在于代理测活结果里（`socks_node.rs:53`、`handlers.rs:281`）。
所以面板回答不了恰恰是 IP 池核心目的的两个问题：**这个号实际从哪个 IP 出去**、
**有没有两个号共用一个出口**。做 IP 池而不记这个，等于装了仪表盘没接传感器。

### 附带缺口（任务 A 会碰到）

- **凭据管理页没有搜索/筛选**，固定 `itemsPerPage = 12`（`dashboard.tsx:181`）。
- **跨页选择陷阱**：`selectedIds` 跨页保留（`dashboard.tsx:146`），但每页只显示 12 个。
  第 1 页勾 5 个 → 翻到第 2 页点批量删除 → 删掉你**当前看不见**的那 5 个。这是现成的误操作陷阱。
- 选择交互只有勾选框 + `Ctrl/Cmd+点击`（`credential-card.tsx:436` 明写「普通左键**不选中**」）。
  **没有** shift 区间选、**没有**框选、**没有**键盘选择。

---

# 任务 A — 画布视图（与卡片/行视图**并存**的第三种视图）

用户原话：**"画布是并存卡片/行视图，然后可以鼠标在上面框选进行多选"**。
所以这是**新增第三档**，不是改造现有两档。现有两档的行为一个字节都不要动。

## A1. 视图开关（4 处，anchor 已核实）

| 改哪 | file:line | 怎么改 |
|---|---|---|
| 类型 | `hooks/use-ui-layout-prefs.ts:16` | `export type CredentialView = 'card' \| 'row' \| 'canvas'` |
| 默认值 | 同文件 `:33` `DEFAULTS` | `credentialView: 'card'` **保持不变**（新视图不能改默认） |
| 注释 | 同文件 `:8-10` | 那段注释明确否决过"拖拽固定位置"，理由是"会被自动排序/轮询冲掉"。**必须在注释里补一句说明画布如何绕开这个否决**（见 A2），否则下一个人会以为画布违反了既有决定 |
| 切换 UI | `settings-page.tsx` 的排版偏好卡 | 加第三档；`dashboard.tsx` 按 `credentialView === 'canvas'` 分支渲染 |

`use-ui-layout-prefs.ts` 用的是 `useSyncExternalStore` + 自定义事件跨组件同步（`:38` `EVENT`），
且 `getSnapshot` 靠 JSON 字符串比对缓存引用（`:54-60`）—— **照这个范式扩展，不要新开 localStorage 键**，
`{...DEFAULTS, ...parsed}`（`:46`）会让旧数据自动补默认值。

## A2. 承重设计：位置编码身份，外观编码状态

`use-ui-layout-prefs.ts:8-10` 那段注释否决拖拽固定位置的理由是对的（会被轮询重排冲掉）。
画布**必须**绕开它，做法是把空间身份与空间排序拆开：

1. **槽位 = 身份的纯函数**：`slotOf(cred) = (cloneGroup ?? apiKeyHash ?? \`id:${id}\`, cloneSeq ?? 0, id)`
   排序后取下标。只在**池成员变化**时改变，轮询（每几秒）不再重排任何东西。
   ⚠️ 分组键**禁止**把 `cloneGroup` 与 `apiKeyHash` 字符串 join —— 前者带 `acct:` 前缀、后者裸 hex
   （`clone-page-impl-plan.md:140` 明写这条）。
2. **外观 = 状态的函数**：色相 = 绑定出口节点、描边环 = 健康三态、脉冲频率 = 在途负载。
   健康三态**必须复用** `overview/credViz.tsx` 的 `Health` 类型与派生函数（它与 `StatusHeatmap` 同口径），
   不要再写一套判据。呼吸/闪动/涟漪语法照 `GlowGrid.tsx` 抄。
3. **排序是一次显式过渡**：用户点「按健康排序」才触发 FLIP 重排（`hooks/use-flip.ts` 已就位），
   不在后台悄悄换位。

## A3. 框选（用户明确要的核心交互）

在画布容器上做，**不要引入 react-flow**（多一个大依赖，而出口与分身是多对一属性关系，
不是需要自由布线的图；CLAUDE.md 禁止在有现成方案时引新库）。

```
onPointerDown(空白处) → 记起点 + setPointerCapture
onPointerMove          → 画选框（绝对定位 div），rAF 节流
onPointerUp            → 命中测试 → 提交选区 → releasePointerCapture
```

铁律五条：

1. **命中测试用槽位几何算，不要读 DOM**。槽位是 `slotOf` 的纯函数 → 每个 cell 的
   `(x, y, w, h)` 可直接算出，与选框矩形求交即可。用 `getBoundingClientRect()` 逐个测在
   几百个 cell 上会掉帧。
2. **必须用 Pointer Events + `setPointerCapture`**，不要 mouse 事件。指针拖到容器外再松开时，
   mouseup 会丢，选框永久卡住（这是框选最常见的 bug）。
3. **拖拽阈值 4px**：小于阈值当点击处理，否则单击会被误判成空框选而清空选区。
4. **修饰键**：裸拖 = 替换选区；`Shift+拖` = 追加；`Alt+拖` = 减选。
   与桌面文件管理器一致（**不要**用 react-flow 的 Shift=框选约定，本仓用户的先验来自 Finder/Explorer）。
5. **`user-select: none` 只在拖拽期间加**，常驻会让 cell 上的文本无法复制。

## A4. 选择代数（补齐当前缺失的三种）

现在只有勾选框 + `Ctrl/Cmd+点击`。三档视图**共用同一套语义**（见 A5 的共享 store）：

| 操作 | 行为 | 现状 |
|---|---|---|
| 左键点 | 单选（替换选区） | ⚠️ 画布新增；卡片视图**保持不选中**不动 |
| Cmd/Ctrl + 点击 | 加/减选 | ✅ 已有 |
| **Shift + 点击** | 沿**槽位顺序**区间选 | ❌ 缺失，必须加 |
| **空白拖拽** | 框选（A3） | ❌ 缺失，本任务核心 |
| 右键 | 命中项在选区内 → 菜单作用于**整个选区**；否则先单选再弹菜单 | ⚠️ 现在恒作用于单个（`credential-card.tsx:445`） |
| Cmd/Ctrl + A | 全选**当前筛选结果** | ❌ 缺失 |
| 方向键 + 空格 | 键盘选择 | ❌ 缺失（可访问性） |
| Esc | 清空 | ❌ 缺失（`deselectAll` 有但无快捷键） |

区间选的锚点必须存 `lastAnchorId`；`disabled` 的号是否参与区间选要与 Polaris 一致
（**排除 disabled**），否则 shift 一拖就把禁用号选进批量启用里。

## A5. 🔴 先做这个：选区状态提出 `dashboard.tsx`

`selectedIds` 现在是 `dashboard.tsx:146` 的局部 state，所以分身视图、运维视图**无法对
"当前选中的号"做任何事**。这是"选中 + 更多操作方法"最大的结构性阻塞，也是任务 B
（在 IP 池里批量指派出口）的前提。

做一个 `hooks/use-credential-selection.ts`，**照 `use-ui-layout-prefs.ts` 的
`useSyncExternalStore` 范式**（不要引 zustand/jotai）。但与它有一点关键不同：
选区是**会话态不是偏好**，所以 **不要写 localStorage** —— 存了会让用户下次打开面板时
带着上次的选区去点批量删除。用模块级变量 + 同一套 `EVENT` 广播即可。

暴露：`ids: ReadonlySet<number>` / `toggle(id)` / `selectOnly(id)` / `selectRange(anchor, to, order)` /
`addMany(ids)` / `removeMany(ids)` / `clear()` / `lastAnchorId`。

**迁移必须一次做完**：`dashboard.tsx` 的 6 组批量处理器（`:301` 删除 / `:349` 重置 / `:397` 启停 /
`:442` 白名单 / `:477` 刷新 / `:523`+`:547` 导出）全部改读 store。留一半会出现两个选区真相源。

## A6. 顺带修掉跨页选择陷阱（本任务内必须做）

两条路，**推荐第一条**：

1. **画布档取消分页、改虚拟滚动**（`@tanstack/react-virtual`，仓里已有 `@tanstack/react-query` 同族）。
   选区永远可见，陷阱自然消失。虚拟化对画布是刚需（`copies` 上限 16 × N 个号，几百 cell 是常态）。
   ⚠️ 顺带好处：`FireCanvas` 的 WebGL 上下文上限问题（`FireCanvas.tsx:3-9` 注释说明只在饱和号挂载）
   在虚拟化后更安全。
2. 若保留分页：必须显示「已选 37 项（其中 25 项不在当前页）」+ 一个「仅保留本页」出口。

**卡片/行视图的分页行为不要动**（用户要的是并存，不是改造）。

## A7. 两个低成本高回报项（用户要的"更人性化"）

**批量操作先预演再执行。** 现在是先提交、后在 toast 里说跳过了几个
（`dashboard.batchDelete.skipHint` / `skippedResult` 等键已存在）。信息是现成的，只是**位置错了**
—— 用户需要它在决策前。改成 `ConfirmDialog` 里先列「将执行 N 项 / 跳过 M 项 + 逐条原因」。

**撤销栈。** 每个批量操作都有天然逆操作（`disabled↔enabled`、`priority` 设回原值、
`proxy`/`tag`/`name` 设回原值），删除本来就进回收站。做客户端逆操作栈 +
「已禁用 12 个号 · 撤销」toast。这是本轮性价比最高的单项。
⚠️ 撤销**只做可逆操作**，删除不进撤销栈（它已有回收站，两套恢复机制会互相打脸）。

---

# 任务 B — IP 池闭环（凭据在池里选 IP / 分身切换 IP）

用户原话：**"我需要IP池子，就是凭证可以在IP池子选择IP，然后凭据分身切换IP也许可以在里面选"**。

## B0. 现状精确盘点（先看清什么已有、什么真缺）

| 能力 | 状态 | 位置 |
|---|---|---|
| 节点池 CRUD + 批量导入 + 测活 | ✅ 已有 | `router.rs:175-181` |
| 节点池前端编辑（含密码三态） | ✅ 已有 | `clone-management-card.tsx:275-330` |
| **行视图**的「出口 IP ▸」选择器 | ✅ 已有 | `credential-row.tsx:777-805`，调 `applyProxy(n.url)` |
| **卡片视图**的节点池下拉 | ❌ **缺失** | `credential-card.tsx:111-199` **只有手填输入框**（`proxyValue`/`proxyUser`/`proxyPass`） |
| **分身面板**的换 IP 入口 | ❌ **缺失** | `CloneGroupsPanel`（`clone-management-card.tsx:732`）grep `bindNode\|applyProxy\|setProxy` **零命中** |
| 批量指派出口 | ❌ 缺失 | `dashboard.tsx` 6 组批量里没有 proxy |
| id 级绑定 | ❌ **缺失（缺口 1）** | 全仓 `proxy_node_id` 零命中 |
| 节点自动健康探测 | ❌ **缺失（缺口 2）** | `main.rs` 无 socks 后台任务 |
| 逐凭据出口 IP | ❌ **缺失（缺口 3）** | `exit_ip` 只在测活结果 |

**所以"分身切换 IP"这个入口确实不存在 —— 用户的判断是准的。**

## B1. 🔴 `proxy_node_id` 权威绑定（**用户已拍板要做**，一切的前置，5 跳管线）

照 `clone-page-impl-plan.md` §2 的 5 跳范式做（那份已被验证过一次，**照抄它的结构**）：

| 跳 | file:line | patch |
|---|---|---|
| 1 | `kiro/model/credentials.rs:229`（`proxy_password` 之后） | `#[serde(default, skip_serializing_if = "Option::is_none")] pub proxy_node_id: Option<u64>,` |
| 2 | `token_manager.rs` 的 `CredentialEntrySnapshot`（`rpm` 一带，grep `pub rpm: u32`） | 加 `pub proxy_node_id: Option<u64>` + 构造处搬运 |
| 3 | `admin/types.rs` 的 `CredentialStatusItem`（grep `pub rpm: u32`） | 同上 |
| 4 | `admin/service.rs` 的逐字段搬运（grep `rpm: entry.rpm,`） | `proxy_node_id: entry.proxy_node_id,` |
| 5 | `admin-ui/src/types/api.ts:65`（`rpm?: number` 之后） | `proxyNodeId?: number` |

**语义（必须写进 doc 注释，否则下一个人会搞反）**：
- `proxy_node_id` = **权威绑定**（用户选了哪个节点）
- `proxy_url` = **解析结果缓存**（该节点当前的 URL，热路径直接读它，不必每次查表）
- 两者关系：`Some(node_id)` 时 `proxy_url` 由节点表派生；手填代理时 `proxy_node_id = None` 而 `proxy_url` 有值。
  **这两种状态必须能区分** —— 否则"手填"和"绑节点"在 UI 上长得一样，而改节点 URL 只应影响后者。

**收益（缺口 1 的三个后果同时消失）**：
1. 改节点 URL → 遍历绑该 id 的凭据同步 `proxy_url`（现在会静默失效）
2. 删节点 → 能精确告知"这 N 个号将回落直连"，而不是猜
3. `boundCredentials` 从启发式变精确（`types/api.ts:964` 那段"启发式"注释可以删掉）

**兼容铁律**：全仓无 `deny_unknown_fields`，所以旧二进制读新文件不 exit(1)。但**旧版一次
`persist_credentials()` 会抹掉该键** → 回滚一次即失去绑定（能由 `proxy_url` 字符串比对自愈到
"大概正确"，但不精确）。**在 CHANGELOG 明写这条**，与 `clone_group` 同款处理，不做回填 reconcile。

## B2. 节点健康探测（缺口 2）

新增后台任务，**照 `refresh_loop.rs` 的受管任务范式**（TIER2 热重载：abort + respawn）。

- 探测复用 `run_proxy_probe`（`clone-page-impl-plan.md:196` 说明要把 `handlers.rs:225-306`
  的探测体抽成 `pub(crate) async fn` 供两处共用 —— **先确认这个抽取是否已做**，没做就先做，
  复制那 80 行会让 `PROXY_TEST_PROBE_URL` 分叉）。
- 健康模型**复用凭据侧的词汇**：EWMA 延迟、连续失败数、冷却。UI 上节点与凭据用同一套健康语言，
  用户只需学一次。**不要**为节点新造一套状态机。
- 探测失败的节点自动**退出分配候选**（`resolve_node_plan`，`service.rs:4842` 一带）。
  ⚠️ 注意 `service.rs:4830` 已有一条既存规则：「已知不通的出口分出去只会让那一份必然失败，
  **显式 `node_ids` 不受此限**」——自动探测的结果要接进同一个判据，且**保持显式指定的豁免**。
- 配置项：`socksNodeProbeEnabled`（默认 **false**，理由同吸收层：默认不改变现有行为）+
  `socksNodeProbeIntervalSecs`（默认 300）。**必须 `#[serde(default)]`** ——
  线上 config.json 是既有文件，缺 default 会导致**加载失败 → 服务起不来**（这是硬约束，不是建议）。

## B3. 逐凭据出口 IP（缺口 3）

最小可做：探测成功时把 `exit_ip` 写回节点的 `last_test`（**已有**），
并在凭据侧记 **last-known 出口 IP**（新字段 or 由 `proxy_node_id` join 节点表现算）。

**推荐现算，不加字段**：`proxy_node_id` → 节点 → `last_test.exit_ip`。理由是出口 IP 会变
（住宅代理常轮换），存进凭据就有了第二份真相，而 join 出来的永远是最新探测值。

**跨账号出口碰撞检测**：按 `exit_ip` 分组，一个 IP 上挂了 >1 个**不同账号**（用
`account_key` 口径，不是 `id`）即告警。`docs/CACHE-RESEARCH.md` 已把跨账号共享列为威胁模型，
这是同一件事的另一面。

## B4. UI 入口（用户真正要的那部分）

### B4a. 独立「IP 池」视图（从 `clone-management-card` 拆出）

`clone-management-card.tsx`（1323 行）里 `SocksNodesPanel`（`:188`）与 `CloneGroupsPanel`（`:732`）
**互不相关**，先拆成两个文件。CLAUDE.md 的「函数 ≤30 行 / 嵌套 ≤3 层」在这个文件里已经不成立。
拆分是纯机械操作，**先拆再加功能**，否则改动风险主要来自文件本身而非需求。

IP 池视图布局（**二部图显式化** —— 这是当前"两个平铺列表"最大的问题）：

```
┌─ 出口节点池（左）──────┐   ┌─ 已绑凭据（右，随左侧选中联动）────┐
│ ● US-W-1  42ms  3 号   │   │ #103 主份    #107 分身#2          │
│ ● JP-1    88ms  1 号   │   │ #112 分身#3                       │
│ ○ SG-2    死    0 号   │   │                                   │
│ ● (未绑) …             │   │ [从选区指派到此节点]              │
└────────────────────────┘   └───────────────────────────────────┘
```

### B4b. 两条绑定路径，都要有

- **拖拽**：从节点池拖一个出口到凭据 cell（画布）或行 = 绑定。可发现性好。
- **选中 + 指派**：框选 N 个凭据 → 点节点「指派到此节点」= 批量绑定。批量时远快于拖拽。
  这条**依赖任务 A5 的共享选区 store**。

### B4c. 卡片视图补节点池下拉

`credential-card.tsx:111-199` 现在只有手填三个输入框。补一个下拉，
**直接复用 `credential-row.tsx:777-805` 的「出口 IP ▸」逻辑**（全局 / 直连 / 池节点列表 / 空态四种情形都已写好），
抽成共享组件 `components/exit-node-picker.tsx` 给三档视图共用。

### B4d. 分身面板补换 IP 入口（用户点名要的）

`CloneGroupsPanel` 的成员格上加出口 chip（同色 = 同出口，**撞色即撞出口，不读文字就能看见风险**），
点 chip 打开 `exit-node-picker`。

### 🔴 只有一个徽标：出口被复用

- **出口被复用**（组内两份走同一节点）→ 告警。多开的意义就是不同出口，撞了等于白多开。

### ⛔ 不要给"主份直连"加告警徽标（用户已拍板，2026-08-06）

**主凭据不需要代理，这是刻意设计，不是缺陷。** 已核实两条路的默认值**刻意相反**：

| 路径 | "主份"是谁 | `assign_primary_node` 缺省 | 依据 |
|---|---|---|---|
| `POST /credentials` + `copies` | 用户**亲手提交**的那一条 | `copies == 1` ⇒ 多开时 **false** | `service.rs:1800-1814`：它的出口由表单里的「出口 IP」决定，池分配不该越过用户的选择；池节点全让给第 2..N 份，`copies=N` 只需 **N-1** 个节点 |
| `POST /credentials/{id}/clone` | 本次新建的**第 1 个分身** | **true** | `types.rs:637-645`：父号一字节不动，N 份全新建、彼此同质，没理由独独让它裸连 |

`types.rs:635` 记的那个「主份裸连 + 两个节点闲置」事故**只属于 clone 那条路**，
且**已于 2026-08-05 修掉**。把它当成通用规则会让**每一个正常的多开组都常亮一个假告警** ——
而假告警会训练用户忽略真告警。

**正确做法**：主份的出口 chip 显示为中性的「直连」态（与"绑了节点"视觉可区分即可），
**不带 ⚠**。只有 clone 路径产出的分身裸连、且池里确实有空闲启用节点时才提示。

## B5. ⚠️ 写入路径的三个坑（踩了就是静默故障）

1. **`SetProxyRequest` 发 snake_case**（`handlers.rs:510-519` **无 `rename_all`**，是全仓少数例外）。
   前端 `credentials.ts:175` 的 `setCredentialProxy` 已经发 `proxy_url`/`proxy_username`/`proxy_password`。
   **不要"顺手统一"成 camelCase**，也不要自己写 `api.post` —— 发 camelCase 会让 `proxy_url` 为空，
   静默变成**"清除代理"**（回退全局）。
2. **密码三态**：省略 = 不改 / `Some("")` = 清空 / `Some(v)` = 设置。
   `clone-page-impl-plan.md:178-182` 有完整说明与两条承重测试名。改节点名时若为"补齐字段"回填空串，
   会把密码抹掉、**已绑该节点的分身全部掉线**。
3. **SSRF 用 `AdminConfigured` 不是 `Strict`**：`Strict` 会拒掉 `198.18.0.0/15`，
   而国内 Clash/Surge 的 fake-IP 池正在该段（已知问题 #19，`ssrf.rs:36`）。
   ⚠️ 另外 `clone-page-impl-plan.md:190-191` 指出 `POST /proxy/test` **当前完全没校验代理侧**
   → 拿到 adminKey 即可用 `latencyMs`/`error` 做内网端口扫描。**本任务顺带修它**（加 `validate_proxy_address`）。

## B6. 🔴 出口轮换与 prompt cache 直接冲突（必须写进 UI 文案）

`affinity.rs` 的会话亲和（session_id → credential_id）是为保住 prompt cache 服务的。
CLAUDE.md 已有实测结论：`rateLimitEnabled` 关掉的原因正是它的最小间隔会在 **241ms** 处踢开亲和绑定
导致缓存全丢；同一份记录给出**速率与 429 率相关性仅 +0.09**。

推论：**换出口 IP 大概率救不了 429，但一定会打断亲和**。所以：

- 轮换必须**每号显式开启**，默认粘滞
- **不要**做"429 就换 IP"的全局策略 —— 在这个系统里很可能是净亏
- 这条要写进配置项的 hint 里（照 `shield-ui-plan.md:108` 的 `suspended.hint` 那种"三段论说清风险"的写法），
  风险要落在用户看得见的地方，不是只写在文档里

---

# 任务 C — kiro_shield 完整内建（可开关，不再需要外置脚本）

用户原话：**"需要计划把 kiro shield 所有功能加进 kirostudio，可以开关，这样就不用外置脚本了，逻辑需要完美"**。

## C0. 🔴 先读这条：核心已经做完了

吸收层**已完整落地**（见 §0）。所以本任务**不是实现吸收层**，而是补完差集。
如果你开始写 `'absorb: loop`，说明你走错了 —— 它已经在 `provider.rs` 里。

**开工前必做**：`TRACKING-2026-08-06.md:25` 显示 **agent F 正在做「M4 吸收层三条」**。
先跑 `git diff --stat src/kiro/provider.rs src/anthropic/handlers.rs` 看它改了什么，
`OPEN-ISSUES-2026-08-06.md:85` 说这三条「默认关，手动开之前必须修」，依据在
`docs/absorb-layer-design.md`。**与 agent F 冲突的部分不要动**，等它产出后再叠。

## C1. shield 功能 × KiroStudio 现状 逐项差集表

shield 参数来自 `TASK-BUILTIN-RETRY.md:39-53` 与 CLAUDE.md（均为实测记录，非猜测）。

| # | shield 的能力 | KiroStudio 现状 | 结论 |
|---|---|---|---|
| 1 | 重试 **429** | ✅ `AbsorbClass::PoolCooldown` + `UpstreamRateLimit` | 已有 |
| 2 | 重试 **503** | ⚠️ **部分**：带 `retry_after_secs=` 的会被 `PoolCooldown` 抓到；**裸 503 不会** | 需确认 |
| 3 | 重试 **500 / 502 / 504** | ❌ **缺失** —— `absorb_class_of`（`handlers.rs:890-918`）**无 5xx 分支** | **C2 要做** |
| 4 | 不重试 4xx | ✅ 已有（`handlers.rs:915-917` 注释明写） | 已有 |
| 5 | 总预算 600s | ✅ 45s（`upstreamRetryAbsorbBudgetSecs`） | **刻意更小**，见 C6 |
| 6 | `MAX_ATTEMPTS=60` | ✅ `maxRounds=3` | **刻意更小**，见 C6 |
| 7 | `MIN_DELAY=1.0` | ✅ 150ms | **刻意更优**（shield 会把 50ms 恢复睡满 1s） |
| 8 | Retry-After clamp[1,15] | ✅ `minDelayMs`/`maxDelaySecs` | 已有且更细 |
| 9 | 退避 `1.0*1.7^(n-1)` | ✅ **更优**：用号池进程内真实恢复秒数（`cooldown.rs`），无需 HTTP 头往返 | 已有 |
| 10 | 预算耗尽返 **503** | ✅ **已实现**：`upstreamRetryAbsorbExhaustedStatus`（默认 429，置 503 生效） | **不要删**，见 C7 |
| 11 | 统计计数器 8 个 | ⚠️ 有 4 个，**规格要 7 个** —— `absorb_rounds_1/2/3plus` **从未实现**（已 grep 确认零命中） | **C3 要做** |
| 12 | `/_shield/stats` 端点 | ❌ `shield_stats_url` **全仓零命中** | **C4 要做** |
| 13 | 整请求重打 | ✅ **刻意不同**：我们是号级换号（`failover`），放大 36 次 vs shield 的 720 次 | 已有且更优 |
| 14 | per-route（Caddy 4 条） | ✅ 内建后天然覆盖四条 handler 路径 | 已有 |

**结论：真正要做的只有 C2 / C3 / C4 三项。** 其余 11 项或已有、或刻意不同（有实测依据）。

## C2. 5xx 吸收（唯一的功能缺口）

`absorb_class_of`（`handlers.rs:890`）目前只认 429 族与 403。shield 的 `RETRYABLE`
含 `500/502/504`，实测 `server_error` 占 0.8%（`TASK-BUILTIN-RETRY.md:195` 的 2h 基线）。

**做法**：加 `AbsorbClass::UpstreamServerError` 分支。四条铁律：

1. **必须新写窄谓词，不要放宽既有的**。`handlers.rs:548-553`（`is_upstream_temporarily_suspended`
   的注释）明写：泛匹配 `AccessDeniedException` 会把永久封号吞成可重试。同理，
   5xx 的谓词**不能**是"包含 5"这种形状。
   ⚠️ 另注意 `OPEN-ISSUES` **H2** 正在处理「397 次 `AccessDeniedException` 落兜底 502」——
   那些 502 **不是真的上游 5xx**，是我们自己映射错的。**若 H2 未修完，先别做 C2**，
   否则你会把 region 错配型 403 当成瞬态 5xx 反复重试（放大一个必然失败）。
2. **分支顺序**：放在 `is_upstream_temporarily_suspended` **之后**（403 优先级更高，
   它有专门的默认关策略）。`absorb_class_of` 的顺序是承重的，`handlers.rs:879-889`
   的注释已列出两条既有顺序依赖，**你的新分支不能破坏它们**。
3. **默认关**：新增 `upstreamRetryAbsorbServerError`（默认 `false`）。
   理由：`handlers.rs:1100` 记录了实测分布「`server_error` 296 个 0 次重试、34 个 1 次」——
   量小，且 5xx 可能是上游真故障（重试只是放大）。**要有开关才能做对照实验**。
4. **`#[serde(default)]` 必须加**（线上 config.json 是既有文件，缺 default → 加载失败 → 服务起不来）。

**回退即 FAIL 测试**：喂一个真实的上游 502 错误串 → `absorb_class_of` 返
`Some(UpstreamServerError)`；喂 H2 那个 `AccessDeniedException` 串 → 必须 `None`。
删掉窄谓词改成泛匹配 → 第二条断言 FAIL。

## C3. 补齐 rounds 分布计数器（3 个）

`recovery_metrics.rs:115-118` 只有 4 个。规格（`SPEC-shield-builtin.md:358-361`）要的
`absorb_rounds_1` / `absorb_rounds_2` / `absorb_rounds_3plus` **从未实现**。

用途：判断预算够不够用。只有总轮次没有分布时，`absorbRounds=100` 无法区分
"100 个请求各 1 轮"（健康）与"33 个请求各 3 轮"（预算接近不够）。

- 后端：`recovery_metrics.rs` 的 `counters!` 宏末尾追加 3 项（宏自动出 camelCase 并进
  `/api/admin/recovery-metrics`，`handlers.rs:689` 无需改）。
  bump 点在 `provider.rs` 吸收成功分支（现在 bump `absorb_recovered` 的那处），按 `absorb_round` 分档。
- 前端：`api/ops.ts` 的 `RecoveryMetrics` 加 3 个 **`?: number`**（必须可选，否则旧后端缺字段时类型说谎）；
  `ops-page.tsx:136` 后追加 3 项 `METRIC_ITEMS`，只有 `absorbRounds3plus` 带 `warn: true`。
- **⚠️ 顺带修一处既有缺陷**：`shield-ui-plan.md:48-50` 指出 `ops-page.tsx` 的
  `const v = data[it.key] as number` 对可选字段在旧后端下拿到 `undefined` →
  `Math.round(undefined)` → 界面显示 **NaN**。改成 `(data[it.key] as number | undefined) ?? 0`。
  **先 grep 确认这条是否已修**（现存 `reclaimedInvokeCalls`/`stray*` 也是可选字段，同样受影响）。
- i18n：`opspage.metric.absorbRounds1/2/3plus` 三语，**`shield-ui-plan.md:69-75`（zh）
  `:118-124`（en）`:167-173`（ja）已有可直接粘贴的 JSON**，照抄不要重写。

## C4. `shield_stats_url`：过渡期把外挂统计也纳入面板

`shield-ui-plan.md` §4 已有**完整方案**（配置 / 端点 / 三态 / 优雅降级四档），照做即可。
关键三点复述（这三点写错就变成"多一个假故障"）：

1. **不能用 `ssrf::validate_outbound_url`** —— 连 `SsrfPolicy::AdminConfigured` 也只豁免
   `198.18.0.0/15`，`127.0.0.0/8` 依旧拦（`ssrf.rs:61` 一带），照抄必然**永远拒绝**。
   改用专用窄校验：scheme 限 http/https、host 解析后**必须环回**、禁重定向、2s 超时、
   经 `common::http_read::read_body_capped` 限 64KiB。
2. **端点恒返 200**，三态由 body 表达（`not_configured` / `unreachable` / `available`）。
   恒 200 是为了让前端不必把"没配"当错误弹 toast。
3. **优雅降级三档**：`not_configured` → **整卡不渲染**（`return null`，绝大多数用户没有外挂，
   不该看到空卡）；`unreachable` → 卡片 + warning callout；`available` → 7 个 StatCard + byStatus badge
   + **必须写副标题**说明"外置进程自身计数，与网关计数器独立"（两套口径不同：外挂是整请求重打、
   网关是号级换号，混看会得出错误结论）。

i18n 13 个 `opspage.shield.*` 键三语全文在 `shield-ui-plan.md:77-89 / :126-138 / :175-187`，照抄。

## C5. 退役五步（用户要的"不用外置脚本"的最后一公里）

`SPEC-shield-builtin.md:371-378` 已定，**每步可独立回滚，任一步不对就停在上一步**：

1. 内置版上线、`enabled=false`。取 `/_shield/stats` 与面板 429 作**基线**。
2. 面板开 `enabled=true`（热更不重启）。两层串联：网关先吸收、剩余漏给 shield。
   判据：`absorbRecovered` 增长且 shield 的 retries 下降。**网关 p50 明显上升 → 拨回 false**。
3. shield 降级为观察者：`MAX_ATTEMPTS` 60 → 2。观察 24h。
4. **Caddy 4 条路由逐条**从 `:8993` 切到 `:8990`，每切一条观察 1h，先切流量最小的。
   `caddy reload` 不是 restart，可秒级回切。
5. shield 进程 `systemctl disable --now`，**二进制与 unit 保留 ≥2 周**。

🔴 **绝对不要擅自停 shield 或改 Caddy 指向** —— 那是别人加的、正在保护生产的东西
（CLAUDE.md 与 `TASK-BUILTIN-RETRY.md:212` 都明写这条）。
改 VPS 上任何文件都要**先改 `~/Documents/WorkSpace/ws-vps` 仓库再 scp 并提交**。

## C6. 为什么内建版的参数比 shield 小得多（不要"对齐"回去）

这是最容易被误判成"内建版更弱"的地方。**实测依据**：

- shield 用 600s / 60 次换来的是 **p50 73.2s** 的客户端延迟，且
  **11.6 次重试才救回 1 个请求**（22448 请求 / 19226 重试 / 1657 吸收 / 325 放弃）。
  **调大预算买到的是延迟，不是成功率。**
- 内建版 45s / 3 轮：`absorb-layer-design.md:96-100` 实算单号池下 **2 轮共 20s**，
  放大上限 36 次上游调用（shield 是 60×12=720）。
- 且吸收**不重入准入闸门**（`acquire_admission` 在循环外，有源码守卫测试钉死），
  shield 的 "1500 次准入/分钟 vs 288 令牌" 那条塌陷链在内建版结构上不存在。

## C7. 明确不要做（都有证据，重做即回归）

- ✅ **「预算耗尽换 503」已落地，不要删。**
  🔴 本条原写「刻意不做」，**已过期且方向与用户决策相反**，2026-08-06 更正。

  **用户 2026-08-05 明确要求的就是这个能力**，原话：
  > 「把这个 Cursor 对 429 会立刻停止会话 改掉 改为 请求对 429 会进行重试 我们作为缓冲」

  依据（外挂 `kiro_shield.py` 原注释亦记同一条）：**Cursor 见 429 会掐会话，见 503 不会。**
  即同一个「网关已尽力重试但没成」的事实，用 429 表达让客户端直接放弃整个会话，
  用 503 表达让它自己退避重试。所以预算耗尽时返 503 是**刻意的**，不是疏漏。

  **代码现状**（改动前先 grep 这三个锚点，别按行号找）：
  - `handlers.rs` 的 `ABSORB_BUDGET_EXHAUSTED_MARKER` 常量 + `map_provider_error` 的
    **第一条**分支（必须第一条：这些错误串都还带着 `retry_after_secs=` /
    `USER_REQUEST_RATE_EXCEEDED` 等原始特征，排后面会被下游分支先接走 → 开关静默失效）。
  - `provider.rs` 的置位点：`absorb_gave_up_after_rounds && absorb.exhausted_as_503`。
    标记只在**真睡过退避、真重打过**时置位 —— 一次都没重试就改状态码是**说谎**。
  - 开关 `upstreamRetryAbsorbExhaustedStatus`，**默认仍 429**（语义正确的那个，
    且 Claude Code 对 429 退避正常）；503 是为 Cursor 一类客户端做的显式让步。

  **已知取舍（本条原先的顾虑，仍然成立）**：终态换 503 后，面板 429 计数确实看不到
  这部分真实限流。**弥补手段**：吸收层那组计数器（`recovery_metrics.rs` 的
  `absorb_budget_exhausted` / `absorb_backoff_truncated` / `absorb_retry_quota_exhausted` /
  `absorb_*_skipped` 等）+ 日志里的结构化 `absorb_stop=` 归因字段，
  两者合起来能还原「有多少请求被吸收层放弃、卡在哪个闸门」。
  ⇒ 可观测性由**吸收层计数器**承担，不再依赖终态状态码。
  改动前必看守卫测试 `absorb_stop_reasons_are_distinguishable_in_logs`
  （删任一 `absorb_stop = "..."` 字段即 FAIL）。
- **不把吸收循环移出 `call_api_with_retry`**。有源码守卫测试
  `admission_gate_is_outside_absorb_loop` 钉死顺序，且断言 `acquire_admission(` 全文恰 1 处。
- **不逐轮上报 AIMD**。四个上报点都有 `absorb_round == 0` 门，去掉任一个会触发
  `absorb-layer-design.md:133-142` 描述的**死锁第三条路径**（RPM 单调滑到 floor 锁死）。
- **不把链内去重集或 `attempts_used` 移进循环**（`:147-148` 称其为"第二条承重不变量"）。
- **不改 `emit_record` 位置**（挪进循环 → 一条客户端请求落 N 条失败记录、面板失败数被轮次乘倍）。
- **PR 说明必须写**：吸收层只压**客户端可见的 429**，**不减少打向上游的请求量**。
  否则会误判"开了吸收层但上游 429 没少 = 没效果"。

---

## 2. i18n（本仓唯一的完整性闸门）

三语扁平点分键，**各 1791 键**（2026-08-06 实测；旧文档说 1523 已过期，**改完自己数一遍**）：

```bash
for f in zh en ja; do printf "%s: " $f; python3 -c "import json;print(len(json.load(open('admin-ui/src/i18n/resources/$f.json'))))"; done
```

- 改完**三份必须仍等量**。这是本仓唯一的 i18n 完整性闸门。
- 新 UI 只开自己的命名空间，能复用的走 `lib/i18n-labels.ts`。
- 插入多组键时**必须自下而上插**，否则行号漂移（`shield-ui-plan.md:54` 的教训）。
- 已有可直接粘贴的三语 JSON：`docs/shield-ui-plan.md` §3（41 键）、
  `docs/clone-page-i18n.json`（51 键）。**照抄不要重写。**
- ⚠️ `ConfirmDialog` 的「取消/处理中…」是**硬编码中文**（`confirm-dialog.tsx:52,59`）。
  本任务大量用 `ConfirmDialog`（预演对话框、批量指派），**英日用户会看到中文**。
  单独一个 patch 修它，不要混在功能 commit 里。

## 3. 验证（每项改动都要过）

```bash
# ⚠️ rust-embed 是编译期嵌入 admin-ui/dist，缺 dist 时 cargo 报 E0599（不是友好报错）
cd admin-ui && pnpm install --frozen-lockfile && pnpm build && cd ..

# ⚠️ 一律加 --no-default-features：Cargo.toml 的 default=["native-tls"] 与出厂配置相反
cargo test --no-default-features --bin kirostudio      # 当前 1189 全绿
cargo clippy --no-default-features --bin kirostudio    # 0 error
cd admin-ui && npx tsc --noEmit
```

### 「回退即 FAIL」是唯一可接受的测试验证方式

**测试"通过"不等于它测到了东西。** 每条测试都要：把修复处改回旧行为 → 跑测试 →
**必须 FAILED** → 再还原。本仓已记录 **8 种"纸面测试"形态**，其中三种最容易踩：

1. 漏了 struct 级 `#[serde(rename_all)]` 导致测试"通过"但什么都没测。
2. **中文注释使字节偏移落在多字节字符中间** → `get(a..b)` 返 `None` → 回退成整段前缀 →
   断言恒真（`absorb-layer-design.md:178-183` 记录的真实事故）。切片前先
   `is_char_boundary`。
3. **测了分支内部，没测分支顺序**（`OPEN-ISSUES` §八 记录：改三处、四条测试、三次"回退即
   FAILED"全过，而修复无效）。`absorb_class_of` 的顺序就是承重的 —— 顺序测试必须显式构造。

### 本任务特有的验证点

| 任务 | 验证 |
|---|---|
| A | 框选：拖到容器外松开（验 pointer capture）；4px 阈值（单击不清空选区）；几百 cell 不掉帧 |
| A | 跨页陷阱：第 1 页选 5 个 → 翻页 → 批量操作的目标集必须与 UI 显示一致 |
| A | 三档视图切换后选区行为一致（共享 store 生效） |
| B | 改节点 URL → 绑该节点的凭据 `proxy_url` 同步；删节点 → 精确告知受影响凭据数 |
| B | 密码三态：省略/空串/有值三条路径各验一次（`omitted_password_keeps_existing` / `empty_password_clears`） |
| B | `setCredentialProxy` 发 **snake_case**（发错会静默变成"清除代理"，必须手工验一次抓包） |
| C | 造真实 502 → 确认 `UpstreamServerError` 被吸收；喂 H2 的 `AccessDeniedException` → 必须不吸收 |
| C | **四条客户端路径逐个验**：`/v1/messages`、`/v1/chat/completions`、`/v1/responses`、`/cc/v1/*` |
| C | 确认**流式响应已开始后不再重试**（这是正确性红线） |

## 4. 硬约束（违反造成真实损失，逐条来自实测事故）

1. 🔴 **工作树有其它会话的未提交改动**（当前 78 文件，且 **8 个 agent 正在并行改**）。
   禁止对真实 index 做 `checkout` / `switch` / `stash` / `reset` / `commit` / `add`；
   **禁止全仓 `cargo fmt`**（历史事故：把别人整树改动冲掉）。
   曾有会话用 `git checkout-index` 毁掉别人 **515 行**未提交代码。
2. **只用 Edit / Write 工具改代码。禁止 `sed` / `python` 批量替换 Rust。**
   历史事故：`sed` 模式含 `|` 破坏大括号；python 替换删掉三个函数整段；
   正则把字段插进 `impl` 块造成 **209 个编译错误**。
3. 🔴 **禁止 heredoc 写文件**（`cat > f <<'EOF'`）。本机实测会让**整个会话的 Bash 永久静默
   且转录零记录**（211 次调用后 3 分 22 秒空白，事后无法排查）。
4. **大块改代码用唯一锚点 replace，不要按行号 splice。** 历史事故：按行号切片再拼
   导致 **481 行连同一整个 IIFE 被复制**，之后三次去重都切错边界。
5. **配置项新增必须带 `#[serde(default)]`** —— 线上 config.json 是既有文件，
   缺 default → 加载失败 → **服务起不来**。
6. **推 `origin` 不推 `public`**（`public` = PUBLIC 仓已冻结，推了不可逆）。
7. **不在 VPS 上编译**（4 核会抢死正在服务的 sub2api），走 Actions `deploy-build.yml`。
8. **不碰线上配置值**（`credentialRpmLimit` / `inboundTargetRpm` 等），依据在
   `ws-vps/docs/02-tuning.md`。「容量口径是假的」那节说明按实测改小会把吞吐掐死一个数量级。
9. macOS/zsh：没有 `timeout`（是 `gtimeout`）；`--include='*.rs'` **必须加引号**
   （zsh 会先展开通配符）；`git status --cached` 不存在。
10. **提交**：用 git plumbing 在临时 index 做快照（`GIT_INDEX_FILE=/tmp/snap.index`），
    **逐个文件 `add`，不要 `-A`**；commit message 用 `-F 文件`而非 `-m`（反引号会被 shell 展开）；
    做完验证分支名与 `git status --porcelain | wc -l` 与开始时一致。
11. **禁止 `Co-Authored-By`** 或任何 Claude 署名行。Conventional Commits，中文描述，动词开头，无句号。

## 5. 实施顺序（每步独立可回滚）

**真依赖只有三条**：A5 → A3/A4/B4b；B1 → B2/B3/B4d；H2 → C2。

| 步 | 内容 | 依赖 | 为什么在这个位置 |
|---|---|---|---|
| S1 | 拆 `clone-management-card.tsx`（`SocksNodesPanel` / `CloneGroupsPanel` 两个文件） | — | 纯机械，先拆再加功能。1323 行上叠功能，风险主要来自文件本身 |
| S2 | **A5 共享选区 store** + 迁移 `dashboard.tsx` 6 组批量处理器 | — | 纯重构零新功能。之后 A3/A4/B4b 都变便宜；留一半会有两个选区真相源 |
| S3 | **B1 `proxy_node_id` 5 跳** | — | 后端纯新增、零行为变化。B2/B3/B4 全部建立在它上面 |
| S4 | A4 选择代数（shift 区间 / Cmd+A / Esc / 键盘）+ A6 修跨页陷阱 + A7 预演与撤销 | S2 | 收益最大且不依赖新后端 |
| S5 | **A3 框选 + 画布视图**（A1/A2） | S2, S4 | 用户点名的核心交互 |
| S6 | B4a 独立 IP 池视图 + B4c 卡片补下拉（抽 `exit-node-picker`）+ B4b 拖拽/批量指派 | S1, S3, S2 | 用户点名"在 IP 池里选 IP" |
| S7 | **B4d 分身换 IP 入口** + **撞出口**徽标（⛔ 不做"主份裸直连"徽标，见 B4d） | S3, S6 | 用户点名"分身切换 IP" |
| S8 | C3 rounds 计数器 + 顺带修 NaN | — | 与 A/B 完全解耦，可并行 |
| S9 | C4 `shield_stats_url` | S8 | 过渡期可观测 |
| S10 | **C2 5xx 吸收** | **H2 必须先修完** | 依赖 502 归因正确，否则会重试一个必然失败 |
| S11 | B2 节点健康探测 + B3 出口 IP 碰撞检测 | S3 | IP 池从"候选列表"变成真的池 |
| S12 | C5 shield 退役五步 | S9, S10 | 上线观测后才能动 Caddy |

**若要控制本轮范围**：最小可交付 = **S1 + S2 + S3 + S4 + S5**（画布与框选可用、
选区打通、`proxy_node_id` 就位）。IP 池 UI（S6/S7）与 shield 差集（S8–S12）可下一轮。

## 6. 用户已拍板的两条（**不要再问，也不要自行改回去**）

**① `proxy_node_id`：加。** 2026-08-06 用户确认。所以 B1 是**确定要做的**，
不是待评估项。它是 B2/B3/B4 的硬前置。

**② 主凭据不需要代理。** 2026-08-06 用户确认「主凭据不需要给代理」，
且 `OPEN-ISSUES` §六 第 4 条那个"两个方向相反"的悬案**就此关闭**。
落地要求见 **B4d 的 ⛔ 小节**：`POST /credentials` 多开时主份直连是**正常态**，
显示为中性「直连」chip，**绝不加 ⚠ 徽标**。加了会让每个正常多开组常亮假告警。

## 7. 仍需用户拍板的（不要替他决定）

1. **出口轮换策略** —— 默认粘滞（保 prompt cache）还是允许 429 时换 IP。B6 给出的实测
   证据倾向**默认粘滞、每号显式开启**。在他答复前**按默认粘滞实现**，把轮换做成配置项默认关。
2. **地理/ASN 标注** —— 需要离线数据集或外部查询。外部查询等于把出口 IP 送到第三方，
   **是数据出境决定**，不要默认加。**在他答复前不要做这一项。**
3. **C2 的 5xx 默认值** —— 本文建议默认关（便于对照实验），可自行按此实现，
   但要在 PR 说明里点出这是默认值选择。
4. `OPEN-ISSUES` §六 其余 4 条（H1 开关粒度 / M7 UI 挂点 / cache 三选一 /
   403 永久禁用阈值）与本任务无直接依赖，**不要顺手动它们**。

---

## 附：三条最容易踩的坑（前几个会话都踩了）

1. **测试"通过"不代表它执行到了被测路径。** 唯一可靠验证是「回退修复 → 必须 FAILED」。
2. **同一前提失败两次就停下来量，不要继续改测试。** 有个案例失败三次才对，
   前两次都是"机制推理正确但复现方式错"，第三次写诊断脚本并排实测才找到真因。
3. **语法检查过 ≠ 功能正常。** `node --check` / `tsc --noEmit` 只能证明"不崩"，
   证明不了"对"。**涉及视觉的结论必须截图看，涉及交互的结论必须在浏览器里驱动一遍**
   —— 框选、拖拽绑定、跨页选区这三项尤其必须真机验证。

