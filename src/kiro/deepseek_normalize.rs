//! deepseek 协议归一化 —— 把 fuckopencode 的核心转换逻辑套进 custom_api 透传。
//!
//! 背景:opencodezen 类 deepseek 兼容网关只认 `deepseek-v4-flash` 且对标准 Anthropic
//! 客户端的字段很挑剔(实测多个 400)。fuckopencode(OpenAI↔Anthropic 协议网关)把这些
//! 坑逐一修掉了。本模块把那套修复逻辑用 Rust 复刻,供 custom_api 透传在转发前调用——
//! 这样 KiroStudio 识别到 opencodezen 凭据(`deepseekNormalize=true`)时,请求先归一化再转发。
//!
//! ⚠️ **范围边界（诚实标注，勿当 overclaim）**：本模块只做**请求侧**归一化（模型名/thinking/
//! reasoning_effort/工具字段）。**响应侧**未实现 `filterThinkingFromStream`（fuckopencode 在
//! thinking disabled 时仍会剥掉上游吐的 thinking 块，否则 Claude Code 报 "Tool result missing"；
//! KiroStudio 的 passthrough 是字节流原样回流，未过滤）。因此说"兼容性等价于直接走 fuckopencode"
//! 只对请求侧成立；响应侧在 thinking disabled 场景可能仍需客户端容忍 thinking 块。
//!
//! 已知 deepseek 坑(见 fuckopencode/src/deepseek.ts):
//! - **模型名**：只认 `deepseek-v4-flash` 等精确名，`claude-*` 被上游 401，须重写。
//! - `thinking:{type:"adaptive"}` + `budget_tokens` → 400,必须归一化
//!   成 `{type:"enabled"|"disabled"}` 并去掉 budget_tokens。
//! - `thinking:disabled` 时若仍带 `reasoning_effort` → 400,须连 effort 一起删。
//! - `reasoning_effort` 需转成 `output_config.effort`；非字符串的 effort 直接删。
//! - `context_management`、工具上的 `strict/defer_loading` → 400 "Extra inputs",须剥离。
//! - **多轮带工具**：thinking 非 disabled 时，assistant 历史消息含 `tool_use` 而无 `thinking`
//!   块 → 次轮间歇 400（deepseek 要求回传 reasoning 内容），须注入空 thinking 块。

use serde_json::Value;

/// opencodezen 只认的 fallback 模型名（对齐 fuckopencode `DEFAULT_FALLBACK_MODEL`）。
const DEFAULT_FALLBACK_MODEL: &str = "deepseek-v4-flash";

/// thinking 开启时 `max_tokens` 的下限。
///
/// 实测根因（2026-08-08 实打 deepseek-v4-flash）：**thinking 计入 max_tokens 预算**。
/// 客户端（claudecodehaha tier-7 等）常发小 `max_tokens`（约 200），deepseek 先 thinking
/// 就把预算吃光 → `stop_reason=max_tokens`、只有 thinking、正文空；而 `max_tokens=4096`
/// 时 `stop_reason=end_turn`、thinking + 正文完整。上游接受大 max_tokens（实测 4096 OK，
/// model_catalog 的 max_output=64_000），所以抬升是安全的。仅 thinking 非 disabled 时
/// 生效；thinking disabled 尊重客户端明确的小预算。
const DEEPSEEK_MIN_MAX_TOKENS: u64 = 4096;

