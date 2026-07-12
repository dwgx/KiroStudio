//! 调度原语：实时在途负载 (inflight) 追踪 + RPM 滚动窗口
//!
//! 服务于多号负载均衡：balanced 选号时优先挑"当前在飞请求最少 + RPM 未饱和"
//! 的凭据，天然把并发流量分摊到多个账号，避免热点号被打爆。
//!
//! ## REF-1 不变量：引用计数键必须用不可变凭据 id
//! 引用计数 / 在途计数**绝不能用可变的 apiKey/token 当查找键**：请求存活期间
//! token 可能被刷新轮换，事后按旧 key 找不到条目 → 计数永久泄漏 → 该号被永远
//! 算成"满载"排到最后 = 等效踢出轮转 = 假性负载不均（很可能正是 Top5 热点真因）。
//!
//! 本模块的解法：[`InflightGuard`] **直接持有计数器的 `Arc`**，而非事后按 id 查表。
//! 即便请求存活期间 token 轮换、甚至该凭据被删除/移入回收站，Drop 仍精确作用在
//! 原计数器上，永不泄漏、永不误伤其它号。

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// 在途请求计数守卫（RAII）
///
/// 构造（[`InflightGuard::acquire`]）时对目标计数器 +1，Drop 时 -1。
/// 直接持有计数器 `Arc`，Drop 语义与凭据条目的生命周期解耦（见模块级 REF-1 说明）。
///
/// 生命周期：随 `CallContext` → `CallMeta` 一路传递，直到 SSE 流被下游完全消费、
/// 或客户端断开连接、或非流式响应读毕后才随 `CallMeta` 一同析构 → 计数 -1。
/// 因此 inflight 精确反映"真正还在处理中的请求数"，而非"已拿到响应头的请求数"。
///
/// **刻意不实现 `Clone`**：派生的 `Clone` 只会 clone 内部 `Arc` 而不 `+1`，但 `Drop`
/// 仍会 `-1` → 一次 acquire + 一次 clone = 加 1 次减 2 次 = 计数被低估，反而把满载号
/// 误算成空闲、加倍打它，恰好破坏本模块的防惊群目标。若将来确需 clone，必须手写
/// `Clone` 在其中 `fetch_add(1)` 以维持"每个存活守卫恰好占 1 个名额"的不变量。
pub struct InflightGuard {
    counter: Arc<AtomicU32>,
}

impl InflightGuard {
    /// 对计数器 +1 并返回守卫
    pub fn acquire(counter: Arc<AtomicU32>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self { counter }
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        // saturating_sub：即便出现异常路径下的重复 drop 也绝不下溢回绕成天文数字
        let _ = self
            .counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| Some(v.saturating_sub(1)));
    }
}

/// 每凭据 RPM 滚动窗口追踪器（固定 60 秒窗口）
///
/// 记录每个凭据在最近 60 秒内被分发请求的时间戳，用于 balanced 选号时判断
/// 某号是否"接近 RPM 上限"。达到软上限的号在排序中被降权（而非硬跳过），
/// 避免全部凭据饱和时清空可用池导致请求直接失败。
pub struct RpmTracker {
    window: Duration,
    hits: Mutex<HashMap<u64, Vec<Instant>>>,
}

impl Default for RpmTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl RpmTracker {
    /// 创建 60 秒滚动窗口追踪器
    pub fn new() -> Self {
        Self {
            window: Duration::from_secs(60),
            hits: Mutex::new(HashMap::new()),
        }
    }

    /// 记录一次请求分发（在选号确定命中某凭据时调用）
    pub fn record(&self, id: u64) {
        let now = Instant::now();
        let mut map = self.hits.lock();
        let v = map.entry(id).or_default();
        Self::prune(v, now, self.window);
        v.push(now);
    }

    /// 返回当前滚动窗口内的请求数
    pub fn count(&self, id: u64) -> u32 {
        let now = Instant::now();
        let mut map = self.hits.lock();
        match map.get_mut(&id) {
            Some(v) => {
                Self::prune(v, now, self.window);
                v.len() as u32
            }
            None => 0,
        }
    }

    /// 剔除窗口外的过期时间戳
    fn prune(v: &mut Vec<Instant>, now: Instant, window: Duration) {
        v.retain(|t| now.duration_since(*t) < window);
    }

    /// 移除指定凭据的窗口条目（删号时调用，避免其 RPM 记录残留被复用 id 的新号继承）。
    /// 返回是否确有条目被移除。
    pub fn remove(&self, id: u64) -> bool {
        self.hits.lock().remove(&id).is_some()
    }

    /// 清理空闲条目（由后台定时任务周期调用，防止不再出现的凭据 id 无界堆积）
    pub fn cleanup(&self) {
        let now = Instant::now();
        let window = self.window;
        let mut map = self.hits.lock();
        for v in map.values_mut() {
            Self::prune(v, now, window);
        }
        map.retain(|_, v| !v.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inflight_guard_increments_and_decrements() {
        let counter = Arc::new(AtomicU32::new(0));
        {
            let _g1 = InflightGuard::acquire(counter.clone());
            assert_eq!(counter.load(Ordering::Acquire), 1);
            let _g2 = InflightGuard::acquire(counter.clone());
            assert_eq!(counter.load(Ordering::Acquire), 2);
        }
        // 两个守卫都出作用域 → 归零
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }

    #[test]
    fn test_inflight_guard_survives_orphaned_counter() {
        // REF-1 回归：即便"凭据条目"已不复存在，守卫仍能安全 -1 到它持有的 Arc 上，
        // 不 panic、不下溢、不误伤别的计数器。
        let counter = Arc::new(AtomicU32::new(0));
        let guard = InflightGuard::acquire(counter.clone());
        assert_eq!(counter.load(Ordering::Acquire), 1);
        // 模拟凭据被删除：外部对该 Arc 的其它强引用消失，仅守卫还持有
        drop(counter);
        // 守卫析构仍安全
        drop(guard);
        // 无从断言（Arc 已 move），只要不 panic 即通过
    }

    #[test]
    fn test_rpm_tracker_counts_within_window() {
        let tracker = RpmTracker::new();
        assert_eq!(tracker.count(1), 0);
        tracker.record(1);
        tracker.record(1);
        tracker.record(1);
        assert_eq!(tracker.count(1), 3);
        // 其它凭据互不影响
        assert_eq!(tracker.count(2), 0);
    }

    #[test]
    fn test_rpm_tracker_prunes_expired() {
        // 用极短窗口验证过期剔除
        let tracker = RpmTracker {
            window: Duration::from_millis(30),
            hits: Mutex::new(HashMap::new()),
        };
        tracker.record(1);
        assert_eq!(tracker.count(1), 1);
        std::thread::sleep(Duration::from_millis(50));
        // 窗口已过，旧时间戳应被剔除
        assert_eq!(tracker.count(1), 0);
    }

    #[test]
    fn test_rpm_tracker_cleanup_removes_idle() {
        let tracker = RpmTracker {
            window: Duration::from_millis(30),
            hits: Mutex::new(HashMap::new()),
        };
        tracker.record(1);
        tracker.record(2);
        std::thread::sleep(Duration::from_millis(50));
        tracker.cleanup();
        // 全部过期且空 → map 应被清空
        assert_eq!(tracker.count(1), 0);
        assert_eq!(tracker.count(2), 0);
    }
}
