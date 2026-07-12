//! Microsoft 365 / Entra ID（Azure AD）外部 IdP 后台上号
//!
//! 背景：Kiro portal 的 OAuth redirect 被硬锁到 `http://localhost:3128`，且外部
//! IdP 登录是**双段** authorization-code + PKCE 流程（portal 先回调带 IdP
//! descriptor，再跳 IdP 拿真正的 code）。服务器绑 loopback 用户浏览器访问不到，
//! 设备码流 portal 又不支持——因此这里采用**服务端算 + 浏览器地址栏 URL 回粘**的
//! 引导流（参考 CLIProxyAPI 的 ParseOAuthCallback 思路，扩到双段）：
//!
//! 1. `start`：生成 PKCE(leg1)+state，返回 `app.kiro.dev/signin?redirect_uri=…:3128…`
//!    给前端。用户在浏览器登录 M365，浏览器会落到**打不开的**
//!    `localhost:3128/signin/callback?issuer_url=…&client_id=…`。
//! 2. `submit_leg1`：用户把上一步地址栏整串 URL 粘回。服务端解析 IdP descriptor，
//!    SSRF 白名单校验 issuer，OIDC discovery 拿 authorize/token 端点，生成 leg2
//!    PKCE+state，返回 IdP authorize URL。
//! 3. 用户打开 IdP authorize URL 授权，浏览器落到打不开的
//!    `localhost:3128/oauth/callback?code=…&state=…`，再把整串 URL 粘回。
//! 4. `submit_leg2`：解析 code+state（校验匹配），到 IdP token 端点换 token，
//!    `ListAvailableProfiles`（带 `TokenType: EXTERNAL_IDP`）解析 profileArn，
//!    组 external_idp 凭据入池。
//!
//! 全程服务器算，用户机器上不跑任何程序、不需要 SSH 隧道。移植自
//! `D:\Project\kiro-login-helper-main\kiro-login-helper.py` 的外部 IdP 段逻辑，
//! 复用 [`crate::kiro::token_manager::validate_microsoft_token_endpoint`] 做 SSRF 白名单。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, bail, Context};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::http_client::{build_client, ProxyConfig};
use crate::kiro::auth::social::generate_pkce;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::{validate_microsoft_token_endpoint, MultiTokenManager};

/// Kiro 托管登录页（用户在浏览器打开）。
const KIRO_SIGNIN_URL: &str = "https://app.kiro.dev/signin";
/// portal 校验并回跳的固定 loopback redirect（硬锁，不可改）。
const SOCIAL_REDIRECT_URI: &str = "http://localhost:3128";
/// 外部 IdP 段（leg2）的回调路径，与 social 段区分。
const OAUTH_CALLBACK_PATH: &str = "/oauth/callback";
/// 与 Kiro IDE 一致的 client tag。
const SOCIAL_REDIRECT_FROM: &str = "KiroIDE";
/// 会话最长存活（秒）：留足用户在浏览器两段登录的时间。
const SESSION_TTL_SECS: u64 = 900;
/// ListAvailableProfiles 的 X-Amz-Target。
const LIST_PROFILES_TARGET: &str = "AmazonCodeWhispererService.ListAvailableProfiles";

/// 单个外部 IdP 上号会话。leg1 完成后填充 `leg2`。
struct ExternalIdpSession {
    /// leg1（portal 段）的 anti-CSRF state。portal 只回外部 IdP descriptor（不回
    /// 可交换的 social code），故这里无需保存 leg1 的 PKCE verifier。
    leg1_state: String,
    priority: u32,
    /// OAuth 请求用的出站代理（自定义优先，否则继承全局）。
    proxy: Option<ProxyConfig>,
    /// 上号时**显式填的**代理（仅此项持久化到新凭据；global 回落不持久化）。
    custom_proxy: Option<ProxyConfig>,
    /// leg2 上下文（提交 leg1 后填充）：IdP authorize/token 端点 + PKCE。
    leg2: Option<Leg2Context>,
    /// leg2 换到 token + 探测到的多 region profile 列表（等用户选定 region 后建号）。
    /// 多 profile 时前端弹窗选一个,select 阶段用暂存的 token 直接建号(避免二次 exchange,
    /// 授权码一次性)。
    pending: Option<PendingLeg2>,
    created_at: Instant,
}

/// leg2 换到 token 后暂存,等 select 阶段选定 profile 建号。
#[derive(Clone)]
struct PendingLeg2 {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
    client_id: String,
    token_endpoint: String,
    issuer_url: String,
    scopes: String,
    /// 该账号在候选 region 探测到的全部 profile(去重)。select 的 arn 必须在此列表内(防呆+防伪)。
    profiles: Vec<ProfileOption>,
}

