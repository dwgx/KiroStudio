//! Anthropic → Kiro 协议转换器
//!
//! 负责将 Anthropic API 请求格式转换为 Kiro API 请求格式

use std::collections::{HashMap, HashSet};

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::kiro::model::requests::conversation::{
    AssistantMessage, ConversationState, CurrentMessage, HistoryAssistantMessage,
    HistoryUserMessage, KiroImage, Message, UserInputMessage, UserInputMessageContext, UserMessage,
};
use crate::kiro::model::requests::kiro::{
    AdditionalModelRequestFields, KiroOutputConfig,
};
use crate::kiro::model::requests::tool::{
    InputSchema, Tool, ToolResult, ToolSpecification, ToolUseEntry,
};

use super::image_resize::{ResizeConfig, ResizeError, maybe_shrink_image_blocking};
use super::types::{ContentBlock, ImageSource, MessagesRequest};

#[path = "schema_normalize.rs"]
mod schema_normalize;
pub(crate) use schema_normalize::normalize_json_schema;

#[path = "tool_compat.rs"]
mod tool_compat;
pub(crate) use tool_compat::{map_tool_input_from_kiro, set_tool_compat_mapping};
pub use tool_compat::restore_tool_use_for_client;
use tool_compat::{
    convert_tools, map_client_tool_name_to_kiro, map_tool_input_to_kiro, map_tool_name,
    tool_compat_mapping_enabled,
};

#[path = "history_overflow.rs"]
mod history_overflow;
pub(crate) use history_overflow::{
    apply_byte_overflow_guard, sanitize_history, truncate_history_if_needed, MAX_PAYLOAD_BYTES,
    TRUNCATION_PLACEHOLDER,
};

/// 从 base64（可能带 data: 前缀）的 PDF 提取纯文本。失败返回 None。
///
/// Kiro 上游不接受 Anthropic 的 `document`(application/pdf) 块。这里在网关侧做
/// 轻量文本抽取（无外部 PDF 库，直接扫描 PDF 内容流里的字面量字符串），把可读
/// 文本转成 `<document>` 文本块随消息下发，让模型至少能读到 PDF 内容。
fn extract_pdf_text_from_base64(data: &str) -> Option<String> {
    use base64::Engine;
    let data = data
        .rsplit_once(',')
        .map(|(_, tail)| tail)
        .unwrap_or(data)
        .trim();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    extract_pdf_text_from_bytes(&bytes)
}

/// 从 PDF 原始字节里抽取内容流字面量字符串（`(...)` 后接 Tj/TJ/' 操作符）。
fn extract_pdf_text_from_bytes(bytes: &[u8]) -> Option<String> {
    let pdf = String::from_utf8_lossy(bytes);
    let mut texts = Vec::new();
    let bytes = pdf.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'(' {
            i += 1;
            continue;
        }
        let Some((raw, next)) = parse_pdf_literal_string(&pdf, i) else {
            i += 1;
            continue;
        };
        i = next;
        // 只保留后面紧跟文本绘制操作符（Tj/TJ/'）的字面量，过滤掉非文本 payload
        let lookahead_end = (i + 32).min(bytes.len());
        let lookahead = &bytes[i..lookahead_end];
        if lookahead.windows(2).any(|w| w == b"Tj" || w == b"TJ") || lookahead.contains(&b'\'') {
            let text = raw.trim();
            if !text.is_empty() {
                texts.push(text.to_string());
            }
        }
    }

    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    }
}

/// 解析 PDF 字面量字符串 `(...)`，处理反斜杠转义（含八进制），返回 (内容, 结束位置)。
fn parse_pdf_literal_string(pdf: &str, start: usize) -> Option<(String, usize)> {
    let bytes = pdf.as_bytes();
    if bytes.get(start) != Some(&b'(') {
        return None;
    }
    let mut out = String::new();
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 1;
                if i >= bytes.len() {
                    break;
                }
                match bytes[i] {
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000c}'),
                    b'(' => out.push('('),
                    b')' => out.push(')'),
                    b'\\' => out.push('\\'),
                    b'\n' | b'\r' => {}
                    c if c.is_ascii_digit() && c < b'8' => {
                        let mut octal = vec![c];
                        for _ in 0..2 {
                            if i + 1 < bytes.len() && (b'0'..=b'7').contains(&bytes[i + 1]) {
                                i += 1;
                                octal.push(bytes[i]);
                            } else {
                                break;
                            }
                        }
                        if let Ok(value) =
                            u8::from_str_radix(std::str::from_utf8(&octal).unwrap_or_default(), 8)
                        {
                            out.push(value as char);
                        }
                    }
                    other => out.push(other as char),
                }
            }
            b')' => return Some((out, i + 1)),
            other => out.push(other as char),
        }
        i += 1;
    }
    None
}

/// 追加到 Write 工具 description 末尾的内容
const WRITE_TOOL_DESCRIPTION_SUFFIX: &str = "- IMPORTANT: If the content to write exceeds 150 lines, you MUST only write the first 50 lines using this tool, then use `Edit` tool to append the remaining content in chunks of no more than 50 lines each. If needed, leave a unique placeholder to help append content. Do NOT attempt to write all content at once.";

/// 追加到 Edit 工具 description 末尾的内容
const EDIT_TOOL_DESCRIPTION_SUFFIX: &str = "- IMPORTANT: If the `new_string` content exceeds 50 lines, you MUST split it into multiple Edit calls, each replacing no more than 50 lines at a time. If used to append content, leave a unique placeholder to help append content. On the final chunk, do NOT include the placeholder.";

/// 追加到 Bash 工具 description 末尾的内容（对齐 kiro 生态：防大命令触发上游参数截断）。
const BASH_TOOL_DESCRIPTION_SUFFIX: &str = "- IMPORTANT: Do not send very large commands, inline scripts, or heredocs through Bash. If a command would exceed 100 lines, 8,000 characters, or roughly 4,000 tokens, create or modify a script/file with chunked Write/Edit calls first, then run a short Bash command that executes it. If a Bash attempt fails due to argument size or truncation, do not retry the same large command; split it smaller.";

/// 追加到系统提示词的分块写入策略
const SYSTEM_CHUNKED_POLICY: &str = "\
When the Write or Edit tool has content size limits, always comply silently. \
Never suggest bypassing these limits via alternative tools. \
Never ask the user whether to switch approaches. \
Complete all chunked operations without commentary.";

/// Claude Code 归因头前缀。
///
/// CC 会把形如 `x-anthropic-billing-header: cc_version=...;cch=...` 的归因头放在
/// system 消息的**第一块**。其中 `cc_version`、`cch` 等字段每次请求都会漂移，
/// 而这一块位于整个 prompt 的最前端。上游 Bedrock prefix cache 要求前缀逐字节一致
/// 才命中（第 N 字节变化则其后全部失效），因此这一块的漂移会让上游缓存命中率≈0。
pub(super) const BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";

/// 归一化后的固定占位符，替换整行漂移的归因头，稳定住转发给上游的 prompt 前缀。
pub(super) const BILLING_HEADER_PLACEHOLDER: &str = "__anthropic_billing_header__";

/// 归一化 Claude Code 的归因头文本块。
///
/// 若 `text` 以归因头前缀开头（该块整行都是每请求漂移的归因字段），折叠成固定占位符；
/// 否则原样返回。此函数同时供两处调用：
/// 1. [`build_history`] 的 system 拼接路径——归一化**转发给上游**的字节，稳定缓存前缀；
/// 2. 影子缓存记账路径——本就归一化，保持两侧一致。
///    // 影子缓存记账已移至 StreamContext.cache_usage (prompt_cache_enabled 开启时)
///
/// 归一化保守：只折叠确定每请求漂移的归因头整块，不触碰其余稳定的 system 内容。
pub(super) fn canonicalize_billing_header(text: &str) -> &str {
    if text.starts_with(BILLING_HEADER_PREFIX) {
        BILLING_HEADER_PLACEHOLDER
    } else {
        text
    }
}

