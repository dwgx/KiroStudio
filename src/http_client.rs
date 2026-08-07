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
            let dec = |s: &str| {
                urlencoding::decode(s)
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| s.to_string())
            };
            let user = if u.is_empty() { None } else { Some(dec(u)) };
            let pass = p.and_then(|p| if p.is_empty() { None } else { Some(dec(p)) });
            (user, pass)
        }
    };

    (clean_url, username, password)
}

/// 一条 SOCKS/HTTP 代理**分享链接**解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedProxyLink {
    /// 干净 URL（`socks5://host:port`，已剥 userinfo 与 `#fragment`）。
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    /// `#` 之后的展示名（如 `US-1-SOCKS5`）；无则 None。
    pub name: Option<String>,
}

/// 一行代理文本**为什么**没能解析出节点。
///
/// 存在的理由：原先解析失败只有 `None` 一种表达，界面只能说「跳过 N 行非链接文本」——
/// 用户无法区分「这行本来就不是链接（标题/分隔线）」与「这行是链接但端口写错了」。
/// 前者不需要动作，后者需要用户改数据，两者却长得一模一样。
///
/// `code()` 返回**稳定字符串**给前端做 i18n 映射：后端不返回译文，
/// 否则面板语言切换对这段文案无效（其余 Admin API 同口径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyLineIssue {
    /// 压根不像代理行（空行、`#` 注释、标题、`地址  : 1.2.3.4` 这类说明行）。**安静跳过**。
    NotProxy,
    /// 像代理行但缺 `host:port` 结构（如 `socks5://nohostcolon`）。
    NoHostPort,
    /// host 字符集不合法（含空白/中文/`:` 等）。
    BadHost,
    /// 端口不是 1..=65535 的纯数字（`0` / `65536` / `dwgxdwgx`）。
    BadPort,
    /// 冒号形态两种读法都成立，**判不定**（如 `1.2.3.4:8080:5.6.7.8:9090`）。
    /// 绝不猜：猜错造出的假节点表现为「节点不通」，比直接报错难查得多。
    AmbiguousColonForm,
}

impl ProxyLineIssue {
    /// 稳定错误码（前端按它查 i18n；改动等于破坏前端契约）。
    pub fn code(self) -> &'static str {
        match self {
            Self::NotProxy => "not_proxy",
            Self::NoHostPort => "no_host_port",
            Self::BadHost => "bad_host",
            Self::BadPort => "bad_port",
            Self::AmbiguousColonForm => "ambiguous",
        }
    }
}

/// 解析代理分享链接，支持三种 userinfo 写法 + `#name` 尾注。
///
/// # 为什么需要它（[`split_proxy_credentials`] 不够）
///
/// 那个函数只做**百分号解码**，而机场/节点商实际下发的是 **base64 userinfo**：
///
/// ```text
/// socks://dXMxdTpwZUxBck9sWWNDSWZHUmxzcFEzZ1lkRHBkMGs5Zzd1aA@192.220.50.26:40002#US-1-SOCKS5
///         └────────────── base64("us1u:peLArOlYcC…") ──────────────┘             └─ 展示名 ─┘
/// ```
///
/// 直接喂给 `split_proxy_credentials` 会把整个 base64 串当成**用户名**（因为里面没有
/// `:`），密码为 None ⇒ 代理认证必然失败 ⇒ 而失败长得像"节点不通"，会把排查带偏。
/// `#US-1-SOCKS5` 还会被当成 host 的一部分留在 URL 里。
///
/// # 判据顺序（承重）
///
/// 1. 先剥 `#fragment` —— 否则它会污染 host（`40002#US-1` 不是合法端口）。
/// 2. userinfo 含 `:` ⇒ 当**明文** `user:pass`（不试 base64）。
///    理由：明文密码完全可能**恰好**是合法 base64，先试 base64 会把明文解成乱码。
/// 3. 不含 `:` 且能 base64 解出含 `:` 的 UTF-8 ⇒ 当 base64。
/// 4. 都不满足 ⇒ 当纯用户名无密码（保持 `split_proxy_credentials` 的旧行为）。
///
/// `scheme` 统一归一到 `socks5://`（`socks://` 是分享链接惯例，reqwest 不认它）。
///
/// # 冒号形态（`host:port:user:pass` 等）
///
/// 本函数只认「有 `@` 分隔 userinfo」的规范形态。代理商还大量下发纯冒号分隔的
/// `host:port:user:pass`，那类输入先由 [`normalize_proxy_line`] 重写成规范形态再进来 ——
/// 归一化**在外面**做是刻意的：本函数的四条 userinfo 判据靠「userinfo 是否含 `:`」区分
/// 明文与 base64，若把「数冒号」的分支塞进 `rsplit_once('@')` 之前，
/// `socks5://user:p:ss@host:1080`（密码含 `:`，现由判据 2 正确处理）的语义就会变成
/// 依赖冒号总数。两层分开后各自可独立测试，且这一层的行为逐字不变。
pub fn parse_proxy_link(raw: &str) -> Option<ParsedProxyLink> {
    // 先试归一化（冒号形态 / `host:port@user:pass` 倒装），失败则原样喂给严格解析。
    // 单条 API 与批量导入共用这一处，避免同一逻辑各写一份而漏改（见 update.rs 那次教训）。
    match normalize_proxy_line(raw) {
        NormalizedLine::Rewritten(s) => parse_proxy_link_strict(&s).ok(),
        NormalizedLine::AsIs => parse_proxy_link_strict(raw).ok(),
        // 冒号形态但判不定 ⇒ 绝不猜。猜错造出的假节点表现为「节点不通」，
        // 比直接报错难查得多。
        NormalizedLine::Rejected(_) => None,
    }
}

