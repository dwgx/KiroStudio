//! 上游 `metadataEvent`
//!
//! EXP-0 实测：本链路 payload **只有** `stopReason`，从不带 `tokenUsage` /
//! cache 真值（见 `docs/CACHE-EXP0-RESULT.md`）。此前落 Unknown，终止信号被丢掉，
//! 干净 EOF 只能靠本地推断出 `end_turn`。

use serde::{Deserialize, Serialize};

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 流末元数据。`stop_reason` 缺省 / 空串视为「未给出终止信号」。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetadataEvent {
    #[serde(default, rename = "stopReason", alias = "stop_reason")]
    pub stop_reason: Option<String>,
}

impl EventPayload for MetadataEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        if frame.payload.is_empty() {
            return Ok(Self::default());
        }
        frame.payload_as_json()
    }
}

/// 映射为 Anthropic `stop_reason`。空 / 缺字段 → `None`（未给出终止信号，走既有推断）。
/// 未知非空值 → `end_turn`（已经收到真实 metadata 帧，流是完整的）。
pub(crate) fn map_metadata_stop_reason(raw: Option<&str>) -> Option<String> {
    let trimmed = raw.map(str::trim).filter(|s| !s.is_empty())?;
    let key = trimmed.to_ascii_lowercase().replace('-', "_");
    Some(match key.as_str() {
        "end_turn" | "endturn" => "end_turn".into(),
        "max_tokens" | "maxtokens" | "length" => "max_tokens".into(),
        "stop_sequence" | "stopsequence" => "stop_sequence".into(),
        "tool_use" | "tooluse" => "tool_use".into(),
        "pause_turn" | "pauseturn" => "pause_turn".into(),
        "refusal" => "refusal".into(),
        "model_context_window_exceeded" | "modelcontextwindowexceeded" => {
            "model_context_window_exceeded".into()
        }
        _ => "end_turn".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::events::Event;
    use crate::kiro::parser::crc::crc32;
    use crate::kiro::parser::frame::{parse_frame, Frame, PRELUDE_SIZE};

    fn build_test_frame(event_type: &str, payload: &str) -> Frame {
        const NAME: &[u8] = b":event-type";
        let mut headers = Vec::new();
        headers.push(NAME.len() as u8);
        headers.extend_from_slice(NAME);
        headers.push(7u8);
        headers.extend_from_slice(&(event_type.len() as u16).to_be_bytes());
        headers.extend_from_slice(event_type.as_bytes());

        let header_length = headers.len() as u32;
        let total_length = (PRELUDE_SIZE + headers.len() + payload.len() + 4) as u32;

        let mut buf = Vec::new();
        buf.extend_from_slice(&total_length.to_be_bytes());
        buf.extend_from_slice(&header_length.to_be_bytes());
        let prelude_crc = crc32(&buf[..8]);
        buf.extend_from_slice(&prelude_crc.to_be_bytes());
        buf.extend_from_slice(&headers);
        buf.extend_from_slice(payload.as_bytes());
        let msg_crc = crc32(&buf);
        buf.extend_from_slice(&msg_crc.to_be_bytes());

        let (frame, _consumed) = parse_frame(&buf).expect("parse frame").expect("frame");
        frame
    }

    #[test]
    fn parses_camel_case_stop_reason() {
        let e: MetadataEvent =
            serde_json::from_str(r#"{"stopReason":"max_tokens"}"#).expect("camelCase");
        assert_eq!(e.stop_reason.as_deref(), Some("max_tokens"));
    }

    #[test]
    fn parses_snake_case_alias() {
        let e: MetadataEvent =
            serde_json::from_str(r#"{"stop_reason":"end_turn"}"#).expect("alias");
        assert_eq!(e.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn missing_field_is_absent_not_error() {
        let e: MetadataEvent = serde_json::from_str("{}").expect("empty object");
        assert!(e.stop_reason.is_none());
        assert!(map_metadata_stop_reason(e.stop_reason.as_deref()).is_none());
    }

    #[test]
    fn empty_string_is_absent() {
        assert!(map_metadata_stop_reason(Some("  ")).is_none());
        assert!(map_metadata_stop_reason(None).is_none());
    }

    #[test]
    fn unknown_nonempty_folds_to_end_turn() {
        assert_eq!(
            map_metadata_stop_reason(Some("somethingElse")).as_deref(),
            Some("end_turn")
        );
    }

    #[test]
    fn maps_known_anthropic_reasons() {
        assert_eq!(
            map_metadata_stop_reason(Some("max_tokens")).as_deref(),
            Some("max_tokens")
        );
        assert_eq!(
            map_metadata_stop_reason(Some("pause_turn")).as_deref(),
            Some("pause_turn")
        );
        assert_eq!(
            map_metadata_stop_reason(Some("refusal")).as_deref(),
            Some("refusal")
        );
        assert_eq!(
            map_metadata_stop_reason(Some("MAX_TOKENS")).as_deref(),
            Some("max_tokens")
        );
    }

    #[test]
    fn from_frame_parses_metadata_event() {
        let frame = build_test_frame("metadataEvent", r#"{"stopReason":"max_tokens"}"#);
        match Event::from_frame(frame).expect("from_frame") {
            Event::Metadata(m) => {
                assert_eq!(m.stop_reason.as_deref(), Some("max_tokens"));
                assert_eq!(
                    map_metadata_stop_reason(m.stop_reason.as_deref()).as_deref(),
                    Some("max_tokens")
                );
            }
            other => panic!("expected Metadata, got {other:?}"),
        }
    }
}
