//! 限流 insight 中文文案（纯本地计算，零上游）。
//!
//! 由 `service.rs` 以 `#[path]` 接入；`AdminService::ratelimit_insights` 仍是调用点。

/// 根据 rpm / 冷却状态推断中文限流文案（纯本地计算，零上游）。
///
/// `gate_active`：RPM 硬门在当前配置下是否真的参与调度（balanced 模式 + 池号数 >1，
/// 见 `MultiTokenManager::rpm_saturation_gate_active`）。硬门不生效时即便 rpm 已经
/// 超过 `rpm_limit`，这个阈值对调度也没有拦截力——继续说"建议分流"会让人以为网关在
/// 限制自己，真实原因通常是上游账户级限流（如 USER_REQUEST_RATE_EXCEEDED），应改口
/// 引导去加号/降并发，而不是"分流"（priority 模式/单号池根本没有分流对象）。
pub(super) fn build_insight_text(
    id: u64,
    rpm: u32,
    rpm_limit: u32,
    saturated: bool,
    gate_active: bool,
    disabled: bool,
    cooldown: Option<&crate::kiro::cooldown::CooldownInfo>,
) -> String {
    use crate::kiro::cooldown::CooldownReason;

    if disabled {
        return format!("#{id} 已禁用（不参与调度）");
    }

    if let Some(c) = cooldown {
        // 向上取整到秒，避免展示"剩 0s"却仍在冷却
        let secs = c.remaining_ms.div_ceil(1000);
        if c.reason == CooldownReason::RateLimitExceeded {
            return format!(
                "#{id} 冷却中（速率限制）剩{secs}s，已触发{}次",
                c.trigger_count
            );
        }
        return format!("#{id} 冷却中（{}）剩{secs}s", c.reason.description());
    }

    if saturated {
        // 调用方保证 saturated=true 时 gate_active 也为 true（saturated 已在上游
        // 与 gate_active 做过 &&），这里的 gate_active 分支只是让语义自文档化，
        // 不依赖调用方的隐式约束。
        return if gate_active {
            format!("#{id} 近60s {rpm}/{rpm_limit} 已达软上限，建议分流")
        } else {
            format!("#{id} 近60s {rpm}/{rpm_limit} 超过软上限，但当前调度模式下无分流对象，疑似上游账户级限流，建议加号或降低并发")
        };
    }
    // 接近软上限（>=80%）也提示，便于提前分流；硬门不生效时同理改口，不建议"分流"。
    if rpm_limit > 0 && (rpm as u64) * 5 >= (rpm_limit as u64) * 4 {
        return if gate_active {
            format!("#{id} 近60s {rpm}/{rpm_limit} 接近软上限，建议分流")
        } else {
            format!("#{id} 近60s {rpm}/{rpm_limit} 接近软上限，但当前调度模式下无分流对象，建议关注上游限流")
        };
    }
    "畅通".to_string()
}

#[cfg(test)]
mod insight_text_tests {
    use super::*;
    use crate::kiro::cooldown::{CooldownInfo, CooldownReason};

    /// 无冷却 + 未饱和 → "畅通"
    #[test]
    fn insight_clear() {
        assert_eq!(build_insight_text(1, 3, 50, false, true, false, None), "畅通");
    }

    /// 速率限制冷却中：含"冷却中（速率限制）剩Ns，已触发K次"，剩余毫秒向上取整到秒
    #[test]
    fn insight_rate_limit_cooldown() {
        let cd = CooldownInfo {
            credential_id: 54,
            reason: CooldownReason::RateLimitExceeded,
            started_at_ms: 0,
            remaining_ms: 21_500, // 向上取整应为 22s
            trigger_count: 3,
        };
        let text = build_insight_text(54, 40, 50, false, true, false, Some(&cd));
        assert_eq!(text, "#54 冷却中（速率限制）剩22s，已触发3次");
    }

    /// 非速率限制冷却：走通用分支（不带触发次数）
    #[test]
    fn insight_other_cooldown() {
        let cd = CooldownInfo {
            credential_id: 7,
            reason: CooldownReason::ServerError,
            started_at_ms: 0,
            remaining_ms: 5_000,
            trigger_count: 1,
        };
        let text = build_insight_text(7, 0, 50, false, true, false, Some(&cd));
        assert_eq!(text, "#7 冷却中（服务器错误）剩5s");
    }

    /// 已达软上限 + 硬门生效(balanced+池>1) → "已达软上限，建议分流"
    #[test]
    fn insight_saturated_gate_active() {
        let text = build_insight_text(54, 50, 50, true, true, false, None);
        assert_eq!(text, "#54 近60s 50/50 已达软上限，建议分流");
    }

    /// 接近软上限（>=80%）+ 硬门生效 → "接近软上限，建议分流"
    #[test]
    fn insight_near_saturation_gate_active() {
        // 40/50 = 80%
        let text = build_insight_text(54, 40, 50, false, true, false, None);
        assert_eq!(text, "#54 近60s 40/50 接近软上限，建议分流");
    }

    /// rpm_limit=0（不限制）时永不判为接近上限，恒"畅通"（与 gate_active 无关）
    #[test]
    fn insight_no_limit_always_clear() {
        assert_eq!(build_insight_text(9, 999, 0, false, true, false, None), "畅通");
        assert_eq!(build_insight_text(9, 999, 0, false, false, false, None), "畅通");
    }

    /// ⭐回归(#虚假饱和告警)：硬门不生效(priority 模式 / 单号池)时，即便 rpm 已达/超过
    /// 阈值，也不能再说"建议分流"——priority 模式下这个阈值对调度没有任何拦截力,
    /// "分流"这个词本身就是误导(根本没有第二个号可分)。改口引导去查上游账户级限流。
    /// 旧代码里 `saturated` 参数一旦为 true 就无条件走"已达软上限，建议分流"分支，
    /// 与 gate_active 完全无关——本测试对着新签名传 gate_active=false 会触发新分支，
    /// 证明新逻辑确实按 gate_active 分岔（旧函数体没有这个参数，编译都过不了，
    /// 这本身就是最强的"旧代码会失败"证据：旧调用点全是 6 个参数）。
    #[test]
    fn insight_saturated_but_gate_inactive_does_not_say_spillover() {
        let text = build_insight_text(54, 51, 25, true, false, false, None);
        assert_eq!(
            text,
            "#54 近60s 51/25 超过软上限，但当前调度模式下无分流对象，疑似上游账户级限流，建议加号或降低并发"
        );
        assert!(!text.contains("建议分流"), "硬门未生效时绝不能出现\"建议分流\"字样: {text}");
    }

    /// 同理:接近软上限但硬门未生效，也不该说"建议分流"。
    #[test]
    fn insight_near_saturation_but_gate_inactive_does_not_say_spillover() {
        let text = build_insight_text(54, 20, 25, false, false, false, None);
        assert!(!text.contains("建议分流"), "硬门未生效时接近上限也不该建议分流: {text}");
        assert!(text.contains("接近软上限"), "仍应保留接近上限的事实描述: {text}");
    }

    /// 已禁用号:显示"已禁用"而非"畅通"(即便有 RPM/未冷却)
    #[test]
    fn insight_disabled() {
        assert_eq!(
            build_insight_text(54, 0, 50, false, true, true, None),
            "#54 已禁用（不参与调度）"
        );
    }
}
