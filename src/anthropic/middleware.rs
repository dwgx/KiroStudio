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
use crate::model::config::CompressionConfig;

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
    /// 是否开启非流式响应的 thinking 块提取
    pub extract_thinking: bool,
    /// prompt 缓存记账是否启用
    pub prompt_cache_enabled: bool,
    /// prompt 缓存影子跟踪器（按凭据分桶）
    pub cache_tracker: Arc<CacheTracker>,
    /// 输入压缩配置（转换后发上游前，超阈值时压缩请求体）
    pub compression: Arc<CompressionConfig>,
}

impl AppState {
    /// 创建新的应用状态
    pub fn new(api_key: impl Into<String>, extract_thinking: bool) -> Self {
        Self::with_prompt_cache(api_key, extract_thinking, true, 3600)
    }

    /// 创建带 prompt 缓存配置的应用状态
    pub fn with_prompt_cache(
        api_key: impl Into<String>,
        extract_thinking: bool,
        prompt_cache_enabled: bool,
        prompt_cache_ttl_seconds: u64,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            kiro_provider: None,
            extract_thinking,
            prompt_cache_enabled,
            cache_tracker: Arc::new(CacheTracker::new(Duration::from_secs(
                prompt_cache_ttl_seconds,
            ))),
            compression: Arc::new(CompressionConfig::default()),
        }
    }

    /// 设置输入压缩配置
    pub fn with_compression(mut self, compression: CompressionConfig) -> Self {
        self.compression = Arc::new(compression);
        self
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
