//! Kiro API Provider
//!
//! 核心组件，负责与 Kiro API 通信
//! 支持流式和非流式请求
//! 支持多凭据故障转移和重试
//! 支持按凭据级 endpoint 切换不同 Kiro API 端点

use reqwest::Client;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::http_client::{ProxyConfig, build_streaming_client};
use crate::kiro::cooldown::CooldownReason;
use crate::kiro::endpoint::{ENDPOINT_FALLBACK_ORDER, KiroEndpoint, RequestContext};
use crate::kiro::endpoint_health::EndpointHealth;
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::TlsBackend;
use parking_lot::Mutex;

#[path = "retry_budget.rs"]
mod retry_budget;
pub use retry_budget::SharedRetryBudget;
use retry_budget::{round_retry_quota, compute_max_retries, ABSOLUTE_MAX_TOTAL_RETRIES};

#[path = "absorb_policy.rs"]
mod absorb_policy;
use absorb_policy::{AbsorbPolicy, should_start_another_round};

/// 🔴 **透传（custom_api）路径单请求的最大换号次数**。
///
/// # 为什么必须有（2026-08-10 审计发现的致命缺口）
///
/// Kiro 主路径有**五道**背压：准入闸门（`:1876`）、全局并发闸（`:2146`）、每凭据并发闸
/// （`:2189`）、重试上限 [`ABSOLUTE_MAX_TOTAL_RETRIES`]（跨吸收轮共享）、动态压力降档
/// （`apply_retry_pressure`）。而透传循环（`try_custom_api_passthrough`）**一道都没有**
/// —— 它是按「低延迟零转换中转」设计的，主路径后来加的调度设施它一项都没跟上。
///
/// 后果（每一环都已核实）：单请求可打 N 次上游（N = 代挂号数，无次数上限），每次
/// `connect_timeout` 10s + `read_timeout` 720s；45s 墙钟**只在每轮进循环时**判
/// （见循环顶部），故最后一跳可以在 45s 之后才开始、并持续到 720s 空闲超时。
/// 叠上外置 shield-k2cc 的 10 次重试 ⇒ **无上限并发 × 无上限次数**。
///
/// 而线上号池当前**全部是 custom_api 代挂号**（无 ksk_ Kiro 号）⇒ **100% 的流量走的
/// 正是这条零背压路径**，主路径那五道闸对当前流量全部失效。
///
/// # 为什么取 6 而不是复用 4
///
/// [`ABSOLUTE_MAX_TOTAL_RETRIES`]=4 是给 Kiro 主路径定的：那里换号意味着换 Kiro 账号，
/// 打太多次会在账号间连环撞风控。透传换号换的是**用户自购的付费中转站**，它们互相独立
/// 且指向不同上游（实测 5 个代挂号指向 5 个不同站点），换号不存在风控连坐，
/// 且「换个站点就成功」是实测常态（`deepseek-v4-flash` 在 1418 返 404 而 1305 返 200）。
/// 所以上限要**略大于典型池规模**以保证能试完全池，但仍是有限的 —— 6 覆盖了实测的
/// 最大池规模（6 个代挂号），同时把最坏放大从「无上限」压到常数级。
const MAX_PASSTHROUGH_FAILOVER_HOPS: usize = 6;

/// 上游压力率（429+5xx）滑动窗口的时长（秒）。
///
/// 窗口内每响应喂一次压力布尔，`rate()` 返回近期压力占比，供
/// [`apply_retry_pressure`] 动态降档。60s 对齐 throttle 的观察窗口径，既不反应过
/// 快的瞬时抖动（去抖交给 AIMD 的 3s 窗口），也不至于滞后到跟不上风控节奏。
const PRESSURE_WINDOW_SECS: u64 = 60;

/// 单个入站请求的重试墙钟预算（秒）。
///
/// ⚠️ 关键防雪崩闸门：小号池下，一个卡住的请求会在每次重试时抢到刚出冷却的号、
/// 又打 429、又把它冷却，如此在 acquire_context 的等待循环（最长 180s）× 多次
/// 重试之间反复横跳，一个请求就能把整池长时间压死（表现为「没有新入站却一直 429
/// / 繁忙」）。这里给单请求一个总时长上限：超时就停止重试、把最后的错误（通常是
/// 429）透传给客户端，让客户端自己退避，而不是继续拖垮整池。取值需覆盖一次正常
/// 大请求的排队+响应，又不至于长到能扫冷全池。
const MAX_REQUEST_RETRY_BUDGET_SECS: u64 = 45;

/// 解析上游 `Retry-After` 头：整数秒，或 RFC 7231 HTTP-date（IMF-fixdate）。
/// HTTP-date 相对现在的剩余秒数；已过期 → 0。解析失败 → None。
pub(crate) fn parse_retry_after_header_value(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<u64>() {
        return Some(n);
    }
    let dt = chrono::DateTime::parse_from_rfc2822(s).ok()?;
    let secs = dt
        .with_timezone(&chrono::Utc)
        .signed_duration_since(chrono::Utc::now())
        .num_seconds();
    Some(secs.max(0) as u64)
}

/// MCP 路径（WebSearch 等工具调用）流式 client 的 read_timeout（空闲间隔秒）。
///
/// 单一源头：`client_for` 构造 MCP client 与 MCP 墙钟推导都从这里取，
/// 改任一个另一个自动跟随（透传墙钟同款范式，见 `try_custom_api_passthrough`
/// 里 `FIRST_BYTE_TIMEOUT_SECS` 的取舍注释 —— 「改了一个忘了另一个」已踩过）。
const MCP_CLIENT_READ_TIMEOUT_SECS: u64 = 720;

/// MCP 单请求重试的墙钟预算（秒），**不能复用主路径 45s**。
///
/// 与透传墙钟（`PASSTHROUGH_WALL_SECS`）同形教训：MCP 用 720s read_timeout 的
/// 流式 client，单次 send() 可**合法**超过 45s（connect 10s + 慢响应）。45s 墙钟下
/// 「首跳 60s 失败 ⇒ elapsed 已过预算 ⇒ 第二个号一次都不会试」—— 换号能力被静默
/// 废掉，而"多号互为备份"正是 failover 循环存在的理由。所以墙钟必须容纳
/// 「至少一次完整的单次尝试 + 一次换号后的再次尝试」：取 read_timeout × 2 + 30s
/// 余量（与透传 `FIRST_BYTE_TIMEOUT_SECS * 2 + 30` 同构）。
///
/// ⚠️ 墙钟这么宽不会失控：本循环的实际约束是次数闸 —— max_retries 被共享预算
/// 剩余（`budget.remaining()`）夹住，墙钟只是「次数闸失效时」的兜底。
const MCP_WALL_SECS: u64 = MCP_CLIENT_READ_TIMEOUT_SECS * 2 + 30;

/// `call_mcp_with_retry` 因「选不到号」失败时给错误打的标记（`context` 前缀）。
///
/// `call_mcp` 入口据此识别「无号池」错误并触发 [`Self::call_mcp_direct`] 直连兜底，
/// 返回给上层前剥掉标记 —— 客户端只见原错误，不见内部标记。与
/// `shared_budget_exhausted=1`（handlers 层据此渲染 503）同款「错误串带内部标记」
/// 模式。
const MCP_POOL_UNAVAILABLE_MARKER: &str = "mcp_pool_unavailable=1";

/// MCP「无号直连」总开关（默认开，见 [`Self::call_mcp_direct`] 的文档）。
///
/// 进程级 AtomicBool 而非配置项：这是「上游是否接受无 ARN MCP 调用」的**实测前
/// 开关**——上线后若发现上游对直连形态拒绝（403/400），关掉即整体退回旧行为，
/// 不用重新发布。测试可关闭验证降级路径。
pub(crate) static MCP_DIRECT_BYPASS_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// 端点桶（同一 host 的限流桶）被 429 封禁的时长。对齐 kiro2cc `BUCKET_THROTTLE_DURATION`。
///
/// 桶 = (credential_id, endpoint_name)。同凭据另一端点（另一 host = 上游另一限流桶）不受影响，
/// 可继续用。到期自动解除（惰性清理在 `select_endpoint` 访问时顺带做；`has_unthrottled_endpoint`
/// 只读不清理，键数 = 号数 × 端点数，无无界增长风险）。
const ENDPOINT_BUCKET_THROTTLE: Duration = Duration::from_secs(30);

/// 死端点负缓存 TTL（5 分钟）。连接层失败通常表示 DNS 不存在（如 codewhisperer.eu-central-1）
/// 或 host 路由黑洞，但配置/网络可能临时修复，过期后自动重试。
///
/// 2026-08-16 从 1800s 收紧到 300s（m1）：一次瞬时抖动让健康端点 30 分钟零流量
/// 的代价太大（恢复探测太慢），5 分钟足够挡 DNS/路由黑洞，抖动自愈更快。
const DEAD_ENDPOINT_TTL: Duration = Duration::from_secs(300);

/// MCP 直连失败短负缓存 TTL（60s，M3）。
///
/// 远短于 [`DEAD_ENDPOINT_TTL`]（300s）：连接层失败是 DNS/路由黑洞（代价是每个
/// 请求的 connect timeout），而直连失败是 token 级问题（401/403/429），失败后
/// 短暂跳过即可，避免每个请求都再白打一跳死 token——60s 足够挡惩罚窗口，恢复
/// 探测更快。
const MCP_DIRECT_NEG_CACHE_TTL: Duration = Duration::from_secs(60);

/// 协议不符隔离 TTL（30 分钟）。
///
/// 上游对某 (端点, region) 返回的不是 event-stream 而是 JSON/文本（协议降级），
/// 说明这条**路由**当前不可用于对话。与 `dead_endpoints` 同为自动过期的软隔离：
/// 上游修好、或部署方改了配置后，过期即自动重试，无需人工介入也无需重启。
const PROTOCOL_BROKEN_TTL: Duration = Duration::from_secs(1800);

/// 对话路径 403 → **换区重试**的目标 region（L1）。`None` = 不该换区。
///
/// # 为什么对话路径需要这一层
///
/// `ksk_` API Key 是**按 region 授权**的：打错区时上游恒返 403
/// `bearer token included in the request is invalid`。而这个信号在对话路径上
/// 原先被当「凭据问题」→ 冷却 + 换号，**换号解决不了**（同一个号换个区就行）。
/// 导入时的探测可能探错（`region_probe` 那条 400 判 `Usable` 的判据已被实测证否），
/// 于是一个实际授权在 us-east-1 的号会被写死 `eu-central-1` → 该号**恒 403、永久废掉**。
///
/// # 判据为什么必须窄
///
/// `has_ever_succeeded` 这个二分是承重的，它把同一句上游文案劈成语义相反的两类：
/// - **已成功过** ⇒ 区是对的（它在这个区真拿到过 200），403 只能是瞬态抖动
///   （实测 4 个号累计 3393 次成功、共吃 42 次这种 403）→ 交给既有
///   `bearer_invalid_but_proven` 分支（冷却 + 换号、不计失败），本函数返 `None`。
/// - **从未成功过** ⇒ 才**可能**是 region 错配（实测 3 个从未成功的号共吃 17 次）。
///
/// 两者若混在一起：给已证明健康的号换区 = 把一个本来对的配置改坏，而那个号下一次
/// 抖动过去就好了。所以宁可漏修（号从未成功过但其实是别的原因），不可误改。
///
/// # 候选只有两个（实测依据）
///
/// `management.*` 与 `runtime.*` 只在 `us-east-1` / `eu-central-1` 解析 DNS，
/// 即 [`crate::kiro::region_probe::PROBE_ORDER`] 的两项。所以「换区」= 换到**另一个**
/// 那个；当前区不在表内（如 profileArn 把区钉在 `us-west-2`）则换到表首项。
///
/// # 只对 `api_key` 号
///
/// OAuth 号的权威 region 是 `profileArn` 第 4 段（`effective_upstream_region` 第一优先），
/// `api_region` 对它**根本不生效** ⇒ 换区既不改变实际请求的 host、也无从回写，
/// 只会白烧一次重试额度。
fn region_retry_target(
    current_region: &str,
    is_api_key: bool,
    has_ever_succeeded: bool,
) -> Option<&'static str> {
    if !is_api_key || has_ever_succeeded {
        return None;
    }
    let order = crate::kiro::region_probe::PROBE_ORDER;
    // 当前区在表内 ⇒ 取下一项（两项表即「换到另一个」）；不在表内 ⇒ 取首项。
    // 用取模而非硬编码 `[1]`/`[0]`：表若将来扩项，这里退化成「顺序轮换」而不是
    // 永远只在前两项之间跳（那种失败会静默）。
    let next = match order.iter().position(|r| *r == current_region) {
        Some(i) => order[(i + 1) % order.len()],
        None => *order.first()?,
    };
    // 表只有一项时上面的取模会算回自己 —— 换到同一个区是纯浪费一次重试额度。
    if next == current_region {
        return None;
    }
    Some(next)
}

/// 近期上游压力滑动窗口。
///
/// 每次上游响应喂一个布尔（成功/4xx false，429/5xx true），窗口保留近
/// [`PRESSURE_WINDOW_SECS`] 秒。`rate()` 返回窗口内**压力占比**（429+5xx 占全部），
/// 供 [`apply_retry_pressure`] 动态降重试预算。
///
/// ⚠️ 5xx 也计入压力：纯 500 风暴同样是「疯狂重试」来源，只计 429 会让降档永不触发。
///
/// 热路径取舍：短临界区（一次 push + 逐出），锁竞争可接受 —— 即使内部 1000 RPM，
/// 每秒也才 17 次写，远低于锁的吞吐上限。
struct RetryPressureWindow {
    deque: std::collections::VecDeque<(std::time::Instant, bool)>,
    window: std::time::Duration,
}

impl RetryPressureWindow {
    fn new(window_secs: u64) -> Self {
        Self {
            deque: std::collections::VecDeque::new(),
            window: std::time::Duration::from_secs(window_secs),
        }
    }

    /// 记录一次上游响应结果。顺带惰性逐出超窗事件（不额外起定时器）。
    fn record(&mut self, is_pressure: bool) {
        let now = std::time::Instant::now();
        self.deque.push_back((now, is_pressure));
        self.prune(now);
    }

    /// 逐出超过窗口的事件（记录与读取共用，避免 rate() 读到空闲前的陈旧高压）。
    fn prune(&mut self, now: std::time::Instant) {
        while let Some(&(t, _)) = self.deque.front() {
            if now.duration_since(t) > self.window {
                self.deque.pop_front();
            } else {
                break;
            }
        }
    }

    /// 窗口内压力占比（0.0..=1.0）。空窗口返 0（无信号 = 不降档）。
    fn rate(&mut self) -> f32 {
        self.prune(std::time::Instant::now());
        let total = self.deque.len();
        if total == 0 {
            return 0.0;
        }
        let n_pressure = self.deque.iter().filter(|(_, is_pressure)| *is_pressure).count();
        n_pressure as f32 / total as f32
    }
}

/// 按近期上游压力率（429+5xx）动态降档重试预算。
///
/// 疯狂重试（号多 + 429/5xx 多）时每个请求顺着号池一路扫过去纯属放大受害面 ——
/// 重试再多也换不到好号（大家都在被限流/过载），不如降档让客户端更快拿到错误自己退避。
/// 阶梯（整数除法，以当前上限 4 为例）：
/// - 压力率 > 50%：预算 × 33/100（4 → 1）
/// - 压力率 > 30%：预算 × 1/2（4 → 2）
/// - 否则：不变
///
/// 只在 `base_retry_quota`（循环外一次计算）处乘系数，`round_retry_quota` 的
/// `min(剩余总额)` 语义天然把降档收进每请求预算，跨吸收轮不叠加。
fn apply_retry_pressure(base: usize, rate: f32) -> usize {
    let scaled = if rate > 0.5 {
        base * 33 / 100
    } else if rate > 0.3 {
        base / 2
    } else {
        base
    };
    scaled.max(1)
}

/// 一次成功调用的元数据（随响应回传给上层，供用量统计埋点关联）
///
/// provider 层掌握凭据/重试/延迟，但看不到最终 usage/credits（流式消费后才知道）；
/// 上层拿到本结构后与 `StreamContext::resolved_usage()` 合并即可产出完整记录。
pub struct CallMeta {
    /// 实际服务该请求的凭据 ID
    pub credential_id: u64,
    /// 请求模型名 = 客户端**原始**名（调用方传入；未提供时回落请求体解析名，可能为 None）
    pub model: Option<String>,
    /// **映射后的模型名**（全局模型映射 `config.model_mapping` 命中且改写时非 None）。
    ///
    /// `model` 恒为客户端**原始**名（供 `requested_model` 口径），本字段携带改写结果
    /// （供 `upstream_model` 口径）；两者在 handler 埋点时分头写入 `RequestRecord`。
    /// 未命中映射 / 凭据豁免为 None；overload_fallback_model 路径记 fallback 名
    /// （显式跳过全局映射表，见 `call_api_with_retry` 末尾）。
    pub mapped_model: Option<String>,
    /// 会话标识（conversationId）
    pub session_id: Option<String>,
    /// 是否流式
    pub is_streaming: bool,
    /// 本次成功前经历的重试次数（0 表示首次即成功）
    pub retries: u32,
    /// 从进入调用到拿到成功响应头的耗时（毫秒）
    pub latency_ms: u64,
    /// 进入本次调用的时刻，与 [`Self::latency_ms`] **同源同起点**。
    ///
    /// 存在理由：`first_token_ms`（TTFB）此前全仓 0 个生产赋值点、线上 24h 全 NULL，
    /// 导致所有延迟分析失效。而首个内容 delta 是在 handler/stream 层才产生的，
    /// 那里拿不到 provider 的计时起点 —— 不导出这个 Instant 就只能用「响应头到首 token」，
    /// 与 `latency_ms` 不同起点、无法相减也无法比较。
    ///
    /// ⚠️ 起点在准入闸门（令牌桶排队）**之前**，故 `first_token_ms` 含入站排队时长；
    /// 想要纯上游生成延迟用 `first_token_ms - latency_ms`（两者同源，差值即
    /// 「响应头 → 首 token」）。failover 重试时不重置，故也含失败尝试耗时，
    /// 需要时按 `record.retries` 过滤。
    pub started_at: std::time::Instant,
    /// 在途请求守卫：随本 meta（进而随响应流）存活，直到 SSE 流被下游完全消费、
    /// 或客户端断开、或非流式响应读毕后才 Drop → 该凭据 inflight -1。
    /// 因此 inflight 反映"真正还在处理中"的请求数，而非"已拿到响应头"的数。
    ///
    /// 不参与 `Debug`（`InflightGuard` 无 Debug）；`CallMeta` 因此不再派生 `Debug`/`Clone`。
    ///
    /// 仅为 RAII 而持有、从不读取：其唯一作用是在 `CallMeta`（进而响应流）析构时
    /// 触发 `Drop` 把 inflight -1，故 `#[allow(dead_code)]` 而非移除。
    #[allow(dead_code)]
    pub inflight: crate::kiro::scheduling::InflightGuard,
}

/// 一次自定义 API 透传的元数据,供 handler 做 usage 埋点。
///
/// 透传路径不进 Kiro 解码器、拿不到真实 token/credit(隔离铁律 3),故只带调度维度信息;
/// token 由 handler 侧估算,credits 恒 None。与 [`CallMeta`] 分离,避免复用 Kiro 的 inflight/重试语义。
pub struct PassthroughMeta {
    /// 服务该请求的自定义 API 凭据 ID
    pub credential_id: u64,
    /// **本次透传 failover 链最先尝试的凭据 ID**（`None` = 首跳即成功/未发生换号）。
    ///
    /// 与 [`Self::credential_id`]（最终服务号）成对后，handlers 层的 usage record 能暴露
    /// 「死号恒选」：`first_attempted_credential_id` 恒为某号而 `credential_id` 恒为另一号
    /// 时，说明该号每次都被选中最前却被换掉（上游持续失败），需要运维处理。
    /// 记录点见 `try_custom_api_passthrough` 循环内 `note_first_attempt`。
    pub first_attempted_credential_id: Option<u64>,
    /// 请求模型名(原样,透传不映射)
    pub model: Option<String>,
    /// **映射后的模型名**（全局模型映射 `config.model_mapping` 命中且改写时非 None）。
    /// `model` 恒为客户端**原始**名（`requested_model` 口径），本字段携带改写结果
    /// （`upstream_model` 口径）。未命中映射 / 凭据豁免时 None。
    pub mapped_model: Option<String>,
    /// 会话标识
    pub session_id: Option<String>,
    /// 据上游 status 推断的用量结果分类
    pub outcome: crate::usage::RequestOutcome,
    /// 从选号到拿到上游响应头的耗时(毫秒)
    pub latency_ms: u64,
    /// 🔴 上游非 2xx 时的**错误原文**（成功恒 `None`）。
    ///
    /// 为什么必须有：改前透传失败的 trace 里 `error_message` 恒为空，
    /// 于是「上游到底说了什么」完全不可见 —— 实测 1439 的 `outcome=bad_request`
    /// `latency_ms=208`（上游真回了 400）却查不到任何原因，根因排查全靠猜。
    /// 现在把上游 body 原文带上来，面板与 trace 都能看见
    /// （如 `messages[1].role must be user or assistant` / `INVALID_MODEL_ID`）。
    pub upstream_error: Option<String>,
    /// 在途请求守卫（2026-08-10 补）：随本 meta（进而随响应流）存活，直到流被下游完全
    /// 消费 / 客户端断开 / 非流式响应读毕后才 Drop → 该凭据 inflight -1。
    ///
    /// # 为什么必须有
    /// `select_custom_api` 的排序键第三项读 `e.inflight`，但改前透传路径**从不占位**
    /// （`InflightGuard` 只由 Kiro 的 `commit_selection` 产出）⇒ 代挂号 inflight 恒为 0
    /// ⇒ 该维度结构性失效，同优先级同 RPM 时 `min_by_key` 平局恒取第一个号。
    ///
    /// 与 [`CallMeta::inflight`] 同款语义：仅为 RAII 而持有、从不读取，故 `#[allow(dead_code)]`
    /// 而非移除。`InflightGuard` 无 `Debug`，所以本结构不派生 `Debug`。
    #[allow(dead_code)]
    pub inflight: crate::kiro::scheduling::InflightGuard,
}

/// MCP（WebSearch 等工具调用）路径在用量库里的模型标识。
///
/// MCP 走的是 JSON-RPC over HTTP，请求体里**没有** `modelId`（不涉及模型推理），
/// 上游响应是搜索结果 JSON、既无 `meteringEvent` 也无任何 token 数。用一个显式常量
/// 标识这条路径，而不是冒用调用方那次请求的模型名——后者会让「某模型消耗了多少 token」
/// 的聚合凭空多出一批 token=0 的记录，反而更难解释。
const MCP_USAGE_MODEL: &str = "mcp";

/// 构造 MCP 路径的一条用量记录。
///
/// **诚实边界**：MCP 调用能确知的只有「哪张凭据、什么时候、被消耗了一次调用额度、
/// 耗时多久、重试了几次」，这恰好也是凭据 `success_count` 已经在记的东西。因此：
/// - `model` = [`MCP_USAGE_MODEL`]（上游请求体无 modelId，见常量注释）
/// - `input_tokens` / `output_tokens` = 0（上游不返回，也无本地估算依据；宁可为 0 也不瞎估）
/// - `credits_used` = None（MCP 响应无 meteringEvent）
/// - `is_streaming` = false（MCP 上游是一次性 JSON POST；WebSearch 对客户端的 SSE
///   是网关本地合成的，不属于这次上游调用的性质）
/// - `session_id` / 客户端画像 = None（provider 层拿不到入站 headers 与 conversationId）
fn build_mcp_record(
    credential_id: u64,
    outcome: crate::usage::RequestOutcome,
    latency_ms: u64,
    retries: u32,
) -> crate::usage::RequestRecord {
    let mut record =
        crate::usage::RequestRecord::new(uuid::Uuid::new_v4().to_string(), MCP_USAGE_MODEL);
    record.requested_model = Some(MCP_USAGE_MODEL.to_string());
    // MCP 路径无模型映射（请求体无 modelId），upstream 保持 None。
    record.credential_id = Some(credential_id);
    record.is_streaming = false;
    record.input_tokens = 0;
    record.output_tokens = 0;
    record.credits_used = None;
    record.latency_ms = latency_ms;
    record.retries = retries;
    record.outcome = outcome;
    record
}

/// Kiro API Provider
///
/// 核心组件，负责与 Kiro API 通信
/// 支持多凭据故障转移和重试机制
/// 按凭据 `endpoint` 字段选择 [`KiroEndpoint`] 实现
pub struct KiroProvider {
    token_manager: Arc<MultiTokenManager>,
    /// 全局代理配置（用于凭据无自定义代理时的回退）
    global_proxy: Option<ProxyConfig>,
    /// Client 缓存：key = effective proxy config, value = reqwest::Client
    /// 不同代理配置的凭据使用不同的 Client，共享相同代理的凭据复用 Client
    client_cache: Mutex<HashMap<Option<ProxyConfig>, Client>>,
    /// TLS 后端配置
    tls_backend: TlsBackend,
    /// 端点实现注册表（key: endpoint 名称）
    endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
    /// 默认端点名称（凭据未指定 endpoint 时使用）
    default_endpoint: String,
    /// 端点桶 429 封禁状态：key = `(credential_id, bucket_key)`，value = 解封时刻。
    ///
    /// 🔴 **key 从 `endpoint_name` 改成 `bucket_key`（= 解析后 host+region 与
    /// X-Amz-Target 共同界定的真实上游桶）**，收口 `bucket_id` 长期是死代码的问题。
    ///
    /// 按端点**名字**分桶在两种情况下是错的：
    /// 1. **不同名字、同一个桶**：非 us-east-1 时 `codewhisperer` 的 host 回退成
    ///    `q.{region}.amazonaws.com`，与 `cli` 的 host 和 X-Amz-Target 全都相同 ——
    ///    它们是**同一个**上游桶却被记成两个。后果：cw 桶被 429 封后 `select_endpoint`
    ///    换到 cli，打回同一个 host 又 429，而 `has_unthrottled_endpoint` 误判"还有可用桶"
    ///    → 持续轰炸同一个已被限流的上游。
    /// 2. **同一名字、不同桶**：同一凭据换区后（region 自纠正会改 `api_region`），
    ///    `cli@us-east-1` 与 `cli@eu-central-1` 是两个独立上游，却因名字相同被合并 ——
    ///    一个区被封会连带把另一个区也判成封禁，白丢可用容量。
    ///
    /// 用 `bucket_key` 后两者都对：region 天然在 host 里、同构端点自动去重。
    endpoint_buckets: Mutex<HashMap<(u64, String), Instant>>,
    /// DNS/连接层失败的端点负缓存（key: "endpoint_name@region", value: 首次失败时刻）。
    ///
    /// 连接层失败通常表示 DNS 不存在（如 eu-central-1 的 codewhisperer）或 host 路由黑洞，
    /// 端点回退逐一尝试会在每个请求上重复白跑 connect timeout。记住首次失败的端点，
    /// 30 分钟内跳过（避免瞬时网络抖动永久拉黑，过期自动重试）。与 `endpoint_buckets`
    /// 正交：后者是上游**明确告知**的限流（429 封桶 30s），本表是我们**观测到**的
    /// 连接层故障（300s），两者互不覆盖。MCP 直连失败负缓存复用本表但 TTL 更短
    /// （60s，见 [`MCP_DIRECT_NEG_CACHE_TTL`]）：键按凭据 id 分段，避免同 region
    /// 一个坏 ksk 连坐健康 OAuth。与 `{endpoint}@{region}` 连接层键不冲突。
    dead_endpoints: Mutex<HashMap<String, Instant>>,
    /// 协议不符的端点隔离缓存（key: "endpoint_name@region", value: 首次判定时刻）。
    ///
    /// 与 [`Self::dead_endpoints`] 互补：那个管「连不上」（DNS/TCP/TLS），这个管
    /// 「连上了但说的不是同一种协议」——上游返回 HTTP 2xx 却给出 JSON/文本而非
    /// AWS event-stream。隔离是**软**的且带 TTL：期内该 (端点, region) 在回退链里被跳过，
    /// 期满自动放行重试（上游修好、配置改对都能自愈，无需人工干预或重启）。
    protocol_broken: Mutex<HashMap<String, Instant>>,
    /// 端点自适应派发：每 `(凭据, 端点)` 记一份 EWMA 成功率，选端点时优先送到更可能
    /// 成功的那个，并保留探索通道防误判自我实现。
    ///
    /// 🔴 **替换了原来的 `endpoint_rotation: AtomicUsize`**（全进程共享的 round-robin
    /// 游标）。那个设计有两个硬缺陷：① 计数器与凭据无关 —— 号 A 的请求会推动号 B 的
    /// 起始位置，"每个凭据按自己的成功比率派发"无从表达；② 完全不看结果 —— 某端点对
    /// 某号恒 400（如 ksk_ 打 `codewhisperer` 实测 `The provided credential is invalid`）
    /// 时，轮换仍雷打不动每隔一次就送一批请求过去白撞，撞回来的失败还占重试预算、
    /// 挤掉本来能成功的那次尝试。
    ///
    /// 与本结构体的 `endpoint_buckets` 正交：后者是上游明确告知的限流**硬门**（封了就
    /// 不可选），本表是我们自己的统计**软偏好**（只在硬门放行的候选之间排序）。算法与
    /// 不持久化的理由见 [`crate::kiro::endpoint_health`] 模块文档。
    /// 指向进程级共享表（[`crate::kiro::endpoint_health::shared`]）。
    /// 用 `&'static` 而非自有实例，是为了让 admin 面板能在**不依赖 provider** 的前提下
    /// 读同一张表（范式对齐 `common::recovery_metrics`，理由详见该模块的 `SHARED` 注释）。
    endpoint_health: &'static EndpointHealth,
    /// 全局上游并发闸：限制**同时在飞**的上游 HTTP 调用数（容量来自
    /// `upstream_concurrency_limit`，重启生效）。防「号多 + 429 多 → 疯狂换号重试」
    /// 把内部上游 RPM 放大到外部 RPM 的十几倍。`OwnedSemaphorePermit` 跨 send 存活、
    /// 作用域结束自动 Drop 释放，免费防泄漏。
    upstream_gate: Arc<tokio::sync::Semaphore>,
    /// **每凭据**上游并发闸：限制单个号同时在飞的上游调用数。懒初始化，按 id 建一把。
    ///
    /// 🔴 为什么全局闸不够（这是移植 kiro2cc 两级闸模型的理由）：
    /// 全局闸只保证「总在飞 ≤ N」，**不保证分布**。号池里一旦有号响应慢（上游对它排队
    /// 而非立刻 429），慢号的请求会长时间占着全局许可 —— 极端情况下 N 个许可全被同一个
    /// 慢号吃掉，其余健康号一个许可都拿不到，**整池吞吐被一个号拖死**，而面板上看到的是
    /// 「并发闸已满」这种系统级症状，根本指不到是哪个号的问题。
    ///
    /// 加一道每凭据闸后，单号最多占 `upstream_per_credential_limit` 个许可，剩下的容量
    /// 必然留给别的号。这与选号层的 `inflight` 排序是**互补**而非重复：inflight 影响
    /// 「优先选谁」（软偏好），本闸是「选中了也不许超」（硬上限）—— 选号可能因亲和/
    /// 饱和门等原因仍旧选中同一个号，那时只有硬闸挡得住。
    ///
    /// 对照 kiro2cc：`MAX_CONCURRENT_REQUESTS=50` + `MAX_CONCURRENT_PER_CREDENTIAL=20`
    /// 两级；本仓全局默认 16（`upstream_concurrency_limit`），故每凭据默认取 8
    /// （见 `default_upstream_per_credential_limit`），保证至少两个号能同时打满。
    ///
    /// 用 `Mutex<HashMap>` 懒初始化而非预建：号是动态增删的（导入/删除/回收站恢复），
    /// 预建需要在每个增删点同步维护，漏一处就是「新号没有闸」。懒初始化让这件事不可能忘。
    upstream_per_credential_gates: Mutex<HashMap<u64, Arc<tokio::sync::Semaphore>>>,
    /// 每凭据闸容量（构造时从配置读定，与 `upstream_gate` 同为重启生效）。
    upstream_per_credential_limit: usize,
    /// 近 60s 上游结果滑动窗口（成功/429），喂给 [`apply_retry_pressure`] 做动态降档。
    retry_pressure: Mutex<RetryPressureWindow>,
}

/// 透传同号吸收是否应重试（纯判定，供 `'retry_same_cred` 循环与单元测试共用）。
///
/// - **429**：只跟总开关（与主路径 `UpstreamRateLimit` 同语义，见 config.rs 字段文档）；
///   5xx：还需 `server_error` 也开（5xx 可能是上游整片故障，重试只在故障期间放大请求量）。
/// - **400 容量类**：还需 `capacity_400` 也开，且错误体命中
///   `crate::kiro::endpoint::default_is_model_temporarily_unavailable` —— 与主路径
///   `absorb_class_of` 分类器**共用同一谓词**（认 `MODEL_TEMPORARILY_UNAVAILABLE` /
///   `model is temporarily unavailable` / `INSUFFICIENT_MODEL_CAPACITY` 三种上游形态，
///   含代挂号错误体直透的「上游 400 容量类」）。同源 ⇒ 上游错误形态演进时两侧同步生效，
///   不漂移。分支排在 5xx 之前，与主路径分类器顺序一致（容量类先判，防 503 形态的
///   容量错误被 5xx 判据抢走）。开关关时落到 `_ => false`，与改前逐字节一致。
/// - **本地失败绝不重试**（`local_failure=true`）：传输层（`connect_error:` 前缀）与
///   确定性本地错误（空错误体 = 缺 base_url / client 构建失败）重打 N 遍只会放大故障，
///   与主路径 `upstream_retry_absorb_server_error` 文档「排除传输层」同语义。
/// - `max_rounds` 是「额外轮次」语义（与主路径 `upstream_retry_absorb_max_rounds` 一致）：
///   `0` = 只打一次即不吸收；`attempt` 从 1 起计，共最多 `max_rounds` 次重试。
///   2026-08-11 对抗审查修：旧实现硬编码「3 次」且把 429 错挂在 server_error 开关上。
///   2026-08-13 对齐主路径：旧判据 `attempt >= max_rounds` 实际只重试 `max_rounds - 1` 次，
///   比主路径（`absorb_round >= effective_max_rounds()`，轮从 0 起计 = `max_rounds` 轮额外）
///   少一轮，改为 `attempt > max_rounds`。
fn passthrough_absorb_should_retry(
    code: u16,
    local_failure: bool,
    enabled: bool,
    server_error: bool,
    capacity_400: bool,
    upstream_err: &str,
    attempt: u32,
    max_rounds: u32,
) -> bool {
    // `max_rounds == 0` 显式排除：`attempt > max_rounds` 对 0 恒真（1 > 0），
    // 不加这道门会让「0 = 不吸收」退化成吸收一次。
    if local_failure || attempt == 0 || max_rounds == 0 || attempt > max_rounds {
        return false;
    }
    match code {
        429 => enabled,
        // ⭐ 400 容量类先判（与主路径 absorb_class_of 的分类器顺序一致：容量类在 5xx 之前）。
        400
            if enabled
                && capacity_400
                && crate::kiro::endpoint::default_is_model_temporarily_unavailable(
                    upstream_err,
                ) =>
        {
            true
        }
        c if (500..600).contains(&c) => enabled && server_error,
        _ => false,
    }
}

