//! Admin UI 静态文件服务模块
//!
//! 使用 rust-embed 嵌入前端构建产物

mod router;

pub use router::{
    bg_pool_stats, clear_bg_pool, create_admin_ui_router, set_login_background_enabled,
    spawn_bg_prefetch,
};
