# 当前状态入口（Windows 2026-08-20 收尾）

> 活树：`D:\Project\kirostudio`。`D:\Macos\workspace\Project\kiro-rs` 是 ZyphrZero/kiro.rs，不是本产品。
>
> **读序：** 本文件 → `docs/TAKEOVER.md` → `.agent/HANDOFF.md`（进度，不进 git）→ `docs/README.md`。
> `.opencode/state.md` 的「CI 2109 / 已部署 w22」**不是现状**。nbus sha 未 ssh = unknown。

## 先看结论

| 项 | 值 |
|---|---|
| HEAD | `59744cb` W2–W14，`master` = `origin/master` |
| 版本 | Cargo.toml **1.1.2**（与 tag `v1.1.2` 对齐） |
| 工作树 | W15–W22 + 收口/吸收/调度。**不要 reset** |
| 本地冻包 | `backup/w15-w22-windows` = `911b914`（旧于当前脏树，仅保险） |
| 本机测试 | `cargo test --no-default-features` → **ok. 2170 passed; 0 failed**（最后一次全量；收尾波会再跑一遍写进 closeout） |
| 线上 | **unknown**（未 ssh nbus/skiapi） |
| 远程 | 只推 `origin` = `dwgx/KiroStudio-skiapi`。**禁止推 `public`**。Owner 已允许 `v1.1.2` tag（不推 master）。 |

## 证据分层

| 层级 | 可写 |
|---|---|
| git | `59744cb` 已推 origin |
| 脏树 | 产品在工作区；未跟踪必须进快照：`src/common/auth_keys.rs`、`src/kiro/auth/sso_token.rs`、`src/kiro/model/events/metadata.rs` |
| 设计稿 | `docs/client-key-design.md`、`docs/compat-upgrade-plan.md` 未实施完，以代码为准 |
| 线上 | 未现读 |

## 本机验证

- rustc 本机 1.96；CI `release.yml` 用 **1.97.1**。一律 `--no-default-features`（纯 rustls）。
- `admin-ui/dist` 在才能编。2026-08-20 已用 `corepack pnpm` frozen-lockfile + `pnpm build`，并重编 debug 二进制嵌入新 dist。无全局 pnpm（`corepack enable` EPERM）。
- 个人：设置 **智能**。中转（前面有 shield）：**稳定** 或 **手动且关 AIMD**。不要 sticky。

## 不进远程

`credentials.json`、`config.json`、`.agent/`、`.grok/`、`.tmp*`、`docs/archive/`、本地 db、密钥。`.opencode/*` 已跟踪属迁移债，新快照不要再扩。`.claude/state/` 已 ignore。

## 发版（只准备，未执行）

下一 tag 建议 **v1.1.2**（须改 Cargo.toml 同号）。CI：`.github/workflows/release.yml` 打 `v*` 后：

1. Ubuntu `cargo test --no-default-features`
2. Linux musl `kirostudio-linux-x86_64`
3. macOS aarch64 + x86_64
4. Windows `kirostudio-windows-x86_64.exe`

快照命令与排除名单见 `docs/TAKEOVER.md` §6。OTA 面板仍要 PAT 才有用。

## 未做（有意）

Client Key；自动加大全局并发 16；ssh/部署；推 master/public。