/// 透传同号吸收的退避毫秒数：`250ms × 2^attempt`，clamp 到配置的
/// `[min_delay_ms, max_delay_secs]` 区间（min 取 1ms 兜底；max 恒 ≥ min，
/// 与主路径 `AbsorbPolicy` 的 clamp 语义一致）。
fn passthrough_absorb_delay_ms(attempt: u32, min_delay_ms: u64, max_delay_secs: u64) -> u64 {
    let base = 250u64.saturating_mul(2u64.saturating_pow(attempt));
    let min = min_delay_ms.max(1);
    let max = max_delay_secs.saturating_mul(1000).max(min);
    base.clamp(min, max)
}

/// 透传 400/404 是否「换号无益」（额度耗尽 / 请求超长）——命中则不 failover 直返。
///
/// 判据用**连续形态**词表（对齐实测/常见上游：OpenAI 系 `insufficient_quota` 与
/// `exceeded your current quota`、one-api 系 `quota exhausted`、DeepSeek 系
/// `Insufficient Balance`、超长类 `too long` / `CONTENT_LENGTH_EXCEEDS_THRESHOLD`），
/// 刻意不认裸 `quota`（2026-08-15 收窄）：body 任意含 `quota` 即判无益，会把
/// 「quota tier / quota 配置」这类**上游能力差异**文案（换一个号可能成功）也吞成
/// 直返，让客户端白吃一个本来能靠换号解决的 400/404。
fn is_hopeless_upstream_400(body_lower: &str) -> bool {
    [
        "too long",
        "content_length_exceeds",
        "usage limit",
        "insufficient balance",
        "insufficient_quota",
        "insufficient quota",
        "quota exceeded",
        "quota_exceeded",
        "quota exhausted",
        "quota_exhausted",
        "quota limit",
        "quota_limit",
        "quota_reached",
        "exceeded your current quota",
        "no quota",
    ]
    .iter()
    .any(|k| body_lower.contains(k))
}

impl KiroProvider {
    /// 创建带代理配置和端点注册表的 KiroProvider 实例
    ///
    /// # Arguments
    /// * `token_manager` - 多凭据 Token 管理器
    /// * `proxy` - 全局代理配置
    /// * `endpoints` - 端点名 → 实现的注册表（至少包含 `default_endpoint` 对应条目）
    /// * `default_endpoint` - 凭据未显式指定 endpoint 时使用的名称
    pub fn with_proxy(
        token_manager: Arc<MultiTokenManager>,
        proxy: Option<ProxyConfig>,
        endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
        default_endpoint: String,
    ) -> Self {
        assert!(
            endpoints.contains_key(&default_endpoint),
            "默认端点 {} 未在 endpoints 注册表中",
            default_endpoint
        );
        let tls_backend = token_manager.config().tls_backend;
        // 告警配置随 provider 构造注入（热更不生效，改配置需重启；未配置时告警 bump 零开销）。
        {
            let ac = token_manager.config();
            crate::common::alerting::init(
                ac.alert_webhook_url.clone(),
                ac.alert_cooldown_secs,
                ac.host.clone(),
            );
        }
        // 预热：构建全局代理对应的 Client
        // 对话路径用流式 client：read_timeout(空闲间隔) 而非总时长，防长流被中途掐断
        // （根因见 build_streaming_client 注释：修 `Connection closed mid-response`）。
        let initial_client =
            build_streaming_client(proxy.as_ref(), 720, tls_backend).expect("创建 HTTP 客户端失败");
        let mut cache = HashMap::new();
        cache.insert(proxy.clone(), initial_client);

        let concurrency_limit = token_manager
            .config()
            .upstream_concurrency_limit
            .max(1);
        // ⚠️ 必须在**构造 Self 之前**算：`token_manager` 会被 move 进结构体，
        // 之后再 `token_manager.config()` 就是 use-after-move（E0382）。
        // 与上面 `concurrency_limit` 同一理由，故并排放在这里。
        //
        // 配 0 视为「不限」并退化成全局闸容量 —— 而不是真的 0：
        // `Semaphore::new(0)` 会让该号永远拿不到许可 = 号被静默废掉，
        // 症状是「号在池里但一个请求都不走」，极难排查。
        let per_credential_limit = {
            let v = token_manager.config().upstream_per_credential_limit;
            if v == 0 { concurrency_limit } else { v.max(1) }
        };
        Self {
            token_manager,
            global_proxy: proxy,
            client_cache: Mutex::new(cache),
            tls_backend,
            endpoints,
            default_endpoint,
            endpoint_buckets: Mutex::new(HashMap::new()),
            dead_endpoints: Mutex::new(HashMap::new()),
            protocol_broken: Mutex::new(HashMap::new()),
            endpoint_health: crate::kiro::endpoint_health::shared(),
            upstream_gate: Arc::new(tokio::sync::Semaphore::new(concurrency_limit)),
            upstream_per_credential_gates: Mutex::new(HashMap::new()),
            upstream_per_credential_limit: per_credential_limit,
            retry_pressure: Mutex::new(RetryPressureWindow::new(PRESSURE_WINDOW_SECS)),
        }
    }

    /// 未禁用的 Kiro 号（非 custom_api）是否存在。WebSearch MCP 快路径依赖它。
    pub fn has_enabled_kiro_credential(&self) -> bool {
        self.token_manager.has_enabled_kiro_credential()
    }

    /// 取（或懒建）某凭据的并发闸。
    ///
    /// 临界区只做 HashMap entry + Arc clone，不含 await、不调用任何可能反向取锁的函数
    /// ⇒ 无锁顺序风险。返回 `Arc` 而非借用，是为了让调用方在**锁外**再 await/acquire
    /// （持锁 await 是 `parking_lot::Mutex` 的硬错误 —— 它不是异步锁）。
    fn per_credential_gate(&self, id: u64) -> Arc<tokio::sync::Semaphore> {
        let mut map = self.upstream_per_credential_gates.lock();
        map.entry(id)
            .or_insert_with(|| {
                Arc::new(tokio::sync::Semaphore::new(
                    self.upstream_per_credential_limit,
                ))
            })
            .clone()
    }

    /// 凭据被删除/purge 时清掉它的并发闸与端点统计，防两张表随号增删无界增长。
    ///
    /// ⚠️ **目前没有调用点**，这是刻意的，不是漏接线：
    /// - `AdminService`（删号入口）**不持有** `KiroProvider`（实测 `admin/service.rs` 里
    ///   只有注释提到 provider，无字段），要接线得新增一条 admin → provider 的依赖，
    ///   属跨层改动，收益却只有内存回收。
    /// - 不清理**不会导致错误**：凭据 id 永不复用（`token_manager` 的 `next_id` 是单调
    ///   计数器，注释明确说明「永不回退、永不复用」），所以陈旧条目不可能被新号读到，
    ///   既不会串号也不会读到旧统计。
    /// - 泄漏量级极小：每号一个 `(u64, Arc<Semaphore>)` + 几条 EWMA 记录，即使反复增删
    ///   上万次也是 KB 级。
    ///
    /// 保留本函数是为了「将来真要接线时有现成入口」，并把上述判断写在这里 ——
    /// 否则下一个人看到 map 只增不减会以为是 bug 而去补一条不必要的跨层依赖。
    pub fn forget_credential_runtime_state(&self, id: u64) {
        self.upstream_per_credential_gates.lock().remove(&id);
        self.endpoint_health.forget_credential(id);
    }

    /// 根据凭据的代理配置获取（或创建并缓存）对应的 reqwest::Client
    fn client_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Client> {
        let effective = credentials.effective_proxy(self.global_proxy.as_ref());
        let mut cache = self.client_cache.lock();
        if let Some(client) = cache.get(&effective) {
            return Ok(client.clone());
        }
        let client = build_streaming_client(
            effective.as_ref(),
            MCP_CLIENT_READ_TIMEOUT_SECS,
            self.tls_backend,
        )?;
        cache.insert(effective, client.clone());
        Ok(client)
    }

