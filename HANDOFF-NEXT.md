# KiroStudio 接手计划书

> 面向下一个接手的 AI。**先完整读这份，再动任何代码。**
> 配套文档：`MASTERPLAN-2026-07-29.md`（7 个批次的完整实施方案，含改动清单/测试/验证/回滚）。
> 上一份交接 `HANDOFF-2026-07-29.md` 已被本文取代，但其中「本轮错误」一节仍值得读。

---

## ⚠️ 本文已被取代（标注日期 2026-08-03）

**本文写于 2026-07-29，已被 `HANDOFF-2026-07-31.md` + `HANDOFF-NEXT-TASKS.md` 取代。
不要照本文的「批 A–E」开工 —— 其中多数已上线。** 正文一律保留（实测数字与教训是这个仓库
最值钱的部分），只在此处与各批次标题处加状态标注。

**当前有效文档顺序**：`HANDOFF-2026-07-31.md` → `HANDOFF-NEXT-TASKS.md` → `docs/CACHE-EXP0-RESULT.md`。

| 批次 | 状态（2026-08-03） | 依据 |
|---|---|---|
| 批 A · 余额查询超时降级 | ✅ 已上线 | 0.7.45 第 9/10 条：6s 超时返 `stale` + 本地 `total_credits_used` 乐观修正 |
| 批 A · `p_avail` 批量化 | ❌ **明确不做** | 无实测支撑；碰选号热路径（错了掀翻分流）；行为不可观测故构造不出「移除即失败」的测试 |
| 批 A · usage/trace 复杂度核查 | ✅ 已上线 | 0.7.45 第 11/12 条：`by_model` 有界化（128/256 + `(other)` 桶）、`MAX_RECENT_LIMIT` 50000→2000（实测 42ms 持锁会让用量写侧排满 10000 后静默丢记录） |
| 批 A · SSE `stream_live` 改 O(1) | ⬜ 未做 | 方案已细化并**改推荐 `watch` 而非 `broadcast`**（live 帧语义是"只要最新的"，watch 无 `Lagged`）。见 07-31 §3.3 |
| 批 B · 多选 + 强制删除 | ✅ 已上线 | 0.7.45 第 18–20 条。D1 按本文推荐落地：`force` 绕禁用门但**仍进回收站** |
| 批 C · OTA 一键升级 | ✅ 已打通 | 0.7.45 起 `{"has_update":false,...,"error":null}`。D2 按本文推荐走 Release。PAT 已写入 `/etc/kirostudio/update.env`（⚠️ 是 `gho_` 型，`gh auth login` 会轮换它）。已知缺陷 #2 的 chunked 绕过也同批修掉（三处响应体读取收口 `common::http_read`） |
| 批 D · `first_token_ms` 埋点 | ✅ 已上线并验证有值 | 本文的「疑似从不写入」**已确证**：全仓 0 个生产赋值点、线上 24h 59458 条全 NULL。0.7.45 打在 `process_kiro_event` + `generate_final_events` 兜底 |
| 批 D · 用量/额度动态刷新（前端） | 🔶 部分 | 余额乐观修正已做；面板实时刷新仍未做 |
| 批 E · cache EXP-1/EXP-2 | ⬜ 未做（前置已解除） | 阻塞它的埋点缺失已修，现在只差线上样本积累。先读 `docs/CACHE-EXP0-RESULT.md` |
| §5 建议补做的 G1 元测试 | ✅ 已落地 | 0.7.45 第 16 条。⚠️ 但**注入时钟未做**，现有测试仍在回拨 `last_decay_at`（本文 §5 规则 1 的过渡态尚未清理） |

**本文正文中已确认过期或错误的具体点**（正文不改，读到时按此为准）：

- **版本**：正文的 `bfc24ab` / 0.7.44 时代早已过去 —— 线上与 `Cargo.toml` 均为 **0.7.46**。
- **测试数**：`931`（§2、§6）过期。**不要相信任何写死的测试数**，以 `cargo test` 实跑为准
  （每轮都在涨，7-31 那份写的 966/969 同样已过期）。
