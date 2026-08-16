//! Kiro IDE Social 登录流程（Portal PKCE OAuth）
//!
//! 复现 Kiro IDE 的 portal-auth-provider 流程：
//! 1. 生成 PKCE code_verifier + code_challenge
//! 2. 启本地 HTTP 回调服务器
//! 3. 返回 portal URL 供用户在浏览器完成登录
//! 4. 捕获回调中的 authorization code
//! 5. 用 code + code_verifier 换取 access_token + refresh_token

use std::net::TcpListener;

use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::model::token_refresh::{SocialCreateTokenRequest, SocialCreateTokenResponse};
use crate::model::config::Config;

/// Portal 认证 URL（Kiro 网页版入口）
pub const KIRO_PORTAL_URL: &str = "https://app.kiro.dev";

/// Kiro auth service 默认端点
pub const KIRO_AUTH_ENDPOINT: &str = "https://prod.us-east-1.auth.desktop.kiro.dev";

/// 按 region 拼 Kiro auth service 端点（2026-08-15 用户调试移植）。
///
/// 🔴 Kiro auth 服务按 region 分布（`prod.{region}.auth.desktop.kiro.dev`）：
/// 硬编码 us-east-1 的部署，配了非默认 region（`effective_auth_region()`）会出现
/// 「上号成功、之后刷不动」或换 token 500。region 为空时回落 us-east-1。
pub fn auth_endpoint_for_region(region: &str) -> String {
    let region = region.trim();
    if region.is_empty() {
        return KIRO_AUTH_ENDPOINT.to_string();
    }
    format!("https://prod.{}.auth.desktop.kiro.dev", region)
}

/// 提取端点 URL 的 authority（host[:port]），用于 host 头。
///
/// 原写法 `trim_start_matches("https://")` 对带尾斜杠/路径的端点会拼出非法 host
/// （如 `...kiro.dev/`）；这里同时剥 scheme 与路径段。
fn auth_host(endpoint: &str) -> String {
    let after_scheme = endpoint
        .find("://")
        .map(|i| &endpoint[i + 3..])
        .unwrap_or(endpoint);
    after_scheme
        .split('/')
        .next()
        .unwrap_or(after_scheme)
        .to_string()
}

/// 还原**授权请求时浏览器实际落到的** redirect_uri，供 token 交换复用。
///
/// 🔴 OAuth2（RFC 6749 §4.1.3）要求换 token 时的 `redirect_uri` 与授权请求里的
/// **逐字节一致**。本地回调模式下 `session.redirect_uri` 只是 `http://127.0.0.1:{port}`
/// （无路径），而浏览器真正落到的是 `http://127.0.0.1:{port}/oauth/callback`
/// （或 `/signin/callback` —— portal 按 login_option 决定挂哪个路径）。拿无路径的
/// base 去换 token 就与授权请求不匹配，Cognito 侧 redirect_uri mismatch，Kiro 的
/// 包装层未捕获 → **500「Oops, something went wrong」**（2026-08-15 用户调试定位，
/// github 登录必现）。
///
/// [`OAuthCallbackData::path`] 记的正是浏览器实际命中的路径，之前**被解析出来却
/// 从未使用** —— 本函数就是它的消费点。
///
/// 远程回调模式下 base 已含路径（`{callbackBaseUrl}/api/admin/auth/callback`），
/// 此时原样返回，避免拼成 `.../callback/api/admin/auth/callback`。
pub fn full_redirect_uri(base: &str, callback_path: &str) -> String {
    let base_trimmed = base.trim_end_matches('/');

    // base 在 scheme 之后是否已经带了路径段
    let after_scheme = base_trimmed
        .find("://")
        .map(|i| &base_trimmed[i + 3..])
        .unwrap_or(base_trimmed);
    if after_scheme.contains('/') {
        return base_trimmed.to_string();
    }

    if callback_path.is_empty() || !callback_path.starts_with('/') {
        return base_trimmed.to_string();
    }

    format!("{}{}", base_trimmed, callback_path)
}

/// 与 IDE 一致的本地回调端口候选列表
const CALLBACK_PORTS: &[u16] = &[
    3128, 4649, 6588, 8008, 9091, 49153, 50153, 51153, 52153, 53153,
];

