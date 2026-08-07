# 当前接手说明（2026-08-07 审计版）

本文件是执行层交接；当前状态入口是仓根 [`STATUS.md`](../STATUS.md)。
根目录的 `HANDOFF-*`、`PLAN-*`、`TRACKING-*`、`OPEN-ISSUES-*` 和旧 `STATUS-*` 全部按历史档案处理：保留证据，不承载当前结论。

## 1. Git 边界

审计开始时的只读记录：

```text
branch: master
HEAD: a066fdf26c88591705d50b97fc2416e5c5434560
master/origin/master: a066fdf
deploy/vps: 495b7702e4e5be8e2d7746d889601b185b921297
porcelain entries: 152 = 96 tracked modifications + 72 untracked paths
```

这意味着：

- `HEAD` 只代表 `a066fdf`；当前源码/测试中的本轮变化不能称为 `master` 已完成。
- `deploy/vps` 的 `495b770` 是独立 ref，虽包含一批代码提交，但不等于线上已运行，也不等于当前工作树的完整快照。
- 当前工作树与 `deploy/vps` 仍有差异；接手者必须按显式路径审阅，不能直接把整个工作树当部署输入。
- 本次没有使用 `git add`、`commit`、`stash`、`reset`、`checkout`、`switch`，也没有触碰真实 index。后续任何快照都必须复用 `CLAUDE.md` 的临时 index 约束。

## 2. 线上边界

本次只读 SSH 观察到：

```text
systemd: ActiveState=active, SubState=running
binary: /opt/kirostudio/bin/kirostudio
version string: 0.7.46
mtime: 2026-08-06 21:58:19 (+08:00, server)
sha256: 50b8bff18f7f218546db35026b278fb1f5a3938a95003ec5a61a5d565ddfbbdc
gateway-status brief: pool=5/5 cap=410RPM models=3/4 load=0.53 warn=1 crit=0
```

只读观察没有证明：

- 该二进制来自哪个 Git commit；
- 它是否包含 `deploy/vps` 的 `495b770` 或当前工作树的任一未提交修复；
- 线上任何具体功能修复已生效；
- `cap` 是实测吞吐而不是配置推导值。

历史文档出现的 `97afaf0` 在当前 Git object database 中不存在，不能作为部署证据。`d8255cf` 存在于历史部署链，但本次没有取得线上 provenance，故只保留为历史 ref。

## 3. 本地验证边界

本次实际执行：

| 命令 | 结果 | 限定 |
|---|---|---|
| `cargo test --no-default-features` | 1361 passed / 0 failed，64.89s | 当前工作树；有 3 个重复 `#[test]` 警告及其他编译警告 |
| `cargo clippy --no-default-features` | exit 0 | 当前工作树；228 条 warning，不是零警告 |
| `cd admin-ui && pnpm exec tsc --noEmit` | exit 0 | pnpm 警告旧式 `onlyBuiltDependencies` 配置被忽略 |
| `cd admin-ui && node --import ./tests/tsx-loader-register.mjs --test 'tests/*.test.ts'` | 37 passed / 0 failed | 当前未跟踪测试目录；不是浏览器验收 |

没有执行：`pnpm build`、浏览器/视觉验收、真实上游端点探测、生产功能回归、部署、线上配置写入。

## 4. 当前代码可确认的“完成”

这里的“完成”只表示当前工作树有实现且本地测试通过，不表示已上线：

- 克隆/分身路径包含同 key 主份组回填与“先快照、后回填”的守卫测试。
- refresh/reload 路径包含逐字段写回、`default_endpoint` restore 以及 restart-only 字段映射守卫。
- region probe、API region 导入/展示/设置路径存在；`fetch_usage_limits_once`、探测错误分类等符号可在当前代码定位。
- Anthropic/OpenAI 错误映射、`Retry-After`、容量 400、图片 magic bytes、吸收层相关回归测试存在。
- 用量 DTO 暴露 `retries_sum`/`retried_requests`；前端 retries 视图及画布/行视图/代理解析/SOCKS 编辑相关文件和测试存在。
- `no_orphan_tests_in_repo`、restart-only restore 映射、共享 context usage predicate 守卫存在。

若需要精确范围，先看 `git diff --stat HEAD`、`git diff --stat deploy/vps`，再逐文件审阅；不要引用旧 handoff 的测试数、行号或线上数字。

## 5. 当前“未完成/未验证”清单

- **未提交/未上线**：上述工作树实现仍未证明进入 `master` 或线上二进制。
- **未验证**：线上 commit provenance、线上功能行为、浏览器视觉、WebGL GPU、真实 Kiro 端点/区域/风控/429 结果。
- **未做**：`pnpm build`、任何部署或配置变更、Caddy/shield 切换、历史证据清理。
- **设计未实现或未验收**：L3 cache 产品选择、IP 池闭环、Codex `previous_response_id`、完整 region 矩阵、若干 UI 方案和历史 open issues 中标为 `[未验]` 的生产推断。

## 6. 外部阻塞与 owner 决策

- 需要 owner 决定部署候选的基准（`master`、`deploy/vps` 或显式挑选路径），以及是否允许发布。
- `origin: KIRO_CLI` 只能做可回滚的单号实验，不能把设计建议写成全池完成。
- cache_read、tool_result 红线、主份是否使用节点、分身数量、裸 IP 告警、region 控件位置均需要 owner 选择。
- 线上 hash 与 Git commit 的映射需要 CI/部署记录或可核对的构建产物；本地 ref 不足以补齐这一事实。

## 7. 安全下一步命令（只列，不代表本次执行）

```bash
git status --short --branch
git diff --check
git diff --name-status HEAD
git diff --name-status deploy/vps

cd admin-ui
pnpm install --frozen-lockfile
pnpm build
cd ..
cargo test --no-default-features
cargo clippy --no-default-features
```

若 owner 批准发布，必须使用临时 `GIT_INDEX_FILE`、显式路径、构建产物 hash、Actions 构建和部署后只读核对；禁止真实 index 写操作、全仓 `cargo fmt`、VPS 本地编译和未经批准的线上配置/Caddy/shield 变更。
