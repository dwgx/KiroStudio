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

/// 令牌桶 + AIMD 状态(**同一把 Mutex 守护**,令牌桶与 target_rpm 调整全原子)。
/// review 修复(Finding 3):target_rpm 原是裸 AtomicU32,MD 与 step_up 的 load-compute-store
/// 相互覆盖(降档被升档冲掉)。纳入锁后所有读改写不可分割。令牌数用定点 ×1000 避免浮点。
struct Bucket {
    tokens_milli: u64,
    last_refill_nanos: u64,
    /// 当前目标 RPM(AIMD 动态 / 手动挡固定)。锁内读改写。
    target_rpm: u32,
    /// 上次乘性降档(MD)的相对纳秒。用于 ①升档探测的静默期 ②MD 去抖窗口。
    last_md_nanos: u64,
    /// 上次升档探测的相对纳秒。
    last_probe_nanos: u64,
}

/// 实测入站速率的滑窗（每秒一个桶的环形缓冲）。
///
/// # 为什么需要它（🔴 这是一个真实故障的修复，不是锦上添花）
///
/// 在此之前 `inboundCurrentRpm` 直接返回 **target**（`admin/service.rs` 的
/// `inbound_current_rpm: ...inbound_target_rpm()`），即「当前速率」这个字段恒等于
/// 「目标速率」，与真实吞吐**无关**。全仓没有任何观测入站速率的计数器。
///
/// 2026-08-06 实测后果：面板显示 500 RPM，而客户端侧实际只有 50~70 RPM。
/// 两个数字差一个数量级，运维据此做过两次限流分析、差点据此改线上 `inboundTargetRpm`。
/// 而真实差距的来源是**重试放大**（实测 4.59×：1317 客户端请求 → 6040 次上游尝试），
/// 那是整形层根本看不见的量 —— 整形在 failover 循环**之外**每请求取 1 个令牌，
/// 而逐号 `RpmTracker` 在**选号时**记账，即每次 failover 尝试都记一次。
/// 两者量纲不同，混着读必然得出错误结论。
///
/// # 为什么用环形桶而不是复用 `RpmTracker`
///
/// `RpmTracker` 按凭据 id 存 `VecDeque<Instant>`，每次 record 都要 prune 一条队列。
/// 这里是**全局单一序列**且只需要总数，用 60 个定长桶即可 O(1) 记账、零分配。
///
/// # 为什么用 Mutex 而不是原子数组
///
/// 与同文件 `Bucket` 同一个理由（见其注释）：跨桶清零是 read-modify-write，裸原子会
/// 相互覆盖丢更新；而整形不在 CPU 热路径（每请求后面紧跟一次上游 HTTP），锁开销可忽略。
struct ObservedRate {
    /// 每秒一个桶，`buckets[sec % 60]`。
    buckets: [u32; OBSERVED_WINDOW_SECS],
    /// 上次记账所处的「进程启动以来的秒数」，用于判断要清掉哪些过期桶。
    last_sec: u64,
}

impl ObservedRate {
    fn new() -> Self {
        Self {
            buckets: [0; OBSERVED_WINDOW_SECS],
            last_sec: 0,
        }
    }

    /// 把 `last_sec` 推进到 `now_sec`，途经的桶全部清零。
    ///
    /// ⚠️ 跨度 ≥ 窗口时必须整体清零而不是逐桶走：空闲 1 小时后回来若逐桶推进，
    /// 要循环 3600 次；而超过一窗的历史本来就该全丢。
    fn advance(&mut self, now_sec: u64) {
        if now_sec == self.last_sec {
            return;
        }
        let gap = now_sec.saturating_sub(self.last_sec);
        if gap >= OBSERVED_WINDOW_SECS as u64 {
            self.buckets = [0; OBSERVED_WINDOW_SECS];
        } else {
            for s in (self.last_sec + 1)..=now_sec {
                self.buckets[(s as usize) % OBSERVED_WINDOW_SECS] = 0;
            }
        }
        self.last_sec = now_sec;
    }
}

/// AIMD 参数(内置,可由 config 覆盖初始值)。
const DEFAULT_STEP_UP: u32 = 10; // 每个探测周期无 429 就 +10 RPM
const AIMD_PROBE_SECS: u64 = 20; // 探测周期:距上次降档 ≥20s 且无新 429 才升档
const MD_FACTOR_PCT: u32 = 50; // 乘性减:×50%(砍半)
const MD_DEBOUNCE_SECS: u64 = 3; // MD 去抖窗口:此窗内重复 429(如单请求 failover 链)只降一档

/// 一个令牌的定点值。令牌数全程按 ×1000 定点存储以避免浮点，故"取走一个令牌"= 扣 1000 milli。
/// 桶容量必须 ≥ 此值，否则永远攒不满一个令牌（见 capacity_milli_locked 的容量塌陷说明）。
const ONE_TOKEN_MILLI: u64 = 1000;

/// 启动 slow-start 窗口（秒）。见 [`GlobalThrottle::boot_ramp_rpm`] 的完整依据。
///
/// 取 60s：`RpmTracker` 的滑窗正是 60s，所以窗口结束的那一刻，选号层的爬坡限制
/// 恰好已经攒够一整窗样本、开始正常工作 —— 两层无缝接力，中间不留空档。
const BOOT_SLOW_START_SECS: u64 = 60;

/// 实测入站速率的滑窗秒数。
///
/// 取 60 与 [`crate::kiro::scheduling::RpmTracker`] 的窗口一致：面板会把「入站实测 RPM」
/// 与「逐号 RPM 之和」并排显示，两者窗口不同会让读者以为放大倍数在变，而那只是窗口差异。
const OBSERVED_WINDOW_SECS: usize = 60;

/// 启动瞬间的有效 RPM 百分比（随后线性升到 100%）。
///
/// 取 25%：线上实测健康号平缓上量到 100+ req/min 只有 2.9% 429，而突然跃升
/// ≥5x 有 48.3%。从 25% 起步意味着首秒的跃升幅度被限制在 4x 以内（低于 5x 那档），
/// 且 60s 内均匀放开，全程不进「≥5x」区间。
///
/// 不取更低（如 10%）：重启后客户端本来就在积压，压太狠会让 queue_max_wait
/// 排满、把延迟转嫁给用户；25% 在「削平 burst」与「别让重启变成一次停服」之间。
const BOOT_START_PCT: u32 = 25;

