//! 冷却管理模块
#![allow(dead_code)]
//!
//! 分类管理不同原因的冷却状态，支持差异化冷却时长和自动清理。
//! 参考 CLIProxyAPIPlus 的实现。

use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 冷却原因
///
/// **当前触发覆盖** (Round 7 静态审查 2026-05-13):
///   生产代码中实际调用 `set_cooldown` 触发的变体:
///     - `RateLimitExceeded`  ← 上游 429 + 普通 throttle
///     - `TokenRefreshFailed` ← refresh wire 失败
///     - `ServerError`        ← 上游 5xx
///     - `ModelUnavailable`   ← `MODEL_TEMPORARILY_UNAVAILABLE` 全局熔断
///
///     - `SuspiciousActivity`  ← 403 `TEMPORARILY_SUSPENDED` 账户级软风控
///     - `AuthenticationFailed` ← `token_manager::report_auth_cooldown`
///                               （**只剩「从未成功过的号刷新还失败」这一类**：
///                                provider 的两处 force-refresh 失败点按
///                                `has_ever_succeeded` 二分，证明过的那侧已改走
///                                `AuthTransient`）
///     - `AuthTransient`       ← `token_manager::report_auth_transient_cooldown`
///                               （provider 401/403 的三处瞬态路径：两处
///                                force-refresh 失败但该号已成功过、一处
///                                `bearer_invalid_but_proven`；外加 403
///                                FEATURE_NOT_SUPPORTED 且后台重探已启动）
///     - `AccountSuspended`    ← `token_manager::report_account_suspended`
///
///   尚未被 set_cooldown 实际触发的变体（保留作为 future 分类点）:
///     - `QuotaExhausted`      — 月度配额耗尽（目前走 RateLimitExceeded）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CooldownReason {
    /// 429 速率限制
    RateLimitExceeded,

    /// 账户级"可疑活动"临时风控（`suspicious activity` + `temporary limits`）。
    /// 介于普通限速与永久封禁之间：Kiro 检测到异常频率、临时限制该账户并"调查中"。
    /// 实测是**软风控**——甩个 429 后号几秒就又能用,故首次只短冷却(20s)不白雪藏;
    /// 只有**持续反复**触发才陡增退避(1.6^n,上限 30min),避免反复砸把账户推向真封禁。
    SuspiciousActivity,

    /// 账户暂停
    AccountSuspended,

    /// 配额耗尽
    QuotaExhausted,

    /// Token 刷新失败
    TokenRefreshFailed,

    /// 认证失败
    AuthenticationFailed,

    /// **瞬态**认证失败：上游回 401/403 但该凭据的认证态并没有真的坏掉。
    ///
    /// # 为什么不能与 [`Self::AuthenticationFailed`] 共用
    ///
    /// 两者在 wire 上**逐字节相同**（同一句 bearer-invalid + 403），语义却相反：
    /// - `AuthenticationFailed` = refreshToken 真废了 ⇒ 硬等 24h 是对的，人工换号才恢复。
    /// - `AuthTransient` = token 对该端点**已被证明有效**（`has_ever_succeeded`），
    ///   或 region 未开通而后台重探已启动 ⇒ 几十秒后大概率就能用。
    ///
    /// 共用一个变体的代价是**不对称的**：把瞬态当永久 = 一次抖动冻掉一个健康号 24h
    /// （`is_auto_recoverable=false` ⇒ `calculate_cooldown_duration` 走
    /// `long_cooldown_secs`=86400s，且不禁用 ⇒ 面板只显示「冷却中」的僵尸，
    /// 比禁用更难发现）；把永久当瞬态 = 每 20s 多打一次注定失败的上游往返，可逆。
    /// 所以只能拆变体，不能改 `AuthenticationFailed` 的时长。
    ///
    /// 同款先例见 `token_manager.rs` 的 `report_refresh_failure_classified`：
    /// 它的文档已点名「绝不能用 `AuthenticationFailed`（会变成僵尸）」，走的正是
    /// [`Self::TokenRefreshFailed`] 这条可自愈路。本变体把该结论补给 401/403 路径。
    AuthTransient,

    /// 服务器错误
    ServerError,

    /// 模型暂时不可用
    ModelUnavailable,
}

