# 当前状态入口（审计快照）

> 更新时间：2026-08-11（本次更新）。
>
> 本文件只记录已经核对过的边界：`HEAD`、未提交工作树、`deploy/vps`、线上只读观察、
> 以及设计/未验证项。需要继续接手时，读本文件后再读
> [`docs/TAKEOVER.md`](docs/TAKEOVER.md) 与 `.claude/state/CURRENT.md`。
> 根目录的 `HANDOFF-*`、`PLAN-*`、`TRACKING-*`、`OPEN-ISSUES-*` 和旧 `STATUS-*` 都是
> **历史档案**，不能覆盖本文件的当前结论。

## 先看结论

- **工作树状态**：约 **90+ 条 porcelain（全部未提交）**——本仓多会话并发，
  `git status --porcelain` 数字只对读取时刻有效，不是未来承诺。**任何提交/部署都需用户明确指示。**
- `HEAD` 为 `802fc1c`（CLAUDE.md / CONTEXT.md 入库）。`origin/master` 与本地一致。
- 版本号 `0.7.46`（Cargo.toml）；**`v0.7.46` 已于 2026-08-11 打 tag 并推送**（tag 指向快照
  commit aa35c26），release.yml 全 5 job 通过，Release 产物齐（含 Windows exe）。
- 分支：`master`（HEAD `802fc1c`）、`fix/macos-support-and-critical-bugs`、`deploy/vps`、
  `backup/worktree-snapshot`、临时 `ci/verify-adaptive`（CI 快照产物，不碰真实 index）、
  远程 `deploy/adaptive`（服务器裸仓，= aa35c26）。
- 已核对：`cargo test --no-default-features` **1697 passed / 0 failed**（skiapi Docker，
   见「本次实际验证」）；admin-ui tsc+build 通过。
- **2026-08-11 已部署**：线上容器 `local-aa35c26-20260811-000811`（healthy），
  快照源码三处改动特征已核对在线；前端 dist 已在服务器重建并随容器上线。

## 证据分层

| 层级 | 当前可写的结论 |
|---|---|
| `HEAD` | `802fc1c` 及其祖先为已提交代码。 |
| 工作树 | ~90+ 条未提交改动（含本会话 ~20 个源文件 + 文档；其余是历史会话的改动）。全部未提交。 |
| `deploy/vps` | 独立部署分支，不包含工作树改动。 |
| 线上 | **2026-08-11 热更新已部署** aa35c26（本地快照全量：/cc/v1 压缩重试、usage 双口径、前端 5 项等全部在线）。 |
| Release | `v0.7.46` tag = aa35c26；linux/macos/windows 产物齐（GitHub Release）。 |
| 设计/计划 | `docs/` 中 RFC、spec、plan 只能证明设计或研究存在，除非有当前代码和测试证据，不得写成已实现。 |

## 本次实际验证（2026-08-11，全部在 skiapi 服务器 Docker 跑）

- `docker build --target builder`：**编译通过**（含全部工作树改动）。
- `cargo test --no-default-features`：**1697 passed / 0 failed**（快照 aa35c26，含用户拍板三项修复）。
- 新测试按名单单跑确认：by_model 7、compress_retry_loop_cc 1、cache_fingerprint 13、
  native_effort 17、request_body_invalid 4、absorb_ 39。
- `docker build --target frontend-builder`（pnpm install + tsc + vite build）：**通过**。
- 多轮 @reviewer 对抗审查（详见 `.claude/state/CURRENT.md`），结论**可交付**。
- 本机（MacBook Air M2 / 8GB）**编不过**（缺 admin-ui/dist 与 node_modules），这是环境限制不是代码问题。

## 本轮工作树改动（2026-08-11，未提交）

完整清单与守卫清单见 `.claude/state/CURRENT.md`。摘要：

