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

/// 在**已小写化**的 body 里找一个 OIDC 错误码，要求其后是词边界。
///
/// 为什么不用裸 `contains`：`invalid_grant` 是 `invalid_grant_type` 的前缀、
/// `invalid_client` 是 `invalid_client_metadata` 的前缀 —— 裸子串会把「另一种错误」
/// 判成前者（错归因）。判据必须落在**完整码**上。
fn body_has_code(lower_body: &str, code: &str) -> bool {
    let mut from = 0usize;
    while let Some(rel) = lower_body[from..].find(code) {
        let end = from + rel + code.len();
        // 码后一个字符必须是「非标识符字符」（`"`、`,`、`}`、空白、串尾皆可）。
        let boundary_ok = lower_body[end..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
        if boundary_ok {
            return true;
        }
        from = end;
    }
    false
}

/// 诊断 token 刷新（oidc /token）的非 2xx 响应。收录本轮实测坐实的模式。
///
/// `status` = 上游 HTTP 状态码；`body` = 上游响应体原文。
pub fn diagnose_refresh(status: u16, body: &str) -> OnboardingDiagnosis {
    // 折叠给用户看的 `raw` 走**与 trace 落盘同一条管道**（先截断到 2KiB 再脱敏）：
    // OIDC 错误体可能回显 refreshToken / clientSecret（`error_description` 里带请求片段），
    // 而本结构体既 `Serialize`（随 AdminErrorResponse 进面板）又 `Debug`，
    // 原文直存等于把凭据放进客户端可见面 + 日志面，且没有长度上界。
    // ⚠️ 判据一律读**原文** `body`，脱敏只作用于展示字段。
    let raw = Some(format!(
        "{} {}",
        status,
        crate::kiro::upstream_trace::sanitize_body(body)
    ));
    // 匹配一律大小写不敏感：错误码/描述的大小写不在上游契约里（`INVALID_GRANT` 同样合法），
    // 按原样区分大小写会把标准错误甩进兜底的 UNKNOWN 分支（错归因 + 引导失真）。
    let lower = body.to_ascii_lowercase();
    // ① refresh_token 永久失效：invalid_grant。
    //    `/token` 端点上的 grant 就是 refresh_token（本仓只发 refresh_token 授权），
    //    故标准码 invalid_grant 即「这个 refresh_token 不能再用」。
    //    此前额外要求描述里出现 "Invalid refresh token" 那句英文 ⇒ 标准形态
    //    `{"error":"invalid_grant","error_description":"The provided authorization grant is invalid"}`
    //    匹配不上，落到兜底 UNKNOWN_REFRESH_ERROR（Gateway、不可重试），
    //    用户失去「重新上号」这条唯一有效引导。
    if status == 400 && body_has_code(&lower, "invalid_grant") {
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
    // ② client 凭证与 token 不匹配 / client 注册过期。
    //    SSO-OIDC 的 client 注册有有效期（~90 天）；或 clientId/secret 与发 refresh_token 的 client 不是同一个。
    //    两支判据：
    //      - 标准码 `invalid_client`（OIDC 用它表示 client 认证失败）。此前它被写在
    //        「先出现 invalid_request」的条件里 ⇒ `{"error":"invalid_client"}` 这种标准体
    //        结构上永远匹配不到（那种体里没有 invalid_request），直接落兜底。
    //      - #98 实测：invalid_request + "Invalid token provided"。
    //    ⚠️ 裸子串 `client` 已删除：它会把「缺 client_id / 参数校验」这类**网关侧请求构造
    //    问题**也判成账号态（Fault::AccountState + 引导重新上号），与本模块铁律
    //    「只有能确证归因才归因」冲突。宁可落 UNKNOWN（Gateway + 提示反馈）也不假归因。
    if status == 400
        && (body_has_code(&lower, "invalid_client")
            || (body_has_code(&lower, "invalid_request") && lower.contains("invalid token")))
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
///
/// `last_err` = 最后一个 region 的上游错误串（`register_client@<region>: ...` /
/// `device_authorization@<region>: ...`，见 `auth/idc.rs` 的探测循环）。
///
/// ⚠️ **必须先看 `last_err` 再归因**：全 region 失败**不等于** start URL 错 ——
/// 网络不通 / 代理配错 / AWS 5xx 会让每个 region 同样失败（探测循环对这类错误是
/// 「记 last_err 继续试下一个」）。把它们说成「你的 start URL 拼错了」会让用户去改一个
/// 没问题的旋钮，正是本模块铁律（只有能确证归因才归因）要拦的形态。
/// 只有确认「上游按实例判否」（400/404，探测主判据）时才归 [`Fault::UserInput`]。
pub fn diagnose_device_auth_all_failed(
    tried_regions: usize,
    last_err: Option<&str>,
) -> OnboardingDiagnosis {
    let raw = last_err.map(|s| s.to_string());
    if let Some(err) = last_err {
        // 5xx：`reqwest::StatusCode` 的 Display 带短语（`503 Service Unavailable`），
        // 故按「空格 + 5 + 两位数字 + 词边界」找状态码，而不是匹配裸 "503"
        // （词边界这条挡的是 `took 500ms` 那类耗时量纲，见 [`has_http_5xx_status`]）。
        if has_http_5xx_status(err) {
            return OnboardingDiagnosis::new(
                Stage::DeviceAuth,
                Fault::Upstream,
                "UPSTREAM_5XX",
                format!(
                    "所试的 {} 个 region 的 IdC 端点均返回上游 5xx（AWS 侧故障），与 start URL 无关。",
                    tried_regions
                ),
                &[
                    "稍后重试；这是 AWS 侧临时故障，不是账号或网关问题。",
                    "若持续多小时，可在 AWS Health Dashboard 确认区域故障。",
                ],
                raw,
                true,
            );
        }
        // 传输层失败（DNS/TCP/TLS/代理）：请求根本没到 AWS，归因给网络。
        // 判据取**具体**形态而非裸 `connect`（后者会命中任意描述里的同词）——
        // 见 [`has_transport_marker`]。
        if has_transport_marker(err) {
            return OnboardingDiagnosis::new(
                Stage::DeviceAuth,
                Fault::Transient,
                "NETWORK_ERROR",
                format!(
                    "所试的 {} 个 region 都因网络/代理问题失败（请求未到达 AWS），与 start URL 无关。",
                    tried_regions
                ),
                &[
                    "检查本机网络与代理配置（网关的 proxy 设置 / 容器出网）。",
                    "确认 oidc.<region>.amazonaws.com 可解析、可连通后重试。",
                ],
                raw,
                true,
            );
        }
    }
    // 其余（含 400/404「该 region 无此实例」与无法识别的形态）：维持原有归因。
    // 未识别形态**不**改判 Gateway：本探测的失败主判据就是 400 invalid_request，
    // 而用户侧引导（查 IAM Identity Center 的实例 region）在那条路径上是唯一可操作项。
    OnboardingDiagnosis::new(
        Stage::DeviceAuth,
        Fault::UserInput,
        "REGION_MISMATCH",
        format!(
            "start URL 在所试的 {} 个 region 均无对应 IdC 实例。",
            tried_regions
        ),
        &[
            "确认 start URL 拼写正确（形如 https://d-xxxxxxxxxx.awsapps.com/start）。",
            "到 AWS IAM Identity Center 控制台 → 设置，查看实例所在 region，在上号时手动填该 region。",
            "IdC 实例绑定单一 region，start URL 本身不含 region 信息，故需确认后手填。",
        ],
        raw,
        false,
    )
}

/// 错误串里是否出现 HTTP 5xx 状态码（形如 ` 503` / ` 502 Service Unavailable`）。
///
/// 判据：前导空格（上游错误串的固定形态是 `<动作> 失败 <code> <phrase>`）+ `5` + 两位数字
/// + **词边界**（后一位不是 ASCII 字母/数字；串尾视为边界）。
///
/// 后一位为什么连**字母**也要排除：三位数后面紧跟字母，说明它是更长的词/量纲的一部分，
/// 不是状态码 —— `took 500ms`（reqwest 的超时文案）正是这种形态。它一旦被当成 503，
/// 「网络超时」就被误归成「上游 5xx」：判成可重试、告诉用户与 start URL 无关、
/// 引他去看 AWS 健康状态，而真正要查的是本机网络/代理。数字同理：`5123 ms`、端口 `5000`
/// 都不是三位状态码。
fn has_http_5xx_status(err: &str) -> bool {
    let b = err.as_bytes();
    (0..b.len().saturating_sub(3)).any(|i| {
        b[i] == b' '
            && b[i + 1] == b'5'
            && b[i + 2].is_ascii_digit()
            && b[i + 3].is_ascii_digit()
            // 词边界：后一位不得是数字或 ASCII 字母（`is_ascii_alphanumeric` = 二者之和）。
            && !b.get(i + 4).is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

/// 传输层失败标记（reqwest `Display` 的实测形态 + 本仓自造前缀）。
///
/// 只收**具体**串：`connect` 这类裸词会命中任意描述里的同词，把真正的上游业务错误
/// 误判成网络问题（反方向的错归因，同样是错）。
fn has_transport_marker(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    [
        "error sending request", // reqwest 连接层包装（DNS/TCP/TLS/代理皆走它）
        "error trying to connect",
        "connection refused",
        "connection reset",
        "connection aborted",
        "connection closed",
        "timed out",
        "timeout",
        "dns error",
        "failed to lookup address",
        "no such host",
        "certificate", // 证书链/时间/主机名不符
        "proxy",
    ]
    .iter()
    .any(|m| lower.contains(m))
}

/// 诊断 ListAvailableProfiles 在某 region 返回空 profile 列表（该 region 未开通）。
/// `probed_region` = 探测的 region；`found_regions` = 已确认有 profile 的 region 列表（可空）。
pub fn diagnose_no_profile_in_region(
    probed_region: &str,
    found_regions: &[String],
) -> OnboardingDiagnosis {
    let found_hint = if found_regions.is_empty() {
        "该账号目前在所有已探测 region 都没有可用 profile。".to_string()
    } else {
        format!(
            "该账号已开通 profile 的 region：{}。",
            found_regions.join("、")
        )
    };
    // 引导**按事实构造**：`found_regions` 为空时根本不存在「上面列出的已开通 region」，
    // 无条件的第三句会把用户指到一个不存在的列表（引导自相矛盾 = 比没有引导更糟）。
    let mut guidance: Vec<&str> = vec![
        "若刚加入订阅组，profile 传播最多需 24 小时，请稍后再探测。",
        "确认该账号确实在此 region 开通了 Kiro/CodeWhisperer 订阅。",
    ];
    if found_regions.is_empty() {
        guidance.push(
            "若已确认该账号从未开通任何 Kiro/CodeWhisperer 订阅，需先在 AWS 侧开通该订阅后再探测。",
        );
    } else {
        guidance.push("可切换到上面列出的已开通 region 使用。");
    }
    OnboardingDiagnosis::new(
        Stage::ProbeRegion,
        Fault::AccountState,
        "NO_PROFILE_IN_REGION",
        format!(
            "该账号在 {} 未开通 Kiro profile。{}",
            probed_region, found_hint
        ),
        &guidance,
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
        assert!(
            d.guidance.iter().any(|g| g.contains("重新上号")),
            "引导应含重新上号"
        );
        assert!(
            d.raw.as_deref().unwrap().contains("Invalid token provided"),
            "原文折叠保留"
        );
    }

    #[test]
    fn test_diagnose_refresh_invalid_grant_permanent() {
        let body =
            r#"{"error":"invalid_grant","error_description":"Invalid refresh token provided"}"#;
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
        // 引导不得指向不存在的「上面列出的 region」列表
        assert!(
            !d.guidance.iter().any(|g| g.contains("上面列出")),
            "found_regions 为空时不能出现指向空列表的引导: {:?}",
            d.guidance
        );
        // 非空时该引导必须在（否则用户不知道可以换区）
        let d2 = diagnose_no_profile_in_region("ap-south-1", &["us-east-1".to_string()]);
        assert!(d2.guidance.iter().any(|g| g.contains("上面列出")));
    }

    /// 标准 OIDC 形态：invalid_grant 且描述**不含** "Invalid refresh token" 那句英文。
    /// 旧判据要求两者同时出现 ⇒ 该体落兜底 UNKNOWN（错归因 + 丢「重新上号」引导）。
    #[test]
    fn test_diagnose_refresh_invalid_grant_without_phrase() {
        let d = diagnose_refresh(
            400,
            r#"{"error":"invalid_grant","error_description":"The provided authorization grant is invalid"}"#,
        );
        assert_eq!(d.code, "REFRESH_TOKEN_INVALID");
        assert_eq!(d.fault, Fault::AccountState);
        assert!(!d.retriable);
    }

    /// 标准 `invalid_client` 体：旧判据要求先有 "invalid_request" ⇒ 该支结构性不可达。
    #[test]
    fn test_diagnose_refresh_invalid_client_is_reachable() {
        let d = diagnose_refresh(
            400,
            r#"{"error":"invalid_client","error_description":"Invalid client credentials"}"#,
        );
        assert_eq!(d.code, "CLIENT_OR_TOKEN_MISMATCH");
        assert_eq!(d.fault, Fault::AccountState);
    }

    /// 大小写变体不得改变归因（上游错误码大小写不在契约里）。
    #[test]
    fn test_diagnose_refresh_matching_is_case_insensitive() {
        let d = diagnose_refresh(400, r#"{"ERROR":"INVALID_GRANT"}"#);
        assert_eq!(d.code, "REFRESH_TOKEN_INVALID");
    }

    /// 裸子串 `client` 已删：`invalid_request` 里只是**提到** client（缺参数等网关侧
    /// 构造问题）不得被判成账号态 + 引导重新上号（假归因）。
    #[test]
    fn test_diagnose_refresh_invalid_request_mentioning_client_is_not_account_state() {
        let d = diagnose_refresh(
            400,
            r#"{"error":"invalid_request","error_description":"Missing client_id parameter"}"#,
        );
        assert_eq!(d.code, "UNKNOWN_REFRESH_ERROR");
        assert_eq!(d.fault, Fault::Gateway, "拿不准必须诚实标 Gateway");
        assert!(!d.retriable);
    }

    /// 词边界：`invalid_grant_type` 不是 `invalid_grant`（前缀误命中 = 错归因）。
    #[test]
    fn test_diagnose_refresh_code_prefix_does_not_match() {
        let d = diagnose_refresh(
            400,
            r#"{"error":"invalid_grant_type","error_description":"unsupported"}"#,
        );
        assert_eq!(d.code, "UNKNOWN_REFRESH_ERROR");
    }

    /// `raw` 是客户端可见（AdminErrorResponse）且 `Debug` 可打印的字段：
    /// 必须已脱敏且有界（OIDC 错误体可能回显 refreshToken / clientSecret）。
    #[test]
    fn test_diagnose_refresh_raw_is_redacted_and_bounded() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ4In0.signature";
        let body = format!(
            r#"{{"error":"invalid_request","error_description":"Invalid token provided","echo":"Bearer {jwt}"}}"#
        );
        let d = diagnose_refresh(400, &body);
        let raw = d.raw.as_deref().unwrap();
        assert!(!raw.contains(jwt), "token 不得原文进 raw: {raw}");
        assert!(raw.contains("[REDACTED]"), "应打码而不是整段丢弃: {raw}");

        // 超大 body 必须有界（旧实现原样存下整个上游响应体）
        let huge = format!(
            "{}<html>{}</html>",
            "x".repeat(crate::kiro::upstream_trace::BODY_MAX_BYTES + 4096),
            "y".repeat(1024)
        );
        let d = diagnose_refresh(502, &huge);
        let raw = d.raw.as_deref().unwrap();
        assert!(
            raw.len() <= crate::kiro::upstream_trace::BODY_MAX_BYTES + 64,
            "raw 必须被截断（{} 字节）",
            raw.len()
        );
    }

    /// 全 region 失败但根因是上游 5xx / 传输层：不得说成「start URL 拼错了」。
    #[test]
    fn test_diagnose_device_auth_5xx_is_upstream_not_user_input() {
        let d = diagnose_device_auth_all_failed(
            6,
            Some("device_authorization@us-east-1: Device authorization 失败 503 Service Unavailable: <html>oops</html>"),
        );
        assert_eq!(d.code, "UPSTREAM_5XX");
        assert_eq!(d.fault, Fault::Upstream);
        assert!(d.retriable, "上游 5xx 可重试");
    }

    #[test]
    fn test_diagnose_device_auth_transport_is_transient_not_user_input() {
        let d = diagnose_device_auth_all_failed(
            6,
            Some(
                "register_client@us-east-1: error sending request for url (https://oidc.us-east-1.amazonaws.com/client/register): error trying to connect: Connection refused (os error 111)",
            ),
        );
        assert_eq!(d.code, "NETWORK_ERROR");
        assert_eq!(d.fault, Fault::Transient);
        assert!(d.retriable);
    }

    /// 主判据（每区 400 = 该区无此实例）仍归 UserInput —— 这是本探测的常态结局，
    /// 上面两条分支不得把它吃掉。
    #[test]
    fn test_diagnose_device_auth_400_keeps_region_mismatch() {
        let d = diagnose_device_auth_all_failed(
            6,
            Some(
                "device_authorization@eu-west-1: Device authorization 失败 400 Bad Request: {\"error\":\"invalid_request\"}",
            ),
        );
        assert_eq!(d.code, "REGION_MISMATCH");
        assert_eq!(d.fault, Fault::UserInput);
        // 无法识别的形态同样保持原归因（见函数文档的取舍说明）
        let d2 = diagnose_device_auth_all_failed(6, None);
        assert_eq!(d2.code, "REGION_MISMATCH");
    }

    /// 5xx 状态码判据不得被别的数字（端口/耗时）误触发。
    #[test]
    fn test_has_http_5xx_status_boundaries() {
        assert!(has_http_5xx_status("x 失败 500 Internal Server Error"));
        assert!(has_http_5xx_status("x 失败 599: something"));
        assert!(!has_http_5xx_status("x 失败 400 Bad Request"));
        assert!(!has_http_5xx_status("took 5123 ms to connect"));
        assert!(!has_http_5xx_status("端口 5000 被占用"));
        assert!(!has_http_5xx_status(""));
        // 后一位是**字母**：`500ms` 是耗时量纲，不是三位状态码（词边界必须含字母）。
        // 旧判据只看「后一位不是数字」⇒ 这里会被判成 5xx ⇒ 网络超时误归成上游故障。
        assert!(!has_http_5xx_status("took 500ms to connect"));
        assert!(!has_http_5xx_status("took 502ms"));
        // 串尾与非字母数字分隔符仍是合法词边界：`500` 后无字符、`502` 后是空格/冒号/括号。
        assert!(has_http_5xx_status("x 失败 500"));
        assert!(has_http_5xx_status("x 失败 502 (Bad Gateway)"));
    }

    /// ⭐ 回归（Ver2 细节）：`took 500ms` 不得被判成 503 —— 网络超时不能归因给上游。
    ///
    /// 旧判据「前导空格 + 5 + 两位数字 + 后一位非数字」会让 `500ms` 里的 `500` 命中 5xx，
    /// 于是 reqwest 的超时文案被归成 [`Fault::Upstream`]：用户被告知「与 start URL 无关，
    /// 等 AWS 恢复」，而真正该查的是本机网络/代理（本模块铁律：只有能确证归因才归因）。
    ///
    /// 回退即 FAIL：把 `is_ascii_alphanumeric` 改回 `is_ascii_digit`。
    #[test]
    fn test_diagnose_device_auth_500ms_is_not_upstream_5xx() {
        let d = diagnose_device_auth_all_failed(
            6,
            Some(
                "register_client@us-east-1: error sending request for url (https://oidc.us-east-1.amazonaws.com/client/register): operation timed out after took 500ms to connect",
            ),
        );
        assert_eq!(d.code, "NETWORK_ERROR", "500ms 是耗时量纲，不是 503");
        assert_eq!(d.fault, Fault::Transient);
        assert!(d.retriable, "网络问题可重试（但不该说成 AWS 侧故障）");
    }

    #[test]
    fn test_serde_roundtrip() {
        // 前端要拿到结构化诊断，序列化必须完整。
        let d = diagnose_refresh(
            400,
            r#"{"error":"invalid_request","error_description":"Invalid token provided"}"#,
        );
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("\"code\":\"CLIENT_OR_TOKEN_MISMATCH\""));
        assert!(json.contains("\"fault\":\"account_state\""));
        assert!(json.contains("\"guidance\""));
        let back: OnboardingDiagnosis = serde_json::from_str(&json).unwrap();
        assert_eq!(back.code, d.code);
    }
}
