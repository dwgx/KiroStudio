//! 用量统计 sink（批次 2.4 + 2.5）
//!
//! 一个 [`UsageSink`] 实现，同时承担两件事：
//! - **JSONL 落盘**：按 UTC 日期分文件（`usage-YYYY-MM-DD.jsonl`），逐条追加写入，
//!   写失败只告警不 panic。冷启动可通过 [`UsageStats::rebuild_from_logs`] 重放恢复。
//! - **内存环形预聚合**：小时/天环形桶（覆盖最近 31 天）+ 按模型/凭据的全量累计 +
//!   per-credential 请求速率环（G-14），供概览页做 O(1) 查询。
//!
//! 环形桶用「绝对编号取模」定位：新记录落桶前若桶的时间标签与当前不符则先清零，
//! 从而以固定内存滚动覆盖旧数据，无需显式过期清理。

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;

use parking_lot::Mutex;
use serde::Serialize;

use super::pipeline::UsageSink;
use super::record::RequestRecord;

/// 小时环形桶数量：24×31，覆盖最近 31 天的逐小时数据
const HOUR_BUCKETS: usize = 24 * 31; // 744
/// 天环形桶数量：覆盖最近 31 天
const DAY_BUCKETS: usize = 31;
/// 速率环形桶数量（每桶 30 秒，20 桶 = 最近 10 分钟）
const RATE_BUCKETS: usize = 20;
/// 速率桶时长（秒）
const RATE_BUCKET_SECS: i64 = 30;
/// 概览默认返回的小时序列点数
const DEFAULT_HOURLY_POINTS: usize = 48;
/// 概览默认返回的天序列点数
const DEFAULT_DAILY_POINTS: usize = 30;

/// 全局实时吞吐环：逐秒桶数量（覆盖最近 60 秒滚动窗口）
const THROUGHPUT_BUCKETS: usize = 60;
/// 全局实时吞吐桶时长（秒）
const THROUGHPUT_BUCKET_SECS: i64 = 1;

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;

/// 聚合指标（小时桶 / 天桶 / 模型 / 凭据 共用的累加字段）
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Aggregate {
    /// 请求总数
    pub requests: u64,
    /// 成功数
    pub success: u64,
    /// 失败数
    pub failure: u64,
    /// 输入 tokens 累计
    pub input_tokens: i64,
    /// 输出 tokens 累计
    pub output_tokens: i64,
    /// credits 累计（仅累加有值的记录）
    pub credits_used: f64,
    /// 延迟累计（毫秒，用于算平均）
    pub latency_sum_ms: u64,
}

impl Aggregate {
    /// 把一条记录累加进本聚合
    fn add(&mut self, r: &RequestRecord) {
        self.requests += 1;
        if r.outcome.is_success() {
            self.success += 1;
        } else {
            self.failure += 1;
        }
        self.input_tokens += r.input_tokens as i64;
        self.output_tokens += r.output_tokens as i64;
        if let Some(c) = r.credits_used {
            self.credits_used += c;
        }
        self.latency_sum_ms += r.latency_ms;
    }

    /// 把另一个聚合并入本聚合（用于跨桶汇总）
    fn merge(&mut self, other: &Aggregate) {
        self.requests += other.requests;
        self.success += other.success;
        self.failure += other.failure;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.credits_used += other.credits_used;
        self.latency_sum_ms += other.latency_sum_ms;
    }

    /// 成功率（0.0~1.0），无请求时为 0
    pub fn success_rate(&self) -> f64 {
        if self.requests == 0 {
            0.0
        } else {
            self.success as f64 / self.requests as f64
        }
    }

    /// 平均延迟（毫秒），无请求时为 0
    pub fn avg_latency_ms(&self) -> f64 {
        if self.requests == 0 {
            0.0
        } else {
            self.latency_sum_ms as f64 / self.requests as f64
        }
    }
}

/// 环形时间桶：一个聚合 + 它当前代表的「绝对编号」时间标签
///
/// `slot` 为该桶归属的绝对小时/天编号（`ts_ms / 桶宽`）。写入前若与传入编号不符，
/// 说明桶被新的时间段复用，先清零再累加，实现环形覆盖。
#[derive(Debug, Clone, Copy, Default)]
struct TimeBucket {
    /// 桶当前代表的绝对编号（-1 表示尚未使用）
    slot: i64,
    agg: Aggregate,
}

impl TimeBucket {
    fn new() -> Self {
        TimeBucket {
            slot: -1,
            agg: Aggregate::default(),
        }
    }
}

/// per-credential 请求速率环形缓冲（G-14）
///
/// 每个凭据维护 [`RATE_BUCKETS`] 个桶，每桶覆盖 [`RATE_BUCKET_SECS`] 秒。
/// 桶按「绝对 30 秒编号取模」定位，写入前对比时间标签，过期则清零，实现 O(1) 滚动。
#[derive(Debug, Clone)]
struct CredRateRing {
    /// 每桶的绝对 30 秒编号（-1 表示未使用）
    slots: [i64; RATE_BUCKETS],
    /// 每桶请求计数
    counts: [u32; RATE_BUCKETS],
}

impl CredRateRing {
    fn new() -> Self {
        CredRateRing {
            slots: [-1; RATE_BUCKETS],
            counts: [0; RATE_BUCKETS],
        }
    }

    /// 在 `slot`（绝对 30 秒编号）对应桶上 +1
    fn bump(&mut self, slot: i64) {
        let idx = slot.rem_euclid(RATE_BUCKETS as i64) as usize;
        if self.slots[idx] != slot {
            self.slots[idx] = slot;
            self.counts[idx] = 0;
        }
        self.counts[idx] += 1;
    }

    /// 以 `now_slot` 为最新桶，返回最近 [`RATE_BUCKETS`] 个桶的计数（从旧到新）。
    /// 已过期（时间标签不在窗口内）的桶返回 0。
    fn recent(&self, now_slot: i64) -> Vec<u32> {
        let mut out = Vec::with_capacity(RATE_BUCKETS);
        // 最旧的桶编号 = now_slot - (RATE_BUCKETS - 1)
        let start = now_slot - (RATE_BUCKETS as i64 - 1);
        for s in start..=now_slot {
            let idx = s.rem_euclid(RATE_BUCKETS as i64) as usize;
            if self.slots[idx] == s {
                out.push(self.counts[idx]);
            } else {
                out.push(0);
            }
        }
        out
    }

    /// 最近 60 秒（当前桶 + 上一桶，每桶 30 秒）的请求数，即 RPM 近似值。
    fn rpm(&self, now_slot: i64) -> u32 {
        let mut sum = 0u32;
        for s in [now_slot, now_slot - 1] {
            let idx = s.rem_euclid(RATE_BUCKETS as i64) as usize;
            if self.slots[idx] == s {
                sum += self.counts[idx];
            }
        }
        sum
    }

