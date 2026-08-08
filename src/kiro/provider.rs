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
use crate::kiro::endpoint::{ENDPOINT_FALLBACK_ORDER, KiroEndpoint, RequestContext};
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::prompt_cache::{PromptCacheProbe, PromptCacheTracker, PromptCacheUsage};
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::TlsBackend;
use parking_lot::Mutex;

/// 死端点负缓存 TTL（30 分钟）。连接层失败通常表示 DNS 不存在（如 codewhisperer.eu-central-1）
/// 或 host 路由黑洞，但配置/网络可能临时修复，过期后自动重试。
const DEAD_ENDPOINT_TTL: Duration = Duration::from_secs(1800);

/// 协议不符隔离 TTL（30 分钟）。
///
/// 上游对某 (端点, region) 返回的不是 event-stream 而是 JSON/文本（协议降级），
/// 说明这条**路由**当前不可用于对话。与 `dead_endpoints` 同为自动过期的软隔离：
/// 上游修好、或部署方改了配置后，过期即自动重试，无需人工介入也无需重启。
const PROTOCOL_BROKEN_TTL: Duration = Duration::from_secs(1800);

/// HTTP Client 缓存容量上限。
///
/// 【为何需要上限】key 是**生效后的代理配置**，而代理可以配在每个凭据上。
/// 正常部署里代理种类是个小常数（直连 + 一两个出口），缓存自然收敛。但只要有
/// 「每个号一个代理」或「代理 URL 带轮换参数」的用法，这个表就是随时间单调增长的——
/// 而每个条目是一个 `reqwest::Client`，**自带一个连接池**。泄漏的不只是几百字节
/// 的 HashMap 条目，是文件描述符和空闲 TCP 连接。
///
/// 32 的依据：正常部署远用不到，异常用法下把内存/fd 占用钉在一个可预期的常数上。
const CLIENT_CACHE_CAP: usize = 32;

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
const ABSOLUTE_MAX_TOTAL_RETRIES: usize = 64;

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
/// 预算 = `(total * per_cred)`，但以 `available` 做下限，
/// 数学上保证每个可用凭据至少被尝试一次；上限为 `ABSOLUTE_MAX_TOTAL_RETRIES`，
/// 但当可用凭据数超过该上限时仍以 `available` 为准，绝不因硬上限漏掉可用号。
///
/// **小号池降重试**：号池很小（`total <= SMALL_POOL_THRESHOLD`）时，每号重试次数降为 1。
/// 因为小池下重试循环只会反复选到同几个号——被限流时多打几次纯属反复砸、加重冷却，
/// 不如让每个号各摸一次就把上游错误透传给客户端（客户端自身有退避重试，比网关内反复砸温和）。
/// 号多时行为完全不变（仍 `MAX_RETRIES_PER_CREDENTIAL`）。
fn compute_max_retries(total: usize, available: usize) -> usize {
    let per_cred = if total <= SMALL_POOL_THRESHOLD {
        1
    } else {
        MAX_RETRIES_PER_CREDENTIAL
    };
    (total * per_cred)
        .max(available)
        .min(ABSOLUTE_MAX_TOTAL_RETRIES.max(available))
}

