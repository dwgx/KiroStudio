//! 用户亲和性管理模块
//!
//! 记录 user_id（会话 conversationId）与 credential_id 的绑定关系，
//! 使连续对话尽量使用同一凭据，降低单会话散落到多账号的关联风险。
//!
//! 已接线到 MultiTokenManager::select_next_credential：
//! key 取自请求 conversationId（由 converter 从 metadata.user_id 派生）。
//! 由 config.affinity_enabled 控制（默认开）。
//!
//! # 晚解绑（P2-6）
//!
//! 绑定**不是**「绑定号一不可用就解」——那会把一次**瞬态** 429 放大成会话换号，
//! 上游 prompt cache 的粘性随之丢掉（backlog P2-6 的动机）。解绑只走这几条路径：
//!
//! ① [`UserAffinityManager::note_rate_limited`]：同会话在绑定号上**连续** 429 达
//!    [`STICKY_429_UNBIND_THRESHOLD`] 次（中间任一次成功、或任一**非 429** 的凭据级
//!    惩罚，都会把计数清零 → 重新计）；
//! ② [`UserAffinityManager::remove_by_credential`]：绑定号被禁用/删除/配额耗尽/封禁
//!    （既有路径，硬失败立刻解绑）；
//! ③ [`UserAffinityManager::bind_after_selection`] 的改绑分支：无 429 粘性时的常规重绑。
//!
//! 其余情况（本请求排除集避让、绑定号 429 冷却中、RPM 饱和/并发上限、模型黑名单、
//! 熔断半开被拒）一律**保持原绑定不改写**：本请求（或本请求的这一跳）由别的号服务，
//! 会话下次仍粘回原号。熔断 **Open** 是例外——它是 8s×1.6^n（最长 30min）的硬状态，
//! 等同号不可用，必须立刻解绑；该否决由调用方（选号末尾的保持判据）注入。

use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 「同会话在绑定号上连续 429」的解绑阈值（P2-6 晚解绑）。
///
/// 取 3 的依据：k2cc 的同款语义（`report_sticky_throttled`，见
/// `research-2026-10-13-kiro/dives/k2cc.md:33`）就是「同一会话在同一账号连续 429 ≥3 次
/// 才解绑」；本仓 health 熔断的 `TRIP_THRESHOLD` 也是 3（连续 3 次 429 跳闸）。
/// ⚠️ 两者**不是**同一个判据：熔断按**族**全局计数（同族任一会话的 429 都算进去），
/// 本计数按**会话**；只有单会话独占该号时两者才在同一事件上跳变。
///
/// 风险（backlog P2-6 已点名）：调大 → 坏号粘性延长（会话在坏号上多吃几次 429 才换）；
/// 调小 → 误解绑增多（一次瞬态 429 就把会话赶走，prompt cache 粘性丢）。
const STICKY_429_UNBIND_THRESHOLD: u32 = 3;

/// 亲和性条目
struct AffinityEntry {
    credential_id: u64,
    last_used: Instant,
    /// 该会话在 `credential_id` 上**连续**吃到的 429 次数（晚解绑计数）。
    ///
    /// 生命周期与条目一致：改绑/移除/过期淘汰时一并消失（计数是条目的字段，
    /// 没有独立清理路径，故不需要额外的清扫任务）。
    /// `> 0` 同时是「晚解绑保持期」的判据（见 [`UserAffinityManager::bind_after_selection`]）。
    consecutive_429: u32,
}

/// 用户亲和性管理器
pub struct UserAffinityManager {
    affinity: Mutex<HashMap<String, AffinityEntry>>,
    ttl: Duration,
}

impl Default for UserAffinityManager {
    fn default() -> Self {
        Self::new()
    }
}

impl UserAffinityManager {
    /// 创建新的亲和性管理器（默认 TTL 30 分钟）
    pub fn new() -> Self {
        Self {
            affinity: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(30 * 60),
        }
    }

    /// 获取用户绑定的凭据 ID（惰性清理过期条目）
    pub fn get(&self, user_id: &str) -> Option<u64> {
        let mut map = self.affinity.lock();
        if let Some(entry) = map.get(user_id) {
            if entry.last_used.elapsed() < self.ttl {
                tracing::debug!(user_id = %user_id, credential_id = %entry.credential_id, "亲和性命中");
                return Some(entry.credential_id);
            }
            // 过期则删除
            tracing::debug!(user_id = %user_id, credential_id = %entry.credential_id, "亲和性过期，已清除");
            map.remove(user_id);
        }
        None
    }

