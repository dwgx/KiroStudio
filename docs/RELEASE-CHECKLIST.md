# 发版闸门（v1.1.2 之后）

> 2026-08-21. 本文件是「可以打下一个 tag」的标准，不是授权去打。
> 打 tag / 推 origin / 动 Homecloud 必须 Owner 点名。

当前产品树：脏 `59744cb` + quality-up 抽取 + 2026-08-21 P0/P1/P2。
公开 Latest 仍是 **v1.1.2**（`5f20596`）。下一版号未定。
本机闸门计数以 HANDOFF 为准（曾核 2247 passed）。

## 硬闸（缺一不可）

1. `cargo test --no-default-features` 本窗口实跑：`NNNN passed; 0 failed`。
   禁止用历史 2171 顶替。
2. 每个本窗口声称修复的项有**具名测试**且命令输出在 HANDOFF / SDD 报告里。
3. 脏树快照若要发：临时 `GIT_INDEX_FILE`，必须包含未跟踪 `src/` + `admin-ui/`。
4. `Cargo.toml` `version` == 将打的 tag（OTA 否则死循环）。见 `release.yml`。
5. 不把 `origin/master`（另一条历史）并进来。发版走 **tag**，不推本机脏 master，除非 Owner 点名。
6. 不推 skiapi。origin = 公开 `dwgx/KiroStudio`。
7. 不进包：凭据、`config.json`、`.agent/`、`.claude/`、`.opencode/`、`AGENTS.md`、`CLAUDE.md`。
8. `cargo --no-default-features` 出厂。不要用 default `native-tls` 当发布构建。

## 正确性清单（本窗口目标）

| ID | 必须在发版前 | 证据落点 |
|---|---|---|
| P0-1 Bug C | 映射工具流式不再 INVALID_TOOL_INPUT | named `bug_c_*` |
| P0-3 config/import | 不再恒 500 | named `import_config_*` |
| P0-4 quota 告警 | bump 在 `report_quota_exhausted` | named guard |
| P0-5 persist 串行 | persist_lock | named persist lock test |
| P0-6 thinking 块序 | sniff 路径先 stop | named sniff test |
| P1-7 / P1-9 | 403 不清 key；CSP frame-ancestors | named helper + CSP test |

P0-1/3–6 与 P1 代码已在脏树。P0-2 已观测：Homecloud **在燃**（1.1.2 + 默认 align）。发版前还要：本窗全量测试、未跟踪 src 进快照、Owner 点名 tag。部署 Homecloud 是另一句话。

## Actions

- 新增 push/PR `ci.yml` 后：公开仓 Actions 对这次快照分支可见红绿（仅当该 yml 已推）。
- 打 tag 后：linux musl / mac 双 / windows 四端 + sha256，与 v1.1.2 同形。
- OTA 默认仓已是 `dwgx/KiroStudio`。不要改回 skiapi。

## 冻结核

选号 12 键、absorb 循环结构、AIMD / `inboundRpmAuto`、sticky：发版波次不得改。