/// OAuth 回调数据
#[derive(Debug, Clone)]
pub struct OAuthCallbackData {
    pub code: String,
    pub login_option: String,
    pub path: String,
    /// OAuth state 参数（用于 CSRF 验证）
    pub state: String,
}

/// 回调服务器关闭句柄
///
/// Drop 时自动向服务器发送关闭信号，服务器退出监听循环并释放端口。
pub struct ServerHandle {
    _shutdown_tx: oneshot::Sender<()>,
}

/// 启动本地回调服务器，返回端口号和关闭句柄
///
/// 关闭句柄 drop 时服务器自动停止。当收到有效的 OAuth 回调时，通过 channel 发送回调数据。
///
/// `expected_state` 是启动 OAuth 流程时生成的 state nonce，
/// 回调时会与 URL 中携带的 state 参数做常量时间比较，不匹配则拒绝（防 CSRF）。
pub fn start_callback_server(
    tx: oneshot::Sender<OAuthCallbackData>,
    expected_state: String,
) -> anyhow::Result<(u16, ServerHandle)> {
    // 直接持有已绑定的 socket，避免 probe-and-bind 的 TOCTOU 竞态
    let (port, std_listener) = bind_available_port()?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        run_callback_server(std_listener, tx, shutdown_rx, expected_state).await;
    });

    Ok((
        port,
        ServerHandle {
            _shutdown_tx: shutdown_tx,
        },
    ))
}

fn bind_available_port() -> anyhow::Result<(u16, std::net::TcpListener)> {
    for &port in CALLBACK_PORTS {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => {
                listener.set_nonblocking(true)?;
                return Ok((port, listener));
            }
            Err(_) => continue,
        }
    }
    anyhow::bail!(
        "所有回调端口均被占用，请确保没有其他程序占用 {:?}",
        CALLBACK_PORTS
    )
}

async fn run_callback_server(
    std_listener: std::net::TcpListener,
    tx: oneshot::Sender<OAuthCallbackData>,
    mut shutdown_rx: oneshot::Receiver<()>,
    expected_state: String,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let port = std_listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let listener = match TcpListener::from_std(std_listener) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("Social 回调服务器初始化失败 (port {}): {}", port, e);
            return;
        }
    };

    tracing::info!("Social 回调服务器已启动: http://127.0.0.1:{}", port);

    // 只等待一次成功的回调，或关闭信号
    let mut tx = Some(tx);
    loop {
        let (mut stream, _addr) = tokio::select! {
            result = listener.accept() => match result {
                Ok(s) => s,
                Err(_) => break,
            },
            _ = &mut shutdown_rx => {
                tracing::info!("Social 回调服务器收到关闭信号，端口 {} 已释放", port);
                break;
            }
        };

        // 🔴 必须读到**完整请求行**才解析（2026-08-15 用户调试移植）：单次 read 只保证
        // 「至少 1 字节」，不保证一个 TCP 段装得下整行。OAuth 回调的 query 里
        // code + state 本就长，叠加浏览器给 127.0.0.1 带的 Cookie 后整个请求头轻易超过
        // 一次读取的量；若请求行被切断，解析出的 code 是**截断的**，拿去换 token
        // 必然失败（上游 500），而现场只看到「Token 交换失败」，根因完全不可见。
        // 读到 CRLF（或 LF）为止，上限 MAX_CALLBACK_REQUEST_BYTES 防 slowloris/内存放大。
        const MAX_CALLBACK_REQUEST_BYTES: usize = 64 * 1024;
        let mut acc: Vec<u8> = Vec::with_capacity(4096);
        let mut chunk = [0u8; 4096];
        let first_line: String = loop {
            if let Some(pos) = acc.iter().position(|&b| b == b'\n') {
                let line = &acc[..pos];
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                break String::from_utf8_lossy(line).into_owned();
            }
            if acc.len() >= MAX_CALLBACK_REQUEST_BYTES {
                // 超限仍无换行：当作畸形请求，用已读部分兜底（后续解析会失败并回 404）
                break String::from_utf8_lossy(&acc).into_owned();
            }
            match stream.read(&mut chunk).await {
                Ok(0) => break String::from_utf8_lossy(&acc).into_owned(), // 对端关闭
                Ok(n) => acc.extend_from_slice(&chunk[..n]),
                Err(_) => break String::new(),
            }
        };
        let first_line = first_line.as_str();

        // GET /oauth/callback?... HTTP/1.1
        if let Some(path_and_query) = first_line.strip_prefix("GET ").and_then(|s| {
            s.strip_suffix(" HTTP/1.1")
                .or_else(|| s.strip_suffix(" HTTP/1.0"))
        }) {
            if let Some(callback) = parse_callback(path_and_query) {
                // Fix 1: Validate OAuth state nonce to prevent CSRF attacks.
                // Compare using == which is constant-time for equal-length strings in Rust
                // (different lengths short-circuit, but a forged state would need to match
                // the exact nonce value regardless).
                if callback.state != expected_state {
                    tracing::warn!(
                        "OAuth state mismatch — possible CSRF attack (received: {:?})",
                        callback.state
                    );
                    let body = "<html><head><meta charset='utf-8'><title>认证失败</title></head>\
                        <body style='font-family:sans-serif;text-align:center;padding:60px'>\
                        <h2>&#10007; OAuth state mismatch - possible CSRF attack</h2>\
                        <p>请关闭此标签页并重新发起登录。</p></body></html>";
                    let response = format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                    break;
                }

                let body = "<html><head><meta charset='utf-8'><title>登录成功</title></head><body style='font-family:sans-serif;text-align:center;padding:60px'><h2>&#10003; 登录成功</h2><p>Token 已更新，请返回 Kiro Admin UI。</p><p style='color:#888;font-size:13px'>此标签页可以关闭。</p></body></html>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;

                if let Some(sender) = tx.take() {
                    let _ = sender.send(callback);
                }
                break;
            } else if path_and_query.starts_with("/oauth/callback")
                || path_and_query.starts_with("/signin/callback")
            {
                // 有 error 参数的回调
                let error_msg = path_and_query
                    .split('?')
                    .nth(1)
                    .and_then(|q| {
                        let p = parse_query_string(q);
                        p.get("error_description")
                            .or_else(|| p.get("error"))
                            .cloned()
                    })
                    .unwrap_or_else(|| "未知错误".to_string());

                let body = format!(
                    "<html><head><meta charset='utf-8'><title>登录失败</title></head><body style='font-family:sans-serif;text-align:center;padding:60px'><h2>&#10007; 登录失败</h2><p>{}</p><p style='color:#888;font-size:13px'>请关闭此标签页并重试。</p></body></html>",
                    error_msg
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
                break;
            }
        }

        // 其他请求返回 404
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
            .await;
    }
}

