use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TlsBackend {
    Rustls,
    NativeTls,
}

impl Default for TlsBackend {
    fn default() -> Self {
        Self::Rustls
    }
}

/// KNA 应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default = "default_region")]
    pub region: String,

    /// Auth Region（用于 Token 刷新），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// API Region（用于 API 请求），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    #[serde(default = "default_kiro_version")]
    pub kiro_version: String,

    #[serde(default)]
    pub machine_id: Option<String>,

    #[serde(default)]
    pub api_key: Option<String>,

    #[serde(default = "default_system_version")]
    pub system_version: String,

    #[serde(default = "default_node_version")]
    pub node_version: String,

    #[serde(default = "default_tls_backend")]
    pub tls_backend: TlsBackend,

    /// 外部 count_tokens API 地址（可选）
    #[serde(default)]
    pub count_tokens_api_url: Option<String>,

    /// count_tokens API 密钥（可选）
    #[serde(default)]
    pub count_tokens_api_key: Option<String>,

    /// count_tokens API 认证类型（可选，"x-api-key" 或 "bearer"，默认 "x-api-key"）
    #[serde(default = "default_count_tokens_auth_type")]
    pub count_tokens_auth_type: String,

    /// HTTP 代理地址（可选）
    /// 支持格式: http://host:port, https://host:port, socks5://host:port
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// 代理认证用户名（可选）
    #[serde(default)]
    pub proxy_username: Option<String>,

    /// 代理认证密码（可选）
    #[serde(default)]
    pub proxy_password: Option<String>,

    /// Admin API 密钥（可选，启用 Admin API 功能）
    #[serde(default)]
    pub admin_api_key: Option<String>,

    /// 负载均衡模式（"priority" 或 "balanced"）
    #[serde(default = "default_load_balancing_mode")]
    pub load_balancing_mode: String,

    /// 是否开启非流式响应的 thinking 块提取（默认 true）
    ///
    /// 启用后，非流式响应中的 `<thinking>...</thinking>` 标签会被解析为
    /// 独立的 `{"type": "thinking", ...}` 内容块,与流式响应行为一致。
    #[serde(default = "default_extract_thinking")]
    pub extract_thinking: bool,

    /// 默认端点名称（凭据未显式指定 endpoint 时使用，默认 "ide"）
    #[serde(default = "default_endpoint")]
    pub default_endpoint: String,

    /// 端点特定的配置
    ///
    /// 键为端点名（如 "ide" / "cli"），值为该端点自由定义的参数对象。
    /// 未在此表出现的端点沿用实现内置默认值。
    #[serde(default)]
    pub endpoints: HashMap<String, serde_json::Value>,

    /// 是否启用失败冷却（429/认证失败等后短暂跳过该凭据，默认 true）
    ///
    /// 纯本地反应式调度：仅在凭据已出错时跳过它一段时间，无副作用，建议常开。
    #[serde(default = "default_cooldown_enabled")]
    pub cooldown_enabled: bool,

    /// 是否启用拟人速率限制（每凭据每日上限 + 请求间隔，默认 false）
    ///
    /// 防关联用：模拟人类节奏。注意默认间隔 1s/请求会拖慢单用户高频工具调用，
    /// 故默认关闭；多账号轮换或在意关联风险时再开。配合 `rate_limit_*` 微调。
    #[serde(default)]
    pub rate_limit_enabled: bool,

    /// 速率限制：每凭据每日最大请求数（仅 rate_limit_enabled 时生效，默认 500）
    #[serde(default = "default_rate_limit_daily")]
    pub rate_limit_daily_max: u32,

    /// 速率限制：最小请求间隔毫秒（仅 rate_limit_enabled 时生效，默认 1000）
    #[serde(default = "default_rate_limit_min_interval_ms")]
    pub rate_limit_min_interval_ms: u64,

    /// 是否启用会话亲和性（同一会话尽量复用同一凭据，默认 true）
    ///
    /// 防关联用：让同一对话粘在同一账号上，避免单次会话散落到多个账号引发关联。
    /// key 取自请求 metadata.user_id 提取的 session UUID（无 session 时随机，不命中即正常轮换）。
    /// 主要在 balanced 模式下生效；priority 模式本就固定单凭据，影响甚微。
    #[serde(default = "default_affinity_enabled")]
    pub affinity_enabled: bool,

    /// 是否启用 prompt 缓存记账（默认 true）
    ///
    /// Kiro 上游不回传 Anthropic 的 cache_read / cache_creation 记账字段。
    /// 开启后，网关侧维护本地影子缓存表，按凭据推算并注入这些字段，
    /// 让下游客户端（Claude Code 等）能显示缓存命中情况。
    /// 这是估算展示，非真实计费（真实计费以 meteringEvent 为准）。
    #[serde(default = "default_prompt_cache_enabled")]
    pub prompt_cache_enabled: bool,

    /// prompt 缓存记账的最大 TTL 秒数（默认 3600，支持 5m/1h 断点）
    #[serde(default = "default_prompt_cache_ttl_seconds")]
    pub prompt_cache_ttl_seconds: u64,

    /// 网页上号回调基地址（可选）
    ///
    /// - 不配置：本地回调模式，后端在本机临时端口接收 OAuth 回调（仅本机浏览器可达）。
    /// - 配置为公网地址（如 `https://kiro.example.com`）：远程回调模式，
    ///   浏览器回调打到 `{callbackBaseUrl}/api/admin/auth/callback`，适合 Docker/服务器部署。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_base_url: Option<String>,

    /// 是否启用用量统计（请求埋点 + SQLite/JSONL 落盘 + 内存预聚合，默认 true）
    ///
    /// 关闭后热路径的埋点管道不初始化，`emit_record` 静默丢弃，零开销。
    #[serde(default = "default_usage_enabled")]
    pub usage_enabled: bool,

    /// 用量数据目录（SQLite 与 JSONL 落盘位置，默认 "data/usage"）
    #[serde(default = "default_usage_data_dir")]
    pub usage_data_dir: String,

    /// 用量明细（SQLite traces）保留天数，超期后台清理（默认 30）
    #[serde(default = "default_usage_retention_days")]
    pub usage_retention_days: i64,

    // ============ 反代安全（批次3）============
    /// CORS 允许来源列表。空 = 允许任意来源（`Access-Control-Allow-Origin: *`，
    /// 保持向后兼容公开 API 场景）。非空时仅回显命中列表的 Origin，凭据请求也受控。
    /// 例：`["https://app.example.com", "http://localhost:5173"]`
    #[serde(default)]
    pub cors_allowed_origins: Vec<String>,

    /// 入口 IP 白名单（CIDR 或单 IP）。空 = 不限制。命中才放行，否则 403。
    /// 支持 IPv4/IPv6 CIDR，例：`["127.0.0.1/32", "10.0.0.0/8", "::1/128"]`。
    /// 客户端 IP 取 TCP 连接对端；若在反代后需按 `trust_forwarded_header` 取 XFF。
    #[serde(default)]
    pub ip_allowlist: Vec<String>,

    /// 是否信任 `X-Forwarded-For` / `X-Real-IP` 头来判定客户端 IP（默认 false）。
    /// **仅当本服务确实部署在可信反代（nginx/traefik）之后才可开启**，
    /// 否则客户端可伪造该头绕过 IP 白名单与限流。
    #[serde(default)]
    pub trust_forwarded_header: bool,

    /// 入口每-IP 限流：每分钟最大请求数。0 = 不限流（默认 0）。
    /// 固定窗口计数，超限返回 429。与凭据级 `rate_limit_*` 相互独立。
    #[serde(default)]
    pub ingress_rate_limit_per_min: u32,

    /// 请求体最大字节数（默认 50MiB）。防止超大 body 打爆内存。
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,

    // ============ 主动 token 预刷新（批次4.4）============
    /// 是否启用后台主动预刷新：在 token 过期前后台刷新，削掉首个请求的刷新延迟与突发。
    /// 默认 true。关闭后退回原有「请求时按需刷新」行为。
    #[serde(default = "default_true")]
    pub proactive_token_refresh: bool,

    /// 预刷新提前量（分钟）：token 剩余有效期低于此值即后台刷新（默认 10）。
    #[serde(default = "default_refresh_lead_minutes")]
    pub token_refresh_lead_minutes: i64,

    /// 后台预刷新扫描间隔（秒，默认 60）。
    #[serde(default = "default_refresh_interval_secs")]
    pub token_refresh_interval_secs: u64,

    // ============ Admin UI 登录页 ============
    /// 登录页是否显示随机背景图（默认 true）。关闭后登录页用纯渐变背景，
    /// 不再请求外部图源。此项立即生效（登录页每次加载时读取）。
    #[serde(default = "default_true")]
    pub login_background_enabled: bool,

    // ============ 余额同步（A6：温和的周期性余额刷新）============
    /// 后台温和刷新余额缓存的间隔（秒）。`0` = 禁用（默认 1800 = 30 分钟）。
    ///
    /// 封号红线：绝不在启动/挂载时批量拉；后台任务用长间隔、逐个刷新且每个之间
    /// 留有间隔（分散节奏），只刷未禁用的号，仅更新缓存供展示，绝不做主动禁用。
    /// 安全第一：可保守设为 0 禁用，由用户在设置里自行开启。
    ///
    /// 热重载批次(HR)会把它做成可热调，本批先作为需重启字段。
    #[serde(default = "default_balance_refresh_interval_secs")]
    pub balance_refresh_interval_secs: u64,

    // ============ 凭据回收站 ============
    /// 回收站保留天数：软删除的凭据超过此天数后由后台任务彻底清理（默认 30）。
    /// `0` 表示永久保留，不自动清理。
    #[serde(default = "default_trash_retention_days")]
    pub trash_retention_days: u32,

    /// 配置文件路径（运行时元数据，不写入 JSON）
    #[serde(skip)]
    config_path: Option<PathBuf>,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_kiro_version() -> String {
    "0.11.107".to_string()
}

