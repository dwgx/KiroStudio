//! 请求记录数据契约
//!
//! 一次 API 请求生命周期的最终结算快照。字段设计参考 cc-switch 的 `RequestLog`
//! （farion1231/cc-switch），裁剪为 Kiro 单上游场景所需。

use serde::{Deserialize, Serialize};

/// 请求结果分类
///
/// 对齐 provider 的失败处置分类（见 [`crate::kiro::cooldown::CooldownReason`]），
/// 便于统计侧按结果聚合健康度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestOutcome {
    /// 成功
    Success,
    /// 上游限流（429）
    RateLimited,
    /// 认证失败（401/403）
    AuthFailed,
    /// 额度用尽（402 MONTHLY_REQUEST_COUNT）
    QuotaExhausted,
    /// 账户被暂停/封禁
    AccountSuspended,
    /// 上游服务器错误（5xx）
    ServerError,
    /// 请求错误（400 等客户端错误）
    BadRequest,
    /// 网络/连接错误（未拿到响应）
    NetworkError,
    /// 其它/未分类失败
    OtherError,
}

impl RequestOutcome {
    /// 是否为成功结果
    pub fn is_success(&self) -> bool {
        matches!(self, RequestOutcome::Success)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            RequestOutcome::Success => "success",
            RequestOutcome::RateLimited => "rate_limited",
            RequestOutcome::AuthFailed => "auth_failed",
            RequestOutcome::QuotaExhausted => "quota_exhausted",
            RequestOutcome::AccountSuspended => "account_suspended",
            RequestOutcome::ServerError => "server_error",
            RequestOutcome::BadRequest => "bad_request",
            RequestOutcome::NetworkError => "network_error",
            RequestOutcome::OtherError => "other_error",
        }
    }
}

/// 单次请求的最终结算记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    /// 请求唯一 ID（用于关联 per-attempt 明细）
    pub request_id: String,
    /// 记录生成的 Unix 毫秒时间戳
    pub ts_ms: i64,
    /// 实际服务该请求的凭据 ID（失败到无凭据可用时为 None）
    pub credential_id: Option<u64>,
    /// 请求模型名
    pub model: String,
    /// 是否流式
    pub is_streaming: bool,
    /// 输入 tokens（优先精确值，回退估算）
    pub input_tokens: i32,
    /// 输出 tokens
    pub output_tokens: i32,
    /// 上游返回的真实 credit 消耗量（无 meteringEvent 时为 None）
    pub credits_used: Option<f64>,
    /// 端到端延迟（毫秒）
    pub latency_ms: u64,
    /// 首字节/首事件延迟（毫秒，流式有意义）
    pub first_token_ms: Option<u64>,
    /// 结果分类
    pub outcome: RequestOutcome,
    /// 本次经历的重试次数（0 表示首次即成功）
    pub retries: u32,
    /// 错误信息（成功时为空）
    pub error_message: Option<String>,
    /// 会话标识（conversationId，用于亲和分析）
    pub session_id: Option<String>,
    /// 请求来源设备（由入站 User-Agent 分类得到，见 [`classify_device`]）
    pub client_device: Option<String>,
    /// 客户端 IP（优先 x-forwarded-for 首段，回退 x-real-ip；拿不到为 None）
    pub client_ip: Option<String>,
    /// 客户端操作系统细分（由 UA 解析，见 [`parse_client_os`]，识别不出为 None）
    pub client_os: Option<String>,
    /// 客户端浏览器 + 版本（由 UA 解析，见 [`parse_client_browser`]，非浏览器为 None）
    pub client_browser: Option<String>,
}

impl RequestRecord {
    /// 构造一条记录，时间戳取当前时刻
    pub fn new(request_id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            ts_ms: chrono::Utc::now().timestamp_millis(),
            credential_id: None,
            model: model.into(),
            is_streaming: false,
            input_tokens: 0,
            output_tokens: 0,
            credits_used: None,
            latency_ms: 0,
            first_token_ms: None,
            outcome: RequestOutcome::Success,
            retries: 0,
            error_message: None,
            session_id: None,
            client_device: None,
            client_ip: None,
            client_os: None,
            client_browser: None,
        }
    }
}

