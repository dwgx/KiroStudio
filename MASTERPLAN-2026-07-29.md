<!-- HISTORICAL-ARCHIVE-MARK -->
> ⚠️ **这是过程记录，不是当前状态。** 当前状态看仓根 `STATUS.md`（唯一真相源）。
>
> 本文件已确证含**过期断言**。历史上多次出现「后来的会话把过期断言当约束」而做出错误决策
> ——最严重的一次：一句无依据的「`q.*` 已停用」注释直接导致了一次错误的架构迁移，
> 改坏 region 探测让 US 号恒 403，上线后才发现并回滚。
>
> 读本文件时：**任何数字（测试数 / 配置值 / 池容量 / 行号）一律现读现验**，
> 结论性断言先按 `STATUS.md` 核一遍。本文件的价值在于**依据与推导过程**，不在于它的结论。

---

# KiroStudio 分批实施计划 — 2026-07-29

> 由 8 路并行侦察 + 对抗性复核产出。**配套读 `HANDOFF-2026-07-29.md`**（交接总览与已知错误）。
> ⚠️ 本文中的行号来自产出时的工作树快照，动手前请用函数名重新定位，不要直接信行号。
> ⚠️ `usage` / `cache` 两路侦察因号池 502 未完成，对应批次深度低于其它六路。

# KiroStudio 主实施计划（8 路战线合并）

> 本文的行号均来自本轮**实地复核**（4 次只读工具调用，覆盖 health.rs / token_manager.rs / update.rs / usage_handlers.rs / trace_db.rs / affinity.rs / credentials.rs 与 4 个前端文件）。凡未亲自复核的，只写函数名并标注来源。

---

## 一、执行摘要（最重要 5 条）

| # | 改什么 | 为什么 | 收益 |
|---|---|---|---|
| **1** | `dashboard.tsx:722` 的 `if (error)` 与 `settings-page.tsx:1620` 的 `if (error \|\| !config)` 改为 `error && !data` | React Query v5 后台 refetch 失败只置 `status:'error'` 并**保留 `data`**，是页面自己把缓存丢掉换成错误卡。用户看到的「加载失败 Request failed with status code 502」就是这两行 | 30s 轮询里任意一次 502 不再清屏；部署 74s 窗口内面板持续可读。**一行判据，零后端风险，用户立刻感知** |
| **2** | `api/usage.ts:15` 与 `api/ops.ts:11` 的 `axios.create` 补 `timeout: 15000` | 两个实例**没有 timeout**（axios 默认 0 = 无限等），只有 `credentials.ts:34` 有。请求挂在 Caddy→KiroStudio 那跳时会挂到网关超时（实测 p90 71.75s / max 1077s），期间 React Query 不并发下一轮 → 面板整块静默冻结 | 「面板卡顿」的真因（后端 admin API 实测 0.3ms，不是查询慢）。冻结上限从 1077s 降到 15s |
| **3** | `health.rs` 的 `decay_idle` 改为**无门槛连续衰减**，并把衰减时钟从 `last_touch` 拆出 | **今晚刚部署的 decay_idle 在有流量时一次都不会执行**：`p_avail_with_load_ref`（health.rs:343-356）的顺序是 `tick_circuit → decay_idle → s.last_touch = now`，而 `decay_idle`（:240-241）第一件事是 `idle < IDLE_DECAY_MIN_SECS(5.0) → return`。该函数是每请求每候选都调的选号热路径，饥饿号同样每轮被触碰 → idle 恒 <5s。测试能过是因为测试手工回拨 `last_touch`（:699、:747 的注释自己写明了） | 修复「有效容量 6→3」的真实机制。**这是本次审计单条价值最高的发现** |
| **4** | `KiroCredentials` 加 `disabled_reason` / `disabled_at` 并在 `persist_credentials` 同步 | `credentials.rs:203` 只有 `disabled: bool`；`persist_credentials`（token_manager.rs:3131）只写 `cred.disabled = e.disabled`（:3158）；加载时对所有 disabled 号一律回填 `Some(DisabledReason::Manual)`（:1565）。**重启后所有自动禁用原因变成「手动禁用」**，并连带击穿三处以 reason 为判据的逻辑（含全池 TooManyFailures 自愈 → 整池禁用后永久死锁） | 用户明确要求的「认定封号必须标明原因」当前**重启即失效**。additive serde 字段，向后兼容 |
| **5** | `open_count` / `admit_prob_seed` / `consecutive_429` 补时间衰减 | 三者都是单向棘轮且 `decay_idle` 明确不碰。`open_count` 唯一归零点是 health.rs:267（HalfOpen 内连续 5 次成功），`admit_prob_seed` 每次半开失败 ×0.5 直到 `MIN_ADMIT_SEED = 0.02`（:80）→ p_avail=0.02 → health_tier=2 排最后 → 拿不到那 2% → 凑不齐 5 次成功 → 退避顶格 1800s **永久化**。而 `report_family_suspicious`（:325）对 403 无条件 `open_count += 1`，403 是临时态 | 这是 decay_idle 治不到的二阶自锁，也是历史「12h/88 次误禁」事故的同一入口 |

---

## 二、冲突矩阵

今晚已改并部署：`src/kiro/{health,scheduling,provider,token_manager}.rs`、`src/main.rs`。工作树另有 47 个其他会话的未提交文件 → **所有后端批次必须串行，不可并行开工**。

### 2.1 `src/kiro/token_manager.rs`（4 路战线抢同一文件，5200+ 行，最高风险）

| 战线 | 触及函数 | 与今晚已改的关系 |
|---|---|---|
| 封号 | `persist_credentials`(:3131)、入池映射(:1565)、`reset_and_enable`、全池自愈块、`CredentialEntry` 结构体、`CredentialEntrySnapshot` | 今晚新增 `persist_disabled_state` 是本战线的落盘收口，**直接复用** |
| 调度 | `select_next_credential` / `sort_key`、`commit_selection`、`report_model_invalid`、`is_model_blocked`(:3821)、`report_suspicious_activity`、`report_failure` | 今晚改了 `compute_max_retries`、403 转移上限、`counts_for` 批量读 — 与本战线**函数不重叠但同区域** |
| 后端并发 | `snapshot`、新增 `live_snapshot`、`p_avail` 调用点批量化 | 今晚已把选号侧改成 `counts_for` 批量；`snapshot` **没跟上**，仍逐条加 rpm 锁 |
| 多选删除 | `delete_credential`(:5240)、新增 `delete_credentials_batch`、`purge_trash_batch`(:4436)、`purge_credential`(:5473) | 无重叠 |

**冲突点与处理**

1. **`CredentialEntry` 结构体要被两个战线各加一个字段**（封号的 `disabled_at`、调度的 `last_selected_at`）。
   → **一次加齐**：在批 2 里同时加两个字段（`last_selected_at` 先加不用），批 3 只写赋值与读取。省掉第二次结构体编辑与随之而来的全部 `CredentialEntry { .. }` 字面量修补。
2. **`snapshot` 被封号（加 `disabledAt`）和后端并发（rpm 批量化）同时改**。
   → 批 2 先加字段，批 4 再动锁结构。反序会导致批 2 的字段加进一个正在被重写的函数。
3. **`delete_credential` 的回滚分支硬编码重建 `CredentialEntry`**（战线 4 的 F5，字段清单含 `success_count: 0` / `disabled: true`）。批 2 一旦给结构体加字段，**这个字面量必须同步补**，否则编译失败 —— 这其实是好事，编译器会替我们抓住它。批 5 再把它改成保存整个被摘走的 entry。
4. `is_rpm_saturated` 自己 `entries.lock()` 且非重入，非测试调用点为零 → 批 3 顺手标 `#[cfg(test)]`，把注释级约束变成编译级。

### 2.2 `src/kiro/health.rs`（今晚刚加 `decay_idle`，批 3 要重写它）

批 3 是**对今晚改动的修正**，不是叠加。三个衰减语义要合并成一套：
- 保留 `last_touch` 语义 = 「键还被引用」，`cleanup`（:416）的 `IDLE_EVICT_SECS` 淘汰继续用它，`p_avail` 继续刷新它是**对的**；
- 新增 `last_decay_at`（衰减基准）与 `last_outcome_at`（只在 :260/:284/:316 三个真实结果点写）；
- 删掉 `IDLE_DECAY_MIN_SECS`（:84）门槛 —— 连续衰减不需要门槛，同轮内 `dt≈0` → `factor≈1` 自然幂等。

⚠️ **改 health.rs 前必须先跑一次现有测试**：health.rs:699 与 :747 手工写 `last_touch` 的两个测试会因语义拆分而失效，需同步改成注入时钟（见第六节）。

### 2.3 其余文件（无跨战线冲突）

| 文件 | 战线 | 批次 |
|---|---|---|
| `admin-ui/src/components/dashboard.tsx` | 前端韧性(F1/F4) + 多选(F1/F11/F12) | 批 1 改错误态，批 5 改选择模式 —— 同文件不同区域，串行即可 |
| `admin-ui/src/components/settings-page.tsx` | 前端韧性(F8) 独占 | 批 1 |
| `admin-ui/src/api/{usage,ops}.ts`、`main.tsx`、`hooks/use-live-stream.ts` | 前端韧性独占 | 批 1 |
| `src/admin/service.rs` | 后端并发(F2 balance/F8 balance_cache) + 多选(batch delete service 层) | 批 4 / 批 5 |
| `src/usage/{trace_db,usage_stats}.rs`、`src/admin/usage_handlers.rs` | 后端并发独占 | 批 4 |
| `src/admin/update.rs`、`src/common/health_marker.rs`、`install-binary.sh`、`.github/workflows/*` | OTA 独占 | 批 6 |
| `src/kiro/{affinity,cooldown,rate_limiter}.rs` | 调度独占 | 批 3 / 批 7 |
| `src/kiro/provider.rs` | 封号(F4/F5/F6 错误分类) | 批 2 尾（今晚已改过 403 分支，需先 `git diff` 确认现状） |

