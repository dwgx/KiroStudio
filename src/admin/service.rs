//! Admin API 业务逻辑服务

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;

use super::error::AdminServiceError;
use super::idc_login::IdcLoginManager;
use super::social_login::SocialLoginManager;
pub use super::social_login::{PollResult, StartResult};
use super::idc_login::{IdcPollResult, IdcStartResult};
use crate::kiro::auth::social::OAuthCallbackData;
use super::types::{
    AddCredentialRequest, AddCredentialResponse, BalanceResponse, ConfigSnapshotResponse,
    CredentialStatusItem, CredentialsStatusResponse, LoadBalancingModeResponse,
    SetLoadBalancingModeRequest, UpdateConfigRequest, UpdateConfigResponse,
};

/// 余额缓存过期时间（秒），5 分钟
const BALANCE_CACHE_TTL_SECS: i64 = 300;

/// 缓存的余额条目（含时间戳）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedBalance {
    /// 缓存时间（Unix 秒）
    cached_at: f64,
    /// 缓存的余额数据
    data: BalanceResponse,
}

/// Admin 服务
///
/// 封装所有 Admin API 的业务逻辑
pub struct AdminService {
    token_manager: Arc<MultiTokenManager>,
    balance_cache: Mutex<HashMap<u64, CachedBalance>>,
    cache_path: Option<PathBuf>,
    /// 已注册的端点名称集合（用于 add_credential 校验）
    known_endpoints: HashSet<String>,
    /// 网页上号会话管理器
    social_login: SocialLoginManager,
    /// IDC 上号会话管理器
    idc_login: IdcLoginManager,
}

impl AdminService {
    pub fn new(
        token_manager: Arc<MultiTokenManager>,
        known_endpoints: impl IntoIterator<Item = String>,
    ) -> Self {
        let cache_path = token_manager
            .cache_dir()
            .map(|d| d.join("kiro_balance_cache.json"));

        let balance_cache = Self::load_balance_cache_from(&cache_path);

        Self {
            social_login: SocialLoginManager::new(token_manager.clone()),
            idc_login: IdcLoginManager::new(token_manager.clone()),
            token_manager,
            balance_cache: Mutex::new(balance_cache),
            cache_path,
            known_endpoints: known_endpoints.into_iter().collect(),
        }
    }