    /// 该环内任一桶的最大时间标签（绝对 30 秒编号），用于判断是否仍活跃。
    /// 无任何写入时返回 -1。
    fn max_slot(&self) -> i64 {
        self.slots.iter().copied().max().unwrap_or(-1)
    }
}

/// 速率环集合（per-credential）。credential_id 为 None 的记录不计入速率。
#[derive(Debug, Default)]
struct RateRing {
    rings: HashMap<u64, CredRateRing>,
}

impl RateRing {
    fn bump(&mut self, credential_id: u64, slot: i64) {
        self.rings
            .entry(credential_id)
            .or_insert_with(CredRateRing::new)
            .bump(slot);
    }

    fn recent(&self, credential_id: u64, now_slot: i64) -> Vec<u32> {
        match self.rings.get(&credential_id) {
            Some(r) => r.recent(now_slot),
            None => vec![0; RATE_BUCKETS],
        }
    }
}

/// 全局实时吞吐环形缓冲：**跨全部凭据/客户端**的逐秒滚动窗口。
///
/// 与 [`RateRing`]（per-credential，选号维度）、[`ClientAgg`]（下游发起方维度）正交：
/// 这里只关心「整个网关此刻流动得多快」，供前端把趋势图画成会流动的粒子——
/// 粒子密度 ∝ 每秒请求数，粒子速度 ∝ 每秒 tokens 吞吐。
///
/// [`THROUGHPUT_BUCKETS`] 个桶各覆盖 [`THROUGHPUT_BUCKET_SECS`] 秒（默认 60×1 秒 = 最近 60 秒）。
/// 桶按「绝对秒编号取模」定位，写入前比对时间标签，过期则清零，O(1) 滚动、固定内存。
/// 纯内存累加，零上游调用。
#[derive(Debug)]
struct GlobalThroughputRing {
    /// 每桶的绝对秒编号（-1 表示未使用）
    slots: [i64; THROUGHPUT_BUCKETS],
    /// 每桶请求计数
    requests: [u32; THROUGHPUT_BUCKETS],
    /// 每桶 tokens（input+output）累计
    tokens: [u64; THROUGHPUT_BUCKETS],
}

impl Default for GlobalThroughputRing {
    fn default() -> Self {
        GlobalThroughputRing {
            slots: [-1; THROUGHPUT_BUCKETS],
            requests: [0; THROUGHPUT_BUCKETS],
            tokens: [0; THROUGHPUT_BUCKETS],
        }
    }
}

impl GlobalThroughputRing {
    /// 在 `slot`（绝对秒编号）对应桶累加一条记录的请求数与 tokens。
    fn bump(&mut self, slot: i64, tokens: u64) {
        let idx = slot.rem_euclid(THROUGHPUT_BUCKETS as i64) as usize;
        if self.slots[idx] != slot {
            // 桶被新的一秒复用，先清零再累加（环形覆盖）
            self.slots[idx] = slot;
            self.requests[idx] = 0;
            self.tokens[idx] = 0;
        }
        self.requests[idx] += 1;
        self.tokens[idx] = self.tokens[idx].saturating_add(tokens);
    }

    /// 以 `now_slot` 为最新桶，返回最近 [`THROUGHPUT_BUCKETS`] 个桶（从旧到新）。
    /// 已过期（时间标签不在窗口内）的桶以 0 值补齐，保证前端连续绘图。
    fn recent(&self, now_slot: i64) -> Vec<ThroughputBucket> {
        let mut out = Vec::with_capacity(THROUGHPUT_BUCKETS);
        let start = now_slot - (THROUGHPUT_BUCKETS as i64 - 1);
        for s in start..=now_slot {
            let idx = s.rem_euclid(THROUGHPUT_BUCKETS as i64) as usize;
            let (requests, tokens) = if self.slots[idx] == s {
                (self.requests[idx], self.tokens[idx])
            } else {
                (0, 0)
            };
            out.push(ThroughputBucket {
                // 桶起始时间（Unix 毫秒，对齐到秒）
                ts_ms: s * THROUGHPUT_BUCKET_SECS * 1000,
                requests,
                tokens,
            });
        }
        out
    }
}

/// 单个 session（窗口）的附加元信息，供客户端聚合时归组与展示。
#[derive(Debug, Clone, Default)]
struct SessionMeta {
    /// 所属客户端 key（client_ip 优先，回退 device）
    client_key: String,
    /// 客户端 IP（可能为 None）
    client_ip: Option<String>,
    /// 设备类型
    device: Option<String>,
}

/// 下游客户端 / 窗口维度的滚动速率聚合。
///
/// 与 [`RateRing`]（per-credential，选号维度）正交：这里按**下游发起方**统计。
/// - `by_session`：按 session_id（窗口 UUID）的速率环
/// - `by_client`：按客户端 key（client_ip 优先，回退 device）的速率环
/// - `session_meta` / `client_sessions`：维护 client ⇄ session 的归组关系
///
/// 复用 [`CredRateRing`] 的环形桶（20×30 秒 = 最近 10 分钟），O(1) 滚动。
/// 查询时按时间窗口惰性剔除不再活跃的 session/client，避免长跑内存无界增长。
#[derive(Debug, Default)]
struct ClientAgg {
    by_session: HashMap<String, CredRateRing>,
    by_client: HashMap<String, CredRateRing>,
    session_meta: HashMap<String, SessionMeta>,
    client_sessions: HashMap<String, std::collections::HashSet<String>>,
}

