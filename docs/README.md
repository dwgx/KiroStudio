# KiroStudio 文档索引

> 本目录只保留**活跃**文档。历史/陈旧文档已归档到 `docs/archive/`(本地保留,不进公开 repo)。

## 活跃文档（当前有效，随代码演进维护）

### 核心技术参考
- **ARCHITECTURE.md** — 系统架构:Rust 网关 + React 运维台的整体设计
- **MODULES.md** — 模块划分与职责(src/ 各子系统)
- **PROTOCOL.md** — 协议转换链路(Anthropic/OpenAI 入站 → Kiro 上游)
- **INVALID-TOOL-PARAMETERS.md** — Invalid tool params 问题的分析与缓解方案

### 运维 / 部署
- **DEPLOY-WINDOWS.md** — Windows 部署说明
- **UI-COMPONENTS.md** — 运维台 UI 组件规范

### 进行中的专项
- **I18N-TASK-FOR-GROK.md** / **I18N-RESIDUAL-FOR-GROK.md** — I18N 三语覆盖任务与残留清单

### 交接
- **PROMPT-NEXT-AI-0717.md** — 最新交接文档(**新窗口/换 AI 先读这份**)

## 归档 (`docs/archive/`)

历史交接链、已落地的规划/研究/设计文档、早期草案,均移至 `docs/archive/`。
该目录整体被 `.gitignore` 忽略(含敏感运营信息:账户/租户/密钥线索,仅本地保留)。
按需查阅:

- **交接链** `PROMPT-NEXT-AI-07xx.md` / `HANDOFF-*.md` — 逐日交接历史(最新一份 0717 在上层活跃区)
- **规划** `PLAN-*.md` / `TODO-MASTER-*.md` / `PLAN.md` — 已落地的实施计划
- **研究** `RESEARCH-*.md` / `DESIGN-M365-*.md` — 限流/热重载/M365 族级等专项研究
- **早期草案** `OUTLINE.md` / `FEATURES.md` / `ENGINE.md` / `LOGIN.md` / `RESILIENCE.md` / `DISCUSSION.md` — 项目初期(7/1 前后)的设计草案,已被代码现状取代
- **其他** `ATTACK-REPORT-*` / `VERIFY-*` / `BACKLOG.md` / `PROJECT-SELF.md` / `WINDOWS-BOOTSTRAP-PLAN.md` / `PROMPT-FIX-KAM-*`
