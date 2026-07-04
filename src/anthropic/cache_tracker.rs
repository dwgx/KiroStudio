//! 网关侧本地影子 prompt 缓存记账
//!
//! Kiro 上游不回传 Anthropic 的 prompt caching 记账字段
//! （`cache_read_input_tokens` / `cache_creation_input_tokens`），但下游客户端
//! （Claude Code 等）期望响应 `usage` 里带这些字段来显示缓存命中情况。
//!
//! 本模块在网关侧维护「按凭据分桶的前缀滚动指纹 + TTL」缓存表：请求进来时
//! 按 Anthropic 缓存语义把 tools / system / messages 摊平成有序块，逐块累计
//! 滚动 SHA256 前缀指纹；命中已存指纹的最长前缀即视为 cache_read，其余新增
//! 部分视为 cache_creation。请求成功后把本次的可缓存断点写回缓存表。
//!
//! **这是估算/影子记账，不是真实计费**：真实计费仍以 meteringEvent 的 credits
//! 为准（见 `crate::kiro::model::events::metering`）。此处只为向下游客户端复现
//! Anthropic 风格的缓存 usage 展示。
//!
//! 前缀指纹的关键性质：任一前缀块变化会级联改变其后所有块的指纹，忠实复刻
//! Anthropic「前缀完全一致才可复用」的语义。

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::token::{
    count_message_content_tokens, count_system_message_tokens, count_tool_definition_tokens,
};

use super::types::{CacheControl, Message, MessagesRequest};

const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(300);
const ONE_HOUR_CACHE_TTL: Duration = Duration::from_secs(3600);
const PREFIX_LOOKBACK_LIMIT: usize = 10;

/// 单次请求推算出的缓存记账结果
#[derive(Debug, Clone, Copy, Default)]
pub struct CacheResult {
    pub cache_read_input_tokens: i32,
    pub cache_creation_input_tokens: i32,
    pub cache_creation_5m_input_tokens: i32,
    pub cache_creation_1h_input_tokens: i32,
}

/// 一次请求摊平后的缓存画像（块序列 + 断点）
#[derive(Debug, Clone)]
pub struct CacheProfile {
    total_input_tokens: i32,
    min_cacheable_tokens: i32,
    blocks: Vec<CacheBlock>,
    breakpoints: Vec<CacheBreakpoint>,
}

#[derive(Debug, Clone)]
struct CacheBlock {
    prefix_fingerprint: [u8; 32],
    cumulative_tokens: i32,
}

#[derive(Debug, Clone)]
struct CacheBreakpoint {
    block_index: usize,
    ttl: Duration,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    #[allow(dead_code)]
    token_count: i32,
    ttl: Duration,
    expires_at: Instant,
}

struct CachedCheckpointStore {
    by_credential: HashMap<u64, HashMap<[u8; 32], CacheEntry>>,
}

/// 影子缓存跟踪器：按凭据分桶存前缀指纹 → 缓存条目
pub struct CacheTracker {
    entries: Mutex<CachedCheckpointStore>,
    max_supported_ttl: Duration,
}

