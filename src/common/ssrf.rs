//! SSRF 防护：出站 URL 抓取的安全校验与 DNS 固定客户端构造
//!
//! 背景：登录页背景图代理（`/admin/api/bg-img?url=`）匿名可达，且把服务端
//! 抓到的响应体原样回给调用方。若不加限制，攻击者可诱导服务端去打内网/本机/
//! 云元数据端点（如 169.254.169.254），造成 SSRF + 内网信息泄露。
//!
//! 本模块提供统一防线：
//! 1. 只允许 http/https（背景图场景进一步只允许 https，由调用方把关）。
//! 2. 解析主机名 → 拿到所有候选 IP → 逐个校验，命中私有/环回/链路本地/
//!    保留/多播等「非公网可路由」段一律拒绝（含 IPv4-mapped IPv6）。
//! 3. 用 `resolve_to_addrs` 把域名**固定**到已校验过的 IP，杜绝「校验后再次
//!    解析」的 DNS rebinding（TOCTOU）绕过。
//! 4. 禁用重定向（`redirect::Policy::none()`），防止 `https://attacker/` 302
//!    跳到内网 `http://169.254.169.254` 绕过 scheme/host 校验。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

/// 判断某个 IPv4 是否属于「禁止出站」的非公网可路由段。
///
/// 覆盖：本网络/未指定、私有、CGNAT、环回、链路本地(含 AWS 元数据
/// 169.254.169.254)、IETF 协议段、文档/测试段、基准测试段、多播、保留、广播。
fn is_forbidden_ipv4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    // 0.0.0.0/8 本网络 / 未指定
    if o[0] == 0 {
        return true;
    }
    // 10.0.0.0/8 私有
    if o[0] == 10 {
        return true;
    }
    // 100.64.0.0/10 CGNAT
    if o[0] == 100 && (o[1] & 0xc0) == 64 {
        return true;
    }
    // 127.0.0.0/8 环回
    if o[0] == 127 {
        return true;
    }
    // 169.254.0.0/16 链路本地（含云元数据 169.254.169.254）
    if o[0] == 169 && o[1] == 254 {
        return true;
    }
    // 172.16.0.0/12 私有
    if o[0] == 172 && (16..=31).contains(&o[1]) {
        return true;
    }
    // 192.0.0.0/24 IETF 协议分配 & 192.0.2.0/24 文档(TEST-NET-1)
    if o[0] == 192 && o[1] == 0 && (o[2] == 0 || o[2] == 2) {
        return true;
    }
    // 192.168.0.0/16 私有
    if o[0] == 192 && o[1] == 168 {
        return true;
    }
    // 198.18.0.0/15 基准测试
    if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
        return true;
    }
    // 198.51.100.0/24 文档(TEST-NET-2)
    if o[0] == 198 && o[1] == 51 && o[2] == 100 {
        return true;
    }
    // 203.0.113.0/24 文档(TEST-NET-3)
    if o[0] == 203 && o[1] == 0 && o[2] == 113 {
        return true;
    }
    // 224.0.0.0/4 多播 + 240.0.0.0/4 保留（含 255.255.255.255 广播）
    if o[0] >= 224 {
        return true;
    }
    false
}

/// 判断某个 IPv6 是否属于「禁止出站」段。IPv4-mapped/兼容地址回落到 v4 校验。
fn is_forbidden_ipv6(ip: Ipv6Addr) -> bool {
    // IPv4-mapped (::ffff:a.b.c.d) 或 IPv4-compatible：按内嵌 v4 判定，
    // 防止用 ::ffff:127.0.0.1 之类绕过。
    if let Some(v4) = ip.to_ipv4() {
        return is_forbidden_ipv4(v4);
    }
    let seg = ip.segments();
    // ::1 环回 / :: 未指定
    if ip == Ipv6Addr::LOCALHOST || ip == Ipv6Addr::UNSPECIFIED {
        return true;
    }
    // fc00::/7 唯一本地地址(ULA)
    if (seg[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    // fe80::/10 链路本地
    if (seg[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // ff00::/8 多播
    if (seg[0] & 0xff00) == 0xff00 {
        return true;
    }
    // 64:ff9b::/96 NAT64 —— 内嵌 v4 目标，按 v4 判定后 32 位
    if seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2] == 0 && seg[3] == 0 && seg[4] == 0 && seg[5] == 0 {
        let v4 = Ipv4Addr::new(
            (seg[6] >> 8) as u8,
            (seg[6] & 0xff) as u8,
            (seg[7] >> 8) as u8,
            (seg[7] & 0xff) as u8,
        );
        return is_forbidden_ipv4(v4);
    }
    false
}

/// 统一入口：该 IP 是否禁止作为出站目标。
fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_forbidden_ipv4(v4),
        IpAddr::V6(v6) => is_forbidden_ipv6(v6),
    }
}

/// 从 `scheme://[user@]host[:port]/...` 中提取 (host, port)。
///
/// 仅支持 http/https；host 支持 IPv6 字面量的 `[::1]` 括号写法。
/// 返回小写 host（不含括号）与端口（缺省按 scheme 推断 80/443）。
fn parse_host_port(url: &str) -> Result<(String, u16), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| "URL 缺少 scheme".to_string())?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port: u16 = match scheme.as_str() {
        "https" => 443,
        "http" => 80,
        _ => return Err(format!("不支持的 scheme: {scheme}")),
    };

    // 去掉 path/query/fragment，只留 authority
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    if authority.is_empty() {
        return Err("URL 缺少主机".to_string());
    }

    // 去掉 userinfo（user:pass@），防止 `host@内网` 之类混淆
    let host_port = match authority.rsplit_once('@') {
        Some((_, hp)) => hp,
        None => authority,
    };

    // IPv6 字面量：[::1]:port
    if let Some(after) = host_port.strip_prefix('[') {
        let (h, tail) = after
            .split_once(']')
            .ok_or_else(|| "IPv6 字面量缺少右括号".to_string())?;
        let port = if let Some(p) = tail.strip_prefix(':') {
            p.parse::<u16>().map_err(|_| "非法端口".to_string())?
        } else {
            default_port
        };
        return Ok((h.to_ascii_lowercase(), port));
    }

    // 普通 host[:port]
    match host_port.rsplit_once(':') {
        Some((h, p)) => {
            let port = p.parse::<u16>().map_err(|_| "非法端口".to_string())?;
            if h.is_empty() {
                return Err("URL 缺少主机".to_string());
            }
            Ok((h.to_ascii_lowercase(), port))
        }
        None => Ok((host_port.to_ascii_lowercase(), default_port)),
    }
}

