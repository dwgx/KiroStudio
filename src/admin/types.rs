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
    /// 凭据级 RPM 容量上限（None=继承全局）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpm_limit: Option<u32>,
    /// 凭据级「允许模型」白名单（成本安全硬门；None/空=不限制）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_models: Option<Vec<String>>,
    /// 「测试可用模型」历史结果（探测打的标签，供前端展示该号测过什么）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tested_models: Option<Vec<crate::kiro::model::credentials::TestedModel>>,
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
    /// 自定义 API 代挂:上游 base_url(展示用；api_key 绝不下发)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// 自定义 API 代挂:请求上限
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_limit: Option<u64>,
    /// 自定义 API 代挂:累计已发请求数
    #[serde(default)]
    pub request_count: u64,
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
    /// 订阅等级标题（如 "Kiro Pro"）。随凭据持久化，重启后即可展示，
    /// 无需等待首次余额刷新；后台温和刷新时会顺带更新。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_title: Option<String>,
    /// API 调用成功次数
    pub success_count: u64,
    /// 生命周期累计 credit 花费（上游 meteringEvent 真实计费累加，独立于用量保留期，只增不清）。
    /// 供前端凭据卡片展示"这个号从入池至今一共花了多少 credit"。
    pub total_credits_used: f64,
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
    /// 当前在途（in-flight）请求数（实时负载，用于观测均衡是否生效）
    pub inflight: u32,
    /// 最近 60 秒滚动窗口内的请求数（RPM 观测）
    pub rpm: u32,
    /// 用户自定义别名/备注（卡片展示优先于 email/#id）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 是否正处于冷却中（429/限流/服务错误后短暂跳过）
    pub cooling_down: bool,
    /// 冷却剩余毫秒（cooling_down 为 true 时有效）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_remaining_ms: Option<u64>,
    /// 冷却原因（如「速率限制」「服务错误」）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_reason: Option<String>,
}

// ============ 凭据回收站 ============

/// 回收站列表响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashListResponse {
    /// 回收站条目总数
    pub total: usize,
    /// 已删除凭据列表（按删除时间倒序）
    pub trash: Vec<TrashItemResponse>,
}

/// 单个回收站条目（不含敏感明文）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashItemResponse {
    /// 凭据唯一 ID（恢复时保持不变）
    pub id: u64,
    /// 优先级
    pub priority: u32,
    /// 认证方式
    pub auth_method: Option<String>,
    /// 用户邮箱
    pub email: Option<String>,
    /// kiroApiKey 的脱敏展示（仅 API Key 凭据）
    pub masked_api_key: Option<String>,
    /// refreshToken 的 SHA-256 哈希（仅 OAuth 凭据，用于前端去重展示）
    pub refresh_token_hash: Option<String>,
    /// kiroApiKey 的 SHA-256 哈希（仅 API Key 凭据）
    pub api_key_hash: Option<String>,
    /// 端点名称
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// 删除时间（RFC3339 格式）
    pub deleted_at: String,
    /// 删除前累计成功次数
    pub success_count: u64,
    /// 删除前最后一次调用时间（RFC3339 格式）
    pub last_used_at: Option<String>,
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

/// 设置凭据级 RPM 容量上限的请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetRpmLimitRequest {
    /// 新 RPM 容量（0/null = 继承全局）
    pub rpm_limit: Option<u32>,
}

/// 修改自定义 API(代挂透传)凭据配置的请求。字段均可选:None=不改。
/// 仅对 custom_api 凭据有效(后端 gate 拒绝非 custom_api 号)。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetCustomApiConfigRequest {
    /// 上游地址(base_url)。None=不改;非空=更新;空串=后端拒绝(必填)。
    #[serde(default)]
    pub base_url: Option<String>,
    /// 上游密钥(api_key)。None=不改;空串=清除;非空=更新。
    #[serde(default)]
    pub api_key: Option<String>,
    /// 请求上限。None=不改;0=不限;>0=更新。
    #[serde(default)]
    pub request_limit: Option<u64>,
    /// 是否归零调用次数(换上游/换 key 时前端可勾选,避免旧计数残留触顶)。
    #[serde(default)]
    pub reset_count: bool,
}