    /// 发起网页上号，返回 portal_url + session_id
    pub fn start_social_login(
        &self,
        priority: u32,
        proxy_url: Option<String>,
    ) -> Result<StartResult, AdminServiceError> {
        self.social_login
            .start(priority, proxy_url)
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))
    }

    /// 轮询网页上号会话状态
    pub async fn poll_social_login(&self, session_id: &str) -> PollResult {
        self.social_login.poll(session_id).await
    }

    /// 远程模式：公网回调路由投递 OAuth 回调
    pub fn deliver_social_callback(&self, data: OAuthCallbackData) -> bool {
        self.social_login.deliver_callback(data)
    }

    /// 发起 IDC (AWS SSO) 上号
    pub async fn start_idc_login(
        &self,
        start_url: &str,
        region: &str,
        priority: u32,
        proxy_url: Option<String>,
    ) -> Result<IdcStartResult, AdminServiceError> {
        self.idc_login
            .start(start_url, region, priority, proxy_url)
            .await
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))
    }

    /// 轮询 IDC 上号会话
    pub async fn poll_idc_login(&self, session_id: &str) -> IdcPollResult {
        self.idc_login.poll(session_id).await
    }

    /// 获取所有凭据状态
    pub fn get_all_credentials(&self) -> CredentialsStatusResponse {
        let snapshot = self.token_manager.snapshot();
        let default_endpoint = self.token_manager.config().default_endpoint.clone();

        let mut credentials: Vec<CredentialStatusItem> = snapshot
            .entries
            .into_iter()
            .map(|entry| CredentialStatusItem {
                id: entry.id,
                priority: entry.priority,
                disabled: entry.disabled,
                failure_count: entry.failure_count,
                is_current: entry.id == snapshot.current_id,
                expires_at: entry.expires_at,
                auth_method: entry.auth_method,
                has_profile_arn: entry.has_profile_arn,
                refresh_token_hash: entry.refresh_token_hash,
                api_key_hash: entry.api_key_hash,
                masked_api_key: entry.masked_api_key,
                email: entry.email,
                success_count: entry.success_count,
                last_used_at: entry.last_used_at.clone(),
                has_proxy: entry.has_proxy,
                proxy_url: entry.proxy_url,
                refresh_failure_count: entry.refresh_failure_count,
                disabled_reason: entry.disabled_reason,
                endpoint: entry.endpoint.unwrap_or_else(|| default_endpoint.clone()),
            })
            .collect();

        // 按优先级排序（数字越小优先级越高）
        credentials.sort_by_key(|c| c.priority);

        CredentialsStatusResponse {
            total: snapshot.total,
            available: snapshot.available,
            current_id: snapshot.current_id,
            credentials,
        }
    }

    /// 设置凭据禁用状态
    pub fn set_disabled(&self, id: u64, disabled: bool) -> Result<(), AdminServiceError> {
        // 先获取当前凭据 ID，用于判断是否需要切换
        let snapshot = self.token_manager.snapshot();
        let current_id = snapshot.current_id;

        self.token_manager
            .set_disabled(id, disabled)
            .map_err(|e| self.classify_error(e, id))?;

        // 只有禁用的是当前凭据时才尝试切换到下一个
        if disabled && id == current_id {
            let _ = self.token_manager.switch_to_next();
        }
        Ok(())
    }

    /// 设置凭据优先级
    pub fn set_priority(&self, id: u64, priority: u32) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_priority(id, priority)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 重置失败计数并重新启用
    pub fn reset_and_enable(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .reset_and_enable(id)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 获取凭据余额（带缓存）
    pub async fn get_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        // 先查缓存
        {
            let cache = self.balance_cache.lock();
            if let Some(cached) = cache.get(&id) {
                let now = Utc::now().timestamp() as f64;
                if (now - cached.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    tracing::debug!("凭据 #{} 余额命中缓存", id);
                    return Ok(cached.data.clone());
                }
            }
        }

        // 缓存未命中或已过期，从上游获取
        let balance = self.fetch_balance(id).await?;

        // 更新缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.insert(
                id,
                CachedBalance {
                    cached_at: Utc::now().timestamp() as f64,
                    data: balance.clone(),
                },
            );
        }
        self.save_balance_cache();

        Ok(balance)
    }

    /// 从上游获取余额（无缓存）
    async fn fetch_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        let usage = self
            .token_manager
            .get_usage_limits_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        let current_usage = usage.current_usage();
        let usage_limit = usage.usage_limit();
        let remaining = (usage_limit - current_usage).max(0.0);
        let usage_percentage = if usage_limit > 0.0 {
            (current_usage / usage_limit * 100.0).min(100.0)
        } else {
            0.0
        };

        Ok(BalanceResponse {
            id,
            subscription_title: usage.subscription_title().map(|s| s.to_string()),
            current_usage,
            usage_limit,
            remaining,
            usage_percentage,
            next_reset_at: usage.next_date_reset,
        })
    }

    /// 添加新凭据
    pub async fn add_credential(
        &self,
        req: AddCredentialRequest,
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        // 校验端点名：未指定则默认合法，指定则必须已注册
        if let Some(ref name) = req.endpoint {
            if !self.known_endpoints.contains(name) {
                let mut known: Vec<&str> =
                    self.known_endpoints.iter().map(|s| s.as_str()).collect();
                known.sort();
                return Err(AdminServiceError::InvalidCredential(format!(
                    "未知端点 \"{}\"，已注册端点: {:?}",
                    name, known
                )));
            }
        }

        // 构建凭据对象
        let email = req.email.clone();
        let new_cred = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: req.refresh_token,
            profile_arn: None,
            expires_at: None,
            auth_method: Some(req.auth_method),
            client_id: req.client_id,
            client_secret: req.client_secret,
            priority: req.priority,
            region: req.region,
            auth_region: req.auth_region,
            api_region: req.api_region,
            machine_id: req.machine_id,
            email: req.email,
            subscription_title: None, // 将在首次获取使用额度时自动更新
            proxy_url: req.proxy_url,
            proxy_username: req.proxy_username,
            proxy_password: req.proxy_password,
            disabled: false, // 新添加的凭据默认启用
            kiro_api_key: req.kiro_api_key,
            endpoint: req.endpoint,
        };

        // 调用 token_manager 添加凭据
        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| self.classify_add_error(e))?;

        // 主动获取订阅等级，避免首次请求时 Free 账号绕过 Opus 模型过滤
        if let Err(e) = self.token_manager.get_usage_limits_for(credential_id).await {
            tracing::warn!("添加凭据后获取订阅等级失败（不影响凭据添加）: {}", e);
        }

        Ok(AddCredentialResponse {
            success: true,
            message: format!("凭据添加成功，ID: {}", credential_id),
            credential_id,
            email,
        })
    }

    /// 删除凭据
    pub fn delete_credential(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .delete_credential(id)
            .map_err(|e| self.classify_delete_error(e, id))?;

        // 清理已删除凭据的余额缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(&id);
        }
        self.save_balance_cache();

        Ok(())
    }

    /// 获取负载均衡模式
    pub fn get_load_balancing_mode(&self) -> LoadBalancingModeResponse {
        LoadBalancingModeResponse {
            mode: self.token_manager.get_load_balancing_mode(),
        }
    }

    /// 获取服务端配置快照（敏感字段脱敏）
    pub fn get_config_snapshot(&self) -> ConfigSnapshotResponse {
        let config = self.token_manager.config();

        let tls_backend = match config.tls_backend {
            crate::model::config::TlsBackend::Rustls => "rustls",
            crate::model::config::TlsBackend::NativeTls => "native-tls",
        }
        .to_string();

        let mut endpoint_names: Vec<String> = self.known_endpoints.iter().cloned().collect();
        endpoint_names.sort();

        let callback_mode = if config.callback_base_url.is_some() {
            "remote"
        } else {
            "local"
        }
        .to_string();

        ConfigSnapshotResponse {
            host: config.host.clone(),
            port: config.port,
            region: config.region.clone(),
            kiro_version: config.kiro_version.clone(),
            system_version: config.system_version.clone(),
            node_version: config.node_version.clone(),
            tls_backend,
            load_balancing_mode: self.token_manager.get_load_balancing_mode(),
            default_endpoint: config.default_endpoint.clone(),
            endpoint_names,
            extract_thinking: config.extract_thinking,
            cooldown_enabled: config.cooldown_enabled,
            rate_limit_enabled: config.rate_limit_enabled,
            rate_limit_daily_max: config.rate_limit_daily_max,
            rate_limit_min_interval_ms: config.rate_limit_min_interval_ms,
            affinity_enabled: config.affinity_enabled,
            has_proxy: config.proxy_url.is_some(),
            proxy_url: config.proxy_url.clone(),
            has_admin_key: config
                .admin_api_key
                .as_ref()
                .map(|k| !k.trim().is_empty())
                .unwrap_or(false),
            callback_mode,
            callback_base_url: config.callback_base_url.clone(),
            cors_allowed_origins: config.cors_allowed_origins.clone(),
            ip_allowlist: config.ip_allowlist.clone(),
            trust_forwarded_header: config.trust_forwarded_header,
            ingress_rate_limit_per_min: config.ingress_rate_limit_per_min,
            max_body_bytes: config.max_body_bytes,
            proactive_token_refresh: config.proactive_token_refresh,
            token_refresh_lead_minutes: config.token_refresh_lead_minutes,
            token_refresh_interval_secs: config.token_refresh_interval_secs,
            config_path: config
                .config_path()
                .map(|p| p.display().to_string()),
        }
    }

    /// 设置负载均衡模式
    pub fn set_load_balancing_mode(
        &self,
        req: SetLoadBalancingModeRequest,
    ) -> Result<LoadBalancingModeResponse, AdminServiceError> {
        // 验证模式值
        if req.mode != "priority" && req.mode != "balanced" {
            return Err(AdminServiceError::InvalidCredential(
                "mode 必须是 'priority' 或 'balanced'".to_string(),
            ));
        }

        self.token_manager
            .set_load_balancing_mode(req.mode.clone())
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        Ok(LoadBalancingModeResponse { mode: req.mode })
    }

    /// 更新服务端配置并持久化到 config.json
    ///
    /// 仅提交的字段被修改。`loadBalancingMode` 立即生效，其余字段需重启进程后生效，
    /// 通过响应的 `restart_fields` 告知前端。敏感字段不在此开放。
    pub fn update_config(
        &self,
        req: UpdateConfigRequest,
    ) -> Result<UpdateConfigResponse, AdminServiceError> {
        let config_path = self
            .token_manager
            .config()
            .config_path()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| {
                AdminServiceError::InternalError(
                    "配置文件路径未知，无法保存配置".to_string(),
                )
            })?;

        // 从磁盘重新加载，避免覆盖进程外的改动
        let mut config = crate::model::config::Config::load(&config_path)
            .map_err(|e| AdminServiceError::InternalError(format!("加载配置失败: {}", e)))?;

        let mut restart_fields: Vec<String> = Vec::new();

        // —— 需重启生效的字段 ——
        if let Some(v) = req.host {
            let v = v.trim().to_string();
            if v.is_empty() {
                return Err(AdminServiceError::InvalidCredential(
                    "host 不能为空".to_string(),
                ));
            }
            if v != config.host {
                config.host = v;
                restart_fields.push("host".into());
            }
        }
        if let Some(v) = req.port {
            if v == 0 {
                return Err(AdminServiceError::InvalidCredential(
                    "port 必须是 1-65535".to_string(),
                ));
            }
            if v != config.port {
                config.port = v;
                restart_fields.push("port".into());
            }
        }
        if let Some(v) = req.region {
            let v = v.trim().to_string();
            if !v.is_empty() && v != config.region {
                config.region = v;
                restart_fields.push("region".into());
            }
        }
        if let Some(v) = req.kiro_version {
            let v = v.trim().to_string();
            if !v.is_empty() && v != config.kiro_version {
                config.kiro_version = v;
                restart_fields.push("kiroVersion".into());
            }
        }
        if let Some(v) = req.system_version {
            let v = v.trim().to_string();
            if !v.is_empty() && v != config.system_version {
                config.system_version = v;
                restart_fields.push("systemVersion".into());
            }
        }
        if let Some(v) = req.node_version {
            let v = v.trim().to_string();
            if !v.is_empty() && v != config.node_version {
                config.node_version = v;
                restart_fields.push("nodeVersion".into());
            }
        }
        if let Some(v) = req.tls_backend {
            let backend = match v.as_str() {
                "rustls" => crate::model::config::TlsBackend::Rustls,
                "native-tls" => crate::model::config::TlsBackend::NativeTls,
                _ => {
                    return Err(AdminServiceError::InvalidCredential(
                        "tlsBackend 必须是 'rustls' 或 'native-tls'".to_string(),
                    ));
                }
            };
            if backend != config.tls_backend {
                config.tls_backend = backend;
                restart_fields.push("tlsBackend".into());
            }
        }
        if let Some(v) = req.default_endpoint {
            let v = v.trim().to_string();
            if !v.is_empty() && v != config.default_endpoint {
                if !self.known_endpoints.is_empty() && !self.known_endpoints.contains(&v) {
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "未知 endpoint '{}'，可用: {}",
                        v,
                        {
                            let mut names: Vec<_> = self.known_endpoints.iter().cloned().collect();
                            names.sort();
                            names.join(", ")
                        }
                    )));
                }
                config.default_endpoint = v;
                restart_fields.push("defaultEndpoint".into());
            }
        }
        if let Some(v) = req.extract_thinking {
            if v != config.extract_thinking {
                config.extract_thinking = v;
                restart_fields.push("extractThinking".into());
            }
        }
        if let Some(v) = req.cooldown_enabled {
            if v != config.cooldown_enabled {
                config.cooldown_enabled = v;
                restart_fields.push("cooldownEnabled".into());
            }
        }
        if let Some(v) = req.rate_limit_enabled {
            if v != config.rate_limit_enabled {
                config.rate_limit_enabled = v;
                restart_fields.push("rateLimitEnabled".into());
            }
        }
        if let Some(v) = req.rate_limit_daily_max {
            if v != config.rate_limit_daily_max {
                config.rate_limit_daily_max = v;
                restart_fields.push("rateLimitDailyMax".into());
            }
        }
        if let Some(v) = req.rate_limit_min_interval_ms {
            if v != config.rate_limit_min_interval_ms {
                config.rate_limit_min_interval_ms = v;
                restart_fields.push("rateLimitMinIntervalMs".into());
            }
        }
        if let Some(v) = req.affinity_enabled {
            if v != config.affinity_enabled {
                config.affinity_enabled = v;
                restart_fields.push("affinityEnabled".into());
            }
        }
        if let Some(v) = req.proxy_url {
            let trimmed = v.trim();
            let new_val = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
            if new_val != config.proxy_url {
                config.proxy_url = new_val;
                restart_fields.push("proxyUrl".into());
            }
        }
        if let Some(v) = req.callback_base_url {
            let trimmed = v.trim();
            let new_val = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.trim_end_matches('/').to_string())
            };
            if new_val != config.callback_base_url {
                config.callback_base_url = new_val;
                restart_fields.push("callbackBaseUrl".into());
            }
        }

        // —— 反代安全（批次3，均需重启生效）——
        if let Some(v) = req.cors_allowed_origins {
            // 去空白、去空项，保持整表替换语义
            let cleaned: Vec<String> =
                v.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            if cleaned != config.cors_allowed_origins {
                config.cors_allowed_origins = cleaned;
                restart_fields.push("corsAllowedOrigins".into());
            }
        }
        if let Some(v) = req.ip_allowlist {
            let cleaned: Vec<String> =
                v.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            // 校验每条 CIDR 合法，非法直接拒绝（避免静默丢弃导致白名单形同虚设）
            for entry in &cleaned {
                if let Err(e) = crate::common::security::validate_cidr(entry) {
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "ipAllowlist 条目 '{entry}' 非法: {e}"
                    )));
                }
            }
            if cleaned != config.ip_allowlist {
                config.ip_allowlist = cleaned;
                restart_fields.push("ipAllowlist".into());
            }
        }
        if let Some(v) = req.trust_forwarded_header {
            if v != config.trust_forwarded_header {
                config.trust_forwarded_header = v;
                restart_fields.push("trustForwardedHeader".into());
            }
        }
        if let Some(v) = req.ingress_rate_limit_per_min {
            if v != config.ingress_rate_limit_per_min {
                config.ingress_rate_limit_per_min = v;
                restart_fields.push("ingressRateLimitPerMin".into());
            }
        }
        if let Some(v) = req.max_body_bytes {
            if v != config.max_body_bytes {
                config.max_body_bytes = v;
                restart_fields.push("maxBodyBytes".into());
            }
        }

        // —— 主动 token 预刷新（批次4.4，需重启生效）——
        if let Some(v) = req.proactive_token_refresh {
            if v != config.proactive_token_refresh {
                config.proactive_token_refresh = v;
                restart_fields.push("proactiveTokenRefresh".into());
            }
        }
        if let Some(v) = req.token_refresh_lead_minutes {
            if v != config.token_refresh_lead_minutes {
                config.token_refresh_lead_minutes = v;
                restart_fields.push("tokenRefreshLeadMinutes".into());
            }
        }
        if let Some(v) = req.token_refresh_interval_secs {
            if v != config.token_refresh_interval_secs {
                config.token_refresh_interval_secs = v;
                restart_fields.push("tokenRefreshIntervalSecs".into());
            }
        }

        // —— 立即生效的字段：负载均衡模式 ——
        let mut lb_changed = false;
        if let Some(mode) = req.load_balancing_mode {
            if mode != "priority" && mode != "balanced" {
                return Err(AdminServiceError::InvalidCredential(
                    "loadBalancingMode 必须是 'priority' 或 'balanced'".to_string(),
                ));
            }
            config.load_balancing_mode = mode;
            lb_changed = true;
        }

        // 持久化（一次写盘）
        config
            .save()
            .map_err(|e| AdminServiceError::InternalError(format!("保存配置失败: {}", e)))?;

        // 负载均衡模式立即应用到运行时
        if lb_changed {
            self.token_manager
                .set_load_balancing_mode(config.load_balancing_mode.clone())
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
        }

        let restart_required = !restart_fields.is_empty();
        let message = if restart_required {
            format!(
                "已保存。{} 个字段需重启服务后生效。",
                restart_fields.len()
            )
        } else if lb_changed {
            "已保存并立即生效。".to_string()
        } else {
            "无改动。".to_string()
        };

        tracing::info!(
            "配置已更新（需重启字段: {:?}, 负载均衡变更: {}）",
            restart_fields,
            lb_changed
        );

        Ok(UpdateConfigResponse {
            success: true,
            message,
            restart_required,
            restart_fields,
        })
    }

    /// 强制刷新指定凭据的 Token
    pub async fn force_refresh_token(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .force_refresh_token_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))
    }

    /// 深度验活：通过实际 API 调用检测账号 suspend 状态
    pub async fn deep_verify_credential(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .deep_verify_credential(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))
    }

    // ============ 余额缓存持久化 ============

    fn load_balance_cache_from(cache_path: &Option<PathBuf>) -> HashMap<u64, CachedBalance> {
        let path = match cache_path {
            Some(p) => p,
            None => return HashMap::new(),
        };

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };

        // 文件中使用字符串 key 以兼容 JSON 格式
        let map: HashMap<String, CachedBalance> = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("解析余额缓存失败，将忽略: {}", e);
                return HashMap::new();
            }
        };

        let now = Utc::now().timestamp() as f64;
        map.into_iter()
            .filter_map(|(k, v)| {
                let id = k.parse::<u64>().ok()?;
                // 丢弃超过 TTL 的条目
                if (now - v.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    Some((id, v))
                } else {
                    None
                }
            })
            .collect()
    }

    fn save_balance_cache(&self) {
        let path = match &self.cache_path {
            Some(p) => p,
            None => return,
        };

        // 持有锁期间完成序列化和写入，防止并发损坏
        let cache = self.balance_cache.lock();
        let map: HashMap<String, &CachedBalance> =
            cache.iter().map(|(k, v)| (k.to_string(), v)).collect();

        match serde_json::to_string_pretty(&map) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!("保存余额缓存失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("序列化余额缓存失败: {}", e),
        }
    }

    // ============ 错误分类 ============

    /// 分类简单操作错误（set_disabled, set_priority, reset_and_enable）
    fn classify_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类余额查询错误（可能涉及上游 API 调用）
    fn classify_balance_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();

        // 1. 凭据不存在
        if msg.contains("不存在") {
            return AdminServiceError::NotFound { id };
        }

        // 2. API Key 凭据不支持刷新：客户端请求错误，映射为 400
        if msg.contains("API Key 凭据不支持刷新") {
            return AdminServiceError::InvalidCredential(msg);
        }

        // 3. 上游服务错误特征：HTTP 响应错误或网络错误
        let is_upstream_error =
            // HTTP 响应错误（来自 refresh_*_token 的错误消息）
            msg.contains("凭证已过期或无效") ||
            msg.contains("权限不足") ||
            msg.contains("已被限流") ||
            msg.contains("服务器错误") ||
            msg.contains("Token 刷新失败") ||
            msg.contains("暂时不可用") ||
            // 网络错误（reqwest 错误）
            msg.contains("error trying to connect") ||
            msg.contains("connection") ||
            msg.contains("timeout") ||
            msg.contains("timed out");

        if is_upstream_error {
            AdminServiceError::UpstreamError(msg)
        } else {
            // 4. 默认归类为内部错误（本地验证失败、配置错误等）
            // 包括：缺少 refreshToken、refreshToken 已被截断、无法生成 machineId 等
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类添加凭据错误
    fn classify_add_error(&self, e: anyhow::Error) -> AdminServiceError {
        let msg = e.to_string();

        // 凭据验证失败（refreshToken 无效、格式错误等）
        let is_invalid_credential = msg.contains("缺少 refreshToken")
            || msg.contains("refreshToken 为空")
            || msg.contains("refreshToken 已被截断")
            || msg.contains("凭据已存在")
            || msg.contains("refreshToken 重复")
            || msg.contains("kiroApiKey 重复")
            || msg.contains("缺少 kiroApiKey")
            || msg.contains("kiroApiKey 为空")
            || msg.contains("凭证已过期或无效")
            || msg.contains("权限不足")
            || msg.contains("已被限流");

        if is_invalid_credential {
            AdminServiceError::InvalidCredential(msg)
        } else if msg.contains("error trying to connect")
            || msg.contains("connection")
            || msg.contains("timeout")
        {
            AdminServiceError::UpstreamError(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类删除凭据错误
    fn classify_delete_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else if msg.contains("只能删除已禁用的凭据") || msg.contains("请先禁用凭据") {
            AdminServiceError::InvalidCredential(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }
}