- **未提交条目数**：`~55`（§0）已涨到 **71**，多会话并行写的风险比当时更高。
- **SSH（§6）**：「只能用密码」**已过期，密钥登录已恢复**，`ssh ws-vps` 直接可用。
  `sshpass` 那段留作故障时的备用手段。
- **线上配置表（§7）两处真值反了**：`inboundThrottleEnabled` 线上是 **true**（`inboundTargetRpm=133`，
  由 `throttle-autotune.timer` 每 2 分钟自动调）、`cooldownEnabled` 线上是 **false**。
  同表 `ccAutoBuffer=false` 也已改为 **true**，且该项**两条实测证据链互相矛盾、需用户拍板**，
  不要自行改任一方向。
- **`credentialRpmLimit`**：正文未列，现值 200 —— 见下面「容量口径是假的」。

### 本文写完之后新增的四条关键事实

1. **403「temporarily suspended」占失败面的大头**：线上近 2h outcome 为
   success 59.5% / **auth_failed 22.3%** / rate_limited 17.2% / server_error 0.8%。
   全部 auth_failed 都是 `Your User ID (...) temporarily is suspended`，且是**突发**形态
   （13:50 一次 928 条、14:50 一次 516 条，中间为 0）= 风控窗口开合，**不是真封号**。
   主线已把它分类为 429 + `Retry-After: 20`（原先落 403 → 客户端与外挂都不重试）。
2. **外挂 shield 的性价比已量化**：`kiro_shield.py`（239 行，挂在 Caddy 与 KiroStudio 之间）
   累计 requests 22448 / retries 19226 / absorbed 1657 / gave_up 325 →
   **11.6 次重试才救回 1 个请求**。且它每次是整请求重打 KiroStudio，而网关内部还有 12 次换号
   → 真实放大上限 60×12。用户诉求是把这个能力**融入内置**且不与既有调度开关冲突。
3. **🔴 容量口径是假的**：`throttle-autotune` 日志「池容量 167，可用 1 个」中的 167 来自
   `credentialRpmLimit=200 × rpmHeadroomFactor 85%`，是**配置算出来的数**而非实测。
   实测单号 RPM 峰值 144、其中 17.2% 是 rate_limited；文档里的「干净吞吐 25~30」是
   *429 率为 0 时* 的 RPM。所以整形设在 133 实际什么都没限住，真瓶颈是上游 429。
   → 用户抱怨「没法调」的根源不是选项多，而是**一个关键数字是假的**，所有依赖它的自动调节在算空气。
4. **失败记录的 `retries` 恒为 0（已修）**：所有失败 outcome 无一例外 `retries=0`
   （auth_failed 1487 / rate_limited 1098 / server_error 118 / bad_request 91），
   同期 success 有 retries=1 —— 即**重试次数只在成功路径落库**，失败面完全看不见。
   主线已在 `provider.rs` 加循环外计数器写入 `fail_record.retries`。
   ⚠️ 仍未做：`Aggregate`（`usage_stats.rs`）无任何 retry 字段、`add` 不读 `r.retries`
   → retries 只能逐条看，无法聚合画趋势；admission 超时那条 bail 既不 `emit_record`
   也不 bump 计数器 → **面板完全隐形**。

---

## 0. 六条硬约束（违反会造成真实损失）

1. **工作树有 ~55 个未提交文件，属多个并行会话。**（2026-08-03：已涨到 **71**，风险更高。
   另记一条后来发生的真实损失：有会话用 `git checkout-index -f` "还原"文件，
   抹掉了别人在 `health.rs` 里的 **515 行**未提交改动。备份只用 `cp` 到 `/tmp`。）
   禁止 `git checkout` / `switch` / `stash` / `reset` / `commit` / `add`，禁止全仓 `cargo fmt`
   （历史事故：有会话跑了全仓 fmt，冲掉别人整树改动）。
   提交/推送用 git plumbing 在临时 index 做快照，**基树必须是 `origin/deploy/vps`**
   （不是 HEAD —— HEAD 缺少其它会话新增的 API，单独取我的文件会编译报 22 个错）：
   ```bash
   export GIT_INDEX_FILE=/tmp/snap.index && rm -f "$GIT_INDEX_FILE"
   git read-tree origin/deploy/vps
   git add -A -- src admin-ui/src Cargo.toml Cargo.lock
   TREE=$(git write-tree); C=$(git commit-tree $TREE -p origin/deploy/vps -m "...")
   git branch -f deploy/vps $C && unset GIT_INDEX_FILE
   git push -f origin deploy/vps
   ```
   做完必须核对：分支名与 `git status --porcelain | wc -l` 与开始时一致。

