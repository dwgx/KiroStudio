//! deepseek 协议归一化 —— 把 fuckopencode 的核心转换逻辑套进 custom_api 透传。
//!
//! 背景:opencodezen 类 deepseek 兼容网关只认 `deepseek-v4-flash` 且对标准 Anthropic
//! 客户端的字段很挑剔(实测多个 400)。fuckopencode(OpenAI↔Anthropic 协议网关)把这些
//! 坑逐一修掉了。本模块把那套修复逻辑用 Rust 复刻,供 custom_api 透传在转发前调用——
//! 这样 KiroStudio 识别到 opencodezen 凭据(`deepseekNormalize=true`)时,请求先归一化再
//! 转发,兼容性等价于直接走 fuckopencode。
//!
//! 已知 deepseek 坑(见 fuckopencode/src/deepseek.ts):
//! - `thinking:{type:"adaptive"}` + `budget_tokens` → 400,必须归一化
//!   成 `{type:"enabled"|"disabled"}` 并去掉 budget_tokens。
//! - `thinking:disabled` 时若仍带 `reasoning_effort` → 400,须连 effort 一起删。
//! - `reasoning_effort` 需转成 `output_config.effort`。
//! - `context_management`、工具上的 `strict/defer_loading` → 400 "Extra inputs",须剥离。

use serde_json::Value;

/// 对一次 Anthropic `/v1/messages` 请求体做 deepseek 归一化(就地修改)。
///
/// 幂等:对任意 Anthropic 请求安全,不改消息语义,只调整协议字段。
/// 非对象 / 非 JSON 结构直接忽略,不会 panic。
pub fn normalize_request(value: &mut Value) {
    let obj = match value.as_object_mut() {
        Some(o) => o,
        None => return,
    };

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

    // 2) reasoning_effort:disabled 时连 effort 一起删;否则映射到 output_config.effort。
    if thinking_disabled {
        obj.remove("reasoning_effort");
        obj.remove("output_config");
    } else if let Some(effort) = obj.get("reasoning_effort").and_then(|x| x.as_str()).map(str::to_string) {
        obj.insert("output_config".into(), serde_json::json!({ "effort": effort }));
        obj.remove("reasoning_effort");
    }

    // 3) 剥 deepseek 不认的 beta 配对字段。
    obj.remove("context_management");
    if let Some(oc) = obj.get_mut("output_config").and_then(|v| v.as_object_mut()) {
        // output_config 只留 effort,其余(format/task_budget 等)会 400。
        let effort = oc.remove("effort");
        oc.clear();
        if let Some(e) = effort {
            oc.insert("effort".into(), e);
        }
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
}
