//! 反代入口安全层（批次3）
//!
//! 提供三道可选防线，均由 [`crate::model::config::Config`] 驱动、默认关闭以保持
//! 向后兼容：
//! - **CORS 收紧**：从「允许任意来源」改为可配置来源白名单（[`build_cors_layer`]）。
//! - **IP 白名单**：CIDR 匹配，命中才放行（[`IpAllowlist`]）。
//! - **入口限流**：每-IP 固定窗口每分钟计数（[`IngressRateLimiter`]）。
//!
//! CIDR 匹配自行实现（IPv4/IPv6 统一按 128 位前缀比较），避免引入新依赖。
//! 客户端 IP 判定见 [`client_ip`]，支持在可信反代后读取 `X-Forwarded-For`。

use std::net::IpAddr;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::anthropic::types::ErrorResponse;

// ============ CIDR 白名单 ============

/// 单条 CIDR 规则：网络地址（映射为 IPv6 的 128 位表示）+ 前缀长度。
#[derive(Debug, Clone, Copy)]
pub struct Cidr {
    /// 网络地址的 128 位大端表示（IPv4 走 IPv4-mapped IPv6 ::ffff:a.b.c.d）
    network: u128,
    /// 有效前缀位数（IPv4 已按 +96 归一到 128 位空间）
    prefix_bits: u32,
}

impl Cidr {
    /// 解析 "1.2.3.4"、"1.2.3.0/24"、"::1"、"2001:db8::/32" 等格式。
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (addr_part, prefix_part) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };

        let ip: IpAddr = addr_part
            .parse()
            .map_err(|_| format!("非法 IP 地址: {addr_part}"))?;

        // 归一到 128 位空间：IPv4 映射到 ::ffff:0:0/96 区段
        let (bits, max_prefix, v4_offset) = match ip {
            IpAddr::V4(v4) => {
                let mapped = u32::from(v4) as u128 | 0xffff_0000_0000u128;
                (mapped, 32u32, 96u32)
            }
            IpAddr::V6(v6) => (u128::from(v6), 128u32, 0u32),
        };

        let prefix_bits = match prefix_part {
            Some(p) => {
                let n: u32 = p.parse().map_err(|_| format!("非法前缀: {p}"))?;
                if n > max_prefix {
                    return Err(format!("前缀 {n} 超出范围(最大 {max_prefix})"));
                }
                n + v4_offset
            }
            None => max_prefix + v4_offset, // 无前缀视为单主机 /32 或 /128
        };

        // 用掩码把主机位清零，得到规范网络地址
        let mask = prefix_mask(prefix_bits);
        Ok(Cidr {
            network: bits & mask,
            prefix_bits,
        })
    }

    /// 判断某 IP（已归一为 128 位）是否落在本网段内。
    fn contains(&self, addr: u128) -> bool {
        let mask = prefix_mask(self.prefix_bits);
        (addr & mask) == self.network
    }

    /// 判断某 `IpAddr` 是否落在本网段内(便捷封装,供业务层黑名单直接用 IpAddr 判定)。
    pub fn contains_ip(&self, ip: IpAddr) -> bool {
        self.contains(ip_to_u128(ip))
    }
}

/// 生成 128 位前缀掩码：高 `prefix_bits` 位为 1。
fn prefix_mask(prefix_bits: u32) -> u128 {
    if prefix_bits == 0 {
        0
    } else if prefix_bits >= 128 {
        u128::MAX
    } else {
        u128::MAX << (128 - prefix_bits)
    }
}

/// 把 IpAddr 归一为 128 位表示，与 [`Cidr::parse`] 保持同一空间。
fn ip_to_u128(ip: IpAddr) -> u128 {
    match ip {
        IpAddr::V4(v4) => u32::from(v4) as u128 | 0xffff_0000_0000u128,
        IpAddr::V6(v6) => u128::from(v6),
    }
}

/// 校验单条 CIDR/IP 字符串是否合法（供 admin 更新配置时前置校验）。
pub fn validate_cidr(s: &str) -> Result<(), String> {
    Cidr::parse(s).map(|_| ())
}

/// IP 白名单：任一 CIDR 命中即放行。空列表表示「不启用白名单」。
#[derive(Debug, Clone)]
pub struct IpAllowlist {
    cidrs: Arc<Vec<Cidr>>,
}

impl IpAllowlist {
    /// 从配置字符串构建，非法条目记录告警并跳过（不致命）。
    pub fn from_config(entries: &[String]) -> Self {
        let mut cidrs = Vec::new();
        for e in entries {
            match Cidr::parse(e) {
                Ok(c) => cidrs.push(c),
                Err(err) => tracing::warn!("忽略非法 IP 白名单条目 '{}': {}", e, err),
            }
        }
        IpAllowlist {
            cidrs: Arc::new(cidrs),
        }
    }

