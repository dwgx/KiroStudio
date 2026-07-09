//! 网页上号（Social OAuth）会话管理
//!
//! 维护进行中的登录会话，支持两种回调模式：
//! - **本地模式**：后端在本机临时 TCP 端口接收 OAuth 回调（仅本机浏览器可达）。
//! - **远程模式**：配置了 `callbackBaseUrl` 时，浏览器回调打到公网路由
//!   `{callbackBaseUrl}/api/admin/auth/callback`，由 [`deliver_callback`] 投递。
//!
//! 流程：start → 返回 portal_url + session_id；前端轮询 poll；
//! 收到回调后用 code 换 token，并把新凭据加入池。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use tokio::sync::oneshot;

use crate::http_client::ProxyConfig;
use crate::kiro::auth::social::{
    self, OAuthCallbackData, ServerHandle,
};
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;

/// 会话最长存活（秒）：超时自动清理，避免内存泄漏
const SESSION_TTL_SECS: u64 = 600;

/// 单个登录会话的内部状态
struct SocialSession {
    /// PKCE verifier，换 token 时用
    code_verifier: String,
    /// 防 CSRF 的 state
    state: String,
    /// 回调用的 redirect_uri（本地为 http://127.0.0.1:port，远程为公网地址）
    redirect_uri: String,
    /// 换 token 用的 auth endpoint
    auth_endpoint: String,
    /// 新凭据的优先级
    priority: u32,
    /// 出站代理（换 token 的 OAuth 请求用；自定义优先，否则继承全局）
    proxy: Option<ProxyConfig>,
    /// 上号时**显式填入**的自定义代理（仅此项持久化到新凭据；global 回落不持久化，
    /// 避免把全局代理钉死在凭据上、后续改全局失效）。None=没填，继承全局。
    custom_proxy: Option<ProxyConfig>,
    /// 创建时间，用于 TTL 清理
    created_at: Instant,
    /// 本地模式：持有回调服务器句柄，drop 时释放端口
    _server_handle: Option<ServerHandle>,
    /// 接收回调数据的接收端（poll 时尝试读取）
    callback_rx: Mutex<Option<oneshot::Receiver<OAuthCallbackData>>>,
    /// 远程模式：暂存的发送端，由公网回调路由投递
    remote_tx: Mutex<Option<oneshot::Sender<OAuthCallbackData>>>,
}

/// 网页上号会话管理器
pub struct SocialLoginManager {
    token_manager: Arc<MultiTokenManager>,
    sessions: Mutex<HashMap<String, Arc<SocialSession>>>,
}

/// start 的返回：给前端展示
pub struct StartResult {
    pub session_id: String,
    pub portal_url: String,
}

/// poll 的返回状态
pub enum PollResult {
    /// 等待用户在浏览器完成登录
    Pending,
    /// 登录完成，凭据已加入池
    Done {
        credential_id: u64,
        email: Option<String>,
    },
    /// 登录失败
    Error(String),
}

