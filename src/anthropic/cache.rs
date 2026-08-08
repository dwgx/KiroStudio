//! Prompt Cache 四层降级链（移植自 k2cc，2026-08-08）。
//!
//! 优先级（高→低）：
//! 1. **metering 真值**：上游 Kiro `MeteringEvent.cacheReadInputTokens/cacheCreationInputTokens`
//! 2. **prefix 估算**：`token::count_prefix_tokens` 的本地前缀估算（KiroStudio 原有 Layer 2）
//! 3. **fingerprint 命中**：账号级前缀指纹（TODO：k2cc `cache/fingerprint.rs` 未移植，恒 None）
//! 4. **ratio 兜底**：50% cache / 30% creation
//!
//! 所有分支输出均经 [`PromptCacheUsage::clamp_to_total`] 截断，保证
//! `cache_creation_5m + cache_creation_1h == cache_creation_input_tokens` 不变量。
//!
//! 入库/对外分叉：本模块产出的 [`CacheUsageBreakdown`] 是**未缩放真值**（直接落库）；
//! 对外下发给客户端时的 ×0.6657 缩放由 `handlers` / `stream` 的 `scale_for_client` 负责。

use crate::anthropic::stream::CacheUsageBreakdown;

/// 四层降级链选出的 cache 记账中间形态。
///
/// 不含 `input_tokens`（billed 口径）：它恒等于 `total - cache_read - cache_creation`，
/// 由消费方（`billed_input_tokens`）派生，避免在同一份数据里存两份会漂移的数字。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PromptCacheUsage {
    pub cache_creation_input_tokens: i32,
    pub cache_read_input_tokens: i32,
    pub cache_creation_5m_input_tokens: i32,
    pub cache_creation_1h_input_tokens: i32,
}

impl PromptCacheUsage {
    /// Layer 4 ratio 兜底：`cache_ratio` 比例计入缓存，其中 `creation_ratio` 比例是新建写入。
    pub(crate) fn from_ratios(input_tokens: i32, cache_ratio: f64, creation_ratio: f64) -> Self {
        let cached_total = ((input_tokens as f64) * cache_ratio.clamp(0.0, 1.0)) as i32;
        let cache_creation = ((cached_total as f64) * creation_ratio.clamp(0.0, 1.0)) as i32;
        let cache_read = cached_total.saturating_sub(cache_creation);
        let (creation_5m, creation_1h) = split_creation_by_ephemeral_ratio(cache_creation, 0.0);
        Self {
            cache_creation_input_tokens: cache_creation,
            cache_read_input_tokens: cache_read,
            cache_creation_5m_input_tokens: creation_5m,
            cache_creation_1h_input_tokens: creation_1h,
        }
    }

    /// 强制截断保证 `cache_read + cache_creation <= total`。
    /// 截断时优先保留 cache_read；5m/1h 按原比例同步缩放，保持 `5m+1h==creation`。
    pub(crate) fn clamp_to_total(self, total_input: i32) -> Self {
        let total = total_input.max(0);
        let cache_read = self.cache_read_input_tokens.clamp(0, total);
        let remaining = total.saturating_sub(cache_read);
        let cache_creation = self.cache_creation_input_tokens.clamp(0, remaining);
        let (creation_5m, creation_1h) = split_creation_preserving_ratio(self, cache_creation);
        Self {
            cache_creation_input_tokens: cache_creation,
            cache_read_input_tokens: cache_read,
            cache_creation_5m_input_tokens: creation_5m,
            cache_creation_1h_input_tokens: creation_1h,
        }
    }

    /// 收敛为 KiroStudio 的 [`CacheUsageBreakdown`]（未缩放真值，落库直接写）。
    pub(crate) fn to_cache_breakdown(self) -> CacheUsageBreakdown {
        CacheUsageBreakdown {
            cache_creation_input_tokens: self.cache_creation_input_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens,
            cache_creation_5m_input_tokens: self.cache_creation_5m_input_tokens,
            cache_creation_1h_input_tokens: self.cache_creation_1h_input_tokens,
        }
    }
}

/// 按 `ephemeral1hRatio` 拆分 cache_creation 到 5m / 1h tier（确定性分配，四舍五入）。
pub(crate) fn split_creation_by_ephemeral_ratio(creation: i32, ratio_1h: f64) -> (i32, i32) {
    let ratio = ratio_1h.clamp(0.0, 1.0);
    let one_h = ((creation as f64 * ratio) + 0.5).floor() as i32;
    let one_h = one_h.clamp(0, creation.max(0));
    let five_m = creation.saturating_sub(one_h);
    (five_m, one_h)
}

/// 截断后按原 5m/1h 比例重算拆分（原 creation 为 0 时安全返回全 0）。
fn split_creation_preserving_ratio(original: PromptCacheUsage, creation: i32) -> (i32, i32) {
    if original.cache_creation_input_tokens > 0 {
        let ratio_1h = original.cache_creation_1h_input_tokens as f64
            / original.cache_creation_input_tokens as f64;
        split_creation_by_ephemeral_ratio(creation, ratio_1h)
    } else {
        (0, 0)
    }
}

