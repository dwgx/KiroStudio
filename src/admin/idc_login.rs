//! IDC (AWS IAM Identity Center) 上号会话管理
//!
//! 对接 AWS SSO-OIDC device authorization grant：
//! start → 返回 verificationUri + userCode + session_id
//! poll  → 轮询 CreateToken 直到用户授权完成
//!
//! 与 social_login.rs 平行的 session manager 模式。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

use crate::http_client::ProxyConfig;
use crate::kiro::auth::idc::{self, DeviceAuth, OidcClient};
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;

const SESSION_TTL_SECS: u64 = 900;

struct IdcSession {
    region: String,
    oidc_client: OidcClient,
    device_auth: DeviceAuth,
    priority: u32,
    proxy: Option<ProxyConfig>,
    created_at: Instant,
}

pub struct IdcLoginManager {
    token_manager: Arc<MultiTokenManager>,
    sessions: Mutex<HashMap<String, Arc<IdcSession>>>,
}

pub struct IdcStartResult {
    pub session_id: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub user_code: String,
    pub expires_in: u64,
}

pub enum IdcPollResult {
    Pending,
    Done { credential_id: u64 },
    Expired,
    Error(String),
}

impl IdcLoginManager {
    pub fn new(token_manager: Arc<MultiTokenManager>) -> Self {
        Self {
            token_manager,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub async fn start(
        &self,
        start_url: &str,
        region: &str,
        priority: u32,
        proxy_url: Option<String>,
    ) -> anyhow::Result<IdcStartResult> {
        self.cleanup_expired();

        let config = self.token_manager.config();
        let global_proxy = config.proxy_url.as_ref().map(|url| {
            let mut p = ProxyConfig::new(url);
            if let (Some(u), Some(pw)) = (&config.proxy_username, &config.proxy_password) {
                p = p.with_auth(u, pw);
            }
            p
        });
        let proxy = proxy_url.map(|u| ProxyConfig::new(&u)).or(global_proxy);

        let oidc_client =
            idc::register_client(region, &config, proxy.as_ref()).await?;

        let device_auth = idc::start_device_authorization(
            region,
            &oidc_client,
            start_url,
            &config,
            proxy.as_ref(),
        )
        .await?;

        let session_id = uuid::Uuid::new_v4().to_string();
        let result = IdcStartResult {
            session_id: session_id.clone(),
            verification_uri: device_auth.verification_uri.clone(),
            verification_uri_complete: device_auth.verification_uri_complete.clone(),
            user_code: device_auth.user_code.clone(),
            expires_in: device_auth.expires_in,
        };

        let session = Arc::new(IdcSession {
            region: region.to_string(),
            oidc_client,
            device_auth,
            priority,
            proxy,
            created_at: Instant::now(),
        });

        self.sessions.lock().insert(session_id, session);
        Ok(result)
    }

    pub async fn poll(&self, session_id: &str) -> IdcPollResult {
        let session = match self.sessions.lock().get(session_id).cloned() {
            Some(s) => s,
            None => return IdcPollResult::Error("IDC 登录会话不存在或已过期".to_string()),
        };

        // 检查是否超时
        if session.created_at.elapsed().as_secs() > session.device_auth.expires_in {
            self.sessions.lock().remove(session_id);
            return IdcPollResult::Expired;
        }

        let config = self.token_manager.config();
        let result = idc::poll_create_token(
            &session.region,
            &session.oidc_client,
            &session.device_auth.device_code,
            &config,
            session.proxy.as_ref(),
        )
        .await;

        match result {
            idc::PollTokenResult::Pending => IdcPollResult::Pending,
            idc::PollTokenResult::Expired => {
                self.sessions.lock().remove(session_id);
                IdcPollResult::Expired
            }
            idc::PollTokenResult::Error(e) => {
                self.sessions.lock().remove(session_id);
                IdcPollResult::Error(e)
            }
            idc::PollTokenResult::Done(token) => {
                let new_cred = KiroCredentials {
                    id: None,
                    access_token: Some(token.access_token),
                    refresh_token: token.refresh_token,
                    profile_arn: None,
                    expires_at: Some(
                        chrono::Utc::now()
                            .checked_add_signed(chrono::Duration::seconds(token.expires_in as i64))
                            .unwrap_or_else(chrono::Utc::now)
                            .to_rfc3339(),
                    ),
                    auth_method: Some("idc".to_string()),
                    client_id: Some(session.oidc_client.client_id.clone()),
                    client_secret: Some(session.oidc_client.client_secret.clone()),
                    token_endpoint: None,
                    issuer_url: None,
                    scopes: None,
                    priority: session.priority,
                    region: Some(session.region.clone()),
                    auth_region: Some(session.region.clone()),
                    api_region: None,
                    machine_id: None,
                    email: None,
                    subscription_title: None,
                    proxy_url: None,
                    proxy_username: None,
                    proxy_password: None,
                    disabled: false,
                    kiro_api_key: None,
                    endpoint: None,
                };

                let credential_id = match self.token_manager.add_credential(new_cred).await {
                    Ok(id) => id,
                    Err(e) => {
                        self.sessions.lock().remove(session_id);
                        return IdcPollResult::Error(format!("凭据加入失败: {}", e));
                    }
                };

                if let Err(e) = self
                    .token_manager
                    .get_usage_limits_for(credential_id)
                    .await
                {
                    tracing::warn!("IDC 上号后获取订阅等级失败: {}", e);
                }

                self.sessions.lock().remove(session_id);
                tracing::info!("IDC 上号成功，新凭据 #{}", credential_id);
                IdcPollResult::Done { credential_id }
            }
        }
    }

    fn cleanup_expired(&self) {
        let mut sessions = self.sessions.lock();
        sessions.retain(|_, s| s.created_at.elapsed().as_secs() < SESSION_TTL_SECS);
    }
}
