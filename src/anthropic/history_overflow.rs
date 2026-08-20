//! 请求体超限字节兜底（sanitize + 按字节截断历史）。
//!
//! 对外符号由 `converter.rs` 再导出，保持既有 `converter::…` 路径。

use std::collections::{HashMap, HashSet};

use crate::kiro::model::requests::conversation::{
    ConversationState, HistoryAssistantMessage, HistoryUserMessage, Message,
};
use crate::kiro::model::requests::tool::ToolResult;

// ============ 请求体超限字节兜底（移植 ref-new-jsjm-KiroStudio truncate.rs + sanitize_history.rs） ============
//
// 上游按**字节**拒绝过大请求体（400 CONTENT_LENGTH_EXCEEDS_THRESHOLD），而客户端侧
// 的自动压缩按 **token** 阈值触发——量纲不对齐：长会话字节先撞线时压缩没机会启动。
// 本段移植参考仓的两层兜底，供 `handlers.rs` 的 CONTENT_LENGTH_EXCEEDS 压缩重试路径
// 调用（正常路径仍只走 token 压缩，见 [`apply_byte_overflow_guard`] 的文档）。

/// 序列化后请求体的字节上限。
///
/// 上游按字节拒绝过大请求（参考仓实测 2MB 左右开始拒、1.1MB 仍可通过；我方上游
/// ~5MiB 硬线）。这里取 900KB，与参考仓同值，保守地留出请求头与序列化开销的余量。
pub(crate) const MAX_PAYLOAD_BYTES: usize = 900 * 1024;

/// 截断时始终保留的最近历史条数。
pub(crate) const MIN_RECENT_HISTORY_TURNS: usize = 4;

/// 丢弃旧历史处插入的占位说明。
pub(crate) const TRUNCATION_PLACEHOLDER: &str = "[Earlier conversation history was truncated to fit the model's input limit. Older messages and tool activity have been omitted.]";

/// 估算单条历史的序列化字节数。
fn entry_size(entry: &Message) -> usize {
    serde_json::to_string(entry).map(|s| s.len()).unwrap_or(0)
}

/// 估算整个 `ConversationState` 的序列化字节数。
pub(super) fn state_size(state: &ConversationState) -> usize {
    serde_json::to_string(state).map(|s| s.len()).unwrap_or(0)
}

/// 丢掉开头连续的 assistant 消息，保证历史以 user 开头。
fn drop_leading_assistant(mut tail: Vec<Message>) -> Vec<Message> {
    while matches!(tail.first(), Some(Message::Assistant(_))) {
        tail.remove(0);
    }
    tail
}

/// 剥掉开头那些「引用了已被丢弃的 toolUse」的孤立 toolResults。
///
/// 历史里 assistant 用 `toolUses` 发起调用、随后的 user 用 `toolResults` 回结果，
/// 两者靠 `tool_use_id` 配对。按字节切历史会把配对切断，留下引用不存在 id 的孤立
/// toolResults，上游据此判定 `400 REQUEST_BODY_INVALID`（截断重排序产生孤儿 tool_use
/// 正是 B8 已踩过的 400 形态）。这里从头逐条剥离，直到首条 user 不再含无主的
/// toolResults。只需处理开头：尾部的配对天然完整（切口只在前端）。
pub(super) fn drop_orphan_tool_results(mut tail: Vec<Message>) -> Vec<Message> {
    loop {
        // 收集当前 tail 里所有 assistant 发起过的 tool_use_id。
        let known: std::collections::HashSet<&str> = tail
            .iter()
            .filter_map(|m| match m {
                Message::Assistant(a) => a.assistant_response_message.tool_uses.as_ref(),
                _ => None,
            })
            .flatten()
            .map(|t| t.tool_use_id.as_str())
            .collect();

        let orphan = match tail.first() {
            Some(Message::User(u)) => {
                let results = &u.user_input_message.user_input_message_context.tool_results;
                !results.is_empty()
                    && results.iter().any(|r| !known.contains(r.tool_use_id.as_str()))
            }
            _ => false,
        };

        if !orphan {
            return tail;
        }
        // 丢掉这条孤立 toolResults 的 user，以及紧随其后的 assistant（保持交替）。
        tail.remove(0);
        if matches!(tail.first(), Some(Message::Assistant(_))) {
            tail.remove(0);
        }
        if tail.is_empty() {
            return tail;
        }
    }
}