/// [`parse_proxy_link`] 的严格核心：**不做**任何冒号形态归一，只认 `[scheme://][userinfo@]host:port[#name]`。
///
/// 返回 `Err(ProxyLineIssue)` 而非 `Option`，让批量导入能把「为什么这行不行」告诉用户 ——
/// 原先返回 `None` 时，界面只能说「跳过 N 行非链接文本」，用户无法区分
/// 「这行不是链接」和「这行是链接但端口写错了」。
fn parse_proxy_link_strict(raw: &str) -> Result<ParsedProxyLink, ProxyLineIssue> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Err(ProxyLineIssue::NotProxy);
    }

    // ① 先剥 fragment（必须最先做，见判据 1）
    let (body, name) = match trimmed.split_once('#') {
        Some((b, n)) => {
            let n = n.trim();
            (
                b,
                if n.is_empty() {
                    None
                } else {
                    Some(n.to_string())
                },
            )
        }
        None => (trimmed, None),
    };

    let (scheme, rest) = match body.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        // 无 scheme：视为 socks5（节点明细里常见裸 host:port）
        None => ("socks5".to_string(), body),
    };
    // `socks://` 是分享链接惯例，reqwest 只认 socks5/socks5h。其余 scheme 原样保留。
    let scheme = match scheme.as_str() {
        "socks" => "socks5".to_string(),
        s => s.to_string(),
    };

    let (userinfo, hostport) = match rest.rsplit_once('@') {
        Some((ui, hp)) => (Some(ui), hp),
        None => (None, rest),
    };
    // host:port 必须**结构上成立**：host 非空且不含空白，port 是纯数字且在 1..=65535。
    //
    // ⚠️ 只判「含 ':'」是不够的：节点商文档里的说明行 `端口  : 40002` 同样含 ':'，
    // 而无 scheme 时的兜底会把它当成 host:port ⇒ 造出一个 host="端口" 的假节点。
    // 批量解析那层有 `://` + `@` 双条件挡着，但**单条 API 调用方没有**，
    // 所以校验必须在这里，不能只依赖调用方。
    let hostport = hostport.trim();
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.trim(), p.trim()),
        None => return Err(ProxyLineIssue::NoHostPort),
    };
    // host 字符集：字母数字 + `.` `-` `_`，或 IPv6 的 `[::1]` 形式。
    //
    // ⚠️ 光判「非空 + 无空白」不够：`端口  : 40002` 被 trim 后 host="端口"，
    // 非空且无空白 ⇒ 会通过。必须按字符集判，否则说明行会造出假节点
    // （而假节点会拼出无法解析的代理地址，失败长得像"节点不通"）。
    let host_ok = if host.starts_with('[') && host.ends_with(']') {
        let inner = &host[1..host.len() - 1];
        !inner.is_empty()
            && inner
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
    } else {
        !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
    };
    if !host_ok {
        return Err(ProxyLineIssue::BadHost);
    }
    match port.parse::<u32>() {
        Ok(n) if (1..=65535).contains(&n) => {}
        _ => return Err(ProxyLineIssue::BadPort),
    }
    let hostport = format!("{host}:{port}");

    let (username, password) = match userinfo.map(str::trim).filter(|s| !s.is_empty()) {
        None => (None, None),
        Some(ui) => {
            if ui.contains(':') {
                // 判据 2：明文 user:pass
                let (u, p) = ui.split_once(':').unwrap();
                let dec = |s: &str| {
                    urlencoding::decode(s)
                        .map(|c| c.into_owned())
                        .unwrap_or_else(|_| s.to_string())
                };
                (
                    Some(dec(u)).filter(|s| !s.is_empty()),
                    Some(dec(p)).filter(|s| !s.is_empty()),
                )
            } else {
                // 判据 3：尝试 base64（补 padding；同时接受标准表与 URL-safe 表）
                let padded = {
                    let mut s = ui.to_string();
                    while s.len() % 4 != 0 {
                        s.push('=');
                    }
                    s
                };
                let decoded = base64_decode_loose(&padded)
                    .and_then(|b| String::from_utf8(b).ok())
                    .filter(|s| s.contains(':'));
                match decoded {
                    Some(d) => {
                        let (u, p) = d.split_once(':').unwrap();
                        (
                            Some(u.to_string()).filter(|s| !s.is_empty()),
                            Some(p.to_string()).filter(|s| !s.is_empty()),
                        )
                    }
                    // 判据 4：当纯用户名
                    None => (Some(ui.to_string()), None),
                }
            }
        }
    };

    Ok(ParsedProxyLink {
        url: format!("{scheme}://{hostport}"),
        username,
        password,
        name,
    })
}

/// 宽松 base64 解码：同时接受标准表（`+/`）与 URL-safe 表（`-_`）。
///
/// 不引新依赖 —— 手写 12 行查表，避免为一个解码函数把 `base64` crate 拉进
/// 生产依赖树（本仓库对新依赖一向保守）。
fn base64_decode_loose(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u8> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        })
    };
    let bytes: Vec<u8> = s
        .bytes()
        .filter(|&c| c != b'=' && !c.is_ascii_whitespace())
        .collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut buf = [0u8; 4];
        for (i, &c) in chunk.iter().enumerate() {
            buf[i] = val(c)?;
        }
        let n = chunk.len();
        // 每 4 个 6-bit 组装成 3 字节；末组按实际长度截断。
        out.push((buf[0] << 2) | (buf[1] >> 4));
        if n > 2 {
            out.push((buf[1] << 4) | (buf[2] >> 2));
        }
        if n > 3 {
            out.push((buf[2] << 6) | buf[3]);
        }
    }
    Some(out)
}

/// [`normalize_proxy_line`] 的三种结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizedLine {
    /// 已重写成 `scheme://user:pass@host:port[#name]` 规范形态。
    Rewritten(String),
    /// 不是本层负责的形态，原样交给严格解析（**行为逐字不变**）。
    AsIs,
    /// 是冒号形态但判不定 —— 必须拒绝并把原因报给用户。
    Rejected(ProxyLineIssue),
}

/// 判断一段文本是否**像 host 字面量**（IPv4 / 含点域名 / `[IPv6]`）。
///
/// 这是消歧判据 2 的实现，也是「要不要把这行当代理数据看」的闸门。
/// **要求含 `.` 或方括号**是承重的：没有它，`端口:40002:说明:文本` 这类文档行会被
/// 当成 host="端口" 的节点数据。单标签主机名（`myproxy:1080:u:p`）因此**不被**冒号形态
/// 接受 —— 代理商导出的冒号形态里 host 恒为 IP，而放宽它就等于让任意
/// `词:数字:词:词` 的说明行造出假节点，代价不对等。
/// （带 scheme 或带 `@` 的规范形态不走这条判据，单标签主机名在那边照常可用。）
fn looks_like_host_literal(s: &str) -> bool {
    if s.starts_with('[') && s.ends_with(']') && s.len() > 2 {
        let inner = &s[1..s.len() - 1];
        return inner
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.');
    }
    s.contains('.')
        && !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// 这一段是否是合法端口（纯数字且 1..=65535）。
fn is_port_seg(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_digit())
        && matches!(s.parse::<u32>(), Ok(n) if (1..=65535).contains(&n))
}

