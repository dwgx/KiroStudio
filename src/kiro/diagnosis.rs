//! 上号智能诊断（无论谁的错都给正确引导）。
//!
//! # 立意
//! 上号是本项目最关键的路径，历来反复翻车的共性不是某个 bug，而是**出错时说不清"是账号
//! 的问题还是网关的问题"，用户只看到裸 502 / 裸报错，不知道该干什么**。本模块把"出错了"
//! 升级成结构化的 **(哪一步 stage + 谁的错 fault + 具体 code + 该干什么 guidance)**。
//!
//! # 诚实边界（铁律）
//! 只有**能确证归因**的错误才归因；真未知的明确标 [`Fault::Gateway`] + `UNKNOWN` code +
//! 附原始 body，并提示反馈——连"我们没覆盖到"也是一种诚实引导，绝不假装知道原因。
//!
//! # 单一真相源
//! 所有上号/刷新/探测路径的错误都经纯函数 [`diagnose`] 产出 [`OnboardingDiagnosis`]，
//! 规则库集中在此，收录实测坐实的错误模式，避免"每条路径各写各的错误处理"。

use serde::{Deserialize, Serialize};

/// 上号流程的阶段（错误发生在哪一步）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// OIDC 客户端注册（IdC device flow step1）。
    Register,
    /// 设备授权发起（IdC device flow step2，带 startUrl）。
    DeviceAuth,
    /// 轮询换 token（IdC device flow step4）。
    Poll,
    /// 动态解析 profileArn（ListAvailableProfiles）。
    ResolveProfile,
    /// 刷新 token（oidc /token）。
    Refresh,
    /// 探测/切换 region profile。
    ProbeRegion,
    /// 验活（getUsageLimits）。
    Verify,
}

impl Stage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Stage::Register => "register",
            Stage::DeviceAuth => "device_auth",
            Stage::Poll => "poll",
            Stage::ResolveProfile => "resolve_profile",
            Stage::Refresh => "refresh",
            Stage::ProbeRegion => "probe_region",
            Stage::Verify => "verify",
        }
    }
}

/// 错误归因方（"是谁的错"——这是引导的核心）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fault {
    /// 用户输入问题（填错 region / start URL 等）——可由用户改正。
    UserInput,
    /// 账号本身状态问题（token 失效 / profile 未开通 / client 过期）——需重新上号或等开通。
    AccountState,
    /// 上游 AWS 侧问题（5xx / 服务不可用）——非我方、非用户，稍后重试。
    Upstream,
    /// 网关（我们）的问题——未覆盖的情况 / bug，需反馈修复。
    Gateway,
    /// 瞬时问题（网络抖动 / 超时）——重试通常即好。
    Transient,
}

impl Fault {
    pub fn as_str(&self) -> &'static str {
        match self {
            Fault::UserInput => "user_input",
            Fault::AccountState => "account_state",
            Fault::Upstream => "upstream",
            Fault::Gateway => "gateway",
            Fault::Transient => "transient",
        }
    }
}

/// 结构化上号诊断：贯穿所有上号/刷新/探测路径，取代裸字符串错误。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnboardingDiagnosis {
    /// 出错的阶段。
    pub stage: Stage,
    /// 归因方（谁的错）。
    pub fault: Fault,
    /// 机器可读错误码（如 `REGION_MISMATCH` / `CLIENT_OR_TOKEN_MISMATCH`）。
    pub code: String,
    /// 一句话中文诊断（给用户看的主行）。
    pub summary: String,
    /// 用户该做什么——有序、可操作的步骤。
    pub guidance: Vec<String>,
    /// 原始上游 status+body（折叠详情，供排障，不进主视线）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// 能否重试（前端据此决定是否给「重试」按钮）。
    pub retriable: bool,
}

impl OnboardingDiagnosis {
    /// 便于日志/错误链传递的单行摘要（含 code + summary，raw 不进以免刷屏）。
    pub fn log_line(&self) -> String {
        format!(
            "[诊断] stage={} fault={} code={} — {}",
            self.stage.as_str(),
            self.fault.as_str(),
            self.code,
            self.summary
        )
    }

    fn new(
        stage: Stage,
        fault: Fault,
        code: &str,
        summary: impl Into<String>,
        guidance: &[&str],
        raw: Option<String>,
        retriable: bool,
    ) -> Self {
        OnboardingDiagnosis {
            stage,
            fault,
            code: code.to_string(),
            summary: summary.into(),
            guidance: guidance.iter().map(|s| s.to_string()).collect(),
            raw,
            retriable,
        }
    }
}

