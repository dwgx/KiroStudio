# 发版闸门（v1.2.4 之后）

> 2026-10-13 复活线改写。本文件是「可以打下一个 tag」的标准，**不是授权**——打 tag / 推 tag 要 Owner 点名。

公开 Latest：**v1.2.4**（`d9dd93f`）。已发 tag 不要重打、不要 retarget（OTA 比较 tag 与 `CARGO_PKG_VERSION`）。

## 硬闸（缺一不可）

1. `cargo test --no-default-features` **本窗口实跑**：`NNNN passed; 0 failed`（HMC Docker 或 GitHub CI；禁止用历史计数顶替）。
2. 每个声称修复的项有**具名测试**，命令输出留档。
3. `Cargo.toml` `version` == 将打的 tag（否则 OTA 死循环；`release.yml` 有门禁）。
4. 出厂构建一律 `--no-default-features`（纯 rustls）；前端 `admin-ui/dist` 必须先构建（编译期嵌入）。
5. 不进包：凭据、`config.json`、`.claude/`、`AGENTS.md`、`CLAUDE.md`。
6. `release.yml` 产物与 v1.2.4 同形：linux-x86_64 / macos-aarch64 / macos-x86_64 / windows-x86_64 + 各自 `.sha256`。

## 发版动作

1. 全量测试绿（本窗口实跑）。
2. `Cargo.toml` 版本 = tag 名；提交。
3. 打 annotated tag 并 push（`release.yml` 自动出四端产物）。
4. 更新 `STATUS.md` 的 Latest 行 + `CHANGELOG.md`。
5. 部署侧跟进：改 `E:\dwgxhomecloud\ops\kirostudio\` 的 image tag + VERSION → 重跑 deploy.sh（见 `STATUS.md`）。

## 冻结核

选号 12 键、absorb 循环结构、AIMD / `inboundRpmAuto`、sticky、`model_catalog.rs` 的单一真相源地位：
发版波次不得顺手改。
