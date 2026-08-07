# region 错配直接烧号 —— 修复设计

> 事故（2026-08-03 晚）：`config.region` us-east-1 → eu-central-1，#449 的 `ksk_` 在 eu-central-1
> 无效 → 403 `AccessDeniedException: The bearer token included in the request is invalid.` →
> `report_failure` ×3 → `TooManyFailures` 自动禁用且 `persist_disabled_state` 落盘（重启也回不来）。
> ⚠️ 行号取自当前工作树。你给的 1203/1218/1223/1158 = 我这里 1213/1228/1233/1169（约 10 行漂移），
> 改前重新 grep 锚点文本。

## 1. 烧号链路（已核实）

| 位置 | 行为 |
|--|--|
| `provider.rs:1169` | region 自动纠正入口，条件 = `403 && is_feature_not_supported && is_external_idp_credential` → `ksk_` 号两道都不过 |
| `provider.rs:1213-1215` | force-refresh 显式 `&& !is_api_key_credential()`（**正确**，`ksk_` 无 refreshToken） |
| `provider.rs:1228 / :1232-1233` | 直落 `AuthFailed` → `report_failure`；`token_manager.rs:1658 MAX_FAILURES=3`、`:4198` 达阈值 `disabled=true`+`TooManyFailures`、`:4155` 落盘 |
| `endpoint/mod.rs:259` | `default_is_bearer_token_invalid` 纯子串匹配，**不区分** region 错配 / key 真废 |

仓里三处注释都写明这个签名就是 region 错配：`service.rs:1082-1090`、`service.rs:3335-3352`、
`token_manager.rs:2161-2166`。网关认得签名，却在热路径上计失败。

`ksk_` 号 region 来源：`credentials.rs:453 effective_api_region` = 凭据 `api_region`（过白名单）
→ `config.api_region` → `config.region`；`:470 effective_upstream_region` 因 api_key 无
`profile_arn` 而落到 `region`/`auth_region` → 同上。**凭据没写 `apiRegion` 时 100% 吃
`config.region`** —— #448/#449 正是此状态（推号方没给 region）。（`config.region` 在
`service.rs:1699` 算进 `restart_fields`，所以是「改配置 + 重启/热载」共同生效，结论不变。）

## 2. 候选方案

| | (a) 热路径试其它 region | (b) 入池时预探 |
|---|---|---|
| 改哪里 | `provider.rs:1213` 前插分支；URL 在 `:893` 由 `ctx.credentials` 决定 → 要改写本地 `api_region` 后**重发**，而 `continue` 会回到 `:837 acquire_context`（重新选号，可能换号）→ 必须 restructure 循环或复制一份 send 逻辑 | `service.rs:1184`（`add_credential` 后那次 `get_usage_limits_for`）→ 换成「按候选 region 逐个探，200 的写回 `api_region`」。复用 `token_manager.rs:563 get_usage_limits`（`:616` 已按 `is_api_key_credential()` 发 `tokentype: API_KEY`），无需新 HTTP 代码 |
| 碰热路径 | **碰，最重**：串行探 3 个 region = 3 次上游往返压在用户请求里，且共用 `MAX_REQUEST_RETRY_BUDGET_SECS=45`（`provider.rs:50`）墙钟 | 不碰 |
| 僵尸号 | 无（只持久化验证过的 region） | 无（不禁用、不计失败） |
| 最坏行为 | 真废 key 每次请求白烧 3 次往返 + 耗尽墙钟 → 用户侧超时率上升 | 上号变慢（N 次 GET）；且**救不了**池里已有的号（#448/#449），也救不了事后改 `config.region`（今晚正是这条） |
| 判定 | 与 `provider.rs:1160-1166` 已写死的裁决冲突（「昂贵 reprobe 绝不上同步对话热路径」）→ **否** | 必要但不充分，与 (c) 并行做 |

