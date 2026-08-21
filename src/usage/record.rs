//! 请求记录数据契约
//!
//! 一次 API 请求生命周期的最终结算快照。字段设计参考 cc-switch 的 `RequestLog`
//! （farion1231/cc-switch），裁剪为 Kiro 单上游场景所需。

use serde::{Deserialize, Serialize};

/// 请求结果分类
///
/// 对齐 provider 的失败处置分类（见 [`crate::kiro::cooldown::CooldownReason`]），
/// 便于统计侧按结果聚合健康度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestOutcome {
    /// 成功
    Success,
    /// 上游限流（429）
    RateLimited,
    /// 认证失败（401/403）
    AuthFailed,
    /// 额度用尽（402 MONTHLY_REQUEST_COUNT）
    QuotaExhausted,
    /// 账户被暂停/封禁
    AccountSuspended,
    /// 上游服务器错误（5xx）
    ServerError,
    /// 请求错误（400 等客户端错误）
    BadRequest,
    /// 网络/连接错误（未拿到响应）
    NetworkError,
    /// 其它/未分类失败
    OtherError,
    /// 模型容量暂时不可用（503 MODEL_TEMPORARILY_UNAVAILABLE）
    ///
    /// 全局容量问题，非凭据问题。不影响凭据健康分，独立于 ServerError 便于可观测。
    ModelUnavailable,
    /// 上游空/近空响应：客户端收到 error（400/429 SSE），面板计失败。
    ///
    /// 不是凭据故障——`is_success` 为 false（计入失败率），但不得走
    /// `report_failure` / 熔断 / absorb。completion 层保持 Ok，仅 usage 记账改写。
    EmptyResponse,
    /// 客户端在流完成前断开（CC Esc / hyper 丢 Body）。
    ///
    /// 上游可能已消耗 token/credit；Drop 兜底补记。同样不计入凭据健康。
    Interrupted,
}

impl RequestOutcome {
    /// 封闭枚举全部变体（统计有界测试 / SQLite roundtrip 共用，避免漏列）。
    pub const ALL: [RequestOutcome; 12] = [
        RequestOutcome::Success,
        RequestOutcome::RateLimited,
        RequestOutcome::AuthFailed,
        RequestOutcome::QuotaExhausted,
        RequestOutcome::AccountSuspended,
        RequestOutcome::ServerError,
        RequestOutcome::BadRequest,
        RequestOutcome::NetworkError,
        RequestOutcome::OtherError,
        RequestOutcome::ModelUnavailable,
        RequestOutcome::EmptyResponse,
        RequestOutcome::Interrupted,
    ];

    /// 是否为成功结果
    pub fn is_success(&self) -> bool {
        matches!(self, RequestOutcome::Success)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            RequestOutcome::Success => "success",
            RequestOutcome::RateLimited => "rate_limited",
            RequestOutcome::AuthFailed => "auth_failed",
            RequestOutcome::QuotaExhausted => "quota_exhausted",
            RequestOutcome::AccountSuspended => "account_suspended",
            RequestOutcome::ServerError => "server_error",
            RequestOutcome::BadRequest => "bad_request",
            RequestOutcome::NetworkError => "network_error",
            RequestOutcome::OtherError => "other_error",
            RequestOutcome::ModelUnavailable => "model_unavailable",
            RequestOutcome::EmptyResponse => "empty_response",
            RequestOutcome::Interrupted => "interrupted",
        }
    }
}

/// 单次请求的最终结算记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    /// 请求唯一 ID（用于关联 per-attempt 明细）
    pub request_id: String,
    /// 记录生成的 Unix 毫秒时间戳
    pub ts_ms: i64,
    /// 实际服务该请求的凭据 ID（失败到无凭据可用时为 None）
    pub credential_id: Option<u64>,
    /// **本条请求链最先尝试的凭据 ID**（failover 首选号；`None` = 无 failover 或首选号不可考）。
    ///
    /// 与 [`Self::credential_id`]（最终服务号）成对构成「换号链」：两者不同 = 首选号
    /// 失败后换号成功。面板据此发现「死号恒选」——`first_attempted_credential_id` 恒为
    /// 某号而 `credential_id` 恒为另一号时，说明该号每次都排最前却被换掉（上游持续
    /// 502/429），需要运维处理其上游。透传 failover 链在 `provider.rs` 的
    /// `try_custom_api_passthrough` 记录首选号，经透传元数据（成功链）与跨层共享预算
    /// （Kiro 主路径失败记录）分别落到两类 usage 埋点。
    ///
    /// serde default，兼容早于本字段的历史 JSONL（缺字段视为 None）。
    #[serde(default)]
    pub first_attempted_credential_id: Option<u64>,
    /// 请求模型名
    pub model: String,
    /// **客户端请求的原始模型名**（映射前；`None` = 无映射或不可得时回落 `model`）。
    ///
    /// 与 [`Self::upstream_model`] 组成「用量统计双口径」：面板可按「客户端看的是哪个模型」
    /// 或「上游实际服务的是哪个模型」分别 group by。全局模型映射（`config.model_mapping`）
    /// 生效时两者不同；未命中映射或失败记录（选号前）时 `None` → 聚合层回落 `model`，
    /// 保证两维度的请求总数相等。
    ///
    /// serde default，兼容早于本字段的历史 JSONL（缺字段视为 None）。
    #[serde(default)]
    pub requested_model: Option<String>,
    /// **实际发给上游的模型名**（映射后；`None` = 未映射，等于 `model`）。
    ///
    /// 仅当全局映射命中且改写时非 None；不映射/豁免/失败记录均为 None。
    /// 与 [`Self::requested_model`] 同一批埋点写入，见 `provider.rs` 的 `mapped_model` 快照。
    #[serde(default)]
    pub upstream_model: Option<String>,
    /// 是否流式
    pub is_streaming: bool,
    /// 输入 tokens —— **gross 口径**（含 `cache_read_tokens` + `cache_creation_tokens`）。
    ///
    /// ⚠ 与响应体里同名的 `usage.input_tokens` **不是一回事**，二者口径相反：
    /// - 本字段（用量统计 / `/api/admin/usage/*`）＝本次请求的**全量**输入 token
    ///   （优先 `contextUsageEvent` 反推的精确值，回退本地估算），缓存命中部分**没有**被剔除。
    /// - Anthropic 响应体里的 `usage.input_tokens`（见
    ///   [`crate::anthropic::stream::billed_input_tokens`]）是 **billed 口径**，
    ///   已减去 cache 读写，与 `cache_read_input_tokens` 互斥。
    ///
    /// 因此消费方要算「总输入」时**直接用本字段即可，不可再加 `cache_read_tokens`**，
    /// 否则缓存部分被计两次。要还原客户端看到的口径请用
    /// [`RequestRecord::billed_input_tokens`]。
    ///
    /// 保持 gross 是有意为之：用量统计关心的是真实上下文规模（缓存只影响计费不影响体积），
    /// 且历史 JSONL/SQLite 数据已按 gross 落库，改口径会让历史数据断裂。
    pub input_tokens: i32,
    /// 输出 tokens
    pub output_tokens: i32,
    /// 本次命中缓存读取的 tokens（cache_read_input_tokens；无缓存记账时为 0）。
    ///
    /// 是 [`Self::input_tokens`] 的**子集**（gross 已包含它），不是额外增量。
    /// serde default，兼容早于本字段的历史 JSONL（缺字段视为 0）。
    #[serde(default)]
    pub cache_read_tokens: i32,
    /// 本次新建缓存写入的 tokens（cache_creation_input_tokens；无缓存记账时为 0）。
    ///
    /// 同样是 [`Self::input_tokens`] 的**子集**。
    /// serde default，兼容早于本字段的历史 JSONL（缺字段视为 0）。
    #[serde(default)]
    pub cache_creation_tokens: i32,
    /// 上游返回的真实 credit 消耗量（无 meteringEvent 时为 None）
    pub credits_used: Option<f64>,
    /// 端到端延迟（毫秒）
    pub latency_ms: u64,
    /// 首字节/首事件延迟（毫秒，流式有意义）
    pub first_token_ms: Option<u64>,
    /// SSE 流中途断流（传输错误/解码器停止/in-band 错误）时已从上游收到的字节数。
    ///
    /// `None` = 本次未中断（正常收尾）；`Some(n)` = 断流时点已收字节（0 表示
    /// 一个字节都没收到就断了）。埋点见 `StreamContext::note_received_bytes` /
    /// `interrupted_bytes`。serde default，兼容早于本字段的历史 JSONL（缺字段视为 None）。
    #[serde(default)]
    pub interrupted_bytes: Option<u64>,
    /// 结果分类
    pub outcome: RequestOutcome,
    /// 本次经历的重试次数（0 表示首次即成功）
    pub retries: u32,
    /// 错误信息（成功时为空）
    pub error_message: Option<String>,
    /// 会话标识（conversationId，用于亲和分析）
    pub session_id: Option<String>,
    /// 请求来源设备（由入站 User-Agent 分类得到，见 [`classify_device`]）
    pub client_device: Option<String>,
    /// 客户端 IP（优先 x-forwarded-for 首段，回退 x-real-ip；拿不到为 None）
    pub client_ip: Option<String>,
    /// 客户端操作系统细分（由 UA 解析，见 [`parse_client_os`]，识别不出为 None）
    pub client_os: Option<String>,
    /// 客户端浏览器 + 版本（由 UA 解析，见 [`parse_client_browser`]，非浏览器为 None）
    pub client_browser: Option<String>,
}

