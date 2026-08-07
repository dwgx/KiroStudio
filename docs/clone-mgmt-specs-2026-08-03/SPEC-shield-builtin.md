## 1. 配置项

`src/model/config.rs` — 六项，全部 `#[serde(default)]`，默认值使 `enabled=false` 时**逐字节等价旧行为**。

**P1** anchor（唯一）：
old_string:
```rust
    #[serde(default = "default_cc_auto_buffer")]
    pub cc_auto_buffer: bool,
```
new_string:
```rust
    #[serde(default = "default_cc_auto_buffer")]
    pub cc_auto_buffer: bool,

    /// 网关内置「上游 429 吸收层」总开关。**默认 false**（等价旧行为，逐字节零变化）。
    ///
    /// 开启后：`handle_*_request` 在**客户端已提交 0 字节**的前提下，对 provider 返回的
    /// 可重试 429（全池冷却 / 上游账户级速率限流）就地退避重打，而不是把 429 直接吐给客户端。
    /// 这是把 VPS 上外置的 `kiro_shield.py` 收进网关，使其统计与开关都进面板。
    #[serde(default)]
    pub upstream_retry_absorb_enabled: bool,

    /// 吸收层**总预算秒数**（默认 60）。从进入 `handle_*_request` 起算的绝对 deadline，
    /// 与 provider 内部的 12 次换号 / 45s 闸门**串联**记账：剩余预算不足一轮就不再重试。
    #[serde(default = "default_absorb_budget_secs")]
    pub upstream_retry_absorb_budget_secs: u64,

    /// 吸收层**最大额外轮次**（默认 4，0=只打一次即不吸收）。与预算取先到者。
    #[serde(default = "default_absorb_max_attempts")]
    pub upstream_retry_absorb_max_attempts: u32,

    /// 退避下限毫秒（默认 150）。号池冷却常在几十~几百毫秒即恢复，
    /// shield 的 `MIN_DELAY=1.0` 会把 50ms 的恢复睡成 1s，这里放开到亚秒级。
    #[serde(default = "default_absorb_min_delay_ms")]
    pub upstream_retry_absorb_min_delay_ms: u64,

    /// 退避上限秒（默认 15）。上游 `Retry-After` 再大也 clamp 到此值，防单请求长挂。
    #[serde(default = "default_absorb_max_delay_secs")]
    pub upstream_retry_absorb_max_delay_secs: u64,

    /// 是否也吸收 **403 账户级临时风控**（默认 false，见 §2 的论证：窗口内重试会加深封禁）。
    #[serde(default)]
    pub upstream_retry_absorb_suspended: bool,
```

**P2** anchor（唯一）：
old_string:
```rust
fn default_cc_auto_buffer() -> bool {
```
new_string:
```rust
/// 吸收层总预算：默认 **60s**。取值依据见 `handlers.rs` 的 `AbsorbPolicy` 文档注释。
fn default_absorb_budget_secs() -> u64 {
    60
}

/// 吸收层最大额外轮次：默认 **4**。
fn default_absorb_max_attempts() -> u32 {
    4
}

/// 退避下限：默认 **150ms**（号池亚秒恢复不该被睡满 1s）。
fn default_absorb_min_delay_ms() -> u64 {
    150
}

/// 退避上限：默认 **15s**（与 shield 的 clamp 上界一致）。
fn default_absorb_max_delay_secs() -> u64 {
    15
}

fn default_cc_auto_buffer() -> bool {
```

**前端（用户要求"设置里能开关"）**
- `src/admin/types.rs`：`ConfigSnapshotResponse` 加 6 个同名字段（已有 `rename_all="camelCase"`）+ `ConfigSnapshotResponse::default()`（:1433 一带）补 `upstream_retry_absorb_enabled: false, upstream_retry_absorb_budget_secs: 60, upstream_retry_absorb_max_attempts: 4, upstream_retry_absorb_min_delay_ms: 150, upstream_retry_absorb_max_delay_secs: 15, upstream_retry_absorb_suspended: false`；`UpdateConfigRequest`（:770 一带）加 6 个 `Option<...>`。
- `src/admin/service.rs`：仿 `cc_auto_buffer`（:1763）加 6 个 `if let Some(v) = req.x { if v != config.x { … absorb_changed = true } }`，末尾统一 `crate::anthropic::set_absorb_policy(AbsorbPolicy::from_config(&config));`（TIER3 热更，见 P4）。
- `admin-ui/src/types/api.ts`：`ConfigSnapshot` 加 `upstreamRetryAbsorbEnabled: boolean` 等 6 项（必填）；`UpdateConfigRequest` 加 6 项可选。
- `admin-ui/src/components/settings-page.tsx`：`form` 类型（:230 一带）+ `c.x`（:321 一带）+ diff（:1522 一带）+ 一个 `<Switch>` 与 4 个数字输入，放在 `ccAutoBuffer` 开关（:2102）同卡片内。
- i18n 三语键（`settings.absorb.*`）：