/// 环境噪音剥离开关的运行时镜像（`config.strip_env_noise`，TIER3 热更）。
///
/// 归一化路径（[`canonicalize_system_text`]）拿不到 config，故沿用
/// [`super::handlers`] 已验证的进程级原子镜像范式：main 启动按配置写入、admin 改开关
/// 立即改写、归一化热路径读镜像，无需重启、无锁近零成本。默认 true（与 config 默认一致）。
///
/// **关键**：转发字节路径（[`build_history`]）与影子缓存记账路径
/// 都经由 [`canonicalize_system_text`] 读同一镜像，
/// 保证两侧对同一 system 块施加**完全一致**的变换，记账与真实缓存不脱节。
/// // 影子缓存记账已移至 StreamContext.cache_usage (prompt_cache_enabled 开启时)
static STRIP_ENV_NOISE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// 设置环境噪音剥离开关（main 启动接线 / admin 热更调用，立即生效，下个请求即读到新值）。
pub fn set_strip_env_noise(enabled: bool) {
    STRIP_ENV_NOISE.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

fn strip_env_noise_enabled() -> bool {
    STRIP_ENV_NOISE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Kiro 原生 effort 开关镜像（`config.native_thinking_effort_enabled`，**默认 false**）。
///
/// 开启后，白名单模型 + thinking 启用时，请求改用顶层
/// `additionalModelRequestFields.output_config.effort` 触发上游原生 reasoning
/// （实测只有它能触发 `reasoningContentEvent`，XML 标签既不触发还污染历史上下文），
/// 并抑制 `<thinking_mode>` 标签注入；关闭（默认）时行为逐字节不变。
///
/// [`build_history`] / [`convert_request`] 是纯转换拿不到 config，沿用 [`STRIP_ENV_NOISE`]
/// 同款进程级原子镜像：main 启动按配置写入、admin 改开关立即改写、转换热路径读镜像。
static NATIVE_THINKING_EFFORT_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 设置 native effort 开关（main 启动接线 / admin 热更调用，立即生效，下个请求即读到新值）。
pub fn set_native_thinking_effort_enabled(enabled: bool) {
    NATIVE_THINKING_EFFORT_ENABLED.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

fn native_thinking_effort_enabled() -> bool {
    NATIVE_THINKING_EFFORT_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// 工具顶层 description 的字符上限镜像（`config.tool_description_max_chars`，TIER3 热更）。
///
/// [`convert_tools`] 是纯转换、拿不到 config，沿用 [`STRIP_ENV_NOISE`] 同款进程级原子镜像：
/// main 启动按配置写入、admin 改值立即改写、转换热路径读镜像，无锁近零成本。
/// schema 内嵌 description 上限取此值的 1/5（保持既有 10000/2000 比例），0 表示不截断。
static TOOL_DESC_MAX_CHARS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(10000);

/// 设置工具描述上限（main 启动接线 / admin 热更调用，立即生效，下个请求即读到新值）。
pub fn set_tool_description_max_chars(n: usize) {
    TOOL_DESC_MAX_CHARS.store(n, std::sync::atomic::Ordering::Relaxed);
}

/// 顶层 description 上限（0 = 不截断）。
fn tool_description_max_chars() -> usize {
    TOOL_DESC_MAX_CHARS.load(std::sync::atomic::Ordering::Relaxed)
}

/// schema 内嵌 description 上限：顶层的 1/5（保持既有 10000→2000 比例）。0（不截断）时同样不截断。
fn schema_description_max_chars() -> usize {
    let top = tool_description_max_chars();
    if top == 0 { 0 } else { (top / 5).max(1) }
}

/// 按字符边界安全截断（防多字节切断）。`max==0` 表示不截断，原样返回。
fn truncate_chars(s: &str, max: usize) -> String {
    if max == 0 {
        return s.to_string();
    }
    match s.char_indices().nth(max) {
        Some((idx, _)) => s[..idx].to_string(),
        None => s.to_string(),
    }
}

/// 归一化单个 system 文本块：折叠归因头 + （开关开启时）剥离环境噪音。
///
/// Claude Code 每次请求都会在 system 里携带「每次漂移」的环境上下文（工作目录/日期/
/// 平台的 `<env>` 块、`gitStatus:`、`Recent commits:`、模型名行等）。这些漂移行位于
/// prompt 前缀，只要变一个字节，上游 Bedrock prefix cache 其后全部失效（命中率≈0），
/// 且它们是关联「这是 Claude Code」的强指纹。剥离它们同时省 token、提命中率、降关联风险。
///
/// 返回 [`std::borrow::Cow`]：未发生任何改写时借用原串零分配；发生折叠/剥离时返回改写副本。
/// 该函数是转发字节与影子指纹两条路径的**唯一**归一化入口，确保两侧字节一致。
///
/// 保守原则：只剥离「确定每请求漂移」的整块 / 整行，绝不触碰稳定的 system 正文
/// （如工具说明、身份声明、任务指令）。
pub(super) fn canonicalize_system_text(text: &str) -> std::borrow::Cow<'_, str> {
    // 1. 归因头整块 → 固定占位符（无条件，历史行为）
    let folded = canonicalize_billing_header(text);
    if !std::ptr::eq(folded, text) {
        return std::borrow::Cow::Borrowed(folded);
    }
    // 2. 环境噪音剥离（受开关控制；默认开）
    if strip_env_noise_enabled()
        && let Some(stripped) = strip_env_noise_lines(text)
    {
        return std::borrow::Cow::Owned(stripped);
    }
    std::borrow::Cow::Borrowed(text)
}

/// 剥离 system 文本里每请求漂移的环境噪音行 / 整段。
///
/// 参照 kiro-account-manager `prompt_filter::filter_env_noise`（MIT），并补齐现代
/// Claude Code 的 `<env>...</env>` 标签块形式。仅当确有内容被剥离时返回 `Some(改写副本)`，
/// 否则返回 `None`（调用方据此零分配借用原串）。
///
/// 剥离目标全部是「确定每请求漂移」或「纯环境元数据」，不含稳定正文：
/// - `<env>...</env>` 整块（工作目录 / 平台 / 日期，每请求 / 每日漂移）；
/// - `# Environment` / `# auto memory` markdown 段（到下一个 `# ` 标题为止）；
/// - `gitStatus:` / `Recent commits:` 声明行、`.claude/projects/` 路径行；
/// - `Assistant knowledge cutoff` / `powered by the model named` 等模型/环境元数据行。
fn strip_env_noise_lines(text: &str) -> Option<String> {
    let mut changed = false;
    let mut out: Vec<&str> = Vec::new();
    let mut skip_section = false; // # Environment / # auto memory markdown 段
    let mut skip_env_tag = false; // <env>...</env> 标签块

    for line in text.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_lowercase();

        // <env>...</env> 块：整块剥离（含首尾标签行）。cwd/platform/date 每次漂移。
        if skip_env_tag {
            changed = true;
            if trimmed.contains("</env>") {
                skip_env_tag = false;
            }
            continue;
        }
        if trimmed.starts_with("<env>") {
            changed = true;
            // 兼容单行 <env>...</env>
            if !trimmed.contains("</env>") {
                skip_env_tag = true;
            }
            continue;
        }

        // # Environment / # auto memory 段：跳到下一个 `# ` 标题（保留该新标题）。
        if trimmed == "# Environment" || trimmed == "# auto memory" {
            skip_section = true;
            changed = true;
            continue;
        }
        if skip_section {
            if trimmed.starts_with("# ") {
                skip_section = false;
                // fall through：保留新标题行
            } else {
                changed = true;
                continue;
            }
        }

        // 单独漂移行 / 环境元数据行
        if trimmed.starts_with("gitStatus:")
            || trimmed.starts_with("Recent commits:")
            || trimmed.starts_with("Assistant knowledge cutoff")
            || lower.contains("powered by the model named")
            || trimmed.contains(".claude/projects/")
            || trimmed.contains("git status at the start of the conversation")
            || trimmed.contains("has been invoked in the following environment")
        {
            changed = true;
            continue;
        }

        out.push(line);
    }

    if !changed {
        return None;
    }
    Some(collapse_blank_lines(&out.join("\n")))
}

/// 连续空行合并为一行，并去除首尾空白（剥离整段后常留多余空行）。
fn collapse_blank_lines(s: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut blanks = 0;
    for line in s.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        out.push(line);
    }
    out.join("\n").trim().to_string()
}

/// 模型映射：将 Anthropic 模型名映射到 Kiro 模型 ID。
///
/// **已重构**:委托给声明式模型目录 [`super::model_catalog`](单一真相源)。
/// 旧实现是 `contains` 子串启发式，有三个真漏洞(Claude3 静默升贵档 / 高版本静默降级 /
/// 子串误命中),且模型清单散落四处易漂移。现改为「精确别名 → 结构化 family+版本 →
/// 老名近似(告警) → 未知拒绝」的分层解析，所有非精确命中都打 warn 日志(可观测)。
pub fn map_model(model: &str) -> Option<String> {
    super::model_catalog::resolve_kiro_id(model).map(|s| s.to_string())
}

/// 根据模型名称返回对应的上下文窗口大小。
///
/// 委托给模型目录 [`super::model_catalog::context_window`]:窗口值直接来自 `ModelSpec.context_window`
/// (单一真相源),不再复用 map_model 的映射结果拼判——避免继承 map_model 误判(如老名被当 1M)。
pub fn get_context_window_size(model: &str) -> i32 {
    super::model_catalog::context_window(model)
}

/// 转换结果
#[derive(Debug)]
pub struct ConversionResult {
    /// 转换后的 Kiro 请求
    pub conversation_state: ConversationState,
    /// 工具名称映射（短名称 → 原始名称），仅当存在超长工具名时非空
    pub tool_name_map: HashMap<String, String>,
    /// 本次请求声明的工具名集合（= 发给模型看到的名字，超长名已缩短）。
    /// 文本化 invoke 重组的"工具名硬护栏"用它：解析出的工具名必须在此集合里才允许捞回,
    /// 否则当普通文本吐出——宁可漏捞不可把正文里讨论的假命令误执行。
    pub known_tool_names: std::collections::HashSet<String>,
    /// 每个工具的**必需参数名**（`input_schema.required`），供流式层做 **Bug C** 校验：
    /// `tool_use` 参数 JSON 合法但缺必需字段（如 `Bash` 缺 `command`）。
    ///
    /// key 与 [`Self::known_tool_names`] 同口径（发给模型的名字，含缩短后的短名）。
    /// 无必需参数的工具不入表；空表 = 不校验。
    pub tool_required_fields: HashMap<String, Vec<String>>,
    /// Kiro 原生 effort 请求（`additionalModelRequestFields.output_config.effort`）。
    ///
    /// 仅当 `native_thinking_effort_enabled` 开启 **且** 模型在白名单 **且** thinking
    /// 启用时 Some；否则 None。开关默认关 ⇒ 恒 None，请求字节与旧版完全一致。
    /// 与 [`build_history`] 的 XML 抑制共用同一判定函数，保证「不发字段就不剥标签」。
    pub additional_model_request_fields: Option<AdditionalModelRequestFields>,
}

/// 转换错误
#[derive(Debug)]
pub enum ConversionError {
    UnsupportedModel(String),
    EmptyMessages,
    /// Claude Code 工具参数无法映射到 Kiro 上游。
    ///
    /// ⚠️ **2026-08-10 起全仓无产出点**（编译器会报 `never constructed`）。
    /// 曾经唯一的产出点是 `Read.pages`，现已改为**降级处理**
    /// （整读 + 把页范围意图写进 `explanation`，见 `map_tool_input_to_kiro` 的 `Read` 分支）
    /// —— 因为用一个可选的范围提示去否决整轮请求，代价（对话中断）远大于收益。
    ///
    /// **刻意保留而不删除**，两个理由：
    /// 1. 它有两处渲染分支（`handlers.rs:1875` 与 `:3119` → 400 `invalid_request_error`），
    ///    删变体会连带删掉那条对外契约；
    /// 2. 「客户端工具参数在 Kiro 侧确实无法表达」是**真实存在**的一类情形，
    ///    未来新增工具映射时很可能需要它。此时该判断的是「能否降级」而非直接沿用本变体
    ///    —— 只有当丢弃该参数会让工具**执行出错**（而非仅结果不精确）时才该用它。
    #[allow(dead_code)]
    ///
    /// 由 [`map_tool_input_to_kiro`] 返回，convert_request 侧把它转成客户端可读的 400，
    /// 而不是把无效参数透传给上游（那会得到更含糊的上游 400）。
    UnsupportedToolMapping { tool_name: String, reason: String },
}

impl std::fmt::Display for ConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConversionError::UnsupportedModel(model) => write!(f, "模型不支持: {}", model),
            ConversionError::EmptyMessages => write!(f, "消息列表为空"),
            ConversionError::UnsupportedToolMapping { tool_name, reason } => {
                write!(f, "工具参数无法映射: {tool_name} — {reason}")
            }
        }
    }
}

