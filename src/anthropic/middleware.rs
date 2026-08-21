//! Anthropic API 中间件

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};

use crate::common::auth;
use crate::kiro::provider::KiroProvider;

use super::types::ErrorResponse;

/// 应用共享状态
#[derive(Clone)]
pub struct AppState {
    /// Kiro Provider（可选，用于实际 API 调用）
    /// 内部使用 MultiTokenManager，已支持线程安全的多凭据管理
    pub kiro_provider: Option<Arc<KiroProvider>>,
}

// 注：`extract_thinking` / `compression` 等热更项由 handlers.rs 的进程级原子/ArcSwap 镜像
// 承载，admin 改配置调 setter 即时生效、无需重启。AppState 只保留真正随请求走的共享句柄。
// apiKey 同理：已迁入 `common::auth_keys` 的进程级热更单元（main.rs 启动播种，
// admin 改配置调 setter），故此处不再持有 `api_key` 字段——留着它只会有两个真相源，
// 改配置后中间件读旧值、面板显示新值。`new` 保留 `_api_key` 参数仅为兼容 router 签名。
// （影子 prompt 缓存记账已整体移除——它不省钱且在大请求热路径同步跑 SHA256 拖慢传输，
//   真正省上游 credit 的是 converter 的 continuationId 确定性派生，与此无关、仍在。）

impl AppState {
    /// 创建应用状态。
    ///
    /// apiKey 的播种在 main.rs 启动校验处完成（`auth_keys::set_user_key`），这里不再存、
    /// 也不播——避免测试里随意构造 `AppState` 污染进程级全局 cell（并行测试会串味）。
    pub fn new(_api_key: impl Into<String>) -> Self {
        Self {
            kiro_provider: None,
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
    // State 仍需保留（`from_fn_with_state` 的签名要求），但鉴权已不读它。
    State(_state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    // 活读进程级热更单元而非 State 里的固化副本：admin 改 apiKey 后即时生效，无需重启
    // （重启会掐断在途流式请求）。空存储恒 false（fail-closed），见 auth_keys 模块级安全说明。
    match auth::extract_api_key(&request) {
        Some(key) if crate::common::auth_keys::user_key_matches(&key) => next.run(request).await,
        _ => {
            // D1 接入（M2 补充）：API key 不匹配走配置 key `api_key_invalid`
            // （status/type/message 可配，默认 = 现状 401 + "Invalid API key"）。
            let (status, error_type, message, _) = super::handlers::resolve_msg(
                &super::handlers::current_error_messages(),
                "api_key_invalid",
                (
                    StatusCode::UNAUTHORIZED,
                    "authentication_error",
                    "Invalid API key",
                    None,
                ),
            );
            (status, Json(ErrorResponse::new(error_type, message))).into_response()
        }
    }
}

// CORS 层构建已迁移至 `crate::common::security::build_cors_layer`（支持来源白名单）。