/// 四层降级链选择终值 cache 记账（返回**未缩放** [`CacheUsageBreakdown`]，对外缩放由调用方做）。
///
/// - `metering`：Layer 1 上游真值 `(cache_read, cache_creation)`
/// - `prefix_estimated_read`：Layer 2 `count_prefix_tokens` 估算
/// - `fingerprint_usage`：Layer 3 账号级指纹（TODO：未移植，恒传 None）
/// - `ratio_fallback`：Layer 4 比例兜底（`from_ratios` 产出）
pub(crate) fn select_final_usage(
    final_input_tokens: i32,
    metering: Option<(i32, i32)>,
    prefix_estimated_read: Option<i32>,
    fingerprint_usage: Option<PromptCacheUsage>,
    ratio_fallback: PromptCacheUsage,
) -> CacheUsageBreakdown {
    let usage = if let Some((read, creation)) = metering {
        // Layer 1：Kiro metering 不返回 5m/1h 拆分，默认全部归为 5m。
        PromptCacheUsage {
            cache_creation_input_tokens: creation,
            cache_read_input_tokens: read,
            cache_creation_5m_input_tokens: creation,
            cache_creation_1h_input_tokens: 0,
        }
        .clamp_to_total(final_input_tokens)
    } else if let Some(estimated) = prefix_estimated_read {
        // Layer 2：prefix 估算（既有行为；无新建缓存）。
        let read = estimated.min(final_input_tokens);
        PromptCacheUsage {
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: read,
            cache_creation_5m_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
        }
        .clamp_to_total(final_input_tokens)
    } else if let Some(fp) = fingerprint_usage {
        // Layer 3：fingerprint 命中（TODO：账号级前缀指纹未移植，恒 None）。
        fp.clamp_to_total(final_input_tokens)
    } else {
        // Layer 4：ratio 兜底。
        ratio_fallback.clamp_to_total(final_input_tokens)
    };
    usage.to_cache_breakdown()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ratio_fallback(total: i32) -> PromptCacheUsage {
        // Layer 4 兜底：50% cache，其中 30% creation。
        PromptCacheUsage::from_ratios(total, 0.5, 0.3)
    }

    fn invariant_holds(u: CacheUsageBreakdown, total: i32) -> bool {
        u.cache_read_input_tokens >= 0
            && u.cache_creation_input_tokens >= 0
            && u.cache_creation_5m_input_tokens + u.cache_creation_1h_input_tokens
                == u.cache_creation_input_tokens
            && u.cache_read_input_tokens + u.cache_creation_input_tokens <= total
    }

    #[test]
    fn layer1_metering_wins_over_all() {
        let total = 1000;
        let final_u = select_final_usage(
            total,
            Some((600, 200)),
            Some(500),
            None,
            ratio_fallback(total),
        );
        assert_eq!(final_u.cache_read_input_tokens, 600);
        assert_eq!(final_u.cache_creation_input_tokens, 200);
        assert_eq!(final_u.cache_creation_5m_input_tokens, 200);
        assert_eq!(final_u.cache_creation_1h_input_tokens, 0);
        assert!(invariant_holds(final_u, total));
    }

    #[test]
    fn layer2_prefix_wins_when_metering_absent() {
        let total = 1000;
        let final_u = select_final_usage(total, None, Some(400), None, ratio_fallback(total));
        assert_eq!(final_u.cache_read_input_tokens, 400);
        assert_eq!(final_u.cache_creation_input_tokens, 0);
        assert!(invariant_holds(final_u, total));
    }

    #[test]
    fn layer4_ratio_fallback_when_higher_absent() {
        let total = 1000;
        let final_u = select_final_usage(total, None, None, None, ratio_fallback(total));
        // 50% × 1000 = 500 cache，其中 30% creation = 150，read = 350。
        assert_eq!(final_u.cache_read_input_tokens, 350);
        assert_eq!(final_u.cache_creation_input_tokens, 150);
        assert!(invariant_holds(final_u, total));
    }

    #[test]
    fn metering_over_total_is_clamped() {
        // 80 + 50 = 130 > 100 → cache_read 优先保留 80，剩余 20 全给 creation。
        let total = 100;
        let final_u = select_final_usage(total, Some((80, 50)), None, None, ratio_fallback(total));
        assert_eq!(final_u.cache_read_input_tokens, 80);
        assert_eq!(final_u.cache_creation_input_tokens, 20);
        assert!(invariant_holds(final_u, total));
    }

    #[test]
    fn prefix_over_total_is_clamped() {
        let total = 100;
        let final_u = select_final_usage(total, None, Some(300), None, ratio_fallback(total));
        assert_eq!(final_u.cache_read_input_tokens, 100);
        assert_eq!(final_u.cache_creation_input_tokens, 0);
        assert!(invariant_holds(final_u, total));
    }

    #[test]
    fn split_creation_by_ephemeral_ratio_boundaries() {
        assert_eq!(split_creation_by_ephemeral_ratio(100, 0.0), (100, 0));
        assert_eq!(split_creation_by_ephemeral_ratio(100, 1.0), (0, 100));
        assert_eq!(split_creation_by_ephemeral_ratio(100, 0.3), (70, 30));
        assert_eq!(split_creation_by_ephemeral_ratio(0, 0.5), (0, 0));
    }
}
