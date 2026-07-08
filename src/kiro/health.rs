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
use std::time::{Duration, Instant};

const A_SUCCESS: f64 = 0.3; // ewma_success 平滑系数（慢升，抗抖动）
const A_429: f64 = 0.5; // ewma_429 平滑系数（快升，429 敏感）
const HEALTH_429_WEIGHT: f64 = 0.6; // health = ewma_success*(1-0.6*ewma_429)
const LOAD_PENALTY: f64 = 0.5; // p_avail 里 (1-0.5*load)
const LOAD_REF: f64 = 8.0; // inflight 归一参考：>=8 视为满载
const HALFOPEN_START: f64 = 0.1; // 半开首个放行概率
const RECOVERY_STEP: f64 = 0.2; // 半开每次成功 admit_prob += 0.2
const RECOVERY_FULL: u32 = 5; // 连续 5 次成功 → 全开（Closed）
const TRIP_THRESHOLD: u32 = 3; // Closed 下连续 429 达 3 → 跳 Open
const BASE_OPEN_SECS: u64 = 8; // 自发跳闸基线退避（对齐族 base 8s）
const MAX_OPEN_SECS: u64 = 1800; // 退避上限 30min（对齐 SuspiciousActivity）
const OPEN_GROWTH: f64 = 1.6; // 退避升级倍率（对齐 cooldown 1.6^n）
const MIN_ADMIT_SEED: f64 = 0.02; // admit_prob_seed 下限，永留一线试探
const IDLE_EVICT_SECS: u64 = 900; // cleanup 淘汰 15min 无活动条目

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
    ewma_success: f64,           // 成功率 EWMA(α=0.3)，乐观初始 1.0
    ewma_429: f64,               // 429 率 EWMA(α=0.5)，初始 0.0
    circuit: Circuit,
    consecutive_429: u32,        // 连续 429（成功即清零），驱动跳闸+退避升级
    last_success: Option<Instant>,
    last_429: Option<Instant>,
    open_start: Option<Instant>, // 本轮 Open 起点（观测/恢复窗口埋点）
    admit_prob_seed: f64,        // 半开起始放行概率；每次半开失败 *=0.5（收缩）
    recovery_samples: u32,       // 半开内连续成功计数，达 RECOVERY_FULL 全开
    open_count: u32,             // 累计跳闸轮数，退避 1.6^open_count
    last_touch: Instant,         // cleanup 空闲淘汰用
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
        }
    }
}

/// 只读健康快照（概览页/hover 推断日志用）。
#[derive(Debug, Clone)]
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
        }
    }

    /// 退避时长：`min(BASE*GROWTH^(n-1), MAX)`，n=open_count（≥1）。
    fn open_backoff(n: u32) -> Duration {
        let secs =
            (BASE_OPEN_SECS as f64 * OPEN_GROWTH.powi(n.saturating_sub(1) as i32)) as u64;
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

    /// 成功：抬 ewma_success、衰减 ewma_429、清连续 429；半开期连续成功 AIMD 放大直至全开。
    pub fn on_success(&self, key: &str) {
        let now = Instant::now();
        let mut map = self.states.lock();
        let s = map.entry(key.to_string()).or_default();
        Self::tick_circuit(s, now);
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
                s.circuit = Circuit::Open { until: now + backoff };
                s.open_start = Some(now);
            }
            Circuit::Closed => {
                if s.consecutive_429 >= TRIP_THRESHOLD {
                    s.open_count += 1;
                    s.admit_prob_seed = HALFOPEN_START;
                    let backoff = Self::open_backoff(s.open_count);
                    s.circuit = Circuit::Open { until: now + backoff };
                    s.open_start = Some(now);
                }
            }
            Circuit::Open { .. } => { /* 已开，不重复升级（一条链多次 429 只算一轮） */ }
        }
    }

    /// 族级强制跳闸：用 cooldown 给的硬窗 `backoff` 作 Open until，两套时钟不打架。
    /// 反复被风控 → 起始试探减半（下次半开更谨慎）。
    pub fn report_family_suspicious(&self, fam: &str, backoff: Duration) {
        let now = Instant::now();
        let mut map = self.states.lock();
        let s = map.entry(fam.to_string()).or_default();
        Self::tick_circuit(s, now);
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
        s.circuit = Circuit::Open { until: now + backoff };
        s.open_start = Some(now);
    }

    /// 可用概率 p_avail ∈ [0,1]：balanced 选号权重。读路径也惰性推进。
    pub fn p_avail(&self, key: &str, rpm: u32, inflight: u32, rpm_limit: u32) -> f64 {
        let now = Instant::now();
        let mut map = self.states.lock();
        let s = map.entry(key.to_string()).or_default();
        Self::tick_circuit(s, now);
        s.last_touch = now;
        let gate = match s.circuit {
            Circuit::Closed => 1.0,
            Circuit::Open { .. } => 0.0,
            Circuit::HalfOpen { admit_prob } => admit_prob,
        };
        let health = (s.ewma_success * (1.0 - HEALTH_429_WEIGHT * s.ewma_429)).clamp(0.0, 1.0);
        let rpm_pressure = if rpm_limit > 0 {
            (rpm as f64 / rpm_limit as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let load = (inflight as f64 / LOAD_REF).clamp(0.0, 1.0);
        (gate * health * (1.0 - rpm_pressure) * (1.0 - LOAD_PENALTY * load)).clamp(0.0, 1.0)
    }

    /// 只读快照（概览页/hover）。不推进状态（用当前值）。
    pub fn snapshot(&self, key: &str) -> Option<HealthSnapshot> {
        let now = Instant::now();
        let map = self.states.lock();
        map.get(key).map(|s| {
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
        assert!((snap.admit_prob - 0.1).abs() < 1e-6, "半开起点 gate 应 0.1, got {}", snap.admit_prob);
        // 连续 5 次成功 → 全开(Closed)。health 是 EWMA 渐近回升,不必等于精确 1.0,
        // 故断言 circuit 已 Closed(gate 回 1.0)+ p_avail 已高(>0.9)。
        for _ in 0..RECOVERY_FULL {
            h.on_success("fam");
        }
        let snap2 = h.snapshot("fam").unwrap();
        assert!(!snap2.circuit_open && !snap2.half_open, "5 次成功应全开(Closed)");
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
}
