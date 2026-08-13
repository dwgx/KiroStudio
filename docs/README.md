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
- **DEPLOYMENT.md** — Docker/systemd 运维健壮性（数据落卷、日志上限、健康探针、crashloop 回滚）
- **SECURITY-BACKUP.md** — 备份与密钥安全(密钥独立于凭据备份)
- **CRASHLOOP-ROLLBACK.md** — 容器崩溃循环回滚流程
- **UI-COMPONENTS.md** — 运维台 UI 组件规范

### 交接
- **TAKEOVER.md** — 最新交接文档(新窗口/换 AI 先读这份)

## 归档 (`docs/archive/`)

历史交接链、已落地的规划/研究/设计文档、早期草案,均移至 `docs/archive/`。
该目录整体被 `.gitignore` 忽略(含敏感运营信息:账户/租户/密钥线索,仅本地保留)。
按需查阅:

- **交接/历史记录** `HISTORY.md` / `ARCHITECTURE-DECISIONS-2026-08-09.md` / `tonight-changes-review.md` — 历史档案与逐日过程记录
- **规划/任务书** `TASK-CANVAS-IPPOOL-SHIELD.md` / `balance-import-impl-plan.md` / `clone-page-impl-plan.md` / `shield-ui-plan.md` / `clone-mgmt-specs-2026-08-03/` — 已落地的实施计划与 spec
- **研究/实验** `CACHE-RFC.md` / `CACHE-RESEARCH.md` / `CACHE-EXP0-RESULT.md` / `capacity-truth.md` / `batch2-recon-E-probe-matrix.md` / `batch2-region-endpoint-matrix.md` / `cache-probe-data/` / `prefix-stability-2026-08-06.md` / `region-burn-fix.md` / `websearch-buffering-rework.md` — 缓存/容量/批量侦察等专项研究
- **设计/实现记录** `absorb-layer-design.md` / `region-self-correction-design.md` / `paragraph-repeat-guard.md` / `auto-compact-fix-2026-08-06.md` — 已实现的吸收层/自纠错/防重复设计
- **已完成专项** `I18N-TASK-FOR-GROK.md` / `I18N-RESIDUAL-FOR-GROK.md` — I18N 三语改造已完成(三语字典 key 齐,组件已接入)
