# kirostudio —— 项目背景（给 subagent 看）

本文件是**专门给 subagent 的项目背景**。主会话和 subagent 都应该先读这里再动手，
避免每次从零摸索。读到的内容当作已确认事实，不要重新验证。

## 这是什么

**高性能 Anthropic 协议网关** —— 把 Anthropic Messages 请求转发到 Kiro / AWS Q /
DeepSeek 系中转（custom_api 透传池），附带一套现代化管理面板。Rust（2024 edition）
+ 前端 `admin-ui/`（pnpm 管理）。版本 `1.1.1`。
部署：**nbus（38.244.34.15，ssh 别名 nbus，端口 52535）= systemd 二进制**（`/opt/kirostudio/kirostudio`，
前端内嵌，`:8990`，公网 api.dwgx.top）；skiapi（143.20.230.62:673）= Docker + 验证机。
**线上当前跑 W13 最终 build（sha 88270616，build_sha=final，healthz 实测 2026-08-16）**；
配置在 `/opt/kirostudio/config/config.json`（nbus）或 `config.json`（模板 `config.example.json`）。

## ⚠️ 读文件顺序（这个项目特别重要）

1. **先读 `STATUS.md`（仓根）** —— 当前状态快照的入口：`HEAD`、未提交工作树、
   线上跑什么、待做什么。这是**当下状态**。
2. 再读 `docs/TAKEOVER.md` —— 执行层交接。
3. `CLAUDE.md` —— 长期约束（推哪里、怎么构建、硬约束、历史事故依据）。
4. **任务状态**：`.opencode/state.md`（波次记录）+ `.opencode/ISSUES.md`（a-e 问题清单，
   含移植候选研究结论）+ `.opencode/DONE.md`（已完成工作+真实证据）。
5. **工作流规范**：`.opencode/WORKFLOW.md`（v2：两类权威分级/任务生命周期 9 步/派发纪律/守卫纪律/
   验证分级/落实核验/文档六件套/会话卫生——所有会话按它执行，不照搬参考仓定义）。
   研究依据：docs/workflow-research.md（联网研究 + origin 10-covenant 对照）。

**不要**用 `HANDOFF-*` / `PLAN-*` / `TRACKING-*` / `OPEN-ISSUES-*` / 旧 `STATUS-*`
判断当前状态 —— 它们是**历史归档**，已确认含过期断言，价值只在推导过程。

## 关键坑

- **配置值记错过多次，用前一律现读** `ssh nbus 'python3 -c ...'` 读
  `/opt/kirostudio/config/config.json`，不要信文档里写的数字。
- 本仓库**多会话并发**，`git status --porcelain` 数字只对读取时刻有效，不是承诺。
- 线上 4 个凭据全是 **custom_api 透传池**（没有 Kiro 池账号）：#1 fuckopencode
  （127.0.0.1:8788，deepseek 链）、#2 deepseekapi-dwgx（api.deepseek.com/anthropic）、
  #3 cursorapi（本机 8008）、#4 pigcode（cdn.pigcode.org，gpt 链）。透传路径**零转换**
  （thinking/DSML 过滤除外），cache 字段是上游原样（默认 0）——模拟缓存功能
  （mockCacheEnabled/mockCacheReadRatio）可配注入。

## 文档

`docs/` 下按主题分：架构、缓存 RFC、部署、参考仓库总结等。动相关代码前翻对应文档。
**会话全景报告（W1-W13）**：`docs/session-report-2026-08-15-16.md`（时间线/数字/交付物/遗留，
`STATUS.md` 的 docs 索引里有每份文件的时效标注）。
**性能实测**：`docs/PERFORMANCE.md`（nbus 实测硬证据 + README §8 草案 + P1/P2 建议，W13 产出）。
**参考仓库（只读）**：`/tmp/ref-zyphr`（ZyphrZero/kiro.rs v0.7.6，codegraph 已建）、
`/tmp/ref-k2cc`（TsinHzl/kiro2cc-proxy v2.9.6，codegraph 已建）——总结见
`docs/ref-ZyphrZero-kiro.rs.md` + `docs/ref-kiro2cc-proxy.md`。
`admin-ui/` 是 pnpm 管理的 React 前端，用 pnpm 别用 npm（会生成冲突锁文件）。

## 给 subagent 的提醒

- 有 `.codegraph/` 索引，先 `codegraph explore` 再 grep（参考仓也有索引）
- 搜内容用 `rg` 不用 `grep`，找文件用 `fd` 不用 `find`
- 读文件用 Read 工具不用 `cat`/`head`
- **验证**：本机编不过，必须走服务器 Docker「验证循环」（快照→scp→`docker build
  --target builder`→`docker run` 显式跑 `cargo test --no-default-features`），
  完整命令见 `CLAUDE.md`；`Dockerfile.verify` 已不存在，勿引用旧命令。
- **部署**：nbus 走「skiapi 构建 release 二进制 → 本机中转 → nbus 替换 + systemctl restart」
  （备份 .bak-前缀，校验 sha256 + healthz build_sha == 快照 commit）；skiapi 走 hotswap。
- 涉及 key/secret 先读 `~/.claude/SECRETS.md`，值绝不 echo/写文件/进 commit