impl ClientAgg {
    /// 从一条记录派生客户端 key：client_ip 优先，回退 device，都无则 "unknown"。
    fn client_key_of(r: &RequestRecord) -> String {
        r.client_ip
            .clone()
            .or_else(|| r.client_device.clone())
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// 累加一条记录到客户端/窗口速率环。
    fn bump(&mut self, r: &RequestRecord, slot: i64) {
        let client_key = Self::client_key_of(r);

        // 客户端维度速率环
        self.by_client
            .entry(client_key.clone())
            .or_insert_with(CredRateRing::new)
            .bump(slot);

        // 窗口维度：仅在有 session_id 时统计（无窗口标识的请求不计入窗口拆分）
        if let Some(sid) = r.session_id.clone() {
            self.by_session
                .entry(sid.clone())
                .or_insert_with(CredRateRing::new)
                .bump(slot);
            self.session_meta.entry(sid.clone()).or_default();
            let meta = self.session_meta.get_mut(&sid).unwrap();
            meta.client_key = client_key.clone();
            meta.client_ip = r.client_ip.clone();
            meta.device = r.client_device.clone();
            self.client_sessions
                .entry(client_key)
                .or_default()
                .insert(sid);
        }
    }

    /// 惰性剔除窗口外（max_slot < now_slot-(RATE_BUCKETS-1)）不再活跃的条目。
    fn prune(&mut self, now_slot: i64) {
        let oldest = now_slot - (RATE_BUCKETS as i64 - 1);
        self.by_session.retain(|_, r| r.max_slot() >= oldest);
        self.by_client.retain(|_, r| r.max_slot() >= oldest);
        // session_meta / client_sessions 与存活的 session/client 对齐
        let live_sessions: std::collections::HashSet<String> =
            self.by_session.keys().cloned().collect();
        let live_clients: std::collections::HashSet<String> =
            self.by_client.keys().cloned().collect();
        self.session_meta.retain(|sid, _| live_sessions.contains(sid));
        for sids in self.client_sessions.values_mut() {
            sids.retain(|sid| live_sessions.contains(sid));
        }
        self.client_sessions
            .retain(|ck, sids| !sids.is_empty() || live_clients.contains(ck));
    }
}

/// 全部内存聚合状态（受一把锁保护）
struct Inner {
    /// 小时环形桶
    hours: Vec<TimeBucket>,
    /// 天环形桶
    days: Vec<TimeBucket>,
    /// 按模型全量累计
    by_model: HashMap<String, Aggregate>,
    /// 按凭据全量累计
    by_credential: HashMap<u64, Aggregate>,
    /// per-credential 速率环
    rate: RateRing,
    /// 下游客户端 / 窗口维度的滚动速率聚合（Task5）
    client_agg: ClientAgg,
    /// 全局实时吞吐环（逐秒滚动 60 秒，供前端画流动粒子）
    throughput: GlobalThroughputRing,
}

impl Inner {
    fn new() -> Self {
        Inner {
            hours: vec![TimeBucket::new(); HOUR_BUCKETS],
            days: vec![TimeBucket::new(); DAY_BUCKETS],
            by_model: HashMap::new(),
            by_credential: HashMap::new(),
            rate: RateRing::default(),
            client_agg: ClientAgg::default(),
            throughput: GlobalThroughputRing::default(),
        }
    }

    /// 把一条记录累加进所有内存聚合（环形桶 + 模型/凭据 + 速率环）
    fn apply(&mut self, r: &RequestRecord) {
        let hour_slot = r.ts_ms.div_euclid(HOUR_MS);
        let day_slot = r.ts_ms.div_euclid(DAY_MS);

        // 小时环形桶
        let hidx = hour_slot.rem_euclid(HOUR_BUCKETS as i64) as usize;
        let hb = &mut self.hours[hidx];
        if hb.slot != hour_slot {
            hb.slot = hour_slot;
            hb.agg = Aggregate::default();
        }
        hb.agg.add(r);

        // 天环形桶
        let didx = day_slot.rem_euclid(DAY_BUCKETS as i64) as usize;
        let db = &mut self.days[didx];
        if db.slot != day_slot {
            db.slot = day_slot;
            db.agg = Aggregate::default();
        }
        db.agg.add(r);

        // 按模型累计
        self.by_model.entry(r.model.clone()).or_default().add(r);

        // 按凭据累计 + 速率环
        let rate_slot = r.ts_ms.div_euclid(RATE_BUCKET_SECS * 1000);
        if let Some(cid) = r.credential_id {
            self.by_credential.entry(cid).or_default().add(r);
            self.rate.bump(cid, rate_slot);
        }

        // 下游客户端 / 窗口维度速率（与 credential 速率共用同一 30 秒桶编号）
        self.client_agg.bump(r, rate_slot);

        // 全局实时吞吐（逐秒桶）：请求数 + tokens(input+output)。
        // token 计数取非负，避免异常负值污染吞吐。
        let sec_slot = r.ts_ms.div_euclid(THROUGHPUT_BUCKET_SECS * 1000);
        let tokens = (r.input_tokens.max(0) as u64) + (r.output_tokens.max(0) as u64);
        self.throughput.bump(sec_slot, tokens);
    }
}

/// 概览页某个时间窗口的汇总（供 admin JSON 输出）
#[derive(Debug, Clone, Serialize)]
pub struct WindowSummary {
    /// 请求总数
    pub requests: u64,
    /// 成功数
    pub success: u64,
    /// 失败数
    pub failure: u64,
    /// 成功率（0.0~1.0）
    pub success_rate: f64,
    /// 输入 tokens 累计
    pub input_tokens: i64,
    /// 输出 tokens 累计
    pub output_tokens: i64,
    /// tokens 总计（输入+输出）
    pub total_tokens: i64,
    /// credits 累计
    pub credits_used: f64,
    /// 平均延迟（毫秒）
    pub avg_latency_ms: f64,
}

impl From<Aggregate> for WindowSummary {
    fn from(a: Aggregate) -> Self {
        WindowSummary {
            requests: a.requests,
            success: a.success,
            failure: a.failure,
            success_rate: a.success_rate(),
            input_tokens: a.input_tokens,
            output_tokens: a.output_tokens,
            total_tokens: a.input_tokens + a.output_tokens,
            credits_used: a.credits_used,
            avg_latency_ms: a.avg_latency_ms(),
        }
    }
}

/// 概览：最近 24 小时 / 7 天 / 30 天 三个窗口
#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    /// 最近 24 小时
    pub last_24h: WindowSummary,
    /// 最近 7 天
    pub last_7d: WindowSummary,
    /// 最近 30 天
    pub last_30d: WindowSummary,
}

/// 时间序列中的一个点
#[derive(Debug, Clone, Serialize)]
pub struct SeriesPoint {
    /// 桶起始时间（Unix 毫秒，UTC 对齐到小时/天）
    pub ts_ms: i64,
    /// 请求数
    pub requests: u64,
    /// 成功数
    pub success: u64,
    /// 失败数
    pub failure: u64,
    /// 输入 tokens
    pub input_tokens: i64,
    /// 输出 tokens
    pub output_tokens: i64,
    /// credits 累计
    pub credits_used: f64,
    /// 平均延迟（毫秒）
    pub avg_latency_ms: f64,
}

/// 按 key（模型名 / 凭据 ID 字符串）聚合的一行
#[derive(Debug, Clone, Serialize)]
pub struct GroupStat {
    /// 分组键
    pub key: String,
    /// 请求数
    pub requests: u64,
    /// 成功率
    pub success_rate: f64,
    /// 输入 tokens
    pub input_tokens: i64,
    /// 输出 tokens
    pub output_tokens: i64,
    /// credits 累计
    pub credits_used: f64,
    /// 平均延迟（毫秒）
    pub avg_latency_ms: f64,
}

impl GroupStat {
    fn from(key: String, a: &Aggregate) -> Self {
        GroupStat {
            key,
            requests: a.requests,
            success_rate: a.success_rate(),
            input_tokens: a.input_tokens,
            output_tokens: a.output_tokens,
            credits_used: a.credits_used,
            avg_latency_ms: a.avg_latency_ms(),
        }
    }
}

/// 单个活跃窗口（session）的 RPM 视图
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRpm {
    /// 窗口标识（session_id / conversationId）
    pub session_id: String,
    /// 该窗口最近 60 秒请求数（RPM）
    pub rpm: u32,
}