**建议改动顺序（强约束）**：批1（纯前端）→ 批2（封号，token_manager 结构体一次加齐）→ 批3（health 重写 + 调度）→ 批4（后端读路径）→ 批5（多选删除）→ 批6（OTA）→ 批7（低优）。

---

## 三、分批实施计划

### 批 1 — 前端错误态与超时（用户马上感知，风险最低）

**为什么放一批**：全部是 `admin-ui/` 下的前端改动，不碰后端，不碰号池，不改任何数据模型。一次 `pnpm build` + `cargo build` 重嵌即生效。改错也只影响面板显示，不影响转发链路。

#### 改动清单

| 文件 | 位置 | 改法 |
|---|---|---|
| `admin-ui/src/components/dashboard.tsx` | :722 `if (error)` | 改 `if (error && !data)`。`error && data` 时正常渲染，页顶插非阻塞过期条（取 `dataUpdatedAt`）。:713 `if (isLoading)` 分支**不动**（首屏无缓存仍走骨架屏） |
| `admin-ui/src/components/settings-page.tsx` | :1620 `if (error \|\| !config)` | 改 `if (error && !config)`。另 :740 / :903 / :1084 / :1338 四处子卡片三元链改为 `error && !data` 才显示 `loadFail`，`error && data` 时渲旧值 + 角标过期圆点 |
| `admin-ui/src/api/usage.ts` | :15 `axios.create` | 加 `timeout: 15000`（与 `credentials.ts:34` 对齐） |
| `admin-ui/src/api/ops.ts` | :11 `axios.create` | 加 `timeout: 15000`；`restartService` 单独传 `{ timeout: 5000 }` 并让调用方容忍 error（重启瞬间必然断连，文件内已有注释说明） |
| `admin-ui/src/main.tsx` | :14-19 `retry` | 4xx 不重试保持不变。网络错误与 502/503/504 放宽到 `failureCount < 3`，显式 `retryDelay: (n) => Math.min(1000 * 2 ** n, 15000)`（覆盖 1+2+4≈7s）。其余 5xx（500 等确定性业务错误）维持 1 次 |
| `admin-ui/src/hooks/use-live-stream.ts` | `connect()` | `setConnected(true)` 前加 `if (!resp.ok) throw new Error('http ' + resp.status)`。fetch 对 502 是 resolve，Caddy 的 HTML 错误体使 `resp.body` 非空 → 现在会假在线闪烁。重连间隔 2000 常量改 `Math.min(2000 * 2 ** attempt, 30000)`，收到第一帧后 `attempt = 0` |
| `admin-ui/src/hooks/use-live-stream.ts` | `onVisibility` | 引入 `activeCtrl` + `generation` 计数：`connect()` 入口先 `activeCtrl?.abort()`；catch/重连分支 `if (myGen !== generation) return`；变可见时若已有存活连接直接返回。修快速切标签页的 SSE 泄漏（后端 `usage_handlers.rs:342` 每条残留连接 1.5s/帧持续推送） |
| `admin-ui/src/components/overview-page.tsx` | `?? 0` 降级点 | query 处于 error 且无 data 时显示 `'—'` 而非 `0`。**故障时安静显示 0 比报错更危险**，看起来像真的没流量 |

**i18n**（`en/ja/zh` 三份同步）：`common.stale.{banner,lastUpdated,retrying,httpError,dot}`、`common.live.{reconnecting,reconnectAttempt}`、`common.value.unavailable`、`dashboard.error.staleNotice`、`settingspage.page.staleNotice`。

#### 回归测试

| 测试名 | 断言 | 旧代码为何失败 |
|---|---|---|
| `should keep credential cards visible when a background poll returns 502` | MSW 让 `getCredentials` 首次成功、第二次 502；断言过期条出现且卡片数 > 0 | 旧代码 :722 无条件走 error 分支 → 渲出 `dashboard.error.loadFailed` 文案，卡片数 0 |
| `should surface a timeout error instead of hanging forever` | 永不响应的 `/usage/overview`；断言 15s 后进 error 且可发起下一轮 | 旧代码 axios timeout=0，promise 永不 settle |
| `should retry a 502 three times with capped exponential backoff` | fake timers 断言重试次数与 1s/2s/4s 间隔序列 | 旧代码 `failureCount < 1` |
| `should not report connected on a 502 SSE response` | mock status 502 + 非空 body；断言 `connected` 始终 false、重连间隔递增 | 旧代码 fetch resolve + body 非空 → 出现一次 `connected=true` |
| `should not leave orphan SSE connections after rapid visibility toggling` | 连续 hidden→visible ×5；断言 fetch 调用数与未 abort 的 signal 数均为 1 | 旧代码 `ctrl` 被覆盖，A2 无人持有 controller → 永不 abort |
| `should keep storage stats visible when the refresh fails` | 逐卡片同 F1 手法 | 同上 |

#### 上线验证

```bash
cd admin-ui && pnpm install --frozen-lockfile && pnpm build && cd ..
cargo build --release --no-default-features   # rust-embed 编译期嵌 dist，缺 dist 直接 E0599
```
- 浏览器 DevTools → Network 阻断 `/api/admin/credentials` 一次，确认卡片**不消失**、顶部出现过期条。
- Network 面板确认 `/usage/*` 请求在 15s 处被主动 cancel（而非 pending 到底）。
- 快速切标签页 5 次后，`ssh ws-vps 'ss -tn state established | grep -c 8990'` 观察连接数不单调累积。

#### 回滚
`git revert` 该批提交 + 重新 `pnpm build && cargo build`。二进制层面用 `ssh ws-vps 'kirostudio-update rollback'`（回到 `kirostudio.prev`）。纯前端，无数据迁移。

---

### 批 2 — 封号原因全链路持久化（用户明确要求的功能）

**为什么放一批**：全部围绕 `disabled_reason` 这一条数据链，且 `CredentialEntry` 结构体只在这一批被扩字段（一次加齐 `disabled_at` + `last_selected_at`，后者供批 3 用）。additive serde 字段，老 `credentials.json` 直接可读。

#### 改动清单

| 文件 | 函数 | 改法 |
|---|---|---|
| `src/kiro/model/credentials.rs` | `KiroCredentials`（:203 附近） | 加 `#[serde(default)] pub disabled_reason: Option<String>` + `#[serde(default)] pub disabled_at: Option<String>`（RFC3339） |
| `src/kiro/token_manager.rs` | `DisabledReason` | 加 `as_str()` / `from_str()` 一对，与既有的字符串映射**共用同一张表**避免二次漂移 |
| 同上 | `persist_credentials`(:3131 / 写入点 :3158) | `cred.disabled = e.disabled` 旁同步 `disabled_reason` / `disabled_at` |
| 同上 | 入池映射(:1565) | 由字符串反解回枚举；**仅当字符串缺失时**才回退 `Manual`。`set_disabled` 手动路径显式写 `"Manual"`，使老文件与真手动可区分 |
| 同上 | `CredentialEntry` | 加 `disabled_at: Option<DateTime<Utc>>` + `last_selected_at: Instant`（后者批 3 用）。⚠️ 加完后 `delete_credential` 回滚分支的硬编码字面量会编译失败 —— 按现状补齐即可，批 5 再重构 |
| 同上 | `CredentialEntrySnapshot` | 暴露 `disabledAt` |
| 同上 | `reset_and_enable` | **加 `entry.consecutive_suspicious = 0;`**。该字段唯一清零点是 `report_success`，被 `SuspiciousActivityAuto` 禁用的号（计数已达阈值 6）人工复活后**一次风控即秒禁**。与该函数已修的 `RequestLimitReached` 是同型 bug |
| 同上 | 全池自愈块（`acquire_context` 内 TooManyFailures 一次性重置） | 补 `persist_disabled_state` 批量落盘 + 清 `cooldown` / `rate_limiter` / `consecutive_suspicious` / `refresh_failure_count`。现状只改内存 → 磁盘仍 disabled=true → 面板与磁盘长期背离，重启即回死态 |
| 同上 | 新增 `CredentialEntry::clear_transient_counters(&mut self)` | 把「复活时必须清零的进程内计数」收敛成一个方法，`reset_and_enable` / 自愈块 / `set_disabled(false)` 三处统一调用。防将来再漏 |
| `src/kiro/provider.rs` | 402 + `is_monthly_request_limit` 两处（对话路径 + MCP 路径） | 去掉状态码硬门，只按 body reason 判定，**保持在 `is_temporary_rate_limit` 之后**（临时态优先，守住历史事故边界）。`handlers.rs` 自己的注释断言该 reason 会以 429 出现 |
| `admin-ui/src/lib/i18n-labels.ts` | 标签映射 | 以后端 `as_str()` 为唯一真源。补 `labels.disabledReason.{requestLimitReached,invalidConfig,tooManyRefreshFailures}`；删掉后端不产生的 `InsufficientBalance` / `SubscriptionInvalid` |
| `admin-ui/src/components/credential-card.tsx` | 禁用徽章区 | 加禁用时刻（相对时间 + hover 绝对时间）。恢复语义徽章留到批 7（依赖 TTL 自愈） |

**⚠️ 落地前需先确认**（战线报告的 open question，我未复核）：前端 `n` 紧凑字段的后端产出点 —— `rename = "n"` 在 src/ 零命中但 `types/api.ts` 有 `n?: string` 且 `use-pool-notifications.ts` 在读它。合并两张标签表前先定位来源（疑为 SSE live 推送的手写 JSON）。

#### 回归测试

