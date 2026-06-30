//! 精细化速率限制系统
#![allow(dead_code)]
//!
//! 实现每日请求限制、请求间隔控制、指数退避等策略，
//! 模拟人类使用模式，降低被检测风险。
//! 参考 CLIProxyAPIPlus 的实现。

use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 默认每日最大请求数
const DEFAULT_DAILY_MAX_REQUESTS: u32 = 500;

/// 默认最小请求间隔（毫秒）
const DEFAULT_MIN_INTERVAL_MS: u64 = 1000;

/// 默认最大请求间隔（毫秒）
const DEFAULT_MAX_INTERVAL_MS: u64 = 2000;

/// 默认抖动百分比
const DEFAULT_JITTER_PERCENT: f64 = 0.3;

/// 默认退避基数（毫秒）
const DEFAULT_BACKOFF_BASE_MS: u64 = 30_000;

/// 默认最大退避时间（毫秒）
const DEFAULT_BACKOFF_MAX_MS: u64 = 300_000;

/// 默认退避倍数
const DEFAULT_BACKOFF_MULTIPLIER: f64 = 1.5;

/// 暂停检测关键词
const SUSPEND_KEYWORDS: &[&str] = &[
    "suspended",
    "banned",
    "quota exceeded",
    "rate limit",
    "too many requests",
    "account disabled",
];

/// 速率限制配置
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// 每日最大请求数
    pub daily_max_requests: u32,

    /// 最小请求间隔（毫秒）
    pub min_interval_ms: u64,

    /// 最大请求间隔（毫秒）
    pub max_interval_ms: u64,

    /// 抖动百分比（0.0 - 1.0）
    pub jitter_percent: f64,

    /// 退避基数（毫秒）
    pub backoff_base_ms: u64,

    /// 最大退避时间（毫秒）
    pub backoff_max_ms: u64,

    /// 退避倍数
    pub backoff_multiplier: f64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            daily_max_requests: DEFAULT_DAILY_MAX_REQUESTS,
            min_interval_ms: DEFAULT_MIN_INTERVAL_MS,
            max_interval_ms: DEFAULT_MAX_INTERVAL_MS,
            jitter_percent: DEFAULT_JITTER_PERCENT,
            backoff_base_ms: DEFAULT_BACKOFF_BASE_MS,
            backoff_max_ms: DEFAULT_BACKOFF_MAX_MS,
            backoff_multiplier: DEFAULT_BACKOFF_MULTIPLIER,
        }
    }
}

/// 凭据速率状态
#[derive(Debug, Clone)]
struct CredentialRateState {
    /// 今日请求计数
    daily_count: u32,

    /// 计数重置时间
    count_reset_at: Instant,

    /// 上次请求时间
    last_request_at: Option<Instant>,

    /// 连续失败次数（用于退避计算）
    consecutive_failures: u32,

    /// 当前退避结束时间
    backoff_until: Option<Instant>,
}

impl Default for CredentialRateState {
    fn default() -> Self {
        Self {
            daily_count: 0,
            count_reset_at: Instant::now() + Duration::from_secs(86400),
            last_request_at: None,
            consecutive_failures: 0,
            backoff_until: None,
        }
    }
}

/// 速率限制器
///
/// 管理所有凭据的速率限制状态
pub struct RateLimiter {
    config: RwLock<RateLimitConfig>,
    states: Mutex<HashMap<u64, CredentialRateState>>,
}