/// 从入站 User-Agent 分类请求来源设备。
///
/// 返回规范小写取值：`claude-code` / `curl` / `windows` / `macos` / `linux` /
/// `python` / `node` / `vscode` / `browser` / `unknown`。`ua` 为 `None` 或空白
/// 时返回 `Some("unknown")`（永远给出一个可展示的值，不返回裸 `None`）。
///
/// 判定按契约优先级从上到下短路匹配：客户端标识（claude-code / curl / python /
/// node / vscode）优先于操作系统标识（Windows / macOS / Linux），最后才是通用
/// 浏览器 UA（Mozilla）兜底。
///
/// Claude Code CLI 实测入站 UA 形如 `claude-cli/2.1.201 (external, cli)`（旧版本
/// 也可能带 `claude-code`），二者统一归为 `claude-code` 类展示；`anthropic` 关键字
/// 作为官方 SDK/客户端的兜底也归入同类，避免全部落到 `unknown`。
pub fn classify_device(ua: Option<&str>) -> Option<String> {
    let raw = match ua {
        Some(s) if !s.trim().is_empty() => s,
        _ => return Some("unknown".to_string()),
    };
    let lower = raw.to_lowercase();

    // 1) 明确的客户端标识优先（这些工具的 UA 里也可能夹带 OS 信息，必须先判定）
    //    claude-cli（新版 CLI 实测值）/ claude-code（旧版）/ anthropic（官方 SDK 兜底）
    //    统一归为 "claude-code" 类，前端展示为 Claude Code。
    let kind = if lower.contains("claude-cli")
        || lower.contains("claude-code")
        || lower.contains("anthropic")
    {
        "claude-code"
    } else if lower.contains("curl") {
        "curl"
    } else if lower.contains("python-requests") || lower.contains("python") {
        "python"
    } else if lower.contains("axios") || lower.contains("node") {
        "node"
    } else if lower.contains("vscode") {
        "vscode"
    }
    // 2) 操作系统标识（多为浏览器/SDK 携带的平台信息）
    else if lower.contains("windows nt") {
        "windows"
    } else if lower.contains("macintosh") || lower.contains("mac os") {
        "macos"
    } else if lower.contains("linux") || lower.contains("x11") {
        "linux"
    }
    // 3) 通用浏览器兜底（Mozilla 且未命中上述任何一类）
    else if lower.contains("mozilla") {
        "browser"
    } else {
        "unknown"
    };

    Some(kind.to_string())
}

/// 从入站 User-Agent 细分客户端操作系统。
///
/// 命中返回规范展示名：`Windows`（"Windows NT 10.0" 既可能是 10 也可能是 11，
/// 不硬判版本）/ `macOS` / `iOS` / `Android` / `Linux`。识别不出或 `ua` 为
/// `None`/空白时返回 `None`（与 `classify_device` 不同，这里不做 unknown 兜底，
/// 让「解析不出」如实为空）。
///
/// 判定顺序：移动端（iOS/Android）优先于桌面端，避免 iPad 的 "Mac OS X" 误判为
/// macOS、Android 的 "Linux" 误判为 Linux。
pub fn parse_client_os(ua: Option<&str>) -> Option<String> {
    let raw = match ua {
        Some(s) if !s.trim().is_empty() => s,
        _ => return None,
    };
    let lower = raw.to_lowercase();

    // 1) 移动端优先：iPad 的 UA 含 "Mac OS X"，Android 的 UA 含 "Linux"，
    //    必须在桌面端判定之前短路，否则会被误分类。
    if lower.contains("iphone") || lower.contains("ipad") || lower.contains("ipod")
        || lower.contains("ios")
    {
        return Some("iOS".to_string());
    }
    if lower.contains("android") {
        return Some("Android".to_string());
    }

    // 2) 桌面端
    if lower.contains("windows nt") || lower.contains("windows") {
        // "Windows NT 10.0" 对应 Win10/Win11 二者，无法从 UA 精确区分，统一记为 Windows
        Some("Windows".to_string())
    } else if lower.contains("mac os") || lower.contains("macintosh") {
        Some("macOS".to_string())
    } else if lower.contains("linux") || lower.contains("x11") {
        Some("Linux".to_string())
    } else {
        None
    }
}