impl std::error::Error for ConversionError {}

/// 从 metadata.user_id 中提取 session UUID
///
/// 支持两种格式:
/// 1. 字符串格式: user_xxx_account__session_0b4445e1-f5be-49e1-87ce-62bbc28ad705
/// 2. JSON 格式: {"device_id":"...","account_uuid":"...","session_id":"UUID"}
///
/// 从 conversationId 确定性派生 agentContinuationId（UUID 形状的 SHA256 前 16 字节）。
///
/// 同一会话恒定、跨会话隔离。目的:稳住转发给上游的会话键,让同一会话连续请求能命中
/// 上游 prefix 缓存的 credit 折扣（见调用点说明）。加固定前缀避免与其它 SHA 用途碰撞。
fn derive_agent_continuation_id(conversation_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agent-continuation:");
    hasher.update(conversation_id.as_bytes());
    let r = hasher.finalize();
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        r[0],
        r[1],
        r[2],
        r[3],
        r[4],
        r[5],
        r[6],
        r[7],
        r[8],
        r[9],
        r[10],
        r[11],
        r[12],
        r[13],
        r[14],
        r[15]
    )
}

/// 提取 session UUID 作为 conversationId
fn extract_session_id(user_id: &str) -> Option<String> {
    // 先尝试 JSON 解析
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(user_id) {
        if let Some(session_id) = json.get("session_id").and_then(|v| v.as_str()) {
            if is_valid_uuid(session_id) {
                return Some(session_id.to_string());
            }
        }
    }

    // 回退到字符串格式: 查找 "session_" 后面的内容
    if let Some(pos) = user_id.find("session_") {
        let session_part = &user_id[pos + 8..]; // "session_" 长度为 8
        // 安全：用 get(..36) 而非 &session_part[..36]。后者按定长字节切片，
        // 若第 36 字节落在多字节 UTF-8 字符中间（客户端可控的 metadata.user_id
        // 如 "session_"+34字符+"中"），会 str range-index panic 打崩请求任务。
        // get(..36) 在非字符边界处返回 None，自然回退到随机 conversationId。
        if let Some(uuid_str) = session_part.get(..36) {
            if is_valid_uuid(uuid_str) {
                return Some(uuid_str.to_string());
            }
        }
    }
    None
}

/// 简单验证 UUID 格式（36 字符，包含 4 个连字符）
fn is_valid_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4
}

/// 首条消息参与派生键的文本上限（Unicode 标量个数；截断走 [`truncate_chars`]）。
const FIRST_MESSAGE_SEED_MAX_CHARS: usize = 4096;

/// 首条消息里用于派生键的可见文本：string，或数组里顶层 `type=text` 块。
/// 忽略 image / document / 其它块；不 `Display` 整个 Value（base64 可达数 MB）。
/// 累计到 `max_chars` 个 Unicode 标量即停，截断走 [`truncate_chars`]（UTF-8 边界）。
fn first_message_text_for_hash(content: &serde_json::Value, max_chars: usize) -> String {
    match content {
        serde_json::Value::String(s) => truncate_chars(s, max_chars),
        serde_json::Value::Array(arr) => {
            let mut out = String::new();
            let mut remaining = max_chars;
            for item in arr {
                if remaining == 0 {
                    break;
                }
                let Some(obj) = item.as_object() else {
                    continue;
                };
                let is_text_block = match obj.get("type") {
                    Some(serde_json::Value::String(t)) => t == "text",
                    None => obj.contains_key("text"),
                    _ => false,
                };
                if !is_text_block {
                    continue;
                }
                let Some(text) = obj.get("text").and_then(|v| v.as_str()) else {
                    continue;
                };
                let piece = truncate_chars(text, remaining);
                remaining = remaining.saturating_sub(piece.chars().count());
                out.push_str(&piece);
            }
            out
        }
        _ => String::new(),
    }
}

/// 无 `metadata.user_id` 时，从「工作上下文」派生稳定 conversationId。
///
/// # 为什么需要
///
/// 只有 Claude Code 会发 `metadata.user_id`（内含 session UUID）。`python` / `curl` /
/// `opencode` 等客户端不发，旧实现回落到 `Uuid::new_v4()` —— **每个请求都是全新会话键，
/// 于是同一工作上下文的连续请求永远拿不到上游 prefix 缓存的 credit 折扣**。
///
/// 2026-08-04 实测（08-03 全天 40,634 请求）：15,776 个「单轮会话」占 38.8% 的请求，
/// 其中 `unknown`/`python`/`curl`/`opencode` 客户端 15,614 个。这批请求输入中位
/// 173,482 token、p90 657,907 —— **不是小请求**，而是永久零命中的大请求。
///
/// # 派生输入的选择
///
/// 用 `system` 文本 + 排序后的工具名集合 + **首条消息的可见文本**，system 走与请求
/// 路径同一套归一化：
///
/// - **system 走 [`canonicalize_system_text`]** —— 它已剥掉每请求漂移的段（`<env>` 块、
///   `gitStatus:`、`# Environment` 等）。不复用它就会让工作目录或日期的变化把键打散，
///   等于没修。
/// - **工具名排序** —— 官方自认造过「工具排序非确定」的事故；不排序则同一上下文因工具
///   顺序抖动而分裂成多个键。
/// - **只吃下标 0 那条的 role + 顶层 text** —— 同一客户端跨会话的 system/tools 往往
///   恒定，只哈希那两项会把无关会话折叠成一个键（k2cc 08-19：6 会话钉在 3 个号上
///   轮转）。首条是「同会话跨轮不变、异会话通常不同」的成分：客户端每轮重发完整历史，
///   下标 0 保持原样。不能纳入全部 messages，否则每轮新键。image / document / base64
///   不进哈希（禁止把整个 content Display 进 hasher）。累计文本上限见
///   [`FIRST_MESSAGE_SEED_MAX_CHARS`]。
///
/// 加固定前缀 `derived-conversation:` 避免与 [`derive_agent_continuation_id`] 的哈希
/// 用途碰撞。返回 UUID 形状是因为下游 `derive_agent_continuation_id` 与上游都按 UUID
/// 形状消费该字段。段与段之间用 `\x1f`，避免拼接歧义。
///
/// # 边界
///
/// system 与 tools 双双为空时返回 `None`（**先判空，再碰首条消息**），让调用方回落到
/// 随机 UUID —— 裸 curl 不应仅凭第一行文本绑死到同一号。那种请求没有可稳定的前缀
/// 可言，强行归到同一个键只会让无关请求互相污染上游会话。
///
/// # 多租户：为何跨用户撞键是安全的
///
/// 不同用户若 system + tools + 首条可见文本完全相同，会派生出同一个 conversationId。
/// **这不会串话**：[`ConversationState`] 每次请求都携带完整 `history`（由
/// [`build_history`] 现场构建），上游不靠 `continuationId` 重建历史。撞键的后果仅是
/// 两人共用一个上游会话键，而前缀字节不同 → 缓存未命中，退化到修复前的状态，不会
/// 读到对方的内容。
///
/// 因此没有按用户加盐。要加盐就得给 [`convert_request`] 传租户标识，那会改动全部调用点，
/// 而换来的只是「本来就不会发生的泄漏」不发生 —— 不值得。若将来上游改为按
/// `continuationId` 服务端保存历史，此处必须立刻改为加盐。
fn derive_conversation_id_from_context(req: &MessagesRequest) -> Option<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"derived-conversation:");

    let mut has_material = false;

    if let Some(system) = req.system.as_deref() {
        for msg in system {
            let canonical = canonicalize_system_text(&msg.text);
            if !canonical.trim().is_empty() {
                has_material = true;
                hasher.update(canonical.as_bytes());
                // 分隔符防止拼接歧义（["ab","c"] 与 ["a","bc"] 必须不同键）
                hasher.update(b"\x1f");
            }
        }
    }

    if let Some(tools) = req.tools.as_deref() {
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        for name in names {
            has_material = true;
            hasher.update(name.as_bytes());
            hasher.update(b"\x1f");
        }
    }

    if !has_material {
        return None;
    }

    if let Some(first) = req.messages.first() {
        hasher.update(first.role.as_bytes());
        hasher.update(b"\x1f");
        let seed = first_message_text_for_hash(&first.content, FIRST_MESSAGE_SEED_MAX_CHARS);
        hasher.update(seed.as_bytes());
        hasher.update(b"\x1f");
    }

    let r = hasher.finalize();
    Some(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        r[0],
        r[1],
        r[2],
        r[3],
        r[4],
        r[5],
        r[6],
        r[7],
        r[8],
        r[9],
        r[10],
        r[11],
        r[12],
        r[13],
        r[14],
        r[15]
    ))
}

