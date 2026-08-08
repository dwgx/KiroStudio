# 当前状态入口（审计快照）

> 更新时间：2026-08-08（本次更新）。
>
> 这份文件只记录已经核对过的边界：`HEAD`、未提交工作树、`deploy/vps`、线上只读观察、
> 以及设计/未验证项。需要继续接手时，读本文件后再读 [`docs/TAKEOVER.md`](docs/TAKEOVER.md)。
> 根目录的 `HANDOFF-*`、`PLAN-*`、`TRACKING-*`、`OPEN-ISSUES-*` 和旧 `STATUS-*` 都是
> **历史档案**，不能覆盖本文件的当前结论。

## 先看结论

- 当前分支是 `master`，`HEAD` 为 `64b0c7b`（clippy 修复），父提交 `6831b83`（全量修复汇聚，25 文件 +3866/−202）。`origin/master` 与本地一致。本次更新没有提交、切换、暂存、重置或改动真实 index。
- 工作树：本次核对时 **0 条 porcelain（干净）**——上一轮审计的 152 条未提交改动已全部合入 `master`，不再存在。⚠️ 但更新过程中并发会话又改了 `CHANGELOG.md`（+59 行）并新增未跟踪的 `HANDOFF-2026-08-08-CONSOLIDATED.md`，即工作树已不再为零；本仓库多会话并发，`git status --porcelain` 数字只对读取时刻有效，不是未来承诺。
- 版本号 `0.7.46`（Cargo.toml）；⚠️ **`v0.7.46` 仍未打 tag**，OTA 升不到这一版（tags 最高 `v0.7.45`）。
- 分支清单：`master`（主线，HEAD `64b0c7b`）、`consolidate/all`（合并记录，`6831b83`）、
  `deploy/vps`（独立部署分支，`495b770`，不含本轮改动）、`snapshot/live-endpoint-buckets`（生产快照，`42df1d2`，
  已是 master 祖先）、`fix/macos-support-and-critical-bugs`、`fix/region-inconclusive-and-clone-family`、
  `backup/worktree-snapshot`（`a36ce85`）。
- 归档 tag：`archive/prod-0e21f79`（cfb8ed9）、`archive/vps-2efcecb`（839800a）——生产 commit 归档。
- 已核对：`cargo test --no-default-features` **1533 passed / 0 failed**、clippy **0 error**、
  admin-ui tsc 干净、前端 Node 测试 **37 passed / 0 failed**（详见「本次实际验证」）。

## 证据分层

| 层级 | 当前可写的结论 |
|---|---|
| `HEAD` | `64b0c7b` 及其祖先为已提交代码；`6831b83` 已合入 master。`origin/master` 与本地一致。 |
| 工作树 | 核对时为 0 条 porcelain；更新期间并发会话又改了 `CHANGELOG.md` 并新增未跟踪 `HANDOFF-2026-08-08-CONSOLIDATED.md`，状态已漂移。 |
| `deploy/vps` | `495b770` 是独立部署分支，**未包含** `6831b83`/`64b0c7b`；不能写成 master 已含或线上已运行。 |
| `snapshot/live-endpoint-buckets` | `42df1d2` 是生产 hotswap 快照，已并入 master 链（6831b83 父提交）。 |
| 线上 | 上轮只读观察（版本字符串 0.7.46、文件 hash、mtime、brief 输出）**本轮未复验**；功能行为与 Git provenance 未确认。 |
| 设计/计划 | `docs/` 中 RFC、spec、plan、matrix 只能证明设计或研究存在，除非有当前代码和测试证据，不得写成已实现。 |

## 本次实际验证

以下命令均在当前工作树运行，未做线上写操作：

- `cargo test --no-default-features`：**1533 passed / 0 failed**，耗时 62.14s。
- `cargo clippy --no-default-features`：**退出码 0，0 error**，但有 **254 条 warning**；因此不得写成“零警告”或“干净”。
- `cd admin-ui && pnpm exec tsc --noEmit`：退出码 0。
- `cd admin-ui && node --import ./tests/tsx-loader-register.mjs --test 'tests/*.test.ts'`：**37 passed / 0 failed**。

这些结果验证的是当前 `HEAD` 在本机的编译/测试行为，不验证线上二进制、生产配置或视觉观感。

## 本轮已合入的改动（`6831b83` 全量修复汇聚）

以下均已提交到 `master`（`6831b83`，随后 `64b0c7b` 修 clippy），有代码与回归测试证据：

1. **会炸修复 3 条**（`passthrough_think_filter.rs`）
   - `passthrough_think_filter` 遇到非 object 的 panic 守卫
   - SSE 事件缓冲无空行长流时的 buf 上限（超限 fail-open 整块透传，不无界增长/卡死）
   - 事件分隔符取 `\n\n` 与 `\r\n\r\n` 的 **min**（混合行尾切在先出现处）
2. **vps 5 项移植**
   - `family_key` clone_group 收族 + `token_manager` 族级累加清零
   - `subscription_unsupported` 调用点
   - openai `plan_thinking` 门
   - `usage_handlers` PipelineHealth（合并既有 dropped 出口）
3. **k2cc 四层 cache 链**（`anthropic/cache.rs` + `metering.rs` + 用量统计）
   - metering 真值 + 5m/1h 拆分 + clamp + 入库真值/对外 0.6657
   - ⚠️ Layer 3 账号级指纹 **fingerprint 未移植，恒 None**（见「已知遗留」）