/// 一个可选 profile:ARN + 解析出的 region + account(供前端展示与用户选择)。
#[derive(Clone, Debug)]
pub struct ProfileOption {
    pub arn: String,
    pub region: String,
    pub account: String,
    /// 该 region profile 是否真开通可用（getUsageLimits 验活 2xx）。
    /// 实测同一 M365 账号 us-east-1 返回 403 FEATURE_NOT_SUPPORTED、eu-central-1 200，
    /// 故列 profile 后逐个验活，前端只让用户选真能用的（或 0 个可用时标注知情）。
    pub usable: bool,
    /// 验活拿到的订阅标题（如 "KIRO POWER"），供前端展示与择优。
    pub subscription_title: Option<String>,
}

/// 外部 IdP 段（leg2）上下文，`submit_leg1` 成功后建立。
#[derive(Clone)]
struct Leg2Context {
    state: String,
    verifier: String,
    token_endpoint: String,
    issuer_url: String,
    client_id: String,
    scopes: String,
    redirect_uri: String,
}

pub struct ExternalIdpLoginManager {
    token_manager: Arc<MultiTokenManager>,
    sessions: Mutex<HashMap<String, ExternalIdpSession>>,
}

/// `start` 结果：session_id + 供用户打开的 Kiro 登录 URL。
pub struct ExternalIdpStartResult {
    pub session_id: String,
    pub signin_url: String,
}

/// `submit_leg1` 结果：供用户打开的 IdP authorize URL。
pub struct ExternalIdpLeg1Result {
    pub authorize_url: String,
}

/// `submit_leg2` 结果:换 token + 探测 profile。
/// - `profiles` 多个 → 前端弹窗选,随后调 select 建号(`credential_id` 为 None)。
/// - `profiles` 恰 1 个 → 后端已自动建号,`credential_id` 有值,前端不弹窗直接完成。
pub struct ExternalIdpLeg2Result {
    pub credential_id: Option<u64>,
    pub profiles: Vec<ProfileOption>,
}

/// `submit_leg2_select` 结果:选定 profile 后入池的新凭据 ID。
pub struct ExternalIdpSelectResult {
    pub credential_id: u64,
}

impl ExternalIdpLoginManager {
    pub fn new(token_manager: Arc<MultiTokenManager>) -> Self {
        Self {
            token_manager,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// 组装代理。返回 `(effective, custom)`：
    /// - effective：OAuth 请求实际用的代理（自定义优先，否则回退全局）。
    /// - custom：上号时**显式填的**代理（拆掉 URL 内嵌账密后），仅此项持久化到新凭据。
    ///
    /// 用户可能把账密内嵌 URL（socks5://user:pass@host:port）——拆出账密独立设置，
    /// 否则 SOCKS5 无法认证；同时避免密码明文留 URL。
    fn resolve_proxy(&self, proxy_url: Option<String>) -> (Option<ProxyConfig>, Option<ProxyConfig>) {
        let config = self.token_manager.config();
        let global = config.proxy_url.as_ref().map(|url| {
            let mut p = ProxyConfig::new(url);
            if let (Some(u), Some(pw)) = (&config.proxy_username, &config.proxy_password) {
                p = p.with_auth(u, pw);
            }
            p
        });
        let custom = proxy_url.filter(|u| !u.trim().is_empty()).map(|u| {
            let (clean, user, pass) = crate::http_client::split_proxy_credentials(&u);
            let mut p = ProxyConfig::new(clean);
            if let (Some(user), Some(pass)) = (user, pass) {
                p = p.with_auth(user, pass);
            }
            p
        });
        (custom.clone().or(global), custom)
    }

    /// 第 1 步：生成 PKCE+state，返回 session_id + Kiro signin URL。
    pub fn start(
        &self,
        priority: u32,
        proxy_url: Option<String>,
    ) -> anyhow::Result<ExternalIdpStartResult> {
        self.cleanup_expired();

        let (_verifier, challenge) = generate_pkce();
        let state = random_state();
        let (proxy, custom_proxy) = self.resolve_proxy(proxy_url);

        // 与 helper.py 一致：signin URL 带 PKCE challenge/state + 固定 loopback redirect。
        let signin_url = format!(
            "{}?state={}&code_challenge={}&code_challenge_method=S256&redirect_uri={}&redirect_from={}",
            KIRO_SIGNIN_URL,
            urlencoding::encode(&state),
            urlencoding::encode(&challenge),
            urlencoding::encode(SOCIAL_REDIRECT_URI),
            SOCIAL_REDIRECT_FROM,
        );

        let session_id = uuid::Uuid::new_v4().to_string();
        self.sessions.lock().insert(
            session_id.clone(),
            ExternalIdpSession {
                leg1_state: state,
                priority,
                proxy,
                custom_proxy,
                leg2: None,
                pending: None,
                created_at: Instant::now(),
            },
        );

        Ok(ExternalIdpStartResult {
            session_id,
            signin_url,
        })
    }

    /// 第 2 步：用户粘回 portal 回调 URL（含 IdP descriptor）。
    /// 解析 descriptor → SSRF 校验 → OIDC discovery → 生成 leg2 PKCE → 返回 IdP authorize URL。
    pub async fn submit_leg1(
        &self,
        session_id: &str,
        pasted_url: &str,
    ) -> anyhow::Result<ExternalIdpLeg1Result> {
        let (leg1_state, proxy) = {
            let sessions = self.sessions.lock();
            let s = sessions
                .get(session_id)
                .ok_or_else(|| anyhow!("外部 IdP 上号会话不存在或已过期"))?;
            if s.created_at.elapsed().as_secs() > SESSION_TTL_SECS {
                bail!("外部 IdP 上号会话已超时，请重新发起");
            }
            (s.leg1_state.clone(), s.proxy.clone())
        };

        let params = parse_query_from_pasted(pasted_url);

        // portal 段可能带 state（CSRF）；带了就校验，避免粘错 URL。
        if params
            .get("state")
            .filter(|v| !v.is_empty())
            .is_some_and(|cb_state| *cb_state != leg1_state)
        {
            bail!("回调 state 不匹配，请确认粘贴的是本次登录跳转后的地址栏 URL");
        }
        if let Some(err) = params.get("error").filter(|v| !v.is_empty()) {
            let desc = params.get("error_description").cloned().unwrap_or_default();
            bail!("Kiro 登录返回错误: {} {}", err, desc);
        }

        let issuer_url = params
            .get("issuer_url")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!("粘贴的 URL 里没有 issuer_url——该账号可能不是外部 IdP（Microsoft）账号，或粘错了地址")
            })?;
        let client_id = params
            .get("client_id")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("外部 IdP descriptor 缺少 client_id"))?;
        let scopes = params.get("scopes").map(|s| s.trim().to_string()).unwrap_or_default();
        let login_hint = params.get("login_hint").map(|s| s.trim().to_string()).unwrap_or_default();

