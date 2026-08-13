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
/// 出站目标的信任策略。按**调用点面向谁**选择，不是全局开关。
///
/// 两类调用点的威胁模型截然不同，用同一套判据必然一头过严一头过松：
/// - [`Self::Strict`]：目标 URL 来自匿名可达端点或外部可控数据（如登录页背景图代理，
///   URL 取自第三方 JSON 源）。攻击者能直接控制目标，必须拦下全部非公网段。
/// - [`Self::AdminConfigured`]：目标由管理员过了 adminKey 鉴权后**亲手填写**
///   （如 custom_api 的 base_url）。此时"能指定出站目标"本身就是该功能的用途，
///   管理员另有 `proxy_url` 等同等能力，故对**不可能通向内网基础设施**的保留段放宽。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SsrfPolicy {
    /// 严格：所有非公网段一律拒绝。
    Strict,
    /// 管理员显式配置：放宽 RFC 2544 基准测试段（fake-IP 代理的默认地址池）
    /// 与**字面量环回**（127.0.0.0/8、::1 —— 本机服务互转的合法场景：
    /// 如网关 → 本机 shield → 本机 fuckopencode，2026-08-13 放行）。
    /// 豁免只对字面量生效；内嵌混淆（Teredo/ISATAP）、私网段、链路本地/元数据仍拒绝。
    AdminConfigured,
}

/// RFC 2544 基准测试段的标签。它被单独拎出来是因为它在
/// [`SsrfPolicy::AdminConfigured`] 下被豁免（fake-IP 代理默认地址池），
/// 理由见 [`is_forbidden_ipv4_with`]。
const BENCHMARK_SEGMENT: &str = "198.18.0.0/15 基准测试段";

/// 判断某个 IPv4 落在哪个「禁止出站」段，返回该段的可读标签（不禁止则 None）。
///
/// 返回标签而非 bool 是为了让拒绝原因可诊断：历史缺陷是报错只说「SSRF 防护」，
/// 用户完全不知道是自己机器的代理导致的（见 [`describe_rejection`]）。
fn forbidden_segment_v4(ip: Ipv4Addr) -> Option<&'static str> {
    let o = ip.octets();
    // 0.0.0.0/8 本网络 / 未指定
    if o[0] == 0 {
        return Some("0.0.0.0/8 本网络");
    }
    // 10.0.0.0/8 私有
    if o[0] == 10 {
        return Some("10.0.0.0/8 私有网段");
    }
    // 100.64.0.0/10 CGNAT
    if o[0] == 100 && (o[1] & 0xc0) == 64 {
        return Some("100.64.0.0/10 运营商级 NAT");
    }
    // 127.0.0.0/8 环回
    if o[0] == 127 {
        return Some("127.0.0.0/8 环回");
    }
    // 169.254.0.0/16 链路本地（含云元数据 169.254.169.254）
    if o[0] == 169 && o[1] == 254 {
        return Some("169.254.0.0/16 链路本地(含云元数据端点)");
    }
    // 172.16.0.0/12 私有
    if o[0] == 172 && (16..=31).contains(&o[1]) {
        return Some("172.16.0.0/12 私有网段");
    }
    // 192.0.0.0/24 IETF 协议分配 & 192.0.2.0/24 文档(TEST-NET-1)
    if o[0] == 192 && o[1] == 0 && (o[2] == 0 || o[2] == 2) {
        return Some("192.0.0.0/24 或 192.0.2.0/24 保留段");
    }
    // 192.168.0.0/16 私有
    if o[0] == 192 && o[1] == 168 {
        return Some("192.168.0.0/16 私有网段");
    }
    // 198.18.0.0/15 基准测试（唯一可被 AdminConfigured 豁免的段）
    if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
        return Some(BENCHMARK_SEGMENT);
    }
    // 198.51.100.0/24 文档(TEST-NET-2)
    if o[0] == 198 && o[1] == 51 && o[2] == 100 {
        return Some("198.51.100.0/24 文档保留段");
    }
    // 203.0.113.0/24 文档(TEST-NET-3)
    if o[0] == 203 && o[1] == 0 && o[2] == 113 {
        return Some("203.0.113.0/24 文档保留段");
    }
    // 224.0.0.0/4 多播 + 240.0.0.0/4 保留（含 255.255.255.255 广播）
    if o[0] >= 224 {
        return Some("224.0.0.0/4 多播或 240.0.0.0/4 保留段");
    }
    None
}

