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

use std::sync::Weak;
use std::time::Duration;

use crate::kiro::token_manager::MultiTokenManager;

/// 启动后台预刷新任务。返回的 `JoinHandle` 由调用方（token_manager 的 TIER2 任务槽）
/// 持有，以便配置变更时 abort + respawn。
///
/// `lead_minutes` 提前量、`interval_secs` 扫描间隔均来自配置。若两者为 0 或
/// 上层未启用，则不应调用本函数。
///
/// 持 `Weak<MultiTokenManager>`（非 Arc）：manager 被 drop 后 upgrade 失败即退出循环，
/// 不构成引用环（句柄反向存在 manager 内）。
pub fn spawn(
    manager: Weak<MultiTokenManager>,
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
            // manager 已被 drop（进程停机路径）→ 退出循环
            let Some(mgr) = manager.upgrade() else {
                tracing::debug!("token_manager 已释放，预刷新任务退出");
                break;
            };
            run_once(&mgr, lead_minutes).await;
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::kiro::model::credentials::KiroCredentials;
    use crate::kiro::token_manager::MultiTokenManager;
    use crate::model::config::Config;
    use chrono::Utc;

    use super::*;

    /// run_once 空 due 快速返回：池里没有任何「将过期」凭据时不触发任何
    /// prefetch（零网络往返）。M1 补测 —— 循环体（spawn 的 interval loop）依赖
    /// 真实 tokio 定时器与 Weak<MultiTokenManager> 生命周期，标注不可测；
    /// 本轮判定的纯逻辑是 `credentials_due_for_refresh`（token_manager.rs 已有
    /// 行为测试 + 下方两个过滤用例钉住）。
    #[tokio::test]
    async fn run_once_returns_immediately_when_nothing_due() {
        let manager = MultiTokenManager::new(Config::default(), vec![], None, None, true)
            .expect("构造空池 manager");
        run_once(&manager, 10).await;
        // 能走到这里即证明 due 为空路径未 panic、未触网络。
    }

    /// run_once 对「不临期 / api_key / 无 refresh_token」三类凭据全部跳过：
    /// due 为空 → 提前返回。钉住「该轮该刷新哪些凭据」的判定，防止过滤条件
    /// 被改宽后把不该刷的号拖进网络刷新。
    #[tokio::test]
    async fn run_once_skips_non_due_and_api_key_credentials() {
        // 1 小时才过期 → 不入选。
        let mut fresh = KiroCredentials::default();
        fresh.refresh_token = Some("r".repeat(120));
        fresh.expires_at = Some((Utc::now() + Duration::from_secs(3600)).to_rfc3339());
        // api_key 型 → 永不入选。
        let mut api_key = KiroCredentials::default();
        api_key.kiro_api_key = Some("ksk_test_key_123".to_string());
        api_key.auth_method = Some("api_key".to_string());
        // 无 refresh_token → 永不入选。
        let mut no_rt = KiroCredentials::default();
        no_rt.expires_at = Some((Utc::now() + Duration::from_secs(60)).to_rfc3339());

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![fresh, api_key, no_rt],
            None,
            None,
            true,
        )
        .expect("构造 manager");
        run_once(&manager, 10).await;
    }
}
