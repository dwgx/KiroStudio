//! Kiro prompt cache 的本地可验证账本。
//!
//! Kiro 的响应协议不返回服务端 cache hit/miss，因此这里不把结果称为上游确认值。
//! 账本只在以下条件全部成立时报告 inferred hit：
//! - 实际凭据、端点、region、模型和 agentContinuationId 相同；
//! - 端点变换后的最终 Kiro JSON 具有完全相同的语义前缀；
//! - 建立该前缀的上一请求已完整成功；
//! - 记录仍在配置 TTL 内。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use sha2::{Digest, Sha256};

const MAX_CACHE_CHECKPOINTS: usize = 16_384;

#[derive(Debug, Clone, Copy, Default)]
pub struct PromptCacheUsage {
    pub cache_read_input_tokens: i32,
    pub cache_creation_input_tokens: i32,
}

#[derive(Debug, Clone)]
pub(crate) struct PromptCacheProbe {
    pub usage: PromptCacheUsage,
    profile: PromptCacheProfile,
    /// 本地有效期从请求探测（即实际发送前）开始，而不是从响应结束提交时开始。
    /// 长响应不能凭空延长上游可能存在的缓存窗口。
    expires_at: Instant,
}

#[derive(Debug, Clone)]
struct PromptCacheProfile {
    checkpoints: Vec<Checkpoint>,
}

#[derive(Debug, Clone)]
struct Checkpoint {
    fingerprint: [u8; 32],
    cumulative_tokens: i32,
}

#[derive(Debug, Clone, Copy)]
struct CacheEntry {
    expires_at: Instant,
    committed_at: Instant,
}

#[derive(Default)]
pub(crate) struct PromptCacheTracker {
    entries: Mutex<HashMap<[u8; 32], CacheEntry>>,
}

impl PromptCacheTracker {
    pub fn probe(
        &self,
        final_body: &str,
        credential_id: u64,
        endpoint: &str,
        region: &str,
        is_1m: bool,
        ttl: Duration,
    ) -> Option<PromptCacheProbe> {
        if ttl.is_zero() {
            return None;
        }
        let profile = build_profile(final_body, credential_id, endpoint, region, is_1m)?;
        profile.checkpoints.last()?;
        let now = Instant::now();
        let mut entries = self.entries.lock();
        entries.retain(|_, entry| entry.expires_at > now);

        let matched_tokens = profile
            .checkpoints
            .iter()
            .rev()
            .find(|checkpoint| entries.contains_key(&checkpoint.fingerprint))
            .map(|checkpoint| checkpoint.cumulative_tokens)
            .unwrap_or(0);

        Some(PromptCacheProbe {
            usage: PromptCacheUsage {
                cache_read_input_tokens: matched_tokens,
                // Kiro 没有返回“本轮创建了多少缓存”的回执。账本只在完整成功后静默
                // 提交检查点，绝不提前把待提交量伪装成 cache_creation。
                cache_creation_input_tokens: 0,
            },
            profile,
            expires_at: now + ttl,
        })
    }

    /// 仅由完整成功的响应收尾调用。失败、截断或客户端中途断开都不会建立缓存记录。
    pub fn commit(&self, probe: &PromptCacheProbe) {
        let now = Instant::now();
        let mut entries = self.entries.lock();
        entries.retain(|_, entry| entry.expires_at > now);

        // 响应在本地 TTL 之后才完整结束：宁可不建立记录，也不能从收尾时重新起算
        // TTL，把一个可能已在上游过期的前缀继续报告为命中。
        if probe.expires_at <= now {
            return;
        }

        for checkpoint in &probe.profile.checkpoints {
            entries.insert(
                checkpoint.fingerprint,
                CacheEntry {
                    expires_at: probe.expires_at,
                    committed_at: now,
                },
            );
        }

        if entries.len() > MAX_CACHE_CHECKPOINTS {
            let excess = entries.len() - MAX_CACHE_CHECKPOINTS;
            let mut oldest: Vec<_> = entries
                .iter()
                .map(|(fingerprint, entry)| (*fingerprint, entry.committed_at))
                .collect();
            oldest.sort_unstable_by_key(|(_, committed_at)| *committed_at);
            for (fingerprint, _) in oldest.into_iter().take(excess) {
                entries.remove(&fingerprint);
            }
        }
    }
}