impl RequestRecord {
    /// 构造一条记录，时间戳取当前时刻
    pub fn new(request_id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            ts_ms: chrono::Utc::now().timestamp_millis(),
            credential_id: None,
            first_attempted_credential_id: None,
            model: model.into(),
            requested_model: None,
            upstream_model: None,
            is_streaming: false,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            credits_used: None,
            latency_ms: 0,
            first_token_ms: None,
            interrupted_bytes: None,
            outcome: RequestOutcome::Success,
            retries: 0,
            error_message: None,
            session_id: None,
            client_device: None,
            client_ip: None,
            client_os: None,
            client_browser: None,
        }
    }

    /// 派生只读值：把 gross 的 [`Self::input_tokens`] 换算成 Anthropic 响应体的
    /// billed 口径（剔除 cache 读写）。**不参与序列化**，只为消费方省去自己减一遍。
    ///
    /// 与 [`crate::anthropic::stream::billed_input_tokens`] 同一算法，饱和减不为负。
    pub fn billed_input_tokens(&self) -> i32 {
        self.input_tokens
            .saturating_sub(self.cache_creation_tokens)
            .saturating_sub(self.cache_read_tokens)
            .max(0)
    }

    /// 把 cache 明细收敛到不超过 gross [`Self::input_tokens`]，维持「cache 是 input 子集」不变量。
    ///
    /// 为什么需要：两个数字**不同源**。`cache_read` 由本地前缀估算得出并按**本地**
    /// `count_all_tokens` 估算值 clamp（handlers.rs 的 `prefix_tokens.min(input_tokens)`），
    /// 而落库的 `input_tokens` 优先取 `contextUsageEvent` 百分比反推的值。上游百分比偏低
    /// （或 window_size 判定与实际不符）时反推值可能小于前缀估算 → 产出
    /// `cache_read > input_tokens` 的自相矛盾记录，面板会显示「缓存读取比总输入还多」。
    /// 这里做一次防御性收敛：creation 先占额度，read 取剩余。
    ///
    /// 在每个埋点写入 cache 字段后调用（`input_tokens` 也已确定）。
    pub fn clamp_cache_to_input(&mut self) {
        let gross = self.input_tokens.max(0);
        let creation = self.cache_creation_tokens.clamp(0, gross);
        let read = self.cache_read_tokens.clamp(0, gross - creation);
        if creation != self.cache_creation_tokens || read != self.cache_read_tokens {
            tracing::debug!(
                request_id = %self.request_id,
                input_tokens = self.input_tokens,
                cache_read_before = self.cache_read_tokens,
                cache_creation_before = self.cache_creation_tokens,
                cache_read_after = read,
                cache_creation_after = creation,
                "cache 明细超过 gross input_tokens（估算与上游反推不同源），已收敛"
            );
            self.cache_creation_tokens = creation;
            self.cache_read_tokens = read;
        }
    }
}

