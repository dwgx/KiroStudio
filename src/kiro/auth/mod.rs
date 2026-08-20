//! Kiro 网页上号（OAuth）模块
//!
//! social: Portal PKCE OAuth（个人账号网页登录，主路径）
//! idc: AWS IAM Identity Center Device Code 登录（企业账号）
//! sso_token: AWS portal Bearer Token 粘贴导入（静默换标准 IdC 凭据）
//! 移植自 ZyphrZero/kiro.rs + Kiro-Go auth/sso_token.go，对接真实 Kiro 端点。
pub mod idc;
pub mod social;
pub mod sso_token;