/// 占位条之后紧跟的 assistant 应答。
///
/// 上游要求历史严格 user/assistant 交替，否则 `400 REQUEST_BODY_INVALID`。
/// 占位本身是一条 user 消息，而 [`drop_leading_assistant`] 又保证 tail 以 user
/// 开头，二者直接相接会形成 user+user。故在中间补一条极短的 assistant 应答。
const PLACEHOLDER_ACK: &str = "Understood.";

/// 若请求体超过 [`MAX_PAYLOAD_BYTES`]，丢弃最旧的历史轮次直至满足上限。
///
/// 返回被丢弃的条数（0 表示未截断）。当前消息本身超限时无能为力——那是单条用户
/// 输入过大，截断历史也救不了，此时返回已丢弃的条数并让请求照常发出（由上游给出
/// 明确错误），而不是静默改写用户的当前输入。
///
/// ⚠️ **前置约束：必须先跑 [`sanitize_history`] 再调用本函数**（生产路径
/// `apply_byte_overflow_guard` 满足）。若不先扁平化，切口落在 toolUse/toolResult
/// 配对中间时，[`drop_orphan_tool_results`] 的连坐删除（孤立 user + 紧随 assistant）
/// 会把**保留段内配对本完整**的下一轮也连锁误删（见 `test_drop_orphan_chain_reaction`，
/// 与参考仓行为一致——参考仓同样靠「sanitize 先行」规避）。sanitize 后历史无残留
/// 结构化工具轮次，该路径成为死代码。
///
/// 保留策略（对齐参考仓 / kiro-go 的 truncatePayloadToLimit）：
/// - 从最新往旧累加，保留能放下的最长后缀，但不少于 [`MIN_RECENT_HISTORY_TURNS`] 条；
/// - 被丢弃处插入一条 [`TRUNCATION_PLACEHOLDER`] user 消息 + 极短 assistant 应答
///   （占位与 tail 之间必须补 assistant，否则 user+user 破坏交替）；
/// - 历史必须以 user 开头，截断后若首条是 assistant 则一并丢弃；
/// - 切口可能切断 toolUse/toolResult 配对，剥掉引用了已丢 toolUse 的孤立 toolResults。
pub(crate) fn truncate_history_if_needed(state: &mut ConversationState, model_id: &str) -> usize {
    if state_size(state) <= MAX_PAYLOAD_BYTES {
        return 0;
    }

    let conversation = std::mem::take(&mut state.history);
    let total = conversation.len();
    if total == 0 {
        return 0;
    }

    let placeholder = Message::User(HistoryUserMessage::new(TRUNCATION_PLACEHOLDER, model_id));

    // 先量出「不含任何历史」的基线大小（含占位条目），再从最新往旧累加。
    let base = state_size(state) + entry_size(&placeholder);

    let sizes: Vec<usize> = conversation.iter().map(entry_size).collect();

    // 保留能放下的最长后缀，但不少于 MIN_RECENT_HISTORY_TURNS 条。
    let mut keep_from = total;
    let mut running = base;
    for i in (0..total).rev() {
        running += sizes[i];
        let kept = total - i;
        if running > MAX_PAYLOAD_BYTES && kept > MIN_RECENT_HISTORY_TURNS {
            break;
        }
        keep_from = i;
    }

    let tail = drop_leading_assistant(conversation[keep_from..].to_vec());
    // 切口可能落在 toolUse/toolResult 之间，留下无主的 toolResults → 上游 400。
    let tail = drop_orphan_tool_results(tail);
    // 上一步可能又暴露出开头的 assistant，再规整一次。
    let tail = drop_leading_assistant(tail);

    let mut rebuilt = Vec::with_capacity(tail.len() + 2);
    if keep_from > 0 {
        // 占位(user) + 应答(assistant)，保持与后续 tail(user 开头) 的严格交替。
        rebuilt.push(placeholder);
        rebuilt.push(Message::Assistant(HistoryAssistantMessage::new(
            PLACEHOLDER_ACK,
        )));
    }
    rebuilt.extend(tail);
    state.history = rebuilt;

    let dropped = keep_from;
    if dropped > 0 {
        tracing::warn!(
            "请求体超过 {} KB，已丢弃最旧 {} 条历史（保留最近 {} 条）并插入占位说明",
            MAX_PAYLOAD_BYTES / 1024,
            dropped,
            total - dropped
        );
    }
    dropped
}