/// UA 里是否存在**词边界**意义上的 `token`（左右都不是字母或数字）。
///
/// 为什么必须有这个函数：UA 分类全是子串匹配，而短 token 的子串误伤是本文件踩过
/// 两次的坑 —— `contains("ios")` 把 `axios/1.6.0` 判成 iOS；改成 `contains("ios/")`
/// **仍然**命中（`axios/` 里含 `ios/`），因为陷阱在**前**边界。加后缀是治不了的，
/// 只能真判边界。
///
/// 传入的 `hay` 必须已经小写化（调用方都先做了 `to_lowercase`），`token` 传小写字面量。
fn token_present(hay: &str, token: &str) -> bool {
    let tb = token.as_bytes();
    let hb = hay.as_bytes();
    if tb.is_empty() || hb.len() < tb.len() {
        return false;
    }
    let is_word = |b: u8| b.is_ascii_alphanumeric();
    let mut from = 0usize;
    while let Some(rel) = hay[from..].find(token) {
        let start = from + rel;
        let end = start + tb.len();
        let left_ok = start == 0 || !is_word(hb[start - 1]);
        let right_ok = end == hb.len() || !is_word(hb[end]);
        if left_ok && right_ok {
            return true;
        }
        // 继续找下一处：步进 1 字节即可（token 内部也可能重叠命中）。
        // `hay` 是 UTF-8，但我们只在 ASCII 边界上前进 —— `find` 返回的 start 一定是
        // 字符边界，+1 后若落在多字节字符中间，下一轮 `find` 依然按字节匹配 ASCII
        // token，不会 panic（切片用的是 from.. 且 from 由 find 结果推得）。
        // 为绝对安全，用 char_indices 找到 start 之后的下一个字符边界。
        from = hay[start..]
            .char_indices()
            .nth(1)
            .map(|(off, _)| start + off)
            .unwrap_or(hay.len());
        if from >= hay.len() {
            break;
        }
    }
    false
}

/// 从入站 User-Agent 分类请求来源设备。
///
/// 返回规范小写取值：`claude-code` / `curl` / `windows` / `macos` / `linux` /
/// `python` / `node` / `vscode` / `browser` / `unknown`。`ua` 为 `None` 或空白
/// 时返回 `Some("unknown")`（永远给出一个可展示的值，不返回裸 `None`）。
///
/// 判定按契约优先级从上到下短路匹配：客户端标识（claude-code / curl / python /
/// node / vscode）优先于操作系统标识（Windows / macOS / Linux），最后才是通用
/// 浏览器 UA（Mozilla）兜底。
///
/// Claude Code CLI 实测入站 UA 形如 `claude-cli/2.1.201 (external, cli)`（旧版本
/// 也可能带 `claude-code`），二者统一归为 `claude-code` 类展示；`anthropic` 关键字
/// 作为官方 SDK/客户端的兜底也归入同类，避免全部落到 `unknown`。
pub fn classify_device(ua: Option<&str>) -> Option<String> {
    let raw = match ua {
        Some(s) if !s.trim().is_empty() => s,
        _ => return Some("unknown".to_string()),
    };
    let lower = raw.to_lowercase();

    // ══════════════════════════════════════════════════════════════════════════
    // 🔴 这里修的是一整类「设备乱识别」缺陷。根因是**把两个维度塞进了一个字段**：
    // 原实现把「客户端是什么」（claude-code/curl/vscode）与「操作系统是什么」
    // （windows/macos/linux）放在同一条 if-else 链上短路匹配，于是二者互相覆盖。
    //
    // 实测误判（改前逐条复现）：
    // - `Cursor/0.42.3 Chrome/124 Electron/30 Safari/537.36` → **unknown**
    //   （Cursor 是本网关最主要的客户端之一，却完全识别不出）
    // - `Mozilla/5.0 (Macintosh; ...) ... Cursor/0.42` → **macos**
    //   （OS 关键字把客户端身份覆盖掉了 —— 同一个 Cursor，装 Mozilla 前缀就变"macOS 设备"）
    // - `Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)` → **macos**
    //   （本函数没有移动端分支，而 iOS 的 UA 里含 `Mac OS X`。旁边的
    //    `parse_client_os` 正确处理了这点 ⇒ 两个函数对同一 UA 给出不同 OS，口径分叉）
    // - `Mozilla/5.0 (Linux; Android 14) ...` → **linux**（同类）
    // - 桌面 Chrome → **windows**/**macos** 而不是 browser（浏览器身份被 OS 吃掉）
    // - 完全无法识别：Kiro / Cline / Roo-Cline / Continue / Zed / okhttp / Go-http-client
    //
    // 修法：**客户端维度穷尽在前，OS 只作为最后兜底**，且 OS 兜底前先判移动端与浏览器。
    // OS 细分本就有专门的 `parse_client_os` 字段承载（前端分列展示），本字段只该回答
    // 「谁在调用」；把 OS 留在这里仅为兼容既有取值（历史数据里存了 windows/macos/linux，
    // 前端 DeviceBadge 也仍有这几个图标），不新增 OS 语义。
    //
    // 匹配顺序内的**优先级也是承重的**：更具体的标识必须先判。
    // 例如 Cursor/Kiro/Windsurf 都是 VSCode 分支，UA 里可能同时含 `vscode`；
    // Cline 跑在 VSCode 里、UA 含 `vscode` 的同时含 `cline`。先判具体品牌，
    // 否则一律塌陷成 vscode，面板上就看不出真实客户端构成。
    // ══════════════════════════════════════════════════════════════════════════

    // 用「带分隔符的词边界」判定，避免子串误伤。
    // 反例（改前真实存在）：`parse_client_os` 用 `contains("ios")` 判 iOS，
    // 于是 **`axios/1.6.0` 被判成 iOS** —— axios 是 Node 生态最常用的 HTTP 库，
    // 属高频 UA，一大批 Node 客户端的 OS 因此记错。本函数不重犯：凡是短 token
    // （≤4 字符）一律走 `token_present`。
    let has = |needle: &str| lower.contains(needle);
    // 短 token（`zed` / `go` / `java` 之类）必须走词边界，否则会被
    // `authorized` / `mongo` / `javascript` 这类常见子串误伤。
    let tok = |needle: &str| token_present(&lower, needle);

    let kind = if has("claude-cli") || has("claude-code") {
        // Claude Code CLI 实测 UA：`claude-cli/2.1.201 (external, cli)`；旧版带 claude-code。
        "claude-code"
    } else if has("cursor") {
        // Cursor（VSCode 分支，Electron）。必须在 vscode 之前判 —— 它的 UA 常同时含两者。
        "cursor"
    } else if has("windsurf") || has("codeium") {
        "windsurf"
    } else if has("cline") {
        // Cline / Roo-Cline（VSCode 插件）：`roo-cline` 也含 `cline`，一并归类。
        "cline"
    } else if has("continue") {
        "continue"
    } else if tok("zed") {
        // `zed` 是极常见的英文词尾（authorized / optimized / customized），
        // 必须按词边界判定，不能用子串。
        "zed"
    } else if has("kiro") {
        // Kiro IDE 自身（本网关的上游同名，但这里是**下游客户端**方向）。
        "kiro"
    } else if has("opencode") {
        // OpenCode（开源 TUI 编码助手）：前端展示为 OpenCode 品牌图标。
        "opencode"
    } else if has("aider") {
        "aider"
    } else if has("vscode") || has("visual studio code") {
        // 通用 VSCode（未命中上述任何具体分支/插件时）。
        "vscode"
    } else if has("jetbrains") || has("intellij") || has("pycharm") || has("goland") {
        "jetbrains"
    } else if has("anthropic") {
        // 官方 SDK/客户端兜底。刻意排在具体客户端**之后**：Claude Code 之外的第三方
        // 工具也可能在 UA 里带 `anthropic-sdk`，那种情况下工具身份比 SDK 身份更有信息量。
        "claude-code"
    } else if has("curl") {
        "curl"
    } else if has("wget") {
        "curl"
    } else if has("postman") || has("insomnia") {
        "postman"
    } else if has("python-requests") || has("python-httpx") || has("aiohttp") || has("python/") {
        // ⚠️ 改前是 `contains("python")` 裸子串 —— 任何含 python 的 UA（如
        // `Mozilla/... PythonPlugin`）都会误判。现在只认真实客户端的规范形式。
        "python"
    } else if has("okhttp") || tok("java") || has("apache-httpclient") {
        // `java` 走词边界：否则 `javascript` 会被判成 Java 客户端。
        "java"
    } else if has("go-http-client") || has("golang") {
        "go"
    } else if has("axios") || has("node-fetch") || has("undici") || has("node.js") || has("node/") {
        // ⚠️ 改前是 `contains("node")`，会误伤任何含 node 的 UA。
        "node"
    } else if has("okio") || has("dart") || has("flutter") {
        "dart"
    }
    // ── 以下才是「不是已知客户端」时的兜底 ──
    // 移动端优先：iOS 的 UA 含 `Mac OS X`、Android 的含 `Linux`，
    // 必须在桌面 OS 判定之前短路（这正是改前 iPhone→macos / Android→linux 的根因）。
    else if has("iphone") || has("ipad") || has("ipod") || has("android") {
        "mobile"
    }
    // 浏览器优先于桌面 OS：一个桌面 Chrome 的身份是「浏览器」，不是「Windows 设备」。
    // 改前顺序相反，导致所有浏览器流量被打散进 windows/macos/linux 三桶。
    else if has("mozilla") || has("applewebkit") || has("chrome") || has("safari") {
        "browser"
    }
    // 最后才是裸 OS 标识（非浏览器、非已知客户端，但 UA 里带平台信息）。
    // 保留这三个取值仅为兼容历史数据与前端既有图标，不鼓励新流量落这里。
    else if has("windows nt") || has("windows") {
        "windows"
    } else if has("macintosh") || has("mac os") {
        "macos"
    } else if has("linux") || has("x11") {
        "linux"
    } else {
        "unknown"
    };

    Some(kind.to_string())
}