| key | zh | en | ja |
|---|---|---|---|
| `.title` | 上游 429 吸收层 | Upstream 429 absorption | 上流 429 吸収 |
| `.enabled` | 启用吸收层 | Enable absorption | 吸収を有効化 |
| `.enabledHint` | 客户端未收到任何字节时，网关就地退避重试可恢复的 429，不把 429 吐给客户端 | Retry recoverable 429s in-gateway while the client has received zero bytes | クライアントへ未送信の間、回復可能な 429 をゲートウェイ内で再試行 |
| `.budgetSecs` / `.maxAttempts` / `.minDelayMs` / `.maxDelaySecs` | 总预算(秒) / 最大轮次 / 退避下限(毫秒) / 退避上限(秒) | Total budget (s) / Max rounds / Min backoff (ms) / Max backoff (s) | 総予算(秒) / 最大回数 / 最小待機(ms) / 最大待機(秒) |
| `.suspended` | 同时吸收 403 临时风控（不建议） | Also absorb 403 temporary suspension (not recommended) | 403 一時制限も吸収（非推奨） |

---

## 2. 吸收什么（复用 `map_provider_error` 的既有分类，零新增字符串匹配）

`map_provider_error`（handlers.rs:690）已把错误分好类。做法是把它拆成「分类」与「渲染」，分类结果挂到 `Response::extensions()`，吸收层只读枚举。

**P3** anchor（唯一）：
old_string:
```rust
/// 将 KiroProvider 错误映射为 HTTP 响应
fn map_provider_error(err: Error) -> Response {
    let err_str = err.to_string();
```
new_string:
```rust
/// 吸收层可重试类别。**判据完全复用 `map_provider_error` 的既有分支谓词**，
/// 不新写任何字符串匹配 —— 新写一套必然与渲染侧漂移（这正是"所有凭据均已禁用落 502"的成因）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AbsorbClass {
    /// 全池冷却快速失败，`retry_after_secs=N` 带号池**真实**恢复秒数。
    PoolCooldown(u64),
    /// 上游账户级速率限流（`USER_REQUEST_RATE_EXCEEDED` 一类）。可重试。
    UpstreamRateLimit,
    /// 403 账户级**临时风控**。默认**不吸收**，见 `AbsorbPolicy::absorb_suspended`。
    Suspended,
}

/// 分类器：谓词与顺序必须与 `map_provider_error_inner` 一致（`should_agree_with_renderer` 钉死）。
fn absorb_class_of(err_str: &str) -> Option<AbsorbClass> {
    if let Some(secs) = err_str
        .split("retry_after_secs=")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|d| d.parse::<u64>().ok())
    {
        return Some(AbsorbClass::PoolCooldown(secs));
    }
    // 模型对本号池**永久**不可用：重试无效，绝不吸收（否则把 404 死循环搬进网关）。
    if err_str.contains("model_unsupported_by_pool=1") {
        return None;
    }
    if is_upstream_rate_limited(err_str) {
        return Some(AbsorbClass::UpstreamRateLimit);
    }
    if is_upstream_temporarily_suspended(err_str) {
        return Some(AbsorbClass::Suspended);
    }
    // 配额耗尽 / 网络 / TLS / 未知兜底：一律不吸收。配额类要等下个计费周期，
    // 网络类由 provider 内部 12 次换号已覆盖，再套一层只是放大。
    None
}

/// 将 KiroProvider 错误映射为 HTTP 响应，并把吸收类别挂进 extensions 供吸收层读取。
fn map_provider_error(err: Error) -> Response {
    let class = absorb_class_of(&err.to_string());
    let mut resp = map_provider_error_inner(err);
    if let Some(c) = class {
        resp.extensions_mut().insert(c);
    }
    resp
}

fn map_provider_error_inner(err: Error) -> Response {
    let err_str = err.to_string();
```

**403 临时风控要不要吸收 —— 判断：默认不吸收，配置可开。**

