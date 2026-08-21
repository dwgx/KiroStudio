//! Kiro 请求类型定义
//!
//! 定义 Kiro API 的主请求结构

use serde::{Deserialize, Serialize};

use super::conversation::ConversationState;

/// Kiro API 请求
///
/// 用于构建发送给 Kiro API 的请求
///
/// # 示例
///
/// ```rust
/// use kirostudio::kiro::model::requests::{
///     KiroRequest, ConversationState, CurrentMessage, UserInputMessage, Tool
/// };
///
/// // 创建简单请求
/// let state = ConversationState::new("conv-123")
///     .with_agent_task_type("vibe")
///     .with_current_message(CurrentMessage::new(
///         UserInputMessage::new("Hello", "claude-3-5-sonnet")
///     ));
///
/// let request = KiroRequest::new(state);
/// let json = request.to_json().unwrap();
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroRequest {
    /// 对话状态
    pub conversation_state: ConversationState,
    /// Profile ARN（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,
    /// Kiro 上游附加模型请求字段（AWS Q 的 `additionalModelRequestFields`）。
    ///
    /// native extended thinking 走这里：`{"output_config":{"effort":"xhigh"}}` 是
    /// 实测能触发上游 `reasoningContentEvent` 的最小形态（见 converter.rs 的
    /// `build_additional_model_request_fields` 说明）。None 时整键不出现在线上。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_model_request_fields: Option<AdditionalModelRequestFields>,
}

/// 顶层附加模型请求字段容器（`additionalModelRequestFields`）。
///
/// ⚠️ 线上格式：外层 `additionalModelRequestFields` 是 camelCase（随
/// [`KiroRequest`] 的 `rename_all`），**内层 `output_config` 保持 snake_case**，
/// 与真实 Kiro CLI 流量一致（见本文件测试 `test_additional_model_request_fields_wire_format`）。
/// 所以本结构体**不能**继承 `rename_all = "camelCase"`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AdditionalModelRequestFields {
    /// Claude 族：`output_config.effort`（Kiro 原生 reasoning 通道）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<KiroOutputConfig>,
    /// GPT 族：`reasoning.effort`（与 Claude `output_config` 并列，互斥使用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<KiroReasoningConfig>,
}

/// effort 控制字段（上游认五档：`low / medium / high / xhigh / max`）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KiroOutputConfig {
    pub effort: String,
}

/// GPT 族 reasoning 通道（`additionalModelRequestFields.reasoning.effort`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KiroReasoningConfig {
    pub effort: String,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_kiro_request_deserialize() {
        let json = r#"{
            "conversationState": {
                "conversationId": "conv-456",
                "currentMessage": {
                    "userInputMessage": {
                        "content": "Test message",
                        "modelId": "claude-3-5-sonnet",
                        "userInputMessageContext": {}
                    }
                }
            }
        }"#;

        let request: KiroRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.conversation_state.conversation_id, "conv-456");
        assert_eq!(
            request
                .conversation_state
                .current_message
                .user_input_message
                .content,
            "Test message"
        );
        // 旧客户端 JSON 没有 additionalModelRequestFields 键 → 必须反序列化且为 None
        // （防未来有人把 Option 字段改成非 Option 或移除 skip 导致旧请求 400）。
        assert!(request.additional_model_request_fields.is_none());
    }

    #[test]
    fn test_kiro_request_deserializes_additional_fields_when_present() {
        let json = r#"{
            "conversationState": {
                "conversationId": "conv-1",
                "currentMessage": {
                    "userInputMessage": {
                        "content": "hi",
                        "modelId": "claude-opus-4-8",
                        "userInputMessageContext": {}
                    }
                }
            },
            "additionalModelRequestFields": {
                "output_config": {"effort": "xhigh"}
            }
        }"#;
        let request: KiroRequest = serde_json::from_str(json).unwrap();
        assert_eq!(
            request
                .additional_model_request_fields
                .expect("带 additionalModelRequestFields 时应解析出来")
                .output_config
                .expect("output_config 应解析出来")
                .effort,
            "xhigh"
        );
    }

    #[test]
    fn test_additional_model_request_fields_wire_format() {
        // 线上格式：外层键 camelCase（additionalModelRequestFields），内层键保持
        // snake_case（output_config），与真实 Kiro CLI 流量一致。
        let fields = AdditionalModelRequestFields {
            output_config: Some(KiroOutputConfig {
                effort: "max".to_string(),
            }),
            reasoning: None,
        };
        let v = serde_json::to_value(&fields).unwrap();
        assert_eq!(v["output_config"]["effort"], "max");
        assert!(
            v.get("outputConfig").is_none(),
            "内层键必须保持 snake_case output_config，实际: {v}"
        );
        assert!(v.get("reasoning").is_none(), "未设 GPT reasoning 时不得出键");

        let gpt = AdditionalModelRequestFields {
            output_config: None,
            reasoning: Some(KiroReasoningConfig {
                effort: "high".to_string(),
            }),
        };
        let v = serde_json::to_value(&gpt).unwrap();
        assert_eq!(v["reasoning"]["effort"], "high");
        assert!(v.get("output_config").is_none());

        // KiroRequest 顶层：camelCase 键 + None 时整键缺席。
        let request = KiroRequest {
            conversation_state: ConversationState::new("conv-789"),
            profile_arn: None,
            additional_model_request_fields: Some(fields),
        };
        let v = serde_json::to_value(&request).unwrap();
        assert_eq!(
            v["additionalModelRequestFields"]["output_config"]["effort"],
            "max"
        );
        let without = KiroRequest {
            conversation_state: ConversationState::new("conv-789"),
            profile_arn: None,
            additional_model_request_fields: None,
        };
        let v = serde_json::to_value(&without).unwrap();
        assert!(
            v.get("additionalModelRequestFields").is_none(),
            "None 时 additionalModelRequestFields 整键不得出现"
        );
    }
}