fn parse_callback(path_and_query: &str) -> Option<OAuthCallbackData> {
    let (path, query) = if let Some(idx) = path_and_query.find('?') {
        (&path_and_query[..idx], &path_and_query[idx + 1..])
    } else {
        return None;
    };

    if path != "/oauth/callback" && path != "/signin/callback" {
        return None;
    }

    let params = parse_query_string(query);

    // 必须有 code 且没有 error
    if params.contains_key("error") {
        return None;
    }

    let code = params.get("code")?.clone();
    let login_option = params.get("login_option").cloned().unwrap_or_default();
    let state = params.get("state").cloned().unwrap_or_default();

    Some(OAuthCallbackData {
        code,
        login_option,
        path: path.to_string(),
        state,
    })
}

/// base64url 编码（无填充），与 Kiro IDE 行为一致
fn base64url_encode(data: &[u8]) -> String {
    // 标准 base64 → 替换 +/= 为 base64url 规范
    let b64 = base64_encode_standard(data);
    b64.replace('+', "-").replace('/', "_").replace('=', "")
}

/// 标准 base64 编码（用于内部转换）
fn base64_encode_standard(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = if chunk.len() > 1 {
            chunk[1] as usize
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            chunk[2] as usize
        } else {
            0
        };
        out.push(CHARS[b0 >> 2] as char);
        out.push(CHARS[((b0 & 3) << 4) | (b1 >> 4)] as char);
        if chunk.len() > 1 {
            out.push(CHARS[((b1 & 0xf) << 2) | (b2 >> 6)] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(CHARS[b2 & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// 生成 PKCE code_verifier 和 code_challenge
pub fn generate_pkce() -> (String, String) {
    // 32 bytes from OS CSPRNG — required by RFC 7636 for PKCE code_verifier
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("OS CSPRNG unavailable");

    let verifier = base64url_encode(&bytes);

    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let digest = hasher.finalize();
    let challenge = base64url_encode(&digest);

    (verifier, challenge)
}

/// 构建供用户在浏览器中访问的 portal URL
pub fn build_portal_url(state: &str, code_challenge: &str, redirect_uri: &str) -> String {
    let params = format!(
        "state={}&code_challenge={}&code_challenge_method=S256&redirect_uri={}&redirect_from=KiroIDE",
        urlencoding::encode(state),
        urlencoding::encode(code_challenge),
        urlencoding::encode(redirect_uri),
    );
    format!("{}/signin?{}", KIRO_PORTAL_URL, params)
}

/// 简易 query string 解析（不依赖 url crate）
fn parse_query_string(query: &str) -> std::collections::HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| {
            let mut iter = pair.splitn(2, '=');
            let key = iter.next()?.to_string();
            let val = iter
                .next()
                .map(|v| {
                    // 简单的 percent-decode（处理 %XX 和 + 号）
                    let with_space = v.replace('+', " ");
                    urlencoding::decode(&with_space)
                        .map(|s| s.into_owned())
                        .unwrap_or_else(|_| with_space)
                })
                .unwrap_or_default();
            Some((key, val))
        })
        .collect()
}

/// 等待 OAuth 本地回调，超时 5 分钟。
///
/// 调用方创建 `oneshot::channel`，将 `tx` 传给 `start_callback_server`，
/// 将 `rx` 传给本函数等待结果。超时后返回带提示的错误。
pub async fn wait_for_callback(
    rx: oneshot::Receiver<OAuthCallbackData>,
) -> anyhow::Result<OAuthCallbackData> {
    // Fix 2: 在无头部署（headless server）环境中，浏览器可能永远不会打开，
    // 或者用户长时间未完成授权。这里设置 5 分钟硬超时，避免 server 进程挂起。
    tokio::time::timeout(std::time::Duration::from_secs(300), rx)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "OAuth 登录超时（5分钟）。服务器部署请在 config.json 配置 callbackBaseUrl 使用远程回调模式。"
            )
        })?
        .map_err(|_| anyhow::anyhow!("OAuth 回调 channel 已关闭（服务器提前退出）"))
}