fn default_system_version() -> String {
    const SYSTEM_VERSIONS: &[&str] = &["darwin#24.6.0", "win32#10.0.22631"];
    SYSTEM_VERSIONS[fastrand::usize(..SYSTEM_VERSIONS.len())].to_string()
}

fn default_node_version() -> String {
    "22.22.0".to_string()
}

fn default_count_tokens_auth_type() -> String {
    "x-api-key".to_string()
}

fn default_tls_backend() -> TlsBackend {
    TlsBackend::Rustls
}

fn default_load_balancing_mode() -> String {
    "priority".to_string()
}

fn default_extract_thinking() -> bool {
    true
}

fn default_endpoint() -> String {
    crate::kiro::endpoint::ide::IDE_ENDPOINT_NAME.to_string()
}

fn default_cooldown_enabled() -> bool {
    true
}

fn default_affinity_enabled() -> bool {
    true
}

fn default_rate_limit_daily() -> u32 {
    500
}

fn default_rate_limit_min_interval_ms() -> u64 {
    1000
}

fn default_prompt_cache_enabled() -> bool {
    true
}

fn default_prompt_cache_ttl_seconds() -> u64 {
    3600
}

fn default_usage_enabled() -> bool {
    true
}

fn default_usage_data_dir() -> String {
    "data/usage".to_string()
}

