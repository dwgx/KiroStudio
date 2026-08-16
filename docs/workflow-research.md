# 工作流研究：AI 编程工作流最佳实践 2025-2026

> 2026-08-16 定稿。用途：为 kirostudio 定制 WORKFLOW v2 提供外部依据。本文件只做研究，
> 不改代码。结论分三类：**值得吸收**（进 WORKFLOW v2 候选）、**不适合我们**（明确列出理由）、
> **已验证一致**（我们已做对，外部证据背书）。
>
> 研究方法说明：本地 searxng 实例（127.0.0.1:8888）在本次研究中几乎全部引擎失效
> （brave 限流、ddg 连接错误、google cse 超时、startpage 连接错误），改用 webfetch 直抓
> 一手权威来源（Anthropic 官方文档/工程博客、ADR 原版文章、Diátaxis）。OpenAI 官方页面
> 返回 403，相关内容未引用。所有结论均带来源链接。

## 来源清单

| 编号 | 来源 | 类型 | 抓取日期 |
|---|---|---|---|
| S1 | Anthropic《Best practices for Claude Code》（code.claude.com/docs） | 官方文档 | 2026-08-16 |
| S2 | Anthropic《Building effective agents》（anthropic.com/research/building-effective-agents, 2024-12） | 工程博客 | 2026-08-16 |
| S3 | Anthropic《How we built our multi-agent research system》（2025-06） | 工程博客 | 2026-08-16 |
| S4 | Anthropic《Effective context engineering for AI agents》（2025-09） | 工程博客/白皮书 | 2026-08-16 |
| S5 | Claude Code《How Claude remembers your project》（code.claude.com/docs/en/memory） | 官方文档 | 2026-08-16 |
| S6 | Claude Code《Explore the context window》（code.claude.com/docs/en/context-window） | 官方文档 | 2026-08-16 |
| S7 | Michael Nygard《Documenting Architecture Decisions》（cognitect.com, 2011-11） | ADR 原版文章 | 2026-08-16 |
| S8 | adr.github.io（GitHub adr 组织主页） | 社区标准 | 2026-08-16 |
| S9 | Diátaxis（diataxis.fr） | 文档方法论 | 2026-08-16 |
| S10 | origin 项目 `.claude/10-covenant/AGENTS.md`（本地参考仓） | 内部参考 | 2026-08-16 |

---

## 1. AI 编程工作流最佳实践 2025-2026

### 1.1 任务分解模式：五种可组合原语，简单优先

Anthropic 通过对数十个团队的分析，把 agentic 工作流归纳为五种可组合模式
（S2）：prompt chaining（固定链 + 程序化门）、routing（按输入分类路由）、
parallelization（sectioning 切分 / voting 多投）、orchestrator-workers（主控动态
分解 + 并行 worker）、evaluator-optimizer（生成 + 评估循环）。

**可操作结论**：

1. **「最成功的实现用的是简单可组合模式，不是复杂框架」**（S2 开篇）。我们的
   orchestrator-workers（主会话派 subagent）+ evaluator-optimizer（自 review +
   落实核验）+ 对抗 review（fresh-context 独立评估）恰好就是这三者的组合，方向被
   官方验证。
2. **orchestrator 必须学会委派**（S3 原则 2）：subagent 的任务描述必须有「目标、
   输出格式、工具/来源指引、任务边界」四要素。Anthropic 实测：描述含糊的早期版本
   出现子代理重复做同一件事、互相留空隙。我们的「上下文现编」纪律（目标/问题/已
   确认事实/文件路径/约束/非目标/输出/证据要求/验收）比官方建议更细，保留。
3. **规模随复杂度**（S3 原则 3）：简单查证 1 个 agent 3-10 次工具调用，复杂研究才
   >10 个 subagent。我们的「小而明确直接做，编排在小题上是净亏」与之同构。
4. **并行化两种用法要分清**（S2）：sectioning（切独立子任务加速）与 voting（同一
   任务多角度求高置信）。我们的「竞争性假设查 bug」是 voting 变体；「按文件边界
   切分」是 sectioning。可以显式命名这两种用法，避免混用。