依据三条：
1. 上游文案自称 `temporarily`，本仓按**账号级**限时态处理（`UPSTREAM_SUSPENDED_RETRY_AFTER_SECS=20`，冷却与 `family_key` 连坐都已生效）。窗口约 10 分钟 ≫ 任何合理的单请求预算（60s），**窗口内重试的成功概率接近 0**，吸收只是把必然失败推迟 60s 再返回，客户端体验更差。
2. 工作树里那条未上线的修复本身就是证据：全池自愈无退避时"每次复活都往刚被 403 的账号再打一轮 → 加深封禁"，41 分钟触发 36 次。吸收层若吸 403，等于在**请求维度**复现同一个错误。
3. provider 内部已换号 12 次。403 是**账号级**、族级连坐已让同族全退，外层再打只会去撞其它族的号 —— 那是扩大受害面而非提高成功率。

留 `upstream_retry_absorb_suspended` 开关是因为族键错配的历史（#12）说明"403 到底是号级还是账号级"在多开分身下可能另有形态；但默认 false，且开启时前端标注"不建议"。

**不吸收真流式 in-band error**：吸收只发生在 `map_provider_error` 之前，即 `call_api_stream` 的 `Err(e)` 分支——此时 `message_start` 尚未生成。`generate_initial_events()` 之后的任何失败都不进吸收层，措辞按「wrapper 一旦 return，即视为已提交」，不依赖 hyper 何时真写 socket。

---

## 3 & 4. 吸收层实现（退避算式 + 预算取值）