    /// 是否启用（非空才启用）。
    pub fn is_active(&self) -> bool {
        !self.cidrs.is_empty()
    }

    /// 判断 IP 是否被允许。未启用时恒为 true。
    pub fn allows(&self, ip: IpAddr) -> bool {
        if self.cidrs.is_empty() {
            return true;
        }
        let addr = ip_to_u128(ip);
        self.cidrs.iter().any(|c| c.contains(addr))
    }
}

/// IP 黑名单：任一 CIDR 命中即**拒绝**(403)。空列表表示「不启用黑名单」。
/// 与白名单相反语义——白名单是"只允许列表内",黑名单是"只拒绝列表内"。两者独立可同时启用
/// (黑名单先判、命中即拒;再判白名单)。用于封禁特定滥用 IP。
#[derive(Debug, Clone)]
pub struct IpBlocklist {
    cidrs: Arc<Vec<Cidr>>,
}

impl IpBlocklist {
    /// 从配置字符串构建,非法条目记录告警并跳过(不致命)。
    pub fn from_config(entries: &[String]) -> Self {
        let mut cidrs = Vec::new();
        for e in entries {
            match Cidr::parse(e) {
                Ok(c) => cidrs.push(c),
                Err(err) => tracing::warn!("忽略非法 IP 黑名单条目 '{}': {}", e, err),
            }
        }
        IpBlocklist {
            cidrs: Arc::new(cidrs),
        }
    }

    /// 是否启用(非空才启用)。
    pub fn is_active(&self) -> bool {
        !self.cidrs.is_empty()
    }

    /// 判断 IP 是否被黑名单拦截(命中任一 CIDR 即 true=拒绝)。未启用时恒为 false(不拦)。
    pub fn blocks(&self, ip: IpAddr) -> bool {
        if self.cidrs.is_empty() {
            return false;
        }
        let addr = ip_to_u128(ip);
        self.cidrs.iter().any(|c| c.contains(addr))
    }
}

// ============ 入口每-IP 限流（固定窗口）============

