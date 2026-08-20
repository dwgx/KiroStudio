# 接手（2026-08-20 Windows 收尾）

现状：仓根 [`STATUS.md`](../STATUS.md)。进度：`.agent/HANDOFF.md`。提示词：`.agent/NEXT-PROMPT.md`。

`.opencode/state.md`、`.opencode/ISSUES.md`、`.agent/closeout-reports/` **是档案**，不承载结论。

## Git

```text
live:     D:\Project\kirostudio
HEAD:     59744cb   master = origin/master
dirty:    是。禁止 reset / checkout / stash / 全仓 fmt
tag:      v1.1.2 → 5f20596
quality:  5a0e174 → origin/quality-up/after-v1.1.2
origin:   https://github.com/dwgx/KiroStudio.git  （公开仓。旧 skiapi 私有仓弃用）
```

快照用临时 `GIT_INDEX_FILE`。不要 stage：凭据、`config.json`、`.agent/`、`.grok/`、`.claude/`、`AGENTS.md`。不要推 `public` / gitee / master，除非 Owner 点名。不要改 tag `v1.1.2`。

## 已收口

- Release 三端产物齐（linux musl / mac 双架构 / windows exe）。
- W1–W6 抽取 + 三处 fail-closed。本机 **2171 passed**。
- Chrome 调度三档；空号池流量 66/66。

## 动手约束

`cargo` 一律 `--no-default-features`。选号 12 键、absorb 循环、AIMD 默认、sticky 不要动。无全局 pnpm：`corepack pnpm`，`$env:CI='true'`。
