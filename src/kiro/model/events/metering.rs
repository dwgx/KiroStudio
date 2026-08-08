//! 计费事件
//!
//! 处理 meteringEvent 类型的事件，解析上游返回的真实 credit 消耗量。
//!
//! 移植自 BenedictKing/kiro.rs（MIT，Copyright kiro.rs contributors），
//! 用于把上游 `meteringEvent` 携带的真实计费量接入本项目的用量统计链路。

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 计费事件
///
/// 上游在响应流末尾返回本次请求消耗的计费量（当前单位为 `credit`）。
/// 这是唯一携带**真实** credit 消耗的事件，token 估算无法替代。
///
/// ⚠️ 上游 metering 事件可选携带真实的 cache_read/cache_creation token 数
/// （Layer 1 真值，k2cc 同款）：字段缺失（`None`）时降级到本地 prefix 估算。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeteringEvent {
    /// 计费单位，当前固定为 `credit`
    #[serde(default)]
    pub unit: String,
    /// 计费单位复数，当前固定为 `credits`
    #[serde(default)]
    pub unit_plural: String,
    /// 本次请求消耗量
    #[serde(default)]
    pub usage: f64,
    /// 命中缓存读取的 token 数（上游真值，缺失为 None）
    #[serde(default)]
    pub cache_read_input_tokens: Option<i32>,
    /// 本次新建缓存写入的 token 数（上游真值，缺失为 None）
    #[serde(default)]
    pub cache_creation_input_tokens: Option<i32>,
}

impl EventPayload for MeteringEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

impl std::fmt::Display for MeteringEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.unit.is_empty() {
            write!(f, "{:.6}", self.usage)
        } else {
            write!(f, "{:.6} {}", self.usage, self.unit)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metering_event_deserialize() {
        let json = r#"{"unit":"credit","unitPlural":"credits","usage":1.5}"#;
        let event: MeteringEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.unit, "credit");
        assert_eq!(event.unit_plural, "credits");
        assert_eq!(event.usage, 1.5);
        assert_eq!(event.cache_read_input_tokens, None);
        assert_eq!(event.cache_creation_input_tokens, None);
    }

    #[test]
    fn test_metering_event_defaults() {
        let event: MeteringEvent = serde_json::from_str("{}").unwrap();
        assert_eq!(event.unit, "");
        assert_eq!(event.usage, 0.0);
        assert_eq!(event.cache_read_input_tokens, None);
        assert_eq!(event.cache_creation_input_tokens, None);
    }

    #[test]
    fn test_metering_event_parses_cache_fields() {
        // 上游 metering 事件可选携带 cache 真值（Layer 1）——必须能解析。
        let json = r#"{"usage":1.5,"cacheReadInputTokens":600,"cacheCreationInputTokens":200}"#;
        let event: MeteringEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.cache_read_input_tokens, Some(600));
        assert_eq!(event.cache_creation_input_tokens, Some(200));
    }

    #[test]
    fn test_metering_event_display() {
        let event = MeteringEvent {
            unit: "credit".to_string(),
            unit_plural: "credits".to_string(),
            usage: 2.25,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        };
        assert_eq!(format!("{event}"), "2.250000 credit");
    }
}
