//! 端点自适应派发：每凭据按各端点的**实测成功率**决定流量去向。
//!
//! # 它替换了什么
//!
//! 原 `select_endpoint` 用一个**全进程共享**的 `AtomicUsize` 做 round-robin 起始偏移
//! （`provider.rs` 的 `endpoint_rotation`）。那个设计有两个硬缺陷：
//!
//! 1. **计数器与凭据无关**：号 A 和号 B 共用同一个游标，A 的请求会推动 B 的起始位置。
//!    "每个凭据自己按成功比率来派发"根本无从表达。
//! 2. **完全不看结果**：某端点对某个号恒 400（例如 ksk_ 打 `codewhisperer` 实测
//!    `The provided credential is invalid`），轮换仍会雷打不动地每隔一次就送一批请求
//!    过去白撞。撞回来的失败还会进重试预算，挤掉本来能成功的那次尝试。
//!
//! 本模块让每个 `(凭据, 端点)` 组合各自记一份成功率，选端点时**优先送到更可能成功的
//! 那个**，同时保留一条探索通道防止误判被自我实现。
//!
//! # 算法：EWMA 成功率 + 探索保底
//!
//! 候选打分 `score = ewma_success_rate`，取最高分者。三条修正：
//!
//! - **冷启动**：样本数为 0 的端点视为满分（`1.0`）并优先于任何有样本者。这让先验完全
//!   由调用方给的候选顺序（`effective_endpoint_order`）决定，且保证每个端点**至少被试
//!   一次**才可能被降权 —— 不试就不可能有样本，不可能有样本就永远不被降权，那是死锁。
//! - **衰减**：EWMA 天然让久远样本指数衰减（`ALPHA` 见下）。上游是会变的（今天 q.* 挂
//!   了明天好了），用累计成功/总数那种算法会被历史拖住几百个请求才翻身。
//! - **探索**：每 `EXPLORE_EVERY` 次选择强制走一次"非最优"候选（轮转挑选）。这是
//!   epsilon-greedy 的确定性变体 —— 选它而不是随机数，是为了让测试可断言（随机 epsilon
//!   在单测里要么固定种子要么统计断言，两者都比确定性计数器脆）。
//!
//! # 为什么不用 UCB / Thompson sampling
//!
//! 候选集极小（ksk_ 号 2 个，OAuth 号 1 个），且真实分布是"某端点直接不可用"这种
//! 近乎二值的形态，不是需要精细置信区间去权衡的多臂老虎机。UCB 的 `sqrt(ln n / n_i)`
//! 项在 n_i 很小时会主导排序，反而让恒失败端点被反复探测；Thompson 需要 RNG，同样
//! 损失可测性。确定性 EWMA + 固定探索周期在这个规模下够用且可证。
//!
//! # 与 429 封禁桶的关系（正交，不可互相覆盖）
//!
//! - `endpoint_buckets`（provider.rs）是**硬门**：被 429 封禁的桶在解封前完全不可选。
//! - 本模块是**软偏好**：只在"硬门放行的候选"之间排序。
//!
//! 两者刻意不合并：硬门是上游明确告知的限流事实（带 Retry-After 语义），软偏好是我们
//! 自己的统计推断。把推断混进硬门会让"统计上不佳"的端点被误当成"被限流"，反之会让
//! 真实限流被一次成功洗白。
//!
//! # 为什么不持久化
//!
//! 成功率是**当前上游状态**的估计，不是凭据属性。重启后从先验（候选顺序）重新学习只
//! 需几个请求就能收敛到与重启前一致的判断；而持久化会带来真正的坏处：把"上次进程存活
//! 期间 q.* 正在故障"这个**已经过期**的结论带进新进程，用陈旧数据压制健康端点。
//! 进程内状态、重启即忘，是刻意选择。

use std::collections::HashMap;

use parking_lot::Mutex;

/// EWMA 平滑系数：新样本占的权重。
///
/// 0.25 ⇒ 单个样本能把成功率拉动 1/4，约 3 个连续失败就能把一个满分端点压到 0.42
/// 以下（1→0.75→0.5625→0.42），足以让位给候选里的健康端点；反过来约 3 次连续成功
/// 也能从 0 爬回 0.58。这个响应速度是刻意的：端点级故障通常是**整体性**的（host 挂/
/// 凭据类型不被该端点接受），不是随机抖动，所以宁可反应快，误判的代价由探索通道兜住。
const ALPHA: f64 = 0.25;