/// 收集历史消息中使用的所有工具名称
fn collect_history_tool_names(history: &[Message]) -> Vec<String> {
    let mut tool_names = Vec::new();

    for msg in history {
        if let Message::Assistant(assistant_msg) = msg {
            if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                for tool_use in tool_uses {
                    if !tool_names.contains(&tool_use.name) {
                        tool_names.push(tool_use.name.clone());
                    }
                }
            }
        }
    }

    tool_names
}

/// 为历史中使用但不在 tools 列表中的工具创建占位符定义
/// Kiro API 要求：历史消息中引用的工具必须在 currentMessage.tools 中有定义
fn create_placeholder_tool(name: &str) -> Tool {
    Tool {
        tool_specification: ToolSpecification {
            name: name.to_string(),
            description: "Tool used in conversation history".to_string(),
            input_schema: InputSchema::from_json(serde_json::json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": true
            })),
        },
    }
}

/// 将 Anthropic 请求转换为 Kiro 请求
pub fn convert_request(req: &MessagesRequest) -> Result<ConversionResult, ConversionError> {
    // 1. 映射模型
    let model_id = map_model(&req.model)
        .ok_or_else(|| ConversionError::UnsupportedModel(req.model.clone()))?;

    // 2. 检查消息列表
    if req.messages.is_empty() {
        return Err(ConversionError::EmptyMessages);
    }

    // 2.5. 预处理 prefill：如果末尾是 assistant，静默丢弃并截断到最后一条 user
    // Claude 4.x 已弃用 assistant prefill，Kiro API 也不支持
    let messages: &[_] = if req.messages.last().is_some_and(|m| m.role != "user") {
        tracing::info!("检测到末尾 assistant 消息（prefill），静默丢弃");
        let last_user_idx = req
            .messages
            .iter()
            .rposition(|m| m.role == "user")
            .ok_or(ConversionError::EmptyMessages)?;
        &req.messages[..=last_user_idx]
    } else {
        &req.messages
    };

    // 3. 生成会话 ID 和代理 ID
    // 优先从 metadata.user_id 中提取 session UUID 作为 conversationId
    // 三级回落：客户端显式 session_id → 工作上下文派生（system + 排序工具名 +
    // messages[0] 可见文本）→ 随机。中间这级是 2026-08-04 新增（L0-5）；08-20 把
    // 首条可见文本纳入 seed，避免同 system/tools 的多会话折叠。见
    // `derive_conversation_id_from_context`。
    let conversation_id = req
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_ref())
        .and_then(|user_id| extract_session_id(user_id))
        .or_else(|| derive_conversation_id_from_context(req))
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    // agentContinuationId 从 conversationId 确定性派生（SHA256），而非每请求随机。
    //
    // 实测(2026-07-07 Phase0):同一大 prompt 前缀一致时上游 credit 折扣约 47%
    // （meteringEvent credits 0.141→0.075）。而每请求随机的 continuationId 若进入上游
    // 会话/前缀键,会让同一会话的连续请求无法命中上游 prefix 缓存,白白丢掉这份折扣。
    // 改为按 conversationId 确定性派生:同一会话稳定复用、跨会话仍相互隔离。
    // 参考 TsinHzl/kiro2cc-proxy derive_agent_continuation_id（MIT）。
    let agent_continuation_id = derive_agent_continuation_id(&conversation_id);

    // 4. 确定触发类型
    let chat_trigger_type = determine_chat_trigger_type(req);

    // 5. 处理最后一条消息作为 current_message（经过 prefill 预处理，末尾必为 user）
    let last_message = messages.last().unwrap();
    let (text_content, images, tool_results) = process_message_content(&last_message.content)?;

    // 6. 转换工具定义（超长名称自动缩短并记录映射）
    let mut tool_name_map = HashMap::new();
    let mut tools = convert_tools(&req.tools, &mut tool_name_map);

    // 7. 构建历史消息（需要先构建，以便收集历史中使用的工具）
    let mut history = build_history(req, messages, &model_id, &mut tool_name_map)?;

    // 8. 验证并过滤 tool_use/tool_result 配对
    // 移除孤立的 tool_result（没有对应的 tool_use）
    // 同时返回孤立的 tool_use_id 集合，用于后续清理
    let (validated_tool_results, orphaned_tool_use_ids) =
        validate_tool_pairing(&history, &tool_results);

    // 9. 从历史中移除孤立的 tool_use（Kiro API 要求 tool_use 必须有对应的 tool_result）
    remove_orphaned_tool_uses(&mut history, &orphaned_tool_use_ids);

    // 10. 收集历史中使用的工具名称，为缺失的工具生成占位符定义
    // Kiro API 要求：历史消息中引用的工具必须在 tools 列表中有定义
    // 注意：Kiro 匹配工具名称时忽略大小写，所以这里也需要忽略大小写比较
    let history_tool_names = collect_history_tool_names(&history);
    let existing_tool_names: std::collections::HashSet<_> = tools
        .iter()
        .map(|t| t.tool_specification.name.to_lowercase())
        .collect();

    for tool_name in history_tool_names {
        if !existing_tool_names.contains(&tool_name.to_lowercase()) {
            tools.push(create_placeholder_tool(&tool_name));
        }
    }

    // 工具名硬护栏集合 = 发给模型的工具名（含缩短后的短名 + 历史占位工具名）。模型文本化 invoke 时
    // 吐的就是它看到的这些名字,重组时据此校验,杜绝把正文里讨论的假命令误重组成 tool_use 执行。
    // 必须在 tools 被 move 进 context 之前收集。
    let known_tool_names: std::collections::HashSet<String> = tools
        .iter()
        .map(|t| t.tool_specification.name.clone())
        .collect();

    // 每个工具的**必需参数名**（Bug C 校验用）。与 `known_tool_names` 同处提取、
    // 同口径（key 是**发给模型的名字**，含 `map_tool_name` 缩短后的短名），
    // 这样流式层校验时不必再做名字还原。
    //
    // Bug C = `tool_use` 参数 **JSON 完全合法但缺必需字段**（如 `Bash` 只给了
    // `description` 没给 `command`）。它既不是 Bug A（JSON 语法坏，`tool_repair_json` 能修）
    // 也不是 Bug B（连 tool_use 块都没吐，网关碰不到），此前落在两者之间的盲区：
    // 客户端拿到合法 JSON 后按 schema 校验失败，报 `The required parameter 'X' is missing`。
    //
    // 只取 `required` 的**名字列表**，不做完整 JSON Schema 校验 —— 后者是过度设计，
    // 且类型不匹配的容错空间远大于"字段整个缺失"。
    // 无 `required` / 非数组 / 空数组的工具不入表（那类工具没有必需参数，无从校验）。
    let tool_required_fields: HashMap<String, Vec<String>> = tools
        .iter()
        .filter_map(|t| {
            let req = t.tool_specification.input_schema.json.get("required")?;
            let names: Vec<String> = req
                .as_array()?
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            if names.is_empty() {
                return None;
            }
            Some((t.tool_specification.name.clone(), names))
        })
        .collect();

    // 11. 构建 UserInputMessageContext
    let mut context = UserInputMessageContext::new();
    if !tools.is_empty() {
        context = context.with_tools(tools);
    }
    if !validated_tool_results.is_empty() {
        context = context.with_tool_results(validated_tool_results);
    }

    // 12. 构建当前消息
    // 保留文本内容，即使有工具结果也不丢弃用户文本
    let content = text_content;

    let mut user_input = UserInputMessage::new(content, &model_id)
        .with_context(context)
        .with_origin("AI_EDITOR");

    if !images.is_empty() {
        user_input = user_input.with_images(images);
    }

    let current_message = CurrentMessage::new(user_input);

    // 13. 构建 ConversationState
    let conversation_state = ConversationState::new(conversation_id)
        .with_agent_continuation_id(agent_continuation_id)
        .with_agent_task_type("vibe")
        .with_chat_trigger_type(chat_trigger_type)
        .with_current_message(current_message)
        .with_history(history);

    if !tool_name_map.is_empty() {
        tracing::info!("工具名称映射: {} 个超长名称已缩短", tool_name_map.len());
    }

    // 14. native effort：开关开 + 白名单模型 + thinking 启用时，把 effort 装进请求级
    // `additionalModelRequestFields.output_config.effort`（Kiro 原生 reasoning 通道）。
    // 与 build_history 里的 XML 抑制共用同一判定（build_additional_model_request_fields
    // 见 generate_thinking_prefix 上方的说明），两者不会分叉。
    let additional_model_request_fields = build_additional_model_request_fields(req, &model_id);

    Ok(ConversionResult {
        conversation_state,
        tool_name_map,
        known_tool_names,
        tool_required_fields,
        additional_model_request_fields,
    })
}