/// 从入站 User-Agent 解析浏览器 + 主版本号。
///
/// 命中返回 `Chrome 120` / `Edge 120` / `Firefox` / `Safari` 形式（Chrome/Edge
/// 带主版本号，Firefox/Safari 仅名称）。curl/python/node 等非浏览器客户端、
/// 识别不出或空 UA 返回 `None`。
///
/// 判定顺序处理浏览器 UA 互相夹带的问题：Edge 的 UA 同时含 "Edg/" 与
/// "Chrome/"，必须先判 Edge；Chrome 的 UA 含 "Safari/"，故 Safari 需排除 Chrome。
pub fn parse_client_browser(ua: Option<&str>) -> Option<String> {
    let raw = match ua {
        Some(s) if !s.trim().is_empty() => s,
        _ => return None,
    };
    let lower = raw.to_lowercase();

    // Edge（Chromium 版标识为 "Edg/"）：其 UA 同时含 Chrome，必须最先判
    if let Some(v) = extract_version_after(&lower, "edg/") {
        return Some(format!("Edge {v}"));
    }
    // Chrome（且非 Edge，上面已短路）
    if lower.contains("chrome/") {
        return match extract_version_after(&lower, "chrome/") {
            Some(v) => Some(format!("Chrome {v}")),
            None => Some("Chrome".to_string()),
        };
    }
    // Firefox
    if lower.contains("firefox/") {
        return Some("Firefox".to_string());
    }
    // Safari（Chrome 的 UA 也含 "safari/"，故须排除 chrome）
    if lower.contains("safari/") && !lower.contains("chrome") {
        return Some("Safari".to_string());
    }
    None
}