5. **多智能体主要价值是「花够 token + 独立上下文」**（S3）：token 用量解释
   BrowseComp 80% 的性能方差；每个 subagent 独立上下文只回 1000-2000 token 摘要，
   是主会话上下文的「压缩器」。我们对 subagent 的定位（信息增益、上下文隔离）
   与官方机理一致。

### 1.2 上下文管理：context rot 是物理现实，三层应对

Anthropic 的 context engineering 白皮书（S4）明确：LLM 有「注意力预算」，
token 越多信息召回越差（context rot 现象）；上下文必须当**有限资源**管理，
目标是「最小的高信号 token 集合」。

**可操作结论**：

1. **三种长程应对技术，按任务选**（S4）：compaction（压缩总结，适合来回讨论型）、
   structured note-taking（外部笔记，适合有里程碑的迭代开发）、sub-agent 架构
   （适合并行探索）。我们三个全在用（自动压缩 + state.md 落盘 + subagent 隔离），
   但可以显式区分「什么时候该压缩」与「什么时候该开新会话」。
2. **压缩后什么存活是有明确定义的**（S6）：系统提示 / CLAUDE.md / memory / MCP
   工具**自动重载**；skill 描述不重载（只有实际调用过的 skill 保留）；对话总结
   保留「请求意图、关键技术概念、改过的文件与代码片段、错误与修复、待办任务」。
   我们的压缩应对协议（状态文件 + 持久锚点）与官方机制完全吻合——状态文件就是
   「外部记忆」，压缩后重读。
3. **结构化笔记是被验证的标准做法**（S4）：agent 定期把笔记写到上下文之外的
   NOTES.md，压缩/重置后读回。官方例子是 Claude 玩 Pokemon 时靠笔记跨千步保持
   目标。我们的 `.opencode/state.md` + 六件套就是这个模式，且加上了「关键证据
   落盘」的强化。
4. **会话卫生**（S1）：`/clear` 频繁用（不相关任务之间）；**同一问题被纠正两次
   后，上下文已被失败方案污染，应该清掉重开**（换一个吸收教训的更精确 prompt）。
   我们的「被纠正两次就先停下来想」+「同 bug 失败 2-3 轮换方向」与之同构，且
   更严格。
5. **CLAUDE.md 是唯一全量常驻的指令载体**（S5/S6）：分层设计（用户级 > 项目级 >
   路径作用域 rules > 按需 skills）本质上是一种「权威按加载时机分层」——越常驻的
   越精简、越该是稳定事实。

### 1.3 验证闭环：给 agent 一个能跑的检查

Claude Code 最佳实践第一条（S1）：**「给 Claude 一个它能跑的检查——测试、构建、
截图对比。这是『你在旁边看着』和『你可以走开』的区别。」** 没有检查时，"看起来
完成"是唯一信号，用户就是验证循环本身。

**可操作结论**：

1. **检查的分级投入**（S1，从轻到重）：
   - prompt 内跑检查（同一条消息里迭代）；
   - /goal 条件（每次轮转后独立复检，持续到解决）；
   - Stop hook（确定性门控，不通过不许收尾）；
   - **独立 subagent 二次意见**（fresh context 的模型尝试反驳结果，干活的人不
     给自己打分）。
   我们的「落实核验 agent 三态核对」就是最重的那一档，且每波必做——比官方默认
   配置更严。
2. **「先写失败测试，再修」是官方推荐**（S1 验证表）：复现 bug 的失败测试先行，
   然后修复到通过。这与我们守卫纪律的「删目标必红」验收标准同一原理。
3. **证据优于断言**（S1）：要求 agent 展示「跑过的命令 + 输出 + 结果截图」，而
   不是声称成功。我们的铁律「证据优于自信」+ DONE.md 证据落盘 = 官方建议的
   仓库级实现。
4. **修根因不压症状**（S1）："fix the build fails with this error… address the
   root cause, don't suppress the error"。我们第 10 节失败模式里已有同类教训
   （守卫静默绿）。

### 1.4 review 分层：fresh context 是对抗审查的前提

**可操作结论**：

