# 内置「上游 429 吸收层」实施方案（下沉版）

> 取代 `SPEC-shield-builtin.md` 的 §2–§4（那版把吸收层包在 `handle_*_request` 外层 → 每轮重入
> `acquire_admission`，即 REVIEW 的 BLOCKER 1）。本版：**吸收循环下沉到 `call_api_with_retry`
> 内部、`acquire_admission()` 之后** —— 准入闸门在循环**上方**，物理上不可能被重入，不靠调用方传 flag。
> SPEC 的 §1/§5/§6（配置清单 / 可观测 / shield 退役五步）仍有效，本文只改被否决的部分。
> **B1**：循环下沉到 provider.rs:820 之后 + 源码守卫钉死顺序与 `acquire_admission(` 全文恰 1 处。
> **B2**：已修（:822 带 `inbound_admission_timeout=1`），下沉后结构上不可达，仍留显式 `None` + 测试。
> **B9**：判据改 `剩余 > delay + ABSORB_MIN_USEFUL_ROUND(20s)`，每轮墙钟上限 `min(45s, 剩余)`。

## 1. 插入位置（provider.rs，行号会漂，改前重新 grep）

`:50` `MAX_REQUEST_RETRY_BUDGET_SECS=45` · `:40/:73` `ABSOLUTE_MAX_TOTAL_RETRIES=12` /
`compute_max_retries=min(total*3,12).max(1)` · `:731` `fn call_api_with_retry` · `:745-:805`
六个链内去重集（`rate_limited_this_call` / `suspended_this_call` / `suspicious_failovers_this_call`
/ `auth_failed_this_call` / `region_corrected_this_call` / `model_unavailable_attempts`）+
`attempts_used` · `:788` `call_started` · **`:820-:826` `acquire_admission()` bail（吸收循环插在
这之后）** · `:828` `for attempt` · `:836-:846` 45s 墙钟闸门 · `:941` 成功 return · `:1377` for
结束 · `:1382` `bump_failover_exhausted` · `:1389-:1454` `overload_fallback_model` ·
`:1456-:1479` `final_error` + **唯一的 `emit_record`**。
`'absorb: loop` 只包 **:828–:1384**：`overload_fallback_model` 与 `emit_record` 留在循环**外**，
否则一条客户端请求落 N 条失败记录、面板失败数被轮次乘倍（正是 #21 想修的反面）。

## 2. patch 骨架

```rust
// ── provider.rs:826 之后（acquire_admission 的 bail 之下）──────────────────
let absorb = AbsorbPolicy::from_config(&self.token_manager.config());
// deadline 与 call_started 同源:准入排队(最长 30s)也计入预算。从此刻起算的话客户端可见
// 延迟 = 30+45 = 75s ≈ shield p50 73.2s,等于把病根搬进来。
let absorb_deadline = call_started + absorb.budget;
let mut absorb_round: u32 = 0;
let mut attempts_base: u32 = 0;                  // 跨轮累计,喂 attempts_used
'absorb: loop {
    let round_started = Instant::now();
    let full = Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS);
    // enabled=false 时两者恒等于旧值 → 逐字节等价(§8)
    let round_clock = if absorb.enabled { round_started } else { call_started };
    let round_budget = if absorb.enabled {
        full.min(absorb_deadline.saturating_duration_since(round_started)) } else { full };
    for attempt in 0..max_retries {
        attempts_used = attempts_base + attempt as u32;            // 原 :832 改此行
        if attempt > 0 && round_clock.elapsed() >= round_budget {  // 原 :836 改判据
            tracing::warn!(...); break;
        }
        // :847-:1376 循环体**一字不改**,除四处 AIMD 门(§5)与成功分支加 1 行:
        //   :929 → if absorb_round > 0 { bump_absorb_recovered(); }
    }
    attempts_base = attempts_used + 1;
    if real_failover_happened {              // 原 :1382 移进轮内,每轮独立判定
        bump_failover_exhausted(); real_failover_happened = false;
    }
    if !absorb.enabled || absorb_round >= absorb.max_rounds { break 'absorb; }
    let Some(err) = last_error.as_ref() else { break 'absorb };
    let Some(class) = absorb_class_of(&err.to_string()) else { break 'absorb };
    if matches!(class, AbsorbClass::Suspended) && !absorb.absorb_suspended {
        bump_absorb_suspend_skipped(); break 'absorb;
    }
    let delay = absorb.backoff(class, absorb_round);
    if !should_start_another_round(absorb_deadline, Instant::now(), delay) {
        bump_absorb_budget_exhausted();
        tracing::warn!(rounds = absorb_round, "吸收预算不足一轮,原样透传 429 + Retry-After");
        break 'absorb;
    }
    tokio::time::sleep(delay).await;
    absorb_round += 1; bump_absorb_round();
    // ⚠️ **不要**重置 last_error:重置后若下一轮无新错误,final_error 落「已达最大重试次数」
    //    通用串 → map_provider_error 兜底 502、丢掉 Retry-After(客户端从此不退避)。
}   // :1389 起(overload_fallback_model / fail_record / emit_record)留在循环外,原样不动
```

