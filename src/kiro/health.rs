//! 号池/族级健康评分 + 熔断半开渐进放回（HealthTracker）
//!
//! 纯本地内存、零上游调用。与 [`crate::kiro::cooldown`]（硬退场时间窗）、
//! [`crate::kiro::scheduling::RpmTracker`]（RPM 滚动窗）、token_manager 的 `report_*` 派发并存，
//! 给 balanced 选号提供一个连续的 `p_avail` 权重（可用概率），并在冷却硬窗过后**逐步试探放回**
//! （熔断器 half-open），而非"冷却一到就全量涌回把刚缓过来的号/族又打进风控"。
//!
//! ## 键 = family_key（族/号同表同算法）
//! 键由调用方按 [`crate::kiro::model::credentials::KiroCredentials::family_key`] 派生：
//! - M365 号 → `m365:{tenant}` / `aws:{account}`（整族连坐一个 HealthState）
//! - IdC/social/api_key → `cred:{id}`（各自独立健康，坚强兜底不受 M365 连坐波及）
//!
//! ## 与 CooldownManager 的分工
//! - Cooldown = 硬退场布尔门（`is_available` 决定此刻能否被选，硬跳过）。
//! - Health   = 到期后的软放回 + 连续权重（half-open 概率放行 + p_avail 排序权重）。
//! - 二者取并集：能全速选 ⇔ cooldown 可用 且 circuit==Closed；半开期由 p_avail 的 gate=admit_prob
//!   做概率软放行（只进 balanced 排序键，**不进 is_entry_selectable 硬门**，避免双重硬挡误伤兜底号）。
//!
//! ## 无定时器（惰性推进）
//! 不开后台线程。每次 on_success/on_429/report_family_suspicious/p_avail 进临界区第一步都
//! `tick_circuit` 按墙钟把到期的 Open 推进到 HalfOpen；无访问的条目停在原状态，由 `cleanup` 淘汰。

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const A_SUCCESS: f64 = 0.3; // ewma_success 平滑系数（慢升，抗抖动）
const A_429: f64 = 0.5; // ewma_429 平滑系数（快升，429 敏感）
const HEALTH_429_WEIGHT: f64 = 0.6; // health = ewma_success*(1-0.6*ewma_429)
const LOAD_PENALTY: f64 = 0.5; // p_avail 里 (1-0.5*load)
/// inflight 归一参考的**下限**（小池语义：>=8 在途视为满载）。
///
/// ⚠️ 这**不再是**固定归一分母，只是自适应基准的地板值——见 [`adaptive_load_ref`]。
/// 保留 8.0 保证小池（个人/小团队）行为与历史完全一致（零回归）。
const LOAD_REF: f64 = 8.0;
/// 自适应基准相对池内平均在途的放大系数。
///
/// 取 2.0 的理由：让「在途 = 池平均」的号落在 load=0.5（中位惩罚），
/// 「在途 = 2× 平均」的号落到 load=1.0（满惩罚）。若取 1.0，平均负载的号就已 load=1.0，
/// 与它更忙的号无法区分——那正是固定 LOAD_REF=8 在企业级失效的形态。
const LOAD_REF_MEAN_FACTOR: f64 = 2.0;

/// 自适应 inflight 归一参考：`max(LOAD_REF, 池内平均在途 × LOAD_REF_MEAN_FACTOR)`。
///
/// ## 为什么必须自适应（实测确证）
/// 固定 `LOAD_REF = 8.0` 时 `load = min(inflight/8, 1.0)`，而由 Little 法则
/// （并发在途 = RPM/60 × 上游延迟，实测延迟 p50=6.3s / p90=17.1s）：
///   - 10000 RPM 需 ~400 号 / ~1042(p50)~2845(p90) 并发在途
///   - p90 延迟下 6000 RPM / 200 号 → **每号常态在途 8.6**
/// 于是全池所有号的 `load` 同时 clamp 到 1.0 → `p_avail` 的 `(1 - LOAD_PENALTY*load)`
/// 退化成常数 0.5 → **负载维度整体失效**，`p_avail` 再也分不清"在途 9"和"在途 50"的号。
/// 中型规模（1000 RPM / 40 号 → 在途 7.1，load=0.89）就已逼近失效。
///
/// ## 如何避免"自适应经典陷阱"
/// 陷阱：基准跟着池负载一起涨 → 相对比值不变 → 压根不惩罚负载。
/// 本实现用 `max(地板, 平均×2)` 而非纯平均，于是：
///   - **小池/低负载**（平均 ≤4）→ 基准恒为地板 8.0，与历史逐位相同（零回归）
///   - **高负载**（平均 >4）→ 基准随平均放大，池内**相对**负载差异始终可分辨
/// 注意本项的目标是"同池内比较各号谁更闲"（排序用），绝对过载保护由
/// RPM 饱和硬门（`effective_saturation_limit`）与入站整形独立承担，不依赖此项。
///
/// ## 确定性
/// 基准由调用方在**一次选号开始时算一次**并传给所有候选（见 token_manager 的 sort_key），
/// 保证同一轮内所有候选用同一个分母 → 排序键仍是稳定全序，`min_by_key` 比较器保持传递性。
pub fn adaptive_load_ref(total_inflight: u64, cred_count: usize) -> f64 {
    if cred_count == 0 {
        return LOAD_REF;
    }
    let mean = total_inflight as f64 / cred_count as f64;
    (mean * LOAD_REF_MEAN_FACTOR).max(LOAD_REF)
}
const HALFOPEN_START: f64 = 0.1; // 半开首个放行概率
const RECOVERY_STEP: f64 = 0.2; // 半开每次成功 admit_prob += 0.2
const RECOVERY_FULL: u32 = 5; // 连续 5 次成功 → 全开（Closed）
pub(crate) const TRIP_THRESHOLD: u32 = 3; // Closed 下连续 429 达 3 → 跳 Open
const BASE_OPEN_SECS: u64 = 8; // 自发跳闸基线退避（对齐族 base 8s）
const MAX_OPEN_SECS: u64 = 1800; // 退避上限 30min（对齐 SuspiciousActivity）
const OPEN_GROWTH: f64 = 1.6; // 退避升级倍率（对齐 cooldown 1.6^n）
const MIN_ADMIT_SEED: f64 = 0.02; // admit_prob_seed 下限，永留一线试探
const IDLE_EVICT_SECS: u64 = 900; // cleanup 淘汰 15min 无活动条目
/// 读路径刷新 `last_touch` 的最小间隔（秒）。
///
/// `last_touch` 只被 [`HealthTracker::cleanup`] 的空闲淘汰读取（门限
/// `IDLE_EVICT_SECS`=900s），按本窗口限频刷新与每次一刷对淘汰语义完全等价
/// （持续活跃的键距上次刷新永远 < 10s，离 900s 门限还差两个数量级），
/// 却把选号读路径（`p_avail_with_load_ref`，对每个候选每轮必调）从「每候选一次写」
/// 降为「每候选最多每 10s 一次写」。衰减时钟是独立的 `last_decay_at`，
/// 本窗口**不碰**衰减语义（半衰期 60s 的连续增量衰减照旧每次调用都结算）。
const LAST_TOUCH_REFRESH_SECS: u64 = 10;
/// 惩罚衰减半衰期：每过这么久，429 惩罚减半、成功率朝 1.0 回归一半、
/// `consecutive_429`/`open_count` 各减 1、`admit_prob_seed` 翻倍恢复。
///
/// 取 60s，与上游 `USER_REQUEST_RATE_EXCEEDED` 的状态型惩罚窗口同量级
/// （实测静置约 2 分钟自愈）。详见 [`HealthTracker::decay_penalties`]。
///
/// ⚠️ 刻意**没有**"活跃门槛"常量。上一版有 5s 门槛并用 `last_touch` 计时，
/// 结果因选号读 p_avail 会刷新 `last_touch` 而永不触发（实测 200 轮零衰减）。
/// 连续增量衰减不需要门槛：指数衰减可组合，同轮 dt≈0 → factor≈1 天然幂等。
const PENALTY_DECAY_HALFLIFE_SECS: f64 = 60.0;