| 测试名 | 断言 | 旧代码为何失败 |
|---|---|---|
| `should_preserve_auto_disable_reason_across_reload` | 数组格式 credentials.json → `report_quota_exhausted(1)` → 同路径重建 `MultiTokenManager` → snapshot 的 `disabledReason == "QuotaExceeded"` 且 `disabledAt` 非空 | 旧代码返回 `"Manual"`（:1565 无条件回填） |
| `should_clear_suspicious_counter_on_reset_and_enable` | 连打 6 次 `report_suspicious_activity` 致自动禁用 → `reset_and_enable` → 再打 1 次 → `disabled == false` | 旧代码计数仍为 6，第 7 次立即再禁 → `true` |
| `should_persist_pool_wide_selfheal` | 全池打到 TooManyFailures → 触发自愈 → **重新加载文件** → `disabled == false` | 旧代码自愈只改内存，文件读回 `true` |
| `should_disable_on_monthly_quota_reason_with_429_status` | mock 429 + `{"reason":"MONTHLY_REQUEST_COUNT"}` → `disabled` 且 `disabledReason == QuotaExceeded` | 旧代码状态码硬门 402 → 落通用瞬态分支 → `disabled == false`，号留在轮转里每次被选中白烧一次上游调用 |

#### 上线验证
```bash
ssh ws-vps 'systemctl restart kirostudio && sleep 5'
ssh ws-vps 'python3 -c "import json;d=json.load(open(\"/etc/kirostudio/credentials.json\"));print([(c.get(\"id\"),c.get(\"disabled\"),c.get(\"disabledReason\")) for c in (d if isinstance(d,list) else [d]) if c.get(\"disabled\")])"'
```
面板号池页确认禁用号原因徽章**重启后仍是具体原因**而非「手动禁用」。

#### 回滚
serde 字段是 additive 且 `#[serde(default)]`，新版写的 `credentials.json` 老版能读（多余字段被忽略）。二进制回滚即完成，**无需数据回滚**。

---

### 批 3 — 调度单向棘轮与饥饿自锁（容量恢复，本次技术含量最高）

**为什么放一批**：S1/S2/S3/S5/S4 是同一类缺陷（惩罚状态只有事件驱动的下降路径、无时间驱动的下降路径），且 S1 与 S2 在同一个 `HealthState` 上互相耦合 —— 只修 S1 会让号浮回高档拿到探测，而 S3 的旧账让探测失败的代价被放大成整轮跳闸。**必须一起改，分开改会造成中间态更差**。

#### 改动清单

**3.1 `src/kiro/health.rs` — 统一连续衰减（S1/S2/S3）**

```
现状（已复核）：
  p_avail_with_load_ref(:343-356):  now → lock → entry → tick_circuit → decay_idle → s.last_touch = now
  decay_idle(:239-245):             idle = now - last_touch; if idle < 5.0 { return } ; factor = 0.5^(idle/60)
  on_success(:260-262) / on_429(:284-286) / report_family_suspicious(:316-318): 各自也写 last_touch = now
```

- `HealthState` 加 `last_decay_at: Instant`、`last_outcome_at: Instant`。`last_touch`（:113）**保留原语义**「键还被引用」，`cleanup`（:416）的 `IDLE_EVICT_SECS` 继续用它，`p_avail` 继续刷新它。
- `decay_idle` → 重命名 `decay(s, now)`，删掉 `IDLE_DECAY_MIN_SECS`（:84）门槛，改幂等连续衰减：
  ```rust
  let dt = now.saturating_duration_since(s.last_decay_at).as_secs_f64();
  let factor = 0.5_f64.powf(dt / IDLE_DECAY_HALFLIFE_SECS);   // :88 = 60.0
  s.ewma_429 *= factor;
  s.ewma_success = 1.0 - (1.0 - s.ewma_success) * factor;
  s.last_decay_at = now;
  ```
  同轮内 `dt≈0` → `factor≈1` 自然无副作用。活跃号每秒衰减因子 0.9885，相对单次 429 的 `A_429=0.5` 跳变可忽略。
- `consecutive_429`（:106）纳入衰减：`last_outcome_at` 超过 `2 × HALFLIFE`（120s，与上游实测静置约 2 分钟自愈同量级）直接归零。**「连续」必须有时间边界** —— 分散在 30 分钟里的 3 次 429 不是连续。
- `open_count`（:112）：每 300s 无新 Open 则 `saturating_sub(1)`，使 `open_backoff`（:193 的 `1.6^(n-1)` capped 1800）阶梯**可逆**。
- `admit_prob_seed`（:110）：朝 `HALFOPEN_START` 指数回归 `seed = HALFOPEN_START - (HALFOPEN_START - seed) * factor`。
- 恢复判据（:267 的 `RECOVERY_FULL` 连续 5 次成功）→ 改「半开窗口内成功率 ≥ 阈值且样本 ≥3」，**去掉连续要求**（在被 429 打的号上连续 5 次概率极低）。
- `health_tier` 改用**不含 gate** 的纯 health 分档，gate（:355-361 取 `admit_prob`）只作概率放行。断掉「低 admit_prob → 排最后 → 拿不到探测 → seed 永不回升」的循环。
- `snapshot`（:382）也调 `decay`，让面板与热路径同口径（现状 snapshot 不调，两者本就不同步）。

**3.2 `src/kiro/token_manager.rs` — 反饥饿强制探测（S9 第一层，最值钱的结构性改动）**

- `commit_selection` 里更新 `last_selected_at`（批 2 已加好字段）。
- `sort_key` 增设**最高优先位** `starved: u8`：`now - last_selected_at > STARVATION_PROBE_SECS`（建议 120s）且该号未 disabled / 未 cooling / 未被 `model_blocklist` 挡时置 0，其余置 1 → 饥饿号本轮无条件排首位拿一个探测请求。
- **这一条把「任何号在 N 秒内必须有机会被选中」从断言变成强制执行**，且不依赖任何具体字段是否有衰减路径。S1/S2/S3/S5 即使全都不修，最坏后果也只是探测请求偶尔失败。

**3.3 `src/kiro/token_manager.rs` — model_blocklist half-open（S4）**

现状（已复核）：`model_blocklist: Mutex<HashMap<(u64, String), Instant>>`（:1371）、`MODEL_BLOCK_TTL = 1800s`（:1479）、插入 :3806、**唯一清除**是 `is_model_blocked`（:3821-3823）按 `t.elapsed() < TTL` 惰性 remove、删号时按 id retain（:5286）。`report_success` **完全不碰它**。这是全仓唯一没有恢复路径的调度状态。

- 条目从 `Instant` 改 `{ blocked_at, next_probe_at }`，`is_model_blocked` 在 `now >= next_probe_at` 时返 false 并把 `next_probe_at` 推后（每 N 分钟放行一个探测）。
- `count_selectable_for_model(model)` 降到 0 时**立即清空该 model 的全部条目并重选一次**（fail-open 优于整体 30 分钟拒服务），只有清空后第二次仍全灭才透传真 400。
- `report_success` 按 `(id, model)` 主动清除条目（正向证据即刻解封）。

**3.4 计数器时间边界（S5/S6）**

- `token_manager.rs`：`consecutive_suspicious` / `failure_count` 递增前先按 `last_failure_at` 折算衰减。今晚 5 条自动禁用路径已全部接入 `persist_disabled_state`，**这让空闲号的 hair trigger 后果从「重启即恢复」升级成「重启也回不来」** —— 优先级因此上调。
- `rate_limiter.rs`：`consecutive_failures` 在 `backoff_until` 到期被置 None 的同一处 `saturating_sub(1)`（退避跨过一档就还一档）。现状到期不清 → 下一次失败直接跳回原高退避档。
- `cooldown.rs`：平静期衰减基准从 `now > entry.expires_at` 改为 `last_trigger_at`（每次 set 都写），使冷却期内的并发重复触发也能被摊薄；`trigger_count` 加绝对上界（现在只有 duration 有 cap）。

**3.5 顺手**：`is_rpm_saturated` 标 `#[cfg(test)]`（非测试调用点为零，parking_lot 非重入，一旦有人在持锁路径调用即选号死锁）。

#### 回归测试

| 测试名 | 断言 | 旧代码为何失败 |
|---|---|---|
`test_decay_works_through_real_p_avail_read_path` | 连续 `on_429` ×3 打成低档后**不手工改任何字段**，注入时钟推进 180s，同时每 1s 调一次 `p_avail` 模拟持续流量；断言 `p_avail ≥ HEALTH_TIER_DEGRADED_MIN(0.40)` | **本条直接抓住今晚的 S1**：旧代码每次 `p_avail` 都刷 `last_touch`，`idle` 恒 1s < 5.0 → 衰减一次都不执行，p_avail 停在 0.345 量级 |
| `test_open_count_decays_and_backoff_is_reversible` | 15 次跳闸使 `open_count ≥ 13`、`open_backoff == 1800s`；只推进 1 小时（零成功零请求）→ `open_count` 降到个位数、`backoff < 60s`、`seed ≥ 0.05` | 旧代码 `open_count` 无任何随时间下降路径，恒 15、恒 1800s、seed 恒 0.02 |
| `test_consecutive_429_expires_with_idle` | `on_429` ×2（未跳闸）→ 推进 300s → 再 `on_429` ×1 → circuit 仍 Closed | 旧代码第三次使 `consecutive_429 = 3 ≥ TRIP_THRESHOLD` → Open + `open_count = 1` |
| `test_no_credential_starves_beyond_probe_window` | 3 号池，#2 health 打到 p_avail≈0.02，连发 200 次选号；#2 至少被选中一次且两次间隔 ≤ `STARVATION_PROBE_SECS` | 旧代码 #2 在 200 轮里被选中 **0 次**（tier=2 恒排最后，其余两号从不饱和） |
| `test_all_blocked_model_clears_and_retries_instead_of_total_outage` | 3 号池全 `report_model_invalid(同 model)` → `acquire_context(该 model)` 返 Ok | 旧代码全灭后 30 分钟恒 `NoCandidate` → bail「所有凭据均已禁用」 |
| `test_success_clears_model_block` | 号被 block 后 `report_success(id, model)` → `is_model_blocked` 返 false | 旧代码 `report_success` 不碰 blocklist |
| `test_consecutive_suspicious_expires_with_idle` | 5 次 `report_suspicious_activity`（=阈值-1）→ 推进 900s → 再 1 次 → 仍 enabled | 旧代码第 6 次达阈值 → 自动禁用 + 落盘 → 重启也不恢复 |
| `test_trigger_count_decays_even_when_recooled_before_expiry` | `set_cooldown` 后在 T/2 处再 set 同 reason，重复 5 轮 → `trigger_count ≤ 3` | 旧代码每轮 `now < expires_at` → 衰减分支跳过 → 线性到 5，时长按 `1.6^4` 放大 |
| `test_all_penalty_counters_have_time_only_recovery`（元测试） | 造最坏状态后**只推进时间**，断言 `p_avail ≥ 0.40` ∧ `consecutive_429 == 0` ∧ `open_count` 下降 ∧ `consecutive_suspicious == 0`。**必须走真实读路径，禁止手工改字段** | 旧代码四个断言至少三个失败 |