2. **不要在 VPS 上编译。** 4 核是瓶颈，会抢死正在服务的 sub2api。
   走 `gh workflow run deploy-build.yml --ref deploy/vps -f run_tests=true`（musl static-pie）。

3. **`origin` = `dwgx/KiroStudio-skiapi`（private，唯一开发仓库）。
   `public` = `dwgx/KiroStudio`（PUBLIC，已冻结）。** 显式写 remote 名时务必确认不是 `public`。

4. **禁止用脚本做批量代码编辑。** 本轮我两次因此出事：
   一次 python 替换把 `on_success`/`on_429`/`report_family_suspicious` 三个函数整段删掉；
   一次正则把字段插进了 `impl` 块，造成 209 个编译错误。
   改代码用 Edit 工具逐处改，或改前先备份到 `/tmp` 再逐步验证。

5. **不要做没有证据支撑的改动。** 用户明确抱怨过这一点。
   本轮我加过一个 `tie_break_jitter` 随机键（基于 52 个样本的误判），
   后来用隔离测试证明它不必要、已删除。判据是：**能否构造出"移除它即失败"的测试**。
   构造不出来就不要留。

6. **每条改动都要有能抓住旧 bug 的回归测试。** 抓不住的测试等于没写。
   验证方法：临时把修复处改回旧行为，跑测试，**必须 FAILED**，再还原。
   本轮 8 个新测试全部这样验过。

---

## 1. 线上现状（2026-07-29 部署后实测）

**部署**：commit `bfc24ab`，二进制 sha256 见 CI 产物。服务 `active`，11 个凭据。

| 指标 | 值 |
|---|---|
| 3min / 60s 成功率 | **100%** |
| 10min 成功率 | 98.5% |
| 延迟 p50 / p90 / p99 | **2647 / 13850 / 27310 ms** |
| 429 频率 | 约 50 次/10min（部署前 747 次/10min） |
| T2 坏档号 / 冷却 / 墙钟超时 / 自动禁用 / panic | **全 0** |
| load1 / 内存可用 | 0.03 / 6362MB |

**分流**：gini 0.431、最热最冷 12x —— **这不是缺陷**。两个会话占 83% 流量
（126 + 78），亲和把它们钉在 #254/#252 上，数字精确对应（108/71）。
其余 7 个号分摊剩余 17%，相当均匀（23/20/19/18/12/9）。
亲和保 prompt cache（每请求成本差 10 倍）的代价就是负载偏斜，这是刻意权衡。

**停机窗口**：74s → **10s**。已加 systemd drop-in
`/etc/systemd/system/kirostudio.service.d/20-stop-timeout.conf`（`TimeoutStopSec=10`），
已镜像到 `ws-vps/config/`。应用侧另有 8s 自退（`shutdown_with_drain_cap`），
drop-in 是它的安全网。

---

## 2. 已完成并上线（当时 931 tests 全绿 / clippy 0 error / tsc 干净）

> ⚠️ 931 是 2026-07-29 的快照，早已过期。**测试数每轮都涨，一律以 `cargo test` 实跑为准。**

### 后端