/// 全局入站节流器。挂在 TokenManager 上,acquire_context 进入时先 await throttle.acquire()。
pub struct GlobalThrottle {
    /// 总开关。关 = acquire() 直接放行。
    enabled: AtomicBool,
    /// 自动挡开关。关 = 固定 target_rpm(手动挡)。
    auto: AtomicBool,
    /// 自动挡上下限。
    rpm_min: AtomicU32,
    rpm_max: AtomicU32,
    /// 令牌桶突发容量(秒)。
    burst_secs: AtomicU32,
    /// 排队最长等待(秒),超时后行为由 queue_timeout_passthrough 决定。
    queue_max_wait_secs: AtomicU32,
    /// 排队超时后是否放行(默认 true)而非返回 429。单号/高 RPM 不流通根治:超时放行去打上游,
    /// 最坏退化成不限速,绝不因网关排队超时把请求卡死拒绝。
    queue_timeout_passthrough: AtomicBool,

    /// 令牌桶状态:**一把轻锁**守护(令牌数×1000 + 上次补充时刻纳秒)。
    /// review 定论:补充(read-modify-write)+ 扣减若用裸原子会相互覆盖丢更新;速率整形不在
    /// CPU 热路径(每请求后面就是一次上游 HTTP,锁开销可忽略),故用 Mutex 换取可证明的正确性。
    bucket: parking_lot::Mutex<Bucket>,
    start: Instant,
    /// 排队者唤醒:补充令牌后 notify_waiters,让等待的 acquire 重试取令牌。
    notify: Notify,

    /// 可观测:累计排队等待次数 / 降档次数 / 升档次数。
    pub queued_total: AtomicU64,
    pub md_total: AtomicU64,
    pub ai_total: AtomicU64,

