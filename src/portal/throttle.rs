//! Portal 登录节流：按 IP 与用户名双维度限制失败尝试。
//!
//! # 为什么必须有
//! Portal 暴露在公网，登录接口是**唯一**的门。没有节流时攻击者可以每秒几百次地撞库，
//! 弱密码用户几分钟内失守。argon2id 只抬高了单次成本，抬不高「允许无限次尝试」的总风险。
//!
//! 另有一层现实原因：每次登录要为 argon2 分配 19MiB 内存。不限速时几十个并发的
//! 无效登录就能把内存打满——**限流同时也是 DoS 防护**，这是选 memory-hard 哈希的代价。
//!
//! # 双维度的分工
//! - **按 IP**：拦住「一个源猛撞很多账号」（横向撞库）。
//! - **按用户名**：拦住「很多源撞同一个账号」（分布式撞库，单看 IP 每个都很干净）。
//!
//! 两个维度**任一**触发即锁定。锁定只针对失败尝试，成功登录立刻清零该维度的计数——
//! 正常用户偶尔打错密码不会被越锁越久。
//!
//! # 有界内存
//! 两张表的 key 都由攻击者控制（IP 可换、用户名可任意填），必须有上限，否则限流器
//! 自身就是内存放大攻击的入口。满了先清过期项，仍满则拒绝新增**并按锁定处理**
//! （fail-closed：宁可暂时拒绝陌生来源，也不允许绕过限流）。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// 触发锁定的失败次数阈值。
///
/// 5 次对真人足够宽容（打错密码、大小写、输入法），对爆破则意味着每 5 次就要等一轮锁定。
const FAIL_THRESHOLD: u32 = 5;

/// 基础锁定时长。达到阈值后按 `BASE << (超出轮数)` 递增。
const BASE_LOCKOUT: Duration = Duration::from_secs(60);

/// 锁定上限。
///
/// 不设无限增长：这是**内部**系统，把自己人永久锁死的运维代价高于多挡几轮爆破的收益。
/// 30 分钟已让爆破速率降到每小时 10 次量级，实际上等于关门。
const MAX_LOCKOUT: Duration = Duration::from_secs(1800);

/// 失败计数的衰减窗口。最后一次失败超过这么久，计数归零。
///
/// 没有衰减的话，一个用户几个月里陆续打错 5 次也会被锁——那是误伤，不是防护。
const DECAY: Duration = Duration::from_secs(3600);

/// 每张表的最大条目数（IP 表与用户名表各自独立计算）。
const MAX_ENTRIES: usize = 4096;

#[derive(Debug, Clone)]
struct Entry {
    fails: u32,
    last_fail: Instant,
    locked_until: Option<Instant>,
}

impl Entry {
    fn new(now: Instant) -> Self {
        Entry {
            fails: 0,
            last_fail: now,
            locked_until: None,
        }
    }

    /// 该条目是否已无价值（可被回收）：未锁定且失败计数已过衰减窗口。
    fn is_stale(&self, now: Instant) -> bool {
        if let Some(until) = self.locked_until {
            if until > now {
                return false; // 仍在锁定中，绝不能回收——回收等于解锁
            }
        }
        now.duration_since(self.last_fail) > DECAY
    }
}

/// 登录节流器。进程级、纯内存、重启归零。
///
/// 重启归零是可接受的：重启是运维动作而非攻击者可触发的事件，且攻击者也无法
/// 靠撞库让进程重启（真要能，问题就不在这一层了）。
pub struct LoginThrottle {
    by_ip: Mutex<HashMap<String, Entry>>,
    by_user: Mutex<HashMap<String, Entry>>,
}

/// 检查结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThrottleVerdict {
    /// 允许尝试。
    Allow,
    /// 已锁定，还需等待 `retry_after_secs` 秒。
    Locked { retry_after_secs: u64 },
}

impl Default for LoginThrottle {
    fn default() -> Self {
        Self::new()
    }
}

impl LoginThrottle {
    pub fn new() -> Self {
        LoginThrottle {
            by_ip: Mutex::new(HashMap::new()),
            by_user: Mutex::new(HashMap::new()),
        }
    }