/// 从入站 User-Agent 细分客户端操作系统。
///
/// 命中返回规范展示名：`Windows`（"Windows NT 10.0" 既可能是 10 也可能是 11，
/// 不硬判版本）/ `macOS` / `iOS` / `Android` / `Linux`。识别不出或 `ua` 为
/// `None`/空白时返回 `None`（与 `classify_device` 不同，这里不做 unknown 兜底，
/// 让「解析不出」如实为空）。
///
/// 判定顺序：移动端（iOS/Android）优先于桌面端，避免 iPad 的 "Mac OS X" 误判为
/// macOS、Android 的 "Linux" 误判为 Linux。
pub fn parse_client_os(ua: Option<&str>) -> Option<String> {
    let raw = match ua {
        Some(s) if !s.trim().is_empty() => s,
        _ => return None,
    };
    let lower = raw.to_lowercase();

    // 1) 移动端优先：iPad 的 UA 含 "Mac OS X"，Android 的 UA 含 "Linux"，
    //    必须在桌面端判定之前短路，否则会被误分类。
    //
    // 🔴 修的缺陷：原判据里有一条裸 `contains("ios")`，而 `ios` 是极常见子串 ——
    // **`axios/1.6.0` 因此被判成 iOS**。axios 是 Node 生态最常用的 HTTP 库，属高频
    // 入站 UA，所以这不是边角情况：一大批 Node 客户端的 OS 长期记成 iOS，
    // 按 OS 分组的面板视图和「按机器」聚合（`derive_machine_key` 吃 client_device）
    // 都会跟着错。同类误伤还有 `Kiosk/`、`BiosClient/`、任何含 ios 的产品名。
    //
    // 现在只认真实 iOS UA：设备名（iphone/ipad/ipod），或**词边界**意义上的 `ios`。
    //
    // ⚠️ 修这个 bug 时我第一版写的是 `contains("ios/")` —— 那**照样命中 `axios/1.6.0`**
    // （`axios/` 里就含 `ios/`）。加后缀不解决问题，因为陷阱在**前**边界：必须确认
    // `ios` 左侧不是字母/数字。所以这里用 `token_present` 做真正的词边界判定，
    // 而不是再叠一层子串花样。
    if lower.contains("iphone")
        || lower.contains("ipad")
        || lower.contains("ipod")
        || token_present(&lower, "ios")
    {
        return Some("iOS".to_string());
    }
    if lower.contains("android") {
        return Some("Android".to_string());
    }

    // 2) 桌面端
    if lower.contains("windows nt") || lower.contains("windows") {
        // "Windows NT 10.0" 对应 Win10/Win11 二者，无法从 UA 精确区分，统一记为 Windows
        Some("Windows".to_string())
    } else if lower.contains("mac os") || lower.contains("macintosh") {
        Some("macOS".to_string())
    } else if lower.contains("linux") || lower.contains("x11") {
        Some("Linux".to_string())
    } else {
        None
    }
}

