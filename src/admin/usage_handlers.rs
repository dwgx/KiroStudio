//! Admin 用量统计查询端点
//!
//! 只读地暴露 [`UsageStats`] 的内存预聚合与 [`TraceDb`] 的明细，供后台图表使用。
//! 统计未启用（`usage_enabled=false`）时相关句柄为 None，端点统一返回 503。

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use super::{middleware::AdminState, types::AdminErrorResponse};

/// 统计未启用时的统一响应
fn stats_disabled() -> axum::response::Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(AdminErrorResponse::new(
            "stats_disabled",
            "用量统计未启用（usage_enabled=false）",
        )),
    )
        .into_response()
}

/// GET /api/admin/usage/overview
/// 最近 24h / 7d / 30d 三窗口概览
pub async fn usage_overview(State(state): State<AdminState>) -> impl IntoResponse {
    match &state.usage_stats {
        Some(stats) => Json(stats.overview()).into_response(),
        None => stats_disabled(),
    }
}

/// 时间序列查询参数
#[derive(Debug, Deserialize)]
pub struct TimeseriesQuery {
    /// 粒度："hourly"（默认）或 "daily"
    #[serde(default)]
    pub granularity: Option<String>,
}

/// GET /api/admin/usage/timeseries?granularity=hourly|daily
/// 按小时（默认最近 48 点）或天（默认最近 30 点）的时间序列
pub async fn usage_timeseries(
    State(state): State<AdminState>,
    Query(query): Query<TimeseriesQuery>,
) -> impl IntoResponse {
    let Some(stats) = &state.usage_stats else {
        return stats_disabled();
    };
    let series = match query.granularity.as_deref() {
        Some("daily") => stats.timeseries_daily(),
        _ => stats.timeseries_hourly(),
    };
    Json(series).into_response()
}

/// GET /api/admin/usage/by-model
/// 按模型分组的累计统计
pub async fn usage_by_model(State(state): State<AdminState>) -> impl IntoResponse {
    match &state.usage_stats {
        Some(stats) => Json(stats.by_model()).into_response(),
        None => stats_disabled(),
    }
}

/// GET /api/admin/usage/by-credential
/// 按凭据分组的累计统计
pub async fn usage_by_credential(State(state): State<AdminState>) -> impl IntoResponse {
    match &state.usage_stats {
        Some(stats) => Json(stats.by_credential()).into_response(),
        None => stats_disabled(),
    }
}

/// recent traces 查询参数
#[derive(Debug, Deserialize)]
pub struct RecentQuery {
    /// 返回条数上限（默认 100，最大 1000）
    #[serde(default)]
    pub limit: Option<usize>,
}

/// GET /api/admin/usage/recent?limit=N
/// 最近 N 条请求明细（按时间倒序）
pub async fn usage_recent(
    State(state): State<AdminState>,
    Query(query): Query<RecentQuery>,
) -> impl IntoResponse {
    let Some(db) = &state.trace_db else {
        return stats_disabled();
    };
    let limit = query.limit.unwrap_or(100).clamp(1, 1000);
    match db.recent(limit) {
        Ok(records) => Json(records).into_response(),
        Err(e) => {
            tracing::warn!("查询用量明细失败: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AdminErrorResponse::internal_error(format!(
                    "查询用量明细失败: {e}"
                ))),
            )
                .into_response()
        }
    }
}

/// rate 查询参数
#[derive(Debug, Deserialize)]
pub struct RateQuery {
    /// 目标凭据 ID
    pub credential_id: u64,
}

/// GET /api/admin/usage/rate?credential_id=N
/// 指定凭据最近 10 分钟的每 30 秒请求数（G-14 速率环）
pub async fn usage_rate(
    State(state): State<AdminState>,
    Query(query): Query<RateQuery>,
) -> impl IntoResponse {
    match &state.usage_stats {
        Some(stats) => Json(stats.recent_rate(query.credential_id)).into_response(),
        None => stats_disabled(),
    }
}