/// 对一次 Anthropic `/v1/messages` 请求体做 deepseek 归一化(就地修改)。
///
/// 幂等:对任意 Anthropic 请求安全,不改消息语义,只调整协议字段。
/// 非对象 / 非 JSON 结构直接忽略,不会 panic。
pub fn normalize_request(value: &mut Value) {
    let obj = match value.as_object_mut() {
        Some(o) => o,
        None => return,
    };

    // 0) 模型名重写：opencodezen 只认 deepseek-v4-flash 等精确名，claude-*/gpt-* 会被上游
    //    401（对齐 fuckopencode `resolveModelName`：命中 map 用映射值，否则 fallback）。
    //    KiroStudio 无 modelMap 配置，简单规则：deepseek-* 保留，其余统一映射到 fallback。
    if let Some(model) = obj.get("model").and_then(|m| m.as_str()) {
        if !model.starts_with("deepseek-") {
            obj.insert("model".into(), serde_json::json!(DEFAULT_FALLBACK_MODEL));
        }
    }

    // 1) thinking 归一化:adaptive→enabled;enabled 去 budget_tokens;disabled 保留;未知形态删除。
    let mut thinking_disabled = false;
    match obj.get("thinking") {
        Some(v) if v.is_object() => {
            match v.get("type").and_then(|x| x.as_str()) {
                Some("adaptive") | Some("enabled") => {
                    obj.insert("thinking".into(), serde_json::json!({ "type": "enabled" }));
                }
                Some("disabled") => {
                    thinking_disabled = true;
                    obj.insert("thinking".into(), serde_json::json!({ "type": "disabled" }));
                }
                _ => {
                    obj.remove("thinking");
                }
            }
        }
        Some(_) => {
            // 字符串等未知形态:deepseek 会 400,删除。
            obj.remove("thinking");
        }
        None => {}
    }

    // 2) reasoning_effort:disabled 时连 effort 一起删;否则字符串映射到 output_config.effort,
    //    非字符串(数字/布尔)deepseek 不认,直接删(残留会 Extra inputs 400)。
    if thinking_disabled {
        obj.remove("reasoning_effort");
        obj.remove("output_config");
    } else {
        if let Some(effort) = obj.get("reasoning_effort").and_then(|x| x.as_str()) {
            obj.insert("output_config".into(), serde_json::json!({ "effort": effort }));
        }
        obj.remove("reasoning_effort");
    }

    // 3) 剥 deepseek 不认的 beta 配对字段。
    obj.remove("context_management");
    if let Some(oc) = obj.get_mut("output_config").and_then(|v| v.as_object_mut()) {
        // output_config 只留 effort,其余(format/task_budget 等)会 400。
        // 非字符串 effort 也删（deepseek 只认 string，别的会 Extra inputs）。
        let effort = oc.remove("effort");
        oc.clear();
        if let Some(e) = effort {
            if e.is_string() {
                oc.insert("effort".into(), e);
            }
        }
    }
    // 空 output_config（effort 缺失/非字符串被清空）deepseek 同样 400，整体删除。
    if obj
        .get("output_config")
        .and_then(|v| v.as_object())
        .is_some_and(|o| o.is_empty())
    {
        obj.remove("output_config");
    }

    // 4) 工具上的 strict/defer_loading 也会 400,剥离。
    if let Some(tools) = obj.get_mut("tools").and_then(|v| v.as_array_mut()) {
        for tool in tools {
            if let Some(t) = tool.as_object_mut() {
                t.remove("strict");
                t.remove("defer_loading");
            }
        }
    }

    // 5) 多轮带工具:thinking 非 disabled 时,assistant 历史含 tool_use 而无 thinking 块
    //    则前插空 thinking 块(否则 deepseek 次轮 400)。对齐 fuckopencode injectMissingThinkingBlocks。
    if !thinking_disabled {
        inject_missing_thinking_blocks(obj);
    }

    // 6) max_tokens 下限保护：thinking 非 disabled 时，deepseek 的 thinking 计入
    //    max_tokens 预算。客户端小预算（如 200）会被 thinking 吃光 → 正文空
    //    （实测 max_tokens=30 只有 thinking；4096 正常出正文）。这里把 < 下限的
    //    抬到下限；缺失补下限；≥ 下限保持。thinking disabled 不调（尊重客户端意图）。
    if !thinking_disabled {
        match obj.get("max_tokens").and_then(|v| v.as_u64()) {
            None => {
                obj.insert("max_tokens".into(), serde_json::json!(DEEPSEEK_MIN_MAX_TOKENS));
            }
            Some(n) if n < DEEPSEEK_MIN_MAX_TOKENS => {
                obj.insert("max_tokens".into(), serde_json::json!(DEEPSEEK_MIN_MAX_TOKENS));
            }
            Some(_) => {}
        }
    }
}

