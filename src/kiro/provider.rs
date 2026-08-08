//! Kiro API Provider
//!
//! 核心组件，负责与 Kiro API 通信
//! 支持流式和非流式请求
//! 支持多凭据故障转移和重试
//! 支持按凭据级 endpoint 切换不同 Kiro API 端点

use reqwest::Client;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::http_client::{ProxyConfig, build_streaming_client};
use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::TlsBackend;
use parking_lot::Mutex;

/// 每个凭据的最大重试次数
const MAX_RETRIES_PER_CREDENTIAL: usize = 3;

/// 小号池阈值：号池 <= 此值时，每号重试次数降为 1（见 [`compute_max_retries`]）。
/// 小池下重试只会反复砸同几个号，被限流时多打几次纯属加重冷却，不如各摸一次即透传。
const SMALL_POOL_THRESHOLD: usize = 3;

/// 总重试次数绝对硬上限（避免无限重试）
///
/// 注意：这只是一个安全上限，不再作为固定的重试预算。真正的预算由
/// [`compute_max_retries`] 依据凭据总数 / 可用数动态计算，保证每个可用
/// 凭据至少能被摸到一次（历史上写死 9 会让凭据 >3 时后面的号一次没试就报错）。
///
/// ⚠️ 由 64 降到 12：64 从未是「合理预算」而只是个防死循环的兜底，但配合
/// `total * 3` 的算法（且 total 曾把 disabled / custom_api 都算进去）实际生效成了
/// 生产日志里的 `尝试 8/36`——一条客户端请求连打十几个号、同一出口 IP，正是风控要抓的
/// 突发特征。叠加 sub2api 侧的 2 次重试 × 10 次账号切换，单请求最坏放大到约 70~108 次
/// 上游调用。12 仍足以让每个号被摸到（可选号 > 12 时下面会以 available 为准不受此限）。
///
/// ⭐ 这个上限是「**每客户端请求**」，开启吸收层后也不变 —— 由 [`round_retry_quota`] 保证。
///
/// 曾经不是：`compute_max_retries` 在 `'absorb: loop` **之外**只算一次，而
/// `for attempt in 0..max_retries` 每个吸收轮都重跑一遍 ⇒ 每轮各拿一份完整 12 ⇒
/// `upstreamRetryAbsorbMaxRounds=3` 时一条客户端请求最坏 (1+3)×12 = **48 次**上游调用、
/// 同一出口 IP，正是上面那段把 64 砍到 12 想压住的突发特征被从另一头放回来。
///
/// 现在每轮的实际配额是 `min(基础配额, 本上限 − 跨轮已用)`，所以无论 `max_rounds`
/// 填多大，单条客户端请求打向上游的总次数恒 ≤ 本值。守卫见
/// `total_upstream_attempts_are_capped_per_request_not_per_round`。
const ABSOLUTE_MAX_TOTAL_RETRIES: usize = 12;

/// 上游压力率（429+5xx）滑动窗口的时长（秒）。
///
/// 窗口内每响应喂一次压力布尔，`rate()` 返回近期压力占比，供
/// [`apply_retry_pressure`] 动态降档。60s 对齐 throttle 的观察窗口径，既不反应过
/// 快的瞬时抖动（去抖交给 AIMD 的 3s 窗口），也不至于滞后到跟不上风控节奏。
const PRESSURE_WINDOW_SECS: u64 = 60;

/// 单个入站请求的重试墙钟预算（秒）。
///
/// ⚠️ 关键防雪崩闸门：小号池下，一个卡住的请求会在每次重试时抢到刚出冷却的号、
/// 又打 429、又把它冷却，如此在 acquire_context 的等待循环（最长 180s）× 多次
/// 重试之间反复横跳，一个请求就能把整池长时间压死（表现为「没有新入站却一直 429
/// / 繁忙」）。这里给单请求一个总时长上限：超时就停止重试、把最后的错误（通常是
/// 429）透传给客户端，让客户端自己退避，而不是继续拖垮整池。取值需覆盖一次正常
/// 大请求的排队+响应，又不至于长到能扫冷全池。
const MAX_REQUEST_RETRY_BUDGET_SECS: u64 = 45;

/// 端点桶（同一 host 的限流桶）被 429 封禁的时长。对齐 kiro2cc `BUCKET_THROTTLE_DURATION`。
///
/// 桶 = (credential_id, endpoint_name)。同凭据另一端点（另一 host = 上游另一限流桶）不受影响，
/// 可继续用。到期自动解除（惰性清理在 `select_endpoint` 访问时顺带做；`has_unthrottled_endpoint`
/// 只读不清理，键数 = 号数 × 端点数，无无界增长风险）。
const ENDPOINT_BUCKET_THROTTLE: Duration = Duration::from_secs(30);

/// 吸收层一轮最坏「能跑出结果」的时长下限。
///
/// 取值等于 token_manager 的 `MAX_TRANSIENT_WAIT_SECS`（20s）而非另造一个数字：全池只是临时
/// 冷却时 `acquire_context` 最多在网关内等 20s 才 bail，因此**剩余预算不足 20s 的一轮，结构上
/// 只可能在 transient wait 里烧完再返回同一个 429** —— 白打一轮上游、客户端白等。
///
/// 这是设计评审 BLOCKER 9 的修法：判据必须是「剩余 ≥ 退避 + 一轮最坏耗时」，而不是
/// 「剩余 ≥ 退避下限」。后者会让第 2 轮必然在半路被 deadline 砍断。
const ABSORB_MIN_USEFUL_ROUND_SECS: u64 = 20;

/// 退避的**绝对下限**。`maxDelaySecs=0` 经 API 可写（配置层对它无 clamp），而 0 退避会
/// 让吸收循环退化成无 sleep 的 `continue` —— 忙等死循环，打满一核且请求永不返回。
/// 50ms 取自号池冷却的实测最快恢复量级（远小于外置 shield 硬编码的 1s）。
const ABSORB_MIN_BACKOFF: Duration = Duration::from_millis(50);

/// 内置「上游 429 吸收层」的运行时策略快照（每次调用从 config 的 ArcSwap 取一份）。
///
/// 不做 TIER3 进程级 static：吸收层在 provider 内，`token_manager.config()` 本身就是 ArcSwap，
/// admin 存盘后 `reload_config` 原子换入即生效 —— 少一层镜像就少 6 个可写错点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AbsorbPolicy {
    enabled: bool,
    budget: Duration,
    max_rounds: u32,
    min_delay: Duration,
    max_delay: Duration,
    /// 是否吸收 403 账户级临时风控（= 外挂所称的「换号空窗」）。**必须存进快照**而不是在
    /// 循环里重新 `config()`：一次调用内只取一份策略，否则 admin 在两轮之间热更会让同一条
    /// 请求前后按不同策略走（前半轮用旧 max_rounds、后半轮用新 suspended 判据），
    /// 行为不可复现也不可测试。
    absorb_suspended: bool,
    /// 是否吸收上游 5xx。默认 false（见 `upstream_retry_absorb_server_error`）。
    absorb_server_error: bool,
    /// 是否吸收带瞬态标记的 400（模型容量）。默认 false。
    absorb_capacity_400: bool,
    /// 换号空窗的**独立预算**。`ZERO` = 未启用 ⇒ 该类沿用总预算与 min_delay 指数曲线
    /// （逐字节等于本字段引入前的行为）。非零时该类换成 20/40/60s 长阶梯 + 独立 deadline。
    swap_budget: Duration,
    /// 预算耗尽时是否给错误串打 `absorb_budget_exhausted=1`（让 handlers 渲染成 503）。
    /// 默认 false（状态码保持透传 429）。
    exhausted_as_503: bool,
}

/// 换号空窗的退避阶梯（秒），逐字取自外挂 `kiro_shield.py` 的 `SWAP_BACKOFF`。
///
/// 为什么是这三档而不是继续用指数：外挂注释里那句「**绝不能用限速那套 1 秒退避** ——
/// 那是拿一个已被封的账号去猛打上游，只会加重风控」是本阶梯存在的全部理由。空窗实测约
/// 10 分钟，20s 起步、封顶 60s 意味着一条请求最多问上游十几次，而 1s 起的指数在同样时长内
/// 会问上百次。超出表长的轮次取最后一档（60s）。
const SWAP_WINDOW_BACKOFF_SECS: [u64; 3] = [20, 40, 60];

impl AbsorbPolicy {
    fn from_config(cfg: &crate::model::config::Config) -> Self {
        // 403 临时风控的额外轮次**硬钉为 1**（不是沿用 max_rounds）：`SELF_HEAL_BASE_BACKOFF`
        // 存在的意义就是停止向刚 403 的账号试探，而 403 是**账号级**的 —— 换号只是把同一个
        // 被惩罚账号走多遍，扩大受害面而非提高成功率。UI 文案也标注了「与自愈退避冲突」。
        let swap_budget = Duration::from_secs(cfg.upstream_retry_absorb_swap_budget_secs);
        // ⭐ 「钉 1」的**前提**是短退避：15s 内重打同一个刚被风控的账号会抵消
        // `SELF_HEAL_BASE_BACKOFF=60s`（那条退避存在的意义就是停止试探）。
        // 设了 swap 预算后该类走 20/40/60s 长阶梯 —— 最短一档就是 20s，前提不再成立，
        // 于是解除钉 1，交回 `max_rounds` + 独立 deadline + `ABSOLUTE_MAX_TOTAL_RETRIES` 三道闸。
        //
        // 不解除的话这个旋钮基本没用：钉 1 时它只能把**一次**重试推迟到 20s 后，
        // 而空窗实测 10 分钟 ⇒ 那一次几乎必然还在窗口内 ⇒ 白等 20s 拿同一个 403。
        let max_rounds = if cfg.upstream_retry_absorb_suspended && swap_budget.is_zero() {
            cfg.upstream_retry_absorb_max_rounds.min(1)
        } else {
            cfg.upstream_retry_absorb_max_rounds
        };
        // ⭐ 两道归一化，顺序不能换。两者都是**下限**方向，因为 `backoff()` 末尾是
        // `d.clamp(min_delay, max_delay)` —— 决定「会不会返 0」的是 `min_delay`（下界），
        // 不是 `max_delay`。
        //
        // ① `min_delay` 抬到 `ABSORB_MIN_BACKOFF`。`minDelayMs=0` 经 Admin API 可写
        //    （`service.rs` 对这两个字段无任何 clamp），而下界为 0 时 `clamp` 不会抬起
        //    任何东西 → `PoolCooldown(0)` 直接返 `Duration::ZERO` → 吸收循环变成无 sleep
        //    的 `continue` = 忙等死循环（`acquire_context` 那次 CPU 打满一核、请求永不
        //    返回正是这个形态）。⚠️ 这条缺陷**在本批之前就存在**（deploy tip 的
        //    `from_config` 同样不给 `min_delay` 下限），不是新引入的。
        //
        // ② `max_delay` 再抬到不低于 `min_delay`。`d.clamp(min, max)` 在 `min > max` 时是
        //    **panic**（std 契约），而两个字段来自面板上两个独立数字框（毫秒 vs 秒），
        //    `minDelayMs=60000` 配 `maxDelaySecs=1` 一次手滑即可配出 —— 那会让此后每个
        //    429 都在请求热路径上 panic。
        //
        //    方向取「抬 max 到 min」而不是「压 min 到 max」：矛盾配置下宁可**退避更久**。
        //    退避久的后果是 `should_start_another_round`（要求剩余 > 退避 + 20s）判不通过
        //    ⇒ 吸收层不干活、回落旧行为；而退避短的后果是对一个还在冷却的号池连打，
        //    正是吸收层要避免的事。前者安全，后者有害。
        //
        // 都用归一化而非拒绝保存：退避窗退化成一个点仍是可用行为，而拒绝保存会把一个
        // 能自愈的配置错误变成运维事故。不变式由构造保证 ⇒ 可用纯函数单测钉死。
        let min_delay =
            Duration::from_millis(cfg.upstream_retry_absorb_min_delay_ms).max(ABSORB_MIN_BACKOFF);
        let max_delay =
            Duration::from_secs(cfg.upstream_retry_absorb_max_delay_secs).max(min_delay);
        Self {
            enabled: cfg.upstream_retry_absorb_enabled,
            // ⭐ 总预算**下限是 45s**（`MAX_REQUEST_RETRY_BUDGET_SECS`），不是面板允许的 1s。
            //
            // 因为 `round_budget()` 用 `min(45s, 剩余预算)` 夹每一轮的 failover 墙钟，
            // 所以这个「吸收层的」旋钮会反向支配**既有的** failover 重试预算：
            // 填 5s ⇒ 第 0 轮（也就是关掉吸收层时唯一的那一轮）的换号墙钟从 45s 变成 5s
            // ⇒ 正常换号重试被截断。运维填小值的动机恰恰是「想压住客户端延迟」，
            // 而实际后果是把与吸收层无关的重试也砍了 —— 面板上完全看不出这层耦合。
            //
            // 抬到 45s 后语义变干净：这个旋钮只能决定**多给几轮**，
            // 永远不会让单轮比关掉吸收层时更短。
            budget: Duration::from_secs(cfg.upstream_retry_absorb_budget_secs)
                .max(Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS)),
            max_rounds,
            min_delay,
            max_delay,
            absorb_suspended: cfg.upstream_retry_absorb_suspended,
            absorb_server_error: cfg.upstream_retry_absorb_server_error,
            absorb_capacity_400: cfg.upstream_retry_absorb_capacity_400,
            swap_budget,
            // 只认精确的 503。其它值（含裸 `#[serde(default)]` 会给的 0）一律按 429 处理：
            // 这个开关的语义是「要不要为 Cursor 让步」，不是「随便填个状态码」——
            // 让 provider 打一个 handlers 认不出的标记只会造成静默的行为分叉。
            exhausted_as_503: cfg.upstream_retry_absorb_exhausted_status == 503,
        }
    }

    /// 该类别当前是否被允许吸收（各类别的独立开关）。
    ///
    /// 抽成纯函数而不是在循环里散写 `if`：三个新类别各有一个开关，散写必然漏一处，
    /// 而漏掉的那一处的表现是「默认关的类别其实在吸收」—— 正是硬约束里最不能出的错。
    /// 纯函数可用单测把「默认配置下三个新类别一律不吸收」钉死。
    fn class_allowed(&self, class: crate::anthropic::AbsorbClass) -> bool {
        use crate::anthropic::AbsorbClass;
        match class {
            // 这两类是吸收层原本就在做的事，跟着总开关走。
            AbsorbClass::PoolCooldown(_) | AbsorbClass::UpstreamRateLimit => true,
            AbsorbClass::SwapWindow => self.absorb_suspended,
            AbsorbClass::TransientServerError => self.absorb_server_error,
            AbsorbClass::TransientCapacity400 => self.absorb_capacity_400,
        }
    }

    /// 该类别的绝对 deadline。换号空窗在设了独立预算时用它自己那份，其余一律用总预算。
    ///
    /// 为什么不能共用一个预算（外挂实测）：换号空窗约 **10 分钟**，而总预算线上是 20s。
    /// 抬总预算会让**所有**类别都能占着客户端连接十分钟，而换号空窗恰恰是唯一等得起的一类
    /// （客户端在补号完成后自动恢复，而不是当场断会话）。
    fn class_deadline(
        &self,
        call_started: std::time::Instant,
        class: crate::anthropic::AbsorbClass,
    ) -> std::time::Instant {
        if matches!(class, crate::anthropic::AbsorbClass::SwapWindow) && !self.swap_budget.is_zero()
        {
            // 与总预算同源地从 `call_started` 起算（含准入排队），理由见调用点：
            // 改成「从此刻起算」会让客户端可见延迟变成排队 + 吸收之和。
            call_started + self.swap_budget
        } else {
            call_started + self.budget
        }
    }

    /// 该类别愿意睡的上限。换号空窗启用长阶梯时可以超过全局 `max_delay`。
    ///
    /// 为什么必须放宽：`max_delay` 默认 15s < 阶梯最短一档 20s ⇒ 不放宽的话长阶梯会被
    /// 全局 clamp 削回 15s，这个旋钮等于没接上（而 `backoff_is_truncated` 只对
    /// `PoolCooldown` 成立，不会拦住这种「睡不够」）。显式设了 swap 预算本身就是
    /// 「这一类可以睡更久」的表态。
    fn class_max_delay(&self, class: crate::anthropic::AbsorbClass) -> Duration {
        if matches!(class, crate::anthropic::AbsorbClass::SwapWindow) && !self.swap_budget.is_zero()
        {
            let ladder_top =
                Duration::from_secs(SWAP_WINDOW_BACKOFF_SECS[SWAP_WINDOW_BACKOFF_SECS.len() - 1]);
            self.max_delay.max(ladder_top)
        } else {
            self.max_delay
        }
    }

    /// 本次调用允许的额外吸收轮次。关闭时恒为 0 —— 把「关 ⇒ 零额外轮次」做成可断言的纯函数，
    /// 而不是散落在调用点的 `if !enabled` 判断（后者无法用单测钉死）。
    fn effective_max_rounds(&self) -> u32 {
        if self.enabled { self.max_rounds } else { 0 }
    }

    /// 本轮 failover 循环的墙钟预算：`min(45s, 剩余吸收预算)`。
    ///
    /// 这就是「吸收轮次不会超总预算」的**机制**（而非靠调用点自觉）：一轮的墙钟上限本身
    /// 被剩余预算夹住，所以吸收轮次与 failover 轮次不是各算一套预算，而是后者被前者显式配额。
    /// 关闭时恒返完整 45s ⇒ 与旧行为逐字节等价。
    fn round_budget(
        &self,
        deadline: std::time::Instant,
        round_started: std::time::Instant,
    ) -> Duration {
        let full = Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS);
        if self.enabled {
            full.min(deadline.saturating_duration_since(round_started))
        } else {
            full
        }
    }

    /// 本轮**真正需要**等多久才有意义 —— 未经 `max_delay` 截断的原始值。
    ///
    /// 与 [`Self::backoff`] 分开是承重的（未修问题 ③）：`PoolCooldown(secs)` 是 cooldown.rs
    /// 的**进程内真值**，而 `max_delay`（默认 15s）是一个与它毫无关系的旋钮。号池给出
    /// 60s（`SELF_HEAL_BASE_BACKOFF = from_secs(60)`，token_manager.rs:890 一带）时，
    /// 只 clamp 不判断的写法会
    /// 睡满 15s 就醒来再打一轮 —— 池子还在冷却 45s，这一轮**结构上必然**拿回同一个 429，
    /// 等于白打一轮上游 + 客户端白等 15s。判「够不够」必须用真值，睡多久才用截断值。
    fn required_wait(&self, class: crate::anthropic::AbsorbClass, round: u32) -> Duration {
        use crate::anthropic::AbsorbClass;
        match class {
            // 号池真值。secs=0 由 backoff() 的 clamp 抬到 min_delay:无 sleep 的 continue 就是
            // 忙等死循环(acquire_context 那次 CPU 打满一核、请求永不返回正是这个形态)。
            AbsorbClass::PoolCooldown(secs) => Duration::from_secs(secs),
            // 无真值可用:指数(非 shield 的 1.7 倍)。已有 max_rounds 与绝对 deadline 双闸,
            // 收敛更快比更平滑重要。
            AbsorbClass::UpstreamRateLimit => self.min_delay.saturating_mul(1u32 << round.min(6)),
            // 换号空窗:设了独立预算 ⇒ 长阶梯 20/40/60s;未设 ⇒ 与限速同曲线(旧行为)。
            //
            // 长阶梯的理由是外挂那句实测结论:「**绝不能用限速那套 1 秒退避** —— 那是拿一个
            // 已被封的账号去猛打上游,只会加重风控」。空窗约 10 分钟,20s 起步意味着一条请求
            // 最多问上游十几次;1s 起的指数在同样时长内会问上百次,那不是重试而是施压。
            AbsorbClass::SwapWindow => {
                if self.swap_budget.is_zero() {
                    self.min_delay.saturating_mul(1u32 << round.min(6))
                } else {
                    let idx = (round as usize).min(SWAP_WINDOW_BACKOFF_SECS.len() - 1);
                    Duration::from_secs(SWAP_WINDOW_BACKOFF_SECS[idx])
                }
            }
            // 5xx:1s 起(逐字取自 shield 的 `MIN_DELAY=1.0`)×2 指数。
            // 为什么比限速类起步更长而封顶更早:5xx 多为上游/网关瞬时抖动,一两秒后重打大概率
            // 就过;但若是整片故障,短退避会在故障期乘倍放大请求量。1s 是「抖动等得起、
            // 故障不放大」的折中,且与本仓既有的 `retry_delay_model_unavailable` 同基数。
            AbsorbClass::TransientServerError => {
                const BASE: Duration = Duration::from_secs(1);
                BASE.saturating_mul(1u32 << round.min(4))
            }
            // 容量类:2s 起 ×2。比 5xx 更长是因为「模型没容量」是**全局**状态
            // (所有凭据对同一模型等价受影响,见 endpoint 侧那条判据的说明),换号不解决问题,
            // 只能等上游腾出容量。与 provider 内部那几次慢速重试(1s base)串联后总时长才够到
            // 容量恢复的量级 —— 内部那几次加起来只有秒级,而容量恢复常在分钟级。
            AbsorbClass::TransientCapacity400 => {
                const BASE: Duration = Duration::from_secs(2);
                BASE.saturating_mul(1u32 << round.min(4))
            }
        }
    }

    /// 本轮实际睡多久：真实需求经 `[min_delay, max_delay]` 夹取。
    ///
    /// 与外置 shield 的逐条差异：① shield 的 `MIN_DELAY` 硬 1s，号池 50ms 就能恢复时白睡
    /// 950ms×每轮（这是它 p50 73.2s 的病根之一），这里下限可配到亚秒级；② shield 只看 HTTP
    /// `Retry-After` 头，这里直接吃 `PoolCooldown(secs)` 的**进程内真值**（就是 cooldown.rs
    /// 算出的剩余秒数，无需 HTTP 头往返）。
    fn backoff(&self, class: crate::anthropic::AbsorbClass, round: u32) -> Duration {
        // 上界按类别取（见 `class_max_delay`）：换号空窗的长阶梯不能被默认 15s 的全局上限削回，
        // 否则那个旋钮等于没接上。其余类别的上界与本改动之前逐字节相同。
        //
        // ⚠️ `clamp` 在 `min > max` 时 panic，而 `class_max_delay` 只会**放大**上界
        // （`self.max_delay.max(ladder_top)`），`from_config` 已保证 `max_delay >= min_delay`
        // ⇒ 不变式仍然成立。
        self.required_wait(class, round)
            .clamp(self.min_delay, self.class_max_delay(class))
    }

    /// 号池给出的**真实恢复时刻**是否超过我们愿意睡的上限 ⇒ 睡醒了池子还没好 ⇒ 这一轮白打。
    ///
    /// **只对 `PoolCooldown` 成立**，这个限定是承重的：只有它携带真值（cooldown.rs 算出的
    /// 剩余秒数）。`UpstreamRateLimit`/`Suspended` 走的是我们自己编的指数兜底 —— 它撞上
    /// `max_delay` 只说明「我们不想睡更久」，**不代表上游没好**（`max_delay` 本来就是为了
    /// 夹住它而存在的）。若把它们也算进来，指数涨过上限后吸收层会对**最主要**的那类
    /// （上游裸 429）提前停止工作，白丢一层保护。
    ///
    /// 这条比 `should_start_another_round` 更早生效：后者只管**预算**够不够睡，管不了
    /// 「睡够了但上游没好」——两者是独立的失败模式，都必须拦。
    fn backoff_is_truncated(&self, class: crate::anthropic::AbsorbClass, round: u32) -> bool {
        matches!(class, crate::anthropic::AbsorbClass::PoolCooldown(_))
            && self.required_wait(class, round) > self.max_delay
    }
}

/// 是否还够再跑一轮吸收：**剩余预算 > 退避 + 一轮最坏耗时**。
///
/// 判据刻意不是「剩余 ≥ 退避下限」（BLOCKER 9）：那样第 2 轮会在半路被 deadline 砍断，
/// 等于白打一轮上游还让客户端多等。纯函数便于单测钉死，无需真实上游。
fn should_start_another_round(
    deadline: std::time::Instant,
    now: std::time::Instant,
    delay: Duration,
) -> bool {
    deadline.saturating_duration_since(now)
        > delay + Duration::from_secs(ABSORB_MIN_USEFUL_ROUND_SECS)
}

/// 本吸收轮还能打几次上游：**跨轮共享**同一个 `ABSOLUTE_MAX_TOTAL_RETRIES` 总额度。
///
/// 未修问题 ②：`compute_max_retries` 在 `'absorb: loop` **之外**只算一次，而
/// `for attempt in 0..max_retries` 每轮重跑 ⇒ 每轮各拿一份完整 12 ⇒ `max_rounds=3` 时
/// 一条客户端请求最坏 (1+3)×12 = **48 次**上游调用、同一出口 IP —— 正是当初把
/// `ABSOLUTE_MAX_TOTAL_RETRIES` 从 64 砍到 12 要压住的突发特征，被吸收层从另一头放回来。
///
/// 修法不是调小 `max_rounds`（那只是把数字挪一挪，语义仍是「每轮各拿一份」），而是让
/// 上限回到它文档承诺的「**每请求**」语义：本轮配额 = `min(基础配额, 总额度 − 已用)`。
/// 于是无论 `max_rounds` 填多少，一条客户端请求打向上游的次数恒 ≤ `ABSOLUTE_MAX_TOTAL_RETRIES`。
///
/// `attempts_before` 传**已完成的尝试次数**（= 循环外的 `attempts_base`）。返回 0 表示
/// 额度已用尽，调用点必须 `break 'absorb` 而不是空跑一轮（空跑会白睡一次退避）。
fn round_retry_quota(base_quota: usize, attempts_before: u32) -> usize {
    let remaining = ABSOLUTE_MAX_TOTAL_RETRIES.saturating_sub(attempts_before as usize);
    base_quota.min(remaining)
}

/// 对话路径 403 → **换区重试**的目标 region（L1）。`None` = 不该换区。
///
/// # 为什么对话路径需要这一层
///
/// `ksk_` API Key 是**按 region 授权**的：打错区时上游恒返 403
/// `bearer token included in the request is invalid`。而这个信号在对话路径上
/// 原先被当「凭据问题」→ 冷却 + 换号，**换号解决不了**（同一个号换个区就行）。
/// 导入时的探测可能探错（`region_probe` 那条 400 判 `Usable` 的判据已被实测证否），
/// 于是一个实际授权在 us-east-1 的号会被写死 `eu-central-1` → 该号**恒 403、永久废掉**。
///
/// # 判据为什么必须窄
///
/// `has_ever_succeeded` 这个二分是承重的，它把同一句上游文案劈成语义相反的两类：
/// - **已成功过** ⇒ 区是对的（它在这个区真拿到过 200），403 只能是瞬态抖动
///   （实测 4 个号累计 3393 次成功、共吃 42 次这种 403）→ 交给既有
///   `bearer_invalid_but_proven` 分支（冷却 + 换号、不计失败），本函数返 `None`。
/// - **从未成功过** ⇒ 才**可能**是 region 错配（实测 3 个从未成功的号共吃 17 次）。
///
/// 两者若混在一起：给已证明健康的号换区 = 把一个本来对的配置改坏，而那个号下一次
/// 抖动过去就好了。所以宁可漏修（号从未成功过但其实是别的原因），不可误改。
///
/// # 候选只有两个（实测依据）
///
/// `management.*` 与 `runtime.*` 只在 `us-east-1` / `eu-central-1` 解析 DNS，
/// 即 [`crate::kiro::region_probe::PROBE_ORDER`] 的两项。所以「换区」= 换到**另一个**
/// 那个；当前区不在表内（如 profileArn 把区钉在 `us-west-2`）则换到表首项。
///
/// # 只对 `api_key` 号
///
/// OAuth 号的权威 region 是 `profileArn` 第 4 段（`effective_upstream_region` 第一优先），
/// `api_region` 对它**根本不生效** ⇒ 换区既不改变实际请求的 host、也无从回写，
/// 只会白烧一次重试额度。
fn region_retry_target(
    current_region: &str,
    is_api_key: bool,
    has_ever_succeeded: bool,
) -> Option<&'static str> {
    if !is_api_key || has_ever_succeeded {
        return None;
    }
    let order = crate::kiro::region_probe::PROBE_ORDER;
    // 当前区在表内 ⇒ 取下一项（两项表即「换到另一个」）；不在表内 ⇒ 取首项。
    // 用取模而非硬编码 `[1]`/`[0]`：表若将来扩项，这里退化成「顺序轮换」而不是
    // 永远只在前两项之间跳（那种失败会静默）。
    let next = match order.iter().position(|r| *r == current_region) {
        Some(i) => order[(i + 1) % order.len()],
        None => *order.first()?,
    };
    // 表只有一项时上面的取模会算回自己 —— 换到同一个区是纯浪费一次重试额度。
    if next == current_region {
        return None;
    }
    Some(next)
}

/// 计算本次调用允许的总重试次数（动态预算）
///
/// - `total`：凭据总数
/// - `available`：当前未禁用（可用）凭据数
///
/// 预算 = `(total * per_cred).min(ABSOLUTE_MAX_TOTAL_RETRIES)`，再以 1 兜底。
///
/// ⚠️ **`available` 已不参与计算**（参数保留只为不动调用点与既有测试）。
/// 因此本函数**不再保证「每个可用凭据至少被尝试一次」** —— 号池大于
/// `ABSOLUTE_MAX_TOTAL_RETRIES` 时，单个请求扫不完全池。这是**刻意的权衡**，
/// 理由见函数体内 `.min()` 处的长注释（旧代码的内层 `.max(available)` 会让硬上限
/// 自我抵消，线上 43 号时预算 = 43，一条请求顺着整池撞一遍直到耗尽 45s 墙钟，
/// 净效果是「号池越大越慢」）。
///
/// 该权衡依赖一个前提：**坏号会被自动禁用从而不进候选集**，故预算 12 足够摸到
/// 足量健康号。号池规模显著超过 `ABSOLUTE_MAX_TOTAL_RETRIES` 时需重新评估这个前提。
///
/// **小号池降重试**：号池很小（`total <= SMALL_POOL_THRESHOLD`）时，每号重试次数降为 1。
/// 因为小池下重试循环只会反复选到同几个号——被限流时多打几次纯属反复砸、加重冷却，
/// 不如让每个号各摸一次就把上游错误透传给客户端（客户端自身有退避重试，比网关内反复砸温和）。
/// 号多时行为完全不变（仍 `MAX_RETRIES_PER_CREDENTIAL`）。
fn compute_max_retries(total: usize, _available: usize) -> usize {
    // `_available` 保留在签名里但**不再参与计算**：见下方 `.min()` 处的说明。
    // 保留参数是为了不改动调用点与既有测试；将来若确认永不需要，再一并删除。
    let per_cred = if total <= SMALL_POOL_THRESHOLD {
        1
    } else {
        MAX_RETRIES_PER_CREDENTIAL
    };
    (total * per_cred)
        // ⚠️ 这里**刻意不再**用 `.max(available)` 抬高上限。
        //
        // 旧代码是 `.min(ABSOLUTE_MAX_TOTAL_RETRIES.max(available))`，那个内层
        // `.max(available)` 会在 `available > 12` 时把硬上限自己抵消掉 → 预算等于
        // 可用号数。线上 43 个号时实测预算 = 43，日志里就是「尝试 43/43」：一条
        // 客户端请求要顺着整池撞一遍，撞到 45s 墙钟预算才失败。
        //
        // 净效果是**号池越大越慢**，与"扩号池提升吞吐"的目标正好相反。而"保证每个
        // 可用号至少被摸一次"这个原始意图本身就站不住：池子有 200 个号时，为一条
        // 请求打 200 次上游只会加重风控，而不会提高这条请求的成功率——真正该做的是
        // 让坏号被自动禁用而**不进入**候选集（见 token_manager 的
        // `report_suspicious_activity`），而不是靠遍历去撞。
        .min(ABSOLUTE_MAX_TOTAL_RETRIES)
        // ⚠️ 地板 1：预算为 0 等于**一次都不尝试**，请求直接以「已达到最大重试次数（0次）」
        // 失败，而 acquire_context 的等待循环根本没机会跑。
        //
        // 旧实现喂 `total_count()`（含 disabled 条目，恒 ≥ 池内号数）所以永远算不出 0，
        // 掩盖了这里缺下限。改喂 `kiro_selectable_count()` 后，**瞬时**全池不可选
        //（全部在冷却中 / inflight 打满）会让它返回 0 → 预算 0 → 请求零重试即失败。
        // 这是真实回归：线上 20 分钟内出现 10 次该错误。
        //
        // 取 1 而非 0 的语义：至少走一遍 acquire_context，让它的等待逻辑有机会等到号
        // 出冷却；等不到再由墙钟预算（MAX_REQUEST_RETRY_BUDGET_SECS）兜底透传错误。
        .max(1)
}

/// 近期上游压力滑动窗口。
///
/// 每次上游响应喂一个布尔（成功/4xx false，429/5xx true），窗口保留近
/// [`PRESSURE_WINDOW_SECS`] 秒。`rate()` 返回窗口内**压力占比**（429+5xx 占全部），
/// 供 [`apply_retry_pressure`] 动态降重试预算。
///
/// ⚠️ 5xx 也计入压力：纯 500 风暴同样是「疯狂重试」来源，只计 429 会让降档永不触发。
///
/// 热路径取舍：短临界区（一次 push + 逐出），锁竞争可接受 —— 即使内部 1000 RPM，
/// 每秒也才 17 次写，远低于锁的吞吐上限。
struct RetryPressureWindow {
    deque: std::collections::VecDeque<(std::time::Instant, bool)>,
    window: std::time::Duration,
}

impl RetryPressureWindow {
    fn new(window_secs: u64) -> Self {
        Self {
            deque: std::collections::VecDeque::new(),
            window: std::time::Duration::from_secs(window_secs),
        }
    }

    /// 记录一次上游响应结果。顺带惰性逐出超窗事件（不额外起定时器）。
    fn record(&mut self, is_pressure: bool) {
        let now = std::time::Instant::now();
        self.deque.push_back((now, is_pressure));
        self.prune(now);
    }

    /// 逐出超过窗口的事件（记录与读取共用，避免 rate() 读到空闲前的陈旧高压）。
    fn prune(&mut self, now: std::time::Instant) {
        while let Some(&(t, _)) = self.deque.front() {
            if now.duration_since(t) > self.window {
                self.deque.pop_front();
            } else {
                break;
            }
        }
    }

    /// 窗口内压力占比（0.0..=1.0）。空窗口返 0（无信号 = 不降档）。
    fn rate(&mut self) -> f32 {
        self.prune(std::time::Instant::now());
        let total = self.deque.len();
        if total == 0 {
            return 0.0;
        }
        let n_pressure = self.deque.iter().filter(|(_, is_pressure)| *is_pressure).count();
        n_pressure as f32 / total as f32
    }
}