impl CooldownReason {
    /// 获取默认冷却时长
    pub fn default_duration(&self) -> Duration {
        match self {
            // 短冷却：429 瞬时限流是上游 burst 软保护、通常几秒内自愈，基线取小
            // （15s），避免小号池下一个卡住的请求把整池长时间压死（见 calculate_cooldown_duration
            // 的温和递增 + 平静期衰减）。
            CooldownReason::RateLimitExceeded => Duration::from_secs(15),
            // 可疑活动风控：基线 20s（实测这是账户级"软"风控——甩个 429 后号几秒就又能用，
            // 一上来就冷却 3min 会把还能用的号白白雪藏、可用号变少）。基线短给号喘息，
            // 只有**反复**触发才由 calculate_cooldown_duration 陡增退避（见那里 1.6^n + 30min 上限）。
            CooldownReason::SuspiciousActivity => Duration::from_secs(20),
            CooldownReason::TokenRefreshFailed => Duration::from_secs(60),
            // 瞬态认证失败：基线取 20s，与 `SuspiciousActivity` 及 handlers 的
            // `UPSTREAM_SUSPENDED_RETRY_AFTER_SECS` 同档 —— 它们描述的是同一类
            // 「上游几十秒后自愈」的账户级抖动。取这个值而非更短，是因为该号刚被上游
            // 拒过一次，秒级回池等于立刻再撞；也不取分钟级，因为小号池下多冻一个号
            // 比多撞一次上游更糟（同 RATE_LIMIT_FLOOR_SECS 的取值理由）。
            // 反复触发由 calculate_cooldown_duration 的 1.3^n 递增兜住（上限 90s）。
            CooldownReason::AuthTransient => Duration::from_secs(20),
            CooldownReason::ServerError => Duration::from_secs(30),
            CooldownReason::ModelUnavailable => Duration::from_secs(300),

            // 长冷却（24 小时）
            // 注意：default_duration() 仅供文档/展示参考。AuthenticationFailed /
            // AccountSuspended / QuotaExhausted 均属不可自动恢复（is_auto_recoverable=false），
            // calculate_cooldown_duration 实际走 long_cooldown_secs（= 86400s），而非此处的值。
            CooldownReason::AuthenticationFailed => Duration::from_secs(86400),
            CooldownReason::AccountSuspended => Duration::from_secs(86400),
            CooldownReason::QuotaExhausted => Duration::from_secs(86400),
        }
    }

    /// 是否可以自动恢复
    pub fn is_auto_recoverable(&self) -> bool {
        match self {
            CooldownReason::RateLimitExceeded => true,
            // 可疑活动可自动恢复(调查窗口过去即解除),但走分钟级冷却而非短冷却。
            CooldownReason::SuspiciousActivity => true,
            CooldownReason::TokenRefreshFailed => true,
            CooldownReason::ServerError => true,
            CooldownReason::ModelUnavailable => true,
            // 承重：必须 true。false 会让 calculate_cooldown_duration 走
            // long_cooldown_secs(86400s) 那条分支 —— 那正是本变体要修的僵尸行为。
            // 它同时决定 token_manager::fallback_cooldown_tier 把该号判 Shallow 而非
            // Deep（全池冷却兜底时，会自愈的号该优先于铁定失败的号）。
            CooldownReason::AuthTransient => true,
            CooldownReason::AuthenticationFailed => false,
            CooldownReason::AccountSuspended => false,
            CooldownReason::QuotaExhausted => false,
        }
    }

    /// 获取原因描述
    pub fn description(&self) -> &'static str {
        match self {
            CooldownReason::RateLimitExceeded => "速率限制",
            CooldownReason::SuspiciousActivity => "可疑活动风控",
            CooldownReason::AccountSuspended => "账户暂停",
            CooldownReason::QuotaExhausted => "配额耗尽",
            CooldownReason::TokenRefreshFailed => "Token 刷新失败",
            CooldownReason::AuthenticationFailed => "认证失败",
            // 与「认证失败」区分开是承重的：面板/巡检只能看到这串文案，
            // 两者若同文案则「24h 僵尸」与「20s 抖动」在运维视角上无法分辨。
            CooldownReason::AuthTransient => "认证瞬态失败",
            CooldownReason::ServerError => "服务器错误",
            CooldownReason::ModelUnavailable => "模型暂时不可用",
        }
    }
}

/// `RateLimitExceeded` 的缩放下限（秒）。
///
/// 取 8 而非对照实现的 60：我们的池子实测只有 1~3 个号，而排除集
/// （`acquire_context_excluding`）已经保证单请求内换号不依赖冷却，
/// 冷却在这里只需承担「跨请求别立刻回头撞同一个惩罚窗口」。
/// 60s × 3 个号 = 全池同时冷却 → 网关自造 503，那正是对照实现注释里警告的形态。
const RATE_LIMIT_FLOOR_SECS: u64 = 8;

/// `SuspiciousActivity` 的缩放下限（秒）。基线 20s，缩放下限取 12s。
///
/// 比限流类高一档：账户级风控是「上游正在调查」，反复砸会加重它
/// （见 `report_suspicious_activity` 的说明）。但同样不取分钟级，
/// 理由与上面一致 —— 小号池全冷却比偶尔多撞一次更糟。
const SUSPICIOUS_FLOOR_SECS: u64 = 12;

/// 冷却条目
#[derive(Debug, Clone)]
struct CooldownEntry {
    /// 冷却原因
    reason: CooldownReason,

    /// 冷却开始时间
    started_at: Instant,

    /// 冷却结束时间
    expires_at: Instant,

    /// 连续触发次数（用于递增冷却时长）
    trigger_count: u32,
}

/// 冷却管理器
///
/// 管理所有凭据的冷却状态
pub struct CooldownManager {
    /// 凭据冷却状态
    entries: Mutex<HashMap<u64, CooldownEntry>>,