/// 从 `haystack` 中定位 `token` 后紧跟的主版本号（第一个点前的数字段）。
///
/// 例如 `extract_version_after("chrome/120.0.6099", "chrome/")` → `Some("120")`。
/// `token` 后无数字则返回 `None`。`haystack` 需为已 lowercase 的串。
fn extract_version_after(haystack: &str, token: &str) -> Option<String> {
    let idx = haystack.find(token)?;
    let rest = &haystack[idx + token.len()..];
    // 主版本号：取到第一个非数字字符为止（"120.0.x" → "120"）
    let major: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if major.is_empty() {
        None
    } else {
        Some(major)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_outcome_success() {
        assert!(RequestOutcome::Success.is_success());
        assert!(!RequestOutcome::RateLimited.is_success());
    }

    #[test]
    fn test_record_roundtrip_json() {
        let mut rec = RequestRecord::new("req-1", "claude-sonnet-4");
        rec.credential_id = Some(3);
        rec.input_tokens = 100;
        rec.output_tokens = 50;
        rec.credits_used = Some(1.5);
        rec.latency_ms = 1234;
        rec.outcome = RequestOutcome::Success;

        let json = serde_json::to_string(&rec).unwrap();
        let back: RequestRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.request_id, "req-1");
        assert_eq!(back.credential_id, Some(3));
        assert_eq!(back.credits_used, Some(1.5));
        assert_eq!(back.outcome, RequestOutcome::Success);
    }

    #[test]
    fn test_outcome_serde_snake_case() {
        let json = serde_json::to_string(&RequestOutcome::AccountSuspended).unwrap();
        assert_eq!(json, "\"account_suspended\"");
    }

    #[test]
    fn test_record_client_device_roundtrip() {
        let mut rec = RequestRecord::new("req-dev", "claude-sonnet-4");
        rec.client_device = Some("claude-code".to_string());
        let json = serde_json::to_string(&rec).unwrap();
        // 序列化沿用 snake_case，前端字段名即 client_device
        assert!(json.contains("\"client_device\":\"claude-code\""));
        let back: RequestRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.client_device, Some("claude-code".to_string()));
    }

    #[test]
    fn test_classify_device_claude_code() {
        assert_eq!(
            classify_device(Some("claude-code/1.2.3")),
            Some("claude-code".to_string())
        );
        // 大小写不敏感
        assert_eq!(
            classify_device(Some("Claude-Code/2.0")),
            Some("claude-code".to_string())
        );
    }

    #[test]
    fn test_classify_device_claude_cli() {
        // 实测入站 UA：Claude Code CLI 新版本用 claude-cli 前缀，且不带 OS 信息
        assert_eq!(
            classify_device(Some("claude-cli/2.1.201 (external, cli)")),
            Some("claude-code".to_string())
        );
        // 大小写不敏感
        assert_eq!(
            classify_device(Some("Claude-CLI/2.1.201")),
            Some("claude-code".to_string())
        );
        // CLI 不带平台信息：OS/浏览器解析返回 None 属正常（不硬造）
        assert_eq!(parse_client_os(Some("claude-cli/2.1.201 (external, cli)")), None);
        assert_eq!(
            parse_client_browser(Some("claude-cli/2.1.201 (external, cli)")),
            None
        );
    }

    #[test]
    fn test_classify_device_anthropic_fallback() {
        // 官方 SDK/客户端兜底：含 anthropic 关键字归入 claude-code 类
        assert_eq!(
            classify_device(Some("anthropic-sdk-python/0.39.0")),
            Some("claude-code".to_string())
        );
    }

    #[test]
    fn test_classify_device_curl() {
        assert_eq!(
            classify_device(Some("curl/8.4.0")),
            Some("curl".to_string())
        );
    }

    #[test]
    fn test_classify_device_python() {
        assert_eq!(
            classify_device(Some("python-requests/2.31.0")),
            Some("python".to_string())
        );
        assert_eq!(
            classify_device(Some("Python/3.11 aiohttp/3.9")),
            Some("python".to_string())
        );
    }

    #[test]
    fn test_classify_device_node() {
        assert_eq!(
            classify_device(Some("axios/1.6.0")),
            Some("node".to_string())
        );
        assert_eq!(
            classify_device(Some("node-fetch/2.6")),
            Some("node".to_string())
        );
    }

    #[test]
    fn test_classify_device_vscode() {
        assert_eq!(
            classify_device(Some("VSCode/1.90.0")),
            Some("vscode".to_string())
        );
    }

    #[test]
    fn test_classify_device_windows() {
        // 纯浏览器 UA 带 Windows NT → windows（客户端标识未命中）
        assert_eq!(
            classify_device(Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36"
            )),
            Some("windows".to_string())
        );
    }

    #[test]
    fn test_classify_device_macos() {
        assert_eq!(
            classify_device(Some(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15"
            )),
            Some("macos".to_string())
        );
    }

    #[test]
    fn test_classify_device_linux() {
        assert_eq!(
            classify_device(Some("Mozilla/5.0 (X11; Linux x86_64) Gecko/20100101")),
            Some("linux".to_string())
        );
    }

    #[test]
    fn test_classify_device_browser() {
        // Mozilla 但不含任何 OS/客户端标识 → browser 兜底
        assert_eq!(
            classify_device(Some("Mozilla/5.0 (compatible; SomeBot/1.0)")),
            Some("browser".to_string())
        );
    }

    #[test]
    fn test_classify_device_unknown() {
        assert_eq!(classify_device(None), Some("unknown".to_string()));
        assert_eq!(classify_device(Some("")), Some("unknown".to_string()));
        assert_eq!(classify_device(Some("   ")), Some("unknown".to_string()));
        assert_eq!(
            classify_device(Some("SomethingWeird/9")),
            Some("unknown".to_string())
        );
    }

    #[test]
    fn test_classify_device_client_priority_over_os() {
        // claude-code 的 UA 夹带 Windows 信息时，仍判为 claude-code（客户端优先）
        assert_eq!(
            classify_device(Some("claude-code/1.0 (Windows NT 10.0)")),
            Some("claude-code".to_string())
        );
    }

    // ---- parse_client_os ----

    #[test]
    fn test_parse_os_windows() {
        assert_eq!(
            parse_client_os(Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36"
            )),
            Some("Windows".to_string())
        );
    }

    #[test]
    fn test_parse_os_macos() {
        assert_eq!(
            parse_client_os(Some(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15"
            )),
            Some("macOS".to_string())
        );
    }

    #[test]
    fn test_parse_os_ios() {
        // iPhone
        assert_eq!(
            parse_client_os(Some(
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15"
            )),
            Some("iOS".to_string())
        );
        // iPad 的 UA 含 "Mac OS X"，但必须判为 iOS 而非 macOS
        assert_eq!(
            parse_client_os(Some(
                "Mozilla/5.0 (iPad; CPU OS 16_0 like Mac OS X) AppleWebKit/605.1.15"
            )),
            Some("iOS".to_string())
        );
    }

    #[test]
    fn test_parse_os_android() {
        // Android 的 UA 含 "Linux"，但必须判为 Android 而非 Linux
        assert_eq!(
            parse_client_os(Some(
                "Mozilla/5.0 (Linux; Android 13; Pixel 7) AppleWebKit/537.36"
            )),
            Some("Android".to_string())
        );
    }

    #[test]
    fn test_parse_os_linux() {
        assert_eq!(
            parse_client_os(Some("Mozilla/5.0 (X11; Linux x86_64) Gecko/20100101")),
            Some("Linux".to_string())
        );
    }

    #[test]
    fn test_parse_os_none() {
        assert_eq!(parse_client_os(None), None);
        assert_eq!(parse_client_os(Some("")), None);
        assert_eq!(parse_client_os(Some("   ")), None);
        // curl 的 UA 不含 OS 信息 → None
        assert_eq!(parse_client_os(Some("curl/8.4.0")), None);
    }

    // ---- parse_client_browser ----

    #[test]
    fn test_parse_browser_edge() {
        // Edge 的 UA 同时含 Chrome/ 与 Edg/，必须判为 Edge
        assert_eq!(
            parse_client_browser(Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.2210.61"
            )),
            Some("Edge 120".to_string())
        );
    }

    #[test]
    fn test_parse_browser_chrome() {
        assert_eq!(
            parse_client_browser(Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/120.0.6099.109 Safari/537.36"
            )),
            Some("Chrome 120".to_string())
        );
    }

    #[test]
    fn test_parse_browser_firefox() {
        assert_eq!(
            parse_client_browser(Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:121.0) Gecko/20100101 Firefox/121.0"
            )),
            Some("Firefox".to_string())
        );
    }

    #[test]
    fn test_parse_browser_safari() {
        // 纯 Safari（不含 Chrome）
        assert_eq!(
            parse_client_browser(Some(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 \
                 (KHTML, like Gecko) Version/17.0 Safari/605.1.15"
            )),
            Some("Safari".to_string())
        );
    }

    #[test]
    fn test_parse_browser_non_browser_none() {
        assert_eq!(parse_client_browser(None), None);
        assert_eq!(parse_client_browser(Some("")), None);
        assert_eq!(parse_client_browser(Some("curl/8.4.0")), None);
        assert_eq!(parse_client_browser(Some("python-requests/2.31.0")), None);
        assert_eq!(parse_client_browser(Some("axios/1.6.0")), None);
    }
}
