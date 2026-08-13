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
use crate::model::config::ModelPrice;

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

/// 机器码派生（单一真相源）——供「按机器」视图展示与入口黑名单判定共用同一套派生逻辑，
/// 保证展示出来能复制的码与拦截时重算的码永远一致（绝不漂移）。
///
/// - `derive_machine_key`：与 [`ClientAgg::machine_key_of`] 同口径（IP 优先 → device 兜底 → unknown），
///   但接受裸字段，便于 handlers 在入口不构造 RequestRecord 也能算。
/// - `machine_code`：`MC-` + `SHA256(machine_key)` 前 12 位十六进制。稳定、可复制、不暴露裸 IP。
pub fn derive_machine_key(client_ip: Option<&str>, client_device: Option<&str>) -> String {
    if let Some(ip) = client_ip {
        if !ip.is_empty() {
            return ip.to_string();
        }
    }
    if let Some(dev) = client_device {
        if !dev.is_empty() {
            return dev.to_string();
        }
    }
    "unknown".to_string()
}

/// 由 machine_key 派生机器码：`MC-` + SHA256 十六进制前 12 位。
pub fn machine_code(machine_key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(machine_key.as_bytes());
    let hex = hex::encode(hasher.finalize());
    format!("MC-{}", &hex[..12])
}