impl CacheTracker {
    pub fn new(max_supported_ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(CachedCheckpointStore {
                by_credential: HashMap::new(),
            }),
            max_supported_ttl,
        }
    }

    /// 把请求摊平成有序块序列并逐块算滚动前缀指纹，产出缓存画像。
    ///
    /// `total_input_tokens` 为外部（远程/本地）算出的输入 token 总数，用于给
    /// 命中/新增 token 数封顶，避免估算块 token 之和超过真实总数。
    pub fn build_profile(
        &self,
        payload: &MessagesRequest,
        total_input_tokens: i32,
    ) -> CacheProfile {
        let flattened = flatten_cacheable_blocks(payload);

        // 与 prompt 内容无关但会影响官方缓存可复用性的固定配置。
        let request_prelude = canonicalize_json(serde_json::json!({
            "model": payload.model,
            "tool_choice": payload.tool_choice,
        }));
        let prelude_bytes = serde_json::to_vec(&request_prelude).unwrap_or_default();
        let mut prefix_hasher = Sha256::new();
        prefix_hasher.update((prelude_bytes.len() as u64).to_be_bytes());
        prefix_hasher.update(&prelude_bytes);

        let mut blocks = Vec::with_capacity(flattened.len());
        let mut breakpoints = Vec::new();
        let mut cumulative_tokens = 0i32;

        let mut active_ttl: Option<Duration> = None;
        let mut seen_breakpoints: std::collections::BTreeSet<usize> =
            std::collections::BTreeSet::new();

        for (index, block) in flattened.into_iter().enumerate() {
            cumulative_tokens = cumulative_tokens.saturating_add(block.tokens);

            let block_bytes = serde_json::to_vec(&block.value).unwrap_or_default();
            let block_hash: [u8; 32] = Sha256::digest(&block_bytes).into();

            let mut next_prefix_hasher = prefix_hasher.clone();
            next_prefix_hasher.update(block_hash);
            let prefix_fingerprint: [u8; 32] = next_prefix_hasher.finalize().into();
            prefix_hasher = Sha256::new();
            prefix_hasher.update(prefix_fingerprint);

            blocks.push(CacheBlock {
                prefix_fingerprint,
                cumulative_tokens,
            });

            if let Some(ttl) = block.breakpoint_ttl {
                let ttl = ttl.min(self.max_supported_ttl);
                active_ttl = Some(ttl);
                if seen_breakpoints.insert(index) {
                    breakpoints.push(CacheBreakpoint {
                        block_index: index,
                        ttl,
                    });
                }
            }

            if block.is_message_end
                && block.message_index.is_some()
                && let Some(ttl) = active_ttl
                && seen_breakpoints.insert(index)
            {
                breakpoints.push(CacheBreakpoint {
                    block_index: index,
                    ttl,
                });
            }
        }

        CacheProfile {
            total_input_tokens: total_input_tokens.max(0),
            min_cacheable_tokens: minimum_cacheable_tokens_for_model(&payload.model),
            blocks,
            breakpoints,
        }
    }

    /// 根据缓存表推算本次请求的 cache_read / cache_creation（不写回）。
    pub fn compute(&self, credential_id: u64, profile: &CacheProfile) -> CacheResult {
        let Some(last_breakpoint) = profile.last_cacheable_breakpoint() else {
            return CacheResult::default();
        };
        let last_breakpoint_tokens = last_breakpoint
            .cumulative_tokens
            .min(profile.total_input_tokens);

        let now = Instant::now();
        let mut entries = self.entries.lock();
        prune_expired(&mut entries.by_credential, now);

        let Some(credential_entries) = entries.by_credential.get_mut(&credential_id) else {
            // 首次请求，需要创建缓存
            tracing::debug!(credential_id, "首次请求，无缓存条目");
            let (cache_5m, cache_1h) = compute_ttl_breakdown(profile, 0);
            return CacheResult {
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: last_breakpoint_tokens,
                cache_creation_5m_input_tokens: cache_5m,
                cache_creation_1h_input_tokens: cache_1h,
            };
        };

        tracing::debug!(
            credential_id,
            entry_count = credential_entries.len(),
            "查找缓存匹配"
        );

        let mut matched_tokens = 0;

        let cacheable_breakpoints = profile.cacheable_breakpoints();
        let candidate_breakpoints: Vec<_> = cacheable_breakpoints
            .iter()
            .rev()
            .take(PREFIX_LOOKBACK_LIMIT)
            .copied()
            .collect();

        'outer: for breakpoint in candidate_breakpoints {
            let candidate = &profile.blocks[breakpoint.block_index];
            if let Some(entry) = credential_entries.get_mut(&candidate.prefix_fingerprint) {
                if entry.expires_at <= now {
                    continue;
                }
                entry.expires_at = now + entry.ttl;
                matched_tokens = breakpoint.cumulative_tokens.min(profile.total_input_tokens);
                break 'outer;
            }
        }

        let new_tokens = last_breakpoint_tokens.saturating_sub(matched_tokens).max(0);
        let (cache_5m, cache_1h) = compute_ttl_breakdown(profile, matched_tokens);

        tracing::debug!(
            credential_id,
            matched_tokens,
            new_tokens,
            cache_5m,
            cache_1h,
            "缓存计算结果"
        );

        CacheResult {
            cache_read_input_tokens: matched_tokens.max(0),
            cache_creation_input_tokens: new_tokens,
            cache_creation_5m_input_tokens: cache_5m,
            cache_creation_1h_input_tokens: cache_1h,
        }
    }

    /// 请求成功后把本次的可缓存断点写回缓存表（供后续请求命中）。
    pub fn update(&self, credential_id: u64, profile: &CacheProfile) {
        let now = Instant::now();
        let mut entries = self.entries.lock();
        prune_expired(&mut entries.by_credential, now);

        let credential_entries = entries.by_credential.entry(credential_id).or_default();

        for breakpoint in profile.cacheable_breakpoints() {
            let block = &profile.blocks[breakpoint.block_index];
            let next_expiry = now + breakpoint.ttl;

            match credential_entries.get_mut(&block.prefix_fingerprint) {
                Some(existing) => {
                    existing.token_count = existing.token_count.max(block.cumulative_tokens);
                    existing.ttl = existing.ttl.max(breakpoint.ttl);
                    existing.expires_at = existing.expires_at.max(next_expiry);
                }
                None => {
                    credential_entries.insert(
                        block.prefix_fingerprint,
                        CacheEntry {
                            token_count: block.cumulative_tokens,
                            ttl: breakpoint.ttl,
                            expires_at: next_expiry,
                        },
                    );
                }
            }
        }
    }
}

