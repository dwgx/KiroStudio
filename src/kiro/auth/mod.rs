//! Kiro 网页上号（OAuth）模块
//!
//! social: Portal PKCE OAuth（个人账号网页登录，主路径）
//! idc: AWS IAM Identity Center Device Code 登录（企业账号）
//! 移植自 ZyphrZero/kiro.rs，对接真实 Kiro 端点。
pub mod idc;
pub mod social;
