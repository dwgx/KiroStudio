//! 上游账号身份（User ID）解析 —— **纯观测，零行为改动**
//!
//! ## 为什么需要它
//!
//! 上游每次 403/429 都在错误体里告诉我们**账号身份**，而网关每次都把它扔掉：
//! 全仓 `Your User ID` 的 10 处命中在本文件出现之前**全是注释与测试字面量**
//! （`endpoint/mod.rs` / `region_probe.rs` / `handlers.rs`），零生产解析。
//!
//! 这条信息是回答下面这个问题的唯一现成来源：
//!
//! > 网关按**凭据**记账（`RpmTracker` 的 `hits: HashMap<credential_id, _>`），
//! > 而上游按**账号**记账。若推号方从一个账号批量签发 N 把 key，
//! > 那么「15 个凭据 × 各自 85 RPM」在上游只是「1 个 User ID 吃 15 份负载」。
//!
//! ✅ **2026-08-07：该假说已被线上数据证实**（不再是假说）。证据不是本模块的映射表，
//! 而是 403 body 直接给出的 `Your User ID (NNN)`：实测一个 User ID 对应 **6 个**
//! cred id（UID 079998937591 → cred 1294..1299）。同期一个 4h 窗口里客户端 429 的
//! **95.5%**（2080/2177）是"池被整批自动禁用清零"，真上游 `ThrottlingException` 仅 28 条。
//!
//! 与原假说的差别：线上那 17 份不是"一个账号签发的 N 把 key"，而是**同一把 key 的
//! N 份多开分身**（同 `cloneGroup`，实测 keyhash 全同）。两者的记账错配是同一个，
//! 而分身这一支更硬 —— 同一把 key 在定义上就是同一个账号，无需映射表即可判定。
//!
//! ⇒ 已据此把 `family_key` 对同 `cloneGroup` 的 api_key 分身收敛为 `clone:{group}`，
//! 并把 `consecutive_suspicious` 计数/清零改为族级（见
//! `token_manager::report_suspicious_activity` 的实测依据一节）。
//! **本模块仍有价值**：它覆盖"同账号签发多把**不同** key"这一支 —— 那种情况
//! `cloneGroup` 与 keyhash 都不同，只有上游 User ID 能揭示，需要本模块的映射表。
//! 已知局限：**429 body 不带 User ID**，只有 403 带 ⇒ 映射表只能等号被风控才建得出。
//! 所以解析器**在拿不准时必须返回 `None`，绝不猜** —— 猜出来的身份会让判据自我实现。
//!
//! ## 为什么不用 `regex`
//!
//! 本仓无该依赖，CLAUDE.md 明确「不引入新库」。判据是固定形状
//! （锚点 + `(数字)`），纯字节扫描足够，且没有灾难性回溯的风险面。
//!
//! ## 另一个身份来源（已侦察，本模块刻意不碰）
//!
//! `web_portal.rs` 有两处 `user_id`：`CsrfSession.user_id`（HTML `<meta name="user-id">`）
//! 与 `UsageUserInfo.user_id`（`getUserUsageAndLimits` 响应字段）。两者都是**网络调用**
//! 才能拿到的权威值，而本模块的契约是「纯函数、无网络、无 IO」⇒ 不在此复用。
//! 二者的取值空间也可能不同（portal 侧测试里是 `u-42` 形态，错误体里是 12 位数字），
//! 混进同一个字段会让映射表出现两种命名空间的 key。见报告里的「第二来源」一节。

/// User ID 数字位数上限。
///
/// 超过即判定「这不是账号 id」并返回 `None`。线上见过的都是 12 位
/// （`898055051935` / `186648603162` / `450334904897`），测试里有 1 位（`(1)`）。
/// 设 24 是给上游换编号方案留余量，同时挡住「一长串数字恰好被括号包住」的误命中。
const MAX_USER_ID_DIGITS: usize = 24;

/// 锚点与 `(` 之间允许的最大间隔（字节）。
///
/// **这个窗口是承重的，不是随手取的常数。** 线上真实形态里锚点与括号之间只有
/// `" "`（`Your User ID (898055051935)`）。而无 ID 的变体
/// （`Your User ID is temporarily suspended.`）后面接的是一整句申诉文案，
/// 里面**可能**出现别的括号（例如联系方式、URL 括注）。没有窗口约束时，
/// 那个无关括号里的数字会被当成账号身份记进 trace ⇒ 映射表被污染 ⇒
/// 「几个账号」这个唯一判据失真。回归测试 `should_not_capture_far_away_parenthesis`
/// 钉住这条。
const ANCHOR_TO_PAREN_WINDOW: usize = 8;