/// 按策略判断 IPv4 是否禁止出站。
///
/// `AdminConfigured` 只豁免 [`BENCHMARK_SEGMENT`]（198.18.0.0/15）这一段。理由：
///
/// 1. **它是代理软件的 fake-IP 池默认段。** Clash / Mihomo / Surge 在 fake-IP 模式下
///    把该段分配给**所有**域名。开了 fake-IP 的机器上，任何合法中转站域名都会解析到
///    198.18.x.x → 严格策略会让管理员**无法添加任何 custom_api 中转站**
///    （实测：api.uu6.top → 198.18.0.46 被拒）。这是本仓已知问题 #19 的生产侧同源缺陷，
///    当时只把测试改用 .invalid 域名绕过，生产路径没动。
/// 2. **它不通向任何内网基础设施。** 该段是 RFC 2544 给设备厂商做吞吐测试用的，
///    既不是 RFC 1918 私有段，也不含云元数据端点（169.254.169.254 属链路本地，
///    仍然拦）。fake-IP 场景下这个地址根本不是一台可达主机——代理会拦截该连接、
///    按 IP 反查回真实域名再出网，所以"打到 198.18.x"实际打到的是那个公网域名。
/// 3. **豁免范围严格限定在管理员已鉴权的调用点。** 匿名可达的背景图代理仍走
///    `Strict`，威胁模型不变。且管理员本就能配 `proxy_url` 指定任意出站通道，
///    放开这一段不新增任何它原本没有的能力。
fn is_forbidden_ipv4_with(ip: Ipv4Addr, policy: SsrfPolicy) -> bool {
    match forbidden_segment_v4(ip) {
        None => false,
        // AdminConfigured（管理员过 adminKey 鉴权后亲手填写 base_url）下豁免两类：
        // 基准测试段（fake-IP 代理的默认地址池）与**字面量环回**（127.0.0.0/8 ——
        // 本机服务互转的合法场景：如网关 → 本机 shield → 本机 fuckopencode）。
        // 豁免只对**字面量**生效：Teredo/ISATAP 内嵌环回走 v6 原生判定（不回落 v4），
        // 仍然拒绝 —— 绕过口未开。私网段/链路本地/元数据在任何策略下都拒绝。
        Some(seg) => {
            !(policy == SsrfPolicy::AdminConfigured
                && (seg == BENCHMARK_SEGMENT || seg == "127.0.0.0/8 环回"))
        }
    }
}

/// 内嵌 v4（NAT64/6to4/Teredo/ISATAP 解混淆）的判定：**环回永不豁免**。
///
/// 2026-08-13 字面量环回放行后，必须把内嵌路径与字面量路径分开——解混淆成
/// 127.x 的地址是攻击者编码出来的（可被用来把出站打回本机/内网），管理员
/// 亲手填 `127.0.0.1` 是意图明确的本机回环，两者语义不同。只有
/// `::ffff:127.0.0.1`（无损映射，等价写法）随字面量放行（见 `is_forbidden_ipv6_with`）。
fn is_forbidden_ipv4_embedded_with(ip: Ipv4Addr, policy: SsrfPolicy) -> bool {
    match forbidden_segment_v4(ip) {
        None => false,
        Some(seg) => !(policy == SsrfPolicy::AdminConfigured && seg == BENCHMARK_SEGMENT),
    }
}

/// 严格策略下的 IPv4 判定（保留原签名，供 v6 内嵌 v4 与既有测试复用）。
fn is_forbidden_ipv4(ip: Ipv4Addr) -> bool {
    is_forbidden_ipv4_with(ip, SsrfPolicy::Strict)
}

/// 判断某个 IPv6 是否属于「禁止出站」段。IPv4-mapped/兼容地址回落到 v4 校验。
fn is_forbidden_ipv6_with(ip: Ipv6Addr, policy: SsrfPolicy) -> bool {
    // ::1 字面量环回必须先于 to_ipv4 回落处理：::1 满足 IPv4-compatible 条件，
    // to_ipv4() 会把它映射成 ::0.0.0.1 → 落到 0.0.0.0/8 本网络段（永不豁免），
    // 管理员配置下的本机回环（2026-08-13 放行）会被误杀。
    if ip == Ipv6Addr::LOCALHOST {
        return !(policy == SsrfPolicy::AdminConfigured);
    }
    // IPv4-mapped (::ffff:a.b.c.d) 或 IPv4-compatible：按内嵌 v4 判定（沿用同一策略，
    // 否则 ::ffff:198.18.0.46 会与裸 198.18.0.46 判定不一致）。
    if let Some(v4) = ip.to_ipv4() {
        return is_forbidden_ipv4_with(v4, policy);
    }
    is_forbidden_ipv6_native(ip, policy)
}

