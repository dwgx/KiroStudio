//! 超额（overage）真开关逻辑
//!
//! 移植自 Foxfishc kiro.rs（MIT），但简化为同步（非 SSE）实现：
//! 直接调用 Web Portal 接口并返回最终结果，满足「能真开关」需求。
//!
//! ⚠️ 计费红线：
//! 1. overage 开启 = 超出 base 额度后按量真实付费；
//! 2. 所有开/关都是幂等的（重复调用不出错，最终以上游状态为准）；
//! 3. 每次开/关都会写审计日志（谁、何时、开/关了哪个号）；
//! 4. 不做任何自动/批量开启——只响应显式的单号 admin 请求；
//! 5. 默认不改动任何号的 overage 状态。

use std::sync::Arc;

use serde::Serialize;

use crate::kiro::token_manager::MultiTokenManager;
use crate::kiro::web_portal;

/// 单号 overage 状态快照（返回给 Admin API / 前端）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverageStatus {
    pub id: u64,
    /// 上游当前的 overage 开关状态（None 表示上游未上报该字段）
    pub enabled: Option<bool>,
    /// 是否具备 profileArn（开启 overage 的必要条件）
    pub has_profile_arn: bool,
    /// 该凭据是否支持 Web Portal（仅 social 凭据支持）
    pub supported: bool,
    /// 状态是否已确认与目标一致（None 表示只读查询、未做开关；
    /// set_overage 轮询命中目标为 Some(true)，超时未生效为 Some(false)）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmed: Option<bool>,
    /// 附加说明（如轮询超时提示），仅在需要时返回给前端
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 读取指定凭据的 overage 状态（只读，实时查询上游）。
pub async fn overage_status(
    manager: &Arc<MultiTokenManager>,
    id: u64,
) -> anyhow::Result<OverageStatus> {
    let ctx = manager.web_portal_context_for(id).await?;
    let usage = web_portal::get_user_usage_and_limits(
        &ctx.token,
        &ctx.idp,
        ctx.proxy.as_ref(),
        ctx.tls_backend,
    )
    .await?;

    Ok(OverageStatus {
        id,
        enabled: usage.overage_enabled(),
        has_profile_arn: ctx.profile_arn.is_some(),
        supported: true,
        confirmed: None,
        note: None,
    })
}

