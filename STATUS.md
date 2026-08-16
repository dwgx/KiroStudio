# 当前状态入口（审计快照）

> 更新时间：2026-08-16（W13 最终收尾：blockers 核验 + 性能实测 + I18N 彻底干净 + 最终部署完成）。
>
> 本文件只记录已经核对过的边界：`HEAD`、未提交工作树、线上观察、以及设计/未验证项。
> 需要继续接手时，读本文件后再读 `docs/TAKEOVER.md` 与 `.claude/state/CURRENT.md`。
> **问题总清单（a-e 五类）在 `.opencode/ISSUES.md`；已完成工作+真实证据在 `.opencode/DONE.md`；
> 波次状态在 `.opencode/state.md`；会话全景报告在 `docs/session-report-2026-08-15-16.md`——
> 四者与本文件保持同步。**
> 根目录的 `HANDOFF-*`、`PLAN-*`、`TRACKING-*`、`OPEN-ISSUES-*` 和旧 `STATUS-*` 都是
> **历史档案**，不能覆盖本文件的当前结论。

## 先看结论

- **HEAD** = `513e7f0`（docs 交接快照，2026-08-15；代码侧最后提交 `1e100a2` 严格语义修复）。
  `origin/master`（dwgx/KiroStudio-skiapi 私有）已推，一致。
- 版本号 `1.1.1`（Cargo.toml，W2-W13 未 bump）；`v1.1.1` release 产物落后线上全部 W2-W13 修复。
- **工作树有大量未提交改动**（多会话并发，W2-W13 全部工作都在工作树里），禁止裸 commit/checkout，
  快照走临时 index（CLAUDE.md 验证循环）。
- **CI 最新**：**2020 passed / 0 failed**（W10-W12 合并验证，skiapi Docker 实测）；前端 tsc 干净 +
  **53 pass** + pnpm build 成功。
- **线上（nbus 38.244.34.15:8990）**：跑 **W13 最终 build（sha256 88270616，build_sha=final，
  version 1.1.1）**（2026-08-16 05:24 部署，healthz 全绿 pool=4，多轮 smoke 全过）；备份
  `.bak-pre-final`。**这是当前线上**。
- 线上 4 通道状态（最近实测）：#1 fuckopencode（deepseek-v4-flash）正常（周配额曾耗尽 24h 恢复，
  端口已固化 8787）；#2 deepseekapi（deepseek-v4-pro）正常；#3 cursorapi 已启用但**号池空**
  （502 持续，sonnet fallback 到 #1）；#4 pigcode（gpt-5.6-sol）正常（上游偶发 502，非网关问题）。
- `public`（dwgx/KiroStudio 公开仓）master 冻结（CLAUDE.md 纪律，停在 7368a70）；只推 tag（发版例外）。

## 证据分层

| 层级 | 当前可写的结论 |
|---|---|
| `HEAD` | `513e7f0`（docs 快照，父提交 `1e100a2`）及其祖先为已提交代码；W2-W13 全部改动未提交（工作树）。 |
| 工作树 | 203 个未提交文件（多会话并发，见上）。 |
| `origin` | 已推 master（私有，唯一开发仓库）。 |
| `public` | master 冻结（7368a70）；v1.1.1 tag + release 已推（发版例外）。 |
| 线上 | nbus **88270616**（build_sha=final）运行中（2026-08-16 05:24 部署，sha256sum 实测一致，healthz 全绿）。 |
| Release | `v1.1.1` = fb682e1；5 产物 + sha256（public）。**注意**：release 产物不含 W2-W13 修复，从 release 下载安装会缺全部修复。 |
| 设计/计划 | `docs/` 中 RFC、spec、plan 只能证明设计或研究存在，除非有当前代码和测试证据，不得写成已实现。 |

## 波次摘要（W1-W13，2026-08-15 → 08-16）

> 全量细节见 `.opencode/state.md`（波次记录）+ `docs/session-report-2026-08-15-16.md`（全景报告）。