/// 便捷封装：直接由裸客户端字段派生机器码。
pub fn machine_code_of(client_ip: Option<&str>, client_device: Option<&str>) -> String {
    machine_code(&derive_machine_key(client_ip, client_device))
}
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
    /// 输入 tokens 累计（**gross 口径**，见 [`RequestRecord::input_tokens`]，已含 cache 两项）
    pub input_tokens: i64,
    /// 输出 tokens 累计
    pub output_tokens: i64,
    /// 缓存读取 tokens 累计（`cache_read_input_tokens`）。
    ///
    /// 是 [`Self::input_tokens`] 的**子集**，不是额外增量 —— 算「总输入」时不得再加它。
    /// 用 i64 而 record 侧是 i32：单条上限 2^31，但累计上百万条会溢出 i32。
    pub cache_read_tokens: i64,
    /// 缓存新建 tokens 累计（`cache_creation_input_tokens`）。同样是 input 的子集。
    pub cache_creation_tokens: i64,
    /// credits 累计（仅累加有值的记录）
    pub credits_used: f64,
    /// 延迟累计（毫秒，用于算平均）
    pub latency_sum_ms: u64,
    /// TTFB 累计（毫秒）。**只累加有值的记录**，故必须配一个独立计数分母。
    #[serde(default)]
    pub first_token_sum_ms: u64,
    /// 有 TTFB 值的记录数（分母）。
    ///
    /// 为什么不能用 `requests` 当分母：`first_token_ms` 是 `Option` ——
    /// 非流式路径、纯错误响应、无内容响应都是 None。用 `requests` 平均会把这些
    /// 当成 0ms 摊进去，系统性拉低平均值。
    #[serde(default)]
    pub first_token_count: u64,
    /// 换号次数累计（`retries`）。
    ///
    /// 数据源：`handlers.rs` 四处写入点 + provider 失败路径的
    /// `fail_record.retries = attempts_used`。
    /// 「烧掉 12 次换号才失败」与「第一次就失败」的区分，正是判断重试预算够不够、
    /// 吸收层有没有效的唯一依据。
    /// 出口已接通：[`WindowSummary`] / [`SeriesPoint`] / [`GroupStat`] 三个 DTO 都下发
    /// 本字段与 [`Self::retried_requests`]（成对，勿只接一个）。
    #[serde(default)]
    pub retries_sum: u64,
    /// **发生过**重试的请求数（`retries > 0`）。
    ///
    /// 与 `retries_sum` 配对是承重的：绝大多数请求 `retries=0`，用 `requests` 当分母
    /// 算出的平均值会被压到接近 0（例：1000 条里 10 条各重试 6 次 ⇒ 6000/1000=0.06，
    /// 看着像"几乎不重试"，而真相是**那 10 条平均重试 6 次**）。
    /// 两个分母各有用途：`retries_sum / requests` 是整池放大倍数，
    /// `retries_sum / retried_requests` 是"真重试时重试几次"。
    #[serde(default)]
    pub retried_requests: u64,
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
        // 历史 JSONL 无这两个字段时 serde default 给 0，累加即无影响
        self.cache_read_tokens += r.cache_read_tokens as i64;
        self.cache_creation_tokens += r.cache_creation_tokens as i64;
        if let Some(c) = r.credits_used {
            self.credits_used += c;
        }
        self.latency_sum_ms += r.latency_ms;
        // TTFB 只在有值时累加（见 first_token_count 的说明）
        if let Some(ft) = r.first_token_ms {
            self.first_token_sum_ms += ft;
            self.first_token_count += 1;
        }
        // 换号次数：sum 恒累加（含 0），计数器只在真重试过时 +1（见 retried_requests）。
        self.retries_sum += r.retries as u64;
        if r.retries > 0 {
            self.retried_requests += 1;
        }
    }

    /// 把另一个聚合并入本聚合（用于跨桶汇总）
    fn merge(&mut self, other: &Aggregate) {
        self.requests += other.requests;
        self.success += other.success;
        self.failure += other.failure;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_creation_tokens += other.cache_creation_tokens;
        self.credits_used += other.credits_used;
        self.latency_sum_ms += other.latency_sum_ms;
        self.first_token_sum_ms += other.first_token_sum_ms;
        self.first_token_count += other.first_token_count;
        self.retries_sum += other.retries_sum;
        self.retried_requests += other.retried_requests;
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

    /// 平均 TTFB（毫秒）。**无有效样本时返回 None**（而非 0）。
    ///
    /// 返回 Option 而非 0 是刻意的：0ms 的 TTFB 在物理上不可能，把"没数据"显示成 0
    /// 比显示"—"危险得多（看起来像"快到测不出"）。这与 overview-page 故障时显示 '—'
    /// 而非 0 的既有约定一致。
    pub fn avg_first_token_ms(&self) -> Option<f64> {
        if self.first_token_count == 0 {
            None
        } else {
            Some(self.first_token_sum_ms as f64 / self.first_token_count as f64)
        }
    }

    /// 每请求平均换号次数（**整池放大倍数**口径，分母是全部请求）。
    ///
    /// 用途：与外置 shield 的放大倍数（实测 3.27x）同口径对比。
    pub fn avg_retries_per_request(&self) -> f64 {
        if self.requests == 0 {
            0.0
        } else {
            self.retries_sum as f64 / self.requests as f64
        }
    }

    /// 真发生重试时的平均次数（分母只算 `retries > 0` 的请求）。
    ///
    /// 与 [`Self::avg_retries_per_request`] 是**两个不同的问题**，不可互相替代：
    /// 绝大多数请求 `retries=0`，只看前者会把"少数请求重试很多次"稀释成"几乎不重试"。
    /// 无重试样本时返 `None`（而非 0.0）—— 0.0 会被误读成"重试过但只重试 0 次"。
    pub fn avg_retries_when_retried(&self) -> Option<f64> {
        if self.retried_requests == 0 {
            None
        } else {
            Some(self.retries_sum as f64 / self.retried_requests as f64)
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

/// 单台「机器」的画像元信息（机器维度聚合用）。
///
/// 机器身份由**设备画像**（device/os/browser）派生，与 IP 解耦：IP 只作为
/// 「这台机器见过的 IP 列表」记录，不参与分组，从而 IP 变化（DHCP/VPN/NAT）时
/// 同一台机器仍合并为一组。device/os/browser 采用「首见填充」（仅当当前为空时写入），
/// 让先出现的画像定义机器，后续记录只补齐缺失字段，避免同 session 画像抖动改写。
#[derive(Debug, Clone, Default)]
struct MachineMeta {
    /// 设备类型（如 claude-code）
    device: Option<String>,
    /// 操作系统细分（如 Windows）
    os: Option<String>,
    /// 浏览器 + 版本（非浏览器为 None）
    browser: Option<String>,
    /// 这台机器见过的所有 IP（去重集合）
    ips: std::collections::HashSet<String>,
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

    // ---- 机器指纹维度（by_client 的 IP 主键会因 IP 变化把同一台机器拆开，
    //      这里改用设备画像派生的稳定 machine_key，IP 仅作「见过的 IP」记录）----
    /// 机器维度速率环（key = machine_key）
    by_machine: HashMap<String, CredRateRing>,
    /// 机器画像元信息（首见填充 + 见过 IP 集合）
    machine_meta: HashMap<String, MachineMeta>,
    /// machine ⇄ session 归组关系
    machine_sessions: HashMap<String, std::collections::HashSet<String>>,
    /// session_id → machine_key 粘滞映射：一旦某 session 归属某机器，
    /// 后续该 session 记录即便换 IP / 画像细节抖动仍归原机器（防拆分）。
    session_machine: HashMap<String, String>,
}

impl ClientAgg {
    /// 从一条记录派生客户端 key：client_ip 优先，回退 device，都无则 "unknown"。
    fn client_key_of(r: &RequestRecord) -> String {
        r.client_ip
            .clone()
            .or_else(|| r.client_device.clone())
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// 从一条记录派生**机器 key**：以 client_ip 为主键（每个 IP = 一台机器）。
    ///
    /// ⚠️ 修正(2026-07-08):原设计用 device|os|browser 画像做 key、不含 IP,想实现"换 IP 也合并"。
    /// 但 **Claude Code 所有客户端的 device/os/browser 完全相同**(都是 claude-code),导致 N 台不同
    /// 机器(不同 IP)全被合并成 1 台——南辕北辙(dwgx:完全错了)。Claude Code 不提供稳定 per-机器
    /// 指纹,**IP 才是区分真实机器的唯一信号**。故机器分组回到以 IP 为主键;真正的"换 IP 合并"由
    /// **session 粘滞**处理(同一 session_id 换 IP 才合并,对应 DHCP/漫游),见 bump() 的 session_machine。
    /// device 仅在无 IP 时兜底,画像(os/browser)只作展示。
    ///
    /// 三者皆空时回退 `"unknown"`（与 device 的 unknown 兜底口径一致）。拼接用 `|`
    /// 分隔并对空字段留空段，保证同画像稳定映射到同一 key。
    fn machine_key_of(r: &RequestRecord) -> String {
        // 以 IP 为主键(每个 IP = 一台真实机器);无 IP 时回退 device;都无则 unknown。
        // 复用 [`derive_machine_key`] 保持与机器码派生同口径(单一真相源,绝不漂移)。
        derive_machine_key(r.client_ip.as_deref(), r.client_device.as_deref())
    }

    /// 本条记录是否带真实 client_ip(唯一稳定的机器区分信号)。
    ///
    /// device 兜底(claude-code 等所有客户端相同)与 `"unknown"` 都不能区分真实机器,
    /// 故它们不能作为 session 粘滞的归属锚点——否则所有缺 IP 的请求会全部粘到同一个
    /// `"unknown"`/device 黑洞,把互不相干的真实机器(及其 IP)错并成一台。
    fn has_stable_machine_key(r: &RequestRecord) -> bool {
        r.client_ip.as_deref().is_some_and(|ip| !ip.is_empty())
    }

    /// 某 machine_key 是否是真实 IP 派生(能 parse 成 IpAddr)。
    /// 用于区分「真实 IP 粘滞(DHCP 漫游,合法)」与「unknown/device 黑洞粘滞(误并)」。
    fn is_ip_key(key: &str) -> bool {
        key.parse::<std::net::IpAddr>().is_ok()
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
            // 单一归属:同一 session 的 client_key 变化时(如先无 IP 落 device/unknown、
            // 后来真实 IP 落 IP 组)先从其它所有 client 组移除,避免重复出现在两个客户端下。
            for (ck, sids) in self.client_sessions.iter_mut() {
                if ck != &client_key {
                    sids.remove(&sid);
                }
            }
            self.client_sessions
                .entry(client_key)
                .or_default()
                .insert(sid);
        }

        // ---- 机器指纹维度 ----
        // 归属规则(修复「unknown 黑洞」误并,同时保留 DHCP/漫游合并):
        //   稳定 key = 真实 IP(能 parse 成 IpAddr);"unknown"/device 兜底不稳定。
        // 1. 已粘到**真实 IP** 机器 → 沿用(DHCP/漫游:同 session 换 IP 不拆机器)。
        // 2. 粘到不稳定 key(unknown/device)但本条有真实 IP → 升级到真实 IP(根治:
        //    早先无 IP 粘到 unknown 的 session,拿到真实 IP 后归位,不再把真实 IP 灌进 unknown)。
        // 3. 粘到不稳定 key 且本条也无 IP → 沿用(没有更好的)。
        // 4. 无粘滞 → 按本条派生。
        let sticky = r
            .session_id
            .as_ref()
            .and_then(|sid| self.session_machine.get(sid))
            .cloned();
        let machine_key = match sticky {
            Some(mk) if Self::is_ip_key(&mk) => mk,
            Some(_) if Self::has_stable_machine_key(r) => Self::machine_key_of(r),
            Some(mk) => mk,
            None => Self::machine_key_of(r),
        };

        self.by_machine
            .entry(machine_key.clone())
            .or_insert_with(CredRateRing::new)
            .bump(slot);

        // 机器画像：device/os/browser 首见填充（仅当前为空才写，让首现画像定义机器），
        // IP 累积进「见过的 IP」集合。
        let mm = self.machine_meta.entry(machine_key.clone()).or_default();
        if mm.device.is_none() {
            mm.device = r.client_device.clone();
        }
        if mm.os.is_none() {
            mm.os = r.client_os.clone();
        }
        if mm.browser.is_none() {
            mm.browser = r.client_browser.clone();
        }
        if let Some(ip) = r.client_ip.clone() {
            mm.ips.insert(ip);
        }

        // 机器 ⇄ session 归组 + 粘滞映射。
        // 粘滞只锚定**真实 IP** 派生的 key(has_stable_machine_key):
        //   - 首次遇真实 IP → 记录粘滞;
        //   - 已有粘滞但本条是真实 IP 且与旧值不同 → 覆盖(把早先误粘到 "unknown"/device 的
        //     session 升级到真实机器,并把它从旧组移除,根治 unknown 黑洞越滚越大)。
        // 缺 IP 的请求绝不建立粘滞(否则不同真实机器的缺 IP 请求会互相并入同一 "unknown")。
        if let Some(sid) = r.session_id.clone() {
            // 单一归属不变量:一个 session 任一时刻只能属于一台机器。归到 machine_key 前,
            // 先把它从**其它所有**机器组移除——否则会重复出现在两台机器下(如先无 IP 落
            // "unknown"/device 组、后来真实 IP 落 IP 组,旧组残留没清 → RPM 双计、两处都列)。
            // 只有当 session 曾以真实 IP 建立粘滞、machine_key 又被粘滞锚回旧 IP 时才不迁移(漫游)。
            for (mk, set) in self.machine_sessions.iter_mut() {
                if mk != &machine_key {
                    set.remove(&sid);
                }
            }
            self.machine_sessions
                .entry(machine_key.clone())
                .or_default()
                .insert(sid.clone());
            // 粘滞映射只锚定真实 IP 派生的 key(缺 IP 不建粘滞,防 unknown 黑洞)。
            if Self::has_stable_machine_key(r) {
                self.session_machine.insert(sid, machine_key);
            }
        }
    }

    /// 惰性剔除窗口外（max_slot < now_slot-(RATE_BUCKETS-1)）不再活跃的条目。
    fn prune(&mut self, now_slot: i64) {
        let oldest = now_slot - (RATE_BUCKETS as i64 - 1);
        self.by_session.retain(|_, r| r.max_slot() >= oldest);
        self.by_client.retain(|_, r| r.max_slot() >= oldest);
        self.by_machine.retain(|_, r| r.max_slot() >= oldest);
        // session_meta / client_sessions 与存活的 session/client 对齐
        let live_sessions: std::collections::HashSet<String> =
            self.by_session.keys().cloned().collect();
        let live_clients: std::collections::HashSet<String> =
            self.by_client.keys().cloned().collect();
        let live_machines: std::collections::HashSet<String> =
            self.by_machine.keys().cloned().collect();
        self.session_meta
            .retain(|sid, _| live_sessions.contains(sid));
        for sids in self.client_sessions.values_mut() {
            sids.retain(|sid| live_sessions.contains(sid));
        }
        self.client_sessions
            .retain(|ck, sids| !sids.is_empty() || live_clients.contains(ck));

        // 机器维度：画像/归组与存活机器 + 存活 session 对齐
        self.machine_meta.retain(|mk, _| live_machines.contains(mk));
        for sids in self.machine_sessions.values_mut() {
            sids.retain(|sid| live_sessions.contains(sid));
        }
        self.machine_sessions
            .retain(|mk, sids| !sids.is_empty() || live_machines.contains(mk));
        // session_machine 粘滞映射仅保留存活 session，避免随 session_id 无界增长
        self.session_machine
            .retain(|sid, _| live_sessions.contains(sid));
    }
}

/// 全部内存聚合状态（受一把锁保护）
struct Inner {
    /// 小时环形桶
    hours: Vec<TimeBucket>,
    /// 天环形桶
    days: Vec<TimeBucket>,
    /// 按「上游实际服务模型」全量累计（key = `r.upstream_model` 映射后名，None 回落
    /// `r.model`）。与 [`Self::by_requested_model`]（客户端原始名口径）独立有界，
    /// 见 `Inner::apply` 的注释。
    by_model: HashMap<String, Aggregate>,
    /// 按「客户端请求的原始模型名」全量累计（key = `r.requested_model`，None 回落
    /// `r.model`）。与 `by_model` 独立有界，见 `Inner::apply` 的注释。
    by_requested_model: HashMap<String, Aggregate>,
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
    /// 模型名 key（`by_model` / `by_requested_model` 两表共用）的最大保留长度（超出即截断）。
    ///
    /// 模型名正常都在 40 字符内（目录里最长约 30）。128 留足余量，
    /// 同时阻断"用超长字符串放大单条内存"这一路。
    const MODEL_KEY_MAX_LEN: usize = 128;
    /// 单张模型表（`by_model` / `by_requested_model`）的最大不同模型数。超过后新模型名
    /// 一律归入 [`Self::MODEL_KEY_OTHER`]。
    ///
    /// 目录里的模型数是十几个量级，256 足以容纳"全部真实模型 + 少量历史遗留名"，
    /// 又把无界增长封死在常数级。
    const MODEL_KEY_CAP: usize = 256;
    /// 超出 [`Self::MODEL_KEY_CAP`] 后的归并桶名。
    const MODEL_KEY_OTHER: &'static str = "(other)";

    /// 把模型名收敛成**有界**的模型表 key（两表共用）。
    ///
    /// 两道：① 超长截断（UTF-8 安全，按字符边界）；② 表满则归入 OTHER 桶。
    /// 已存在的 key 永远直接命中（调用方先查 entry），所以真实模型一旦入表不会被挤进 OTHER。
    fn normalize_model_key(model: &str, current_len: usize) -> String {
        // 按**字符**截断而非字节，避免切裂多字节序列。
        let truncated: String = if model.len() > Self::MODEL_KEY_MAX_LEN {
            model.chars().take(Self::MODEL_KEY_MAX_LEN).collect()
        } else {
            model.to_string()
        };
        // current_len 是插入**前**的长度，故用 >= 判定。
        if current_len >= Self::MODEL_KEY_CAP {
            return Self::MODEL_KEY_OTHER.to_string();
        }
        truncated
    }

    fn new() -> Self {
        Inner {
            hours: vec![TimeBucket::new(); HOUR_BUCKETS],
            days: vec![TimeBucket::new(); DAY_BUCKETS],
            by_model: HashMap::new(),
            by_requested_model: HashMap::new(),
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

        // 按「上游实际服务模型」累计（映射双口径的 upstream 维度）。
        //
        // 🔴 修复的缺陷：`by_model` 的 key 是**外部可控字符串**，而这张表**永不回收** ——
        // `ClientAgg::prune` 只清 by_session / by_client / by_machine，不碰 by_model，
        // 全仓也没有任何 retain / 上限。
        //
        // 可控性链路（逐跳确认）：`handlers.rs` 的 custom_api 透传在 `should_try_custom_api_first()`
        // 为真时**先于** `convert_request` 执行，而它建 record 用的是
        // `meta.model.unwrap_or_else(|| payload.model.clone())` —— 即客户端 JSON 里的**原始**
        // model 字符串，从未过 `map_model` 校验（Kiro 主路径的 model 来自映射后的 kiro id，是受控的）。
        //
        // 后果：持有效 API key 的客户端用随机 model 名反复打 `/v1/messages`（服务端配了代挂号时），
        // 每次都在 by_model 里多一个永久条目 → 内存单调增长，重启才清；更糟的是
        // `rebuild_from_logs` 冷启动会把 30 天内 JSONL 里的全部脏 key **重放回内存**。
        // 且 `GET /api/admin/usage/models` 会把整张表序列化返回，面板一并被拖垮。
        // 实测：500 个随机 model 名 → 500 个条目，无任何回收。
        //
        // 修法是在**边界**收敛而非事后清理：超长名截断、表满则归入 OTHER 桶。
        // 归桶而不是丢弃，是为了保住"总量守恒"——面板的模型分布仍能对上总请求数。
        //
        // ⭐ 2026-08-11 全量审计修复（双口径复制品）：key 改用 `upstream_model`
        // （映射后/上游实际服务名，None 回落 `r.model`）。旧实现的 key 是 `r.model`，
        // 与 `by_requested_model` 的回落值（`requested_model` 要么等于 `r.model`、要么
        // None 回落）恒等 ⇒ 两表内容恒等，`by_model` 从未表达过注释声称的「上游口径」。
        // 映射后名虽是受控集，仍走同一套有界策略（MODEL_KEY_CAP/MAX_LEN/OTHER 对两表
        // 分别生效）——历史 JSONL 重放可能带旧实现的脏 key/预判值，不能假设受控。
        // 未映射/失败记录（None）回落 `r.model`，保证「上游维度请求总数 = 总请求数」。
        self.by_model
            .entry(Self::normalize_model_key(
                r.upstream_model.as_deref().unwrap_or(&r.model),
                self.by_model.len(),
            ))
            .or_default()
            .add(r);

        // 双口径的第二张表：**客户端请求的原始模型名**（`requested_model`；`None` 回落
        // `r.model`）。与 `by_model` 各自独立有界（MODEL_KEY_CAP/MAX_LEN/OTHER 对两表
        // 分别生效），避免「一张表两个口径」绕过表满归桶 —— 外部可控字符串无法靠
        // 映射把同一个 key 塞进两张表挤爆任意一张。
        //
        // 两表语义（2026-08-11 审计修复后）：`by_model` 收 `upstream_model`（映射后名），
        // 本表收 `requested_model`（客户端原始名）——映射命中且改写时两表 key 不同；
        // 未映射/失败记录（None）各自回落 `r.model`，两维度请求总数恒等。
        self.by_requested_model
            .entry(Self::normalize_model_key(
                r.requested_model.as_deref().unwrap_or(&r.model),
                self.by_requested_model.len(),
            ))
            .or_default()
            .add(r);

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
    /// 输入 tokens 累计（gross，已含 cache 两项）
    pub input_tokens: i64,
    /// 输出 tokens 累计
    pub output_tokens: i64,
    /// tokens 总计（输入+输出）
    pub total_tokens: i64,
    /// 缓存读取 tokens 累计（是 [`Self::input_tokens`] 的子集，不可再加）
    pub cache_read_tokens: i64,
    /// 缓存新建 tokens 累计（同为 [`Self::input_tokens`] 的子集）
    pub cache_creation_tokens: i64,
    /// credits 累计
    pub credits_used: f64,
    /// 平均延迟（毫秒）
    pub avg_latency_ms: f64,
    /// 换号次数累计（[`Aggregate::retries_sum`] 的直出）。
    ///
    /// 与 [`Self::retried_requests`] **成对**下发、且两个原始计数都不省略：
    /// 前端要能自己换分母（整池放大倍数 vs 真重试时几次），只给一个平均值会锁死口径。
    pub retries_sum: u64,
    /// **发生过**重试的请求数（`retries > 0`），[`Self::retries_sum`] 的第二个分母。
    pub retried_requests: u64,
    /// 每请求平均换号次数（分母 = 全部请求）。可与外置 shield 的放大倍数同口径对比。
    pub avg_retries_per_request: f64,
    /// 真发生重试时的平均次数（分母 = `retried_requests`）。
    ///
    /// 无重试样本时为 `null` 而非 0 —— 0 会被读成"重试过但只重试 0 次"。
    pub avg_retries_when_retried: Option<f64>,
    /// 平均 TTFB（毫秒）。无有效样本时为 `null`（见 [`Aggregate::avg_first_token_ms`]：
    /// 0ms 物理上不可能，把"没数据"显示成 0 比显示"—"危险得多）。
    pub avg_first_token_ms: Option<f64>,
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
            cache_read_tokens: a.cache_read_tokens,
            cache_creation_tokens: a.cache_creation_tokens,
            credits_used: a.credits_used,
            avg_latency_ms: a.avg_latency_ms(),
            retries_sum: a.retries_sum,
            retried_requests: a.retried_requests,
            avg_retries_per_request: a.avg_retries_per_request(),
            avg_retries_when_retried: a.avg_retries_when_retried(),
            avg_first_token_ms: a.avg_first_token_ms(),
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
    /// 全部(保留期内所有天桶合计;受 stats 保留期限制,非严格历史全量)
    pub all_time: WindowSummary,
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
    /// 输入 tokens（gross，已含 cache 两项）
    pub input_tokens: i64,
    /// 输出 tokens
    pub output_tokens: i64,
    /// 缓存读取 tokens（是 [`Self::input_tokens`] 的子集，不可再加）
    pub cache_read_tokens: i64,
    /// 缓存新建 tokens（同为 [`Self::input_tokens`] 的子集）
    pub cache_creation_tokens: i64,
    /// credits 累计
    pub credits_used: f64,
    /// 平均延迟（毫秒）
    pub avg_latency_ms: f64,
    /// 该桶内的换号次数累计（画重试趋势的唯一数据源）。
    ///
    /// 序列点上**只给原始计数、不给平均值**：分母（requests / retried_requests）
    /// 两个都在同一个点里，前端要哪个口径自己除，避免在 DTO 层锁死口径。
    pub retries_sum: u64,
    /// 该桶内**发生过**重试的请求数（`retries > 0`）。
    pub retried_requests: u64,
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
    /// 输入 tokens（gross，已含 cache 两项）
    pub input_tokens: i64,
    /// 输出 tokens
    pub output_tokens: i64,
    /// 缓存读取 tokens（是 [`Self::input_tokens`] 的子集，不可再加）
    pub cache_read_tokens: i64,
    /// 缓存新建 tokens（同为 [`Self::input_tokens`] 的子集）
    pub cache_creation_tokens: i64,
    /// credits 累计
    pub credits_used: f64,
    /// 估算成本（元）：按模型单价表（[`crate::model::config::Config::model_pricing`]）
    /// 推算，见 [`estimate_cost`]。
    ///
    /// 无单价表 / 模型不在表中 = 0.0（不估算）。与 [`Self::credits_used`]（上游真实
    /// 计费）是**两个独立口径**：credits 是上游 metering 返回的真值，cost 是本机按
    /// 单价表对 token 用量的推算，仅作「哪个模型最花钱」的排序参考。
    pub cost: f64,
    /// 平均延迟（毫秒）
    pub avg_latency_ms: f64,
    /// 该分组的换号次数累计：看**哪个模型 / 哪个号**在烧重试预算。
    pub retries_sum: u64,
    /// 该分组内**发生过**重试的请求数（`retries > 0`），与 [`Self::retries_sum`] 成对。
    pub retried_requests: u64,
    /// 该分组每请求平均换号次数（分母 = 该分组请求数）。
    pub avg_retries_per_request: f64,
}

/// 按单价表估算成本（元）：`tokens / 1_000_000 × 单价`，四档分别计价后求和。
///
/// - `input_tokens` 传 **gross** 口径（[`Aggregate::input_tokens`] 已含 cache 两项）：
///   input 部分内部按 billed 口径折算（减去 cache 读+建，饱和非负），避免缓存被计两次
///   （与 [`crate::usage::record::RequestRecord::billed_input_tokens`] 同口径）。
/// - `price` 为 None（模型无单价）时返回 0.0（不估算）。
///
/// 纯函数、无状态，可独立单测。
fn estimate_cost(
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_creation_tokens: i64,
    price: Option<&ModelPrice>,
) -> f64 {
    let Some(p) = price else {
        return 0.0;
    };
    let billed_input = (input_tokens - cache_read_tokens - cache_creation_tokens).max(0);
    let per_mtok = |tokens: i64, unit: f64| tokens as f64 / 1_000_000.0 * unit;
    per_mtok(billed_input, p.input_per_mtok)
        + per_mtok(output_tokens, p.output_per_mtok)
        + per_mtok(cache_read_tokens, p.cache_read_per_mtok)
        + per_mtok(cache_creation_tokens, p.cache_creation_per_mtok)
}

impl GroupStat {
    fn from(key: String, a: &Aggregate, price: Option<&ModelPrice>) -> Self {
        GroupStat {
            key,
            requests: a.requests,
            success_rate: a.success_rate(),
            input_tokens: a.input_tokens,
            output_tokens: a.output_tokens,
            cache_read_tokens: a.cache_read_tokens,
            cache_creation_tokens: a.cache_creation_tokens,
            credits_used: a.credits_used,
            cost: estimate_cost(
                a.input_tokens,
                a.output_tokens,
                a.cache_read_tokens,
                a.cache_creation_tokens,
                price,
            ),
            avg_latency_ms: a.avg_latency_ms(),
            retries_sum: a.retries_sum,
            retried_requests: a.retried_requests,
            avg_retries_per_request: a.avg_retries_per_request(),
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

/// 单台机器（按设备指纹分组，IP 变化不拆分）的 RPM 视图。
///
/// 与 [`ClientRpm`]（按 IP 分组）的关键区别：分组主键是**设备画像派生的
/// machine_key**（不含 IP），IP 只作 [`ips`] 列表展示；同一 session 一旦归属某机器，
/// 后续换 IP 仍归该机器。供前端「机器指纹分组」视图使用。
///
/// 单个 IP → 机器码映射。用于漫游机器（多 IP）逐 IP 展示可复制的封禁码。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineIpCode {
    /// 该 IP 字符串
    pub ip: String,
    /// 该 IP 派生的机器码（`machine_code(ip)`，入口按当前请求 IP 重算时命中的正是它）
    pub code: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineRpm {
    /// 机器分组键（设备画像派生，稳定标识一台机器）
    pub machine_key: String,
    /// 机器码（`MC-` + SHA256 前 12 位，可复制、用于黑名单封禁；不暴露裸 IP）。
    /// 对应 [`machine_key`]。当机器见过多个 IP（漫游）时，主键码只覆盖粘滞 IP，
    /// 封禁其它 IP 需用 [`ip_codes`] 里对应 IP 的码——因为入口拦截按**当前请求 IP** 重算。
    pub machine_code: String,
    /// 设备类型（如 claude-code）
    pub device: Option<String>,
    /// 操作系统细分（如 Windows）
    pub os: Option<String>,
    /// 浏览器 + 版本（非浏览器为 None）
    pub browser: Option<String>,
    /// 这台机器见过的所有 IP（升序去重）
    pub ips: Vec<String>,
    /// 每个见过的 IP 各自的机器码（`ip → machine_code(ip)`，与 [`ips`] 同序）。
    /// 前端应展示每个 IP 的码供复制：复制哪个 IP 的码就精准封哪个 IP，
    /// 与入口「按当前请求 IP 重算」的拦截口径一一对应（漫游多 IP 也能逐个封）。
    pub ip_codes: Vec<MachineIpCode>,
    /// 该机器最近 60 秒请求数（RPM，聚合其所有窗口）
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

        // ══════════════════════════════════════════════════════════════════════
        // 🔴 修的缺陷：窗口名与实际覆盖范围不符，导致面板数字每天定时"跳水"。
        //
        // 原实现的 `last_7d` 是「最近 7 个**日历天桶**」（`slot >= now_day - 6`），
        // 不是「最近 7×24 小时」。当天那个桶只累积到"此刻"，所以实际覆盖是
        // `6 天 + 今天已过的时长`：
        //   - UTC 刚过零点（00:05）→ 实际只覆盖 **6.00 天**（比名字少 23.9 小时）
        //   - UTC 12:00          → 6.50 天
        //   - UTC 23:55          → 7.00 天
        // 于是每天 UTC 零点一到，`last_7d` 会**断崖式掉掉近一天的量**，看起来像流量
        // 暴跌，实际只是窗口缩了。`last_30d` 同理，`last_24h` 也少一个小时零头
        // （`slot >= now_hour - 23` 只有 23 个整小时 + 当前小时的零头）。
        //
        // 改法：**统一用小时桶算真正的滚动窗口**。小时环有 744 个桶（31 天），
        // 足够覆盖 24h / 7d / 30d 三个窗口，所以不需要天桶参与，也就不会被
        // 日历边界截断。窗口定义变成闭区间 `[now_hour - (N-1), now_hour]`，
        // 即"最近 N 个小时桶"，N 分别取 24 / 168 / 720。
        //
        // 残留的粒度误差只有「当前小时未走完」这一项（最多 59 分钟），且它对三个窗口
        // 是**同向同量**的，不会再出现零点跳水。要完全消除得按毫秒切分桶内数据，
        // 而桶是聚合值、内部已无时间信息，做不到 —— 这是刻意接受的精度上限。
        //
        // ⚠️ 时区：桶按 UTC 对齐（`HOUR_MS`/`DAY_MS` 整除，无本地偏移）。滚动窗口
        // 对时区**不敏感**（"最近 24 小时"在任何时区都是同一段时间），所以本修复顺带
        // 消除了原实现"当天=UTC 当天"带来的时区困扰：+08:00 的用户原先看到的"今天"
        // 是北京时间早 8 点才开始的一天。日历口径若将来要做（如"本月账单"），
        // 需要额外引入时区配置，那是另一件事。
        // ══════════════════════════════════════════════════════════════════════
        const H_24H: i64 = 24;
        const H_7D: i64 = 7 * 24;
        const H_30D: i64 = 30 * 24;

        let mut agg24 = Aggregate::default();
        let mut agg7 = Aggregate::default();
        let mut agg30 = Aggregate::default();
        for b in &inner.hours {
            // slot < 0 是未初始化的空桶；slot > now_hour 是时钟回拨留下的未来桶，都跳过。
            if b.slot < 0 || b.slot > now_hour {
                continue;
            }
            let age = now_hour - b.slot; // 0 = 当前小时
            if age < H_24H {
                agg24.merge(&b.agg);
            }
            if age < H_7D {
                agg7.merge(&b.agg);
            }
            if age < H_30D {
                agg30.merge(&b.agg);
            }
        }

        // 全部：仍用天桶（保留期内所有天，受 stats 保留期限制，非严格历史全量）。
        // 这里用天桶是对的 —— all_time 不是滚动窗口，不存在边界截断问题，而天桶的
        // 保留期（31 天）与小时桶一致但内存占用小得多。
        let mut agg_all = Aggregate::default();
        for b in &inner.days {
            if b.slot < 0 || b.slot > now_day {
                continue;
            }
            agg_all.merge(&b.agg);
        }

        Overview {
            last_24h: agg24.into(),
            last_7d: agg7.into(),
            last_30d: agg30.into(),
            all_time: agg_all.into(),
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
                cache_read_tokens: agg.cache_read_tokens,
                cache_creation_tokens: agg.cache_creation_tokens,
                credits_used: agg.credits_used,
                avg_latency_ms: agg.avg_latency_ms(),
                retries_sum: agg.retries_sum,
                retried_requests: agg.retried_requests,
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
                cache_read_tokens: agg.cache_read_tokens,
                cache_creation_tokens: agg.cache_creation_tokens,
                credits_used: agg.credits_used,
                avg_latency_ms: agg.avg_latency_ms(),
                retries_sum: agg.retries_sum,
                retried_requests: agg.retried_requests,
            });
        }
        out
    }

    /// 按「上游实际服务模型」全量聚合（映射双口径的 upstream 维度；按请求数降序）。
    ///
    /// key = `r.upstream_model`（映射后/上游实际服务名），`None`（未映射/失败记录）
    /// 回落 `r.model`。与 [`Self::by_requested_model`]（客户端原始名）是同一批记录的
    /// 两个口径，请求总数恒等，只是分组键不同。
    ///
    /// `pricing` 为模型单价表（空表 = 不估算成本）：key 按本表口径（上游实际服务名）
    /// 命中，命中行 [`GroupStat::cost`] 按 [`estimate_cost`] 推算，未命中为 0.0。
    pub fn by_model(&self, pricing: &HashMap<String, ModelPrice>) -> Vec<GroupStat> {
        let inner = self.inner.lock();
        let mut out: Vec<GroupStat> = inner
            .by_model
            .iter()
            .map(|(k, a)| GroupStat::from(k.clone(), a, pricing.get(k)))
            .collect();
        out.sort_by(|a, b| b.requests.cmp(&a.requests).then(a.key.cmp(&b.key)));
        out
    }

    /// 按「客户端请求的原始模型名」全量聚合（映射双口径的 requested 维度）。
    ///
    /// 与 [`Self::by_model`] 是同一批记录的两个口径：`by_model` 记**上游实际服务**的
    /// 模型（`upstream_model`，映射命中时是映射后名；未映射回落 `r.model`），本方法记
    /// **客户端点名**的模型（`requested_model`，`None` 回落 `r.model`）。两维度的请求
    /// 总数恒等（同一批 `apply`），只是分组键不同。
    ///
    /// `pricing` 同 [`Self::by_model`]；注意单价表按**上游实际服务名**配置，
    /// 客户端原始名通常不在表中（未映射时两表 key 相同才会命中），cost 多为 0.0。
    pub fn by_requested_model(&self, pricing: &HashMap<String, ModelPrice>) -> Vec<GroupStat> {
        let inner = self.inner.lock();
        let mut out: Vec<GroupStat> = inner
            .by_requested_model
            .iter()
            .map(|(k, a)| GroupStat::from(k.clone(), a, pricing.get(k)))
            .collect();
        out.sort_by(|a, b| b.requests.cmp(&a.requests).then(a.key.cmp(&b.key)));
        out
    }

    /// 按凭据全量聚合（按请求数降序，key 为凭据 ID 字符串）。
    ///
    /// 成本不按凭据估算（单价表按模型配置，聚合层丢掉了模型维度）——
    /// 但 `cost` 字段随行下发恒为 0.0，前端统一按「有值且 >0 才显示」处理。
    pub fn by_credential(&self) -> Vec<GroupStat> {
        let inner = self.inner.lock();
        let mut out: Vec<GroupStat> = inner
            .by_credential
            .iter()
            .map(|(k, a)| GroupStat::from(k.to_string(), a, None))
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

    /// 机器指纹 RPM 视图：按设备画像分组的每台机器当前 RPM + 见过的 IP + 活跃窗口。
    ///
    /// 与 [`clients`] 的关键区别：分组主键是设备画像派生的 machine_key（不含 IP），
    /// 因此同一台机器换 IP（DHCP/VPN/NAT）仍合并为一组；IP 作为 [`MachineRpm::ips`]
    /// 列表展示。仅返回近 10 分钟内有活动的机器/窗口（查询时惰性 prune）。按 RPM 降序。
    pub fn machines(&self) -> Vec<MachineRpm> {
        self.machines_at(chrono::Utc::now().timestamp_millis())
    }

    /// 机器指纹 RPM 视图（可注入基准时间，便于测试）
    pub fn machines_at(&self, now_ms: i64) -> Vec<MachineRpm> {
        let now_slot = now_ms.div_euclid(RATE_BUCKET_SECS * 1000);
        let mut inner = self.inner.lock();
        inner.client_agg.prune(now_slot);

        let mut out: Vec<MachineRpm> = Vec::with_capacity(inner.client_agg.by_machine.len());
        for (machine_key, ring) in &inner.client_agg.by_machine {
            let rpm = ring.rpm(now_slot);
            // 该机器名下的活跃窗口
            let mut sessions: Vec<SessionRpm> = Vec::new();
            if let Some(sids) = inner.client_agg.machine_sessions.get(machine_key) {
                for sid in sids {
                    if let Some(sring) = inner.client_agg.by_session.get(sid) {
                        let s_rpm = sring.rpm(now_slot);
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

            // 机器画像 + 见过的 IP（升序，便于前端稳定展示）
            let (device, os, browser, mut ips) = inner
                .client_agg
                .machine_meta
                .get(machine_key)
                .map(|m| {
                    (
                        m.device.clone(),
                        m.os.clone(),
                        m.browser.clone(),
                        m.ips.iter().cloned().collect::<Vec<String>>(),
                    )
                })
                .unwrap_or((None, None, None, Vec::new()));
            ips.sort();

            // 每个见过的 IP 各派生一个码：复制哪个 IP 的码就封哪个 IP，与入口「按当前请求 IP
            // 重算」的拦截口径一一对应（漫游机器多 IP 时逐个可封，不留绕过缺口）。
            let ip_codes: Vec<MachineIpCode> = ips
                .iter()
                .map(|ip| MachineIpCode {
                    ip: ip.clone(),
                    code: machine_code(ip),
                })
                .collect();

            out.push(MachineRpm {
                machine_code: machine_code(machine_key),
                machine_key: machine_key.clone(),
                device,
                os,
                browser,
                ips,
                ip_codes,
                rpm,
                active_sessions: sessions.len(),
                sessions,
            });
        }
        out.sort_by(|a, b| b.rpm.cmp(&a.rpm).then(a.machine_key.cmp(&b.machine_key)));
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
    /// 只读内存聚合，零上游调用（避免触发上游风控）。供前端画数据流动粒子。
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

    /// 空单价表（= 不估算成本），供既有用例传参
    fn no_pricing() -> HashMap<String, ModelPrice> {
        HashMap::new()
    }

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

    /// 同 [`rec`]，额外指定 cache 读取/新建 tokens（两者均为 input 的子集）
    fn rec_cache(
        offset_ms: i64,
        cid: Option<u64>,
        model: &str,
        input: i32,
        output: i32,
        cache_read: i32,
        cache_creation: i32,
    ) -> RequestRecord {
        let mut r = rec(
            offset_ms,
            cid,
            model,
            RequestOutcome::Success,
            input,
            output,
        );
        r.cache_read_tokens = cache_read;
        r.cache_creation_tokens = cache_creation;
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
        s.on_record(&rec(
            120_000,
            Some(1),
            "m1",
            RequestOutcome::RateLimited,
            0,
            0,
        ));

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

    /// 🔴 窗口是**滚动**的，不是日历天桶 —— 钉死"零点跳水"缺陷。
    ///
    /// 构造：在"6 天 23 小时前"放一条。它距今不足 7×24 小时，所以**必须**计入
    /// `last_7d`。旧实现按日历天桶算（`slot >= now_day - 6`），这条记录落在第 7 天
    /// 之前的桶里会被排除 —— 于是每天 UTC 零点一过，`last_7d` 就少掉近一天的量。
    #[test]
    fn overview_windows_are_rolling_not_calendar_days() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        // now 取一个"刚过 UTC 零点"的时刻，这是旧实现偏差最大的相位。
        let now = BASE_MS + 5 * 60_000; // BASE_MS 是 UTC 日边界，+5 分钟
        // 6 天 23 小时前：距今 167h < 168h ⇒ 属于最近 7 天。
        s.on_record(&rec(
            5 * 60_000 - 167 * HOUR_MS,
            Some(1),
            "m",
            RequestOutcome::Success,
            1,
            1,
        ));
        // 23 小时前：属于最近 24 小时。
        s.on_record(&rec(
            5 * 60_000 - 23 * HOUR_MS,
            Some(1),
            "m",
            RequestOutcome::Success,
            1,
            1,
        ));

        let ov = s.overview_at(now);
        assert_eq!(
            ov.last_24h.requests, 1,
            "23 小时前那条必须计入 last_24h（滚动 24 小时）"
        );
        assert_eq!(
            ov.last_7d.requests, 2,
            "6 天 23 小时前那条必须计入 last_7d —— 旧的日历天桶口径会漏掉它，\
             导致每天 UTC 零点后 last_7d 断崖式掉近一天的量"
        );
        assert_eq!(ov.last_30d.requests, 2, "两条都在 30 天内");
    }

    /// 滚动窗口的边界是排他的：正好超出窗口的记录不得计入。
    #[test]
    fn overview_windows_exclude_records_just_outside() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        let now = BASE_MS + 30 * HOUR_MS;
        // 距今正好 24 小时（age == 24）⇒ 落在 last_24h 之外，但在 7d 内。
        s.on_record(&rec(
            30 * HOUR_MS - 24 * HOUR_MS,
            Some(1),
            "m",
            RequestOutcome::Success,
            1,
            1,
        ));
        let ov = s.overview_at(now);
        assert_eq!(
            ov.last_24h.requests, 0,
            "age 恰好 24 小时应被排除（窗口是最近 24 个小时桶：age < 24）"
        );
        assert_eq!(ov.last_7d.requests, 1, "但仍在 7 天窗口内");
    }

    #[test]
    fn test_cross_hour_and_cross_day() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        // 三个不同小时各 1 条（同一天）
        s.on_record(&rec(0, Some(1), "m", RequestOutcome::Success, 1, 1));
        s.on_record(&rec(HOUR_MS, Some(1), "m", RequestOutcome::Success, 1, 1));
        s.on_record(&rec(
            2 * HOUR_MS,
            Some(1),
            "m",
            RequestOutcome::Success,
            1,
            1,
        ));

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
        s.on_record(&rec(
            1000,
            Some(1),
            "sonnet",
            RequestOutcome::Success,
            10,
            5,
        ));
        s.on_record(&rec(
            2000,
            Some(2),
            "opus",
            RequestOutcome::ServerError,
            3,
            0,
        ));

        let models = s.by_model(&no_pricing());
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

    /// ⭐ 成本估算纯函数：输入 tokens（gross 口径）+ 单价 → 元。
    ///
    /// 关键口径：`input_tokens` 是 gross（已含 cache 两项），input 部分必须按 billed
    /// （gross 减 cache 读+建）计价 —— 否则缓存被计两次，成本系统性偏高。
    #[test]
    fn estimate_cost_pure_function() {
        let price = ModelPrice {
            input_per_mtok: 2.5,
            output_per_mtok: 12.5,
            cache_read_per_mtok: 0.3,
            cache_creation_per_mtok: 3.125,
        };
        // 100 万 input（其中 60 万 cache_read + 20 万 cache_creation）+ 40 万 output：
        // billed input = 1_000_000 - 600_000 - 200_000 = 200_000
        // 0.2M × 2.5 + 0.4M × 12.5 + 0.6M × 0.3 + 0.2M × 3.125
        // = 0.5 + 5.0 + 0.18 + 0.625 = 6.305
        let cost = estimate_cost(1_000_000, 400_000, 600_000, 200_000, Some(&price));
        assert!((cost - 6.305).abs() < 1e-9, "cost={cost}");

        // cache 超过 gross 的反常数据：billed input 饱和减为 0，input 部分零计价，
        // 但 cache 读/建仍按各自单价计（它们是真实发生的用量）。
        let cost2 = estimate_cost(100, 0, 80, 40, Some(&price));
        let expected2 = 80.0 / 1e6 * 0.3 + 40.0 / 1e6 * 3.125;
        assert!((cost2 - expected2).abs() < 1e-12, "cost2={cost2}");

        // 无单价（None）→ 0.0（不估算）
        assert_eq!(estimate_cost(1_000_000, 1, 0, 0, None), 0.0);
    }

    /// ⭐ 成本只按「上游实际服务名」命中单价表：命中行有 cost，未命中行与空表恒 0。
    #[test]
    fn by_model_estimates_cost_from_pricing_table() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        s.on_record(&rec_cache(0, Some(1), "sonnet", 1_000_000, 400_000, 600_000, 200_000));
        s.on_record(&rec_cache(1_000, Some(1), "opus", 100, 50, 0, 0));

        // 空表（不估算）→ 全部 cost=0
        let models = s.by_model(&no_pricing());
        assert!(models.iter().all(|m| m.cost == 0.0), "空表不得估算成本");

        // 命中表：sonnet 有价，opus 不在表中 → 0
        let mut pricing = HashMap::new();
        pricing.insert(
            "sonnet".to_string(),
            ModelPrice {
                input_per_mtok: 2.5,
                output_per_mtok: 12.5,
                cache_read_per_mtok: 0.3,
                cache_creation_per_mtok: 3.125,
            },
        );
        let models = s.by_model(&pricing);
        let sonnet = models.iter().find(|m| m.key == "sonnet").unwrap();
        assert!(
            (sonnet.cost - 6.305).abs() < 1e-9,
            "sonnet 应命中单价表：cost={}",
            sonnet.cost
        );
        let opus = models.iter().find(|m| m.key == "opus").unwrap();
        assert_eq!(opus.cost, 0.0, "opus 不在单价表，不得估算");

        // 序列化出口：cost 字段真的下发（字段在但没下发 = 前端白改）
        let json = serde_json::to_string(&models).unwrap();
        assert!(json.contains("\"cost\":6.305"), "{json}");

        // 按凭据恒 0（单价表按模型配置，聚合层无模型维度）
        assert!(s.by_credential().iter().all(|c| c.cost == 0.0));
    }

    /// 映射双口径：`by_model`（上游实际服务名）与 `by_requested_model`（客户端原始名）
    /// 是同一批记录的两种分组，请求总数恒等，只是 key 不同。
    ///
    /// ⭐ 2026-08-11 修复后的语义：`by_model` 聚合 `upstream_model`（映射后名），
    /// `by_requested_model` 聚合 `requested_model`（客户端原始名）——映射后名与原始名
    /// 不同的记录**分属两表**。修复前 `by_model` key 用 `r.model`，与回落值恒等 ⇒
    /// 两表内容恒等（审计登记的双口径复制品缺陷）。
    #[test]
    fn test_by_model_and_by_requested_model_two_dimensions() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        // 客户端点 claude-haiku-4-5（r.model = 原始名，与真实埋点形态一致），
        // 映射后上游实际服务 claude-sonnet-4-5。
        // ⚠️ 构造必须这样：若把 r.model 直接设成映射后名，旧实现（key=r.model）下
        // 本测试的「by_model 不得出现原始名」断言照样绿 —— 回归保护失效
        // （2026-08-11 对抗审查 M1）。
        {
            let mut r = rec(0, Some(1), "claude-haiku-4-5", RequestOutcome::Success, 10, 5);
            r.requested_model = Some("claude-haiku-4-5".to_string());
            r.upstream_model = Some("claude-sonnet-4-5".to_string());
            s.on_record(&r);
        }
        // 未映射的记录：requested_model / upstream_model 缺省（None）→ 都回落 model。
        s.on_record(&rec(1000, Some(1), "claude-opus-4-8", RequestOutcome::Success, 20, 10));

        let upstream = s.by_model(&no_pricing());
        assert_eq!(upstream.len(), 2, "上游维度应有 sonnet + opus 两条");
        let sonnet = upstream.iter().find(|m| m.key == "claude-sonnet-4-5").unwrap();
        assert_eq!(sonnet.requests, 1);

        let requested = s.by_requested_model(&no_pricing());
        assert_eq!(requested.len(), 2, "请求维度应有 haiku + opus 两条");
        let haiku = requested.iter().find(|m| m.key == "claude-haiku-4-5").unwrap();
        assert_eq!(haiku.requests, 1);
        let opus = requested.iter().find(|m| m.key == "claude-opus-4-8").unwrap();
        assert_eq!(opus.requests, 1);

        // 分属两表：by_model 不得出现客户端原始名，by_requested_model 不得出现映射后名。
        assert!(
            upstream.iter().all(|m| m.key != "claude-haiku-4-5"),
            "by_model 按映射后名分组，不得出现客户端原始名"
        );
        assert!(
            requested.iter().all(|m| m.key != "claude-sonnet-4-5"),
            "by_requested_model 按客户端原始名分组，不得出现映射后名"
        );

        // 两维度总数恒等（同一批记录的不同分组）。
        let upstream_total: u64 = upstream.iter().map(|g| g.requests).sum();
        let requested_total: u64 = requested.iter().map(|g| g.requests).sum();
        assert_eq!(upstream_total, requested_total);
        assert_eq!(upstream_total, 2);
    }

    /// ⭐ 2026-08-11 全量审计修复（双口径复制品）的回落回归：
    /// `by_model` 聚合 `upstream_model`（映射后名），`None` 时回落 `r.model`。
    ///
    /// 三类样本：① 映射后名 ≠ 原始名 → 分属两表；② 映射后名 = 原始名（等价未映射）；
    /// ③ `upstream_model = None`（未映射/失败记录）→ `by_model` 回落 `r.model`。
    #[test]
    fn by_model_aggregates_upstream_model_with_fallback() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        // ① 映射后名 ≠ 原始名（r.model = 原始名，与真实埋点形态一致；旧实现 key=r.model
        // 时本测试的「by_model 不得出现原始名」断言必红 —— 对抗审查 M1 修正）。
        {
            let mut r = rec(0, Some(1), "claude-haiku-4-5", RequestOutcome::Success, 10, 5);
            r.requested_model = Some("claude-haiku-4-5".to_string());
            r.upstream_model = Some("claude-sonnet-4-5".to_string());
            s.on_record(&r);
        }
        // ② 映射后名 = 原始名（映射到同名，等价未映射）。
        {
            let mut r = rec(1_000, Some(1), "claude-opus-4-8", RequestOutcome::Success, 20, 10);
            r.requested_model = Some("claude-opus-4-8".to_string());
            r.upstream_model = Some("claude-opus-4-8".to_string());
            s.on_record(&r);
        }
        // ③ upstream_model = None（未映射/失败记录）→ by_model 回落 r.model。
        s.on_record(&rec(2_000, Some(1), "claude-opus-4-8", RequestOutcome::Success, 5, 5));

        let upstream = s.by_model(&no_pricing());
        let sonnet = upstream
            .iter()
            .find(|m| m.key == "claude-sonnet-4-5")
            .unwrap_or_else(|| panic!("映射后名应出现在 by_model: {:?}", upstream));
        assert_eq!(sonnet.requests, 1);
        let opus = upstream
            .iter()
            .find(|m| m.key == "claude-opus-4-8")
            .unwrap_or_else(|| panic!("None 应回落 r.model: {:?}", upstream));
        assert_eq!(opus.requests, 2, "② 与 ③ 都归 opus");

        let requested = s.by_requested_model(&no_pricing());
        let haiku = requested
            .iter()
            .find(|m| m.key == "claude-haiku-4-5")
            .unwrap_or_else(|| panic!("原始名应出现在 by_requested_model: {:?}", requested));
        assert_eq!(haiku.requests, 1);
        let opus_r = requested
            .iter()
            .find(|m| m.key == "claude-opus-4-8")
            .unwrap();
        assert_eq!(opus_r.requests, 2, "② 与 ③（None 回落 model）都归 opus");

        // 分属两表：by_model 里没有原始名、by_requested_model 里没有映射后名。
        assert!(
            upstream.iter().all(|m| m.key != "claude-haiku-4-5"),
            "by_model 不得出现客户端原始名"
        );
        assert!(
            requested.iter().all(|m| m.key != "claude-sonnet-4-5"),
            "by_requested_model 不得出现映射后名"
        );

        // 总量守恒：两维度请求总数都等于总请求数（3）。
        let upstream_total: u64 = upstream.iter().map(|g| g.requests).sum();
        let requested_total: u64 = requested.iter().map(|g| g.requests).sum();
        assert_eq!(upstream_total, 3, "by_model 总量必须守恒");
        assert_eq!(requested_total, 3, "by_requested_model 总量必须守恒");
    }

    /// 双口径下 `by_requested_model` 对**外部可控**字符串同样有界：
    /// 随机模型名塞满 requested 表也归入 OTHER，不会无界增长（复现 #21 教训的
    /// `by_model` 无界缺陷在第二张表上不复发）。
    #[test]
    fn test_by_requested_model_is_bounded() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        // 用 distinct 的原始模型名塞超过 MODEL_KEY_CAP 的记录。
        for i in 0..(Inner::MODEL_KEY_CAP + 50) {
            let mut r = rec(
                i as i64,
                Some(1),
                &format!("mapped-{i}"),
                RequestOutcome::Success,
                1,
                0,
            );
            r.requested_model = Some(format!("client-model-{i}"));
            s.on_record(&r);
        }
        let requested = s.by_requested_model(&no_pricing());
        // 表满后新名归入 OTHER 桶：条目数 ≤ CAP + 1（含 OTHER）。
        assert!(
            requested.len() <= Inner::MODEL_KEY_CAP + 1,
            "by_requested_model 无界增长：{} 条",
            requested.len()
        );
    }

    /// ⭐ 回归（已知问题 #21）：`retries` 必须进聚合层，且两个口径都要对。
    ///
    /// 数据一直是齐的（`handlers.rs` 四处写入点 + provider 失败路径），但
    /// `Aggregate` 既无字段、`add()` 也不读 ⇒ **画不出趋势、算不出分布**，
    /// 只能在逐条详情里一条条翻。而「烧 12 次换号才失败」与「首次即失败」的区分
    /// 正是判断重试预算够不够、吸收层有没有效的唯一依据。
    ///
    /// 删掉 `add()` 里那两行 → 本测试必 FAILED。
    #[test]
    fn aggregate_must_expose_retries_in_both_calibers() {
        let mut agg = Aggregate::default();
        // 10 条：8 条零重试，2 条各重试 6 次。
        for _ in 0..8 {
            let mut r = RequestRecord::new("t".to_string(), "m".to_string());
            r.outcome = RequestOutcome::Success;
            r.retries = 0;
            agg.add(&r);
        }
        for _ in 0..2 {
            let mut r = RequestRecord::new("t".to_string(), "m".to_string());
            r.outcome = RequestOutcome::RateLimited;
            r.retries = 6;
            agg.add(&r);
        }
        assert_eq!(agg.retries_sum, 12, "换号次数必须累计（旧代码恒 0）");
        assert_eq!(agg.retried_requests, 2, "只有真重试过的请求计入分母");

        // 口径①整池放大倍数：12/10 = 1.2
        assert!((agg.avg_retries_per_request() - 1.2).abs() < 1e-9);
        // 口径②真重试时的平均次数：12/2 = 6.0
        //
        // 这两个数**必须都能算出来**：只有口径① 时 1.2 会被读成"几乎不重试"，
        // 而真相是那 2 条各重试了 6 次。这正是加 retried_requests 分母的理由。
        assert_eq!(agg.avg_retries_when_retried(), Some(6.0));

        // merge 必须同样带上两个字段，否则跨桶汇总（逐小时/逐天）会把它们清零 ——
        // 而趋势图正是跨桶汇总出来的，漏了 merge 等于字段只在单桶内有效。
        let mut total = Aggregate::default();
        total.merge(&agg);
        total.merge(&agg);
        assert_eq!(total.retries_sum, 24, "merge 必须累加 retries_sum");
        assert_eq!(total.retried_requests, 4, "merge 必须累加 retried_requests");

        // 无重试样本时返 None 而非 0.0：后者会被误读成"重试过但只重试 0 次"。
        let empty = Aggregate::default();
        assert_eq!(empty.avg_retries_when_retried(), None);
        assert_eq!(empty.avg_retries_per_request(), 0.0);
    }

    /// 构造一条带 `retries` 的记录（聚合层已覆盖，这里专测 DTO 出口）。
    fn rec_retries(offset_ms: i64, cid: Option<u64>, model: &str, retries: u32) -> RequestRecord {
        let mut r = rec(offset_ms, cid, model, RequestOutcome::RateLimited, 10, 5);
        r.retries = retries;
        r
    }

    /// ⭐ 回归（已知问题 #21 的**出口**部分）：三个 DTO 必须真的把 retries 下发出去。
    ///
    /// 聚合层（`Aggregate`）早就在算，但 `WindowSummary` / `SeriesPoint` / `GroupStat`
    /// 三个输出结构一个字段都没有 ⇒ `/api/admin/usage/*` 全都不下发，前端拿不到。
    /// 本测试断言的是**序列化后的 JSON 文本**（而不是 Rust 字段），因为字段存在
    /// 但 serde 改了名（如误加 `rename_all = "camelCase"`）对前端同样等于没有。
    ///
    /// 删掉任一 DTO 里的 `retries_sum:` 那行 → 本测试必 FAILED。
    #[test]
    fn usage_dtos_must_emit_retries_in_snake_case() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        // 3 条同一小时同一模型同一号：2 条各重试 3 次，1 条零重试。
        s.on_record(&rec_retries(0, Some(9), "sonnet", 3));
        s.on_record(&rec_retries(1_000, Some(9), "sonnet", 3));
        s.on_record(&rec_retries(2_000, Some(9), "sonnet", 0));

        // ① WindowSummary：两个原始计数 + 两个口径的平均值都要在
        let ov = s.overview_at(BASE_MS + 2_000);
        assert_eq!(ov.last_24h.retries_sum, 6);
        assert_eq!(ov.last_24h.retried_requests, 2);
        assert!(
            (ov.last_24h.avg_retries_per_request - 2.0).abs() < 1e-9,
            "6/3"
        );
        assert_eq!(ov.last_24h.avg_retries_when_retried, Some(3.0), "6/2");

        let ov_json = serde_json::to_string(&s.overview_at(BASE_MS + 2_000)).unwrap();
        assert!(ov_json.contains("\"retries_sum\":6"), "{ov_json}");
        assert!(ov_json.contains("\"retried_requests\":2"), "{ov_json}");
        assert!(
            ov_json.contains("\"avg_retries_per_request\":2.0"),
            "{ov_json}"
        );
        assert!(
            ov_json.contains("\"avg_retries_when_retried\":3.0"),
            "{ov_json}"
        );
        // camelCase 变体一个都不许出现：前端类型定义按 snake_case 写的，
        // 出现 camelCase 说明 DTO 上被误加了 rename_all（字段在但前端读不到）。
        for camel in [
            "retriesSum",
            "retriedRequests",
            "avgRetriesPerRequest",
            "avgRetriesWhenRetried",
        ] {
            assert!(!ov_json.contains(camel), "出口不得 camelCase：{camel}");
        }

        // ② SeriesPoint：只给原始计数（分母都在同一个点里，口径交给前端）
        let series = s.timeseries_hourly_at(BASE_MS + 2_000, 1);
        assert_eq!(series.last().unwrap().retries_sum, 6);
        assert_eq!(series.last().unwrap().retried_requests, 2);
        let series_json = serde_json::to_string(&series).unwrap();
        assert!(series_json.contains("\"retries_sum\":6"), "{series_json}");
        assert!(
            series_json.contains("\"retried_requests\":2"),
            "{series_json}"
        );
        // 天桶同样要接上（漏一个构造点 = 切到「按天」后趋势整条归零）
        let daily_json = serde_json::to_string(&s.timeseries_daily_at(BASE_MS + 2_000, 1)).unwrap();
        assert!(daily_json.contains("\"retries_sum\":6"), "{daily_json}");

        // ③ GroupStat：按模型 / 按凭据两条路径都走 GroupStat::from，各断言一次
        let models = s.by_model(&no_pricing());
        let m = models.iter().find(|g| g.key == "sonnet").unwrap();
        assert_eq!(m.retries_sum, 6);
        assert_eq!(m.retried_requests, 2);
        assert!((m.avg_retries_per_request - 2.0).abs() < 1e-9);
        let models_json = serde_json::to_string(&models).unwrap();
        assert!(models_json.contains("\"retries_sum\":6"), "{models_json}");
        assert!(
            models_json.contains("\"avg_retries_per_request\":2.0"),
            "{models_json}"
        );

        let creds_json = serde_json::to_string(&s.by_credential()).unwrap();
        assert!(creds_json.contains("\"retries_sum\":6"), "{creds_json}");
        assert!(
            creds_json.contains("\"retried_requests\":2"),
            "{creds_json}"
        );
    }

    /// TTFB 平均值同样只有单测在调 —— 出口接上后 `WindowSummary` 必须带它，
    /// 且**无样本时是 `null` 而不是 0**（0ms 物理不可能，显示 0 比显示 "—" 危险）。
    #[test]
    fn window_summary_must_emit_avg_first_token_ms_as_null_when_no_sample() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        // rec() 不设 first_token_ms ⇒ 该窗口无 TTFB 样本
        s.on_record(&rec(0, Some(3), "m", RequestOutcome::Success, 1, 1));
        let ov = s.overview_at(BASE_MS);
        assert_eq!(ov.last_24h.avg_first_token_ms, None);
        let json = serde_json::to_string(&s.overview_at(BASE_MS)).unwrap();
        assert!(json.contains("\"avg_first_token_ms\":null"), "{json}");

        // 有样本时下发真实平均值
        let s2 = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        let mut r = rec(0, Some(3), "m", RequestOutcome::Success, 1, 1);
        r.first_token_ms = Some(300);
        s2.on_record(&r);
        let mut r2 = rec(1_000, Some(3), "m", RequestOutcome::Success, 1, 1);
        r2.first_token_ms = Some(500);
        s2.on_record(&r2);
        let ov2 = s2.overview_at(BASE_MS + 1_000);
        assert_eq!(ov2.last_24h.avg_first_token_ms, Some(400.0));
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
        s.on_record(&rec_client(
            0,
            Some("w1"),
            Some("1.1.1.1"),
            Some("claude-code"),
        ));
        s.on_record(&rec_client(
            1_000,
            Some("w1"),
            Some("1.1.1.1"),
            Some("claude-code"),
        ));
        s.on_record(&rec_client(
            2_000,
            Some("w2"),
            Some("1.1.1.1"),
            Some("claude-code"),
        ));
        // 客户端 B(2.2.2.2) 一个窗口 1 条
        s.on_record(&rec_client(
            0,
            Some("w3"),
            Some("2.2.2.2"),
            Some("claude-code"),
        ));

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
        s.on_record(&rec_client(
            0,
            Some("w1"),
            Some("1.1.1.1"),
            Some("claude-code"),
        ));
        s.on_record(&rec_client(
            0,
            Some("w2"),
            Some("2.2.2.2"),
            Some("claude-code"),
        ));

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
        s.on_record(&rec_client(
            0,
            Some("old"),
            Some("9.9.9.9"),
            Some("claude-code"),
        ));
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

    /// 构造一条带完整机器画像（session/ip/device/os/browser）的记录
    fn rec_machine(
        offset_ms: i64,
        session: Option<&str>,
        ip: Option<&str>,
        device: Option<&str>,
        os: Option<&str>,
        browser: Option<&str>,
    ) -> RequestRecord {
        let mut r = rec_client(offset_ms, session, ip, device);
        r.client_os = os.map(|s| s.to_string());
        r.client_browser = browser.map(|s| s.to_string());
        r
    }

    #[test]
    fn test_machines_different_ip_no_session_are_separate() {
        // 修正后语义:IP 为主键。不同 IP 且无 session 关联 = 不同机器(即便 Claude Code 画像相同)。
        // 这正是修复"7 个不同 IP 被合并成 1 台"的核心。
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        s.on_record(&rec_machine(
            0,
            Some("w1"),
            Some("203.0.113.23"),
            Some("claude-code"),
            Some("Windows"),
            None,
        ));
        s.on_record(&rec_machine(
            1_000,
            Some("w2"),
            Some("10.0.0.9"),
            Some("claude-code"),
            Some("Windows"),
            None,
        ));

        let machines = s.machines_at(BASE_MS + 1_000);
        assert_eq!(
            machines.len(),
            2,
            "不同 IP 且无 session 关联应是两台机器(画像相同也不合并)"
        );
    }

    #[test]
    fn test_machines_same_ip_is_one_machine() {
        // 同一 IP = 同一台机器(IP 是主键)。IP 相同即便画像不同也归一台,该 IP 见过的画像取首现。
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        s.on_record(&rec_machine(
            0,
            Some("w1"),
            Some("1.1.1.1"),
            Some("claude-code"),
            Some("Windows"),
            None,
        ));
        s.on_record(&rec_machine(
            0,
            Some("w2"),
            Some("1.1.1.1"),
            Some("claude-code"),
            Some("Windows"),
            None,
        ));

        let machines = s.machines_at(BASE_MS);
        assert_eq!(machines.len(), 1, "同一 IP 应是一台机器");
        assert_eq!(machines[0].rpm, 2);
        assert_eq!(machines[0].ips, vec!["1.1.1.1".to_string()]);
    }

    #[test]
    fn test_machines_session_sticky_across_ip_change() {
        // 同一 session 一旦归属某机器，后续该 session 记录即便换 IP、
        // 甚至画像细节缺失，仍归原机器（防止 session 迁移把机器拆开）。
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        // 首条：完整画像，session=w1 归属 claude-code|Windows| 机器
        s.on_record(&rec_machine(
            0,
            Some("w1"),
            Some("1.1.1.1"),
            Some("claude-code"),
            Some("Windows"),
            None,
        ));
        // 同 session 换 IP 且画像字段缺失（os=None）——若按当前画像会派生出不同 key，
        // 但粘滞映射应让它仍归原机器。
        s.on_record(&rec_machine(
            1_000,
            Some("w1"),
            Some("2.2.2.2"),
            Some("claude-code"),
            None,
            None,
        ));

        let machines = s.machines_at(BASE_MS + 1_000);
        assert_eq!(machines.len(), 1, "同 session 换 IP 应仍归同一台机器");
        let m = &machines[0];
        assert_eq!(m.rpm, 2);
        assert_eq!(m.active_sessions, 1);
        assert_eq!(m.ips, vec!["1.1.1.1".to_string(), "2.2.2.2".to_string()]);
    }

    #[test]
    fn test_machines_unknown_no_ip_not_merged_black_hole() {
        // 回归:多个**不同** session 都缺 IP → 各自归 "unknown",但不能因此把它们
        // 后来拿到的真实 IP 全灌进同一个 "unknown" 黑洞(dwgx 实测:4 个天差地别的 IP
        // 被并成一台 unknown)。缺 IP 请求不建立粘滞,后续真实 IP 应各自归位到真实机器。
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        // 两个不同 session,首条都缺 IP → 都落 "unknown",但不粘滞
        s.on_record(&rec_machine(
            0,
            Some("wa"),
            None,
            Some("claude-code"),
            None,
            None,
        ));
        s.on_record(&rec_machine(
            0,
            Some("wb"),
            None,
            Some("claude-code"),
            None,
            None,
        ));
        // 各自后续拿到**不同** IP → 应归位到两台不同真实机器,而非都并进 unknown
        // (用 RFC5737 文档保留段 203.0.113.0/24 / 198.51.100.0/24 作样例)
        s.on_record(&rec_machine(
            1_000,
            Some("wa"),
            Some("203.0.113.13"),
            Some("claude-code"),
            None,
            None,
        ));
        s.on_record(&rec_machine(
            1_000,
            Some("wb"),
            Some("198.51.100.185"),
            Some("claude-code"),
            None,
            None,
        ));

        let machines = s.machines_at(BASE_MS + 1_000);
        // 核心断言:两个不相干的真实 IP 各自独立成机器(黑洞根治)。
        let ip_machines: Vec<_> = machines
            .iter()
            .filter(|m| m.machine_key.parse::<std::net::IpAddr>().is_ok())
            .collect();
        assert_eq!(
            ip_machines.len(),
            2,
            "两个不同真实 IP 应各自成一台机器: {:?}",
            machines
                .iter()
                .map(|m| (&m.machine_key, &m.ips))
                .collect::<Vec<_>>()
        );
        // 关键:没有任何一台机器把两个不相干的公网 IP 混在一起(这正是 dwgx 看到的误并)。
        for m in &machines {
            assert!(
                m.ips.len() <= 1,
                "单台机器不应聚合多个不相干 IP: {} -> {:?}",
                m.machine_key,
                m.ips
            );
        }
    }

    #[test]
    fn test_machines_session_not_double_listed_after_ip_arrives() {
        // dwgx 实测:同一 session 先无 IP(落 device/unknown 组)后来带真实 IP(落 IP 组),
        // 旧组残留没清 → session 同时出现在两台机器下、RPM 双计。这里回归该「单一归属」不变量。
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        // 首条:无 IP → 落 device("claude-code")组
        s.on_record(&rec_machine(
            0,
            Some("s1"),
            None,
            Some("claude-code"),
            None,
            None,
        ));
        // 同 session 后续带真实 IP → 应迁到 IP 组,且从 device 组移除(不再两处都在)
        s.on_record(&rec_machine(
            1_000,
            Some("s1"),
            Some("203.0.113.23"),
            Some("claude-code"),
            None,
            None,
        ));

        let machines = s.machines_at(BASE_MS + 1_000);
        // 统计 s1 出现在几台机器下 —— 必须恰好 1 台
        let appearances: usize = machines
            .iter()
            .filter(|m| m.sessions.iter().any(|w| w.session_id == "s1"))
            .count();
        assert_eq!(
            appearances,
            1,
            "session s1 应只归属一台机器,不能在多台重复出现: {:?}",
            machines
                .iter()
                .map(|m| (
                    &m.machine_key,
                    m.sessions.iter().map(|w| &w.session_id).collect::<Vec<_>>()
                ))
                .collect::<Vec<_>>()
        );
        // 且归属到真实 IP 那台
        let owner = machines
            .iter()
            .find(|m| m.sessions.iter().any(|w| w.session_id == "s1"))
            .unwrap();
        assert_eq!(owner.machine_key, "203.0.113.23", "应归属真实 IP 机器");
    }

    #[test]
    fn test_machines_prune_stale() {
        // 过期机器应被 prune（与 clients 一致），10 分钟后查询为空。
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        s.on_record(&rec_machine(
            0,
            Some("w1"),
            Some("9.9.9.9"),
            Some("claude-code"),
            Some("Windows"),
            None,
        ));
        let later = s.machines_at(BASE_MS + 11 * 60 * 1000);
        assert!(later.is_empty(), "过期机器应被回收");
    }

    #[test]
    fn test_record_cache_tokens_roundtrip_and_default() {
        // 新增 cache 字段应能序列化/反序列化，且旧 JSONL（缺字段）回退 0。
        let mut r = rec(0, Some(1), "m", RequestOutcome::Success, 10, 5);
        r.cache_read_tokens = 128;
        r.cache_creation_tokens = 64;
        let json = serde_json::to_string(&r).unwrap();
        let back: RequestRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cache_read_tokens, 128);
        assert_eq!(back.cache_creation_tokens, 64);

        // 缺字段的历史行：serde default 回退 0，不报错
        let legacy = r#"{"request_id":"x","ts_ms":0,"credential_id":null,"model":"m","is_streaming":false,"input_tokens":1,"output_tokens":1,"credits_used":null,"latency_ms":0,"first_token_ms":null,"outcome":"success","retries":0,"error_message":null,"session_id":null,"client_device":null,"client_ip":null,"client_os":null,"client_browser":null}"#;
        let legacy_rec: RequestRecord = serde_json::from_str(legacy).unwrap();
        assert_eq!(legacy_rec.cache_read_tokens, 0);
        assert_eq!(legacy_rec.cache_creation_tokens, 0);
    }

    #[test]
    fn should_accumulate_cache_tokens_in_all_overview_windows() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        // 同一小时两条：cache_read 12000+3000，cache_creation 500+0
        s.on_record(&rec_cache(0, Some(1), "m", 20_000, 100, 12_000, 500));
        s.on_record(&rec_cache(60_000, Some(1), "m", 5_000, 50, 3_000, 0));

        let ov = s.overview_at(BASE_MS + 60_000);
        for (name, w) in [
            ("last_24h", &ov.last_24h),
            ("last_7d", &ov.last_7d),
            ("last_30d", &ov.last_30d),
            ("all_time", &ov.all_time),
        ] {
            assert_eq!(w.cache_read_tokens, 15_000, "{name} cache_read 累计");
            assert_eq!(w.cache_creation_tokens, 500, "{name} cache_creation 累计");
            // cache 是 gross input 的子集：不得超过 input_tokens
            assert_eq!(w.input_tokens, 25_000, "{name} input 保持 gross 口径");
            assert!(
                w.cache_read_tokens + w.cache_creation_tokens <= w.input_tokens,
                "{name} cache 合计不得超过 gross input"
            );
            // total_tokens 口径不变（仍是 input+output，不因 cache 字段而变）
            assert_eq!(w.total_tokens, 25_150, "{name} total_tokens 口径不变");
        }
    }

    #[test]
    fn should_expose_cache_tokens_in_hourly_and_daily_timeseries() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        s.on_record(&rec_cache(0, Some(1), "m", 1_000, 10, 800, 100));
        s.on_record(&rec_cache(HOUR_MS, Some(1), "m", 2_000, 20, 1_500, 0));
        s.on_record(&rec_cache(DAY_MS, Some(1), "m", 3_000, 30, 2_000, 200));

        let hourly = s.timeseries_hourly_at(BASE_MS + HOUR_MS, 2);
        assert_eq!(hourly[0].cache_read_tokens, 800);
        assert_eq!(hourly[0].cache_creation_tokens, 100);
        assert_eq!(hourly[1].cache_read_tokens, 1_500);
        assert_eq!(hourly[1].cache_creation_tokens, 0);

        let daily = s.timeseries_daily_at(BASE_MS + DAY_MS, 2);
        assert_eq!(daily[0].cache_read_tokens, 2_300, "第一天两条合计");
        assert_eq!(daily[0].cache_creation_tokens, 100);
        assert_eq!(daily[1].cache_read_tokens, 2_000, "第二天一条");
        assert_eq!(daily[1].cache_creation_tokens, 200);
    }

    #[test]
    fn should_expose_cache_tokens_in_by_model_and_by_credential() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        s.on_record(&rec_cache(0, Some(1), "sonnet", 1_000, 10, 900, 50));
        s.on_record(&rec_cache(1_000, Some(1), "sonnet", 1_000, 10, 600, 0));
        s.on_record(&rec_cache(2_000, Some(2), "opus", 500, 5, 400, 20));

        let models = s.by_model(&no_pricing());
        let sonnet = models.iter().find(|m| m.key == "sonnet").unwrap();
        assert_eq!(sonnet.cache_read_tokens, 1_500);
        assert_eq!(sonnet.cache_creation_tokens, 50);
        let opus = models.iter().find(|m| m.key == "opus").unwrap();
        assert_eq!(opus.cache_read_tokens, 400);
        assert_eq!(opus.cache_creation_tokens, 20);

        let creds = s.by_credential();
        let c1 = creds.iter().find(|c| c.key == "1").unwrap();
        assert_eq!(c1.cache_read_tokens, 1_500);
        assert_eq!(c1.cache_creation_tokens, 50);
        let c2 = creds.iter().find(|c| c.key == "2").unwrap();
        assert_eq!(c2.cache_read_tokens, 400);
        assert_eq!(c2.cache_creation_tokens, 20);
    }

    #[test]
    fn should_keep_cache_totals_zero_for_legacy_records_without_cache() {
        // 旧数据（cache 字段缺失 → serde default 0）灌进来后，各出口 cache 累计必须恒为 0，
        // 且原有 requests/tokens 统计不受新字段影响。
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        s.on_record(&rec(0, Some(1), "m", RequestOutcome::Success, 10, 5));
        s.on_record(&rec(1_000, Some(1), "m", RequestOutcome::RateLimited, 0, 0));

        let ov = s.overview_at(BASE_MS + 1_000);
        assert_eq!(ov.last_24h.requests, 2);
        assert_eq!(ov.last_24h.input_tokens, 10);
        assert_eq!(ov.last_24h.cache_read_tokens, 0);
        assert_eq!(ov.last_24h.cache_creation_tokens, 0);
        let point = s.timeseries_hourly_at(BASE_MS, 1);
        assert_eq!(point[0].cache_read_tokens, 0);
        assert_eq!(point[0].cache_creation_tokens, 0);
        assert_eq!(s.by_model(&no_pricing())[0].cache_read_tokens, 0);
        assert_eq!(s.by_credential()[0].cache_creation_tokens, 0);
    }

    #[test]
    fn should_reset_cache_totals_when_ring_bucket_is_reused() {
        // 环形桶被新时间段复用时 cache 累计必须跟着清零，不能残留上一圈的数据
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        s.on_record(&rec_cache(0, Some(1), "m", 10_000, 10, 9_000, 500));
        let ring_span = HOUR_BUCKETS as i64 * HOUR_MS;
        s.on_record(&rec_cache(ring_span, Some(1), "m", 100, 1, 70, 0));

        let series = s.timeseries_hourly_at(BASE_MS + ring_span, 1);
        assert_eq!(series[0].cache_read_tokens, 70, "旧圈 cache 应已被覆盖");
        assert_eq!(series[0].cache_creation_tokens, 0);
    }

    #[test]
    fn should_restore_cache_totals_after_rebuild_from_jsonl() {
        let dir = std::env::temp_dir().join(format!(
            "kiro_us_cache_rebuild_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        {
            let s = UsageStats::new(dir.clone());
            s.on_record(&rec_cache(0, Some(1), "m1", 1_000, 10, 800, 100));
            s.on_record(&rec_cache(1_000, Some(1), "m1", 2_000, 20, 1_200, 0));
        }
        let s2 = UsageStats::new(dir.clone());
        s2.rebuild_from_logs();
        let ov = s2.overview_at(BASE_MS + 1_000);
        assert_eq!(ov.last_24h.cache_read_tokens, 2_000);
        assert_eq!(ov.last_24h.cache_creation_tokens, 100);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn should_serialize_cache_fields_in_snake_case() {
        // 聚合出口沿用 snake_case（无 rename_all），前端按此字段名对接
        let s = UsageStats::new(std::env::temp_dir().join("kiro_us_test_ignore"));
        s.on_record(&rec_cache(0, Some(1), "m", 1_000, 10, 800, 100));
        let ov = serde_json::to_string(&s.overview_at(BASE_MS)).unwrap();
        assert!(ov.contains("\"cache_read_tokens\":800"), "{ov}");
        assert!(ov.contains("\"cache_creation_tokens\":100"), "{ov}");
        let series = serde_json::to_string(&s.timeseries_hourly_at(BASE_MS, 1)).unwrap();
        assert!(series.contains("\"cache_read_tokens\":800"), "{series}");
        let models = serde_json::to_string(&s.by_model(&no_pricing())).unwrap();
        assert!(models.contains("\"cache_creation_tokens\":100"), "{models}");
        let creds = serde_json::to_string(&s.by_credential()).unwrap();
        assert!(creds.contains("\"cache_read_tokens\":800"), "{creds}");
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
        let models = s2.by_model(&no_pricing());
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
        let good =
            serde_json::to_string(&rec(0, Some(1), "m", RequestOutcome::Success, 1, 1)).unwrap();
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
        assert!(serde_json::to_string(&s.by_model(&no_pricing())).is_ok());
        assert!(serde_json::to_string(&s.by_credential()).is_ok());
        assert!(serde_json::to_string(&s.throughput_at(BASE_MS)).is_ok());
        assert!(serde_json::to_string(&s.clients_at(BASE_MS)).is_ok());
        assert!(serde_json::to_string(&s.machines_at(BASE_MS)).is_ok());
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
        assert!(
            later
                .recent_buckets
                .iter()
                .all(|b| b.requests == 0 && b.tokens == 0)
        );

        // 相隔恰好一整圈（60 秒）落入同一桶但 slot 不同 → 清零覆盖，不叠加旧值
        let ring_span = THROUGHPUT_BUCKETS as i64 * THROUGHPUT_BUCKET_SECS * 1000;
        s.on_record(&rec(ring_span, Some(1), "m", RequestOutcome::Success, 7, 0));
        let snap = s.throughput_at(BASE_MS + ring_span);
        assert_eq!(snap.current_rpm, 1, "只应看到新记录");
        let last = snap.recent_buckets.last().unwrap();
        assert_eq!(last.tokens, 7);
    }

    #[test]
    fn test_machine_code_derivation_stable_and_format() {
        // 机器码格式：MC- + 12 位十六进制，稳定可复制。
        let code = machine_code_of(Some("203.0.113.23"), Some("claude-code"));
        assert!(code.starts_with("MC-"), "机器码应以 MC- 开头: {code}");
        assert_eq!(code.len(), 15, "机器码应为 MC- + 12 位: {code}");
        assert!(
            code[3..].chars().all(|c| c.is_ascii_hexdigit()),
            "MC- 之后应全为十六进制: {code}"
        );

        // 确定性：同输入永远同码。
        assert_eq!(
            code,
            machine_code_of(Some("203.0.113.23"), Some("claude-code"))
        );

        // IP 优先：有 IP 时 device 不影响码（machine_key = IP）。
        assert_eq!(
            machine_code_of(Some("203.0.113.23"), Some("claude-code")),
            machine_code_of(Some("203.0.113.23"), Some("vscode")),
            "有 IP 时机器码只由 IP 决定"
        );

        // 无 IP 回退 device；都无回退 unknown，三者互不相同。
        let by_device = machine_code_of(None, Some("claude-code"));
        let by_unknown = machine_code_of(None, None);
        assert_ne!(code, by_device);
        assert_ne!(by_device, by_unknown);
        assert_eq!(by_unknown, machine_code("unknown"));

        // machines_at 填充的 machine_code 与独立派生一致（展示=拦截同一真相源）。
        let s = UsageStats::new(std::env::temp_dir().join(format!(
            "kiro_mc_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        )));
        s.on_record(&rec_machine(
            1_000,
            Some("w1"),
            Some("203.0.113.23"),
            Some("claude-code"),
            None,
            None,
        ));
        let machines = s.machines_at(BASE_MS + 1_000);
        let m = machines
            .iter()
            .find(|m| m.machine_key == "203.0.113.23")
            .unwrap();
        assert_eq!(m.machine_code, machine_code("203.0.113.23"));
    }

    /// TTFB 平均必须只按**有值样本**平均，且无样本时返回 None 而非 0。
    ///
    /// 为什么单独测：`first_token_ms` 是 Option（非流式/纯错误/无内容都是 None）。
    /// 若用 `requests` 当分母，那些 None 会被当 0ms 摊进去 → 平均值系统性偏低；
    /// 而返回 0 会让面板显示"0ms TTFB"（物理不可能，却看起来像"快到测不出"）。
    #[test]
    fn aggregate_averages_ttfb_over_valued_samples_only() {
        let mut agg = Aggregate::default();
        assert_eq!(agg.avg_first_token_ms(), None, "无样本应为 None，不是 0");

        let mut r1 = RequestRecord::new("a", "m");
        r1.latency_ms = 1000;
        r1.first_token_ms = Some(200);
        agg.add(&r1);

        // 一条没有 TTFB 的记录（模拟非流式）：不得进分母
        let mut r2 = RequestRecord::new("b", "m");
        r2.latency_ms = 3000;
        r2.first_token_ms = None;
        agg.add(&r2);

        assert_eq!(agg.requests, 2);
        assert_eq!(
            agg.avg_first_token_ms(),
            Some(200.0),
            "只有 1 条有 TTFB，平均应是 200 而非 100（用 requests 当分母就会得到 100）"
        );
    }

    #[test]
    fn test_machine_ip_codes_cover_every_roaming_ip() {
        // F1 回归:同一 session 漫游多 IP 会被粘滞合并成一台机器(主键=首个真实 IP)。
        // 主键 machine_code 只覆盖粘滞 IP,但入口拦截按**当前请求 IP** 重算——若只暴露主键码,
        // 客户端换到第二个 IP 就绕过封禁。修复=ip_codes 对每个见过的 IP 各给一个码,
        // 每个码 == 入口按该 IP 重算的码,逐个可封,无绕过缺口。
        let s = UsageStats::new(std::env::temp_dir().join(format!(
            "kiro_mc_roam_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        )));
        // 同一 session "roam" 先后用两个 IP(DHCP/VPN 漫游)→ 合并为一台机器,ips 收两个。
        s.on_record(&rec_machine(
            0,
            Some("roam"),
            Some("203.0.113.13"),
            Some("claude-code"),
            None,
            None,
        ));
        s.on_record(&rec_machine(
            1_000,
            Some("roam"),
            Some("203.0.113.99"),
            Some("claude-code"),
            None,
            None,
        ));

        let machines = s.machines_at(BASE_MS + 1_000);
        // 该机器(粘滞主键=首个真实 IP 203.0.113.13)应收录两个漫游 IP。
        let m = machines
            .iter()
            .find(|m| m.machine_key == "203.0.113.13")
            .expect("漫游应合并到首个真实 IP 机器");
        assert!(m.ips.contains(&"203.0.113.13".to_string()));
        assert!(
            m.ips.contains(&"203.0.113.99".to_string()),
            "第二个漫游 IP 应被收录: {:?}",
            m.ips
        );

        // 关键:ip_codes 覆盖每个见过的 IP,且每个码 == 入口按该 IP 重算的码。
        for ip in &m.ips {
            let entry = m
                .ip_codes
                .iter()
                .find(|c| &c.ip == ip)
                .unwrap_or_else(|| panic!("ip_codes 应覆盖每个见过的 IP,缺 {ip}"));
            assert_eq!(
                entry.code,
                machine_code_of(Some(ip), None),
                "IP {ip} 的展示码必须 == 入口按当前 IP 重算的码(否则封禁绕过)"
            );
        }
        // 第二个漫游 IP 的码 ≠ 主键码(否则会误以为拉黑主键就够)。
        let second_code = m.ip_codes.iter().find(|c| c.ip == "203.0.113.99").unwrap();
        assert_ne!(
            second_code.code, m.machine_code,
            "漫游第二 IP 的码应独立于主键码"
        );
    }
    /// 回归：`by_model` 对**外部可控**的模型名必须有界。
    ///
    /// **旧代码为何 FAIL**：`by_model` 的 key 直接用 `r.model`，而这张表**永不回收**
    /// （`ClientAgg::prune` 只清 by_session/by_client/by_machine，全仓无 retain 也无上限）。
    /// 实测 500 个随机 model 名 → **500 个永久条目**。
    ///
    /// 可控性链路：custom_api 透传在 `should_try_custom_api_first()` 为真时**先于**
    /// `convert_request` 执行，其 record 用的是客户端 JSON 里的**原始** `payload.model`，
    /// 从未过 `map_model` 校验（Kiro 主路径用映射后的 kiro id，受控）。
    /// 于是持有效 key 的客户端可用随机 model 名把内存推到无界；
    /// 且 `rebuild_from_logs` 冷启动会把 30 天 JSONL 里的脏 key **重放回内存**，
    /// `GET /api/admin/usage/models` 还会把整张表序列化返回。
    #[test]
    fn by_model_is_bounded_against_arbitrary_model_names() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_bymodel_bounded"));
        // 远超上限的不同模型名
        for i in 0..(Inner::MODEL_KEY_CAP * 2) {
            s.on_record(&rec(
                i as i64 * 10,
                Some(1),
                &format!("junk-model-{i}"),
                RequestOutcome::Success,
                1,
                1,
            ));
        }
        let models = s.by_model(&no_pricing());
        assert!(
            models.len() <= Inner::MODEL_KEY_CAP + 1,
            "by_model 无界增长：{} 个条目（上限 {} + OTHER 桶）",
            models.len(),
            Inner::MODEL_KEY_CAP
        );
        // 超限的都归入 OTHER 桶，总量守恒（面板的模型分布仍要对上总请求数）
        let total: u64 = models.iter().map(|m| m.requests).sum();
        assert_eq!(
            total,
            (Inner::MODEL_KEY_CAP * 2) as u64,
            "归并不得丢请求数：模型分布之和必须等于总请求数"
        );
        assert!(
            models.iter().any(|m| m.key == Inner::MODEL_KEY_OTHER),
            "超限的模型名应归入 {} 桶而非被丢弃",
            Inner::MODEL_KEY_OTHER
        );
    }

    /// 回归：超长模型名必须被截断（阻断"用超长字符串放大单条内存"）。
    #[test]
    fn by_model_truncates_overlong_names() {
        let s = UsageStats::new(std::env::temp_dir().join("kiro_bymodel_trunc"));
        let huge = "x".repeat(10_000);
        s.on_record(&rec(0, Some(1), &huge, RequestOutcome::Success, 1, 1));
        let models = s.by_model(&no_pricing());
        assert_eq!(models.len(), 1);
        assert!(
            models[0].key.chars().count() <= Inner::MODEL_KEY_MAX_LEN,
            "模型名未截断：{} 字符",
            models[0].key.chars().count()
        );
    }
}