| # | 改动 | 修的是什么 |
|---|---|---|
| 1 | `decay_penalties` 取代失效的 `decay_idle` | 上一版用 `now - last_touch` 计时并设 5s 门槛，而 `last_touch` 会被**选号读 `p_avail`** 刷新（对每个候选都读）→ 饥饿号"空闲时长"恒为 0 → 每次在门槛处 return。实测旧实现 200 轮选号后 `ewma_429: 0.875→0.875`，**一次未衰减**。现改独立时钟 `last_decay_at` + 去门槛连续衰减 |
| 2 | 离散字段小数进位 `decay_carry` | `(dt/60) as u32` 在热路径上恒为 0（单次 dt 仅微秒）→ `consecutive_429`/`open_count`/`admit_prob_seed` 仍永不衰减，零碎时间被丢弃。**这是 review 自己抓到的、与 #1 同类的缺陷** |
| 3 | 覆盖全部三个单向棘轮 | 原先只碰 EWMA 两项。补 `open_count` 与 `admit_prob_seed` —— 死锁链：403 → `report_family_suspicious` 对**临时态**无条件 `open_count++` → 退避顶格 1800s + seed 收缩到 0.02 → `p_avail=0.02` → `health_tier=2` 排最后 → 拿不到那 2% 试探 → 凑不齐 5 次成功 → **永久化** |
| 4 | G2 反饥饿强制探测 | 排序键新增第②位 `starved`：任何**可选**号超 `STARVATION_PROBE_SECS`(180s) 未被选中，下轮无条件排最前。排在 `unusable` 之后 → 熔断/饱和号仍沉底，不绕过硬门 |
| 5 | 封号原因持久化 | `KiroCredentials` 原先只有 `disabled: bool`，加载时对所有禁用号一律回填 `Manual` → 自动禁用原因**重启即丢失**，并击穿以 reason 为判据的自愈逻辑。新增 `disabled_reason` + `disabled_at`（serde camelCase，`#[serde(default)]` 兼容旧文件），9 个禁用点全部记录时间戳 |
| 6 | `shutdown_with_drain_cap`(8s) | `with_graceful_shutdown` 原先无限等在途 SSE drain → 只能等 systemd 90s 超时 SIGKILL。实测一次部署停机 74 秒、产生 167 次 502（占当日 502 的 41%），502 的 p50 duration 仅 0.01s（连接被瞬间拒绝，非超时） |

早前批次（同样已上线）：死号自动禁用 `consecutive_suspicious`（连续 6 次风控且零成功）、
`compute_max_retries` 恒封顶 12、403 跨号转移上限 3、`RpmTracker` VecDeque + `counts_for` 批量读、
未识别事件按类型只告警一次。

### 前端

| 改动 | 修的是什么 |
|---|---|
| `dashboard.tsx` / `settings-page.tsx` 的 `if (error)` → `error && !data` | React Query v5 后台 refetch 失败只置 `status:'error'` 并**保留 `data`**，是页面自己把可用缓存换成错误卡。用户看到的「加载失败 Request failed with status code 502」即此 |
| `api/usage.ts` / `api/ops.ts` 补 `timeout: 15000` | 原为 axios 默认 0 = 无限等，请求挂在网关那跳时面板整块冻结（实测 p90 71.75s / max 1077s）。`restartService` 单独 5s 并容忍 error |
| 502/503/504 与网络错误重试放宽到 3 次 + 指数退避（上限 15s） | 原 `failureCount < 1` |
| SSE 补 `!resp.ok` 检查 + 指数重连 + `generation` 计数 | fetch 对 502 是 resolve 且 Caddy 错误页让 body 非空 → 原先假报"在线"再闪回；快速切标签页会泄漏连接（后端每条残留连接 1.5s/帧持续推送） |
| `overview-page.tsx` 故障时显示 `'—'` 而非 `0` | 安静显示 0 比报错更危险 —— 看起来像真的没流量 |

### 本轮已处理的两个遗留

- **`tie_break_jitter` 已删除**（见硬约束 #5）。原测试改为性质回归
  `test_jitter_breaks_full_tie_among_fresh_credentials`，只断言"全新号池会铺开流量"，
  不声称是某个实现的功劳。
- **`admin-ui/pnpm-workspace.yaml` 已填**。它原是 pnpm 生成的**未填模板**
  （值为字面量 `set this to true or false`），会让 `pnpm install`/`pnpm build` 以
  `ERR_PNPM_IGNORED_BUILDS` 直接失败；而 rust-embed 是编译期嵌入 `admin-ui/dist`，
  前端构建失败会连带让 cargo 报 **E0599**（`no associated function named 'get'`）
  —— 排查方向极易被带偏。现填 `@swc/core: true` / `esbuild: true`。

---

## 3. 未完成的工作（按建议顺序）