    /// 最大短冷却时长（秒）
    max_short_cooldown_secs: u64,

    /// 长冷却时长（秒）
    long_cooldown_secs: u64,

    /// 冷却时长缩放百分比（默认 100=原时长）。热更即时生效。只缩放**可自动恢复**(短/瞬时)冷却基数,
    /// 认证失败/封号/配额耗尽那类长硬窗**不缩放**(防误配把死号提前放行)。
    cooldown_scale_pct: std::sync::atomic::AtomicU32,
}

impl Default for CooldownManager {
    fn default() -> Self {
        Self::new()
    }
}

impl CooldownManager {
    /// 创建新的冷却管理器
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            max_short_cooldown_secs: 90, // 上限 90s（原 300s 太黏，小号池下一个卡住请求能把整池压死数分钟）
            long_cooldown_secs: 86400,   // 24 小时
            cooldown_scale_pct: std::sync::atomic::AtomicU32::new(100),
        }
    }

    /// 设置冷却时长缩放百分比（10..500,admin 热更调用,即时生效）。越界 clamp。
    pub fn set_cooldown_scale_pct(&self, pct: u32) {
        self.cooldown_scale_pct
            .store(pct.clamp(10, 500), std::sync::atomic::Ordering::Relaxed);
    }

    /// 对**可自动恢复**的短/瞬时冷却时长按 scale 缩放；长硬窗(不可恢复)原样返回。
    ///
    /// # 缩放下限（2026-08-04 加入）
    ///
    /// 缩放对**限流类**冷却必须有下限。线上 `cooldownScalePct=10` 时
    /// `RateLimitExceeded` 的 15s 基线被缩成 **1.5s** —— 而上游那是账号级惩罚窗口，
    /// 1.5s 后回池等于立刻再撞一次，冷却事实上不存在。
    ///
    /// 对照实现（ZyphrZero kiro.rs v0.7.1）给速率类固定 60s，并在注释里写明为什么
    /// **不**复用 suspicious 的 300s：「否则 8 个号一起被限时会全部长冷却、直接 503」。
    /// 我们的号池同样小，所以取下限而不是固定值 —— 让用户仍能用 scalePct 调，
    /// 但调不到「等于没有」。
    ///
    /// 只对限流/风控两类设下限：`ServerError`(30s)/`ModelUnavailable`(300s)/
    /// `TokenRefreshFailed`(60s) 是本地可判定的瞬态，缩到很短再试是合理的。
    fn scaled_duration(&self, reason: CooldownReason, base: Duration) -> Duration {
        if !reason.is_auto_recoverable() {
            return base; // 认证失败/封号/配额:不缩放,防误配放行死号
        }
        let pct = self
            .cooldown_scale_pct
            .load(std::sync::atomic::Ordering::Relaxed);
        let scaled = if pct == 100 {
            base
        } else {
            Duration::from_millis((base.as_millis() as u64).saturating_mul(pct as u64) / 100)
        };
        match Self::scale_floor(reason) {
            // 下限只抬不压：base 本身比下限还短时（不该发生，但防将来改小基线）取 base，
            // 免得下限反而把冷却拉长到超过该原因的设计意图。
            Some(floor) => scaled.max(floor.min(base)),
            None => scaled,
        }
    }

    /// 该原因的缩放下限。`None` = 不设下限（可被 scalePct 任意缩短）。
    fn scale_floor(reason: CooldownReason) -> Option<Duration> {
        match reason {
            // 上游账号级惩罚窗口：缩到秒级以下等于没有冷却（见 scaled_duration 说明）。
            CooldownReason::RateLimitExceeded => Some(Duration::from_secs(RATE_LIMIT_FLOOR_SECS)),
            CooldownReason::SuspiciousActivity => Some(Duration::from_secs(SUSPICIOUS_FLOOR_SECS)),
            // AuthTransient 刻意**不设**下限：与限流/风控两类不同，它不是「上游正在罚这个号」
            // 的信号 —— 该号的 token 已被证明有效（或 region 正在后台重探），缩短后提前回池
            // 最坏只是多一次上游往返，而不会像限流那样把惩罚窗口砸得更深。
            _ => None,
        }
    }

    /// 使用自定义配置创建冷却管理器
    #[allow(dead_code)]
    pub fn with_config(max_short_cooldown_secs: u64, long_cooldown_secs: u64) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            max_short_cooldown_secs,
            long_cooldown_secs,
            cooldown_scale_pct: std::sync::atomic::AtomicU32::new(100),
        }
    }

    /// 设置凭据冷却
    ///
    /// 时长由 `calculate_cooldown_duration` 根据 trigger_count 指数计算，可能比剩余时间更长或更短。
    /// 若新到期时间 > 已有到期时间则延长冷却；反之保留更长的已有冷却（不缩短）。
    ///
    /// 返回实际的冷却时长
    pub fn set_cooldown(&self, credential_id: u64, reason: CooldownReason) -> Duration {
        self.set_cooldown_with_duration(credential_id, reason, None)
    }

    /// 设置「瞬时 burst」冷却：冷却时长固定取该原因的基线，**不随 trigger_count 指数放大**。
    ///
    /// 用于裸 429（上游 `reason:null` 且无 Retry-After）这类瞬时软限流——通常几秒内自愈，
    /// 用分级递增（`1.3^n`）只会把小 burst 拖成几十秒长冷却、进而把整池压垮（自造雪崩）。
    /// `trigger_count` 仍照常累加（供限流健康观测 / 平静期衰减），但**不参与时长计算**，
    /// 所以反复裸 429 也只是固定基线冷却（如 15s），到期即恢复参与调度。
    ///
    /// 已在更久冷却中时不缩短（取 max），避免把一个上游明确指定的长冷却覆盖成短的。
    pub fn set_transient_cooldown(&self, credential_id: u64, reason: CooldownReason) -> Duration {
        let mut entries = self.entries.lock();
        let now = Instant::now();
        let baseline = self.scaled_duration(reason, reason.default_duration());

        let entry = entries
            .entry(credential_id)
            .or_insert_with(|| CooldownEntry {
                reason,
                started_at: now,
                expires_at: now,
                trigger_count: 0,
            });

        // 平静期衰减（与 set_cooldown_with_duration 同口径）：长时间没再触发就回退强度。
        const DECAY_WINDOW_SECS: u64 = 60;
        if entry.reason == reason && now > entry.expires_at {
            let calm = now.saturating_duration_since(entry.expires_at).as_secs();
            let decay = (calm / DECAY_WINDOW_SECS) as u32;
            if decay > 0 {
                entry.trigger_count = entry.trigger_count.saturating_sub(decay);
            }
        }

        if entry.reason == reason {
            entry.trigger_count += 1;
        } else {
            entry.reason = reason;
            entry.trigger_count = 1;
        }

        // 关键差异：时长固定取基线，不乘 1.3^n。
        let new_expires = now + baseline;
        // 若当前已在更久的冷却中，保留较长的到期时间（不缩短已有长冷却）。
        if new_expires > entry.expires_at {
            entry.started_at = now;
            entry.expires_at = new_expires;
        }
        let remaining = entry.expires_at.saturating_duration_since(now);

        tracing::info!(
            credential_id = %credential_id,
            reason = %reason.description(),
            duration_secs = %remaining.as_secs(),
            trigger_count = %entry.trigger_count,
            "凭据进入瞬时冷却（固定基线，不升级）"
        );

        remaining
    }

    /// 设置凭据冷却（自定义时长）
    pub fn set_cooldown_with_duration(
        &self,
        credential_id: u64,
        reason: CooldownReason,
        custom_duration: Option<Duration>,
    ) -> Duration {
        let mut entries = self.entries.lock();
        let now = Instant::now();

        // 获取或创建条目
        let entry = entries
            .entry(credential_id)
            .or_insert_with(|| CooldownEntry {
                reason,
                started_at: now,
                expires_at: now,
                trigger_count: 0,
            });

        // 平静期衰减：若距上次冷却「结束」已过去足够久（说明期间该号是健康的，
        // 只是没走到一次显式成功来清零），按经过的平静时长回退 trigger_count，
        // 避免小号池长期整池冷却、trigger_count 只增不减一路顶格到上限。
        // 每经过 DECAY_WINDOW 的平静时间回退一级。
        const DECAY_WINDOW_SECS: u64 = 60;
        if entry.reason == reason && now > entry.expires_at {
            let calm = now.saturating_duration_since(entry.expires_at).as_secs();
            let decay = (calm / DECAY_WINDOW_SECS) as u32;
            if decay > 0 {
                entry.trigger_count = entry.trigger_count.saturating_sub(decay);
            }
        }

        // 更新触发次数
        if entry.reason == reason {
            entry.trigger_count += 1;
        } else {
            entry.reason = reason;
            entry.trigger_count = 1;
        }

        // 计算冷却时长
        let duration = custom_duration
            .unwrap_or_else(|| self.calculate_cooldown_duration(reason, entry.trigger_count));

        let new_expires = now + duration;
        // 不缩短已有更长的冷却（与 set_transient_cooldown 一致）：若上游 Retry-After 已设定
        // 更长冷却，自定义时长不应将其覆盖为更短的值。
        if new_expires > entry.expires_at {
            entry.started_at = now;
            entry.expires_at = new_expires;
        }

        tracing::info!(
            credential_id = %credential_id,
            reason = %reason.description(),
            duration_secs = %duration.as_secs(),
            trigger_count = %entry.trigger_count,
            "凭据进入冷却"
        );

        duration
    }

    /// 检查凭据是否在冷却中
    ///
    /// 返回 `None` 表示不在冷却中，`Some((reason, remaining))` 表示冷却原因和剩余时间
    pub fn check_cooldown(&self, credential_id: u64) -> Option<(CooldownReason, Duration)> {
        let entries = self.entries.lock();
        let now = Instant::now();

        entries.get(&credential_id).and_then(|entry| {
            if now < entry.expires_at {
                Some((
                    entry.reason,
                    entry.expires_at.saturating_duration_since(now),
                ))
            } else {
                None
            }
        })
    }

    /// 检查凭据是否可用（不在冷却中或冷却已过期）
    pub fn is_available(&self, credential_id: u64) -> bool {
        self.check_cooldown(credential_id).is_none()
    }

    /// 读取该凭据当前的连续触发次数（未命中返回 0）。用于判断"持续被风控"的强度，
    /// 例如可疑活动 trigger_count 很高说明号基本被 Kiro 盯死，可据此自动禁用。
    pub fn trigger_count(&self, credential_id: u64) -> u32 {
        self.entries
            .lock()
            .get(&credential_id)
            .map(|e| e.trigger_count)
            .unwrap_or(0)
    }

    /// 清除凭据冷却
    pub fn clear_cooldown(&self, credential_id: u64) -> bool {
        let mut entries = self.entries.lock();
        entries.remove(&credential_id).is_some()
    }

    /// 清除所有已过期的冷却
    pub fn cleanup_expired(&self) -> usize {
        let mut entries = self.entries.lock();
        let now = Instant::now();
        let before_count = entries.len();

        entries.retain(|_, entry| now < entry.expires_at);

        let removed = before_count - entries.len();
        if removed > 0 {
            tracing::debug!("清理了 {} 个过期冷却条目", removed);
        }
        removed
    }

    /// 获取所有冷却中的凭据
    pub fn get_all_cooldowns(&self) -> Vec<CooldownInfo> {
        let entries = self.entries.lock();
        let now = Instant::now();

        entries
            .iter()
            .filter(|(_, entry)| now < entry.expires_at)
            .map(|(&id, entry)| CooldownInfo {
                credential_id: id,
                reason: entry.reason,
                started_at_ms: entry.started_at.elapsed().as_millis() as u64,
                remaining_ms: entry.expires_at.saturating_duration_since(now).as_millis() as u64,
                trigger_count: entry.trigger_count,
            })
            .collect()
    }

    /// 计算冷却时长
    fn calculate_cooldown_duration(&self, reason: CooldownReason, trigger_count: u32) -> Duration {
        let base = reason.default_duration();

        // 可疑活动风控：短基线(20s) + 陡增(1.6^n),不受普通短冷却上限(90s)钳制,上限 30min。
        // 曲线:首次20s / 3次~51s / 5次~131s / 8次~9min / 10次封顶30min。
        // 设计意图:偶发一次(号其实还能用)只冷 20s 不白雪藏;只有**持续**被风控才陡升退避,
        // 避免反复砸把账户从软风控推向真封禁。平静期衰减(set_cooldown 内)会随空闲回落 trigger_count。
        if reason == CooldownReason::SuspiciousActivity {
            const SUSPICIOUS_MAX_SECS: u64 = 1800; // 30 分钟
            let multiplier = 1.6_f64.powi((trigger_count.saturating_sub(1)) as i32);
            let duration_secs = (base.as_secs() as f64 * multiplier) as u64;
            // scale 作用在递增后、封顶前:缩放后仍不超过 30min 硬顶。
            return self.scaled_duration(
                reason,
                Duration::from_secs(duration_secs.min(SUSPICIOUS_MAX_SECS)),
            );
        }

        if reason.is_auto_recoverable() {
            // 可自动恢复的原因：温和递增冷却时长（1.3^n，原 1.5^n 涨太快），
            // 上限 max_short_cooldown_secs。配合平静期衰减，避免小号池长期顶格。
            let multiplier = 1.3_f64.powi((trigger_count.saturating_sub(1)) as i32);
            let duration_secs = (base.as_secs() as f64 * multiplier) as u64;
            let capped_secs = duration_secs.min(self.max_short_cooldown_secs);
            self.scaled_duration(reason, Duration::from_secs(capped_secs))
        } else {
            // 非自动恢复原因实际走 long_cooldown_secs (86400s = 24h)，
            // 而非 default_duration() 文档中的值（AuthenticationFailed: 3600s 是参考值不是实际值）。
            // 不可自动恢复的原因：使用长冷却时长
            Duration::from_secs(self.long_cooldown_secs)
        }
    }
}

