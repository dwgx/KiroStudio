//! Kiro API Provider
//!
//! 核心组件，负责与 Kiro API 通信
//! 支持流式和非流式请求
//! 支持多凭据故障转移和重试
//! 支持按凭据级 endpoint 切换不同 Kiro API 端点

use reqwest::Client;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::http_client::{ProxyConfig, build_streaming_client};
use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::TlsBackend;
use parking_lot::Mutex;

/// 每个凭据的最大重试次数
const MAX_RETRIES_PER_CREDENTIAL: usize = 3;

/// 小号池阈值：号池 <= 此值时，每号重试次数降为 1（见 [`compute_max_retries`]）。
/// 小池下重试只会反复砸同几个号，被限流时多打几次纯属加重冷却，不如各摸一次即透传。
const SMALL_POOL_THRESHOLD: usize = 3;

/// 总重试次数绝对硬上限（避免无限重试）
///
/// 注意：这只是一个安全上限，不再作为固定的重试预算。真正的预算由
/// [`compute_max_retries`] 依据凭据总数 / 可用数动态计算，保证每个可用
/// 凭据至少能被摸到一次（历史上写死 9 会让凭据 >3 时后面的号一次没试就报错）。
///
/// ⚠️ 由 64 降到 12：64 从未是「合理预算」而只是个防死循环的兜底，但配合
/// `total * 3` 的算法（且 total 曾把 disabled / custom_api 都算进去）实际生效成了
/// 生产日志里的 `尝试 8/36`——一条客户端请求连打十几个号、同一出口 IP，正是风控要抓的
/// 突发特征。叠加 sub2api 侧的 2 次重试 × 10 次账号切换，单请求最坏放大到约 70~108 次
/// 上游调用。12 仍足以让每个号被摸到（可选号 > 12 时下面会以 available 为准不受此限）。
const ABSOLUTE_MAX_TOTAL_RETRIES: usize = 12;

/// 单个入站请求的重试墙钟预算（秒）。
///
/// ⚠️ 关键防雪崩闸门：小号池下，一个卡住的请求会在每次重试时抢到刚出冷却的号、
/// 又打 429、又把它冷却，如此在 acquire_context 的等待循环（最长 180s）× 多次
/// 重试之间反复横跳，一个请求就能把整池长时间压死（表现为「没有新入站却一直 429
/// / 繁忙」）。这里给单请求一个总时长上限：超时就停止重试、把最后的错误（通常是
/// 429）透传给客户端，让客户端自己退避，而不是继续拖垮整池。取值需覆盖一次正常
/// 大请求的排队+响应，又不至于长到能扫冷全池。
const MAX_REQUEST_RETRY_BUDGET_SECS: u64 = 45;

/// 计算本次调用允许的总重试次数（动态预算）
///
/// - `total`：凭据总数
/// - `available`：当前未禁用（可用）凭据数
///
/// 预算 = `(total * per_cred).min(ABSOLUTE_MAX_TOTAL_RETRIES)`，再以 1 兜底。
///
/// ⚠️ **`available` 已不参与计算**（参数保留只为不动调用点与既有测试）。
/// 因此本函数**不再保证「每个可用凭据至少被尝试一次」** —— 号池大于
/// `ABSOLUTE_MAX_TOTAL_RETRIES` 时，单个请求扫不完全池。这是**刻意的权衡**，
/// 理由见函数体内 `.min()` 处的长注释（旧代码的内层 `.max(available)` 会让硬上限
/// 自我抵消，线上 43 号时预算 = 43，一条请求顺着整池撞一遍直到耗尽 45s 墙钟，
/// 净效果是「号池越大越慢」）。
///
/// 该权衡依赖一个前提：**坏号会被自动禁用从而不进候选集**，故预算 12 足够摸到
/// 足量健康号。号池规模显著超过 `ABSOLUTE_MAX_TOTAL_RETRIES` 时需重新评估这个前提。
///
/// **小号池降重试**：号池很小（`total <= SMALL_POOL_THRESHOLD`）时，每号重试次数降为 1。
/// 因为小池下重试循环只会反复选到同几个号——被限流时多打几次纯属反复砸、加重冷却，
/// 不如让每个号各摸一次就把上游错误透传给客户端（客户端自身有退避重试，比网关内反复砸温和）。
/// 号多时行为完全不变（仍 `MAX_RETRIES_PER_CREDENTIAL`）。
fn compute_max_retries(total: usize, _available: usize) -> usize {
    // `_available` 保留在签名里但**不再参与计算**：见下方 `.min()` 处的说明。
    // 保留参数是为了不改动调用点与既有测试；将来若确认永不需要，再一并删除。
    let per_cred = if total <= SMALL_POOL_THRESHOLD {
        1
    } else {
        MAX_RETRIES_PER_CREDENTIAL
    };
    (total * per_cred)
        // ⚠️ 这里**刻意不再**用 `.max(available)` 抬高上限。
        //
        // 旧代码是 `.min(ABSOLUTE_MAX_TOTAL_RETRIES.max(available))`，那个内层
        // `.max(available)` 会在 `available > 12` 时把硬上限自己抵消掉 → 预算等于
        // 可用号数。线上 43 个号时实测预算 = 43，日志里就是「尝试 43/43」：一条
        // 客户端请求要顺着整池撞一遍，撞到 45s 墙钟预算才失败。
        //
        // 净效果是**号池越大越慢**，与"扩号池提升吞吐"的目标正好相反。而"保证每个
        // 可用号至少被摸一次"这个原始意图本身就站不住：池子有 200 个号时，为一条
        // 请求打 200 次上游只会加重风控，而不会提高这条请求的成功率——真正该做的是
        // 让坏号被自动禁用而**不进入**候选集（见 token_manager 的
        // `report_suspicious_activity`），而不是靠遍历去撞。
        .min(ABSOLUTE_MAX_TOTAL_RETRIES)
        // ⚠️ 地板 1：预算为 0 等于**一次都不尝试**，请求直接以「已达到最大重试次数（0次）」
        // 失败，而 acquire_context 的等待循环根本没机会跑。
        //
        // 旧实现喂 `total_count()`（含 disabled 条目，恒 ≥ 池内号数）所以永远算不出 0，
        // 掩盖了这里缺下限。改喂 `kiro_selectable_count()` 后，**瞬时**全池不可选
        //（全部在冷却中 / inflight 打满）会让它返回 0 → 预算 0 → 请求零重试即失败。
        // 这是真实回归：线上 20 分钟内出现 10 次该错误。
        //
        // 取 1 而非 0 的语义：至少走一遍 acquire_context，让它的等待逻辑有机会等到号
        // 出冷却；等不到再由墙钟预算（MAX_REQUEST_RETRY_BUDGET_SECS）兜底透传错误。
        .max(1)
}