### 批 A — 后端读路径韧性（MASTERPLAN 批 4）· 建议先做
> 🔶 **状态 2026-08-03：1/3/4 项已上线，第 2 项（`p_avail` 批量化）明确不做。**
> 只剩「SSE `stream_live` 改 O(1)」，且推荐实现已从 `broadcast` 改为 `watch`。见顶部表格。

前端已经不清屏了，但**后端仍有会挂住的读路径**，两者要配对才完整。

1. **余额查询是 502 的真实来源之一**。`credentials/{id}/balance` 打 `app.kiro.dev` 上游，
   面板不该为上游慢而 502。要加超时 + 缓存降级（有缓存就返缓存 + 标记 stale）。
   实测 Caddy 日志里该端点有 5 次 502。
2. **`health.p_avail` 每候选一次独立加 `states` 锁**（`rpm` 已批量化，这个还没）。
   1000 RPM × N 号下评估影响，做批量化（参照 `RpmTracker::counts_for` 的模式）。
3. `usage_stats` 的 `overview`/`timeseries`/`by_model`/`recent` 复杂度核查；
   `trace_db` 的 `query_*` 是否走索引。
4. SSE `stream_live` 每 ~1.5s 推快照，多标签页时成本是 O(标签页)。
   改 broadcast 摊成 O(1)。

**验证**：面板并发打开 3 个标签页 + 制造上游慢响应，观察是否仍冻结。

### 批 B — 凭据多选 + 强制删除（MASTERPLAN 批 5）
> ✅ **状态 2026-08-03：已上线（0.7.45）。** D1 按本节推荐落地：`force` 绕禁用门、
> **仍进回收站**（`purge` 未实现，理由同本节）。批量删除端点已有。

用户原话：「在凭据管理、均衡负载那个地方加入一个多选按钮，点击后点击凭据卡片可以多选，
多选菜单按钮里要多加一个强制删除」。

**已实测的现状**：`DELETE /credentials/{id}` 返回
`400 "只能删除已禁用的凭据（请先禁用凭据 #229）"` —— 删一个号要两次调用，
批量删 N 个 = 2N 次往返。这正是「强制删除」要解决的摩擦。

**⚠️ 需要用户拍板**：「强制删除」的确切语义。
建议拆成两个独立参数：`force`（绕过"必须先禁用"这道门）与 `purge`（跳过回收站），
**`purge` 默认 false**。理由：删除本已是软删（进 `trash.json`，settings-page 有完整
回收站 UI，`restore_credential` 恢复为禁用态、id 不变），而 adminKey 明文存 localStorage
且全仓无 CSP —— 一旦 XSS 就能整池清空，回收站是被打穿后**唯一的兜底**。

其它要点：批量端点 vs N 次单调用（建议批量，`ids` 上限 200）；部分失败仍返 200
并逐条标 `ok`/`error`（参照 `import/keys` 的既有模式）；前端复用现有 `Dialog`
（`components/ui/` 下**没有** dropdown-menu，不要为一个下拉引 radix）；三语 i18n。

### 批 C — OTA 一键升级（MASTERPLAN 批 6）
> ✅ **状态 2026-08-03：已打通。** 两个阻塞项都解了（PAT 已写入 `update.env`；走 Release）。
> 本节末尾的 chunked 绕过（已知缺陷 #2）也已修 —— 三处响应体读取收口到 `common::http_read`。
> ⚠️ 唯一遗留：那个 PAT 是 `gho_` 型，用户下次 `gh auth login` 会轮换它 → OTA 静默失效。

**两个阻塞项，都需要先解决才能写代码**：

1. **VPS 上 `/etc/kirostudio/update.env` 的 PAT 仍为空**（我刚验过）。
   面板「检查更新」必然失败。需要在 GitHub 网页手工建 fine-grained PAT
   （只勾 `dwgx/KiroStudio-skiapi`，Contents: Read-only），填进去后
   `systemctl restart kirostudio`（EnvironmentFile 在启动时读取）。
2. **`update.rs` 拉的是 GitHub Release，而 `deploy-build.yml` 产出的是 Actions
   artifact（不是 Release asset）**。所以仓库若没有 Release，按钮必然失败。
   **建议走 Release**（打 tag 触发 `release.yml`），不要改代码去读 artifact ——
   后者有四重不兼容：zip 包装、按 run id 而非 tag 寻址、需 `actions:read`、14 天过期。
   保留 `deploy-build.yml` 服务 VPS hotswap（它刻意不建 Release，避免每次部署构建污染版本线）。

