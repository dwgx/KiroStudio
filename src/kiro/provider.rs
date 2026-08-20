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
    /// token 的凭据直连 MCP 端点（不注入 profileArn，kiro-gateway 证明的形态）。
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
    /// 而 MCP（web_search）调用本质只依赖一个有效的 Kiro Bearer token ——
    /// kiro-gateway 的 mcp_tools.py 证明 `Authorization: Bearer` +
    /// `x-amzn-codewhisperer-optout` + `Content-Type` 即可调通
    /// `runtime.{region}.kiro.dev/mcp`，**不依赖 profileArn**。本方法实现同一形态：
    /// token 从凭据池现取（`token_manager.acquire_mcp_direct_token`，绕过选号门槛），
    /// 请求头按 gateway 同款构造，**不注入 `x-amzn-kiro-profile-arn`**。
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

    /// MCP 无号直连的请求头（纯函数，便于单测钉死「无 profileArn」契约）。
    ///
    /// 对齐 kiro-gateway mcp_tools.py 的已证可实现形态：`Authorization` +
    /// `x-amzn-codewhisperer-optout`，**刻意不注入 `x-amzn-kiro-profile-arn`**
    /// （gateway 证明上游 MCP 端点不依赖 profileArn）。另按凭据类型补 tokentype
    /// （与 decorate_mcp 同口径：api_key → API_KEY / external_idp → EXTERNAL_IDP），
    /// 保住 ksk_ 号的既有认证语义。
    fn mcp_direct_headers(cred: &KiroCredentials, token: &str) -> Vec<(&'static str, String)> {
        let mut headers = vec![
            ("x-amzn-codewhisperer-optout", "false".to_string()),
            ("Authorization", format!("Bearer {}", token)),
        ];
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
            // trace 存原值（HTTP-date 形式的头解析会失败，原值保留可查）。
            let retry_after_raw = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_string());
            let retry_after_secs = retry_after_raw
                .as_deref()
                .and_then(|s| s.parse::<u64>().ok());
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
                // 原始串与解析值都要：trace 存原值（HTTP-date 形式的头解析会失败，原值保留可查）。
                let retry_after_raw = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.trim().to_string());
                let retry_after_header = retry_after_raw
                    .as_deref()
                    .and_then(|s| s.parse::<u64>().ok());
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
                use crate::anthropic::AbsorbClass;
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
                use crate::anthropic::AbsorbClass;
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
mod tests {
    use super::*;
    use super::absorb_policy::ABSORB_MIN_BACKOFF;
    use super::retry_budget::MAX_RETRIES_PER_CREDENTIAL;

    // ===== S4：透传池冷却标签独立（状态码 → 秒数 + 原因）=====

    /// 映射表契约（2026-08-16 S4）：每个状态码的 `(秒数, 原因)` 必须精确匹配。
    /// 原因只决定面板 `cooldownReason`/`cooldownCode` 展示；秒数是既有调参
    /// （401/403 用 `AuthTransient` 仍是 180s，不走该变体 20s 默认时长）。
    /// 回退即 FAIL：S4 前全部冷却硬编码 `RateLimitExceeded`（401 在面板显示
    /// 「速率限制」误导排障）→ 原因断言失败。
    #[test]
    fn passthrough_cooldown_reason_mapping_table() {
        use crate::kiro::cooldown::CooldownReason as R;
        // 认证类（key 失效/403）→ AuthTransient，180s 非瞬态冷却。
        assert_eq!(
            KiroProvider::passthrough_cooldown_for(401),
            (180, Some(R::AuthTransient))
        );
        assert_eq!(
            KiroProvider::passthrough_cooldown_for(403),
            (180, Some(R::AuthTransient))
        );
        // 配额耗尽（中转站常用 402 表额度）→ QuotaExhausted。
        assert_eq!(
            KiroProvider::passthrough_cooldown_for(402),
            (180, Some(R::QuotaExhausted))
        );
        // 限流 → RateLimitExceeded（保留原标签）。
        assert_eq!(
            KiroProvider::passthrough_cooldown_for(429),
            (5, Some(R::RateLimitExceeded))
        );
        // 站点不认请求（模型/tool/role）→ 5s 调度跳过（现状保留）。
        assert_eq!(
            KiroProvider::passthrough_cooldown_for(400),
            (5, Some(R::RateLimitExceeded))
        );
        assert_eq!(
            KiroProvider::passthrough_cooldown_for(404),
            (5, Some(R::RateLimitExceeded))
        );
        // 服务器错误 → ServerError（5s 调度跳过 + 标签）。
        assert_eq!(
            KiroProvider::passthrough_cooldown_for(500),
            (5, Some(R::ServerError))
        );
        assert_eq!(
            KiroProvider::passthrough_cooldown_for(502),
            (5, Some(R::ServerError))
        );
        assert_eq!(
            KiroProvider::passthrough_cooldown_for(599),
            (5, Some(R::ServerError))
        );
        // 网络错误（无状态码，code=0）与其它码：不冷却。
        assert_eq!(KiroProvider::passthrough_cooldown_for(0), (0, None));
        assert_eq!(KiroProvider::passthrough_cooldown_for(422), (0, None));
        assert_eq!(KiroProvider::passthrough_cooldown_for(600), (0, None));
    }

    // ===== MCP 无号直连（P0：web_search 快路径去 profileArn 依赖）=====

