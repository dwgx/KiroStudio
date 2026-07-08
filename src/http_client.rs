//! HTTP Client 构建模块
//!
//! 提供统一的 HTTP Client 构建功能，支持代理配置

use reqwest::{Client, Proxy};
use std::time::Duration;

use crate::model::config::TlsBackend;

/// 代理配置
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ProxyConfig {
    /// 代理地址，支持 http/https/socks5
    pub url: String,
    /// 代理认证用户名
    pub username: Option<String>,
    /// 代理认证密码
    pub password: Option<String>,
}

impl ProxyConfig {
    /// 从 url 创建代理配置
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            username: None,
            password: None,
        }
    }

    /// 设置认证信息
    pub fn with_auth(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }
}

/// 从原始代理输入中拆出 URL 与内嵌的账号密码。
///
/// 用户上号/编辑代理时常把账密直接写进 URL（如
/// `socks5://user:pass@38.244.34.185:1080`），但下游 reqwest 的 SOCKS5 代理不会可靠地
/// 从 URL 里提取 userinfo 做认证；必须拆成独立的 username/password 交给 `Proxy::basic_auth`。
/// 本函数把内嵌 `user:pass@` 从 host 前剥离、做百分号解码，返回 **不含账密的干净 URL** 与
/// 拆出的账密，供各上号/设置路径统一规整（凭据、OAuth 登录、全局代理都走它）。
///
/// 兼容的格式（“各种格式可识别”）：
/// - `scheme://user:pass@host:port`（内嵌账密，dwgx 的场景）
/// - `scheme://user@host:port`（仅用户名）
/// - `scheme://host:port`（无账密）
/// - 无 scheme 的 `user:pass@host:port` / `host:port`（原样保留 host 段，仅剥账密）
/// - `direct`（原样返回，语义=显式不走代理）与空串（原样）
/// - host（IPv6 用 `[::1]:1080` 形式）不含 `@`，故按最后一个 `@` 分隔 userinfo 不会误伤。
///
/// 返回 `(clean_url, username, password)`。若无内嵌账密则后两者为 `None`，`clean_url` 与
/// 去空白后的输入一致。
pub fn split_proxy_credentials(raw: &str) -> (String, Option<String>, Option<String>) {
    let trimmed = raw.trim();
    // direct / 空：原样返回（direct 语义由上层判定，空视为清除）。
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("direct") {
        return (trimmed.to_string(), None, None);
    }

    // 拆 scheme://rest；无 scheme 时 scheme_prefix 为空，rest 为整串。
    let (scheme_prefix, rest) = match trimmed.split_once("://") {
        Some((scheme, rest)) => (format!("{scheme}://"), rest),
        None => (String::new(), trimmed),
    };

    // host 段不含 '@'，故 userinfo 与 host 的分隔符是**最后一个** '@'
    // （即便密码里含 '@' 也能正确切分）。
    let (userinfo, hostport) = match rest.rsplit_once('@') {
        Some((ui, hp)) => (Some(ui), hp),
        None => (None, rest),
    };

    let clean_url = format!("{scheme_prefix}{hostport}");

    let (username, password) = match userinfo {
        None => (None, None),
        Some(ui) => {
            // userinfo = user[:pass]；两段都做百分号解码（内嵌账密可能被 URL 编码）。
            let (u, p) = match ui.split_once(':') {
                Some((u, p)) => (u, Some(p)),
                None => (ui, None),
            };
            let dec = |s: &str| urlencoding::decode(s).map(|c| c.into_owned()).unwrap_or_else(|_| s.to_string());
            let user = if u.is_empty() { None } else { Some(dec(u)) };
            let pass = p.and_then(|p| if p.is_empty() { None } else { Some(dec(p)) });
            (user, pass)
        }
    };

    (clean_url, username, password)
}

