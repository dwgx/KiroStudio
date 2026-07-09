//! Anthropic API 路由配置

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
};

use crate::kiro::provider::KiroProvider;

use crate::common::security::build_cors_layer;

use super::{
    handlers::{count_tokens, get_models, post_messages, post_messages_cc},
    middleware::{AppState, auth_middleware},
};

/// 创建 Anthropic API 路由
///
/// # 端点
/// - `GET /v1/models` - 获取可用模型列表
/// - `POST /v1/messages` - 创建消息（对话）
/// - `POST /v1/messages/count_tokens` - 计算 token 数量
///
/// # 认证
/// 所有 `/v1` 路径需要 API Key 认证，支持：
/// - `x-api-key` header
/// - `Authorization: Bearer <token>` header
///
/// # 参数
/// - `api_key`: API 密钥，用于验证客户端请求
/// - `kiro_provider`: 可选的 KiroProvider，用于调用上游 API

/// 创建带有 KiroProvider 的 Anthropic API 路由
#[allow(clippy::too_many_arguments)]
pub fn create_router_with_provider(
    api_key: impl Into<String>,
    kiro_provider: Option<KiroProvider>,
    extract_thinking: bool,
    cors_allowed_origins: &[String],
    max_body_bytes: usize,
    compression: crate::model::config::CompressionConfig,
    strip_env_noise: bool,
) -> Router {
    let mut state = AppState::new(api_key);
    if let Some(provider) = kiro_provider {
        state = state.with_kiro_provider(provider);
    }

    // TIER3 配置热重载：把热路径开关/压缩配置播种进进程级镜像（handler 读镜像而非固化 state）。
    // 之后 admin 改配置调对应 setter 即时生效、无需重启（extract_thinking / compression 两项）。
    super::handlers::set_extract_thinking(extract_thinking);
    super::handlers::set_compression(compression);
    // 环境噪音剥离开关：播种进 converter 进程级镜像（归一化路径读镜像），admin 改后即时生效。
    super::converter::set_strip_env_noise(strip_env_noise);

    // 需要认证的 /v1 路由
    let v1_routes = Router::new()
        .route("/models", get(get_models))
        .route("/messages", post(post_messages))
        .route("/messages/count_tokens", post(count_tokens))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // 需要认证的 /cc/v1 路由（Claude Code 兼容端点）
    // 与 /v1 的区别：流式响应会等待 contextUsageEvent 后再发送 message_start
    let cc_v1_routes = Router::new()
        .route("/messages", post(post_messages_cc))
        .route("/messages/count_tokens", post(count_tokens))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // 请求体上限：0 = 不限制（dwgx：没必要限制，可显式填 0 完全放开）。
    // ⚠️ 注意：/v1 与 /cc/v1 的请求体是 **buffered**（JsonExtractor 一次性读入内存），
    // 不是流式——设 0（disable）等于允许任意大 body 全量入内存，理论上有 OOM 敞口。
    // 因此默认值保留 256MiB 大软上限（default_max_body_bytes），既远超正常请求（上游
    // compression 4MiB 就触发、~5MiB 就 400），又挡住恶意超大 body 打死进程。想彻底放开的
    // 用户可显式把 maxBodyBytes 设为 0。
    let body_limit_layer = if max_body_bytes == 0 {
        DefaultBodyLimit::disable()
    } else {
        DefaultBodyLimit::max(max_body_bytes)
    };

    Router::new()
        .nest("/v1", v1_routes)
        .nest("/cc/v1", cc_v1_routes)
        .layer(build_cors_layer(cors_allowed_origins))
        .layer(body_limit_layer)
        .with_state(state)
}