/// 一次成功调用的元数据（随响应回传给上层，供用量统计埋点关联）
///
/// provider 层掌握凭据/重试/延迟，但看不到最终 usage/credits（流式消费后才知道）；
/// 上层拿到本结构后与 `StreamContext::resolved_usage()` 合并即可产出完整记录。
pub struct CallMeta {
    /// 实际服务该请求的凭据 ID
    pub credential_id: u64,
    /// 请求模型名（从请求体解析，可能为 None）
    pub model: Option<String>,
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
    /// 请求模型名(原样,透传不映射)
    pub model: Option<String>,
    /// 会话标识
    pub session_id: Option<String>,
    /// 据上游 status 推断的用量结果分类
    pub outcome: crate::usage::RequestOutcome,
    /// 从选号到拿到上游响应头的耗时(毫秒)
    pub latency_ms: u64,
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
        // 预热：构建全局代理对应的 Client
        // 对话路径用流式 client：read_timeout(空闲间隔) 而非总时长，防长流被中途掐断
        // （根因见 build_streaming_client 注释：修 `Connection closed mid-response`）。
        let initial_client = build_streaming_client(proxy.as_ref(), 720, tls_backend)
            .expect("创建 HTTP 客户端失败");
        let mut cache = HashMap::new();
        cache.insert(proxy.clone(), initial_client);

