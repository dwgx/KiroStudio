# 当前状态入口（审计快照）

> 更新时间：2026-08-07（本次审计；线上快照的服务器时间见下文）。
>
> 这份文件只记录已经核对过的边界：`HEAD`、未提交工作树、`deploy/vps`、线上只读观察、
> 以及设计/未验证项。需要继续接手时，读本文件后再读 [`docs/TAKEOVER.md`](docs/TAKEOVER.md)。
> 根目录的 `HANDOFF-*`、`PLAN-*`、`TRACKING-*`、`OPEN-ISSUES-*` 和旧 `STATUS-*` 都是
> **历史档案**，不能覆盖本文件的当前结论。

## 先看结论

- 当前分支是 `master`，`HEAD` 为 `a066fdf`。本次审计没有提交、切换、暂存、重置或改动真实 index。
- 审计开始时工作树有 **96 个已跟踪修改 + 72 个未跟踪路径 = 152 条 porcelain 状态**，其中包含大量他人源码/测试/文档改动。这个数字是审计起点，不是未来状态承诺。
- 审计期间并发出现根目录未跟踪的 `TAKEOVER-2026-08-07.md`；它不是本次审计的目标或依据，未被修改，需另行审阅后才能决定是否纳入入口。
- `deploy/vps` 当前指向 `495b770`，与 `master`/`HEAD` 不同；它不能证明 `master` 已包含这些改动。当前工作树又与 `deploy/vps` 存在差异，因此三者不可混写。
- 线上只读检查确认：服务 `active/running`，二进制版本字符串 `0.7.46`，服务器文件 mtime `2026-08-06 21:58`（服务器时区），SHA-256 为 `50b8bff18f7f218546db35026b278fb1f5a3938a95003ec5a61a5d565ddfbbdc`。无法从该二进制证明 Git commit，故不把它归因到 `d8255cf`、`495b770` 或历史档案中出现的 `97afaf0`（后者在本仓不存在）。
- 线上同一只读窗口的 `gateway-status brief` 为：`pool=5/5 cap=410RPM models=3/4 load=0.53 warn=1 crit=0`。这是快照，不是容量承诺，也不是本地改动已上线的证明。

## 证据分层

| 层级 | 当前可写的结论 |
|---|---|
| `HEAD` | 仅能把 `a066fdf` 及其祖先称为已提交代码。当前 `master` 没有本轮工作树改动。 |
| 工作树 | 本地源码/测试/文档确实存在，但未提交、未必完整，也未必与 `deploy/vps` 相同。 |
| `deploy/vps` | `495b770` 是独立部署分支上的提交；不能写成 `master` 已合入，也不能单凭 ref 写成线上已运行。 |
| 线上 | 只有本文件“线上只读检查”列出的服务、版本字符串、文件 hash、时间和 brief 输出被直接确认；功能行为与 Git provenance 未确认。 |
| 设计/计划 | `docs/` 中 RFC、spec、plan、matrix 只能证明设计或研究存在，除非有当前代码和测试证据，不得写成已实现。 |

## 本次实际验证

以下命令均在当前工作树运行，未做线上写操作：

- `cargo test --no-default-features`：**1361 passed / 0 failed**，耗时 64.89s。输出有 3 个重复 `#[test]` 属性警告及其他 unused/dead-code 警告。
- `cargo clippy --no-default-features`：退出码 0，但有 **228 条 warning**；因此不得写成“零警告”或“干净”。
- `cd admin-ui && pnpm exec tsc --noEmit`：退出码 0；pnpm 另提示 `package.json` 内旧式 `pnpm.onlyBuiltDependencies` 配置被忽略。
- `cd admin-ui && node --import ./tests/tsx-loader-register.mjs --test 'tests/*.test.ts'`：**37 passed / 0 failed**。

这些结果验证的是当前工作树在本机的编译/测试行为，不验证 `HEAD`、线上二进制、生产配置或视觉观感。

## 工作树中已存在且有本地代码/测试证据的改动（未称为已上线）

代表性证据如下，完整变更仍以 `git diff` 和未跟踪文件清单为准：