fn default_usage_retention_days() -> i64 {
    30
}

fn default_max_body_bytes() -> usize {
    50 * 1024 * 1024
}

fn default_true() -> bool {
    true
}

fn default_refresh_lead_minutes() -> i64 {
    10
}

fn default_refresh_interval_secs() -> u64 {
    60
}

fn default_trash_retention_days() -> u32 {
    30
}

fn default_balance_refresh_interval_secs() -> u64 {
    1800
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            region: default_region(),
            auth_region: None,
            api_region: None,
            kiro_version: default_kiro_version(),
            machine_id: None,
            api_key: None,
            system_version: default_system_version(),
            node_version: default_node_version(),
            tls_backend: default_tls_backend(),
            count_tokens_api_url: None,
            count_tokens_api_key: None,
            count_tokens_auth_type: default_count_tokens_auth_type(),
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            admin_api_key: None,
            load_balancing_mode: default_load_balancing_mode(),
            extract_thinking: default_extract_thinking(),
            default_endpoint: default_endpoint(),
            endpoints: HashMap::new(),
            cooldown_enabled: default_cooldown_enabled(),
            rate_limit_enabled: false,
            rate_limit_daily_max: default_rate_limit_daily(),
            rate_limit_min_interval_ms: default_rate_limit_min_interval_ms(),
            affinity_enabled: default_affinity_enabled(),
            prompt_cache_enabled: default_prompt_cache_enabled(),
            prompt_cache_ttl_seconds: default_prompt_cache_ttl_seconds(),
            callback_base_url: None,
            usage_enabled: default_usage_enabled(),
            usage_data_dir: default_usage_data_dir(),
            usage_retention_days: default_usage_retention_days(),
            cors_allowed_origins: Vec::new(),
            ip_allowlist: Vec::new(),
            trust_forwarded_header: false,
            ingress_rate_limit_per_min: 0,
            max_body_bytes: default_max_body_bytes(),
            proactive_token_refresh: default_true(),
            token_refresh_lead_minutes: default_refresh_lead_minutes(),
            token_refresh_interval_secs: default_refresh_interval_secs(),
            login_background_enabled: default_true(),
            trash_retention_days: default_trash_retention_days(),
            balance_refresh_interval_secs: default_balance_refresh_interval_secs(),
            config_path: None,
        }
    }
}

impl Config {
    /// 获取默认配置文件路径
    pub fn default_config_path() -> &'static str {
        "config.json"
    }

    /// 获取有效的 Auth Region（用于 Token 刷新）
    /// 优先使用 auth_region，未配置时回退到 region
    pub fn effective_auth_region(&self) -> &str {
        self.auth_region.as_deref().unwrap_or(&self.region)
    }

    /// 获取有效的 API Region（用于 API 请求）
    /// 优先使用 api_region，未配置时回退到 region
    pub fn effective_api_region(&self) -> &str {
        self.api_region.as_deref().unwrap_or(&self.region)
    }

    /// 从文件加载配置
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            // 配置文件不存在，返回默认配置
            let mut config = Self::default();
            config.config_path = Some(path.to_path_buf());
            return Ok(config);
        }

        let content = fs::read_to_string(path)?;
        let mut config: Config = serde_json::from_str(&content)?;
        config.config_path = Some(path.to_path_buf());
        Ok(config)
    }

    /// 获取配置文件路径（如果有）
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// 将当前配置写回原始配置文件
    pub fn save(&self) -> anyhow::Result<()> {
        let path = self
            .config_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，无法保存配置"))?;

        let content = serde_json::to_string_pretty(self).context("序列化配置失败")?;
        fs::write(path, content).with_context(|| format!("写入配置文件失败: {}", path.display()))?;
        Ok(())
    }
}