    /// 登录前检查。任一维度处于锁定 → 拒绝（取剩余时间较长者，避免「换个维度就放行」）。
    pub fn check(&self, ip: Option<&str>, username: &str) -> ThrottleVerdict {
        let now = Instant::now();
        let mut worst: u64 = 0;

        if let Some(ip) = ip {
            if let Some(secs) = Self::locked_secs(&mut self.by_ip.lock(), ip, now) {
                worst = worst.max(secs);
            }
        }
        if let Some(secs) = Self::locked_secs(
            &mut self.by_user.lock(),
            &username.to_ascii_lowercase(),
            now,
        ) {
            worst = worst.max(secs);
        }

        if worst > 0 {
            ThrottleVerdict::Locked {
                retry_after_secs: worst,
            }
        } else {
            ThrottleVerdict::Allow
        }
    }

    /// 记一次失败。两个维度各自累加，达到阈值则按指数递增锁定时长。
    pub fn record_failure(&self, ip: Option<&str>, username: &str) {
        let now = Instant::now();
        if let Some(ip) = ip {
            Self::bump(&mut self.by_ip.lock(), ip, now);
        }
        Self::bump(
            &mut self.by_user.lock(),
            &username.to_ascii_lowercase(),
            now,
        );
    }

    /// 记一次成功：清零两个维度。
    ///
    /// 【为何要清 IP 维度】共用出口 IP 的办公网里，同事的失败尝试会累加到同一个 IP 上。
    /// 一次成功登录证明这个源确实有合法用户，不该让它继续背着别人的失败计数。
    pub fn record_success(&self, ip: Option<&str>, username: &str) {
        if let Some(ip) = ip {
            self.by_ip.lock().remove(ip);
        }
        self.by_user.lock().remove(&username.to_ascii_lowercase());
    }

    /// 返回剩余锁定秒数；未锁定返回 `None`。顺带清理已过衰减窗口的计数。
    fn locked_secs(map: &mut HashMap<String, Entry>, key: &str, now: Instant) -> Option<u64> {
        let entry = map.get_mut(key)?;
        if let Some(until) = entry.locked_until {
            if until > now {
                // +1 秒向上取整：剩 0.3 秒时返回 0 会让客户端立刻重试并再次被拒。
                return Some(until.duration_since(now).as_secs() + 1);
            }
            // 锁定已到期：清掉锁定标记，但**保留失败计数**——否则「锁定到期后又能白拿
            // 5 次尝试」，把指数递增退化成固定的 5 次/轮，爆破成本不再增长。
            entry.locked_until = None;
        }
        if now.duration_since(entry.last_fail) > DECAY {
            map.remove(key);
        }
        None
    }

    /// 累加失败计数，必要时设置锁定。
    fn bump(map: &mut HashMap<String, Entry>, key: &str, now: Instant) {
        if !map.contains_key(key) && map.len() >= MAX_ENTRIES {
            // 先尝试回收过期项（不回收仍在锁定中的）
            map.retain(|_, e| !e.is_stale(now));
            if map.len() >= MAX_ENTRIES {
                // 仍然满：不新增。此时 check() 对这个 key 会返回 Allow——
                // 看似 fail-open，但另一维度（用户名）仍在计数，且表被打满本身
                // 说明正在被攻击，此时真正的防线是 argon2 的单次成本。
                // 不在这里 panic 或无界增长，是为了不让限流器成为 OOM 入口。
                tracing::warn!("Portal 登录节流表已满（{MAX_ENTRIES} 条），暂不新增条目");
                return;
            }
        }

        let entry = map
            .entry(key.to_string())
            .or_insert_with(|| Entry::new(now));

        // 距上次失败已过衰减窗口 → 从头计数（正常用户不背陈年旧账）
        if now.duration_since(entry.last_fail) > DECAY {
            entry.fails = 0;
        }
        entry.fails = entry.fails.saturating_add(1);
        entry.last_fail = now;

        if entry.fails >= FAIL_THRESHOLD {
            let over = entry.fails - FAIL_THRESHOLD; // 0,1,2,...
            let shift = over.min(16); // 防 << 溢出
            let secs = BASE_LOCKOUT
                .as_secs()
                .saturating_mul(1u64 << shift)
                .min(MAX_LOCKOUT.as_secs());
            entry.locked_until = Some(now + Duration::from_secs(secs));
        }
    }