impl SocialLoginManager {
    pub fn new(token_manager: Arc<MultiTokenManager>) -> Self {
        Self {
            token_manager,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// 发起一次网页上号
    ///
    /// - `priority`：新凭据优先级
    /// - `proxy_url`：可选，自定义出站代理（不填继承全局）
    pub fn start(
        &self,
        priority: u32,
        proxy_url: Option<String>,
    ) -> anyhow::Result<StartResult> {
        self.cleanup_expired();

        let config = self.token_manager.config();
        let auth_endpoint = social::KIRO_AUTH_ENDPOINT.to_string();
        let global_proxy = config.proxy_url.as_ref().map(|url| {
            let mut p = ProxyConfig::new(url);
            if let (Some(u), Some(pw)) = (&config.proxy_username, &config.proxy_password) {
                p = p.with_auth(u, pw);
            }
            p
        });
        // 用户可能把账密内嵌进 URL（socks5://user:pass@host:port）——拆出账密，否则登录
        // 走的代理无法认证。拆分后干净 URL + 独立账密交给 ProxyConfig。
        let custom_proxy = proxy_url.map(|u| {
            let (clean, user, pass) = crate::http_client::split_proxy_credentials(&u);
            let mut p = ProxyConfig::new(clean);
            if let (Some(user), Some(pass)) = (user, pass) {
                p = p.with_auth(user, pass);
            }
            p
        });
        // OAuth 请求用的出站代理：自定义优先，否则继承全局。
        let proxy = custom_proxy.clone().or(global_proxy);

        let (code_verifier, code_challenge) = social::generate_pkce();
        let state = uuid::Uuid::new_v4().to_string();
        let session_id = uuid::Uuid::new_v4().to_string();

        // 回调模式：配了 callbackBaseUrl → 远程；否则本地起临时端口
        let callback_base = config
            .callback_base_url
            .as_ref()
            .map(|s| s.trim_end_matches('/').to_string());

        let (redirect_uri, server_handle, remote_tx, callback_rx) = match callback_base {
            Some(base) => {
                // 远程：回调打到公网路由，暂存 Sender 等待投递
                let (tx, rx) = oneshot::channel::<OAuthCallbackData>();
                let redirect = format!("{}/api/admin/auth/callback", base);
                (redirect, None, Some(tx), rx)
            }
            None => {
                // 本地：起临时 TCP 端口，浏览器在本机回调
                let (tx, rx) = oneshot::channel::<OAuthCallbackData>();
                let (port, handle) = social::start_callback_server(tx)?;
                (
                    format!("http://127.0.0.1:{}", port),
                    Some(handle),
                    None,
                    rx,
                )
            }
        };

        let portal_url = social::build_portal_url(&state, &code_challenge, &redirect_uri);

        let session = Arc::new(SocialSession {
            code_verifier,
            state,
            redirect_uri,
            auth_endpoint,
            priority,
            proxy,
            custom_proxy,
            created_at: Instant::now(),
            _server_handle: server_handle,
            callback_rx: Mutex::new(Some(callback_rx)),
            remote_tx: Mutex::new(remote_tx),
        });

        self.sessions
            .lock()
            .insert(session_id.clone(), session);

        Ok(StartResult {
            session_id,
            portal_url,
        })
    }

    /// 轮询会话状态；完成时换 token 并落库
    pub async fn poll(&self, session_id: &str) -> PollResult {
        let session = match self.sessions.lock().get(session_id).cloned() {
            Some(s) => s,
            None => return PollResult::Error("登录会话不存在或已过期".to_string()),
        };

        // 尝试非阻塞读取回调
        let callback = {
            let mut guard = session.callback_rx.lock();
            match guard.as_mut() {
                Some(rx) => match rx.try_recv() {
                    Ok(data) => {
                        *guard = None; // 取走后置空
                        Some(data)
                    }
                    Err(oneshot::error::TryRecvError::Empty) => None,
                    Err(oneshot::error::TryRecvError::Closed) => {
                        return PollResult::Error("回调通道已关闭".to_string());
                    }
                },
                None => None, // 已经被取走（可能正在处理）
            }
        };

        let callback = match callback {
            Some(c) => c,
            None => return PollResult::Pending,
        };

        // CSRF 校验
        if !callback.state.is_empty() && callback.state != session.state {
            self.sessions.lock().remove(session_id);
            return PollResult::Error("OAuth state 校验失败（可能的 CSRF）".to_string());
        }

        // 用 code 换 token
        let config = self.token_manager.config().clone();
        let token = match social::exchange_code_for_token(
            &session.auth_endpoint,
            &callback.code,
            &session.code_verifier,
            &session.redirect_uri,
            &config,
            session.proxy.as_ref(),
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                self.sessions.lock().remove(session_id);
                return PollResult::Error(format!("Token 交换失败: {}", e));
            }
        };

        let refresh_token = match token.refresh_token {
            Some(t) => t,
            None => {
                self.sessions.lock().remove(session_id);
                return PollResult::Error("Kiro 未返回 refresh_token".to_string());
            }
        };

        // 上号时**显式填的**代理要持久化到该凭据（否则登录用了代理、之后请求却回落全局代理，
        // 表现为“上号输入的代理没有自动加入”）。只持久化 custom_proxy（用户填的），
        // 不持久化 global 回落（避免把全局代理钉死在凭据上）。已是拆好账密的 ProxyConfig。
        let (proxy_url, proxy_username, proxy_password) = match &session.custom_proxy {
            Some(p) => (
                Some(p.url.clone()),
                p.username.clone(),
                p.password.clone(),
            ),
            None => (None, None, None),
        };

        // 构建并加入凭据池
        let new_cred = KiroCredentials {
            id: None,
            access_token: Some(token.access_token),
            refresh_token: Some(refresh_token),
            profile_arn: token.profile_arn,
            expires_at: token.expires_at,
            auth_method: Some("social".to_string()),
            client_id: None,
            client_secret: None,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            priority: session.priority,
            rpm_limit: None,
            region: None,
            auth_region: None,
            api_region: None,
            machine_id: None,
            email: None,
            name: None,
            subscription_title: None,
            proxy_url,
            proxy_username,
            proxy_password,
            disabled: false,
            kiro_api_key: None,
            endpoint: None,
        };

        let credential_id = match self.token_manager.add_credential(new_cred).await {
            Ok(id) => id,
            Err(e) => {
                self.sessions.lock().remove(session_id);
                return PollResult::Error(format!("凭据加入失败: {}", e));
            }
        };

        // 主动拉订阅等级（失败不阻断）
        if let Err(e) = self
            .token_manager
            .get_usage_limits_for(credential_id)
            .await
        {
            tracing::warn!("网页上号后获取订阅等级失败（不影响上号）: {}", e);
        }

        let email = {
            let snap = self.token_manager.snapshot();
            snap.entries
                .into_iter()
                .find(|e| e.id == credential_id)
                .and_then(|e| e.email)
        };

        self.sessions.lock().remove(session_id);
        tracing::info!("网页上号成功，新凭据 #{}", credential_id);
        PollResult::Done {
            credential_id,
            email,
        }
    }

    /// 远程模式：公网回调路由收到 code 后投递给对应会话
    ///
    /// 通过 state 匹配会话（远程回调没有 session_id，用 OAuth state 关联）。
    pub fn deliver_callback(&self, data: OAuthCallbackData) -> bool {
        let sessions = self.sessions.lock();
        for session in sessions.values() {
            if session.state == data.state {
                if let Some(tx) = session.remote_tx.lock().take() {
                    let _ = tx.send(data);
                    return true;
                }
                return false;
            }
        }
        false
    }

    /// 清理过期会话
    fn cleanup_expired(&self) {
        let mut sessions = self.sessions.lock();
        sessions.retain(|_, s| s.created_at.elapsed().as_secs() < SESSION_TTL_SECS);
    }
}
