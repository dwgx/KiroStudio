//! 内置上游 429 吸收层策略快照。由 `provider.rs` 以 `#[path]` 子模块接入。
//!
//! 吸收循环仍留在 `provider.rs`；本文件只负责策略快照、类别闸门与退避。

use std::time::Duration;

use super::MAX_REQUEST_RETRY_BUDGET_SECS;

/// 吸收层一轮最坏「能跑出结果」的时长下限。
///
/// 取值等于 token_manager 的 `MAX_TRANSIENT_WAIT_SECS`（20s）而非另造一个数字：全池只是临时
/// 冷却时 `acquire_context` 最多在网关内等 20s 才 bail，因此**剩余预算不足 20s 的一轮，结构上
/// 只可能在 transient wait 里烧完再返回同一个 429** —— 白打一轮上游、客户端白等。
///
/// 这是设计评审 BLOCKER 9 的修法：判据必须是「剩余 ≥ 退避 + 一轮最坏耗时」，而不是
/// 「剩余 ≥ 退避下限」。后者会让第 2 轮必然在半路被 deadline 砍断。
const ABSORB_MIN_USEFUL_ROUND_SECS: u64 = 20;

/// 退避的**绝对下限**。`maxDelaySecs=0` 经 API 可写（配置层对它无 clamp），而 0 退避会
/// 让吸收循环退化成无 sleep 的 `continue` —— 忙等死循环，打满一核且请求永不返回。
/// 50ms 取自号池冷却的实测最快恢复量级（远小于外置 shield 硬编码的 1s）。
pub(super) const ABSORB_MIN_BACKOFF: Duration = Duration::from_millis(50);

/// 内置「上游 429 吸收层」的运行时策略快照（每次调用从 config 的 ArcSwap 取一份）。
///
/// 不做 TIER3 进程级 static：吸收层在 provider 内，`token_manager.config()` 本身就是 ArcSwap，
/// admin 存盘后 `reload_config` 原子换入即生效 —— 少一层镜像就少 6 个可写错点。
///
/// # 快照粒度：一次函数调用一份，不是一条客户端请求一份（2026-08-14 标注）
///
/// 各调用点保证在**函数内只取一份**并贯穿全函数：主路径在吸收循环外取、
/// 透传在 failover 循环外取。但**请求级一致**（同一条客户端请求的所有调用点共用
/// 同一份策略）需要上层下传：透传失败后落主路径、以及 WebSearch 每轮重进
/// `call_api_stream`，都会在各自函数入口各取一份新快照 —— 两处之间若恰好热更，
/// 同一条请求会混用两代策略。修法属 handler 层（请求入口构造一份下传，或挂到
/// 每请求共享的 `SharedRetryBudget` 上），不在本文件范围，此处仅记录边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AbsorbPolicy {
    pub(super) enabled: bool,
    pub(super) budget: Duration,
    pub(super) max_rounds: u32,
    pub(super) min_delay: Duration,
    pub(super) max_delay: Duration,
    /// 是否吸收 403 账户级临时风控（= 外挂所称的「换号空窗」）。**必须存进快照**而不是在
    /// 循环里重新 `config()`：一次调用内只取一份策略，否则 admin 在两轮之间热更会让同一条
    /// 请求前后按不同策略走（前半轮用旧 max_rounds、后半轮用新 suspended 判据），
    /// 行为不可复现也不可测试。
    pub(super) absorb_suspended: bool,
    /// 是否吸收上游 5xx。默认 false（见 `upstream_retry_absorb_server_error`）。
    pub(super) absorb_server_error: bool,
    /// 是否吸收带瞬态标记的 400（模型容量）。默认 false。
    pub(super) absorb_capacity_400: bool,
    /// 换号空窗的**独立预算**。`ZERO` = 未启用 ⇒ 该类沿用总预算与 min_delay 指数曲线
    /// （逐字节等于本字段引入前的行为）。非零时该类换成 20/40/60s 长阶梯 + 独立 deadline。
    pub(super) swap_budget: Duration,
    /// 预算耗尽时是否给错误串打 `absorb_budget_exhausted=1`（让 handlers 渲染成 503）。
    /// 默认 false（状态码保持透传 429）。
    pub(super) exhausted_as_503: bool,
}