/// 设置凭据级「允许模型」白名单的请求（成本安全硬门；空/null = 不限制）
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetAllowedModelsRequest {
    /// 允许的 kiro modelId 列表（如 `["deepseek-3.2","glm-5"]`）。空/null = 不限制。
    #[serde(default)]
    pub allowed_models: Option<Vec<String>>,
}

/// 添加凭据请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddCredentialRequest {
    pub access_token: Option<String>,

    /// 刷新令牌（OAuth 凭据必填，API Key 凭据不需要）
    pub refresh_token: Option<String>,

    /// 认证方式（可选，默认 social）
    #[serde(default = "default_auth_method")]
    pub auth_method: String,

    /// OIDC Client ID（IdC 认证需要）
    pub client_id: Option<String>,

    /// OIDC Client Secret（IdC 认证需要）
    pub client_secret: Option<String>,

    pub token_endpoint: Option<String>,

    pub issuer_url: Option<String>,

    pub scopes: Option<String>,

    pub profile_arn: Option<String>,

    pub expires_at: Option<String>,

    /// 优先级（可选，默认 0）
    #[serde(default)]
    pub priority: u32,

    /// 凭据级 RPM 容量上限（可选，None/0=继承全局）
    #[serde(default)]
    pub rpm_limit: Option<u32>,

    // ==== 自定义 API 代挂透传（authMethod=custom_api 时前端填入）====
    /// 自定义 API 上游基址（Anthropic 兼容中转站，透传目标）
    #[serde(default)]
    pub base_url: Option<String>,
    /// 自定义 API 密钥（透传时替换成它）
    #[serde(default)]
    pub api_key: Option<String>,
    /// 请求上限（累计达到后自动禁用，None/0=不限）
    #[serde(default)]
    pub request_limit: Option<u64>,

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

    /// 用户自定义别名/备注（可选，卡片展示优先于 email/#id）
    #[serde(default)]
    pub name: Option<String>,

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
    /// 使用限额（base，不含 overage）
    pub usage_limit: f64,
    /// 剩余额度（overage 感知：overage 开启时含 overage cap）
    pub remaining: f64,
    /// 使用百分比（基于 effective_limit 计算）
    pub usage_percentage: f64,
    /// 下次重置时间（Unix 时间戳）
    pub next_reset_at: Option<f64>,
    /// 是否开启超额（Online Overage）。serde default 兼容旧磁盘缓存。
    #[serde(default)]
    pub overage_enabled: bool,
    /// 超额上限（overage cap）。未开启时为 0。serde default 兼容旧磁盘缓存。
    #[serde(default)]
    pub overage_cap: f64,
    /// 有效使用限额（base + overage cap）。serde default 兼容旧磁盘缓存。
    #[serde(default)]
    pub effective_limit: f64,
}

// ============ 批量缓存余额（A10）============

/// 单条已缓存余额快照（含缓存时间戳，供前端判断新鲜度）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedBalanceItem {
    /// 缓存的余额数据
    #[serde(flatten)]
    pub balance: BalanceResponse,
    /// 缓存写入时间（Unix 秒），前端据此判断新鲜度
    pub cached_at: f64,
}

/// 批量已缓存余额响应
///
/// 仅返回【已缓存】凭据的快照，只读缓存，绝不触发任何上游调用。
/// 缓存未命中的凭据不出现在 balances 中（前端可按需单独拉取）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedBalancesResponse {
    /// 已缓存的凭据数量
    pub total: usize,
    /// id -> 缓存余额快照
    pub balances: std::collections::HashMap<u64, CachedBalanceItem>,
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
    /// 新凭据优先级（默认 0：所有号平权，越小越优先）
    #[serde(default = "default_login_priority")]
    pub priority: u32,
    /// 可选自定义出站代理（不填继承全局）
    #[serde(default)]
    pub proxy_url: Option<String>,
}

