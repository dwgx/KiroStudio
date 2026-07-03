//! Admin API 类型定义

use serde::{Deserialize, Serialize};

// ============ 凭据状态 ============

/// 所有凭据状态响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialsStatusResponse {
    /// 凭据总数
    pub total: usize,
    /// 可用凭据数量（未禁用）
    pub available: usize,
    /// 当前活跃凭据 ID
    pub current_id: u64,
    /// 各凭据状态列表
    pub credentials: Vec<CredentialStatusItem>,
}

/// 单个凭据的状态信息
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialStatusItem {
    /// 凭据唯一 ID
    pub id: u64,
    /// 优先级（数字越小优先级越高）
    pub priority: u32,
    /// 是否被禁用
    pub disabled: bool,
    /// 连续失败次数
    pub failure_count: u32,
    /// 是否为当前活跃凭据
    pub is_current: bool,
    /// Token 过期时间（RFC3339 格式）
    pub expires_at: Option<String>,
    /// 认证方式
    pub auth_method: Option<String>,
    /// 是否有 Profile ARN
    pub has_profile_arn: bool,
    /// refreshToken 的 SHA-256 哈希（仅 OAuth 凭据，用于前端去重）
    pub refresh_token_hash: Option<String>,
    /// kiroApiKey 的 SHA-256 哈希（仅 API Key 凭据，用于前端去重）
    pub api_key_hash: Option<String>,
    /// kiroApiKey 的脱敏展示（仅 API Key 凭据，用于前端显示）
    pub masked_api_key: Option<String>,
    /// 用户邮箱（用于前端显示）
    pub email: Option<String>,
    /// API 调用成功次数
    pub success_count: u64,
    /// 最后一次 API 调用时间（RFC3339 格式）
    pub last_used_at: Option<String>,
    /// 是否配置了凭据级代理
    pub has_proxy: bool,
    /// 代理 URL（用于前端展示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    /// Token 刷新连续失败次数
    pub refresh_failure_count: u32,
    /// 禁用原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// 端点名称（决定该凭据走哪套 Kiro API，已回退到默认端点）
    pub endpoint: String,
}

// ============ 操作请求 ============

/// 启用/禁用凭据请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetDisabledRequest {
    /// 是否禁用
    pub disabled: bool,
}

/// 修改优先级请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetPriorityRequest {
    /// 新优先级值
    pub priority: u32,
}

/// 添加凭据请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddCredentialRequest {
    /// 刷新令牌（OAuth 凭据必填，API Key 凭据不需要）
    pub refresh_token: Option<String>,

    /// 认证方式（可选，默认 social）
    #[serde(default = "default_auth_method")]
    pub auth_method: String,

    /// OIDC Client ID（IdC 认证需要）
    pub client_id: Option<String>,

    /// OIDC Client Secret（IdC 认证需要）
    pub client_secret: Option<String>,

    /// 优先级（可选，默认 0）
    #[serde(default)]
    pub priority: u32,

    /// 凭据级 Region 配置（用于 OIDC token 刷新）
    /// 未配置时回退到 config.json 的全局 region
    pub region: Option<String>,

    /// 凭据级 Auth Region（用于 Token 刷新）
    pub auth_region: Option<String>,

    /// 凭据级 API Region（用于 API 请求）
    pub api_region: Option<String>,

    /// 凭据级 Machine ID（可选，64 位字符串）
    /// 未配置时回退到 config.json 的 machineId
    pub machine_id: Option<String>,

    /// 用户邮箱（可选，用于前端显示）
    pub email: Option<String>,

    /// 凭据级代理 URL（可选，特殊值 "direct" 表示不使用代理）
    pub proxy_url: Option<String>,

    /// 凭据级代理认证用户名（可选）
    pub proxy_username: Option<String>,

    /// 凭据级代理认证密码（可选）
    pub proxy_password: Option<String>,

    /// Kiro API Key（API Key 凭据必填，格式: ksk_xxxxxxxx）
    /// 设置后直接作为 Bearer Token 使用，无需 refreshToken
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kiro_api_key: Option<String>,

    /// 端点名称（可选，未配置时使用 config.defaultEndpoint）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

fn default_auth_method() -> String {
    "social".to_string()
}

/// 添加凭据成功响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddCredentialResponse {
    pub success: bool,
    pub message: String,
    /// 新添加的凭据 ID
    pub credential_id: u64,
    /// 用户邮箱（如果获取成功）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

// ============ 余额查询 ============

/// 余额查询响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BalanceResponse {
    /// 凭据 ID
    pub id: u64,
    /// 订阅类型
    pub subscription_title: Option<String>,
    /// 当前使用量
    pub current_usage: f64,
    /// 使用限额
    pub usage_limit: f64,
    /// 剩余额度
    pub remaining: f64,
    /// 使用百分比
    pub usage_percentage: f64,
    /// 下次重置时间（Unix 时间戳）
    pub next_reset_at: Option<f64>,
}

// ============ 负载均衡配置 ============

/// 负载均衡模式响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadBalancingModeResponse {
    /// 当前模式（"priority" 或 "balanced"）
    pub mode: String,
}

/// 设置负载均衡模式请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetLoadBalancingModeRequest {
    /// 模式（"priority" 或 "balanced"）
    pub mode: String,
}

// ============ 通用响应 ============

/// 操作成功响应
#[derive(Debug, Serialize)]
pub struct SuccessResponse {
    pub success: bool,
    pub message: String,
}