        // SSRF：issuer 来自 portal 回调（可被诱导），先按 Microsoft 登录域白名单校验，
        // 再 discover；discover 拿到的 authorize/token 端点也逐一校验。
        validate_microsoft_token_endpoint(&issuer_url)
            .context("外部 IdP issuer_url 未通过 Microsoft 登录域白名单校验")?;

        let (auth_endpoint, token_endpoint) =
            oidc_discover(&issuer_url, proxy.as_ref(), self.token_manager.config().tls_backend).await?;
        validate_microsoft_token_endpoint(&auth_endpoint)
            .context("OIDC discovery 的 authorization_endpoint 未通过白名单校验")?;
        validate_microsoft_token_endpoint(&token_endpoint)
            .context("OIDC discovery 的 token_endpoint 未通过白名单校验")?;

        let (verifier, challenge) = generate_pkce();
        let leg2_state = random_state();
        let redirect_uri = format!("{}{}", SOCIAL_REDIRECT_URI, OAUTH_CALLBACK_PATH);
        let authorize_url = build_authorize_url(
            &auth_endpoint,
            &client_id,
            &redirect_uri,
            &scopes,
            &challenge,
            &leg2_state,
            &login_hint,
        );

        {
            let mut sessions = self.sessions.lock();
            let s = sessions
                .get_mut(session_id)
                .ok_or_else(|| anyhow!("外部 IdP 上号会话不存在或已过期"))?;
            s.leg2 = Some(Leg2Context {
                state: leg2_state,
                verifier,
                token_endpoint,
                issuer_url,
                client_id,
                scopes,
                redirect_uri,
            });
        }