`AbsorbPolicy` 从 `token_manager.config()`（token_manager.rs:2037，ArcSwap）读，**不新建 TIER3
static**：TIER1 的 `reload_config`（:2045）在 admin 存盘后原子换入，热更路径现成。

## 3. 预算记账

三个时钟：`call_started`（:788，供 `latency_ms`/`started_at` 的 TTFB 同源，**不能动**）·
`round_clock`（起点=本轮 failover，上限 `round_budget`，供原 :836 闸门）· `absorb_deadline`（=
`call_started + budget`，默认 45s，供吸收准入判据）。`round_budget = min(45s, absorb_deadline -
round_started)` 就是「不会超预算」的**机制**：一轮的墙钟上限被剩余预算夹住 ⇒ 吸收轮次与 failover
轮次不是各算一套，而是**后者被前者显式配额**；唯一溢出来源是闸门只在 attempt 顶部检查
（最坏 = 一次在途上游调用），与今天 45s 闸门性质相同。

```rust
/// 一轮最坏「能跑出结果」的下限。取 MAX_TRANSIENT_WAIT_SECS(token_manager.rs:1687) 而非另造
/// 数字:全池冷却时 acquire_context 最多等 20s 才 bail,剩余不足 20s 的一轮结构上只可能在
/// transient wait 里烧完再返回同一个 429 —— 白打一轮上游 + 客户端白等。
const ABSORB_MIN_USEFUL_ROUND: Duration = Duration::from_secs(20);
fn should_start_another_round(deadline: Instant, now: Instant, delay: Duration) -> bool {
    deadline.saturating_duration_since(now) > delay + ABSORB_MIN_USEFUL_ROUND   // ⚠️ 不是 >= min_delay
}
```

实算（单号池 `compute_max_retries(1,1)=1` ⇒ 每轮只打 1 次上游；全池冷却是**毫秒级 fast-fail**，
token_manager.rs:3410/3425/3444/3496/3518 五处 bail）：`retry_after=10` 时 t=0 判 45>30 ✓ → 睡 10
→ 轮 1（budget 35）→ t≈10 判 35>30 ✓ → 睡 10 → 轮 2（budget 25）→ t≈20 判 25>30 ✗ 停 ⇒
**2 轮、总 20s**，客户端见 429 从 <2s 变 20s，远小于 shield p50 73.2s；真打上游的一轮（1~3s）
吃不满 45s。放大上限 `max_rounds=3 × compute_max_retries ≤ 12` = **最坏 36 次上游调用**
（单号池实为 4 次）对比 shield 的 60×12=720；且吸收**不重入准入**，
`1500 次准入/分钟 vs 288 令牌`那条塌陷链断掉。

## 4. 可吸收判定（复用既有谓词，零新增字符串）

`handlers.rs` 两个私有谓词改 `pub(crate)`（一词改动）：`is_upstream_rate_limited`（:518）、
`is_upstream_temporarily_suspended`（:554）。分类器**放 handlers.rs 紧邻它们**（同文件才防漂移）：

```rust
pub(crate) enum AbsorbClass { PoolCooldown(u64), UpstreamRateLimit, Suspended }
pub(crate) fn absorb_class_of(err_str: &str) -> Option<AbsorbClass> {
    // ① 准入超时=网关自己的背压,重试只是把同一请求塞回同一个满桶。下沉后结构上不可达
    //    (provider.rs:820 的 bail 在循环外),显式列出防将来闸门被移进循环。
    if err_str.contains("inbound_admission_timeout=1") { return None; }
    // ② 模型对本号池**永久**不可用(否则把 404 死循环搬进网关)。**必须排在 retry_after_secs
    //    之前**:token_manager.rs:3510 那条不带 retry_after,而 :3518「模型级过滤但可恢复」那条
    //    带 —— 顺序反了就把永久态当可恢复态吸收。
    if err_str.contains("model_unsupported_by_pool=1") { return None; }
    // ③ 全池冷却 / 整池 RPM 饱和 / 真耗尽:号池算出的**真实**恢复秒数
    if let Some(secs) = parse_retry_after_secs(err_str) { return Some(PoolCooldown(secs)); }
    if is_upstream_rate_limited(err_str) { return Some(UpstreamRateLimit); }
    if is_upstream_temporarily_suspended(err_str) { return Some(Suspended); }
    None   // 配额(MONTHLY_REQUEST_COUNT/QUOTA)/网络/TLS/其它 4xx/未知:一律不吸收
}
```