/// 冷却信息（公开 API）
#[derive(Debug, Clone)]
pub struct CooldownInfo {
    /// 凭据 ID
    pub credential_id: u64,

    /// 冷却原因
    pub reason: CooldownReason,

    /// 冷却开始时间（毫秒前）
    pub started_at_ms: u64,

    /// 剩余冷却时间（毫秒）
    pub remaining_ms: u64,

    /// 连续触发次数
    pub trigger_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cooldown_manager_new() {
        let manager = CooldownManager::new();
        assert!(manager.is_available(1));
    }

    #[test]
    fn test_cooldown_set_and_check() {
        let manager = CooldownManager::new();

        let duration = manager.set_cooldown(1, CooldownReason::RateLimitExceeded);
        // 首次 429 冷却基线（温和化后为 15s；曾为 60s）
        assert!(duration.as_secs() >= 15);

        let (reason, remaining) = manager.check_cooldown(1).unwrap();
        assert_eq!(reason, CooldownReason::RateLimitExceeded);
        assert!(remaining.as_secs() > 0);

        assert!(!manager.is_available(1));
    }

    #[test]
    fn test_cooldown_clear() {
        let manager = CooldownManager::new();

        manager.set_cooldown(1, CooldownReason::ServerError);
        assert!(!manager.is_available(1));

        assert!(manager.clear_cooldown(1));
        assert!(manager.is_available(1));
    }

