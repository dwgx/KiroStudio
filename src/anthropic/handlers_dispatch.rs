//! Dual-endpoint helpers for `handlers.rs` (`#[path]` sibling).
//!
//! Parent stays a file. Compress-retry loops stay in each HTTP entry.
//! Frame decode used to exist in three copies (stream / buffered / non-stream).

use std::sync::Arc;

use axum::{Json, response::IntoResponse, response::Response};

use crate::kiro::model::events::Event;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::token;

use super::super::converter::convert_request;
use super::{
    BufferedStreamContext, CacheUsageBreakdown, ClientInfo, CompletionStatus, ErrorResponse,
    MessagesRequest, SseEvent, StreamContext, build_kiro_request_body, current_compression,
    dispatch_websearch_paths, estimate_cache_breakdown, override_thinking_from_model_name,
    prompt_cache_enabled, render_conversion_error, render_serialization_failed,
};

/// Sink for one AWS event-stream chunk. Streaming yields SSE events immediately;
/// buffered / non-stream return empty and accumulate elsewhere.
pub(super) trait FrameDecodeSink {
    fn on_event(&mut self, event: Event) -> Vec<SseEvent>;
    fn mark_decoder_stopped(&mut self, message: String);
}

impl FrameDecodeSink for StreamContext {
    fn on_event(&mut self, event: Event) -> Vec<SseEvent> {
        self.process_kiro_event(&event)
    }

    fn mark_decoder_stopped(&mut self, message: String) {
        StreamContext::mark_decoder_stopped(self, message);
    }
}

impl FrameDecodeSink for BufferedStreamContext {
    fn on_event(&mut self, event: Event) -> Vec<SseEvent> {
        self.process_and_buffer(&event);
        Vec::new()
    }

    fn mark_decoder_stopped(&mut self, message: String) {
        BufferedStreamContext::mark_decoder_stopped(self, message);
    }
}

/// Non-stream path: collect decoded events and map decoder-stop onto `completion`.
pub(super) struct NonStreamDecodeSink<'a> {
    pub events: &'a mut Vec<Event>,
    pub completion: &'a mut CompletionStatus,
}

impl FrameDecodeSink for NonStreamDecodeSink<'_> {
    fn on_event(&mut self, event: Event) -> Vec<SseEvent> {
        self.events.push(event);
        Vec::new()
    }

    fn mark_decoder_stopped(&mut self, message: String) {
        if self.completion.is_ok() {
            *self.completion = CompletionStatus::DecoderStopped { message };
        }
    }
}

/// Drain one chunk (or a whole body) through the AWS event-stream decoder.
///
/// Shared by streaming, buffered, and non-stream (3→1). Caller still emits the
/// streaming inline SSE error after `decoder.is_stopped()` — buffered waits until
/// stream end.
pub(super) fn decode_frames_into<C: FrameDecodeSink>(
    decoder: &mut EventStreamDecoder,
    chunk: &[u8],
    ctx: &mut C,
) -> Vec<SseEvent> {
    if let Err(e) = decoder.feed(chunk) {
        tracing::warn!("缓冲区溢出: {}", e);
    }

    let mut events = Vec::new();
    let mut last_decode_err: Option<String> = None;
    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => {
                // from_frame 按值吞 frame，事件类型须在 move 前先拥有化捕获。
                let et = frame.event_type().map(|s| s.to_string());
                match Event::from_frame(frame) {
                    Ok(event) => {
                        events.extend(ctx.on_event(event));
                    }
                    Err(err) => {
                        if et.as_deref() == Some("toolUseEvent") {
                            tracing::warn!(
                                "toolUseEvent 帧解析失败,按响应截断处理: {}",
                                err
                            );
                            ctx.mark_decoder_stopped(format!(
                                "toolUseEvent 帧解析失败: {}",
                                err
                            ));
                        } else {
                            tracing::warn!(
                                "事件帧解析失败(event_type={:?}),已忽略: {}",
                                et.as_deref(),
                                err
                            );
                        }
                    }
                }
            }
            Err(e) => {
                last_decode_err = Some(e.to_string());
                tracing::warn!("解码事件失败: {}", e);
            }
        }
    }

    if decoder.is_stopped() {
        ctx.mark_decoder_stopped(
            last_decode_err.unwrap_or_else(|| "解码器连续错误已停止".to_string()),
        );
    }
    events
}

