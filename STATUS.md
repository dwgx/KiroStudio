# 当前状态入口（Windows 2026-08-20）

> 活树：`D:\Project\kirostudio`。读序：本文件 → `docs/TAKEOVER.md` → `.agent/HANDOFF.md`。

## 先看结论

| 项 | 值 |
|---|---|
| 工作区 HEAD | `59744cb`。`master` 未因发版而移动 |
| 发版 | **v1.1.2** = `5f20596`。**origin 已切到公开仓 `dwgx/KiroStudio`。** 旧私有 `KiroStudio-skiapi` 弃用。 |
| Cargo | **1.1.2** |
| 本机测试 | `cargo test --no-default-features` → **2171 passed; 0 failed** |
| 质量提升 | W1–W6 抽取已落地；相对 tag 的快照推 `quality-up/after-v1.1.2`（不改 v1.1.2 tag、不推 master） |

## 发版产物（tag v1.1.2）

https://github.com/dwgx/KiroStudio/releases/tag/v1.1.2

- `kirostudio-linux-x86_64` + sha256
- `kirostudio-macos-aarch64` + sha256
- `kirostudio-macos-x86_64` + sha256
- `kirostudio-windows-x86_64.exe` + sha256

公开仓发版流水线：https://github.com/dwgx/KiroStudio/actions（tag `v1.1.2` 已推，产物以公开 Release 为准）

## 本机验证

- `cargo` 一律 `--no-default-features`。
- Chrome：设置 → 调度，智能 / 稳定 / 手动。
- 空号池流量烟测 66/66。

## 文档包

新窗：`STATUS.md` → `docs/TAKEOVER.md` → `.agent/HANDOFF.md` → `.agent/NEXT-PROMPT.md`。  
`.opencode/state.md` / `ISSUES.md` 已标档案。未入库设计稿在 `docs/archive/design-drafts/`（gitignore）。

## 不进远程

凭据、`config.json`、`.agent/`、`.grok/`、`.claude/`、`AGENTS.md`、密钥。禁止 `git push public`。不要 reset 脏树。