/// 单个下游客户端的 RPM 视图（按 client_ip 优先，回退 device 分组）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientRpm {
    /// 客户端分组键（client_ip 优先，回退 device）
    pub client_key: String,
    /// 客户端 IP（可能为 None）
    pub client_ip: Option<String>,
    /// 设备类型（如 claude-code）
    pub device: Option<String>,
    /// 该客户端最近 60 秒请求数（RPM，聚合其所有窗口）
    pub rpm: u32,
    /// 活跃窗口数（distinct session_id，近 10 分钟内有请求）
    pub active_sessions: usize,
    /// 各活跃窗口的 RPM（按 RPM 降序）
    pub sessions: Vec<SessionRpm>,
}

/// 全局实时吞吐的单个逐秒桶
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThroughputBucket {
    /// 桶起始时间（Unix 毫秒，对齐到秒）
    pub ts_ms: i64,
    /// 该秒的请求数
    pub requests: u32,
    /// 该秒的 tokens（input+output）吞吐
    pub tokens: u64,
}

/// 全局实时吞吐快照：当前速率 + 最近 60 秒逐秒桶。
///
/// 供前端把趋势图渲染成会流动的粒子：
/// - `current_rpm`：最近 60 秒总请求数（每分钟请求数近似）
/// - `current_rps`：最近 60 秒请求数 / 60，用作粒子**密度**
/// - `current_tokens_per_sec`：最近 60 秒 tokens 总量 / 60，用作粒子**速度**
/// - `recent_buckets`：最近 60 秒逐秒明细（从旧到新，空秒补 0），供细粒度动画
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThroughputSnapshot {
    /// 最近 60 秒总请求数（RPM 近似）
    pub current_rpm: u32,
    /// 最近 60 秒平均每秒请求数（粒子密度）
    pub current_rps: f64,
    /// 最近 60 秒平均每秒 tokens 吞吐（粒子速度）
    pub current_tokens_per_sec: f64,
    /// 窗口时长（秒），前端据此换算速率
    pub window_secs: u32,
    /// 最近 60 秒逐秒桶（从旧到新，空秒补 0）
    pub recent_buckets: Vec<ThroughputBucket>,
}

/// 用量统计 sink：JSONL 落盘 + 内存环形预聚合
pub struct UsageStats {
    /// JSONL 数据目录
    dir: PathBuf,
    /// 当前打开的日文件（日期字符串 + 句柄），跨天时轮换
    file: Mutex<Option<(String, File)>>,
    /// 内存聚合状态
    inner: Mutex<Inner>,
    /// rebuild 时解析失败的行数（累计）
    parse_errors: Mutex<u64>,
}

impl UsageStats {
    /// 构造。`dir` 为 JSONL 数据目录（会在首次写入时按需创建）。
    pub fn new(dir: PathBuf) -> UsageStats {
        UsageStats {
            dir,
            file: Mutex::new(None),
            inner: Mutex::new(Inner::new()),
            parse_errors: Mutex::new(0),
        }
    }

    /// 根据 Unix 毫秒时间戳换算 UTC 日期字符串（`YYYY-MM-DD`）
    fn date_str(ts_ms: i64) -> String {
        chrono::DateTime::from_timestamp_millis(ts_ms)
            .unwrap_or_else(|| chrono::DateTime::from_timestamp_millis(0).unwrap())
            .format("%Y-%m-%d")
            .to_string()
    }

    /// 当天文件的完整路径
    fn file_path(&self, date: &str) -> PathBuf {
        self.dir.join(format!("usage-{date}.jsonl"))
    }

    /// 把一行 JSON 追加写入当天文件。失败只 warn 不 panic。
    fn append_line(&self, ts_ms: i64, line: &str) {
        let date = Self::date_str(ts_ms);
        let mut guard = self.file.lock();

        // 跨天或首次：轮换文件句柄
        let need_open = match guard.as_ref() {
            Some((cur_date, _)) => cur_date != &date,
            None => true,
        };
        if need_open {
            if let Err(e) = fs::create_dir_all(&self.dir) {
                tracing::warn!("用量统计：创建目录 {:?} 失败：{e}", self.dir);
                return;
            }
            let path = self.file_path(&date);
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(f) => *guard = Some((date.clone(), f)),
                Err(e) => {
                    tracing::warn!("用量统计：打开日文件 {:?} 失败：{e}", path);
                    return;
                }
            }
        }