/// 把历史里的结构化工具调用扁平化为文本，只保留活跃轮次。
///
/// 上游只接受**一个活跃工具轮次**：最后一条 history assistant 的 `toolUses` ⟺
/// 当前消息的 `toolResults`。历史里残留多组结构化 toolUses/toolResults 会被判
/// `400 REQUEST_BODY_INVALID`（参考仓 sanitize_history.rs，对齐 kiro-go 的
/// sanitizeKiroHistory）。本函数把历史里除活跃轮次外的所有结构化工具调用叙述为文本：
/// - assistant 的 `toolUses` 直接清空，**不**写入任何「调用了工具 X」的文本
///   （长历史里出现几十个「用文本调用工具」的范例，模型会模仿而不再发结构化调用）；
/// - user 的 `toolResults` 转成 `[工具名] 输出` 形式并入正文（工具身份靠
///   `tool_use_id → name` 映射保留）；
/// - 顺带大幅缩小请求体（结构化 JSON 比纯文本冗余得多），从根上降低截断触发频率。
///
/// `current_tool_result_ids` 是**当前**消息携带的 `tool_use_id` 集合。当历史最后一条
/// 是 assistant 且其 toolUses 被该集合完全覆盖时，这条保持结构化（即活跃轮次）。
/// 部分覆盖（末条 toolUses=[A,B] 而 current 只应答 A）属**畸形输入**：A 被清空后
/// current 的 A 变孤立 result，上游本就判 REQUEST_BODY_INVALID（严格 ⟺ 约束），
/// 本函数不为此防御，错误码从 CONTENT_LENGTH_EXCEEDS 变为 REQUEST_BODY_INVALID 而已。
pub(crate) fn sanitize_history(history: &mut [Message], current_tool_result_ids: &HashSet<String>) {
    if history.is_empty() {
        return;
    }

    // 快速检查：历史里是否有任何工具调用/结果。无则跳过（避免无谓遍历）。
    let has_tools = history.iter().any(|m| match m {
        Message::Assistant(a) => a
            .assistant_response_message
            .tool_uses
            .as_ref()
            .is_some_and(|uses| !uses.is_empty()),
        Message::User(u) => !u
            .user_input_message
            .user_input_message_context
            .tool_results
            .is_empty(),
    });
    if !has_tools {
        return;
    }

    // 先建 tool_use_id → 工具名 的全量映射：即便某轮的 toolUses 被清空，
    // 其结果侧仍能标出来源工具。
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for m in history.iter() {
        if let Message::Assistant(a) = m {
            if let Some(uses) = &a.assistant_response_message.tool_uses {
                for tu in uses {
                    if !tu.tool_use_id.is_empty() && !tu.name.is_empty() {
                        tool_names.insert(tu.tool_use_id.clone(), tu.name.clone());
                    }
                }
            }
        }
    }

    // 判定活跃轮次：最后一条 assistant 的 toolUses 全部被当前 toolResults 应答。
    let active_idx: Option<usize> = if current_tool_result_ids.is_empty() {
        None
    } else {
        let last = history.len() - 1;
        match &history[last] {
            Message::Assistant(a) => match &a.assistant_response_message.tool_uses {
                Some(uses) if !uses.is_empty() => uses
                    .iter()
                    .all(|tu| current_tool_result_ids.contains(&tu.tool_use_id))
                    .then_some(last),
                _ => None,
            },
            _ => None,
        }
    };

    for (i, m) in history.iter_mut().enumerate() {
        match m {
            Message::Assistant(a) => {
                if Some(i) == active_idx {
                    continue; // 活跃轮次保持结构化
                }
                // 清空结构化调用，且不写任何调用叙述（见函数文档反模式）。
                a.assistant_response_message.tool_uses = None;
            }
            Message::User(u) => {
                let ctx = &mut u.user_input_message.user_input_message_context;
                if !ctx.tool_results.is_empty() {
                    let narrated = narrate_tool_results(&ctx.tool_results, &tool_names);
                    if !narrated.is_empty() {
                        let content = &mut u.user_input_message.content;
                        if content.trim().is_empty() {
                            *content = narrated;
                        } else {
                            content.push_str("\n\n");
                            content.push_str(&narrated);
                        }
                    }
                    ctx.tool_results.clear();
                }
                // 历史条目不该携带工具规格（只有当前消息需要）。
                ctx.tools.clear();
            }
        }
    }
}

