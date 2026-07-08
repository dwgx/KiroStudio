//! Anthropic API 中间件

use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};

use crate::common::auth;
use crate::kiro::provider::KiroProvider;

use super::cache_tracker::CacheTracker;
use super::types::ErrorResponse;

/// 应用共享状态
#[derive(Clone)]
pub struct AppState {
    /// API 密钥
    pub api_key: String,
    /// Kiro Provider（可选，用于实际 API 调用）
    /// 内部使用 MultiTokenManager，已支持线程安全的多凭据管理
    pub kiro_provider: Option<Arc<KiroProvider>>,
    /// prompt 缓存影子跟踪器（按凭据分桶）
    pub cache_tracker: Arc<CacheTracker>,
}

// 注：`extract_thinking` / `prompt_cache_enabled` / `compression` 三项已迁出 AppState
// （TIER3 配置热重载）：它们现由 handlers.rs 的进程级原子/ArcSwap 镜像承载，admin 改配置调
// setter 即时生效、无需重启。AppState 只保留真正随请求走且无需热更的共享句柄。

impl AppState {
    /// 创建应用状态。
    ///
    /// `prompt_cache_ttl_seconds` 固化进 `CacheTracker`（缓存表容忍的最长 TTL，运行时改它需
    /// 重建 tracker 丢缓存，属诚实边界保留重启）。其余热更开关由 handlers 镜像承载，不进 AppState。
    pub fn with_cache_ttl(
        api_key: impl Into<String>,
        prompt_cache_ttl_seconds: u64,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            kiro_provider: None,
            cache_tracker: Arc::new(CacheTracker::new(Duration::from_secs(
                prompt_cache_ttl_seconds,
            ))),
        }
    }

    /// 设置 KiroProvider
    pub fn with_kiro_provider(mut self, provider: KiroProvider) -> Self {
        self.kiro_provider = Some(Arc::new(provider));
        self
    }
}

/// API Key 认证中间件
pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    match auth::extract_api_key(&request) {
        Some(key) if auth::constant_time_eq(&key, &state.api_key) => next.run(request).await,
        _ => {
            let error = ErrorResponse::authentication_error();
            (StatusCode::UNAUTHORIZED, Json(error)).into_response()
        }
    }
}

// CORS 层构建已迁移至 `crate::common::security::build_cors_layer`（支持来源白名单）。
