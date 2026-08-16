# 当前接手说明（2026-08-16 W13 复核版）

本文件是执行层交接；当前状态入口是仓根 [`STATUS.md`](../STATUS.md)。
任务状态三件套：`.opencode/state.md`（波次）、`.opencode/ISSUES.md`（a-e 问题清单）、
`.opencode/DONE.md`（已完成+证据）；subagent 背景见 `.claude/CONTEXT.md`；
会话全景报告见 `docs/session-report-2026-08-15-16.md`。
根目录的 `HANDOFF-*`、`PLAN-*`、`TRACKING-*`、`OPEN-ISSUES-*` 和旧 `STATUS-*` 全部按历史档案处理：保留证据，不承载当前结论。

## 1. Git 边界（2026-08-16 复核）

```text
branch: master
HEAD: 513e7f0（2026-08-15 docs 交接快照；代码侧最后提交 1e100a2 严格语义修复）
master/origin/master: 513e7f0（已推，一致）
porcelain entries: 203 个未提交（多会话并发：W2-W13 全部工作）
```

- `HEAD` = `513e7f0`；当前状态一律以仓根 STATUS.md 为准。
- 工作树有 W2-W13 大量已 CI 验证的未提交改动；禁止裸 git 操作，快照走临时 index（CLAUDE.md）。
- 部署目标：**线上是 nbus（38.244.34.15:8990，systemd 二进制）**，skiapi（143.20.230.62:673）是验证机。

## 2. 线上边界（2026-08-16 实测）

```text
systemd: ActiveState=active, SubState=running
binary: /opt/kirostudio/kirostudio
sha256 前缀: 88270616（W13 最终 build）
healthz: {"build_sha":"final","config_loaded":true,"ok":true,"pool_count":4,
          "sqlite_writable":true,"version":"1.1.1"}
备份: /opt/kirostudio/kirostudio.bak-pre-final（前序 edf27204）
```

线上跑的是 W13 收尾 build（build_sha=final，2026-08-16 05:24 部署）。部署细节与判定标准见
`.claude/state/CURRENT.md`「如何验证与部署」节；4 通道状态见 STATUS.md「先看结论」。

## 3. 本地验证边界

**本机 8GB 编不过 Rust**，所有后端验证必须走 skiapi Docker 验证循环（CLAUDE.md 完整命令）：
快照（临时 index）→ `verify-snapshot.sh` → scp → docker build → 显式跑
`cargo test --no-default-features`。判定 `2020 passed; 0 failed`（当前基线）。
前端：`cd admin-ui && pnpm install && pnpm exec tsc --noEmit && node --test 'tests/*.test.ts'`
（53 测试）+ `pnpm build`。

## 4. 当前代码可确认的“完成”

- W1-W13 全部工作在工作树中（已 CI 验证，未提交）：51 项审计修复、模拟缓存、错误码可配置（42 key）、
  模型兼容层（通配符/路径段/统计对齐）、绊脚石 16 项 + 并发工程类、性能仪表盘、i18n 三语 2344 键。
- 线上 88270616 实测 sha256sum 一致 + healthz 全绿 + 4 通道 smoketest 通过（详见 DONE.md 十一/十二节）。
- 若需要精确范围，先看 `git status --porcelain` + `.opencode/state.md` 波次记录；不要引用旧 handoff
  的测试数、行号或线上数字。

## 5. 当前“未完成/未验证”清单

- **未提交**：W2-W13 全部改动仍未进 master（并发纪律，提交需 owner 批准走正式流程）。
- **待 owner 决策**：websearch 结构性缺陷（D/A/B/C 选型）、v1.1.2 发版、opencode 配置切换、
  gpt-5.6-sol 来源排查、native_thinking_effort_enabled（详见 STATUS.md「需要 owner 决策」）。
- **未验证**：微软 SSO 完整登录流程（回调已修，待用户实测）、流式路径性能基准（PERFORMANCE.md §10）、
  生图三层不通（路由 404 + catalog 400 + key 无 image 权限）。
- **未做（有意）**：OTA 未启用、upstream_trace 默认关、性能 P1/P2 建议（PERFORMANCE.md §9）。

## 6. 外部阻塞与 owner 决策

- websearch 结构性缺陷修复选项需 owner 拍板（D 补 Kiro 号零代码推荐 / A 判定前移 / B 降级转发 / C 大工程）。
- v1.1.2 发版需 owner 批准（release 产物落后线上全部 W2-W13 修复）。
- 部署需 owner 批准；部署后必须校验 sha256 + 产物 static-pie 形态 + healthz build_sha == 快照 commit
  （W12 踩过缓存坏产物坑）。

## 7. 安全下一步命令（只列，不代表本次执行）

```bash
git status --short --branch
git diff --check

cd admin-ui
pnpm install --frozen-lockfile
pnpm build
node --import ./tests/tsx-loader-register.mjs --test 'tests/*.test.ts'
```

后端验证/部署流程（临时 GIT_INDEX_FILE 快照、verify-snapshot.sh、skiapi Docker、nbus 替换 + restart、
build_sha 校验）见 `.claude/state/CURRENT.md`；禁止真实 index 写操作、全仓 `cargo fmt`、VPS 本地编译和
未经批准的线上配置变更。