    /// 更新最后使用时间（续期）
    pub fn touch(&self, user_id: &str) {
        let mut map = self.affinity.lock();
        if let Some(entry) = map.get_mut(user_id) {
            entry.last_used = Instant::now();
        }
    }

    /// 移除指定凭据的所有绑定（凭据被禁用时调用）
    pub fn remove_by_credential(&self, credential_id: u64) {
        let mut map = self.affinity.lock();
        map.retain(|_, entry| entry.credential_id != credential_id);
    }

    /// 清理过期条目（由后台定时任务周期调用，防止空闲 session 无界堆积）
    pub fn cleanup(&self) {
        let mut map = self.affinity.lock();
        let ttl = self.ttl;
        let before = map.len();
        map.retain(|_, entry| entry.last_used.elapsed() < ttl);
        let removed = before - map.len();
        if removed > 0 {
            tracing::debug!(removed, remaining = map.len(), "亲和性定时清理");
        }
    }

    /// 上游 429 上报（**会话维度**）：仅当该会话当前**绑定** `credential_id` 时 +1；
    /// 达到 [`STICKY_429_UNBIND_THRESHOLD`] 即**解绑**（移除条目，下次选号按常规重绑）。
    ///
    /// 为什么按会话而不是按号：backlog P2-6 的原文是「同一会话在同一账号连续 429 ≥3 次」。
    /// 429 打在别的号上（本请求 failover 陪跑）与本会话的绑定粘性无关，故不计数。
    /// 计数与「凭据级惩罚」解耦：`cooldownEnabled=false`（线上实际值）时冷却不生效，
    /// 但本计数照记——它不读冷却状态，只由调用方显式上报（与 `health.on_429` 同款口径）。
    ///
    /// 返回累计计数；未命中（无绑定 / 已过期 / 绑的是别的号）返回 0。
    pub fn note_rate_limited(&self, user_id: &str, credential_id: u64) -> u32 {
        let mut map = self.affinity.lock();
        let ttl = self.ttl;
        let counted = match map.get_mut(user_id) {
            Some(entry)
                if entry.last_used.elapsed() < ttl && entry.credential_id == credential_id =>
            {
                entry.consecutive_429 = entry.consecutive_429.saturating_add(1);
                Some(entry.consecutive_429)
            }
            _ => None,
        };
        match counted {
            Some(n) if n >= STICKY_429_UNBIND_THRESHOLD => {
                map.remove(user_id);
                tracing::warn!(
                    user_id = %user_id,
                    credential_id = %credential_id,
                    count = n,
                    "亲和性绑定号连续 429 达阈值，解绑（晚解绑释放）"
                );
                n
            }
            Some(n) => {
                tracing::debug!(
                    user_id = %user_id,
                    credential_id = %credential_id,
                    count = n,
                    threshold = STICKY_429_UNBIND_THRESHOLD,
                    "亲和性绑定号连续 429，未达阈值暂不解绑（晚解绑保持）"
                );
                n
            }
            None => 0,
        }
    }

    /// 清零所有「绑定 `credential_id`」的会话的连续 429 计数（**按号广播**）。
    ///
    /// # 为什么是广播而不是按会话
    ///
    /// 清零信号有两条，两条都**不携带会话维度**（也刻意不要——见下）：
    /// 1. 该号**任一次成功**（`report_success`）：号证明可用 ⇒ 所有粘它的会话的 429
    ///    粘性都该清。按会话清会让「别的会话成功过」这个更强的事实被无视。
    /// 2. 该号任一**非 429** 的凭据级惩罚（认证失败/5xx/风控/通用失败/刷新失败）：
    ///    那是与限流不同性质的失败 ——「其它失败必须仍立刻解绑」（P2-6 约束①）。
    ///    清计数即让下一次选号落回「立刻改绑」，把保持期限制在**纯 429** 序列上。
    ///
    /// # 代价
    ///
    /// 每次成功一次 O(会话数) 扫描。会话数由 TTL（30min）与 `cleanup` 约束在活跃会话
    /// 量级（数十~数百），扫描是纯字段比较，可忽略；若将来上万会话再引入 per-cred
    /// 反向索引（当前不值得为它加一份需要同步维护的索引状态）。
    pub fn clear_streaks_by_credential(&self, credential_id: u64) {
        let mut map = self.affinity.lock();
        let mut cleared = 0usize;
        for entry in map.values_mut() {
            if entry.credential_id == credential_id && entry.consecutive_429 > 0 {
                entry.consecutive_429 = 0;
                cleared += 1;
            }
        }
        if cleared > 0 {
            tracing::debug!(
                credential_id = %credential_id,
                cleared,
                "亲和性连续 429 计数已清零（该号成功 / 非 429 惩罚）"
            );
        }
    }

