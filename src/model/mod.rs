//! 应用配置模型

pub mod arg;
pub mod config;
pub mod error_messages;
/// 吸收层类别（kiro 策略与 anthropic 分类器共用，不放协议模块）。
pub(crate) mod absorb;
pub(crate) use absorb::AbsorbClass;