        if let Some((_, f)) = guard.as_mut() {
            if let Err(e) = writeln!(f, "{line}") {
                tracing::warn!("用量统计：写入 JSONL 失败：{e}");
            }
        }
    }

    /// 冷启动重放：读取目录下所有 `usage-*.jsonl`，逐行反序列化累加进内存聚合。
    /// 解析失败的行跳过并计数（累计到 [`parse_error_count`]）。
    pub fn rebuild_from_logs(&self) {
        let entries = match fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(_) => {
                // 目录不存在视为无历史，正常冷启动
                return;
            }
        };

        // 收集并排序文件名，保证按日期顺序重放（对聚合结果无影响，仅利于可读性）
        let mut files: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let is_usage_jsonl = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("usage-") && n.ends_with(".jsonl"))
                .unwrap_or(false);
            if is_usage_jsonl {
                files.push(path);
            }
        }
        files.sort();

        let mut errors = 0u64;
        let mut inner = self.inner.lock();
        for path in files {
            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("用量统计：读取 {:?} 失败：{e}", path);
                    continue;
                }
            };
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<RequestRecord>(line) {
                    Ok(rec) => inner.apply(&rec),
                    Err(_) => errors += 1,
                }
            }
        }
        drop(inner);
        *self.parse_errors.lock() += errors;
        if errors > 0 {
            tracing::warn!("用量统计：重放跳过 {errors} 条无法解析的记录");
        }
    }

    /// 重放累计的解析失败行数
    pub fn parse_error_count(&self) -> u64 {
        *self.parse_errors.lock()
    }

    /// 概览：以「当前时刻」为基准汇总最近 24 小时 / 7 天 / 30 天。
    ///
    /// - 24 小时窗口用小时桶累加（更细粒度）
    /// - 7 天 / 30 天窗口用天桶累加
    pub fn overview(&self) -> Overview {
        let now = chrono::Utc::now().timestamp_millis();
        self.overview_at(now)
    }

    /// 概览（可注入基准时间，便于测试）
    pub fn overview_at(&self, now_ms: i64) -> Overview {
        let inner = self.inner.lock();
        let now_hour = now_ms.div_euclid(HOUR_MS);
        let now_day = now_ms.div_euclid(DAY_MS);

        // 最近 24 小时：小时 slot ∈ [now_hour-23, now_hour]
        let mut agg24 = Aggregate::default();
        for b in &inner.hours {
            if b.slot >= 0 && b.slot >= now_hour - 23 && b.slot <= now_hour {
                agg24.merge(&b.agg);
            }
        }

        // 最近 7 天 / 30 天：天 slot 区间
        let mut agg7 = Aggregate::default();
        let mut agg30 = Aggregate::default();
        for b in &inner.days {
            if b.slot < 0 || b.slot > now_day {
                continue;
            }
            if b.slot >= now_day - 6 {
                agg7.merge(&b.agg);
            }
            if b.slot >= now_day - 29 {
                agg30.merge(&b.agg);
            }
        }

        Overview {
            last_24h: agg24.into(),
            last_7d: agg7.into(),
            last_30d: agg30.into(),
        }
    }

    /// 最近 `points` 个小时桶的时间序列（从旧到新），默认 [`DEFAULT_HOURLY_POINTS`]。
    /// 空桶（无数据）也会以 0 值补齐，保证前端连续绘图。
    pub fn timeseries_hourly(&self) -> Vec<SeriesPoint> {
        self.timeseries_hourly_at(chrono::Utc::now().timestamp_millis(), DEFAULT_HOURLY_POINTS)
    }

    /// 小时序列（可注入基准时间与点数，便于测试）
    pub fn timeseries_hourly_at(&self, now_ms: i64, points: usize) -> Vec<SeriesPoint> {
        let points = points.min(HOUR_BUCKETS);
        let inner = self.inner.lock();
        let now_hour = now_ms.div_euclid(HOUR_MS);
        let start = now_hour - (points as i64 - 1);
        let mut out = Vec::with_capacity(points);
        for slot in start..=now_hour {
            let idx = slot.rem_euclid(HOUR_BUCKETS as i64) as usize;
            let b = &inner.hours[idx];
            let agg = if b.slot == slot {
                b.agg
            } else {
                Aggregate::default()
            };
            out.push(SeriesPoint {
                ts_ms: slot * HOUR_MS,
                requests: agg.requests,
                success: agg.success,
                failure: agg.failure,
                input_tokens: agg.input_tokens,
                output_tokens: agg.output_tokens,
                credits_used: agg.credits_used,
                avg_latency_ms: agg.avg_latency_ms(),
            });
        }
        out
    }

    /// 最近 `points` 个天桶的时间序列（从旧到新），默认 [`DEFAULT_DAILY_POINTS`]。
    pub fn timeseries_daily(&self) -> Vec<SeriesPoint> {
        self.timeseries_daily_at(chrono::Utc::now().timestamp_millis(), DEFAULT_DAILY_POINTS)
    }

    /// 天序列（可注入基准时间与点数，便于测试）
    pub fn timeseries_daily_at(&self, now_ms: i64, points: usize) -> Vec<SeriesPoint> {
        let points = points.min(DAY_BUCKETS);
        let inner = self.inner.lock();
        let now_day = now_ms.div_euclid(DAY_MS);
        let start = now_day - (points as i64 - 1);
        let mut out = Vec::with_capacity(points);
        for slot in start..=now_day {
            let idx = slot.rem_euclid(DAY_BUCKETS as i64) as usize;
            let b = &inner.days[idx];
            let agg = if b.slot == slot {
                b.agg
            } else {
                Aggregate::default()
            };
            out.push(SeriesPoint {
                ts_ms: slot * DAY_MS,
                requests: agg.requests,
                success: agg.success,
                failure: agg.failure,
                input_tokens: agg.input_tokens,
                output_tokens: agg.output_tokens,
                credits_used: agg.credits_used,
                avg_latency_ms: agg.avg_latency_ms(),
            });
        }
        out
    }

    /// 按模型全量聚合（按请求数降序）
    pub fn by_model(&self) -> Vec<GroupStat> {
        let inner = self.inner.lock();
        let mut out: Vec<GroupStat> = inner
            .by_model
            .iter()
            .map(|(k, a)| GroupStat::from(k.clone(), a))
            .collect();
        out.sort_by(|a, b| b.requests.cmp(&a.requests).then(a.key.cmp(&b.key)));
        out
    }

    /// 按凭据全量聚合（按请求数降序，key 为凭据 ID 字符串）
    pub fn by_credential(&self) -> Vec<GroupStat> {
        let inner = self.inner.lock();
        let mut out: Vec<GroupStat> = inner
            .by_credential
            .iter()
            .map(|(k, a)| GroupStat::from(k.to_string(), a))
            .collect();
        out.sort_by(|a, b| b.requests.cmp(&a.requests).then(a.key.cmp(&b.key)));
        out
    }

    /// 某凭据最近 10 分钟每 30 秒的请求数（20 个点，从旧到新），供前端画 sparkline。
    pub fn recent_rate(&self, credential_id: u64) -> Vec<u32> {
        self.recent_rate_at(credential_id, chrono::Utc::now().timestamp_millis())
    }

    /// 速率查询（可注入基准时间，便于测试）
    pub fn recent_rate_at(&self, credential_id: u64, now_ms: i64) -> Vec<u32> {
        let now_slot = now_ms.div_euclid(RATE_BUCKET_SECS * 1000);
        let inner = self.inner.lock();
        inner.rate.recent(credential_id, now_slot)
    }

    /// 下游客户端 RPM 视图：每个客户端当前 RPM + 活跃窗口数 + 各窗口 RPM。
    ///
    /// 按 client_ip（优先）或 device 分组；窗口按 session_id 拆分。仅返回近 10 分钟内
    /// 有活动的客户端/窗口（查询时惰性 prune 掉过期条目）。按客户端 RPM 降序。
    pub fn clients(&self) -> Vec<ClientRpm> {
        self.clients_at(chrono::Utc::now().timestamp_millis())
    }

    /// 客户端 RPM 视图（可注入基准时间，便于测试）
    pub fn clients_at(&self, now_ms: i64) -> Vec<ClientRpm> {
        let now_slot = now_ms.div_euclid(RATE_BUCKET_SECS * 1000);
        let mut inner = self.inner.lock();
        // 查询时机做惰性回收，避免不再活跃的窗口/客户端长期滞留
        inner.client_agg.prune(now_slot);

        let mut out: Vec<ClientRpm> = Vec::with_capacity(inner.client_agg.by_client.len());
        for (client_key, ring) in &inner.client_agg.by_client {
            let rpm = ring.rpm(now_slot);
            // 该客户端名下的活跃窗口
            let mut sessions: Vec<SessionRpm> = Vec::new();
            if let Some(sids) = inner.client_agg.client_sessions.get(client_key) {
                for sid in sids {
                    if let Some(sring) = inner.client_agg.by_session.get(sid) {
                        let s_rpm = sring.rpm(now_slot);
                        // 近 10 分钟内该窗口任一桶存活即视为活跃
                        if sring.max_slot() >= now_slot - (RATE_BUCKETS as i64 - 1) {
                            sessions.push(SessionRpm {
                                session_id: sid.clone(),
                                rpm: s_rpm,
                            });
                        }
                    }
                }
            }
            sessions.sort_by(|a, b| b.rpm.cmp(&a.rpm).then(a.session_id.cmp(&b.session_id)));

            // 取该 client 任一窗口的 meta 补充 ip/device（无窗口时为 None）
            let (client_ip, device) = inner
                .client_agg
                .client_sessions
                .get(client_key)
                .and_then(|sids| sids.iter().next())
                .and_then(|sid| inner.client_agg.session_meta.get(sid))
                .map(|m| (m.client_ip.clone(), m.device.clone()))
                .unwrap_or((None, None));

            out.push(ClientRpm {
                client_key: client_key.clone(),
                client_ip,
                device,
                rpm,
                active_sessions: sessions.len(),
                sessions,
            });
        }
        out.sort_by(|a, b| b.rpm.cmp(&a.rpm).then(a.client_key.cmp(&b.client_key)));
        out
    }

    /// 主动回收客户端/窗口维度聚合里不再活跃的条目（后台定时调用）。
    ///
    /// `by_session` / `by_client` / `session_meta` / `client_sessions` 四张 map 的 key
    /// 是**客户端可控**的 session_id（UUID）与 client_ip。它们原本只在查询端点
    /// [`clients_at`] 里惰性 `prune`；若长时间无人打开概览页，这些 map 会随不断变化的
    /// session_id 无界增长（中高危内存泄漏）。
    ///
    /// 本方法把同一套窗口剔除逻辑（[`ClientAgg::prune`]）搬到后台定时任务里主动执行，
    /// 与 [`clients_at`] 完全一致：剔除 max_slot 落在 `[now_slot-(RATE_BUCKETS-1), now_slot]`
    /// 窗口之外的 session/client，并同步清理 meta / 归组关系。
    ///
    /// 线程安全：与所有查询/写入路径共用同一把 `inner` 锁，短临界区内完成回收。
    /// 返回回收后仍存活的 (session 数, client 数)，便于调用方按需记日志。
    pub fn cleanup_client_stats(&self) -> (usize, usize) {
        self.cleanup_client_stats_at(chrono::Utc::now().timestamp_millis())
    }

    /// 客户端聚合回收（可注入基准时间，便于测试）
    pub fn cleanup_client_stats_at(&self, now_ms: i64) -> (usize, usize) {
        let now_slot = now_ms.div_euclid(RATE_BUCKET_SECS * 1000);
        let mut inner = self.inner.lock();
        inner.client_agg.prune(now_slot);
        (
            inner.client_agg.by_session.len(),
            inner.client_agg.by_client.len(),
        )
    }

    /// 全局实时吞吐快照：最近 60 秒逐秒桶 + 当前 RPM / RPS / tokens 每秒。
    ///
    /// 只读内存聚合，零上游调用（封号红线）。供前端画数据流动粒子。
    pub fn throughput(&self) -> ThroughputSnapshot {
        self.throughput_at(chrono::Utc::now().timestamp_millis())
    }

    /// 吞吐快照（可注入基准时间，便于测试）
    pub fn throughput_at(&self, now_ms: i64) -> ThroughputSnapshot {
        let now_slot = now_ms.div_euclid(THROUGHPUT_BUCKET_SECS * 1000);
        let inner = self.inner.lock();
        let buckets = inner.throughput.recent(now_slot);
        drop(inner);

        let total_requests: u64 = buckets.iter().map(|b| b.requests as u64).sum();
        let total_tokens: u64 = buckets.iter().map(|b| b.tokens).sum();
        let window_secs = THROUGHPUT_BUCKETS as u32; // 桶数 × 1 秒
        let w = window_secs as f64;

        ThroughputSnapshot {
            current_rpm: total_requests.min(u32::MAX as u64) as u32,
            current_rps: total_requests as f64 / w,
            current_tokens_per_sec: total_tokens as f64 / w,
            window_secs,
            recent_buckets: buckets,
        }
    }
}