impl RateLimiter {
    /// 创建新的速率限制器
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config: RwLock::new(config),
            states: Mutex::new(HashMap::new()),
        }
    }

    /// 使用默认配置创建速率限制器
    pub fn with_defaults() -> Self {
        Self::new(RateLimitConfig::default())
    }

    /// 热更新速率限制配置
    pub fn update_config(&self, new_config: RateLimitConfig) {
        *self.config.write() = new_config;
    }

    /// 检查凭据是否可以发送请求
    ///
    /// 返回 `Ok(())` 表示可以发送，`Err(Duration)` 表示需要等待的时间
    pub fn check_rate_limit(&self, credential_id: u64) -> Result<(), Duration> {
        let config = self.config.read().clone();
        let mut states = self.states.lock();
        let state = states.entry(credential_id).or_default();
        let now = Instant::now();

        // 检查是否需要重置每日计数
        if now >= state.count_reset_at {
            state.daily_count = 0;
            state.count_reset_at = now + Duration::from_secs(86400);
        }

        // 检查每日限制
        if state.daily_count >= config.daily_max_requests {
            let wait_time = state.count_reset_at.saturating_duration_since(now);
            return Err(wait_time);
        }

        // 检查退避状态
        if let Some(backoff_until) = state.backoff_until {
            if now < backoff_until {
                return Err(backoff_until.saturating_duration_since(now));
            }
            // 退避已结束，清除状态
            state.backoff_until = None;
        }

        // 检查请求间隔
        if let Some(last_request) = state.last_request_at {
            let min_interval = Self::calculate_interval_with_config(&config);
            let elapsed = now.saturating_duration_since(last_request);
            if elapsed < min_interval {
                return Err(min_interval - elapsed);
            }
        }

        Ok(())
    }

    /// 尝试获取一次“发送许可”（原子检查 + 占位）
    ///
    /// `check_rate_limit()` 仅做检查，不会更新状态，无法在并发场景下避免“同时放行”。
    /// 本方法在同一把锁内完成检查与 `last_request_at` 更新，用于：
    /// - 限制单个凭据的请求频率（近似 RPM/最小间隔）
    /// - 在并发请求下将流量自然分流到其他可用凭据
    ///
    /// 返回 `Ok(())` 表示已占用一个发送窗口；`Err(Duration)` 表示需要等待的时间。
    pub fn try_acquire(&self, credential_id: u64) -> Result<(), Duration> {
        let config = self.config.read().clone();
        let min_interval = Self::calculate_interval_with_config(&config);

        let mut states = self.states.lock();
        let state = states.entry(credential_id).or_default();
        let now = Instant::now();

        // 检查是否需要重置每日计数
        if now >= state.count_reset_at {
            state.daily_count = 0;
            state.count_reset_at = now + Duration::from_secs(86400);
        }

        // 检查每日限制
        if state.daily_count >= config.daily_max_requests {
            let wait_time = state.count_reset_at.saturating_duration_since(now);
            return Err(wait_time);
        }

        // 检查退避状态
        if let Some(backoff_until) = state.backoff_until {
            if now < backoff_until {
                return Err(backoff_until.saturating_duration_since(now));
            }
            // 退避已结束，清除状态
            state.backoff_until = None;
        }

        // 检查请求间隔
        if let Some(last_request) = state.last_request_at {
            let elapsed = now.saturating_duration_since(last_request);
            if elapsed < min_interval {
                return Err(min_interval - elapsed);
            }
        }

        // 占位：更新上次请求时间，避免并发下同一凭据被同时放行
        state.last_request_at = Some(now);
        Ok(())
    }

    /// 记录请求成功
    pub fn record_success(&self, credential_id: u64) {
        let mut states = self.states.lock();
        let state = states.entry(credential_id).or_default();

        state.daily_count += 1;
        state.last_request_at = Some(Instant::now());
        state.consecutive_failures = 0;
        state.backoff_until = None;
    }

    /// 记录请求失败
    ///
    /// 返回下次可以重试的等待时间
    pub fn record_failure(&self, credential_id: u64, error_message: Option<&str>) -> Duration {
        // 先获取 config 读锁，再获取 states 锁（与 check_rate_limit/try_acquire 保持一致）
        let config = self.config.read().clone();

        let mut states = self.states.lock();
        let state = states.entry(credential_id).or_default();
        let now = Instant::now();

        state.consecutive_failures += 1;
        state.last_request_at = Some(now);

        // 检查是否触发暂停检测
        let is_suspended = error_message
            .map(|msg| {
                let lower = msg.to_ascii_lowercase();
                SUSPEND_KEYWORDS.iter().any(|kw| lower.contains(kw))
            })
            .unwrap_or(false);

        // 计算退避时间
        let backoff = if is_suspended {
            // 暂停检测触发长时间退避（1 小时）
            Duration::from_secs(3600)
        } else {
            Self::calculate_backoff_with_config(&config, state.consecutive_failures)
        };

        state.backoff_until = Some(now + backoff);
        backoff
    }

    /// 获取凭据的当前状态
    pub fn get_state(&self, credential_id: u64) -> Option<RateLimitState> {
        let config = self.config.read();
        let states = self.states.lock();
        states.get(&credential_id).map(|s| {
            let now = Instant::now();
            RateLimitState {
                daily_count: s.daily_count,
                daily_remaining: config.daily_max_requests.saturating_sub(s.daily_count),
                consecutive_failures: s.consecutive_failures,
                is_in_backoff: s.backoff_until.map(|t| now < t).unwrap_or(false),
                backoff_remaining_ms: s
                    .backoff_until
                    .map(|t| t.saturating_duration_since(now).as_millis() as u64)
                    .unwrap_or(0),
            }
        })
    }

    /// 重置凭据的速率限制状态
    pub fn reset(&self, credential_id: u64) {
        let mut states = self.states.lock();
        states.remove(&credential_id);
    }

    /// 重置所有凭据的速率限制状态
    #[allow(dead_code)]
    pub fn reset_all(&self) {
        let mut states = self.states.lock();
        states.clear();
    }

    /// 计算请求间隔（带抖动）
    fn calculate_interval(&self) -> Duration {
        let config = self.config.read();
        Self::calculate_interval_with_config(&config)
    }

    /// 使用指定配置计算请求间隔（避免重复获取读锁）
    fn calculate_interval_with_config(config: &RateLimitConfig) -> Duration {
        let base = (config.min_interval_ms + config.max_interval_ms) / 2;
        let jitter_range = (base as f64 * config.jitter_percent) as u64;
        let jitter = if jitter_range > 0 {
            fastrand::u64(0..=jitter_range * 2) as i64 - jitter_range as i64
        } else {
            0
        };
        let interval = (base as i64 + jitter)
            .max(config.min_interval_ms as i64)
            .min(config.max_interval_ms as i64) as u64;
        Duration::from_millis(interval)
    }

    /// 计算指数退避时间
    fn calculate_backoff(&self, failures: u32) -> Duration {
        let config = self.config.read();
        Self::calculate_backoff_with_config(&config, failures)
    }

    /// 使用指定配置计算指数退避时间（避免重复获取读锁）
    fn calculate_backoff_with_config(config: &RateLimitConfig, failures: u32) -> Duration {
        let base = config.backoff_base_ms as f64;
        let multiplier = config.backoff_multiplier;
        let max = config.backoff_max_ms;

        // 指数退避：base * multiplier^(failures-1)
        let backoff = base * multiplier.powi((failures.saturating_sub(1)) as i32);
        let backoff_ms = (backoff as u64).min(max);

        // 添加抖动
        let jitter_range = (backoff_ms as f64 * config.jitter_percent) as u64;
        let jitter = if jitter_range > 0 {
            fastrand::u64(0..=jitter_range)
        } else {
            0
        };

        // 在添加抖动后再进行上限约束，确保不超过 backoff_max_ms
        let final_backoff = (backoff_ms + jitter).min(max);
        Duration::from_millis(final_backoff)
    }
}

