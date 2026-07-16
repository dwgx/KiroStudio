//! 入站请求整形(admission control / pacing)+ RPM 自动挡(AIMD)。
//!
//! 背景:单号被上游账户级限流(USER_REQUEST_RATE_EXCEEDED)打爆时,冷却是"号挂了之后"的补救,
//! 减不了并发。这里在**入站唯一关口** `acquire_context` 前放一个全局令牌桶:请求太快就在网关这侧
//! **异步排队等令牌**,以受控的目标 RPM 匀速滴给下游选号 + 上游——把突发削平,让号根本不被打爆。
//!
//! ## 令牌桶
//! - 容量 = target_rpm/60 × burst_secs(允许小突发),按 target_rpm 匀速补充。
//! - acquire() 有令牌立即放行;没有则 async 等到下一个令牌可用或超时(排队)。
//!
//! ## RPM 自动挡(AIMD:加性增 / 乘性减)
//! - 每隔一段时间无上游 429 → target_rpm 加性增(+step),上探到 ceiling。
//! - 收到上游 429(provider 反馈)→ target_rpm 乘性减(×0.5),下探到 floor。
//! - 自动收敛到"上游不 429 的最高稳定速率";号多了自动提速,被限了自动退档。
//!
//! 全字段原子,热路径无锁。开关关时 acquire() 直接放行(零开销)。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// 令牌桶内部状态(Mutex 守护)。令牌数用定点 ×1000 避免浮点。
struct Bucket {
    tokens_milli: u64,
    last_refill_nanos: u64,
}

/// AIMD 参数(内置,可由 config 覆盖初始值)。
const DEFAULT_STEP_UP: u32 = 10; // 每个探测周期无 429 就 +10 RPM
const AIMD_PROBE_SECS: u64 = 20; // 探测周期:距上次降档 ≥20s 且无新 429 才升档
const MD_FACTOR_PCT: u32 = 50; // 乘性减:×50%(砍半)

/// 全局入站节流器。挂在 TokenManager 上,acquire_context 进入时先 await throttle.acquire()。
pub struct GlobalThrottle {
    /// 总开关。关 = acquire() 直接放行。
    enabled: AtomicBool,
    /// 自动挡开关。关 = 固定 target_rpm(手动挡)。
    auto: AtomicBool,
    /// 当前目标 RPM(自动挡下由 AIMD 动态调整;手动挡 = 固定值)。
    target_rpm: AtomicU32,
    /// 自动挡上下限。
    rpm_min: AtomicU32,
    rpm_max: AtomicU32,
    /// 令牌桶突发容量(秒)。
    burst_secs: AtomicU32,
    /// 排队最长等待(秒),超时返回错误让上层带 Retry-After 回给客户端。
    queue_max_wait_secs: AtomicU32,

    /// 令牌桶状态:**一把轻锁**守护(令牌数×1000 + 上次补充时刻纳秒)。
    /// review 定论:补充(read-modify-write)+ 扣减若用裸原子会相互覆盖丢更新;速率整形不在
    /// CPU 热路径(每请求后面就是一次上游 HTTP,锁开销可忽略),故用 Mutex 换取可证明的正确性。
    bucket: parking_lot::Mutex<Bucket>,
    start: Instant,
    /// 排队者唤醒:补充令牌后 notify_waiters,让等待的 acquire 重试取令牌。
    notify: Notify,

    /// AIMD:上次降档(收到 429)的相对纳秒;升档要求距此 ≥AIMD_PROBE_SECS。
    last_md_nanos: AtomicU64,
    /// 上次升档探测的相对纳秒。
    last_probe_nanos: AtomicU64,
    /// 可观测:累计排队等待次数 / 降档次数 / 升档次数。
    pub queued_total: AtomicU64,
    pub md_total: AtomicU64,
    pub ai_total: AtomicU64,
}

