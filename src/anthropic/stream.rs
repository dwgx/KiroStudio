//! 流式响应处理模块
//!
//! 实现 Kiro → Anthropic 流式响应转换和 SSE 状态管理

use std::borrow::Cow;
use std::collections::HashMap;

use serde_json::json;
use uuid::Uuid;

use crate::kiro::model::events::{Event, ReasoningContentEvent};
use crate::usage::RequestOutcome;

/// 一次响应的**完成状态**，贯穿流式 / 缓冲 / 非流式三条收尾路径。
///
/// # 为什么需要它
///
/// 历史 BUG：上游在流中途发来 in-band `Event::Error`、或读流/解码中断时，收尾逻辑
/// 仍按 `message_stop` + HTTP 200 正常结束，用量埋点也硬编码 `outcome=Success`。
/// 下游 Claude Code 收到 200 + `end_turn` 就把**截断输出当成功**，既不重试、又污染
/// 熔断/健康信号（失败被记成成功）。
///
/// `CompletionStatus` 把「这次到底成没成」显式建模，收尾时据此统一决定三件事：
/// - 用量记账的 [`RequestOutcome`]（RateLimited / ServerError / NetworkError…）
/// - 回给客户端的 SSE `error` 事件类型（overloaded_error / api_error）
/// - 非流式响应的 HTTP 状态码（429 / 502）
///
/// # 铁律
///
/// `ContentLengthExceededException`（= max_tokens 干净收尾）**不是**失败，
/// 不设置任何非 `Ok` 状态；它照常走 message_stop + 200。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionStatus {
    /// 正常完成（含 max_tokens 干净收尾）
    Ok,
    /// 上游在响应流中 in-band 下发的错误事件（`:message-type=error`）
    UpstreamError { code: String, message: String },
    /// 读流 / 读响应体传输中断（未拿到完整响应）
    TransportError { message: String },
    /// 解码器连续错误超限、永久停止（响应必然截断）
    DecoderStopped { message: String },
}

impl CompletionStatus {
    /// 是否正常完成
    pub fn is_ok(&self) -> bool {
        matches!(self, CompletionStatus::Ok)
    }

    /// 映射为用量记账的最终结果分类。
    ///
    /// 上游错误按 code/message 关键字粗分：限流类 → `RateLimited`，其余 → `ServerError`；
    /// 传输中断 → `NetworkError`；解码器停止 → `ServerError`（响应被上游截断）。
    pub fn outcome(&self) -> RequestOutcome {
        match self {
            CompletionStatus::Ok => RequestOutcome::Success,
            CompletionStatus::UpstreamError { code, message } => {
                if is_rate_limit_signal(code) || is_rate_limit_signal(message) {
                    RequestOutcome::RateLimited
                } else {
                    RequestOutcome::ServerError
                }
            }
            CompletionStatus::TransportError { .. } => RequestOutcome::NetworkError,
            CompletionStatus::DecoderStopped { .. } => RequestOutcome::ServerError,
        }
    }

    /// 回给客户端的 SSE `error` 事件的 `type` 字段。
    ///
    /// 限流类用 `overloaded_error`（Claude Code 会按过载退避重试），其余用 `api_error`。
    pub fn sse_error_type(&self) -> &'static str {
        match self {
            CompletionStatus::Ok => "api_error",
            CompletionStatus::UpstreamError { code, message } => {
                if is_rate_limit_signal(code) || is_rate_limit_signal(message) {
                    "overloaded_error"
                } else {
                    "api_error"
                }
            }
            CompletionStatus::TransportError { .. } => "api_error",
            CompletionStatus::DecoderStopped { .. } => "api_error",
        }
    }

    /// 非流式响应的 HTTP 状态码：限流 429，其余 502。
    pub fn http_status_u16(&self) -> u16 {
        match self.outcome() {
            RequestOutcome::RateLimited => 429,
            _ => 502,
        }
    }

    /// 面向客户端的错误描述（用于 SSE error 事件 / 非流式错误体）。
    pub fn client_message(&self) -> String {
        match self {
            CompletionStatus::Ok => String::new(),
            CompletionStatus::UpstreamError { code, message } => {
                if message.is_empty() {
                    format!("上游返回错误: {}", code)
                } else {
                    format!("上游返回错误: {} - {}", code, message)
                }
            }
            CompletionStatus::TransportError { message } => {
                format!("上游响应流中断: {}", message)
            }
            CompletionStatus::DecoderStopped { message } => {
                format!("上游响应解析中断: {}", message)
            }
        }
    }
}

/// 判断错误 code/message 是否属于「限流/过载」信号（大小写不敏感的关键字匹配）。
fn is_rate_limit_signal(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("throttl")
        || lower.contains("toomanyrequests")
        || lower.contains("too many requests")
        || lower.contains("ratelimit")
        || lower.contains("rate limit")
        || lower.contains("429")
        || lower.contains("overload")
        || lower.contains("quota")
        || lower.contains("exhaust")
}

/// thinking 块的 signature 占位字符串。
///
/// Anthropic 协议下，流式 `{type:"thinking"}` 块结束前必须发一个 `signature_delta`
/// 事件，SDK 会把它聚合进 thinking 块的 `signature` 字段。客户端（Claude Code）在
/// 下一轮把该 assistant 消息回传时会本地校验 thinking 块必须带**非空** signature，
/// 否则抛出 `The content[].thinking in the thinking mode must be passed back to the API`。
///
/// 上游 Kiro 不是 Anthropic 服务端，不下发真实签名，因此这里发一个非空占位字符串以
/// 满足客户端本地校验。该占位符只在客户端 ↔ KiroStudio 之间存在：回传时 converter 只读
/// `block.thinking`，`ContentBlock` 无 signature 字段且未 deny_unknown_fields，serde
/// 静默丢弃客户端回传的假签名，故永不转发给 Kiro。
pub(super) const THINKING_SIGNATURE_PLACEHOLDER: &str = "kirostudio-thinking-signature";

/// Prompt 缓存记账明细（影子估算：凭 continuationId 前缀估算 Bedrock prefix cache 命中量）。
///
/// Bedrock prefix cache 是不透明的——上游不返回 cache_read_input_tokens。
/// 通过在本地估算 [系统提示 + 历史轮次] 的 token 数来填充此字段，让 Claude Code
/// 客户端的 "cache hits" 指标正常显示。
#[derive(Debug, Clone, Copy)]
pub(crate) struct CacheUsageBreakdown {
    pub cache_creation_input_tokens: i32,
    pub cache_read_input_tokens: i32,
    pub cache_creation_5m_input_tokens: i32,
    pub cache_creation_1h_input_tokens: i32,
}

/// 将总输入 token 转为 Anthropic usage 的 input_tokens 口径（剔除 cache 读写）
///
/// Anthropic 语义：`usage.input_tokens` 只计「未命中缓存、非本次新建缓存」的部分，
/// cache_read / cache_creation 单独列出。
pub(crate) fn billed_input_tokens(
    input_tokens: i32,
    cache_creation_input_tokens: i32,
    cache_read_input_tokens: i32,
) -> i32 {
    input_tokens
        .saturating_sub(cache_creation_input_tokens)
        .saturating_sub(cache_read_input_tokens)
        .max(0)
}

/// 找到小于等于目标位置的最近有效UTF-8字符边界
///
/// UTF-8字符可能占用1-4个字节，直接按字节位置切片可能会切在多字节字符中间导致panic。
/// 这个函数从目标位置向前搜索，找到最近的有效字符边界。
fn find_char_boundary(s: &str, target: usize) -> usize {
    if target >= s.len() {
        return s.len();
    }
    if target == 0 {
        return 0;
    }
    // 从目标位置向前搜索有效的字符边界
    let mut pos = target;
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// 需要跳过的包裹字符
///
/// 当 thinking 标签被这些字符包裹时，认为是在引用标签而非真正的标签：
/// - 反引号 (`)：行内代码
/// - 双引号 (")：字符串
/// - 单引号 (')：字符串
const QUOTE_CHARS: &[u8] = &[b'`', b'"', b'\''];

/// 检查指定位置的字符是否是引用字符
fn is_quote_char(buffer: &str, pos: usize) -> bool {
    buffer
        .as_bytes()
        .get(pos)
        .map(|c| QUOTE_CHARS.contains(c))
        .unwrap_or(false)
}

/// thinking 标签名（不含 `<` / `/` / `>`），大小写不敏感比对。
const THINKING_TAG_NAME: &[u8] = b"thinking";

/// 标签名之后到 `>` 之间允许的最大字节数（属性区上限）。
///
/// # 为什么必须有上限
///
/// 放宽为「容属性」后，「可能是半个标签」的尾巴**失去了 10 字节的天然上界**
/// （`<thinking foo="...">` 可任意长）。若无上限，一个永不闭合的 `<thinking xxxx...`
/// 会让扣留窗口无界增长 ⇒ 整条流的可见文本全被囤住不下发，复刻已知问题 #14
/// （`invoke_sniff_buffer` 无界持有 → 流停摆）。64 字节远超真实属性
/// （实测生产方 `converter.rs` 根本不发属性），超出即判定「这不是标签」。
const MAX_THINKING_TAG_INNER_BYTES: usize = 64;

/// 一个 thinking 标签的匹配结果：起始字节位置 + **实际**字节长度。
///
/// # 为什么必须带长度
///
/// 放宽为大小写不敏感 + 容属性后，标签长度**不再是常量**：
/// `<thinking>` 10 字节、`<thinking foo="1">` 18 字节、`</thinking >` 12 字节。
/// 此前全套查找函数只返回起点，调用方各自写死 `"<thinking>".len()` 跳过标签
/// （10 处，见 git 历史），任一处漏改就会把属性残片（`foo="1">`）留在缓冲里当正文。
/// 把长度和起点绑在同一个返回值里，调用方**无从假设**固定长度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ThinkingTagMatch {
    /// `<` 所在的字节下标
    start: usize,
    /// 从 `<` 到 `>`（含）的字节数
    len: usize,
}

impl ThinkingTagMatch {
    /// 标签之后第一个字节的下标
    fn end(&self) -> usize {
        self.start + self.len
    }
}

/// `buffer[pos..]` 是否恰好以一个 thinking 标签开头；是则返回该标签字节长度。
///
/// 语法（**大小写不敏感**）：
/// - 开标签：`<thinking` + 可选属性区 + `>`，属性区必须以空白起头
///   （否则 `<thinkingfoo>` 会被误认成 thinking 标签）
/// - 闭标签：`</thinking` + 可选空白 + `>`
///
/// 属性区/空白区内出现 `<` 或换行即判定「不是标签」——散文里的 `a < b`、
/// 跨行的 `<` 不该被吞成标签。
///
/// 返回 `None` 有两种含义（调用方须自行区分）：此处不是标签，或标签尚未到齐
/// （还没见到 `>`，可能跨 chunk）。
fn thinking_tag_len_at(buffer: &str, pos: usize, closing: bool) -> Option<usize> {
    let b = buffer.as_bytes();
    let mut i = pos;
    if b.get(i) != Some(&b'<') {
        return None;
    }
    i += 1;
    if closing {
        if b.get(i) != Some(&b'/') {
            return None;
        }
        i += 1;
    } else if b.get(i) == Some(&b'/') {
        // 闭标签不得被当成开标签
        return None;
    }
    let name_end = i + THINKING_TAG_NAME.len();
    if b.len() < name_end || !b[i..name_end].eq_ignore_ascii_case(THINKING_TAG_NAME) {
        return None;
    }
    i = name_end;
    match b.get(i) {
        Some(&b'>') => return Some(i + 1 - pos),
        // 名字后必须是 `>` 或空白，否则是别的标签名（`<thinkingfoo>`）
        Some(c) if c.is_ascii_whitespace() => {}
        _ => return None,
    }
    let inner_start = i;
    while let Some(&c) = b.get(i) {
        if i - inner_start >= MAX_THINKING_TAG_INNER_BYTES {
            return None;
        }
        match c {
            b'>' => return Some(i + 1 - pos),
            // 散文里的 `<` / 跨行内容 ⇒ 不是标签
            b'<' | b'\n' | b'\r' => return None,
            // 闭标签的 `>` 之前只允许空白（`</thinking >`）
            _ if closing && !c.is_ascii_whitespace() => return None,
            _ => i += 1,
        }
    }
    // 到缓冲末尾还没见到 `>` —— 可能是跨 chunk 的半标签，交由扣留逻辑处理
    None
}

/// 从 `from` 起扫描下一个 thinking 标签（不做任何引用/后缀判定）。
///
/// 全套 `find_real_*` 都建立在它之上，**标签形态的判据只有这一份**。本仓的教训是
/// 两套判据必然漂移，漂移的后果是「某形态在一条路径被剥、在另一条泄漏」。
fn scan_thinking_tag(buffer: &str, from: usize, closing: bool) -> Option<ThinkingTagMatch> {
    let b = buffer.as_bytes();
    let mut i = from.min(b.len());
    while i < b.len() {
        // `<` 是 ASCII，命中位置必在字符边界上，切片安全
        if b[i] == b'<' {
            if let Some(len) = thinking_tag_len_at(buffer, i, closing) {
                return Some(ThinkingTagMatch { start: i, len });
            }
        }
        i += 1;
    }
    None
}

/// 标签是否被引用字符包裹（正文里在**引用**标签，不是真标签）。
fn thinking_tag_is_quoted(buffer: &str, m: &ThinkingTagMatch) -> bool {
    let before = m.start > 0 && is_quote_char(buffer, m.start - 1);
    before || is_quote_char(buffer, m.end())
}

/// 查找真正的 thinking 结束标签（不被引用字符包裹，且后面有双换行符）
///
/// 当模型在思考过程中提到 `</thinking>` 时，通常会用反引号、引号等包裹，
/// 或者在同一行有其他内容（如"关于 </thinking> 标签"）。
/// 这个函数会跳过这些情况，只返回真正的结束标签位置。
///
/// 跳过的情况：
/// - 被引用字符包裹（反引号、引号等）
/// - 后面没有双换行符（真正的结束标签后面会有 `\n\n`）
/// - 标签在缓冲区末尾（流式处理时需要等待更多内容）
///
/// # 参数
/// - `buffer`: 要搜索的字符串
///
/// # 返回值
/// - `Some(pos)`: 真正的结束标签的起始位置
/// - `None`: 没有找到真正的结束标签
fn find_real_thinking_end_tag(buffer: &str) -> Option<ThinkingTagMatch> {
    let mut search_start = 0;

    while let Some(m) = scan_thinking_tag(buffer, search_start, true) {
        let absolute_pos = m.start;

        // 如果被引用字符包裹，跳过
        if thinking_tag_is_quoted(buffer, &m) {
            search_start = absolute_pos + 1;
            continue;
        }

        // 检查后面的内容
        let after_content = &buffer[m.end()..];

        // 标签后什么都还没到 → 等更多内容（可能是 `\n\n` 也可能是别的）
        if after_content.is_empty() {
            return None;
        }

        let next = after_content.chars().next().unwrap();

        // 紧跟下一个标签（`</thinking><invoke ...`）→ 立即判定结束，零字节可等。
        if next == '<' {
            return Some(m);
        }

        if next.is_whitespace() {
            let ws: &str = {
                let n: usize = after_content
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .map(char::len_utf8)
                    .sum();
                &after_content[..n]
            };
            // ⚠️ 跨 chunk 关键：空白串若**一直延伸到缓冲末尾且还没攒够 `\n\n`**，
            // 它仍可能长成完整段落分隔（`\n` 在本 chunk、第二个 `\n` 在下一个）。此时必须等，
            // 否则只消耗先到的那一个换行，剩下的漏进正文变成开头多一个空行。
            // 流真就此结束时由 `find_real_thinking_end_tag_at_buffer_end` 兜底收尾。
            if ws.len() == after_content.len() && ws.len() < PARAGRAPH_BREAK_LEN {
                return None;
            }
            // 空白串里**含换行**才算真结束（另起一行/空行）。纯行内空白（`</thinking> more`）
            // 更像正文里顺口提到标签，不认 —— 认了会把思考截断在半句话上。
            if ws.contains('\n') {
                return Some(m);
            }
            search_start = absolute_pos + 1;
            continue;
        }

        // 后面紧跟普通正文字符 → 更像是正文里提到标签，跳过继续搜索
        search_start = absolute_pos + 1;
    }

    None
}

/// 段落分隔（`\n\n`）的字节数 —— 结束标签后最多消耗这么多换行。
const PARAGRAPH_BREAK_LEN: usize = 2;

/// `</thinking>` 结束标签**实际消耗的字节数**（标签本身 + 其后最多一个 `\n\n` 段落分隔）。
///
/// # 为什么不能写死 `"</thinking>\n\n".len()`
///
/// [`find_real_thinking_end_tag`] 的后缀判据已放宽为「任意空白或紧跟 `<`」——
/// 只有 `\n\n` 这一种形态才恰好是 13 字节。写死 13 会在其余形态下**多切 2 字节**，
/// 把正文首两个字符吃掉（`</thinking>\nAnswer` → 切掉 `\nA` → 客户端看到 `nswer`）；
/// 而紧跟 `<invoke` 时更会切掉 `<i`，让文本化工具调用**永远无法重组**。
///
/// # 为什么参数是 [`ThinkingTagMatch`] 而不是位置
///
/// 标签本身也不是定长（`</thinking>` 11 / `</thinking >` 12 字节）。只给位置的话
/// 本函数只能写死 11，带属性/带空白的闭标签就会切错。长度必须由**匹配方**给出。
///
/// 语义：跳过标签，再跳过最多两个换行（保持既有 `\n\n` 段落分隔的剥离行为），
/// 其余字符一律保留。
fn thinking_end_tag_consumed_len(buffer: &str, m: &ThinkingTagMatch) -> usize {
    let rest = &buffer[m.end().min(buffer.len())..];
    let nl = rest
        .bytes()
        .take(PARAGRAPH_BREAK_LEN)
        .take_while(|b| *b == b'\n')
        .count();
    m.len + nl
}

/// 查找缓冲区末尾的 thinking 结束标签（允许末尾只有空白字符）
///
/// 用于“边界事件”场景：例如 thinking 结束后立刻进入 tool_use，或流结束，
/// 此时 `</thinking>` 后面可能没有 `\n\n`，但结束标签依然应被识别并过滤。
///
/// 约束：只有当 `</thinking>` 之后全部都是空白字符时才认为是结束标签，
/// 以避免在 thinking 内容中提到 `</thinking>`（非结束标签）时误判。
fn find_real_thinking_end_tag_at_buffer_end(buffer: &str) -> Option<ThinkingTagMatch> {
    let mut search_start = 0;

    while let Some(m) = scan_thinking_tag(buffer, search_start, true) {
        if thinking_tag_is_quoted(buffer, &m) {
            search_start = m.start + 1;
            continue;
        }

        // 只有当标签后面全部是空白字符时才认定为结束标签
        if buffer[m.end()..].trim().is_empty() {
            return Some(m);
        }

        search_start = m.start + 1;
    }

    None
}

/// 找到一个「[`find_real_thinking_end_tag`] 的严格判据**永远不可能再满足**」的 `</thinking>`。
///
/// # 为什么需要它（否则答案会被永久丢弃）
///
/// 严格判据要求结束标签后跟「含换行的空白」或 `<`。它对**跨 chunk**是必要的：标签后
/// 还什么都没到时必须等，否则会把段落分隔的后半个换行漏进正文。
///
/// 但有一类形态**等也没用**：`</thinking>Answer` —— 标签后紧跟的普通字符**已经到了**，
/// 后续 chunk 再来多少内容都改不了它，严格判据对这个位置永久为假。
/// 而 [`StreamContext::strip_inline_thinking_when_disabled`] 在判据返回 `None` 时会
/// **丢弃整段**（客户端没要 thinking ⇒ 未闭合就全是思考内容）⇒ `Answer` 连同后面
/// 所有正文一起消失，客户端收到**空回答**，而这在面板上是一次「成功」——完全无痕。
///
/// 本函数把「该等」与「等也没用」区分开，只对后者放行。判据是**可证的**而非启发式：
///
/// | 标签后 | 结论 |
/// |---|---|
/// | 空 | **等** —— 可能长成 `\n\n` 或 `<` |
/// | 全空白且无换行、且顶到缓冲末尾 | **等** —— 下一个 chunk 可能补上换行 |
/// | 含换行的空白 / `<` | 严格判据本就会命中，不该走到这里 |
/// | 普通字符（非空白非 `<`） | **永久不可满足 ⇒ 就地判定结束** |
///
/// 只在 `!thinking_enabled` 的剥离路径用。thinking 开启时残留会进 thinking 面板、
/// 不算泄漏也不算吞字，无需放宽（放宽反而可能把正文里顺口提到的标签当成真结束）。
fn find_permanently_unsatisfiable_end_tag(buffer: &str) -> Option<ThinkingTagMatch> {
    let mut search_start = 0;
    while let Some(m) = scan_thinking_tag(buffer, search_start, true) {
        let absolute_pos = m.start;
        let after = &buffer[m.end()..];
        match after.chars().next() {
            // 标签后还什么都没到 → 等
            None => return None,
            Some(c) if c == '<' => {
                // 严格判据会命中，交给它
                search_start = absolute_pos + 1;
            }
            Some(c) if c.is_whitespace() => {
                let ws_len: usize = after
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .map(char::len_utf8)
                    .sum();
                if ws_len == after.len() {
                    // 空白顶到缓冲末尾 → 还可能长成 `\n\n`，等
                    return None;
                }
                // 空白后还有别的内容：含换行则严格判据已命中；纯行内空白（`</thinking> more`）
                // 严格判据判为"正文顺口提到标签"。两种都交给严格判据，这里不抢。
                search_start = absolute_pos + 1;
            }
            // 普通字符已就位 ⇒ 该位置的严格判据永久为假
            Some(_) => return Some(m),
        }
    }
    None
}

/// 找**孤立的** thinking 闭标签：没有配对开标签的 `</thinking>`。
///
/// # 为什么它需要一套独立（最宽松）的判据
///
/// [`find_real_thinking_end_tag`] 的后缀判据（要求标签后跟含换行的空白或 `<`）是为
/// **闭合一个已开启的思考块**服务的：判早了会把思考截断在半句话上，所以宁可等。
/// 但「没有开标签」时那套判据反而有害 —— `答案开始</thinking>答案继续` 的两侧都是
/// **真正文**，没有任何理由等，也没有任何理由丢；而不认它的直接后果就是标签字面量
/// 原样进 `text_delta`（实测泄漏形态①②）。
///
/// 处置：把它当纯标记剥掉，两侧正文都保留。唯一保留的过滤是引用包裹
/// （`用 \`</thinking>\` 结束` 是正文在引用标签，不是标签）。
///
/// 只在「不在思考块内」时调用 —— 块内必须走严格判据，否则正文里顺口提到的标签
/// 会把思考块提前掐断。
fn find_stray_thinking_end_tag(buffer: &str) -> Option<ThinkingTagMatch> {
    let mut search_start = 0;
    while let Some(m) = scan_thinking_tag(buffer, search_start, true) {
        if thinking_tag_is_quoted(buffer, &m) {
            search_start = m.start + 1;
            continue;
        }
        return Some(m);
    }
    None
}

/// 把一段**即将作为可见正文下发**的文本里的孤立闭标签剥掉（保留两侧正文）。
///
/// 用于几条「缓冲原样倒给客户端」的收尾路径（`thinking_extracted` 之后的剩余内容、
/// EOF 残留）。这些路径此前是 [`find_stray_thinking_end_tag`] 的旁路：主循环剥了，
/// 收尾分支照样把标签倒出去。判据复用同一个函数，不新写匹配。
fn strip_stray_thinking_end_tags(text: &str) -> Cow<'_, str> {
    if find_stray_thinking_end_tag(text).is_none() {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(m) = find_stray_thinking_end_tag(rest) {
        out.push_str(&rest[..m.start]);
        let cut = m.start + thinking_end_tag_consumed_len(rest, &m);
        rest = &rest[cut..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// 流已结束（EOF）时，把 thinking 剥离器的残留缓冲拆成「丢弃的思考」与「必须下发的可见尾巴」。
///
/// # 为什么 EOF 需要一套单独的判据
///
/// 流式期间三个查找函数都刻意保守：[`find_real_thinking_end_tag`] 要求标签后跟空白或 `<`，
/// [`find_real_thinking_end_tag_at_buffer_end`] 要求标签后**全是**空白。保守是对的 ——
/// 半个标签跨 chunk 到达时宁可多等一个 chunk，也不能把它当正文吐出去。
///
/// 但 EOF 时「再等一个 chunk」已不存在，保守就变成了**静默吞字**：
/// `</thinking>Answer`（零空白、紧跟普通字符）这一形态三个函数全不认 ⇒
/// `in_thinking_block` 永远回不到 false ⇒ 整个 `Answer` 连同标签一起蒸发，
/// 客户端收到空回答，而面板上这是一次「成功」——**完全无痕**。
///
/// EOF 时缓冲里就是全部剩余内容，不再有歧义，因此可以放宽到「字面量 `</thinking>`」。
///
/// # 返回值
///
/// 标签后的内容（`trim_start` 后）作为可见文本返回；找不到标签时返回空串
/// （整段都还在未闭合的思考块里 ⇒ 全部丢弃，与流式期间「客户端没要就不给」的口径一致，
/// 也与 [`StreamContext::process_reasoning_content`] 在 `!thinking_enabled` 时直接丢帧一致）。
fn split_unclosed_thinking_residue_at_eof(buffer: &str) -> &str {
    // 不做引用包裹判定（保持「EOF 最宽松」的既有语义），但标签形态仍走统一匹配，
    // 否则大写/带属性的闭标签在这里认不出来 ⇒ 标签后的正文被整段丢弃。
    match scan_thinking_tag(buffer, 0, true) {
        Some(m) => buffer[m.end()..].trim_start(),
        None => "",
    }
}

/// `tail`（以 `<` 开头、且**尚未**见到 `>`）是否还可能长成一个 thinking 标签。
fn could_grow_into_thinking_tag(tail: &str) -> bool {
    let b = tail.as_bytes();
    debug_assert_eq!(b.first(), Some(&b'<'));
    let mut i = 1;
    if b.get(i) == Some(&b'/') {
        i += 1;
    }
    let rest = &b[i.min(b.len())..];
    let n = THINKING_TAG_NAME.len();
    if rest.len() < n {
        // 名字还没打完：必须是名字的真前缀（`<thi` 可以，`<div` 不行）
        return THINKING_TAG_NAME[..rest.len()].eq_ignore_ascii_case(rest);
    }
    if !rest[..n].eq_ignore_ascii_case(THINKING_TAG_NAME) {
        return false;
    }
    // 名字已完整、`>` 还没到 ⇒ 处在属性区（开标签）或空白区（闭标签）。
    // 上限与 `thinking_tag_len_at` 一致，否则扣留窗口会无界增长（见
    // `MAX_THINKING_TAG_INNER_BYTES` 的说明）。
    let inner = &rest[n..];
    if inner.len() > MAX_THINKING_TAG_INNER_BYTES {
        return false;
    }
    // 属性区起头必须是空白；区内不得出现 `<` 或换行
    if let Some(&first) = inner.first() {
        if !first.is_ascii_whitespace() {
            return false;
        }
    }
    !inner.iter().any(|c| matches!(c, b'<' | b'\n' | b'\r'))
}

/// 缓冲区末尾**真的可能是 thinking 标签**的字节数（0 = 尾巴不可能是标签，可立即放行）。
///
/// # 为什么不能无条件扣一个固定长度
///
/// 扣留尾巴是为了防"标签跨 chunk 断开时把半个标签当正文吐出去"。但无条件扣
/// `"<thinking>".len()` = **10 字节**会连带扣住别的东西 —— `</invoke>` 恰好只有
/// **9 字节**，于是文本化 invoke 的闭合标签被扣在缓冲里，重组层永远看到未闭合的
/// `<invoke`，把它当纯文本吐出去 → **工具不执行**。
/// （这正是 `generate_final_events` 那条 reclaim 旁路的成因，同型缺陷。）
///
/// 反过来，扣得太少同样致命：固定扣 10 字节**盖不住 11 字节的 `</thinking>`**，
/// 于是孤立闭标签（实测泄漏形态①②）整条穿透进可见正文。
///
/// 所以判据不能是「某个字面量的真前缀」，只能是**按标签语法判定**：
///
/// | 尾巴 | 结论 |
/// |---|---|
/// | 不含 `<` | 0（散文尾巴立刻放行，首字节少等一个 chunk） |
/// | `<` 之后不可能长成 thinking 标签（`</invoke>`、`a < b`） | 0 |
/// | 半个标签（`<thin` / `</thinki` / `<thinking fo`） | 扣住整条尾巴 |
/// | 已是完整标签、其后**只剩空白** | 扣住 —— `\n\n` 段落分隔可能跨 chunk 未到齐 |
/// | 已是完整标签、其后已有实质内容 | 0 —— 该由 finder 判定，不是"等更多"的形态 |
///
/// 上界由 [`MAX_THINKING_TAG_INNER_BYTES`] 保证（带属性后标签不再定长，无上限会
/// 复刻已知问题 #14 的流停摆）。
fn partial_thinking_tag_suffix_len(buffer: &str) -> usize {
    // 标签必以 `<` 开头；只有最后一个 `<` 之后的部分才可能是"还没到齐的标签"。
    let Some(p) = buffer.rfind('<') else {
        return 0;
    };
    let tail_len = buffer.len() - p;
    for closing in [true, false] {
        if let Some(len) = thinking_tag_len_at(buffer, p, closing) {
            let rest = &buffer[p + len..];
            return if rest.trim().is_empty() { tail_len } else { 0 };
        }
    }
    if could_grow_into_thinking_tag(&buffer[p..]) {
        tail_len
    } else {
        0
    }
}

/// 查找真正的 thinking 开始标签（不被引用字符包裹）
///
/// 与 `find_real_thinking_end_tag` 类似，跳过被引用字符包裹的开始标签。
fn find_real_thinking_start_tag(buffer: &str) -> Option<ThinkingTagMatch> {
    let mut search_start = 0;

    while let Some(m) = scan_thinking_tag(buffer, search_start, false) {
        // 如果不被引用字符包裹，则是真正的开始标签
        if !thinking_tag_is_quoted(buffer, &m) {
            return Some(m);
        }

        // 继续搜索下一个匹配
        search_start = m.start + 1;
    }

    None
}

/// 从完整文本中提取 thinking 块（用于非流式响应）
///
/// 使用与流式处理相同的标签检测逻辑（引用字符过滤），确保一致性。
/// 非流式场景下文本已完整，无需处理跨 chunk 分割问题。
///
/// # 返回值
/// - `(Some(thinking_content), remaining_text)` — 检测到有效 thinking 块
/// - `(None, original_text)` — 未检测到，原样返回
pub(crate) fn extract_thinking_from_complete_text(text: &str) -> (Option<String>, String) {
    let open = match find_real_thinking_start_tag(text) {
        Some(m) => m,
        // 没有开标签，但可能有**孤立闭标签**（形态①）。原样返回就是把标签字面量
        // 交给客户端，故仍需剥一遍；判据与流式路径同一函数。
        None => return (None, strip_stray_thinking_end_tags(text).into_owned()),
    };

    let before = &text[..open.start];
    let after_open = &text[open.end()..];

    // 查找结束标签：优先匹配带 \n\n 后缀的，退而使用末尾匹配
    let (thinking_raw, text_after) = if let Some(m) = find_real_thinking_end_tag(after_open) {
        (
            &after_open[..m.start],
            &after_open[m.start + thinking_end_tag_consumed_len(after_open, &m)..],
        )
    } else if let Some(m) = find_real_thinking_end_tag_at_buffer_end(after_open) {
        (&after_open[..m.start], after_open[m.end()..].trim_start())
    } else {
        // 找不到有效的结束标签，不做提取
        return (None, text.to_string());
    };

    // 剥离开头的换行符（与流式处理一致：模型输出 <thinking>\n）
    let thinking_content = thinking_raw.strip_prefix('\n').unwrap_or(thinking_raw);

    // 组装剩余文本：跳过纯空白的 before 部分
    let mut remaining = String::new();
    if !before.trim().is_empty() {
        remaining.push_str(before);
    }
    remaining.push_str(text_after);

    if thinking_content.is_empty() {
        (None, remaining)
    } else {
        (Some(thinking_content.to_string()), remaining)
    }
}

/// 客户端**没有**声明 thinking 时，从**完整文本**（非流式响应）里剥掉内联 `<thinking>` 块。
///
/// # 为什么非流式也必须剥
///
/// 剥离逻辑此前只存在于流式路径（[`StreamContext::strip_inline_thinking_when_disabled`]），
/// 而非流式 `handlers.rs` 的 `!thinking_enabled` 分支把上游文本**原样**塞进响应 ⇒
/// 内联 `<thinking>` 标签连同模型的内部推理**逐字泄漏**给客户端。
///
/// 同一种内容在本仓已有明确口径：`process_reasoning_content` 在 `!thinking_enabled` 时
/// **直接丢弃整帧**。流式剥、非流式漏，是同一内容两套处置。
///
/// # 判据完全复用，不新写一套
///
/// 起止标签走 [`find_real_thinking_start_tag`] / [`find_real_thinking_end_tag`] /
/// [`find_real_thinking_end_tag_at_buffer_end`]，EOF 兜底走
/// [`split_unclosed_thinking_residue_at_eof`] —— 与流式路径**同一批函数**。
/// 本仓的教训是两套判据必然漂移，而漂移的后果是「某形态在一条路径被剥、在另一条泄漏」。
///
/// # 与 thinking 开启时的差异
///
/// [`extract_thinking_from_complete_text`] 在找不到有效结束标签时**原样返回**
/// （thinking 开启时那是对的：内容会进 thinking 面板，不算泄漏）。
/// 这里不行 —— 原样返回就是泄漏本体。故未闭合时按 EOF 兜底处理：
/// 丢思考本体，只留标签之后的正文。
pub(crate) fn strip_thinking_from_complete_text(text: &str) -> String {
    let Some(open) = find_real_thinking_start_tag(text) else {
        // 无开标签时仍可能有孤立闭标签（形态①）——原样返回即泄漏，剥一遍。
        return strip_stray_thinking_end_tags(text).into_owned();
    };

    let before = &text[..open.start];
    let after_open = &text[open.end()..];

    let text_after = if let Some(m) = find_real_thinking_end_tag(after_open) {
        &after_open[m.start + thinking_end_tag_consumed_len(after_open, &m)..]
    } else if let Some(m) = find_real_thinking_end_tag_at_buffer_end(after_open) {
        after_open[m.end()..].trim_start()
    } else {
        // 未闭合（或闭合形态是流式判据不认的 `</thinking>Answer`）：EOF 兜底。
        split_unclosed_thinking_residue_at_eof(after_open)
    };

    // 与 `extract_thinking_from_complete_text` 一致：纯空白的 before 不保留
    // （模型常输出 `\n<thinking>`，留着会让正文凭空多一个前导空行）。
    let mut out = String::new();
    if !before.trim().is_empty() {
        out.push_str(before);
    }
    out.push_str(text_after);
    out
}

/// SSE 事件
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: serde_json::Value,
}

/// 判定一个 SSE 事件是否是「首 token」候选 —— 即**真实模型输出**的第一片。
///
/// 只认三种 delta：
/// - `text_delta`（正文）、`thinking_delta`（思考）：必须**非空**。空 delta 是关块占位
///   （见 `generate_final_events` 里补空 thinking_delta 的那几处），不代表有内容产出。
/// - `input_json_delta`（工具参数）：工具调用本身就是输出，无需判空。
///
/// 明确**不算**首 token 的：`message_start` / `ping` / `content_block_start`
/// （都不含模型输出，且 `message_start` 在流开始前就发了）、`signature_delta`
/// （thinking 块的签名收尾，不是内容）。
///
/// 判空是刻意的保险：`create_text_delta_events` 自身无空串守卫且有 8 处调用方，
/// 未逐一验证是否都传非空 —— 若某处传空，这里挡住，不会把空 delta 误当首 token。
fn is_first_content_delta(e: &SseEvent) -> bool {
    if e.event != "content_block_delta" {
        return false;
    }
    let d = &e.data["delta"];
    match d["type"].as_str() {
        Some("text_delta") => !d["text"].as_str().unwrap_or("").is_empty(),
        Some("thinking_delta") => !d["thinking"].as_str().unwrap_or("").is_empty(),
        Some("input_json_delta") => true,
        _ => false,
    }
}

/// 该事件是否为**用户可见正文**的 delta（text / tool_use 的 input，**不含** thinking）。
///
/// 与 [`is_first_content_delta`] 只差 `thinking_delta` 一项，但**不能复用它**：那个判据服务
/// TTFB 打点（thinking 也算"上游开始出货"），而这里服务「本轮到底有没有给用户看的东西」——
/// 空响应兜底的判据里若把 thinking 算进正文，`!thinking_enabled` 下恰好永远为假、
/// `thinking_enabled` 下又会把纯思考轮当成有正文，两头都错。
fn is_visible_body_delta(e: &SseEvent) -> bool {
    if e.event != "content_block_delta" {
        return false;
    }
    let d = &e.data["delta"];
    match d["type"].as_str() {
        Some("text_delta") => !d["text"].as_str().unwrap_or("").is_empty(),
        Some("input_json_delta") => true,
        _ => false,
    }
}

impl SseEvent {
    pub fn new(event: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            event: event.into(),
            data,
        }
    }

    /// 格式化为 SSE 字符串
    pub fn to_sse_string(&self) -> String {
        format!(
            "event: {}\ndata: {}\n\n",
            self.event,
            serde_json::to_string(&self.data).unwrap_or_default()
        )
    }

    /// 构造 Anthropic 规范的 SSE `error` 事件。
    ///
    /// 上游流中途失败(读流 Err)时用它显式告知客户端"本次响应未正常完成"，
    /// 而非把截断的输出当作 message_stop 正常收尾——后者会让 Claude Code 把半截结果当成功，
    /// 不触发重试。发了 error 事件，客户端(Claude Code)才会按 overloaded/api_error 退避重试。
    /// 形如 `{"type":"error","error":{"type":"overloaded_error","message":"..."}}`。
    pub fn error_event(error_type: &str, message: impl Into<String>) -> Self {
        Self::new(
            "error",
            serde_json::json!({
                "type": "error",
                "error": { "type": error_type, "message": message.into() },
            }),
        )
    }
}

/// 内容块状态
#[derive(Debug, Clone)]
struct BlockState {
    block_type: String,
    started: bool,
    stopped: bool,
}

impl BlockState {
    fn new(block_type: impl Into<String>) -> Self {
        Self {
            block_type: block_type.into(),
            started: false,
            stopped: false,
        }
    }
}

/// SSE 状态管理器
///
/// 确保 SSE 事件序列符合 Claude API 规范：
/// 1. message_start 只能出现一次
/// 2. content_block 必须先 start 再 delta 再 stop
/// 3. message_delta 只能出现一次，且在所有 content_block_stop 之后
/// 4. message_stop 在最后
#[derive(Debug)]
pub struct SseStateManager {
    /// message_start 是否已发送
    message_started: bool,
    /// message_delta 是否已发送
    message_delta_sent: bool,
    /// 活跃的内容块状态
    active_blocks: HashMap<i32, BlockState>,
    /// 消息是否已结束
    message_ended: bool,
    /// 下一个块索引
    next_block_index: i32,
    /// 当前 stop_reason
    stop_reason: Option<String>,
    /// 是否有工具调用
    has_tool_use: bool,
}

impl Default for SseStateManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SseStateManager {
    pub fn new() -> Self {
        Self {
            message_started: false,
            message_delta_sent: false,
            active_blocks: HashMap::new(),
            message_ended: false,
            next_block_index: 0,
            stop_reason: None,
            has_tool_use: false,
        }
    }

    /// 判断指定块是否处于可接收 delta 的打开状态
    fn is_block_open_of_type(&self, index: i32, expected_type: &str) -> bool {
        self.active_blocks
            .get(&index)
            .is_some_and(|b| b.started && !b.stopped && b.block_type == expected_type)
    }

    /// 获取下一个块索引
    pub fn next_block_index(&mut self) -> i32 {
        let index = self.next_block_index;
        self.next_block_index += 1;
        index
    }

    /// 记录工具调用
    pub fn set_has_tool_use(&mut self, has: bool) {
        self.has_tool_use = has;
    }

    /// 设置 stop_reason
    pub fn set_stop_reason(&mut self, reason: impl Into<String>) {
        self.stop_reason = Some(reason.into());
    }

    /// 检查是否存在非 thinking 类型的内容块（如 text 或 tool_use）
    fn has_non_thinking_blocks(&self) -> bool {
        self.active_blocks
            .values()
            .any(|b| b.block_type != "thinking")
    }

    /// 获取最终的 stop_reason
    pub fn get_stop_reason(&self) -> String {
        if let Some(ref reason) = self.stop_reason {
            reason.clone()
        } else if self.has_tool_use {
            "tool_use".to_string()
        } else {
            "end_turn".to_string()
        }
    }

    /// 处理 message_start 事件
    pub fn handle_message_start(&mut self, event: serde_json::Value) -> Option<SseEvent> {
        if self.message_started {
            tracing::debug!("跳过重复的 message_start 事件");
            return None;
        }
        self.message_started = true;
        Some(SseEvent::new("message_start", event))
    }

    /// 处理 content_block_start 事件
    pub fn handle_content_block_start(
        &mut self,
        index: i32,
        block_type: &str,
        data: serde_json::Value,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 如果是 tool_use 块，先关闭之前的文本块
        if block_type == "tool_use" {
            self.has_tool_use = true;
            for (block_index, block) in self.active_blocks.iter_mut() {
                if block.block_type == "text" && block.started && !block.stopped {
                    // 自动发送 content_block_stop 关闭文本块
                    events.push(SseEvent::new(
                        "content_block_stop",
                        json!({
                            "type": "content_block_stop",
                            "index": block_index
                        }),
                    ));
                    block.stopped = true;
                }
            }
        }

        // 检查块是否已存在
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.started {
                tracing::debug!("块 {} 已启动，跳过重复的 content_block_start", index);
                return events;
            }
            block.started = true;
        } else {
            let mut block = BlockState::new(block_type);
            block.started = true;
            self.active_blocks.insert(index, block);
        }

        events.push(SseEvent::new("content_block_start", data));
        events
    }

    /// 处理 content_block_delta 事件
    pub fn handle_content_block_delta(
        &mut self,
        index: i32,
        data: serde_json::Value,
    ) -> Option<SseEvent> {
        // 确保块已启动
        if let Some(block) = self.active_blocks.get(&index) {
            if !block.started || block.stopped {
                tracing::warn!(
                    "块 {} 状态异常: started={}, stopped={}",
                    index,
                    block.started,
                    block.stopped
                );
                return None;
            }
        } else {
            // 块不存在，可能需要先创建
            tracing::warn!("收到未知块 {} 的 delta 事件", index);
            return None;
        }

        Some(SseEvent::new("content_block_delta", data))
    }

    /// 处理 content_block_stop 事件
    pub fn handle_content_block_stop(&mut self, index: i32) -> Option<SseEvent> {
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.stopped {
                tracing::debug!("块 {} 已停止，跳过重复的 content_block_stop", index);
                return None;
            }
            block.stopped = true;
            return Some(SseEvent::new(
                "content_block_stop",
                json!({
                    "type": "content_block_stop",
                    "index": index
                }),
            ));
        }
        None
    }

    /// 生成最终事件序列
    ///
    /// `input_tokens` 已是 billed 口径（剔除 cache 读写）；`cache_usage` 存在时
    /// 额外注入 cache_read / cache_creation 字段。
    pub fn generate_final_events(
        &mut self,
        input_tokens: i32,
        output_tokens: i32,
        cache_usage: Option<CacheUsageBreakdown>,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 关闭所有未关闭的块
        for (index, block) in self.active_blocks.iter_mut() {
            if block.started && !block.stopped {
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop",
                        "index": index
                    }),
                ));
                block.stopped = true;
            }
        }

        // 发送 message_delta
        if !self.message_delta_sent {
            self.message_delta_sent = true;
            let mut usage_json = json!({
                "input_tokens": input_tokens,
                "output_tokens": output_tokens
            });
            if let Some(cache_usage) = cache_usage {
                usage_json["cache_creation_input_tokens"] =
                    json!(cache_usage.cache_creation_input_tokens);
                usage_json["cache_read_input_tokens"] = json!(cache_usage.cache_read_input_tokens);
                usage_json["cache_creation"] = json!({
                    "ephemeral_5m_input_tokens": cache_usage.cache_creation_5m_input_tokens,
                    "ephemeral_1h_input_tokens": cache_usage.cache_creation_1h_input_tokens
                });
            }
            events.push(SseEvent::new(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": self.get_stop_reason(),
                        "stop_sequence": null
                    },
                    "usage": usage_json
                }),
            ));
        }

        // 发送 message_stop
        if !self.message_ended {
            self.message_ended = true;
            events.push(SseEvent::new(
                "message_stop",
                json!({ "type": "message_stop" }),
            ));
        }

        events
    }
}

use super::converter::get_context_window_size;

/// 流处理上下文
/// 一次请求解析出的最终用量快照（供用量统计埋点消费）
#[derive(Debug, Clone, Copy)]
pub struct ResolvedUsage {
    /// 输入 tokens —— **gross 口径**（含 cache 读写，优先 `contextUsageEvent` 精确值，回退估算）。
    ///
    /// ⚠ 与发给客户端的 `usage.input_tokens` 口径相反：响应体那个是 billed 口径
    /// （已经 [`billed_input_tokens`] 剔除过 cache 读写，与 `cache_read_input_tokens` 互斥），
    /// 本字段是**未剔除的全量**，落进 [`crate::usage::RequestRecord::input_tokens`] 也是 gross。
    /// 消费方算「总输入」直接用本字段，**不可再加 `cache_read_tokens`**（会把缓存计两次）。
    pub input_tokens: i32,
    /// 输出 tokens
    pub output_tokens: i32,
    /// 上游返回的真实 credit 消耗量（无 meteringEvent 时为 None）
    pub credits_used: Option<f64>,
    /// 本次命中缓存读取的 tokens（无缓存记账时为 0）。是 `input_tokens` 的子集，非增量。
    pub cache_read_tokens: i32,
    /// 本次新建缓存写入的 tokens（无缓存记账时为 0）。是 `input_tokens` 的子集，非增量。
    pub cache_creation_tokens: i32,
}

pub struct StreamContext {
    /// SSE 状态管理器
    pub state_manager: SseStateManager,
    /// 请求的模型名称
    pub model: String,
    /// 消息 ID
    pub message_id: String,
    /// 输入 tokens（估算值，未剔除 cache）
    pub input_tokens: i32,
    /// prompt 缓存记账明细（可选，注入响应 usage）
    pub cache_usage: Option<CacheUsageBreakdown>,
    /// 从 contextUsageEvent 计算的实际输入 tokens
    pub context_input_tokens: Option<i32>,
    /// 输出 tokens 累计
    pub output_tokens: i32,
    /// 从 meteringEvent 解析的真实 credit 消耗量（上游给出，token 估算无法替代）
    pub credits_used: Option<f64>,
    /// 工具块索引映射 (tool_id -> block_index)
    pub tool_block_indices: HashMap<String, i32>,
    /// 出站工具名映射 (tool_use_id -> 还原后的 Claude Code 名)。
    ///
    /// 供 `generate_final_events` 的截断兜底用：残留 `tool_input_sent`（流在 stop 前结束）
    /// 需要把 Kiro 参数还原成客户端形态，而兜底路径没有 `ToolUseEvent` 只有 id —— 必须从
    /// 这里查名字。正常 stop 路径直接拿 `tool_use.name`，不依赖本 map。
    tool_use_names: HashMap<String, String>,
    /// 每个 tool_use_id 已经转发给客户端的 input JSON 累计内容。
    ///
    /// 用于修复 `Invalid tool parameters`：Kiro 的 `ToolUseEvent.input` 在同一 tool_use_id 上
    /// **可能是累积快照**（每帧带"到目前为止的完整 JSON"）而非纯增量。若原样把每帧当
    /// `input_json_delta` 转发，Claude Code 会把累积片段再拼一次 → JSON 重复损坏 → 报错。
    /// 这里记录已发内容，转发前做前缀检测：累积则只发差量，纯增量则原样发（自适应两种上游行为）。
    tool_input_sent: HashMap<String, String>,
    /// 工具名称反向映射（短名称 → 原始名称），用于响应时还原
    pub tool_name_map: HashMap<String, String>,
    /// thinking 是否启用
    pub thinking_enabled: bool,
    /// thinking 内容缓冲区
    pub thinking_buffer: String,
    /// 是否在 thinking 块内
    pub in_thinking_block: bool,
    /// thinking 块是否已提取完成
    pub thinking_extracted: bool,
    /// thinking 块索引
    pub thinking_block_index: Option<i32>,
    /// 上游 `reasoningContentEvent` 携带的思考签名（若有）。
    ///
    /// 关闭 thinking 块时优先回传它 —— Foxfishc 实测（Round 6, 2026-05-13）：
    /// 「伪造签名不被上游识别，cache_read 仍 0」；真签名是多轮 cache 命中的关键。
    /// 若上游不发（`None`），`create_signature_delta_event` 回退占位符，行为与改动前逐字节一致。
    pub pending_reasoning_signature: Option<String>,
    /// 文本块索引（thinking 启用时动态分配）
    pub text_block_index: Option<i32>,
    /// 是否需要剥离 thinking 内容开头的换行符
    /// 模型输出 `<thinking>\n` 时，`\n` 可能与标签在同一 chunk 或下一 chunk
    strip_thinking_leading_newline: bool,
    /// DSML 标记跨 chunk 探测缓冲:保留可能是半个 DeepSeek 工具标记(如 `<｜DSML` / `<｜tool▁`)
    /// 的文本尾巴,等下一个 chunk 拼上再判定,避免标记被从中间切开导致漏字或漏标记。详见 strip_dsml_markers。
    dsml_tail_buffer: String,
    /// tail 里的残留是否**已确认为 DSML 关键字标记**(而非"不确定的正文半标记")。
    /// true=流结束 flush 时丢弃(标记噪音,不补发,否则 `<｜DSML…` 会当正文泄漏);
    /// false=flush 时作普通文本补发(被误判的正文/末尾孤立 `<`,不吞字)。
    dsml_tail_is_marker: bool,
    /// 本次响应的完成状态（收尾时据此决定 outcome / SSE error / HTTP 码）。
    /// 默认 `Ok`；in-band `Event::Error` / 传输中断 / 解码器停止时被置为对应失败态。
    completion: CompletionStatus,
    /// 是否已向客户端内联发过 SSE `error` 事件。
    /// in-band `Event::Error`（及非 max_tokens 的 Exception）会在事件流中就地补发，
    /// 收尾逻辑据此避免重复补发同一个 error 事件。
    error_event_emitted: bool,
    /// 泄漏 token 清洗（`tool_clean_leaked_tokens` 开启时）：当前文本处理位置是否在**行首/块首**。
    /// 泄漏控制 token（course/課/count/care）只出现在行首且与后文无空格粘连，故只在行首尝试剥离，
    /// 正常正文里的这些词（有空格分隔）绝不误删。初始 true（响应开头即行首），每段文本按是否以
    /// 换行结尾更新。非持久跨请求，随 StreamContext 生命周期。
    at_line_start: bool,
    /// 泄漏 token 诊断（可观测，不影响清洗剥离判据）：本请求累计真剥掉的泄漏 token 数。
    leaked_stripped: u32,
    /// 本请求检测到的 saturation 泄漏行数（整行就是纯泄漏词的行，#70544 整段退化的信号）。
    leaked_saturation_lines: u32,
    /// 本请求检测到「文本化工具调用」的 chunk 数(assistantResponseEvent 文本流里出现 <invoke/antml:/
    /// <parameter 标记)。这是决定要不要做 R4 重组层的取证依据——无条件累加(不受 KIRO_INVOKE_TRACE 限),
    /// 收尾经 recovery_metrics 暴露。
    textified_invoke_hits: u32,
    // ===== 文本化 invoke 重组(R4,移植 ZyphrZero__kiro.rs v0.6.5)=====
    /// 本次请求声明的工具名集合(=模型看到的名字)。重组硬护栏:解析出的工具名必须在此才允许捞回,
    /// 否则当文本吐——宁可漏捞不可把正文讨论的假命令误执行。
    known_tool_names: std::collections::HashSet<String>,
    /// invoke 嗅探缓冲:文本先进这里,决策安全(完整块过四道门 / 确认非泄漏)后才释放。跨 chunk 累积。
    invoke_sniff_buffer: String,
    /// 代码围栏(```)开合状态:围栏内的 <invoke> 是展示代码不捞回。跨 chunk 追踪奇偶。
    code_fence_open: bool,
    /// 围栏扫描的未完成行尾巴(等换行拼齐再判定是否围栏行)。
    fence_scan_partial: String,
    /// stray token(call/count/card/court)连续独占行复读计数,超阈值熔断本轮文本(治退化刷屏)。
    stray_repeat_last: String,
    stray_repeat_run: u32,
    /// 本请求真重组成结构化 tool_use 的次数 + stray 熔断触发次数(可观测)。
    reclaimed_invoke_count: u32,
    stray_guard_tripped: bool,
    /// stray 泄漏形态观测(纯统计不改输出):本请求见过的"独占 stray 行"数 / "句中紧贴 CJK 的 stray 词"数。
    /// 点亮句中泄漏黑洞——决定要不要开保守清洗的取证依据。收尾经 recovery_metrics 暴露。
    stray_standalone_seen: u32,
    stray_inline_seen: u32,
    /// 重组容错总开关(config tool_reclaim_textified_invoke;默认开)。关=退回纯转发(原样吐文本)。
    reclaim_enabled: bool,
    /// 首个**真实内容** delta 落定的时刻（TTFB 打点）。
    ///
    /// `None` = 本轮从未产生内容（纯错误 / 空响应）→ `first_token_ms` 落库 NULL，这是正确的。
    /// 只认 text_delta / thinking_delta / input_json_delta；`message_start` / `ping` /
    /// `content_block_start` / `signature_delta` / 关块用的空 delta 都不算首 token。
    first_token_at: Option<std::time::Instant>,
    /// 本轮是否出现过**结构化** reasoning 流（`reasoningContentEvent`）。
    ///
    /// # 为什么需要这个标志（E1 的关键约束）
    ///
    /// 结构化 reasoning 是**纯增量且没有终止帧** —— 上游不会告诉我们"思考结束了"。
    /// 真实形态是：N 帧 `reasoningContentEvent`，紧接着 `assistantResponseEvent` 携带普通正文。
    ///
    /// 而文本嗅探路径的分支是 `else if self.in_thinking_block`：一旦结构化流开过 thinking 块，
    /// 后续**不带任何标签**的正文就会落进那个分支，被当作思考内容发成 `thinking_delta`
    /// → **用户可见的答案整段消失进思考面板**，且 `has_non_thinking_blocks()` 为 false 会让
    /// 收尾把 stop_reason 置成 max_tokens、只吐一个空格文本块 = 客户端显示空答案。
    ///
    /// 所以两条路径必须**互斥**而不是共享状态：本标志置位后，首个非空正文 delta
    /// 先关掉 reasoning 开的 thinking 块，再按普通文本走 —— 见 `process_assistant_response`。
    reasoning_stream_seen: bool,
    /// 本轮是否已向客户端发出过**用户可见正文**（非空 text_delta / tool_use 的 input_json_delta）。
    ///
    /// 只用于「空响应兜底」的判据，刻意**不复用** `first_token_at`：后者把 `thinking_delta`
    /// 也算内容（TTFB 口径），拿它当"有正文"会让兜底永不触发。
    /// 判定点在 `process_kiro_event`（流式常态）与 `generate_final_events`（整轮被 hold 到
    /// 收尾才 flush 的形态）两处，缺任一处都会漏判。
    body_content_seen: bool,
    /// `!thinking_enabled` 时被丢弃的结构化 reasoning 原文（截断累积，上限
    /// [`Self::MAX_DISCARDED_REASONING_BYTES`]）。
    ///
    /// 存它**不是**为了下发给用户：正常情况（有正文）它到收尾就随 ctx 一起丢掉。
    /// 唯一用途是兜底「本轮只有 reasoning、正文为空」这一形态 —— 那时客户端会拿到
    /// **完全空的响应**，而空响应比"看到一段推理"更糟（Claude Code 侧表现为无输出且不重试）。
    discarded_reasoning: String,
}

/// 泄漏 token 剥离的命中信息（诊断计数用，不影响剥离判据）。
#[derive(Debug, Clone, Copy)]
struct StripHit {
    /// 是否真剥掉了泄漏词。
    stripped: bool,
    /// 是否为"独占整行"泄漏（saturation 信号：整行就是纯泄漏词，#70544 整段退化）。
    standalone: bool,
}

impl StripHit {
    fn none() -> Self {
        StripHit {
            stripped: false,
            standalone: false,
        }
    }
}

impl StreamContext {
    /// invoke 嗅探缓冲 hold 上限(256 KiB):行首未闭合的 <invoke 累计超此值仍没等到 </invoke>,
    /// 放弃 hold 当普通文本吐,避免永不闭合的半块把流卡死。多行参数(apply_patch)是常态,故按字节非行数。
    const MAX_INVOKE_HOLD_BYTES: usize = 262_144;

    /// `!thinking_enabled` 时丢弃的 reasoning 最多留多少字节（32 KiB）作空响应兜底素材。
    ///
    /// 有上限是必须的：这条路径**每帧都在累积一份注定不下发的文本**，无界即等于给
    /// 「上游只吐 reasoning 不吐正文」的退化响应开了一个按响应长度增长的内存放大器
    /// （同一形态的教训见已知问题 #14：`invoke_sniff_buffer` 无界持有把整条流卡死）。
    /// 32 KiB 足够承载一段可读的推理；超出部分丢弃 —— 兜底的目标是"别给空响应"，
    /// 不是"完整复现推理过程"。
    const MAX_DISCARDED_REASONING_BYTES: usize = 32 * 1024;

    /// 创建启用thinking的StreamContext
    pub fn new_with_thinking(
        model: impl Into<String>,
        input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
    ) -> Self {
        Self::new_full(
            model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
            std::collections::HashSet::new(),
        )
    }

    /// 完整构造:额外接 known_tool_names(文本化 invoke 重组的工具名硬护栏)。
    /// new_with_thinking 是它的薄封装(空工具集=不启用重组捞回,兼容既有调用/测试)。
    pub fn new_full(
        model: impl Into<String>,
        input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
        known_tool_names: std::collections::HashSet<String>,
    ) -> Self {
        Self {
            state_manager: SseStateManager::new(),
            model: model.into(),
            message_id: format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
            input_tokens,
            cache_usage: None,
            context_input_tokens: None,
            output_tokens: 0,
            credits_used: None,
            tool_block_indices: HashMap::new(),
            tool_use_names: HashMap::new(),
            tool_input_sent: HashMap::new(),
            tool_name_map,
            thinking_enabled,
            thinking_buffer: String::new(),
            in_thinking_block: false,
            thinking_extracted: false,
            thinking_block_index: None,
            pending_reasoning_signature: None,
            text_block_index: None,
            strip_thinking_leading_newline: false,
            dsml_tail_buffer: String::new(),
            dsml_tail_is_marker: false,
            completion: CompletionStatus::Ok,
            error_event_emitted: false,
            at_line_start: true,
            leaked_stripped: 0,
            leaked_saturation_lines: 0,
            textified_invoke_hits: 0,
            known_tool_names,
            invoke_sniff_buffer: String::new(),
            code_fence_open: false,
            fence_scan_partial: String::new(),
            stray_repeat_last: String::new(),
            stray_repeat_run: 0,
            reclaimed_invoke_count: 0,
            stray_guard_tripped: false,
            stray_standalone_seen: 0,
            stray_inline_seen: 0,
            reclaim_enabled: super::handlers::tool_reclaim_textified_invoke_enabled(),
            first_token_at: None,
            reasoning_stream_seen: false,
            body_content_seen: false,
            discarded_reasoning: String::new(),
        }
    }

    /// 设置 prompt 缓存记账明细（前缀估算注入；在 generate_initial_events 之前调用）
    pub fn set_cache_usage(&mut self, cache_usage: Option<CacheUsageBreakdown>) {
        self.cache_usage = cache_usage;
    }

    /// 生成 message_start 事件
    ///
    /// `input_tokens` 采用 billed 口径（剔除 cache 读写），并在有缓存记账时
    /// 注入 cache_read / cache_creation 字段。
    pub fn create_message_start_event(&self) -> serde_json::Value {
        let billed = self
            .cache_usage
            .map(|c| {
                billed_input_tokens(
                    self.input_tokens,
                    c.cache_creation_input_tokens,
                    c.cache_read_input_tokens,
                )
            })
            .unwrap_or(self.input_tokens);
        let mut usage = json!({
            "input_tokens": billed,
            "output_tokens": 1
        });
        if let Some(c) = self.cache_usage {
            usage["cache_creation_input_tokens"] = json!(c.cache_creation_input_tokens);
            usage["cache_read_input_tokens"] = json!(c.cache_read_input_tokens);
        }
        json!({
            "type": "message_start",
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": self.model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": usage
            }
        })
    }

    /// 生成初始事件序列 (message_start + 文本块 start)
    ///
    /// 当 thinking 启用时，不在初始化时创建文本块，而是等到实际收到内容时再创建。
    /// 这样可以确保 thinking 块（索引 0）在文本块（索引 1）之前。
    pub fn generate_initial_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // message_start
        let msg_start = self.create_message_start_event();
        if let Some(event) = self.state_manager.handle_message_start(msg_start) {
            events.push(event);
        }

        // 如果启用了 thinking，不在这里创建文本块
        // thinking 块和文本块会在 process_content_with_thinking 中按正确顺序创建
        if self.thinking_enabled {
            return events;
        }

        // 创建初始文本块（仅在未启用 thinking 时）
        let text_block_index = self.state_manager.next_block_index();
        self.text_block_index = Some(text_block_index);
        let text_block_events = self.state_manager.handle_content_block_start(
            text_block_index,
            "text",
            json!({
                "type": "content_block_start",
                "index": text_block_index,
                "content_block": {
                    "type": "text",
                    "text": ""
                }
            }),
        );
        events.extend(text_block_events);

        events
    }

    /// 处理 Kiro 事件并转换为 Anthropic SSE 事件。
    ///
    /// 本函数是**唯一 choke point**：流式（handlers 的 SSE 循环）与 buffered
    /// （`BufferedStreamContext::process_and_buffer`）都走它，故 TTFB 打点放这里
    /// 一处即可覆盖两条路径，而不必在 4 个 delta 构造点分别插桩（易漏）。
    pub fn process_kiro_event(&mut self, event: &Event) -> Vec<SseEvent> {
        let events = self.process_kiro_event_inner(event);
        self.mark_first_token_if_content(&events);
        self.observe_visible_body(&events);
        events
    }

    /// 若本批事件含首个**真实内容** delta，则打点（幂等：只记第一次）。
    fn mark_first_token_if_content(&mut self, events: &[SseEvent]) {
        if self.first_token_at.is_some() {
            return;
        }
        if events.iter().any(is_first_content_delta) {
            self.first_token_at = Some(std::time::Instant::now());
        }
    }

    /// 记录「本轮已发出用户可见正文」（供空响应兜底判据用，单向置位）。
    fn observe_visible_body(&mut self, events: &[SseEvent]) {
        if self.body_content_seen {
            return;
        }
        // tool_use 也算"本轮有产出"（客户端会拿它去执行工具，不是空响应）。
        // 单看 delta 不够：入参为空的 tool_use 可能一个 input_json_delta 都不发。
        if self.state_manager.has_tool_use || events.iter().any(is_visible_body_delta) {
            self.body_content_seen = true;
        }
    }

    /// 首个真实内容 delta 落定的时刻（供 handler 算 `first_token_ms`）。
    pub fn first_token_at(&self) -> Option<std::time::Instant> {
        self.first_token_at
    }

    fn process_kiro_event_inner(&mut self, event: &Event) -> Vec<SseEvent> {
        match event {
            Event::AssistantResponse(resp) => self.process_assistant_response(&resp.content),
            Event::ToolUse(tool_use) => self.process_tool_use(tool_use),
            // ⭐ E1：上游的**结构化** thinking 增量流。此前落 EventType::Unknown 被丢弃，
            // 我们转而从正文里嗅探 `<thinking>` 标签把边界猜回来（见 process_thinking_content）。
            // 现在直接用上游给的边界，不再猜。
            Event::ReasoningContent(reasoning) => self.process_reasoning_content(&reasoning),
            Event::ContextUsage(context_usage) => {
                // 从上下文使用百分比计算实际的 input_tokens
                let window_size = get_context_window_size(&self.model);
                let pct = context_usage.context_usage_percentage;

                // ⭐ 下界守卫：只有 `pct > 0 且有限` 才是**可用信号**，否则**不覆盖**已有值。
                //
                // 为什么必须有下界（原代码只判了 `>= 100.0` 上界）：
                // `ContextUsageEvent.context_usage_percentage` 带 `#[serde(default)]`，
                // 上游少发该字段 / 发 null / 发脏值时它一律是 **0.0**，而 0.0 会算出
                // `actual_input_tokens = 0` 并被无条件写进 `context_input_tokens`。
                // 那个字段是**最终计费口径的源头**：`generate_final_events` 与
                // `finish_and_get_all_events` 都走 `context_input_tokens.unwrap_or(本地估算)`，
                // 再由 `billed_input_tokens` 扣掉 cache 读写 —— 于是 input_tokens 与
                // cache 相关字段一起归零，客户端看到「这轮没吃 token」的假账。
                //
                // 为什么「上游明确说 0%」也归到脏值一侧：真实请求恒有 system prompt +
                // 至少一条 user message，占用率**不可能**为 0；且 serde 默认值与
                // 显式 0 在这里逐字节不可分辨，无法只放过其中一个。两者都当「没给」处理
                // 即可 —— 代价对称性明显：错信 0 = 计费信号错且不可逆（账已出），
                // 而丢一次真 0 只是退回本地估算（`unwrap_or` 的既有兜底），可逆。
                //
                // 负值与 NaN/inf 同理必须拦：负值会让 `billed_input_tokens` 拿到负数，
                // NaN/inf 经 `as i32` 是饱和转换（NaN→0、inf→i32::MAX），后者更糟。
                if let Some(actual_input_tokens) = context_input_tokens_from_pct(pct, window_size) {
                    self.context_input_tokens = Some(actual_input_tokens);
                    tracing::debug!(
                        "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                        pct,
                        actual_input_tokens
                    );
                } else {
                    // 不覆盖 `context_input_tokens`：保留上一次的有效值，或让下游
                    // `unwrap_or` 退回本地估算。warn 而非 debug —— 这代表上游协议异常。
                    tracing::warn!(
                        "收到无效 contextUsageEvent（{}%，非正或非有限值），忽略该信号、\
                         不覆盖已有 input_tokens（避免计费口径被归零）",
                        pct
                    );
                }

                // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded。
                // 上界判定与上面的下界守卫**互不依赖**：即便将来下界改动，这条也照旧生效。
                if pct >= 100.0 {
                    self.state_manager
                        .set_stop_reason("model_context_window_exceeded");
                }
                Vec::new()
            }
            Event::Error {
                error_code,
                error_message,
            } => {
                tracing::error!("收到 in-band 错误事件: {} - {}", error_code, error_message);
                // 记录完成状态为上游错误：收尾时据此把 outcome 记成失败、非流式返回非 200。
                // 幂等：只在首个错误落定，后续错误不覆盖（保留首因）。
                if self.completion.is_ok() {
                    self.completion = CompletionStatus::UpstreamError {
                        code: error_code.clone(),
                        message: error_message.clone(),
                    };
                }
                // in-band 错误已发生，立即内联发一个 SSE error 事件显式告知客户端
                // “本次响应未正常完成”，避免截断输出被当作 message_stop 正常收尾。
                // 标记已发，收尾路径据此不重复补发。
                self.error_event_emitted = true;
                vec![SseEvent::error_event(
                    self.completion.sse_error_type(),
                    self.completion.client_message(),
                )]
            }
            Event::Metering(metering) => {
                // 记录上游返回的真实 credit 消耗量（累加，兼容单请求多次计费事件）
                self.credits_used = Some(self.credits_used.unwrap_or(0.0) + metering.usage);
                tracing::debug!("收到 meteringEvent: {} {}", metering.usage, metering.unit);
                Vec::new()
            }
            Event::Exception {
                exception_type,
                message,
            } => {
                // 铁律：ContentLengthExceededException = max_tokens 干净收尾，绝不算失败。
                // 它是模型正常耗尽输出预算，照常走 message_stop + 200。
                if exception_type == "ContentLengthExceededException" {
                    self.state_manager.set_stop_reason("max_tokens");
                    tracing::warn!("收到 ContentLengthExceededException：按 max_tokens 干净收尾");
                    return Vec::new();
                }
                // 其它异常是上游真实失败，等同 in-band 错误处理：置失败态 + 内联发 error 事件。
                tracing::error!("收到 in-band 异常事件: {} - {}", exception_type, message);
                if self.completion.is_ok() {
                    self.completion = CompletionStatus::UpstreamError {
                        code: exception_type.clone(),
                        message: message.clone(),
                    };
                }
                self.error_event_emitted = true;
                vec![SseEvent::error_event(
                    self.completion.sse_error_type(),
                    self.completion.client_message(),
                )]
            }
            _ => Vec::new(),
        }
    }

    /// 返回本次请求已解析出的最终用量（供统计埋点使用）
    ///
    /// - `input_tokens` 优先用 contextUsageEvent 计算的精确值，回退到估算；
    ///   **gross 口径**（含 cache），与 message_start/message_delta 里发给客户端的
    ///   billed 口径同名字段刻意不同，详见 [`ResolvedUsage::input_tokens`]
    /// - `output_tokens` 为流式累计
    /// - `credits_used` 为 meteringEvent 的真实计费量（可能为 None）
    pub fn resolved_usage(&self) -> ResolvedUsage {
        ResolvedUsage {
            input_tokens: self.context_input_tokens.unwrap_or(self.input_tokens),
            output_tokens: self.output_tokens,
            credits_used: self.credits_used,
            cache_read_tokens: self
                .cache_usage
                .map(|c| c.cache_read_input_tokens)
                .unwrap_or(0),
            cache_creation_tokens: self
                .cache_usage
                .map(|c| c.cache_creation_input_tokens)
                .unwrap_or(0),
        }
    }

    /// 本次响应的完成状态（收尾时读取以决定 outcome / HTTP 码）
    pub fn completion(&self) -> &CompletionStatus {
        &self.completion
    }

    /// 用量记账应采用的最终结果分类（去掉硬编码 Success，改读真实完成状态）
    pub fn completion_outcome(&self) -> RequestOutcome {
        self.completion.outcome()
    }

    /// 是否已向客户端内联发过 SSE error 事件（收尾据此避免重复补发）
    pub fn error_event_emitted(&self) -> bool {
        self.error_event_emitted
    }

    /// 标记已向客户端发过 SSE error 事件（收尾路径手动补发后调用）
    pub fn mark_error_event_emitted(&mut self) {
        self.error_event_emitted = true;
    }

    /// 标记传输层中断（读流/读响应体 Err）：置失败态供收尾记账。
    /// 幂等：已是失败态则保留首因。
    pub fn mark_transport_error(&mut self, message: impl Into<String>) {
        if self.completion.is_ok() {
            self.completion = CompletionStatus::TransportError {
                message: message.into(),
            };
        }
    }

    /// 标记解码器永久停止（连续错误超限，响应必然截断）：置失败态供收尾记账。
    /// 幂等：已是失败态则保留首因。
    pub fn mark_decoder_stopped(&mut self, message: impl Into<String>) {
        if self.completion.is_ok() {
            self.completion = CompletionStatus::DecoderStopped {
                message: message.into(),
            };
        }
    }

    /// 剥离 DeepSeek 的工具调用协议标记(DSML 特殊 token),它们本是模型内部"要开始调工具"的
    /// 分隔符、不该出现在给用户的文本流里,但 Kiro 上游未过滤、当普通文本发下来(实测坐实:
    /// deepseek 调工具前先吐 `<｜DSML｜function_calls` / `<｜tool▁calls▁begin｜>` 家族标记,
    /// 之后才发真正的 toolUseEvent 帧)。原样透传会让客户端看到乱码标记。
    ///
    /// 标记用全角竖线 `｜`(U+FF5C)分隔,形如 `<｜DSML｜...` / `<｜tool▁calls▁begin｜>` /
    /// `<｜tool▁call▁begin｜>` / `<｜tool▁sep｜>` / `<｜tool▁call▁end｜>` 等。
    ///
    /// 跨 chunk 安全:标记可能被上游分帧从中间切开。策略——先把上轮留存的尾巴拼到本次内容前,
    /// 然后:①遇到 `<｜` 开头且已闭合 `｜>` 或后接 DSML/tool 关键字的完整标记 → 整段丢弃;
    /// ②末尾若是"半个可能的标记"(有 `<｜` 但还没闭合)→ 留到 dsml_tail_buffer 等下轮;
    /// ③其余正常文本原样输出。只对**含全角竖线的 `<｜` 序列**动手,绝不误伤正常 `<` 文本。
    /// 是否对本请求模型启用 DSML 剥离:**只对会吐 DSML 工具标记的国产模型**(deepseek/qwen/glm/
    /// minimax/kimi/moonshot 等)启用;Claude 系绝不剥离(它不产生这些标记,剥离只会误伤正文/吞字)。
    fn dsml_filter_applicable(&self) -> bool {
        let m = self.model.to_ascii_lowercase();
        // Claude 系明确排除(最主力路径,零风险优先)。
        if m.contains("claude") || m.contains("opus") || m.contains("sonnet") || m.contains("haiku")
        {
            return false;
        }
        m.contains("deepseek")
            || m.contains("qwen")
            || m.contains("glm")
            || m.contains("minimax")
            || m.contains("kimi")
            || m.contains("moonshot")
            || m.contains("deepglm") // 兜底泛化(未来国产名)
    }

    /// `<｜` 之后是否确为已知 DSML/工具协议标记关键字(白名单)。只有命中才剥离,
    /// 避免正文里合法的 `<｜…>`(CJK 排版 / 用户引用 token / 代码)被误删。
    /// DeepSeek 标记家族:`<｜DSML｜…` / `<｜tool▁calls▁begin｜>` / `<｜tool▁call▁begin｜>` /
    /// `<｜tool▁sep｜>` / `<｜tool▁call▁end｜>` / `<｜tool▁calls▁end｜>` 等,均以 `DSML`/`tool` 开头。
    fn is_dsml_keyword_after_pipe(rest: &str) -> bool {
        // rest = `<｜` 之后的内容(不含 `<｜`)。大小写不敏感前缀匹配。
        let r = rest.trim_start().to_ascii_lowercase();
        r.starts_with("dsml") || r.starts_with("tool") || r.starts_with("function")
    }

    /// DSML 尾巴缓冲的最大保留字符数:超过说明 `<｜` 后长期不闭合、大概率**不是**标记(正常正文),
    /// 应作为普通文本放行,避免无界囤积 + 静默吞正文。DeepSeek 标记都很短(<40 字符)。
    const DSML_TAIL_MAX: usize = 48;

    fn strip_dsml_markers(&mut self, content: &str) -> String {
        // 模型门控:非国产模型(尤其 Claude)完全不走剥离,原样返回,零风险零开销。
        if !self.dsml_filter_applicable() {
            return content.to_string();
        }
        // 快路径:无待处理尾巴且不含 `<`,直接返回。
        if self.dsml_tail_buffer.is_empty() && !content.contains('<') {
            return content.to_string();
        }
        let mut work = std::mem::take(&mut self.dsml_tail_buffer);
        self.dsml_tail_is_marker = false; // 取出后复位;若本轮再 hold 确认标记会重新置 true
        work.push_str(content);

        let mut out = String::with_capacity(work.len());
        let chars: Vec<char> = work.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            // 探测 DSML 标记起点:`<` 紧跟全角竖线 `｜`(U+FF5C)。
            if chars[i] == '<' && i + 1 < chars.len() && chars[i + 1] == '\u{FF5C}' {
                let rest: String = chars[i + 2..].iter().collect();
                // 白名单校验:`<｜` 后必须确为 DSML/tool/function 关键字才当标记;否则是正文,原样输出。
                // 若关键字尚不完整(rest 太短还看不出)且没闭合,则 hold 到下轮再判。
                let looks_marker = Self::is_dsml_keyword_after_pipe(&rest);
                let closed = chars[i..].iter().position(|&c| c == '>');
                if looks_marker {
                    if let Some(rel_gt) = closed {
                        i += rel_gt + 1; // 完整标记 `<｜…>` 整段丢弃
                        continue;
                    } else {
                        // 已**确认是 DSML/tool 关键字标记**但无 `>` 闭合:DeepSeek 的 `<｜DSML｜function_calls`
                        // 这类标记本就不以 `>` 收尾、以后续(转成真 toolUseEvent 帧)为界,文本流里到此即断。
                        // 它是标记噪音**不是正文**——丢弃本 chunk 从 `<｜` 起的余下全部,标记 tail 为
                        // "确认标记残留",使流结束 flush 时**丢弃而非补发**(补发会把 <｜DSML… 当正文泄漏)。
                        let held: String = chars[i..].iter().collect();
                        if held.chars().count() > Self::DSML_TAIL_MAX {
                            // 超长仍无 `>`:关键字命中大概率是误判(正文恰以 <｜tool… 开头且很长),
                            // 放行为正文避免吞掉大段合法内容(误判从宽,宁可偶尔漏个标记也不吞正文)。
                            out.push_str(&held);
                        } else {
                            self.dsml_tail_buffer = held;
                            self.dsml_tail_is_marker = true;
                        }
                        return out;
                    }
                } else {
                    // `<｜` 后不是关键字:可能是(a)正文里合法 `<｜…>`→原样输出这个 `<`,继续扫;
                    // (b)关键字还没到齐(rest 短且未闭合)→ hold 等下轮确认。
                    let undecided = closed.is_none() && rest.chars().count() < 8;
                    if undecided {
                        let held: String = chars[i..].iter().collect();
                        if held.chars().count() <= Self::DSML_TAIL_MAX {
                            self.dsml_tail_buffer = held;
                            return out;
                        }
                        // 超长:放行为正文
                    }
                    // 确定不是标记:原样输出 `<` 后继续(不跳过后续内容)。
                    out.push(chars[i]);
                    i += 1;
                    continue;
                }
            }
            out.push(chars[i]);
            i += 1;
        }
        // 边界:输出末尾孤立 `<`(标记 `<｜` 可能被从 `<` 与 `｜` 间切开)。hold 等下轮拼判。
        if out.ends_with('<') {
            out.pop();
            self.dsml_tail_buffer.push('<');
        }
        out
    }

    /// 流结束时把 DSML 尾巴缓冲里的残留作为**普通文本**补发,避免末尾孤立 `<`/未闭合半标记被静默吞掉。
    /// 收尾路径(generate_final_events/finish)调用。返回残留文本(空则无事)。
    pub fn flush_dsml_tail(&mut self) -> Vec<SseEvent> {
        if self.dsml_tail_buffer.is_empty() {
            return Vec::new();
        }
        let leftover = std::mem::take(&mut self.dsml_tail_buffer);
        let was_marker = self.dsml_tail_is_marker;
        self.dsml_tail_is_marker = false;
        if was_marker {
            // 已确认是 DSML 关键字标记的残留(如 `<｜DSML｜function_calls` 无 `>` 收尾):丢弃,
            // 绝不补发——补发会把标记当正文泄漏给客户端(这正是之前实测漏的那条)。
            return Vec::new();
        }
        // 否则是被误判为半标记的正文(或流在 `<` 后截断):按普通文本发出,不吞字。
        self.create_text_delta_events(&leftover)
    }

    /// 处理助手响应事件
    /// 已知的模型泄漏控制/规划 token（#70544 高多字节密度紧邻工具标签时,模型把内部规划 token
    /// 当可见文本吐出,以行首粘连形式漏进输出）。真实日志实测最高频是 `court`(独占整行 202 次),
    /// 及 course/count/care/card/call 粘 CJK。**注意:此现象发生在 Claude/opus 侧,故清洗必须对
    /// Claude 生效**(不能像 DSML 那样门控排除 Claude,否则正好漏掉主战场)。
    /// 删除死条目 `coursecount`(被 `course` 前缀遮蔽,strip_prefix 顺序遍历永不可达)。
    const LEAKED_CONTROL_TOKENS: &'static [&'static str] = &[
        "court", "course", "count", "care", "card", "call", "課", "课",
    ];

    /// 判断字符是否为「泄漏粘连」信号:CJK 表意文字 或 全角标点/字符(U+3000..U+303F、U+FF00..U+FFEF、
    /// U+4E00..U+9FFF 等)。**收严关键**:此前用「非空格非小写即剥」过宽,把 `count: 42`(冒号)、
    /// `countDown()`(大写)、`care2share`(数字)这类**正常英文**行首误删。现在只认 CJK/全角——
    /// 正常英文的 ASCII 冒号/数字/大写字母一律不触发,杜绝对 Claude 正文的误删。
    fn is_leak_glue_char(c: char) -> bool {
        let u = c as u32;
        // CJK 统一表意 + 扩展A + 兼容 + 全角/半角形式 + CJK 标点。
        (0x4E00..=0x9FFF).contains(&u)      // CJK 统一表意
            || (0x3400..=0x4DBF).contains(&u) // CJK 扩展 A
            || (0x3000..=0x303F).contains(&u) // CJK 标点(、。「」等)
            || (0xFF00..=0xFFEF).contains(&u) // 全角 ASCII + 全角标点(：！？，等)
            || (0x2E80..=0x2EFF).contains(&u) // CJK 部首补充
    }

    /// 保守清洗行首泄漏的控制 token（`tool_clean_leaked_tokens` 开启时）。
    ///
    /// 只处理**行首**：若文本（在行首位置）以某个已知泄漏词开头，且该词紧邻的下一个字符是
    /// 非 ASCII 字母/非空格（CJK、全角标点、冒号、大写字母跳变等跨类粘连），判定为泄漏并剥掉该词。
    /// 返回清洗后的文本。误删防护：词后是空格或普通 ASCII 小写延续（如 `counter`/`careful`）→ 不剥。
    fn clean_leaked_tokens(&mut self, content: &str) -> String {
        if !self.at_line_start {
            return content.to_string();
        }
        // 逐行处理：只对每行的行首做一次判定（保守，不递归剥多层）。
        let mut out = String::with_capacity(content.len());
        for (i, line) in content.split_inclusive('\n').enumerate() {
            // split_inclusive 保留了行尾 \n；首段是否真在行首由 at_line_start 保证（i==0）
            // 或前一段以 \n 结尾（i>0 必然行首）。
            let is_line_start = i > 0 || self.at_line_start;
            if is_line_start {
                // 诊断（可观测，不改剥离判据）：strip 返回是否命中 + 是否为独占整行泄漏，累加计数。
                let (cleaned, hit) = Self::strip_leaked_prefix(line);
                if hit.stripped {
                    self.leaked_stripped += 1;
                }
                if hit.standalone {
                    self.leaked_saturation_lines += 1;
                }
                out.push_str(&cleaned);
            } else {
                out.push_str(line);
            }
        }
        out
    }

    /// 独占整行即视为泄漏的高置信 token:这几个词在正常英文里**极少独占一整行**,
    /// 但在 #70544 泄漏里恰恰大量独占行(court 实测 202 次全独占行)。仅这几个可"整行即剥",
    /// call/card/count/care/course 独占行可能是正常内容(标题/变量/列表),**不**享此特例。
    const LEAK_STANDALONE_TOKENS: &'static [&'static str] = &["court", "課", "课"];

    /// 剥掉单行行首的一个泄漏词（若命中粘连特征）。返回 (清洗后文本, 命中信息)。
    /// **剥离判据完全不变**（0.7.14 已收严），只额外返回命中标志供诊断计数用。
    fn strip_leaked_prefix(line: &str) -> (String, StripHit) {
        // 先处理"独占整行"特例:整行(去掉行尾 \n / 空白后)恰等于某高置信 token → 泄漏,整行剥空。
        let trimmed = line.trim_end_matches(['\n', '\r', ' ', '\t']);
        for &tok in Self::LEAK_STANDALONE_TOKENS {
            if trimmed == tok {
                // 保留行尾换行(维持行结构),只把词本身剥掉。standalone=true(saturation 信号)。
                return (
                    line[tok.len()..].to_string(),
                    StripHit {
                        stripped: true,
                        standalone: true,
                    },
                );
            }
        }
        for &tok in Self::LEAKED_CONTROL_TOKENS {
            if let Some(rest) = line.strip_prefix(tok) {
                // 判定粘连：rest 的首字符必须是 **CJK / 全角** 粘连信号,才算泄漏(收严:
                // 排除 ASCII 冒号/数字/大写等——那些是正常英文,误删 count:42 / countDown())。
                match rest.chars().next() {
                    None => return (line.to_string(), StripHit::none()), // 行尾就是这个词→ 保守不剥。
                    Some(c) => {
                        if Self::is_leak_glue_char(c) {
                            return (
                                rest.to_string(),
                                StripHit {
                                    stripped: true,
                                    standalone: false,
                                },
                            ); // CJK/全角粘连 → 剥。
                        }
                        return (line.to_string(), StripHit::none()); // 其余→ 正常英文,不剥。
                    }
                }
            }
        }
        (line.to_string(), StripHit::none())
    }

    fn process_assistant_response(&mut self, content: &str) -> Vec<SseEvent> {
        // 【诊断探针·KIRO_INVOKE_TRACE】坐实「文本化工具调用」现象(#70544 变体):Claude 系模型
        // 偶发把工具调用语法当纯文本吐进 assistantResponseEvent(丢 antml: 前缀 + 夹 court 泄漏词),
        // 客户端拿到 <invoke.../> 文本解析不了直接断连。此探针在文本流里出现工具调用标记时如实记一条
        // (含现场文本片段),用于抓真实语料定性——上游到底走文本流还是 toolUseEvent。平时零开销。
        if contains_textified_tool_call(content) {
            // 无条件计数(取证:决定是否值得做 R4 文本化重组层)——不受 KIRO_INVOKE_TRACE 限。
            self.textified_invoke_hits += 1;
            crate::common::recovery_metrics::bump_textified_invoke();
            // 详细现场语料仅在探针开启时打(含文本片段,量大)。
            if invoke_trace_enabled() {
                let snippet: String = content.chars().take(200).collect();
                tracing::warn!(
                    target: "kiro::invoke_trace",
                    model = %self.model,
                    "[invoke_trace] assistantResponseEvent 文本流出现工具调用标记(疑似文本化 invoke 泄漏): {:?}",
                    snippet
                );
            }
        }
        // 【诊断探针·stray 泄漏形态观测】纯统计不改输出,零误删风险。目的:点亮"句中/独占 stray 词"黑洞——
        // 现有 leakedCleaned 只在**行首**清洗命中才计数,句中泄漏(如 `重读course了`)完全静默穿透、
        // 连计数都没进,导致线上全 0 无法区分"没泄漏"还是"泄漏了没检测到"。这里在**清洗前**扫原始
        // content,按形态分类计数(独占 stray 行 / 句中紧贴 CJK 的 stray 词),供运维页看真机泄漏形态,
        // 再据此决定要不要开保守清洗。开销:仅在 content 含已知 stray 词时才细扫(快路径先 contains)。
        observe_stray_leak_forms(
            content,
            &mut self.stray_standalone_seen,
            &mut self.stray_inline_seen,
        );
        // 先剥离 DeepSeek DSML 工具协议标记(跨 chunk 安全),再走后续文本处理。
        let cleaned = self.strip_dsml_markers(content);
        // 泄漏控制 token 清洗（开关默认 true，见 handlers::tool_clean_leaked_tokens_enabled）：
        // 仅行首粘连特征命中才剥，误删风险极低。
        let cleaned = if super::handlers::tool_clean_leaked_tokens_enabled() {
            self.clean_leaked_tokens(&cleaned)
        } else {
            cleaned
        };
        // 更新行首标志：本段非空时，按是否以换行结尾决定下段起点是否在行首。
        if !cleaned.is_empty() {
            self.at_line_start = cleaned.ends_with('\n');
        }
        // stray 复读熔断:**所有路径的公共入口**(thinking / 无工具 / reclaim 都在此之后),
        // 治 Opus 退化刷屏(课/course/任意短词连写或独占行)。脱离 reclaim 路径独立生效——
        // 这修了审计发现的两个 HIGH 盲区:①thinking 提前 return 绕过 ②无工具请求绕过。
        // 熔断已 tripped → 返回空丢弃剩余;截断 → 只保留阈值前文本。
        let guarded = self.stray_guard_filter(&cleaned).into_owned();
        if guarded.is_empty() {
            return Vec::new();
        }
        let content = guarded.as_str();

        // 估算 tokens
        self.output_tokens += estimate_tokens(content);

        // 如果启用了thinking，需要处理thinking块
        if self.thinking_enabled {
            // ⭐ E1 关键约束：**结构化 reasoning 流与文本嗅探必须互斥**。
            //
            // 结构化 reasoning 无终止帧，真实形态是「N 帧 reasoning → 普通正文（无标签）」。
            // 而嗅探路径的分支是 `else if self.in_thinking_block`：若 reasoning 已开过 thinking 块，
            // 这段不带标签的正文会被当思考内容发成 thinking_delta →
            // **用户可见答案整段消失进思考面板**，且 has_non_thinking_blocks()=false 会让收尾
            // 把 stop_reason 置 max_tokens、只吐一个空格文本块 = 客户端显示空答案。
            //
            // 所以在这里先关掉 reasoning 开的块：首个非空正文 delta 即视为「思考结束」，
            // 之后正文照常走 text_delta。thinking_extracted 置位使嗅探路径也不再重新开块。
            if self.reasoning_stream_seen && self.in_thinking_block {
                let mut events = self.close_reasoning_thinking_block();
                events.extend(self.process_content_with_thinking(content));
                return events;
            }
            return self.process_content_with_thinking(content);
        }

        // 客户端没声明 thinking，但模型仍可能吐内联 `<thinking>` 标签 —— 剥掉，不当正文下发。
        // 口径与 `process_reasoning_content`(结构化帧在 !thinking_enabled 时直接丢弃)对齐，
        // 详见 `strip_inline_thinking_when_disabled` 的文档注释。
        // 放在 reclaim 之前:思考内容里可能含 `<invoke>` 样文本(模型在"想"要调什么工具),
        // 若先进 sniff 缓冲就会把思考里的假 invoke 重组成真 tool_use —— 那是凭空执行工具。
        let stripped = self.strip_inline_thinking_when_disabled(content);
        if stripped.is_empty() {
            return Vec::new();
        }
        let content = stripped.as_str();

        // 文本化 invoke 重组(开关开且本次请求带了工具):文本先进 sniff 缓冲,决策安全后才释放
        // (完整块过四道门重组 / 半块 hold 等闭合 / 非泄漏当文本)。开关关或无声明工具则走原路径。
        if self.reclaim_enabled && !self.known_tool_names.is_empty() {
            self.invoke_sniff_buffer.push_str(content);
            return self.drain_invoke_sniff_buffer(false);
        }

        // 非 thinking 模式同样复用统一的 text_delta 发送逻辑，
        // 以便在 tool_use 自动关闭文本块后能够自愈重建新的文本块，避免“吞字”。
        self.create_text_delta_events(content)
    }

    /// 客户端**没有**声明 thinking 时，把内联 `<thinking>…</thinking>` 从正文里剥掉。
    ///
    /// # 为什么必须做
    ///
    /// `thinking_enabled` 取自客户端请求体（`handlers.rs` 的 `payload.thinking.is_enabled()`），
    /// 而模型是否吐 `<thinking>` 标签**与客户端要不要无关** —— 它可能照吐。
    ///
    /// 而本仓对同一种内容已有明确口径：`process_reasoning_content` 在
    /// `!thinking_enabled` 时 **直接丢弃整帧**（"客户端没要就不给"）。内联标签若原样穿透，
    /// 就变成「结构化帧丢弃、内联标签泄漏」——同一种内容两套处置，模型的内部推理
    /// 被当正文吐给用户。这就是缺陷本体，本函数把口径对齐。
    ///
    /// # 状态复用是安全的
    ///
    /// 非 thinking 模式下 `thinking_buffer` / `in_thinking_block` 这两个字段**无人使用**
    /// （`process_content_with_thinking` 是唯一读写方，而它只在 `thinking_enabled` 时被调）。
    /// 故此处复用它们承载跨 chunk 状态，不新增字段、不与 thinking 路径互相干扰。
    ///
    /// # 与 thinking 路径共用同一套标签判据
    ///
    /// 起止标签都走 `find_real_thinking_start_tag` / `find_real_thinking_end_tag`
    /// （跳过被反引号包裹的、要求结束标签后跟 `\n\n`），**刻意不新写一套匹配**：
    /// 两套判据必然漂移，而漂移的后果是"某种形态在一条路径被剥、在另一条泄漏"。
    ///
    /// 尾部保留 `"</thinking>\n\n".len()`：标签可能跨 chunk 断开，过早放行会把半个
    /// 结束标签当正文吐出去。
    fn strip_inline_thinking_when_disabled(&mut self, content: &str) -> String {
        self.thinking_buffer.push_str(content);
        let mut visible = String::new();
        loop {
            if self.in_thinking_block {
                match find_real_thinking_end_tag(&self.thinking_buffer) {
                    Some(m) => {
                        // 丢弃思考内容本体 + 结束标签 + 其后的段落分隔换行（长度按实际形态算，
                        // 不能写死 13 —— 详见 `thinking_end_tag_consumed_len`）。
                        let cut =
                            m.start + thinking_end_tag_consumed_len(&self.thinking_buffer, &m);
                        self.thinking_buffer = self.thinking_buffer[cut..].to_string();
                        self.in_thinking_block = false;
                    }
                    None => {
                        // 严格判据不认，但可能是「等也没用」的形态（`</thinking>Answer`：
                        // 标签后的普通字符已就位，后续 chunk 改不了它）。那种形态若按下面
                        // 「整段丢弃」处理，答案会被**永久丢弃**且无痕 —— 见
                        // `find_permanently_unsatisfiable_end_tag`。
                        if let Some(m) =
                            find_permanently_unsatisfiable_end_tag(&self.thinking_buffer)
                        {
                            let cut =
                                m.start + thinking_end_tag_consumed_len(&self.thinking_buffer, &m);
                            self.thinking_buffer = self.thinking_buffer[cut..].to_string();
                            self.in_thinking_block = false;
                            continue;
                        }
                        // 结束标签未到：整段都还是思考内容（全部丢弃），只保留**真的可能是**
                        // 半个 `</thinking>` 的尾巴等下一个 chunk。
                        let keep = partial_thinking_tag_suffix_len(&self.thinking_buffer);
                        let cut = self.thinking_buffer.len() - keep;
                        self.thinking_buffer = self.thinking_buffer[cut..].to_string();
                        break;
                    }
                }
            } else {
                let open = find_real_thinking_start_tag(&self.thinking_buffer);
                // 孤立闭标签若排在开标签之前，必须先剥它 —— 否则它会被当成
                // 「开标签之前的正文」原样下发（实测泄漏形态①②）。
                let stray = find_stray_thinking_end_tag(&self.thinking_buffer)
                    .filter(|s| open.is_none_or(|o| s.start < o.start));
                if let Some(s) = stray {
                    visible.push_str(&self.thinking_buffer[..s.start]);
                    let cut = s.start + thinking_end_tag_consumed_len(&self.thinking_buffer, &s);
                    self.thinking_buffer = self.thinking_buffer[cut..].to_string();
                    continue;
                }
                match open {
                    Some(m) => {
                        visible.push_str(&self.thinking_buffer[..m.start]);
                        self.thinking_buffer = self.thinking_buffer[m.end()..].to_string();
                        self.in_thinking_block = true;
                    }
                    None => {
                        // 无起始标签：除**真的可能是**标签的尾巴外全部可见。
                        //
                        // ⚠️ 这里绝不能无条件扣 `"<thinking>".len()`=10 字节：`</invoke>` 只有
                        // 9 字节，会被整条扣住 → 下游重组层永远看不到闭合标签 → 工具不执行；
                        // 而 10 字节又盖不住 11 字节的 `</thinking>` ⇒ 孤立闭标签穿透。
                        // 两头都由 `partial_thinking_tag_suffix_len` 按标签语法判定。
                        let keep = partial_thinking_tag_suffix_len(&self.thinking_buffer);
                        let cut = self.thinking_buffer.len() - keep;
                        visible.push_str(&self.thinking_buffer[..cut]);
                        self.thinking_buffer = self.thinking_buffer[cut..].to_string();
                        break;
                    }
                }
            }
        }
        visible
    }

    /// 处理包含thinking块的内容
    fn process_content_with_thinking(&mut self, content: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 将内容添加到缓冲区进行处理
        self.thinking_buffer.push_str(content);

        loop {
            if !self.in_thinking_block && !self.thinking_extracted {
                let open_tag = find_real_thinking_start_tag(&self.thinking_buffer);
                // 孤立闭标签（没有配对开标签）若排在开标签之前，必须先剥掉：否则它会被
                // 当成「开标签之前的正文」原样进 text_delta（实测泄漏形态①②）。
                // 两侧都是真正文，故只丢标签本身、内容全留。
                if let Some(s) = find_stray_thinking_end_tag(&self.thinking_buffer)
                    .filter(|s| open_tag.is_none_or(|o| s.start < o.start))
                {
                    let before = self.thinking_buffer[..s.start].to_string();
                    let cut = s.start + thinking_end_tag_consumed_len(&self.thinking_buffer, &s);
                    self.thinking_buffer = self.thinking_buffer[cut..].to_string();
                    // 与下面开标签分支同一口径：纯空白前缀不下发，避免 thinking 块之前
                    // 凭空多一个 text 块（会让客户端看到「新块 start → 旧块 stop」交错）。
                    if !before.trim().is_empty() {
                        events.extend(self.emit_non_thinking_text(&before));
                    }
                    continue;
                }
                // 查找 <thinking> 开始标签（跳过被反引号包裹的）
                if let Some(open) = open_tag {
                    // 发送 <thinking> 之前的内容作为 text_delta
                    // 注意：如果前面只是空白字符（如 adaptive 模式返回的 \n\n），则跳过，
                    // 避免在 thinking 块之前产生无意义的 text 块导致客户端解析失败
                    let before_thinking = self.thinking_buffer[..open.start].to_string();
                    if !before_thinking.is_empty() && !before_thinking.trim().is_empty() {
                        events.extend(self.emit_non_thinking_text(&before_thinking));
                    }

                    // 进入 thinking 块
                    self.in_thinking_block = true;
                    self.strip_thinking_leading_newline = true;
                    self.thinking_buffer = self.thinking_buffer[open.end()..].to_string();

                    // 创建 thinking 块的 content_block_start 事件
                    let thinking_index = self.state_manager.next_block_index();
                    self.thinking_block_index = Some(thinking_index);
                    let start_events = self.state_manager.handle_content_block_start(
                        thinking_index,
                        "thinking",
                        json!({
                            "type": "content_block_start",
                            "index": thinking_index,
                            "content_block": {
                                "type": "thinking",
                                "thinking": ""
                            }
                        }),
                    );
                    events.extend(start_events);
                } else {
                    // 没有找到 <thinking>，检查是否可能是部分标签
                    //
                    // ⚠️ 这里原本无条件扣 `"<thinking>".len()` = **10 字节**，而
                    // `</thinking>` 是 **11 字节** ⇒ 孤立闭标签整条穿透进可见正文
                    // （实测泄漏形态①②的本体）。改为按标签语法判定实际可能的尾巴长度：
                    // 不可能是标签的散文尾巴扣留 0（首字节少等一个 chunk），而
                    // `</thinki`、`<thinking fo` 这类半标签一律扣住。
                    let keep = partial_thinking_tag_suffix_len(&self.thinking_buffer);
                    let safe_len = self.thinking_buffer.len() - keep;
                    if safe_len > 0 {
                        let safe_content = self.thinking_buffer[..safe_len].to_string();
                        // 如果 thinking 尚未提取，且安全内容只是空白字符，
                        // 则不发送为 text_delta，继续保留在缓冲区等待更多内容。
                        // 这避免了 4.6 模型中 <thinking> 标签跨事件分割时，
                        // 前导空白（如 "\n\n"）被错误地创建为 text 块，
                        // 导致 text 块先于 thinking 块出现的问题。
                        if !safe_content.is_empty() && !safe_content.trim().is_empty() {
                            events.extend(self.emit_non_thinking_text(&safe_content));
                            self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                        } else if self.thinking_buffer.len() > MAX_THINKING_BUFFER_BYTES {
                            // review Finding 5 修复:上游若持续吐纯空白(无 <thinking>),纯空白分支
                            // 既不 emit 也不收缩 → thinking_buffer 无界增长 OOM(远程 DoS)。
                            // 超上限时把纯空白安全内容按普通文本吐出并收缩,只保留可能的半标签尾巴。
                            events.extend(self.emit_non_thinking_text(&safe_content));
                            self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                        }
                    }
                    break;
                }
            } else if self.in_thinking_block {
                // 剥离 <thinking> 标签后紧跟的换行符（可能跨 chunk）
                if self.strip_thinking_leading_newline {
                    if self.thinking_buffer.starts_with('\n') {
                        self.thinking_buffer = self.thinking_buffer[1..].to_string();
                        self.strip_thinking_leading_newline = false;
                    } else if !self.thinking_buffer.is_empty() {
                        // buffer 非空但不以 \n 开头，不再需要剥离
                        self.strip_thinking_leading_newline = false;
                    }
                    // buffer 为空时保留标志，等待下一个 chunk
                }

                // 在 thinking 块内，查找 </thinking> 结束标签（跳过被反引号包裹的）
                if let Some(end_m) = find_real_thinking_end_tag(&self.thinking_buffer) {
                    // 提取 thinking 内容
                    let thinking_content = self.thinking_buffer[..end_m.start].to_string();
                    if !thinking_content.is_empty() {
                        if let Some(thinking_index) = self.thinking_block_index {
                            events.push(
                                self.create_thinking_delta_event(thinking_index, &thinking_content),
                            );
                        }
                    }

                    // 结束 thinking 块
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;

                    // 发送空的 thinking_delta 事件，然后发送 content_block_stop 事件
                    if let Some(thinking_index) = self.thinking_block_index {
                        // 先发送空的 thinking_delta
                        events.push(self.create_thinking_delta_event(thinking_index, ""));
                        // 再发送 signature_delta（满足客户端 thinking 模式本地校验）
                        events.push(self.create_signature_delta_event(thinking_index));
                        // 最后发送 content_block_stop
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }

                    // 剥离结束标签 + 其后的段落分隔换行（长度按实际形态算，见
                    // `thinking_end_tag_consumed_len`：后缀可能是 `\n\n` / `\n` / 空格 / `<`）
                    let cut =
                        end_m.start + thinking_end_tag_consumed_len(&self.thinking_buffer, &end_m);
                    self.thinking_buffer = self.thinking_buffer[cut..].to_string();
                } else {
                    // 没有找到结束标签，发送当前缓冲区内容作为 thinking_delta。
                    // 保留末尾可能是（半个或整个）结束标签的内容：
                    // find_real_thinking_end_tag 要求标签后跟含换行的空白才返回 Some，
                    // 因此 `</thinking>` 已到、`\n\n` 未到时也必须继续扣住，
                    // 否则标签字面量会被当作 thinking 内容发出。
                    //
                    // 长度**不能写死** `"</thinking>\n\n".len()`=13：带属性/带空白的闭标签
                    // （`</thinking >`）比它长，扣不住就漏；改为按标签语法判定。
                    let keep = partial_thinking_tag_suffix_len(&self.thinking_buffer);
                    let safe_len = self.thinking_buffer.len() - keep;
                    if safe_len > 0 {
                        let safe_content = self.thinking_buffer[..safe_len].to_string();
                        if !safe_content.is_empty() {
                            if let Some(thinking_index) = self.thinking_block_index {
                                events.push(
                                    self.create_thinking_delta_event(thinking_index, &safe_content),
                                );
                            }
                        }
                        self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                    }
                    break;
                }
            } else {
                // thinking 已提取完成，剩余内容作为 text_delta。
                // 这里曾是孤立闭标签的旁路：主循环剥了，本分支照样原样倒给客户端
                // （`<thinking>a</thinking>\n\n正文</thinking>尾` 这种二次闭标签）。
                if !self.thinking_buffer.is_empty() {
                    let remaining = self.thinking_buffer.clone();
                    self.thinking_buffer.clear();
                    let remaining = strip_stray_thinking_end_tags(&remaining);
                    if !remaining.is_empty() {
                        events.extend(self.emit_non_thinking_text(&remaining));
                    }
                }
                break;
            }
        }

        events
    }

    /// 非 thinking 文本的统一出口：当 reclaim 开关开且请求带工具时，
    /// 先进 invoke_sniff_buffer（与非 thinking 路径保持一致），否则直接发 text_delta。
    /// 这修复了 thinking 模式下 process_content_with_thinking 内部各文本分支
    /// 直接调用 create_text_delta_events 导致的 invoke_sniff_buffer 旁路 bug。
    fn emit_non_thinking_text(&mut self, text: &str) -> Vec<SseEvent> {
        if self.reclaim_enabled && !self.known_tool_names.is_empty() {
            self.invoke_sniff_buffer.push_str(text);
            self.drain_invoke_sniff_buffer(false)
        } else {
            self.create_text_delta_events(text)
        }
    }

    /// 创建 text_delta 事件
    ///
    /// 如果文本块尚未创建，会先创建文本块。
    /// 当发生 tool_use 时，状态机会自动关闭当前文本块；后续文本会自动创建新的文本块继续输出。
    ///
    /// 返回值包含可能的 content_block_start 事件和 content_block_delta 事件。
    fn create_text_delta_events(&mut self, text: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 如果当前 text_block_index 指向的块已经被关闭（例如 tool_use 开始时自动 stop），
        // 则丢弃该索引并创建新的文本块继续输出，避免 delta 被状态机拒绝导致“吞字”。
        if let Some(idx) = self.text_block_index {
            if !self.state_manager.is_block_open_of_type(idx, "text") {
                self.text_block_index = None;
            }
        }

        // 获取或创建文本块索引
        let text_index = if let Some(idx) = self.text_block_index {
            idx
        } else {
            // 文本块尚未创建，需要先创建
            let idx = self.state_manager.next_block_index();
            self.text_block_index = Some(idx);

            // 发送 content_block_start 事件
            let start_events = self.state_manager.handle_content_block_start(
                idx,
                "text",
                json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": {
                        "type": "text",
                        "text": ""
                    }
                }),
            );
            events.extend(start_events);
            idx
        };

        // 发送 content_block_delta 事件
        if let Some(delta_event) = self.state_manager.handle_content_block_delta(
            text_index,
            json!({
                "type": "content_block_delta",
                "index": text_index,
                "delta": {
                    "type": "text_delta",
                    "text": text
                }
            }),
        ) {
            events.push(delta_event);
        }

        events
    }

    // ===== 文本化 invoke 重组(R4,移植 ZyphrZero__kiro.rs v0.6.5)=====

    /// 把重组出的 (工具名, input_json) 合成为标准结构化 tool_use 的 6 步 SSE
    /// (content_block_start type:tool_use → input_json_delta → content_block_stop)。
    /// set_has_tool_use(true) → get_stop_reason 自然返回 tool_use(不用 borrow-retry,就地修复)。
    /// 工具名经 tool_name_map 还原(超长名缩短过的还原回客户端原名)。
    fn synthesize_tool_use(&mut self, parsed_name: String, input_json: String) -> Vec<SseEvent> {
        let mut events = Vec::new();
        self.state_manager.set_has_tool_use(true);
        self.reclaimed_invoke_count += 1;
        crate::common::recovery_metrics::bump_reclaimed_invoke();
        let block_index = self.state_manager.next_block_index();
        let tool_use_id = format!("toolu_{}", Uuid::new_v4().to_string().replace('-', ""));
        self.tool_block_indices
            .insert(tool_use_id.clone(), block_index);
        let name = self
            .tool_name_map
            .get(&parsed_name)
            .cloned()
            .unwrap_or(parsed_name);
        events.extend(self.state_manager.handle_content_block_start(
            block_index,
            "tool_use",
            json!({
                "type": "content_block_start",
                "index": block_index,
                "content_block": { "type": "tool_use", "id": tool_use_id, "name": name, "input": {} }
            }),
        ));
        if let Some(d) = self.state_manager.handle_content_block_delta(
            block_index,
            json!({
                "type": "content_block_delta",
                "index": block_index,
                "delta": { "type": "input_json_delta", "partial_json": input_json }
            }),
        ) {
            events.push(d);
        }
        if let Some(s) = self.state_manager.handle_content_block_stop(block_index) {
            events.push(s);
        }
        events
    }

    /// stray token 复读熔断:对即将作为文本吐出的内容,检测 call/count/card/court 连续独占行复读。
    /// 跨 chunk 维护 (stray_repeat_last, stray_repeat_run);超阈值后本请求剩余文本全丢(熔断已 tripped)。
    /// 返回截断后可安全吐出的文本(熔断已触发则返回空)。开关关或已 tripped 走各自快路径。
    fn stray_guard_filter<'a>(&mut self, text: &'a str) -> std::borrow::Cow<'a, str> {
        if self.stray_guard_tripped {
            return std::borrow::Cow::Borrowed("");
        }
        if !super::handlers::tool_stray_repeat_guard_enabled() {
            return std::borrow::Cow::Borrowed(text);
        }
        // 两条独立的复读检测,取先命中的截断点(取 min):
        // ① 逐行独占:同一 stray 行连续重复(跨 chunk 维护 run),覆盖 "课\n课\n课\n…" 形态。
        // ② 结构性签名:任意"短 token"(≤6 字符、纯字母或纯 CJK、无空格标点)连续重复 ≥阈值——
        //    **不依赖硬编码词表、不依赖换行**,覆盖 "课课课…"(单行连写)/"coursecourse…"/未来任何新退化词。
        //    这是治本:硬编码词表(course 都漏过)+ 独占行精确匹配(单行连写漏)双盲区的通用兜底。
        let line_cut = self.detect_stray_line_repeat(text);
        let sig_cut = detect_structural_flood(text);
        let cut_at = match (line_cut, sig_cut) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        match cut_at {
            Some(pos) => {
                self.stray_guard_tripped = true;
                crate::common::recovery_metrics::bump_stray_guard_tripped();
                tracing::warn!(target: "kiro::invoke_trace", model = %self.model,
                    "[invoke_reclaim] stray token 复读超阈值({}),熔断本轮剩余文本", REPEAT_GUARD_TRIP_THRESHOLD);
                std::borrow::Cow::Owned(text[..pos].to_string())
            }
            None => std::borrow::Cow::Borrowed(text),
        }
    }

    /// ① 逐行独占 stray 词复读检测(跨 chunk 维护 stray_repeat_last/run,保留已知词提前介入)。
    /// 返回截断字节偏移(命中阈值)或 None。
    fn detect_stray_line_repeat(&mut self, text: &str) -> Option<usize> {
        let mut offset = 0usize;
        for segment in text.split_inclusive('\n') {
            let line = segment.trim();
            if !line.is_empty()
                && (STRAY_INVOKE_TOKENS.contains(&line) || is_short_flood_token(line))
            {
                if line == self.stray_repeat_last {
                    self.stray_repeat_run += 1;
                } else {
                    self.stray_repeat_last = line.to_string();
                    self.stray_repeat_run = 1;
                }
                if self.stray_repeat_run >= REPEAT_GUARD_TRIP_THRESHOLD {
                    return Some(offset);
                }
            } else if !line.is_empty() {
                self.stray_repeat_last = line.to_string();
                self.stray_repeat_run = 0;
            }
            offset += segment.len();
        }
        None
    }

    /// 重组路径里"当文本吐"的统一出口。stray 熔断已在 process_assistant_response 顶层对全部入站文本
    /// 统一执行过(进 sniff 缓冲的内容都已过滤),这里直接裸发,**不再重复跑有状态的 guard**
    /// (重复跑会二次累加 stray_repeat_run 导致误判/提前熔断)。
    fn emit_text_delta_guarded(&mut self, text: &str) -> Vec<SseEvent> {
        if text.is_empty() {
            return Vec::new();
        }
        self.create_text_delta_events(text)
    }

    /// invoke 嗅探缓冲驱动:文本进缓冲后,循环找完整/半 <invoke> 块,过四道门决定"重组捞回 vs 当文本"。
    /// flush=true 时流已结束,残留半块当普通文本吐(绝不静默吞)。移植 ZyphrZero drain_invoke_sniff_buffer。
    fn drain_invoke_sniff_buffer(&mut self, flush: bool) -> Vec<SseEvent> {
        let mut events = Vec::new();
        // 取出本地 buffer 一次性驱动(避免每轮 clone;退化大缓冲下省 O(n²))。
        let mut buf = std::mem::take(&mut self.invoke_sniff_buffer);
        loop {
            match find_invoke_start(&buf) {
                Some(start) => match find_invoke_block_end(&buf, start) {
                    Some(end) => {
                        // 完整块:过四道门。
                        let before = strip_trailing_stray_tokens(&buf[..start]).to_string();
                        let fence_after_before = fence_open_after(
                            self.code_fence_open,
                            &self.fence_scan_partial,
                            &before,
                        );
                        let parsed = parse_invoke_block(&buf[start..end]);
                        let name_known = parsed
                            .as_ref()
                            .map(|(n, _)| self.known_tool_names.contains(n))
                            .unwrap_or(false);
                        if invoke_looks_like_real_leak(&before) && !fence_after_before && name_known
                        {
                            // 真泄漏:吐块前文本(剥掉尾部独立 stray 行)+ 合成 tool_use。
                            if !before.is_empty() {
                                // before 的围栏状态要并入(它会作为文本吐,推进围栏奇偶)。
                                advance_code_fence_state(
                                    &mut self.code_fence_open,
                                    &mut self.fence_scan_partial,
                                    &before,
                                );
                                events.extend(self.emit_text_delta_guarded(&before));
                            }
                            let (name, input_json) =
                                parsed.expect("parsed is Some when name_known");
                            events.extend(self.synthesize_tool_use(name, input_json));
                        } else {
                            // 不捞回(句中/围栏内/工具名未知/解析失败)→ 整段当普通文本吐。
                            let seg = buf[..end].to_string();
                            advance_code_fence_state(
                                &mut self.code_fence_open,
                                &mut self.fence_scan_partial,
                                &seg,
                            );
                            events.extend(self.emit_text_delta_guarded(&seg));
                        }
                        buf = buf[end..].to_string();
                        continue;
                    }
                    None => {
                        // 半块(未闭合)。行首判定:非行首/围栏内当文本直接吐,不 hold。
                        let before = strip_trailing_stray_tokens(&buf[..start]).to_string();
                        let fence_after_before = fence_open_after(
                            self.code_fence_open,
                            &self.fence_scan_partial,
                            &before,
                        );
                        if !invoke_looks_like_real_leak(&before) || fence_after_before {
                            if !buf.is_empty() {
                                let seg = buf.clone();
                                advance_code_fence_state(
                                    &mut self.code_fence_open,
                                    &mut self.fence_scan_partial,
                                    &seg,
                                );
                                events.extend(self.emit_text_delta_guarded(&seg));
                            }
                            break;
                        }
                        // 行首未闭合块:吐 start 前文本,保留 start.. 等闭合。
                        if start > 0 {
                            let seg = buf[..start].to_string();
                            advance_code_fence_state(
                                &mut self.code_fence_open,
                                &mut self.fence_scan_partial,
                                &seg,
                            );
                            events.extend(self.emit_text_delta_guarded(&seg));
                        }
                        let remainder = buf[start..].to_string();
                        if flush {
                            if !remainder.is_empty() {
                                events.extend(self.emit_text_delta_guarded(&remainder));
                            }
                        } else if remainder.len() > Self::MAX_INVOKE_HOLD_BYTES {
                            // 纯字节上限兜底:永不闭合的 <invoke 不能无限 hold 卡死流。
                            events.extend(self.emit_text_delta_guarded(&remainder));
                        } else {
                            self.invoke_sniff_buffer = remainder;
                        }
                        break;
                    }
                },
                None => {
                    // 无 invoke 开标签。flush 全吐;否则保留可能是半个 <invoke 开标签的尾巴。
                    if flush {
                        if !buf.is_empty() {
                            let seg = buf.clone();
                            advance_code_fence_state(
                                &mut self.code_fence_open,
                                &mut self.fence_scan_partial,
                                &seg,
                            );
                            events.extend(self.emit_text_delta_guarded(&seg));
                        }
                    } else {
                        let keep = partial_invoke_tag_suffix_len(&buf);
                        let emit_len = buf.len() - keep;
                        if emit_len > 0 {
                            let seg = buf[..emit_len].to_string();
                            advance_code_fence_state(
                                &mut self.code_fence_open,
                                &mut self.fence_scan_partial,
                                &seg,
                            );
                            events.extend(self.emit_text_delta_guarded(&seg));
                        }
                        self.invoke_sniff_buffer = buf[emit_len..].to_string();
                    }
                    break;
                }
            }
        }
        events
    }

    /// 收尾 flush invoke 嗅探缓冲(流结束):残留半块当普通文本吐,绝不静默吞。
    fn flush_invoke_sniff_buffer(&mut self) -> Vec<SseEvent> {
        if self.invoke_sniff_buffer.is_empty() {
            return Vec::new();
        }
        self.drain_invoke_sniff_buffer(true)
    }

    /// 处理上游 `reasoningContentEvent` 的一帧结构化思考增量（E1）。
    ///
    /// # 与文本嗅探路径的关系
    ///
    /// 文本嗅探（`process_thinking_content` 里找 `<thinking>` 标签）**保留作兜底**不删：
    /// 上游可能对某些模型仍走内联标签，两条路径都可能触发。因此两者**共用同一个**
    /// `in_thinking_block` / `thinking_block_index` 状态 —— 这是"绝不重复开块"的保证：
    /// 若嗅探路径已经开过 thinking 块，本函数直接往同一个 index 追加 delta。
    ///
    /// # 为什么不在这里关块
    ///
    /// 上游不发"思考结束"信号（`reasoningContentEvent` 是纯增量，没有终止帧），
    /// 收尾统一由 `generate_final_events` 处理（它会补 signature_delta + content_block_stop）——
    /// 与嗅探路径遇不到 `</thinking>` 时的收尾走同一条路，不新增第二套收尾逻辑。
    fn process_reasoning_content(&mut self, reasoning: &ReasoningContentEvent) -> Vec<SseEvent> {
        // 缓存上游真签名（若有）：关闭 thinking 块时 `create_signature_delta_event` 优先回传它。
        // Foxfishc 实测「伪造签名不被识别，cache_read 仍 0」——真签名是多轮 cache 命中的关键。
        // 上游不发则保持 None，收尾回退占位符，行为与改动前逐字节一致。
        if let Some(sig) = reasoning.signature.as_deref() {
            if !sig.is_empty() {
                self.pending_reasoning_signature = Some(sig.to_string());
            }
        }
        let text = &reasoning.text;
        // thinking 未开启（客户端没要 thinking）→ 本帧不下发。
        // 不能当正文发：那会把模型的内部推理混进用户可见回答里。
        //
        // 但**不能就地扔掉**：若本轮上游只吐 reasoning、一个字正文都没有，客户端会拿到
        // 完全空的响应。所以原文留一份（有上限），到 `generate_final_events` 再按
        // 「正文确实为空」判定要不要降级下发 —— 判定必须在收尾做，因为正文可能整轮被
        // hold 在 invoke_sniff_buffer 里、直到 flush 才出现。
        if !self.thinking_enabled {
            self.retain_discarded_reasoning(text);
            return Vec::new();
        }
        if text.is_empty() {
            return Vec::new();
        }

        // 标记本轮走过结构化流：供 process_assistant_response 判定「正文到来即关块」，
        // 使两条路径互斥（否则正文会被当思考内容，答案消失）。
        self.reasoning_stream_seen = true;

        let mut events = Vec::new();
        // 首帧才开块；若嗅探路径已开过，复用它的 index（不重复开块）。
        if !self.in_thinking_block {
            self.in_thinking_block = true;
            let idx = self.state_manager.next_block_index();
            self.thinking_block_index = Some(idx);
            events.extend(self.state_manager.handle_content_block_start(
                idx,
                "thinking",
                json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": { "type": "thinking", "thinking": "" }
                }),
            ));
        }

        if let Some(idx) = self.thinking_block_index {
            events.push(self.create_thinking_delta_event(idx, text));
        }
        events
    }

    /// 把 `!thinking_enabled` 下不下发的 reasoning 原文攒进 `discarded_reasoning`，
    /// 按 [`Self::MAX_DISCARDED_REASONING_BYTES`] 截断（UTF-8 边界安全）。
    ///
    /// 达上限后**丢弃后续帧而非滚动覆盖**：兜底要的是"开头那段能读懂的推理"，
    /// 保尾巴反而会给出一段没有上文的残句。
    fn retain_discarded_reasoning(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let room =
            Self::MAX_DISCARDED_REASONING_BYTES.saturating_sub(self.discarded_reasoning.len());
        if room == 0 {
            return;
        }
        if text.len() <= room {
            self.discarded_reasoning.push_str(text);
        } else {
            // 直接按字节切会 panic 在多字节字符中间（中文推理是常态）。
            let cut = find_char_boundary(text, room);
            self.discarded_reasoning.push_str(&text[..cut]);
        }
    }

    /// 空响应兜底把 reasoning 降级成正文前，补上正文路径的两道文本清洗。
    ///
    /// # 为什么需要单独一个函数，而不是让兜底走 `process_assistant_response`
    ///
    /// 兜底刻意绕开整条正文链（理由见调用处：sniff 缓冲已 flush 过、推理里的
    /// `<invoke>` 不能被重组成真 tool_use）。但"绕开重组"不等于"该绕开清洗"——
    /// reasoning 与正文是**同一次生成**的产物，正文会遇到的模型侧退化它同样会遇到，
    /// 而降级之后它就是客户端眼里的正文了。所以这里只把两道清洗**单独**调一遍：
    ///
    /// ① `clean_leaked_tokens`（#70544 行首泄漏词，开关 `tool_clean_leaked_tokens`）——
    ///    它是无状态的逐行处理，可以独立调用，且会照常累加 `leaked_stripped` 计数，
    ///    使兜底路径的泄漏在收尾诊断里同样可见（不黑箱）。
    /// ② `strip_stray_thinking_end_tags`（孤立 `</thinking>` 标记）—— 推理文本里出现
    ///    闭标签字面量时，原样下发就是把标记泄漏给客户端（正文侧同一形态已有此清洗）。
    ///
    /// # 为什么**不**调 `strip_inline_thinking_when_disabled`
    ///
    /// 那个函数是**跨 chunk 有状态**的剥离器：遇到未闭合的 `<thinking>` 会把余下全部
    /// 内容当思考丢掉，遇到疑似半标签的尾巴会扣进 `thinking_buffer` 等下一个 chunk。
    /// 而此刻已经过了收尾 flush（本文件 :3159 那条），扣下的尾巴**再没有人 drain**。
    /// 更根本的是语义反了：兜底的全部目的就是"把推理当正文发出去"，再拿"剥掉推理"的
    /// 剥离器过一遍，最坏情况是整段被丢 ⇒ 重新变成它要修的那个空响应。
    fn sanitize_degraded_reasoning(&mut self, reasoning: &str) -> String {
        let cleaned = if super::handlers::tool_clean_leaked_tokens_enabled() {
            self.clean_leaked_tokens(reasoning)
        } else {
            reasoning.to_string()
        };
        strip_stray_thinking_end_tags(&cleaned).into_owned()
    }

    /// 关闭由**结构化 reasoning 流**开启的 thinking 块（首个正文 delta 到来时调用）。
    ///
    /// 上游不发"思考结束"信号，所以以「首个非空 assistantResponse 正文」作为结束标志。
    /// 收尾顺序与嗅探路径遇到 `</thinking>` 时一致：空 thinking_delta → signature_delta →
    /// content_block_stop，保证客户端侧 thinking 块结构完整（Anthropic SDK 会校验 signature 非空）。
    ///
    /// 置 `thinking_extracted = true` 使嗅探路径此后不再重新开块（`process_content_with_thinking`
    /// 的首个分支条件含 `!self.thinking_extracted`）。
    fn close_reasoning_thinking_block(&mut self) -> Vec<SseEvent> {
        let Some(idx) = self.thinking_block_index else {
            self.in_thinking_block = false;
            return Vec::new();
        };
        let mut events = Vec::new();
        events.push(self.create_thinking_delta_event(idx, ""));
        events.push(self.create_signature_delta_event(idx));
        events.extend(self.state_manager.handle_content_block_stop(idx));
        self.in_thinking_block = false;
        self.thinking_extracted = true;
        events
    }

    /// 创建 thinking_delta 事件
    fn create_thinking_delta_event(&self, index: i32, thinking: &str) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "thinking_delta",
                    "thinking": thinking
                }
            }),
        )
    }

    /// 创建 signature_delta 事件
    ///
    /// thinking 块流式结束前（`content_block_stop` 之前）必须发一个 signature_delta，
    /// 携带非空签名，满足客户端 thinking 模式下的本地校验。详见
    /// [`THINKING_SIGNATURE_PLACEHOLDER`]。
    ///
    /// 优先回传**上游真签名**（若本流收到过 `reasoningContentEvent` 且带 `signature`）：
    /// Foxfishc 实测「伪造签名不被上游识别，cache_read 仍 0」，真签名是多轮 cache 命中的关键。
    /// 上游不发则回退占位符（`take` 只消费一次，thinking 块只在流末尾关一次）。
    fn create_signature_delta_event(&mut self, index: i32) -> SseEvent {
        let signature = self
            .pending_reasoning_signature
            .take()
            .unwrap_or_else(|| THINKING_SIGNATURE_PLACEHOLDER.to_string());
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "signature_delta",
                    "signature": signature
                }
            }),
        )
    }

    /// 处理工具使用事件
    fn process_tool_use(
        &mut self,
        tool_use: &crate::kiro::model::events::ToolUseEvent,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        self.state_manager.set_has_tool_use(true);

        // tool_use 必须发生在 thinking 结束之后。
        // 但当 `</thinking>` 后面没有 `\n\n`（例如紧跟 tool_use 或流结束）时，
        // thinking 结束标签会滞留在 thinking_buffer，导致后续 flush 时把 `</thinking>` 当作内容输出。
        // 这里在开始 tool_use block 前做一次“边界场景”的结束标签识别与过滤。
        if self.thinking_enabled && self.in_thinking_block {
            if let Some(end_m) = find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer) {
                let thinking_content = self.thinking_buffer[..end_m.start].to_string();
                if !thinking_content.is_empty() {
                    if let Some(thinking_index) = self.thinking_block_index {
                        events.push(
                            self.create_thinking_delta_event(thinking_index, &thinking_content),
                        );
                    }
                }

                // 结束 thinking 块
                self.in_thinking_block = false;
                self.thinking_extracted = true;

                if let Some(thinking_index) = self.thinking_block_index {
                    // 先发送空的 thinking_delta
                    events.push(self.create_thinking_delta_event(thinking_index, ""));
                    // 再发送 signature_delta（满足客户端 thinking 模式本地校验）
                    events.push(self.create_signature_delta_event(thinking_index));
                    // 最后发送 content_block_stop
                    if let Some(stop_event) =
                        self.state_manager.handle_content_block_stop(thinking_index)
                    {
                        events.push(stop_event);
                    }
                }

                // 把结束标签后的内容当作普通文本（通常为空或空白）
                let remaining = self.thinking_buffer[end_m.end()..].trim_start().to_string();
                self.thinking_buffer.clear();
                if !remaining.is_empty() {
                    events.extend(self.create_text_delta_events(&remaining));
                }
            } else if self.reasoning_stream_seen {
                // 结构化 reasoning 开的 thinking 块：内容直接以 thinking_delta 下发、不进
                // thinking_buffer，故上面的 `find_real_thinking_end_tag_at_buffer_end` 恒为 None。
                // 工具调用前必须先把块关掉 —— 否则 tool_use 块 start 时 thinking 块仍未 stop，
                // 违反 Anthropic SSE「先 stop 当前块、再 start 下一块」的顺序契约（CC 解析报错）。
                // 兜底 flush buffer：极边缘的「嗅探开块 → 又来 reasoning」混合场景里，
                // buffer 可能还有嗅探暂存的内容，关块前先作为 thinking_delta 补发（不吞字）。
                if !self.thinking_buffer.is_empty() {
                    if let Some(idx) = self.thinking_block_index {
                        events.push(self.create_thinking_delta_event(idx, &self.thinking_buffer));
                    }
                    self.thinking_buffer.clear();
                }
                events.extend(self.close_reasoning_thinking_block());
            }
        }

        // thinking 模式下，process_content_with_thinking 可能会为了探测 `<thinking>` 而暂存一小段尾部文本。
        // 如果此时直接开始 tool_use，状态机会自动关闭 text block，导致这段"待输出文本"看起来被 tool_use 吞掉。
        // 约束：只在尚未进入 thinking block、且 thinking 尚未被提取时，将缓冲区当作普通文本 flush。
        if self.thinking_enabled
            && !self.in_thinking_block
            && !self.thinking_extracted
            && !self.thinking_buffer.is_empty()
        {
            let buffered = std::mem::take(&mut self.thinking_buffer);
            events.extend(self.create_text_delta_events(&buffered));
        }

        // 获取或分配块索引
        let block_index = if let Some(&idx) = self.tool_block_indices.get(&tool_use.tool_use_id) {
            idx
        } else {
            let idx = self.state_manager.next_block_index();
            self.tool_block_indices
                .insert(tool_use.tool_use_id.clone(), idx);
            idx
        };

        // 还原工具名称（如果有映射）
        let original_name = self
            .tool_name_map
            .get(&tool_use.name)
            .cloned()
            .unwrap_or_else(|| tool_use.name.clone());
        // 仅当入站映射过才记下 tool_use_id → 还原名，供 generate_final_events 截断兜底
        // 做参数还原。未映射的（Kiro 直接发同名工具）不记 —— 兜底据此区分「该还原」与
        // 「该原样透传」，避免把不认识的参数清空。
        if self.tool_name_map.contains_key(&tool_use.name) {
            self.tool_use_names
                .insert(tool_use.tool_use_id.clone(), original_name.clone());
        }

        // 发送 content_block_start
        let start_events = self.state_manager.handle_content_block_start(
            block_index,
            "tool_use",
            json!({
                "type": "content_block_start",
                "index": block_index,
                "content_block": {
                    "type": "tool_use",
                    "id": tool_use.tool_use_id,
                    "name": original_name,
                    "input": {}
                }
            }),
        );
        events.extend(start_events);

        // ⭐修复 Invalid tool parameters（根治，非逐片透传）：
        // 根因（4路研究 + kiro2api 参照实现结论）：Kiro 的 toolUseEvent.input 逐帧到达，逐片当
        // partial_json 原样透传时，一旦(a)上游帧非严格前缀单调（启发式 else 分支重复拼接）、或
        // (b)中间帧被静默丢弃/截断，客户端拼接后的**总 JSON** 就非法 → 报 Invalid tool parameters。
        // Anthropic 契约：客户端只在 content_block_stop 才把所有 partial_json 拼接后**一次性** parse，
        // 不要求逐片合法。故最稳做法（kiro2api 已验证）：按 tool_use_id **缓冲**到 stop，校验后
        // **一次性发单个 delta**。全程 String 级重组，绝不做字节切片，char-boundary panic 面彻底消除。
        //
        // 重组语义（与真实上游模式对齐，见 `merge_tool_input` 完备决策表）：
        //   累积快照 / 纯增量碎片 / 重复终帧 / 迟到旧短快照 / 非前缀重写 均被正确处理。
        //   关键：非前缀双完整对象不再被无脑 append 成 `}{` 粘连非法 JSON（Invalid tool parameters 类型 C）。
        if !tool_use.input.is_empty() {
            let model = self.model.clone();
            let buf = self
                .tool_input_sent
                .entry(tool_use.tool_use_id.clone())
                .or_default();
            // 帧探针（KIRO_TOOL_TRACE）：抓上游逐帧原文 + 合并轨迹，定性 Invalid tool parameters 真因。
            let buf_before = buf.clone();
            *buf = merge_tool_input(buf, &tool_use.input);
            trace_tool_frame(
                &model,
                &tool_use.tool_use_id,
                &tool_use.name,
                tool_use.stop,
                &tool_use.input,
                &buf_before,
                buf,
            );
        }

        // 仅在 stop 时把完整缓冲一次性发出 + 关闭块（此前只累积、不发 partial_json）。
        if tool_use.stop {
            let mut assembled = self
                .tool_input_sent
                .remove(&tool_use.tool_use_id)
                .unwrap_or_default();
            // 出站参数还原：把 Kiro 参数形态还原成 Claude Code 参数形态
            // （fs_write 的 path/text → Write 的 file_path/content、read_file 的 start_line →
            // Read 的 offset）。
            // ⚠️ **仅当该 Kiro 工具名入站时映射过**（tool_name_map 有记录）才做还原；否则
            // （Kiro 直接发同名工具，未经历入站映射，如 DSML 调试场景的裸 "Write"）原样透传，
            // 避免 map_tool_input_from_kiro 把不认识的参数（code/note/DSML 标记）清空成 {}。
            // 仅对合法 JSON 生效；非法串交 flush_tool_input 的 repair 层。
            if !assembled.is_empty() && self.tool_name_map.contains_key(&tool_use.name) {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&assembled) {
                    assembled =
                        crate::anthropic::converter::map_tool_input_from_kiro(&original_name, value)
                            .to_string();
                }
            }
            events.extend(self.flush_tool_input(block_index, assembled));
            if let Some(stop_event) = self.state_manager.handle_content_block_stop(block_index) {
                events.push(stop_event);
            }
        }

        events
    }

    /// 把某 tool_use 累积完整的 input 作为**单个** input_json_delta 发出（stop 时调用 / 截断收尾兜底）。
    ///
    /// 校验完整 JSON：合法→原样发；非法→告警并尽力发（不静默吞成空参数——空参数会让客户端把
    /// 一个失败的工具调用当成"无参数成功调用"执行，比报错更危险）。空串→不发（无参工具，客户端得 `{}`）。
    fn flush_tool_input(&mut self, block_index: i32, mut assembled: String) -> Vec<SseEvent> {
        if assembled.is_empty() {
            return Vec::new();
        }
        self.output_tokens += (assembled.len() as i32 + 3) / 4;
        if serde_json::from_str::<serde_json::Value>(&assembled).is_err() {
            // 拼装后仍非法：多因上游发了非法 JSON（如 JSON 不支持的 \x 转义 / 截断 \uXXXX / 裸控制符）
            // 或中间帧丢失截断。客户端拿坏 JSON 直接 parse 失败 → Invalid tool parameters。
            // 归因标签（纯可观测，绝不进控制流）：单遍 string-aware 扫描把非法串按责任方分流——
            // truncated=帧丢失/上游截断、illegal_chars=模型侧非法转义/裸控制符、两者兼有、其它畸形。
            // 判据与 repair 层同源，只用于日志定位真因（"修不好的残留到底是谁的责任"）。
            let defect = classify_tool_json_defect(&assembled);
            // malformed 子型细分（纯诊断）：只在归因 Malformed 时算——它是"结构闭合+字符合法但仍非法"的
            // 兜底类,笼统 "malformed" 无法区分类型 A(上游吐坏)/类型 C(我们拼坏)。子型标签(glued/
            // trailing_comma/missing_comma/expected_value/...)直接指向责任方。其它归因输出 "-"（不适用）。
            let subkind = if defect == ToolJsonDefect::Malformed {
                malformed_subkind(&assembled)
            } else {
                "-"
            };
            tracing::warn!(
                block_index,
                defect = defect.as_str(),
                subkind,
                "tool_use 拼装后 input 非合法 JSON（长度 {}），归因={} 子型={}",
                assembled.len(),
                defect.as_str(),
                subkind
            );
            // 帧探针（KIRO_TOOL_TRACE）：非法时额外打印**完整拼装串**全文（含 model + 归因标签 + 子型），
            // 用于坐实是类型 A（上游模型帧本身含非法转义/乱码 token）还是类型 C（合并逻辑洞，已修）。
            if tool_trace_enabled() {
                tracing::warn!(
                    target: "kiro::tool_trace",
                    model = %self.model,
                    block_index,
                    defect = defect.as_str(),
                    subkind,
                    assembled_len = assembled.len(),
                    assembled = %assembled,
                    "[tool_trace] 拼装后非法 JSON 全文（定性 Invalid tool parameters）"
                );
            }
            // 缓解④（根治向，默认开）：先尝试把坏 JSON 修成合法（转义非法反斜杠/裸控制符、补全截断），
            // 修复后强制复验通过才用。成功则 assembled 已是合法 JSON、直接落到下方正常发送路径，
            // 完全跳过失败态对齐/暴露错误逻辑（客户端能正常 parse，无需退避重试）。
            if super::handlers::tool_repair_json_enabled() {
                if let Some(repaired) = repair_tool_json(&assembled) {
                    tracing::info!(
                        block_index,
                        orig_len = assembled.len(),
                        repaired_len = repaired.len(),
                        "tool_use 非法 JSON 已修复为合法 JSON（Invalid tool parameters 根治）"
                    );
                    // 修复成功:assembled 已合法。**不 early-return**——fall through 到函数尾的统一
                    // 出口(unwrap_double_encoded + 发送),否则 repair 结果恰好是双重编码串时会跳过
                    // 洞1 解包(review confirmed:如 \U 被修成字面后整体成 Value::String)。两条路径
                    // (原本合法 / 修复后)从此经同一 unwrap + 发送出口,消除路径不一致。
                    assembled = repaired;
                    // 修复成功 → 跳过下方缓解②/③/⑤(那些是给"修不好"的),直接走统一出口。
                    // 用标签跳出 is_err 块:此处 break 掉外层 if is_err 的剩余分支。
                }
                // 修不好 → 落入下方缓解②/③/⑤（与不开修复层等价，最坏情况不劣化）。
            }
            // 若 repair 已把 assembled 修成合法,则跳过缓解②/③/⑤(它们只服务"仍非法"的残留)。
            let repaired_ok = serde_json::from_str::<serde_json::Value>(&assembled).is_ok();
            if !repaired_ok {
                // 缓解⑤：截断跨轮恢复（开关默认关）。只在**修复层已启用且也补不回**（修复层开时走到这里
                // = 上面 repair 已返回 None）且归因为真截断（Truncated / TruncatedAndIllegal）时触发：
                // 不发不完整的 partial_json
                // （半截参数会被客户端当完整调用执行，比整轮失败更危险），改置失败态 + 收尾补发 SSE error，
                // 让客户端退避后重试整个请求。绝不 report_failure 连坐号（工具截断≠号坏，隔离铁律）。
                // 非截断的畸形（IllegalChars/Malformed）不归本开关管，仍走②/③按原语义处理。
                if should_recover_truncation(
                    defect,
                    super::handlers::tool_truncation_recovery_enabled(),
                    super::handlers::tool_repair_json_enabled(),
                ) {
                    tracing::warn!(
                        block_index,
                        defect = defect.as_str(),
                        "tool_use 参数真截断且修复层补不回：置失败态让客户端重试整轮（截断跨轮恢复）"
                    );
                    if self.completion.is_ok() {
                        self.completion = CompletionStatus::UpstreamError {
                        code: "INVALID_TOOL_INPUT".to_string(),
                        message: "工具调用参数被上游截断（缺整段值），请重试；如反复触发可拆小该调用。"
                            .to_string(),
                    };
                    }
                    // 不发这条截断的坏 partial_json（收尾兜底据失败态补发 SSE error）。
                    return Vec::new();
                }
                // 缓解②：流式失败态对齐（开关默认关）。开启时把流式也置 UpstreamError{INVALID_TOOL_INPUT}
                // 失败态，与非流式对齐（收尾记 ServerError、不污染成功率、收尾兜底会补发 SSE error）。
                // 幂等：只在首个失败落定。绝不 report_failure 连坐号（工具非法≠号坏，隔离铁律）。
                if super::handlers::tool_stream_align_failure_enabled() && self.completion.is_ok() {
                    self.completion = CompletionStatus::UpstreamError {
                        code: "INVALID_TOOL_INPUT".to_string(),
                        message: "工具调用参数非合法 JSON（模型侧生成异常）".to_string(),
                    };
                }
                // 缓解③：如实暴露错误。**不发坏 JSON 的判据绑定"失败态已置"（completion.is_err），
                // 而非③开关本身**——消除②③拆开的两个矛盾组合（验证报告缺陷2/3）：
                //   ·②开③关(旧):置了失败态却 fall-through 发坏 JSON → 记账失败却发坏 JSON,自相矛盾;
                //   ·②关③开(旧):completion 仍 Ok 却 return 吞掉 → 记成功但客户端拿 input:{} 当成功执行(更危险)。
                // 新语义:只要②或⑤已置失败态(completion.is_err)→ 一律不发坏 partial_json(收尾据失败态补 SSE
                // error);completion 仍 Ok(②③都关)→ 保持现状原样发出交客户端(绝不静默吞成空参)。
                // ③开关现语义 = 是否额外"主动置失败态并暴露"(见下),与"不发坏 JSON"由失败态统一裁决解耦。
                if super::handlers::tool_expose_error_to_client_enabled() && self.completion.is_ok()
                {
                    // ③开但②没置态(如②关):③自己置失败态,保证"暴露错误"语义成立(不发坏 JSON + 收尾补 error)。
                    self.completion = CompletionStatus::UpstreamError {
                        code: "INVALID_TOOL_INPUT".to_string(),
                        message: "工具调用参数非合法 JSON（模型侧生成异常）".to_string(),
                    };
                }
                // 统一出口:失败态已置(②/③/⑤任一)→ 不发坏 JSON。这一条兜住所有开关组合的自洽。
                if !self.completion.is_ok() {
                    return Vec::new();
                }
            } // end if !repaired_ok(修复成功则跳过②/③/⑤)
        }
        // 洞1:整包双重编码解包。走到这里 assembled 已是合法 JSON(原本合法 / 修复后)。若它其实是
        // 「被再套一层字符串编码的 object/array」,解一层还原成客户端能按 object 消费的形态,消灭
        // 一类漏过 repair 层的 InputValidationError。
        // 【P2-1 解耦】此前裹在 tool_repair_json 开关下,导致用户为排查关掉 repair 时连带关掉解包——
        // 而解包不改语义(只剥误加的一层字符串编码)、对合法 object/array 是安全 no-op(as_str 返回
        // None 即不动),与"修坏 JSON"是正交能力,应独立恒开。故移出开关无条件跑。
        if let Some(unwrapped) = unwrap_double_encoded(&assembled) {
            tracing::info!(
                block_index,
                "tool_use 参数为双重编码,已解一层还原为 object/array"
            );
            assembled = unwrapped;
        }
        self.state_manager
            .handle_content_block_delta(
                block_index,
                json!({
                    "type": "content_block_delta",
                    "index": block_index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": assembled
                    }
                }),
            )
            .into_iter()
            .collect()
    }

    /// 生成最终事件序列
    pub fn generate_final_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 截断兜底：若某 tool_use 累积了 input 但流在 stop 之前就结束（上游截断/客户端断开），
        // 缓冲会残留、块未关闭。这里把残留 input 尽力发出并关闭块，避免客户端卡在未闭合的
        // tool_use 块上。（正常路径 stop 时已 flush + remove，此处只处理未收到 stop 的残留。）
        if !self.tool_input_sent.is_empty() {
            let pending: Vec<(String, String)> = self.tool_input_sent.drain().collect();
            for (tool_use_id, mut assembled) in pending {
                if let Some(&idx) = self.tool_block_indices.get(&tool_use_id) {
                    // 截断兜底同样做参数还原：块 start 已用还原名（tool_use_names 记录，
                    // 且只记录映射过的），残留 input 需还原成客户端形态，否则名参错配
                    // （Write + {path,text}）。未映射的（tool_use_names 无记录）原样透传。
                    if let Some(cname) = self.tool_use_names.get(&tool_use_id).cloned() {
                        if let Ok(value) =
                            serde_json::from_str::<serde_json::Value>(&assembled)
                        {
                            assembled = crate::anthropic::converter::map_tool_input_from_kiro(
                                &cname,
                                value,
                            )
                            .to_string();
                        }
                    }
                    events.extend(self.flush_tool_input(idx, assembled));
                    if let Some(stop_event) = self.state_manager.handle_content_block_stop(idx) {
                        events.push(stop_event);
                    }
                }
            }
        }

        // Flush thinking_buffer 中的剩余内容。
        //
        // ⚠️ 门不能只看 `!buffer.is_empty()`：扣留窗口改为「按标签语法判定」后，不可能是
        // 标签的思考内容会被**及时**下发，于是「`in_thinking_block=true` 而 buffer 为空」
        // 成了常态。只看 buffer 会让 thinking 块在收尾时**不被 stop**，而其后的 DSML /
        // reclaim 残留仍会 start 一个 text 块 ⇒ SSE 出现「新块 start → 旧块 stop」交错，
        // 违反 Anthropic「先 stop 当前块再 start 下一块」契约（CC 解析报错）。
        if self.thinking_enabled && (self.in_thinking_block || !self.thinking_buffer.is_empty()) {
            if self.in_thinking_block {
                // 末尾可能残留 `</thinking>`（例如紧跟 tool_use 或流结束），需要在 flush 时过滤掉结束标签。
                if let Some(end_m) = find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer)
                {
                    let thinking_content = self.thinking_buffer[..end_m.start].to_string();
                    if !thinking_content.is_empty() {
                        if let Some(thinking_index) = self.thinking_block_index {
                            events.push(
                                self.create_thinking_delta_event(thinking_index, &thinking_content),
                            );
                        }
                    }

                    // 关闭 thinking 块：先发送空的 thinking_delta，再发 signature_delta，最后 content_block_stop
                    if let Some(thinking_index) = self.thinking_block_index {
                        events.push(self.create_thinking_delta_event(thinking_index, ""));
                        events.push(self.create_signature_delta_event(thinking_index));
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }

                    // 把结束标签后的内容当作普通文本（通常为空或空白）
                    let remaining = self.thinking_buffer[end_m.end()..].trim_start().to_string();
                    self.thinking_buffer.clear();
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;
                    if !remaining.is_empty() {
                        events.extend(self.create_text_delta_events(&remaining));
                    }
                } else {
                    // 如果还在 thinking 块内，发送剩余内容作为 thinking_delta
                    // （buffer 可能为空 —— 内容已在流式期间及时下发，此处只负责收尾 stop）
                    if !self.thinking_buffer.is_empty() {
                        if let Some(thinking_index) = self.thinking_block_index {
                            events.push(self.create_thinking_delta_event(
                                thinking_index,
                                &self.thinking_buffer,
                            ));
                        }
                    }
                    // 关闭 thinking 块：先发送空的 thinking_delta，再发送 content_block_stop
                    if let Some(thinking_index) = self.thinking_block_index {
                        // 先发送空的 thinking_delta
                        events.push(self.create_thinking_delta_event(thinking_index, ""));
                        // 再发送 signature_delta（满足客户端 thinking 模式本地校验）
                        events.push(self.create_signature_delta_event(thinking_index));
                        // 最后发送 content_block_stop
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }
                }
            } else {
                // 否则发送剩余内容 —— 走**统一出口** `emit_non_thinking_text`，不要直接
                // create_text_delta_events。
                //
                // 这里是 reclaim 旁路的最后一处：thinking 尚未出现时，
                // `process_content_with_thinking`(:1553) 会扣留 `"<thinking>".len()` = 10 字节
                // 当「可能是半标签」，而 `</invoke>` 只有 9 字节 —— 于是文本化 invoke 的**闭合标签**
                // 滞留在 thinking_buffer，只有前半截进了 invoke_sniff_buffer。若这里直接吐 text_delta，
                // 重组层拿到的永远是「未闭合的 `<invoke`」→ 当纯文本发给客户端 → **工具不执行**。
                // 线上实测 textifiedInvokeHits 17 : reclaimedInvokeCalls 1 即此。
                //
                // 并回 sniff 缓冲后，下方 :2466 的收尾 flush 会把完整块重组成 tool_use。
                //
                // 残留里可能有被扣留窗口按「可能是标签」扣住的**完整**孤立闭标签
                // （`</thinking>` 已到、`\n\n` 永远不会到了）。EOF 时它已确定是标记而非
                // 正文，原样下发即泄漏；判据复用 `find_stray_thinking_end_tag`。
                let buffer_content =
                    strip_stray_thinking_end_tags(&self.thinking_buffer).into_owned();
                if !buffer_content.is_empty() {
                    events.extend(self.emit_non_thinking_text(&buffer_content));
                }
            }
            self.thinking_buffer.clear();
        }

        // 客户端**没要** thinking 时剥离器（`strip_inline_thinking_when_disabled`）的残留收尾。
        //
        // ⚠️ 必须是独立分支，不能并进上面那个 `if`：那条的门是
        // `self.thinking_enabled && !buffer.is_empty()`，而剥离器**只在 `!thinking_enabled`
        // 时**往同一个 `thinking_buffer` 写 —— 两个条件互斥 ⇒ 那条 flush 对剥离器的残留
        // **永远不执行** ⇒ 残留静默蒸发。
        //
        // 两种残留都必须处理，否则都是**静默吞字**（面板记成功、客户端收到空/截断回答）：
        // ① `in_thinking_block=true`：思考块没闭合。丢思考本体，但标签**之后**的正文要下发
        //    （`</thinking>Answer` 这种零空白形态流式期间三个查找函数都不认，见
        //    `split_unclosed_thinking_residue_at_eof`）。
        // ② `in_thinking_block=false`：尾巴是被 `partial_tag_suffix_len` 扣住的「可能是半个
        //    `<thinking>`」。EOF 时它已确定**不是**标签（后续 chunk 不存在了），是正文的一部分
        //    —— 散文里一个孤立 `<`（"条件 a < b"）恰好落在流末尾就是这个形态。
        if !self.thinking_enabled && !self.thinking_buffer.is_empty() {
            let residue = std::mem::take(&mut self.thinking_buffer);
            let visible = if self.in_thinking_block {
                Cow::Borrowed(split_unclosed_thinking_residue_at_eof(&residue))
            } else {
                // 尾巴可能是被扣留窗口扣住的**完整**孤立闭标签（`\n\n` 永远不会到了）。
                // EOF 时它已确定是标记，原样下发即泄漏。
                strip_stray_thinking_end_tags(&residue)
            };
            self.in_thinking_block = false;
            if !visible.is_empty() {
                // 走统一出口：残留里可能含文本化 invoke 的尾巴，直接 create_text_delta_events
                // 会绕开重组层（与上面 :2595 那条注释记的是同一型缺陷）。
                let visible = visible.to_string();
                events.extend(self.emit_non_thinking_text(&visible));
            }
        }

        // Flush DSML 尾巴缓冲:把被误判为半标记而 hold 住的残留(或末尾孤立 `<`)作为普通文本补发,
        // 避免静默吞字。**必须放在 thinking 块收尾之后**:thinking 模式下 strip_dsml_markers 先于
        // process_content_with_thinking 执行,末尾残留 `<` 被 hold 进 dsml_tail_buffer(不进
        // thinking_buffer)。若在 thinking 块 stop 之前 flush,create_text_delta_events 会先开一个
        // text 块(更大索引),而更小索引的 thinking 块尚未 stop → SSE 出现「新块 start → 旧块 stop」
        // 交错,违反 Anthropic「先 stop 当前块再 start 下一块」契约,CC 可能解析报错。放在此处,
        // 残留 text 块在 thinking 块 stop 之后才 start,顺序合法;残留也使 has_non_thinking_blocks()
        // 变真,避免下方「仅 thinking」分支多补一个空格 text 块。
        events.extend(self.flush_dsml_tail());
        // 收尾 flush invoke 嗅探缓冲:流结束时残留的半块(未等到 </invoke>)当普通文本吐,绝不静默吞。
        events.extend(self.flush_invoke_sniff_buffer());

        // ⭐ 空响应兜底（`!thinking_enabled`）：本轮**只有** reasoning、正文为空时，
        // 把攒下的 reasoning 降级成正文下发。
        //
        // 🔴 位置是判据的一部分，不能上移：上面三处 flush（thinking_buffer / dsml_tail /
        // invoke_sniff_buffer）都可能在此刻才吐出本轮唯一的正文 —— 整轮文本被 hold 到收尾
        // 是**已知常态**（行首未闭合的 `<invoke`、末尾孤立 `<`）。若把本分支排在 flush 之前，
        // `body_content_seen` 还是 false ⇒ 推理与正文**双份**下发，等于把内部推理泄漏给
        // 明确表示不想看它的用户。所以这里先按已产出的 events 补一次判定，再决定。
        //
        // 与下方「只有 thinking 块 → 补空格文本块」那条互斥（两条的门分别是
        // `!self.thinking_enabled` 与 `self.thinking_enabled`），谁在前都不影响结果；
        // 排这里只为紧跟它依赖的 flush。
        //
        // 🔴 第三道门 `completion.is_ok()`：**失败**的一轮不得被兜底塞进推理文本。
        // 本函数是所有收尾路径的公共出口，失败路径同样会走到这里：
        //   · 传输层读流 Err → `mark_transport_error` 后 `handlers.rs:1587` 直接调本函数；
        //   · decoder 永久停止 → `mark_decoder_stopped` 后由流末 `handlers.rs:1603` 调；
        //   · buffered 缓冲溢出 → `process_and_buffer` 内 `mark_decoder_stopped`（本文件
        //     :3455），收尾经 `finish_and_get_all_events` 调；
        //   · in-band Error/Exception 帧、工具参数真截断恢复（:2967）也都置了失败态。
        // 这些路径已各自补发了 SSE error 事件，客户端据此退避重试。若此时再补一段推理文本，
        // 客户端看到的是「error + 一段推理正文」，而记账侧仍按失败落库 —— 等于给一次失败
        // 凭空记上 output_tokens，且推理文本对客户端毫无用处（它本来就要重试整轮）。
        // 兜底的目标只有一个：避免**成功**的一轮返回空内容。
        self.observe_visible_body(&events);
        if !self.thinking_enabled && !self.body_content_seen && self.completion.is_ok() {
            let reasoning = std::mem::take(&mut self.discarded_reasoning);
            // 纯空白的 reasoning 降级下发没有意义（客户端看到的还是空），保持既有丢弃行为。
            if !reasoning.trim().is_empty() {
                tracing::warn!(
                    model = %self.model,
                    reasoning_bytes = reasoning.len(),
                    "本轮上游只吐结构化 reasoning、无正文；thinking 未开启故 reasoning 本应丢弃，\
                     但那会让客户端收到完全空的响应 —— 降级为正文下发以避免空响应"
                );
                let cleaned = self.sanitize_degraded_reasoning(&reasoning);
                // 清洗后可能只剩空白（整段都是泄漏词/标记）：那就退回丢弃，不发空 delta
                // （`create_text_delta_events` 自身无空串守卫，空 text_delta 只会让客户端
                // 多一个无意义的块）。
                if !cleaned.trim().is_empty() {
                    // 降级出来的文本**计入** output_tokens：它是真下发的内容，
                    // 而 `process_reasoning_content` 那条丢弃路径从不计数（正确，因为没发）。
                    // 计的是**清洗后**的量 —— 记账口径必须与实际下发的内容一致。
                    self.output_tokens += estimate_tokens(&cleaned);
                    // 刻意用 create_text_delta_events 而非 emit_non_thinking_text：
                    // ① 此刻 invoke_sniff_buffer 刚 flush 过，再往里塞就没人 drain 了（会静默吞）；
                    // ② 推理里出现 `<invoke …>` 是"模型在想要调什么工具"，绝不能被重组层
                    //    捞成真 tool_use —— 那是凭空执行工具（同一理由见
                    //    `process_assistant_response` 里剥离器排在 reclaim 之前那段注释）。
                    events.extend(self.create_text_delta_events(&cleaned));
                }
            }
        }

        // 如果整个流中只产生了 thinking 块，没有 text 也没有 tool_use，
        // 则设置 stop_reason 为 max_tokens（表示模型耗尽了 token 预算在思考上），
        // 并补发一套完整的 text 事件（内容为一个空格），确保 content 数组中有 text 块
        if self.thinking_enabled
            && self.thinking_block_index.is_some()
            && !self.state_manager.has_non_thinking_blocks()
        {
            self.state_manager.set_stop_reason("max_tokens");
            events.extend(self.create_text_delta_events(" "));
        }

        // 使用从 contextUsageEvent 计算的 input_tokens，如果没有则使用估算值
        let final_input_tokens = self.context_input_tokens.unwrap_or(self.input_tokens);
        // 剔除 cache 读写，得到 Anthropic usage 的 input_tokens 口径
        let billed = self
            .cache_usage
            .map(|c| {
                billed_input_tokens(
                    final_input_tokens,
                    c.cache_creation_input_tokens,
                    c.cache_read_input_tokens,
                )
            })
            .unwrap_or(final_input_tokens);

        // 生成最终事件
        events.extend(self.state_manager.generate_final_events(
            billed,
            self.output_tokens,
            self.cache_usage,
        ));

        // 泄漏 token 诊断收尾（可观测，不改任何已发内容）：本请求若清洗过泄漏 token / 命中 saturation,
        // 如实记一条——绝不黑箱。saturation（整段纯泄漏词行）= #70544 模型侧整段退化的信号,网关只能
        // 清洗单个粘连、救不了整段（Bug B），此处标注归因便于 dwgx 判"是模型抽风非网关问题"。
        if self.leaked_stripped > 0 || self.leaked_saturation_lines > 0 {
            // 可观测:本请求发生过泄漏清洗 / 命中 saturation 退化(各计一次请求级)。
            crate::common::recovery_metrics::bump_leaked_cleaned_request();
            if self.leaked_saturation_lines > 0 {
                crate::common::recovery_metrics::bump_leaked_saturation_request();
            }
            tracing::warn!(
                model = %self.model,
                leaked_stripped = self.leaked_stripped,
                saturation_lines = self.leaked_saturation_lines,
                "检测到 #70544 泄漏 token：已清洗 {} 个（其中 {} 行为整段纯泄漏词=模型侧整段退化，网关仅能清洗不能根治，建议该模型高多字节上下文场景 /clear 或换 sonnet）",
                self.leaked_stripped,
                self.leaked_saturation_lines,
            );
            if leak_trace_enabled() {
                tracing::warn!(
                    target: "kiro::leak_trace",
                    model = %self.model,
                    leaked_stripped = self.leaked_stripped,
                    saturation_lines = self.leaked_saturation_lines,
                    "[leak_trace] 本请求泄漏 token 清洗全貌"
                );
            }
        }
        // stray 泄漏形态观测收尾(请求级各计一次):点亮 clean 层够不到的句中/独占黑洞。
        // 这与 leaked_stripped 互补——leaked_stripped 只记行首真剥掉的,这里记"见到但可能没处理"的形态。
        if self.stray_standalone_seen > 0 {
            crate::common::recovery_metrics::bump_stray_standalone_seen();
        }
        if self.stray_inline_seen > 0 {
            crate::common::recovery_metrics::bump_stray_inline_seen();
            tracing::warn!(
                model = %self.model,
                inline_seen = self.stray_inline_seen,
                standalone_seen = self.stray_standalone_seen,
                "检测到句中/独占 stray 泄漏词(clean 层只清行首、句中未处理,此为观测取证:确认真机泄漏形态)"
            );
        }

        // TTFB 兜底：整轮文本被 hold 在 invoke_sniff_buffer / dsml_tail_buffer / thinking_buffer
        // 里、直到流末才 flush 的响应，其首个内容 delta 是在**本函数**产生的，不经过
        // process_kiro_event 的打点。不补这一处，这批响应的 first_token_ms 会永远是 NULL，
        // 等于把一个已知形态的缺陷藏进数据里。
        // 注：此时的数值 ≈ 整轮耗时（因为内容确实是到最后才吐出来的），语义正确但与
        // "TTFB 应该很小" 的直觉相反 —— 这反映的是真实用户体验，不是打点错误。
        self.mark_first_token_if_content(&events);

        events
    }
}

/// 缓冲流处理上下文 - 用于 /cc/v1/messages 流式请求
///
/// 与 `StreamContext` 不同，此上下文会缓冲所有事件直到流结束，
/// 然后用从 `contextUsageEvent` 计算的正确 `input_tokens` 更正 `message_start` 事件。
///
/// 工作流程：
/// 1. 使用 `StreamContext` 正常处理所有 Kiro 事件
/// 2. 把生成的 SSE 事件缓存起来（而不是立即发送）
/// 3. 流结束时，找到 `message_start` 事件并更新其 `input_tokens`
/// 4. 一次性返回所有事件
pub struct BufferedStreamContext {
    /// 内部流处理上下文（复用现有的事件处理逻辑）
    inner: StreamContext,
    /// 缓冲的所有事件（包括 message_start、content_block_start 等）
    event_buffer: Vec<SseEvent>,
    /// 估算的 input_tokens（用于回退）
    estimated_input_tokens: i32,
    /// 是否已经生成了初始事件
    initial_events_generated: bool,
    /// 已缓冲事件的累计字节数（C4：内存上限守卫，超 [`MAX_BUFFERED_EVENT_BYTES`] 即停止缓冲）
    buffered_bytes: usize,
    /// 是否已因超出缓冲上限而截断（只标记一次，避免重复置失败态/重复告警）
    buffer_overflowed: bool,
}

impl BufferedStreamContext {
    /// 创建缓冲流上下文
    pub fn new(
        model: impl Into<String>,
        estimated_input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
        known_tool_names: std::collections::HashSet<String>,
    ) -> Self {
        let inner = StreamContext::new_full(
            model,
            estimated_input_tokens,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
        );
        Self {
            inner,
            event_buffer: Vec::new(),
            estimated_input_tokens,
            initial_events_generated: false,
            buffered_bytes: 0,
            buffer_overflowed: false,
        }
    }

    /// 估算一批事件的字节占用（event 名 + 序列化 data），用于缓冲上限累计。
    fn estimate_events_bytes(events: &[SseEvent]) -> usize {
        events
            .iter()
            .map(|e| e.event.len() + serde_json::to_string(&e.data).map(|s| s.len()).unwrap_or(0))
            .sum()
    }

    /// 是否已因缓冲超上限而截断（C4）。
    pub fn buffer_overflowed(&self) -> bool {
        self.buffer_overflowed
    }

    /// 返回本次请求解析出的最终用量（供用量统计埋点使用）
    /// 首个真实内容 delta 时刻（透传内部 StreamContext）。
    ///
    /// ⚠️ 语义：这是**上游首 token 到达网关**的时刻，**不是客户端看到的时刻** ——
    /// buffered 分发把整轮憋到流末才吐，客户端观测到的 TTFB ≈ latency_ms 总值。
    /// 两者的差正是 ccAutoBuffer 的代价，分开记录才能量化它。
    pub fn first_token_at(&self) -> Option<std::time::Instant> {
        self.inner.first_token_at()
    }

    pub fn resolved_usage(&self) -> ResolvedUsage {
        self.inner.resolved_usage()
    }

    /// 本次响应的完成状态（透传内部 StreamContext）
    pub fn completion(&self) -> &CompletionStatus {
        self.inner.completion()
    }

    /// 用量记账应采用的最终结果分类（透传，去硬编码 Success）
    pub fn completion_outcome(&self) -> RequestOutcome {
        self.inner.completion_outcome()
    }

    /// 是否已内联发过 SSE error 事件（透传）
    pub fn error_event_emitted(&self) -> bool {
        self.inner.error_event_emitted()
    }

    /// 标记已发过 SSE error 事件（透传）
    pub fn mark_error_event_emitted(&mut self) {
        self.inner.mark_error_event_emitted();
    }

    /// 标记传输层中断（透传）
    pub fn mark_transport_error(&mut self, message: impl Into<String>) {
        self.inner.mark_transport_error(message);
    }

    /// 标记解码器永久停止（透传）
    pub fn mark_decoder_stopped(&mut self, message: impl Into<String>) {
        self.inner.mark_decoder_stopped(message);
    }

    /// 设置 prompt 缓存记账明细（前缀估算注入；在 process_and_buffer 之前调用）
    pub fn set_cache_usage(&mut self, cache_usage: Option<CacheUsageBreakdown>) {
        self.inner.set_cache_usage(cache_usage);
    }

    /// 处理 Kiro 事件并缓冲结果
    ///
    /// 复用 StreamContext 的事件处理逻辑，但把结果缓存而不是立即发送。
    pub fn process_and_buffer(&mut self, event: &crate::kiro::model::events::Event) {
        // C4:已超缓冲上限 → 停止继续缓冲(丢弃后续事件),防 OOM。已置截断失败态,收尾按截断处理。
        if self.buffer_overflowed {
            return;
        }

        // 首次处理事件时，先生成初始事件（message_start 等）
        if !self.initial_events_generated {
            let initial_events = self.inner.generate_initial_events();
            self.buffered_bytes += Self::estimate_events_bytes(&initial_events);
            self.event_buffer.extend(initial_events);
            self.initial_events_generated = true;
        }

        // 处理事件并缓冲结果
        let events = self.inner.process_kiro_event(event);
        self.buffered_bytes += Self::estimate_events_bytes(&events);
        self.event_buffer.extend(events);

        // C4:累计字节超上限 → 按"响应截断"处置(复用 decoder_stopped 收尾:发 SSE error,
        // 不把半截缓冲当成功)。只置一次,后续事件在上面的 early-return 丢弃。
        if self.buffered_bytes > MAX_BUFFERED_EVENT_BYTES {
            self.buffer_overflowed = true;
            tracing::warn!(
                buffered_bytes = self.buffered_bytes,
                limit = MAX_BUFFERED_EVENT_BYTES,
                "缓冲流事件超出内存上限,按响应截断处置(停止继续缓冲,防 OOM)"
            );
            self.inner
                .mark_decoder_stopped("缓冲流事件超出内存上限(疑似异常超长响应)".to_string());
        }
    }

    /// 完成流处理并返回所有事件
    ///
    /// 此方法会：
    /// 1. 生成最终事件（message_delta, message_stop）
    /// 2. 用正确的 input_tokens 更正 message_start 事件
    /// 3. 返回所有缓冲的事件
    pub fn finish_and_get_all_events(&mut self) -> Vec<SseEvent> {
        // 如果从未处理过事件，也要生成初始事件
        if !self.initial_events_generated {
            let initial_events = self.inner.generate_initial_events();
            self.event_buffer.extend(initial_events);
            self.initial_events_generated = true;
        }

        // 生成最终事件
        let final_events = self.inner.generate_final_events();
        self.event_buffer.extend(final_events);

        // 获取正确的 input_tokens
        let final_input_tokens = self
            .inner
            .context_input_tokens
            .unwrap_or(self.estimated_input_tokens);
        // 剔除 cache 读写得到 billed 口径（与 message_delta 保持一致）
        let cache_usage = self.inner.cache_usage;
        let billed = cache_usage
            .map(|c| {
                billed_input_tokens(
                    final_input_tokens,
                    c.cache_creation_input_tokens,
                    c.cache_read_input_tokens,
                )
            })
            .unwrap_or(final_input_tokens);

        // 更正 message_start 事件中的 input_tokens（并补齐 cache 字段）
        for event in &mut self.event_buffer {
            if event.event == "message_start" {
                if let Some(message) = event.data.get_mut("message") {
                    if let Some(usage) = message.get_mut("usage") {
                        usage["input_tokens"] = serde_json::json!(billed);
                        if let Some(c) = cache_usage {
                            usage["cache_creation_input_tokens"] =
                                serde_json::json!(c.cache_creation_input_tokens);
                            usage["cache_read_input_tokens"] =
                                serde_json::json!(c.cache_read_input_tokens);
                        }
                    }
                }
            }
        }

        std::mem::take(&mut self.event_buffer)
    }
}

/// 简单的 token 估算
fn estimate_tokens(text: &str) -> i32 {
    let chars: Vec<char> = text.chars().collect();
    let mut chinese_count = 0;
    let mut other_count = 0;

    for c in &chars {
        if *c >= '\u{4E00}' && *c <= '\u{9FFF}' {
            chinese_count += 1;
        } else {
            other_count += 1;
        }
    }

    // 中文约 1.5 字符/token，英文约 4 字符/token
    let chinese_tokens = (chinese_count * 2 + 2) / 3;
    let other_tokens = (other_count + 3) / 4;

    (chinese_tokens + other_tokens).max(1)
}

/// 判定一段字符串是否为一个**完整合法**的 JSON 值（对象/数组/标量均可）。
/// 用于 `merge_tool_input` 识别「非前缀重写」：两帧各自都完整时不能追加。
fn is_complete_json(s: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(s).is_ok()
}

/// 非法工具参数 JSON 的缺陷归因（**纯可观测**，只写日志，绝不进控制流）。
///
/// 服务于「修不好的残留按责任方分流」定位真因：
/// - `Truncated`：结构未闭合（缺 `}`/`]` 或字符串未收尾）→ 指向上游 Kiro 截断/超时/网络侧，可查。
/// - `IllegalChars`：含非法转义（`\x`/`\U` 等）或裸控制符 → 指向模型侧生成异常，网关只能缓解。
/// - `TruncatedAndIllegal`：两者兼有。
/// - `Malformed`：结构闭合但仍非法（如 `}{` 粘连、键后无值）→ 归为其它畸形。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolJsonDefect {
    Truncated,
    IllegalChars,
    TruncatedAndIllegal,
    Malformed,
}

impl ToolJsonDefect {
    /// 供日志字段用的稳定短标签。
    fn as_str(&self) -> &'static str {
        match self {
            ToolJsonDefect::Truncated => "truncated",
            ToolJsonDefect::IllegalChars => "illegal_chars",
            ToolJsonDefect::TruncatedAndIllegal => "truncated_and_illegal",
            ToolJsonDefect::Malformed => "malformed",
        }
    }
}

/// 单遍 string-aware 扫描的诊断计数（判据与两层 repair 一一对应，不新造语义）。
struct ToolJsonScan {
    /// 结构未闭合：括号栈非空，或扫描结束仍在字符串内。
    truncated: bool,
    /// 含 JSON 非法转义（非九种合法转义）或裸控制符。
    illegal_chars: bool,
    /// 出现 `}{` / `} {` 粘连（非前缀双对象特征）。
    glued: bool,
}

/// 截断跨轮恢复的**纯决策**。三条件同时满足才触发：
/// 1. `recovery_on`：截断恢复开关开（默认关）。
/// 2. `repair_on`：JSON 修复层**已启用**——恢复的语义前提是「修复层也补不回」。修复层关时无法断言
///    此截断不可修（很多截断如未闭合字符串其实能被结构层补全），故修复层关则不触发，退回②/③原语义。
/// 3. 归因为真截断（Truncated / TruncatedAndIllegal）——非截断畸形（IllegalChars/Malformed）不归本开关管。
///
/// 抽成纯函数以便离线测试判据，避免在并行测试里 set/get 进程级开关造成互相污染。
fn should_recover_truncation(defect: ToolJsonDefect, recovery_on: bool, repair_on: bool) -> bool {
    recovery_on
        && repair_on
        && matches!(
            defect,
            ToolJsonDefect::Truncated | ToolJsonDefect::TruncatedAndIllegal
        )
}

/// 对已知非法的工具参数串做单遍扫描并归因。只在 `flush_tool_input` 的 `from_str` 已失败分支调用。
fn classify_tool_json_defect(s: &str) -> ToolJsonDefect {
    let scan = scan_tool_json(s);
    match (scan.truncated, scan.illegal_chars) {
        (true, true) => ToolJsonDefect::TruncatedAndIllegal,
        (true, false) => ToolJsonDefect::Truncated,
        (false, true) => ToolJsonDefect::IllegalChars,
        (false, false) => ToolJsonDefect::Malformed,
    }
}

/// 【纯诊断·不进控制流】把归因为 `Malformed`(结构闭合+无非法字符但仍 parse 失败)的串**再细分**成
/// 可区分的子型,供日志定性「malformed 到底是哪种畸形」——这是解开「类型 A(上游模型帧本身吐坏,
/// 网关修不了)还是类型 C(我们合并逻辑把好帧拼坏,能修)」的钥匙:
///   - `glued`      : `}{` / `} {` 粘连(两个完整对象黏一起)——偏类型 C(合并/上游重写),`merge_tool_input`
///                     第 6 步本应消灭,若仍出现说明帧序列走了未覆盖分支,**优先怀疑我们侧**。
///   - `trailing_comma` : 尾逗号 `{"a":1,}` / `[1,2,]`——偏模型侧多吐一个逗号。
///   - `missing_comma`  : 缺分隔逗号 `{"a":1"b":2}`——偏模型侧漏吐逗号/上游丢了逗号帧。
///   - `expected_value` : 键后/数组位缺值 `{"a":}`——偏模型侧生成中断在值前。
///   - `expected_colon` : 键后缺冒号 `{"a" 1}`——偏模型侧。
///   - `key_not_string` : 键不是字符串 `{a:1}`——偏模型侧吐了裸键。
///   - `trailing_chars` : 首个完整值后还有多余字符(非 `}{` 粘连的其它尾随)——偏上游多发。
///   - `other`          : serde 消息未落入上述已知形态(留观测,收够样本再补分类)。
///
/// 判据源 = serde_json 的**官方错误 Display 消息**(稳定字符串,非自造启发式) + `scan.glued`(已算出)。
/// 只在 Malformed 分支调用(调用方保证 `from_str` 已失败),纯读不改内容。
fn malformed_subkind(s: &str) -> &'static str {
    // glued 优先:scan 已单遍算出 `}{` 粘连,语义比 serde 的 "trailing characters" 更精确。
    if scan_tool_json(s).glued {
        return "glued";
    }
    // 取 serde 官方错误消息做形态判据(Display 文本稳定,见 serde_json/src/error.rs ErrorCode::fmt)。
    let msg = match serde_json::from_str::<serde_json::Value>(s) {
        Ok(_) => return "other", // 理论不可达(调用方已确保非法),兜底不 panic。
        Err(e) => e.to_string(),
    };
    // 匹配官方消息里的稳定短语(全小写包含匹配,不依赖行列号)。
    if msg.contains("trailing comma") {
        "trailing_comma"
    } else if msg.contains("expected `:`") {
        "expected_colon"
    } else if msg.contains("expected `,` or `}`") || msg.contains("expected `,` or `]`") {
        // serde 在缺分隔逗号时报「expected `,` or `}`/`]`」——即两个值之间少了逗号。
        "missing_comma"
    } else if msg.contains("expected value") {
        "expected_value"
    } else if msg.contains("key must be a string") {
        "key_not_string"
    } else if msg.contains("trailing characters") {
        "trailing_chars"
    } else {
        "other"
    }
}

/// 单遍 string-aware 扫描，判据与 repair 层同源（合法转义集 `" \ / b f n r t u`，其余非法；
/// 裸控制符 <0x20 非法；串未闭合 / 括号栈非空 / 末尾悬空转义 = 截断）。只归因，不改内容。
fn scan_tool_json(s: &str) -> ToolJsonScan {
    let mut in_string = false;
    let mut escaped = false;
    let mut depth: i32 = 0;
    let mut illegal_chars = false;
    let mut glued = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            if escaped {
                if !matches!(c, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u') {
                    illegal_chars = true;
                }
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            } else if (c as u32) < 0x20 {
                illegal_chars = true;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if c == '}' {
                    let mut look = chars.clone();
                    while let Some(&n) = look.peek() {
                        if n.is_whitespace() {
                            look.next();
                        } else {
                            if n == '{' {
                                glued = true;
                            }
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let truncated = in_string || depth > 0 || escaped;
    ToolJsonScan {
        truncated,
        illegal_chars,
        glued,
    }
}

/// 尝试把模型吐出的**非法 JSON 工具参数**修成合法 JSON（根治 `Invalid tool parameters`）。
///
/// # 为什么做这个（对着 Claude Code 客户端源码 + 官方 issue 的确凿依据）
/// 客户端（2.1.207）拿到累积的 `partial_json` 后直接 `JSON.parse`（`Rq`+`JSON.parse`，仅剥 BOM、
/// **不做任何修复**），parse 失败即包成 `{__unparsedToolInput:{raw,len}}` → 渲染成 "Invalid tool
/// parameters"。官方源码 `HLy` 明列三类成因：**未转义反斜杠 / 未转义控制符 / 截断输出**，且
/// 对应 issue（#69522 长 unicode 转义、#20015 Windows 路径反斜杠、#29715 smart quote/控制符）
/// 全部 Open/not-planned——**官方不修**。这些请求经本网关时，我们在发给客户端前把坏 JSON 修好，
/// 客户端就能 parse 成功，"Invalid tool parameters" 从本侧消失。
///
/// # 安全契约（调用方 `flush_tool_input` 已保证 + 本函数复验）
/// - **只在 `from_str` 已失败时调用**：合法 JSON 永不进入本函数（对正常流零影响）。
/// - **修复后必须复验**：返回 `Some` 当且仅当修复结果 `from_str` 通过；修不好返回 `None`，
///   调用方退回现状（原样透传），**最坏情况 == 修复前行为**，不会更糟。
/// - **只修字符级噪声，绝不臆测语义**：仅转义字符串内的非法转义/裸控制符、补全结构截断，
///   不新增/删除/改写任何键值语义。
///
/// 整包双重编码解包(洞1):工具 input 契约上顶层必是 object。模型偶发把整个参数对象**再套一层
/// JSON 字符串编码**(double-encoded),如发出 `"{\"path\":\"a\"}"` 而非 `{"path":"a"}`——此时
/// `from_str` 会**成功**得到 `Value::String`,但客户端按 object 消费该工具参数就报
/// InputValidationError(参数类型不符)。这类**漏过 repair 层**(它 from_str 成功、不进修复)。
///
/// 本函数在 from_str **成功**后调用:若解析结果是 `Value::String(inner)` 且 `inner` 本身能再
/// parse 成 object/array,返回解一层后的合法 JSON 串;否则 `None`(不动)。
///
/// # 铁律
/// - **只解一层**:深层嵌套(`"\"nested\""`)保守不碰,避免过度解包改语义。
/// - **复验必 object/array 才用**:顶层数字/布尔/纯字符串 → None(工具 input 顶层不该是标量,
///   但也不臆测,原样交上层)。零语义损失(只是剥掉误加的一层字符串编码)。
pub(crate) fn unwrap_double_encoded(s: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let inner = v.as_str()?; // 顶层必须是 JSON 字符串才可能是双重编码
    let reparsed: serde_json::Value = serde_json::from_str(inner).ok()?;
    // 只有解出 object/array 才认定是"误套一层"的双重编码;标量不动。
    if reparsed.is_object() || reparsed.is_array() {
        serde_json::to_string(&reparsed).ok()
    } else {
        None
    }
}

/// 返回修复后的合法 JSON 串；无法修成合法则 `None`。
pub(crate) fn repair_tool_json(s: &str) -> Option<String> {
    // 空串不在本函数职责内（flush_tool_input 上游已处理空串），保守拒绝。
    if s.trim().is_empty() {
        return None;
    }
    // 第一层：字符级修复（转义字符串内非法转义 + 裸控制符）。
    let char_fixed = repair_json_char_level(s);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&char_fixed) {
        return serde_json::to_string(&v).ok();
    }
    // 第二层：在字符级修复基础上再补全结构截断（缺 `}` / `]` / 收尾 `"`）。
    let struct_fixed = repair_json_structure(&char_fixed);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&struct_fixed) {
        return serde_json::to_string(&v).ok();
    }
    // 第三层：glued 粘连修复（`}{...}` 头部多余 `}` 来自上一个 JSON 对象泄漏）。
    // 仅在前两层均失败后触发，保守策略：剥离头部 `}` 及其后直到下一个 `{` 之间的垃圾字符，
    // 剩余部分必须能被 serde_json 解析为合法 JSON 才采用，否则放弃（不比前两层差）。
    if let Some(glued_fixed) = repair_json_glued(s) {
        return Some(glued_fixed);
    }
    None
}

/// glued 粘连修复：剥掉头部多余的 `}` 及其后到下一个 `{` 之间的任意字符。
///
/// 例：`}{\"path\": \"src/foo.rs\"}` → `{\"path\": \"src/foo.rs\"}`
///
/// 保守策略：
/// - 只在原串（trim 后）以 `}` 开头时尝试。
/// - 找到第一个 `{` 并截取子串，子串必须 serde_json 解析成功才返回，否则 `None`。
/// - 不修改原有字符级/结构级修复的结果，仅作为最后兜底。
fn repair_json_glued(s: &str) -> Option<String> {
    let trimmed = s.trim_start();
    if !trimmed.starts_with('}') {
        return None;
    }
    // 找到第一个 '{' 的位置（在原串中，而非 trim 后偏移，保持 slice 安全）。
    let brace_pos = s.find('{')?;
    let candidate = &s[brace_pos..];
    // 对候选串先做字符级修复再验证，与前两层保持一致的处理质量。
    let char_fixed = repair_json_char_level(candidate);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&char_fixed) {
        return serde_json::to_string(&v).ok();
    }
    // 字符级修复后仍不合法，再尝试结构层补全（截断 + glued 同时出现的罕见情况）。
    let struct_fixed = repair_json_structure(&char_fixed);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&struct_fixed) {
        return serde_json::to_string(&v).ok();
    }
    None
}

/// JSON 字符级修复：状态机扫描，**只修字符串字面量内部**的非法字符，结构字符（`{}[]:,` 等）原样保留。
///
/// 修两类（对应客户端 `HLy` 列的成因）：
/// 1. **裸控制符**（U+0000..=U+001F 未转义，如真实换行/制表符混进字符串值）→ 转义成 `\n`/`\t`/`\uXXXX`。
/// 2. **非法反斜杠转义**：JSON 只认 `\" \\ \/ \b \f \n \r \t \uXXXX` 九种。其它 `\x` 一律非法——
///    - `\U`（Windows 路径 `C:\Users` 的典型泄漏）、`\x41`、`\.`、行尾孤立 `\` 等 → 把该反斜杠**再转义**
///      成 `\\`（还原成"字面反斜杠 + 原字符"，这是模型本想表达路径/字面量时的正确 JSON）。
///    - `\uXXXX` 若后随不足 4 位 hex（截断）→ 同样降级成字面 `\\u...`，交由结构层或复验兜底。
///
/// 字符串外的字符原样透传（不碰任何结构）。非字符串区的裸控制符（缩进空白等）JSON 本就允许，不动。
fn repair_json_char_level(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    let mut chars = s.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if !in_string {
            // 结构区：只关心进入字符串的 `"`，其余原样。
            out.push(c);
            if c == '"' {
                in_string = true;
            }
            continue;
        }
        // 字符串内：
        match c {
            '"' => {
                // 字符串结束（未转义的引号）。
                out.push(c);
                in_string = false;
            }
            '\\' => {
                // 转义序列：看下一个字符决定合法性。
                match chars.next() {
                    None => {
                        // 行尾孤立反斜杠 → 转义成字面反斜杠。
                        out.push_str("\\\\");
                    }
                    Some(esc) => match esc {
                        // 九种合法转义原样保留。
                        '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' => {
                            out.push('\\');
                            out.push(esc);
                        }
                        'u' => {
                            // 必须后随 4 位 hex，否则截断 → 降级字面。
                            let mut hex = String::new();
                            for _ in 0..4 {
                                match chars.peek() {
                                    Some(h) if h.is_ascii_hexdigit() => {
                                        hex.push(*h);
                                        chars.next();
                                    }
                                    _ => break,
                                }
                            }
                            if hex.len() == 4 {
                                // 洞4:UTF-16 代理对完整性(对应 #69522 长 unicode 转义 parse 失败)。
                                // serde_json 只接受**成对**的代理(高 D800-DBFF 紧跟低 DC00-DFFF);
                                // 孤立高代理 / 孤立低代理会被判非法 JSON。这里:
                                //   - 高代理 + 后随合法低代理 → 原样保留(合法 emoji 如 😀 不碰);
                                //   - 高代理但后面不是合法低代理 → 孤立,降级字面 \\uXXXX;
                                //   - 直接遇到低代理(没被前面的高代理配对消费) → 孤立,降级字面。
                                let cp = u32::from_str_radix(&hex, 16).unwrap_or(0);
                                if (0xD800..=0xDBFF).contains(&cp) {
                                    // 高代理:向前看是否紧跟 \uYYYY 且 YYYY 是合法低代理。
                                    if let Some(low_hex) = peek_low_surrogate(&mut chars) {
                                        out.push_str("\\u");
                                        out.push_str(&hex);
                                        out.push_str("\\u");
                                        out.push_str(&low_hex);
                                    } else {
                                        // 孤立高代理 → 降级字面。
                                        out.push_str("\\\\u");
                                        out.push_str(&hex);
                                    }
                                } else if (0xDC00..=0xDFFF).contains(&cp) {
                                    // 孤立低代理(合法对已在高代理分支被整体消费,能到这里必是孤立)→ 降级字面。
                                    out.push_str("\\\\u");
                                    out.push_str(&hex);
                                } else {
                                    // BMP 普通码位 → 原样保留。
                                    out.push_str("\\u");
                                    out.push_str(&hex);
                                }
                            } else {
                                // 截断的 \uXX → 字面反斜杠 + u + 已收集 hex。
                                out.push_str("\\\\u");
                                out.push_str(&hex);
                            }
                        }
                        // 非法转义（\U \x \. 等）→ 反斜杠降级字面，原字符正常写入
                        // （若原字符本身是控制符，落入下方 push_escaped_char 再处理）。
                        other => {
                            out.push_str("\\\\");
                            push_escaped_char(&mut out, other);
                        }
                    },
                }
            }
            // 裸控制符 → 转义。
            c if (c as u32) < 0x20 => {
                push_escaped_char(&mut out, c);
            }
            // 普通字符原样。
            _ => out.push(c),
        }
    }
    out
}

/// 向前看：紧接的是否为 `\uYYYY` 且 YYYY 是合法低代理(DC00-DFFF)。是则**消费**这 6 个字符
/// (`\` `u` + 4 hex)并返回 `Some("YYYY")`;否则不消费任何字符、返回 `None`。
/// 用于 [`repair_json_char_level`] 判定高代理后是否紧跟合法低代理(合法代理对整体保留)。
fn peek_low_surrogate(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<String> {
    // clone 迭代器做无损前瞻:先在 clone 上验证,确认合法才在真迭代器上消费。
    let mut look = chars.clone();
    if look.next() != Some('\\') {
        return None;
    }
    if look.next() != Some('u') {
        return None;
    }
    let mut hex = String::new();
    for _ in 0..4 {
        match look.next() {
            Some(h) if h.is_ascii_hexdigit() => hex.push(h),
            _ => return None,
        }
    }
    let cp = u32::from_str_radix(&hex, 16).ok()?;
    if (0xDC00..=0xDFFF).contains(&cp) {
        // 合法低代理 → 在真迭代器上消费掉这 6 个字符(\ u + 4 hex)。
        for _ in 0..6 {
            chars.next();
        }
        Some(hex)
    } else {
        None
    }
}

/// 把一个字符写进输出：控制符转义成 JSON 合法形式（`\n`/`\t`/`\uXXXX`），其余原样。
fn push_escaped_char(out: &mut String, c: char) {
    match c {
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\u{08}' => out.push_str("\\b"),
        '\u{0C}' => out.push_str("\\f"),
        c if (c as u32) < 0x20 => {
            out.push_str(&format!("\\u{:04x}", c as u32));
        }
        _ => out.push(c),
    }
}

/// JSON 结构补全：针对**截断**（流被上游/网络在中途切断，缺尾部 `"`/`}`/`]`）。
///
/// 单遍扫描跟踪：是否在字符串内、括号栈（`{`/`[`）。扫完若仍在字符串内先补收尾 `"`，
/// 再按栈逆序补 `}`/`]`。假设**输入已过字符级修复**（转义已合法），故这里只需按结构闭合。
/// 保守边界：若结尾停在"键后无值"或"逗号后无元素"这类语义残缺处，闭合后仍非法 → 交由
/// 调用方复验 `from_str` 拒绝（返回 None 退回透传）。不猜测缺失的值，只补闭合符号。
fn repair_json_structure(s: &str) -> String {
    let mut in_string = false;
    let mut escaped = false;
    let mut stack: Vec<char> = Vec::new();
    for c in s.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    let mut out = s.to_string();
    // 末尾悬空转义符（单个 `\`）→ 补成字面反斜杠，避免收尾 `"` 被它吃掉。
    if escaped {
        out.push('\\');
    }
    // 仍在字符串内 → 先闭合字符串。
    if in_string {
        out.push('"');
    }
    // 逆序补齐未闭合的括号。
    while let Some(closer) = stack.pop() {
        out.push(closer);
    }
    out
}

/// 工具帧探针总开关（环境变量 `KIRO_TOOL_TRACE` 非空即开）。用 `OnceLock` 缓存，避免每帧读环境变量。
///
/// 这是**常驻代码**的诊断探针（非临时旁挂），用于坐实 `Invalid tool parameters` 真因：
/// dwgx 现场复现时设 `KIRO_TOOL_TRACE=1` 重启网关，即可抓到上游 `toolUseEvent.input` 的**逐帧原文**
/// 与 `merge_tool_input` 的合并轨迹，据此定性：
///   - **类型 C（网关侧，已修）**：原始帧序列里出现「非前缀双完整对象」等，合并后仍为合法 JSON；
///   - **类型 A（模型抽风，网关修不了）**：某原始帧本身就含非法转义 / 乱码控制 token，
///     拼装后 `flush_tool_input` 报「非合法 JSON」——此时网关只能如实透传，责任在上游模型。
/// 平时零开销（未设环境变量时 `tool_trace_enabled()` 恒 false，探针整体短路）。
fn tool_trace_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("KIRO_TOOL_TRACE")
            .map(|v| !v.trim().is_empty() && v != "0")
            .unwrap_or(false)
    })
}

/// 文本化工具调用诊断探针总开关(环境变量 `KIRO_INVOKE_TRACE` 非空即开)。平时零开销。
/// 开启时,assistantResponseEvent 文本流里出现工具调用标记(文本化 invoke)即记一条现场语料,
/// 用于坐实「模型把工具调用当纯文本吐出」现象(#70544 变体,致客户端断连)。
fn invoke_trace_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("KIRO_INVOKE_TRACE")
            .map(|v| !v.trim().is_empty() && v != "0")
            .unwrap_or(false)
    })
}

// ============================================================================
// 文本化 invoke 解析纯函数集（从 ZyphrZero/kiro.rs 移植，逐字保真逻辑）
//
// 这批函数全部是纯函数：不触碰 StreamContext / 任何可变状态，只对入参字符串做
// 结构解析。用于从「模型把工具调用当纯文本吐出」的退化输出（#70544 变体）里把
// `<invoke name="...">...<parameter ...>...</parameter>...</invoke>` 结构捞回。
// 复用本文件既有的 `QUOTE_CHARS` / `is_quote_char`（与 kiro.rs 完全一致）。
//
// 本阶段只落地函数 + 单测（隔离验证），暂不接入任何状态机。
// ============================================================================

/// 检查 `name_pos`（指向标签名首字母）的前面是否构成合法的开标签起始，
/// 兼容裸写法 `<tag` 和带命名空间前缀的写法 `<prefix:tag`。
///
/// 返回 `Some(lt_pos)`（指向 `<` 的字节位置）表示合法；`None` 表示不是标签。
///
/// 注：本阶段这批 invoke 解析纯函数仅落地 + 单测隔离验证，尚未接入状态机，
/// 故统一 `#[allow(dead_code)]`；后续接线阶段移除。
#[allow(dead_code)]
fn open_tag_lt_pos(buffer: &str, name_pos: usize) -> Option<usize> {
    let bytes = buffer.as_bytes();
    if name_pos == 0 {
        return None;
    }
    let prev = bytes[name_pos - 1];
    if prev == b'<' {
        return Some(name_pos - 1);
    }
    // 形如 `<prefix:tag`：name 前面是 ':'，再往前是一段标识符，再往前是 '<'
    if prev == b':' {
        let i = name_pos - 1; // 指向 ':'
        let mut j = i; // 标识符左边界扫描
        while j > 0 && {
            let c = bytes[j - 1];
            c.is_ascii_alphanumeric() || c == b'_'
        } {
            j -= 1;
        }
        // 标识符非空，且其左边是 '<'
        if j < i && j > 0 && bytes[j - 1] == b'<' {
            return Some(j - 1);
        }
    }
    None
}

/// 查找未被引用字符包裹的 invoke 开标签，返回指向 `<` 的字节位置
///
/// 兼容裸 `<invoke ...>` 与带命名空间前缀 `<prefix:invoke ...>` 两种写法。
/// 复用 `is_quote_char`：若 `<` 前紧贴反引号/引号等包裹字符，视为引用，跳过。
#[allow(dead_code)]
fn find_invoke_start(buffer: &str) -> Option<usize> {
    let mut search = 0;
    while let Some(rel) = buffer[search..].find("invoke") {
        let name_pos = search + rel;
        if let Some(lt) = open_tag_lt_pos(buffer, name_pos) {
            // 标签名后必须是边界字符（空白或 '>'），避免误匹配 invoked 之类
            let after = name_pos + "invoke".len();
            let next_ok = buffer.as_bytes().get(after).map_or(true, |c| {
                c.is_ascii_whitespace() || *c == b'>' || *c == b'/'
            });
            let has_quote_before = lt > 0 && is_quote_char(buffer, lt - 1);
            if next_ok && !has_quote_before {
                return Some(lt);
            }
        }
        search = name_pos + "invoke".len();
    }
    None
}

/// 从 `start` 之后查找第一个 invoke 闭标签，返回结束位置（exclusive，含闭标签）
///
/// 兼容裸 `</invoke>` 与带前缀 `</prefix:invoke>`。找不到返回 `None`（块还没到齐）。
#[allow(dead_code)]
fn find_invoke_block_end(buffer: &str, start: usize) -> Option<usize> {
    // 块 A 的边界 = 下一个 `<invoke` 开标签（即下一个块 B 的起点），没有则到 buffer 结尾。
    // 这样连发 burst（A 紧跟 B）时，A 的搜索区间被 B 的开标签卡住，绝不会吃进 B。
    let boundary = match find_next_invoke_open(buffer, start) {
        Some(p) => p,
        None => buffer.len(),
    };
    // 在 [start, boundary) 区间里取【最后一个】 `</invoke>` 作为真闭合。
    // 贪婪取最后一个 → patch 正文里出现的字面 `</invoke>` 不会导致提前截断；
    // 区间被下一个块开标签卡住 → 不会跨块误合并。
    find_last_invoke_close(buffer, start, boundary)
}

/// 从 `start` 之后查找下一个真正的 `<invoke`（或 `<prefix:invoke`）开标签的字节位置。
/// 跳过 `start` 处当前块自身的开标签。
#[allow(dead_code)]
fn find_next_invoke_open(buffer: &str, start: usize) -> Option<usize> {
    // 先跳过当前块的开标签：从 start 之后第一个 '>' 之后开始找。
    let after_open = match buffer[start..].find('>') {
        Some(rel) => start + rel + 1,
        None => return None,
    };
    // 注意：不能复用 find_invoke_start——它对 `<` 前是 `>`（引用字符）的情况会拒绝，
    // 而连发 burst 里 B 的 `<invoke` 恰好紧跟在 A 的 `</invoke>` 的 `>` 后面。
    // 这里只认结构：`<invoke` 或 `<prefix:invoke`，开标签名后须是空白/`>`/`/` 边界。
    let region = &buffer[after_open..];
    let mut search = 0usize;
    while let Some(rel) = region[search..].find("invoke") {
        let name_pos = search + rel;
        if let Some(lt) = open_tag_lt_pos(region, name_pos) {
            let after = name_pos + "invoke".len();
            let next_ok = region.as_bytes().get(after).map_or(true, |c| {
                c.is_ascii_whitespace() || *c == b'>' || *c == b'/'
            });
            if next_ok {
                return Some(after_open + lt);
            }
        }
        search = name_pos + "invoke".len();
    }
    None
}

/// 在 `[from, boundary)` 区间内查找最后一个 `</invoke>` / `</prefix:invoke>` 的结束位置
/// （exclusive，含闭标签）。找不到返回 `None`（块还没到齐）。
#[allow(dead_code)]
fn find_last_invoke_close(buffer: &str, from: usize, boundary: usize) -> Option<usize> {
    let region_end = boundary.min(buffer.len());
    if from >= region_end {
        return None;
    }
    let region = &buffer[from..region_end];
    let bytes = region.as_bytes();
    let mut search = 0usize;
    let mut last: Option<usize> = None;
    while let Some(rel) = region[search..].find("invoke>") {
        let name_pos = search + rel;
        // '</invoke>' 形式
        if name_pos >= 2 && &region[name_pos - 2..name_pos] == "</" {
            last = Some(from + name_pos + "invoke>".len());
        } else if name_pos >= 1 && bytes[name_pos - 1] == b':' {
            // '</prefix:invoke>' 形式
            let mut j = name_pos - 1; // ':'
            while j > 0 && {
                let c = bytes[j - 1];
                c.is_ascii_alphanumeric() || c == b'_'
            } {
                j -= 1;
            }
            if j >= 2 && &region[j - 2..j] == "</" {
                last = Some(from + name_pos + "invoke>".len());
            }
        }
        search = name_pos + "invoke>".len();
    }
    last
}

/// 从标签字符串中抠出 `name="..."` 的值（取第一个匹配）
#[allow(dead_code)]
fn extract_name_attr(tag: &str) -> Option<String> {
    let needle = "name=\"";
    let rel = tag.find(needle)?;
    let start = rel + needle.len();
    let end_rel = tag[start..].find('"')?;
    Some(tag[start..start + end_rel].to_string())
}

/// 解析一个完整 invoke 块，抠出 (tool_name, input_json_string)
///
/// - tool name 来自 invoke 开标签的 `name="..."`（兼容 antml: 前缀）
/// - 参数为零个或多个 `<parameter name="K">V</parameter>`（兼容前缀）
/// - 参数值取到下一个参数开标签前的**最后一个** `</parameter>` 为界（贪婪），
///   允许多行 / 含 `<` / 中文 / 含字面 `</parameter>`（P0-1 修复）
/// - 用 serde_json 拼成 object（值都是字符串，自动转义）
/// - 无合法 name 或拼不出合法 JSON 返回 `None`
#[allow(dead_code)]
fn parse_invoke_block(block: &str) -> Option<(String, String)> {
    // invoke 开标签 = 块开头到第一个 '>'
    let open_end = block.find('>')?;
    let open_tag = &block[..=open_end];
    let tool_name = extract_name_attr(open_tag)?;
    if tool_name.is_empty() {
        return None;
    }

    let mut map = serde_json::Map::new();
    let body = &block[open_end + 1..];
    let mut cursor = 0usize;
    while let Some(rel) = body[cursor..].find("parameter name=\"") {
        let name_kw = cursor + rel;
        // 确认是真正的 '<parameter' 或 '<prefix:parameter' 开标签
        // name_kw 指向 'parameter'，往前应是 '<' 或 '<prefix:'
        // 确认是真正的开标签（'<parameter' / '<prefix:parameter'）；仅用于校验，不需要位置值
        if open_tag_lt_pos(body, name_kw).is_none() {
            cursor = name_kw + "parameter".len();
            continue;
        }
        // 找该参数开标签的 '>'
        let tag_gt = match body[name_kw..].find('>') {
            Some(r) => name_kw + r,
            None => break, // 开标签未闭合，停止
        };
        let param_open_tag = &body[name_kw..tag_gt + 1];
        // 从 'parameter name="..."' 抠 key（剥掉前缀干扰：直接找 name="）
        let key = match extract_name_attr(param_open_tag) {
            Some(k) => k,
            None => {
                cursor = tag_gt + 1;
                continue;
            }
        };
        // 参数值取到 </parameter>（兼容前缀）为界。find_param_close 较贵，只调一次，
        // 同时复用 (闭标签起始, 闭标签结束) 两个值：起始用于切值，结束用于推进游标。
        let val_start = tag_gt + 1;
        let (close_start, close_end) = match find_param_close(body, val_start) {
            Some(pair) => pair,
            None => break, // 值未闭合，停止
        };
        let value = &body[val_start..close_start];
        map.insert(key, serde_json::Value::String(value.to_string()));
        // 推进到闭标签之后
        cursor = close_end;
    }

    let obj = serde_json::Value::Object(map);
    let s = serde_json::to_string(&obj).ok()?;
    Some((tool_name, s))
}

/// 从 `from` 开始查找第一个 parameter 闭标签，返回 (起始位置, 结束位置 exclusive)
///
/// 兼容裸 `</parameter>` 与带前缀 `</prefix:parameter>`。
#[allow(dead_code)]
fn find_param_close(body: &str, from: usize) -> Option<(usize, usize)> {
    // P0-1：参数值（尤其 apply_patch 的 patch 正文）可能含字面 `</parameter>`。
    // 朴素「取第一个 </parameter>」会把值截断。改成「贪婪取边界内最后一个 </parameter>」：
    // 边界 = 下一个 `<parameter name="` 开标签（多参数场景），没有则到 body 结尾。
    // 这样：① 单参数（含 apply_patch）取到真正的最后一个闭合，内容里的字面闭合不误伤；
    //      ② 多参数仍按下一个参数开标签正确切分。
    // 局限（已诚实标注）：若参数值里同时含字面 `<parameter name="`，边界判定会偏早；
    // 实测 apply_patch 正文极少出现该字面串，可接受。
    let boundary = match find_next_param_open(body, from) {
        Some(p) => p,
        None => body.len(),
    };
    let region = &body[from..boundary];
    let kw = "parameter>";
    let mut last: Option<(usize, usize)> = None;
    let mut search = 0usize;
    let bytes = region.as_bytes();
    while let Some(rel) = region[search..].find(kw) {
        let name_pos = search + rel;
        // '</parameter>' 形式
        if name_pos >= 2 && &region[name_pos - 2..name_pos] == "</" {
            last = Some((from + name_pos - 2, from + name_pos + kw.len()));
        } else if name_pos >= 1 && bytes[name_pos - 1] == b':' {
            // '</prefix:parameter>' 形式
            let mut j = name_pos - 1; // ':'
            while j > 0 && {
                let c = bytes[j - 1];
                c.is_ascii_alphanumeric() || c == b'_'
            } {
                j -= 1;
            }
            if j >= 2 && &region[j - 2..j] == "</" {
                last = Some((from + j - 2, from + name_pos + kw.len()));
            }
        }
        search = name_pos + kw.len();
    }
    last
}

/// 从 `from` 开始查找下一个 `<parameter name="`（或 `<prefix:parameter name="`）开标签的字节位置。
/// 用于 `find_param_close` 的贪婪边界：当前参数值最多吃到下一个参数开标签之前。
#[allow(dead_code)]
fn find_next_param_open(body: &str, from: usize) -> Option<usize> {
    let mut search = from;
    while let Some(rel) = body[search..].find("parameter name=\"") {
        let kw_pos = search + rel;
        // 必须是真正的开标签：'parameter' 前面是 '<' 或 '<prefix:'
        if let Some(lt) = open_tag_lt_pos(body, kw_pos) {
            return Some(lt);
        }
        search = kw_pos + "parameter".len();
    }
    None
}

/// 剥掉块前文本尾部的独立 stray token 行（单独一行的 `call` / `count` / `card` / `court`）
///
/// 实测里 `<invoke>` 前常出现一行裸 `call`/`count`，需要从块前叙述文本里剥掉，
/// 避免泄漏给客户端。只剥“尾部、且独占一行”的 stray token，前面的正常叙述保留。
/// 已实测到的 stray token 集合：Opus 长上下文退化时，泄漏的 `<invoke>` 前常有一行裸的
/// `call` / `count` / `card`。集合形式便于以后扩充。
///
/// 生产语料（KiroStudio #70544 变体）里 `court` 是最主要的 stray token，故并入集合。
/// 中文变体 `課`/`课` 也是我们实测到的高置信泄漏词（见 LEAKED_CONTROL_TOKENS），一并纳入熔断计数，
/// 否则中文退化刷屏时逐字清洗能剥、但复读熔断（32 次截断止血）抓不到 → 仍会耗尽 max_tokens。
#[allow(dead_code)]
const STRAY_INVOKE_TOKENS: &[&str] = &["call", "count", "card", "court", "課", "课"];

/// thinking 缓冲上限(review Finding 5):上游持续吐纯空白时,纯空白分支既不 emit 也不收缩会让
/// thinking_buffer 无界增长 OOM。超此上限即强制按普通文本吐出收缩。256KiB 远超正常 thinking 前导空白。
const MAX_THINKING_BUFFER_BYTES: usize = 262_144;

/// 缓冲流(BufferedStreamContext,/cc + CC auto-buffer)累计事件字节上限(C4 修复):
/// 缓冲模式把整段上游流收进内存再更正 message_start 的 input_tokens。无上限时超长流式工具
/// 参数 + 大 thinking(或异常上游持续推送)会让 event_buffer 无界增长直至 OOM(对比 thinking
/// 已有 256KiB 上限、decoder 16MB 上限)。64MiB 远超任何正常 Claude 响应(含大工具参数与长
/// thinking),超限即按"响应截断"处置(mark_decoder_stopped),复用既有截断→SSE error 收尾语义,
/// 不再继续吃内存。
const MAX_BUFFERED_EVENT_BYTES: usize = 64 * 1024 * 1024;

/// 复读熔断阈值：同一个 stray token（call/count/card/court）连续作为独占一行重复出现
/// 超过这么多次，判定为「Opus 长上下文退化复读死循环」，立即熔断本轮文本输出。
///
/// 取值权衡：正常工具调用前最多出现 1 个引导词行（偶有 2~3），绝不会连续几十次。
/// 设为 32 远高于正常上限、又远低于退化时的数万次，既不误伤正常引导词，又能尽早止血。
#[allow(dead_code)]
const REPEAT_GUARD_TRIP_THRESHOLD: u32 = 32;

/// stray 泄漏观测词表(与 clean 层 LEAKED_CONTROL_TOKENS 对齐,纯观测用)。
const STRAY_OBSERVE_TOKENS: &[&str] = &[
    "court", "course", "count", "care", "card", "call", "課", "课",
];

/// 判断字符是否 CJK 表意文字(观测"stray 词紧贴 CJK"的判据,与 clean 层 is_leak_glue_char 同族)。
fn is_cjk_ideograph(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}')
}

/// 【纯观测】扫 content 里的 stray 泄漏形态,累加两类计数(**不修改 content**):
/// - standalone:某 stray 词**独占一整行**(trim 后整行 == 词)——高置信泄漏(court 实测全独占行)。
/// - inline:某 stray 词出现在句中且**紧贴 CJK 表意字**(如 `重读course课`/`值是count的`)——
///   正常中英混排会有空格分隔,紧贴 CJK 是泄漏特征。用于点亮 clean 层够不到的句中黑洞。
/// 快路径:先 contains 任一词才细扫,正常文本零开销。
fn observe_stray_leak_forms(content: &str, standalone: &mut u32, inline: &mut u32) {
    // 快路径:一个都不含直接返回。
    if !STRAY_OBSERVE_TOKENS.iter().any(|t| content.contains(*t)) {
        return;
    }
    // 独占行:逐行 trim 后整行等于某 stray 词。
    for line in content.split('\n') {
        let t = line.trim();
        if STRAY_OBSERVE_TOKENS.contains(&t) {
            *standalone = standalone.saturating_add(1);
        }
    }
    // 句中紧贴 CJK:词出现处,其紧邻(前或后)是 CJK 表意字。
    for tok in STRAY_OBSERVE_TOKENS {
        let tb = tok.as_bytes();
        let mut from = 0usize;
        while let Some(rel) = content[from..].find(*tok) {
            let start = from + rel;
            let end = start + tb.len();
            let before_cjk = content[..start]
                .chars()
                .next_back()
                .is_some_and(is_cjk_ideograph);
            let after_cjk = content[end..].chars().next().is_some_and(is_cjk_ideograph);
            if before_cjk || after_cjk {
                *inline = inline.saturating_add(1);
            }
            from = end;
        }
    }
}

/// 判断一个 trim 后的行是否"看起来像退化刷屏 token":短(≤6 字符)、且全为字母或全为 CJK 表意文字,
/// 无空格/标点/数字。用于逐行检测里放宽词表(不止已知的 call/count/card/court/課/课),
/// 但仍保守(要求整行就是这么个短纯词),正常句子/代码不会整行是这种。
fn is_short_flood_token(line: &str) -> bool {
    let n = line.chars().count();
    if n == 0 || n > 6 {
        return false;
    }
    let all_ascii_alpha = line.chars().all(|c| c.is_ascii_alphabetic());
    // CJK 统一表意文字区(含扩展 A):课/課 等中文单字刷屏。
    let all_cjk = line
        .chars()
        .all(|c| matches!(c, '\u{3400}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}'));
    all_ascii_alpha || all_cjk
}

/// ② 结构性洪水检测:**不依赖换行、不依赖词表**。扫描文本里"同一个短 token 连续紧邻重复"的最长游程,
/// 覆盖单行连写 "课课课…课" / "coursecoursecourse…" / 逐字符重复,任意退化词都抓。
/// 命中(游程 ≥ 阈值)返回该游程起点的字节偏移(从那里截断)。
///
/// 算法:对每个可能的 token 长度(1..=6 字符),检测是否有从某位置起、同一 token 连续重复 ≥阈值次。
/// 优先抓最靠前的命中点。中文单字(len=1 char)刷屏是最常见形态,单独快速扫一遍。
fn detect_structural_flood(text: &str) -> Option<usize> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let n = chars.len();
    if n < REPEAT_GUARD_TRIP_THRESHOLD as usize {
        return None;
    }
    let thresh = REPEAT_GUARD_TRIP_THRESHOLD as usize;
    // 单字符游程(最常见:中文"课"连写、单字母连写)。只对"字母或 CJK"的字符计游程,
    // 避免把正常重复(如 "----" 分隔线、"...")误判——那些是标点不在此列。
    let is_floodable = |c: char| {
        c.is_ascii_alphabetic() || matches!(c, '\u{3400}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}')
    };
    let mut i = 0usize;
    while i < n {
        let (byte_start, ch) = chars[i];
        if is_floodable(ch) {
            let mut j = i + 1;
            while j < n && chars[j].1 == ch {
                j += 1;
            }
            if j - i >= thresh {
                return Some(byte_start);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    // 多字符 token 连写(如 "coursecourse…"):对 token 长度 2..=6 char 滑窗检测连续相等块。
    for tok_len in 2..=6usize {
        if n < tok_len * thresh {
            continue;
        }
        let mut i = 0usize;
        while i + tok_len <= n {
            // 当前 token = chars[i..i+tok_len],要求全 floodable(纯词,不含空格标点)。
            if !chars[i..i + tok_len].iter().all(|(_, c)| is_floodable(*c)) {
                i += 1;
                continue;
            }
            let tok: Vec<char> = chars[i..i + tok_len].iter().map(|(_, c)| *c).collect();
            let mut reps = 1usize;
            let mut k = i + tok_len;
            while k + tok_len <= n
                && chars[k..k + tok_len]
                    .iter()
                    .map(|(_, c)| *c)
                    .eq(tok.iter().copied())
            {
                reps += 1;
                k += tok_len;
            }
            if reps >= thresh {
                return Some(chars[i].0);
            }
            i = if reps > 1 { k } else { i + 1 };
        }
    }
    None
}

/// 块级复读折叠：对「已完整的整段文本」做一次性复读熔断。
///
/// 用于非流式 / web_search loop 路径（`extract_invoke_content_blocks` 入口）——
/// 那条路不经过流式 `emit_text_delta_raw` 的逐 chunk 熔断，所以在这里独立兜一次。
///
/// 规则与流式版一致：同一个 `STRAY_INVOKE_TOKENS`（call/count/card/court）连续作为独占一行
/// 重复超过 `REPEAT_GUARD_TRIP_THRESHOLD` 次，判定为 Opus 退化复读，**从超阈值处截断**，
/// 丢弃其后的全部复读垃圾（断雪球、不灌历史）。阈值内的少量引导词重复原样保留。
#[allow(dead_code)]
fn collapse_stray_token_floods(text: &str) -> std::borrow::Cow<'_, str> {
    let mut last_line = "";
    let mut run: u32 = 0;
    let mut cut_at: Option<usize> = None;
    let mut offset = 0usize;
    for segment in text.split_inclusive('\n') {
        let line = segment.trim();
        if STRAY_INVOKE_TOKENS.contains(&line) {
            if line == last_line {
                run += 1;
            } else {
                last_line = line;
                run = 1;
            }
            if run >= REPEAT_GUARD_TRIP_THRESHOLD {
                // 从「本段（这一行）开头」截断：保留阈值内已累计的内容。
                cut_at = Some(offset);
                break;
            }
        } else if !line.is_empty() {
            last_line = line;
            run = 0;
        }
        offset += segment.len();
    }
    match cut_at {
        Some(pos) => std::borrow::Cow::Owned(text[..pos].to_string()),
        None => std::borrow::Cow::Borrowed(text),
    }
}

/// 剥掉块前文本尾部独占一行的 stray token（保留其前一行的换行）
#[allow(dead_code)]
fn strip_trailing_stray_tokens(before: &str) -> &str {
    let mut end = before.len();
    loop {
        let bytes = before.as_bytes();
        // 先跳过尾部的换行符，定位“最后一行”的真实结束位置
        let mut e = end;
        while e > 0 && (bytes[e - 1] == b'\n' || bytes[e - 1] == b'\r') {
            e -= 1;
        }
        let line_start = before[..e].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let last_line = before[line_start..e].trim();
        // Opus 长上下文退化时，泄漏的 <invoke> 前常有一个孤立的 stray token 行。
        // 实测样本里出现过 call / count / card / court；用集合便于以后扩充。
        if STRAY_INVOKE_TOKENS.contains(&last_line) {
            // 只剥 stray token 行本身，【保留】前一行末尾的换行符。
            // 旧实现用 line_start - 1 把前一行的换行也吞掉，会把前面的叙述正文和
            // 后续 <invoke> 挤到同一行，导致 invoke_looks_like_real_leak 的“行首”判定
            // 失败、漏捞真泄漏（narrative\ncall\n<invoke>）。改成 end = line_start：
            //   "some text\ncall" -> "some text\n"（行首信号保留）
            //   "call"（无前导正文）-> ""（line_start==0）
            end = line_start;
            if end == 0 {
                return "";
            }
        } else {
            break;
        }
    }
    &before[..end]
}

/// 判定一个 `<invoke>` 块到底像“真泄漏的工具调用”还是“正文里讨论的文本”
///
/// 实测真泄漏的 `<invoke>` 都出现在**行首**（前面是流的开头、或上一行已经换行结束），
/// 而正文讨论里的 `<invoke>` 一般**嵌在一句话中间**——前面同一行还有普通文字。
///
/// 判定规则（输入 `before` 是 `<invoke>` 之前、已剥过 stray token 的文本）：
/// - `before` 为空（`<invoke>` 在流开头）→ 像真泄漏，抓。
/// - `before` 去掉尾部空格/制表符后以换行结尾（`<invoke>` 独占新行）→ 抓。
/// - 否则（同一行前面还有非空白正文）→ 像讨论文本，不抓。
///
/// 注意：这里的“尾部空白”只剥行内空白（空格 / 制表符），不剥换行；
/// 换行结尾才是“另起一行”的信号。
#[allow(dead_code)]
fn invoke_looks_like_real_leak(before: &str) -> bool {
    // 剥掉尾部的行内空白（空格 / 制表符），但保留换行
    let trimmed = before.trim_end_matches([' ', '\t']);
    // 行首：要么前面什么都没有，要么上一行已经以换行结束
    trimmed.is_empty() || trimmed.ends_with('\n') || trimmed.ends_with('\r')
}

/// 推进「代码围栏」奇偶状态，对切分到多个 chunk 的 ``` 分隔符鲁棒。
///
/// 只在遇到换行符时才对「已重组的完整行」判定是否为围栏行（行首去空白后以 ``` 开头）。
/// 未遇换行的尾部留在 `partial` 里，等后续 chunk 拼齐——所以即使 ``` 被切成
/// `` `` `` + `` ` `` 两个 chunk，重组成完整行后仍能正确翻转 `open`。
///
/// 返回值仅在内部使用；主要副作用是更新 `open` 与 `partial`。
#[allow(dead_code)]
fn advance_code_fence_state(open: &mut bool, partial: &mut String, text: &str) {
    // review Finding 6 修复:围栏判定只需"行首若干字节是否 ```",无换行的超长行会让 partial 无界增长。
    // 一旦当前行已超过判定所需长度(远大于 "```" + 缩进),就不再累积字符(围栏与否已定),防无界 String。
    const FENCE_SCAN_LINE_CAP: usize = 256;
    for ch in text.chars() {
        if ch == '\n' {
            if partial.trim_start().starts_with("```") {
                *open = !*open;
            }
            partial.clear();
        } else if partial.len() < FENCE_SCAN_LINE_CAP {
            partial.push(ch);
        }
        // 超过 cap 的同一行剩余字符丢弃(围栏判定不需要;遇换行才重置)。
    }
}

/// 纯函数：在不改动真实状态的前提下，试算「把 `text` 走完之后围栏是否打开」。
/// 用于 drain 决策处判断某个 `<invoke>` 是否落在围栏内。
#[allow(dead_code)]
fn fence_open_after(open: bool, partial: &str, text: &str) -> bool {
    let mut o = open;
    let mut p = partial.to_string();
    advance_code_fence_state(&mut o, &mut p, text);
    // 还要考虑：partial 里残留的「未换行行」如果本身已经是 ``` 开头，
    // 它在遇到换行前不算翻转（保守：只有完整行才翻转）。这里返回已翻转的 o。
    o
}

/// 计算缓冲区末尾”可能是部分 `<invoke` 开标签前缀”的字节数，需要保留等待更多内容
///
/// 例如缓冲区以 `<inv` / `<` / `<i` 结尾时，可能是被切碎的 invoke 开标签，
/// 保留这段尾巴等下一个 chunk 拼齐，避免把半个标签当文本吐出去。
///
/// ⚠️ **安全上界**：真正的部分开标签（`<invoke` / `<invoke` 等）最多只有几十字节。
/// 若从末尾最后一个 `<` 到缓冲区结尾的字节数超过此阈值，说明这个 `<` 只是正文里的普通
/// `<`（中文散文的”a < b”、代码里的比较运算符等），**不是**未闭合的 invoke 开标签。
/// 此时应把整段缓冲（含 `<`）当普通文本吐出去，而不是无限持有导致流停摆：
///   1. `invoke_sniff_buffer` 一旦积压，下一轮 chunk 追加进来，lt=0，emit_len=0，
///      没有任何输出，请求看起来挂死（客户端无增量输出 + 无界内存增长）；
///   2. 根本触发路径：reclaim 开（默认）+ 请求带工具 + 模型输出含一个孤立 `<`，
///      比如”条件 a < b 时触发”这样在中文段落里极为常见的表达式。
/// 64 字节远超最长合法部分标签（`<parameter name=”` ≈ 18 字节含引号，
/// 加最长的 antml: 前缀也不超过 32 字节），同时对真正被切碎的标签有充足余量。
#[allow(dead_code)]
fn partial_invoke_tag_suffix_len(buf: &str) -> usize {
    /// 最长合法开标签前缀的安全上界（字节）。
    /// `<parameter name=”` ≈ 23 字节，`<invoke` = 7 字节；64 字节极为保守。
    /// 超过这个长度的”尾巴”一定不是被切碎的开标签，不应该再持有。
    const MAX_PARTIAL_TAG_BYTES: usize = 64;
    // 任何形如 `<...`（最后一个 '<' 之后没有 '>'）的尾巴都可能是部分开标签
    if let Some(lt) = buf.rfind('<') {
        if !buf[lt..].contains('>') {
            let tail_len = buf.len() - lt;
            // 安全上界：真正的部分开标签只有几十字节，超过则是正文中的普通 '<'，
            // 不应持有（否则导致缓冲区无界增长 + 整条响应停摆）。
            if tail_len <= MAX_PARTIAL_TAG_BYTES {
                return tail_len;
            }
        }
    }
    0
}

/// 检测文本片段里是否出现「文本化的工具调用标记」。
/// 覆盖:Anthropic 工具调用语法 `<invoke`/`</invoke>`/`<parameter name=`(不论是否带 antml: 前缀),
/// 及 `<function_calls>` 包裹。仅诊断用(探针),不改控制流。
fn contains_textified_tool_call(text: &str) -> bool {
    text.contains("<invoke")
        || text.contains("</invoke")
        || text.contains("<parameter name=")
        || text.contains("function_calls>")
        || text.contains("antml:")
}

/// 泄漏 token 探针总开关（环境变量 `KIRO_LEAK_TRACE` 非空即开）。仿 `KIRO_TOOL_TRACE`,平时零开销。
/// 开启时收尾额外打印本请求泄漏 token 清洗全貌，用于坐实 #70544 在流经网关的下游泄漏程度。
fn leak_trace_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("KIRO_LEAK_TRACE")
            .map(|v| !v.trim().is_empty() && v != "0")
            .unwrap_or(false)
    })
}

/// 记录一帧 `toolUseEvent.input` 的合并轨迹（仅 `KIRO_TOOL_TRACE` 开启时）。
///
/// 输出到 `tracing` 的 `kiro::tool_trace` target（`RUST_LOG=kiro::tool_trace=trace` 可单独放行），
/// 逐帧打印：model / tool_use_id / name / stop / 原始帧原文 / 合并前后缓冲，非前缀重写与非法 JSON
/// 额外标注。原文可能含用户数据，故仅在显式开探针时输出。
fn trace_tool_frame(
    model: &str,
    tool_use_id: &str,
    name: &str,
    stop: bool,
    raw_frame: &str,
    buf_before: &str,
    buf_after: &str,
) {
    if !tool_trace_enabled() {
        return;
    }
    let frame_ok = is_complete_json(raw_frame);
    let after_ok = is_complete_json(buf_after);
    // `}{` 粘连是类型 C 的典型非法特征，单独标注便于一眼识别。
    let glued = buf_after.contains("}{") || buf_after.contains("} {");
    tracing::trace!(
        target: "kiro::tool_trace",
        model = model,
        tool_use_id = tool_use_id,
        tool_name = name,
        stop = stop,
        raw_frame_len = raw_frame.len(),
        raw_frame_json_ok = frame_ok,
        buf_before_len = buf_before.len(),
        buf_after_len = buf_after.len(),
        buf_after_json_ok = after_ok,
        buf_after_glued = glued,
        raw_frame = %raw_frame,
        buf_after = %buf_after,
        "[tool_trace] 帧合并轨迹"
    );
}

/// 合并同一 tool_use_id 逐帧到达的 input，返回合并后的新缓冲值。
///
/// 上游 `toolUseEvent.input` 的到达模式并不统一：可能是**纯增量碎片**（每帧只带新片段）、
/// **累积快照**（每帧是"到目前为止的完整 JSON"）、偶发**重复终帧**、迟到的**旧短快照**，
/// 甚至**非前缀重写**（同一 id 先发一个完整对象、再发另一个措辞不同的完整对象）。
/// 旧实现只有「前缀替换 / 否则 append」两步，遇到非前缀双完整对象会拼成 `}{` 粘连的非法 JSON
/// → 客户端 `JSON.parse` 失败 → **Invalid tool parameters（类型 C）**。
///
/// 完备决策表（顺序敏感）：
///   1. frame 空           → buf 不变
///   2. buf 空             → frame
///   3. frame == buf       → buf 不变（重复终帧，不翻倍）
///   4. frame 以 buf 为前缀且更长 → frame（累积快照，取最新最全）
///   5. buf 已是完整合法 JSON，且以 frame 为前缀（frame 更短） → buf 不变（丢弃迟到的旧短快照）
///   6. buf 与 frame 各自都是完整合法 JSON → frame（非前缀重写，只留最新完整对象，消灭 `}{` 粘连）
///   7. 否则               → buf + frame 追加（真增量碎片，还原完整内容）
///
/// 注意第 6 步的前提是**两者各自都完整**：单个完整 JSON 对象无法再被增量扩展，因此第二个
/// 完整对象必然是重写而非续写；反之若 frame 仅是"看似完整"的内层片段（如 `{"inner":1}` 续在
/// `{"outer":` 之后），buf 尚不完整则不触发第 6 步，仍走第 7 步正确追加。
///
/// 第 5 步的 `buf 必须完整` 前置条件同理不可少：`buf.starts_with(frame)` 只是**字符串形状**
/// 判定，几乎所有正在拼接中的 JSON 对象 buf 都以 `{` 开头 —— 若 buf 尚不完整（例如
/// `{"outer":`），此时收到一个单独成帧的 `{`（如嵌套对象的开括号，甚至整个对象重发的开括号）
/// 会被误判成"比 frame 更全的缓冲"而丢弃这个 `{`，导致拼出来的串缺一个 `{`，轻则嵌套结构
/// 缺括号，重则客户端整段 `input` 解析失败退化成 `{}`。只有 buf **已经**是完整合法 JSON 时，
/// 后续再来的更短前缀帧才必然是迟到的旧快照，丢弃才安全。
/// `contextUsageEvent` 的百分比 → `input_tokens`。**无效信号返回 `None`（调用方必须不覆盖已有值）。**
///
/// # 为什么必须是一个共享函数，而不是两处各自判定
///
/// 这个判据有**两个**调用点：流式（`StreamContext::process_kiro_event`）与非流式
/// （`handlers.rs` 的缓冲聚合循环）。它们此前是两份独立实现，而只有流式那份有下界守卫 ——
/// 非流式那份直接 `pct * window / 100.0`，于是同一个上游异常在两条路径上表现不同：
/// 流式忽略脏值、非流式把 `input_tokens` 写成 0（或 NaN 饱和成的 `i32::MAX`）。
///
/// 本仓已多次踩「同一判据两份实现，只修了其中一份」这个形态（`endpoint_for` 与
/// `for_credentials`、`restart_fields` 与 reload restore 表、`cleanup_verdict` 与
/// `batch-delete`）。所以这里不是「给第二处也加个守卫」，而是**让两处物理上共用同一段代码**，
/// 使分叉在结构上不可能发生。源码守卫见 `context_usage_predicate_must_be_shared`。
///
/// # 判据本身
///
/// - `pct <= 0`：真实请求恒有 system prompt + 至少一条 user message，占用率**不可能**为 0。
///   而 `ContextUsageEvent.context_usage_percentage` 带 `#[serde(default)]` ⇒ 上游少发该字段 /
///   发 null 时它就是 `0.0`，与显式 0 逐字节不可分辨。两者都当「没给」处理：
///   错信 0 = 计费信号错且不可逆（账已出），丢一次真 0 只是退回本地估算（可逆）。
/// - `!pct.is_finite()`：NaN/inf 经 `as i32` 是**饱和**转换（NaN→0、inf→`i32::MAX`），后者更糟。
/// - 负值：会让 `billed_input_tokens` 拿到负数。
pub(crate) fn context_input_tokens_from_pct(pct: f64, window_size: i32) -> Option<i32> {
    if pct > 0.0 && pct.is_finite() {
        Some((pct * (window_size as f64) / 100.0) as i32)
    } else {
        None
    }
}

pub(crate) fn merge_tool_input(buf: &str, frame: &str) -> String {
    // 1. 空帧 → 缓冲不变
    if frame.is_empty() {
        return buf.to_string();
    }
    // 2. 缓冲空 → 取本帧
    if buf.is_empty() {
        return frame.to_string();
    }
    // 3. 完全重复终帧 → 不变（避免翻倍）
    if frame == buf {
        return buf.to_string();
    }
    // 4. 累积快照：本帧以缓冲为前缀且更长 → 用本帧整体替换
    if frame.len() > buf.len() && frame.starts_with(buf) {
        return frame.to_string();
    }
    // 5. 迟到的旧短快照：缓冲以本帧为前缀（本帧更短）且缓冲**已是完整合法 JSON**
    //    → 保留更全的缓冲，丢弃本帧。前置条件 `is_complete_json(buf)` 不可省：buf 尚在
    //    拼接中途时（如 `{"outer":`）若来一个单独成帧的 `{`，纯形状判定会把这个 `{` 误判为
    //    "迟到的旧短快照"而吞掉，导致最终串缺开头的 `{`（Invalid tool parameters 根因之一）。
    if buf.len() > frame.len() && buf.starts_with(frame) && is_complete_json(buf) {
        return buf.to_string();
    }
    // 6. 非前缀重写：缓冲与本帧各自都是完整合法 JSON → 只留最新完整对象（消灭 `}{` 粘连）
    if is_complete_json(buf) && is_complete_json(frame) {
        return frame.to_string();
    }
    // 7. 真增量碎片：追加还原完整内容
    let mut merged = String::with_capacity(buf.len() + frame.len());
    merged.push_str(buf);
    merged.push_str(frame);
    merged
}

#[cfg(test)]
mod usage_caliber_tests {
    //! `input_tokens` 的**两个口径**：
    //! - 发给客户端的 `usage.input_tokens` = billed（已剔除 cache，与 cache_read 互斥）
    //! - 落进 `RequestRecord` 的 `input_tokens` = gross（含 cache，是 cache_read 的超集）
    //!
    //! 同名不同义，零注释时极易被下游当同一口径而把 cache 计两次。此处把契约钉死：
    //! 谁改动其中一侧的口径，本模块必失败。
    use super::*;

    fn ctx_with_cache(input_tokens: i32, cache_read: i32, cache_creation: i32) -> StreamContext {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-sonnet-5",
            input_tokens,
            false,
            HashMap::new(),
        );
        ctx.set_cache_usage(Some(CacheUsageBreakdown {
            cache_creation_input_tokens: cache_creation,
            cache_read_input_tokens: cache_read,
            cache_creation_5m_input_tokens: cache_creation,
            cache_creation_1h_input_tokens: 0,
        }));
        ctx
    }

    #[test]
    fn should_report_gross_input_to_record_and_billed_to_client() {
        let ctx = ctx_with_cache(12_500, 12_000, 300);

        // 客户端侧：billed 口径（12500 - 300 - 12000 = 200）
        let msg_start = ctx.create_message_start_event();
        assert_eq!(msg_start["message"]["usage"]["input_tokens"], 200);
        assert_eq!(
            msg_start["message"]["usage"]["cache_read_input_tokens"],
            12_000
        );

        // 统计侧：gross 口径（未剔除 cache）
        let usage = ctx.resolved_usage();
        assert_eq!(
            usage.input_tokens, 12_500,
            "ResolvedUsage.input_tokens 必须是 gross，改成 billed 会让历史统计数据断裂"
        );
        assert_eq!(usage.cache_read_tokens, 12_000);
        assert_eq!(usage.cache_creation_tokens, 300);
    }

    #[test]
    fn should_keep_gross_input_as_superset_of_cache_in_record() {
        // 落库后：cache 是 input 的子集，消费方直接用 input_tokens 即为总输入
        let ctx = ctx_with_cache(12_500, 12_000, 300);
        let usage = ctx.resolved_usage();
        let mut record = crate::usage::RequestRecord::new("req-caliber", "claude-sonnet-5");
        record.input_tokens = usage.input_tokens;
        record.cache_read_tokens = usage.cache_read_tokens;
        record.cache_creation_tokens = usage.cache_creation_tokens;
        record.clamp_cache_to_input();

        assert!(
            record.cache_read_tokens + record.cache_creation_tokens <= record.input_tokens,
            "cache 必须是 gross input 的子集"
        );
        // 还原客户端口径应与 message_start 一致
        assert_eq!(
            record.billed_input_tokens(),
            ctx.create_message_start_event()["message"]["usage"]["input_tokens"]
                .as_i64()
                .unwrap() as i32
        );
    }

    #[test]
    fn should_prefer_context_usage_over_estimate_for_gross_input() {
        // resolved_usage 优先上游反推值；这正是它与 cache（本地估算）不同源的根因
        let mut ctx = ctx_with_cache(10_000, 9_000, 0);
        ctx.context_input_tokens = Some(4_000);
        assert_eq!(ctx.resolved_usage().input_tokens, 4_000);
        // 此时 cache_read(9000) > input(4000) → 落库前必须收敛，否则面板出现矛盾数字
        let usage = ctx.resolved_usage();
        let mut record = crate::usage::RequestRecord::new("req-mismatch", "claude-sonnet-5");
        record.input_tokens = usage.input_tokens;
        record.cache_read_tokens = usage.cache_read_tokens;
        record.clamp_cache_to_input();
        assert_eq!(record.cache_read_tokens, 4_000);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试辅助：把 [`ThinkingTagMatch`] 压成起始字节位置。
    ///
    /// 查找函数改为返回「起点 + **实际**长度」（大小写不敏感 + 容属性后标签不再定长），
    /// 而既有位置断言锁的是起点语义 —— 用这个投影保留它们的原值，不改判据强度。
    fn tag_start(m: Option<ThinkingTagMatch>) -> Option<usize> {
        m.map(|m| m.start)
    }

    // ========================================================================
    // 文本化 invoke 解析纯函数单测（从 ZyphrZero/kiro.rs 移植 + KiroStudio 补充）
    // 这些函数是纯函数，直接对字符串断言，不经过 StreamContext 状态机。
    // 命名统一含 `invoke`，便于 `cargo test -- invoke` 精准挑选。
    // ========================================================================

    #[test]
    fn test_invoke_parse_complete_block() {
        // 🟢 完整块：<invoke name="Bash"><parameter name="command">ls</parameter></invoke>
        let block = r#"<invoke name="Bash"><parameter name="command">ls</parameter></invoke>"#;
        let (name, input) = parse_invoke_block(block).expect("应解析出 tool");
        assert_eq!(name, "Bash");
        let parsed: serde_json::Value = serde_json::from_str(&input).expect("input 应为合法 JSON");
        assert_eq!(parsed["command"], "ls");
    }

    #[test]
    fn test_invoke_parse_antml_prefix_tolerated() {
        // 🟢 带 antml: 命名空间前缀应被容忍（开/闭标签均带前缀）。
        // 用拼接构造标签，避免源码里出现字面工具调用标记。
        let ns = "antml:";
        let block = format!(
            "<{ns}invoke name=\"X\"><{ns}parameter name=\"y\">v</{ns}parameter></{ns}invoke>"
        );
        let (name, input) = parse_invoke_block(&block).expect("带前缀的块应能解析");
        assert_eq!(name, "X");
        let parsed: serde_json::Value = serde_json::from_str(&input).expect("input 应为合法 JSON");
        assert_eq!(parsed["y"], "v");
    }

    #[test]
    fn test_invoke_parse_param_value_with_lt_multiline_chinese() {
        // 🟢 参数值含 `<`、多行、中文 → 不被截断
        let value = "第一行 a < b\n第二行 路径 /tmp/中文";
        let block = format!(
            "<invoke name=\"write_file\"><parameter name=\"content\">{value}</parameter></invoke>"
        );
        let (name, input) = parse_invoke_block(&block).expect("应解析出 tool");
        assert_eq!(name, "write_file");
        let parsed: serde_json::Value = serde_json::from_str(&input).expect("input 应为合法 JSON");
        assert_eq!(
            parsed["content"], value,
            "参数值应完整保留（含 < / 多行 / 中文）"
        );
    }

    #[test]
    fn test_invoke_parse_apply_patch_literal_close_tag_survives() {
        // 🟢 P0-1：apply_patch 正文里含字面 </parameter> —— 贪婪取最后一个闭合，不被提前截断。
        let closing = format!("</{}>", "parameter");
        let value = format!("patch line 1\n此处有字面 {closing} 标记\npatch line 3");
        let block = format!(
            "<invoke name=\"apply_patch\"><parameter name=\"input\">{value}</parameter></invoke>"
        );
        let (name, input) = parse_invoke_block(&block).expect("应解析出 tool");
        assert_eq!(name, "apply_patch");
        let parsed: serde_json::Value = serde_json::from_str(&input).expect("input 应为合法 JSON");
        assert_eq!(parsed["input"], value, "含字面闭合标签的正文应完整保留");
    }

    #[test]
    fn test_invoke_parse_two_params() {
        // 🟢 多参数：按下一个参数开标签正确切分
        let block = r#"<invoke name="t"><parameter name="a">1</parameter><parameter name="b">2</parameter></invoke>"#;
        let (name, input) = parse_invoke_block(block).expect("应解析出 tool");
        assert_eq!(name, "t");
        let parsed: serde_json::Value = serde_json::from_str(&input).expect("input 应为合法 JSON");
        assert_eq!(parsed["a"], "1");
        assert_eq!(parsed["b"], "2");
    }

    #[test]
    fn test_invoke_parse_no_params() {
        // 🟢 零参数块 → 合法但 input 为空对象
        let block = r#"<invoke name="noop"></invoke>"#;
        let (name, input) = parse_invoke_block(block).expect("应解析出 tool");
        assert_eq!(name, "noop");
        assert_eq!(input, "{}");
    }

    #[test]
    fn test_invoke_parse_empty_name_rejected() {
        // 🔴 name 为空 → None
        let block = r#"<invoke name=""><parameter name="x">v</parameter></invoke>"#;
        assert!(parse_invoke_block(block).is_none(), "空 name 应被拒绝");
    }

    #[test]
    fn test_invoke_find_start_bare_and_prefixed() {
        // 🟢 裸 `<invoke` 与带前缀 `<prefix:invoke` 都能定位到 '<'
        assert_eq!(find_invoke_start("<invoke name=\"x\">"), Some(0));
        assert_eq!(find_invoke_start("abc\n<invoke name=\"x\">"), Some(4));
        let prefixed = "<invoke name=\"x\">";
        assert_eq!(find_invoke_start(prefixed), Some(0));
    }

    #[test]
    fn test_invoke_find_start_backtick_wrapped_is_skipped() {
        // 🔴 被反引号包裹的 <invoke 视为引用，跳过
        assert_eq!(find_invoke_start("示例：`<invoke name=\"x\">`"), None);
    }

    #[test]
    fn test_invoke_find_start_ignores_invoked_word() {
        // 🔴 `invoked` 这类词不构成开标签（标签名后需边界字符）
        assert_eq!(find_invoke_start("the model invoked a tool"), None);
    }

    #[test]
    fn test_invoke_block_end_greedy_and_unclosed() {
        // 🟢 完整块 → 返回含闭标签的结束位置；未闭合 → None
        let full = r#"<invoke name="x"><parameter name="c">ls</parameter></invoke>"#;
        let end = find_invoke_block_end(full, 0).expect("完整块应有结束位置");
        assert_eq!(end, full.len());

        let unclosed = r#"<invoke name="x"><parameter name="c">ls"#;
        assert!(
            find_invoke_block_end(unclosed, 0).is_none(),
            "未闭合块应返回 None"
        );
    }

    #[test]
    fn test_invoke_next_open_finds_second_burst() {
        // 🟢 连发 burst：A 紧跟 B，find_next_invoke_open 跳过 A 自身开标签，定位到 B
        let s = r#"<invoke name="a"><parameter name="x">1</parameter></invoke><invoke name="b"><parameter name="y">2</parameter></invoke>"#;
        let b_pos = find_next_invoke_open(s, 0).expect("应找到第二个块开标签");
        assert_eq!(
            &s[b_pos..b_pos + "<invoke name=\"b\"".len()],
            "<invoke name=\"b\""
        );
    }

    #[test]
    fn test_invoke_two_blocks_parsed_via_block_end() {
        // 🟢 用 find_invoke_block_end + parse_invoke_block 串起两块，各自独立解析
        let s = r#"<invoke name="a"><parameter name="x">1</parameter></invoke><invoke name="b"><parameter name="y">2</parameter></invoke>"#;
        let start_a = find_invoke_start(s).unwrap();
        let end_a = find_invoke_block_end(s, start_a).unwrap();
        let (na, _) = parse_invoke_block(&s[start_a..end_a]).unwrap();
        assert_eq!(na, "a");

        let start_b = find_next_invoke_open(s, start_a).unwrap();
        let end_b = find_invoke_block_end(s, start_b).unwrap();
        let (nb, _) = parse_invoke_block(&s[start_b..end_b]).unwrap();
        assert_eq!(nb, "b");
        assert_eq!(end_b, s.len());
    }

    #[test]
    fn test_invoke_last_close_greedy_skips_literal() {
        // 🟢 区间内含字面 </invoke> → find_last_invoke_close 取最后一个真闭合
        let s = format!(
            "<invoke name=\"x\"><parameter name=\"c\">正文里有字面 {} 标记</parameter></invoke>",
            "</invoke>"
        );
        let end = find_last_invoke_close(&s, 0, s.len()).expect("应找到最后一个闭合");
        assert_eq!(end, s.len());
    }

    #[test]
    fn test_invoke_open_tag_lt_pos_bare_and_prefixed() {
        // 🟢 open_tag_lt_pos：裸 `<tag` 与 `<prefix:tag` 都能回溯到 '<'
        let bare = "<invoke";
        let name_pos = bare.find("invoke").unwrap();
        assert_eq!(open_tag_lt_pos(bare, name_pos), Some(0));

        let prefixed = "<invoke";
        let np = prefixed.find("invoke").unwrap();
        assert_eq!(open_tag_lt_pos(prefixed, np), Some(0));

        // 前面不是 '<' 也不是合法前缀 → None
        let bad = "xinvoke";
        let bp = bad.find("invoke").unwrap();
        assert_eq!(open_tag_lt_pos(bad, bp), None);
    }

    #[test]
    fn test_invoke_extract_name_attr() {
        assert_eq!(
            extract_name_attr(r#"<invoke name="Bash">"#),
            Some("Bash".to_string())
        );
        assert_eq!(
            extract_name_attr(r#"<parameter name="cmd">"#),
            Some("cmd".to_string())
        );
        assert_eq!(extract_name_attr("<invoke>"), None);
    }

    #[test]
    fn test_invoke_next_param_open() {
        // 🟢 find_next_param_open 定位下一个参数开标签的 '<'
        let body = r#"<parameter name="a">1</parameter><parameter name="b">2</parameter>"#;
        // 从第一个参数值区起找下一个参数开标签
        let first_val_start = body.find('>').unwrap() + 1;
        let next = find_next_param_open(body, first_val_start).expect("应找到第二个参数开标签");
        assert_eq!(
            &body[next..next + "<parameter name=\"b\"".len()],
            "<parameter name=\"b\""
        );
    }

    #[test]
    fn test_invoke_looks_like_real_leak_line_start() {
        // 🟢 行首（空 / 换行结尾）→ 像真泄漏；句中 → 不像
        assert!(invoke_looks_like_real_leak(""));
        assert!(invoke_looks_like_real_leak("some text\n"));
        assert!(invoke_looks_like_real_leak("some text\n   "));
        assert!(invoke_looks_like_real_leak("some text\r"));
        assert!(!invoke_looks_like_real_leak("讨论 "));
        assert!(!invoke_looks_like_real_leak("- "));
    }

    #[test]
    fn test_invoke_strip_trailing_stray_preserves_newline() {
        // 回归：narrative\ncall → 只剥 stray 行，保留前一行换行（行首信号不丢）
        let got = strip_trailing_stray_tokens("some text\ncall");
        assert_eq!(got, "some text\n", "必须保留叙述行末的换行");
        assert!(invoke_looks_like_real_leak(got), "剥完仍应像行首泄漏");
    }

    #[test]
    fn test_invoke_strip_trailing_stray_court_token() {
        // 🟢 KiroStudio 生产语料：court 是主要 stray token，应被剥
        assert_eq!(
            strip_trailing_stray_tokens("先看结果。\ncourt"),
            "先看结果。\n"
        );
        assert_eq!(strip_trailing_stray_tokens("court"), "");
        // 多个连续 stray 行全部剥掉
        assert_eq!(strip_trailing_stray_tokens("正文\ncall\ncourt"), "正文\n");
    }

    #[test]
    fn test_invoke_strip_trailing_stray_keeps_non_stray() {
        // 🔴 非 stray 的末行不剥
        assert_eq!(strip_trailing_stray_tokens("hello world"), "hello world");
    }

    #[test]
    fn test_invoke_collapse_stray_token_floods() {
        // 🟢 复读死循环：court 独占一行连续 100 次 → 从超阈值处截断
        let mut s = String::from("正文引导\n");
        for _ in 0..100 {
            s.push_str("court\n");
        }
        s.push_str("<invoke name=\"x\">");
        let collapsed = collapse_stray_token_floods(&s);
        // 截断后应只保留阈值内内容，court 出现次数远小于 100
        let court_count = collapsed.matches("court").count();
        assert!(
            court_count < 100,
            "复读应被熔断截断，court 次数={court_count}"
        );
        assert!(!collapsed.contains("<invoke"), "超阈值后的内容应被丢弃");
    }

    #[test]
    fn test_invoke_collapse_stray_token_chinese_flood() {
        // 🟢 中文变体 課/课 独占行复读也应被熔断(修复:原集合漏了中文,逐字清洗剥得掉但熔断抓不到)。
        for tok in ["課", "课"] {
            let mut s = String::from("正文\n");
            for _ in 0..100 {
                s.push_str(tok);
                s.push('\n');
            }
            let collapsed = collapse_stray_token_floods(&s);
            let cnt = collapsed.matches(tok).count();
            assert!(cnt < 100, "中文 {tok} 复读应被熔断截断,次数={cnt}");
        }
    }

    #[test]
    fn test_invoke_collapse_stray_token_below_threshold() {
        // 🔴 阈值内的少量引导词重复原样保留
        let s = "call\ncall\n<invoke name=\"x\">";
        let collapsed = collapse_stray_token_floods(s);
        assert_eq!(collapsed, s, "阈值内不应截断");
    }

    #[test]
    fn test_invoke_code_fence_state_toggle() {
        // 🟢 代码围栏奇偶翻转：一对 ``` 归零
        let mut open = false;
        let mut partial = String::new();
        advance_code_fence_state(&mut open, &mut partial, "```rust\nlet x = 1;\n```\n");
        assert!(!open, "一对围栏后应回到关闭态");

        // 单个开围栏 → 打开
        let mut open2 = false;
        let mut partial2 = String::new();
        advance_code_fence_state(&mut open2, &mut partial2, "```\n代码\n");
        assert!(open2, "单个开围栏后应为打开态");
    }

    #[test]
    fn test_invoke_fence_open_after_pure() {
        // 🟢 fence_open_after 纯试算，不改传入状态
        assert!(fence_open_after(false, "", "```\n"), "进入围栏");
        assert!(!fence_open_after(true, "", "```\n"), "离开围栏");
        assert!(!fence_open_after(false, "", "普通文本\n"), "普通文本不翻转");
    }

    #[test]
    fn test_invoke_partial_tag_suffix_len() {
        // 🟢 缓冲区末尾的半个开标签应被识别为需保留的尾巴
        assert_eq!(partial_invoke_tag_suffix_len("hello<inv"), 4);
        assert_eq!(partial_invoke_tag_suffix_len("hello<"), 1);
        // 已闭合的标签结尾 → 无需保留
        assert_eq!(partial_invoke_tag_suffix_len("<invoke>"), 0);
        assert_eq!(partial_invoke_tag_suffix_len("no angle bracket"), 0);
    }

    #[test]
    fn test_partial_invoke_tag_suffix_bounded_no_stream_stall() {
        // ⭐流停摆回归(旧代码必失败):`<` 之后的尾巴一旦超过"最长可能的半个开标签",
        // 就一定不是被切碎的 invoke 标签,而是正文里的普通 `<`(中文散文的"a < b"、
        // 数学式、代码里的比较运算符)。旧代码无上限地把它全部 hold 住:
        //   一旦这个 `<` 落到缓冲区首位,keep=buf.len()、emit_len=0 → 此后**整条响应
        //   的所有文本都不再下发**,全部囤到流结束才 flush,且缓冲无界增长。
        // 修复后超过 64 字节即判定"不是标签",返回 0 让正文正常吐出。

        // 短尾巴(可能是真的半个标签)→ 仍然保留,行为不变。
        assert_eq!(partial_invoke_tag_suffix_len("text<inv"), 4);
        assert_eq!(partial_invoke_tag_suffix_len("text<parameter na"), 13);

        // 长尾巴(正文里的普通 `<`)→ 必须返回 0(不 hold),否则流停摆。
        let prose = format!("条件 a < b 时触发{}", "后面还有很多正文".repeat(20));
        assert_eq!(
            partial_invoke_tag_suffix_len(&prose),
            0,
            "正文中的普通 `<` 后跟大量文本时绝不能 hold(旧代码在此无界持有 → 整条流停摆)"
        );

        // 关键退化场景:`<` 恰好在缓冲区**首位**且后面全是正文。
        // 旧代码此时 keep=len、emit_len=0 → 一个字节都不输出。
        let stuck = format!("<{}", "x".repeat(500));
        assert_eq!(
            partial_invoke_tag_suffix_len(&stuck),
            0,
            "`<` 在首位且尾巴超长时必须返回 0,否则 emit_len=0 → 流永久停摆"
        );

        // 边界:恰好 64 字节的尾巴仍视为可能的标签;65 字节则不再 hold。
        let at_limit = format!("<{}", "a".repeat(63)); // 尾巴 = 64 字节
        assert_eq!(partial_invoke_tag_suffix_len(&at_limit), 64);
        let over_limit = format!("<{}", "a".repeat(64)); // 尾巴 = 65 字节
        assert_eq!(partial_invoke_tag_suffix_len(&over_limit), 0);
    }

    #[test]
    fn test_sse_event_format() {
        let event = SseEvent::new("message_start", json!({"type": "message_start"}));
        let sse_str = event.to_sse_string();

        assert!(sse_str.starts_with("event: message_start\n"));
        assert!(sse_str.contains("data: "));
        assert!(sse_str.ends_with("\n\n"));
    }

    fn mk_ctx() -> StreamContext {
        StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new())
    }

    /// 回归：TTFB 打点必须在**首个真实内容 delta** 落定时发生，且只记第一次。
    ///
    /// **旧代码为何 FAIL**：`first_token_ms` 全仓 **0 个生产赋值点**，线上 traces.db
    /// 24 小时 59458 条该列**全 NULL** —— 所有延迟分析失效，并且它阻塞了
    /// 「用量/额度动态刷新」与缓存实验两条线（都需要能分解端到端延迟）。
    /// 旧代码下 `first_token_at()` 恒为 None，第二个断言必然 FAIL。
    #[test]
    fn first_token_is_marked_on_first_real_content_delta_only() {
        let mut ctx = mk_thinking_ctx();
        assert!(ctx.first_token_at().is_none(), "初始未产出内容，不该有打点");

        // 首个真实内容（thinking_delta）→ 应打点
        ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: "思考".into(),
                ..Default::default()
            },
        ));
        let first = ctx
            .first_token_at()
            .expect("首个真实内容 delta 后必须有打点");

        // 再来内容 → 打点**不得**被刷新（TTFB 是"第一次"，不是"最后一次"）
        std::thread::sleep(std::time::Duration::from_millis(5));
        ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: "更多思考".into(),
                ..Default::default()
            },
        ));
        assert_eq!(
            ctx.first_token_at(),
            Some(first),
            "打点必须幂等：后续内容不得覆盖首 token 时刻，否则测的是最后一片而非 TTFB"
        );
    }

    /// 非内容事件（metering / contextUsage）绝不能被当成首 token。
    ///
    /// 这条守住的是最容易错的边界：这些事件也会经过同一个 choke point，
    /// 若判据写成"有事件产出就打点"，TTFB 会变成"首个任意帧"，数值系统性偏小。
    #[test]
    fn non_content_events_do_not_mark_first_token() {
        let mut ctx = mk_thinking_ctx();
        ctx.process_kiro_event(&Event::ContextUsage(
            crate::kiro::model::events::ContextUsageEvent {
                context_usage_percentage: 12.0,
            },
        ));
        assert!(
            ctx.first_token_at().is_none(),
            "contextUsageEvent 不含模型输出，不得触发 TTFB 打点"
        );
    }

    /// 回归（A8）：`contextUsagePercentage` 的**非正/非有限**值绝不能覆盖已算出的
    /// `context_input_tokens`。
    ///
    /// **旧代码为何 FAIL**：该 match arm 只判 `>= 100.0` 上界，无条件执行
    /// `self.context_input_tokens = Some(pct * window / 100.0)`。而该字段带
    /// `#[serde(default)]`，上游少发/发脏值时恒为 `0.0` ⇒ 写进 `Some(0)` ⇒
    /// 下游 `context_input_tokens.unwrap_or(本地估算)` 拿到 0（而非退回估算）⇒
    /// `billed_input_tokens` 连 cache 字段一起归零 = 计费信号错。
    /// 旧代码下第二个断言拿到的是 `Some(0)`，必然 FAIL。
    #[test]
    fn invalid_context_usage_percentage_must_not_overwrite_input_tokens() {
        use crate::kiro::model::events::ContextUsageEvent;
        let mut ctx = mk_ctx();

        // 先来一个**有效**信号，坐实正常路径仍会写入（对照组：防守卫写成"一律不写"）。
        ctx.process_kiro_event(&Event::ContextUsage(ContextUsageEvent {
            context_usage_percentage: 10.0,
        }));
        let good = ctx
            .context_input_tokens
            .expect("有效百分比必须算出 input_tokens");
        assert!(good > 0, "10% 的窗口占用必须是正 token 数，实得 {good}");

        // 脏值（serde 默认值 / 上游漏发都长这样）绝不能把它清零。
        for bad in [0.0_f64, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            ctx.process_kiro_event(&Event::ContextUsage(ContextUsageEvent {
                context_usage_percentage: bad,
            }));
            assert_eq!(
                ctx.context_input_tokens,
                Some(good),
                "无效百分比 {bad} 覆盖了已有 input_tokens（计费口径被归零）"
            );
        }
    }

    /// 配套（A8）：加了下界守卫后，`>= 100.0` 的上界行为**不得**被连带改坏。
    ///
    /// 这条防的是「改分支时把上界判定挪进 else 里」这类顺序性回归：
    /// 100% 必须仍然置 `model_context_window_exceeded`，且 input_tokens 照常写入。
    #[test]
    fn context_usage_at_full_still_sets_stop_reason() {
        use crate::kiro::model::events::ContextUsageEvent;
        let mut ctx = mk_ctx();
        ctx.process_kiro_event(&Event::ContextUsage(ContextUsageEvent {
            context_usage_percentage: 100.0,
        }));
        assert_eq!(
            ctx.state_manager.get_stop_reason(),
            "model_context_window_exceeded",
            "100% 占用必须置 model_context_window_exceeded"
        );
        assert!(
            ctx.context_input_tokens.is_some_and(|t| t > 0),
            "100% 是有效信号，仍必须写入 input_tokens"
        );
    }

    /// 空 delta（关块占位）不算首 token。
    #[test]
    fn empty_delta_is_not_first_token() {
        assert!(
            !is_first_content_delta(&SseEvent::new(
                "content_block_delta",
                json!({"delta": {"type": "thinking_delta", "thinking": ""}})
            )),
            "空 thinking_delta 是关块占位，不代表有内容产出"
        );
        assert!(
            is_first_content_delta(&SseEvent::new(
                "content_block_delta",
                json!({"delta": {"type": "text_delta", "text": "hi"}})
            )),
            "非空 text_delta 应算首 token（对照组，防止判据过严把真内容也挡掉）"
        );
        assert!(
            !is_first_content_delta(&SseEvent::new(
                "message_start",
                json!({"type": "message_start"})
            )),
            "message_start 在内容产出前就发了，绝不能算首 token"
        );
    }

    /// 回归（E1）：`reasoningContentEvent` 必须产出 `thinking_delta`。
    ///
    /// **旧代码为何 FAIL**：该事件此前落 `EventType::Unknown`，payload 被**直接丢弃**
    /// （只按类型 warn 一次），产出零个 SSE 事件。本测试断言至少有一个
    /// `content_block_delta` 且 `delta.type == "thinking_delta"`，旧代码下事件列表是空的。
    ///
    /// 价值不在"多一个功能"：上游本就给了结构化思考边界，我们扔掉后改用文本嗅探
    /// `<thinking>` 标签把边界猜回来 —— 已知致命缺陷 #14（invoke_sniff_buffer 无界持有
    /// 导致整条流停摆）就出在那套嗅探上。接入结构化流是**移除一整类缺陷的来源**。
    /// thinking **开启**的 ctx（mk_ctx 的第三参 thinking_enabled 是 false，E1 需要 true）。
    fn mk_thinking_ctx() -> StreamContext {
        StreamContext::new_with_thinking("deepseek", 10, true, HashMap::new())
    }

    #[test]
    fn should_emit_thinking_delta_from_reasoning_content_event() {
        let mut ctx = mk_thinking_ctx();
        let ev = Event::ReasoningContent(crate::kiro::model::events::ReasoningContentEvent {
            text: "让我先看一下目录结构".to_string(),
            ..Default::default()
        });
        let out = ctx.process_kiro_event(&ev);

        assert!(
            !out.is_empty(),
            "结构化思考帧不应产出零事件（旧代码丢弃 payload）"
        );
        let joined: String = out.iter().map(|e| e.to_sse_string()).collect();
        assert!(
            joined.contains("\"type\":\"thinking_delta\""),
            "应产出 thinking_delta，实际: {joined}"
        );
        assert!(
            joined.contains("让我先看一下目录结构"),
            "思考内容应被下发，实际: {joined}"
        );
    }

    /// 回归（E1）：结构化流与文本嗅探**共用**同一个 thinking 块，绝不重复开块。
    ///
    /// 两条路径都保留（上游可能对某些模型仍走内联标签），所以必须共享
    /// `in_thinking_block` / `thinking_block_index`。若各自开块，客户端会收到两个
    /// `content_block_start(thinking)`，Anthropic SDK 侧属协议违规。
    #[test]
    fn should_not_double_open_thinking_block_when_both_paths_fire() {
        let mut ctx = mk_thinking_ctx();
        let mut all = String::new();

        // 先走结构化流（开块）
        all.push_str(
            &ctx.process_kiro_event(&Event::ReasoningContent(
                crate::kiro::model::events::ReasoningContentEvent {
                    text: "结构化思考".into(),
                    ..Default::default()
                },
            ))
            .iter()
            .map(|e| e.to_sse_string())
            .collect::<String>(),
        );
        // 再喂含 <thinking> 的正文（嗅探路径）
        all.push_str(
            &ctx.process_kiro_event(&Event::AssistantResponse(
                crate::kiro::model::events::AssistantResponseEvent {
                    content: "<thinking>内联思考</thinking>正文".into(),
                },
            ))
            .iter()
            .map(|e| e.to_sse_string())
            .collect::<String>(),
        );

        let opens = all.matches("\"type\":\"thinking\"").count();
        assert!(
            opens <= 1,
            "thinking 块只应被开一次，实际匹配 {opens} 次；两条路径必须共用同一个块。全文: {all}"
        );
    }

    /// 回归（E1 最严重的坑）：结构化 reasoning 之后的**普通正文**必须走 `text_delta`，
    /// 绝不能被当成思考内容。
    ///
    /// **旧代码（E1 首版）为何 FAIL**：结构化 reasoning 是**纯增量、无终止帧**的，
    /// 真实上游形态是「N 帧 reasoningContentEvent → assistantResponseEvent 携带普通正文」。
    /// 而文本嗅探路径的分支是 `else if self.in_thinking_block` —— reasoning 一旦开过 thinking 块，
    /// 后续**不带任何标签**的正文就落进那个分支，被发成 `thinking_delta`。
    ///
    /// 实测旧代码的输出（本测试抓到的原始报文）：
    /// ```text
    /// data: {"delta":{"thinking":"这是给用","type":"thinking_delta"},...}
    /// ```
    /// 即**用户可见的答案整段消失进思考面板**；更糟的是 `has_non_thinking_blocks()` 为 false
    /// 会让收尾把 `stop_reason` 置成 `max_tokens` 并只吐一个空格文本块 → 客户端显示**空答案**。
    ///
    /// 修法是让两条路径**互斥**（`reasoning_stream_seen` + 首个正文即关块），
    /// 而不是共享 `in_thinking_block` 状态。原先那个"共用同一个块"的测试只喂了含完整
    /// `<thinking>…</thinking>` 配对的正文 —— 恰好是代码能处理的那种，所以漏掉了这个形态。
    #[test]
    fn prose_after_reasoning_stream_goes_to_text_not_thinking() {
        let mut ctx = mk_thinking_ctx();
        ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: "先想一下".into(),
                ..Default::default()
            },
        ));
        // 真实上游形态：reasoning 无终止符，紧接着就是普通正文（不带任何标签）
        let out = ctx.process_kiro_event(&Event::AssistantResponse(
            crate::kiro::model::events::AssistantResponseEvent {
                content: "这是给用户看的答案".into(),
            },
        ));
        let j: String = out.iter().map(|e| e.to_sse_string()).collect();
        assert!(
            j.contains("text_delta") && j.contains("这是给用户看的答案"),
            "正文必须作为 text_delta 下发，否则答案会消失进 thinking 面板。实际: {j}"
        );
    }

    /// thinking 未开启时结构化思考帧必须**整帧丢弃**，绝不能混进用户可见正文。
    #[test]
    fn reasoning_content_is_dropped_when_thinking_disabled() {
        // thinking_enabled = false
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        let out = ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: "内部推理".into(),
                ..Default::default()
            },
        ));
        let joined: String = out.iter().map(|e| e.to_sse_string()).collect();
        assert!(
            !joined.contains("内部推理"),
            "未开 thinking 时内部推理绝不能下发给客户端，实际: {joined}"
        );
    }

    // ===== M1：`!thinking_enabled` 且本轮只有 reasoning ⇒ 空响应兜底 =====

    fn reasoning_ev(text: &str) -> Event {
        Event::ReasoningContent(crate::kiro::model::events::ReasoningContentEvent {
            text: text.to_string(),
            ..Default::default()
        })
    }

    fn text_ev(content: &str) -> Event {
        Event::AssistantResponse(crate::kiro::model::events::AssistantResponseEvent {
            content: content.to_string(),
        })
    }

    /// 把一轮事件跑完（含真实收尾），返回全部 SSE 报文拼串。
    fn run_turn(ctx: &mut StreamContext, events: &[Event]) -> String {
        let mut all = ctx.generate_initial_events();
        for ev in events {
            all.extend(ctx.process_kiro_event(ev));
        }
        all.extend(ctx.generate_final_events());
        all.iter().map(|e| e.to_sse_string()).collect()
    }

    /// ① thinking 关 + 本轮**只有** reasoning（无正文）→ 必须有非空输出。
    ///
    /// **旧代码为何 FAIL**：`process_reasoning_content` 在 `!thinking_enabled` 时
    /// `return Vec::new()` 就地丢帧，且没有任何别处留底 ⇒ 整轮只剩
    /// message_start / content_block_start / message_delta / message_stop，
    /// **一个 text_delta 都没有** ⇒ 客户端（Claude Code）显示完全空的回答且不重试。
    #[test]
    fn reasoning_only_turn_degrades_to_text_when_thinking_disabled() {
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        let joined = run_turn(
            &mut ctx,
            &[
                reasoning_ev("先分析一下这个问题"),
                reasoning_ev("然后得出结论"),
            ],
        );

        assert!(
            joined.contains("\"type\":\"text_delta\""),
            "只有 reasoning 的一轮必须降级出正文，否则客户端拿到空响应。实际: {joined}"
        );
        assert!(
            joined.contains("先分析一下这个问题") && joined.contains("然后得出结论"),
            "降级下发应保留 reasoning 原文（多帧要拼齐）。实际: {joined}"
        );
        assert!(
            !joined.contains("thinking_delta"),
            "降级走的是 text_delta，绝不能凭空造 thinking 块（客户端没要 thinking）。实际: {joined}"
        );
    }

    /// ② thinking 关 + **有正文** → reasoning 仍被丢弃（防推理泄漏给用户）。
    ///
    /// 这是兜底的反向护栏：兜底若写成"无条件转正文"（kiro-rs 的做法），
    /// 用户明确不想看的内部推理就会混进可见回答里。
    #[test]
    fn reasoning_stays_dropped_when_body_text_present() {
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        let joined = run_turn(
            &mut ctx,
            &[
                reasoning_ev("内部推理不该给用户看"),
                text_ev("这是给用户看的答案"),
            ],
        );

        assert!(
            joined.contains("这是给用户看的答案"),
            "正文必须下发。实际: {joined}"
        );
        assert!(
            !joined.contains("内部推理不该给用户看"),
            "有正文时 reasoning 必须保持丢弃，否则内部推理泄漏给明确没要 thinking 的客户端。实际: {joined}"
        );
    }

    /// ②' 顺序护栏：正文整轮被 hold 在 `invoke_sniff_buffer`、只在收尾 flush 才出现时，
    /// 兜底**不得**误判为"无正文"。
    ///
    /// 这条测的是**分支顺序**而非分支内容：兜底判据依赖 `body_content_seen`，而该标志在
    /// flush 之前必然为 false（正文还在缓冲里）。把兜底分支上移到
    /// `flush_invoke_sniff_buffer()` 之前，本测试即 FAIL —— 输出会同时出现推理与正文（双份）。
    /// 行首未闭合的 `<invoke` 被 hold 到收尾是**已知常态**（见 MAX_INVOKE_HOLD_BYTES 注释），
    /// 不是构造出来的边角形态。
    #[test]
    fn fallback_must_run_after_sniff_flush_not_before() {
        let mut known = std::collections::HashSet::new();
        known.insert("Bash".to_string());
        let mut ctx = StreamContext::new_full("claude-opus-4.6", 10, false, HashMap::new(), known);
        // 显式置位：本测试的前提是"正文被 hold 进 sniff 缓冲"，而 `reclaim_enabled` 默认值
        // 来自进程级 `AtomicBool`（`tool_reclaim_textified_invoke_enabled()`）。若有人把默认
        // 改成 false（或同进程别的测试改了它），文本会直接下发、`body_content_seen` 在 flush
        // 之前就为真 ⇒ 这条顺序守卫**静默失效**（照样 PASS，但已不测顺序）。
        ctx.reclaim_enabled = true;
        let lt = "<";
        // 行首未闭合的 invoke 半块 → 整轮 hold 在 sniff 缓冲，收尾 flush 才当文本吐。
        let held = format!("{lt}invoke name=\"Bash\">{lt}parameter name=\"command\">ls");
        let mut all = ctx.generate_initial_events();
        all.extend(ctx.process_kiro_event(&reasoning_ev("推理绝不能双份下发")));
        all.extend(ctx.process_kiro_event(&text_ev(&held)));
        // 前提校验：正文此刻**确实**还在 sniff 缓冲里、且兜底判据仍认为"无正文"。
        // 少了这两条，`reclaim_enabled` 一旦失效本测试就退化成"随便一轮都能过"。
        assert!(
            !ctx.invoke_sniff_buffer.is_empty(),
            "前提校验：正文应被 hold 在 sniff 缓冲（否则测的不是收尾顺序）"
        );
        assert!(
            !ctx.body_content_seen,
            "前提校验：flush 之前 body_content_seen 必须仍为 false（这正是顺序敏感的原因）"
        );
        all.extend(ctx.generate_final_events());
        let joined: String = all.iter().map(|e| e.to_sse_string()).collect();

        assert!(
            joined.contains("ls"),
            "被 hold 的正文必须在收尾 flush 出来。实际: {joined}"
        );
        assert!(
            !joined.contains("推理绝不能双份下发"),
            "正文只是晚到（hold 在 sniff 缓冲），不是没有 —— 兜底必须排在 flush 之后。实际: {joined}"
        );
    }

    /// ③ thinking 开 → 行为完全不变（reasoning 走 thinking_delta，不受兜底影响）。
    #[test]
    fn reasoning_only_turn_unchanged_when_thinking_enabled() {
        let mut ctx = mk_thinking_ctx();
        let joined = run_turn(&mut ctx, &[reasoning_ev("思考内容")]);

        assert!(
            joined.contains("\"type\":\"thinking_delta\"") && joined.contains("思考内容"),
            "thinking 开启时 reasoning 照旧走 thinking_delta。实际: {joined}"
        );
        // 既有行为：只有 thinking 块时收尾补一个空格 text 块 + stop_reason=max_tokens。
        // 兜底若误伤这条路径（把思考内容再当正文发一遍），这里会抓到第二份"思考内容"。
        assert_eq!(
            joined.matches("思考内容").count(),
            1,
            "思考内容只应出现一次（thinking_delta 里），不得被兜底重复成正文。实际: {joined}"
        );
        assert!(
            joined.contains("max_tokens"),
            "thinking-only 轮的既有收尾（stop_reason=max_tokens）不得被改变。实际: {joined}"
        );
    }

    /// ④ thinking 关 + 只有 tool_use（无 text）→ 不是空响应，兜底不得触发。
    ///
    /// tool_use 轮客户端会去执行工具，本身就是有效产出；此时下发推理是纯泄漏。
    #[test]
    fn tool_use_only_turn_does_not_trigger_reasoning_fallback() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4.6", 10, false, HashMap::new());
        let joined = run_turn(
            &mut ctx,
            &[
                reasoning_ev("推理：应该调用 Bash"),
                Event::ToolUse(crate::kiro::model::events::ToolUseEvent {
                    name: "Bash".into(),
                    tool_use_id: "toolu_1".into(),
                    input: "{\"command\":\"ls\"}".into(),
                    stop: true,
                }),
            ],
        );

        assert!(
            joined.contains("tool_use"),
            "tool_use 块必须下发。实际: {joined}"
        );
        assert!(
            !joined.contains("推理：应该调用 Bash"),
            "有 tool_use 即非空响应，推理必须保持丢弃。实际: {joined}"
        );
    }

    /// 兜底素材有上限，且截断必须落在 UTF-8 边界（中文推理是常态，切错即 panic）。
    #[test]
    fn discarded_reasoning_is_capped_at_utf8_boundary() {
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        // 每帧 3 KiB 级中文，喂到远超上限。
        let frame: String = "推".repeat(4096); // 3 字节/字 → 12 KiB/帧
        for _ in 0..8 {
            ctx.process_kiro_event(&reasoning_ev(&frame));
        }
        assert!(
            ctx.discarded_reasoning.len() <= StreamContext::MAX_DISCARDED_REASONING_BYTES,
            "丢弃素材必须有上限，实际 {} 字节",
            ctx.discarded_reasoning.len()
        );
        // 能走到这里说明每次截断都落在字符边界（否则 push_str 前的切片已 panic）；
        // 再显式确认内容没有被切出半个字符。
        assert!(
            ctx.discarded_reasoning.chars().all(|c| c == '推'),
            "截断后不得出现残缺字符"
        );
    }

    /// buffered 路径（`/cc/v1` 的 ccAutoBuffer）同样要有兜底。
    ///
    /// 两条 ctx 都把事件喂给同一个 `StreamContext`（buffered 只是把产出攒起来），
    /// 所以修在 `generate_final_events` 一处即可覆盖 —— 这条测试就是那个"即可"的凭据。
    #[test]
    fn buffered_path_also_degrades_reasoning_only_turn() {
        let mut ctx = BufferedStreamContext::new(
            "deepseek",
            10,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        ctx.process_and_buffer(&reasoning_ev("buffered 路径的推理"));
        let joined: String = ctx
            .finish_and_get_all_events()
            .iter()
            .map(|e| e.to_sse_string())
            .collect();

        assert!(
            joined.contains("\"type\":\"text_delta\"") && joined.contains("buffered 路径的推理"),
            "buffered 路径也必须避免空响应。实际: {joined}"
        );
    }

    // ===== P5-a：失败的一轮不得被兜底塞进推理文本 =====

    /// 传输层读流 Err（`handlers.rs:1578` 的 `mark_transport_error` → :1587 直接调
    /// `generate_final_events`）：这一轮已经补发过 SSE error 事件、记账按 NetworkError 落库，
    /// 兜底若照样触发，客户端看到的是「error + 一段推理正文」，而面板上这是一次失败
    /// —— 等于给失败凭空记上 output_tokens。
    ///
    /// 用真实链路的置态入口 `mark_transport_error` 而非直接改字段，串也用 reqwest 那类
    /// 错误的实际形态（handlers 传的是 `e.to_string()`）。
    #[test]
    fn transport_error_turn_does_not_get_reasoning_fallback() {
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        let mut all = ctx.generate_initial_events();
        all.extend(ctx.process_kiro_event(&reasoning_ev("失败轮的推理不该下发")));
        // 与 handlers.rs:1578 同构：读流 Err 后置传输失败态，再收尾。
        ctx.mark_transport_error("error reading a body from connection: connection reset by peer");
        all.extend(ctx.generate_final_events());
        let joined: String = all.iter().map(|e| e.to_sse_string()).collect();

        assert!(
            !ctx.completion().is_ok(),
            "前提校验：传输失败态必须已置位，否则本测试测的不是失败路径"
        );
        assert!(
            !joined.contains("失败轮的推理不该下发"),
            "失败的一轮绝不能被兜底补上推理正文（客户端本来就要重试整轮）。实际: {joined}"
        );
        assert_eq!(
            ctx.resolved_usage().output_tokens,
            0,
            "没下发任何内容就不得记 output_tokens"
        );
    }

    /// 对照组：同样的事件序列，只是**没有**失败态 → 兜底必须照旧触发。
    ///
    /// 没有这条对照，上面那条用「把兜底整块删掉」也能过 —— 它证明的是"失败才不发"，
    /// 而不是"永远不发"。
    #[test]
    fn successful_reasoning_only_turn_still_gets_fallback() {
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        let joined = run_turn(&mut ctx, &[reasoning_ev("失败轮的推理不该下发")]);
        assert!(
            ctx.completion().is_ok(),
            "前提校验：本轮应为成功态（对照组）"
        );
        assert!(
            joined.contains("失败轮的推理不该下发"),
            "成功轮的兜底不得被 completion 门误伤。实际: {joined}"
        );
    }

    /// decoder 永久停止（`handlers.rs:1552` / buffered 缓冲溢出 :3455 共用
    /// `mark_decoder_stopped`）：响应必然截断，同样不得补推理。
    ///
    /// buffered 路径走 `finish_and_get_all_events`，与流式共用同一个 `generate_final_events`，
    /// 故这里直接测 buffered —— 覆盖溢出那条真实入口的收尾形态。
    #[test]
    fn decoder_stopped_buffered_turn_does_not_get_reasoning_fallback() {
        let mut ctx = BufferedStreamContext::new(
            "deepseek",
            10,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        ctx.process_and_buffer(&reasoning_ev("截断轮的推理不该下发"));
        // 与 stream.rs:3455（缓冲溢出）/ handlers.rs:1552（解码器停止）同构的置态入口。
        ctx.mark_decoder_stopped("缓冲流事件超出内存上限(疑似异常超长响应)");
        let joined: String = ctx
            .finish_and_get_all_events()
            .iter()
            .map(|e| e.to_sse_string())
            .collect();

        assert!(
            !joined.contains("截断轮的推理不该下发"),
            "截断（decoder 停止 / 缓冲溢出）的一轮不得被兜底补推理。实际: {joined}"
        );
    }

    /// in-band Error 帧（上游在流里直接报错）：`process_kiro_event` 已置 UpstreamError
    /// 并内联发了 error 事件，兜底同样不得触发。
    #[test]
    fn inband_error_turn_does_not_get_reasoning_fallback() {
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        let mut all = ctx.generate_initial_events();
        all.extend(ctx.process_kiro_event(&reasoning_ev("in-band 失败轮的推理")));
        all.extend(ctx.process_kiro_event(&Event::Error {
            error_code: "ThrottlingException".into(),
            error_message: "Too many requests".into(),
        }));
        all.extend(ctx.generate_final_events());
        let joined: String = all.iter().map(|e| e.to_sse_string()).collect();

        assert!(
            !joined.contains("in-band 失败轮的推理"),
            "上游 in-band 报错的一轮不得被兜底补推理。实际: {joined}"
        );
    }

    // ===== P5-b：兜底文本必须过两道正文清洗 =====

    /// #70544 行首泄漏词（`court` 粘 CJK）在兜底文本里同样要被清洗。
    ///
    /// 正文路径由 `process_assistant_response` → `clean_leaked_tokens` 处理，而兜底刻意
    /// 绕开整条正文链 ⇒ 同一形态在兜底里原样泄漏。词表与粘连判据取自
    /// `LEAKED_CONTROL_TOKENS` / `is_leak_glue_char`（实测 `court` 独占行 202 次）。
    #[test]
    fn degraded_reasoning_gets_leaked_token_cleaning() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4.6", 10, false, HashMap::new());
        let joined = run_turn(&mut ctx, &[reasoning_ev("court需要先读文件再改")]);

        assert!(
            joined.contains("需要先读文件再改"),
            "推理正文本体必须保留。实际: {joined}"
        );
        assert!(
            !joined.contains("court"),
            "行首泄漏词必须被清洗，兜底不得绕过 clean_leaked_tokens。实际: {joined}"
        );
        assert!(
            ctx.leaked_stripped > 0,
            "兜底路径的泄漏清洗也要计数（收尾诊断可见，不黑箱）"
        );
    }

    /// 孤立 `</thinking>` 标记不得随兜底文本泄漏给客户端。
    ///
    /// 判据复用 `strip_stray_thinking_end_tags`（正文侧几条收尾路径已在用），
    /// 两侧真正文都保留。
    #[test]
    fn degraded_reasoning_strips_stray_thinking_end_tag() {
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        let joined = run_turn(&mut ctx, &[reasoning_ev("前半段推理</thinking>后半段推理")]);

        assert!(
            joined.contains("前半段推理") && joined.contains("后半段推理"),
            "标记两侧的正文都必须保留。实际: {joined}"
        );
        assert!(
            !joined.contains("</thinking>"),
            "孤立闭标签是标记不是正文，兜底不得原样下发。实际: {joined}"
        );
    }

    /// 清洗把整段吃空时退回丢弃，不发空 text_delta。
    ///
    /// `create_text_delta_events` 自身无空串守卫，发空 delta 只会给客户端多一个无意义的块。
    #[test]
    fn degraded_reasoning_all_stripped_emits_no_empty_delta() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4.6", 10, false, HashMap::new());
        // 整行就是高置信独占泄漏词（LEAK_STANDALONE_TOKENS，实测 court 全独占行）。
        let joined = run_turn(&mut ctx, &[reasoning_ev("court\n")]);
        assert!(
            !joined.contains("\"type\":\"text_delta\""),
            "清洗后只剩空白时不得发 text_delta。实际: {joined}"
        );
        assert_eq!(
            ctx.resolved_usage().output_tokens,
            0,
            "没下发内容就不得记 output_tokens"
        );
    }

    // ===== P5-c：兜底轮的 TTFB 打点 =====

    /// 只有兜底文本的一轮**确实有输出**，`first_token_at` 不得为 None。
    ///
    /// 兜底文本是在 `generate_final_events` 里产生的，不经过 `process_kiro_event` 的打点；
    /// 覆盖它靠的是该函数末尾那一处 `mark_first_token_if_content`。把那一行删掉，
    /// 本测试即 FAIL（面板 `first_token_ms` 落 NULL = 有输出却测不到延迟）。
    #[test]
    fn fallback_only_turn_marks_first_token() {
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        ctx.process_kiro_event(&reasoning_ev("只有推理的一轮"));
        assert!(
            ctx.first_token_at().is_none(),
            "前提校验：!thinking_enabled 下 reasoning 帧不产出任何 delta，此刻不该有打点"
        );
        ctx.generate_final_events();
        assert!(
            ctx.first_token_at().is_some(),
            "兜底文本是真下发的内容，收尾必须补上 TTFB 打点，否则 first_token_ms 恒为 NULL"
        );
    }

    /// 反向：被 completion 门挡住的失败轮**没有**内容产出，打点必须保持 None。
    #[test]
    fn gated_fallback_turn_does_not_mark_first_token() {
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        ctx.process_kiro_event(&reasoning_ev("失败轮的推理"));
        ctx.mark_transport_error("connection reset by peer");
        ctx.generate_final_events();
        assert!(
            ctx.first_token_at().is_none(),
            "失败轮没下发任何内容，first_token_ms 应为 NULL（打点判据只认非空 delta）"
        );
    }

    #[test]
    fn test_strip_dsml_full_marker_in_one_chunk() {
        // DeepSeek 工具协议标记应被整段剥离,正常文本保留。
        let mut ctx = mk_ctx();
        let out = ctx.strip_dsml_markers("先看目录。\n\n<｜DSML｜function_calls｜>后续");
        assert_eq!(
            out, "先看目录。\n\n后续",
            "DSML 完整标记应被剥离,前后正常文本保留"
        );
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_tool_calls_family() {
        let mut ctx = mk_ctx();
        let out = ctx.strip_dsml_markers("<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>正文");
        assert_eq!(out, "正文", "tool_calls 家族标记应全部剥离");
    }

    #[test]
    fn test_strip_dsml_cross_chunk_split() {
        // 标记被上游从中间切成两个 chunk:第一块留半个标记到 tail,第二块拼上闭合后整段剥离。
        let mut ctx = mk_ctx();
        let out1 = ctx.strip_dsml_markers("正常文字<｜DSML｜func");
        assert_eq!(out1, "正常文字", "闭合前只输出正常文字,半个标记留 tail");
        assert!(!ctx.dsml_tail_buffer.is_empty(), "半个标记应留在 tail 缓冲");
        let out2 = ctx.strip_dsml_markers("tion_calls｜>之后");
        assert_eq!(out2, "之后", "拼上闭合后整段标记被剥离,只剩后续文本");
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_split_at_angle_bracket() {
        // 实测坐实的分帧:`<` 单独在前一帧末尾,`｜DSML…` 在下一帧。
        let mut ctx = mk_ctx();
        let out1 = ctx.strip_dsml_markers("创建网页。\n\n<");
        assert_eq!(out1, "创建网页。\n\n", "末尾孤立 < 应 hold 到 tail,不输出");
        assert_eq!(ctx.dsml_tail_buffer, "<");
        let out2 = ctx.strip_dsml_markers("｜DSML｜function_calls｜>正文");
        assert_eq!(out2, "正文", "拼上后 <｜DSML…> 整段剥离");
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_trailing_angle_then_normal() {
        // 末尾 < 被 hold,但下一帧是正常文本(非｜)→ < 应被还原输出,不丢字。
        let mut ctx = mk_ctx();
        let out1 = ctx.strip_dsml_markers("比较 a <");
        assert_eq!(out1, "比较 a ");
        let out2 = ctx.strip_dsml_markers(" b");
        assert_eq!(out2, "< b", "孤立 < 后接正常文本应还原,不误吞");
    }

    #[test]
    fn test_strip_dsml_does_not_touch_normal_text() {
        // 不含 DSML 的正常文本(哪怕有普通 < 号)绝不被改动。
        let mut ctx = mk_ctx();
        let out = ctx.strip_dsml_markers("if a < b && c > d 这是正常代码");
        assert_eq!(out, "if a < b && c > d 这是正常代码");
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_claude_model_never_filtered() {
        // 门控:Claude 系模型完全不剥离——哪怕内容里恰好含 <｜…>(如用户让 Claude 解释 DSML)也原样保留。
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.6", 10, false, HashMap::new());
        let s = "DeepSeek 的标记写作 <｜DSML｜function_calls｜> 你看";
        assert_eq!(
            ctx.strip_dsml_markers(s),
            s,
            "Claude 模型不应剥离任何 <｜…>"
        );
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_keyword_whitelist_preserves_normal_fullwidth() {
        // 国产模型下,<｜ 后不是 DSML/tool/function 关键字的正文(CJK 排版)不被误删。
        let mut ctx = mk_ctx(); // deepseek
        let s = "见 <｜注｜关于x｜> 说明";
        assert_eq!(
            ctx.strip_dsml_markers(s),
            s,
            "非关键字的 <｜…> 属正文,应保留"
        );
    }

    // ===== 文本化 invoke 重组端到端(旧代码上会失败:旧代码把 <invoke> 当纯文本吐,不重组)=====

    /// 造一个开了重组 + 声明了工具 Bash 的 ctx。
    fn mk_reclaim_ctx() -> StreamContext {
        let mut known = std::collections::HashSet::new();
        known.insert("Bash".to_string());
        StreamContext::new_full("claude-opus-4.6", 10, false, HashMap::new(), known)
    }

    /// 判定事件流里是否有结构化 tool_use 的 content_block_start。
    fn has_tool_use_block(events: &[SseEvent]) -> bool {
        events.iter().any(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        })
    }

    #[test]
    fn test_reclaim_textified_invoke_to_tool_use() {
        // 行首完整 <invoke name="Bash"><parameter name="command">ls</parameter></invoke> + 工具名已声明
        // → 应重组成结构化 tool_use,且收尾 stop_reason=tool_use。用 concat 拼避免源码里出现字面工具标签。
        let mut ctx = mk_reclaim_ctx();
        let lt = "<";
        let block = format!(
            "{lt}invoke name=\"Bash\">{lt}parameter name=\"command\">ls -la{lt}/parameter>{lt}/invoke>"
        );
        let mut events = ctx.process_assistant_response(&block);
        events.extend(ctx.flush_invoke_sniff_buffer());
        assert!(
            has_tool_use_block(&events),
            "行首完整 invoke 块应重组成 tool_use"
        );
        assert_eq!(ctx.reclaimed_invoke_count, 1);
        assert_eq!(
            ctx.state_manager.get_stop_reason(),
            "tool_use",
            "重组后 stop_reason 应为 tool_use"
        );
    }

    #[test]
    fn test_reclaim_still_works_when_thinking_enabled() {
        // 回归:thinking 开启时重组层必须同样生效。
        //
        // 真机形态就是这个:Claude Code 恒带 thinking,所以 thinking_enabled=true 是**生产常态**。
        // 旧代码在 `if self.thinking_enabled { return process_content_with_thinking(..) }` 处提前
        // 返回,永远走不到下面的 reclaim 分支 → 文本化 invoke 全部当纯文本吐给客户端,工具不执行
        // (线上实测 textifiedInvokeHits=17 / reclaimedInvokeCalls=1)。
        // 既有 reclaim 测试全传 thinking_enabled=false,恰好绕开了这条唯一的生产路径。
        let mut known = std::collections::HashSet::new();
        known.insert("Bash".to_string());
        let mut ctx = StreamContext::new_full("claude-opus-4.6", 10, true, HashMap::new(), known);
        let lt = "<";
        let block = format!(
            "{lt}invoke name=\"Bash\">{lt}parameter name=\"command\">ls -la{lt}/parameter>{lt}/invoke>"
        );
        let mut events = ctx.process_assistant_response(&block);
        // ⚠️ 必须走 `generate_final_events()`（真实收尾路径），**不要**直接调
        // `flush_invoke_sniff_buffer()`：后者会跳过 generate_final_events 里
        // thinking_buffer 的排空分支，而那里正是最后一处 reclaim 旁路所在
        // （:2451 曾直接 create_text_delta_events）。直接调 flush 的写法能被
        // 「只在 flush 处并回尾巴」的假修复骗过 —— 那种修法在生产上完全无效，
        // 因为收尾时 thinking_buffer 早已被 :2453 清空。
        events.extend(ctx.generate_final_events());
        assert!(
            has_tool_use_block(&events),
            "thinking 开启时行首完整 invoke 块同样应重组成 tool_use(生产常态路径)"
        );
        assert_eq!(ctx.reclaimed_invoke_count, 1);
    }

    #[test]
    fn test_reclaim_works_for_invoke_after_thinking_close_midstream() {
        // 回归:`</thinking>\n\n` 之后紧跟文本化 invoke 块 —— Claude Code 的**生产形态**
        // (先思考,再调工具)。走 process_content_with_thinking 的
        // 「thinking 已提取」分支(:1649),该分支把残留整段交给 emit_non_thinking_text。
        //
        // 注:generate_final_events 里 `</thinking>` 之后那条 remaining 分支(:2425)
        // **不可达**于非空正文 —— find_real_thinking_end_tag_at_buffer_end 要求标签后
        // 全为空白,故 remaining.trim_start() 恒空。不要为它写测试(写了也只会
        // 走到本条覆盖的 :1649 路径上,是假覆盖)。
        let mut known = std::collections::HashSet::new();
        known.insert("Read".to_string());
        let mut ctx = StreamContext::new_full("claude-opus-4.6", 10, true, HashMap::new(), known);
        let lt = "<";
        // `find_real_thinking_end_tag` 要求 `</thinking>` 后紧跟 `\n\n` 才判定为真结束标签。
        let content = format!(
            "{lt}thinking>盘算一下{lt}/thinking>\n\n{lt}invoke name=\"Read\">\
             {lt}parameter name=\"file_path\">/tmp/a.txt{lt}/parameter>{lt}/invoke>"
        );
        let mut events = ctx.process_assistant_response(&content);
        events.extend(ctx.generate_final_events());
        assert!(
            has_tool_use_block(&events),
            "thinking 闭合后紧跟的 invoke 块应重组成 tool_use(Claude Code 生产形态)"
        );
        assert_eq!(ctx.reclaimed_invoke_count, 1);
    }

    #[test]
    fn thinking_tags_stripped_when_client_did_not_request_thinking() {
        // ⭐ 回归（删掉 strip_inline_thinking_when_disabled 的调用即必失败）：
        // 客户端没声明 thinking 时，模型仍可能吐内联 `<thinking>` 标签。
        // 本仓对同一种内容已有口径 —— process_reasoning_content 在 !thinking_enabled
        // 时**直接丢弃整帧**。内联标签若原样穿透就成了「结构化帧丢弃、内联标签泄漏」，
        // 模型的内部推理被当正文吐给用户。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        // `find_real_thinking_end_tag` 要求结束标签后跟 `\n\n`。
        let events =
            ctx.process_assistant_response("前言<thinking>内部推理不该外泄</thinking>\n\n正文");
        let text: String = events
            .iter()
            .filter(|e| e.event == "content_block_delta")
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect();
        assert!(
            !text.contains("<thinking>") && !text.contains("</thinking>"),
            "thinking 标签不该出现在正文里，实际: {text:?}"
        );
        assert!(
            !text.contains("内部推理不该外泄"),
            "思考内容不该作为正文下发（口径须与 process_reasoning_content 的丢弃一致），实际: {text:?}"
        );
        assert!(
            text.contains("前言") && text.contains("正文"),
            "标签外的正文必须完整保留，实际: {text:?}"
        );
    }

    #[test]
    fn stripper_must_not_withhold_invoke_closing_tag() {
        // ⭐ 回归：剥离器扣留"半个标签"尾巴时若无条件扣 `"<thinking>".len()`=10 字节，
        // 会连带扣住 `</invoke>`（**只有 9 字节**）→ 重组层永远看到未闭合的 `<invoke`
        // → 当纯文本吐出 → **工具不执行**。这是 generate_final_events 那条 reclaim
        // 旁路的同型缺陷，第一版实现真的踩了（3 个既有 reclaim 测试当场变红）。
        //
        // 判据落在 partial_tag_suffix_len 上：不含 `<thinking>` 真前缀的尾巴必须立刻放行。
        assert_eq!(
            partial_thinking_tag_suffix_len("...</parameter></invoke>"),
            0,
            "`></invoke>` 这类尾巴不可能长成 thinking 标签，必须零扣留"
        );
        // 真的是半个标签 → 必须扣住，否则半标签会当正文外泄。
        assert_eq!(partial_thinking_tag_suffix_len("正文<thin"), 5);
        assert_eq!(partial_thinking_tag_suffix_len("正文<"), 1);
        // 多字节边界不得panic、不得切坏字符。
        assert_eq!(partial_thinking_tag_suffix_len("正文中文"), 0);
        // 大小写不敏感 + 容属性后仍必须扣住这些半标签，否则形态③④跨 chunk 时会外泄。
        assert_eq!(partial_thinking_tag_suffix_len("正文</THINKI"), 8);
        assert_eq!(partial_thinking_tag_suffix_len("正文<thinking fo"), 12);
        // 上界：属性区超过 MAX_THINKING_TAG_INNER_BYTES 即判定「不是标签」，
        // 否则扣留窗口无界增长 → 整条流停摆（已知问题 #14 同型）。
        let flood = format!(
            "正文<thinking {}",
            "x".repeat(MAX_THINKING_TAG_INNER_BYTES + 5)
        );
        assert_eq!(partial_thinking_tag_suffix_len(&flood), 0);
    }

    /// 🔬 取证探针：真机上到底哪种形态会让 thinking 标签泄漏到客户端可见文本里。
    ///
    /// 用户报告仍看到 thinking tag，而已有修复覆盖的是三种形态
    /// （`</thinking>Answer` 零空白 / 收尾残留蒸发 / 非流式零剥离）。本条把**尚未覆盖**
    /// 的候选形态一次全喂进去，断言"客户端可见文本里绝不含 thinking 标签字面量"，
    /// 让失败输出直接指出是哪一种。
    ///
    /// 不是回归测试，是**测量**：先让它告诉我真形态，再据此写修复与真回归。
    #[test]
    fn probe_which_shapes_leak_thinking_tags() {
        let lt = "<";
        let shapes: Vec<(&str, String)> = vec![
            // ① 孤立结束标签（无开标签）—— 「还没进 thinking 块」那条路径只扣留
            //    `"<thinking>".len()`=10 字节，而 `</thinking>` 是 11 字节。
            ("孤立闭标签", format!("答案开始{lt}/thinking>答案继续")),
            // ② 孤立结束标签跨 chunk 断开
            ("孤立闭标签跨chunk", format!("答案{lt}/think|ing>继续")),
            // ③ 只有开标签、永不闭合（流结束）
            ("开标签不闭合", format!("前言{lt}thinking>思考到一半就断了")),
            // ④ 嵌套/重复开标签
            (
                "重复开标签",
                format!("{lt}thinking>外{lt}thinking>内{lt}/thinking>\n\n正文"),
            ),
            // ⑤ 大写/混合大小写
            (
                "大写标签",
                format!("前言{lt}THINKING>思考{lt}/THINKING>\n\n正文"),
            ),
            // ⑥ 带属性的开标签
            (
                "带属性",
                format!("前言{lt}thinking foo=\"1\">思考{lt}/thinking>\n\n正文"),
            ),
            // ⑦ 标签前后有空格
            (
                "标签内空格",
                format!("前言{lt} thinking >思考{lt} /thinking >\n\n正文"),
            ),
        ];

        let mut leaked: Vec<String> = Vec::new();
        for thinking_enabled in [true, false] {
            for (name, input) in &shapes {
                let mut ctx = StreamContext::new_with_thinking(
                    "claude-sonnet-5",
                    1,
                    thinking_enabled,
                    HashMap::new(),
                );
                let mut ev = ctx.generate_initial_events();
                // 含 `|` 的用例按管道位置切成两个 chunk，模拟跨 chunk 到达。
                if let Some(pos) = input.find('|') {
                    ev.extend(ctx.process_assistant_response(&input[..pos]));
                    ev.extend(ctx.process_assistant_response(&input[pos + 1..]));
                } else {
                    ev.extend(ctx.process_assistant_response(input));
                }
                ev.extend(ctx.generate_final_events());
                // 只取客户端可见的 text_delta（thinking_delta 里出现标签不算泄漏）
                let visible: String = ev
                    .iter()
                    .filter(|e| e.event == "content_block_delta")
                    .filter_map(|e| e.data["delta"]["text"].as_str())
                    .collect();
                let low = visible.to_ascii_lowercase();
                if low.contains("<thinking") || low.contains("</thinking") {
                    leaked.push(format!(
                        "  [thinking_enabled={thinking_enabled}] {name}\n    可见文本: {visible:?}"
                    ));
                }
            }
        }
        assert!(
            leaked.is_empty(),
            "以下形态会把 thinking 标签泄漏进客户端可见文本：\n{}",
            leaked.join("\n")
        );
    }

    /// 走真实流路径取客户端**可见正文**（`text_delta` 拼接）。
    ///
    /// 探针只断言「不含标签字面量」——那挡不住「标签剥了但正文也被吃掉」的假修复。
    /// 下面四条按形态各自断言**精确**可见文本。
    fn visible_text_through_stream(thinking_enabled: bool, chunks: &[&str]) -> String {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-sonnet-5",
            1,
            thinking_enabled,
            HashMap::new(),
        );
        let mut ev = ctx.generate_initial_events();
        for c in chunks {
            ev.extend(ctx.process_assistant_response(c));
        }
        ev.extend(ctx.generate_final_events());
        ev.iter()
            .filter(|e| e.event == "content_block_delta")
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect()
    }

    /// ⭐ 回归 · 泄漏形态①：**孤立闭标签**（没有配对开标签）。
    ///
    /// 「还没进 thinking 块」那条路径原先无条件扣留 `"<thinking>".len()` = **10 字节**
    /// 当可能的半标签，而 `</thinking>` 是 **11 字节** ⇒ 整条标签穿透进 `text_delta`，
    /// 客户端逐字看到 `答案开始</thinking>答案继续`（实测）。
    ///
    /// 处置口径：孤立闭标签两侧都是**真正文**，故只丢标签本身、内容全留。
    ///
    /// 把 `partial_thinking_tag_suffix_len` 换回「固定扣 10 字节」→ 本测试必 FAILED。
    #[test]
    fn stray_end_tag_without_open_tag_must_be_stripped_keeping_both_sides() {
        for thinking_enabled in [true, false] {
            assert_eq!(
                visible_text_through_stream(thinking_enabled, &["答案开始</thinking>答案继续"]),
                "答案开始答案继续",
                "thinking={thinking_enabled}：孤立闭标签必须剥掉，两侧正文都不得丢"
            );
            // 标签后带段落分隔：与真结束标签同口径，吃掉 `\n\n`
            assert_eq!(
                visible_text_through_stream(thinking_enabled, &["答案</thinking>\n\n继续"]),
                "答案继续",
                "thinking={thinking_enabled}：孤立闭标签后的段落分隔应被一并消耗"
            );
            // 对照：被反引号包裹的是**正文在引用标签**，不得剥
            assert_eq!(
                visible_text_through_stream(thinking_enabled, &["用 `</thinking>` 收尾"]),
                "用 `</thinking>` 收尾",
                "thinking={thinking_enabled}：引用包裹的标签是正文，不得当标记剥掉"
            );
        }
    }

    /// ⭐ 回归 · 泄漏形态②：孤立闭标签**跨 chunk 断开**。
    ///
    /// 扣留窗口必须够得住 `</thinking>` 的 11 字节，否则前半截（`</think`）先被当正文
    /// 发走，客户端看到 `答案</thinking>继续`（实测）。
    ///
    /// 逐字节切开喂入 —— 任何一个切点上扣留不足都会漏出半个标签。
    #[test]
    fn stray_end_tag_split_across_chunks_must_not_leak_half_tag() {
        let input = "答案</thinking>继续";
        for thinking_enabled in [true, false] {
            // 在每个字符边界切成两段，穷举所有跨 chunk 形态
            for cut in 1..input.len() {
                if !input.is_char_boundary(cut) {
                    continue;
                }
                let got =
                    visible_text_through_stream(thinking_enabled, &[&input[..cut], &input[cut..]]);
                assert_eq!(
                    got, "答案继续",
                    "thinking={thinking_enabled} 切点={cut}：跨 chunk 的孤立闭标签不得泄漏半截"
                );
            }
        }
    }

    /// ⭐ 回归 · 泄漏形态③：**大写 / 混合大小写**标签。
    ///
    /// 原实现用写死字面量 `"<thinking>"` 做精确 `find`，全文件 thinking 路径上**一处
    /// 大小写归一化都没有** ⇒ `<THINKING>` 匹配不上 ⇒ 标签**连同思考内容一起**泄漏：
    /// 客户端看到 `前言<THINKING>思考</THINKING>\n\n正文`（实测，"思考"二字进了正文）。
    ///
    /// 把匹配改回大小写敏感 → 本测试必 FAILED。
    #[test]
    fn uppercase_thinking_tags_must_be_recognized() {
        for (name, chunks) in [
            ("全大写", vec!["前言<THINKING>思考</THINKING>\n\n正文"]),
            ("混合", vec!["前言<Thinking>思考</ThInKiNg>\n\n正文"]),
            (
                "大写跨chunk",
                vec!["前言<THINK", "ING>思考</THINKING>\n\n正文"],
            ),
        ] {
            for thinking_enabled in [true, false] {
                assert_eq!(
                    visible_text_through_stream(thinking_enabled, &chunks),
                    "前言正文",
                    "形态「{name}」thinking={thinking_enabled}：\
                     大写标签与其思考内容都不得进可见正文"
                );
            }
        }
        // thinking 开启时思考内容必须**进 thinking 块**（不是被丢掉）
        let mut ctx = StreamContext::new_with_thinking("claude-sonnet-5", 1, true, HashMap::new());
        let mut ev = ctx.generate_initial_events();
        ev.extend(ctx.process_assistant_response("前言<THINKING>思考</THINKING>\n\n正文"));
        ev.extend(ctx.generate_final_events());
        assert_eq!(
            collect_thinking_content(&ev),
            "思考",
            "大写标签内的内容应作为 thinking 下发，而不是被静默丢弃"
        );
    }

    /// ⭐ 回归 · 泄漏形态④：**带属性**的开标签。
    ///
    /// 精确 `find("<thinking>")` 匹配不上 `<thinking foo="1">` ⇒ 标签连同思考内容一起
    /// 泄漏（实测：`前言<thinking foo="1">思考</thinking>\n\n正文`）。
    ///
    /// 这条同时钉住**长度必须取自匹配结果**：属性使开标签变成 18 字节，若调用方仍按
    /// 写死的 10 字节跳过标签，属性残片 `foo="1">` 会留在缓冲里当思考/正文 → FAILED。
    #[test]
    fn thinking_tag_with_attributes_must_be_recognized() {
        for (name, chunks) in [
            (
                "单属性",
                vec!["前言<thinking foo=\"1\">思考</thinking>\n\n正文"],
            ),
            (
                "多属性",
                vec!["前言<thinking a=\"1\" b='2'>思考</thinking>\n\n正文"],
            ),
            (
                "闭标签带空白",
                vec!["前言<thinking>思考</thinking >\n\n正文"],
            ),
            (
                "属性跨chunk",
                vec!["前言<thinking fo", "o=\"1\">思考</thinking>\n\n正文"],
            ),
        ] {
            for thinking_enabled in [true, false] {
                assert_eq!(
                    visible_text_through_stream(thinking_enabled, &chunks),
                    "前言正文",
                    "形态「{name}」thinking={thinking_enabled}：\
                     带属性标签、属性残片、思考内容都不得进可见正文"
                );
            }
        }
        // 长度取自匹配结果的直接证据：属性残片不得出现在 thinking 内容里
        let mut ctx = StreamContext::new_with_thinking("claude-sonnet-5", 1, true, HashMap::new());
        let mut ev = ctx.generate_initial_events();
        ev.extend(
            ctx.process_assistant_response("前言<thinking foo=\"1\">思考</thinking>\n\n正文"),
        );
        ev.extend(ctx.generate_final_events());
        assert_eq!(
            collect_thinking_content(&ev),
            "思考",
            "属性残片（foo=\"1\">）不得留在 thinking 内容里 —— 说明跳标签用的是写死长度"
        );
    }

    /// ⭐ 回归：放宽匹配**不得**误伤散文里的 `<`，也不得让扣留窗口无界增长。
    ///
    /// 容属性后「可能是半个标签」失去了 10 字节的天然上界（`<thinking foo="...">` 可任意长）。
    /// 无上界时一个永不闭合的 `<thinking xxxx…` 会把整条可见文本囤死不下发 ——
    /// 这正是已知问题 #14（`invoke_sniff_buffer` 无界持有 → 流停摆）的同型。
    #[test]
    fn relaxed_matching_must_not_swallow_prose_or_stall_stream() {
        for thinking_enabled in [true, false] {
            // 散文里的比较运算符：`<` 后接空格/字母都不可能长成 thinking 标签
            assert_eq!(
                visible_text_through_stream(thinking_enabled, &["条件 a < b 时成立"]),
                "条件 a < b 时成立",
                "thinking={thinking_enabled}：散文里的 `<` 不得被吞"
            );
            // 别的标签名不得被当成 thinking
            assert_eq!(
                visible_text_through_stream(thinking_enabled, &["<thinkingfoo>正文"]),
                "<thinkingfoo>正文",
                "thinking={thinking_enabled}：`<thinkingfoo>` 是别的标签名，不得误剥"
            );
            // 超长属性区（永不闭合）必须放行为正文，不得囤住整条流。
            //
            // ⚠️ 填充串刻意**不用**单字符重复（`"x".repeat(200)`）：那会命中与本 case 无关的
            // 反刷屏熔断 `detect_structural_flood`（同一字母连续 ≥阈值即截断并置
            // `stray_guard_tripped`），于是断言失败的原因不是标签匹配。用递增编号做填充，
            // 既无单字符游程也无重复 token，且远超 `MAX_THINKING_TAG_INNER_BYTES`。
            let filler: String = (0..40).map(|i| format!("a{i} ")).collect();
            assert!(
                filler.len() > MAX_THINKING_TAG_INNER_BYTES,
                "填充串必须超过属性区上限才能测到"
            );
            let flood = format!("正文<thinking {filler}");
            let got = visible_text_through_stream(thinking_enabled, &[&flood]);
            assert!(
                got.contains("正文") && got.contains("a39"),
                "thinking={thinking_enabled}：永不闭合的超长伪标签必须放行为正文（否则流停摆），\
                 实际 {} 字节: {got:?}",
                got.len()
            );
        }
    }

    /// ⭐ 回归（**走真实流路径**）：thinking 关闭 + `</thinking>` 后**紧跟普通字符**时，
    /// 答案不得被永久丢弃。
    ///
    /// 这是 `strip_inline_thinking_when_disabled` 里最严重的形态：严格判据要求结束标签后
    /// 跟「含换行的空白」或 `<`，而 `</thinking>ANSWER` 两者都不是 ⇒ 判据返回 `None` ⇒
    /// 剥离器走「整段都还是思考内容」分支**丢弃全部内容** ⇒ 客户端收到**空回答**。
    /// 而后续 chunk 再来多少内容都改不了这个位置的判据（普通字符已就位）——
    /// 所以这不是"等一等就好"，是**永久丢弃**，且面板记为一次成功，完全无痕。
    ///
    /// 删掉 `find_permanently_unsatisfiable_end_tag` 那个分支 → 本测试必 FAILED。
    #[test]
    fn stripper_must_not_drop_answer_when_end_tag_hugs_text() {
        for (name, input, want) in [
            ("紧跟字母", "<thinking>盘算</thinking>ANSWER", "ANSWER"),
            ("紧跟中文", "<thinking>盘算</thinking>答案在此", "答案在此"),
            (
                "前言+紧跟",
                "前言<thinking>盘算</thinking>ANSWER",
                "前言ANSWER",
            ),
        ] {
            let mut ctx =
                StreamContext::new_with_thinking("claude-sonnet-5", 1, false, HashMap::new());
            let mut ev = ctx.generate_initial_events();
            ev.extend(ctx.process_assistant_response(input));
            ev.extend(ctx.generate_final_events());
            let text: String = ev
                .iter()
                .filter(|e| e.event == "content_block_delta")
                .filter_map(|e| e.data["delta"]["text"].as_str())
                .collect();
            assert_eq!(
                text, want,
                "形态「{name}」：thinking 关闭时答案必须完整下发（旧代码整段丢弃 → 空回答）"
            );
        }
    }

    /// ⭐ 回归：thinking 关闭时剥离器的**残留收尾**不得静默蒸发。
    ///
    /// `generate_final_events` 里原有的 flush 门是
    /// `self.thinking_enabled && !self.thinking_buffer.is_empty()`，而剥离器**只在
    /// `!thinking_enabled` 时**往同一个 `thinking_buffer` 写 —— 两个条件**互斥** ⇒
    /// 那条 flush 对剥离器的残留**永远不执行**。
    ///
    /// 删掉 `!self.thinking_enabled && !self.thinking_buffer.is_empty()` 那个分支
    /// → 本测试必 FAILED。
    #[test]
    fn stripper_residue_must_not_evaporate_at_stream_end() {
        // ① 尾巴是被 partial_tag_suffix_len 扣住的孤立 `<`（散文 "条件 a < b" 落在流末尾）。
        //    EOF 时它已确定不是标签 —— 是正文的一部分，必须下发。
        {
            let mut ctx =
                StreamContext::new_with_thinking("claude-sonnet-5", 1, false, HashMap::new());
            let mut ev = ctx.generate_initial_events();
            ev.extend(ctx.process_assistant_response("条件 a <"));
            ev.extend(ctx.generate_final_events());
            let text: String = ev
                .iter()
                .filter(|e| e.event == "content_block_delta")
                .filter_map(|e| e.data["delta"]["text"].as_str())
                .collect();
            assert_eq!(
                text, "条件 a <",
                "末尾孤立 `<` 必须在收尾时补发，不得静默吞字"
            );
        }
        // ② 未闭合的 thinking 块：思考本体丢弃（客户端没要），但**标签之后**的正文要下发。
        //    分两个 chunk 送达，逼真实跨 chunk 状态。
        {
            let mut ctx =
                StreamContext::new_with_thinking("claude-sonnet-5", 1, false, HashMap::new());
            let mut ev = ctx.generate_initial_events();
            ev.extend(ctx.process_assistant_response("<thinking>盘算一"));
            ev.extend(ctx.process_assistant_response("下"));
            ev.extend(ctx.generate_final_events());
            let text: String = ev
                .iter()
                .filter(|e| e.event == "content_block_delta")
                .filter_map(|e| e.data["delta"]["text"].as_str())
                .collect();
            assert!(
                !text.contains("盘算") && !text.contains("<thinking>"),
                "未闭合思考块的内容与标签都不得下发，实际: {text:?}"
            );
        }
    }

    /// ⭐ 回归：EOF 兜底只在**真的到了流末尾**才放宽，不得在流式期间抢判。
    ///
    /// 对照条：`split_unclosed_thinking_residue_at_eof` 用的是字面量 `</thinking>`，
    /// 若把它挪进流式热路径，正文里顺口提到该标签就会被当成真结束。
    #[test]
    fn eof_residue_split_is_literal_and_only_at_eof() {
        // 有标签 → 标签之后的内容作为可见尾巴（trim_start 掉紧随的空白）
        assert_eq!(
            split_unclosed_thinking_residue_at_eof("思考</thinking>\n\nANSWER"),
            "ANSWER"
        );
        assert_eq!(
            split_unclosed_thinking_residue_at_eof("思考</thinking>ANSWER"),
            "ANSWER"
        );
        // 无标签 → 整段都在未闭合思考块里，全部丢弃（口径同 process_reasoning_content 丢帧）
        assert_eq!(
            split_unclosed_thinking_residue_at_eof("思考到一半就断了"),
            ""
        );
        // 多字节内容不得 panic
        assert_eq!(split_unclosed_thinking_residue_at_eof("中文思考"), "");
    }

    /// ⭐ 回归：「等也没用」判据必须只对**永久不可满足**的位置放行。
    ///
    /// 放宽过头会把「该等」的形态提前判定，把段落分隔的后半个换行漏进正文
    /// （`\n` 在本 chunk、第二个 `\n` 在下一个）。
    #[test]
    fn permanently_unsatisfiable_end_tag_distinguishes_wait_from_hopeless() {
        // 该等：标签后什么都没到
        assert_eq!(
            find_permanently_unsatisfiable_end_tag("思考</thinking>"),
            None
        );
        // 该等：空白顶到缓冲末尾且未攒够 `\n\n`
        assert_eq!(
            find_permanently_unsatisfiable_end_tag("思考</thinking>\n"),
            None
        );
        assert_eq!(
            find_permanently_unsatisfiable_end_tag("思考</thinking> "),
            None
        );
        // 交给严格判据：含换行的空白 / 紧跟 `<`
        assert_eq!(
            find_permanently_unsatisfiable_end_tag("思考</thinking>\n\nA"),
            None
        );
        assert_eq!(
            find_permanently_unsatisfiable_end_tag("思考</thinking><invoke"),
            None
        );
        // 等也没用：普通字符已就位
        assert_eq!(
            tag_start(find_permanently_unsatisfiable_end_tag(
                "思考</thinking>ANSWER"
            )),
            Some("思考".len())
        );
        // 纯行内空白后还有内容 → 严格判据判为"正文顺口提到标签"，这里不抢
        assert_eq!(
            find_permanently_unsatisfiable_end_tag("思考</thinking> more"),
            None
        );
    }

    /// ⭐ 回归：**非流式**路径同样必须剥离内联 thinking。
    ///
    /// 剥离逻辑此前只在流式路径存在，非流式 `handlers.rs` 的 `!thinking_enabled` 分支把
    /// 上游文本**原样**塞进响应 ⇒ 标签与模型内部推理逐字泄漏。
    ///
    /// 本条同时钉住「判据必须复用」——若非流式另写一套匹配，下面任一形态都会漂移。
    #[test]
    fn non_stream_path_strips_inline_thinking_when_disabled() {
        for (name, input, want) in [
            ("双换行", "<thinking>盘算</thinking>\n\nANSWER", "ANSWER"),
            ("单换行", "<thinking>盘算</thinking>\nANSWER", "ANSWER"),
            ("紧跟正文", "<thinking>盘算</thinking>ANSWER", "ANSWER"),
            (
                "紧跟标签",
                "<thinking>盘算</thinking><invoke n=\"R\">",
                "<invoke n=\"R\">",
            ),
            (
                "前言保留",
                "前言<thinking>盘算</thinking>\n\nANSWER",
                "前言ANSWER",
            ),
            ("无标签原样", "普通回答", "普通回答"),
            ("未闭合全丢", "<thinking>盘算到一半", ""),
        ] {
            assert_eq!(
                strip_thinking_from_complete_text(input),
                want,
                "形态「{name}」非流式剥离结果不符"
            );
        }
        // 被反引号包裹的标签是**正文里的引用**，不得当真标签剥掉（与流式判据一致）。
        let quoted = "用 `<thinking>` 包裹推理";
        assert_eq!(strip_thinking_from_complete_text(quoted), quoted);
    }

    /// ⭐ 源码级守卫：非流式 `!thinking_enabled` 分支**必须**调剥离函数。
    ///
    /// 上面那条只断言纯函数自身 —— 把 `handlers.rs` 的调用点改回
    /// `"text": text_content` 它依然全绿（纸面测试，本仓已踩过四次）。
    /// 这条钉住调用点本身。
    #[test]
    fn non_stream_handler_must_call_the_stripper() {
        let src = include_str!("handlers.rs");
        // needle 运行时拼接，避免本断言自身成为 grep 的假命中源。
        let call = ["strip_thinking_from_complete_text", "(&text_content)"].concat();
        assert!(
            src.contains(&call),
            "handlers.rs 的非流式分支必须调 strip_thinking_from_complete_text(&text_content)，\
             否则 thinking 标签在非流式路径逐字泄漏"
        );
        // 承重：不得再有「原样塞入」的写法（那正是缺陷本体）。
        //
        // ⚠️ 必须先切掉注释行再搜：第一版裸搜全文，命中的是**修复本身留下的那句
        // 说明注释**（"此前这里是 ... 原样塞入"）⇒ 修复正确却报 FAILED。
        // 这正是本仓反复踩的「锚点选到散文」，只是方向相反（假阳性而非假阴性）。
        let code_only: String = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let raw = ["\"text\": text_content"].concat();
        assert!(
            !code_only.contains(&raw),
            "handlers.rs 不得把 text_content 原样塞进响应（内联 thinking 标签会泄漏）"
        );
    }

    #[test]
    fn test_reclaim_gated_by_unknown_tool_name() {
        // 工具名硬护栏:解析出的工具名不在声明表里 → 不重组,当普通文本吐(宁可漏捞不误执行)。
        let mut known = std::collections::HashSet::new();
        known.insert("Read".to_string()); // 只声明 Read,没声明 Bash
        let mut ctx = StreamContext::new_full("claude-opus-4.6", 10, false, HashMap::new(), known);
        let lt = "<";
        let block = format!(
            "{lt}invoke name=\"Bash\">{lt}parameter name=\"x\">1{lt}/parameter>{lt}/invoke>"
        );
        let mut events = ctx.process_assistant_response(&block);
        events.extend(ctx.flush_invoke_sniff_buffer());
        assert!(!has_tool_use_block(&events), "未声明的工具名不应被重组执行");
        assert_eq!(ctx.reclaimed_invoke_count, 0);
    }

    #[test]
    fn test_reclaim_split_across_chunks() {
        // 跨 chunk 切分的 invoke 块:分片到达仍应重组(sniff 缓冲 hold 到闭合)。
        let mut ctx = mk_reclaim_ctx();
        let lt = "<";
        let mut events = Vec::new();
        events.extend(ctx.process_assistant_response(&format!("{lt}invoke name=\"Ba")));
        events.extend(
            ctx.process_assistant_response(&format!("sh\">{lt}parameter name=\"command\">echo hi")),
        );
        events.extend(ctx.process_assistant_response(&format!("{lt}/parameter>{lt}/invoke>")));
        events.extend(ctx.flush_invoke_sniff_buffer());
        assert!(
            has_tool_use_block(&events),
            "跨 chunk 分片的 invoke 应重组成 tool_use"
        );
    }

    #[test]
    fn test_reclaim_disabled_when_no_tools_declared() {
        // 未声明任何工具(known 空)→ 不进重组路径,<invoke> 原样当文本吐(new_with_thinking 空集=不启用)。
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4.6", 10, false, HashMap::new());
        let lt = "<";
        let block = format!(
            "{lt}invoke name=\"Bash\">{lt}parameter name=\"c\">x{lt}/parameter>{lt}/invoke>"
        );
        let events = ctx.process_assistant_response(&block);
        assert!(!has_tool_use_block(&events), "无声明工具时不重组");
        assert!(
            ctx.invoke_sniff_buffer.is_empty(),
            "不启用重组则不进 sniff 缓冲"
        );
    }

    // ===== 结构性 stray 熔断(治 course/课 打地鼠 + 修 thinking/无工具盲区,旧代码上会失败)=====

    #[test]
    fn test_structural_flood_single_line_cjk() {
        // 单行连写「课」×100(无换行)——旧独占行匹配漏,结构性检测应抓到并从游程起点截断。
        let s = format!("正常开头 {}", "课".repeat(100));
        let cut = detect_structural_flood(&s);
        assert!(cut.is_some(), "单行连写课刷屏应被结构性检测命中");
    }

    #[test]
    fn test_structural_flood_multichar_course() {
        // "coursecourse…" ×40(词表里根本没 course)——多字符 token 连写应被抓。
        let s = "course".repeat(40);
        assert!(
            detect_structural_flood(&s).is_some(),
            "course 连写应被结构性检测命中(不靠词表)"
        );
    }

    #[test]
    fn test_structural_flood_normal_text_safe() {
        // 正常文本(含少量重复词)不应误判。
        assert!(
            detect_structural_flood("这是一段正常的中文回复,讲解代码逻辑和实现细节。").is_none()
        );
        assert!(detect_structural_flood("the quick brown fox jumps over the lazy dog").is_none());
        assert!(
            detect_structural_flood("aaa bbb ccc").is_none(),
            "短重复但未达阈值不误判"
        );
    }

    #[test]
    fn test_observe_stray_leak_forms() {
        let mut sa = 0u32;
        let mut il = 0u32;
        // 独占行:course 单独一行 → standalone。
        observe_stray_leak_forms("正常\ncourse\n继续", &mut sa, &mut il);
        assert_eq!(sa, 1, "course 独占行应计 standalone");
        // 句中紧贴 CJK:`重读course了` 里 course 前后都贴 CJK → inline。
        let (mut sa2, mut il2) = (0u32, 0u32);
        observe_stray_leak_forms("重读course了", &mut sa2, &mut il2);
        assert_eq!(il2, 1, "句中紧贴 CJK 的 course 应计 inline");
        // 正常英文(有空格分隔)不误判:"the course is" 里 course 两侧是空格非 CJK。
        let (mut sa3, mut il3) = (0u32, 0u32);
        observe_stray_leak_forms("the course is good", &mut sa3, &mut il3);
        assert_eq!(sa3, 0, "正常英文散文不计 standalone");
        assert_eq!(il3, 0, "有空格分隔的正常 course 不计 inline");
        // 完全不含 stray 词:零开销快路径 + 零计数。
        let (mut sa4, mut il4) = (0u32, 0u32);
        observe_stray_leak_forms("一段完全正常的中文回复讲解逻辑", &mut sa4, &mut il4);
        assert_eq!(sa4 + il4, 0);
    }

    #[test]
    fn test_stray_guard_covers_thinking_path() {
        // 核心盲区修复:thinking 开着时,课刷屏也要被熔断(旧代码 thinking 提前 return 完全绕过)。
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4.6", 10, true, HashMap::new());
        let flood = format!("{}", "课".repeat(200));
        let events = ctx.process_assistant_response(&flood);
        assert!(ctx.stray_guard_tripped, "thinking 路径的课刷屏也应触发熔断");
        // 熔断后本轮应几乎不吐正文(截断在游点起点)。
        let _ = events;
    }

    #[test]
    fn test_thinking_buffer_bounded_on_whitespace_flood() {
        // review Finding 5 修复:上游持续吐纯空白(无 <thinking>)时 thinking_buffer 不应无界增长。
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4.6", 10, true, HashMap::new());
        // 每次喂一大块纯空白(不含 <thinking>),多轮累积。旧代码 buffer 只涨不裁。
        for _ in 0..20 {
            let _ = ctx.process_content_with_thinking(&" ".repeat(50_000));
        }
        assert!(
            ctx.thinking_buffer.len() <= MAX_THINKING_BUFFER_BYTES + 50_000,
            "纯空白洪水下 thinking_buffer 应被上限约束,实测 {} 字节",
            ctx.thinking_buffer.len()
        );
    }

    #[test]
    fn test_stray_guard_covers_no_tools_path() {
        // 核心盲区修复:无工具声明请求(known_tool_names 空)的课刷屏也要被处理。
        // 用单行连写(泄漏清洗器只剥行首独占,够不到单行连写)专门验证 guard 生效——
        // 逐行独占的课会被 clean_leaked_tokens 先剥掉(那也是有效清除路径),故此处用连写测 guard。
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4.6", 10, false, HashMap::new());
        let flood = format!("正文{}", "课".repeat(200));
        let events = ctx.process_assistant_response(&flood);
        assert!(
            ctx.stray_guard_tripped,
            "无工具请求的单行课连写刷屏应触发 guard 熔断"
        );
        // 熔断后吐出的文本里课的数量应远少于 200(截断在游程起点)。
        let emitted: String = events
            .iter()
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect();
        assert!(emitted.matches('课').count() < 200, "熔断应截断掉大部分课");
    }

    #[test]
    fn test_strip_dsml_flush_recovers_leftover() {
        // 末尾孤立 < 被 hold 后,若流结束(无下一帧),flush_dsml_tail 应把它作为普通文本补发,不吞字。
        let mut ctx = mk_ctx();
        let out = ctx.strip_dsml_markers("结尾是 a <");
        assert_eq!(out, "结尾是 a ");
        assert_eq!(ctx.dsml_tail_buffer, "<");
        // 模拟流结束:flush 应产出含 "<" 的 text_delta 事件,tail 清空。
        let flushed = ctx.flush_dsml_tail();
        assert!(!flushed.is_empty(), "flush 应补发残留 <,不静默吞字");
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_marker_no_gt_discarded_on_flush() {
        // 实测漏的形态:<｜DSML｜function_calls 单帧到达且不以 > 收尾(DeepSeek 标记本就以后续为界)。
        // 应被识别为标记 hold 到 tail 且标记 is_marker,流结束 flush 时**丢弃不补发**,不泄漏。
        let mut ctx = mk_ctx(); // deepseek
        let out = ctx.strip_dsml_markers("我来看看目录。\n\n<｜DSML｜function_calls");
        assert_eq!(out, "我来看看目录。\n\n", "正文保留,标记 hold 不输出");
        assert!(ctx.dsml_tail_is_marker, "应标记为确认标记");
        let flushed = ctx.flush_dsml_tail();
        assert!(
            flushed.is_empty(),
            "确认标记的残留 flush 时丢弃,绝不当正文补发(否则泄漏)"
        );
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_unclosed_marker_bounded_flush() {
        // 是关键字但超长不闭合 → 不无界囤积,放行为正文(防吞正文/防无界)。
        let mut ctx = mk_ctx();
        let long = format!("<｜tool{}", "x".repeat(60)); // >DSML_TAIL_MAX 且无 >
        let out = ctx.strip_dsml_markers(&long);
        assert!(out.contains("tool"), "超长未闭合应放行为正文,不吞");
    }

    #[test]
    fn test_dsml_flush_after_thinking_close_keeps_block_order() {
        // 回归(对抗 review #2):国产模型 + thinking 开启,流在 thinking 块内结束,且最后一帧
        // 内容以孤立 `<` 收尾(被 strip_dsml_markers hold 进 dsml_tail_buffer,不进 thinking_buffer)。
        // generate_final_events 必须先 stop thinking 块,再把 DSML 残留作为 text 块 start——
        // 否则会出现「text 块 start(大索引)→ thinking 块 stop(小索引)」交错,违反 Anthropic
        // 「先 stop 当前块再 start 下一块」契约,CC 解析报错。
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, true, HashMap::new());

        // 驱动进入 thinking 块并停在块内;末尾孤立 `<` 会被 DSML 逻辑 hold 到 tail。
        let _ = ctx.process_assistant_response("<thinking>我在想 <");
        assert!(ctx.in_thinking_block, "应仍处于 thinking 块内");
        assert_eq!(
            ctx.dsml_tail_buffer, "<",
            "末尾孤立 < 应被 hold 到 DSML tail,而非进 thinking_buffer"
        );

        let events = ctx.generate_final_events();

        // 收集块生命周期事件,校验:同一 index 的 text start 必须在 thinking stop 之后。
        // 找出 thinking 块 stop 的位置与任何 text 块 start 的位置。
        let mut thinking_stop_pos: Option<usize> = None;
        let mut text_start_pos: Option<usize> = None;
        for (pos, e) in events.iter().enumerate() {
            let idx = e.data.get("index").and_then(|v| v.as_i64());
            match e.event.as_str() {
                "content_block_stop" => {
                    // thinking 块索引来自 ctx.thinking_block_index
                    if idx == ctx.thinking_block_index.map(|i| i as i64) {
                        thinking_stop_pos = Some(pos);
                    }
                }
                "content_block_start" => {
                    let is_text = e
                        .data
                        .get("content_block")
                        .and_then(|cb| cb.get("type"))
                        .and_then(|t| t.as_str())
                        == Some("text");
                    if is_text && text_start_pos.is_none() {
                        text_start_pos = Some(pos);
                    }
                }
                _ => {}
            }
        }

        // 残留 `<` 会作为 text 块补发,thinking 块必然先 stop。
        let ts = thinking_stop_pos.expect("thinking 块应被 stop");
        if let Some(txs) = text_start_pos {
            assert!(
                ts < txs,
                "thinking 块 stop(pos={}) 必须早于 DSML 残留 text 块 start(pos={}),否则块顺序交错",
                ts,
                txs
            );
        }
        assert!(ctx.dsml_tail_buffer.is_empty(), "flush 后 tail 应清空");
    }

    #[test]
    fn test_sse_state_manager_message_start() {
        let mut manager = SseStateManager::new();

        // 第一次应该成功
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_some());

        // 第二次应该被跳过
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_none());
    }

    #[test]
    fn test_sse_state_manager_block_lifecycle() {
        let mut manager = SseStateManager::new();

        // 创建块
        let events = manager.handle_content_block_start(0, "text", json!({}));
        assert_eq!(events.len(), 1);

        // delta
        let event = manager.handle_content_block_delta(0, json!({}));
        assert!(event.is_some());

        // stop
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_some());

        // 重复 stop 应该被跳过
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_none());
    }

    #[test]
    fn test_tool_name_reverse_mapping_in_stream() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut map = HashMap::new();
        map.insert(
            "short_abc12345".to_string(),
            "mcp__very_long_original_tool_name".to_string(),
        );

        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, map);
        let _ = ctx.generate_initial_events();

        // 模拟 Kiro 返回短名称的 tool_use
        let tool_event = Event::ToolUse(ToolUseEvent {
            name: "short_abc12345".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"{"key":"value"}"#.to_string(),
            stop: true,
        });

        let events = ctx.process_kiro_event(&tool_event);

        // content_block_start 中的 name 应该是原始长名称
        let start_event = events
            .iter()
            .find(|e| e.event == "content_block_start")
            .unwrap();
        assert_eq!(
            start_event.data["content_block"]["name"], "mcp__very_long_original_tool_name",
            "应还原为原始工具名称"
        );
    }

    /// 跑一串 tool_use 帧，返回 (拼接出的 partial_json 全文, 发出的 input_json_delta 事件数)。
    /// 根治后应恒为「单个 delta 在 stop 时发出」，故 delta 数应为 0(空参)或 1(有参)。
    fn run_tool_frames(ctx: &mut StreamContext, frames: &[(&str, bool)]) -> (String, usize) {
        use crate::kiro::model::events::ToolUseEvent;
        let mut out = String::new();
        let mut delta_count = 0usize;
        for (input, stop) in frames {
            let ev = Event::ToolUse(ToolUseEvent {
                name: "t".to_string(),
                tool_use_id: "toolu_x".to_string(),
                input: input.to_string(),
                stop: *stop,
            });
            for e in ctx.process_kiro_event(&ev) {
                if e.event == "content_block_delta" && e.data["delta"]["type"] == "input_json_delta"
                {
                    out.push_str(e.data["delta"]["partial_json"].as_str().unwrap_or(""));
                    delta_count += 1;
                }
            }
        }
        (out, delta_count)
    }

    /// 兼容旧断言：只取拼接全文。
    fn collect_tool_partial_json(ctx: &mut StreamContext, frames: &[(&str, bool)]) -> String {
        run_tool_frames(ctx, frames).0
    }

    #[test]
    fn test_tool_input_cumulative_snapshots() {
        // 上游发累积快照：每帧是"到目前为止的完整 JSON"。转发拼接后不应重复,应恰为最终完整 JSON。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let joined = collect_tool_partial_json(
            &mut ctx,
            &[
                (r#"{"a""#, false),
                (r#"{"a":1"#, false),
                (r#"{"a":1,"b":2}"#, true),
            ],
        );
        assert_eq!(
            joined, r#"{"a":1,"b":2}"#,
            "累积模式:拼接后应为完整 JSON,无重复"
        );
    }

    #[test]
    fn test_tool_input_repeated_final_frame() {
        // 累积模式常见收尾：stop 帧重复带上完整 JSON（与上一帧相同）→ 不应重复发。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let joined = collect_tool_partial_json(
            &mut ctx,
            &[
                (r#"{"a":1}"#, false),
                (r#"{"a":1}"#, true), // 完全重复帧
            ],
        );
        assert_eq!(joined, r#"{"a":1}"#, "重复帧不应二次转发");
    }

    #[test]
    fn test_tool_input_pure_deltas() {
        // 上游发纯增量:每帧是不同片段。转发原样,拼接后仍为完整 JSON。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let joined = collect_tool_partial_json(
            &mut ctx,
            &[(r#"{"a""#, false), (r#":1,"#, false), (r#""b":2}"#, true)],
        );
        assert_eq!(
            joined, r#"{"a":1,"b":2}"#,
            "增量模式:原样转发,拼接后为完整 JSON"
        );
    }

    #[test]
    fn test_tool_input_single_full_snapshot() {
        // 单帧完整 JSON（最常见）:原样一次发出。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let joined = collect_tool_partial_json(&mut ctx, &[(r#"{"k":"v"}"#, true)]);
        assert_eq!(joined, r#"{"k":"v"}"#);
    }

    #[test]
    fn test_tool_input_single_delta_invariant() {
        // 根治不变式：无论上游发几帧，最终只在 stop 发**一个** input_json_delta（缓冲到 stop 再发）。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let (joined, n) = run_tool_frames(
            &mut ctx,
            &[
                (r#"{"a""#, false),
                (r#"{"a":1"#, false),
                (r#"{"a":1,"b":2}"#, true),
            ],
        );
        assert_eq!(joined, r#"{"a":1,"b":2}"#);
        assert_eq!(n, 1, "应只发一个 delta（缓冲到 stop 一次性发）");
    }

    #[test]
    fn test_tool_input_non_prefix_trap() {
        // 旧逐片启发式的致命陷阱:上游第二帧不以第一帧为前缀(非单调重写)。
        // 根治后（merge_tool_input 第 6 步）：两帧各自都是完整合法 JSON → 视为"重写",
        // 只保留最新完整对象,消灭 `}{` 粘连非法串(Invalid tool parameters 类型 C 根因)。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let (joined, n) = run_tool_frames(
            &mut ctx,
            &[(r#"{"path":"/a"}"#, false), (r#"{"path":"/b"}"#, true)],
        );
        // 只发一个 delta,且结果是合法 JSON(第二帧),不再是 `}{` 粘连串。
        assert_eq!(n, 1, "非前缀帧也只发一个 delta");
        assert_eq!(
            joined, r#"{"path":"/b"}"#,
            "非前缀双完整对象只留最新完整对象"
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&joined).is_ok(),
            "结果必须是合法 JSON"
        );
    }

    #[test]
    fn test_tool_input_illegal_json_at_stop_repaired_by_default() {
        // 修复层默认开：上游发本就非法的 JSON（\x 是 JSON 不支持的转义）→ 修复层介入修成合法后发出，
        // 客户端能正常 parse，不再报 Invalid tool parameters。`\xd7` 的 `\x` 非法 → 降级字面 `\\xd7`。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let (joined, n) = run_tool_frames(&mut ctx, &[(r#"{"a":"\xd7"}"#, true)]);
        assert_eq!(n, 1, "非法 JSON 也要发出(不静默空参)");
        assert!(
            serde_json::from_str::<serde_json::Value>(&joined).is_ok(),
            "修复层默认开：发给客户端的必须是合法 JSON，实际={}",
            joined
        );
        // 值语义：`\x` 非法转义降级为字面反斜杠，值为字面 `\xd7`。
        let v: serde_json::Value = serde_json::from_str(&joined).unwrap();
        assert_eq!(
            v["a"].as_str().unwrap(),
            r"\xd7",
            "非法 \\x 转义降级为字面反斜杠"
        );
    }

    // 注：修复层"关闭时原样透传"的行为不单独用 static 开关测——进程级 static 在并行测试下会互相
    // 污染（一个测试 set(false) 期间别的 ON 前提测试恰好在跑就会假失败）。透传分支的正确性由
    // flush_tool_input 里 `if tool_repair_json_enabled()` 的显式门控保证（关则完全不调 repair），
    // 修复函数本身的正确性由上面的纯函数注入测试独立覆盖，两者组合已充分且无并发风险。

    #[test]
    fn test_tool_input_truncated_stream_flushes_on_final() {
        // 截断:tool_use 帧永不带 stop,流结束。generate_final_events 应把残留缓冲发出并关闭块,
        // 客户端不会卡在未闭合 tool_use 块上。
        use crate::kiro::model::events::ToolUseEvent;
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        // 两帧累积,均无 stop
        for input in [r#"{"a""#, r#"{"a":1}"#] {
            let ev = Event::ToolUse(ToolUseEvent {
                name: "t".to_string(),
                tool_use_id: "toolu_x".to_string(),
                input: input.to_string(),
                stop: false,
            });
            let evs = ctx.process_kiro_event(&ev);
            // stop 前不应发任何 input_json_delta
            assert!(!evs.iter().any(|e| e.event == "content_block_delta"
                && e.data["delta"]["type"] == "input_json_delta"));
        }
        let finals = ctx.generate_final_events();
        let delta = finals.iter().find(|e| {
            e.event == "content_block_delta" && e.data["delta"]["type"] == "input_json_delta"
        });
        assert!(delta.is_some(), "截断收尾应 flush 残留 tool input");
        assert_eq!(delta.unwrap().data["delta"]["partial_json"], r#"{"a":1}"#);
        // 块应被关闭
        assert!(
            finals.iter().any(|e| e.event == "content_block_stop"),
            "截断应关闭 tool 块"
        );
    }

    #[test]
    fn test_text_delta_after_tool_use_restarts_text_block() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());

        let initial_events = ctx.generate_initial_events();
        assert!(
            initial_events
                .iter()
                .any(|e| e.event == "content_block_start"
                    && e.data["content_block"]["type"] == "text")
        );

        let initial_text_index = ctx
            .text_block_index
            .expect("initial text block index should exist");

        // tool_use 开始会自动关闭现有 text block
        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "test_tool".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });
        assert!(
            tool_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(initial_text_index as i64)
            }),
            "tool_use should stop the previous text block"
        );

        // 之后再来文本增量，应自动创建新的 text block 而不是往已 stop 的块里写 delta
        let text_events = ctx.process_assistant_response("hello");
        let new_text_start_index = text_events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        assert!(
            new_text_start_index.is_some(),
            "should start a new text block"
        );
        assert_ne!(
            new_text_start_index.unwrap(),
            initial_text_index as i64,
            "new text block index should differ from the stopped one"
        );
        assert!(
            text_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "hello"
            }),
            "should emit text_delta after restarting text block"
        );
    }

    #[test]
    fn test_tool_use_flushes_pending_thinking_buffer_text_before_tool_block() {
        // thinking 模式下，**可能是半个 `<thinking>` 的尾巴**会被暂存在 thinking_buffer 等
        // 跨 chunk 匹配。当紧接着出现 tool_use 时，应先 flush 这段文本，再开始 tool_use block。
        //
        // ⚠️ 输入形态已随扣留判据更新：原用两段中文（"有修" + "改："）作为"被暂存的短文本"，
        // 那是**旧判据的副作用**而非不变量 —— 旧代码无条件扣末尾 10 字节，中文又因
        // `find_char_boundary` 向前退到字符边界，于是 12 字节的纯中文恰好一个字都发不出。
        // 扣留改为「按标签语法判定」后，不可能是标签的散文立即下发（这正是
        // `partial_thinking_tag_suffix_len` 的设计目的：首字节少等一个 chunk），
        // 纯中文不再滞留。本测试要钉的是「**有**待发内容时先 flush 再开 tool_use」，
        // 故改喂真的会被扣住的半标签；下方所有块顺序断言一字未改。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        // 两段拼起来是 `<think` —— `<thinking>` 的真前缀，必须被扣在缓冲里等下一个 chunk。
        let ev1 = ctx.process_assistant_response("<thi");
        assert!(
            ev1.iter().all(|e| e.event != "content_block_delta"),
            "half tag prefix should be buffered under thinking mode"
        );
        let ev2 = ctx.process_assistant_response("nk");
        assert!(
            ev2.iter().all(|e| e.event != "content_block_delta"),
            "half tag prefix should still be buffered under thinking mode"
        );
        assert_eq!(
            ctx.thinking_buffer, "<think",
            "半标签必须完整滞留（否则下面的 flush 场景根本没被触发，断言会变成恒真的纸面测试）"
        );

        let events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });

        let text_start_index = events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        let pos_text_delta = events.iter().position(|e| {
            e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta"
        });
        let pos_text_stop = text_start_index.and_then(|idx| {
            events.iter().position(|e| {
                e.event == "content_block_stop" && e.data["index"].as_i64() == Some(idx)
            })
        });
        let pos_tool_start = events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });

        assert!(
            text_start_index.is_some(),
            "should start a text block to flush buffered text"
        );
        assert!(
            pos_text_delta.is_some(),
            "should flush buffered text as text_delta"
        );
        assert!(
            pos_text_stop.is_some(),
            "should stop text block before tool_use block starts"
        );
        assert!(pos_tool_start.is_some(), "should start tool_use block");

        let pos_text_delta = pos_text_delta.unwrap();
        let pos_text_stop = pos_text_stop.unwrap();
        let pos_tool_start = pos_tool_start.unwrap();

        assert!(
            pos_text_delta < pos_text_stop && pos_text_stop < pos_tool_start,
            "ordering should be: text_delta -> text_stop -> tool_use_start"
        );

        assert!(
            events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "<think"
            }),
            "flushed text should equal the buffered prefix"
        );
    }

    #[test]
    fn test_estimate_tokens() {
        assert!(estimate_tokens("Hello") > 0);
        assert!(estimate_tokens("你好") > 0);
        assert!(estimate_tokens("Hello 你好") > 0);
    }

    #[test]
    fn test_find_real_thinking_start_tag_basic() {
        // 基本情况：正常的开始标签
        assert_eq!(
            tag_start(find_real_thinking_start_tag("<thinking>")),
            Some(0)
        );
        assert_eq!(
            tag_start(find_real_thinking_start_tag("prefix<thinking>")),
            Some(6)
        );
        // 名字后紧跟别的字母 ⇒ 是另一个标签名，不得误判
        assert_eq!(find_real_thinking_start_tag("<thinkingfoo>"), None);
    }

    #[test]
    fn test_find_real_thinking_start_tag_with_backticks() {
        // 被反引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("`<thinking>`"), None);
        assert_eq!(find_real_thinking_start_tag("use `<thinking>` tag"), None);

        // 先有被包裹的，后有真正的开始标签
        assert_eq!(
            tag_start(find_real_thinking_start_tag(
                "about `<thinking>` tag<thinking>content"
            )),
            Some(22)
        );
    }

    #[test]
    fn test_find_real_thinking_start_tag_with_quotes() {
        // 被双引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("\"<thinking>\""), None);
        assert_eq!(find_real_thinking_start_tag("the \"<thinking>\" tag"), None);

        // 被单引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("'<thinking>'"), None);

        // 混合情况
        assert_eq!(
            tag_start(find_real_thinking_start_tag(
                "about \"<thinking>\" and '<thinking>' then<thinking>"
            )),
            Some(40)
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_basic() {
        // 基本情况：正常的结束标签后面有双换行符
        assert_eq!(
            tag_start(find_real_thinking_end_tag("</thinking>\n\n")),
            Some(0)
        );
        assert_eq!(
            tag_start(find_real_thinking_end_tag("content</thinking>\n\n")),
            Some(7)
        );
        assert_eq!(
            tag_start(find_real_thinking_end_tag(
                "some text</thinking>\n\nmore text"
            )),
            Some(9)
        );

        // 没有双换行符的情况
        assert_eq!(find_real_thinking_end_tag("</thinking>"), None);
        assert_eq!(find_real_thinking_end_tag("</thinking>\n"), None);
        assert_eq!(find_real_thinking_end_tag("</thinking> more"), None);
    }

    /// ⭐ 回归：思考正文最后一个字符是 **ASCII 标点** 时，结束标签必须仍被识别。
    ///
    /// 旧 `QUOTE_CHARS` 有 30 个字符（含 `.` `,` `:` `;` `!` `?` `)` `]` `}` `%` …），
    /// 而文档注释只承诺 3 个引号。于是「思考以句号结尾」这种**散文常态**被判成
    /// 「标签被引用包裹」→ 跳过 → 找不到结束标签。
    ///
    /// 后果不是标签泄漏，而是**答案整段消失**：thinking 开启时正文被当思考内容发进
    /// thinking 面板（`has_non_thinking_blocks()=false` 还会把 stop_reason 伪造成
    /// `max_tokens`、只吐一个空格 text 块）；thinking 关闭时整段被丢弃，用户看到空回答。
    ///
    /// 把 `QUOTE_CHARS` 改回 30 个字符 → 本测试必 FAILED。
    #[test]
    fn end_tag_recognized_when_thinking_ends_with_ascii_punctuation() {
        for c in [
            '.', ',', ':', ';', '!', '?', ')', ']', '}', '%', '=', '-', '*', '&', '#',
        ] {
            let body = format!("reason{c}</thinking>\n\nANSWER");
            assert_eq!(
                tag_start(find_real_thinking_end_tag(&body)),
                Some("reason".len() + c.len_utf8()),
                "思考正文以 {c:?} 结尾时结束标签必须仍被识别（旧 QUOTE_CHARS 会漏判 → 答案消失）"
            );
        }
        // 对照：成对引号包裹的**引用**仍必须被跳过（收窄不等于放弃引用判定）
        assert_eq!(find_real_thinking_end_tag("`</thinking>`\n\n"), None);
        assert_eq!(find_real_thinking_end_tag("\"</thinking>\"\n\n"), None);
        assert_eq!(find_real_thinking_end_tag("'</thinking>'\n\n"), None);
    }

    /// ⭐ 回归：结束标签后**不是恰好 `\n\n`** 的形态也必须被识别。
    ///
    /// 旧判据只接受 `after_content.starts_with("\n\n")`，于是单换行 / 紧跟 `<` 全部漏判。
    /// 其中 `</thinking><invoke ...>` 是 Claude Code 的真实生产形态 ——
    /// 漏判会让整个工具调用连同答案一起被吞（见
    /// [`thinking_end_adjacent_invoke_must_still_reclaim_tool`]）。
    ///
    /// 把判据改回 `starts_with("\n\n")` → 本测试必 FAILED。
    #[test]
    fn end_tag_recognized_with_non_double_newline_suffix() {
        // 单换行 + 正文
        assert_eq!(
            tag_start(find_real_thinking_end_tag("reason\n</thinking>\nANSWER")),
            Some(7)
        );
        // 紧跟下一个标签（零空白）
        assert_eq!(
            tag_start(find_real_thinking_end_tag(
                "reason\n</thinking><invoke name=\"R\">"
            )),
            Some(7)
        );
        // 行内空白后接换行
        assert_eq!(
            tag_start(find_real_thinking_end_tag("reason\n</thinking> \n\nANSWER")),
            Some(7)
        );
        // 对照：纯行内空白（无换行）仍不认 —— 更像正文顺口提到标签
        assert_eq!(find_real_thinking_end_tag("</thinking> more"), None);
        // 对照：空白顶到缓冲末尾且未攒够 `\n\n` → 等下一个 chunk（防吃掉段落分隔的后半个）
        assert_eq!(find_real_thinking_end_tag("</thinking>"), None);
        assert_eq!(find_real_thinking_end_tag("</thinking>\n"), None);
    }

    /// ⭐ 回归：结束标签消耗长度必须按**实际后缀形态**算，不能写死 13 字节。
    ///
    /// 判据放宽后只有 `\n\n` 才恰好 13。写死会在其余形态下多切 2 字节：
    /// `</thinking>\nAnswer` 切掉 `\nA` → 客户端看到 `nswer`；
    /// `</thinking><invoke` 切掉 `<i` → 文本化工具调用永远无法重组。
    ///
    /// 把三处 `thinking_end_tag_consumed_len` 换回 `"</thinking>\n\n".len()` → 本测试必 FAILED。
    ///
    /// 同时钉住**标签本身也不定长**：长度取自匹配结果而非写死 11。若把
    /// `thinking_end_tag_consumed_len` 改回按 `"</thinking>".len()` 算，
    /// 下面 `</THINKING >` 那条（12 字节）会少切 1 字节 → 残留 `>` 泄漏 → FAILED。
    #[test]
    fn end_tag_consumed_len_matches_actual_suffix() {
        // 用**匹配结果**驱动，覆盖「匹配 → 算长度」全链，而非只喂写死的 0 位置。
        let consumed = |buf: &str| {
            let m = scan_thinking_tag(buf, 0, true).expect("应能匹配到闭标签");
            (thinking_end_tag_consumed_len(buf, &m), m.len)
        };
        const TAG: usize = "</thinking>".len();
        assert_eq!(consumed("</thinking>\n\nA"), (TAG + 2, TAG));
        assert_eq!(consumed("</thinking>\nA"), (TAG + 1, TAG));
        assert_eq!(consumed("</thinking><invoke"), (TAG, TAG));
        // 三个以上换行只吃掉段落分隔的两个，其余留给正文（保持既有行为）
        assert_eq!(consumed("</thinking>\n\n\nA"), (TAG + 2, TAG));
        // 大写：长度恰好相同（侥幸），但必须真的匹配上
        assert_eq!(consumed("</THINKING>\n\nA"), (TAG + 2, TAG));
        // 带空白的闭标签：**12 字节**，写死 11 会残留一个 `>`
        assert_eq!(consumed("</thinking >\n\nA"), (TAG + 1 + 2, TAG + 1));
    }

    /// ⭐ 回归（**走真实流路径**，不是只测纯函数）：单换行后缀时正文首字符不得被吃掉。
    ///
    /// 上面那条 `end_tag_consumed_len_matches_actual_suffix` 只断言辅助函数自身，
    /// 把三处**调用点**换回写死 13 它依然全绿 —— 那是纸面测试，挡不住回退。
    /// 本条从 `process_assistant_response` 入口喂真实文本、断言客户端看到的可见正文，
    /// 任一调用点写死 13 → `\n` + 正文首字节被一起切掉 → `ANSWER` 变 `NSWER` → FAILED。
    #[test]
    fn single_newline_suffix_must_not_eat_first_answer_char() {
        for thinking_enabled in [true, false] {
            let mut ctx = StreamContext::new_with_thinking(
                "claude-sonnet-5",
                1,
                thinking_enabled,
                HashMap::new(),
            );
            let mut ev = ctx.generate_initial_events();
            ev.extend(ctx.process_assistant_response("<thinking>\nreason\n</thinking>\nANSWER"));
            ev.extend(ctx.generate_final_events());
            let text: String = ev
                .iter()
                .filter(|e| e.event == "content_block_delta")
                .filter_map(|e| e.data["delta"]["text"].as_str())
                .collect();
            assert_eq!(
                text.trim(),
                "ANSWER",
                "thinking={thinking_enabled}：单换行后缀时正文必须完整（写死 13 会吃掉首字符）"
            );
        }
    }

    /// ⭐ 回归（最严重的复合缺陷）：`</thinking>` 紧跟文本化 `<invoke>` 时
    /// **工具必须仍被重组执行**。
    ///
    /// 旧代码下 4 种真实形态里有 3 种让工具**静默不执行**：结束标签漏判 →
    /// 整段（含 invoke 块）滞留 thinking_buffer → 被当思考内容发走，
    /// invoke 从未进入 sniff 缓冲。thinking 关闭时更是连文本都没有，
    /// 工具调用**毫无痕迹地消失** —— 面板上看不出任何异常。
    #[test]
    fn thinking_end_adjacent_invoke_must_still_reclaim_tool() {
        let shapes = [
            (
                "紧跟无换行",
                "<thinking>\nread it\n</thinking><invoke name=\"Read\"><parameter name=\"path\">/a</parameter></invoke>",
            ),
            (
                "单换行",
                "<thinking>\nread it\n</thinking>\n<invoke name=\"Read\"><parameter name=\"path\">/a</parameter></invoke>",
            ),
            (
                "双换行",
                "<thinking>\nread it\n</thinking>\n\n<invoke name=\"Read\"><parameter name=\"path\">/a</parameter></invoke>",
            ),
            (
                "句号紧贴标签",
                "<thinking>\nread it.</thinking>\n\n<invoke name=\"Read\"><parameter name=\"path\">/a</parameter></invoke>",
            ),
        ];
        for (name, full) in shapes {
            for thinking_enabled in [true, false] {
                let known: std::collections::HashSet<String> =
                    ["Read".to_string()].into_iter().collect();
                let mut ctx = StreamContext::new_full(
                    "claude-sonnet-5",
                    1,
                    thinking_enabled,
                    HashMap::new(),
                    known,
                );
                ctx.reclaim_enabled = true;
                let mut ev = ctx.generate_initial_events();
                ev.extend(ctx.process_assistant_response(full));
                ev.extend(ctx.generate_final_events());

                let tools: Vec<&str> = ev
                    .iter()
                    .filter(|e| e.event == "content_block_start")
                    .filter_map(|e| e.data["content_block"]["name"].as_str())
                    .collect();
                assert_eq!(
                    tools,
                    ["Read"],
                    "形态「{name}」thinking={thinking_enabled} 时工具必须被重组执行，实际 {tools:?}"
                );
                assert_eq!(
                    ctx.reclaimed_invoke_count, 1,
                    "形态「{name}」应恰好重组 1 次"
                );
            }
        }
    }

    #[test]
    fn test_find_real_thinking_end_tag_with_backticks() {
        // 被反引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("`</thinking>`\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("mention `</thinking>` in code\n\n"),
            None
        );

        // 只有前面有反引号
        assert_eq!(find_real_thinking_end_tag("`</thinking>\n\n"), None);

        // 只有后面有反引号
        assert_eq!(find_real_thinking_end_tag("</thinking>`\n\n"), None);
    }

    #[test]
    fn test_find_real_thinking_end_tag_with_quotes() {
        // 被双引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("\"</thinking>\"\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("the string \"</thinking>\" is a tag\n\n"),
            None
        );

        // 被单引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("'</thinking>'\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("use '</thinking>' as marker\n\n"),
            None
        );

        // 混合情况：双引号包裹后有真正的标签
        assert_eq!(
            tag_start(find_real_thinking_end_tag(
                "about \"</thinking>\" tag</thinking>\n\n"
            )),
            Some(23)
        );

        // 混合情况：单引号包裹后有真正的标签
        assert_eq!(
            tag_start(find_real_thinking_end_tag(
                "about '</thinking>' tag</thinking>\n\n"
            )),
            Some(23)
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_mixed() {
        // 先有被包裹的，后有真正的结束标签
        assert_eq!(
            tag_start(find_real_thinking_end_tag(
                "discussing `</thinking>` tag</thinking>\n\n"
            )),
            Some(28)
        );

        // 多个被包裹的，最后一个是真正的
        assert_eq!(
            tag_start(find_real_thinking_end_tag(
                "`</thinking>` and `</thinking>` done</thinking>\n\n"
            )),
            Some(36)
        );

        // 多种引用字符混合
        assert_eq!(
            tag_start(find_real_thinking_end_tag(
                "`</thinking>` and \"</thinking>\" and '</thinking>' done</thinking>\n\n"
            )),
            Some(54)
        );
    }

    #[test]
    fn test_tool_use_immediately_after_thinking_filters_end_tag_and_closes_thinking_block() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();

        // thinking 内容以 `</thinking>` 结尾，但后面没有 `\n\n`（模拟紧跟 tool_use 的场景）
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));

        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });
        all_events.extend(tool_events);

        all_events.extend(ctx.generate_final_events());

        // 不应把 `</thinking>` 当作 thinking 内容输出
        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered from output"
        );

        // thinking block 必须在 tool_use block 之前关闭
        let thinking_index = ctx
            .thinking_block_index
            .expect("thinking block index should exist");
        let pos_thinking_stop = all_events.iter().position(|e| {
            e.event == "content_block_stop"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });
        let pos_tool_start = all_events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });
        assert!(
            pos_thinking_stop.is_some(),
            "thinking block should be stopped"
        );
        assert!(pos_tool_start.is_some(), "tool_use block should be started");
        assert!(
            pos_thinking_stop.unwrap() < pos_tool_start.unwrap(),
            "thinking block should stop before tool_use block starts"
        );
    }

    #[test]
    fn test_final_flush_filters_standalone_thinking_end_tag() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered during final flush"
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_same_chunk() {
        // <thinking>\n 在同一个 chunk 中，\n 应被剥离
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>\nHello world");

        // 找到所有 thinking_delta 事件
        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        // 拼接所有 thinking 内容
        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_cross_chunk() {
        // <thinking> 在第一个 chunk 末尾，\n 在第二个 chunk 开头
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events1 = ctx.process_assistant_response("<thinking>");
        let events2 = ctx.process_assistant_response("\nHello world");

        let mut all_events = Vec::new();
        all_events.extend(events1);
        all_events.extend(events2);

        let thinking_deltas: Vec<_> = all_events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n across chunks, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_no_strip_when_no_leading_newline() {
        // <thinking> 后直接跟内容（无 \n），内容应完整保留
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>abc</thinking>\n\ntext");

        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .filter(|e| {
                !e.data["delta"]["thinking"]
                    .as_str()
                    .unwrap_or("")
                    .is_empty()
            })
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert_eq!(full_thinking, "abc", "thinking content should be 'abc'");
    }

    #[test]
    fn test_text_after_thinking_strips_leading_newlines() {
        // `</thinking>\n\n` 后的文本不应以 \n\n 开头
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>\nabc</thinking>\n\n你好");

        let text_deltas: Vec<_> = events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .collect();

        let full_text: String = text_deltas
            .iter()
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_text.starts_with('\n'),
            "text after thinking should not start with \\n, got: {:?}",
            full_text
        );
        assert_eq!(full_text, "你好");
    }

    /// 辅助函数：从事件列表中提取所有 thinking_delta 的拼接内容
    fn collect_thinking_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// 辅助函数：从事件列表中提取所有 text_delta 的拼接内容
    fn collect_text_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect()
    }

    #[test]
    fn test_end_tag_newlines_split_across_events() {
        // `</thinking>\n` 在 chunk 1，`\n` 在 chunk 2，`text` 在 chunk 3
        // 确保 `</thinking>` 不会被部分当作 thinking 内容发出
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_end_tag_alone_in_chunk_then_newlines_in_next() {
        // `</thinking>` 单独在一个 chunk，`\n\ntext` 在下一个 chunk
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all.extend(ctx.process_assistant_response("\n\n你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_start_tag_newline_split_across_events() {
        // `\n\n` 在 chunk 1，`<thinking>` 在 chunk 2，`\n` 在 chunk 3
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("\n\n"));
        all.extend(ctx.process_assistant_response("<thinking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("abc</thinking>\n\ntext"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "text", "text should be 'text', got: {:?}", text);
    }

    #[test]
    fn test_full_flow_maximally_split() {
        // 极端拆分：每个关键边界都在不同 chunk
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        // \n\n<thinking>\n 拆成多段
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("<thin"));
        all.extend(ctx.process_assistant_response("king>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("hello"));
        // </thinking>\n\n 拆成多段
        all.extend(ctx.process_assistant_response("</thi"));
        all.extend(ctx.process_assistant_response("nking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("world"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "hello",
            "thinking should be 'hello', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "world", "text should be 'world', got: {:?}", text);
    }

    #[test]
    fn test_thinking_only_sets_max_tokens_stop_reason() {
        // 整个流只有 thinking 块，没有 text 也没有 tool_use，stop_reason 应为 max_tokens
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "max_tokens",
            "stop_reason should be max_tokens when only thinking is produced"
        );

        // 应补发一套完整的 text 事件（content_block_start + delta 空格 + content_block_stop）
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "text"
            }),
            "should emit text content_block_start"
        );
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == " "
            }),
            "should emit text_delta with a single space"
        );
        // text block 应被 generate_final_events 自动关闭
        let text_block_index = all_events
            .iter()
            .find_map(|e| {
                if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                    e.data["index"].as_i64()
                } else {
                    None
                }
            })
            .expect("text block should exist");
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(text_block_index)
            }),
            "text block should be stopped"
        );
    }

    #[test]
    fn test_thinking_with_text_keeps_end_turn_stop_reason() {
        // thinking + text 的情况，stop_reason 应为 end_turn
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n\nHello"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "end_turn",
            "stop_reason should be end_turn when text is also produced"
        );
    }

    #[test]
    fn test_thinking_with_tool_use_keeps_tool_use_stop_reason() {
        // thinking + tool_use 的情况，stop_reason 应为 tool_use
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(
            ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
                name: "test_tool".to_string(),
                tool_use_id: "tool_1".to_string(),
                input: "{}".to_string(),
                stop: true,
            }),
        );
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "tool_use",
            "stop_reason should be tool_use when tool_use is present"
        );
    }

    /// B3 回归：流式 thinking 块结束前必须发一个非空 signature_delta，且排在
    /// content_block_stop 之前。否则客户端下一轮回传时本地校验失败报错
    /// "The content[].thinking in the thinking mode must be passed back"。
    #[test]
    fn test_thinking_block_emits_signature_delta_before_stop() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n\nHello"));
        all_events.extend(ctx.generate_final_events());

        let thinking_index = ctx
            .thinking_block_index
            .expect("thinking block index should exist");

        // 存在一个非空 signature_delta，index 指向 thinking 块
        let pos_sig = all_events.iter().position(|e| {
            e.event == "content_block_delta"
                && e.data["delta"]["type"] == "signature_delta"
                && e.data["delta"]["signature"]
                    .as_str()
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });
        assert!(pos_sig.is_some(), "应发出非空 signature_delta");

        // signature_delta 必须排在 thinking 块的 content_block_stop 之前
        let pos_stop = all_events.iter().position(|e| {
            e.event == "content_block_stop"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });
        assert!(pos_stop.is_some(), "thinking 块应被关闭");
        assert!(
            pos_sig.unwrap() < pos_stop.unwrap(),
            "signature_delta 必须排在 content_block_stop 之前"
        );
    }

    /// ⭐ 回归（P3-1）：`reasoningContentEvent` 带真签名 → `signature_delta` 必须用**上游真签名**，
    /// 而不是占位符。Foxfishc 实测「伪造签名不被上游识别，cache_read 仍 0」——真签名是
    /// 多轮 cache 命中的关键。把 `create_signature_delta_event` 里的
    /// `pending_reasoning_signature.take()` 改回恒占位符 → 本测试 FAILED。
    #[test]
    fn reasoning_signature_is_forwarded_to_client_instead_of_placeholder() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let ev = Event::ReasoningContent(crate::kiro::model::events::ReasoningContentEvent {
            text: "思考过程".to_string(),
            signature: Some("upstream-real-signature-abc123".to_string()),
            ..Default::default()
        });
        ctx.process_kiro_event(&ev);
        let all_events = ctx.generate_final_events();

        let sig = all_events
            .iter()
            .find(|e| e.data["delta"]["type"] == "signature_delta")
            .expect("应发出 signature_delta")
            .data["delta"]["signature"]
            .as_str()
            .expect("signature 应为字符串");
        assert_eq!(
            sig, "upstream-real-signature-abc123",
            "必须回传上游真签名（否则多轮 cache 永不命中）"
        );
    }

    /// 对照：上游 reasoningContentEvent 不带签名 → 回退占位符，行为与改动前逐字节一致。
    #[test]
    fn reasoning_without_signature_falls_back_to_placeholder() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let ev = Event::ReasoningContent(crate::kiro::model::events::ReasoningContentEvent {
            text: "思考".to_string(),
            ..Default::default()
        });
        ctx.process_kiro_event(&ev);
        let all_events = ctx.generate_final_events();

        let sig = all_events
            .iter()
            .find(|e| e.data["delta"]["type"] == "signature_delta")
            .expect("应发出 signature_delta")
            .data["delta"]["signature"]
            .as_str()
            .expect("signature 应为字符串");
        assert_eq!(
            sig, THINKING_SIGNATURE_PLACEHOLDER,
            "无上游签名应回退占位符"
        );
    }

    /// ⭐ 回归（P3-2）：thinking 开启 + 结构化 reasoning 开块 + 直接 tool_use（无正文）——
    /// thinking 块必须**先 stop 再 start tool_use 块**（Anthropic SSE 块顺序契约）。
    ///
    /// 旧代码缺这条：`process_tool_use` 只按 sniff 路径（`thinking_buffer` 里有
    /// `</thinking>`）关块，而 reasoning 开的块内容直接以 thinking_delta 下发、buffer 恒空，
    /// 于是 tool_use 块 start 时 thinking 块仍未 stop → SSE 出现「新块 start → 旧块 stop」
    /// 交错（工具块 index 1 先于思考块 index 0 收尾），CC 解析报错。把
    /// `close_reasoning_thinking_block` 接进 `process_tool_use` 的 reasoning 分支 → 本测试 FAILED。
    #[test]
    fn reasoning_opened_thinking_block_closed_before_tool_use() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.5", 10, true, HashMap::new());
        let mut all = ctx.generate_initial_events();
        all.extend(ctx.process_kiro_event(&reasoning_ev("思考如何调用工具")));
        all.extend(ctx.process_kiro_event(&Event::ToolUse(
            crate::kiro::model::events::ToolUseEvent {
                name: "Bash".into(),
                tool_use_id: "toolu_1".into(),
                input: "{\"command\":\"ls\"}".into(),
                stop: true,
            },
        )));
        all.extend(ctx.generate_final_events());

        // 思考块 stop（index 0）必须出现在工具块 start（index 1）之前。
        let thinking_stop_pos = all
            .iter()
            .position(|e| e.event == "content_block_stop" && e.data["index"].as_i64() == Some(0))
            .expect("thinking 块必须收尾");
        let tool_start_pos = all
            .iter()
            .position(|e| e.event == "content_block_start" && e.data["index"].as_i64() == Some(1))
            .expect("tool_use 块必须存在");
        assert!(
            thinking_stop_pos < tool_start_pos,
            "thinking 块必须在 tool_use 块 start 之前 stop（SSE 块顺序契约）"
        );
    }

    // ============ 「截断即成功」修复：CompletionStatus 回归 ============

    #[test]
    fn test_completion_status_outcome_and_http_mapping() {
        // 上游限流类错误 → RateLimited / overloaded_error / 429
        let rl = CompletionStatus::UpstreamError {
            code: "ThrottlingException".to_string(),
            message: "rate exceeded".to_string(),
        };
        assert_eq!(rl.outcome(), RequestOutcome::RateLimited);
        assert_eq!(rl.sse_error_type(), "overloaded_error");
        assert_eq!(rl.http_status_u16(), 429);

        // 普通上游错误 → ServerError / api_error / 502
        let se = CompletionStatus::UpstreamError {
            code: "InternalServerException".to_string(),
            message: "boom".to_string(),
        };
        assert_eq!(se.outcome(), RequestOutcome::ServerError);
        assert_eq!(se.sse_error_type(), "api_error");
        assert_eq!(se.http_status_u16(), 502);

        // 传输中断 → NetworkError / 502
        let te = CompletionStatus::TransportError {
            message: "connection reset".to_string(),
        };
        assert_eq!(te.outcome(), RequestOutcome::NetworkError);
        assert_eq!(te.http_status_u16(), 502);

        // 解码器停止 → ServerError / 502
        let ds = CompletionStatus::DecoderStopped {
            message: "too many errors".to_string(),
        };
        assert_eq!(ds.outcome(), RequestOutcome::ServerError);
        assert_eq!(ds.http_status_u16(), 502);

        // Ok → Success
        assert_eq!(CompletionStatus::Ok.outcome(), RequestOutcome::Success);
        assert!(CompletionStatus::Ok.is_ok());
    }

    #[test]
    fn test_inband_error_event_sets_failure_and_emits_error_event() {
        // 回归 BUG①/③：in-band Event::Error 应内联发 SSE error 事件，并把 completion 置失败态，
        // 使收尾 outcome 不再是硬编码的 Success。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();

        let events = ctx.process_kiro_event(&Event::Error {
            error_code: "InternalServerException".to_string(),
            error_message: "upstream boom".to_string(),
        });

        // 内联发出了 error 事件
        assert!(
            events
                .iter()
                .any(|e| e.event == "error" && e.data["error"]["type"] == "api_error"),
            "in-band 错误应内联发出 SSE error 事件"
        );
        assert!(ctx.error_event_emitted(), "应标记已发 error 事件");
        // completion 置为失败态，outcome 不再是 Success
        assert!(!ctx.completion().is_ok());
        assert_eq!(ctx.completion_outcome(), RequestOutcome::ServerError);
    }

    #[test]
    fn test_content_length_exceeded_is_not_failure() {
        // 铁律：ContentLengthExceededException = max_tokens 干净收尾，绝不算失败。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();

        let events = ctx.process_kiro_event(&Event::Exception {
            exception_type: "ContentLengthExceededException".to_string(),
            message: "max tokens".to_string(),
        });

        assert!(events.is_empty(), "CL 异常不应发 error 事件");
        assert!(ctx.completion().is_ok(), "CL 异常不应置失败态");
        assert_eq!(ctx.completion_outcome(), RequestOutcome::Success);
        assert!(!ctx.error_event_emitted());
    }

    #[test]
    fn test_non_cl_exception_marks_failure_and_emits_error() {
        // 非 CL 异常是上游真实失败：置失败态 + 内联发 error 事件。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();

        let events = ctx.process_kiro_event(&Event::Exception {
            exception_type: "ThrottlingException".to_string(),
            message: "slow down".to_string(),
        });

        assert!(
            events
                .iter()
                .any(|e| e.event == "error" && e.data["error"]["type"] == "overloaded_error"),
            "限流类异常应发 overloaded_error"
        );
        assert_eq!(ctx.completion_outcome(), RequestOutcome::RateLimited);
    }

    #[test]
    fn test_mark_transport_and_decoder_stopped_are_idempotent() {
        // 传输中断 / 解码器停止的 setter 应置失败态，且幂等保留首因。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();

        ctx.mark_transport_error("reset");
        assert_eq!(ctx.completion_outcome(), RequestOutcome::NetworkError);
        // 首因已定，后续 mark 不覆盖
        ctx.mark_decoder_stopped("later error");
        assert_eq!(
            ctx.completion_outcome(),
            RequestOutcome::NetworkError,
            "幂等：应保留首个失败原因"
        );
    }

    #[test]
    fn test_buffered_context_delegates_completion() {
        // BufferedStreamContext 应把完成状态透传给内部 StreamContext。
        let mut ctx = BufferedStreamContext::new(
            "test-model",
            1,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        ctx.process_and_buffer(&Event::Error {
            error_code: "InternalServerException".to_string(),
            error_message: "boom".to_string(),
        });
        assert!(!ctx.completion().is_ok());
        assert_eq!(ctx.completion_outcome(), RequestOutcome::ServerError);
        assert!(ctx.error_event_emitted());
    }

    #[test]
    fn test_buffered_context_bounds_memory_on_flood() {
        // C4 回归:上游持续推送超长文本时,缓冲事件累计字节超上限应触发截断守卫——
        // 停止继续缓冲(event_buffer 不再无界增长)并置失败态。旧实现无上限会一直吃内存 OOM。
        use crate::kiro::model::events::AssistantResponseEvent;
        let mut ctx = BufferedStreamContext::new(
            "test-model",
            1,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );

        // 每帧约 1MiB 文本;喂 200 帧(约 200MiB) 远超 64MiB 上限。
        // 用变化的自然文本(非单字符/单词连写),避免触发 stray 复读熔断(那会吞掉内容)。
        let sentence = "The quick brown fox jumps over the lazy dog while parsing tokens. ";
        let chunk = sentence.repeat((1024 * 1024) / sentence.len() + 1);
        for _ in 0..200 {
            ctx.process_and_buffer(&Event::AssistantResponse(AssistantResponseEvent {
                content: chunk.clone(),
            }));
            if ctx.buffer_overflowed() {
                break;
            }
        }

        assert!(ctx.buffer_overflowed(), "超长响应应触发缓冲上限守卫");
        // 缓冲不应无界增长:累计字节应止于略超上限(不是 200MiB 全吃进来)。
        assert!(
            ctx.buffered_bytes <= MAX_BUFFERED_EVENT_BYTES + 4 * 1024 * 1024,
            "超限后应停止缓冲,不再继续增长(实际 {} 字节)",
            ctx.buffered_bytes
        );
        // 截断按失败态处置(收尾会发 SSE error,不把半截当成功)。
        assert!(!ctx.completion().is_ok(), "缓冲溢出应置失败态");

        // 溢出后继续喂事件应被丢弃(early-return),字节数不再变。
        let before = ctx.buffered_bytes;
        ctx.process_and_buffer(&Event::AssistantResponse(AssistantResponseEvent {
            content: chunk.clone(),
        }));
        assert_eq!(ctx.buffered_bytes, before, "溢出后应丢弃后续事件,不再缓冲");
    }

    // ==================== merge_tool_input 决策表回归（Invalid tool parameters 类型 C 根治） ====================

    /// 累积快照三帧：每帧是"到目前为止的完整 JSON" → 最终取最后最全的一帧。
    #[test]
    fn test_merge_cumulative_snapshots() {
        let mut buf = String::new();
        buf = merge_tool_input(&buf, r#"{"path""#);
        buf = merge_tool_input(&buf, r#"{"path":"a.txt""#);
        buf = merge_tool_input(&buf, r#"{"path":"a.txt","content":"hi"}"#);
        assert_eq!(buf, r#"{"path":"a.txt","content":"hi"}"#);
        assert!(serde_json::from_str::<serde_json::Value>(&buf).is_ok());
    }

    /// 纯增量碎片三帧：每帧只带新片段 → 追加拼成完整 JSON。
    #[test]
    fn test_merge_pure_increments() {
        let mut buf = String::new();
        buf = merge_tool_input(&buf, r#"{"path":"#);
        buf = merge_tool_input(&buf, r#""a.txt","content""#);
        buf = merge_tool_input(&buf, r#":"hi"}"#);
        assert_eq!(buf, r#"{"path":"a.txt","content":"hi"}"#);
        assert!(serde_json::from_str::<serde_json::Value>(&buf).is_ok());
    }

    /// 重复终帧：同一完整快照来两次 → 不翻倍。
    #[test]
    fn test_merge_duplicate_final_frame() {
        let full = r#"{"a":1,"b":2}"#;
        let mut buf = String::new();
        buf = merge_tool_input(&buf, full);
        buf = merge_tool_input(&buf, full);
        assert_eq!(buf, full, "重复终帧不应翻倍");
    }

    /// 核心：两个各自完整、彼此非前缀的对象 → 结果是第二帧，而不是 `{"a":1}{"a":2}` 粘连串。
    #[test]
    fn test_merge_nonprefix_double_object_keeps_latest() {
        let buf = merge_tool_input(r#"{"a":1}"#, r#"{"a":2}"#);
        assert_eq!(
            buf, r#"{"a":2}"#,
            "非前缀双完整对象应只留最新，消灭 object 粘连"
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&buf).is_ok(),
            "结果必须是合法 JSON"
        );
    }

    /// 完整对象之后来一个更短的旧前缀快照 → 保持完整，不被旧短帧覆盖。
    #[test]
    fn test_merge_full_then_shorter_prefix_kept() {
        let full = r#"{"path":"a.txt","content":"hi"}"#;
        let buf = merge_tool_input(full, r#"{"path""#);
        assert_eq!(buf, full, "迟到的旧短前缀快照应被丢弃，保留更全缓冲");
    }

    /// 空帧不改变缓冲；空缓冲取本帧。
    #[test]
    fn test_merge_empty_edges() {
        assert_eq!(merge_tool_input("abc", ""), "abc", "空帧 → 缓冲不变");
        assert_eq!(merge_tool_input("", "abc"), "abc", "空缓冲 → 取本帧");
        assert_eq!(merge_tool_input("", ""), "", "双空 → 空");
    }

    /// 真增量碎片（各帧本身非法）→ append 后拼成合法整体，不被第 6 步误判。
    #[test]
    fn test_merge_illegal_fragments_append() {
        // 第一帧是未闭合的合法前缀，第二帧续上闭合：两者都不是完整 JSON → 走追加。
        let buf = merge_tool_input(r#"{"x":[1,2"#, r#",3]}"#);
        assert_eq!(buf, r#"{"x":[1,2,3]}"#);
        assert!(serde_json::from_str::<serde_json::Value>(&buf).is_ok());
    }

    /// 回归（🔴 第 5 步裸判据吞掉单独成帧的 `{`）：buf 尚不完整（`{"nested":`）时，
    /// 上游把嵌套对象的开括号单独发成一帧 → 旧代码用「buf.starts_with(frame)」纯形状判据，
    /// 而几乎所有 JSON 对象前缀都以 `{` 开头，于是把这个合法续帧误判成「迟到的旧短快照」而丢弃，
    /// 拼出的串永久缺这个 `{`。修复后必须追加，最终三帧拼出完整合法 JSON。
    #[test]
    fn test_merge_standalone_open_brace_not_dropped_when_buf_incomplete() {
        let mut buf = String::new();
        buf = merge_tool_input(&buf, r#"{"nested":"#);
        buf = merge_tool_input(&buf, "{");
        buf = merge_tool_input(&buf, r#""a":1}}"#);
        assert!(
            buf.starts_with('{'),
            "拼出的串必须以 `{{` 开头，实际: {}",
            buf
        );
        assert_eq!(buf, r#"{"nested":{"a":1}}"#, "实际: {}", buf);
        assert!(
            serde_json::from_str::<serde_json::Value>(&buf).is_ok(),
            "必须能解析成完整合法 JSON，实际: {}",
            buf
        );
    }

    /// 顺序钉死：同一个短前缀帧 `{`，buf 完整 vs 不完整，第 5 步的判定必须相反 ——
    /// 前置条件必须挂在第 5 步本身，而不是被更前面的步骤悄悄接管，也不能被更后面的
    /// 步骤（6/7）兜底出错误结果。两种 buf 都不触发第 1~4 步，唯一的分岔点就是第 5 步
    /// 新增的 `is_complete_json(buf)` 前置条件。
    #[test]
    fn test_merge_short_prefix_frame_gating_depends_on_buf_completeness() {
        // buf 不完整：单独成帧的 `{` 必须被追加（落到第 7 步），不能被第 5 步丢弃。
        let incomplete_result = merge_tool_input(r#"{"nested":"#, "{");
        assert_eq!(
            incomplete_result, r#"{"nested":{"#,
            "buf 不完整时，单独成帧的 `{{` 必须被追加，不能丢弃，实际: {}",
            incomplete_result
        );

        // buf 完整：更短的迟到快照仍应被丢弃（第 5 步应正常触发，不能被修复误伤）。
        let complete_result = merge_tool_input(r#"{"a":1}"#, "{");
        assert_eq!(
            complete_result, r#"{"a":1}"#,
            "buf 完整时，更短的迟到旧快照仍应被丢弃，实际: {}",
            complete_result
        );
    }

    // ============ JSON 修复层（缓解④，根治向）离线注入测试 ============
    // 数据源：Claude Code 官方 issue 坐实的真实坏帧成因——
    //   #20015 Windows 路径反斜杠（\U 非法转义）、#69522 长 unicode 转义、#29715 裸控制符/smart quote。
    // 契约：repair_tool_json 只在 from_str 已失败时被调用；返回 Some 必为合法 JSON，否则 None。

    /// 铁律①：合法 JSON 绝不进入修复函数——但即便误入，也应原样往返、语义不变（幂等安全网）。
    #[test]
    fn test_repair_noop_on_valid_json() {
        let valid = r#"{"path":"C:/Users/foo","content":"hello world"}"#;
        // repair_tool_json 内部先 char_level 再复验；合法 JSON 的 char_level 不改结构、复验即过。
        let repaired = repair_tool_json(valid).expect("合法 JSON 应能往返");
        let a: serde_json::Value = serde_json::from_str(valid).unwrap();
        let b: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(a, b, "合法 JSON 修复往返后语义必须完全一致");
    }

    /// #20015：Windows 路径反斜杠 `C:\Users`（模型直接吐字面 `\U`，JSON 非法转义）。
    /// 现状：客户端 JSON.parse 失败 → Invalid tool parameters。修复后合法，客户端不再报错。
    ///
    /// 诚实边界：`\U`、`\d` 是 JSON 非法转义 → 修复层把反斜杠降级成字面 `\\`，值正确还原。
    /// 但 `\t`（如 `\test.txt`）是 JSON **合法**转义（制表符）——修复层**不碰合法转义**（碰了会破坏
    /// 正常场景），故这里用 `program.exe` 这类**不含合法转义字符**的路径，锁死"非法转义被正确还原"。
    #[test]
    fn test_repair_windows_path_backslash() {
        // content 值里 C:\Users\dwgx\program.exe —— \U \d \p 都是 JSON 非法转义（无合法转义歧义）。
        let bad = r#"{"file_path":"C:\Users\dwgx\program.exe"}"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提：Windows 反斜杠路径确为非法 JSON"
        );
        let fixed = repair_tool_json(bad).expect("Windows 路径反斜杠应可修复");
        let v: serde_json::Value = serde_json::from_str(&fixed).expect("修复后必合法");
        assert_eq!(
            v["file_path"].as_str().unwrap(),
            r"C:\Users\dwgx\program.exe",
            "修复后路径值应还原成字面反斜杠（非法转义 \\U\\d\\p 降级为字面）"
        );
    }

    /// #29715：裸控制符（真实换行/制表符混进字符串值，未转义）→ JSON 非法。修复后转义成 \n/\t。
    #[test]
    fn test_repair_bare_control_chars() {
        // content 值里有真实换行和 tab（裸控制符），JSON 字符串内非法。
        let bad = "{\"content\":\"line1\nline2\tend\"}";
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提：裸控制符确为非法 JSON"
        );
        let fixed = repair_tool_json(bad).expect("裸控制符应可修复");
        let v: serde_json::Value = serde_json::from_str(&fixed).expect("修复后必合法");
        assert_eq!(
            v["content"].as_str().unwrap(),
            "line1\nline2\tend",
            "修复后控制符应还原为真实换行/制表符（值语义不变）"
        );
    }

    /// 截断输出（流被中途切断，缺收尾 `"` 和 `}`）→ 结构层补全。
    #[test]
    fn test_repair_truncated_structure() {
        let bad = r#"{"path":"a.txt","content":"unfinished"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提：截断串确为非法 JSON"
        );
        let fixed = repair_tool_json(bad).expect("截断应可补全");
        let v: serde_json::Value = serde_json::from_str(&fixed).expect("补全后必合法");
        assert_eq!(v["path"].as_str().unwrap(), "a.txt");
        assert_eq!(v["content"].as_str().unwrap(), "unfinished");
    }

    /// #69522：截断的 `\u` 转义（`\uD83`——不足 4 位 hex）→ 降级字面，复验兜底。
    #[test]
    fn test_repair_truncated_unicode_escape() {
        let bad = r#"{"q":"emoji \uD83"}"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提：截断 \\u 转义确为非法 JSON"
        );
        // 能修成合法即达标（降级为字面 \uD83 文本），语义退化可接受、总比客户端整个报错强。
        let fixed = repair_tool_json(bad).expect("截断 unicode 转义应可修成合法");
        assert!(
            serde_json::from_str::<serde_json::Value>(&fixed).is_ok(),
            "修复后必为合法 JSON"
        );
    }

    /// 洞4:合法 UTF-16 代理对(😀 = 😀)必须**原样保留**,不被误降级。
    #[test]
    fn test_repair_keeps_valid_surrogate_pair() {
        // 构造一个整体非法(裸控制符触发 repair)、但含合法代理对的串,验证代理对不被破坏。
        let bad = "{\"emoji\":\"\\uD83D\\uDE00\nx\"}"; // 含真实换行=裸控制符→非法,触发 repair
        assert!(serde_json::from_str::<serde_json::Value>(bad).is_err());
        let fixed = repair_tool_json(bad).expect("应可修复");
        let v: serde_json::Value = serde_json::from_str(&fixed).expect("修复后合法");
        // 合法代理对应解码成 😀,不被降级为字面。
        assert!(
            v["emoji"].as_str().unwrap().contains('😀'),
            "合法代理对必须保留为 emoji"
        );
    }

    /// 洞4:孤立高代理(无低代理配对)→ 降级字面,修成合法 JSON。
    #[test]
    fn test_repair_isolated_high_surrogate() {
        let bad = r#"{"x":"\uD83Dnext"}"#; // 高代理后跟普通文本,孤立
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提:孤立高代理非法"
        );
        let fixed = repair_tool_json(bad).expect("孤立高代理应可降级修复");
        assert!(
            serde_json::from_str::<serde_json::Value>(&fixed).is_ok(),
            "修复后必合法"
        );
    }

    /// 洞4:孤立低代理 → 降级字面,修成合法 JSON。
    #[test]
    fn test_repair_isolated_low_surrogate() {
        let bad = r#"{"x":"\uDE00abc"}"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提:孤立低代理非法"
        );
        let fixed = repair_tool_json(bad).expect("孤立低代理应可降级修复");
        assert!(
            serde_json::from_str::<serde_json::Value>(&fixed).is_ok(),
            "修复后必合法"
        );
    }

    /// glued 粘连修复：头部多余 `}` 剥除后得到合法 JSON。
    #[test]
    fn test_repair_glued_leading_brace() {
        let bad = r#"}{"path": "src/foo.rs"}"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提：glued 串非合法 JSON"
        );
        let fixed = repair_tool_json(bad).expect("glued 粘连应可修复");
        let v: serde_json::Value = serde_json::from_str(&fixed).expect("修复后必须是合法 JSON");
        assert_eq!(v["path"].as_str().unwrap(), "src/foo.rs");
    }

    /// glued 粘连修复（带空白分隔）：`} {` 变体同样可剥除。
    #[test]
    fn test_repair_glued_with_space() {
        let bad = r#"} {"key": "value"}"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提：非法"
        );
        let fixed = repair_tool_json(bad).expect("glued (带空白) 应可修复");
        let v: serde_json::Value = serde_json::from_str(&fixed).unwrap();
        assert_eq!(v["key"].as_str().unwrap(), "value");
    }

    /// glued 修复保守性：`}` 后无 `{` 时返回 None，不乱猜。
    #[test]
    fn test_repair_glued_no_opening_brace_returns_none() {
        let bad = r#"}not_json_at_all"#;
        assert!(
            repair_tool_json(bad).is_none(),
            "无 `{{` 时修复层应返回 None"
        );
    }

    /// 洞1:整包双重编码解包——顶层是被字符串编码的 object → 解一层还原。
    #[test]
    fn test_unwrap_double_encoded_object() {
        // 双重编码:整个 {"path":"a.txt"} 被再套一层字符串编码。
        let double = r#""{\"path\":\"a.txt\"}""#;
        // 顶层 from_str 成功但得到 String(漏过 repair 层)。
        assert!(
            serde_json::from_str::<serde_json::Value>(double)
                .unwrap()
                .is_string()
        );
        let unwrapped = unwrap_double_encoded(double).expect("应解一层");
        let v: serde_json::Value = serde_json::from_str(&unwrapped).unwrap();
        assert_eq!(v["path"].as_str().unwrap(), "a.txt");
    }

    /// review confirmed 回归(端到端):合法的双重编码串经 flush_tool_input 必须被 unwrap 成 object
    /// 再发出。这锁死"unwrap 在函数尾统一出口执行"——修复前 repair 成功分支 early-return 会绕过它,
    /// 修复后两条路径(原本合法 / 修复后)都经同一 unwrap 出口。此处走"原本合法"路径(from_str 成功
    /// → 跳过 repair → 命中尾部 unwrap),验证出口本身正确;repair 分支的 fall-through 由结构保证
    /// (assembled=repaired 后不再 return,与本路径汇合到同一 unwrap)。
    #[test]
    fn test_flush_unwraps_double_encoded_at_exit() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.6", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        // 合法的双重编码:整个 {"path":"a.txt"} 被再套一层字符串编码(from_str 成功但得 String)。
        let double = r#""{\"path\":\"a.txt\"}""#;
        assert!(
            serde_json::from_str::<serde_json::Value>(double)
                .unwrap()
                .is_string(),
            "前提:顶层 from_str 成功但是 String(双重编码,会漏过 repair)"
        );
        let evs = ctx.flush_tool_input(0, double.to_string());
        let delta = evs
            .iter()
            .find(|e| e.data["delta"]["type"] == "input_json_delta")
            .expect("应发出 input_json_delta");
        let partial = delta.data["delta"]["partial_json"].as_str().unwrap();
        let v: serde_json::Value = serde_json::from_str(partial).expect("发出的必是合法 JSON");
        assert!(
            v.is_object(),
            "双重编码必须在出口被 unwrap 成 object,实际={}",
            partial
        );
        assert_eq!(v["path"].as_str().unwrap(), "a.txt");
    }

    /// 洞1:双重编码 array 也解;标量/普通 object 不动。
    #[test]
    fn test_unwrap_double_encoded_boundaries() {
        // array 双重编码 → 解。
        let arr = r#""[1,2,3]""#;
        assert!(unwrap_double_encoded(arr).is_some());
        // 正常 object(非双重编码)→ 不动(顶层不是 String)。
        assert!(unwrap_double_encoded(r#"{"a":1}"#).is_none());
        // 顶层是字符串但内层是标量(不是 object/array)→ 不动(不臆测)。
        assert!(unwrap_double_encoded(r#""hello""#).is_none());
        assert!(unwrap_double_encoded(r#""42""#).is_none());
    }

    /// 端到端：flush_tool_input 收到非法 JSON（开关默认开）→ 修复成功 → 发出的 partial_json 必合法。
    /// 这是客户端真正消费的字节，锁死"客户端不再报 Invalid tool parameters"。
    #[test]
    fn test_flush_tool_input_repairs_before_sending() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.6", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        // Windows 路径非法转义（#20015 真实成因）。
        let bad = r#"{"file_path":"C:\Users\x\a.txt"}"#.to_string();
        let evs = ctx.flush_tool_input(0, bad);
        let delta = evs
            .iter()
            .find(|e| e.data["delta"]["type"] == "input_json_delta")
            .expect("应发出 input_json_delta");
        let partial = delta.data["delta"]["partial_json"].as_str().unwrap();
        assert!(
            serde_json::from_str::<serde_json::Value>(partial).is_ok(),
            "flush 发给客户端的 partial_json 必须是合法 JSON（修复层已介入）：{}",
            partial
        );
    }

    /// 真实上游模式回归（2026-07-13 本地网关 KIRO_TOOL_TRACE 抓包坐实）：Kiro toolUseEvent.input
    /// 是**纯增量碎片**——每帧只带新片段（`{"path": "` → `test.txt"` → `, "con` → …），buf 单调增长、
    /// 全程无 `}{` 粘连，最后一帧拼成完整合法 JSON。含反斜杠 / emoji / 多语言 / 引号亦无碍。
    /// 这是 Invalid tool parameters 类型 C 修复覆盖的主路径，此测试锁死其正确性防回归。
    #[test]
    fn test_merge_real_upstream_incremental_capture() {
        // 逐帧照抄一次真实抓包的碎片序列（含转义反斜杠与 emoji）。
        let frames = [
            r#"{"path": ""#,
            r#"test.txt""#,
            r#", "con"#,
            r#"tent": "H"#,
            "ello World! ",
            r#"🌍\n\nend"#,
            r#""}"#,
        ];
        let mut buf = String::new();
        let mut glued_ever = false;
        for f in frames {
            buf = merge_tool_input(&buf, f);
            if buf.contains("}{") {
                glued_ever = true;
            }
        }
        assert!(!glued_ever, "纯增量拼装全程不应出现 }}{{ 粘连");
        assert_eq!(
            buf,
            r#"{"path": "test.txt", "content": "Hello World! 🌍\n\nend"}"#
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&buf).is_ok(),
            "纯增量碎片最终应拼成合法 JSON（类型 C 主路径）"
        );
    }

    /// 曾经的"类型 A 只能透传"契约，已被修复层（缓解④，默认开）**升级为根治**：上游模型帧含 JSON
    /// 非法转义（`\x` —— JSON 只认 `\uXXXX`）时，`flush_tool_input` 先修成合法 JSON 再发，客户端能
    /// 正常 parse，不再报 Invalid tool parameters。此测试锁死"端到端非法 → 发出的必是合法 JSON"。
    /// （修复层关闭时的原样透传行为由纯函数契约 + repair_off 专测覆盖。）
    #[test]
    fn test_process_tool_use_type_a_illegal_escape_repaired() {
        use crate::kiro::model::events::ToolUseEvent;
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.6", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        // `\x41` 是 JSON 非法转义（合法应为 `A`）——模拟上游模型控制 token 抽风产出的非法串。
        let illegal = r#"{"path":"a.txt","content":"bad\x41escape"}"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(illegal).is_err(),
            "前提：该串确为非法 JSON（\\x 转义）"
        );
        let evs = ctx.process_tool_use(&ToolUseEvent {
            name: "write_file".to_string(),
            tool_use_id: "toolu_typea".to_string(),
            input: illegal.to_string(),
            stop: true,
        });
        let delta = evs
            .iter()
            .find(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "input_json_delta"
            })
            .expect("应发出 input_json_delta（修复后，不吞）");
        let assembled = delta.data["delta"]["partial_json"].as_str().unwrap();
        assert!(
            serde_json::from_str::<serde_json::Value>(assembled).is_ok(),
            "修复层默认开：发给客户端的必是合法 JSON，实际={}",
            assembled
        );
    }

    /// 泄漏 token 清洗（收严高信号）：行首泄漏词直贴 **CJK/全角** 粘连 → 剥离；正常英文用法（含
    /// ASCII 冒号/数字/大写）→ 绝不误删。court/課/课 独占整行 → 剥（高置信 #70544 泄漏）。
    #[test]
    fn test_strip_leaked_prefix() {
        // 辅助：只取清洗后文本（新签名返回 (String, StripHit)）。
        let s = |line: &str| StreamContext::strip_leaked_prefix(line).0;
        // CJK/全角粘连 → 剥离。
        assert_eq!(s("course重读文件"), "重读文件");
        assert_eq!(s("課我加的是"), "我加的是");
        assert_eq!(s("care：我把"), "：我把"); // 全角冒号
        assert_eq!(s("count你好"), "你好");
        assert_eq!(s("court重读"), "重读"); // 新增 court
        assert_eq!(s("card表格"), "表格"); // 新增 card
        assert_eq!(s("call调用"), "调用"); // 新增 call
        // court/課/课 独占整行 → 剥空(保留换行)。
        assert_eq!(s("court\n"), "\n");
        assert_eq!(s("court"), "");
        assert_eq!(s("課\n"), "\n");
        // 【收严关键】正常英文含 ASCII 冒号/数字/大写 → 绝不误删(旧逻辑会误剥)。
        assert_eq!(s("count: 42"), "count: 42"); // 半角冒号
        assert_eq!(s("countDown()"), "countDown()"); // 大写
        assert_eq!(s("care2share"), "care2share"); // 数字
        assert_eq!(s("courseCatalog"), "courseCatalog"); // 大写
        assert_eq!(s("card#1"), "card#1"); // ASCII 标点
        // 正常英文：词后空格 / 小写延续 → 原样保留。
        assert_eq!(s("count the items"), "count the items");
        assert_eq!(s("counter offer"), "counter offer");
        assert_eq!(s("careful now"), "careful now");
        assert_eq!(s("call me"), "call me");
        // call/card/count/care/course 独占整行 → **不**享特例(可能是正常内容),保守不剥。
        assert_eq!(s("count"), "count");
        assert_eq!(s("card"), "card");
        assert_eq!(s("call"), "call");
        // 非泄漏词开头 → 原样。
        assert_eq!(s("hello世界"), "hello世界");
    }

    /// 诊断计数：strip 命中信息（stripped / standalone）正确——供收尾泄漏诊断计数。
    #[test]
    fn test_strip_leaked_prefix_hit_flags() {
        let (_, hit) = StreamContext::strip_leaked_prefix("court\n"); // 独占整行
        assert!(
            hit.stripped && hit.standalone,
            "独占整行 court 应 stripped+standalone"
        );
        let (_, hit) = StreamContext::strip_leaked_prefix("course重读"); // 粘连非独占
        assert!(
            hit.stripped && !hit.standalone,
            "粘连剥离应 stripped 但非 standalone"
        );
        let (_, hit) = StreamContext::strip_leaked_prefix("count: 42"); // 正常英文不剥
        assert!(!hit.stripped && !hit.standalone, "正常英文不应命中");
    }

    /// clean_leaked_tokens：只在行首处理，多行时每行行首各判一次；并累加诊断计数。
    #[test]
    fn test_clean_leaked_tokens_multiline() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.6", 1, false, HashMap::new());
        let input = "course重读\nnormal line\ncount你好";
        assert_eq!(ctx.clean_leaked_tokens(input), "重读\nnormal line\n你好");
        assert_eq!(ctx.leaked_stripped, 2, "剥了 course / count 两个");
        assert_eq!(ctx.leaked_saturation_lines, 0, "无独占整行泄漏");
    }

    /// saturation 计数：满屏纯 court 独占行 → 每行计入 saturation。
    #[test]
    fn test_clean_leaked_tokens_saturation_count() {
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4.8", 1, false, HashMap::new());
        let input = "court\ncourt\ncourt\n";
        ctx.clean_leaked_tokens(input);
        assert_eq!(
            ctx.leaked_saturation_lines, 3,
            "3 行纯 court 独占行=saturation 信号"
        );
        assert_eq!(ctx.leaked_stripped, 3);
    }

    /// process_tool_use 端到端：非前缀双对象场景，flush 出的 partial_json 必须是合法 JSON（第二帧）。
    #[test]
    fn test_process_tool_use_nonprefix_double_object_emits_legal_json() {
        use crate::kiro::model::events::ToolUseEvent;
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.6", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let _ = ctx.process_tool_use(&ToolUseEvent {
            name: "AskUserQuestion".to_string(),
            tool_use_id: "toolu_np".to_string(),
            input: r#"{"a":1}"#.to_string(),
            stop: false,
        });
        let evs = ctx.process_tool_use(&ToolUseEvent {
            name: "AskUserQuestion".to_string(),
            tool_use_id: "toolu_np".to_string(),
            input: r#"{"a":2}"#.to_string(),
            stop: true,
        });
        let delta = evs
            .iter()
            .find(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "input_json_delta"
            })
            .expect("应有 input_json_delta");
        let assembled = delta.data["delta"]["partial_json"].as_str().unwrap();
        assert_eq!(assembled, r#"{"a":2}"#, "非前缀双对象只发最新完整对象");
        assert!(
            serde_json::from_str::<serde_json::Value>(assembled).is_ok(),
            "流式 flush 的 partial_json 必须合法"
        );
    }

    /// 隔离回归：tool_use input 含小于号 + 全角竖线 DSML 起始标记（U+FF5C）时，
    /// 拼装完全不经过 strip_dsml_markers，原样保留（Claude 系）。
    #[test]
    fn test_tool_input_not_stripped_by_dsml_claude() {
        use crate::kiro::model::events::ToolUseEvent;
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.6", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let payload = r#"{"code":"if a < b","note":"<｜DSML｜function_calls｜>","x":"a<｜tool"}"#;
        let evs = ctx.process_tool_use(&ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "toolu_d".to_string(),
            input: payload.to_string(),
            stop: true,
        });
        let delta = evs
            .iter()
            .find(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "input_json_delta"
            })
            .expect("delta");
        let assembled = delta.data["delta"]["partial_json"].as_str().unwrap();
        assert_eq!(assembled, payload, "tool input 应原样保留，DSML 未碰");
    }

    /// 隔离回归：国产模型（deepseek）下 DSML 门控放行，但 tool input 拼装路径同样不经过剥离。
    #[test]
    fn test_tool_input_not_stripped_by_dsml_deepseek() {
        use crate::kiro::model::events::ToolUseEvent;
        let mut ctx = StreamContext::new_with_thinking("deepseek-v3", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let payload = r#"{"code":"if a < b","note":"<｜DSML｜function_calls｜>"}"#;
        let evs = ctx.process_tool_use(&ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "toolu_d2".to_string(),
            input: payload.to_string(),
            stop: true,
        });
        let delta = evs
            .iter()
            .find(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "input_json_delta"
            })
            .expect("delta");
        let assembled = delta.data["delta"]["partial_json"].as_str().unwrap();
        assert_eq!(
            assembled, payload,
            "国产模型下 tool input 也应原样，DSML 只作用于 text/thinking"
        );
    }

    // ============ 截断诊断归因标签（短板 2.5，纯可观测）离线测试 ============
    // classify_tool_json_defect 只在 from_str 已失败分支被调、只写日志，绝不进控制流。
    // 判据与 repair 层同源：truncated=结构未闭合/串未终结、illegal_chars=非法转义或裸控制符。

    /// 截断（缺收尾 `"` 和 `}`）→ 归因 Truncated。
    #[test]
    fn test_classify_defect_truncated() {
        let s = r#"{"path":"a.txt","content":"unfinished"#;
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::Truncated);
    }

    /// 非法转义（`\x`）+ 结构完整闭合 → 归因 IllegalChars。
    #[test]
    fn test_classify_defect_illegal_chars() {
        let s = r#"{"path":"bad\x41escape"}"#;
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::IllegalChars);
    }

    /// 裸控制符（真实换行未转义）+ 结构完整 → 归因 IllegalChars。
    #[test]
    fn test_classify_defect_bare_control() {
        let s = "{\"content\":\"line1\nline2\"}";
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::IllegalChars);
    }

    /// 既含非法转义又结构截断 → 归因 TruncatedAndIllegal。
    #[test]
    fn test_classify_defect_truncated_and_illegal() {
        let s = r#"{"path":"C:\Users\x"#;
        assert_eq!(
            classify_tool_json_defect(s),
            ToolJsonDefect::TruncatedAndIllegal
        );
    }

    /// 结构闭合、字符合法，但 `}{` 粘连（非前缀双对象）→ 归因 Malformed。
    #[test]
    fn test_classify_defect_malformed_glued() {
        let s = r#"{"a":1}{"b":2}"#;
        let scan = scan_tool_json(s);
        assert!(scan.glued, "应识别出 }}{{ 粘连");
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::Malformed);
    }

    // ============ malformed 子型细分（第一步:解开类型 A/C 的诊断钥匙）离线测试 ============
    // malformed_subkind 只在归因 Malformed 时调用,纯诊断不进控制流。判据源=serde 官方错误消息 + scan.glued。
    // 每条都先确认:①确实 parse 失败 ②归因确为 Malformed(结构闭合+字符合法)③子型标签正确。

    #[test]
    fn test_malformed_subkind_glued() {
        // `}{` 粘连:两个完整对象黏一起——偏类型 C(合并/上游重写),优先怀疑我们侧。
        let s = r#"{"a":1}{"b":2}"#;
        assert!(serde_json::from_str::<serde_json::Value>(s).is_err());
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::Malformed);
        assert_eq!(malformed_subkind(s), "glued");
    }

    #[test]
    fn test_malformed_subkind_trailing_comma() {
        // 尾逗号:偏模型侧多吐一个逗号。
        let s = r#"{"a":1,"b":2,}"#;
        assert!(serde_json::from_str::<serde_json::Value>(s).is_err());
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::Malformed);
        assert_eq!(malformed_subkind(s), "trailing_comma");
    }

    #[test]
    fn test_malformed_subkind_missing_comma() {
        // 两个键值之间缺分隔逗号:偏模型侧漏吐/上游丢了逗号帧。
        let s = r#"{"a":1"b":2}"#;
        assert!(serde_json::from_str::<serde_json::Value>(s).is_err());
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::Malformed);
        assert_eq!(malformed_subkind(s), "missing_comma");
    }

    #[test]
    fn test_malformed_subkind_expected_value() {
        // 键后缺值:偏模型侧生成中断在值前。
        let s = r#"{"a":}"#;
        assert!(serde_json::from_str::<serde_json::Value>(s).is_err());
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::Malformed);
        assert_eq!(malformed_subkind(s), "expected_value");
    }

    #[test]
    fn test_malformed_subkind_key_not_string() {
        // 键不是字符串(裸键):偏模型侧吐了非法键。
        let s = r#"{a:1}"#;
        assert!(serde_json::from_str::<serde_json::Value>(s).is_err());
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::Malformed);
        assert_eq!(malformed_subkind(s), "key_not_string");
    }

    #[test]
    fn test_malformed_subkind_trailing_chars() {
        // 首个完整值后有非 `}{` 的多余字符(不触发 glued 那条)——偏上游多发尾随。
        let s = r#"{"a":1} garbage"#;
        assert!(serde_json::from_str::<serde_json::Value>(s).is_err());
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::Malformed);
        // 非 `}{`,glued=false,应落到 trailing_chars。
        assert_eq!(malformed_subkind(s), "trailing_chars");
    }

    /// 短板2：截断跨轮恢复纯决策——恢复开关开 + 修复层开 + 归因真截断,三条件全满足才触发。
    #[test]
    fn test_contains_textified_tool_call_detector() {
        // 文本化 invoke 标记(不论是否带 antml: 前缀)应命中。
        assert!(contains_textified_tool_call(r#"<invoke name="Bash">"#));
        assert!(contains_textified_tool_call(r#"<invoke name="Bash">"#));
        assert!(contains_textified_tool_call("</invoke>"));
        assert!(contains_textified_tool_call(
            r#"<parameter name="command">"#
        ));
        assert!(contains_textified_tool_call("</function_calls>"));
        // 正常文本不误命中。
        assert!(!contains_textified_tool_call(
            "这是一段正常的助手回复,讲 invoke 概念但无标签"
        ));
        assert!(!contains_textified_tool_call(
            "函数调用 function calls 讨论"
        ));
        assert!(!contains_textified_tool_call(""));
    }

    #[test]
    fn test_should_recover_truncation_decision() {
        use ToolJsonDefect::*;
        // 恢复开关关 → 任何情况都不触发（默认行为不变）。
        assert!(!should_recover_truncation(Truncated, false, true));
        assert!(!should_recover_truncation(TruncatedAndIllegal, false, true));
        // 修复层关 → 不触发（无法断言"修复也补不回",退回②/③原语义）。
        assert!(!should_recover_truncation(Truncated, true, false));
        assert!(!should_recover_truncation(TruncatedAndIllegal, true, false));
        // 恢复开 + 修复开 + 真截断 → 触发。
        assert!(should_recover_truncation(Truncated, true, true));
        assert!(should_recover_truncation(TruncatedAndIllegal, true, true));
        // 恢复开 + 修复开但非截断畸形 → 不归本开关管（仍走②/③）。
        assert!(!should_recover_truncation(IllegalChars, true, true));
        assert!(!should_recover_truncation(Malformed, true, true));
    }

    /// 稳定短标签：日志字段值不随重构漂移。
    #[test]
    fn test_defect_as_str_labels() {
        assert_eq!(ToolJsonDefect::Truncated.as_str(), "truncated");
        assert_eq!(ToolJsonDefect::IllegalChars.as_str(), "illegal_chars");
        assert_eq!(
            ToolJsonDefect::TruncatedAndIllegal.as_str(),
            "truncated_and_illegal"
        );
        assert_eq!(ToolJsonDefect::Malformed.as_str(), "malformed");
    }
}