/// 每-IP 固定窗口限流器：每 60s 窗口内计数，超 `max_per_min` 拒绝。
///
/// 固定窗口实现简单、内存可控；窗口切换时计数归零。相比滑窗略宽松，
/// 但对「防滥用/防扫描」这一目标足够，且无需存储每请求时间戳。
pub struct IngressRateLimiter {
    max_per_min: u32,
    /// ip -> (窗口起点, 该窗口内已计数)
    windows: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl IngressRateLimiter {
    pub fn new(max_per_min: u32) -> Self {
        IngressRateLimiter {
            max_per_min,
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// 是否启用（max>0 才启用）。
    pub fn is_active(&self) -> bool {
        self.max_per_min > 0
    }

    /// 记录一次请求并判断是否放行。未启用时恒为 true。
    pub fn check(&self, ip: IpAddr) -> bool {
        if self.max_per_min == 0 {
            return true;
        }
        let window = Duration::from_secs(60);
        let now = Instant::now();
        let mut map = self.windows.lock();

        // 惰性清理：窗口整体过期的条目顺手移除，避免 map 无限增长
        map.retain(|_, (start, _)| now.duration_since(*start) < window);

        let entry = map.entry(ip).or_insert((now, 0));
        if now.duration_since(entry.0) >= window {
            // 开新窗口
            *entry = (now, 0);
        }
        if entry.1 >= self.max_per_min {
            false
        } else {
            entry.1 += 1;
            true
        }
    }
}

// ============ 客户端 IP 判定 ============

/// 从请求判定客户端 IP。
///
/// - `trust_forwarded`=true 时优先取 `X-Forwarded-For` **最右**段、再 `X-Real-IP`；
///   仅在本服务确实位于可信反代之后才应开启，否则可被伪造（取最右防伪造，见下方实现注释）。
/// - 否则回退到 TCP 连接对端地址（`ConnectInfo`）。
pub fn client_ip(req: &Request<Body>, peer: Option<SocketAddr>, trust_forwarded: bool) -> Option<IpAddr> {
    if trust_forwarded {
        if let Some(xff) = req.headers().get("x-forwarded-for") {
            if let Ok(s) = xff.to_str() {
                // 安全(H2):取**最右**段,不是最左。XFF 是各级代理依次追加的链
                // (client, proxy1, proxy2, ...),最左是客户端**可任意伪造**的值——取最左
                // 会让攻击者发 `X-Forwarded-For: <白名单IP>` 绕过 IP 白名单,或每次换伪造 IP
                // 绕过每-IP 限流。只有**最右**那段是紧邻本服务的可信反代追加的、不可伪造。
                // (前提:本服务确实位于可信反代之后才开 trust_forwarded;默认关。)
                if let Some(last) = s.split(',').next_back() {
                    if let Ok(ip) = last.trim().parse::<IpAddr>() {
                        return Some(ip);
                    }
                }
            }
        }
        if let Some(xr) = req.headers().get("x-real-ip") {
            if let Ok(s) = xr.to_str() {
                if let Ok(ip) = s.trim().parse::<IpAddr>() {
                    return Some(ip);
                }
            }
        }
    }
    peer.map(|p| p.ip())
}

// ============ 中间件状态与实现 ============

/// 入口安全中间件共享状态。
#[derive(Clone)]
pub struct SecurityState {
    pub allowlist: IpAllowlist,
    pub blocklist: IpBlocklist,
    pub rate_limiter: Arc<IngressRateLimiter>,
    pub trust_forwarded: bool,
}

impl SecurityState {
    /// 若三道防线都未启用则返回 None，调用方据此决定是否挂载中间件。
    pub fn from_config(
        ip_allowlist: &[String],
        ip_blocklist: &[String],
        ingress_rate_limit_per_min: u32,
        trust_forwarded: bool,
    ) -> Option<Self> {
        let allowlist = IpAllowlist::from_config(ip_allowlist);
        let blocklist = IpBlocklist::from_config(ip_blocklist);
        let rate_limiter = IngressRateLimiter::new(ingress_rate_limit_per_min);
        if !allowlist.is_active() && !blocklist.is_active() && !rate_limiter.is_active() {
            return None;
        }
        Some(SecurityState {
            allowlist,
            blocklist,
            rate_limiter: Arc::new(rate_limiter),
            trust_forwarded,
        })
    }
}

/// 入口安全中间件：先查 IP 白名单（403），再查限流（429）。
pub async fn security_middleware(
    State(state): State<SecurityState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let ip = client_ip(&request, Some(peer), state.trust_forwarded);

    // IP 黑名单(先判、命中即拒):封禁特定滥用 IP,优先于白名单。无法判定 IP 时不拦(交给后续白名单/放行)。
    if state.blocklist.is_active() {
        if let Some(ip) = ip {
            if state.blocklist.blocks(ip) {
                tracing::debug!("入口 IP 黑名单拒绝: {:?}", ip);
                return (
                    StatusCode::FORBIDDEN,
                    Json(ErrorResponse::new(
                        "permission_error",
                        "来源 IP 已被封禁",
                    )),
                )
                    .into_response();
            }
        }
    }

    // IP 白名单
    if state.allowlist.is_active() {
        match ip {
            Some(ip) if state.allowlist.allows(ip) => {}
            _ => {
                tracing::debug!("入口 IP 白名单拒绝: {:?}", ip);
                return (
                    StatusCode::FORBIDDEN,
                    Json(ErrorResponse::new(
                        "permission_error",
                        "来源 IP 不在允许列表内",
                    )),
                )
                    .into_response();
            }
        }
    }

    // 每-IP 限流
    if state.rate_limiter.is_active() {
        // 无法判定 IP 时按对端地址，仍无则放行（本地/单元测试场景）
        if let Some(ip) = ip {
            if !state.rate_limiter.check(ip) {
                tracing::debug!("入口限流触发: {}", ip);
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(ErrorResponse::new(
                        "rate_limit_error",
                        "请求过于频繁，请稍后再试",
                    )),
                )
                    .into_response();
            }
        }
    }

    next.run(request).await
}

// ============ CORS ============

/// 根据配置构建 CORS 层。
///
/// - `allowed_origins` 为空：沿用旧行为，允许任意来源（`Any`），适合公开 API。
/// - 非空：仅允许命中列表的具体来源，并允许携带凭据（credentials）。
pub fn build_cors_layer(allowed_origins: &[String]) -> tower_http::cors::CorsLayer {
    use axum::http::Method;
    use tower_http::cors::{AllowHeaders, Any, CorsLayer};

    if allowed_origins.is_empty() {
        return CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);
    }

    let origins: Vec<axum::http::HeaderValue> = allowed_origins
        .iter()
        .filter_map(|o| o.parse().ok())
        .collect();

    // 注意：tower-http 在 allow_credentials(true) 与 Any(methods/headers) 组合时会 panic
    //（CORS 规范禁止 `*` 与凭据同用）。故这里用显式方法列表 + 镜像请求头。
    CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers(AllowHeaders::mirror_request())
        .allow_credentials(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn test_cidr_ipv4_single_host() {
        let c = Cidr::parse("127.0.0.1").unwrap();
        assert!(c.contains(ip_to_u128("127.0.0.1".parse().unwrap())));
        assert!(!c.contains(ip_to_u128("127.0.0.2".parse().unwrap())));
    }

    #[test]
    fn test_cidr_ipv4_subnet() {
        let c = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(c.contains(ip_to_u128(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)))));
        assert!(c.contains(ip_to_u128(IpAddr::V4(Ipv4Addr::new(10, 255, 255, 255)))));
        assert!(!c.contains(ip_to_u128(IpAddr::V4(Ipv4Addr::new(11, 0, 0, 1)))));
    }