/// 按近期上游压力率（429+5xx）动态降档重试预算。
///
/// 疯狂重试（号多 + 429/5xx 多）时每个请求扫 12 个号纯属放大受害面 —— 重试再多也换不到
/// 好号（大家都在被限流/过载），不如降档让客户端更快拿到错误自己退避。阶梯（整数除法）：
/// - 压力率 > 50%：预算 × 33/100（12 → 3）
/// - 压力率 > 30%：预算 × 1/2（12 → 6）
/// - 否则：不变
///
/// 只在 `base_retry_quota`（循环外一次计算）处乘系数，`round_retry_quota` 的
/// `min(剩余总额)` 语义天然把降档收进每请求预算，跨吸收轮不叠加。
fn apply_retry_pressure(base: usize, rate: f32) -> usize {
    let scaled = if rate > 0.5 {
        base * 33 / 100
    } else if rate > 0.3 {
        base / 2
    } else {
        base
    };
    scaled.max(1)
}

/// 一次成功调用的元数据（随响应回传给上层，供用量统计埋点关联）
///
/// provider 层掌握凭据/重试/延迟，但看不到最终 usage/credits（流式消费后才知道）；
/// 上层拿到本结构后与 `StreamContext::resolved_usage()` 合并即可产出完整记录。
pub struct CallMeta {
    /// 实际服务该请求的凭据 ID
    pub credential_id: u64,
    /// 请求模型名（从请求体解析，可能为 None）
    pub model: Option<String>,
    /// 会话标识（conversationId）
    pub session_id: Option<String>,
    /// 是否流式
    pub is_streaming: bool,
    /// 本次成功前经历的重试次数（0 表示首次即成功）
    pub retries: u32,
    /// 从进入调用到拿到成功响应头的耗时（毫秒）
    pub latency_ms: u64,
    /// 进入本次调用的时刻，与 [`Self::latency_ms`] **同源同起点**。
    ///
    /// 存在理由：`first_token_ms`（TTFB）此前全仓 0 个生产赋值点、线上 24h 全 NULL，
    /// 导致所有延迟分析失效。而首个内容 delta 是在 handler/stream 层才产生的，
    /// 那里拿不到 provider 的计时起点 —— 不导出这个 Instant 就只能用「响应头到首 token」，
    /// 与 `latency_ms` 不同起点、无法相减也无法比较。
    ///
    /// ⚠️ 起点在准入闸门（令牌桶排队）**之前**，故 `first_token_ms` 含入站排队时长；
    /// 想要纯上游生成延迟用 `first_token_ms - latency_ms`（两者同源，差值即
    /// 「响应头 → 首 token」）。failover 重试时不重置，故也含失败尝试耗时，
    /// 需要时按 `record.retries` 过滤。
    pub started_at: std::time::Instant,
    /// 在途请求守卫：随本 meta（进而随响应流）存活，直到 SSE 流被下游完全消费、
    /// 或客户端断开、或非流式响应读毕后才 Drop → 该凭据 inflight -1。
    /// 因此 inflight 反映"真正还在处理中"的请求数，而非"已拿到响应头"的数。
    ///
    /// 不参与 `Debug`（`InflightGuard` 无 Debug）；`CallMeta` 因此不再派生 `Debug`/`Clone`。
    ///
    /// 仅为 RAII 而持有、从不读取：其唯一作用是在 `CallMeta`（进而响应流）析构时
    /// 触发 `Drop` 把 inflight -1，故 `#[allow(dead_code)]` 而非移除。
    #[allow(dead_code)]
    pub inflight: crate::kiro::scheduling::InflightGuard,
}

/// 一次自定义 API 透传的元数据,供 handler 做 usage 埋点。
///
/// 透传路径不进 Kiro 解码器、拿不到真实 token/credit(隔离铁律 3),故只带调度维度信息;
/// token 由 handler 侧估算,credits 恒 None。与 [`CallMeta`] 分离,避免复用 Kiro 的 inflight/重试语义。
pub struct PassthroughMeta {
    /// 服务该请求的自定义 API 凭据 ID
    pub credential_id: u64,
    /// 请求模型名(原样,透传不映射)
    pub model: Option<String>,
    /// 会话标识
    pub session_id: Option<String>,
    /// 据上游 status 推断的用量结果分类
    pub outcome: crate::usage::RequestOutcome,
    /// 从选号到拿到上游响应头的耗时(毫秒)
    pub latency_ms: u64,
}

/// MCP（WebSearch 等工具调用）路径在用量库里的模型标识。
///
/// MCP 走的是 JSON-RPC over HTTP，请求体里**没有** `modelId`（不涉及模型推理），
/// 上游响应是搜索结果 JSON、既无 `meteringEvent` 也无任何 token 数。用一个显式常量
/// 标识这条路径，而不是冒用调用方那次请求的模型名——后者会让「某模型消耗了多少 token」
/// 的聚合凭空多出一批 token=0 的记录，反而更难解释。
const MCP_USAGE_MODEL: &str = "mcp";

/// 构造 MCP 路径的一条用量记录。
///
/// **诚实边界**：MCP 调用能确知的只有「哪张凭据、什么时候、被消耗了一次调用额度、
/// 耗时多久、重试了几次」，这恰好也是凭据 `success_count` 已经在记的东西。因此：
/// - `model` = [`MCP_USAGE_MODEL`]（上游请求体无 modelId，见常量注释）
/// - `input_tokens` / `output_tokens` = 0（上游不返回，也无本地估算依据；宁可为 0 也不瞎估）
/// - `credits_used` = None（MCP 响应无 meteringEvent）
/// - `is_streaming` = false（MCP 上游是一次性 JSON POST；WebSearch 对客户端的 SSE
///   是网关本地合成的，不属于这次上游调用的性质）
/// - `session_id` / 客户端画像 = None（provider 层拿不到入站 headers 与 conversationId）
fn build_mcp_record(
    credential_id: u64,
    outcome: crate::usage::RequestOutcome,
    latency_ms: u64,
    retries: u32,
) -> crate::usage::RequestRecord {
    let mut record =
        crate::usage::RequestRecord::new(uuid::Uuid::new_v4().to_string(), MCP_USAGE_MODEL);
    record.credential_id = Some(credential_id);
    record.is_streaming = false;
    record.input_tokens = 0;
    record.output_tokens = 0;
    record.credits_used = None;
    record.latency_ms = latency_ms;
    record.retries = retries;
    record.outcome = outcome;
    record
}

/// Kiro API Provider
///
/// 核心组件，负责与 Kiro API 通信
/// 支持多凭据故障转移和重试机制
/// 按凭据 `endpoint` 字段选择 [`KiroEndpoint`] 实现
pub struct KiroProvider {
    token_manager: Arc<MultiTokenManager>,
    /// 全局代理配置（用于凭据无自定义代理时的回退）
    global_proxy: Option<ProxyConfig>,
    /// Client 缓存：key = effective proxy config, value = reqwest::Client
    /// 不同代理配置的凭据使用不同的 Client，共享相同代理的凭据复用 Client
    client_cache: Mutex<HashMap<Option<ProxyConfig>, Client>>,
    /// TLS 后端配置
    tls_backend: TlsBackend,
    /// 端点实现注册表（key: endpoint 名称）
    endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
    /// 默认端点名称（凭据未指定 endpoint 时使用）
    default_endpoint: String,
    /// 端点桶 429 封禁状态：key = (credential_id, endpoint_name)，value = 解封时刻。
    /// 同一凭据的多端点（如 ksk_ 的 `cli`/`cli-runtime`）是上游的独立限流桶，某桶被封不波及其它。
    endpoint_buckets: Mutex<HashMap<(u64, String), Instant>>,
    /// 端点轮换计数器：`select_endpoint` 按请求轮换起始端点（round-robin），
    /// 让 ksk_ 号的流量从开始就分散到 q.* 与 runtime.* 两个桶，而不是永远先打 q.*。
    /// 对照 kiro2cc：它是每请求轮换起始端点，因此同一号多桶时流量均匀分散。
    /// `len == 1`（单端点 OAuth 号）时 start 恒 0，行为与固定优先序完全一致。
    endpoint_rotation: AtomicUsize,
    /// 全局上游并发闸：限制**同时在飞**的上游 HTTP 调用数（容量来自
    /// `upstream_concurrency_limit`，重启生效）。防「号多 + 429 多 → 疯狂换号重试」
    /// 把内部上游 RPM 放大到外部 RPM 的十几倍。`OwnedSemaphorePermit` 跨 send 存活、
    /// 作用域结束自动 Drop 释放，免费防泄漏。
    upstream_gate: Arc<tokio::sync::Semaphore>,
    /// 近 60s 上游结果滑动窗口（成功/429），喂给 [`apply_retry_pressure`] 做动态降档。
    retry_pressure: Mutex<RetryPressureWindow>,
}

impl KiroProvider {
    /// 创建带代理配置和端点注册表的 KiroProvider 实例
    ///
    /// # Arguments
    /// * `token_manager` - 多凭据 Token 管理器
    /// * `proxy` - 全局代理配置
    /// * `endpoints` - 端点名 → 实现的注册表（至少包含 `default_endpoint` 对应条目）
    /// * `default_endpoint` - 凭据未显式指定 endpoint 时使用的名称
    pub fn with_proxy(
        token_manager: Arc<MultiTokenManager>,
        proxy: Option<ProxyConfig>,
        endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
        default_endpoint: String,
    ) -> Self {
        assert!(
            endpoints.contains_key(&default_endpoint),
            "默认端点 {} 未在 endpoints 注册表中",
            default_endpoint
        );
        let tls_backend = token_manager.config().tls_backend;
        // 预热：构建全局代理对应的 Client
        // 对话路径用流式 client：read_timeout(空闲间隔) 而非总时长，防长流被中途掐断
        // （根因见 build_streaming_client 注释：修 `Connection closed mid-response`）。
        let initial_client =
            build_streaming_client(proxy.as_ref(), 720, tls_backend).expect("创建 HTTP 客户端失败");
        let mut cache = HashMap::new();
        cache.insert(proxy.clone(), initial_client);

        let concurrency_limit = token_manager
            .config()
            .upstream_concurrency_limit
            .max(1);
        Self {
            token_manager,
            global_proxy: proxy,
            client_cache: Mutex::new(cache),
            tls_backend,
            endpoints,
            default_endpoint,
            endpoint_buckets: Mutex::new(HashMap::new()),
            endpoint_rotation: AtomicUsize::new(0),
            upstream_gate: Arc::new(tokio::sync::Semaphore::new(concurrency_limit)),
            retry_pressure: Mutex::new(RetryPressureWindow::new(PRESSURE_WINDOW_SECS)),
        }
    }

    /// 根据凭据的代理配置获取（或创建并缓存）对应的 reqwest::Client
    fn client_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Client> {
        let effective = credentials.effective_proxy(self.global_proxy.as_ref());
        let mut cache = self.client_cache.lock();
        if let Some(client) = cache.get(&effective) {
            return Ok(client.clone());
        }
        let client = build_streaming_client(effective.as_ref(), 720, self.tls_backend)?;
        cache.insert(effective, client.clone());
        Ok(client)
    }

