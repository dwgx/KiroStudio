//! 主动 token 预刷新后台循环（批次4.4，G-12）
//!
//! 原有机制是「请求到来时按需刷新」：token 过期后第一个命中该凭据的请求要
//! 同步等待一次刷新往返，且多凭据同时过期会形成刷新突发。本模块在后台按固定
//! 间隔扫描，提前 `lead_minutes` 把将过期的 token 刷掉，把刷新从热路径移走。
//!
//! 设计取舍：
//! - 复用 [`MultiTokenManager::prefetch_refresh_token_for`]，其内部持 refresh_lock，
//!   与请求路径的按需刷新互斥；且拿锁后二次确认 token 仍将过期才刷新，避免重刷
//!   请求路径刚刷好的 token。
//! - 逐个（顺序）刷新而非并发，避免同一时刻对上游打出刷新突发（本就是要削的峰）。
//! - 单张凭据刷新失败：由 prefetch_refresh_token_for 内部按错误类型累计失败计数 /
//!   禁用坏凭据（与请求路径处置一致），本 loop 不中断整轮。
//! - 收到停机信号即退出（由 tokio::select! 在调用侧或 interval 上体现）。

use std::sync::Arc;
use std::time::Duration;

use crate::kiro::token_manager::MultiTokenManager;

/// 启动后台预刷新任务。返回的 `JoinHandle` 由调用方持有（通常直接 detach）。
///
/// `lead_minutes` 提前量、`interval_secs` 扫描间隔均来自配置。若两者为 0 或
/// 上层未启用，则不应调用本函数。
pub fn spawn(
    manager: Arc<MultiTokenManager>,
    lead_minutes: i64,
    interval_secs: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // 至少 5 秒一轮，避免误配 0 导致空转
        let period = Duration::from_secs(interval_secs.max(5));
        let mut ticker = tokio::time::interval(period);
        // 错过的 tick 直接跳过而非补偿，防止唤醒后连刷
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        tracing::info!(
            "主动 token 预刷新已启动：提前 {} 分钟，每 {} 秒扫描一次",
            lead_minutes,
            period.as_secs()
        );

        loop {
            ticker.tick().await;
            run_once(&manager, lead_minutes).await;
        }
    })
}

/// 执行一轮扫描 + 刷新。抽出便于单测。
async fn run_once(manager: &MultiTokenManager, lead_minutes: i64) {
    let due = manager.credentials_due_for_refresh(lead_minutes);
    if due.is_empty() {
        return;
    }
    tracing::debug!("预刷新：{} 张凭据将过期，开始逐个刷新", due.len());
    for id in due {
        // 条件刷新 + 失败处置均在 prefetch_refresh_token_for 内部完成
        manager.prefetch_refresh_token_for(id, lead_minutes).await;
    }
}
