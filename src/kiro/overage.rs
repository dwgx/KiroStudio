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
    })
}

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

    // 回查真实状态，确认写入生效
    let usage = web_portal::get_user_usage_and_limits(
        &ctx.token,
        &ctx.idp,
        ctx.proxy.as_ref(),
        ctx.tls_backend,
    )
    .await?;
    let enabled = usage.overage_enabled();

    tracing::info!(
        credential_id = id,
        target_enabled,
        confirmed = ?enabled,
        "Overage 开关完成"
    );

    Ok(OverageStatus {
        id,
        enabled,
        has_profile_arn: true,
        supported: true,
    })
}
