//! 每请求共享重试预算（纯类型 / 纯函数）。由 `provider.rs` 以 `#[path]` 子模块接入。
//!
//! 重试 / 吸收循环仍留在 `provider.rs`；本文件只负责总额度、配额计算与共享预算。

/// 每个凭据的最大重试次数
pub(super) const MAX_RETRIES_PER_CREDENTIAL: usize = 3;

/// 小号池阈值：号池 <= 此值时，每号重试次数降为 1（见 [`compute_max_retries`]）。
/// 小池下重试只会反复砸同几个号，被限流时多打几次纯属加重冷却，不如各摸一次即透传。
pub(super) const SMALL_POOL_THRESHOLD: usize = 3;

/// 总重试次数硬上限 —— 与 kiro.rs 对齐（4 次）。
///
/// 依据（最初定 12 时的推算；后 64→12→4 逐步收紧）：17 份分身共享 3 个上游账号，
/// 摸 12 个并发分身 = 对同一账号连打 12 次，正是风控要抓的突发特征。高峰期多账号
/// 同时触顶时，过多重试会在账号间连环撞墙、放大限流；被限时尽早返回而非耗尽配额。
/// 配合 429 专用长退避（见 `retry_delay_throttle`），尽快把错误交还给客户端。
///
/// ⭐ 这个上限是「**每客户端请求**」，开启吸收层后也不变 —— 由 [`round_retry_quota`] 保证。
///
/// 曾经不是：`compute_max_retries` 在 `'absorb: loop` **之外**只算一次，而
/// `'attempt: for attempt in 0..max_retries` 每个吸收轮都重跑一遍 ⇒ 每轮各拿一份完整 4 ⇒
/// `upstreamRetryAbsorbMaxRounds=3` 时一条客户端请求最坏 (1+3)×4 = **16 次**上游调用、
/// 同一出口 IP，正是把上限压到 4 想压住的突发特征被从另一头放回来。
///
/// 现在每轮的实际配额是 `min(基础配额, 本上限 − 跨轮已用)`，所以无论 `max_rounds`
/// 填多大，单条客户端请求打向上游的总次数恒 ≤ 本值。守卫见
/// `total_upstream_attempts_are_capped_per_request_not_per_round`。
pub(super) const ABSOLUTE_MAX_TOTAL_RETRIES: usize = 4;

/// 每客户端请求的上游调用**共享预算**（2026-08-11 方案 A，RPM 放大治本）。
///
/// # 为什么必须有
///
/// `ABSOLUTE_MAX_TOTAL_RETRIES` 的「每请求」语义此前只约束**单次** `call_api_with_retry`
/// 调用内：websearch 回灌**每一轮**（上限 5 轮）都重新走一遍完整 failover、压缩重试
/// 每一轮同理、MCP 调用（`call_mcp_with_retry`，WebSearch 的搜索）与透传 failover
/// 各自独立拿配额——一次客户端请求最坏可打 20+ 次上游。外部 30-50 RPM 因此放大成
/// 500-1000+ 上游 RPM（用户实测观测），端点选错时每轮都失败、每轮都换号重试，放大
/// 成倍。
///
/// # 语义
///
/// handler 层**每客户端请求创建一个**，沿整条调用链传递（主路径 failover / 压缩重试轮 /
/// websearch 回灌轮含 MCP / 透传 failover 全部从同一预算扣）。预算耗尽后各层
/// `round_retry_quota` 返回 0、failover 直接停止，错误上抛给客户端自己退避。
/// 无论嵌套多少层，每请求上游调用恒 ≤ `ABSOLUTE_MAX_TOTAL_RETRIES`。
///
/// `remaining` 用 Mutex：同一请求内各层可能在并发任务中执行（如 websearch 轮次），
/// 但任一时刻只有一层在扣（各层是串行 await 链），锁无竞争。
///
/// ⭐ **链内首选号**（N4 可观测）：`first_attempted_credential_id` 是整条客户端请求链
/// **最先尝试**的凭据 ID，供失败/成功记录的 usage 埋点暴露「死号恒选」。
/// 为什么挂在预算上：透传 failover（`try_custom_api_passthrough`）与 Kiro 主路径
/// （`call_api_with_retry`）是 handlers 层的**两次独立调用**，各自只拿得到自己那一段的
/// 选号信息 —— 而「透传先试了哪几个号」在透传全败返回 `None` 时随返回值一起丢失。
/// 预算沿整条链传递（handler 每请求创建一份），是最小的跨层携带通道：
/// 透传首跳先写（首写生效），Kiro 主路径首个选中的号兜底，`fail_record` 读同一份。
#[derive(Debug)]
pub struct SharedRetryBudget {
    remaining: std::sync::Mutex<u32>,
    /// 整条请求链最先尝试的凭据 ID（首写生效；None = 链内尚未选中任何凭据）。
    first_attempted_credential_id: std::sync::Mutex<Option<u64>>,
}