/// 熔断状态。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Circuit {
    /// 全速：正常参与选号。
    Closed,
    /// 熔断：到 `until` 前不放行（p_avail=0）。
    Open { until: Instant },
    /// 半开：按 `admit_prob` 概率试探放行；连续成功升概率、失败回 Open。
    HalfOpen { admit_prob: f64 },
}

#[derive(Debug, Clone)]
struct HealthState {
    ewma_success: f64, // 成功率 EWMA(α=0.3)，乐观初始 1.0
    ewma_429: f64,     // 429 率 EWMA(α=0.5)，初始 0.0
    circuit: Circuit,
    consecutive_429: u32, // 连续 429（成功即清零），驱动跳闸+退避升级
    last_success: Option<Instant>,
    last_429: Option<Instant>,
    open_start: Option<Instant>, // 本轮 Open 起点（观测/恢复窗口埋点）
    admit_prob_seed: f64,        // 半开起始放行概率；每次半开失败 *=0.5（收缩）
    recovery_samples: u32,       // 半开内连续成功计数，达 RECOVERY_FULL 全开
    open_count: u32,             // 累计跳闸轮数，退避 1.6^open_count
    last_touch: Instant,         // cleanup 空闲淘汰用（任何读写都会刷新，读路径按 LAST_TOUCH_REFRESH_SECS 限频）
    /// 上次**惩罚衰减**的推进时刻（增量衰减基准）。
    ///
    /// ⚠️ 必须与 `last_touch` 分开，这是一个已实测的生产缺陷的根因：
    /// `last_touch` 的语义是「这个键还被引用」（供 `cleanup` 的 `IDLE_EVICT_SECS` 淘汰），
    /// 而选号对**每个候选**都读 `p_avail` → 会刷新它。若拿它当衰减时钟，
    /// 饥饿号（拿不到请求但每轮都被读）的「空闲时长」恒为 0，衰减永不发生。
    /// 实测：200 轮选号后 `ewma_429: 0.875 → 0.875`，一次都没衰减。
    last_decay_at: Instant,
    /// 离散惩罚字段的**小数进位累加器**（单位：半衰期）。
    ///
    /// ⚠️ 没有它会重现 S0 那一类缺陷：`decay_penalties` 在选号热路径上每次被调用时
    /// `dt` 只有微秒级，`(dt / HALFLIFE) as u32` 恒为 0 → `consecutive_429` /
    /// `open_count` / `admit_prob_seed` 这些**离散**字段永不衰减，而 `last_decay_at`
    /// 已被推进 → 那些零碎时间被永久丢弃。
    /// 连续字段（EWMA）不受影响，因为指数衰减可组合（`f(dt₁)·f(dt₂)=f(dt₁+dt₂)`）；
    /// 离散字段必须自己攒够一个半衰期才能减一步。
    decay_carry: f64,
}

impl Default for HealthState {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            ewma_success: 1.0,
            ewma_429: 0.0,
            circuit: Circuit::Closed,
            consecutive_429: 0,
            last_success: None,
            last_429: None,
            open_start: None,
            admit_prob_seed: HALFOPEN_START,
            recovery_samples: 0,
            open_count: 0,
            last_touch: now,
            last_decay_at: now,
            decay_carry: 0.0,
        }
    }
}

/// p_avail 健康分档边界(L2 重排序键用):把连续 p_avail 粗量化成 3 档,让"负载"能在同档内成为
/// 一等分流键(治惠群),坏号仍靠粗档沉底。宽边界(0.75/0.40)降低边界抖动。
pub const HEALTH_TIER_HEALTHY_MIN: f64 = 0.75;
pub const HEALTH_TIER_DEGRADED_MIN: f64 = 0.40;

/// p_avail → 健康档:0=healthy(p≥0.75)、1=degraded(p≥0.40)、2=bad(其余,含熔断 Open 的 p=0)。
/// 升序排序键用:档小(健康)排前,同档内再按负载分流。
pub fn health_tier(p: f64) -> u8 {
    if p >= HEALTH_TIER_HEALTHY_MIN {
        0
    } else if p >= HEALTH_TIER_DEGRADED_MIN {
        1
    } else {
        2
    }
}

/// 只读健康快照（概览页/hover 推断日志用）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthSnapshot {
    pub circuit_open: bool,
    pub half_open: bool,
    pub admit_prob: f64,
    pub health: f64,
    pub ewma_success: f64,
    pub ewma_429: f64,
    pub consecutive_429: u32,
    pub open_remaining_secs: u64,
}

/// 健康追踪器：单一 `Mutex<HashMap<family_key, HealthState>>`。
pub struct HealthTracker {
    states: Mutex<HashMap<String, HealthState>>,
    /// 429 降权关闭开关(默认 false=降权生效)。运维页可热更:true 时 p_avail 的 health 项跳过
    /// EWMA-429 惩罚(某些场景不想让偶发 429 影响分流)。熔断 gate 不受此开关影响(429 跳闸仍生效)。
    disable_429_weight: AtomicBool,
}