#### 上线验证（零成本判定 S1 是否真的是线上主因）
```bash
# 拉两次 health-snapshots，间隔 2 分钟，找 rpm=0/inflight=0 且 ewma_429>0.3 的号
for i in 1 2; do
  curl -s -H "X-Admin-Key: $K" https://k1ro.skiapi.dev/api/admin/health-snapshots \
    | python3 -c 'import sys,json;[print(s.get("key"),s.get("ewma429"),s.get("openCount"),s.get("openRemainingSecs")) for s in json.load(sys.stdin)]'
  [ $i = 1 ] && sleep 120
done
```
- 修复前后对比：`ewma_429` 是否随空闲下降、`open_remaining_secs` 是否有接近 1800 的族键（若最大只有几十秒，说明线上还没走到 S2 的顶格，S2 可降级为预防性）。
- `ssh ws-vps 'gateway-status'` 看有效容量：修复后 6 号应全部处于可选档，不再出现 4 个 T2 坏档 + rpm=0/inflight=0 空转。

#### 回滚
纯内存状态机，无持久化格式变更。二进制回滚即完成。⚠️ 回滚会同时丢掉 `starved` 强制探测这道兜底，回滚后需重新观察是否复现饥饿。

---

### 批 4 — 后端读路径韧性（admin API 与 usage 层）

**为什么放一批**：三条都在「面板读路径不该影响转发链路」这一主题下，且都不碰选号逻辑。

#### 改动清单

| 文件 | 函数 | 改法 |
|---|---|---|
| `src/admin/usage_handlers.rs` | `usage_recent`(:120) → `db.recent(limit)`(:128) 等全部 trace_db 调用点 | 包 `tokio::task::spawn_blocking`（clone `Arc<TraceDb>` 进闭包）。**全仓 `spawn_blocking` 零命中**（已复核），而 `TraceDb` 是 `Mutex<Connection>` 同步 rusqlite，与 usage-pipeline 写线程共享同一把锁 → 一次大查询同时占死一个 tokio worker 和整个 DB 锁，写入侧被顶到 channel 满而**静默丢真实请求记录** |
| 同上 | `MAX_RECENT_LIMIT`(:103) / `resolve_recent_limit`(:110-114) | `50_000` → `2_000`。`Some(0)` 现在返回 50000（:113），意味着前端「全部」会把 5 万个 `RequestRecord` 全量物化成 Vec 再序列化 JSON。「全部」语义交给 `search` 的分页。⚠️ 需同步改 :481/:494/:495 三个现有断言 |
| 同上 | `count_matching` 调用 | 改上限探测（`SELECT 1 FROM traces{where} LIMIT 1001` 数行 → 显示「1000+」）或 30s 结果缓存。现状无过滤时是全表 `COUNT(*)` |
| `src/main.rs` | 保留清理 `tokio::spawn` | `retention_cleanup` 的全表 `DELETE` 移出 tokio worker（最坏最久的那个） |
| `src/usage/trace_db.rs` | `init` 的 `execute_batch` | **已复核有 `PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;`（:147-148）** → 战线报告的悲观估计可以下调。但**未见 `busy_timeout`** → 补 `PRAGMA busy_timeout=5000`，避免锁等待直接返 SQLITE_BUSY |
| `src/admin/service.rs` | `get_balance` / `fetch_balance` | ① `fetch_balance` 外层包 `tokio::time::timeout(8s)`；② 超时或 Err 时**回退 `balance_cache` 的最后已知值**，返 200 + `stale: true` + `cachedAt`，只有连缓存都没有才返错 —— 面板永不因上游慢而 502。`get_cached_balances` 证明缓存保留 7 天内的值，`get_balance` 完全不用它；③ 每 id 单飞（`Mutex<HashMap<u64, Arc<Notify>>>`），并发只打上游一次（顺带减少封号面）；④ 复用 Client（`provider.rs` 已有 Client 缓存范式），别每次 build |
| `src/kiro/token_manager.rs` | `get_usage_limits` | 面板余额这种交互式请求单独传 8~10s，别用 `build_client(proxy, 60, ...)` 的 60s |
| 同上 | `snapshot` | 改 `let counts = self.rpm.counts_for(&ids)` 一次取回（与今晚已改的选号侧同构）。现状仍逐条 `self.rpm.count(e.id)`，锁获取 O(n) |
| 同上 | 新增 `live_snapshot()` | 只返回 id/rpm/inflight/disabled/family_key，**不做两次 SHA256、不 clone 展示字段**。`LiveFrame` 只用到 id/rpm/inflight/cooldown/health，现状每帧算的 sha256 全被丢弃 |
| 同上 | `p_avail` 调用点 | 新增 `p_avail_batch(&[(family_key, rpm, inflight, rpm_limit)], load_ref) -> Vec<f64>`：一次 `states.lock()` 内对**去重后的族键**各推进一次 decay，再算每个候选。`p_avail_with_load_ref` 是**写操作**（:353-356 推进状态），改 RwLock 无用，必须批量化。顺带消除「同族键在一轮选号内被 tick N 次」的顺序依赖 |
| `src/admin/usage_handlers.rs` | `stream_live`(:337) / interval(:342) | 改单生产者：一个后台任务每 1500ms 算一帧 `broadcast::send`，`stream_live` 只 subscribe。工作量从 O(标签页) 降到 O(1) |
| `src/usage/usage_stats.rs` | `Inner::add` 的 `by_model.entry(r.model.clone())` | model 名来自请求体、外部可控、无基数上限无 prune（`hours`/`days` 是定长 Vec，`client_agg` 有 prune，`by_credential` 键是 u64）。归一化：命中 `model_catalog` 已知集合则原样计，否则并入固定键 `other`；或硬上限 200 键 |
| `src/admin/service.rs` | `save_balance_cache` | 锁内只 clone 快照，放锁后再序列化；写盘改走 `common::fs_atomic` 并放 `spawn_blocking`；加 5s debounce。现状持锁期间做 `serde_json::to_string_pretty` + 非原子 `std::fs::write` —— 而**批 4 的陈旧降级依赖这份缓存可靠**，SIGKILL 窗口正好是它被写坏的时候 |

**i18n**：`credentialcard.balanceBar.{stale,staleHint,upstreamTimeout}`、`usage.recent.{limitCapped,totalApprox}`。

#### 回归测试

| 测试名 | 断言 | 旧代码为何失败 |
|---|---|---|
| `should_keep_ingesting_while_large_query_runs` | 多线程 runtime，一个任务反复 `recent(2000)`，另一线程持续 insert；5s 内 insert 全成功且 `dropped_count() == 0` | 读侧长时间独占 conn 锁 → insert 排队 → channel 填满 → dropped 递增 |
| `should_cap_recent_limit_to_2000` | `resolve_recent_limit(Some(0)) == 2000` | 旧代码返回 50000（:113） |
| `should_return_stale_balance_when_upstream_times_out` | 注入必然超时的 fetch + 预置 10 分钟前的缓存（已过 300s TTL）；断言 Ok 且 `stale == true`、数值等于缓存值 | 旧代码 `fetch_balance` 报错 → `Err(UpstreamError)` → 502 |
| `should_collapse_concurrent_balance_requests_to_one_upstream_call` | 并发 10 个同 id 请求 → 上游调用计数 == 1 | 旧代码无单飞，10 次真实上游调用 |
| `should_batch_rpm_lookups_in_snapshot` | 计数包装的 RpmTracker，N=20 时 rpm 锁获取次数 == 1 | 旧代码 20 次 |
| `should_not_hash_secrets_in_live_snapshot` | `live_snapshot` 返回类型不含 `refresh_token_hash` / `api_key_hash`（编译期保证） | 旧代码走 `snapshot`，每帧两次 SHA256 |
| `should_cap_by_model_cardinality` | 灌 1000 条不同 model 名 → `by_model().len() <= 上限`，`other` 桶计数 == 未知名记录数 | 旧代码返回 1000 |
| `should_survive_truncated_cache_file` | 写半截 JSON 后 `load_balance_cache_from` 不 panic 且 **warn! 出来** | 旧代码静默返回空 map = 静默丢全部余额 |

#### 上线验证
```bash
# 余额降级：临时把 proxy 指向黑洞，或用 iptables DROP 上游，点面板余额
curl -s -w '\n%{http_code} %{time_total}\n' -H "X-Admin-Key: $K" \
  https://k1ro.skiapi.dev/api/admin/credentials/1/balance   # 期望 200 + stale:true + <9s
# 大查询不影响写入
curl -s -H "X-Admin-Key: $K" 'https://k1ro.skiapi.dev/api/admin/usage/recent?limit=0' >/dev/null &
ssh ws-vps 'journalctl -u kirostudio --since "1 min ago" | grep -i "dropped\|channel"'   # 期望零命中
ssh ws-vps 'gateway-status brief'
```