/// 严格策略下的 IPv6 判定（保留原签名，供既有测试复用）。
fn is_forbidden_ipv6(ip: Ipv6Addr) -> bool {
    is_forbidden_ipv6_with(ip, SsrfPolicy::Strict)
}

/// 非 IPv4-mapped 的原生 IPv6 段判定 + 各种内嵌 v4 形式。
fn is_forbidden_ipv6_native(ip: Ipv6Addr, policy: SsrfPolicy) -> bool {
    let seg = ip.segments();
    // ::1 环回 / :: 未指定
    if ip == Ipv6Addr::LOCALHOST {
        // 与 v4 环回同口径：AdminConfigured（管理员亲手填写）下放行本机回环。
        return !(policy == SsrfPolicy::AdminConfigured);
    }
    if ip == Ipv6Addr::UNSPECIFIED {
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
    if seg[0] == 0x0064
        && seg[1] == 0xff9b
        && seg[2] == 0
        && seg[3] == 0
        && seg[4] == 0
        && seg[5] == 0
    {
        let v4 = Ipv4Addr::new(
            (seg[6] >> 8) as u8,
            (seg[6] & 0xff) as u8,
            (seg[7] >> 8) as u8,
            (seg[7] & 0xff) as u8,
        );
        return is_forbidden_ipv4_embedded_with(v4, policy);
    }
    // 6to4 (RFC 3056): 2002::/16 —— 前缀内嵌 IPv4 地址（bits 16–47）。
    // 例：2002:7f00:0001:: 内嵌 127.0.0.1，2002:a9fe:a9fe:: 内嵌 169.254.169.254。
    // ip.to_ipv4() 仅覆盖 ::ffff: 和 :: 兼容形式，不覆盖 6to4，故需单独处理。
    if seg[0] == 0x2002 {
        let embedded_v4 = Ipv4Addr::new(
            (seg[1] >> 8) as u8,
            (seg[1] & 0xff) as u8,
            (seg[2] >> 8) as u8,
            (seg[2] & 0xff) as u8,
        );
        if is_forbidden_ipv4_embedded_with(embedded_v4, policy) {
            return true;
        }
    }
    // Teredo (RFC 4380): 2001:0000::/32 —— 客户端 IPv4 在最后 32 位、按位取反混淆。
    // 例：2001:0000:...:80ff:fffe 解混淆后是 127.0.0.1。`to_ipv4()` 不覆盖它，
    // 但攻击者同样可用它在支持 Teredo 的主机上把出站打向内网，故按裸 v4 同口径判定。
    if seg[0] == 0x2001 && seg[1] == 0x0000 {
        let teredo_v4 = Ipv4Addr::new(
            (!seg[6] >> 8) as u8,
            (!seg[6] & 0xff) as u8,
            (!seg[7] >> 8) as u8,
            (!seg[7] & 0xff) as u8,
        );
        if is_forbidden_ipv4_embedded_with(teredo_v4, policy) {
            return true;
        }
    }
    // ISATAP (RFC 5214): 接口标识符形如 0000:5EFE:xxxx:xxxx（或 u 位置位的 0200:5EFE:），
    // 后 32 位为内嵌 IPv4。例：2001:db8::5efe:7f00:1 内嵌 127.0.0.1。
    if (seg[4] == 0x0000 || seg[4] == 0x0200) && seg[5] == 0x5efe {
        let isatap_v4 = Ipv4Addr::new(
            (seg[6] >> 8) as u8,
            (seg[6] & 0xff) as u8,
            (seg[7] >> 8) as u8,
            (seg[7] & 0xff) as u8,
        );
        if is_forbidden_ipv4_embedded_with(isatap_v4, policy) {
            return true;
        }
    }
    false
}

/// 统一入口：该 IP 是否禁止作为出站目标（按策略）。
fn is_forbidden_ip_with(ip: IpAddr, policy: SsrfPolicy) -> bool {
    match ip {
        IpAddr::V4(v4) => is_forbidden_ipv4_with(v4, policy),
        IpAddr::V6(v6) => is_forbidden_ipv6_with(v6, policy),
    }
}

/// 严格策略下的统一入口（保留原签名，供既有测试复用）。
fn is_forbidden_ip(ip: IpAddr) -> bool {
    is_forbidden_ip_with(ip, SsrfPolicy::Strict)
}

/// 把「解析到禁止段」渲染成可诊断的中文拒绝原因。
///
/// 历史缺陷：报错只有一句「自定义 API base_url 校验失败(SSRF 防护)」，管理员看不出
/// 是自己机器开了代理 fake-IP 导致的，只会以为网关坏了或中转站不可用（这正是本轮
/// 用户遇到的情形）。命中基准测试段时额外给出可操作的提示。
fn describe_rejection(ip: IpAddr) -> String {
    let seg = match ip {
        IpAddr::V4(v4) => forbidden_segment_v4(v4),
        // v6 的具体段名未细分，统一给一个够用的标签
        IpAddr::V6(v6) => v6.to_ipv4().and_then(forbidden_segment_v4),
    };
    match seg {
        Some(s) if s == BENCHMARK_SEGMENT => format!(
            "目标解析到 {ip}（{s}）。该段是 Clash/Mihomo 等代理软件 fake-IP 模式的默认地址池——\
             若本机开着此类代理，所有域名都会解析到这里。请关闭 fake-IP（改用 redir-host）\
             或让网关直连 DNS 后重试"
        ),
        Some(s) => format!("目标解析到非公网地址 {ip}（{s}），已拒绝"),
        None => format!("目标解析到非公网地址 {ip}，已拒绝"),
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
        // 代理节点地址（`validate_proxy_address`）。1080 是 SOCKS 的惯例端口。
        // 放在这里而不是在 `validate_proxy_address` 里另写一份解析：userinfo 剥离与
        // IPv6 字面量这两段是安全承重的（`host@内网` 混淆、`[::1]:port`），
        // 复制一份必然与本函数漂移。**不放宽任何调用方**：
        // `validate_outbound_url` 在调用本函数**之前**先过自己的 scheme 白名单
        // （只有 https/http），所以 socks 到不了那条路径。
        "socks5" | "socks5h" => 1080,
        _ => return Err(format!("不支持的 scheme: {scheme}")),
    };

    // 去掉 path/query/fragment，只留 authority
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
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
    validate_outbound_url_with(url, allow_http, SsrfPolicy::Strict).await
}

/// 同 [`validate_outbound_url`]，但显式指定信任策略（见 [`SsrfPolicy`]）。
///
/// 管理员在面板里亲手配置的出站目标（custom_api base_url）应传
/// [`SsrfPolicy::AdminConfigured`]；匿名可达 / 外部数据驱动的抓取一律用
/// [`SsrfPolicy::Strict`]。
pub async fn validate_outbound_url_with(
    url: &str,
    allow_http: bool,
    policy: SsrfPolicy,
) -> Result<(), String> {
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
                if is_forbidden_ip_with(sa.ip(), policy) {
                    return Err(describe_rejection(sa.ip()));
                }
            }
            Ok(())
        }
        // DNS 失败 = 网络问题而非攻击，放行（IP 字面量不会走到这里）。出站禁重定向兜底。
        Err(_) => Ok(()),
    }
}

