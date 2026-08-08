//! Portal：面向多用户的独立凭据查看入口。
//!
//! # 它是什么
//! 一个与 `/api/admin` **完全分离**的最小页面：用户用自己的账号密码登录，查看
//! 「外部推送过来的凭据」并直接复制明文 key。没有任何管理能力（改配置、增删凭据、
//! 看日志都不在这里）。
//!
//! # 三条不可动摇的设计约束
//! 1. **零共享状态**：鉴权、会话、审计都是自己的一套。`adminApiKey` 泄露不会打开 portal，
//!    portal 用户被撞库也拿不到管理面。
//! 2. **明文只在响应里，不在磁盘上**：[`store`] 只存元数据，明文在请求时按
//!    `credential_id` 从活的凭据池回查。
//! 3. **默认关闭**：这个入口的能力是「读出可用的明文凭据」，公网暴露的后果是整池泄露。
//!    因此配置项默认 off，必须显式打开。
//!
//! # 部署前提
//! 网关自身是 **HTTP、无 TLS**。公网暴露必须前置 HTTPS（Nginx/Caddy/Cloudflare），
//! 否则密码和凭据都在链路上明文传输——那时任何应用层加固都是装饰。

pub mod admin_api;
pub mod admin_auth;
pub mod auth;
pub mod credits;
pub mod http;
pub mod page;
pub mod password;
pub mod role;
pub mod sink;
pub mod store;
pub mod throttle;

pub use admin_auth::PortalAdminAuth;
pub use auth::PortalAuth;
pub use store::PortalDb;

/// 审计表保留的最近条数上限。
///
/// 审计是「每次登录 + 每次明文外显」都写一条的高频表，公网暴露下更是撞库者的**写放大**
/// 目标：一次不限速的爆破就能灌进百万行。定期只留最近 N 条，与 traces 的按天清理同类。
/// 5000 条足够覆盖内部小规模场景的可追溯窗口，又不会让 SQLite 文件失控增长。
pub const MAX_AUDIT_ROWS: usize = 5000;

/// Portal 数据库文件名（与用量库同目录）。
pub const DB_FILE_NAME: &str = "portal.db";
