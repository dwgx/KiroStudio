//! Admin 用量统计查询端点
//!
//! 只读地暴露 [`UsageStats`] 的内存预聚合与 [`TraceDb`] 的明细，供后台图表使用。
//! 统计未启用（`usage_enabled=false`）时相关句柄为 None，端点统一返回 503。

use std::convert::Infallible;
use std::time::Duration;

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
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

/// 用量管道背压计数（`/api/admin/usage/overview` 的 `pipeline` 字段）。
///
/// # 为什么要挂在 overview 上
/// 同一个响应里给出「窗口统计」与「这批统计漏了多少」，读数的人才有机会判断可信度。
/// 若只放在 `/recovery-metrics`（那里也有，见 `usage_pipeline_dropped`），
/// 看成功率的人不会顺手去查另一个端点 —— 而恰恰是看成功率的人需要这个数。
///
/// # 判读
/// `dropped_records` 非零即意味着 `last_24h`/`last_7d` 等窗口的 requests/success/token
/// **少算了这么多条**（热路径为了不阻塞请求主动放弃）。单看它无法判断严重程度，
/// 故配对给出 `written_records` 与派生的 `drop_rate`。
#[derive(Debug, Clone, Serialize)]
pub struct PipelineHealth {
    /// 有界通道满、`try_send` 失败而被丢弃的记录数（自进程启动累计）。
    pub dropped_records: u64,
    /// worker 真分发给全部 sink 的记录数（即进了聚合/SQLite 的那批）。
    pub written_records: u64,
    /// 丢弃率 = dropped/(dropped+written)。分母为 0 时给 0.0。
    pub drop_rate: f64,
}

impl PipelineHealth {
    /// 从管道的进程级计数器取当前值。
    fn current() -> Self {
        let dropped = crate::usage::pipeline::dropped_count();
        let written = crate::usage::pipeline::written_count();
        let total = dropped + written;
        Self {
            dropped_records: dropped,
            written_records: written,
            // 分母 0（进程刚起、一条都没走过）时给 0.0 而不是 NaN：NaN 会被
            // serde_json 序列化成 `null`，前端做数值比较时静默变成"没问题"。
            drop_rate: if total == 0 {
                0.0
            } else {
                dropped as f64 / total as f64
            },
        }
    }
}

/// `GET /api/admin/usage/overview` 的响应体。
///
/// `windows` 用 `#[serde(flatten)]` 摊平，使既有的 `last_24h` / `last_7d` / `last_30d` /
/// `all_time` **仍在顶层** —— 前端契约不变，本次只是**新增**兄弟字段 `pipeline`。
/// （`Overview` 在 `usage_stats.rs`，不在本次可改文件范围内，故用包装而非给它加字段。）
///
/// `dropped` / `parse_errors` 两个键是既有出口（已知问题 #12），必须保留在顶层：
/// 既有前端类型按这两个键读取（管道满丢弃 + JSONL 重放解析失败），删掉会让面板
/// 静默丢失丢失率的可观测性。`pipeline` 提供的是配对后的 written/drop_rate 口径，
/// 二者并存不冲突。
#[derive(Debug, Clone, Serialize)]
pub struct OverviewResponse {
    /// 三窗口 + all_time 统计（摊平到顶层）。
    #[serde(flatten)]
    pub windows: crate::usage::usage_stats::Overview,
    /// 管道满丢弃的原始计数（既有出口，与 `pipeline.dropped_records` 同源）。
    pub dropped: u64,
    /// JSONL 重放解析失败数（既有出口）。
    pub parse_errors: u64,
    /// 丢弃配对计数（written + 派生 drop_rate）。
    pub pipeline: PipelineHealth,
}