/// 校验一个**代理节点地址**（`socks5://` / `socks5h://` / `http://` / `https://`）。
///
/// 为什么不能直接用 [`validate_outbound_url`]：那个函数的 scheme 白名单只有
/// `https`/`http`，代理节点的 `socks5://` 会被它直接拒掉。
///
/// # 这不是 fail-closed，别当它是
///
/// 本函数只拦**当场解析得到的**内网/保留地址。三个已知缺口：
///
/// 1. **DNS 失败放行**（下方 `Err(_) => Ok(())`）。与 `validate_outbound_url` 同口径，
///    但那个函数的 fail-open 有「出站禁重定向」兜底，代理隧道**没有**对应兜底。
/// 2. **不在使用时复验**。入表时解析到公网、之后 DNS 重指到内网（短 TTL / DNS 重绑定），
///    reqwest 每次连接自行解析，没有 `resolve_to_addrs` 固定，于是照走内网。
/// 3. **旁路存在**：`set_credential_proxy` 与 `/proxy/test` 都不做任何地址校验，
///    同一个内网地址从那两条路进来不受本函数管辖。
///
/// 所以本函数的定位是**降低误配概率**（管理员手滑填了 `127.0.0.1`），
/// 不是安全边界。要真正封住需要：使用时复验 + 固定解析结果 + 覆盖另两条入口。
///
/// 之所以仍然拦：节点地址会被写进凭据并在请求热路径上使用，
/// 允许 `socks5://127.0.0.1:x` 等于把网关变成一个可被指使的内网探测器
/// （逐个试 `socks5://10.0.0.x:port`，靠测速的成功/失败与延迟当信号）。
///
/// # 策略：[`SsrfPolicy::AdminConfigured`]（与 custom_api base_url 同口径）
///
/// 节点地址是**管理员过了 adminKey 鉴权后亲手填的**，与 custom_api 的 base_url 同类，
/// 故用同一套策略。这只放开 198.18.0.0/15（RFC 2544 基准段）一段，理由见
/// [`is_forbidden_ipv4_with`]：Clash / Mihomo / Surge 的 fake-IP 池默认就是该段，
/// 开了 fake-IP 的机器上**任意域名**都解析到 198.18.x.x —— `Strict` 会让管理员
/// 连一个域名形式的代理节点都加不进来（本仓已知问题 #19 的同源缺陷）。
///
/// ⚠️ **它只放开了基准段与字面量环回**：`socks5://127.0.0.1:40002`（本机 `ssh -D`
/// 隧道，2026-08-13 起放行——本机回环无横向面，与 custom_api base_url 同口径）；
/// `socks5://192.168.x.x:7890`（局域网 Clash/gluetun 旁车）**仍然被拒**。
/// 旁车形态需要一个显式的配置开关（类似 `trustForwardedHeader` 那种「管理员知情下
/// 放开」的旋钮），不能靠换策略解决 —— `AdminConfigured` 的豁免范围只有
/// 基准段 + 字面量环回两条（内嵌混淆仍拒）。
pub async fn validate_proxy_address(url: &str) -> Result<(), String> {
    let scheme = url
        .split_once("://")
        .map(|(s, _)| s.to_ascii_lowercase())
        .ok_or_else(|| "代理地址缺少 scheme（应形如 socks5://host:port）".to_string())?;
    const ALLOWED: &[&str] = &["socks5", "socks5h", "http", "https"];
    if !ALLOWED.iter().any(|s| *s == scheme) {
        return Err(format!(
            "代理 scheme 不被允许（仅 socks5/socks5h/http/https）: {scheme}"
        ));
    }
    let (host, port) = parse_host_port(url)?;
    match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(iter) => {
            for sa in iter {
                if is_forbidden_ip_with(sa.ip(), SsrfPolicy::AdminConfigured) {
                    return Err(describe_rejection(sa.ip()));
                }
            }
            Ok(())
        }
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
    if !allowed_schemes
        .iter()
        .any(|s| s.eq_ignore_ascii_case(&scheme))
    {
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

    /// `socks5://` 必须被接受 —— 这正是不能复用 `validate_outbound_url` 的原因。
    ///
    /// 回退即 FAIL：把 `validate_proxy_address` 换成 `validate_outbound_url(url, true)`，
    /// 第一条断言失败（scheme 白名单只有 https/http）→ 任何 SOCKS 节点都存不进去。
    ///
    /// ⚠️ 用 `.invalid` TLD（RFC 6761 保证永不解析）：测试不得依赖真实 DNS，
    /// 否则开 fake-IP 代理的机器上会走到禁止段判定（本仓已知问题 #19 的成因）。
    ///
    /// 最后那条 198.18.0.46 是**策略断言**：节点地址走
    /// [`SsrfPolicy::AdminConfigured`]，故 fake-IP 池默认段（198.18.0.0/15）必须放行。
    /// 回退即 FAIL：把策略改回 `Strict`，该条失败 —— 开了 Clash fake-IP 的机器上
    /// 任意域名都解析到这一段，管理员一个域名形式的节点都加不进来。
    #[tokio::test]
    async fn proxy_address_accepts_socks_schemes_and_rejects_others() {
        for ok in [
            "socks5://node.invalid:40002",
            "socks5h://node.invalid:40002",
            "http://node.invalid:8080",
            "https://node.invalid:443",
            // fake-IP 池段：AdminConfigured 下唯一被豁免的禁止段。
            "socks5://198.18.0.46:40002",
        ] {
            assert!(
                validate_proxy_address(ok).await.is_ok(),
                "{ok} 应被接受（代理节点允许 socks5）"
            );
        }
        for bad in [
            "ftp://node.invalid:21",
            "file:///etc/passwd",
            "node.invalid:1080",
        ] {
            assert!(validate_proxy_address(bad).await.is_err(), "{bad} 应被拒绝");
        }
    }

    /// 内网/环回**IP 字面量**必须拒绝 —— 换用 `AdminConfigured` 后**依然如此**。
    ///
    /// 这条同时是策略豁免范围的边界断言：`AdminConfigured` 只放开
    /// 198.18.0.0/15（见上一条测试），**其余禁止段一个都没放开**。
    /// 逐条按 `is_forbidden_ip_with` 的实际判据挑：环回 / RFC1918 两段 / CGNAT /
    /// 链路本地(含云元数据) / 文档段 / IPv6 环回 / ULA / 6to4 内嵌元数据地址。
    ///
    /// ⚠️ 这条只覆盖字面量。域名走 DNS 失败分支时是**放行**的，
    /// 且没有使用时复验 —— 见 `validate_proxy_address` 的「这不是 fail-closed」一节。
    ///
    /// 回退即 FAIL：删掉 `is_forbidden_ip_with` 那道检查 → 管理员可填
    /// `socks5://127.0.0.1:x` / `socks5://10.0.0.x:x`，把网关变成可被指使的
    /// 内网扫描器（用测速接口的成功/失败当探测信号）。
    #[tokio::test]
    async fn proxy_address_rejects_internal_targets() {
        // 2026-08-13：字面量环回代理（socks5://127.0.0.1、[::1]）放行——本机
        // v2rayN/ssh -D 隧道是合法代理场景，环回无横向面。其余内网/保留段仍拒绝。
        for bad in [
            "socks5://10.0.0.5:1080",
            "socks5://192.168.1.1:1080",
            "socks5://172.16.0.1:1080",
            "socks5://100.64.0.1:1080",
            "socks5://169.254.169.254:1080",
            "socks5://198.51.100.7:1080",
            "http://[fc00::1]:8080",
            // 6to4 内嵌 169.254.169.254：AdminConfigured 也不豁免内嵌形式。
            "http://[2002:a9fe:a9fe::1]:8080",
        ] {
            assert!(
                validate_proxy_address(bad).await.is_err(),
                "{bad} 指向内网/环回/保留段，AdminConfigured 下必须仍然拒绝"
            );
        }
    }

    #[test]
    fn test_forbidden_ipv4() {
        // 内网/环回/链路本地/元数据/多播/保留一律禁止
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.5.5",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "192.0.2.5",
            "198.18.0.1",
        ] {
            assert!(is_forbidden_ip(ip.parse().unwrap()), "{ip} 应被禁止");
        }
    }

    #[test]
    fn test_allowed_ipv4() {
        for ip in ["8.8.8.8", "1.1.1.1", "210.140.92.183"] {
            assert!(!is_forbidden_ip(ip.parse().unwrap()), "{ip} 应被放行");
        }
    }

    /// 管理员显式配置的出站目标：198.18.0.0/15 必须放行。
    ///
    /// 这是生产缺陷的回归守卫：该段是 Clash/Mihomo fake-IP 模式的默认地址池，
    /// 开着此类代理的机器上**任何**域名都解析到这里（实测 api.uu6.top → 198.18.0.46），
    /// 严格策略下管理员无法添加任何 custom_api 中转站。
    #[test]
    fn should_allow_benchmark_range_for_admin_configured_targets() {
        for ip in ["198.18.0.46", "198.18.0.1", "198.19.255.255"] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(
                is_forbidden_ip_with(addr, SsrfPolicy::Strict),
                "{ip} 在严格策略下仍应拒绝（匿名端点的威胁模型不变）"
            );
            assert!(
                !is_forbidden_ip_with(addr, SsrfPolicy::AdminConfigured),
                "{ip} 对管理员亲手配置的目标应放行，否则 fake-IP 环境下无法上号"
            );
        }
    }

    /// 管理员配置下环回（127.0.0.0/8、::1）放行（本机服务互转合法场景，
    /// 2026-08-13）：网关 → 本机 shield → 本机 fuckopencode。严格策略下仍拒绝。
    /// 内嵌混淆（Teredo/ISATAP 解混淆成环回）在两种策略下都必须拒绝（绕过口未开）。
    #[test]
    fn admin_configured_allows_literal_loopback_only() {
        for ip in ["127.0.0.1", "127.0.0.2", "::1"] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(
                is_forbidden_ip_with(addr, SsrfPolicy::Strict),
                "{ip} 在严格策略下仍应拒绝（匿名端点不得打环回）"
            );
            assert!(
                !is_forbidden_ip_with(addr, SsrfPolicy::AdminConfigured),
                "{ip} 管理员亲手填写 base_url 时应放行（本机回环）"
            );
        }
        // 私网段 / 链路本地 / 元数据：管理员配置下仍拒绝（横向/云元数据面不变）。
        for ip in ["10.0.0.1", "172.16.0.1", "192.168.1.1", "169.254.169.254"] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(
                is_forbidden_ip_with(addr, SsrfPolicy::AdminConfigured),
                "{ip} 管理员配置下私网/元数据仍必须拒绝"
            );
        }
    }

    /// 豁免范围必须**只有** 198.18.0.0/15 —— 内网与元数据端点在两种策略下都得拦。
    ///
    /// 这条是放宽策略的安全边界守卫：若将来有人把豁免扩大到整个 forbidden 集合，
    /// 这个测试会立刻失败。
    #[test]
    fn should_still_reject_internal_targets_even_when_admin_configured() {
        for ip in [
            "169.254.169.254", // 云元数据端点：最主要的 SSRF 攻击目标
            // 2026-08-13：127.0.0.1 字面量环回已在 AdminConfigured 下放行（本机服务互转），
            // 从拒绝列表移除；内嵌混淆环回（Teredo/ISATAP 解混淆）仍拒绝（见内嵌测试）。
            "10.0.0.1",
            "172.16.5.5",
            "192.168.1.1",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "192.0.2.5", // 文档段：与基准段相邻但不豁免
            "198.51.100.7",
            "203.0.113.9",
        ] {
            assert!(
                is_forbidden_ip_with(ip.parse().unwrap(), SsrfPolicy::AdminConfigured),
                "{ip} 即便是管理员配置也必须拒绝"
            );
        }
    }

    /// 内嵌 v4 的 IPv6 形式必须与裸 IPv4 判定一致（否则 ::ffff:198.18.0.46 成了绕过口）。
    #[test]
    fn should_apply_same_policy_to_v4_mapped_and_nat64_and_6to4() {
        // 基准段：AdminConfigured 下三种内嵌形式都应与裸 IP 一样被放行
        for ip in [
            "::ffff:198.18.0.46",
            "64:ff9b::198.18.0.46",
            "2002:c612:002e::",
        ] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(
                !is_forbidden_ip_with(addr, SsrfPolicy::AdminConfigured),
                "{ip} 内嵌基准段，应与裸 IP 判定一致（放行）"
            );
            assert!(
                is_forbidden_ip_with(addr, SsrfPolicy::Strict),
                "{ip} 严格策略下仍应拒绝"
            );
        }
        // 元数据端点：任何内嵌形式在任何策略下都必须拦
        for ip in [
            "::ffff:169.254.169.254",
            "64:ff9b::169.254.169.254",
            "2002:a9fe:a9fe::",
        ] {
            assert!(
                is_forbidden_ip_with(ip.parse().unwrap(), SsrfPolicy::AdminConfigured),
                "{ip} 内嵌元数据端点，必须拒绝"
            );
        }
    }

    /// Teredo / ISATAP 内嵌 v4 同样必须按裸 IPv4 同口径判定（否则成为绕过口）。
    ///
    /// - Teredo (RFC 4380): `2001:0000::/32`，客户端 v4 在最后 32 位且按位取反混淆。
    ///   `2001:0000:1234:5678:8000:0000:8000:00fe` 解混淆后 = 127.255.255.1（环回）。
    /// - ISATAP (RFC 5214): 接口标识符 `0000:5EFE:xxxx:xxxx`，后 32 位为内嵌 v4。
    ///   `2001:db8::5efe:7f00:1` = 127.0.0.1；`2001:db8::5efe:a9fe:a9fe` = 169.254.169.254。
    ///
    /// 内嵌**公网** v4 时仍放行（只按内嵌的 v4 是否命中禁止段判定，与 6to4/NAT64 同口径）。
    #[test]
    fn should_apply_same_policy_to_teredo_and_isatap() {
        // 内嵌环回/链路本地：两种策略下都必须拦
        for ip in [
            "2001:0000:1234:5678:8000:0000:8000:00fe", // Teredo → 127.255.255.1
            "2001:0000:1234:5678:ffff:ffff:ffff:fffe", // Teredo → 0.0.0.0
            "2001:db8::5efe:7f00:1",                   // ISATAP → 127.0.0.1
            "2001:db8::5efe:a9fe:a9fe",                // ISATAP → 169.254.169.254
        ] {
            assert!(
                is_forbidden_ip_with(ip.parse().unwrap(), SsrfPolicy::Strict),
                "{ip} 严格策略下内嵌环回/元数据必须拒绝"
            );
            assert!(
                is_forbidden_ip_with(ip.parse().unwrap(), SsrfPolicy::AdminConfigured),
                "{ip} 管理员配置下内嵌环回/元数据也必须拒绝"
            );
        }
        // 内嵌公网 v4：放行（与裸 8.8.8.8 一致）
        let teredo_public = "2001:0000:1234:5678:ffff:ffff:f7f7:f7f7"; // Teredo → 8.8.8.8
        assert!(
            !is_forbidden_ip(teredo_public.parse().unwrap()),
            "Teredo 内嵌公网 v4 应放行: {teredo_public}"
        );
        let isatap_public = "2001:db8::5efe:808:808"; // ISATAP → 8.8.8.8
        assert!(
            !is_forbidden_ip(isatap_public.parse().unwrap()),
            "ISATAP 内嵌公网 v4 应放行: {isatap_public}"
        );
    }

    /// 拒绝原因必须可诊断：命中基准段时要点出「代理 fake-IP」这个真实原因。
    #[test]
    fn should_explain_fake_ip_cause_in_rejection_message() {
        let msg = describe_rejection("198.18.0.46".parse().unwrap());
        assert!(msg.contains("198.18.0.46"), "要带上实际解析到的 IP: {msg}");
        assert!(
            msg.contains("fake-IP"),
            "要点出 fake-IP 这个真实原因: {msg}"
        );

        // 内网目标不应误报成 fake-IP 问题
        let msg2 = describe_rejection("169.254.169.254".parse().unwrap());
        assert!(msg2.contains("云元数据"), "元数据端点要如实说明: {msg2}");
        assert!(!msg2.contains("fake-IP"), "不应误导为代理问题: {msg2}");
    }

    #[test]
    fn test_forbidden_ipv6() {
        for ip in [
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            // 6to4 (RFC 3056): 2002::/16 内嵌私有/回环 IPv4
            "2002:7f00:0001::", // 内嵌 127.0.0.1
            "2002:a9fe:a9fe::", // 内嵌 169.254.169.254
            "2002:0a00:0001::", // 内嵌 10.0.0.1
            "2002:c0a8:0101::", // 内嵌 192.168.1.1
        ] {
            assert!(
                is_forbidden_ip(ip.parse().unwrap()),
                "{ip} 应被禁止（含6to4嵌私有/回环IPv4）"
            );
        }
    }

    #[test]
    fn test_allowed_ipv6() {
        assert!(!is_forbidden_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn test_parse_host_port() {
        assert_eq!(
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
        assert!(
            validate_outbound_url("http://169.254.169.254/latest/meta-data", true)
                .await
                .is_err()
        );
        assert!(
            validate_outbound_url("https://127.0.0.1/v1/messages", true)
                .await
                .is_err()
        );
        assert!(
            validate_outbound_url("http://10.0.0.1:6379", true)
                .await
                .is_err()
        );
        assert!(validate_outbound_url("http://[::1]/x", true).await.is_err());
        // userinfo 混淆：@ 后是内网 → 拒绝（parse_host_port 剥 userinfo 取真实 host）。
        assert!(
            validate_outbound_url("https://ok.com@169.254.169.254/x", true)
                .await
                .is_err()
        );
        // scheme 门：allow_http=false 时 http 被拒。
        assert!(
            validate_outbound_url("http://8.8.8.8/x", false)
                .await
                .is_err()
        );
        // 非 http(s) scheme 一律拒。
        assert!(
            validate_outbound_url("ftp://8.8.8.8/x", true)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_validate_outbound_url_allows_public_ip() {
        // 公网 IP 字面量放行（用 IP 免真实 DNS 依赖）。
        assert!(
            validate_outbound_url("https://8.8.8.8/v1/messages", false)
                .await
                .is_ok()
        );
        assert!(
            validate_outbound_url("http://1.1.1.1/x", true)
                .await
                .is_ok()
        );
        // allow_http=false 下 https 公网放行。
        assert!(
            validate_outbound_url("https://1.1.1.1/x", false)
                .await
                .is_ok()
        );
    }
}