#### 回滚
`MAX_RECENT_LIMIT` 下调是纯行为收紧（前端「全部」返回变少），无格式变更。`spawn_blocking` / broadcast 是内部结构调整。二进制回滚即完成。

---

### 批 5 — 多选模式 + 强制删除

**关键现状纠正**：多选**已经实现**（`dashboard.tsx` 的 `selectedIds` / `toggleSelect` + 9 个批量按钮 + `credential-card.tsx` 的 Checkbox）。用户缺的是**常驻多选模式开关** —— 现在选中只能靠卡片左上角复选框或按住 Ctrl/Cmd 左键（`handleCardClick` 第一行就 `if (!(e.ctrlKey || e.metaKey)) return`）。前端成本很低。

**「强制删除」的真实语义由后端一道硬门定义**（已复核）：`token_manager.rs:5252` `anyhow::bail!("只能删除已禁用的凭据（请先禁用凭据 #{}）", id)`，被 `classify_delete_error` 归 400。所以**强制删除 = 绕过「必须先禁用」这道门**，不是「跳过回收站」。

#### 改动清单

**后端**

| 文件 | 函数 | 改法 |
|---|---|---|
| `src/kiro/token_manager.rs` | `delete_credential`(:5240) | 拆 `delete_credential_inner(&self, id, force)`，硬门(:5252) 改 `if !entry.disabled && !force`。保留 `delete_credential(id) = inner(id, false)` 以免动既有调用方与 :8819 的现有测试。强制删启用号时 `warn!` 带 email/inflight 留痕 |
| 同上 | `delete_credential` 回滚分支 | 改为保存整个被摘走的 `CredentialEntry`（`inflight: Arc<AtomicU32>` 与 `refresh_lock: Arc<TokioMutex>` 可直接搬回；`last_full_reprobe_at` / `reprobe_in_flight` 需重建），落盘失败原样 push 回去。现状硬编码 `disabled: true` / `success_count: 0` —— 加上 force 后，**落盘失败会把启用号静默变成禁用号并清零统计** |
| 同上 | 新增 `delete_credentials_batch(&self, ids, force) -> Vec<(u64, Result<(),String>)>` | 一次 entries 锁内逐个校验+摘出+推 trash，锁外统一清 affinity/cooldown/rpm/model_blocklist/rate_limiter 与 `select_highest_priority`，**只落盘一次**。现状每删一个号做 `persist_trash` + `persist_credentials` + `save_stats` 三次全量写，43 号 = 129 次原子写（`persist_credentials` 还要整份重写、开加密时整体重新封） |
| 同上 | `purge_trash_batch`(:4436) | 收紧 `Some(list) if list.is_empty() => 返 0 + warn!`。现状 `_` 同时覆盖 None 与 `Some(vec![])` → **传空数组即清空整个回收站**；且 `if purge_credential(id).is_ok()` 吞掉全部错误只回一个计数（违反本仓「绝不静默吞错」约定） |
| `src/admin/router.rs` | 路由 | `.route("/credentials/batch-delete", post(batch_delete_credentials))`，放 `/credentials/{id}` 附近并加同款注释（该文件已有「静态段与 `{id}` 同层共存，matchit 静态优先」的先例：`trash`、`balances/cached`） |
| `src/admin/types.rs` | 新增 | `BatchDeleteRequest` / `BatchDeleteResultItem` / `BatchDeleteResponse`，照抄 `ImportKeysResponse` 的既有约定（部分失败仍 200，逐条看 `results[].ok`） |
| `src/admin/service.rs` | `batch_delete_credentials` | 含 `balance_cache` 逐 id remove + 一次 `save_balance_cache`（对齐现有 `delete_credential` 的缓存清理） |

**端点签名**

```
POST /api/admin/credentials/batch-delete
{ "ids": [12,15,27],       // 必填，非空，上限 200（线上号池 43 量级）
  "force": true,           // 默认 false = 保持"只能删已禁用"
  "purge": false,          // 默认 false = 进回收站
  "confirm": "DELETE 3" }  // force||purge 时必填，服务端校验

200（部分失败也 200）
{ "success": true, "total": 3, "deleted": 2, "purged": 0, "failed": 1, "elapsedMs": 41,
  "results": [ {"id":12,"ok":true}, {"id":15,"ok":true,"forced":true},
               {"id":27,"ok":false,"error":"凭据不存在: 27"} ] }

400：ids 空 / 超上限 / force||purge 但 confirm 不匹配
```

**服务端强制的打字确认是必须的，不能只做前端弹框**：adminKey 明文存 localStorage + 全仓无 CSP，任何 XSS 都能直接调管理 API。`confirm` 严格等于 `format!("DELETE {}", ids.len())` 让攻击者必须理解业务语义才能构造，不能单纯重放 URL。

**前端**

| 文件 | 改法 |
|---|---|
| `dashboard.tsx` | 加 `selectionMode` state + 工具栏 toggle（lucide `ListChecks`）；退出时 `deselectAll()`；挂 Escape 退出。加「本页全选 / 全部全选（带总数 N）/ 反选」三个 ghost 按钮；计数 Badge 显示「已选 X / 共 N（本页 Y）」。翻页不丢选中已由现有 effect 保证（剪枝依据是全量 `data.credentials` 而非当前页） |
| `credential-card.tsx` | 判定改 `if (!selectionMode && !(e.ctrlKey \|\| e.metaKey)) return`；cursor 改 `(ctrlHeld \|\| selectionMode)`；selectionMode 时给 Card 加「可选中」底态；**删掉死参数 `additive`**（两处调用都传 `true`，而 dashboard 侧 `onToggleSelect={() => toggleSelect(id)}` 把形参丢弃，从未生效） |
| 工具栏 | 9 个按钮已 flex-wrap 平铺，再加会挤爆。⚠️ **已复核 `components/ui/` 下没有 dropdown-menu**（有 `confirm-dialog.tsx` / `dialog.tsx`）→ **不要为此新引 radix 依赖**，改用「更多操作」按钮 + 复用现有 Dialog 列操作清单 |
| 新增 `batch-delete-result-dialog.tsx` | props `{ open, onOpenChange, submitting, response }`。批量删改成单次请求后没有逐条进度 → submitting 时 spinner，返回后按 `results[]` 渲 ✓/✗ + error，底部「成功 X / 失败 Y / 已进回收站 Z」+「打开回收站」跳转 + 失败项一键重试 |
| 强制删除确认框 | **独立弹框**（不复用通用 `confirmState`），要求手动输入 `DELETE N`；框内列出将被删的号（email/#id + 是否启用 + 是否 inflight>0 + 是否 isCurrent），启用号数量红字单列 |

#### 回归测试

| 测试名 | 断言 | 旧代码为何失败 |
|---|---|---|
| `force_delete_rollback_restores_original_entry` | enabled 号 + cache_dir 不可写使 `persist_trash` 失败 → entries 中该号 `disabled == false` 且 `success_count` 保持原值 | 旧回滚分支硬编码 `disabled: true` / `success_count: 0` |
| `batch_delete_persists_once` | 计数包装的落盘 → 删 10 个号，`persist_credentials` 调用次数 == 1 | 旧代码 10 次（外加 10 次 persist_trash + 10 次 save_stats） |
| `purge_trash_batch_rejects_empty_list` | `purge_trash_batch(Some(vec![]))` 返 0 且不清空回收站 | 旧代码 `_` 分支 → 清空全部 |
| `batch_delete_requires_confirm_when_forced` | `force: true` + `confirm` 不匹配 → 400 | 新端点 |
| `should not wipe the admin key on an HTML 401 from the reverse proxy` | content-type `text/html` 的 401 不调 `removeApiKey` | 旧拦截器无法区分 Caddy basic_auth 的 401 与 adminKey 失效 → 白白清掉正确的 key |

#### 上线验证
```bash
# 造一个已禁用的测试号，走批量端点
curl -s -X POST -H "X-Admin-Key: $K" -H 'Content-Type: application/json' \
  -d '{"ids":[999],"force":false,"purge":false}' \
  https://k1ro.skiapi.dev/api/admin/credentials/batch-delete
# 确认落回收站可恢复
curl -s -H "X-Admin-Key: $K" https://k1ro.skiapi.dev/api/admin/credentials/trash
```
`ssh ws-vps 'gateway-status'` 确认号池容量未受影响。**先在测试号上验证，不要拿生产号试强制删除。**

#### 回滚
新端点是纯新增，老端点行为不变 → 二进制回滚即完成。⚠️ **已删除的号在回收站里，回滚后仍需手工 restore**（`restore_credential` 恢复为禁用态、id 不变）。

---

### 批 6 — OTA 一键升级打通

**「检查更新」必然失败的确切失败点已定位**（已复核）：`update.rs:140` 拼 `repos/{repo}/tags`，而私有仓库 `dwgx/KiroStudio-skiapi` 的 tags 与 releases **均为空**。有 token 时候选被裁成 direct 单个 → API 返 200 + `[]` → `versions.is_empty()` → continue → 候选耗尽 → 错误文案「无法获取远端版本信息（所有镜像失败）」**完全误导**（不是镜像失败，是仓库没有任何 tag）。当前线上 token 为空字符串 → `update_token()` 判 None → 4 个 gh-proxy 镜像匿名请求私有仓库 → 全 404 → 同一条错误。

**第二层阻塞（结论：必须走 Release）**：OTA 唯一下载路径是 `github.com/{repo}/releases/download/{tag}/{asset}`（:145 与 :420），而 `deploy-build.yml` 产出的是 `actions/upload-artifact@v4`（zip 包装、按 run id 而非 tag 寻址、需 `actions:read`、14 天过期），`update.rs` **全文无 artifact 代码路径**。改代码去读 artifact 的性价比为负（要新增 list runs→找 artifact id→下 zip→解包→定位成员→校验一整条链，且失去 tag 语义与 sha256 独立信道）。

#### 改动清单（运维 + 代码，运维先行）