impl SuccessResponse {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            success: true,
            message: message.into(),
        }
    }
}

/// 错误响应
#[derive(Debug, Serialize)]
pub struct AdminErrorResponse {
    pub error: AdminError,
}

#[derive(Debug, Serialize)]
pub struct AdminError {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

impl AdminErrorResponse {
    pub fn new(error_type: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: AdminError {
                error_type: error_type.into(),
                message: message.into(),
            },
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new("invalid_request", message)
    }

    pub fn authentication_error() -> Self {
        Self::new("authentication_error", "Invalid or missing admin API key")
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }

    pub fn api_error(message: impl Into<String>) -> Self {
        Self::new("api_error", message)
    }

    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new("internal_error", message)
    }
}

// ============ 网页上号（Social OAuth）============

/// 发起网页上号请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartSocialLoginRequest {
    /// 新凭据优先级（默认 100）
    #[serde(default = "default_login_priority")]
    pub priority: u32,
    /// 可选自定义出站代理（不填继承全局）
    #[serde(default)]
    pub proxy_url: Option<String>,
}

fn default_login_priority() -> u32 {
    100
}

/// 发起网页上号响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartSocialLoginResponse {
    pub session_id: String,
    /// 供用户在浏览器打开的 Kiro 登录地址
    pub portal_url: String,
}

/// 轮询网页上号响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PollSocialLoginResponse {
    /// pending | done | error
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ============ 服务端配置快照（设置页只读展示 + 部分可改）============

/// 服务端配置快照（敏感字段脱敏后返回）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSnapshotResponse {
    pub host: String,
    pub port: u16,
    pub region: String,
    pub kiro_version: String,
    pub system_version: String,
    pub node_version: String,
    pub tls_backend: String,
    pub load_balancing_mode: String,
    pub default_endpoint: String,
    pub endpoint_names: Vec<String>,
    pub extract_thinking: bool,
    pub cooldown_enabled: bool,
    pub rate_limit_enabled: bool,
    pub rate_limit_daily_max: u32,
    pub rate_limit_min_interval_ms: u64,
    pub affinity_enabled: bool,
    /// 是否配置了全局代理（不回传明文）
    pub has_proxy: bool,
    pub proxy_url: Option<String>,
    /// 是否配置了 admin key（不回传明文）
    pub has_admin_key: bool,
    /// 回调模式：local（本地端口）/ remote（公网回调）
    pub callback_mode: String,
    pub callback_base_url: Option<String>,
    // ---- 反代安全（批次3）----
    pub cors_allowed_origins: Vec<String>,
    pub ip_allowlist: Vec<String>,
    pub trust_forwarded_header: bool,
    pub ingress_rate_limit_per_min: u32,
    pub max_body_bytes: usize,
    // ---- 主动 token 预刷新（批次4.4）----
    pub proactive_token_refresh: bool,
    pub token_refresh_lead_minutes: i64,
    pub token_refresh_interval_secs: u64,
    /// 配置文件路径（运行时只读元数据）
    pub config_path: Option<String>,
}

/// 更新服务端配置请求
///
/// 所有字段可选：仅提交的字段被修改并持久化到 config.json。
/// 敏感字段（admin key / api key / 代理密码）不在此开放。
/// 除 `load_balancing_mode` 立即生效外，其余字段需重启进程后生效。
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateConfigRequest {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub region: Option<String>,
    pub kiro_version: Option<String>,
    pub system_version: Option<String>,
    pub node_version: Option<String>,
    /// "rustls" | "native-tls"
    pub tls_backend: Option<String>,
    /// "priority" | "balanced"（立即生效）
    pub load_balancing_mode: Option<String>,
    pub default_endpoint: Option<String>,
    pub extract_thinking: Option<bool>,
    pub cooldown_enabled: Option<bool>,
    pub rate_limit_enabled: Option<bool>,
    pub rate_limit_daily_max: Option<u32>,
    pub rate_limit_min_interval_ms: Option<u64>,
    pub affinity_enabled: Option<bool>,
    /// 全局代理地址；传空字符串表示清除
    pub proxy_url: Option<String>,
    /// 网页上号回调基地址；传空字符串表示清除（回退本地模式）
    pub callback_base_url: Option<String>,
    // ---- 反代安全（批次3，均需重启生效）----
    /// CORS 允许来源列表（整表替换）
    pub cors_allowed_origins: Option<Vec<String>>,
    /// 入口 IP 白名单（CIDR/单 IP，整表替换）
    pub ip_allowlist: Option<Vec<String>>,
    /// 是否信任 X-Forwarded-For
    pub trust_forwarded_header: Option<bool>,
    /// 入口每-IP 每分钟限流（0=关闭）
    pub ingress_rate_limit_per_min: Option<u32>,
    /// 请求体最大字节数
    pub max_body_bytes: Option<usize>,
    // ---- 主动 token 预刷新（批次4.4，需重启生效）----
    pub proactive_token_refresh: Option<bool>,
    pub token_refresh_lead_minutes: Option<i64>,
    pub token_refresh_interval_secs: Option<u64>,
}

/// 更新服务端配置响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateConfigResponse {
    pub success: bool,
    pub message: String,
    /// 是否有字段需要重启才能生效
    pub restart_required: bool,
    /// 需要重启才生效的已改字段名（前端用于提示）
    pub restart_fields: Vec<String>,
}