### (c) 403 bearer-invalid 给一次换 region 机会，失败才计失败
天真版（同步换 region 重试）= (a)，坑同上。**推荐版（异步裁决）**：403 bearer-invalid 且
`is_api_key_credential()` 时 ①**不**调 `report_failure`；②设一段**可自动恢复**的短冷却挪出
候选（不是 `report_auth_cooldown`，见 §6.2）；③`trigger_region_probe(id)` 后台异步；
④**失败计数交给探测裁决** —— 找到可用 region 则写回 + 清冷却 + `failure_count` 归零
（`token_manager.rs:4290` 语义），候选全废则在后台任务里 `report_failure(id)`。
- 热路径：只多两次 `entries` 短临界区（置 flag + 设冷却），无网络。本请求正常 failover 换号。
- 僵尸号防线（三条缺一不可）：① 探测起不来（取 token 失败 / in-flight 抢不到 / 6h 冷却内 / 开关
  关）→ **必须**回落 `report_failure`，否则真废 key 拿不到判决 = 僵尸；② 每号 6h 内最多一轮
  （复用 `REPROBE_ALL_BAD_COOLDOWN`，`token_manager.rs:746`），冷却内的 403 走原路径计失败；
  ③ 单轮最多探 3 个 region（§4）。
- 最坏：真废 key 从「3 次请求即禁用」变成「首个 403 触发一轮 ≤3 次 GET，探完即计失败」→ 死亡
  延后一轮探测（数秒），仍然会死。

## 3. 推荐：(c) 异步裁决 + (b) 入池预探
(c) 治存量与「事后改 config」，(b) 让新号自带 `apiRegion`，把 (c) 的触发率压到接近 0。收敛路径：
- **region 错配**（#449 型）：首个 403 → 60s 软冷却 + 后台探测 → 命中 us-east-1 → 写回
  `credentials[449].apiRegion` + `persist_credentials()` + 清冷却 → ≤1 分钟后照常调度，
  **此后不再吃 `config.region`**（凭据字段优先，`credentials.rs:453`）。失败计数恒 0，永不禁用。
- **key 真废**：首个 403 → 后台探完 3 个 region 全非 200 → 任务内 `report_failure` → 第 2/3 次
  请求（6h 探测冷却内走原路径）各计 1 次 → 达 3 次禁用。终点与今天一致，只慢一轮探测。面板
  不会显示「可用」：软冷却期它是冷却态，探测失败后立即回到失败计数轨道。

## 4. region 候选集
来源 `src/kiro/regions.rs`。**不要**遍历 `KIRO_DIALOG_REGIONS`（34 项，是白名单不是候选集）。按序
取前 3（`MAX_REGION_PROBES_PER_EVENT = 3`）：① 该号自己的 `api_region`/`region`（非空且未试过）；
② **池内实测有效 region**，按该 region 上 `success_count` 之和降序 —— 池子跨 region（#449 在
us-east-1 成功 376 次、#450 自带 eu-central-1），这是唯一有实测依据的排序键，且纯本地计算
（`entries.lock()` 一次遍历，无网络）；③ `config.effective_api_region()`（`model/config.rs:954`）；
④ 兜底 `PROFILE_PROBE_REGIONS`（`regions.rs:60`，6 项）按序补齐。

去重后截断到 3；每项必须过 `KiroCredentials::is_supported_region()`（`credentials.rs:440`，防污染
值拼进 host）。**不设全局兜底 region**，见 §6.1。
探测：`get_usage_limits(&cred_clone_with_region, &cfg, &token, proxy)`，200 = 可用。
⚠️ **待验证假设（EXP-1，动手前先做）**：它打 `management.{region}.kiro.dev`，对话打
`runtime.{region}.kiro.dev`。用 #449 的 key 分别打 management.us-east-1 / eu-central-1，确认信号与
runtime 一致（预期 200 / 403 bearer invalid）。若不一致，退路是用一次极小
`generateAssistantResponse` 探测（耗真实配额）。

## 5. 新增配置项
只加**一个 kill switch**：`apiKeyRegionAutoProbe`，bool，默认 `true`，定义在 `model/config.rs`
（`region` 附近，`:30-41` 一带）。不加阈值/region 列表（列表按 §4 推导；多加配置只会造出第二个
「假数字」）。三层镜像位置（照 `cooldown_enabled` 的 TIER1 范式）：
- TIER1 原子镜像：`token_manager.rs` 加 `AtomicBool` 字段，在 `pub fn reload_config`（`:2045`）内 store。
- Admin 落盘+热应用：`service.rs:1846` 的 TIER1 块内加
  `if let Some(v) = req.api_key_region_auto_probe { … hot_changed = true; }`，**不要** push 进 `restart_fields`。
