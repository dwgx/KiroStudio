//! Admin API 模块
//!
//! 提供凭据管理和监控功能的 HTTP API
//!
//! # 功能
//! - 查询所有凭据状态
//! - 启用/禁用凭据
//! - 修改凭据优先级
//! - 重置失败计数
//! - 查询凭据余额
//!
//! # 使用
//! ```ignore
//! let admin_service = AdminService::new(token_manager.clone(), endpoint_names);
//! let admin_state = AdminState::new(admin_api_key, admin_service);
//! let admin_router = create_admin_router(admin_state);
//! ```

mod error;
mod external_idp_login;
mod handlers;
mod idc_login;
mod middleware;
mod router;
mod service;
mod social_login;
pub mod types;
mod update;
mod usage_handlers;

pub use middleware::AdminState;
pub use router::create_admin_router;
pub use service::AdminService;
// Windows 系统托盘「重启服务」复用面板一键重启的自重启逻辑（同源）。
#[cfg(windows)]
pub(crate) use service::spawn_windows_relaunch_process;