| 波次 | 内容 | 验证/部署 |
|---|---|---|
| W1 | 全仓深度审计：11 MAJOR + 40 MINOR 清单 | 0 BLOCKER |
| W2 | 51 项修复（协议 15/安全 5/健壮性 9/接线/前端 5/docs 3） | CI 1766→**1845**；前端 46→48 |
| W2.5 | 对抗 review → 3 MAJOR + 8 MINOR 全修 | 全绿 |
| W3 | 上线 + 4 通道 smoketest + 参考仓库吃透（zyphr/k2cc，8 路总结） | 部署 **52748cf2** |
| W4 | 9 路对比研究 + 2 路对抗评审（否决 fingerprint cap、改设计跨月恢复/模型路由、降级 rotation_bias） | ISSUES (a)-(e) 落盘 |
| W5 | 模拟缓存功能 + 修复清单 7 项 + 4 路深度验证 | CI **1886**；前端 48/0 |
| W6 | 模型兼容波：mapped_model 预判缺口 + 登记 A/B/C + sub2api 通配符/路径段移植 + 测试矩阵 | CI **1921**，未部署 |
| W7-W8 | 错误码/提示词可配置化（42 key + 热加载 + 前端弹窗）+ 7 路 smoke | CI **1955**；部署 **a3ae8874**；诊断 websearch 结构性缺陷 |
| W9 | 前端 a11y（323 console issues → 0）+ i18n 完美化（528 行中文扫尾）+ Token 交换修复 | CI **1961**；前端 48/0 |
| W10-W12 | 绊脚石 16 项 + 并发工程类（count_tokens 可注入/真并发测试/播种自检/healthz build_sha/告警扩展/alerting poison 修复） | CI **2020**；前端 **53**；部署 **edf27204**（缓存坏产物回滚重建） |
| W13 | 最终收尾四线：blockers 核验 14/16 有效 + 2 瑕疵 4 MINOR 全修（stats_stale 接线等）；性能实测 + 仪表盘；I18N 三语 2344 键 | CI 2020/0 + 前端 53/53；部署 **88270616**（build_sha=final） |

## 已知遗留（当前代码里有 TODO / 半接线 / 未做；详见 .opencode/ISSUES.md）

- **websearch 快路径结构性缺陷**（W7-W8 诊断，待 owner 决策）：快路径 MCP 硬依赖 Kiro 池号，
  纯透传池时透传拦截（静默失效）或无号 502。选项 D 补 Kiro 号（零代码，推荐）/ A 判定前移 /
  B 无号降级转发 / C 大工程。
- **opencode 配置（用户侧）**：kirostudio provider 指向 k1ro.skiapi.dev（skiapi，Kiro 池冷却中）——
  建议切 api.dwgx.top + deepseek-v4-pro（唯一稳的路）。**待用户确认**（key 由用户提供）。
- **v1.1.2 发版**：release 产物落后线上全部 W2-W13 修复，需 bump → CI → tag → Actions → nbus。
- **gpt-5.6-sol 请求来源**：03:12 两条来自未知客户端（session 41e82a06/7c1b459e，IP 未记录）。
- **#3 cursorapi 号池空**：502 持续，sonnet 已 fallback 到 #1；长期依赖单通道有单点风险。
- **性能 P1 建议**（docs/PERFORMANCE.md §9，只记录未实施）：UUIDv4→v7、RPM intern、
  EndpointHealth 单遍历。
- **A-5/A-6 低危项**：429 换区后 403 绕圈；select_endpoint 备区只取 PROBE_ORDER 第一项。
- **D 类阈值**：无真实故障分布数据，先修度量再调参（PERFORMANCE.md §9 P2）。
- **参考仓库移植候选全量**：见 .opencode/ISSUES.md (d) 节（模型正向路由/客户端 Key 分发待做，
  rotation_bias/fingerprint cap 否决）。
- **websearch 整轮缓冲改造**：设计文档已备（docs/archive/websearch-buffering-rework.md），等上号实测。

## 未验证、未做与阻塞

### 未验证
- nbus 功能回归长周期表现（黑名单 30min TTL、严格语义、ramp_tier、pinned client 固化解析、stats_stale
  watchdog——真实流量观察中）。
- 流式路径性能基准（PERFORMANCE.md §10 附录：只测了非流式，流式 TTFB 待下一轮）。
- 微软 SSO 完整登录流程（回调路径已修并部署，待用户登录实测一次）。
- 生图三层不通（路由 404 + catalog 400 + 上游 key 无 image 权限）——排障记录见前序波次。

### 未做（有意，有证据）
- 未动 public master（冻结纪律）。
- 未填 OTA token；OTA 自动检查默认关（restart-only 语义已接线）。
- upstream_trace 默认关（接线完成未开启，开启前按 PERFORMANCE.md §9 P2-6 复测 CPU 基线）。
- native_thinking_effort_enabled 默认关（待上号实测后开）。
- 性能 P1/P2 建议全部未实施（PERFORMANCE.md §9，只记录）。

### 需要 owner 决策
1. websearch 结构性缺陷修复选项（D 补 Kiro 号 / A 判定前移 / B 降级转发 / C 大工程）。
2. 是否发 v1.1.2（release 产物落后线上全部 W2-W13 修复）。
3. opencode 配置是否切 api.dwgx.top（v4-pro 唯一可用路）。
4. gpt-5.6-sol 请求来源排查。
5. 是否开启 native_thinking_effort_enabled。
6. 移植优先级：模型感知正向路由 / 客户端 Key 分发等（见 ISSUES.md (d)）。