fn build_profile(
    final_body: &str,
    credential_id: u64,
    endpoint: &str,
    region: &str,
    is_1m: bool,
) -> Option<PromptCacheProfile> {
    let body: serde_json::Value = serde_json::from_str(final_body).ok()?;
    let state = body.get("conversationState")?;
    let continuation_id = state.get("agentContinuationId")?.as_str()?;
    let current = state.pointer("/currentMessage/userInputMessage")?;
    let model = current.get("modelId")?.as_str()?;

    // 工具定义影响上游 prompt 前缀，但 converter 把它们放在 currentMessage 中。
    // 单独纳入 prelude，随后从每条用户消息中移除，令“上一轮 current user”与
    // “下一轮 history user”可以按同一语义结构精确比较。
    let tools = current
        .pointer("/userInputMessageContext/tools")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    let prelude = canonicalize_json(serde_json::json!({
        "schema": 1,
        "credential_id": credential_id,
        "endpoint": endpoint,
        "region": region,
        "is_1m": is_1m,
        "profile_arn": body.get("profileArn"),
        "agent_continuation_id": continuation_id,
        "model": model,
        "agent_task_type": state.get("agentTaskType"),
        "tools": tools,
    }));

    let mut hasher = Sha256::new();
    hash_value(&mut hasher, &prelude);
    let mut cumulative_tokens = token_count(&tools);
    let mut checkpoints = Vec::new();

    if let Some(history) = state.get("history").and_then(|value| value.as_array()) {
        for message in history {
            let normalized = normalize_history_message(message)?;
            cumulative_tokens = cumulative_tokens.saturating_add(token_count(&normalized));
            hash_value(&mut hasher, &normalized);
            checkpoints.push(Checkpoint {
                fingerprint: hasher.clone().finalize().into(),
                cumulative_tokens,
            });
        }
    }

    let normalized_current = normalize_user_message(current.clone());
    cumulative_tokens = cumulative_tokens.saturating_add(token_count(&normalized_current));
    hash_value(&mut hasher, &normalized_current);
    checkpoints.push(Checkpoint {
        fingerprint: hasher.finalize().into(),
        cumulative_tokens,
    });

    Some(PromptCacheProfile { checkpoints })
}

fn normalize_history_message(message: &serde_json::Value) -> Option<serde_json::Value> {
    if let Some(user) = message.get("userInputMessage") {
        return Some(normalize_user_message(user.clone()));
    }
    message.get("assistantResponseMessage").map(|assistant| {
        canonicalize_json(serde_json::json!({
            "role": "assistant",
            "message": assistant,
        }))
    })
}

fn normalize_user_message(mut user: serde_json::Value) -> serde_json::Value {
    if let Some(object) = user.as_object_mut() {
        // IDE/CLI 对 currentMessage 使用不同 origin；该字段不属于模型可见内容。
        object.remove("origin");
        if let Some(context) = object
            .get_mut("userInputMessageContext")
            .and_then(|value| value.as_object_mut())
        {
            context.remove("tools");
            if context.is_empty() {
                object.remove("userInputMessageContext");
            }
        }
    }
    canonicalize_json(serde_json::json!({
        "role": "user",
        "message": user,
    }))
}