impl GlobalThrottle {
    /// 从 config 初值构造。
    pub fn new(
        enabled: bool,
        auto: bool,
        target_rpm: u32,
        rpm_min: u32,
        rpm_max: u32,
        burst_secs: u32,
        queue_max_wait_secs: u32,
    ) -> Self {
        let target = target_rpm.clamp(rpm_min.max(1), rpm_max.max(1));
        Self {
            enabled: AtomicBool::new(enabled),
            auto: AtomicBool::new(auto),
            target_rpm: AtomicU32::new(target),
            rpm_min: AtomicU32::new(rpm_min.max(1)),
            rpm_max: AtomicU32::new(rpm_max.max(1)),
            burst_secs: AtomicU32::new(burst_secs.max(1)),
            queue_max_wait_secs: AtomicU32::new(queue_max_wait_secs.max(1)),
            bucket: parking_lot::Mutex::new(Bucket {
                // 初始给满桶(允许启动后一个小突发)。
                tokens_milli: (target as u64) * 1000 / 60 * (burst_secs.max(1) as u64),
                last_refill_nanos: 0,
            }),
            start: Instant::now(),
            notify: Notify::new(),
            last_md_nanos: AtomicU64::new(0),
            last_probe_nanos: AtomicU64::new(0),
            queued_total: AtomicU64::new(0),
            md_total: AtomicU64::new(0),
            ai_total: AtomicU64::new(0),
        }
    }

    fn now_nanos(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }

    /// 令牌桶容量上限(定点 ×1000)。
    fn capacity_milli(&self) -> u64 {
        let rpm = self.target_rpm.load(Ordering::Relaxed) as u64;
        let burst = self.burst_secs.load(Ordering::Relaxed) as u64;
        (rpm * 1000 / 60).max(1) * burst
    }

    /// 尝试取一个令牌(定点 1000):在**一把锁内**完成"按经过时间补充 → 判足 → 扣减 → 推进时钟",
    /// 补充与扣减不可分割,杜绝并发丢更新/超发。
    fn try_take(&self) -> bool {
        let cap = self.capacity_milli();
        let rpm = self.target_rpm.load(Ordering::Relaxed) as u64;
        let per_sec_milli = rpm * 1000 / 60;
        let now = self.now_nanos();
        let mut b = self.bucket.lock();
        let elapsed = now.saturating_sub(b.last_refill_nanos);
        let add = per_sec_milli.saturating_mul(elapsed) / 1_000_000_000;
        b.tokens_milli = (b.tokens_milli + add).min(cap);
        b.last_refill_nanos = now;
        if b.tokens_milli >= 1000 {
            b.tokens_milli -= 1000;
            true
        } else {
            false
        }
    }

