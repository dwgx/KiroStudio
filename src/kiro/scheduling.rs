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

/// RPM 滑窗长度（秒）。**单一真相源**：`RpmTracker::new()` 的窗口时长与
/// 爬坡折算（`RAMP_RECENT_SECS` 除它）都从这里取，改窗口只动这一处。
/// `RAMP_RECENT_SECS` 必须能整除它（折算要求）。
pub(crate) const RPM_WINDOW_SECS: u64 = 60;

/// 爬坡压力判定的「近期」窗口（秒）。必须能整除 [`RPM_WINDOW_SECS`]（用于折算分钟值）。
///
/// 取 10s：足够短到能在一分钟内**及早**看出跃升（不必等整分钟过完），
/// 又足够长到不被三五个请求的抖动主导。
pub(crate) const RAMP_RECENT_SECS: u32 = 10;

/// 爬坡判定所需的最小窗口样本数。低于此值不判（返回档位 0）。
///
/// 新入池的号与低负载时段窗口内本来就只有几个请求，比值会剧烈抖动
/// （1 → 3 就是 3x）。对这些情形判爬坡只会误伤：它们**应该**被逐步加量，
/// 而不是被排序键压住不给流量。取 20 ≈ 实测健康号一分钟的下限量级。
pub(crate) const RAMP_MIN_SAMPLES: u32 = 20;

/// 爬坡压力档（slew-rate 分档）：近 [`RAMP_RECENT_SECS`] 的速率折算成分钟值，
/// 与整 `RPM_WINDOW_SECS` 窗口均值（即 `total` 本身）比。比值越大 = 正在被猛灌
/// → 档位越高 → 同健康档内让路给「已经平稳在跑」的号。
///
/// 实测依据（24h 全量，控制「前一分钟无 429」以排除 429 放大的计数虚高）：
/// ≥5x 跃升 → 48.3% 429 ／ 2~5x → 5.4% ／ 平稳 → 0.7%，**69 倍**；
/// 且与绝对速率交叉后每一档内跃升都是主因（100+ req/min 平缓只有 2.9%，
/// <50 req/min 突然跃升有 36.4%）。
///
/// 返回值：0 = 平稳（或样本不足 `RAMP_MIN_SAMPLES` 不判），1 = 2~5x，2 = ≥5x。
///
/// 消费点：Kiro 主路径排序键第⑤位与透传池排序键第 2 键（token_manager.rs），
/// 两处共用本函数，改档位/改窗口折算只动这里，防两池分叉。
pub(crate) fn ramp_tier_of(recent: u32, total: u32) -> u8 {
    // 窗口内样本太少时不判（新号/低负载，判了只会误伤）。
    if total < RAMP_MIN_SAMPLES {
        0u8
    } else {
        // 近 RAMP_RECENT_SECS 秒折算分钟值 vs 窗口均值（即 total 本身，窗口正是 RPM_WINDOW_SECS）。
        let projected = recent as u64 * (RPM_WINDOW_SECS / RAMP_RECENT_SECS as u64);
        let base = total.max(1) as u64;
        if projected >= base * 5 {
            2 // ≥5x：实测 48.3% 429
        } else if projected >= base * 2 {
            1 // 2~5x：实测 5.4%
        } else {
            0 // 平稳：实测 0.7%
        }
    }
}

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
///
/// ## 模型维度（2026-08-14 新增）
/// `model_hits` 是 `hits` 的同构细分：每 (凭据, 模型) 的近期分发计数，供选号排序
/// 把「正在被同一爆款模型猛灌的号」与「该模型最近没打过的号」区分开，把热点模型
/// 摊到整池。键名用**选号时的原始模型名**（模型映射发生在选号之后，选号侧只有
/// 原始名）——与白名单/模型黑名单同源同口径，同一请求的两种计数落在同一模型名下。
/// 阈值/上限刻意不新增：模型级只是分流计数，饱和判定仍复用每凭据 rpm_limit。
pub struct RpmTracker {
    window: Duration,
    hits: Mutex<HashMap<u64, VecDeque<Instant>>>,
    model_hits: Mutex<HashMap<(u64, String), VecDeque<Instant>>>,
}