- 请求体 / 前端：`admin/types.rs` 加 `Option<bool>`；`components/settings-page.tsx` + 三份
  `i18n/resources/{en,ja,zh}.json`。

新计数器：`recovery_metrics.rs:93-95` 的 `counters!` 宏内加 `region_swap_ok` / `region_swap_fail`。
**不要复用** `region_reprobe_ok/fail`（那是 external_idp profile 重探，混用会让两个机制的排障
数据不可分）。前端三处同步：`api/ops.ts:128`、`components/ops-page.tsx:123`、三份 i18n（`:830`）。

`CredentialEntry` 加两字段（`token_manager.rs:1195-1210` 一带），4 处构造点同步初始化（`:1854 /
:5895 / :6088 / :6269`，编译器会逼你改全）：`region_probe_in_flight: AtomicBool`（per-id 抢占
守卫，同 `reprobe_in_flight` 范式）、`last_region_probe_at: Mutex<Option<Instant>>`（上次「候选
全废」时刻，`REPROBE_ALL_BAD_COOLDOWN` 内不再重探）。

## 6. 明确不该做
1. **不改 `config.region` / `config.api_region` 兜底值**。池子跨 region 是事实（#449 有效
   region=us-east-1，#450 自带 eu-central-1），任何单一全局值都烧掉另一半。正确杠杆是**每凭据
   `apiRegion`**。
2. **不用 `report_auth_cooldown`（`token_manager.rs:4418`）当软冷却**：它落
   `CooldownReason::AuthenticationFailed`，`cooldown.rs:92 is_auto_recoverable=false` → 实际走
   `long_cooldown_secs=86400`（`cooldown.rs:159`）。24h 不可自动恢复 = 面板显示「冷却中」的
   僵尸，比烧号更难发现。→ 新增 `CooldownReason::RegionMismatchProbing`（baseline 60s，
   `is_auto_recoverable=true`），改 `cooldown.rs` 4 处：`:28` enum 本体、`:59 default_duration`、
   `:85 is_auto_recoverable`、`:99 description`。
   ⚠️ **顺带发现（本次不做，要记）**：`provider.rs:1201` 的 FEATURE_NOT_SUPPORTED 分支也调
   `report_auth_cooldown`，而 `token_manager.rs:6543` 的后台重探成功只清
   `last_usage_403_feature_not_supported`、**不调 `clear_cooldown`** → 注释承诺的「重探成功后
   自动恢复」在代码里不存在，那号仍被冻 24h。本方案成功路径必须显式 `clear_cooldown`。
3. **不一律跳过 bearer-invalid 的失败计数**（你指出的僵尸号坑）。计数只能**延后**到探测有结论；
   任何「探测没跑成」的分支都必须回落 `report_failure`。
4. **不把 `provider.rs:1169` 的 `is_external_idp_credential()` 放宽到 api_key**：
   FEATURE_NOT_SUPPORTED（profile 未在该 region 开通）与 bearer-invalid（token 未在该 region
   授权）是两个信号；混进一个分支会让 profile 重探跑到 api_key 号上，而它没 profileArn，
   `probe_all_usable_profiles` 对它必然空手。
5. **不同步探测**（见 (a)）。

## 7. patch 骨架
`provider.rs`，插在 `:1213` 那个 force-refresh `if` **之前**：
```rust
// 403 bearer-invalid + api_key 号 = region 错配的已知签名（service.rs:1082 / :3335 /
// token_manager.rs:2161）。绝不在此 report_failure：那把「region 配错」当「key 报废」，
// 3 次即 TooManyFailures 落盘禁用（今晚 #449 就这样被烧）。
if status.as_u16() == 403
    && endpoint.is_bearer_token_invalid(&body)
    && ctx.credentials.is_api_key_credential()
    && region_probe_this_call.insert(ctx.id)
{
    // 返回 false = 探测起不来（in-flight / 6h 冷却 / 开关关）→ 必须回落原失败路径，
    // 否则真废的 key 拿不到判决 = 僵尸号。
    if self.token_manager.trigger_region_probe(ctx.id) {
        last_outcome = crate::usage::RequestOutcome::AuthFailed;
        last_error = Some(anyhow::anyhow!(
            "{} 403 bearer-invalid（疑 region 错配，后台探测中，本请求换号）: {} {}",
            api_type, status, body
        ));
        continue; // 不 report_failure
    }
}
```
循环外（`:776` 附近，镜像 `region_corrected_this_call` 的去重惯例）加
`let mut region_probe_this_call: HashSet<u64> = HashSet::new();`。