/// 按 `:` 切段并带上每段在原串中的起始偏移，但把 `[...]` 里的 IPv6 冒号当成整体。
///
/// 不做这层保护的话，`[2001:db8::1]:1080:u:p` 会被切成 7 段而彻底判不定。
///
/// **偏移是必须的**：读法 A 的密码要吃掉「第 3 个 `:` 之后的全部余下」，
/// 而 `str::splitn(4, ':')` 不认方括号 —— 用它算尾巴会把
/// `[2001:db8::1]:1080:u:p` 切成 `[2001` / `db8` / `` / `1]:1080:u:p`。
fn split_colon_segments_indexed(s: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0usize;
    let mut depth = 0i32;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'[' => depth += 1,
            b']' => depth -= 1,
            b':' if depth <= 0 => {
                out.push((start, &s[start..i]));
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push((start, &s[start..]));
    out
}

/// [`split_colon_segments_indexed`] 的只要段不要偏移版。
fn split_colon_segments(s: &str) -> Vec<&str> {
    split_colon_segments_indexed(s)
        .into_iter()
        .map(|(_, seg)| seg)
        .collect()
}

/// 把代理商常见的**非规范**写法重写成 `parse_proxy_link` 认得的规范形态。
///
/// 负责三种（其余原样放过）：
///
/// | 形态 | 判据 | 例 |
/// |---|---|---|
/// | `host:port:user:pass` | 第 2 段是合法端口 | `130.180.228.34:6318:u:p` |
/// | `user:pass:host:port` | 第 4 段是合法端口（**恰好 4 段**） | `u:p:130.180.228.34:6318` |
/// | `host:port@user:pass` | `@` 左侧是合法 host:port 且右侧含 `:` | `1.2.3.4:1080@u:p` |
///
/// # 🔴 消歧判据（按优先级，第一条命中即定，**绝不猜**）
///
/// 1. 只有一种读法的那段是合法端口 ⇒ 采用该读法。
/// 2. 两段都是合法端口 ⇒ 看哪个 host 候选**像 host 字面量**（IPv4/含点域名/`[IPv6]`）；
///    恰好一个像 ⇒ 采用它。
/// 3. 两条都判不定（都像 / 都不像，如 `1.2.3.4:8080:5.6.7.8:9090`）⇒
///    `Rejected(AmbiguousColonForm)`。
///
/// 为什么不许猜：当前的失败模式是**静默跳过**（用户看到 0 条成功，明确知道要改格式）。
/// 猜错则变成**造出一个假节点**，表现为「节点不通」—— 那要翻代理日志才能定位，
/// 比直接报错难查得多。
///
/// # 5 段及以上
///
/// 只允许读法 1，密码取第 3 个 `:` 之后的**全部余下**（`splitn(4)`），
/// 于是 `host:port:user:pa:ss` 的密码是 `pa:ss`。读法 2 在 5 段起不再尝试 ——
/// 那时「多出来的冒号属于用户名还是密码」本身没有判据。
///
/// # 裸 IPv6 一律不接
///
/// `2001:db8::1:1080:u:p` 无解（IPv6 自带大量 `:`），必须写成 `[2001:db8::1]:1080:u:p`。
pub fn normalize_proxy_line(raw: &str) -> NormalizedLine {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return NormalizedLine::AsIs;
    }

    // ① fragment 必须最先剥（与 parse_proxy_link 判据 1 同序）：否则
    // `host:port:user:pass#名字` 的 `#名字` 会黏在密码上。
    let (body, frag) = match trimmed.split_once('#') {
        Some((b, f)) => (b.trim(), Some(f)),
        None => (trimmed, None),
    };
    // ② scheme 也先剥：`socks5://1.2.3.4:1080:u:p` 里的 `//` 会干扰数段。
    //    重写时原样拼回去（归一化不改 scheme 语义，socks→socks5 由严格解析层负责）。
    let (scheme_prefix, rest) = match body.split_once("://") {
        Some((s, r)) => (format!("{s}://"), r),
        None => (String::new(), body),
    };

    let rebuild = |user: &str, pass: &str, host: &str, port: &str| -> NormalizedLine {
        let mut s = format!("{scheme_prefix}{user}:{pass}@{host}:{port}");
        if let Some(f) = frag {
            s.push('#');
            s.push_str(f);
        }
        NormalizedLine::Rewritten(s)
    };

    // ③ 已含 `@` ⇒ 本就是规范形态，**不走冒号形态**。
    //    这道先手是承重的：`user:p:ss@host:1080`（密码含 `:`）也有 4 段，
    //    若先数冒号就会把它重写坏 —— 而它现在由严格解析的判据 2 正确处理。
    if let Some((left, right)) = rest.rsplit_once('@') {
        // 唯一例外：`host:port@user:pass` 倒装。仅当**标准读法解不出**
        // （即 `right` 不是合法 host:port）且左侧确实是 host:port 时才交换，
        // 故 `1.2.3.4:1080@5.6.7.8:8080`（两侧都合法）保持标准读法不变。
        if parse_proxy_link_strict(right).is_err() && right.contains(':') {
            let l = split_colon_segments(left);
            if l.len() == 2 && looks_like_host_literal(l[0]) && is_port_seg(l[1]) {
                let (u, p) = right.split_once(':').unwrap();
                return rebuild(u, p, l[0], l[1]);
            }
        }
        return NormalizedLine::AsIs;
    }

    let indexed = split_colon_segments_indexed(rest);
    let segs: Vec<&str> = indexed.iter().map(|(_, s)| *s).collect();
    // 少于 4 段（`host:port` / `host:port:user`）维持原有行为：前者严格解析接受，
    // 后者被拒。放宽它会让 `port : 40002` 这类说明行造出假节点。
    if segs.len() < 4 {
        return NormalizedLine::AsIs;
    }

    // 读法 A：host:port:user:pass（≥4 段都可，密码吃掉余下全部冒号）
    let a_ok = is_port_seg(segs[1]) && looks_like_host_literal(segs[0]);
    // 读法 B：user:pass:host:port（**仅**恰好 4 段）
    let b_ok = segs.len() == 4 && is_port_seg(segs[3]) && looks_like_host_literal(segs[2]);

    let use_a = match (a_ok, b_ok) {
        (true, false) => true,
        (false, true) => false,
        // 判据 3：两读法都成立 ⇒ 判不定。此处两个 host 候选都已过
        // looks_like_host_literal（判据 2 内含在 a_ok/b_ok 里），所以无从区分。
        (true, true) => return NormalizedLine::Rejected(ProxyLineIssue::AmbiguousColonForm),
        // 都不成立：不是冒号形态（可能是中文说明行、时间戳、裸 IPv6）⇒
        // 原样交给严格解析，由它给出 BadHost/BadPort/NotProxy。
        (false, false) => return NormalizedLine::AsIs,
    };

    if use_a {
        // 密码 = 第 4 段起的**全部余下**（`host:port:user:pa:ss` ⇒ pass=`pa:ss`）。
        // 用带偏移的切段而非 `rest.splitn(4, ':')`：后者不认方括号，
        // 会把 `[2001:db8::1]:1080:u:p` 切坏（该缺陷已被 colon_form_boundaries_rejected 抓到）。
        let pass = &rest[indexed[3].0..];
        rebuild(segs[2], pass, segs[0], segs[1])
    } else {
        rebuild(segs[0], segs[1], segs[2], segs[3])
    }
}

/// 单行文本的分类结果（批量导入的逐行报告用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyLineVerdict {
    /// 解析成功。
    Parsed(ParsedProxyLink),
    /// 不像代理行，**安静跳过**（标题/分隔线/说明文字/curl 示例）。
    Skipped,
    /// 像代理行但有问题 —— 必须报给用户。
    Invalid(ProxyLineIssue),
}

/// 一行文本 → 解析成功 / 安静跳过 / 报错。
///
/// # 🔴 判据顺序（承重，改序即回归）
///
/// 「像不像链接」的闸门必须排在**归一化之后**：原先它是
/// `t.contains("://") && t.contains('@')`，排在 `parse_proxy_link` **之前**，
/// 于是纯冒号形态（一个 `://` 和 `@` 都没有）**根本走不到解析器**。
/// 只改解析器而不动这道闸门 ⇒ 纯函数测试全绿而功能依然不通
/// （与 2026-08-05 那次 INSUFFICIENT_MODEL_CAPACITY 无效修复同构：
/// 判据改对了，但更靠前的分支先短路）。
///
/// 闸门放宽为「以 `scheme://` 开头 **或** 含 `@` 且能解析 **或** 归一化成功」。
/// 三条都不满足才安静跳过，故 `地址  : 1.2.3.4`（无 `@`、无 scheme、只有 1 个 `:`）
/// 仍被跳过，而 `user:pass@1.2.3.4:1080`（单条 API 早就支持、批量却跳过的老缺口）
/// 现在也能进。
pub fn classify_proxy_line(line: &str) -> ProxyLineVerdict {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return ProxyLineVerdict::Skipped;
    }
    let cleaned = strip_line_quotes(t);

    match normalize_proxy_line(cleaned) {
        NormalizedLine::Rewritten(s) => match parse_proxy_link_strict(&s) {
            Ok(p) => ProxyLineVerdict::Parsed(p),
            // 归一化成功说明它确实是代理形态，此时的失败必须报出来
            // （典型：端口 0 / 65536 被归一化层的 is_port_seg 挡住前先过了 a_ok？
            //  不会 —— 但 host 字符集在此仍会兜住，报 BadHost 比静默跳过有用）。
            Err(e) => ProxyLineVerdict::Invalid(e),
        },
        NormalizedLine::Rejected(e) => ProxyLineVerdict::Invalid(e),
        NormalizedLine::AsIs => {
            let parsed = parse_proxy_link_strict(cleaned);
            let looks_like_link = starts_with_scheme(cleaned)
                || (cleaned.contains('@') && parsed.is_ok())
                // 冒号形态但端口/host 有一处不合法（`1.2.3.4:0:u:p` / `1.2.3.4:99999:u:p`）：
                // 第一段像 host 字面量就当代理数据看，报错而不是静默跳过。
                || (split_colon_segments(cleaned).len() >= 4
                    && looks_like_host_literal(split_colon_segments(cleaned)[0]));
            match parsed {
                Ok(p) if looks_like_link => ProxyLineVerdict::Parsed(p),
                // 裸 `host:port` 单条 API 接受、批量**维持拒绝**：放宽会让
                // `port : 40002` 这类英文说明行造出 host="port" 的假节点。
                Ok(_) => ProxyLineVerdict::Skipped,
                Err(e) if looks_like_link => ProxyLineVerdict::Invalid(e),
                Err(_) => ProxyLineVerdict::Skipped,
            }
        }
    }
}