**运维侧（先做，不需要改代码就能验证前半段）**
1. GitHub 网页建 fine-grained PAT，只勾 `dwgx/KiroStudio-skiapi`，权限 Contents:read（+ Actions:read 仅在将来要读 artifact 时才需要）。
2. 填 `/etc/kirostudio/update.env` 的 `KIROSTUDIO_UPDATE_TOKEN`，`KIROSTUDIO_UPDATE_REPO=dwgx/KiroStudio-skiapi`，重启服务。
3. 在私有仓库打 tag `v0.7.44`（**必须等于 Cargo.toml 的 0.7.44**，release.yml test job 已有一致性门禁），触发 release.yml 建 Release + 上传 `kirostudio-linux-x86_64` + `.sha256`（资产名与 `ASSET_BIN` 的 Linux x86_64 分支一致）。
4. ⚠️ **本地已是 0.7.44 ≥ 该 tag** → `has_update = false` 是**正确结果**，不要因为「点了没反应」再排查一遍权限。真正验证通路要等下一次真实发版（首个 OTA tag 建议 ≥ v0.7.45）。

**代码侧**

| 文件 | 函数 | 改法 |
|---|---|---|
| `src/admin/update.rs` | `fetch_versions`(:259) / `github_api_candidates`(:140) | 改读 `repos/{repo}/releases?per_page=10`（天然按创建时间倒序、分页可控，顺带消除 tags 无 `per_page` 只取首页 30 条的隐患）。**按 HTTP 状态分类报错**：401/403→「OTA 令牌无效或缺少 Contents:read」，404→「仓库不存在或令牌无该仓库权限」，200 但空数组→「仓库尚无 Release，无可升级目标」，网络错→保留现文案 |
| 同上 | `check_for_updates` | 过滤 `draft == false`，且**只保留 assets 同时含 `ASSET_BIN` 与 `{ASSET_BIN}.sha256` 的 release**。一次请求同时解决「Release 是否存在」「本平台资产是否齐」——现状 `has_update` 只证明 tag 存在，面板会亮出一个必然 404 的升级按钮（触发窗口：tag 推了但构建失败；或三个 build job 并行追加资产期间） |
| 同上 | 新增 `read_body_capped(resp, cap, what)` | 先 CL 预检，再 `while let Some(chunk) = resp.chunk().await?`，超 cap 即 bail。替换 :394 与 :437 两处 `resp.bytes()`。**已知缺陷 #2 确认仍只修一半**：:386/:431 的 200MiB 预检只在 `Content-Length` 存在时生效，镜像用 `Transfer-Encoding: chunked` 即绕过。参照实现就在同仓 `admin_ui/router.rs` 的 `resp.chunk()` 累计截断循环。sha256 单独给 `MAX_SHA_BYTES = 4096`（一个「200MiB 的 sha256 文件」本身就是攻击信号） |
| 同上 | `http_client`(:247 附近) | 下载单独构建 client：`.connect_timeout(10s)` + **不设总 timeout**（改空闲超时），靠 `MAX_DOWNLOAD_BYTES` 兜住体积。现状 `.timeout(30s)` 是含 body 读取的整请求超时，15.3MB 资产要求持续 >4 Mbps —— 跨境到 objects.githubusercontent.com 达不到，会表现为「点升级等 30 秒报所有镜像下载失败」而运维误判成权限问题。同时给单候选加 3 次指数退避（1s/2s/4s），日志打已下载字节数以区分「连不上」与「下得慢」 |
| 同上 | `perform_update` | rename 之前 `let _ = tokio::fs::remove_file(exe.with_extension("health")).await;` |
| `src/common/health_marker.rs` | `read_status` | 解析 `version=` 行，仅当等于 `LOCAL_VERSION` 才置 `health_confirmed = true`，否则 false + 把旧版本号放 `health_detail`。现状 `health_confirmed: health_body.is_some()` 不比对版本 → 升级后 0-30s 窗口面板同时显示「本版已稳定确认」和「回滚点仍在」，自相矛盾 |
| `install-binary.sh` + `deploy/install-service.sh` | unit 生成段 | 加 `EnvironmentFile=-/etc/kirostudio/update.env`（`-` 前缀 = 缺失不阻断启动）+ `install -m 0600` 建含注释的模板。**全仓 `EnvironmentFile` / `KIROSTUDIO_UPDATE` 零命中** → 任何按仓库脚本重装的机器会静默把 OTA 指回 `update.rs:32` 的默认值 `dwgx/KiroStudio`（已冻结的公开仓库） |
| `install-binary.sh` | launchd plist 生成段 | `ProgramArguments` 指向 wrapper 脚本：开头 exec `rollback-guard.sh`（同一份逻辑）再 exec 二进制，顺便 source 同一个 env 文件。现状 macOS 只有 `KeepAlive=true`、无 pre-launch 钩子 → 坏二进制无限拉起、`.bak` 无消费者。若暂不做，至少在 `update_status` 返回 `rollbackAutomated: false` 让面板明说「本平台无自动回滚」 |
| `.github/workflows/release.yml` | build matrix | `ASSET_BIN` 枚举 6 个 OS×ARCH，release.yml 只产 4 个 → aarch64 Linux / aarch64 Windows 必 404。二选一并写死结论：补交叉构建 job，或在 `update.rs` 用 cfg 门显式标「OTA 不支持，请手动更新」 |
| `src/admin/update.rs` | `with_update_auth` 文档注释 | 改为「reqwest 默认在跨 host 重定向时剥离 Authorization，这正是预签名 URL 需要的；**不要**给 updater client 设置保留敏感头的自定义 redirect 策略」。现注释把「靠框架行为」写成了「协议无害」（S3 端点收到同时带 Authorization 与查询串签名的请求会返 400） |

#### 回归测试

| 测试名 | 断言 | 旧代码为何失败 |
|---|---|---|
| `check_reports_no_release_when_tags_empty` | httptest 返 200 + `[]`；error 文案含「尚无 Release」 | 旧代码返「所有镜像失败」 |
| `chunked_response_without_content_length_is_capped` | chunked、无 CL、超 cap 的响应 → Err 含「超过上限」 | 旧代码 `resp.bytes()` 把整个 body 读进来返 Ok |
| `release_without_platform_asset_is_not_offered` | mock assets 只含 windows 的 release → Linux 下 `has_update == false` + error 说明 | 旧代码只看 tag → `has_update = true` → 点了 404 |
| `stale_health_marker_from_other_version_is_not_confirmed` | 写 `version=0.0.1` 的 .health → `health_confirmed == false` | 旧代码返 true |
| `asset_api_route_used_when_token_present` | token 存在时候选 URL 为 `api.github.com/repos/.../releases/assets/`；无 token 仍 github.com + 4 镜像 | 仅在 OTA-3 验证为需要时才加（见下） |

#### 上线验证 — OTA-3 是本批最大未知，先验证再改代码

私有仓库能否用 `Authorization: Bearer <PAT>` 直接下 `github.com/{repo}/releases/download/...`，我**无法实证**（该私有仓库当前零 Release）。Release 建出来后先跑：
```bash
curl -sS -o /dev/null -w '%{http_code}\n' -H "Authorization: Bearer $PAT" \
  https://github.com/dwgx/KiroStudio-skiapi/releases/download/v0.7.44/kirostudio-linux-x86_64
```
- 返 200 → 现状可用，不改 `download_asset`。
- 返 404/403 → 新增 API 资产路径：① `GET api.github.com/repos/{repo}/releases/tags/{tag}` 拿 assets 数组与 id；② `GET api.github.com/repos/{repo}/releases/assets/{id}` 带 `Accept: application/octet-stream`（会 302 到预签名 URL，reqwest 跨 host 剥 Authorization 正是 S3 需要的）。sha256 文件也要一起改，且**仍固定 api.github.com 直连**，才不破坏「哈希与二进制信任根解耦」这条红线。

另需实机确认（无 SSH 时无法验证）：
```bash
ssh ws-vps 'systemctl cat kirostudio | grep -E "ExecStartPre|EnvironmentFile|Environment="'
tr '\0' '\n' < /proc/$(pidof kirostudio)/environ | grep KIROSTUDIO_UPDATE_REPO   # 只查 REPO
tr '\0' '\n' < /proc/$(pidof kirostudio)/environ | grep -c KIROSTUDIO_UPDATE_TOKEN  # 令牌只看存在性，不回显
```
若 unit 没挂 `ExecStartPre=-.../rollback-guard.sh`，则 OTA 的 `.bak` 在生产上同样无消费者 —— 与 macOS 情形等价。CLAUDE.md 写二进制在 `/opt/kirostudio/bin/kirostudio`、热更用 `kirostudio.prev`，而 `rollback-guard.sh` 默认 `KIRO_WORKDIR=/home/dwgx_user/KiroStudio`、OTA 用 `.bak` —— **线上部署形态与仓库脚本不一致，必须实机对齐**。

#### 回滚
OTA 代码改动不影响转发链路。`update.env` 是新增文件，删掉即回到默认（但默认指向已冻结公开仓库，属于「不可用」而非「危险」）。在此期间用 `ssh ws-vps 'kirostudio-update'` 更新，功能等价。

---

### 批 7 — 低优先与依赖实验的项