        Self {
            token_manager,
            global_proxy: proxy,
            client_cache: Mutex::new(cache),
            tls_backend,
            endpoints,
            default_endpoint,
        }
    }

    /// 根据凭据的代理配置获取（或创建并缓存）对应的 reqwest::Client
    fn client_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Client> {
        let effective = credentials.effective_proxy(self.global_proxy.as_ref());
        let mut cache = self.client_cache.lock();
        if let Some(client) = cache.get(&effective) {
            return Ok(client.clone());
        }
        let client = build_streaming_client(effective.as_ref(), 720, self.tls_backend)?;
        cache.insert(effective, client.clone());
        Ok(client)
    }

    /// 根据凭据选择 endpoint 实现
    fn endpoint_for(
        &self,
        credentials: &KiroCredentials,
    ) -> anyhow::Result<Arc<dyn KiroEndpoint>> {
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

    /// 发送非流式 API 请求
    ///
    /// 支持多凭据故障转移（见 [`Self::call_api_with_retry`]）
    pub async fn call_api(
        &self,
        request_body: &str,
        is_1m: bool,
    ) -> anyhow::Result<(reqwest::Response, CallMeta)> {
        self.call_api_with_retry(request_body, false, is_1m).await
    }

    /// 发送流式 API 请求
    pub async fn call_api_stream(
        &self,
        request_body: &str,
        is_1m: bool,
    ) -> anyhow::Result<(reqwest::Response, CallMeta)> {
        self.call_api_with_retry(request_body, true, is_1m).await
    }

    /// 发送 MCP API 请求（WebSearch 等工具调用）
    pub async fn call_mcp(&self, request_body: &str) -> anyhow::Result<reqwest::Response> {
        self.call_mcp_with_retry(request_body).await
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
    ) -> Option<(axum::response::Response, PassthroughMeta)> {
        // 从**custom_api 专属选号池**里 failover 调度(独立于 Kiro 选号,守两池隔离铁律)。
        // 语义(dwgx 定):池内按优先级+RPM 均衡选号;某号 403 额度满/401 key 失效/429/5xx →
        // 给该号短冷却 + 换下一个 custom_api;全部 custom_api 不可用 → 返回 None,由上层落 Kiro 主力路径。
        // 4xx(非 403,客户端请求错误)→ 换号也一样错,直接把该响应返给客户端(不 failover、不落 Kiro)。
        // 注:model/user_id 暂不参与 custom_api 选号(代挂上游自行处理模型),仅随 meta 供埋点关联。
        let mut excluded: HashSet<u64> = HashSet::new();
        loop {
            let (id, cred) = match self.token_manager.select_custom_api(&excluded) {
                Some(x) => x,
                // 无更多可用 custom_api 号:①一开始就没(excluded 空)→ 池里无透传号,零开销落 Kiro;
                // ②都试过失败(excluded 非空)→ custom_api 全额度满/失败,failover 落 Kiro 主力。
                None => return None,
            };
            let started = std::time::Instant::now();
            let (resp, status) = crate::kiro::passthrough::forward(
                &cred,
                raw_body.clone(),
                self.global_proxy.as_ref(),
                self.tls_backend,
            )
            .await;
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
                let meta = PassthroughMeta {
                    credential_id: id,
                    model: model.map(|s| s.to_string()),
                    session_id: user_id.map(|s| s.to_string()),
                    outcome,
                    latency_ms,
                };
                return Some((resp, meta));
            }

            // ⭐ 显式列出「该 failover 的状态码」而非用"4xx 非403"反推——后者会让 401/429 先命中
            //    下方 4xx 直返、永远到不了 failover(对抗 review B1 抓到的持久黑洞:429 号不切换)。
            // - 401 key 失效 / 402·403 额度耗尽 / 429 限流 / 5xx 上游错误 → 该号短冷却 + 换下一个 custom_api。
            // - 其余 4xx(400/404/422 等客户端请求错误)→ 换号/落 Kiro 也一样错,直接返给客户端。
            let should_failover = matches!(code, 401 | 402 | 403 | 429) || (500..600).contains(&code);
            if !should_failover {
                let meta = PassthroughMeta {
                    credential_id: id,
                    model: model.map(|s| s.to_string()),
                    session_id: user_id.map(|s| s.to_string()),
                    outcome,
                    latency_ms,
                };
                return Some((resp, meta));
            }

            // 冷却时长按性质。⭐ dwgx 定的语义:**代挂号是用户自购的付费中转站,不是 Kiro 号**,
            // 它没有"被风控"这个状态,429 只代表"它现在忙"。
            //
            // 🔴 修复:429 原先给 30s 冷却。那是把 Kiro 号的风控模型错套到代挂号上——
            // 用户已经为这个上游付过钱,把它按下 30 秒既不能让它变快,又白白缩小了可用池
            // (极端情况:两个代挂号轮流 429 → 两个都被冷却 → 整池不可用 → 回落 Kiro,
            //  而 Kiro 侧此刻可能正被风控烧号)。偶尔 429 只该 failover,不该留痕。
            //
            // 现在:429 与 5xx 同列为**瞬态**,本请求链内 exclude 换下一个号即可,零冷却。
            // "一直 429"由 record_passthrough_result 的持续过载观察窗兜住
            // (PASSTHROUGH_OVERLOAD_WINDOW 内零成功才禁用),不靠冷却。
            let cooldown_secs = match code {
                // 401 key 失效 / 402·403 额度耗尽:**非瞬态**,短期内重试必然还是失败。
                // 给冷却是为了别让同一请求链外的后续请求继续撞它;真正的处置(自动禁用)
                // 由 record_passthrough_result 的连续失败计数负责。
                401 | 402 | 403 => 180,
                // 429 / 5xx / 网络:瞬态。给一个**极短**的调度级跳过,而不是零。
                //
                // 为什么不是 0（审查发现的延迟回归）：`excluded` 只在**本请求链内**生效，
                // 跨请求不起作用。若完全不冷却，一个 100% 429 的中转站会被**每一个**新请求
                // 重新选中（select_custom_api 按 priority/RPM 排序，它排在前面），
                // 每次都白付一次上游往返才 failover —— 而持续过载的自动禁用要 300s 才生效，
                // 这 5 分钟内每个请求都要多等一个 RTT。
                //
                // 5s 是刻意取的平衡点：它**不是**惩罚（不进 health、不计失败、不影响自动禁用判据，
                // 满足"偶尔 429 绝不惩罚"），只是调度上避免同一秒内把所有请求都撞向同一个忙站；
                // 而 5s 远低于人可感知的池容量缩水（旧值 30s 才是真正的惩罚性退避）。
                429 => 5,
                // 5xx / 网络：真瞬态，可能只是抖一下，不跳过。
                _ => 0,
            };
            if cooldown_secs > 0 {
                self.token_manager.cooldown_custom_api(id, cooldown_secs);
                tracing::warn!(
                    credential_id = id,
                    status = code,
                    "自定义 API 透传失败(非瞬态),该号冷却 {}s 并 failover 下一个 custom_api",
                    cooldown_secs
                );
            } else {
                tracing::warn!(
                    credential_id = id,
                    status = code,
                    "自定义 API 透传失败(瞬态,如 429/5xx),**不冷却**,仅本请求内 failover 下一个 custom_api"
                );
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
    pub fn token_manager(&self) -> &MultiTokenManager {
        &self.token_manager
    }

    /// 内部方法：带重试逻辑的 MCP API 调用
    async fn call_mcp_with_retry(&self, request_body: &str) -> anyhow::Result<reqwest::Response> {
        let call_started = std::time::Instant::now();
        let max_retries =
            // 预算按「Kiro 路径**实际可选**的号数」算，而非 entries.len()：后者含 disabled
            // 与 custom_api 条目（is_entry_selectable 永远拒绝 custom_api），会把预算凭空
            // 抬高 —— 生产日志的 `尝试 8/36` 即由此而来。见 kiro_selectable_count 的说明。
            {
                let selectable = self.token_manager.kiro_selectable_count();
                compute_max_retries(selectable, selectable)
            };
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        // 与对话路径同款的两个链内状态：
        // - `rate_limited_this_call`：同一请求链内每个号只因风控冷却一次，不重复惩罚。
        // - `suspicious_failovers_this_call`：账户级风控的跨号转移上限，防线性扫全池。
        let mut rate_limited_this_call: HashSet<u64> = HashSet::new();
        let mut suspicious_failovers_this_call: usize = 0;
        const MAX_SUSPICIOUS_FAILOVERS_PER_CALL: usize = 3;

        for attempt in 0..max_retries {
            // MCP 调用（WebSearch 等工具）不涉及模型选择，无需按模型过滤凭据
            let ctx = match self.token_manager.acquire_context(None, None).await {
                Ok(c) => c,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, &config);

            let endpoint = match self.endpoint_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    last_error = Some(e);
                    // endpoint 解析失败：记为失败，换下一张凭据
                    self.token_manager.report_failure(ctx.id);
                    continue;
                }
            };

            let rctx = RequestContext {
                credentials: &ctx.credentials,
                token: &ctx.token,
                machine_id: &machine_id,
                config: &config,
                // MCP(WebSearch 等)不涉及模型对话上下文,无 1M 语义。
                is_1m: false,
            };

            let url = endpoint.mcp_url(&rctx);
            let body = endpoint.transform_mcp_body(request_body, &rctx);

            let base = self
                .client_for(&ctx.credentials)?
                .post(&url)
                .body(body)
                .header("content-type", "application/json");
            let request = endpoint.decorate_mcp(base, &rctx);

            let response = match request.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!(
                        "MCP 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
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
                return Ok(response);
            }

            // 失败响应
            let body = response.text().await.unwrap_or_default();

            // 402 额度用尽
            if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body) {
                let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 400 Bad Request
            if status.as_u16() == 400 {
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 401/403 凭据问题
            if matches!(status.as_u16(), 401 | 403) {
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
                    if self.token_manager.force_refresh_token_for(ctx.id).await.is_ok() {
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                    // 刷新失败 = 认证态有问题，加一段冷却让调度避开它
                    self.token_manager.report_auth_cooldown(ctx.id);
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
                        anyhow::bail!(
                            "MCP 请求失败（账户被封禁且所有凭据已用尽）: {} {}",
                            status,
                            body
                        );
                    }
                    last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                    continue;
                }

                let has_available = self.token_manager.report_failure(ctx.id);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 瞬态错误
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                tracing::warn!(
                    "MCP 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                if attempt + 1 < max_retries {
                    sleep(Self::retry_delay(attempt)).await;
                }
                continue;
            }

            // 其他 4xx
            if status.is_client_error() {
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 兜底
            last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!("MCP 请求失败：已达到最大重试次数（{}次）", max_retries)
        }))
    }

    /// 内部方法：带重试逻辑的 API 调用
    ///
    /// 重试策略：
    /// - 每个凭据最多重试 MAX_RETRIES_PER_CREDENTIAL 次
    /// - 总重试预算由 [`compute_max_retries`] 动态计算：以可用凭据数为下限，
    ///   保证每个可用凭据至少被摸一次；以 ABSOLUTE_MAX_TOTAL_RETRIES 为安全上限
    async fn call_api_with_retry(
        &self,
        request_body: &str,
        is_stream: bool,
        is_1m: bool,
    ) -> anyhow::Result<(reqwest::Response, CallMeta)> {
        let max_retries =
            // 预算按「Kiro 路径**实际可选**的号数」算，而非 entries.len()：后者含 disabled
            // 与 custom_api 条目（is_entry_selectable 永远拒绝 custom_api），会把预算凭空
            // 抬高 —— 生产日志的 `尝试 8/36` 即由此而来。见 kiro_selectable_count 的说明。
            {
                let selectable = self.token_manager.kiro_selectable_count();
                compute_max_retries(selectable, selectable)
            };
        let mut last_error: Option<anyhow::Error> = None;
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
        // MODEL_TEMPORARILY_UNAVAILABLE 全局容量问题专用计数：只允许 1 次慢速退避重试，
        // 耗尽后立即 break（而非继续烧光 max_retries 切换凭据——所有凭据受同一模型过载影响）。
        let mut model_unavailable_attempts: usize = 0;
        const MAX_MODEL_UNAVAILABLE_RETRIES: usize = 1;
        let api_type = if is_stream { "流式" } else { "非流式" };

        // 一次解析同时取出模型信息与会话标识（conversationId），避免热路径上对
        // 整个请求体做两次全量 serde_json::from_str（大请求体尤其昂贵）。
        let (model, session_id) = Self::extract_model_and_session(request_body);

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

        // 入站整形准入闸门:**整个客户端请求只过一次**(在 failover 循环外),突发被令牌桶排队削平。
        // review Finding 1 修复:不在 acquire_context 里扣(否则 failover N 跳扣 N 令牌 + fast-fail 空转白扣)。
        // 排队超时用与全池冷却同款的 retry_after_secs= 标记 → 下游归类为 RateLimited + 带 Retry-After。
        if let Err(retry_after) = self.token_manager.acquire_admission().await {
            anyhow::bail!(
                "入站限速排队超时(网关目标 {} RPM 保护上游)retry_after_secs={}",
                self.token_manager.inbound_target_rpm(),
                retry_after
            );
        }

        for attempt in 0..max_retries {
            // 与成功分支的 `retries: attempt as u32` 同口径：记「已尝试次数 - 1」＝重试次数。
            // 放在墙钟闸门**之前**递增：闸门 break 时也要反映"这一轮进来过"，
            // 否则墙钟耗尽的失败会少记一次，而那正是要观测的形态。
            attempts_used = attempt as u32;
            // 墙钟闸门：单请求重试总时长超预算就停止（把最后错误透传给客户端，
            // 让它自己退避）。防止一个卡住的请求在小号池里反复扫冷全池、把偶发 429
            // 拖成持续雪崩。首次尝试(attempt==0)不受此限，保证至少打一次。
            if attempt > 0
                && call_started.elapsed() >= std::time::Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS)
            {
                tracing::warn!(
                    "单请求重试已达墙钟预算 {}s（尝试 {}/{}），停止重试并透传上游错误，避免拖垮整池",
                    MAX_REQUEST_RETRY_BUDGET_SECS,
                    attempt,
                    max_retries
                );
                break;
            }
            // 获取调用上下文（绑定 index、credentials、token）
            let ctx = match self
                .token_manager
                .acquire_context(model.as_deref(), session_id.as_deref())
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
                    continue;
                }
            };

            // 可观测:attempt>0 且真拿到了一个号 = 一次 failover 换号(真打了下一个号)。
            // 放在 acquire_context 成功之后,避免全池冷却 continue(没拿到号)误计一跳。
            if attempt > 0 {
                crate::common::recovery_metrics::bump_failover_hop();
                real_failover_happened = true;
            }

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, &config);

            let endpoint = match self.endpoint_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    last_error = Some(e);
                    self.token_manager.report_failure(ctx.id);
                    continue;
                }
            };

            let rctx = RequestContext {
                credentials: &ctx.credentials,
                token: &ctx.token,
                machine_id: &machine_id,
                config: &config,
                is_1m,
            };

            let url = endpoint.api_url(&rctx);
            let body = endpoint.transform_api_body(request_body, &rctx);

            let base = self
                .client_for(&ctx.credentials)?
                .post(&url)
                .body(body)
                .header("content-type", endpoint.content_type());
            let request = endpoint.decorate_api(base, &rctx);

            last_credential_id = Some(ctx.id);

            let response = match request.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!(
                        "API 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    // 网络错误通常是上游/链路瞬态问题，不应导致"禁用凭据"或"切换凭据"
                    // （否则一段时间网络抖动会把所有凭据都误禁用，需要重启才能恢复）
                    last_error = Some(e.into());
                    last_outcome = crate::usage::RequestOutcome::NetworkError;
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
                let meta = CallMeta {
                    credential_id: ctx.id,
                    model: model.clone(),
                    session_id: session_id.clone(),
                    is_streaming: is_stream,
                    retries: attempt as u32,
                    latency_ms: call_started.elapsed().as_millis() as u64,
                    started_at: call_started,
                    // 移交在途守卫：从此随响应流存活，流真正消费完才 -1
                    inflight: ctx.inflight,
                };
                return Ok((response, meta));
            }

            // 失败响应：先从响应头提取 Retry-After（body 消费后头就没了），再读取 body
            let retry_after_header = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok());
            let body = response.text().await.unwrap_or_default();

            // 客户端请求校验错误（如 TOOL_USE_RESULT_MISMATCH）：请求构造问题，
            // 换号/重试都只会重复失败并浪费配额，立即终止（不计凭据失败）。
            if endpoint.is_client_validation_error(&body) {
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
                break;
            }

            // 账户级临时风控限速（suspicious activity + temporary limits）：
            // ⚠️ 必须在 is_account_suspended 之前判定，否则含 "suspended...suspicious
            // activity" 的临时限速文案会被误判成永久封禁，白冻一个还能用的号 24h。
            // 处置：只设短冷却 + 立即 failover，不禁用、不计永久失败。
            if endpoint.is_temporary_rate_limit(&body) {
                tracing::warn!(
                    "API 请求失败（账户临时风控限速，非永久封禁；短冷却后 failover，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                last_outcome = crate::usage::RequestOutcome::RateLimited;
                // 账户级风控也是上游限速信号 → 入站整形 RPM 自动降档。
                self.token_manager.report_upstream_rate_limited();
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
                    break;
                }
                if attempt + 1 < max_retries {
                    sleep(Self::retry_delay(attempt)).await;
                }
                continue;
            }

            // 注：524 网关超时（Cloudflare 等）落入下方通用 5xx 分支即按可重试瞬态
            // 错误处理（不禁用、退避后换号），无需单列——与通用路径行为一致。

            // 402 Payment Required 且额度用尽：禁用凭据并故障转移
            if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body) {
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
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败（所有凭据已用尽）: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    break;
                }
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                continue;
            }

            // 账户被暂停/封禁：不论状态码，body 命中 suspend 信号即直接禁用并转移
            // （不可自动恢复，等待人工处理，避免反复打已封的号）
            if endpoint.is_account_suspended(&body) {
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
                self.token_manager.report_upstream_pressure();
                let has_available = self.token_manager.report_account_suspended(ctx.id);
                if !has_available {
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败（账户被封禁且所有凭据已用尽）: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    break;
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
                    break;
                }
                suspended_this_call = true;
                tokio::time::sleep(Self::retry_delay(attempt)).await;
                continue;
            }

            // 400 INVALID_MODEL_ID：该号已不能服务请求的模型（多为订阅取消/降级）。
            // 不是客户端请求错误——换个订阅仍有效的号往往能成功。故给该号冷却 + failover，
            // 而非直接把 400 透传（那样坏号还留在轮转里，下个请求又命中它）。
            // 只有当所有号都返回它（report 返回 has_available=false）时，才是模型本身无效、透传。
            if status.as_u16() == 400 && endpoint.is_invalid_model_id(&body) {
                last_outcome = crate::usage::RequestOutcome::BadRequest;
                // 模型级处置：只把"该号+该模型"记进短期黑名单并 failover 到对此模型仍可用的号；
                // 绝不冷却/禁用整个号（该号对其它模型照常可用）。返回 false = 所有未禁用号都已对
                // 此模型进黑名单 → 说明是模型本身无效，透传真 400 给客户端(而非 429/502 死循环)。
                let has_available_for_model =
                    self.token_manager.report_model_invalid(ctx.id, model.as_deref());
                if !has_available_for_model {
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败（模型 {:?} 对所有号均 INVALID_MODEL_ID，判定模型无效）: {} {}",
                        api_type,
                        model.as_deref().unwrap_or(""),
                        status,
                        body
                    ));
                    // 透传真实 400：这是客户端请求了一个所有号都不支持的模型，重试无意义。
                    break;
                }
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败（凭据 #{} 对模型 {:?} INVALID_MODEL_ID，切换到仍支持的号）: {} {}",
                    api_type,
                    ctx.id,
                    model.as_deref().unwrap_or(""),
                    status,
                    body
                ));
                continue;
            }

            // 400 Bad Request - 其它请求问题（客户端构造错误），重试/切换凭据无意义
            if status.as_u16() == 400 {
                last_outcome = crate::usage::RequestOutcome::BadRequest;
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                break;
            }

            // 401/403 - 更可能是凭据/权限问题：计入失败并允许故障转移
            if matches!(status.as_u16(), 401 | 403) {
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
                    let corrected = self.token_manager.sync_region_from_arn_for(ctx.id);
                    self.token_manager.mark_usage_403_feature_not_supported(ctx.id);
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
                            api_type, status, body
                        ));
                        // continue → 下一轮 acquire_context 重克隆已改好 region 的 creds(不复用旧 ctx/url)。
                        continue;
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
                    self.token_manager.report_auth_cooldown(ctx.id);
                    last_error = Some(anyhow::anyhow!(
                        "{} 403 FEATURE_NOT_SUPPORTED(region 未开通,冷却换号,后台重探中): {} {}",
                        api_type, status, body
                    ));
                    // continue:下一轮 acquire_context 选别的号;全池不可用时由 max_retries/墙钟兜底透传。
                    continue;
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
                    if self.token_manager.force_refresh_token_for(ctx.id).await.is_ok() {
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                    // 刷新失败 = 认证态有问题，加一段冷却让调度避开它
                    self.token_manager.report_auth_cooldown(ctx.id);
                }

                last_outcome = crate::usage::RequestOutcome::AuthFailed;
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
                    break;
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
                continue;
            }

            // 503 MODEL_TEMPORARILY_UNAVAILABLE — 模型容量问题，非凭据问题。
            // 使用慢速退避（1s base）；不调用 report_failure / report_rate_limited，
            // 不影响凭据健康分（健康分反映凭据质量，与模型过载无关）。
            // 只允许 MAX_MODEL_UNAVAILABLE_RETRIES 次慢速重试，耗尽后直接 break 透传错误——
            // 继续切换凭据无意义（所有凭据对同一过载模型等价）。
            if status.as_u16() == 503 && endpoint.is_model_temporarily_unavailable(&body) {
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
                    break;
                }
                // 慢速退避：1s base，比通用 200ms 更长，避免反复冲击过载路径。
                sleep(Self::retry_delay_model_unavailable(model_unavailable_attempts - 1)).await;
                continue;
            }

            // 429/408/5xx - 瞬态上游错误：重试但不禁用或切换凭据
            // （避免 429 high traffic / 502 high load 等瞬态错误把所有凭据锁死）
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                tracing::warn!(
                    "API 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                // 429 限流：给该凭据设置短冷却，让调度优先换用其它凭据
                // （仍不禁用、不计永久失败，冷却到期自动恢复）
                if status.as_u16() == 429 {
                    last_outcome = crate::usage::RequestOutcome::RateLimited;
                    // 上游 429 → 入站整形 RPM 自动挡乘性降档(削平后续入站速率,别继续挤爆上游)。
                    self.token_manager.report_upstream_rate_limited();
                    // 优先用上游给出的精确重置时间：响应头 Retry-After 优先，其次错误 body
                    let retry_after = retry_after_header
                        .or_else(|| endpoint.extract_retry_after_secs(&body));
                    // 本请求链内该号首次 429 才设冷却；再次 429 只换号 failover，不重复累加
                    // trigger_count / 延长冷却（见 rate_limited_this_call 定义处的根因说明）。
                    if rate_limited_this_call.insert(ctx.id) {
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
                        self.token_manager.report_upstream_pressure();
                    }
                }
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                if attempt + 1 < max_retries {
                    sleep(Self::retry_delay(attempt)).await;
                }
                continue;
            }

            // 其他 4xx - 通常为请求/配置问题：直接返回，不计入凭据失败
            if status.is_client_error() {
                last_outcome = crate::usage::RequestOutcome::BadRequest;
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                break;
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
        }

        // 所有重试都失败:埋点一条失败记录后返回错误。
        // 可观测:仅当真的换号 failover 过(打了 >1 个号)才计「耗尽」——首个号即因客户端错误/
        // 模型无效 break 的不算池耗尽,避免运维看错(误判池死实为客户端请求问题)。
        if real_failover_happened {
            crate::common::recovery_metrics::bump_failover_exhausted();
        }

        // overload_fallback_model：MODEL_TEMPORARILY_UNAVAILABLE 耗尽重试预算后，
        // 若配置了备用模型，以备用模型做最后一次尝试（限 1 次，不再套完整 failover 循环）。
        // 典型用途：opus 系列过载时切到容量独立的 sonnet（前提：用户已知晓响应质量/计费差异）。
        if last_outcome == crate::usage::RequestOutcome::ModelUnavailable {
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
                        match request.send().await {
                            Ok(resp) if resp.status().is_success() => {
                                self.token_manager.report_success(ctx.id);
                                let meta = CallMeta {
                                    credential_id: ctx.id,
                                    model: Some(fallback_model_id.clone()),
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
                                tracing::warn!(
                                    "overload_fallback_model {} 也失败: {}",
                                    fallback_model_id,
                                    resp.status()
                                );
                            }
                            Err(e) => {
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

        let final_error = last_error.unwrap_or_else(|| {
            anyhow::anyhow!(
                "{} API 请求失败：已达到最大重试次数（{}次）",
                api_type,
                max_retries
            )
        });
        let mut fail_record = crate::usage::RequestRecord::new(
            uuid::Uuid::new_v4().to_string(),
            model.clone().unwrap_or_default(),
        );
        fail_record.credential_id = last_credential_id;
        fail_record.session_id = session_id.clone();
        fail_record.is_streaming = is_stream;
        fail_record.latency_ms = call_started.elapsed().as_millis() as u64;
        fail_record.outcome = last_outcome;
        // ⭐ 失败记录必须带真实换号次数。此前这里没有设 `retries` → 恒为默认 0，
        // 使「烧掉 12 次换号才失败」与「第一次就失败」在面板上不可区分。
        // 与成功分支 `retries: attempt as u32`（本文件下方）同口径。
        fail_record.retries = attempts_used;
        fail_record.error_message = Some(final_error.to_string());
        crate::usage::emit_record(fail_record);

        Err(final_error)
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
    /// 用于 overload_fallback_model：过载重试耗尽时，以备用模型再试一次。
    /// 替换路径：`conversationState.currentMessage.userInputMessage.modelId`。
    /// 解析/序列化失败时原样返回，保证函数不 panic。
    fn rewrite_model_id(request_body: &str, new_model: &str) -> String {
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(request_body) else {
            return request_body.to_string();
        };
        if let Some(mid) = v.pointer_mut(
            "/conversationState/currentMessage/userInputMessage/modelId",
        ) {
            *mid = serde_json::Value::String(new_model.to_string());
        }
        serde_json::to_string(&v).unwrap_or_else(|_| request_body.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 预算恒被 `ABSOLUTE_MAX_TOTAL_RETRIES` 封顶，**且刻意不再随可用号数抬高**。
    ///
    /// ⚠️ 本测试此前名为 `..._covers_every_available_credential`，断言 `r >= total`
    /// 并声称"保证每个可用凭据至少被尝试一次"。那个承诺在移除内层 `.max(available)`
    /// 之后已不成立 —— 它当时**只是碰巧通过**：`total=10` 时预算 `min(30,12)=12`，
    /// 而 `12 >= 10` 恰好为真。把 `total` 改成 20 就会失败（预算仍 12 < 20），
    /// 即那是个会在号池扩容时才爆的定时炸弹，且它在维护一条代码已不提供的不变式。
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

        // 刚过小池阈值（total=4）恢复常规 total*MAX_RETRIES_PER_CREDENTIAL。
        assert_eq!(compute_max_retries(4, 4), 4 * MAX_RETRIES_PER_CREDENTIAL);

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

    /// 回归（大号池不得放大重试 · 本轮核心）：预算恒 ≤ 12，与池子大小无关。
    ///
    /// **旧代码为何失败**：`.min(ABSOLUTE_MAX_TOTAL_RETRIES.max(available))` 里的内层
    /// `.max(available)` 在 `available > 12` 时把硬上限自己抵消掉 → 预算 = available。
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
            "43 号池（线上实测规模）预算必须是 12 而非 43"
        );
    }

    #[test]
    fn test_extract_model_and_session_both_present() {
        // 一次解析应同时取出 modelId 与 conversationId（与旧双解析等价）
        let body = r#"{
            "conversationState": {
                "conversationId": "sess-123",
                "currentMessage": {
                    "userInputMessage": { "modelId": "claude-sonnet-4" }
                }
            }
        }"#;
        let (model, session) = KiroProvider::extract_model_and_session(body);
        assert_eq!(model.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(session.as_deref(), Some("sess-123"));
    }

    #[test]
    fn test_extract_model_and_session_partial() {
        // 只有 conversationId、无 modelId：model=None、session=Some
        let only_session = r#"{"conversationState":{"conversationId":"s1"}}"#;
        let (model, session) = KiroProvider::extract_model_and_session(only_session);
        assert_eq!(model, None);
        assert_eq!(session.as_deref(), Some("s1"));

        // 只有 modelId、无 conversationId：model=Some、session=None
        let only_model = r#"{"conversationState":{"currentMessage":{"userInputMessage":{"modelId":"m"}}}}"#;
        let (model, session) = KiroProvider::extract_model_and_session(only_model);
        assert_eq!(model.as_deref(), Some("m"));
        assert_eq!(session, None);
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

}

