//! 使用额度查询数据模型
//!
//! 包含 getUsageLimits API 的响应类型定义

use serde::Deserialize;

/// Overage（超额）默认额度上限
///
/// 上游偶尔会报告 overage 已开启但不带具体 cap，此时回退到该 UI 约定值，
/// 避免把"已开超额"的号仍按 base 耗尽显示。
pub const DEFAULT_OVERAGE_CAP: f64 = 10_000.0;

/// 精度字段缺失时回退整数兄弟字段。
///
/// `usageLimit` / `currentUsage` 与 `*WithPrecision` 都是 `#[serde(default)]`：上游若只给
/// 整数（`usageLimit` / `currentUsage`），精度字段会静默反序列化成 `0.0`，直接相加就把真实
/// 额度报成 0（面板显示 0 限额、`balance_factor` 跟着失真）。故只在「精度值为 0 且整数非 0」
/// 时回退 —— 真值本来就是 0 的情形（两边都是 0）不受影响。
fn precise_or_int(precise: f64, int_value: i64) -> f64 {
    if precise == 0.0 && int_value != 0 {
        int_value as f64
    } else {
        precise
    }
}

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
    ///
    /// ⚠️ 当前**只反序列化、不参与判定**（`is_active()` 只看状态字符串）。刻意如此：上游是否
    /// 会在试用到期后仍返回 `ACTIVE` 没有实证，而这个字段出现 `0`/异常值时按时间判定会**反向**
    /// 丢掉有效的试用额度（比多算更糟，2026-10-14 审计条目）。要改成时间感知，先抓一份
    /// 「状态仍为 ACTIVE 但已过 freeTrialExpiry」的真实响应，再定「过期即不计」的规则。
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
    /// 检查免费试用是否处于激活状态（**大小写不敏感**）。
    ///
    /// 与 [`Bonus::is_active`] 同口径：上游的状态字符串大小写不定（`ACTIVE` / `Active` /
    /// `active`），严格 `==` 会把 `"Active"` 判成未激活 → free trial 的额度与用量**都不计入**
    /// → 面板额度偏低。bonus 那处已因同款 bug 修过一次（见 `Bonus::is_active` 的注释），
    /// 这里漏改过，属同一类。
    pub fn is_active(&self) -> bool {
        self.free_trial_status
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("ACTIVE"))
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
    ///
    /// ⚠️ **已知假设**：上游 `usageBreakdownList` 目前只返回一项（该账号的唯一计费资源），
    /// 故 `usage_limit()` / `current_usage()` / overage cap 都只看这一项，多出来的明细被
    /// 丢弃。若将来出现多项（不同资源类型各带独立配额），这里的取舍需要先定规则再改：
    /// 「求和」与「按类型过滤」会给出完全不同的额度，而这两个数字直接进面板余额与
    /// `balance_factor` 选号权重，猜错会把额度放大（比丢弃更糟）。
    /// 验证方法：抓一次多明细的 `getUsageLimits` 响应（config `upstreamTracePath` 落盘的
    /// 上游报文），确认各项是「同一配额的不同维度」还是「各自独立的配额」。
    fn primary_breakdown(&self) -> Option<&UsageBreakdown> {
        self.usage_breakdown_list.first()
    }

    /// 获取总使用限额
    ///
    /// 累加基础额度、激活的免费试用额度和激活的奖励额度。取 `*WithPrecision`（缺失时回退
    /// 整数字段，见 `precise_or_int`）。
    pub fn usage_limit(&self) -> f64 {
        let Some(breakdown) = self.primary_breakdown() else {
            return 0.0;
        };

        let mut total = precise_or_int(
            breakdown.usage_limit_with_precision,
            breakdown.usage_limit,
        );

        // 累加激活的 free trial 额度
        if let Some(trial) = &breakdown.free_trial_info {
            if trial.is_active() {
                total += precise_or_int(trial.usage_limit_with_precision, trial.usage_limit);
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

    /// 获取总当前使用量
    ///
    /// 累加基础使用量、激活的免费试用使用量和激活的奖励使用量。同 [`Self::usage_limit`]：
    /// 取 `*WithPrecision`，缺失时回退整数字段。
    pub fn current_usage(&self) -> f64 {
        let Some(breakdown) = self.primary_breakdown() else {
            return 0.0;
        };

        let mut total = precise_or_int(
            breakdown.current_usage_with_precision,
            breakdown.current_usage,
        );

        // 累加激活的 free trial 使用量
        if let Some(trial) = &breakdown.free_trial_info {
            if trial.is_active() {
                total += precise_or_int(
                    trial.current_usage_with_precision,
                    trial.current_usage,
                );
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

    /// 🔴 上游只给整数（缺 `*WithPrecision`）时不得把额度报成 0。
    ///
    /// 回退即 FAIL：去掉 `precise_or_int`，两个断言立刻变 0.0（面板 0 限额 +
    /// `balance_factor` 失真）。free trial 的整数对同样要回退。
    #[test]
    fn integer_only_payload_falls_back_from_precision_fields() {
        let raw = r#"{
            "usageBreakdownList": [{
                "currentUsage": 100,
                "usageLimit": 10000,
                "freeTrialInfo": {
                    "freeTrialStatus": "ACTIVE",
                    "currentUsage": 20,
                    "usageLimit": 500
                }
            }]
        }"#;
        let resp: UsageLimitsResponse = serde_json::from_str(raw).expect("解析应成功");
        assert_eq!(
            resp.usage_limit(),
            10500.0,
            "base 10000 + trial 500（都只有整数字段）"
        );
        assert_eq!(resp.current_usage(), 120.0, "base 100 + trial 20");

        // 真值本来就是 0（两边都是 0）不得被改写；精度字段有值时以精度为准。
        let zero: UsageLimitsResponse = serde_json::from_str(
            r#"{"usageBreakdownList":[{"currentUsage":0,"usageLimit":0}]}"#,
        )
        .unwrap();
        assert_eq!(zero.usage_limit(), 0.0);
        let precise: UsageLimitsResponse = serde_json::from_str(
            r#"{"usageBreakdownList":[{"usageLimit":10000,"usageLimitWithPrecision":9999.5}]}"#,
        )
        .unwrap();
        assert_eq!(precise.usage_limit(), 9999.5);
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

    /// 🔴 free trial 的状态大小写同样不定：`"Active"` / `"active"` 必须计入额度与用量。
    ///
    /// 回退即 FAIL：改回严格 `== "ACTIVE"` 后，`Active` / `active` 两轮会少算 trial 的
    /// 500 额度与 20 用量（面板额度偏低，与修过的 bonus 同款）。
    #[test]
    fn free_trial_status_case_insensitive_counts_toward_limit() {
        for status in ["ACTIVE", "Active", "active"] {
            let raw = format!(
                r#"{{
                    "usageBreakdownList": [{{
                        "currentUsage": 100,
                        "currentUsageWithPrecision": 100.0,
                        "usageLimit": 10000,
                        "usageLimitWithPrecision": 10000.0,
                        "freeTrialInfo": {{
                            "currentUsage": 20,
                            "currentUsageWithPrecision": 20.0,
                            "usageLimit": 500,
                            "usageLimitWithPrecision": 500.0,
                            "freeTrialStatus": "{status}"
                        }}
                    }}]
                }}"#
            );
            let resp: UsageLimitsResponse = serde_json::from_str(&raw).expect("解析应成功");
            assert_eq!(
                resp.usage_limit(),
                10500.0,
                "status={status} 时 free trial 额度应计入（base 10000 + trial 500）"
            );
            assert_eq!(
                resp.current_usage(),
                120.0,
                "status={status} 时 free trial 用量应计入（base 100 + trial 20）"
            );
        }

        // 对照组：非 ACTIVE 的 trial 不计入。
        let raw = r#"{
            "usageBreakdownList": [{
                "currentUsage": 100,
                "currentUsageWithPrecision": 100.0,
                "usageLimit": 10000,
                "usageLimitWithPrecision": 10000.0,
                "freeTrialInfo": {
                    "currentUsage": 20,
                    "currentUsageWithPrecision": 20.0,
                    "usageLimit": 500,
                    "usageLimitWithPrecision": 500.0,
                    "freeTrialStatus": "EXPIRED"
                }
            }]
        }"#;
        let resp: UsageLimitsResponse = serde_json::from_str(raw).expect("解析应成功");
        assert_eq!(resp.usage_limit(), 10000.0, "EXPIRED trial 不得计入");
        assert_eq!(resp.current_usage(), 100.0);
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