    #[test]
    fn test_cooldown_incremental() {
        let manager = CooldownManager::new();

        // 第一次冷却
        let d1 = manager.set_cooldown(1, CooldownReason::RateLimitExceeded);

        // 清除后再次触发，应该有更长的冷却
        manager.clear_cooldown(1);
        let d2 = manager.set_cooldown(1, CooldownReason::RateLimitExceeded);

        // 由于触发次数增加，第二次冷却应该更长
        assert!(d2 >= d1);
    }

    /// 冷却上限温和化：连续多次触发也不超过 max_short_cooldown_secs（90s）。
    #[test]
    fn test_cooldown_capped() {
        let manager = CooldownManager::new();
        let mut last = Duration::ZERO;
        for _ in 0..20 {
            last = manager.set_cooldown(1, CooldownReason::RateLimitExceeded);
        }
        assert!(
            last.as_secs() <= 90,
            "冷却应被上限 90s 钳住，实际 {}s",
            last.as_secs()
        );
    }

    #[test]
    fn test_cooldown_reason_auto_recoverable() {
        assert!(CooldownReason::RateLimitExceeded.is_auto_recoverable());
        assert!(CooldownReason::ServerError.is_auto_recoverable());
        assert!(!CooldownReason::AccountSuspended.is_auto_recoverable());
        assert!(!CooldownReason::QuotaExhausted.is_auto_recoverable());
    }