`parse_retry_after_secs` 从 handlers.rs:703-708 与 :729-734 那份**重复**的解析代码抽出，三个调用点
共用（消掉漂移面，而非靠测试比对两份拷贝 —— REVIEW §7e）。退避：`PoolCooldown(secs)` 用**号池进程内
真值**（`cooldown.rs` 的剩余秒数，无需 HTTP 头往返），其余 `min_delay << round`；统一
`clamp(min_delay, max_delay)`；`secs=0` 也睡满 `min_delay`（#9：无 sleep 的 continue 就是忙等
死循环）。**403 Suspended 默认不吸收**，开启时 `max_rounds` 硬钉 1（`from_config` 里
`if absorb_suspended { max_rounds = max_rounds.min(1) }`）—— REVIEW §4：
`SELF_HEAL_BASE_BACKOFF=60s` 存在的意义就是停止试探，15s 内重打同账号直接抵消它。

## 5. AIMD 二次放大的规避

四个 AIMD 上报点（provider.rs **:984** 临时风控 / **:1066** suspend / **:1308** 上游 429 /
**:1332** 5xx 风暴）全部加同一道门：`if absorb_round == 0 { …report_upstream_*() }` ——
**只报第一轮，不是跳过全部**（跳过全部会退回「403/5xx 风暴时 AIMD 毫无反应」那条已修缺陷）。
依据：AIMD 的输入语义是「**客户端**请求撞上游的频率」，一条客户端请求无论吸收几轮都只是
**一个** RPM 事件。若逐轮上报：`MD_DEBOUNCE_SECS=3`（throttle.rs:40）挡不住吸收轮次
（退避 ≥150ms、`PoolCooldown` 常 8~15s，全部 >3s 穿窗）→ 每轮真降一档 → `last_md_nanos`
被反复推进（throttle.rs:252）→ `maybe_step_up` 的 `AIMD_PROBE_SECS=20s` 静默期永不满足
（实测每 6.4s 一次 429）→ RPM 单调滑到 floor 锁死，与已修的 #11 是**同一死锁的第三条路径**。
升档侧不用管：`maybe_step_up` 在 `acquire_admission`（token_manager.rs:3178）内，吸收不重入
准入 ⇒ 结构上只在准入时调一次。凭据级惩罚（`report_rate_limited_with_retry_after` / `report_suspicious_activity` /
`report_server_error`）**不加门**：已被 `rate_limited_this_call` 去重（:1314/:988/:1329），
而该集合声明在 `'absorb: loop` **外**（:752）⇒ 跨轮共享 ⇒ 同号在整条客户端请求内只罚一次，
冷却不会被吸收轮次指数拉长（15→72s 那条自造雪崩）。**所有链内去重集与 `attempts_used`
都必须留在 `'absorb: loop` 之外** —— 本方案的第二条承重不变量。

## 6. 六项配置 + 镜像一致性

`upstreamRetryAbsorb{Enabled=false, BudgetSecs=45, MaxRounds=3, MinDelayMs=150,
MaxDelaySecs=15, Suspended=false}`。与 SPEC 两处差异：**BudgetSecs 60→45**（与
`MAX_REQUEST_RETRY_BUDGET_SECS` 同值，`round_budget` 的 `min()` 才同源；60 只多买延迟不多买轮次）；
**`maxAttempts` 改名 `maxRounds`**（`attempts` 在 provider 已指 failover 轮次，同名两义必混）。
**下沉后不需要 TIER3 static**（直接读 ArcSwap），SPEC 的「6 字段 × 3 镜像 = 18 个可写错点」降到 12：

- `config.rs:115`（`pub cc_auto_buffer` 之后）6 字段全 `#[serde(default)]`/`default="fn"` ·
  `:644` 5 个 `default_absorb_*()` · `:873` `Config::default()` 6 项**必须调 `default_*()` 不写字面量**