        Ok(ExternalIdpLeg1Result { authorize_url })
    }

    /// 第 3 步：用户粘回 IdP 授权回调 URL（含 code+state）。
    /// 校验 state → 换 token → 解析 profileArn → 组 external_idp 凭据入池。
    pub async fn submit_leg2(
        &self,
        session_id: &str,
        pasted_url: &str,
    ) -> anyhow::Result<ExternalIdpLeg2Result> {
        // priority/custom_proxy 在 build_and_add_credential 里从 session 重新读,此处只需 leg2+proxy。
        let (leg2, proxy) = {
            let sessions = self.sessions.lock();
            let s = sessions
                .get(session_id)
                .ok_or_else(|| anyhow!("外部 IdP 上号会话不存在或已过期"))?;
            if s.created_at.elapsed().as_secs() > SESSION_TTL_SECS {
                bail!("外部 IdP 上号会话已超时，请重新发起");
            }
            let leg2 = s
                .leg2
                .clone()
                .ok_or_else(|| anyhow!("请先完成第 2 步（粘贴登录跳转后的地址）"))?;
            (leg2, s.proxy.clone())
        };

        let params = parse_query_from_pasted(pasted_url);
        if let Some(err) = params.get("error").filter(|v| !v.is_empty()) {
            let desc = params.get("error_description").cloned().unwrap_or_default();
            bail!("外部 IdP 授权返回错误: {} {}", err, desc);
        }
        let cb_state = params.get("state").map(|s| s.trim()).unwrap_or("");
        if cb_state.is_empty() || cb_state != leg2.state {
            bail!("回调 state 不匹配，请确认粘贴的是授权后跳转的地址栏 URL");
        }
        let code = params
            .get("code")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("粘贴的 URL 里没有授权码 code"))?;

        let tls = self.token_manager.config().tls_backend;

        // token 端点已在 leg1 白名单校验过，这里再校验一次（防会话被并发改写）。
        validate_microsoft_token_endpoint(&leg2.token_endpoint)?;
        let token = exchange_code(&leg2, &code, proxy.as_ref(), tls).await?;

        // 外部 IdP token 自身不带 profileArn，必须走 ListAvailableProfiles 解析
        // （带 TokenType: EXTERNAL_IDP，否则上游静默返回空列表）。
        // ⭐多 region:每个 region 端点只返回本 region 的 profile(实测),故**逐 region 探测合并**,
        // 把该账号在候选 region 的全部 profile 都拿出来让用户选(对齐官方 kiro-cli 的选择体验)。
        let mut profiles = list_all_available_profiles(
            &token.access_token,
            &self.token_manager.config().kiro_version,
            proxy.as_ref(),
            tls,
        )
        .await
        .context("解析 CodeWhisperer profileArn 失败（该 M365 账号可能未开通 Kiro/CodeWhisperer）")?;

        if profiles.is_empty() {
            bail!("该账号在候选 region 均无可用 profile（可能未开通 Kiro/CodeWhisperer）");
        }

        // ⭐验活（本 bug 核心）：逐个 profile 真发 getUsageLimits，标出哪些 region 真开通。
        // 实测同账号 us-east-1 → 403 FEATURE_NOT_SUPPORTED、eu-central-1 → 200 KIRO POWER，
        // 只列出/自动选 usable 的，杜绝「导入成功但一直 403」的坑。
        probe_profiles_usable(
            &mut profiles,
            &token.access_token,
            &self.token_manager.config(),
            proxy.as_ref(),
        )
        .await;
        // usable 排前面（select/展示都优先真能用的）。
        profiles.sort_by(|a, b| b.usable.cmp(&a.usable));
        let usable_count = profiles.iter().filter(|p| p.usable).count();

        // 暂存 token + profiles 到 session,供 select 阶段建号(避免二次 exchange,授权码一次性)。
        {
            let mut sessions = self.sessions.lock();
            if let Some(s) = sessions.get_mut(session_id) {
                s.pending = Some(PendingLeg2 {
                    access_token: token.access_token.clone(),
                    refresh_token: token.refresh_token.clone(),
                    expires_in: token.expires_in,
                    client_id: leg2.client_id.clone(),
                    token_endpoint: leg2.token_endpoint.clone(),
                    issuer_url: leg2.issuer_url.clone(),
                    scopes: leg2.scopes.clone(),
                    profiles: profiles.clone(),
                });
            }
        }

        // 恰 1 个 usable:无需打扰用户,直接选它建号并返回 credential_id(前端不弹窗)。
        if usable_count == 1 {
            let chosen = profiles
                .iter()
                .find(|p| p.usable)
                .expect("usable_count==1 保证存在")
                .arn
                .clone();
            let cred_id = self.build_and_add_credential(session_id, &chosen).await?;
            return Ok(ExternalIdpLeg2Result {
                credential_id: Some(cred_id),
                profiles,
            });
        }

        // 多个 usable → 返回列表给前端弹窗选(只应选 usable 的),select 阶段再建号。
        // 0 个 usable(全 FEATURE_NOT_SUPPORTED/验活失败) → 也返回全部(标 usable=false),
        //   让用户知情该账号目前无任何 region 开通(不擅自建一个必 403 的号)。
        tracing::info!(
            "外部 IdP 账号 {} 个 region profile（{} 个可用）,返回前端选择",
            profiles.len(),
            usable_count
        );
        Ok(ExternalIdpLeg2Result {
            credential_id: None,
            profiles,
        })
    }

    /// leg2 选定阶段:用户从多 region profile 里选一个,用暂存 token 建号入池。
    /// arn 必须在 session 暂存的 profiles 列表内(防呆+防伪造)。
    pub async fn submit_leg2_select(
        &self,
        session_id: &str,
        arn: &str,
    ) -> anyhow::Result<ExternalIdpSelectResult> {
        let cred_id = self.build_and_add_credential(session_id, arn).await?;
        Ok(ExternalIdpSelectResult { credential_id: cred_id })
    }

    /// 用 session 暂存的 token + 指定 arn 构建凭据并入池。arn 必须在暂存 profiles 内。
    /// region/auth_region 全部取 **arn 内的 region**(防呆铁律:region 与 ARN 物理绑定)。
    async fn build_and_add_credential(
        &self,
        session_id: &str,
        arn: &str,
    ) -> anyhow::Result<u64> {
        let (pending, priority, custom_proxy) = {
            let sessions = self.sessions.lock();
            let s = sessions
                .get(session_id)
                .ok_or_else(|| anyhow!("外部 IdP 上号会话不存在或已过期"))?;
            let pending = s
                .pending
                .clone()
                .ok_or_else(|| anyhow!("请先完成第 3 步（换取 token 并列出 profile）"))?;
            (pending, s.priority, s.custom_proxy.clone())
        };

        // 防呆+防伪:选的 arn 必须确在本账号探测到的 profiles 列表里。
        let mut chosen = pending
            .profiles
            .iter()
            .find(|p| p.arn == arn.trim())
            .ok_or_else(|| anyhow!("选中的 profile 不在该账号可用列表内（arn 不匹配）"))?
            .clone();

        // 自动纠正（J）：若选中的 profile 验活不可用（FEATURE_NOT_SUPPORTED 等），但同账号存在
        // 其它已验活可用的 region profile，则自动改用可用的那个——绝不建一个必 403 的死号。
        // （单 usable 时 submit_leg2 已自动选好；此处兜住「多 profile 弹窗里误选了不可用项」。）
        if !chosen.usable {
            if let Some(usable) = pending.profiles.iter().find(|p| p.usable) {
                tracing::info!(
                    "选中的 region profile {} 未开通（FEATURE_NOT_SUPPORTED）,自动改用可用的 {}",
                    chosen.arn,
                    usable.arn
                );
                chosen = usable.clone();
            } else {
                bail!(
                    "该账号所有 region profile 目前均未开通（FEATURE_NOT_SUPPORTED）,暂无法建号——请确认账号已开通 Kiro 订阅"
                );
            }
        }
        // region 以 arn 内解析为准(chosen.region 已是白名单校验过的 arn region)。
        let region = chosen.region.clone();

        let expires_at = (pending.expires_in > 0).then(|| {
            chrono::Utc::now()
                .checked_add_signed(chrono::Duration::seconds(pending.expires_in))
                .unwrap_or_else(chrono::Utc::now)
                .to_rfc3339()
        });

        let new_cred = KiroCredentials {
            access_token: Some(pending.access_token.clone()),
            refresh_token: (!pending.refresh_token.is_empty())
                .then(|| pending.refresh_token.clone()),
            profile_arn: Some(chosen.arn.clone()),
            expires_at,
            auth_method: Some("external_idp".to_string()),
            client_id: Some(pending.client_id.clone()),
            token_endpoint: Some(pending.token_endpoint.clone()),
            issuer_url: Some(pending.issuer_url.clone()),
            scopes: (!pending.scopes.trim().is_empty()).then(|| pending.scopes.clone()),
            priority,
            region: Some(region.clone()),
            auth_region: Some(region.clone()),
            proxy_url: custom_proxy.as_ref().map(|p| p.url.clone()),
            proxy_username: custom_proxy.as_ref().and_then(|p| p.username.clone()),
            proxy_password: custom_proxy.as_ref().and_then(|p| p.password.clone()),
            ..Default::default()
        };

        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .context("外部 IdP 凭据加入池失败")?;

        // 顺带拉一次订阅等级（失败不阻断，仅告警）。
        if let Err(e) = self.token_manager.get_usage_limits_for(credential_id).await {
            tracing::warn!("外部 IdP 上号后获取订阅等级失败: {}", e);
        }

        // 新号自动初始化(异步):刷 token + 解析 profileArn(external_idp 尤其需要真实 arn,否则 400/403)。
        self.token_manager.spawn_initial_refresh(credential_id);

        self.sessions.lock().remove(session_id);
        tracing::info!(
            "外部 IdP（Microsoft）上号成功，新凭据 #{}（region={}）",
            credential_id,
            region
        );
        Ok(credential_id)
    }

    fn cleanup_expired(&self) {
        self.sessions
            .lock()
            .retain(|_, s| s.created_at.elapsed().as_secs() < SESSION_TTL_SECS);
    }
}