    /// 选号结束后的绑定写入 —— **晚解绑**（P2-6）的唯一写入口。
    ///
    /// 语义（`selected_id` = 本次真正选中的号）：
    /// - 无条目（或已过期）→ 建立绑定（绑定发生在真正选出一个号之后，失败不绑）。
    /// - 条目已绑 `selected_id` → 只续期（`last_used`），**不动**连续 429 计数。
    /// - 条目绑了别的号：
    ///   - `keep_when(bound_id, consecutive_429)` 为真 → **不改写**：本次只是临时避让
    ///     （排除集 / 429 冷却 / 饱和 / 并发上限 / 模型黑名单），会话下次仍粘回原号，
    ///     保住 prompt cache。⚠️ 也**不续期** —— 保持期不刷新 TTL，绑定最迟一个 TTL
    ///     （30min）后自然淘汰，防「坏号 + 陈旧粘性」被无限续命（这是晚解绑的兜底）。
    ///   - 为假 → 改绑到 `selected_id`，计数清零（新绑定与旧号的 429 历史无关）。
    ///
    /// `keep_when` 由调用方注入，因为判据里有一维（熔断 Open）只有调度层看得到：
    /// `MultiTokenManager` 传 `streak > 0 && !family_circuit_open(bound_id)`。
    /// 调用方**已持** `entries` 锁与本管理器锁之间无环（见调用点注释）。
    pub fn bind_after_selection<F>(&self, user_id: &str, selected_id: u64, keep_when: F)
    where
        F: FnOnce(u64, u32) -> bool,
    {
        let mut map = self.affinity.lock();
        let ttl = self.ttl;
        // 分两步写（先判存活、再取可变引用）：避免在 `get_mut` 的借用仍活着时对 map
        // 做 `insert`（借用检查器会拒；这样读起来也更清楚）。
        let live = matches!(map.get(user_id), Some(e) if e.last_used.elapsed() < ttl);
        if !live {
            // 无条目 / 已过期：建立绑定（过期条目直接覆盖）。
            tracing::debug!(user_id = %user_id, credential_id = %selected_id, "建立亲和性绑定");
            map.insert(
                user_id.to_string(),
                AffinityEntry {
                    credential_id: selected_id,
                    last_used: Instant::now(),
                    consecutive_429: 0,
                },
            );
            return;
        }
        let entry = map.get_mut(user_id).expect("上面刚判定条目存活");
        if entry.credential_id == selected_id {
            // 同号：续期（等价旧 `set` 的刷新语义），计数保留。
            entry.last_used = Instant::now();
        } else if !keep_when(entry.credential_id, entry.consecutive_429) {
            tracing::debug!(
                user_id = %user_id,
                from = entry.credential_id,
                to = selected_id,
                "亲和性改绑（原绑定不再保持）"
            );
            entry.credential_id = selected_id;
            entry.consecutive_429 = 0;
            entry.last_used = Instant::now();
        } else {
            tracing::debug!(
                user_id = %user_id,
                kept = entry.credential_id,
                avoided = selected_id,
                streak = entry.consecutive_429,
                "亲和性保持原绑定不改写（本跳临时避让，晚解绑）"
            );
        }
    }

