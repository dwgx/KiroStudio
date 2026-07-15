//! OpenAI ↔ Anthropic 双向翻译(chat/completions)。
//!
//! 移植自 CLIProxyAPI `claude/openai/chat-completions/{request,response}.go`(target=claude 即
//! Anthropic 上游,与我们方向一致)+ sub2api apicompat 的踩坑规则。用 serde_json::Value 做字段级
//! 改写(对应 Go 的 gjson/sjson)。所有规则都为防我们上游(CodeWhisperer,吃 Anthropic 格式)400。

use serde_json::{json, Map, Value};

/// 默认 max_tokens(OpenAI 请求可不带,Anthropic 必填)。
const DEFAULT_MAX_TOKENS: i64 = 32000;

/// 生成 n 个十六进制字符(响应 id 用)。
pub fn random_hex(n: usize) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // 轻量随机:时间 + 进程内计数器 + 地址熵,足够做 id(非安全用途)。
    static CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    let c = CTR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut seed = t ^ (c.wrapping_mul(0x9E3779B97F4A7C15)) ^ (&CTR as *const _ as u64);
    let mut out = String::with_capacity(n);
    const HEX: &[u8] = b"0123456789abcdef";
    for _ in 0..n {
        // xorshift64
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.push(HEX[(seed & 0xf) as usize] as char);
    }
    out
}

/// 生成一个 Anthropic 工具调用 id(toolu_<24 随机>)。
fn gen_tool_call_id() -> String {
    format!("toolu_{}", random_hex(24))
}

/// OpenAI temperature ∈ [0,2],Anthropic(及兼容上游)∈ [0,1]。custom_api 透传路径把翻译后的
/// body 原样发给 Anthropic 兼容上游,>1 会 400(Kiro 主路径不吃 temperature,但透传路径吃)。
/// clamp 到 [0,1](2.0→1.0 仍是最高随机性,语义最接近),两条路径都安全。
fn clamp_anthropic_temperature(t: f64) -> f64 {
    t.clamp(0.0, 1.0)
}

/// 把 OpenAI `response_format` 翻成一段 system 指令(Anthropic 无原生 JSON mode,只能提示引导)。
///
/// - `{"type":"text"}` 或缺失 → None(默认,无需注入)。
/// - `{"type":"json_object"}` → 要求只输出合法 JSON。
/// - `{"type":"json_schema","json_schema":{"name","schema","strict"}}` → 附上目标 schema,要求严格遵循。
///
/// ⚠️ 诚实边界:这是**尽力引导**,非上游硬保证(Anthropic 不像 OpenAI 有服务端 JSON 约束/schema
/// 校验)。绝大多数场景足够,但不能承诺 100% 合法/完全贴合 schema。作为 system 块追加(不覆盖用户
/// 自己的 system),两条路径(Kiro 主路径 + custom_api 透传)都安全。
fn response_format_instruction(rf: &Value) -> Option<String> {
    let typ = rf.get("type").and_then(|v| v.as_str())?;
    match typ {
        "json_object" => Some(
            "You must respond with a single valid JSON value only. \
             Do not include any prose, explanation, markdown code fences, or text outside the JSON."
                .to_string(),
        ),
        "json_schema" => {
            // json_schema 既可能是 {json_schema:{schema:{..}}}(chat),也可能扁平在顶层(responses)。
            let js = rf.get("json_schema").unwrap_or(rf);
            let schema = js.get("schema").or_else(|| rf.get("schema"));
            let name = js.get("name").and_then(|v| v.as_str()).unwrap_or("Response");
            let mut instr = format!(
                "You must respond with a single valid JSON value only, with no prose, explanation, \
                 or markdown fences. The JSON must conform to this JSON Schema (named \"{name}\")"
            );
            if let Some(s) = schema {
                instr.push_str(":\n");
                instr.push_str(&s.to_string());
            } else {
                instr.push('.');
            }
            Some(instr)
        }
        // "text" 或未知类型:默认文本,不注入。
        _ => None,
    }
}