/// 缓存满时淘汰**最久未用**的一条，为新条目腾位。
///
/// # 为什么这个缓存需要上限
/// key 是「生效的代理配置」。常规部署下代理种类是个小的固定集合，永远碰不到上限，
/// 所以这是纯兜底、不改变正常路径行为。但若部署方用「每次轮换一个代理 URL」的方式
/// 出网，这个 map 会随时间单调增长，而每个条目是一个自带连接池的 `reqwest::Client`
/// ——泄漏的是 fd 和空闲 TCP 连接，不只是内存。
///
/// # 为什么淘汰被移除的 Client 是安全的
/// 从 map 里移除只是「不再复用它」。若仍有在途请求持有克隆，那些请求完全不受影响：
/// `Client` 内部是 Arc 语义，最后一个持有者走完才真正释放连接池。
///
/// # 为什么按 `last_used` 而不是插入时刻
/// 全局代理对应的 Client 往往是**最早创建**也**最热**的那一个。按创建时刻淘汰会
/// 优先干掉它，然后立刻又要重建——正好是最坏的选择。
///
/// 泛型化到 `K`/`V` 只为可测：真实类型里 `V = Client` 造不出来（要建真实连接池），
/// 而淘汰策略本身与 value 类型无关。
fn evict_lru_if_full<K: Clone + Eq + std::hash::Hash, V>(
    cache: &mut HashMap<K, (V, Instant)>,
    cap: usize,
) {
    // `>=` 而非 `>`：调用方紧接着要插入一条，先腾位才不会瞬时超出容量。
    if cache.len() < cap {
        return;
    }
    if let Some(victim) = cache
        .iter()
        .min_by_key(|(_, (_, last_used))| *last_used)
        .map(|(k, _)| k.clone())
    {
        cache.remove(&victim);
        tracing::debug!("client_cache 达到上限 {}，淘汰最久未用的条目", cap);
    }
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
    /// 实际服务本请求的端点名 + upstream region。
    ///
    /// 响应体是流式消费的，协议不符（上游把 event-stream 降级成 JSON/文本）只能在
    /// **解码阶段**才发现，那时 provider 早已返回。handler 据此把"这条路由坏了"
    /// 回报给 provider（见 [`KiroProvider::report_protocol_mismatch`]），实现
    /// 自动隔离 + 自动换路，而不是每个请求重踩同一个坑。
    pub endpoint_name: String,
    pub upstream_region: String,
    /// 基于本次实际发送报文得到的缓存探针。只读命中来自此前完整成功提交的记录；
    /// 本探针本身要等当前响应完整成功后才允许提交。
    pub(crate) prompt_cache_probe: Option<PromptCacheProbe>,
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

/// Kiro API Provider
///
/// 核心组件，负责与 Kiro API 通信
/// 支持多凭据故障转移和重试机制
/// 按凭据 `endpoint` 字段选择 [`KiroEndpoint`] 实现
pub struct KiroProvider {
    token_manager: Arc<MultiTokenManager>,
    /// 全局代理配置（用于凭据无自定义代理时的回退）
    global_proxy: Option<ProxyConfig>,
    /// Client 缓存：key = effective proxy config, value = (Client, 最后使用时刻)。
    /// 不同代理配置的凭据使用不同的 Client，共享相同代理的凭据复用 Client。
    ///
    /// 带 LRU 淘汰，容量见 [`CLIENT_CACHE_CAP`]——每个条目自带一个连接池，
    /// 无上限增长泄漏的是 fd 和空闲 TCP 连接，不只是内存。
    client_cache: Mutex<HashMap<Option<ProxyConfig>, (Client, Instant)>>,
    /// TLS 后端配置
    tls_backend: TlsBackend,
    /// 端点实现注册表（key: endpoint 名称）
    endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
    /// 默认端点名称（凭据未指定 endpoint 时使用）
    default_endpoint: String,
    /// DNS/连接层失败的端点负缓存（key: "endpoint_name@region", value: 首次失败时刻）。
    /// 连接层失败通常表示 DNS 不存在（如 eu-central-1 的 codewhisperer）或
    /// host 路由黑洞，端点回退逐一尝试会在每个请求上重复白跑 connect timeout。
    /// 记住首次失败的端点，30 分钟内跳过（避免瞬时网络抖动永久拉黑，过期自动重试）。
    dead_endpoints: Mutex<HashMap<String, Instant>>,
    /// 协议不符的端点隔离缓存（key: "endpoint_name@region", value: 首次判定时刻）。
    ///
    /// 与 [`Self::dead_endpoints`] 互补：那个管「连不上」（DNS/TCP/TLS），这个管
    /// 「连上了但说的不是同一种协议」——上游返回 HTTP 2xx 却给出 JSON/文本而非
    /// AWS event-stream（生产实证 2026-08-04：`cli` 端点 845 次）。这类响应会被
    /// 记成功、健康分只升不降，调度持续把流量喂给一条注定截断的路由。
    ///
    /// 隔离是**软**的且带 TTL：期内该 (端点, region) 在回退链里被降级到最后，
    /// 期满自动放行重试。上游修好、配置改对都能自愈，无需人工干预或重启。
    protocol_broken: Mutex<HashMap<String, Instant>>,
    /// Kiro 不回传 cache hit/miss；此账本只维护“最终报文前缀 + 完整成功”的本地可验证状态。
    prompt_cache: PromptCacheTracker,
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
        let initial_client =
            build_streaming_client(proxy.as_ref(), 720, tls_backend).expect("创建 HTTP 客户端失败");
        let mut cache = HashMap::new();
        cache.insert(proxy.clone(), (initial_client, Instant::now()));

        Self {
            token_manager,
            global_proxy: proxy,
            client_cache: Mutex::new(cache),
            tls_backend,
            endpoints,
            default_endpoint,
            dead_endpoints: Mutex::new(HashMap::new()),
            protocol_broken: Mutex::new(HashMap::new()),
            prompt_cache: PromptCacheTracker::default(),
        }
    }

    /// 返回本次调用在发送前查询到的严格本地缓存状态，并钳制到总输入 token 范围。
    /// 该值是 inferred（Kiro 未提供服务端回执），但不会由历史长度直接猜测。
    pub(crate) fn prompt_cache_usage(
        &self,
        meta: &CallMeta,
        total_input_tokens: i32,
    ) -> Option<PromptCacheUsage> {
        let raw = meta.prompt_cache_probe.as_ref()?.usage;
        let total = total_input_tokens.max(0);
        let read = raw.cache_read_input_tokens.clamp(0, total);
        let creation = raw
            .cache_creation_input_tokens
            .clamp(0, total.saturating_sub(read));
        Some(PromptCacheUsage {
            cache_read_input_tokens: read,
            cache_creation_input_tokens: creation,
        })
    }

    /// 当前调用完整成功后提交其前缀；失败、截断和下游提前断开均不得调用。
    pub(crate) fn commit_prompt_cache(&self, meta: &CallMeta) {
        if let Some(probe) = &meta.prompt_cache_probe {
            self.prompt_cache.commit(probe);
        }
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

    /// 记一次 (端点, region) 连接层失败。仅用于**连接层**失败（DNS/TCP/TLS），
    /// HTTP 状态码错误（429/5xx）绝不进这里——那是容量问题，host 本身是好的。
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

    /// 记一次 (端点, region) 协议不符。仅由解码层在**确定性**判据命中时回报
    /// （首字节不可能属于合法帧长度，见 `parser::frame::sniff_non_event_stream`），
    /// 绝不因业务错误码或偶发截断进入这里。
    fn mark_route_protocol_broken(&self, endpoint_name: &str, region: &str) {
        let key = format!("{}@{}", endpoint_name, region);
        self.protocol_broken
            .lock()
            .insert(key, std::time::Instant::now());
    }

    /// 根据凭据的代理配置获取（或创建并缓存）对应的 reqwest::Client。
    ///
    /// 命中时顺手刷新 `last_used`，这样 LRU 淘汰的是真正冷掉的条目，
    /// 而不是「最早被创建」的那个（全局代理往往是第一个建的，也是最热的）。
    ///
    /// 缓存有容量上限，见 [`evict_lru_if_full`]。
    fn client_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Client> {
        let effective = credentials.effective_proxy(self.global_proxy.as_ref());
        let mut cache = self.client_cache.lock();
        if let Some((client, last_used)) = cache.get_mut(&effective) {
            *last_used = Instant::now();
            return Ok(client.clone());
        }

        // 未命中：先腾位再插入，避免瞬时超出容量。
        evict_lru_if_full(&mut cache, CLIENT_CACHE_CAP);

        let client = build_streaming_client(effective.as_ref(), 720, self.tls_backend)?;
        cache.insert(effective, (client.clone(), Instant::now()));
        Ok(client)
    }

    /// 根据凭据选择 endpoint 实现
    fn endpoint_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Arc<dyn KiroEndpoint>> {
        let name = credentials
            .endpoint
            .as_deref()
            .unwrap_or(&self.default_endpoint);
        self.endpoints
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("未知端点: {}", name))
    }

    /// 构造本次调用的端点回退链
    ///
    /// 以凭据/配置指定的端点为链首（保持既有行为不变），其余已注册的备用端点按
    /// [`ENDPOINT_FALLBACK_ORDER`] 顺序补齐。`endpoint_fallback = false` 或只注册了
    /// 一个端点时退化为单元素链，行为与改动前完全一致。
    fn endpoint_chain_for(
        &self,
        credentials: &KiroCredentials,
        fallback_enabled: bool,
        upstream_region: &str,
    ) -> anyhow::Result<Vec<Arc<dyn KiroEndpoint>>> {
        let primary = self.endpoint_for(credentials)?;
        let primary_is_cli = primary.name() == crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;

        // cli 端点历史上被排除在回退链外（单元素链）。但「主端点已被证实说的不是
        // event-stream 协议」时，单元素链意味着**没有任何出路**——每个请求都必然
        // 撞同一面墙，凭据被反复选中、反复返回截断响应（生产实证 2026-08-04：
        // #142 是唯一 cli 号，845 次解码器停止，failureCount 始终 0）。
        //
        // 因此这里只在**已证实协议不符**时才为 cli 补上回退链：是纯增量的逃生通道，
        // 不改变任何当前正常工作的路径；隔离 TTL 到期后自动恢复原有单元素行为。
        let primary_broken = self.is_route_protocol_broken(primary.name(), upstream_region);
        if !fallback_enabled || (primary_is_cli && !primary_broken) {
            return Ok(vec![primary]);
        }

        let primary_name = primary.name();
        // 主端点协议不符 → 它排在链尾（仍保留兜底位，避免整条链为空无人发送）。
        let mut chain = if primary_broken {
            tracing::warn!(
                "端点 {} 在 region {} 处于协议不符隔离期，本次请求优先改走回退端点",
                primary_name,
                upstream_region
            );
            Vec::new()
        } else {
            vec![primary.clone()]
        };
        for name in ENDPOINT_FALLBACK_ORDER {
            if *name == primary_name {
                continue;
            }
            // 同样跳过其它已知协议不符的端点（链尾兜底除外，见下）。
            if self.is_route_protocol_broken(name, upstream_region) {
                continue;
            }
            if let Some(ep) = self.endpoints.get(*name) {
                chain.push(ep.clone());
            }
        }
        // 兜底铁律：链绝不为空（否则 response 恒 None，请求无人发送）。
        if chain.is_empty() {
            chain.push(primary);
        }
        Ok(chain)
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
            let should_failover =
                matches!(code, 401 | 402 | 403 | 429) || (500..600).contains(&code);
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

            // 冷却时长按性质:额度/认证类恢复慢给长冷却,限流中等,5xx 瞬态给短冷却。
            let cooldown_secs = match code {
                401 | 402 | 403 => 180, // 额度用尽/key 失效:短期内重试仍会失败,冷却久点避免频繁撞
                429 => 30,              // 限流:中等退避
                _ => 15,                // 5xx / 网络:瞬态,短冷却快速再参与
            };
            self.token_manager.cooldown_custom_api(id, cooldown_secs);
            tracing::warn!(
                credential_id = id,
                status = code,
                "自定义 API 透传失败,该号冷却 {}s 并 failover 下一个 custom_api",
                cooldown_secs
            );
            excluded.insert(id);
            // 丢弃本次错误响应,继续循环试下一个 custom_api;全部试完 select 返 None → 落 Kiro。
        }
    }

    /// 累加一次请求的真实 credit 花费到该凭据的生命周期累计（透传到 token_manager）。
    ///
    /// handler 在请求完成、从上游 meteringEvent 拿到真实计费量后调用；provider 持有
    /// token_manager，handler 只有 provider，故在此开一个薄 passthrough。
    /// 回报一次「上游响应不是 event-stream」的协议不符（由 handler 在解码阶段发现）。
    ///
    /// 这是**全自动自愈**的入口。协议不符意味着这条 (端点, region) 路由本身坏了——
    /// 上游用 200 回了一段 JSON/文本，凭据本身完全正常。处置：
    ///
    /// 1. **隔离路由**：把 (端点, region) 记入检疫表，后续请求自动绕开（TTL 到期自动重试，
    ///    上游修好后无需人工干预）。
    /// 2. **记凭据失败**：让健康分如实下降、触发 failover。这修的是最恶劣的那个洞——
    ///    旧实现走 `status.is_success()` 就 `report_success`，坏号 `failureCount` 恒为 0、
    ///    健康分只升不降，调度器于是持续挑它，每次都回截断响应（生产实证 2026-08-04：
    ///    #142 累计 845 次解码器停止、`failureCount` 始终为 0）。
    ///
    /// 幂等且线程安全；同一路由重复回报只刷新检疫起始时刻。
    pub fn report_protocol_mismatch(&self, meta: &CallMeta, detail: &str) {
        self.mark_route_protocol_broken(&meta.endpoint_name, &meta.upstream_region);
        tracing::error!(
            "凭据 #{} 经端点 {}@{} 返回非 event-stream 响应，已隔离该路由 {}s 并记一次失败: {}",
            meta.credential_id,
            meta.endpoint_name,
            meta.upstream_region,
            PROTOCOL_BROKEN_TTL.as_secs(),
            detail
        );
        self.token_manager.report_failure(meta.credential_id);
    }

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
        let total_credentials = self.token_manager.total_count();
        let max_retries =
            compute_max_retries(total_credentials, self.token_manager.available_count());
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();

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
                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                // API Key 凭据无 refresh_token，跳过强刷避免无意义重试+24h冷却
                if endpoint.is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
                    force_refreshed.insert(ctx.id);
                    if ctx.credentials.is_api_key_credential() {
                        tracing::warn!(
                            "凭据 #{} (API Key) bearer token 被上游拒绝，跳过强刷（API Key 不支持刷新）",
                            ctx.id
                        );
                    } else {
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
                        // 刷新失败 = 认证态有问题，加一段冷却让调度避开它
                        self.token_manager.report_auth_cooldown(ctx.id);
                    }
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
        let total_credentials = self.token_manager.total_count();
        let max_retries =
            compute_max_retries(total_credentials, self.token_manager.available_count());
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        // 本次请求重试链内「已因 429 冷却过」的凭据集合。防止同一个请求的一条重试链
        // 反复砸同一个号、把同一次限流事件当成多次独立事件累加 trigger_count / 指数延长冷却
        // （根因：小号池下重试循环反复选到同两个号，单请求就把 trigger_count 刷到 7、冷却 15→72s，
        //  自造雪崩）。首次 429 才设冷却，同链再 429 只换号 failover，不重复惩罚。
        // 跨请求（新请求 = 新集合）仍正常累加，保留「持续被限流的号冷却渐长」的合理行为。
        let mut rate_limited_this_call: HashSet<u64> = HashSet::new();
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
            // 墙钟闸门：单请求重试总时长超预算就停止（把最后错误透传给客户端，
            // 让它自己退避）。防止一个卡住的请求在小号池里反复扫冷全池、把偶发 429
            // 拖成持续雪崩。首次尝试(attempt==0)不受此限，保证至少打一次。
            if attempt > 0
                && call_started.elapsed()
                    >= std::time::Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS)
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

            let upstream_region = ctx.credentials.effective_upstream_region(&config);

            let chain = match self.endpoint_chain_for(
                &ctx.credentials,
                config.endpoint_fallback,
                &upstream_region,
            ) {
                Ok(c) => c,
                Err(e) => {
                    last_error = Some(e);
                    self.token_manager.report_failure(ctx.id);
                    continue;
                }
            };

            last_credential_id = Some(ctx.id);

            // 端点级回退：上游 429/5xx 多为端点容量问题，先在同一凭据上换端点重试，
            // 不消耗凭据重试预算（max_retries）、不设凭据冷却、不扣健康分。只有整条
            // 端点链都失败时才把最后一次响应交给下面的凭据级错误分类逻辑处理。
            let mut endpoint = chain[0].clone();
            let mut response: Option<reqwest::Response> = None;
            let mut prompt_cache_probe: Option<PromptCacheProbe> = None;
            let last_idx = chain.len() - 1;

            for (idx, candidate) in chain.iter().enumerate() {
                // 死端点负缓存：该 (端点, region) 近期连接层失败过 → 跳过本跳。
                // 生产实证：codewhisperer.eu-central-1.amazonaws.com 根本不存在（DNS 无记录），
                // 每个请求都要白跑一次 DNS 失败才继续，等于回退链凭空多一跳延迟且毫无收益。
                // 注意：链尾绝不跳过——否则整条链无人发送，response 恒 None（见下方 else 分支）。
                if idx != last_idx && self.is_endpoint_dead(candidate.name(), &upstream_region) {
                    tracing::debug!(
                        "端点 {} 在 region {} 近期连接失败（负缓存 {}s 内），跳过本跳",
                        candidate.name(),
                        upstream_region,
                        DEAD_ENDPOINT_TTL.as_secs()
                    );
                    continue;
                }
                // 协议隔离：该 (端点, region) 近期被证实返回非 event-stream 响应 → 跳过本跳。
                // 与死端点负缓存同构，但触发条件不同：那个是"连不上"，这个是"连上了但
                // 说的不是同一种协议"。链尾同样绝不跳过（否则 response 恒 None）。
                if idx != last_idx
                    && self.is_route_protocol_broken(candidate.name(), &upstream_region)
                {
                    tracing::debug!(
                        "端点 {} 在 region {} 近期返回非 event-stream 响应（协议隔离 {}s 内），跳过本跳",
                        candidate.name(),
                        upstream_region,
                        PROTOCOL_BROKEN_TTL.as_secs()
                    );
                    continue;
                }
                let rctx = RequestContext {
                    credentials: &ctx.credentials,
                    token: &ctx.token,
                    machine_id: &machine_id,
                    config: &config,
                    is_1m,
                };

                let url = candidate.api_url(&rctx);
                let body = candidate.transform_api_body(request_body, &rctx);
                let candidate_cache_probe = config
                    .prompt_cache_enabled
                    .then(|| {
                        self.prompt_cache.probe(
                            &body,
                            ctx.id,
                            candidate.name(),
                            &upstream_region,
                            is_1m,
                            Duration::from_secs(config.prompt_cache_ttl_seconds),
                        )
                    })
                    .flatten();

                // content-type 由端点声明（单一真相源）。历史缺陷：这里硬编码
                // application/json，而 cli 端点在 decorate_api 里又 append 了
                // x-amz-json-1.0 —— reqwest 的 .header() 是 append 而非 insert，
                // 于是请求带**两个** content-type。上游取第一个（application/json），
                // Coral 框架不认该操作，回 200 + {"Output":{...UnknownOperationException}}，
                // 被当成功记账后喂进 event-stream 解码器（生产实证 2026-08-04）。
                let base = self
                    .client_for(&ctx.credentials)?
                    .post(&url)
                    .body(body)
                    .header("content-type", candidate.content_type());
                let request = candidate.decorate_api(base, &rctx);

                match request.send().await {
                    Ok(resp) => {
                        let status = resp.status();
                        let transient =
                            matches!(status.as_u16(), 408 | 429) || status.is_server_error();
                        // 非瞬态（含成功）→ 就用这个响应，交给下游逻辑。
                        // 瞬态且还有备用端点 → 换下一个端点重试。
                        // 拿到 HTTP 响应 = 连接层通的（哪怕是 429/5xx）→ 清负缓存。
                        // 负缓存只针对"连不上"（DNS/TCP/TLS），绝不针对上游返回的业务错误。
                        self.mark_endpoint_alive(candidate.name(), &upstream_region);
                        if !transient || idx == last_idx {
                            endpoint = candidate.clone();
                            response = Some(resp);
                            prompt_cache_probe = candidate_cache_probe;
                            break;
                        }
                        tracing::warn!(
                            "端点 {} 返回瞬态错误 {}，回退到下一端点（凭据 #{} 不计失败，尝试 {}/{}）",
                            candidate.name(),
                            status,
                            ctx.id,
                            attempt + 1,
                            max_retries
                        );
                    }
                    Err(e) => {
                        // 连接层失败：记负缓存（下次自动跳过此 (端点, region)）。
                        // reqwest::Error 的 `.is_connect()` 仅含 TCP connect 失败，DNS 归 `.is_request()`，
                        // 故综合判断 request/connect/timeout（避免漏掉 DNS 不存在的场景）。
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
                        if idx == last_idx {
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
                            break;
                        }
                        tracing::warn!("端点 {} 发送失败，回退到下一端点: {}", candidate.name(), e);
                    }
                }
            }

            let Some(response) = response else {
                // 整条端点链都发送失败（网络层）：错误已在链内记录。
                if attempt + 1 < max_retries {
                    sleep(Self::retry_delay(attempt)).await;
                }
                continue;
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
                    // 供解码侧回报协议不符时定位「哪条路由坏了」（见 report_protocol_mismatch）。
                    endpoint_name: endpoint.name().to_string(),
                    upstream_region: upstream_region.to_string(),
                    prompt_cache_probe,
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
                        api_type,
                        status,
                        body
                    ));
                    // continue:下一轮 acquire_context 选别的号;全池不可用时由 max_retries/墙钟兜底透传。
                    continue;
                }

                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                // API Key 凭据无 refresh_token，跳过强刷避免无意义重试+24h冷却
                if endpoint.is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
                    force_refreshed.insert(ctx.id);
                    if ctx.credentials.is_api_key_credential() {
                        tracing::warn!(
                            "凭据 #{} (API Key) bearer token 被上游拒绝，跳过强刷（API Key 不支持刷新）",
                            ctx.id
                        );
                    } else {
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
                        // 刷新失败 = 认证态有问题，加一段冷却让调度避开它
                        self.token_manager.report_auth_cooldown(ctx.id);
                    }
                }

                last_outcome = crate::usage::RequestOutcome::AuthFailed;
                let has_available = self.token_manager.report_failure(ctx.id);
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

            // 503/429 模型容量问题（MODEL_TEMPORARILY_UNAVAILABLE / INSUFFICIENT_MODEL_CAPACITY）
            // — 全局容量不足，非凭据问题。
            //
            // 使用慢速退避（1s base）；不调用 report_failure / report_rate_limited，
            // 不影响凭据健康分（健康分反映凭据质量，与模型过载无关），不设凭据冷却
            // （所有凭据对同一过载模型完全等价，切换无意义）。
            //
            // 只允许 MAX_MODEL_UNAVAILABLE_RETRIES 次慢速重试，耗尽后直接 break 透传错误——
            // 继续切换凭据无意义。
            //
            // 已知信号：
            // - 503 + MODEL_TEMPORARILY_UNAVAILABLE (经典形式)
            // - 429 + INSUFFICIENT_MODEL_CAPACITY (生产实证 2026-08-01，claude-opus-5-thinking)
            if (status.as_u16() == 503 || status.as_u16() == 429)
                && endpoint.is_model_temporarily_unavailable(&body)
            {
                model_unavailable_attempts += 1;
                tracing::warn!(
                    "模型容量不足（容量信号，第 {}/{} 次）: {} {}",
                    model_unavailable_attempts,
                    MAX_MODEL_UNAVAILABLE_RETRIES + 1,
                    status,
                    body
                );
                last_outcome = crate::usage::RequestOutcome::ModelUnavailable;
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败（模型容量不足，建议稍后重试）: {} {}",
                    api_type,
                    status,
                    body
                ));
                if model_unavailable_attempts > MAX_MODEL_UNAVAILABLE_RETRIES {
                    // 已用完慢速重试预算，透传过载错误给客户端，让其自行退避。
                    break;
                }
                // 慢速退避：1s base，比通用 200ms 更长，避免反复冲击过载路径。
                sleep(Self::retry_delay_model_unavailable(
                    model_unavailable_attempts - 1,
                ))
                .await;
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
                // 瞬态 429 的确定性标记(附加到 last_error 串尾)。
                //
                // 为什么需要它:重试耗尽后 handler 的 map_provider_error 靠错误串分类。瞬态 429 的串
                // 形如 `流式 API 请求失败: 429 Too Many Requests {...}`,**不含**任何已知关键字,于是
                // 落入兜底 `502 api_error` —— 客户端(Claude Code)把限流当成网关故障,且 502 不带
                // Retry-After,退避策略完全走偏(生产实证 2026-08-04:240 次 429 全部回成 502)。
                //
                // 标记随**这一条具体错误**走:后续尝试若换成别的错误(如 400),last_error 被覆盖、
                // 标记自然消失,绝不会把非限流错误误报成限流。
                //
                // ⚠️ 命名刻意避开 `retry_after_secs=` 子串:那是「全池冷却」路径的标记,
                // map_provider_error 用 `contains("retry_after_secs=")` 判定它。若本标记含该子串会
                // 被那条分支抢先命中,回出「所有凭据都在冷却」的错误文案(实际是上游限流)。
                let mut rl_marker = String::new();
                // 429 限流：给该凭据设置短冷却，让调度优先换用其它凭据
                // （仍不禁用、不计永久失败，冷却到期自动恢复）
                if status.as_u16() == 429 {
                    last_outcome = crate::usage::RequestOutcome::RateLimited;
                    // 上游 429 → 入站整形 RPM 自动挡乘性降档(削平后续入站速率,别继续挤爆上游)。
                    self.token_manager.report_upstream_rate_limited();
                    // 优先用上游给出的精确重置时间：响应头 Retry-After 优先，其次错误 body
                    let retry_after =
                        retry_after_header.or_else(|| endpoint.extract_retry_after_secs(&body));
                    // 0 = 上游未给出精确重置时间(裸 429),由 handler 用默认退避提示。
                    rl_marker = format!(" upstream_429_retry={}", retry_after.unwrap_or(0));
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
                }
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}{}",
                    api_type,
                    status,
                    body,
                    rl_marker
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
                        let fallback_cache_probe = config
                            .prompt_cache_enabled
                            .then(|| {
                                self.prompt_cache.probe(
                                    &body,
                                    ctx.id,
                                    endpoint.name(),
                                    ctx.credentials.effective_upstream_region(&config),
                                    is_1m,
                                    Duration::from_secs(config.prompt_cache_ttl_seconds),
                                )
                            })
                            .flatten();
                        // 同上：content-type 由端点声明，避免重复头（见主路径注释）。
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
                                    endpoint_name: endpoint.name().to_string(),
                                    // 本分支自己的凭据/config 现算，绝不借用主循环的
                                    // upstream_region（那是另一个凭据的，会把隔离记到错的路由上）。
                                    upstream_region: ctx
                                        .credentials
                                        .effective_upstream_region(&config)
                                        .to_string(),
                                    prompt_cache_probe: fallback_cache_probe,
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
        if let Some(mid) =
            v.pointer_mut("/conversationState/currentMessage/userInputMessage/modelId")
        {
            *mid = serde_json::Value::String(new_model.to_string());
        }
        serde_json::to_string(&v).unwrap_or_else(|_| request_body.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_max_retries_covers_every_available_credential() {
        // total=10 available=10：预算至少 10，保证每个可用凭据都能被摸一次
        let r = compute_max_retries(10, 10);
        assert!(r >= 10, "10 个可用凭据应至少允许 10 次尝试，实际 {}", r);

        // 常规按 total*MAX_RETRIES_PER_CREDENTIAL 走
        assert_eq!(compute_max_retries(10, 10), 10 * MAX_RETRIES_PER_CREDENTIAL);
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

    /// 端点回退链：链首是主端点，其余按固定顺序补齐且无重复。
    ///
    /// 这层回退正是单凭据（`compute_max_retries(1,1) == 1`，凭据级零重试）下
    /// 唯一的 429 容错来源，故链长必须 > 1 才有意义。
    #[test]
    fn test_endpoint_fallback_order_is_usable_as_chain() {
        use crate::kiro::endpoint::ENDPOINT_FALLBACK_ORDER;

        assert!(
            ENDPOINT_FALLBACK_ORDER.len() > 1,
            "回退链需至少 2 个端点，否则单凭据下 429 仍无容错"
        );
        assert_eq!(
            ENDPOINT_FALLBACK_ORDER[0],
            crate::kiro::endpoint::ide::IDE_ENDPOINT_NAME,
            "链首必须是 ide，保持既有默认行为不变"
        );
        let mut uniq = ENDPOINT_FALLBACK_ORDER.to_vec();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(
            uniq.len(),
            ENDPOINT_FALLBACK_ORDER.len(),
            "回退链不得有重复端点（会白打同一 host 浪费预算）"
        );
    }

    #[test]
    fn test_compute_max_retries_respects_absolute_upper_bound() {
        // 巨量凭据：预算不超过 ABSOLUTE_MAX.max(available)
        let total = 1000usize;
        let available = 1000usize;
        let r = compute_max_retries(total, available);
        assert!(r <= ABSOLUTE_MAX_TOTAL_RETRIES.max(available));
        // available <= ABSOLUTE_MAX 时封顶到 ABSOLUTE_MAX
        assert_eq!(compute_max_retries(100, 50), ABSOLUTE_MAX_TOTAL_RETRIES);
    }

    #[test]
    fn test_compute_max_retries_available_exceeds_absolute_cap() {
        // 可用凭据数超过绝对上限时，仍以 available 为下限，不因硬上限漏掉可用号
        let available = ABSOLUTE_MAX_TOTAL_RETRIES + 20;
        let r = compute_max_retries(available, available);
        assert!(r >= available, "可用数超上限时预算仍应 >= available");
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
        let only_model =
            r#"{"conversationState":{"currentMessage":{"userInputMessage":{"modelId":"m"}}}}"#;
        let (model, session) = KiroProvider::extract_model_and_session(only_model);
        assert_eq!(model.as_deref(), Some("m"));
        assert_eq!(session, None);
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

    // ===== 协议不符自愈闭环（生产事故 2026-08-04 回归） =====

    /// 构造一个注册了全部端点的 provider，用于验证回退链/隔离逻辑。
    fn provider_with_all_endpoints(default_endpoint: &str) -> KiroProvider {
        use crate::kiro::endpoint::{
            AmazonQEndpoint, CliEndpoint, CodeWhispererEndpoint, IdeEndpoint,
        };
        let manager = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![],
                None,
                None,
                false,
            )
            .expect("构建 token manager"),
        );
        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert("ide".into(), Arc::new(IdeEndpoint::new()));
        endpoints.insert("cli".into(), Arc::new(CliEndpoint::new()));
        endpoints.insert("codewhisperer".into(), Arc::new(CodeWhispererEndpoint));
        endpoints.insert("amazonq".into(), Arc::new(AmazonQEndpoint));
        KiroProvider::with_proxy(manager, None, endpoints, default_endpoint.to_string())
    }

    fn cli_credential() -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.endpoint = Some("cli".to_string());
        c.kiro_api_key = Some("ksk_test".to_string());
        c.auth_method = Some("api_key".to_string());
        c
    }

    /// 隔离是**软**的且自动过期：TTL 未到判 true，人工干预不是必需的。
    #[test]
    fn test_protocol_broken_quarantine_is_recorded_and_scoped() {
        let p = provider_with_all_endpoints("ide");
        assert!(
            !p.is_route_protocol_broken("cli", "us-east-1"),
            "初始不应有任何隔离"
        );

        p.mark_route_protocol_broken("cli", "us-east-1");
        assert!(p.is_route_protocol_broken("cli", "us-east-1"));

        // 隔离按 (端点, region) 精确划界：不连坐别的 region，也不连坐别的端点。
        assert!(
            !p.is_route_protocol_broken("cli", "eu-central-1"),
            "隔离不得跨 region 连坐"
        );
        assert!(
            !p.is_route_protocol_broken("ide", "us-east-1"),
            "隔离不得跨端点连坐"
        );
    }

    /// 核心回归：cli 号在**未**证实协议不符时保持既有单元素链（行为逐字不变）；
    /// 一旦证实，自动获得回退链——这正是 #142 缺失的逃生通道。
    #[test]
    fn test_cli_gets_escape_hatch_only_after_protocol_mismatch() {
        let p = provider_with_all_endpoints("ide");
        let cred = cli_credential();

        // 未隔离：单元素链，与改动前完全一致（不引入任何新行为）。
        let chain = p.endpoint_chain_for(&cred, true, "us-east-1").unwrap();
        assert_eq!(chain.len(), 1, "未证实协议不符时 cli 仍是单元素链");
        assert_eq!(chain[0].name(), "cli");

        // 证实协议不符后：获得回退链，且 cli 不再占链首。
        p.mark_route_protocol_broken("cli", "us-east-1");
        let chain = p.endpoint_chain_for(&cred, true, "us-east-1").unwrap();
        assert!(
            chain.len() > 1,
            "协议不符后必须有逃生通道，否则每个请求都撞同一面墙（#142 的成因）"
        );
        assert_ne!(chain[0].name(), "cli", "已证实坏掉的端点绝不能继续占链首");
    }

    /// 铁律：回退链**绝不为空**。链为空 ⇒ 没有任何端点发送 ⇒ response 恒 None
    /// ⇒ 请求静默无人处理。即使所有端点都被隔离，也必须留一个兜底。
    #[test]
    fn test_chain_never_empty_even_when_all_routes_quarantined() {
        let p = provider_with_all_endpoints("ide");
        for name in ["ide", "cli", "codewhisperer", "amazonq"] {
            p.mark_route_protocol_broken(name, "us-east-1");
        }

        // cli 号（历史单元素路径）
        let chain = p
            .endpoint_chain_for(&cli_credential(), true, "us-east-1")
            .unwrap();
        assert!(!chain.is_empty(), "全隔离时 cli 链仍不得为空");

        // 默认 ide 号
        let chain = p
            .endpoint_chain_for(&KiroCredentials::default(), true, "us-east-1")
            .unwrap();
        assert!(!chain.is_empty(), "全隔离时 ide 链仍不得为空");
    }

    /// `endpoint_fallback = false` 时行为不变（部署方显式关掉回退的意图必须被尊重）。
    #[test]
    fn test_fallback_disabled_still_single_element_even_if_broken() {
        let p = provider_with_all_endpoints("ide");
        p.mark_route_protocol_broken("cli", "us-east-1");
        let chain = p
            .endpoint_chain_for(&cli_credential(), false, "us-east-1")
            .unwrap();
        assert_eq!(chain.len(), 1, "显式关闭回退时绝不擅自加端点");
    }

    /// 隔离期内，健康端点排在被隔离端点之前（调度优先走能用的那条）。
    #[test]
    fn test_broken_primary_is_demoted_not_removed() {
        let p = provider_with_all_endpoints("ide");
        p.mark_route_protocol_broken("ide", "us-east-1");
        let chain = p
            .endpoint_chain_for(&KiroCredentials::default(), true, "us-east-1")
            .unwrap();
        assert!(!chain.is_empty());
        assert_ne!(chain[0].name(), "ide", "被隔离的主端点不应仍占链首");
    }

    /// **端到端闭环**：这是整改的核心断言——单独验证每一层都不足以证明系统会自愈。
    ///
    /// 复刻生产事故的完整链路：
    ///   ① 上游回 AWS JSON 信封（用生产日志里的真实字节）
    ///   ② 解码器判定协议不符（而非啃出 19 亿字节后停止）
    ///   ③ handler 回报 provider
    ///   ④ 凭据被记一次**失败**（旧实现恒为 0，健康分只升不降 → 无限重选坏号）
    ///   ⑤ 该路由进隔离，后续请求自动改走别的端点
    #[test]
    fn test_end_to_end_protocol_mismatch_closes_the_loop() {
        use crate::kiro::parser::decoder::EventStreamDecoder;

        // ① + ② 解码层：生产真实字节 → 判定协议不符，且**不是**误导性的长度错误
        let production_body =
            br#"{"Output":{"__type":"com.amazon.coral.service#InternalServerException"},"Version":"1.0"}"#;
        let mut decoder = EventStreamDecoder::new();
        decoder.feed(production_body).unwrap();
        let mut saw_misleading_length = false;
        for r in decoder.decode_iter() {
            if let Err(e) = r {
                if e.to_string().contains("1953527156") || e.to_string().contains("2065846133") {
                    saw_misleading_length = true;
                }
            }
        }
        assert!(
            decoder.is_protocol_mismatch(),
            "解码层必须把 JSON 信封判为协议不符（这是闭环的起点）"
        );
        assert!(
            !saw_misleading_length,
            "绝不能再报出'消息长度 19 亿字节'——那会把根因彻底埋掉"
        );

        // ③ + ④ + ⑤ 回报层：凭据记失败 + 路由隔离
        let cred = cli_credential();
        let manager = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![cred.clone()],
                None,
                None,
                true,
            )
            .expect("构建 token manager"),
        );
        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert(
            "ide".into(),
            Arc::new(crate::kiro::endpoint::IdeEndpoint::new()),
        );
        endpoints.insert(
            "cli".into(),
            Arc::new(crate::kiro::endpoint::CliEndpoint::new()),
        );
        endpoints.insert(
            "codewhisperer".into(),
            Arc::new(crate::kiro::endpoint::CodeWhispererEndpoint),
        );
        let provider =
            KiroProvider::with_proxy(manager.clone(), None, endpoints, "ide".to_string());

        let id = manager.snapshot().entries[0].id;
        let before = manager.snapshot().entries[0].failure_count;

        let meta = CallMeta {
            credential_id: id,
            model: None,
            session_id: None,
            is_streaming: true,
            retries: 0,
            latency_ms: 1,
            endpoint_name: "cli".to_string(),
            upstream_region: "us-east-1".to_string(),
            prompt_cache_probe: None,
            inflight: crate::kiro::scheduling::InflightGuard::acquire(Default::default()),
        };
        provider.report_protocol_mismatch(&meta, "上游返回 AWS JSON 1.0 信封");

        let after = manager.snapshot().entries[0].failure_count;
        assert_eq!(
            after,
            before + 1,
            "协议不符必须记一次失败——旧实现恒为 0，健康分只升不降，\
             调度器于是无限重选这个必然截断的号（生产实证：845 次，failureCount 始终 0）"
        );
        assert!(
            provider.is_route_protocol_broken("cli", "us-east-1"),
            "该路由必须进隔离，否则下一个请求还会撞同一面墙"
        );
        // 隔离后 cli 凭据拿到逃生通道，不再是死路一条的单元素链。
        let chain = provider
            .endpoint_chain_for(&cred, true, "us-east-1")
            .unwrap();
        assert!(
            chain.len() > 1,
            "隔离后必须有逃生通道，否则每个请求都必然失败"
        );
        assert_ne!(chain[0].name(), "cli", "坏路由不应仍占链首");
    }

    // ===== client_cache 容量上限 =====
    //
    // 直接测 `evict_lru_if_full` 而不是 `client_for`：后者每次未命中都要真的
    // 建一个 reqwest::Client（连 TLS 后端一起初始化），造满 32 个只为验证淘汰
    // 逻辑，既慢又把「策略对不对」和「Client 能不能建起来」两件事绑在一起。

    /// 造一个只有时间戳有意义的假缓存条目。
    ///
    /// `Client::new()` 不发起任何连接，构造是纯本地的，所以当占位符用是安全的。
    fn stub_entry(age: Duration) -> (Client, Instant) {
        (Client::new(), Instant::now() - age)
    }

    fn proxy_key(n: usize) -> Option<ProxyConfig> {
        Some(ProxyConfig {
            url: format!("http://proxy-{n}.invalid:8080"),
            username: None,
            password: None,
        })
    }

    /// 未满时一个都不许动。
    ///
    /// 【为何单独测这一档】常规部署里代理种类个位数，永远碰不到上限——也就是说
    /// 这条路径覆盖的是**几乎 100% 的真实流量**。若淘汰逻辑在未满时也误伤，
    /// 表现是热点 Client 被反复重建、连接池每次从零开始，性能悄悄退化而无人报错。
    #[test]
    fn nothing_is_evicted_while_under_capacity() {
        let mut cache: HashMap<Option<ProxyConfig>, (Client, Instant)> = HashMap::new();
        for i in 0..5 {
            cache.insert(proxy_key(i), stub_entry(Duration::from_secs(i as u64)));
        }
        evict_lru_if_full(&mut cache, 32);
        assert_eq!(cache.len(), 5, "未满就不该淘汰任何条目");
    }

    /// 满了就腾出**恰好一个**位子，且被淘汰的是最久未用的那个。
    #[test]
    fn eviction_removes_exactly_the_coldest_entry() {
        let mut cache: HashMap<Option<ProxyConfig>, (Client, Instant)> = HashMap::new();
        // 0 号最冷（1 小时未用），其余都是最近几秒用过的。
        cache.insert(proxy_key(0), stub_entry(Duration::from_secs(3600)));
        for i in 1..4 {
            cache.insert(proxy_key(i), stub_entry(Duration::from_secs(i as u64)));
        }

        evict_lru_if_full(&mut cache, 4);

        assert_eq!(cache.len(), 3, "满了必须且只需腾出一个位子");
        assert!(
            !cache.contains_key(&proxy_key(0)),
            "被淘汰的必须是最久未用的那个（1 小时未用），实际它还在"
        );
        for i in 1..4 {
            assert!(
                cache.contains_key(&proxy_key(i)),
                "热条目 {i} 被误伤——LRU 选错了受害者"
            );
        }
    }

    /// **反向断言：淘汰的不是「最早插入」的那个。**
    ///
    /// 这条是整组里区分力最强的。全局代理往往是构造函数里第一个插进去的
    /// （见 `with_proxy` 的预热），同时也是最热的那个。若把 LRU 写成了 FIFO
    /// （比如按插入顺序、或忘了在命中时刷新 `last_used`），受害者就恰好是
    /// 那个每个请求都要用的 Client——表现是主链路的连接池被反复丢弃重建。
    #[test]
    fn eviction_is_lru_not_fifo() {
        let mut cache: HashMap<Option<ProxyConfig>, (Client, Instant)> = HashMap::new();
        // 模拟：全局代理(None)最早插入，但刚刚用过（最热）。
        cache.insert(None, stub_entry(Duration::from_millis(1)));
        // 后插入的几个反而更冷。
        cache.insert(proxy_key(1), stub_entry(Duration::from_secs(600)));
        cache.insert(proxy_key(2), stub_entry(Duration::from_secs(300)));

        evict_lru_if_full(&mut cache, 3);

        assert!(
            cache.contains_key(&None),
            "最早插入但最热的全局代理 Client 被淘汰了——这是 FIFO 而非 LRU。\
             它是主链路每个请求都要用的那个，反复重建等于连接池永远冷启动。"
        );
        assert!(
            !cache.contains_key(&proxy_key(1)),
            "真正最冷的（10 分钟未用）应当被淘汰"
        );
    }

    /// 容量上限必须给正常部署留足余量。
    ///
    /// 这条钉的是取值而非逻辑：若哪天有人把 CAP 调到个位数，常规多代理部署就会
    /// 开始持续抖动（每个请求淘汰上一个请求刚建的 Client），而所有逻辑用例仍全绿。
    #[test]
    fn capacity_is_generous_enough_to_never_bind_in_normal_deployments() {
        assert!(
            CLIENT_CACHE_CAP >= 16,
            "CLIENT_CACHE_CAP={CLIENT_CACHE_CAP} 太小：这是纯兜底上限，\
             常规部署（代理种类个位数）绝不该碰到它，否则会持续抖动"
        );
    }

    /// 空缓存上调用不许 panic。
    ///
    /// `min_by_key` 对空迭代器返回 `None`，写成 `.unwrap()` 就会在这里炸——
    /// 而 cap=0 这种配置错误本该是「不缓存」，不该让整个进程崩掉。
    #[test]
    fn eviction_on_empty_cache_is_a_noop() {
        let mut cache: HashMap<Option<ProxyConfig>, (Client, Instant)> = HashMap::new();
        evict_lru_if_full(&mut cache, 0);
        assert!(cache.is_empty());
    }
}