/// 计算不同 TTL 的缓存创建 token 数
fn compute_ttl_breakdown(profile: &CacheProfile, matched_tokens: i32) -> (i32, i32) {
    let Some(last_breakpoint) = profile.last_cacheable_breakpoint() else {
        return (0, 0);
    };

    let new_tokens = last_breakpoint
        .cumulative_tokens
        .min(profile.total_input_tokens)
        .saturating_sub(matched_tokens)
        .max(0);

    if new_tokens == 0 {
        return (0, 0);
    }

    if last_breakpoint.ttl == ONE_HOUR_CACHE_TTL {
        (0, new_tokens)
    } else {
        (new_tokens, 0)
    }
}

impl CacheProfile {
    #[cfg(test)]
    pub fn total_input_tokens(&self) -> i32 {
        self.total_input_tokens
    }

    fn cacheable_breakpoints(&self) -> Vec<ResolvedBreakpoint> {
        self.breakpoints
            .iter()
            .filter_map(|breakpoint| {
                let block = self.blocks.get(breakpoint.block_index)?;
                if block.cumulative_tokens < self.min_cacheable_tokens {
                    return None;
                }

                Some(ResolvedBreakpoint {
                    block_index: breakpoint.block_index,
                    cumulative_tokens: block.cumulative_tokens,
                    ttl: breakpoint.ttl,
                })
            })
            .collect()
    }

    fn last_cacheable_breakpoint(&self) -> Option<ResolvedBreakpoint> {
        self.cacheable_breakpoints().into_iter().last()
    }
}

#[derive(Debug, Clone, Copy)]
struct ResolvedBreakpoint {
    block_index: usize,
    cumulative_tokens: i32,
    ttl: Duration,
}

#[derive(Debug)]
struct PendingBlock {
    value: serde_json::Value,
    tokens: i32,
    breakpoint_ttl: Option<Duration>,
    message_index: Option<usize>,
    is_message_end: bool,
}

fn flatten_cacheable_blocks(payload: &MessagesRequest) -> Vec<PendingBlock> {
    let mut blocks = Vec::new();

    if let Some(tools) = &payload.tools {
        for (tool_index, tool) in tools.iter().enumerate() {
            let mut value = serde_json::to_value(tool).unwrap_or(serde_json::Value::Null);
            let breakpoint_ttl = extract_cache_ttl(&value);
            strip_cache_control(&mut value);

            blocks.push(PendingBlock {
                value: canonicalize_json(serde_json::json!({
                    "kind": "tool",
                    "tool_index": tool_index,
                    "tool": value,
                })),
                tokens: count_tool_definition_tokens(tool) as i32,
                breakpoint_ttl,
                message_index: None,
                is_message_end: false,
            });
        }
    }

    if let Some(system) = &payload.system {
        for (system_index, block) in system.iter().enumerate() {
            let mut value = serde_json::to_value(block).unwrap_or(serde_json::Value::Null);
            let breakpoint_ttl = extract_cache_ttl(&value);
            strip_cache_control(&mut value);
            canonicalize_system_block_for_cache(&mut value);

            blocks.push(PendingBlock {
                value: canonicalize_json(serde_json::json!({
                    "kind": "system",
                    "system_index": system_index,
                    "block": value,
                })),
                tokens: count_system_message_tokens(block) as i32,
                breakpoint_ttl,
                message_index: None,
                is_message_end: false,
            });
        }
    }

    for (message_index, message) in payload.messages.iter().enumerate() {
        blocks.extend(flatten_message_blocks(message_index, message));
    }

    blocks
}