/// 把 OpenAI chat/completions 请求 JSON 翻译成 Anthropic MessagesRequest JSON。
///
/// - `model` 用调用方已解析好的模型名(经 model_catalog 归一,GPT-5.6 等已在表)。
/// - `stream` 由调用方决定(与出站是否 SSE 一致)。
/// 返回可直接喂给 `anthropic::handlers::post_messages` 的请求体字节。
pub fn openai_chat_to_anthropic(model: &str, raw: &Value, stream: bool) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    // max_tokens:OpenAI 优先 max_completion_tokens,再 max_tokens,兜底默认。
    let max_tokens = raw
        .get("max_completion_tokens")
        .and_then(|v| v.as_i64())
        .or_else(|| raw.get("max_tokens").and_then(|v| v.as_i64()))
        .unwrap_or(DEFAULT_MAX_TOKENS);
    out.insert("max_tokens".into(), json!(max_tokens));
    out.insert("stream".into(), json!(stream));

    // reasoning_effort → thinking(简化:开 enabled;none→disabled)。上游按 modelId 给窗口,budget 非必需。
    // 先算出 thinking 是否开启,后面决定 temperature/top_p 是否透传(Anthropic thinking 模式只接受
    // temperature=1 且不接受采样参数改写)。
    let mut thinking_enabled = false;
    if let Some(effort) = raw.get("reasoning_effort").and_then(|v| v.as_str()) {
        let e = effort.trim().to_lowercase();
        if e == "none" {
            out.insert("thinking".into(), json!({"type": "disabled"}));
        } else if !e.is_empty() {
            out.insert("thinking".into(), json!({"type": "enabled"}));
            thinking_enabled = true;
        }
    }

    // temperature/top_p:thinking 开启时**都不透传**(Anthropic thinking 模式只接受 temperature=1、
    // 且不接受非默认 top_p/top_k,透传客户端的值会让 Anthropic 兼容上游/透传路径 400)。thinking 关时正常透传。
    if !thinking_enabled {
        if let Some(t) = raw.get("temperature").and_then(|v| v.as_f64()) {
            out.insert("temperature".into(), json!(clamp_anthropic_temperature(t)));
        }
        if let Some(tp) = raw.get("top_p").and_then(|v| v.as_f64()) {
            out.insert("top_p".into(), json!(tp));
        }
    }

    // stop → stop_sequences。空串是合法 JSON 但 Anthropic 拒绝空 stop sequence(透传路径 400),
    // 过滤掉;全空则不下发该字段。
    match raw.get("stop") {
        Some(Value::Array(arr)) => {
            let seqs: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().filter(|s| !s.is_empty()).map(String::from))
                .collect();
            if !seqs.is_empty() {
                out.insert("stop_sequences".into(), json!(seqs));
            }
        }
        Some(Value::String(s)) if !s.is_empty() => {
            out.insert("stop_sequences".into(), json!([s]));
        }
        _ => {}
    }

    // messages 分流
    let mut system_blocks: Vec<Value> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    if let Some(Value::Array(msgs)) = raw.get("messages") {
        for m in msgs {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
            match role {
                "system" | "developer" => {
                    collect_system_text(m.get("content"), &mut system_blocks);
                }
                "user" | "assistant" => {
                    if let Some(msg) = build_user_or_assistant(role, m) {
                        messages.push(msg);
                    }
                }
                "tool" => {
                    messages.push(build_tool_result(m));
                }
                "function" => {
                    // legacy function role:name 当 call_id。
                    messages.push(build_legacy_function_result(m));
                }
                _ => {}
            }
        }
    }

    // ⭐工具配对修复 + 同角色合并(防 Anthropic 交替/配对不变量 400)。
    // Kiro 主路径 convert_request 有自己的归一,但 custom_api 透传路径把本 body 原样转发给
    // Anthropic 兼容上游,不经任何修复——故在此就把输出归一成合法 Anthropic 消息序列,两条路径都安全。
    // 规则(移植 sub2api normalizeAnthropicToolPairing + mergeConsecutiveMessages):
    //   ① 丢弃无对应 tool_use 的孤儿 tool_result;② 丢弃无对应 tool_result 的悬空 tool_use;
    //   ③ 合并连续同角色消息(并行工具调用/结果归组),恢复 user/assistant 严格交替。
    messages = normalize_tool_pairing_and_merge(messages);

    // 空消息兜底:补一条空 user,防 Anthropic「messages 非空」400。
    // 覆盖两种退化输入:①system-only(有 system 无 user/assistant);②整体空请求。
    if messages.is_empty() {
        messages.push(json!({"role": "user", "content": [{"type": "text", "text": ""}]}));
    }

    // response_format(JSON mode):Anthropic 无原生支持,翻成 system 指令引导(追加,不覆盖用户 system)。
    if let Some(rf) = raw.get("response_format") {
        if let Some(instr) = response_format_instruction(rf) {
            system_blocks.push(json!({"type": "text", "text": instr}));
        }
    }

    if !system_blocks.is_empty() {
        out.insert("system".into(), json!(system_blocks));
    }
    out.insert("messages".into(), json!(messages));

    // tools
    if let Some(Value::Array(tools)) = raw.get("tools") {
        let mut anth_tools: Vec<Value> = Vec::new();
        for t in tools {
            if t.get("type").and_then(|v| v.as_str()) == Some("function") {
                if let Some(func) = t.get("function") {
                    let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let desc = func.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    let schema = func
                        .get("parameters")
                        .or_else(|| func.get("parametersJsonSchema"))
                        .cloned();
                    anth_tools.push(json!({
                        "name": name,
                        "description": desc,
                        "input_schema": normalize_input_schema(schema),
                    }));
                }
            }
        }
        if !anth_tools.is_empty() {
            out.insert("tools".into(), json!(anth_tools));
        }
    }

    // tool_choice。⚠️ 仅当确实下发了 tools 才设:Anthropic 对「有 tool_choice 无 tools」400。
    // 客户端可能发 tool_choice 但 tools 为空/全被过滤(非 function 类型),此时必须不下发 tool_choice。
    if out.contains_key("tools") {
        if let Some(tc) = raw.get("tool_choice") {
            match tc {
                Value::String(s) => match s.as_str() {
                    "auto" => { out.insert("tool_choice".into(), json!({"type": "auto"})); }
                    "required" => { out.insert("tool_choice".into(), json!({"type": "any"})); }
                    // "none" = 客户端明确禁止调用工具 → 显式下发 {type:none},绝不能不设(不设=回落默认 auto,
                    // 模型仍可能调工具,违背客户端意图)。Kiro 转换层不认时会忽略,无害。
                    "none" => { out.insert("tool_choice".into(), json!({"type": "none"})); }
                    _ => {}
                },
                Value::Object(_) => {
                    if tc.get("type").and_then(|v| v.as_str()) == Some("function") {
                        if let Some(name) = tc.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()) {
                            out.insert("tool_choice".into(), json!({"type": "tool", "name": name}));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    Value::Object(out)
}

/// tool input_schema 归一化:缺失/null → 空 object schema;object 缺 properties → 补空。
/// Anthropic 对缺 properties 的 object schema 会 400。
fn normalize_input_schema(schema: Option<Value>) -> Value {
    match schema {
        Some(Value::Object(mut m)) => {
            let is_object = m.get("type").and_then(|v| v.as_str()) == Some("object");
            if is_object && !m.contains_key("properties") {
                m.insert("properties".into(), json!({}));
            }
            if !m.contains_key("type") {
                m.insert("type".into(), json!("object"));
                m.entry("properties").or_insert(json!({}));
            }
            Value::Object(m)
        }
        _ => json!({"type": "object", "properties": {}}),
    }
}

/// system content(字符串或 parts 数组)→ Anthropic system text 块,追加到 blocks。
fn collect_system_text(content: Option<&Value>, blocks: &mut Vec<Value>) {
    match content {
        Some(Value::String(s)) if !s.is_empty() => {
            blocks.push(json!({"type": "text", "text": s}));
        }
        Some(Value::Array(parts)) => {
            for p in parts {
                if p.get("type").and_then(|v| v.as_str()) == Some("text") {
                    let t = p.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    blocks.push(json!({"type": "text", "text": t}));
                }
            }
        }
        _ => {}
    }
}

/// user/assistant message → Anthropic message(text/image/file + assistant 的 tool_calls→tool_use)。
fn build_user_or_assistant(role: &str, m: &Value) -> Option<Value> {
    let mut content: Vec<Value> = Vec::new();
    match m.get("content") {
        Some(Value::String(s)) if !s.is_empty() => {
            content.push(json!({"type": "text", "text": s}));
        }
        Some(Value::Array(parts)) => {
            for p in parts {
                if let Some(block) = openai_content_part_to_anthropic(p) {
                    content.push(block);
                }
            }
        }
        _ => {}
    }

    // assistant 的 tool_calls → tool_use 块
    if role == "assistant" {
        if let Some(Value::Array(calls)) = m.get("tool_calls") {
            for call in calls {
                if call.get("type").and_then(|v| v.as_str()) == Some("function") {
                    let id = call
                        .get("id")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(sanitize_tool_id)
                        .unwrap_or_else(gen_tool_call_id);
                    let func = call.get("function");
                    let name = func.and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("");
                    let input = func
                        .and_then(|f| f.get("arguments"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .filter(|v| v.is_object())
                        .unwrap_or_else(|| json!({}));
                    content.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
                }
            }
        }
    }

    // assistant 只有 tool_use 没文本时,content 已含 tool_use;user 空内容则跳过发一条空块防呆。
    if content.is_empty() {
        // user 空 content:补空 text 保持交替;assistant 空:也给空 text(极少见)。
        content.push(json!({"type": "text", "text": ""}));
    }
    Some(json!({"role": role, "content": content}))
}

/// tool role → user 的 tool_result 块。
fn build_tool_result(m: &Value) -> Value {
    let tool_use_id = m
        .get("tool_call_id")
        .and_then(|v| v.as_str())
        .map(sanitize_tool_id)
        .unwrap_or_default();
    let content = tool_result_content(m.get("content"));
    json!({
        "role": "user",
        "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": content}]
    })
}

/// legacy function role:用 name 当 call_id。
fn build_legacy_function_result(m: &Value) -> Value {
    let tool_use_id = m
        .get("name")
        .and_then(|v| v.as_str())
        .map(sanitize_tool_id)
        .unwrap_or_default();
    let content = tool_result_content(m.get("content"));
    json!({
        "role": "user",
        "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": content}]
    })
}

/// tool_result 的 content:空 → "(empty)"(Anthropic 不接受空串);字符串原样;数组转块。
fn tool_result_content(content: Option<&Value>) -> Value {
    match content {
        Some(Value::String(s)) => {
            if s.is_empty() { json!("(empty)") } else { json!(s) }
        }
        Some(Value::Array(parts)) => {
            let mut blocks: Vec<Value> = Vec::new();
            for p in parts {
                if let Some(s) = p.as_str() {
                    blocks.push(json!({"type": "text", "text": s}));
                } else if let Some(b) = openai_content_part_to_anthropic(p) {
                    blocks.push(b);
                }
            }
            if blocks.is_empty() { json!("(empty)") } else { json!(blocks) }
        }
        _ => json!("(empty)"),
    }
}

/// OpenAI content part → Anthropic 块(text / image_url / file)。
fn openai_content_part_to_anthropic(part: &Value) -> Option<Value> {
    match part.get("type").and_then(|v| v.as_str()) {
        Some("text") => {
            let t = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
            Some(json!({"type": "text", "text": t}))
        }
        Some("image_url") => {
            let url = part.get("image_url").and_then(|u| u.get("url")).and_then(|v| v.as_str()).unwrap_or("");
            image_url_to_anthropic(url)
        }
        Some("file") => {
            let data = part.get("file").and_then(|f| f.get("file_data")).and_then(|v| v.as_str()).unwrap_or("");
            data_uri_to_document(data)
        }
        _ => None,
    }
}

/// image_url:data URI → base64 image 块;http(s) url → url image 块。空 base64 丢弃。
fn image_url_to_anthropic(url: &str) -> Option<Value> {
    if url.is_empty() {
        return None;
    }
    if let Some(rest) = url.strip_prefix("data:") {
        let comma = rest.find(',')?;
        let meta = &rest[..comma];
        let data = &rest[comma + 1..];
        if data.is_empty() {
            return None; // 空 base64 data URI 丢弃(上游报错)
        }
        let media_type = meta.split(';').next().filter(|s| !s.is_empty()).unwrap_or("image/png");
        Some(json!({
            "type": "image",
            "source": {"type": "base64", "media_type": media_type, "data": data}
        }))
    } else {
        Some(json!({"type": "image", "source": {"type": "url", "url": url}}))
    }
}

/// file data URI → Anthropic document(base64)块。
fn data_uri_to_document(data: &str) -> Option<Value> {
    let rest = data.strip_prefix("data:")?;
    let semi = rest.find(';')?;
    let comma = rest.find(',')?;
    if comma <= semi {
        return None;
    }
    let media_type = &rest[..semi];
    let b64 = &rest[comma + 1..];
    Some(json!({
        "type": "document",
        "source": {"type": "base64", "media_type": media_type, "data": b64}
    }))
}

/// 清洗工具 id(Anthropic id 只接受安全字符;这里保守只留字母数字_-)。
fn sanitize_tool_id(id: &str) -> String {
    let s: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
    if s.is_empty() { gen_tool_call_id() } else { s }
}

/// 工具配对修复 + 同角色合并,产出合法 Anthropic 消息序列。
///
/// 步骤:
/// 1. 收集所有 tool_result 的 tool_use_id 集合(哪些调用被回应了)。
/// 2. 遍历消息:assistant 里未被回应的 tool_use 块丢弃;user 里的孤儿 tool_result(无对应
///    tool_use)丢弃。丢空块后若消息 content 空则整条丢。
/// 3. 合并连续同角色消息(content 数组拼接),恢复 user/assistant 交替。
fn normalize_tool_pairing_and_merge(messages: Vec<Value>) -> Vec<Value> {
    // ① 收集被回应的 tool_use_id(来自所有 user 消息的 tool_result 块)。
    let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in &messages {
        if m.get("role").and_then(|v| v.as_str()) == Some("user") {
            if let Some(Value::Array(content)) = m.get("content") {
                for b in content {
                    if b.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                        if let Some(id) = b.get("tool_use_id").and_then(|v| v.as_str()) {
                            answered.insert(id.to_string());
                        }
                    }
                }
            }
        }
    }
    // 收集所有出现过的 tool_use_id(用于判断 tool_result 是否孤儿)。
    let mut declared: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in &messages {
        if m.get("role").and_then(|v| v.as_str()) == Some("assistant") {
            if let Some(Value::Array(content)) = m.get("content") {
                for b in content {
                    if b.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                        if let Some(id) = b.get("id").and_then(|v| v.as_str()) {
                            declared.insert(id.to_string());
                        }
                    }
                }
            }
        }
    }

    // ② 过滤块。
    let mut filtered: Vec<Value> = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let content = match m.get("content") {
            Some(Value::Array(c)) => c.clone(),
            other => {
                // 非数组 content(不应出现,build_* 都产数组)原样保留。
                if let Some(o) = other {
                    filtered.push(json!({"role": role, "content": o.clone()}));
                }
                continue;
            }
        };
        let mut kept: Vec<Value> = Vec::new();
        for b in content {
            match b.get("type").and_then(|v| v.as_str()) {
                Some("tool_use") if role == "assistant" => {
                    let id = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    if answered.contains(id) {
                        kept.push(b); // 只保留被回应的 tool_use
                    }
                    // 未回应的悬空 tool_use 丢弃
                }
                Some("tool_result") if role == "user" => {
                    let id = b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                    if declared.contains(id) {
                        kept.push(b); // 只保留有对应 tool_use 的 tool_result
                    }
                    // 孤儿 tool_result 丢弃
                }
                _ => kept.push(b),
            }
        }
        if !kept.is_empty() {
            filtered.push(json!({"role": role, "content": kept}));
        }
    }

    // ③ 合并连续同角色消息(content 数组拼接)。
    let mut merged: Vec<Value> = Vec::new();
    for m in filtered {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let same_as_prev = merged
            .last()
            .and_then(|l| l.get("role").and_then(|v| v.as_str()))
            == Some(role.as_str());
        if same_as_prev {
            // 同角色:把本条 content 追加到上一条的 content 数组。
            let cur_content = m.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default();
            if let Some(prev_arr) = merged
                .last_mut()
                .and_then(|l| l.get_mut("content"))
                .and_then(|c| c.as_array_mut())
            {
                prev_arr.extend(cur_content);
                continue;
            }
        }
        merged.push(m);
    }

    // ④ 首条必须是 user(Anthropic/兼容上游硬要求,否则 400)。窗口化对话可能因开头 orphan
    // tool_result 被丢、或客户端 assistant 起头,导致首条是 assistant → 补一条空 user 打头。
    if merged
        .first()
        .and_then(|m| m.get("role").and_then(|v| v.as_str()))
        == Some("assistant")
    {
        merged.insert(0, json!({"role": "user", "content": [{"type": "text", "text": ""}]}));
    }

    merged
}

// ============ /v1/responses 请求 → Anthropic ============