1. **fresh context 的 reviewer 才能独立评估**（S1/S6）：reviewer 只看到 diff 和
   验收标准，看不到产生它的推理过程，「按自己的标准评估结果」。我们主线 review
   的「给原始目标/验收/diff/证据，不给作者自辩」与官方 Writer/Reviewer 双会话
   模式完全同构。
2. **对抗 review 的已知副作用**（S1 原文警告）：**「被要求找缺口的 reviewer 通常
   会报告一些，即使工作本身没问题——因为那是它的任务。追每个 finding 会导致过度
   工程：多余的抽象层、防御代码、为不可能发生的用例写测试。」** 对策：只标记
   影响正确性或明确需求的缺口，其余算可选。我们的对抗 review 目前没有这条限幅
   指令——这是明确的改进点（见 §3 建议 3）。
3. **评估要判最终状态，不判过程**（S3 附录）：多轮改状态的任务，检查「最终状态
   是否达成」而不是「是否走了预设步骤」。守卫机制验证的就是最终状态（needle 在
   生产代码里命中、删目标会红），与 end-state 评估同构。
4. **subagent 输出落盘，减少「传话游戏」**（S3 附录）：子代理把成果写到外部存储，
   回传轻量引用，避免大输出在对话链中逐级失真、烧 token。我们的核验报告进
   DONE.md、subagent 详细输出落盘到临时文件——已是这个模式。

---

## 2. 权威分级体系：两类权威的来源与变体

### 2.1 origin 的「两类权威」到底是什么

origin 宪法（S10）把权威问题分成两个互不混用的链：

- **行为事实链**（仓库现在是什么样）：可复现执行（测试/确定性探针/观察输出）>
  代码+测试+git log/blame > STATUS.md（先查日期再核对当前 commit）> 架构/协议/
  研究散文。并明确：「代码能揭示当前行为，但不代表行为正确；通过的测试可以
  保留已知缺陷——要把这个区别明确报告出来。」
- **规范决策链**（仓库应该变成什么样）：owner 当前明确方向 > 宪法 AGENTS.md >
  已接受的未失效 DECISIONS.md > 该实现车道的规范契约 > 设计原则 > 提案与实施
  计划 > 咨询/会话记录/实验。冲突时不默认挑方便的那个，要找更高层裁决或
  要 owner 裁定。

### 2.2 这种模式的来源与变体

「两类权威」不是一个单一来源的框架，而是工程界几个独立共识的组合：

1. **「可复现执行 > 文档」是经验主义传统**：Nygard 2011 年就写下
   **「大文档永远不会被更新」（Large documents are never kept up to date）
   （S7）**——文档必然过时，所以可执行事实（代码、测试）比散文更可信。这正是
   origin 行为事实链第 1-2 级排在最前的原因。Anthropic 的 "trust-then-verify
   gap"（S1：agent 产出看似合理的实现但不处理边界）是同一个原则在 agent 场景
   的表述。
2. **「决策记录 + superseded」是 ADR 传统**（S7/S8）：决定写进简短记录（Context /
   Decision / Status / Consequences），**被替换的决定不删除、标 superseded 并指
   向替代者**——「它曾经是决定这个事实仍然相关」。这是 origin 规范决策链第 3 级
   （DECISIONS.md）的机制来源，也是「决策记录 vs 提案」分层（记录有规范权重、
   提案没有）的来源。
3. **「用户最新指令 > 一切」是 agent 宪法运动的共同点**：Anthropic 文档与 origin
   宪法都有——Claude Code 把 CLAUDE.md 描述为「context 而非强制配置」，指令文件
   只是行为引导（S5）；真正强制用 hook。我们的铁律第 1 条「用户最新明确指令 >
   一切」与全行业一致。
4. **「权威按加载时机分层」是 Claude Code 记忆体系的隐性权威链**（S5/S6）：
   全量常驻（CLAUDE.md，<200 行）> 路径作用域（rules）> 按需加载（skills）。
   越常驻越精简、越该是稳定事实——与 origin「越核心的规范越靠前」同理。
5. **「陈述事实与规范意图分开」对应 Diátaxis 的区分**（S9）：reference（事实，
   是就是）与 explanation（意图，为什么）分属不同象限，写法不同、失效速度不同。
   origin 把行为事实与规范决策分成两条链，本质是同一认识的治理化。

