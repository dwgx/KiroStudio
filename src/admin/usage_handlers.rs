//! Admin 用量统计查询端点
//!
//! 只读地暴露 [`UsageStats`] 的内存预聚合与 [`TraceDb`] 的明细，供后台图表使用。
//! 统计未启用（`usage_enabled=false`）时相关句柄为 None，端点统一返回 503。

use std::convert::Infallible;
use std::time::Duration;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    Json,
};
use futures::Stream;
use serde::{Deserialize, Serialize};

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

/// GET /api/admin/usage/clients
/// 下游客户端 RPM 视图：每个客户端（按 IP/设备分组）当前 RPM + 活跃窗口数 + 各窗口 RPM。
/// 与 by-credential（选号维度）正交，这是**发起方**维度。
pub async fn usage_clients(State(state): State<AdminState>) -> impl IntoResponse {
    match &state.usage_stats {
        Some(stats) => Json(stats.clients()).into_response(),
        None => stats_disabled(),
    }
}

/// GET /api/admin/usage/machines
/// 机器维度 RPM 视图：按设备指纹（device+os+browser，会话粘滞）分组，**IP 变化不拆分**。
/// 修复同一台机器换 IP 被拆成多组的问题；IP 仅作"见过的 IP"列表展示。
pub async fn usage_machines(State(state): State<AdminState>) -> impl IntoResponse {
    match &state.usage_stats {
        Some(stats) => Json(stats.machines()).into_response(),
        None => stats_disabled(),
    }
}

/// GET /api/admin/usage/throughput
/// 全局实时吞吐快照：当前 RPM / RPS / tokens 每秒 + 最近 60 秒逐秒桶。
/// 供前端把趋势图渲染成会流动的粒子（密度∝RPS，速度∝tokens/s）。
/// 只读内存聚合，零上游调用。
pub async fn usage_throughput(State(state): State<AdminState>) -> impl IntoResponse {
    match &state.usage_stats {
        Some(stats) => Json(stats.throughput()).into_response(),
        None => stats_disabled(),
    }
}

/// GET /api/admin/usage/cache
/// 影子 prompt 缓存记账的累计命中率快照（进程级累计，只读，零上游）。
/// 供前端展示"缓存确实生效"：请求数 / 命中数 / 命中率 / 累计 cache_read/creation tokens。
/// 与 usage_enabled 无关（缓存记账独立），故不走 stats_disabled。
pub async fn usage_cache(State(_state): State<AdminState>) -> impl IntoResponse {
    Json(crate::anthropic::cache_stats_snapshot())
}

/// GET /api/admin/ratelimit/insights
/// 每号一条限流健康快照：rpm / 软上限 / 是否饱和 / 在途 / 冷却明细 / 近期 429 /
/// 中文推断文案。全部取自内存（token_manager 快照 + cooldown 快照 + config 软上限），
/// **零上游调用**（封号红线）。按 rpm 降序、id 升序。
pub async fn ratelimit_insights(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.ratelimit_insights())
}

/// SSE 实时流的一帧数据（camelCase）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LiveFrame {
    /// 全局在途请求数（所有号之和）
    global_inflight: u32,
    /// 全局最近 60 秒 RPM（所有号之和）
    global_rpm: u32,
    /// 每号精简状态
    creds: Vec<super::service::LiveCred>,
    /// 全局实时吞吐（当前 RPS / tokens 每秒）；统计未启用时为 null
    throughput: Option<LiveThroughput>,
}

/// SSE 帧内的吞吐精简部分
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LiveThroughput {
    /// 当前每秒请求数
    current_rps: f64,
    /// 当前每秒 tokens 吞吐
    tokens_per_sec: f64,
}

/// GET /api/admin/stream/live  (text/event-stream)
///
/// 每约 1.5 秒推送一帧轻量快照 {globalInflight, globalRpm, creds:[...], throughput:{...}}。
/// 只读内存零上游。用 KeepAlive 防中间件断流；客户端断开时 axum 会 drop 该流自动结束。
pub async fn stream_live(
    State(state): State<AdminState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // 用 tokio interval 作为节拍源；每个 tick 生成一帧。
    // 首个 tick 立即触发（Interval 默认行为），让客户端连上即拿到首帧。
    let interval = tokio::time::interval(Duration::from_millis(1500));
    let init = (state, interval);

    let stream = futures::stream::unfold(init, |(state, mut interval)| async move {
        interval.tick().await;

        let (global_inflight, global_rpm, creds) = state.service.live_creds();
        let throughput = state.usage_stats.as_ref().map(|s| {
            let t = s.throughput();
            LiveThroughput {
                current_rps: t.current_rps,
                tokens_per_sec: t.current_tokens_per_sec,
            }
        });

        let frame = LiveFrame {
            global_inflight,
            global_rpm,
            creds,
            throughput,
        };

        // 序列化失败极不可能（结构均为 Serialize）；失败则跳过该帧的数据但仍保持流。
        let event = match Event::default().json_data(&frame) {
            Ok(ev) => ev,
            Err(_) => Event::default().comment("frame-serialize-error"),
        };

        Some((Ok(event), (state, interval)))
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}