    /// 根据凭据选择 endpoint 实现
    fn endpoint_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Arc<dyn KiroEndpoint>> {
        // ⭐ 必须走 effective_endpoint，与 `endpoint::for_credentials` / `main.rs` 启动校验
        // / admin snapshot 三处口径一致。
        //
        // 🔴 修复的缺陷（另一位 review 抓到，实测确证）：此处原先只读 `credentials.endpoint`
        // 原始字段，**漏了 `ksk_` API Key 号自动路由到 CLI 端点**这一层。
        // 而 `endpoint/mod.rs` 的 `for_credentials` 文档明写"口径与 endpoint_for 完全一致" ——
        // 那句话此前是**假的**：旁路走 effective_endpoint、请求热路径不走。
        //
        // 后果链（与线上号池被烧直接相关）：一个健康的 `ksk_` 号若未手工填 `endpoint: cli`，
        // 请求会打到 IDE 端点 → 403 → 连续 6 次触发 `report_suspicious_activity`
        // → 判定死号自动禁用。实测 `effective_endpoint()` 返回 `cli` 而此处返回 `ide`。
        let name = credentials.effective_endpoint(&self.default_endpoint);
        self.endpoints
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("未知端点: {}", name))
    }

    /// 按凭据的端点候选顺序选**第一个未封禁**的端点实现（q.* 优先、runtime.* 回退）。
    ///
    /// - 起始索引按请求轮换（round-robin）：从 [`KiroCredentials::effective_endpoint_order`]
    ///   的候选里取 `start = 轮换计数器 % len` 作为遍历起点，再按原顺序向后取。
    ///   这样 ksk_ 号的流量从开始就分散到 q.* 与 runtime.* 两个桶（对照 kiro2cc 的
    ///   `select_endpoint` 按 attempt 偏移轮询），而不是永远先打 q.*、打爆了才换 runtime.*。
    /// - 顺序遍历语义保留：仍按候选序跳过冷却桶、取第一个非冷却；`len == 1` 时轮换无效果，
    ///   行为与固定优先序完全一致。
    /// - 返回 `None` = 该凭据所有端点桶当前都在封禁期 → 调用方应走凭据级冷却/换号。
    fn select_endpoint(
        &self,
        credentials: &KiroCredentials,
        id: u64,
    ) -> Option<Arc<dyn KiroEndpoint>> {
        let order = credentials.effective_endpoint_order(&self.default_endpoint);
        if order.is_empty() {
            return None;
        }
        let len = order.len();
        // 按请求轮换起始索引：同一凭据的连续请求落在不同端点。len == 1 时恒取 0。
        let start = self.endpoint_rotation.fetch_add(1, Ordering::Relaxed) % len;
        let mut buckets = self.endpoint_buckets.lock();
        let now = Instant::now();
        for i in 0..len {
            let name = order[(start + i) % len];
            let key = (id, name.to_string());
            // 惰性清理：已过期即视为可用并从 map 移除，防无界增长。
            if let Some(&until) = buckets.get(&key) {
                if now < until {
                    continue; // 该桶仍在封禁期，换下一个端点
                }
                buckets.remove(&key);
            }
            if let Some(ep) = self.endpoints.get(name) {
                return Some(ep.clone());
            }
        }
        None
    }

    /// 该凭据是否还有**未封禁**的端点桶（429 时决定「换端点继续」还是「冷却换号」）。
    fn has_unthrottled_endpoint(&self, credentials: &KiroCredentials, id: u64) -> bool {
        let order = credentials.effective_endpoint_order(&self.default_endpoint);
        if order.is_empty() {
            return false;
        }
        let buckets = self.endpoint_buckets.lock();
        let now = Instant::now();
        order.iter().any(|name| {
            let throttled =
                matches!(buckets.get(&(id, name.to_string())), Some(&until) if now < until);
            !throttled
        })
    }

    /// 发送非流式 API 请求
    ///
    /// 支持多凭据故障转移（见 [`Self::call_api_with_retry`]）
    pub async fn call_api(
        &self,
        request_body: &str,
        is_1m: bool,
    ) -> anyhow::Result<(reqwest::Response, CallMeta)> {
        self.call_api_with_retry(request_body, false, is_1m).await
    }

    /// 发送流式 API 请求
    pub async fn call_api_stream(
        &self,
        request_body: &str,
        is_1m: bool,
    ) -> anyhow::Result<(reqwest::Response, CallMeta)> {
        self.call_api_with_retry(request_body, true, is_1m).await
    }

    /// 发送 MCP API 请求（WebSearch 等工具调用）
    pub async fn call_mcp(&self, request_body: &str) -> anyhow::Result<reqwest::Response> {
        self.call_mcp_with_retry(request_body).await
    }

    /// 混入池分流:选一次号,若命中「自定义 API」凭据则原样透传原始 Anthropic 请求体到其上游、
    /// 返回 `Some(透传响应)`;若选到 Kiro 号(或无自定义号)则返回 `None`,由调用方走原 Kiro 路径。
    ///
    /// ⚠️ 与 Kiro 主路径隔离:本方法只在选到 custom_api 时接管;选到 Kiro 号时**立即释放**
    /// (drop inflight 守卫)并返回 None,不影响后续 Kiro 正常选号/转发。`raw_body` 是**未经
    /// Kiro 转换**的客户端原始请求体(透传要原样发)。
    ///
    /// `model` 供选号做模型过滤/亲和(与 Kiro 路径同源解析);命中自定义号时记一次请求(上限计数)。
    pub async fn try_custom_api_passthrough(
        &self,
        raw_body: bytes::Bytes,
        model: Option<&str>,
        user_id: Option<&str>,
    ) -> Option<(axum::response::Response, PassthroughMeta)> {
        // 从**custom_api 专属选号池**里 failover 调度(独立于 Kiro 选号,守两池隔离铁律)。
        // 语义(dwgx 定):池内按优先级+RPM 均衡选号;某号 403 额度满/401 key 失效/429/5xx →
        // 给该号短冷却 + 换下一个 custom_api;全部 custom_api 不可用 → 返回 None,由上层落 Kiro 主力路径。
        // 4xx(非 403,客户端请求错误)→ 换号也一样错,直接把该响应返给客户端(不 failover、不落 Kiro)。
        // 注:model/user_id 暂不参与 custom_api 选号(代挂上游自行处理模型),仅随 meta 供埋点关联。
        let mut excluded: HashSet<u64> = HashSet::new();
        loop {
            let (id, cred) = match self.token_manager.select_custom_api(&excluded, model) {
                Some(x) => x,
                // 无更多可用 custom_api 号:①一开始就没(excluded 空)→ 池里无透传号,零开销落 Kiro;
                // ②都试过失败(excluded 非空)→ custom_api 全额度满/失败,failover 落 Kiro 主力。
                None => return None,
            };
            let started = std::time::Instant::now();
            // 全局 deepseek 归一化配置（TIER1 热重载，per-凭据在 forward 内覆盖）。
            let ds_cfg = self.token_manager.config().deepseek_normalize.clone();
            let (resp, status) = crate::kiro::passthrough::forward(
                &cred,
                raw_body.clone(),
                self.global_proxy.as_ref(),
                self.tls_backend,
                &ds_cfg,
            )
            .await;
            let latency_ms = started.elapsed().as_millis() as u64;
            // 据上游 status 推断 outcome(与 Kiro 主路径同口径)。502 含真上游 5xx 与本地连接失败。
            let code = status.as_u16();
            let outcome = match code {
                s if (200..300).contains(&s) => crate::usage::RequestOutcome::Success,
                429 => crate::usage::RequestOutcome::RateLimited,
                402 => crate::usage::RequestOutcome::QuotaExhausted, // 中转站常用 402 表额度耗尽
                401 | 403 => crate::usage::RequestOutcome::AuthFailed,
                s if (500..600).contains(&s) => crate::usage::RequestOutcome::ServerError,
                s if (400..500).contains(&s) => crate::usage::RequestOutcome::BadRequest,
                _ => crate::usage::RequestOutcome::OtherError,
            };
            // 轻量结果计数(隔离铁律:绝不复用 report_success/failure 的 cooldown/family 连坐)。
            self.token_manager.record_passthrough_result(id, outcome);

            // 成功 → 直接返回该号的响应流。
            if (200..300).contains(&code) {
                let meta = PassthroughMeta {
                    credential_id: id,
                    model: model.map(|s| s.to_string()),
                    session_id: user_id.map(|s| s.to_string()),
                    outcome,
                    latency_ms,
                };
                return Some((resp, meta));
            }

            // ⭐ 显式列出「该 failover 的状态码」而非用"4xx 非403"反推——后者会让 401/429 先命中
            //    下方 4xx 直返、永远到不了 failover(对抗 review B1 抓到的持久黑洞:429 号不切换)。
            // - 401 key 失效 / 402·403 额度耗尽 / 429 限流 / 5xx 上游错误 → 该号短冷却 + 换下一个 custom_api。
            // - 其余 4xx(400/404/422 等客户端请求错误)→ 换号/落 Kiro 也一样错,直接返给客户端。
            let should_failover =
                matches!(code, 401 | 402 | 403 | 429) || (500..600).contains(&code);
            if !should_failover {
                let meta = PassthroughMeta {
                    credential_id: id,
                    model: model.map(|s| s.to_string()),
                    session_id: user_id.map(|s| s.to_string()),
                    outcome,
                    latency_ms,
                };
                return Some((resp, meta));
            }

            // 冷却时长按性质。⭐ dwgx 定的语义:**代挂号是用户自购的付费中转站,不是 Kiro 号**,
            // 它没有"被风控"这个状态,429 只代表"它现在忙"。
            //
            // 🔴 修复:429 原先给 30s 冷却。那是把 Kiro 号的风控模型错套到代挂号上——
            // 用户已经为这个上游付过钱,把它按下 30 秒既不能让它变快,又白白缩小了可用池
            // (极端情况:两个代挂号轮流 429 → 两个都被冷却 → 整池不可用 → 回落 Kiro,
            //  而 Kiro 侧此刻可能正被风控烧号)。偶尔 429 只该 failover,不该留痕。
            //
            // 现在:429 与 5xx 同列为**瞬态**,本请求链内 exclude 换下一个号即可,零冷却。
            // 无论 429 持续多久都不写 disabled；这里只做本请求内 failover 与短调度跳过。
            let cooldown_secs = match code {
                // 401 key 失效 / 402·403 额度耗尽:**非瞬态**,短期内重试必然还是失败。
                // 给冷却是为了别让同一请求链外的后续请求继续撞它;真正的处置(自动禁用)
                // 由 record_passthrough_result 的连续失败计数负责。
                401 | 402 | 403 => 180,
                // 429 / 5xx / 网络:瞬态。给一个**极短**的调度级跳过,而不是零。
                //
                // 为什么不是 0（审查发现的延迟回归）：`excluded` 只在**本请求链内**生效，
                // 跨请求不起作用。若完全不冷却，一个 100% 429 的中转站会被**每一个**新请求
                // 重新选中（select_custom_api 按 priority/RPM 排序，它排在前面），
                // 每次都白付一次上游往返才 failover —— 而持续过载的自动禁用要 300s 才生效，
                // 若完全不跳过，每个新请求都会多等一个失败 RTT。
                //
                // 5s 是刻意取的平衡点：它**不是**惩罚（不进 health、不计失败、不影响自动禁用判据，
                // 满足"偶尔 429 绝不惩罚"），只是调度上避免同一秒内把所有请求都撞向同一个忙站；
                // 而 5s 远低于人可感知的池容量缩水（旧值 30s 才是真正的惩罚性退避）。
                429 => 5,
                // 5xx / 网络：真瞬态，可能只是抖一下，不跳过。
                _ => 0,
            };
            if cooldown_secs > 0 {
                self.token_manager.cooldown_custom_api(id, cooldown_secs);
                tracing::warn!(
                    credential_id = id,
                    status = code,
                    "自定义 API 透传失败(非瞬态),该号冷却 {}s 并 failover 下一个 custom_api",
                    cooldown_secs
                );
            } else {
                tracing::warn!(
                    credential_id = id,
                    status = code,
                    "自定义 API 透传失败(瞬态,如 429/5xx),**不冷却**,仅本请求内 failover 下一个 custom_api"
                );
            }
            excluded.insert(id);
            // 丢弃本次错误响应,继续循环试下一个 custom_api;全部试完 select 返 None → 落 Kiro。
        }
    }

    /// 累加一次请求的真实 credit 花费到该凭据的生命周期累计（透传到 token_manager）。
    ///
    /// handler 在请求完成、从上游 meteringEvent 拿到真实计费量后调用；provider 持有
    /// token_manager，handler 只有 provider，故在此开一个薄 passthrough。
    pub fn report_credits(&self, credential_id: u64, credits: f64) {
        self.token_manager.add_credits(credential_id, credits);
    }

    /// 借出内部的号池管理器（只读用途）。
    ///
    /// handler 只持有 provider，但需要在**分派之前**做跨池优先级仲裁
    /// （`should_try_custom_api_first`：决定这次请求先走 custom_api 透传还是先走 Kiro）。
    /// 与 `report_credits` 同款薄 passthrough 思路，避免把仲裁逻辑复制到 handler 层。
    pub fn token_manager(&self) -> &MultiTokenManager {
        &self.token_manager
    }

    /// 内部方法：带重试逻辑的 MCP API 调用
    async fn call_mcp_with_retry(&self, request_body: &str) -> anyhow::Result<reqwest::Response> {
        let call_started = std::time::Instant::now();
        let max_retries =
            // 预算按「Kiro 路径**实际可选**的号数」算，而非 entries.len()：后者含 disabled
            // 与 custom_api 条目（is_entry_selectable 永远拒绝 custom_api），会把预算凭空
            // 抬高 —— 生产日志的 `尝试 8/36` 即由此而来。见 kiro_selectable_count 的说明。
            {
                let selectable = self.token_manager.kiro_selectable_count();
                compute_max_retries(selectable, selectable)
            };
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        // 与对话路径同款的两个链内状态：
        // - `rate_limited_this_call`：同一请求链内每个号只因风控冷却一次，不重复惩罚。
        // - `suspicious_failovers_this_call`：账户级风控的跨号转移上限，防线性扫全池。
        let mut rate_limited_this_call: HashSet<u64> = HashSet::new();
        let mut suspicious_failovers_this_call: usize = 0;
        const MAX_SUSPICIOUS_FAILOVERS_PER_CALL: usize = 3;
        // 已知问题 #11：MCP 路径失败零埋点 → 失败在面板上不可见。以下在所有失败出口
        // （5 条 bail + client_for `?` + 重试耗尽）统一 emit_record + bump_mcp_failure。
        let mut last_credential_id: Option<u64> = None;
        let mut last_outcome = crate::usage::RequestOutcome::OtherError;
        let mut attempts_used: u32 = 0;

        for attempt in 0..max_retries {
            // 失败记录的 retries 用「已尝试次数 - 1」＝重试次数（与对话路径同口径）。
            attempts_used = attempt as u32;
            // MCP 调用（WebSearch 等工具）不涉及模型选择，无需按模型过滤凭据
            let ctx = match self.token_manager.acquire_context(None, None).await {
                Ok(c) => {
                    last_credential_id = Some(c.id);
                    c
                }
                Err(e) => {
                    let es = e.to_string();
                    if es.contains("retry_after_secs=") || es.contains("冷却") {
                        last_outcome = crate::usage::RequestOutcome::RateLimited;
                    }
                    last_error = Some(e);
                    continue;
                }
            };

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, &config);

            let endpoint = match self.select_endpoint(&ctx.credentials, ctx.id) {
                Some(e) => e,
                None => {
                    last_outcome = crate::usage::RequestOutcome::RateLimited;
                    last_error = Some(anyhow::anyhow!(
                        "凭据 #{} 所有端点桶均处于 429 封禁期",
                        ctx.id
                    ));
                    // ⚠️ 不得 report_failure：None 代表**端点桶 30s 封禁**（瞬态），不是未知端点
                    // 配置错误。report_failure 会累计 failure_count → TooManyFailures 永久禁用
                    // 一个只是被上游限流 30s 的健康号。设 30s 短冷却让调度避开，等桶解封。
                    if rate_limited_this_call.insert(ctx.id) {
                        self.token_manager.report_rate_limited_with_retry_after(
                            ctx.id,
                            Some(ENDPOINT_BUCKET_THROTTLE.as_secs()),
                        );
                    }
                    continue;
                }
            };

            let rctx = RequestContext {
                credentials: &ctx.credentials,
                token: &ctx.token,
                machine_id: &machine_id,
                config: &config,
                // MCP(WebSearch 等)不涉及模型对话上下文,无 1M 语义。
                is_1m: false,
            };

            let url = endpoint.mcp_url(&rctx);
            let body = endpoint.transform_mcp_body(request_body, &rctx);

            // client_for 失败（代理/TLS 配置错误等）也走失败埋点：此前 `?` 裸传播，
            // 面板上这条请求同样不存在（已知问题 #11 的 7 个失败出口之一）。
            let client = match self.client_for(&ctx.credentials) {
                Ok(c) => c,
                Err(e) => {
                    crate::common::recovery_metrics::bump_mcp_failure();
                    crate::usage::emit_record(build_mcp_record(
                        ctx.id,
                        crate::usage::RequestOutcome::OtherError,
                        call_started.elapsed().as_millis() as u64,
                        attempts_used,
                    ));
                    return Err(e);
                }
            };
            let base = client
                .post(&url)
                .body(body)
                .header("content-type", "application/json");
            let request = endpoint.decorate_mcp(base, &rctx);

            let response = match request.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    last_outcome = crate::usage::RequestOutcome::NetworkError;
                    tracing::warn!(
                        "MCP 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    last_error = Some(e.into());
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            };

            let status = response.status();

            // 成功响应
            if status.is_success() {
                self.token_manager.report_success(ctx.id);
                // 用量埋点：MCP 成功路径也落一条记录。
                // 历史缺陷：这里只调 report_success 让凭据 success_count +1，却没有任何
                // emit_record，于是「凭据统计的成功次数」恒大于「用量库的记录数」
                // （实测某号 success_count=2070 而 SQLite 仅 951 条），号池可视化与用量
                // 明细对不上账。字段口径见 [`build_mcp_record`] 的诚实边界说明。
                crate::usage::emit_record(build_mcp_record(
                    ctx.id,
                    crate::usage::RequestOutcome::Success,
                    call_started.elapsed().as_millis() as u64,
                    attempt as u32,
                ));
                return Ok(response);
            }

            // 失败响应
            let body = response.text().await.unwrap_or_default();

            // 额度用尽（**不门控状态码**，理由同对话路径那处的长注释：
            // 上游已从 402 改用 400，402 实测 6 小时 0 次而 400+OVERAGE 564 次）
            if endpoint.is_monthly_request_limit(&body) {
                let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                if !has_available {
                    // 失败埋点（#11）：此前裸 bail，失败在面板上不存在。
                    crate::common::recovery_metrics::bump_mcp_failure();
                    crate::usage::emit_record(build_mcp_record(
                        ctx.id,
                        crate::usage::RequestOutcome::QuotaExhausted,
                        call_started.elapsed().as_millis() as u64,
                        attempts_used,
                    ));
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_outcome = crate::usage::RequestOutcome::QuotaExhausted;
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 400 Bad Request
            if status.as_u16() == 400 {
                crate::common::recovery_metrics::bump_mcp_failure();
                crate::usage::emit_record(build_mcp_record(
                    ctx.id,
                    crate::usage::RequestOutcome::BadRequest,
                    call_started.elapsed().as_millis() as u64,
                    attempts_used,
                ));
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 401/403 凭据问题
            if matches!(status.as_u16(), 401 | 403) {
                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会。
                //
                // ⚠️ **api_key 号必须跳过**：它没有 refreshToken，`refresh_token()` 对它是
                // 契约级 bail（"API Key 凭据不支持刷新 Token"，见 token_manager.rs 该处注释：
                // 那个 bail 是给面板「强制刷新」按钮设计的，让错误传播成 400）。
                // 在**请求热路径**上调它则是纯损耗：结构上不可能成功，而失败会
                // ① 计入失败计数、② 落 auth 冷却。更糟的是该错误串不含任何永久 HTTP 码，
                // 被刷新层的瞬态判据（黑名单式）当成可重试 → 1s/2s 退避重试 3 次。
                //
                // 线上实测（本轮多开时暴露）：一个 api_key 号遇 403 后每轮白等约 3 秒、
                // 连计 3 次失败即被判死号自动禁用 —— 相当于**把它的死亡速度放大三倍**。
                // 对 api_key 号，401/403 的含义就是「这个 key 现在不被接受」，
                // 直接走下方的风控/失败分类即可，不该绕一趟刷新。
                if endpoint.is_bearer_token_invalid(&body)
                    && !force_refreshed.contains(&ctx.id)
                    && !ctx.credentials.is_api_key_credential()
                {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    if self
                        .token_manager
                        .force_refresh_token_for(ctx.id)
                        .await
                        .is_ok()
                    {
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                    // 刷新失败 = 认证态有问题，加一段冷却让调度避开它。
                    //
                    // ⭐ 时长按**该号是否被证明过**二分（与对话路径同处逐字同款）：
                    // 刷新层内部已对 5xx/网络错误退避重试 3 次（见
                    // `report_refresh_failure_classified` 的文档），所以能走到这里的
                    // 刷新失败里上游 token 端点抖动占大头。一个已成功过的号吃一次抖动
                    // 就被冻 24h（`AuthenticationFailed` 的 `is_auto_recoverable=false`
                    // ⇒ long_cooldown 86400s）= 面板上的僵尸；而从未成功过的号刷新还失败，
                    // 大概率 refreshToken 真废了，该硬冻等人工。
                    if self.token_manager.has_ever_succeeded(ctx.id) {
                        self.token_manager.report_auth_transient_cooldown(ctx.id);
                    } else {
                        self.token_manager.report_auth_cooldown(ctx.id);
                    }
                }

                // 账户级**临时**风控限速（suspicious activity / temporary limits）：
                // 与对话路径同口径（见 `call_api_with_retry` 的 is_temporary_rate_limit 分支），
                // 必须在落 `report_failure` 之前判定。
                //
                // 历史缺陷（本分支原先直接 report_failure）：403 TEMPORARILY_SUSPENDED 是
                // **临时态**，而 report_failure 累加 failure_count，达 MAX_FAILURES_PER_CREDENTIAL
                // 即以 TooManyFailures（**永久型**标签）禁用。于是一个只是被临时限流的号，
                // 走 WebSearch/MCP 被打 3 次 403 就被永久禁用 —— 正是历史事故的同一误判形态
                // （403 曾被当永久封禁 → 12h 内 88 次误禁 + 36 次全池自愈活锁）。对话路径已修，
                // 本路径此前漏修；且自动禁用落盘后（persist_disabled_state）该误禁**重启也回不来**。
                if endpoint.is_temporary_rate_limit(&body) {
                    last_outcome = crate::usage::RequestOutcome::RateLimited;
                    tracing::warn!(
                        "MCP 请求失败（账户临时风控限速，非永久封禁；分钟级退避后 failover，尝试 {}/{}）: {} {}",
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );
                    // 账户级风控也是上游限速信号 → 入站整形 RPM 自动降档。
                    self.token_manager.report_upstream_rate_limited();
                    // 本请求链内该号首次触发才设冷却；再次触发只 failover，不重复惩罚
                    // （与对话路径的 rate_limited_this_call 同款去重，避免一条链把号砸进更深风控）。
                    if rate_limited_this_call.insert(ctx.id) {
                        self.token_manager.report_suspicious_activity(ctx.id);
                    } else {
                        tracing::debug!(
                            "凭据 #{} 本 MCP 请求链内已因风控冷却过，再次触发仅 failover，不重复惩罚",
                            ctx.id
                        );
                    }
                    last_error = Some(anyhow::anyhow!(
                        "MCP 请求失败（账户级可疑活动风控，分钟级退避）: {} {}",
                        status,
                        body
                    ));
                    // 跨号转移上限：与对话路径同款，超过即停止遍历并透传错误。
                    // 不设上限会线性扫全池，既让用户干等，又把整池号一起送进上游风控。
                    suspicious_failovers_this_call += 1;
                    if suspicious_failovers_this_call >= MAX_SUSPICIOUS_FAILOVERS_PER_CALL {
                        tracing::error!(
                            "本次 MCP 请求已因账户级风控转移 {} 次号，停止遍历号池并透传错误",
                            suspicious_failovers_this_call
                        );
                        break;
                    }
                    continue;
                }

                // 账户被永久暂停/封禁：禁用该号并换号（同样先于通用失败判定，
                // 使 disabled_reason 落 AccountSuspended 而非 TooManyFailures）。
                if endpoint.is_account_suspended(&body) {
                    tracing::error!(
                        "MCP 请求失败（账户被暂停/封禁，禁用凭据并切换，尝试 {}/{}）: {} {}",
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );
                    self.token_manager.report_upstream_pressure();
                    let has_available = self.token_manager.report_account_suspended(ctx.id);
                    if !has_available {
                        // 失败埋点（#11）。
                        crate::common::recovery_metrics::bump_mcp_failure();
                        crate::usage::emit_record(build_mcp_record(
                            ctx.id,
                            crate::usage::RequestOutcome::AccountSuspended,
                            call_started.elapsed().as_millis() as u64,
                            attempts_used,
                        ));
                        anyhow::bail!(
                            "MCP 请求失败（账户被封禁且所有凭据已用尽）: {} {}",
                            status,
                            body
                        );
                    }
                    last_outcome = crate::usage::RequestOutcome::AccountSuspended;
                    last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                    continue;
                }

                let has_available = self.token_manager.report_failure(ctx.id);
                if !has_available {
                    // 失败埋点（#11）。
                    crate::common::recovery_metrics::bump_mcp_failure();
                    crate::usage::emit_record(build_mcp_record(
                        ctx.id,
                        crate::usage::RequestOutcome::AuthFailed,
                        call_started.elapsed().as_millis() as u64,
                        attempts_used,
                    ));
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_outcome = crate::usage::RequestOutcome::AuthFailed;
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 瞬态错误
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                last_outcome = if status.as_u16() == 429 {
                    crate::usage::RequestOutcome::RateLimited
                } else {
                    crate::usage::RequestOutcome::ServerError
                };
                tracing::warn!(
                    "MCP 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                // 🔀 429 换桶：仅多端点凭据封当前 host 桶 30s。MCP 的 `acquire_context(None, None)`
                // 无 tried 排除集 ⇒ 同凭据可被反复选中，封桶后下一轮 `select_endpoint` 自动跳过
                // 它换下一端点；全部端点都封时 `select_endpoint` 返回 None → None 分支设 30s 冷却
                // 兜底，不会死循环。
                if status.as_u16() == 429 {
                    let order = ctx.credentials.effective_endpoint_order(&self.default_endpoint);
                    if order.len() > 1 {
                        self.endpoint_buckets.lock().insert(
                            (ctx.id, endpoint.name().to_string()),
                            Instant::now() + ENDPOINT_BUCKET_THROTTLE,
                        );
                    }
                }
                if attempt + 1 < max_retries {
                    sleep(Self::retry_delay(attempt)).await;
                }
                continue;
            }

            // 其他 4xx
            if status.is_client_error() {
                // 失败埋点（#11）。
                crate::common::recovery_metrics::bump_mcp_failure();
                crate::usage::emit_record(build_mcp_record(
                    ctx.id,
                    crate::usage::RequestOutcome::BadRequest,
                    call_started.elapsed().as_millis() as u64,
                    attempts_used,
                ));
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 兜底
            last_outcome = crate::usage::RequestOutcome::OtherError;
            last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        // 重试耗尽：失败也落一条记录（#11，第 7 个失败出口）。credential_id 未知时如实置 None。
        crate::common::recovery_metrics::bump_mcp_failure();
        let mut rec = build_mcp_record(
            last_credential_id.unwrap_or_default(),
            last_outcome,
            call_started.elapsed().as_millis() as u64,
            attempts_used,
        );
        if last_credential_id.is_none() {
            rec.credential_id = None;
        }
        crate::usage::emit_record(rec);
        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!("MCP 请求失败：已达到最大重试次数（{}次）", max_retries)
        }))
    }

    /// 内部方法：带重试逻辑的 API 调用
    ///
    /// 重试策略：
    /// - 每个凭据最多重试 MAX_RETRIES_PER_CREDENTIAL 次
    /// - 总重试预算由 [`compute_max_retries`] 动态计算：以可用凭据数为下限，
    ///   保证每个可用凭据至少被摸一次；以 ABSOLUTE_MAX_TOTAL_RETRIES 为安全上限
    async fn call_api_with_retry(
        &self,
        request_body: &str,
        is_stream: bool,
        is_1m: bool,
    ) -> anyhow::Result<(reqwest::Response, CallMeta)> {
        // 「基础」配额:一轮 failover 链最多摸几个号。吸收层开启时它**不是**本轮的实际配额
        // —— 实际配额还要被跨轮总额度夹一次(见 round_retry_quota)。刻意不叫 `max_retries`:
        // 循环内那个同名变量才是本轮生效值,同名两义必混。
        let base_retry_quota =
            // 预算按「Kiro 路径**实际可选**的号数」算，而非 entries.len()：后者含 disabled
            // 与 custom_api 条目（is_entry_selectable 永远拒绝 custom_api），会把预算凭空
            // 抬高 —— 生产日志的 `尝试 8/36` 即由此而来。见 kiro_selectable_count 的说明。
            {
                let selectable = self.token_manager.kiro_selectable_count();
                // 动态降档：近期上游压力率（429+5xx）高（疯狂重试）时按比例收缩预算，
                // 避免号多 + 压力多时每个请求扫 12 个号、把内部上游 RPM 放大到外部 RPM 的十几倍。
                // 只在进循环前算一次，跨轮不叠加。
                let raw = compute_max_retries(selectable, selectable);
                let pressure = self.retry_pressure.lock().rate();
                let scaled = apply_retry_pressure(raw, pressure);
                if scaled != raw {
                    tracing::warn!(
                        "上游压力率 {:.1}% 过高，重试预算从 {} 动态降档到 {}（防内部放大）",
                        pressure * 100.0,
                        raw,
                        scaled
                    );
                }
                scaled
            };
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        // 本次请求重试链内「已因 429 冷却过」的凭据集合。防止同一个请求的一条重试链
        // 反复砸同一个号、把同一次限流事件当成多次独立事件累加 trigger_count / 指数延长冷却
        // （根因：小号池下重试循环反复选到同两个号，单请求就把 trigger_count 刷到 7、冷却 15→72s，
        //  自造雪崩）。首次 429 才设冷却，同链再 429 只换号 failover，不重复惩罚。
        // 跨请求（新请求 = 新集合）仍正常累加，保留「持续被限流的号冷却渐长」的合理行为。
        let mut rate_limited_this_call: HashSet<u64> = HashSet::new();
        // 本次请求是否已因「账户被暂停」转移过号。suspend 是账号级信号且多伴随同出口 IP
        // 的整体风控，遍历全池只会把剩下的号一起烧掉（见 suspend 分支处的说明）。
        let mut suspended_this_call = false;
        // 本次请求已因**账户级临时风控**（403 TEMPORARILY_SUSPENDED）转移过多少次号。
        //
        // ⚠️ 此前该分支只有 `rate_limited_this_call` 的**同号**去重，没有任何**跨号**上限，
        // 于是可以线性扫全池：线上 43 个号实测「尝试 43/43」，一条请求打 43 次上游、
        // 耗尽 45s 墙钟才失败。而 account_suspended 分支早就有 `suspended_this_call`
        // 限一次——同为账号级风控信号，这里缺了等价物。
        //
        // 取 3 而非 1：403 有两种成因，必须都照顾到。
        //   ① 单号被上游盯上（换号就能成功）→ 需要允许换几次；
        //   ② 同出口 IP 整池风控（换号无用，只会把更多号烧进风控）→ 必须尽快停。
        // 3 次足以跨过少数坏号拿到好号，又不会把整池扫一遍。配合自动禁用
        // （连续零成功即移出候选集），坏号根本不该反复进入候选，这个上限只是纵深防御。
        let mut suspicious_failovers_this_call: usize = 0;
        /// 单请求因账户级临时风控最多转移几次号（见 `suspicious_failovers_this_call`）。
        const MAX_SUSPICIOUS_FAILOVERS_PER_CALL: usize = 3;
        // 本次请求已在通用 401/403 分支惩罚过的号，避免同一个号在一条请求里被连打 3 次
        // 直接推到 TooManyFailures（custom_api 路径早有 excluded 集，Kiro 路径此前没有）。
        let mut auth_failed_this_call: HashSet<u64> = HashSet::new();
        // 本请求链内已因 403 FEATURE_NOT_SUPPORTED 做过「本地 region 纠正 + 重试」的号(镜像
        // force_refreshed 去重惯例)。防同一坏号在一条链里反复本地纠正+重试烧光 max_retries。
        let mut region_corrected_this_call: HashSet<u64> = HashSet::new();
        // ⭐ L1 换区重试的**每号一次**上限（镜像 `force_refreshed` 去重惯例）。
        //
        // 不加上限就是两个区来回打：A 区 403 → 换 B → B 区 403 → 换回 A → …… 一条客户端
        // 请求把额度全烧在同一个号的两个区之间，同一出口 IP 连打 = 正是风控要抓的突发特征。
        // 本仓刚因「吸收层放大」修过一轮，这里不重犯。
        let mut region_switched_this_call: HashSet<u64> = HashSet::new();
        // ⭐ L1 换区后**本次请求内生效**的 region（id → region），在建请求时覆盖凭据的
        // `api_region`。
        //
        // 为什么用 per-call 覆盖而不是直接改凭据再重试：换区能不能成功还不知道，
        // 先改再试等于拿一个**未验证的猜测**覆盖掉线上配置 —— 若这次失败是别的原因
        // （限流/上游抖动），号的 region 就被无依据地改坏了。L2 的回写只在**这个区真的
        // 拿到 200 之后**才发生（见成功分支），那时它是**已验证**的事实。
        let mut region_override_this_call: HashMap<u64, String> = HashMap::new();
        // ⭐ 本次客户端请求**已经打过**的号：喂给 acquire_context_excluding,让下一跳
        // 结构性避开它,不再依赖 `cooldownEnabled`(线上它是 false ⇒ failover 事实上不换号,
        // 一个真实 429 被放大成连环 429)。与其它去重集同样声明在 'absorb 循环之外 ⇒
        // 跨吸收轮共享 ⇒ 一条客户端请求内不会反复回头打同一个号。
        // 全池都试过时排除集自动退化成"允许重选"(见 acquire_context_excluding 不变量 1)。
        let mut tried_this_call: HashSet<u64> = HashSet::new();
        // MODEL_TEMPORARILY_UNAVAILABLE 全局容量问题专用计数：只允许 1 次慢速退避重试，
        // 耗尽后立即 break（而非继续烧光 max_retries 切换凭据——所有凭据受同一模型过载影响）。
        let mut model_unavailable_attempts: usize = 0;
        const MAX_MODEL_UNAVAILABLE_RETRIES: usize = 1;
        let api_type = if is_stream { "流式" } else { "非流式" };

        // 一次解析同时取出模型信息与会话标识（conversationId），避免热路径上对
        // 整个请求体做两次全量 serde_json::from_str（大请求体尤其昂贵）。
        let (model, session_id) = Self::extract_model_and_session(request_body);

        // 用量埋点：记录进入调用的时刻与最后服务的凭据/失败分类
        let call_started = std::time::Instant::now();
        let mut last_credential_id: Option<u64> = None;
        let mut last_outcome = crate::usage::RequestOutcome::OtherError;
        // 是否真的发生过 failover(打了 >1 个号)。用于区分「整池换号都失败=真耗尽」与
        // 「首个号就因客户端错误/模型无效 break=不是池的问题」——后者不该计 failover_exhausted。
        let mut real_failover_happened = false;
        // 本次调用实际尝试过的次数（循环外可见，供**失败**记录使用）。
        //
        // 为什么需要它：成功分支用循环变量 `attempt`（见下方 `retries: attempt as u32`），
        // 但 `attempt` 在循环结束后已出作用域，而失败记录是在循环**之后**组装的。
        // 此前 `fail_record` 因此完全没有设 `retries` → 落库即默认 0。
        //
        // 后果（线上实测坐实）：近 2 小时全部失败样本 **无一例外 retries=0**
        // （auth_failed 1487 / rate_limited 1098 / server_error 118 / bad_request 91），
        // 而同期成功样本有 retries=1、历史上号池大时到过 7 以上。
        // 即「烧掉 12 次换号才失败」与「第一次就失败」在面板上完全不可区分 ——
        // 而那恰是最需要看的那类样本（判断重试预算是否够用、吸收层是否有效的唯一依据）。
        let mut attempts_used: u32 = 0;
        // ⭐ **真正打到上游**的次数（跨吸收轮累计），只用来喂 [`round_retry_quota`]。
        //
        // 为什么不能复用 `attempts_used`：后者是 for 循环的**迭代计数**，含两类零上游调用的空转
        // —— ① `acquire_context_excluding` 失败的 fast-fail（全池冷却时 `all_cooling_fast_fail`
        // 默认开，wait>2s 即裸 `continue`，不 sleep 也不打上游）；② endpoint 解析失败。
        //
        // 复用它的后果（本轮修复的缺陷）：`compute_max_retries` 在 pool≥4 时恒为
        // `ABSOLUTE_MAX_TOTAL_RETRIES`=12，于是全池冷却下第 0 轮在**毫秒级**把 12 个额度
        // 全烧在 fast-fail 上 → 轮末 `attempts_base=12` → 额度闸门命中 → `break 'absorb`
        // ⇒ **`absorb_round` 恒 0，吸收层等于没开**。而 PoolCooldown 正是吸收层要拦的主类别，
        // 排在额度闸门之后的截断闸门因此**永远不被求值**（顺序在这里是承重的）。
        //
        // 也不能反过来让 `attempts_used` 只计上游调用：它另有用途（失败记录的
        // `fail_record.retries`，要反映客户端视角的真实换号次数，含 acquire 失败与墙钟 break）。
        // 两个语义必须分成两个变量。
        let mut upstream_calls: u32 = 0;

        // 入站整形准入闸门:**整个客户端请求只过一次**(在 failover 循环外),突发被令牌桶排队削平。
        // review Finding 1 修复:不在 acquire_context 里扣(否则 failover N 跳扣 N 令牌 + fast-fail 空转白扣)。
        //
        // ⚠️ 标记 `inbound_admission_timeout=1` 是**必须**的,不能只靠 `retry_after_secs=`:
        // 它与全池冷却在语义上正好相反 ——
        //   · 全池冷却 = **上游**没准备好,等一会儿真的会好 → 值得重试;
        //   · 准入超时 = **网关自己**在保护上游主动限流(背压),重试只是把同一个请求
        //     再塞回同一个已经满的桶 → 队列更长、客户端等更久,而且拿不到任何额外的成功概率。
        // 两者若共用同一个标记就在字符串上不可区分,任何吸收/重试层都会把网关自己的背压
        // 信号当成"上游稍后会好"去重试(实测形态:2 轮 × 30s = 客户端等 60s 才拿到 429,
        // 而正确行为是 <2s 立刻拿到 429 由客户端自己退避)。
        // 保留 `retry_after_secs=` 是为了让**客户端**仍拿到 429 + Retry-After(那对客户端是对的);
        // 新标记只用于让网关内部的分类器把它判成"不可吸收"。
        if let Err(retry_after) = self.token_manager.acquire_admission().await {
            let err = anyhow::anyhow!(
                "入站限速排队超时(网关目标 {} RPM 保护上游)inbound_admission_timeout=1 retry_after_secs={}",
                self.token_manager.inbound_target_rpm(),
                retry_after
            );
            // ⭐ 可观测性(已知问题 #20):此前这里是裸 `bail!` —— **既不 emit_record 也不 bump
            // 任何计数器** ⇒ 被网关自己背压掐掉的请求在面板上**根本不存在** ⇒ 成功率偏乐观。
            // 而面板成功率是后续一切限流调参的唯一依据,依据有偏则调参全是在算空气。
            //
            // ⚠️ 这里**只加可观测性,绝不加重试**。`acquire_admission()` 在
            // `call_api_with_retry` **内部**、45s 墙钟闸门也在它内部,在此重试会让一条
            // 客户端请求把同一个请求反复塞回同一个已满的桶(队列更长、客户端等更久、
            // 成功概率不增)。正解见 `TASK-BUILTIN-RETRY.md`。
            //
            // outcome 用 `RateLimited`(客户端确实收到 429),但与「上游 429」在两处可区分:
            //   ① `credential_id = None` —— 上游根本没被请求过,没有号可归因;
            //   ② `error_message` 带 `inbound_admission_timeout=1` 字面量。
            // ⚠️ 仅靠 ① 不够:全池冷却的 bail 同样是 `credential_id IS NULL`
            // (面板那条 `sum(credential_id is null)` 查询即因此不可区分)。②才是判据。
            crate::common::recovery_metrics::bump_inbound_admission_timeout();
            let mut record = crate::usage::RequestRecord::new(
                uuid::Uuid::new_v4().to_string(),
                model.clone().unwrap_or_default(),
            );
            record.session_id = session_id.clone();
            record.is_streaming = is_stream;
            record.latency_ms = call_started.elapsed().as_millis() as u64;
            record.outcome = crate::usage::RequestOutcome::RateLimited;
            record.error_message = Some(err.to_string());
            crate::usage::emit_record(record);
            return Err(err);
        }

        // ── 内置「上游 429 吸收层」──────────────────────────────────────────────
        // 位置是承重的:**必须在上面那道 acquire_admission 之下、failover 循环之外**。
        // 入站令牌是「每客户端请求一个」,若把吸收放在闸门之上(如包在 handler 外层),
        // 一条客户端请求会吃 N 个令牌 → 300 并发 × N 轮 vs 一个按号数算出的 RPM 桶 =
        // 桶恒空 → 每轮排队满 30s 才 bail → 客户端从 <2s 拿到 429 变成 60s 才拿到。
        // 那是外置 shield 的 p50 73.2s 被搬进网关(设计评审 BLOCKER 1)。下沉后这条路
        // 结构上不存在:闸门在循环上方,物理上不可能被重入,不依赖调用方传任何 flag。
        let absorb = AbsorbPolicy::from_config(&self.token_manager.config());
        // deadline 与 call_started 同源:准入排队(最长 inbound_queue_max_wait_secs)也计入
        // 预算。若改成从此刻起算,客户端可见延迟 = 排队 30s + 吸收 45s = 75s ≈ shield 的
        // p50 73.2s,等于把病根换个地方搬进来。
        let absorb_deadline = call_started + absorb.budget;
        // 本轮生效的 deadline。默认等于总预算那个；只有在**上一轮末尾**判定为换号空窗且
        // 该类设了独立预算时，才在 sleep 处换成它自己那份（`class_deadline`）。
        //
        // 为什么要用一个可变量而不是直接用 `class_deadline`：类别只有在一轮**跑完**、
        // 拿到 `last_error` 之后才知道，而 `round_budget` 在进轮时就要用 deadline。
        // 逐轮记录「本轮是被哪一类触发的」就把两者对齐了，且不会让某一类的宽预算
        // 泄漏给下一轮的其它类别（下一轮若是别的类，这里会被改回 `absorb_deadline`）。
        let mut round_deadline = absorb_deadline;
        // 吸收层跑过至少一轮却仍放弃 ⇒ 终态状态码可按配置换成 503（见
        // `ABSORB_BUDGET_EXHAUSTED_MARKER`）。只在真睡过退避、真重打过的情形置位：
        // 一次都没重试就改状态码是**说谎**（网关没尽力，却告诉客户端「我们暂时不可用」）。
        let mut absorb_gave_up_after_rounds = false;
        let mut absorb_round: u32 = 0;
        // 跨轮累计的尝试数(喂 attempts_used)。声明在 'absorb 之外,故失败记录里的 retries
        // 是整条客户端请求的真实总换号数,而不是最后一轮的局部计数。
        let mut attempts_base: u32 = 0;

        // ⚠️ 所有「链内去重集」(rate_limited_this_call / suspended_this_call /
        // suspicious_failovers_this_call / auth_failed_this_call / region_corrected_this_call
        // / model_unavailable_attempts) 都声明在本循环**之外** ⇒ 跨吸收轮共享 ⇒ 同一个号在
        // 整条客户端请求内只被惩罚一次。若把它们挪进轮内,同号会被反复罚 → trigger_count 累加
        // → 冷却 15s 指数拉长到 72s,那正是「单请求自造雪崩」的成因。本方案的第二条承重不变量。
        'absorb: loop {
            let round_started = std::time::Instant::now();
            // 关闭时两者恒等于旧值(round_clock == call_started、round_budget == 完整 45s),
            // 故墙钟闸门的判据与旧代码逐字节相同。见 docs/absorb-layer-design.md §8。
            let round_clock = if absorb.enabled {
                round_started
            } else {
                call_started
            };
            // 用 `round_deadline`（而非固定的 `absorb_deadline`）：换号空窗设了独立预算时，
            // 由它触发的那一轮才拿得到那份更宽的墙钟。第 0 轮两者恒相等 ⇒ 旧行为不变。
            let round_budget = absorb.round_budget(round_deadline, round_started);
            // ⭐ 未修问题 ②：本轮实际配额 = min(基础配额, 跨轮总额度剩余)。
            // 声明在轮**内**（与去重集相反）是刻意的：它是每轮重算的**派生量**，
            // 而它依赖的累计量 `upstream_calls` 在轮外 ⇒ 上限回到「每请求」语义。
            // 关闭吸收层时 upstream_calls 只在唯一一轮内增长、且这里只在进轮时读一次
            // ⇒ 恒等于 base_retry_quota（本身已 ≤ ABSOLUTE_MAX_TOTAL_RETRIES）⇒ 逐字节等价旧行为。
            //
            // ⚠️ 喂的是 `upstream_calls` 而**不是** `attempts_base`：后者含 fast-fail 空转,
            // 会让全池冷却在毫秒内烧空额度、把吸收层整体旁路掉(见其声明处的长注释)。
            // 「每请求 ≤ 12 次上游调用」这个不变量仍然成立:进轮时 quota ≤ 12 − upstream_calls,
            // 而本轮内最多再打 quota 次 ⇒ 轮末 upstream_calls ≤ 12。
            let max_retries = round_retry_quota(base_retry_quota, upstream_calls);

            for attempt in 0..max_retries {
                // 与成功分支的 `retries: attempt as u32` 同口径：记「已尝试次数 - 1」＝重试次数。
                // 放在墙钟闸门**之前**递增：闸门 break 时也要反映"这一轮进来过"，
                // 否则墙钟耗尽的失败会少记一次，而那正是要观测的形态。
                attempts_used = attempts_base + attempt as u32;
                // 墙钟闸门：单请求重试总时长超预算就停止（把最后错误透传给客户端，
                // 让它自己退避）。防止一个卡住的请求在小号池里反复扫冷全池、把偶发 429
                // 拖成持续雪崩。首次尝试(attempt==0)不受此限，保证至少打一次。
                //
                // 吸收层开启时 round_clock/round_budget 变成「本轮起点 / min(45s, 剩余预算)」：
                // 一轮的墙钟上限被剩余总预算夹住,这就是吸收轮次不会超预算的机制本身。
                if attempt > 0 && round_clock.elapsed() >= round_budget {
                    tracing::warn!(
                        "单请求重试已达墙钟预算 {:?}（尝试 {}/{}，吸收轮次 {}），停止重试并透传上游错误，避免拖垮整池",
                        round_budget,
                        attempt,
                        max_retries,
                        absorb_round
                    );
                    break;
                }
                // 获取调用上下文（绑定 index、credentials、token）
                //
                // ⭐ 传入 `tried_this_call`：本请求已试过的号在下一跳被**结构性**排除，
                // 不再依赖 `cooldownEnabled`。此前 failover 能否真的换号完全取决于那个开关
                // （`is_entry_selectable` 里的冷却硬门是唯一排除机制），线上它是 false ⇒
                // 一个真实 429 被放大成连环 429。全池都试过时排除集自动退化（允许重选），
                // 见 `acquire_context_excluding` 的不变量 1。
                let ctx = match self
                    .token_manager
                    .acquire_context_excluding(
                        model.as_deref(),
                        session_id.as_deref(),
                        &tried_this_call,
                    )
                    .await
                {
                    Ok(c) => c,
                    Err(e) => {
                        // 全池冷却快速失败(带 retry_after_secs / "冷却")归类为 RateLimited,
                        // 用量明细显示"限流"而非扎眼的"其它错误"(dwgx:那些其它错误 0/0 很恶心)。
                        let es = e.to_string();
                        if es.contains("retry_after_secs=") || es.contains("冷却") {
                            last_outcome = crate::usage::RequestOutcome::RateLimited;
                        }
                        last_error = Some(e);
                        continue;
                    }
                };

                // 可观测:attempt>0 且真拿到了一个号 = 一次 failover 换号(真打了下一个号)。
                // 放在 acquire_context 成功之后,避免全池冷却 continue(没拿到号)误计一跳。
                if attempt > 0 {
                    crate::common::recovery_metrics::bump_failover_hop();
                    real_failover_happened = true;
                }

                // 记入「本请求已试过」：下一跳 acquire_context_excluding 会优先避开它。
                // 必须在真正拿到号之后、发请求之前记 —— 记在发请求之后的话，一条在 send()
                // 处失败（网络错误 continue）的路径就不会被记入，下一跳又选它。
                tried_this_call.insert(ctx.id);

                let config = self.token_manager.config();

                // ⭐ L1：本请求链内该号已被判定 region 错配 ⇒ 用换过的区建本次请求。
                //
                // 只在**真有覆盖**时才 clone 凭据：热路径上 99.99% 的请求走 `Borrowed`
                // 分支，零额外拷贝（`acquire_context` 已经 clone 过一次，再无条件多一次
                // 就是给每个正常请求加成本去伺候一个极少数的纠错路径）。
                let call_creds: std::borrow::Cow<'_, KiroCredentials> =
                    match region_override_this_call.get(&ctx.id) {
                        Some(region) => {
                            let mut c = ctx.credentials.clone();
                            c.api_region = Some(region.clone());
                            std::borrow::Cow::Owned(c)
                        }
                        None => std::borrow::Cow::Borrowed(&ctx.credentials),
                    };

                let machine_id = machine_id::generate_from_credentials(&call_creds, &config);

                let endpoint = match self.select_endpoint(&call_creds, ctx.id) {
                    Some(e) => e,
                    None => {
                        last_error = Some(anyhow::anyhow!(
                            "凭据 #{} 所有端点桶均处于 429 封禁期",
                            ctx.id
                        ));
                        // ⚠️ 不得 report_failure：None 代表**端点桶 30s 封禁**（瞬态），不是未知
                        // 端点配置错误。report_failure 会累计 failure_count → TooManyFailures
                        // 永久禁用健康号。设 30s 短冷却让调度避开，等桶解封。
                        if rate_limited_this_call.insert(ctx.id) {
                            self.token_manager.report_rate_limited_with_retry_after(
                                ctx.id,
                                Some(ENDPOINT_BUCKET_THROTTLE.as_secs()),
                            );
                        }
                        continue;
                    }
                };

                let rctx = RequestContext {
                    credentials: &call_creds,
                    token: &ctx.token,
                    machine_id: &machine_id,
                    config: &config,
                    is_1m,
                };

                let url = endpoint.api_url(&rctx);
                let body = endpoint.transform_api_body(request_body, &rctx);

                let base = self
                    .client_for(&ctx.credentials)?
                    .post(&url)
                    .body(body)
                    .header("content-type", endpoint.content_type());
                let request = endpoint.decorate_api(base, &rctx);

                last_credential_id = Some(ctx.id);

                // ⭐ 全局上游并发闸：限制**同时在飞**的上游 HTTP 调用数（防放大）。
                //
                // 拿 `OwnedSemaphorePermit` 跨 `send().await` 存活、响应头拿到后离开本
                // 作用域自动 Drop 释放 —— 免费防泄漏。**不用 `acquire().await`**（无限等待
                // 会把客户端延迟堆到秒级，与 gate 满时"系统已饱和"的语义矛盾）：
                // `try_acquire_owned` 拿不到就 **break 本轮重试**（而非 continue 无 sleep 空转），
                // 把错误透传给客户端让它自己退避。
                //
                // ⚠️ 不递增 `upstream_calls`：闸门挡住的是"根本没发出去"的调用，不该占用
                // 「每请求 ≤12 次上游调用」的额度 —— 该不变量（含吸收层、墙钟闸门、
                // round_retry_quota）全部不受影响。
                let _gate = match self.upstream_gate.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        tracing::warn!(
                            "上游并发闸已满，本轮重试 break 以免放大（尝试 {}/{}）",
                            attempt + 1,
                            max_retries
                        );
                        // ⚠️ 只在还没有更具体错误时设置 gate-full 错误：链内若先有可吸收的
                        // 429（带 retry_after_secs），覆盖它会把这轮错误判成"不可吸收"而旁路
                        // 吸收层。`last_error` 已有值时保留原错误，仅 break 本轮。
                        if last_error.is_none() {
                            // 带 `upstream_gate_full=1` + `retry_after_secs` 供 handlers 的
                            // map_provider_error 识别成 429 + Retry-After（让客户端退避，
                            // 而不是 502 让客户端立即重发、重新灌满闸门）。
                            last_error = Some(anyhow::anyhow!(
                                "上游并发闸已满，停止本轮重试以免放大 upstream_gate_full=1 retry_after_secs=2"
                            ));
                            last_outcome = crate::usage::RequestOutcome::RateLimited;
                        }
                        break;
                    }
                };

                let send_result = request.send().await;
                // ⭐ 额度只在这里累加:此刻请求**已经发出去了**(无论上游怎么回、哪怕连接失败),
                // 才算真花掉一次「打上游」的机会。放在 send 之后而非循环顶部是本修复的全部内容。
                //
                // 网络错误(`Err`)也计:它同样占了一次出站连接 + 一次退避 sleep,不计会让
                // 「上游整体不可达」变成额度永不递减的死磨(每轮都拿满配额重打)。
                upstream_calls += 1;
                let response = match send_result {
                    Ok(resp) => resp,
                    Err(e) => {
                        tracing::warn!(
                            "API 请求发送失败（尝试 {}/{}）: {}",
                            attempt + 1,
                            max_retries,
                            e
                        );
                        // 网络错误通常是上游/链路瞬态问题，不应导致"禁用凭据"或"切换凭据"
                        // （否则一段时间网络抖动会把所有凭据都误禁用，需要重启才能恢复）
                        last_error = Some(e.into());
                        last_outcome = crate::usage::RequestOutcome::NetworkError;
                        if attempt + 1 < max_retries {
                            sleep(Self::retry_delay(attempt)).await;
                        }
                        continue;
                    }
                };

                let status = response.status();

                // 喂动态降档信号：**每个**上游响应都记一次（成功/4xx false，429/5xx true），
                // 供 base_retry_quota 处的 apply_retry_pressure 收缩重试预算。
                // 与 AIMD 的 report_upstream_rate_limited 是两套独立机制、两套门控，勿混。
                //
                // ⚠️ 5xx 必须也算压力（true）：纯 500 风暴同样是「疯狂重试」的来源，
                // 若只计 429，5xx 落进"成功"桶会把 rate() 稀释到趋近 0 → 降档永不触发。
                // 4xx（客户端错误）不算压力：它是请求本身的问题，不是上游过载信号。
                let code = status.as_u16();
                self.retry_pressure
                    .lock()
                    .record(code == 429 || code >= 500);

                // 成功响应
                if status.is_success() {
                    self.token_manager.report_success(ctx.id);

                    // ⭐ L2：换区**成功后**立刻把这个区回写进 `api_region` 并持久化。
                    //
                    // 时机是承重的：只有走到这里，那个区才从「猜测」变成**已验证事实**
                    // （这个号在这个区真拿到了 200）。回写早于此就是拿未验证的猜测覆盖配置。
                    //
                    // ⇒ 第一次自我纠正之后就写死，后续请求零额外开销。这比「每次都试两个区」
                    // 的无状态做法省掉一次往返，也不再依赖任何外部脚本预先喂 region。
                    //
                    // 只对 `api_key` 号：OAuth 号的权威 region 是 `profileArn`
                    // （`effective_upstream_region` 第一优先），回写 `api_region` 对它**不生效**，
                    // 只会在面板上留一个看起来生效其实被压住的值，把排障带偏。
                    // （`region_retry_target` 已在入口拦掉非 api_key，这里是第二道 —— 判据
                    //   两处都写是刻意的：将来若有人放宽入口那道门，这里仍不会写坏 OAuth 号。）
                    if let Some(region) = region_override_this_call.get(&ctx.id) {
                        if ctx.credentials.is_api_key_credential() {
                            // ⚠️ 回写失败**绝不让请求失败**：本次请求已经用新区成功了，
                            // 回写只是让下次省一跳。把它变成硬失败等于用一个纯优化项
                            // 去否掉一个已经成功的响应。
                            if let Err(e) = self
                                .token_manager
                                .set_credential_api_region(ctx.id, Some(region.clone()))
                            {
                                tracing::warn!(
                                    "凭据 #{} 换区成功但回写 api_region={} 失败（本次请求不受影响，\
                                     下次仍需重新换区一次）: {}",
                                    ctx.id,
                                    region,
                                    e
                                );
                            } else {
                                tracing::info!(
                                    "凭据 #{} region 自纠正完成：api_region 已写死为 {}（后续请求零额外开销）",
                                    ctx.id,
                                    region
                                );
                            }
                        }
                    }
                    // 可观测:吸收层真把一个本该回给客户端的 429 救回来了(客户端全程未见 429)。
                    // 只在 absorb_round > 0 时计,否则每个正常成功请求都会被记成"吸收成功"。
                    if absorb_round > 0 {
                        crate::common::recovery_metrics::bump_absorb_recovered();
                        tracing::info!(rounds = absorb_round, "吸收层重试成功，客户端未见 429");
                    }
                    let meta = CallMeta {
                        credential_id: ctx.id,
                        model: model.clone(),
                        session_id: session_id.clone(),
                        is_streaming: is_stream,
                        // 跨吸收轮累计:客户端视角的一条请求总共换了多少次号。
                        retries: attempts_base + attempt as u32,
                        latency_ms: call_started.elapsed().as_millis() as u64,
                        started_at: call_started,
                        // 移交在途守卫：从此随响应流存活，流真正消费完才 -1
                        inflight: ctx.inflight,
                    };
                    return Ok((response, meta));
                }

                // 失败响应：先从响应头提取 Retry-After（body 消费后头就没了），再读取 body
                let retry_after_header = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<u64>().ok());
                let body = response.text().await.unwrap_or_default();

                // 客户端请求校验错误（如 TOOL_USE_RESULT_MISMATCH / TOOL_SCHEMA_INVALID）：请求构造问题，
                // 换号/重试都只会重复失败并浪费配额，立即终止（不计凭据失败）。
                // `is_client_validation_error` 覆盖 TOOL_USE_RESULT_MISMATCH；TOOL_SCHEMA_INVALID
                // 是同一语义（客户端工具 schema 非法，非上游故障）的另一 reason（ZyphrZero/kiro.rs
                // endpoint/mod.rs 的 CLIENT_VALIDATION_REASONS 两者都收），此处补认。
                if endpoint.is_client_validation_error(&body)
                    || body.contains("TOOL_SCHEMA_INVALID")
                {
                    tracing::warn!(
                        "API 请求失败（客户端请求校验错误，不重试）: {} {}",
                        status,
                        body
                    );
                    last_outcome = crate::usage::RequestOutcome::BadRequest;
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败（请求校验错误）: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    break;
                }

                // 账户级临时风控限速（suspicious activity + temporary limits）：
                // ⚠️ 必须在 is_account_suspended 之前判定，否则含 "suspended...suspicious
                // activity" 的临时限速文案会被误判成永久封禁，白冻一个还能用的号 24h。
                // 处置：只设短冷却 + 立即 failover，不禁用、不计永久失败。
                if endpoint.is_temporary_rate_limit(&body) {
                    tracing::warn!(
                        "API 请求失败（账户临时风控限速，非永久封禁；短冷却后 failover，尝试 {}/{}）: {} {}",
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );
                    last_outcome = crate::usage::RequestOutcome::RateLimited;
                    // 账户级风控也是上游限速信号 → 入站整形 RPM 自动降档。
                    // 只在第 0 轮上报(见本文件 'absorb 循环处的 AIMD 放大说明)。
                    if absorb_round == 0 {
                        self.token_manager.report_upstream_rate_limited();
                    }
                    // 账户级可疑活动风控：走分钟级退避（report_suspicious_activity），而非普通
                    // 429 的 15s 瞬时冷却。本请求链内该号首次触发才设冷却；再次触发只 failover，
                    // 不重复惩罚（同 rate_limited_this_call 去重，避免一条链把号砸进更深风控）。
                    if rate_limited_this_call.insert(ctx.id) {
                        self.token_manager.report_suspicious_activity(ctx.id);
                    } else {
                        tracing::debug!(
                            "凭据 #{} 本请求链内已因风控冷却过，再次触发仅 failover，不重复惩罚",
                            ctx.id
                        );
                    }
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败（账户级可疑活动风控，分钟级退避）: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    // 跨号转移上限：超过即停止遍历，把错误透传给客户端自行退避。
                    // 不设上限就会线性扫全池（实测 43 号 → 尝试 43/43 → 45s 墙钟），
                    // 既让用户干等，又把整池号一起送进上游风控。
                    suspicious_failovers_this_call += 1;
                    if suspicious_failovers_this_call >= MAX_SUSPICIOUS_FAILOVERS_PER_CALL {
                        tracing::error!(
                            "本次请求已因账户级风控转移 {} 次号，停止遍历号池并透传错误\
                         （避免扫冷全池 + 同出口 IP 连续触发风控）",
                            suspicious_failovers_this_call
                        );
                        break;
                    }
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }

                // 注：524 网关超时（Cloudflare 等）落入下方通用 5xx 分支即按可重试瞬态
                // 错误处理（不禁用、退避后换号），无需单列——与通用路径行为一致。

                // 402 Payment Required 且额度用尽：禁用凭据并故障转移
                // 🔴 **刻意不门控状态码** —— 只认 body 里的额度信号。
                //
                // 旧代码是 `status == 402 && is_monthly_request_limit(&body)`，而线上实测
                // （2026-08-05，6 小时窗口）：
                //   · `402 Payment Required` 出现 **0 次**
                //   · `400 Bad Request` + `"reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"` 出现 **564 次**
                // ⇒ 那道 402 门**从不成立** ⇒ 564 个「额度已耗尽」的请求全部落到下方通用
                // 400 分支 `break` 掉，凭据**不被禁用、继续留在轮转里**，每个新请求都再撞一次。
                // 实测 #508 一个号就吃了 543 次。这正是「大量 400 没有自动禁用」的成因。
                //
                // 为什么改成只看 body：额度耗尽是**账号级终态**，上游用哪个状态码表达它是
                // 上游的自由（它已经从 402 改到 400 了）。而 `is_monthly_request_limit`
                // 的判据是 `MONTHLY_REQUEST_COUNT` / `OVERAGE_REQUEST_LIMIT_EXCEEDED`
                // 两个**明确的 reason 字面量**（`endpoint/mod.rs:235`），本身已经足够窄 ——
                // 用它当唯一判据比再叠一个会漂的状态码更稳。
                //
                // ⚠️ 位置必须在通用 400 分支**之前**（本分支现在就在那之前）；挪到之后即失效。
                if endpoint.is_monthly_request_limit(&body) {
                    tracing::warn!(
                        "API 请求失败（额度已用尽，禁用凭据并切换，尝试 {}/{}）: {} {}",
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );

                    last_outcome = crate::usage::RequestOutcome::QuotaExhausted;
                    let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                    if !has_available {
                        last_error = Some(anyhow::anyhow!(
                            "{} API 请求失败（所有凭据已用尽）: {} {}",
                            api_type,
                            status,
                            body
                        ));
                        break;
                    }
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    continue;
                }

                // 账户被暂停/封禁：不论状态码，body 命中 suspend 信号即直接禁用并转移
                // （不可自动恢复，等待人工处理，避免反复打已封的号）
                if endpoint.is_account_suspended(&body) {
                    tracing::error!(
                        "API 请求失败（账户被暂停/封禁，禁用凭据并切换，尝试 {}/{}）: {} {}",
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );
                    last_outcome = crate::usage::RequestOutcome::AccountSuspended;
                    // suspend 是账号级风控信号：同样让入站 AIMD 降档，否则网关会继续按原速率
                    // 往正在拒绝我们的上游灌流量，把风控进一步激化（此前 AIMD 只认 429）。
                    // 只在第 0 轮上报(见本文件 'absorb 循环处的 AIMD 放大说明)。
                    if absorb_round == 0 {
                        self.token_manager.report_upstream_pressure();
                    }
                    let has_available = self.token_manager.report_account_suspended(ctx.id);
                    if !has_available {
                        last_error = Some(anyhow::anyhow!(
                            "{} API 请求失败（账户被封禁且所有凭据已用尽）: {} {}",
                            api_type,
                            status,
                            body
                        ));
                        break;
                    }
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败（账户被暂停）: {} {}",
                        api_type,
                        status,
                        body
                    ));

                    // ⚠️ 每请求最多因 suspend 转移**一次**，且转移前退避。
                    //
                    // 此前这里是裸 `continue`（无 sleep、无冷却，而 report_account_suspended
                    // 也不设冷却），于是一条客户端请求会在几秒内用 8~12 个不同账号打同一端点、
                    // 同一出口 IP —— 日志里的「尝试 8/36」就是第 8 个号被烧。这正是风控要抓的
                    // 突发特征：我们在放大自己的封禁（实测 12 小时 88 次 suspend 禁用）。
                    //
                    // 限一次的理由：suspend 是**账号级**信号，多半伴随同出口 IP 的整体风控。
                    // 既然第一个号已被判定，继续遍历全池极可能把剩下的号一起烧掉，而本次请求
                    // 成功率并不会因此提高。宁可这一条请求失败，也不要赔掉整个号池。
                    if suspended_this_call {
                        tracing::error!(
                            "本次请求已因账户暂停转移过一次，不再遍历号池（避免同 IP 连续触发风控）"
                        );
                        break;
                    }
                    suspended_this_call = true;
                    tokio::time::sleep(Self::retry_delay(attempt)).await;
                    continue;
                }

                // 400 INVALID_MODEL_ID：该号已不能服务请求的模型（多为订阅取消/降级）。
                // 不是客户端请求错误——换个订阅仍有效的号往往能成功。故给该号冷却 + failover，
                // 而非直接把 400 透传（那样坏号还留在轮转里，下个请求又命中它）。
                // 只有当所有号都返回它（report 返回 has_available=false）时，才是模型本身无效、透传。
                if status.as_u16() == 400 && endpoint.is_invalid_model_id(&body) {
                    last_outcome = crate::usage::RequestOutcome::BadRequest;
                    // 模型级处置：只把"该号+该模型"记进短期黑名单并 failover 到对此模型仍可用的号；
                    // 绝不冷却/禁用整个号（该号对其它模型照常可用）。返回 false = 所有未禁用号都已对
                    // 此模型进黑名单 → 说明是模型本身无效，透传真 400 给客户端(而非 429/502 死循环)。
                    let has_available_for_model = self
                        .token_manager
                        .report_model_invalid(ctx.id, model.as_deref());
                    if !has_available_for_model {
                        last_error = Some(anyhow::anyhow!(
                            "{} API 请求失败（模型 {:?} 对所有号均 INVALID_MODEL_ID，判定模型无效）: {} {}",
                            api_type,
                            model.as_deref().unwrap_or(""),
                            status,
                            body
                        ));
                        // 透传真实 400：这是客户端请求了一个所有号都不支持的模型，重试无意义。
                        break;
                    }
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败（凭据 #{} 对模型 {:?} INVALID_MODEL_ID，切换到仍支持的号）: {} {}",
                        api_type,
                        ctx.id,
                        model.as_deref().unwrap_or(""),
                        status,
                        body
                    ));
                    continue;
                }

                // ⭐ 400 + 模型容量不足 —— **必须排在下面那条通用 400 之前**。
                //
                // 上游对「模型没容量」发过两种形态：503 `MODEL_TEMPORARILY_UNAVAILABLE`，
                // 以及 400 `ThrottlingException` + `reason:INSUFFICIENT_MODEL_CAPACITY`。
                // 后者的 HTTP 状态是 400，于是会被下面那条通用 400 分支**先接住并 break**，
                // 而真正的容量处置（慢速退避 + 不惩罚凭据健康）在本函数更后面（约 :1588）
                // ——**永远走不到**。
                //
                // 实测坐实这个顺序缺陷：修复上线后（19:05:15）逐分钟仍全部落 `bad_request`
                // （19:19 / 19:21 / …… / 19:45），近 6h 共 590 次。而当时 endpoint 判据、
                // provider 状态门、handlers 映射三处都已改对、四条测试全绿 —— 因为那些测试
                // 测的是纯函数与 `include_str!` 状态门守卫，**没有一条走 provider 的真实分支链**，
                // 所以顺序错误对它们完全不可见。
                //
                // 这里只做「转交」：不复制那套处置逻辑（复制必然漂移），而是让它落到下方
                // 统一的容量分支。用 `continue` 之外的方式表达"别被通用 400 吃掉"。
                let is_capacity_400 =
                    status.as_u16() == 400 && endpoint.is_model_temporarily_unavailable(&body);

                // 400 Bad Request - 其它请求问题（客户端构造错误），重试/切换凭据无意义
                if status.as_u16() == 400 && !is_capacity_400 {
                    last_outcome = crate::usage::RequestOutcome::BadRequest;
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    break;
                }

                // 401/403 - 更可能是凭据/权限问题：计入失败并允许故障转移
                if matches!(status.as_u16(), 401 | 403) {
                    tracing::warn!(
                        "API 请求失败（可能为凭据错误，尝试 {}/{}）: {} {}",
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );

                    // region 自动纠正一条龙:403 FEATURE_NOT_SUPPORTED = 该 region 的 profile 未开通。
                    // 这**不是**凭据坏(号本身好、只是 region 配错),绝不当普通 401/403 冷却 + 换号误伤它。
                    // 处置(对抗复核裁决:昂贵 reprobe 绝不上同步对话热路径):
                    //   ① 廉价本地纠正 sync_region_from_arn(纯字符串,无网络)——修"region 字段与 ARN 漂移";
                    //   ② 置 flag + 触发 per-id 守卫的**后台异步**重探(不阻塞本请求,为后续请求恢复);
                    //   ③ 仅当本地纠正真改了 region 且本链未纠正过 → continue 重试一次(不 report_failure);
                    //   否则落下方 report_failure + failover(本请求换号,重探已在后台启动)。
                    // 非 external_idp 号(social/idc)第二条件即短路,行为逐字不变。
                    if status.as_u16() == 403
                        && endpoint.is_feature_not_supported(&body)
                        && ctx.credentials.is_external_idp_credential()
                    {
                        let corrected = self.token_manager.sync_region_from_arn_for(ctx.id);
                        self.token_manager
                            .mark_usage_403_feature_not_supported(ctx.id);
                        self.token_manager.trigger_background_reprobe(ctx.id);
                        if corrected
                            && region_corrected_this_call.insert(ctx.id)
                            && call_started.elapsed()
                                < std::time::Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS)
                        {
                            tracing::info!(
                                "凭据 #{} 403 FEATURE_NOT_SUPPORTED:已本地纠正 region,同号重试一次(不冷却)",
                                ctx.id
                            );
                            last_outcome = crate::usage::RequestOutcome::ServerError;
                            last_error = Some(anyhow::anyhow!(
                                "{} 403 FEATURE_NOT_SUPPORTED(已本地纠正 region 重试): {} {}",
                                api_type,
                                status,
                                body
                            ));
                            // continue → 下一轮 acquire_context 重克隆已改好 region 的 creds(不复用旧 ctx/url)。
                            continue;
                        }
                        // 本地纠不动(ARN region 本身就是未开通那个,常见)→ failover 换号服务本请求,
                        // 后台异步重探已启动为该号后续请求恢复。给该号一段**认证冷却**(临时跳过、非禁用、
                        // 不累计失败),让调度本链内避开它、别反复选回来空撞 403;冷却到期或后台重探成功后
                        // 自动恢复。绝不 report_failure 连坐(region 配错≠号坏,隔离铁律)。
                        tracing::info!(
                            "凭据 #{} 403 FEATURE_NOT_SUPPORTED:本地纠正无效,冷却+failover 换号(后台重探已启动)",
                            ctx.id
                        );
                        last_outcome = crate::usage::RequestOutcome::ServerError;
                        // ⭐ 必须是**瞬态**冷却：上面三行刚 `trigger_background_reprobe`,
                        // 这条路径的全部设计前提就是「后台重探会把 region 修对,该号随后自愈」
                        // （见上方注释「冷却到期或后台重探成功后自动恢复」）。
                        // 而 `report_auth_cooldown` 落的 `AuthenticationFailed`
                        // `is_auto_recoverable=false` ⇒ 实际是 86400s 硬窗 ——
                        // 注释承诺的自愈**永远不会发生**,重探成功了号也回不了池。
                        // `AuthTransient` 的 20s 基线正好覆盖一次重探往返;若重探更慢,
                        // 该号回池再撞一次 403 只是让 1.3^n 递增(上限 90s)、不计失败。
                        self.token_manager.report_auth_transient_cooldown(ctx.id);
                        last_error = Some(anyhow::anyhow!(
                            "{} 403 FEATURE_NOT_SUPPORTED(region 未开通,冷却换号,后台重探中): {} {}",
                            api_type,
                            status,
                            body
                        ));
                        // continue:下一轮 acquire_context 选别的号;全池不可用时由 max_retries/墙钟兜底透传。
                        continue;
                    }

                    // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会。
                    // ⚠️ api_key 号跳过 —— 理由与对话路径同处的长注释一致（结构上不可能成功，
                    // 且失败会计入失败 + 落冷却 + 被瞬态判据重试 3 次，把死亡速度放大三倍）。
                    if endpoint.is_bearer_token_invalid(&body)
                        && !force_refreshed.contains(&ctx.id)
                        && !ctx.credentials.is_api_key_credential()
                    {
                        force_refreshed.insert(ctx.id);
                        tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                        if self
                            .token_manager
                            .force_refresh_token_for(ctx.id)
                            .await
                            .is_ok()
                        {
                            tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                            continue;
                        }
                        tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                        // 刷新失败 = 认证态有问题，加一段冷却让调度避开它。
                        // 时长按「该号是否被证明过」二分 —— 理由与 MCP 路径同处逐字同款
                        // （刷新层已内部重试过瞬态错误，故到这里的抖动不该换来 24h 硬冻；
                        // 但从未成功过的号刷新还失败 = refreshToken 大概率真废了）。
                        if self.token_manager.has_ever_succeeded(ctx.id) {
                            self.token_manager.report_auth_transient_cooldown(ctx.id);
                        } else {
                            self.token_manager.report_auth_cooldown(ctx.id);
                        }
                    }

                    last_outcome = crate::usage::RequestOutcome::AuthFailed;

                    // 🔴 `bearer token invalid` 打在**已经成功过**的号上 = 瞬态，不计失败。
                    //
                    // 同一句上游文案含义相反：
                    // - 从未成功过 → 大概率 region 错配（`ksk_` 按 region 授权，打错区恒 403），
                    //   该计失败、该被禁用（实测 3 个从未成功的号共吃 17 次，那是真错配）。
                    // - 已经成功过 → token 对该端点**证明有效**，403 只能是抖动
                    //   （实测 4 个成功过的号累计 3393 次成功、共吃 42 次这种 403）。
                    //
                    // 为什么 `failure_count` 的「连续」语义兜不住：`report_success` 确实归零它，
                    // 但那要求成功**先落地**。高并发下同一秒内成功与失败交错（实测单号 60+ RPM），
                    // 三个并发请求各自 +1 就到阈值，中间没有成功插进来。实测 #481：2412 次成功、
                    // 93.9% 成功率，仍在 1 秒内被 3 次瞬态 403 推到 `TooManyFailures`
                    // → 池子少一个号 → 剩下的吃更多流量 → 更容易撞惩罚窗口。
                    // 当天全池 116 次禁用 / 42 次自愈，池子一直在抖。
                    //
                    // 处置与 `is_temporary_rate_limit` 同款：设短冷却让调度避开它 + failover，
                    // **不** `report_failure`。冷却会自动恢复，真错配的号（从未成功）不受影响。
                    let bearer_invalid_but_proven = endpoint.is_bearer_token_invalid(&body)
                        && self.token_manager.has_ever_succeeded(ctx.id);
                    if bearer_invalid_but_proven {
                        tracing::warn!(
                            "凭据 #{} 收到 bearer-invalid 403，但它已成功过 ⇒ 判为瞬态：\
                         只设短冷却 + failover，不计失败（防高并发下 3 次抖动把健康号打死）",
                            ctx.id
                        );
                        if auth_failed_this_call.insert(ctx.id) {
                            // ⭐ 上面那句 warn 自称「只设短冷却」，而 `report_auth_cooldown`
                            // 落的 `AuthenticationFailed` 实际是 24h 硬窗
                            // （`is_auto_recoverable=false` ⇒ long_cooldown 86400s）——
                            // 注释与实现分叉，且分叉的方向恰好抵消了本分支存在的意义：
                            // 本分支的全部目的就是「别把已证明健康的号（实测 #481：2412 次
                            // 成功、93.9% 成功率）因几次抖动打死」，落 24h 只是把
                            // 「被禁用」换成「更难发现的冷却僵尸」。
                            // `bearer_invalid_but_proven` 已含 `has_ever_succeeded`，
                            // 正是 `AuthTransient` 的判据，这里无需再判。
                            self.token_manager.report_auth_transient_cooldown(ctx.id);
                        }
                        // ⭐ 机器可读标记 `bearer_invalid_transient=1`（同款范式:
                        // `pool_permanently_exhausted=1` / `model_unsupported_by_pool=1` /
                        // `inbound_admission_timeout=1`）。中文文案保留给人读。
                        //
                        // 为什么必须有:上面这个二分（`has_ever_succeeded`）是**只有这里**才做得出的
                        // 判断 —— handler 层拿到的只有一个错误字符串,而 region 错配与瞬态抖动
                        // 在上游文案上**逐字节相同**（都是那句 bearer-invalid + 403）。
                        // 于是 `is_upstream_region_mismatch_403` 会把这条已证明健康的号也判成
                        // region 坏:① 给出错误的排障方向（去改 region,而号本来就是对的）;
                        // ② 状态码从 502（在外挂 kiro_shield 的 RETRYABLE 集内、会重试）变成
                        // 403（4xx 不重试）⇒ 一次纯抖动被固化成客户端可见的硬失败。
                        //
                        // ⚠️ 字面量逐字节承重:handlers 侧按它做排除。改名/改大小写/加空格都会
                        // 让那条排除静默失效（回到误判），且编译不报错。
                        last_error = Some(anyhow::anyhow!(
                            "{} API 请求失败（token 瞬态失效，已冷却换号）bearer_invalid_transient=1: {} {}",
                            api_type,
                            status,
                            body
                        ));
                        continue;
                    }

                    // ⭐ L1：**从未成功过**的号吃 bearer-invalid 403 ⇒ 判 region 错配，换区重试。
                    //
                    // 顺序是承重的，本分支必须落在这两条之后：
                    //   ① `status == 403` 门 ⇒ **401 先让路**。token 死了 ≠ 区错了：401 该走
                    //      force-refresh / 计失败，换区对它毫无作用（换个区照样是死 token）。
                    //   ② 上面那条 `bearer_invalid_but_proven` 已 `continue` ⇒ **已成功过的号
                    //      到不了这里**。两条分支吃的是**逐字节相同**的上游文案，唯一的区分位
                    //      就是 `has_ever_succeeded`；顺序反了就会给一个区本来是对的健康号改区。
                    //
                    // ⚠️ 绝不 `report_failure` / 不冷却：region 配错≠号坏（隔离铁律，与上面
                    // FEATURE_NOT_SUPPORTED 那条同款）。惩罚它只会让一个其实好的号被推向禁用。
                    //
                    // `last_outcome` 保持上面已置的 `AuthFailed` 不动：403 bearer-invalid 在
                    // 客户端视角确实是授权层拒绝，改成 ServerError 会把它伪装成上游故障。
                    if status.as_u16() == 403
                        && endpoint.is_bearer_token_invalid(&body)
                        && !region_switched_this_call.contains(&ctx.id)
                        && call_started.elapsed()
                            < Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS)
                    {
                        // 用 `call_creds` 而非 `ctx.credentials`：前者才是**本次请求真正打出去**
                        // 的那个区（含本链内已生效的覆盖），据它算「另一个区」才不会算错。
                        let current = call_creds.effective_upstream_region(&config).to_string();
                        if let Some(target) = region_retry_target(
                            &current,
                            call_creds.is_api_key_credential(),
                            self.token_manager.has_ever_succeeded(ctx.id),
                        ) {
                            // 每号一次上限（见 `region_switched_this_call` 声明处）。
                            region_switched_this_call.insert(ctx.id);
                            region_override_this_call.insert(ctx.id, target.to_string());
                            // ⚠️ 必须把它从「本请求已试过」里摘掉：否则下一跳
                            // `acquire_context_excluding` 会**结构性避开它**，于是换区重试打的
                            // 是别人的号 —— 覆盖值躺在 map 里没人用，等于没换区。摘掉只是让它
                            // 恢复**可被选中**（仍要过冷却/RPM 等既有硬门），不是强行指定。
                            // 若调度这一跳选了别的号并成功，本次覆盖不回写（L2 按 id 取），
                            // 自纠正顺延到下一条客户端请求 —— 迟一点，但绝不会写错。
                            tried_this_call.remove(&ctx.id);
                            tracing::warn!(
                                "凭据 #{} 从未成功过且吃 bearer-invalid 403 ⇒ 判 region 错配：\
                                 {} → {}，同号换区重试一次（不计失败、不冷却）",
                                ctx.id,
                                current,
                                target
                            );
                            last_error = Some(anyhow::anyhow!(
                                "{} API 请求失败（疑似 region 错配，已换区 {} → {} 重试）: {} {}",
                                api_type,
                                current,
                                target,
                                status,
                                body
                            ));
                            continue;
                        }
                    }

                    // 同一个号在一条请求里只惩罚一次：report_failure 累计 3 次即禁用，而循环里
                    // 没有排除集时同号可被连选连打，一条请求就能把它推到 TooManyFailures，
                    // 进而触发全池禁用 → 自愈活锁。custom_api 路径早有 excluded 集，这里补齐。
                    let has_available = if auth_failed_this_call.insert(ctx.id) {
                        self.token_manager.report_failure(ctx.id)
                    } else {
                        tracing::warn!(
                            "凭据 #{} 本次请求已计过一次认证失败，不重复惩罚（防单请求推至 TooManyFailures）",
                            ctx.id
                        );
                        true
                    };
                    if !has_available {
                        last_error = Some(anyhow::anyhow!(
                            "{} API 请求失败（所有凭据已用尽）: {} {}",
                            api_type,
                            status,
                            body
                        ));
                        break;
                    }

                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    // 换号前退避：此前是裸 continue，401/403 风暴下会以零间隔连打多个号，
                    // 与 suspend 分支同一类自我放大。
                    tokio::time::sleep(Self::retry_delay(attempt)).await;
                    continue;
                }

                // 503 MODEL_TEMPORARILY_UNAVAILABLE — 模型容量问题，非凭据问题。
                // 使用慢速退避（1s base）；不调用 report_failure / report_rate_limited，
                // 不影响凭据健康分（健康分反映凭据质量，与模型过载无关）。
                // 只允许 MAX_MODEL_UNAVAILABLE_RETRIES 次慢速重试，耗尽后直接 break 透传错误——
                // 继续切换凭据无意义（所有凭据对同一过载模型等价）。
                // ⚠️ 状态门必须同时收 **503 与 400**：上游对「模型没容量」这同一件事发过两种形态 ——
                // 503 `MODEL_TEMPORARILY_UNAVAILABLE`，以及 400 `ThrottlingException` +
                // `reason:INSUFFICIENT_MODEL_CAPACITY`（实测 24h 272 次）。
                //
                // 原先写死 `== 503`，于是那 272 次逐条落空所有分支、走到函数末尾兜底 ⇒
                // 客户端拿到 **502 Bad Gateway 且无 Retry-After** ⇒ 按永久性服务端故障处理 ⇒
                // 不退避、原样重发。这与 `temporarily is suspended` 修复前是同一个缺陷形态。
                //
                // 400 通常是「请求本身有问题，重试无意义」，所以这里**不放宽整个 400**，
                // 只放宽带该 reason 字面量的那一种 —— 判据在
                // `default_is_model_temporarily_unavailable` 内，两个状态共用同一套处置。
                if (status.as_u16() == 503 || status.as_u16() == 400)
                    && endpoint.is_model_temporarily_unavailable(&body)
                {
                    model_unavailable_attempts += 1;
                    tracing::warn!(
                        "模型暂时不可用（MODEL_TEMPORARILY_UNAVAILABLE，第 {}/{} 次）: {} {}",
                        model_unavailable_attempts,
                        MAX_MODEL_UNAVAILABLE_RETRIES + 1,
                        status,
                        body
                    );
                    last_outcome = crate::usage::RequestOutcome::ModelUnavailable;
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败（模型暂时不可用，建议稍后重试）: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    if model_unavailable_attempts > MAX_MODEL_UNAVAILABLE_RETRIES {
                        // 已用完慢速重试预算，透传过载错误给客户端，让其自行退避。
                        break;
                    }
                    // 慢速退避：1s base，比通用 200ms 更长，避免反复冲击过载路径。
                    sleep(Self::retry_delay_model_unavailable(
                        model_unavailable_attempts - 1,
                    ))
                    .await;
                    continue;
                }

                // 429/408/5xx - 瞬态上游错误：重试但不禁用或切换凭据
                // （避免 429 high traffic / 502 high load 等瞬态错误把所有凭据锁死）
                if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                    tracing::warn!(
                        "API 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );
                    // 429 限流：优先换端点桶（另一 host = 上游另一限流桶），同号换完所有端点
                    // 才走凭据级冷却换号。（仍不禁用、不计永久失败，冷却到期自动恢复）
                    if status.as_u16() == 429 {
                        last_outcome = crate::usage::RequestOutcome::RateLimited;
                        // 上游 429 → 入站整形 RPM 自动挡乘性降档(削平后续入站速率,别继续挤爆上游)。
                        // 只在第 0 轮上报(见本文件 'absorb 循环处的 AIMD 放大说明)。
                        if absorb_round == 0 {
                            self.token_manager.report_upstream_rate_limited();
                        }
                        // 优先用上游给出的精确重置时间：响应头 Retry-After 优先，其次错误 body
                        let retry_after =
                            retry_after_header.or_else(|| endpoint.extract_retry_after_secs(&body));

                        // 🔀 端点桶换桶：**仅当该凭据有回退端点**（端点顺序 > 1，如 ksk_ 的
                        // `cli`/`cli-runtime` 两个独立限流桶）才封禁当前 host 桶 30s 并尝试换下一
                        // 端点；单端点凭据（OAuth 号）**不封桶**、直接走原凭据级冷却换号——
                        // 桶 30s > 凭据冷却 15s 的窗口会让 select_endpoint 返回 None，若该分支落
                        // report_failure 会把瞬态封禁累成永久禁用（见 select_endpoint 的 None 注释）。
                        let order = call_creds.effective_endpoint_order(&self.default_endpoint);
                        if order.len() > 1 {
                            self.endpoint_buckets.lock().insert(
                                (ctx.id, endpoint.name().to_string()),
                                Instant::now() + ENDPOINT_BUCKET_THROTTLE,
                            );
                            if self.has_unthrottled_endpoint(&call_creds, ctx.id) {
                                // ⭐ 照抄 bearer-invalid 403 换区先例（见上文 `tried_this_call.remove`
                                // 的注释）：摘掉"本请求已试过"标记，让 acquire_context_excluding 下轮
                                // 可重新选中本号；同时**不设凭据级冷却**（也不占 rate_limited_this_call，
                                // 否则"全部端点都封"时去重逻辑误判已冷却过、永远不设冷却），避免调度
                                // 改换别的号——那样换端点就落空了。
                                tried_this_call.remove(&ctx.id);
                                tracing::warn!(
                                    "凭据 #{} 端点 {} 429 ⇒ 封桶 {}s，换下一端点继续（本请求链内）",
                                    ctx.id,
                                    endpoint.name(),
                                    ENDPOINT_BUCKET_THROTTLE.as_secs()
                                );
                            } else if rate_limited_this_call.insert(ctx.id) {
                                // 所有端点桶都已封禁：按原有逻辑设凭据级冷却，让调度换号。
                                self.token_manager
                                    .report_rate_limited_with_retry_after(ctx.id, retry_after);
                            } else {
                                tracing::debug!(
                                    "凭据 #{} 本请求链内已冷却过，再次 429 仅换号 failover，不重复惩罚",
                                    ctx.id
                                );
                            }
                        } else if rate_limited_this_call.insert(ctx.id) {
                            // 单端点凭据：与改动前逐字节一致（短冷却换号，不涉及桶）。
                            self.token_manager
                                .report_rate_limited_with_retry_after(ctx.id, retry_after);
                        } else {
                            tracing::debug!(
                                "凭据 #{} 本请求链内已冷却过，再次 429 仅换号 failover，不重复惩罚",
                                ctx.id
                            );
                        }
                    } else {
                        last_outcome = crate::usage::RequestOutcome::ServerError;
                        // 5xx 也给该号设短冷却（30s，自动恢复）。此前只 sleep 就换号、不设冷却，
                        // 失败的号下一轮立刻可再被选中，于是 500 风暴时请求在同一批坏号之间
                        // 来回打（实测一小时 408 次 500），把重试预算烧光却没换到好号。
                        // 本请求链内同号只设一次，复用 429 的去重集语义，避免重复累加。
                        if status.is_server_error() && rate_limited_this_call.insert(ctx.id) {
                            self.token_manager.report_server_error(ctx.id);
                            // 5xx 风暴同样是上游压力信号 → 入站 AIMD 降档。
                            // 只在第 0 轮上报(见本文件 'absorb 循环处的 AIMD 放大说明)。
                            if absorb_round == 0 {
                                self.token_manager.report_upstream_pressure();
                            }
                        }
                    }
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }

                // 其他 4xx - 通常为请求/配置问题：直接返回，不计入凭据失败
                if status.is_client_error() {
                    last_outcome = crate::usage::RequestOutcome::BadRequest;
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    break;
                }

                // 兜底：当作可重试的瞬态错误处理（不切换凭据）
                tracing::warn!(
                    "API 请求失败（未知错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                last_outcome = crate::usage::RequestOutcome::OtherError;
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                if attempt + 1 < max_retries {
                    sleep(Self::retry_delay(attempt)).await;
                }
            }

            // ── 本轮 failover 链已耗尽,决定是否再吸收一轮 ────────────────────────────
            // 下一轮的尝试计数从本轮末尾续上(+1 = 本轮最后那次尝试本身)。
            attempts_base = attempts_used + 1;

            // 关闭时 effective_max_rounds() 恒为 0 ⇒ 这里必定 break，
            // 下面的分类/退避/sleep/计数器一概不执行 ⇒ 逐字节等价旧行为。
            if absorb_round >= absorb.effective_max_rounds() {
                // 轮次用尽也是「吸收层跑过并放弃」的一种（且开着时是最常见的一种）。
                // `absorb_round > 0` 这道限定是承重的：关闭吸收层时这里恒是 0 ⇒ 不置位 ⇒
                // 渲染路径逐字节不变。
                absorb_gave_up_after_rounds |= absorb_round > 0;
                break 'absorb;
            }
            // ⭐ 未修问题 ②：跨轮总额度已用尽 ⇒ 下一轮配额为 0。**必须在这里 break**,不能
            // 靠「进了轮再发现 for 循环跑 0 次」：那样会先睡满一次退避、且 attempts_base 又 +1,
            // 变成每轮白睡一次退避直到 max_rounds 用完 —— 客户端多等好几个退避却零次上游调用。
            //
            // ⚠️ 判据喂 `upstream_calls`（真打上游的次数）而非 `attempts_base`（迭代计数）：
            // 后者含 fast-fail 空转,会在全池冷却时把额度在毫秒内烧空 ⇒ 本闸门抢在下面的截断
            // 闸门之前恒命中 ⇒ 吸收层对它最该拦的那一类（PoolCooldown）从来没起过作用。
            if round_retry_quota(base_retry_quota, upstream_calls) == 0 {
                // ⚠️ 三个 break 'absorb 的 warn 文案必须**互相可分辨**,且各自点名该调哪个旋钮:
                // 本条与下面两条此前都只是散文,而下面两条还共用同一个计数器 ⇒ 面板/日志都区分不出
                // 「额度用尽」「上游恢复期太长」「预算不够睡」三种完全不同的结局,运维会去抬错的旋钮。
                // 这里用 `absorb_stop` 这个结构化字段做机器可读判据(不依赖中文文案不变)。
                // ⭐ 这道闸门此前**不 bump 任何计数器** ⇒ 这类请求既不进吸收比的分子也不进
                // 分母 ⇒ 面板上的吸收比偏乐观（分母里少了被额度掐掉的那批）。而它与另两条
                // 放弃结局的区别是承重的：这是**每请求硬上限**，抬任何 upstreamRetryAbsorb*
                // 旋钮都不会改变结局 —— 归到 budget_exhausted 会把运维引向抬预算（无效）。
                crate::common::recovery_metrics::bump_absorb_retry_quota_exhausted();
                tracing::warn!(
                    absorb_stop = "retry_quota_exhausted",
                    rounds = absorb_round,
                    upstream_calls,
                    attempts = attempts_base,
                    "吸收层已用尽跨轮总重试额度（{} 次真实上游调用），停止吸收并透传上游错误。\
                     这是**每请求**硬上限,与 upstreamRetryAbsorb* 各旋钮无关,抬那些配置不会改变本结局",
                    ABSOLUTE_MAX_TOTAL_RETRIES
                );
                absorb_gave_up_after_rounds |= absorb_round > 0;
                break 'absorb;
            }
            let Some(err) = last_error.as_ref() else {
                break 'absorb;
            };
            let Some(class) = crate::anthropic::absorb_class_of(&err.to_string()) else {
                break 'absorb;
            };
            // ⭐ 各类别的独立开关。判据收在 `class_allowed` 一处（散写必然漏一处，而漏掉那处
            // 的表现是「默认关的类别其实在吸收」—— 硬约束里最不能出的错）。
            //
            // 每类各有可分辨的 skip 计数器：上线后「这一类到底出现过几次、开了会救回多少」
            // 只能靠这组数回答。共用一个桶的话，开三个开关后面板上仍是一个数 ⇒ 无法归因，
            // 也就无法决定该关掉哪个（外挂那 11.6:1 的重试比正是不分类别一律重试的账单）。
            if !absorb.class_allowed(class) {
                use crate::anthropic::AbsorbClass;
                match class {
                    AbsorbClass::SwapWindow => {
                        crate::common::recovery_metrics::bump_absorb_suspend_skipped()
                    }
                    AbsorbClass::TransientServerError => {
                        crate::common::recovery_metrics::bump_absorb_server_error_skipped()
                    }
                    AbsorbClass::TransientCapacity400 => {
                        crate::common::recovery_metrics::bump_absorb_capacity_400_skipped()
                    }
                    // 这两类跟着总开关走，`class_allowed` 对它们恒 true ⇒ 不可达。
                    AbsorbClass::PoolCooldown(_) | AbsorbClass::UpstreamRateLimit => {}
                }
                tracing::debug!(
                    absorb_stop = "class_absorb_disabled",
                    ?class,
                    rounds = absorb_round,
                    "该类别的吸收开关未开启，按现状透传上游错误"
                );
                break 'absorb;
            }
            // ⭐ 未修问题 ③：号池真实恢复时刻超过我们愿意睡的上限 ⇒ 睡醒了池子还在冷却,
            // 这一轮**结构上必然**拿回同一个错误。典型:全池自愈退避 60s
            // (SELF_HEAL_BASE_BACKOFF, token_manager.rs:890 一带) vs max_delay 默认 15s。
            // 此前只 clamp 不判断 ⇒ 睡 15s → 白打一轮 → 客户端多等 15s 拿同一个 429。
            // 必须**在** should_start_another_round 之前判:那条只看预算够不够,
            // 看不出「睡够了但上游没好」—— 两者是独立的失败模式。
            if absorb.backoff_is_truncated(class, absorb_round) {
                // ⭐ 已拆出独立计数器（原先与下面「预算不足一轮」共用
                // `bump_absorb_budget_exhausted()`）：两者该调的旋钮**相反** —— 本条要抬
                // `upstreamRetryAbsorbMaxDelaySecs`（我们愿意睡的上限 < 号池给出的真实恢复
                // 时刻），下面那条要抬 `upstreamRetryAbsorbBudgetSecs`（总预算装不下一轮）。
                // 共用一个桶时面板上看到「吸收比低」无从判断该动哪个，而实测运维会去抬
                // budget，真正的瓶颈是 maxDelay。结构化 `absorb_stop` 仍保留（日志侧判据）。
                crate::common::recovery_metrics::bump_absorb_backoff_truncated();
                tracing::warn!(
                    absorb_stop = "backoff_truncated",
                    rounds = absorb_round,
                    ?class,
                    required_wait_secs = absorb.required_wait(class, absorb_round).as_secs(),
                    max_delay_secs = absorb.class_max_delay(class).as_secs(),
                    "号池真实恢复时间超过退避上限，再吸收一轮必然拿回同一错误，直接透传。\
                     要吸收这一类需抬 upstreamRetryAbsorbMaxDelaySecs（**不是** budgetSecs）"
                );
                absorb_gave_up_after_rounds |= absorb_round > 0;
                break 'absorb;
            }
            let delay = absorb.backoff(class, absorb_round);
            // 本类别的 deadline：换号空窗设了独立预算时用它自己那份（空窗实测 10 分钟 ≫ 总预算
            // 20~45s，共用一个预算装不下）。其余类别恒等于总预算那个 ⇒ 旧行为不变。
            let class_deadline = absorb.class_deadline(call_started, class);
            // 判据是「剩余 > 退避 + 一轮最坏耗时」,不是「剩余 >= 退避」:后者会让这一轮在半路
            // 被 deadline 砍断,白打一轮上游还让客户端多等(设计评审 BLOCKER 9)。
            if !should_start_another_round(class_deadline, std::time::Instant::now(), delay) {
                // 与上一条截断闸门已拆成两个计数器(见那里的长注释),靠 `absorb_stop` 也能区分:
                // 本条的瓶颈是**总预算**,该抬 `upstreamRetryAbsorbBudgetSecs`
                // (换号空窗类则是 upstreamRetryAbsorbSwapBudgetSecs)。
                crate::common::recovery_metrics::bump_absorb_budget_exhausted();
                tracing::warn!(
                    absorb_stop = "budget_too_small_for_round",
                    rounds = absorb_round,
                    ?class,
                    delay_secs = delay.as_secs(),
                    "吸收层预算不足一轮，原样透传上游 429 + Retry-After 让客户端退避。\
                     要吸收这一类需抬 upstreamRetryAbsorbBudgetSecs（**不是** maxDelaySecs）"
                );
                absorb_gave_up_after_rounds |= absorb_round > 0;
                break 'absorb;
            }
            sleep(delay).await;
            // 下一轮的墙钟按**触发本次重试的类别**记账。换号空窗那份更宽的预算只在它自己
            // 触发的轮次生效,不会泄漏给下一轮的其它类别(下一轮若是别的类会被改回来)。
            round_deadline = class_deadline;
            absorb_round += 1;
            crate::common::recovery_metrics::bump_absorb_round();
            // 每类各一个 round 计数器:哪一类在真起作用只能靠这组数回答(见 recovery_metrics 说明)。
            {
                use crate::anthropic::AbsorbClass;
                match class {
                    AbsorbClass::PoolCooldown(_) => {
                        crate::common::recovery_metrics::bump_absorb_round_pool_cooldown()
                    }
                    AbsorbClass::UpstreamRateLimit => {
                        crate::common::recovery_metrics::bump_absorb_round_rate_limit()
                    }
                    AbsorbClass::SwapWindow => {
                        crate::common::recovery_metrics::bump_absorb_round_swap_window()
                    }
                    AbsorbClass::TransientServerError => {
                        crate::common::recovery_metrics::bump_absorb_round_server_error()
                    }
                    AbsorbClass::TransientCapacity400 => {
                        crate::common::recovery_metrics::bump_absorb_round_capacity_400()
                    }
                }
            }
            // ⚠️ 刻意**不重置** last_error:若下一轮没产生新错误(如全池冷却 fast-fail 后 last_error
            // 未被覆盖),重置会让 final_error 落到「已达到最大重试次数」通用串 →
            // map_provider_error 认不出来 → 兜底 502 且无 Retry-After → 客户端从此不退避。
        }

        // 整条客户端请求失败收尾：failover 耗尽只在**吸收循环真正结束**且确有换号 failover 时
        // 记一次（已知问题 #13）。此前放在轮内且每轮清零 ⇒ 一条请求跑 N 轮就计 N 次（多计）；
        // 且成功路径在循环内 return，这里根本走不到 ⇒ 已恢复的请求不再误计为耗尽。
        // 仅当真的换号 failover 过（打了 >1 个号）才计——首个号即因客户端错误/模型无效 break
        // 的不算池耗尽（该区分语义不变，见 `real_failover_happened` 声明处）。
        if real_failover_happened {
            crate::common::recovery_metrics::bump_failover_exhausted();
        }

        // 所有吸收轮与重试都失败:埋点一条失败记录后返回错误。
        // ⚠️ emit_record 与下面的 overload_fallback_model 都必须留在 'absorb **之外**:
        // 放进轮内会让一条客户端请求落 N 条失败记录,面板失败数被吸收轮次乘倍。

        // overload_fallback_model：MODEL_TEMPORARILY_UNAVAILABLE 耗尽重试预算后，
        // 若配置了备用模型，以备用模型做最后一次尝试（限 1 次，不再套完整 failover 循环）。
        // 典型用途：opus 系列过载时切到容量独立的 sonnet（前提：用户已知晓响应质量/计费差异）。
        if last_outcome == crate::usage::RequestOutcome::ModelUnavailable {
            let cfg = self.token_manager.config();
            if let Some(ref fallback_model_id) = cfg.overload_fallback_model.clone() {
                tracing::warn!(
                    "MODEL_TEMPORARILY_UNAVAILABLE 重试耗尽，尝试 overload_fallback_model: {}",
                    fallback_model_id
                );
                let fallback_body = Self::rewrite_model_id(request_body, fallback_model_id);
                if let Ok(ctx) = self
                    .token_manager
                    .acquire_context(Some(fallback_model_id), session_id.as_deref())
                    .await
                {
                    let config = self.token_manager.config();
                    let machine_id =
                        machine_id::generate_from_credentials(&ctx.credentials, &config);
                    // overload fallback：降级模型重试走单端点（首选），不参与换桶——罕见路径。
                    if let Ok(endpoint) = self.endpoint_for(&ctx.credentials) {
                        let rctx = RequestContext {
                            credentials: &ctx.credentials,
                            token: &ctx.token,
                            machine_id: &machine_id,
                            config: &config,
                            is_1m,
                        };
                        let url = endpoint.api_url(&rctx);
                        let body = endpoint.transform_api_body(&fallback_body, &rctx);
                        let base = self
                            .client_for(&ctx.credentials)?
                            .post(&url)
                            .body(body)
                            .header("content-type", endpoint.content_type());
                        let request = endpoint.decorate_api(base, &rctx);
                        match request.send().await {
                            Ok(resp) if resp.status().is_success() => {
                                self.token_manager.report_success(ctx.id);
                                let meta = CallMeta {
                                    credential_id: ctx.id,
                                    model: Some(fallback_model_id.clone()),
                                    session_id: session_id.clone(),
                                    is_streaming: is_stream,
                                    retries: (model_unavailable_attempts + 1) as u32,
                                    latency_ms: call_started.elapsed().as_millis() as u64,
                                    started_at: call_started,
                                    inflight: ctx.inflight,
                                };
                                return Ok((resp, meta));
                            }
                            Ok(resp) => {
                                tracing::warn!(
                                    "overload_fallback_model {} 也失败: {}",
                                    fallback_model_id,
                                    resp.status()
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "overload_fallback_model {} 请求错误: {}",
                                    fallback_model_id,
                                    e
                                );
                            }
                        }
                    }
                }
            }
        }

        let final_error = last_error.unwrap_or_else(|| {
            anyhow::anyhow!(
                "{} API 请求失败：已达到最大重试次数（{}次）",
                api_type,
                base_retry_quota
            )
        });
        // ⭐ 吸收层真的重试过却仍失败,且部署侧要求这类终态回 503:给错误串打机器可读标记,
        // 由 `map_provider_error` 的第一条分支换状态码。
        //
        // 为什么标记必须在**这里**打而不是让 handlers 自己判：handlers 拿到的只有一个错误串,
        // 分不出「吸收层跑过并放弃」与「吸收层根本没开、429 原样透传」。后者改成 503 是错的
        // （网关一次都没重试,却告诉客户端「我们这边暂时不可用」）。这个二分只有 provider 做得出来,
        // 与 `bearer_invalid_transient=1`（`has_ever_succeeded` 那个二分）同款范式。
        //
        // 两个条件都不成立时（默认配置即如此）本段不执行 ⇒ 错误串与渲染路径逐字节不变。
        let final_error = if absorb_gave_up_after_rounds && absorb.exhausted_as_503 {
            // 走 `handlers::` 全路径而不在 `anthropic/mod.rs` 加 re-export：那个文件不在本次
            // 改动范围内，而 `handlers` 本身就是 `pub(crate) mod` ⇒ 直接可达，少改一处即少一个
            // 要同步的真值面。
            let marker = crate::anthropic::handlers::ABSORB_BUDGET_EXHAUSTED_MARKER;
            // ⚠️ 用 `context` 而非重建错误：保留原始错误链（面板/日志里那句上游原文是排障的
            // 唯一线索），同时 `to_string()` 里出现标记 —— anyhow 的 Display 只打最外层,
            // 故标记必须与原文拼在同一层里。
            anyhow::anyhow!("{} {}", final_error, marker)
        } else {
            final_error
        };
        let mut fail_record = crate::usage::RequestRecord::new(
            uuid::Uuid::new_v4().to_string(),
            model.clone().unwrap_or_default(),
        );
        fail_record.credential_id = last_credential_id;
        fail_record.session_id = session_id.clone();
        fail_record.is_streaming = is_stream;
        fail_record.latency_ms = call_started.elapsed().as_millis() as u64;
        fail_record.outcome = last_outcome;
        // ⭐ 失败记录必须带真实换号次数。此前这里没有设 `retries` → 恒为默认 0，
        // 使「烧掉 12 次换号才失败」与「第一次就失败」在面板上不可区分。
        // 与成功分支 `retries: attempt as u32`（本文件下方）同口径。
        fail_record.retries = attempts_used;
        fail_record.error_message = Some(final_error.to_string());
        crate::usage::emit_record(fail_record);

        Err(final_error)
    }

    /// 从请求体中一次性提取模型信息与会话标识（conversationId）。
    ///
    /// 热路径优化（P0-A）：原先 `extract_model_from_request` 与
    /// `extract_session_id_from_request` 各自对整个请求体做一次全量
    /// `serde_json::from_str`，一次调用要解析两遍。合并成解析一次 `Value`、
    /// 再取两个字段，行为完全等价但只付出一次解析开销。
    ///
    /// - model：`conversationState.currentMessage.userInputMessage.modelId`
    /// - session：`conversationState.conversationId`（由 converter 从原始
    ///   metadata.user_id 的 session UUID 派生；无真实 session 时为随机 UUID，
    ///   每次不同，自然不命中亲和性，等价于常规轮换）。
    ///
    /// 请求体解析失败（非法 JSON）时两者都返回 None，与旧实现一致。
    fn extract_model_and_session(request_body: &str) -> (Option<String>, Option<String>) {
        use serde_json::Value;

        let json: Value = match serde_json::from_str(request_body) {
            Ok(v) => v,
            Err(_) => return (None, None),
        };

        let conversation_state = json.get("conversationState");

        let model = conversation_state
            .and_then(|cs| cs.get("currentMessage"))
            .and_then(|m| m.get("userInputMessage"))
            .and_then(|u| u.get("modelId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let session_id = conversation_state
            .and_then(|cs| cs.get("conversationId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        (model, session_id)
    }

    fn retry_delay(attempt: usize) -> Duration {
        // 指数退避 + 少量抖动，避免上游抖动时放大故障
        const BASE_MS: u64 = 200;
        const MAX_MS: u64 = 2_000;
        let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
        let backoff = exp.min(MAX_MS);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }

    /// 慢速退避：专用于 MODEL_TEMPORARILY_UNAVAILABLE（容量过载）。
    ///
    /// 1s base，2x 指数，30s 上限 + 25% jitter。
    /// 与通用 `retry_delay`（200ms base，基础设施瞬态）区分：过载是容量级问题，
    /// 短暂快速重试只是反复冲击同一过载路径，慢速更合理。
    fn retry_delay_model_unavailable(attempt: usize) -> Duration {
        const BASE_MS: u64 = 1_000;
        const MAX_MS: u64 = 30_000;
        let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(5) as u32));
        let backoff = exp.min(MAX_MS);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }

    /// 将序列化的 Kiro 请求体中的 modelId 替换为指定值。
    ///
    /// 用于 overload_fallback_model：过载重试耗尽时，以备用模型再试一次。
    /// 替换路径：`conversationState.currentMessage.userInputMessage.modelId`。
    /// 解析/序列化失败时原样返回，保证函数不 panic。
    fn rewrite_model_id(request_body: &str, new_model: &str) -> String {
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(request_body) else {
            return request_body.to_string();
        };
        if let Some(mid) =
            v.pointer_mut("/conversationState/currentMessage/userInputMessage/modelId")
        {
            *mid = serde_json::Value::String(new_model.to_string());
        }
        serde_json::to_string(&v).unwrap_or_else(|_| request_body.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 动态降档阶梯的边界：0/0.3/0.5 为不变档，0.31/0.51 触发降档，地板 1。
    #[test]
    fn test_apply_retry_pressure_staircase() {
        assert_eq!(apply_retry_pressure(12, 0.0), 12);
        assert_eq!(apply_retry_pressure(12, 0.3), 12, "0.3 恰好是阈值，不降");
        assert_eq!(apply_retry_pressure(12, 0.5), 6, "0.5 未过 0.5 档但过 0.3 档 → 砍半");
        assert_eq!(apply_retry_pressure(12, 0.31), 6, ">0.3 砍半");
        assert_eq!(apply_retry_pressure(12, 0.51), 3, ">0.5 砍到 33%（12*33/100=3）");
        assert_eq!(apply_retry_pressure(12, 1.0), 3, "满额 429 也只砍到 3，不归零");
        assert_eq!(apply_retry_pressure(1, 1.0), 1, "地板 1：降档绝不归零");
        assert_eq!(apply_retry_pressure(3, 0.51), 1, "3 的 33% 向下取整到 1");
    }

    /// 窗口 rate() 是纯计算：直接注入状态验证 429 占比。
    #[test]
    fn test_retry_pressure_window_rate() {
        let mut w = RetryPressureWindow::new(60);
        assert_eq!(w.rate(), 0.0, "空窗口无信号，不降档");
        // 5 成功 + 5 个 429 → 50%
        for i in 0..10 {
            w.deque.push_back((std::time::Instant::now(), i % 2 == 1));
        }
        assert!((w.rate() - 0.5).abs() < 1e-6);
        // 全 429 → 100%
        let mut w2 = RetryPressureWindow::new(60);
        for _ in 0..4 {
            w2.deque.push_back((std::time::Instant::now(), true));
        }
        assert_eq!(w2.rate(), 1.0);
    }

    /// 🔴 回归：5xx 与 429 同样计入压力（纯 500 风暴降档必须触发）；
    /// 4xx（客户端错误）不算压力。
    #[test]
    fn test_retry_pressure_window_counts_5xx_and_not_4xx() {
        let mut w = RetryPressureWindow::new(60);
        // 2 个 500 + 1 个 200 → 压力率 2/3
        w.deque.push_back((std::time::Instant::now(), false)); // 200
        w.deque.push_back((std::time::Instant::now(), true)); // 500
        w.deque.push_back((std::time::Instant::now(), true)); // 500
        assert!(
            (w.rate() - 2.0 / 3.0).abs() < 1e-6,
            "5xx 必须计入压力（纯 500 风暴降档才不失效），实际 {}",
            w.rate()
        );

        // 4xx 不算压力：2 个 400 + 1 个 200 → 压力率 0
        let mut w2 = RetryPressureWindow::new(60);
        w2.deque.push_back((std::time::Instant::now(), false)); // 200
        w2.deque.push_back((std::time::Instant::now(), false)); // 400
        w2.deque.push_back((std::time::Instant::now(), false)); // 400
        assert_eq!(w2.rate(), 0.0, "4xx（客户端错误）不算压力");
    }

    /// record() 顺带逐出超窗事件：极小窗口 + sleep 后，旧事件被清出。
    #[tokio::test]
    async fn test_retry_pressure_window_prune_expired() {
        let mut w = RetryPressureWindow::new(1); // 1s 窗口
        w.record(true);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(w.deque.len(), 1, "窗口内一条还在");
        // 换一个 0 秒窗口：第二次 record 必把第一条逐出
        let mut w0 = RetryPressureWindow::new(0);
        w0.record(true);
        w0.record(false);
        assert_eq!(w0.deque.len(), 1, "0 秒窗口下第一条立即过期");
        assert_eq!(w0.rate(), 0.0, "剩下的那一条是 false");
    }

    /// 并发闸 Semaphore：容量 N 时 N 个 permit 全过、第 N+1 拿不到、Drop 后恢复。
    #[tokio::test]
    async fn test_upstream_gate_concurrency() {
        let gate = Arc::new(tokio::sync::Semaphore::new(2));
        let p1 = gate.clone().try_acquire_owned().unwrap();
        let p2 = gate.clone().try_acquire_owned().unwrap();
        assert!(
            gate.clone().try_acquire_owned().is_err(),
            "容量 2 时第 3 个拿不到"
        );
        drop(p1);
        let p3 = gate.clone().try_acquire_owned().unwrap();
        drop(p2);
        drop(p3);
        let p4 = gate.clone().try_acquire_owned().unwrap();
        drop(p4);
        assert_eq!(gate.available_permits(), 2, "全部 Drop 后 permit 复原");
    }

    /// 预算恒被 `ABSOLUTE_MAX_TOTAL_RETRIES` 封顶，**且刻意不再随可用号数抬高**。
    ///
    /// ⚠️ 本测试此前名为 `..._covers_every_available_credential`，断言 `r >= total`
    /// 并声称"保证每个可用凭据至少被尝试一次"。那个承诺在移除内层 `.max(available)`
    /// 之后已不成立 —— 它当时**只是碰巧通过**：`total=10` 时预算 `min(30,12)=12`，
    /// 而 `12 >= 10` 恰好为真。把 `total` 改成 20 就会失败（预算仍 12 < 20），
    /// 即那是个会在号池扩容时才爆的定时炸弹，且它在维护一条代码已不提供的不变式。
    ///
    /// 现在改为锁住真实行为：封顶生效。若有人把 `.max(available)` 加回来（那正是
    /// 「号池越大越慢」的成因：线上 43 号时预算 = 43，单请求扫全池耗尽 45s 墙钟），
    /// `large_pool_stays_capped` 会立刻失败。
    #[test]
    fn test_compute_max_retries_is_capped_and_ignores_available() {
        // 常规池：按 total*per_cred 走，但受绝对上限封顶。
        assert_eq!(
            compute_max_retries(10, 10),
            (10 * MAX_RETRIES_PER_CREDENTIAL).min(ABSOLUTE_MAX_TOTAL_RETRIES)
        );

        // ⭐ 承重断言：大池必须仍被封顶，**不因可用号多而放开**。
        let large = compute_max_retries(20, 20);
        assert_eq!(
            large, ABSOLUTE_MAX_TOTAL_RETRIES,
            "大号池预算必须封顶在 {}，实际 {} —— 若等于 available 则说明 .max(available) 被加回来了",
            ABSOLUTE_MAX_TOTAL_RETRIES, large
        );

        // `available` 不参与计算：同一 total 下改变 available 不应改变结果。
        assert_eq!(
            compute_max_retries(20, 1),
            compute_max_retries(20, 20),
            "available 已不参与预算计算，改变它不该影响结果"
        );
    }

    /// 预算永不为 0：0 意味着一次都不尝试，请求立刻以「最大重试次数（0次）」失败。
    ///
    /// 这是真实回归的守卫：把预算基数从 `total_count()`（含 disabled，恒非 0）改成
    /// `kiro_selectable_count()` 后，瞬时全池不可选会让基数为 0 → 预算 0 →
    /// acquire_context 的等待逻辑根本没机会跑。线上 20 分钟内出现 10 次。
    #[test]
    fn should_never_return_zero_retry_budget() {
        assert_eq!(
            compute_max_retries(0, 0),
            1,
            "全池瞬时不可选时也必须至少尝试一次，否则请求零重试即失败"
        );
        for (t, a) in [(0usize, 0usize), (0, 1), (1, 0), (1, 1)] {
            assert!(
                compute_max_retries(t, a) >= 1,
                "compute_max_retries({t}, {a}) 不得为 0"
            );
        }
    }

    /// 收紧上限的意图守卫：一条请求不该能连打十几个号。
    ///
    /// 生产事故里 `尝试 8/36` 的 36 = 12 号 × 3，配合 suspend 分支的零延迟遍历，
    /// 一条客户端请求几秒内烧掉 8~12 个账号（同一出口 IP），正是风控要抓的突发特征。
    #[test]
    fn should_cap_retry_budget_well_below_historic_36() {
        // 与生产同规模的池子（12 个可选号）
        assert!(
            compute_max_retries(12, 12) <= ABSOLUTE_MAX_TOTAL_RETRIES,
            "12 号池的预算必须被上限约束，不能回到 36"
        );
        assert!(
            ABSOLUTE_MAX_TOTAL_RETRIES < 36,
            "绝对上限必须显著小于事故时的 36"
        );
    }

    #[test]
    fn test_compute_max_retries_small_pool() {
        // 小号池降重试：total<=SMALL_POOL_THRESHOLD 时每号只重试 1 次，
        // 每个号各摸一次即透传上游错误，避免在小池上反复砸同几个号加重冷却。
        assert_eq!(compute_max_retries(3, 3), 3, "3 号池应每号只摸 1 次 = 3");
        assert_eq!(compute_max_retries(2, 2), 2, "2 号池应每号只摸 1 次 = 2");
        // 只有 1 个凭据仍至少能试 1 次
        assert_eq!(compute_max_retries(1, 1), 1);

        // 刚过小池阈值（total=4）恢复常规 total*MAX_RETRIES_PER_CREDENTIAL。
        assert_eq!(compute_max_retries(4, 4), 4 * MAX_RETRIES_PER_CREDENTIAL);

        // 小池但部分禁用：available 做下限，仍保证可用号被摸到。
        assert!(compute_max_retries(3, 2) >= 2);
    }

    #[test]
    fn test_compute_max_retries_respects_absolute_upper_bound() {
        // 巨量凭据：预算**恒**被 ABSOLUTE_MAX 封顶，不再随 available 放大。
        assert!(compute_max_retries(1000, 1000) <= ABSOLUTE_MAX_TOTAL_RETRIES);
        assert_eq!(
            compute_max_retries(100, 5),
            ABSOLUTE_MAX_TOTAL_RETRIES,
            "可用号少于上限时应封顶到 ABSOLUTE_MAX"
        );
    }

    /// 回归（大号池不得放大重试 · 本轮核心）：预算恒 ≤ 12，与池子大小无关。
    ///
    /// **旧代码为何失败**：`.min(ABSOLUTE_MAX_TOTAL_RETRIES.max(available))` 里的内层
    /// `.max(available)` 在 `available > 12` 时把硬上限自己抵消掉 → 预算 = available。
    /// 线上 43 个号实测预算 = 43，日志即「尝试 43/43」：一条请求顺着整池撞一遍、
    /// 耗尽 45s 墙钟才失败 → 用户体感 45 秒卡死，且**号池越大越慢**。
    /// 旧代码下 `compute_max_retries(43, 43)` 返回 43，本断言会失败。
    #[test]
    fn should_not_scale_retry_budget_with_pool_size() {
        for available in [13usize, 43, 200, 1000] {
            let r = compute_max_retries(available, available);
            assert!(
                r <= ABSOLUTE_MAX_TOTAL_RETRIES,
                "{available} 个可用号时预算为 {r}，必须被 {ABSOLUTE_MAX_TOTAL_RETRIES} 封顶——\
                 否则号池越大单请求越慢（线上实测 43 号 → 尝试 43/43 → 45s 墙钟）"
            );
        }
        // 线上确切规模的定点回归
        assert_eq!(
            compute_max_retries(43, 43),
            ABSOLUTE_MAX_TOTAL_RETRIES,
            "43 号池（线上实测规模）预算必须是 12 而非 43"
        );
    }

    #[test]
    fn test_extract_model_and_session_both_present() {
        // 一次解析应同时取出 modelId 与 conversationId（与旧双解析等价）
        let body = r#"{
            "conversationState": {
                "conversationId": "sess-123",
                "currentMessage": {
                    "userInputMessage": { "modelId": "claude-sonnet-4" }
                }
            }
        }"#;
        let (model, session) = KiroProvider::extract_model_and_session(body);
        assert_eq!(model.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(session.as_deref(), Some("sess-123"));
    }

    #[test]
    fn test_extract_model_and_session_partial() {
        // 只有 conversationId、无 modelId：model=None、session=Some
        let only_session = r#"{"conversationState":{"conversationId":"s1"}}"#;
        let (model, session) = KiroProvider::extract_model_and_session(only_session);
        assert_eq!(model, None);
        assert_eq!(session.as_deref(), Some("s1"));

        // 只有 modelId、无 conversationId：model=Some、session=None
        let only_model =
            r#"{"conversationState":{"currentMessage":{"userInputMessage":{"modelId":"m"}}}}"#;
        let (model, session) = KiroProvider::extract_model_and_session(only_model);
        assert_eq!(model.as_deref(), Some("m"));
        assert_eq!(session, None);
    }

    #[test]
    fn should_build_mcp_record_with_honest_zeros_and_no_credits() {
        let rec = build_mcp_record(7, crate::usage::RequestOutcome::Success, 123, 2);
        assert_eq!(rec.credential_id, Some(7), "必须归属到真实服务的凭据");
        assert_eq!(rec.model, MCP_USAGE_MODEL, "MCP 无 modelId，用显式常量标识");
        // MCP 上游既不返回 token 数也无本地估算依据：只能是 0，不许瞎估。
        assert_eq!(rec.input_tokens, 0);
        assert_eq!(rec.output_tokens, 0);
        assert_eq!(rec.cache_read_tokens, 0);
        assert_eq!(rec.cache_creation_tokens, 0);
        assert_eq!(rec.credits_used, None, "MCP 响应无 meteringEvent");
        assert!(!rec.is_streaming, "MCP 上游是一次性 JSON POST");
        assert_eq!(rec.latency_ms, 123);
        assert_eq!(rec.retries, 2);
        assert_eq!(rec.outcome, crate::usage::RequestOutcome::Success);
        assert!(rec.error_message.is_none(), "成功记录不应带错误信息");
        // request_id 每条唯一，否则 SQLite 主键冲突会静默丢记录。
        let other = build_mcp_record(7, crate::usage::RequestOutcome::Success, 123, 2);
        assert_ne!(rec.request_id, other.request_id);
    }

    /// 源码级守卫：MCP 成功分支里 `report_success` 与 `emit_record` 必须成对出现。
    ///
    /// 单测覆盖不到 `call_mcp_with_retry`（需真实上游 + 号池），而这正是回归发生的地方：
    /// 历史实现只加凭据计数器不落用量记录，导致 success_count 恒大于用量库记录数。
    #[test]
    fn should_emit_usage_record_in_mcp_success_branch() {
        let src = include_str!("provider.rs");
        let mcp_fn = src
            .split("async fn call_mcp_with_retry")
            .nth(1)
            .expect("call_mcp_with_retry 不应被改名");
        // 截到该函数内第一次出现「失败响应」处理为止，只看成功分支。
        let success_branch = mcp_fn
            .split("// 失败响应")
            .next()
            .expect("成功分支的定位注释不应被删改");
        assert!(
            success_branch.contains("report_success"),
            "成功分支应上报凭据成功"
        );
        assert!(
            success_branch.contains("emit_record(build_mcp_record("),
            "MCP 成功分支必须落一条用量记录，否则凭据计数与用量库对不上账"
        );
    }

    /// 源码级守卫（已知问题 #11）：MCP 路径的**失败出口**必须 emit_record + bump 计数器。
    ///
    /// 历史缺陷：`call_mcp_with_retry` 只有成功分支 emit_record，失败全部零埋点 ⇒
    /// MCP 失败在面板与 recovery-metrics 端点上完全不存在，成功率的分子分母对不上账。
    /// 单测覆盖不到（需真实上游 + 号池），用源码断言钉死 7 个失败出口。
    #[test]
    fn mcp_failure_exits_must_emit_record_and_bump_counter() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let mcp_fn = src
            .split("async fn call_mcp_with_retry")
            .nth(1)
            .expect("call_mcp_with_retry 不应被改名");
        // 只看成功分支之后的失败区（把本测试的 needle 排除在命中集外）。
        let failure_region = mcp_fn
            .split("// 失败响应")
            .nth(1)
            .expect("失败响应的定位注释不应被删改");
        assert!(
            failure_region.contains("crate::common::recovery_metrics::bump_mcp_failure()"),
            "MCP 失败出口必须 bump 专用计数器，否则失败在 recovery-metrics 端点上不可见"
        );
        assert!(
            failure_region.contains("emit_record(build_mcp_record("),
            "MCP 失败出口必须 emit_record，否则失败在用量面板上不存在（#11）"
        );
        // client_for 那个出口在「// 失败响应」标记之前，故按整个 MCP 函数计数（排除测试段）。
        assert_eq!(
            mcp_fn
                .matches("crate::common::recovery_metrics::bump_mcp_failure()")
                .count(),
            7,
            "MCP 应有 7 个失败出口（5 条 bail + client_for `?` + 重试耗尽）各自 bump；\
             数量变化说明出口新增/删除，需同步本守卫"
        );
    }

    /// ⭐ 源码级守卫：两处 force-refresh 调用点都必须跳过 api_key 号。
    ///
    /// 单测覆盖不到（需真实上游返回 401/403 才会走到该分支），而这是**会加速烧号**的路径：
    /// api_key 号没有 refreshToken，`refresh_token()` 对它是契约级 bail，
    /// 在热路径上调它结构上不可能成功，却会计入失败 + 落 auth 冷却。
    ///
    /// 线上实测（本轮多开时暴露）：一个 api_key 号遇 403 后每轮白等约 3 秒
    /// （错误串不含任何 HTTP 码 → 被刷新层的黑名单式瞬态判据当可重试 → 1s+2s 退避），
    /// 连计 3 次失败即判死号自动禁用 —— 死亡速度被放大三倍。
    ///
    /// 断言两处而非一处：对话路径与 MCP 路径各有一份 force-refresh 逻辑，
    /// 这种「同款逻辑复制两份」正是本仓 #4 类漏改事故的成因（对话路径修了、MCP 漏了）。
    #[test]
    /// 🔴 额度耗尽判定**不得门控状态码** —— 只认 body 里的 reason 字面量。
    ///
    /// # 实测（2026-08-05，6 小时窗口）
    ///
    /// - `402 Payment Required`：**0 次**
    /// - `400 Bad Request` + `"reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"`：**564 次**
    ///
    /// 旧代码 `status == 402 && is_monthly_request_limit(&body)` ⇒ 那道门从不成立 ⇒
    /// 564 个额度耗尽的请求落到通用 400 分支 `break`，凭据**不禁用、继续留在轮转里**，
    /// 每个新请求再撞一次（实测 #508 一个号吃了 543 次）。
    ///
    /// 回退即 FAIL：把 `if endpoint.is_monthly_request_limit(&body)` 改回
    /// `if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body)` → 本条失败。
    #[test]
    fn quota_exhausted_must_not_be_gated_on_status_code() {
        let src = include_str!("provider.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        // needle 运行时拼接（include_str! 自匹配坑，本仓库踩过四次）。
        let bad = format!(
            "status.as_u16() == 402 && endpoint.is_monthly_request_limit{}",
            "("
        );
        assert!(
            !prod.contains(&bad),
            "额度耗尽不得门控 402：上游已改用 400（实测 402 六小时 0 次、400+OVERAGE 564 次），\
             门控会让所有额度耗尽的号继续留在轮转里反复被撞"
        );
        // 两条路径（对话 + MCP）都必须有不带状态码门控的判定。
        let good = format!("if endpoint.is_monthly_request_limit(&body){}", " {");
        assert_eq!(
            prod.matches(&good).count(),
            2,
            "对话路径与 MCP 路径都必须有该判定（当前 {} 处）",
            prod.matches(&good).count()
        );
        // 顺序守卫：必须在通用 400 分支之前，否则 400 先 break 就永远走不到。
        let qi = prod.find(&good).expect("额度判定不该被改名");
        let generic400 = format!("if status.as_u16() == 400 {}", "{");
        if let Some(gi) = prod.find(&generic400) {
            assert!(
                qi < gi,
                "额度判定必须排在通用 400 分支之前（挪到之后即失效）"
            );
        }
    }

    /// ⭐ 源码级守卫（客户端格式错误不重试防 503 风暴）：客户端请求校验错误分支必须**同时**
    /// 认 `TOOL_USE_RESULT_MISMATCH`（endpoint 层 `is_client_validation_error` 覆盖）与
    /// `TOOL_SCHEMA_INVALID`（本处补认），且命中后直接 break —— 不重试、不换号、不进吸收层。
    ///
    /// 参考 ZyphrZero/kiro.rs endpoint/mod.rs 的 `CLIENT_VALIDATION_REASONS`：这两个 reason
    /// 都是客户端请求构造问题（多轮工具结果不匹配 / 工具 schema 非法），重试/换号只会白烧
    /// 并发请求，放大成上游 503 风暴。漏认任一都会把它们当可重试瞬态错误处理。
    ///
    /// 用源码级守卫而非行为测试：`call_api_with_retry` 需真实上游 + 号池，单测造不出
    /// （本仓既有惯例）。
    #[test]
    fn client_validation_error_recognizes_both_markers_and_breaks() {
        let full = include_str!("provider.rs");
        // 切掉测试段：本测试自身的字面量不能成为假命中源。
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        // 定位该分支：条件文本到第一个 `{` 为止。
        let marker = "if endpoint.is_client_validation_error(&body)";
        let at = src
            .find(marker)
            .expect("客户端请求校验错误分支不应被删除");
        let cond_end = src[at..]
            .find('{')
            .map(|i| at + i)
            .unwrap_or(src.len());
        let cond = &src[at..cond_end];
        assert!(
            cond.contains("TOOL_SCHEMA_INVALID"),
            "客户端请求校验错误分支必须同时认 TOOL_USE_RESULT_MISMATCH（endpoint 层\
             is_client_validation_error）与 TOOL_SCHEMA_INVALID（本处补认）：漏认后者会把\
             客户端构造错误当可重试瞬态，白烧并发请求并放大成上游 503 风暴"
        );
        // 命中后必须 break（直接失败），分支内不得 continue（continue 即重试/换号）。
        let branch_body = &src[at..src[at..]
            .find("break")
            .map(|i| at + i)
            .expect("命中后必须 break（直接失败、不重试不换号）：改回 continue 即回归")];
        assert!(
            !branch_body.contains("continue"),
            "客户端请求校验错误分支内不得 continue：continue 即重试/换号，\
             与『客户端错不重试』的语义冲突"
        );
    }

    // ⚠️ `#[test]` 曾在 2026-08-06 之前的某次改动中丢失，导致本守卫**从未运行过**
    // （表现为编译期 `function is never used` 警告，而非测试失败 —— 所以没人注意）。
    // 上一轮已补过一次又退化，故此处留注记：删这行属性等于悄悄关掉一条守卫。
    #[test]
    fn force_refresh_must_skip_api_key_credentials_at_both_sites() {
        let src = include_str!("provider.rs");
        // ⚠️ needle 必须**运行时拼接**：若把完整串写成一个字面量，它自己也会出现在
        // 本文件里，被 include_str! 读到并多算一处（第一版就是这样，测试在回退前就 FAIL）。
        let needle = format!("{}{}", "if endpoint.is_bearer_token_invalid", "(&body)");
        let sites: Vec<&str> = src.split(needle.as_str()).skip(1).collect();
        assert_eq!(
            sites.len(),
            2,
            "预期恰好两处 force-refresh 调用点（对话路径 + MCP 路径）；\
             数量变化说明有新增/删除，需同步本守卫"
        );
        for (i, site) in sites.iter().enumerate() {
            // 只看该 if 的条件部分（到左花括号为止）
            let cond = site.split('{').next().unwrap_or("");
            assert!(
                cond.contains("is_api_key_credential"),
                "第 {} 处 force-refresh 未跳过 api_key 号：它结构上不可能刷新成功，\
                 却会计入失败并被退避重试，把该号的死亡速度放大三倍。条件为: {cond}",
                i + 1
            );
        }
    }

    /// ⭐ 源码级守卫：**失败记录必须带 `retries`**。
    ///
    /// 单测覆盖不到 `call_api_with_retry` 的失败路径（需真实上游 + 号池才能把重试预算跑穿），
    /// 而这正是回归发生过的地方：`fail_record` 组装块设了 credential_id / session_id /
    /// is_streaming / latency_ms / outcome / error_message，**唯独漏了 `retries`** →
    /// 落库即 `RequestRecord::new` 的默认 0。
    ///
    /// 线上实测坐实（近 2 小时）：全部失败样本 **无一例外 retries=0**
    /// （auth_failed 1487 / rate_limited 1098 / server_error 118 / bad_request 91），
    /// 而同期成功样本有 retries=1、历史号池大时到过 7 以上 —— 统计上不可能，
    /// 除非失败路径从不赋值。后果是「烧掉 12 次换号才失败」与「第一次就失败」
    /// 在面板上完全不可区分，而那恰是判断重试预算是否够用的唯一依据。
    ///
    /// 用源码级守卫而非行为测试的理由与上面两个测试相同。
    #[test]
    fn fail_record_must_carry_retries() {
        let src = include_str!("provider.rs");
        // 定位失败记录组装块：从 `let mut fail_record` 到紧随其后的 `emit_record`。
        let block = src
            .split("let mut fail_record")
            .nth(1)
            .expect("fail_record 组装块不应被改名/删除");
        let block = block
            .split("emit_record")
            .next()
            .expect("fail_record 之后应紧跟 emit_record");
        assert!(
            block.contains("fail_record.retries"),
            "失败记录必须设 retries，否则一切失败样本的重试次数恒为 0，\
             无法区分『扫穿整池才失败』与『首次即失败』"
        );
    }

    /// ⭐ 源码级守卫（已知问题 #20）：准入闸门超时必须**既 emit_record 又 bump 计数器**。
    ///
    /// 旧代码是裸 `anyhow::bail!` —— 被网关自己背压掐掉的请求在面板上**完全不存在**，
    /// 于是看到的成功率**偏乐观**（分母里少了这批）。而面板成功率是本项目后续一切限流
    /// 调参判断的依据，依据本身有偏则调参全是在算空气。实测这类 bail 在高峰时段
    /// 逐小时占比可达两位数。
    ///
    /// 用源码级守卫而非行为测试：触发它需要真实令牌桶排满 + 真实 TokenManager +
    /// 走满 `inbound_queue_max_wait_secs`（默认 5s）的 await，单测里造不出且会拖慢全套。
    /// 判据锚点选**代码**（函数调用与字段赋值），不选注释里的散文 —— 本仓踩过四次
    /// 「锚点选到注释」导致守卫静默通过。
    #[test]
    fn admission_timeout_must_be_observable() {
        let src = include_str!("provider.rs");
        // 切掉测试段：否则本测试自身的 needle 会成为假命中源（本仓的既有惯例）。
        let prod_all = src.split("#[cfg(test)]").next().expect("生产段应存在");
        // ⚠️ 再切掉注释行。第一版没切，把 `bump_inbound_admission_timeout()` 注释掉后
        // 守卫**依然全绿** —— needle 命中的是注释里那句自我说明（本仓的「纸面测试」形态，
        // 已踩过五次）。断言只看真代码。
        let prod: String = prod_all
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let prod = prod.as_str();
        // 定位准入闸门那条 bail 的处理块：从 acquire_admission 调用点到紧随的 `return Err`。
        let gate = ["acquire_admission", "().await"].concat();
        let block = prod
            .split(gate.as_str())
            .nth(1)
            .expect("acquire_admission 调用点不应被改名");
        let block = block
            .split("return Err")
            .next()
            .expect("准入超时分支应以 return Err 收尾（裸 bail! 即回归：无 emit_record 的空间）");

        assert!(
            block.contains("bump_inbound_admission_timeout()"),
            "准入超时必须 bump 专用计数器，否则面板无法把『网关自己的背压』与『上游 429』\
             分开统计 —— 混在一个桶里会让人去调错的旋钮（credentialRpmLimit 而非 inboundTargetRpm）"
        );
        assert!(
            block.contains("emit_record(record)"),
            "准入超时必须 emit_record，否则这批请求在面板上不存在 ⇒ 成功率偏乐观 ⇒ \
             后续一切限流调参的依据有偏"
        );
        // 承重：必须带 error_message，那是与「全池冷却」区分的**唯一**判据 ——
        // 两者都是 credential_id IS NULL，光看那个字段分不开。
        assert!(
            block.contains("record.error_message"),
            "准入超时记录必须带 error_message（含 inbound_admission_timeout=1 字面量）：\
             它与全池冷却同为 credential_id IS NULL，仅靠 credential_id 不可区分"
        );
        // 承重：绝不能在这里重试。闸门在 call_api_with_retry 内部、45s 墙钟也在它内部，
        // 在此重试 = 把同一个请求反复塞回同一个已满的桶（队列更长、成功率不增）。
        let retry_needle = ["acquire_admission", "().await"].concat();
        assert_eq!(
            prod.matches(retry_needle.as_str()).count(),
            1,
            "acquire_admission 全文必须恰好一处调用点：新增第二处即意味着某条路径会重复\
             扣令牌或在背压上重试"
        );
    }

    /// 源码级守卫（E2）：MCP 的 401/403 分支必须**先**判账户级风控/封禁，
    /// 才允许落通用 `report_failure`。
    ///
    /// 用源码级守卫的理由与上一个测试相同：`call_mcp_with_retry` 需真实上游 + 号池，
    /// 单测覆盖不到，而这正是回归发生的地方（本条修复前该分支就是裸 `report_failure`）。
    ///
    /// **旧代码为何失败**：403 分支内只有 `report_failure`，缺
    /// `is_temporary_rate_limit` / `is_account_suspended` 两道判定。
    /// 而 403 `TEMPORARILY_SUSPENDED` 是**临时态**，`report_failure` 累加
    /// `failure_count` 达阈值即以 `TooManyFailures`（**永久型**标签）禁用 →
    /// 临时限流的号走 WebSearch 被打 3 次就永久禁用。这正是历史事故
    /// （12h 内 88 次误禁 + 36 次全池自愈活锁）的同一误判形态：对话路径已修，
    /// 本路径此前漏修。
    #[test]
    fn should_classify_account_risk_before_generic_failure_in_mcp_auth_branch() {
        let src = include_str!("provider.rs");
        let mcp_fn = src
            .split("async fn call_mcp_with_retry")
            .nth(1)
            .expect("call_mcp_with_retry 不应被改名");
        // 只看 401/403 分支：从它的定位注释起，到下一个「瞬态错误」分支为止。
        //
        // ⚠️ 先坐实两个定位标记的**唯一性**，否则本测试会在标记被改名时**静默失效**：
        // `.split(x).next()` 永不返回 None，所以若标记消失，`auth_branch` 会变成
        // 「函数剩余全文」—— 那里同样含 is_temporary_rate_limit / report_failure，
        // 顺序断言可能照样通过，于是守卫形同虚设（审查发现的真实弱点）。
        // 每个标记应恰好出现 2 次：一次在被守卫的代码里，一次在本测试的 split 字面量里。
        const AUTH_MARKER: &str = "// 401/403 凭据问题";
        const TRANSIENT_MARKER: &str = "// 瞬态错误";
        assert_eq!(
            src.matches(AUTH_MARKER).count(),
            2,
            "401/403 定位标记必须唯一（代码 1 处 + 本测试 1 处）；数量变了说明标记被改动，\
             守卫会退化成扫全文而静默失效 —— 请同时更新代码与本测试"
        );
        assert_eq!(
            src.matches(TRANSIENT_MARKER).count(),
            2,
            "瞬态错误定位标记必须唯一（代码 1 处 + 本测试 1 处），同上"
        );
        let auth_branch = mcp_fn
            .split(AUTH_MARKER)
            .nth(1)
            .expect("401/403 分支的定位注释不应被删改")
            .split(TRANSIENT_MARKER)
            .next()
            .expect("瞬态错误分支的定位注释不应被删改");
        // 边界健全性：分支切片必须显著短于整个函数，否则说明切错了（扫到全文）。
        assert!(
            auth_branch.len() < mcp_fn.len() / 2,
            "401/403 分支切片异常大（{} vs 函数 {}），定位失败",
            auth_branch.len(),
            mcp_fn.len()
        );

        let rate_limit_at = auth_branch
            .find("is_temporary_rate_limit")
            .expect("MCP 403 必须判账户级临时风控，否则临时态会被贴 TooManyFailures 永久标签");
        let suspended_at = auth_branch
            .find("is_account_suspended")
            .expect("MCP 403 必须判账户封禁，否则 disabled_reason 会落成 TooManyFailures");
        // 匹配**调用点**而非注释：分支内的说明注释里也出现 report_failure 字样。
        let generic_failure_at = auth_branch
            .find("self.token_manager.report_failure(")
            .expect("非风控 403 仍应计入通用失败（对照：不能修过头把真失败也放过）");

        assert!(
            rate_limit_at < generic_failure_at,
            "临时风控判定必须在 report_failure 之前（顺序错等于没修）"
        );
        assert!(
            suspended_at < generic_failure_at,
            "封禁判定必须在 report_failure 之前"
        );
        // 与对话路径同款：风控命中走分钟级退避，而非累加永久失败。
        assert!(
            auth_branch.contains("report_suspicious_activity"),
            "MCP 风控命中应走 report_suspicious_activity（分钟级退避）"
        );
    }

    #[test]
    fn test_extract_model_and_session_invalid_json() {
        // 非法 JSON：两者都为 None（与旧实现一致，不 panic）
        let (model, session) = KiroProvider::extract_model_and_session("not json");
        assert_eq!(model, None);
        assert_eq!(session, None);

        // 合法 JSON 但缺 conversationState：两者都为 None
        let (model, session) = KiroProvider::extract_model_and_session(r#"{"foo":"bar"}"#);
        assert_eq!(model, None);
        assert_eq!(session, None);
    }
    /// 回归（🔴 会杀号的缺陷）：请求热路径的端点解析必须与 `effective_endpoint` 同口径。
    ///
    /// **旧代码为何 FAIL**：`endpoint_for` 只读 `credentials.endpoint` 原始字段，
    /// 漏了「`ksk_` API Key 号自动路由到 CLI 端点」这一层（`effective_endpoint` 的第 ② 步）。
    /// 实测：同一个 ksk_ 号，`effective_endpoint()` 返回 `cli`，而热路径返回 `ide`。
    ///
    /// **为什么严重**：`ksk_` 号打 IDE 端点会 403（两个端点按凭据类型绑定、不可互换）。
    /// 403 走 `report_suspicious_activity`，连续 6 次即判死号自动禁用 ——
    /// 于是一个**完全健康**的 ksk_ 号，只因没手工填 `endpoint: cli` 就被烧掉。
    /// 这与线上号池"单号存活 25~60 分钟"的现象直接相关。
    ///
    /// 用源码级断言而非构造 provider：`endpoint_for` 需要完整的 endpoints 注册表 + 配置，
    /// 而缺陷本身只在"读哪个字段"这一行，源码断言足以锁死且不会因重构失效。
    #[test]
    fn endpoint_for_must_use_effective_endpoint_not_raw_field() {
        let src = include_str!("provider.rs");
        let body = src
            .split("fn endpoint_for")
            .nth(1)
            .expect("endpoint_for 不应被改名")
            .split("\n    /// ")
            .next()
            .expect("函数体应以下一项文档注释为界");
        assert!(
            body.contains("effective_endpoint"),
            "请求热路径必须走 effective_endpoint（否则 ksk_ 号走错端点 → 403 → 被当死号禁用）"
        );
        assert!(
            !body.contains(".endpoint\n            .as_deref()"),
            "不得回退到直读 credentials.endpoint 原始字段"
        );
    }

    /// 配套：坐实 `effective_endpoint` 对 ksk_ 号确实路由到 CLI（本回归的前提）。
    #[test]
    fn effective_endpoint_routes_api_key_credential_to_cli() {
        let mut c = crate::kiro::model::credentials::KiroCredentials::default();
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("ksk_test_key".to_string());
        c.endpoint = None;
        assert_eq!(
            c.effective_endpoint("ide"),
            crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME,
            "ksk_ 号未显式配置时应自动路由到 CLI"
        );
        // 显式配置优先（面板可切回 ide 救急）
        c.endpoint = Some("ide".to_string());
        assert_eq!(c.effective_endpoint("ide"), "ide", "显式配置必须优先");
    }

    /// ⭐ 守卫：`select_endpoint` 必须按 `effective_endpoint_order` 候选顺序遍历，
    /// 而不是只取 `effective_endpoint` 单值。若回退成单值，429 换桶机制失去「q.* 封桶后落
    /// runtime.*」的能力，等于回到单端点。
    #[test]
    fn select_endpoint_must_use_endpoint_order_for_bucket_fallback() {
        let src = include_str!("provider.rs");
        let body = src
            .split("fn select_endpoint")
            .nth(1)
            .expect("select_endpoint 不应被改名")
            .split("\n    /// ")
            .next()
            .expect("函数体应以下一项文档注释为界");
        assert!(
            body.contains("effective_endpoint_order"),
            "select_endpoint 必须用 effective_endpoint_order 遍历候选端点（q.* 优先、runtime.* 回退）"
        );
        assert!(
            body.contains("endpoint_buckets"),
            "select_endpoint 必须查询端点桶封禁状态"
        );
    }

    // ══════════ select_endpoint 主动轮换（round-robin）══════════

    /// 用真实端点注册表构造 provider（select_endpoint 只查 name，不触达实现细节）。
    fn provider_with_default(default_endpoint: &str) -> KiroProvider {
        let cfg = crate::model::config::Config::default();
        let tm = Arc::new(
            MultiTokenManager::new(cfg, vec![], None, None, false).expect("测试 token manager"),
        );
        KiroProvider::with_proxy(
            tm,
            None,
            crate::kiro::endpoint::registry(),
            default_endpoint.to_string(),
        )
    }

    /// ksk_ API Key 凭据：`effective_endpoint_order` 返回多端点候选链（q.* 优先、其余回退）。
    fn ksk_credential() -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("ksk_test_key".to_string());
        c.endpoint = None;
        c
    }

    /// 连续 N 次 select_endpoint 同一凭据应轮流覆盖所有非冷却端点（断言起点在轮换）。
    #[test]
    fn select_endpoint_rotates_through_all_available_endpoints() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let cred = ksk_credential();
        let order = cred.effective_endpoint_order(cli);
        assert!(order.len() >= 2, "ksk_ 号应为多端点候选链");
        // 计数器起点 0 → 起点依次 0,1,2,...,0,1,2,...，全可用时轮流命中 order 全序。
        let got: Vec<&str> = (0..order.len() * 2)
            .map(|_| {
                provider
                    .select_endpoint(&cred, 123)
                    .expect("全可用必有返回")
                    .name()
            })
            .collect();
        let expected: Vec<&str> = (0..2).flat_map(|_| order.iter().copied()).collect();
        assert_eq!(
            got, expected,
            "同一凭据的连续请求必须轮流覆盖所有非冷却端点"
        );
    }

    /// 部分桶冷却时，轮换仍只选非冷却桶（跳过被封桶，恒命中剩余桶）。
    #[test]
    fn select_endpoint_rotation_skips_cooled_buckets() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let cred = ksk_credential();
        let order = cred.effective_endpoint_order(cli);
        // 封掉首选桶（q.*），其余候选保持可用。
        provider.endpoint_buckets.lock().insert(
            (123, order[0].to_string()),
            Instant::now() + Duration::from_secs(60),
        );
        let mut picked: HashSet<&str> = HashSet::new();
        for _ in 0..order.len() * 2 {
            let ep = provider
                .select_endpoint(&cred, 123)
                .expect("还有非冷却桶，必有返回");
            assert_ne!(ep.name(), order[0], "轮换不得选中被封的端点桶");
            picked.insert(ep.name());
        }
        // 足够多的调用里，所有非冷却桶都应被轮换到。
        assert_eq!(
            picked.len(),
            order.len() - 1,
            "部分冷却时轮换应覆盖所有非冷却桶"
        );
    }

    /// 全部冷却返回 None（既有语义，轮换不得破坏）。
    #[test]
    fn select_endpoint_rotation_all_cooled_returns_none() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let cred = ksk_credential();
        let order = cred.effective_endpoint_order(cli);
        {
            let mut buckets = provider.endpoint_buckets.lock();
            for name in &order {
                buckets.insert((123, name.to_string()), Instant::now() + Duration::from_secs(60));
            }
        }
        assert!(
            provider.select_endpoint(&cred, 123).is_none(),
            "全部冷却必须返回 None"
        );
    }

    /// order 长度 1（单端点 OAuth 号）：轮换恒取起点 0，行为与固定优先序完全一致（零回归）。
    #[test]
    fn select_endpoint_single_endpoint_does_not_rotate() {
        let provider = provider_with_default("ide");
        let cred = KiroCredentials::default(); // 无 api_key、无显式 endpoint → order=[ide]
        for _ in 0..4 {
            let ep = provider
                .select_endpoint(&cred, 9)
                .expect("单端点不封必有返回");
            assert_eq!(ep.name(), "ide", "单端点轮换无效果，恒返回唯一端点");
        }
    }

    /// ⭐ 守卫：429 分支必须实现「封当前端点桶 + 判断是否还有未封端点 + 换端点时摘出本号」。
    /// 这三步缺一，换桶就退化成「设凭据冷却换号」，q.*/runtime.* 双桶形同虚设。
    #[test]
    fn bucket_switch_on_429_must_throttle_and_release_credential() {
        let src = include_str!("provider.rs");
        let prod: String = src
            .split("#[cfg(test)]")
            .next()
            .expect("生产段应存在")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // 封桶时长常量必须存在且被生产段使用。
        assert!(
            prod.contains("ENDPOINT_BUCKET_THROTTLE"),
            "429 分支必须封禁端点桶（引用 ENDPOINT_BUCKET_THROTTLE）"
        );
        assert!(
            prod.contains("has_unthrottled_endpoint"),
            "429 必须用 has_unthrottled_endpoint 判断是否还有未封端点（决定换端点还是换号）"
        );
        assert!(
            prod.contains("tried_this_call.remove(&ctx.id)"),
            "换端点路径必须把本号从 tried_this_call 摘出，否则 acquire_context_excluding 结构性避开它"
        );
    }

    /// ⭐ 守卫（BLOCKER 回归）：换端点继续分支**不得**占位 `rate_limited_this_call`。
    ///
    /// 若在 `tried_this_call.remove` 后顺手 `rate_limited_this_call.insert(ctx.id)` 当"占位"，
    /// 则当第二端点也 429、`has_unthrottled_endpoint` 返回 false 时，`else if rate_limited_this_call
    /// .insert(ctx.id)` 恒为 false → 落最终 else 只打 debug → **凭据级冷却永不设置**，只靠
    /// `tried_this_call` 排除，跨请求又靠 `select_endpoint` None 分支设 30s 冷却兜底。双端连 429
    /// 的凭据会失去"全部封 → 冷却换号"的语义，退化成在桶窗口内反复打上游。
    #[test]
    fn bucket_switch_branch_must_not_occupy_rate_limited_this_call() {
        let src = include_str!("provider.rs");
        let start = src
            .find("has_unthrottled_endpoint(&call_creds")
            .expect("429 换桶判断应存在");
        // 窗口从换桶判断（`has_unthrottled_endpoint`）截到「全部端点都封」的 `else if` 之前，
        // 中间正好是换端点继续分支体（tried_this_call.remove + warn），不应含任何 insert。
        let end = src[start..]
            .find("else if rate_limited_this_call")
            .map(|i| start + i)
            .expect("全部封分支的 else if 应存在");
        let window = &src[start..end];
        assert!(
            !window.contains("rate_limited_this_call.insert"),
            "换端点继续分支不得占位 rate_limited_this_call —— 否则全部端点都封时去重逻辑误判 \
             已冷却过、永不设凭据级冷却（双端点连 429 时凭据冷却失效）"
        );
    }

    // ══════════ 上游 429 吸收层 ══════════

    fn absorb_cfg(enabled: bool) -> crate::model::config::Config {
        let mut c = crate::model::config::Config::default();
        c.upstream_retry_absorb_enabled = enabled;
        c
    }

    /// ⭐ BLOCKER 9 守卫：吸收准入判据必须是「剩余 > 退避 + 一轮最坏耗时(20s)」。
    ///
    /// 回退即 FAIL：把 `should_start_another_round` 换回「剩余 >= 退避」（即删掉
    /// `+ ABSORB_MIN_USEFUL_ROUND_SECS`），下面第二条断言立刻失败 —— 那种判据下
    /// 剩余 25s / 退避 10s 会被判定"够跑一轮"，然后这一轮必然在半路被 deadline 砍断：
    /// 白打一轮上游、客户端白等，正是外置 shield 的 p50 73.2s 的成因。
    #[test]
    fn absorb_budget_gate_requires_room_for_a_full_round() {
        let now = std::time::Instant::now();
        let d = Duration::from_secs;

        // 剩余 45s、退避 10s ⇒ 45 > 10+20 ⇒ 可以再跑一轮。
        assert!(
            should_start_another_round(now + d(45), now, d(10)),
            "剩余 45s / 退避 10s 应当允许再跑一轮"
        );
        // ⭐ 承重断言：剩余 25s、退避 10s ⇒ 25 > 30 为假 ⇒ 必须放弃。
        //   若判据退回 `剩余 >= 退避`，25 >= 10 会为真 → 本断言 FAIL。
        assert!(
            !should_start_another_round(now + d(25), now, d(10)),
            "剩余 25s 不足以容纳 退避 10s + 一轮最坏 20s，必须放弃而非白打一轮"
        );
        // 边界：恰好等于 delay+20 也要拒（严格大于）。
        assert!(
            !should_start_another_round(now + d(30), now, d(10)),
            "恰好等于 退避+一轮最坏耗时 时必须拒绝（严格大于）"
        );
        // deadline 已过：saturating 归零，必拒，且不 panic。
        assert!(!should_start_another_round(now, now + d(5), d(1)));
    }

    /// 关闭时 `effective_max_rounds()` 恒为 0 ⇒ 「关 ⇒ 零额外轮次」。
    ///
    /// 回退即 FAIL：把 `effective_max_rounds` 改成无条件返回 `self.max_rounds`
    /// （即删掉 `if self.enabled`），第一条断言失败。这条是「默认关等价旧行为」
    /// 的唯一可断言支点 —— 循环里的 `absorb_round >= effective_max_rounds()`
    /// 正是靠它在关闭时立即 break。
    #[test]
    fn absorb_policy_disabled_yields_zero_rounds() {
        let off = AbsorbPolicy::from_config(&absorb_cfg(false));
        assert_eq!(
            off.effective_max_rounds(),
            0,
            "吸收层关闭时必须是零额外轮次（否则 'absorb 循环不会立即 break）"
        );
        let on = AbsorbPolicy::from_config(&absorb_cfg(true));
        assert_eq!(
            on.effective_max_rounds(),
            crate::model::config::Config::default().upstream_retry_absorb_max_rounds,
            "开启时应当用配置的 max_rounds"
        );
    }

    /// 关闭时 `round_budget()` 恒返完整 45s，与旧代码的墙钟判据逐字节等价。
    ///
    /// 回退即 FAIL：把 `round_budget` 里的 `if self.enabled` 去掉（无条件夹 deadline），
    /// 则关闭状态下剩余预算会参与 min() → 墙钟闸门行为改变 → 第一条断言失败。
    #[test]
    fn absorb_disabled_keeps_legacy_wall_clock_budget() {
        let off = AbsorbPolicy::from_config(&absorb_cfg(false));
        let now = std::time::Instant::now();
        let full = Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS);
        // 即便 deadline 已经过期，关闭状态也必须返完整 45s（等价旧行为）。
        assert_eq!(off.round_budget(now, now + Duration::from_secs(99)), full);
        assert_eq!(off.round_budget(now + Duration::from_secs(1), now), full);

        // 开启时：一轮上限被剩余预算夹住，这就是"吸收轮不会超总预算"的机制。
        let on = AbsorbPolicy::from_config(&absorb_cfg(true));
        let squeezed = on.round_budget(now + Duration::from_secs(12), now);
        assert_eq!(
            squeezed,
            Duration::from_secs(12),
            "剩余 12s 时一轮墙钟预算必须被夹到 12s，而不是仍用 45s"
        );
        assert!(
            on.round_budget(now + Duration::from_secs(600), now) <= full,
            "剩余预算再大，单轮也不得超过 MAX_REQUEST_RETRY_BUDGET_SECS"
        );
    }

    /// 403 临时风控被允许吸收时，额外轮次**硬钉为 1**。
    ///
    /// 回退即 FAIL：删掉 `from_config` 里的 `.min(1)`，断言失败。
    /// 依据：403 是账号级、族级连坐已让同族全退，多轮重试只会把更多号烧进正在惩罚的窗口，
    /// 且与 `SELF_HEAL_BASE_BACKOFF=60s`（存在的意义就是停止试探）直接冲突。
    #[test]
    fn absorb_suspended_pins_rounds_to_one() {
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_max_rounds = 3;
        c.upstream_retry_absorb_suspended = true;
        assert_eq!(
            AbsorbPolicy::from_config(&c).effective_max_rounds(),
            1,
            "开启 403 吸收时额外轮次必须硬钉 1（与自愈退避冲突，多轮会加深封禁）"
        );
    }

    /// 一次调用只取**一份**策略快照：`absorb_suspended` 必须来自 `AbsorbPolicy`，
    /// 循环里不得再 `self.token_manager.config()` 重读。
    ///
    /// 回退即 FAIL：把循环里的 `absorb.absorb_suspended` 换回
    /// `self.token_manager.config().upstream_retry_absorb_suspended`，断言失败。
    /// 理由：admin 在两个吸收轮之间热更配置，会让同一条客户端请求前半程按旧策略、
    /// 后半程按新策略走（`max_rounds` 已按旧值定好，suspended 判据却用了新值），
    /// 行为既不可复现也无法用测试固定。
    #[test]
    fn absorb_policy_is_snapshotted_once_per_call() {
        let src = include_str!("provider.rs");
        let retry_fn = src
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        let body = retry_fn.split("mod tests").next().unwrap_or(retry_fn);
        assert_eq!(
            body.matches("AbsorbPolicy::from_config").count(),
            1,
            "一次调用只应取一份策略快照"
        );
        let reread = format!("{}{}", "config().upstream_retry_absorb", "_suspended");
        assert!(
            !body.contains(reread.as_str()),
            "吸收循环内不得重读 config 的 suspended 标记：应使用 AbsorbPolicy 快照，\
             否则轮次之间的热更会让同一条请求前后按不同策略走"
        );
        // 策略里确实带上了这个字段（防有人删字段又改回重读）。
        assert!(
            src.contains("absorb_suspended: bool"),
            "AbsorbPolicy 必须持有 absorb_suspended 字段"
        );
    }

    /// 退避：号池真值优先，且恒被 clamp 进 [min_delay, max_delay]。
    ///
    /// 回退即 FAIL：删掉 clamp 的下界 → `PoolCooldown(0)` 会返回 0 → 吸收循环变成无 sleep 的
    /// 忙等（正是 acquire_context 那次 CPU 打满一核、请求永不返回的事故形态），第二条断言失败。
    #[test]
    fn absorb_backoff_prefers_pool_truth_and_clamps() {
        use crate::anthropic::AbsorbClass;
        let p = AbsorbPolicy::from_config(&absorb_cfg(true));

        // 号池给的真值在区间内 → 原样采用（无需等 HTTP Retry-After 头往返）。
        assert_eq!(
            p.backoff(AbsorbClass::PoolCooldown(8), 0),
            Duration::from_secs(8)
        );
        // ⭐ 承重断言：0 秒也必须睡满 min_delay，绝不返回 0。
        assert_eq!(
            p.backoff(AbsorbClass::PoolCooldown(0), 0),
            p.min_delay,
            "退避为 0 会让吸收循环变成忙等死循环，必须抬到 min_delay"
        );
        // 超上限被夹（防单请求长挂）。
        assert_eq!(p.backoff(AbsorbClass::PoolCooldown(9999), 0), p.max_delay);
        // 无真值：指数增长且不越界。
        let r0 = p.backoff(AbsorbClass::UpstreamRateLimit, 0);
        let r2 = p.backoff(AbsorbClass::UpstreamRateLimit, 2);
        assert!(r2 > r0, "无号池真值时应指数退避");
        assert!(r2 <= p.max_delay);
        // 大 round 不得 panic（移位溢出）也不得越界。
        assert!(p.backoff(AbsorbClass::UpstreamRateLimit, 64) <= p.max_delay);
    }

    /// ⭐ `min_delay > max_delay` 不得 panic：`Duration::clamp` 的 std 契约是
    /// `min > max` 即 panic，而这两个值来自面板上两个独立数字框（毫秒框上限 60000 /
    /// 秒框下限 1），`minDelayMs=60000` + `maxDelaySecs=1` 一次手滑即可配出。
    ///
    /// 回退即 FAIL：删掉 `from_config` 里 `min_delay` 的 `.min(max_delay)`，
    /// 下面每一条 `backoff` 调用都会 panic（`assertion failed: min <= max`），
    /// 而 panic 发生在**请求热路径**上 —— 开启吸收层后每个 429 都会打到。
    #[test]
    fn absorb_min_delay_above_max_is_normalized_not_panicking() {
        use crate::anthropic::AbsorbClass;
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_min_delay_ms = 60_000; // 面板毫秒框上限
        c.upstream_retry_absorb_max_delay_secs = 1; // 面板秒框下限
        let p = AbsorbPolicy::from_config(&c);

        assert!(
            p.min_delay <= p.max_delay,
            "构造后必须满足 min_delay <= max_delay，否则 backoff 的 clamp 会 panic"
        );
        // 方向是「抬 max 到 min」：矛盾配置下宁可退避更久（吸收层不干活、回落旧行为），
        // 而不是退避更短（对还在冷却的号池连打，正是吸收层要避免的事）。
        assert_eq!(p.min_delay, Duration::from_secs(60), "min 应被尊重");
        assert_eq!(
            p.max_delay,
            Duration::from_secs(60),
            "max 应被抬到不低于 min"
        );

        // 三类都不得 panic，且结果落在退化后的单点区间上。
        assert_eq!(p.backoff(AbsorbClass::PoolCooldown(0), 0), p.max_delay);
        assert_eq!(p.backoff(AbsorbClass::PoolCooldown(9999), 0), p.max_delay);
        assert_eq!(p.backoff(AbsorbClass::UpstreamRateLimit, 5), p.max_delay);
        assert_eq!(p.backoff(AbsorbClass::SwapWindow, 0), p.max_delay);
        // 新增的两类同样不得 panic（`class_max_delay` 只对 SwapWindow 且设了 swap 预算时放宽，
        // 这里 swap 预算是 0 ⇒ 五类共用同一个退化区间）。
        assert_eq!(p.backoff(AbsorbClass::TransientServerError, 3), p.max_delay);
        assert_eq!(p.backoff(AbsorbClass::TransientCapacity400, 3), p.max_delay);
    }

    /// ⭐ 吸收总预算**不得低于** 45s，否则它会反向砍掉既有的 failover 墙钟。
    ///
    /// 回退即 FAIL：删掉 `from_config` 里 budget 的
    /// `.max(Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS))` —— 面板允许填 1，
    /// 而 `round_budget()` 是 `min(45s, 剩余预算)`，于是填 5 会让**第 0 轮**
    /// （关掉吸收层时唯一的那一轮）的换号墙钟从 45s 变成 5s：与吸收层无关的正常
    /// 重试被截断，而面板上看不出这层耦合。
    #[test]
    fn absorb_budget_cannot_shrink_the_failover_wall_clock() {
        let full = Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS);
        let now = std::time::Instant::now();

        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_budget_secs = 5; // 面板允许的小值
        let p = AbsorbPolicy::from_config(&c);
        assert!(
            p.budget >= full,
            "总预算被抬到不低于 45s，实际 {:?}",
            p.budget
        );
        // 承重：第 0 轮（round_started == deadline - budget 起点）仍拿满 45s。
        assert_eq!(
            p.round_budget(now + p.budget, now),
            full,
            "第 0 轮的 failover 墙钟不得因吸收层旋钮变短"
        );

        // 反向：填大值应能真的放宽总预算（旋钮仍然有用，只是单向）。
        let mut c2 = absorb_cfg(true);
        c2.upstream_retry_absorb_budget_secs = 120;
        assert_eq!(
            AbsorbPolicy::from_config(&c2).budget,
            Duration::from_secs(120),
            "大于 45s 的值必须原样生效，否则这个旋钮等于没有"
        );
    }

    /// ⭐ `maxDelaySecs=0` 不得产生零退避 —— 那是忙等死循环，不是「不等待」。
    ///
    /// 回退即 FAIL：删掉 `from_config` 里 `max_delay` 的 `.max(ABSORB_MIN_BACKOFF)` ——
    /// `max_delay=0` 会把 `min_delay` 也经 `.min()` 压成 0，`backoff()` 对每一类都返
    /// `Duration::ZERO`，吸收循环变成无 sleep 的 `continue`：打满一核、请求永不返回。
    /// 该值经 Admin API 可写（`service.rs` 对这两个字段无 clamp），所以这是可达状态。
    #[test]
    fn absorb_zero_max_delay_cannot_produce_busy_loop() {
        use crate::anthropic::AbsorbClass;
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_max_delay_secs = 0;
        c.upstream_retry_absorb_min_delay_ms = 0;
        let p = AbsorbPolicy::from_config(&c);

        assert!(
            p.max_delay >= ABSORB_MIN_BACKOFF,
            "max_delay 必须有绝对下限"
        );
        for (label, d) in [
            (
                "PoolCooldown(0)",
                p.backoff(AbsorbClass::PoolCooldown(0), 0),
            ),
            (
                "PoolCooldown(9999)",
                p.backoff(AbsorbClass::PoolCooldown(9999), 0),
            ),
            (
                "UpstreamRateLimit",
                p.backoff(AbsorbClass::UpstreamRateLimit, 0),
            ),
            ("SwapWindow", p.backoff(AbsorbClass::SwapWindow, 0)),
            (
                "TransientServerError",
                p.backoff(AbsorbClass::TransientServerError, 0),
            ),
            (
                "TransientCapacity400",
                p.backoff(AbsorbClass::TransientCapacity400, 0),
            ),
        ] {
            assert!(
                d >= ABSORB_MIN_BACKOFF,
                "{label} 退避为 {d:?}，零/过小退避会让吸收循环变成忙等死循环"
            );
        }
    }

    /// ⭐ 源码级守卫：`bearer token invalid` 打在**已成功过**的号上必须判瞬态，
    /// 且该判定必须在 `report_failure` **之前**。
    ///
    /// 用源码断言：走到这条分支需要真实上游返 403 + 真实号池，行为测试写不了
    /// （本仓惯例，见 `should_emit_usage_record_in_mcp_success_branch`）。
    ///
    /// 回退即 FAIL：删掉 `bearer_invalid_but_proven` 那段，或把它移到
    /// `report_failure` 之后 —— 高并发下 3 次瞬态 403 会在 1 秒内把一个
    /// 93.9% 成功率的号推到 `TooManyFailures`（实测 #481：2412 次成功仍被禁），
    /// 池子少一个号 → 剩下的吃更多流量 → 更易撞惩罚窗口。当天 116 次禁用/42 次自愈。
    ///
    /// 同时钉住「从未成功的号不受影响」：那些是真 region 错配（实测 3 个号 17 次），
    /// 必须继续计失败并被禁用，否则死号会永久占着调度位。
    #[test]
    fn bearer_invalid_on_proven_credential_must_not_count_as_failure() {
        // needle 运行时拼接：完整字面量会被 include_str! 读到自己而自匹配（本文件已踩三次）。
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);

        let guard = format!("{}{}", "bearer_invalid_but", "_proven");
        let proven_check = format!("{}{}", "has_ever_", "succeeded(ctx.id)");
        let punish = format!("{}{}", "report_failure", "(ctx.id)");

        let guard_at = src.find(guard.as_str()).expect("瞬态判定不应被改名");
        assert!(
            src.contains(proven_check.as_str()),
            "必须用 has_ever_succeeded 区分「真 region 错配」与「瞬态抖动」"
        );
        // 对话路径的 report_failure 必须在守卫之后。
        let punish_at = src
            .rfind(punish.as_str())
            .expect("report_failure 调用点不应被改名");
        assert!(
            guard_at < punish_at,
            "瞬态判定必须在 report_failure 之前，否则健康号仍会被 3 次抖动打死"
        );
        // 处置必须是冷却而非计失败。
        let cooldown = format!("{}{}", "report_auth_", "cooldown(ctx.id)");
        assert!(
            src.contains(cooldown.as_str()),
            "瞬态分支应设短冷却让调度避开该号，而不是什么都不做（否则下一跳可能再选它）"
        );
    }

    /// ⭐ 未修问题 ②（跨轮次数预算）：`ABSOLUTE_MAX_TOTAL_RETRIES` 必须是「**每请求**」
    /// 而非「每轮」的上限。
    ///
    /// 缺陷是两处组合出来的：单看 `=12` 没问题，单看「每轮重跑 for 循环」也没问题，
    /// 但配额在循环外只算一次、循环每轮重跑 ⇒ 每轮各拿一份完整 12 ⇒ `max_rounds=3`
    /// 时一条客户端请求最坏 (1+3)×12 = **48 次**上游调用、同一出口 IP，正是当初把
    /// 64 砍到 12 要压住的突发特征。
    ///
    /// 本测试模拟整条客户端请求：把每轮配额按 `round_retry_quota` 算出来累加，
    /// 断言总和恒 ≤ 12。回退即 FAIL：让 `round_retry_quota` 忽略 `attempts_before`
    /// （直接 `base_quota`）→ 总和变 48 → 第二条断言失败。
    #[test]
    fn total_upstream_attempts_are_capped_per_request_not_per_round() {
        // 大号池 ⇒ 基础配额吃满 12（compute_max_retries(12,12) == 12）。
        let base = compute_max_retries(12, 12);
        assert_eq!(base, ABSOLUTE_MAX_TOTAL_RETRIES, "前提：基础配额吃满硬上限");

        // 模拟 1 + max_rounds 轮，每轮把配额跑满（最坏情况）。
        let max_rounds = crate::model::config::Config::default().upstream_retry_absorb_max_rounds;
        let mut attempts_base: u32 = 0;
        let mut total: usize = 0;
        for _round in 0..=max_rounds {
            let quota = round_retry_quota(base, attempts_base);
            if quota == 0 {
                break;
            }
            total += quota;
            // 与热路径同款递推：attempts_used = attempts_base + (quota-1)，再 +1。
            attempts_base += quota as u32;
        }
        assert!(max_rounds >= 1, "前提：默认 max_rounds 至少 1 轮才有意义");
        assert!(
            total <= ABSOLUTE_MAX_TOTAL_RETRIES,
            "一条客户端请求打向上游的总次数 {} 超过硬上限 {} —— 上限退化成「每轮」语义，\
             max_rounds={} 时单请求会打 (1+{})×{} 次上游、同一出口 IP",
            total,
            ABSOLUTE_MAX_TOTAL_RETRIES,
            max_rounds,
            max_rounds,
            base
        );
    }

    /// `round_retry_quota` 的边界：额度用尽必须返 0（调用点据此 break，不空跑一轮）。
    ///
    /// 回退即 FAIL：把 `saturating_sub` 换成 `-` 会在 attempts > 12 时 panic；
    /// 把 `.min(remaining)` 删掉则第三、四条断言失败。
    #[test]
    fn round_retry_quota_shrinks_and_hits_zero() {
        let base = ABSOLUTE_MAX_TOTAL_RETRIES;
        assert_eq!(round_retry_quota(base, 0), base, "第 0 轮拿满基础配额");
        assert_eq!(round_retry_quota(base, 4), base - 4, "第 1 轮只剩 12-4");
        assert_eq!(
            round_retry_quota(base, ABSOLUTE_MAX_TOTAL_RETRIES as u32),
            0,
            "额度用尽必须返 0，否则调用点会空跑一轮、白睡一次退避"
        );
        // 超额（墙钟 break 后 attempts_base 可能越过上限）不得下溢 panic。
        assert_eq!(round_retry_quota(base, 999), 0);
        // 小号池：基础配额本就小于剩余额度时，不得被抬高。
        assert_eq!(round_retry_quota(2, 0), 2, "基础配额是上界，不能被额度抬高");

        // ⭐ 吸收层**关闭**时的逐字节等价（docs/absorb-layer-design.md §8）：只跑一轮 ⇒
        // attempts_base 恒 0；而 compute_max_retries 自身已 `.min(ABSOLUTE_MAX_TOTAL_RETRIES)`
        // ⇒ 本函数恒为恒等映射 ⇒ 关闭路径的行为与改动前完全相同。
        for pool in [0usize, 1, 3, 4, 12, 43, 1000] {
            let base = compute_max_retries(pool, pool);
            assert_eq!(
                round_retry_quota(base, 0),
                base,
                "吸收层关闭时（attempts_base 恒 0）本函数必须是恒等映射，池大小={pool}"
            );
        }
    }

    /// ⭐ 源码守卫：本轮配额必须**在** `'absorb: loop` 内经 `round_retry_quota` 算出。
    ///
    /// 纯函数单测证明不了热路径真的用了它（那正是「测了分支内部没测分支顺序」的形态）。
    /// 回退即 FAIL：把 `let max_retries = round_retry_quota(..)` 挪回循环外，
    /// 或改成直接用 `base_retry_quota` → 两条位置断言之一失败。
    #[test]
    fn per_round_quota_is_computed_inside_absorb_loop() {
        // ⚠️ 必须先切掉 `#[cfg(test)]` 之后的内容：`include_str!` 读整份源码，本测试自身
        // 也含这些 needle 的拼接结果，不切则位置比较命中测试里的那个 → 守卫静默失效
        // （前一版 `per_round_retry_cap_*` 正是这个形态：改完也照样通过）。
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let loop_marker = format!("{}{}", "'absorb: ", "loop {");
        // ⚠️ 第二个实参必须是 `upstream_calls`（真打上游的次数）而**不是** `attempts_base`
        // （迭代计数,含 fast-fail 空转）。喂错会让全池冷却在毫秒内烧空额度 ⇒ 吸收层被整体旁路,
        // 且这件事在纯函数单测里看不出来（两者类型相同、函数本身行为不变）。
        let quota_call = format!(
            "{}{}",
            "round_retry_quota(base_retry_quota", ", upstream_calls)"
        );
        let decl = format!("{}{}", "let max_retries = ", "round_retry_quota(");

        let loop_at = src
            .find(loop_marker.as_str())
            .expect("'absorb: loop 不应被改名");
        let decl_at = src
            .find(decl.as_str())
            .expect("本轮配额必须由 round_retry_quota 算出（跨轮共享总额度）");
        assert!(
            decl_at > loop_at,
            "本轮配额必须在 'absorb: loop **内**重算：算在循环外等于每轮各拿一份完整配额，\
             上限退化成「每轮」语义（max_rounds=3 时单请求最坏 48 次上游调用）"
        );
        assert!(
            src.contains(quota_call.as_str()),
            "配额必须同时喂入基础配额与**跨轮累计**尝试数，否则夹不住总量"
        );

        // 额度耗尽必须在 sleep 之前 break：否则每轮白睡一次退避却零次上游调用。
        let zero_gate = format!(
            "{}{}",
            "round_retry_quota(base_retry_quota, upstream_calls) ==", " 0"
        );
        let sleep_at = src
            .rfind(&format!("{}{}", "sleep(delay)", ".await"))
            .expect("吸收轮的 sleep 不应被改名");
        let zero_at = src
            .find(zero_gate.as_str())
            .expect("必须有「额度耗尽即 break」的闸门");
        assert!(
            zero_at < sleep_at,
            "额度耗尽的闸门必须排在 sleep 之前，否则客户端会为零次上游调用白等多个退避"
        );
    }

    /// ⭐ 未修问题 ③：退避被 `max_delay` 截断时**不得**再起一轮。
    ///
    /// 号池真值 60s（`SELF_HEAL_BASE_BACKOFF`）vs `max_delay` 默认 15s：只 clamp 不判断
    /// ⇒ 睡 15s 醒来池子还在冷却 45s ⇒ 这一轮结构上必然拿回同一个 429 = 白打一轮上游
    /// + 客户端白等 15s。
    ///
    /// 回退即 FAIL：把 `backoff_is_truncated` 改成 `required_wait > max_delay` 之外的任何
    /// 恒假式（如 `false`），第二、三条断言失败。
    #[test]
    fn truncated_backoff_means_round_is_futile() {
        use crate::anthropic::AbsorbClass;
        let p = AbsorbPolicy::from_config(&absorb_cfg(true));

        // 号池真值在退避上限之内 → 睡够就真到恢复时刻 → 这一轮有意义。
        assert!(
            !p.backoff_is_truncated(AbsorbClass::PoolCooldown(8), 0),
            "8s < max_delay，睡满即到恢复时刻，这一轮是有意义的"
        );
        // ⭐ 承重：全池自愈退避 60s 远超 max_delay ⇒ 必须判定「白打」。
        assert!(
            p.backoff_is_truncated(AbsorbClass::PoolCooldown(60), 0),
            "号池要 60s 才恢复而我们最多睡 {:?}，睡醒仍在冷却 —— 必须判白打",
            p.max_delay
        );
        // 而 clamp 后的睡眠时长看不出这件事（这正是必须分成两个函数的理由）。
        assert_eq!(
            p.backoff(AbsorbClass::PoolCooldown(60), 0),
            p.max_delay,
            "睡多久仍用截断值，判断够不够才用真值"
        );
        // ⭐ 反向承重：指数兜底撞上限**不算**白打。它是我们自己编的数、不是上游真值，
        // `max_delay` 本来就是为夹住它而存在。若这里判 true，吸收层会对**最主要**的那类
        // （上游裸 429）在 round 涨上去后提前停工，白丢一层保护。
        assert!(
            !p.backoff_is_truncated(AbsorbClass::UpstreamRateLimit, 30),
            "指数兜底无真值，撞 max_delay 只说明「我们不想睡更久」，不代表上游没好"
        );
        assert!(
            !p.backoff_is_truncated(AbsorbClass::SwapWindow, 30),
            "同上：SwapWindow（换号空窗）也没有号池真值"
        );
        // 新增两类同理：它们的曲线是我们自己编的数，撞上限不代表上游没好。
        assert!(!p.backoff_is_truncated(AbsorbClass::TransientServerError, 30));
        assert!(!p.backoff_is_truncated(AbsorbClass::TransientCapacity400, 30));
    }

    /// ⭐ 源码守卫（分支**顺序**）：截断判定必须排在 `should_start_another_round` **之前**。
    ///
    /// 两者是独立失败模式：前者管「睡够了上游好没好」，后者管「预算够不够睡」。
    /// 顺序反了的后果不是断言不成立而是**归因错**：预算判据用的是被截断的 15s（比真实
    /// 需求小），会先判「预算够」放行 → 白打一轮，且面板上记成 `absorb_round` 成功起轮
    /// 而不是被拦。回退即 FAIL：把 `backoff_is_truncated` 那段挪到
    /// `should_start_another_round` 之后，位置断言失败。
    #[test]
    fn truncation_gate_precedes_budget_gate() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let trunc = format!(
            "{}{}",
            "absorb.backoff_is_truncated", "(class, absorb_round)"
        );
        // ⚠️ 实参已从 `absorb_deadline` 改为 `class_deadline`（换号空窗要用它自己那份预算）。
        // 按实参定位是原设计，保留这种写法：它顺带钉住「预算闸门吃的是某个 deadline 变量」。
        let budget = format!("{}{}", "should_start_another_round", "(class_deadline");

        let trunc_at = src.find(trunc.as_str()).expect("截断闸门不应被改名/删除");
        let budget_at = src.find(budget.as_str()).expect("预算闸门不应被改名");
        assert!(
            trunc_at < budget_at,
            "截断闸门必须排在预算闸门之前：预算判据吃的是被 max_delay 夹小后的 delay，\
             先跑它会把「睡醒也没好」的一轮判成「预算够」而放行"
        );
    }

    /// ⭐ BLOCKER 1 的机械防线（源码级）：准入闸门必须在吸收循环**之上**，且全文只有一处。
    ///
    /// 回退即 FAIL：把 `acquire_admission` 移进 `'absorb: loop`（或在循环内再加一个调用点），
    /// 断言立刻失败。这是本方案唯一的正确性支点 —— 入站令牌是「每客户端请求一个」，
    /// 若吸收重入闸门，一条请求吃 N 个令牌 → 令牌桶按 N 倍速率被抽干 → 每轮排队满 30s 才
    /// bail → 客户端从 <2s 拿到 429 变成 60s 才拿到（外置 shield 的 p50 73.2s 被搬进网关）。
    /// 单测覆盖不到（需真实号池 + 上游），故用源码断言。
    #[test]
    fn admission_gate_must_stay_above_absorb_loop() {
        let src = include_str!("provider.rs");
        // needle 运行时拼接：写成完整字面量会被 include_str! 读到自己而多算一处。
        let gate = format!("{}{}", "acquire_admission", "().await");
        let loop_marker = format!("{}{}", "'absorb: ", "loop {");

        let gate_at = src
            .find(gate.as_str())
            .expect("acquire_admission 调用点不应被改名");
        let loop_at = src
            .find(loop_marker.as_str())
            .expect("'absorb: loop 不应被改名");
        assert!(
            gate_at < loop_at,
            "准入闸门必须在吸收循环之上：吸收重入 acquire_admission 会让一条客户端请求\
             吃 N 个入站令牌，把令牌桶按 N 倍速率抽干（设计评审 BLOCKER 1）"
        );
        assert_eq!(
            src.matches(gate.as_str()).count(),
            1,
            "acquire_admission 全文必须恰好一处调用点；新增第二处即意味着某条路径会重复扣令牌"
        );
    }

    /// 源码守卫：失败埋点与备用模型兜底必须留在吸收循环**之外**。
    ///
    /// 放进轮内会让一条客户端请求落 N 条失败记录 / 打 N 次备用模型，面板失败数被吸收轮次乘倍。
    ///
    /// ⚠️ 强度说明（避免把它当成比实际更硬的防线）：
    /// - `emit_record` 那一半**实际由编译器兜底** —— `fail_record` 在循环之后才构造，把
    ///   `emit_record(fail_record)` 挪进轮内会直接 E0425 `cannot find value`（已实测验证）。
    ///   本断言只是让意图显式化，真正拦住回退的是借用检查。
    /// - `overload_fallback_model` 那一半**是本测试独有的**：那段只依赖 `last_outcome` /
    ///   `model` / `session_id`，全都在循环内可见，搬进去能正常编译 —— 编译器不会报错，
    ///   只会静默变成"每轮都打一次备用模型"。这一半是这条测试存在的真正理由。
    #[test]
    fn emit_record_and_fallback_stay_outside_absorb_loop() {
        let src = include_str!("provider.rs");
        let retry_fn = src
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        let end_marker = format!("{}{}", "break ", "'absorb;");
        let last_break = retry_fn
            .rfind(end_marker.as_str())
            .expect("'absorb 循环的 break 不应被改名");

        // ⚠️ 锚点必须是 `emit_record(fail_record)` 而不是泛的 `emit_record(` ——
        // 准入闸门超时（已知问题 #20 的修复）也 emit 一条记录，而它**刻意**在吸收循环
        // **之上**（闸门本身就在循环外，见 `admission_timeout_must_be_observable`）。
        // 泛锚点会先命中那一处，把「位置在循环后」的断言判成失败，而实际并无回归。
        // 本测试要钉的是**失败记录**那一条：它按吸收轮次乘倍才会污染面板失败数。
        for needle in ["emit_record(fail_record)", "overload_fallback_model"] {
            let at = retry_fn
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} 应仍在 call_api_with_retry 内"));
            assert!(
                at > last_break,
                "{needle} 必须位于吸收循环之后（循环外）：放进轮内会让一条客户端请求\
                 落 N 条失败记录，面板失败数被吸收轮次乘倍"
            );
        }
    }

    /// ⭐ 源码守卫（已知问题 #13）：`failover_exhausted` 只能在吸收循环**之外**、整条客户端
    /// 请求失败后记一次。
    ///
    /// 历史缺陷：bump 放在轮内且每轮清零 ⇒ 一条请求跑 N 轮就计 N 次（多计）；成功路径在轮内
    /// return 前也会被误计。回退即 FAIL：把 bump 挪回 'absorb 循环内 → `bump_at < loop_at`。
    #[test]
    fn failover_exhausted_bumped_once_outside_absorb_loop() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let retry_fn = src
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        let loop_at = retry_fn
            .find(format!("{}{}", "'absorb: ", "loop {").as_str())
            .expect("'absorb: loop 不应被改名");
        let bump_at = retry_fn
            .find("crate::common::recovery_metrics::bump_failover_exhausted()")
            .expect("failover_exhausted bump 不应被删除");
        assert!(
            bump_at > loop_at,
            "failover_exhausted 必须在吸收循环之外记（一次/请求）：放在轮内会被吸收轮次乘倍（#13）"
        );
        assert_eq!(
            retry_fn
                .matches("crate::common::recovery_metrics::bump_failover_exhausted()")
                .count(),
            1,
            "call_api_with_retry 内必须恰好一处 failover_exhausted bump（整条请求失败才记一次）"
        );
    }

    /// ⭐ 源码守卫：链内去重集必须声明在吸收循环**之外**（跨轮共享）。
    ///
    /// 回退即 FAIL：把 `rate_limited_this_call` 的 `let mut` 挪进 `'absorb: loop`，断言失败。
    /// 挪进去会让同一个号在每一轮都被重新惩罚 → trigger_count 累加 → 冷却 15s 被指数拉长到
    /// 72s，即「单请求自造雪崩」（这条历史根因写在该集合的声明处注释里）。
    #[test]
    fn chain_dedup_sets_declared_outside_absorb_loop() {
        let src = include_str!("provider.rs");
        let retry_fn = src
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        let loop_at = retry_fn
            .find(format!("{}{}", "'absorb: ", "loop {").as_str())
            .expect("'absorb: loop 不应被改名");

        for set_name in [
            "let mut rate_limited_this_call",
            "let mut suspended_this_call",
            "let mut suspicious_failovers_this_call",
            "let mut auth_failed_this_call",
            "let mut region_corrected_this_call",
            // L1 换区：挪进轮内 ⇒ 每号一次上限退化成「每轮一次」，两个区来回打。
            "let mut region_switched_this_call",
            // L1 覆盖表：挪进轮内 ⇒ 上一轮换好的区在下一轮丢失，退回打错区。
            "let mut region_override_this_call",
            "let mut model_unavailable_attempts",
            "let mut attempts_used",
            // 挪进轮内会让每轮各拿一份完整 12 次上游调用额度 —— 那正是 round_retry_quota
            // 存在的理由（max_rounds=3 时单请求最坏 48 次上游调用、同一出口 IP）。
            "let mut upstream_calls",
        ] {
            let at = retry_fn
                .find(set_name)
                .unwrap_or_else(|| panic!("{set_name} 不应被改名/删除"));
            assert!(
                at < loop_at,
                "{set_name} 必须声明在吸收循环之外（跨轮共享）：挪进轮内会让同号被反复惩罚，\
                 冷却从 15s 指数拉长到 72s（单请求自造雪崩）"
            );
        }
    }

    /// ⭐ 源码守卫：四处 AIMD 上报点必须全部被 `absorb_round == 0` 包裹。
    ///
    /// 回退即 FAIL：去掉任一处的门，该处的上报数量断言失败。
    /// 依据：AIMD 的输入语义是「客户端请求撞上游的频率」，一条客户端请求无论吸收几轮都只是
    /// **一个** RPM 事件。逐轮上报时 `MD_DEBOUNCE_SECS=3` 挡不住吸收轮次（退避 ≥150ms、
    /// 号池真值常 8~15s，全部 >3s 穿窗）→ 每轮真降一档 → `last_md_nanos` 被反复推进 →
    /// `maybe_step_up` 的 20s 静默期永不满足（实测每 6.4s 一次 429）→ RPM 单调滑到 floor
    /// 锁死。这与已修的「AIMD 升档饿死」是同一死锁的第三条触发路径。
    #[test]
    fn aimd_reports_are_gated_to_first_absorb_round() {
        let src = include_str!("provider.rs");
        let retry_fn = src
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        // 只看到吸收循环收尾为止，避免把测试自身的字符串算进来。
        let body = retry_fn
            .split("mod tests")
            .next()
            .expect("测试模块分隔不应消失");
        let gate = format!("{}{}", "absorb_round ", "== 0");

        let sites = [
            "report_upstream_rate_limited()",
            "report_upstream_pressure()",
        ];
        let total: usize = sites.iter().map(|s| body.matches(s).count()).sum();
        assert_eq!(
            total, 4,
            "call_api_with_retry 内应恰有 4 处 AIMD 上报点（临时风控/suspend/429/5xx）；\
             数量变化需同步本守卫"
        );
        // 每处上报点之前的 200 字节窗口内必须出现 `absorb_round == 0` 这道门。
        // `split_at` 拿到该处之前的全部文本，再取尾部窗口 —— 门与调用之间只隔注释与花括号。
        for site in sites {
            let mut searched_from = 0usize;
            let mut nth = 0usize;
            while let Some(rel) = body[searched_from..].find(site) {
                let abs = searched_from + rel;
                nth += 1;
                // 取该处之前最多 200 字节的窗口。本文件含中文注释，字节偏移可能落在多字节
                // 字符中间 —— 必须往前挪到合法字符边界，**不能**回退成"整段前缀"
                // （那会把别处的门也算进来，使断言恒真：本守卫第一版就是这个 bug，
                //   删掉一处门后测试照样通过，等于白写）。
                let mut window_start = abs.saturating_sub(200);
                while window_start < abs && !body.is_char_boundary(window_start) {
                    window_start += 1;
                }
                let window = &body[window_start..abs];
                assert!(
                    window.contains(gate.as_str()),
                    "AIMD 上报点 {site}（第 {nth} 处）之前 200 字节内必须有 `absorb_round == 0` 门，\
                     否则吸收轮次会把同一个上游压力事件放大 N 倍喂给 AIMD，\
                     使 RPM 单调滑到 floor 锁死"
                );
                searched_from = abs + site.len();
            }
        }
    }

    // ══════════ P1-a：瞬态 bearer-invalid 403 的机器可读标记 ══════════

    /// 真实链路会产生的那条串（上游 body 取自 `region_probe.rs:130` 记录的实测形态）。
    /// 拼法与热路径的 `format!` 逐字节同构：`{api_type} API 请求失败（…）标记: {status} {body}`。
    const REAL_TRANSIENT_403: &str = r#"流式 API 请求失败（token 瞬态失效，已冷却换号）bearer_invalid_transient=1: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"The bearer token included in the request is invalid."}"#;

    /// ⭐ P1-a：瞬态那条 bail 必须带 `bearer_invalid_transient=1`，且**逐字节**如此。
    ///
    /// 为什么需要标记：这个二分（`has_ever_succeeded`）只有 provider 做得出 —— region 错配与
    /// 瞬态抖动的上游文案**完全相同**。handler 侧只看到字符串，会把已证明健康的号判成 region
    /// 坏（排障方向错），且状态码从 502（外挂 RETRYABLE 内、会重试）变成 403（4xx 不重试）。
    ///
    /// 回退即 FAIL（已实测）：把格式串里的 `bearer_invalid_transient=1` 删掉 →
    /// 第一条 `assert!(src.contains(...))` FAIL。
    #[test]
    fn transient_bearer_invalid_bail_carries_machine_readable_marker() {
        let full = include_str!("provider.rs");
        // 必须切掉测试模块：本测试自身含该字面量，不切则断言恒真（本仓「源码守卫静默失效」的老坑）。
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);

        // 逐字节钉死格式串前缀：标记名、大小写、位置（中文文案之后、冒号之前）全在内。
        // handlers 侧按精确字面量 `bearer_invalid_transient=1` 做排除，任何漂移都会让那条
        // 排除静默失效（编译不报错、测试若只匹配子串也发现不了）。
        let fmt = "API 请求失败（token 瞬态失效，已冷却换号）bearer_invalid_transient=1: {} {}";
        assert!(
            src.contains(fmt),
            "瞬态 bearer-invalid 403 的 bail 必须带 bearer_invalid_transient=1 标记，\
             且位置在中文文案之后、`: {{status}} {{body}}` 之前（handlers 侧按精确字面量排除）"
        );

        // 同款范式的既有标记都只有一处产生点，本条也应如此（多处产生 = 语义被稀释）。
        // ⚠️ 计数只能按**格式串**（带 `: {} {}` 尾巴）算，不能按裸标记名 —— 注释里也会提它，
        // 那样计数会把注释算进来，断言变成对注释文字的约束（本测试第一版即此形态，实测 left=2）。
        assert_eq!(
            src.matches(fmt).count(),
            1,
            "该标记应只有唯一产生点（瞬态分支）；多处产生会让 handler 侧的排除覆盖到别的语义"
        );

        // ⭐ 承重：这条串**确实**落在 region-mismatch 判据的射程内 —— 这才是标记必要的证明。
        // 直接调 endpoint 侧那个谓词（handlers 的 `is_upstream_region_mismatch_403` 就是
        // 「它 && 403 && 无 401」），不在本文件重写一份子串匹配。
        assert!(
            crate::kiro::endpoint::default_is_bearer_token_invalid(REAL_TRANSIENT_403),
            "前提：瞬态串必然命中 bearer-invalid 谓词（与 region 错配逐字节同文案）"
        );
        assert!(
            REAL_TRANSIENT_403.contains("403"),
            "前提：瞬态串带 403 语境"
        );
        assert!(
            !REAL_TRANSIENT_403.to_ascii_lowercase().contains("401"),
            "前提：瞬态串不含 401（否则 region 判据本就会让路，标记也就不必要了）"
        );
        // ⇒ 三个前提同时成立 = 不加标记时 region-mismatch 判据必然误命中。
        assert!(
            REAL_TRANSIENT_403.contains("bearer_invalid_transient=1"),
            "所以必须有一个 region 判据看得见的机器可读区分位"
        );
    }

    // ══════════ P1-b：额度只计真正打到上游的次数 ══════════

    /// ⭐ P1-b（行为）：全池冷却 fast-fail 一整轮**不得**消耗跨轮重试额度。
    ///
    /// 缺陷推导（已独立复核）：`compute_max_retries(pool,pool)` 在 pool≥4 时恒为 12；
    /// 全池冷却时 `all_cooling_fast_fail` 默认开、wait>2s ⇒ `acquire_context_excluding` 裸 bail
    /// ⇒ 热路径 `continue`（不 sleep、不打上游）⇒ 第 0 轮在毫秒级跑完 12 次迭代。
    /// 旧代码用迭代计数 `attempts_base`（= 11+1 = 12）喂额度闸门 ⇒ 闸门命中 ⇒ `break 'absorb`
    /// ⇒ `absorb_round` 恒 0，吸收层对 pool≥4 等于没开。
    ///
    /// 本测试用两种口径各跑一遍同一个「一轮全 fast-fail」剧本，断言只有「计上游调用」这一种
    /// 能让第 1 轮拿到非零配额。回退即 FAIL（已实测）：把热路径改回喂 `attempts_base` 时，
    /// 单靠本测试**不会**失败（它是纯函数模拟），故必须与下面的源码守卫成对存在 —— 那条才是
    /// 「测了分支内部没测分支顺序」的防线。
    #[test]
    fn fast_fail_round_must_not_consume_upstream_retry_quota() {
        let pool = 17usize; // 线上实测规模；任何 ≥4 都会撞满硬上限
        let base = compute_max_retries(pool, pool);
        assert_eq!(
            base, ABSOLUTE_MAX_TOTAL_RETRIES,
            "前提：pool={pool} 时基础配额吃满硬上限"
        );

        // 剧本：第 0 轮 max_retries 次迭代**全部**在 acquire 处 fast-fail（零次 send）。
        let round0_iterations = base;

        // 旧口径（迭代计数）：attempts_used = 0 + (n-1)，轮末 attempts_base = attempts_used + 1。
        let attempts_base_after_round0 = (round0_iterations - 1) as u32 + 1;
        assert_eq!(
            round_retry_quota(base, attempts_base_after_round0),
            0,
            "旧口径下一整轮 fast-fail 就把 12 个额度全烧光 ⇒ 额度闸门命中 ⇒ 吸收层被旁路"
        );

        // 新口径（真实上游调用数）：一轮全 fast-fail ⇒ 一次都没打上游 ⇒ 额度分毫未动。
        let upstream_calls_after_round0 = 0u32;
        assert_eq!(
            round_retry_quota(base, upstream_calls_after_round0),
            base,
            "fast-fail 不打上游，不该消耗「打上游」的额度 —— 否则 PoolCooldown（吸收层最该拦的\
             那一类）从来没被吸收过"
        );

        // ⭐ 反向承重：新口径**不能**把上限放开。真打上游时必须照样递减、照样收敛到 0。
        let mut upstream_calls = 0u32;
        let mut rounds = 0usize;
        loop {
            let quota = round_retry_quota(base, upstream_calls);
            if quota == 0 {
                break;
            }
            // 最坏情形：本轮把配额全花在真实上游调用上。
            upstream_calls += quota as u32;
            rounds += 1;
            assert!(rounds <= 64, "必须收敛，否则是无界重试");
        }
        assert_eq!(
            upstream_calls, ABSOLUTE_MAX_TOTAL_RETRIES as u32,
            "「每请求 ≤ {} 次上游调用」的不变量必须仍然成立（换口径不等于放开上限）",
            ABSOLUTE_MAX_TOTAL_RETRIES
        );
    }

    /// ⭐ P1-b（源码位置，**这条才是承重的**）：额度累加点必须在 `send()` **之后**。
    ///
    /// 纯函数模拟证明不了热路径喂的是哪个变量（那正是「测了分支内部没测分支顺序」的形态）。
    /// 回退即 FAIL（已实测）：把 `upstream_calls += 1;` 挪到 `for attempt` 循环顶部（即
    /// `attempts_used = ...` 旁边），位置断言失败 —— 那样它就退化成迭代计数，缺陷原样回归。
    #[test]
    fn retry_quota_counts_only_calls_that_reached_upstream() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);

        // ⚠️ 必须先切到 `call_api_with_retry` 内再定位 —— 全文 `request.send().await` 有三处
        // （MCP 路径 :732 最靠前、备用模型 :1976 最靠后）。在全文上 `find` 会锚到 MCP 那处，
        // 于是「把累加挪回循环顶部」这个正是要拦的回退**照样通过**（实测：本测试第一版只有
        // 第三条 acquire 断言抓到，send 断言静默为真）。这就是「测了分支内部没测分支顺序」。
        let retry_fn = src
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        let send_at = retry_fn
            .find(format!("{}{}", "request.send()", ".await").as_str())
            .expect("send 调用点不应被改名");
        let bump = format!("{}{}", "upstream_calls ", "+= 1;");
        let bump_at = retry_fn.find(bump.as_str()).expect("额度累加点不应被删除");
        assert!(
            bump_at > send_at,
            "额度累加必须在 send() 之后：放在循环顶部会把 acquire fast-fail 的空转也算成\
             一次上游调用 ⇒ 全池冷却时毫秒内烧空 12 个额度 ⇒ 吸收层整体旁路"
        );

        // 累加点必须唯一：多处累加会让同一次 send 扣多份额度（上限被隐式砍半）。
        assert_eq!(
            retry_fn.matches(bump.as_str()).count(),
            1,
            "额度累加点必须恰好一处，否则一次上游调用扣多份额度"
        );

        // 且必须排在 acquire 的 fast-fail `continue` 之后 —— 用 acquire 调用点做锚。
        let acquire_at = retry_fn
            .find("acquire_context_excluding(")
            .expect("acquire_context_excluding 调用点不应被改名");
        assert!(
            bump_at > acquire_at,
            "额度累加必须在 acquire 之后：acquire 失败的路径压根没打上游"
        );

        // 闸门与累加口径必须一致：喂 attempts_base 就等于缺陷回归（编译不报错）。
        let gate = format!(
            "{}{}",
            "round_retry_quota(base_retry_quota, upstream_calls) ==", " 0"
        );
        assert!(
            src.contains(gate.as_str()),
            "跨轮额度闸门必须按 upstream_calls 判定，与累加口径同源"
        );
    }

    /// ⭐ P1-b（分支**顺序**）：额度闸门必须排在截断闸门之前，且三道闸门顺序固定。
    ///
    /// 顺序在这里是承重的：三道都 `break 'absorb`，谁先求值决定了「这一轮为什么停」的归因，
    /// 也决定了截断闸门有没有机会被求值。缺陷期正是额度闸门（被 fast-fail 提前触发）
    /// 抢在截断闸门之前恒命中 ⇒ `:1844` 那条从来没跑过。
    ///
    /// 回退即 FAIL（已实测）：把额度闸门那段挪到 `backoff_is_truncated` 之后，第一条断言失败。
    #[test]
    fn quota_gate_precedes_truncation_and_budget_gates() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);

        let quota_at = src
            .find(
                format!(
                    "{}{}",
                    "round_retry_quota(base_retry_quota, upstream_calls) ==", " 0"
                )
                .as_str(),
            )
            .expect("额度闸门不应被改名");
        let trunc_at = src
            .find(
                format!(
                    "{}{}",
                    "absorb.backoff_is_truncated", "(class, absorb_round)"
                )
                .as_str(),
            )
            .expect("截断闸门不应被改名");
        // 实参已改为 `class_deadline`（换号空窗用它自己那份预算），见
        // `truncation_gate_precedes_budget_gate` 处的同款说明。
        let budget_at = src
            .find(format!("{}{}", "should_start_another_round", "(class_deadline").as_str())
            .expect("预算闸门不应被改名");

        assert!(
            quota_at < trunc_at,
            "额度闸门（每请求硬上限）必须最先求值：它是不可协商的安全上限，\
             而截断/预算闸门都是策略性放弃 —— 顺序反了会让硬上限被策略旁路"
        );
        assert!(
            trunc_at < budget_at,
            "截断闸门必须排在预算闸门之前（既有不变量，见 truncation_gate_precedes_budget_gate）"
        );
    }

    // ══════════ P1-c：三道 break 闸门的日志必须可分辨 ══════════

    /// ⭐ P1-c：三种停止吸收的结局必须在日志里**机器可分辨**，且各自点名旋钮。
    ///
    /// 背景：`:1845` 与 `:1859` 两个语义相反的闸门在 bump **同一个**
    /// `bump_absorb_budget_exhausted()` ⇒ 面板算出的吸收比无法归因 ⇒ 运维会去抬
    /// `upstreamRetryAbsorbBudgetSecs`，而真正该动的是 `upstreamRetryAbsorbMaxDelaySecs`。
    /// 而额度闸门连计数器都没有 ⇒ 主导结局在面板上完全不存在。
    /// 拆计数器要改 `recovery_metrics.rs`（不属本次改动范围），故先在日志侧收口。
    ///
    /// 回退即 FAIL（已实测）：删掉任一 `absorb_stop = "..."` 字段，对应断言失败。
    #[test]
    fn absorb_stop_reasons_are_distinguishable_in_logs() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);

        // 三个结局各有唯一的机器可读判据（不依赖中文文案不变）。
        for reason in [
            "retry_quota_exhausted",
            "backoff_truncated",
            "budget_too_small_for_round",
        ] {
            let field = format!("absorb_stop = {:?}", reason);
            assert_eq!(
                src.matches(field.as_str()).count(),
                1,
                "结局 {reason} 必须有且仅有一处 absorb_stop 标注：\
                 三道闸门都是 break 'absorb，没有机器可读判据时日志与面板都区分不出停在哪一道"
            );
        }

        // 两个共用计数器的闸门必须各自点名**不同**的旋钮 —— 这是归因混淆的实际危害面。
        assert!(
            src.contains("需抬 upstreamRetryAbsorbMaxDelaySecs"),
            "截断闸门必须点名 maxDelaySecs：它的瓶颈是「我们愿意睡的上限」小于号池真实恢复时刻"
        );
        assert!(
            src.contains("需抬 upstreamRetryAbsorbBudgetSecs"),
            "预算闸门必须点名 budgetSecs：它的瓶颈是总预算装不下一轮"
        );

        // ⭐ 归因混淆**已修**：三个结局各有独立计数器，本守卫随之从 `== 2` 改为 `== 1`。
        // ⚠️ 必须按**全路径调用**计数：短名在注释里也出现，按短名算会把注释计进来
        // （本测试第一版即此形态，实测 left=3 right=2）。
        assert_eq!(
            src.matches("crate::common::recovery_metrics::bump_absorb_budget_exhausted()")
                .count(),
            1,
            "`budget_exhausted` 现在**只**属于「总预算装不下一轮」这一个闸门。\
             另两个结局已各有独立计数器（backoff_truncated / retry_quota_exhausted）——\
             若这里又变回 2，说明有人把某个闸门重新并回了这个桶，归因混淆会复发"
        );
        // 另两个结局各有且仅有一处 bump（拆分是否真落到调用点，而不只是声明了计数器）。
        for call in [
            "crate::common::recovery_metrics::bump_absorb_backoff_truncated()",
            "crate::common::recovery_metrics::bump_absorb_retry_quota_exhausted()",
        ] {
            assert_eq!(
                src.matches(call).count(),
                1,
                "{call} 必须有且仅有一处调用（拆了计数器却漏改调用点是本仓已发生过的形态）"
            );
        }
    }

    /// ⭐ 硬约束守卫：**默认配置下三个新类别一律不吸收**。
    ///
    /// 线上正在服务，新能力必须靠显式开启。判据收在 `class_allowed` 一处（散写 `if` 必然漏
    /// 一处，而漏掉那处的表现正是「默认关的类别其实在吸收」）。
    ///
    /// 回退验证：把 `class_allowed` 里 `AbsorbClass::TransientServerError => self.absorb_server_error`
    /// 改成 `=> true` → 本测试 FAILED。
    #[test]
    fn new_absorb_classes_are_all_gated_off_by_default() {
        use crate::anthropic::AbsorbClass;
        // 总开关开着（否则 effective_max_rounds()=0，测不到类别闸门本身）。
        let p = AbsorbPolicy::from_config(&absorb_cfg(true));

        assert!(
            !p.class_allowed(AbsorbClass::SwapWindow),
            "换号空窗默认不吸收（upstreamRetryAbsorbSuspended 默认 false）"
        );
        assert!(
            !p.class_allowed(AbsorbClass::TransientServerError),
            "5xx 默认不吸收：外挂实测 11.6 次重试才救回 1 个请求，那是不分机理一律重试的账单"
        );
        assert!(
            !p.class_allowed(AbsorbClass::TransientCapacity400),
            "容量 400 默认不吸收"
        );
        // 原有两类跟着总开关走，行为不变（否则本改动会把吸收层的既有作用对象也关掉）。
        assert!(p.class_allowed(AbsorbClass::PoolCooldown(3)));
        assert!(p.class_allowed(AbsorbClass::UpstreamRateLimit));

        // 显式开启必须真生效，否则这些开关等于不存在。
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_server_error = true;
        c.upstream_retry_absorb_capacity_400 = true;
        c.upstream_retry_absorb_suspended = true;
        let on = AbsorbPolicy::from_config(&c);
        assert!(on.class_allowed(AbsorbClass::TransientServerError));
        assert!(on.class_allowed(AbsorbClass::TransientCapacity400));
        assert!(on.class_allowed(AbsorbClass::SwapWindow));
    }

    /// ⭐ 合并外挂缺口 3：换号空窗需要**完全不同的退避节奏**。
    ///
    /// 外挂原文：「KiroStudio 换号（auto_disable + 切下一个凭据 + 推送补号）实测有约 10 分钟的
    /// 空窗……**绝不能用限速那套 1 秒退避** —— 那是拿一个已被封的账号去猛打上游，只会加重风控。」
    ///
    /// 回退验证：把 `required_wait` 里 SwapWindow 的 `if self.swap_budget.is_zero()` 分支删掉
    /// （只留指数曲线）→ 本测试 FAILED。
    #[test]
    fn swap_window_uses_long_ladder_only_when_budget_configured() {
        use crate::anthropic::AbsorbClass;

        // ① 默认（swap 预算 0）：与限速同曲线 ⇒ 逐字节等于本字段引入前的行为。
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_suspended = true;
        let old = AbsorbPolicy::from_config(&c);
        for round in 0..3 {
            assert_eq!(
                old.required_wait(AbsorbClass::SwapWindow, round),
                old.required_wait(AbsorbClass::UpstreamRateLimit, round),
                "未设 swap 预算时必须沿用旧曲线（默认不改变现有行为）"
            );
        }
        assert_eq!(
            old.class_max_delay(AbsorbClass::SwapWindow),
            old.max_delay,
            "未设 swap 预算时上界不得被放宽"
        );

        // ② 设了 swap 预算：换成 20/40/60s 长阶梯，且超表长取最后一档。
        c.upstream_retry_absorb_swap_budget_secs = 600;
        let laddered = AbsorbPolicy::from_config(&c);
        for (round, want) in [(0u32, 20u64), (1, 40), (2, 60), (7, 60)] {
            assert_eq!(
                laddered.required_wait(AbsorbClass::SwapWindow, round),
                Duration::from_secs(want),
                "第 {round} 轮应睡 {want}s（外挂 SWAP_BACKOFF 阶梯）"
            );
        }
        // ⭐ 承重：长阶梯**不能被默认 15s 的全局上限削回** —— 否则这个旋钮等于没接上，
        // 且 `backoff_is_truncated` 只对 PoolCooldown 成立，不会拦住这种「睡不够」。
        assert_eq!(
            laddered.backoff(AbsorbClass::SwapWindow, 0),
            Duration::from_secs(20),
            "20s 阶梯必须真的睡 20s（max_delay 默认 15s，不放宽上界就会被削成 15s）"
        );

        // ⭐ 其它类别的上界**不得**被这个旋钮波及（只放宽换号空窗那一类）。
        assert_eq!(
            laddered.class_max_delay(AbsorbClass::UpstreamRateLimit),
            laddered.max_delay
        );
        assert_eq!(
            laddered.class_max_delay(AbsorbClass::TransientServerError),
            laddered.max_delay
        );
    }

    /// 新增两类的退避曲线：5xx 短（1s 起）、容量类中等（2s 起）。
    ///
    /// 回退验证：把 `TransientServerError` 的 `BASE` 从 1s 改成 2s（与容量类同曲线）→ FAILED。
    /// 两条曲线必须**可区分**：5xx 多为瞬时抖动，容量类是全局状态、换号不解决问题。
    #[test]
    fn transient_5xx_backs_off_shorter_than_capacity_class() {
        use crate::anthropic::AbsorbClass;
        let mut c = absorb_cfg(true);
        // 抬高上界，让曲线本身可见（默认 15s 会把两条都 clamp 到同一个值）。
        c.upstream_retry_absorb_max_delay_secs = 300;
        let p = AbsorbPolicy::from_config(&c);

        assert_eq!(
            p.required_wait(AbsorbClass::TransientServerError, 0),
            Duration::from_secs(1),
            "5xx 起步 1s（逐字取自外挂 MIN_DELAY=1.0）"
        );
        assert_eq!(
            p.required_wait(AbsorbClass::TransientCapacity400, 0),
            Duration::from_secs(2),
            "容量类起步 2s：全局容量问题，换号不解决，比 5xx 更该慢"
        );
        for round in 0..4 {
            assert!(
                p.required_wait(AbsorbClass::TransientServerError, round)
                    < p.required_wait(AbsorbClass::TransientCapacity400, round),
                "第 {round} 轮：5xx 必须严格短于容量类（两类曲线不得退化成同一条）"
            );
        }
    }

    /// ⭐ 换号空窗的**独立 deadline**：只有它拿那份更宽的预算，其余类别一律用总预算。
    ///
    /// 回退验证：把 `class_deadline` 的 `matches!(..., SwapWindow)` 条件删掉（所有类别都用
    /// swap 预算）→ 本测试 FAILED。那会让**所有**类别都能占着客户端连接十分钟，
    /// 而换号空窗恰恰是唯一等得起的一类。
    #[test]
    fn swap_budget_deadline_does_not_leak_to_other_classes() {
        use crate::anthropic::AbsorbClass;
        let now = std::time::Instant::now();
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_suspended = true;
        c.upstream_retry_absorb_swap_budget_secs = 600;
        let p = AbsorbPolicy::from_config(&c);

        assert_eq!(
            p.class_deadline(now, AbsorbClass::SwapWindow),
            now + Duration::from_secs(600),
            "换号空窗必须用它自己那份预算（空窗实测 10 分钟 ≫ 总预算 20~45s）"
        );
        for other in [
            AbsorbClass::PoolCooldown(5),
            AbsorbClass::UpstreamRateLimit,
            AbsorbClass::TransientServerError,
            AbsorbClass::TransientCapacity400,
        ] {
            assert_eq!(
                p.class_deadline(now, other),
                now + p.budget,
                "{other:?} 必须仍用总预算 —— swap 预算泄漏给其它类别 = 所有请求都可能长挂十分钟"
            );
        }

        // 未设 swap 预算时，换号空窗也回到总预算（默认不改变现有行为）。
        c.upstream_retry_absorb_swap_budget_secs = 0;
        let old = AbsorbPolicy::from_config(&c);
        assert_eq!(
            old.class_deadline(now, AbsorbClass::SwapWindow),
            now + old.budget
        );
    }

    /// ⭐ 「额外轮次钉 1」的解除条件：**只在设了 swap 预算时**解除。
    ///
    /// 钉 1 的前提是短退避（15s 内重打同一个刚被风控的账号会抵消 `SELF_HEAL_BASE_BACKOFF=60s`）。
    /// 长阶梯最短一档就是 20s，前提不再成立。不解除的话这个旋钮基本没用：它只能把**一次**
    /// 重试推迟到 20s 后，而空窗实测 10 分钟 ⇒ 那一次几乎必然还在窗口内。
    ///
    /// 回退验证：把 `from_config` 里的 `&& swap_budget.is_zero()` 删掉 → 第一条断言 FAILED
    /// （存量 `suspended=true` 的部署会从 1 轮变成 3 轮，属默认行为变更）。
    #[test]
    fn suspended_round_pin_released_only_with_swap_budget() {
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_suspended = true;
        assert_eq!(
            AbsorbPolicy::from_config(&c).effective_max_rounds(),
            1,
            "未设 swap 预算时必须仍钉 1（存量 suspended=true 的部署行为逐字节不变）"
        );

        c.upstream_retry_absorb_swap_budget_secs = 600;
        assert_eq!(
            AbsorbPolicy::from_config(&c).effective_max_rounds(),
            c.upstream_retry_absorb_max_rounds,
            "设了 swap 预算即解除钉 1，交回 max_rounds + 独立 deadline + 总额度三道闸"
        );

        // 总开关关闭时一切照旧恒 0（这条是吸收层「关 ⇒ 逐字节等价旧行为」的根）。
        let mut off = absorb_cfg(false);
        off.upstream_retry_absorb_suspended = true;
        off.upstream_retry_absorb_swap_budget_secs = 600;
        assert_eq!(AbsorbPolicy::from_config(&off).effective_max_rounds(), 0);
    }

    /// ⭐ 缺口 4 的 provider 侧：**只在吸收层真跑过并放弃、且配置为 503 时**打标记。
    ///
    /// 源码级守卫（走到那段需要真实上游 + 真实号池，行为测试写不了 —— 本仓惯例）。
    ///
    /// 回退验证：把 `exhausted_as_503` 的判据从 `== 503` 改成 `!= 429`，或把
    /// `absorb_gave_up_after_rounds |= absorb_round > 0` 里的限定去掉 → 对应断言 FAILED。
    #[test]
    fn exhausted_503_marker_is_gated_on_both_conditions() {
        let src = include_str!("provider.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(src);

        // ① 只认精确的 503：其它值（含裸 serde default 会给的 0）一律按 429 处理。
        assert!(
            prod.contains("cfg.upstream_retry_absorb_exhausted_status == 503"),
            "必须只认精确 503 —— 打一个 handlers 认不出的标记只会造成静默的行为分叉"
        );
        // ② 标记必须同时受「真跑过轮次」约束：一次都没重试就改状态码是说谎。
        assert!(
            prod.contains("absorb_gave_up_after_rounds && absorb.exhausted_as_503"),
            "标记必须两个条件都满足才打（跑过轮次 且 配置为 503）"
        );
        // ③ 每处置位都带 `absorb_round > 0` 限定 —— 关闭吸收层时这里恒 0 ⇒ 不置位 ⇒
        //    渲染路径逐字节不变。这是「默认不改变现有行为」的机制本身。
        let sets = prod
            .matches("absorb_gave_up_after_rounds |= absorb_round > 0")
            .count();
        assert!(
            sets >= 3,
            "三条放弃结局（轮次用尽 / 额度用尽 / 退避被截断）都应置位，当前 {sets} 处"
        );
        assert!(
            !prod.contains("absorb_gave_up_after_rounds = true"),
            "不得无条件置位：那会让「吸收层没开也返 503」，等于对客户端说谎"
        );
    }

    /// 每个 `AbsorbClass` 都必须能在计数器上分辨（否则上线后无法判断哪类在起作用）。
    ///
    /// 回退验证：删掉 `bump_absorb_round_swap_window()` 那一处调用 → FAILED。
    #[test]
    fn every_absorb_class_has_a_distinguishable_counter() {
        let src = include_str!("provider.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(src);
        for call in [
            "bump_absorb_round_pool_cooldown()",
            "bump_absorb_round_rate_limit()",
            "bump_absorb_round_swap_window()",
            "bump_absorb_round_server_error()",
            "bump_absorb_round_capacity_400()",
            "bump_absorb_server_error_skipped()",
            "bump_absorb_capacity_400_skipped()",
        ] {
            assert!(
                prod.contains(call),
                "{call} 必须被调用：五类共用一个 absorb_rounds 时，开三个开关后面板上仍是\
                 一个数 ⇒ 无法归因，也就无法决定该关掉哪个"
            );
        }
    }

    // ══════════ L1/L2：对话路径 region 自纠正 ══════════

    /// 真实链路会产生的 403 body（`region_probe.rs:130` 记录的实测形态，与
    /// `REAL_TRANSIENT_403` 里嵌的那段 body 逐字节同源）。
    ///
    /// 用它而不是自编串：上一轮审查抓到过「用合成串测试，而真实链路不产生那种串」——
    /// 那种测试全绿而线上判据全部漏命中。
    const REAL_BEARER_INVALID_BODY: &str = r#"{"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"The bearer token included in the request is invalid."}"#;

    /// L1 主用例：**从未成功过**的 `api_key` 号吃 region 错配 403 ⇒ 必须换区（而非换号）。
    ///
    /// 回退即 FAIL：把 `region_retry_target` 的 `has_ever_succeeded` 取反，或让它恒返
    /// `None` → 第二条断言 FAILED（拿不到目标区 = 热路径不会 `continue` 换区，
    /// 落到下方 `report_failure` + failover 换号，而换号治不了 region 错配）。
    #[test]
    fn never_succeeded_api_key_with_region_mismatch_403_switches_region() {
        // 前提：这条真实 body 确实命中热路径那道谓词（否则本测试测的不是同一条路）。
        assert!(
            crate::kiro::endpoint::default_is_bearer_token_invalid(REAL_BEARER_INVALID_BODY),
            "前提：真实 403 body 必须命中 is_bearer_token_invalid，否则热路径根本进不了该分支"
        );

        let target = region_retry_target("eu-central-1", true, false);
        assert_eq!(
            target,
            Some("us-east-1"),
            "从未成功过的 api_key 号打错区 ⇒ 必须换到**另一个**候选区；\
             返 None 就是回到「当凭据问题换号」的旧行为，而换号解决不了 region 错配"
        );

        // 反向也成立（US 号被探测写成 eu 是实测形态，但反过来同样要能纠）。
        assert_eq!(
            region_retry_target("us-east-1", true, false),
            Some("eu-central-1"),
            "换区必须是双向的，否则只能纠正一个方向"
        );
    }

    /// L1 收窄用例：**已成功过**的号吃**同一条** 403 ⇒ 必须**不**换区。
    ///
    /// 这是 L1 与既有 `bearer_invalid_but_proven` 的分界线：同一句上游文案，
    /// `has_ever_succeeded` 是唯一区分位。已成功过 = 这个区真拿到过 200 ⇒ 区是对的，
    /// 403 只能是抖动（实测 4 个号累计 3393 次成功、共吃 42 次）⇒ 该走瞬态分支。
    ///
    /// 回退即 FAIL：把 `region_retry_target` 里的 `|| has_ever_succeeded` 删掉 → 断言 FAILED
    /// （已证明健康的号会被换区 = 把一个本来对的配置改坏，且下一次抖动过去它本来就好了）。
    #[test]
    fn proven_credential_with_same_403_must_not_switch_region() {
        assert_eq!(
            region_retry_target("eu-central-1", true, true),
            None,
            "已成功过的号必须让路给既有瞬态分支（冷却+换号、不计失败），绝不换区"
        );
    }

    /// L2 的门：OAuth 号不换区、也就不回写 `api_region`。
    ///
    /// 依据：OAuth 号的权威 region 是 `profileArn` 第 4 段（`effective_upstream_region`
    /// 第一优先），`api_region` 对它根本不生效 ⇒ 换区不改变实际 host（白烧一次额度），
    /// 回写则在面板上留一个"看起来生效其实被压住"的值，把排障带偏。
    ///
    /// 回退即 FAIL：删掉 `region_retry_target` 里的 `!is_api_key` 门 → 断言 FAILED。
    #[test]
    fn oauth_credential_must_not_switch_or_write_back_region() {
        assert_eq!(
            region_retry_target("eu-central-1", false, false),
            None,
            "OAuth 号的 region 由 profileArn 决定，换区/回写 api_region 对它无效"
        );

        // 回写点必须**显式**带 `is_api_key_credential` 门（第二道）：入口那道门若被放宽，
        // 这里仍不能把 OAuth 号的 api_region 写坏。
        let src = include_str!("provider.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(src);
        let writeback = format!("{}{}", "set_credential_api_region", "(ctx.id");
        let at = prod
            .find(writeback.as_str())
            .expect("L2 回写调用点不应被改名/删除");
        // 回写之前的窗口内必须出现 api_key 门。窗口取 600 字节（中间隔着注释）。
        // ⚠️ 必须挪到合法字符边界：本文件含中文注释，裸切会 panic；而回退成"整段前缀"
        // 会让断言恒真（别处的门也被算进来），那等于白写。
        let mut window_start = at.saturating_sub(600);
        while window_start < at && !prod.is_char_boundary(window_start) {
            window_start += 1;
        }
        assert!(
            prod[window_start..at].contains("is_api_key_credential()"),
            "L2 回写点前必须有 is_api_key_credential 门，否则 OAuth 号会被写进一个不生效的 api_region"
        );
    }

    /// 候选表的形状假设：只有两项，且首项 `eu-central-1`。
    ///
    /// 实测依据：`management.*` 与 `runtime.*` 只在 `us-east-1` / `eu-central-1` 解析 DNS。
    /// 表若被扩项，`region_retry_target` 的「换到另一个」就退化成「顺序轮换」——
    /// 语义变了，本测试会 FAIL 以强制重新审视。
    #[test]
    fn region_retry_falls_back_to_first_candidate_when_current_is_off_table() {
        assert_eq!(
            crate::kiro::region_probe::PROBE_ORDER.len(),
            2,
            "前提：候选只有两个（实测只有这两区解析 DNS）。扩表需重新审视 region_retry_target 的语义"
        );
        // 当前区不在表内（真实成因：profileArn 把区钉在 us-west-2）⇒ 换到表首项。
        assert_eq!(
            region_retry_target("us-west-2", true, false),
            Some(crate::kiro::region_probe::PROBE_ORDER[0]),
            "当前区不在候选表内时必须落到表首项，而不是返 None（那样该号永远纠不过来）"
        );
    }

    /// 🔴 **顺序断言**：换区分支必须排在 `bearer_invalid_transient` 之后、401 之后。
    ///
    /// 为什么必须有这条：本仓「纸面测试」第 8 种形态 —— **测了分支内部，没测分支顺序**。
    /// 真实事故：改三处、四条测试、三次「回退即 FAILED」全过而修复无效，因为一条通用分支
    /// 排在特化分支之前先 `break` 了。上面那几条纯函数测试对顺序**完全不可见**：
    /// `region_retry_target` 可以完美无缺而热路径根本走不到它。
    ///
    /// 断言的是**最终行为**（换区 vs 换号），三条各自钉一个会让行为反转的顺序关系：
    /// ① 瞬态分支在前 ⇒ 已成功过的号在到达换区分支**之前**就被 `continue` 掉；
    /// ② 换区分支带 403 门 ⇒ 401 落不进来（401 该 force-refresh/计失败，换区对它无用）；
    /// ③ 换区分支在通用 `report_failure` 之前 ⇒ region 错配的号走的是换区，不是换号 + 计失败。
    #[test]
    fn region_switch_branch_ordered_after_transient_and_401() {
        let src = include_str!("provider.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(src);
        let retry_fn = prod
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");

        // needle 运行时拼接：完整字面量会被 include_str! 读到自己而自匹配（本文件已踩三次）。
        let transient_guard = format!("{}{}", "bearer_invalid_but", "_proven");
        let transient_marker = format!("{}{}", "bearer_invalid_", "transient=1");
        let region_guard = format!("{}{}", "region_switched_", "this_call.contains");
        let punish = format!("{}{}", "report_failure", "(ctx.id)");

        let transient_at = retry_fn
            .find(transient_guard.as_str())
            .expect("既有瞬态判定不应被改名");
        let marker_at = retry_fn
            .find(transient_marker.as_str())
            .expect("瞬态机器可读标记不应被删");
        let region_at = retry_fn
            .find(region_guard.as_str())
            .expect("换区分支的每号一次门不应被改名");
        let punish_at = retry_fn
            .rfind(punish.as_str())
            .expect("通用 401/403 的 report_failure 不应被改名");

        // ① 瞬态在前：已成功过的号必须在换区分支之前就被 continue 掉。
        // 顺序反了 ⇒ 已证明健康的号（区是对的）会被换区，把对的配置改坏。
        assert!(
            transient_at < region_at && marker_at < region_at,
            "换区分支必须排在 bearer_invalid_transient 之后：\
             顺序反了会让已成功过的号（区本来是对的）被换区，且瞬态标记再也打不出来"
        );

        // ② 401 让路：换区分支的判据里必须带 403 门。
        // 取该分支起点前的窗口，断言 403 门与它同处一条 `if` 条件里。
        let mut window_start = region_at.saturating_sub(200);
        while window_start < region_at && !retry_fn.is_char_boundary(window_start) {
            window_start += 1;
        }
        assert!(
            retry_fn[window_start..region_at].contains("status.as_u16() == 403"),
            "换区分支必须带 403 门（401 让路）：401 是 token 死了 ≠ 区错了，\
             换个区照样是死 token，只会白烧一次重试额度并延后真正的 force-refresh"
        );

        // ③ 换区在计失败之前：region 配错≠号坏（隔离铁律）。
        // 顺序反了 ⇒ 号先被 report_failure（累计 3 次即禁用），换区永远轮不到，
        // 即回到「US 号导入即废」那个形态。
        assert!(
            region_at < punish_at,
            "换区分支必须排在通用 report_failure 之前：反了则 region 错配的号先被计失败\
             （3 次即禁用），换区分支永远走不到"
        );

        // 换区分支**绝不能**调用 report_failure / 冷却：那是「号坏了」的处置。
        // 取该分支体的一段窗口（到下一处 `continue;` 为止）做否定断言。
        let branch_body = &retry_fn[region_at..];
        let branch_end = branch_body
            .find("// 同一个号在一条请求里只惩罚一次")
            .expect("换区分支与通用惩罚分支之间的注释锚点不应消失");
        let branch = &branch_body[..branch_end];
        assert!(
            !branch.contains(punish.as_str()),
            "换区分支内绝不能 report_failure：region 配错≠号坏，惩罚它会把一个其实好的号推向禁用"
        );
    }

    /// L1 上限：同一个号在一次客户端请求内**最多换区一次**。
    ///
    /// 不加上限就是两个区来回打（A 403 → 换 B → B 403 → 换回 A → …），一条客户端请求
    /// 把额度全烧在同一个号的两个区之间、同一出口 IP 连打 = 正是风控要抓的突发特征。
    /// 本仓刚因「吸收层放大」修过一轮。
    ///
    /// 回退即 FAIL：删掉 `!region_switched_this_call.contains(&ctx.id)` 这道门 → 第一条
    /// 断言 FAILED；把那个集合的 `let mut` 挪进 `'absorb: loop` → 第三条 FAILED
    /// （挪进去 ⇒ 每一轮各拿一份新集合 ⇒ 上限退化成「每轮一次」，吸收 3 轮就是 4 次）。
    #[test]
    fn region_switch_capped_once_per_credential_per_call() {
        let src = include_str!("provider.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(src);
        let retry_fn = prod
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");

        let gate = format!("!{}{}", "region_switched_this_call", ".contains(&ctx.id)");
        assert!(
            retry_fn.contains(gate.as_str()),
            "必须有 per-call 的每号一次门，否则同一个号会在两个区之间来回打、烧光重试额度"
        );
        let mark = format!("{}{}", "region_switched_this_call", ".insert(ctx.id)");
        assert!(
            retry_fn.contains(mark.as_str()),
            "命中换区后必须置位，否则那道 contains 门恒不成立 = 等于没有上限"
        );

        // 集合必须声明在吸收循环**之外**（跨轮共享），否则上限退化成「每轮一次」。
        let decl = format!("let mut {}", "region_switched_this_call");
        let decl_at = retry_fn.find(decl.as_str()).expect("集合声明不应被改名");
        let loop_at = retry_fn
            .find(format!("{}{}", "'absorb: ", "loop {").as_str())
            .expect("'absorb: loop 不应被改名");
        assert!(
            decl_at < loop_at,
            "换区去重集必须声明在吸收循环之外：挪进轮内 ⇒ 每轮各拿一份 ⇒ 上限退化成\
             「每轮一次」，吸收 3 轮就是 4 次换区"
        );

        // 换区后必须把该号从 tried_this_call 摘掉，否则下一跳会结构性避开它 ⇒
        // 覆盖值躺在 map 里没人用 = 换区等于没做（这是最容易静默失效的一处）。
        let unexclude = format!("{}{}", "tried_this_call", ".remove(&ctx.id)");
        assert!(
            retry_fn.contains(unexclude.as_str()),
            "换区后必须把该号从 tried_this_call 摘掉，否则 acquire_context_excluding 会避开它，\
             换区重试打的是别人的号 —— 覆盖值没人用，等于没换区"
        );
    }
}