    /// 根据凭据选择 endpoint 实现
    fn endpoint_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Arc<dyn KiroEndpoint>> {
        // ⭐ 必须走 effective_endpoint，与 `endpoint::for_credentials` / `main.rs` 启动校验
        // / admin snapshot 三处口径一致。
        //
        // 🔴 修复的缺陷（另一位 review 抓到，实测确证）：此处原先只读 `credentials.endpoint`
        // 原始字段，**漏了 `ksk_` API Key 号自动路由到 CLI 端点**这一层。
        // 而 `endpoint/mod.rs` 的 `for_credentials` 文档明写"口径与 endpoint_for 完全一致" ——
        // 那句话此前是**假的**：旁路走 effective_endpoint、请求热路径不走。
        //
        // 后果链（与线上号池被烧直接相关）：一个健康的 `ksk_` 号若未手工填 `endpoint: cli`，
        // 请求会打到 IDE 端点 → 403 → 连续 6 次触发 `report_suspicious_activity`
        // → 判定死号自动禁用。实测 `effective_endpoint()` 返回 `cli` 而此处返回 `ide`。
        let name = credentials.effective_endpoint(&self.default_endpoint);
        self.endpoints
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("未知端点: {}", name))
    }

    /// 该 (端点, region) 是否处于死亡负缓存窗口内（跳过本跳，别再白跑一次 DNS/连接超时）。
    fn is_endpoint_dead(&self, endpoint_name: &str, region: &str) -> bool {
        let key = format!("{}@{}", endpoint_name, region);
        let mut dead = self.dead_endpoints.lock();
        match dead.get(&key) {
            Some(at) if at.elapsed() < DEAD_ENDPOINT_TTL => true,
            // TTL 已过 → 清掉条目，让它重新试一次（region 可能恢复/配置已改）。
            Some(_) => {
                dead.remove(&key);
                false
            }
            None => false,
        }
    }

    /// MCP 直连失败短负缓存（M3）：直连失败（401/403/429）后 60s 内跳过**该凭据**
    /// 在该 region 的直连。
    ///
    /// 复用 [`Self::dead_endpoints`] 键空间：端点名写成 `mcp-direct@{id}`，
    /// [`Self::mark_endpoint_dead`] 再拼 `@{region}` → 按凭据划界。同 region
    /// 另一个号不受连坐（坏 ksk 不得蒙住健康 OAuth）。TTL 远短于连接层失败
    /// （60s vs 300s）：直连是「无号」场景的轻量探测，失败后短暂跳过即可。
    fn is_mcp_direct_blocked(&self, credential_id: u64, region: &str) -> bool {
        let key = format!("mcp-direct@{}@{}", credential_id, region);
        let mut dead = self.dead_endpoints.lock();
        match dead.get(&key) {
            Some(at) if at.elapsed() < MCP_DIRECT_NEG_CACHE_TTL => true,
            // TTL 已过 → 清掉条目，让它重新试一次（token/region 可能已恢复）。
            Some(_) => {
                dead.remove(&key);
                false
            }
            None => false,
        }
    }

    /// 记一次 (端点, region) 连接层失败。仅用于**连接层**失败（DNS/TCP/TLS），
    /// HTTP 状态码错误（429/5xx）绝不进这里——那是容量问题，host 本身是好的。
    /// （M3 例外：MCP 直连的 HTTP 级失败以 `mcp-direct@{id}` 作端点名走本函数，
    /// 由 [`Self::is_mcp_direct_blocked`] 按凭据 id + region 读，TTL 更短。）
    fn mark_endpoint_dead(&self, endpoint_name: &str, region: &str) {
        let key = format!("{}@{}", endpoint_name, region);
        self.dead_endpoints
            .lock()
            .insert(key, std::time::Instant::now());
    }

    /// 清除 (端点, region) 的负缓存。拿到 HTTP 响应 = 连接层通了（哪怕业务层 429/5xx）。
    fn mark_endpoint_alive(&self, endpoint_name: &str, region: &str) {
        let key = format!("{}@{}", endpoint_name, region);
        self.dead_endpoints.lock().remove(&key);
    }

    /// 该 (端点, region) 是否处于「协议不符」隔离窗口内。
    ///
    /// 与 [`Self::is_endpoint_dead`] 同款自愈语义：TTL 一到自动清条目并放行重试，
    /// 因此上游恢复或配置改对之后无需人工干预、无需重启即自动回到轮转。
    fn is_route_protocol_broken(&self, endpoint_name: &str, region: &str) -> bool {
        let key = format!("{}@{}", endpoint_name, region);
        let mut broken = self.protocol_broken.lock();
        match broken.get(&key) {
            Some(at) if at.elapsed() < PROTOCOL_BROKEN_TTL => true,
            // TTL 已过 → 清掉条目，让它重新试一次（上游可能已修好）。
            Some(_) => {
                broken.remove(&key);
                false
            }
            None => false,
        }
    }

    /// 记一次 (端点, region) 协议不符（上游返回非 event-stream 响应）。
    ///
    /// 仅应在**确定性判据**命中时回报（例如解码层首字节不可能属于合法 event-stream
    /// 帧长），绝不因业务错误码或偶发截断进入这里。当前本仓的解码层回报链路尚未
    /// 接线到此处（provider 允许改动范围内无解码层入口），方法先就绪供测试与后续接线；
    /// 未接线时 `protocol_broken` 表恒空，链行为等价「仅 dead_endpoint 负缓存生效」。
    fn mark_route_protocol_broken(&self, endpoint_name: &str, region: &str) {
        let key = format!("{}@{}", endpoint_name, region);
        self.protocol_broken
            .lock()
            .insert(key, std::time::Instant::now());
    }

    /// 构造本次调用的端点回退链（链式回退，P0 移植）。
    ///
    /// 以 `head`（= [`Self::select_endpoint`] 选中的端点：桶机制 + EWMA 健康分已应用）
    /// 为链首，先按凭据的 [`KiroCredentials::effective_endpoint_order`] 补齐
    /// （ksk_ 号 = CLI 族端点：q.* 优先、runtime.* 回退，与 select_endpoint 同源 →
    /// 轮内链与跨轮桶机制天然对齐），再用 [`ENDPOINT_FALLBACK_ORDER`] 按**协议族**
    /// 补齐：ksk_ 剔除 ide；OAuth/Social/IdC **不**补 codewhisperer / amazonq
    /// （CLI 族 + 硬编码 `tokentype: API_KEY`，OAuth Bearer 打过去是确定性 403）。
    /// 显式 `endpoint` 仍走 ① 的凭据候选，不经本补齐。`endpoint_fallback = false`
    /// 或注册表只有一个端点时退化为单元素链。
    ///
    /// 规则（对齐参考仓 jsjm）：
    /// - 主端点被证实协议不符 → 不占链首（降级出链，兜底位除外）；
    /// - 其余协议不符的端点同样跳过；
    /// - **兜底铁律**：链绝不为空（否则 response 恒 None，请求无人发送）。
    fn endpoint_chain_for(
        &self,
        head: &Arc<dyn KiroEndpoint>,
        credentials: &KiroCredentials,
        fallback_enabled: bool,
        upstream_region: &str,
    ) -> Vec<Arc<dyn KiroEndpoint>> {
        if !fallback_enabled {
            return vec![head.clone()];
        }
        let head_name = head.name();
        let head_broken = self.is_route_protocol_broken(head_name, upstream_region);
        // 主端点协议不符 → 它不占链首（仍保留兜底位，避免整条链为空无人发送）。
        let mut chain = if head_broken {
            tracing::warn!(
                "端点 {} 在 region {} 处于协议不符隔离期，本次请求优先改走回退端点",
                head_name,
                upstream_region
            );
            Vec::new()
        } else {
            vec![head.clone()]
        };
        // ① 凭据候选顺序补齐（与 select_endpoint 同源，ksk_ 号 = CLI 族端点）。
        for name in credentials.effective_endpoint_order(&self.default_endpoint) {
            if name == head_name || self.is_route_protocol_broken(name, upstream_region) {
                continue;
            }
            if let Some(ep) = self.endpoints.get(name) {
                chain.push(ep.clone());
            }
        }
        // ② 通用补齐顺序。按协议族裁剪 ENDPOINT_FALLBACK_ORDER，禁止跨族兜底：
        //
        // ksk_（API_KEY）号是 CLI 协议族：codewhisperer / amazonq 同为 CLI
        // （服务根 `/` + X-Amz-Target + `tokentype: API_KEY`），ide 是 OAuth/IDE
        // 协议端点 —— ksk_ 打 ide 必 403。对抗审查 M2：ksk_ **整体剔除 ide**
        // （不是挪到链尾：链尾兜底铁律永不跳过，容量风暴时必被真打 → 403
        // 从从未成功号 report_failure 累计 → TooManyFailures 误禁用，#481 同型）。
        //
        // OAuth/Social/IdC 对称：不得从本表补 CLI 族。CLI decorate 对所有 CLI 族
        // 端点硬编码 tokentype=API_KEY，OAuth Bearer 打过去同样是确定性 403，
        // 且 403 不在链内瞬时跳转集合里，会离开链进入凭据级认证冷却。
        // 显式 `endpoint` 已在 ① 入链；本表只保留 ide。不把 endpoint_fallback
        // 默认改 false（那会拆掉 ksk_ 的 cli→cw→amazonq 同族回退）。
        let mut fallback_order: Vec<&str> = ENDPOINT_FALLBACK_ORDER.to_vec();
        let ide_name = crate::kiro::endpoint::ide::IDE_ENDPOINT_NAME;
        if !credentials.is_custom_api_credential() && credentials.is_api_key_credential() {
            fallback_order.retain(|n| *n != ide_name);
        } else if !credentials.is_custom_api_credential() {
            fallback_order.retain(|n| *n == ide_name);
        }
        for name in fallback_order {
            if chain.iter().any(|ep| ep.name() == name) {
                continue;
            }
            // 同样跳过其它已知协议不符的端点（链尾兜底除外，见下）。
            if self.is_route_protocol_broken(name, upstream_region) {
                continue;
            }
            if let Some(ep) = self.endpoints.get(name) {
                chain.push(ep.clone());
            }
        }
        // 兜底铁律：链绝不为空（否则 response 恒 None，请求无人发送）。
        if chain.is_empty() {
            chain.push(head.clone());
        }
        chain
    }

    /// 在凭据的端点候选里选一个：**硬门筛完 → 按实测成功率派发**。
    ///
    /// 两段刻意分离（不可合并，理由见 [`crate::kiro::endpoint_health`] 模块文档）：
    ///
    /// 1. **硬门**（本函数）：剔除被 429 封禁的桶。封禁是上游明确告知的限流事实，
    ///    带 Retry-After 语义，不容统计推断插手。顺带惰性清理已过期条目防 map 无界增长。
    /// 2. **软偏好**（[`EndpointHealth::pick`]）：在硬门放行的候选之间，按该**凭据自己**
    ///    在各端点上的 EWMA 成功率挑一个，并周期性探索非最优候选。
    ///
    /// 🔴 **这里原先是 round-robin**（`start = 全局计数器 % len` 后顺序取第一个未封禁者）。
    /// 换掉的原因是它既不"每凭据"也不"按成功率"：计数器全进程共享，且无论某端点对某号
    /// 是否恒失败，都照样每隔一次送一批请求过去白撞。
    ///
    /// 候选顺序仍承载**先验**：冷启动（无样本）与同分时靠前者胜出，所以
    /// [`KiroCredentials::effective_endpoint_order`] 里 q.* 优先、runtime.* 回退的既有语义
    /// 在没有统计数据时逐字保留。
    ///
    /// 返回 `None` = 该凭据所有端点桶当前都在封禁期 → 调用方应走凭据级冷却/换号。
    /// 仅测试用：旧签名的薄封装（只取端点、丢掉备区）。
    ///
    /// 存在理由：`select_endpoint` 2026-08-10 改为返回 `(端点, 备用 region)` 以支持
    /// 「当前区所有桶被 429 封禁 ⇒ 改用备区桶」。9 处既有测试只关心选中哪个端点，
    /// 让它们各自解元组会淹没断言本身。**生产代码禁止用它** —— 丢掉备区会导致
    /// URL 打当前区而封禁记账写备区桶键，即"封禁写进去读不到"。
    ///
    /// ⚠️ 刻意**不加**测试专用的条件编译属性：本文件有多个守卫测试靠"找该属性第一次
    /// 出现的位置"来切出「生产代码区」再做断言（如
    /// `quota_exhausted_must_not_be_gated_on_status_code`）。若在 tests 模块之前出现
    /// 该属性——**哪怕只是写在注释里的字面量**——切分点就会提前、生产区被截断，
    /// 那些守卫**静默失效**（不报错、不 FAIL，最难发现的一种坏法）。
    /// 代价只是本函数会被编进 release（一个单行 map，可忽略），换来守卫不被破坏。
    /// 同款坑本轮已踩过一次（`upstream_hops` 累加位置的守卫），故此处刻意绕开。
    #[allow(dead_code)]
    fn pick_endpoint_for_test(
        &self,
        credentials: &KiroCredentials,
        id: u64,
    ) -> Option<Arc<dyn KiroEndpoint>> {
        self.select_endpoint(credentials, id).map(|(ep, _)| ep)
    }

    /// 返回 `(端点, 备用 region)`。第二项为 `Some(区)` 时，调用方**必须**把它覆盖到
    /// 请求所用凭据的 `api_region` 上 —— 否则请求仍打当前区，而封禁记账会写到备区的桶，
    /// 造成「封禁写进去读不到」的漂移（与 `bucket_key` 守卫测试防的是同一类错误）。
    fn select_endpoint(
        &self,
        credentials: &KiroCredentials,
        id: u64,
    ) -> Option<(Arc<dyn KiroEndpoint>, Option<&'static str>)> {
        let order = credentials.effective_endpoint_order(&self.default_endpoint);
        if order.is_empty() {
            return None;
        }

        // ① 硬门：只留未封禁且**确实已注册**的端点。
        //    注册检查必须在这一层做，否则 pick 可能选中一个 `endpoints` 里不存在的名字
        //    （凭据 endpoint 字段是面板可手填的），随后 get 拿不到而整体返回 None ——
        //    那等于"有可用端点却报全封禁"，会误触发凭据级冷却。
        //
        // 🔴 桶键用 `endpoint.bucket_key(credentials, config)` 而非端点**名字**：
        // 名字既会把「同 host+target 的不同名端点」（非 us-east-1 的 codewhisperer 与 cli）
        // 误判成两个桶，也会把「同名但不同 region」当成一个桶。详见 `endpoint_buckets`
        // 字段注释。config 在这里取一次快照给整轮用，避免每个候选各 load 一遍。
        let config = self.token_manager.config();
        let allowed: Vec<&str> = {
            let mut buckets = self.endpoint_buckets.lock();
            let now = Instant::now();
            let mut keep = Vec::with_capacity(order.len());
            for name in &order {
                // 未注册的名字直接跳过：拿不到实现就算不出桶键，也不可能被选中。
                let Some(ep) = self.endpoints.get(*name) else {
                    continue;
                };
                let key = (id, ep.bucket_key(credentials, &config));
                if let Some(&until) = buckets.get(&key) {
                    if now < until {
                        continue; // 该桶仍在封禁期
                    }
                    buckets.remove(&key); // 惰性清理已过期条目
                }
                keep.push(*name);
            }
            keep
        };

        // ② 软偏好：按该凭据的实测成功率派发。
        if let Some(picked) = self.endpoint_health.pick(id, &allowed) {
            return self
                .endpoints
                .get(picked)
                .cloned()
                .map(|ep| (ep, None::<&'static str>));
        }

        // ────────────────────────────────────────────────────────────────────
        // ③ 🔴 **当前区全封 ⇒ 尝试备用 region 的桶**（2026-08-10 新增）。
        //
        // # 修的是什么
        // 一个 `ksk_` 号的桶集合此前**只含当前 region 的两个**（`q.<区>` 与
        // `runtime.<区>`）—— 因为 `bucket_key(credentials, config)` 里的 region 来自
        // `effective_upstream_region`，那是一次算定的**固定值**。于是：
        //   当前区两个桶被 429 各封 30s（`ENDPOINT_BUCKET_THROTTLE`）
        //   ⇒ 本函数返 None ⇒ 调用方判「所有端点桶均处于 429 封禁期」⇒ 该号不可用
        //   ⇒ **另一个区即使完全空闲也永远不会被尝试**。
        // 实测后果：单号有效 RPM 被压到「30s 窗口能挤进多少」，即用户观察到的十几二十
        // （对照：`service.rs:2139` 记录同批 key 探到正确 region 后有号跑到 881/881 全成功）。
        //
        // # 为什么不改 403 那条换区路径（`region_retry_target`）
        // 那条有 `has_ever_succeeded` 门控且**只处理 403**，门控有实测依据
        // （4 个号累计 3393 次成功、共吃 42 次 bearer-invalid 403，那些确实是瞬态抖动
        // 而非 region 错配）。放宽它会让健康号被误判换区。**429 需要自己的路径。**
        //
        // # 为什么这样安全
        // - **只在当前区全封时才走到这里** ⇒ 正常路径行为零改变（上面已 return）。
        // - 桶键本身含 region（靠 host 隐含携带）⇒ 备区的封禁状态与当前区**天然独立**，
        //   不会互相污染，也不需要新的 key 结构。
        // - 只对 `api_key`（ksk_）号启用：OAuth 号的 region 由 `profileArn` 权威决定，
        //   拿它去打别的区必然 403（`endpoint/mod.rs:358` 实测：文案与 bearer-invalid
        //   完全不同），换区对它有害无益。
        if !credentials.is_api_key_credential() {
            return None;
        }
        let cur = credentials.effective_upstream_region(&config);
        // 备区候选复用 `region_probe::PROBE_ORDER`（与 403 换区同一份顺序，避免两处漂移），
        // 跳过当前区本身。
        let alt = crate::kiro::region_probe::PROBE_ORDER
            .iter()
            .copied()
            .find(|r| *r != cur)?;

        let mut alt_cred = credentials.clone();
        alt_cred.api_region = Some(alt.to_string());
        let alt_allowed: Vec<&str> = {
            let mut buckets = self.endpoint_buckets.lock();
            let now = Instant::now();
            let mut keep = Vec::with_capacity(order.len());
            for name in &order {
                let Some(ep) = self.endpoints.get(*name) else {
                    continue;
                };
                let key = (id, ep.bucket_key(&alt_cred, &config));
                if let Some(&until) = buckets.get(&key) {
                    if now < until {
                        continue;
                    }
                    buckets.remove(&key);
                }
                keep.push(*name);
            }
            keep
        };
        let picked = self.endpoint_health.pick(id, &alt_allowed)?;
        tracing::info!(
            credential_id = id,
            from_region = cur,
            to_region = alt,
            endpoint = picked,
            "当前 region 所有端点桶均被 429 封禁，改用备用 region 的桶（仅 ksk_ 号）"
        );
        self.endpoints.get(picked).cloned().map(|ep| (ep, Some(alt)))
    }

    /// 记一次端点级结果，喂给自适应派发表。
    ///
    /// 🔴 **口径承重**：`success` 只反映「**这个端点是否愿意受理这个凭据**」，
    /// 绝不能把凭据自身的问题算进来。
    ///
    /// - 算端点失败：连接失败、该端点特有的 400（如 ksk_ 打 codewhisperer 的
    ///   `The provided credential is invalid`）、该端点的 429。
    /// - **不算**端点失败：402 额度耗尽、403 账号封禁/暂停、refreshToken 失效 ——
    ///   这些换端点一样失败，记进去只会污染判断，让健康端点被无辜降权，
    ///   最终把一个"号坏了"误传成"端点坏了"，并因此把流量赶到真正更差的端点上。
    fn report_endpoint_outcome(&self, id: u64, endpoint_name: &str, success: bool) {
        self.endpoint_health.record(id, endpoint_name, success);
    }

    /// 端点自适应派发的全量快照（供 admin 面板展示每凭据每端点的成功率与样本数）。
    ///
    /// 没有可观测就调不了也证不了 —— 这是本仓的历史教训（CLAUDE.md 记载
    /// 「先修度量，再谈调参」：一个关键数字是配置自乘出来的假值，导致所有依赖它的
    /// 自动调节都在算空气）。
    pub fn endpoint_health_snapshot(
        &self,
    ) -> Vec<crate::kiro::endpoint_health::EndpointHealthSnapshot> {
        self.endpoint_health.snapshot()
    }

    /// 每凭据并发闸的容量（供测试与面板展示；构造时固定，重启生效）。
    pub fn per_credential_limit(&self) -> usize {
        self.upstream_per_credential_limit
    }

    /// 该凭据是否还有**未封禁**的端点桶（429 时决定「换端点继续」还是「冷却换号」）。
    fn has_unthrottled_endpoint(&self, credentials: &KiroCredentials, id: u64) -> bool {
        let order = credentials.effective_endpoint_order(&self.default_endpoint);
        if order.is_empty() {
            return false;
        }
        let config = self.token_manager.config();
        let buckets = self.endpoint_buckets.lock();
        let now = Instant::now();
        let has_unthrottled_in_region = |creds: &KiroCredentials| -> bool {
            order.iter().any(|name| {
                let Some(ep) = self.endpoints.get(*name) else { return false };
                let key = (id, ep.bucket_key(creds, &config));
                let throttled = matches!(buckets.get(&key), Some(&until) if now < until);
                !throttled
            })
        };
        if has_unthrottled_in_region(credentials) {
            return true;
        }
        // 当前 region 的桶全封时，也检查备用 region（仅 api_key 号适用，OAuth/IdC
        // 的 region 由 profileArn 权威决定，换区无意义）。与 select_endpoint 同口径：
        // 当前区必须用 `effective_upstream_region` 判定（2026-08-11 修：此前用裸
        // api_region 比较，api_region=None 且有效区恰为 PROBE_ORDER 首项时会把
        // 备区算成当前区、重复查同一批桶 → 修复静默失效，走冷却换号）。
        if credentials.is_api_key_credential() {
            let cur = credentials.effective_upstream_region(&config);
            let alt = crate::kiro::region_probe::PROBE_ORDER
                .iter()
                .copied()
                .find(|r| *r != cur);
            if let Some(alt_region) = alt {
                let mut alt_creds = credentials.clone();
                alt_creds.api_region = Some(alt_region.to_string());
                if has_unthrottled_in_region(&alt_creds) {
                    return true;
                }
            }
        }
        false
    }

    /// 端点桶最短剩余封禁秒数。`credential_id=Some` 时先看该号，没有再用全表。
    /// 无有效剩余（已过期 / 亚秒）→ 2（handlers A5 要 `retry_after_secs=` 才能 429 而非 502）。
    fn shortest_endpoint_bucket_retry_after_secs(&self, credential_id: Option<u64>) -> u64 {
        let now = Instant::now();
        let buckets = self.endpoint_buckets.lock();
        let min_of = |want: Option<u64>| -> Option<u64> {
            buckets
                .iter()
                .filter(|((id, _), until)| **until > now && want.map(|w| *id == w).unwrap_or(true))
                .map(|(_, until)| until.saturating_duration_since(now).as_secs())
                .filter(|&s| s > 0)
                .min()
        };
        min_of(credential_id)
            .or_else(|| min_of(None))
            .unwrap_or(2)
    }

    /// 每个启用中的 Kiro 号都没有未封禁端点桶（含 ksk 备区）。空 Kiro 池不算。
    fn all_enabled_kiro_endpoint_buckets_sealed(&self) -> bool {
        let snap = self.token_manager.peek_enabled_kiro();
        if snap.is_empty() {
            return false;
        }
        snap.iter()
            .all(|(id, cred)| !self.has_unthrottled_endpoint(cred, *id))
    }

    /// last hop：全池端点桶 429 封禁且终态还是无 `retry_after_secs=` 的 generic 串
    /// → 打上最短桶 TTL（或 2s），让 `map_provider_error` A5 走 429 + Retry-After 而非 502。
    /// 不增加 hop。已有标记 / 非 RateLimited 类终态不改。
    fn with_sealed_bucket_retry_after(
        &self,
        err: anyhow::Error,
        last_outcome: crate::usage::RequestOutcome,
    ) -> anyhow::Error {
        let s = err.to_string();
        if s.contains("retry_after_secs=") {
            return err;
        }
        if !self.all_enabled_kiro_endpoint_buckets_sealed() {
            return err;
        }
        let generic = matches!(
            last_outcome,
            crate::usage::RequestOutcome::RateLimited
                | crate::usage::RequestOutcome::OtherError
                | crate::usage::RequestOutcome::ServerError
        ) || s.contains("所有端点桶均处于")
            || s.contains("已达到最大重试次数");
        if !generic {
            return err;
        }
        let secs = self.shortest_endpoint_bucket_retry_after_secs(None);
        anyhow::anyhow!("{s} retry_after_secs={secs}")
    }

    /// 发送非流式 API 请求
    ///
    /// 支持多凭据故障转移（见 [`Self::call_api_with_retry`]）；
    /// `budget` 为每客户端请求共享的上游预算（2026-08-11 方案 A，防跨层 RPM 放大）。
    pub async fn call_api(
        &self,
        request_body: &str,
        is_1m: bool,
        budget: &SharedRetryBudget,
        client_model: Option<&str>,
    ) -> anyhow::Result<(reqwest::Response, CallMeta)> {
        self.call_api_with_retry(request_body, false, is_1m, budget, client_model)
            .await
    }

    /// 发送流式 API 请求
    pub async fn call_api_stream(
        &self,
        request_body: &str,
        is_1m: bool,
        budget: &SharedRetryBudget,
        client_model: Option<&str>,
    ) -> anyhow::Result<(reqwest::Response, CallMeta)> {
        self.call_api_with_retry(request_body, true, is_1m, budget, client_model)
            .await
    }

    /// 发送 MCP API 请求（WebSearch 等工具调用）
    ///
    /// 成功时返回 (响应, 实际使用的凭据 id) —— 调用方（websearch.rs 快路径埋点）
    /// 需要 credential_id 落用量记录；此前只返回 Response，埋点只能写 None。
    ///
    /// # 无号直连兜底（P0，2026-08-16）
    ///
    /// `call_mcp_with_retry` 因「池子选不到号」失败时（错误带
    /// [`MCP_POOL_UNAVAILABLE_MARKER`] 标记 —— 纯 custom_api 透传池 / 全池禁用的
    /// 结构信号），改走 [`Self::call_mcp_direct`]：用池里**任意**带 Kiro Bearer
    /// token 的凭据直连 MCP 端点。OAuth 与有号路径同口径带
    /// `x-amzn-kiro-profile-arn`（runtime 主机 2026-08 起要求）；ksk_ 永不带。
    /// 直连失败 → 降级返回原错误（客户端行为与现状逐字节一致）。
    ///
    /// **有号路径零变化**：直连只在 `acquire_context` 彻底失败后触发，成功路径
    /// 与开关关闭时的错误路径都逐字节等于旧实现。
    pub async fn call_mcp(
        &self,
        request_body: &str,
        budget: &SharedRetryBudget,
    ) -> anyhow::Result<(reqwest::Response, u64)> {
        match self.call_mcp_with_retry(request_body, budget).await {
            Ok(v) => Ok(v),
            Err(e) => {
                let es = e.to_string();
                let Some(pool_err) = es.strip_prefix(MCP_POOL_UNAVAILABLE_MARKER) else {
                    return Err(e);
                };
                if MCP_DIRECT_BYPASS_ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
                    match self.call_mcp_direct(request_body, budget).await {
                        Ok(v) => return Ok(v),
                        Err(direct_err) => {
                            tracing::warn!(
                                direct_err = %direct_err,
                                pool_err = %pool_err.trim_start_matches(": "),
                                "MCP 无号直连失败，降级返回池子错误"
                            );
                        }
                    }
                }
                // 剥掉内部标记再上抛（客户端只见原错误）。
                Err(anyhow::anyhow!("{}", pool_err.trim_start_matches(": ")))
            }
        }
    }

    /// MCP「无号直连」：不经过 `acquire_context` 的选号门槛，用池里任意带 Kiro
    /// token 的凭据直接打 MCP 端点。
    ///
    /// # 为什么必须有（P0，W8 诊断的 websearch 结构性缺陷）
    ///
    /// 快路径 MCP 调用此前**硬依赖 Kiro 池号**：`acquire_context` 的选号要求凭据
    /// 过 `is_entry_selectable`（禁用 / 冷却 / custom_api 结构性排除），纯 custom_api
    /// 透传池（线上现状：4 个代挂号）**一个号都选不到** → WebSearch 快路径恒 502。
    /// 而 MCP（web_search）调用本质只依赖一个有效的 Kiro Bearer token。
    /// token 从凭据池现取（`token_manager.acquire_mcp_direct_token`，绕过选号门槛），
    /// URL 固定 `runtime.{region}.kiro.dev/mcp`。该主机 2026-08 起要求
    /// `x-amzn-kiro-profile-arn`（无头 → 400 `profileArn is required`；
    /// `q.*.amazonaws.com/mcp` 有无 ARN 都 200）。OAuth 与
    /// [`crate::kiro::endpoint::ide::IdeEndpoint::decorate_mcp`] 同口径带头；
    /// ksk_ **永不**带头（IDE 主机 + CLI 令牌套 ARN 会 403 Invalid token）。
    ///
    /// # 边界
    ///
    /// - 纯 custom_api 池无 Kiro token → 返回 Err（调用方降级回池子错误）。
    /// - OAuth token 可能已过期（不做刷新，保持最小）→ 上游 401 → **同请求换下一个
    ///   带 Kiro token 的号**；全部试完或共享预算耗尽才降级回池子错误。
    /// - **失败短负缓存（M3）**：非 2xx（401/403/429）落 60s 负缓存，键按凭据 id
    ///   分段（复用 [`Self::dead_endpoints`]，TTL 更短），期内跳过**该号**直连
    ///   （同请求仍可换其它号；无候选才降级）。同 region 其它号不连坐。
    /// - 直连不占 inflight / 不改健康分 / 不进 endpoint 429 桶 —— 刻意：这是
    ///   「拿 token 直接打」的轻量路径（gateway 模型），不是调度路径。
    /// - URL 恒为 IDE 协议的 `runtime.{region}.kiro.dev/mcp`：MCP 端点是 IDE 协议
    ///   的（`endpoint/ide.rs`），CLI 端点的 `mcp_url` 是 `q.*` 兜底（cli.rs），
    ///   不适合直连。region 仍走 `effective_upstream_region`（白名单校验内建）。
    async fn call_mcp_direct(
        &self,
        request_body: &str,
        budget: &SharedRetryBudget,
    ) -> anyhow::Result<(reqwest::Response, u64)> {
        let mut exclude: HashSet<u64> = HashSet::new();
        let mut last_err: Option<anyhow::Error> = None;
        loop {
            let (id, cred, token) = match self
                .token_manager
                .acquire_mcp_direct_token_excluding(&exclude)
            {
                Some(v) => v,
                None => {
                    return Err(last_err.unwrap_or_else(|| {
                        anyhow::anyhow!("MCP 无号直连：凭据池无可用 Kiro token（纯 custom_api 池）")
                    }));
                }
            };
            exclude.insert(id);

            let config = self.token_manager.config();
            // M3 失败短负缓存：直连失败（401/403/429）落 60s 负缓存，键含凭据 id。
            // 先 acquire 再查，保证键对上即将发送的 token。同 region 其它号不连坐。
            let region = cred.effective_upstream_region(&config);
            if self.is_mcp_direct_blocked(id, region) {
                last_err = Some(anyhow::anyhow!(
                    "MCP 无号直连负缓存生效（{}s），跳过直连降级回池子错误",
                    MCP_DIRECT_NEG_CACHE_TTL.as_secs()
                ));
                continue;
            }
            let machine_id = machine_id::generate_from_credentials(&cred, &config);
            let rctx = RequestContext {
                credentials: &cred,
                token: &token,
                machine_id: &machine_id,
                config: &config,
                is_1m: false,
            };
            // MCP 端点是 IDE 协议的（runtime.*.kiro.dev/mcp），直连固定走它；CLI 端点的
            // mcp_url 是 q.* 兜底（cli.rs:205），不适合。region 解析与 ide 端点同源。
            let endpoint = crate::kiro::endpoint::ide::IdeEndpoint::new();
            let url = endpoint.mcp_url(&rctx);
            let body = endpoint.transform_mcp_body(request_body, &rctx);

            let client = self.client_for(&cred)?;
            let mut req = client
                .post(&url)
                .body(body)
                .header("content-type", "application/json");
            for (name, value) in Self::mcp_direct_headers(&cred, &token) {
                req = req.header(name, value);
            }

            let response = match req.send().await {
                Ok(resp) => {
                    // 共享预算：请求已真实发出，无论成败都算一次上游调用（与主循环同口径）。
                    budget.consume(1);
                    resp
                }
                Err(e) => {
                    budget.consume(1);
                    last_err = Some(e.into());
                    if budget.remaining() == 0 {
                        return Err(last_err.take().expect("just set"));
                    }
                    continue;
                }
            };
            // ⭐ 非 2xx = 直连失败（无 ARN 形态被上游拒的 403/400、token 过期的 401、
            // 429 限流等）：落 60s 负缓存（M3，期内跳过直连不再白打该号）后**同请求换下一个
            // token**，绝不把错误响应体当成功解析。
            if !response.status().is_success() {
                self.mark_endpoint_dead(&format!("mcp-direct@{}", id), region);
                last_err = Some(anyhow::anyhow!(
                    "MCP 无号直连上游响应: {}",
                    response.status()
                ));
                if budget.remaining() == 0 {
                    return Err(last_err.take().expect("just set"));
                }
                continue;
            }
            return Ok((response, id));
        }
    }

    /// MCP 无号直连的请求头（纯函数，单测定死 OAuth 带头 / ksk_ 永不带）。
    ///
    /// runtime MCP 要 `x-amzn-kiro-profile-arn`。OAuth 用
    /// [`KiroCredentials::effective_profile_arn`]（与 `decorate_mcp` 同口径，
    /// 含 idc/social 缺 ARN 时的 BuilderId 占位）。ksk_ 即使库里有 `profile_arn`
    /// 也不发：直连固定 IDE 主机，CLI 令牌套 ARN 会 403。
    /// tokentype 与 decorate_mcp 同口径：api_key → API_KEY / external_idp → EXTERNAL_IDP。
    fn mcp_direct_headers(cred: &KiroCredentials, token: &str) -> Vec<(&'static str, String)> {
        let mut headers = vec![
            ("x-amzn-codewhisperer-optout", "false".to_string()),
            ("Authorization", format!("Bearer {}", token)),
        ];
        // ksk_ 必须先于 effective_profile_arn：后者在库里已有 ARN 时会原样返回。
        if !cred.is_api_key_credential() {
            if let Some(arn) = cred.effective_profile_arn() {
                headers.push(("x-amzn-kiro-profile-arn", arn));
            }
        }
        if cred.is_api_key_credential() {
            headers.push(("tokentype", "API_KEY".to_string()));
        } else if cred.is_external_idp_credential() {
            headers.push(("tokentype", "EXTERNAL_IDP".to_string()));
        }
        headers
    }

    /// 预判 custom_api 透传**本跳**改写后发给上游的模型名（`PassthroughMeta.mapped_model` 口径）。
    ///
    /// 必须与 `passthrough::forward` 内部的改写链逐位一致。deepseek 归一化已移除
    /// （2026-08-16），全局模型映射是**唯一**改写源（`config.model_mapping`，豁免号跳过）：
    /// forward 的顺序（passthrough.rs body 处理链）为「非豁免 → `map_target`（原始名）」，
    /// 此处复用同一判定本体与同一豁免判据，与改写层不可能出现口径分裂。
    ///
    /// 返回值：改写发生（映射命中）→ `Some(最终上游名)`；未改写 → `None`（消费端回落
    /// 原始名，对齐 `usage::record::RequestRecord::upstream_model` 语义）。forward 在 JSON
    /// 解析失败时零改写；该场景到不了这里（调用方拿到的 `model` 必来自已解析成功的 payload，
    /// forward 二次解析同一字节流必然成功），由「未改写 → None」分支保守覆盖。
    fn predict_passthrough_upstream_model(
        model: Option<&str>,
        cred: &KiroCredentials,
        mapping_rules: &std::collections::HashMap<String, String>,
    ) -> Option<String> {
        let m = model?;
        // 全局映射（豁免时跳过，与 forward 的 exempt 分支一致）。
        let final_model = if cred.model_mapping_exempt == Some(true) {
            m.to_string()
        } else {
            crate::kiro::model_mapping::map_target(m, mapping_rules)
                .unwrap_or_else(|| m.to_string())
        };
        // 未改写（最终名 == 原始名）→ None，消费端回落原始名。
        if final_model != m {
            Some(final_model)
        } else {
            None
        }
    }

    /// 透传池失败冷却决策：按上游状态码返回 `(冷却秒数, 冷却原因)`。
    ///
    /// # 秒数（既有调参，2026-08-10 定，dwgx 语义）
    ///
    /// 代挂号是用户自购的付费中转站,不是 Kiro 号,它没有"被风控"这个状态,429 只代表
    /// "它现在忙"。429 原先给 30s 冷却——那是把 Kiro 号的风控模型错套到代挂号上:
    /// 用户已经为这个上游付过钱,把它按下 30 秒既不能让它变快,又白白缩小了可用池
    /// (极端情况:两个代挂号轮流 429 → 两个都被冷却 → 整池不可用 → 回落 Kiro,
    ///  而 Kiro 侧此刻可能正被风控烧号)。偶尔 429 只该 failover,不该留痕。
    ///
    /// - `401|402|403` = 180s：**非瞬态**,短期内重试必然还是失败。给冷却是为了别让
    ///   同一请求链外的后续请求继续撞它。代挂号**绝不自动禁用**:record_passthrough_result
    ///   只记观测计数,180s 冷却只是调度级跳过,管理员设置的 enabled 状态永不被改写。
    /// - `429` / `400|404` = 5s：瞬态/站点属性,给一个**极短**的调度级跳过,而不是零。
    ///
    ///   为什么不是 0（审查发现的延迟回归）：`excluded` 只在**本请求链内**生效,
    ///   跨请求不起作用。若完全不冷却,一个 100% 拒绝的中转站会被**每一个**新请求
    ///   重新选中(select_custom_api 按 priority/RPM 排序,它排在前面),每次都白付一次
    ///   上游往返才 failover——若不跳过,每个新请求都会多等一个失败 RTT(代挂号**没有**
    ///   自动禁用兜底,只能靠这 5s 调度级跳过稀释同一秒内撞向同一个忙站的频率)。
    ///   5s 是刻意取的平衡点:它**不是**惩罚(不进 health、不计失败、不影响自动禁用判据),
    ///   只是调度上避免同一秒内把所有请求都撞向同一个忙站;而 5s 远低于人可感知的
    ///   池容量缩水(旧值 30s 才是真正的惩罚性退避)。
    ///
    ///   400/404 与 429 同列：该上游对**这类请求**不认(模型不支持 / tool 配对更严 /
    ///   role 白名单更严),是它的稳定属性而非抖动,短期内同类请求还会失败。但绝不给
    ///   长冷却:换个模型的请求它可能就认,长冷却会白丢池容量。404 与 400 同性质
    ///   (k2cc 用 400 INVALID_MODEL_ID、denzao 用 404 model_not_found,只是不同站点
    ///   表达「本站不认这个请求」的不同状态码)。
    /// - `5xx` = 5s（2026-08-16 S4 起）：与 429 同档调度级跳过 + 原因标签 `ServerError`。
    ///   行为变化:5xx 此前完全不冷却(仅排序键余温软降权);5s 硬跳过与其互补——余温
    ///   只在排序键平局时生效,5s 硬跳过保证该号 5s 内绝不重选,死号恒 502 时仍由
    ///   余温承担 60s 降权。
    /// - `_`（网络错误/其它）= 0：真瞬态,不跳过,仅记失败余温。
    ///
    /// # 原因（2026-08-16 S4「透传池冷却标签独立」）
    ///
    /// 此前所有冷却统一打 `RateLimitExceeded` 标签 ⇒ 401/402/403 在面板显示「速率限制」,
    /// 误导排障(W14 实测确认)。现按语义映射,原因只决定面板 `cooldownReason`/
    /// `cooldownCode`(admin service 读 `CooldownInfo.reason` 下发,前端 i18n 走 code):
    ///
    /// | 状态码 | 秒数 | CooldownReason | 面板标签 |
    /// |---|---|---|---|
    /// | 401 / 403 | 180 | `AuthTransient` | 认证瞬态失败 |
    /// | 402 | 180 | `QuotaExhausted` | 配额耗尽 |
    /// | 429 | 5 | `RateLimitExceeded` | 速率限制(保留) |
    /// | 400 / 404 | 5 | `RateLimitExceeded` | 速率限制(现状保留) |
    /// | 500-599 | 5 | `ServerError` | 服务器错误 |
    /// | 其它 | 0 | 无(不冷却) | — |
    ///
    /// ⚠️ **秒数不走 `CooldownReason::default_duration()`**：时长是这里显式给出的既有
    /// 调参,原因只决定标签——401 用 `AuthTransient` 后仍冷却 180s(不是该变体的 20s
    /// 默认值),不会因换标签而改变时长。
    ///
    /// 不变式:返回的秒数 > 0 ⟺ 原因 = Some(调用点据此 expect)。
    fn passthrough_cooldown_for(code: u16) -> (u64, Option<CooldownReason>) {
        match code {
            401 | 403 => (180, Some(CooldownReason::AuthTransient)),
            402 => (180, Some(CooldownReason::QuotaExhausted)),
            429 => (5, Some(CooldownReason::RateLimitExceeded)),
            400 | 404 => (5, Some(CooldownReason::RateLimitExceeded)),
            (500..600) => (5, Some(CooldownReason::ServerError)),
            _ => (0, None),
        }
    }

    /// 混入池分流:选一次号,若命中「自定义 API」凭据则原样透传原始 Anthropic 请求体到其上游、
    /// 返回 `Some(透传响应)`;若选到 Kiro 号(或无自定义号)则返回 `None`,由调用方走原 Kiro 路径。
    ///
    /// ⚠️ 与 Kiro 主路径隔离:本方法只在选到 custom_api 时接管;选到 Kiro 号时**立即释放**
    /// (drop inflight 守卫)并返回 None,不影响后续 Kiro 正常选号/转发。`raw_body` 是**未经
    /// Kiro 转换**的客户端原始请求体(透传要原样发)。
    ///
    /// `model` 供选号做模型过滤/亲和(与 Kiro 路径同源解析);命中自定义号时记一次请求(上限计数)。
    pub async fn try_custom_api_passthrough(
        &self,
        raw_body: bytes::Bytes,
        model: Option<&str>,
        user_id: Option<&str>,
        // 客户端请求头（P3：按白名单转发 `anthropic-beta` 等）。透传成功时用得上。
        client_headers: Option<&axum::http::HeaderMap>,
        retry_budget: &SharedRetryBudget,
    ) -> Option<(axum::response::Response, PassthroughMeta)> {
        // 从**custom_api 专属选号池**里 failover 调度(独立于 Kiro 选号,守两池隔离铁律)。
        // 语义(dwgx 定):池内按优先级+RPM 均衡选号;某号 403 额度满/401 key 失效/429/5xx →
        // 给该号短冷却 + 换下一个 custom_api;全部 custom_api 不可用 → 返回 None,由上层落 Kiro 主力路径。
        // 4xx(非 403,客户端请求错误)→ 换号也一样错,直接把该响应返给客户端(不 failover、不落 Kiro)。
        // 注:model/user_id 暂不参与 custom_api 选号(代挂上游自行处理模型),仅随 meta 供埋点关联。
        // 全局模型映射规则：循环外快照一次（与 Kiro 主路径同约定），透传各跳共用同一份。
        let mapping_rules = self.token_manager.config().model_mapping.clone();
        // 本调用实际改写后的模型名（成功/失败返回的 PassthroughMeta 都用它；
        // None = 未命中映射 / 凭据豁免）。
        let mut mapped_model: Option<String> = None;
        let mut excluded: HashSet<u64> = HashSet::new();

        // 🔴 P1：给透传 failover 循环加**墙钟预算**（主路径早就有了，透传漏了）。
        //
        // 改前这个 loop 无任何时间上限。每个 `forward` 带 30s connect_timeout —— 上游
        // 全挂时：每个号先烧 30s 再换下一个，N 个号就是 N×30s 的最坏等待，而客户端
        // 早已超时断开。叠上 sub2api 侧的重试 × 账号切换，`TASK-BUILTIN-RETRY.md` 记录
        // 单请求最坏放大到 ~70~108 次上游调用。
        //
        // 🔴 **透传墙钟不能直接复用主路径的 45s**（2026-08-10 修同日引入的不一致）。
        //
        // 原写法 `MAX_REQUEST_RETRY_BUDGET_SECS`(=45)。但同日把透传首字节超时从 30s
        // 放宽到 **90s**（`passthrough.rs::FIRST_BYTE_TIMEOUT_SECS`，理由见那里：
        // 30s 恰好落在上游响应头延迟的 p90 上，砍掉了一成正常请求）之后，
        // 45s < 90s 就产生了真实的逻辑冲突：
        //
        // **首跳耗时 50s 失败 ⇒ 墙钟已过 ⇒ 第二个号一次都不会试** —— 换号能力被
        // 静默废掉，而"多号互为备份"正是这个循环存在的理由。
        //
        // 所以墙钟必须容纳「至少一次完整的首字节超时 + 一次换号后的再次尝试」：
        // 取 `FIRST_BYTE_TIMEOUT_SECS × 2 + 30s 余量`。两个尺度从此由同一个源头推导，
        // 改任一个另一个自动跟随（避免这次这种"改了一个忘了另一个"再发生）。
        //
        // ⚠️ 为什么不反过来把首字节超时压回 45s 以内：那等于回到砍掉一成请求的旧状态
        // （实测 44 条请求在 30.7s 后才出响应头但最终 200 成功）。
        // 上游慢是既定事实，网关该做的是容纳它，而不是按自己的时间表掐断。
        //
        // ⚠️ 客户端侧不会因此干等更久：吸收层（`upstreamRetryAbsorb*`）在更外层管
        // 「客户端总共等多久」，本墙钟只管「单轮透传内部最多换号多久」。
        const PASSTHROUGH_WALL_SECS: u64 =
            crate::kiro::passthrough::FIRST_BYTE_TIMEOUT_SECS * 2 + 30;
        let budget = std::time::Duration::from_secs(PASSTHROUGH_WALL_SECS);
        let wall_deadline = std::time::Instant::now() + budget;
        let mut started;
        // 🔴 **真正打到上游的次数**（不含被并发闸挡住的空转），受
        // [`MAX_PASSTHROUGH_FAILOVER_HOPS`] 约束。
        //
        // 为什么墙钟不够、必须再加次数闸：墙钟只在**每轮进循环时**判（见下方 `wall_deadline`
        // 检查），所以最后一跳可以在墙钟边界之前刚好进来、然后独自跑到 `read_timeout`
        // 720s。⇒ 单请求的真实上界不是 210s，而是「210s + 最后一跳的 720s」，且中间能打的
        // 上游次数无上限。次数闸把这个上界压到常数级。
        let mut upstream_hops: usize = 0;
        // ⭐ 本链最先尝试的凭据 ID（N4 首选号）：首次 `select_custom_api` 成功后置位，
        // 随 PassthroughMeta 供成功链的 usage record；共享预算携带同一份（见
        // `SharedRetryBudget::note_first_attempt` 注释）。
        let mut first_attempted_id: Option<u64> = None;
        // 🔴 **闸门空转次数，与 `upstream_hops` 分开计**（2026-08-10）。
        //
        // 为什么必须分开：两个约束彼此冲突，合用一个计数器无法同时满足 ——
        // ① 守卫测试 `passthrough_loop_must_have_concurrency_and_hop_gates` 要求
        //    hop 累加语句出现在 `passthrough::forward` **之后**，理由是
        //    （⚠️ 这里刻意不写那条语句的字面量：守卫用 `code.find()` 取**第一个**出现位置，
        //     注释里出现字面量会让它命中注释而非真实代码，断言随即失效）
        //    「闸门挡住的空转不该吃掉真正换号的配额，池子越大越早耗尽配额而一次上游都没打成」；
        // ② 但闸门空转**确实消耗资源**：走到闸门时 `select_custom_api` 已经占位
        //    （inflight+1 + `rpm.record`）。完全不计数会让 N 号池闸门全满时，
        //    一个请求对 N 个号各**虚记**一次 RPM 却一次上游都没打 ——
        //    而 `rpm.count` 是 `observed_upstream_rpm` 的输入、被线上 `throttle-autotune`
        //    每 2 分钟用来调 `inboundTargetRpm` ⇒ 污染自动调参。
        //
        // 分开后：`upstream_hops` 只数真实上游调用（守卫的语义不变），
        // `gate_skips` 只数闸门空转并有自己的上限，两者都不会无界。
        let mut gate_skips: usize = 0;
        // 上限取 hop 上限的 2 倍：闸门瞬时满是正常现象（许可会在数百 ms 内周转），
        // 允许比换号更宽松地重试；但必须有界，否则并发高峰下会在循环里空转到墙钟耗尽。
        const MAX_GATE_SKIPS: usize = MAX_PASSTHROUGH_FAILOVER_HOPS * 2;

        // ⭐ 共享预算次数闸（2026-08-11 方案 A）：透传此前只受
        // `MAX_PASSTHROUGH_FAILOVER_HOPS=6` 独立约束——主路径先试透传再落 Kiro，
        // 两层各自拿满配额同样是跨层放大源。现与 Kiro 主路径共用「每请求」总额度：
        // 实际换号上限 = min(透传独立上限, 预算剩余)。预算耗尽后即使落 Kiro 主路径，
        // 主路径也拿不到配额（同一预算），请求整体停止换号——这正是「每请求 ≤4 次
        // 上游」的完整语义。
        //
        // ⚠️ 必须**进循环前快照一次**（对抗审查 MAJOR，2026-08-11）：若在 `loop` 内
        // 每轮重算，第 N 跳后 hop_cap = remaining − N，停止判据 `upstream_hops >= hop_cap`
        // 变成 N ≥ R0−N ⇒ 预算 4 只打 2 跳——换号覆盖面腰斩、浪费额度（「换站即成功」
        // 的常态场景只试一半号）。快照语义与主路径 `round_retry_quota`（进轮算一次）
        // 镜像一致：预算在换号过程中花掉，但停止上限取**进循环时的剩余**。
        // 同号吸收策略在进循环前快照一次（2026-08-11 对抗审查 m3：此前在外层 loop 内
        // 每跳重读，admin 热更配置时同一条请求的不同跳按不同开关/退避走，行为不可复现；
        // 与主路径 AbsorbPolicy 的「一次调用内只取一份策略」约定一致）。
        let absorb_cfg = self.token_manager.config();
        let absorb_retry_enabled = absorb_cfg.upstream_retry_absorb_enabled;
        let absorb_retry_server_error = absorb_cfg.upstream_retry_absorb_server_error;
        // 400 容量类开关（2026-08-13 补齐：此前只消费上面 5 个旋钮，透传吸收对 400 容量
        // 类永远不生效——与主路径语义有差距）。suspended/swap_budget_secs 刻意不接入：
        // 代挂号的 403 是「额度满」而非主路径的「账号被风控」语义（见下方 403 冷却 180s
        // 的说明），套主路径那套长阶梯只会拖慢换号；budget_secs 由外层 failover 的共享
        // 墙钟/预算代理（同号重试不击穿 hop_cap，见循环内注释）。
        let absorb_retry_capacity_400 = absorb_cfg.upstream_retry_absorb_capacity_400;
        let absorb_max_rounds = absorb_cfg.upstream_retry_absorb_max_rounds;
        let absorb_min_delay_ms = absorb_cfg.upstream_retry_absorb_min_delay_ms;
        let absorb_max_delay_secs = absorb_cfg.upstream_retry_absorb_max_delay_secs;
        let hop_cap = MAX_PASSTHROUGH_FAILOVER_HOPS.min(retry_budget.remaining() as usize);
        loop {
            // 次数闸：已打满上限 → 停止换号（与墙钟同样返 `None`，语义一致）。
            // ⚠️ 放在墙钟检查之前：两者都是「本请求不再换号」，先判更便宜的那个。
            if upstream_hops >= hop_cap {
                tracing::warn!(
                    tried = excluded.len(),
                    hops = upstream_hops,
                    max_hops = hop_cap,
                    "custom_api 透传已达最大换号次数（共享预算约束），停止换号并落 Kiro 主力路径"
                );
                return None;
            }
            // 闸门空转闸：与上面的换号次数闸分开（理由见 `gate_skips` 声明处）。
            // 不判它的话，并发高峰下「选号 → 闸满 → continue」会一直空转到墙钟耗尽，
            // 且每轮都白记一次 rpm。
            if gate_skips >= MAX_GATE_SKIPS {
                tracing::warn!(
                    tried = excluded.len(),
                    gate_skips,
                    max_gate_skips = MAX_GATE_SKIPS,
                    "custom_api 透传因并发闸持续满载而空转过多，停止换号并落 Kiro 主力路径"
                );
                return None;
            }
            // 预算耗尽 → 停止换号，返回 `None` 落 Kiro 主力路径。
            //
            // 为什么返 None 而不是把最后一个失败响应抛给客户端：`None` 是这个函数既有的
            // 「透传不可用，交给 Kiro」信号（见下方 select_custom_api 返 None 的两个分支），
            // 复用它能保证行为一致 —— 客户端仍有机会被 Kiro 主路径服务，而不是直接吃错误。
            // 主路径自己也有 45s 预算与 `ABSOLUTE_MAX_TOTAL_RETRIES`(=4) 次上限，不会再无限放大。
            if std::time::Instant::now() >= wall_deadline {
                tracing::warn!(
                    tried = excluded.len(),
                    budget_secs = PASSTHROUGH_WALL_SECS,
                    "custom_api 透传 failover 超过墙钟预算，停止换号并落 Kiro 主力路径"
                );
                return None;
            }
            // 第三项 `inflight_guard` 是**选号时的原子占位**（2026-08-10 补）：
            // 它必须活到本次上游调用结束（Drop 即 inflight-1）。
            //
            // 为什么承重：改前 `select_custom_api` 只返回 `(id, cred)`、不占位 ⇒ 代挂号的
            // inflight 恒为 0 ⇒ 排序键「在途」那一维结构性失效（同优先级同 RPM 时恒压第一个
            // 号）；且 `rpm.record` 要等上游返回后才发生 ⇒ 选号到记账之间一整个 RTT 的惊群窗口。
            //
            // ⚠️ **不要把它绑成 `_`**（`let (id, cred, _) = ...`）：那会让 guard 当场 Drop，
            // inflight 立刻减回去，整个修复失效且不会有任何编译错误提示。
            // 成功路径把它移交给 `PassthroughMeta`（随响应流存活），失败路径由循环下一轮
            // 覆盖变量时自然 Drop。
            let (id, cred, inflight_guard) =
                match self
                    .token_manager
                    .select_custom_api_or_wait(&excluded, model)
                    .await
                {
                    Some(x) => x,
                    // 无更多可用 custom_api 号:
                    // ①一开始就没(excluded 空)→ 池里无透传号,零开销落 Kiro;
                    // ②都试过失败(excluded 非空)→ custom_api 全额度满/失败,failover 落 Kiro;
                    // ③纯代挂池全部 CREDENTIAL_MAX_CONCURRENCY → or_wait 已短等重试,仍 None。
                    // 混池 custom 满且有可选 Kiro：or_wait 立刻 None（分流），不睡。
                    None => return None,
                };
            // ⭐ 首选号（N4 可观测）：本链最先选中的号即「首选」。写两份 —— 本链的
            // `first_attempted_id` 随 PassthroughMeta 供成功链的 usage record；跨层共享
            // 预算携带同一份（首写生效），供「透传全败 → 落 Kiro 主路径」的 fail_record
            // —— handlers 先试透传再落 Kiro，预算里就是整条链真正最先尝试的号。
            if first_attempted_id.is_none() {
                first_attempted_id = Some(id);
                retry_budget.note_first_attempt(id);
            }
            started = std::time::Instant::now();
            // 透传路径的改写链在 forward 内部（仅全局模型映射；deepseek 归一化已移除）。
            // 改写是否真的发生由 forward 判断（JSON 解析失败等情况下不会改写）；这里按
            // 凭据豁免与映射规则预判 mapped_model，仅用于 PassthroughMeta 埋点。
            // 🔴 修复（2026-08-11 全量审计）：每跳**重算并重置** —— 旧代码只在命中时覆盖、
            // 未命中/豁免时保留上一跳的值。混合豁免/非豁免 custom_api 号池 failover 后
            // （第 1 跳非豁免命中映射、第 2 跳豁免原样转发），最后一跳的 PassthroughMeta
            // 仍带旧跳的映射名，与实际服务模型不符。现改为每跳先归 None 再按本跳凭据重算。
            mapped_model = Self::predict_passthrough_upstream_model(
                model, &cred, &mapping_rules,
            );
            // 🔴 **全局上游并发闸**（2026-08-10 补：透传路径此前完全绕过它）。
            //
            // 与主路径同一个 `upstream_gate` 语义：限制**同时在飞**的上游 HTTP 调用总数。
            // 透传此前无任何并发限制，而线上 100% 流量走透传 ⇒ 这道闸对当前流量是**唯一**
            // 的全局并发保护。
            //
            // ⚠️ 与主路径的差异：满时 **`continue` 换号**而非 `break`。
            // 理由：主路径 break 是「放弃本轮、交给吸收层」，而透传 break 会 `return None`
            // 把请求打回 Kiro 主路径 —— 当前池里没有 ksk 号，那等于必然报错。
            // 换号则可能命中另一个号（下面的每凭据闸更可能有余量），是更优处置。
            // 与每凭据闸的 `continue` 语义一致。
            //
            // ⚠️ **permit 的生命周期与主路径刻意一致**（别"顺手"延长它）：许可在
            // `return Some((resp, meta))` 时随作用域 Drop，即这道闸限制的是「同时在等**响应头**
            // 的调用数」，**不是**「同时在传输的流数」。主路径同款（见 `:2143` 注释「响应头
            // 拿到后离开本作用域自动 Drop 释放」），在飞的流由 `CallMeta.inflight` 的
            // `InflightGuard` 单独跟踪。
            // ⚠️ 透传路径目前**没有** inflight 等价物（这是另一个已知缺口：`select_custom_api`
            // 的排序键读 inflight/rpm 却不在选号时占位 ⇒ 惊群），故此处不要试图用 permit
            // 去补那个洞 —— 那会把「等响应头」与「传输中」两个口径混成一个，两边都不准。
            // 🔴 **闸门满时必须"等"，不能"排除该号"**（2026-08-10 修回归）。
            //
            // 上一版写的是 `try_acquire` 失败 → `excluded.insert(id)` + `continue`，
            // 那在**单号池**上直接制造 429：池里只剩一个可用号时，把它排除掉
            // ⇒ `select_custom_api` 返 None ⇒ `return None` ⇒ 落 Kiro ⇒ 无 ksk 号 ⇒ 429，
            // 且 trace 里 **`credential_id` 为空**（请求在选号阶段就被挡死，一次上游都没打）。
            // 线上实测：1305 的 `inflight=6` / 每凭据闸容量 8，并发一冲高就复现。
            //
            // 正确语义：并发闸是**削峰**（让请求排队等许可），不是**丢弃**。许可由前面的
            // 请求在拿到响应头后释放，通常几百毫秒内就有；等一下远好过让客户端吃 429。
            //
            // 🔴 **但等待上限必须是独立的短值，不能取「墙钟剩余」**（2026-08-10 对抗评审抓出）。
            //
            // 上一版写 `gate_wait = 墙钟剩余`（=210s），有两个叠加后致命的后果：
            // ① **单次等待就能吃光整个墙钟** ⇒ `continue` 后顶部墙钟检查 `return None`
            //    ⇒ 落 Kiro ⇒ 纯代挂池无 ksk 号 ⇒ 429。客户端**等 210 秒**才拿到错误，
            //    比它要解决的「当场断会话」更糟（Claude Code 的 HTTP 超时远短于 210s，
            //    实际表现为客户端超时断连）。
            // ② 这 210s 里该请求**已持有 `select_custom_api` 的占位**（inflight+1），
            //    反而加剧闸门拥塞 —— 自我强化的正反馈。
            //
            // 取 `GATE_WAIT_MAX` = 3s：许可在拿到响应头后即释放（实测代挂上游 p50 12.7s
            // 但响应头通常几百 ms 到数秒），3s 足够跨过一次正常的许可周转；而超时后
            // **换号**（`excluded` + hop 累加）比继续死等同一个满载的闸更可能成功。
            // 同时 3s × 6 跳 = 18s 最坏，仍远小于墙钟，不会挤掉真正的换号预算。
            const GATE_WAIT_MAX: std::time::Duration = std::time::Duration::from_secs(3);
            let gate_wait = wall_deadline
                .saturating_duration_since(std::time::Instant::now())
                .min(GATE_WAIT_MAX);
            let _gate = match tokio::time::timeout(
                gate_wait,
                self.upstream_gate.clone().acquire_owned(),
            )
            .await
            {
                Ok(Ok(permit)) => permit,
                // 等不到许可 → **换号**而不是死等：全局闸满说明系统整体在飞请求多，
                // 但换个号可能命中另一个每凭据闸有余量的号（下面那道闸更可能放行）。
                // ⚠️ 必须 `excluded.insert` + 累加 hop，否则这条路径会
                //    ①反复选中同一个号空转 ②每次白记一次 rpm（污染 autotune 输入）。
                Ok(Err(_)) | Err(_) => {
                    tracing::warn!(
                        credential_id = id,
                        waited_ms = gate_wait.as_millis(),
                        "透传全局并发闸等待超时（系统满载），换下一个 custom_api 号"
                    );
                    excluded.insert(id);
                    gate_skips += 1;
                    continue;
                }
            };

            // 🔴 **每凭据并发闸**（同上，透传此前也绕过）。
            //
            // 没有它时的真实故障形态：某个中转站响应慢（上游排队而非立刻 429），它的请求
            // 长时间占着全局许可 → 极端情况下全部许可被同一个慢站吃掉 → 其余健康站拿不到
            // 许可，**整池吞吐被一个站拖死**，而症状表现为系统级「并发闸已满」，指不到是哪个号。
            //
            // 🔴 满时的处置要**看还有没有别的号可换**（2026-08-10 修回归）：
            //
            // ① 池里还有其它可用号 → `excluded.insert(id)` + `continue` 换号。本号已打满，
            //    换下一个是最佳处置（另一个号的闸大概率空着）。`excluded.insert` 必不可少：
            //    否则 `select_custom_api` 会再选中它（按 priority/rpm 排序，打满的号 rpm
            //    未必高、可能排更前）→ 无 sleep 空转。
            // ② **池里就这一个号** → 必须**等许可**，绝不能排除它。
            //    上一版无条件走 ① ⇒ 单号池上把唯一的号排除掉 ⇒ select 返 None ⇒ 落 Kiro
            //    ⇒ 无 ksk 号 ⇒ **429 且 trace 的 credential_id 为空**（一次上游都没打）。
            //    实测线上 1305 `inflight=6` / 闸容量 8，并发一冲高即复现 —— 这是纯回归，
            //    改前（无闸门）反而不会 429。
            //
            // 判据用 `excluded` 而非池大小：走到这里 `excluded` 里是「本请求已试过的号」，
            // 若把当前号也排除后就无号可选，那就属于情形 ②。
            // ⚠️ 必须用 `has_other_custom_api_candidate`（只探测）而非 `select_custom_api`
            // （会占位）—— 后者每次探测都白白 inflight+1 + rpm.record，直接污染这两个计数。
            //
            // ⚠️ **惰性求值**：探测要加 `entries` 锁并跑完整过滤链（含 deepseek 白名单感知），
            // 而闸门**绝大多数时候不满** —— 无条件先算等于给热路径白加一次锁竞争。
            // 所以先 `try_acquire`，只在它真的失败时才探测。
            // （不写成闭包是因为闭包会借用 `excluded`，而失败分支里要 `excluded.insert`，
            //  借用检查过不去。）
            let cred_permit = self.per_credential_gate(id).try_acquire_owned();
            let _cred_gate = match cred_permit {
                Ok(permit) => permit,
                Err(_)
                    if {
                        let mut probe = excluded.clone();
                        probe.insert(id);
                        self.token_manager
                            .has_other_custom_api_candidate(&probe, model)
                    } =>
                {
                    tracing::debug!(
                        credential_id = id,
                        limit = self.upstream_per_credential_limit,
                        "透传凭据级并发闸已满，换下一个 custom_api 号"
                    );
                    excluded.insert(id);
                    // ⚠️ 必须计数（2026-08-10 对抗评审抓出）：这条路径每次都已消耗一次
                    // `select_custom_api` 占位（inflight+1 + `rpm.record`）却一次上游都没打。
                    // 不累加的话，N 号池闸门全满时一个请求会对 N 个号各**虚记**一次 RPM，
                    // 而 `rpm.count` 是 `observed_upstream_rpm` 的输入、被线上
                    // `throttle-autotune` 每 2 分钟用来调 `inboundTargetRpm`
                    // ⇒ 直接污染自动调参（CLAUDE.md 已记「容量口径是假的」这个历史坑，
                    // 别在同一个口径上再加一层虚数）。
                    gate_skips += 1;
                    continue;
                }
                // 唯一可用号且闸已满：短等一下许可（削峰），等不到就放弃本请求的换号。
                //
                // 🔴 **上限同样必须是独立短值**（2026-08-10 对抗评审抓出）。原写「墙钟剩余」
                // 比全局闸那处更危险，因为这个等待发生在**已持有全局许可之后**
                // （`_gate` 在上面绑定，作用域覆盖整个循环体）⇒ 等待者攥着全局许可不放
                // ⇒ 16 个全局许可可被「正在等每凭据许可」的请求全部占死
                // ⇒ 新请求连全局闸都进不去 ⇒ 整池对外表现为**完全无响应 210s 后集体 429**。
                //
                // 复用同一个 `GATE_WAIT_MAX`(3s)：两道闸的等待预算由同一常量约束，
                // 嵌套最坏 3+3=6s，不会出现「攥着上游许可长时间空等」的塌陷。
                Err(_) => {
                    let wait = wall_deadline
                        .saturating_duration_since(std::time::Instant::now())
                        .min(GATE_WAIT_MAX);
                    match tokio::time::timeout(wait, self.per_credential_gate(id).acquire_owned())
                        .await
                    {
                        Ok(Ok(permit)) => {
                            tracing::debug!(
                                credential_id = id,
                                "唯一可用号的凭据闸已满，等到许可后继续（未丢弃请求）"
                            );
                            permit
                        }
                        Ok(Err(_)) | Err(_) => {
                            tracing::warn!(
                                credential_id = id,
                                waited_ms = wait.as_millis(),
                                "唯一可用号的凭据闸等待超时，停止换号"
                            );
                            // 同上：已消耗一次占位却没打上游，必须计数防空转 + 防虚记。
                            // 这里**不** `excluded.insert`：走到本分支说明它是唯一可用号，
                            // 排除它只会让 `select_custom_api` 立刻返 None（等价于放弃），
                            // 而 gate_skips 闸已足够终止循环。
                            gate_skips += 1;
                            continue;
                        }
                    }
                }
            };

            // 第三项 `upstream_err` 是**非 2xx 时上游的错误体原文**（成功恒空串）。
            // 用它把笼统的 400/502 分成「换号可能有救」与「换号也一样错」两类。
            //
            // 🔴 2026-08-11 对抗审查修：同号吸收的判据/退避/预算全部改读配置
            // （upstream_retry_absorb_*），不再硬编码；429 只跟总开关、5xx 需
            // server_error 也开（与 Kiro 主路径语义一致）；本地失败（connect_error
            // 前缀 / 空错误体）绝不重试；同号重试不击穿外层 failover 的墙钟与跳数上限。
            // 策略在进循环前快照一次（本仓「一次调用内只取一份策略」约定）。
            //
            // ⚠️ 已知取舍（默认关，运维显式开启时适用）：退避 sleep 期间仍持有全局/
            // 每凭据并发闸许可（绑定在外层循环体作用域），高并发下略压缩全局并发槽；
            // 429 重试不读取上游 Retry-After（退避被 max_delay_secs 夹住，最坏 15s）。
            //
            // ⚠️ `upstream_retry_absorb_exhausted_status` 对透传路径**不生效**（如实标注，
            // 2026-08-13）：主路径耗尽时由 provider 打 `absorb_budget_exhausted=1` 标记、
            // handlers 据此渲染 503；而透传路径的失败出口只有两个 —— 4xx 直返（回上游
            // 原始响应体，不经错误渲染链）与全部号失败后落 Kiro 主路径（终态错误由主路径
            // 构造，且只记主路径自己的轮次）。透传吸收耗尽的终态语义由 Kiro 主路径决定，
            // 这里没有可打标记的错误串出口，硬构造 503 响应改动大且违背「透传返原样」。
            let mut same_cred_attempt: u32 = 0;
            let (resp, status, upstream_err) = 'retry_same_cred: loop {
                let (resp, status, upstream_err) = crate::kiro::passthrough::forward(
                    &cred,
                    raw_body.clone(),
                    self.global_proxy.as_ref(),
                    self.tls_backend,
                    &mapping_rules,
                    client_headers,
                ).await;
                // 每次 forward 调用都是一次真实上游请求（含同号重试）。
                upstream_hops += 1;
                // 共享预算扣减（2026-08-11 方案 A）：forward 已真实发出。
                retry_budget.consume(1);
                let code = status.as_u16();
                // 本地失败不重试：`connect_error:` 前缀 = 传输层失败（与主路径
                // upstream_retry_absorb_server_error 文档「排除传输层」同语义）；
                // 空错误体 = 缺 base_url / client 构建失败（确定性本地错误）。
                // 代价：上游真返的空体 5xx 不被吸收 —— 漏吸收一次优于把本地故障
                // 放大 N 遍；空体 429 走下方 failover 冷却，即改前行为，安全。
                let local_failure = upstream_err.is_empty()
                    || upstream_err.starts_with("connect_error:");
                if passthrough_absorb_should_retry(
                        code, local_failure, absorb_retry_enabled, absorb_retry_server_error,
                        absorb_retry_capacity_400, &upstream_err,
                        same_cred_attempt + 1, absorb_max_rounds)
                    // 墙钟/跳数闸：同号重试只应在预算内进行，不得击穿外层
                    // 「真正打到上游的次数受 hop_cap（= min(MAX_PASSTHROUGH_FAILOVER_HOPS,
                    // 共享预算剩余)）」的承诺（对抗审查抓出：改前内层用
                    // MAX_PASSTHROUGH_FAILOVER_HOPS 常量，预算已被外层烧完时同号循环仍
                    // 可连打至 max_rounds 次——单请求击穿「每请求 ≤4」；2026-08-11 修复）。
                    && upstream_hops < hop_cap
                    && std::time::Instant::now() < wall_deadline
                {
                    same_cred_attempt += 1;
                    // 埋点（2026-08-13 补，此前透传吸收零计数）：`bump_absorb_round` 与
                    // 主路径同款「真睡完退避并重打了一轮」。透传流量占池大头时（全代挂号），
                    // 缺这组数会让面板的吸收比与真实行为脱节。
                    crate::common::recovery_metrics::bump_absorb_round();
                    let ms = passthrough_absorb_delay_ms(
                        same_cred_attempt, absorb_min_delay_ms, absorb_max_delay_secs);
                    let delay = std::time::Duration::from_millis(ms);
                    tracing::warn!(
                        credential_id = id,
                        status = code,
                        attempt = same_cred_attempt,
                        delay_ms = ms,
                        "透传 5xx/429：同号退避重试（吸收层启用）"
                    );
                    tokio::time::sleep(delay).await;
                    // 同号重试不换凭据，也不排除自己，直接重新 forward
                    continue 'retry_same_cred;
                }
                break 'retry_same_cred (resp, status, upstream_err);
            };
            let latency_ms = started.elapsed().as_millis() as u64;
            // 据上游 status 推断 outcome(与 Kiro 主路径同口径)。502 含真上游 5xx 与本地连接失败。
            let code = status.as_u16();
            let outcome = match code {
                s if (200..300).contains(&s) => crate::usage::RequestOutcome::Success,
                429 => crate::usage::RequestOutcome::RateLimited,
                402 => crate::usage::RequestOutcome::QuotaExhausted, // 中转站常用 402 表额度耗尽
                401 | 403 => crate::usage::RequestOutcome::AuthFailed,
                s if (500..600).contains(&s) => crate::usage::RequestOutcome::ServerError,
                s if (400..500).contains(&s) => crate::usage::RequestOutcome::BadRequest,
                _ => crate::usage::RequestOutcome::OtherError,
            };
            // 轻量结果计数(隔离铁律:绝不复用 report_success/failure 的 cooldown/family 连坐)。
            self.token_manager.record_passthrough_result(id, outcome);

            // 成功 → 直接返回该号的响应流。
            if (200..300).contains(&code) {
                // 可观测（2026-08-13 补）：吸收层真把一个本该 failover 的响应救回来了。
                // 与主路径同款计数器，只在真重试过时计（`same_cred_attempt > 0`），
                // 否则每个正常成功请求都会被记成「吸收成功」。
                if same_cred_attempt > 0 {
                    crate::common::recovery_metrics::bump_absorb_recovered();
                }
                let meta = PassthroughMeta {
                    credential_id: id,
                    first_attempted_credential_id: first_attempted_id,
                    model: model.map(|s| s.to_string()),
                    mapped_model: mapped_model.clone(),
                    // S6 P1-1：session 与 Kiro 路径同源（同一函数从 user_id 提取 UUID）。
                    // 此前直接把原始 user_id 串当 session —— 同一会话跨 Kiro/透传拆成
                    // 两个 by_session key，且 account_uuid 明文进 trace。现提取不到即 None。
                    session_id: user_id.and_then(Self::extract_session_uuid),
                    outcome,
                    latency_ms,
                    upstream_error: None, // 成功路径无错误体
                    // 移交在途守卫：从此随响应流存活，流真正消费完才 inflight-1
                    // （与 Kiro 路径 `CallMeta.inflight` 同款）。
                    inflight: inflight_guard,
                };
                return Some((resp, meta));
            }

            // ⭐ 显式列出「该 failover 的状态码」而非用"4xx 非403"反推——后者会让 401/429 先命中
            //    下方 4xx 直返、永远到不了 failover(对抗 review B1 抓到的持久黑洞:429 号不切换)。
            // - 401 key 失效 / 402·403 额度耗尽 / 429 限流 / 5xx 上游错误 → 该号短冷却 + 换下一个 custom_api。
            // - 其余 4xx(404/422 等客户端请求错误)→ 换号/落 Kiro 也一样错,直接返给客户端。
            //
            // 🔴 **400 现在按错误内容分流**（原先一律直返给客户端）。
            //
            // 原注释的假设是「400 换号也一样错」—— 那**在单上游时成立，在代挂号池里不成立**：
            // 实测线上 5 个代挂号指向 5 个**完全不同**的上游（opencode.ai / 本机 k2cc /
            // api.skiapi.dev / fuckopencode / router.denzao），模型能力与协议宽容度各不相同。
            // 于是同一个请求在 A 站 400、在 B 站 200 是常态，典型三类：
            //   - `INVALID_MODEL_ID` / `Invalid model...` → 该上游不认这个模型，**别的站可能认**
            //   - `Invalid tool use format` / `TOOL_USE_RESULT_MISMATCH` → 上游对 tool 配对更严
            //     （k2cc 的 ctx-truncate 会把 tool_use 与其 tool_result 切开造成孤儿），
            //     宽容一些的上游能收
            //   - `messages[N].role must be user or assistant` → 上游对 role 白名单更严
            // 这些直返给客户端 = Claude Code 当场报错中断，而**池子里还有能成功的号没试**。
            //
            // 反过来，确定换号无益的 400 必须**继续直返**，否则就是拿全池去撞同一面墙：
            //   - 额度类（`usage limit` / `quota` / `insufficient`）：账号级状态，换号是另一个账号
            //     的额度，但同号重试无意义 —— 这类走 402/429 语义已被上面的 matches! 覆盖；
            //     若上游错用 400 表达额度，这里靠关键词识别并**不**failover。
            //   - 请求体超长（`too long` / `CONTENT_LENGTH_EXCEEDS_THRESHOLD`）：换号一样超，
            //     且重试只是浪费预算 + 加重上游负担。
            let err_lower = upstream_err.to_lowercase();
            // 🔴 **404 也要换号**（2026-08-10 修，原先只判 400）。
            //
            // 实测证据（真打线上两个代挂上游，同一个模型响应不同）：
            //   | 模型 | 1305(k2cc) | 1418(router.denzao.com) |
            //   | deepseek-v4-flash | **200 OK** | **404 model_not_found** |
            //   | claude-opus-5     | **200 OK** | 502 |
            //   | claude-mythos-5   | 400 INVALID_MODEL_ID | 404 model_not_found |
            // 第一行是决定性的：404 直返客户端 = **池里另一个号明明能成功却不试** ⇒
            // Claude Code/Cursor 把 404 当「模型不存在」当场断会话。
            //
            // 为什么 404 与 400 同性质：两者都是「**这个上游**不认这个请求」，而代挂号池里
            // 5 个号指向 5 个**完全不同**的中转站（能力/协议宽容度各异），"A 站 404、B 站 200"
            // 是常态。只是不同站用不同状态码表达同一件事（k2cc 用 400 INVALID_MODEL_ID、
            // denzao 用 404 model_not_found）。按状态码区分处置是错的，按**语义**才对。
            //
            // ⚠️ 与 handlers.rs:1475 那条「模型永久不可用 → 404 无 Retry-After」不冲突：
            // 那条管的是**我们自己生成**的 404（池内白名单/订阅档不含该模型，静态配置决定），
            // 这里管的是**上游返回**的 404（该站不认，别站可能认）。两种 404 语义相反，
            // 此前被混为一谈正是缺陷根源。
            //
            // 沿用 400 的「非 hopeless 即换号」白名单式兜底而不为 404 新增关键词：
            // 实测两个上游的 404 body 都是 `model_not_found`（配置性/临时，该换号），
            // 且 404 的语义空间比 400 窄；无样本支持的猜测性匹配只会制造误判。
            let is_upstream_error_worth_retry = matches!(code, 400 | 404) && {
                // 先排除"换号无益"的：额度 / 超长。命中即不 failover。
                // 判据是连续形态词表（is_hopeless_upstream_400），不认裸 `quota`
                // （上游能力差异类文案含 quota 字样时仍给换号机会）。
                let hopeless = is_hopeless_upstream_400(&err_lower);
                // 其余一律给换号机会：上游差异导致的 400/404 占实测绝大多数
                // （INVALID_MODEL_ID 52 次 / Invalid tool use 19 次 / role 白名单 / model_not_found）。
                // 空错误体（读取失败）也给机会 —— 宁可多试一个号，也不让客户端白吃错误。
                !hopeless
            };
            let should_failover = matches!(code, 401 | 402 | 403 | 429)
                || (500..600).contains(&code)
                || is_upstream_error_worth_retry;
            // 🔴 模型黑名单（2026-08-14 根治）：上游明确说「该模型不支持」——
            // model_not_found / no available channel（如 pigcode 的
            // "No available channel for model claude-opus-5 under group GPT-PRO"）。
            // 这是该号对该模型的**稳定属性**（不是抖动）：记 (id, model) 短黑名单，
            // 同一请求的后续 failover 与后续请求都不再选它，不再白付一跳。
            // 只认语义特征不认状态码：503/404/400 都可能携带（不同中转站表达不同）。
            let upstream_says_model_unsupported = !upstream_err.is_empty()
                && (err_lower.contains("model_not_found")
                    || err_lower.contains("no available channel")
                    || err_lower.contains("model not found")
                    // 对齐 sub2api 关键词表（"unknown model" 是 newapi/one-api 系上游
                    // 的标准拒绝文案）。
                    || err_lower.contains("unknown model"));
            if upstream_says_model_unsupported {
                if let Some(m) = model {
                    self.token_manager.mark_model_unsupported(id, m);
                    tracing::warn!(
                        credential_id = id,
                        model = %m,
                        "上游明确不支持该模型，记模型黑名单 30min（该号该模型不再被选）"
                    );
                }
            }
            if matches!(code, 400 | 404) {
                tracing::warn!(
                    credential_id = id,
                    status = code,
                    failover = is_upstream_error_worth_retry,
                    upstream_error = %upstream_err.chars().take(200).collect::<String>(),
                    "自定义 API 透传 400/404：按上游错误内容决定是否换号（换号无益的额度/超长类直返）"
                );
            }
            if !should_failover {
                let meta = PassthroughMeta {
                    credential_id: id,
                    first_attempted_credential_id: first_attempted_id,
                    model: model.map(|s| s.to_string()),
                    mapped_model: mapped_model.clone(),
                    // S6 P1-1：同成功路径，session 与 Kiro 同源提取（见 :2373 处注释）。
                    session_id: user_id.and_then(Self::extract_session_uuid),
                    outcome,
                    latency_ms,
                    // 🔴 上游错误体：非 2xx 时带上，让面板/trace 能看到上游原文
                    // （不再出现 `outcome=bad_request` 但 `error_message` 为空的盲区）。
                    upstream_error: if upstream_err.is_empty() {
                        None
                    } else {
                        Some(upstream_err.chars().take(400).collect())
                    },
                    // 同成功路径：错误响应体也要流给客户端，守卫随它存活。
                    inflight: inflight_guard,
                };
                return Some((resp, meta));
            }

            // 冷却决策(秒数 + 原因)收敛到 [`Self::passthrough_cooldown_for`] 一处:
            // 秒数是 2026-08-10 定下的既有调参(dwgx 语义:代挂号 429 只是"它现在忙"),
            // 原因(S4)只决定面板标签/cooldownCode,不改变时长(显式传秒数,不走
            // CooldownReason 默认时长表——401 用 AuthTransient 仍是 180s)。
            let (cooldown_secs, cooldown_reason) = Self::passthrough_cooldown_for(code);
            // 🔴 M1.2（2026-08-16 对抗审查 MAJOR）：400/404 **不记失败余温**——
            // 坏请求（无效 tool schema / 该站不认模型）是全池同质的客户端错误，一次
            // failover 把所有号打上余温会让 60s 内任何请求零尝试直返 503（毒化整池）。
            // 其模型语义已由 `mark_model_unsupported` 黑名单通道覆盖（稳定属性）。
            // 仍记热的：5xx/429/401/402/403（账户级/限流/上游故障，跨请求记忆继续生效）。
            let records_warmth = !matches!(code, 400 | 404);
            if cooldown_secs > 0 {
                // 🔴 N2 日志诚实化（2026-08-16）：`cooldown_custom_api` 被
                // `cooldown_enabled` 门控——线上 cooldownEnabled=false 时它什么都不做，
                // 旧文案一律打印「该号冷却 Ns 并 failover」是撒谎（实际没冷却，
                // 跨请求的死号仍被每个新请求重新选中）。现在按返回值分两档，
                // 没真冷却就明说，并指出现实中承担跨请求降权的是排序键失败余温位。
                let reason = cooldown_reason.expect(
                    "cooldown_secs > 0 时必有原因(passthrough_cooldown_for 不变式)",
                );
                let cooled = self.token_manager.cooldown_custom_api(id, cooldown_secs, reason);
                if cooled {
                    tracing::warn!(
                        credential_id = id,
                        status = code,
                        reason = %reason.description(),
                        "自定义 API 透传失败,该号冷却 {}s({})并 failover 下一个 custom_api",
                        cooldown_secs, reason.description()
                    );
                } else {
                    tracing::warn!(
                        credential_id = id,
                        status = code,
                        cooldown_secs = cooldown_secs,
                        "自定义 API 透传失败,但冷却未启用(cooldownEnabled=false):\
                         不设冷却,仅本请求链内 failover——{}",
                        if records_warmth {
                            "跨请求靠排序键失败余温(60s 降权)避开该号"
                        } else {
                            "400/404 是客户端错误不记余温,换号不降权"
                        }
                    );
                }
            } else {
                // 网络错误/其它：真瞬态，不冷却，但记失败余温——死号恒 502 时
                // 排序键据余温把它降权，不再每请求白打一跳（见 mark_passthrough_failure）。
                tracing::warn!(
                    credential_id = id,
                    status = code,
                    "自定义 API 透传失败(网络/其它),**不冷却**,记失败余温(60s 降权),\
                     仅本请求内 failover 下一个 custom_api"
                );
            }
            // 🔴 N1 根治（2026-08-16）：任何判定「值得 failover」的透传失败都记失败时刻
            // ——排序键「失败余温」位据此降权。5xx 走上方 5s 短冷却（S4 起，非 0）、
            // 网络错误走 cooldown_secs=0 分支不冷却，但**同样要记余温**：死号恒 502
            // （如线上 #3 cursorapi）时不再每请求白打一跳才 failover。该位独立于
            // cooldownEnabled 开关（线上 false 时冷却体系整体失效），是本修复在线上
            // 生效的根基。
            if records_warmth {
                self.token_manager.mark_passthrough_failure(id);
            }
            excluded.insert(id);
            // 丢弃本次错误响应,继续循环试下一个 custom_api;全部试完 select 返 None → 落 Kiro。
        }
    }

    /// 累加一次请求的真实 credit 花费到该凭据的生命周期累计（透传到 token_manager）。
    ///
    /// handler 在请求完成、从上游 meteringEvent 拿到真实计费量后调用；provider 持有
    /// token_manager，handler 只有 provider，故在此开一个薄 passthrough。
    pub fn report_credits(&self, credential_id: u64, credits: f64) {
        self.token_manager.add_credits(credential_id, credits);
    }

    /// 借出内部的号池管理器（只读用途）。
    ///
    /// handler 只持有 provider，但需要在**分派之前**做跨池优先级仲裁
    /// （`should_try_custom_api_first`：决定这次请求先走 custom_api 透传还是先走 Kiro）。
    /// 与 `report_credits` 同款薄 passthrough 思路，避免把仲裁逻辑复制到 handler 层。
    /// 返回内部 Arc<MultiTokenManager>（供 spawn 长生命周期任务持有）。
    pub fn token_manager_arc(&self) -> Arc<MultiTokenManager> {
        self.token_manager.clone()
    }

    pub fn token_manager(&self) -> &MultiTokenManager {
        &self.token_manager
    }

    /// 内部方法：带重试逻辑的 MCP API 调用
    ///
    /// 成功时返回 (响应, 实际使用的凭据 id)：调用方（websearch 快路径）的用量埋点
    /// 需要它写 credential_id（此前返回裸 Response，快路径埋点只能写 None）。
    async fn call_mcp_with_retry(
        &self,
        request_body: &str,
        budget: &SharedRetryBudget,
    ) -> anyhow::Result<(reqwest::Response, u64)> {
        let call_started = std::time::Instant::now();
        let max_retries =
            // 预算按「Kiro 路径**实际可选**的号数」算，而非 entries.len()：后者含 disabled
            // 与 custom_api 条目（is_entry_selectable 永远拒绝 custom_api），会把预算凭空
            // 抬高 —— 生产日志的 `尝试 8/36` 即由此而来。见 kiro_selectable_count 的说明。
            {
                let selectable = self.token_manager.kiro_selectable_count();
                compute_max_retries(selectable, selectable)
            };
        // ⭐ 2026-08-11 方案 A：MCP 调用此前**完全不受**每请求总预算约束（独立 failover
        // 循环、max_retries 由可选号数决定）——websearch 回灌每轮一次 MCP + 一次模型调用，
        // 是跨层 RPM 放大的另一重灾区。现纳入共享预算：本轮最多 min(原配额, 剩余)。
        let max_retries = max_retries.min(budget.remaining() as usize);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        // 与对话路径同款的两个链内状态：
        // - `rate_limited_this_call`：同一请求链内每个号只因风控冷却一次，不重复惩罚。
        // - `suspicious_failovers_this_call`：账户级风控的跨号转移上限，防线性扫全池。
        let mut rate_limited_this_call: HashSet<u64> = HashSet::new();
        let mut suspicious_failovers_this_call: usize = 0;
        const MAX_SUSPICIOUS_FAILOVERS_PER_CALL: usize = 3;
        // 已知问题 #11：MCP 路径失败零埋点 → 失败在面板上不可见。以下在所有失败出口
        // （5 条 bail + client_for `?` + 重试耗尽）统一 emit_record + bump_mcp_failure。
        let mut last_credential_id: Option<u64> = None;
        let mut last_outcome = crate::usage::RequestOutcome::OtherError;
        let mut attempts_used: u32 = 0;

        for attempt in 0..max_retries {
            // 失败记录的 retries 用「已尝试次数 - 1」＝重试次数（与对话路径同口径）。
            attempts_used = attempt as u32;
            // ⭐ 墙钟闸门：单请求 MCP 重试总时长超预算就停止（把最后错误透传给客户端，
            // 让它自己退避）。本循环此前只有次数闸无墙钟 —— retry_delay 指数退避叠加
            // 后，一条慢请求可以在小号池里拖过分钟级、反复扫同一个坏号，把偶发 429
            // 拖成持续雪崩。与对话路径的 round_clock 闸门（见 call_api_with_retry）
            // 同款语义：首次尝试(attempt==0)不受此限，保证至少打一次。
            // 预算取 [`MCP_WALL_SECS`]（≈read_timeout×2+30）而非主路径 45s —— 推导
            // 见该常量注释：45s < 单次合法耗时会掐死换号（同透传墙钟教训）。
            if attempt > 0 && call_started.elapsed() >= Duration::from_secs(MCP_WALL_SECS) {
                tracing::warn!(
                    "单请求 MCP 重试已达墙钟预算 {}s（尝试 {}/{}），停止重试并透传上游错误，避免拖垮整池",
                    MCP_WALL_SECS,
                    attempt,
                    max_retries
                );
                break;
            }
            // MCP 调用（WebSearch 等工具）不涉及模型选择，无需按模型过滤凭据
            let ctx = match self.token_manager.acquire_context(None, None).await {
                Ok(c) => {
                    last_credential_id = Some(c.id);
                    c
                }
                Err(e) => {
                    let es = e.to_string();
                    if es.contains("retry_after_secs=") || es.contains("冷却") {
                        last_outcome = crate::usage::RequestOutcome::RateLimited;
                    }
                    // ⭐ 无号标记（P0）：选不到号 = 池子没有可用 Kiro 凭据（纯
                    // custom_api 池 / 全池禁用）。打上标记供 `call_mcp` 入口识别并
                    // 触发「无号直连」兜底；错误原文保留在链里，返回客户端前剥掉。
                    last_error = Some(e.context(MCP_POOL_UNAVAILABLE_MARKER));
                    continue;
                }
            };

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, &config);

            let (endpoint, alt_region) = match self.select_endpoint(&ctx.credentials, ctx.id) {
                Some(e) => e,
                None => {
                    last_outcome = crate::usage::RequestOutcome::RateLimited;
                    last_error = Some(anyhow::anyhow!(
                        "凭据 #{} 所有端点桶均处于 429 封禁期 retry_after_secs={}",
                        ctx.id,
                        self.shortest_endpoint_bucket_retry_after_secs(Some(ctx.id))
                    ));
                    // ⚠️ 不得 report_failure：None 代表**端点桶 30s 封禁**（瞬态），不是未知端点
                    // 配置错误。report_failure 会累计 failure_count → TooManyFailures 永久禁用
                    // 一个只是被上游限流 30s 的健康号。设 30s 短冷却让调度避开，等桶解封。
                    if rate_limited_this_call.insert(ctx.id) {
                        self.token_manager.report_rate_limited_with_retry_after(
                            ctx.id,
                            Some(ENDPOINT_BUCKET_THROTTLE.as_secs()),
                        );
                    }
                    continue;
                }
            };

            // 备区生效：`select_endpoint` 判定当前区全封时会给出备用 region，
            // 必须覆盖到**实际发请求所用的凭据**上，否则 URL 还是打当前区、
            // 而 429 记账写的是备区的桶键 ⇒ 封禁写进去读不到（对着已 429 的上游持续轰炸）。
            // 用 `Cow` 避免正常路径（`alt_region == None`）多一次 clone。
            let req_cred = match alt_region {
                Some(r) => {
                    let mut c = ctx.credentials.clone();
                    c.api_region = Some(r.to_string());
                    std::borrow::Cow::Owned(c)
                }
                None => std::borrow::Cow::Borrowed(&ctx.credentials),
            };
            let rctx = RequestContext {
                credentials: &req_cred,
                token: &ctx.token,
                machine_id: &machine_id,
                config: &config,
                // MCP(WebSearch 等)不涉及模型对话上下文,无 1M 语义。
                is_1m: false,
            };

            let url = endpoint.mcp_url(&rctx);
            let body = endpoint.transform_mcp_body(request_body, &rctx);

            // client_for 失败（代理/TLS 配置错误等）也走失败埋点：此前 `?` 裸传播，
            // 面板上这条请求同样不存在（已知问题 #11 的 7 个失败出口之一）。
            let client = match self.client_for(&ctx.credentials) {
                Ok(c) => c,
                Err(e) => {
                    crate::common::recovery_metrics::bump_mcp_failure();
                    crate::usage::emit_record(build_mcp_record(
                        ctx.id,
                        crate::usage::RequestOutcome::OtherError,
                        call_started.elapsed().as_millis() as u64,
                        attempts_used,
                    ));
                    return Err(e);
                }
            };
            let base = client
                .post(&url)
                .body(body)
                .header("content-type", "application/json");
            let request = endpoint.decorate_mcp(base, &rctx);

            let response = match request.send().await {
                Ok(resp) => {
                    // 共享预算扣减（2026-08-11 方案 A）：请求已真实发出，无论成败都算
                    // 一次上游调用。
                    budget.consume(1);
                    resp
                }
                Err(e) => {
                    budget.consume(1);
                    last_outcome = crate::usage::RequestOutcome::NetworkError;
                    tracing::warn!(
                        "MCP 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    // 上游 trace（P0-A）：网络错误无响应体，独立组装一条（status=None）。
                    // 守卫只覆盖「读到失败 body 之后」的分支，这里在守卫组装点之前。
                    if crate::kiro::upstream_trace::is_enabled() {
                        crate::kiro::upstream_trace::emit(
                            crate::kiro::upstream_trace::UpstreamTrace {
                                ts: chrono::Utc::now().to_rfc3339(),
                                credential_id: ctx.id,
                                endpoint: endpoint.name().to_string(),
                                url: url.clone(),
                                region: req_cred.effective_upstream_region(&config).to_string(),
                                model: None,
                                attempt: attempt as u32,
                                absorb_round: 0,
                                upstream_calls: attempt as u32 + 1,
                                status: None,
                                retry_after_raw: None,
                                retry_after_secs: None,
                                body: None,
                                network_error: Some(crate::kiro::upstream_trace::sanitize_body(
                                    &e.to_string(),
                                )),
                                latency_ms: call_started.elapsed().as_millis() as u64,
                                verdict: "network_error".to_string(),
                                cred_ever_succeeded: self.token_manager.has_ever_succeeded(ctx.id),
                            },
                        );
                    }
                    last_error = Some(e.into());
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            };

            let status = response.status();

            // 成功响应
            if status.is_success() {
                self.token_manager.report_success(ctx.id);
                // 上游 trace（P0-A）：守卫不覆盖成功路径（成功时 body 还没读，也不该读），
                // 成功侧用独立 emit 直接发一条 verdict="success"（body 恒 None，对话内容绝不落盘）。
                if crate::kiro::upstream_trace::is_enabled() {
                    crate::kiro::upstream_trace::emit(
                        crate::kiro::upstream_trace::UpstreamTrace {
                            ts: chrono::Utc::now().to_rfc3339(),
                            credential_id: ctx.id,
                            endpoint: endpoint.name().to_string(),
                            url: url.clone(),
                            region: req_cred.effective_upstream_region(&config).to_string(),
                            model: None,
                            attempt: attempt as u32,
                            absorb_round: 0,
                            upstream_calls: attempt as u32 + 1,
                            status: Some(status.as_u16()),
                            retry_after_raw: None,
                            retry_after_secs: None,
                            body: None,
                            network_error: None,
                            latency_ms: call_started.elapsed().as_millis() as u64,
                            verdict: "success".to_string(),
                            cred_ever_succeeded: true,
                        },
                    );
                }
                // 用量埋点：MCP 成功路径也落一条记录。
                // 历史缺陷：这里只调 report_success 让凭据 success_count +1，却没有任何
                // emit_record，于是「凭据统计的成功次数」恒大于「用量库的记录数」
                // （实测某号 success_count=2070 而 SQLite 仅 951 条），号池可视化与用量
                // 明细对不上账。字段口径见 [`build_mcp_record`] 的诚实边界说明。
                crate::usage::emit_record(build_mcp_record(
                    ctx.id,
                    crate::usage::RequestOutcome::Success,
                    call_started.elapsed().as_millis() as u64,
                    attempt as u32,
                ));
                return Ok((response, ctx.id));
            }

            // 失败响应
            // 先取 Retry-After（body 消费后 response 不再可用），原始串与解析值都要：
            // trace 存原值；秒数认整数或 HTTP-date。
            let retry_after_raw = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_string());
            let retry_after_secs = retry_after_raw
                .as_deref()
                .and_then(parse_retry_after_header_value);
            let body = response.text().await.unwrap_or_default();

            // ── 上游 trace 失败守卫（P0-A）────────────────────────────────────
            // 成功路径在 body 读取前已 return，守卫只覆盖失败分支；`verdict` 由下方各
            // 失败分支打标签，漏标的分支自然落 unclassified（验收脚本据此统计）。
            let mut mcp_trace_guard = crate::kiro::upstream_trace::FailureTraceGuard::new(
                crate::kiro::upstream_trace::is_enabled(),
                || crate::kiro::upstream_trace::UpstreamTrace {
                    ts: chrono::Utc::now().to_rfc3339(),
                    credential_id: ctx.id,
                    endpoint: endpoint.name().to_string(),
                    url: url.clone(),
                    region: req_cred.effective_upstream_region(&config).to_string(),
                    model: None,
                    attempt: attempt as u32,
                    absorb_round: 0,
                    upstream_calls: attempt as u32 + 1,
                    status: Some(status.as_u16()),
                    retry_after_raw: retry_after_raw.clone(),
                    retry_after_secs,
                    body: Some(crate::kiro::upstream_trace::sanitize_body(&body)),
                    network_error: None,
                    latency_ms: call_started.elapsed().as_millis() as u64,
                    verdict: crate::kiro::upstream_trace::VERDICT_UNCLASSIFIED.to_string(),
                    cred_ever_succeeded: self.token_manager.has_ever_succeeded(ctx.id),
                },
            );

            // 额度用尽（**不门控状态码**，理由同对话路径那处的长注释：
            // 上游已从 402 改用 400，402 实测 6 小时 0 次而 400+OVERAGE 564 次）
            if endpoint.is_monthly_request_limit(&body) {
                mcp_trace_guard.verdict("monthly_limit");
                let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                if !has_available {
                    // 失败埋点（#11）：此前裸 bail，失败在面板上不存在。
                    crate::common::recovery_metrics::bump_mcp_failure();
                    crate::usage::emit_record(build_mcp_record(
                        ctx.id,
                        crate::usage::RequestOutcome::QuotaExhausted,
                        call_started.elapsed().as_millis() as u64,
                        attempts_used,
                    ));
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_outcome = crate::usage::RequestOutcome::QuotaExhausted;
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 400 Bad Request
            if status.as_u16() == 400 {
                mcp_trace_guard.verdict("generic_400");
                crate::common::recovery_metrics::bump_mcp_failure();
                crate::usage::emit_record(build_mcp_record(
                    ctx.id,
                    crate::usage::RequestOutcome::BadRequest,
                    call_started.elapsed().as_millis() as u64,
                    attempts_used,
                ));
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 401/403 凭据问题
            if matches!(status.as_u16(), 401 | 403) {
                // 外层先标粗标签，子出口再覆盖成更精确的名字（verdict 最后一次写入生效）。
                mcp_trace_guard.verdict("auth_4xx");
                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会。
                //
                // ⚠️ **api_key 号必须跳过**：它没有 refreshToken，`refresh_token()` 对它是
                // 契约级 bail（"API Key 凭据不支持刷新 Token"，见 token_manager.rs 该处注释：
                // 那个 bail 是给面板「强制刷新」按钮设计的，让错误传播成 400）。
                // 在**请求热路径**上调它则是纯损耗：结构上不可能成功，而失败会
                // ① 计入失败计数、② 落 auth 冷却。更糟的是该错误串不含任何永久 HTTP 码，
                // 被刷新层的瞬态判据（黑名单式）当成可重试 → 1s/2s 退避重试 3 次。
                //
                // 线上实测（本轮多开时暴露）：一个 api_key 号遇 403 后每轮白等约 3 秒、
                // 连计 3 次失败即被判死号自动禁用 —— 相当于**把它的死亡速度放大三倍**。
                // 对 api_key 号，401/403 的含义就是「这个 key 现在不被接受」，
                // 直接走下方的风控/失败分类即可，不该绕一趟刷新。
                if endpoint.is_bearer_token_invalid(&body)
                    && !force_refreshed.contains(&ctx.id)
                    && !ctx.credentials.is_api_key_credential()
                {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    if self
                        .token_manager
                        .force_refresh_token_for(ctx.id)
                        .await
                        .is_ok()
                    {
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                    // 刷新失败 = 认证态有问题，加一段冷却让调度避开它。
                    //
                    // ⭐ 时长按**该号是否被证明过**二分（与对话路径同处逐字同款）：
                    // 刷新层内部已对 5xx/网络错误退避重试 3 次（见
                    // `report_refresh_failure_classified` 的文档），所以能走到这里的
                    // 刷新失败里上游 token 端点抖动占大头。一个已成功过的号吃一次抖动
                    // 就被冻 24h（`AuthenticationFailed` 的 `is_auto_recoverable=false`
                    // ⇒ long_cooldown 86400s）= 面板上的僵尸；而从未成功过的号刷新还失败，
                    // 大概率 refreshToken 真废了，该硬冻等人工。
                    if self.token_manager.has_ever_succeeded(ctx.id) {
                        self.token_manager.report_auth_transient_cooldown(ctx.id);
                    } else {
                        self.token_manager.report_auth_cooldown(ctx.id);
                    }
                }

                // 订阅不覆盖本应用/模型：**永久**条件 → 立即终止，不重试、不计凭据失败。
                //
                // 与对话路径同口径（见 `call_api_with_retry` 的同名分支）。本路径必须**同时**
                // 有这一条：MCP/WebSearch 打的是同一个上游、用的是同一个凭据，订阅不覆盖时
                // 拿到的是同一个 403。漏在这里的后果与那条历史缺陷同形 ——
                // 上面那段注释记着「对话路径已修，本路径此前漏修」，而本仓 issue #2 的
                // 结论就是「同一逻辑各写一份」正是漏改的成因。
                if endpoint.is_subscription_unsupported(&body) {
                    mcp_trace_guard.verdict("subscription_unsupported");
                    tracing::warn!(
                        "MCP 请求失败（订阅不覆盖本应用/模型，永久条件；不重试、不计凭据失败）: {} {}",
                        status,
                        body
                    );
                    last_error = Some(anyhow::anyhow!(
                        "MCP 请求失败（订阅不支持该应用/模型，重试无效）: {} {} \
                         subscription_unsupported=1",
                        status,
                        body
                    ));
                    break;
                }

                // 账户级**临时**风控限速（suspicious activity / temporary limits）：
                // 与对话路径同口径（见 `call_api_with_retry` 的 is_temporary_rate_limit 分支），
                // 必须在落 `report_failure` 之前判定。
                //
                // 历史缺陷（本分支原先直接 report_failure）：403 TEMPORARILY_SUSPENDED 是
                // **临时态**，而 report_failure 累加 failure_count，达 MAX_FAILURES_PER_CREDENTIAL
                // 即以 TooManyFailures（**永久型**标签）禁用。于是一个只是被临时限流的号，
                // 走 WebSearch/MCP 被打 3 次 403 就被永久禁用 —— 正是历史事故的同一误判形态
                // （403 曾被当永久封禁 → 12h 内 88 次误禁 + 36 次全池自愈活锁）。对话路径已修，
                // 本路径此前漏修；且自动禁用落盘后（persist_disabled_state）该误禁**重启也回不来**。
                if endpoint.is_temporary_rate_limit(&body) {
                    mcp_trace_guard.verdict("temporary_rate_limit");
                    last_outcome = crate::usage::RequestOutcome::RateLimited;
                    tracing::warn!(
                        "MCP 请求失败（账户临时风控限速，非永久封禁；分钟级退避后 failover，尝试 {}/{}）: {} {}",
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );
                    // 账户级风控也是上游限速信号 → 入站整形 RPM 自动降档。
                    self.token_manager.report_upstream_rate_limited();
                    // 本请求链内该号首次触发才设冷却；再次触发只 failover，不重复惩罚
                    // （与对话路径的 rate_limited_this_call 同款去重，避免一条链把号砸进更深风控）。
                    if rate_limited_this_call.insert(ctx.id) {
                        self.token_manager.report_suspicious_activity(ctx.id);
                    } else {
                        tracing::debug!(
                            "凭据 #{} 本 MCP 请求链内已因风控冷却过，再次触发仅 failover，不重复惩罚",
                            ctx.id
                        );
                    }
                    last_error = Some(anyhow::anyhow!(
                        "MCP 请求失败（账户级可疑活动风控，分钟级退避）: {} {}",
                        status,
                        body
                    ));
                    // 跨号转移上限：与对话路径同款，超过即停止遍历并透传错误。
                    // 不设上限会线性扫全池，既让用户干等，又把整池号一起送进上游风控。
                    suspicious_failovers_this_call += 1;
                    if suspicious_failovers_this_call >= MAX_SUSPICIOUS_FAILOVERS_PER_CALL {
                        tracing::error!(
                            "本次 MCP 请求已因账户级风控转移 {} 次号，停止遍历号池并透传错误",
                            suspicious_failovers_this_call
                        );
                        break;
                    }
                    continue;
                }

                // 账户被永久暂停/封禁：禁用该号并换号（同样先于通用失败判定，
                // 使 disabled_reason 落 AccountSuspended 而非 TooManyFailures）。
                if endpoint.is_account_suspended(&body) {
                    mcp_trace_guard.verdict("account_suspended");
                    tracing::error!(
                        "MCP 请求失败（账户被暂停/封禁，禁用凭据并切换，尝试 {}/{}）: {} {}",
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );
                    self.token_manager.report_upstream_pressure();
                    let has_available = self.token_manager.report_account_suspended(ctx.id);
                    if !has_available {
                        // 失败埋点（#11）。
                        crate::common::recovery_metrics::bump_mcp_failure();
                        crate::usage::emit_record(build_mcp_record(
                            ctx.id,
                            crate::usage::RequestOutcome::AccountSuspended,
                            call_started.elapsed().as_millis() as u64,
                            attempts_used,
                        ));
                        anyhow::bail!(
                            "MCP 请求失败（账户被封禁且所有凭据已用尽）: {} {}",
                            status,
                            body
                        );
                    }
                    last_outcome = crate::usage::RequestOutcome::AccountSuspended;
                    last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                    continue;
                }

                let has_available = self.token_manager.report_failure(ctx.id);
                if !has_available {
                    // 失败埋点（#11）。
                    crate::common::recovery_metrics::bump_mcp_failure();
                    crate::usage::emit_record(build_mcp_record(
                        ctx.id,
                        crate::usage::RequestOutcome::AuthFailed,
                        call_started.elapsed().as_millis() as u64,
                        attempts_used,
                    ));
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_outcome = crate::usage::RequestOutcome::AuthFailed;
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 瞬态错误
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                if status.as_u16() == 429 {
                    mcp_trace_guard.verdict("rate_limited");
                } else {
                    mcp_trace_guard.verdict("server_error");
                }
                last_outcome = if status.as_u16() == 429 {
                    crate::usage::RequestOutcome::RateLimited
                } else {
                    crate::usage::RequestOutcome::ServerError
                };
                tracing::warn!(
                    "MCP 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                // 🔀 429 换桶：仅多端点凭据封当前 host 桶 30s。MCP 的 `acquire_context(None, None)`
                // 无 tried 排除集 ⇒ 同凭据可被反复选中，封桶后下一轮 `select_endpoint` 自动跳过
                // 它换下一端点；全部端点都封时 `select_endpoint` 返回 None → None 分支设 30s 冷却
                // 兜底，不会死循环。
                if status.as_u16() == 429 {
                    let order = ctx.credentials.effective_endpoint_order(&self.default_endpoint);
                    if order.len() > 1 {
                        // 桶键用 `bucket_id(&rctx)`：这里有真实 ctx，可直接算。
                        // 与 select 侧的 `bucket_key` 逐字节等价（后者只是用占位
                        // token/machine_id 构造 ctx，而 api_url 不读这两个字段）。
                        self.endpoint_buckets.lock().insert(
                            (ctx.id, endpoint.bucket_id(&rctx)),
                            Instant::now() + ENDPOINT_BUCKET_THROTTLE,
                        );
                    }
                }
                // 端点自适应派发：429 与该端点特有的 400（如 ksk_ 打 codewhisperer 的
                // `The provided credential is invalid`）都算「该端点不愿受理本凭据」。
                // 刻意**排除** 402/403 —— 那是凭据自己的问题（额度耗尽/账号封禁），
                // 换端点一样失败，记进去会把「号坏了」误传成「端点坏了」。
                let code = status.as_u16();
                if code == 429 || code == 400 {
                    self.report_endpoint_outcome(ctx.id, endpoint.name(), false);
                }
                if attempt + 1 < max_retries {
                    sleep(Self::retry_delay(attempt)).await;
                }
                continue;
            }

            // 其他 4xx
            if status.is_client_error() {
                mcp_trace_guard.verdict("other_4xx");
                // 失败埋点（#11）。
                crate::common::recovery_metrics::bump_mcp_failure();
                crate::usage::emit_record(build_mcp_record(
                    ctx.id,
                    crate::usage::RequestOutcome::BadRequest,
                    call_started.elapsed().as_millis() as u64,
                    attempts_used,
                ));
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 兜底
            last_outcome = crate::usage::RequestOutcome::OtherError;
            last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        // 重试耗尽：失败也落一条记录（#11，第 7 个失败出口）。credential_id 未知时如实置 None。
        crate::common::recovery_metrics::bump_mcp_failure();
        let mut rec = build_mcp_record(
            last_credential_id.unwrap_or_default(),
            last_outcome,
            call_started.elapsed().as_millis() as u64,
            attempts_used,
        );
        if last_credential_id.is_none() {
            rec.credential_id = None;
        }
        crate::usage::emit_record(rec);
        if max_retries == 0 {
            // 每客户端请求的共享上游预算已耗尽（2026-08-11 方案 A：此前各层独立拿配额，
            // 预算耗尽不可能出现；现在可能发生在 websearch 回灌靠后轮次或压缩重试轮）。
            // 语义与「重试耗尽」一致：错误上抛给客户端自己退避，绝不空跑。
            Err(anyhow::anyhow!(
                "MCP 请求失败：每客户端请求的上游调用预算已耗尽（shared_budget_exhausted=1）"
            ))
        } else {
            Err(self.with_sealed_bucket_retry_after(
                last_error.unwrap_or_else(|| {
                    anyhow::anyhow!("MCP 请求失败：已达到最大重试次数（{}次）", max_retries)
                }),
                last_outcome,
            ))
        }
    }

    /// 内部方法：带重试逻辑的 API 调用
    ///
    /// 重试策略：
    /// - 每个凭据最多重试 MAX_RETRIES_PER_CREDENTIAL 次
    /// - 总重试预算由 [`compute_max_retries`] 动态计算：以可用凭据数为下限、以
    ///   ABSOLUTE_MAX_TOTAL_RETRIES 为硬上限（号池 > 4 时不再保证每个号都被摸到 ——
    ///   摸穿全池正是风控要抓的突发特征）
    async fn call_api_with_retry(
        &self,
        request_body: &str,
        is_stream: bool,
        is_1m: bool,
        budget: &SharedRetryBudget,
        client_model: Option<&str>,
    ) -> anyhow::Result<(reqwest::Response, CallMeta)> {
        // 「基础」配额:一轮 failover 链最多摸几个号。吸收层开启时它**不是**本轮的实际配额
        // —— 实际配额还要被跨轮总额度夹一次(见 round_retry_quota)。刻意不叫 `max_retries`:
        // 循环内那个同名变量才是本轮生效值,同名两义必混。
        let base_retry_quota =
            // 预算按「Kiro 路径**实际可选**的号数」算，而非 entries.len()：后者含 disabled
            // 与 custom_api 条目（is_entry_selectable 永远拒绝 custom_api），会把预算凭空
            // 抬高 —— 生产日志的 `尝试 8/36` 即由此而来。见 kiro_selectable_count 的说明。
            {
                let selectable = self.token_manager.kiro_selectable_count();
                // 动态降档：近期上游压力率（429+5xx）高（疯狂重试）时按比例收缩预算，
                // 避免号多 + 压力多时每个请求顺着号池一路扫过去、把内部上游 RPM 放大到
                // 外部 RPM 的十几倍。只在进循环前算一次，跨轮不叠加。
                let raw = compute_max_retries(selectable, selectable);
                let pressure = self.retry_pressure.lock().rate();
                let scaled = apply_retry_pressure(raw, pressure);
                if scaled != raw {
                    tracing::warn!(
                        "上游压力率 {:.1}% 过高，重试预算从 {} 动态降档到 {}（防内部放大）",
                        pressure * 100.0,
                        raw,
                        scaled
                    );
                }
                scaled
            };
        let mut last_error: Option<anyhow::Error> = None;
        // ⭐ S3：重试链内**首个**上游 429 的显式 Retry-After（最早类型化 429 保留）。
        //
        // 重试链里第一个 429 的退避指令不该被后续 generic 错误（5xx 等）覆盖：
        // 终态非 429 时用它把「429 语义 + 上游精确 RA」带回客户端（见下方
        // `assemble_final_error`）。参考 zyphr 的 `take_rate_limit_error`（最早类型化
        // 429 优先，ref-ZyphrZero-kiro.rs.md 机制 #8）。
        //
        // 🔴 m7（2026-08-16 对抗审查 RA MINOR）：RA 合并语义 `.or()` → `.max()`。
        // `.or()` = 首个**带值**者胜出——第二个号 429 RA=120 时客户端拿首个 10s 就重试，
        // 提前撞回上游仍在限流的窗口。`.max()` = 保留最大 RA（「上游说多久等多久」，
        // 保守退避；首个 429 无 RA、后续 429 有 RA 时取后者；先 429 后 5xx（无 RA）
        // 时首个 RA 仍保留——`None < Some`）。见 [`merge_upstream_429_retry_after`]。
        let mut first_upstream_429_retry_after: Option<u64> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        // 本次请求重试链内「已因 429 冷却过」的凭据集合。防止同一个请求的一条重试链
        // 反复砸同一个号、把同一次限流事件当成多次独立事件累加 trigger_count / 指数延长冷却
        // （根因：小号池下重试循环反复选到同两个号，单请求就把 trigger_count 刷到 7、冷却 15→72s，
        //  自造雪崩）。首次 429 才设冷却，同链再 429 只换号 failover，不重复惩罚。
        // 跨请求（新请求 = 新集合）仍正常累加，保留「持续被限流的号冷却渐长」的合理行为。
        let mut rate_limited_this_call: HashSet<u64> = HashSet::new();
        // 本次请求是否已因「账户被暂停」转移过号。suspend 是账号级信号且多伴随同出口 IP
        // 的整体风控，遍历全池只会把剩下的号一起烧掉（见 suspend 分支处的说明）。
        let mut suspended_this_call = false;
        // 本次请求已因**账户级临时风控**（403 TEMPORARILY_SUSPENDED）转移过多少次号。
        //
        // ⚠️ 此前该分支只有 `rate_limited_this_call` 的**同号**去重，没有任何**跨号**上限，
        // 于是可以线性扫全池：线上 43 个号实测「尝试 43/43」，一条请求打 43 次上游、
        // 耗尽 45s 墙钟才失败。而 account_suspended 分支早就有 `suspended_this_call`
        // 限一次——同为账号级风控信号，这里缺了等价物。
        //
        // 取 3 而非 1：403 有两种成因，必须都照顾到。
        //   ① 单号被上游盯上（换号就能成功）→ 需要允许换几次；
        //   ② 同出口 IP 整池风控（换号无用，只会把更多号烧进风控）→ 必须尽快停。
        // 3 次足以跨过少数坏号拿到好号，又不会把整池扫一遍。配合自动禁用
        // （连续零成功即移出候选集），坏号根本不该反复进入候选，这个上限只是纵深防御。
        let mut suspicious_failovers_this_call: usize = 0;
        /// 单请求因账户级临时风控最多转移几次号（见 `suspicious_failovers_this_call`）。
        const MAX_SUSPICIOUS_FAILOVERS_PER_CALL: usize = 3;
        // 本次请求已在通用 401/403 分支惩罚过的号，避免同一个号在一条请求里被连打 3 次
        // 直接推到 TooManyFailures（custom_api 路径早有 excluded 集，Kiro 路径此前没有）。
        let mut auth_failed_this_call: HashSet<u64> = HashSet::new();
        // 本请求链内已因 403 FEATURE_NOT_SUPPORTED 做过「本地 region 纠正 + 重试」的号(镜像
        // force_refreshed 去重惯例)。防同一坏号在一条链里反复本地纠正+重试烧光 max_retries。
        let mut region_corrected_this_call: HashSet<u64> = HashSet::new();
        // ⭐ L1 换区重试的**每号一次**上限（镜像 `force_refreshed` 去重惯例）。
        //
        // 不加上限就是两个区来回打：A 区 403 → 换 B → B 区 403 → 换回 A → …… 一条客户端
        // 请求把额度全烧在同一个号的两个区之间，同一出口 IP 连打 = 正是风控要抓的突发特征。
        // 本仓刚因「吸收层放大」修过一轮，这里不重犯。
        //
        // ⭐ A-5 起本集合是**两条换区路径共享的「本请求已换区」标记**：L1 403 换区
        // （下方 region_retry_target 分支）与 429 备区换桶（select_endpoint 返回
        // `alt_region` 后的 Cow 重绑处）都往这里 insert，403 分支的门
        // `!contains(&ctx.id)` 因此对**任何一条路径**先换过区的号都生效 ——
        // 否则 429 换到备区后吃 403，L1 会按当前区算回原区，而原区桶还在封禁期，
        // select_endpoint 又弹回备区 ⇒ 同一请求内 A→B→A→B 振荡。
        let mut region_switched_this_call: HashSet<u64> = HashSet::new();
        // ⭐ L1 换区后**本次请求内生效**的 region（id → region），在建请求时覆盖凭据的
        // `api_region`。
        //
        // 为什么用 per-call 覆盖而不是直接改凭据再重试：换区能不能成功还不知道，
        // 先改再试等于拿一个**未验证的猜测**覆盖掉线上配置 —— 若这次失败是别的原因
        // （限流/上游抖动），号的 region 就被无依据地改坏了。L2 的回写只在**这个区真的
        // 拿到 200 之后**才发生（见成功分支），那时它是**已验证**的事实。
        let mut region_override_this_call: HashMap<u64, String> = HashMap::new();
        // ⭐ 本次客户端请求**已经打过**的号：喂给 acquire_context_excluding,让下一跳
        // 结构性避开它,不再依赖 `cooldownEnabled`(线上它是 false ⇒ failover 事实上不换号,
        // 一个真实 429 被放大成连环 429)。与其它去重集同样声明在 'absorb 循环之外 ⇒
        // 跨吸收轮共享 ⇒ 一条客户端请求内不会反复回头打同一个号。
        // 全池都试过时排除集自动退化成"允许重选"(见 acquire_context_excluding 不变量 1)。
        let mut tried_this_call: HashSet<u64> = HashSet::new();
        // MODEL_TEMPORARILY_UNAVAILABLE 全局容量问题专用计数：只允许 1 次慢速退避重试，
        // 耗尽后立即 break（而非继续烧光 max_retries 切换凭据——所有凭据受同一模型过载影响）。
        let mut model_unavailable_attempts: usize = 0;
        const MAX_MODEL_UNAVAILABLE_RETRIES: usize = 1;
        let api_type = if is_stream { "流式" } else { "非流式" };

        // 一次解析同时取出模型信息与会话标识（conversationId），避免热路径上对
        // 整个请求体做两次全量 serde_json::from_str（大请求体尤其昂贵）。
        let (model, session_id) = Self::extract_model_and_session(request_body);
        // 客户端**原始**模型名（调用方从入站 payload 传入；Kiro 请求体里的 modelId 已被
        // converter 归一化成 Kiro id，不再是客户端原始名）。供成功/失败埋点的
        // `requested_model` 口径；None = 调用方未提供（如 test 工具），回落请求体解析名。
        let client_model_owned = client_model.map(str::to_string);

        // ⭐ 全局模型映射规则：**循环外只快照一次**（TIER1 热重载下同一请求的多次
        // failover 跳必须用同一份规则，否则第 1 跳 A→B、第 2 跳 A→C，`mapped_model`
        // 单值无从归属）。克隆的是规则表，映射在循环内按每凭据豁免决定是否应用。
        let mapping_rules = self.token_manager.config().model_mapping.clone();
        // 本次调用实际改写后的模型名（循环外声明，成功/失败路径共享；见 CallMeta.mapped_model）。
        // None = 未命中映射 / 凭据豁免；overload_fallback 路径记 fallback 名。失败记录同样用它，
        // 保证「按 upstream_model 聚合」时失败样本不凭空消失（复现 #21 教训）。
        // ⚠️ 2026-08-11 起为「最后一跳」语义：每跳同步本跳真实映射结果（未映射也置 None），
        // 不再跨跳残留旧值，见循环内 mapped_this_attempt 之后的同步点。
        let mut mapped_model: Option<String> = None;

        // 用量埋点：记录进入调用的时刻与最后服务的凭据/失败分类
        let call_started = std::time::Instant::now();
        let mut last_credential_id: Option<u64> = None;
        let mut last_outcome = crate::usage::RequestOutcome::OtherError;
        // 是否真的发生过 failover(打了 >1 个号)。用于区分「整池换号都失败=真耗尽」与
        // 「首个号就因客户端错误/模型无效 break=不是池的问题」——后者不该计 failover_exhausted。
        let mut real_failover_happened = false;
        // 本次调用实际尝试过的次数（循环外可见，供**失败**记录使用）。
        //
        // 为什么需要它：成功分支用循环变量 `attempt`（见下方 `retries: attempt as u32`），
        // 但 `attempt` 在循环结束后已出作用域，而失败记录是在循环**之后**组装的。
        // 此前 `fail_record` 因此完全没有设 `retries` → 落库即默认 0。
        //
        // 后果（线上实测坐实）：近 2 小时全部失败样本 **无一例外 retries=0**
        // （auth_failed 1487 / rate_limited 1098 / server_error 118 / bad_request 91），
        // 而同期成功样本有 retries=1、历史上号池大时到过 7 以上。
        // 即「烧掉 12 次换号才失败」与「第一次就失败」在面板上完全不可区分 ——
        // 而那恰是最需要看的那类样本（判断重试预算是否够用、吸收层是否有效的唯一依据）。
        let mut attempts_used: u32 = 0;
        // ⭐ **真正打到上游**的次数（跨吸收轮累计），只用来喂 [`round_retry_quota`]。
        //
        // 为什么不能复用 `attempts_used`：后者是 for 循环的**迭代计数**，含两类零上游调用的空转
        // —— ① `acquire_context_excluding` 失败的 fast-fail（全池冷却时 `all_cooling_fast_fail`
        // 默认开，wait>2s 即裸 `continue`，不 sleep 也不打上游）；② endpoint 解析失败。
        //
        // 复用它的后果（本轮修复的缺陷）：`compute_max_retries` 在 pool≥4 时恒为
        // `ABSOLUTE_MAX_TOTAL_RETRIES`=4，于是全池冷却下第 0 轮在**毫秒级**把 4 个额度
        // 全烧在 fast-fail 上 → 轮末 `attempts_base=4` → 额度闸门命中 → `break 'absorb`
        // ⇒ **`absorb_round` 恒 0，吸收层等于没开**。而 PoolCooldown 正是吸收层要拦的主类别，
        // 排在额度闸门之后的截断闸门因此**永远不被求值**（顺序在这里是承重的）。
        //
        // 也不能反过来让 `attempts_used` 只计上游调用：它另有用途（失败记录的
        // `fail_record.retries`，要反映客户端视角的真实换号次数，含 acquire 失败与墙钟 break）。
        // 两个语义必须分成两个变量。
        let mut upstream_calls: u32 = 0;

        // 入站整形准入闸门：**整个客户端请求只过一次**，位于 handler 层入口
        // （post_messages / post_messages_cc 的 try_inbound_admission_gate，见
        // handlers.rs），本函数只保留吸收层；突发由令牌桶在入口排队削平。
        // review Finding 1 修复:不在 acquire_context 里扣(否则 failover N 跳扣 N 令牌 + fast-fail 空转白扣)。
        //
        // ⚠️ 标记 `inbound_admission_timeout=1` 是**必须**的,不能只靠 `retry_after_secs=`:
        // 它与全池冷却在语义上正好相反 ——
        //   · 全池冷却 = **上游**没准备好,等一会儿真的会好 → 值得重试;
        //   · 准入超时 = **网关自己**在保护上游主动限流(背压),重试只是把同一个请求
        //     再塞回同一个已经满的桶 → 队列更长、客户端等更久,而且拿不到任何额外的成功概率。
        // 两者若共用同一个标记就在字符串上不可区分,任何吸收/重试层都会把网关自己的背压
        // 信号当成"上游稍后会好"去重试(实测形态:2 轮 × 30s = 客户端等 60s 才拿到 429,
        // 而正确行为是 <2s 立刻拿到 429 由客户端自己退避)。
        // 保留 `retry_after_secs=` 是为了让**客户端**仍拿到 429 + Retry-After(那对客户端是对的);
        // 新标记只用于让网关内部的分类器把它判成"不可吸收"。
        // 🔴 2026-08-10：acquire_admission 已移至 handlers 层
        // （post_messages 与 post_messages_cc 入口统一过闸，2026-08-11 补 CC 入口），
        // 透传与 Kiro 两条路径都在 handler 层过闸门，provider 不再重复调用。

        // ── 内置「上游 429 吸收层」──────────────────────────────────────────────
        // 吸收层在闸门之下（结构化保证不会被重入）。acquire_admission 已移至 handlers 层，
        // 两条路径统一在 handler 入口过闸，provider 不再重复调。
        // ⭐ 配置快照：一次调用只取一份（与上方 mapping_rules 同约定）。此前 `config`
        // 在下方每跳 attempt 循环内重读（ArcSwap load + 引用计数增减 × 每跳），
        // 热更配置会让同一条请求的不同 failover 跳按不同配置走，行为不可复现。
        let config = self.token_manager.config();
        let absorb = AbsorbPolicy::from_config(&config);
        // deadline 与 call_started 同源:准入排队(最长 inbound_queue_max_wait_secs)也计入
        // 预算。若改成从此刻起算,客户端可见延迟 = 排队 30s + 吸收 45s = 75s ≈ shield 的
        // p50 73.2s,等于把病根换个地方搬进来。
        let absorb_deadline = call_started + absorb.budget;
        // 本轮生效的 deadline。默认等于总预算那个；只有在**上一轮末尾**判定为换号空窗且
        // 该类设了独立预算时，才在 sleep 处换成它自己那份（`class_deadline`）。
        //
        // 为什么要用一个可变量而不是直接用 `class_deadline`：类别只有在一轮**跑完**、
        // 拿到 `last_error` 之后才知道，而 `round_budget` 在进轮时就要用 deadline。
        // 逐轮记录「本轮是被哪一类触发的」就把两者对齐了，且不会让某一类的宽预算
        // 泄漏给下一轮的其它类别（下一轮若是别的类，这里会被改回 `absorb_deadline`）。
        let mut round_deadline = absorb_deadline;
        // 吸收层跑过至少一轮却仍放弃 ⇒ 终态状态码可按配置换成 503（见
        // `ABSORB_BUDGET_EXHAUSTED_MARKER`）。只在真睡过退避、真重打过的情形置位：
        // 一次都没重试就改状态码是**说谎**（网关没尽力，却告诉客户端「我们暂时不可用」）。
        let mut absorb_gave_up_after_rounds = false;
        let mut absorb_round: u32 = 0;
        // 跨轮累计的尝试数(喂 attempts_used)。声明在 'absorb 之外,故失败记录里的 retries
        // 是整条客户端请求的真实总换号数,而不是最后一轮的局部计数。
        let mut attempts_base: u32 = 0;

        // ⚠️ 所有「链内去重集」(rate_limited_this_call / suspended_this_call /
        // suspicious_failovers_this_call / auth_failed_this_call / region_corrected_this_call
        // / model_unavailable_attempts) 都声明在本循环**之外** ⇒ 跨吸收轮共享 ⇒ 同一个号在
        // 整条客户端请求内只被惩罚一次。若把它们挪进轮内,同号会被反复罚 → trigger_count 累加
        // → 冷却 15s 指数拉长到 72s,那正是「单请求自造雪崩」的成因。本方案的第二条承重不变量。
        'absorb: loop {
            let round_started = std::time::Instant::now();
            // 关闭时两者恒等于旧值(round_clock == call_started、round_budget == 完整 45s),
            // 故墙钟闸门的判据与旧代码逐字节相同。见 docs/absorb-layer-design.md §8。
            let round_clock = if absorb.enabled {
                round_started
            } else {
                call_started
            };
            // 用 `round_deadline`（而非固定的 `absorb_deadline`）：换号空窗设了独立预算时，
            // 由它触发的那一轮才拿得到那份更宽的墙钟。第 0 轮两者恒相等 ⇒ 旧行为不变。
            let round_budget = absorb.round_budget(round_deadline, round_started);
            // ⭐ 未修问题 ②：本轮实际配额 = min(基础配额, 跨轮总额度剩余)。
            // 声明在轮**内**（与去重集相反）是刻意的：它是每轮重算的**派生量**，
            // 而它依赖的累计量 `budget.used()` 在跨层共享 ⇒ 上限回到「每请求」语义。
            // 关闭吸收层时预算只在唯一一轮内被消费、且这里只在进轮时读一次
            // ⇒ 恒等于 base_retry_quota（本身已 ≤ ABSOLUTE_MAX_TOTAL_RETRIES）⇒ 逐字节等价旧行为。
            //
            // ⚠️ 喂的是 `budget.used()`（跨层共享的已用量——MCP/透传先行消费后与局部
        // `upstream_calls` 不等）而**不是** `attempts_base`：后者含 fast-fail 空转,
            // 会让全池冷却在毫秒内烧空额度、把吸收层整体旁路掉(见其声明处的长注释)。
            // 「每请求 ≤ ABSOLUTE_MAX_TOTAL_RETRIES 次上游调用」这个不变量仍然成立:进轮时
            // quota ≤ ABSOLUTE_MAX_TOTAL_RETRIES − budget.used(), 而本轮内最多再打 quota
            // 次 ⇒ 轮末 budget.used() ≤ ABSOLUTE_MAX_TOTAL_RETRIES（跨层总额度共享）。
            let max_retries = round_retry_quota(base_retry_quota, budget.used());

            // 同号续跳：429 换桶 / L1 换区必须打回**刚失败的那个号**。
            // 只靠把该号从排除集摘掉不够——选号按 RPM/在途排序，会优先捡还没打过的陪跑号，
            // 备区 hop 被偷走（A-5 实测：受害者只打到当前区、备区 0 次，队列耗尽变 500）。
            let mut reuse_ctx: Option<crate::kiro::token_manager::CallContext> = None;
            'attempt: for attempt in 0..max_retries {
                // 与成功分支的 `retries: attempt as u32` 同口径：记「已尝试次数 - 1」＝重试次数。
                // 放在墙钟闸门**之前**递增：闸门 break 时也要反映"这一轮进来过"，
                // 否则墙钟耗尽的失败会少记一次，而那正是要观测的形态。
                attempts_used = attempts_base + attempt as u32;
                // 墙钟闸门：单请求重试总时长超预算就停止（把最后错误透传给客户端，
                // 让它自己退避）。防止一个卡住的请求在小号池里反复扫冷全池、把偶发 429
                // 拖成持续雪崩。首次尝试(attempt==0)不受此限，保证至少打一次。
                //
                // 吸收层开启时 round_clock/round_budget 变成「本轮起点 / min(45s, 剩余预算)」：
                // 一轮的墙钟上限被剩余总预算夹住,这就是吸收轮次不会超预算的机制本身。
                if attempt > 0 && round_clock.elapsed() >= round_budget {
                    tracing::warn!(
                        "单请求重试已达墙钟预算 {:?}（尝试 {}/{}，吸收轮次 {}），停止重试并透传上游错误，避免拖垮整池",
                        round_budget,
                        attempt,
                        max_retries,
                        absorb_round
                    );
                    break 'attempt;
                }
                // 获取调用上下文（绑定 index、credentials、token）
                //
                // ⭐ 传入 `tried_this_call`：本请求已试过的号在下一跳被**结构性**排除，
                // 不再依赖 `cooldownEnabled`。此前 failover 能否真的换号完全取决于那个开关
                // （`is_entry_selectable` 里的冷却硬门是唯一排除机制），线上它是 false ⇒
                // 一个真实 429 被放大成连环 429。全池都试过时排除集自动退化（允许重选），
                // 见 `acquire_context_excluding` 的不变量 1。
                //
                // 同号续跳（429 换桶 / L1 换区）跳过选号，沿用上一跳的 CallContext：
                // 摘排除集只让该号**可被**选中，并不能让它胜过更空闲的陪跑号。
                let same_cred_retry = reuse_ctx.is_some();
                let ctx = if let Some(c) = reuse_ctx.take() {
                    c
                } else {
                    match self
                        .token_manager
                        .acquire_context_excluding(
                            model.as_deref(),
                            session_id.as_deref(),
                            &tried_this_call,
                        )
                        .await
                    {
                        Ok(c) => c,
                        Err(e) => {
                            // 全池冷却快速失败(带 retry_after_secs / "冷却")归类为 RateLimited,
                            // 用量明细显示"限流"而非扎眼的"其它错误"(dwgx:那些其它错误 0/0 很恶心)。
                            let es = e.to_string();
                            if es.contains("retry_after_secs=") || es.contains("冷却") {
                                last_outcome = crate::usage::RequestOutcome::RateLimited;
                            }
                            last_error = Some(e);
                            continue 'attempt;
                        }
                    }
                };

                // 可观测:attempt>0 且真拿到了一个号 = 一次 failover 换号(真打了下一个号)。
                // 放在 acquire_context 成功之后,避免全池冷却 continue(没拿到号)误计一跳。
                // 同号续跳不是换号，不计 failover hop。
                if attempt > 0 && !same_cred_retry {
                    crate::common::recovery_metrics::bump_failover_hop();
                    real_failover_happened = true;
                }

                // 记入「本请求已试过」：下一跳 acquire_context_excluding 会优先避开它。
                // 必须在真正拿到号之后、发请求之前记 —— 记在发请求之后的话，一条在 send()
                // 处失败（网络错误 continue）的路径就不会被记入，下一跳又选它。
                tried_this_call.insert(ctx.id);
                // ⭐ 链内首选号（Kiro 侧兜底）：透传未试过（预算尚未置位）时，本路径首个
                // 拿到的号即整链首选；预算首写生效，不会覆盖透传已记的值（handlers 层
                // 先试透传再落本路径）。供失败记录的 `first_attempted_credential_id`。
                budget.note_first_attempt(ctx.id);

                // 配置来自循环外快照（见快照处的说明）：所有 failover 跳共用同一份。
                // `'classify` 是 labeled **block**（不是 loop）：无标签 continue/break 仍
                // 作用在外层 `for attempt`；`break 'classify` 只退出本块，好把 ctx 移进
                // reuse_ctx（块内 rctx 借用结束后才能 move）。
                let mut retry_same = false;
                'classify: {

                // ⭐ L1：本请求链内该号已被判定 region 错配 ⇒ 用换过的区建本次请求。
                //
                // 只在**真有覆盖**时才 clone 凭据：热路径上 99.99% 的请求走 `Borrowed`
                // 分支，零额外拷贝（`acquire_context` 已经 clone 过一次，再无条件多一次
                // 就是给每个正常请求加成本去伺候一个极少数的纠错路径）。
                let call_creds: std::borrow::Cow<'_, KiroCredentials> =
                    match region_override_this_call.get(&ctx.id) {
                        Some(region) => {
                            let mut c = ctx.credentials.clone();
                            c.api_region = Some(region.clone());
                            std::borrow::Cow::Owned(c)
                        }
                        None => std::borrow::Cow::Borrowed(&ctx.credentials),
                    };

                let machine_id = machine_id::generate_from_credentials(&call_creds, &config);

                let (selected_endpoint, alt_region) = match self.select_endpoint(&call_creds, ctx.id) {
                    Some(e) => e,
                    None => {
                        last_outcome = crate::usage::RequestOutcome::RateLimited;
                        last_error = Some(anyhow::anyhow!(
                            "凭据 #{} 所有端点桶均处于 429 封禁期（当前区与备用区的桶都在封禁中）retry_after_secs={}",
                            ctx.id,
                            self.shortest_endpoint_bucket_retry_after_secs(Some(ctx.id))
                        ));
                        // ⚠️ 不得 report_failure：None 代表**端点桶 30s 封禁**（瞬态），不是未知
                        // 端点配置错误。report_failure 会累计 failure_count → TooManyFailures
                        // 永久禁用健康号。设 30s 短冷却让调度避开，等桶解封。
                        if rate_limited_this_call.insert(ctx.id) {
                            self.token_manager.report_rate_limited_with_retry_after(
                                ctx.id,
                                Some(ENDPOINT_BUCKET_THROTTLE.as_secs()),
                            );
                        }
                        continue 'attempt;
                    }
                };

                // 🔴 备区生效：`select_endpoint` 判定「当前区所有桶都被 429 封禁」时给出备区，
                // 这里必须把它作用到**实际发请求的凭据**上 —— 否则 URL 仍打当前区，
                // 而 429 记账用的是备区桶键 ⇒ 封禁写进去读不到（对已 429 的上游持续轰炸）。
                //
                // 复用上面那套 `region_override_this_call` 的同款写法（`Cow` + 覆盖
                // `api_region`），而不是另造一条路径：两处若分叉，桶键同源这个不变量
                // 就会以最难查的形式破掉。
                let call_creds: std::borrow::Cow<'_, KiroCredentials> = match alt_region {
                    Some(r) => {
                        // ⭐ A-5 共享感知：429 备区换桶也置位「本请求已换区」标记
                        // （与 L1 403 换区同一份 `region_switched_this_call`，见其声明处）。
                        //
                        // 不置位的振荡路径（实测形态）：本号当前区全封 → 换到备区 A →
                        // A 区回 bearer-invalid 403 → L1 按「已换到的区」算
                        // `region_retry_target` 换回原区 → 原区桶还在 30s 封禁期 →
                        // select_endpoint 又把请求弹回备区 → 同一请求内 A→B→A→B，
                        // L1 的换区意图被彻底打空、白烧上游往返，最后仍落 report_failure。
                        // 置位后 L1 的门 `!contains(&ctx.id)` 直接挡住：本号本请求只换
                        // 一次区，惩罚换号交给下方通用分支。
                        //
                        // 只置位标记、**不**写 `region_override_this_call`：那是 L1/L2 的
                        // 「换区自纠正」通道（成功后把 api_region 持久化回写凭据），
                        // 备区换桶只是躲避**瞬态**封禁，写进去会让一次 30s 封禁把号的
                        // region 永久改掉，两条路径的语义就被污染了。
                        region_switched_this_call.insert(ctx.id);
                        let mut c = call_creds.into_owned();
                        c.api_region = Some(r.to_string());
                        std::borrow::Cow::Owned(c)
                    }
                    None => call_creds,
                };

                let rctx = RequestContext {
                    credentials: &call_creds,
                    token: &ctx.token,
                    machine_id: &machine_id,
                    config: &config,
                    is_1m,
                };

                last_credential_id = Some(ctx.id);

                // ⭐ 端点级链式回退（P0 移植，A-5 痛点修复）：
                // 上游 429/5xx 多为**端点级**容量问题而非凭据额度问题：同一凭据换到另一个
                // 上游端点常常立刻成功（kiro-go 的 `endpointFallback` 即此机制；参考仓 jsjm
                // 同款实现已实测 200）。链首 = `select_endpoint` 选中的端点（**桶机制 + EWMA
                // 健康分已应用**，即「先走桶内选端点」）；链内顺延在**同一凭据、同一轮 attempt**
                // 内立即重试：不消耗 max_retries 预算、不设凭据冷却、不扣健康分（这些只在整条
                // 链都失败、落到下方凭据级分类逻辑时发生）—— 这是与既有「跨轮换端点」
                // （429 封桶 → 下一轮 select_endpoint 换桶）正交的增量层。
                //
                // ⚠️ 但**每跳消耗共享预算**（`ABSOLUTE_MAX_TOTAL_RETRIES`，对抗审查 M1）：
                // 链内跳不触碰 attempt 计数、不设冷却、不扣健康分，却是真实上游调用——
                // 链循环顶部的预算闸保证整条客户端请求（含链式回退）总上游调用 ≤
                // ABSOLUTE_MAX_TOTAL_RETRIES，吸收层 `round_retry_quota(base, used())`
                // 的「进轮算一次」拦不住轮内追加的跳数，必须由闸补齐。
                //
                // 与参考仓的结构差异：参考仓在 acquire 后构造链并以配置端点为链首；本仓
                // select_endpoint 已按桶/健康分选过端点，故以**选中端点**为链首，再按凭据
                // 候选顺序（ksk_ 号 = CLI 族 4 端点）与 ENDPOINT_FALLBACK_ORDER 补齐。
                let upstream_region = call_creds.effective_upstream_region(&config).to_string();
                let chain = self.endpoint_chain_for(
                    &selected_endpoint,
                    &call_creds,
                    config.endpoint_fallback,
                    &upstream_region,
                );
                let mut chain_idx = 0usize;
                // 第四元组 = 是否「bail 整个 attempt 循环」（全局并发闸满：系统饱和，
                // 换号无意义，透传错误 —— 原 break 语义）；false = 链尾网络错误/凭据级
                // 闸满（原 continue 语义：退避后换号重试）。
                let (endpoint, response, last_url, bail_attempt_loop) = 'endpoint_chain: loop {
                    let candidate = &chain[chain_idx];

                    // ⭐ 链内共享预算闸（对抗审查 M1）：链式回退的每一跳都是**真实上游
                    // 调用**，必须受「每请求 ≤ ABSOLUTE_MAX_TOTAL_RETRIES 次上游调用」的
                    // 共享预算约束。此前的缺口：`round_retry_quota` 只在进轮时算一次，
                    // 拦得住「跨轮」却拦不住「轮内链式回退追加的跳数」——4 attempts ×
                    // 5 跳 = 20 次真实调用，共享预算账本 saturated 但实际超发，吸收层
                    // 一轮即死。预算耗尽 = 系统饱和（可能是 MCP/压缩/透传等其它层先
                    // 吃完的），换号无意义 → 与全局并发闸同语义，bail 整个 attempt
                    // 循环，透传已有错误让客户端自己退避。
                    if budget.remaining() == 0 {
                        tracing::warn!(
                            "每请求上游调用共享预算已用尽（{} 次），停止链式回退（尝试 {}/{}）",
                            ABSOLUTE_MAX_TOTAL_RETRIES,
                            attempt + 1,
                            max_retries
                        );
                        if last_error.is_none() {
                            last_error = Some(anyhow::anyhow!(
                                "每请求上游调用共享预算已用尽（ABSOLUTE_MAX_TOTAL_RETRIES={}），\
                                 停止链式回退并透传上游错误",
                                ABSOLUTE_MAX_TOTAL_RETRIES
                            ));
                        }
                        break 'endpoint_chain (candidate.clone(), None, candidate.api_url(&rctx), true);
                    }

                    // 死端点负缓存 / 协议隔离 / 桶封禁：跳过本跳（**链尾绝不跳过**：兜底铁律，
                    // 否则整条链无人发送、response 恒 None）。桶封禁用与 select 侧同款判据
                    // —— 链式回退加在桶机制**之上**，顺延同样避开已封禁桶，不破坏既有封禁语义。
                    if chain_idx + 1 < chain.len() {
                        if self.is_endpoint_dead(candidate.name(), &upstream_region) {
                            tracing::debug!(
                                "端点 {} 在 region {} 近期连接失败（负缓存 {}s 内），跳过本跳",
                                candidate.name(),
                                upstream_region,
                                DEAD_ENDPOINT_TTL.as_secs()
                            );
                            chain_idx += 1;
                            continue 'endpoint_chain;
                        }
                        if self.is_route_protocol_broken(candidate.name(), &upstream_region) {
                            tracing::debug!(
                                "端点 {} 在 region {} 近期返回非 event-stream 响应（协议隔离 {}s 内），跳过本跳",
                                candidate.name(),
                                upstream_region,
                                PROTOCOL_BROKEN_TTL.as_secs()
                            );
                            chain_idx += 1;
                            continue 'endpoint_chain;
                        }
                        if self
                            .endpoint_buckets
                            .lock()
                            .get(&(ctx.id, candidate.bucket_key(&call_creds, &config)))
                            .is_some_and(|&until| Instant::now() < until)
                        {
                            chain_idx += 1;
                            continue 'endpoint_chain;
                        }
                    }

                    let url = candidate.api_url(&rctx);

                // ⭐ 全局模型映射：**选号之后、发上游之前**改写模型名。
                // 白名单门（选号）只看**原始**模型名；改写后不再判白名单（用户拍板决定 3）。
                // 顺序：先映射 → 再 deepseek 归一化（deepseek 归一化在 `transform_api_body`
                // 内部/上游侧，映射必须在它之前，否则 deepseek 先把名压成 fallback，
                // 映射规则再也匹配不到原始名）。
                //
                // 每凭据豁免：`model_mapping_exempt=true` 完全跳过（安全阀 —— 覆盖
                // 「映射后名该号上游不认」的场景，见 `model_mapping` 模块文档）。
                //
                // 热路径开销控制：只有「映射命中且需改写」那一跳才做一次 `rewrite_model_id`
                // （全量解析+序列化），未命中零开销 —— 与 `extract_model_and_session` 的
                // 「一次解析」优化一致，不会每跳都全量解析 MB 级请求体。
                // 本跳改写的 body 与映射后名；None = 本跳未映射（原样转发）。
                let mapped_this_attempt: Option<(String, String)> =
                    if call_creds.model_mapping_exempt != Some(true) {
                        // model 可能为 None（请求体无 modelId）——空串不会命中任何规则，
                        // map_target 对空串返回 None，行为等价「未映射」。
                        let target = crate::kiro::model_mapping::map_target(
                            model.as_deref().unwrap_or_default(),
                            &mapping_rules,
                        );
                        if let Some(t) = target {
                            let mapped_body = Self::rewrite_model_id(request_body, &t);
                            // 改写成功（rewrite_model_id 解析失败会原样返回）才能认定映射生效；
                            // 若 body 非 JSON，mapped_body == request_body，映射实际没发生，
                            // 保守回落 None 而非谎报 mapped_model。
                            if mapped_body != request_body {
                                Some((mapped_body, t))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                // 🔴 修复（2026-08-11 全量审计）：`mapped_model` 必须每跳同步为**本跳**的真实
                // 映射结果，而不是「命中时覆盖、未命中保留旧值」。旧实现下混合豁免/非豁免号池
                // failover 后（第 1 跳非豁免命中映射、第 2 跳豁免原样转发），成功/失败记录里的
                // `upstream_model` 仍是**旧跳**的映射名，与实际服务模型错位。
                // 每跳统一同步：改写成功 → 映射后名；豁免/未命中/改写失败 → None（聚合层回落
                // `r.model`）。成功路径（CallMeta）与失败路径（fail_record）消费同一变量，
                // 天然一致，不会出现一边更新另一边残留旧值的 None 泄漏。
                mapped_model = mapped_this_attempt.as_ref().map(|(_, t)| t.clone());
                // 本跳实际发给上游的 body：映射命中用改写后的，否则原样。
                let body = match &mapped_this_attempt {
                    Some((mapped_body, _)) => candidate.transform_api_body(mapped_body, &rctx),
                    None => candidate.transform_api_body(request_body, &rctx),
                };

                let base = self
                    .client_for(&ctx.credentials)?
                    .post(&url)
                    .body(body)
                    .header("content-type", candidate.content_type());
                let request = candidate.decorate_api(base, &rctx);

                // ⭐ 全局上游并发闸：限制**同时在飞**的上游 HTTP 调用数（防放大）。
                //
                // 拿 `OwnedSemaphorePermit` 跨 `send().await` 存活、响应头拿到后离开本
                // 作用域自动 Drop 释放 —— 免费防泄漏。**不用 `acquire().await`**（无限等待
                // 会把客户端延迟堆到秒级，与 gate 满时"系统已饱和"的语义矛盾）：
                // `try_acquire_owned` 拿不到就 **break 本轮重试**（而非 continue 无 sleep 空转），
                // 把错误透传给客户端让它自己退避。
                //
                // ⚠️ 不递增 `upstream_calls`：闸门挡住的是"根本没发出去"的调用，不该占用
                // 「每请求 ≤ ABSOLUTE_MAX_TOTAL_RETRIES 次上游调用」的额度 —— 该不变量（含吸收层、墙钟闸门、
                // round_retry_quota）全部不受影响。
                let _gate = match self.upstream_gate.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        tracing::warn!(
                            "上游并发闸已满，本轮重试 break 以免放大（尝试 {}/{}）",
                            attempt + 1,
                            max_retries
                        );
                        // ⚠️ 只在还没有更具体错误时设置 gate-full 错误：链内若先有可吸收的
                        // 429（带 retry_after_secs），覆盖它会把这轮错误判成"不可吸收"而旁路
                        // 吸收层。`last_error` 已有值时保留原错误，仅 break 本轮。
                        if last_error.is_none() {
                            // 带 `upstream_gate_full=1` + `retry_after_secs` 供 handlers 的
                            // map_provider_error 识别成 429 + Retry-After（让客户端退避，
                            // 而不是 502 让客户端立即重发、重新灌满闸门）。
                            last_error = Some(anyhow::anyhow!(
                                "上游并发闸已满，停止本轮重试以免放大 upstream_gate_full=1 retry_after_secs=2"
                            ));
                            last_outcome = crate::usage::RequestOutcome::RateLimited;
                        }
                        break 'endpoint_chain (candidate.clone(), None, url, true);
                    }
                };

                // ⭐ 每凭据并发闸（第二级）：全局闸只管「总在飞 ≤ N」，不管**分布**。
                //
                // 没有这一级时的真实故障形态：某个号响应慢（上游对它排队而不是立刻 429），
                // 它的请求长时间占着全局许可 → 极端情况下全部许可被同一个慢号吃掉 →
                // 其余健康号拿不到许可，**整池吞吐被一个号拖死**，而症状显示为系统级的
                // 「并发闸已满」，排障时指不到是哪个号。
                //
                // 与全局闸同样用 `try_acquire_owned`（不等待）：语义一致 —— 拿不到就说明
                // 这个号已经打满，应当**换号**而不是排队等它。所以这里 `continue` 而非
                // `break`：break 会终止整条重试链（等于放弃本请求），而本号打满恰恰是
                // 「换下一个号」的最佳时机，池里其它号很可能是空闲的。
                //
                // 空转防护已由上游保证：本号在 `:1844` 就已加入 `tried_this_call`
                // （那行刻意放在"拿到号之后、发请求之前"），所以下一轮
                // `acquire_context_excluding` 会结构性避开它，不会出现「又选中它 →
                // 又拿不到许可 → 无 sleep 空转」。此处**不要**重复 insert。
                //
                // ⚠️ 同样不递增 `upstream_calls`：闸门挡住的请求**根本没发出去**，
                // 不该占用「每请求 ≤ ABSOLUTE_MAX_TOTAL_RETRIES 次上游调用」的额度。
                let _cred_gate = match self.per_credential_gate(ctx.id).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        tracing::debug!(
                            credential_id = ctx.id,
                            limit = self.upstream_per_credential_limit,
                            "凭据级并发闸已满，换号（尝试 {}/{}）",
                            attempt + 1,
                            max_retries
                        );
                        if last_error.is_none() {
                            last_error = Some(anyhow::anyhow!(
                                "凭据 #{} 并发闸已满（上限 {}），换号重试",
                                ctx.id,
                                self.upstream_per_credential_limit
                            ));
                        }
                        // 凭据级闸按号（ctx.id）计，链内换端点打的是同一个号 ⇒ 整链都会满。
                        // 非链尾继续顺延（无网络 I/O，纯空转开销），链尾交循环外「退避换号」
                        // （原 continue 语义，见循环外的 None 分支）。
                        if chain_idx + 1 < chain.len() {
                            chain_idx += 1;
                            continue 'endpoint_chain;
                        }
                        break 'endpoint_chain (candidate.clone(), None, url, false);
                    }
                };

                let send_result = request.send().await;
                // ⭐ 额度只在这里累加:此刻请求**已经发出去了**(无论上游怎么回、哪怕连接失败),
                // 才算真花掉一次「打上游」的机会。放在 send 之后而非循环顶部是本修复的全部内容。
                //
                // 网络错误(`Err`)也计:它同样占了一次出站连接 + 一次退避 sleep,不计会让
                // 「上游整体不可达」变成额度永不递减的死磨(每轮都拿满配额重打)。
                upstream_calls += 1;
                // 共享预算同步扣减（2026-08-11 方案 A）：跨层（websearch 轮/压缩轮/透传）
                // 共用同一「每请求」总额度，upstream_calls 只是本调用内的展示计数。
                budget.consume(1);
                match send_result {
                    Ok(resp) => {
                        let status = resp.status();
                        // 喂动态降档信号：**每个**上游响应都记一次（成功/4xx false，429/5xx true），
                        // 供 base_retry_quota 处的 apply_retry_pressure 收缩重试预算。
                        // 与 AIMD 的 report_upstream_rate_limited 是两套独立机制、两套门控，勿混。
                        //
                        // ⚠️ 5xx 必须也算压力（true）：纯 500 风暴同样是「疯狂重试」的来源，
                        // 若只计 429，5xx 落进"成功"桶会把 rate() 稀释到趋近 0 → 降档永不触发。
                        // 4xx（客户端错误）不算压力：它是请求本身的问题，不是上游过载信号。
                        let code = status.as_u16();
                        self.retry_pressure
                            .lock()
                            .record(code == 429 || code >= 500);
                        // 拿到 HTTP 响应 = 连接层通了（哪怕是 429/5xx）→ 清负缓存。
                        // 负缓存只针对"连不上"（DNS/TCP/TLS），绝不针对上游返回的业务错误。
                        self.mark_endpoint_alive(candidate.name(), &upstream_region);
                        // ⭐ 链式回退核心：瞬态错误（显式列表，见下）且还有备用端点 →
                        // 立即换下一端点重试。**不消耗 max_retries 预算、不设凭据冷却、
                        // 不扣健康分**（但每跳消耗共享预算，见链循环顶部的预算闸）
                        // —— 与下方「整链失败后交凭据级分类」的既有路径正交。列表里
                        // 的 5xx 是 MODEL_TEMPORARILY_UNAVAILABLE 一类的容量错误，换
                        // host 可能恰有容量，链内换端点无害；400 形态
                        // （INSUFFICIENT_MODEL_CAPACITY）不属于瞬态，仍走下方既有容量
                        // 分支（不惩罚凭据的语义保留）。
                        //
                        // 🔴 对抗审查 m4：瞬态判定**收窄为显式列表**，不再用
                        // `is_server_error()`（501/505 也被它顺延，白烧一跳）——
                        // 501 Not Implemented / 505 HTTP Version Not Supported 是网关
                        // 对请求的**确定性**答复，换 host 不会变；只认实测可恢复的
                        // 408（请求超时）/ 429（限流）/ 500 / 502 / 503 / 504
                        // （容量/网关抖动）/ 524（Cloudflare 上游超时）。
                        let transient =
                            matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504 | 524);
                        if transient && chain_idx + 1 < chain.len() {
                            // ⭐ 链首（select 选中的端点）被 429 证实容量满 → 封桶，让下一轮
                            // `select_endpoint` 避开它（与下方既有 429 分支**同守卫**：
                            // 凭据候选 >1 才封，否则 select 会因单候选全封而返回 None，把
                            // 瞬态封禁累成凭据级冷却）。顺延跳**不**封桶：它们是纯立即重试，
                            // 不产生调度状态（用户约束：链内跳不设冷却、不扣健康分）。
                            //
                            // 不封链首的后果（为什么必须有这行）：`select_endpoint` 的硬门
                            // 只认桶封禁、EWMA 只看健康分，而链内跳两样都不写 ⇒ 容量满的链首
                            // 健康分不降、桶不封 → 每轮都被选中 → 每请求白打一跳，且
                            // `has_unthrottled_endpoint` 恒判"还有可用桶"、凭据冷却永不触发。
                            if chain_idx == 0
                                && status.as_u16() == 429
                                && call_creds
                                    .effective_endpoint_order(&self.default_endpoint)
                                    .len()
                                    > 1
                            {
                                self.endpoint_buckets.lock().insert(
                                    (ctx.id, candidate.bucket_key(&call_creds, &config)),
                                    Instant::now() + ENDPOINT_BUCKET_THROTTLE,
                                );
                            }
                            tracing::warn!(
                                "端点 {} 返回瞬态错误 {}，链式回退到下一端点（凭据 #{} 不计失败、不耗重试预算，尝试 {}/{}）",
                                candidate.name(),
                                status,
                                ctx.id,
                                attempt + 1,
                                max_retries
                            );
                            chain_idx += 1;
                            continue 'endpoint_chain;
                        }
                        break 'endpoint_chain (candidate.clone(), Some(resp), url, false);
                    }
                    Err(e) => {
                        // 连接层失败：记负缓存（下次自动跳过此 (端点, region)）。
                        // reqwest::Error 的 `.is_connect()` 仅含 TCP connect 失败，DNS 归
                        // `.is_request()`，故综合判断 request/connect/timeout（避免漏掉
                        // DNS 不存在的场景）。
                        if e.is_connect() || e.is_timeout() || e.is_request() {
                            self.mark_endpoint_dead(candidate.name(), &upstream_region);
                            tracing::debug!(
                                "端点 {} 在 region {} 连接层失败，记入负缓存 (TTL {}s): {}",
                                candidate.name(),
                                upstream_region,
                                DEAD_ENDPOINT_TTL.as_secs(),
                                e
                            );
                        }
                        if chain_idx + 1 < chain.len() {
                            tracing::warn!(
                                "端点 {} 发送失败，链式回退到下一端点: {}",
                                candidate.name(),
                                e
                            );
                            chain_idx += 1;
                            continue 'endpoint_chain;
                        }
                        // 链尾：网络错误（上游 trace + 错误记录）。
                        tracing::warn!(
                            "API 请求发送失败（尝试 {}/{}）: {}",
                            attempt + 1,
                            max_retries,
                            e
                        );
                        // 上游 trace（P0-A）：网络错误无响应体，独立组装一条（status=None）。
                        // 守卫只覆盖「读到失败 body 之后」的分支，这里在守卫组装点之前。
                        if crate::kiro::upstream_trace::is_enabled() {
                            crate::kiro::upstream_trace::emit(
                                crate::kiro::upstream_trace::UpstreamTrace {
                                    ts: chrono::Utc::now().to_rfc3339(),
                                    credential_id: ctx.id,
                                    endpoint: candidate.name().to_string(),
                                    url: url.clone(),
                                    region: call_creds
                                        .effective_upstream_region(&config)
                                        .to_string(),
                                    model: model.clone(),
                                    attempt: attempt as u32,
                                    absorb_round,
                                    upstream_calls,
                                    status: None,
                                    retry_after_raw: None,
                                    retry_after_secs: None,
                                    body: None,
                                    network_error: Some(crate::kiro::upstream_trace::sanitize_body(
                                        &e.to_string(),
                                    )),
                                    latency_ms: call_started.elapsed().as_millis() as u64,
                                    verdict: "network_error".to_string(),
                                    cred_ever_succeeded: self
                                        .token_manager
                                        .has_ever_succeeded(ctx.id),
                                },
                            );
                        }
                        // 网络错误通常是上游/链路瞬态问题，不应导致"禁用凭据"或"切换凭据"
                        // （否则一段时间网络抖动会把所有凭据都误禁用，需要重启才能恢复）
                        last_error = Some(e.into());
                        last_outcome = crate::usage::RequestOutcome::NetworkError;
                        break 'endpoint_chain (candidate.clone(), None, url, false);
                    }
                }
                };

                let response = match response {
                    Some(resp) => resp,
                    None => {
                        if bail_attempt_loop {
                            // 全局并发闸已满：停止本轮重试并透传错误（原 break 语义）。
                            break 'attempt;
                        }
                        // 整条端点链都发送失败（网络层）或凭据级闸满：错误已在链内记录。
                        // 与改动前逐字节一致的收尾（sleep + 换号重试）。
                        if attempt + 1 < max_retries {
                            sleep(Self::retry_delay(attempt)).await;
                        }
                        continue 'attempt;
                    }
                };

                let status = response.status();

                // 成功响应
                if status.is_success() {
                    self.token_manager.report_success(ctx.id);
                    // 上游 trace（P0-A）：守卫不覆盖成功路径（成功时 body 还没读，也不该读），
                    // 成功侧用独立 emit 直接发一条 verdict="success"（body 恒 None，对话内容绝不落盘）。
                    if crate::kiro::upstream_trace::is_enabled() {
                        crate::kiro::upstream_trace::emit(
                            crate::kiro::upstream_trace::UpstreamTrace {
                                ts: chrono::Utc::now().to_rfc3339(),
                                credential_id: ctx.id,
                                endpoint: endpoint.name().to_string(),
                                url: last_url.clone(),
                                region: call_creds
                                    .effective_upstream_region(&config)
                                    .to_string(),
                                model: model.clone(),
                                attempt: attempt as u32,
                                absorb_round,
                                upstream_calls,
                                status: Some(status.as_u16()),
                                retry_after_raw: None,
                                retry_after_secs: None,
                                body: None,
                                network_error: None,
                                latency_ms: call_started.elapsed().as_millis() as u64,
                                verdict: "success".to_string(),
                                cred_ever_succeeded: true,
                            },
                        );
                    }
                    // 端点自适应派发：这个端点**受理了**这个凭据 → 记一次成功。
                    // 与 `report_success`（凭据健康）分开记：两者维度不同，一个号可能在
                    // 端点 A 上恒 200、在端点 B 上恒 400，凭据级健康分看不出这种差异。
                    self.report_endpoint_outcome(ctx.id, endpoint.name(), true);

                    // ⭐ L2：换区**成功后**立刻把这个区回写进 `api_region` 并持久化。
                    //
                    // 时机是承重的：只有走到这里，那个区才从「猜测」变成**已验证事实**
                    // （这个号在这个区真拿到了 200）。回写早于此就是拿未验证的猜测覆盖配置。
                    //
                    // ⇒ 第一次自我纠正之后就写死，后续请求零额外开销。这比「每次都试两个区」
                    // 的无状态做法省掉一次往返，也不再依赖任何外部脚本预先喂 region。
                    //
                    // 只对 `api_key` 号：OAuth 号的权威 region 是 `profileArn`
                    // （`effective_upstream_region` 第一优先），回写 `api_region` 对它**不生效**，
                    // 只会在面板上留一个看起来生效其实被压住的值，把排障带偏。
                    // （`region_retry_target` 已在入口拦掉非 api_key，这里是第二道 —— 判据
                    //   两处都写是刻意的：将来若有人放宽入口那道门，这里仍不会写坏 OAuth 号。）
                    if let Some(region) = region_override_this_call.get(&ctx.id) {
                        if ctx.credentials.is_api_key_credential() {
                            // ⚠️ 回写失败**绝不让请求失败**：本次请求已经用新区成功了，
                            // 回写只是让下次省一跳。把它变成硬失败等于用一个纯优化项
                            // 去否掉一个已经成功的响应。
                            if let Err(e) = self
                                .token_manager
                                .set_credential_api_region(ctx.id, Some(region.clone()))
                            {
                                tracing::warn!(
                                    "凭据 #{} 换区成功但回写 api_region={} 失败（本次请求不受影响，\
                                     下次仍需重新换区一次）: {}",
                                    ctx.id,
                                    region,
                                    e
                                );
                            } else {
                                tracing::info!(
                                    "凭据 #{} region 自纠正完成：api_region 已写死为 {}（后续请求零额外开销）",
                                    ctx.id,
                                    region
                                );
                            }
                        }
                    }
                    // 可观测:吸收层真把一个本该回给客户端的 429 救回来了(客户端全程未见 429)。
                    // 只在 absorb_round > 0 时计,否则每个正常成功请求都会被记成"吸收成功"。
                    if absorb_round > 0 {
                        crate::common::recovery_metrics::bump_absorb_recovered();
                        tracing::info!(rounds = absorb_round, "吸收层重试成功，客户端未见 429");
                    }
                    let meta = CallMeta {
                        credential_id: ctx.id,
                        model: client_model_owned.clone().or_else(|| model.clone()),
                        // 映射后名（仅映射命中并改写时 Some，否则 None=未映射）。注意：
                        // failover 跨多跳时取**最后一跳**的映射结果（2026-08-11 修复：
                        // 每跳同步，最后一跳未映射/豁免时同样置 None，不再残留旧跳值），
                        // 与响应实际由哪跳返回一致。
                        mapped_model: mapped_model.clone(),
                        session_id: session_id.clone(),
                        is_streaming: is_stream,
                        // 跨吸收轮累计:客户端视角的一条请求总共换了多少次号。
                        retries: attempts_base + attempt as u32,
                        latency_ms: call_started.elapsed().as_millis() as u64,
                        started_at: call_started,
                        // 移交在途守卫：从此随响应流存活，流真正消费完才 -1
                        inflight: ctx.inflight,
                    };
                    return Ok((response, meta));
                }

                // 失败响应：先从响应头提取 Retry-After（body 消费后头就没了），再读取 body。
                // 原始串与解析值都要：trace 存原值；秒数认整数或 HTTP-date。
                let retry_after_raw = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.trim().to_string());
                let retry_after_header = retry_after_raw
                    .as_deref()
                    .and_then(parse_retry_after_header_value);
                let body = response.text().await.unwrap_or_default();

                // ── 上游 trace 失败守卫（P0-A）────────────────────────────────
                // 成功路径在 body 读取前已 return，守卫只覆盖失败分支；`verdict` 由下方
                // 各失败分支打标签（401/403 大分支先标粗标签、子出口再覆盖），漏标的分支
                // 自然落 unclassified（验收脚本据此统计）。
                let mut trace_guard = crate::kiro::upstream_trace::FailureTraceGuard::new(
                    crate::kiro::upstream_trace::is_enabled(),
                    || crate::kiro::upstream_trace::UpstreamTrace {
                        ts: chrono::Utc::now().to_rfc3339(),
                        credential_id: ctx.id,
                        endpoint: endpoint.name().to_string(),
                        url: last_url.clone(),
                        region: call_creds.effective_upstream_region(&config).to_string(),
                        model: model.clone(),
                        attempt: attempt as u32,
                        absorb_round,
                        upstream_calls,
                        status: Some(status.as_u16()),
                        retry_after_raw: retry_after_raw.clone(),
                        retry_after_secs: retry_after_header,
                        body: Some(crate::kiro::upstream_trace::sanitize_body(&body)),
                        network_error: None,
                        latency_ms: call_started.elapsed().as_millis() as u64,
                        verdict: crate::kiro::upstream_trace::VERDICT_UNCLASSIFIED.to_string(),
                        cred_ever_succeeded: self.token_manager.has_ever_succeeded(ctx.id),
                    },
                );

                // 订阅不覆盖本应用/模型：**永久**条件，换区与重试都无效 → 立即终止。
                //
                // 必须排在下方所有 403 分支之前：那些分支分别会换区（L1）、设短冷却后
                // failover、或计入凭据失败，而本条三者都不该做 ——
                // 实测同一把 key 在 `q.us-east-1` 回「bearer token invalid」（该区未授权，
                // 归 L1 换区）、在 `q.eu-central-1` 回本条文案（区是对的、token 是对的，
                // 订阅不覆盖）。换区拿到的还是同一个错，重试同理，只是白烧上游往返。
                //
                // 不计凭据失败（不走 report_failure）：号本身没坏，是订阅档位不含该应用/模型，
                // 记成凭据失败会在 3 次后把它自动禁用，把「换个模型就能用」误报成「号废了」。
                // 上游原话**原样带进错误消息**：本条加入前该文案全仓零命中，运维只能看到
                // 网关自己的推测（「订阅档位或成本白名单」二选一），归因要靠猜。
                if endpoint.is_subscription_unsupported(&body) {
                    trace_guard.verdict("subscription_unsupported");
                    tracing::warn!(
                        "API 请求失败（订阅不覆盖本应用/模型，永久条件；不换区、不重试、\
                         不计凭据失败）: {} {}",
                        status,
                        body
                    );
                    last_outcome = crate::usage::RequestOutcome::BadRequest;
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败（订阅不支持该应用/模型，换区与重试均无效）: {} {} \
                         subscription_unsupported=1",
                        api_type,
                        status,
                        body
                    ));
                    break 'attempt;
                }

                // 客户端请求校验错误（如 TOOL_USE_RESULT_MISMATCH / TOOL_SCHEMA_INVALID）：请求构造问题，
                // 换号/重试都只会重复失败并浪费配额，立即终止（不计凭据失败）。
                // `is_client_validation_error` 覆盖 TOOL_USE_RESULT_MISMATCH；TOOL_SCHEMA_INVALID
                // 是同一语义（客户端工具 schema 非法，非上游故障）的另一 reason（ZyphrZero/kiro.rs
                // endpoint/mod.rs 的 CLIENT_VALIDATION_REASONS 两者都收），此处补认。
                if endpoint.is_client_validation_error(&body)
                    || body.contains("TOOL_SCHEMA_INVALID")
                {
                    trace_guard.verdict("client_validation");
                    tracing::warn!(
                        "API 请求失败（客户端请求校验错误，不重试）: {} {}",
                        status,
                        body
                    );
                    last_outcome = crate::usage::RequestOutcome::BadRequest;
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败（请求校验错误）: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    break 'attempt;
                }

                // 账户级临时风控限速（suspicious activity + temporary limits）：
                // ⚠️ 必须在 is_account_suspended 之前判定，否则含 "suspended...suspicious
                // activity" 的临时限速文案会被误判成永久封禁，白冻一个还能用的号 24h。
                // 处置：只设短冷却 + 立即 failover，不禁用、不计永久失败。
                if endpoint.is_temporary_rate_limit(&body) {
                    trace_guard.verdict("temporary_rate_limit");
                    tracing::warn!(
                        "API 请求失败（账户临时风控限速，非永久封禁；短冷却后 failover，尝试 {}/{}）: {} {}",
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );
                    last_outcome = crate::usage::RequestOutcome::RateLimited;
                    // 账户级风控也是上游限速信号 → 入站整形 RPM 自动降档。
                    // 只在第 0 轮上报(见本文件 'absorb 循环处的 AIMD 放大说明)。
                    if absorb_round == 0 {
                        self.token_manager.report_upstream_rate_limited();
                    }
                    // 账户级可疑活动风控：走分钟级退避（report_suspicious_activity），而非普通
                    // 429 的 15s 瞬时冷却。本请求链内该号首次触发才设冷却；再次触发只 failover，
                    // 不重复惩罚（同 rate_limited_this_call 去重，避免一条链把号砸进更深风控）。
                    if rate_limited_this_call.insert(ctx.id) {
                        self.token_manager.report_suspicious_activity(ctx.id);
                    } else {
                        tracing::debug!(
                            "凭据 #{} 本请求链内已因风控冷却过，再次触发仅 failover，不重复惩罚",
                            ctx.id
                        );
                    }
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败（账户级可疑活动风控，分钟级退避）: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    // 跨号转移上限：超过即停止遍历，把错误透传给客户端自行退避。
                    // 不设上限就会线性扫全池（实测 43 号 → 尝试 43/43 → 45s 墙钟），
                    // 既让用户干等，又把整池号一起送进上游风控。
                    suspicious_failovers_this_call += 1;
                    if suspicious_failovers_this_call >= MAX_SUSPICIOUS_FAILOVERS_PER_CALL {
                        tracing::error!(
                            "本次请求已因账户级风控转移 {} 次号，停止遍历号池并透传错误\
                         （避免扫冷全池 + 同出口 IP 连续触发风控）",
                            suspicious_failovers_this_call
                        );
                        break 'attempt;
                    }
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue 'attempt;
                }

                // 注：524 网关超时（Cloudflare 等）落入下方通用 5xx 分支即按可重试瞬态
                // 错误处理（不禁用、退避后换号），无需单列——与通用路径行为一致。

                // 402 Payment Required 且额度用尽：禁用凭据并故障转移
                // 🔴 **刻意不门控状态码** —— 只认 body 里的额度信号。
                //
                // 旧代码是 `status == 402 && is_monthly_request_limit(&body)`，而线上实测
                // （2026-08-05，6 小时窗口）：
                //   · `402 Payment Required` 出现 **0 次**
                //   · `400 Bad Request` + `"reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"` 出现 **564 次**
                // ⇒ 那道 402 门**从不成立** ⇒ 564 个「额度已耗尽」的请求全部落到下方通用
                // 400 分支 `break` 掉，凭据**不被禁用、继续留在轮转里**，每个新请求都再撞一次。
                // 实测 #508 一个号就吃了 543 次。这正是「大量 400 没有自动禁用」的成因。
                //
                // 为什么改成只看 body：额度耗尽是**账号级终态**，上游用哪个状态码表达它是
                // 上游的自由（它已经从 402 改到 400 了）。而 `is_monthly_request_limit`
                // 的判据是 `MONTHLY_REQUEST_COUNT` / `OVERAGE_REQUEST_LIMIT_EXCEEDED`
                // 两个**明确的 reason 字面量**（`endpoint/mod.rs:235`），本身已经足够窄 ——
                // 用它当唯一判据比再叠一个会漂的状态码更稳。
                //
                // ⚠️ 位置必须在通用 400 分支**之前**（本分支现在就在那之前）；挪到之后即失效。
                if endpoint.is_monthly_request_limit(&body) {
                    trace_guard.verdict("monthly_limit");
                    tracing::warn!(
                        "API 请求失败（额度已用尽，禁用凭据并切换，尝试 {}/{}）: {} {}",
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );

                    last_outcome = crate::usage::RequestOutcome::QuotaExhausted;
                    let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                    if !has_available {
                        // 🔴 带**显式标记** `quota_exhausted_all=1`（2026-08-10 补）。
                        //
                        // 为什么不能只靠中文文案：`handlers.rs` 的 `translate_quota_subscription`
                        // 原先用裸串 `contains("MONTHLY_REQUEST_COUNT") || contains("QUOTA")`
                        // 判「月度配额耗尽」，而**这两个串来自上游 body**，单号耗尽时的
                        // `last_error`（下面那条 continue 分支）同样带着它们 ——
                        // 且 `last_error` 是**刻意不重置**的（见 'absorb 循环末尾的说明）⇒
                        // 池里其余号明明健康，最终错误却被判成"全部配额耗尽"，归因口径被污染。
                        //
                        // 这与本仓既有的 `pool_permanently_exhausted=1` /
                        // `model_unsupported_by_pool=1` 是同一范式（`handlers.rs:1481` 注释
                        // 已写明「用显式标记而非中文文案匹配」）—— 这处是移植时漏掉的一环。
                        // 参考实现：kiro2cc-proxy 用 `QUOTA_EXHAUSTED_ALL_MARKER` 做同一件事。
                        last_error = Some(anyhow::anyhow!(
                            "{} API 请求失败（所有凭据已用尽）quota_exhausted_all=1: {} {}",
                            api_type,
                            status,
                            body
                        ));
                        break 'attempt;
                    }
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    continue 'attempt;
                }

                // 账户被暂停/封禁：不论状态码，body 命中 suspend 信号即直接禁用并转移
                // （不可自动恢复，等待人工处理，避免反复打已封的号）
                if endpoint.is_account_suspended(&body) {
                    trace_guard.verdict("account_suspended");
                    tracing::error!(
                        "API 请求失败（账户被暂停/封禁，禁用凭据并切换，尝试 {}/{}）: {} {}",
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );
                    last_outcome = crate::usage::RequestOutcome::AccountSuspended;
                    // suspend 是账号级风控信号：同样让入站 AIMD 降档，否则网关会继续按原速率
                    // 往正在拒绝我们的上游灌流量，把风控进一步激化（此前 AIMD 只认 429）。
                    // 只在第 0 轮上报(见本文件 'absorb 循环处的 AIMD 放大说明)。
                    if absorb_round == 0 {
                        self.token_manager.report_upstream_pressure();
                    }
                    let has_available = self.token_manager.report_account_suspended(ctx.id);
                    if !has_available {
                        last_error = Some(anyhow::anyhow!(
                            "{} API 请求失败（账户被封禁且所有凭据已用尽）: {} {}",
                            api_type,
                            status,
                            body
                        ));
                        break 'attempt;
                    }
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败（账户被暂停）: {} {}",
                        api_type,
                        status,
                        body
                    ));

                    // ⚠️ 每请求最多因 suspend 转移**一次**，且转移前退避。
                    //
                    // 此前这里是裸 `continue`（无 sleep、无冷却，而 report_account_suspended
                    // 也不设冷却），于是一条客户端请求会在几秒内用 8~12 个不同账号打同一端点、
                    // 同一出口 IP —— 日志里的「尝试 8/36」就是第 8 个号被烧。这正是风控要抓的
                    // 突发特征：我们在放大自己的封禁（实测 12 小时 88 次 suspend 禁用）。
                    //
                    // 限一次的理由：suspend 是**账号级**信号，多半伴随同出口 IP 的整体风控。
                    // 既然第一个号已被判定，继续遍历全池极可能把剩下的号一起烧掉，而本次请求
                    // 成功率并不会因此提高。宁可这一条请求失败，也不要赔掉整个号池。
                    if suspended_this_call {
                        tracing::error!(
                            "本次请求已因账户暂停转移过一次，不再遍历号池（避免同 IP 连续触发风控）"
                        );
                        break 'attempt;
                    }
                    suspended_this_call = true;
                    tokio::time::sleep(Self::retry_delay(attempt)).await;
                    continue 'attempt;
                }

                // 400 INVALID_MODEL_ID：该号已不能服务请求的模型（多为订阅取消/降级）。
                // 不是客户端请求错误——换个订阅仍有效的号往往能成功。故给该号冷却 + failover，
                // 而非直接把 400 透传（那样坏号还留在轮转里，下个请求又命中它）。
                // 只有当所有号都返回它（report 返回 has_available=false）时，才是模型本身无效、透传。
                if status.as_u16() == 400 && endpoint.is_invalid_model_id(&body) {
                    trace_guard.verdict("invalid_model_id");
                    last_outcome = crate::usage::RequestOutcome::BadRequest;
                    // 模型级处置：只把"该号+该模型"记进短期黑名单并 failover 到对此模型仍可用的号；
                    // 绝不冷却/禁用整个号（该号对其它模型照常可用）。返回 false = 所有未禁用号都已对
                    // 此模型进黑名单 → 说明是模型本身无效，透传真 400 给客户端(而非 429/502 死循环)。
                    let has_available_for_model = self
                        .token_manager
                        .report_model_invalid(ctx.id, model.as_deref());
                    if !has_available_for_model {
                        last_error = Some(anyhow::anyhow!(
                            "{} API 请求失败（模型 {:?} 对所有号均 INVALID_MODEL_ID，判定模型无效）: {} {}",
                            api_type,
                            model.as_deref().unwrap_or(""),
                            status,
                            body
                        ));
                        // 透传真实 400：这是客户端请求了一个所有号都不支持的模型，重试无意义。
                        break 'attempt;
                    }
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败（凭据 #{} 对模型 {:?} INVALID_MODEL_ID，切换到仍支持的号）: {} {}",
                        api_type,
                        ctx.id,
                        model.as_deref().unwrap_or(""),
                        status,
                        body
                    ));
                    continue 'attempt;
                }

                // ⭐ 400 + 模型容量不足 —— **必须排在下面那条通用 400 之前**。
                //
                // 上游对「模型没容量」发过两种形态：503 `MODEL_TEMPORARILY_UNAVAILABLE`，
                // 以及 400 `ThrottlingException` + `reason:INSUFFICIENT_MODEL_CAPACITY`。
                // 后者的 HTTP 状态是 400，于是会被下面那条通用 400 分支**先接住并 break**，
                // 而真正的容量处置（慢速退避 + 不惩罚凭据健康）在本函数更后面（约 :1588）
                // ——**永远走不到**。
                //
                // 实测坐实这个顺序缺陷：修复上线后（19:05:15）逐分钟仍全部落 `bad_request`
                // （19:19 / 19:21 / …… / 19:45），近 6h 共 590 次。而当时 endpoint 判据、
                // provider 状态门、handlers 映射三处都已改对、四条测试全绿 —— 因为那些测试
                // 测的是纯函数与 `include_str!` 状态门守卫，**没有一条走 provider 的真实分支链**，
                // 所以顺序错误对它们完全不可见。
                //
                // 这里只做「转交」：不复制那套处置逻辑（复制必然漂移），而是让它落到下方
                // 统一的容量分支。用 `continue` 之外的方式表达"别被通用 400 吃掉"。
                let is_capacity_400 =
                    status.as_u16() == 400 && endpoint.is_model_temporarily_unavailable(&body);

                // 400 Bad Request - 其它请求问题（客户端构造错误），重试/切换凭据无意义
                if status.as_u16() == 400 && !is_capacity_400 {
                    trace_guard.verdict("generic_400");
                    last_outcome = crate::usage::RequestOutcome::BadRequest;
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    break 'attempt;
                }

                // 401/403 - 更可能是凭据/权限问题：计入失败并允许故障转移
                if matches!(status.as_u16(), 401 | 403) {
                    // 外层先标粗标签，子出口再覆盖成更精确的名字（verdict 最后一次写入生效）。
                    trace_guard.verdict("auth_4xx");
                    tracing::warn!(
                        "API 请求失败（可能为凭据错误，尝试 {}/{}）: {} {}",
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );

                    // region 自动纠正一条龙:403 FEATURE_NOT_SUPPORTED = 该 region 的 profile 未开通。
                    // 这**不是**凭据坏(号本身好、只是 region 配错),绝不当普通 401/403 冷却 + 换号误伤它。
                    // 处置(对抗复核裁决:昂贵 reprobe 绝不上同步对话热路径):
                    //   ① 廉价本地纠正 sync_region_from_arn(纯字符串,无网络)——修"region 字段与 ARN 漂移";
                    //   ② 置 flag + 触发 per-id 守卫的**后台异步**重探(不阻塞本请求,为后续请求恢复);
                    //   ③ 仅当本地纠正真改了 region 且本链未纠正过 → continue 重试一次(不 report_failure);
                    //   否则落下方 report_failure + failover(本请求换号,重探已在后台启动)。
                    // 非 external_idp 号(social/idc)第二条件即短路,行为逐字不变。
                    if status.as_u16() == 403
                        && endpoint.is_feature_not_supported(&body)
                        && ctx.credentials.is_external_idp_credential()
                    {
                        trace_guard.verdict("region_feature_403");
                        let corrected = self.token_manager.sync_region_from_arn_for(ctx.id);
                        self.token_manager
                            .mark_usage_403_feature_not_supported(ctx.id);
                        self.token_manager.trigger_background_reprobe(ctx.id);
                        if corrected
                            && region_corrected_this_call.insert(ctx.id)
                            && call_started.elapsed()
                                < std::time::Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS)
                        {
                            tracing::info!(
                                "凭据 #{} 403 FEATURE_NOT_SUPPORTED:已本地纠正 region,同号重试一次(不冷却)",
                                ctx.id
                            );
                            last_outcome = crate::usage::RequestOutcome::ServerError;
                            last_error = Some(anyhow::anyhow!(
                                "{} 403 FEATURE_NOT_SUPPORTED(已本地纠正 region 重试): {} {}",
                                api_type,
                                status,
                                body
                            ));
                            // continue → 下一轮 acquire_context 重克隆已改好 region 的 creds(不复用旧 ctx/url)。
                            continue 'attempt;
                        }
                        // 本地纠不动(ARN region 本身就是未开通那个,常见)→ failover 换号服务本请求,
                        // 后台异步重探已启动为该号后续请求恢复。给该号一段**认证冷却**(临时跳过、非禁用、
                        // 不累计失败),让调度本链内避开它、别反复选回来空撞 403;冷却到期或后台重探成功后
                        // 自动恢复。绝不 report_failure 连坐(region 配错≠号坏,隔离铁律)。
                        tracing::info!(
                            "凭据 #{} 403 FEATURE_NOT_SUPPORTED:本地纠正无效,冷却+failover 换号(后台重探已启动)",
                            ctx.id
                        );
                        last_outcome = crate::usage::RequestOutcome::ServerError;
                        // ⭐ 必须是**瞬态**冷却：上面三行刚 `trigger_background_reprobe`,
                        // 这条路径的全部设计前提就是「后台重探会把 region 修对,该号随后自愈」
                        // （见上方注释「冷却到期或后台重探成功后自动恢复」）。
                        // 而 `report_auth_cooldown` 落的 `AuthenticationFailed`
                        // `is_auto_recoverable=false` ⇒ 实际是 86400s 硬窗 ——
                        // 注释承诺的自愈**永远不会发生**,重探成功了号也回不了池。
                        // `AuthTransient` 的 20s 基线正好覆盖一次重探往返;若重探更慢,
                        // 该号回池再撞一次 403 只是让 1.3^n 递增(上限 90s)、不计失败。
                        self.token_manager.report_auth_transient_cooldown(ctx.id);
                        last_error = Some(anyhow::anyhow!(
                            "{} 403 FEATURE_NOT_SUPPORTED(region 未开通,冷却换号,后台重探中): {} {}",
                            api_type,
                            status,
                            body
                        ));
                        // continue:下一轮 acquire_context 选别的号;全池不可用时由 max_retries/墙钟兜底透传。
                        continue 'attempt;
                    }

                    // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会。
                    // ⚠️ api_key 号跳过 —— 理由与对话路径同处的长注释一致（结构上不可能成功，
                    // 且失败会计入失败 + 落冷却 + 被瞬态判据重试 3 次，把死亡速度放大三倍）。
                    if endpoint.is_bearer_token_invalid(&body)
                        && !force_refreshed.contains(&ctx.id)
                        && !ctx.credentials.is_api_key_credential()
                    {
                        force_refreshed.insert(ctx.id);
                        tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                        if self
                            .token_manager
                            .force_refresh_token_for(ctx.id)
                            .await
                            .is_ok()
                        {
                            tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                            continue 'attempt;
                        }
                        tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                        // 刷新失败 = 认证态有问题，加一段冷却让调度避开它。
                        // 时长按「该号是否被证明过」二分 —— 理由与 MCP 路径同处逐字同款
                        // （刷新层已内部重试过瞬态错误，故到这里的抖动不该换来 24h 硬冻；
                        // 但从未成功过的号刷新还失败 = refreshToken 大概率真废了）。
                        if self.token_manager.has_ever_succeeded(ctx.id) {
                            self.token_manager.report_auth_transient_cooldown(ctx.id);
                        } else {
                            self.token_manager.report_auth_cooldown(ctx.id);
                        }
                    }

                    last_outcome = crate::usage::RequestOutcome::AuthFailed;

                    // 🔴 `bearer token invalid` 打在**已经成功过**的号上 = 瞬态，不计失败。
                    //
                    // 同一句上游文案含义相反：
                    // - 从未成功过 → 大概率 region 错配（`ksk_` 按 region 授权，打错区恒 403），
                    //   该计失败、该被禁用（实测 3 个从未成功的号共吃 17 次，那是真错配）。
                    // - 已经成功过 → token 对该端点**证明有效**，403 只能是抖动
                    //   （实测 4 个成功过的号累计 3393 次成功、共吃 42 次这种 403）。
                    //
                    // 为什么 `failure_count` 的「连续」语义兜不住：`report_success` 确实归零它，
                    // 但那要求成功**先落地**。高并发下同一秒内成功与失败交错（实测单号 60+ RPM），
                    // 三个并发请求各自 +1 就到阈值，中间没有成功插进来。实测 #481：2412 次成功、
                    // 93.9% 成功率，仍在 1 秒内被 3 次瞬态 403 推到 `TooManyFailures`
                    // → 池子少一个号 → 剩下的吃更多流量 → 更容易撞惩罚窗口。
                    // 当天全池 116 次禁用 / 42 次自愈，池子一直在抖。
                    //
                    // 处置与 `is_temporary_rate_limit` 同款：设短冷却让调度避开它 + failover，
                    // **不** `report_failure`。冷却会自动恢复，真错配的号（从未成功）不受影响。
                    let bearer_invalid_but_proven = endpoint.is_bearer_token_invalid(&body)
                        && self.token_manager.has_ever_succeeded(ctx.id);
                    if bearer_invalid_but_proven {
                        trace_guard.verdict("bearer_invalid_transient");
                        tracing::warn!(
                            "凭据 #{} 收到 bearer-invalid 403，但它已成功过 ⇒ 判为瞬态：\
                         只设短冷却 + failover，不计失败（防高并发下 3 次抖动把健康号打死）",
                            ctx.id
                        );
                        if auth_failed_this_call.insert(ctx.id) {
                            // ⭐ 上面那句 warn 自称「只设短冷却」，而 `report_auth_cooldown`
                            // 落的 `AuthenticationFailed` 实际是 24h 硬窗
                            // （`is_auto_recoverable=false` ⇒ long_cooldown 86400s）——
                            // 注释与实现分叉，且分叉的方向恰好抵消了本分支存在的意义：
                            // 本分支的全部目的就是「别把已证明健康的号（实测 #481：2412 次
                            // 成功、93.9% 成功率）因几次抖动打死」，落 24h 只是把
                            // 「被禁用」换成「更难发现的冷却僵尸」。
                            // `bearer_invalid_but_proven` 已含 `has_ever_succeeded`，
                            // 正是 `AuthTransient` 的判据，这里无需再判。
                            self.token_manager.report_auth_transient_cooldown(ctx.id);
                        }
                        // ⭐ 机器可读标记 `bearer_invalid_transient=1`（同款范式:
                        // `pool_permanently_exhausted=1` / `model_unsupported_by_pool=1` /
                        // `inbound_admission_timeout=1`）。中文文案保留给人读。
                        //
                        // 为什么必须有:上面这个二分（`has_ever_succeeded`）是**只有这里**才做得出的
                        // 判断 —— handler 层拿到的只有一个错误字符串,而 region 错配与瞬态抖动
                        // 在上游文案上**逐字节相同**（都是那句 bearer-invalid + 403）。
                        // 于是 `is_upstream_region_mismatch_403` 会把这条已证明健康的号也判成
                        // region 坏:① 给出错误的排障方向（去改 region,而号本来就是对的）;
                        // ② 状态码从 502（在外挂 kiro_shield 的 RETRYABLE 集内、会重试）变成
                        // 403（4xx 不重试）⇒ 一次纯抖动被固化成客户端可见的硬失败。
                        //
                        // ⚠️ 字面量逐字节承重:handlers 侧按它做排除。改名/改大小写/加空格都会
                        // 让那条排除静默失效（回到误判），且编译不报错。
                        last_error = Some(anyhow::anyhow!(
                            "{} API 请求失败（token 瞬态失效，已冷却换号）bearer_invalid_transient=1: {} {}",
                            api_type,
                            status,
                            body
                        ));
                        continue 'attempt;
                    }

                    // ⭐ L1：**从未成功过**的号吃 bearer-invalid 403 ⇒ 判 region 错配，换区重试。
                    //
                    // 顺序是承重的，本分支必须落在这两条之后：
                    //   ① `status == 403` 门 ⇒ **401 先让路**。token 死了 ≠ 区错了：401 该走
                    //      force-refresh / 计失败，换区对它毫无作用（换个区照样是死 token）。
                    //   ② 上面那条 `bearer_invalid_but_proven` 已 `continue` ⇒ **已成功过的号
                    //      到不了这里**。两条分支吃的是**逐字节相同**的上游文案，唯一的区分位
                    //      就是 `has_ever_succeeded`；顺序反了就会给一个区本来是对的健康号改区。
                    //
                    // ⚠️ 绝不 `report_failure` / 不冷却：region 配错≠号坏（隔离铁律，与上面
                    // FEATURE_NOT_SUPPORTED 那条同款）。惩罚它只会让一个其实好的号被推向禁用。
                    //
                    // `last_outcome` 保持上面已置的 `AuthFailed` 不动：403 bearer-invalid 在
                    // 客户端视角确实是授权层拒绝，改成 ServerError 会把它伪装成上游故障。
                    if status.as_u16() == 403
                        && endpoint.is_bearer_token_invalid(&body)
                        && !region_switched_this_call.contains(&ctx.id)
                        && call_started.elapsed()
                            < Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS)
                    {
                        trace_guard.verdict("region_mismatch_403");
                        // 用 `call_creds` 而非 `ctx.credentials`：前者才是**本次请求真正打出去**
                        // 的那个区（含本链内已生效的覆盖），据它算「另一个区」才不会算错。
                        let current = call_creds.effective_upstream_region(&config).to_string();
                        if let Some(target) = region_retry_target(
                            &current,
                            call_creds.is_api_key_credential(),
                            self.token_manager.has_ever_succeeded(ctx.id),
                        ) {
                            // 每号一次上限（见 `region_switched_this_call` 声明处）。
                            region_switched_this_call.insert(ctx.id);
                            region_override_this_call.insert(ctx.id, target.to_string());
                            // ⚠️ 必须把它从「本请求已试过」里摘掉：否则下一跳
                            // `acquire_context_excluding` 会**结构性避开它**，于是换区重试打的
                            // 是别人的号 —— 覆盖值躺在 map 里没人用，等于没换区。摘掉只是让它
                            // 恢复**可被选中**（仍要过冷却/RPM 等既有硬门），不是强行指定。
                            // 若调度这一跳选了别的号并成功，本次覆盖不回写（L2 按 id 取），
                            // 自纠正顺延到下一条客户端请求 —— 迟一点，但绝不会写错。
                            tried_this_call.remove(&ctx.id);
                            tracing::warn!(
                                "凭据 #{} 从未成功过且吃 bearer-invalid 403 ⇒ 判 region 错配：\
                                 {} → {}，同号换区重试一次（不计失败、不冷却）",
                                ctx.id,
                                current,
                                target
                            );
                            last_error = Some(anyhow::anyhow!(
                                "{} API 请求失败（疑似 region 错配，已换区 {} → {} 重试）: {} {}",
                                api_type,
                                current,
                                target,
                                status,
                                body
                            ));
                            retry_same = true;
                            break 'classify;
                        }
                    }

                    // 同一个号在一条请求里只惩罚一次：report_failure 累计 3 次即禁用，而循环里
                    // 没有排除集时同号可被连选连打，一条请求就能把它推到 TooManyFailures，
                    // 进而触发全池禁用 → 自愈活锁。custom_api 路径早有 excluded 集，这里补齐。
                    let has_available = if auth_failed_this_call.insert(ctx.id) {
                        self.token_manager.report_failure(ctx.id)
                    } else {
                        tracing::warn!(
                            "凭据 #{} 本次请求已计过一次认证失败，不重复惩罚（防单请求推至 TooManyFailures）",
                            ctx.id
                        );
                        true
                    };
                    if !has_available {
                        last_error = Some(anyhow::anyhow!(
                            "{} API 请求失败（所有凭据已用尽）: {} {}",
                            api_type,
                            status,
                            body
                        ));
                        break 'attempt;
                    }

                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    // 换号前退避：此前是裸 continue，401/403 风暴下会以零间隔连打多个号，
                    // 与 suspend 分支同一类自我放大。
                    tokio::time::sleep(Self::retry_delay(attempt)).await;
                    continue 'attempt;
                }

                // 503 MODEL_TEMPORARILY_UNAVAILABLE — 模型容量问题，非凭据问题。
                // 使用慢速退避（1s base）；不调用 report_failure / report_rate_limited，
                // 不影响凭据健康分（健康分反映凭据质量，与模型过载无关）。
                // 只允许 MAX_MODEL_UNAVAILABLE_RETRIES 次慢速重试，耗尽后直接 break 透传错误——
                // 继续切换凭据无意义（所有凭据对同一过载模型等价）。
                // ⚠️ 状态门必须同时收 **503 与 400**：上游对「模型没容量」这同一件事发过两种形态 ——
                // 503 `MODEL_TEMPORARILY_UNAVAILABLE`，以及 400 `ThrottlingException` +
                // `reason:INSUFFICIENT_MODEL_CAPACITY`（实测 24h 272 次）。
                //
                // 原先写死 `== 503`，于是那 272 次逐条落空所有分支、走到函数末尾兜底 ⇒
                // 客户端拿到 **502 Bad Gateway 且无 Retry-After** ⇒ 按永久性服务端故障处理 ⇒
                // 不退避、原样重发。这与 `temporarily is suspended` 修复前是同一个缺陷形态。
                //
                // 400 通常是「请求本身有问题，重试无意义」，所以这里**不放宽整个 400**，
                // 只放宽带该 reason 字面量的那一种 —— 判据在
                // `default_is_model_temporarily_unavailable` 内，两个状态共用同一套处置。
                if (status.as_u16() == 503 || status.as_u16() == 400)
                    && endpoint.is_model_temporarily_unavailable(&body)
                {
                    // 400 形态（INSUFFICIENT_MODEL_CAPACITY）与 503 形态（容量不足）分开标：
                    // 两者处置相同但成因不同，trace 需要区分。
                    if status.as_u16() == 400 {
                        trace_guard.verdict("capacity_400");
                    } else {
                        trace_guard.verdict("model_unavailable");
                    }
                    model_unavailable_attempts += 1;
                    tracing::warn!(
                        "模型暂时不可用（MODEL_TEMPORARILY_UNAVAILABLE，第 {}/{} 次）: {} {}",
                        model_unavailable_attempts,
                        MAX_MODEL_UNAVAILABLE_RETRIES + 1,
                        status,
                        body
                    );
                    last_outcome = crate::usage::RequestOutcome::ModelUnavailable;
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败（模型暂时不可用，建议稍后重试）: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    if model_unavailable_attempts > MAX_MODEL_UNAVAILABLE_RETRIES {
                        // 已用完慢速重试预算，透传过载错误给客户端，让其自行退避。
                        break 'attempt;
                    }
                    // 慢速退避：1s base，比通用 200ms 更长，避免反复冲击过载路径。
                    sleep(Self::retry_delay_model_unavailable(
                        model_unavailable_attempts - 1,
                    ))
                    .await;
                    continue 'attempt;
                }

                // 429/408/5xx - 瞬态上游错误：重试但不禁用或切换凭据
                // （避免 429 high traffic / 502 high load 等瞬态错误把所有凭据锁死）
                if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                    if status.as_u16() == 429 {
                        trace_guard.verdict("rate_limited");
                    } else {
                        trace_guard.verdict("server_error");
                    }
                    tracing::warn!(
                        "API 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );
                    // 429 限流：优先换端点桶（另一 host = 上游另一限流桶），同号换完所有端点
                    // 才走凭据级冷却换号。（仍不禁用、不计永久失败，冷却到期自动恢复）
                    // ⭐ S2：本次 429 的显式上游 RA，供错误串 marker 透传给客户端
                    // （凭据冷却在下方共用同一值；配额类 429 被上方 monthly-limit 分支
                    // 先接走，不会到这里 —— marker 恒为速率类）。
                    let mut upstream_retry_after: Option<u64> = None;
                    if status.as_u16() == 429 {
                        last_outcome = crate::usage::RequestOutcome::RateLimited;
                        // 上游 429 → 入站整形 RPM 自动挡乘性降档(削平后续入站速率,别继续挤爆上游)。
                        // 只在第 0 轮上报(见本文件 'absorb 循环处的 AIMD 放大说明)。
                        if absorb_round == 0 {
                            self.token_manager.report_upstream_rate_limited();
                        }
                        // 优先用上游给出的精确重置时间：响应头 Retry-After 优先，其次错误 body
                        let retry_after =
                            retry_after_header.or_else(|| endpoint.extract_retry_after_secs(&body));
                        // S2/S3：记下本次 429 的显式 RA（客户端透传 marker）+ 重试链内
                        // 429 RA 的合并（m7：`.max()` 保留最大 RA，见声明处说明与
                        // [`merge_upstream_429_retry_after`]）。
                        upstream_retry_after = retry_after;
                        first_upstream_429_retry_after =
                            merge_upstream_429_retry_after(first_upstream_429_retry_after, retry_after);

                        // 🔀 端点桶换桶：**仅当该凭据有回退端点**（端点顺序 > 1，如 ksk_ 的
                        // `cli`/`cli-runtime` 两个独立限流桶）才封禁当前 host 桶 30s 并尝试换下一
                        // 端点；单端点凭据（OAuth 号）**不封桶**、直接走原凭据级冷却换号——
                        // 桶 30s > 凭据冷却 15s 的窗口会让 select_endpoint 返回 None，若该分支落
                        // report_failure 会把瞬态封禁累成永久禁用（见 select_endpoint 的 None 注释）。
                        // 端点自适应派发：429 记该端点一次失败。放在 `order.len() > 1`
                        // 守卫**之外**是刻意的 —— 单端点凭据也该积累统计（将来它被加上
                        // 回退端点时立刻有数据可用），而封桶才必须受那个守卫约束
                        // （桶 30s > 凭据冷却 15s 会把瞬态封禁累成永久禁用）。
                        self.report_endpoint_outcome(ctx.id, endpoint.name(), false);

                        let order = call_creds.effective_endpoint_order(&self.default_endpoint);
                        if order.len() > 1 {
                            // 桶键同 select 侧口径（见 `endpoint_buckets` 字段注释）。
                            self.endpoint_buckets.lock().insert(
                                (ctx.id, endpoint.bucket_id(&rctx)),
                                Instant::now() + ENDPOINT_BUCKET_THROTTLE,
                            );
                            if self.has_unthrottled_endpoint(&call_creds, ctx.id) {
                                // ⭐ 照抄 bearer-invalid 403 换区先例（见上文排除集摘除
                                // 的注释）：摘掉"本请求已试过"标记，让 acquire_context_excluding 下轮
                                // 可重新选中本号；同时**不设凭据级冷却**（也不占 rate_limited_this_call，
                                // 否则"全部端点都封"时去重逻辑误判已冷却过、永远不设冷却）。
                                // 仅摘排除集不够：选号会优先更空闲的陪跑号，换桶/换区 hop 被偷走。
                                // 下一跳必须沿用本 CallContext（reuse_ctx），不重新选号。
                                tried_this_call.remove(&ctx.id);
                                retry_same = true;
                                tracing::warn!(
                                    "凭据 #{} 端点 {} 429 ⇒ 封桶 {}s，换下一端点继续（本请求链内）",
                                    ctx.id,
                                    endpoint.name(),
                                    ENDPOINT_BUCKET_THROTTLE.as_secs()
                                );
                            } else if rate_limited_this_call.insert(ctx.id) {
                                // 所有端点桶都已封禁：按原有逻辑设凭据级冷却，让调度换号。
                                self.token_manager
                                    .report_rate_limited_with_retry_after(ctx.id, retry_after);
                            } else {
                                tracing::debug!(
                                    "凭据 #{} 本请求链内已冷却过，再次 429 仅换号 failover，不重复惩罚",
                                    ctx.id
                                );
                            }
                        } else if rate_limited_this_call.insert(ctx.id) {
                            // 单端点凭据：与改动前逐字节一致（短冷却换号，不涉及桶）。
                            self.token_manager
                                .report_rate_limited_with_retry_after(ctx.id, retry_after);
                        } else {
                            tracing::debug!(
                                "凭据 #{} 本请求链内已冷却过，再次 429 仅换号 failover，不重复惩罚",
                                ctx.id
                            );
                        }
                    } else {
                        last_outcome = crate::usage::RequestOutcome::ServerError;
                        // 5xx 也给该号设短冷却（30s，自动恢复）。此前只 sleep 就换号、不设冷却，
                        // 失败的号下一轮立刻可再被选中，于是 500 风暴时请求在同一批坏号之间
                        // 来回打（实测一小时 408 次 500），把重试预算烧光却没换到好号。
                        // 本请求链内同号只设一次，复用 429 的去重集语义，避免重复累加。
                        if status.is_server_error() && rate_limited_this_call.insert(ctx.id) {
                            self.token_manager.report_server_error(ctx.id);
                            // 5xx 风暴同样是上游压力信号 → 入站 AIMD 降档。
                            // 只在第 0 轮上报(见本文件 'absorb 循环处的 AIMD 放大说明)。
                            if absorb_round == 0 {
                                self.token_manager.report_upstream_pressure();
                            }
                        }
                    }
                    // ⭐ S2：429 且上游给了显式 Retry-After → 把网关自己的 marker
                    // （`upstream_retry_after=N`）打进错误串，由 map_provider_error 的
                    // A7 分支决议成客户端 Retry-After 头（优先级：上游真值 > 配置 > 8s）。
                    // 与 `retry_after_secs=`（号池冷却真值，A5 全池语义）刻意不同名——
                    // 单凭据上游 429 复用它会落 A5 的「所有凭据冷却」文案，语义错位。
                    // 配额类 429 不会到这里（上方 monthly-limit 分支不门控状态码先接走）。
                    last_error = Some(match upstream_retry_after {
                        Some(secs) => anyhow::anyhow!(
                            "{} API 请求失败: {} {} {}{}",
                            api_type,
                            status,
                            body,
                            crate::anthropic::handlers::UPSTREAM_RETRY_AFTER_MARKER_PREFIX,
                            secs
                        ),
                        None => {
                            anyhow::anyhow!("{} API 请求失败: {} {}", api_type, status, body)
                        }
                    });
                    if attempt + 1 < max_retries {
                        // 429 用专用长退避（1s→2s→4s→8s）：被限流时短重试只会连打同一上游；
                        // 5xx/408 仍走通用 200ms 指数（基础设施瞬态，快速重试合理）。
                        if status.as_u16() == 429 {
                            sleep(Self::retry_delay_throttle(attempt)).await;
                        } else {
                            sleep(Self::retry_delay(attempt)).await;
                        }
                    }
                    if retry_same {
                        break 'classify;
                    }
                    continue 'attempt;
                }

                // 其他 4xx - 通常为请求/配置问题：直接返回，不计入凭据失败
                if status.is_client_error() {
                    trace_guard.verdict("other_4xx");
                    last_outcome = crate::usage::RequestOutcome::BadRequest;
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    break 'attempt;
                }

                // 兜底：当作可重试的瞬态错误处理（不切换凭据）
                tracing::warn!(
                    "API 请求失败（未知错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                last_outcome = crate::usage::RequestOutcome::OtherError;
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                if attempt + 1 < max_retries {
                    sleep(Self::retry_delay(attempt)).await;
                }
                } // 'classify
                if retry_same {
                    reuse_ctx = Some(ctx);
                }
            }

            // ── 本轮 failover 链已耗尽,决定是否再吸收一轮 ────────────────────────────
            // 下一轮的尝试计数从本轮末尾续上(+1 = 本轮最后那次尝试本身)。
            attempts_base = attempts_used + 1;

            // 关闭时 effective_max_rounds() 恒为 0 ⇒ 这里必定 break，
            // 下面的分类/退避/sleep/计数器一概不执行 ⇒ 逐字节等价旧行为。
            if absorb_round >= absorb.effective_max_rounds() {
                // 轮次用尽也是「吸收层跑过并放弃」的一种（且开着时是最常见的一种）。
                // `absorb_round > 0` 这道限定是承重的：关闭吸收层时这里恒是 0 ⇒ 不置位 ⇒
                // 渲染路径逐字节不变。
                absorb_gave_up_after_rounds |= absorb_round > 0;
                break 'absorb;
            }
            // ⭐ 未修问题 ②：跨轮总额度已用尽 ⇒ 下一轮配额为 0。**必须在这里 break**,不能
            // 靠「进了轮再发现 for 循环跑 0 次」：那样会先睡满一次退避、且 attempts_base 又 +1,
            // 变成每轮白睡一次退避直到 max_rounds 用完 —— 客户端多等好几个退避却零次上游调用。
            //
            // ⚠️ 判据喂 `budget.used()`（跨层共享的已用量）而非 `attempts_base`（迭代计数）：
            // 后者含 fast-fail 空转,会在全池冷却时把额度在毫秒内烧空 ⇒ 本闸门抢在下面的截断
            // 闸门之前恒命中 ⇒ 吸收层对它最该拦的那一类（PoolCooldown）从来没起过作用。
            if round_retry_quota(base_retry_quota, budget.used()) == 0 {
                // ⚠️ 三个 break 'absorb 的 warn 文案必须**互相可分辨**,且各自点名该调哪个旋钮:
                // 本条与下面两条此前都只是散文,而下面两条还共用同一个计数器 ⇒ 面板/日志都区分不出
                // 「额度用尽」「上游恢复期太长」「预算不够睡」三种完全不同的结局,运维会去抬错的旋钮。
                // 这里用 `absorb_stop` 这个结构化字段做机器可读判据(不依赖中文文案不变)。
                // ⭐ 这道闸门此前**不 bump 任何计数器** ⇒ 这类请求既不进吸收比的分子也不进
                // 分母 ⇒ 面板上的吸收比偏乐观（分母里少了被额度掐掉的那批）。而它与另两条
                // 放弃结局的区别是承重的：这是**每请求硬上限**，抬任何 upstreamRetryAbsorb*
                // 旋钮都不会改变结局 —— 归到 budget_exhausted 会把运维引向抬预算（无效）。
                crate::common::recovery_metrics::bump_absorb_retry_quota_exhausted();
                // 告警：跨轮总重试额度耗尽（每请求硬上限，抬任何吸收旋钮都不改变结局的强信号）。
                crate::common::alerting::bump("absorb_retry_quota_exhausted");
                tracing::warn!(
                    absorb_stop = "retry_quota_exhausted",
                    rounds = absorb_round,
                    upstream_calls,
                    attempts = attempts_base,
                    budget_used = budget.used(),
                    "吸收层已用尽跨轮总重试额度（{} 次真实上游调用），停止吸收并透传上游错误。\
                     这是**每请求**硬上限,与 upstreamRetryAbsorb* 各旋钮无关,抬那些配置不会改变本结局",
                    ABSOLUTE_MAX_TOTAL_RETRIES
                );
                absorb_gave_up_after_rounds |= absorb_round > 0;
                break 'absorb;
            }
            let Some(err) = last_error.as_ref() else {
                break 'absorb;
            };
            let Some(class) = crate::anthropic::absorb_class_of(&err.to_string()) else {
                break 'absorb;
            };
            // ⭐ 各类别的独立开关。判据收在 `class_allowed` 一处（散写必然漏一处，而漏掉那处
            // 的表现是「默认关的类别其实在吸收」—— 硬约束里最不能出的错）。
            //
            // 每类各有可分辨的 skip 计数器：上线后「这一类到底出现过几次、开了会救回多少」
            // 只能靠这组数回答。共用一个桶的话，开三个开关后面板上仍是一个数 ⇒ 无法归因，
            // 也就无法决定该关掉哪个（外挂那 11.6:1 的重试比正是不分类别一律重试的账单）。
            if !absorb.class_allowed(class) {
                use crate::model::AbsorbClass;
                match class {
                    AbsorbClass::SwapWindow => {
                        crate::common::recovery_metrics::bump_absorb_suspend_skipped()
                    }
                    AbsorbClass::TransientServerError => {
                        crate::common::recovery_metrics::bump_absorb_server_error_skipped()
                    }
                    AbsorbClass::TransientCapacity400 => {
                        crate::common::recovery_metrics::bump_absorb_capacity_400_skipped()
                    }
                    // 这两类跟着总开关走，`class_allowed` 对它们恒 true ⇒ 不可达。
                    AbsorbClass::PoolCooldown(_) | AbsorbClass::UpstreamRateLimit => {}
                }
                tracing::debug!(
                    absorb_stop = "class_absorb_disabled",
                    ?class,
                    rounds = absorb_round,
                    "该类别的吸收开关未开启，按现状透传上游错误"
                );
                break 'absorb;
            }
            // ⭐ 未修问题 ③：号池真实恢复时刻超过我们愿意睡的上限 ⇒ 睡醒了池子还在冷却,
            // 这一轮**结构上必然**拿回同一个错误。典型:全池自愈退避 60s
            // (config.self_heal_base_backoff_secs（默认 60s）, token_manager.rs:890 一带) vs max_delay 默认 15s。
            // 此前只 clamp 不判断 ⇒ 睡 15s → 白打一轮 → 客户端多等 15s 拿同一个 429。
            // 必须**在** should_start_another_round 之前判:那条只看预算够不够,
            // 看不出「睡够了但上游没好」—— 两者是独立的失败模式。
            if absorb.backoff_is_truncated(class, absorb_round) {
                // ⭐ 已拆出独立计数器（原先与下面「预算不足一轮」共用
                // `bump_absorb_budget_exhausted()`）：两者该调的旋钮**相反** —— 本条要抬
                // `upstreamRetryAbsorbMaxDelaySecs`（我们愿意睡的上限 < 号池给出的真实恢复
                // 时刻），下面那条要抬 `upstreamRetryAbsorbBudgetSecs`（总预算装不下一轮）。
                // 共用一个桶时面板上看到「吸收比低」无从判断该动哪个，而实测运维会去抬
                // budget，真正的瓶颈是 maxDelay。结构化 `absorb_stop` 仍保留（日志侧判据）。
                crate::common::recovery_metrics::bump_absorb_backoff_truncated();
                tracing::warn!(
                    absorb_stop = "backoff_truncated",
                    rounds = absorb_round,
                    ?class,
                    required_wait_secs = absorb.required_wait(class, absorb_round).as_secs(),
                    max_delay_secs = absorb.class_max_delay(class).as_secs(),
                    "号池真实恢复时间超过退避上限，再吸收一轮必然拿回同一错误，直接透传。\
                     要吸收这一类需抬 upstreamRetryAbsorbMaxDelaySecs（**不是** budgetSecs）"
                );
                absorb_gave_up_after_rounds |= absorb_round > 0;
                break 'absorb;
            }
            let delay = absorb.backoff(class, absorb_round);
            // 本类别的 deadline：换号空窗设了独立预算时用它自己那份（空窗实测 10 分钟 ≫ 总预算
            // 20~45s，共用一个预算装不下）。其余类别恒等于总预算那个 ⇒ 旧行为不变。
            let class_deadline = absorb.class_deadline(call_started, class);
            // 判据是「剩余 > 退避 + 一轮最坏耗时」,不是「剩余 >= 退避」:后者会让这一轮在半路
            // 被 deadline 砍断,白打一轮上游还让客户端多等(设计评审 BLOCKER 9)。
            if !should_start_another_round(class_deadline, std::time::Instant::now(), delay) {
                // 与上一条截断闸门已拆成两个计数器(见那里的长注释),靠 `absorb_stop` 也能区分:
                // 本条的瓶颈是**总预算**,该抬 `upstreamRetryAbsorbBudgetSecs`
                // (换号空窗类则是 upstreamRetryAbsorbSwapBudgetSecs)。
                crate::common::recovery_metrics::bump_absorb_budget_exhausted();
                // 告警：吸收总预算不足一轮（429 风暴下的典型结局）。
                crate::common::alerting::bump("absorb_budget_exhausted");
                tracing::warn!(
                    absorb_stop = "budget_too_small_for_round",
                    rounds = absorb_round,
                    ?class,
                    delay_secs = delay.as_secs(),
                    "吸收层预算不足一轮，原样透传上游 429 + Retry-After 让客户端退避。\
                     要吸收这一类需抬 upstreamRetryAbsorbBudgetSecs（**不是** maxDelaySecs）"
                );
                absorb_gave_up_after_rounds |= absorb_round > 0;
                break 'absorb;
            }
            sleep(delay).await;
            // 下一轮的墙钟按**触发本次重试的类别**记账。换号空窗那份更宽的预算只在它自己
            // 触发的轮次生效,不会泄漏给下一轮的其它类别(下一轮若是别的类会被改回来)。
            round_deadline = class_deadline;
            absorb_round += 1;
            crate::common::recovery_metrics::bump_absorb_round();
            // 每类各一个 round 计数器:哪一类在真起作用只能靠这组数回答(见 recovery_metrics 说明)。
            {
                use crate::model::AbsorbClass;
                match class {
                    AbsorbClass::PoolCooldown(_) => {
                        crate::common::recovery_metrics::bump_absorb_round_pool_cooldown();
                        // 告警：全池冷却吸收轮（429 风暴信号，冷却窗口内去重）。
                        crate::common::alerting::bump("absorb_pool_cooldown");
                    }
                    AbsorbClass::UpstreamRateLimit => {
                        crate::common::recovery_metrics::bump_absorb_round_rate_limit()
                    }
                    AbsorbClass::SwapWindow => {
                        crate::common::recovery_metrics::bump_absorb_round_swap_window()
                    }
                    AbsorbClass::TransientServerError => {
                        crate::common::recovery_metrics::bump_absorb_round_server_error()
                    }
                    AbsorbClass::TransientCapacity400 => {
                        crate::common::recovery_metrics::bump_absorb_round_capacity_400()
                    }
                }
            }
            // ⚠️ 刻意**不重置** last_error:若下一轮没产生新错误(如全池冷却 fast-fail 后 last_error
            // 未被覆盖),重置会让 final_error 落到「已达到最大重试次数」通用串 →
            // map_provider_error 认不出来 → 兜底 502 且无 Retry-After → 客户端从此不退避。
        }

        // 整条客户端请求失败收尾：failover 耗尽只在**吸收循环真正结束**且确有换号 failover 时
        // 记一次（已知问题 #13）。此前放在轮内且每轮清零 ⇒ 一条请求跑 N 轮就计 N 次（多计）；
        // 且成功路径在循环内 return，这里根本走不到 ⇒ 已恢复的请求不再误计为耗尽。
        // 仅当真的换号 failover 过（打了 >1 个号）才计——首个号即因客户端错误/模型无效 break
        // 的不算池耗尽（该区分语义不变，见 `real_failover_happened` 声明处）。
        if real_failover_happened {
            crate::common::recovery_metrics::bump_failover_exhausted();
            // 告警：全池 failover 号全灭（整条请求失败）。
            crate::common::alerting::bump("failover_exhausted");
        }

        // 所有吸收轮与重试都失败:埋点一条失败记录后返回错误。
        // ⚠️ 失败记录与下面的备用模型兜底都必须留在 'absorb **之外**:
        // 放进轮内会让一条客户端请求落 N 条失败记录,面板失败数被吸收轮次乘倍。

        // 备用模型兜底：MODEL_TEMPORARILY_UNAVAILABLE 耗尽重试预算后，
        // 若配置了备用模型，以备用模型做最后一次尝试（限 1 次，不再套完整 failover 循环）。
        // 典型用途：opus 系列过载时切到容量独立的 sonnet（前提：用户已知晓响应质量/计费差异）。
        if last_outcome == crate::usage::RequestOutcome::ModelUnavailable {
            // ⭐ 共享预算（2026-08-11 方案 A，对抗审查 M2）：fallback 是一次真实上游调用，
            // 必须扣预算；预算已耗尽（used=4）时跳过兜底直接透传最后错误——否则
            // 「4+1=5」击穿「每请求 ≤4」的承诺。
            if budget.used() >= ABSOLUTE_MAX_TOTAL_RETRIES as u32 {
                tracing::warn!(
                    "MODEL_TEMPORARILY_UNAVAILABLE 重试耗尽，但每请求共享预算已用尽，\
                     跳过备用模型兜底（overload_fallback_model）"
                );
            } else {
            let cfg = self.token_manager.config();
            if let Some(ref fallback_model_id) = cfg.overload_fallback_model.clone() {
                tracing::warn!(
                    "MODEL_TEMPORARILY_UNAVAILABLE 重试耗尽，尝试 overload_fallback_model: {}",
                    fallback_model_id
                );
                let fallback_body = Self::rewrite_model_id(request_body, fallback_model_id);
                if let Ok(ctx) = self
                    .token_manager
                    .acquire_context(Some(fallback_model_id), session_id.as_deref())
                    .await
                {
                    let config = self.token_manager.config();
                    let machine_id =
                        machine_id::generate_from_credentials(&ctx.credentials, &config);
                    // overload fallback：降级模型重试走单端点（首选），不参与换桶——罕见路径。
                    if let Ok(endpoint) = self.endpoint_for(&ctx.credentials) {
                        let rctx = RequestContext {
                            credentials: &ctx.credentials,
                            token: &ctx.token,
                            machine_id: &machine_id,
                            config: &config,
                            is_1m,
                        };
                        let url = endpoint.api_url(&rctx);
                        let body = endpoint.transform_api_body(&fallback_body, &rctx);
                        let base = self
                            .client_for(&ctx.credentials)?
                            .post(&url)
                            .body(body)
                            .header("content-type", endpoint.content_type());
                        let request = endpoint.decorate_api(base, &rctx);
                        let send_result = request.send().await;
                        // 共享预算扣减（2026-08-11 方案 A，对抗审查 M2）：fallback 是真实
                        // 上游调用，成败都扣。
                        budget.consume(1);
                        match send_result {
                            Ok(resp) if resp.status().is_success() => {
                                self.token_manager.report_success(ctx.id);
                                let meta = CallMeta {
                                    credential_id: ctx.id,
                                    // 契约：model 恒为客户端原始名（requested_model 口径）。
                                    model: client_model_owned.clone().or_else(|| model.clone()),
                                    // overload_fallback 显式跳过**全局映射表**：fallback 名是
                                    // 运维拍板的目标，再套全局映射会依赖 HashMap 迭代顺序产生
                                    // 不确定行为（A→B 且 B→C 时 fallback=B 是否再被改写无从判定）。
                                    // 它就是"实际发给上游的名"，直接进 mapped_model
                                    // （upstream_model 口径；不再回落 model 造成失真）。
                                    mapped_model: Some(fallback_model_id.clone()),
                                    session_id: session_id.clone(),
                                    is_streaming: is_stream,
                                    retries: (model_unavailable_attempts + 1) as u32,
                                    latency_ms: call_started.elapsed().as_millis() as u64,
                                    started_at: call_started,
                                    inflight: ctx.inflight,
                                };
                                return Ok((resp, meta));
                            }
                            Ok(resp) => {
                                // 🔴 F2（对抗审查 2026-08-15）：fallback 尝试**发出后无论成败**，
                                // mapped_model 都要更新为 fallback 名（与成功路径 :4366 同键空间）——
                                // 否则失败样本 fail_record.upstream_model 归到主循环名/原始名，
                                // by_model 聚合失真。fallback 场景恰是上游过载时最可能失败的时候。
                                mapped_model = Some(fallback_model_id.clone());
                                tracing::warn!(
                                    "overload_fallback_model {} 也失败: {}",
                                    fallback_model_id,
                                    resp.status()
                                );
                            }
                            Err(e) => {
                                mapped_model = Some(fallback_model_id.clone());
                                tracing::warn!(
                                    "overload_fallback_model {} 请求错误: {}",
                                    fallback_model_id,
                                    e
                                );
                            }
                        }
                    }
                }
            }
        }
        }

        let final_error = self.with_sealed_bucket_retry_after(
            last_error.unwrap_or_else(|| {
                if budget.remaining() == 0 {
                    // 每客户端请求的共享上游预算已耗尽（2026-08-11 方案 A）：可能发生在
                    // websearch 回灌靠后轮次或压缩重试轮——「每请求 ≤4 次上游」的承诺达成后
                    // 不再空打，错误上抛给客户端自己退避。
                    anyhow::anyhow!(
                        "{} API 请求失败：每客户端请求的上游调用预算已耗尽（shared_budget_exhausted=1）",
                        api_type
                    )
                } else {
                    anyhow::anyhow!(
                        "{} API 请求失败：已达到最大重试次数（{}次）",
                        api_type,
                        base_retry_quota
                    )
                }
            }),
            last_outcome,
        );
        // ⭐ 吸收层真的重试过却仍失败,且部署侧要求这类终态回 503:给错误串打机器可读标记,
        // 由 `map_provider_error` 的第一条分支换状态码。
        //
        // 为什么标记必须在**这里**打而不是让 handlers 自己判：handlers 拿到的只有一个错误串,
        // 分不出「吸收层跑过并放弃」与「吸收层根本没开、429 原样透传」。后者改成 503 是错的
        // （网关一次都没重试,却告诉客户端「我们这边暂时不可用」）。这个二分只有 provider 做得出来,
        // 与 `bearer_invalid_transient=1`（`has_ever_succeeded` 那个二分）同款范式。
        //
        // 两个条件都不成立时（默认配置即如此）本段不执行 ⇒ 错误串与渲染路径逐字节不变。
        let final_error = if absorb_gave_up_after_rounds && absorb.exhausted_as_503 {
            // 走 `handlers::` 全路径而不在 `anthropic/mod.rs` 加 re-export：那个文件不在本次
            // 改动范围内，而 `handlers` 本身就是 `pub(crate) mod` ⇒ 直接可达，少改一处即少一个
            // 要同步的真值面。
            let marker = crate::anthropic::handlers::ABSORB_BUDGET_EXHAUSTED_MARKER;
            // ⚠️ 用 `context` 而非重建错误：保留原始错误链（面板/日志里那句上游原文是排障的
            // 唯一线索），同时 `to_string()` 里出现标记 —— anyhow 的 Display 只打最外层,
            // 故标记必须与原文拼在同一层里。
            anyhow::anyhow!("{} {}", final_error, marker)
        } else {
            final_error
        };
        // ⭐ S3：最早类型化 429 保留 —— 把重试链内首个上游 429 的显式 RA 并入终态
        // （若终态是 generic 瞬态失败且未被既有标记分支覆盖）。限定集见
        // `assemble_final_error`：吸收耗尽 503 不转换、永久态/配额/背压分支不转换。
        let final_error =
            assemble_final_error(final_error, first_upstream_429_retry_after, last_outcome);
        let mut fail_record = crate::usage::RequestRecord::new(
            uuid::Uuid::new_v4().to_string(),
            client_model_owned.clone().or(model.clone()).unwrap_or_default(),
        );
        fail_record.credential_id = last_credential_id;
        // ⭐ 失败记录必须带「链内首选号」（N4）：透传 failover 首跳已由共享预算记录
        // （handlers 先试透传再落本路径，预算里是整条链真正最先尝试的号；本路径首个
        // 选中的号兜底）。此前失败样本 credential_id=None 且无首选号信息，面板看不到
        // 「死号恒选」—— 首选号恒为某号却全链失败时，说明该号每次都被选中最前却被换掉。
        fail_record.first_attempted_credential_id = budget.first_attempted();
        fail_record.session_id = session_id.clone();
        fail_record.is_streaming = is_stream;
        fail_record.latency_ms = call_started.elapsed().as_millis() as u64;
        fail_record.outcome = last_outcome;
        // ⭐ 失败记录同样带双口径：`requested_model` = 客户端原始名（= client_model，
        // 未提供时回落请求体解析名），`upstream_model` = 循环内最后成功映射的名
        // （选号后、改写成功才可能非 None；全池冷却/准入超时等根本没进循环的失败
        // 路径为 None，聚合层回落 model）。
        // 缺失会让「按 upstream_model 聚合」时失败样本凭空消失 → 成功率偏乐观（#21 教训）。
        fail_record.requested_model = client_model_owned.clone().or(model.clone());
        fail_record.upstream_model = mapped_model.clone();
        // ⭐ 失败记录必须带真实换号次数。此前这里没有设 `retries` → 恒为默认 0，
        // 使「烧掉 12 次换号才失败」与「第一次就失败」在面板上不可区分。
        // 与成功分支 `retries: attempt as u32`（本文件下方）同口径。
        fail_record.retries = attempts_used;
        fail_record.error_message = Some(final_error.to_string());
        crate::usage::emit_record(fail_record);

        Err(final_error)
    }

    /// 从原始 `metadata.user_id` 提取会话 UUID（S6 P1-1 透传 session 归一）。
    ///
    /// 语义镜像 `anthropic::converter::extract_session_id`（converter.rs:857）——
    /// 透传路径的埋点 session 必须与 Kiro 路径**同源**：Kiro 的 conversationId（L1）
    /// 由同一函数从 user_id 提取，两条路径共用同一个 key，同一会话跨 Kiro/透传
    /// 不再拆成两个 by_session key；同时只把 UUID 落 trace，`account_uuid` /
    /// `user_xxx_account__` 前缀等明文不再进 trace（脱敏，S6 P1-4）。
    ///
    /// 提取不到（无 session / 非法形状）→ `None`（不再回落原始 user_id 串）。
    ///
    /// ⚠️ converter 的版本是私有函数（本次改动范围不含 converter.rs），这里按同一
    /// 语义复制一份。若将来 converter 侧改动提取逻辑，必须同步本函数（或把 converter
    /// 的函数提升为 `pub` 后删掉本副本——两份拷贝必然漂移是本仓已记的教训）。
    fn extract_session_uuid(user_id: &str) -> Option<String> {
        // JSON 格式: {"device_id":"...","account_uuid":"...","session_id":"UUID"}
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(user_id) {
            if let Some(session_id) = json.get("session_id").and_then(|v| v.as_str()) {
                if Self::is_valid_uuid_shape(session_id) {
                    return Some(session_id.to_string());
                }
            }
        }
        // 字符串格式: user_xxx_account__session_0b4445e1-...
        if let Some(pos) = user_id.find("session_") {
            // 安全：用 get(..36) 而非定长字节切片。客户端可控串可能在第 36 字节落在
            // 多字节 UTF-8 字符中间，定长切片会 panic（converter 同款防御）。
            if let Some(uuid_str) = user_id[pos + 8..].get(..36) {
                if Self::is_valid_uuid_shape(uuid_str) {
                    return Some(uuid_str.to_string());
                }
            }
        }
        None
    }

    /// 简单校验 UUID 形状（36 字符 + 4 个连字符；镜像 converter::is_valid_uuid）。
    /// 只做形状校验（与 converter 一致），不做 hex 校验——客户端真实 UUID 全 hex、
    /// L2 派生键是合法 UUID 形状，均不受影响。
    fn is_valid_uuid_shape(s: &str) -> bool {
        s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4
    }

    /// 从请求体中一次性提取模型信息与会话标识（conversationId）。
    ///
    /// 热路径优化（P0-A）：原先 `extract_model_from_request` 与
    /// `extract_session_id_from_request` 各自对整个请求体做一次全量
    /// `serde_json::from_str`，一次调用要解析两遍。合并成解析一次 `Value`、
    /// 再取两个字段，行为完全等价但只付出一次解析开销。
    ///
    /// - model：`conversationState.currentMessage.userInputMessage.modelId`
    /// - session：`conversationState.conversationId`（由 converter 从原始
    ///   metadata.user_id 的 session UUID 派生；无真实 session 时为随机 UUID，
    ///   每次不同，自然不命中亲和性，等价于常规轮换）。
    ///
    /// 请求体解析失败（非法 JSON）时两者都返回 None，与旧实现一致。
    fn extract_model_and_session(request_body: &str) -> (Option<String>, Option<String>) {
        use serde_json::Value;

        let json: Value = match serde_json::from_str(request_body) {
            Ok(v) => v,
            Err(_) => return (None, None),
        };

        let conversation_state = json.get("conversationState");

        let model = conversation_state
            .and_then(|cs| cs.get("currentMessage"))
            .and_then(|m| m.get("userInputMessage"))
            .and_then(|u| u.get("modelId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let session_id = conversation_state
            .and_then(|cs| cs.get("conversationId"))
            .and_then(|v| v.as_str())
            // S6 P1-2 会话键形状门：会话键只认 UUID 形状。converter 产的 conversationId
            // 恒为 UUID 形状（L1 提取校验 / L2 派生格式化 / L3 random），非 UUID 形状只
            // 可能是异常值，不再进 by_session / traces。
            // ⚠️ 局限（诚实标注）：L3 随机兜底（converter.rs:1058）产的是**合法形状**的
            // 随机 UUID，provider 无法与 L1 真会话区分——根治需 converter 侧打 is_derived
            // 标记（研究 P1-2），本次改动范围不含 converter.rs，该残余留待同系改动。
            .filter(|s| Self::is_valid_uuid_shape(s))
            .map(|s| s.to_string());

        (model, session_id)
    }

    fn retry_delay(attempt: usize) -> Duration {
        // 指数退避 + 少量抖动，避免上游抖动时放大故障
        const BASE_MS: u64 = 200;
        const MAX_MS: u64 = 2_000;
        let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
        let backoff = exp.min(MAX_MS);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }

    /// 429 专用长退避：`1s → 2s → 4s → 8s`（上限 8s）。
    ///
    /// 与通用 `retry_delay`（200ms base，基础设施瞬态）区分：429 是**被上游限流**，
    /// 短退避会在同一账号上连打 —— 重试上限降到 4 之后，每次 429 都是宝贵的出账机会，
    /// 用长退避把一次客户端请求的 4 次上游调用摊到最坏 ~15s，尽早把错误交还给客户端
    /// （客户端有自己的退避），而不是在同一窗口内把同一账号砸 4 次。
    fn retry_delay_throttle(attempt: usize) -> Duration {
        const BASE_MS: u64 = 1_000;
        const MAX_MS: u64 = 8_000;
        let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
        let backoff = exp.min(MAX_MS);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }

    /// 慢速退避：专用于 MODEL_TEMPORARILY_UNAVAILABLE（容量过载）。
    ///
    /// 1s base，2x 指数，30s 上限 + 25% jitter。
    /// 与通用 `retry_delay`（200ms base，基础设施瞬态）区分：过载是容量级问题，
    /// 短暂快速重试只是反复冲击同一过载路径，慢速更合理。
    fn retry_delay_model_unavailable(attempt: usize) -> Duration {
        const BASE_MS: u64 = 1_000;
        const MAX_MS: u64 = 30_000;
        let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(5) as u32));
        let backoff = exp.min(MAX_MS);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }

    /// 将序列化的 Kiro 请求体中的 modelId 替换为指定值。
    ///
    /// 用于备用模型兜底（配置项与 `MODEL_TEMPORARILY_UNAVAILABLE` 重试耗尽联动）：
    /// 过载重试耗尽时，以备用模型再试一次。
    /// 替换路径：`conversationState.currentMessage.userInputMessage.modelId`。
    /// 解析/序列化失败时原样返回，保证函数不 panic。
    fn rewrite_model_id(request_body: &str, new_model: &str) -> String {
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(request_body) else {
            return request_body.to_string();
        };
        if let Some(mid) =
            v.pointer_mut("/conversationState/currentMessage/userInputMessage/modelId")
        {
            *mid = serde_json::Value::String(new_model.to_string());
        }
        serde_json::to_string(&v).unwrap_or_else(|_| request_body.to_string())
    }
}