/// 剥掉行首尾的引号/反引号（文档里的代码块残留）。
///
/// 抽成函数是因为 [`classify_proxy_line`] 与 [`mask_proxy_line`] **必须用同一套清洗**：
/// 两者分叉时会出现「这行被成功导入，但回显的原文里密码是明文」——
/// 脱敏方按未清洗的串解析 ⇒ 解析不出 ⇒ 没有「已识别的密码」可替换 ⇒ 整行原样回显。
/// （同一逻辑各写一份而漏改，与 update.rs 那次是同一类事故。）
fn strip_line_quotes(s: &str) -> &str {
    s.trim_matches(|c| c == '`' || c == '"' || c == '\'' || c == ' ')
}

/// 是否以 `scheme://` 开头（`[a-zA-Z][a-zA-Z0-9+.-]*://`）。
fn starts_with_scheme(s: &str) -> bool {
    match s.split_once("://") {
        Some((scheme, _)) => {
            !scheme.is_empty()
                && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '.' || c == '-')
        }
        None => false,
    }
}

/// 逐行报告：整段文本 → 每行一条 `(1 起的行号, 原文, 判定)`。
///
/// 与 [`parse_proxy_links_bulk`] 的区别是**不丢信息**：粘贴内重复、以及每条失败的原因
/// 都能带回界面。安静跳过的行（标题/分隔线/说明文字）**不进结果**，
/// 只计入返回的 `skipped` —— 一份节点商文档里那类行有几十条，全列出来会把真正
/// 要看的几行埋掉。
///
/// 粘贴内重复标为 `Invalid(NotProxy)`？不。重复不是「无效」，它单独由第二个返回值
/// （`dup_in_paste` 的行号集合）表达，界面按它默认不勾选。
pub fn parse_proxy_lines_report(text: &str) -> ProxyLinesReport {
    let mut items: Vec<ProxyLineReportItem> = Vec::new();
    let mut skipped = 0usize;
    let mut seen: Vec<String> = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let lineno = idx + 1;
        match classify_proxy_line(line) {
            ProxyLineVerdict::Skipped => skipped += 1,
            ProxyLineVerdict::Invalid(e) => items.push(ProxyLineReportItem {
                lineno,
                raw: mask_proxy_line(line),
                link: None,
                issue: Some(e),
                dup_in_paste: false,
            }),
            ProxyLineVerdict::Parsed(p) => {
                let dup = seen.iter().any(|u| u == &p.url);
                if !dup {
                    seen.push(p.url.clone());
                }
                items.push(ProxyLineReportItem {
                    lineno,
                    raw: mask_proxy_line(line),
                    link: Some(p),
                    issue: None,
                    dup_in_paste: dup,
                });
            }
        }
    }
    ProxyLinesReport { items, skipped }
}

/// [`parse_proxy_lines_report`] 的结果。
#[derive(Debug, Clone)]
pub struct ProxyLinesReport {
    /// 每条**值得报给用户**的行（成功 + 失败；安静跳过的不进来）。
    pub items: Vec<ProxyLineReportItem>,
    /// 安静跳过的行数（空行/注释/标题/说明文字）。
    pub skipped: usize,
}

/// 逐行报告的一条。
#[derive(Debug, Clone)]
pub struct ProxyLineReportItem {
    /// 原始行号（1 起，与用户粘的文本对齐）。
    pub lineno: usize,
    /// 原文（**密码已脱敏**）。失败行靠它让用户一眼看出是格式问题还是脏数据。
    pub raw: String,
    /// 解析成功时的结果。
    pub link: Option<ParsedProxyLink>,
    /// 失败原因。
    pub issue: Option<ProxyLineIssue>,
    /// 同一次粘贴内与更靠前的某行地址重复。
    pub dup_in_paste: bool,
}

/// 单行原文脱敏：把已识别出的密码与 base64 userinfo 替换成 `***`，并截断到 200 字符。
///
/// 为什么仍回显原文：失败行的**形状**就是诊断信息本身（用户要判断是自己格式写错了
/// 还是数据脏了）。而密码不能回显 —— 这份响应会进浏览器 devtools 与 access log。
/// 解析不出来的行没有「已识别的密码」可脱敏，只能整行回显：这类行按定义没进池子，
/// 且是管理员本人在同一会话里刚粘进来的文本，回显不扩大暴露面。
fn mask_proxy_line(line: &str) -> String {
    let t = line.trim();
    let mut out = t.to_string();
    // ⚠️ 解析用的串必须与 `classify_proxy_line` 同样先剥引号，否则
    // `"1.2.3.4:1080:u:s3cr3tpw"` 这类带引号的行会被**成功导入**却整行原样回显
    // （引号让解析失败 ⇒ 没有已识别的密码可替换 ⇒ 明文密码进 devtools 与 access log）。
    // 替换仍在原串 `out` 上做，故显示出来的形状不变，只有密码变 `***`。
    if let Some(p) = parse_proxy_link(strip_line_quotes(t)) {
        if let Some(pw) = p.password.as_deref().filter(|s| !s.is_empty()) {
            out = out.replace(pw, "***");
        }
    }
    // base64 userinfo：密码在编码里，上面的 replace 抓不到 ⇒ 整段 userinfo 打掉。
    if let Some((head, tail)) = out.rsplit_once('@') {
        let ui = head.rsplit_once("://").map(|(_, u)| u).unwrap_or(head);
        if !ui.is_empty() && !ui.contains(':') {
            let prefix = &head[..head.len() - ui.len()];
            out = format!("{prefix}***@{tail}");
        }
    }
    const MAX: usize = 200;
    if out.chars().count() > MAX {
        out = out.chars().take(MAX).collect::<String>() + "…";
    }
    out
}

/// 批量解析：整段粘贴的多行文本 → 节点列表。
///
/// 逐行解析，跳过空行、`#` 开头的注释行、以及解析不出 host:port 的说明文字
/// （节点商发的文档里混着大量 `端口: 40002` / `curl --socks5-hostname ...` 之类的行，
/// 一律安静跳过而不是报错 —— 用户就是整段复制过来的）。
///
/// 返回 `(解析成功的节点, 被跳过的行数)`。按 `url` 去重（同一节点在明细里会出现两次：
/// 一次在"整段复制导入"区、一次在"逐台明细"区）。
///
/// 实现委托给 [`parse_proxy_lines_report`]，两者**判据只有一份** —— 先前它自带一道
/// `contains("://") && contains('@')` 的闸门，那道闸门排在解析器之前，
/// 是纯冒号形态一条都进不来的真正原因。
///
/// 「被跳过的行数」= 安静跳过 + 报错行，与旧口径一致（旧实现两者都计入 `skipped`）。
pub fn parse_proxy_links_bulk(text: &str) -> (Vec<ParsedProxyLink>, usize) {
    let report = parse_proxy_lines_report(text);
    let invalid = report.items.iter().filter(|i| i.issue.is_some()).count();
    let nodes: Vec<ParsedProxyLink> = report
        .items
        .into_iter()
        .filter(|i| !i.dup_in_paste)
        .filter_map(|i| i.link)
        .collect();
    (nodes, report.skipped + invalid)
}