/// Converted Kiro body + maps shared by `/v1` and `/cc/v1` after path-specific
/// parse / passthrough. Compress-retry loops stay in the entry points.
pub(super) struct KiroDispatchPrep {
    pub payload: MessagesRequest,
    pub request_body: String,
    pub conv_state_for_compress_retry:
        crate::kiro::model::requests::conversation::ConversationState,
    pub native_fields_for_compress_retry:
        Option<crate::kiro::model::requests::kiro::AdditionalModelRequestFields>,
    pub input_tokens: i32,
    pub cache_breakdown: Option<CacheUsageBreakdown>,
    pub fingerprint_usage: Option<crate::anthropic::cache::PromptCacheUsage>,
    pub thinking_enabled: bool,
    pub tool_name_map: std::collections::HashMap<String, String>,
    pub known_tool_names: std::collections::HashSet<String>,
    pub tool_required_fields: std::collections::HashMap<String, Vec<String>>,
}

/// Thinking override, websearch, convert, compress-build, token/cache maps.
/// `Err` is an early HTTP response (websearch / convert / serialize).
pub(super) async fn prepare_kiro_dispatch(
    mut payload: MessagesRequest,
    provider: &Arc<crate::kiro::provider::KiroProvider>,
    retry_budget: &crate::kiro::provider::SharedRetryBudget,
    client: &ClientInfo,
) -> Result<KiroDispatchPrep, Response> {
    override_thinking_from_model_name(&mut payload);

    if let Some(resp) = dispatch_websearch_paths(provider, &payload, retry_budget, client).await {
        return Err(resp);
    }

    let conversion_result = match convert_request(&payload) {
        Ok(result) => result,
        Err(e) => {
            let (status, error_type, message) = render_conversion_error(&e);
            tracing::warn!("请求转换失败: {}", e);
            return Err((status, Json(ErrorResponse::new(error_type, message))).into_response());
        }
    };

    let conv_state_for_compress_retry = conversion_result.conversation_state.clone();
    let native_fields_for_compress_retry =
        conversion_result.additional_model_request_fields.clone();
    let request_body = match build_kiro_request_body(
        conversion_result.conversation_state,
        conversion_result.additional_model_request_fields,
        &current_compression(),
        None,
    ) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return Err(render_serialization_failed(&e));
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    let input_tokens = token::count_all_tokens(
        &payload.model,
        payload.system.as_deref(),
        &payload.messages,
        payload.tools.as_deref(),
    ) as i32;

    let prefix_tokens = token::count_prefix_tokens(payload.system.as_deref(), &payload.messages);
    let fingerprint_usage = prompt_cache_enabled()
        .then(|| crate::anthropic::cache_fingerprint::compute_fingerprint_usage(&payload))
        .flatten();
    let cache_breakdown = fingerprint_usage
        .map(|u| u.clamp_to_total(input_tokens).to_cache_breakdown())
        .or_else(|| estimate_cache_breakdown(prompt_cache_enabled(), prefix_tokens, input_tokens));

    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    Ok(KiroDispatchPrep {
        payload,
        request_body,
        conv_state_for_compress_retry,
        native_fields_for_compress_retry,
        input_tokens,
        cache_breakdown,
        fingerprint_usage,
        thinking_enabled,
        tool_name_map: conversion_result.tool_name_map,
        known_tool_names: conversion_result.known_tool_names,
        tool_required_fields: conversion_result.tool_required_fields,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingSink {
        events: usize,
        stopped: Option<String>,
    }

    impl FrameDecodeSink for RecordingSink {
        fn on_event(&mut self, _event: Event) -> Vec<SseEvent> {
            self.events += 1;
            Vec::new()
        }

        fn mark_decoder_stopped(&mut self, message: String) {
            self.stopped = Some(message);
        }
    }

    #[test]
    fn empty_chunk_yields_no_events_and_does_not_stop() {
        let mut decoder = EventStreamDecoder::new();
        let mut sink = RecordingSink {
            events: 0,
            stopped: None,
        };
        let sse = decode_frames_into(&mut decoder, &[], &mut sink);
        assert!(sse.is_empty());
        assert_eq!(sink.events, 0);
        assert!(sink.stopped.is_none());
        assert!(!decoder.is_stopped());
    }
}
