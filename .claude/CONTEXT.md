# kirostudio —— 项目背景（给 subagent 看）

本文件是**专门给 subagent 的项目背景**。主会话和 subagent 都应该先读这里再动手，
避免每次从零摸索。读到的内容当作已确认事实，不要重新验证。

## 这是什么

**高性能 Anthropic 协议网关** —— 把 Anthropic Messages 请求转发到 Kiro / AWS Q，
附带一套现代化管理面板。Rust（2024 edition）+ 前端 `admin-ui/`（pnpm 管理）。
Docker 部署，配置在 `config.json`（`config.example.json` 是模板）。

## ⚠️ 读文件顺序（这个项目特别重要）

1. **先读 `STATUS.md`（仓根）** —— 当前状态快照的入口：`HEAD`、未提交工作树、
   线上跑什么、待做什么。这是**当下状态**。
2. 再读 `docs/TAKEOVER.md` —— 执行层交接。
3. `CLAUDE.md` —— 长期约束（推哪里、怎么构建、硬约束、历史事故依据）。

**不要**用 `HANDOFF-*` / `PLAN-*` / `TRACKING-*` / `OPEN-ISSUES-*` / 旧 `STATUS-*`
判断当前状态 —— 它们是**历史归档**，已确认含过期断言，价值只在推导过程。

## 关键坑

- **配置值记错过多次，用前一律现读** `ssh skiapi 'grep ... /opt/kirostudio/data/config.json'`，
  不要信文档里写的数字（`credentialRpmLimit` 记过 85/200，现读 100；`inboundTargetRpm`
  由 autotune 每 2 分钟自动调）。
- 本仓库**多会话并发**，`git status --porcelain` 数字只对读取时刻有效，不是承诺。
- 版本号 `v0.7.46` 但 tag 可能没打，OTA 升不到最新版 —— 判断发布前查 tag。

## 文档

`docs/` 下按主题分：架构、缓存 RFC、Windows 部署、模块等。动相关代码前翻对应文档。
`admin-ui/` 是 pnpm 管理的 React 前端，用 pnpm 别用 npm（会生成冲突锁文件）。

## 给 subagent 的提醒

- 有 `.codegraph/` 索引，先 `codegraph explore` 再 grep
- 搜内容用 `rg` 不用 `grep`，找文件用 `fd` 不用 `find`
- 读文件用 Read 工具不用 `cat`/`head`
- 涉及 key/secret 先读 `~/.claude/SECRETS.md`，值绝不 echo/写文件/进 commit