    #[test]
    fn test_cidr_ipv4_24() {
        let c = Cidr::parse("192.168.1.0/24").unwrap();
        assert!(c.contains(ip_to_u128("192.168.1.55".parse().unwrap())));
        assert!(!c.contains(ip_to_u128("192.168.2.1".parse().unwrap())));
    }

    #[test]
    fn test_cidr_ipv6() {
        let c = Cidr::parse("::1/128").unwrap();
        assert!(c.contains(ip_to_u128(IpAddr::V6(Ipv6Addr::LOCALHOST))));
        assert!(!c.contains(ip_to_u128("::2".parse().unwrap())));

        let net = Cidr::parse("2001:db8::/32").unwrap();
        assert!(net.contains(ip_to_u128("2001:db8:1234::1".parse().unwrap())));
        assert!(!net.contains(ip_to_u128("2001:db9::1".parse().unwrap())));
    }

    #[test]
    fn test_cidr_reject_invalid() {
        assert!(Cidr::parse("not-an-ip").is_err());
        assert!(Cidr::parse("10.0.0.0/33").is_err());
        assert!(Cidr::parse("::1/129").is_err());
    }

    #[test]
    fn test_allowlist_empty_allows_all() {
        let al = IpAllowlist::from_config(&[]);
        assert!(!al.is_active());
        assert!(al.allows("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn test_allowlist_mixed() {
        let al = IpAllowlist::from_config(&[
            "127.0.0.1/32".to_string(),
            "10.0.0.0/8".to_string(),
            "::1/128".to_string(),
        ]);
        assert!(al.is_active());
        assert!(al.allows("127.0.0.1".parse().unwrap()));
        assert!(al.allows("10.5.6.7".parse().unwrap()));
        assert!(al.allows("::1".parse().unwrap()));
        assert!(!al.allows("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn test_allowlist_skips_invalid_entries() {
        // 非法条目被跳过，合法条目仍生效
        let al = IpAllowlist::from_config(&["bad".to_string(), "127.0.0.1/32".to_string()]);
        assert!(al.is_active());
        assert!(al.allows("127.0.0.1".parse().unwrap()));
        assert!(!al.allows("1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn test_rate_limiter_disabled() {
        let rl = IngressRateLimiter::new(0);
        assert!(!rl.is_active());
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        for _ in 0..1000 {
            assert!(rl.check(ip));
        }
    }

    #[test]
    fn test_rate_limiter_blocks_over_limit() {
        let rl = IngressRateLimiter::new(3);
        assert!(rl.is_active());
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        assert!(rl.check(ip));
        assert!(rl.check(ip));
        assert!(rl.check(ip));
        // 第 4 次超限
        assert!(!rl.check(ip));
        // 另一 IP 独立计数
        let ip2: IpAddr = "5.6.7.8".parse().unwrap();
        assert!(rl.check(ip2));
    }

    #[test]
    fn test_security_state_none_when_all_disabled() {
        assert!(SecurityState::from_config(&[], &[], 0, false).is_none());
        assert!(SecurityState::from_config(&["127.0.0.1/32".to_string()], &[], 0, false).is_some());
        // 只配黑名单也应启用中间件。
        assert!(SecurityState::from_config(&[], &["1.2.3.4/32".to_string()], 0, false).is_some());
        assert!(SecurityState::from_config(&[], &[], 100, false).is_some());
    }

    #[test]
    fn test_ip_blocklist_blocks_and_passes() {
        let bl = IpBlocklist::from_config(&["1.2.3.4/32".to_string(), "5.6.0.0/16".to_string()]);
        assert!(bl.is_active());
        // 命中单 IP → 拦。
        assert!(bl.blocks("1.2.3.4".parse().unwrap()));
        // 命中子网 → 拦。
        assert!(bl.blocks("5.6.7.8".parse().unwrap()));
        // 不在黑名单 → 放行(blocks=false)。
        assert!(!bl.blocks("9.9.9.9".parse().unwrap()));
    }

    #[test]
    fn test_ip_blocklist_empty_blocks_nothing() {
        let bl = IpBlocklist::from_config(&[]);
        assert!(!bl.is_active());
        // 空黑名单:任何 IP 都不拦(恒 false)。
        assert!(!bl.blocks("1.2.3.4".parse().unwrap()));
    }
}