/// 换号空窗的退避阶梯（秒），逐字取自外挂 `kiro_shield.py` 的 `SWAP_BACKOFF`。
///
/// 为什么是这三档而不是继续用指数：外挂注释里那句「**绝不能用限速那套 1 秒退避** ——
/// 那是拿一个已被封的账号去猛打上游，只会加重风控」是本阶梯存在的全部理由。空窗实测约
/// 10 分钟，20s 起步、封顶 60s 意味着一条请求最多问上游十几次，而 1s 起的指数在同样时长内
/// 会问上百次。超出表长的轮次取最后一档（60s）。
const SWAP_WINDOW_BACKOFF_SECS: [u64; 3] = [20, 40, 60];

impl AbsorbPolicy {
    pub(super) fn from_config(cfg: &crate::model::config::Config) -> Self {
        // 403 临时风控的额外轮次**硬钉为 1**（不是沿用 max_rounds）：`config.self_heal_base_backoff_secs（默认 60s）`
        // 存在的意义就是停止向刚 403 的账号试探，而 403 是**账号级**的 —— 换号只是把同一个
        // 被惩罚账号走多遍，扩大受害面而非提高成功率。UI 文案也标注了「与自愈退避冲突」。
        let swap_budget = Duration::from_secs(cfg.upstream_retry_absorb_swap_budget_secs);
        // ⭐ 「钉 1」的**前提**是短退避：15s 内重打同一个刚被风控的账号会抵消
        // `config.self_heal_base_backoff_secs（默认 60s）=60s`（那条退避存在的意义就是停止试探）。
        // 设了 swap 预算后该类走 20/40/60s 长阶梯 —— 最短一档就是 20s，前提不再成立，
        // 于是解除钉 1，交回 `max_rounds` + 独立 deadline + `ABSOLUTE_MAX_TOTAL_RETRIES` 三道闸。
        //
        // 不解除的话这个旋钮基本没用：钉 1 时它只能把**一次**重试推迟到 20s 后，
        // 而空窗实测 10 分钟 ⇒ 那一次几乎必然还在窗口内 ⇒ 白等 20s 拿同一个 403。
        let max_rounds = if cfg.upstream_retry_absorb_suspended && swap_budget.is_zero() {
            cfg.upstream_retry_absorb_max_rounds.min(1)
        } else {
            cfg.upstream_retry_absorb_max_rounds
        };
        // ⭐ 两道归一化，顺序不能换。两者都是**下限**方向，因为 `backoff()` 末尾是
        // `d.clamp(min_delay, max_delay)` —— 决定「会不会返 0」的是 `min_delay`（下界），
        // 不是 `max_delay`。
        //
        // ① `min_delay` 抬到 `ABSORB_MIN_BACKOFF`。`minDelayMs=0` 经 Admin API 可写
        //    （`service.rs` 对这两个字段无任何 clamp），而下界为 0 时 `clamp` 不会抬起
        //    任何东西 → `PoolCooldown(0)` 直接返 `Duration::ZERO` → 吸收循环变成无 sleep
        //    的 `continue` = 忙等死循环（`acquire_context` 那次 CPU 打满一核、请求永不
        //    返回正是这个形态）。⚠️ 这条缺陷**在本批之前就存在**（deploy tip 的
        //    `from_config` 同样不给 `min_delay` 下限），不是新引入的。
        //
        // ② `max_delay` 再抬到不低于 `min_delay`。`d.clamp(min, max)` 在 `min > max` 时是
        //    **panic**（std 契约），而两个字段来自面板上两个独立数字框（毫秒 vs 秒），
        //    `minDelayMs=60000` 配 `maxDelaySecs=1` 一次手滑即可配出 —— 那会让此后每个
        //    429 都在请求热路径上 panic。
        //
        //    方向取「抬 max 到 min」而不是「压 min 到 max」：矛盾配置下宁可**退避更久**。
        //    退避久的后果是 `should_start_another_round`（要求剩余 > 退避 + 20s）判不通过
        //    ⇒ 吸收层不干活、回落旧行为；而退避短的后果是对一个还在冷却的号池连打，
        //    正是吸收层要避免的事。前者安全，后者有害。
        //
        // 都用归一化而非拒绝保存：退避窗退化成一个点仍是可用行为，而拒绝保存会把一个
        // 能自愈的配置错误变成运维事故。不变式由构造保证 ⇒ 可用纯函数单测钉死。
        let min_delay =
            Duration::from_millis(cfg.upstream_retry_absorb_min_delay_ms).max(ABSORB_MIN_BACKOFF);
        let max_delay =
            Duration::from_secs(cfg.upstream_retry_absorb_max_delay_secs).max(min_delay);
        Self {
            enabled: cfg.upstream_retry_absorb_enabled,
            // ⭐ 总预算**下限是 45s**（`MAX_REQUEST_RETRY_BUDGET_SECS`），不是面板允许的 1s。
            //
            // 因为 `round_budget()` 用 `min(45s, 剩余预算)` 夹每一轮的 failover 墙钟，
            // 所以这个「吸收层的」旋钮会反向支配**既有的** failover 重试预算：
            // 填 5s ⇒ 第 0 轮（也就是关掉吸收层时唯一的那一轮）的换号墙钟从 45s 变成 5s
            // ⇒ 正常换号重试被截断。运维填小值的动机恰恰是「想压住客户端延迟」，
            // 而实际后果是把与吸收层无关的重试也砍了 —— 面板上完全看不出这层耦合。
            //
            // 抬到 45s 后语义变干净：这个旋钮只能决定**多给几轮**，
            // 永远不会让单轮比关掉吸收层时更短。
            budget: Duration::from_secs(cfg.upstream_retry_absorb_budget_secs)
                .max(Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS)),
            max_rounds,
            min_delay,
            max_delay,
            absorb_suspended: cfg.upstream_retry_absorb_suspended,
            absorb_server_error: cfg.upstream_retry_absorb_server_error,
            absorb_capacity_400: cfg.upstream_retry_absorb_capacity_400,
            swap_budget,
            // 只认精确的 503。其它值（含裸 `#[serde(default)]` 会给的 0）一律按 429 处理：
            // 这个开关的语义是「要不要为 Cursor 让步」，不是「随便填个状态码」——
            // 让 provider 打一个 handlers 认不出的标记只会造成静默的行为分叉。
            exhausted_as_503: cfg.upstream_retry_absorb_exhausted_status == 503,
        }
    }

    /// 该类别当前是否被允许吸收（各类别的独立开关）。
    ///
    /// 抽成纯函数而不是在循环里散写 `if`：三个新类别各有一个开关，散写必然漏一处，
    /// 而漏掉的那一处的表现是「默认关的类别其实在吸收」—— 正是硬约束里最不能出的错。
    /// 纯函数可用单测把「默认配置下三个新类别一律不吸收」钉死。
    pub(super) fn class_allowed(&self, class: crate::anthropic::AbsorbClass) -> bool {
        use crate::anthropic::AbsorbClass;
        match class {
            // 这两类是吸收层原本就在做的事，跟着总开关走。
            AbsorbClass::PoolCooldown(_) | AbsorbClass::UpstreamRateLimit => true,
            AbsorbClass::SwapWindow => self.absorb_suspended,
            AbsorbClass::TransientServerError => self.absorb_server_error,
            AbsorbClass::TransientCapacity400 => self.absorb_capacity_400,
        }
    }

    /// 该类别的绝对 deadline。换号空窗在设了独立预算时用它自己那份，其余一律用总预算。
    ///
    /// 为什么不能共用一个预算（外挂实测）：换号空窗约 **10 分钟**，而总预算线上是 20s。
    /// 抬总预算会让**所有**类别都能占着客户端连接十分钟，而换号空窗恰恰是唯一等得起的一类
    /// （客户端在补号完成后自动恢复，而不是当场断会话）。
    pub(super) fn class_deadline(
        &self,
        call_started: std::time::Instant,
        class: crate::anthropic::AbsorbClass,
    ) -> std::time::Instant {
        if matches!(class, crate::anthropic::AbsorbClass::SwapWindow) && !self.swap_budget.is_zero()
        {
            // 与总预算同源地从 `call_started` 起算（含准入排队），理由见调用点：
            // 改成「从此刻起算」会让客户端可见延迟变成排队 + 吸收之和。
            call_started + self.swap_budget
        } else {
            call_started + self.budget
        }
    }

    /// 该类别愿意睡的上限。换号空窗启用长阶梯时可以超过全局 `max_delay`。
    ///
    /// 为什么必须放宽：`max_delay` 默认 15s < 阶梯最短一档 20s ⇒ 不放宽的话长阶梯会被
    /// 全局 clamp 削回 15s，这个旋钮等于没接上（而 `backoff_is_truncated` 只对
    /// `PoolCooldown` 成立，不会拦住这种「睡不够」）。显式设了 swap 预算本身就是
    /// 「这一类可以睡更久」的表态。
    pub(super) fn class_max_delay(&self, class: crate::anthropic::AbsorbClass) -> Duration {
        if matches!(class, crate::anthropic::AbsorbClass::SwapWindow) && !self.swap_budget.is_zero()
        {
            let ladder_top =
                Duration::from_secs(SWAP_WINDOW_BACKOFF_SECS[SWAP_WINDOW_BACKOFF_SECS.len() - 1]);
            self.max_delay.max(ladder_top)
        } else {
            self.max_delay
        }
    }

    /// 本次调用允许的额外吸收轮次。关闭时恒为 0 —— 把「关 ⇒ 零额外轮次」做成可断言的纯函数，
    /// 而不是散落在调用点的 `if !enabled` 判断（后者无法用单测钉死）。
    pub(super) fn effective_max_rounds(&self) -> u32 {
        if self.enabled { self.max_rounds } else { 0 }
    }

    /// 本轮 failover 循环的墙钟预算：`min(45s, 剩余吸收预算)`。
    ///
    /// 这就是「吸收轮次不会超总预算」的**机制**（而非靠调用点自觉）：一轮的墙钟上限本身
    /// 被剩余预算夹住，所以吸收轮次与 failover 轮次不是各算一套预算，而是后者被前者显式配额。
    /// 关闭时恒返完整 45s ⇒ 与旧行为逐字节等价。
    pub(super) fn round_budget(
        &self,
        deadline: std::time::Instant,
        round_started: std::time::Instant,
    ) -> Duration {
        let full = Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS);
        if self.enabled {
            full.min(deadline.saturating_duration_since(round_started))
        } else {
            full
        }
    }

    /// 本轮**真正需要**等多久才有意义 —— 未经 `max_delay` 截断的原始值。
    ///
    /// 与 [`Self::backoff`] 分开是承重的（未修问题 ③）：`PoolCooldown(secs)` 是 cooldown.rs
    /// 的**进程内真值**，而 `max_delay`（默认 15s）是一个与它毫无关系的旋钮。号池给出
    /// 60s（`config.self_heal_base_backoff_secs（默认 60s） = from_secs(60)`，token_manager.rs:890 一带）时，
    /// 只 clamp 不判断的写法会
    /// 睡满 15s 就醒来再打一轮 —— 池子还在冷却 45s，这一轮**结构上必然**拿回同一个 429，
    /// 等于白打一轮上游 + 客户端白等 15s。判「够不够」必须用真值，睡多久才用截断值。
    pub(super) fn required_wait(&self, class: crate::anthropic::AbsorbClass, round: u32) -> Duration {
        use crate::anthropic::AbsorbClass;
        match class {
            // 号池真值。secs=0 由 backoff() 的 clamp 抬到 min_delay:无 sleep 的 continue 就是
            // 忙等死循环(acquire_context 那次 CPU 打满一核、请求永不返回正是这个形态)。
            AbsorbClass::PoolCooldown(secs) => Duration::from_secs(secs),
            // 无真值可用:指数(非 shield 的 1.7 倍)。已有 max_rounds 与绝对 deadline 双闸,
            // 收敛更快比更平滑重要。
            AbsorbClass::UpstreamRateLimit => self.min_delay.saturating_mul(1u32 << round.min(6)),
            // 换号空窗:设了独立预算 ⇒ 长阶梯 20/40/60s;未设 ⇒ 与限速同曲线(旧行为)。
            //
            // 长阶梯的理由是外挂那句实测结论:「**绝不能用限速那套 1 秒退避** —— 那是拿一个
            // 已被封的账号去猛打上游,只会加重风控」。空窗约 10 分钟,20s 起步意味着一条请求
            // 最多问上游十几次;1s 起的指数在同样时长内会问上百次,那不是重试而是施压。
            AbsorbClass::SwapWindow => {
                if self.swap_budget.is_zero() {
                    self.min_delay.saturating_mul(1u32 << round.min(6))
                } else {
                    let idx = (round as usize).min(SWAP_WINDOW_BACKOFF_SECS.len() - 1);
                    Duration::from_secs(SWAP_WINDOW_BACKOFF_SECS[idx])
                }
            }
            // 5xx:1s 起(逐字取自 shield 的 `MIN_DELAY=1.0`)×2 指数。
            // 为什么比限速类起步更长而封顶更早:5xx 多为上游/网关瞬时抖动,一两秒后重打大概率
            // 就过;但若是整片故障,短退避会在故障期乘倍放大请求量。1s 是「抖动等得起、
            // 故障不放大」的折中,且与本仓既有的 `retry_delay_model_unavailable` 同基数。
            AbsorbClass::TransientServerError => {
                const BASE: Duration = Duration::from_secs(1);
                BASE.saturating_mul(1u32 << round.min(4))
            }
            // 容量类:2s 起 ×2。比 5xx 更长是因为「模型没容量」是**全局**状态
            // (所有凭据对同一模型等价受影响,见 endpoint 侧那条判据的说明),换号不解决问题,
            // 只能等上游腾出容量。与 provider 内部那几次慢速重试(1s base)串联后总时长才够到
            // 容量恢复的量级 —— 内部那几次加起来只有秒级,而容量恢复常在分钟级。
            AbsorbClass::TransientCapacity400 => {
                const BASE: Duration = Duration::from_secs(2);
                BASE.saturating_mul(1u32 << round.min(4))
            }
        }
    }

    /// 本轮实际睡多久：真实需求经 `[min_delay, max_delay]` 夹取。
    ///
    /// 与外置 shield 的逐条差异：① shield 的 `MIN_DELAY` 硬 1s，号池 50ms 就能恢复时白睡
    /// 950ms×每轮（这是它 p50 73.2s 的病根之一），这里下限可配到亚秒级；② shield 只看 HTTP
    /// `Retry-After` 头，这里直接吃 `PoolCooldown(secs)` 的**进程内真值**（就是 cooldown.rs
    /// 算出的剩余秒数，无需 HTTP 头往返）。
    pub(super) fn backoff(&self, class: crate::anthropic::AbsorbClass, round: u32) -> Duration {
        // 上界按类别取（见 `class_max_delay`）：换号空窗的长阶梯不能被默认 15s 的全局上限削回，
        // 否则那个旋钮等于没接上。其余类别的上界与本改动之前逐字节相同。
        //
        // ⚠️ `clamp` 在 `min > max` 时 panic，而 `class_max_delay` 只会**放大**上界
        // （`self.max_delay.max(ladder_top)`），`from_config` 已保证 `max_delay >= min_delay`
        // ⇒ 不变式仍然成立。
        self.required_wait(class, round)
            .clamp(self.min_delay, self.class_max_delay(class))
    }

    /// 号池给出的**真实恢复时刻**是否超过我们愿意睡的上限 ⇒ 睡醒了池子还没好 ⇒ 这一轮白打。
    ///
    /// **只对 `PoolCooldown` 成立**，这个限定是承重的：只有它携带真值（cooldown.rs 算出的
    /// 剩余秒数）。`UpstreamRateLimit`/`Suspended` 走的是我们自己编的指数兜底 —— 它撞上
    /// `max_delay` 只说明「我们不想睡更久」，**不代表上游没好**（`max_delay` 本来就是为了
    /// 夹住它而存在的）。若把它们也算进来，指数涨过上限后吸收层会对**最主要**的那类
    /// （上游裸 429）提前停止工作，白丢一层保护。
    ///
    /// 这条比 `should_start_another_round` 更早生效：后者只管**预算**够不够睡，管不了
    /// 「睡够了但上游没好」——两者是独立的失败模式，都必须拦。
    pub(super) fn backoff_is_truncated(&self, class: crate::anthropic::AbsorbClass, round: u32) -> bool {
        matches!(class, crate::anthropic::AbsorbClass::PoolCooldown(_))
            && self.required_wait(class, round) > self.max_delay
    }
}

/// 是否还够再跑一轮吸收：**剩余预算 > 退避 + 一轮最坏耗时**。
///
/// 判据刻意不是「剩余 ≥ 退避下限」（BLOCKER 9）：那样第 2 轮会在半路被 deadline 砍断，
/// 等于白打一轮上游还让客户端多等。纯函数便于单测钉死，无需真实上游。
pub(super) fn should_start_another_round(
    deadline: std::time::Instant,
    now: std::time::Instant,
    delay: Duration,
) -> bool {
    deadline.saturating_duration_since(now)
        > delay + Duration::from_secs(ABSORB_MIN_USEFUL_ROUND_SECS)
}