/// 换到的 IdP token（外部 IdP 段公有客户端 authorization_code grant）。
struct ExchangedToken {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

/// 生成 URL-safe 随机 state（两段 UUIDv4 拼成 32 字节的十六进制，天然 URL-safe）。
fn random_state() -> String {
    let uuid_a = uuid::Uuid::new_v4();
    let uuid_b = uuid::Uuid::new_v4();
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(uuid_a.as_bytes());
    bytes[16..].copy_from_slice(uuid_b.as_bytes());
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

/// 从用户粘贴的整串 URL / query / 片段里解析出 query 参数（容错：
/// 接受 `http://localhost:3128/...?a=b`、`/...?a=b`、`?a=b`、`a=b&c=d`）。
fn parse_query_from_pasted(pasted: &str) -> HashMap<String, String> {
    let s = pasted.trim();
    // 取 '?' 之后；没有 '?' 则整串当 query（用户可能只粘了 query 部分）。
    let query = match s.split_once('?') {
        Some((_, q)) => q,
        None => s,
    };
    // 去掉可能存在的 fragment（#...）。
    let query = query.split('#').next().unwrap_or(query);
    query
        .split('&')
        .filter_map(|pair| {
            let mut it = pair.splitn(2, '=');
            let k = it.next()?.trim();
            if k.is_empty() {
                return None;
            }
            let v = it.next().unwrap_or("");
            let v = percent_decode(&v.replace('+', " "));
            Some((k.to_string(), v))
        })
        .collect()
}

/// 轻量 percent-decode（不引额外依赖，复用 urlencoding）。
fn percent_decode(s: &str) -> String {
    urlencoding::decode(s)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| s.to_string())
}

/// OIDC discovery：拉 `{issuer}/.well-known/openid-configuration`，返回
/// (authorization_endpoint, token_endpoint)。不跟随重定向（防被 discovery 主机
/// 反跳到内网）。
async fn oidc_discover(
    issuer_url: &str,
    proxy: Option<&ProxyConfig>,
    tls: crate::model::config::TlsBackend,
) -> anyhow::Result<(String, String)> {
    let doc_url = format!(
        "{}/.well-known/openid-configuration",
        issuer_url.trim_end_matches('/')
    );
    // 禁止跟随重定向。
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none());
    let client = match tls {
        crate::model::config::TlsBackend::Rustls => client.use_rustls_tls(),
        crate::model::config::TlsBackend::NativeTls => client,
    };
    let client = if let Some(p) = proxy {
        let mut rp = reqwest::Proxy::all(&p.url)?;
        if let (Some(u), Some(pw)) = (&p.username, &p.password) {
            rp = rp.basic_auth(u, pw);
        }
        client.proxy(rp)
    } else {
        client
    }
    .build()?;