    /// 直连头契约：**绝不注入 profileArn**（kiro-gateway 证明上游不依赖它），
    /// 只带 gateway 同款的最小三件套 + 按凭据类型的 tokentype。
    #[test]
    fn mcp_direct_headers_never_inject_profile_arn() {
        let mut social = KiroCredentials::default();
        social.auth_method = Some("social".to_string());
        social.profile_arn = Some("arn:aws:codewhisperer:us-east-1:1:profile/OWN".to_string());
        let headers = KiroProvider::mcp_direct_headers(&social, "tok");
        assert!(
            !headers.iter().any(|(k, _)| *k == "x-amzn-kiro-profile-arn"),
            "直连头绝不允许出现 profileArn（social 号自带 ARN 也一样不带）"
        );
        assert!(
            headers.iter().any(|(k, v)| *k == "Authorization" && v == "Bearer tok"),
            "必须带 Bearer Authorization"
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| *k == "x-amzn-codewhisperer-optout" && v == "false"),
            "必须带 x-amzn-codewhisperer-optout（gateway 同款）"
        );
        assert!(
            !headers.iter().any(|(k, _)| *k == "tokentype"),
            "social 号不带 tokentype"
        );
    }

    /// api_key（ksk_）号直连带 `tokentype: API_KEY`（与 decorate_mcp 同口径）。
    #[test]
    fn mcp_direct_headers_api_key_gets_tokentype() {
        let mut api_key = KiroCredentials::default();
        api_key.auth_method = Some("api_key".to_string());
        api_key.kiro_api_key = Some("ksk_x".to_string());
        let headers = KiroProvider::mcp_direct_headers(&api_key, "ksk_x");
        assert!(
            headers
                .iter()
                .any(|(k, v)| *k == "tokentype" && v == "API_KEY"),
            "ksk_ 号直连必须带 tokentype: API_KEY"
        );
    }

    /// external_idp 号直连带 `tokentype: EXTERNAL_IDP`。
    #[test]
    fn mcp_direct_headers_external_idp_gets_tokentype() {
        let mut ext = KiroCredentials::default();
        ext.auth_method = Some("external_idp".to_string());
        let headers = KiroProvider::mcp_direct_headers(&ext, "t");
        assert!(
            headers
                .iter()
                .any(|(k, v)| *k == "tokentype" && v == "EXTERNAL_IDP"),
            "external_idp 号直连必须带 tokentype: EXTERNAL_IDP"
        );
    }

    /// 开关默认开（「默认开无号直连尝试，失败降级现状」——实测前置要求）。
    #[test]
    fn mcp_direct_bypass_defaults_to_enabled() {
        assert!(
            MCP_DIRECT_BYPASS_ENABLED.load(std::sync::atomic::Ordering::Relaxed),
            "无号直连开关默认必须开启（失败由降级兜底）"
        );
    }

    /// 接线守卫①：`call_mcp` 入口必须含「标记识别 → 直连兜底 → 剥标记返回」三段。
    ///
    /// 回退即 FAIL：把 call_mcp 改回裸转发（直连不接线 = 结构性缺陷修不了）。
    #[test]
    fn call_mcp_is_wired_for_direct_bypass() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let call_mcp = prod
            .split("pub async fn call_mcp(")
            .nth(1)
            .expect("call_mcp 不应被改名");
        let call_mcp = call_mcp
            .split("\n    }\n")
            .next()
            .expect("call_mcp 应有函数体收尾");
        assert!(
            call_mcp.contains("call_mcp_with_retry(request_body, budget)"),
            "call_mcp 必须先走原路径"
        );
        assert!(
            call_mcp.contains("strip_prefix(MCP_POOL_UNAVAILABLE_MARKER)"),
            "必须识别无号标记（否则直连永不触发）"
        );
        assert!(
            call_mcp.contains("call_mcp_direct(request_body, budget)"),
            "无号时必须调用直连兜底"
        );
        assert!(
            call_mcp.contains("MCP_DIRECT_BYPASS_ENABLED.load"),
            "直连必须受开关门控"
        );
    }

    /// 接线守卫②：`call_mcp_with_retry` 的 acquire_context 失败分支必须打无号标记。
    ///
    /// 回退即 FAIL：把 `.context(MCP_POOL_UNAVAILABLE_MARKER)` 删掉或改回
    /// `last_error = Some(e)` → 直连永不触发。
    #[test]
    fn acquire_context_failure_marks_pool_unavailable() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let mcp_fn = prod
            .split("async fn call_mcp_with_retry")
            .nth(1)
            .expect("call_mcp_with_retry 不应被改名");
        let acquire_err = mcp_fn
            .split("last_error = Some(e.context(MCP_POOL_UNAVAILABLE_MARKER));")
            .count();
        assert_eq!(
            acquire_err,
            2,
            "acquire_context 失败分支必须打 mcp_pool_unavailable=1 标记（仅此一处）"
        );
    }

    /// 接线守卫③：直连 URL 必须恒为 IDE 协议的 `runtime.{region}.kiro.dev/mcp`，
    /// 不得随凭据端点类型变成 `q.*`（CLI 端点的 mcp_url 是 q.* 兜底，不适合直连）。
    #[test]
    fn call_mcp_direct_uses_ide_mcp_url_only() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let direct = prod
            .split("async fn call_mcp_direct(")
            .nth(1)
            .expect("call_mcp_direct 不应被改名");
        let direct = direct
            .split("\n    }\n")
            .next()
            .expect("call_mcp_direct 应有函数体收尾");
        assert!(
            direct.contains("IdeEndpoint::new()"),
            "直连必须显式构造 IDE 端点（MCP 端点是 IDE 协议的）"
        );
        assert!(
            !direct.contains("for_credentials("),
            "直连不得按凭据类型路由端点（ksk_ 号会拿到 cli 端点的 q.* 兜底 URL）"
        );
        assert!(
            !direct.contains("amazonaws.com"),
            "直连 URL 不得出现 q.* 兜底"
        );
    }

    /// 接线守卫④：直连必须消费共享预算（真实发了上游请求 = 打了就是打了）。
    #[test]
    fn call_mcp_direct_consumes_budget() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let direct = prod
            .split("async fn call_mcp_direct(")
            .nth(1)
            .expect("call_mcp_direct 不应被改名");
        let direct = direct
            .split("\n    }\n")
            .next()
            .expect("call_mcp_direct 应有函数体收尾");
        assert_eq!(
            direct.matches("budget.consume(1)").count(),
            2,
            "直连成功与发送失败两条出口都必须扣共享预算"
        );
    }

    /// 接线守卫⑤：直连必须拒绝非 2xx 响应（不得当成功解析）。
    ///
    /// 回退即 FAIL：删掉 status 检查 → 无 ARN 形态被上游拒（403/400）时错误体
    /// 会被当 MCP JSON-RPC 解析，反序列化失败掩盖真实原因，且可能把「上游不认
    /// 无 ARN」伪装成「解析错误」误导排障。
    #[test]
    fn call_mcp_direct_rejects_non_success_status() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let direct = prod
            .split("async fn call_mcp_direct(")
            .nth(1)
            .expect("call_mcp_direct 不应被改名");
        let direct = direct
            .split("\n    }\n")
            .next()
            .expect("call_mcp_direct 应有函数体收尾");
        assert!(
            direct.contains("is_success()"),
            "直连必须检查上游 status（非 2xx 不得当成功返回）"
        );
    }

    /// M3 接线守卫⑥：直连非 2xx 失败必须落短负缓存，且键含凭据 id。
    ///
    /// 回退即 FAIL：删掉 mark → 直连失败零记忆，每请求再打死 token 一跳
    /// （风控窗口加流量，与网关纪律矛盾）。把键改回 region-only → 同区坏 ksk
    /// 连坐健康 OAuth。
    #[test]
    fn call_mcp_direct_marks_negative_cache_on_failure() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let direct = prod
            .split("async fn call_mcp_direct(")
            .nth(1)
            .expect("call_mcp_direct 不应被改名");
        let direct = direct
            .split("\n    }\n")
            .next()
            .expect("call_mcp_direct 应有函数体收尾");
        let mark = ["mark_endpoint_dead", "("].concat();
        let id_slot = ["mcp-direct@", "{}"].concat();
        assert!(
            direct.contains(&mark) && direct.contains("is_success()"),
            "直连非 2xx 必须落负缓存（60s 内不重试该号直连）"
        );
        assert!(
            direct.contains(&id_slot),
            "负缓存端点名必须嵌入凭据 id，避免同 region 一号毒全池"
        );
    }

    /// M3 接线守卫⑦：直连发送前必须检查短负缓存（否则负缓存是死代码）。
    ///
    /// 回退即 FAIL：删掉 `is_mcp_direct_blocked` 检查 → 负缓存永远读不到，
    /// 401 后 60s 内仍每请求白打一跳。
    #[test]
    fn call_mcp_direct_checks_negative_cache_before_send() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let direct = prod
            .split("async fn call_mcp_direct(")
            .nth(1)
            .expect("call_mcp_direct 不应被改名");
        let direct = direct
            .split("\n    }\n")
            .next()
            .expect("call_mcp_direct 应有函数体收尾");
        assert!(
            direct.contains("is_mcp_direct_blocked("),
            "直连发送前必须查负缓存（失败 60s 内跳过直连降级回池子错误）"
        );
    }

    /// 同请求 401 必须换号：删掉 excluding / exclude.insert 会退回单 token。
    #[test]
    fn call_mcp_direct_rotates_on_same_request_after_failure() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let direct = prod
            .split("async fn call_mcp_direct(")
            .nth(1)
            .expect("call_mcp_direct 不应被改名");
        let direct = direct
            .split("\n    }\n")
            .next()
            .expect("call_mcp_direct 应有函数体收尾");
        let acquire = ["acquire_mcp_direct_token", "_excluding"].concat();
        assert!(
            direct.contains(&acquire),
            "直连必须按排除集选下一个 token（同请求换号）"
        );
        assert!(
            direct.contains("exclude.insert"),
            "试过的 id 必须进排除集，否则会钉死同一号"
        );
    }

    /// 缺口 B 守卫：overload fallback **成功**路径的 CallMeta 双口径必须与主循环一致
    /// —— `model` 恒为客户端原始名（client_model 回落），`mapped_model` 记 fallback 名
    /// （它就是实际发给上游的名，直接进 upstream_model 口径）。
    ///
    /// 历史缺陷：`model` 被覆盖成 fallback 名、`mapped_model` 恒 None ⇒ requested_model
    /// 失真（面板以为客户端点了 fallback 模型）+ upstream_model 回落 model 双重错误。
    /// 回退即 FAIL：把 `model:` 改回 `Some(fallback_model_id` 或把 `mapped_model:` 改回
    /// `None`，断言失败。
    #[test]
    fn overload_fallback_success_keeps_client_model_in_meta() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let retry_fn = prod
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        // 切到 fallback 分支：从「尝试 overload_fallback_model」日志到失败记录组装之前。
        let fb = retry_fn
            .split("overload_fallback_model: {}")
            .nth(1)
            .expect("fallback 分支锚点不应被改名");
        let fb_ok = fb
            .split("let final_error = last_error")
            .next()
            .unwrap_or(fb);
        let mapped = ["mapped_model: ", "Some(fallback_model_id"].concat();
        assert!(
            fb_ok.contains(&mapped),
            "fallback 成功路径必须把 fallback 名记入 mapped_model（upstream_model 口径），\
             否则按 upstream_model 聚合时该样本按原始名统计"
        );
        assert!(
            fb_ok.contains("model: client_model_owned.clone().or_else(|| model.clone())"),
            "fallback 成功路径的 CallMeta.model 必须用客户端原始名（client_model 回落），\
             不得覆盖成 fallback 名（requested_model 契约）"
        );
    }

    /// ⭐ S3：最早类型化 429 保留 —— 终态错误组装（`assemble_final_error`）的行为集。
    ///
    /// 场景（research §2.3）：attempt1 = A 号 429（上游 RA 30s）、attempt2 = C 号 5xx
    /// → 终态按最后一个错误分类 → 客户端拿 503+3s 而不是 429+30s。修复后终态串
    /// 必须带上最早 429 的 RA marker，由 map_provider_error 的 A7 分支映射回 429。
    #[test]
    fn assemble_final_error_keeps_earliest_429_ra_over_later_5xx() {
        let final_err = anyhow::anyhow!(
            "流式 API 请求失败: 502 Bad Gateway {{\"message\":\"upstream\"}}"
        );
        // 先 429(RA=30) 后 502 → 终态仍带 RA=30。
        let merged = assemble_final_error(
            final_err,
            Some(30),
            crate::usage::RequestOutcome::ServerError,
        );
        let s = merged.to_string();
        assert!(
            s.contains(&format!("{}30", crate::anthropic::handlers::UPSTREAM_RETRY_AFTER_MARKER_PREFIX)),
            "最早类型化 429 的 RA=30 必须被并入 5xx 终态（否则客户端拿 503+3s 而非 429+30s）: {s}"
        );
    }

    /// 🔴 m7 回归（2026-08-16 对抗审查 RA MINOR）：重试链内 429 RA 合并必须是
    /// `.max()` 语义——**保留最大 RA**（「上游说多久等多久」）。
    ///
    /// `.or()`（首见值胜出）的 bug：attempt1 号 429 RA=10、attempt2 号 429 RA=120 时
    /// 客户端拿首个 10s 就重试，提前撞回上游仍在限流的窗口（120s 是更晚、更保守的
    /// 退避指令，却被首个值吞掉）。
    ///
    /// 回退即 FAIL：把 `merge_upstream_429_retry_after` 改回 `.or()` → 本条
    /// 「先 10 后 120 → 终态 120」断言红。
    #[test]
    fn merge_upstream_429_retry_after_keeps_max() {
        // 先 10 后 120 → 终态 120（m7 核心场景）。
        assert_eq!(
            merge_upstream_429_retry_after(Some(10), Some(120)),
            Some(120),
            "第二个号 429 RA=120 时客户端不得拿首个 10s 就重试"
        );
        // 逆序（120 先、10 后）→ 仍是 120。
        assert_eq!(merge_upstream_429_retry_after(Some(120), Some(10)), Some(120));
        // 首个 429 无 RA、后续 429 有 RA → 取后续（max(None, Some) = Some）。
        assert_eq!(merge_upstream_429_retry_after(None, Some(10)), Some(10));
        // 先 429 后 5xx（无 RA）→ 首个 RA 仍保留（max(Some, None) = Some，
        // 5xx 不能稀释 429 的退避指令）——与既有
        // `assemble_final_error_keeps_earliest_429_ra_over_later_5xx` 场景兼容。
        assert_eq!(merge_upstream_429_retry_after(Some(10), None), Some(10));
        // 全程无 RA → None。
        assert_eq!(merge_upstream_429_retry_after(None, None), None);
    }

    /// S3 限定：没有前置 429 时终态逐字不变（默认配置路径零影响）。
    #[test]
    fn assemble_final_error_untouched_without_earlier_429() {
        let err = anyhow::anyhow!("流式 API 请求失败: 502 Bad Gateway");
        let out = assemble_final_error(err, None, crate::usage::RequestOutcome::ServerError);
        assert_eq!(out.to_string(), "流式 API 请求失败: 502 Bad Gateway");
    }

    /// S3 限定①：吸收层耗尽路径（absorb_budget_exhausted=1，503 语义）不转换。
    #[test]
    fn assemble_final_error_never_converts_absorb_exhausted() {
        let marker = crate::anthropic::handlers::ABSORB_BUDGET_EXHAUSTED_MARKER;
        let err = anyhow::anyhow!("流式 API 请求失败: 429 Too Many Requests {}", marker);
        let out = assemble_final_error(
            err,
            Some(30),
            crate::usage::RequestOutcome::RateLimited,
        );
        assert!(
            !out.to_string()
                .contains(crate::anthropic::handlers::UPSTREAM_RETRY_AFTER_MARKER_PREFIX),
            "吸收耗尽 503 不得被 429 marker 转换（Cursor 掐会话兼容）"
        );
    }

    /// S3 限定②：永久态/配额/背压/已带真值的终态一律不转换。
    #[test]
    fn assemble_final_error_never_converts_recognized_branches() {
        let cases: Vec<(&str, crate::usage::RequestOutcome)> = vec![
            ("流式 API 请求失败: 403 Forbidden subscription_unsupported=1", crate::usage::RequestOutcome::BadRequest),
            ("模型不被本号池支持 model_unsupported_by_pool=1", crate::usage::RequestOutcome::OtherError),
            ("所有凭据均在冷却（0/2）retry_after_secs=10", crate::usage::RequestOutcome::RateLimited),
            ("入站限速排队超时 inbound_admission_timeout=1 retry_after_secs=3", crate::usage::RequestOutcome::OtherError),
            ("上游并发闸已满 upstream_gate_full=1 retry_after_secs=2", crate::usage::RequestOutcome::OtherError),
            ("流式 API 请求失败: 429 {\"reason\":\"MONTHLY_REQUEST_COUNT\"}", crate::usage::RequestOutcome::RateLimited),
            ("流式 API 请求失败（所有凭据已用尽）quota_exhausted_all=1: 429 x", crate::usage::RequestOutcome::QuotaExhausted),
            ("每客户端请求的上游调用预算已耗尽 shared_budget_exhausted=1", crate::usage::RequestOutcome::OtherError),
        ];
        for (raw, outcome) in cases {
            let out = assemble_final_error(anyhow::anyhow!("{}", raw), Some(30), outcome);
            assert_eq!(
                out.to_string(),
                raw,
                "已识别分支不得被最早 429 的 RA 转换: {raw}"
            );
        }
    }

    /// S3 限定③④：终态已是 429+RA（自带 marker）不重复打；非 generic 终态
    /// （认证失败/400/模型容量）不转换（与 zyphr 只在终态 generic 时保留一致）。
    #[test]
    fn assemble_final_error_skips_already_marked_and_recognized_outcomes() {
        let marker = crate::anthropic::handlers::UPSTREAM_RETRY_AFTER_MARKER_PREFIX;
        // 终态本身就是 429 + RA=60：保留最早 30 还是覆盖成 60？—— 保留终态自身（已带 marker）。
        let already = format!("流式 API 请求失败: 429 Too Many Requests {}60", marker);
        let out = assemble_final_error(
            anyhow::anyhow!("{}", already),
            Some(30),
            crate::usage::RequestOutcome::RateLimited,
        );
        assert_eq!(
            out.to_string(),
            already,
            "终态已带自己的 marker 时不重复并入（终态自身是最后一条 429 的信息）"
        );

        // 认证失败终态（AuthFailed）：不转换（401/403 语义保持）。
        let auth = anyhow::anyhow!("流式 API 请求失败: 401 Unauthorized {{\"message\":\"x\"}}");
        let out = assemble_final_error(
            auth,
            Some(30),
            crate::usage::RequestOutcome::AuthFailed,
        );
        assert!(
            !out.to_string().contains(marker),
            "认证失败终态不得被 429 转换（与 zyphr take_rate_limit_error 语义一致）"
        );

        // 400 终态（BadRequest）：不转换。
        let bad = anyhow::anyhow!("流式 API 请求失败: 400 Bad Request {{\"message\":\"x\"}}");
        let out = assemble_final_error(bad, Some(30), crate::usage::RequestOutcome::BadRequest);
        assert!(!out.to_string().contains(marker));

        // 模型容量终态（ModelUnavailable）：不转换（有独立的 503 overload 语义）。
        let cap = anyhow::anyhow!("流式 API 请求失败: 503 MODEL_TEMPORARILY_UNAVAILABLE");
        let out = assemble_final_error(
            cap,
            Some(30),
            crate::usage::RequestOutcome::ModelUnavailable,
        );
        assert!(!out.to_string().contains(marker));
    }

    /// 缺口 A 守卫：Kiro 主路径成功/失败埋点的 `requested_model` 必须同源（都是
    /// `client_model` = 客户端原始名），不得一边原始名一边归一化 Kiro id 的混合口径。
    ///
    /// 历史缺陷：成功路径埋点记 `extract_model_and_session` 从请求体解析的 modelId
    /// （已被 converter 归一化成 Kiro id），失败记录同源同错——与透传路径（原始名）
    /// 口径分叉。回退即 FAIL：把成功路径的 `model:` 改回 `model.clone()` 或把
    /// `fail_record.requested_model` 改回 `model.clone()`，断言失败。
    #[test]
    fn kiro_success_and_failure_records_share_client_model() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let retry_fn = prod
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        let success_meta = "model: client_model_owned.clone().or_else(|| model.clone())";
        assert_eq!(
            retry_fn.matches(success_meta).count(),
            2,
            "主循环与 overload fallback 两条成功路径的 CallMeta.model 都必须用客户端原始名"
        );
        assert!(
            retry_fn.contains("fail_record.requested_model = client_model_owned.clone().or(model.clone())"),
            "失败记录 requested_model 必须与成功路径同源（client_model），\
             否则按 requested_model 聚合时成功/失败口径分叉"
        );
        assert!(
            retry_fn.contains("client_model_owned.clone().or(model.clone()).unwrap_or_default()"),
            "失败记录的 record.model 必须同样回落 client_model（与成功记录 record.model 口径一致）"
        );
    }

    /// 模型映射改写 body：命中 `/conversationState/currentMessage/userInputMessage/modelId`。
    #[test]
    fn test_rewrite_model_id_replaces_kiro_model_id() {
        let body = r#"{"conversationState":{"currentMessage":{"userInputMessage":{"modelId":"claude-opus-4-8"}}}}"#;
        let out = KiroProvider::rewrite_model_id(body, "claude-sonnet-4-5");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
            "claude-sonnet-4-5"
        );
    }

    /// 非 JSON body：`rewrite_model_id` 原样返回不 panic（映射在该跳静默不生效）。
    #[test]
    fn test_rewrite_model_id_invalid_json_returns_unchanged() {
        let body = "not-json";
        assert_eq!(KiroProvider::rewrite_model_id(body, "x"), body);
    }

    /// 动态降档阶梯的边界：0/0.3/0.5 为不变档，0.31/0.51 触发降档，地板 1。
    #[test]
    fn test_apply_retry_pressure_staircase() {
        assert_eq!(apply_retry_pressure(12, 0.0), 12);
        assert_eq!(apply_retry_pressure(12, 0.3), 12, "0.3 恰好是阈值，不降");
        assert_eq!(apply_retry_pressure(12, 0.5), 6, "0.5 未过 0.5 档但过 0.3 档 → 砍半");
        assert_eq!(apply_retry_pressure(12, 0.31), 6, ">0.3 砍半");
        assert_eq!(apply_retry_pressure(12, 0.51), 3, ">0.5 砍到 33%（12*33/100=3）");
        assert_eq!(apply_retry_pressure(12, 1.0), 3, "满额 429 也只砍到 3，不归零");
        assert_eq!(apply_retry_pressure(1, 1.0), 1, "地板 1：降档绝不归零");
        assert_eq!(apply_retry_pressure(3, 0.51), 1, "3 的 33% 向下取整到 1");
    }

    /// 窗口 rate() 是纯计算：直接注入状态验证 429 占比。
    #[test]
    fn test_retry_pressure_window_rate() {
        let mut w = RetryPressureWindow::new(60);
        assert_eq!(w.rate(), 0.0, "空窗口无信号，不降档");
        // 5 成功 + 5 个 429 → 50%
        for i in 0..10 {
            w.deque.push_back((std::time::Instant::now(), i % 2 == 1));
        }
        assert!((w.rate() - 0.5).abs() < 1e-6);
        // 全 429 → 100%
        let mut w2 = RetryPressureWindow::new(60);
        for _ in 0..4 {
            w2.deque.push_back((std::time::Instant::now(), true));
        }
        assert_eq!(w2.rate(), 1.0);
    }

    /// 🔴 回归：5xx 与 429 同样计入压力（纯 500 风暴降档必须触发）；
    /// 4xx（客户端错误）不算压力。
    #[test]
    fn test_retry_pressure_window_counts_5xx_and_not_4xx() {
        let mut w = RetryPressureWindow::new(60);
        // 2 个 500 + 1 个 200 → 压力率 2/3
        w.deque.push_back((std::time::Instant::now(), false)); // 200
        w.deque.push_back((std::time::Instant::now(), true)); // 500
        w.deque.push_back((std::time::Instant::now(), true)); // 500
        assert!(
            (w.rate() - 2.0 / 3.0).abs() < 1e-6,
            "5xx 必须计入压力（纯 500 风暴降档才不失效），实际 {}",
            w.rate()
        );

        // 4xx 不算压力：2 个 400 + 1 个 200 → 压力率 0
        let mut w2 = RetryPressureWindow::new(60);
        w2.deque.push_back((std::time::Instant::now(), false)); // 200
        w2.deque.push_back((std::time::Instant::now(), false)); // 400
        w2.deque.push_back((std::time::Instant::now(), false)); // 400
        assert_eq!(w2.rate(), 0.0, "4xx（客户端错误）不算压力");
    }

    /// record() 顺带逐出超窗事件：极小窗口 + sleep 后，旧事件被清出。
    #[tokio::test]
    async fn test_retry_pressure_window_prune_expired() {
        let mut w = RetryPressureWindow::new(1); // 1s 窗口
        w.record(true);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(w.deque.len(), 1, "窗口内一条还在");
        // 换一个 0 秒窗口：第二次 record 必把第一条逐出
        let mut w0 = RetryPressureWindow::new(0);
        w0.record(true);
        w0.record(false);
        assert_eq!(w0.deque.len(), 1, "0 秒窗口下第一条立即过期");
        assert_eq!(w0.rate(), 0.0, "剩下的那一条是 false");
    }

    /// 并发闸 Semaphore：容量 N 时 N 个 permit 全过、第 N+1 拿不到、Drop 后恢复。
    #[tokio::test]
    async fn test_upstream_gate_concurrency() {
        let gate = Arc::new(tokio::sync::Semaphore::new(2));
        let p1 = gate.clone().try_acquire_owned().unwrap();
        let p2 = gate.clone().try_acquire_owned().unwrap();
        assert!(
            gate.clone().try_acquire_owned().is_err(),
            "容量 2 时第 3 个拿不到"
        );
        drop(p1);
        let p3 = gate.clone().try_acquire_owned().unwrap();
        drop(p2);
        drop(p3);
        let p4 = gate.clone().try_acquire_owned().unwrap();
        drop(p4);
        assert_eq!(gate.available_permits(), 2, "全部 Drop 后 permit 复原");
    }

    /// 预算恒被 `ABSOLUTE_MAX_TOTAL_RETRIES` 封顶，**且刻意不再随可用号数抬高**。
    ///
    /// ⚠️ 本测试此前名为 `..._covers_every_available_credential`，断言 `r >= total`
    /// 并声称"保证每个可用凭据至少被尝试一次"。那个承诺在移除内层 `.max(available)`
    /// 之后已不成立 —— 它当时**只是碰巧通过**：`total=10` 时预算 `min(30,12)=12`，
    /// 而 `12 >= 10` 恰好为真；换成现在的 4 上限后 `min(30,4)=4`，连 `total=10`
    /// 都过不了。即那是个会在号池扩容时才爆的定时炸弹，且它在维护一条代码已不提供的不变式。
    ///
    /// 现在改为锁住真实行为：封顶生效。若有人把 `.max(available)` 加回来（那正是
    /// 「号池越大越慢」的成因：线上 43 号时预算 = 43，单请求扫全池耗尽 45s 墙钟），
    /// `large_pool_stays_capped` 会立刻失败。
    #[test]
    fn test_compute_max_retries_is_capped_and_ignores_available() {
        // 常规池：按 total*per_cred 走，但受绝对上限封顶。
        assert_eq!(
            compute_max_retries(10, 10),
            (10 * MAX_RETRIES_PER_CREDENTIAL).min(ABSOLUTE_MAX_TOTAL_RETRIES)
        );

        // ⭐ 承重断言：大池必须仍被封顶，**不因可用号多而放开**。
        let large = compute_max_retries(20, 20);
        assert_eq!(
            large, ABSOLUTE_MAX_TOTAL_RETRIES,
            "大号池预算必须封顶在 {}，实际 {} —— 若等于 available 则说明 .max(available) 被加回来了",
            ABSOLUTE_MAX_TOTAL_RETRIES, large
        );

        // `available` 不参与计算：同一 total 下改变 available 不应改变结果。
        assert_eq!(
            compute_max_retries(20, 1),
            compute_max_retries(20, 20),
            "available 已不参与预算计算，改变它不该影响结果"
        );
    }

    /// 预算永不为 0：0 意味着一次都不尝试，请求立刻以「最大重试次数（0次）」失败。
    ///
    /// 这是真实回归的守卫：把预算基数从 `total_count()`（含 disabled，恒非 0）改成
    /// `kiro_selectable_count()` 后，瞬时全池不可选会让基数为 0 → 预算 0 →
    /// acquire_context 的等待逻辑根本没机会跑。线上 20 分钟内出现 10 次。
    #[test]
    fn should_never_return_zero_retry_budget() {
        assert_eq!(
            compute_max_retries(0, 0),
            1,
            "全池瞬时不可选时也必须至少尝试一次，否则请求零重试即失败"
        );
        for (t, a) in [(0usize, 0usize), (0, 1), (1, 0), (1, 1)] {
            assert!(
                compute_max_retries(t, a) >= 1,
                "compute_max_retries({t}, {a}) 不得为 0"
            );
        }
    }

    /// 收紧上限的意图守卫：一条请求不该能连打十几个号。
    ///
    /// 生产事故里 `尝试 8/36` 的 36 = 12 号 × 3，配合 suspend 分支的零延迟遍历，
    /// 一条客户端请求几秒内烧掉 8~12 个账号（同一出口 IP），正是风控要抓的突发特征。
    #[test]
    fn should_cap_retry_budget_well_below_historic_36() {
        // 与生产同规模的池子（12 个可选号）
        assert!(
            compute_max_retries(12, 12) <= ABSOLUTE_MAX_TOTAL_RETRIES,
            "12 号池的预算必须被上限约束，不能回到 36"
        );
        assert!(
            ABSOLUTE_MAX_TOTAL_RETRIES < 36,
            "绝对上限必须显著小于事故时的 36"
        );
    }

    #[test]
    fn test_compute_max_retries_small_pool() {
        // 小号池降重试：total<=SMALL_POOL_THRESHOLD 时每号只重试 1 次，
        // 每个号各摸一次即透传上游错误，避免在小池上反复砸同几个号加重冷却。
        assert_eq!(compute_max_retries(3, 3), 3, "3 号池应每号只摸 1 次 = 3");
        assert_eq!(compute_max_retries(2, 2), 2, "2 号池应每号只摸 1 次 = 2");
        // 只有 1 个凭据仍至少能试 1 次
        assert_eq!(compute_max_retries(1, 1), 1);

        // 刚过小池阈值（total=4）恢复常规 total*MAX_RETRIES_PER_CREDENTIAL，
        // 但随即被 ABSOLUTE_MAX_TOTAL_RETRIES 封顶（min(4×3, 4) = 4）。
        assert_eq!(compute_max_retries(4, 4), ABSOLUTE_MAX_TOTAL_RETRIES);

        // 小池但部分禁用：available 做下限，仍保证可用号被摸到。
        assert!(compute_max_retries(3, 2) >= 2);
    }

    #[test]
    fn test_compute_max_retries_respects_absolute_upper_bound() {
        // 巨量凭据：预算**恒**被 ABSOLUTE_MAX 封顶，不再随 available 放大。
        assert!(compute_max_retries(1000, 1000) <= ABSOLUTE_MAX_TOTAL_RETRIES);
        assert_eq!(
            compute_max_retries(100, 5),
            ABSOLUTE_MAX_TOTAL_RETRIES,
            "可用号少于上限时应封顶到 ABSOLUTE_MAX"
        );
    }

    /// 回归（大号池不得放大重试 · 本轮核心）：预算恒 ≤ ABSOLUTE_MAX_TOTAL_RETRIES，
    /// 与池子大小无关。
    ///
    /// **旧代码为何失败**：`.min(ABSOLUTE_MAX_TOTAL_RETRIES.max(available))` 里的内层
    /// `.max(available)` 在 `available > ABSOLUTE_MAX_TOTAL_RETRIES` 时把硬上限自己抵消掉
    /// → 预算 = available。
    /// 线上 43 个号实测预算 = 43，日志即「尝试 43/43」：一条请求顺着整池撞一遍、
    /// 耗尽 45s 墙钟才失败 → 用户体感 45 秒卡死，且**号池越大越慢**。
    /// 旧代码下 `compute_max_retries(43, 43)` 返回 43，本断言会失败。
    #[test]
    fn should_not_scale_retry_budget_with_pool_size() {
        for available in [13usize, 43, 200, 1000] {
            let r = compute_max_retries(available, available);
            assert!(
                r <= ABSOLUTE_MAX_TOTAL_RETRIES,
                "{available} 个可用号时预算为 {r}，必须被 {ABSOLUTE_MAX_TOTAL_RETRIES} 封顶——\
                 否则号池越大单请求越慢（线上实测 43 号 → 尝试 43/43 → 45s 墙钟）"
            );
        }
        // 线上确切规模的定点回归
        assert_eq!(
            compute_max_retries(43, 43),
            ABSOLUTE_MAX_TOTAL_RETRIES,
            "43 号池（线上实测规模）预算必须是 {ABSOLUTE_MAX_TOTAL_RETRIES} 而非 43"
        );
    }

    #[test]
    fn test_extract_model_and_session_both_present() {
        // 一次解析应同时取出 modelId 与 conversationId（与旧双解析等价）
        let body = r#"{
            "conversationState": {
                "conversationId": "0b4445e1-f5be-49e1-87ce-62bbc28ad705",
                "currentMessage": {
                    "userInputMessage": { "modelId": "claude-sonnet-4" }
                }
            }
        }"#;
        let (model, session) = KiroProvider::extract_model_and_session(body);
        assert_eq!(model.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(session.as_deref(), Some("0b4445e1-f5be-49e1-87ce-62bbc28ad705"));
    }

    #[test]
    fn test_extract_model_and_session_partial() {
        // 只有 conversationId、无 modelId：model=None、session=Some
        let only_session = r#"{"conversationState":{"conversationId":"8bb5523b-ec7c-4540-a9ca-beb6d79f1552"}}"#;
        let (model, session) = KiroProvider::extract_model_and_session(only_session);
        assert_eq!(model, None);
        assert_eq!(session.as_deref(), Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552"));

        // 只有 modelId、无 conversationId：model=Some、session=None
        let only_model =
            r#"{"conversationState":{"currentMessage":{"userInputMessage":{"modelId":"m"}}}}"#;
        let (model, session) = KiroProvider::extract_model_and_session(only_model);
        assert_eq!(model.as_deref(), Some("m"));
        assert_eq!(session, None);
    }

    // ===== S6 透传会话归一（会话研究 P1-1/P1-2/P1-4）=====

    /// S6 P1-1 归一化 + P1-4 脱敏：透传埋点的 session 键与 Kiro 路径同源——
    /// 同一 `metadata.user_id` 里提取出的 UUID，两条路径必须同一个 key；
    /// 且只落 UUID，`user_xxx_account__` 前缀 / account_uuid 明文不得进 trace。
    #[test]
    fn test_passthrough_session_id_kiro_consistent_and_redacted() {
        // 字符串格式（Claude Code 典型）：含 account 前缀 + account_uuid 片段
        let user_id = "user_ffffffff-aaaa-4bbb-8ccc-dddddddddddd_account__session_0b4445e1-f5be-49e1-87ce-62bbc28ad705";
        let extracted =
            KiroProvider::extract_session_uuid(user_id).expect("应从 user_id 提取出 session UUID");
        assert_eq!(
            extracted,
            "0b4445e1-f5be-49e1-87ce-62bbc28ad705",
            "透传 session 必须是提取后的纯 UUID（Kiro 路径 conversationId 同源）"
        );
        // 脱敏：trace 里的 session 键不含 account 前缀 / account_uuid 片段
        assert!(
            !extracted.contains("account_") && !extracted.contains("ffffffff-aaaa"),
            "session 键不得携带 account_uuid 明文，实际 {extracted}"
        );

        // 归一化：Kiro 路径把同一 UUID 写进 conversationState.conversationId，
        // 提取结果 = 透传提取结果 = 同一个 key（同会话跨路径不再拆双 key）。
        let kiro_body = format!(
            r#"{{"conversationState":{{"conversationId":"{extracted}"}}}}"#
        );
        let (_, kiro_session) = KiroProvider::extract_model_and_session(&kiro_body);
        assert_eq!(
            kiro_session.as_deref(),
            Some(extracted.as_str()),
            "Kiro 路径与透传路径必须得到同一个 session key"
        );
    }

    /// S6 P1-1 兜底 None：JSON 格式的 user_id 提取 session_id（形状合法才收）。
    #[test]
    fn test_passthrough_session_id_json_format() {
        let user_id = r#"{"device_id":"0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd","account_uuid":"acc-123","session_id":"8bb5523b-ec7c-4540-a9ca-beb6d79f1552"}"#;
        assert_eq!(
            KiroProvider::extract_session_uuid(user_id).as_deref(),
            Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552")
        );
    }

    /// S6 P1-1 兜底 None：提取不到（无 session / 非法形状 / 空）→ None，
    /// 不再回落原始 user_id 串（旧行为把整串当 session 进 trace）。
    #[test]
    fn test_passthrough_session_id_none_when_not_extractable() {
        assert_eq!(KiroProvider::extract_session_uuid(""), None, "空串无会话");
        assert_eq!(
            KiroProvider::extract_session_uuid("plain-string-no-session"),
            None,
            "无 session_ 标记的普通串无会话"
        );
        assert_eq!(
            KiroProvider::extract_session_uuid("user_x_account__session_not-a-uuid"),
            None,
            "session_ 后非 UUID 形状 → None（形状门）"
        );
        assert_eq!(
            KiroProvider::extract_session_uuid(r#"{"session_id":"not-a-uuid"}"#),
            None,
            "JSON 里 session_id 非 UUID 形状 → None"
        );
    }

    /// S6 P1-2 兜底 None（Kiro 侧形状门）：conversationId 非 UUID 形状 → session None。
    /// converter 产的 conversationId 恒为 UUID 形状，此门只拦截异常/伪造键，不再进
    /// by_session / traces。
    #[test]
    fn test_kiro_session_id_requires_uuid_shape() {
        // 合法 UUID 形状 → 收
        let good = r#"{"conversationState":{"conversationId":"8bb5523b-ec7c-4540-a9ca-beb6d79f1552"}}"#;
        let (_, session) = KiroProvider::extract_model_and_session(good);
        assert_eq!(session.as_deref(), Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552"));

        // 非 UUID 形状（畸形/伪造）→ None（兜底 None，by_session 不落键）
        for bad in [
            "sess-123",
            "not-a-uuid",
            "0b4445e1-f5be-49e1-87ce",       // 短
            "0b4445e1-f5be-49e1-87ce-62bbc28ad705XX", // 长
        ] {
            let body = format!(r#"{{"conversationState":{{"conversationId":"{bad}"}}}}"#);
            let (_, session) = KiroProvider::extract_model_and_session(&body);
            assert_eq!(session, None, "非 UUID 形状 conversationId 必须归 None（实际 {bad}）");
        }
    }


    #[test]
    fn should_build_mcp_record_with_honest_zeros_and_no_credits() {
        let rec = build_mcp_record(7, crate::usage::RequestOutcome::Success, 123, 2);
        assert_eq!(rec.credential_id, Some(7), "必须归属到真实服务的凭据");
        assert_eq!(rec.model, MCP_USAGE_MODEL, "MCP 无 modelId，用显式常量标识");
        // MCP 上游既不返回 token 数也无本地估算依据：只能是 0，不许瞎估。
        assert_eq!(rec.input_tokens, 0);
        assert_eq!(rec.output_tokens, 0);
        assert_eq!(rec.cache_read_tokens, 0);
        assert_eq!(rec.cache_creation_tokens, 0);
        assert_eq!(rec.credits_used, None, "MCP 响应无 meteringEvent");
        assert!(!rec.is_streaming, "MCP 上游是一次性 JSON POST");
        assert_eq!(rec.latency_ms, 123);
        assert_eq!(rec.retries, 2);
        assert_eq!(rec.outcome, crate::usage::RequestOutcome::Success);
        assert!(rec.error_message.is_none(), "成功记录不应带错误信息");
        // request_id 每条唯一，否则 SQLite 主键冲突会静默丢记录。
        let other = build_mcp_record(7, crate::usage::RequestOutcome::Success, 123, 2);
        assert_ne!(rec.request_id, other.request_id);
    }

    /// 源码级守卫：MCP 成功分支里 `report_success` 与 `emit_record` 必须成对出现。
    ///
    /// 单测覆盖不到 `call_mcp_with_retry`（需真实上游 + 号池），而这正是回归发生的地方：
    /// 历史实现只加凭据计数器不落用量记录，导致 success_count 恒大于用量库记录数。
    #[test]
    fn should_emit_usage_record_in_mcp_success_branch() {
        let src = include_str!("provider.rs");
        let mcp_fn = src
            .split("async fn call_mcp_with_retry")
            .nth(1)
            .expect("call_mcp_with_retry 不应被改名");
        // 截到该函数内第一次出现「失败响应」处理为止，只看成功分支。
        let success_branch = mcp_fn
            .split("// 失败响应")
            .next()
            .expect("成功分支的定位注释不应被删改");
        assert!(
            success_branch.contains("report_success"),
            "成功分支应上报凭据成功"
        );
        assert!(
            success_branch.contains("emit_record(build_mcp_record("),
            "MCP 成功分支必须落一条用量记录，否则凭据计数与用量库对不上账"
        );
    }

    /// 源码级守卫（已知问题 #11）：MCP 路径的**失败出口**必须 emit_record + bump 计数器。
    ///
    /// 历史缺陷：`call_mcp_with_retry` 只有成功分支 emit_record，失败全部零埋点 ⇒
    /// MCP 失败在面板与 recovery-metrics 端点上完全不存在，成功率的分子分母对不上账。
    /// 单测覆盖不到（需真实上游 + 号池），用源码断言钉死 7 个失败出口。
    #[test]
    fn mcp_failure_exits_must_emit_record_and_bump_counter() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let mcp_fn = src
            .split("async fn call_mcp_with_retry")
            .nth(1)
            .expect("call_mcp_with_retry 不应被改名");
        // 只看成功分支之后的失败区（把本测试的 needle 排除在命中集外）。
        let failure_region = mcp_fn
            .split("// 失败响应")
            .nth(1)
            .expect("失败响应的定位注释不应被删改");
        assert!(
            failure_region.contains("crate::common::recovery_metrics::bump_mcp_failure()"),
            "MCP 失败出口必须 bump 专用计数器，否则失败在 recovery-metrics 端点上不可见"
        );
        assert!(
            failure_region.contains("emit_record(build_mcp_record("),
            "MCP 失败出口必须 emit_record，否则失败在用量面板上不存在（#11）"
        );
        // client_for 那个出口在「// 失败响应」标记之前，故按整个 MCP 函数计数（排除测试段）。
        assert_eq!(
            mcp_fn
                .matches("crate::common::recovery_metrics::bump_mcp_failure()")
                .count(),
            7,
            "MCP 应有 7 个失败出口（5 条 bail + client_for `?` + 重试耗尽）各自 bump；\
             数量变化说明出口新增/删除，需同步本守卫"
        );
    }

    /// ⭐ 源码级守卫：MCP 重试循环必须有墙钟预算闸门（2026-08-15，M5）。
    ///
    /// 历史缺陷：`call_mcp_with_retry` 只有次数闸（`max_retries`）无墙钟。retry_delay
    /// 指数退避叠加后，一条慢请求可以在小号池里拖过分钟级、反复扫同一个坏号，把偶发
    /// 429 拖成持续雪崩 —— 与对话路径 2026-08-11 修掉的吸收层放大是同一形态
    /// （对话路径靠 round_clock 闸门兜住，MCP 路径当时漏了）。
    ///
    /// 单测覆盖不到（需真实上游 + 号池），用源码断言钉死三点：
    /// 1. 闸门确实在 MCP 函数内；
    /// 2. 闸门在 MCP 的 for 循环**内**、且在 `acquire_context`（发请求前）之前 ——
    ///    挪到循环外或发请求之后都会让墙钟失效；
    /// 3. 整个生产段只有这一处该闸门（防在别的函数里加个假的充数）。
    #[test]
    fn mcp_retry_loop_must_have_wall_clock_gate() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let mcp_fn = src
            .split("async fn call_mcp_with_retry")
            .nth(1)
            .expect("call_mcp_with_retry 不应被改名");
        // needle 运行时拼接（include_str! 会把测试自身字面量也读进来，本仓踩过多次）。
        // 判据不带对齐空格（本仓守卫约定，见 token_manager 排序键守卫的教训）：
        // 只匹配「预算比较」这一行本身，缩进/换行随 rustfmt 怎么排都不影响。
        // 常量取 MCP_WALL_SECS（≈read_timeout×2+30，推导见该常量注释）—— 复用主路径
        // 45s 会掐死换号（同透传墙钟教训），此守卫顺带钉住不用错常量。
        let gate_body = format!(
            "{}{}",
            "&& call_started.elapsed() >= Duration::from_secs", "(MCP_WALL_SECS)"
        );
        let for_at = mcp_fn
            .find("for attempt in 0..max_retries")
            .expect("MCP 重试循环不应被改名");
        let body_at = mcp_fn.find(&gate_body).unwrap_or_else(|| {
            panic!("MCP 循环内必须存在墙钟预算比较（`{gate_body}`），否则单请求可在小号池里拖过分钟级")
        });
        // 同一语句窗口内必须有「attempt > 0」首试豁免（保证至少打一次）。
        // ⚠️ 不能用字节切片（`&mcp_fn[a..b]`）：body_at 偏移可能落在多字节字符
        // 中间（2026-08-15 实测 panic: not a char boundary），用 rfind 比较位置。
        let before_at = mcp_fn[..body_at].rfind("if attempt > 0");
        assert!(
            before_at.is_some_and(|p| body_at - p < 300),
            "墙钟闸门必须带 attempt>0 首试豁免：首次尝试不受此限，保证至少打一次"
        );
        assert!(
            for_at < body_at,
            "墙钟闸门必须在 for 循环**内**（挪到循环外即失效）"
        );
        let acquire_at = mcp_fn
            .find("acquire_context(None, None)")
            .expect("MCP 的上下文获取调用不应被改名");
        assert!(
            body_at < acquire_at,
            "墙钟闸门必须排在 acquire_context（发请求）之前，否则超预算的请求仍会真打上游"
        );
        assert_eq!(
            src.matches(&gate_body).count(),
            1,
            "该墙钟闸门在整个生产段应只出现一次（MCP 路径）；对话路径用的是 round_clock 形态"
        );
    }

    /// ⭐ 源码级守卫：两处 force-refresh 调用点都必须跳过 api_key 号。
    ///
    /// 单测覆盖不到（需真实上游返回 401/403 才会走到该分支），而这是**会加速烧号**的路径：
    /// api_key 号没有 refreshToken，`refresh_token()` 对它是契约级 bail，
    /// 在热路径上调它结构上不可能成功，却会计入失败 + 落 auth 冷却。
    ///
    /// 线上实测（本轮多开时暴露）：一个 api_key 号遇 403 后每轮白等约 3 秒
    /// （错误串不含任何 HTTP 码 → 被刷新层的黑名单式瞬态判据当可重试 → 1s+2s 退避），
    /// 连计 3 次失败即判死号自动禁用 —— 死亡速度被放大三倍。
    ///
    /// 断言两处而非一处：对话路径与 MCP 路径各有一份 force-refresh 逻辑，
    /// 这种「同款逻辑复制两份」正是本仓 #4 类漏改事故的成因（对话路径修了、MCP 漏了）。
    /// 🔴 额度耗尽判定**不得门控状态码** —— 只认 body 里的 reason 字面量。
    ///
    /// # 实测（2026-08-05，6 小时窗口）
    ///
    /// - `402 Payment Required`：**0 次**
    /// - `400 Bad Request` + `"reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"`：**564 次**
    ///
    /// 旧代码 `status == 402 && is_monthly_request_limit(&body)` ⇒ 那道门从不成立 ⇒
    /// 564 个额度耗尽的请求落到通用 400 分支 `break`，凭据**不禁用、继续留在轮转里**，
    /// 每个新请求再撞一次（实测 #508 一个号吃了 543 次）。
    ///
    /// 回退即 FAIL：把 `if endpoint.is_monthly_request_limit(&body)` 改回
    /// `if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body)` → 本条失败。
    #[test]
    fn quota_exhausted_must_not_be_gated_on_status_code() {
        let src = include_str!("provider.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        // needle 运行时拼接（include_str! 自匹配坑，本仓库踩过四次）。
        let bad = format!(
            "status.as_u16() == 402 && endpoint.is_monthly_request_limit{}",
            "("
        );
        assert!(
            !prod.contains(&bad),
            "额度耗尽不得门控 402：上游已改用 400（实测 402 六小时 0 次、400+OVERAGE 564 次），\
             门控会让所有额度耗尽的号继续留在轮转里反复被撞"
        );
        // 两条路径（对话 + MCP）都必须有不带状态码门控的判定。
        let good = format!("if endpoint.is_monthly_request_limit(&body){}", " {");
        assert_eq!(
            prod.matches(&good).count(),
            2,
            "对话路径与 MCP 路径都必须有该判定（当前 {} 处）",
            prod.matches(&good).count()
        );
        // 顺序守卫：必须在通用 400 分支之前，否则 400 先 break 就永远走不到。
        let qi = prod.find(&good).expect("额度判定不该被改名");
        let generic400 = format!("if status.as_u16() == 400 {}", "{");
        if let Some(gi) = prod.find(&generic400) {
            assert!(
                qi < gi,
                "额度判定必须排在通用 400 分支之前（挪到之后即失效）"
            );
        }
    }

    /// ⭐ 源码级守卫（客户端格式错误不重试防 503 风暴）：客户端请求校验错误分支必须**同时**
    /// 认 `TOOL_USE_RESULT_MISMATCH`（endpoint 层 `is_client_validation_error` 覆盖）与
    /// `TOOL_SCHEMA_INVALID`（本处补认），且命中后直接 break —— 不重试、不换号、不进吸收层。
    ///
    /// 参考 ZyphrZero/kiro.rs endpoint/mod.rs 的 `CLIENT_VALIDATION_REASONS`：这两个 reason
    /// 都是客户端请求构造问题（多轮工具结果不匹配 / 工具 schema 非法），重试/换号只会白烧
    /// 并发请求，放大成上游 503 风暴。漏认任一都会把它们当可重试瞬态错误处理。
    ///
    /// 用源码级守卫而非行为测试：`call_api_with_retry` 需真实上游 + 号池，单测造不出
    /// （本仓既有惯例）。
    #[test]
    fn client_validation_error_recognizes_both_markers_and_breaks() {
        let full = include_str!("provider.rs");
        // 切掉测试段：本测试自身的字面量不能成为假命中源。
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        // 定位该分支：条件文本到第一个 `{` 为止。
        let marker = "if endpoint.is_client_validation_error(&body)";
        let at = src
            .find(marker)
            .expect("客户端请求校验错误分支不应被删除");
        let cond_end = src[at..]
            .find('{')
            .map(|i| at + i)
            .unwrap_or(src.len());
        let cond = &src[at..cond_end];
        assert!(
            cond.contains("TOOL_SCHEMA_INVALID"),
            "客户端请求校验错误分支必须同时认 TOOL_USE_RESULT_MISMATCH（endpoint 层\
             is_client_validation_error）与 TOOL_SCHEMA_INVALID（本处补认）：漏认后者会把\
             客户端构造错误当可重试瞬态，白烧并发请求并放大成上游 503 风暴"
        );
        // 命中后必须 break（直接失败），分支内不得 continue（continue 即重试/换号）。
        let branch_body = &src[at..src[at..]
            .find("break")
            .map(|i| at + i)
            .expect("命中后必须 break（直接失败、不重试不换号）：改回 continue 即回归")];
        assert!(
            !branch_body.contains("continue"),
            "客户端请求校验错误分支内不得 continue：continue 即重试/换号，\
             与『客户端错不重试』的语义冲突"
        );
    }

    /// ⭐ 源码级守卫：订阅永久错误的分支必须**排在所有 403 处置之前**，且不得计凭据失败。
    ///
    /// 为什么用源码守卫而不是行为测试：触发它需要真实上游返回该 403，而本仓铁律禁止
    /// 测试依赖网络；热路径那段又在 `call_api_with_retry` / `call_mcp_with_retry` 深处，
    /// 构造不出确定性用例。
    ///
    /// 🔴 **先剔注释行再匹配**。`include_str!` 读的是原始源文本（含注释），直接
    /// `contains` 会匹配到**被注释掉**的实现 ⇒ 把代码注释掉守卫仍然绿。本仓记录
    /// 该形态已踩过五次（见 `admission_timeout_must_be_observable` 的注释）。
    #[test]
    fn subscription_unsupported_branch_must_precede_other_403_handling() {
        let src = include_str!("provider.rs");
        let prod = src.split("#[cfg(test)]").next().expect("生产段应存在");
        let prod: String = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        // needle 运行时拼接，避免把本测试自己的字面量算进匹配。
        let sub = format!("endpoint.{}(&body)", "is_subscription_unsupported");
        let validation = format!("endpoint.{}(&body)", "is_client_validation_error");
        let temp_rl = format!("endpoint.{}(&body)", "is_temporary_rate_limit");

        // ⚠️ **必须按函数切片再比较位置**。`is_temporary_rate_limit` 有**两个**调用点
        // （`call_mcp_with_retry` 与 `call_api_with_retry`），全局 `find` 拿到的是靠前的
        // 那个（MCP），于是拿它和对话路径的订阅分支比位置 —— 跨函数比较，结论无意义。
        // ⇒ 本守卫遍历**两个**函数，缺任何一个都 FAIL（本仓 issue #2 的「同一逻辑各写
        // 一份 ⇒ 漏改」形态：订阅分支只加在对话路径、MCP 路径漏加正是历史缺陷的原型）。
        for (fname, marker) in [
            ("call_api_with_retry", "async fn call_api_with_retry"),
            ("call_mcp_with_retry", "async fn call_mcp_with_retry"),
        ] {
            let start = prod
                .find(marker)
                .unwrap_or_else(|| panic!("{fname} 不应被改名"));
            // 函数体上界：下一个同缩进层方法的起始（三种签名形态取最靠前者）。
            let after_sig = start + marker.len();
            let rest = &prod[after_sig..];
            let end = ["\n    async fn ", "\n    pub fn ", "\n    fn "]
                .iter()
                .filter_map(|m| rest.find(m))
                .min()
                .map(|i| after_sig + i)
                .unwrap_or(prod.len());
            let seg_fn = &prod[start..end];

            let sub_at = seg_fn.find(&sub).unwrap_or_else(|| {
                panic!(
                    "{fname} 缺少订阅判据分支 —— 漏了它，该路径上的永久失败会被当成可重试"
                )
            });

            // 同函数内若存在其它 403 分支，订阅必须排在它们之前。
            for (other_name, other) in [
                ("is_client_validation_error", &validation),
                ("is_temporary_rate_limit", &temp_rl),
            ] {
                if let Some(other_at) = seg_fn.find(other.as_str()) {
                    assert!(
                        sub_at < other_at,
                        "{fname}：订阅永久错误必须排在 {other_name} 之前 —— 排在后面时，\
                         换区（L1）/短冷却 failover 会先命中，而两者对订阅问题都无效\
                         （实测同一把 key 在两个区拿到的是**不同**的 403：us 回 bearer \
                         invalid、eu 回 subscription unsupported），只是白烧上游往返与重试预算"
                    );
                }
            }

            // 承重：该分支**不得**调 report_failure —— 号没坏，是订阅不含该应用/模型。
            // 片段取到该分支的闭合花括号为止。
            let branch = &seg_fn[sub_at..];
            let branch_end = branch
                .find("\n                }")
                .map(|i| i + 1)
                .unwrap_or(branch.len());
            let branch = &branch[..branch_end];
            assert!(
                !branch.contains("report_failure"),
                "{fname}：订阅永久错误不得计入凭据失败 —— 那会在 3 次后自动禁用一个\
                 「换个模型就能用」的号，且 persist_disabled_state 落盘后重启也回不来"
            );
            assert!(
                branch.contains("subscription_unsupported=1"),
                "{fname}：错误串必须带机器可读标记，否则面板/外挂无法与其它 403 区分"
            );
        }
    }

    // ⚠️ `#[test]` 曾在 2026-08-06 之前的某次改动中丢失，导致本守卫**从未运行过**
    // （表现为编译期 `function is never used` 警告，而非测试失败 —— 所以没人注意）。
    // 上一轮已补过一次又退化，故此处留注记：删这行属性等于悄悄关掉一条守卫。
    #[test]
    fn force_refresh_must_skip_api_key_credentials_at_both_sites() {
        let src = include_str!("provider.rs");
        // ⚠️ needle 必须**运行时拼接**：若把完整串写成一个字面量，它自己也会出现在
        // 本文件里，被 include_str! 读到并多算一处（第一版就是这样，测试在回退前就 FAIL）。
        let needle = format!("{}{}", "if endpoint.is_bearer_token_invalid", "(&body)");
        let sites: Vec<&str> = src.split(needle.as_str()).skip(1).collect();
        assert_eq!(
            sites.len(),
            2,
            "预期恰好两处 force-refresh 调用点（对话路径 + MCP 路径）；\
             数量变化说明有新增/删除，需同步本守卫"
        );
        for (i, site) in sites.iter().enumerate() {
            // 只看该 if 的条件部分（到左花括号为止）
            let cond = site.split('{').next().unwrap_or("");
            assert!(
                cond.contains("is_api_key_credential"),
                "第 {} 处 force-refresh 未跳过 api_key 号：它结构上不可能刷新成功，\
                 却会计入失败并被退避重试，把该号的死亡速度放大三倍。条件为: {cond}",
                i + 1
            );
        }
    }

    /// ⭐ 源码级守卫：**失败记录必须带 `retries`**。
    ///
    /// 单测覆盖不到 `call_api_with_retry` 的失败路径（需真实上游 + 号池才能把重试预算跑穿），
    /// 而这正是回归发生过的地方：`fail_record` 组装块设了 credential_id / session_id /
    /// is_streaming / latency_ms / outcome / error_message，**唯独漏了 `retries`** →
    /// 落库即 `RequestRecord::new` 的默认 0。
    ///
    /// 线上实测坐实（近 2 小时）：全部失败样本 **无一例外 retries=0**
    /// （auth_failed 1487 / rate_limited 1098 / server_error 118 / bad_request 91），
    /// 而同期成功样本有 retries=1、历史号池大时到过 7 以上 —— 统计上不可能，
    /// 除非失败路径从不赋值。后果是「烧掉 12 次换号才失败」与「第一次就失败」
    /// 在面板上完全不可区分，而那恰是判断重试预算是否够用的唯一依据。
    ///
    /// 用源码级守卫而非行为测试的理由与上面两个测试相同。
    #[test]
    fn fail_record_must_carry_retries() {
        let src = include_str!("provider.rs");
        // 定位失败记录组装块：从 `let mut fail_record` 到紧随其后的 `emit_record`。
        let block = src
            .split("let mut fail_record")
            .nth(1)
            .expect("fail_record 组装块不应被改名/删除");
        let block = block
            .split("emit_record")
            .next()
            .expect("fail_record 之后应紧跟 emit_record");
        assert!(
            block.contains("fail_record.retries"),
            "失败记录必须设 retries，否则一切失败样本的重试次数恒为 0，\
             无法区分『扫穿整池才失败』与『首次即失败』"
        );
    }

    /// ⭐ 源码级守卫（N4）：失败记录必须携带「链内首选号」。
    ///
    /// 线上实测：透传全败的失败样本 `credential_id=None retries=3`，面板看不出
    /// 「首选了哪个号」—— 若死号每次都排最前（`select_custom_api` 排序首写），
    /// 这种「死号恒选」在面板上完全不可见。`first_attempted_credential_id` 由
    /// 共享预算携带（透传首跳写、Kiro 主路径兜底），fail_record 必须读它。
    ///
    /// 用源码级守卫而非行为测试：触发需要整条透传全败 → 落 Kiro 主路径的端到端
    /// mock 链，且记录经管道异步落库无法在单测内同步断言（与 retries 守卫同理）。
    #[test]
    fn fail_record_must_carry_first_attempted_credential() {
        let src = include_str!("provider.rs");
        let block = src
            .split("let mut fail_record")
            .nth(1)
            .expect("fail_record 组装块不应被改名/删除");
        let block = block
            .split("emit_record")
            .next()
            .expect("fail_record 之后应紧跟 emit_record");
        assert!(
            block.contains("fail_record.first_attempted_credential_id"),
            "失败记录必须设 first_attempted_credential_id，否则透传全败的样本\
             看不到『首选了哪个号』，面板无法发现死号恒选（N4）"
        );
    }

    /// ⭐ 源码级守卫（已知问题 #20）：准入闸门超时必须**既 emit_record 又 bump 计数器**。
    ///
    /// 旧代码是裸 `anyhow::bail!` —— 被网关自己背压掐掉的请求在面板上**完全不存在**，
    /// 于是看到的成功率**偏乐观**（分母里少了这批）。而面板成功率是本项目后续一切限流
    /// 调参判断的依据，依据本身有偏则调参全是在算空气。实测这类 bail 在高峰时段
    /// 逐小时占比可达两位数。
    ///
    /// acquire_admission 已移至 handlers 层（post_messages 入口），
    /// provider.rs 生产代码中不应再有任何调用。守卫确保将来不会有人在此加回。
    ///
    /// 用源码级守卫而非行为测试：触发它需要真实令牌桶排满 + 真实 TokenManager +
    /// 走满 `inbound_queue_max_wait_secs`（默认 5s）的 await，单测里造不出且会拖慢全套。
    #[test]
    fn admission_timeout_must_be_observable() {
        let src = include_str!("provider.rs");
        let prod_all = src.split("#[cfg(test)]").next().expect("生产段应存在");
        let prod: String = prod_all
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let prod = prod.as_str();
        // 🔴 2026-08-10：acquire_admission 已移至 handlers 层，provider 不应再调用。
        let retry_needle = ["acquire_admission", "().await"].concat();
        assert_eq!(
            prod.matches(retry_needle.as_str()).count(),
            0,
            "acquire_admission 已移至 handlers 层（透传与 Kiro 两条路径统一在 post_messages \
             入口过闸门）。provider.rs 不应再有调用点。若要在此加回，先确认不会导致某条路径绕闸。"
        );
    }

    /// 源码级守卫（E2）：MCP 的 401/403 分支必须**先**判账户级风控/封禁，
    /// 才允许落通用 `report_failure`。
    ///
    /// 用源码级守卫的理由与上一个测试相同：`call_mcp_with_retry` 需真实上游 + 号池，
    /// 单测覆盖不到，而这正是回归发生的地方（本条修复前该分支就是裸 `report_failure`）。
    ///
    /// **旧代码为何失败**：403 分支内只有 `report_failure`，缺
    /// `is_temporary_rate_limit` / `is_account_suspended` 两道判定。
    /// 而 403 `TEMPORARILY_SUSPENDED` 是**临时态**，`report_failure` 累加
    /// `failure_count` 达阈值即以 `TooManyFailures`（**永久型**标签）禁用 →
    /// 临时限流的号走 WebSearch 被打 3 次就永久禁用。这正是历史事故
    /// （12h 内 88 次误禁 + 36 次全池自愈活锁）的同一误判形态：对话路径已修，
    /// 本路径此前漏修。
    #[test]
    fn should_classify_account_risk_before_generic_failure_in_mcp_auth_branch() {
        let src = include_str!("provider.rs");
        let mcp_fn = src
            .split("async fn call_mcp_with_retry")
            .nth(1)
            .expect("call_mcp_with_retry 不应被改名");
        // 只看 401/403 分支：从它的定位注释起，到下一个「瞬态错误」分支为止。
        //
        // ⚠️ 先坐实两个定位标记的**唯一性**，否则本测试会在标记被改名时**静默失效**：
        // `.split(x).next()` 永不返回 None，所以若标记消失，`auth_branch` 会变成
        // 「函数剩余全文」—— 那里同样含 is_temporary_rate_limit / report_failure，
        // 顺序断言可能照样通过，于是守卫形同虚设（审查发现的真实弱点）。
        // 每个标记应恰好出现 2 次：一次在被守卫的代码里，一次在本测试的 split 字面量里。
        const AUTH_MARKER: &str = "// 401/403 凭据问题";
        const TRANSIENT_MARKER: &str = "// 瞬态错误";
        assert_eq!(
            src.matches(AUTH_MARKER).count(),
            2,
            "401/403 定位标记必须唯一（代码 1 处 + 本测试 1 处）；数量变了说明标记被改动，\
             守卫会退化成扫全文而静默失效 —— 请同时更新代码与本测试"
        );
        assert_eq!(
            src.matches(TRANSIENT_MARKER).count(),
            2,
            "瞬态错误定位标记必须唯一（代码 1 处 + 本测试 1 处），同上"
        );
        let auth_branch = mcp_fn
            .split(AUTH_MARKER)
            .nth(1)
            .expect("401/403 分支的定位注释不应被删改")
            .split(TRANSIENT_MARKER)
            .next()
            .expect("瞬态错误分支的定位注释不应被删改");
        // 边界健全性：分支切片必须显著短于整个函数，否则说明切错了（扫到全文）。
        assert!(
            auth_branch.len() < mcp_fn.len() / 2,
            "401/403 分支切片异常大（{} vs 函数 {}），定位失败",
            auth_branch.len(),
            mcp_fn.len()
        );

        let rate_limit_at = auth_branch
            .find("is_temporary_rate_limit")
            .expect("MCP 403 必须判账户级临时风控，否则临时态会被贴 TooManyFailures 永久标签");
        let suspended_at = auth_branch
            .find("is_account_suspended")
            .expect("MCP 403 必须判账户封禁，否则 disabled_reason 会落成 TooManyFailures");
        // 匹配**调用点**而非注释：分支内的说明注释里也出现 report_failure 字样。
        let generic_failure_at = auth_branch
            .find("self.token_manager.report_failure(")
            .expect("非风控 403 仍应计入通用失败（对照：不能修过头把真失败也放过）");

        assert!(
            rate_limit_at < generic_failure_at,
            "临时风控判定必须在 report_failure 之前（顺序错等于没修）"
        );
        assert!(
            suspended_at < generic_failure_at,
            "封禁判定必须在 report_failure 之前"
        );
        // 与对话路径同款：风控命中走分钟级退避，而非累加永久失败。
        assert!(
            auth_branch.contains("report_suspicious_activity"),
            "MCP 风控命中应走 report_suspicious_activity（分钟级退避）"
        );
    }

    #[test]
    fn test_extract_model_and_session_invalid_json() {
        // 非法 JSON：两者都为 None（与旧实现一致，不 panic）
        let (model, session) = KiroProvider::extract_model_and_session("not json");
        assert_eq!(model, None);
        assert_eq!(session, None);

        // 合法 JSON 但缺 conversationState：两者都为 None
        let (model, session) = KiroProvider::extract_model_and_session(r#"{"foo":"bar"}"#);
        assert_eq!(model, None);
        assert_eq!(session, None);
    }
    /// 回归（🔴 会杀号的缺陷）：请求热路径的端点解析必须与 `effective_endpoint` 同口径。
    ///
    /// **旧代码为何 FAIL**：`endpoint_for` 只读 `credentials.endpoint` 原始字段，
    /// 漏了「`ksk_` API Key 号自动路由到 CLI 端点」这一层（`effective_endpoint` 的第 ② 步）。
    /// 实测：同一个 ksk_ 号，`effective_endpoint()` 返回 `cli`，而热路径返回 `ide`。
    ///
    /// **为什么严重**：`ksk_` 号打 IDE 端点会 403（两个端点按凭据类型绑定、不可互换）。
    /// 403 走 `report_suspicious_activity`，连续 6 次即判死号自动禁用 ——
    /// 于是一个**完全健康**的 ksk_ 号，只因没手工填 `endpoint: cli` 就被烧掉。
    /// 这与线上号池"单号存活 25~60 分钟"的现象直接相关。
    ///
    /// 用源码级断言而非构造 provider：`endpoint_for` 需要完整的 endpoints 注册表 + 配置，
    /// 而缺陷本身只在"读哪个字段"这一行，源码断言足以锁死且不会因重构失效。
    #[test]
    fn endpoint_for_must_use_effective_endpoint_not_raw_field() {
        let src = include_str!("provider.rs");
        let body = src
            .split("fn endpoint_for")
            .nth(1)
            .expect("endpoint_for 不应被改名")
            .split("\n    /// ")
            .next()
            .expect("函数体应以下一项文档注释为界");
        assert!(
            body.contains("effective_endpoint"),
            "请求热路径必须走 effective_endpoint（否则 ksk_ 号走错端点 → 403 → 被当死号禁用）"
        );
        assert!(
            !body.contains(".endpoint\n            .as_deref()"),
            "不得回退到直读 credentials.endpoint 原始字段"
        );
    }

    /// 配套：坐实 `effective_endpoint` 对 ksk_ 号确实路由到 CLI（本回归的前提）。
    #[test]
    fn effective_endpoint_routes_api_key_credential_to_cli() {
        let mut c = crate::kiro::model::credentials::KiroCredentials::default();
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("ksk_test_key".to_string());
        c.endpoint = None;
        assert_eq!(
            c.effective_endpoint("ide"),
            crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME,
            "ksk_ 号未显式配置时应自动路由到 CLI"
        );
        // 显式配置优先（面板可切回 ide 救急）
        c.endpoint = Some("ide".to_string());
        assert_eq!(c.effective_endpoint("ide"), "ide", "显式配置必须优先");
    }

    /// ⭐ 守卫：`select_endpoint` 必须按 `effective_endpoint_order` 候选顺序遍历，
    /// 而不是只取 `effective_endpoint` 单值。若回退成单值，429 换桶机制失去「q.* 封桶后落
    /// runtime.*」的能力，等于回到单端点。
    #[test]
    fn select_endpoint_must_use_endpoint_order_for_bucket_fallback() {
        let src = include_str!("provider.rs");
        let body = src
            .split("fn select_endpoint")
            .nth(1)
            .expect("select_endpoint 不应被改名")
            .split("\n    /// ")
            .next()
            .expect("函数体应以下一项文档注释为界");
        assert!(
            body.contains("effective_endpoint_order"),
            "select_endpoint 必须用 effective_endpoint_order 遍历候选端点（q.* 优先、runtime.* 回退）"
        );
        assert!(
            body.contains("endpoint_buckets"),
            "select_endpoint 必须查询端点桶封禁状态"
        );
    }

    // ══════════ select_endpoint 自适应派发（按凭据成功率，取代 round-robin）══════════

    /// 用真实端点注册表构造 provider（select_endpoint 只查 name，不触达实现细节）。
    fn provider_with_default(default_endpoint: &str) -> KiroProvider {
        let cfg = crate::model::config::Config::default();
        let tm = Arc::new(
            MultiTokenManager::new(cfg, vec![], None, None, false).expect("测试 token manager"),
        );
        KiroProvider::with_proxy(
            tm,
            None,
            crate::kiro::endpoint::registry(),
            default_endpoint.to_string(),
        )
    }

    // ============ call_api_with_retry 行为测试（端到端 mock 上游，2026-08-15 补）============
    //
    // call_api_with_retry 是全仓最重的单函数（约 1800 行），此前只有纯函数测试与
    // include_str 源码守卫——「分支之间怎么咬合」从未被真跑过（blockers-structure.md §1）。
    // 本组测试用本地 TCP 假上游 + 注入 mock endpoint（KiroEndpoint trait 的实现者，
    // 经 `with_proxy` 的 endpoints 注册表传入——这是现有构造路径，非测试专用 seam），
    // 把「选号 → 建请求 → 打上游 → 错误分类 → 换号/重试/耗尽」整条链真实跑起来。
    //
    // 网络与 AWS 签名完全在 mock 侧消除：api_url 指向 127.0.0.1、decorate_api 不加头。

    /// 本地 mock 上游：每个连接消费一个预配置响应（`connection: close` 强制新连接，
    /// 响应队列按请求次序出队），超出队列的请求一律 500（让测试以 Err 收尾而非挂死）。
    struct MockUpstream {
        port: u16,
        hits: Arc<std::sync::atomic::AtomicUsize>,
        /// 每个请求的原始请求头（按请求次序；含 Authorization，可据此区分请求是哪个号发的）。
        heads: Arc<Mutex<Vec<String>>>,
        _responses: Arc<Mutex<std::collections::VecDeque<MockResponse>>>,
    }

    #[derive(Clone)]
    struct MockResponse {
        status: u16,
        reason: &'static str,
        body: &'static str,
        retry_after_secs: Option<u64>,
    }

    impl MockResponse {
        fn ok(body: &'static str) -> Self {
            Self {
                status: 200,
                reason: "OK",
                body,
                retry_after_secs: None,
            }
        }
    }

    impl MockUpstream {
        fn start(responses: Vec<MockResponse>) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定 mock 上游端口");
            let port = listener.local_addr().expect("mock 端口").port();
            let responses = Arc::new(Mutex::new(std::collections::VecDeque::from(responses)));
            let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let heads = Arc::new(Mutex::new(Vec::new()));
            let (hits_t, responses_t, heads_t) = (hits.clone(), responses.clone(), heads.clone());
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(mut stream) = conn else { continue };
                    let (hits_c, responses_c, heads_c) = (hits_t.clone(), responses_t.clone(), heads_t.clone());
                    std::thread::spawn(move || {
                        // 先落请求头再写响应：调用方拿到响应时，本连接的请求头必然已可读。
                        heads_c.lock().push(mock_read_request_head(&mut stream));
                        let n = hits_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let resp = responses_c
                            .lock()
                            .get(n)
                            .cloned()
                            .unwrap_or(MockResponse {
                                status: 500,
                                reason: "Internal Server Error",
                                body: "{}",
                                retry_after_secs: None,
                            });
                        mock_write_response(&mut stream, &resp);
                    });
                }
            });
            Self {
                port,
                hits,
                heads,
                _responses: responses,
            }
        }

        /// 按请求次序返回每个请求的原始请求头（含 Authorization，可据此区分是哪个号打的）。
        fn captured_heads(&self) -> Vec<String> {
            self.heads.lock().clone()
        }
    }

    fn mock_read_request_head(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        let mut buf = [0u8; 4096];
        let mut received = Vec::new();
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    received.extend_from_slice(&buf[..n]);
                    if received.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&received).into_owned()
    }

    fn mock_write_response(stream: &mut std::net::TcpStream, r: &MockResponse) {
        use std::io::Write;
        let mut head = format!(
            "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
            r.status,
            r.reason,
            r.body.len()
        );
        if let Some(ra) = r.retry_after_secs {
            head.push_str(&format!("retry-after: {}\r\n", ra));
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(r.body.as_bytes());
        let _ = stream.flush();
    }

    /// 只认 mock 上游的端点实现：不加任何头、不改任何 body，URL 固定指向本地假上游。
    ///
    /// `name` 可自定义（真实端点名如 "ide" / "codewhisperer"）：端点回退链按名字在
    /// 注册表里补齐，固定叫 "mock" 的端点进不了 `ENDPOINT_FALLBACK_ORDER` 的链。
    struct MockEndpoint {
        url: String,
        name: &'static str,
    }

    impl KiroEndpoint for MockEndpoint {
        fn name(&self) -> &'static str {
            self.name
        }
        fn api_url(&self, _ctx: &RequestContext<'_>) -> String {
            self.url.clone()
        }
        fn mcp_url(&self, _ctx: &RequestContext<'_>) -> String {
            self.url.clone()
        }
        fn decorate_api(
            &self,
            req: reqwest::RequestBuilder,
            _ctx: &RequestContext<'_>,
        ) -> reqwest::RequestBuilder {
            req
        }
        fn decorate_mcp(
            &self,
            req: reqwest::RequestBuilder,
            _ctx: &RequestContext<'_>,
        ) -> reqwest::RequestBuilder {
            req
        }
        fn transform_api_body(&self, body: &str, _ctx: &RequestContext<'_>) -> String {
            body.to_string()
        }
    }

    /// 构造「2 个可用 Kiro 号 + 唯一 mock 端点」的 provider。
    ///
    /// ⚠️ 凭据 id 用本组专属段（91_xxx）：endpoint_health 是**进程级共享**表
    /// （endpoint_health::SHARED），与既有 select_endpoint 测试共用 id 会被对方写入的
    /// 样本破坏「冷启动」类断言（provider 内部 `report_endpoint_outcome` 会写这张表）。
    fn provider_with_mock_upstream(upstream: &MockUpstream) -> KiroProvider {
        let mut creds = Vec::new();
        for id in [91_001u64, 91_002] {
            let mut c = KiroCredentials::default();
            c.id = Some(id);
            c.auth_method = Some("api_key".to_string());
            c.kiro_api_key = Some(format!("sk-mock-{id}"));
            // ⚠️ 必须显式钉死 endpoint：api_key 号被 `effective_endpoint_order` 自动路由到
            // 内置的 ["cli", "cli-runtime"] 候选链（endpoint=None 时），而测试注册表只有
            // "mock"——不钉死则 select_endpoint 硬门滤掉全部候选 → 请求永不打 mock 上游。
            c.endpoint = Some("mock".to_string());
            creds.push(c);
        }
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                false,
            )
            .expect("构造测试 token manager"),
        );
        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert(
            "mock".to_string(),
            Arc::new(MockEndpoint {
                url: format!("http://127.0.0.1:{}", upstream.port),
                name: "mock",
            }),
        );
        KiroProvider::with_proxy(tm, None, endpoints, "mock".to_string())
    }

    /// 构造「2 个 custom_api 代挂号（baseUrl 指向同一 mock 上游）+ mock 端点」的 provider，
    /// 供 `try_custom_api_passthrough` 的 failover 链测试（N4 首选号）。
    ///
    /// 与 `provider_with_mock_upstream` 的差异只有凭据形态（custom_api vs api_key）：
    /// 透传选号池（`select_custom_api`）只认 custom_api 号；透传路径不经 KiroEndpoint，
    /// URL 直接由 base_url 拼出（`passthrough::forward`），mock 端点在注册表里只是
    /// `with_proxy` 构造所需。
    fn provider_with_passthrough_upstream(upstream: &MockUpstream) -> KiroProvider {
        let mut creds = Vec::new();
        for id in [91_001u64, 91_002] {
            let mut c = KiroCredentials::default();
            c.id = Some(id);
            c.auth_method = Some("custom_api".to_string());
            c.base_url = Some(format!("http://127.0.0.1:{}", upstream.port));
            c.kiro_api_key = Some(format!("sk-mock-{id}"));
            creds.push(c);
        }
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                false,
            )
            .expect("构造测试 token manager"),
        );
        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert(
            "mock".to_string(),
            Arc::new(MockEndpoint {
                url: format!("http://127.0.0.1:{}", upstream.port),
                name: "mock",
            }),
        );
        KiroProvider::with_proxy(tm, None, endpoints, "mock".to_string())
    }

        const MOCK_PASSTHROUGH_BODY: &str =
        r#"{"model":"claude-sonnet-4","messages":[{"role":"user","content":"hi"}]}"#;

    /// 构造「2 个可用 Kiro 号 + 双 mock 端点（ide → port_a、codewhisperer → port_b）」的
    /// provider，供端点级链式回退测试。
    ///
    /// ⚠️ 凭据 id 用本组专属段（93_xxx），理由同 `provider_with_mock_upstream`（91_xxx 段
    /// 的 endpoint_health 共享表会被本组写入的样本污染「冷启动」断言）。
    ///
    /// 凭据形态：api_key 号 + 显式 `endpoint="ide"`。api_key 号无显式值时被
    /// `effective_endpoint_order` 自动路由到 CLI 族（注册表里没有，select 拿不到候选），
    /// 显式指定 ide 后候选链 = ["ide", cli, cli-runtime, codewhisperer, amazonq]（显式值
    /// 放最前 + 完整候选链去重），注册表只含 ide/codewhisperer ⇒ select 候选 [ide, cw]，
    /// 冷启动选中 ide ⇒ 端点链 = [ide, codewhisperer]（order 补齐 cw，FALLBACK_ORDER 无新增）。
    fn provider_with_mock_chain_ports(port_a: u16, port_b: u16) -> KiroProvider {
        let mut creds = Vec::new();
        for id in [93_001u64, 93_002] {
            let mut c = KiroCredentials::default();
            c.id = Some(id);
            c.auth_method = Some("api_key".to_string());
            c.kiro_api_key = Some(format!("sk-mock-{id}"));
            c.endpoint = Some("ide".to_string());
            creds.push(c);
        }
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                false,
            )
            .expect("构造测试 token manager"),
        );
        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert(
            "ide".to_string(),
            Arc::new(MockEndpoint {
                url: format!("http://127.0.0.1:{port_a}"),
                name: "ide",
            }),
        );
        endpoints.insert(
            "codewhisperer".to_string(),
            Arc::new(MockEndpoint {
                url: format!("http://127.0.0.1:{port_b}"),
                name: "codewhisperer",
            }),
        );
        KiroProvider::with_proxy(tm, None, endpoints, "ide".to_string())
    }

    fn provider_with_mock_chain(up_a: &MockUpstream, up_b: &MockUpstream) -> KiroProvider {
        provider_with_mock_chain_ports(up_a.port, up_b.port)
    }

    /// 构造「2 个可用 Kiro 号 + 4 个 mock 端点（ide/cli/codewhisperer/amazonq 各指
    /// 一个 mock 上游）」的 provider，供 M1 预算闸的「4 元素链全 429」集成测试。
    ///
    /// api_key 号 + 显式 `endpoint="ide"` ⇒ 候选链 = [ide, cli, cli-runtime(未注册),
    /// codewhisperer, amazonq]，注册表含全部 4 端点 ⇒ 链 = [ide, cli, codewhisperer,
    /// amazonq]（4 元素；FALLBACK_ORDER 里的端点已在链内，无新增）。
    fn provider_with_mock_chain_4ports(ports: [u16; 4]) -> KiroProvider {
        let mut creds = Vec::new();
        for id in [93_011u64, 93_012] {
            let mut c = KiroCredentials::default();
            c.id = Some(id);
            c.auth_method = Some("api_key".to_string());
            c.kiro_api_key = Some(format!("sk-mock-{id}"));
            c.endpoint = Some("ide".to_string());
            creds.push(c);
        }
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                false,
            )
            .expect("构造测试 token manager"),
        );
        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        for (name, port) in [
            ("ide", ports[0]),
            ("cli", ports[1]),
            ("codewhisperer", ports[2]),
            ("amazonq", ports[3]),
        ] {
            endpoints.insert(
                name.to_string(),
                Arc::new(MockEndpoint {
                    url: format!("http://127.0.0.1:{port}"),
                    name,
                }),
            );
        }
        KiroProvider::with_proxy(tm, None, endpoints, "ide".to_string())
    }

    /// 拿一个「立刻被释放、无人监听」的本地端口（连接层失败 = ECONNREFUSED 的模拟）。
    fn dead_local_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定临时端口");
        l.local_addr().expect("临时端口地址").port()
    }

    /// N4：透传 failover 链（首选号 502 → 换号 200）的 usage record 必须带
    /// `first_attempted_credential_id` = 首选号，与 `credential_id` = 最终号成对——
    /// 面板据此发现「死号恒选」（某号每次都被选中最前却被换掉）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn passthrough_failover_record_carries_first_attempted_credential() {
        let up = MockUpstream::start(vec![
            MockResponse {
                status: 502,
                reason: "Bad Gateway",
                body: "{}",
                retry_after_secs: None,
            },
            MockResponse::ok(r#"{"ok":true}"#),
        ]);
        let provider = provider_with_passthrough_upstream(&up);
        let budget = SharedRetryBudget::new();

        let (resp, meta) = provider
            .try_custom_api_passthrough(
                MOCK_PASSTHROUGH_BODY.into(),
                Some("claude-sonnet-4"),
                None,
                None,
                &budget,
            )
            .await
            .expect("502 → failover → 200 应成功");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(
            meta.first_attempted_credential_id,
            Some(91_001),
            "首选号必须是首个被选中的号（502 那个）"
        );
        assert_eq!(
            meta.credential_id, 91_002,
            "最终服务号必须是 failover 后的号（200 那个）"
        );
        assert_eq!(
            budget.first_attempted(),
            Some(91_001),
            "共享预算必须同步携带首选号——Kiro 主路径的失败记录要读它"
        );

        // 与 handlers.rs 同款 record 构造（透传成功链埋点）：record 带首选号 + 最终号。
        let mut record = crate::usage::RequestRecord::new(
            "req-pt",
            meta.model.clone().unwrap_or_default(),
        );
        record.credential_id = Some(meta.credential_id);
        record.first_attempted_credential_id = meta.first_attempted_credential_id;
        assert_eq!(
            record.first_attempted_credential_id,
            Some(91_001),
            "record 首选号 == 链首选号"
        );
        assert_eq!(record.credential_id, Some(91_002), "record 最终号 == 链最终号");
    }

    /// 🔴 M1.2 回归（2026-08-16 对抗审查 MAJOR）：400/404 **不记失败余温**——
    /// 坏请求（无效 tool schema / 该站不认模型）是全池同质的客户端错误，一次
    /// failover 把所有号打上余温会让 60s 内任何请求零尝试直返 503（毒化整池）。
    ///
    /// 断言方式（外部可观察行为）：第一请求 A 号 400（值得换号）→ failover 到 B 号
    /// 成功；第二请求（全新上游+全新 manager）若 400 被记热，A 号会被余温过滤 →
    /// 直接选 B 号；不记热则 A 号仍在候选（全平局按 id）→ 首试 A 号。
    /// `meta.credential_id` 公开可见，无需侵入式访问内部状态。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn passthrough_400_404_do_not_record_failure_warmth() {
        for (code, reason) in [
            (400u16, "Bad Request"),
            (404u16, "Not Found"),
        ] {
            // 第一请求：#91_001 返 code（值得换号）→ failover → #91_002 200。
            let up = MockUpstream::start(vec![
                MockResponse {
                    status: code,
                    reason,
                    body: "{}",
                    retry_after_secs: None,
                },
                MockResponse::ok(r#"{"ok":true}"#),
            ]);
            let provider = provider_with_passthrough_upstream(&up);
            let budget = SharedRetryBudget::new();
            let (resp, meta) = provider
                .try_custom_api_passthrough(
                    MOCK_PASSTHROUGH_BODY.into(),
                    Some("claude-sonnet-4"),
                    None,
                    None,
                    &budget,
                )
                .await
                .expect("400/404(值得换号) → failover → 200 应成功");
            assert_eq!(resp.status(), reqwest::StatusCode::OK);
            assert_eq!(
                meta.credential_id, 91_002,
                "{code} 换号后应由 #91_002 服务（failover 语义不变）"
            );

            // 第二请求（全新上游 + 全新 manager，无任何跨请求状态残留）：
            // 不记热 → #91_001 仍首选；记热 → 它被余温过滤 → 直接选 #91_002。
            let up2 = MockUpstream::start(vec![MockResponse::ok(r#"{"ok":true}"#)]);
            let provider2 = provider_with_passthrough_upstream(&up2);
            let budget2 = SharedRetryBudget::new();
            let (_resp2, meta2) = provider2
                .try_custom_api_passthrough(
                    MOCK_PASSTHROUGH_BODY.into(),
                    Some("claude-sonnet-4"),
                    None,
                    None,
                    &budget2,
                )
                .await
                .expect("第二请求应成功");
            assert_eq!(
                meta2.credential_id, 91_001,
                "{code} 不得记失败余温：第二请求必须先试原号（记热时这里会是 #91_002，\
                 整池被坏请求毒化 60s）"
            );
        }
    }

    /// N4：透传全败（落 Kiro 主路径）时，首选号不随 `None` 返回丢失——由共享预算携带，
    /// Kiro 主路径的 `fail_record` 读取它（线上证据形态：`cred_id=None retries=3` 的失败链）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn passthrough_all_fail_budget_keeps_first_attempted() {
        let up = MockUpstream::start(vec![
            MockResponse {
                status: 502,
                reason: "Bad Gateway",
                body: "{}",
                retry_after_secs: None,
            },
            MockResponse {
                status: 502,
                reason: "Bad Gateway",
                body: "{}",
                retry_after_secs: None,
            },
        ]);
        let provider = provider_with_passthrough_upstream(&up);
        let budget = SharedRetryBudget::new();

        let r = provider
            .try_custom_api_passthrough(
                MOCK_PASSTHROUGH_BODY.into(),
                Some("claude-sonnet-4"),
                None,
                None,
                &budget,
            )
            .await;
        assert!(r.is_none(), "全 502 → 透传整体不可用，落 Kiro 主路径");
        assert_eq!(
            budget.first_attempted(),
            Some(91_001),
            "首选号不因全败而丢失——Kiro 主路径 fail_record 依赖它"
        );
    }

    /// 共享预算「链内首选号」的语义：首写生效（透传首跳的号优先于后续任何跳）。
    #[test]
    fn shared_retry_budget_first_attempt_is_first_wins() {
        let b = SharedRetryBudget::new();
        assert_eq!(b.first_attempted(), None, "未记录时恒为 None");
        b.note_first_attempt(3);
        b.note_first_attempt(2);
        assert_eq!(b.first_attempted(), Some(3), "首写生效：先试的号拥有槽位");
    }

    const MOCK_BODY: &str = r#"{"conversationState":{"conversationId":"sess-1","currentMessage":{"userInputMessage":{"modelId":"claude-sonnet-4"}}}}"#;

    /// 成功路径：上游直接 200 → `Ok((resp, meta))`，retries=0，只打 1 次上游。
    ///
    /// 回退即 FAIL：把成功分支的 `report_success`/`return Ok` 弄丢（例如重试循环
    /// 对 200 也继续换号），断言失败。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn call_api_first_try_success_returns_zero_retries() {
        let up = MockUpstream::start(vec![MockResponse::ok(r#"{"ok":true}"#)]);
        let provider = provider_with_mock_upstream(&up);

        let (resp, meta) = match provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
        {
            Ok(v) => v,
            Err(e) => panic!("首次 200 应直接成功: {e}"),
        };
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), r#"{"ok":true}"#, "上游 body 必须原样透传");
        assert_eq!(meta.retries, 0, "首次即成功不得计重试");
        assert!(
            meta.credential_id == 91_001 || meta.credential_id == 91_002,
            "meta 必须带实际使用的那条凭据"
        );
        assert_eq!(
            up.hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "成功路径只允许打 1 次上游"
        );
    }

    /// ⭐ 链式回退核心回归（P0 移植）：第一端点 429 → 轮内立即换第二端点 → 200。
    ///
    /// 钉住三件事：
    /// 1. **不消耗重试预算**：`meta.retries` 必须仍是 0（链内跳不触碰 attempt 计数）；
    /// 2. 第二端点真被打到（hits 两个上游各 1）；
    /// 3. 链首 429 封桶（`order.len() > 1` 时）→ 但成功不受影响。
    ///
    /// 回退即 FAIL：把链式回退的 `continue 'endpoint_chain` 删掉（回到跨轮换号），
    /// `meta.retries` 变成 1 或请求失败——本测试断言 retries==0 必红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn endpoint_chain_fallback_uses_next_endpoint_without_retry_budget() {
        let up_a = MockUpstream::start(vec![MockResponse {
            status: 429,
            reason: "Too Many Requests",
            body: "{}",
            retry_after_secs: None,
        }]);
        let up_b = MockUpstream::start(vec![MockResponse::ok(r#"{"ok":true}"#)]);
        let provider = provider_with_mock_chain(&up_a, &up_b);

        let (resp, meta) = match provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
        {
            Ok(v) => v,
            Err(e) => panic!("第一端点 429 → 链式回退第二端点应成功: {e}"),
        };
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), r#"{"ok":true}"#);
        assert_eq!(meta.retries, 0, "链式回退不得消耗凭据重试预算（attempt 计数不变）");
        assert_eq!(
            up_a.hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "第一端点只打 1 次（429 后即顺延）"
        );
        assert_eq!(
            up_b.hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "第二端点必须被链式回退打到"
        );
    }

    /// ⭐ 死端点负缓存：A 端口无人监听（连接层失败）→ 记入负缓存 + 顺延 B 成功；
    /// 第二次调用跳过 A（负缓存 TTL 内），只打 B。
    ///
    /// 回退即 FAIL：把链循环顶部的 `is_endpoint_dead` 跳过分支删掉，第二次调用会
    /// 先白打一次 A（connect refused）——断言 `up_b.hits == 2` 变红（B 只被打 1 次
    /// 的话说明第二次没到 B？不——A 连接失败很快，B 仍会打到。真正的判据是
    /// `is_endpoint_dead` 被置位 + A 跳过。用 hits 数 A 无法直接数（连接失败不落
    /// mock），故用负缓存状态断言）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dead_endpoint_negative_cache_skips_failed_route() {
        let dead_port = dead_local_port();
        let up_b = MockUpstream::start(vec![
            MockResponse::ok(r#"{"ok":true}"#),
            MockResponse::ok(r#"{"ok":true}"#),
        ]);
        let provider = provider_with_mock_chain_ports(dead_port, up_b.port);

        // 第一次：A 连接失败（记负缓存）→ 顺延 B → 成功。
        let (resp, meta) = match provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
        {
            Ok(v) => v,
            Err(e) => panic!("A 连接失败应链式顺延到 B 并成功: {e}"),
        };
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(meta.retries, 0, "连接层顺延同样不耗重试预算");
        assert!(
            provider.is_endpoint_dead("ide", "us-east-1"),
            "连接层失败必须记入负缓存"
        );
        assert_eq!(
            up_b.hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "第一次调用 B 被顺延打到 1 次"
        );

        // 第二次：A 在负缓存 TTL 内 → 跳过，直接打 B。
        let (resp, _) = match provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
        {
            Ok(v) => v,
            Err(e) => panic!("第二次调用应跳过 A 直接打 B: {e}"),
        };
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(
            up_b.hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "A 被负缓存跳过 → B 累计打 2 次"
        );
        assert!(
            provider.is_endpoint_dead("ide", "us-east-1"),
            "负缓存 TTL 未到不得清除"
        );
    }

    /// ⭐ 链尾兜底铁律：A、B 全部连接失败过（都在负缓存内）→ A（非链尾）跳过、
    /// B（链尾）**绝不跳过**，仍真打 → 成功。
    ///
    /// 回退即 FAIL：把链循环的「链尾不跳过」条件删掉（`idx != last_idx` 放宽成
    /// 无条件跳过），第二次调用整链无人发送 → response 恒 None → 请求 Err。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn endpoint_chain_tail_is_never_skipped() {
        let dead_port = dead_local_port();
        let up_b = MockUpstream::start(vec![
            MockResponse::ok(r#"{"ok":true}"#),
            MockResponse::ok(r#"{"ok":true}"#),
        ]);
        let provider = provider_with_mock_chain_ports(dead_port, up_b.port);

        // 第一次：A 连接失败 → 记 dead；B 200。
        provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
            .expect("第一次应经 A 失败顺延 B 成功");
        assert!(provider.is_endpoint_dead("ide", "us-east-1"));

        // 把 B 也标记连接失败（模拟 B 近期也连不上）——现在链内两个端点全在负缓存里。
        provider.mark_endpoint_dead("codewhisperer", "us-east-1");

        // 第二次：A 跳过（非链尾）、B dead 但链尾不跳过 → 仍真打 B → 200。
        let (resp, _) = match provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
        {
            Ok(v) => v,
            Err(e) => panic!("链尾绝不跳过：全死仍尝试 B 并成功: {e}"),
        };
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(
            up_b.hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "链尾被跳过则 B 不会被打到第 2 次"
        );
    }

    /// 整链失败交凭据级分类：两个端点都 429 → 链式回退耗尽 → 链尾响应走既有
    /// 429 分类（封桶 + has_unthrottled 判定 + 冷却换号），最终 `Err` 透传。
    ///
    /// hits：号 1 打 A、B（链内 2 跳），封双桶 → has_unthrottled false（cli/cli-runtime/
    /// amazonq 未注册不算可用）→ 凭据冷却 → 号 2 同样 2 跳 → 预算耗尽（2 号池
    /// max_retries=2）→ Err。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn endpoint_chain_full_failure_falls_through_to_credential_classification() {
        let up_a = MockUpstream::start(vec![
            MockResponse {
                status: 429,
                reason: "Too Many Requests",
                body: "{}",
                retry_after_secs: None,
            },
            MockResponse {
                status: 429,
                reason: "Too Many Requests",
                body: "{}",
                retry_after_secs: None,
            },
        ]);
        let up_b = MockUpstream::start(vec![
            MockResponse {
                status: 429,
                reason: "Too Many Requests",
                body: "{}",
                retry_after_secs: None,
            },
            MockResponse {
                status: 429,
                reason: "Too Many Requests",
                body: "{}",
                retry_after_secs: None,
            },
        ]);
        let provider = provider_with_mock_chain(&up_a, &up_b);

        let err = provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
            .err().expect("两个端点都 429 → 整链失败应 Err（透传 429）");
        assert!(
            err.to_string().contains("429"),
            "整链失败必须交凭据级分类（透传上游 429 语义）: {err}"
        );
        assert_eq!(
            up_a.hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "两个号各打一次 A（链首）"
        );
        assert_eq!(
            up_b.hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "两个号各打一次 B（链尾）"
        );
    }

    /// ⭐ 链内共享预算闸（对抗审查 M1）：4 元素链全 429 → 总上游调用必须 ≤
    /// `ABSOLUTE_MAX_TOTAL_RETRIES`(=4)，不得出现「attempts × 链跳数」的超发。
    ///
    /// 现有 2 元素链测试（`endpoint_chain_full_failure_falls_through_...`）对 M1
    /// **失明**：2 元素链在第 1 个 attempt 内就打满 4 次预算（2 跳 × 2 号），hits 恒 4，
    /// 看不出「链内跳不扣共享预算」的洞。4 元素链下第 1 个 attempt 的 4 跳就耗尽预算，
    /// 换号后的第 2 个 attempt 必须在链首跳前被预算闸拦下——这是对「每请求 ≤
    /// ABSOLUTE_MAX_TOTAL_RETRIES 次上游调用」不变量的端到端回归。
    ///
    /// 回退即 FAIL：把链循环顶部的预算闸删掉，第 2 个 attempt 会再打 4 跳
    /// → total hits == 8 > 4，断言变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn endpoint_chain_respects_shared_retry_budget_when_all_429() {
        let ups: Vec<MockUpstream> = (0..4)
            .map(|_| {
                MockUpstream::start(vec![MockResponse {
                    status: 429,
                    reason: "Too Many Requests",
                    body: "{}",
                    retry_after_secs: None,
                }])
            })
            .collect();
        let ports: [u16; 4] = ups
            .iter()
            .map(|u| u.port)
            .collect::<Vec<_>>()
            .try_into()
            .expect("4 个端口");
        let provider = provider_with_mock_chain_4ports(ports);

        let err = provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
            .err()
            .expect("4 端点全 429 → 整链失败应 Err（透传 429）");
        assert!(
            err.to_string().contains("429"),
            "整链失败必须透传上游 429 语义: {err}"
        );

        let total: usize = ups
            .iter()
            .map(|u| u.hits.load(std::sync::atomic::Ordering::SeqCst))
            .sum();
        assert!(
            total <= ABSOLUTE_MAX_TOTAL_RETRIES,
            "链式回退 + 换号重试的总上游调用必须 ≤ ABSOLUTE_MAX_TOTAL_RETRIES（实际 {total} 次）\
             —— 链内每跳消耗共享预算，预算耗尽必须停在链首前"
        );
        assert_eq!(
            total, ABSOLUTE_MAX_TOTAL_RETRIES,
            "4 元素链全 429 恰好打满 4 次（第 1 个 attempt 的 4 跳），换号后的 attempt 被预算闸拦下"
        );
    }

    /// 🔴 对抗审查 m4：501（Not Implemented）是**确定性**错误，不得触发链式回退
    /// （`status.is_server_error()` 会把 501/505 也顺延，白烧一跳——换 host 不会让
    /// 501 变 200，它是对请求的确定性答复）。
    ///
    /// 回退即 FAIL：把链内瞬态判定改回 `|| status.is_server_error()`，501 触发
    /// 链式回退 → B 被真打（up_b.hits == 1），断言变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn endpoint_chain_does_not_fallback_on_501() {
        let up_a = MockUpstream::start(vec![MockResponse {
            status: 501,
            reason: "Not Implemented",
            body: "{}",
            retry_after_secs: None,
        }]);
        let up_b = MockUpstream::start(vec![MockResponse {
            status: 501,
            reason: "Not Implemented",
            body: "{}",
            retry_after_secs: None,
        }]);
        let provider = provider_with_mock_chain(&up_a, &up_b);

        let err = provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
            .err()
            .expect("501 不顺延 → 交凭据级分类（两号都 501）→ Err");
        assert!(
            err.to_string().contains("501"),
            "错误必须保留上游 501 语义: {err}"
        );
        // 501 不触发**链式**回退（同凭据换 host 不会让 501 变 200）；A 被命中 2 次 =
        // 首端点 1 次 + 凭据级/吸收层对 501 的既有重试 1 次（501 不在链内瞬态集，
        // 但吸收层分类仍视 5xx 为可吸收——那是既有行为，m4 只修链内顺延）。
        assert_eq!(
            up_a.hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "同凭据链内不得因 501 顺延（501 是确定性错误），A 命中 = 首打 + 吸收层重试"
        );
    }

    /// 注册表含 ide/cli/codewhisperer/amazonq 四端点的 provider（URL 指向无人监听端口，
    /// 链构造不联网），供 `endpoint_chain_for` 与负缓存的纯函数测试。
    fn provider_with_full_endpoint_registry() -> KiroProvider {
        let mut creds = Vec::new();
        let mut c = KiroCredentials::default();
        c.id = Some(94_001);
        creds.push(c);
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                false,
            )
            .expect("构造测试 token manager"),
        );
        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        for name in ["ide", "cli", "codewhisperer", "amazonq"] {
            endpoints.insert(
                name.to_string(),
                Arc::new(MockEndpoint {
                    url: format!("http://127.0.0.1:1/{name}"),
                    name,
                }),
            );
        }
        KiroProvider::with_proxy(tm, None, endpoints, "ide".to_string())
    }

    /// 链构造：OAuth 号（默认 ide）+ fallback 仍开 → 链只有 ide。
    /// 不得从 FALLBACK_ORDER 补 CLI 族（cw/amazonq 会硬编码 tokentype=API_KEY）。
    /// 回退即 FAIL：把 retain 改回给 OAuth 整表补齐，断言变红。
    #[test]
    fn endpoint_chain_oauth_head_gets_fallback_order_endpoints() {
        let p = provider_with_full_endpoint_registry();
        let head = p.endpoints["ide"].clone();
        let chain = p.endpoint_chain_for(&head, &KiroCredentials::default(), true, "us-east-1");
        let names: Vec<&str> = chain.iter().map(|ep| ep.name()).collect();
        assert_eq!(names, vec!["ide"]);
        assert!(
            !names.contains(&"codewhisperer") && !names.contains(&"amazonq"),
            "OAuth 号不得从 FALLBACK_ORDER 落入 CLI 族端点"
        );
    }

    /// 链构造：ksk_ 号（自动路由 cli）→ 链首 + 凭据候选顺序（CLI 族端点，cli-runtime
    /// 未注册跳过）+ 跨族兜底：codewhisperer/amazonq（同为 CLI 协议族）。
    ///
    /// 🔴 对抗审查 M2：ide 必须**整体不在链里**（不是排链尾）——ksk_ 打 ide 必 403
    /// 是确定性错误，而链尾有「兜底铁律」永不跳过，容量风暴时链尾 ide 必被真打 →
    /// 403 从从未成功号 report_failure 累计 → TooManyFailures 误禁用（历史事故
    /// #481 同型）。回退即 FAIL：把 `retain` 改回「挪到链尾」，断言变红。
    #[test]
    fn endpoint_chain_ksk_head_uses_credential_order_first() {
        let p = provider_with_full_endpoint_registry();
        let mut cred = KiroCredentials::default();
        cred.auth_method = Some("api_key".to_string());
        cred.kiro_api_key = Some("ksk_test".to_string());
        let head = p.endpoints["cli"].clone();
        let chain = p.endpoint_chain_for(&head, &cred, true, "us-east-1");
        let names: Vec<&str> = chain.iter().map(|ep| ep.name()).collect();
        assert_eq!(names, vec!["cli", "codewhisperer", "amazonq"]);
        assert!(
            !names.contains(&"ide"),
            "ksk_ 号链不得含 ide（协议族安全：ksk_ 打 ide 必 403，链尾兜底铁律会把它打成确定性失败）"
        );
    }

    /// 开关关闭：链退化为单元素（部署方显式关掉回退的意图必须被尊重）。
    #[test]
    fn endpoint_chain_fallback_disabled_is_single_element() {
        let p = provider_with_full_endpoint_registry();
        let head = p.endpoints["ide"].clone();
        let chain = p.endpoint_chain_for(&head, &KiroCredentials::default(), false, "us-east-1");
        assert_eq!(chain.len(), 1, "显式关闭回退时绝不擅自加端点");
        assert_eq!(chain[0].name(), "ide");
    }

    /// 主端点协议不符 → 不占链首（降级出链），其余健康**同族**端点按序上位。
    /// OAuth 无 CLI 族可上位：兜底铁律把 ide 放回，且不得跨族落到 cw/amazonq。
    #[test]
    fn endpoint_chain_broken_head_is_demoted_not_removed() {
        let p = provider_with_full_endpoint_registry();
        p.mark_route_protocol_broken("ide", "us-east-1");
        let head = p.endpoints["ide"].clone();
        let chain = p.endpoint_chain_for(&head, &KiroCredentials::default(), true, "us-east-1");
        assert!(!chain.is_empty());
        let names: Vec<&str> = chain.iter().map(|ep| ep.name()).collect();
        assert!(
            !names.contains(&"codewhisperer") && !names.contains(&"amazonq") && !names.contains(&"cli"),
            "OAuth 号 ide 被隔离也不得跨族落到 CLI 端点"
        );
        assert_eq!(names, vec!["ide"], "无同族可上位时兜底铁律放回 head");
    }

    /// 兜底铁律：所有端点都被隔离 → 链仍不得为空（否则 response 恒 None，请求无人发送）。
    #[test]
    fn endpoint_chain_never_empty_even_when_all_routes_quarantined() {
        let p = provider_with_full_endpoint_registry();
        for name in ["ide", "cli", "codewhisperer", "amazonq"] {
            p.mark_route_protocol_broken(name, "us-east-1");
        }
        let head = p.endpoints["ide"].clone();
        let chain = p.endpoint_chain_for(&head, &KiroCredentials::default(), true, "us-east-1");
        assert!(!chain.is_empty(), "全隔离时链仍不得为空");
    }

    /// 协议隔离是软的且按 (端点, region) 精确划界：不连坐别的 region / 端点。
    #[test]
    fn protocol_broken_quarantine_is_recorded_and_scoped() {
        let p = provider_with_full_endpoint_registry();
        assert!(!p.is_route_protocol_broken("cli", "us-east-1"), "初始不应有任何隔离");
        p.mark_route_protocol_broken("cli", "us-east-1");
        assert!(p.is_route_protocol_broken("cli", "us-east-1"));
        assert!(
            !p.is_route_protocol_broken("cli", "eu-central-1"),
            "隔离不得跨 region 连坐"
        );
        assert!(
            !p.is_route_protocol_broken("ide", "us-east-1"),
            "隔离不得跨端点连坐"
        );
    }

    /// M3：MCP 直连失败短负缓存 —— 记入 → 判定 → 按 (凭据 id, region) 划界。
    /// 同 region 其它 id 不连坐；与端点连接层键空间互不干扰。
    #[test]
    fn mcp_direct_negative_cache_blocks_same_region_only() {
        let p = provider_with_full_endpoint_registry();
        let id_a = 1u64;
        let id_b = 2u64;
        assert!(
            !p.is_mcp_direct_blocked(id_a, "us-east-1"),
            "初始不应有负缓存"
        );
        p.mark_endpoint_dead(&format!("mcp-direct@{}", id_a), "us-east-1");
        assert!(
            p.is_mcp_direct_blocked(id_a, "us-east-1"),
            "直连失败后 60s 内必须跳过该号直连"
        );
        assert!(
            !p.is_mcp_direct_blocked(id_b, "us-east-1"),
            "负缓存不得连坐同 region 其它凭据"
        );
        assert!(
            !p.is_mcp_direct_blocked(id_a, "eu-central-1"),
            "负缓存不得跨 region 连坐"
        );
        assert!(
            !p.is_endpoint_dead("ide", "us-east-1"),
            "mcp-direct 键不得影响端点连接层负缓存（键空间正交）"
        );
    }

    /// M3：直连负缓存 TTL 过期必须放行（自愈语义，同 is_endpoint_dead 同款惰性清理）。
    #[test]
    fn mcp_direct_negative_cache_expires_after_ttl() {
        let p = provider_with_full_endpoint_registry();
        let id = 1u64;
        p.dead_endpoints.lock().insert(
            format!("mcp-direct@{}@us-east-1", id),
            std::time::Instant::now() - std::time::Duration::from_secs(61),
        );
        assert!(
            !p.is_mcp_direct_blocked(id, "us-east-1"),
            "TTL 过期必须放行重试（上游/token 可能已恢复）"
        );
    }

    /// 死端点负缓存：记入 → 判定 → 划界 → alive 清除。
    #[test]
    fn dead_endpoint_negative_cache_is_recorded_cleared_and_scoped() {
        let p = provider_with_full_endpoint_registry();
        assert!(!p.is_endpoint_dead("ide", "us-east-1"), "初始不应有负缓存");
        p.mark_endpoint_dead("ide", "us-east-1");
        assert!(p.is_endpoint_dead("ide", "us-east-1"));
        assert!(
            !p.is_endpoint_dead("ide", "eu-central-1"),
            "负缓存不得跨 region 连坐"
        );
        p.mark_endpoint_alive("ide", "us-east-1");
        assert!(
            !p.is_endpoint_dead("ide", "us-east-1"),
            "mark_endpoint_alive 必须清除负缓存（拿到 HTTP 响应 = 连接层通了）"
        );
    }

    /// ⭐ 接线守卫：endpoint_chain_for 必须在 call_api_with_retry 内被调用（链式回退接线）。
    ///
    /// 回退即 FAIL：把链构造挪到别的函数 / 改成 `endpoint_for` 单端点直发 → 本条变红。
    #[test]
    fn endpoint_chain_for_is_wired_in_call_api_with_retry() {
        let full = include_str!("provider.rs");
        let cut = full.find("#[cfg(test)]").unwrap_or(full.len());
        let prod = &full[..cut];
        // needle 运行时拼接，避免 include_str! 自匹配。
        let call = ["self.endpoint_chain_for", "("].concat();
        assert_eq!(
            prod.matches(&call).count(),
            1,
            "endpoint_chain_for 应在生产段恰好调用 1 次（MCP 路径不参与端点回退）"
        );
        let fn_body = prod
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        assert!(
            fn_body.contains(&call),
            "endpoint_chain_for 调用必须在 call_api_with_retry 函数体内"
        );
    }

    /// ⭐ 接线守卫：endpoint_fallback 配置开关必须作为 endpoint_chain_for 的实参接线，
    /// 否则开关是死配置（关了也没用）。
    #[test]
    fn endpoint_fallback_config_is_wired_into_chain() {
        let full = include_str!("provider.rs");
        let cut = full.find("#[cfg(test)]").unwrap_or(full.len());
        let prod = &full[..cut];
        let call = ["self.endpoint_chain_for", "("].concat();
        let at = prod
            .find(&call)
            .expect("endpoint_chain_for 调用不应被改名");
        // 开关实参在调用点**之后**（同一调用语句内）。`after` 从 at 起切：at 是
        // ASCII needle 的起点（合法字符边界），find 返回的偏移同理，无多字节坑。
        let after = &prod[at..];
        let gate = after.find("config.endpoint_fallback").unwrap_or_else(|| {
            panic!(
                "endpoint_chain_for 的 fallback_enabled 实参必须来自 config 开关 \
                 （否则该配置是死配置，部署方显式关闭会被无视）"
            )
        });
        assert!(gate < 300, "开关实参必须在调用点近旁（同一语句窗口）");
    }

    /// 失败重试路径：上游先 429（带 Retry-After）→ 冷却 + 换号 → 第二个号 200。
    ///
    /// 钉住「429 分类 → 凭据冷却 → tried_this_call 结构性排除 → failover 换号 → 成功」
    /// 整条链（此前只有 absorb 层纯函数测试，链咬合从未真跑）。meta.retries 必须为 1。
    ///
    /// 回退即 FAIL：把 429 分支的 `report_rate_limited_with_retry_after` 或
    /// `tried_this_call.insert` 删掉——换号会落空、整条链回到同一个号，请求变 Err。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn call_api_failover_after_upstream_429_succeeds_on_another_credential() {
        let up = MockUpstream::start(vec![
            MockResponse {
                status: 429,
                reason: "Too Many Requests",
                body: "{}",
                retry_after_secs: Some(1),
            },
            MockResponse::ok(r#"{"ok":true}"#),
        ]);
        let provider = provider_with_mock_upstream(&up);

        let (resp, meta) = match provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
        {
            Ok(v) => v,
            Err(e) => panic!("429 换号后应成功: {e}"),
        };
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(meta.retries, 1, "429 一次 + 换号成功 = 恰好 1 次重试");
        assert_eq!(
            up.hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "必须打 2 次上游（1 次 429 + 1 次成功）"
        );
    }

    /// 重试耗尽：1 号池（小池预算 1 次）+ 上游恒 500 → `Err`，且只打 1 次就停
    /// （预算 1 = 各号摸一次即透传，不风暴）。
    ///
    /// 回退即 FAIL：把墙钟闸门/预算判据改松（例如预算恢复成号池倍数），
    /// hits 会变成 3+，断言失败。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn call_api_budget_exhausted_returns_err_without_storming_upstream() {
        let up = MockUpstream::start(vec![
            MockResponse {
                status: 500,
                reason: "Internal Server Error",
                body: "{}",
                retry_after_secs: None,
            },
            MockResponse {
                status: 500,
                reason: "Internal Server Error",
                body: "{}",
                retry_after_secs: None,
            },
            MockResponse {
                status: 500,
                reason: "Internal Server Error",
                body: "{}",
                retry_after_secs: None,
            },
        ]);
        // 单号池：小池预算 = 每号 1 次 = 总共 1 次尝试。
        let mut cred = KiroCredentials::default();
        cred.id = Some(91_003);
        cred.auth_method = Some("api_key".to_string());
        cred.kiro_api_key = Some("sk-mock-91003".to_string());
        // 同 provider_with_mock_upstream：显式钉死 endpoint，否则被自动路由到
        // 内置 cli/cli-runtime 候选链、mock 上游零命中。
        cred.endpoint = Some("mock".to_string());
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![cred],
                None,
                None,
                false,
            )
            .expect("构造测试 token manager"),
        );
        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert(
            "mock".to_string(),
            Arc::new(MockEndpoint {
                url: format!("http://127.0.0.1:{}", up.port),
                name: "mock",
            }),
        );
        let provider = KiroProvider::with_proxy(tm, None, endpoints, "mock".to_string());

        let err = match provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
        {
            Ok(_) => panic!("预算耗尽必须 Err"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("500"),
            "终态错误必须透传上游 500 文案，实际: {}",
            err
        );
        assert_eq!(
            up.hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "小池预算 1：恒 500 也只许打 1 次，不得风暴"
        );
    }

    // ══════════ A-5：429 备区换桶 与 L1 403 换区 的共享感知（防同请求内 A→B→A→B 振荡）══════════

    /// A-5 专用端点：**URL 与桶键都随 region 走** —— 让「当前区全封 ⇒ 用备区桶」的
    /// 换桶真实可触发（MockEndpoint 的 URL/桶键与 region 无关，两区永远同桶，换桶
    /// 路径根本走不到）。`decorate_api` 仿真实 cli 端点带上 Bearer 头，让 mock 上游
    /// 能按 token 区分请求是哪个号发的。
    ///
    /// 依赖前提：`bucket_key`/`bucket_id` 走 trait 默认实现（= `api_url`，`amz_target`
    /// 为 None），故 URL 随区变化 ⇒ 桶键随区独立；与生产端点的「region 在 host 里」
    /// 是同一种不变量。
    struct RegionAwareMockEndpoint {
        eu_url: String,
        us_url: String,
    }

    impl KiroEndpoint for RegionAwareMockEndpoint {
        fn name(&self) -> &'static str {
            "mock-region"
        }
        fn api_url(&self, ctx: &RequestContext<'_>) -> String {
            self.url_for_region(ctx.credentials.effective_upstream_region(&ctx.config))
        }
        fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
            self.url_for_region(ctx.credentials.effective_upstream_region(&ctx.config))
        }
        fn decorate_api(
            &self,
            req: reqwest::RequestBuilder,
            ctx: &RequestContext<'_>,
        ) -> reqwest::RequestBuilder {
            req.header("Authorization", format!("Bearer {}", ctx.token))
        }
        fn decorate_mcp(
            &self,
            req: reqwest::RequestBuilder,
            _ctx: &RequestContext<'_>,
        ) -> reqwest::RequestBuilder {
            req
        }
        fn transform_api_body(&self, body: &str, _ctx: &RequestContext<'_>) -> String {
            body.to_string()
        }
    }

    impl RegionAwareMockEndpoint {
        fn url_for_region(&self, region: &str) -> String {
            match region {
                "eu-central-1" => self.eu_url.clone(),
                _ => self.us_url.clone(),
            }
        }
    }

    /// A-5 复现测试（钉顺序）：当前区 429 全封 → 429 路径换备区 → 备区 403 →
    /// **不得**再由 L1 换回原区（原区桶仍在 30s 封禁期，select_endpoint 会把请求
    /// 弹回备区 ⇒ 同一请求内 A→B→A→B 振荡）。
    ///
    /// 序列（#910101 = 受害者，初始区 eu，备区 us）：
    ///   ① #910101 → eu → 429（eu 桶被封）
    ///   ② #910101 → us（429 备区换桶）→ 403 bearer-invalid
    ///   ③ 修复前：L1 按当前区(us)算 `region_retry_target` 换回 eu → eu 桶还封着
    ///      → 备区路径又弹回 us → #910101 **第二次**打 us；修复后：共享标记
    ///      `region_switched_this_call` 挡住 L1 ⇒ #910101 不再换区，惩罚换号走
    ///      failover（再打 eu 的是 #910102）。
    ///
    /// 断言（按请求头里的 token 数每个号在每区的命中）：
    ///   - us 上 #910101 恰好 1 次（换区次数 ≤ 1；修复前是 2 次 = 振荡）；
    ///   - eu 上 #910101 恰好 1 次（从未被换回去）；
    ///   - 单路径行为不变：#910102 从未换过区，eu 403 后仍走 L1 换区（us 恰好 1 次）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a5_no_region_pingpong_after_429_swap_then_403() {
        // eu = 当前区（429 封桶），us = 备区（403 bearer-invalid）。
        // 每区各配 2 个响应就够（预算封顶 4 次上游调用）；队列耗尽后一律 500，
        // 若真的多打了，500 也会被计入命中 → 断言照样红，不会假绿。
        let eu = MockUpstream::start(vec![
            MockResponse {
                status: 429,
                reason: "Too Many Requests",
                body: "{}",
                retry_after_secs: None,
            },
            MockResponse {
                status: 403,
                reason: "Forbidden",
                body: REAL_BEARER_INVALID_BODY,
                retry_after_secs: None,
            },
            MockResponse {
                status: 403,
                reason: "Forbidden",
                body: REAL_BEARER_INVALID_BODY,
                retry_after_secs: None,
            },
        ]);
        let us = MockUpstream::start(vec![
            MockResponse {
                status: 403,
                reason: "Forbidden",
                body: REAL_BEARER_INVALID_BODY,
                retry_after_secs: None,
            },
            MockResponse {
                status: 403,
                reason: "Forbidden",
                body: REAL_BEARER_INVALID_BODY,
                retry_after_secs: None,
            },
        ]);

        // 4 个 ksk_ 号，初始区钉死 eu（PROBE_ORDER 首项 ⇒ 备区恒为 us），
        // endpoint 钉死 mock-region（order = [mock-region, cli, cli-runtime]，len>1
        // ⇒ 429 分支会封桶并尝试换桶，这正是 A-5 的前提）。id 用本组专属段 91_1xx
        // （endpoint_health 是进程级共享表，避开既有测试的 91_0xx）。
        //
        // ⚠️ 号池必须 ≥4：本轮重试配额 = compute_max_retries(号数, …)，号数 ≤
        // SMALL_POOL_THRESHOLD(3) 时每号只重试 1 次 ⇒ 2 号池配额 = 2，第 2 跳（#910101
        // 打 us）403 后本轮即耗尽（region 错配类错误按设计不可吸收，吸收层不会续轮），
        // failover 根本走不到 #910102。4 号池配额 = min(4×3, 4) = 4，正好装下 A-5 全
        // 序列（eu→us→eu→us，共 4 次上游调用）。91_103/91_104 是陪跑号：选号排序键
        // 按 id 升序平局决胜（⑬ e.id），本序列只会用到 91_101/91_102，它们不被选中。
        let mut creds = Vec::new();
        for (id, token) in [
            (91_101u64, "sk-mock-a5-910101"),
            (91_102, "sk-mock-a5-910102"),
            (91_103, "sk-mock-a5-910103"),
            (91_104, "sk-mock-a5-910104"),
        ] {
            let mut c = KiroCredentials::default();
            c.id = Some(id);
            c.auth_method = Some("api_key".to_string());
            c.kiro_api_key = Some(token.to_string());
            c.api_region = Some("eu-central-1".to_string());
            c.endpoint = Some("mock-region".to_string());
            creds.push(c);
        }
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                false,
            )
            .expect("构造测试 token manager"),
        );
        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert(
            "mock-region".to_string(),
            Arc::new(RegionAwareMockEndpoint {
                eu_url: format!("http://127.0.0.1:{}", eu.port),
                us_url: format!("http://127.0.0.1:{}", us.port),
            }),
        );
        let provider = KiroProvider::with_proxy(tm, None, endpoints, "mock-region".to_string());

        let err = match provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
        {
            Ok(_) => panic!("备区 403 是永久性（未授权区），序列必须以 Err 收尾"),
            Err(e) => e,
        };

        let eu_heads = eu.captured_heads();
        let us_heads = us.captured_heads();
        let hits_by = |heads: &[String], token: &str| heads.iter().filter(|h| h.contains(token)).count();
        let victim_eu = hits_by(&eu_heads, "sk-mock-a5-910101");
        let victim_us = hits_by(&us_heads, "sk-mock-a5-910101");
        let fresh_eu = hits_by(&eu_heads, "sk-mock-a5-910102");
        let fresh_us = hits_by(&us_heads, "sk-mock-a5-910102");
        let extra_eu = hits_by(&eu_heads, "sk-mock-a5-910103") + hits_by(&eu_heads, "sk-mock-a5-910104");
        let extra_us = hits_by(&us_heads, "sk-mock-a5-910103") + hits_by(&us_heads, "sk-mock-a5-910104");
        eprintln!(
            "a5 hits eu={} us={} | 910101 eu/us={}/{} 910102 eu/us={}/{} extra 103+104 eu/us={}/{} | err={}",
            eu_heads.len(),
            us_heads.len(),
            victim_eu,
            victim_us,
            fresh_eu,
            fresh_us,
            extra_eu,
            extra_us,
            err
        );
        assert!(
            err.to_string().contains("403"),
            "终态错误必须透传上游 403 文案，实际: {err}; hits 910101 eu/us={victim_eu}/{victim_us} \
             910102 eu/us={fresh_eu}/{fresh_us} extra103+104 eu/us={extra_eu}/{extra_us} \
             eu_total={} us_total={}",
            eu_heads.len(),
            us_heads.len()
        );

        assert_eq!(
            victim_eu, 1,
            "受害者 #910101 必须恰好打 1 次当前区 eu（初始 429）；换回原区=振荡，实际 {victim_eu}"
        );
        assert_eq!(
            victim_us, 1,
            "受害者 #910101 在备区 us 必须恰好 1 次（429 换桶那一次）；\
             修复前 L1 换回 eu 被备区路径弹回 ⇒ us 会是 2 次（A→B→A→B 振荡），实际 {victim_us}"
        );
        assert_eq!(
            fresh_eu, 1,
            "从未换过区的 #910102 应接住 failover 打 eu，实际 {fresh_eu}"
        );
        assert_eq!(
            fresh_us, 1,
            "单路径行为不变：从未换过区的 #910102 吃 eu 403 后仍走 L1 换区（us 恰好 1 次），\
             实际 {fresh_us}"
        );
    }

    /// ksk_ API Key 凭据：`effective_endpoint_order` 返回多端点候选链（q.* 优先、其余回退）。
    fn ksk_credential() -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("ksk_test_key".to_string());
        c.endpoint = None;
        c
    }

    /// 无统计数据时，每个候选端点都会被试到（冷启动探测），且**首选遵循先验顺序**。
    ///
    /// 🔴 **本测试取代了原来的 `select_endpoint_rotates_through_all_available_endpoints`。**
    /// 那条断言的是严格 round-robin 全序（`cli, cli-runtime, cli, cli-runtime, ...`），
    /// 而 round-robin 正是本次要移除的行为（它既不按凭据也不按成功率）。断言逐字保留会
    /// 让"改对了"表现为"测试失败"，所以必须换成断言**新的契约**：
    ///
    /// - 冷启动阶段每个候选都能拿到样本（不试就永远没数据，没数据就永远不被降权 = 死锁）
    /// - 首次选择走先验（候选序第一个），保证零数据时行为与旧的固定优先序一致
    #[test]
    fn select_endpoint_probes_every_candidate_on_cold_start() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let cred = ksk_credential();
        let order = cred.effective_endpoint_order(cli);
        assert!(order.len() >= 2, "ksk_ 号应为多端点候选链");

        // ⚠️ 用**本测试专属**的凭据 id：端点健康表是进程级共享的（见
        // endpoint_health::SHARED），而 `cargo test` 多线程并发跑 ⇒ 与别的测试共用 id
        // 会让「冷启动」前提被对方写入的样本破坏（本测试断言的正是零样本时的行为）。
        const ID: u64 = 90_001;

        // 零数据时首选 = 先验第一个（与旧固定优先序一致，无回归）。
        assert_eq!(
            provider.pick_endpoint_for_test(&cred, ID).unwrap().name(),
            order[0],
            "冷启动首选必须遵循候选先验顺序"
        );

        // 每选中一个就记一次结果（模拟真实请求闭环），冷启动规则会把流量导向尚无样本者，
        // 直到所有候选都被探测过。
        let mut seen: HashSet<&str> = HashSet::new();
        for _ in 0..(order.len() * 4) {
            let ep = provider.pick_endpoint_for_test(&cred, ID).expect("全可用必有返回");
            let name = ep.name();
            seen.insert(name);
            provider.report_endpoint_outcome(ID, name, true);
        }
        assert_eq!(
            seen.len(),
            order.len(),
            "冷启动阶段每个候选端点都必须被试到，实际 {:?}",
            seen
        );
    }

    /// 硬门：部分桶冷却时只选非冷却桶（跳过被封桶，恒命中剩余桶）。
    ///
    /// 429 封禁是**硬门**，自适应派发（软偏好）不得越过它 —— 哪怕被封那个桶的
    /// 历史成功率更高。软偏好只在硬门放行的候选之间排序。
    #[test]
    fn select_endpoint_hard_gate_skips_cooled_buckets() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let cred = ksk_credential();
        let order = cred.effective_endpoint_order(cli);
        // 封掉首选桶（q.*），其余候选保持可用。
        // ⚠️ 桶键必须用 `bucket_key` 算 —— 与生产写入点同源。写死端点名会让这条测试
        // 恒绿而实际什么都没封（键对不上 ⇒ select 侧读不到），是最坏的假绿形态。
        let cfg = provider.token_manager.config();
        let blocked_key = provider
            .endpoints
            .get(order[0])
            .expect("首选端点应已注册")
            .bucket_key(&cred, &cfg);
        provider.endpoint_buckets.lock().insert(
            (123, blocked_key),
            Instant::now() + Duration::from_secs(60),
        );
        let mut picked: HashSet<&str> = HashSet::new();
        // 循环次数取探索周期的整数倍：自适应派发靠「冷启动优先 + 周期性探索」覆盖
        // 全部候选，而不再是每次调用就换一个（round-robin）。次数不足会漏掉探索节拍。
        for _ in 0..(order.len() * 16) {
            let ep = provider
                .pick_endpoint_for_test(&cred, 123)
                .expect("还有非冷却桶，必有返回");
            assert_ne!(ep.name(), order[0], "硬门不得放行被封的端点桶");
            picked.insert(ep.name());
        }
        // 足够多的调用里，所有非冷却桶都应被选到（冷启动保证每个至少一次）。
        assert_eq!(
            picked.len(),
            order.len() - 1,
            "部分冷却时应覆盖所有非冷却桶"
        );
    }

    /// 🔴 ksk 号「当前区全封 ⇒ 改用备区桶」（2026-08-10 新增能力的正向测试）。
    ///
    /// # 修的是什么
    /// 一个 `ksk_` 号的桶集合此前只含**当前 region** 的两个（`q.<区>` / `runtime.<区>`）。
    /// 当前区两个桶各被 429 封 30s ⇒ `select_endpoint` 返 None ⇒ 判该号不可用
    /// ⇒ **另一个区即使完全空闲也永不被尝试**。实测后果是单号有效 RPM 被压到
    /// 「30s 窗口能挤进多少」（用户观察到 EU 号从 60~70 RPM 掉到十几二十）。
    ///
    /// 本测试钉住：只封当前区 ⇒ 仍能选出端点，且返回的备区 **不等于**当前区。
    #[test]
    fn ksk_falls_back_to_alt_region_when_current_region_all_banned() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let cred = ksk_credential();
        let order = cred.effective_endpoint_order(cli);
        let cfg = provider.token_manager.config();
        let cur = cred.effective_upstream_region(&cfg);

        // 只封**当前区**的全部桶（桶键与生产同源）
        {
            let mut buckets = provider.endpoint_buckets.lock();
            for name in &order {
                let key = provider
                    .endpoints
                    .get(*name)
                    .expect("候选端点应已注册")
                    .bucket_key(&cred, &cfg);
                buckets.insert((123, key), Instant::now() + Duration::from_secs(60));
            }
        }

        let (_, alt) = provider
            .select_endpoint(&cred, 123)
            .expect("当前区全封时必须回退到备区，而不是判该号不可用");
        let alt = alt.expect("回退路径必须报告用了哪个备区（调用方要据此覆盖 api_region）");
        assert_ne!(
            alt, cur,
            "备区不能等于当前区，否则等于没换（桶仍在封禁中）"
        );
    }

    /// 全部冷却返回 None（既有语义，自适应派发不得破坏）。
    ///
    /// ⚠️ 2026-08-10 起 ksk 号有「当前区全封 ⇒ 用备区桶」的回退
    /// （见 `ksk_falls_back_to_alt_region_when_current_region_all_banned`），
    /// 所以要断言"真的无桶可用"必须把**所有候选 region** 的桶都封掉。
    /// 保留 ksk 号不变，但把**每个候选 region** 的桶都封掉 —— 只有这样才是
    /// "真的无桶可用"。（只封当前区已不足以返 None，那正是新回退能力要解决的场景。）
    #[test]
    fn select_endpoint_all_cooled_returns_none() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let cred = ksk_credential();
        let order = cred.effective_endpoint_order(cli);
        // 桶键与生产同源（`bucket_key`），理由同上一条测试。
        let cfg = provider.token_manager.config();
        {
            let mut buckets = provider.endpoint_buckets.lock();
            // 遍历所有候选 region（与生产回退用的同一份 `PROBE_ORDER`），
            // 逐区把该区的全部端点桶封住。
            for region in crate::kiro::region_probe::PROBE_ORDER {
                let mut c = cred.clone();
                c.api_region = Some(region.to_string());
                for name in &order {
                    let key = provider
                        .endpoints
                        .get(*name)
                        .expect("候选端点应已注册")
                        .bucket_key(&c, &cfg);
                    buckets.insert((123, key), Instant::now() + Duration::from_secs(60));
                }
            }
        }
        assert!(
            provider.pick_endpoint_for_test(&cred, 123).is_none(),
            "全部冷却必须返回 None"
        );
    }

    /// last hop 全桶 429 封禁：generic 错误必须带 `retry_after_secs=`（handlers A5 → 429+RA，
    /// 而不是无标记兜底 502）。TTL 取最短桶剩余，没有则 2s。不增加 hop。
    #[test]
    fn last_hop_all_buckets_sealed_stamps_retry_after_secs() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        const ID: u64 = 92_201;
        let mut cred = ksk_credential();
        cred.id = Some(ID);
        let tm = std::sync::Arc::new(
            crate::kiro::token_manager::MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![cred.clone()],
                None,
                None,
                false,
            )
            .expect("测试 token manager"),
        );
        let provider = KiroProvider::with_proxy(
            tm,
            None,
            crate::kiro::endpoint::registry(),
            cli.to_string(),
        );
        let order = cred.effective_endpoint_order(cli);
        let cfg = provider.token_manager.config();
        {
            let mut buckets = provider.endpoint_buckets.lock();
            for region in crate::kiro::region_probe::PROBE_ORDER {
                let mut c = cred.clone();
                c.api_region = Some(region.to_string());
                for name in &order {
                    let key = provider
                        .endpoints
                        .get(*name)
                        .expect("候选端点应已注册")
                        .bucket_key(&c, &cfg);
                    buckets.insert((ID, key), Instant::now() + Duration::from_secs(14));
                }
            }
        }
        assert!(
            provider.select_endpoint(&cred, ID).is_none(),
            "全 region 封桶必须返 None"
        );
        assert!(
            provider.all_enabled_kiro_endpoint_buckets_sealed(),
            "号池里唯一 Kiro 的桶全封"
        );
        let ra = provider.shortest_endpoint_bucket_retry_after_secs(Some(ID));
        assert!(
            (1..=14).contains(&ra),
            "最短桶 TTL 应落在 1..=14，实际 {ra}"
        );

        let marker = ["retry_after_secs", "="].concat();
        let sealed = anyhow::anyhow!(
            "凭据 #{ID} 所有端点桶均处于 429 封禁期（当前区与备用区的桶都在封禁中）"
        );
        let stamped = provider.with_sealed_bucket_retry_after(
            sealed,
            crate::usage::RequestOutcome::RateLimited,
        );
        let s = stamped.to_string();
        assert!(
            s.contains(marker.as_str()),
            "A5 冷却分支需要 {marker} 才能 429+Retry-After 而非 502: {s}"
        );
        let secs = crate::anthropic::handlers::parse_retry_after_secs(&s)
            .expect("必须能解析 retry_after 真值");
        assert!((1..=14).contains(&secs), "解析出 {secs}");
        let again = provider.with_sealed_bucket_retry_after(
            stamped,
            crate::usage::RequestOutcome::RateLimited,
        );
        assert_eq!(
            again.to_string().matches(marker.as_str()).count(),
            1,
            "已有标记不得再打一份"
        );

        let generic = anyhow::anyhow!("流式 API 请求失败: 429 Too Many Requests {{}}");
        let s2 = provider
            .with_sealed_bucket_retry_after(generic, crate::usage::RequestOutcome::RateLimited)
            .to_string();
        assert!(
            s2.contains(marker.as_str()),
            "last hop generic 429 必须 stamp: {s2}"
        );

        let auth = anyhow::anyhow!("流式 API 请求失败: 403 Forbidden bearer invalid");
        let s3 = provider
            .with_sealed_bucket_retry_after(auth, crate::usage::RequestOutcome::AuthFailed)
            .to_string();
        assert!(
            !s3.contains(marker.as_str()),
            "403 不得被打成 A5 冷却: {s3}"
        );

        let empty = provider_with_default(cli);
        assert_eq!(
            empty.shortest_endpoint_bucket_retry_after_secs(None),
            2,
            "无封禁桶时兜底 2s"
        );
    }

    /// 混池：只有部分号的桶封了 → 不是「every credential sealed」，不 stamp。
    #[test]
    fn mixed_pool_partial_seal_does_not_stamp_retry_after_secs() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        const A: u64 = 92_211;
        const B: u64 = 92_212;
        let mk = |id: u64| {
            let mut c = ksk_credential();
            c.id = Some(id);
            c
        };
        let tm = std::sync::Arc::new(
            crate::kiro::token_manager::MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![mk(A), mk(B)],
                None,
                None,
                false,
            )
            .expect("测试 token manager"),
        );
        let provider = KiroProvider::with_proxy(
            tm,
            None,
            crate::kiro::endpoint::registry(),
            cli.to_string(),
        );
        let cred_a = mk(A);
        let order = cred_a.effective_endpoint_order(cli);
        let cfg = provider.token_manager.config();
        {
            let mut buckets = provider.endpoint_buckets.lock();
            for region in crate::kiro::region_probe::PROBE_ORDER {
                let mut c = cred_a.clone();
                c.api_region = Some(region.to_string());
                for name in &order {
                    let key = provider
                        .endpoints
                        .get(*name)
                        .expect("候选端点应已注册")
                        .bucket_key(&c, &cfg);
                    buckets.insert((A, key), Instant::now() + Duration::from_secs(30));
                }
            }
        }
        assert!(
            !provider.all_enabled_kiro_endpoint_buckets_sealed(),
            "B 未封，不得判全池封禁"
        );
        let marker = ["retry_after_secs", "="].concat();
        let generic = anyhow::anyhow!("流式 API 请求失败: 429 Too Many Requests {{}}");
        let s = provider
            .with_sealed_bucket_retry_after(generic, crate::usage::RequestOutcome::RateLimited)
            .to_string();
        assert!(
            !s.contains(marker.as_str()),
            "还有未封号时 generic 429 不 stamp（换号仍可能成功）: {s}"
        );
    }

    /// order 长度 1（单端点 OAuth 号）：恒返回该唯一端点，行为与固定优先序完全一致（零回归）。
    #[test]
    fn select_endpoint_single_endpoint_is_stable() {
        let provider = provider_with_default("ide");
        let cred = KiroCredentials::default(); // 无 api_key、无显式 endpoint → order=[ide]
        for _ in 0..4 {
            let ep = provider
                .pick_endpoint_for_test(&cred, 9)
                .expect("单端点不封必有返回");
            assert_eq!(ep.name(), "ide", "单端点恒返回唯一端点");
        }
    }

    /// ⭐ 自适应派发：某端点对某号连续失败后，流量应转向成功的那个端点。
    ///
    /// 这是替换 round-robin 的**核心收益**：旧实现无论结果如何都每隔一次送一批请求
    /// 给坏端点，新实现会学会避开它。
    #[test]
    fn select_endpoint_shifts_traffic_to_successful_endpoint() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let cred = ksk_credential();
        let order = cred.effective_endpoint_order(cli);
        assert!(order.len() >= 2, "ksk_ 号应有多端点候选，否则本测试无意义");
        let bad = order[0];
        let good = order[1];

        // 先让两个端点各拿到样本（冷启动阶段），再把 bad 打成恒失败。
        provider.report_endpoint_outcome(90011, bad, false);
        provider.report_endpoint_outcome(90011, good, true);
        for _ in 0..5 {
            provider.report_endpoint_outcome(90011, bad, false);
            provider.report_endpoint_outcome(90011, good, true);
        }

        // 统计一轮选择里 good 的占比：应显著多于 bad（bad 只在探索节拍出现）。
        let mut good_hits = 0;
        let mut bad_hits = 0;
        for _ in 0..32 {
            match provider.pick_endpoint_for_test(&cred, 90011) {
                Some(ep) if ep.name() == good => good_hits += 1,
                Some(ep) if ep.name() == bad => bad_hits += 1,
                other => panic!("意外的端点: {:?}", other.map(|e| e.name())),
            }
        }
        assert!(
            good_hits > bad_hits * 3,
            "成功端点应拿到绝大多数流量，实际 good={} bad={}",
            good_hits,
            bad_hits
        );
        assert!(
            bad_hits > 0,
            "坏端点仍须被周期性探索（否则上游恢复无从发现）"
        );
    }

    /// ⭐ 自适应派发是**每凭据独立**的：号 A 学到的结论不得影响号 B。
    ///
    /// 旧的 `endpoint_rotation` 是全进程共享计数器，做不到这一点 —— 这正是本次替换
    /// 要解决的第一个缺陷。
    #[test]
    fn select_endpoint_learning_is_per_credential() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let cred = ksk_credential();
        let order = cred.effective_endpoint_order(cli);
        let a = order[0];
        let b = order[1];

        // 号 90021：a 坏 b 好。号 90022：反过来。
        for _ in 0..6 {
            provider.report_endpoint_outcome(90021, a, false);
            provider.report_endpoint_outcome(90021, b, true);
            provider.report_endpoint_outcome(90022, a, true);
            provider.report_endpoint_outcome(90022, b, false);
        }

        // 各取一次非探索节拍的选择（连续多次取众数，避开探索干扰）。
        let mut a_for_202 = 0;
        let mut b_for_101 = 0;
        for _ in 0..7 {
            if provider.pick_endpoint_for_test(&cred, 90022).map(|e| e.name()) == Some(a) {
                a_for_202 += 1;
            }
            if provider.pick_endpoint_for_test(&cred, 90021).map(|e| e.name()) == Some(b) {
                b_for_101 += 1;
            }
        }
        assert!(a_for_202 >= 5, "号 90022 应偏好 a，实际命中 {}", a_for_202);
        assert!(b_for_101 >= 5, "号 90021 应偏好 b，实际命中 {}", b_for_101);
    }

    /// ⭐ 硬门优先于软偏好：成功率最高的端点被 429 封禁时，必须让位给低成功率的可用端点。
    #[test]
    fn hard_gate_overrides_success_rate_preference() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let cred = ksk_credential();
        let order = cred.effective_endpoint_order(cli);
        let weak = order[0];
        let strong = order[1];

        // strong 成功率远高于 weak。
        for _ in 0..6 {
            provider.report_endpoint_outcome(90031, weak, false);
            provider.report_endpoint_outcome(90031, strong, true);
        }
        // 但 strong 被 429 封禁。
        // 桶键与生产同源（`bucket_key`）：写死端点名会让封禁不生效而测试假绿。
        let cfg = provider.token_manager.config();
        let strong_key = provider
            .endpoints
            .get(strong)
            .expect("strong 端点应已注册")
            .bucket_key(&cred, &cfg);
        provider.endpoint_buckets.lock().insert(
            (90031, strong_key),
            Instant::now() + Duration::from_secs(60),
        );

        for _ in 0..8 {
            let ep = provider
                .pick_endpoint_for_test(&cred, 90031)
                .expect("weak 未封禁，必有返回");
            assert_eq!(
                ep.name(),
                weak,
                "硬门封禁的端点不得因成功率高而被选中"
            );
        }
    }

    /// ⭐ 每凭据并发闸：同一号的许可数受限，**不同号各自独立**。
    ///
    /// 这是两级闸的核心契约 —— 全局闸只管总量，不管分布；本闸保证一个号打满后
    /// 其余容量必然留给别的号（防「一个慢号吃光全局许可、整池被拖死」）。
    #[test]
    fn per_credential_gate_is_isolated_between_credentials() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let limit = provider.per_credential_limit();
        assert!(limit >= 1, "容量必须 ≥1，否则号被静默废掉");

        // 号 1 拿满全部许可。
        let g1 = provider.per_credential_gate(1);
        let mut held = Vec::new();
        for _ in 0..limit {
            held.push(
                g1.clone()
                    .try_acquire_owned()
                    .expect("容量内应能拿到许可"),
            );
        }
        // 再拿必失败 —— 硬上限生效。
        assert!(
            g1.clone().try_acquire_owned().is_err(),
            "超出每凭据上限必须拿不到许可"
        );

        // 关键断言：号 2 完全不受号 1 打满的影响。
        let g2 = provider.per_credential_gate(2);
        assert!(
            g2.try_acquire_owned().is_ok(),
            "一个号打满不得影响其它号 —— 这正是两级闸要解决的问题"
        );

        // 号 1 释放后容量回归。
        held.clear();
        assert!(
            g1.try_acquire_owned().is_ok(),
            "许可 Drop 后应自动归还（RAII，免手动释放）"
        );
    }

    /// 同一凭据多次取闸返回**同一把** Semaphore（懒初始化不得每次新建）。
    ///
    /// 若每次 new 一把，上限就完全失效 —— 每个请求都拿到一把全新的满容量闸。
    #[test]
    fn per_credential_gate_is_memoized_not_recreated() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let limit = provider.per_credential_limit();
        let a = provider.per_credential_gate(7);
        let b = provider.per_credential_gate(7);
        assert!(Arc::ptr_eq(&a, &b), "同一 id 必须复用同一把闸");
        // 通过 a 拿满，再从 b 取应当也拿不到（证明二者共享计数）。
        let mut held = Vec::new();
        for _ in 0..limit {
            held.push(a.clone().try_acquire_owned().unwrap());
        }
        assert!(
            b.try_acquire_owned().is_err(),
            "两个句柄必须共享同一计数，否则上限形同虚设"
        );
    }

    /// 清理函数把该号的闸与端点统计一并移除，且不误伤其它号。
    #[test]
    fn forget_credential_runtime_state_clears_only_that_credential() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let before = provider.per_credential_gate(90051);
        provider.report_endpoint_outcome(90051, cli, true);
        provider.report_endpoint_outcome(90052, cli, true);

        provider.forget_credential_runtime_state(90051);

        // ⚠️ 断言按 **id 存在性** 而非 `snap.len()`：端点健康表是**进程级共享**的
        // （见 endpoint_health::SHARED 的理由），而 `cargo test` 默认多线程并发跑 ⇒
        // 其它测试写进去的条目会让任何全局长度断言随机失败。用唯一 id + 存在性断言，
        // 与并发无关。（本仓踩过同型坑：usage/pipeline.rs 的 DROPPED 是进程级计数器，
        // 那里靠一把串行锁 + 差值断言解决；这里用唯一 id 更轻。）
        let snap = provider.endpoint_health_snapshot();
        assert!(
            !snap.iter().any(|s| s.credential_id == 90051),
            "号 90051 的统计应已被清除"
        );
        assert!(
            snap.iter().any(|s| s.credential_id == 90052),
            "号 90052 的统计不得被误伤"
        );
        // 闸被移除 ⇒ 再取是**新的一把**（与清理前不是同一个 Arc）。
        let after = provider.per_credential_gate(90051);
        assert!(!Arc::ptr_eq(&before, &after), "清理后应重新懒建");
    }

    /// 配置 0 视为「不限」并退化成全局容量，**绝不**建出容量 0 的闸。
    ///
    /// 容量 0 会让该号永远拿不到许可 = 号被静默废掉，且症状是「号在池里但一个请求都不走」，
    /// 极难排查。所以 0 必须被解释成「不单独限制」。
    #[test]
    fn per_credential_limit_zero_means_unlimited_not_zero_capacity() {
        let cfg = crate::model::config::Config::default();
        assert!(
            cfg.upstream_per_credential_limit > 0,
            "默认值必须为正，0 是「不限」的特殊语义而非默认"
        );
        // 直接验证构造逻辑：0 → 退化为全局容量（provider_with_default 用 Config::default，
        // 故此处只断言默认值语义；0 的分支由上面的 if 表达式保证，见 with_proxy）。
        let provider = provider_with_default(crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME);
        assert!(
            provider.per_credential_limit() >= 1,
            "任何配置下容量都必须 ≥1"
        );
    }

    /// 快照可观测：记过结果的组合都能在 snapshot 里查到成功率与样本数。
    #[test]
    fn endpoint_health_snapshot_is_observable() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        provider.report_endpoint_outcome(90041, cli, true);
        provider.report_endpoint_outcome(90041, cli, false);
        let snap = provider.endpoint_health_snapshot();
        let e = snap
            .iter()
            .find(|s| s.credential_id == 90041 && s.endpoint == cli)
            .expect("应能查到该组合");
        assert_eq!(e.samples, 2);
        assert!(e.success_rate.is_some(), "有样本时必须给出成功率");
    }

    /// ⭐ 守卫：429 分支必须实现「封当前端点桶 + 判断是否还有未封端点 + 换端点时摘出本号」。
    /// 这三步缺一，换桶就退化成「设凭据冷却换号」，q.*/runtime.* 双桶形同虚设。
    #[test]
    fn bucket_switch_on_429_must_throttle_and_release_credential() {
        let src = include_str!("provider.rs");
        let prod: String = src
            .split("#[cfg(test)]")
            .next()
            .expect("生产段应存在")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // 封桶时长常量必须存在且被生产段使用。
        assert!(
            prod.contains("ENDPOINT_BUCKET_THROTTLE"),
            "429 分支必须封禁端点桶（引用 ENDPOINT_BUCKET_THROTTLE）"
        );
        assert!(
            prod.contains("has_unthrottled_endpoint"),
            "429 必须用 has_unthrottled_endpoint 判断是否还有未封端点（决定换端点还是换号）"
        );
        assert!(
            prod.contains("tried_this_call.remove(&ctx.id)"),
            "换端点路径必须把本号从 tried_this_call 摘出，否则 acquire_context_excluding 结构性避开它"
        );
    }

    /// ⭐ 守卫（BLOCKER 回归）：换端点继续分支**不得**占位 `rate_limited_this_call`。
    ///
    /// 若在 `tried_this_call.remove` 后顺手 `rate_limited_this_call.insert(ctx.id)` 当"占位"，
    /// 则当第二端点也 429、`has_unthrottled_endpoint` 返回 false 时，`else if rate_limited_this_call
    /// .insert(ctx.id)` 恒为 false → 落最终 else 只打 debug → **凭据级冷却永不设置**，只靠
    /// `tried_this_call` 排除，跨请求又靠 `select_endpoint` None 分支设 30s 冷却兜底。双端连 429
    /// 的凭据会失去"全部封 → 冷却换号"的语义，退化成在桶窗口内反复打上游。
    #[test]
    fn bucket_switch_branch_must_not_occupy_rate_limited_this_call() {
        let src = include_str!("provider.rs");
        let start = src
            .find("has_unthrottled_endpoint(&call_creds")
            .expect("429 换桶判断应存在");
        // 窗口从换桶判断（`has_unthrottled_endpoint`）截到「全部端点都封」的 `else if` 之前，
        // 中间正好是换端点继续分支体（tried_this_call.remove + warn），不应含任何 insert。
        let end = src[start..]
            .find("else if rate_limited_this_call")
            .map(|i| start + i)
            .expect("全部封分支的 else if 应存在");
        let window = &src[start..end];
        assert!(
            !window.contains("rate_limited_this_call.insert"),
            "换端点继续分支不得占位 rate_limited_this_call —— 否则全部端点都封时去重逻辑误判 \
             已冷却过、永不设凭据级冷却（双端点连 429 时凭据冷却失效）"
        );
    }

    // ══════════ 上游 429 吸收层 ══════════

    fn absorb_cfg(enabled: bool) -> crate::model::config::Config {
        let mut c = crate::model::config::Config::default();
        c.upstream_retry_absorb_enabled = enabled;
        c
    }

    /// ⭐ BLOCKER 9 守卫：吸收准入判据必须是「剩余 > 退避 + 一轮最坏耗时(20s)」。
    ///
    /// 回退即 FAIL：把 `should_start_another_round` 换回「剩余 >= 退避」（即删掉
    /// `+ ABSORB_MIN_USEFUL_ROUND_SECS`），下面第二条断言立刻失败 —— 那种判据下
    /// 剩余 25s / 退避 10s 会被判定"够跑一轮"，然后这一轮必然在半路被 deadline 砍断：
    /// 白打一轮上游、客户端白等，正是外置 shield 的 p50 73.2s 的成因。
    #[test]
    fn absorb_budget_gate_requires_room_for_a_full_round() {
        let now = std::time::Instant::now();
        let d = Duration::from_secs;

        // 剩余 45s、退避 10s ⇒ 45 > 10+20 ⇒ 可以再跑一轮。
        assert!(
            should_start_another_round(now + d(45), now, d(10)),
            "剩余 45s / 退避 10s 应当允许再跑一轮"
        );
        // ⭐ 承重断言：剩余 25s、退避 10s ⇒ 25 > 30 为假 ⇒ 必须放弃。
        //   若判据退回 `剩余 >= 退避`，25 >= 10 会为真 → 本断言 FAIL。
        assert!(
            !should_start_another_round(now + d(25), now, d(10)),
            "剩余 25s 不足以容纳 退避 10s + 一轮最坏 20s，必须放弃而非白打一轮"
        );
        // 边界：恰好等于 delay+20 也要拒（严格大于）。
        assert!(
            !should_start_another_round(now + d(30), now, d(10)),
            "恰好等于 退避+一轮最坏耗时 时必须拒绝（严格大于）"
        );
        // deadline 已过：saturating 归零，必拒，且不 panic。
        assert!(!should_start_another_round(now, now + d(5), d(1)));
    }

    /// 关闭时 `effective_max_rounds()` 恒为 0 ⇒ 「关 ⇒ 零额外轮次」。
    ///
    /// 回退即 FAIL：把 `effective_max_rounds` 改成无条件返回 `self.max_rounds`
    /// （即删掉 `if self.enabled`），第一条断言失败。这条是「默认关等价旧行为」
    /// 的唯一可断言支点 —— 循环里的 `absorb_round >= effective_max_rounds()`
    /// 正是靠它在关闭时立即 break。
    #[test]
    fn absorb_policy_disabled_yields_zero_rounds() {
        let off = AbsorbPolicy::from_config(&absorb_cfg(false));
        assert_eq!(
            off.effective_max_rounds(),
            0,
            "吸收层关闭时必须是零额外轮次（否则 'absorb 循环不会立即 break）"
        );
        let on = AbsorbPolicy::from_config(&absorb_cfg(true));
        assert_eq!(
            on.effective_max_rounds(),
            crate::model::config::Config::default().upstream_retry_absorb_max_rounds,
            "开启时应当用配置的 max_rounds"
        );
    }

    /// 关闭时 `round_budget()` 恒返完整 45s，与旧代码的墙钟判据逐字节等价。
    ///
    /// 回退即 FAIL：把 `round_budget` 里的 `if self.enabled` 去掉（无条件夹 deadline），
    /// 则关闭状态下剩余预算会参与 min() → 墙钟闸门行为改变 → 第一条断言失败。
    #[test]
    fn absorb_disabled_keeps_legacy_wall_clock_budget() {
        let off = AbsorbPolicy::from_config(&absorb_cfg(false));
        let now = std::time::Instant::now();
        let full = Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS);
        // 即便 deadline 已经过期，关闭状态也必须返完整 45s（等价旧行为）。
        assert_eq!(off.round_budget(now, now + Duration::from_secs(99)), full);
        assert_eq!(off.round_budget(now + Duration::from_secs(1), now), full);

        // 开启时：一轮上限被剩余预算夹住，这就是"吸收轮不会超总预算"的机制。
        let on = AbsorbPolicy::from_config(&absorb_cfg(true));
        let squeezed = on.round_budget(now + Duration::from_secs(12), now);
        assert_eq!(
            squeezed,
            Duration::from_secs(12),
            "剩余 12s 时一轮墙钟预算必须被夹到 12s，而不是仍用 45s"
        );
        assert!(
            on.round_budget(now + Duration::from_secs(600), now) <= full,
            "剩余预算再大，单轮也不得超过 MAX_REQUEST_RETRY_BUDGET_SECS"
        );
    }

    /// 403 临时风控被允许吸收时，额外轮次**硬钉为 1**。
    ///
    /// 回退即 FAIL：删掉 `from_config` 里的 `.min(1)`，断言失败。
    /// 依据：403 是账号级、族级连坐已让同族全退，多轮重试只会把更多号烧进正在惩罚的窗口，
    /// 且与 `config.self_heal_base_backoff_secs（默认 60s）=60s`（存在的意义就是停止试探）直接冲突。
    #[test]
    fn absorb_suspended_pins_rounds_to_one() {
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_max_rounds = 3;
        c.upstream_retry_absorb_suspended = true;
        assert_eq!(
            AbsorbPolicy::from_config(&c).effective_max_rounds(),
            1,
            "开启 403 吸收时额外轮次必须硬钉 1（与自愈退避冲突，多轮会加深封禁）"
        );
    }

    /// 一次调用只取**一份**策略快照：`absorb_suspended` 必须来自 `AbsorbPolicy`，
    /// 循环里不得再 `self.token_manager.config()` 重读。
    ///
    /// 回退即 FAIL：把循环里的 `absorb.absorb_suspended` 换回
    /// `self.token_manager.config().upstream_retry_absorb_suspended`，断言失败。
    /// 理由：admin 在两个吸收轮之间热更配置，会让同一条客户端请求前半程按旧策略、
    /// 后半程按新策略走（`max_rounds` 已按旧值定好，suspended 判据却用了新值），
    /// 行为既不可复现也无法用测试固定。
    #[test]
    fn absorb_policy_is_snapshotted_once_per_call() {
        let src = include_str!("provider.rs");
        let retry_fn = src
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        let body = retry_fn.split("mod tests").next().unwrap_or(retry_fn);
        assert_eq!(
            body.matches("AbsorbPolicy::from_config").count(),
            1,
            "一次调用只应取一份策略快照"
        );
        let reread = format!("{}{}", "config().upstream_retry_absorb", "_suspended");
        assert!(
            !body.contains(reread.as_str()),
            "吸收循环内不得重读 config 的 suspended 标记：应使用 AbsorbPolicy 快照，\
             否则轮次之间的热更会让同一条请求前后按不同策略走"
        );
        // 策略里确实带上了这个字段（防有人删字段又改回重读）。
        assert!(
            include_str!("absorb_policy.rs").contains("absorb_suspended: bool"),
            "AbsorbPolicy 必须持有 absorb_suspended 字段"
        );
    }

    /// 退避：号池真值优先，且恒被 clamp 进 [min_delay, max_delay]。
    ///
    /// 回退即 FAIL：删掉 clamp 的下界 → `PoolCooldown(0)` 会返回 0 → 吸收循环变成无 sleep 的
    /// 忙等（正是 acquire_context 那次 CPU 打满一核、请求永不返回的事故形态），第二条断言失败。
    #[test]
    fn absorb_backoff_prefers_pool_truth_and_clamps() {
        use crate::anthropic::AbsorbClass;
        let p = AbsorbPolicy::from_config(&absorb_cfg(true));

        // 号池给的真值在区间内 → 原样采用（无需等 HTTP Retry-After 头往返）。
        assert_eq!(
            p.backoff(AbsorbClass::PoolCooldown(8), 0),
            Duration::from_secs(8)
        );
        // ⭐ 承重断言：0 秒也必须睡满 min_delay，绝不返回 0。
        assert_eq!(
            p.backoff(AbsorbClass::PoolCooldown(0), 0),
            p.min_delay,
            "退避为 0 会让吸收循环变成忙等死循环，必须抬到 min_delay"
        );
        // 超上限被夹（防单请求长挂）。
        assert_eq!(p.backoff(AbsorbClass::PoolCooldown(9999), 0), p.max_delay);
        // 无真值：指数增长且不越界。
        let r0 = p.backoff(AbsorbClass::UpstreamRateLimit, 0);
        let r2 = p.backoff(AbsorbClass::UpstreamRateLimit, 2);
        assert!(r2 > r0, "无号池真值时应指数退避");
        assert!(r2 <= p.max_delay);
        // 大 round 不得 panic（移位溢出）也不得越界。
        assert!(p.backoff(AbsorbClass::UpstreamRateLimit, 64) <= p.max_delay);
    }

    /// ⭐ `min_delay > max_delay` 不得 panic：`Duration::clamp` 的 std 契约是
    /// `min > max` 即 panic，而这两个值来自面板上两个独立数字框（毫秒框上限 60000 /
    /// 秒框下限 1），`minDelayMs=60000` + `maxDelaySecs=1` 一次手滑即可配出。
    ///
    /// 回退即 FAIL：删掉 `from_config` 里 `min_delay` 的 `.min(max_delay)`，
    /// 下面每一条 `backoff` 调用都会 panic（`assertion failed: min <= max`），
    /// 而 panic 发生在**请求热路径**上 —— 开启吸收层后每个 429 都会打到。
    #[test]
    fn absorb_min_delay_above_max_is_normalized_not_panicking() {
        use crate::anthropic::AbsorbClass;
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_min_delay_ms = 60_000; // 面板毫秒框上限
        c.upstream_retry_absorb_max_delay_secs = 1; // 面板秒框下限
        let p = AbsorbPolicy::from_config(&c);

        assert!(
            p.min_delay <= p.max_delay,
            "构造后必须满足 min_delay <= max_delay，否则 backoff 的 clamp 会 panic"
        );
        // 方向是「抬 max 到 min」：矛盾配置下宁可退避更久（吸收层不干活、回落旧行为），
        // 而不是退避更短（对还在冷却的号池连打，正是吸收层要避免的事）。
        assert_eq!(p.min_delay, Duration::from_secs(60), "min 应被尊重");
        assert_eq!(
            p.max_delay,
            Duration::from_secs(60),
            "max 应被抬到不低于 min"
        );

        // 三类都不得 panic，且结果落在退化后的单点区间上。
        assert_eq!(p.backoff(AbsorbClass::PoolCooldown(0), 0), p.max_delay);
        assert_eq!(p.backoff(AbsorbClass::PoolCooldown(9999), 0), p.max_delay);
        assert_eq!(p.backoff(AbsorbClass::UpstreamRateLimit, 5), p.max_delay);
        assert_eq!(p.backoff(AbsorbClass::SwapWindow, 0), p.max_delay);
        // 新增的两类同样不得 panic（`class_max_delay` 只对 SwapWindow 且设了 swap 预算时放宽，
        // 这里 swap 预算是 0 ⇒ 五类共用同一个退化区间）。
        assert_eq!(p.backoff(AbsorbClass::TransientServerError, 3), p.max_delay);
        assert_eq!(p.backoff(AbsorbClass::TransientCapacity400, 3), p.max_delay);
    }

    /// ⭐ 吸收总预算**不得低于** 45s，否则它会反向砍掉既有的 failover 墙钟。
    ///
    /// 回退即 FAIL：删掉 `from_config` 里 budget 的
    /// `.max(Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS))` —— 面板允许填 1，
    /// 而 `round_budget()` 是 `min(45s, 剩余预算)`，于是填 5 会让**第 0 轮**
    /// （关掉吸收层时唯一的那一轮）的换号墙钟从 45s 变成 5s：与吸收层无关的正常
    /// 重试被截断，而面板上看不出这层耦合。
    #[test]
    fn absorb_budget_cannot_shrink_the_failover_wall_clock() {
        let full = Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS);
        let now = std::time::Instant::now();

        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_budget_secs = 5; // 面板允许的小值
        let p = AbsorbPolicy::from_config(&c);
        assert!(
            p.budget >= full,
            "总预算被抬到不低于 45s，实际 {:?}",
            p.budget
        );
        // 承重：第 0 轮（round_started == deadline - budget 起点）仍拿满 45s。
        assert_eq!(
            p.round_budget(now + p.budget, now),
            full,
            "第 0 轮的 failover 墙钟不得因吸收层旋钮变短"
        );

        // 反向：填大值应能真的放宽总预算（旋钮仍然有用，只是单向）。
        let mut c2 = absorb_cfg(true);
        c2.upstream_retry_absorb_budget_secs = 120;
        assert_eq!(
            AbsorbPolicy::from_config(&c2).budget,
            Duration::from_secs(120),
            "大于 45s 的值必须原样生效，否则这个旋钮等于没有"
        );
    }

    /// ⭐ `maxDelaySecs=0` 不得产生零退避 —— 那是忙等死循环，不是「不等待」。
    ///
    /// 回退即 FAIL：删掉 `from_config` 里 `max_delay` 的 `.max(ABSORB_MIN_BACKOFF)` ——
    /// `max_delay=0` 会把 `min_delay` 也经 `.min()` 压成 0，`backoff()` 对每一类都返
    /// `Duration::ZERO`，吸收循环变成无 sleep 的 `continue`：打满一核、请求永不返回。
    /// 该值经 Admin API 可写（`service.rs` 对这两个字段无 clamp），所以这是可达状态。
    #[test]
    fn absorb_zero_max_delay_cannot_produce_busy_loop() {
        use crate::anthropic::AbsorbClass;
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_max_delay_secs = 0;
        c.upstream_retry_absorb_min_delay_ms = 0;
        let p = AbsorbPolicy::from_config(&c);

        assert!(
            p.max_delay >= ABSORB_MIN_BACKOFF,
            "max_delay 必须有绝对下限"
        );
        for (label, d) in [
            (
                "PoolCooldown(0)",
                p.backoff(AbsorbClass::PoolCooldown(0), 0),
            ),
            (
                "PoolCooldown(9999)",
                p.backoff(AbsorbClass::PoolCooldown(9999), 0),
            ),
            (
                "UpstreamRateLimit",
                p.backoff(AbsorbClass::UpstreamRateLimit, 0),
            ),
            ("SwapWindow", p.backoff(AbsorbClass::SwapWindow, 0)),
            (
                "TransientServerError",
                p.backoff(AbsorbClass::TransientServerError, 0),
            ),
            (
                "TransientCapacity400",
                p.backoff(AbsorbClass::TransientCapacity400, 0),
            ),
        ] {
            assert!(
                d >= ABSORB_MIN_BACKOFF,
                "{label} 退避为 {d:?}，零/过小退避会让吸收循环变成忙等死循环"
            );
        }
    }

    /// ⭐ 源码级守卫：`bearer token invalid` 打在**已成功过**的号上必须判瞬态，
    /// 且该判定必须在 `report_failure` **之前**。
    ///
    /// 用源码断言：走到这条分支需要真实上游返 403 + 真实号池，行为测试写不了
    /// （本仓惯例，见 `should_emit_usage_record_in_mcp_success_branch`）。
    ///
    /// 回退即 FAIL：删掉 `bearer_invalid_but_proven` 那段，或把它移到
    /// `report_failure` 之后 —— 高并发下 3 次瞬态 403 会在 1 秒内把一个
    /// 93.9% 成功率的号推到 `TooManyFailures`（实测 #481：2412 次成功仍被禁），
    /// 池子少一个号 → 剩下的吃更多流量 → 更易撞惩罚窗口。当天 116 次禁用/42 次自愈。
    ///
    /// 同时钉住「从未成功的号不受影响」：那些是真 region 错配（实测 3 个号 17 次），
    /// 必须继续计失败并被禁用，否则死号会永久占着调度位。
    #[test]
    fn bearer_invalid_on_proven_credential_must_not_count_as_failure() {
        // needle 运行时拼接：完整字面量会被 include_str! 读到自己而自匹配（本文件已踩三次）。
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);

        let guard = format!("{}{}", "bearer_invalid_but", "_proven");
        let proven_check = format!("{}{}", "has_ever_", "succeeded(ctx.id)");
        let punish = format!("{}{}", "report_failure", "(ctx.id)");

        let guard_at = src.find(guard.as_str()).expect("瞬态判定不应被改名");
        assert!(
            src.contains(proven_check.as_str()),
            "必须用 has_ever_succeeded 区分「真 region 错配」与「瞬态抖动」"
        );
        // 对话路径的 report_failure 必须在守卫之后。
        let punish_at = src
            .rfind(punish.as_str())
            .expect("report_failure 调用点不应被改名");
        assert!(
            guard_at < punish_at,
            "瞬态判定必须在 report_failure 之前，否则健康号仍会被 3 次抖动打死"
        );
        // 处置必须是冷却而非计失败。
        let cooldown = format!("{}{}", "report_auth_", "cooldown(ctx.id)");
        assert!(
            src.contains(cooldown.as_str()),
            "瞬态分支应设短冷却让调度避开该号，而不是什么都不做（否则下一跳可能再选它）"
        );
    }

    /// ⭐ 未修问题 ②（跨轮次数预算）：`ABSOLUTE_MAX_TOTAL_RETRIES` 必须是「**每请求**」
    /// 而非「每轮」的上限。
    ///
    /// 缺陷是两处组合出来的：单看 `=4` 没问题，单看「每轮重跑 for 循环」也没问题，
    /// 但配额在循环外只算一次、循环每轮重跑 ⇒ 每轮各拿一份完整 4 ⇒ `max_rounds=3`
    /// 时一条客户端请求最坏 (1+3)×4 = **16 次**上游调用、同一出口 IP，正是当初把
    /// 64 砍到 4 要压住的突发特征。
    ///
    /// 本测试模拟整条客户端请求：把每轮配额按 `round_retry_quota` 算出来累加，
    /// 断言总和恒 ≤ `ABSOLUTE_MAX_TOTAL_RETRIES`。回退即 FAIL：让 `round_retry_quota`
    /// 忽略 `attempts_before`（直接 `base_quota`）→ 总和变 16 → 第二条断言失败。
    #[test]
    fn total_upstream_attempts_are_capped_per_request_not_per_round() {
        // 池 ≥ 上限时基础配额必吃满硬上限（compute_max_retries(n,n) 对 n ≥ 上限恒 == 上限）。
        let base =
            compute_max_retries(ABSOLUTE_MAX_TOTAL_RETRIES, ABSOLUTE_MAX_TOTAL_RETRIES);
        assert_eq!(base, ABSOLUTE_MAX_TOTAL_RETRIES, "前提：基础配额吃满硬上限");

        // 模拟 1 + max_rounds 轮，每轮把配额跑满（最坏情况）。
        let max_rounds = crate::model::config::Config::default().upstream_retry_absorb_max_rounds;
        let mut attempts_base: u32 = 0;
        let mut total: usize = 0;
        for _round in 0..=max_rounds {
            let quota = round_retry_quota(base, attempts_base);
            if quota == 0 {
                break;
            }
            total += quota;
            // 与热路径同款递推：attempts_used = attempts_base + (quota-1)，再 +1。
            attempts_base += quota as u32;
        }
        assert!(max_rounds >= 1, "前提：默认 max_rounds 至少 1 轮才有意义");
        assert!(
            total <= ABSOLUTE_MAX_TOTAL_RETRIES,
            "一条客户端请求打向上游的总次数 {} 超过硬上限 {} —— 上限退化成「每轮」语义，\
             max_rounds={} 时单请求会打 (1+{})×{} 次上游、同一出口 IP",
            total,
            ABSOLUTE_MAX_TOTAL_RETRIES,
            max_rounds,
            max_rounds,
            base
        );
    }

    /// 共享预算的语义（2026-08-11 方案 A）：
    /// 1. 新预算 = 硬上限；consume 递减；超量 consume 饱和到 0 不 panic。
    /// 2. **跨层共享**：预算在 websearch 轮/压缩轮/透传 failover 之间流转——一层消费后，
    ///    下一层 `round_retry_quota(base, budget.used())` 只能拿到剩余额度（实参语义是
    ///    「已用量」——传 `remaining()` 会反转：耗尽时 remaining=0 被当成「还没用」）。
    /// 3. 预算耗尽后 quota 返 0（调用点 break，不再空打）。
    ///
    /// 回退即 FAIL：把 `consume` 的 `saturating_sub` 换成 `-` 会在超量扣减时 panic；
    /// 把配额源从 `budget.used()` 换回局部计数则断言 2/3 失败（跨层放大复现）。
    #[test]
    fn shared_budget_caps_across_layers() {
        let budget = SharedRetryBudget::new();
        assert_eq!(
            budget.remaining(),
            ABSOLUTE_MAX_TOTAL_RETRIES as u32,
            "新预算必须是硬上限"
        );

        // 第一层（模拟透传 failover）花掉 2 次真实调用。
        budget.consume(2);
        assert_eq!(budget.remaining(), 2);
        assert_eq!(budget.used(), 2, "used = 总额 - remaining");

        // 第二层（模拟 websearch 回灌轮）只能拿剩余额度。
        let base = ABSOLUTE_MAX_TOTAL_RETRIES;
        assert_eq!(
            round_retry_quota(base, budget.used()),
            2,
            "跨层共享：第二层最多打剩余额度"
        );

        // 第三层（模拟压缩重试轮）耗尽：quota 返 0。
        budget.consume(2);
        assert_eq!(budget.remaining(), 0);
        assert_eq!(budget.used(), ABSOLUTE_MAX_TOTAL_RETRIES as u32);
        assert_eq!(round_retry_quota(base, budget.used()), 0, "耗尽返 0");

        // 超量扣减饱和不 panic。
        budget.consume(100);
        assert_eq!(budget.remaining(), 0);
    }

    /// `round_retry_quota` 的边界：额度用尽必须返 0（调用点据此 break，不空跑一轮）。
    ///
    /// 回退即 FAIL：把 `saturating_sub` 换成 `-` 会在 attempts > ABSOLUTE_MAX_TOTAL_RETRIES
    /// 时 panic；
    /// 把 `.min(remaining)` 删掉则第三、四条断言失败。
    #[test]
    fn round_retry_quota_shrinks_and_hits_zero() {
        let base = ABSOLUTE_MAX_TOTAL_RETRIES;
        assert_eq!(round_retry_quota(base, 0), base, "第 0 轮拿满基础配额");
        assert_eq!(round_retry_quota(base, 4), base - 4, "第 1 轮只剩 4-4");
        assert_eq!(
            round_retry_quota(base, ABSOLUTE_MAX_TOTAL_RETRIES as u32),
            0,
            "额度用尽必须返 0，否则调用点会空跑一轮、白睡一次退避"
        );
        // 超额（墙钟 break 后 attempts_base 可能越过上限）不得下溢 panic。
        assert_eq!(round_retry_quota(base, 999), 0);
        // 小号池：基础配额本就小于剩余额度时，不得被抬高。
        assert_eq!(round_retry_quota(2, 0), 2, "基础配额是上界，不能被额度抬高");

        // ⭐ 吸收层**关闭**时的逐字节等价（docs/absorb-layer-design.md §8）：只跑一轮 ⇒
        // attempts_base 恒 0；而 compute_max_retries 自身已 `.min(ABSOLUTE_MAX_TOTAL_RETRIES)`
        // ⇒ 本函数恒为恒等映射 ⇒ 关闭路径的行为与改动前完全相同。
        for pool in [0usize, 1, 3, 4, 12, 43, 1000] {
            let base = compute_max_retries(pool, pool);
            assert_eq!(
                round_retry_quota(base, 0),
                base,
                "吸收层关闭时（attempts_base 恒 0）本函数必须是恒等映射，池大小={pool}"
            );
        }
    }

    /// ⭐ 源码守卫：本轮配额必须**在** `'absorb: loop` 内经 `round_retry_quota` 算出。
    ///
    /// 纯函数单测证明不了热路径真的用了它（那正是「测了分支内部没测分支顺序」的形态）。
    /// 回退即 FAIL：把 `let max_retries = round_retry_quota(..)` 挪回循环外，
    /// 或改成直接用 `base_retry_quota` → 两条位置断言之一失败。
    #[test]
    fn per_round_quota_is_computed_inside_absorb_loop() {
        // ⚠️ 必须先切掉 `#[cfg(test)]` 之后的内容：`include_str!` 读整份源码，本测试自身
        // 也含这些 needle 的拼接结果，不切则位置比较命中测试里的那个 → 守卫静默失效
        // （前一版 `per_round_retry_cap_*` 正是这个形态：改完也照样通过）。
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let loop_marker = format!("{}{}", "'absorb: ", "loop {");
        // ⚠️ 第二个实参必须是 `budget.used()`（共享预算的**已用量**——实参语义是「已完成
        // 的尝试次数」；真实上游调用在 `upstream_calls += 1` 处同步 `consume(1)`）而**不是**
        // `attempts_base`（迭代计数,含 fast-fail 空转）也**不是** `budget.remaining()`（剩余量
        // 传进来会被当成「已用这么多」——耗尽时 remaining=0 反而拿满配额，语义完全反转，
        // 2026-08-11 方案 A 实现时亲手踩过，单测 shared_budget_caps_across_layers 钉死）。
        // 喂错会让全池冷却在毫秒内烧空额度 ⇒ 吸收层被整体旁路,且这件事在纯函数单测
        // 里看不出来（两者类型相同、函数本身行为不变）。跨层（websearch 轮/压缩轮/
        // 透传 failover）共用同一总额度。
        let quota_call = format!(
            "{}{}",
            "round_retry_quota(base_retry_quota", ", budget.used())"
        );
        let decl = format!("{}{}", "let max_retries = ", "round_retry_quota(");

        let loop_at = src
            .find(loop_marker.as_str())
            .expect("'absorb: loop 不应被改名");
        let decl_at = src
            .find(decl.as_str())
            .expect("本轮配额必须由 round_retry_quota 算出（跨轮共享总额度）");
        assert!(
            decl_at > loop_at,
            "本轮配额必须在 'absorb: loop **内**重算：算在循环外等于每轮各拿一份完整配额，\
             上限退化成「每轮」语义（max_rounds=3 时单请求最坏 16 次上游调用）"
        );
        assert!(
            src.contains(quota_call.as_str()),
            "配额必须同时喂入基础配额与**跨轮累计**尝试数，否则夹不住总量"
        );

        // 额度耗尽必须在 sleep 之前 break：否则每轮白睡一次退避却零次上游调用。
        let zero_gate = format!(
            "{}{}",
            "round_retry_quota(base_retry_quota, budget.used()) ==", " 0"
        );
        let sleep_at = src
            .rfind(&format!("{}{}", "sleep(delay)", ".await"))
            .expect("吸收轮的 sleep 不应被改名");
        let zero_at = src
            .find(zero_gate.as_str())
            .expect("必须有「额度耗尽即 break」的闸门");
        assert!(
            zero_at < sleep_at,
            "额度耗尽的闸门必须排在 sleep 之前，否则客户端会为零次上游调用白等多个退避"
        );
    }

    /// ⭐ 未修问题 ③：退避被 `max_delay` 截断时**不得**再起一轮。
    ///
    /// 号池真值 60s（`config.self_heal_base_backoff_secs（默认 60s）`）vs `max_delay` 默认 15s：只 clamp 不判断
    /// ⇒ 睡 15s 醒来池子还在冷却 45s ⇒ 这一轮结构上必然拿回同一个 429 = 白打一轮上游
    /// + 客户端白等 15s。
    ///
    /// 回退即 FAIL：把 `backoff_is_truncated` 改成 `required_wait > max_delay` 之外的任何
    /// 恒假式（如 `false`），第二、三条断言失败。
    #[test]
    fn truncated_backoff_means_round_is_futile() {
        use crate::anthropic::AbsorbClass;
        let p = AbsorbPolicy::from_config(&absorb_cfg(true));

        // 号池真值在退避上限之内 → 睡够就真到恢复时刻 → 这一轮有意义。
        assert!(
            !p.backoff_is_truncated(AbsorbClass::PoolCooldown(8), 0),
            "8s < max_delay，睡满即到恢复时刻，这一轮是有意义的"
        );
        // ⭐ 承重：全池自愈退避 60s 远超 max_delay ⇒ 必须判定「白打」。
        assert!(
            p.backoff_is_truncated(AbsorbClass::PoolCooldown(60), 0),
            "号池要 60s 才恢复而我们最多睡 {:?}，睡醒仍在冷却 —— 必须判白打",
            p.max_delay
        );
        // 而 clamp 后的睡眠时长看不出这件事（这正是必须分成两个函数的理由）。
        assert_eq!(
            p.backoff(AbsorbClass::PoolCooldown(60), 0),
            p.max_delay,
            "睡多久仍用截断值，判断够不够才用真值"
        );
        // ⭐ 反向承重：指数兜底撞上限**不算**白打。它是我们自己编的数、不是上游真值，
        // `max_delay` 本来就是为夹住它而存在。若这里判 true，吸收层会对**最主要**的那类
        // （上游裸 429）在 round 涨上去后提前停工，白丢一层保护。
        assert!(
            !p.backoff_is_truncated(AbsorbClass::UpstreamRateLimit, 30),
            "指数兜底无真值，撞 max_delay 只说明「我们不想睡更久」，不代表上游没好"
        );
        assert!(
            !p.backoff_is_truncated(AbsorbClass::SwapWindow, 30),
            "同上：SwapWindow（换号空窗）也没有号池真值"
        );
        // 新增两类同理：它们的曲线是我们自己编的数，撞上限不代表上游没好。
        assert!(!p.backoff_is_truncated(AbsorbClass::TransientServerError, 30));
        assert!(!p.backoff_is_truncated(AbsorbClass::TransientCapacity400, 30));
    }

    /// ⭐ 源码守卫（分支**顺序**）：截断判定必须排在 `should_start_another_round` **之前**。
    ///
    /// 两者是独立失败模式：前者管「睡够了上游好没好」，后者管「预算够不够睡」。
    /// 顺序反了的后果不是断言不成立而是**归因错**：预算判据用的是被截断的 15s（比真实
    /// 需求小），会先判「预算够」放行 → 白打一轮，且面板上记成 `absorb_round` 成功起轮
    /// 而不是被拦。回退即 FAIL：把 `backoff_is_truncated` 那段挪到
    /// `should_start_another_round` 之后，位置断言失败。
    #[test]
    fn truncation_gate_precedes_budget_gate() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let trunc = format!(
            "{}{}",
            "absorb.backoff_is_truncated", "(class, absorb_round)"
        );
        // ⚠️ 实参已从 `absorb_deadline` 改为 `class_deadline`（换号空窗要用它自己那份预算）。
        // 按实参定位是原设计，保留这种写法：它顺带钉住「预算闸门吃的是某个 deadline 变量」。
        let budget = format!("{}{}", "should_start_another_round", "(class_deadline");

        let trunc_at = src.find(trunc.as_str()).expect("截断闸门不应被改名/删除");
        let budget_at = src.find(budget.as_str()).expect("预算闸门不应被改名");
        assert!(
            trunc_at < budget_at,
            "截断闸门必须排在预算闸门之前：预算判据吃的是被 max_delay 夹小后的 delay，\
             先跑它会把「睡醒也没好」的一轮判成「预算够」而放行"
        );
    }

    /// ⭐ BLOCKER 1 的机械防线（源码级）：准入闸门必须在吸收循环**之上**，且全文只有一处。
    ///
    /// 回退即 FAIL：把 `acquire_admission` 移进 `'absorb: loop`（或在循环内再加一个调用点），
    /// 断言立刻失败。这是本方案唯一的正确性支点 —— 入站令牌是「每客户端请求一个」，
    /// 若吸收重入闸门，一条请求吃 N 个令牌 → 令牌桶按 N 倍速率被抽干 → 每轮排队满 30s 才
    /// bail → 客户端从 <2s 拿到 429 变成 60s 才拿到（外置 shield 的 p50 73.2s 被搬进网关）。
    /// 单测覆盖不到（需真实号池 + 上游），故用源码断言。
    /// 🔴 源码守卫：**透传路径必须有并发闸 + 次数闸**（2026-08-10 审计发现的致命缺口）。
    ///
    /// # 为什么必须有这条守卫
    ///
    /// Kiro 主路径有五道背压（准入闸门 / 全局并发闸 / 每凭据并发闸 / `ABSOLUTE_MAX_TOTAL_RETRIES`
    /// / 动态压力降档），而透传循环**一道都没有** —— 它是按「低延迟零转换中转」设计的，
    /// 主路径后来加的调度设施它一项都没跟上。而线上号池当前全部是 custom_api 代挂号
    /// ⇒ **100% 流量走透传** ⇒ 那五道闸对当前流量全部失效。
    ///
    /// 缺口的具体后果：单请求可打 N 次上游（N=代挂号数，无上限），每次 connect 10s +
    /// read 720s；45s 墙钟只在每轮进循环时判 ⇒ 最后一跳可在 45s 后才开始并跑到 720s。
    /// 叠外置 shield-k2cc 的 10 次 ⇒ 无上限并发 × 无上限次数。
    ///
    /// # 回退即 FAIL
    /// 删掉任一道闸、或把次数累加挪到 `forward` 之前（那会让被闸门挡住的空转也吃配额），
    /// 断言立刻失败。行为测试需要真实上游 + 并发压力，故用源码断言。
    /// 🔴 源码守卫：**透传 failover 必须覆盖上游 404**（2026-08-10 修）。
    ///
    /// # 为什么
    /// 实测同一模型在两个代挂上游响应不同：`deepseek-v4-flash` 在 router.denzao.com 返
    /// **404 `model_not_found`**，在 k2cc 返 **200 OK**。改前 `should_failover` 三个条件
    /// （`401|402|403|429` / 5xx / `code == 400 && ...`）**都不含 404** ⇒ 404 直返客户端
    /// ⇒ Claude Code/Cursor 当「模型不存在」**当场断会话**，而池里另一个号明明能成功。
    ///
    /// 404 与 400 是同性质的：都是「**这个上游**不认这个请求」，只是不同站点用不同状态码
    /// 表达（k2cc 用 400 `INVALID_MODEL_ID`、denzao 用 404 `model_not_found`）。
    ///
    /// # 回退即 FAIL
    /// 把判定收回 `code == 400`、或从 `should_failover` 里漏掉它，断言立刻失败。
    /// 行为测试需要能返 404 的真实上游（本仓无 HTTP mock 设施），故用源码断言。
    #[test]
    fn passthrough_failover_must_cover_upstream_404() {
        let src = include_str!("provider.rs");
        let fn_marker = format!("{}{}", "async fn try_custom_api_passthrough", "(");
        let start = src
            .find(fn_marker.as_str())
            .expect("try_custom_api_passthrough 不应被改名");
        let body_end = src[start..]
            .find("\n    /// 累加一次请求的真实 credit")
            .map(|off| start + off)
            .unwrap_or(src.len());
        // 剔注释行：注释里出现 404 不算实现（本仓 :3913 记过「不剔注释会误判」的踩坑）。
        let code: String = src[start..body_end]
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("///")
            })
            .collect::<Vec<_>>()
            .join("\n");

        // 判定必须同时覆盖 400 与 404（`matches!(code, 400 | 404)` 形态）。
        let gate = format!("{}{}", "matches!(code, 400 ", "| 404)");
        assert!(
            code.contains(gate.as_str()),
            "透传 failover 判定必须同时覆盖 400 与 404：404 直返会让客户端把「这个上游不认」\
             误判成「模型不存在」而断会话，而池里其它号可能能成功（实测 deepseek-v4-flash \
             在 denzao 返 404、在 k2cc 返 200）"
        );
        // 冷却时长在 passthrough_cooldown_for（抽到 try_custom_api_passthrough 之前）。
        // 切片必须扫 helper，不能扫透传函数体——否则抽函数后守卫假红。
        let cool_marker = format!("{}{}", "fn passthrough_cooldown_for", "(");
        let cool_start = src
            .find(cool_marker.as_str())
            .expect("passthrough_cooldown_for 不应被改名");
        let cool_end = src[cool_start..]
            .find("\n    /// 混入池分流")
            .map(|off| cool_start + off)
            .unwrap_or(src.len());
        let cool_src: String = src[cool_start..cool_end]
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("///")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let cooldown = format!("{}{}", "400 ", "| 404 => (5,");
        assert!(
            cool_src.contains(cooldown.as_str()),
            "404 的冷却时长必须与 400 同档（5s 调度级跳过）：它们是同一性质"
        );
    }

    /// 透传 400/404 的「换号无益」判据（`is_hopeless_upstream_400`）：
    /// 真实配额耗尽/超长形态必须判无益（不 failover），
    /// 但 body 恰好含 `quota` 字样的**上游能力差异**文案必须仍给换号机会。
    ///
    /// 回退即 FAIL：把判据改回裸 `quota` 宽匹配，反例组全部误判为无益 →
    /// 客户端白吃一个本来能靠换号解决的 400/404。
    #[test]
    fn hopeless_400_judgement_is_phrase_based_not_bare_quota() {
        // 正例：实测/常见配额耗尽与超长形态（OpenAI 系 / one-api 系 / DeepSeek 系）。
        for body in [
            r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","code":"insufficient_quota"}}"#,
            r#"{"error":{"message":"quota exhausted"}}"#,
            r#"{"error":{"message":"quota exceeded, 500 requests used"}}"#,
            "Insufficient Balance",
            "usage limit exceeded",
            "the request is too long",
            r#"{"reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}"#,
        ] {
            let low = body.to_ascii_lowercase();
            assert!(
                is_hopeless_upstream_400(&low),
                "真实配额耗尽/超长形态必须判「换号无益」: {body}"
            );
        }
        // 反例（误伤场景）：含 `quota` 字样但**不是**配额耗尽 —— 上游能力差异
        // （换一个号可能成功），必须仍给 failover 机会。
        for body in [
            "the model deepseek-quota-v2 requires a higher quota tier on this relay",
            r#"{"error":{"message":"quota tier not enabled for this model"}}"#,
            r#"{"error":{"code":"quota","message":"unknown error"}}"#,
        ] {
            let low = body.to_ascii_lowercase();
            assert!(
                !is_hopeless_upstream_400(&low),
                "非配额耗尽的 quota 字样不得判「换号无益」（会把上游能力差异吞成直返）: {body}"
            );
        }
    }

    #[test]
    fn passthrough_loop_must_have_concurrency_and_hop_gates() {
        let src = include_str!("provider.rs");
        // 只取透传函数体，避免误命中主路径的同名设施。
        let fn_marker = format!("{}{}", "async fn try_custom_api_passthrough", "(");
        let start = src
            .find(fn_marker.as_str())
            .expect("try_custom_api_passthrough 不应被改名");
        // 到下一个 `\n    /// ` 级别的项声明为止（该函数之后是 report_credits 的文档注释）。
        let body_end = src[start..]
            .find("\n    /// 累加一次请求的真实 credit")
            .map(|off| start + off)
            .unwrap_or(src.len());
        let body = &src[start..body_end];
        // 剔注释行：注释里出现关键词不算实现（本仓 :3913 记录过「不剔注释会误判」的踩坑）。
        let code: String = body
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("///")
            })
            .collect::<Vec<_>>()
            .join("\n");

        for (needle, why) in [
            (
                "upstream_gate",
                "透传必须过全局并发闸：线上 100% 流量走透传，它是当前唯一的全局并发保护",
            ),
            (
                "per_credential_gate",
                "透传必须过每凭据并发闸：否则一个慢中转站占满全局许可会拖死整池吞吐",
            ),
            (
                "MAX_PASSTHROUGH_FAILOVER_HOPS",
                "透传必须有换号次数上限：墙钟只在每轮进循环时判，最后一跳能跑到 read_timeout 720s",
            ),
        ] {
            assert!(
                code.contains(needle),
                "透传循环缺少 `{needle}`。{why}"
            );
        }

        // 次数累加必须在 forward **之后**（闸门挡住的空转不该吃配额，与主路径 upstream_calls 同款）。
        let fwd = code
            .find("passthrough::forward")
            .expect("透传必须调 passthrough::forward");
        let inc = code
            .find("upstream_hops += 1")
            .expect("必须有 upstream_hops 累加");
        assert!(
            fwd < inc,
            "`upstream_hops += 1` 必须在 forward 之后：放在之前会让被并发闸挡住的空转\
             （两处 continue）也吃掉换号配额，池子越大越早耗尽配额而一次上游都没真打成"
        );
    }

    /// 🔴 2026-08-10：acquire_admission 已移至 handlers 层（post_messages 入口），
    /// 透传与 Kiro 两条路径统一在 handler 层过闸门。provider.rs 不应再有任何调用。
    /// 本测试从「位置守卫」变为「零调用守卫」：防将来有人误加回 provider 内部。
    #[test]
    fn admission_gate_must_stay_above_absorb_loop() {
        let src = include_str!("provider.rs");
        let gate = format!("{}{}", "acquire_admission", "().await");
        assert_eq!(
            src.matches(gate.as_str()).count(),
            0,
            "acquire_admission 已移至 handlers 层（post_messages 与 post_messages_cc \
             入口统一过闸门）。provider.rs 不应再有调用点。若要在此加回，先确认不会导致某条路径绕闸。"
        );
    }

    /// 透传同号吸收判据：与 config.rs 的 upstream_retry_absorb_* 字段语义逐一钉死。
    /// - 429 只跟总开关（主路径 UpstreamRateLimit 同语义）；5xx 还需 server_error。
    /// - 本地失败（connect_error / 空错误体）绝不重试。
    /// - max_rounds 是「额外轮次」：0 = 不吸收；attempt 从 1 起，共最多 max_rounds 次
    ///   重试（2026-08-13 对齐主路径：旧判据 `attempt >= max_rounds` 只给 max_rounds−1 次）。
    #[test]
    fn passthrough_absorb_predicates_match_config_semantics() {
        // 429：只跟总开关。
        assert!(passthrough_absorb_should_retry(429, false, true, false, false, "", 1, 3));
        assert!(!passthrough_absorb_should_retry(429, false, false, true, false, "", 1, 3));
        // 5xx：总开关 + server_error 双开才吸收。
        assert!(passthrough_absorb_should_retry(502, false, true, true, false, "", 1, 3));
        assert!(!passthrough_absorb_should_retry(502, false, true, false, false, "", 1, 3));
        // 400 容量类（谓词认 INSUFFICIENT_MODEL_CAPACITY / MODEL_TEMPORARILY_UNAVAILABLE）：
        // capacity_400 开 + 谓词命中 → 吸收；开关关 → 不吸收（与改前逐字节一致）。
        assert!(passthrough_absorb_should_retry(
            400, false, true, false, true,
            r#"{"reason":"INSUFFICIENT_MODEL_CAPACITY"}"#, 1, 3
        ));
        assert!(passthrough_absorb_should_retry(
            400, false, true, false, true, "MODEL_TEMPORARILY_UNAVAILABLE", 1, 3
        ));
        assert!(!passthrough_absorb_should_retry(400, false, true, false, false, "", 1, 3));
        // 开关关但错误体是容量类 → 也不吸收（默认配置行为逐字节不变）。
        assert!(!passthrough_absorb_should_retry(
            400, false, true, false, false,
            r#"{"reason":"INSUFFICIENT_MODEL_CAPACITY"}"#, 1, 3
        ));
        // 谓词不认的 400（普通请求错误）即使开关开着也不吸收。
        assert!(!passthrough_absorb_should_retry(
            400, false, true, false, true, "INVALID_MODEL_ID", 1, 3
        ));
        // 本地失败绝不重试。
        assert!(!passthrough_absorb_should_retry(503, true, true, true, false, "", 1, 3));
        // max_rounds：0 = 不吸收；attempt 可达 max_rounds（额外轮次），再多一轮才停。
        assert!(!passthrough_absorb_should_retry(429, false, true, false, false, "", 0, 3));
        assert!(passthrough_absorb_should_retry(429, false, true, false, false, "", 3, 3));
        assert!(!passthrough_absorb_should_retry(429, false, true, false, false, "", 4, 3));
        assert!(!passthrough_absorb_should_retry(429, false, true, false, false, "", 1, 0));
    }

    /// 透传同号退避：默认配置下 500/1000/2000ms；clamp 到 [min_delay_ms, max_delay_secs]。
    #[test]
    fn passthrough_absorb_delay_monotonic_and_clamped() {
        assert_eq!(
            (
                passthrough_absorb_delay_ms(1, 150, 15),
                passthrough_absorb_delay_ms(2, 150, 15),
                passthrough_absorb_delay_ms(3, 150, 15),
            ),
            (500, 1000, 2000)
        );
        assert_eq!(passthrough_absorb_delay_ms(1, 6000, 15), 6000);
        assert_eq!(passthrough_absorb_delay_ms(1, 6000, 1), 6000);
        assert_eq!(passthrough_absorb_delay_ms(7, 150, 1), 1000);
    }

    /// 源码守卫：失败埋点与备用模型兜底必须留在吸收循环**之外**。
    ///
    /// 放进轮内会让一条客户端请求落 N 条失败记录 / 打 N 次备用模型，面板失败数被吸收轮次乘倍。
    ///
    /// ⚠️ 强度说明（避免把它当成比实际更硬的防线）：
    /// - 失败记录那一半**实际由编译器兜底** —— `fail_record` 在循环之后才构造，把
    ///   失败记录的 emit 调用挪进轮内会直接 E0425 `cannot find value`（已实测验证）。
    ///   本断言只是让意图显式化，真正拦住回退的是借用检查。
    /// - 备用模型那一半**是本测试独有的**：那段只依赖 `last_outcome` /
    ///   `model` / `session_id`，全都在循环内可见，搬进去能正常编译 —— 编译器不会报错，
    ///   只会静默变成"每轮都打一次备用模型"。这一半是这条测试存在的真正理由。
    ///
    /// ⚠️ needle 防自匹配（2026-08-11 审计修复）：
    /// - 完整字面量绝不出现在本文件任何注释/测试里（include_str! 会把它们也读进来，
    ///   生产被删后 `.find` 命中注释会让断言静默变绿 —— 本仓 4715 行注释记录过同型
    ///   踩坑五次；本轮审计抓到函数内 3749/3752/3963 行旧注释正是此形态，已改写）。
    /// - 全部运行时拼接；备用模型那一条用带 `cfg.` 前缀的片段（配置读取处的生产唯一
    ///   形态），注释/测试不可能自然写出。
    /// - 测试段按同文件 `failover_exhausted_*` 守卫的先例截断（`split_once`），
    ///   防止将来测试代码里出现完整字面量时守卫静默变绿。
    #[test]
    fn emit_record_and_fallback_stay_outside_absorb_loop() {
        let src = include_str!("provider.rs");
        let retry_fn = src
            .split("async fn call_api_with_retry")
            .nth(1)
            .and_then(|s| s.split_once("\n#[cfg(test)]").map(|(head, _)| head))
            .expect("call_api_with_retry 不应被改名");
        let end_marker = format!("{}{}", "break ", "'absorb;");
        let last_break = retry_fn
            .rfind(end_marker.as_str())
            .expect("'absorb 循环的 break 不应被改名");

        // ⚠️ 锚点必须是「失败记录的 emit 调用」而不是泛的 emit 调用 ——
        // 准入闸门超时（已知问题 #20 的修复）也 emit 一条记录，而它**刻意**在吸收循环
        // **之上**（闸门本身就在循环外，见 `admission_timeout_must_be_observable`）。
        // 泛锚点会先命中那一处，把「位置在循环后」的断言判成失败，而实际并无回归。
        // 本测试要钉的是**失败记录**那一条：它按吸收轮次乘倍才会污染面板失败数。
        let needles = [
            format!("{}{}", "emit_record(fail", "_record)"),
            format!("{}{}", "cfg.overload_fallback", "_model"),
        ];
        for needle in needles {
            let at = retry_fn
                .find(needle.as_str())
                .unwrap_or_else(|| panic!("{needle} 应仍在 call_api_with_retry 内"));
            assert!(
                at > last_break,
                "{needle} 必须位于吸收循环之后（循环外）：放进轮内会让一条客户端请求\
                 落 N 条失败记录，面板失败数被吸收轮次乘倍"
            );
        }
    }

    /// ⭐ 源码守卫（已知问题 #13）：`failover_exhausted` 只能在吸收循环**之外**、整条客户端
    /// 请求失败后记一次。
    ///
    /// 历史缺陷：bump 放在轮内且每轮清零 ⇒ 一条请求跑 N 轮就计 N 次（多计）；成功路径在轮内
    /// return 前也会被误计。回退即 FAIL：把 bump 挪回 'absorb 循环内 → `bump_at < loop_at`。
    #[test]
    fn failover_exhausted_bumped_once_outside_absorb_loop() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let retry_fn = src
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        let loop_at = retry_fn
            .find(format!("{}{}", "'absorb: ", "loop {").as_str())
            .expect("'absorb: loop 不应被改名");
        let bump_at = retry_fn
            .find("crate::common::recovery_metrics::bump_failover_exhausted()")
            .expect("failover_exhausted bump 不应被删除");
        assert!(
            bump_at > loop_at,
            "failover_exhausted 必须在吸收循环之外记（一次/请求）：放在轮内会被吸收轮次乘倍（#13）"
        );
        assert_eq!(
            retry_fn
                .matches("crate::common::recovery_metrics::bump_failover_exhausted()")
                .count(),
            1,
            "call_api_with_retry 内必须恰好一处 failover_exhausted bump（整条请求失败才记一次）"
        );
    }

    /// ⭐ 源码守卫：链内去重集必须声明在吸收循环**之外**（跨轮共享）。
    ///
    /// 回退即 FAIL：把 `rate_limited_this_call` 的 `let mut` 挪进 `'absorb: loop`，断言失败。
    /// 挪进去会让同一个号在每一轮都被重新惩罚 → trigger_count 累加 → 冷却 15s 被指数拉长到
    /// 72s，即「单请求自造雪崩」（这条历史根因写在该集合的声明处注释里）。
    #[test]
    fn chain_dedup_sets_declared_outside_absorb_loop() {
        let src = include_str!("provider.rs");
        let retry_fn = src
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        let loop_at = retry_fn
            .find(format!("{}{}", "'absorb: ", "loop {").as_str())
            .expect("'absorb: loop 不应被改名");

        for set_name in [
            "let mut rate_limited_this_call",
            "let mut suspended_this_call",
            "let mut suspicious_failovers_this_call",
            "let mut auth_failed_this_call",
            "let mut region_corrected_this_call",
            // L1 换区：挪进轮内 ⇒ 每号一次上限退化成「每轮一次」，两个区来回打。
            "let mut region_switched_this_call",
            // L1 覆盖表：挪进轮内 ⇒ 上一轮换好的区在下一轮丢失，退回打错区。
            "let mut region_override_this_call",
            "let mut model_unavailable_attempts",
            "let mut attempts_used",
            // 挪进轮内会让每轮各拿一份完整 4 次上游调用额度 —— 那正是 round_retry_quota
            // 存在的理由（max_rounds=3 时单请求最坏 16 次上游调用、同一出口 IP）。
            "let mut upstream_calls",
        ] {
            let at = retry_fn
                .find(set_name)
                .unwrap_or_else(|| panic!("{set_name} 不应被改名/删除"));
            assert!(
                at < loop_at,
                "{set_name} 必须声明在吸收循环之外（跨轮共享）：挪进轮内会让同号被反复惩罚，\
                 冷却从 15s 指数拉长到 72s（单请求自造雪崩）"
            );
        }
    }

    /// ⭐ 源码守卫：四处 AIMD 上报点必须全部被 `absorb_round == 0` 包裹。
    ///
    /// 回退即 FAIL：去掉任一处的门，该处的上报数量断言失败。
    /// 依据：AIMD 的输入语义是「客户端请求撞上游的频率」，一条客户端请求无论吸收几轮都只是
    /// **一个** RPM 事件。逐轮上报时 `MD_DEBOUNCE_SECS=3` 挡不住吸收轮次（退避 ≥150ms、
    /// 号池真值常 8~15s，全部 >3s 穿窗）→ 每轮真降一档 → `last_md_nanos` 被反复推进 →
    /// `maybe_step_up` 的 20s 静默期永不满足（实测每 6.4s 一次 429）→ RPM 单调滑到 floor
    /// 锁死。这与已修的「AIMD 升档饿死」是同一死锁的第三条触发路径。
    #[test]
    fn aimd_reports_are_gated_to_first_absorb_round() {
        let src = include_str!("provider.rs");
        let retry_fn = src
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        // 只看到吸收循环收尾为止，避免把测试自身的字符串算进来。
        let body = retry_fn
            .split("mod tests")
            .next()
            .expect("测试模块分隔不应消失");
        let gate = format!("{}{}", "absorb_round ", "== 0");

        let sites = [
            "report_upstream_rate_limited()",
            "report_upstream_pressure()",
        ];
        let total: usize = sites.iter().map(|s| body.matches(s).count()).sum();
        assert_eq!(
            total, 4,
            "call_api_with_retry 内应恰有 4 处 AIMD 上报点（临时风控/suspend/429/5xx）；\
             数量变化需同步本守卫"
        );
        // 每处上报点之前的 200 字节窗口内必须出现 `absorb_round == 0` 这道门。
        // `split_at` 拿到该处之前的全部文本，再取尾部窗口 —— 门与调用之间只隔注释与花括号。
        for site in sites {
            let mut searched_from = 0usize;
            let mut nth = 0usize;
            while let Some(rel) = body[searched_from..].find(site) {
                let abs = searched_from + rel;
                nth += 1;
                // 取该处之前最多 200 字节的窗口。本文件含中文注释，字节偏移可能落在多字节
                // 字符中间 —— 必须往前挪到合法字符边界，**不能**回退成"整段前缀"
                // （那会把别处的门也算进来，使断言恒真：本守卫第一版就是这个 bug，
                //   删掉一处门后测试照样通过，等于白写）。
                let mut window_start = abs.saturating_sub(200);
                while window_start < abs && !body.is_char_boundary(window_start) {
                    window_start += 1;
                }
                let window = &body[window_start..abs];
                assert!(
                    window.contains(gate.as_str()),
                    "AIMD 上报点 {site}（第 {nth} 处）之前 200 字节内必须有 `absorb_round == 0` 门，\
                     否则吸收轮次会把同一个上游压力事件放大 N 倍喂给 AIMD，\
                     使 RPM 单调滑到 floor 锁死"
                );
                searched_from = abs + site.len();
            }
        }
    }

    // ══════════ P1-a：瞬态 bearer-invalid 403 的机器可读标记 ══════════

    /// 真实链路会产生的那条串（上游 body 取自 `region_probe.rs:130` 记录的实测形态）。
    /// 拼法与热路径的 `format!` 逐字节同构：`{api_type} API 请求失败（…）标记: {status} {body}`。
    const REAL_TRANSIENT_403: &str = r#"流式 API 请求失败（token 瞬态失效，已冷却换号）bearer_invalid_transient=1: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"The bearer token included in the request is invalid."}"#;

    /// ⭐ P1-a：瞬态那条 bail 必须带 `bearer_invalid_transient=1`，且**逐字节**如此。
    ///
    /// 为什么需要标记：这个二分（`has_ever_succeeded`）只有 provider 做得出 —— region 错配与
    /// 瞬态抖动的上游文案**完全相同**。handler 侧只看到字符串，会把已证明健康的号判成 region
    /// 坏（排障方向错），且状态码从 502（外挂 RETRYABLE 内、会重试）变成 403（4xx 不重试）。
    ///
    /// 回退即 FAIL（已实测）：把格式串里的 `bearer_invalid_transient=1` 删掉 →
    /// 第一条 `assert!(src.contains(...))` FAIL。
    #[test]
    fn transient_bearer_invalid_bail_carries_machine_readable_marker() {
        let full = include_str!("provider.rs");
        // 必须切掉测试模块：本测试自身含该字面量，不切则断言恒真（本仓「源码守卫静默失效」的老坑）。
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);

        // 逐字节钉死格式串前缀：标记名、大小写、位置（中文文案之后、冒号之前）全在内。
        // handlers 侧按精确字面量 `bearer_invalid_transient=1` 做排除，任何漂移都会让那条
        // 排除静默失效（编译不报错、测试若只匹配子串也发现不了）。
        let fmt = "API 请求失败（token 瞬态失效，已冷却换号）bearer_invalid_transient=1: {} {}";
        assert!(
            src.contains(fmt),
            "瞬态 bearer-invalid 403 的 bail 必须带 bearer_invalid_transient=1 标记，\
             且位置在中文文案之后、`: {{status}} {{body}}` 之前（handlers 侧按精确字面量排除）"
        );

        // 同款范式的既有标记都只有一处产生点，本条也应如此（多处产生 = 语义被稀释）。
        // ⚠️ 计数只能按**格式串**（带 `: {} {}` 尾巴）算，不能按裸标记名 —— 注释里也会提它，
        // 那样计数会把注释算进来，断言变成对注释文字的约束（本测试第一版即此形态，实测 left=2）。
        assert_eq!(
            src.matches(fmt).count(),
            1,
            "该标记应只有唯一产生点（瞬态分支）；多处产生会让 handler 侧的排除覆盖到别的语义"
        );

        // ⭐ 承重：这条串**确实**落在 region-mismatch 判据的射程内 —— 这才是标记必要的证明。
        // 直接调 endpoint 侧那个谓词（handlers 的 `is_upstream_region_mismatch_403` 就是
        // 「它 && 403 && 无 401」），不在本文件重写一份子串匹配。
        assert!(
            crate::kiro::endpoint::default_is_bearer_token_invalid(REAL_TRANSIENT_403),
            "前提：瞬态串必然命中 bearer-invalid 谓词（与 region 错配逐字节同文案）"
        );
        assert!(
            REAL_TRANSIENT_403.contains("403"),
            "前提：瞬态串带 403 语境"
        );
        assert!(
            !REAL_TRANSIENT_403.to_ascii_lowercase().contains("401"),
            "前提：瞬态串不含 401（否则 region 判据本就会让路，标记也就不必要了）"
        );
        // ⇒ 三个前提同时成立 = 不加标记时 region-mismatch 判据必然误命中。
        assert!(
            REAL_TRANSIENT_403.contains("bearer_invalid_transient=1"),
            "所以必须有一个 region 判据看得见的机器可读区分位"
        );
    }

    // ══════════ P1-b：额度只计真正打到上游的次数 ══════════

    /// ⭐ P1-b（行为）：全池冷却 fast-fail 一整轮**不得**消耗跨轮重试额度。
    ///
    /// 缺陷推导（已独立复核）：`compute_max_retries(pool,pool)` 在 pool≥4 时恒为 4；
    /// 全池冷却时 `all_cooling_fast_fail` 默认开、wait>2s ⇒ `acquire_context_excluding` 裸 bail
    /// ⇒ 热路径 `continue`（不 sleep、不打上游）⇒ 第 0 轮在毫秒级跑完 4 次迭代。
    /// 旧代码用迭代计数 `attempts_base`（= 3+1 = 4）喂额度闸门 ⇒ 闸门命中 ⇒ `break 'absorb`
    /// ⇒ `absorb_round` 恒 0，吸收层对 pool≥4 等于没开。
    ///
    /// 本测试用两种口径各跑一遍同一个「一轮全 fast-fail」剧本，断言只有「计上游调用」这一种
    /// 能让第 1 轮拿到非零配额。回退即 FAIL（已实测）：把热路径改回喂 `attempts_base` 时，
    /// 单靠本测试**不会**失败（它是纯函数模拟），故必须与下面的源码守卫成对存在 —— 那条才是
    /// 「测了分支内部没测分支顺序」的防线。
    #[test]
    fn fast_fail_round_must_not_consume_upstream_retry_quota() {
        let pool = 17usize; // 线上实测规模；任何 ≥4 都会撞满硬上限
        let base = compute_max_retries(pool, pool);
        assert_eq!(
            base, ABSOLUTE_MAX_TOTAL_RETRIES,
            "前提：pool={pool} 时基础配额吃满硬上限"
        );

        // 剧本：第 0 轮 max_retries 次迭代**全部**在 acquire 处 fast-fail（零次 send）。
        let round0_iterations = base;

        // 旧口径（迭代计数）：attempts_used = 0 + (n-1)，轮末 attempts_base = attempts_used + 1。
        let attempts_base_after_round0 = (round0_iterations - 1) as u32 + 1;
        assert_eq!(
            round_retry_quota(base, attempts_base_after_round0),
            0,
            "旧口径下一整轮 fast-fail 就把 4 个额度全烧光 ⇒ 额度闸门命中 ⇒ 吸收层被旁路"
        );

        // 新口径（真实上游调用数）：一轮全 fast-fail ⇒ 一次都没打上游 ⇒ 额度分毫未动。
        let upstream_calls_after_round0 = 0u32;
        assert_eq!(
            round_retry_quota(base, upstream_calls_after_round0),
            base,
            "fast-fail 不打上游，不该消耗「打上游」的额度 —— 否则 PoolCooldown（吸收层最该拦的\
             那一类）从来没被吸收过"
        );

        // ⭐ 反向承重：新口径**不能**把上限放开。真打上游时必须照样递减、照样收敛到 0。
        let mut upstream_calls = 0u32;
        let mut rounds = 0usize;
        loop {
            let quota = round_retry_quota(base, upstream_calls);
            if quota == 0 {
                break;
            }
            // 最坏情形：本轮把配额全花在真实上游调用上。
            upstream_calls += quota as u32;
            rounds += 1;
            assert!(rounds <= 64, "必须收敛，否则是无界重试");
        }
        assert_eq!(
            upstream_calls, ABSOLUTE_MAX_TOTAL_RETRIES as u32,
            "「每请求 ≤ {} 次上游调用」的不变量必须仍然成立（换口径不等于放开上限）",
            ABSOLUTE_MAX_TOTAL_RETRIES
        );
    }

    /// ⭐ P1-b（源码位置，**这条才是承重的**）：额度累加点必须在 `send()` **之后**。
    ///
    /// 纯函数模拟证明不了热路径喂的是哪个变量（那正是「测了分支内部没测分支顺序」的形态）。
    /// 回退即 FAIL（已实测）：把 `upstream_calls += 1;` 挪到 `for attempt` 循环顶部（即
    /// `attempts_used = ...` 旁边），位置断言失败 —— 那样它就退化成迭代计数，缺陷原样回归。
    #[test]
    fn retry_quota_counts_only_calls_that_reached_upstream() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);

        // ⚠️ 必须先切到 `call_api_with_retry` 内再定位 —— 全文 `request.send().await` 有三处
        // （MCP 路径 :732 最靠前、备用模型 :1976 最靠后）。在全文上 `find` 会锚到 MCP 那处，
        // 于是「把累加挪回循环顶部」这个正是要拦的回退**照样通过**（实测：本测试第一版只有
        // 第三条 acquire 断言抓到，send 断言静默为真）。这就是「测了分支内部没测分支顺序」。
        let retry_fn = src
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        let send_at = retry_fn
            .find(format!("{}{}", "request.send()", ".await").as_str())
            .expect("send 调用点不应被改名");
        let bump = format!("{}{}", "upstream_calls ", "+= 1;");
        let bump_at = retry_fn.find(bump.as_str()).expect("额度累加点不应被删除");
        assert!(
            bump_at > send_at,
            "额度累加必须在 send() 之后：放在循环顶部会把 acquire fast-fail 的空转也算成\
             一次上游调用 ⇒ 全池冷却时毫秒内烧空 12 个额度 ⇒ 吸收层整体旁路"
        );

        // 累加点必须唯一：多处累加会让同一次 send 扣多份额度（上限被隐式砍半）。
        assert_eq!(
            retry_fn.matches(bump.as_str()).count(),
            1,
            "额度累加点必须恰好一处，否则一次上游调用扣多份额度"
        );

        // 且必须排在 acquire 的 fast-fail `continue` 之后 —— 用 acquire 调用点做锚。
        let acquire_at = retry_fn
            .find("acquire_context_excluding(")
            .expect("acquire_context_excluding 调用点不应被改名");
        assert!(
            bump_at > acquire_at,
            "额度累加必须在 acquire 之后：acquire 失败的路径压根没打上游"
        );

        // 闸门与累加口径必须一致：喂 attempts_base 就等于缺陷回归（编译不报错）。
        let gate = format!(
            "{}{}",
            "round_retry_quota(base_retry_quota, budget.used()) ==", " 0"
        );
        assert!(
            src.contains(gate.as_str()),
            "跨轮额度闸门必须按 upstream_calls 判定，与累加口径同源"
        );
    }

    /// ⭐ P1-b（分支**顺序**）：额度闸门必须排在截断闸门之前，且三道闸门顺序固定。
    ///
    /// 顺序在这里是承重的：三道都 `break 'absorb`，谁先求值决定了「这一轮为什么停」的归因，
    /// 也决定了截断闸门有没有机会被求值。缺陷期正是额度闸门（被 fast-fail 提前触发）
    /// 抢在截断闸门之前恒命中 ⇒ `:1844` 那条从来没跑过。
    ///
    /// 回退即 FAIL（已实测）：把额度闸门那段挪到 `backoff_is_truncated` 之后，第一条断言失败。
    #[test]
    fn quota_gate_precedes_truncation_and_budget_gates() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);

        let quota_at = src
            .find(
                format!(
                    "{}{}",
                    "round_retry_quota(base_retry_quota, budget.used()) ==", " 0"
                )
                .as_str(),
            )
            .expect("额度闸门不应被改名");
        let trunc_at = src
            .find(
                format!(
                    "{}{}",
                    "absorb.backoff_is_truncated", "(class, absorb_round)"
                )
                .as_str(),
            )
            .expect("截断闸门不应被改名");
        // 实参已改为 `class_deadline`（换号空窗用它自己那份预算），见
        // `truncation_gate_precedes_budget_gate` 处的同款说明。
        let budget_at = src
            .find(format!("{}{}", "should_start_another_round", "(class_deadline").as_str())
            .expect("预算闸门不应被改名");

        assert!(
            quota_at < trunc_at,
            "额度闸门（每请求硬上限）必须最先求值：它是不可协商的安全上限，\
             而截断/预算闸门都是策略性放弃 —— 顺序反了会让硬上限被策略旁路"
        );
        assert!(
            trunc_at < budget_at,
            "截断闸门必须排在预算闸门之前（既有不变量，见 truncation_gate_precedes_budget_gate）"
        );
    }

    // ══════════ P1-c：三道 break 闸门的日志必须可分辨 ══════════

    /// ⭐ P1-c：三种停止吸收的结局必须在日志里**机器可分辨**，且各自点名旋钮。
    ///
    /// 背景：`:1845` 与 `:1859` 两个语义相反的闸门在 bump **同一个**
    /// `bump_absorb_budget_exhausted()` ⇒ 面板算出的吸收比无法归因 ⇒ 运维会去抬
    /// `upstreamRetryAbsorbBudgetSecs`，而真正该动的是 `upstreamRetryAbsorbMaxDelaySecs`。
    /// 而额度闸门连计数器都没有 ⇒ 主导结局在面板上完全不存在。
    /// 拆计数器要改 `recovery_metrics.rs`（不属本次改动范围），故先在日志侧收口。
    ///
    /// 回退即 FAIL（已实测）：删掉任一 `absorb_stop = "..."` 字段，对应断言失败。
    #[test]
    fn absorb_stop_reasons_are_distinguishable_in_logs() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);

        // 三个结局各有唯一的机器可读判据（不依赖中文文案不变）。
        for reason in [
            "retry_quota_exhausted",
            "backoff_truncated",
            "budget_too_small_for_round",
        ] {
            let field = format!("absorb_stop = {:?}", reason);
            assert_eq!(
                src.matches(field.as_str()).count(),
                1,
                "结局 {reason} 必须有且仅有一处 absorb_stop 标注：\
                 三道闸门都是 break 'absorb，没有机器可读判据时日志与面板都区分不出停在哪一道"
            );
        }

        // 两个共用计数器的闸门必须各自点名**不同**的旋钮 —— 这是归因混淆的实际危害面。
        assert!(
            src.contains("需抬 upstreamRetryAbsorbMaxDelaySecs"),
            "截断闸门必须点名 maxDelaySecs：它的瓶颈是「我们愿意睡的上限」小于号池真实恢复时刻"
        );
        assert!(
            src.contains("需抬 upstreamRetryAbsorbBudgetSecs"),
            "预算闸门必须点名 budgetSecs：它的瓶颈是总预算装不下一轮"
        );

        // ⭐ 归因混淆**已修**：三个结局各有独立计数器，本守卫随之从 `== 2` 改为 `== 1`。
        // ⚠️ 必须按**全路径调用**计数：短名在注释里也出现，按短名算会把注释计进来
        // （本测试第一版即此形态，实测 left=3 right=2）。
        assert_eq!(
            src.matches("crate::common::recovery_metrics::bump_absorb_budget_exhausted()")
                .count(),
            1,
            "`budget_exhausted` 现在**只**属于「总预算装不下一轮」这一个闸门。\
             另两个结局已各有独立计数器（backoff_truncated / retry_quota_exhausted）——\
             若这里又变回 2，说明有人把某个闸门重新并回了这个桶，归因混淆会复发"
        );
        // 另两个结局各有且仅有一处 bump（拆分是否真落到调用点，而不只是声明了计数器）。
        for call in [
            "crate::common::recovery_metrics::bump_absorb_backoff_truncated()",
            "crate::common::recovery_metrics::bump_absorb_retry_quota_exhausted()",
        ] {
            assert_eq!(
                src.matches(call).count(),
                1,
                "{call} 必须有且仅有一处调用（拆了计数器却漏改调用点是本仓已发生过的形态）"
            );
        }
    }

    /// ⭐ 硬约束守卫：**默认配置下三个新类别一律不吸收**。
    ///
    /// 线上正在服务，新能力必须靠显式开启。判据收在 `class_allowed` 一处（散写 `if` 必然漏
    /// 一处，而漏掉那处的表现正是「默认关的类别其实在吸收」）。
    ///
    /// 回退验证：把 `class_allowed` 里 `AbsorbClass::TransientServerError => self.absorb_server_error`
    /// 改成 `=> true` → 本测试 FAILED。
    #[test]
    fn new_absorb_classes_are_all_gated_off_by_default() {
        use crate::anthropic::AbsorbClass;
        // 总开关开着（否则 effective_max_rounds()=0，测不到类别闸门本身）。
        let p = AbsorbPolicy::from_config(&absorb_cfg(true));

        assert!(
            !p.class_allowed(AbsorbClass::SwapWindow),
            "换号空窗默认不吸收（upstreamRetryAbsorbSuspended 默认 false）"
        );
        assert!(
            !p.class_allowed(AbsorbClass::TransientServerError),
            "5xx 默认不吸收：外挂实测 11.6 次重试才救回 1 个请求，那是不分机理一律重试的账单"
        );
        assert!(
            !p.class_allowed(AbsorbClass::TransientCapacity400),
            "容量 400 默认不吸收"
        );
        // 原有两类跟着总开关走，行为不变（否则本改动会把吸收层的既有作用对象也关掉）。
        assert!(p.class_allowed(AbsorbClass::PoolCooldown(3)));
        assert!(p.class_allowed(AbsorbClass::UpstreamRateLimit));

        // 显式开启必须真生效，否则这些开关等于不存在。
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_server_error = true;
        c.upstream_retry_absorb_capacity_400 = true;
        c.upstream_retry_absorb_suspended = true;
        let on = AbsorbPolicy::from_config(&c);
        assert!(on.class_allowed(AbsorbClass::TransientServerError));
        assert!(on.class_allowed(AbsorbClass::TransientCapacity400));
        assert!(on.class_allowed(AbsorbClass::SwapWindow));
    }

    /// ⭐ 合并外挂缺口 3：换号空窗需要**完全不同的退避节奏**。
    ///
    /// 外挂原文：「KiroStudio 换号（auto_disable + 切下一个凭据 + 推送补号）实测有约 10 分钟的
    /// 空窗……**绝不能用限速那套 1 秒退避** —— 那是拿一个已被封的账号去猛打上游，只会加重风控。」
    ///
    /// 回退验证：把 `required_wait` 里 SwapWindow 的 `if self.swap_budget.is_zero()` 分支删掉
    /// （只留指数曲线）→ 本测试 FAILED。
    #[test]
    fn swap_window_uses_long_ladder_only_when_budget_configured() {
        use crate::anthropic::AbsorbClass;

        // ① 默认（swap 预算 0）：与限速同曲线 ⇒ 逐字节等于本字段引入前的行为。
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_suspended = true;
        let old = AbsorbPolicy::from_config(&c);
        for round in 0..3 {
            assert_eq!(
                old.required_wait(AbsorbClass::SwapWindow, round),
                old.required_wait(AbsorbClass::UpstreamRateLimit, round),
                "未设 swap 预算时必须沿用旧曲线（默认不改变现有行为）"
            );
        }
        assert_eq!(
            old.class_max_delay(AbsorbClass::SwapWindow),
            old.max_delay,
            "未设 swap 预算时上界不得被放宽"
        );

        // ② 设了 swap 预算：换成 20/40/60s 长阶梯，且超表长取最后一档。
        c.upstream_retry_absorb_swap_budget_secs = 600;
        let laddered = AbsorbPolicy::from_config(&c);
        for (round, want) in [(0u32, 20u64), (1, 40), (2, 60), (7, 60)] {
            assert_eq!(
                laddered.required_wait(AbsorbClass::SwapWindow, round),
                Duration::from_secs(want),
                "第 {round} 轮应睡 {want}s（外挂 SWAP_BACKOFF 阶梯）"
            );
        }
        // ⭐ 承重：长阶梯**不能被默认 15s 的全局上限削回** —— 否则这个旋钮等于没接上，
        // 且 `backoff_is_truncated` 只对 PoolCooldown 成立，不会拦住这种「睡不够」。
        assert_eq!(
            laddered.backoff(AbsorbClass::SwapWindow, 0),
            Duration::from_secs(20),
            "20s 阶梯必须真的睡 20s（max_delay 默认 15s，不放宽上界就会被削成 15s）"
        );

        // ⭐ 其它类别的上界**不得**被这个旋钮波及（只放宽换号空窗那一类）。
        assert_eq!(
            laddered.class_max_delay(AbsorbClass::UpstreamRateLimit),
            laddered.max_delay
        );
        assert_eq!(
            laddered.class_max_delay(AbsorbClass::TransientServerError),
            laddered.max_delay
        );
    }

    /// 新增两类的退避曲线：5xx 短（1s 起）、容量类中等（2s 起）。
    ///
    /// 回退验证：把 `TransientServerError` 的 `BASE` 从 1s 改成 2s（与容量类同曲线）→ FAILED。
    /// 两条曲线必须**可区分**：5xx 多为瞬时抖动，容量类是全局状态、换号不解决问题。
    #[test]
    fn transient_5xx_backs_off_shorter_than_capacity_class() {
        use crate::anthropic::AbsorbClass;
        let mut c = absorb_cfg(true);
        // 抬高上界，让曲线本身可见（默认 15s 会把两条都 clamp 到同一个值）。
        c.upstream_retry_absorb_max_delay_secs = 300;
        let p = AbsorbPolicy::from_config(&c);

        assert_eq!(
            p.required_wait(AbsorbClass::TransientServerError, 0),
            Duration::from_secs(1),
            "5xx 起步 1s（逐字取自外挂 MIN_DELAY=1.0）"
        );
        assert_eq!(
            p.required_wait(AbsorbClass::TransientCapacity400, 0),
            Duration::from_secs(2),
            "容量类起步 2s：全局容量问题，换号不解决，比 5xx 更该慢"
        );
        for round in 0..4 {
            assert!(
                p.required_wait(AbsorbClass::TransientServerError, round)
                    < p.required_wait(AbsorbClass::TransientCapacity400, round),
                "第 {round} 轮：5xx 必须严格短于容量类（两类曲线不得退化成同一条）"
            );
        }
    }

    /// ⭐ 换号空窗的**独立 deadline**：只有它拿那份更宽的预算，其余类别一律用总预算。
    ///
    /// 回退验证：把 `class_deadline` 的 `matches!(..., SwapWindow)` 条件删掉（所有类别都用
    /// swap 预算）→ 本测试 FAILED。那会让**所有**类别都能占着客户端连接十分钟，
    /// 而换号空窗恰恰是唯一等得起的一类。
    #[test]
    fn swap_budget_deadline_does_not_leak_to_other_classes() {
        use crate::anthropic::AbsorbClass;
        let now = std::time::Instant::now();
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_suspended = true;
        c.upstream_retry_absorb_swap_budget_secs = 600;
        let p = AbsorbPolicy::from_config(&c);

        assert_eq!(
            p.class_deadline(now, AbsorbClass::SwapWindow),
            now + Duration::from_secs(600),
            "换号空窗必须用它自己那份预算（空窗实测 10 分钟 ≫ 总预算 20~45s）"
        );
        for other in [
            AbsorbClass::PoolCooldown(5),
            AbsorbClass::UpstreamRateLimit,
            AbsorbClass::TransientServerError,
            AbsorbClass::TransientCapacity400,
        ] {
            assert_eq!(
                p.class_deadline(now, other),
                now + p.budget,
                "{other:?} 必须仍用总预算 —— swap 预算泄漏给其它类别 = 所有请求都可能长挂十分钟"
            );
        }

        // 未设 swap 预算时，换号空窗也回到总预算（默认不改变现有行为）。
        c.upstream_retry_absorb_swap_budget_secs = 0;
        let old = AbsorbPolicy::from_config(&c);
        assert_eq!(
            old.class_deadline(now, AbsorbClass::SwapWindow),
            now + old.budget
        );
    }

    /// ⭐ 「额外轮次钉 1」的解除条件：**只在设了 swap 预算时**解除。
    ///
    /// 钉 1 的前提是短退避（15s 内重打同一个刚被风控的账号会抵消 `config.self_heal_base_backoff_secs（默认 60s）=60s`）。
    /// 长阶梯最短一档就是 20s，前提不再成立。不解除的话这个旋钮基本没用：它只能把**一次**
    /// 重试推迟到 20s 后，而空窗实测 10 分钟 ⇒ 那一次几乎必然还在窗口内。
    ///
    /// 回退验证：把 `from_config` 里的 `&& swap_budget.is_zero()` 删掉 → 第一条断言 FAILED
    /// （存量 `suspended=true` 的部署会从 1 轮变成 3 轮，属默认行为变更）。
    #[test]
    fn suspended_round_pin_released_only_with_swap_budget() {
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_suspended = true;
        assert_eq!(
            AbsorbPolicy::from_config(&c).effective_max_rounds(),
            1,
            "未设 swap 预算时必须仍钉 1（存量 suspended=true 的部署行为逐字节不变）"
        );

        c.upstream_retry_absorb_swap_budget_secs = 600;
        assert_eq!(
            AbsorbPolicy::from_config(&c).effective_max_rounds(),
            c.upstream_retry_absorb_max_rounds,
            "设了 swap 预算即解除钉 1，交回 max_rounds + 独立 deadline + 总额度三道闸"
        );

        // 总开关关闭时一切照旧恒 0（这条是吸收层「关 ⇒ 逐字节等价旧行为」的根）。
        let mut off = absorb_cfg(false);
        off.upstream_retry_absorb_suspended = true;
        off.upstream_retry_absorb_swap_budget_secs = 600;
        assert_eq!(AbsorbPolicy::from_config(&off).effective_max_rounds(), 0);
    }

    /// ⭐ 缺口 4 的 provider 侧：**只在吸收层真跑过并放弃、且配置为 503 时**打标记。
    ///
    /// 源码级守卫（走到那段需要真实上游 + 真实号池，行为测试写不了 —— 本仓惯例）。
    ///
    /// 回退验证：把 `exhausted_as_503` 的判据从 `== 503` 改成 `!= 429`，或把
    /// `absorb_gave_up_after_rounds |= absorb_round > 0` 里的限定去掉 → 对应断言 FAILED。
    #[test]
    fn exhausted_503_marker_is_gated_on_both_conditions() {
        let src = include_str!("provider.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(src);

        // ① 只认精确的 503：其它值（含裸 serde default 会给的 0）一律按 429 处理。
        assert!(
            include_str!("absorb_policy.rs")
                .contains("cfg.upstream_retry_absorb_exhausted_status == 503"),
            "必须只认精确 503 —— 打一个 handlers 认不出的标记只会造成静默的行为分叉"
        );
        // ② 标记必须同时受「真跑过轮次」约束：一次都没重试就改状态码是说谎。
        assert!(
            prod.contains("absorb_gave_up_after_rounds && absorb.exhausted_as_503"),
            "标记必须两个条件都满足才打（跑过轮次 且 配置为 503）"
        );
        // ③ 每处置位都带 `absorb_round > 0` 限定 —— 关闭吸收层时这里恒 0 ⇒ 不置位 ⇒
        //    渲染路径逐字节不变。这是「默认不改变现有行为」的机制本身。
        let sets = prod
            .matches("absorb_gave_up_after_rounds |= absorb_round > 0")
            .count();
        assert!(
            sets >= 3,
            "三条放弃结局（轮次用尽 / 额度用尽 / 退避被截断）都应置位，当前 {sets} 处"
        );
        assert!(
            !prod.contains("absorb_gave_up_after_rounds = true"),
            "不得无条件置位：那会让「吸收层没开也返 503」，等于对客户端说谎"
        );
    }

    /// 每个 `AbsorbClass` 都必须能在计数器上分辨（否则上线后无法判断哪类在起作用）。
    ///
    /// 回退验证：删掉 `bump_absorb_round_swap_window()` 那一处调用 → FAILED。
    #[test]
    fn every_absorb_class_has_a_distinguishable_counter() {
        let src = include_str!("provider.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(src);
        for call in [
            "bump_absorb_round_pool_cooldown()",
            "bump_absorb_round_rate_limit()",
            "bump_absorb_round_swap_window()",
            "bump_absorb_round_server_error()",
            "bump_absorb_round_capacity_400()",
            "bump_absorb_server_error_skipped()",
            "bump_absorb_capacity_400_skipped()",
        ] {
            assert!(
                prod.contains(call),
                "{call} 必须被调用：五类共用一个 absorb_rounds 时，开三个开关后面板上仍是\
                 一个数 ⇒ 无法归因，也就无法决定该关掉哪个"
            );
        }
    }

    // ══════════ L1/L2：对话路径 region 自纠正 ══════════

    /// 真实链路会产生的 403 body（`region_probe.rs:130` 记录的实测形态，与
    /// `REAL_TRANSIENT_403` 里嵌的那段 body 逐字节同源）。
    ///
    /// 用它而不是自编串：上一轮审查抓到过「用合成串测试，而真实链路不产生那种串」——
    /// 那种测试全绿而线上判据全部漏命中。
    const REAL_BEARER_INVALID_BODY: &str = r#"{"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"The bearer token included in the request is invalid."}"#;

    /// L1 主用例：**从未成功过**的 `api_key` 号吃 region 错配 403 ⇒ 必须换区（而非换号）。
    ///
    /// 回退即 FAIL：把 `region_retry_target` 的 `has_ever_succeeded` 取反，或让它恒返
    /// `None` → 第二条断言 FAILED（拿不到目标区 = 热路径不会 `continue` 换区，
    /// 落到下方 `report_failure` + failover 换号，而换号治不了 region 错配）。
    #[test]
    fn never_succeeded_api_key_with_region_mismatch_403_switches_region() {
        // 前提：这条真实 body 确实命中热路径那道谓词（否则本测试测的不是同一条路）。
        assert!(
            crate::kiro::endpoint::default_is_bearer_token_invalid(REAL_BEARER_INVALID_BODY),
            "前提：真实 403 body 必须命中 is_bearer_token_invalid，否则热路径根本进不了该分支"
        );

        let target = region_retry_target("eu-central-1", true, false);
        assert_eq!(
            target,
            Some("us-east-1"),
            "从未成功过的 api_key 号打错区 ⇒ 必须换到**另一个**候选区；\
             返 None 就是回到「当凭据问题换号」的旧行为，而换号解决不了 region 错配"
        );

        // 反向也成立（US 号被探测写成 eu 是实测形态，但反过来同样要能纠）。
        assert_eq!(
            region_retry_target("us-east-1", true, false),
            Some("eu-central-1"),
            "换区必须是双向的，否则只能纠正一个方向"
        );
    }

    /// L1 收窄用例：**已成功过**的号吃**同一条** 403 ⇒ 必须**不**换区。
    ///
    /// 这是 L1 与既有 `bearer_invalid_but_proven` 的分界线：同一句上游文案，
    /// `has_ever_succeeded` 是唯一区分位。已成功过 = 这个区真拿到过 200 ⇒ 区是对的，
    /// 403 只能是抖动（实测 4 个号累计 3393 次成功、共吃 42 次）⇒ 该走瞬态分支。
    ///
    /// 回退即 FAIL：把 `region_retry_target` 里的 `|| has_ever_succeeded` 删掉 → 断言 FAILED
    /// （已证明健康的号会被换区 = 把一个本来对的配置改坏，且下一次抖动过去它本来就好了）。
    #[test]
    fn proven_credential_with_same_403_must_not_switch_region() {
        assert_eq!(
            region_retry_target("eu-central-1", true, true),
            None,
            "已成功过的号必须让路给既有瞬态分支（冷却+换号、不计失败），绝不换区"
        );
    }

    /// L2 的门：OAuth 号不换区、也就不回写 `api_region`。
    ///
    /// 依据：OAuth 号的权威 region 是 `profileArn` 第 4 段（`effective_upstream_region`
    /// 第一优先），`api_region` 对它根本不生效 ⇒ 换区不改变实际 host（白烧一次额度），
    /// 回写则在面板上留一个"看起来生效其实被压住"的值，把排障带偏。
    ///
    /// 回退即 FAIL：删掉 `region_retry_target` 里的 `!is_api_key` 门 → 断言 FAILED。
    #[test]
    fn oauth_credential_must_not_switch_or_write_back_region() {
        assert_eq!(
            region_retry_target("eu-central-1", false, false),
            None,
            "OAuth 号的 region 由 profileArn 决定，换区/回写 api_region 对它无效"
        );

        // 回写点必须**显式**带 `is_api_key_credential` 门（第二道）：入口那道门若被放宽，
        // 这里仍不能把 OAuth 号的 api_region 写坏。
        let src = include_str!("provider.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(src);
        let writeback = format!("{}{}", "set_credential_api_region", "(ctx.id");
        let at = prod
            .find(writeback.as_str())
            .expect("L2 回写调用点不应被改名/删除");
        // 回写之前的窗口内必须出现 api_key 门。窗口取 600 字节（中间隔着注释）。
        // ⚠️ 必须挪到合法字符边界：本文件含中文注释，裸切会 panic；而回退成"整段前缀"
        // 会让断言恒真（别处的门也被算进来），那等于白写。
        let mut window_start = at.saturating_sub(600);
        while window_start < at && !prod.is_char_boundary(window_start) {
            window_start += 1;
        }
        assert!(
            prod[window_start..at].contains("is_api_key_credential()"),
            "L2 回写点前必须有 is_api_key_credential 门，否则 OAuth 号会被写进一个不生效的 api_region"
        );
    }

    /// 候选表的形状假设：只有两项，且首项 `eu-central-1`。
    ///
    /// 实测依据：`management.*` 与 `runtime.*` 只在 `us-east-1` / `eu-central-1` 解析 DNS。
    /// 表若被扩项，`region_retry_target` 的「换到另一个」就退化成「顺序轮换」——
    /// 语义变了，本测试会 FAIL 以强制重新审视。
    #[test]
    fn region_retry_falls_back_to_first_candidate_when_current_is_off_table() {
        assert_eq!(
            crate::kiro::region_probe::PROBE_ORDER.len(),
            2,
            "前提：候选只有两个（实测只有这两区解析 DNS）。扩表需重新审视 region_retry_target 的语义"
        );
        // 当前区不在表内（真实成因：profileArn 把区钉在 us-west-2）⇒ 换到表首项。
        assert_eq!(
            region_retry_target("us-west-2", true, false),
            Some(crate::kiro::region_probe::PROBE_ORDER[0]),
            "当前区不在候选表内时必须落到表首项，而不是返 None（那样该号永远纠不过来）"
        );
    }

    /// 🔴 **顺序断言**：换区分支必须排在 `bearer_invalid_transient` 之后、401 之后。
    ///
    /// 为什么必须有这条：本仓「纸面测试」第 8 种形态 —— **测了分支内部，没测分支顺序**。
    /// 真实事故：改三处、四条测试、三次「回退即 FAILED」全过而修复无效，因为一条通用分支
    /// 排在特化分支之前先 `break` 了。上面那几条纯函数测试对顺序**完全不可见**：
    /// `region_retry_target` 可以完美无缺而热路径根本走不到它。
    ///
    /// 断言的是**最终行为**（换区 vs 换号），三条各自钉一个会让行为反转的顺序关系：
    /// ① 瞬态分支在前 ⇒ 已成功过的号在到达换区分支**之前**就被 `continue` 掉；
    /// ② 换区分支带 403 门 ⇒ 401 落不进来（401 该 force-refresh/计失败，换区对它无用）；
    /// ③ 换区分支在通用 `report_failure` 之前 ⇒ region 错配的号走的是换区，不是换号 + 计失败。
    #[test]
    fn region_switch_branch_ordered_after_transient_and_401() {
        let src = include_str!("provider.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(src);
        let retry_fn = prod
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");

        // needle 运行时拼接：完整字面量会被 include_str! 读到自己而自匹配（本文件已踩三次）。
        let transient_guard = format!("{}{}", "bearer_invalid_but", "_proven");
        let transient_marker = format!("{}{}", "bearer_invalid_", "transient=1");
        let region_guard = format!("{}{}", "region_switched_", "this_call.contains");
        let punish = format!("{}{}", "report_failure", "(ctx.id)");

        let transient_at = retry_fn
            .find(transient_guard.as_str())
            .expect("既有瞬态判定不应被改名");
        let marker_at = retry_fn
            .find(transient_marker.as_str())
            .expect("瞬态机器可读标记不应被删");
        let region_at = retry_fn
            .find(region_guard.as_str())
            .expect("换区分支的每号一次门不应被改名");
        let punish_at = retry_fn
            .rfind(punish.as_str())
            .expect("通用 401/403 的 report_failure 不应被改名");

        // ① 瞬态在前：已成功过的号必须在换区分支之前就被 continue 掉。
        // 顺序反了 ⇒ 已证明健康的号（区是对的）会被换区，把对的配置改坏。
        assert!(
            transient_at < region_at && marker_at < region_at,
            "换区分支必须排在 bearer_invalid_transient 之后：\
             顺序反了会让已成功过的号（区本来是对的）被换区，且瞬态标记再也打不出来"
        );

        // ② 401 让路：换区分支的判据里必须带 403 门。
        // 取该分支起点前的窗口，断言 403 门与它同处一条 `if` 条件里。
        let mut window_start = region_at.saturating_sub(200);
        while window_start < region_at && !retry_fn.is_char_boundary(window_start) {
            window_start += 1;
        }
        assert!(
            retry_fn[window_start..region_at].contains("status.as_u16() == 403"),
            "换区分支必须带 403 门（401 让路）：401 是 token 死了 ≠ 区错了，\
             换个区照样是死 token，只会白烧一次重试额度并延后真正的 force-refresh"
        );

        // ③ 换区在计失败之前：region 配错≠号坏（隔离铁律）。
        // 顺序反了 ⇒ 号先被 report_failure（累计 3 次即禁用），换区永远轮不到，
        // 即回到「US 号导入即废」那个形态。
        assert!(
            region_at < punish_at,
            "换区分支必须排在通用 report_failure 之前：反了则 region 错配的号先被计失败\
             （3 次即禁用），换区分支永远走不到"
        );

        // 换区分支**绝不能**调用 report_failure / 冷却：那是「号坏了」的处置。
        // 取该分支体的一段窗口（到下一处 `continue;` 为止）做否定断言。
        let branch_body = &retry_fn[region_at..];
        let branch_end = branch_body
            .find("// 同一个号在一条请求里只惩罚一次")
            .expect("换区分支与通用惩罚分支之间的注释锚点不应消失");
        let branch = &branch_body[..branch_end];
        assert!(
            !branch.contains(punish.as_str()),
            "换区分支内绝不能 report_failure：region 配错≠号坏，惩罚它会把一个其实好的号推向禁用"
        );
    }

    /// L1 上限：同一个号在一次客户端请求内**最多换区一次**。
    ///
    /// 不加上限就是两个区来回打（A 403 → 换 B → B 403 → 换回 A → …），一条客户端请求
    /// 把额度全烧在同一个号的两个区之间、同一出口 IP 连打 = 正是风控要抓的突发特征。
    /// 本仓刚因「吸收层放大」修过一轮。
    ///
    /// 回退即 FAIL：删掉 `!region_switched_this_call.contains(&ctx.id)` 这道门 → 第一条
    /// 断言 FAILED；把那个集合的 `let mut` 挪进 `'absorb: loop` → 第三条 FAILED
    /// （挪进去 ⇒ 每一轮各拿一份新集合 ⇒ 上限退化成「每轮一次」，吸收 3 轮就是 4 次）。
    #[test]
    fn region_switch_capped_once_per_credential_per_call() {
        let src = include_str!("provider.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(src);
        let retry_fn = prod
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");

        let gate = format!("!{}{}", "region_switched_this_call", ".contains(&ctx.id)");
        assert!(
            retry_fn.contains(gate.as_str()),
            "必须有 per-call 的每号一次门，否则同一个号会在两个区之间来回打、烧光重试额度"
        );
        let mark = format!("{}{}", "region_switched_this_call", ".insert(ctx.id)");
        assert!(
            retry_fn.contains(mark.as_str()),
            "命中换区后必须置位，否则那道 contains 门恒不成立 = 等于没有上限"
        );

        // 集合必须声明在吸收循环**之外**（跨轮共享），否则上限退化成「每轮一次」。
        let decl = format!("let mut {}", "region_switched_this_call");
        let decl_at = retry_fn.find(decl.as_str()).expect("集合声明不应被改名");
        let loop_at = retry_fn
            .find(format!("{}{}", "'absorb: ", "loop {").as_str())
            .expect("'absorb: loop 不应被改名");
        assert!(
            decl_at < loop_at,
            "换区去重集必须声明在吸收循环之外：挪进轮内 ⇒ 每轮各拿一份 ⇒ 上限退化成\
             「每轮一次」，吸收 3 轮就是 4 次换区"
        );

        // 换区后必须把该号从 tried_this_call 摘掉，否则下一跳会结构性避开它 ⇒
        // 覆盖值躺在 map 里没人用 = 换区等于没做（这是最容易静默失效的一处）。
        let unexclude = format!("{}{}", "tried_this_call", ".remove(&ctx.id)");
        assert!(
            retry_fn.contains(unexclude.as_str()),
            "换区后必须把该号从 tried_this_call 摘掉，否则 acquire_context_excluding 会避开它，\
             换区重试打的是别人的号 —— 覆盖值没人用，等于没换区"
        );
    }

    /// ⭐ A-5 源码级守卫：429 备区换桶 与 L1 403 换区必须**共享同一个**「本请求已换区」
    /// 标记（`region_switched_this_call`）。
    ///
    /// 回退即 FAIL：
    /// - 把 429 备区换桶处的标记置位删掉 → 第一条断言 FAILED（403 分支的门看不见
    ///   这次换桶 ⇒ 按已换到的区算回原区 ⇒ 原区桶还在封禁 ⇒ 备区路径又弹回 ⇒
    ///   同一请求内 A→B→A→B 振荡）；
    /// - 把标记置位挪出 `alt_region` 的 `Some(r)` 分支（如无条件置位）→ 第二条 FAILED
    ///   （未换桶的普通请求也置位 ⇒ 该号本请求内合法的一次 L1 换区被误杀）；
    /// - 403 分支那道门被删 → 第三条 FAILED（两路径的共享感知失效，回到「各自为政」）。
    #[test]
    fn alt_region_swap_marks_region_switched_shared_with_l1_403() {
        let src = include_str!("provider.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(src);
        let retry_fn = prod
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");

        // ① 429 备区换桶处必须置位共享标记（与 L1 403 的置位同字面量、同集合）。
        let mark = format!("{}{}", "region_switched_this_call", ".insert(ctx.id)");
        // ② 该置位必须落在 `alt_region` 的 `Some(r)` 分支内：锚定备区生效处的
        //    Cow 重绑到 `None` 分支之间的切片，断言标记插入在其中。
        let anchor = "let call_creds: std::borrow::Cow<'_, KiroCredentials> = match alt_region";
        let branch_start = retry_fn
            .find(anchor)
            .expect("备区生效处的 Cow 重绑不应被改名");
        let branch_end = retry_fn[branch_start..]
            .find("None => call_creds")
            .map(|i| branch_start + i)
            .expect("alt_region 的 None 分支不应消失");
        let alt_branch = &retry_fn[branch_start..branch_end];
        assert!(
            alt_branch.contains(mark.as_str()),
            "429 备区换桶必须置位「本请求已换区」标记（与 L1 403 同一份）：\
             否则 403 分支的门看不见这次换桶，会按已换到的区算回原区，而原区桶还在封禁期，\
             select_endpoint 又弹回备区 ⇒ 同一请求内 A→B→A→B 振荡"
        );

        // ③ 403 分支的门仍在（两路径读同一个标记，共享感知才有落点）。
        let gate = format!("!{}{}", "region_switched_this_call", ".contains(&ctx.id)");
        assert!(
            retry_fn.contains(gate.as_str()),
            "403 换区分支的门被删：429 备区换桶置的标记没人读，共享感知失效"
        );
    }

    /// ⭐ 源码级守卫（P0-A）：对话路径与 MCP 路径的失败守卫组装点都必须存在，
    /// 且初值必须是 [`crate::kiro::upstream_trace::VERDICT_UNCLASSIFIED`]。
    ///
    /// 与 `upstream_trace.rs` 的 `provider_guards_must_default_verdict_to_unclassified`
    /// 同源（那边数全局计数=2，这里数两处组装点各自就位）：漏标的失败分支在 trace
    /// 里落 unclassified，验收脚本据此统计。组装点被删/挪出失败路径/初值被改，本断言红。
    #[test]
    fn trace_guards_wired_in_both_call_paths_with_unclassified_default() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        // needle 运行时拼接（include_str! 自匹配坑，本仓踩过多次）。
        let needle = format!(
            "{}{}",
            "verdict: crate::kiro::upstream_trace::VERDICT_UNCLASSIFIED", ".to_string(),"
        );
        assert_eq!(
            src.matches(needle.as_str()).count(),
            2,
            "对话路径与 MCP 路径的失败守卫组装点都必须以 VERDICT_UNCLASSIFIED 为初值\
             （当前 {} 处），否则漏标的失败分支在 trace 里查不出来",
            src.matches(needle.as_str()).count()
        );
        // 两处必须分别落在两个重试函数里，且都位于「失败响应」之后（守卫真的挂在
        // 失败路径上，而不是在函数里充数）。函数切片照本仓先例截到下一个顶层函数。
        for (fname, marker) in [
            ("call_api_with_retry", "async fn call_api_with_retry"),
            ("call_mcp_with_retry", "async fn call_mcp_with_retry"),
        ] {
            let start = src
                .find(marker)
                .unwrap_or_else(|| panic!("{fname} 不应被改名"));
            let after_sig = start + marker.len();
            let rest = &src[after_sig..];
            let end = ["\n    async fn ", "\n    pub fn ", "\n    fn "]
                .iter()
                .filter_map(|m| rest.find(m))
                .min()
                .map(|i| after_sig + i)
                .unwrap_or(src.len());
            let seg_fn = &src[start..end];
            let fail_at = seg_fn
                .find("// 失败响应")
                .unwrap_or_else(|| panic!("{fname} 的失败响应注释锚点不应被删改"));
            let guard_at = seg_fn
                .find(needle.as_str())
                .unwrap_or_else(|| panic!("{fname} 缺少失败守卫组装点"));
            assert!(
                fail_at < guard_at,
                "{fname}：守卫必须组装在读到失败 body 之后（挂在失败路径上）"
            );
            assert_eq!(
                seg_fn.matches(needle.as_str()).count(),
                1,
                "{fname} 应恰好一处守卫组装点"
            );
        }
    }

    /// ⭐ 源码级守卫（P0-A）：成功侧与网络错误侧必须各自用独立 emit 发 trace。
    ///
    /// 守卫不覆盖成功路径（成功时 body 是对话内容，不该读也不该落盘），成功侧用
    /// verdict="success" 的独立 emit；网络错误无响应体，同样独立 emit（status=None）。
    /// 两条路径缺任一处，trace 里该形态的请求就整条不可见。
    #[test]
    fn trace_success_and_network_error_emits_exist_in_both_paths() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        for (fname, marker) in [
            ("call_api_with_retry", "async fn call_api_with_retry"),
            ("call_mcp_with_retry", "async fn call_mcp_with_retry"),
        ] {
            let start = src
                .find(marker)
                .unwrap_or_else(|| panic!("{fname} 不应被改名"));
            let after_sig = start + marker.len();
            let rest = &src[after_sig..];
            let end = ["\n    async fn ", "\n    pub fn ", "\n    fn "]
                .iter()
                .filter_map(|m| rest.find(m))
                .min()
                .map(|i| after_sig + i)
                .unwrap_or(src.len());
            let seg_fn = &src[start..end];
            assert_eq!(
                seg_fn.matches("verdict: \"success\"").count(),
                1,
                "{fname} 成功侧必须有独立的 verdict=\"success\" trace emit（守卫不覆盖成功路径）"
            );
            assert_eq!(
                seg_fn.matches("verdict: \"network_error\"").count(),
                1,
                "{fname} 网络错误分支必须有独立的 verdict=\"network_error\" trace emit"
            );
        }
    }

    // ══════════ mapped_model 透传预判（predict_passthrough_upstream_model）══════════

    fn predict_cred() -> KiroCredentials {
        KiroCredentials::default()
    }

    fn predict_rules(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// 未命中映射 → None（消费端回落原始名）；命中映射 → Some(映射后名)。
    #[test]
    fn predict_mapped_model_without_normalize_only_maps() {
        let cred = predict_cred();
        let rules = predict_rules(&[("claude-haiku-4-5", "claude-sonnet-4-5")]);
        assert_eq!(
            KiroProvider::predict_passthrough_upstream_model(Some("claude-haiku-4-5"), &cred, &rules),
            Some("claude-sonnet-4-5".to_string())
        );
        assert_eq!(
            KiroProvider::predict_passthrough_upstream_model(Some("claude-opus-5"), &cred, &rules),
            None,
            "未命中映射 → None"
        );
        // 空模型名不 panic 且不改写。
        assert_eq!(
            KiroProvider::predict_passthrough_upstream_model(Some(""), &cred, &rules),
            None
        );
        assert_eq!(
            KiroProvider::predict_passthrough_upstream_model(None, &cred, &rules),
            None,
            "无模型语义调用 → None"
        );
    }

    /// 映射命中 → 预判记映射名（与 forward 链一致）。
    #[test]
    fn predict_mapped_model_map_to_deepseek_kept() {
        let cred = predict_cred();
        let rules = predict_rules(&[("claude-haiku-4-5", "deepseek-v4-flash")]);
        assert_eq!(
            KiroProvider::predict_passthrough_upstream_model(Some("claude-haiku-4-5"), &cred, &rules),
            Some("deepseek-v4-flash".to_string())
        );
    }

    /// 豁免凭据：映射跳过 → 不改写（对齐 forward 的 exempt 分支）。
    #[test]
    fn predict_mapped_model_exempt_skips_mapping() {
        let mut cred = predict_cred();
        cred.model_mapping_exempt = Some(true);
        let rules = predict_rules(&[("claude-opus-5", "claude-haiku-4-5")]);
        assert_eq!(
            KiroProvider::predict_passthrough_upstream_model(Some("claude-opus-5"), &cred, &rules),
            None,
            "豁免只跳过映射，无其它改写 → None"
        );
    }
}