    #[test]
    fn test_cooldown_custom_duration() {
        let manager = CooldownManager::new();

        let custom = Duration::from_secs(10);
        let duration =
            manager.set_cooldown_with_duration(1, CooldownReason::ServerError, Some(custom));

        assert_eq!(duration, custom);
    }

    #[test]
    fn test_cooldown_get_all() {
        let manager = CooldownManager::new();

        manager.set_cooldown(1, CooldownReason::RateLimitExceeded);
        manager.set_cooldown(2, CooldownReason::ServerError);

        let cooldowns = manager.get_all_cooldowns();
        assert_eq!(cooldowns.len(), 2);
    }

    #[test]
    fn test_cooldown_reason_description() {
        assert_eq!(CooldownReason::RateLimitExceeded.description(), "速率限制");
        assert_eq!(CooldownReason::AccountSuspended.description(), "账户暂停");
    }

    /// 瞬时冷却：反复裸 429 时冷却时长恒为基线，绝不随 trigger_count 指数放大。
    #[test]
    fn test_transient_cooldown_does_not_escalate() {
        let manager = CooldownManager::new();
        let baseline = CooldownReason::RateLimitExceeded
            .default_duration()
            .as_secs();

        // 连打 10 次瞬时冷却，每次到期后再打（模拟裸 429 burst 反复发生）。
        let mut last = Duration::ZERO;
        for _ in 0..10 {
            last = manager.set_transient_cooldown(1, CooldownReason::RateLimitExceeded);
            manager.clear_cooldown(1); // 模拟到期/成功清除，下一次重新进入
        }
        // 对比：普通 set_cooldown 连打 10 次会指数涨到接近 90s 上限。
        // 瞬时冷却应始终 == 基线（15s），证明不升级。
        assert_eq!(
            last.as_secs(),
            baseline,
            "瞬时冷却应恒为基线 {}s，不随触发次数放大，实际 {}s",
            baseline,
            last.as_secs()
        );
    }

    /// 可疑活动风控：首次冷却短(基线 20s),不白雪藏还能用的号;明显长于普通限速的即时性但不夸张。
    #[test]
    fn test_suspicious_activity_first_trigger_is_short() {
        let manager = CooldownManager::new();
        let dur = manager.set_cooldown(1, CooldownReason::SuspiciousActivity);
        assert_eq!(
            dur.as_secs(),
            20,
            "可疑活动首次应为短基线 20s,实际 {}s",
            dur.as_secs()
        );
    }

    /// 可疑活动风控:持续触发才陡增,上限 30min(1800s),不无限涨也不被普通90s上限压回。
    #[test]
    fn test_suspicious_activity_escalates_capped_at_30min() {
        let manager = CooldownManager::new();
        let mut last = Duration::ZERO;
        for _ in 0..30 {
            last = manager.set_cooldown(1, CooldownReason::SuspiciousActivity);
        }
        assert!(
            last.as_secs() <= 1800,
            "可疑活动冷却上限应为 30min,实际 {}s",
            last.as_secs()
        );
        assert!(
            last.as_secs() > 90,
            "持续触发应超普通短冷却上限(90s),实际 {}s",
            last.as_secs()
        );
    }

