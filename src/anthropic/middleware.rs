//! Anthropic API 中间件

use std::sync::Arc;

use axum::{
    body::Body,
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
// userKey 同理：已迁入 `common::auth_keys` 的进程级单元，故此处不再持有 `api_key` 字段
// ——留着它只会有两个真相源，改配置后中间件读旧值、面板显示新值。
// （影子 prompt 缓存记账已整体移除——它不省钱且在大请求热路径同步跑 SHA256 拖慢传输，
//   真正省上游 credit 的是 converter 的 continuationId 确定性派生，与此无关、仍在。）

impl AppState {
    /// 创建应用状态，并把 userKey 播种进热更单元（后续 admin 改配置走 setter）。
    ///
    /// 空值在 main 启动检查处已 `exit(1)`；此处再兜一层：播种失败说明拿到了空 key，
    /// 继续跑会让 `/v1` 匿名可达，故直接 panic 而非静默放过（fail-closed）。
    pub fn new(api_key: impl Into<String>) -> Self {
        crate::common::auth_keys::set_user_key(&api_key.into())
            .expect("userKey 为空——拒绝以无鉴权方式提供 /v1（空值会导致鉴权 fail-open）");
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
///
/// 不再取 `State<AppState>`：key 已迁入进程级热更单元，中间件无需任何 State
/// （留着空 State 只会让人误以为鉴权还读它）。故注册处用 `from_fn` 而非
/// `from_fn_with_state`。
pub async fn auth_middleware(request: Request<Body>, next: Next) -> Response {
    // 走进程级热更单元而非固化副本：admin 改 userKey 后即时生效，无需重启
    // （重启会掐断在途流式请求）。空存储恒 false，见 auth_keys 模块级安全说明。
    match auth::extract_api_key(&request) {
        Some(key) if crate::common::auth_keys::user_key_matches(&key) => next.run(request).await,
        _ => {
            let error = ErrorResponse::authentication_error();
            (StatusCode::UNAUTHORIZED, Json(error)).into_response()
        }
    }
}

// CORS 层构建已迁移至 `crate::common::security::build_cors_layer`（支持来源白名单）。