    /// 仅供测试：指定 TTL（生产路径用 [`Self::new`] 的 30min）。
    #[cfg(test)]
    fn with_ttl(ttl: Duration) -> Self {
        Self {
            affinity: Mutex::new(HashMap::new()),
            ttl,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 夹具自检：绑定与计数都只在「该会话绑的是该号」时才动 —— 打错的号必须完全无感。
    ///
    /// 回退即 FAIL：把 `note_rate_limited` 的 `entry.credential_id == credential_id` 条件
    /// 删掉（变成「只要会话有绑定就计数」）→ 本测试第二个断言（别的号 429 不计数）红。
    #[test]
    fn note_rate_limited_counts_only_for_the_bound_credential() {
        let m = UserAffinityManager::new();
        m.bind_after_selection("s1", 7, |_, _| false);
        assert_eq!(m.get("s1"), Some(7), "夹具自检：绑定已建立");

        assert_eq!(m.note_rate_limited("s1", 9), 0, "别的号 429 不计到本会话");
        assert_eq!(m.note_rate_limited("s2", 7), 0, "没有绑定的会话不计数");
        assert_eq!(m.note_rate_limited("s1", 7), 1, "绑定号 429 计 1");
        assert_eq!(m.note_rate_limited("s1", 7), 2, "绑定号连续 429 累加");
        assert_eq!(m.get("s1"), Some(7), "未达阈值不得解绑");
    }

    /// 阈值：第 3 次 429 解绑（条目移除）；且计数**不会**跨过阈值继续存在。
    ///
    /// 回退即 FAIL：把阈值常量从 3 改成 4（或把 `map.remove` 删掉）→ 第 3 次后
    /// `get` 仍返回 Some(7)，断言红。
    #[test]
    fn note_rate_limited_releases_binding_at_threshold() {
        let m = UserAffinityManager::new();
        m.bind_after_selection("s1", 7, |_, _| false);
        assert_eq!(m.note_rate_limited("s1", 7), 1);
        assert_eq!(m.note_rate_limited("s1", 7), 2);
        assert_eq!(
            m.note_rate_limited("s1", 7),
            STICKY_429_UNBIND_THRESHOLD,
            "第 3 次应达阈值"
        );
        assert_eq!(m.get("s1"), None, "达阈值必须解绑（条目移除）");
    }

    /// 清零：① 按号广播只清「绑该号」的条目（对照条目必须留着，否则就是误清）；
    /// ② 清零后计数从 1 重新开始 —— 这是「其它失败立刻解绑」的机制：
    ///    保持期只覆盖**纯 429** 序列。
    ///
    /// 回退即 FAIL：把 `clear_streaks_by_credential` 的 `entry.credential_id ==
    /// credential_id` 条件删掉（变成全表清零）→ 对照条目 s2 的计数也归零，
    /// 第二个断言（s2 仍记到 1）红。
    #[test]
    fn clear_streaks_is_broadcast_scoped_to_the_credential() {
        let m = UserAffinityManager::new();
        m.bind_after_selection("s1", 7, |_, _| false);
        m.bind_after_selection("s2", 8, |_, _| false);
        assert_eq!(m.note_rate_limited("s1", 7), 1);
        assert_eq!(m.note_rate_limited("s2", 8), 1);

        m.clear_streaks_by_credential(7);
        // s1（绑 7）已清零：再 429 从 1 重新计
        assert_eq!(m.note_rate_limited("s1", 7), 1, "清零后应从 1 重新计数");
        // s2（绑 8）不受影响：仍是连续第 2 次
        assert_eq!(m.note_rate_limited("s2", 8), 2, "别的号的计数不得被误清");
    }

    /// 保持期不续期 TTL（兜底）：TTL 到期的条目按「不存在」处理 —— 计数既不复活
    /// 旧绑定、也不新建绑定。
    ///
    /// 回退即 FAIL：把 `note_rate_limited` 的 `entry.last_used.elapsed() < ttl`
    /// 判据删掉 → 过期条目被复活计数（返回 1 而非 0），断言红。
    #[test]
    fn expired_entry_is_not_counted_and_not_resurrected() {
        let m = UserAffinityManager::with_ttl(Duration::ZERO);
        m.bind_after_selection("s1", 7, |_, _| false);
        assert_eq!(m.note_rate_limited("s1", 7), 0, "过期条目不得计数");
        assert_eq!(m.get("s1"), None, "过期条目不得被复活");
    }

    /// 保持判据是**调用方注入**的：`keep_when` 为真时条目原样不动（含计数与
    /// `last_used` —— 保持期不续期），为假时改绑且计数清零。
    ///
    /// 回退即 FAIL：把 `keep_when` 分支删掉（无条件改绑）→ 第一个断言
    /// （仍绑 7）红；把改绑分支里的 `entry.consecutive_429 = 0` 删掉 →
    /// 第三个断言（改绑后计数从 1 起）红。
    #[test]
    fn bind_after_selection_keeps_or_rebinds_by_caller_verdict() {
        let m = UserAffinityManager::new();
        m.bind_after_selection("s1", 7, |_, _| false);
        assert_eq!(m.note_rate_limited("s1", 7), 1);

        // keep_when 真 + 计数 1 → 保持原绑定
        let seen = std::cell::Cell::new(None);
        m.bind_after_selection("s1", 9, |bound, streak| {
            seen.set(Some((bound, streak)));
            true
        });
        assert_eq!(seen.get(), Some((7, 1)), "回调必须拿到（绑定号, 计数）");
        assert_eq!(m.get("s1"), Some(7), "keep_when 真时必须保持原绑定");

        // keep_when 假 → 改绑到 9 且计数清零（下一次 429 从 1 重新计）
        m.bind_after_selection("s1", 9, |_, _| false);
        assert_eq!(m.get("s1"), Some(9), "keep_when 假时必须改绑");
        assert_eq!(m.note_rate_limited("s1", 9), 1, "改绑后计数必须清零重计");
    }
}