    /// 入站准入:有令牌立即放行;否则异步排队等待,直到拿到令牌或超时。
    /// 超时返回 Err(建议 Retry-After 秒数),上层据此给客户端带 Retry-After 的 429。
    pub async fn acquire(&self) -> Result<(), u64> {
        if !self.enabled.load(Ordering::Relaxed) {
            return Ok(());
        }
        if self.try_take() {
            return Ok(());
        }
        // 需要排队。
        self.queued_total.fetch_add(1, Ordering::Relaxed);
        let deadline =
            Instant::now() + Duration::from_secs(self.queue_max_wait_secs.load(Ordering::Relaxed) as u64);
        loop {
            // 估算下一个令牌到达时间,睡到那时或被 notify 唤醒(取先到)。
            let rpm = self.target_rpm.load(Ordering::Relaxed).max(1) as u64;
            let per_token = Duration::from_millis((60_000 / rpm).max(1));
            let now = Instant::now();
            if now >= deadline {
                let retry = self.queue_max_wait_secs.load(Ordering::Relaxed) as u64;
                return Err(retry.max(1));
            }
            let wait = per_token.min(deadline.saturating_duration_since(now));
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.notify.notified() => {}
            }
            if self.try_take() {
                return Ok(());
            }
        }
    }

    /// 上游 429 反馈:乘性减档(×MD_FACTOR%),记降档时刻。provider 检到上游限流时调用。
    pub fn report_upstream_429(&self) {
        if !self.enabled.load(Ordering::Relaxed) || !self.auto.load(Ordering::Relaxed) {
            return;
        }
        let cur = self.target_rpm.load(Ordering::Relaxed);
        let floor = self.rpm_min.load(Ordering::Relaxed);
        let next = ((cur * MD_FACTOR_PCT) / 100).max(floor);
        if next != cur {
            self.target_rpm.store(next, Ordering::Relaxed);
            self.md_total.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(target: "kiro::throttle", "上游429 → RPM自动降档 {cur}→{next}(下限{floor})");
        }
        self.last_md_nanos.store(self.now_nanos(), Ordering::Relaxed);
    }

    /// 周期性探测升档:距上次降档 ≥AIMD_PROBE_SECS 且距上次升档 ≥AIMD_PROBE_SECS,无新 429 → 加性增。
    /// 由后台 tick 或每次成功请求调用(轻量,内部判周期)。
    pub fn maybe_step_up(&self) {
        if !self.enabled.load(Ordering::Relaxed) || !self.auto.load(Ordering::Relaxed) {
            return;
        }
        let now = self.now_nanos();
        let probe_gap = Duration::from_secs(AIMD_PROBE_SECS).as_nanos() as u64;
        let since_md = now.saturating_sub(self.last_md_nanos.load(Ordering::Relaxed));
        let since_probe = now.saturating_sub(self.last_probe_nanos.load(Ordering::Relaxed));
        if since_md < probe_gap || since_probe < probe_gap {
            return;
        }
        let cur = self.target_rpm.load(Ordering::Relaxed);
        let ceil = self.rpm_max.load(Ordering::Relaxed);
        if cur >= ceil {
            self.last_probe_nanos.store(now, Ordering::Relaxed);
            return;
        }
        let next = (cur + DEFAULT_STEP_UP).min(ceil);
        self.target_rpm.store(next, Ordering::Relaxed);
        self.last_probe_nanos.store(now, Ordering::Relaxed);
        self.ai_total.fetch_add(1, Ordering::Relaxed);
        self.notify.notify_waiters(); // 提速后唤醒排队者
        tracing::debug!(target: "kiro::throttle", "RPM自动升档 {cur}→{next}(上限{ceil})");
    }

    /// 热更:admin 改配置后同步各字段。
    pub fn update(
        &self,
        enabled: bool,
        auto: bool,
        target_rpm: u32,
        rpm_min: u32,
        rpm_max: u32,
        burst_secs: u32,
        queue_max_wait_secs: u32,
    ) {
        self.enabled.store(enabled, Ordering::Relaxed);
        self.auto.store(auto, Ordering::Relaxed);
        let lo = rpm_min.max(1);
        let hi = rpm_max.max(1);
        self.rpm_min.store(lo, Ordering::Relaxed);
        self.rpm_max.store(hi, Ordering::Relaxed);
        self.burst_secs.store(burst_secs.max(1), Ordering::Relaxed);
        self.queue_max_wait_secs
            .store(queue_max_wait_secs.max(1), Ordering::Relaxed);
        // target 重置策略(review 自查修复:避免无关配置保存把 AIMD 学到的档位打回初值):
        // - 手动挡(auto=false):直接用配置的 target(手动挡就该固定用它)。
        // - 自动挡(auto=true):**保留当前学到的 target**,只重新 clamp 到新上下限——否则每次保存
        //   任意无关配置都会把自动挡辛苦收敛的速率(如被 429 降到 40)打回初值(100)→ 立刻又打爆上游。
        let cur = self.target_rpm.load(Ordering::Relaxed);
        let next = if auto {
            cur.clamp(lo, hi)
        } else {
            target_rpm.clamp(lo, hi)
        };
        self.target_rpm.store(next, Ordering::Relaxed);
        self.notify.notify_waiters();
    }

    /// 当前目标 RPM(可观测)。
    pub fn current_target_rpm(&self) -> u32 {
        self.target_rpm.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(enabled: bool, auto: bool, rpm: u32) -> GlobalThrottle {
        GlobalThrottle::new(enabled, auto, rpm, 20, 300, 2, 30)
    }

    #[tokio::test]
    async fn test_disabled_passes_through() {
        let t = mk(false, true, 100);
        // 关闭时无条件放行,不消耗令牌。
        for _ in 0..1000 {
            assert!(t.acquire().await.is_ok());
        }
    }

    #[tokio::test]
    async fn test_burst_then_throttle() {
        // 100 RPM,burst 2s → 桶容量 ≈ 100/60*2 ≈ 3.3 个令牌。前几个立即过,之后要等。
        let t = mk(true, false, 100);
        let mut immediate = 0;
        for _ in 0..3 {
            // 抢初始桶(不 await 等待,用 try_take 直接测)。
            if t.try_take() {
                immediate += 1;
            }
        }
        assert!(immediate >= 1, "初始突发应有令牌立即放行,实得 {immediate}");
        // 桶抽干后 try_take 应失败(需排队)。
        while t.try_take() {}
        assert!(!t.try_take(), "桶干后应无法立即取令牌");
    }

    #[test]
    fn test_aimd_md_halves_on_429() {
        let t = mk(true, true, 200);
        assert_eq!(t.current_target_rpm(), 200);
        t.report_upstream_429();
        assert_eq!(t.current_target_rpm(), 100, "429 应砍半");
        t.report_upstream_429();
        assert_eq!(t.current_target_rpm(), 50);
        // 下探到下限 20 不再降(50→25→20)。
        t.report_upstream_429();
        assert_eq!(t.current_target_rpm(), 25);
        t.report_upstream_429();
        assert_eq!(t.current_target_rpm(), 20, "不低于下限 20");
        t.report_upstream_429();
        assert_eq!(t.current_target_rpm(), 20);
    }

    #[test]
    fn test_aimd_disabled_when_manual() {
        // 手动挡(auto=false):429 不降档。
        let t = mk(true, false, 200);
        t.report_upstream_429();
        assert_eq!(t.current_target_rpm(), 200, "手动挡不受 429 影响");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn test_concurrent_no_overadmit() {
        // review 修复验证:高并发下令牌桶不超发。60 RPM + burst 2s → 桶容量 = 60/60*2 = 2 令牌。
        // 瞬时(几乎零 elapsed)并发抢 100 次,只应有 ≈桶容量(2,±1)个成功——绝不接近 100。
        use std::sync::Arc;
        let t = Arc::new(mk(true, false, 60));
        let mut handles = vec![];
        for _ in 0..100 {
            let t = t.clone();
            handles.push(tokio::spawn(async move { t.try_take() }));
        }
        let mut ok = 0;
        for h in handles {
            if h.await.unwrap() {
                ok += 1;
            }
        }
        assert!(ok <= 4, "瞬时并发只应放行≈桶容量个令牌,实得 {ok}(超发=丢更新bug复现)");
        assert!(ok >= 1, "至少应放行初始桶里的令牌");
    }

    #[test]
    fn test_update_hot_reload() {
        let t = mk(true, true, 100);
        t.update(true, false, 150, 10, 500, 3, 45);
        assert_eq!(t.current_target_rpm(), 150);
        // 手动挡了,429 不降。
        t.report_upstream_429();
        assert_eq!(t.current_target_rpm(), 150);
    }

    #[test]
    fn test_auto_mode_preserves_learned_rpm_on_reload() {
        // review 自查修复:自动挡下 AIMD 学到的档位不应被无关配置保存打回初值。
        let t = mk(true, true, 200);
        // 模拟被 429 降档到 50。
        t.report_upstream_429(); // 200→100
        t.report_upstream_429(); // 100→50
        assert_eq!(t.current_target_rpm(), 50);
        // 无关配置保存(target 传的还是初值 200,但自动挡应保留学到的 50,只 re-clamp)。
        t.update(true, true, 200, 20, 300, 2, 30);
        assert_eq!(t.current_target_rpm(), 50, "自动挡保存无关配置不应打回初值");
        // 若新下限抬高到 80,则学到的 50 被 clamp 到 80。
        t.update(true, true, 200, 80, 300, 2, 30);
        assert_eq!(t.current_target_rpm(), 80, "学到值低于新下限时 clamp 到下限");
    }
}
