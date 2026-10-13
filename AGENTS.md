# AGENTS.md（kirostudio）

本项目完整指引在 **`CLAUDE.md`**（opencode / Claude Code / omp 共用同一套约定）。
每次开始任务前：

1. 读 `STATUS.md`（仓根，当前状态快照入口）
2. 读 `CLAUDE.md`（长期约束：构建、验证、部署、踩坑）
3. 机器级板（hmc 服务与未决清单）：`C:\Users\dwgx1\.dwgx\ops\boards\KiroStudio.md`

**硬约束（摘录，详见 CLAUDE.md）**：

- 所有 `cargo` 命令一律 `--no-default-features`；构建前先 `cd admin-ui && pnpm install --frozen-lockfile && pnpm build`（dist 是编译期嵌入，缺了 cargo 直接 E0599）
- 验证走 **HMC（Docker）** 或 **GitHub CI**（push 任意分支即跑 `ci.yml`）；本机 Windows 构建后置
- 仓库 = 公开仓 `dwgx/KiroStudio`（唯一 remote `origin`）；提交遵循 `CONTRIBUTING.md`（Conventional Commits，不写 AI 署名）
- 凭据值只进 `E:\dwgxhomecloud-vault`；值绝不输出/写文件/进 commit
- 部署：HMC 工作区 `kirostudio`，工具在 `E:\dwgxhomecloud\ops\kirostudio\`（deploy.sh 幂等）；机器规矩见 `E:\dwgxhomecloud\AGENTS.md`