/// 诊断 token 刷新（oidc /token）的非 2xx 响应。收录本轮实测坐实的模式。
///
/// `status` = 上游 HTTP 状态码；`body` = 上游响应体原文。
pub fn diagnose_refresh(status: u16, body: &str) -> OnboardingDiagnosis {
    let raw = Some(format!("{} {}", status, body));
    // ① refresh_token 永久失效：invalid_grant + Invalid refresh token provided。
    if status == 400
        && body.contains("\"invalid_grant\"")
        && body.contains("Invalid refresh token")
    {
        return OnboardingDiagnosis::new(
            Stage::Refresh,
            Fault::AccountState,
            "REFRESH_TOKEN_INVALID",
            "refresh_token 已失效（被吊销或过期），无法自动刷新。",
            &[
                "点该凭据的『重新上号』重新走一次授权即可恢复。",
                "重新上号前对话/余额若仍能用，是 access_token 还在有效期内，过期后会中断。",
            ],
            raw,
            false,
        );
    }
    // ② client 凭证与 token 不匹配 / client 注册过期（#98 实测：invalid_request + Invalid token provided）。
    //    SSO-OIDC 的 client 注册有有效期（~90 天）；或 clientId/secret 与发 refresh_token 的 client 不是同一个。
    if status == 400
        && body.contains("\"invalid_request\"")
        && (body.contains("Invalid token") || body.contains("invalid_client") || body.contains("client"))
    {
        return OnboardingDiagnosis::new(
            Stage::Refresh,
            Fault::AccountState,
            "CLIENT_OR_TOKEN_MISMATCH",
            "OIDC client 凭证与 token 不匹配，或 client 注册已过期（约 90 天有效期）。",
            &[
                "点该凭据的『重新上号』重新注册 client 并授权即可恢复。",
                "此时 access_token 通常仍在有效期内，对话/余额不受影响，但过期后会中断——建议尽快重新上号。",
            ],
            raw,
            false,
        );
    }
    // ③ 认证失效（401）。
    if status == 401 {
        return OnboardingDiagnosis::new(
            Stage::Refresh,
            Fault::AccountState,
            "AUTH_EXPIRED",
            "凭证已过期或无效，需要重新认证。",
            &["点『重新上号』重新授权。"],
            raw,
            false,
        );
    }
    // ④ 限流（429）——瞬时，可重试。
    if status == 429 {
        return OnboardingDiagnosis::new(
            Stage::Refresh,
            Fault::Transient,
            "RATE_LIMITED",
            "刷新请求被上游限流。",
            &["稍等片刻后重试；频繁刷新会触发限流。"],
            raw,
            true,
        );
    }
    // ⑤ 上游 5xx——AWS 侧，可重试。
    if (500..=599).contains(&status) {
        return OnboardingDiagnosis::new(
            Stage::Refresh,
            Fault::Upstream,
            "UPSTREAM_5XX",
            "AWS OIDC 服务暂时不可用（上游 5xx）。",
            &["稍后重试；这是 AWS 侧临时故障，不是账号或网关问题。"],
            raw,
            true,
        );
    }
    // ⑥ 未识别——诚实标 Gateway/UNKNOWN，附原文，提示反馈。
    OnboardingDiagnosis::new(
        Stage::Refresh,
        Fault::Gateway,
        "UNKNOWN_REFRESH_ERROR",
        "刷新失败，出现网关尚未覆盖的错误。",
        &[
            "查看下方原始信息；这可能是网关未识别的情况，请反馈以便修复。",
            "可先尝试『重新上号』，多数刷新问题重新授权后即可恢复。",
        ],
        raw,
        false,
    )
}

/// 诊断 IdC device flow 全 region 探测失败（start URL 在所有候选 region 都无对应实例）。
pub fn diagnose_device_auth_all_failed(tried_regions: usize, last_err: Option<&str>) -> OnboardingDiagnosis {
    OnboardingDiagnosis::new(
        Stage::DeviceAuth,
        Fault::UserInput,
        "REGION_MISMATCH",
        format!("start URL 在所试的 {} 个 region 均无对应 IdC 实例。", tried_regions),
        &[
            "确认 start URL 拼写正确（形如 https://d-xxxxxxxxxx.awsapps.com/start）。",
            "到 AWS IAM Identity Center 控制台 → 设置，查看实例所在 region，在上号时手动填该 region。",
            "IdC 实例绑定单一 region，start URL 本身不含 region 信息，故需确认后手填。",
        ],
        last_err.map(|s| s.to_string()),
        false,
    )
}

/// 诊断 ListAvailableProfiles 在某 region 返回空 profile 列表（该 region 未开通）。
/// `probed_region` = 探测的 region；`found_regions` = 已确认有 profile 的 region 列表（可空）。
pub fn diagnose_no_profile_in_region(probed_region: &str, found_regions: &[String]) -> OnboardingDiagnosis {
    let found_hint = if found_regions.is_empty() {
        "该账号目前在所有已探测 region 都没有可用 profile。".to_string()
    } else {
        format!("该账号已开通 profile 的 region：{}。", found_regions.join("、"))
    };
    OnboardingDiagnosis::new(
        Stage::ProbeRegion,
        Fault::AccountState,
        "NO_PROFILE_IN_REGION",
        format!("该账号在 {} 未开通 Kiro profile。{}", probed_region, found_hint),
        &[
            "若刚加入订阅组，profile 传播最多需 24 小时，请稍后再探测。",
            "确认该账号确实在此 region 开通了 Kiro/CodeWhisperer 订阅。",
            "可切换到上面列出的已开通 region 使用。",
        ],
        None,
        true,
    )
}