/// 速率限制状态（公开 API）
#[derive(Debug, Clone)]
pub struct RateLimitState {
    /// 今日请求计数
    pub daily_count: u32,

    /// 今日剩余请求数
    pub daily_remaining: u32,

    /// 连续失败次数
    pub consecutive_failures: u32,

    /// 是否处于退避状态
    pub is_in_backoff: bool,

    /// 退避剩余时间（毫秒）
    pub backoff_remaining_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limiter_new() {
        let limiter = RateLimiter::with_defaults();
        assert!(limiter.check_rate_limit(1).is_ok());
    }

    #[test]
    fn test_rate_limiter_daily_limit() {
        let config = RateLimitConfig {
            daily_max_requests: 2,
            min_interval_ms: 0,
            max_interval_ms: 0,
            ..Default::default()
        };
        let limiter = RateLimiter::new(config);

        // 前两次请求应该成功
        assert!(limiter.check_rate_limit(1).is_ok());
        limiter.record_success(1);
        assert!(limiter.check_rate_limit(1).is_ok());
        limiter.record_success(1);

        // 第三次应该被限制
        assert!(limiter.check_rate_limit(1).is_err());
    }

    #[test]
    fn test_rate_limiter_backoff() {
        let config = RateLimitConfig {
            backoff_base_ms: 100,
            backoff_multiplier: 2.0,
            jitter_percent: 0.0, // 禁用抖动以便测试
            min_interval_ms: 0,
            max_interval_ms: 0,
            ..Default::default()
        };
        let limiter = RateLimiter::new(config);

        // 记录失败
        let backoff1 = limiter.record_failure(1, None);
        assert!(backoff1.as_millis() >= 100);

        // 第二次失败应该有更长的退避
        let backoff2 = limiter.record_failure(1, None);
        assert!(backoff2.as_millis() >= 200);
    }

    #[test]
    fn test_rate_limiter_suspend_detection() {
        let limiter = RateLimiter::with_defaults();

        // 触发暂停检测
        let backoff = limiter.record_failure(1, Some("Your account has been suspended"));
        assert!(backoff.as_secs() >= 3600);
    }

    #[test]
    fn test_rate_limiter_success_resets_failures() {
        let limiter = RateLimiter::with_defaults();

        // 记录几次失败
        limiter.record_failure(1, None);
        limiter.record_failure(1, None);

        let state = limiter.get_state(1).unwrap();
        assert_eq!(state.consecutive_failures, 2);

        // 成功后重置
        limiter.reset(1);
        limiter.record_success(1);

        let state = limiter.get_state(1).unwrap();
        assert_eq!(state.consecutive_failures, 0);
    }

    #[test]
    fn test_rate_limiter_get_state() {
        let limiter = RateLimiter::with_defaults();

        // 初始状态不存在
        assert!(limiter.get_state(1).is_none());

        // 记录成功后有状态
        limiter.record_success(1);
        let state = limiter.get_state(1).unwrap();
        assert_eq!(state.daily_count, 1);
        assert_eq!(state.consecutive_failures, 0);
    }

    #[test]
    fn test_rate_limiter_reset() {
        let limiter = RateLimiter::with_defaults();

        limiter.record_success(1);
        limiter.record_failure(1, None);

        assert!(limiter.get_state(1).is_some());

        limiter.reset(1);
        assert!(limiter.get_state(1).is_none());
    }
}
