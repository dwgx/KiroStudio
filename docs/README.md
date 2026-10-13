# KiroStudio 文档索引（2026-10-13 复活线改写）

**新窗读序：** 仓根 `STATUS.md`（当下状态）→ `CLAUDE.md`（长期约束）→ 本索引 → 按需开文档。

机器级板（hmc 服务、未决清单、核对命令）：`C:\Users\dwgx1\.dwgx\ops\boards\KiroStudio.md`。
旧纪元的 `.agent/HANDOFF.md` / `.agent/NEXT-PROMPT.md` 等**不在公开克隆里**，不要再引用。

## 状态 / 接手

- `TAKEOVER.md` — 执行层接手（v1.2.4 时代，简短）
- `RELEASE-CHECKLIST.md` — 下一个 tag 的闸门（打 tag 要 Owner 点名）
- `IMPROVEMENT-PLAN-2026-08-21.md` — 08-21 审查基线（历史推导材料，不当待办总表）
- `PERFORMANCE.md` — 性能实测（数字用前现读）

## 核心技术

- `ARCHITECTURE.md` / `MODULES.md` / `PROTOCOL.md` / `INVALID-TOOL-PARAMETERS.md`
- `UI-COMPONENTS.md`
- `model-compat-plan.md` / `error-codes-inventory.md` / `quota-402-design.md`

> ⚠️ 行号、「唯一端点」、行数类断言会过期，以代码为准（历史实测：行数差 2.5 倍、ide/cli 两个端点都注册）。

## 运维

- `DEPLOYMENT.md` — 部署总览（部分内容为旧 VPS 纪元，以 `STATUS.md` + `E:\dwgxhomecloud\ops\kirostudio\` 为准）
- `DEPLOY-WINDOWS.md` — Windows 本地部署
- `SECURITY-BACKUP.md` / `CRASHLOOP-ROLLBACK.md`

## 参考（教训与对照，不照搬）

- `ref-ZyphrZero-kiro.rs.md` / `ref-kiro2cc-proxy.md`
- `p2-family-rl-response-model.md`

## 归档

历史过程稿（`HANDOFF-*` / `PLAN-*` / `TRACKING-*` / 旧 spec）是推导材料，不承载当前结论；
新结论一律进 `STATUS.md` 或对应主题文档。