**P4** anchor（唯一）：
old_string:
```rust
/// 处理流式请求
async fn handle_stream_request(
```
new_string:
```rust
/// 吸收层策略（进程级 ArcSwap 镜像，admin 热更即时生效）。
///
/// # 预算 60s 的取值依据
/// shield 实测 600s/60 次换来 1.07:1 吸收比，但 **p50 达 73.2s**（300 并发）——
/// 长预算买到的是延迟而非成功率。三条约束定出 60s：
/// ① 与 provider **串联**：provider 一轮最坏 45s（`MAX_REQUEST_RETRY_BUDGET_SECS`），
///    60s 预算 ⇒ 最坏 2 轮，真实放大 ≈ 2×12=24 次上游调用，而 shield 是 60×12=720。
/// ② 号池 429 是**快速 bail**（毫秒级返回 `retry_after_secs=N`），不消耗 45s，
///    故 60s 内实际能跑满 `max_attempts=4` 轮；45s 那条闸门只在真打上游时才吃满。
/// ③ 60s + 上游首轮 ≈ 客户端 idle timeout 的安全区内；再长就要动客户端超时配置。
/// 预算耗尽时默认**保留最后一次的原始响应**（429 + Retry-After），因为本仓默认客户端是
/// Claude Code，它对 429+Retry-After 的退避是正确路径（见 `map_provider_error` 注释）。
/// ⚠️ 2026-08-06 更正：**换 503 已作为可选项实现**（`upstreamRetryAbsorbExhaustedStatus`），
/// 供 Cursor 一类见 429 即掐会话的客户端使用。本行原写「不像 shield 换成 503」已不准确 ——
/// 不是不做，是**默认不做、可开**。见本文件末尾「不该做的」那条的更正说明。
#[derive(Debug, Clone, Copy)]
struct AbsorbPolicy {
    enabled: bool,
    budget: std::time::Duration,
    max_attempts: u32,
    min_delay: std::time::Duration,
    max_delay: std::time::Duration,
    absorb_suspended: bool,
}

impl Default for AbsorbPolicy {
    fn default() -> Self {
        // 与 config.rs 的 default_absorb_* 逐项一致（`absorb_defaults_match_config` 钉死）。
        Self {
            enabled: false,
            budget: std::time::Duration::from_secs(60),
            max_attempts: 4,
            min_delay: std::time::Duration::from_millis(150),
            max_delay: std::time::Duration::from_secs(15),
            absorb_suspended: false,
        }
    }
}

static ABSORB_POLICY: std::sync::OnceLock<arc_swap::ArcSwap<AbsorbPolicy>> =
    std::sync::OnceLock::new();

fn absorb_policy() -> AbsorbPolicy {
    **ABSORB_POLICY
        .get_or_init(|| arc_swap::ArcSwap::from_pointee(AbsorbPolicy::default()))
        .load()
}

/// main 启动接线 / admin 热更调用（TIER3，下一个请求即生效）。
pub fn set_absorb_policy(cfg: &crate::model::config::Config) {
    let p = AbsorbPolicy {
        enabled: cfg.upstream_retry_absorb_enabled,
        budget: std::time::Duration::from_secs(cfg.upstream_retry_absorb_budget_secs),
        max_attempts: cfg.upstream_retry_absorb_max_attempts,
        min_delay: std::time::Duration::from_millis(cfg.upstream_retry_absorb_min_delay_ms),
        max_delay: std::time::Duration::from_secs(cfg.upstream_retry_absorb_max_delay_secs),
        absorb_suspended: cfg.upstream_retry_absorb_suspended,
    };
    ABSORB_POLICY
        .get_or_init(|| arc_swap::ArcSwap::from_pointee(AbsorbPolicy::default()))
        .store(std::sync::Arc::new(p));
}

/// 退避时长。**优先用号池给的真实恢复秒数**，拿不到才走指数兜底。
///
/// 与 shield（`MIN_DELAY=1.0`；有 Retry-After 则 clamp[1,15]，否则 `1.0*1.7^(n-1)` clamp 12）逐条差异：
/// ① shield 的下限硬 1s，号池 50ms 就能恢复时白睡 950ms×每轮 → 这里下限 150ms（可配）。
/// ② shield 只看 HTTP `Retry-After` 头；这里直接吃 `PoolCooldown(secs)` 的**进程内真值**，
///    无需经 HTTP 头往返，且它就是 `cooldown.rs` 算出的剩余秒数。
/// ③ 上游速率限流无真值时用 `min_delay * 2^(n-1)`（指数，非 1.7），
///    因为已有 `max_attempts` 与绝对 deadline 双闸，收敛更快比更平滑重要。
fn absorb_backoff(p: &AbsorbPolicy, class: AbsorbClass, round: u32) -> std::time::Duration {
    let d = match class {
        // 号池真值：0 视为"已恢复"，仍睡 min_delay 避免忙等（#9 的教训：无 sleep 的 continue 是死循环）。
        AbsorbClass::PoolCooldown(secs) => std::time::Duration::from_secs(secs).max(p.min_delay),
        _ => p.min_delay.saturating_mul(1u32 << round.min(6)),
    };
    d.clamp(p.min_delay, p.max_delay)
}

/// 吸收循环：把 `f` 反复调用直到成功或预算/轮次耗尽。
///
/// 不变量：`f` 一旦 return 非 2xx，即视为**客户端 0 字节已提交**（`map_provider_error` 的所有
/// 分支都在 `generate_initial_events()` 之前）。`should_return_map_provider_error_before_stream`
/// 用源码断言把这条钉死。
async fn absorb_retry<F, Fut>(mut f: F) -> Response
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Response>,
{
    let p = absorb_policy();
    let mut resp = f().await;
    if !p.enabled || p.max_attempts == 0 {
        return resp;
    }
    let deadline = std::time::Instant::now() + p.budget;
    let mut round = 0u32;
    while round < p.max_attempts {
        let Some(class) = resp.extensions().get::<AbsorbClass>().copied() else {
            return resp; // 成功，或不可吸收的错误类别
        };
        if matches!(class, AbsorbClass::Suspended) && !p.absorb_suspended {
            crate::common::recovery_metrics::bump_absorb_suspend_skipped();
            return resp;
        }
        let delay = absorb_backoff(&p, class, round);
        // 串联记账：连"睡完 + 至少还能打一轮的余量"都不够就不再重试（避免超出预算才发现）。
        if std::time::Instant::now() + delay >= deadline {
            crate::common::recovery_metrics::bump_absorb_budget_exhausted();
            tracing::warn!(rounds = round, ?class, "吸收层预算耗尽，原样透传上游 429 + Retry-After");
            return resp;
        }
        tokio::time::sleep(delay).await;
        round += 1;
        crate::common::recovery_metrics::bump_absorb_attempt();
        resp = f().await;
    }
    if resp.extensions().get::<AbsorbClass>().is_some() {
        crate::common::recovery_metrics::bump_absorb_budget_exhausted();
        tracing::warn!(rounds = round, "吸收层轮次耗尽");
    } else {
        crate::common::recovery_metrics::bump_absorb_recovered();
        match round {
            1 => crate::common::recovery_metrics::bump_absorb_rounds_1(),
            2 => crate::common::recovery_metrics::bump_absorb_rounds_2(),
            _ => crate::common::recovery_metrics::bump_absorb_rounds_3plus(),
        }
        tracing::info!(rounds = round, "吸收层重试成功，客户端未见 429");
    }
    resp
}

/// 处理流式请求（吸收层包装，签名与调用点不变）
#[allow(clippy::too_many_arguments)]
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    cache_breakdown: Option<CacheUsageBreakdown>,
    client: ClientInfo,
) -> Response {
    absorb_retry(|| {
        handle_stream_request_once(
            provider.clone(),
            request_body,
            model,
            input_tokens,
            thinking_enabled,
            tool_name_map.clone(),
            known_tool_names.clone(),
            cache_breakdown,
            client.clone(),
        )
    })
    .await
}

/// 处理流式请求（单次尝试，原实现）
async fn handle_stream_request_once(
```

