use serde::{Deserialize, Serialize};

/// 脱敏展示：只留长度（`<set:N>`），不留内容。
///
/// 本模块的 struct 都装**可直接复用的活凭证**（`refresh_token` / `client_secret` /
/// `access_token` / 授权码 / PKCE verifier），一旦被 `{:?}` 打进日志即等于泄露。
/// 故一律**刻意不派生 `Debug`**，改为手写脱敏实现 —— 与 `credentials.rs::mask_secret`
/// 同款约定：在类型层面脱敏，杜绝任何调用点（日志 / 错误链）意外泄密。
fn mask_str(v: &str) -> String {
    format!("<set:{}>", v.len())
}

/// 刷新 Token 的请求体 (Social 认证)
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshRequest {
    pub refresh_token: String,
}

impl std::fmt::Debug for RefreshRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshRequest")
            .field("refresh_token", &mask_str(&self.refresh_token))
            .finish()
    }
}

/// 刷新 Token 的响应体 (Social 认证)
///
/// `expires_in` 单位为**秒**（调用方按 `Utc::now() + seconds(expires_in)` 折算过期时刻）；
/// 缺失 = 上游没给有效期，此时沿用凭据上原有的过期时间，**不**视为「立即过期」。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// 上游可能随刷新回带新的 profileArn（有则覆盖本地记录）。
    #[serde(default)]
    pub profile_arn: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
}

impl std::fmt::Debug for RefreshResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshResponse")
            .field("access_token", &mask_str(&self.access_token))
            .field("refresh_token", &self.refresh_token.as_deref().map(mask_str))
            .field("profile_arn", &self.profile_arn)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// IdC Token 刷新请求体 (AWS SSO OIDC)
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdcRefreshRequest {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    pub grant_type: String,
}

impl std::fmt::Debug for IdcRefreshRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdcRefreshRequest")
            .field("client_id", &mask_str(&self.client_id))
            .field("client_secret", &mask_str(&self.client_secret))
            .field("refresh_token", &mask_str(&self.refresh_token))
            .field("grant_type", &self.grant_type)
            .finish()
    }
}

/// IdC Token 刷新响应体 (AWS SSO OIDC)
///
/// `expires_in` 单位为**秒**；缺失表示上游没给有效期（沿用原有过期时间）。
/// `profile_arn` 是 AWS SSO 侧回带的 profile 标识，有则覆盖本地记录。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdcRefreshResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub profile_arn: Option<String>,
}

impl std::fmt::Debug for IdcRefreshResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdcRefreshResponse")
            .field("access_token", &mask_str(&self.access_token))
            .field("refresh_token", &self.refresh_token.as_deref().map(mask_str))
            .field("expires_in", &self.expires_in)
            .field("profile_arn", &self.profile_arn)
            .finish()
    }
}

/// External IdP token refresh response (Microsoft Entra ID / OAuth2 form flow).
///
/// `expires_in` 单位为**秒**；缺失表示上游没给有效期（沿用原有过期时间）。
#[derive(Deserialize)]
pub struct ExternalIdpRefreshResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
}

impl std::fmt::Debug for ExternalIdpRefreshResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalIdpRefreshResponse")
            .field("access_token", &mask_str(&self.access_token))
            .field("refresh_token", &self.refresh_token.as_deref().map(mask_str))
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

// ============ Social (Portal) 网页登录流程 ============

/// Social token 交换请求体（PKCE）
///
/// ⚠️ 字段名**刻意用 snake_case**（不带 `rename_all`）：本请求按 OAuth2 标准表单字段名
/// 发给上游 token 端点（`code_verifier` / `redirect_uri`），与本模块其它 camelCase 结构体
/// 的差异是协议要求，不是遗漏。
#[derive(Serialize)]
pub struct SocialCreateTokenRequest {
    pub code: String,
    pub code_verifier: String,
    pub redirect_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invitation_code: Option<String>,
}

impl std::fmt::Debug for SocialCreateTokenRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SocialCreateTokenRequest")
            // 授权码与 PKCE verifier 都是可换取 token 的活凭证；invitation_code 同属敏感邀请码。
            .field("code", &mask_str(&self.code))
            .field("code_verifier", &mask_str(&self.code_verifier))
            .field("redirect_uri", &self.redirect_uri)
            .field(
                "invitation_code",
                &self.invitation_code.as_deref().map(mask_str),
            )
            .finish()
    }
}

/// Social token 响应体
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocialCreateTokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// 上游给的**绝对**过期时刻（形态未定，仅原样留档；有效期以 `expires_in` 为准）。
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub profile_arn: Option<String>,
}

impl std::fmt::Debug for SocialCreateTokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SocialCreateTokenResponse")
            .field("access_token", &mask_str(&self.access_token))
            .field("refresh_token", &self.refresh_token.as_deref().map(mask_str))
            .field("expires_at", &self.expires_at)
            .field("expires_in", &self.expires_in)
            .field("profile_arn", &self.profile_arn)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "SECRET-VALUE";

    fn assert_masked(name: &str, dbg: String) {
        assert!(
            !dbg.contains(SECRET),
            "{name} 的 Debug 泄露了凭证（回退即 FAIL：改回 #[derive(Debug)] 会原样打印）: {dbg}"
        );
        assert!(
            dbg.contains("<set:"),
            "{name} 的 Debug 应给脱敏占位而不是裸值: {dbg}"
        );
    }

    /// 脱敏守卫：所有活凭证都不得出现在 `{:?}` 输出里。
    #[test]
    fn debug_never_leaks_secrets() {
        assert_masked(
            "RefreshRequest",
            format!(
                "{:?}",
                RefreshRequest {
                    refresh_token: SECRET.into()
                }
            ),
        );
        assert_masked(
            "RefreshResponse",
            format!(
                "{:?}",
                RefreshResponse {
                    access_token: SECRET.into(),
                    refresh_token: Some(SECRET.into()),
                    profile_arn: None,
                    expires_in: Some(3600),
                }
            ),
        );
        assert_masked(
            "IdcRefreshRequest",
            format!(
                "{:?}",
                IdcRefreshRequest {
                    client_id: SECRET.into(),
                    client_secret: SECRET.into(),
                    refresh_token: SECRET.into(),
                    grant_type: "refresh_token".into(),
                }
            ),
        );
        assert_masked(
            "IdcRefreshResponse",
            format!(
                "{:?}",
                IdcRefreshResponse {
                    access_token: SECRET.into(),
                    refresh_token: Some(SECRET.into()),
                    expires_in: None,
                    profile_arn: None,
                }
            ),
        );
        assert_masked(
            "ExternalIdpRefreshResponse",
            format!(
                "{:?}",
                ExternalIdpRefreshResponse {
                    access_token: SECRET.into(),
                    refresh_token: Some(SECRET.into()),
                    expires_in: None,
                }
            ),
        );
        assert_masked(
            "SocialCreateTokenRequest",
            format!(
                "{:?}",
                SocialCreateTokenRequest {
                    code: SECRET.into(),
                    code_verifier: SECRET.into(),
                    redirect_uri: "http://127.0.0.1:3128/signin/callback".into(),
                    invitation_code: Some(SECRET.into()),
                }
            ),
        );
        assert_masked(
            "SocialCreateTokenResponse",
            format!(
                "{:?}",
                SocialCreateTokenResponse {
                    access_token: SECRET.into(),
                    refresh_token: Some(SECRET.into()),
                    expires_at: Some("2026-10-14T00:00:00Z".into()),
                    expires_in: Some(3600),
                    profile_arn: None,
                }
            ),
        );
    }
}
