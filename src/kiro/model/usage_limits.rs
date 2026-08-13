//! 使用额度查询数据模型
//!
//! 包含 getUsageLimits API 的响应类型定义

use serde::Deserialize;

/// Overage（超额）默认额度上限
///
/// 上游偶尔会报告 overage 已开启但不带具体 cap，此时回退到该 UI 约定值，
/// 避免把"已开超额"的号仍按 base 耗尽显示。
pub const DEFAULT_OVERAGE_CAP: f64 = 10_000.0;

/// 使用额度查询响应
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLimitsResponse {
    /// 下次重置日期 (Unix 时间戳)
    #[serde(default)]
    pub next_date_reset: Option<f64>,

    /// 订阅信息
    #[serde(default)]
    pub subscription_info: Option<SubscriptionInfo>,

    /// 使用量明细列表
    #[serde(default)]
    pub usage_breakdown_list: Vec<UsageBreakdown>,

    /// 超额（Online Overage）配置。字段缺失（旧号/无 overage）时为 None。
    #[serde(default)]
    pub overage_configuration: Option<OverageConfiguration>,
}

/// 超额（Online Overage）配置
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverageConfiguration {
    /// 是否开启超额。字段缺失时为 None（视为未开启）。
    #[serde(default)]
    pub overage_enabled: Option<bool>,

    /// 超额状态字符串（`"ENABLED"` / `"DISABLED"`）。
    ///
    /// 上游用**两种方式**表达同一件事，不同账号/时期返回其中之一：布尔
    /// `overageEnabled` 或字符串 `overageStatus`。只认布尔会把「只给了
    /// overageStatus=ENABLED」的号判为未开超额 → cap 不计入 → 额度显示偏低。
    /// 摘自 ZyphrZero/kiro.rs 的同名字段。
    #[serde(default)]
    pub overage_status: Option<String>,
}

/// 订阅信息
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionInfo {
    /// 订阅标题 (KIRO PRO+ / KIRO FREE 等)
    #[serde(default)]
    pub subscription_title: Option<String>,
}

/// 使用量明细
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct UsageBreakdown {
    /// 当前使用量
    #[serde(default)]
    pub current_usage: i64,

    /// 当前使用量（精确值）
    #[serde(default)]
    pub current_usage_with_precision: f64,

    /// 奖励额度列表（上游**可能显式返回 null**，不只是缺字段）。
    ///
    /// ⚠️ 必须用 `Option<Vec<_>>` 而不是 `Vec<_>`：`#[serde(default)]` 只在**字段缺失**时
    /// 生效，显式 `"bonuses": null` 会让整个 `UsageLimitsResponse` 反序列化失败 →
    /// 余额查询整条失败 → 面板显示 0/回退，把「base + bonus」的真实额度吞掉。
    /// 参照 Foxfishc/kiro.rs 的同名字段（它的注释就写着「可能为 null」）。
    #[serde(default)]
    pub bonuses: Option<Vec<Bonus>>,

    /// 免费试用信息
    #[serde(default)]
    pub free_trial_info: Option<FreeTrialInfo>,

    /// 下次重置日期 (Unix 时间戳)
    #[serde(default)]
    pub next_date_reset: Option<f64>,

    /// 使用限额
    #[serde(default)]
    pub usage_limit: i64,

    /// 使用限额（精确值）
    #[serde(default)]
    pub usage_limit_with_precision: f64,

    /// 该明细的超额上限（overage cap）。字段缺失时为 None，
    /// 回退到 DEFAULT_OVERAGE_CAP（仅当 overage 已开启）。
    #[serde(default)]
    pub overage_cap: Option<f64>,
}

/// 奖励额度
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bonus {
    /// 当前使用量
    #[serde(default)]
    pub current_usage: f64,

    /// 使用限额
    #[serde(default)]
    pub usage_limit: f64,

    /// 状态 (ACTIVE / EXPIRED)
    #[serde(default)]
    pub status: Option<String>,
}

impl Bonus {
    /// 检查 bonus 是否处于激活状态（**大小写不敏感**）。
    ///
    /// 改前用严格 `== "ACTIVE"`：上游若返回 `"Active"`/`"active"` 就判为未激活，
    /// bonus 额度不计入 `usage_limit()` → 官方面板显示 15000（base+bonus）而我们只显
    /// 10000。Foxfishc/kiro.rs 与 ZyphrZero/kiro.rs 两家都用 `eq_ignore_ascii_case`，
    /// 这里对齐它们。
    pub fn is_active(&self) -> bool {
        self.status
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("ACTIVE"))
            .unwrap_or(false)
    }
}