| 项 | 内容 | 为什么排最后 |
|---|---|---|
| S7 亲和分档 | `affinity.rs:37` 的 `ttl: Duration::from_secs(30 * 60)` 固定 30 分钟、`get`(:42) 是布尔语义、`touch`(:70) 可无限续期 → 远超 Anthropic prompt cache 的 5 分钟 TTL。改三档：`age < 5min` = Hot（硬亲和短路）/ `5-15min` = Warm（只作 sort_key 固定加分项，让负载与健康能压过它）/ `≥15min` = Cold（丢弃重选）；`touch` 只在 Hot 档续期 | **依赖 EXP-1/EXP-2 结论**。上游是否真有隐式前缀缓存折扣尚未证实（11% 命中率里的 `cache_read` 是我们自己注入的估算值，非真值）。若上游没有前缀缓存折扣，Hot 档的 5min 硬亲和本身也没有收益依据，设计应退化为「亲和仅作弱偏置」 |
| S8 流绝对墙钟上限 | `create_sse_stream` 的 `select!` 加第三条分支 + `streamMaxWallClockSecs`（默认宽松 900s，0=关闭），超时走既有 `mark_transport_error` 收尾 | **需先只加埋点观测 P99 正常流时长再定默认值**。现只有 502 max=1077s 一个数据点，无法区分「正常超长思考流」与「病态慢速流」，贸然设值会掐断合法长流 |
| 后端 F5 复合索引 | 建 `(outcome, ts_ms DESC)` / `(credential_id, ts_ms DESC)` / `(model, ts_ms DESC)` / `(session_id, ts_ms DESC)`；分页改 keyset（前端传上一页最后一行 ts_ms 作 cursor）替掉 OFFSET | 严重度取决于 traces 表实际行数（只读约束下未查线上 DB）。**先跑 `SELECT COUNT(*) FROM traces` 与 `page_count*page_size` 再定优先级**。批 4 的 `spawn_blocking` + limit 收紧已经把最坏情况从「拖死 worker」降到「慢一点」 |
| 批量优先级 | 补 `handleBatchSetPriority`（单号 API 已存在），沿用现有 `confirmState` + ok/fail 计数 + 汇总 toast | 纯增量便利功能 |
| `placeholderData` | `useUsageTimeseries` / `useUsageRecent` / `useUsageRecentLive` 各加 `placeholderData: (prev) => prev`（v5 写法）；配 `isPlaceholderData` 加半透明遮罩 | 观感优化。全仓只有 `ops-detail-dialogs.tsx` 一处用了这个模式 |
| CSP | 给 admin_ui 静态响应加 `Content-Security-Policy` | **这是批 5 全部 XSS 假设的根因**，但属独立安全 PR，不要和多选功能混在一起。做了它，批 5 的服务端 confirm 校验仍应保留（纵深防御） |
| CredentialCard memo | `React.memo` + 父级 `useCallback` 稳定回调 + `balanceMap` 改传整个 Map | 卡片已分页（规模压到一屏），且字段含每轮都变的 rpm/inflight/health，重渲染本身难避免。收益远小于批 1 |

---

## 四、需要用户决策

| # | 决策点 | 我的推荐与理由 |
|---|---|---|
| **D1** | **「强制删除」的确切语义**：是「不用先禁用」还是「彻底删不进回收站」？ | **推荐前者**：`force`（绕禁用门）与 `purge`（跳回收站）拆成两个独立参数，`purge` 默认 false。理由：删除本已是软删（进 trash.json，settings-page 有完整回收站 UI 可恢复，`restore_credential` 恢复为禁用态、id 不变），而 adminKey 明文存 localStorage + 全仓无 CSP，一旦 XSS 就能整池清空 —— 保留回收站是被打穿后**唯一的兜底**。trash 受 `trashRetentionDays` 自动清理约束，留存成本近零。若用户要的是后者，`purge` 默认值需反过来，安全权衡随之改变 |
| **D2** | **OTA 要不要改成打 Release** | **必须走 Release，不要改代码去读 artifact**。已复核 `update.rs` 全文无 artifact 路径，而 artifact 有四重不兼容（zip 包装、按 run id 而非 tag 寻址、需 actions:read、14 天过期）。保留 `deploy-build.yml` 服务 VPS hotswap（它刻意不建 Release，避免每次部署构建污染版本线），OTA 另走 tag→release.yml。改造成读 artifact 的代价是一整条新链路 + 失去 tag 语义与 sha256 独立信道，性价比为负 |
| **D3** | **affinity TTL 从 30min 缩到 5min 会否牺牲缓存命中** | **先做 EXP-1/EXP-2 再动**，本批不改。理由：11% 命中率里的 `cache_read` 是我们自己注入的估算值（`docs/CACHE-EXP0-RESULT.md` 已确证上游不发 prompt cache 真值，`metadataEvent` 只有 `stopReason`），所以「缩短 TTL 会掉命中率」这个担忧**当前没有可测量的依据**。已知代价侧是确定的（一个 session 占 35% 流量、gini 0.325）。若必须现在动，只做 Warm 档（5-15min 从硬亲和降为 sort_key 加分项），保留 Hot 档不变 —— 这样最坏情况是「5 分钟内的缓存收益全保住」 |
| **D4** | **`refetchOnWindowFocus` 是否开回 true** | **推荐开回 true**。`main.tsx` 的注释只解释了 retry 策略、没解释这一项，且 `use-usage.ts` 的注释「重新可见时 react-query 借由 focus 事件自动复轮」与它**直接矛盾**（注释描述的行为并未生效）。这些 admin 端点都是只读内存/本地聚合（实测 0.3ms、零上游、无封号风险），收益是切回标签页即刻新数据 + error 态立刻自愈。⚠️ 改前值得确认一下是不是当初为压某个抖动才关的 —— 若是，用 `staleTime` 节流而非关闭 focus refetch |
| **D5** | **关键 query 是否设更长 `gcTime`** | **推荐给号池与 config-snapshot 设 `gcTime: 30 * 60 * 1000`**。批 1 的 `error && !data` 依赖缓存里有 `data`，而 v5 默认 gcTime 5min —— 502 持续超 5 分钟后缓存被 GC、`data` 变 undefined，仍会退回整页错误卡。代价是一点内存，换长故障期间的可读性。（⚠️ 我未复核当前是否已配 gcTime，落地前 grep 一次） |
| **D6** | **SSE / 面板端点是否加并发连接上限** | **推荐先做批 4 的 broadcast 改造，暂不加硬上限**。broadcast 把每帧成本从 O(标签页) 摊成 O(1)，已解决主要问题；连接数本身仍无界（每条一个 tokio 任务 + keep-alive 定时器），但这取决于「面板是否可能被多人/多标签页长期挂着」——**需用户确认使用场景**。若是单人运维，不值得加 |
| **D7** | **批量端点 ids 上限定多少** | **推荐 200**（线上号池 43 量级，留 4 倍余量）。400 错误里说明上限值 |
| **D8** | **强制删「正在服务的号」（inflight>0）要不要后端拒绝** | **推荐不拒绝，只在确认框里标红 + 后端 `warn!` 记 inflight 值**。「号卡住了要拔掉」正是用户要强制删除的动机之一，后端拒绝会让该场景无法完成。⚠️ 但实现前必须确认一件事：`InflightGuard::drop` 与 `report_success` / `report_failure` 在 entry 已从 entries 移除时走哪条分支（我只复核了 `delete_credential` 本身不检查 inflight，未读 Guard drop 与上报路径）。**若上报会 panic 或误伤其它号，则必须改成拒绝** |
| **D9** | **删除 isCurrent 号是否需要更高确认强度** | **推荐在确认框里单独标注**。删 current 会触发 `select_highest_priority` 换号，进而丢该号的 prompt cache 亲和 —— 按本项目既有教训（LiteLLM 事故：换号=缓存全丢，日花费涨 2-3 倍），这一项值得单列 |

---

## 五、明确不做（避免下一个人重复提）

| 不做的事 | 理由 |
|---|---|
| **削减前端轮询频率** | **请求量不是瓶颈**。逐条清点所有 `refetchInterval`：overview ≈44 req/min、ops ≈20 req/min + 1 条 SSE、usage ≈12-25、settings ≈4，三窗齐开 ≈130 req/min ≈ 2.2 rps，后端实测 0.3ms/次 → 占用可忽略。且 React Query v5 默认在 `document.hidden` 时暂停 interval，SSE 也在 hidden 时主动 abort → 后台标签页近似零负载。真正该改的是错误态语义、超时、重试与断线体验 |
| **引入虚拟化列表 / 新前端依赖** | 大列表都已分页（凭据卡分页、usage recent PAGE_SIZE=20）。`components/ui/` 下**已复核无 dropdown-menu**，但有 dialog / confirm-dialog → 批量操作菜单用现有 Dialog 实现，不为一个下拉引 radix |
| **把 `refetchInterval` 的 `document.hidden ? false : MS` 手写判断当性能优化来做** | v5 默认已覆盖，那些写法是重复保险。清理它属整理，不是优化 —— 可以顺手做，但别当成收益 |
| **改 `rateLimitEnabled` / `inboundRpmAuto` 回默认值** | 线上刻意偏离，依据在 `ws-vps/docs/02-tuning.md`：前者的每号最小间隔 1000ms 会在 241ms 处踢开亲和绑定 → 每次换号 → prompt cache 全丢，而 5339 样本实测速率与 429 率相关性仅 +0.09；后者的内置 AIMD 是单向棘轮（429 砍半、回升要 20s 静默 ×N，而实测每 6.4s 一次 429）→ 单调下滑锁死，实测卡在 30 RPM 而号池能跑 216 |
| **开 `trustForwardedHeader`** | sub2api 的透传白名单（`gateway_service.go` 的 `allowedHeaders`）里没有 `X-Forwarded-For` 且不转发它 → 开了也拿不到真实用户 IP，而 KiroStudio 看到的 `client_ip` 恒为服务器自身地址，按它配 IP 黑名单会一封封掉全部流量。已知问题 #6 的修法（把 flag 传进 handler 层统一口径）仍值得做，但**不改配置值** |
| **启用输入压缩来解上游 5MiB 限制** | 实测 6.88MB→6.66MB 只省 3%，压缩后仍超限，而它改写历史中段**破坏前缀缓存**。收益为负 |
| **任何形式的消息搬移 / 重排 / system 上提** | LiteLLM 事故：中途 system 上提顶层 → CC 命中率 90%→25-45%，日花费涨 2-3 倍 |
| **给 `promptCacheEnabled` 补读取点** | 它当前是死配置（代码里零读取点）。补读取点等于**新增行为**，而 `docs/CACHE-RFC.md` 的 L0 层价值尚未由 EXP-1/EXP-2 证实。要么先做实验，要么把这个配置项标记为 deprecated —— 不要因为「有个开关没接线」就接上 |
| **把 `Fault` 枚举直接当运行时 FaultKind 复用** | `diagnosis.rs` 的 `Stage` / `Fault` / `diagnose_refresh(status, body)` 确认**非死码**（被 `admin/types.rs`、`admin/error.rs`、`token_manager.rs`、`auth/idc.rs` 引用），是理想的分类内核。但我**未逐行读变体清单** —— 现有变体可能只覆盖上号阶段（refresh / device auth / profile probe）。落地前先核对覆盖度，需要则**扩展**该枚举（补 `Stage::Runtime` + 运行时变体）而非另建平行枚举 |
| **给 `resp.bytes()` 只加更大的上限** | 已知缺陷 #2 的正解是**流式截断**（同仓 `admin_ui/router.rs` 已有两处参照实现），加大上限治不了 chunked 绕过 |
| **在 VPS 上编译** | 那台机器 4 核是瓶颈，编译会抢死正在服务的 sub2api。走 GitHub Actions 出 musl static-pie |
| **全仓 `cargo fmt`** | 历史事故：有会话跑了全仓 fmt，把别人整树的改动回退冲掉。当前工作树有 47 个其他会话的未提交文件 |