## 下一步（仅 owner 批准后执行）

1. websearch 结构性缺陷按 owner 选型修复（D 零代码，可直接做）。
2. 发 v1.1.2（bump → CI → tag origin+public → Actions → nbus）让 release 产物追上线上修复。
3. P0 移植波（ISSUES.md (d)：模型感知正向路由 / 客户端 Key 分发）+ 压测回归（v1.0.1 基线：
   QPS 1702 / P99 319ms / 0 击穿）。
4. 微软 SSO 完整流程实测（用户登录）。
5. 性能：流式基准补测 + P1 建议择机实施（PERFORMANCE.md §9）。
6. 后续发版流程照旧：波次实现 → review → CI（skiapi Docker）→ bump → push origin → tag + public
   release（ALLOW_PUBLIC_PUSH=1）→ nbus 部署验证（校验 sha + build_sha == 快照 commit）。

## docs/ 索引（2026-08-16 核对）

> 活跃文档按主题分；过时/被替代的保留为推导材料（标注时效），历史文档在 `docs/archive/`。

| 文件 | 用途 | 时效 |
|---|---|---|
| `ARCHITECTURE.md` | 系统架构（Rust 网关 + React 运维台） | ✅ 活跃（W2 更新 12 键排序） |
| `MODULES.md` / `PROTOCOL.md` / `UI-COMPONENTS.md` / `INVALID-TOOL-PARAMETERS.md` / `DEPLOY-WINDOWS.md` | 模块/协议/组件/工具参数/Windows 部署 | ✅ 活跃（稳定区） |
| `TAKEOVER.md` | 执行层交接（git 边界/守卫/验证/部署） | ✅ 活跃（2026-08-15 复核版） |
| `PERFORMANCE.md` | 性能实测硬证据 + README §8 草案 + P1/P2 建议 | ✅ 活跃（W13 新建，2026-08-16） |
| `session-report-2026-08-15-16.md` | 会话全景报告（W1-W13 时间线/数字/交付物/遗留） | ✅ 活跃（W13 新建） |
| `README.md` | docs 目录索引 | ✅ 活跃（需随新增同步） |
| `blockers-*.md` ×6（structure/protocol/config/concurrency/engineering/testing） | 绊脚石审计清单（锁/配置/协议/工程/结构/测试） | 🟡 已闭环（所列待修项由 W7-W13 修复，正文保留为审计快照） |
| `error-codes-*.md` ×4（inventory/config-design/config-mechanism/client-behavior） | 错误码清单 + 可配置化设计/机制/客户端判据 | ✅ 已实现（W7-W8）；inventory 承重串勘误已落（W11） |
| `ref-ZyphrZero-kiro.rs.md` / `ref-kiro2cc-proxy.md` | 参考仓库机制/问题/教训总结 | ✅ 活跃（W3-W4，30+ 条核验） |
| `model-compat-plan.md` | 模型兼容层分期规划（P0 完成/P1/P2） | 🟡 P0 已实现（W6），P1/P2 待做 |
| `model-forward-routing-design.md` | 模型正向路由实现级设计（P1 候选） | 🟡 设计稿，未实施（待 P1） |
| `quota-402-design.md` | QUOTA_EXHAUSTED_ALL→402 改造研究（结论：条件做） | 🟡 设计稿，未实施（跨月恢复已做，402 配套待 owner） |
| `cooldown-reason-i18n-design.md` | cooldownReason 语言耦合改造设计 | ✅ 已实现（W11 枚举化） |
| `p2-family-rl-response-model.md` | P2 设计细化（家族限流 A5 不做 / 响应观察 A3 待做） | 🟡 设计稿，未实施 |
| `CRASHLOOP-ROLLBACK.md` / `DEPLOYMENT.md` / `SECURITY-BACKUP.md` | 部署/回滚/备份操作手册 | ✅ 活跃（部署流程事实以 CLAUDE.md 为准） |
| `archive/`（CACHE-RFC、HISTORY、CURRENT-2026-08-11、websearch-buffering-rework 等 26 项） | 历史归档（推导材料） | 🟠 归档，不承载当前结论 |

## 历史档案入口

所有 `HANDOFF-*`、`HANDOFF-NEXT*`、`OPEN-ISSUES-*`、`STATUS-2026-08-05.md`、`PLAN-*`、
`TRACKING-*`、`TASK-BUILTIN-RETRY.md`、`PROMPT-FOR-DEEPSEEK.md`、`DEEPSEEK-CONTENT-GAP-SPEC.md`、
`PASSTHROUGH-COMPAT-SPEC.md` 均保留为证据与推导材料。历史文档中的数字、线上状态、行号和
「已上线/待修」措辞必须先用本文件和当前代码重新核对。