### 2.3 对我们意味着什么

我们已有「证据优于自信」铁律和「STATUS.md 先读」约定，但**没有把「冲突时信
谁」写成显式链**。实际踩过的坑（线上配置 camelCase vs 代码 snake_case 误读、
文档滞后导致"已修还标待做"、注释写守卫字面量）都是「低权威源压过高权威源」
的实例。详见 §3 建议 1。

---

## 3. 文档体系设计：状态文件 / 决策记录 / 交接

### 3.1 ADR：写给未来开发者的短记录

Nygard 原版（S7）要点：ADR 是 1-2 页的短文件，四段式（Context/Decision/Status/
Consequences）；**被替换的决定保留并标 superseded**；「每一条 ADR 都当作与未来
开发者的对话来写」。adr.github.io（S8）确认这是被 AWS、Azure WAF、IEEE 采纳的
行业标准，并有 MADR 等模板变体。

**对我们**：完整 ADR 目录（doc/arch/adr-NNN.md 顺序编号）在单人 + 多会话项目里
过重。但我们**确实缺「已接受决策 + 被替换标记」的轻量机制**：ISSUES.md 现在记录
「移植候选研究结论」和否决项，但没有「接受某个方案」的正式记录与替换链。
建议以轻量变体吸收（§3 建议 2）。

### 3.2 CLAUDE.md 设计原则：越短越被遵守

Claude Code 记忆文档（S5）给出可操作标准：

1. **<200 行**；每行问自己「删掉这行会导致 agent 犯错吗」，不会就删。
2. **只写「不写会犯错」的内容**：构建命令、异于默认的代码风格、仓库礼仪、
   架构决策、环境怪癖、常见坑。**排除**：agent 读代码能自己知道的、频繁变化的
   信息（长说明、逐文件描述）。
3. **重复犯错才加**：Claude 犯同样的错第二次 → 写进 CLAUDE.md。
4. **内容分级**：常驻（CLAUDE.md）vs 路径作用域（rules）vs 按需（skills）vs
   变化信息（状态文件）。

**对我们**：六件套职责表（STATUS/CLAUDE/CONTEXT/CURRENT/ISSUES/DONE/state）与
「常驻 vs 变化信息」分层天然吻合——CLAUDE.md 只装长期约束，STATUS.md 装变化
状态，CURRENT.md 装会话态 + 守卫清单。「重复犯错才加」与我们失败模式清单的
演进方式一致。

### 3.3 状态文件与交接：业界验证的标准模式

1. **结构化笔记 / agentic memory 是长程任务的标配**（S4）：NOTES.md 模式——
   定期把关键状态写到上下文外，压缩后读回继续。官方用「写计划到 Memory 防止
   超 200K 截断丢计划」做生产验证。我们的 `.opencode/state.md` + 压缩恢复协议
   是同一模式的成熟实现。
2. **索引 + 主题文件的组织**（S5 auto memory）：MEMORY.md 是 200 行/25KB 的
   索引，详细内容放主题文件按需读。我们的 CURRENT.md 守卫清单（带行号）+
   docs 细分文档同构。
3. **压缩后什么存活是硬事实**（S6）：见 §1.2 结论 2。**只有落盘的东西在压缩后
   一定回来**——这从机制层面证明了「文档六件套每波同步」不是仪式。
4. **多会话交接**（S1）：会话命名 + checkpoint 恢复 + Writer/Reviewer 双会话
   隔离。我们的「波次结束立即同步，不许攒」对应官方「状态文件随波次更新」。

### 3.4 Diátaxis：文档按读者需求分四象限

Diátaxis（S9）把技术文档分四类：tutorial（学习路径）、how-to（完成任务）、
reference（事实查阅）、explanation（理解为什么）。它不解决权威问题，但解决
「一份内容该放哪、该多稳定」。