    let resp = client
        .get(&doc_url)
        .header("Accept", "application/json")
        .send()
        .await
        .context("OIDC discovery 请求失败")?;
    if !resp.status().is_success() {
        bail!("OIDC discovery 返回 {}", resp.status());
    }
    let doc: serde_json::Value = resp.json().await.context("OIDC discovery 响应解析失败")?;
    let auth_endpoint = doc
        .get("authorization_endpoint")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("OIDC discovery 缺少 authorization_endpoint"))?;
    let token_endpoint = doc
        .get("token_endpoint")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("OIDC discovery 缺少 token_endpoint"))?;
    Ok((auth_endpoint, token_endpoint))
}

/// 构建外部 IdP 段浏览器授权 URL（authorization_code + PKCE）。
fn build_authorize_url(
    auth_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    scopes: &str,
    challenge: &str,
    state: &str,
    login_hint: &str,
) -> String {
    let mut url = format!(
        "{}?client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&response_mode=query&state={}",
        auth_endpoint,
        urlencoding::encode(client_id),
        urlencoding::encode(redirect_uri),
        urlencoding::encode(scopes),
        urlencoding::encode(challenge),
        urlencoding::encode(state),
    );
    if !login_hint.trim().is_empty() {
        url.push_str(&format!("&login_hint={}", urlencoding::encode(login_hint)));
    }
    url
}