/// 确定聊天触发类型
/// "AUTO" 模式可能会导致 400 Bad Request 错误
fn determine_chat_trigger_type(_req: &MessagesRequest) -> String {
    "MANUAL".to_string()
}

/// 历史图片去重上限。
///
/// 历史中被"上浮"到顶层 images 的图片总数上限，防止多轮对话里累积的截图 base64
/// 把请求体撑爆（与"400 Input too long"防护对齐）。仅约束历史去重路径，当前轮图片
/// （dedup 为 None）不受此限、永远保留。
const MAX_TOTAL_IMAGES: usize = 20;

/// 处理消息内容，提取文本、图片和工具结果
fn process_message_content(
    content: &serde_json::Value,
) -> Result<(String, Vec<KiroImage>, Vec<ToolResult>), ConversionError> {
    // 当前轮消息不做去重（dedup 为 None），本轮所有图片永远保留
    process_message_content_dedup(content, None)
}

/// 与 [`process_message_content`] 相同，但当 `dedup` 为 `Some` 时按 SHA256 对图片去重：
/// 同一张图（base64 完全一致）在历史多轮中反复出现时只在首次保留，之后替换为占位符文本，
/// 避免同一截图跨多轮反复以 base64 重发而烧 token。
fn process_message_content_dedup(
    content: &serde_json::Value,
    mut dedup: Option<&mut std::collections::HashSet<String>>,
) -> Result<(String, Vec<KiroImage>, Vec<ToolResult>), ConversionError> {
    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    let mut tool_results = Vec::new();

    match content {
        serde_json::Value::String(s) => {
            text_parts.push(s.clone());
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                if let Ok(block) = serde_json::from_value::<ContentBlock>(item.clone()) {
                    match block.block_type.as_str() {
                        "text" => {
                            if let Some(text) = block.text {
                                text_parts.push(text);
                            }
                        }
                        "image" => {
                            if let Some(source) = block.source
                                && let Some(placeholder) =
                                    extract_kiro_image(&source, &mut dedup, &mut images)
                            {
                                // 去重/超额命中时补占位符文本，保证图片语义不至于完全消失
                                text_parts.push(placeholder);
                            }
                        }
                        "document" => {
                            // Kiro 不接受 document 块；对 PDF 做轻量文本抽取转成文本块下发，
                            // 抽取失败则留占位说明，避免文档语义完全丢失。
                            if let Some(source) = block.source
                                && source.media_type == "application/pdf"
                            {
                                match extract_pdf_text_from_base64(&source.data) {
                                    Some(text) => text_parts.push(format!(
                                        "<document media_type=\"application/pdf\">\n{}\n</document>",
                                        text
                                    )),
                                    None => text_parts.push(
                                        "[PDF 文档已附加，但文本提取失败]".to_string(),
                                    ),
                                }
                            }
                        }
                        "tool_result" => {
                            if let Some(tool_use_id) = block.tool_use_id {
                                let result_content = extract_tool_result_content(
                                    &block.content,
                                    &mut dedup,
                                    &mut images,
                                );
                                let is_error = block.is_error.unwrap_or(false);

                                let mut result = if is_error {
                                    ToolResult::error(&tool_use_id, result_content)
                                } else {
                                    ToolResult::success(&tool_use_id, result_content)
                                };
                                result.status =
                                    Some(if is_error { "error" } else { "success" }.to_string());

                                tool_results.push(result);
                            }
                        }
                        "tool_use" => {
                            // tool_use 在 assistant 消息中处理，这里忽略
                        }
                        // Claude Code ToolSearch 延迟加载占位块（tool_name 字段，无内容）：
                        // 客户端本地延迟加载用，转发上游无意义 —— 静默跳过，不报错。
                        "tool_reference" => {}
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    Ok((text_parts.join("\n"), images, tool_results))
}

/// 从 media_type 获取图片格式
fn get_image_format(media_type: &str) -> Option<String> {
    match media_type {
        "image/jpeg" => Some("jpeg".to_string()),
        "image/png" => Some("png".to_string()),
        "image/gif" => Some("gif".to_string()),
        "image/webp" => Some("webp".to_string()),
        _ => None,
    }
}

/// 嗅探图片 magic bytes 所需的字节数。
///
/// webp 判据最长：`RIFF`(4) + 文件长度(4) + `WEBP`(4) = 12 字节，其余格式都更短。
const IMAGE_MAGIC_PROBE_BYTES: usize = 12;

/// 只解码 base64 头部若干字节，够判类型即止。
///
/// 图片动辄几百 KB，为判类型解整张图纯属浪费。base64 每 4 字符对应 3 字节，故取
/// 前 16 个有效字符（=12 字节，正好覆盖 webp 判据）单独解码——切在 4 的整数倍上，
/// 无需补 padding。客户端偶尔发 `data:image/png;base64,` 前缀或带换行的 base64，
/// 这里一并剥掉，否则解码直接失败、退化成"认不出"。
fn decode_base64_head(data: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let payload = data.rsplit_once(',').map(|(_, tail)| tail).unwrap_or(data);
    // 过滤空白（换行/缩进）后再截取，避免把有效字符数算少
    let head: Vec<u8> = payload
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .take(IMAGE_MAGIC_PROBE_BYTES.div_ceil(3) * 4)
        .collect();
    // 不足 4 字符无法解出任何完整字节；非 4 倍数说明整张图本就极短，交给下游按原样处理
    if head.len() < 4 || head.len() % 4 != 0 {
        return None;
    }
    base64::engine::general_purpose::STANDARD.decode(&head).ok()
}

/// 按 magic bytes 判断图片真实格式，认不出返回 None（不猜）。
fn sniff_image_format(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8]) {
        return Some("jpeg");
    }
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        return Some("png");
    }
    if bytes.starts_with(b"GIF8") {
        return Some("gif");
    }
    // RIFF 容器还装 wav/avi，必须连偏移 8 处的 `WEBP` 一起验
    if bytes.starts_with(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    None
}

/// 定出下发给上游的图片格式：**magic bytes 优先于客户端声明的 media_type**。
///
/// 客户端（尤其截图工具链）经常声明 `image/png` 而实际字节是 jpeg，上游据此回
/// `ValidationException` 400。实测这类 400 有 49 次，纯本地可修。
///
/// 顺序是本函数的全部意义：先嗅探、认出就用嗅探结果覆盖声明值；只有**认不出**
/// （不匹配任何 magic）才回退到声明值——不猜，否则会把上游本来能接受的格式改坏。
fn resolve_image_format(source: &ImageSource) -> Option<String> {
    let declared = get_image_format(&source.media_type);
    let sniffed = decode_base64_head(&source.data)
        .as_deref()
        .and_then(sniff_image_format);

    match sniffed {
        Some(actual) => {
            if declared.as_deref() != Some(actual) {
                tracing::debug!(
                    declared = %source.media_type,
                    actual = %actual,
                    "图片 media_type 与 magic bytes 不符，按实际字节纠正"
                );
            }
            Some(actual.to_string())
        }
        None => declared,
    }
}

/// 把一个 image 块的 source 转成 `KiroImage` 并上浮到顶层 `images`。
///
/// tool_result 内的图片与顶层 image 块走同一条转换链（mime 校验 + SHA256 去重 + 上浮），
/// 因为上游 Kiro 的 `ToolResult` 没有 image 字段，图片只能走 `userInputMessage.images[]`
/// 顶层通道。
///
/// 返回值：
/// - `Some(placeholder)`：历史去重命中、历史图片数超过 [`MAX_TOTAL_IMAGES`] 上限、或图片超过安全上限，图片被省略；
/// - `None`：图片已上浮到 `images`，或格式不支持（无法转换）。
fn extract_kiro_image(
    source: &ImageSource,
    dedup: &mut Option<&mut std::collections::HashSet<String>>,
    images: &mut Vec<KiroImage>,
) -> Option<String> {
    // 单图 base64 大小上限（硬安全网，非降采样目标）：AWS Q / Kiro 上游有 per-field 大小硬限制，
    // 8MiB 是远超大图的阈值；超限省略 + 占位符（不 fail 请求，与 MAX_TOTAL_IMAGES 同风格）。
    // 8MiB 内的超大图（如 4K 截图常超 1MiB）不再省略，走下方 maybe_shrink_image 降采样后不丢图。
    const MAX_SINGLE_IMAGE_BASE64_BYTES: usize = 8 * 1024 * 1024;
    if source.data.len() > MAX_SINGLE_IMAGE_BASE64_BYTES {
        tracing::warn!(
            bytes = source.data.len(),
            "单图 base64 超过 {} 字节，省略该图（防上游 per-field 大小限制）",
            MAX_SINGLE_IMAGE_BASE64_BYTES
        );
        return Some("[image omitted: exceeds single-image size limit]".to_string());
    }

    // 格式以 magic bytes 为准、声明值兜底；都定不出时无声跳过（与旧行为一致，不补占位符）
    let format = resolve_image_format(source)?;

    // 历史去重：只在 dedup 为 Some（历史路径）时生效，当前轮图片永远保留
    if let Some(seen) = dedup.as_deref_mut() {
        let mut hasher = Sha256::new();
        hasher.update(source.data.as_bytes());
        let digest = format!("{:x}", hasher.finalize());

        if !seen.insert(digest) {
            // 已见过同一张图：省略 base64，返回去重占位符
            return Some("[image omitted: identical to an earlier screenshot]".to_string());
        }
        // 首次见到但已超过历史图片配额：撤销刚插入的指纹，返回超额占位符
        if seen.len() > MAX_TOTAL_IMAGES {
            return Some("[image omitted: too many images in history]".to_string());
        }
    }

    // 智能降采样（GreyGunG image_resize 移植）：8MiB 内的图先尝试缩小到上游可接受的长边
    // /字节预算（PNG/WebP/JPEG 一律重编码 JPEG，GIF 保留原格式防丢动画）。小图零开销直通。
    let cfg = ResizeConfig::from_env();
    // 图片解码+缩放+编码是同步 CPU 重活（几十~几百 ms），在 tokio worker 上同步跑会独占
    // 该 worker 的服务能力。本转换链（convert_request）是同步签名（handlers/websearch 的
    // 调用点不改动），故在这里用 block_in_place + block_on：worker 线程等待期间让位给
    // 其他任务，重活实际执行在 spawn_blocking 的 blocking 池（blocking 区可合法 block_on）。
    let processed = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(maybe_shrink_image_blocking(
            cfg,
            format.clone(),
            source.data.clone(),
        ))
    });
    match processed {
        Ok(processed) => {
            images.push(KiroImage::from_base64(processed.format, processed.data_base64));
        }
        Err(ResizeError::LimitExceeded(_)) => {
            // 安全上限（base64/解码字节/像素/GIF 超预算）触发：维持旧行为"省略 + 占位符"，
            // 不 fail 请求——把超限 payload 直接透传上游比省略更危险。
            tracing::warn!(
                bytes = source.data.len(),
                format = %format,
                "单图超过安全上限，省略该图（防上游 per-field 大小限制）"
            );
            return Some("[image omitted: exceeds image safety limits]".to_string());
        }
        Err(e) => {
            // 解码/编码失败：保留原图透传（坏图不该让整请求失败），警告留痕。
            tracing::warn!(
                error = %e,
                bytes = source.data.len(),
                format = %format,
                "图片降采样失败，保留原图透传"
            );
            images.push(KiroImage::from_base64(format, source.data.clone()));
        }
    }
    None
}