**对我们**：六件套自查——STATUS.md（当前事实，reference 类）、CLAUDE.md（长期
约束+操作，how-to/reference 类）、CONTEXT.md（subagent 背景解释，explanation
类）、DONE.md（证据，reference 类）。**缺 tutorial 类**（新 subagent/新会话的
上手路径）——但我们的 subagent 是现编 prompt 的，不需要 tutorial 文档，列为
不必要（§4）。

---

## 4. 质量闭环：测试驱动、验证前置、失败模式预防

### 4.1 业界共识：验证前置 + 失败测试先行

1. **「先写失败测试再修」横跨三家**：Anthropic（S1：write a failing test that
   reproduces the issue, then fix it）、origin 宪法（S10：A new defect test must
   fail against the defective behavior and pass after the fix）、ADR 传统
   （S7 的「缺陷测试」理念）。我们守卫纪律的「删目标必红」是同一验收标准的
   针式实现——外部证据表明这是行业级共识而非我们发明。
2. **修根因不压症状**（S1）：官方原话 "address the root cause, don't suppress
   the error"。我们第 10 节失败模式（守卫静默绿 ×5）就是「压症状」的典型反面
   教材。
3. **检查分级投入**（S1）：见 §1.3 结论 1。我们有最高档（独立核验 agent），
   且纳入「新增测试按名单跑确认真执行」——补上了「检查本身可能是假的」这个
   缺口（对应 trust-then-verify gap）。
4. **LLM 评估要「判结果不判过程」**（S3 附录 end-state eval）：见 §1.4 结论 3。

### 4.2 失败模式清单：业界标准做法，我们可以结构化

Anthropic 官方把「Avoid common failure patterns」列为最佳实践章节（S1）：
kitchen sink session（一个会话堆不相关任务）、repeated correction（反复纠正
→ 上下文被失败方案污染 → 清掉重开）、over-specified CLAUDE.md（太长导致规则
被忽略）、trust-then-verify gap（看起来完成但没验边界）、infinite exploration
（不限范围的「调查」填满上下文）。

**对我们**：第 10 节「常见失败模式」比官方清单更具体（仓库级教训带次数），
方向完全正确。可改进：把「现象」与「预防动作」显式挂钩到守卫纪律，形成
「模式 → 预防 → 对应守卫」结构（§3 建议 5）。

### 4.3 与我们的守卫机制对照

- 守卫（needle 拼接、删目标必红）= 「确定性门控」的思想：Anthropic 建议用
  Stop hook 做「无论 agent 决定什么都执行」的强制（S1/S5）；我们在 opencode
  上无 hook 可用，用「代码内守卫 + 本地模拟验证」实现了同等强度的确定性检查。
- 「网络抖动假失败重试一次再判」= 对验证工具本身的健壮性管理（S1 的 verify
  the verifier 精神）。

---

## 5. 对现有 WORKFLOW.md 的改进建议清单

### A. 值得吸收（进 WORKFLOW v2 候选）

**A1. 显式「两类权威」链（最高价值，吸收 origin S10）**
在 WORKFLOW.md 新增一节（或并入铁律区）：

- **行为事实链**（回答「现在是什么样」）：服务器实测/本地可复现执行 > 代码与
  测试（git log/blame 辅助意图）> 守卫清单与 STATUS.md（先核对日期与 commit）>
  架构/设计/研究文档。附 origin 原句精神：「代码能揭示行为但不代表行为正确，
  通过测试可保留已知缺陷——明确报告这个区别」。
- **规范决策链**（回答「应该变成什么样」）：用户最新明确指令 > 本文件
  （WORKFLOW.md/CLAUDE.md）> ISSUES.md 已接受决策（含 superseded 标记）>
  参考仓研究与提案。冲突时不默认挑方便的，找更高层裁决或问用户。

价值：subagent 拿到互相冲突信息（STATUS.md 说 A、代码是 B）时有了裁决依据；
把我们已有的「证据优于自信」从口号升级成可执行的查表。

**A2. 轻量决策记录：ISSUES.md 增加「已接受决策」段（吸收 ADR S7/S8）**
每条：决策 / 背景 / 被替换的旧决策（superseded 链）/ 日期。被替换不删除只标记。
现有「移植候选研究结论」与「否决项」段保留，新增「已接受」段补齐决策闭环。
不做独立 doc/arch 目录与顺序编号（单人项目过重）。