impl Default for HealthTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthTracker {
    pub fn new() -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
            disable_429_weight: AtomicBool::new(false),
        }
    }

    /// 设置 429 降权是否关闭(运维热更)。true=关闭降权(health 跳过 429 惩罚)。
    pub fn set_disable_429_weight(&self, disabled: bool) {
        self.disable_429_weight.store(disabled, Ordering::Relaxed);
    }

    /// 退避时长：`min(BASE*GROWTH^(n-1), MAX)`，n=open_count（≥1）。
    fn open_backoff(n: u32) -> Duration {
        let secs = (BASE_OPEN_SECS as f64 * OPEN_GROWTH.powi(n.saturating_sub(1) as i32)) as u64;
        Duration::from_secs(secs.min(MAX_OPEN_SECS))
    }

    /// 惰性推进：把到期的 Open 推到 HalfOpen（无定时器核心）。
    fn tick_circuit(s: &mut HealthState, now: Instant) {
        if let Circuit::Open { until } = s.circuit {
            if now >= until {
                s.circuit = Circuit::HalfOpen {
                    admit_prob: s.admit_prob_seed,
                };
                s.recovery_samples = 0;
            }
        }
    }

    /// 惰性**惩罚衰减**：按距上次衰减推进的时长，连续回退所有单向惩罚状态。
    ///
    /// ## 修的是什么（两个已实测的生产缺陷）
    ///
    /// **① 饥饿自锁。** `ewma_429` 原先只在 `on_success` 里衰减、`ewma_success` 只在成功时抬升。
    /// 于是形成死锁：号吃几次 429 → `health` 跌破分档边界 → 进 T1/T2 → `health_tier` 是
    /// 排序键第④位（第③位是 prio_key），低档只在高档全部饱和时才被选到 → **拿不到请求 → 没有成功 →
    /// EWMA 永不回升 → 永久留在低档**。实测 6 号池里 4 个号进 T2 且 `rpm=0 inflight=0`
    /// 完全空转，有效容量 6→3，**全程零 429**。这是"越跑越慢"的确切机制。
    ///
    /// **② 上一版修复本身失效（本次修正的重点）。** 上一版用 `now - last_touch` 当空闲时长
    /// 并设 5s 门槛，但 `last_touch` 会被**选号读 `p_avail`** 刷新（对每个候选都读），
    /// 于是饥饿号的"空闲时长"恒为 0，`decay_idle` 每次都在门槛处 return。
    /// 实测：200 轮选号后 `ewma_429: 0.875 → 0.875`，一次都没衰减。
    /// 而它的测试之所以通过，是因为测试**手工回拨 `last_touch`** —— 绕过了要验证的读路径。
    ///
    /// 本版两处根治：
    /// - 时钟改用独立的 `last_decay_at`（只由本函数与三个真实结果点推进，选号读不刷新）；
    /// - **去掉门槛，改连续增量衰减**。指数衰减可组合（`f(dt₁)·f(dt₂) = f(dt₁+dt₂)`），
    ///   所以按调用频率切分不改变总衰减量，同轮内 `dt≈0` → `factor≈1` 天然幂等，
    ///   不需要门槛去"保护活跃号"。
    ///
    /// ## 为什么按时间衰减是对的
    ///
    /// 上游 `USER_REQUEST_RATE_EXCEEDED` 是**状态型惩罚窗口**（实测静置约 2 分钟自愈，
    /// 命中率衰减 `<1s 47%` → `>120s 0.9%`）。"这个号有多可疑"本就是随时间自然消退的量，
    /// 不是必须靠新样本才能更新的量。用过期的 429 观测压制选号，是在用过期信息做决策。
    ///
    /// 半衰期 `PENALTY_DECAY_HALFLIFE_SECS`（60s）与上游惩罚窗口同量级。
    /// 活跃且**真的在失败**的号不会被洗白：每次失败 `on_429` 以 `A_429=0.5` 抬升，
    /// 远快于 2s 间隔内约 2.3% 的衰减量。
    ///
    /// ## 覆盖的状态（三个单向棘轮，缺一个就还会自锁）
    ///
    /// | 状态 | 原先唯一的下降路径 | 为何不够 |
    /// |---|---|---|
    /// | `ewma_429` / `ewma_success` | `on_success` | 拿不到请求就没有成功 |
    /// | `consecutive_429` | `on_success` 置 0 | 同上 |
    /// | `open_count` | 半开内连续 5 次成功 | 退避 `1.6^n` 顶格 30min，而 `report_family_suspicious` 对 **403（临时态）** 也无条件 `+= 1` |
    /// | `admit_prob_seed` | 半开成功 | 每次半开失败 ×0.5 直到下限 0.02 → `p_avail=0.02` → 排最后 → 拿不到那 2% 试探 → 凑不齐 5 次成功 → **永久化** |
    ///
    /// **不碰 `Circuit`**：Open→HalfOpen 的推进是 `tick_circuit` 的职责，有独立时序。
    /// 真跳闸的号仍由 `gate=0` 拦住，本函数绝不把它放回（有回归测试守这一条）。
    fn decay_penalties(s: &mut HealthState, now: Instant) {
        let dt = now.saturating_duration_since(s.last_decay_at).as_secs_f64();
        if dt <= 0.0 {
            return;
        }
        s.last_decay_at = now;
        // 指数衰减因子：每过一个半衰期减半。dt≈0 → factor≈1（幂等）。
        let factor = 0.5_f64.powf(dt / PENALTY_DECAY_HALFLIFE_SECS);

        // 429 惩罚按因子衰减。
        s.ewma_429 *= factor;
        // 成功率朝中性 1.0 回归（不是直接置 1.0）：空闲不等于证明健康，
        // 只是旧的失败观测过期了。与 ewma_429 用同一因子，保持两个方向对称。
        s.ewma_success = 1.0 - (1.0 - s.ewma_success) * factor;

        // ── 离散字段：攒够一个半衰期才减一步（见 `decay_carry` 的说明）。
        //
        // 必须走进位累加而非 `(dt / HALFLIFE) as u32`：本函数在选号热路径上被高频调用，
        // 单次 dt 只有微秒，整除恒为 0 → 离散字段永不衰减，且零碎时间随
        // `last_decay_at` 的推进被丢弃。这正是上一版 S0 缺陷的同一类形态。
        s.decay_carry += dt / PENALTY_DECAY_HALFLIFE_SECS;
        // 上限 64 步：防御性，避免进程挂起很久后一次消费过多（结果等价，只是省循环）。
        let mut steps = 0u32;
        while s.decay_carry >= 1.0 && steps < 64 {
            s.decay_carry -= 1.0;
            steps += 1;
        }
        if steps > 0 {
            // 连续 429 计数：递减而非清零，保留"最近确实连续失败过"的强度信息。
            s.consecutive_429 = s.consecutive_429.saturating_sub(steps);
            // open_count 同步递减：否则退避 1.6^n 顶格后永不回落（见上表）。
            s.open_count = s.open_count.saturating_sub(steps);
            // 半开起始概率朝 HALFOPEN_START 恢复：每步翻倍，封顶在起始值。
            // 这是解开"seed 收缩到 0.02 → 拿不到那 2% 试探 → 永远 0.02"死锁的关键。
            if s.admit_prob_seed < HALFOPEN_START {
                let restored = s.admit_prob_seed * 2f64.powi(steps.min(16) as i32);
                s.admit_prob_seed = restored.min(HALFOPEN_START);
            }
        }
    }

    /// 成功：抬 ewma_success、衰减 ewma_429、清连续 429；半开期连续成功 AIMD 放大直至全开。
    pub fn on_success(&self, key: &str) {
        let now = Instant::now();
        let mut map = self.states.lock();
        let s = map.entry(key.to_string()).or_default();
        Self::tick_circuit(s, now);
        // 先结算已过去的时间衰减，再叠加本次结果（保证 last_decay_at 单调推进，
        // 且"空闲期间的恢复"不会被一次新结果吞掉）。
        Self::decay_penalties(s, now);
        s.ewma_success = A_SUCCESS + (1.0 - A_SUCCESS) * s.ewma_success;
        s.ewma_429 = (1.0 - A_429) * s.ewma_429;
        s.consecutive_429 = 0;
        s.last_success = Some(now);
        s.last_touch = now;
        if let Circuit::HalfOpen { admit_prob } = s.circuit {
            s.recovery_samples += 1;
            if s.recovery_samples >= RECOVERY_FULL {
                s.circuit = Circuit::Closed;
                s.open_count = 0;
                s.admit_prob_seed = HALFOPEN_START;
            } else {
                let next = (admit_prob + RECOVERY_STEP).min(1.0);
                s.circuit = Circuit::HalfOpen { admit_prob: next };
            }
        }
    }

    /// 裸 429（单号）：MD 拉低 ewma_success、抬 ewma_429；连续达阈值跳闸 Open，半开期 429 立即回 Open。
    pub fn on_429(&self, key: &str) {
        let now = Instant::now();
        let mut map = self.states.lock();
        let s = map.entry(key.to_string()).or_default();
        Self::tick_circuit(s, now);
        // 先结算已过去的时间衰减，再叠加本次结果（保证 last_decay_at 单调推进，
        // 且"空闲期间的恢复"不会被一次新结果吞掉）。
        Self::decay_penalties(s, now);
        s.ewma_success = (1.0 - A_SUCCESS) * s.ewma_success;
        s.ewma_429 = A_429 + (1.0 - A_429) * s.ewma_429;
        s.consecutive_429 += 1;
        s.last_429 = Some(now);
        s.last_touch = now;
        match s.circuit {
            Circuit::HalfOpen { .. } => {
                s.open_count += 1;
                s.admit_prob_seed = (s.admit_prob_seed * 0.5).max(MIN_ADMIT_SEED);
                let backoff = Self::open_backoff(s.open_count);
                s.circuit = Circuit::Open {
                    until: now + backoff,
                };
                s.open_start = Some(now);
            }
            Circuit::Closed => {
                if s.consecutive_429 >= TRIP_THRESHOLD {
                    s.open_count += 1;
                    s.admit_prob_seed = HALFOPEN_START;
                    let backoff = Self::open_backoff(s.open_count);
                    s.circuit = Circuit::Open {
                        until: now + backoff,
                    };
                    s.open_start = Some(now);
                }
            }
            Circuit::Open { .. } => { /* 已开，不重复升级（一条链多次 429 只算一轮） */
            }
        }
    }

    /// 族级强制跳闸：用 cooldown 给的硬窗 `backoff` 作 Open until，两套时钟不打架。
    /// 反复被风控 → 起始试探减半（下次半开更谨慎）。
    pub fn report_family_suspicious(&self, fam: &str, backoff: Duration) {
        let now = Instant::now();
        let mut map = self.states.lock();
        let s = map.entry(fam.to_string()).or_default();
        Self::tick_circuit(s, now);
        // 先结算已过去的时间衰减，再叠加本次结果（保证 last_decay_at 单调推进，
        // 且"空闲期间的恢复"不会被一次新结果吞掉）。
        Self::decay_penalties(s, now);
        s.ewma_429 = A_429 + (1.0 - A_429) * s.ewma_429;
        s.consecutive_429 += 1;
        s.last_429 = Some(now);
        s.last_touch = now;
        s.admit_prob_seed = match s.circuit {
            Circuit::Open { .. } | Circuit::HalfOpen { .. } => {
                (s.admit_prob_seed * 0.5).max(MIN_ADMIT_SEED)
            }
            Circuit::Closed => HALFOPEN_START,
        };
        s.open_count += 1;
        s.circuit = Circuit::Open {
            until: now + backoff,
        };
        s.open_start = Some(now);
    }

    /// 可用概率 p_avail ∈ [0,1]：选号权重。读路径也惰性推进。
    ///
    /// 用固定的 [`LOAD_REF`] 作 inflight 归一分母——**仅适用于小池/测试**。
    /// 选号热路径请用 [`Self::p_avail_with_load_ref`] 传入自适应基准
    /// （见 [`adaptive_load_ref`]），否则企业级规模下负载维度会整体失效。
    pub fn p_avail(&self, key: &str, rpm: u32, inflight: u32, rpm_limit: u32) -> f64 {
        self.p_avail_with_load_ref(key, rpm, inflight, rpm_limit, LOAD_REF)
    }

    /// 可用概率 p_avail ∈ [0,1]，inflight 归一分母由调用方显式给出。
    ///
    /// `load_ref` 应取 [`adaptive_load_ref`] 的结果，且**一次选号内对所有候选用同一个值**
    /// （否则各候选分母不同 → 排序键非传递 → min_by_key 偶发选错号）。
    pub fn p_avail_with_load_ref(
        &self,
        key: &str,
        rpm: u32,
        inflight: u32,
        rpm_limit: u32,
        load_ref: f64,
    ) -> f64 {
        let now = Instant::now();
        let mut map = self.states.lock();
        // 借用 `key` 查表，仅在**首次**遇到该键时才做一次 String 分配插入。
        //
        // 选号读路径对每个候选每轮都调本函数，键早已存在，`entry(key.to_string())`
        // 的每次堆分配是纯浪费（43 号池一轮选号 43 次分配，100 并发下放大成
        // 临界区内的分配风暴）。get-then-insert 让热路径零分配、行为逐字节等价
        // （两种写法在同一把 Mutex 内完成，无并发差异）。
        let s = match map.get_mut(key) {
            Some(s) => s,
            None => {
                map.insert(key.to_string(), HealthState::default());
                map.get_mut(key).expect("刚插入必有")
            }
        };
        Self::tick_circuit(s, now);
        Self::decay_penalties(s, now);
        // 限频刷新：`last_touch` 只喂 cleanup 的空闲淘汰（900s 门限），按
        // LAST_TOUCH_REFRESH_SECS 窗口刷新语义等价，读路径从「每候选一次写」
        // 降为「最多每 10s 一次写」。衰减照旧每次调用结算（last_decay_at 独立）。
        if now.saturating_duration_since(s.last_touch)
            >= Duration::from_secs(LAST_TOUCH_REFRESH_SECS)
        {
            s.last_touch = now;
        }
        let gate = match s.circuit {
            Circuit::Closed => 1.0,
            Circuit::Open { .. } => 0.0,
            Circuit::HalfOpen { admit_prob } => admit_prob,
        };
        // 429 降权:默认生效(health 含 EWMA-429 惩罚);运维关闭开关后跳过惩罚(只用 ewma_success)。
        let health = if self.disable_429_weight.load(Ordering::Relaxed) {
            s.ewma_success.clamp(0.0, 1.0)
        } else {
            (s.ewma_success * (1.0 - HEALTH_429_WEIGHT * s.ewma_429)).clamp(0.0, 1.0)
        };
        let rpm_pressure = if rpm_limit > 0 {
            (rpm as f64 / rpm_limit as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        // 归一分母由调用方给出（自适应，见 adaptive_load_ref）。防御性下限避免除零/负数误配。
        let load = (inflight as f64 / load_ref.max(1.0)).clamp(0.0, 1.0);
        (gate * health * (1.0 - rpm_pressure) * (1.0 - LOAD_PENALTY * load)).clamp(0.0, 1.0)
    }

    /// 只读快照（概览页/hover）。先推进到期的 Open→HalfOpen，保证展示状态与热路径一致。
    ///
    /// 不调用 tick_circuit 时，已到期的 Open 在面板上持续显示 circuit_open=true，
    /// 直到下一次真实流量触发 on_success/on_429 才推进，造成"已恢复但面板仍显断路"假象。
    pub fn snapshot(&self, key: &str) -> Option<HealthSnapshot> {
        let now = Instant::now();
        let mut map = self.states.lock();
        map.get_mut(key).map(|s| {
            // 推进到期熔断器状态（惰性推进，无定时器）
            Self::tick_circuit(s, now);
            let (circuit_open, half_open, admit_prob, open_remaining_secs) = match s.circuit {
                Circuit::Closed => (false, false, 1.0, 0),
                Circuit::Open { until } => (
                    true,
                    false,
                    0.0,
                    until.saturating_duration_since(now).as_secs(),
                ),
                Circuit::HalfOpen { admit_prob } => (false, true, admit_prob, 0),
            };
            HealthSnapshot {
                circuit_open,
                half_open,
                admit_prob,
                health: (s.ewma_success * (1.0 - HEALTH_429_WEIGHT * s.ewma_429)).clamp(0.0, 1.0),
                ewma_success: s.ewma_success,
                ewma_429: s.ewma_429,
                consecutive_429: s.consecutive_429,
                open_remaining_secs,
            }
        })
    }

    /// 空闲淘汰（周期调用，防 String 键无界堆积）。
    pub fn cleanup(&self) {
        let now = Instant::now();
        self.states
            .lock()
            .retain(|_, s| now.duration_since(s.last_touch) < Duration::from_secs(IDLE_EVICT_SECS));
    }

    /// 手动清除（admin 重新启用号时对齐 clear_cooldown）。
    pub fn clear(&self, key: &str) -> bool {
        self.states.lock().remove(key).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ht() -> HealthTracker {
        HealthTracker::new()
    }

    #[test]
    fn test_default_state_is_fully_available() {
        let h = ht();
        assert!((h.p_avail("k", 0, 0, 0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_health_formula() {
        let h = ht();
        // 造 ewma_success=0.8 / ewma_429=0.5 → health=0.8*(1-0.6*0.5)=0.8*0.7=0.56
        {
            let mut map = h.states.lock();
            let s = map.entry("k".into()).or_default();
            s.ewma_success = 0.8;
            s.ewma_429 = 0.5;
        }
        let p = h.p_avail("k", 0, 0, 0);
        assert!((p - 0.56).abs() < 1e-6, "p={p}");
    }

    /// 429 降权开关(0.7.24):关闭后 p_avail 的 health 跳过 EWMA-429 惩罚(只用 ewma_success)。
    /// 旧代码无此开关,429 惩罚恒生效。
    #[test]
    fn test_health_429_weight_toggle() {
        let h = ht();
        {
            let mut map = h.states.lock();
            let s = map.entry("k".into()).or_default();
            s.ewma_success = 0.8;
            s.ewma_429 = 0.5;
        }
        // 默认降权开:health=0.8×(1-0.6×0.5)=0.56。
        assert!(
            (h.p_avail("k", 0, 0, 0) - 0.56).abs() < 1e-6,
            "降权开 → 0.56"
        );
        // 关闭降权:health=ewma_success=0.8(跳过 429 惩罚)。
        h.set_disable_429_weight(true);
        assert!(
            (h.p_avail("k", 0, 0, 0) - 0.8).abs() < 1e-6,
            "降权关 → 0.8(跳过 429 惩罚)"
        );
    }

    #[test]
    fn test_rpm_pressure_scales_p_avail() {
        let h = ht();
        assert!((h.p_avail("k", 20, 0, 20)).abs() < 1e-9, "rpm==limit → 0");
        let half = h.p_avail("k2", 10, 0, 20);
        assert!((half - 0.5).abs() < 1e-6, "rpm=半 → 0.5, got {half}");
    }

    #[test]
    fn test_rpm_limit_zero_disables_pressure() {
        let h = ht();
        assert!((h.p_avail("k", 9999, 0, 0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_load_penalty() {
        let h = ht();
        let full = h.p_avail("k", 0, 8, 0); // inflight=LOAD_REF → 1-0.5*1=0.5
        assert!((full - 0.5).abs() < 1e-6, "got {full}");
    }

    // ============ 自适应 LOAD_REF（企业级负载维度不失效）============

    #[test]
    fn test_adaptive_load_ref_small_pool_keeps_floor() {
        // 零回归：小池/低负载时基准恒为地板 8.0，与历史固定值逐位相同。
        assert_eq!(adaptive_load_ref(0, 0), 8.0, "空池取地板");
        assert_eq!(adaptive_load_ref(0, 5), 8.0, "全空闲取地板");
        assert_eq!(adaptive_load_ref(10, 5), 8.0, "平均2×2=4 < 8 → 取地板");
        assert_eq!(adaptive_load_ref(20, 5), 8.0, "平均4×2=8 → 恰为地板");
    }

    #[test]
    fn test_adaptive_load_ref_scales_at_enterprise_load() {
        // 企业级：平均在途 > 4 时基准随之放大。
        // 200 号 × 平均 8.6 在途（实测 p90 延迟下 6000RPM/200号 的稳态）→ 基准 17.2
        let r = adaptive_load_ref(1720, 200);
        assert!((r - 17.2).abs() < 1e-9, "平均8.6×2=17.2, got {r}");
        // 400 号 × 平均 7.1（10000RPM p90）→ 14.2
        let r2 = adaptive_load_ref(2840, 400);
        assert!((r2 - 14.2).abs() < 1e-9, "平均7.1×2=14.2, got {r2}");
    }

    #[test]
    fn test_enterprise_load_dimension_retains_resolution() {
        // ⭐ 核心回归（旧代码必失败）：企业级规模下负载维度必须仍能区分不同在途的号。
        //
        // 旧代码固定 LOAD_REF=8：inflight 9 与 50 的 load 都 clamp 到 1.0 →
        // p_avail 完全相同（都是 0.5）→ 负载维度整体失效，分流塌成随机。
        let h = ht();
        // 稳态：200 号池平均在途 8.6 → 自适应基准 17.2
        let load_ref = adaptive_load_ref(1720, 200);

        let busy = h.p_avail_with_load_ref("busy", 0, 50, 0, load_ref);
        let mid = h.p_avail_with_load_ref("mid", 0, 17, 0, load_ref);
        let light = h.p_avail_with_load_ref("light", 0, 9, 0, load_ref);
        let idle = h.p_avail_with_load_ref("idle", 0, 1, 0, load_ref);

        // 必须严格单调可分：越空闲 p_avail 越高。
        assert!(
            idle > light && light > mid && mid >= busy,
            "负载维度应严格可分: idle={idle} light={light} mid={mid} busy={busy}"
        );
        // 旧代码等价对照：固定 8.0 时 9 与 50 不可区分（都 load=1.0 → 0.5）。
        let old_light = h.p_avail_with_load_ref("o1", 0, 9, 0, 8.0);
        let old_busy = h.p_avail_with_load_ref("o2", 0, 50, 0, 8.0);
        assert!(
            (old_light - old_busy).abs() < 1e-9,
            "对照组:固定基准下 9 与 50 在途不可区分(这正是被修的缺陷)"
        );
        // 而自适应下它们必须可分。
        assert!(
            light > busy,
            "自适应基准下 9 与 50 在途必须可区分: {light} vs {busy}"
        );
    }

    #[test]
    fn test_adaptive_load_ref_uniform_rise_still_discriminates() {
        // 自适应经典陷阱检查：全池等量上涨时，仍须能区分池内**相对**负载。
        let h = ht();
        // 场景 A：平均 10（总 100/10 号）→ 基准 20
        let ref_a = adaptive_load_ref(100, 10);
        // 场景 B：全池翻倍，平均 20 → 基准 40
        let ref_b = adaptive_load_ref(200, 10);
        assert!(ref_b > ref_a, "基准应随池负载上涨");
        // 两个场景里"相对更闲的号"都必须比"相对更忙的号"得分高。
        for (lab, r, lo, hi) in [("A", ref_a, 5u32, 20u32), ("B", ref_b, 10, 40)] {
            let idle = h.p_avail_with_load_ref(&format!("{lab}-idle"), 0, lo, 0, r);
            let busy = h.p_avail_with_load_ref(&format!("{lab}-busy"), 0, hi, 0, r);
            assert!(
                idle > busy,
                "场景{lab}: 相对空闲号必须优于相对繁忙号 ({idle} vs {busy})"
            );
        }
    }

    #[test]
    fn test_load_ref_defensive_lower_bound() {
        // 误配防御：load_ref 传 0/负数不得除零或产生 NaN。
        let h = ht();
        let p0 = h.p_avail_with_load_ref("k", 0, 5, 0, 0.0);
        let pneg = h.p_avail_with_load_ref("k2", 0, 5, 0, -3.0);
        assert!(p0.is_finite() && (0.0..=1.0).contains(&p0), "got {p0}");
        assert!(
            pneg.is_finite() && (0.0..=1.0).contains(&pneg),
            "got {pneg}"
        );
    }

    #[test]
    fn test_trip_after_consecutive_429() {
        let h = ht();
        h.on_429("k");
        h.on_429("k");
        assert!(h.p_avail("k", 0, 0, 0) > 0.0, "2 次未跳闸");
        h.on_429("k"); // 第 3 次 → Open
        assert!((h.p_avail("k", 0, 0, 0)).abs() < 1e-9, "3 次应跳闸 gate=0");
    }

    #[test]
    fn test_success_resets_consecutive_429() {
        let h = ht();
        h.on_429("k");
        h.on_429("k");
        h.on_success("k"); // 归零
        h.on_429("k");
        h.on_429("k");
        assert!(h.p_avail("k", 0, 0, 0) > 0.0, "归零后再 2 次不应跳闸");
    }

    #[test]
    fn test_open_to_halfopen_then_aimd_recovery() {
        let h = ht();
        // 用极短 backoff 强制跳闸
        h.report_family_suspicious("fam", Duration::from_millis(30));
        assert!((h.p_avail("fam", 0, 0, 0)).abs() < 1e-9, "Open 期 gate=0");
        std::thread::sleep(Duration::from_millis(50));
        // 惰性推进 → HalfOpen；p_avail 会叠乘 health(suspicious 抬高 ewma_429→health<1),
        // 故用 snapshot 查纯 gate=admit_prob=HALFOPEN_START(0.1)。
        let _ = h.p_avail("fam", 0, 0, 0);
        let snap = h.snapshot("fam").unwrap();
        assert!(snap.half_open, "应进入半开");
        assert!(
            (snap.admit_prob - 0.1).abs() < 1e-6,
            "半开起点 gate 应 0.1, got {}",
            snap.admit_prob
        );
        // 连续 5 次成功 → 全开(Closed)。health 是 EWMA 渐近回升,不必等于精确 1.0,
        // 故断言 circuit 已 Closed(gate 回 1.0)+ p_avail 已高(>0.9)。
        for _ in 0..RECOVERY_FULL {
            h.on_success("fam");
        }
        let snap2 = h.snapshot("fam").unwrap();
        assert!(
            !snap2.circuit_open && !snap2.half_open,
            "5 次成功应全开(Closed)"
        );
        assert!(h.p_avail("fam", 0, 0, 0) > 0.9, "全开后 p_avail 应高");
    }

    #[test]
    fn test_halfopen_failure_reopens_and_shrinks_seed() {
        let h = ht();
        h.report_family_suspicious("fam", Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(35));
        let _ = h.p_avail("fam", 0, 0, 0); // 推进到 HalfOpen
        h.on_429("fam"); // 半开失败 → 回 Open,seed 0.1→0.05
        // 立刻应 Open(gate=0)
        assert!((h.p_avail("fam", 0, 0, 0)).abs() < 1e-9);
    }

    #[test]
    fn test_open_backoff_monotonic_capped() {
        assert!(HealthTracker::open_backoff(1) < HealthTracker::open_backoff(3));
        assert_eq!(HealthTracker::open_backoff(50).as_secs(), MAX_OPEN_SECS);
    }

    #[test]
    fn test_idc_key_independent_from_m365() {
        let h = ht();
        h.report_family_suspicious("m365:tenantA", Duration::from_secs(30));
        // IdC 键不受影响
        assert!((h.p_avail("cred:61", 0, 0, 0) - 1.0).abs() < 1e-9);
        assert!((h.p_avail("m365:tenantA", 0, 0, 0)).abs() < 1e-9);
    }

    #[test]
    fn test_clear_removes_key() {
        let h = ht();
        h.on_429("k");
        assert!(h.clear("k"));
        assert!(h.snapshot("k").is_none());
    }

    #[test]
    fn test_ewma_429_decays_on_success() {
        let h = ht();
        h.on_429("k");
        h.on_429("k");
        let before = h.snapshot("k").unwrap().ewma_429;
        for _ in 0..5 {
            h.on_success("k");
        }
        let after = h.snapshot("k").unwrap().ewma_429;
        assert!(after < before, "ewma_429 应随成功衰减 {before}->{after}");
    }

    /// 回归（S0 · 本轮最重要的测试）：**选号读取不得阻止衰减**。
    ///
    /// **旧代码为何失败**：上一版 `decay_idle` 用 `now - last_touch` 计时并设 5s 门槛，
    /// 而 `p_avail` 每次调用都 `s.last_touch = now`。选号对**每个候选**都读 p_avail，
    /// 于是饥饿号的"空闲时长"恒为 0 → 每次都在门槛处 return → **一次都不衰减**。
    /// 实测旧代码：200 轮选号后 `ewma_429: 0.875 → 0.875`（完全没动）。
    ///
    /// 本测试复现的正是那个场景：号已空闲 180s（衰减时钟回拨），
    /// 然后连打 200 轮选号读取。旧实现下这 200 次读会把时钟刷成 now、
    /// 吃掉全部应得的衰减；新实现下衰减时钟独立于 `last_touch`，读取不影响它。
    ///
    /// ⚠️ 刻意**只回拨衰减时钟**、不碰任何其它私有状态 —— 上一版的测试靠回拨
    /// `last_touch` 才"通过"，绕过了自己要验证的那条读路径，这是 S0 漏网的直接原因。
    #[test]
    fn test_selection_reads_do_not_block_decay() {
        let h = ht();
        for _ in 0..3 {
            h.on_429("starved");
        }
        let before = h.snapshot("starved").unwrap();
        assert!(
            before.health < HEALTH_TIER_HEALTHY_MIN,
            "前置：应已跌出健康档，实际 {}",
            before.health
        );

        // 该号已空闲 180s（= 3 个半衰期）：只推进衰减时钟，零成功、零失败
        {
            let mut map = h.states.lock();
            map.get_mut("starved").unwrap().last_decay_at =
                Instant::now() - Duration::from_secs(180);
        }

        // 生产真实情形：选号每轮对所有候选读 p_avail，饥饿号也被读到 200 次
        let load_ref = adaptive_load_ref(0, 1);
        for _ in 0..200 {
            let _ = h.p_avail_with_load_ref("starved", 0, 0, 0, load_ref);
        }

        let after = h.snapshot("starved").unwrap();
        assert!(
            after.ewma_429 < before.ewma_429 * 0.5,
            "空闲 3 个半衰期后 ewma_429 应显著衰减，200 轮选号读取不得吃掉它：{} -> {}",
            before.ewma_429,
            after.ewma_429
        );
        assert!(
            after.health >= HEALTH_TIER_HEALTHY_MIN,
            "衰减后应浮回健康档重新参与竞争，实际 health={}（旧实现恒不变）",
            after.health
        );
    }

    /// 回归（S0 核心）：只推进时间、零成功零请求，惩罚必须能自行恢复。
    ///
    /// 用注入的时间差直接调 `decay_penalties`（它是纯函数式的状态推进），
    /// 覆盖"空闲 3 个半衰期后回到健康档"这条语义。
    ///
    /// **旧代码为何失败**：旧 `decay_idle` 有 5s 门槛且时钟被读路径刷新，
    /// 在真实调用序列里等价于空操作。
    #[test]
    fn test_penalties_recover_with_time_only() {
        let h = ht();
        for _ in 0..3 {
            h.on_429("k");
        }
        let before = h.snapshot("k").unwrap();
        assert!(
            before.health < HEALTH_TIER_HEALTHY_MIN,
            "前置：应已跌出健康档，实际 {}",
            before.health
        );

        // 只推进时间：把衰减时钟往回拨 3 个半衰期（180s），不产生任何成功/失败
        {
            let mut map = h.states.lock();
            let s = map.get_mut("k").unwrap();
            s.last_decay_at = Instant::now() - Duration::from_secs(180);
        }
        let load_ref = adaptive_load_ref(0, 1);
        let _ = h.p_avail_with_load_ref("k", 0, 0, 0, load_ref);
        let after = h.snapshot("k").unwrap();

        assert!(
            after.health > before.health,
            "只推进时间后 health 必须回升：{} -> {}",
            before.health,
            after.health
        );
        assert!(
            after.ewma_429 < before.ewma_429,
            "ewma_429 必须衰减：{} -> {}",
            before.ewma_429,
            after.ewma_429
        );
        assert_eq!(
            after.consecutive_429, 0,
            "3 个半衰期后 consecutive_429 应已减到 0（旧代码只在 on_success 才清零）"
        );
    }

    /// 回归（S1：三个单向棘轮）：`open_count` 与 `admit_prob_seed` 必须能靠时间恢复。
    ///
    /// **旧代码为何失败**：`open_count` 唯一归零点是"半开内连续 5 次成功"，
    /// `admit_prob_seed` 每次半开失败 ×0.5 直到下限 0.02。死锁链：
    /// 403 → open_count++ → 退避顶格 1800s + seed=0.02 → p_avail=0.02 → 排最后
    /// → 拿不到那 2% 试探 → 凑不齐 5 次成功 → **永久化**。
    /// 旧 `decay_idle` 完全不碰这两个字段，所以治不到。
    #[test]
    fn test_open_count_and_admit_seed_decay_over_time() {
        let h = ht();
        // 反复族级 403（临时态却无条件 open_count += 1）+ 半开失败收缩 seed
        for _ in 0..6 {
            h.report_family_suspicious("fam", Duration::from_secs(1));
        }
        let (oc_before, seed_before) = {
            let map = h.states.lock();
            let s = map.get("fam").unwrap();
            (s.open_count, s.admit_prob_seed)
        };
        assert!(
            oc_before >= 5,
            "前置：open_count 应已累积，实际 {oc_before}"
        );
        assert!(
            seed_before < HALFOPEN_START,
            "前置：admit_prob_seed 应已收缩，实际 {seed_before}"
        );

        // 只推进时间 4 个半衰期
        {
            let mut map = h.states.lock();
            let s = map.get_mut("fam").unwrap();
            s.last_decay_at = Instant::now() - Duration::from_secs(240);
        }
        let load_ref = adaptive_load_ref(0, 1);
        let _ = h.p_avail_with_load_ref("fam", 0, 0, 0, load_ref);

        let (oc_after, seed_after) = {
            let map = h.states.lock();
            let s = map.get("fam").unwrap();
            (s.open_count, s.admit_prob_seed)
        };
        assert!(
            oc_after < oc_before,
            "open_count 必须随时间下降（否则退避永久顶格 30min）：{oc_before} -> {oc_after}"
        );
        assert!(
            seed_after > seed_before,
            "admit_prob_seed 必须随时间恢复（否则永远拿不到试探）：{seed_before} -> {seed_after}"
        );
    }

    /// 回归（离散字段的小数进位 · review 抓到的 S0 同类缺陷）：
    /// **高频小 dt 调用累计后，离散惩罚字段也必须衰减。**
    ///
    /// **有 bug 的实现为何失败**：原先用 `(dt / HALFLIFE) as u32` 算步数。
    /// 而 `decay_penalties` 在选号热路径上每候选每轮都被调用，单次 `dt` 只有微秒，
    /// 整除恒为 0 → `consecutive_429` / `open_count` / `admit_prob_seed` **永不衰减**，
    /// 且零碎时间随 `last_decay_at` 的推进被永久丢弃。
    /// 这与 S0（衰减时钟被读路径刷掉）是同一类形态：**衰减看似实现了，实际不发生**。
    ///
    /// 本测试**不一次性回拨大 dt**（那样两种实现都能过，正是原测试的盲区），
    /// 而是模拟真实调用形态：把时钟每次只回拨 6 秒（= 0.1 个半衰期）、调 30 次，
    /// 累计 180 秒 = 3 个半衰期。有 bug 的实现下 `steps` 恒 0，离散字段一步不动。
    #[test]
    fn test_discrete_penalties_decay_via_fraction_carry() {
        let h = ht();
        for _ in 0..6 {
            h.report_family_suspicious("fam", Duration::from_secs(1));
        }
        let oc_before = {
            let map = h.states.lock();
            map.get("fam").unwrap().open_count
        };
        assert!(
            oc_before >= 5,
            "前置：open_count 应已累积，实际 {oc_before}"
        );

        // 模拟高频调用：每次只推进 0.1 个半衰期（6s），共 30 次 = 3 个半衰期
        let load_ref = adaptive_load_ref(0, 1);
        for _ in 0..30 {
            {
                let mut map = h.states.lock();
                let st = map.get_mut("fam").unwrap();
                st.last_decay_at = Instant::now() - Duration::from_secs(6);
            }
            let _ = h.p_avail_with_load_ref("fam", 0, 0, 0, load_ref);
        }

        let oc_after = {
            let map = h.states.lock();
            map.get("fam").unwrap().open_count
        };
        assert!(
            oc_after <= oc_before.saturating_sub(3),
            "累计 3 个半衰期后 open_count 应至少减 3（有 bug 的实现下一步不动）：\
             {oc_before} -> {oc_after}"
        );
    }

    /// 活跃且**真的在失败**的号不得被时间衰减洗白（防过度放宽）。
    #[test]
    fn test_actively_failing_credential_is_not_whitewashed() {
        let h = ht();
        let load_ref = adaptive_load_ref(0, 1);
        // 每"2 秒"一次 429，中间夹选号读；断言 health 仍被压在低位
        for _ in 0..8 {
            h.on_429("busy");
            {
                let mut map = h.states.lock();
                let s = map.get_mut("busy").unwrap();
                s.last_decay_at = Instant::now() - Duration::from_secs(2);
            }
            let _ = h.p_avail_with_load_ref("busy", 0, 0, 0, load_ref);
        }
        let health = h.snapshot("busy").unwrap().health;
        assert!(
            health < HEALTH_TIER_HEALTHY_MIN,
            "持续失败的号不能被衰减洗白回健康档，实际 health={health}"
        );
    }

    /// 空闲衰减**不得**把真跳闸（Open）的号放回：熔断是独立的硬门。
    #[test]
    fn test_idle_decay_does_not_reopen_tripped_circuit() {
        let h = ht();
        for _ in 0..TRIP_THRESHOLD {
            h.on_429("tripped");
        }
        assert!(
            h.snapshot("tripped").unwrap().circuit_open,
            "前置：应已跳闸"
        );
        {
            let mut map = h.states.lock();
            map.get_mut("tripped").unwrap().last_touch = Instant::now() - Duration::from_secs(600);
        }
        let load_ref = adaptive_load_ref(0, 1);
        let p = h.p_avail_with_load_ref("tripped", 0, 0, 0, load_ref);
        // Open 期间 gate=0 → p_avail 必须为 0，不因 EWMA 被衰减而复活
        // （到期后由 tick_circuit 走 HalfOpen，那是另一条正确路径）
        let snap = h.snapshot("tripped").unwrap();
        if snap.circuit_open {
            assert_eq!(p, 0.0, "Open 期间 p_avail 必须为 0，衰减不得绕过熔断");
        }
    }
    /// **G1 元测试** —— 让「越跑越慢」这类缺陷结构性不再出现。
    ///
    /// # 为什么必须有它
    ///
    /// 本项目在同一位置踩过**两次**：
    /// 1. `decay_idle` 是带测试、审查过的修复，却被同一函数几行后的 `s.last_touch = now`
    ///    完全失效 —— `p_avail` 对**每个候选**都刷新 `last_touch`，饥饿号"空闲时长"恒为 0。
    ///    实测旧实现 200 轮选号后 `ewma_429: 0.875 → 0.875`，**一次未衰减**。
    /// 2. 修好后 review 又发现**离散字段**仍不衰减（`(dt/HALFLIFE) as u32` 在热路径恒为 0）。
    ///
    /// 靠逐字段人工审查保证"每个惩罚状态都有下降路径"已两次证明不可靠，故改为元测试：
    /// 打到最坏状态 → **只推进时间**（零成功、零请求）→ 断言惩罚量在恢复。
    ///
    /// # 两条纪律
    ///
    /// - **不手工写任何私有字段**。旧测试靠回拨 `last_decay_at` 才"通过"，绕过了要验证的读路径
    ///   —— 这正是缺陷 1 漏网的直接原因。本测试只调公开 API。
    /// - **必须走真实读路径**，且要**高频反复读**（模拟选号对每个候选都读 `p_avail`）。
    ///   只读一次的话 `last_touch` 还是旧值、dt 仍大，S0 形态不会显形。
    ///
    /// # 判据为什么是「衰减量级」而不是「严格下降」
    ///
    /// 实测（两种实现各跑一遍对比）得出的结论，与直觉不同，值得写下来：
    ///
    /// - 只把衰减时钟换成 `last_touch` **并不足以**复现 S0。因为当前代码里
    ///   `decay_penalties(s, now)` 在 `s.last_touch = now` **之前**执行，所以即便读
    ///   `last_touch` 也仍能拿到正确的已过时间（实测两者衰减比 0.98472 vs 0.98436，几乎相同）。
    /// - S0 的**真正**成因是那个已被删除的**门槛**：`if idle < IDLE_DECAY_MIN_SECS { return }`。
    ///   门槛 + 被读路径刷新的时钟这两者**同时**存在，才会让每次调用都在门槛处 return
    ///   → 衰减一次都不执行（实测 `actual = 1.000000`，与历史"200 轮选号后 ewma 一次未降"吻合）。
    ///
    /// 所以判据用「衰减必须达到理论量级」而非「严格小于」：前者能同时抓住
    /// 「完全不衰减」（门槛形态）与「衰减被稀释」（若将来又引入读频依赖）两类，
    /// 而「严格小于」对前者有效、对后者无效。已验证：复现门槛形态时本测试 FAIL。
    #[test]
    fn g1_all_penalty_states_recover_with_time_alone() {
        let h = HealthTracker::new();
        const KEY: &str = "g1:worst-case";

        // ── 造最坏状态：全部经由公开 API，不碰任何私有字段 ──
        for _ in 0..12 {
            h.on_429(KEY);
        }
        h.report_family_suspicious(KEY, Duration::from_secs(MAX_OPEN_SECS));

        let before = h.snapshot(KEY).expect("状态应已建立");
        assert!(
            before.ewma_429 > 0.5,
            "前提不成立：最坏状态应有高 ewma_429，实际 {}",
            before.ewma_429
        );

        // ── 只推进时间；期间**高频反复读**真实读路径（生产选号即如此）──
        let elapsed_start = std::time::Instant::now();
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(30));
            // 每"轮选号"读多次（模拟多候选）——这是压死 last_touch、让 S0 显形的关键
            for _ in 0..5 {
                let _ = h.p_avail(KEY, 0, 0, 0);
            }
        }
        let wall = elapsed_start.elapsed().as_secs_f64();
        let after = h.snapshot(KEY).expect("状态应仍在");

        // 理论衰减：factor = 0.5^(wall / halflife)，与**读取次数无关**。
        let theoretical = 0.5_f64.powf(wall / PENALTY_DECAY_HALFLIFE_SECS);
        let actual_ratio = after.ewma_429 / before.ewma_429;
        // 留足容差（0.6 倍理论衰减量）以吸收调度抖动；S0 下衰减被读频稀释，远达不到这个线。
        let min_decay = 1.0 - (1.0 - theoretical) * 0.6;
        assert!(
            actual_ratio <= min_decay,
            "ewma_429 的衰减被读取路径稀释了（S0 缺陷形态）：\
             墙钟 {wall:.2}s 应衰减到 ≤{min_decay:.6}，实际只到 {actual_ratio:.6}。\
             检查衰减时钟是否误用了会被 p_avail 刷新的 last_touch，而非独立的 last_decay_at"
        );
        assert!(
            after.ewma_success >= before.ewma_success,
            "ewma_success 未朝中性回归：{} → {}",
            before.ewma_success,
            after.ewma_success
        );
    }

    /// G1 配套：坐实 `decay_penalties` 覆盖了**当前所有已知惩罚字段**。
    ///
    /// 新增惩罚状态时的检查清单（写进测试以便被 CI 强制读到）：
    /// - [ ] 它有**时间衰减**路径吗？（不是"成功时清零" —— 拿不到请求就没有成功）
    /// - [ ] 若是**离散量**，是否走了 `decay_carry` 进位？（`(dt/HALFLIFE) as u32` 在热路径恒为 0）
    /// - [ ] 衰减时钟用 `last_decay_at` 而**不是** `last_touch`？（后者被选号读路径刷新）
    /// - [ ] 是否已纳入 `g1_all_penalty_states_recover_with_time_alone` 的断言？
    #[test]
    fn g1_decay_covers_every_known_penalty_field() {
        let src = include_str!("health.rs");
        let body = src
            .split("fn decay_penalties")
            .nth(1)
            .expect("decay_penalties 不应被改名");
        // 只看函数体（到下一个 `fn ` 为止），避免扫到本测试自己的字段名字面量。
        let body = body.split("\n    fn ").next().unwrap_or(body);

        for field in [
            "ewma_429",
            "ewma_success",
            "consecutive_429",
            "open_count",
            "admit_prob_seed",
            "decay_carry",
        ] {
            assert!(
                body.contains(field),
                "惩罚字段 `{field}` 未出现在 decay_penalties 中 —— \
                 它将没有时间衰减路径，会成为新的单向棘轮（历史事故形态）"
            );
        }
    }
}