/// 解析结果的三态分类。
///
/// 只有 `Option<String>` 时，「上游没发 suspend 文案」与「发了但没带 ID」
/// 长得一样（都是 `None`）—— 而这两件事对判据的含义相反：前者说明这批 429
/// 不是账号级风控，后者说明是风控但拿不到身份。离线分析必须能分开数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserIdSignal {
    /// 命中锚点且拿到完整数字 ID
    Parsed(String),
    /// 命中锚点但没有可信 ID（无括号 / 括号内被截断如 `(1866...)` / 非数字）
    AnchorWithoutId,
    /// body 里根本没有 User ID 锚点（例如普通 429 `USER_REQUEST_RATE_EXCEEDED`）
    Absent,
}

/// 从上游错误体解析账号 User ID。**纯函数，无网络、无 IO。**
///
/// 返回 `None` 的三种情况都是刻意的（见 [`UserIdSignal`]）：拿不准就不猜。
pub fn parse_upstream_user_id(body: &str) -> Option<String> {
    match classify_upstream_user_id(body) {
        UserIdSignal::Parsed(id) => Some(id),
        UserIdSignal::AnchorWithoutId | UserIdSignal::Absent => None,
    }
}

/// [`parse_upstream_user_id`] 的三态版本，供离线分析区分「无锚点」与「有锚点无 ID」。
///
/// 扫描策略：找到每一个 `user id` 锚点（大小写不敏感、允许中间是空白/`-`/`_`/无分隔），
/// 在其后 [`ANCHOR_TO_PAREN_WINDOW`] 字节内找 `(`，括号内必须是纯数字。
/// 第一个满足全部条件的即返回；有锚点但无一满足 ⇒ [`UserIdSignal::AnchorWithoutId`]。
pub fn classify_upstream_user_id(body: &str) -> UserIdSignal {
    let bytes = body.as_bytes();
    let mut anchor_seen = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let Some(after_anchor) = match_anchor(bytes, i) else {
            i += 1;
            continue;
        };
        anchor_seen = true;
        if let Some(id) = extract_parenthesized_digits(bytes, after_anchor) {
            return UserIdSignal::Parsed(id);
        }
        // 锚点在但形状不对：从锚点之后继续扫，别在第一个就放弃
        // （线上见过同一条 body 里两处锚点，前一处不带 ID）。
        i = after_anchor;
    }
    if anchor_seen {
        UserIdSignal::AnchorWithoutId
    } else {
        UserIdSignal::Absent
    }
}

/// 在 `bytes[at..]` 处尝试匹配 `user` + 可选分隔 + `id` 锚点。
///
/// 命中返回锚点**结束后**的字节下标。全 ASCII 比较 ⇒ 不会切断多字节字符
/// （UTF-8 续字节均 >= 0x80，永远不会等于任何 ASCII 字面量）。
fn match_anchor(bytes: &[u8], at: usize) -> Option<usize> {
    let mut i = eat_ascii_ci(bytes, at, b"user")?;
    // 分隔符：空白 / `-` / `_` 任意个（含 0 个，覆盖 `userid`）
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r' | b'-' | b'_') {
        i += 1;
    }
    let i = eat_ascii_ci(bytes, i, b"id")?;
    Some(i)
}

/// 大小写不敏感地吃掉一个 ASCII 字面量，成功则返回其后的下标。
fn eat_ascii_ci(bytes: &[u8], at: usize, lit: &[u8]) -> Option<usize> {
    let end = at.checked_add(lit.len())?;
    if end > bytes.len() {
        return None;
    }
    for (k, want) in lit.iter().enumerate() {
        if !bytes[at + k].eq_ignore_ascii_case(want) {
            return None;
        }
    }
    Some(end)
}