`token_manager.rs`（照 `:6590 trigger_background_reprobe` 抄结构，含 `InFlightGuard`）：
```rust
/// 返回 true = 已接管（起了后台探测，调用方**不要**计失败）；false = 调用方按原路径计失败。
pub fn trigger_region_probe(self: &Arc<Self>, id: u64) -> bool { /* 抢 region_probe_in_flight
   + 6h 双检 + 开关门控 → detached spawn */ }

/// 逐候选 region：clone 凭据 → 覆盖 api_region/auth_region → get_usage_limits() → 200 即命中。
/// 命中：写回 + persist_credentials() + clear_cooldown(id) + failure_count 归零 + bump_region_swap_ok()。
/// 全废：*last_region_probe_at = Some(now) + report_failure(id) + bump_region_swap_fail()。
async fn probe_and_fix_api_region(&self, id: u64, creds: &KiroCredentials, token: &str) -> bool
```

`service.rs:1184`（方案 b）：那次 `get_usage_limits_for` 换成「api_key 号 → 按 §4 候选逐个探，
第一个 200 的写回 `api_region` 后再取订阅等级」；非 api_key 号行为逐字不变。

## 8. 「回退即 FAIL」测试清单
`add_credential` 会调真实上游（`service.rs:1184`），provider 的 403 分支也要真实上游才走得到 →
这两处**只能**源码级守卫（惯例见 `provider.rs:1770
force_refresh_must_skip_api_key_credentials_at_both_sites`）。needle 必须**运行时拼接**（`include_str!`
会把字面量自己也读进来，那个测试的注释里有教训）。1–3 在 `provider.rs`，4–5 在 `token_manager.rs`：
1. 新分支必须出现在 `let has_available = if auth_failed_this_call.insert` **之前**
   （`src.find(a) < src.find(b)`）。守「被挪到 report_failure 之后 = 等于没加」。
2. 该分支体内**不得**含 `report_failure`，**必须**含 `trigger_region_probe`。
3. `trigger_region_probe` 返回值必须被消费（写成 `if self.token_manager.trigger_region_probe(...)`
   而非裸调用）。僵尸号第一防线。
4. `probe_and_fix_api_region` 的「全废」分支**必须**含 `report_failure` —— 僵尸号第二防线，全方案
   里唯一让真废 key 死掉的地方。
5. 命中分支必须含 `clear_cooldown`（否则复刻 §6.2 那个 24h 僵尸缺陷）。
6. 现有 `provider.rs:1770` 与 `service.rs:3351 multi_open_must_inherit_api_region_from_parent` 保持
   绿（新分支绝不顺手改掉那两处 `!is_api_key_credential()`）。

纯行为单测（无网络）：候选集抽成纯函数（入参 = 凭据 + 池内 `(region, success_count)` + config
region）后断言 ① 号自己 region 排第一 ② 池内按 success_count 降序 ③ 去重 ④ 截断到 3 ⑤ 非白名单
被剔除（喂 `"evil.com/"`）；`CooldownReason::RegionMismatchProbing` 的 `is_auto_recoverable()==true
&& default_duration()<=300s`（守「照抄 AuthenticationFailed 写成 86400 不可恢复」）；
`region_swap_ok/fail` 出现在 camelCase 快照里（照 `recovery_metrics.rs:113
test_snapshot_serializes_camelcase`）；`apiKeyRegionAutoProbe` 默认 `true` 且源码里**不出现在**
`restart_fields.push` 附近（守 TIER1 语义别退化成「要重启」）。