impl Default for RpmTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl RpmTracker {
    /// 创建 RPM 滚动窗口追踪器（窗口长度见 [`RPM_WINDOW_SECS`]）
    pub fn new() -> Self {
        Self {
            window: Duration::from_secs(RPM_WINDOW_SECS),
            hits: Mutex::new(HashMap::new()),
            model_hits: Mutex::new(HashMap::new()),
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

    /// 记录一次「该凭据 × 该模型」的分发（与 [`Self::record`] 同点调用，模型级分流计数）。
    ///
    /// `model` 为选号时的原始模型名（映射发生在选号之后，见模块级说明）；
    /// 调用方对空模型名负责（无模型语义的调用不调本方法，避免空键条目堆积）。
    pub fn record_model(&self, id: u64, model: &str) {
        let now = Instant::now();
        let mut map = self.model_hits.lock();
        let v = map.entry((id, model.to_string())).or_default();
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

    /// 一次加锁批量读取「近窗内该凭据 × 该模型」的请求数（选号热路径专用，与
    /// [`Self::counts_for`] 同款理由：排序键对每个候选都要读，逐个加锁会放大竞争）。
    ///
    /// 候选们的模型名相同（同一请求同一次选号），故对每个候选按 `(id, model)` 键
    /// 做一次哈希查找即可覆盖全部候选——与 `counts_for` 的 `get_mut(&id)` 同构，
    /// n 次哈希查找替代此前的全表扫描（遍历**全部** (凭据, 模型) 条目做字符串
    /// 比较；M 通常远大于 n，43 号 × 20 模型 ≈ 860 条目）。未出现的
    /// (凭据, 模型) 组合返回 0（与 `counts_for` 的缺键语义一致）。
    pub fn model_counts_for(&self, ids: &[u64], model: &str) -> std::collections::HashMap<u64, u32> {
        let now = Instant::now();
        let window = self.window;
        let mut map = self.model_hits.lock();
        // 键是 (u64, String)，而查询键只有 &str——(u64, String) 没有 Borrow 到
        // (u64, &str) 的桥，必须按候选构造临时键（一次 hash + 短字符串分配，
        // 仍远优于 M 次长模型名比较）。
        let mut out = std::collections::HashMap::with_capacity(ids.len());
        for &id in ids {
            let n = match map.get_mut(&(id, model.to_string())) {
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

    /// 该号窗口内**第 `k` 老**（k=1 即最老）命中距今的时长；窗口内不足 `k` 条时返 None。
    ///
    /// 与 [`Self::oldest_age`] 的区别：`oldest_age` 只答「第一个名额何时释放」，本方法答
    /// 「第 k 个名额何时释放」——L4 背压在 limit 被**热调低**（窗口内计数 fresh_count >
    /// limit）时需要等到第 `fresh_count - limit + 1` 条过期，窗口内才回落到限值内，
    /// 用 `oldest_age` 会低估恢复时间、回给客户端的 Retry-After 偏小。
    ///
    /// 实现：per-id `VecDeque` 时间戳单调递增（`record` 只 push 队尾），`VecDeque::get`
    /// 按索引直接取，O(1) 无需队尾扫描。
    pub fn kth_oldest_age(&self, id: u64, k: u32) -> Option<Duration> {
        if k == 0 {
            return None;
        }
        let now = Instant::now();
        let mut map = self.hits.lock();
        let v = map.get_mut(&id)?;
        Self::prune(v, now, self.window);
        v.get(k as usize - 1).map(|t| now.duration_since(*t))
    }

    /// 该号窗口长度(60s),供恢复窗口计算。
    pub fn window(&self) -> Duration {
        self.window
    }

    /// 移除指定凭据的窗口条目（删号时调用，避免其 RPM 记录残留被复用 id 的新号继承）。
    /// 模型维度同款清理：该凭据的全部 (id, 模型) 条目一并移除。
    /// 返回是否确有条目被移除。
    pub fn remove(&self, id: u64) -> bool {
        let had_hit = self.hits.lock().remove(&id).is_some();
        let mut mh = self.model_hits.lock();
        let had_model = mh.keys().any(|(cid, _)| *cid == id);
        mh.retain(|(cid, _), _| *cid != id);
        had_hit || had_model
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
        // 模型维度同款清理：过期条目剔除后空 (id, 模型) 组合一并移除，防无界堆积。
        let mut mh = self.model_hits.lock();
        for v in mh.values_mut() {
            Self::prune(v, now, window);
        }
        mh.retain(|_, v| !v.is_empty());
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
            model_hits: Mutex::new(HashMap::new()),
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
            model_hits: Mutex::new(HashMap::new()),
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
            model_hits: Mutex::new(HashMap::new()),
        };
        tracker.record(1);
        tracker.record(2);
        assert_eq!(tracker.counts_for(&[1, 2]).get(&1).copied(), Some(1));
        std::thread::sleep(Duration::from_millis(50));
        let batch = tracker.counts_for(&[1, 2]);
        assert_eq!(batch.get(&1).copied(), Some(0), "过期项应被剔除");
        assert_eq!(batch.get(&2).copied(), Some(0));
    }

    /// 模型维度：每 (凭据 × 模型) 独立计数，且与每凭据计数互不干扰。
    #[test]
    fn test_rpm_tracker_model_dimension_counts_per_cred_model() {
        let tracker = RpmTracker::new();
        tracker.record_model(1, "claude-sonnet-4-5");
        tracker.record_model(1, "claude-sonnet-4-5");
        tracker.record_model(1, "claude-opus-4-8");
        tracker.record_model(2, "claude-sonnet-4-5");
        let batch = tracker.model_counts_for(&[1, 2], "claude-sonnet-4-5");
        assert_eq!(batch.get(&1).copied(), Some(2), "#1 的 sonnet 计数");
        assert_eq!(batch.get(&2).copied(), Some(1), "#2 的 sonnet 计数");
        let other = tracker.model_counts_for(&[1, 2], "claude-opus-4-8");
        assert_eq!(other.get(&1).copied(), Some(1), "#1 的 opus 计数独立");
        assert_eq!(
            other.get(&2).copied(),
            Some(0),
            "未出现过的 (凭据, 模型) 组合必须返回 0 而不是缺键"
        );
        assert_eq!(tracker.count(1), 0, "模型级计数不污染每凭据计数");
        assert_eq!(tracker.count(2), 0);
    }

    /// 模型维度：滑窗过期剔除与 cleanup 清空同样生效（与每凭据维度同款语义）。
    #[test]
    fn test_rpm_tracker_model_dimension_prunes_and_cleanup() {
        let tracker = RpmTracker {
            window: Duration::from_millis(30),
            hits: Mutex::new(HashMap::new()),
            model_hits: Mutex::new(HashMap::new()),
        };
        tracker.record_model(1, "m");
        assert_eq!(tracker.model_counts_for(&[1], "m").get(&1).copied(), Some(1));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            tracker.model_counts_for(&[1], "m").get(&1).copied(),
            Some(0),
            "过期项应被剔除"
        );
        tracker.record_model(1, "m");
        tracker.record_model(2, "m");
        std::thread::sleep(Duration::from_millis(50));
        tracker.cleanup();
        assert_eq!(
            tracker.model_counts_for(&[1, 2], "m").values().sum::<u32>(),
            0,
            "cleanup 后全部过期空条目应被清空"
        );
    }

    /// 模型维度：remove 删号时该凭据的模型级条目一并移除（防复用 id 的新号继承计数）。
    #[test]
    fn test_rpm_tracker_remove_clears_model_dimension() {
        let tracker = RpmTracker::new();
        tracker.record_model(1, "m");
        tracker.record_model(1, "m");
        assert!(tracker.remove(1), "remove 应报告确有条目被移除");
        assert_eq!(
            tracker.model_counts_for(&[1], "m").get(&1).copied(),
            Some(0),
            "删号后该号的模型级计数必须清零"
        );
        // 其它号的条目不受影响
        tracker.record_model(2, "m");
        assert_eq!(tracker.model_counts_for(&[1, 2], "m").get(&2).copied(), Some(1));
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

    /// kth_oldest_age 的序关系：第 k 老按 k 升序递减（k=1 最老），且 k=1 与
    /// oldest_age 完全一致（L4 背压新旧口径的兼容锚点）。
    #[test]
    fn test_kth_oldest_age_orders_consistent_with_oldest() {
        let tracker = RpmTracker::new();
        tracker.record(1);
        std::thread::sleep(Duration::from_millis(20));
        tracker.record(1);
        std::thread::sleep(Duration::from_millis(20));
        tracker.record(1);
        std::thread::sleep(Duration::from_millis(20));
        tracker.record(1);

        let oldest = tracker.oldest_age(1).expect("有命中必有最老年龄");
        let k1 = tracker.kth_oldest_age(1, 1).expect("k=1 即最老");
        let k2 = tracker.kth_oldest_age(1, 2).expect("窗口内 4 条，k=2 存在");
        let k3 = tracker.kth_oldest_age(1, 3).expect("窗口内 4 条，k=3 存在");
        let k4 = tracker.kth_oldest_age(1, 4).expect("窗口内 4 条，k=4 存在");

        // 两次独立调用间有微秒级流逝，用容差而非严格相等（实测 0.04ms 级抖动）。
        assert!(
            oldest.abs_diff(k1) <= Duration::from_millis(2),
            "k=1 必须约等于 oldest_age（等价时与旧行为一致），实际 {oldest:?} vs {k1:?}"
        );
        // 时间戳越老 age 越大：k 升序 → age 降序（每条间隔 ~20ms，容忍 5ms 抖动）。
        assert!(
            k1 >= k2 && k2 >= k3 && k3 >= k4,
            "序必须 k1(最老) >= k2 >= k3 >= k4，实际 {k1:?} >= {k2:?} >= {k3:?} >= {k4:?}"
        );
        assert!(
            k1 - k4 >= Duration::from_millis(30),
            "首尾年龄差应体现 3 次 sleep 间隔（约 60ms），实际 {k1:?} vs {k4:?}"
        );

        // 越界与非法 k 必须返 None：k=0 无意义；k=5 超出窗口内条数。
        assert_eq!(tracker.kth_oldest_age(1, 0), None, "k=0 无意义");
        assert_eq!(tracker.kth_oldest_age(1, 5), None, "k 超出窗口内条数");
        assert_eq!(tracker.kth_oldest_age(99, 1), None, "无命中号返 None");
    }

    /// 限流热调低（fresh_count > limit）时，第 k 老（k=fresh-limit+1）的恢复点
    /// 必须比最老一条更晚——这就是 Retry-After 精确化的依据。
    #[test]
    fn test_kth_oldest_age_release_index_later_than_oldest() {
        let tracker = RpmTracker::new();
        for _ in 0..5 {
            tracker.record(1);
            std::thread::sleep(Duration::from_millis(15));
        }
        let fresh = tracker.count(1);
        let limit = 2;
        let release_index = fresh - limit + 1; // = 4：等第 4 老过期，窗口内只剩 limit-1 条
        let release_wait = tracker
            .kth_oldest_age(1, release_index)
            .map(|age| tracker.window().saturating_sub(age))
            .expect("窗口内 5 条，k=4 必存在");
        let old_wait = tracker
            .oldest_age(1)
            .map(|age| tracker.window().saturating_sub(age))
            .expect("必有最老");
        assert!(
            release_wait > old_wait,
            "limit 热调低后恢复点必须更晚：release_wait={release_wait:?} 应 > old_wait={old_wait:?}"
        );
    }
}