/// 重试链内 429 显式 Retry-After 的合并（2026-08-16 对抗审查 m7：RA MINOR）。
///
/// `.max()` 而非 `.or()`：`.or()` 是**首个带值者**胜出——attempt1 的号 429 RA=10、
/// attempt2 的号 429 RA=120 时，客户端拿首个 10s 就重试，提前撞回上游仍在限流的
/// 窗口（被第二个号明确告知要等 120s）。`.max()` 保留**最大 RA**（「上游说多久等
/// 多久」的保守口径）：`max(None, Some(10)) = Some(10)`（首个 429 无 RA、后续有 RA
/// 时取后续）；`max(Some(120), None) = Some(120)`（先 429 后 5xx 无 RA 时首个 RA
/// 仍保留——`None < Some`，5xx 不能稀释 429 的退避指令）。
///
/// 抽成纯函数便于测试（与 `assemble_final_error` 同范式）——若回退成 `.or()`，
/// `merge_upstream_429_retry_after_keeps_max` 断言红。
fn merge_upstream_429_retry_after(
    current: Option<u64>,
    retry_after: Option<u64>,
) -> Option<u64> {
    current.max(retry_after)
}

/// S3：最早类型化 429 保留 —— 决定是否把重试链内首个上游 429 的显式 RA 并入终态错误串。
///
/// 场景（scheduling-429-research.md §2.3）：多号池 attempt1 = A 号 429（上游 RA 30s）、
/// attempt2 = C 号 5xx → 终态按**最后一个**错误分类 → 客户端拿 503+3s 而不是 429+30s，
/// 丢失「429 语义 + 上游精确 RA」（CC 对 429 走 `max(Retry-After, 退避)` 精确等待，
/// 对 503 只能指数退避，更早重打）。这里把 marker 并入终态串，map_provider_error 的
/// A7 分支（含 marker 判据）返回 429 + 上游 RA。
///
/// RA 值由 [`merge_upstream_429_retry_after`] 以 `.max()` 语义产生（m7：保留最大 RA，
/// 而非首见值）；本函数只负责「是否并入」，不重算。
///
/// # 限定（scheduling-429-research.md 方案 S2 的限定集）
///
/// ① **吸收层耗尽路径不转换**：`absorb_budget_exhausted=1` 的 503 是「网关已尽力」的
///    兼容语义（Cursor 见 429 掐会话），A2 分支本来就是 map_provider_error 第一条 ——
///    该 marker 存在即跳过；
/// ② **永久态/配额/背压分支不转换**：subscription_unsupported / model_unsupported /
///    inbound_admission_timeout / upstream_gate_full / shared_budget / 配额类
///    （各带自己的结构化 marker 或 reason 词表），转换会让对应分支的语义被 429 吞掉；
/// ③ **终态已是 429+RA 或带号池真值不重复打**（已有 marker / `retry_after_secs=`）；
/// ④ **仅 generic 瞬态终态转换**（last_outcome ∈ ServerError/OtherError/RateLimited，
///    覆盖 5xx/408/传输层/裸 429 终态）—— 认证失败/400/配额/风控/模型容量等已识别
///    终态保持原映射（与 zyphr 只在「终态 generic」时保留类型化 429 的语义一致）。
///
/// 抽成纯函数便于测试（与 `retry_delay` 等纯函数同范式）。
fn assemble_final_error(
    final_error: anyhow::Error,
    first_upstream_429_retry_after: Option<u64>,
    last_outcome: crate::usage::RequestOutcome,
) -> anyhow::Error {
    let Some(earliest_ra) = first_upstream_429_retry_after else {
        return final_error;
    };
    let s = final_error.to_string();
    let marker = crate::anthropic::handlers::UPSTREAM_RETRY_AFTER_MARKER_PREFIX;
    let eligible = !s.contains(marker)
        && !s.contains("retry_after_secs=")
        && !s.contains(crate::anthropic::handlers::ABSORB_BUDGET_EXHAUSTED_MARKER)
        && !s.contains("shared_budget_exhausted=1")
        && !s.contains("subscription_unsupported=1")
        && !s.contains("model_unsupported_by_pool=1")
        && !s.contains("inbound_admission_timeout=1")
        && !s.contains("upstream_gate_full=1")
        && !s.contains("quota_exhausted_all=1")
        && !crate::kiro::endpoint::default_is_monthly_request_limit(&s)
        && matches!(
            last_outcome,
            crate::usage::RequestOutcome::ServerError
                | crate::usage::RequestOutcome::OtherError
                | crate::usage::RequestOutcome::RateLimited
        );
    if eligible {
        // 与吸收 marker 同款：用 `context` 拼接保留原始错误链（面板/日志排障线索），
        // marker 必须与原文在同一层（anyhow 的 Display 只打最外层）。
        anyhow::anyhow!("{} {}{}", final_error, marker, earliest_ra)
    } else {
        final_error
    }
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;