/// 校验一个出站 URL 的目标不落私网/环回/链路本地/元数据/保留段（写入时主防线）。
///
/// 用于 custom_api 写入 base_url 时先校验最终透传 URL。校验语义（安全 vs 可用的权衡）：
/// - scheme 不合法 → 拒绝。
/// - 解析成功 + 任一候选 IP 命中禁止段 → 拒绝（真 SSRF，含 IP 字面量如 169.254.169.254）。
/// - **DNS 解析失败 → 放行**：解析失败是网络问题（离线/DNS 抖动/中转站临时下线），
///   不是攻击信号；硬拒会让合法中转站因一时网络问题加不进号。IP 字面量（最主要的元数据/
///   内网攻击向量）走 lookup_host 不经真实 DNS、直接返回，仍会被立即拦下。域名指向内网这条
///   二阶风险由出站禁重定向兜底（见透传/deep_verify 的 no_redirect client）。
///
/// `allow_http=false` 仅允许 https；true 时额外允许 http（明文中转站，IP 层禁止段仍拦）。
/// 成功返回 ()，失败返回拒绝原因。
pub async fn validate_outbound_url(url: &str, allow_http: bool) -> Result<(), String> {
    let scheme = url
        .split_once("://")
        .map(|(s, _)| s.to_ascii_lowercase())
        .ok_or_else(|| "URL 缺少 scheme".to_string())?;
    let allowed: &[&str] = if allow_http {
        &["https", "http"]
    } else {
        &["https"]
    };
    if !allowed.iter().any(|s| *s == scheme) {
        return Err(format!(
            "scheme 不被允许(仅 https{}): {scheme}",
            if allow_http { "/http" } else { "" }
        ));
    }
    let (host, port) = parse_host_port(url)?;
    match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(iter) => {
            let addrs: Vec<SocketAddr> = iter.collect();
            for sa in &addrs {
                if is_forbidden_ip(sa.ip()) {
                    return Err(format!("目标解析到非公网地址，已拒绝: {}", sa.ip()));
                }
            }
            Ok(())
        }
        // DNS 失败 = 网络问题而非攻击，放行（IP 字面量不会走到这里）。出站禁重定向兜底。
        Err(_) => Ok(()),
    }
}