- 凭据克隆/分身：同 key 主份组回填、取同 key 快照先于回填、重复添加路径、节点分配与相关 Admin service/token manager 测试。
- 配置/刷新：`default_endpoint` restore 守卫、刷新写回逐字段合并、region probe 与 API region 处理。
- 错误出口：容量 400、吸收预算、`Retry-After`、图片 magic bytes、OpenAI 错误透传、`AuthTransient` 等当前源码路径有回归测试。
- 用量与 UI：`retries_sum` 出口、画布布局/火焰候选、行/画布组件、代理解析/SOCKS 节点编辑等当前工作树文件及测试存在。
- 测试卫生守卫：`no_orphan_tests_in_repo`、restart-only restore 映射守卫、共享 context usage predicate 守卫存在于当前工作树。

以上项目的正确标签是：**工作树已实现/本地已验证，尚未证明已提交到 master 或在线上生效**。其中画布、火焰密度、真实浏览器交互和生产行为没有本次视觉/线上验收。

## 未验证、未做与阻塞

### 未验证

- 当前线上二进制对应哪个 Git commit，以及它是否包含工作树或 `deploy/vps` 的具体修复。
- 线上功能回归：克隆主份不再重复显示、region 修复、Retry-After、吸收层、用量 retries、画布交互等。
- `pnpm build` / rust-embed 产物；本次只跑了 TypeScript 类型检查和 Node 测试。
- 浏览器视觉观感、WebGL 火焰 GPU 负载、真实外部 Kiro 端点、跨区/风控/429 生产数据。
- 历史 `OPEN-ISSUES-2026-08-06.md` 中标为“未验”的线上数据、缓存结论、容量推断和审计项。

### 未做

- 没有提交、push、触发 Actions、替换线上二进制、改线上配置或切换 Caddy/shield。
- 没有把工作树改动合并到 `master`；没有删除任何历史证据。
- 没有把 RFC/spec 中的 L3 cache、IP 池闭环、Codex `previous_response_id`、region 矩阵等设计写成完成项。

### 外部阻塞

- 线上 commit provenance 需要部署系统/构建产物的额外证据；本地 Git ref 和远端运行二进制 hash 不足以建立映射。
- 任何上线动作都需要 owner 明确部署范围、基准分支和回滚方案；本次请求只授权文档审计，因此没有执行。

### 需要 owner 决策

- 是否把当前工作树整理成部署候选，以及部署基准取 `master`、`deploy/vps` 还是另行审阅的显式路径集合。
- 是否进行 `origin: KIRO_CLI` 的单号实验；不要在没有实验设计和回滚窗口时全池切换。
- cache_read 的产品口径（关闭、保留并标注、或移植更重的真实计量）；以及是否放宽 `tool_result` 缓存红线。
- 主份是否走节点、父号裸 IP 告警是否保留、分身数量是否调整；这些会改变生产流量/风控，不由文档审计代决。
- region 手改控件放右键菜单还是详情对话框；这只是 UI 方案选择，不应误写为缺陷已修。

## 下一步（仅 owner 批准后执行）

1. 先保存新的只读 `git status --short --branch`，再用 `git diff --check`、`git diff --name-status HEAD`、`git diff --name-status deploy/vps` 明确候选范围。
2. 若要把前端作为部署候选，先运行 `cd admin-ui && pnpm install --frozen-lockfile && pnpm build`，确认 rust-embed 所需 `dist`，再复跑 Rust 测试/检查。
3. 只用临时 `GIT_INDEX_FILE` 和显式路径制作快照；不得对真实 index 使用 `add/commit/stash/reset/checkout/switch`，不得全仓 `cargo fmt`。
4. 部署前记录构建产物 SHA-256；部署后分别核对 systemd 状态、运行版本、二进制 hash、`gateway-status brief` 和目标功能回归。任何一项无法对应就停在“未确认”。

## 历史档案入口

所有 `HANDOFF-*`、`HANDOFF-NEXT*`、`OPEN-ISSUES-*`、`STATUS-2026-08-05.md`、`PLAN-*`、`TRACKING-*` 均保留为证据与推导材料，已经有 `HISTORICAL-ARCHIVE-MARK` 的文件继续保留，不删除、不改写成当前结论。历史文档中的数字、线上状态、行号和“已上线/待修”措辞必须先用本文件和当前代码重新核对。