    /// 实测入站速率滑窗（见 [`ObservedRate`] 的完整依据）。
    ///
    /// ⚠️ 它统计的是**客户端请求数**，与逐号 `RpmTracker` 的**上游尝试数**量纲不同。
    /// 两者的比值就是重试放大倍数，别把它们当同一个量比较。
    observed: parking_lot::Mutex<ObservedRate>,
    /// 累计放行数（不受滑窗影响，用于对账"滑窗是否在正常滚动"）。
    pub admitted_total: AtomicU64,
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
        queue_timeout_passthrough: bool,
    ) -> Self {
        // ⚠️ `hi` 必须 `.max(lo)`：`u32::clamp` 在 `min > max` 时**panic**，而
        // `inbound_rpm_min` / `inbound_rpm_max` 在 admin 配置 API 里是**各自独立**
        // clamp 到 [1,100_000] 的，没有任何交叉校验 ⇒ 面板上把 min 填得比 max 大
        // （或手改 config.json）即可让进程在**启动时**panic。实测 min=500/max=300
        // 直接命中本行。这里取"上限不低于下限"而非静默交换两者：交换会让整形阈值
        // 变成用户没填过的值，更难排查。
        let lo = rpm_min.max(1);
        let hi = rpm_max.max(1).max(lo);
        let target = target_rpm.clamp(lo, hi);
        Self {
            enabled: AtomicBool::new(enabled),
            auto: AtomicBool::new(auto),
            rpm_min: AtomicU32::new(lo),
            // 存 `hi`（已 `.max(lo)`）而不是裸 `rpm_max`：否则 `rpm_max` 留 300 而
            // `target` 已被 clamp 到 lo=500，两者互相矛盾 —— `maybe_step_up` 读
            // `rpm_max` 当上限，会立刻把 target 压回 300，而 `rpm_min` 又要求 ≥500。
            rpm_max: AtomicU32::new(hi),
            burst_secs: AtomicU32::new(burst_secs.max(1)),
            queue_max_wait_secs: AtomicU32::new(queue_max_wait_secs.max(1)),
            queue_timeout_passthrough: AtomicBool::new(queue_timeout_passthrough),
            bucket: parking_lot::Mutex::new(Bucket {
                // 初始给满桶(允许启动后一个小突发)。与 capacity_milli_locked 同口径：
                // 必须 .max(ONE_TOKEN_MILLI)，否则低 target_rpm 启动时初始桶连一个令牌都装不下，
                // 首个请求就得白排队(容量塌陷)。
                //
                // ⭐ 初始桶同样按 BOOT_START_PCT 折算：否则 `try_take` 的启动爬坡
                // （按折算后 RPM 算容量）会被这个**未折算**的初始桶绕过 —— 进程刚起来
                // 那一瞬间仍放行一个满量突发，而那正是 slow-start 要削掉的东西
                // （实测重启后第一分钟是爬坡完全不设防的窗口，见 boot_ramp_rpm）。
                tokens_milli: ((((target as u64) * BOOT_START_PCT as u64 / 100) * 1000 / 60)
                    .max(1)
                    * (burst_secs.max(1) as u64))
                    .max(ONE_TOKEN_MILLI),
                last_refill_nanos: 0,
                target_rpm: target,
                last_md_nanos: 0,
                last_probe_nanos: 0,
            }),
            start: Instant::now(),
            notify: Notify::new(),
            queued_total: AtomicU64::new(0),
            md_total: AtomicU64::new(0),
            ai_total: AtomicU64::new(0),
            observed: parking_lot::Mutex::new(ObservedRate::new()),
            admitted_total: AtomicU64::new(0),
        }
    }

    fn now_nanos(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }

    /// 令牌桶容量上限(定点 ×1000)。按锁内 target_rpm 算(调用方已持锁)。
    ///
    /// ⚠️ 容量必须 ≥ ONE_TOKEN_MILLI(1000)，否则**桶永远攒不满一个令牌** → try_take 恒 false →
    /// 所有入站请求必须排满 queue_max_wait_secs(默认 30s)：passthrough=true 时全部超时放行
    /// （限速彻底失效，且每个请求白等 30s），false 时全部 429。
    ///
    /// 历史 bug（容量塌陷）：容量 = (rpm*1000/60).max(1) * burst_secs，取一个令牌需 1000 milli，
    /// 即隐含要求 `rpm * burst_secs >= 60`。默认 inbound_burst_secs=2 时，只要 target_rpm <= 29
    /// 容量就 < 1000。而 AIMD 从默认 100 连降两档即到 25（100→50→25→20=floor），
    /// rpm_min 默认 20 时容量仅 666 —— 也就是说**默认配置下一旦被上游 429 打两次降档，
    /// 整个网关的入站整形就永久塌陷**。这里用 `.max(ONE_TOKEN_MILLI)` 兜底：低 RPM 时容量
    /// 至少能装一个令牌（突发能力退化为 1，符合"低速率就该没有突发"的语义），
    /// 补充速率仍严格由 target_rpm 决定，不会超发。
    fn capacity_milli_locked(&self, target_rpm: u32) -> u64 {
        let burst = self.burst_secs.load(Ordering::Relaxed) as u64;
        (((target_rpm as u64) * 1000 / 60).max(1) * burst).max(ONE_TOKEN_MILLI)
    }

    /// 尝试取一个令牌(定点 1000):在**一把锁内**完成"按经过时间补充 → 判足 → 扣减",
    /// 补充与扣减不可分割,杜绝并发丢更新/超发。
    /// Finding 4 修复:**只把已折算成令牌的那段时间推进时钟**——按整数除法算出真正兑现的
    /// 纳秒(consumed = add_milli 对应的时间),剩余不足 1 个 milli 的零头留到下次,失败时也不吞时间。
    fn try_take(&self) -> bool {
        let now = self.now_nanos();
        let mut b = self.bucket.lock();
        // ⭐ 启动 slow-start：进程刚起来时按爬坡因子折算有效 RPM（见 boot_ramp_rpm）。
        let effective_rpm = self.boot_ramp_rpm(b.target_rpm);
        let per_sec_milli = (effective_rpm as u64) * 1000 / 60;
        let cap = self.capacity_milli_locked(effective_rpm);
        let elapsed = now.saturating_sub(b.last_refill_nanos);
        if per_sec_milli > 0 {
            let add = per_sec_milli.saturating_mul(elapsed) / 1_000_000_000;
            if add > 0 {
                b.tokens_milli = (b.tokens_milli + add).min(cap);
                // 只推进"真正兑现了 add 个 milli"所需的时间;零头(不足 1 milli 的 elapsed)留到下次累积,
                // 避免高并发下每次调用都把不足量的 elapsed 清零 → 补充被反复吞掉 → 有效 RPM 塌缩。
                let consumed_nanos = add.saturating_mul(1_000_000_000) / per_sec_milli;
                b.last_refill_nanos = b.last_refill_nanos.saturating_add(consumed_nanos);
                // 若已撞容量顶(桶满),时间戳直接对齐到 now(多余时间无意义,防 last 落后过多)。
                if b.tokens_milli >= cap {
                    b.last_refill_nanos = now;
                }
            }
        } else {
            b.last_refill_nanos = now;
        }
        if b.tokens_milli >= ONE_TOKEN_MILLI {
            b.tokens_milli -= ONE_TOKEN_MILLI;
            true
        } else {
            false
        }
    }

    /// 记一次「被放行的客户端请求」。
    ///
    /// ⚠️ 只在 [`Self::acquire`] 的**唯一出口**调用，不要在各 `return Ok(())` 分支里分别调。
    /// 原实现有四条放行路径（关闭直接放行 / 首次取到令牌 / 排队后取到 / 排队超时 passthrough），
    /// 逐条埋点意味着**将来新增一条就漏一条**，而漏了以后表现是「实测 RPM 偏低」——
    /// 与它要修的那个假数字症状一模一样，几乎不可能被发现。
    fn record_admitted(&self) {
        self.admitted_total.fetch_add(1, Ordering::Relaxed);
        let now_sec = self.start.elapsed().as_secs();
        let mut o = self.observed.lock();
        o.advance(now_sec);
        let idx = (now_sec as usize) % OBSERVED_WINDOW_SECS;
        o.buckets[idx] = o.buckets[idx].saturating_add(1);
    }

    /// 最近 60 秒**实测**放行的客户端请求数（即真实入站 RPM）。
    ///
    /// 与 [`Self::current_target_rpm`] 是两个完全不同的量：后者是配置/AIMD 的**目标**。
    /// 面板此前把 target 当 current 显示，见 [`ObservedRate`] 的故障记录。
    pub fn observed_inbound_rpm(&self) -> u32 {
        let now_sec = self.start.elapsed().as_secs();
        let mut o = self.observed.lock();
        o.advance(now_sec);
        o.buckets.iter().copied().sum()
    }

    /// AIMD 可观测三元组：`(累计排队次数, 累计降档次数, 累计升档次数)`。
    ///
    /// # 为什么补这个读取口（2026-08-10）
    ///
    /// 这三个计数器此前是**只写不读的死代码** —— 全仓（含前端）除了声明、初始化与
    /// `fetch_add` 之外**零读取点**。于是「AIMD 降了几档、升回来几次、多少请求真的排过队」
    /// 运维完全看不到，而这三个数正是判断「整形是否在起作用、是否卡在下限」的唯一依据。
    ///
    /// 这与 `CLAUDE.md` 记的那条教训同型：**先修度量，再谈调参**，否则是在算空气
    /// （历史上 `inboundTargetRpm` 就因为容量口径是假的而"怎么调都没用"）。
    ///
    /// `Relaxed` 足够：三个数只用于展示与趋势判断，不参与任何控制决策，不需要跨线程同步语义。
    pub fn aimd_counters(&self) -> (u64, u64, u64) {
        (
            self.queued_total.load(Ordering::Relaxed),
            self.md_total.load(Ordering::Relaxed),
            self.ai_total.load(Ordering::Relaxed),
        )
    }

    /// 入站准入:有令牌立即放行;否则异步排队等待,直到拿到令牌或超时。
    /// 超时返回 Err(建议 Retry-After 秒数),上层据此给客户端带 Retry-After 的 429。
    ///
    /// 放行计数收口在这里（见 [`Self::record_admitted`]），内部逻辑在 `acquire_inner`。
    pub async fn acquire(&self) -> Result<(), u64> {
        let r = self.acquire_inner().await;
        if r.is_ok() {
            self.record_admitted();
        }
        r
    }

    async fn acquire_inner(&self) -> Result<(), u64> {
        if !self.enabled.load(Ordering::Relaxed) {
            return Ok(());
        }
        if self.try_take() {
            return Ok(());
        }
        // 需要排队。
        self.queued_total.fetch_add(1, Ordering::Relaxed);
        let deadline = Instant::now()
            + Duration::from_secs(self.queue_max_wait_secs.load(Ordering::Relaxed) as u64);
        loop {
            // 估算下一个令牌到达时间,睡到那时或被 notify 唤醒(取先到)。
            let rpm = self.current_target_rpm().max(1) as u64;
            let per_token = Duration::from_millis((60_000 / rpm).max(1));
            let now = Instant::now();
            if now >= deadline {
                // 排队超时:passthrough=true(默认)则**放行**去打上游(单号/高RPM不流通根治——
                // 不因网关排队超时把请求卡死拒绝,最坏退化成不限速);false 才返回 Retry-After 让客户端退避。
                if self.queue_timeout_passthrough.load(Ordering::Relaxed) {
                    tracing::warn!(
                        target: "kiro::throttle",
                        "入站排队超时,放行去打上游(passthrough,不拒绝)"
                    );
                    return Ok(());
                }
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

    /// 上游 429 反馈:乘性减档(×MD_FACTOR%)。**锁内原子**(Finding 3)+ **去抖窗口**(Finding 2)。
    /// Finding 2 修复:单请求 failover 链会对每个 429 号各调一次,若每次都砍半 → 一波上游限流被连乘
    /// 降到 floor。加去抖:距上次 MD < MD_DEBOUNCE_SECS 内的重复 429 只更新时刻不再降档,
    /// 使"一波上游限流"至多降一档。
    pub fn report_upstream_429(&self) {
        if !self.enabled.load(Ordering::Relaxed) || !self.auto.load(Ordering::Relaxed) {
            return;
        }
        let now = self.now_nanos();
        let floor = self.rpm_min.load(Ordering::Relaxed);
        let debounce = Duration::from_secs(MD_DEBOUNCE_SECS).as_nanos() as u64;
        let mut b = self.bucket.lock();
        // 去抖:距上次**真降档**还在窗口内 → 直接返回,不再降。
        // ⚠️ 关键修复(升档饿死死锁):此分支**绝不刷新** last_md_nanos。
        //   last_md_nanos 语义 = "上次真正降档的时刻",被 maybe_step_up 用作升档静默期判据
        //   (距上次降档 ≥20s 才升档)。旧代码在去抖分支也 `last_md_nanos = now`,导致:上游只要
        //   持续零星 429(哪怕都被去抖挡掉、RPM 已在 floor 无法再降),last_md 就被反复刷成 now →
        //   升档的"距上次降档≥20s"永不满足 → RPM 卡在 floor(20)死锁不回升 → 表现为"不调度了,
        //   必须重启网关"(重启清零 last_md 才恢复)。去抖窗口本就该基于"上次真降档",不刷新后:
        //   一波 failover 链(通常几百 ms 内)仍落在同一 3s 窗、只降一档(去抖语义不变),而持续 429
        //   不再污染升档静默期。
        if b.last_md_nanos != 0 && now.saturating_sub(b.last_md_nanos) < debounce {
            return;
        }
        let cur = b.target_rpm;
        let next = ((cur * MD_FACTOR_PCT) / 100).max(floor).max(1);
        // ⚠️ 关键修复(升档饿死死锁·第二处):`last_md_nanos` 的语义严格是"上次**真正降档**的时刻",
        //   因此只有 next != cur（确实降了档）才允许刷新它。
        //   历史 bug：这里曾无条件 `b.last_md_nanos = now`，于是当 target_rpm **已经在 rpm_min 下限**
        //   （next == cur，本次并没有真降档）时，时间戳照样被推进。而 maybe_step_up 要求
        //   `since_md >= AIMD_PROBE_SECS(20s)` 才升档，于是只要上游持续零星 429（间隔 >3s 穿过
        //   去抖窗、又 <20s），last_md 就被反复刷成 now → 升档静默期永不满足 → RPM 永久卡在
        //   floor(默认 20) 再也回不去，表现为"网关突然不调度了，必须重启"。
        //   与上面那处去抖分支的修复是同一个死锁的两条触发路径：去抖分支管"3s 内重复 429"，
        //   这里管"已在下限、降不动了"。两处都不刷新，才真正闭合。
        if next != cur {
            b.target_rpm = next;
            b.last_md_nanos = now;
            drop(b);
            self.md_total.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(target: "kiro::throttle", "上游429 → RPM自动降档 {cur}→{next}(下限{floor})");
        }
    }

    /// 周期性探测升档:距上次降档 ≥AIMD_PROBE_SECS 且距上次升档 ≥AIMD_PROBE_SECS,无新 429 → 加性增。
    /// **锁内原子**(Finding 3):target_rpm 与 last_probe/md 同锁,MD 与 step_up 不再相互覆盖。
    pub fn maybe_step_up(&self) {
        if !self.enabled.load(Ordering::Relaxed) || !self.auto.load(Ordering::Relaxed) {
            return;
        }
        let now = self.now_nanos();
        let probe_gap = Duration::from_secs(AIMD_PROBE_SECS).as_nanos() as u64;
        let ceil = self.rpm_max.load(Ordering::Relaxed);
        let mut b = self.bucket.lock();
        let since_md = now.saturating_sub(b.last_md_nanos);
        let since_probe = now.saturating_sub(b.last_probe_nanos);
        if since_md < probe_gap || since_probe < probe_gap {
            return;
        }
        let cur = b.target_rpm;
        b.last_probe_nanos = now;
        if cur >= ceil {
            return;
        }
        let next = (cur + DEFAULT_STEP_UP).min(ceil);
        b.target_rpm = next;
        drop(b);
        self.ai_total.fetch_add(1, Ordering::Relaxed);
        // 只唤醒一个排队者:新令牌仅产出了一个(step=+10RPM 可能慢于实际消耗速率),
        // 其余排队者下次 per_token 定时后自然重试,避免惊群。
        self.notify.notify_one();
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
        queue_timeout_passthrough: bool,
    ) {
        self.enabled.store(enabled, Ordering::Relaxed);
        self.auto.store(auto, Ordering::Relaxed);
        self.queue_timeout_passthrough
            .store(queue_timeout_passthrough, Ordering::Relaxed);
        let lo = rpm_min.max(1);
        // 同 `new()`：`hi` 必须 `.max(lo)`，否则下面 `clamp(lo, hi)` 在 min>max 时 panic。
        // 热更路径上 panic 的后果比启动时更糟 —— 面板保存一次配置就打死正在服务的进程。
        let hi = rpm_max.max(1).max(lo);
        self.rpm_min.store(lo, Ordering::Relaxed);
        self.rpm_max.store(hi, Ordering::Relaxed);
        self.burst_secs.store(burst_secs.max(1), Ordering::Relaxed);
        self.queue_max_wait_secs
            .store(queue_max_wait_secs.max(1), Ordering::Relaxed);
        // target 重置策略(锁内,review 自查修复:避免无关配置保存把 AIMD 学到的档位打回初值):
        // - 手动挡(auto=false):直接用配置的 target(手动挡就该固定用它)。
        // - 自动挡(auto=true):**保留当前学到的 target**,只重新 clamp 到新上下限——否则每次保存
        //   任意无关配置都会把自动挡辛苦收敛的速率(如被 429 降到 40)打回初值(100)→ 立刻又打爆上游。
        let mut b = self.bucket.lock();
        b.target_rpm = if auto {
            b.target_rpm.clamp(lo, hi)
        } else {
            target_rpm.clamp(lo, hi)
        };
        drop(b);
        self.notify.notify_waiters();
    }

    /// 当前目标 RPM(可观测)。
    pub fn current_target_rpm(&self) -> u32 {
        self.bucket.lock().target_rpm
    }

    /// 启动 slow-start：把有效 RPM 从 [`BOOT_START_PCT`]% 线性升到 100%，
    /// 历时 [`BOOT_SLOW_START_SECS`] 秒。窗口过后恒等于入参（零开销、零影响）。
    ///
    /// # 为什么需要它（实测）
    ///
    /// 上游惩罚的是**速率的跃升**而非绝对吞吐：控制「前一分钟无 429」后，
    /// ≥5x 跃升 = **48.3%** 429，平稳 = **0.7%**（24h 全量，凭据×分钟配对）。
    ///
    /// 而 `RpmTracker` 是**纯内存**的 ⇒ 每次重启爬坡历史清零 ⇒ 重启后每个号
    /// `total=0` 落到「样本不足不判」分支 ⇒ **选号层的爬坡限制在重启后第一分钟
    /// 完全不设防**，客户端积压的请求瞬间满量灌向刚回池的号。
    ///
    /// 线上 20:00 起实测 **23 次重启 / 27 次热重载**（用户确认是他自己换号的脚本），
    /// 每次都造一次这样的 burst。日志里可见 20:20:30 启动、**20:20:32 就打死一个
    /// 93.9% 成功率的号**。
    ///
    /// # 为什么放在入站整形而不是选号层
    ///
    /// 选号排序键只能**重新分配**流量，不能降低总量 —— 刚重启时全池都是空白，
    /// 它无从区分谁该让路。而这里是令牌桶：超出的请求被**排队削平**（线上
    /// `inboundQueueTimeoutPassthrough=true` ⇒ 排队到期后放行而非拒绝），
    /// 所以它平滑而不拒绝，正是 slow-start 该有的语义。
    ///
    /// # 与 AIMD 的关系
    ///
    /// 这是**读时乘数**，不写 `b.target_rpm` ⇒ AIMD 的降档/升档状态完全不受影响，
    /// 窗口结束后自动回到 AIMD 自己算出来的值。若写进 target_rpm 会与 AIMD
    /// 的 load-compute-store 打架（那正是本文件 review Finding 3 修过的那类缺陷）。
    fn boot_ramp_rpm(&self, target_rpm: u32) -> u32 {
        let elapsed = self.start.elapsed().as_secs();
        if elapsed >= BOOT_SLOW_START_SECS {
            return target_rpm;
        }
        // pct 从 BOOT_START_PCT 线性升到 100。
        let span = 100u64.saturating_sub(BOOT_START_PCT as u64);
        let pct = BOOT_START_PCT as u64 + span * elapsed / BOOT_SLOW_START_SECS.max(1);
        // 下限 1：折算后为 0 会让令牌桶永不补充（capacity_milli_locked 有 .max 兜底，
        // 但补充速率为 0 时排队者只能靠 passthrough 超时放行，等于白等满 queue_max_wait）。
        (((target_rpm as u64) * pct / 100) as u32).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(enabled: bool, auto: bool, rpm: u32) -> GlobalThrottle {
        // 测试默认 passthrough=false(超时返回 Err),保持既有排队/超发测试语义不变。
        GlobalThrottle::new(enabled, auto, rpm, 20, 300, 2, 30, false)
    }

    /// 🔴 实测入站 RPM 必须是**实测**，不能等于 target。
    ///
    /// 这是那个假数字的直接回归：`inboundCurrentRpm` 曾返回 target，于是面板显示 500
    /// 而客户端实际只有 50~70。本测试钉死「没放行过任何请求时实测必须是 0，而 target 是
    /// 配置值」—— 只要有人再把 current 接回 target，这条立刻红。
    #[tokio::test]
    async fn observed_rpm_must_not_equal_target_when_idle() {
        // 注意 rpm 取 200：`mk` 的 rpm_max 是 300，传 600 会被正确 clamp 成 300，
        // 那样这条测试就在测 clamp 而不是测实测/target 的区分。
        let t = mk(true, false, 200);
        assert_eq!(
            t.current_target_rpm(),
            200,
            "target 应为配置值（对照组，证明 200 这个数确实存在）"
        );
        assert_eq!(
            t.observed_inbound_rpm(),
            0,
            "零流量时实测入站必须是 0；若它返回 200 说明又把 target 当 current 了"
        );

        // 放行 3 个请求后，实测应恰好是 3（而 target 不变）。
        for _ in 0..3 {
            t.acquire().await.expect("容量足够，不该排队超时");
        }
        assert_eq!(t.observed_inbound_rpm(), 3, "实测应等于真实放行数");
        assert_eq!(t.current_target_rpm(), 200, "target 不该被放行影响");
        assert_eq!(t.admitted_total.load(Ordering::Relaxed), 3);
    }

    /// 整形**关闭**时也必须计数。
    ///
    /// 关闭时 `acquire_inner` 在第一行就 `return Ok(())`。若把埋点写在「取到令牌」那条
    /// 分支里，关闭整形的部署实测 RPM 恒 0 —— 而线上确实存在 `inboundThrottleEnabled=false`
    /// 的配置，那种部署会完全失去入站可观测性。
    #[tokio::test]
    async fn observed_rpm_must_count_when_throttle_disabled() {
        let t = mk(false, false, 600);
        for _ in 0..5 {
            t.acquire().await.expect("关闭时必然放行");
        }
        assert_eq!(
            t.observed_inbound_rpm(),
            5,
            "整形关闭时仍须记账，否则该部署没有任何入站实测数字"
        );
    }

    /// 排队超时 passthrough 放行的请求也要计数。
    ///
    /// 那条路径**确实打了上游**，不计入会让实测 RPM 系统性偏低，而偏低正是本次要修的症状。
    #[tokio::test]
    async fn observed_rpm_must_count_passthrough_admits() {
        // rpm=1 + burst=1 → 桶极小；passthrough=true；queue_wait=0 使其立即超时放行。
        let t = GlobalThrottle::new(true, false, 1, 1, 300, 1, 0, true);
        let mut admitted = 0;
        for _ in 0..4 {
            if t.acquire().await.is_ok() {
                admitted += 1;
            }
        }
        assert_eq!(admitted, 4, "passthrough=true 时全部放行");
        assert_eq!(
            t.observed_inbound_rpm(),
            4,
            "passthrough 放行的请求打了上游，必须计入实测"
        );
    }

    /// 滑窗跨度超过一整窗时必须整体清零，而不是逐桶循环。
    ///
    /// 直接驱动 `ObservedRate` 而不是等 60 秒真实时间（测试不该睡一分钟）。
    #[test]
    fn observed_window_advance_clears_stale_buckets() {
        let mut o = ObservedRate::new();
        o.buckets[0] = 7;
        o.last_sec = 0;

        // 窗口内推进：只清途经的桶，桶 0 的值应仍在（尚未离开窗口）。
        o.advance(5);
        assert_eq!(o.buckets[0], 7, "窗口内推进不该清掉仍在窗内的桶");
        assert_eq!(o.buckets[5], 0);

        // 跨越整窗：全清。
        o.advance(5 + OBSERVED_WINDOW_SECS as u64 + 10);
        assert!(
            o.buckets.iter().all(|&b| b == 0),
            "跨度 ≥ 一窗必须整体清零"
        );
    }

    /// ⭐ 回归：`inbound_rpm_min > inbound_rpm_max` 不得 panic。
    ///
    /// `u32::clamp` 的契约是 `min <= max`，否则 **panic**。而 admin 配置 API 把
    /// `inboundRpmMin` / `inboundRpmMax` **各自独立**clamp 到 [1,100_000]，彼此不可见
    /// ⇒ 面板上把 min 填得比 max 大（或手改 config.json）即可让进程 panic。
    ///
    /// 两条路径都要覆盖：`new()` 是启动时 panic（服务起不来），`update()` 是**热更时**
    /// panic（面板保存一次配置就打死正在服务的进程，后果更糟）。
    ///
    /// 把 `.max(lo)` 去掉 → 本测试必 panic ⇒ FAILED。
    #[test]
    fn min_greater_than_max_must_not_panic() {
        // 启动路径
        let t = GlobalThrottle::new(true, true, 100, 500, 300, 2, 30, false);
        assert!(
            t.current_target_rpm() >= 500,
            "min=500 时 target 不得低于下限，实际 {}",
            t.current_target_rpm()
        );
        // 承重：存下来的上下限必须自洽，否则 maybe_step_up 读 rpm_max 当上限会与
        // rpm_min 互相矛盾（一个要求 ≥500、一个要求 ≤300）。
        assert!(
            t.rpm_max.load(Ordering::Relaxed) >= t.rpm_min.load(Ordering::Relaxed),
            "存储的 rpm_max({}) 不得小于 rpm_min({})",
            t.rpm_max.load(Ordering::Relaxed),
            t.rpm_min.load(Ordering::Relaxed)
        );
        // 热更路径（同一个进程内改配置）
        let t2 = mk(true, true, 100);
        t2.update(true, true, 100, 900, 400, 2, 30, false);
        assert!(
            t2.rpm_max.load(Ordering::Relaxed) >= t2.rpm_min.load(Ordering::Relaxed),
            "热更后存储的上下限同样必须自洽"
        );
    }

    #[tokio::test]
    async fn test_queue_timeout_passthrough_admits_not_reject() {
        // 单号/高RPM不流通根治:passthrough=true 时排队超时应**放行**(Ok)而非拒绝(Err)。
        // 极低 RPM(1)+ 极短 queue_max_wait(1s)+ 桶抽干 → 下一个 acquire 必排队超时。
        let t = GlobalThrottle::new(true, false, 1, 1, 300, 1, 1, true);
        while t.try_take() {} // 抽干初始桶
        // passthrough=true:超时放行,返回 Ok。
        assert!(
            t.acquire().await.is_ok(),
            "passthrough 开:排队超时应放行(Ok)不拒绝"
        );
    }

    #[tokio::test]
    async fn test_queue_timeout_reject_when_passthrough_off() {
        // passthrough=false 时保持旧行为:排队超时返回 Err(retry 秒数)。
        let t = GlobalThrottle::new(true, false, 1, 1, 300, 1, 1, false);
        while t.try_take() {} // 抽干初始桶
        assert!(
            t.acquire().await.is_err(),
            "passthrough 关:排队超时应返回 Err(拒绝)"
        );
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

    // 测试辅助:清掉 MD 去抖时刻,模拟"过了去抖窗口"(否则连续 429 只降一档)。
    fn clear_md_debounce(t: &GlobalThrottle) {
        t.bucket.lock().last_md_nanos = 0;
    }

    // 测试辅助:读当前 last_md_nanos(升档静默期判据源)。
    fn last_md(t: &GlobalThrottle) -> u64 {
        t.bucket.lock().last_md_nanos
    }
    // 测试辅助:把 last_md_nanos 强制设成"很久以前"(0),再看去抖 429 是否会把它推进到 now。
    fn set_last_md(t: &GlobalThrottle, v: u64) {
        t.bucket.lock().last_md_nanos = v;
    }

    #[test]
    fn test_debounced_429_does_not_refresh_last_md_no_upshift_starvation() {
        // ⭐死锁回归(旧代码必失败):RPM 已在下限、持续 429 都被去抖挡掉时,last_md_nanos 绝不能被
        // 去抖分支推进——否则升档静默期(maybe_step_up 要求距上次降档≥20s)永不满足 → RPM 卡死不回升。
        let t = mk(true, true, 200);
        // 先真降一档,记下 last_md(此后它应是"上次真降档"的稳定锚点)。
        t.report_upstream_429(); // 200→100,记 last_md=t0
        let t0 = last_md(&t);
        assert!(t0 > 0, "首次真降档应记录 last_md");
        assert_eq!(t.current_target_rpm(), 100);
        // 把 RPM 直接压到下限,并把 last_md 设回"很久以前"(0=从未降,模拟静默期已够长)。
        set_last_md(&t, 0);
        t.report_upstream_429(); // last_md=0 → 不在去抖窗 → 真降档 100→50,记新 last_md=t1
        let t1 = last_md(&t);
        assert_eq!(t.current_target_rpm(), 50);
        assert!(t1 > 0);
        // 紧接着一串"去抖窗内"的 429(模拟持续零星限流):它们都应被去抖挡掉且**不推进 last_md**。
        for _ in 0..5 {
            t.report_upstream_429();
        }
        assert_eq!(t.current_target_rpm(), 50, "去抖窗内的连续429只降过一档");
        assert_eq!(
            last_md(&t),
            t1,
            "⭐关键:去抖挡掉的429绝不能刷新last_md(旧代码在此刷新→升档静默期永不满足→RPM卡死)"
        );
    }

    #[test]
    fn test_at_floor_429_does_not_refresh_last_md_no_upshift_starvation() {
        // ⭐死锁回归·第二条触发路径(旧代码必失败):已在 rpm_min 下限时,429 穿过去抖窗后
        // next == cur(降不动了),此时**绝不能**刷新 last_md_nanos——否则只要上游持续零星 429
        // (间隔 >3s 穿过去抖、又 <20s 不到升档静默期),last_md 就被反复推进,
        // maybe_step_up 的 since_md>=20s 永不满足 → RPM 永久卡在 floor 再也回不去。
        let t = mk(true, true, 200);
        // 直接把 target 压到下限(mk 的 rpm_min=20)。
        t.bucket.lock().target_rpm = 20;
        // last_md=0 表示"从未降档/静默期已足够长",保证下面这次 429 能穿过去抖窗。
        set_last_md(&t, 0);

        t.report_upstream_429();

        assert_eq!(t.current_target_rpm(), 20, "已在下限,不应再降");
        assert_eq!(
            last_md(&t),
            0,
            "⭐关键:已在下限、本次并未真降档时绝不能刷新 last_md\
             (旧代码在此无条件刷新 → 升档静默期 since_md>=20s 永不满足 → RPM 永久卡在 floor)"
        );

        // 对照组:真降档时**必须**刷新 last_md(证明上面的"不刷新"是精确针对"没降动"这一情形,
        // 而不是把 last_md 的维护整个删掉了)。
        let t2 = mk(true, true, 200);
        set_last_md(&t2, 0);
        t2.report_upstream_429();
        assert_eq!(t2.current_target_rpm(), 100, "未到下限时应正常降档");
        assert!(
            last_md(&t2) > 0,
            "真降档必须刷新 last_md,否则去抖窗与升档静默期都失去锚点"
        );
    }

    #[test]
    fn test_bucket_capacity_never_below_one_token() {
        // ⭐容量塌陷回归(旧代码必失败):容量 = (rpm*1000/60).max(1)*burst,而取一个令牌需 1000 milli,
        // 隐含要求 rpm*burst >= 60。默认 burst_secs=2 时 rpm<=29 容量就 <1000 → 桶永远攒不满
        // 一个令牌 → try_take 恒 false → 所有请求排满 queue_max_wait_secs。
        // 而 AIMD 从默认 100 连降两档即到 25(100→50→25),rpm_min 默认 20 → 默认配置下
        // 被上游 429 打两次就整体塌陷。
        for rpm in [1u32, 5, 20, 25, 29, 30, 60, 100] {
            for burst in [1u32, 2, 3] {
                let t = GlobalThrottle::new(true, false, rpm, 1, 300, burst, 30, false);
                let cap = {
                    let b = t.bucket.lock();
                    t.capacity_milli_locked(b.target_rpm)
                };
                assert!(
                    cap >= ONE_TOKEN_MILLI,
                    "rpm={rpm} burst={burst}: 桶容量 {cap} < 一个令牌({ONE_TOKEN_MILLI}) → 永远取不到令牌(塌陷)"
                );
                // 初始桶也必须至少装得下一个令牌,否则首个请求就得白排队。
                assert!(
                    t.bucket.lock().tokens_milli >= ONE_TOKEN_MILLI,
                    "rpm={rpm} burst={burst}: 初始令牌数不足一个令牌"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_low_rpm_still_admits_immediately_at_floor() {
        // 端到端佐证容量塌陷已修:默认 burst_secs=2 + target_rpm=20(=AIMD floor) 时,
        // 首个请求必须**立即**放行,而不是排队 30s 后靠 passthrough 超时兜底。
        // passthrough=false 保证"若塌陷则返回 Err",使断言有判别力。
        let t = GlobalThrottle::new(true, false, 20, 20, 300, 2, 30, false);
        let r = tokio::time::timeout(Duration::from_millis(500), t.acquire()).await;
        assert!(
            matches!(r, Ok(Ok(()))),
            "低 RPM(20)+默认 burst(2) 首个请求应立即放行,实际={r:?}(旧代码容量 666<1000 → 排队直至超时)"
        );
    }

    #[test]
    fn test_aimd_md_debounce_single_drop_per_burst() {
        // Finding 2 修复:一波 failover 链的多次 429(去抖窗内)只降一档,不连乘到 floor。
        let t = mk(true, true, 200);
        t.report_upstream_429(); // 200→100(首次)
        t.report_upstream_429(); // 去抖窗内,不再降
        t.report_upstream_429(); // 去抖窗内,不再降
        assert_eq!(t.current_target_rpm(), 100, "一波连续429只降一档(去抖)");
    }

    #[test]
    fn test_aimd_md_halves_across_windows() {
        // 跨去抖窗的 429 才继续降档(模拟窗口过去)。
        let t = mk(true, true, 200);
        t.report_upstream_429();
        assert_eq!(t.current_target_rpm(), 100);
        clear_md_debounce(&t);
        t.report_upstream_429();
        assert_eq!(t.current_target_rpm(), 50);
        clear_md_debounce(&t);
        t.report_upstream_429();
        assert_eq!(t.current_target_rpm(), 25);
        clear_md_debounce(&t);
        t.report_upstream_429();
        assert_eq!(t.current_target_rpm(), 20, "不低于下限 20");
        clear_md_debounce(&t);
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

    /// 🔴 启动 slow-start：进程刚起来时有效 RPM 必须被折算，窗口后必须完全恢复。
    ///
    /// 依据：上游惩罚速率跃升（≥5x = 48.3% 429 / 平稳 = 0.7%），而 `RpmTracker`
    /// 是纯内存的 ⇒ 每次重启爬坡历史清零 ⇒ 选号层的爬坡限制在重启后第一分钟
    /// 完全不设防。线上 20:00 起 23 次重启，每次都造一次满量 burst。
    ///
    /// 回退即 FAIL：把 `try_take` 里的 `boot_ramp_rpm(b.target_rpm)` 改回
    /// `b.target_rpm` → 本条第一个断言失败。
    #[test]
    fn test_boot_slow_start_scales_then_fully_recovers() {
        let t = mk(true, false, 400);
        let at_boot = t.boot_ramp_rpm(400);
        assert!(
            at_boot < 400,
            "启动瞬间有效 RPM 必须被折算（否则重启后第一分钟是满量 burst）；实得 {at_boot}"
        );
        assert_eq!(
            at_boot,
            400 * BOOT_START_PCT / 100,
            "启动瞬间应恰为 BOOT_START_PCT%"
        );
        // 折算后绝不为 0：补充速率为 0 会让排队者只能靠 passthrough 超时放行，
        // 等于白等满 queue_max_wait。
        assert!(
            t.boot_ramp_rpm(1) >= 1,
            "折算下限必须 >=1，否则令牌桶永不补充"
        );
        assert!(t.boot_ramp_rpm(3) >= 1);
    }

    /// 与上一条配对：窗口**结束后**必须逐字节等于入参（零残留影响）。
    ///
    /// 只有上一条时，把折算写成永久生效（忘记 elapsed 判断）也能通过 ——
    /// 那会让整形永久跑在 25%，把吞吐掐掉四分之三。
    #[test]
    fn test_boot_ramp_is_identity_after_window() {
        let t = mk(true, false, 400);
        let mut t2 = mk(true, false, 400);
        // start 是同模块私有字段，测试可直接改，用它模拟"窗口已过"。
        t2.start = Instant::now() - Duration::from_secs(BOOT_SLOW_START_SECS + 1);
        for rpm in [1u32, 20, 60, 300, 400, 1200] {
            assert_eq!(
                t2.boot_ramp_rpm(rpm),
                rpm,
                "窗口结束后必须恒等于入参（否则整形永久跑在 {BOOT_START_PCT}%，吞吐被掐掉大半）"
            );
        }
        t2.start = Instant::now() - Duration::from_secs(BOOT_SLOW_START_SECS);
        assert_eq!(t2.boot_ramp_rpm(400), 400, "elapsed == 窗口长度应已恢复");
        assert!(t.boot_ramp_rpm(400) < 400, "对照：未过窗口的仍在折算");
    }

    /// 爬坡必须**单调不减**：窗口内回落会自己造出一次「低 → 高」跃升，
    /// 那正是本机制要消除的形态（自造 burst）。
    #[test]
    fn test_boot_ramp_is_monotonic_across_window() {
        let mut t = mk(true, false, 400);
        let mut last = 0u32;
        for sec in 0..=BOOT_SLOW_START_SECS {
            t.start = Instant::now() - Duration::from_secs(sec);
            let v = t.boot_ramp_rpm(400);
            assert!(
                v >= last,
                "第 {sec}s 的有效 RPM {v} 小于前一秒的 {last} —— 窗口内回落会自造一次跃升"
            );
            assert!(v <= 400, "折算值不得超过 target_rpm");
            last = v;
        }
        assert_eq!(last, 400, "窗口末尾必须已经升到满量");
    }

    /// `try_take` 必须**真的**用折算后的 RPM 补充令牌（而非只有 `boot_ramp_rpm` 存在）。
    ///
    /// # 为什么需要单独一条
    ///
    /// 前面几条都直接调 `boot_ramp_rpm`，所以把 `try_take` 里那行改回
    /// `b.target_rpm` 它们**照样绿** —— 函数存在但没接线，等于没实现。
    /// （我第一版就是这样，回退验证时发现的。）
    ///
    /// 判据：把 `start` 推到窗口正中（≈50% 折算），耗干桶后让它补充一整个窗口的量，
    /// 观察补到的令牌数落在「折算后容量」而不是「满量容量」。
    #[test]
    fn test_try_take_actually_applies_boot_ramp() {
        // ⚠️ 必须用**实际生效**的 target 算期望值：`mk` 传的 rpm 会被 clamp 到
        // rpm_max(300)，我第一版拿传入的 1200 去算 full_cap，于是两种实现都远小于它、
        // 断言恒真（回退验证时才发现）。这里直接用 GlobalThrottle::new 显式放开上限。
        let mut t = GlobalThrottle::new(true, false, 1200, 20, 1200, 2, 30, false);
        let target = t.current_target_rpm();
        assert_eq!(target, 1200, "前提：target 未被 clamp，否则期望值算错");
        // 窗口正中：pct = 25 + 75*30/60 ≈ 62%。
        t.start = Instant::now() - Duration::from_secs(BOOT_SLOW_START_SECS / 2);
        // 先耗干初始桶。
        while t.try_take() {}
        // 让时间过去足够久以补满桶：直接把 last_refill 推回去（同模块可访问）。
        {
            let mut b = t.bucket.lock();
            b.last_refill_nanos = 0;
        }
        // 此刻 now_nanos ≈ 30s，补充按折算后速率算，且被折算后容量夹住。
        let mut got = 0;
        while t.try_take() {
            got += 1;
            if got > 200 {
                break;
            }
        }
        // 满量容量 = target/60*burst = 1200/60*2 = 40 令牌；折算 62% ≈ 24。
        let full_cap = target / 60 * 2;
        assert!(
            got < full_cap,
            "try_take 必须按折算后 RPM 算容量：实得 {got} 已达满量容量 {full_cap}，\
             说明 boot_ramp_rpm 没有被接进 try_take（函数存在但没生效）"
        );
        assert!(got >= 1, "折算后仍应能放行若干令牌，实得 {got}");
    }

    /// 初始桶也必须按 `BOOT_START_PCT` 折算 —— 否则启动瞬间的突发绕过 slow-start。
    ///
    /// 回退即 FAIL：把构造函数里的初始 `tokens_milli` 改回未折算版本 → 本条失败。
    #[test]
    fn test_initial_bucket_respects_boot_start_pct() {
        // 400 RPM / burst 2s：未折算桶 ≈ 400/60*2 = 13 令牌；折算 25% ≈ 3 令牌。
        let t = mk(true, false, 400);
        let mut immediate = 0;
        while t.try_take() {
            immediate += 1;
            if immediate > 50 {
                break; // 防御：不该到这里
            }
        }
        assert!(
            immediate <= 5,
            "启动瞬间可取令牌数应≈折算后桶容量(25% ⇒ ~3)，实得 {immediate}\
             （未折算会是 ~13，等于 slow-start 被初始桶绕过）"
        );
        assert!(immediate >= 1, "至少要能立即放行一个，否则首个请求白排队");
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
        assert!(
            ok <= 4,
            "瞬时并发只应放行≈桶容量个令牌,实得 {ok}(超发=丢更新bug复现)"
        );
        assert!(ok >= 1, "至少应放行初始桶里的令牌");
    }

    #[test]
    fn test_update_hot_reload() {
        let t = mk(true, true, 100);
        t.update(true, false, 150, 10, 500, 3, 45, false);
        assert_eq!(t.current_target_rpm(), 150);
        // 手动挡了,429 不降。
        t.report_upstream_429();
        assert_eq!(t.current_target_rpm(), 150);
    }

    #[test]
    fn test_auto_mode_preserves_learned_rpm_on_reload() {
        // review 自查修复:自动挡下 AIMD 学到的档位不应被无关配置保存打回初值。
        let t = mk(true, true, 200);
        // 模拟被 429 降档到 50(跨去抖窗)。
        t.report_upstream_429(); // 200→100
        clear_md_debounce(&t);
        t.report_upstream_429(); // 100→50
        assert_eq!(t.current_target_rpm(), 50);
        // 无关配置保存(target 传的还是初值 200,但自动挡应保留学到的 50,只 re-clamp)。
        t.update(true, true, 200, 20, 300, 2, 30, false);
        assert_eq!(t.current_target_rpm(), 50, "自动挡保存无关配置不应打回初值");
        // 若新下限抬高到 80,则学到的 50 被 clamp 到 80。
        t.update(true, true, 200, 80, 300, 2, 30, false);
        assert_eq!(
            t.current_target_rpm(),
            80,
            "学到值低于新下限时 clamp 到下限"
        );
    }
}
