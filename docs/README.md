# KiroStudio 文档索引（2026-08-20 Windows）

活跃文档在本目录。过期过程稿在 `docs/archive/`（gitignore，仅本地）。

**读序（新窗）：** 仓根 `STATUS.md` → 本目录 `TAKEOVER.md` → `.agent/HANDOFF.md`（本地进度，不进远程）→ 下面「核心技术」。不要用仓根 `HANDOFF-*` / `.opencode/state.md` 当现状。

## 新窗 / 状态

- `TAKEOVER.md` — 执行层：git / 验证 / 快照允许名单 / tag 准备
- `PERFORMANCE.md` — 性能实测（W13；线上数字仍须现读）

## 核心技术

- `ARCHITECTURE.md` / `MODULES.md` / `PROTOCOL.md` / `INVALID-TOOL-PARAMETERS.md`
- `UI-COMPONENTS.md`

## 运维

- `DEPLOY-WINDOWS.md` / `DEPLOYMENT.md` / `SECURITY-BACKUP.md` / `CRASHLOOP-ROLLBACK.md`

## 仍有效的研究与设计（未全部落地）

- `error-codes-inventory.md` — 错误码清单（W7–W8 已实现可配置化）
- `ref-ZyphrZero-kiro.rs.md` / `ref-kiro2cc-proxy.md` — 参考仓教训
- `model-compat-plan.md` — P0 已做，P1/P2 视代码
- `client-key-design.md` / `compat-upgrade-plan.md` — 未实施或只实施了一部分，以代码为准
- `quota-402-design.md` / `p2-family-rl-response-model.md` — 设计稿

## 归档

`docs/archive/relay-2026-08-20/`：本波从仓根/docs 移走的 blockers、scheduling 研究、W13 会话报告、deepseek/passthrough 旧 spec、旧 workflow 脚本。推导材料，不承载当前结论。
