# 当前状态入口（Windows 2026-08-20 收尾）

> 活树：`D:\Project\kirostudio`。  
> 读序：本文件 → `docs/TAKEOVER.md` → `.agent/HANDOFF.md` → `.agent/NEXT-PROMPT.md`。

下一窗从这里接着干。`.opencode/state.md` 不是现状。

## 先看结论

| 项 | 值 |
|---|---|
| 活树 HEAD | `59744cb`，工作树仍脏。**不要 reset** |
| origin | **公开仓** `https://github.com/dwgx/KiroStudio.git` |
| 旧私有 | `dwgx/KiroStudio-skiapi`（remote `skiapi`，push 已关）。不要再推 |
| 发版 | **v1.1.2** = `5f20596`。公开 Release **Latest**，四端资产齐 |
| Cargo | **1.1.2** |
| 本机测试 | `cargo test --no-default-features` → **2171 passed; 0 failed** |
| 质量提升 | 分支 `quality-up/after-v1.1.2`（相对 tag 的抽取快照）。**没并进 master、没改 tag** |
| Homecloud | 二进制已是 1.1.2；无 `.git`；OTA 默认公开仓 |

## 发版产物

https://github.com/dwgx/KiroStudio/releases/tag/v1.1.2

- `kirostudio-linux-x86_64`
- `kirostudio-macos-aarch64`
- `kirostudio-macos-x86_64`
- `kirostudio-windows-x86_64.exe`

（各带 sha256。）公开仓 Actions 已绿。OTA 默认问这个仓，面板「当前 / 最新」应对齐到 1.1.2。

## 本机已做过（不要重做）

- 调度三档烟测（智能 / 稳定 / 手动）
- 空号池流量 66/66
- W1–W6 神文件抽取 + 独立 review
- MCP 同请求换号、websearch `metadataEvent`、SSO 空 refresh 拒入池
- 失败 Actions run 已清（仓库没删）

## 约束

`cargo --no-default-features`。选号 12 键、absorb 循环、AIMD 默认、sticky 不要动。  
不要推 `skiapi` / gitee / 本地 `master`，除非 Owner 点名。不要改 tag `v1.1.2`。  
不要 `git pull origin master`（公开仓 master 与本地 59744cb 不是同一条线）。

不进远程：凭据、`config.json`、`.agent/`、`.grok/`、`.claude/`、`AGENTS.md`、密钥。