    /// 可疑活动:反复触发时长单调不减(陡增曲线),验证"持续被风控才拉长退避"。
    #[test]
    fn test_suspicious_activity_monotonic_escalation() {
        let manager = CooldownManager::new();
        let d1 = manager.set_cooldown(1, CooldownReason::SuspiciousActivity);
        let d2 = manager.set_cooldown(1, CooldownReason::SuspiciousActivity);
        let d3 = manager.set_cooldown(1, CooldownReason::SuspiciousActivity);
        assert!(
            d2 >= d1 && d3 >= d2,
            "冷却应随连续触发单调不减:{:?}/{:?}/{:?}",
            d1,
            d2,
            d3
        );
        assert!(
            d3.as_secs() > 20,
            "第3次应已超首次基线 20s,实际 {}s",
            d3.as_secs()
        );
    }

    /// 可疑活动号在冷却期内不可用(退出选号轮转,分流自然避开被风控的号)。
    #[test]
    fn test_suspicious_activity_makes_unavailable() {
        let manager = CooldownManager::new();
        manager.set_cooldown(1, CooldownReason::SuspiciousActivity);
        assert!(!manager.is_available(1), "可疑活动冷却期内该号应不可用");
    }

    /// 🔴 回归：`cooldownScalePct` 绝不能把限流冷却缩到「等于没有」。
    ///
    /// 线上实测值 `cooldownScalePct=10` 会把 `RateLimitExceeded` 的 15s 基线缩成
    /// **1.5s** —— 而上游那是账号级惩罚窗口，1.5s 后回池等于立刻再撞一次。
    ///
    /// 回退即 FAIL：删掉 `scaled_duration` 里的 `scale_floor` 分支 → 本条失败。
    #[test]
    fn test_scale_pct_cannot_erase_rate_limit_cooldown() {
        let manager = CooldownManager::new();
        // 最激进的缩放（set_cooldown_scale_pct 的 clamp 下界就是 10）。
        manager.set_cooldown_scale_pct(10);
        let d = manager.set_transient_cooldown(1, CooldownReason::RateLimitExceeded);
        assert!(
            d.as_secs() >= RATE_LIMIT_FLOOR_SECS,
            "scalePct=10 下限流冷却必须仍 >= {}s（旧行为是 15s×10% = 1.5s，\
             等于没有冷却）；实际 {:?}",
            RATE_LIMIT_FLOOR_SECS,
            d
        );

        let m2 = CooldownManager::new();
        m2.set_cooldown_scale_pct(10);
        let d2 = m2.set_cooldown(2, CooldownReason::SuspiciousActivity);
        assert!(
            d2.as_secs() >= SUSPICIOUS_FLOOR_SECS,
            "风控冷却同样要有下限；实际 {:?}",
            d2
        );
    }

    /// 与上一条配对：下限**只对限流/风控两类**生效，其余仍可被 scalePct 缩短。
    ///
    /// 只有上一条时，把 `scale_floor` 写成「对所有原因都返回下限」也能过 ——
    /// 那会让 `ServerError`(30s)/`ModelUnavailable`(300s) 这些本地可判定的瞬态
    /// 也被抬起来，用户调 scalePct 就再也调不动它们了。
    #[test]
    fn test_scale_floor_only_applies_to_throttle_reasons() {
        let manager = CooldownManager::new();
        manager.set_cooldown_scale_pct(10);
        // ServerError 基线 30s → 10% = 3s，应该真的被缩到 3s 附近（不被下限抬起）。
        let d = manager.set_transient_cooldown(1, CooldownReason::ServerError);
        assert!(
            d.as_secs() < RATE_LIMIT_FLOOR_SECS,
            "ServerError 不该被限流下限抬起（它是本地可判定的瞬态，缩短再试合理）；实际 {:?}",
            d
        );
        assert_eq!(
            CooldownManager::scale_floor(CooldownReason::ServerError),
            None
        );
        assert_eq!(
            CooldownManager::scale_floor(CooldownReason::ModelUnavailable),
            None
        );
        assert!(CooldownManager::scale_floor(CooldownReason::RateLimitExceeded).is_some());
    }

    /// `scalePct=100`（不缩放）时下限不得改变原有时长 —— 守「下限只抬不压」。
    #[test]
    fn test_scale_floor_does_not_alter_unscaled_duration() {
        let manager = CooldownManager::new();
        // 默认 100，不缩放。基线 15s，下限 8s < 15s ⇒ 结果必须仍是 15s。
        let d = manager.set_transient_cooldown(1, CooldownReason::RateLimitExceeded);
        assert_eq!(
            d.as_secs(),
            CooldownReason::RateLimitExceeded
                .default_duration()
                .as_secs(),
            "不缩放时下限不该改变基线时长；实际 {:?}",
            d
        );
    }

