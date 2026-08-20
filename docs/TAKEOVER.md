# 当前接手说明（2026-08-20 Windows）

执行层。状态入口是仓根 [`STATUS.md`](../STATUS.md)。进度 `.agent/HANDOFF.md`。收口板 `.agent/CLOSEOUT.md`。
过程记录 `.opencode/state.md` / `ISSUES.md` / `DONE.md` 是历史+脏叙事，结论以 STATUS 为准。
`.claude/state/CURRENT.md` 的 W13 / nbus `88270616` 段是 **2026-08-16 历史**；后半守卫名单仍要用。

## 1. Git 边界

```text
live:   D:\Project\kirostudio
branch: master = origin/master   (0/0)
HEAD:   59744cb  W2-W14 milestone
freeze: backup/w15-w22-windows = 911b914   (local only; not master; not pushed)
dirty:  W15-W22 product stack + closeout T0-T4 hunks
        src + admin-ui + untracked auth_keys.rs, sso_token.rs
        + untracked docs/client-key-design.md, docs/compat-upgrade-plan.md
remote: origin = https://github.com/dwgx/KiroStudio-skiapi.git
```

- 禁止 `git checkout` / `stash` / `reset` / 全仓 `fmt`（会冲掉别人的脏树）。
- 提交必须 Owner 点名。默认用临时 `GIT_INDEX_FILE` 快照，不碰真实 index。
- 不要 stage：`credentials.json`、`config.json`、`.agent/`、`.grok/`、`.claude/`、凭据备份、本地 db。
- `.opencode/*` 已跟踪属迁移债；新提交尽量不要再扩。
- 本仓 `.git/config` 已设 `core.filemode=false`。
- 本切片 **无 push、无部署**。冻包 `911b914` 不要当成已上线。

## 2. 线上边界

2026-08-20 **未 ssh** nbus / skiapi。nbus sha / `build_sha` / pool = **unknown**。
旧数字（nbus `88270616`、`build_sha=final`，脏 state 的 CI 2109/0 与 w18/w19/w22 部署）**不是现状**。
部署目标仍是：**nbus = 生产 systemd 二进制**，**skiapi = 验证机**。未授权不 ssh。

## 3. 本地验证边界（Windows）

- rustc **1.96.0**（CI release 用 1.97.1）。`admin-ui/dist` 在。`cargo` 一律 `--no-default-features`。
- 全量：`cargo test --no-default-features` → **ok. 2170 passed; 0 failed**（收尾波会再跑，以 closeout `test-t5-full.md` 为准）。
- 前端：本机无 `pnpm`。tsc 未复跑；发版 CI 会 `pnpm build`。
- skiapi Docker / nbus 部署必须 Owner 点名。
- Mac 8GB /「只走 skiapi」**不是这台 Windows**。

## 4. 平台对照

| 项 | Mac（已结束） | Windows（当前） |
|---|---|---|
| 路径 | `/Users/dwgx/Documents/WorkSpace/Project/kirostudio` | `D:\Project\kirostudio` |
| 主写 | OpenCode + Claude | Grok 编排；大改可交 Cursor |
| 后端编 | 8GB 编不过，只走 skiapi | 本机 check/test 二进制已能编；T5 待跑 |
| Mac 镜像 | — | `D:\Macos\workspace\Project\kirostudio` 不存在，不要新建从 `D:\Project` 打出的 junction |

## 5. 完成 / 未完成

- **已进 git**：W2–W14（`59744cb`）。
- **冻包过时**：`911b914` 早于吸收/调度收口，不要当当前树。
- **在脏树**：W15–W22 + 吸收（conversationId / websearch 失败埋点 / OAuth 不走 CLI / MCP 按号负缓存 / 上号解析 / stopReason）+ 调度（ConcurrencyFull 纯代挂短等、封桶 429+RA、非流式 metadata、设置三档）。
- **未验证**：线上 sha、SSO 实登、MCP 无号活流量、`tool_reference`、前端 tsc。
- **有意未做**：Client Key、加大全局 16、部署 nbus。已知 3 条 review bug 未修（MCP 不轮换 / websearch decode_round / SSO 空 refresh）。

## 6. 打包 / tag（准备，未执行）

禁止真实 `git add`。快照用临时 index。**不要** `git push public`。**不要** 在 Cargo.toml 仍是 1.1.1 时打 `v1.1.2`（OTA 死循环，CI 也会拒）。

**进快照：** `src/`（含未跟踪 `auth_keys.rs` `sso_token.rs` `metadata.rs`）、`admin-ui/`、`build.rs`、`Dockerfile`、`Cargo.toml`、`CHANGELOG.md`、`STATUS.md`、`docs/README.md`、`docs/TAKEOVER.md`、设计稿 `docs/client-key-design.md` `docs/compat-upgrade-plan.md`。

**不准进：** `credentials.json` `config.json` `.agent/` `.grok/` `.tmp*` `docs/archive/` `.opencode/` 扩写、密钥、本地 db、`*.log`。

```text
# PowerShell 示意（Owner 点名后再跑；GIT_INDEX_FILE 用临时路径）
$env:GIT_INDEX_FILE = "$env:TEMP\ks-snap.index"
Remove-Item $env:GIT_INDEX_FILE -ErrorAction SilentlyContinue
git read-tree HEAD
git add -A -- src admin-ui build.rs Dockerfile Cargo.toml CHANGELOG.md STATUS.md docs/README.md docs/TAKEOVER.md docs/client-key-design.md docs/compat-upgrade-plan.md
# 核：git ls-files --others --exclude-standard -- src admin-ui 必须空
# TREE=$(git write-tree); 再 commit-tree + 分支，不碰真实 index
# tag v1.1.2 且 Cargo.toml version=1.1.2 后 push origin tag → 触发 release.yml
Remove-Item Env:GIT_INDEX_FILE
```

发 tag 后 CI：Ubuntu 测试门禁 → Linux musl + macOS 双架构 + Windows exe。本机 smoketest 不能替代 CI 三端。

## 7. 安全下一步

不要 `reset` 脏树。下一动作只有 Owner 点名的那件：bump+tag、或 ssh、或实号烟测。