/// 到 IdP token 端点换 token（公有客户端 authorization_code grant，form 编码）。
async fn exchange_code(
    leg2: &Leg2Context,
    code: &str,
    proxy: Option<&ProxyConfig>,
    tls: crate::model::config::TlsBackend,
) -> anyhow::Result<ExchangedToken> {
    let client = build_client(proxy, 30, tls)?;
    let mut form = vec![
        ("client_id", leg2.client_id.as_str()),
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", leg2.redirect_uri.as_str()),
        ("code_verifier", leg2.verifier.as_str()),
    ];
    if !leg2.scopes.trim().is_empty() {
        form.push(("scope", leg2.scopes.as_str()));
    }

    let resp = client
        .post(&leg2.token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await
        .context("外部 IdP token 交换请求失败")?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    let access = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if !status.is_success() || access.is_empty() {
        let err = body.get("error").and_then(|v| v.as_str()).unwrap_or("");
        let desc = body
            .get("error_description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        bail!("外部 IdP token 交换失败（{}）: {} {}", status, err, desc);
    }
    Ok(ExchangedToken {
        access_token: access,
        refresh_token: body
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        expires_in: body.get("expires_in").and_then(|v| v.as_i64()).unwrap_or(0),
    })
}

/// 解析外部 IdP token 的 CodeWhisperer profileArn。
/// **必须**带 `TokenType: EXTERNAL_IDP`，否则上游静默返回空列表。
/// 打**单个 region** 的 `q.{region}.amazonaws.com` ListAvailableProfiles,返回该 region 的
/// 全部 profile arn(实测每端点只返回本 region 的 profile)。空列表返回 Ok(vec![]),不报错
/// (交由上层跨 region 合并后统一判空)。
async fn list_region_profile_arns(
    access_token: &str,
    region: &str,
    kiro_version: &str,
    proxy: Option<&ProxyConfig>,
    tls: crate::model::config::TlsBackend,
) -> anyhow::Result<Vec<String>> {
    if access_token.trim().is_empty() {
        bail!("access_token 为空");
    }
    let host = format!("q.{}.amazonaws.com", region);
    let url = format!("https://{}/", host);
    // machine_id 从 access_token 派生（此时凭据尚未入池），与 helper.py 一致。
    let machine_id = sha256_hex(access_token.as_bytes());
    let user_agent = format!(
        "aws-sdk-js/1.0.0 ua/2.1 os/windows#10.0.26200 lang/js md/nodejs#22.21.1 api/codewhispererruntime#1.0.0 m/N,E KiroIDE-{}-{}",
        kiro_version, machine_id
    );
    let x_amz_user_agent = format!("aws-sdk-js/1.0.0 KiroIDE-{}-{}", kiro_version, machine_id);

    let client = build_client(proxy, 30, tls)?;
    let resp = client
        .post(&url)
        .header("Content-Type", "application/x-amz-json-1.0")
        .header("Accept", "application/x-amz-json-1.0")
        .header("Authorization", format!("Bearer {}", access_token))
        .header("X-Amz-Target", LIST_PROFILES_TARGET)
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("x-amzn-kiro-agent-mode", "vibe")
        .header("x-amzn-codewhisperer-optout", "true")
        .header("User-Agent", &user_agent)
        .header("x-amz-user-agent", &x_amz_user_agent)
        .header("TokenType", "EXTERNAL_IDP")
        .header("host", &host)
        .body("{}")
        .send()
        .await
        .context("ListAvailableProfiles 请求失败")?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        bail!("ListAvailableProfiles 返回 {}", status);
    }
    let arns = body
        .get("profiles")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.get("arn").and_then(|a| a.as_str()))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(arns)
}

/// 跨候选 region 探测该账号的**全部** profile,合并去重为 `Vec<ProfileOption>`。
///
/// 每个 region 端点只返回本 region 的 profile(实测 2026-07-12),故必须逐 region 打。
/// 候选 region = `PROFILE_PROBE_REGIONS`(us-east-1 / eu-central-1,dwgx 定的最普遍两个)。
/// 并发探测,单 region 失败不阻断其它;按 arn 去重。region 由 arn 严格解析(白名单校验)。
async fn list_all_available_profiles(
    access_token: &str,
    kiro_version: &str,
    proxy: Option<&ProxyConfig>,
    tls: crate::model::config::TlsBackend,
) -> anyhow::Result<Vec<ProfileOption>> {
    use crate::kiro::token_manager::PROFILE_PROBE_REGIONS;
    // 并发打所有候选 region。
    let futs = PROFILE_PROBE_REGIONS.iter().map(|region| {
        let region = region.to_string();
        async move {
            match list_region_profile_arns(access_token, &region, kiro_version, proxy, tls).await {
                Ok(arns) => arns,
                Err(e) => {
                    tracing::debug!("region={} 探测 profile 失败(不阻断): {}", region, e);
                    Vec::new()
                }
            }
        }
    });
    let per_region: Vec<Vec<String>> = futures::future::join_all(futs).await;

    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<ProfileOption> = Vec::new();
    for arn in per_region.into_iter().flatten() {
        if !seen.insert(arn.clone()) {
            continue; // 去重(理论上不同 region 端点返回的 arn 不重,防御性)
        }
        // region 用 KiroCredentials 的严格白名单解析(与对话端点选 region 同一套逻辑)。
        let region = KiroCredentials::region_from_profile_arn(&arn)
            .map(|s| s.to_string())
            .or_else(|| region_from_profile_arn(&arn))
            .unwrap_or_default();
        let account = account_from_profile_arn(&arn).unwrap_or_default();
        out.push(ProfileOption {
            arn,
            region,
            account,
            usable: false,
            subscription_title: None,
        });
    }
    Ok(out)
}