impl UsageSink for UsageStats {
    fn on_record(&self, record: &RequestRecord) {
        // 先更新内存聚合（不会失败），再落盘（失败仅告警）
        {
            let mut inner = self.inner.lock();
            inner.apply(record);
        }
        match serde_json::to_string(record) {
            Ok(line) => self.append_line(record.ts_ms, &line),
            Err(e) => tracing::warn!("用量统计：序列化记录失败：{e}"),
        }
    }

    fn name(&self) -> &'static str {
        "usage_stats"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::record::RequestOutcome;

    /// UTC 基准时间：2026-07-03T00:00:00Z 的 Unix 毫秒
    const BASE_MS: i64 = 1_783_036_800_000;

    /// 构造一条记录：指定时间偏移、凭据、模型、结果与 tokens
    fn rec(
        offset_ms: i64,
        cid: Option<u64>,
        model: &str,
        outcome: RequestOutcome,
        input: i32,
        output: i32,
    ) -> RequestRecord {
        let mut r = RequestRecord::new("req", model);
        r.ts_ms = BASE_MS + offset_ms;
        r.credential_id = cid;
        r.outcome = outcome;
        r.input_tokens = input;
        r.output_tokens = output;
        r.latency_ms = 100;
        r
    }

    /// 校验 BASE_MS 确实对齐到 2026-07-03 UTC 零点
    #[test]
    fn test_base_ms_is_utc_midnight() {
        assert_eq!(UsageStats::date_str(BASE_MS), "2026-07-03");
        assert_eq!(BASE_MS % DAY_MS, 0, "BASE_MS 应对齐到 UTC 零点");
    }

    #[test]
    fn test_apply_hourly_and_daily_buckets() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        // 同一小时内 3 条
        s.on_record(&rec(0, Some(1), "m1", RequestOutcome::Success, 10, 5));
        s.on_record(&rec(60_000, Some(1), "m1", RequestOutcome::Success, 20, 10));
        s.on_record(&rec(120_000, Some(1), "m1", RequestOutcome::RateLimited, 0, 0));