/// 归一化 system 文本块：把 Claude Code 的 `x-anthropic-billing-header:` 归因头
/// 折叠成固定占位符，避免每次请求归因头轻微漂移（版本号/随机 cch）就打破缓存前缀。
///
/// 复用 [`super::converter::canonicalize_billing_header`]，与转发路径保持同一套归一化规则，
/// 确保影子指纹计算与实际转发给上游的字节一致。
fn canonicalize_system_block_for_cache(value: &mut serde_json::Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };

    let is_text_block = obj
        .get("type")
        .and_then(|v| v.as_str())
        .map(|t| t == "text")
        .unwrap_or(true);
    if !is_text_block {
        return;
    }

    let Some(text) = obj.get("text").and_then(|v| v.as_str()) else {
        return;
    };
    let canonical = super::converter::canonicalize_billing_header(text);
    // 只有实际发生折叠（返回占位符）时才改写，避免无谓的字符串分配。
    if !std::ptr::eq(canonical, text) {
        let canonical = canonical.to_string();
        obj.insert("text".to_string(), serde_json::Value::String(canonical));
    }
}

fn flatten_message_blocks(message_index: usize, message: &Message) -> Vec<PendingBlock> {
    match &message.content {
        serde_json::Value::String(text) => vec![build_message_block(
            message_index,
            &message.role,
            0,
            serde_json::json!({
                "type": "text",
                "text": text,
            }),
            None,
            true,
        )],
        serde_json::Value::Array(blocks) => {
            let last_block_index = blocks.len().saturating_sub(1);
            blocks
                .iter()
                .enumerate()
                .map(|(block_index, block)| {
                    let breakpoint_ttl = extract_cache_ttl(block);
                    let mut normalized = block.clone();
                    strip_cache_control(&mut normalized);
                    build_message_block(
                        message_index,
                        &message.role,
                        block_index,
                        normalized,
                        breakpoint_ttl,
                        block_index == last_block_index,
                    )
                })
                .collect()
        }
        other => vec![build_message_block(
            message_index,
            &message.role,
            0,
            other.clone(),
            None,
            true,
        )],
    }
}

fn build_message_block(
    message_index: usize,
    role: &str,
    block_index: usize,
    block: serde_json::Value,
    breakpoint_ttl: Option<Duration>,
    is_message_end: bool,
) -> PendingBlock {
    PendingBlock {
        tokens: count_message_content_tokens(&block) as i32,
        value: canonicalize_json(serde_json::json!({
            "kind": "message",
            "message_index": message_index,
            "role": role,
            "block_index": block_index,
            "block": block,
        })),
        breakpoint_ttl,
        message_index: Some(message_index),
        is_message_end,
    }
}

fn extract_cache_ttl(value: &serde_json::Value) -> Option<Duration> {
    let cache_control = value.get("cache_control")?;
    let cache_control: CacheControl = serde_json::from_value(cache_control.clone()).ok()?;
    if cache_control.cache_type != "ephemeral" {
        return None;
    }

    Some(match cache_control.ttl.as_deref() {
        Some("1h") => ONE_HOUR_CACHE_TTL,
        _ => DEFAULT_CACHE_TTL,
    })
}

fn strip_cache_control(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(arr) => {
            for item in arr {
                strip_cache_control(item);
            }
        }
        serde_json::Value::Object(map) => {
            map.remove("cache_control");
            for item in map.values_mut() {
                strip_cache_control(item);
            }
        }
        _ => {}
    }
}

fn minimum_cacheable_tokens_for_model(model: &str) -> i32 {
    let model_lower = model.to_lowercase();

    if model_lower.contains("opus") {
        4096
    } else if model_lower.contains("haiku-3") || model_lower.contains("haiku_3") {
        2048
    } else {
        1024
    }
}

fn prune_expired(entries: &mut HashMap<u64, HashMap<[u8; 32], CacheEntry>>, now: Instant) {
    entries.retain(|_, credential_entries| {
        credential_entries.retain(|_, entry| entry.expires_at > now);
        !credential_entries.is_empty()
    });
}