顺带修已知缺陷 #2：`MAX_DOWNLOAD_BYTES` 的 Content-Length 预检**能被 chunked 响应绕过**，
正解是流式截断（同仓 `admin_ui/router.rs` 已有两处参照实现），不是加大上限。

### 批 D — 用量与额度动态刷新（MASTERPLAN 批 7）
> 🔶 **状态 2026-08-03：`first_token_ms` 埋点已上线并验证线上有值**（本节的「疑似从不写入」
> 已确证：全仓 0 个生产赋值点、24h 59458 条全 NULL）。余额乐观修正也已上线。
> 仍未做：面板侧的实时刷新。

用户原话：「自动显示用量、动态变更；用了余额之后要刷新额度显示」。

**⚠️ 该路侦察上次因号池 502 失败，MASTERPLAN 里这块深度不足，需重跑。**

已知线索：
- **`first_token_ms` 疑似从不写入 `traces.db`**（另一份文档记录过 200 行全 NULL）。
  若确认，这是独立的可观测性缺陷，**会让所有延迟分析失效**，应优先修。
- `credits_used` 的写入链同样需核实（`meteringEvent.usage` → `RequestRecord`）。
- 设计方向：请求完成后用本地累加值**乐观更新**显示，后台异步校准；
  **避免每次请求都打 `web_portal`**（那是上游探测，会加重风控）。
  前端可复用已有的 `/api/admin/stream/live` SSE。

### 批 E — cache 专题续做（MASTERPLAN 批 7）
> ⬜ **状态 2026-08-03：未做，但前置已解除。** 本节说的「先修批 D 的埋点才能做实验」
> 已经做完了，现在只差线上样本积累。

**同样因 502 未完成侦察。** 先读 `docs/CACHE-EXP0-RESULT.md`（唯一必读的一份）。

已知：
- EXP-0 已确证**上游不发 prompt cache 真值**（`metadataEvent` 只有 `stopReason`）。
- EXP-1 跑过 8 档但 `credits_used` / `first_token_ms` / `cache_read_input_tokens`
  **全为 null**（见 `docs/cache-probe-data/observations.jsonl`）→ RFC 设计的
  「用 credit 当命中率仪器」在该采集路径拿不到自变量。**先修批 D 的埋点才能做实验。**
- 线上 `cache_read` 是**我们自己注入的估算值**，命中率 11% 不可信。
- 压缩实测 6.88MB→6.66MB 只省 3%，压缩后仍超上游 ~5MiB 限制，而它**改写历史中段
  破坏前缀缓存** → 收益为负，应评估是否直接关掉。

---

## 4. 待用户决策（不要自己拍板）

| # | 决策点 | 我的推荐与理由 |
|---|---|---|
| D1 | 「强制删除」的确切语义 | 拆 `force` + `purge` 两参数，`purge` 默认 false（见批 B） |
| D2 | OTA 走 Release 还是改读 artifact | **走 Release**。artifact 四重不兼容，改造代价是一整条新链路 + 失去 tag 语义与 sha256 独立信道 |
| D3 | affinity TTL 是否从 30min 缩到 5min | **先做 EXP-1/EXP-2 再动**。「缩短会掉命中率」当前**没有可测量依据**（11% 是估算值），而代价侧是确定的（一个会话占 83% 流量、gini 0.431）。若必须现在动，只降 Warm 档（5-15min 从硬亲和改为加分项），保住 Hot 档 |
| D4 | 压缩是否直接关掉 | 倾向关。只省 3% 却破坏前缀缓存，且压缩后仍超限 —— 但要先确认关掉后大请求的 400 率不上升 |
| D5 | 面板 SSE 是否加并发连接上限 | 先做批 A 的 broadcast 改造，暂不加硬上限。是否需要取决于「面板是否会被多人/多标签页长期挂着」，需用户确认使用场景 |

---

## 5. 防回归守卫（让「越跑越慢」结构性不再出现）

