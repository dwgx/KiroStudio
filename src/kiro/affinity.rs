//! 用户亲和性管理模块
//!
//! 记录 user_id（会话 conversationId）与 credential_id 的绑定关系，
//! 使连续对话尽量使用同一凭据，降低单会话散落到多账号的关联风险。
//!
//! 已接线到 MultiTokenManager::select_next_credential：
//! key 取自请求 conversationId（由 converter 从 metadata.user_id 派生）。
//! 由 config.affinity_enabled 控制（默认开）。

use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 亲和性条目
struct AffinityEntry {
    credential_id: u64,
    last_used: Instant,
}

/// 用户亲和性管理器
pub struct UserAffinityManager {
    affinity: Mutex<HashMap<String, AffinityEntry>>,
    ttl: Duration,
}

impl Default for UserAffinityManager {
    fn default() -> Self {
        Self::new()
    }
}

impl UserAffinityManager {
    /// 创建新的亲和性管理器（默认 TTL 30 分钟）
    pub fn new() -> Self {
        Self {
            affinity: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(30 * 60),
        }
    }

    /// 获取用户绑定的凭据 ID（惰性清理过期条目）
    pub fn get(&self, user_id: &str) -> Option<u64> {
        let mut map = self.affinity.lock();
        if let Some(entry) = map.get(user_id) {
            if entry.last_used.elapsed() < self.ttl {
                tracing::debug!(user_id = %user_id, credential_id = %entry.credential_id, "亲和性命中");
                return Some(entry.credential_id);
            }
            // 过期则删除
            tracing::debug!(user_id = %user_id, credential_id = %entry.credential_id, "亲和性过期，已清除");
            map.remove(user_id);
        }
        None
    }

    /// 设置用户与凭据的绑定
    pub fn set(&self, user_id: &str, credential_id: u64) {
        tracing::debug!(user_id = %user_id, credential_id = %credential_id, "建立亲和性绑定");
        let mut map = self.affinity.lock();
        map.insert(
            user_id.to_string(),
            AffinityEntry {
                credential_id,
                last_used: Instant::now(),
            },
        );
    }

    /// 更新最后使用时间（续期）
    pub fn touch(&self, user_id: &str) {
        let mut map = self.affinity.lock();
        if let Some(entry) = map.get_mut(user_id) {
            entry.last_used = Instant::now();
        }
    }

    /// 移除指定凭据的所有绑定（凭据被禁用时调用）
    pub fn remove_by_credential(&self, credential_id: u64) {
        let mut map = self.affinity.lock();
        map.retain(|_, entry| entry.credential_id != credential_id);
    }

    /// 清理过期条目
    #[allow(dead_code)]
    pub fn cleanup(&self) {
        let mut map = self.affinity.lock();
        let ttl = self.ttl;
        map.retain(|_, entry| entry.last_used.elapsed() < ttl);
    }
}
