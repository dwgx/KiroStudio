# 接手（2026-08-20 收尾，下一窗从这里继续）

现状只信仓根 [`STATUS.md`](../STATUS.md)。进度 `.agent/HANDOFF.md`。整段提示词 `.agent/NEXT-PROMPT.md`。

档案（不承载结论）：`.opencode/state.md`、`.opencode/ISSUES.md`、`.agent/closeout-reports/`、`docs/archive/`。

## Git

```text
live:     D:\Project\kirostudio
HEAD:     59744cb   脏树。禁止 reset / checkout / stash / 全仓 fmt
origin:   https://github.com/dwgx/KiroStudio.git     ← 公开仓，推这里
skiapi:   dwgx/KiroStudio-skiapi                     ← 旧私有，push 已关
tag:      v1.1.2 → 5f20596（公开 Latest，四端齐）
quality:  origin/quality-up/after-v1.1.2
```

快照用临时 `GIT_INDEX_FILE`。不要 stage：凭据、`config.json`、`.agent/`、`.grok/`、`.claude/`、`AGENTS.md`。  
`git-ai-policy` 会挡 `AGENTS.md` / `CLAUDE.md`。推公开仓若被钩子拦：Owner 已允许时才设 `ALLOW_PUBLIC_PUSH=1`。

不要把公开仓 `master` 并进本脏树。发版靠 **tag**。

## 已收口

- 公开仓 v1.1.2 Release + Actions 三端（linux musl / mac 双 / windows）
- W1–W6 抽取 + 三处 fail-closed。本机 **2171 passed**
- Homecloud：1.1.2，OTA 走公开仓默认
- 失败 CI 记录已删；私有仓仓库网页未删

## 动手

`cargo` 一律 `--no-default-features`。无全局 pnpm：`corepack pnpm`，`$env:CI='true'`。  
选号 12 键、absorb 循环、AIMD、sticky 不要动。