- `admin/types.rs:653` snapshot 6 字段 · `:770` `UpdateConfigRequest` 6 个 `Option<..>`
  （🔧 **实施更正**：SPEC 与本文原稿都说 `:1433` 有个 `ConfigSnapshotResponse::default()` 手写
  字面量、是「第三处默认值镜像」。**该 impl 不存在** —— `:1433` 是一个**测试夹具**的结构体
  字面量。故只有两处默认值镜像（`default_*()` 与 `Config::default()`），真实漂移面是
  「快照有没有逐字段读 config」，由 `absorb_snapshot_maps_every_field_from_config` 守卫）
- `admin/service.rs:1533` snapshot 映射 6 行 · `:1766` 一带 6 个 `if let Some(v)=req.x { if
  v!=config.x { …; absorb_changed=true } }` · `:2314` 一带**不需要** TIER3 setter
- 🔴 **`admin/service.rs:2242`**（`hot_or_display_changed` OR 链）加 `|| absorb_changed`。**漏这行
  则面板开关存了盘但 `reload_config`（:2252）不触发 → ArcSwap 仍旧值 → 开关静默无效**（下沉方案
  唯一新增风险点，必须有测试）
- `recovery_metrics.rs:107`（`counters!` 末尾）`absorb_rounds` / `absorb_recovered` /
  `absorb_budget_exhausted` / `absorb_suspend_skipped`（宏自动出 camelCase 并进 recovery-metrics）
- 前端：`types/api.ts:349/:431` · `settings-page.tsx:230/:321/:1537/:2117`（form 类型 / 初值 / diff /
  1 `<Switch>`+4 数字输入，同 `ccAutoBuffer` 卡片）· `api/ops.ts:118` + `ops-page.tsx:116`
  `METRIC_ITEMS` 4 项（`absorbRecovered` 不带 `warn`）· i18n 三语键表见 SPEC §1

## 7. 「回退即 FAIL」测试清单

> ✅ **已逐条突变验证**（不是纸面声明）：对下表前 6 条各做一次真实回退突变，确认
> 未突变时 PASS、突变后 FAIL，然后还原。过程中抓到**本守卫自己的一个 bug**：
> `aimd_reports_only_on_first_absorb_round` 第一版用 `body.get(a..b).unwrap_or(&body[..abs])`
> 取窗口，而中文注释使字节偏移常落在多字节字符中间 → `get` 返 `None` → 回退成**整段前缀**
> （含别处的门）→ 断言恒真：删掉一处门测试照样通过，等于白写。已改为向前挪到
> `is_char_boundary` 再切片，无回退分支。**这正是"测试必须真的会失败"要防的形态。**

| 测试（位置） | 删掉哪行 → 哪个断言失败 |
|---|---|
| `absorb_config_default_goes_through_default_fns`（config.rs） | 把 `Config::default()` 里任一项写成字面量 → `assert_eq!(cfg.x, default_absorb_x())` FAIL |
| `absorb_disabled_by_default`（config.rs） | enabled 默认改 true → `assert!(!Config::default().upstream_retry_absorb_enabled)` FAIL |
| `absorb_fields_absent_from_json_default_to_off`（config.rs） | 摘掉任一 `#[serde(default...)]` → 旧 config.json 反序列化报 missing field FAIL（线上既有配置加载失败那条路径是 exit(1)） |
| `absorb_snapshot_maps_every_field_from_config`（service.rs 源码守卫） | 快照里任一项写死字面量 → 断言 `x: config.x,` 存在 FAIL（写死会让面板永远显示默认值） |
| `admission_gate_is_outside_absorb_loop`（provider.rs 源码守卫 `include_str!`） | 把 `acquire_admission` 移进循环 → `find("acquire_admission") < find("'absorb: loop")` FAIL；并断言 `acquire_admission(` 全文恰 1 处 → 第二个调用点即 FAIL（**B1 的机械防线**） |
| `emit_record_and_fallback_stay_outside_absorb_loop`（源码守卫） | 把 `overload_fallback_model` 挪进循环 → 断言其位于 `break 'absorb` 之后 FAIL（防每轮都打一次备用模型）。🔧 **实施更正**：`emit_record` 那一半**实际由编译器兜底**（`fail_record` 在循环后才构造，挪进去即 E0425，已实测），本测试对它只是意图声明 |
| `dedup_sets_declared_outside_absorb_loop`（源码守卫） | 把 `rate_limited_this_call` 的 `let mut` 挪进循环 → 断言其位置早于 `'absorb: loop` FAIL（防跨轮重复罚同号、冷却 15→72s） |
| `aimd_reports_only_on_first_absorb_round`（源码守卫） | 去掉四处任一 `absorb_round == 0` → 断言 4 个上报点全被该条件包裹 FAIL（**§5 死锁第三路径**） |
| `absorb_budget_gate_requires_full_round`（provider.rs 纯函数单测） | 判据换回 `剩余 >= min_delay` → `assert!(!should_start_another_round(now+25s, now, 10s))` FAIL（25>10 但 25<10+20） |
| `admission_timeout_is_never_absorbable`（handlers.rs） | 删 `absorb_class_of` 第一条 `None` → 喂 provider.rs:822 原串 → `is_none()` FAIL（**B2**） |
| `model_unsupported_is_never_absorbable`（handlers.rs） | 删第二条 `None` **或把它排到 `retry_after_secs` 之后** → 喂 token_manager.rs:3510 原串 FAIL |
| `quota_and_transport_are_not_absorbable`（handlers.rs） | 谓词放宽成裸 403 / `AccessDeniedException` → `MONTHLY_REQUEST_COUNT` 与 `error sending request` 被判可吸收 FAIL |
| `absorb_config_change_triggers_reload`（service.rs，沿用 `cc_auto_buffer_changed` 同款） | 删 service.rs:2242 的 `\|\| absorb_changed` → 断言 `hot_or_display_changed` FAIL（面板开关静默无效那条） |