**A3. 对抗 review 加限幅指令（吸收 S1 的明确警告）**
主线 review prompt 固定加一句：「只报影响正确性或需求覆盖的缺口（BLOCKER/
MAJOR），风格偏好与可选项一律不报。」防「被要求找茬就找茬」导致的过度工程
（多余的抽象、防御代码、为不可能用例写测试）。

**A4. 验证闭环显式分级表（吸收 S1）**
在 §5 验证循环处补一张分级表，明确每档触发条件：
prompt 内自检（小改动）→ 按名单跑测试（新增/修改测试）→ 落实核验 agent
（每波必做，已有）→ 服务器 CI（判定标准已有）。补一句「验证工具本身要验证
（假失败重试、按名单确认执行）」——我们已有实践，缺显式表述。

**A5. 失败模式清单结构化（吸收 S1 的 common failure patterns 章节）**
把 §10 从「现象 + 次数」扩展为「现象 → 根因 → 预防动作 → 对应守卫」四列
（保持现状的仓库级具体性，不加行业通识条目）。已具备：条目 1（needle 模拟
验证）、6（静默绿）、2（文件边界）已能直接对应到守卫纪律。

**A6. 会话卫生显式化（吸收 S1/S4）**
在任务生命周期或派发纪律补一句：「同一问题被纠正 2 次或同 bug 失败 2-3 轮
→ 停止当前路径，清上下文重开（新 prompt 必须吸收教训）」。现有规则是「停下
汇报换方向」，补上「重开」这个动作与条件。

**A7. 检查是假的检查：trust-then-verify gap 进失败模式（吸收 S1）**
把「agent 说测试通过但没真跑（前端 37/37 虚报）」已记录的事实，在 §10 补为
显式条目：「验证声明必须复核（测试输出、命令回显），不可凭『逻辑自洽』接受」
——已有相关纪律，做成显式失败模式条目。

### B. 不适合我们（明确排除）

**B1. 完整 ADR 体系**（doc/arch/adr-NNN.md 顺序编号、每个架构决策一条记录，
S7/S8）：单人 + 多会话 + 8GB 小机，记录维护成本 > 收益。轻量决策记录（A2）够用。
理由：Nygard 自己也说 ADR 的价值在「团队成员轮换时传递上下文」，我们是单人多
会话，决策上下文通过六件套 + 状态文件传递。

**B2. CLAUDE.md <200 行硬限制**（S5）：我们已有六件套分层，CLAUDE.md 只装长期
约束且本来不长。限制对我们没有约束力，不引入。

**B3. 工具机制硬套**（/goal、Stop hook、agent teams、checkpointing，S1）：
这些是 Claude Code 产品机制，opencode 没有等价物。机制不移植，只移植背后的
原则（确定性门控 → 守卫；fresh context 评审 → 对抗 review）。

**B4. 大规模自治编排**（S3 的 3-5 subagent 并行 + 自主派生，research 场景）：
8GB 物理上限已定死并行 ≤5；且我们已有按文件边界派发纪律，不做「agent 自主
决定 spawn 多少 worker」的自治化。

**B5. Diátaxis 全套重组**（S9）：六件套职责表已明确且经过 W2-W6 验证，不做
结构性重组。只做「缺 tutorial 类文档」的确认：subagent 是现编 prompt 的，
不需要 tutorial，确认不需要。

**B6. LLM-as-judge 评估体系**（S3）：我们的「核验 agent 三态核对 + 服务器 CI
确定性判定」比 LLM 打分更可靠（我们的产出是代码，可确定性验证；LLM judge 适
用于自由文本评估）。不引入。

### C. 已验证一致（外部证据背书，不改动）

