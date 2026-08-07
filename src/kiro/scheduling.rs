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
use std::collections::{HashMap, VecDeque};
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
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                Some(v.saturating_sub(1))
            });
    }
}

/// 每凭据 RPM 滚动窗口追踪器（固定 60 秒窗口）
///
/// 记录每个凭据在最近 60 秒内被分发请求的时间戳，用于 balanced 选号时判断
/// 某号是否"接近 RPM 上限"。达到软上限的号在排序中被降权（而非硬跳过），
/// 避免全部凭据饱和时清空可用池导致请求直接失败。
pub struct RpmTracker {
    window: Duration,
    hits: Mutex<HashMap<u64, VecDeque<Instant>>>,
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
        v.push_back(now);
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

    /// 一次加锁批量读取多个凭据的窗口计数（选号热路径专用）。
    ///
    /// ## 为什么需要它
    ///
    /// 选号的排序键闭包对**每个候选**都要读 RPM，此前每次都单独调 [`Self::count`] →
    /// 每候选一次独立加锁。43 号池实测一次选号至少 43 次加锁（排序键里读 2 次 +
    /// 饱和判定 1 次，最坏 129 次），而这整段都在 `entries` 锁的临界区内 →
    /// 1000 RPM（约 17 次选号/秒）下锁竞争与临界区时长被成倍放大。
    ///
    /// 改为一次加锁取回全部候选的计数，锁获取次数从 O(n) 降到 O(1)。
    pub fn counts_for(&self, ids: &[u64]) -> std::collections::HashMap<u64, u32> {
        let now = Instant::now();
        let window = self.window;
        let mut map = self.hits.lock();
        let mut out = std::collections::HashMap::with_capacity(ids.len());
        for &id in ids {
            let n = match map.get_mut(&id) {
                Some(v) => {
                    Self::prune(v, now, window);
                    v.len() as u32
                }
                None => 0,
            };
            out.insert(id, n);
        }
        out
    }

    /// 一次加锁批量读取「近 `recent` 秒的请求数」与「窗口内总数」。
    ///
    /// 用于**爬坡（slew-rate）压力**判定：上游惩罚的是**速率的跃升**，不是绝对吞吐。
    ///
    /// # 实测依据（2026-08-04，24h 全量，按「凭据 × 分钟」配对）
    ///
    /// 控制「前一分钟完全无 429」以排除 429 放大导致的计数虚高之后：
    ///
    /// | 本分钟 / 前一分钟 | 429 率 |
    /// |---|---|
    /// | ≥5x 跃升 | **48.3%** |
    /// | 2–5x | 5.4% |
    /// | 平稳 | **0.7%** |
    ///
    /// 且与绝对速率交叉制表后，**每一档绝对速率内跃升都是主因**：
    /// 100+ req/min 平缓上量只有 **2.9%** 429，而 <50 req/min 突然跃升有 **36.4%**。
    ///
    /// 这解释了此前所有互相矛盾的观测：同一个号同样 ~90 req/min，
    /// 上一分钟是 5 就 98% 429、上一分钟是 88 就 0% 429。
    ///
    /// 返回 `(recent_count, window_count)`。调用方据此算爬坡比例。
    pub fn ramp_counts_for(
        &self,
        ids: &[u64],
        recent: Duration,
    ) -> std::collections::HashMap<u64, (u32, u32)> {
        let now = Instant::now();
        let window = self.window;
        let mut map = self.hits.lock();
        let mut out = std::collections::HashMap::with_capacity(ids.len());
        for &id in ids {
            let v = match map.get_mut(&id) {
                Some(v) => v,
                None => {
                    out.insert(id, (0u32, 0u32));
                    continue;
                }
            };
            Self::prune(v, now, window);
            // 时间戳单调递增，从队尾往前数即可，无需扫全窗。
            let recent_n = v
                .iter()
                .rev()
                .take_while(|t| now.saturating_duration_since(**t) <= recent)
                .count() as u32;
            out.insert(id, (recent_n, v.len() as u32));
        }
        out
    }

