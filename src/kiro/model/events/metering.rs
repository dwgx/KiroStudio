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
    }

    #[test]
    fn test_metering_event_defaults() {
        let event: MeteringEvent = serde_json::from_str("{}").unwrap();
        assert_eq!(event.unit, "");
        assert_eq!(event.usage, 0.0);
    }

    #[test]
    fn test_metering_event_display() {
        let event = MeteringEvent {
            unit: "credit".to_string(),
            unit_plural: "credits".to_string(),
            usage: 2.25,
        };
        assert_eq!(format!("{event}"), "2.250000 credit");
    }
}
