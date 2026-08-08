//! Anthropic API 兼容服务模块
//!
//! 提供与 Anthropic Claude API 兼容的 HTTP 服务端点。
//!
//! # 支持的端点
//!
//! ## 标准端点 (/v1)
//! - `GET /v1/models` - 获取可用模型列表
//! - `POST /v1/messages` - 创建消息（对话）
//! - `POST /v1/messages/count_tokens` - 计算 token 数量
//!
//! ## Claude Code 兼容端点 (/cc/v1)
//! - `POST /cc/v1/messages` - 创建消息（流式响应会等待 contextUsageEvent 后再发送 message_start，确保 input_tokens 准确）
//! - `POST /cc/v1/messages/count_tokens` - 计算 token 数量（与 /v1 相同）
//!
//! # 使用示例
//! ```rust,ignore
//! use kirostudio::anthropic;
//!
//! let app = anthropic::create_router("your-api-key");
//! let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
//! axum::serve(listener, app).await?;
//! ```

pub mod compressor;
pub(crate) mod cache;
pub(crate) mod converter;
pub(crate) mod handlers;
pub(crate) mod image_resize;
pub(crate) mod middleware;
pub(crate) mod model_catalog;
mod router;
mod stream;
pub mod types;
mod websearch;

pub use converter::{set_strip_env_noise, set_tool_description_max_chars};
pub use handlers::set_cc_auto_buffer;
pub use handlers::set_collect_client_fingerprint;
pub use handlers::set_extract_thinking;
pub use handlers::set_prompt_cache_enabled;
pub use handlers::set_trust_forwarded_header;
/// 吸收层分类器：供 `kiro::provider` 的 'absorb 循环判定「这个错误值不值得就地重试」。
/// 刻意复用 handlers 侧的既有谓词而不在 provider 另写一套字符串匹配 —— 两份拷贝必然漂移。
/// `pub(crate)`：这是网关内部的重试策略，不属于 anthropic 模块对外的 API 面。
pub(crate) use handlers::{AbsorbClass, absorb_class_of};
pub use handlers::{
    set_tool_clean_leaked_tokens, set_tool_expose_error_to_client, set_tool_repair_json,
    set_tool_stream_align_failure, set_tool_truncation_recovery,
};
pub use router::create_router_with_provider;