/// 轮询确认参数：提交后每 1 秒回查一次，最多 10 秒。
///
/// 上游 UpdateBillingPreferences 写入未必立即对 GetUserUsageAndLimits 生效，
/// 单次回查可能读到旧值。移植自 Foxfishc 的轮询确认思路（但收敛为同步返回、
/// 非 SSE），命中目标即返回，超时返回当前值并在 note 中标注。
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
const POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 开启或关闭指定凭据的 overage。
///
/// 幂等：无论目标状态与当前是否一致，都提交一次 UpdateBillingPreferences，
/// 再回查一次真实状态返回。重复调用不会报错。
///
/// ⚠️ `target_enabled = true` 会触发真实按量付费，仅在显式 admin 请求时调用。
pub async fn set_overage(
    manager: &Arc<MultiTokenManager>,
    id: u64,
    target_enabled: bool,
) -> anyhow::Result<OverageStatus> {
    let ctx = manager.web_portal_context_for(id).await?;

    let profile_arn = ctx.profile_arn.as_deref().ok_or_else(|| {
        anyhow::anyhow!("凭据缺少 profileArn，无法开关 overage（请先刷新 Token）")
    })?;

    // 审计日志：记录谁在何时开关了哪个号的 overage（计费敏感操作）
    tracing::info!(
        credential_id = id,
        target_enabled,
        idp = %ctx.idp,
        "Overage 开关请求：提交 UpdateBillingPreferences"
    );

    web_portal::update_billing_preferences(
        &ctx.token,
        &ctx.idp,
        profile_arn,
        target_enabled,
        ctx.proxy.as_ref(),
        ctx.tls_backend,
    )
    .await?;

    // 轮询回查：上游写入未必对 GetUserUsageAndLimits 立即生效，单次回查可能读到
    // 旧值。每 1 秒回查一次、最多 10 秒；命中目标即返回，超时返回当前值 + note 标注。
    // 注：这仍是显式单号操作触发的按需回查（非周期主动巡检），不会增加上游限流风险。
    let started = std::time::Instant::now();
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        // 首次立即查（很多情况上游是同步写入），之后每次 sleep 再查
        if attempt > 1 {
            tokio::time::sleep(POLL_INTERVAL).await;
        }

        let usage = web_portal::get_user_usage_and_limits(
            &ctx.token,
            &ctx.idp,
            ctx.proxy.as_ref(),
            ctx.tls_backend,
        )
        .await?;
        let last_enabled = usage.overage_enabled();

        // 命中目标状态：确认生效，返回
        if last_enabled == Some(target_enabled) {
            tracing::info!(
                credential_id = id,
                target_enabled,
                attempts = attempt,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "Overage 开关完成（已确认生效）"
            );
            return Ok(OverageStatus {
                id,
                enabled: last_enabled,
                has_profile_arn: true,
                supported: true,
                confirmed: Some(true),
                note: None,
            });
        }

        // 超时：不再等待，返回当前值并标注未确认（提交已成功，可能稍后才生效）
        if started.elapsed() >= POLL_TIMEOUT {
            let note = format!(
                "已提交 UpdateBillingPreferences，但 {}s 内未观察到上游 overageEnabled={} 生效（当前上游值 {:?}），稍后可刷新确认",
                POLL_TIMEOUT.as_secs(),
                target_enabled,
                last_enabled
            );
            tracing::warn!(
                credential_id = id,
                target_enabled,
                attempts = attempt,
                current = ?last_enabled,
                "Overage 开关提交成功但轮询超时未确认生效"
            );
            return Ok(OverageStatus {
                id,
                enabled: last_enabled,
                has_profile_arn: true,
                supported: true,
                confirmed: Some(false),
                note: Some(note),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 轮询确认参数应符合「每秒一次、最多 10 秒」的约定
    #[test]
    fn test_poll_params() {
        assert_eq!(POLL_INTERVAL.as_secs(), 1);
        assert_eq!(POLL_TIMEOUT.as_secs(), 10);
    }

    /// 只读查询：confirmed / note 为 None，不应出现在序列化输出里（skip_serializing_if）
    #[test]
    fn test_status_readonly_omits_confirmed_and_note() {
        let st = OverageStatus {
            id: 1,
            enabled: Some(true),
            has_profile_arn: true,
            supported: true,
            confirmed: None,
            note: None,
        };
        let json = serde_json::to_value(&st).unwrap();
        assert_eq!(json["enabled"], serde_json::json!(true));
        assert!(json.get("confirmed").is_none(), "只读查询不应带 confirmed 字段");
        assert!(json.get("note").is_none(), "只读查询不应带 note 字段");
    }

    /// 轮询超时：confirmed=false + note 应被序列化并携带说明
    #[test]
    fn test_status_timeout_carries_note() {
        let st = OverageStatus {
            id: 2,
            enabled: Some(false),
            has_profile_arn: true,
            supported: true,
            confirmed: Some(false),
            note: Some("超时未确认".to_string()),
        };
        let json = serde_json::to_value(&st).unwrap();
        assert_eq!(json["confirmed"], serde_json::json!(false));
        assert_eq!(json["note"], serde_json::json!("超时未确认"));
    }

    /// 命中目标：confirmed=true、note 省略
    #[test]
    fn test_status_confirmed_ok() {
        let st = OverageStatus {
            id: 3,
            enabled: Some(true),
            has_profile_arn: true,
            supported: true,
            confirmed: Some(true),
            note: None,
        };
        let json = serde_json::to_value(&st).unwrap();
        assert_eq!(json["confirmed"], serde_json::json!(true));
        assert!(json.get("note").is_none());
    }
}
