//! 公共工具模块

pub mod alerting;
pub mod auth;
/// 鉴权密钥热更单元（apiKey/adminApiKey 的进程级 ArcSwap，单一真相源）
pub mod auth_keys;
pub mod fs_atomic;
pub mod health_marker;
pub mod http_read;
pub mod log_buffer;
pub mod recovery_metrics;
pub mod secret_store;
pub mod security;
pub mod ssrf;
/// 测试卫生守卫（仅测试期有用，生产代码零调用 —— 见该模块文档）。
#[cfg(test)]
pub mod test_hygiene;