/// 对齐 fuckopencode `injectMissingThinkingBlocks`：thinking 非 disabled 时，assistant
/// 历史消息含 `tool_use` 但缺 `thinking` 块 → 在 content 头部前插空 thinking 块。
/// deepseek 在「带工具 + thinking」的多轮里要求 assistant 回传 reasoning 内容，缺失会
/// 400（间歇，取决于上游启发式检查）；Claude Code 这类客户端不回传，社区代理都注入空块。
fn inject_missing_thinking_blocks(obj: &mut serde_json::Map<String, Value>) {
    let Some(messages) = obj.get_mut("messages").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for msg in messages {
        let Some(msg_obj) = msg.as_object_mut() else { continue };
        if msg_obj.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let Some(content) = msg_obj.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        let has_tool_use = content
            .iter()
            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"));
        let has_thinking = content
            .iter()
            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("thinking"));
        if has_tool_use && !has_thinking {
            content.insert(
                0,
                serde_json::json!({ "type": "thinking", "thinking": "", "signature": "" }),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(input: serde_json::Value) -> serde_json::Value {
        let mut v = input;
        normalize_request(&mut v);
        v
    }

    #[test]
    fn adaptive_thinking_becomes_enabled_without_budget() {
        let out = norm(serde_json::json!({
            "thinking": { "type": "adaptive", "budget_tokens": 4096 }
        }));
        assert_eq!(out["thinking"], serde_json::json!({ "type": "enabled" }));
        assert!(out.get("budget_tokens").is_none());
    }

    #[test]
    fn enabled_thinking_keeps_type_drops_budget() {
        let out = norm(serde_json::json!({
            "thinking": { "type": "enabled", "budget_tokens": 1024 }
        }));
        assert_eq!(out["thinking"], serde_json::json!({ "type": "enabled" }));
    }

    #[test]
    fn disabled_thinking_strips_reasoning_effort() {
        let out = norm(serde_json::json!({
            "thinking": { "type": "disabled" },
            "reasoning_effort": "high"
        }));
        assert_eq!(out["thinking"], serde_json::json!({ "type": "disabled" }));
        assert!(out.get("reasoning_effort").is_none());
        assert!(out.get("output_config").is_none());
    }

    #[test]
    fn reasoning_effort_maps_to_output_config_effort() {
        let out = norm(serde_json::json!({ "reasoning_effort": "high" }));
        assert_eq!(out["output_config"], serde_json::json!({ "effort": "high" }));
        assert!(out.get("reasoning_effort").is_none());
    }

    #[test]
    fn strips_context_management_and_tool_extra_fields() {
        let out = norm(serde_json::json!({
            "context_management": { "enable": true },
            "tools": [
                { "name": "a", "input_schema": { "type": "object" }, "strict": true, "defer_loading": true },
                { "name": "b", "input_schema": { "type": "object" } }
            ]
        }));
        assert!(out.get("context_management").is_none());
        let tools = out["tools"].as_array().unwrap();
        assert!(tools[0].get("strict").is_none());
        assert!(tools[0].get("defer_loading").is_none());
        assert_eq!(tools[0]["name"], "a");
        assert_eq!(tools[1]["name"], "b");
    }

    #[test]
    fn non_object_body_is_noop() {
        let out = norm(serde_json::json!("just a string"));
        assert_eq!(out, serde_json::json!("just a string"));
    }

    #[test]
    fn unknown_thinking_shape_is_removed() {
        let out = norm(serde_json::json!({ "thinking": "custom-mode" }));
        assert!(out.get("thinking").is_none());
    }

    #[test]
    fn output_config_keeps_only_effort() {
        let out = norm(serde_json::json!({
            "output_config": { "effort": "max", "format": { "type": "json" }, "task_budget": 5 }
        }));
        assert_eq!(out["output_config"], serde_json::json!({ "effort": "max" }));
    }

    /// 模型名重写：非 deepseek-* 统一映射到 fallback（opencodezen 只认 flash），deepseek-* 保留。
    #[test]
    fn model_is_rewritten_to_fallback_when_not_deepseek() {
        assert_eq!(norm(serde_json::json!({ "model": "claude-sonnet-4" }))["model"], "deepseek-v4-flash");
        assert_eq!(norm(serde_json::json!({ "model": "gpt-4o" }))["model"], "deepseek-v4-flash");
        assert_eq!(norm(serde_json::json!({ "model": "deepseek-v4-flash" }))["model"], "deepseek-v4-flash");
        assert_eq!(norm(serde_json::json!({ "model": "deepseek-reasoner" }))["model"], "deepseek-reasoner");
    }

    /// 空 output_config 整体删除（非字符串 effort 被清空 / 无 effort 的 output_config 都删）。
    #[test]
    fn empty_output_config_is_removed() {
        let out = norm(serde_json::json!({ "reasoning_effort": 5 }));
        assert!(out.get("output_config").is_none(), "非字符串 effort 应被清空并删除 output_config");
        assert!(out.get("reasoning_effort").is_none());

        let out2 = norm(serde_json::json!({ "output_config": { "format": { "type": "json" } } }));
        assert!(out2.get("output_config").is_none(), "无 effort 的 output_config 应删除");
    }

    /// thinking:null（字段存在但为 null）应被删除。
    #[test]
    fn thinking_null_is_removed() {
        let out = norm(serde_json::json!({ "thinking": null }));
        assert!(out.get("thinking").is_none());
    }

    /// 多轮带工具 + thinking 开启：assistant 历史含 tool_use 缺 thinking → 前插空块；
    /// 已有 thinking 不重复；thinking disabled 不注入。
    #[test]
    fn injects_empty_thinking_for_tool_use_turns() {
        let out = norm(serde_json::json!({
            "thinking": { "type": "enabled" },
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": [
                    { "type": "text", "text": "using tool" },
                    { "type": "tool_use", "id": "t1", "name": "fs_write", "input": {} }
                ]}
            ]
        }));
        let msgs = out["messages"].as_array().unwrap();
        let assistant = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        let content = assistant["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "thinking", "tool_use 消息前应注入 thinking 块");
        assert_eq!(content[0]["thinking"], "");
        assert_eq!(content[0]["signature"], "");

        // 已有 thinking 不重复注入
        let out2 = norm(serde_json::json!({
            "thinking": { "type": "enabled" },
            "messages": [
                { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "reason", "signature": "s" },
                    { "type": "tool_use", "id": "t1", "name": "fs_write", "input": {} }
                ]}
            ]
        }));
        let content2 = out2["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content2.len(), 2, "已有 thinking 不重复注入");

        // thinking disabled 不注入
        let out3 = norm(serde_json::json!({
            "thinking": { "type": "disabled" },
            "messages": [
                { "role": "assistant", "content": [
                    { "type": "tool_use", "id": "t1", "name": "fs_write", "input": {} }
                ]}
            ]
        }));
        let content3 = out3["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content3[0]["type"], "tool_use", "thinking disabled 不注入");
    }

    /// max_tokens 下限保护（根因：deepseek thinking 计入 max_tokens，小预算被吃光 → 正文空）。
    /// thinking 非 disabled 时 < 4096 抬到 4096、缺失补 4096、≥ 保持；disabled 不调。
    #[test]
    fn max_tokens_floor_applied_only_when_thinking_enabled() {
        // thinking enabled + 小 max_tokens → 抬到 4096
        let out = norm(serde_json::json!({
            "thinking": { "type": "enabled" },
            "max_tokens": 200
        }));
        assert_eq!(out["max_tokens"], 4096, "小 max_tokens 应抬到下限");

        // thinking adaptive（归一化成 enabled）+ 小 max_tokens → 抬到 4096
        let out = norm(serde_json::json!({
            "thinking": { "type": "adaptive" },
            "max_tokens": 30
        }));
        assert_eq!(out["max_tokens"], 4096, "adaptive 归一化为 enabled 后同样抬升");

        // thinking enabled + 缺失 max_tokens → 补 4096
        let out = norm(serde_json::json!({ "thinking": { "type": "enabled" } }));
        assert_eq!(out["max_tokens"], 4096, "缺失 max_tokens 应补下限");

        // thinking enabled + ≥ 4096 → 保持
        let out = norm(serde_json::json!({
            "thinking": { "type": "enabled" },
            "max_tokens": 5000
        }));
        assert_eq!(out["max_tokens"], 5000, "≥ 下限的 max_tokens 应保持");

        // thinking disabled + 小 max_tokens → 不变（尊重客户端明确的小预算）
        let out = norm(serde_json::json!({
            "thinking": { "type": "disabled" },
            "max_tokens": 100
        }));
        assert_eq!(out["max_tokens"], 100, "thinking disabled 不抬升");

        // thinking disabled + 缺失 → 不补
        let out = norm(serde_json::json!({ "thinking": { "type": "disabled" } }));
        assert!(out.get("max_tokens").is_none(), "thinking disabled 不补 max_tokens");
    }

    /// 幂等：对已归一化的请求再归一化，结果不变。
    #[test]
    fn normalize_is_idempotent() {
        let input = serde_json::json!({
            "model": "claude-opus-4",
            "thinking": { "type": "adaptive", "budget_tokens": 4096 },
            "reasoning_effort": "high",
            "context_management": { "enable": true },
            "tools": [ { "name": "a", "strict": true } ]
        });
        let mut once = input.clone();
        normalize_request(&mut once);
        let mut twice = once.clone();
        normalize_request(&mut twice);
        assert_eq!(once, twice, "二次归一化结果不变");
    }
}