impl SharedRetryBudget {
    pub fn new() -> Self {
        Self {
            remaining: std::sync::Mutex::new(ABSOLUTE_MAX_TOTAL_RETRIES as u32),
            first_attempted_credential_id: std::sync::Mutex::new(None),
        }
    }

    /// 当前剩余额度（配额计算输入：每轮 `min(base, remaining)`）。
    pub fn remaining(&self) -> u32 {
        *self.remaining.lock().unwrap()
    }

    /// 已用量（`round_retry_quota(base, attempts_before)` 的实参语义是「已完成的尝试次数」，
    /// 不是剩余量——传剩余会把语义反转：耗尽时 remaining=0 被当成「还没用」而拿满配额）。
    pub fn used(&self) -> u32 {
        (ABSOLUTE_MAX_TOTAL_RETRIES as u32).saturating_sub(self.remaining())
    }

    /// 一次真实上游调用后扣减（无论成败——打了就是打了）。
    pub fn consume(&self, n: u32) {
        let mut r = self.remaining.lock().unwrap();
        *r = r.saturating_sub(n);
    }

    /// 记录链内最先尝试的凭据 ID（**首写生效**：已有值不再覆盖，保证
    /// 「透传首跳 → Kiro 首跳」的先后顺序语义 —— 先跑的那层拥有槽位）。
    pub fn note_first_attempt(&self, id: u64) {
        let mut f = self.first_attempted_credential_id.lock().unwrap();
        if f.is_none() {
            *f = Some(id);
        }
    }

    /// 链内最先尝试的凭据 ID（`None` = 链内尚未选中任何凭据）。
    pub fn first_attempted(&self) -> Option<u64> {
        *self.first_attempted_credential_id.lock().unwrap()
    }
}

impl Default for SharedRetryBudget {
    fn default() -> Self {
        Self::new()
    }
}

/// 本吸收轮还能打几次上游：**跨轮共享**同一个 `ABSOLUTE_MAX_TOTAL_RETRIES` 总额度。
///
/// 未修问题 ②：`compute_max_retries` 在 `'absorb: loop` **之外**只算一次，而
/// `for attempt in 0..max_retries` 每轮重跑 ⇒ 每轮各拿一份完整 4 ⇒ `max_rounds=3` 时
/// 一条客户端请求最坏 (1+3)×4 = **16 次**上游调用、同一出口 IP —— 正是当初把
/// `ABSOLUTE_MAX_TOTAL_RETRIES` 从 64 砍到 4 要压住的突发特征，被吸收层从另一头放回来。
///
/// 修法不是调小 `max_rounds`（那只是把数字挪一挪，语义仍是「每轮各拿一份」），而是让
/// 上限回到它文档承诺的「**每请求**」语义：本轮配额 = `min(基础配额, 总额度 − 已用)`。
/// 于是无论 `max_rounds` 填多少，一条客户端请求打向上游的次数恒 ≤ `ABSOLUTE_MAX_TOTAL_RETRIES`。
///
/// `attempts_before` 传**已完成的尝试次数**（= 循环外的 `attempts_base`）。返回 0 表示
/// 额度已用尽，调用点必须 `break 'absorb` 而不是空跑一轮（空跑会白睡一次退避）。
pub(super) fn round_retry_quota(base_quota: usize, attempts_before: u32) -> usize {
    let remaining = ABSOLUTE_MAX_TOTAL_RETRIES.saturating_sub(attempts_before as usize);
    base_quota.min(remaining)
}