        let ov = s.overview_at(BASE_MS + 120_000);
        assert_eq!(ov.last_24h.requests, 3);
        assert_eq!(ov.last_24h.success, 2);
        assert_eq!(ov.last_24h.failure, 1);
        assert_eq!(ov.last_24h.input_tokens, 30);
        assert_eq!(ov.last_24h.output_tokens, 15);
        assert!((ov.last_24h.success_rate - 2.0 / 3.0).abs() < 1e-9);
        // 天窗口应包含同样 3 条
        assert_eq!(ov.last_7d.requests, 3);
        assert_eq!(ov.last_30d.requests, 3);
    }

    #[test]
    fn test_cross_hour_and_cross_day() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        // 三个不同小时各 1 条（同一天）
        s.on_record(&rec(0, Some(1), "m", RequestOutcome::Success, 1, 1));
        s.on_record(&rec(HOUR_MS, Some(1), "m", RequestOutcome::Success, 1, 1));
        s.on_record(&rec(2 * HOUR_MS, Some(1), "m", RequestOutcome::Success, 1, 1));

        let series = s.timeseries_hourly_at(BASE_MS + 2 * HOUR_MS, 3);
        assert_eq!(series.len(), 3);
        assert_eq!(series[0].requests, 1);
        assert_eq!(series[1].requests, 1);
        assert_eq!(series[2].requests, 1);
        // 时间戳应对齐到小时
        assert_eq!(series[0].ts_ms, BASE_MS);
        assert_eq!(series[2].ts_ms, BASE_MS + 2 * HOUR_MS);

        // 跨天：再加一条隔天记录
        s.on_record(&rec(DAY_MS, Some(1), "m", RequestOutcome::Success, 1, 1));
        let daily = s.timeseries_daily_at(BASE_MS + DAY_MS, 2);
        assert_eq!(daily.len(), 2);
        assert_eq!(daily[0].requests, 3, "第一天 3 条");
        assert_eq!(daily[1].requests, 1, "第二天 1 条");
    }

    #[test]
    fn test_ring_overwrite_old_data() {
        // 同一环形桶被相隔正好 HOUR_BUCKETS 小时的两条记录复用，旧数据应被覆盖
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        s.on_record(&rec(0, Some(1), "m", RequestOutcome::Success, 100, 100));
        // 相隔 744 小时 = 恰好一整圈，落入同一桶但 slot 不同 → 清零覆盖
        let ring_span = HOUR_BUCKETS as i64 * HOUR_MS;
        s.on_record(&rec(ring_span, Some(1), "m", RequestOutcome::Success, 7, 7));

        // 查询最新那一小时，应只看到新记录（7,7），旧记录已被环形覆盖
        let series = s.timeseries_hourly_at(BASE_MS + ring_span, 1);
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].requests, 1);
        assert_eq!(series[0].input_tokens, 7);
    }

    #[test]
    fn test_by_model_and_by_credential() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        s.on_record(&rec(0, Some(1), "sonnet", RequestOutcome::Success, 10, 5));
        s.on_record(&rec(1000, Some(1), "sonnet", RequestOutcome::Success, 10, 5));
        s.on_record(&rec(2000, Some(2), "opus", RequestOutcome::ServerError, 3, 0));

        let models = s.by_model();
        // sonnet 请求最多，排第一
        assert_eq!(models[0].key, "sonnet");
        assert_eq!(models[0].requests, 2);
        let opus = models.iter().find(|m| m.key == "opus").unwrap();
        assert_eq!(opus.requests, 1);
        assert!((opus.success_rate - 0.0).abs() < 1e-9);

        let creds = s.by_credential();
        let c1 = creds.iter().find(|c| c.key == "1").unwrap();
        assert_eq!(c1.requests, 2);
        assert_eq!(c1.input_tokens, 20);
        let c2 = creds.iter().find(|c| c.key == "2").unwrap();
        assert_eq!(c2.requests, 1);
    }

    #[test]
    fn test_rate_ring() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        // 同一 30 秒桶内 2 条
        s.on_record(&rec(0, Some(7), "m", RequestOutcome::Success, 1, 1));
        s.on_record(&rec(10_000, Some(7), "m", RequestOutcome::Success, 1, 1));
        // 下一个 30 秒桶 1 条
        s.on_record(&rec(35_000, Some(7), "m", RequestOutcome::Success, 1, 1));

        // 以第二个桶为 now，返回 20 个点，最新两点为 [2, 1]
        let rate = s.recent_rate_at(7, BASE_MS + 35_000);
        assert_eq!(rate.len(), RATE_BUCKETS);
        assert_eq!(rate[RATE_BUCKETS - 1], 1, "最新桶 1 条");
        assert_eq!(rate[RATE_BUCKETS - 2], 2, "上一桶 2 条");
        // 其余为 0
        assert_eq!(rate[0], 0);

        // 未知凭据返回全 0
        let empty = s.recent_rate_at(999, BASE_MS + 35_000);
        assert_eq!(empty, vec![0u32; RATE_BUCKETS]);

        // 时间前进到窗口之外，旧数据不再出现
        let later = s.recent_rate_at(7, BASE_MS + 60 * 60 * 1000);
        assert_eq!(later, vec![0u32; RATE_BUCKETS]);
    }

    /// 构造一条带客户端画像的记录（含 session_id / client_ip / device）
    fn rec_client(
        offset_ms: i64,
        session: Option<&str>,
        ip: Option<&str>,
        device: Option<&str>,
    ) -> RequestRecord {
        let mut r = rec(offset_ms, Some(1), "m", RequestOutcome::Success, 1, 1);
        r.session_id = session.map(|s| s.to_string());
        r.client_ip = ip.map(|s| s.to_string());
        r.client_device = device.map(|s| s.to_string());
        r
    }

    #[test]
    fn test_clients_rpm_by_ip_and_sessions() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        // 客户端 A(1.1.1.1) 开两个窗口：w1 打 2 条，w2 打 1 条（均在近 60 秒内）
        s.on_record(&rec_client(0, Some("w1"), Some("1.1.1.1"), Some("claude-code")));
        s.on_record(&rec_client(1_000, Some("w1"), Some("1.1.1.1"), Some("claude-code")));
        s.on_record(&rec_client(2_000, Some("w2"), Some("1.1.1.1"), Some("claude-code")));
        // 客户端 B(2.2.2.2) 一个窗口 1 条
        s.on_record(&rec_client(0, Some("w3"), Some("2.2.2.2"), Some("claude-code")));

        // now 落在同一 30 秒桶，60 秒 RPM 覆盖以上全部
        let clients = s.clients_at(BASE_MS + 2_000);
        assert_eq!(clients.len(), 2, "应聚合出两个客户端");

        // A 排第一（RPM=3），两个活跃窗口
        let a = &clients[0];
        assert_eq!(a.client_key, "1.1.1.1");
        assert_eq!(a.client_ip.as_deref(), Some("1.1.1.1"));
        assert_eq!(a.rpm, 3);
        assert_eq!(a.active_sessions, 2);
        // 窗口按 RPM 降序：w1(2) 在前
        assert_eq!(a.sessions[0].session_id, "w1");
        assert_eq!(a.sessions[0].rpm, 2);
        assert_eq!(a.sessions[1].rpm, 1);

        let b = &clients[1];
        assert_eq!(b.client_key, "2.2.2.2");
        assert_eq!(b.rpm, 1);
        assert_eq!(b.active_sessions, 1);
    }

    #[test]
    fn test_cleanup_client_stats_reclaims_stale_entries() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        // 两个客户端各开一个窗口
        s.on_record(&rec_client(0, Some("w1"), Some("1.1.1.1"), Some("claude-code")));
        s.on_record(&rec_client(0, Some("w2"), Some("2.2.2.2"), Some("claude-code")));

        // 窗口内回收：条目仍活跃，四张 map 都应保留
        let (sessions, clients) = s.cleanup_client_stats_at(BASE_MS);
        assert_eq!(sessions, 2, "窗口内 session 不应被回收");
        assert_eq!(clients, 2, "窗口内 client 不应被回收");

        // 10 分钟后回收：全部过期，四张 map 应清空（这是无查询时也能回收的关键）
        let (sessions, clients) = s.cleanup_client_stats_at(BASE_MS + 11 * 60 * 1000);
        assert_eq!(sessions, 0, "过期 session 应被后台回收");
        assert_eq!(clients, 0, "过期 client 应被后台回收");
    }

    #[test]
    fn test_clients_prune_stale_window() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        s.on_record(&rec_client(0, Some("old"), Some("9.9.9.9"), Some("claude-code")));
        // 10 分钟后查询：旧窗口/客户端应被 prune 掉
        let later = s.clients_at(BASE_MS + 11 * 60 * 1000);
        assert!(later.is_empty(), "过期窗口应被回收，结果为空");
    }

    #[test]
    fn test_clients_ip_fallback_to_device() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        // 无 IP，回退用 device 作为分组键
        s.on_record(&rec_client(0, Some("w1"), None, Some("claude-code")));
        let clients = s.clients_at(BASE_MS);
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].client_key, "claude-code");
        assert_eq!(clients[0].client_ip, None);
        assert_eq!(clients[0].device.as_deref(), Some("claude-code"));
    }

    #[test]
    fn test_jsonl_write_and_rebuild() {
        // 用唯一临时目录，落盘后新建实例重放，聚合应一致
        let dir = std::env::temp_dir().join(format!(
            "kiro_us_rebuild_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);

        {
            let s = UsageStats::new(dir.clone());
            s.on_record(&rec(0, Some(1), "m1", RequestOutcome::Success, 10, 5));
            s.on_record(&rec(1000, Some(2), "m2", RequestOutcome::RateLimited, 0, 0));
            // 跨天，验证按天分文件
            s.on_record(&rec(DAY_MS, Some(1), "m1", RequestOutcome::Success, 7, 3));
        }

        // 应生成两个日文件
        let f1 = dir.join("usage-2026-07-03.jsonl");
        let f2 = dir.join("usage-2026-07-04.jsonl");
        assert!(f1.exists(), "第一天文件应存在");
        assert!(f2.exists(), "第二天文件应存在");

        // 新实例重放
        let s2 = UsageStats::new(dir.clone());
        s2.rebuild_from_logs();
        let ov = s2.overview_at(BASE_MS + DAY_MS);
        assert_eq!(ov.last_7d.requests, 3, "重放后应恢复全部 3 条");
        assert_eq!(ov.last_7d.success, 2);
        let models = s2.by_model();
        let m1 = models.iter().find(|m| m.key == "m1").unwrap();
        assert_eq!(m1.requests, 2);
        assert_eq!(m1.input_tokens, 17);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_rebuild_skips_bad_lines() {
        let dir = std::env::temp_dir().join(format!(
            "kiro_us_bad_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // 一行合法 + 一行垃圾 + 一行空行
        let good = serde_json::to_string(&rec(0, Some(1), "m", RequestOutcome::Success, 1, 1)).unwrap();
        let path = dir.join("usage-2026-07-03.jsonl");
        fs::write(&path, format!("{good}\nNOT JSON\n\n")).unwrap();

        let s = UsageStats::new(dir.clone());
        s.rebuild_from_logs();
        assert_eq!(s.parse_error_count(), 1, "应跳过 1 条无法解析的行");
        let ov = s.overview_at(BASE_MS);
        assert_eq!(ov.last_24h.requests, 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_rebuild_missing_dir_is_noop() {
        let dir = std::env::temp_dir().join(format!("kiro_us_absent_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let s = UsageStats::new(dir);
        // 目录不存在不应 panic
        s.rebuild_from_logs();
        assert_eq!(s.overview_at(BASE_MS).last_24h.requests, 0);
    }

    #[test]
    fn test_query_structs_serialize() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        s.on_record(&rec(0, Some(1), "m", RequestOutcome::Success, 1, 1));
        // 确认查询结果可被 serde_json 序列化（供 admin JSON 输出）
        assert!(serde_json::to_string(&s.overview_at(BASE_MS)).is_ok());
        assert!(serde_json::to_string(&s.timeseries_hourly_at(BASE_MS, 5)).is_ok());
        assert!(serde_json::to_string(&s.timeseries_daily_at(BASE_MS, 5)).is_ok());
        assert!(serde_json::to_string(&s.by_model()).is_ok());
        assert!(serde_json::to_string(&s.by_credential()).is_ok());
        assert!(serde_json::to_string(&s.throughput_at(BASE_MS)).is_ok());
    }

    #[test]
    fn test_throughput_ring_basic() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        // 同一秒 2 条（各 tokens=15），下一秒 1 条（tokens=3），跨全部凭据聚合
        s.on_record(&rec(0, Some(1), "m", RequestOutcome::Success, 10, 5));
        s.on_record(&rec(500, Some(2), "m", RequestOutcome::Success, 10, 5));
        s.on_record(&rec(1_000, Some(3), "m", RequestOutcome::Success, 2, 1));

        let snap = s.throughput_at(BASE_MS + 1_000);
        // 桶数固定 60，从旧到新，空秒补 0
        assert_eq!(snap.recent_buckets.len(), THROUGHPUT_BUCKETS);
        assert_eq!(snap.window_secs, THROUGHPUT_BUCKETS as u32);
        // 最新桶（now 秒）：1 条请求，3 tokens
        let last = snap.recent_buckets.last().unwrap();
        assert_eq!(last.requests, 1);
        assert_eq!(last.tokens, 3);
        assert_eq!(last.ts_ms, BASE_MS + 1_000);
        // 上一桶：2 条请求，30 tokens
        let prev = &snap.recent_buckets[THROUGHPUT_BUCKETS - 2];
        assert_eq!(prev.requests, 2);
        assert_eq!(prev.tokens, 30);
        // 窗口内合计：3 请求 / 33 tokens
        assert_eq!(snap.current_rpm, 3);
        assert!((snap.current_rps - 3.0 / 60.0).abs() < 1e-9);
        assert!((snap.current_tokens_per_sec - 33.0 / 60.0).abs() < 1e-9);
    }

    #[test]
    fn test_throughput_ring_expiry_and_overwrite() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        s.on_record(&rec(0, Some(1), "m", RequestOutcome::Success, 100, 100));
        // 时间前进到窗口之外（>60 秒），旧数据不再出现
        let later = s.throughput_at(BASE_MS + 120_000);
        assert_eq!(later.current_rpm, 0);
        assert_eq!(later.current_tokens_per_sec, 0.0);
        assert!(later.recent_buckets.iter().all(|b| b.requests == 0 && b.tokens == 0));

        // 相隔恰好一整圈（60 秒）落入同一桶但 slot 不同 → 清零覆盖，不叠加旧值
        let ring_span = THROUGHPUT_BUCKETS as i64 * THROUGHPUT_BUCKET_SECS * 1000;
        s.on_record(&rec(ring_span, Some(1), "m", RequestOutcome::Success, 7, 0));
        let snap = s.throughput_at(BASE_MS + ring_span);
        assert_eq!(snap.current_rpm, 1, "只应看到新记录");
        let last = snap.recent_buckets.last().unwrap();
        assert_eq!(last.tokens, 7);
    }
}