/// 构建 HTTP Client
///
/// # Arguments
/// * `proxy` - 可选的代理配置
/// * `timeout_secs` - 超时时间（秒）
///
/// # Returns
/// 配置好的 reqwest::Client
pub fn build_client(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    let mut builder = Client::builder().timeout(Duration::from_secs(timeout_secs));

    match tls_backend {
        TlsBackend::Rustls => {
            builder = builder.use_rustls_tls();
        }
        TlsBackend::NativeTls => {
            #[cfg(feature = "native-tls")]
            {
                builder = builder.use_native_tls();
            }
            #[cfg(not(feature = "native-tls"))]
            {
                anyhow::bail!("此构建版本未包含 native-tls 后端，请在配置中改用 rustls");
            }
        }
    }

    if let Some(proxy_config) = proxy {
        let mut proxy = Proxy::all(&proxy_config.url)?;

        // 设置代理认证
        if let (Some(username), Some(password)) = (&proxy_config.username, &proxy_config.password) {
            proxy = proxy.basic_auth(username, password);
        }

        builder = builder.proxy(proxy);
        tracing::debug!("HTTP Client 使用代理: {}", proxy_config.url);
    }

    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_config_new() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        assert_eq!(config.url, "http://127.0.0.1:7890");
        assert!(config.username.is_none());
        assert!(config.password.is_none());
    }

    #[test]
    fn test_proxy_config_with_auth() {
        let config = ProxyConfig::new("socks5://127.0.0.1:1080").with_auth("user", "pass");
        assert_eq!(config.url, "socks5://127.0.0.1:1080");
        assert_eq!(config.username, Some("user".to_string()));
        assert_eq!(config.password, Some("pass".to_string()));
    }

    #[test]
    fn test_split_proxy_inline_credentials() {
        // dwgx 的真实输入：账密内嵌在 socks5 URL 里。
        let (url, user, pass) =
            split_proxy_credentials("socks5://dwgxsocks:Dwgxnbnb0705@38.244.34.185:1080");
        assert_eq!(url, "socks5://38.244.34.185:1080");
        assert_eq!(user, Some("dwgxsocks".to_string()));
        assert_eq!(pass, Some("Dwgxnbnb0705".to_string()));
    }

    #[test]
    fn test_split_proxy_various_formats() {
        // 仅用户名
        let (u, user, pass) = split_proxy_credentials("http://onlyuser@host:3128");
        assert_eq!(u, "http://host:3128");
        assert_eq!(user, Some("onlyuser".to_string()));
        assert_eq!(pass, None);

        // 无账密
        let (u, user, pass) = split_proxy_credentials("socks5://1.2.3.4:1080");
        assert_eq!(u, "socks5://1.2.3.4:1080");
        assert!(user.is_none() && pass.is_none());

        // 无 scheme + 内嵌账密
        let (u, user, pass) = split_proxy_credentials("user:pass@1.2.3.4:1080");
        assert_eq!(u, "1.2.3.4:1080");
        assert_eq!(user, Some("user".to_string()));
        assert_eq!(pass, Some("pass".to_string()));

        // 密码含 @（按最后一个 @ 分隔，不误伤）
        let (u, user, pass) = split_proxy_credentials("socks5://user:p@ss@host:1080");
        assert_eq!(u, "socks5://host:1080");
        assert_eq!(user, Some("user".to_string()));
        assert_eq!(pass, Some("p@ss".to_string()));

        // 百分号编码的账密解码
        let (_u, user, pass) = split_proxy_credentials("http://us%40er:p%3Ass@host:3128");
        assert_eq!(user, Some("us@er".to_string()));
        assert_eq!(pass, Some("p:ss".to_string()));

        // direct / 空 原样
        assert_eq!(split_proxy_credentials("direct").0, "direct");
        assert_eq!(split_proxy_credentials("  ").0, "");
    }

    #[test]
    fn test_build_client_without_proxy() {
        let client = build_client(None, 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    #[test]
    fn test_build_client_with_proxy() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        let client = build_client(Some(&config), 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }
}
