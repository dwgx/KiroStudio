//! Admin UI 静态文件服务模块
//!
//! 使用 rust-embed 嵌入前端构建产物

mod router;

pub use router::{
    bg_pool_stats, clear_bg_pool, create_admin_ui_router, serve_help_page,
    set_login_background_enabled, set_login_background_r18, spawn_bg_prefetch, trigger_bg_refill,
};