| 我们的机制 | 外部对应 |
|---|---|
| 铁律 2 证据优于自信 + DONE.md 证据落盘 | S1「show evidence rather than asserting success」 |
| 铁律 1 用户最新指令 > 一切 | S10 origin 宪法 5.1 / S5「context 而非强制配置」 |
| 生命周期「小而明确直接做」 | S1「能一句话描述 diff 就跳过计划」 |
| 派发纪律「上下文现编 + 信息增益」 | S3「教 orchestrator 如何委派」+ 原则 4 |
| 落实核验（独立 agent 三态核对） | S1「verification subagent：干活的人不给自己打分」 |
| 守卫「删目标必红」 | S10「defect test 必须 fail-then-pass」 |
| 压缩恢复协议（状态文件 + 锚点） | S4 structured note-taking / S6「只有落盘的一定回来」 |
| 核验报告进 DONE.md（防传话失真） | S3「subagent 输出落盘避免 game of telephone」 |
| 部署 .bak-<tag> 备份回滚 | S3 rainbow deployment 原则 |

---

## 6. 与 origin 体系的对比

### 6.1 origin 强在哪

1. **权威分级显式化**（S10 §1）：两类权威写成宪法条款，subagent 在信息冲突时有
   明确的查表裁决链——我们只有口号（证据优于自信），没有链。
2. **规范契约分层细致**：legacy runtime vs Realm protocol 双车道各有其规范
   契约（PROTOCOL.md vs docs/PROTOCOL-SPEC），改动时先声明车道。我们是单仓单
   协议，不需要，但这个「先声明你在改哪条车道」的纪律有价值。
3. **研究证据不可改写**（S10 §5）：失败的假设和记录保留原样，新结论只能叠加、
   带出处。我们的 ISSUES.md 否决项（fingerprint break 等）做到了「记录否决」，
   但没到「研究过程证据不可编辑」的程度——小项目不必这么重。
4. **单一 gate 入口**（make verify）：一次跑全部核心检查。我们有服务器验证循环
   但命令分散在 CLAUDE.md，无单入口。
5. **预注册协议**（pre-registered protocol）：实验先写协议再执行，防止事后修饰。

### 6.2 我们强在哪

1. **实战提炼的具体性**：origin 是「抽象规范」（宪法条款），我们是「血泪清单」
   （needle 缺空格 ×3、并行同文件冲突、`{e}` 命名捕获 E0425）。规范告诉你信谁，
   清单告诉你坑在哪——两者互补，清单在我们这更可执行。
2. **落实核验机制化**：origin 靠「lead integrator 对委派结论负责、自己重读关键
   文件」的个人纪律；我们把「每波派独立核验 agent + 三态表 + 报告进 DONE.md」
   做成流程，不依赖单次自觉。
3. **资源约束下的现实编排**：8GB 并行上限、按文件边界派发、探查放开执行收
   紧——origin 没有物理资源纪律（Go 项目本地能跑全套验证）。
4. **守卫/针机制**：把「验证必须是真验证」落地为代码级守卫 + 删目标必红验收，
   origin 只有「defect test 必须 fail-then-pass」的原则，没有针式落地。
5. **文档六件套职责表**：每个文件有职责与同步时机，origin 的 memory 体系
   （MEMORY-SYSTEM.md）更复杂但没有这么清晰的「每波同步」纪律。

### 6.3 互补点（双向）

- **我们吸收 origin**：两类权威链（A1）、决策记录 superseded（A2）、「声明车道」
   精神（改哪条权威链先说清——对应我们「行为事实 vs 规范决策」要分开问）。
- **origin 可吸收我们**（仅记录，不实施）：核验 agent 机制、守卫/针、按文件边界
   派发、文档每波同步纪律。它们的宪法在「谁来证明完成」上只有个人纪律，没有
   机制化。
- **共同基础**：用户最新指令 > 一切；证据/可复现执行 > 文档；决策记录保留历史；
   失败测试先行。三个来源（Anthropic、origin、ADR 传统）在这些点上互相印证，
   说明这些是稳定的行业共识，不是个人偏好。

---

## 7. 一句话总结

行业 2025-2026 的共识与我们 W1-W6 的实践高度吻合（验证闭环、fresh-context
对抗 review、状态文件压缩恢复、失败模式清单都得到一手来源背书）；最大增量是
把「证据优于自信」升级为**显式两类权威链**，把「否决了什么都记了」补上
**已接受决策记录**，并给对抗 review 加**限幅指令**防过度工程。其余以轻量
吸收为主，不为仪式感加流程。