/// 免费试用信息
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct FreeTrialInfo {
    /// 当前使用量
    #[serde(default)]
    pub current_usage: i64,

    /// 当前使用量（精确值）
    #[serde(default)]
    pub current_usage_with_precision: f64,

    /// 免费试用过期时间 (Unix 时间戳)
    #[serde(default)]
    pub free_trial_expiry: Option<f64>,

    /// 免费试用状态 (ACTIVE / EXPIRED)
    #[serde(default)]
    pub free_trial_status: Option<String>,

    /// 使用限额
    #[serde(default)]
    pub usage_limit: i64,

    /// 使用限额（精确值）
    #[serde(default)]
    pub usage_limit_with_precision: f64,
}

// ============ 便捷方法实现 ============

impl FreeTrialInfo {
    /// 检查免费试用是否处于激活状态
    pub fn is_active(&self) -> bool {
        self.free_trial_status
            .as_deref()
            .map(|s| s == "ACTIVE")
            .unwrap_or(false)
    }
}

impl UsageLimitsResponse {
    /// 获取订阅标题
    pub fn subscription_title(&self) -> Option<&str> {
        self.subscription_info
            .as_ref()
            .and_then(|info| info.subscription_title.as_deref())
    }

    /// 获取第一个使用量明细
    fn primary_breakdown(&self) -> Option<&UsageBreakdown> {
        self.usage_breakdown_list.first()
    }

    /// 获取总使用限额（精确值）
    ///
    /// 累加基础额度、激活的免费试用额度和激活的奖励额度
    pub fn usage_limit(&self) -> f64 {
        let Some(breakdown) = self.primary_breakdown() else {
            return 0.0;
        };

        let mut total = breakdown.usage_limit_with_precision;

        // 累加激活的 free trial 额度
        if let Some(trial) = &breakdown.free_trial_info {
            if trial.is_active() {
                total += trial.usage_limit_with_precision;
            }
        }

        // 累加激活的 bonus 额度（bonuses 可能是 null → 视作空列表）
        for bonus in breakdown.bonuses.iter().flatten() {
            if bonus.is_active() {
                total += bonus.usage_limit;
            }
        }

        total
    }

    /// 获取总当前使用量（精确值）
    ///
    /// 累加基础使用量、激活的免费试用使用量和激活的奖励使用量
    pub fn current_usage(&self) -> f64 {
        let Some(breakdown) = self.primary_breakdown() else {
            return 0.0;
        };

        let mut total = breakdown.current_usage_with_precision;

        // 累加激活的 free trial 使用量
        if let Some(trial) = &breakdown.free_trial_info {
            if trial.is_active() {
                total += trial.current_usage_with_precision;
            }
        }

        // 累加激活的 bonus 使用量（bonuses 可能是 null → 视作空列表）
        for bonus in breakdown.bonuses.iter().flatten() {
            if bonus.is_active() {
                total += bonus.current_usage;
            }
        }

        total
    }

    // ============ Overage（超额）感知 ============