4. **deepseek 缺陷修复**
   - 图片降采样 `hard_max_pixels` / kill switch / GIF 帧数（`image_resize.rs`）
   - 白名单门序 `effective_model`（`deepseek_normalize.rs`）
   - `deducted_chars` 累计口径
   - 端点桶 region 维度 + `bucket_id`/`amz_target` 守卫（`endpoint/mod.rs`、`amazonq.rs`、`codewhisperer.rs`）
5. **重试上限 12→4 + 429 长退避**（`provider.rs`）
   - `ABSOLUTE_MAX_TOTAL_RETRIES = 4`（原 12）
   - `retry_delay_throttle`：429 专用 `1s → 2s → 4s → 8s`（上限 8s），把一次请求的上游调用摊开，尽早交还客户端

## 已知遗留（当前代码里有 TODO / 半接线）

- **bucket_id 接线需 provider ctx**：`endpoint/mod.rs` 已实现 `bucket_id(ctx)` + `amz_target()`，
  但 provider 侧 `endpoint_buckets` 的 key 仍用 `name.to_string()`（`provider.rs` 两处 `insert`），
  `select_endpoint` 处缺完整 `RequestContext`（无 token/machine_id）。需在 429 封桶写入点按当时 ctx 计算并同步读取键。
- **cache Layer3 fingerprint TODO**：`anthropic/cache.rs` 的账号级前缀指纹层未移植（k2cc `cache/fingerprint.rs`），恒传 `None`。
- **codewhisperer/amazonq 面板下拉未移除**：`ENDPOINT_NAMES` 已含两者，前端动态读 `config.endpointNames`
  自动多出端点按钮；如需从面板隐藏需额外处理。

## 未验证、未做与阻塞

### 未验证

- 线上当前二进制对应哪个 Git commit，以及它是否包含 `6831b83` 的具体修复（上轮只读观察本轮未复验）。
- 并发会话的 `HANDOFF-2026-08-08-CONSOLIDATED.md` 声称已用 `kirostudio-hotswap deploy` 部署到新机
  `143.20.230.62` 容器并退役旧机 `.248`——**本文件未复验该线上状态**，需 owner 或后续巡检确认。
- 生产功能回归：cache 真值链、重试上限 4、端点换桶、deepseek 修复、429 长退避在真实流量上的表现。
- `pnpm build` / rust-embed `dist` 重建（rust-embed 需 `admin-ui/dist`；本轮测试编译通过说明现有 dist 可用，但未重新构建）。
- 浏览器视觉观感、WebGL 火焰 GPU 负载、真实外部 Kiro 端点、跨区/风控/429 生产数据。
- 历史 `OPEN-ISSUES-2026-08-06.md` 中标为“未验”的线上数据、缓存结论、容量推断和审计项。

### 未做

- 没有 push、触发 Actions、替换线上二进制、改线上配置或切换 Caddy/shield。
- 没有把 `6831b83` 之后的改动合入 `deploy/vps`；没有删除任何历史证据。
- `v0.7.46` 未打 tag；没有把 RFC/spec 中尚未实现的 L3 cache 指纹、provider ctx 接线等写成完成项。

### 需要 owner 决策

- 是否给 `v0.7.46` 打 tag（OTA 升级的前置条件，且 `KIROSTUDIO_UPDATE_TOKEN` 为空时面板按钮必然失败）。
- provider 侧 bucket_id 接线（改动选号热路径 + 429 封桶写入点）是否本轮做。
- cache Layer3 账号级指纹是否移植（k2cc `cache/fingerprint.rs`）。
- 面板端点下拉是否隐藏 `codewhisperer`/`amazonq`。
- 部署基准取 `master`、`deploy/vps` 还是另行审阅的显式路径集合。

## 下一步（仅 owner 批准后执行）

1. 若要上线本轮改动：确认部署基准分支，用临时 `GIT_INDEX_FILE` 和显式路径制作快照，不得对真实 index 使用 `add/commit/stash/reset/checkout/switch`，不得全仓 `cargo fmt`。
2. 若要 OTA 可用：打 `v0.7.46` tag 并在线上 `/etc/kirostudio/update.env` 填入 fine-grained PAT。
3. 处理三个已知遗留前，先读 `endpoint/mod.rs` 的 `bucket_id` 注释与 `anthropic/cache.rs` 的 fingerprint TODO 原文，确认接线范围。
4. 部署前记录构建产物 SHA-256；部署后分别核对服务状态、运行版本、二进制 hash、`gateway-status brief` 和目标功能回归。任何一项无法对应就停在“未确认”。

## 历史档案入口

所有 `HANDOFF-*`、`HANDOFF-NEXT*`、`OPEN-ISSUES-*`、`STATUS-2026-08-05.md`、`PLAN-*`、`TRACKING-*` 均保留为证据与推导材料，已经有 `HISTORICAL-ARCHIVE-MARK` 的文件继续保留，不删除、不改写成当前结论。历史文档中的数字、线上状态、行号和“已上线/待修”措辞必须先用本文件和当前代码重新核对。

> 注意：`HANDOFF-2026-08-08-CONSOLIDATED.md` 是最新一份交接文档（并发会话新增、尚未提交），
> 声称 master 已部署到新机容器；其结论与线上状态需另行核验后才能采信。