`handle_stream_request_buffered` 与 `handle_non_stream_request` 同型（各自 anchor 是 `async fn handle_stream_request_buffered(` / `async fn handle_non_stream_request(`，均唯一；后者无 `known_tool_names` 参数）。`ClientInfo` 已 `#[derive(Clone)]`，`CacheUsageBreakdown` 需 `Copy`（若非则改 `.clone()`）。

`src/main.rs` 启动接线：在 `set_cc_auto_buffer` 附近加 `crate::anthropic::set_absorb_policy(&config);`；`src/anthropic/mod.rs` 加 `pub use handlers::set_absorb_policy;`。

---

## 5. 可观测（shield 的统计面板看不到 —— 内置版不能重复）

`src/common/recovery_metrics.rs` 的 `counters!` 宏末尾追加：
```rust
    // 上游 429 吸收层：重试轮次总数 / 吸收成功(客户端未见 429) / 预算耗尽 / 403 风控跳过不吸收。
    absorb_attempts: bump_absorb_attempt,
    absorb_recovered: bump_absorb_recovered,
    absorb_budget_exhausted: bump_absorb_budget_exhausted,
    absorb_suspend_skipped: bump_absorb_suspend_skipped,
    // 每请求重试数分布（1 / 2 / ≥3 轮才成功），判断预算是否够用。
    absorb_rounds_1: bump_absorb_rounds_1,
    absorb_rounds_2: bump_absorb_rounds_2,
    absorb_rounds_3plus: bump_absorb_rounds_3plus,
```
宏自动生成 camelCase 字段，`/api/admin/recovery-metrics` 自动带出（`handlers.rs:689` 无需改）。

前端：`admin-ui/src/api/ops.ts` 的 `RecoveryMetrics` 加 7 个 `?: number`（可选，兼容旧后端）；`ops-page.tsx` 的 `METRIC_ITEMS`（:117）追加 7 项，`absorbRecovered` / `absorbRounds*` 不带 `warn`，`absorbAttempts`/`absorbBudgetExhausted`/`absorbSuspendSkipped` 带 `warn: true`；三语加 `opspage.metric.absorb*` 键（zh：吸收重试轮次/吸收成功/预算耗尽/风控跳过/1 轮成功/2 轮成功/≥3 轮成功）。

吸收比可由面板直接算：`absorbRecovered / (absorbRecovered + absorbBudgetExhausted)`，对应 shield 的 1.07:1。

---

## 6. shield 退役路径（别人加的、正在保护生产，不擅自删）

五步，每步可独立回滚，任一步不对就停在上一步：
1. **上线内置版，`enabled=false`**。行为零变化，shield 仍在 8993 全量承载。取此刻 `/_shield/stats` 与面板 429 作基线。
2. **面板开 `enabled=true`**（热更，不重启）。此时**两层串联**：网关先吸收，剩下的漏给 shield。判据：`absorbRecovered` 开始增长且 shield 的重试次数下降。若网关 p50 明显上升 → 拨回 false。
3. **shield 降级为观察者**：把它的 `MAX_ATTEMPTS` 从 60 调到 2（改 `/opt/kirostudio/bin/kiro_shield.py`，先改 `ws-vps` 仓库再 scp）。观察 24h：若客户端成功率不掉，说明网关已吃下 shield 原本吃的量。
4. **Caddy 4 条路由逐条从 :8993 切到 :8990**，每切一条观察 1h（先切流量最小的那条）。`gateway-status` 巡检 + `caddy reload` 可秒级回切。
5. **shield 进程停但不删**（`systemctl disable --now`，二进制与 unit 文件保留 ≥2 周）。确认无回退需求后再由 VPS 仓库的人决定删除。

---

## 7. 实施顺序（4 个 commit，每个的"回退即 FAIL"判据）