/// 构建 HTTP Client
///
/// # Arguments
/// * `proxy` - 可选的代理配置
/// * `timeout_secs` - 超时时间（秒）
///
/// # Returns
/// 配置好的 reqwest::Client
/// 构建**流式专用** HTTP Client（对话路径）。
///
/// 与 [`build_client`] 的关键区别：用 `read_timeout`（两次数据之间的**空闲间隔**上限）
/// 替代 `.timeout()`（整个请求生命周期的**总时长**上限）。
///
/// 根因（2026-07-11 定位 `Connection closed mid-response`）：reqwest 的 `.timeout()` 覆盖
/// 读取响应体全过程，对流式是致命的——一个健康但耗时长的大请求（opus 大 prompt / 64k
/// max_tokens，生成可超 12 分钟）会在**流中途被硬掐**，上游流没读完就断，我方转出的 SSE
/// 随之断裂，下游客户端报 `Connection closed mid-response` 并疯狂重试。
///
/// 改用 `read_timeout` 后：只要上游持续吐数据（token/ping），流就永不被超时掐断；只有真正
/// **卡死**（`idle_secs` 内一个字节都没来）才中断——这才是流式该有的语义。另设一个宽松的
/// `connect_timeout` 防连不上时无限等。
pub fn build_streaming_client(
    proxy: Option<&ProxyConfig>,
    idle_secs: u64,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    let mut builder = Client::builder()
        .read_timeout(Duration::from_secs(idle_secs))
        .connect_timeout(Duration::from_secs(30));
    builder = apply_tls_and_proxy(builder, proxy, tls_backend)?;
    Ok(builder.build()?)
}

pub fn build_client(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    let builder = Client::builder().timeout(Duration::from_secs(timeout_secs));
    let builder = apply_tls_and_proxy(builder, proxy, tls_backend)?;
    Ok(builder.build()?)
}

/// 与 [`build_streaming_client`] 相同，但**禁用重定向**（`redirect::Policy::none()`）。
/// 供 custom_api 透传出站用：写入时已校验 base_url 目标非内网（SSRF 主防线），但公网中转站
/// 若返回 `302 Location: http://169.254.169.254/...` 仍能把请求跳向内网/元数据——禁重定向
/// 堵死这条最典型的 SSRF 绕过链（纵深防护 C2）。
pub fn build_streaming_client_no_redirect(
    proxy: Option<&ProxyConfig>,
    idle_secs: u64,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    let mut builder = Client::builder()
        .read_timeout(Duration::from_secs(idle_secs))
        .connect_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none());
    builder = apply_tls_and_proxy(builder, proxy, tls_backend)?;
    Ok(builder.build()?)
}

/// 与 [`build_client`] 相同，但**禁用重定向**。供 custom_api deep_verify 出站用（同上，防
/// 302 跳内网的盲 SSRF）。
pub fn build_client_no_redirect(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    let builder = Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .redirect(reqwest::redirect::Policy::none());
    let builder = apply_tls_and_proxy(builder, proxy, tls_backend)?;
    Ok(builder.build()?)
}