/// 提取工具结果内容
///
/// 文本元素保留为 tool_result 占位符文本；`type=="image"` 的块被提取成 `KiroImage`
/// 并上浮到顶层 `images`（上游 `ToolResult` 无 image 字段，图片只能走顶层通道）。
/// 若某个 tool_result 只有图片没有文本，则用占位符文本 "[image attached]"。
fn extract_tool_result_content(
    content: &Option<serde_json::Value>,
    dedup: &mut Option<&mut std::collections::HashSet<String>>,
    images: &mut Vec<KiroImage>,
) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => {
            let mut parts = Vec::new();
            let mut had_image = false;
            for item in arr {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    parts.push(text.to_string());
                } else if item.get("type").and_then(|v| v.as_str()) == Some("image")
                    && let Ok(block) = serde_json::from_value::<ContentBlock>(item.clone())
                    && let Some(source) = block.source
                {
                    had_image = true;
                    if let Some(placeholder) = extract_kiro_image(&source, dedup, images) {
                        parts.push(placeholder);
                    }
                }
            }
            if parts.is_empty() && had_image {
                "[image attached]".to_string()
            } else {
                parts.join("\n")
            }
        }
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

/// 验证并过滤 tool_use/tool_result 配对
///
/// 收集所有 tool_use_id，验证 tool_result 是否匹配
/// 静默跳过孤立的 tool_use 和 tool_result，输出警告日志
///
/// # Arguments
/// * `history` - 历史消息引用
/// * `tool_results` - 当前消息中的 tool_result 列表
///
/// # Returns
/// 元组：(经过验证和过滤后的 tool_result 列表, 孤立的 tool_use_id 集合)
fn validate_tool_pairing(
    history: &[Message],
    tool_results: &[ToolResult],
) -> (Vec<ToolResult>, std::collections::HashSet<String>) {
    use std::collections::HashSet;

    // 1. 收集所有历史中的 tool_use_id
    let mut all_tool_use_ids: HashSet<String> = HashSet::new();
    // 2. 收集历史中已经有 tool_result 的 tool_use_id
    let mut history_tool_result_ids: HashSet<String> = HashSet::new();

    for msg in history {
        match msg {
            Message::Assistant(assistant_msg) => {
                if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                    for tool_use in tool_uses {
                        all_tool_use_ids.insert(tool_use.tool_use_id.clone());
                    }
                }
            }
            Message::User(user_msg) => {
                // 收集历史 user 消息中的 tool_results
                for result in &user_msg
                    .user_input_message
                    .user_input_message_context
                    .tool_results
                {
                    history_tool_result_ids.insert(result.tool_use_id.clone());
                }
            }
        }
    }

    // 3. 计算真正未配对的 tool_use_ids（排除历史中已配对的）
    let mut unpaired_tool_use_ids: HashSet<String> = all_tool_use_ids
        .difference(&history_tool_result_ids)
        .cloned()
        .collect();

    // 4. 过滤并验证当前消息的 tool_results
    let mut filtered_results = Vec::new();

    for result in tool_results {
        if unpaired_tool_use_ids.contains(&result.tool_use_id) {
            // 配对成功
            filtered_results.push(result.clone());
            unpaired_tool_use_ids.remove(&result.tool_use_id);
        } else if all_tool_use_ids.contains(&result.tool_use_id) {
            // tool_use 存在但已经在历史中配对过了，这是重复的 tool_result
            tracing::warn!(
                "跳过重复的 tool_result：该 tool_use 已在历史中配对，tool_use_id={}",
                result.tool_use_id
            );
        } else {
            // 孤立 tool_result - 找不到对应的 tool_use
            tracing::warn!(
                "跳过孤立的 tool_result：找不到对应的 tool_use，tool_use_id={}",
                result.tool_use_id
            );
        }
    }

    // 5. 检测真正孤立的 tool_use（有 tool_use 但在历史和当前消息中都没有 tool_result）
    for orphaned_id in &unpaired_tool_use_ids {
        tracing::warn!(
            "检测到孤立的 tool_use：找不到对应的 tool_result，将从历史中移除，tool_use_id={}",
            orphaned_id
        );
    }

    (filtered_results, unpaired_tool_use_ids)
}

/// 从历史消息中移除孤立的 tool_use
///
/// Kiro API 要求每个 tool_use 必须有对应的 tool_result，否则返回 400 Bad Request。
/// 此函数遍历历史中的 assistant 消息，移除没有对应 tool_result 的 tool_use。
///
/// # Arguments
/// * `history` - 可变的历史消息列表
/// * `orphaned_ids` - 需要移除的孤立 tool_use_id 集合
fn remove_orphaned_tool_uses(
    history: &mut [Message],
    orphaned_ids: &std::collections::HashSet<String>,
) {
    if orphaned_ids.is_empty() {
        return;
    }

    for msg in history.iter_mut() {
        if let Message::Assistant(assistant_msg) = msg {
            if let Some(ref mut tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                let original_len = tool_uses.len();
                tool_uses.retain(|tu| !orphaned_ids.contains(&tu.tool_use_id));

                // 如果移除后为空，设置为 None
                if tool_uses.is_empty() {
                    assistant_msg.assistant_response_message.tool_uses = None;
                } else if tool_uses.len() != original_len {
                    tracing::debug!(
                        "从 assistant 消息中移除了 {} 个孤立的 tool_use",
                        original_len - tool_uses.len()
                    );
                }
            }
        }
    }
}