/// 从 `from` 起在窗口内找 `(数字)`，成功返回数字串。
///
/// 严格要求闭括号：`(1866...)` 这种被省略号截断的形态在遇到 `.` 时即失败 ⇒ 返回 None。
/// 这正是「拿不准不猜」那条契约的落点 —— 截断前缀相同的两个账号会被并成一个。
fn extract_parenthesized_digits(bytes: &[u8], from: usize) -> Option<String> {
    let window_end = from.saturating_add(ANCHOR_TO_PAREN_WINDOW).min(bytes.len());
    let mut i = from;
    // 窗口内找 `(`；只允许跨过空白与 ASCII 单词字符（`is` / `：` 之类的 ASCII 部分），
    // 遇到 `(` 即停。窗口本身是硬上界。
    while i < window_end && bytes[i] != b'(' {
        i += 1;
    }
    if i >= window_end || bytes[i] != b'(' {
        return None;
    }
    let mut j = i + 1;
    // 括号内允许前导空白（上游改书写时留余量）
    while j < bytes.len() && matches!(bytes[j], b' ' | b'\t') {
        j += 1;
    }
    let digits_start = j;
    while j < bytes.len() && bytes[j].is_ascii_digit() {
        j += 1;
    }
    let digits_len = j - digits_start;
    if digits_len == 0 || digits_len > MAX_USER_ID_DIGITS {
        return None;
    }
    // 允许尾随空白，然后必须是 `)`
    let mut k = j;
    while k < bytes.len() && matches!(bytes[k], b' ' | b'\t') {
        k += 1;
    }
    if k >= bytes.len() || bytes[k] != b')' {
        return None;
    }
    // 全 ASCII 数字，切片必然落在字符边界上
    std::str::from_utf8(&bytes[digits_start..j])
        .ok()
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 线上实测原文（逐字，取自 `endpoint/mod.rs` / `handlers.rs` 的记录）。
    const REAL_SUSPEND_898: &str = r#"流式 API 请求失败: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"Your User ID (898055051935) temporarily is suspended. We've locked your account as a security precaution. To restore access, please contact our support team to verify your identity: https://aws.amazon.com/contact-us/"}"#;

    #[test]
    fn should_parse_real_suspend_body() {
        assert_eq!(
            parse_upstream_user_id(REAL_SUSPEND_898),
            Some("898055051935".to_string())
        );
        assert_eq!(
            classify_upstream_user_id(REAL_SUSPEND_898),
            UserIdSignal::Parsed("898055051935".to_string())
        );
    }

    /// 全部已知带 ID 的变体（含测试里出现的 1 位短 ID）。
    #[test]
    fn should_parse_all_known_id_bearing_variants() {
        for (body, want) in [
            (
                r#"{"message":"Your User ID (186648603162) temporarily is suspended."}"#,
                "186648603162",
            ),
            (
                r#"{"message":"Your User ID (450334904897) temporarily is suspended."}"#,
                "450334904897",
            ),
            ("Your User ID (1) temporarily is suspended.", "1"),
            (
                "403 Forbidden {\"message\":\"Your User ID (898055051935) temporarily is suspended.\"}",
                "898055051935",
            ),
        ] {
            assert_eq!(
                parse_upstream_user_id(body),
                Some(want.to_string()),
                "带 ID 的变体必须解析出来: {body}"
            );
        }
    }

    /// ⭐ 无 ID 的变体必须返回 None，**不能猜**。
    ///
    /// 这三条是任务里点名的形态。`(1866...)` 是被省略号截断的形态：
    /// 前缀 `1866` 是真的，但它不是完整账号号码 —— 把它当身份记进映射表会让
    /// 两个不同账号（同前缀）被并成一个，正好朝「假说成立」的方向造假。
    #[test]
    fn should_return_none_when_id_absent_or_truncated() {
        for body in [
            "Your User ID is temporarily suspended.",
            "Your User ID temporarily is suspended.",
            r#"{"message":"Your User ID (1866...) temporarily is suspended. We've locked your account as a security precaution."}"#,
            r#"{"message":"Your User ID (abc) temporarily is suspended."}"#,
            r#"{"message":"Your User ID (12a) temporarily is suspended."}"#,
            r#"{"message":"Your User ID () temporarily is suspended."}"#,
        ] {
            assert_eq!(
                parse_upstream_user_id(body),
                None,
                "拿不准必须返回 None 而不是猜: {body}"
            );
            assert_eq!(
                classify_upstream_user_id(body),
                UserIdSignal::AnchorWithoutId,
                "锚点在但 ID 不可信，应可与「无锚点」区分: {body}"
            );
        }
    }

    /// 负例一：普通 429（账号级限流的 reason 码），body 里**没有**身份。
    ///
    /// 这条同时是本步最重要的**局限**证据：`USER_REQUEST_RATE_EXCEEDED` 不带 User ID
    /// ⇒ 映射表只能靠 403 suspend 那类 body 建立，429 本身建不出来。
    #[test]
    fn should_return_absent_for_plain_rate_limited_body() {
        let body = r#"429 Too Many Requests {"__type":"com.amazon.kiro.runtimeservice#ThrottlingException","message":"Too many requests, please wait before trying again.","reason":"USER_REQUEST_RATE_EXCEEDED"}"#;
        assert_eq!(parse_upstream_user_id(body), None);
        assert_eq!(classify_upstream_user_id(body), UserIdSignal::Absent);
    }

    /// 负例二：空串。
    #[test]
    fn should_return_absent_for_empty_body() {
        assert_eq!(parse_upstream_user_id(""), None);
        assert_eq!(classify_upstream_user_id(""), UserIdSignal::Absent);
    }

    /// 大小写与分隔符变体（上游改书写方式不该让解析静默归零）。
    #[test]
    fn should_be_case_and_separator_insensitive() {
        for body in [
            "your user id (123456789012) temporarily is suspended.",
            "YOUR USER ID (123456789012) TEMPORARILY IS SUSPENDED.",
            r#"{"user-id":"x","message":"User-ID (123456789012) suspended"}"#,
            "user_id (123456789012) suspended",
            "userid (123456789012) suspended",
            "Your  User \tID (123456789012) suspended",
        ] {
            assert_eq!(
                parse_upstream_user_id(body),
                Some("123456789012".to_string()),
                "书写变体必须命中: {body}"
            );
        }
    }

    /// ⭐ 窗口守卫：锚点后面很远处的括号**绝不能**被当成身份。
    ///
    /// 回退即 FAIL：把 [`ANCHOR_TO_PAREN_WINDOW`] 放大到覆盖整句，这条括号里的
    /// 电话号会被记成账号 id ⇒ 映射表污染 ⇒ 「几个账号」这个唯一判据失真。
    #[test]
    fn should_not_capture_far_away_parenthesis() {
        let body = "Your User ID is temporarily suspended. We've locked your account as a security precaution. Contact support (18664860316) to verify.";
        assert_eq!(parse_upstream_user_id(body), None);
        assert_eq!(
            classify_upstream_user_id(body),
            UserIdSignal::AnchorWithoutId
        );
    }

    /// 两个锚点、第一个没带 ID：必须继续扫到第二个而不是在第一个就放弃。
    #[test]
    fn should_keep_scanning_after_anchor_without_id() {
        let body = "Your User ID is temporarily suspended. Your User ID (450334904897) temporarily is suspended.";
        assert_eq!(
            parse_upstream_user_id(body),
            Some("450334904897".to_string())
        );
    }

    /// 位数上限：一长串数字恰好被括号包住时不当成账号 id。
    #[test]
    fn should_reject_absurdly_long_digit_run() {
        let long = "1".repeat(MAX_USER_ID_DIGITS + 1);
        let body = format!("Your User ID ({long}) temporarily is suspended.");
        assert_eq!(parse_upstream_user_id(&body), None);
        // 恰好等于上限仍接受（边界）
        let ok = "9".repeat(MAX_USER_ID_DIGITS);
        let body_ok = format!("Your User ID ({ok}) temporarily is suspended.");
        assert_eq!(parse_upstream_user_id(&body_ok), Some(ok));
    }

    /// UTF-8 安全：真实 body 前面挂着中文错误文案（`流式 API 请求失败: ...`），
    /// 多字节字符在锚点之前。按字节索引扫描时切错边界会 panic。
    #[test]
    fn should_be_utf8_safe_with_cjk_prefix() {
        let body = "非流式 API 请求失败（第 3 次尝试，已换号）: 403 Forbidden {\"message\":\"Your User ID (898055051935) temporarily is suspended.\"}";
        assert_eq!(
            parse_upstream_user_id(body),
            Some("898055051935".to_string())
        );
    }

    /// ⭐ 承重：解析必须在**已脱敏且已截断**的 body 上仍然成立。
    ///
    /// trace 落盘的 body 走的是 `sanitize_body`（先截断到 `BODY_MAX_BYTES`、再 `redact`）。
    /// 若 User ID 被那条管道吃掉，整条观测链在真实数据上就是空的 —— 而单测用裸 body
    /// 会全绿，正是本仓记录过的「测了分支内部、没测真实输入」那种伪证。
    #[test]
    fn should_survive_sanitize_body_pipeline() {
        let sanitized = crate::kiro::upstream_trace::sanitize_body(REAL_SUSPEND_898);
        assert_eq!(
            parse_upstream_user_id(&sanitized),
            Some("898055051935".to_string()),
            "脱敏/截断后仍须解析得到身份，否则线上 trace 里这个字段恒为空"
        );
        // 同时确认脱敏没把数字打码（User ID 不是密钥，可落盘）
        assert!(sanitized.contains("898055051935"));
    }
}
