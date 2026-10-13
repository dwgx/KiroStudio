# kirostudio —— 项目背景（给 subagent 看）

本文件是**专门给 subagent 的项目背景**。主会话和 subagent 都应该先读这里再动手，避免每次从零摸索。

## 这是什么

**Anthropic 协议网关** —— 把 Anthropic Messages 请求转发到 Kiro / AWS Q，附带管理面板。
Rust（2024 edition）+ 前端 `admin-ui/`（pnpm 管理）。配置在 `config.json`（`config.example.json` 是模板）。

## 读文件顺序

1. **先读 `STATUS.md`（仓根）** —— 当下状态（HEAD、master 线、线上跑什么、待做什么）。
2. 再读 `CLAUDE.md` —— 长期约束（构建、验证、部署、设计原则、教训）。
3. 机器级板：`C:\Users\dwgx1\.dwgx\ops\boards\KiroStudio.md`（hmc 服务与未决清单）。

**不要**用旧 `HANDOFF-*` / `PLAN-*` / `TRACKING-*` 判断当前状态 —— 历史归档，含过期断言。

## 关键事实

- **唯一仓库 = 公开仓 `dwgx/KiroStudio`**（remote `origin`）。旧私有仓 `KiroStudio-skiapi` 已删（404）。
- **验证**：GitHub CI（push 任意分支跑 `ci.yml`）或 HMC Docker（`ssh hmc`，仓库 `Dockerfile` 的 builder target）。
  本机 Windows 也可构建（rustc 1.97.1 pin / node 24 / pnpm 11.7；先 `pnpm build` 出 `admin-ui/dist`）。
- **部署**：HMC 工作区 `kirostudio`；`E:\dwgxhomecloud\ops\kirostudio\deploy.sh` 幂等升级。
- 所有 `cargo` 命令 `--no-default-features`；`admin-ui/dist` 编译期嵌入（缺 dist 报 E0599）。
- 模型知识只改 `src/anthropic/model_catalog.rs`（单一真相源，带守卫测试）。
- 文档里的配置值/行数会过期，用前现读；凭据值只进 `E:\dwgxhomecloud-vault`，绝不 echo/写文件/进 commit。

## 给 subagent 的提醒

- 搜内容用 `rg`，找文件用 `fd`；读文件用 Read 工具。
- 写仓库文件前确认你不是唯一写手（本仓当前是干净克隆，正常 `git add/commit` 可用）。
- 结论必须能被盘上文件/命令输出复核；没跑过的标 `[UNVERIFIED]`。