    /// 🔴 `AuthTransient` 必须是**短且可自愈**的冷却。
    ///
    /// 背景：provider 的四处 401/403 路径经 `report_auth_cooldown` 落
    /// `AuthenticationFailed`（24h 硬窗），而其中 `provider.rs:1778` 一带的注释**明写**
    /// 「只设短冷却 + failover，不计失败」。注释与行为分叉了 24 小时。
    ///
    /// 回退即 FAIL：把 `is_auto_recoverable()` 里 `AuthTransient` 改回 false
    /// → 走 long_cooldown_secs(86400s) → 本条两个断言都失败。
    #[test]
    fn test_auth_transient_is_short_and_auto_recoverable() {
        let manager = CooldownManager::new();
        assert!(
            CooldownReason::AuthTransient.is_auto_recoverable(),
            "瞬态认证失败必须可自愈；否则 calculate_cooldown_duration 会走 86400s 长硬窗"
        );
        let d = manager.set_cooldown(1, CooldownReason::AuthTransient);
        assert_eq!(
            d.as_secs(),
            20,
            "首次瞬态认证失败应为 20s 基线，实际 {}s",
            d.as_secs()
        );
        // 上界用 max_short_cooldown_secs 的实际值守：只要它没走长硬窗那条分支，
        // 就绝不可能落到 24h 量级。
        assert!(
            d.as_secs() <= 90,
            "瞬态认证失败绝不能落长硬窗（旧行为 86400s），实际 {}s",
            d.as_secs()
        );
    }

    /// 与上一条配对：反复触发也只在**短冷却上限**内递增，不会溢出成长硬窗。
    ///
    /// 只有上一条时，把 `AuthTransient` 的 `default_duration()` 写成 20s 但
    /// `is_auto_recoverable()` 留 false 仍可能因首次值巧合而漏过 —— 递增到顶才能
    /// 区分「走的是 1.3^n + 90s 上限」还是「走的是 long_cooldown_secs」。
    #[test]
    fn test_auth_transient_escalation_stays_within_short_cap() {
        let manager = CooldownManager::new();
        let mut last = Duration::ZERO;
        for _ in 0..30 {
            last = manager.set_cooldown(1, CooldownReason::AuthTransient);
        }
        assert!(
            last.as_secs() <= 90,
            "反复瞬态认证失败应被短冷却上限 90s 钳住，实际 {}s（若为 86400 说明走错分支）",
            last.as_secs()
        );
        assert!(
            last.as_secs() >= 20,
            "递增不该低于基线，实际 {}s",
            last.as_secs()
        );
    }

    /// 🔴 守护 `AuthenticationFailed` **本身没被改动**。
    ///
    /// 修这个 bug 的另一条路是「把 AuthenticationFailed 的时长改短」—— 那会让
    /// **真的**认证失败（refreshToken 已废）也变成短冷却，于是每 20s 就拿一个铁定
    /// 失败的号去打上游，永远不停。本条把那条路封死：新增变体可以，改这条不行。
    #[test]
    fn test_authentication_failed_semantics_unchanged() {
        assert!(
            !CooldownReason::AuthenticationFailed.is_auto_recoverable(),
            "真认证失败必须保持不可自愈（refreshToken 废了，短冷却只会反复打上游）"
        );
        assert_eq!(
            CooldownReason::AuthenticationFailed
                .default_duration()
                .as_secs(),
            86400,
            "真认证失败的 24h 时长是刻意的，不得改短"
        );
        let manager = CooldownManager::new();
        let d = manager.set_cooldown(1, CooldownReason::AuthenticationFailed);
        assert_eq!(
            d.as_secs(),
            manager.long_cooldown_secs,
            "真认证失败应走 long_cooldown_secs 长硬窗"
        );
        // 两个变体的文案必须可区分，否则运维视角上「20s 抖动」与「24h 僵尸」无法分辨。
        assert_ne!(
            CooldownReason::AuthenticationFailed.description(),
            CooldownReason::AuthTransient.description()
        );
    }

    /// `AuthTransient` 不设缩放下限（与限流/风控两类相反），见 `scale_floor` 的说明。
    #[test]
    fn test_auth_transient_has_no_scale_floor() {
        assert_eq!(
            CooldownManager::scale_floor(CooldownReason::AuthTransient),
            None,
            "瞬态认证失败不是「上游在罚这个号」的信号，允许 scalePct 调短"
        );
    }

    /// 瞬时冷却不缩短已有的更长冷却（如上游 Retry-After 指定的长冷却）。
    #[test]
    fn test_transient_cooldown_keeps_longer_existing() {
        let manager = CooldownManager::new();
        // 先设一个长冷却（模拟 Retry-After 指定 300s）。
        manager.set_cooldown_with_duration(
            1,
            CooldownReason::RateLimitExceeded,
            Some(Duration::from_secs(300)),
        );
        // 再来一个裸 429 瞬时冷却，不应把 300s 缩短成 15s。
        let remaining = manager.set_transient_cooldown(1, CooldownReason::RateLimitExceeded);
        assert!(
            remaining.as_secs() > 100,
            "瞬时冷却不应缩短已有长冷却，剩余应仍接近 300s，实际 {}s",
            remaining.as_secs()
        );
    }
}