/// 对已列出的候选 profile 逐个验活（getUsageLimits），填 `usable` / `subscription_title`。
///
/// 用 leg2 的 access_token 构造临时 external_idp 凭据（尚未入池），复用
/// [`crate::kiro::token_manager::probe_profile_usable`]（与刷新/切换路径同一套判据）。
/// 单个验活失败/限流不阻断其它——只是该项 usable=false（保守，绝不因 429 判死 region）。
async fn probe_profiles_usable(
    profiles: &mut [ProfileOption],
    access_token: &str,
    config: &crate::model::config::Config,
    proxy: Option<&ProxyConfig>,
) {
    use crate::kiro::token_manager::{probe_profile_usable, ProfileProbeOutcome};
    // 临时 external_idp 基准凭据：只需 auth_method + token 供 probe 构造请求。
    let base = KiroCredentials {
        access_token: Some(access_token.to_string()),
        auth_method: Some("external_idp".to_string()),
        ..Default::default()
    };
    for p in profiles.iter_mut() {
        match probe_profile_usable(&base, config, access_token, proxy, &p.arn).await {
            ProfileProbeOutcome::Usable { subscription_title } => {
                p.usable = true;
                p.subscription_title = subscription_title;
            }
            other => {
                p.usable = false;
                tracing::debug!("profile {} 验活不可用: {:?}", p.arn, other);
            }
        }
    }
}

/// 从 `arn:aws:codewhisperer:{region}:...` 提取 region（index 3）。
fn region_from_profile_arn(arn: &str) -> Option<String> {
    let parts: Vec<&str> = arn.trim().split(':').collect();
    parts.get(3).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// 从 `arn:aws:codewhisperer:{region}:{account}:...` 提取 account（index 4，供前端展示区分账号）。
fn account_from_profile_arn(arn: &str) -> Option<String> {
    let parts: Vec<&str> = arn.trim().split(':').collect();
    parts.get(4).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// SHA-256 十六进制。
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_query_from_full_url() {
        let p = parse_query_from_pasted(
            "http://localhost:3128/signin/callback?issuer_url=https%3A%2F%2Flogin.microsoftonline.com%2Ftid%2Fv2.0&client_id=abc&scopes=openid%20profile",
        );
        assert_eq!(
            p.get("issuer_url").map(|s| s.as_str()),
            Some("https://login.microsoftonline.com/tid/v2.0")
        );
        assert_eq!(p.get("client_id").map(|s| s.as_str()), Some("abc"));
        assert_eq!(p.get("scopes").map(|s| s.as_str()), Some("openid profile"));
    }

    #[test]
    fn test_parse_query_from_bare_query() {
        let p = parse_query_from_pasted("code=AQAB123&state=xyz");
        assert_eq!(p.get("code").map(|s| s.as_str()), Some("AQAB123"));
        assert_eq!(p.get("state").map(|s| s.as_str()), Some("xyz"));
    }

    #[test]
    fn test_parse_query_strips_fragment() {
        let p = parse_query_from_pasted("/oauth/callback?code=c1&state=s1#frag=nope");
        assert_eq!(p.get("code").map(|s| s.as_str()), Some("c1"));
        assert!(p.get("frag").is_none());
    }

    #[test]
    fn test_region_from_profile_arn() {
        assert_eq!(
            region_from_profile_arn("arn:aws:codewhisperer:us-west-2:123:profile/X"),
            Some("us-west-2".to_string())
        );
        assert_eq!(region_from_profile_arn("garbage"), None);
    }

    #[test]
    fn test_account_from_profile_arn() {
        // 多 profile 账号 region 不同、account 也不同,前端要靠 account 区分展示。
        assert_eq!(
            account_from_profile_arn(
                "arn:aws:codewhisperer:us-east-1:617485799832:profile/D7XY4GXYQWG4"
            ),
            Some("617485799832".to_string())
        );
        assert_eq!(
            account_from_profile_arn(
                "arn:aws:codewhisperer:eu-central-1:155119901513:profile/ACPYXKUPYE3H"
            ),
            Some("155119901513".to_string())
        );
        assert_eq!(account_from_profile_arn("garbage"), None);
    }

    #[test]
    fn test_build_authorize_url_has_pkce_and_state() {
        let u = build_authorize_url(
            "https://login.microsoftonline.com/tid/oauth2/v2.0/authorize",
            "client-1",
            "http://localhost:3128/oauth/callback",
            "openid profile offline_access",
            "CHALLENGE",
            "STATE1",
            "user@example.com",
        );
        assert!(u.contains("code_challenge=CHALLENGE"));
        assert!(u.contains("code_challenge_method=S256"));
        assert!(u.contains("state=STATE1"));
        assert!(u.contains("response_type=code"));
        assert!(u.contains("login_hint=user%40example.com"));
    }

    #[test]
    fn test_random_state_unique_and_urlsafe() {
        let a = random_state();
        let b = random_state();
        assert_ne!(a, b);
        assert!(!a.contains('+') && !a.contains('/') && !a.contains('='));
    }
}