/// 把 TLS 后端选择 + 可选代理（含账密）应用到 builder 上（[`build_client`] /
/// [`build_streaming_client`] 共用，避免两处逻辑漂移）。
fn apply_tls_and_proxy(
    mut builder: reqwest::ClientBuilder,
    proxy: Option<&ProxyConfig>,
    tls_backend: TlsBackend,
) -> anyhow::Result<reqwest::ClientBuilder> {
    match tls_backend {
        TlsBackend::Rustls => {
            builder = builder.use_rustls_tls();
        }
        TlsBackend::NativeTls => {
            #[cfg(feature = "native-tls")]
            {
                builder = builder.use_native_tls();
            }
            // 防呆：出厂发布版一律 --no-default-features（纯 rustls，见 build.bat / release.yml），
            // 不含 native-tls 后端。旧 config.json 里残留 tlsBackend="native-tls" 时，
            // **静默回退 rustls** 而非报错——否则整条上游调用（刷 token / 转发）全挂，
            // 网关直接废，得手改配置才能救回。rustls 内置 webpki + native-roots 双证书源，
            // 功能上完全等价，回退无副作用。（前端已移除 native-tls 选项，此分支仅兜底旧配置。）
            #[cfg(not(feature = "native-tls"))]
            {
                tracing::warn!(
                    "配置 tlsBackend=native-tls，但本构建未编译 native-tls 后端；已自动回退 rustls（功能等价）"
                );
                builder = builder.use_rustls_tls();
            }
        }
    }

    if let Some(proxy_config) = proxy {
        let mut proxy = Proxy::all(&proxy_config.url)?;
        if let (Some(username), Some(password)) = (&proxy_config.username, &proxy_config.password) {
            proxy = proxy.basic_auth(username, password);
        }
        builder = builder.proxy(proxy);
        tracing::debug!("HTTP Client 使用代理: {}", proxy_config.url);
    }

    Ok(builder)
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
        // 账密内嵌在 socks5 URL 里（虚构样例）：应拆出干净 URL + 独立账密。
        let (url, user, pass) =
            split_proxy_credentials("socks5://proxyuser:proxypass@127.0.0.1:1080");
        assert_eq!(url, "socks5://127.0.0.1:1080");
        assert_eq!(user, Some("proxyuser".to_string()));
        assert_eq!(pass, Some("proxypass".to_string()));
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

    /// 🔴 base64 userinfo 分享链接必须被正确解析（真实节点数据）。
    ///
    /// `split_proxy_credentials` 只做百分号解码 ⇒ 会把整个 base64 串当成**用户名**
    /// （里面没有 `:`），密码为 None ⇒ 代理认证必然失败，而那个失败长得像
    /// 「节点不通」，会把排查带偏。`#US-1-SOCKS5` 还会留在 URL 里污染 host。
    #[test]
    fn parse_proxy_link_decodes_base64_userinfo_and_fragment() {
        // 现场真实数据（2026-08-05 用户提供的 5 台美国机）
        let cases: [(&str, &str, &str, &str, &str); 5] = [
            (
                "socks://dXMxdTpwZUxBck9sWWNDSWZHUmxzcFEzZ1lkRHBkMGs5Zzd1aA@192.220.50.26:40002#US-1-SOCKS5",
                "socks5://192.220.50.26:40002",
                "us1u",
                "peLArOlYcCIfGRlspQ3gYdDpd0k9g7uh",
                "US-1-SOCKS5",
            ),
            (
                "socks://dXMydTpVc2o5a2NEcW9SQVdpekVPRTcxS2FYcGxlekJzQnlFeg@192.220.24.104:40002#US-2-SOCKS5",
                "socks5://192.220.24.104:40002",
                "us2u",
                "Usj9kcDqoRAWizEOE71KaXplezBsByEz",
                "US-2-SOCKS5",
            ),
            (
                "socks://dXMzdTo2c0JRWlV2MHdDbWw3eW01cGlvb21KWUdDMUJSWWFIRw@192.220.24.27:40002#US-3-SOCKS5",
                "socks5://192.220.24.27:40002",
                "us3u",
                "6sBQZUv0wCml7ym5pioomJYGC1BRYaHG",
                "US-3-SOCKS5",
            ),
            (
                "socks://dXM0dTp0RE90aDRKQlFUNkJhOW5GTXE3NVZQeXI4eTFkbElheA@192.220.24.251:40002#US-4-SOCKS5",
                "socks5://192.220.24.251:40002",
                "us4u",
                "tDOth4JBQT6Ba9nFMq75VPyr8y1dlIax",
                "US-4-SOCKS5",
            ),
            (
                "socks://bmJ1c3U6VFRGV3VmTFE5VEZiMHRkQWVhUFM3aUxrMmt1U0paMXc@38.244.34.15:40002#NBUS-LA-SOCKS5",
                "socks5://38.244.34.15:40002",
                "nbusu",
                "TTFWufLQ9TFb0tdAeaPS7iLk2kuSJZ1w",
                "NBUS-LA-SOCKS5",
            ),
        ];
        for (raw, url, user, pass, name) in cases {
            let p = parse_proxy_link(raw).unwrap_or_else(|| panic!("应能解析: {raw}"));
            assert_eq!(
                p.url, url,
                "scheme 必须归一到 socks5 且剥掉 userinfo/fragment"
            );
            assert_eq!(p.username.as_deref(), Some(user));
            assert_eq!(
                p.password.as_deref(),
                Some(pass),
                "密码必须从 base64 解出，否则代理认证失败"
            );
            assert_eq!(p.name.as_deref(), Some(name));
        }
    }

    /// ⭐ 承重判据：**明文 `user:pass` 绝不能被当 base64 解**。
    ///
    /// 明文密码完全可能恰好是合法 base64。若先试 base64，会把明文解成乱码，
    /// 而认证失败同样长得像「节点不通」。故判据是「含 `:` ⇒ 明文，不试 base64」。
    #[test]
    fn plaintext_userinfo_is_never_base64_decoded() {
        // "dXNlcg" 是合法 base64（解出 "user"），但这里它是**明文用户名**
        let p = parse_proxy_link("socks5://dXNlcg:dXNlcg@1.2.3.4:1080").unwrap();
        assert_eq!(
            p.username.as_deref(),
            Some("dXNlcg"),
            "含 : 即明文，不得 base64 解码"
        );
        assert_eq!(p.password.as_deref(), Some("dXNlcg"));

        // base64 解出来不含 ':' 时也不能采用（否则会把纯用户名解成乱码）
        let p2 = parse_proxy_link("socks5://dXNlcg@1.2.3.4:1080").unwrap();
        assert_eq!(
            p2.username.as_deref(),
            Some("dXNlcg"),
            "解出不含 : ⇒ 当纯用户名"
        );
        assert_eq!(p2.password, None);
    }

    /// fragment 必须**最先剥**：否则它污染 host（`40002#US-1` 不是合法端口）。
    #[test]
    fn fragment_is_stripped_before_host_parsing() {
        let p = parse_proxy_link("socks://dXMxdTpwYXNz@1.2.3.4:40002#Name-With-Dash").unwrap();
        assert_eq!(
            p.url, "socks5://1.2.3.4:40002",
            "URL 里绝不能残留 #fragment"
        );
        assert_eq!(p.name.as_deref(), Some("Name-With-Dash"));
        // 无 fragment 时 name 为 None
        assert_eq!(
            parse_proxy_link("socks5://1.2.3.4:1080").unwrap().name,
            None
        );
    }

    /// 无效行必须返回 None（不含 host:port 的说明文字）。
    #[test]
    fn invalid_lines_return_none() {
        for bad in [
            "",
            "   ",
            "# 注释",
            "端口  : 40002",
            "socks5://",
            "socks5://nohostcolon",
        ] {
            assert!(parse_proxy_link(bad).is_none(), "不该解析出节点: {bad:?}");
        }
    }

    /// 🔴 整段复制导入：用户直接粘贴节点商的完整文档。
    ///
    /// 那份文档里混着标题、分隔线、`端口: 40002`、`curl --socks5-hostname ...`
    /// 等大量说明行，且同一节点出现两次（"整段复制"区 + "逐台明细"区）。
    /// 必须安静跳过说明行并按 url 去重 —— 用户就是整段复制过来的。
    #[test]
    fn bulk_parse_handles_real_pasted_document() {
        let doc = "\
==================================================================\n\
  SOCKS5 节点  ·  4 台美国机  ·  Xray-core 26.3.27\n\
==================================================================\n\
\n\
端口 40002  ·  认证 用户名/密码  ·  UDP 已开启\n\
\n\
socks://dXMxdTpwZUxBck9sWWNDSWZHUmxzcFEzZ1lkRHBkMGs5Zzd1aA@192.220.50.26:40002#US-1-SOCKS5\n\
socks://dXMydTpVc2o5a2NEcW9SQVdpekVPRTcxS2FYcGxlekJzQnlFeg@192.220.24.104:40002#US-2-SOCKS5\n\
\n\
[US-1]  192.220.50.26\n\
  地址  : 192.220.50.26\n\
  端口  : 40002\n\
  用户名: us1u\n\
  密码  : peLArOlYcCIfGRlspQ3gYdDpd0k9g7uh\n\
\n\
  socks://dXMxdTpwZUxBck9sWWNDSWZHUmxzcFEzZ1lkRHBkMGs5Zzd1aA@192.220.50.26:40002#US-1-SOCKS5\n\
\n\
curl:\n\
  curl --socks5-hostname us1u:peLArOlYcCIfGRlspQ3gYdDpd0k9g7uh@192.220.50.26:40002 https://api.ipify.org\n\
";
        let (nodes, _skipped) = parse_proxy_links_bulk(doc);
        assert_eq!(
            nodes.len(),
            2,
            "应解析出 2 个去重后的节点（明细区那次重复必须被去掉），实得 {}: {:?}",
            nodes.len(),
            nodes.iter().map(|n| &n.url).collect::<Vec<_>>()
        );
        assert_eq!(nodes[0].url, "socks5://192.220.50.26:40002");
        assert_eq!(
            nodes[0].password.as_deref(),
            Some("peLArOlYcCIfGRlspQ3gYdDpd0k9g7uh")
        );
        assert_eq!(nodes[1].url, "socks5://192.220.24.104:40002");
        // curl 示例行含 "@" 但无 "://" ⇒ 必须被跳过，不能造出一个假节点
        assert!(
            !nodes.iter().any(|n| n.url.contains("ipify")),
            "curl 示例行不该被解析成节点"
        );
    }

    // ===== 冒号形态（2026-08-05 新增）=====

    /// 用户实际粘贴的那 10 行（`host:port:user:pass`）必须 **10/10** 识别。
    ///
    /// 🔴 这条走的是**真实调用链** `parse_proxy_links_bulk`，不是纯函数：
    /// 只改 `parse_proxy_link` 而不动批量层那道 `contains("://") && contains('@')`
    /// 闸门时，纯函数测试会全绿而这条必挂 —— 那道闸门排在解析器**之前**。
    ///
    /// 前 3 行是现场真实数据；后 7 行同形态补齐到 10 行（用户报的是「10 行 0 条成功」）。
    #[test]
    fn colon_form_end_to_end_ten_real_lines() {
        let paste = "\
130.180.228.34:6318:dwgxdwgx:dwgxdwgx\n\
9.142.211.219:5384:dwgxdwgx:dwgxdwgx\n\
45.56.183.65:8387:dwgxdwgx:dwgxdwgx\n\
104.207.42.11:9021:dwgxdwgx:dwgxdwgx\n\
23.129.64.78:7204:dwgxdwgx:dwgxdwgx\n\
185.199.110.153:6001:dwgxdwgx:dwgxdwgx\n\
198.51.100.24:41003:dwgxdwgx:dwgxdwgx\n\
203.0.113.99:1080:dwgxdwgx:dwgxdwgx\n\
192.0.2.44:8899:dwgxdwgx:dwgxdwgx\n\
172.104.55.7:30001:dwgxdwgx:dwgxdwgx\n";
        let (nodes, skipped) = parse_proxy_links_bulk(paste);
        assert_eq!(
            nodes.len(),
            10,
            "10 行应 10/10 识别，实得 {}（skipped={}）: {:?}",
            nodes.len(),
            skipped,
            nodes.iter().map(|n| &n.url).collect::<Vec<_>>()
        );
        assert_eq!(skipped, 0, "不该有任何行被跳过");
        assert_eq!(nodes[0].url, "socks5://130.180.228.34:6318");
        assert_eq!(nodes[0].username.as_deref(), Some("dwgxdwgx"));
        assert_eq!(nodes[0].password.as_deref(), Some("dwgxdwgx"));
        assert_eq!(nodes[9].url, "socks5://172.104.55.7:30001");
        // 逐行报告口径应一致
        let rep = parse_proxy_lines_report(paste);
        assert_eq!(rep.items.len(), 10);
        assert!(
            rep.items
                .iter()
                .all(|i| i.link.is_some() && i.issue.is_none())
        );
        assert!(rep.items.iter().all(|i| !i.dup_in_paste));
        assert_eq!(rep.items[0].lineno, 1);
        assert_eq!(rep.items[9].lineno, 10);
    }

    /// 三种新增形态各一条正向。
    #[test]
    fn colon_form_three_shapes_parse() {
        // ① host:port:user:pass
        let a = parse_proxy_link("130.180.228.34:6318:us1u:pw1").expect("host:port:user:pass");
        assert_eq!(a.url, "socks5://130.180.228.34:6318");
        assert_eq!(a.username.as_deref(), Some("us1u"));
        assert_eq!(a.password.as_deref(), Some("pw1"));

        // ② user:pass:host:port（第 4 段是端口，第 3 段像 host）
        let b = parse_proxy_link("us2u:pw2:130.180.228.34:6318").expect("user:pass:host:port");
        assert_eq!(b.url, "socks5://130.180.228.34:6318");
        assert_eq!(b.username.as_deref(), Some("us2u"));
        assert_eq!(b.password.as_deref(), Some("pw2"));

        // ③ host:port@user:pass（倒装 @）
        let c = parse_proxy_link("130.180.228.34:6318@us3u:pw3").expect("host:port@user:pass");
        assert_eq!(c.url, "socks5://130.180.228.34:6318");
        assert_eq!(c.username.as_deref(), Some("us3u"));
        assert_eq!(c.password.as_deref(), Some("pw3"));

        // 带 scheme 与 #name 也要通（归一化必须先剥这两者）
        let d = parse_proxy_link("socks://1.2.3.4:1080:us4u:pw4#JP-1").expect("scheme+fragment");
        assert_eq!(d.url, "socks5://1.2.3.4:1080");
        assert_eq!(d.username.as_deref(), Some("us4u"));
        assert_eq!(d.password.as_deref(), Some("pw4"));
        assert_eq!(d.name.as_deref(), Some("JP-1"));
    }

    /// 🔴 消歧：两种读法各一条 + 真正判不定的必须**被拒**而不是猜。
    #[test]
    fn colon_form_disambiguation_never_guesses() {
        // 判据 1：只有第 2 段是端口 ⇒ 读法 A
        let a = parse_proxy_link("130.180.228.34:6318:dwgxdwgx:dwgxdwgx").unwrap();
        assert_eq!(a.url, "socks5://130.180.228.34:6318");
        // 判据 1 反向：只有第 4 段是端口 ⇒ 读法 B
        let b = parse_proxy_link("dwgxdwgx:dwgxdwgx:130.180.228.34:6318").unwrap();
        assert_eq!(b.url, "socks5://130.180.228.34:6318");
        assert_eq!(b.username.as_deref(), Some("dwgxdwgx"));

        // 判据 2：两段都是端口，但只有第 1 段像 host 字面量 ⇒ 读法 A
        let c = parse_proxy_link("10.0.0.1:1080:12345:8080").unwrap();
        assert_eq!(
            c.url, "socks5://10.0.0.1:1080",
            "12345 不像 host ⇒ 必须读 A"
        );
        assert_eq!(c.password.as_deref(), Some("8080"));

        // 🔴 判据 3：两读法都成立 ⇒ 拒绝（绝不猜）
        assert_eq!(
            normalize_proxy_line("1.2.3.4:8080:5.6.7.8:9090"),
            NormalizedLine::Rejected(ProxyLineIssue::AmbiguousColonForm)
        );
        assert!(
            parse_proxy_link("1.2.3.4:8080:5.6.7.8:9090").is_none(),
            "判不定必须拒绝"
        );
        assert_eq!(
            classify_proxy_line("1.2.3.4:8080:5.6.7.8:9090"),
            ProxyLineVerdict::Invalid(ProxyLineIssue::AmbiguousColonForm),
            "判不定要报给用户，不能安静跳过"
        );
    }

    /// 边界：端口 0 / 65536 / 非数字 / host 含非法字符。
    #[test]
    fn colon_form_boundaries_rejected() {
        // 端口 0 与 65536：两段都不是合法端口 ⇒ 归一化不接，但第 1 段像 host ⇒ 报错而非静默跳过
        for bad in ["1.2.3.4:0:u:p", "1.2.3.4:65536:u:p", "1.2.3.4:99999:u:p"] {
            assert!(parse_proxy_link(bad).is_none(), "端口越界不该解析: {bad}");
            assert!(
                matches!(classify_proxy_line(bad), ProxyLineVerdict::Invalid(_)),
                "端口越界应报错而非跳过: {bad}"
            );
        }
        // 端口 65535 是上界内 ⇒ 必须通过
        assert_eq!(
            parse_proxy_link("1.2.3.4:65535:u:p").unwrap().url,
            "socks5://1.2.3.4:65535"
        );
        // 端口非数字（两段都不是端口）
        assert!(parse_proxy_link("1.2.3.4:abcd:u:p").is_none());
        // host 含非法字符 / 不像 host 字面量（无点）⇒ 不接
        assert!(parse_proxy_link("有中文:1080:u:p").is_none());
        assert!(
            parse_proxy_link("nodothost:1080:u:p").is_none(),
            "单标签主机名不走冒号形态"
        );
        // 裸 IPv6 无解，必须拒
        assert!(parse_proxy_link("2001:db8::1:1080:u:p").is_none());
        // 加方括号则可用
        let v6 = parse_proxy_link("[2001:db8::1]:1080:u:p").expect("[IPv6]:port:user:pass");
        assert_eq!(v6.url, "socks5://[2001:db8::1]:1080");
        assert_eq!(v6.password.as_deref(), Some("p"));
    }

    /// 密码含 `:` 归密码（`splitn(4)`），且 5 段以上只允许读法 A。
    #[test]
    fn colon_form_password_may_contain_colon() {
        let p = parse_proxy_link("1.2.3.4:1080:user:pa:ss").expect("5 段应按读法 A");
        assert_eq!(p.url, "socks5://1.2.3.4:1080");
        assert_eq!(p.username.as_deref(), Some("user"));
        assert_eq!(
            p.password.as_deref(),
            Some("pa:ss"),
            "第 3 个 : 之后全部归密码"
        );
    }

    /// 🔴 已支持格式的回归：归一化层必须对它们**完全无感**。
    #[test]
    fn existing_formats_unchanged_by_normalization() {
        // base64 userinfo（现场真实数据）
        let b = parse_proxy_link(
            "socks://dXMxdTpwZUxBck9sWWNDSWZHUmxzcFEzZ1lkRHBkMGs5Zzd1aA@192.220.50.26:40002#US-1-SOCKS5",
        )
        .unwrap();
        assert_eq!(b.url, "socks5://192.220.50.26:40002");
        assert_eq!(b.username.as_deref(), Some("us1u"));
        assert_eq!(
            b.password.as_deref(),
            Some("peLArOlYcCIfGRlspQ3gYdDpd0k9g7uh")
        );
        assert_eq!(b.name.as_deref(), Some("US-1-SOCKS5"));

        // 🔴 密码含 ':' 的规范形态：有 4 段，若归一化先数冒号就会被重写坏
        let c = parse_proxy_link("socks5://user:p:ss@host:1080").unwrap();
        assert_eq!(c.url, "socks5://host:1080");
        assert_eq!(c.username.as_deref(), Some("user"));
        assert_eq!(
            c.password.as_deref(),
            Some("p:ss"),
            "含 @ 时绝不能走冒号形态"
        );
        assert_eq!(
            normalize_proxy_line("socks5://user:p:ss@host:1080"),
            NormalizedLine::AsIs
        );

        // 密码含 '@'（按最后一个 @ 切）
        let d = parse_proxy_link("socks5://user:p@ss@host:1080").unwrap();
        assert_eq!(d.password.as_deref(), Some("p@ss"));

        // 两侧都是合法 host:port 时保持标准读法（不误当倒装 @）
        let e = parse_proxy_link("1.2.3.4:1080@5.6.7.8:8080").unwrap();
        assert_eq!(e.url, "socks5://5.6.7.8:8080", "两侧都合法 ⇒ 标准读法不变");

        // 无 scheme / 仅用户名 / 百分号编码
        assert_eq!(parse_proxy_link("host:port").map(|p| p.url), None);
        assert_eq!(
            parse_proxy_link("socks5://1.2.3.4:1080").unwrap().url,
            "socks5://1.2.3.4:1080"
        );
        assert_eq!(
            parse_proxy_link("http://onlyuser@host:3128")
                .unwrap()
                .username
                .as_deref(),
            Some("onlyuser")
        );
        let f = parse_proxy_link("http://us%40er:p%3Ass@host:3128").unwrap();
        assert_eq!(f.username.as_deref(), Some("us@er"));
        assert_eq!(f.password.as_deref(), Some("p:ss"));

        // 说明行/注释/空串必须仍被拒
        for bad in [
            "",
            "   ",
            "# 注释",
            "端口  : 40002",
            "socks5://",
            "socks5://nohostcolon",
        ] {
            assert!(parse_proxy_link(bad).is_none(), "不该解析出节点: {bad:?}");
        }
    }

    /// 🔴 `端口  : 40002` 这类说明行在**批量层**必须仍被安静跳过（不报错、不造假节点）。
    #[test]
    fn doc_noise_lines_still_skipped_in_bulk() {
        let noise = "\
==================================================================\n\
  SOCKS5 节点  ·  4 台美国机\n\
端口 40002  ·  认证 用户名/密码\n\
  地址  : 192.220.50.26\n\
  端口  : 40002\n\
  用户名: us1u\n\
  密码  : peLArOlYcCIfGRlspQ3gYdDpd0k9g7uh\n\
port : 40002\n\
curl:\n\
";
        let rep = parse_proxy_lines_report(noise);
        assert!(
            rep.items.is_empty(),
            "说明行不该进结果（会把真正要看的行埋掉），实得 {:?}",
            rep.items
                .iter()
                .map(|i| (&i.raw, i.issue))
                .collect::<Vec<_>>()
        );
        let (nodes, _) = parse_proxy_links_bulk(noise);
        assert!(nodes.is_empty(), "说明行不该造出节点");
    }

    /// 粘贴内重复只标记不丢弃，且 `parse_proxy_links_bulk` 仍去重。
    #[test]
    fn dup_within_paste_is_flagged_not_dropped() {
        let paste = "1.2.3.4:1080:u:p\n1.2.3.4:1080:u2:p2\n5.6.7.8:1080:u:p\n";
        let rep = parse_proxy_lines_report(paste);
        assert_eq!(
            rep.items.len(),
            3,
            "重复行仍要出现在报告里（用户要看到它被跳过）"
        );
        assert!(!rep.items[0].dup_in_paste);
        assert!(rep.items[1].dup_in_paste, "同地址第二次出现应标为重复");
        assert!(!rep.items[2].dup_in_paste);
        let (nodes, _) = parse_proxy_links_bulk(paste);
        assert_eq!(nodes.len(), 2, "落库侧必须去重");
    }

    /// 原文回显必须**脱敏密码**（这份响应会进 devtools 与 access log）。
    #[test]
    fn report_raw_masks_password() {
        let rep = parse_proxy_lines_report("1.2.3.4:1080:someuser:s3cr3tpw\n");
        assert_eq!(rep.items.len(), 1);
        let raw = &rep.items[0].raw;
        assert!(!raw.contains("s3cr3tpw"), "密码不得回显: {raw}");
        assert!(raw.contains("someuser"), "用户名保留（诊断需要）: {raw}");
        assert!(raw.contains("1.2.3.4:1080"), "地址保留: {raw}");

        // 🔴 带引号/反引号的行（文档代码块残留）：`classify_proxy_line` 会剥引号后
        // 成功导入，所以脱敏也必须剥引号后再解析 —— 否则这行会被导入却明文回显密码。
        for quoted in [
            "\"1.2.3.4:1080:someuser:s3cr3tpw\"",
            "`1.2.3.4:1080:someuser:s3cr3tpw`",
            "'1.2.3.4:1080:someuser:s3cr3tpw'",
        ] {
            let r = parse_proxy_lines_report(&format!("{quoted}\n"));
            assert_eq!(r.items.len(), 1, "带引号的行应被导入: {quoted}");
            assert!(r.items[0].link.is_some(), "带引号的行应解析成功: {quoted}");
            assert!(
                !r.items[0].raw.contains("s3cr3tpw"),
                "带引号的行密码也不得回显: {quoted} → {}",
                r.items[0].raw
            );
        }

        // base64 userinfo：密码在编码里 ⇒ 整段 userinfo 打掉
        let rep2 = parse_proxy_lines_report(
            "socks://dXMxdTpwZUxBck9sWWNDSWZHUmxzcFEzZ1lkRHBkMGs5Zzd1aA@192.220.50.26:40002#US-1\n",
        );
        let raw2 = &rep2.items[0].raw;
        assert!(
            !raw2.contains("dXMxdTpwZUxB"),
            "base64 userinfo 必须打掉: {raw2}"
        );
        assert!(raw2.contains("192.220.50.26:40002"));
    }

    /// 老缺口：`user:pass@host:port`（无 scheme）单条早就支持，批量却跳过。
    #[test]
    fn schemeless_userinfo_form_now_accepted_in_bulk() {
        let (nodes, skipped) = parse_proxy_links_bulk("us1u:pw1@1.2.3.4:1080\n");
        assert_eq!(nodes.len(), 1, "无 scheme 的 userinfo 形态应能批量导入");
        assert_eq!(skipped, 0);
        assert_eq!(nodes[0].url, "socks5://1.2.3.4:1080");
        assert_eq!(nodes[0].password.as_deref(), Some("pw1"));
    }
}