    /// 上游报告的 overage 开启状态（两个字段都缺时为 None）。
    ///
    /// 上游对同一事实有**两种表达**：布尔 `overageEnabled` 与字符串
    /// `overageStatus`（`"ENABLED"`/`"DISABLED"`）。布尔优先，缺失时回落字符串
    /// （大小写不敏感）——摘自 ZyphrZero/kiro.rs 的 `overage_enabled()`。
    /// 只认布尔会把「只给 overageStatus」的号判为未开超额，cap 不计入 → 额度偏低。
    pub fn overage_enabled_reported(&self) -> Option<bool> {
        let cfg = self.overage_configuration.as_ref()?;
        if let Some(enabled) = cfg.overage_enabled {
            return Some(enabled);
        }
        cfg.overage_status
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("ENABLED"))
    }

    /// overage 是否开启（字段缺失时按未开启处理）
    pub fn overage_enabled(&self) -> bool {
        self.overage_enabled_reported().unwrap_or(false)
    }

    /// 按给定 enabled 状态解析 overage 上限（cap）
    ///
    /// - 未开启：返回 0
    /// - 开启：取 primary_breakdown 的 overage_cap（有限且为正时），否则回退默认值
    pub fn overage_cap_for(&self, enabled: bool) -> f64 {
        if !enabled {
            return 0.0;
        }
        self.primary_breakdown()
            .and_then(|breakdown| breakdown.overage_cap)
            .filter(|cap| cap.is_finite() && *cap > 0.0)
            .unwrap_or(DEFAULT_OVERAGE_CAP)
    }

    /// 有效使用限额（base + overage cap）
    ///
    /// enabled 时 = base + cap；未开启时 = base（与 usage_limit() 一致）。
    pub fn effective_usage_limit_for(&self, overage_enabled: bool) -> f64 {
        self.usage_limit() + self.overage_cap_for(overage_enabled)
    }

    /// 有效剩余额度 = max(0, effective_limit - current_usage)
    pub fn effective_remaining_for(&self, overage_enabled: bool) -> f64 {
        (self.effective_usage_limit_for(overage_enabled) - self.current_usage()).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个带单条 breakdown 的响应，可选 overage 开关与 cap。
    fn make_resp(
        limit: f64,
        usage: f64,
        overage_enabled: Option<bool>,
        overage_cap: Option<f64>,
    ) -> UsageLimitsResponse {
        UsageLimitsResponse {
            next_date_reset: None,
            subscription_info: None,
            usage_breakdown_list: vec![UsageBreakdown {
                current_usage: usage as i64,
                current_usage_with_precision: usage,
                bonuses: None,
                free_trial_info: None,
                next_date_reset: None,
                usage_limit: limit as i64,
                usage_limit_with_precision: limit,
                overage_cap,
            }],
            overage_configuration: overage_enabled.map(|e| OverageConfiguration {
                overage_enabled: Some(e),
                overage_status: None,
            }),
        }
    }

    #[test]
    fn overage_disabled_effective_equals_base() {
        // 无 overage 配置：effective_limit == base，remaining 按 base 计算
        let resp = make_resp(1000.0, 1000.0, None, None);
        assert_eq!(resp.overage_enabled(), false);
        assert_eq!(resp.overage_cap_for(resp.overage_enabled()), 0.0);
        assert_eq!(resp.effective_usage_limit_for(false), 1000.0);
        assert_eq!(resp.effective_remaining_for(false), 0.0);
    }

    #[test]
    fn overage_enabled_with_explicit_cap() {
        // 开启 overage 且带显式 cap：effective_limit = base + cap
        let resp = make_resp(1000.0, 1000.0, Some(true), Some(5000.0));
        assert_eq!(resp.overage_enabled(), true);
        assert_eq!(resp.overage_cap_for(true), 5000.0);
        assert_eq!(resp.effective_usage_limit_for(true), 6000.0);
        // base 已耗尽但 overage 还剩 5000
        assert_eq!(resp.effective_remaining_for(true), 5000.0);
    }

    /// 🔴 `"bonuses": null` 必须能解析（改前 `Vec<Bonus>` 遇显式 null 直接反序列化失败，
    /// 整条余额查询挂掉 → 面板显示 0/回退）。用真实 JSON 走 serde 而不是手构结构体，
    /// 否则测不到反序列化这一层。
    #[test]
    fn bonuses_explicit_null_deserializes() {
        let raw = r#"{
            "usageBreakdownList": [{
                "currentUsage": 100,
                "currentUsageWithPrecision": 100.0,
                "bonuses": null,
                "usageLimit": 10000,
                "usageLimitWithPrecision": 10000.0
            }]
        }"#;
        let resp: UsageLimitsResponse =
            serde_json::from_str(raw).expect("bonuses=null 必须能解析（改前会失败）");
        assert_eq!(resp.usage_limit(), 10000.0);
        assert_eq!(resp.current_usage(), 100.0);
    }

    /// 🔴 官方面板 15000 = base 10000 + 激活 bonus 5000。上游 bonus 状态大小写不定，
    /// 改前严格 `== "ACTIVE"` 会把 `"Active"` 判为未激活 → 只显 10000。
    #[test]
    fn bonus_status_case_insensitive_counts_toward_limit() {
        for status in ["ACTIVE", "Active", "active"] {
            let raw = format!(
                r#"{{
                    "usageBreakdownList": [{{
                        "currentUsage": 0,
                        "currentUsageWithPrecision": 0.0,
                        "bonuses": [{{"currentUsage": 0.0, "usageLimit": 5000.0, "status": "{status}"}}],
                        "usageLimit": 10000,
                        "usageLimitWithPrecision": 10000.0
                    }}]
                }}"#
            );
            let resp: UsageLimitsResponse = serde_json::from_str(&raw).expect("解析应成功");
            assert_eq!(
                resp.usage_limit(),
                15000.0,
                "status={status} 时 bonus 应计入总额度（base 10000 + bonus 5000）"
            );
        }
        // 对照组：非 ACTIVE 的 bonus 不计入
        let raw = r#"{
            "usageBreakdownList": [{
                "currentUsage": 0,
                "currentUsageWithPrecision": 0.0,
                "bonuses": [{"currentUsage": 0.0, "usageLimit": 5000.0, "status": "EXPIRED"}],
                "usageLimit": 10000,
                "usageLimitWithPrecision": 10000.0
            }]
        }"#;
        let resp: UsageLimitsResponse = serde_json::from_str(raw).expect("解析应成功");
        assert_eq!(resp.usage_limit(), 10000.0, "EXPIRED bonus 不得计入");
    }

    /// 🔴 上游只给字符串 `overageStatus=ENABLED`（不给布尔 overageEnabled）时也要
    /// 判定为已开超额。改前只认布尔 → 判为未开 → cap 不计入 → 额度偏低。
    #[test]
    fn overage_status_string_recognized() {
        let raw = r#"{
            "usageBreakdownList": [{
                "currentUsage": 10000,
                "currentUsageWithPrecision": 10000.0,
                "usageLimit": 10000,
                "usageLimitWithPrecision": 10000.0,
                "overageCap": 5000.0
            }],
            "overageConfiguration": {"overageStatus": "ENABLED"}
        }"#;
        let resp: UsageLimitsResponse = serde_json::from_str(raw).expect("解析应成功");
        assert_eq!(
            resp.overage_enabled_reported(),
            Some(true),
            "只给 overageStatus=ENABLED 也应判为已开启（改前返回 None）"
        );
        assert!(resp.overage_enabled());
        assert_eq!(resp.effective_usage_limit_for(true), 15000.0);
        assert_eq!(resp.effective_remaining_for(true), 5000.0);
    }

    /// 布尔优先于字符串：两者冲突时以 `overageEnabled` 为准。
    #[test]
    fn overage_bool_takes_precedence_over_status_string() {
        let raw = r#"{
            "usageBreakdownList": [{"usageLimit": 1000, "usageLimitWithPrecision": 1000.0}],
            "overageConfiguration": {"overageEnabled": false, "overageStatus": "ENABLED"}
        }"#;
        let resp: UsageLimitsResponse = serde_json::from_str(raw).expect("解析应成功");
        assert_eq!(resp.overage_enabled_reported(), Some(false));
        // DISABLED 字符串同样被识别
        let raw2 = r#"{
            "usageBreakdownList": [{"usageLimit": 1000, "usageLimitWithPrecision": 1000.0}],
            "overageConfiguration": {"overageStatus": "DISABLED"}
        }"#;
        let resp2: UsageLimitsResponse = serde_json::from_str(raw2).expect("解析应成功");
        assert_eq!(resp2.overage_enabled_reported(), Some(false));
    }

    #[test]
    fn overage_enabled_without_cap_uses_default() {
        // 开启 overage 但上游未带 cap：回退 DEFAULT_OVERAGE_CAP
        let resp = make_resp(1000.0, 200.0, Some(true), None);
        assert_eq!(resp.overage_cap_for(true), DEFAULT_OVERAGE_CAP);
        assert_eq!(
            resp.effective_usage_limit_for(true),
            1000.0 + DEFAULT_OVERAGE_CAP
        );
        assert_eq!(
            resp.effective_remaining_for(true),
            1000.0 + DEFAULT_OVERAGE_CAP - 200.0
        );
    }

    #[test]
    fn overage_enabled_but_flag_false_disables_cap() {
        // overage_configuration 存在但 overage_enabled=false：cap 为 0
        let resp = make_resp(1000.0, 500.0, Some(false), Some(5000.0));
        assert_eq!(resp.overage_enabled(), false);
        assert_eq!(resp.overage_cap_for(resp.overage_enabled()), 0.0);
        assert_eq!(resp.effective_usage_limit_for(false), 1000.0);
        assert_eq!(resp.effective_remaining_for(false), 500.0);
    }

    #[test]
    fn overage_cap_ignores_nonpositive_and_nonfinite() {
        // cap 为 0 或负数时视作无效，回退默认值
        let resp = make_resp(1000.0, 0.0, Some(true), Some(0.0));
        assert_eq!(resp.overage_cap_for(true), DEFAULT_OVERAGE_CAP);
        let resp_neg = make_resp(1000.0, 0.0, Some(true), Some(-1.0));
        assert_eq!(resp_neg.overage_cap_for(true), DEFAULT_OVERAGE_CAP);
    }

    #[test]
    fn overage_field_absent_deserializes_ok() {
        // 旧号 JSON 完全没有 overage 字段也能反序列化，且视作未开启
        let json = r#"{
            "usageBreakdownList": [
                {"currentUsageWithPrecision": 10.0, "usageLimitWithPrecision": 100.0}
            ]
        }"#;
        let resp: UsageLimitsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.overage_enabled(), false);
        assert_eq!(resp.effective_usage_limit_for(false), 100.0);
        assert_eq!(resp.effective_remaining_for(false), 90.0);
    }
}