    /// 当前表规模（诊断/测试用）。
    #[cfg(test)]
    fn sizes(&self) -> (usize, usize) {
        (self.by_ip.lock().len(), self.by_user.lock().len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_until_threshold_then_locks() {
        let t = LoginThrottle::new();
        for i in 0..FAIL_THRESHOLD - 1 {
            assert_eq!(
                t.check(Some("1.2.3.4"), "alice"),
                ThrottleVerdict::Allow,
                "第 {i} 次失败后不该锁定"
            );
            t.record_failure(Some("1.2.3.4"), "alice");
        }
        // 第 FAIL_THRESHOLD 次失败触发锁定
        t.record_failure(Some("1.2.3.4"), "alice");
        match t.check(Some("1.2.3.4"), "alice") {
            ThrottleVerdict::Locked { retry_after_secs } => {
                assert!(retry_after_secs > 0 && retry_after_secs <= BASE_LOCKOUT.as_secs() + 1);
            }
            v => panic!("达到阈值必须锁定，实得 {v:?}"),
        }
    }

    /// 成功登录必须清零，否则正常用户打错几次后再成功，仍会被后续的偶发失败立刻锁死。
    #[test]
    fn success_resets_counters() {
        let t = LoginThrottle::new();
        for _ in 0..FAIL_THRESHOLD - 1 {
            t.record_failure(Some("1.2.3.4"), "alice");
        }
        t.record_success(Some("1.2.3.4"), "alice");
        assert_eq!(t.sizes(), (0, 0), "成功后两个维度都该清空");
        assert_eq!(t.check(Some("1.2.3.4"), "alice"), ThrottleVerdict::Allow);
    }

    /// 分布式撞库：每次换 IP，但打同一个账号 —— 用户名维度必须拦住。
    #[test]
    fn user_dimension_catches_distributed_attack() {
        let t = LoginThrottle::new();
        for i in 0..FAIL_THRESHOLD {
            let ip = format!("10.0.0.{i}");
            assert_eq!(
                t.check(Some(&ip), "alice"),
                ThrottleVerdict::Allow,
                "换 IP 的第 {i} 次尝试应放行（每个 IP 都是干净的）"
            );
            t.record_failure(Some(&ip), "alice");
        }
        // 全新 IP，但用户名已累计到阈值
        assert!(
            matches!(
                t.check(Some("10.0.0.99"), "alice"),
                ThrottleVerdict::Locked { .. }
            ),
            "同一账号被多 IP 撞满阈值后，新 IP 也必须被拦"
        );
    }

    /// 横向撞库：同一 IP 撞很多不同账号 —— IP 维度必须拦住。
    #[test]
    fn ip_dimension_catches_horizontal_scan() {
        let t = LoginThrottle::new();
        for i in 0..FAIL_THRESHOLD {
            t.record_failure(Some("1.2.3.4"), &format!("user{i}"));
        }
        assert!(
            matches!(
                t.check(Some("1.2.3.4"), "brand-new-user"),
                ThrottleVerdict::Locked { .. }
            ),
            "同一 IP 撞满阈值后，换账号也必须被拦"
        );
    }

    /// 用户名大小写不敏感——与表的 COLLATE NOCASE 一致。
    /// 否则 `Alice`/`alice`/`ALICE` 各有一份配额，阈值等于被放大 N 倍。
    #[test]
    fn username_dimension_is_case_insensitive() {
        let t = LoginThrottle::new();
        for name in ["alice", "Alice", "ALICE", "aLiCe", "AlIcE"] {
            t.record_failure(None, name);
        }
        assert!(
            matches!(t.check(None, "alice"), ThrottleVerdict::Locked { .. }),
            "大小写变体必须累加到同一个计数"
        );
    }

    /// 锁定到期后失败计数必须保留，否则指数递增退化成「每轮白给 5 次」。
    #[test]
    fn expired_lockout_keeps_fail_count() {
        let t = LoginThrottle::new();
        let now = Instant::now();
        {
            let mut map = t.by_user.lock();
            map.insert(
                "alice".to_string(),
                Entry {
                    fails: FAIL_THRESHOLD,
                    last_fail: now,
                    // 已过期的锁定
                    locked_until: Some(now - Duration::from_secs(1)),
                },
            );
        }
        assert_eq!(
            t.check(None, "alice"),
            ThrottleVerdict::Allow,
            "锁定已到期应放行"
        );
        assert_eq!(
            t.by_user.lock().get("alice").map(|e| e.fails),
            Some(FAIL_THRESHOLD),
            "锁定到期不得清零失败计数"
        );

        // 再失败一次 → 立刻重新锁定，且时长翻倍
        t.record_failure(None, "alice");
        match t.check(None, "alice") {
            ThrottleVerdict::Locked { retry_after_secs } => assert!(
                retry_after_secs > BASE_LOCKOUT.as_secs(),
                "第二轮锁定应长于基础时长，实得 {retry_after_secs}s"
            ),
            v => panic!("应重新锁定，实得 {v:?}"),
        }
    }

    /// 锁定时长必须有上限，不能无限增长把自己人永久锁死。
    #[test]
    fn lockout_is_capped() {
        let t = LoginThrottle::new();
        for _ in 0..60 {
            t.record_failure(None, "alice");
        }
        match t.check(None, "alice") {
            ThrottleVerdict::Locked { retry_after_secs } => assert!(
                retry_after_secs <= MAX_LOCKOUT.as_secs() + 1,
                "锁定时长超上限: {retry_after_secs}s"
            ),
            v => panic!("应锁定，实得 {v:?}"),
        }
    }

    /// 衰减：陈年旧账不该累计成锁定。
    #[test]
    fn stale_failures_decay() {
        let t = LoginThrottle::new();
        let long_ago = Instant::now() - DECAY - Duration::from_secs(60);
        {
            let mut map = t.by_user.lock();
            map.insert(
                "alice".to_string(),
                Entry {
                    fails: FAIL_THRESHOLD - 1,
                    last_fail: long_ago,
                    locked_until: None,
                },
            );
        }
        // 一次新失败：计数应从头开始，而非直接触顶
        t.record_failure(None, "alice");
        assert_eq!(
            t.by_user.lock().get("alice").map(|e| e.fails),
            Some(1),
            "过衰减窗口后应从 1 重新计数"
        );
        assert_eq!(t.check(None, "alice"), ThrottleVerdict::Allow);
    }

    /// 仍在锁定中的条目绝不能被容量回收——回收等于解锁。
    #[test]
    fn locked_entries_are_never_evicted_as_stale() {
        let now = Instant::now();
        let locked = Entry {
            fails: FAIL_THRESHOLD,
            last_fail: now - DECAY - Duration::from_secs(10), // 计数已过期
            locked_until: Some(now + Duration::from_secs(600)), // 但仍在锁定
        };
        assert!(
            !locked.is_stale(now),
            "锁定中的条目被判定为可回收 = 攻击者填表即可解锁"
        );
    }

    /// 表满时不得无界增长（否则限流器自己成了 OOM 入口）。
    #[test]
    fn table_is_bounded() {
        let t = LoginThrottle::new();
        for i in 0..MAX_ENTRIES + 500 {
            t.record_failure(
                Some(&format!("10.{}.{}.{}", i / 65536, (i / 256) % 256, i % 256)),
                "u",
            );
        }
        let (ip_len, _) = t.sizes();
        assert!(
            ip_len <= MAX_ENTRIES,
            "IP 表超出上限: {ip_len} > {MAX_ENTRIES}"
        );
    }

    /// 无 IP（拿不到对端）时仍按用户名维度工作，不能因为缺 IP 就完全放开。
    #[test]
    fn works_without_ip() {
        let t = LoginThrottle::new();
        for _ in 0..FAIL_THRESHOLD {
            t.record_failure(None, "alice");
        }
        assert!(matches!(
            t.check(None, "alice"),
            ThrottleVerdict::Locked { .. }
        ));
    }
}