/// 从入站 User-Agent 解析浏览器 + 主版本号。
///
/// 命中返回 `Chrome 120` / `Edge 120` / `Firefox` / `Safari` 形式（Chrome/Edge
/// 带主版本号，Firefox/Safari 仅名称）。curl/python/node 等非浏览器客户端、
/// 识别不出或空 UA 返回 `None`。
///
/// 判定顺序处理浏览器 UA 互相夹带的问题：Edge 的 UA 同时含 "Edg/" 与
/// "Chrome/"，必须先判 Edge；Chrome 的 UA 含 "Safari/"，故 Safari 需排除 Chrome。
pub fn parse_client_browser(ua: Option<&str>) -> Option<String> {
    let raw = match ua {
        Some(s) if !s.trim().is_empty() => s,
        _ => return None,
    };
    let lower = raw.to_lowercase();

    // Edge（Chromium 版标识为 "Edg/"）：其 UA 同时含 Chrome，必须最先判
    if let Some(v) = extract_version_after(&lower, "edg/") {
        return Some(format!("Edge {v}"));
    }
    // Chrome（且非 Edge，上面已短路）
    if lower.contains("chrome/") {
        return match extract_version_after(&lower, "chrome/") {
            Some(v) => Some(format!("Chrome {v}")),
            None => Some("Chrome".to_string()),
        };
    }
    // Firefox
    if lower.contains("firefox/") {
        return Some("Firefox".to_string());
    }
    // Safari（Chrome 的 UA 也含 "safari/"，故须排除 chrome）
    if lower.contains("safari/") && !lower.contains("chrome") {
        return Some("Safari".to_string());
    }
    None
}