## 8. `enabled=false` 逐字节等价

三处差异在关闭时各自退化为恒等：① `round_clock = call_started` ⇒ 墙钟起点是今天同一个 `Instant`；
② `round_budget = min(45s, 45s) = 45s` ⇒ 判据与 :836-:837 逐字节相同；③ `attempts_base` 只在
`break 'absorb` **之后**赋值、循环只跑一遍 ⇒ 恒 0 ⇒ `attempts_used == attempt as u32`，与 :832
相同。其余全部短路在 `if !absorb.enabled || … { break 'absorb }` 之前：不读 `last_error`、不调
`absorb_class_of`、不 sleep、不 bump 任何 absorb 计数器；四处 AIMD 门恒真 ⇒ 上报不变；
`emit_record` 恰一次。钉死（不需要真上游）：`absorb_policy_disabled_is_identity`
（`from_config(&Config::default())` → `effective_max_rounds() == 0`，把「关 ⇒ 零额外轮次」变成
可断言的纯函数）· `absorb_round_zero_keeps_legacy_wall_clock`（`round_budget(false, 任意剩余)
== 45s` 且 `round_clock_start(false, a, b) == a`）· §7 两条源码守卫兼管「关时不多落记录/不多上报」。

## 9. 不做 / 不改

**不改 shield**：退役走 SPEC §6 五步（`enabled=false` 上线取基线 → 面板开 → shield
`MAX_ATTEMPTS` 降 2 → Caddy 4 条路由逐条切 → 停但不删），串联期网关先吸收、剩余漏给 shield。
~~**不换 503**：预算耗尽保留最后一次 429 + Retry-After（CC 对 429 的退避是正确路径；shield 换 503
反而让 Cursor 掐会话）。~~
⇒ 🔴 **本条已过期（2026-08-06），且括号里的因果写反了**：shield 换 503 恰恰是**为了避免**
Cursor 掐会话（Cursor 见 **429** 会掐，见 503 不会）。**换 503 已实现为可选项**
`upstreamRetryAbsorbExhaustedStatus`（默认仍 429，Cursor 用户置 503），用户 2026-08-05 明确要求。
锚点：`handlers.rs` 的 `ABSORB_BUDGET_EXHAUSTED_MARKER` + `provider.rs` 的
`absorb_gave_up_after_rounds`。详见 `docs/TASK-CANVAS-IPPOOL-SHIELD.md` C7。**PR 说明必须写**：吸收层只压**客户端可见的 429**，不减少打向上游的请求量，
容量口径修正是账号级 RPM 那条独立 PR —— 否则会误判「开了吸收层但上游 429 没少 = 没效果」。
线上对照：`inboundTargetRpm=337` 被 `inboundRpmMax=300` 夹住；单号 #445（eu-central-1）2858 请求
99.8% ≈ 57 RPM 持续、#444 3228 请求 99.1% ≈ 65 RPM ⇒ 单号池 `compute_max_retries(1,1)=1`，
吸收轮次是该配置下**唯一**的重试来源，也是 `max_rounds` 默认 3（而非 SPEC 的 4）仍够用的依据。