/// 把 toolResults 叙述成 `[工具名] 输出` 形式的文本。
fn narrate_tool_results(
    results: &[ToolResult],
    names: &HashMap<String, String>,
) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(results.len());
    for r in results {
        let mut texts: Vec<&str> = Vec::new();
        for c in &r.content {
            if let Some(t) = c.get("text").and_then(|v| v.as_str()) {
                if !t.trim().is_empty() {
                    texts.push(t);
                }
            }
        }
        let body = if texts.is_empty() {
            "(no output)".to_string()
        } else {
            texts.join("\n")
        };
        match names.get(&r.tool_use_id) {
            Some(name) if !name.is_empty() => parts.push(format!("[{name}] {body}")),
            _ => parts.push(body),
        }
    }
    parts.join("\n")
}

/// 压缩重试前的字节兜底（只在 400 CONTENT_LENGTH_EXCEEDS 触发压缩重试的路径调用）。
///
/// 与 token 压缩的关系（谁先）：正常路径只走 token 压缩（`compressor` +
/// `adaptive_compress_loop`，保语义）；上游按**字节**拒绝，量纲与 token 不对齐，
/// 一旦 400 CONTENT_LENGTH_EXCEEDS 触发压缩重试，先做字节兜底再走 token 压缩：
/// 1. [`sanitize_history`]：扁平化历史里除活跃轮次外的结构化工具轮次（缩小体积，
///    参考仓顺序 sanitize → truncate，扁平化能降低截断触发频率）；
/// 2. [`truncate_history_if_needed`]：超过 [`MAX_PAYLOAD_BYTES`] 时丢最旧历史 +
///    占位说明 + 保 user/assistant 交替 + 剥孤立 toolResult。
///
/// 幂等：多次调用安全（截断后 size ≤ 上限则 no-op；sanitize 后无工具则早退）。
pub(crate) fn apply_byte_overflow_guard(state: &mut ConversationState) {
    // 当前消息的 toolResults 即活跃轮次的应答集合；先从 state 自身派生，
    // 不依赖调用方从转换层传值。
    let current_tool_result_ids: HashSet<String> = state
        .current_message
        .user_input_message
        .user_input_message_context
        .tool_results
        .iter()
        .map(|r| r.tool_use_id.clone())
        .collect();
    sanitize_history(&mut state.history, &current_tool_result_ids);
    let model_id = state.current_message.user_input_message.model_id.clone();
    truncate_history_if_needed(state, &model_id);
}