/// 计算本次调用允许的总重试次数（动态预算）
///
/// - `total`：凭据总数
/// - `available`：当前未禁用（可用）凭据数
///
/// 预算 = `(total * per_cred).min(ABSOLUTE_MAX_TOTAL_RETRIES)`，再以 1 兜底。
///
/// ⚠️ **`available` 已不参与计算**（参数保留只为不动调用点与既有测试）。
/// 因此本函数**不再保证「每个可用凭据至少被尝试一次」** —— 号池大于
/// `ABSOLUTE_MAX_TOTAL_RETRIES` 时，单个请求扫不完全池。这是**刻意的权衡**，
/// 理由见函数体内 `.min()` 处的长注释（旧代码的内层 `.max(available)` 会让硬上限
/// 自我抵消，线上 43 号时预算 = 43，一条请求顺着整池撞一遍直到耗尽 45s 墙钟，
/// 净效果是「号池越大越慢」）。
///
/// 该权衡依赖一个前提：**坏号会被自动禁用从而不进候选集**，故预算 4 足够摸到
/// 足量健康号。号池规模显著超过 `ABSOLUTE_MAX_TOTAL_RETRIES` 时需重新评估这个前提。
///
/// **小号池降重试**：号池很小（`total <= SMALL_POOL_THRESHOLD`）时，每号重试次数降为 1。
/// 因为小池下重试循环只会反复选到同几个号——被限流时多打几次纯属反复砸、加重冷却，
/// 不如让每个号各摸一次就把上游错误透传给客户端（客户端自身有退避重试，比网关内反复砸温和）。
/// 号多时行为完全不变（仍 `MAX_RETRIES_PER_CREDENTIAL`）。
pub(super) fn compute_max_retries(total: usize, _available: usize) -> usize {
    // `_available` 保留在签名里但**不再参与计算**：见下方 `.min()` 处的说明。
    // 保留参数是为了不改动调用点与既有测试；将来若确认永不需要，再一并删除。
    let per_cred = if total <= SMALL_POOL_THRESHOLD {
        1
    } else {
        MAX_RETRIES_PER_CREDENTIAL
    };
    (total * per_cred)
        // ⚠️ 这里**刻意不再**用 `.max(available)` 抬高上限。
        //
        // 旧代码是 `.min(ABSOLUTE_MAX_TOTAL_RETRIES.max(available))`，那个内层
        // `.max(available)` 会在 `available > ABSOLUTE_MAX_TOTAL_RETRIES` 时把硬上限
        // 自己抵消掉 → 预算等于可用号数。线上 43 个号时实测预算 = 43，日志里就是
        // 「尝试 43/43」：一条
        // 客户端请求要顺着整池撞一遍，撞到 45s 墙钟预算才失败。
        //
        // 净效果是**号池越大越慢**，与"扩号池提升吞吐"的目标正好相反。而"保证每个
        // 可用号至少被摸一次"这个原始意图本身就站不住：池子有 200 个号时，为一条
        // 请求打 200 次上游只会加重风控，而不会提高这条请求的成功率——真正该做的是
        // 让坏号被自动禁用而**不进入**候选集（见 token_manager 的
        // `report_suspicious_activity`），而不是靠遍历去撞。
        .min(ABSOLUTE_MAX_TOTAL_RETRIES)
        // ⚠️ 地板 1：预算为 0 等于**一次都不尝试**，请求直接以「已达到最大重试次数（0次）」
        // 失败，而 acquire_context 的等待循环根本没机会跑。
        //
        // 旧实现喂 `total_count()`（含 disabled 条目，恒 ≥ 池内号数）所以永远算不出 0，
        // 掩盖了这里缺下限。改喂 `kiro_selectable_count()` 后，**瞬时**全池不可选
        //（全部在冷却中 / inflight 打满）会让它返回 0 → 预算 0 → 请求零重试即失败。
        // 这是真实回归：线上 20 分钟内出现 10 次该错误。
        //
        // 取 1 而非 0 的语义：至少走一遍 acquire_context，让它的等待逻辑有机会等到号
        // 出冷却；等不到再由墙钟预算（MAX_REQUEST_RETRY_BUDGET_SECS）兜底透传错误。
        .max(1)
}