1. **P0-2 压缩死锁**：`compress_retry_target` 公式修正（3/4)^n 递减 + 90s 跨轮总预算（前会话）+ 响应头 strip + 守卫测试。
2. **P0-3/P0-4**：透传同号吸收重试（`passthrough_absorb_should_retry`）、入站闸门提到 handler 层（`try_inbound_admission_gate`，/v1 + /cc/v1 双入口）。
3. **P2 错误翻译**：`REQUEST_BODY_INVALID` / `Invalid tool use format` / `Improperly formed` → 400 `invalid_request_error`（原来 502 兜底或 502「凭据无效」误分类）；3 处收尾埋点补 error_message（闭合 38 条 NULL 盲区）。
4. **P3 接线**：absorb 3 字段（capacity_400 / swap_budget_secs / exhausted_status）+ nativeThinkingEffortEnabled 全链路（后端 + 前端 + i18n 三语 + 守卫）；注释漂移修复 8 处。
5. **P1 移植（用户拍板）**：
   - **native effort/thinking 映射**（`native_thinking_effort_enabled` 默认关）：`output_config.effort` 注入 + 白名单 4 模型 + XML 抑制共用判定。
   - **缓存 fingerprint 模拟器**（cache 链 Layer 3，纯内存）：`src/anthropic/cache_fingerprint.rs` 新增，最长公共前缀命中 + 会话隔离 + TTL。
6. **文档**：CLAUDE.md（验证循环更新 + P1 结论）、STATUS.md（本文件）、CURRENT.md、AGENTS.md（新建，opencode 接手入口）、CONTEXT.md。

## 已知遗留（当前代码里有 TODO / 半接线 / 未做）

- **P1 两项移植未线上实测**：native effort 的 reasoningContentEvent 触发、指纹命中率、error_message 盲区闭合——需要可用 ksk 号后才能验证。native effort 默认关是刻意的。
- **A-5/A-6 低危项（记录待复核，不修）**：429 换区后 403 绕圈最多浪费一跳；`select_endpoint` 备区只取 PROBE_ORDER 第一项（当前 2 项无实际影响，扩表前需改）。
- **D 类阈值**（RAMP_MIN_SAMPLES / MAX_REQUEST_RETRY_BUDGET_SECS / POOL_EXHAUSTED_RETRY_AFTER_SECS 等）：「健康时观测」定性存在，但无真实故障分布数据——**先修度量再调参**（项目铁律）。
- **upstream_trace 的 DROPPED/WRITTEN 计数器无消费点**：确认 trace 排障功能是否还在用后再决定接出口。
- **cache Layer3 fingerprint**：已移植（2026-08-11）。
- **bucket_id 接线需 provider ctx**（08-08 遗留）：`endpoint_buckets` 的 key 仍用 `name.to_string()`，需在 429 封桶写入点按 ctx 计算——未做，与 region 维度修复无关。
- **前端端点下拉未隐藏 codewhisperer/amazonq**（08-08 遗留，未做）。
- **OTA token 仍为空**（线上 `/etc/kirostudio/update.env`）：`v0.7.46` tag 已打（2026-08-11），
  但 OTA 按钮仍需填 token 才能用（本轮部署走的是 hotswap，不经 OTA）。

## 未验证、未做与阻塞

### 未验证
- 线上新二进制的**功能回归**（部署后只做了健康与快照源码核对，未打真实流量）。
- P1 移植、P2 翻译、error_message 修复在**真实流量**上的表现（无可用 ksk 号）。
- 前端视觉观感（tsc/build 通过，未做浏览器回归）。

### 未做（有意，有证据）
- 未改线上配置 / 未动号池 / 未动 OTA token。
- 未把工作树合入 `deploy/vps`；未删任何历史证据。

### 需要 owner 决策
- 是否填 OTA token（`v0.7.46` tag 已打，填 token 后面板 OTA 可用）。
- 是否开启 `native_thinking_effort_enabled`（默认关；建议上号实测后开）。
- websearch 整轮缓冲改造（设计文档已备，docs/websearch-buffering-rework.md，等上号实测）。

## 下一步（仅 owner 批准后执行）

1. 上号后：实测 native effort / 指纹命中率 / error_message 盲区闭合 / websearch TTFB，
   再决定开开关与 websearch 改造实施。
2. 若要 OTA 可用：填 `KIROSTUDIO_UPDATE_TOKEN`。
3. 前端 dist 更新流程已跑通（服务器 node:22-alpine 构建 + hotswap 前同步），后续部署照此执行。

## 历史档案入口

所有 `HANDOFF-*`、`HANDOFF-NEXT*`、`OPEN-ISSUES-*`、`STATUS-2026-08-05.md`、`PLAN-*`、
`TRACKING-*` 均保留为证据与推导材料。历史文档中的数字、线上状态、行号和「已上线/待修」
措辞必须先用本文件和当前代码重新核对。`HANDOFF-2026-08-08-CONSOLIDATED.md` 声称的
线上状态（已部署新机容器）与真实线上需另行核验后才能采信。
