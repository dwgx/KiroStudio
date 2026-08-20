# 当前接手说明（2026-08-20 Windows）

状态入口 [`STATUS.md`](../STATUS.md)。进度 `.agent/HANDOFF.md`。

## Git

```text
live:   D:\Project\kirostudio
HEAD:   59744cb  (master = origin/master)
tag:    v1.1.2 → 5f20596  (origin；Actions 三端绿)
dirty:  是。不要 reset
origin: https://github.com/dwgx/KiroStudio-skiapi.git
```

- 禁止 `checkout` / `stash` / `reset` / 全仓 `fmt`。
- 快照用临时 `GIT_INDEX_FILE`。不要 stage 凭据、`config.json`、`.agent/`、`.grok/`、`.claude/`、`AGENTS.md`。
- 不要推 `public` / gitee / master，除非 Owner 点名。

## 已完成

- tag **v1.1.2** 三端产物已上 Release。
- W1–W6 神文件抽取 + MCP 同请求换号 / websearch metadata / SSO 空 refresh 拒入池。本机 **2171 passed**。
- 调度三档烟测、空号池流量 66/66。

## 约束

选号 12 键、absorb 循环、AIMD 默认、sticky 不要动。`cargo --no-default-features`。