/// 上游 5xx 是否值得短暂重试（500/502/503/504）。4xx 是参数问题，重试无益。
fn is_retryable_status(status: u16) -> bool {
    matches!(status, 500 | 502 | 503 | 504)
}

/// 交换失败的用户可读消息：区分 4xx（参数被拒，用户侧问题）与 5xx（上游服务异常）。
fn exchange_error_message(status: u16, body: &str) -> String {
    match status {
        400..=499 => format!("Social token 交换失败（参数被上游拒绝）{}: {}", status, body),
        500..=599 => format!("Social token 交换失败（上游服务异常，请稍后重试）{}: {}", status, body),
        _ => format!("Social token 交换失败 {}: {}", status, body),
    }
}

/// 日志用文本截断：只留前 `max_chars` 字符（body 可能很长或含多余细节）。
fn truncate_log_text(s: &str, max_chars: usize) -> String {
    let truncated: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        format!("{}...(truncated)", truncated)
    } else {
        truncated
    }
}

/// 5xx 短暂重试上限：最多重试 1 次。上游偶发故障 500ms 内可自愈，一次足够，不放大上游压力。
const MAX_TOKEN_EXCHANGE_RETRIES: usize = 1;
const TOKEN_EXCHANGE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// 用 authorization code 换取 access_token + refresh_token
pub async fn exchange_code_for_token(
    auth_endpoint: &str,
    code: &str,
    code_verifier: &str,
    full_redirect_uri: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<SocialCreateTokenResponse> {
    // 端点可能带尾斜杠/路径（部署配置）——trim 后拼 /oauth/token 才正确。
    let auth_endpoint = auth_endpoint.trim_end_matches('/');
    let url = format!("{}/oauth/token", auth_endpoint);
    let client = build_client(proxy, 30, config.tls_backend)?;

    let body = SocialCreateTokenRequest {
        code: code.to_string(),
        code_verifier: code_verifier.to_string(),
        redirect_uri: full_redirect_uri.to_string(),
        invitation_code: None,
    };

    let kiro_version = &config.kiro_version;
    let user_agent = format!("KiroIDE-{}", kiro_version);

    // host 头只能是 authority（host[:port]）：原写法只剥 `https://`，端点带尾斜杠
    // 或路径时会拼出 `...kiro.dev/` 这种非法值（2026-08-15 用户调试移植）。
    let host_header = auth_host(auth_endpoint);

    let mut retries: usize = 0;
    loop {
        let resp = client
            .post(&url)
            .header("Accept", "application/json, text/plain, */*")
            .header("Content-Type", "application/json")
            .header("User-Agent", &user_agent)
            .header("host", &host_header)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if status.is_success() {
            return resp
                .json::<SocialCreateTokenResponse>()
                .await
                .map_err(|e| anyhow::anyhow!("解析 Social token 响应失败: {}", e));
        }

        let body_text = resp.text().await.unwrap_or_default();
        // 日志脱敏：code 是敏感值，只记长度 + 前 4 字符，绝不记完整 code。
        let code_prefix: String = code.chars().take(4).collect();
        tracing::warn!(
            "Social token 交换失败: auth_endpoint={}, code_len={}, code_prefix={:?}, status={}, body={:?}",
            auth_endpoint,
            code.len(),
            code_prefix,
            status,
            truncate_log_text(&body_text, 500),
        );

        // 5xx（500/502/503/504）短暂重试 1 次；4xx 是参数问题，重试无益，直接失败。
        if is_retryable_status(status.as_u16()) && retries < MAX_TOKEN_EXCHANGE_RETRIES {
            retries += 1;
            tokio::time::sleep(TOKEN_EXCHANGE_RETRY_DELAY).await;
            continue;
        }

        anyhow::bail!("{}", exchange_error_message(status.as_u16(), &body_text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// 2026-08-15 用户调试移植：redirect_uri 必须还原浏览器实际落到的完整 URI。
    #[test]
    fn full_redirect_uri_reconstructs_browser_path() {
        // 本地回调：base 无路径 + 回调路径 → 拼出浏览器实际命中的完整 URI。
        assert_eq!(
            full_redirect_uri("http://127.0.0.1:3128", "/oauth/callback"),
            "http://127.0.0.1:3128/oauth/callback"
        );
        assert_eq!(
            full_redirect_uri("http://127.0.0.1:3128/", "/signin/callback"),
            "http://127.0.0.1:3128/signin/callback"
        );
        // 远程回调：base 已含路径 → 原样返回，不重复拼接。
        assert_eq!(
            full_redirect_uri("https://api.dwgx.top/api/admin/auth/callback", "/signin/callback"),
            "https://api.dwgx.top/api/admin/auth/callback"
        );
        // 无回调路径 → base 原样。
        assert_eq!(full_redirect_uri("http://127.0.0.1:3128", ""), "http://127.0.0.1:3128");
    }

    /// 2026-08-15 用户调试移植：auth 端点按 region 拼，空 region 回落 us-east-1。
    #[test]
    fn auth_endpoint_for_region_switches_region() {
        assert_eq!(
            auth_endpoint_for_region("us-east-1"),
            "https://prod.us-east-1.auth.desktop.kiro.dev"
        );
        assert_eq!(
            auth_endpoint_for_region("eu-west-1"),
            "https://prod.eu-west-1.auth.desktop.kiro.dev"
        );
        assert_eq!(auth_endpoint_for_region(""), KIRO_AUTH_ENDPOINT);
        assert_eq!(auth_endpoint_for_region("  "), KIRO_AUTH_ENDPOINT);
    }

    /// host 头必须是纯 authority：剥 scheme 与路径段。
    #[test]
    fn auth_host_strips_scheme_and_path() {
        assert_eq!(auth_host("https://prod.us-east-1.auth.desktop.kiro.dev"), "prod.us-east-1.auth.desktop.kiro.dev");
        assert_eq!(auth_host("https://prod.us-east-1.auth.desktop.kiro.dev/"), "prod.us-east-1.auth.desktop.kiro.dev");
        assert_eq!(auth_host("http://127.0.0.1:8787/v1"), "127.0.0.1:8787");
        assert_eq!(auth_host("prod.us-east-1.auth.desktop.kiro.dev"), "prod.us-east-1.auth.desktop.kiro.dev");
    }

    /// Config 所有字段都有 serde default，空 JSON 即可构造。
    fn test_config() -> Config {
        serde_json::from_str("{}").unwrap()
    }

    /// 本地 TCP mock：按顺序对每次连接返回 (status, body)，并统计收到请求次数。
    async fn spawn_mock_server(
        responses: Vec<(u16, &'static str)>,
        hits: Arc<AtomicUsize>,
    ) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            for (status, body) in responses {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 4096];
                let _ = sock.read(&mut buf).await;
                hits.fetch_add(1, Ordering::SeqCst);
                let reason = match status {
                    500 => "Internal Server Error",
                    502 => "Bad Gateway",
                    400 => "Bad Request",
                    _ => "OK",
                };
                let resp = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    reason,
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        port
    }

    #[test]
    fn test_is_retryable_status() {
        for s in [500u16, 502, 503, 504] {
            assert!(is_retryable_status(s), "{s} 应可重试");
        }
        for s in [400u16, 401, 403, 404, 409, 429, 499, 501, 505, 600] {
            assert!(!is_retryable_status(s), "{s} 不应重试");
        }
    }

    #[test]
    fn test_exchange_error_message_4xx_vs_5xx() {
        assert_eq!(
            exchange_error_message(400, "bad params"),
            "Social token 交换失败（参数被上游拒绝）400: bad params"
        );
        assert_eq!(
            exchange_error_message(503, "overloaded"),
            "Social token 交换失败（上游服务异常，请稍后重试）503: overloaded"
        );
        // 非 4xx/5xx（如 3xx）保持原通用文案
        assert_eq!(
            exchange_error_message(301, "moved"),
            "Social token 交换失败 301: moved"
        );
    }

    #[test]
    fn test_truncate_log_text() {
        assert_eq!(truncate_log_text("短文本", 500), "短文本");
        let long = "x".repeat(600);
        assert_eq!(
            truncate_log_text(&long, 500),
            format!("{}...(truncated)", "x".repeat(500))
        );
        // 多字节字符按字符截断，不截出非法 UTF-8、不 panic
        let wide = "界".repeat(600);
        let t = truncate_log_text(&wide, 500);
        assert_eq!(t.chars().count(), 514);
        assert!(t.starts_with("界"));
    }

    #[tokio::test]
    async fn test_retry_5xx_then_success() {
        let hits = Arc::new(AtomicUsize::new(0));
        let port = spawn_mock_server(
            vec![
                (500, r#"{"error":"boom"}"#),
                (200, r#"{"accessToken":"tok","refreshToken":"rt","profileArn":"arn"}"#),
            ],
            hits.clone(),
        )
        .await;
        let config = test_config();
        let resp = exchange_code_for_token(
            &format!("http://127.0.0.1:{}", port),
            "secret-code-1234",
            "verifier",
            "http://127.0.0.1:9999/callback",
            &config,
            None,
        )
        .await
        .expect("500 后重试应成功");
        assert_eq!(resp.access_token, "tok");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "5xx 应重试 1 次（共 2 次请求）");
    }

    #[tokio::test]
    async fn test_retry_5xx_still_fails_with_5xx_message() {
        let hits = Arc::new(AtomicUsize::new(0));
        let port = spawn_mock_server(
            vec![(500, r#"{"error":"boom"}"#), (500, r#"{"error":"boom2"}"#)],
            hits.clone(),
        )
        .await;
        let config = test_config();
        let err = exchange_code_for_token(
            &format!("http://127.0.0.1:{}", port),
            "secret-code-1234",
            "verifier",
            "http://127.0.0.1:9999/callback",
            &config,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("上游服务异常，请稍后重试"),
            "重试仍失败应保持 5xx 文案: {err}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2, "5xx 应重试 1 次");
        assert!(
            !err.contains("secret-code-1234"),
            "错误消息不得含完整 code（脱敏）: {err}"
        );
    }

    #[tokio::test]
    async fn test_4xx_no_retry_and_message() {
        let hits = Arc::new(AtomicUsize::new(0));
        let port =
            spawn_mock_server(vec![(400, r#"{"error":"bad params"}"#)], hits.clone()).await;
        let config = test_config();
        let err = exchange_code_for_token(
            &format!("http://127.0.0.1:{}", port),
            "secret-code-1234",
            "verifier",
            "http://127.0.0.1:9999/callback",
            &config,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("参数被上游拒绝"), "4xx 文案: {err}");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "4xx 不得重试");
        assert!(!err.contains("secret-code-1234"), "错误消息不得含完整 code: {err}");
    }
}