/// 校验一个出站 URL 并构造「已固定 DNS + 禁重定向」的安全 reqwest 客户端。
///
/// 成功返回的 `Client` 已把目标域名固定到本次校验通过的 IP 集合，直接对同一
/// URL 发起 `get(url)` 即可，无需担心二次解析导致的 rebinding。
///
/// 失败（scheme 不合法、无法解析、解析到内网/保留 IP）返回 Err，调用方据此
/// 拒绝请求（返回 4xx），绝不发起出站。
///
/// 注意：`allowed_schemes` 由调用方指定（背景图场景应只传 `["https"]`）。
pub async fn build_guarded_client(
    url: &str,
    timeout: Duration,
    allowed_schemes: &[&str],
) -> Result<reqwest::Client, String> {
    // scheme 白名单
    let scheme = url
        .split_once("://")
        .map(|(s, _)| s.to_ascii_lowercase())
        .ok_or_else(|| "URL 缺少 scheme".to_string())?;
    if !allowed_schemes.iter().any(|s| s.eq_ignore_ascii_case(&scheme)) {
        return Err(format!("scheme 不被允许: {scheme}"));
    }

    let (host, port) = parse_host_port(url)?;

    // 解析所有候选地址：主机名 → IP 列表。
    // 若 host 本身是 IP 字面量，lookup_host 会直接原样返回，不走 DNS。
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| format!("DNS 解析失败: {e}"))?
        .collect();

    if addrs.is_empty() {
        return Err("主机未解析到任何地址".to_string());
    }

    // 任一候选 IP 命中禁止段即整体拒绝（保守：不做「挑一个公网的」放行）。
    for sa in &addrs {
        if is_forbidden_ip(sa.ip()) {
            return Err(format!("目标解析到非公网地址，已拒绝: {}", sa.ip()));
        }
    }

    // 构造客户端：
    // - resolve_to_addrs 把该域名固定到刚校验过的 IP，杜绝二次解析(rebinding)。
    // - redirect none 禁止跟随重定向，防止 302 跳内网绕过。
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(&host, &addrs)
        .build()
        .map_err(|e| format!("构造 HTTP 客户端失败: {e}"))?;

    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_forbidden_ipv4() {
        // 内网/环回/链路本地/元数据/多播/保留一律禁止
        for ip in [
            "127.0.0.1", "10.0.0.1", "172.16.5.5", "172.31.255.255",
            "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0",
            "224.0.0.1", "240.0.0.1", "255.255.255.255", "192.0.2.5",
            "198.18.0.1",
        ] {
            assert!(
                is_forbidden_ip(ip.parse().unwrap()),
                "{ip} 应被禁止"
            );
        }
    }

    #[test]
    fn test_allowed_ipv4() {
        for ip in ["8.8.8.8", "1.1.1.1", "210.140.92.183"] {
            assert!(
                !is_forbidden_ip(ip.parse().unwrap()),
                "{ip} 应被放行"
            );
        }
    }

    #[test]
    fn test_forbidden_ipv6() {
        for ip in [
            "::1", "::", "fe80::1", "fc00::1", "fd12:3456::1",
            "ff02::1", "::ffff:127.0.0.1", "::ffff:169.254.169.254",
        ] {
            assert!(
                is_forbidden_ip(ip.parse().unwrap()),
                "{ip} 应被禁止"
            );
        }
    }

    #[test]
    fn test_allowed_ipv6() {
        assert!(!is_forbidden_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn test_parse_host_port() {        assert_eq!(
            parse_host_port("https://example.com/a/b?x=1").unwrap(),
            ("example.com".to_string(), 443)
        );
        assert_eq!(
            parse_host_port("http://example.com:8080/x").unwrap(),
            ("example.com".to_string(), 8080)
        );
        assert_eq!(
            parse_host_port("https://[::1]:9000/x").unwrap(),
            ("::1".to_string(), 9000)
        );
        assert_eq!(
            parse_host_port("https://[2001:db8::1]/x").unwrap(),
            ("2001:db8::1".to_string(), 443)
        );
        // userinfo 混淆：取 @ 之后的真实 host
        assert_eq!(
            parse_host_port("https://user:pass@example.com/x").unwrap(),
            ("example.com".to_string(), 443)
        );
        assert!(parse_host_port("ftp://example.com").is_err());
        assert!(parse_host_port("not-a-url").is_err());
    }

    #[tokio::test]
    async fn test_validate_outbound_url_rejects_internal_and_scheme() {
        // 元数据/环回/内网 IP 字面量：拒绝（IP 字面量 lookup_host 直接返回，不走真实 DNS）。
        assert!(validate_outbound_url("http://169.254.169.254/latest/meta-data", true).await.is_err());
        assert!(validate_outbound_url("https://127.0.0.1/v1/messages", true).await.is_err());
        assert!(validate_outbound_url("http://10.0.0.1:6379", true).await.is_err());
        assert!(validate_outbound_url("http://[::1]/x", true).await.is_err());
        // userinfo 混淆：@ 后是内网 → 拒绝（parse_host_port 剥 userinfo 取真实 host）。
        assert!(validate_outbound_url("https://ok.com@169.254.169.254/x", true).await.is_err());
        // scheme 门：allow_http=false 时 http 被拒。
        assert!(validate_outbound_url("http://8.8.8.8/x", false).await.is_err());
        // 非 http(s) scheme 一律拒。
        assert!(validate_outbound_url("ftp://8.8.8.8/x", true).await.is_err());
    }

    #[tokio::test]
    async fn test_validate_outbound_url_allows_public_ip() {
        // 公网 IP 字面量放行（用 IP 免真实 DNS 依赖）。
        assert!(validate_outbound_url("https://8.8.8.8/v1/messages", false).await.is_ok());
        assert!(validate_outbound_url("http://1.1.1.1/x", true).await.is_ok());
        // allow_http=false 下 https 公网放行。
        assert!(validate_outbound_url("https://1.1.1.1/x", false).await.is_ok());
    }
}