/// 生成thinking标签前缀
fn generate_thinking_prefix(req: &MessagesRequest) -> Option<String> {
    if let Some(t) = &req.thinking {
        if t.thinking_type == "enabled" {
            return Some(format!(
                "<thinking_mode>enabled</thinking_mode><max_thinking_length>{}</max_thinking_length>",
                t.budget_tokens
            ));
        } else if t.thinking_type == "adaptive" {
            let effort = req
                .output_config
                .as_ref()
                .map(|c| c.effort.as_str())
                .unwrap_or("high");
            return Some(format!(
                "<thinking_mode>adaptive</thinking_mode><thinking_effort>{}</thinking_effort>",
                effort
            ));
        }
    }
    None
}

/// —— native effort 路径 ——
///
/// 背景：本仓此前 Opus/Sonnet 的 extended thinking 只靠 `<thinking_mode>` XML 标签注入。
/// 参考仓（GreyGunG/Kiro-RS-Tool @795b9ca，2026-06-07 对 Kiro CLI 2.6.0 + Opus 4.8/xHigh
/// 的黑盒实测）结论：**只有请求级 `additionalModelRequestFields.output_config.effort`
/// 能触发上游 `reasoningContentEvent`**；XML 标签既不触发，还会把 `thinking_mode` /
/// `max_thinking_length` 塞进历史上下文（污染 + 烧 token）。本段把该机制移植过来。
///
/// 保守边界：白名单是参考仓**单次实测的硬编码推测值**，按本仓 `model_catalog` 校准——
/// 只放 catalog 里存在、且参考仓实测过的 4 个 kiro_id；未实测的模型（opus-5 / sonnet-5 /
/// 4.5 / 4.0 等）一律不进白名单，宁可回退 XML 注入。白名单与 catalog 的一致性由守卫测试
/// `native_effort_whitelist_models_exist_in_catalog` 钉死。
///
/// 开关 `native_thinking_effort_enabled` 默认 false ⇒ 本路径整体不生效，请求字节与旧版
/// 完全一致；开启后仅命中白名单模型 + thinking 启用时改写。
const EFFORTS_WITH_XHIGH: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const EFFORTS_WITHOUT_XHIGH: &[&str] = &["low", "medium", "high", "max"];

/// 模型 → 白名单允许的 effort 档位表（key 是 `map_model` 映射后的 Kiro modelId）。
///
/// - `claude-opus-4.8` / `claude-opus-4.7`：实测认 `output_config` + 五档（含 xhigh）；
/// - `claude-opus-4.6` / `claude-sonnet-4.6`：同一 schema 路径，但**无 xhigh** 档。
fn native_reasoning_efforts(model_id: &str) -> Option<&'static [&'static str]> {
    match model_id {
        "claude-opus-4.8" | "claude-opus-4.7" => Some(EFFORTS_WITH_XHIGH),
        "claude-opus-4.6" | "claude-sonnet-4.6" => Some(EFFORTS_WITHOUT_XHIGH),
        _ => None,
    }
}

/// 请求是否真的想要 reasoning（thinking 启用，或显式给了非空 effort）。
fn requested_native_reasoning(req: &MessagesRequest) -> bool {
    req.thinking.as_ref().is_some_and(|t| t.is_enabled())
        || req
            .output_config
            .as_ref()
            .is_some_and(|oc| !oc.effort.trim().is_empty())
}

/// budget_tokens → effort 档位（参考仓同款映射表）。
fn effort_from_budget_tokens(tokens: i32) -> &'static str {
    match tokens {
        i32::MIN..=4_000 => "low",
        4_001..=16_000 => "medium",
        16_001..=64_000 => "high",
        _ => "xhigh",
    }
}

/// 归一化 effort：trim + 小写，只认 low/medium/high/xhigh/max，否则回退 "high"。
fn normalize_thinking_effort(effort: &str) -> &'static str {
    match effort.trim().to_ascii_lowercase().as_str() {
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" => "xhigh",
        "max" => "max",
        _ => "high",
    }
}

/// 选档：显式 `output_config.effort` 优先（归一化后），否则 thinking 启用时按
/// budget_tokens 映射，都没给默认 "high"。白名单不允许的档位 → 回退档位表最后一个
/// （五档表最后是 "max"，四档表最后也是 "max"）。
///
/// ⚠️ 空 effort 视同「未给」：与 [`requested_native_reasoning`] 的判空口径一致
/// （那边 `effort.trim().is_empty()` = 没要求），否则 enabled thinking + 大 budget
/// 时客户端空 effort 会被归一化成 high，覆盖掉 budget 映射出的更高档。
fn select_native_reasoning_effort(
    req: &MessagesRequest,
    efforts: &'static [&'static str],
) -> &'static str {
    let requested = req
        .output_config
        .as_ref()
        .filter(|oc| !oc.effort.trim().is_empty())
        .map(|oc| normalize_thinking_effort(&oc.effort))
        .or_else(|| {
            req.thinking.as_ref().map(|t| {
                if t.thinking_type == "enabled" {
                    effort_from_budget_tokens(t.budget_tokens)
                } else {
                    normalize_thinking_effort("")
                }
            })
        })
        .unwrap_or_else(|| normalize_thinking_effort(""));
    if efforts.contains(&requested) {
        requested
    } else {
        efforts.last().copied().unwrap_or("high")
    }
}

/// native effort 总判定：开关 → thinking 未显式禁用 → 白名单 → 选档。
///
/// `build_additional_model_request_fields`（产出请求字段）与 build_history 的 XML 抑制
/// **共用本函数**，同一请求上两处判定恒一致，不会出现「发了字段还塞标签」或
/// 「剥了标签又没发字段」的分叉。
fn native_thinking_effort(req: &MessagesRequest, model_id: &str) -> Option<&'static str> {
    if !native_thinking_effort_enabled() {
        return None;
    }
    if req
        .thinking
        .as_ref()
        .is_some_and(|t| t.thinking_type == "disabled")
    {
        return None;
    }
    let efforts = native_reasoning_efforts(model_id)?;
    if !requested_native_reasoning(req) {
        return None;
    }
    Some(select_native_reasoning_effort(req, efforts))
}

/// 构建请求级 `additionalModelRequestFields`（native effort 通道）。
fn build_additional_model_request_fields(
    req: &MessagesRequest,
    model_id: &str,
) -> Option<AdditionalModelRequestFields> {
    let effort = native_thinking_effort(req, model_id)?;
    Some(AdditionalModelRequestFields {
        output_config: Some(KiroOutputConfig {
            effort: effort.to_string(),
        }),
    })
}

/// native 路径下抑制 XML 标签注入；否则原样走 [`generate_thinking_prefix`]。
///
/// 为什么必须抑制：实测塞 `<thinking_mode>` 标签既不触发上游 reasoningContentEvent，
/// 还会把标签文字污染进历史上下文（参考仓 converter.rs 1451-1455 的实测结论）。
/// 非 native 路径（开关关 / 非白名单）行为逐字节不变。
fn generate_thinking_prefix_for_model(req: &MessagesRequest, model_id: &str) -> Option<String> {
    if native_thinking_effort(req, model_id).is_some() {
        return None;
    }
    generate_thinking_prefix(req)
}

/// 检查内容是否已包含thinking标签
fn has_thinking_tags(content: &str) -> bool {
    content.contains("<thinking_mode>") || content.contains("<max_thinking_length>")
}