/// 把 OpenAI Responses 请求 JSON 翻译成 Anthropic MessagesRequest JSON。
///
/// Responses 与 chat/completions 的差异:
/// - `input` 可为字符串(单条 user)或 item 数组;`instructions` 顶层字段 → system。
/// - item 类型:`message`(input_text→user/output_text→assistant)、`function_call`→assistant tool_use、
///   `function_call_output`→user tool_result、`reasoning`(带 encrypted_content 签名,Anthropic 不通用→丢,
///   只保留可见性,由上游重新思考)。
/// - `max_output_tokens`→max_tokens;`reasoning.effort`→thinking。
/// 复用 [`normalize_tool_pairing_and_merge`] 做配对/交替归一(不自己写 pending-flush 机器)。
pub fn openai_responses_to_anthropic(model: &str, raw: &Value, stream: bool) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    let max_tokens = raw
        .get("max_output_tokens")
        .and_then(|v| v.as_i64())
        .or_else(|| raw.get("max_tokens").and_then(|v| v.as_i64()))
        .unwrap_or(DEFAULT_MAX_TOKENS);
    out.insert("max_tokens".into(), json!(max_tokens));
    out.insert("stream".into(), json!(stream));

    // reasoning.effort → thinking(none→disabled,其余→enabled)。
    let mut thinking_enabled = false;
    if let Some(effort) = raw.get("reasoning").and_then(|r| r.get("effort")).and_then(|v| v.as_str()) {
        let e = effort.trim().to_lowercase();
        if e == "none" {
            out.insert("thinking".into(), json!({"type": "disabled"}));
        } else if !e.is_empty() {
            out.insert("thinking".into(), json!({"type": "enabled"}));
            thinking_enabled = true;
        }
    }
    // temperature/top_p:同 chat 路径,thinking 开启时都不透传(Anthropic thinking 模式约束),防透传路径 400。
    if !thinking_enabled {
        if let Some(t) = raw.get("temperature").and_then(|v| v.as_f64()) {
            out.insert("temperature".into(), json!(clamp_anthropic_temperature(t)));
        }
        if let Some(tp) = raw.get("top_p").and_then(|v| v.as_f64()) {
            out.insert("top_p".into(), json!(tp));
        }
    }

    // system:instructions 顶层字段(Responses 惯例)。
    let mut system_blocks: Vec<Value> = Vec::new();
    if let Some(instr) = raw.get("instructions").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        system_blocks.push(json!({"type": "text", "text": instr}));
    }

    // input:字符串 → 单条 user;数组 → 逐 item 转。
    let mut messages: Vec<Value> = Vec::new();
    match raw.get("input") {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                messages.push(json!({"role": "user", "content": [{"type": "text", "text": s}]}));
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                let typ = item.get("type").and_then(|v| v.as_str()).unwrap_or_else(|| {
                    // 无 type 但有 role → 当 message。
                    if item.get("role").is_some() { "message" } else { "" }
                });
                match typ {
                    "message" => {
                        let role_raw = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                        if role_raw == "system" || role_raw == "developer" {
                            collect_responses_system(item.get("content"), &mut system_blocks);
                        } else if let Some(msg) = build_responses_message(role_raw, item) {
                            messages.push(msg);
                        }
                    }
                    // function_call(arguments=JSON 字符串)与 custom_tool_call(input=自由字符串,
                    // Codex 的 apply_patch 类自定义工具走这个)都翻成 assistant tool_use。
                    // 区别:function_call 的 arguments 期望是 JSON object;custom_tool_call 的 input 可能是
                    // 非 JSON 的自由文本 → 非 object 时包成 {"input": <原文>} 保住内容(Anthropic input 必须 object)。
                    "function_call" | "custom_tool_call" => {
                        let call_id = item
                            .get("call_id")
                            .and_then(|v| v.as_str())
                            .map(sanitize_tool_id)
                            .unwrap_or_else(gen_tool_call_id);
                        let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        // function_call 用 arguments、custom_tool_call 用 input。
                        let raw_args = item
                            .get("arguments")
                            .or_else(|| item.get("input"))
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty());
                        let input = match raw_args.and_then(|s| serde_json::from_str::<Value>(s).ok()) {
                            Some(v) if v.is_object() => v,
                            // 非 JSON object(自由文本/JSON 标量)→ 包进 {"input": ...} 保内容不丢。
                            _ => match raw_args {
                                Some(s) => json!({"input": s}),
                                None => json!({}),
                            },
                        };
                        messages.push(json!({
                            "role": "assistant",
                            "content": [{"type": "tool_use", "id": call_id, "name": name, "input": input}]
                        }));
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        let call_id = item
                            .get("call_id")
                            .and_then(|v| v.as_str())
                            .map(sanitize_tool_id)
                            .unwrap_or_default();
                        let content = responses_output_content(item.get("output"));
                        messages.push(json!({
                            "role": "user",
                            "content": [{"type": "tool_result", "tool_use_id": call_id, "content": content}]
                        }));
                    }
                    // reasoning item:Anthropic thinking 签名与 OpenAI encrypted_content 不通用,丢弃
                    // (上游会按 thinking 配置重新推理)。其余未知 type 忽略。
                    _ => {}
                }
            }
        }
        _ => {}
    }

    // tools(Responses 的 function tool:顶层 name/parameters,非 chat 的 function 嵌套)。
    if let Some(Value::Array(tools)) = raw.get("tools") {
        let mut anth_tools: Vec<Value> = Vec::new();
        for t in tools {
            if t.get("type").and_then(|v| v.as_str()) == Some("function") {
                let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let desc = t.get("description").and_then(|v| v.as_str()).unwrap_or("");
                let schema = t.get("parameters").cloned();
                anth_tools.push(json!({
                    "name": name,
                    "description": desc,
                    "input_schema": normalize_input_schema(schema),
                }));
            }
        }
        if !anth_tools.is_empty() {
            out.insert("tools".into(), json!(anth_tools));
        }
    }

    // tool_choice(Responses 同 chat:字符串 auto/required/none 或 {type:function,name})。
    // ⚠️ 同 chat:仅当确实下发 tools 才设,防 Anthropic「有 tool_choice 无 tools」400。
    if out.contains_key("tools") {
        if let Some(tc) = raw.get("tool_choice") {
            match tc {
                Value::String(s) => match s.as_str() {
                    "auto" => { out.insert("tool_choice".into(), json!({"type": "auto"})); }
                    "required" => { out.insert("tool_choice".into(), json!({"type": "any"})); }
                    "none" => { out.insert("tool_choice".into(), json!({"type": "none"})); }
                    _ => {}
                },
                Value::Object(_) => {
                    if tc.get("type").and_then(|v| v.as_str()) == Some("function") {
                        if let Some(name) = tc.get("name").and_then(|v| v.as_str()) {
                            out.insert("tool_choice".into(), json!({"type": "tool", "name": name}));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    messages = normalize_tool_pairing_and_merge(messages);
    // 空消息兜底:同 chat 路径,覆盖 instructions-only 与整体空 input,防 Anthropic 400。
    if messages.is_empty() {
        messages.push(json!({"role": "user", "content": [{"type": "text", "text": ""}]}));
    }

    // 结构化输出:Responses API 原生用 `text.format`(type=json_object/json_schema),部分客户端仍发
    // `response_format`。两者都认,翻成 system 指令引导(追加,不覆盖用户 instructions)。
    let rf = raw
        .get("text")
        .and_then(|t| t.get("format"))
        .or_else(|| raw.get("response_format"));
    if let Some(rf) = rf {
        if let Some(instr) = response_format_instruction(rf) {
            system_blocks.push(json!({"type": "text", "text": instr}));
        }
    }

    if !system_blocks.is_empty() {
        out.insert("system".into(), json!(system_blocks));
    }
    out.insert("messages".into(), json!(messages));
    Value::Object(out)
}

/// Responses system item content → system text 块。
fn collect_responses_system(content: Option<&Value>, blocks: &mut Vec<Value>) {
    match content {
        Some(Value::String(s)) if !s.is_empty() => blocks.push(json!({"type": "text", "text": s})),
        Some(Value::Array(parts)) => {
            for p in parts {
                if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                    blocks.push(json!({"type": "text", "text": t}));
                }
            }
        }
        _ => {}
    }
}

/// Responses message item(input_text/output_text/input_image/input_file)→ Anthropic message。
/// role 由 content part 类型推断(input_*→user,output_*→assistant),回落 item.role。
fn build_responses_message(role_hint: &str, item: &Value) -> Option<Value> {
    let mut content: Vec<Value> = Vec::new();
    let mut inferred_role: Option<&str> = None;
    match item.get("content") {
        Some(Value::String(s)) if !s.is_empty() => {
            content.push(json!({"type": "text", "text": s}));
        }
        Some(Value::Array(parts)) => {
            for p in parts {
                match p.get("type").and_then(|v| v.as_str()) {
                    Some("input_text") => {
                        inferred_role = Some("user");
                        content.push(json!({"type": "text", "text": p.get("text").and_then(|v| v.as_str()).unwrap_or("")}));
                    }
                    Some("output_text") => {
                        inferred_role = Some("assistant");
                        content.push(json!({"type": "text", "text": p.get("text").and_then(|v| v.as_str()).unwrap_or("")}));
                    }
                    Some("input_image") => {
                        let url = p.get("image_url").and_then(|v| v.as_str())
                            .or_else(|| p.get("url").and_then(|v| v.as_str())).unwrap_or("");
                        if let Some(b) = image_url_to_anthropic(url) {
                            content.push(b);
                            inferred_role.get_or_insert("user");
                        }
                    }
                    Some("input_file") => {
                        let fd = p.get("file_data").and_then(|v| v.as_str()).unwrap_or("");
                        if let Some(b) = data_uri_to_document(fd) {
                            content.push(b);
                            inferred_role.get_or_insert("user");
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    let role = inferred_role.unwrap_or(match role_hint {
        "assistant" => "assistant",
        _ => "user",
    });
    if content.is_empty() {
        content.push(json!({"type": "text", "text": ""}));
    }
    Some(json!({"role": role, "content": content}))
}

/// Responses function_call_output 的 output(字符串或结构)→ Anthropic tool_result content。
fn responses_output_content(output: Option<&Value>) -> Value {
    match output {
        Some(Value::String(s)) => if s.is_empty() { json!("(empty)") } else { json!(s) },
        Some(Value::Array(parts)) => {
            let mut blocks = Vec::new();
            for p in parts {
                if let Some(s) = p.as_str() {
                    blocks.push(json!({"type": "text", "text": s}));
                } else if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                    blocks.push(json!({"type": "text", "text": t}));
                }
            }
            if blocks.is_empty() { json!("(empty)") } else { json!(blocks) }
        }
        Some(v) => json!(v.to_string()),
        None => json!("(empty)"),
    }
}

/// Anthropic stop_reason → OpenAI finish_reason。
pub fn map_stop_reason(anthropic: &str) -> &'static str {
    match anthropic {
        "end_turn" | "stop_sequence" => "stop",
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        "content_filter" => "content_filter",
        _ => "stop",
    }
}

// ============ 响应:Anthropic → OpenAI ============

/// Anthropic usage → OpenAI usage 折算(prompt_tokens 含 cache,Anthropic input 不含)。
#[derive(Default, Clone)]
struct UsageTokens {
    input: i64,
    output: i64,
    cache_creation: i64,
    cache_read: i64,
    has: bool,
}

impl UsageTokens {
    fn merge(&mut self, usage: &Value) {
        if !usage.is_object() {
            return;
        }
        self.has = true;
        if let Some(v) = usage.get("input_tokens").and_then(|v| v.as_i64()) { self.input = v; }
        if let Some(v) = usage.get("output_tokens").and_then(|v| v.as_i64()) { self.output = v; }
        if let Some(v) = usage.get("cache_creation_input_tokens").and_then(|v| v.as_i64()) { self.cache_creation = v; }
        if let Some(v) = usage.get("cache_read_input_tokens").and_then(|v| v.as_i64()) { self.cache_read = v; }
    }
    /// (prompt, completion, total, cached)
    fn openai(&self) -> (i64, i64, i64, i64) {
        let cached = self.cache_read;
        let prompt = self.input + self.cache_creation + cached;
        let completion = self.output;
        (prompt, completion, prompt + completion, cached)
    }
}

/// 单个工具调用的流式累加器。
struct ToolAccum {
    id: String,
    name: String,
    args: String,
    /// 分配给 OpenAI 的 tool_calls 数组位序(0-based,单调递增),**非** Anthropic content-block index。
    /// OpenAI SDK 按此 index 重建 tool_calls 数组;若用 content-block index(思考关时首个 text 块占 0,
    /// 工具从 1 起)会让 SDK 造出 index=0 的幻影空工具调用。
    openai_index: i64,
}

/// Anthropic SSE → OpenAI chat.completion.chunk 的流式状态机。
/// 逐个 Anthropic 事件喂入,吐出 0..n 个 OpenAI chunk 的 JSON(不含 `data: ` 前缀,由调用方包裹)。
pub struct ChatStreamConverter {
    model: String,
    response_id: String,
    created: i64,
    usage: UsageTokens,
    /// content block index → 工具累加器(仅 tool_use 块)。
    tools: std::collections::HashMap<i64, ToolAccum>,
    /// 已分配的 OpenAI tool_calls 位序计数(每遇一个 tool_use 块 +1),保证 0-based 连续。
    next_tool_index: i64,
}

impl ChatStreamConverter {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            response_id: String::new(),
            created: 0,
            usage: UsageTokens::default(),
            tools: std::collections::HashMap::new(),
            next_tool_index: 0,
        }
    }

    fn base_chunk(&self) -> Value {
        json!({
            "id": self.response_id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": null}]
        })
    }

    /// 喂一个 Anthropic 事件(已解析的 JSON),返回要发给客户端的 OpenAI chunk 列表。
    pub fn push_event(&mut self, ev: &Value) -> Vec<Value> {
        let event_type = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match event_type {
            "message_start" => {
                if let Some(msg) = ev.get("message") {
                    self.response_id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if self.response_id.is_empty() {
                        self.response_id = crate::openai::types::gen_chat_completion_id();
                    }
                    self.created = now_unix();
                    if let Some(u) = msg.get("usage") { self.usage.merge(u); }
                    let mut chunk = self.base_chunk();
                    chunk["choices"][0]["delta"] = json!({"role": "assistant"});
                    return vec![chunk];
                }
                vec![]
            }
            "content_block_start" => {
                let cb = ev.get("content_block");
                if cb.and_then(|c| c.get("type")).and_then(|v| v.as_str()) == Some("tool_use") {
                    let index = ev.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                    let id = cb.and_then(|c| c.get("id")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = cb.and_then(|c| c.get("name")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                    // 分配连续的 OpenAI tool_calls 位序(与 Anthropic content-block index 解耦)。
                    let openai_index = self.next_tool_index;
                    self.next_tool_index += 1;
                    self.tools.insert(index, ToolAccum { id, name, args: String::new(), openai_index });
                }
                vec![]
            }
            "content_block_delta" => {
                let delta = match ev.get("delta") { Some(d) => d, None => return vec![] };
                match delta.get("type").and_then(|v| v.as_str()) {
                    Some("text_delta") => {
                        if let Some(t) = delta.get("text").and_then(|v| v.as_str()) {
                            let mut chunk = self.base_chunk();
                            chunk["choices"][0]["delta"] = json!({"content": t});
                            return vec![chunk];
                        }
                        vec![]
                    }
                    Some("thinking_delta") => {
                        if let Some(t) = delta.get("thinking").and_then(|v| v.as_str()) {
                            let mut chunk = self.base_chunk();
                            chunk["choices"][0]["delta"] = json!({"reasoning_content": t});
                            return vec![chunk];
                        }
                        vec![]
                    }
                    Some("input_json_delta") => {
                        // 累加不发(缓冲到 content_block_stop 一次性吐完整 tool_call,规避 accumulator shear)。
                        if let Some(pj) = delta.get("partial_json").and_then(|v| v.as_str()) {
                            let index = ev.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                            if let Some(acc) = self.tools.get_mut(&index) {
                                acc.args.push_str(pj);
                            }
                        }
                        vec![]
                    }
                    _ => vec![],
                }
            }
            "content_block_stop" => {
                let index = ev.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                if let Some(acc) = self.tools.remove(&index) {
                    let args = if acc.args.is_empty() { "{}".to_string() } else { acc.args };
                    let mut chunk = self.base_chunk();
                    chunk["choices"][0]["delta"] = json!({
                        "tool_calls": [{
                            "index": acc.openai_index,
                            "id": acc.id,
                            "type": "function",
                            "function": {"name": acc.name, "arguments": args}
                        }]
                    });
                    return vec![chunk];
                }
                vec![]
            }
            "message_delta" => {
                // OpenAI 流式规范:finish_reason 放在带 choices 的 chunk;usage 单独放在**最后一个
                // choices:[] 的 chunk**(紧邻 [DONE])。此前把 usage 塞在 finish_reason 同一 chunk 上,
                // 严格 SDK 在 choices[0] 找不到独立 usage。拆成两个 chunk:先 finish_reason,再 usage。
                let mut out = Vec::new();
                if let Some(sr) = ev.get("delta").and_then(|d| d.get("stop_reason")).and_then(|v| v.as_str()) {
                    let mut chunk = self.base_chunk();
                    chunk["choices"][0]["finish_reason"] = json!(map_stop_reason(sr));
                    out.push(chunk);
                }
                if let Some(u) = ev.get("usage") {
                    self.usage.merge(u);
                    let (p, c, t, cached) = self.usage.openai();
                    let mut usage_chunk = self.base_chunk();
                    // usage chunk 的 choices 为空数组(OpenAI 规范);usage 挂顶层。
                    usage_chunk["choices"] = json!([]);
                    usage_chunk["usage"] = json!({
                        "prompt_tokens": p, "completion_tokens": c, "total_tokens": t,
                        "prompt_tokens_details": {"cached_tokens": cached}
                    });
                    out.push(usage_chunk);
                }
                out
            }
            "error" => {
                if let Some(e) = ev.get("error") {
                    let msg = e.get("message").and_then(|v| v.as_str()).unwrap_or("upstream error");
                    let typ = e.get("type").and_then(|v| v.as_str()).unwrap_or("api_error");
                    return vec![json!({"error": {"message": msg, "type": typ}})];
                }
                vec![]
            }
            _ => vec![],
        }
    }
}

/// 非流式聚合:把内部产出的一串 Anthropic SSE 事件(已解析)聚合成单个 OpenAI chat.completion。
pub fn aggregate_chat_completion(model: &str, events: &[Value]) -> Value {
    let mut message_id = String::new();
    let mut created = now_unix();
    let mut usage = UsageTokens::default();
    let mut stop_reason = String::from("end_turn");
    let mut text = String::new();
    let mut reasoning = String::new();
    // index → (id, name, args)
    let mut tools: std::collections::BTreeMap<i64, (String, String, String)> = std::collections::BTreeMap::new();

    for ev in events {
        match ev.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "message_start" => {
                if let Some(m) = ev.get("message") {
                    message_id = m.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    created = now_unix();
                    if let Some(u) = m.get("usage") { usage.merge(u); }
                }
            }
            "content_block_start" => {
                if ev.get("content_block").and_then(|c| c.get("type")).and_then(|v| v.as_str()) == Some("tool_use") {
                    let index = ev.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                    let cb = ev.get("content_block");
                    let id = cb.and_then(|c| c.get("id")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = cb.and_then(|c| c.get("name")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                    tools.insert(index, (id, name, String::new()));
                }
            }
            "content_block_delta" => {
                if let Some(delta) = ev.get("delta") {
                    match delta.get("type").and_then(|v| v.as_str()) {
                        Some("text_delta") => {
                            if let Some(t) = delta.get("text").and_then(|v| v.as_str()) { text.push_str(t); }
                        }
                        Some("thinking_delta") => {
                            if let Some(t) = delta.get("thinking").and_then(|v| v.as_str()) { reasoning.push_str(t); }
                        }
                        Some("input_json_delta") => {
                            if let Some(pj) = delta.get("partial_json").and_then(|v| v.as_str()) {
                                let index = ev.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                                if let Some(e) = tools.get_mut(&index) { e.2.push_str(pj); }
                            }
                        }
                        _ => {}
                    }
                }
            }
            "message_delta" => {
                if let Some(sr) = ev.get("delta").and_then(|d| d.get("stop_reason")).and_then(|v| v.as_str()) {
                    stop_reason = sr.to_string();
                }
                if let Some(u) = ev.get("usage") { usage.merge(u); }
            }
            _ => {}
        }
    }

    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    // OpenAI 规范:assistant 只返回 tool_calls(无文本)时 content 应为 null,不是空串——
    // 部分严格客户端(及 OpenAI SDK 类型)对「空串 + tool_calls」处理不同,给 null 最兼容。
    if text.is_empty() && !tools.is_empty() {
        message.insert("content".into(), Value::Null);
    } else {
        message.insert("content".into(), json!(text));
    }
    if !reasoning.is_empty() {
        message.insert("reasoning".into(), json!(reasoning));
        message.insert("reasoning_content".into(), json!(reasoning));
    }
    let finish_reason = if !tools.is_empty() {
        let calls: Vec<Value> = tools
            .values()
            .map(|(id, name, args)| {
                let a = if args.is_empty() { "{}" } else { args.as_str() };
                json!({"id": id, "type": "function", "function": {"name": name, "arguments": a}})
            })
            .collect();
        message.insert("tool_calls".into(), json!(calls));
        "tool_calls".to_string()
    } else {
        map_stop_reason(&stop_reason).to_string()
    };

    let (p, c, t, cached) = usage.openai();
    let id = if message_id.is_empty() { crate::openai::types::gen_chat_completion_id() } else { message_id };
    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "message": Value::Object(message), "finish_reason": finish_reason}],
        "usage": {
            "prompt_tokens": p, "completion_tokens": c, "total_tokens": t,
            "prompt_tokens_details": {"cached_tokens": cached}
        }
    })
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ============ /v1/responses 响应:Anthropic → Responses SSE ============

/// 生成一个 Responses 风格 id(resp_<hex> / item_<hex>)。
fn gen_responses_id(prefix: &str) -> String {
    format!("{}_{}", prefix, random_hex(24))
}

/// Anthropic SSE → OpenAI Responses SSE 事件序列的流式状态机。
///
/// 产出完整生命周期(严格客户端 Codex 靠 sequence_number 排序、靠 output_item.added 先于 delta):
/// response.created → response.in_progress →(每个块:output_item.added → content_part.added →
/// output_text.delta* → output_text.done → content_part.done → output_item.done)→ response.completed。
/// tool_use 块走 function_call item(function_call_arguments.delta/done)。
/// reasoning 块:签名不兼容(见 memory),只发文本 summary 事件不带 encrypted_content。
///
/// 每个 `push_event` 返回若干 `(event_type, json)` 对,由调用方格式化成 `event: T\ndata: {..}\n\n`。
pub struct ResponsesStreamConverter {
    model: String,
    response_id: String,
    created: i64,
    seq: i64,
    started: bool,
    usage: UsageTokens,
    /// content-block index → (kind, item_id, openai_output_index)。kind: "text"/"tool"/"reasoning"。
    blocks: std::collections::HashMap<i64, BlockState>,
    next_output_index: i64,
    stop_reason: String,
    /// 已闭合块的快照(按 output_index 顺序),供 message_stop 重建 response.completed.output。
    finished: Vec<FinishedItem>,
}

struct BlockState {
    kind: &'static str,
    item_id: String,
    output_index: i64,
    /// tool_use 的 call_id/name(function_call item 用)。
    call_id: String,
    name: String,
    /// 累加缓冲:文本块累 text、tool 块累 arguments、reasoning 块累 summary text。
    /// 终结事件(.done / output_item.done)必须回填全量(严格客户端如 Codex 从终态 item 取权威内容,
    /// delta 仅供 UI),否则工具拿空参、消息落空文本。
    buf: String,
}

/// 已完成块的快照,供 message_stop 重建 response.completed.output。
#[derive(Clone)]
struct FinishedItem {
    kind: &'static str,
    item_id: String,
    call_id: String,
    name: String,
    text: String,
}

impl ResponsesStreamConverter {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            response_id: String::new(),
            created: 0,
            seq: 0,
            started: false,
            usage: UsageTokens::default(),
            blocks: std::collections::HashMap::new(),
            next_output_index: 0,
            stop_reason: "end_turn".to_string(),
            finished: Vec::new(),
        }
    }

    fn next_seq(&mut self) -> i64 {
        let s = self.seq;
        self.seq += 1;
        s
    }

    /// 喂一个 Anthropic 事件,返回 (event_type, data_json) 列表。
    pub fn push_event(&mut self, ev: &Value) -> Vec<(String, Value)> {
        let et = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match et {
            "message_start" => {
                let msg = match ev.get("message") { Some(m) => m, None => return vec![] };
                self.response_id = msg.get("id").and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty()).map(String::from)
                    .unwrap_or_else(|| gen_responses_id("resp"));
                self.created = now_unix();
                if let Some(u) = msg.get("usage") { self.usage.merge(u); }
                self.started = true;
                let s1 = self.next_seq();
                let created = json!({
                    "type": "response.created", "sequence_number": s1,
                    "response": {"id": self.response_id, "object": "response", "created_at": self.created,
                        "status": "in_progress", "model": self.model, "output": []}
                });
                let s2 = self.next_seq();
                let inprog = json!({
                    "type": "response.in_progress", "sequence_number": s2,
                    "response": {"id": self.response_id, "object": "response", "created_at": self.created, "status": "in_progress"}
                });
                vec![("response.created".into(), created), ("response.in_progress".into(), inprog)]
            }
            "content_block_start" => {
                let index = ev.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                let cb = ev.get("content_block");
                let btype = cb.and_then(|c| c.get("type")).and_then(|v| v.as_str()).unwrap_or("text");
                let output_index = self.next_output_index;
                self.next_output_index += 1;
                match btype {
                    "tool_use" => {
                        let call_id = cb.and_then(|c| c.get("id")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let name = cb.and_then(|c| c.get("name")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let item_id = gen_responses_id("fc");
                        self.blocks.insert(index, BlockState { kind: "tool", item_id: item_id.clone(), output_index, call_id: call_id.clone(), name: name.clone(), buf: String::new() });
                        let s = self.next_seq();
                        let item = json!({
                            "type": "response.output_item.added", "sequence_number": s, "output_index": output_index,
                            "item": {"id": item_id, "type": "function_call", "status": "in_progress", "arguments": "", "call_id": call_id, "name": name}
                        });
                        vec![("response.output_item.added".into(), item)]
                    }
                    "thinking" => {
                        // reasoning:发 item + summary_part（不带 encrypted_content,签名不兼容）。
                        let item_id = gen_responses_id("rs");
                        self.blocks.insert(index, BlockState { kind: "reasoning", item_id: item_id.clone(), output_index, call_id: String::new(), name: String::new(), buf: String::new() });
                        let s1 = self.next_seq();
                        let item = json!({
                            "type": "response.output_item.added", "sequence_number": s1, "output_index": output_index,
                            "item": {"id": item_id, "type": "reasoning", "status": "in_progress", "summary": []}
                        });
                        let s2 = self.next_seq();
                        let part = json!({
                            "type": "response.reasoning_summary_part.added", "sequence_number": s2,
                            "item_id": self.blocks[&index].item_id, "output_index": output_index, "summary_index": 0,
                            "part": {"type": "summary_text", "text": ""}
                        });
                        vec![("response.output_item.added".into(), item), ("response.reasoning_summary_part.added".into(), part)]
                    }
                    _ => {
                        // text/message
                        let item_id = gen_responses_id("msg");
                        self.blocks.insert(index, BlockState { kind: "text", item_id: item_id.clone(), output_index, call_id: String::new(), name: String::new(), buf: String::new() });
                        let s1 = self.next_seq();
                        let item = json!({
                            "type": "response.output_item.added", "sequence_number": s1, "output_index": output_index,
                            "item": {"id": item_id, "type": "message", "status": "in_progress", "content": [], "role": "assistant"}
                        });
                        let s2 = self.next_seq();
                        let part = json!({
                            "type": "response.content_part.added", "sequence_number": s2,
                            "item_id": self.blocks[&index].item_id, "output_index": output_index, "content_index": 0,
                            "part": {"type": "output_text", "text": "", "annotations": []}
                        });
                        vec![("response.output_item.added".into(), item), ("response.content_part.added".into(), part)]
                    }
                }
            }
            "content_block_delta" => {
                let index = ev.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                let delta = match ev.get("delta") { Some(d) => d, None => return vec![] };
                // 先取 item_id/output_index(不可变),累加缓冲用可变借用,避免同时借用冲突。
                let (item_id, output_index) = match self.blocks.get(&index) {
                    Some(b) => (b.item_id.clone(), b.output_index),
                    None => return vec![],
                };
                match delta.get("type").and_then(|v| v.as_str()) {
                    Some("text_delta") => {
                        let t = delta.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if let Some(b) = self.blocks.get_mut(&index) { b.buf.push_str(t); }
                        let s = self.next_seq();
                        vec![("response.output_text.delta".into(), json!({
                            "type": "response.output_text.delta", "sequence_number": s,
                            "item_id": item_id, "output_index": output_index, "content_index": 0, "delta": t
                        }))]
                    }
                    Some("input_json_delta") => {
                        let pj = delta.get("partial_json").and_then(|v| v.as_str()).unwrap_or("");
                        if let Some(b) = self.blocks.get_mut(&index) { b.buf.push_str(pj); }
                        let s = self.next_seq();
                        vec![("response.function_call_arguments.delta".into(), json!({
                            "type": "response.function_call_arguments.delta", "sequence_number": s,
                            "item_id": item_id, "output_index": output_index, "delta": pj
                        }))]
                    }
                    Some("thinking_delta") => {
                        let t = delta.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                        if let Some(b) = self.blocks.get_mut(&index) { b.buf.push_str(t); }
                        let s = self.next_seq();
                        vec![("response.reasoning_summary_text.delta".into(), json!({
                            "type": "response.reasoning_summary_text.delta", "sequence_number": s,
                            "item_id": item_id, "output_index": output_index, "summary_index": 0, "delta": t
                        }))]
                    }
                    _ => vec![],
                }
            }
            "content_block_stop" => {
                let index = ev.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                let bs = match self.blocks.remove(&index) { Some(b) => b, None => return vec![] };
                self.close_block(bs)
            }
            "message_delta" => {
                if let Some(sr) = ev.get("delta").and_then(|d| d.get("stop_reason")).and_then(|v| v.as_str()) {
                    self.stop_reason = sr.to_string();
                }
                if let Some(u) = ev.get("usage") { self.usage.merge(u); }
                vec![]
            }
            "message_stop" => {
                // 完成:发 response.completed（含 usage + status + **回填的 output 数组**)。
                let (p, c, t, cached) = self.usage.openai();
                let status = if self.stop_reason == "max_tokens" { "incomplete" } else { "completed" };
                let output = self.build_output();
                let s = self.next_seq();
                // 骨架补全 background:false / error:null(Codex 严格解析器期望完整骨架,见参考 response.go)。
                let mut resp = json!({
                    "id": self.response_id, "object": "response", "created_at": self.created,
                    "status": status, "background": false, "error": Value::Null,
                    "model": self.model, "output": output,
                    "usage": {"input_tokens": p, "output_tokens": c, "total_tokens": t,
                        "input_tokens_details": {"cached_tokens": cached}}
                });
                if status == "incomplete" {
                    resp["incomplete_details"] = json!({"reason": "max_output_tokens"});
                }
                vec![("response.completed".into(), json!({
                    "type": "response.completed", "sequence_number": s, "response": resp
                }))]
            }
            "error" => {
                if let Some(e) = ev.get("error") {
                    let msg = e.get("message").and_then(|v| v.as_str()).unwrap_or("upstream error");
                    let s = self.next_seq();
                    return vec![("response.failed".into(), json!({
                        "type": "response.failed", "sequence_number": s,
                        "response": {"id": self.response_id, "status": "failed", "error": {"message": msg}}
                    }))];
                }
                vec![]
            }
            _ => vec![],
        }
    }

    /// 关闭一个块:按类型发 .done + output_item.done,**回填累加的全量内容**(严格客户端从终态取权威)。
    /// 同时把块快照存进 self.finished,供 message_stop 重建 response.completed.output。
    fn close_block(&mut self, bs: BlockState) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        match bs.kind {
            "tool" => {
                let args = if bs.buf.is_empty() { "{}".to_string() } else { bs.buf.clone() };
                let s1 = self.next_seq();
                out.push(("response.function_call_arguments.done".into(), json!({
                    "type": "response.function_call_arguments.done", "sequence_number": s1,
                    "item_id": bs.item_id, "output_index": bs.output_index, "arguments": args
                })));
                let s2 = self.next_seq();
                out.push(("response.output_item.done".into(), json!({
                    "type": "response.output_item.done", "sequence_number": s2, "output_index": bs.output_index,
                    "item": {"id": bs.item_id, "type": "function_call", "status": "completed",
                        "call_id": bs.call_id, "name": bs.name, "arguments": args}
                })));
                self.finished.push(FinishedItem { kind: "tool", item_id: bs.item_id, call_id: bs.call_id, name: bs.name, text: if bs.buf.is_empty() { "{}".into() } else { bs.buf } });
            }
            "reasoning" => {
                let text = bs.buf.clone();
                // 补 summary_text.done(带 text),再 part.done(带 part.text),再 item.done(summary 回填)。
                let s0 = self.next_seq();
                out.push(("response.reasoning_summary_text.done".into(), json!({
                    "type": "response.reasoning_summary_text.done", "sequence_number": s0,
                    "item_id": bs.item_id, "output_index": bs.output_index, "summary_index": 0, "text": text
                })));
                let s1 = self.next_seq();
                out.push(("response.reasoning_summary_part.done".into(), json!({
                    "type": "response.reasoning_summary_part.done", "sequence_number": s1,
                    "item_id": bs.item_id, "output_index": bs.output_index, "summary_index": 0,
                    "part": {"type": "summary_text", "text": text}
                })));
                let s2 = self.next_seq();
                out.push(("response.output_item.done".into(), json!({
                    "type": "response.output_item.done", "sequence_number": s2, "output_index": bs.output_index,
                    "item": {"id": bs.item_id, "type": "reasoning", "status": "completed",
                        "summary": [{"type": "summary_text", "text": text}]}
                })));
                self.finished.push(FinishedItem { kind: "reasoning", item_id: bs.item_id, call_id: String::new(), name: String::new(), text: bs.buf });
            }
            _ => {
                let text = bs.buf.clone();
                let s1 = self.next_seq();
                out.push(("response.output_text.done".into(), json!({
                    "type": "response.output_text.done", "sequence_number": s1,
                    "item_id": bs.item_id, "output_index": bs.output_index, "content_index": 0, "text": text
                })));
                let s2 = self.next_seq();
                out.push(("response.content_part.done".into(), json!({
                    "type": "response.content_part.done", "sequence_number": s2,
                    "item_id": bs.item_id, "output_index": bs.output_index, "content_index": 0,
                    "part": {"type": "output_text", "text": text, "annotations": []}
                })));
                let s3 = self.next_seq();
                out.push(("response.output_item.done".into(), json!({
                    "type": "response.output_item.done", "sequence_number": s3, "output_index": bs.output_index,
                    "item": {"id": bs.item_id, "type": "message", "status": "completed", "role": "assistant",
                        "content": [{"type": "output_text", "text": text, "annotations": []}]}
                })));
                self.finished.push(FinishedItem { kind: "text", item_id: bs.item_id, call_id: String::new(), name: String::new(), text: bs.buf });
            }
        }
        out
    }

    /// 用已闭合块快照重建 response.completed 的 output 数组(严格客户端把它当最终权威内容)。
    fn build_output(&self) -> Vec<Value> {
        self.finished.iter().map(|f| match f.kind {
            "tool" => json!({"id": f.item_id, "type": "function_call", "status": "completed",
                "call_id": f.call_id, "name": f.name, "arguments": f.text}),
            "reasoning" => json!({"id": f.item_id, "type": "reasoning", "status": "completed",
                "summary": [{"type": "summary_text", "text": f.text}]}),
            _ => json!({"id": f.item_id, "type": "message", "status": "completed", "role": "assistant",
                "content": [{"type": "output_text", "text": f.text, "annotations": []}]}),
        }).collect()
    }
}

/// 非流式:内部 Anthropic 事件序列 → 单个 Responses response 对象。
pub fn aggregate_responses(model: &str, events: &[Value]) -> Value {
    let mut response_id = String::new();
    let mut created = now_unix();
    let mut usage = UsageTokens::default();
    let mut stop_reason = String::from("end_turn");
    // 按出现顺序收集 output items。
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tools: Vec<(String, String, String)> = Vec::new(); // (call_id, name, args)
    let mut cur_tool: Option<(String, String, String)> = None;

    for ev in events {
        match ev.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "message_start" => {
                if let Some(m) = ev.get("message") {
                    response_id = m.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    created = now_unix();
                    if let Some(u) = m.get("usage") { usage.merge(u); }
                }
            }
            "content_block_start" => {
                if ev.get("content_block").and_then(|c| c.get("type")).and_then(|v| v.as_str()) == Some("tool_use") {
                    let cb = ev.get("content_block");
                    let id = cb.and_then(|c| c.get("id")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = cb.and_then(|c| c.get("name")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                    cur_tool = Some((id, name, String::new()));
                }
            }
            "content_block_delta" => {
                if let Some(d) = ev.get("delta") {
                    match d.get("type").and_then(|v| v.as_str()) {
                        Some("text_delta") => { if let Some(t) = d.get("text").and_then(|v| v.as_str()) { text.push_str(t); } }
                        Some("thinking_delta") => { if let Some(t) = d.get("thinking").and_then(|v| v.as_str()) { reasoning.push_str(t); } }
                        Some("input_json_delta") => { if let (Some(pj), Some(ct)) = (d.get("partial_json").and_then(|v| v.as_str()), cur_tool.as_mut()) { ct.2.push_str(pj); } }
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                if let Some(t) = cur_tool.take() { tools.push(t); }
            }
            "message_delta" => {
                if let Some(sr) = ev.get("delta").and_then(|d| d.get("stop_reason")).and_then(|v| v.as_str()) { stop_reason = sr.to_string(); }
                if let Some(u) = ev.get("usage") { usage.merge(u); }
            }
            _ => {}
        }
    }

    // 组 output 数组:reasoning → message → function_calls(顺序对齐 Responses 惯例)。
    let mut output: Vec<Value> = Vec::new();
    if !reasoning.is_empty() {
        output.push(json!({"id": gen_responses_id("rs"), "type": "reasoning", "status": "completed",
            "summary": [{"type": "summary_text", "text": reasoning}]}));
    }
    if !text.is_empty() {
        output.push(json!({"id": gen_responses_id("msg"), "type": "message", "status": "completed", "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}]}));
    }
    for (call_id, name, args) in &tools {
        let a = if args.is_empty() { "{}" } else { args.as_str() };
        output.push(json!({"id": gen_responses_id("fc"), "type": "function_call", "status": "completed",
            "call_id": call_id, "name": name, "arguments": a}));
    }

    let (p, c, t, cached) = usage.openai();
    let status = if stop_reason == "max_tokens" { "incomplete" } else { "completed" };
    let id = if response_id.is_empty() { gen_responses_id("resp") } else { response_id };
    // 骨架补全 background:false / error:null(与流式 response.completed 对齐,Codex 期望完整骨架)。
    let mut resp = json!({
        "id": id, "object": "response", "created_at": created, "status": status,
        "background": false, "error": Value::Null, "model": model,
        "output": output,
        "usage": {"input_tokens": p, "output_tokens": c, "total_tokens": t, "input_tokens_details": {"cached_tokens": cached}}
    });
    if status == "incomplete" {
        resp["incomplete_details"] = json!({"reason": "max_output_tokens"});
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_basic_roles_and_system() {
        let raw = json!({
            "model": "gpt-5.6-sol",
            "messages": [
                {"role": "system", "content": "you are helpful"},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"}
            ]
        });
        let a = openai_chat_to_anthropic("gpt-5.6-sol", &raw, true);
        assert_eq!(a["model"], "gpt-5.6-sol");
        assert_eq!(a["stream"], true);
        assert_eq!(a["max_tokens"], 32000); // 默认
        // system 抽到顶层
        assert_eq!(a["system"][0]["type"], "text");
        assert_eq!(a["system"][0]["text"], "you are helpful");
        // user/assistant 进 messages
        assert_eq!(a["messages"][0]["role"], "user");
        assert_eq!(a["messages"][0]["content"][0]["text"], "hi");
        assert_eq!(a["messages"][1]["role"], "assistant");
    }

    #[test]
    fn test_request_max_tokens_precedence() {
        let raw = json!({"model": "m", "max_completion_tokens": 100, "max_tokens": 50, "messages": []});
        let a = openai_chat_to_anthropic("m", &raw, false);
        assert_eq!(a["max_tokens"], 100, "max_completion_tokens 优先");
        let raw2 = json!({"model": "m", "max_tokens": 50, "messages": []});
        assert_eq!(openai_chat_to_anthropic("m", &raw2, false)["max_tokens"], 50);
    }

    #[test]
    fn test_request_assistant_tool_calls_to_tool_use() {
        // 配对(assistant tool_use + 后续 tool_result),否则悬空 tool_use 会被归一丢弃。
        let raw = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "天气?"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc", "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"city\":\"NYC\"}"}
                    }]
                },
                {"role": "tool", "tool_call_id": "call_abc", "content": "sunny"}
            ]
        });
        let a = openai_chat_to_anthropic("m", &raw, false);
        // messages[1] = assistant(含 tool_use)
        let content = &a["messages"][1]["content"];
        let tu = content.as_array().unwrap().iter().find(|b| b["type"] == "tool_use").unwrap();
        assert_eq!(tu["id"], "call_abc");
        assert_eq!(tu["name"], "get_weather");
        assert_eq!(tu["input"]["city"], "NYC");
    }

    #[test]
    fn test_request_tool_role_to_tool_result() {
        // 配对输入(assistant 先声明 tool_use call_abc)。
        let raw = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "q"},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_abc", "type": "function", "function": {"name": "f", "arguments": "{}"}
                }]},
                {"role": "tool", "tool_call_id": "call_abc", "content": "sunny"}
            ]
        });
        let a = openai_chat_to_anthropic("m", &raw, false);
        // messages[2] 是 user(tool_result)
        let block = &a["messages"][2]["content"][0];
        assert_eq!(a["messages"][2]["role"], "user");
        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["tool_use_id"], "call_abc");
        assert_eq!(block["content"], "sunny");
    }

    #[test]
    fn test_request_empty_tool_result_becomes_placeholder() {
        let raw = json!({"model": "m", "messages": [
            {"role": "user", "content": "q"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "c", "type": "function", "function": {"name": "f", "arguments": "{}"}
            }]},
            {"role": "tool", "tool_call_id": "c", "content": ""}
        ]});
        let a = openai_chat_to_anthropic("m", &raw, false);
        // messages[2] = user(tool_result),空内容 → "(empty)"
        assert_eq!(a["messages"][2]["content"][0]["content"], "(empty)");
    }

    #[test]
    fn test_request_tool_schema_normalization() {
        // 缺 properties 的 object schema 要补 properties
        let raw = json!({
            "model": "m", "messages": [],
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {"type": "object"}}}]
        });
        let a = openai_chat_to_anthropic("m", &raw, false);
        assert_eq!(a["tools"][0]["input_schema"]["properties"], json!({}));
        // 完全无 parameters → 空 object schema
        let raw2 = json!({"model": "m", "messages": [], "tools": [{"type": "function", "function": {"name": "f"}}]});
        let a2 = openai_chat_to_anthropic("m", &raw2, false);
        assert_eq!(a2["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn test_request_tool_choice_mapping() {
        // tool_choice 只在有 tools 时才下发,故构造时带一个 tool。
        let mk = |tc: Value| {
            let raw = json!({
                "model": "m", "messages": [], "tool_choice": tc,
                "tools": [{"type": "function", "function": {"name": "f"}}]
            });
            openai_chat_to_anthropic("m", &raw, false)
        };
        assert_eq!(mk(json!("auto"))["tool_choice"]["type"], "auto");
        assert_eq!(mk(json!("required"))["tool_choice"]["type"], "any");
        assert_eq!(mk(json!("none"))["tool_choice"]["type"], "none");
        let forced = mk(json!({"type": "function", "function": {"name": "f"}}));
        assert_eq!(forced["tool_choice"]["type"], "tool");
        assert_eq!(forced["tool_choice"]["name"], "f");
    }

    #[test]
    fn test_request_system_only_appends_empty_user() {
        let raw = json!({"model": "m", "messages": [{"role": "system", "content": "hi"}]});
        let a = openai_chat_to_anthropic("m", &raw, false);
        assert_eq!(a["messages"].as_array().unwrap().len(), 1);
        assert_eq!(a["messages"][0]["role"], "user");
    }

    #[test]
    fn test_request_image_url_data_uri() {
        let raw = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,QUJD"}}
            ]}]
        });
        let a = openai_chat_to_anthropic("m", &raw, false);
        let block = &a["messages"][0]["content"][0];
        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["type"], "base64");
        assert_eq!(block["source"]["media_type"], "image/jpeg");
        assert_eq!(block["source"]["data"], "QUJD");
    }

    #[test]
    fn test_stream_text_flow() {
        let mut conv = ChatStreamConverter::new("gpt-5.6-sol");
        let start = conv.push_event(&json!({"type": "message_start", "message": {"id": "msg_1", "usage": {"input_tokens": 10}}}));
        assert_eq!(start[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(start[0]["id"], "msg_1");
        let d = conv.push_event(&json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hi"}}));
        assert_eq!(d[0]["choices"][0]["delta"]["content"], "Hi");
        let md = conv.push_event(&json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}));
        // OpenAI 规范:finish_reason chunk 与 usage chunk 分开;usage chunk 的 choices 为空。
        assert_eq!(md[0]["choices"][0]["finish_reason"], "stop");
        assert!(md[0].get("usage").is_none(), "finish_reason chunk 不带 usage");
        assert_eq!(md[1]["choices"], json!([]), "usage chunk 的 choices 为空数组");
        assert_eq!(md[1]["usage"]["completion_tokens"], 5);
    }

    #[test]
    fn test_stream_tool_call_buffered_until_stop() {
        let mut conv = ChatStreamConverter::new("m");
        conv.push_event(&json!({"type": "message_start", "message": {"id": "m1"}}));
        // tool_use 开始:不吐 chunk
        let s = conv.push_event(&json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t1", "name": "f"}}));
        assert!(s.is_empty());
        // 增量 args:不吐(缓冲)
        let d1 = conv.push_event(&json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"a\":"}}));
        assert!(d1.is_empty());
        let d2 = conv.push_event(&json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "1}"}}));
        assert!(d2.is_empty());
        // stop:一次性吐完整 tool_call
        let stop = conv.push_event(&json!({"type": "content_block_stop", "index": 0}));
        let tc = &stop[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["id"], "t1");
        assert_eq!(tc["function"]["name"], "f");
        assert_eq!(tc["function"]["arguments"], "{\"a\":1}");
    }

    #[test]
    fn test_usage_cache_reallocation() {
        // OpenAI prompt_tokens 含 cache;Anthropic input 不含
        let mut u = UsageTokens::default();
        u.merge(&json!({"input_tokens": 100, "cache_read_input_tokens": 30, "cache_creation_input_tokens": 20, "output_tokens": 40}));
        let (p, c, t, cached) = u.openai();
        assert_eq!(p, 150); // 100+30+20
        assert_eq!(c, 40);
        assert_eq!(t, 190);
        assert_eq!(cached, 30);
    }

    #[test]
    fn test_aggregate_nonstream() {
        let events = vec![
            json!({"type": "message_start", "message": {"id": "msg_x", "usage": {"input_tokens": 5}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hello "}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "world"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}),
        ];
        let c = aggregate_chat_completion("gpt-5.6-sol", &events);
        assert_eq!(c["object"], "chat.completion");
        assert_eq!(c["choices"][0]["message"]["content"], "Hello world");
        assert_eq!(c["choices"][0]["finish_reason"], "stop");
        assert_eq!(c["usage"]["prompt_tokens"], 5);
        assert_eq!(c["usage"]["completion_tokens"], 3);
    }

    #[test]
    fn test_map_stop_reason() {
        assert_eq!(map_stop_reason("end_turn"), "stop");
        assert_eq!(map_stop_reason("tool_use"), "tool_calls");
        assert_eq!(map_stop_reason("max_tokens"), "length");
        assert_eq!(map_stop_reason("stop_sequence"), "stop");
    }

    #[test]
    fn test_stream_tool_index_is_sequential_not_block_index() {
        // 回归(review HIGH):思考关时首个 text 块占 content-block index 0,工具从 1 起。
        // OpenAI tool_calls index 必须是 0-based 连续位序,否则 SDK 造幻影空工具。
        let mut conv = ChatStreamConverter::new("m");
        conv.push_event(&json!({"type": "message_start", "message": {"id": "m1"}}));
        // 先来个 text 块(index 0)
        conv.push_event(&json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}));
        conv.push_event(&json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}));
        conv.push_event(&json!({"type": "content_block_stop", "index": 0}));
        // 第一个 tool_use 在 content-block index 1
        conv.push_event(&json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "t1", "name": "f"}}));
        let stop1 = conv.push_event(&json!({"type": "content_block_stop", "index": 1}));
        assert_eq!(stop1[0]["choices"][0]["delta"]["tool_calls"][0]["index"], 0, "首个工具应是 OpenAI index 0,非 content-block 1");
        // 第二个 tool_use 在 content-block index 2 → OpenAI index 1
        conv.push_event(&json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "t2", "name": "g"}}));
        let stop2 = conv.push_event(&json!({"type": "content_block_stop", "index": 2}));
        assert_eq!(stop2[0]["choices"][0]["delta"]["tool_calls"][0]["index"], 1);
    }

    #[test]
    fn test_temperature_dropped_when_thinking_enabled() {
        // 回归(review):thinking 开启时不透传 temperature(Anthropic 只接受 1,透传非1值→400)。
        let raw = json!({"model": "m", "messages": [], "reasoning_effort": "high", "temperature": 0.5});
        let a = openai_chat_to_anthropic("m", &raw, false);
        assert_eq!(a["thinking"]["type"], "enabled");
        assert!(a.get("temperature").is_none(), "thinking 开时不应透传 temperature");
        // thinking 关时正常透传。
        let raw2 = json!({"model": "m", "messages": [], "temperature": 0.5});
        assert_eq!(openai_chat_to_anthropic("m", &raw2, false)["temperature"], 0.5);
    }

    #[test]
    fn test_normalize_drops_orphan_tool_result() {
        // 无对应 tool_use 的孤儿 tool_result 应被丢弃(丢空后整条 user 消失)。
        let msgs = vec![
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "nonexistent", "content": "x"}]}),
        ];
        let out = normalize_tool_pairing_and_merge(msgs);
        assert!(out.is_empty(), "孤儿 tool_result 丢弃后无消息");
    }

    #[test]
    fn test_normalize_drops_unanswered_tool_use() {
        // 悬空 tool_use(无对应 tool_result)丢弃;保留文本。
        let msgs = vec![
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "hi"},
                {"type": "tool_use", "id": "t1", "name": "f", "input": {}}
            ]}),
        ];
        let out = normalize_tool_pairing_and_merge(msgs);
        // 首条本是 assistant(丢悬空 tool_use 后剩 text)→ 补空 user 打头。
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["role"], "user");
        let content = out[1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1, "悬空 tool_use 丢弃,只剩 text");
        assert_eq!(content[0]["type"], "text");
    }

    #[test]
    fn test_normalize_prepends_user_when_assistant_first() {
        // 回归(review):窗口化对话开头 orphan tool_result 被丢后首条成 assistant → 补空 user 打头,
        // 否则 Anthropic「首条必须 user」400。
        let msgs = vec![
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "trimmed", "content": "r"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "hello"}]}),
        ];
        let out = normalize_tool_pairing_and_merge(msgs);
        // orphan tool_result 丢弃 → 首条本会是 assistant → 补空 user。
        assert_eq!(out[0]["role"], "user", "首条必须 user");
        assert_eq!(out[1]["role"], "assistant");
    }

    #[test]
    fn test_request_temperature_clamped_to_anthropic_range() {
        // 回归(review P0):OpenAI temperature∈[0,2],Anthropic∈[0,1];透传路径 >1 会 400。
        // clamp 到 [0,1](2.0→1.0)。
        let raw = json!({"model": "m", "messages": [], "temperature": 1.7});
        let a = openai_chat_to_anthropic("m", &raw, false);
        assert_eq!(a["temperature"], 1.0, "temperature 1.7 应 clamp 到 1.0");
        // 范围内不动。
        let raw2 = json!({"model": "m", "messages": [], "temperature": 0.3});
        assert_eq!(openai_chat_to_anthropic("m", &raw2, false)["temperature"], 0.3);
        // responses 路径同样 clamp。
        let raw3 = json!({"model": "m", "input": "x", "temperature": 2.0});
        assert_eq!(openai_responses_to_anthropic("m", &raw3, false)["temperature"], 1.0);
    }

    #[test]
    fn test_request_stop_empty_string_filtered() {
        // 回归(review P0):空串 stop sequence 被 Anthropic 拒绝(透传路径 400),须过滤。
        // 数组含空串:只留非空。
        let raw = json!({"model": "m", "messages": [], "stop": ["", "END", ""]});
        let a = openai_chat_to_anthropic("m", &raw, false);
        assert_eq!(a["stop_sequences"], json!(["END"]));
        // 全空数组:不下发 stop_sequences。
        let raw2 = json!({"model": "m", "messages": [], "stop": ["", ""]});
        assert!(openai_chat_to_anthropic("m", &raw2, false).get("stop_sequences").is_none());
        // 单个空串:不下发。
        let raw3 = json!({"model": "m", "messages": [], "stop": ""});
        assert!(openai_chat_to_anthropic("m", &raw3, false).get("stop_sequences").is_none());
    }

    #[test]
    fn test_request_fully_empty_still_gets_user_message() {
        // 回归(review P1):整体空请求(无 messages 无 system)也要补空 user,防 Anthropic
        // 「messages 非空」400(此前只在有 system 时才补)。
        let raw = json!({"model": "m", "messages": []});
        let a = openai_chat_to_anthropic("m", &raw, false);
        let msgs = a["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1, "空请求应补一条空 user");
        assert_eq!(msgs[0]["role"], "user");
        // responses:无 input 无 instructions 同样兜底。
        let raw2 = json!({"model": "m"});
        let a2 = openai_responses_to_anthropic("m", &raw2, false);
        assert_eq!(a2["messages"].as_array().unwrap().len(), 1);
        assert_eq!(a2["messages"][0]["role"], "user");
    }

    #[test]
    fn test_aggregate_content_null_when_only_tool_calls() {
        // 回归(review P1):assistant 只返回 tool_calls(无文本)时 content 应为 null,非空串。
        let events = vec![
            json!({"type": "message_start", "message": {"id": "m1"}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t1", "name": "f"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{}"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
        ];
        let c = aggregate_chat_completion("m", &events);
        assert!(c["choices"][0]["message"]["content"].is_null(), "只有 tool_calls 时 content 应为 null");
        assert_eq!(c["choices"][0]["finish_reason"], "tool_calls");
        // 有文本时 content 仍是字符串(不误伤)。
        let events2 = vec![
            json!({"type": "message_start", "message": {"id": "m2"}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        ];
        let c2 = aggregate_chat_completion("m", &events2);
        assert_eq!(c2["choices"][0]["message"]["content"], "hi");
    }

    #[test]
    fn test_request_response_format_json_object() {
        // response_format json_object → 追加一段要求纯 JSON 的 system 指令(不覆盖用户 system)。
        let raw = json!({
            "model": "m",
            "messages": [{"role": "system", "content": "be terse"}, {"role": "user", "content": "hi"}],
            "response_format": {"type": "json_object"}
        });
        let a = openai_chat_to_anthropic("m", &raw, false);
        let sys = a["system"].as_array().unwrap();
        assert_eq!(sys.len(), 2, "用户 system + JSON 指令");
        assert_eq!(sys[0]["text"], "be terse");
        assert!(sys[1]["text"].as_str().unwrap().contains("valid JSON"));
        // type:text 不注入。
        let raw2 = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}], "response_format": {"type": "text"}});
        let a2 = openai_chat_to_anthropic("m", &raw2, false);
        assert!(a2.get("system").is_none(), "text 格式不注入 system");
    }

    #[test]
    fn test_request_response_format_json_schema_embeds_schema() {
        let raw = json!({
            "model": "m", "messages": [{"role": "user", "content": "hi"}],
            "response_format": {"type": "json_schema", "json_schema": {
                "name": "Weather", "schema": {"type": "object", "properties": {"temp": {"type": "number"}}}
            }}
        });
        let a = openai_chat_to_anthropic("m", &raw, false);
        let instr = a["system"][0]["text"].as_str().unwrap();
        assert!(instr.contains("Weather"), "指令带 schema 名");
        assert!(instr.contains("\"temp\""), "指令内嵌 schema 内容");
    }

    #[test]
    fn test_request_top_p_dropped_when_thinking_enabled() {
        // 回归(review):thinking 开启时 top_p 也不透传(与 temperature 同,Anthropic thinking 模式
        // 不接受非默认采样参数,透传路径 400)。此前只 gate 了 temperature、漏了 top_p。
        let raw = json!({"model": "m", "messages": [], "reasoning_effort": "high", "top_p": 0.9, "temperature": 0.5});
        let a = openai_chat_to_anthropic("m", &raw, false);
        assert_eq!(a["thinking"]["type"], "enabled");
        assert!(a.get("top_p").is_none(), "thinking 开时不应透传 top_p");
        assert!(a.get("temperature").is_none(), "thinking 开时不应透传 temperature");
        // thinking 关时 top_p 正常透传。
        let raw2 = json!({"model": "m", "messages": [], "top_p": 0.9});
        assert_eq!(openai_chat_to_anthropic("m", &raw2, false)["top_p"], 0.9);
        // responses 路径同样 gate。
        let raw3 = json!({"model": "m", "input": "x", "reasoning": {"effort": "high"}, "top_p": 0.5});
        let a3 = openai_responses_to_anthropic("m", &raw3, false);
        assert!(a3.get("top_p").is_none(), "responses thinking 开时不应透传 top_p");
    }

    #[test]
    fn test_request_tool_choice_dropped_without_tools() {
        // 回归(review):tool_choice 无 tools 时不下发(Anthropic「有 tool_choice 无 tools」400)。
        // 客户端发 tool_choice 但无 tools。
        let raw = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}], "tool_choice": "auto"});
        let a = openai_chat_to_anthropic("m", &raw, false);
        assert!(a.get("tool_choice").is_none(), "无 tools 时不应下发 tool_choice");
        assert!(a.get("tools").is_none());
        // tools 全被过滤(非 function 类型)→ 同样不下发 tool_choice。
        let raw2 = json!({
            "model": "m", "messages": [{"role": "user", "content": "hi"}],
            "tool_choice": "required", "tools": [{"type": "code_interpreter"}]
        });
        let a2 = openai_chat_to_anthropic("m", &raw2, false);
        assert!(a2.get("tools").is_none(), "非 function 工具被过滤");
        assert!(a2.get("tool_choice").is_none(), "工具全过滤后不下发 tool_choice");
        // responses 路径同样。
        let raw3 = json!({"model": "m", "input": "hi", "tool_choice": "auto"});
        let a3 = openai_responses_to_anthropic("m", &raw3, false);
        assert!(a3.get("tool_choice").is_none(), "responses 无 tools 时不下发 tool_choice");
    }

    #[test]
    fn test_responses_custom_tool_call_maps_to_tool_use() {
        // Codex 用 custom_tool_call/custom_tool_call_output(apply_patch 类自定义工具),需与
        // function_call 一样翻成 tool_use/tool_result。input 是自由文本(非 JSON object)→ 包进 {"input":...}。
        let raw = json!({
            "model": "m",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "patch it"}]},
                {"type": "custom_tool_call", "call_id": "ct1", "name": "apply_patch", "input": "*** Begin Patch\n..."},
                {"type": "custom_tool_call_output", "call_id": "ct1", "output": "applied"}
            ]
        });
        let a = openai_responses_to_anthropic("m", &raw, false);
        let msgs = a["messages"].as_array().unwrap();
        let tu = msgs[1]["content"].as_array().unwrap().iter().find(|b| b["type"] == "tool_use").unwrap();
        assert_eq!(tu["id"], "ct1");
        assert_eq!(tu["name"], "apply_patch");
        // 自由文本 input 包进 {"input": <原文>}(Anthropic input 必须 object)。
        assert_eq!(tu["input"]["input"], "*** Begin Patch\n...");
        // output → tool_result
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "ct1");
        assert_eq!(msgs[2]["content"][0]["content"], "applied");
        // custom_tool_call 的 input 若恰是合法 JSON object,则原样用(不套壳)。
        let raw2 = json!({"model": "m", "input": [
            {"type": "custom_tool_call", "call_id": "c2", "name": "f", "input": "{\"k\":1}"},
            {"type": "custom_tool_call_output", "call_id": "c2", "output": "ok"}
        ]});
        let a2 = openai_responses_to_anthropic("m", &raw2, false);
        // 首条 assistant(tool_use)会被 normalize 补空 user 打头,故跨消息找 tool_use。
        let tu2 = a2["messages"].as_array().unwrap().iter()
            .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
            .find(|b| b["type"] == "tool_use").unwrap();
        assert_eq!(tu2["input"]["k"], 1, "合法 JSON object input 原样用");
    }

    #[test]
    fn test_responses_completed_skeleton_has_background_and_error() {
        // Codex 严格解析器期望 response.completed 完整骨架(background:false + error:null)。
        // 流式:
        let mut conv = ResponsesStreamConverter::new("m");
        conv.push_event(&json!({"type": "message_start", "message": {"id": "r1"}}));
        conv.push_event(&json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}));
        conv.push_event(&json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}));
        conv.push_event(&json!({"type": "content_block_stop", "index": 0}));
        let done = conv.push_event(&json!({"type": "message_stop"}));
        let resp = &done[0].1["response"];
        assert_eq!(resp["background"], false);
        assert!(resp["error"].is_null());
        assert_eq!(resp["status"], "completed");
        // 非流式:
        let events = vec![
            json!({"type": "message_start", "message": {"id": "r2"}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "x"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        ];
        let r = aggregate_responses("m", &events);
        assert_eq!(r["background"], false);
        assert!(r["error"].is_null());
    }

    #[test]
    fn test_responses_text_format_json_object() {
        // Responses API 原生 text.format 路径。
        let raw = json!({"model": "m", "input": "hi", "text": {"format": {"type": "json_object"}}});
        let a = openai_responses_to_anthropic("m", &raw, false);
        assert!(a["system"][0]["text"].as_str().unwrap().contains("valid JSON"));
        // 也认 response_format(部分客户端仍发)。
        let raw2 = json!({"model": "m", "input": "hi", "response_format": {"type": "json_object"}});
        let a2 = openai_responses_to_anthropic("m", &raw2, false);
        assert!(a2["system"][0]["text"].as_str().unwrap().contains("valid JSON"));
    }

    #[test]
    fn test_normalize_keeps_paired_tool_and_merges() {
        // 配对的 tool_use+tool_result 保留;连续同角色合并。开头 user 提问(真实对话形态)。
        let msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "q"}]}),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "f", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "next"}]}),
        ];
        let out = normalize_tool_pairing_and_merge(msgs);
        // user(q) + assistant(tool_use) + 合并后的 user(tool_result + text)
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[1]["content"][0]["type"], "tool_use");
        assert_eq!(out[2]["role"], "user");
        assert_eq!(out[2]["content"].as_array().unwrap().len(), 2, "两条 user 合并");
    }

    // ===== /v1/responses 测试 =====

    #[test]
    fn test_responses_request_string_input() {
        let raw = json!({"model": "gpt-5.6-sol", "input": "hello", "instructions": "be brief"});
        let a = openai_responses_to_anthropic("gpt-5.6-sol", &raw, true);
        assert_eq!(a["model"], "gpt-5.6-sol");
        assert_eq!(a["stream"], true);
        assert_eq!(a["system"][0]["text"], "be brief");
        assert_eq!(a["messages"][0]["role"], "user");
        assert_eq!(a["messages"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn test_responses_request_array_input_with_function_call() {
        let raw = json!({
            "model": "m",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "q"}]},
                {"type": "function_call", "call_id": "fc1", "name": "get", "arguments": "{\"k\":1}"},
                {"type": "function_call_output", "call_id": "fc1", "output": "done"}
            ]
        });
        let a = openai_responses_to_anthropic("m", &raw, false);
        let msgs = a["messages"].as_array().unwrap();
        // user(q) + assistant(tool_use fc1) + user(tool_result fc1)
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        let tu = msgs[1]["content"].as_array().unwrap().iter().find(|b| b["type"] == "tool_use").unwrap();
        assert_eq!(tu["id"], "fc1");
        assert_eq!(tu["name"], "get");
        assert_eq!(tu["input"]["k"], 1);
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["content"], "done");
    }

    #[test]
    fn test_responses_request_max_output_tokens() {
        let raw = json!({"model": "m", "input": "x", "max_output_tokens": 500});
        assert_eq!(openai_responses_to_anthropic("m", &raw, false)["max_tokens"], 500);
    }

    #[test]
    fn test_responses_stream_text_lifecycle() {
        let mut conv = ResponsesStreamConverter::new("m");
        let start = conv.push_event(&json!({"type": "message_start", "message": {"id": "resp_1"}}));
        assert_eq!(start[0].0, "response.created");
        assert_eq!(start[1].0, "response.in_progress");
        // text 块生命周期
        let cbs = conv.push_event(&json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}));
        assert_eq!(cbs[0].0, "response.output_item.added");
        assert_eq!(cbs[1].0, "response.content_part.added");
        let d = conv.push_event(&json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hi"}}));
        assert_eq!(d[0].0, "response.output_text.delta");
        assert_eq!(d[0].1["delta"], "Hi");
        let stop = conv.push_event(&json!({"type": "content_block_stop", "index": 0}));
        assert_eq!(stop[0].0, "response.output_text.done");
        assert_eq!(stop[0].1["text"], "Hi", "output_text.done 必须回填全量 text");
        assert_eq!(stop[2].0, "response.output_item.done");
        assert_eq!(stop[2].1["item"]["content"][0]["text"], "Hi", "message item 回填 content");
        conv.push_event(&json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}));
        let done = conv.push_event(&json!({"type": "message_stop"}));
        assert_eq!(done[0].0, "response.completed");
        assert_eq!(done[0].1["response"]["status"], "completed");
        // response.completed.output 必须回填(非空壳)
        let output = done[0].1["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["content"][0]["text"], "Hi", "completed.output 回填全量内容");
        // sequence_number 单调递增
        let s_created = start[0].1["sequence_number"].as_i64().unwrap();
        let s_completed = done[0].1["response"].get("id").map(|_| ()).and(done[0].1["sequence_number"].as_i64()).unwrap();
        assert!(s_completed > s_created);
    }

    #[test]
    fn test_responses_stream_function_call_lifecycle() {
        let mut conv = ResponsesStreamConverter::new("m");
        conv.push_event(&json!({"type": "message_start", "message": {"id": "r1"}}));
        let added = conv.push_event(&json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t1", "name": "f"}}));
        assert_eq!(added[0].0, "response.output_item.added");
        assert_eq!(added[0].1["item"]["type"], "function_call");
        assert_eq!(added[0].1["item"]["call_id"], "t1");
        let d = conv.push_event(&json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"a\":1}"}}));
        assert_eq!(d[0].0, "response.function_call_arguments.delta");
        let stop = conv.push_event(&json!({"type": "content_block_stop", "index": 0}));
        assert_eq!(stop[0].0, "response.function_call_arguments.done");
        assert_eq!(stop[0].1["arguments"], "{\"a\":1}", "arguments.done 必须回填全量参数");
        assert_eq!(stop[1].0, "response.output_item.done");
        assert_eq!(stop[1].1["item"]["arguments"], "{\"a\":1}", "function_call item 回填 arguments");
        assert_eq!(stop[1].1["item"]["call_id"], "t1");
        // completed.output 回填 function_call
        let done = conv.push_event(&json!({"type": "message_stop"}));
        let output = done[0].1["response"]["output"].as_array().unwrap();
        assert_eq!(output[0]["type"], "function_call");
        assert_eq!(output[0]["arguments"], "{\"a\":1}");
    }

    #[test]
    fn test_responses_aggregate_nonstream() {
        let events = vec![
            json!({"type": "message_start", "message": {"id": "resp_x", "usage": {"input_tokens": 5}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hello"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
        ];
        let r = aggregate_responses("m", &events);
        assert_eq!(r["object"], "response");
        assert_eq!(r["status"], "completed");
        let msg = r["output"].as_array().unwrap().iter().find(|o| o["type"] == "message").unwrap();
        assert_eq!(msg["content"][0]["text"], "Hello");
        assert_eq!(r["usage"]["input_tokens"], 5);
        assert_eq!(r["usage"]["output_tokens"], 2);
    }

}