本轮的教训是这一节存在的理由：**`decay_idle` 是带测试、审查过的修复，
却被同一个函数几行后的一行赋值完全失效；修好后 review 又发现离散字段仍不衰减。**
靠逐字段审查保证「每个惩罚状态都有下降路径」已两次证明不可靠。

### 已落地

**G2 反饥饿强制探测**（见 §2 #4）。它的价值不在修某个 bug，而在于
**让所有单向状态缺陷的最坏后果降级为「偶尔损失一个探测请求」** ——
即使将来又引入新的单向惩罚，也不会再出现「有效容量 6→3」。

### 建议补做

**G1 元测试 — 只推进时间，且必须走真实读路径**

```rust
#[test]
fn test_all_penalty_counters_have_time_only_recovery() {
    // 造最坏状态：连续 429 打到跳闸、多轮半开失败、族级 403
    // 然后【只推进时间】—— 零成功、零请求
    // 断言：p_avail >= 0.40 ∧ consecutive_429 == 0 ∧ open_count 下降
    //       ∧ admit_prob_seed 恢复 ∧ consecutive_suspicious == 0
}
```

两条硬规则（建议写进 `CONTRIBUTING.md`）：

1. **禁止在测试里手工写私有状态字段。**
   这正是本轮 S0 漏网的直接原因 —— 旧测试靠回拨 `last_touch` 才"通过"，
   绕过了自己要验证的那条读路径。
   （⚠️ 现有几个测试仍在回拨 `last_decay_at`，属过渡状态，改注入时钟后应清理。）
2. 改为**注入时钟**：`HealthTracker` 持一个 `Clock` trait 对象
   （生产 `Instant::now()` / 测试 `MockClock`），测试只调公开 API + 推进时钟。

**新增惩罚状态时的检查清单**（建议写进 `health.rs` 模块文档）：

- [ ] 它有**时间衰减**路径吗？（不是「成功时清零」—— 拿不到请求就没有成功）
- [ ] 如果它是**离散量**，是否走了 `decay_carry` 进位？
      （直接 `(dt/HALFLIFE) as u32` 在热路径上恒为 0）
- [ ] 它是否被纳入 G1 元测试的断言列表？
- [ ] 衰减时钟用的是 `last_decay_at` 而**不是** `last_touch`？
      （后者会被选号读路径刷新）

---

## 6. 环境与验证

```bash
# 构建（必须带 --no-default-features：Cargo.toml 的 default=["native-tls"] 与出厂配置相反）
cd admin-ui && pnpm install --frozen-lockfile && pnpm build && cd ..   # rust-embed 编译期需要 dist
cargo test --no-default-features --bin kirostudio        # 全绿（数量以实跑为准，别写死）
cargo clippy --no-default-features --bin kirostudio      # 0 error

# 出 VPS 二进制
gh workflow run deploy-build.yml --repo dwgx/KiroStudio-skiapi --ref deploy/vps -f run_tests=true
gh run download <RUN_ID> --repo dwgx/KiroStudio-skiapi -D .
# 三处 sha256 必须一致：CI 产物 / 本地 / 服务器
```

**热替换流程**（已验证，停机约 10s）：

```bash
# 1. 校验并上传
sshpass -e scp <binary> ws-vps:/tmp/ks-new
ssh ws-vps 'sha256sum /tmp/ks-new'          # 与 CI 的 .sha256 对比
# 2. 冒烟测试（必做：不能替换后才发现跑不起来）
ssh ws-vps 'chmod +x /tmp/ks-new && /tmp/ks-new --help && ldd /tmp/ks-new'
# 3. 备份 + 原子替换 + 重启
ssh ws-vps 'S=$(date +%s)
  cp -a /opt/kirostudio/bin/kirostudio /opt/kirostudio/bin/kirostudio.bak.$S
  cp -a /opt/kirostudio/data/config.json /opt/kirostudio/data/config.json.bak.$S
  cp -a /opt/kirostudio/data/credentials.json /opt/kirostudio/data/credentials.json.bak.$S
  install -o kirostudio -g kirostudio -m 755 /tmp/ks-new /opt/kirostudio/bin/kirostudio
  systemctl restart kirostudio'
# 4. 复验：/v1/models 应 200，看 journal 有无 ERROR/panic
```