/// 每多少次选择强制探索一次非最优候选。
///
/// 8 ⇒ 稳态下约 12.5% 的流量用于探索。取这个量级的依据：端点级恢复（上游修好 host）
/// 是分钟级事件，而生产 RPM 是每分钟几十到几百，12.5% 足以在一分钟内拿到多个样本
/// 完成翻转；再高就是拿正常流量去撞已知的坏端点，纯损失。
const EXPLORE_EVERY: u32 = 8;

/// 单个 `(凭据, 端点)` 组合的成功率估计。
#[derive(Debug, Clone, Copy)]
struct EndpointStat {
    /// EWMA 成功率，值域 [0.0, 1.0]。`samples == 0` 时该字段无意义（见 `score`）。
    ewma: f64,
    /// 累计样本数。仅用于区分"冷启动"与"已学习"，不参与打分（衰减由 EWMA 负责）。
    /// 饱和加法防溢出：达到 u64::MAX 后停止增长，不影响任何判定（只用于 != 0 判断与展示）。
    samples: u64,
}

impl EndpointStat {
    fn new() -> Self {
        Self {
            ewma: 1.0,
            samples: 0,
        }
    }

    /// 记一个结果并更新 EWMA。
    fn record(&mut self, success: bool) {
        let x = if success { 1.0 } else { 0.0 };
        if self.samples == 0 {
            // 首个样本直接作为初值，而不是从 1.0 往 x 靠 —— 否则第一次失败只能把
            // 成功率打到 0.75，一个恒失败的端点要 3 个样本才显出问题。
            self.ewma = x;
        } else {
            self.ewma = ALPHA * x + (1.0 - ALPHA) * self.ewma;
        }
        self.samples = self.samples.saturating_add(1);
    }

    /// 打分：冷启动（无样本）恒为 1.0 且被视为优先于任何有样本者（见 `pick` 的比较逻辑）。
    fn score(&self) -> f64 {
        if self.samples == 0 { 1.0 } else { self.ewma }
    }
}

/// 一个 `(凭据, 端点)` 组合的成功率快照（面板/接口可观测）。
#[derive(Debug, Clone)]
pub struct EndpointHealthSnapshot {
    pub credential_id: u64,
    pub endpoint: String,
    /// EWMA 成功率；无样本时为 `None`（区分"没数据"与"成功率 0"，
    /// 两者在面板上完全不是一回事）。
    pub success_rate: Option<f64>,
    pub samples: u64,
}

/// 端点自适应派发表：`(credential_id, endpoint_name)` → 成功率。
///
/// 用一把 `parking_lot::Mutex` 保护整张表。选择时的临界区只做纯内存比较（无 IO、无
/// await、不调用任何可能反向取锁的外部函数），因此不存在锁顺序问题 —— 这是刻意的：
/// `token_manager.rs:5400` 那条"`family_key_of` 必须在锁外调用"的历史教训说明，
/// 在持锁期间调用会二次取锁的函数是本仓踩过的坑。
#[derive(Debug, Default)]
pub struct EndpointHealth {
    stats: Mutex<HashMap<(u64, String), EndpointStat>>,
    /// 探索节拍计数器。与 stats 同锁保护（放一起省一次加锁，且两者总是一起访问）。
    explore_tick: Mutex<u32>,
}

/// 进程级共享实例（供 admin 面板读快照）。
///
/// # 为什么用进程级全局而不是从 provider 传进 admin
///
/// `AdminService` 只持有 `token_manager`，**不持有** `KiroProvider`（实测
/// `admin/service.rs:381-401` 的字段列表）。要让面板读到这张表，三条路：
/// 1. 给 admin 加一条 `Arc<KiroProvider>` 依赖 —— 跨层耦合，且 provider 构造在 admin 之后，
///    接线要改 `main.rs` 的装配顺序；
/// 2. 把表挪进 `token_manager` —— 但它是**调度**层的状态，端点是**请求**层的概念，
///    放进去会让 token_manager 再多一个与它职责无关的字段（那文件已经 15k 行）；
/// 3. 进程级全局 + 面板直接读 —— 与本仓既有的 `common::recovery_metrics`
///    （`admin/handlers.rs:1054` 直接 `recovery_metrics::snapshot()`，零依赖注入）
///    **完全同一范式**。
///
/// 选 3。代价是"全局可变状态"，但它只被 provider 写、只被面板读，且语义上本就是
/// 进程唯一的一张观测表（不存在"两个号池"的场景）。
static SHARED: std::sync::OnceLock<EndpointHealth> = std::sync::OnceLock::new();

