# AGENTS.md（kirostudio）

本项目完整指引在 **`CLAUDE.md`**（opencode 与 Claude Code 共用同一套约定）。
每次开始任务前：

1. 读 `STATUS.md`（仓根，当前状态快照入口）
2. 读 `.claude/state/CURRENT.md`（最近会话状态与源码级守卫清单）
3. 读 `CLAUDE.md`（长期约束：推哪里、构建、部署、踩坑）

**硬约束（摘录，详见 CLAUDE.md）**：
- 本机 8GB 编不过 Rust —— 验证必须走 `skiapi` 服务器 Docker「验证循环」（CLAUDE.md 有完整命令）
- 禁止 `git add/commit/checkout/stash/reset/fmt`（多会话并发工作树）；提交用临时 index 快照
- 所有 `cargo` 命令一律 `--no-default-features`
- 涉及凭证先读 `~/.claude/SECRETS.md`，值绝不输出/写文件/进 commit
- 改代码前先看 `.claude/state/CURRENT.md` 的「源码级守卫」清单（改文案/挪位置会故意红）