**⚠️ 不要用 `set -e` 包住 curl 健康检查** —— curl 连接失败返回非 0 会让整个脚本中止，
看起来像服务坏了。本轮踩过这个坑。

**SSH 当前只能用密码**（服务器 `PubkeyAuthentication no` 且 `authorized_keys` 被清空）：

> ⚠️ **已过期（2026-08-03）：密钥登录已恢复，`ssh ws-vps` 直接可用。**
> 下面这段留作密钥再次失效时的备用手段。

```bash
export SSHPASS='<见 ws-vps/secrets/CREDENTIALS.md>'
sshpass -e ssh -o StrictHostKeyChecking=accept-new ws-vps '<命令>'
```

**⚠️ `gateway-status` 会假绿** —— 它只看「是否在冷却中」而不看「拿去用能否成功」。
本轮出现过它报「43 个号全可用」而实际全在返 403。判断号池真实健康要看
`traces.db` 的**逐号成功率**，不要信巡检的号数。

---

## 7. 线上配置（刻意偏离默认值，不要"按默认更安全"改回去）

完整依据在 `ws-vps/docs/02-tuning.md`。

> ⚠️ **本表三处线上真值已变（2026-08-03 实读 79 项配置）**：
> `inboundThrottleEnabled` = **true**（`inboundTargetRpm=133`，由 `throttle-autotune.timer`
> 每 2 分钟自动调）；`cooldownEnabled` = **false**；`ccAutoBuffer` = **true**
> （⚠️ 该项两条实测证据链互相矛盾，**需用户拍板**，不要自行改任一方向）。
> 表里的实测数字（+0.09 spearman、6.4s 一次 429、774ms）仍有效，只是状态变了。
> 另：`credentialRpmLimit=200` 算出的"池容量 167"是**配置推导值不是实测值**，见顶部第 3 条。

| 配置 | 线上值 | 为什么不能改回 |
|---|---|---|
| `rateLimitEnabled` | **false** | 每号最小间隔 1000ms 会在 241ms 处踢开亲和绑定 → 每次换号 → prompt cache 全丢。而 5339 样本实测速率与 429 率 spearman 仅 **+0.09** |
| `inboundRpmAuto` | **false** | 内置 AIMD 是单向棘轮：429 砍半、回升要 20s 静默 ×N，而实测每 6.4s 一次 429 → 单调下滑锁死在下限（实测卡 30 RPM 而号池能跑 216）。**低档还会触发令牌桶容量塌陷**（`rpm*burst >= 60` 的隐含要求） |
| `inboundThrottleEnabled` | false | 吞吐上限改由号池真实容量决定 |
| `ccAutoBuffer` | **false** | 实测 774ms（p50 3646→1873ms），且四种计费形态 `input_tokens` 完全一致 —— "换 TTFB 保精度"在这条链路上不成立 |
| `cooldownEnabled` | **true** | 死号自动禁用的冷却/健康惩罚需要它（计数与禁用本身已不依赖它） |
| `trustForwardedHeader` | **false** | sub2api 的透传白名单里没有 `X-Forwarded-For` 且不转发它 → 开了也拿不到真实用户 IP，而 KiroStudio 看到的 `client_ip` 恒为服务器自身地址，按它配 IP 黑名单会**一封封掉全部流量** |

---

## 8. 建议的第一步

1. **读 `MASTERPLAN-2026-07-29.md` 的批 4**，做批 A（后端读路径韧性）。
   风险最低、与已上线的前端改动配对、用户能直接感知。
2. 同时向用户确认 D1（强制删除语义）与 D2（OTA 走 Release），
   这两条不确认无法开工批 B / 批 C。
3. 批 D 的第一件事是**核实 `first_token_ms` 与 `credits_used` 是否真被写入**
   —— 它是批 E 的前置，也是所有延迟分析的基础。

**每批上线后必须在进程持续运行 + 有真实流量的窗口里观测。**
不要在重启后立刻看 EWMA 类指标 —— 重启会把它们重置成乐观初值 1.0，
本轮我就因此把重启效果误报成了修复效果。