/// 从 `haystack` 中定位 `token` 后紧跟的主版本号（第一个点前的数字段）。
///
/// 例如 `extract_version_after("chrome/120.0.6099", "chrome/")` → `Some("120")`。
/// `token` 后无数字则返回 `None`。`haystack` 需为已 lowercase 的串。
fn extract_version_after(haystack: &str, token: &str) -> Option<String> {
    let idx = haystack.find(token)?;
    let rest = &haystack[idx + token.len()..];
    // 主版本号：取到第一个非数字字符为止（"120.0.x" → "120"）
    let major: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if major.is_empty() { None } else { Some(major) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_outcome_success() {
        assert!(RequestOutcome::Success.is_success());
        assert!(!RequestOutcome::RateLimited.is_success());
        assert!(
            !RequestOutcome::EmptyResponse.is_success(),
            "空响应必须计入面板失败率"
        );
        assert!(
            !RequestOutcome::Interrupted.is_success(),
            "客户端断连必须计入面板失败率"
        );
    }

    /// 封闭枚举：as_str / serde snake_case / ALL 三者一一对应；漏列新变体会红。
    #[test]
    fn all_outcomes_serde_snake_case_and_all_list() {
        assert_eq!(RequestOutcome::ALL.len(), 12);
        for o in RequestOutcome::ALL {
            match o {
                RequestOutcome::Success => assert!(o.is_success()),
                RequestOutcome::RateLimited
                | RequestOutcome::AuthFailed
                | RequestOutcome::QuotaExhausted
                | RequestOutcome::AccountSuspended
                | RequestOutcome::ServerError
                | RequestOutcome::BadRequest
                | RequestOutcome::NetworkError
                | RequestOutcome::OtherError
                | RequestOutcome::ModelUnavailable
                | RequestOutcome::EmptyResponse
                | RequestOutcome::Interrupted => assert!(!o.is_success()),
            }
            let json = serde_json::to_string(&o).unwrap();
            assert_eq!(json, format!("\"{}\"", o.as_str()));
            let back: RequestOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(back, o);
        }
        assert_eq!(
            serde_json::to_string(&RequestOutcome::EmptyResponse).unwrap(),
            "\"empty_response\""
        );
        assert_eq!(
            serde_json::to_string(&RequestOutcome::Interrupted).unwrap(),
            "\"interrupted\""
        );
    }

    #[test]
    fn test_record_roundtrip_json() {
        let mut rec = RequestRecord::new("req-1", "claude-sonnet-4");
        rec.credential_id = Some(3);
        rec.input_tokens = 100;
        rec.output_tokens = 50;
        rec.credits_used = Some(1.5);
        rec.latency_ms = 1234;
        rec.outcome = RequestOutcome::Success;

        let json = serde_json::to_string(&rec).unwrap();
        let back: RequestRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.request_id, "req-1");
        assert_eq!(back.credential_id, Some(3));
        assert_eq!(back.credits_used, Some(1.5));
        assert_eq!(back.outcome, RequestOutcome::Success);
    }

    /// failover 首选号：序列化 roundtrip 保留，旧 JSONL（缺字段）反序列化为 None。
    #[test]
    fn test_first_attempted_credential_roundtrip_and_legacy() {
        let mut rec = RequestRecord::new("req-failover", "claude-sonnet-4");
        rec.credential_id = Some(2);
        rec.first_attempted_credential_id = Some(3);
        let json = serde_json::to_string(&rec).unwrap();
        let back: RequestRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.credential_id, Some(2), "最终服务号必须保留");
        assert_eq!(back.first_attempted_credential_id, Some(3), "首选号必须保留");

        let legacy = r#"{"request_id":"req-old","ts_ms":1,"credential_id":null,"model":"claude-opus-4-8","is_streaming":false,"input_tokens":1,"output_tokens":1,"credits_used":null,"latency_ms":1,"first_token_ms":null,"outcome":"success","retries":0,"error_message":null,"session_id":null}"#;
        let old: RequestRecord = serde_json::from_str(legacy).unwrap();
        assert_eq!(
            old.first_attempted_credential_id, None,
            "历史 JSONL 缺字段必须回落 None，不炸反序列化"
        );
    }

    /// 映射双口径：序列化 roundtrip 保留两个字段，且旧 JSONL（缺字段）反序列化不炸。
    #[test]
    fn test_request_record_mapping_dimensions_roundtrip_and_legacy() {
        // 新记录：双口径都有值。
        let mut rec = RequestRecord::new("req-map", "claude-sonnet-4-5");
        rec.requested_model = Some("claude-haiku-4-5".to_string());
        rec.upstream_model = Some("claude-sonnet-4-5".to_string());
        let json = serde_json::to_string(&rec).unwrap();
        let back: RequestRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.requested_model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(back.upstream_model.as_deref(), Some("claude-sonnet-4-5"));

        // 旧 JSONL（历史数据）：缺 requested_model / upstream_model 字段 → serde default
        // 补 None，反序列化成功，回落逻辑（聚合层用 r.model）保持正确。
        let legacy = r#"{"request_id":"req-old","ts_ms":1,"credential_id":null,"model":"claude-opus-4-8","is_streaming":false,"input_tokens":1,"output_tokens":1,"credits_used":null,"latency_ms":1,"first_token_ms":null,"outcome":"success","retries":0,"error_message":null,"session_id":null}"#;
        let old: RequestRecord = serde_json::from_str(legacy).unwrap();
        assert_eq!(old.requested_model, None);
        assert_eq!(old.upstream_model, None);
    }

    #[test]
    fn should_expose_billed_input_tokens_derived_from_gross() {
        // record.input_tokens 是 gross（含 cache），派生方法还原客户端看到的 billed 口径
        let mut rec = RequestRecord::new("req-gross", "claude-sonnet-5");
        rec.input_tokens = 12_500;
        rec.cache_read_tokens = 12_000;
        rec.cache_creation_tokens = 300;
        assert_eq!(rec.billed_input_tokens(), 200);
        // 派生值不得进入序列化（否则前端会多出一个口径字段）。
        // 注意 request_id 不含 "billed"，避免自身数据污染这条子串断言。
        let json = serde_json::to_string(&rec).unwrap();
        assert!(!json.contains("billed"), "派生值不应被序列化: {json}");
    }

    #[test]
    fn should_return_zero_billed_when_cache_covers_whole_input() {
        let mut rec = RequestRecord::new("req-full-hit", "claude-sonnet-5");
        rec.input_tokens = 1_000;
        rec.cache_read_tokens = 1_000;
        assert_eq!(
            rec.billed_input_tokens(),
            0,
            "全命中时 billed 为 0，不得为负"
        );
    }

    #[test]
    fn should_clamp_cache_read_exceeding_gross_input() {
        // 触发场景：cache_read 按本地估算 clamp（=9000），而 input_tokens 取
        // contextUsageEvent 反推值（=4000，上游百分比偏低）→ 矛盾记录
        let mut rec = RequestRecord::new("req-clamp", "claude-sonnet-5");
        rec.input_tokens = 4_000;
        rec.cache_read_tokens = 9_000;
        rec.clamp_cache_to_input();
        assert_eq!(
            rec.cache_read_tokens, 4_000,
            "cache_read 不得超过 gross input"
        );
        assert_eq!(rec.billed_input_tokens(), 0);
    }

    #[test]
    fn should_clamp_creation_first_then_read_with_remaining_budget() {
        let mut rec = RequestRecord::new("req-clamp2", "claude-sonnet-5");
        rec.input_tokens = 1_000;
        rec.cache_creation_tokens = 700;
        rec.cache_read_tokens = 900;
        rec.clamp_cache_to_input();
        // creation 先占 700，read 只剩 300 的额度
        assert_eq!(rec.cache_creation_tokens, 700);
        assert_eq!(rec.cache_read_tokens, 300);
        assert_eq!(
            rec.cache_creation_tokens + rec.cache_read_tokens,
            rec.input_tokens,
            "cache 合计不得超过 gross input"
        );
    }

    #[test]
    fn should_leave_consistent_cache_untouched_on_clamp() {
        let mut rec = RequestRecord::new("req-noop", "claude-sonnet-5");
        rec.input_tokens = 12_500;
        rec.cache_read_tokens = 12_000;
        rec.cache_creation_tokens = 300;
        rec.clamp_cache_to_input();
        assert_eq!(rec.cache_read_tokens, 12_000, "正常记录不应被改动");
        assert_eq!(rec.cache_creation_tokens, 300);
    }

    #[test]
    fn should_zero_cache_when_gross_input_is_zero_or_negative() {
        // 失败记录可能 input_tokens=0（拿不到反推值也没估算）；cache 必须跟着归零
        let mut rec = RequestRecord::new("req-zero", "claude-sonnet-5");
        rec.input_tokens = 0;
        rec.cache_read_tokens = 500;
        rec.cache_creation_tokens = 50;
        rec.clamp_cache_to_input();
        assert_eq!(rec.cache_read_tokens, 0);
        assert_eq!(rec.cache_creation_tokens, 0);
    }

    #[test]
    fn test_outcome_serde_snake_case() {
        let json = serde_json::to_string(&RequestOutcome::AccountSuspended).unwrap();
        assert_eq!(json, "\"account_suspended\"");
    }

    #[test]
    fn test_record_client_device_roundtrip() {
        let mut rec = RequestRecord::new("req-dev", "claude-sonnet-4");
        rec.client_device = Some("claude-code".to_string());
        let json = serde_json::to_string(&rec).unwrap();
        // 序列化沿用 snake_case，前端字段名即 client_device
        assert!(json.contains("\"client_device\":\"claude-code\""));
        let back: RequestRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.client_device, Some("claude-code".to_string()));
    }

    #[test]
    fn test_classify_device_claude_code() {
        assert_eq!(
            classify_device(Some("claude-code/1.2.3")),
            Some("claude-code".to_string())
        );
        // 大小写不敏感
        assert_eq!(
            classify_device(Some("Claude-Code/2.0")),
            Some("claude-code".to_string())
        );
    }

    #[test]
    fn test_classify_device_opencode() {
        assert_eq!(
            classify_device(Some("opencode/0.3.1")),
            Some("opencode".to_string())
        );
        assert_eq!(
            classify_device(Some("OpenCode/1.0 (linux)")),
            Some("opencode".to_string())
        );
    }

    #[test]
    fn test_classify_device_claude_cli() {
        // 实测入站 UA：Claude Code CLI 新版本用 claude-cli 前缀，且不带 OS 信息
        assert_eq!(
            classify_device(Some("claude-cli/2.1.201 (external, cli)")),
            Some("claude-code".to_string())
        );
        // 大小写不敏感
        assert_eq!(
            classify_device(Some("Claude-CLI/2.1.201")),
            Some("claude-code".to_string())
        );
        // CLI 不带平台信息：OS/浏览器解析返回 None 属正常（不硬造）
        assert_eq!(
            parse_client_os(Some("claude-cli/2.1.201 (external, cli)")),
            None
        );
        assert_eq!(
            parse_client_browser(Some("claude-cli/2.1.201 (external, cli)")),
            None
        );
    }

    #[test]
    fn test_classify_device_anthropic_fallback() {
        // 官方 SDK/客户端兜底：含 anthropic 关键字归入 claude-code 类
        assert_eq!(
            classify_device(Some("anthropic-sdk-python/0.39.0")),
            Some("claude-code".to_string())
        );
    }

    #[test]
    fn test_classify_device_curl() {
        assert_eq!(
            classify_device(Some("curl/8.4.0")),
            Some("curl".to_string())
        );
    }

    #[test]
    fn test_classify_device_python() {
        assert_eq!(
            classify_device(Some("python-requests/2.31.0")),
            Some("python".to_string())
        );
        assert_eq!(
            classify_device(Some("Python/3.11 aiohttp/3.9")),
            Some("python".to_string())
        );
    }

    #[test]
    fn test_classify_device_node() {
        assert_eq!(
            classify_device(Some("axios/1.6.0")),
            Some("node".to_string())
        );
        assert_eq!(
            classify_device(Some("node-fetch/2.6")),
            Some("node".to_string())
        );
    }

    #[test]
    fn test_classify_device_vscode() {
        assert_eq!(
            classify_device(Some("VSCode/1.90.0")),
            Some("vscode".to_string())
        );
    }

    /// 🔴 契约变更（刻意）：**带 Mozilla/AppleWebKit 的浏览器 UA 归 `browser`，
    /// 不再按 OS 打散成 windows/macos/linux。**
    ///
    /// 这三条测试原先断言的正是被修掉的缺陷：一个桌面 Chrome 的**身份**是「浏览器」，
    /// 而不是「Windows 设备」。旧口径把全部浏览器流量按 OS 打散进三个桶，面板上既看不出
    /// "有多少浏览器在调用"，也让 `client_os` 字段（专门承载 OS 细分）变成重复信息。
    /// OS 现在由 `parse_client_os` 单独给出，两个字段各答一个问题、前端分列展示。
    ///
    /// `windows`/`macos`/`linux` 三个取值**仍保留**（历史数据与前端图标兼容），
    /// 但只在「非浏览器、非已知客户端、UA 里裸带平台信息」时才命中 —— 见下一条测试。
    #[test]
    fn test_classify_device_browser_ua_is_browser_not_os() {
        for ua in [
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15",
            "Mozilla/5.0 (X11; Linux x86_64) Gecko/20100101",
        ] {
            assert_eq!(
                classify_device(Some(ua)),
                Some("browser".to_string()),
                "浏览器 UA 应归 browser 而非按 OS 打散：{ua}"
            );
        }
        // OS 细分仍然拿得到，只是换到了专门的字段上。
        assert_eq!(
            parse_client_os(Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36"
            )),
            Some("Windows".to_string())
        );
    }

    /// 裸 OS 标识（无 Mozilla、非已知客户端）仍落 windows/macos/linux，保持历史取值可达。
    #[test]
    fn test_classify_device_bare_os_tokens_still_reachable() {
        assert_eq!(
            classify_device(Some("MyTool/1.0 (Windows NT 10.0)")),
            Some("windows".to_string())
        );
        assert_eq!(
            classify_device(Some("MyTool/1.0 (Macintosh)")),
            Some("macos".to_string())
        );
        assert_eq!(
            classify_device(Some("MyTool/1.0 (Linux x86_64)")),
            Some("linux".to_string())
        );
    }

    /// 🔴 移动端不再被误判成桌面 OS。
    ///
    /// 改前 `classify_device` 没有移动端分支，而 iOS 的 UA 含 `Mac OS X`、Android 的含
    /// `Linux` ⇒ iPhone 被记成 `macos`、Android 被记成 `linux`。而旁边的
    /// `parse_client_os` **正确**处理了这点，于是同一条 UA 在两个字段上给出互相矛盾的
    /// 平台，按设备聚合的视图会把 iPhone 流量混进 macOS 桶。
    #[test]
    fn test_classify_device_mobile_not_misread_as_desktop_os() {
        let iphone = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) Safari/604.1";
        let android = "Mozilla/5.0 (Linux; Android 14) Chrome/124.0 Mobile Safari/537.36";
        assert_eq!(classify_device(Some(iphone)), Some("mobile".to_string()));
        assert_eq!(classify_device(Some(android)), Some("mobile".to_string()));
        // 两个字段口径一致：设备维度说 mobile，OS 维度说具体平台。
        assert_eq!(parse_client_os(Some(iphone)), Some("iOS".to_string()));
        assert_eq!(parse_client_os(Some(android)), Some("Android".to_string()));
    }

    /// 🔴 Cursor 必须被识别 —— 改前它是 `unknown`（裸 UA）或 `macos`（带 Mozilla 前缀）。
    ///
    /// Cursor 是本网关最主要的下游客户端之一，两种形态都识别不出等于面板上看不见它。
    /// 第二个用例尤其重要：它证明 OS 关键字不再覆盖客户端身份。
    #[test]
    fn test_classify_device_cursor_both_ua_shapes() {
        assert_eq!(
            classify_device(Some(
                "Cursor/0.42.3 Chrome/124.0.6367.243 Electron/30.0.6 Safari/537.36"
            )),
            Some("cursor".to_string())
        );
        assert_eq!(
            classify_device(Some(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Cursor/0.42"
            )),
            Some("cursor".to_string()),
            "OS 关键字不得覆盖客户端身份"
        );
    }

    /// 具体客户端优先于其宿主编辑器：Cline 跑在 VSCode 里，UA 可能同时含两者。
    /// 若先判 vscode，全部插件流量会塌陷成 vscode，看不出真实客户端构成。
    #[test]
    fn test_classify_device_specific_client_beats_host_editor() {
        assert_eq!(
            classify_device(Some("Mozilla/5.0 (Windows NT 10.0) vscode/1.85 cline/3.1.0")),
            Some("cline".to_string())
        );
        assert_eq!(
            classify_device(Some("Roo-Cline/3.0")),
            Some("cline".to_string())
        );
        // 未命中具体分支时才落通用 vscode。
        assert_eq!(
            classify_device(Some("VSCode/1.90.0")),
            Some("vscode".to_string())
        );
    }

    /// 新增客户端类别都能被识别（改前全部落 unknown）。
    #[test]
    fn test_classify_device_newly_recognized_clients() {
        for (ua, expect) in [
            ("Kiro/1.28.3 Chrome/128 Electron/32", "kiro"),
            ("Zed/0.150.0", "zed"),
            ("Zed-Preview/0.1", "zed"),
            ("okhttp/4.12.0", "java"),
            ("Go-http-client/2.0", "go"),
            ("opencode/1.0", "opencode"),
            ("continue/0.9", "continue"),
            ("windsurf/1.2", "windsurf"),
            ("PostmanRuntime/7.36", "postman"),
        ] {
            assert_eq!(
                classify_device(Some(ua)),
                Some(expect.to_string()),
                "UA {ua} 应归类为 {expect}"
            );
        }
    }

    /// 🔴 子串误伤回归：这些 UA **不得**命中同名短 token。
    ///
    /// 本文件在这一类问题上踩过两次（`ios` 命中 `axios`；改成 `ios/` 后**仍**命中
    /// `axios/`），所以用测试钉死。
    #[test]
    fn test_substring_traps_do_not_misfire() {
        // axios 是 Node 生态最常用 HTTP 库，属高频 UA：OS 必须为 None（不是 iOS）。
        assert_eq!(parse_client_os(Some("axios/1.6.0")), None);
        assert_eq!(
            classify_device(Some("axios/1.6.0")),
            Some("node".to_string())
        );
        // 其它含 "ios" 子串的产品名同样不得判成 iOS。
        assert_eq!(parse_client_os(Some("Kiosk/2.0")), None);
        assert_eq!(parse_client_os(Some("BiosClient/1.0")), None);
        // "zed" 是常见英文词尾，不得把 authorized 判成 Zed 编辑器。
        assert_eq!(
            classify_device(Some("Mozilla/5.0 authorized-app/1.0")),
            Some("browser".to_string())
        );
        // "java" 不得命中 javascript。
        assert_eq!(
            classify_device(Some("MyJavaScriptApp/1.0")),
            Some("unknown".to_string())
        );
        // 真实 iOS 仍须命中（词边界不能把正例也挡掉）。
        assert_eq!(
            parse_client_os(Some("MyApp/1.0 (iOS 17.0)")),
            Some("iOS".to_string())
        );
    }

    /// `token_present` 的边界语义单测（纯函数，直接测比通过 UA 间接测更稳）。
    #[test]
    fn test_token_present_word_boundary() {
        assert!(token_present("ios/17", "ios"));
        assert!(token_present("app (ios 17)", "ios"));
        assert!(token_present("ios", "ios"));
        assert!(token_present("app;ios;1", "ios"));
        assert!(!token_present("axios/1.6.0", "ios"), "前边界必须挡住 axios");
        assert!(!token_present("kiosk/2.0", "ios"));
        assert!(!token_present("iosx/1", "ios"), "后边界必须挡住 iosx");
        assert!(!token_present("", "ios"));
        // 多字节字符不得导致 panic 或漏匹配。
        assert!(token_present("客户端 ios/17", "ios"));
        assert!(!token_present("客户端axios/17", "ios"));
    }

    #[test]
    fn test_classify_device_browser() {
        // Mozilla 但不含任何 OS/客户端标识 → browser 兜底
        assert_eq!(
            classify_device(Some("Mozilla/5.0 (compatible; SomeBot/1.0)")),
            Some("browser".to_string())
        );
    }

    #[test]
    fn test_classify_device_unknown() {
        assert_eq!(classify_device(None), Some("unknown".to_string()));
        assert_eq!(classify_device(Some("")), Some("unknown".to_string()));
        assert_eq!(classify_device(Some("   ")), Some("unknown".to_string()));
        assert_eq!(
            classify_device(Some("SomethingWeird/9")),
            Some("unknown".to_string())
        );
    }

    #[test]
    fn test_classify_device_client_priority_over_os() {
        // claude-code 的 UA 夹带 Windows 信息时，仍判为 claude-code（客户端优先）
        assert_eq!(
            classify_device(Some("claude-code/1.0 (Windows NT 10.0)")),
            Some("claude-code".to_string())
        );
    }

    // ---- parse_client_os ----

    #[test]
    fn test_parse_os_windows() {
        assert_eq!(
            parse_client_os(Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36"
            )),
            Some("Windows".to_string())
        );
    }

    #[test]
    fn test_parse_os_macos() {
        assert_eq!(
            parse_client_os(Some(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15"
            )),
            Some("macOS".to_string())
        );
    }

    #[test]
    fn test_parse_os_ios() {
        // iPhone
        assert_eq!(
            parse_client_os(Some(
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15"
            )),
            Some("iOS".to_string())
        );
        // iPad 的 UA 含 "Mac OS X"，但必须判为 iOS 而非 macOS
        assert_eq!(
            parse_client_os(Some(
                "Mozilla/5.0 (iPad; CPU OS 16_0 like Mac OS X) AppleWebKit/605.1.15"
            )),
            Some("iOS".to_string())
        );
    }

    #[test]
    fn test_parse_os_android() {
        // Android 的 UA 含 "Linux"，但必须判为 Android 而非 Linux
        assert_eq!(
            parse_client_os(Some(
                "Mozilla/5.0 (Linux; Android 13; Pixel 7) AppleWebKit/537.36"
            )),
            Some("Android".to_string())
        );
    }

    #[test]
    fn test_parse_os_linux() {
        assert_eq!(
            parse_client_os(Some("Mozilla/5.0 (X11; Linux x86_64) Gecko/20100101")),
            Some("Linux".to_string())
        );
    }

    #[test]
    fn test_parse_os_none() {
        assert_eq!(parse_client_os(None), None);
        assert_eq!(parse_client_os(Some("")), None);
        assert_eq!(parse_client_os(Some("   ")), None);
        // curl 的 UA 不含 OS 信息 → None
        assert_eq!(parse_client_os(Some("curl/8.4.0")), None);
    }

    // ---- parse_client_browser ----

    #[test]
    fn test_parse_browser_edge() {
        // Edge 的 UA 同时含 Chrome/ 与 Edg/，必须判为 Edge
        assert_eq!(
            parse_client_browser(Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.2210.61"
            )),
            Some("Edge 120".to_string())
        );
    }

    #[test]
    fn test_parse_browser_chrome() {
        assert_eq!(
            parse_client_browser(Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/120.0.6099.109 Safari/537.36"
            )),
            Some("Chrome 120".to_string())
        );
    }

    #[test]
    fn test_parse_browser_firefox() {
        assert_eq!(
            parse_client_browser(Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:121.0) Gecko/20100101 Firefox/121.0"
            )),
            Some("Firefox".to_string())
        );
    }

    #[test]
    fn test_parse_browser_safari() {
        // 纯 Safari（不含 Chrome）
        assert_eq!(
            parse_client_browser(Some(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 \
                 (KHTML, like Gecko) Version/17.0 Safari/605.1.15"
            )),
            Some("Safari".to_string())
        );
    }

    #[test]
    fn test_parse_browser_non_browser_none() {
        assert_eq!(parse_client_browser(None), None);
        assert_eq!(parse_client_browser(Some("")), None);
        assert_eq!(parse_client_browser(Some("curl/8.4.0")), None);
        assert_eq!(parse_client_browser(Some("python-requests/2.31.0")), None);
        assert_eq!(parse_client_browser(Some("axios/1.6.0")), None);
    }
}