/// 构建历史消息
///
/// # Arguments
/// * `req` - 原始请求，用于读取 `system`、`thinking` 等配置字段
/// * `messages` - 经过 prefill 预处理的消息切片，末尾必定是 user 消息。
///   注意：该切片与 `req.messages` 可能不同（prefill 时会截断末尾的 assistant 消息），
///   调用方应始终使用此参数而非 `req.messages`。
/// * `model_id` - 已映射的 Kiro 模型 ID
fn build_history(
    req: &MessagesRequest,
    messages: &[super::types::Message],
    model_id: &str,
    tool_name_map: &mut HashMap<String, String>,
) -> Result<Vec<Message>, ConversionError> {
    let mut history = Vec::new();

    // 生成thinking前缀（如果需要）
    // native effort 路径（开关开 + 白名单 + thinking 启用，见
    // build_additional_model_request_fields 上方的说明）下不注入 XML 标签：
    // 实测只有 `additionalModelRequestFields.output_config.effort` 能触发上游
    // reasoningContentEvent，塞标签既不触发还污染历史上下文。
    let thinking_prefix = generate_thinking_prefix_for_model(req, model_id);

    // 1. 处理系统消息
    //
    // 先把 system 归一化成"有效文本或无"，再统一决策注入内容。
    // 这里刻意不用 `if let Some(system) = .. {} else if let Some(prefix) = ..`：
    // `system` 存在但归一化后为空（如 `"system": ""` 被 types.rs 的 visit_str 变成
    // `Some(vec![{text:""}])`，或整块被环境噪音剥空）时，外层分支已匹配，控制流永远到不了
    // else 分支 → thinking 前缀被静默丢弃，扩展思考不生效且无任何日志。
    let system_content: Option<String> = req.system.as_ref().and_then(|system| {
        // 归一化每一块 system 文本：折叠 CC 归因头（第一块，每请求漂移）+ 剥离环境噪音
        // （<env> 块 / gitStatus / Recent commits / 模型名行等，每请求漂移）。
        // 稳定住转发给上游的 prompt 前缀，避免 Bedrock prefix cache 因这些漂移而 0 命中，
        // 同时省 token、降 CC 身份被关联风险。空块（整块被剥空）直接丢弃不参与拼接。
        let joined: String = system
            .iter()
            .map(|s| canonicalize_system_text(&s.text).into_owned())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if joined.is_empty() {
            None
        } else {
            Some(joined)
        }
    });

    // 最终要写入首条 user 消息的内容：三种情形共用一个出口
    //   ① 有 system 有效文本 → [thinking 前缀 +] system + 分块策略
    //   ② 无 system 有效文本但有 thinking → 仅 thinking 前缀
    //   ③ 两者都无 → 不插入 system 配对
    let system_injection: Option<String> = match (system_content, thinking_prefix.as_ref()) {
        (Some(content), prefix) => {
            // 追加分块写入策略到系统消息
            let content = format!("{}\n{}", content, SYSTEM_CHUNKED_POLICY);
            // 注入thinking标签到系统消息最前面（如果需要且不存在）
            Some(match prefix {
                Some(p) if !has_thinking_tags(&content) => format!("{}\n{}", p, content),
                _ => content,
            })
        }
        (None, Some(prefix)) => {
            // 没有可用系统文本但有 thinking 配置，仅以 thinking 前缀插入系统消息
            Some(prefix.clone())
        }
        (None, None) => None,
    };

    if let Some(final_content) = system_injection {
        // 系统消息作为 user + assistant 配对
        let user_msg = HistoryUserMessage::new(final_content, model_id);
        history.push(Message::User(user_msg));

        let assistant_msg = HistoryAssistantMessage::new("I will follow these instructions.");
        history.push(Message::Assistant(assistant_msg));
    }

    // 2. 处理常规消息历史
    // 最后一条消息作为 currentMessage，不加入历史
    // 经过 prefill 预处理后，messages 末尾必定是 user，故直接截掉最后一条即可
    let history_end_index = messages.len().saturating_sub(1);

    // 收集并配对消息
    let mut user_buffer: Vec<&super::types::Message> = Vec::new();
    let mut assistant_buffer: Vec<&super::types::Message> = Vec::new();
    // 跨整个历史的图片 SHA256 去重集合：同一张图只在首次出现时保留 base64
    let mut image_dedup: std::collections::HashSet<String> = std::collections::HashSet::new();

    for i in 0..history_end_index {
        let msg = &messages[i];

        if msg.role == "user" {
            // 先处理累积的 assistant 消息
            if !assistant_buffer.is_empty() {
                let merged = merge_assistant_messages(&assistant_buffer, tool_name_map)?;
                history.push(Message::Assistant(merged));
                assistant_buffer.clear();
            }
            user_buffer.push(msg);
        } else if msg.role == "assistant" {
            // 先处理累积的 user 消息
            if !user_buffer.is_empty() {
                let merged_user = merge_user_messages(&user_buffer, model_id, &mut image_dedup)?;
                history.push(Message::User(merged_user));
                user_buffer.clear();
            }
            // 累积 assistant 消息（支持连续多条）
            assistant_buffer.push(msg);
        }
    }

    // 处理末尾累积的 assistant 消息
    if !assistant_buffer.is_empty() {
        let merged = merge_assistant_messages(&assistant_buffer, tool_name_map)?;
        history.push(Message::Assistant(merged));
    }

    // 处理结尾的孤立 user 消息
    if !user_buffer.is_empty() {
        let merged_user = merge_user_messages(&user_buffer, model_id, &mut image_dedup)?;
        history.push(Message::User(merged_user));

        // 自动配对一个 "OK" 的 assistant 响应
        let auto_assistant = HistoryAssistantMessage::new("OK");
        history.push(Message::Assistant(auto_assistant));
    }

    Ok(history)
}

/// 合并多个 user 消息
fn merge_user_messages(
    messages: &[&super::types::Message],
    model_id: &str,
    dedup: &mut std::collections::HashSet<String>,
) -> Result<HistoryUserMessage, ConversionError> {
    let mut content_parts = Vec::new();
    let mut all_images = Vec::new();
    let mut all_tool_results = Vec::new();

    for msg in messages {
        let (text, images, tool_results) =
            process_message_content_dedup(&msg.content, Some(dedup))?;
        if !text.is_empty() {
            content_parts.push(text);
        }
        all_images.extend(images);
        all_tool_results.extend(tool_results);
    }

    let content = content_parts.join("\n");
    // 保留文本内容，即使有工具结果也不丢弃用户文本
    let mut user_msg = UserMessage::new(&content, model_id);

    if !all_images.is_empty() {
        user_msg = user_msg.with_images(all_images);
    }

    if !all_tool_results.is_empty() {
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(all_tool_results);
        user_msg = user_msg.with_context(ctx);
    }

    Ok(HistoryUserMessage {
        user_input_message: user_msg,
    })
}

/// 转换 assistant 消息
fn convert_assistant_message(
    msg: &super::types::Message,
    tool_name_map: &mut HashMap<String, String>,
) -> Result<HistoryAssistantMessage, ConversionError> {
    let mut thinking_content = String::new();
    let mut text_content = String::new();
    let mut tool_uses = Vec::new();

    match &msg.content {
        serde_json::Value::String(s) => {
            text_content = s.clone();
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                if let Ok(block) = serde_json::from_value::<ContentBlock>(item.clone()) {
                    match block.block_type.as_str() {
                        "thinking" => {
                            if let Some(thinking) = block.thinking {
                                thinking_content.push_str(&thinking);
                            }
                        }
                        "text" => {
                            if let Some(text) = block.text {
                                text_content.push_str(&text);
                            }
                        }
                        "tool_use" => {
                            if let (Some(id), Some(name)) = (block.id, block.name) {
                                let input = block.input.unwrap_or(serde_json::json!({}));
                                // 历史 tool_use 也走 CC↔Kiro 映射：名（Write→fs_write）+ 参数
                                // （file_path→path、old_string→oldStr 等），否则多轮工具上下文
                                // 与当前轮的工具协议不一致。Read.pages 无 Kiro 等价 → 传播错误。
                                // 开关关闭时回退透传（仅超长缩短）。
                                let map_enabled = tool_compat_mapping_enabled();
                                let mapped_name = if map_enabled {
                                    map_client_tool_name_to_kiro(&name, tool_name_map)
                                } else {
                                    map_tool_name(&name, tool_name_map)
                                };
                                let input = if map_enabled {
                                    map_tool_input_to_kiro(&name, input)?
                                } else {
                                    input
                                };
                                tool_uses
                                    .push(ToolUseEntry::new(id, mapped_name).with_input(input));
                            }
                        }
                        // Claude Code ToolSearch 延迟加载占位块：静默跳过（同 user 消息侧）。
                        "tool_reference" => {}
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    // 组合 thinking 和 text 内容
    // 格式: <thinking>思考内容</thinking>\n\ntext内容
    // 注意: Kiro API 要求 content 字段不能为空，当只有 tool_use 时需要占位符
    let final_content = if !thinking_content.is_empty() {
        if !text_content.is_empty() {
            format!(
                "<thinking>{}</thinking>\n\n{}",
                thinking_content, text_content
            )
        } else {
            format!("<thinking>{}</thinking>", thinking_content)
        }
    } else if text_content.is_empty() && !tool_uses.is_empty() {
        " ".to_string()
    } else {
        text_content
    };

    let mut assistant = AssistantMessage::new(final_content);
    if !tool_uses.is_empty() {
        assistant = assistant.with_tool_uses(tool_uses);
    }

    Ok(HistoryAssistantMessage {
        assistant_response_message: assistant,
    })
}

/// 合并多个连续的 assistant 消息为一条
/// 用于处理网络不稳定时产生的连续 assistant 消息（Issue #79）
fn merge_assistant_messages(
    messages: &[&super::types::Message],
    tool_name_map: &mut HashMap<String, String>,
) -> Result<HistoryAssistantMessage, ConversionError> {
    assert!(!messages.is_empty());
    if messages.len() == 1 {
        return convert_assistant_message(messages[0], tool_name_map);
    }

    let mut all_tool_uses: Vec<ToolUseEntry> = Vec::new();
    let mut content_parts: Vec<String> = Vec::new();

    for msg in messages {
        let converted = convert_assistant_message(msg, tool_name_map)?;
        let am = converted.assistant_response_message;
        if !am.content.trim().is_empty() {
            content_parts.push(am.content);
        }
        if let Some(tus) = am.tool_uses {
            all_tool_uses.extend(tus);
        }
    }

    let content = if content_parts.is_empty() && !all_tool_uses.is_empty() {
        " ".to_string()
    } else {
        content_parts.join("\n\n")
    };

    let mut assistant = AssistantMessage::new(content);
    if !all_tool_uses.is_empty() {
        assistant = assistant.with_tool_uses(all_tool_uses);
    }
    Ok(HistoryAssistantMessage {
        assistant_response_message: assistant,
    })
}

#[cfg(test)]
#[path = "converter_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "converter_native_effort_tests.rs"]
mod native_effort_tests;
#[cfg(test)]
#[path = "converter_byte_overflow_tests.rs"]
mod byte_overflow_tests;