/// GET /api/admin/usage/overview
/// 最近 24h / 7d / 30d 三窗口概览 + 用量管道丢弃计数
pub async fn usage_overview(State(state): State<AdminState>) -> impl IntoResponse {
    match &state.usage_stats {
        Some(stats) => Json(OverviewResponse {
            windows: stats.overview(),
            dropped: crate::usage::pipeline::dropped_count(),
            parse_errors: stats.parse_error_count(),
            pipeline: PipelineHealth::current(),
        })
        .into_response(),
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
/// 按「上游实际服务模型」分组的累计统计（映射双口径的 upstream 维度：
/// key = `upstream_model` 映射后名，None 回落 `model`）。
///
/// 成本估算：单价表取自当前配置快照（空表 = 不估算），命中模型的行下发 `cost`。
pub async fn usage_by_model(State(state): State<AdminState>) -> impl IntoResponse {
    match &state.usage_stats {
        Some(stats) => Json(stats.by_model(&state.service.model_pricing())).into_response(),
        None => stats_disabled(),
    }
}

/// GET /api/admin/usage/by-requested-model
/// 按「客户端请求的原始模型名」分组的累计统计（映射双口径的 requested 维度）。
pub async fn usage_by_requested_model(State(state): State<AdminState>) -> impl IntoResponse {
    match &state.usage_stats {
        Some(stats) => Json(stats.by_requested_model(&state.service.model_pricing()))
            .into_response(),
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

/// GET /api/admin/usage/by-outcome
/// 按结果分类（success/rate_limited/auth_failed/...）分组的累计统计。
///
/// 解决 A2/F1：`Aggregate` 只有 success/failure 二值，429/配额/auth 分布画不出，
/// 只能 `traces/search?outcome=` 逐条过滤。本端点下发全量累计的 outcome 分布
/// （key = snake_case outcome 名，各 key 的 requests 之和恒等于总请求数）。
pub async fn usage_by_outcome(State(state): State<AdminState>) -> impl IntoResponse {
    match &state.usage_stats {
        Some(stats) => Json(stats.by_outcome()).into_response(),
        None => stats_disabled(),
    }
}

/// recent traces 查询参数
#[derive(Debug, Deserialize)]
pub struct RecentQuery {
    /// 返回条数上限。语义见 [`resolve_recent_limit`]：
    /// - 缺省 → 默认 100 条
    /// - 0    → 前端"全部"，取到硬上限 [`MAX_RECENT_LIMIT`] 为止
    /// - 其它 → 裁剪到 [1, MAX_RECENT_LIMIT]
    #[serde(default)]
    pub limit: Option<usize>,
}

/// 最近请求明细返回条数的硬上限（兜底：全量查询也不至于把服务/前端拖垮）。
///
/// dwgx 需求「最近请求支持真全部」：前端"全部"选项传 `limit=0`，服务端解释为
/// 「取到该硬上限为止」。前端表格分页渲染（每页 20 行），故不存在 DOM 爆炸。
///
/// # 为什么从 50000 降到 2000（实测驱动）
///
/// 原注释断言「5 万条对本地 SQLite 单次查询与 JSON 序列化均可控」——**线上实测不成立**：
/// 13.5 万行的库上取 5 万行需 **42ms**、原始文本 **6.5MB**（JSON 序列化后更大），
/// 而不是此前假定的 admin API 那个 0.3ms 量级（差约 140 倍）。
///
/// 真正的危害不在响应慢，而在**持锁**：`TraceDb` 是单连接 + 一把 `parking_lot::Mutex`
/// （`trace_db.rs:132-135`），用量写入管道（专用 OS 线程）与本查询**共享**这把锁。
/// 一次 42ms 的持锁会让写侧排队，排满 `CHANNEL_CAPACITY`(10_000) 后 `try_send` 失败
/// → **静默丢弃真实请求记录**（`pipeline.rs` 只在 2 的幂次告警）。
/// 即「点一下面板的『全部』」会造成用量数据丢失。
///
/// 2000 条够任何人工排障翻页（前端每页 20 行 = 100 页），更大范围应走
/// `traces_search` 的分页/过滤而不是一次性拉全量。
///
/// ⚠️ 不要用 `spawn_blocking` 代替降上限：那只解决"不占 tokio worker"，
/// 锁照样被占死 42ms、写侧照样排队 —— 它解决的是三个问题里最不重要的那个。
pub const MAX_RECENT_LIMIT: usize = 2_000;

/// 解析「最近请求」的实际取数条数（纯函数，便于单测）。
///
/// - `None`（缺省参数）→ 默认 100 条
/// - `Some(0)` → 前端"全部"，取到硬上限 [`MAX_RECENT_LIMIT`]
/// - `Some(n)` → 裁剪到 `[1, MAX_RECENT_LIMIT]`
pub fn resolve_recent_limit(limit: Option<usize>) -> usize {
    match limit {
        None => 100,
        Some(0) => MAX_RECENT_LIMIT,
        Some(n) => n.clamp(1, MAX_RECENT_LIMIT),
    }
}

/// GET /api/admin/usage/recent?limit=N
/// 最近 N 条请求明细（按时间倒序）。`limit=0` 表示"全部"（取到硬上限）。
pub async fn usage_recent(
    State(state): State<AdminState>,
    Query(query): Query<RecentQuery>,
) -> impl IntoResponse {
    let Some(db) = &state.trace_db else {
        return stats_disabled();
    };
    let limit = resolve_recent_limit(query.limit);
    match db.recent(limit) {
        Ok(records) => Json(records).into_response(),
        Err(e) => {
            // ⚠️ 内部错误细节只进服务端日志，绝不下发客户端（MINOR-5：500 响应体
            // 暴露 SQLite/内部路径会给攻击者情报，且对前端排障无益）。
            tracing::warn!("查询用量明细失败: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AdminErrorResponse::internal_error("查询用量明细失败，请稍后重试")),
            )
                .into_response()
        }
    }
}

/// trace 明细搜索/过滤/分页查询参数（camelCase，全部可选）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TracesSearchQuery {
    /// 模型精确匹配
    #[serde(default)]
    pub model: Option<String>,
    /// 客户端请求的**原始**模型名精确匹配（映射双口径的 requested 维度）
    #[serde(default)]
    pub requested_model: Option<String>,
    /// 凭据 ID 精确匹配
    #[serde(default)]
    pub credential_id: Option<u64>,
    /// 客户端 IP 子串匹配
    #[serde(default)]
    pub client_ip: Option<String>,
    /// 会话 ID 精确匹配
    #[serde(default)]
    pub session_id: Option<String>,
    /// 结果精确匹配（success/rate_limited/...）
    #[serde(default)]
    pub outcome: Option<String>,
    /// 时间范围起点（Unix 毫秒，含）
    #[serde(default)]
    pub ts_from: Option<i64>,
    /// 时间范围终点（Unix 毫秒，含）
    #[serde(default)]
    pub ts_to: Option<i64>,
    /// 全文子串匹配 error_message / request_id / model
    #[serde(default)]
    pub text: Option<String>,
    /// 是否流式
    #[serde(default)]
    pub is_streaming: Option<bool>,
    /// 单页条数（默认 50，服务端裁剪到 [1, 500]）
    #[serde(default)]
    pub limit: Option<usize>,
    /// 偏移（默认 0）
    #[serde(default)]
    pub offset: Option<usize>,
}

impl TracesSearchQuery {
    /// 把空串归一为 None（前端清空过滤框常传空串，避免退化成「精确匹配空值」）。
    fn norm(v: Option<String>) -> Option<String> {
        v.and_then(|s| {
            let t = s.trim().to_string();
            if t.is_empty() { None } else { Some(t) }
        })
    }

    /// 映射为存储层 [`TraceFilter`]。
    fn to_filter(&self) -> crate::usage::TraceFilter {
        crate::usage::TraceFilter {
            model: Self::norm(self.model.clone()),
            requested_model: Self::norm(self.requested_model.clone()),
            credential_id: self.credential_id,
            client_ip: Self::norm(self.client_ip.clone()),
            session_id: Self::norm(self.session_id.clone()),
            outcome: Self::norm(self.outcome.clone()),
            ts_from: self.ts_from,
            ts_to: self.ts_to,
            text: Self::norm(self.text.clone()),
            is_streaming: self.is_streaming,
        }
    }
}

/// GET /api/admin/usage/traces/search
/// 多维过滤 + 分页查询请求明细，返回 `{items: [...], total: N}`。
/// items 按 ts_ms 倒序、单页最多 500 条；total 为同过滤条件下的匹配总数（供分页）。
pub async fn traces_search(
    State(state): State<AdminState>,
    Query(query): Query<TracesSearchQuery>,
) -> impl IntoResponse {
    let Some(db) = &state.trace_db else {
        return stats_disabled();
    };
    let filter = query.to_filter();
    let limit = query.limit.unwrap_or(50);
    let offset = query.offset.unwrap_or(0);

    // 先查总数再取本页；两处同一 filter 保证 total 与 items 口径一致。
    let total = match db.count_filtered(&filter) {
        Ok(n) => n,
        Err(e) => {
            // ⚠️ 内部错误细节只进服务端日志（MINOR-5），见 usage_recent 同款注释。
            tracing::warn!("统计 trace 明细失败: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AdminErrorResponse::internal_error("统计 trace 明细失败，请稍后重试")),
            )
                .into_response();
        }
    };

    match db.search(&filter, limit, offset) {
        Ok(items) => Json(serde_json::json!({ "items": items, "total": total })).into_response(),
        Err(e) => {
            tracing::warn!("查询 trace 明细失败: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AdminErrorResponse::internal_error("查询 trace 明细失败，请稍后重试")),
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
        // 保活心跳（占位，逻辑不变）
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

// ============ 运维日志:内存环形缓冲拉取 / 实时流 / 一键导出 ============

/// 日志查询参数:增量游标 since(seq)+ 最低级别 level(ERROR/WARN/INFO/DEBUG)。
#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    /// 只取 seq > since 的(轮询增量);缺省取全部环形缓冲。
    #[serde(default)]
    pub since: Option<u64>,
    /// 最低级别过滤(≥):如 level=WARN 只返回 WARN+ERROR。缺省不过滤。
    #[serde(default)]
    pub level: Option<String>,
}

/// GET /api/admin/logs?since=N&level=WARN
/// 拉取内存环形缓冲的最近日志(增量 + 级别过滤)。零上游、纯内存。
pub async fn logs_poll(Query(q): Query<LogsQuery>) -> impl IntoResponse {
    let entries = crate::common::log_buffer::snapshot(q.since, q.level.as_deref());
    Json(serde_json::json!({ "logs": entries })).into_response()
}

/// GET /api/admin/logs/stream
/// SSE 实时日志直播:先回放当前环形缓冲(补上下文),再逐条推送新日志。面板不必 SSH/tail。
/// 用 unfold 状态机(与 stream_live 同范式,不引新依赖):状态 = (待回放队列, broadcast 接收端)。
pub async fn logs_stream() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = crate::common::log_buffer::subscribe();
    // 回放最近缓冲(补上下文),VecDeque 便于 pop_front 逐条吐。
    let replay: std::collections::VecDeque<_> =
        crate::common::log_buffer::snapshot(None, None).into();

    let stream = futures::stream::unfold((replay, rx), |(mut replay, mut rx)| async move {
        // 先吐完回放队列。
        if let Some(entry) = replay.pop_front() {
            let ev = Event::default()
                .json_data(&entry)
                .unwrap_or_else(|_| Event::default().comment("log-serialize-error"));
            return Some((Ok(ev), (replay, rx)));
        }
        // 回放完毕,进入实时推送。滞后跳过、关闭结束流。
        loop {
            match rx.recv().await {
                Ok(entry) => {
                    let ev = Event::default()
                        .json_data(&entry)
                        .unwrap_or_else(|_| Event::default().comment("log-serialize-error"));
                    return Some((Ok(ev), (replay, rx)));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

/// GET /api/admin/logs/export?level=WARN
/// 把环形缓冲打包成 JSONL 下载(每行一条 LogEntry),供用户直接附到 bug 报告——不必 SSH/grep。
pub async fn logs_export(Query(q): Query<LogsQuery>) -> impl IntoResponse {
    use axum::http::header;
    let entries = crate::common::log_buffer::snapshot(q.since, q.level.as_deref());
    let mut body = String::with_capacity(entries.len() * 128);
    for e in &entries {
        // 每条一行 JSON(JSONL)。序列化极不可能失败(纯 Serialize 结构),失败则跳过该行。
        if let Ok(line) = serde_json::to_string(e) {
            body.push_str(&line);
            body.push('\n');
        }
    }
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let filename = format!("kirostudio-logs-{}.jsonl", ts);
    (
        [
            (header::CONTENT_TYPE, "application/x-ndjson".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", filename),
            ),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::{MAX_RECENT_LIMIT, resolve_recent_limit};

    #[test]
    fn test_resolve_recent_limit_default_when_absent() {
        // 缺省参数 → 默认 100 条
        assert_eq!(resolve_recent_limit(None), 100);
    }

    #[test]
    fn test_resolve_recent_limit_zero_means_all() {
        // limit=0 是前端"全部"的约定 → 取到硬上限
        assert_eq!(resolve_recent_limit(Some(0)), MAX_RECENT_LIMIT);
    }

    #[test]
    fn test_resolve_recent_limit_normal_values_pass_through() {
        assert_eq!(resolve_recent_limit(Some(1)), 1);
        assert_eq!(resolve_recent_limit(Some(200)), 200);
        // 用**符号**而非字面量：硬上限已从 50000 降到 2000（见 MAX_RECENT_LIMIT 的说明），
        // 写死字面量会让上限的每次调整都连带改这里，且容易写出 > 上限的值。
        assert_eq!(
            resolve_recent_limit(Some(MAX_RECENT_LIMIT)),
            MAX_RECENT_LIMIT
        );
    }

    #[test]
    fn test_resolve_recent_limit_clamped_to_hard_cap() {
        // 超过硬上限（含旧的 5000 之上）一律裁剪到 MAX_RECENT_LIMIT，防拖垮服务
        assert_eq!(
            resolve_recent_limit(Some(MAX_RECENT_LIMIT + 1)),
            MAX_RECENT_LIMIT
        );
        assert_eq!(resolve_recent_limit(Some(usize::MAX)), MAX_RECENT_LIMIT);
        // 回归（实测驱动）：上限必须足够小，使单次查询的**持锁时间**不会顶住用量写入管道。
        //
        // TraceDb 是单连接 + 一把 parking_lot::Mutex，写管道（专用 OS 线程）与 admin 查询
        // 共享它。线上实测 13.5 万行的库取 5 万行需 42ms / 6.5MB —— 那会让写侧排队，
        // 排满 CHANNEL_CAPACITY(10_000) 即**静默丢真实请求记录**。
        // 也就是说「点一下面板的『全部』」会造成用量数据丢失。
        // 若有人把上限调回万级，这条断言会 FAIL 并指回这段说明。
        assert!(
            MAX_RECENT_LIMIT <= 5_000,
            "MAX_RECENT_LIMIT={MAX_RECENT_LIMIT} 过大：单次查询持锁会顶住用量写入管道并丢记录。\
             需要更大范围请走 traces_search 的分页/过滤，不要一次性拉全量"
        );
    }

    /// ⭐ 源码守卫（MINOR-5）：500 响应体不得携带内部错误细节。
    ///
    /// 三处 500 收口（usage_recent / traces_search 的 count 与 search）都把内部错误
    /// 细节（SQLite 错误、路径等）写进了响应体 —— 那是给攻击者的情报，且对前端
    /// 排障无益。修复后 `{e}` 只进 `tracing::warn!`，响应体是通用文案。
    ///
    /// 回退即 FAIL：任一处把响应体改回 `internal_error(format!("...: {e}"))` ——
    /// `internal_error(format!(` 计数从 0 变 1，本条红。
    #[test]
    fn internal_error_bodies_must_not_carry_error_details() {
        let src = include_str!("usage_handlers.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        // 响应体不再用 format! 拼错误详情（{e} 只属于 tracing::warn! 那侧）。
        let formatted = format!("internal_error(format{}", "!(");
        assert_eq!(
            prod.matches(formatted.as_str()).count(),
            0,
            "500 响应体不得用 format! 拼内部错误细节（{{e}} 只留在 tracing::warn! 侧）——\
             泄漏 SQLite/内部路径等于给攻击者情报"
        );
        // 通用文案必须存在（三处收口各一条）。
        for msg in ["查询用量明细失败，请稍后重试", "统计 trace 明细失败，请稍后重试"] {
            assert!(
                prod.contains(msg),
                "500 响应体必须是通用文案（缺: {msg}）"
            );
        }
        // 日志侧仍必须保留完整错误（{:#} 或 {e} 至少一处）—— 修文案不能把排障信息也删了。
        assert!(
            prod.contains("tracing::warn!(\"查询用量明细失败: {:#}\", e)")
                || prod.contains("tracing::warn!(\"统计 trace 明细失败: {:#}\", e)")
                || prod.contains("tracing::warn!(\"查询 trace 明细失败: {:#}\", e)"),
            "tracing::warn! 必须保留完整错误（响应体去细节 ≠ 日志去细节）"
        );
    }

    // ===== 端点级：retries 指标必须真的出现在 HTTP 响应体里 =====
    //
    // 为什么要在**端点**层再测一遍（`usage_stats.rs` 已测过 DTO 序列化）：
    // 那边测的是 `serde_json::to_string(dto)`，而这里过的是 axum 的 `Json(..)`
    // + `IntoResponse` 全链路。两者之间还能出岔子（handler 拿错方法、包了层
    // 别的 DTO、状态未启用时静默 503），只有读**响应体**才能证明前端真拿得到。

    use std::sync::Arc;

    use axum::body::to_bytes;
    use axum::extract::{Query, State};
    use axum::response::IntoResponse;

    use super::{
        TimeseriesQuery, usage_by_credential, usage_by_model, usage_by_outcome, usage_overview,
        usage_timeseries,
    };
    use crate::admin::middleware::AdminState;
    use crate::admin::service::AdminService;
    use crate::kiro::token_manager::MultiTokenManager;
    use crate::usage::pipeline::UsageSink;
    use crate::usage::record::{RequestOutcome, RequestRecord};
    use crate::usage::usage_stats::UsageStats;

    /// 造一个最小 AdminService（单个 api_key 凭据，零上游调用）。
    fn mk_service() -> AdminService {
        let mut c = crate::kiro::model::credentials::KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("ksk_test".to_string());
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![c],
                None,
                None,
                false,
            )
            .expect("构造 token manager"),
        );
        AdminService::new(tm, Vec::<String>::new())
    }

    /// 造一个挂了用量统计的 AdminState：3 条记录（2 条各重试 3 次 + 1 条零重试）。
    fn state_with_retry_records() -> AdminState {
        let stats = Arc::new(UsageStats::new(
            std::env::temp_dir().join("kiro_usage_handlers_test_ignore"),
        ));
        for retries in [3u32, 3, 0] {
            let mut r = RequestRecord::new("req", "sonnet");
            r.credential_id = Some(9);
            r.outcome = RequestOutcome::RateLimited;
            r.input_tokens = 10;
            r.output_tokens = 5;
            r.latency_ms = 100;
            r.retries = retries;
            stats.on_record(&r);
        }
        let mut st = AdminState::new("k", mk_service());
        st.usage_stats = Some(stats);
        st
    }

    /// 取 handler 响应体文本（响应体不大，直接全读）。
    async fn body_text(resp: axum::response::Response) -> String {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// ⭐ 回归（已知问题 #21 出口部分）：`GET /usage/overview` 必须下发 retries 两项计数。
    ///
    /// 删掉 `WindowSummary` 的 `retries_sum:` 那行 → 编译期就断（字段必填），
    /// 改成 `#[serde(skip)]` 或加 `rename_all = "camelCase"` → 本测试 FAILED。
    #[tokio::test]
    async fn overview_endpoint_emits_retries_fields() {
        let body = body_text(
            usage_overview(State(state_with_retry_records()))
                .await
                .into_response(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let w = &v["last_24h"];
        assert_eq!(w["retries_sum"], 6, "{body}");
        assert_eq!(w["retried_requests"], 2, "{body}");
        assert_eq!(w["avg_retries_per_request"], 2.0, "6/3 = 2.0；{body}");
        assert_eq!(w["avg_retries_when_retried"], 3.0, "6/2 = 3.0；{body}");
        // 前端类型按 snake_case 写死 —— camelCase 出现即等于前端读不到
        assert!(!body.contains("retriesSum"), "出口不得 camelCase：{body}");
    }

    /// `GET /usage/timeseries` 两种粒度都必须带 retries（漏一个 = 切粒度后趋势归零）。
    #[tokio::test]
    async fn timeseries_endpoint_emits_retries_for_both_granularities() {
        for g in ["hourly", "daily"] {
            let st = state_with_retry_records();
            let resp = usage_timeseries(
                State(st),
                Query(TimeseriesQuery {
                    granularity: Some(g.to_string()),
                }),
            )
            .await
            .into_response();
            let body = body_text(resp).await;
            let pts: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
            let last = pts.last().expect("至少一个桶");
            assert_eq!(last["retries_sum"], 6, "granularity={g}；{body}");
            assert_eq!(last["retried_requests"], 2, "granularity={g}；{body}");
        }
    }

    /// `GET /usage/by-model` 与 `/usage/by-credential` 都走 `GroupStat`，两条路径各断言一次。
    #[tokio::test]
    async fn group_endpoints_emit_retries_fields() {
        let by_model = body_text(
            usage_by_model(State(state_with_retry_records()))
                .await
                .into_response(),
        )
        .await;
        let rows: Vec<serde_json::Value> = serde_json::from_str(&by_model).unwrap();
        let m = rows.iter().find(|r| r["key"] == "sonnet").unwrap();
        assert_eq!(m["retries_sum"], 6, "{by_model}");
        assert_eq!(m["retried_requests"], 2, "{by_model}");
        assert_eq!(m["avg_retries_per_request"], 2.0, "{by_model}");

        let by_cred = body_text(
            usage_by_credential(State(state_with_retry_records()))
                .await
                .into_response(),
        )
        .await;
        let rows: Vec<serde_json::Value> = serde_json::from_str(&by_cred).unwrap();
        let c = rows.iter().find(|r| r["key"] == "9").unwrap();
        assert_eq!(c["retries_sum"], 6, "{by_cred}");
        assert_eq!(c["retried_requests"], 2, "{by_cred}");
    }

    /// ⭐ 回归（F1/A2 的 API 出口）：`GET /usage/by-outcome` 必须真的下发按 outcome
    /// 分组的行 —— 过 axum `Json(..)` + `IntoResponse` 全链路，响应体即前端所见。
    ///
    /// 删掉 handler（或改成 stats_disabled 之外的空返回）→ 本测试 FAILED。
    #[tokio::test]
    async fn by_outcome_endpoint_emits_grouped_rows() {
        // 2 条 rate_limited（各重试 3 次）+ 1 条 success 零重试
        let stats = Arc::new(UsageStats::new(
            std::env::temp_dir().join("kiro_usage_handlers_test_ignore"),
        ));
        for (outcome, retries) in [
            (RequestOutcome::RateLimited, 3u32),
            (RequestOutcome::RateLimited, 3u32),
            (RequestOutcome::Success, 0u32),
        ] {
            let mut r = RequestRecord::new("req", "sonnet");
            r.credential_id = Some(9);
            r.outcome = outcome;
            r.input_tokens = 10;
            r.output_tokens = 5;
            r.latency_ms = 100;
            r.retries = retries;
            stats.on_record(&r);
        }
        let mut st = AdminState::new("k", mk_service());
        st.usage_stats = Some(stats);

        let body = body_text(usage_by_outcome(State(st)).await.into_response()).await;
        let rows: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        let rate_limited = rows.iter().find(|r| r["key"] == "rate_limited").unwrap();
        assert_eq!(rate_limited["requests"], 2, "{body}");
        // GroupStat 复用：retries 两个口径随行下发（前端可看「每个 outcome 烧了多少重试」）
        assert_eq!(rate_limited["retries_sum"], 6, "{body}");
        assert_eq!(rate_limited["retried_requests"], 2, "{body}");
        let success = rows.iter().find(|r| r["key"] == "success").unwrap();
        assert_eq!(success["requests"], 1, "{body}");
        assert_eq!(success["retries_sum"], 0, "{body}");
        // snake_case key 契约（camelCase 出现 = 前端读不到）
        assert!(!body.contains("rateLimited"), "出口不得 camelCase：{body}");
        // 不存在的 outcome 不得出现（只下发命中变体）
        assert!(
            rows.iter().all(|r| r["key"] != "auth_failed"),
            "未命中的 outcome 不应下发：{body}"
        );
    }

    /// ⭐ 回归（已知问题 #12）：统计丢失（管道满丢弃 / JSONL 重放解析失败）必须可观测。
    ///
    /// 删除 `usage_overview` 里的 `dropped` / `parse_errors` 两个键 → 本测试 FAILED。
    #[tokio::test]
    async fn overview_endpoint_emits_dropped_and_parse_error_counters() {
        // 造一个含坏行的 JSONL 目录，rebuild 后 parse_errors = 1
        let dir = std::env::temp_dir().join(format!(
            "kiro_usage_ov_obs_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 文件名日期必须落在 rebuild 的 31 天环形窗内（P1-3）；写死 2026-07-03
        // 在 2026-08-21 会被跳过，parse_errors 假 0。
        let today = chrono::Utc::now().format("%Y-%m-%d");
        std::fs::write(dir.join(format!("usage-{today}.jsonl")), "NOT JSON\n").unwrap();

        let stats = Arc::new(UsageStats::new(dir.clone()));
        stats.rebuild_from_logs();
        assert_eq!(stats.parse_error_count(), 1);

        let mut st = AdminState::new("k", mk_service());
        st.usage_stats = Some(stats);
        let body = body_text(usage_overview(State(st)).await.into_response()).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["parse_errors"], 1, "{body}");
        assert!(v.get("dropped").is_some(), "dropped 计数必须出现在 overview：{body}");
        // 追加出口不得改变既有形状：四个窗口字段仍在
        assert!(v.get("last_24h").is_some(), "{body}");
        assert!(v.get("all_time").is_some(), "{body}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ===== 用量管道丢弃计数的出口 =====
    //
    // 缺陷背景：`pipeline::dropped_count()` / `written_count()` 此前**零非定义读者** ——
    // 通道满时热路径静默丢记录且计数无出口 ⇒ 面板成功率/RPM 系统性偏乐观，而限流调参
    // 正是以面板数为依据。下面两条钉死出口存在且下发的是**真数**。

    /// ⭐ 核心回归：真制造丢弃 → `GET /usage/overview` 的 `pipeline.dropped_records`
    /// 必须反映出来。
    ///
    /// 用 `pipeline::with_drop_burst` 走**生产的** `Pipeline::submit`（容量 1 的真通道），
    /// 计数落在生产同一个全局 `DROPPED` 上 —— 所以这里断言的是"出口读到了真数"，
    /// 不是断言一个测试专用的影子计数器。
    ///
    /// 回退验证：把 `usage_overview` 里的 `pipeline: PipelineHealth::current()` 换回
    /// 原来的 `Json(stats.overview())` → 响应体没有 `pipeline` 键 → 本测试 FAILED。
    #[tokio::test]
    async fn overview_endpoint_exposes_pipeline_drop_counters() {
        // 断言用 `>=`：`DROPPED` 是进程级的，别的测试也在丢；`with_drop_burst` 只保证
        // burst 期间独占。下界成立即证明出口非硬编码 0（真读了计数器）。
        let before =
            crate::usage::pipeline::with_drop_burst(10, |before, dropped| {
                assert_eq!(dropped, 9, "容量 1 投 10 条应丢 9 条");
                before
            });

        let body = body_text(
            usage_overview(State(state_with_retry_records()))
                .await
                .into_response(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();

        let p = &v["pipeline"];
        assert!(
            !p.is_null(),
            "overview 必须带 pipeline 字段，否则丢弃计数在面板上仍然不存在：{body}"
        );
        let dropped = p["dropped_records"].as_u64().unwrap_or_else(|| {
            panic!("pipeline.dropped_records 必须是数字：{body}");
        });
        assert!(
            dropped >= before + 9,
            "出口下发的必须是真计数器（期望 >= {}，实得 {dropped}）：{body}",
            before + 9
        );
        assert!(
            p["written_records"].is_u64(),
            "written 必须与 dropped 配对下发，否则丢弃率算不出来（只有分子没有分母）：{body}"
        );
        assert!(
            p["drop_rate"].is_number(),
            "drop_rate 必须是数字（NaN 会被序列化成 null，前端数值比较会静默当成没问题）：{body}"
        );

        // 前端类型按 snake_case 写死 —— camelCase 出现即等于前端读不到
        assert!(!body.contains("droppedRecords"), "出口不得 camelCase：{body}");
    }

    /// 摊平不得破坏既有契约：加 `pipeline` 的同时 `last_24h` 等必须仍在**顶层**。
    ///
    /// `OverviewResponse` 用 `#[serde(flatten)]` 包 `Overview`。若哪天有人去掉 flatten，
    /// 窗口数据会退到 `windows.last_24h` → 前端图表全空但**没有任何编译错误**。
    #[tokio::test]
    async fn overview_keeps_window_fields_at_top_level() {
        let body = body_text(
            usage_overview(State(state_with_retry_records()))
                .await
                .into_response(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        for k in ["last_24h", "last_7d", "last_30d", "all_time"] {
            assert!(
                !v[k].is_null(),
                "{k} 必须留在顶层（flatten 被去掉会让前端图表全空且不报错）：{body}"
            );
        }
        assert!(
            v["windows"].is_null(),
            "不得出现 windows 包装层（说明 flatten 丢了）：{body}"
        );
    }
}
