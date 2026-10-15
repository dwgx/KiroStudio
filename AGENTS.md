# AGENTS.md（kirostudio）

本项目完整指引在 **`CLAUDE.md`**（opencode / Claude Code / omp 共用同一套约定）。
每次开始任务前：

1. 读 `STATUS.md`（仓根，当前状态快照入口）
2. 读 `CLAUDE.md`（长期约束：构建、验证、部署、踩坑）
3. 机器级板（hmc 服务与未决清单）：`C:\Users\dwgx1\.dwgx\ops\boards\KiroStudio.md`

> 历史蒸馏：`SESSION-DIGEST.md`（会话时间线/决策/死胡同）+ `WORKFLOW.md`（派发/收工/恢复）；均为本地维护件（未入 git，见 .gitignore）。

**硬约束（摘录，详见 CLAUDE.md）**：

- 所有 `cargo` 命令一律 `--no-default-features`；构建前先 `cd admin-ui && pnpm install --frozen-lockfile && pnpm build`（dist 是编译期嵌入，缺了 cargo 直接 E0599）
- 验证走 **HMC（Docker）** 或 **GitHub CI**（push 任意分支即跑 `ci.yml`）；本机 Windows 构建后置
- 仓库 = 公开仓 `dwgx/KiroStudio`（唯一 remote `origin`）；提交遵循 `CONTRIBUTING.md`（Conventional Commits，不写 AI 署名）
- 凭据值只进 `E:\dwgxhomecloud-vault`；值绝不输出/写文件/进 commit
- 部署：HMC 工作区 `kirostudio`，工具在 `E:\dwgxhomecloud\ops\kirostudio\`（deploy.sh 幂等）；**升级要改两处**（`workspace.yml` 的 image tag + `deploy.sh` 的 VERSION）；**降级需显式 `--allow-downgrade`**；机器规矩见 `E:\dwgxhomecloud\AGENTS.md`

**三条已发生过的坑（别重蹈）**：

- **`cargo test … | grep …` 会掩盖退出码**（曾因此推过编译红）。一律 `if cargo test … > log; then …` 显式门控。
- **新增 setter / 镜像 / 标记必须同时交出生产调用点 + 能触发它的守卫测试**——本仓高频残缺形态是「同一语义两道过滤器」「有函数无调用点」（改了不报错，只表现为「改了没用」）。
- **部署链必须验尺寸/哈希**：`-f`/magic 挡不住被截断的上传（宿主 /tmp 满 → scp 静默截断 32 KiB → 容器 `exec format error` 反复重启 = 2026-10-14 实际服务中断）。