/// 取进程级共享实例。
pub fn shared() -> &'static EndpointHealth {
    SHARED.get_or_init(EndpointHealth::new)
}

impl EndpointHealth {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记一次上游结果。
    ///
    /// `success` 的口径由调用方定义，但必须满足一条：**只反映"这个端点是否愿意受理
    /// 这个凭据"**，不能把凭据自身的问题算进来。例如 402 额度耗尽、403 账号封禁是
    /// 凭据的问题（换端点也一样失败），把它们记成端点失败会污染判断、让健康端点被
    /// 无辜降权。调用点的具体分类见 `provider.rs` 的 `report_endpoint_outcome` 注释。
    pub fn record(&self, credential_id: u64, endpoint: &str, success: bool) {
        let mut stats = self.stats.lock();
        stats
            .entry((credential_id, endpoint.to_string()))
            .or_insert_with(EndpointStat::new)
            .record(success);
    }

    /// 从**已通过硬门**的候选里挑一个。
    ///
    /// `candidates` 必须已经剔除被 429 封禁的端点（硬门在 provider 侧做），且顺序承载
    /// 先验：同分时靠前者胜出。返回 `None` 仅当 `candidates` 为空。
    ///
    /// 选择规则：
    /// 1. 有任何冷启动候选（样本 0）→ 取第一个冷启动候选（保证每端点至少被试一次）。
    /// 2. 否则每 `EXPLORE_EVERY` 次强制探索：取**次优**候选（轮转，见下）。
    /// 3. 否则取最高分；同分取候选序靠前者。
    pub fn pick<'a>(&self, credential_id: u64, candidates: &[&'a str]) -> Option<&'a str> {
        if candidates.is_empty() {
            return None;
        }
        if candidates.len() == 1 {
            // 单候选时无从选择，也不该消耗探索节拍（否则单端点 OAuth 号会白白推进
            // 计数器，让多端点号的探索周期变得不可预测）。
            return Some(candidates[0]);
        }

        let stats = self.stats.lock();
        let scored: Vec<(usize, f64, u64)> = candidates
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let st = stats
                    .get(&(credential_id, (*name).to_string()))
                    .copied()
                    .unwrap_or_else(EndpointStat::new);
                (i, st.score(), st.samples)
            })
            .collect();
        drop(stats);

        // ① 冷启动优先：任何没样本的端点先试一次。
        if let Some((i, _, _)) = scored.iter().find(|(_, _, samples)| *samples == 0) {
            return Some(candidates[*i]);
        }

        // 按分数降序、同分按候选序升序，得到偏好排列。
        let mut order = scored.clone();
        order.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });

        // ② 探索节拍：每 EXPLORE_EVERY 次走一次非最优。
        let mut tick = self.explore_tick.lock();
        *tick = tick.wrapping_add(1);
        let should_explore = *tick % EXPLORE_EVERY == 0;
        let tick_now = *tick;
        drop(tick);

        if should_explore && order.len() > 1 {
            // 在"非最优"里轮转，保证候选 ≥3 时每个都能被探到（只固定取 order[1]
            // 的话，第三名永远等不到探索机会 → 它若已恢复也无从发现）。
            let alt = 1 + (tick_now as usize / EXPLORE_EVERY as usize) % (order.len() - 1);
            return Some(candidates[order[alt].0]);
        }

        Some(candidates[order[0].0])
    }

    /// 全量快照（面板可观测）。按 (凭据 id, 端点名) 排序，输出稳定便于 diff。
    pub fn snapshot(&self) -> Vec<EndpointHealthSnapshot> {
        let stats = self.stats.lock();
        let mut out: Vec<EndpointHealthSnapshot> = stats
            .iter()
            .map(|((id, ep), st)| EndpointHealthSnapshot {
                credential_id: *id,
                endpoint: ep.clone(),
                success_rate: if st.samples == 0 {
                    None
                } else {
                    Some(st.ewma)
                },
                samples: st.samples,
            })
            .collect();
        out.sort_by(|a, b| {
            a.credential_id
                .cmp(&b.credential_id)
                .then_with(|| a.endpoint.cmp(&b.endpoint))
        });
        out
    }

    /// 删除某凭据的全部端点统计（凭据被删除/purge 时调用，防表随号增删无界增长）。
    pub fn forget_credential(&self, credential_id: u64) {
        let mut stats = self.stats.lock();
        stats.retain(|(id, _), _| *id != credential_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLI: &str = "cli";
    const RT: &str = "cli-runtime";

    /// 冷启动：无任何样本时，选择完全由候选顺序（先验）决定。
    #[test]
    fn cold_start_follows_candidate_order() {
        let h = EndpointHealth::new();
        assert_eq!(h.pick(1, &[CLI, RT]), Some(CLI));
        assert_eq!(h.pick(1, &[RT, CLI]), Some(RT));
    }

    /// 每个端点至少被试一次：第一个端点有样本后，冷启动规则会把流量给还没试过的那个。
    #[test]
    fn every_endpoint_gets_tried_at_least_once() {
        let h = EndpointHealth::new();
        h.record(1, CLI, true);
        // CLI 已有样本、RT 还没有 ⇒ 必须先试 RT，否则 RT 永远拿不到样本。
        assert_eq!(h.pick(1, &[CLI, RT]), Some(RT));
    }

    /// 成功率高者胜出：CLI 恒失败、RT 恒成功 ⇒ 稳态选 RT。
    #[test]
    fn prefers_higher_success_rate() {
        let h = EndpointHealth::new();
        for _ in 0..5 {
            h.record(1, CLI, false);
            h.record(1, RT, true);
        }
        // 非探索节拍上应当稳定选 RT。EXPLORE_EVERY=8，故前 7 次里至少大部分是 RT。
        let picks: Vec<_> = (0..7).map(|_| h.pick(1, &[CLI, RT])).collect();
        assert!(
            picks.iter().all(|p| *p == Some(RT)),
            "非探索节拍应恒选成功率高的 RT，实际 {:?}",
            picks
        );
    }

    /// 每凭据独立：号 1 上 CLI 坏、号 2 上 CLI 好，两者判断互不干扰。
    #[test]
    fn per_credential_isolation() {
        let h = EndpointHealth::new();
        for _ in 0..5 {
            h.record(1, CLI, false);
            h.record(1, RT, true);
            h.record(2, CLI, true);
            h.record(2, RT, false);
        }
        assert_eq!(h.pick(1, &[CLI, RT]), Some(RT), "号 1 应选 RT");
        assert_eq!(h.pick(2, &[CLI, RT]), Some(CLI), "号 2 应选 CLI");
    }

    /// 探索不锁死：即使 CLI 恒失败，长跑中它仍会被周期性重试
    /// （否则上游修好后永远发现不了）。
    #[test]
    fn exploration_prevents_permanent_lockout() {
        let h = EndpointHealth::new();
        for _ in 0..5 {
            h.record(1, CLI, false);
            h.record(1, RT, true);
        }
        let mut saw_cli = false;
        for _ in 0..(EXPLORE_EVERY * 2) {
            if h.pick(1, &[CLI, RT]) == Some(CLI) {
                saw_cli = true;
            }
        }
        assert!(
            saw_cli,
            "恒失败端点也必须被周期性探索，否则上游恢复无从发现"
        );
    }

    /// 衰减生效：坏端点连续成功后能翻回来（EWMA 让旧样本指数衰减）。
    #[test]
    fn ewma_allows_recovery_after_upstream_heals() {
        let h = EndpointHealth::new();
        // CLI 先坏透。
        for _ in 0..5 {
            h.record(1, CLI, false);
        }
        h.record(1, RT, true);
        // 上游修好，CLI 连续成功。
        for _ in 0..8 {
            h.record(1, CLI, true);
        }
        let snap = h.snapshot();
        let cli = snap
            .iter()
            .find(|s| s.endpoint == CLI)
            .expect("应有 CLI 统计");
        assert!(
            cli.success_rate.unwrap() > 0.8,
            "连续成功后 EWMA 应回升到 0.8 以上，实际 {:?}",
            cli.success_rate
        );
    }

    /// 首个样本直接作为初值：一次失败就应显著降权，而不是被 1.0 初值拖住。
    #[test]
    fn first_sample_is_not_diluted_by_optimistic_prior() {
        let mut st = EndpointStat::new();
        st.record(false);
        assert_eq!(st.ewma, 0.0, "首个失败样本应直接把 EWMA 打到 0");
        assert_eq!(st.samples, 1);
    }

    /// 与硬门正交：pick 只在传入的候选里选，被硬门剔除的端点不可能被返回。
    #[test]
    fn never_returns_endpoint_excluded_by_hard_gate() {
        let h = EndpointHealth::new();
        // 让 RT 成为最优。
        for _ in 0..5 {
            h.record(1, CLI, false);
            h.record(1, RT, true);
        }
        // 硬门只放行 CLI（RT 被 429 封禁）⇒ 必须返回 CLI，哪怕它成功率低。
        for _ in 0..(EXPLORE_EVERY * 2) {
            assert_eq!(
                h.pick(1, &[CLI]),
                Some(CLI),
                "硬门只放行 CLI 时不得返回被封禁的 RT"
            );
        }
    }

    /// 单候选不消耗探索节拍：否则单端点号会干扰多端点号的探索周期。
    #[test]
    fn single_candidate_does_not_consume_explore_tick() {
        let h = EndpointHealth::new();
        for _ in 0..5 {
            h.record(1, CLI, false);
            h.record(1, RT, true);
        }
        // 连续大量单候选调用。
        for _ in 0..(EXPLORE_EVERY * 3) {
            assert_eq!(h.pick(1, &[RT]), Some(RT));
        }
        // 节拍未被推进 ⇒ 接下来 EXPLORE_EVERY-1 次多候选调用都应是最优解。
        let picks: Vec<_> = (0..(EXPLORE_EVERY - 1))
            .map(|_| h.pick(1, &[CLI, RT]))
            .collect();
        assert!(
            picks.iter().all(|p| *p == Some(RT)),
            "单候选不应推进探索节拍，实际 {:?}",
            picks
        );
    }

    /// 空候选返回 None（调用方据此走凭据级冷却/换号）。
    #[test]
    fn empty_candidates_returns_none() {
        let h = EndpointHealth::new();
        assert_eq!(h.pick(1, &[]), None);
    }

    /// 快照区分"无样本"与"成功率 0"。
    #[test]
    fn snapshot_distinguishes_no_data_from_zero_rate() {
        let h = EndpointHealth::new();
        h.record(1, CLI, false);
        let snap = h.snapshot();
        assert_eq!(snap.len(), 1, "只 record 过 CLI，RT 不应出现在快照里");
        assert_eq!(snap[0].success_rate, Some(0.0));
        assert_eq!(snap[0].samples, 1);
    }

    /// 凭据删除后统计被清理（防无界增长）。
    #[test]
    fn forget_credential_clears_only_that_credential() {
        let h = EndpointHealth::new();
        h.record(1, CLI, true);
        h.record(2, CLI, true);
        h.forget_credential(1);
        let snap = h.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].credential_id, 2);
    }

    /// 探索会轮转到不同的非最优候选（三候选时第三名也能被探到）。
    #[test]
    fn exploration_rotates_among_non_optimal_candidates() {
        const THIRD: &str = "codewhisperer";
        let h = EndpointHealth::new();
        // RT 最优、CLI 次之、THIRD 最差，三者都已有样本。
        for _ in 0..6 {
            h.record(1, RT, true);
        }
        h.record(1, CLI, true);
        h.record(1, CLI, false);
        for _ in 0..6 {
            h.record(1, THIRD, false);
        }
        let mut seen = std::collections::HashSet::new();
        for _ in 0..(EXPLORE_EVERY * 6) {
            if let Some(p) = h.pick(1, &[RT, CLI, THIRD]) {
                seen.insert(p);
            }
        }
        assert!(
            seen.contains(CLI) && seen.contains(THIRD),
            "探索应轮转覆盖全部非最优候选，实际见到 {:?}",
            seen
        );
    }
}