| # | commit | 测试 | 回退即 FAIL 的构造 |
|---|---|---|---|
| 1 | `feat(config): 新增上游 429 吸收层六项配置` | `absorb_defaults_match_config`：断言 `AbsorbPolicy::default()` 六项 == `default_absorb_*()` | 改任一 `default_absorb_*` 返回值 → FAIL（钉死"config 与进程镜像默认不一致"这一历史缺陷，见 `default_cc_auto_buffer` 注释） |
| 2 | `refactor(handlers): 抽出 absorb_class_of，分类与渲染分离` | `should_agree_with_renderer`：对 6 组真实错误串断言 `absorb_class_of` 与 `map_provider_error` 的 status 一致（`Some(_)` ⟺ 429、`None` ⟺ 非 429 或配额类无 Retry-After）；`suspended_is_classified_not_absorbed` | 删掉 `model_unsupported_by_pool=1 → None` 那行 → 404 被判为可吸收 → FAIL |
| 3 | `feat(handlers): 内置 429 吸收层（默认关）` | ① `disabled_policy_calls_f_exactly_once`（用计数闭包，`enabled=false` 时调用次数必须 == 1）；② `absorbs_pool_cooldown_then_succeeds`（前 2 次返 429+ext、第 3 次返 200，断言最终 200 且 `round==2`）；③ `never_exceeds_deadline`（budget=1s、伪造 `PoolCooldown(30)` → 必须**不 sleep 30s** 直接 exhausted 返回）；④ `suspended_not_absorbed_by_default`；⑤ 源码守卫 `map_provider_error_only_before_stream`：`include_str!("handlers.rs")` 断言三个 `*_once` 函数体内 `map_provider_error` 均出现在 `generate_initial_events`/`BufferedStreamContext::new` **之前**，且全文 `map_provider_error(` 的调用点恰为 3 处 | 去掉 ③ 的 deadline 预检 → 单请求挂 30s → FAIL（这条正是 shield 的 `MIN_DELAY` + 600s 预算换来 p50 73.2s 的病根）；把 `absorb_retry` 套到 `call_api_stream` 外层 → 守卫测试 FAIL |
| 4 | `feat(admin): 吸收层开关 + 计数器上面板` | `snapshot` camelCase 断言加 `absorbAttempts`；service 热更断言 `set_absorb_policy` 被调用（沿用 `cc_auto_buffer_changed` 同款测试） | 前端字段名写错 → `types/api.ts` 与后端 camelCase 不符，settings diff 不生效（无自动测试，靠 code review + 手工验一次） |

~~**不该做的**：给"预算耗尽"换 503（shield 那样）—— 本仓客户端是 Claude Code，它对 429+Retry-After 的退避是正确路径，构造不出"移除即失败"的测试，只会掩盖 429 让面板看不见真实限流。~~

⇒ 🔴 **上面这条已过期并作废（2026-08-06）。它是 `TASK-CANVAS-IPPOOL-SHIELD.md` C7 引用的源头，
两处都已更正。** 用户 2026-08-05 明确要求的正是这个能力（原话「把这个 Cursor 对 429 会立刻停止
会话 改掉 改为 请求对 429 会进行重试 我们作为缓冲」）：**Cursor 见 429 会掐会话，见 503 不会。**

**已实现**：`handlers.rs` 的 `ABSORB_BUDGET_EXHAUSTED_MARKER` + `map_provider_error` 第一条分支，
`provider.rs` 在 `absorb_gave_up_after_rounds && absorb.exhausted_as_503` 时置位；
开关 `upstreamRetryAbsorbExhaustedStatus`，**默认仍 429**（前提「本仓客户端是 Claude Code」
只对默认值成立，Cursor 用户置 503）。标记只在**真重试过**时置位 —— 一次没重试就改状态码是说谎，
这个二分只有 provider 做得出来，也正是「构造不出测试」那句话的反面：
它可测（喂带/不带标记的错误串断言 503/429），已有测试覆盖。
原顾虑「掩盖 429 让面板看不见真实限流」**仍成立，作为已知取舍**，由吸收层计数器
（`absorb_budget_exhausted` 等）+ 日志 `absorb_stop=` 归因弥补。

**PR 说明必须写**：吸收层只压**客户端可见的 429**，不减少打向上游的请求量；真正的容量口径修正是账号级 RPM 那条独立 PR 的步骤 C。否则会误判"开了吸收层但上游 429 没少 = 没效果"。