fn default_login_priority() -> u32 {
    // 默认 0:所有号平权(见 handlers.rs default_priority 说明)。
    0
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
    /// 服务端版本（编译期注入 Cargo.toml version），供前端展示真实版本,不再硬编码。
    pub server_version: String,
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
    /// Claude Code 自动切缓冲协议（识别到 CC 请求时 /v1 流式自动走 buffered，准确 input_tokens）
    pub cc_auto_buffer: bool,
    /// 是否剥离转发给上游的 system 环境噪音（省 token / 提缓存命中 / 降关联，立即生效）
    pub strip_env_noise: bool,
    /// 工具错误缓解：泄漏控制 token 清洗 / 流式失败态对齐 / 如实暴露错误（均立即生效，默认关）
    pub tool_clean_leaked_tokens: bool,
    pub tool_stream_align_failure: bool,
    pub tool_expose_error_to_client: bool,
    /// JSON 修复层（根治向）：非法工具参数修成合法 JSON 再发客户端（立即生效，默认开）
    pub tool_repair_json: bool,
    /// 截断跨轮恢复：真截断且修复层补不回时置失败态让客户端重试整轮（立即生效，默认关）
    pub tool_truncation_recovery: bool,
    /// 入站工具顶层 description 字符上限（默认 10000，立即生效，0=不截断）
    pub tool_description_max_chars: usize,
    pub cooldown_enabled: bool,
    pub rate_limit_enabled: bool,
    pub rate_limit_daily_max: u32,
    pub rate_limit_min_interval_ms: u64,
    pub affinity_enabled: bool,
    /// 均衡模式下是否叠加优先级分发
    pub priority_in_balanced: bool,
    /// 是否配置了全局代理（不回传明文）
    pub has_proxy: bool,
    pub proxy_url: Option<String>,
    /// 是否配置了 admin key（不回传明文）
    pub has_admin_key: bool,
    /// 是否配置了 userKey（下游对话 api_key，不回传明文）
    pub has_api_key: bool,
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
    // ---- Admin UI 登录页 ----
    pub login_background_enabled: bool,
    /// 登录页背景图是否走 R18 图源（立即生效）
    pub login_background_r18: bool,
    // ---- 余额同步（A6）----
    /// 后台温和余额刷新间隔（秒，0=禁用）
    pub balance_refresh_interval_secs: u64,
    // ---- 隐私 ----
    /// 是否采集下游客户端指纹（device/ip/os/browser，立即生效）
    pub collect_client_fingerprint: bool,
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
    pub cc_auto_buffer: Option<bool>,
    pub strip_env_noise: Option<bool>,
    pub tool_clean_leaked_tokens: Option<bool>,
    pub tool_stream_align_failure: Option<bool>,
    pub tool_expose_error_to_client: Option<bool>,
    pub tool_repair_json: Option<bool>,
    pub tool_truncation_recovery: Option<bool>,
    pub tool_description_max_chars: Option<usize>,
    pub cooldown_enabled: Option<bool>,
    pub rate_limit_enabled: Option<bool>,
    pub rate_limit_daily_max: Option<u32>,
    pub rate_limit_min_interval_ms: Option<u64>,
    pub affinity_enabled: Option<bool>,
    pub priority_in_balanced: Option<bool>,
    /// 全局代理地址；传空字符串表示清除
    pub proxy_url: Option<String>,
    /// 全局代理认证用户名；出于安全前端不回显已存值，仅在非空时更新
    #[serde(default)]
    pub proxy_username: Option<String>,
    /// 全局代理认证密码；出于安全前端不回显已存值，仅在非空时更新
    #[serde(default)]
    pub proxy_password: Option<String>,
    /// 网页上号回调基地址；传空字符串表示清除（回退本地模式）
    pub callback_base_url: Option<String>,
    /// 下游客户端对话 API Key（userKey，x-api-key）。出于安全前端不回显已存值，仅在非空时更新；
    /// ⚠️需重启生效（认证中间件在启动时固化 key）。空白值会被后端拒绝（防 fail-open）。
    #[serde(default)]
    pub api_key: Option<String>,
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
    // ---- Admin UI 登录页（立即生效）----
    pub login_background_enabled: Option<bool>,
    /// 登录页背景图是否走 R18 图源（立即生效）
    pub login_background_r18: Option<bool>,
    // ---- 余额同步（A6，需重启生效）----
    /// 后台温和余额刷新间隔（秒，0=禁用）
    pub balance_refresh_interval_secs: Option<u64>,
    // ---- 隐私（立即生效）----
    /// 是否采集下游客户端指纹（device/ip/os/browser）
    pub collect_client_fingerprint: Option<bool>,
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

// ============ 存储统计 / 清理（运维）============

/// 单个数据分区的占用统计
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoragePartition {
    /// 分区键（与清理 target 一致）：traces | usage_jsonl | trash | bg_cache
    pub key: String,
    /// 展示名（中文）
    pub label: String,
    /// 占用字节数（内存分区为常驻内存字节）
    pub bytes: u64,
    /// 条目/文件数（trace 为行数，usage_jsonl 为文件数，trash 为条目数，bg_cache 为张数）
    pub items: u64,
    /// 落盘路径（内存分区为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 是否为纯内存分区（无落盘，清理即释放内存）
    pub in_memory: bool,
}

/// 存储统计响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageStatsResponse {
    /// 各分区占用明细
    pub partitions: Vec<StoragePartition>,
    /// 落盘分区字节合计（不含纯内存分区）
    pub total_disk_bytes: u64,
    /// 统计是否可用（用量统计未启用时 trace/jsonl 分区缺失）
    pub usage_enabled: bool,
}