fn canonicalize_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(canonicalize_json).collect())
        }
        serde_json::Value::Object(map) => {
            let ordered: BTreeMap<_, _> = map
                .into_iter()
                .map(|(key, value)| (key, canonicalize_json(value)))
                .collect();

            let mut out = serde_json::Map::new();
            for (key, value) in ordered {
                out.insert(key, value);
            }
            serde_json::Value::Object(out)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::types::{SystemMessage, Tool};
    use crate::token;

    fn build_request(messages: Vec<Message>) -> MessagesRequest {
        MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            messages,
            stream: false,
            system: Some(vec![SystemMessage {
                text: "system".to_string(),
                block_type: None,
                cache_control: None,
            }]),
            tools: Some(vec![Tool {
                tool_type: None,
                name: "echo".to_string(),
                description: "echo".to_string(),
                input_schema: Default::default(),
                max_uses: None,
                cache_control: None,
            }]),
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        }
    }

    fn build_request_with_system(
        messages: Vec<Message>,
        system: Vec<SystemMessage>,
    ) -> MessagesRequest {
        let mut request = build_request(messages);
        request.system = Some(system);
        request
    }

    fn msg(role: &str, content: serde_json::Value) -> Message {
        Message {
            role: role.to_string(),
            content,
        }
    }

    fn cache_text(text: &str) -> serde_json::Value {
        serde_json::json!([{
            "type": "text",
            "text": text,
            "cache_control": { "type": "ephemeral" }
        }])
    }

    fn long_cacheable_text() -> String {
        std::iter::repeat("cacheable prompt chunk")
            .take(256)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn medium_turn_text(label: &str) -> String {
        format!(
            "{} {}",
            label,
            std::iter::repeat("conversation growth chunk")
                .take(80)
                .collect::<Vec<_>>()
                .join(" ")
        )
    }

    fn estimate_input_tokens(request: &MessagesRequest) -> i32 {
        token::count_all_tokens(
            &request.model,
            request.system.as_deref(),
            &request.messages,
            request.tools.as_deref(),
        ) as i32
    }

    #[test]
    fn first_request_is_all_creation() {
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let req = build_request(vec![msg("user", cache_text(&long_cacheable_text()))]);
        let total = estimate_input_tokens(&req);
        let profile = tracker.build_profile(&req, total);
        let result = tracker.compute(1, &profile);

        assert_eq!(result.cache_read_input_tokens, 0);
        // cache_creation 以 total_input_tokens 封顶（块 token 估算口径与总量口径不同，
        // 尤其工具按固定 150 计，可能超过按内容计的总量）
        assert_eq!(
            result.cache_creation_input_tokens,
            profile
                .last_cacheable_breakpoint()
                .map(|bp| bp.cumulative_tokens.min(profile.total_input_tokens()))
                .unwrap_or(0)
        );
        assert!(result.cache_creation_input_tokens > 0);
    }

    #[test]
    fn same_prefix_second_request_is_read_hit() {
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let req1 = build_request(vec![msg("user", cache_text(&long_cacheable_text()))]);
        let total1 = estimate_input_tokens(&req1);
        let profile1 = tracker.build_profile(&req1, total1);
        tracker.update(1, &profile1);

        let req2 = build_request(vec![msg("user", cache_text(&long_cacheable_text()))]);
        let total2 = estimate_input_tokens(&req2);
        let profile2 = tracker.build_profile(&req2, total2);
        let result = tracker.compute(1, &profile2);

        assert_eq!(
            result.cache_read_input_tokens,
            profile1
                .last_cacheable_breakpoint()
                .map(|bp| bp.cumulative_tokens.min(profile1.total_input_tokens()))
                .unwrap_or(0)
        );
        assert_eq!(result.cache_creation_input_tokens, 0);
    }

    #[test]
    fn prefix_change_does_not_hit() {
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let system1 = vec![SystemMessage {
            text: long_cacheable_text(),
            block_type: Some("text".to_string()),
            cache_control: Some(CacheControl {
                cache_type: "ephemeral".to_string(),
                ttl: None,
            }),
        }];
        let system2 = vec![SystemMessage {
            text: format!("{} extra", long_cacheable_text()),
            block_type: Some("text".to_string()),
            cache_control: Some(CacheControl {
                cache_type: "ephemeral".to_string(),
                ttl: None,
            }),
        }];

        let req1 = build_request_with_system(vec![msg("user", serde_json::json!("hi"))], system1);
        let profile1 = tracker.build_profile(&req1, estimate_input_tokens(&req1));
        tracker.update(1, &profile1);

        let req2 = build_request_with_system(vec![msg("user", serde_json::json!("hi"))], system2);
        let profile2 = tracker.build_profile(&req2, estimate_input_tokens(&req2));
        let result = tracker.compute(1, &profile2);

        assert_eq!(result.cache_read_input_tokens, 0);
    }

    #[test]
    fn ttl_expiry_invalidates_cache() {
        // max_supported_ttl 设为 0 → 写回的条目立即过期，下次 compute 不命中
        let tracker = CacheTracker::new(Duration::from_secs(0));
        let req1 = build_request(vec![msg("user", cache_text(&long_cacheable_text()))]);
        let profile1 = tracker.build_profile(&req1, estimate_input_tokens(&req1));
        tracker.update(1, &profile1);

        std::thread::sleep(Duration::from_millis(5));

        let req2 = build_request(vec![msg("user", cache_text(&long_cacheable_text()))]);
        let profile2 = tracker.build_profile(&req2, estimate_input_tokens(&req2));
        let result = tracker.compute(1, &profile2);

        assert_eq!(result.cache_read_input_tokens, 0);
    }

    #[test]
    fn different_credentials_do_not_share_cache() {
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let req = build_request(vec![msg("user", cache_text(&long_cacheable_text()))]);
        let profile = tracker.build_profile(&req, estimate_input_tokens(&req));
        // 凭据 1 写回
        tracker.update(1, &profile);

        // 凭据 2 查询：必须不命中凭据 1 的缓存
        let result_cred2 = tracker.compute(2, &profile);
        assert_eq!(
            result_cred2.cache_read_input_tokens, 0,
            "不同凭据不应共享缓存条目"
        );

        // 凭据 1 自己查询：应命中
        let result_cred1 = tracker.compute(1, &profile);
        assert!(result_cred1.cache_read_input_tokens > 0);
    }

    #[test]
    fn attribution_header_drift_does_not_break_cache_hit() {
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let system1 = vec![
            SystemMessage {
                text: "x-anthropic-billing-header: cc_version=2.1.87.1; cc_entrypoint=cli; cch=aaaaa;".to_string(),
                block_type: Some("text".to_string()),
                cache_control: None,
            },
            SystemMessage {
                text: long_cacheable_text(),
                block_type: Some("text".to_string()),
                cache_control: Some(CacheControl {
                    cache_type: "ephemeral".to_string(),
                    ttl: None,
                }),
            },
        ];
        let system2 = vec![
            SystemMessage {
                text: "x-anthropic-billing-header: cc_version=2.1.87.222; cc_entrypoint=cli; cch=bbbbb; extra=xyz;".to_string(),
                block_type: Some("text".to_string()),
                cache_control: None,
            },
            SystemMessage {
                text: long_cacheable_text(),
                block_type: Some("text".to_string()),
                cache_control: Some(CacheControl {
                    cache_type: "ephemeral".to_string(),
                    ttl: None,
                }),
            },
        ];

        let req1 = build_request_with_system(vec![msg("user", serde_json::json!("hello"))], system1);
        let profile1 = tracker.build_profile(&req1, estimate_input_tokens(&req1));
        tracker.update(1, &profile1);

        let req2 = build_request_with_system(vec![msg("user", serde_json::json!("hello"))], system2);
        let profile2 = tracker.build_profile(&req2, estimate_input_tokens(&req2));
        let result = tracker.compute(1, &profile2);

        // 归因头漂移被归一化 → 仍命中
        assert!(result.cache_read_input_tokens > 0);
        assert_eq!(result.cache_creation_input_tokens, 0);
    }

    #[test]
    fn prefix_match_with_appended_turn_reads_previous_prefix_cache() {
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let long = long_cacheable_text();
        let req1 = build_request(vec![
            msg("user", cache_text(&long)),
            msg("assistant", serde_json::json!("R1")),
        ]);
        let profile1 = tracker.build_profile(&req1, estimate_input_tokens(&req1));
        tracker.update(1, &profile1);

        let req2 = build_request(vec![
            msg("user", cache_text(&long)),
            msg("assistant", serde_json::json!("R1")),
            msg("user", serde_json::json!(medium_turn_text("R2"))),
        ]);
        let profile2 = tracker.build_profile(&req2, estimate_input_tokens(&req2));
        let result = tracker.compute(1, &profile2);

        assert!(result.cache_read_input_tokens > 0);
    }
}