fn hash_value(hasher: &mut Sha256, value: &serde_json::Value) {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn token_count(value: &serde_json::Value) -> i32 {
    let serialized = serde_json::to_string(value).unwrap_or_default();
    crate::token::count_tokens(&serialized).min(i32::MAX as u64) as i32
}

fn canonicalize_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonicalize_json).collect())
        }
        serde_json::Value::Object(values) => {
            let mut pairs: Vec<_> = values.into_iter().collect();
            pairs.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
            let mut ordered = serde_json::Map::new();
            for (key, value) in pairs {
                ordered.insert(key, canonicalize_json(value));
            }
            serde_json::Value::Object(ordered)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(history: serde_json::Value, current: &str) -> String {
        serde_json::json!({
            "conversationState": {
                "agentContinuationId": "continuation-1",
                "agentTaskType": "vibe",
                "conversationId": "conversation-1",
                "history": history,
                "currentMessage": {"userInputMessage": {
                    "content": current,
                    "modelId": "claude-sonnet-4.6",
                    "origin": "AI_EDITOR",
                    "userInputMessageContext": {"tools": []}
                }}
            }
        })
        .to_string()
    }

    #[test]
    fn successful_previous_request_enables_exact_continuation_hit() {
        let tracker = PromptCacheTracker::default();
        let ttl = Duration::from_secs(300);
        let first = tracker
            .probe(
                &body(serde_json::json!([]), "hello"),
                1,
                "ide",
                "us-east-1",
                false,
                ttl,
            )
            .unwrap();
        assert_eq!(first.usage.cache_read_input_tokens, 0);
        assert_eq!(first.usage.cache_creation_input_tokens, 0);
        tracker.commit(&first);

        let second_body = body(
            serde_json::json!([
                {"userInputMessage": {"content": "hello", "modelId": "claude-sonnet-4.6", "origin": "AI_EDITOR"}},
                {"assistantResponseMessage": {"content": "hi"}}
            ]),
            "next",
        );
        let second = tracker
            .probe(&second_body, 1, "ide", "us-east-1", false, ttl)
            .unwrap();
        assert!(second.usage.cache_read_input_tokens > 0);
        assert_eq!(second.usage.cache_creation_input_tokens, 0);
    }

    #[test]
    fn uncommitted_failed_request_never_hits() {
        let tracker = PromptCacheTracker::default();
        let ttl = Duration::from_secs(300);
        let first_body = body(serde_json::json!([]), "hello");
        let _failed = tracker
            .probe(&first_body, 1, "ide", "us-east-1", false, ttl)
            .unwrap();
        let retry = tracker
            .probe(&first_body, 1, "ide", "us-east-1", false, ttl)
            .unwrap();
        assert_eq!(retry.usage.cache_read_input_tokens, 0);
    }

    #[test]
    fn credential_endpoint_and_region_are_isolated() {
        let tracker = PromptCacheTracker::default();
        let ttl = Duration::from_secs(300);
        let request = body(serde_json::json!([]), "hello");
        let first = tracker
            .probe(&request, 1, "ide", "us-east-1", false, ttl)
            .unwrap();
        tracker.commit(&first);
        assert_eq!(
            tracker
                .probe(&request, 2, "ide", "us-east-1", false, ttl)
                .unwrap()
                .usage
                .cache_read_input_tokens,
            0
        );
        assert_eq!(
            tracker
                .probe(&request, 1, "cli", "us-east-1", false, ttl)
                .unwrap()
                .usage
                .cache_read_input_tokens,
            0
        );
        assert_eq!(
            tracker
                .probe(&request, 1, "ide", "eu-west-1", false, ttl)
                .unwrap()
                .usage
                .cache_read_input_tokens,
            0
        );
    }

    #[test]
    fn changed_content_and_continuation_do_not_hit() {
        let tracker = PromptCacheTracker::default();
        let ttl = Duration::from_secs(300);
        let request = body(serde_json::json!([]), "hello");
        let first = tracker
            .probe(&request, 1, "ide", "us-east-1", false, ttl)
            .unwrap();
        tracker.commit(&first);

        let changed = body(serde_json::json!([]), "different");
        assert_eq!(
            tracker
                .probe(&changed, 1, "ide", "us-east-1", false, ttl)
                .unwrap()
                .usage
                .cache_read_input_tokens,
            0
        );

        let mut different_continuation: serde_json::Value = serde_json::from_str(&request).unwrap();
        different_continuation["conversationState"]["agentContinuationId"] =
            serde_json::json!("continuation-2");
        assert_eq!(
            tracker
                .probe(
                    &different_continuation.to_string(),
                    1,
                    "ide",
                    "us-east-1",
                    false,
                    ttl,
                )
                .unwrap()
                .usage
                .cache_read_input_tokens,
            0
        );
    }

    #[test]
    fn ttl_expiry_removes_hit_without_sleeping() {
        let tracker = PromptCacheTracker::default();
        let ttl = Duration::from_secs(300);
        let request = body(serde_json::json!([]), "hello");
        let first = tracker
            .probe(&request, 1, "ide", "us-east-1", false, ttl)
            .unwrap();
        tracker.commit(&first);

        for entry in tracker.entries.lock().values_mut() {
            entry.expires_at = Instant::now() - Duration::from_millis(1);
        }
        assert_eq!(
            tracker
                .probe(&request, 1, "ide", "us-east-1", false, ttl)
                .unwrap()
                .usage
                .cache_read_input_tokens,
            0
        );
    }

    #[test]
    fn response_finishing_after_probe_ttl_does_not_commit() {
        let tracker = PromptCacheTracker::default();
        let request = body(serde_json::json!([]), "hello");
        let mut probe = tracker
            .probe(
                &request,
                1,
                "ide",
                "us-east-1",
                false,
                Duration::from_secs(300),
            )
            .unwrap();
        // 不 sleep：直接模拟一个在响应结束前已经过期的探针。
        probe.expires_at = Instant::now() - Duration::from_millis(1);
        tracker.commit(&probe);

        let next = tracker
            .probe(
                &request,
                1,
                "ide",
                "us-east-1",
                false,
                Duration::from_secs(300),
            )
            .unwrap();
        assert_eq!(next.usage.cache_read_input_tokens, 0);
    }

    #[test]
    fn one_million_mode_is_isolated_and_invalid_body_is_unknown() {
        let tracker = PromptCacheTracker::default();
        let ttl = Duration::from_secs(300);
        let request = body(serde_json::json!([]), "hello");
        let first = tracker
            .probe(&request, 1, "ide", "us-east-1", false, ttl)
            .unwrap();
        tracker.commit(&first);

        assert_eq!(
            tracker
                .probe(&request, 1, "ide", "us-east-1", true, ttl)
                .unwrap()
                .usage
                .cache_read_input_tokens,
            0
        );
        assert!(
            tracker
                .probe("not-json", 1, "ide", "us-east-1", false, ttl)
                .is_none()
        );
        assert!(
            tracker
                .probe("{}", 1, "ide", "us-east-1", false, ttl)
                .is_none()
        );
    }
}