---

## 六、防回归守卫（让「越跑越慢」这类问题结构性不再出现）

S1 是这一节存在的理由：**decay_idle 是今晚刚写的、带测试的、代码审查过的修复，而它被同一个函数几行后的一行赋值完全失效**。靠代码审查保证「每个惩罚状态都有下降路径」已经证明不可靠。以下四层按性价比排序。

### G1 — 元测试：只推进时间，且必须走真实读路径（最高价值，最便宜）

```rust
#[test]
fn test_all_penalty_counters_have_time_only_recovery() {
    // 造最坏状态：连续 429 打到跳闸、多轮半开失败、族级 403
    // 然后【只推进时间】——零成功、零请求
    // 断言：p_avail >= 0.40 ∧ consecutive_429 == 0 ∧ open_count 下降 ∧ consecutive_suspicious == 0
}
```

**两条硬规则**（写进 `CONTRIBUTING.md` 与 health.rs 模块文档）：
1. **禁止在测试里手工写私有状态字段**。health.rs:699 / :747 现在正是这么做的（`s.last_touch = Instant::now() - Duration::from_secs(180)`），注释还自陈「用 last_touch 回拨模拟空闲（不真等 60s）」—— 这就是 S1 漏网的**直接原因**：测试绕过了它要验证的那条读路径。
2. 改为**注入时钟**：`HealthTracker` 持一个 `Clock` trait 对象（生产 = `Instant::now()`，测试 = 可推进的 `MockClock`）。测试只调公开 API + 推进时钟，永不碰字段。

这一条能抓住 S1、S2、S3、S5 全部四类，且抓住**将来任何新增的惩罚计数**（只要新计数被纳入元测试的断言列表）。

### G2 — 反饥饿强制探测：把断言变成强制执行（结构性兜底）

批 3 的 `sort_key` 最高优先位 `starved`。**这一条的价值不在修哪个 bug，而在于它让所有单向状态缺陷的最坏后果降级为「偶尔损失一个探测请求」**：

- 任何号超过 `STARVATION_PROBE_SECS` 未被选中且未 disabled / 未 cooling / 未被 blocklist 挡 → 本轮无条件排首位。
- **不依赖任何具体字段是否有衰减路径**。S1/S2/S3/S5 即使全都不修，池子也不会出现「有效容量 6→3」。
- 配套元测试 `test_no_credential_starves_beyond_probe_window`：3 号池，一号 health 打到最差档，连发 200 次选号，断言它至少被选中一次。旧代码该号被选中 **0 次**。

### G3 — `TimeDecayed` trait + maintenance ticker 主动调用（切断耦合根因）

```rust
trait TimeDecayed { fn decay(&mut self, now: Instant); }
```
`HealthState`、`CooldownEntry`、`CredentialEntry` 的惩罚字段全部实现，由已存在的 5 分钟 maintenance ticker（`main.rs` 有两个 300s interval，`token_manager` 侧已在调 affinity/rpm/health 的 cleanup）**主动调用一次 `decay_all(now)`**。

**这一层的意义**：主动调用切断了「衰减只在读路径里发生，而读路径自己刷掉了衰减时钟」这类耦合 —— 也就是 S1 的根因。即使有人将来又在读路径里写坏了时钟，ticker 这条独立路径仍会推进衰减。

### G4 — 单一收口，把「容易漏」变成「编译器/类型系统不允许漏」

| 收口 | 替代掉的散点 | 效果 |
|---|---|---|
| `CredentialEntry::clear_transient_counters()` | `reset_and_enable` / 全池自愈 / `set_disabled(false)` 三处各自手写清零列表 | 新增一个惩罚计数时，只在一个地方补 —— 而不是希望三处都记得。`reset_and_enable` 漏了 `consecutive_suspicious` 就是这类漏 |
| `persist_disabled_state(id)`（今晚已建） | 5 条自动禁用路径 | 已完成。批 2 的自愈块也接进来 |
| `classify_upstream_failure(status, &body, endpoint, cred) -> FailureVerdict` | 对话路径 7 道判定 vs MCP 路径 4 道判定 | 消除口径分叉（MCP 路径完全没有 suspend / 临时风控 / INVALID_MODEL_ID 分类，403 一律 `report_failure` → 累计 3 次以 TooManyFailures 禁用 → **临时态被贴永久型标签**，正是历史事故的同一误判形态）。新增信号只改一处 |
| `read_body_capped(resp, cap, what)` | `update.rs` 两处 `resp.bytes()`、将来任何外部下载 | 让「加上限」不再是每个调用点各自记得的事 |
| `api/client.ts` 单例（三个 axios 实例合一） | `usage.ts` / `ops.ts` / `credentials.ts` 各建实例 | timeout 与 401 拦截器**不可能再漏配**。`credentials.ts:34` 的注释已写明 timeout 是「登录卡顿的成因之一」——这个坑踩过一次却没推广到另两个实例 |
| `useQueryWithStale` 自定义 hook | `dashboard.tsx:722` / `settings-page.tsx:1620` + 4 张卡片各自手写 `if (error)` | 把「`error && !data` 才降级，`error && data` 渲旧值 + 过期条」封成一个 hook。约定：**页面组件不得直接读 `error` 做整页分支** |
| `is_rpm_saturated` 标 `#[cfg(test)]` | 注释级的「不可在持锁时调用」警告 | 非测试代码若调用即编译失败。parking_lot 非重入，一旦发生就是整个网关选号死锁 |

### G5 — 可观测：把「没被抓住」变成「至少被看见」

同一个 maintenance ticker 里跑不变量检查：对每个未禁用凭据，若 `now - last_selected_at > 300s` 而同期池内总请求数 > 0 → `tracing::error!` + 记 `recovery_metrics` 的 `starvation_detected{cred_id}` 计数。面板 `recovery-metrics` 端点已存在，可直接展示。

配套面板字段（`ops.starvation.*` / `ops.health.{openCount,admitProbSeed}` / `ops.modelBlocklist.*`）：把 `open_count`、`admit_prob_seed`、`model_blocklist` 的 `next_probe_at` 暴露出来。**这三个值现在完全不可见** —— 而它们正是自锁的直接证据。若线上早就能看到某个族键的 `open_remaining_secs` 顶在 1800，本次审计的一半工作都不必要。

---

## 附：本轮复核纠正的几处战线判断

| 战线原判断 | 复核结果 |
|---|---|
| 后端并发战线：trace_db 的 PRAGMA 未知，若是默认 `journal_mode=delete` 则读写互斥更糟 | **WAL + synchronous=NORMAL 已开启**（trace_db.rs:147-148，模块文档 :5 也写明）→ 悲观估计可下调。但**未见 `busy_timeout`**，建议补 |
| 多选战线：`components/ui/` 下是否有 dropdown-menu 未确认 | **确认没有**（有 dialog / confirm-dialog / select）→ 批量操作菜单必须用现有 Dialog，**不要为此引 radix** |
| 调度战线：decay_idle 是否真的不生效「按代码推演」 | **已实地确认**：`p_avail_with_load_ref`（health.rs:343-356）的执行顺序就是 `tick_circuit → decay_idle → s.last_touch = now`，而 `decay_idle`（:240-241）第一行就是 `idle < 5.0 → return`。S1 从「推断」升级为「确证」 |
| 前端战线：`refetchOnWindowFocus: false` 可能有历史原因 | `main.tsx:10-19` 的注释只覆盖 retry 策略，**该项无任何注释说明** → 无文档化理由，但仍建议改前问一句（D4） |
| 封号战线：`persist_credentials` 只写 disabled | **确认**：`credentials.rs:203` 只有 `disabled: bool`，`token_manager.rs:3158` 只写 `cred.disabled = e.disabled`，入池 :1565 无条件回填 `Manual` |
| OTA 战线：`fetch_versions` 读 tags | **确认** `update.rs:140` 为 `repos/{repo}/tags`，无 `per_page`；:145 与 :420 两处下载 URL 均为 `releases/download` 形态；:386/:431 的 CL 预检后紧跟 :394 的 `resp.bytes()` |

**仍未复核、落地前必须确认的三项**：① `InflightGuard::drop` 与 `report_success/report_failure` 在 entry 已移除时的分支（决定 D8）；② `diagnosis.rs` 的 `Fault` / `Stage` 变体清单（决定复用还是扩展）；③ 前端 `n` 紧凑字段的后端产出点（决定两张标签表能否合并）。