/// 诊断 getUsageLimits 403 FEATURE_NOT_SUPPORTED（该 region profile 未开通/未激活）。
pub fn diagnose_feature_not_supported(region: &str) -> OnboardingDiagnosis {
    OnboardingDiagnosis::new(
        Stage::Verify,
        Fault::AccountState,
        "FEATURE_NOT_SUPPORTED",
        format!("{} 的 profile 未开通该功能（FEATURE_NOT_SUPPORTED）。", region),
        &[
            "网关会在刷新时自动重探并纠正到可用 region。",
            "或在凭据设置里手动切换到已开通的 region。",
        ],
        None,
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diagnose_refresh_98_real_body_client_mismatch() {
        // #98 实测真实 body（0714 用真 token 抓的）：invalid_request / Invalid token provided。
        // 此前落兜底裸 502，现在应精准归 CLIENT_OR_TOKEN_MISMATCH + 引导重新上号。
        let body = r#"{"error":"invalid_request","error_description":"Invalid token provided","reason":null}"#;
        let d = diagnose_refresh(400, body);
        assert_eq!(d.code, "CLIENT_OR_TOKEN_MISMATCH");
        assert_eq!(d.fault, Fault::AccountState);
        assert_eq!(d.stage, Stage::Refresh);
        assert!(!d.retriable, "client 过期不是重试能解决的");
        assert!(!d.guidance.is_empty(), "必须给用户可操作引导");
        assert!(d.guidance.iter().any(|g| g.contains("重新上号")), "引导应含重新上号");
        assert!(d.raw.as_deref().unwrap().contains("Invalid token provided"), "原文折叠保留");
    }

    #[test]
    fn test_diagnose_refresh_invalid_grant_permanent() {
        let body = r#"{"error":"invalid_grant","error_description":"Invalid refresh token provided"}"#;
        let d = diagnose_refresh(400, body);
        assert_eq!(d.code, "REFRESH_TOKEN_INVALID");
        assert_eq!(d.fault, Fault::AccountState);
        assert!(!d.retriable);
    }

    #[test]
    fn test_diagnose_refresh_5xx_upstream_retriable() {
        let d = diagnose_refresh(503, "service unavailable");
        assert_eq!(d.code, "UPSTREAM_5XX");
        assert_eq!(d.fault, Fault::Upstream);
        assert!(d.retriable, "上游 5xx 可重试");
    }

    #[test]
    fn test_diagnose_refresh_429_transient() {
        let d = diagnose_refresh(429, "too many requests");
        assert_eq!(d.fault, Fault::Transient);
        assert!(d.retriable);
    }

    #[test]
    fn test_diagnose_refresh_unknown_is_honest_gateway() {
        // 未识别错误：诚实标 Gateway/UNKNOWN，不假装知道原因，附原文。
        let d = diagnose_refresh(418, "i am a teapot");
        assert_eq!(d.code, "UNKNOWN_REFRESH_ERROR");
        assert_eq!(d.fault, Fault::Gateway);
        assert!(d.raw.as_deref().unwrap().contains("teapot"));
    }

    #[test]
    fn test_diagnose_device_auth_region_mismatch() {
        let d = diagnose_device_auth_all_failed(6, Some("device_authorization@eu-west-1: 400"));
        assert_eq!(d.code, "REGION_MISMATCH");
        assert_eq!(d.fault, Fault::UserInput);
        assert!(d.guidance.iter().any(|g| g.contains("IAM Identity Center")));
    }

    #[test]
    fn test_diagnose_no_profile_in_region_lists_found() {
        // #98 实测：eu-central-1 返回 profiles:[]，us-east-1 有。应告知已开通的是 us-east-1。
        let found = vec!["us-east-1".to_string()];
        let d = diagnose_no_profile_in_region("eu-central-1", &found);
        assert_eq!(d.code, "NO_PROFILE_IN_REGION");
        assert_eq!(d.fault, Fault::AccountState);
        assert!(d.summary.contains("eu-central-1"));
        assert!(d.summary.contains("us-east-1"), "应告知已开通的 region");
        assert!(d.retriable, "可能传播中,允许稍后重探");
    }

    #[test]
    fn test_diagnose_no_profile_empty_found() {
        let d = diagnose_no_profile_in_region("ap-south-1", &[]);
        assert!(d.summary.contains("所有已探测 region"));
    }

    #[test]
    fn test_serde_roundtrip() {
        // 前端要拿到结构化诊断，序列化必须完整。
        let d = diagnose_refresh(400, r#"{"error":"invalid_request","error_description":"Invalid token provided"}"#);
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("\"code\":\"CLIENT_OR_TOKEN_MISMATCH\""));
        assert!(json.contains("\"fault\":\"account_state\""));
        assert!(json.contains("\"guidance\""));
        let back: OnboardingDiagnosis = serde_json::from_str(&json).unwrap();
        assert_eq!(back.code, d.code);
    }
}
