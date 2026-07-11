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

use crate::http_client::{ProxyConfig, build_client};
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
        let initial_client = build_client(proxy.as_ref(), 720, tls_backend)
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
        let client = build_client(effective.as_ref(), 720, self.tls_backend)?;
        cache.insert(effective, client.clone());
        Ok(client)
    }

    /// 根据凭据选择 endpoint 实现
    fn endpoint_for(
        &self,
        credentials: &KiroCredentials,
    ) -> anyhow::Result<Arc<dyn KiroEndpoint>> {
        let name = credentials
            .endpoint
            .as_deref()
            .unwrap_or(&self.default_endpoint);
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
    ) -> anyhow::Result<(reqwest::Response, CallMeta)> {
        self.call_api_with_retry(request_body, false).await
    }

    /// 发送流式 API 请求
    pub async fn call_api_stream(
        &self,
        request_body: &str,
    ) -> anyhow::Result<(reqwest::Response, CallMeta)> {
        self.call_api_with_retry(request_body, true).await
    }

    /// 发送 MCP API 请求（WebSearch 等工具调用）
    pub async fn call_mcp(&self, request_body: &str) -> anyhow::Result<reqwest::Response> {
        self.call_mcp_with_retry(request_body).await
    }

    /// 累加一次请求的真实 credit 花费到该凭据的生命周期累计（透传到 token_manager）。
    ///
    /// handler 在请求完成、从上游 meteringEvent 拿到真实计费量后调用；provider 持有
    /// token_manager，handler 只有 provider，故在此开一个薄 passthrough。
    pub fn report_credits(&self, credential_id: u64, credits: f64) {
        self.token_manager.add_credits(credential_id, credits);
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
                if endpoint.is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
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
        let api_type = if is_stream { "流式" } else { "非流式" };

        // 一次解析同时取出模型信息与会话标识（conversationId），避免热路径上对
        // 整个请求体做两次全量 serde_json::from_str（大请求体尤其昂贵）。
        let (model, session_id) = Self::extract_model_and_session(request_body);

        // 用量埋点：记录进入调用的时刻与最后服务的凭据/失败分类
        let call_started = std::time::Instant::now();
        let mut last_credential_id: Option<u64> = None;
        let mut last_outcome = crate::usage::RequestOutcome::OtherError;

        for attempt in 0..max_retries {
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
            };

            let url = endpoint.api_url(&rctx);
            let body = endpoint.transform_api_body(request_body, &rctx);

            let base = self
                .client_for(&ctx.credentials)?
                .post(&url)
                .body(body)
                .header("content-type", "application/json");
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
                let has_available = self.token_manager.report_model_invalid(ctx.id);
                if !has_available {
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败（模型不可用且所有凭据已用尽）: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    break;
                }
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败（凭据 #{} INVALID_MODEL_ID，切换凭据）: {} {}",
                    api_type,
                    ctx.id,
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

                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                if endpoint.is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
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

        // 所有重试都失败：埋点一条失败记录后返回错误
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
        let only_model = r#"{"conversationState":{"currentMessage":{"userInputMessage":{"modelId":"m"}}}}"#;
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
}