    /// 剔除窗口外的过期时间戳。
    ///
    /// ## 为什么用 `VecDeque` + 前端弹出，而不是 `Vec::retain`
    ///
    /// 时间戳是**单调递增**追加的（`record` 只在队尾 push），所以过期项必然是一段
    /// 连续的前缀。`retain` 却要扫描**全部** w 个元素并做 O(w) 移动，而从队首弹出
    /// 只需处理真正过期的那几个 —— 稳态下每次仅 0~1 个，摊还 O(1)。
    ///
    /// 规模差异（每号 200 RPM → w≈200，43 号池）：
    ///   旧：单次选号 ≈ 43 候选 × 200 = 8600 次比较，1000 RPM 下每秒约 15 万次；
    ///   新：稳态每次只弹出刚过期的那 1~2 个。
    fn prune(v: &mut VecDeque<Instant>, now: Instant, window: Duration) {
        while let Some(&front) = v.front() {
            if now.duration_since(front) >= window {
                v.pop_front();
            } else {
                // 单调递增 → 首个未过期即可停，后面的必然都在窗口内。
                break;
            }
        }
    }

    /// 该号窗口内**最老命中**距今的时长(供 L4 背压估算"最短 RPM 恢复窗口":最老那条再过
    /// `window - oldest_age` 就会过期腾出一个名额)。无命中返回 None。
    pub fn oldest_age(&self, id: u64) -> Option<Duration> {
        let now = Instant::now();
        let mut map = self.hits.lock();
        let v = map.get_mut(&id)?;
        Self::prune(v, now, self.window);
        v.front().map(|t| now.duration_since(*t))
    }

    /// 该号窗口长度(60s),供恢复窗口计算。
    pub fn window(&self) -> Duration {
        self.window
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

    /// 批量读与逐个读必须完全等价（选号热路径用前者，观测/测试仍用后者）。
    #[test]
    fn test_counts_for_matches_individual_count() {
        let tracker = RpmTracker::new();
        for _ in 0..7 {
            tracker.record(1);
        }
        for _ in 0..3 {
            tracker.record(2);
        }
        // id=3 从未出现，必须返回 0 而不是缺键
        let batch = tracker.counts_for(&[1, 2, 3]);
        assert_eq!(batch.get(&1).copied(), Some(tracker.count(1)));
        assert_eq!(batch.get(&2).copied(), Some(tracker.count(2)));
        assert_eq!(batch.get(&3).copied(), Some(0), "未出现过的号必须返回 0");
        assert_eq!(batch.len(), 3);
    }

    /// 批量读同样要剔除过期项（不能因为走了新路径就漏掉滑窗语义）。
    #[test]
    fn test_counts_for_prunes_expired() {
        let tracker = RpmTracker {
            window: Duration::from_millis(30),
            hits: Mutex::new(HashMap::new()),
        };
        tracker.record(1);
        tracker.record(2);
        assert_eq!(tracker.counts_for(&[1, 2]).get(&1).copied(), Some(1));
        std::thread::sleep(Duration::from_millis(50));
        let batch = tracker.counts_for(&[1, 2]);
        assert_eq!(batch.get(&1).copied(), Some(0), "过期项应被剔除");
        assert_eq!(batch.get(&2).copied(), Some(0));
    }

    /// 回归（滑窗剔除必须是摊还 O(1) 而非每次全扫）：大量命中下 prune 只处理过期前缀。
    ///
    /// **旧实现为何有问题**：`hits` 是 `Vec<Instant>` 且 prune 用
    /// `v.retain(|t| now - t < window)` —— 每次 record/count 都扫描**全部** w 个元素
    /// 并做 O(w) 元素移动。而选号排序键对每个候选都要读 RPM，于是单次选号
    /// ≈ O(n×w)：43 号池 × 每号 200 RPM ≈ 8600 次比较，1000 RPM（≈17 次选号/秒）
    /// 下每秒约 15 万次，且全部串行在锁内。
    ///
    /// 时间戳是单调追加的，过期项必然是连续前缀，所以改用 `VecDeque` 从队首弹出，
    /// 稳态下每次仅弹出刚过期的 0~2 个。本测试用一个**远大于**典型 w 的规模，
    /// 断言全部读取能在明显低于"全扫"的时间内完成 —— 旧实现在同规模下会因
    /// 反复全量 retain 而显著变慢。
    #[test]
    fn test_prune_is_amortized_not_full_scan() {
        let tracker = RpmTracker::new(); // 60s 窗口，期间无项过期
        const N: usize = 20_000;
        let t0 = Instant::now();
        for _ in 0..N {
            tracker.record(1);
        }
        // 窗口内无过期 → 每次 prune 都应在检查队首后立即 break（O(1)）。
        let elapsed = t0.elapsed();
        assert_eq!(tracker.count(1), N as u32);
        assert!(
            elapsed < Duration::from_secs(2),
            "{N} 次 record 耗时 {elapsed:?}：prune 应是摊还 O(1) 的队首弹出，\
             而非每次 O(w) 全量 retain"
        );
    }
}