/// 存储清理请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageCleanupRequest {
    /// 清理目标（白名单枚举）：traces | usage_jsonl | trash | bg_cache | all
    pub target: String,
    /// 保留天数：删除早于 N 天前的数据。省略时按各分区的配置默认保留期。
    #[serde(default)]
    pub older_than_days: Option<i64>,
}

/// 单个分区的清理结果
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageCleanupItem {
    /// 分区键
    pub key: String,
    /// 清理的条目/文件数
    pub removed: u64,
    /// 释放的字节数（不可精确统计时为 0）
    pub freed_bytes: u64,
    /// 说明（如跳过原因）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 存储清理响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageCleanupResponse {
    pub success: bool,
    pub message: String,
    /// 各分区清理明细
    pub results: Vec<StorageCleanupItem>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_config_deserializes_login_background_r18() {
        // 前端以 camelCase 提交 loginBackgroundR18，应正确落到 snake_case 字段。
        let json = r#"{"loginBackgroundR18": false}"#;
        let req: UpdateConfigRequest = serde_json::from_str(json).expect("反序列化应成功");
        assert_eq!(req.login_background_r18, Some(false));
        // 未提交的字段应为 None（仅改动字段才更新）。
        assert_eq!(req.login_background_enabled, None);
    }

    #[test]
    fn update_config_omits_login_background_r18_when_absent() {
        // 请求体不含该字段时应为 None，不会误改。
        let req: UpdateConfigRequest = serde_json::from_str("{}").expect("空对象应成功");
        assert_eq!(req.login_background_r18, None);
    }

    #[test]
    fn config_snapshot_serializes_login_background_r18() {
        // 快照以 camelCase 下发，前端据此渲染开关初值。
        let snap = ConfigSnapshotResponse {
            server_version: "0.0.0".into(),
            host: "127.0.0.1".into(),
            port: 8080,
            region: "us-east-1".into(),
            kiro_version: "0.0.0".into(),
            system_version: "sys".into(),
            node_version: "node".into(),
            tls_backend: "rustls".into(),
            load_balancing_mode: "priority".into(),
            default_endpoint: "ide".into(),
            endpoint_names: vec![],
            extract_thinking: true,
            cc_auto_buffer: true,
            cooldown_enabled: true,
            rate_limit_enabled: false,
            rate_limit_daily_max: 500,
            rate_limit_min_interval_ms: 1000,
            affinity_enabled: true,
            priority_in_balanced: false,
            has_proxy: false,
            proxy_url: None,
            has_admin_key: false,
            has_api_key: false,
            callback_mode: "local".into(),
            callback_base_url: None,
            cors_allowed_origins: vec![],
            ip_allowlist: vec![],
            trust_forwarded_header: false,
            ingress_rate_limit_per_min: 0,
            max_body_bytes: 0,
            proactive_token_refresh: true,
            token_refresh_lead_minutes: 10,
            token_refresh_interval_secs: 60,
            login_background_enabled: true,
            login_background_r18: false,
            balance_refresh_interval_secs: 1800,
            collect_client_fingerprint: true,
            strip_env_noise: true,
            tool_clean_leaked_tokens: true,
            tool_stream_align_failure: true,
            tool_expose_error_to_client: true,
            tool_repair_json: true,
            tool_truncation_recovery: false,
            tool_description_max_chars: 10000,
            config_path: None,
        };
        let s = serde_json::to_string(&snap).expect("序列化应成功");
        assert!(s.contains("\"loginBackgroundR18\":false"));
        assert!(s.contains("\"loginBackgroundEnabled\":true"));
    }
}
