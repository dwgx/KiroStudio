//! Admin API 业务逻辑服务

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;

use super::error::AdminServiceError;
use super::external_idp_login::{
    ExternalIdpLeg1Result, ExternalIdpLeg2Result, ExternalIdpLoginManager, ExternalIdpSelectResult,
    ExternalIdpStartResult,
};
use super::idc_login::IdcLoginManager;
use super::social_login::SocialLoginManager;
pub use super::social_login::{PollResult, StartResult};
use super::idc_login::{IdcPollResult, IdcStartResult};
use crate::kiro::auth::social::OAuthCallbackData;
use super::types::{
    AddCredentialRequest, AddCredentialResponse, BalanceResponse, ConfigSnapshotResponse,
    CredentialStatusItem, CredentialsStatusResponse, LoadBalancingModeResponse,
    SetLoadBalancingModeRequest, StorageCleanupItem, StorageCleanupResponse, StoragePartition,
    StorageStatsResponse, TrashItemResponse, TrashListResponse, UpdateConfigRequest,
    UpdateConfigResponse,
};
use crate::usage::TraceDb;

/// 余额缓存【新鲜度】阈值（秒），5 分钟。
/// 仅用于 `get_balance` 的按需（hover）路径：决定是否需要重新向上游拉取。
/// 注意：这【不是】展示缓存的丢弃阈值——展示用 `BALANCE_CACHE_DISPLAY_MAX_AGE_SECS`。
const BALANCE_CACHE_TTL_SECS: i64 = 300;

/// 余额缓存【展示保留】上限（秒），7 天。
///
/// 关键修复（对齐 Foxfishc 的“重启后余额缓存不丢”目标，但契合我方单一数据源架构）：
/// 展示路径（启动加载 + 批量缓存端点）绝不能用 5 分钟的新鲜度阈值去丢弃条目，
/// 否则会出现两个症状：
///   1. 重启后磁盘缓存几乎必然 >5 分钟 → 被丢弃 → 前端显示“未知”；
///   2. 后台温和刷新间隔为 30 分钟，但展示缓存 5 分钟后即被过滤 →
///      每 30 分钟里有 25 分钟批量端点返回空 → 前端长期“未知”。
/// 因此展示缓存保留最近 7 天的最后已知值，并把 `cached_at` 交给前端判断新鲜度
/// （前端展示“截至 X 分钟前”而非直接抹掉数字）。超过 7 天才丢弃，避免无界陈旧。
const BALANCE_CACHE_DISPLAY_MAX_AGE_SECS: i64 = 7 * 24 * 3600;

/// 缓存的余额条目（含时间戳）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedBalance {
    /// 缓存时间（Unix 秒）
    cached_at: f64,
    /// 缓存的余额数据
    data: BalanceResponse,
}

/// 限流 insights 中单个凭据的冷却明细（只读快照）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CooldownDetail {
    /// 冷却原因（中文描述，如"速率限制"）
    pub reason: String,
    /// 剩余冷却时间（毫秒）
    pub remaining_ms: u64,
    /// 连续触发次数
    pub trigger_count: u32,
}

/// 限流 insights 单条（每号一条），零上游只读内存快照。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitInsight {
    /// 凭据 ID
    pub id: u64,
    /// 最近 60 秒滚动窗口内的选号次数（RPM）
    pub rpm: u32,
    /// 每凭据 RPM 软上限（0 = 不限制）
    pub rpm_limit: u32,
    /// 是否已达软上限（rpm_limit>0 且 rpm>=rpm_limit）
    pub rpm_saturated: bool,
    /// 当前在途请求数
    pub inflight: u32,
    /// 是否已禁用（禁用号不参与调度，UI 应显示"已禁用"而非"畅通"）
    pub disabled: bool,
    /// 冷却明细；未冷却时为 null
    pub cooldown: Option<CooldownDetail>,
    /// 近期 429 次数（取自速率限制冷却的连续触发计数，零上游）
    pub recent429: u32,
    /// 中文推断文案（如"#54 冷却中（速率限制）剩22s，已触发3次""畅通"）
    pub insight_text: String,
    /// 真实熔断/健康快照(circuit Open/HalfOpen + EWMA 健康分 + 试探概率 + 熔断剩余秒)。
    /// 后端 HealthTracker 现成算好,此前无出口——现暴露给运维观测。无健康记录(从未被选过)时为 null,
    /// 前端按缺省=Closed 满血处理。族级(M365 同租户共享),故同族多号快照一致(连坐语义)。
    pub health: Option<crate::kiro::health::HealthSnapshot>,
}

/// SSE 实时流中单个凭据的轻量快照
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveCred {
    /// 凭据 ID
    pub id: u64,
    /// 最近 60 秒 RPM
    pub rpm: u32,
    /// 当前在途请求数
    pub inflight: u32,
    /// 是否正在冷却
    pub cooling_down: bool,
    /// 冷却剩余毫秒；未冷却时为 null
    pub cooldown_remaining_ms: Option<u64>,
    /// 熔断器是否 Open(真实熔断态,非启发式)。无健康记录时为 false(缺省满血)。
    pub circuit_open: bool,
    /// 健康分 [0,1](EWMA 成功率 × 429 惩罚)。无健康记录时为 1.0(缺省满血)。
    pub health_score: f64,
}

/// 根据 rpm / 冷却状态推断中文限流文案（纯本地计算，零上游）。
fn build_insight_text(
    id: u64,
    rpm: u32,
    rpm_limit: u32,
    saturated: bool,
    disabled: bool,
    cooldown: Option<&crate::kiro::cooldown::CooldownInfo>,
) -> String {
    use crate::kiro::cooldown::CooldownReason;

    if disabled {
        return format!("#{id} 已禁用（不参与调度）");
    }

    if let Some(c) = cooldown {
        // 向上取整到秒，避免展示"剩 0s"却仍在冷却
        let secs = c.remaining_ms.div_ceil(1000);
        if c.reason == CooldownReason::RateLimitExceeded {
            return format!(
                "#{id} 冷却中（速率限制）剩{secs}s，已触发{}次",
                c.trigger_count
            );
        }
        return format!("#{id} 冷却中（{}）剩{secs}s", c.reason.description());
    }

    if saturated {
        return format!("#{id} 近60s {rpm}/{rpm_limit} 已达软上限，建议分流");
    }
    // 接近软上限（>=80%）也提示，便于提前分流
    if rpm_limit > 0 && (rpm as u64) * 5 >= (rpm_limit as u64) * 4 {
        return format!("#{id} 近60s {rpm}/{rpm_limit} 接近软上限，建议分流");
    }
    "畅通".to_string()
}

/// Admin 服务
///
/// 封装所有 Admin API 的业务逻辑
pub struct AdminService {
    token_manager: Arc<MultiTokenManager>,
    balance_cache: Mutex<HashMap<u64, CachedBalance>>,
    cache_path: Option<PathBuf>,
    /// 已注册的端点名称集合（用于 add_credential 校验）
    known_endpoints: HashSet<String>,
    /// 网页上号会话管理器
    social_login: SocialLoginManager,
    /// IDC 上号会话管理器
    idc_login: IdcLoginManager,
    /// 外部 IdP（Microsoft）上号会话管理器
    external_idp_login: ExternalIdpLoginManager,
    /// 后台温和余额刷新任务句柄（TIER2 热重载：改 balanceRefreshIntervalSecs 后 abort+respawn
    /// 即时生效不重启）。None = 当前未运行（间隔=0 或尚未启动）。
    balance_task: Mutex<Option<JoinHandle<()>>>,
}

impl AdminService {
    pub fn new(
        token_manager: Arc<MultiTokenManager>,
        known_endpoints: impl IntoIterator<Item = String>,
    ) -> Self {
        let cache_path = token_manager
            .cache_dir()
            .map(|d| d.join("kiro_balance_cache.json"));

        let balance_cache = Self::load_balance_cache_from(&cache_path);

        Self {
            social_login: SocialLoginManager::new(token_manager.clone()),
            idc_login: IdcLoginManager::new(token_manager.clone()),
            external_idp_login: ExternalIdpLoginManager::new(token_manager.clone()),
            token_manager,
            balance_cache: Mutex::new(balance_cache),
            cache_path,
            known_endpoints: known_endpoints.into_iter().collect(),
            balance_task: Mutex::new(None),
        }
    }

    /// 发起网页上号，返回 portal_url + session_id
    pub fn start_social_login(
        &self,
        priority: u32,
        proxy_url: Option<String>,
    ) -> Result<StartResult, AdminServiceError> {
        self.social_login
            .start(priority, proxy_url)
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))
    }

    /// 轮询网页上号会话状态
    pub async fn poll_social_login(&self, session_id: &str) -> PollResult {
        self.social_login.poll(session_id).await
    }

    /// 远程模式：公网回调路由投递 OAuth 回调
    pub fn deliver_social_callback(&self, data: OAuthCallbackData) -> bool {
        self.social_login.deliver_callback(data)
    }

    /// 发起 IDC (AWS SSO) 上号
    pub async fn start_idc_login(
        &self,
        start_url: &str,
        region: &str,
        priority: u32,
        proxy_url: Option<String>,
    ) -> Result<IdcStartResult, AdminServiceError> {
        self.idc_login
            .start(start_url, region, priority, proxy_url)
            .await
            .map_err(|e| {
                // 结构化诊断优先(全 region 失败→REGION_MISMATCH),否则退回内部错误。
                if let Some(de) = e.downcast_ref::<crate::kiro::token_manager::DiagnosedError>() {
                    AdminServiceError::Diagnosed(de.diagnosis.clone())
                } else {
                    AdminServiceError::InternalError(e.to_string())
                }
            })
    }

    /// 轮询 IDC 上号会话
    pub async fn poll_idc_login(&self, session_id: &str) -> IdcPollResult {
        self.idc_login.poll(session_id).await
    }

    /// 外部 IdP（Microsoft）上号 · 第 1 步：生成 signin URL。
    pub fn start_external_idp_login(
        &self,
        priority: u32,
        proxy_url: Option<String>,
        preferred_region: Option<String>,
    ) -> Result<ExternalIdpStartResult, AdminServiceError> {
        self.external_idp_login
            .start(priority, proxy_url, preferred_region)
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))
    }

    /// 外部 IdP 上号 · 第 2 步：粘回 portal 回调 URL，返回 IdP authorize URL。
    pub async fn submit_external_idp_leg1(
        &self,
        session_id: &str,
        pasted_url: &str,
    ) -> Result<ExternalIdpLeg1Result, AdminServiceError> {
        self.external_idp_login
            .submit_leg1(session_id, pasted_url)
            .await
            .map_err(|e| AdminServiceError::InvalidCredential(e.to_string()))
    }

    /// 外部 IdP 上号 · 第 3 步：粘回授权回调 URL，换 token + 探测多 region profile。
    /// 返回 profile 列表(多个则前端弹窗选,恰 1 个后端已自动建号)。
    pub async fn submit_external_idp_leg2(
        &self,
        session_id: &str,
        pasted_url: &str,
    ) -> Result<ExternalIdpLeg2Result, AdminServiceError> {
        self.external_idp_login
            .submit_leg2(session_id, pasted_url)
            .await
            .map_err(|e| AdminServiceError::InvalidCredential(e.to_string()))
    }

    /// 外部 IdP 上号 · 第 3 步选定:多 region profile 里选一个 arn,用暂存 token 建号入池。
    pub async fn submit_external_idp_leg2_select(
        &self,
        session_id: &str,
        arn: &str,
    ) -> Result<ExternalIdpSelectResult, AdminServiceError> {
        self.external_idp_login
            .submit_leg2_select(session_id, arn)
            .await
            .map_err(|e| AdminServiceError::InvalidCredential(e.to_string()))
    }

    /// 获取所有凭据状态
    pub fn get_all_credentials(&self) -> CredentialsStatusResponse {
        let snapshot = self.token_manager.snapshot();
        let default_endpoint = self.token_manager.config().default_endpoint.clone();

        // 当前冷却快照（429/限流感官）：按凭据 id 建表,合并进每张卡的状态。
        let cooldowns: std::collections::HashMap<u64, crate::kiro::cooldown::CooldownInfo> = self
            .token_manager
            .cooldown_snapshot()
            .into_iter()
            .map(|c| (c.credential_id, c))
            .collect();

        let mut credentials: Vec<CredentialStatusItem> = snapshot
            .entries
            .into_iter()
            .map(|entry| {
                let cd = cooldowns.get(&entry.id);
                CredentialStatusItem {
                id: entry.id,
                priority: entry.priority,
                rpm_limit: entry.rpm_limit,
                allowed_models: entry.allowed_models,
                tested_models: entry.tested_models,
                disabled: entry.disabled,
                failure_count: entry.failure_count,
                is_current: entry.id == snapshot.current_id,
                expires_at: entry.expires_at,
                auth_method: entry.auth_method,
                base_url: entry.base_url,
                request_limit: entry.request_limit,
                request_count: entry.request_count,
                has_profile_arn: entry.has_profile_arn,
                refresh_token_hash: entry.refresh_token_hash,
                api_key_hash: entry.api_key_hash,
                masked_api_key: entry.masked_api_key,
                email: entry.email,
                subscription_title: entry.subscription_title,
                success_count: entry.success_count,
                total_credits_used: entry.total_credits_used,
                last_used_at: entry.last_used_at.clone(),
                has_proxy: entry.has_proxy,
                proxy_url: entry.proxy_url,
                refresh_failure_count: entry.refresh_failure_count,
                disabled_reason: entry.disabled_reason,
                endpoint: entry.endpoint.unwrap_or_else(|| default_endpoint.clone()),
                inflight: entry.inflight,
                rpm: entry.rpm,
                name: entry.name,
                cooling_down: cd.is_some(),
                cooldown_remaining_ms: cd.map(|c| c.remaining_ms),
                cooldown_reason: cd.map(|c| c.reason.description().to_string()),
                }
            })
            .collect();

        // 按优先级排序（数字越小优先级越高）
        credentials.sort_by_key(|c| c.priority);

        CredentialsStatusResponse {
            total: snapshot.total,
            available: snapshot.available,
            current_id: snapshot.current_id,
            credentials,
        }
    }

    /// 限流 insights（BE-A2）：每号一条只读快照，供前端限流健康抽屉展示。
    ///
    /// 数据全部取自内存：token_manager 快照（rpm/inflight）、cooldown 快照（冷却明细/
    /// 连续触发次数），以及 config 的每凭据 RPM 软上限。**零上游调用**（封号红线）。
    /// 列表按 rpm 降序、id 升序，方便前端把最热的号排在前面。
    pub fn ratelimit_insights(&self) -> Vec<RateLimitInsight> {
        let snapshot = self.token_manager.snapshot();

        // 冷却快照：按 id 建表，合并进每条 insight
        let cooldowns: std::collections::HashMap<u64, crate::kiro::cooldown::CooldownInfo> = self
            .token_manager
            .cooldown_snapshot()
            .into_iter()
            .map(|c| (c.credential_id, c))
            .collect();

        // 熔断/健康快照:按 id 建表(后端现成算好,此前无出口)。无记录的号不在表中=缺省满血。
        let mut healths = self.token_manager.health_snapshots();

        let mut out: Vec<RateLimitInsight> = snapshot
            .entries
            .into_iter()
            .map(|e| {
                let cd = cooldowns.get(&e.id);
                // 有效饱和阈值:复用调度真相源 effective_saturation_limit——per-cred(>0)>全局(>0)>兜底 30,
                // **再应用 L3 headroom 折扣**(默认 factor=85 → 兜底 30 打折为 25)。此前 UI 侧只按 base 重算
                // (不含 headroom),会出现"调度已在 rpm≥25 硬门拦下并释放亲和、UI 仍显示畅通/无火焰"的观测
                // 口径漂移(误导加号决策)。改走同一真相源,饱和判定与调度完全对齐。
                let eff_limit = self.token_manager.effective_saturation_limit(e.rpm_limit);
                let saturated = e.rpm >= eff_limit;
                // recent429：速率限制类冷却的连续触发计数近似"近期 429 次数"（零上游）；
                // 非速率限制冷却或无冷却则为 0。
                let recent429 = cd
                    .filter(|c| {
                        c.reason == crate::kiro::cooldown::CooldownReason::RateLimitExceeded
                    })
                    .map(|c| c.trigger_count)
                    .unwrap_or(0);
                let insight_text =
                    build_insight_text(e.id, e.rpm, eff_limit, saturated, e.disabled, cd);
                RateLimitInsight {
                    id: e.id,
                    rpm: e.rpm,
                    rpm_limit: eff_limit,
                    rpm_saturated: saturated && !e.disabled,
                    inflight: e.inflight,
                    disabled: e.disabled,
                    cooldown: cd.map(|c| CooldownDetail {
                        reason: c.reason.description().to_string(),
                        remaining_ms: c.remaining_ms,
                        trigger_count: c.trigger_count,
                    }),
                    recent429,
                    insight_text,
                    health: healths.remove(&e.id),
                }
            })
            .collect();

        out.sort_by(|a, b| b.rpm.cmp(&a.rpm).then(a.id.cmp(&b.id)));
        out
    }

    /// SSE 实时流的一帧轻量快照（BE-A2）：全局 inflight/rpm + 每号精简状态。
    ///
    /// 只读内存零上游。吞吐部分由 SSE handler 侧从 usage_stats 补充（此处只出凭据维度）。
    pub fn live_creds(&self) -> (u32, u32, Vec<LiveCred>) {
        let snapshot = self.token_manager.snapshot();
        let cooldowns: std::collections::HashMap<u64, crate::kiro::cooldown::CooldownInfo> = self
            .token_manager
            .cooldown_snapshot()
            .into_iter()
            .map(|c| (c.credential_id, c))
            .collect();

        let healths = self.token_manager.health_snapshots();

        let mut global_inflight: u32 = 0;
        let mut global_rpm: u32 = 0;
        let creds: Vec<LiveCred> = snapshot
            .entries
            .into_iter()
            .map(|e| {
                global_inflight = global_inflight.saturating_add(e.inflight);
                global_rpm = global_rpm.saturating_add(e.rpm);
                let cd = cooldowns.get(&e.id);
                let h = healths.get(&e.id);
                LiveCred {
                    id: e.id,
                    rpm: e.rpm,
                    inflight: e.inflight,
                    cooling_down: cd.is_some(),
                    cooldown_remaining_ms: cd.map(|c| c.remaining_ms),
                    // 无健康记录=缺省满血(Closed, health=1.0)。
                    circuit_open: h.map(|s| s.circuit_open).unwrap_or(false),
                    health_score: h.map(|s| s.health).unwrap_or(1.0),
                }
            })
            .collect();

        (global_inflight, global_rpm, creds)
    }

    /// 设置凭据禁用状态
    pub fn set_disabled(&self, id: u64, disabled: bool) -> Result<(), AdminServiceError> {
        // 先获取当前凭据 ID，用于判断是否需要切换
        let snapshot = self.token_manager.snapshot();
        let current_id = snapshot.current_id;

        self.token_manager
            .set_disabled(id, disabled)
            .map_err(|e| self.classify_error(e, id))?;

        // 只有禁用的是当前凭据时才尝试切换到下一个
        if disabled && id == current_id {
            let _ = self.token_manager.switch_to_next();
        }
        Ok(())
    }

    /// 设置凭据优先级
    pub fn set_priority(&self, id: u64, priority: u32) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_priority(id, priority)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 设置凭据级 RPM 容量上限（0/None=继承全局）
    pub fn set_rpm_limit(&self, id: u64, rpm_limit: Option<u32>) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_rpm_limit(id, rpm_limit)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 设置凭据级「允许模型」白名单（成本安全硬门；空=不限制）。
    pub fn set_allowed_models(
        &self,
        id: u64,
        models: Option<Vec<String>>,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_allowed_models(id, models)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 设置凭据自定义别名/备注（传 None 或空清除）
    pub fn set_credential_name(
        &self,
        id: u64,
        name: Option<String>,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_credential_name(id, name)
            .map_err(|e| self.classify_error(e, id))
    }

    pub fn set_credential_proxy(
        &self,
        id: u64,
        proxy_url: Option<String>,
        proxy_username: Option<String>,
        proxy_password: Option<String>,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_credential_proxy(id, proxy_url, proxy_username, proxy_password)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 修改自定义 API(代挂透传)凭据的 base_url / api_key / 请求上限(仅 custom_api 号,后端 gate)。
    pub async fn set_custom_api_config(
        &self,
        id: u64,
        base_url: Option<String>,
        api_key: Option<String>,
        request_limit: Option<u64>,
        reset_count: bool,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_custom_api_config(id, base_url, api_key, request_limit, reset_count)
            .await
            .map_err(|e| self.classify_error(e, id))
    }

    /// 批量清空回收站（ids 为空清空全部）。返回成功清除数。
    pub fn purge_trash_batch(&self, ids: Option<Vec<u64>>) -> usize {
        self.token_manager.purge_trash_batch(ids)
    }

    /// 重置失败计数并重新启用
    pub fn reset_and_enable(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .reset_and_enable(id)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 读取单号 overage 状态（实时查询上游 Web Portal，只读）
    pub async fn overage_status(
        &self,
        id: u64,
    ) -> Result<crate::kiro::overage::OverageStatus, AdminServiceError> {
        crate::kiro::overage::overage_status(&self.token_manager, id)
            .await
            .map_err(|e| self.classify_error(e, id))
    }

    /// 开启单号 overage（⚠️ 触发真实按量付费）。幂等。
    pub async fn enable_overage(
        &self,
        id: u64,
    ) -> Result<crate::kiro::overage::OverageStatus, AdminServiceError> {
        crate::kiro::overage::set_overage(&self.token_manager, id, true)
            .await
            .map_err(|e| self.classify_error(e, id))
    }

    /// 关闭单号 overage。幂等。
    pub async fn disable_overage(
        &self,
        id: u64,
    ) -> Result<crate::kiro::overage::OverageStatus, AdminServiceError> {
        crate::kiro::overage::set_overage(&self.token_manager, id, false)
            .await
            .map_err(|e| self.classify_error(e, id))
    }

    /// 获取凭据余额（带缓存）
    pub async fn get_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        // 先查缓存
        {
            let cache = self.balance_cache.lock();
            if let Some(cached) = cache.get(&id) {
                let now = Utc::now().timestamp() as f64;
                if (now - cached.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    tracing::debug!("凭据 #{} 余额命中缓存", id);
                    return Ok(cached.data.clone());
                }
            }
        }

        // 缓存未命中或已过期，从上游获取
        let balance = self.fetch_balance(id).await?;

        // 更新缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.insert(
                id,
                CachedBalance {
                    cached_at: Utc::now().timestamp() as f64,
                    data: balance.clone(),
                },
            );
        }
        self.save_balance_cache();

        Ok(balance)
    }

    /// 从上游获取余额（无缓存）
    async fn fetch_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        let usage = self
            .token_manager
            .get_usage_limits_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        // overage（超额）感知：开了 Online Overage 的号 base 耗尽后仍有额度，
        // 用 effective 变体（base + overage cap）计算 remaining/百分比，避免展示失真。
        let overage_enabled = usage.overage_enabled();
        let overage_cap = usage.overage_cap_for(overage_enabled);
        let current_usage = usage.current_usage();
        let usage_limit = usage.usage_limit();
        let effective_limit = usage.effective_usage_limit_for(overage_enabled);
        let remaining = usage.effective_remaining_for(overage_enabled);
        let usage_percentage = if effective_limit > 0.0 {
            (current_usage / effective_limit * 100.0).min(100.0)
        } else {
            0.0
        };

        Ok(BalanceResponse {
            id,
            subscription_title: usage.subscription_title().map(|s| s.to_string()),
            current_usage,
            usage_limit,
            remaining,
            usage_percentage,
            next_reset_at: usage.next_date_reset,
            overage_enabled,
            overage_cap,
            effective_limit,
        })
    }

    /// 批量读取【已缓存】的凭据余额快照（A10）
    ///
    /// 为降低账号被上游限流的风险：只读 balance_cache，绝不触发任何上游 getUsageLimits 调用。
    ///
    /// 修复：返回最近 7 天内的最后已知值（不再用 5 分钟新鲜度阈值过滤）。
    /// 后台温和刷新间隔为 30 分钟，若这里仍按 5 分钟丢弃，前端每 30 分钟只有 5 分钟
    /// 能看到数字。改为按【展示保留上限】过滤，并把 `cached_at` 交给前端标注新鲜度
    /// （“截至 X 分钟前”），让余额/订阅等级“慢慢自动更新”且重启不丢。
    /// 仅陈旧超过 7 天的条目才不返回（前端可按需单独 hover 拉取）。
    pub fn get_cached_balances(&self) -> super::types::CachedBalancesResponse {
        use super::types::{CachedBalanceItem, CachedBalancesResponse};

        let now = Utc::now().timestamp() as f64;
        let cache = self.balance_cache.lock();
        let balances: HashMap<u64, CachedBalanceItem> = cache
            .iter()
            .filter(|(_, c)| (now - c.cached_at) < BALANCE_CACHE_DISPLAY_MAX_AGE_SECS as f64)
            .map(|(id, c)| {
                (
                    *id,
                    CachedBalanceItem {
                        balance: c.data.clone(),
                        cached_at: c.cached_at,
                    },
                )
            })
            .collect();

        CachedBalancesResponse {
            total: balances.len(),
            balances,
        }
    }

    /// 温和地周期性刷新所有【未禁用】凭据的余额缓存（A6）
    ///
    /// 为降低账号被上游限流的风险：
    /// - 逐个刷新，每个之间 sleep `spacing_secs` 秒，绝不并发一次性打所有号。
    /// - 只刷未禁用的号。
    /// - 仅更新缓存供展示，绝不因 remaining 低就自动禁用凭据（不做主动禁用）。
    ///
    /// 由 main.rs 的后台任务按长间隔调用（默认 30 分钟）。
    pub async fn refresh_all_balances_gently(&self, spacing_secs: u64) {
        // 取未禁用凭据 id 快照（只读，不持锁跨 await）
        let ids: Vec<u64> = self
            .token_manager
            .snapshot()
            .entries
            .into_iter()
            .filter(|e| !e.disabled)
            .map(|e| e.id)
            .collect();

        if ids.is_empty() {
            return;
        }

        tracing::info!("后台温和余额刷新开始：{} 个未禁用凭据", ids.len());
        let spacing = std::time::Duration::from_secs(spacing_secs.max(1));

        for (idx, id) in ids.iter().enumerate() {
            // 分散节奏：从第二个开始，每个之间先 sleep，避免一瞬间并发打多个号
            if idx > 0 {
                tokio::time::sleep(spacing).await;
            }

            match self.fetch_balance(*id).await {
                Ok(balance) => {
                    {
                        let mut cache = self.balance_cache.lock();
                        cache.insert(
                            *id,
                            CachedBalance {
                                cached_at: Utc::now().timestamp() as f64,
                                data: balance,
                            },
                        );
                    }
                    self.save_balance_cache();
                    tracing::debug!("后台温和余额刷新：凭据 #{} 已更新缓存", id);
                }
                Err(e) => {
                    // 单个失败不影响整体节奏；仅更新缓存展示，不做任何禁用动作
                    tracing::warn!("后台温和余额刷新：凭据 #{} 刷新失败（忽略）: {}", id, e);
                }
            }
        }

        // 回推余额快照给调度器(余额加权分流用)。用当前缓存的 remaining/effective_limit + 该号此刻的
        // total_credits_used 作本地累加修正基线。号被删/禁用则不含在表里(调度侧缺表=中性因子1.0)。
        self.push_balance_snapshots_to_scheduler();

        tracing::info!("后台温和余额刷新完成");
    }

    /// 把当前余额缓存 + 各号 total_credits_used 基线打包成 BalanceSnapshot 表,回推给调度器。
    /// 供余额加权分流:remaining/effective_limit 归一成剩余比例,credits_used 作累加修正基线。
    fn push_balance_snapshots_to_scheduler(&self) {
        use crate::kiro::token_manager::BalanceSnapshot;
        // 各号当前累计花费(本地实时,作累加修正基线)。
        let used_by_id: std::collections::HashMap<u64, f64> = self
            .token_manager
            .snapshot()
            .entries
            .into_iter()
            .map(|e| (e.id, e.total_credits_used))
            .collect();
        let snaps: std::collections::HashMap<u64, BalanceSnapshot> = {
            let cache = self.balance_cache.lock();
            cache
                .iter()
                .filter_map(|(id, cb)| {
                    let eff = if cb.data.effective_limit > 0.0 {
                        cb.data.effective_limit
                    } else {
                        // 旧缓存可能无 effective_limit,回退 usage_limit(base)。<=0 则跳过(调度侧缺表=中性)。
                        cb.data.usage_limit
                    };
                    if eff <= 0.0 {
                        return None;
                    }
                    Some((
                        *id,
                        BalanceSnapshot {
                            remaining_at_cache: cb.data.remaining,
                            effective_limit: eff,
                            credits_used_at_cache: used_by_id.get(id).copied().unwrap_or(0.0),
                        },
                    ))
                })
                .collect()
        };
        self.token_manager.set_balance_snapshots(snaps);
    }

    /// 重挂后台温和余额刷新任务（TIER2 热重载）。
    ///
    /// 读当前 config 的 `balance_refresh_interval_secs`，abort 旧任务后按需 spawn 新任务：
    /// - 启动时调用一次（替代 main.rs 原内联 detached spawn，让任务"从启动即受管"）；
    /// - admin 改 `balanceRefreshIntervalSecs` 后调用 → 间隔即时生效，无需重启；
    /// - 间隔=0 表示禁用，仅 abort 不重建。
    ///
    /// 任务体持 `Weak<Self>`：AdminService 被 drop 后下一轮 upgrade 失败即自我退出，
    /// 不构成 Arc 引用环（句柄存在 self 内，闭包只借弱引用）。
    /// 幂等：重复调用先 abort 旧句柄再重建，不会累积多个循环。
    /// 保留原有防风控节奏：首轮等满一个完整间隔才开始，逐个刷新每个间隔 4 秒。
    pub fn respawn_balance_task(self: &Arc<Self>) {
        let interval = self.token_manager.config().balance_refresh_interval_secs;
        let mut slot = self.balance_task.lock();
        // 先杀旧任务（若有），无论间隔如何都先停，避免旧间隔残留
        if let Some(old) = slot.take() {
            old.abort();
        }
        if interval == 0 {
            tracing::info!("后台温和余额刷新未启用（balance_refresh_interval_secs=0）");
            return;
        }
        let weak = Arc::downgrade(self);
        let handle = tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval(std::time::Duration::from_secs(interval));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // 跳过第一次立即触发的 tick，避免启动/重挂即批量拉（降低上游限流风险）
            ticker.tick().await;
            loop {
                ticker.tick().await;
                // service 已被 drop（进程停机路径）→ 退出循环
                let Some(svc) = weak.upgrade() else {
                    tracing::debug!("AdminService 已释放，余额刷新任务退出");
                    break;
                };
                // 每个号之间 sleep 4 秒，分散节奏
                svc.refresh_all_balances_gently(4).await;
            }
        });
        *slot = Some(handle);
        tracing::info!(
            "后台温和余额刷新已启用：间隔 {} 秒（逐个刷新，每个间隔 4 秒，不做主动禁用）",
            interval
        );
    }

    /// 添加新凭据
    pub async fn add_credential(
        &self,
        req: AddCredentialRequest,
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        // 校验端点名：未指定则默认合法，指定则必须已注册
        if let Some(ref name) = req.endpoint {
            if !self.known_endpoints.contains(name) {
                let mut known: Vec<&str> =
                    self.known_endpoints.iter().map(|s| s.as_str()).collect();
                known.sort();
                return Err(AdminServiceError::InvalidCredential(format!(
                    "未知端点 \"{}\"，已注册端点: {:?}",
                    name, known
                )));
            }
        }

        // 代理输入规整：URL 里可能内嵌账密（socks5://user:pass@host:port）——拆出干净 URL 与账密，
        // 独立账密字段优先，缺省时回退 URL 内嵌值。避免密码明文留 URL + 保证 SOCKS5 能认证。
        let (proxy_url, proxy_username, proxy_password) = match req
            .proxy_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(raw) => {
                let (clean, inline_user, inline_pass) =
                    crate::http_client::split_proxy_credentials(raw);
                (
                    Some(clean),
                    req.proxy_username.clone().filter(|s| !s.is_empty()).or(inline_user),
                    req.proxy_password.clone().filter(|s| !s.is_empty()).or(inline_pass),
                )
            }
            None => (None, None, None),
        };

        // 构建凭据对象
        let email = req.email.clone();
        let new_cred = KiroCredentials {
            id: None,
            access_token: req.access_token,
            refresh_token: req.refresh_token,
            profile_arn: req.profile_arn,
            expires_at: req.expires_at,
            auth_method: Some(req.auth_method),
            client_id: req.client_id,
            client_secret: req.client_secret,
            token_endpoint: req.token_endpoint,
            issuer_url: req.issuer_url,
            scopes: req.scopes,
            priority: req.priority,
            rpm_limit: req.rpm_limit,
            // 新增号默认不设白名单（不限制）；上号后经 /credentials/{id}/allowed-models 单独设置。
            allowed_models: None,
            tested_models: None,
            // 自定义 API 代挂透传字段（auth_method=custom_api 时由前端填入）。
            base_url: req.base_url,
            api_key: req.api_key,
            request_limit: req.request_limit,
            region: req.region,
            auth_region: req.auth_region,
            api_region: req.api_region,
            machine_id: req.machine_id,
            email: req.email,
            name: req.name,
            subscription_title: None, // 将在首次获取使用额度时自动更新
            proxy_url,
            proxy_username,
            proxy_password,
            disabled: false, // 新添加的凭据默认启用
            kiro_api_key: req.kiro_api_key,
            endpoint: req.endpoint,
        };

        // 调用 token_manager 添加凭据
        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| self.classify_add_error(e))?;

        // 主动获取订阅等级，避免首次请求时 Free 账号绕过 Opus 模型过滤
        if let Err(e) = self.token_manager.get_usage_limits_for(credential_id).await {
            tracing::warn!("添加凭据后获取订阅等级失败（不影响凭据添加）: {}", e);
        }

        // 新号自动初始化(异步,不阻塞响应):刷 token + 解析 profileArn，根治上号初期查余额 403(#89)。
        // 门控在 spawn_initial_refresh 内部(custom_api/api_key 自动跳过)。
        self.token_manager.spawn_initial_refresh(credential_id);

        Ok(AddCredentialResponse {
            success: true,
            message: format!("凭据添加成功，ID: {}", credential_id),
            credential_id,
            email,
        })
    }

    /// 删除凭据
    pub fn delete_credential(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .delete_credential(id)
            .map_err(|e| self.classify_delete_error(e, id))?;

        // 清理已删除凭据的余额缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(&id);
        }
        self.save_balance_cache();

        Ok(())
    }

    /// 列出回收站中的已删除凭据
    pub fn list_trash(&self) -> TrashListResponse {
        let mut items: Vec<TrashItemResponse> = self
            .token_manager
            .list_trash()
            .into_iter()
            .map(|t| TrashItemResponse {
                id: t.id,
                priority: t.priority,
                auth_method: t.auth_method,
                email: t.email,
                masked_api_key: t.masked_api_key,
                refresh_token_hash: t.refresh_token_hash,
                api_key_hash: t.api_key_hash,
                endpoint: t.endpoint,
                deleted_at: t.deleted_at,
                success_count: t.success_count,
                last_used_at: t.last_used_at,
            })
            .collect();

        // 最近删除的排在前面
        items.sort_by(|a, b| b.deleted_at.cmp(&a.deleted_at));

        TrashListResponse {
            total: items.len(),
            trash: items,
        }
    }

    /// 从回收站恢复凭据
    pub fn restore_credential(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .restore_credential(id)
            .map_err(|e| self.classify_trash_error(e, id))
    }

    /// 从回收站彻底删除凭据（不可恢复）
    pub fn purge_credential(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .purge_credential(id)
            .map_err(|e| self.classify_trash_error(e, id))?;

        // 清理彻底删除凭据的余额缓存残留
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(&id);
        }
        self.save_balance_cache();

        Ok(())
    }

    /// 获取负载均衡模式
    pub fn get_load_balancing_mode(&self) -> LoadBalancingModeResponse {        LoadBalancingModeResponse {
            mode: self.token_manager.get_load_balancing_mode(),
        }
    }

    /// 当前 TLS 后端（供出站 HTTP client 构建复用配置，如代理测活）。
    pub fn tls_backend(&self) -> crate::model::config::TlsBackend {
        self.token_manager.config().tls_backend
    }

    /// 获取服务端配置快照（敏感字段脱敏）
    pub fn get_config_snapshot(&self) -> ConfigSnapshotResponse {
        let config = self.token_manager.config();

        let tls_backend = match config.tls_backend {
            crate::model::config::TlsBackend::Rustls => "rustls",
            crate::model::config::TlsBackend::NativeTls => "native-tls",
        }
        .to_string();

        let mut endpoint_names: Vec<String> = self.known_endpoints.iter().cloned().collect();
        endpoint_names.sort();

        let callback_mode = if config.callback_base_url.is_some() {
            "remote"
        } else {
            "local"
        }
        .to_string();

        ConfigSnapshotResponse {
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            host: config.host.clone(),
            port: config.port,
            region: config.region.clone(),
            kiro_version: config.kiro_version.clone(),
            system_version: config.system_version.clone(),
            node_version: config.node_version.clone(),
            tls_backend,
            load_balancing_mode: self.token_manager.get_load_balancing_mode(),
            default_endpoint: config.default_endpoint.clone(),
            endpoint_names,
            extract_thinking: config.extract_thinking,
            cc_auto_buffer: config.cc_auto_buffer,
            strip_env_noise: config.strip_env_noise,
            tool_clean_leaked_tokens: config.tool_clean_leaked_tokens,
            tool_stream_align_failure: config.tool_stream_align_failure,
            tool_expose_error_to_client: config.tool_expose_error_to_client,
            tool_repair_json: config.tool_repair_json,
            tool_truncation_recovery: config.tool_truncation_recovery,
            tool_description_max_chars: config.tool_description_max_chars,
            encrypt_credentials_at_rest: config.encrypt_credentials_at_rest,
            cooldown_enabled: config.cooldown_enabled,
            rate_limit_enabled: config.rate_limit_enabled,
            rate_limit_daily_max: config.rate_limit_daily_max,
            rate_limit_min_interval_ms: config.rate_limit_min_interval_ms,
            affinity_enabled: config.affinity_enabled,
            priority_in_balanced: config.priority_in_balanced,
            rpm_headroom_factor: config.rpm_headroom_factor,
            rpm_reserve_slots: config.rpm_reserve_slots,
            rpm_hard_gate_overload_wait: config.rpm_hard_gate_overload_wait,
            balance_weight_enabled: config.balance_weight_enabled,
            balance_weight_floor: config.balance_weight_floor,
            health_429_weight_enabled: config.health_429_weight_enabled,
            has_proxy: config.proxy_url.is_some(),
            proxy_url: config.proxy_url.clone(),
            has_admin_key: config
                .admin_api_key
                .as_ref()
                .map(|k| !k.trim().is_empty())
                .unwrap_or(false),
            has_api_key: config
                .api_key
                .as_ref()
                .map(|k| !k.trim().is_empty())
                .unwrap_or(false),
            callback_mode,
            callback_base_url: config.callback_base_url.clone(),
            cors_allowed_origins: config.cors_allowed_origins.clone(),
            ip_allowlist: config.ip_allowlist.clone(),
            trust_forwarded_header: config.trust_forwarded_header,
            ingress_rate_limit_per_min: config.ingress_rate_limit_per_min,
            max_body_bytes: config.max_body_bytes,
            proactive_token_refresh: config.proactive_token_refresh,
            token_refresh_lead_minutes: config.token_refresh_lead_minutes,
            token_refresh_interval_secs: config.token_refresh_interval_secs,
            login_background_enabled: config.login_background_enabled,
            login_background_r18: config.login_background_r18,
            balance_refresh_interval_secs: config.balance_refresh_interval_secs,
            collect_client_fingerprint: config.collect_client_fingerprint,
            config_path: config
                .config_path()
                .map(|p| p.display().to_string()),
        }
    }

    /// 设置负载均衡模式
    pub fn set_load_balancing_mode(
        &self,
        req: SetLoadBalancingModeRequest,
    ) -> Result<LoadBalancingModeResponse, AdminServiceError> {
        // 验证模式值
        if req.mode != "priority" && req.mode != "balanced" {
            return Err(AdminServiceError::InvalidCredential(
                "mode 必须是 'priority' 或 'balanced'".to_string(),
            ));
        }

        self.token_manager
            .set_load_balancing_mode(req.mode.clone())
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        Ok(LoadBalancingModeResponse { mode: req.mode })
    }

    /// 更新服务端配置并持久化到 config.json
    ///
    /// 仅提交的字段被修改。TIER1（冷却/限流/亲和/RPM/负载均衡）改后 reload_config 即时生效；
    /// TIER2（主动预刷新开关/提前量/间隔、余额刷新间隔）改后 abort+respawn 后台任务即时生效；
    /// 其余固化项（host/port/proxy/tls 等）需重启，通过响应的 `restart_fields` 告知前端。
    /// 敏感字段不在此开放。
    pub fn update_config(
        self: &Arc<Self>,
        req: UpdateConfigRequest,
    ) -> Result<UpdateConfigResponse, AdminServiceError> {
        let config_path = self
            .token_manager
            .config()
            .config_path()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| {
                AdminServiceError::InternalError(
                    "配置文件路径未知，无法保存配置".to_string(),
                )
            })?;

        // 从磁盘重新加载，避免覆盖进程外的改动
        let mut config = crate::model::config::Config::load(&config_path)
            .map_err(|e| AdminServiceError::InternalError(format!("加载配置失败: {}", e)))?;

        let mut restart_fields: Vec<String> = Vec::new();
        // TIER1 运行时字段是否有变更 → save 后统一 reload_config 热应用（不重启即生效）。
        let mut hot_changed = false;
        // TIER2 后台任务字段是否有变更 → save+reload 后 respawn 对应任务（不重启即生效）。
        let mut refresh_task_changed = false;
        let mut balance_task_changed = false;
        // TIER3 AppState 热更字段：extract_thinking 改后调 handlers setter 即时生效（不重启）。
        let mut extract_thinking_changed: Option<bool> = None;
        // CC 自动切缓冲开关：改后调 handlers setter 即时生效（进程级镜像，不重启）。
        let mut cc_auto_buffer_changed: Option<bool> = None;
        // 环境噪音剥离开关：改后调 converter setter 即时生效（进程级镜像，不重启）。
        let mut strip_env_noise_changed: Option<bool> = None;
        // 工具错误缓解三开关：改后调 handlers setter 即时生效（进程级镜像，不重启）。
        let mut tool_clean_leaked_tokens_changed: Option<bool> = None;
        let mut tool_stream_align_failure_changed: Option<bool> = None;
        let mut tool_expose_error_to_client_changed: Option<bool> = None;
        let mut tool_repair_json_changed: Option<bool> = None;
        let mut tool_truncation_recovery_changed: Option<bool> = None;
        let mut tool_description_max_chars_changed: Option<usize> = None;
        // at-rest 加密开关变更:变更后立即重写凭据/回收站文件(明文↔密文),不等下次偶发变更。
        let mut encrypt_at_rest_changed = false;

        // —— 需重启生效的字段 ——
        if let Some(v) = req.host {
            let v = v.trim().to_string();
            if v.is_empty() {
                return Err(AdminServiceError::InvalidCredential(
                    "host 不能为空".to_string(),
                ));
            }
            if v != config.host {
                config.host = v;
                restart_fields.push("host".into());
            }
        }
        if let Some(v) = req.port {
            if v == 0 {
                return Err(AdminServiceError::InvalidCredential(
                    "port 必须是 1-65535".to_string(),
                ));
            }
            if v != config.port {
                config.port = v;
                restart_fields.push("port".into());
            }
        }
        if let Some(v) = req.region {
            let v = v.trim().to_string();
            if !v.is_empty() && v != config.region {
                config.region = v;
                restart_fields.push("region".into());
            }
        }
        if let Some(v) = req.kiro_version {
            let v = v.trim().to_string();
            if !v.is_empty() && v != config.kiro_version {
                config.kiro_version = v;
                restart_fields.push("kiroVersion".into());
            }
        }
        if let Some(v) = req.system_version {
            let v = v.trim().to_string();
            if !v.is_empty() && v != config.system_version {
                config.system_version = v;
                restart_fields.push("systemVersion".into());
            }
        }
        if let Some(v) = req.node_version {
            let v = v.trim().to_string();
            if !v.is_empty() && v != config.node_version {
                config.node_version = v;
                restart_fields.push("nodeVersion".into());
            }
        }
        if let Some(v) = req.tls_backend {
            // 出厂发布版一律纯 rustls（见 build.bat / release.yml 的 --no-default-features）。
            // native-tls 已是死路：前端已移除该选项，此处对任何非 rustls 值一律归一到 rustls，
            // 避免把一个"点了会触发回退警告"的死后端持久化进 config.json。宽容接收旧客户端/
            // 旧脚本传来的 "native-tls"，静默归一而非报错（防呆）。
            let backend = match v.as_str() {
                "native-tls" => {
                    tracing::warn!("tlsBackend=native-tls 已废弃，自动归一到 rustls（功能等价）");
                    crate::model::config::TlsBackend::Rustls
                }
                _ => crate::model::config::TlsBackend::Rustls,
            };
            if backend != config.tls_backend {
                config.tls_backend = backend;
                restart_fields.push("tlsBackend".into());
            }
        }
        if let Some(v) = req.default_endpoint {
            let v = v.trim().to_string();
            if !v.is_empty() && v != config.default_endpoint {
                if !self.known_endpoints.is_empty() && !self.known_endpoints.contains(&v) {
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "未知 endpoint '{}'，可用: {}",
                        v,
                        {
                            let mut names: Vec<_> = self.known_endpoints.iter().cloned().collect();
                            names.sort();
                            names.join(", ")
                        }
                    )));
                }
                config.default_endpoint = v;
                restart_fields.push("defaultEndpoint".into());
            }
        }
        // —— 提取 thinking 开关（TIER3 AppState 热更：改后调 handlers setter 即时生效不重启）——
        if let Some(v) = req.extract_thinking {
            if v != config.extract_thinking {
                config.extract_thinking = v;
                extract_thinking_changed = Some(v);
            }
        }
        // —— CC 自动切缓冲开关（TIER3 热更：改后调 handlers setter 即时生效不重启）——
        if let Some(v) = req.cc_auto_buffer {
            if v != config.cc_auto_buffer {
                config.cc_auto_buffer = v;
                cc_auto_buffer_changed = Some(v);
            }
        }
        // —— 环境噪音剥离开关（改后调 converter setter 即时生效不重启）——
        if let Some(v) = req.strip_env_noise {
            if v != config.strip_env_noise {
                config.strip_env_noise = v;
                strip_env_noise_changed = Some(v);
            }
        }
        if let Some(v) = req.tool_clean_leaked_tokens {
            if v != config.tool_clean_leaked_tokens {
                config.tool_clean_leaked_tokens = v;
                tool_clean_leaked_tokens_changed = Some(v);
            }
        }
        if let Some(v) = req.tool_stream_align_failure {
            if v != config.tool_stream_align_failure {
                config.tool_stream_align_failure = v;
                tool_stream_align_failure_changed = Some(v);
            }
        }
        if let Some(v) = req.tool_expose_error_to_client {
            if v != config.tool_expose_error_to_client {
                config.tool_expose_error_to_client = v;
                tool_expose_error_to_client_changed = Some(v);
            }
        }
        if let Some(v) = req.tool_repair_json {
            if v != config.tool_repair_json {
                config.tool_repair_json = v;
                tool_repair_json_changed = Some(v);
            }
        }
        if let Some(v) = req.tool_truncation_recovery {
            if v != config.tool_truncation_recovery {
                config.tool_truncation_recovery = v;
                tool_truncation_recovery_changed = Some(v);
            }
        }
        if let Some(v) = req.tool_description_max_chars {
            if v != config.tool_description_max_chars {
                config.tool_description_max_chars = v;
                tool_description_max_chars_changed = Some(v);
            }
        }
        // at-rest 加密开关:热更(persist 每次读 self.config() 现值)。开→关或关→开都在下次 persist 生效;
        // 想立即把已有明文转密文(或密文转明文),改完开关后触发任意一次凭据变更(或下方主动 persist)即可。
        if let Some(v) = req.encrypt_credentials_at_rest {
            if v != config.encrypt_credentials_at_rest {
                config.encrypt_credentials_at_rest = v;
                hot_changed = true;
                encrypt_at_rest_changed = true;
            }
        }
        // —— TIER1 运行时热更字段：改完 reload_config 即时生效,不进 restart_fields ——
        // （冷却/限流开关/每日上限/间隔/亲和性;由下方统一 reload_config 一并热应用）
        if let Some(v) = req.cooldown_enabled {
            if v != config.cooldown_enabled {
                config.cooldown_enabled = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.rate_limit_enabled {
            if v != config.rate_limit_enabled {
                config.rate_limit_enabled = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.rate_limit_daily_max {
            if v != config.rate_limit_daily_max {
                config.rate_limit_daily_max = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.rate_limit_min_interval_ms {
            if v != config.rate_limit_min_interval_ms {
                config.rate_limit_min_interval_ms = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.affinity_enabled {
            if v != config.affinity_enabled {
                config.affinity_enabled = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.priority_in_balanced {
            if v != config.priority_in_balanced {
                config.priority_in_balanced = v;
                hot_changed = true;
            }
        }
        // ---- 智能调度(全部热更即时生效)。整百分比字段服务端 clamp 到 [0,100],不信任前端。----
        if let Some(v) = req.rpm_headroom_factor {
            let v = v.min(100);
            if v != config.rpm_headroom_factor {
                config.rpm_headroom_factor = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.rpm_reserve_slots {
            // 预留名额上界防 u32 极值污染(远超真实 RPM 容量即无意义,100_000 与 rpm_limit 上界一致)。
            let v = v.min(100_000);
            if v != config.rpm_reserve_slots {
                config.rpm_reserve_slots = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.rpm_hard_gate_overload_wait {
            if v != config.rpm_hard_gate_overload_wait {
                config.rpm_hard_gate_overload_wait = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.balance_weight_enabled {
            if v != config.balance_weight_enabled {
                config.balance_weight_enabled = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.balance_weight_floor {
            let v = v.min(100);
            if v != config.balance_weight_floor {
                config.balance_weight_floor = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.health_429_weight_enabled {
            if v != config.health_429_weight_enabled {
                config.health_429_weight_enabled = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.proxy_url {
            let trimmed = v.trim();
            let new_val = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
            if new_val != config.proxy_url {
                config.proxy_url = new_val;
                restart_fields.push("proxyUrl".into());
            }
        }
        // 代理账密：前端出于安全不回显已存值,只在非空时更新;显式传空串表示清除。
        if let Some(v) = req.proxy_username {
            let new_val = if v.trim().is_empty() { None } else { Some(v.trim().to_string()) };
            if new_val != config.proxy_username {
                config.proxy_username = new_val;
                restart_fields.push("proxyUsername".into());
            }
        }
        if let Some(v) = req.proxy_password {
            let new_val = if v.is_empty() { None } else { Some(v) };
            if new_val != config.proxy_password {
                config.proxy_password = new_val;
                restart_fields.push("proxyPassword".into());
            }
        }
        if let Some(v) = req.callback_base_url {
            let trimmed = v.trim();
            let new_val = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.trim_end_matches('/').to_string())
            };
            if new_val != config.callback_base_url {
                config.callback_base_url = new_val;
                restart_fields.push("callbackBaseUrl".into());
            }
        }
        // userKey（下游对话 api_key）：仅在非空白时更新（防 fail-open：空 key 会让 /v1 匿名可达）。
        // 前端不回显现值，传空串=不改。需重启生效（auth 中间件启动时固化 key）。
        if let Some(v) = req.api_key {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                let new_val = Some(trimmed.to_string());
                if new_val != config.api_key {
                    config.api_key = new_val;
                    restart_fields.push("apiKey".into());
                }
            }
        }

        // —— 反代安全（批次3，均需重启生效）——
        if let Some(v) = req.cors_allowed_origins {
            // 去空白、去空项，保持整表替换语义
            let cleaned: Vec<String> =
                v.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            if cleaned != config.cors_allowed_origins {
                config.cors_allowed_origins = cleaned;
                restart_fields.push("corsAllowedOrigins".into());
            }
        }
        if let Some(v) = req.ip_allowlist {
            let cleaned: Vec<String> =
                v.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            // 校验每条 CIDR 合法，非法直接拒绝（避免静默丢弃导致白名单形同虚设）
            for entry in &cleaned {
                if let Err(e) = crate::common::security::validate_cidr(entry) {
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "ipAllowlist 条目 '{entry}' 非法: {e}"
                    )));
                }
            }
            if cleaned != config.ip_allowlist {
                config.ip_allowlist = cleaned;
                restart_fields.push("ipAllowlist".into());
            }
        }
        if let Some(v) = req.trust_forwarded_header {
            if v != config.trust_forwarded_header {
                config.trust_forwarded_header = v;
                restart_fields.push("trustForwardedHeader".into());
            }
        }
        if let Some(v) = req.ingress_rate_limit_per_min {
            if v != config.ingress_rate_limit_per_min {
                config.ingress_rate_limit_per_min = v;
                restart_fields.push("ingressRateLimitPerMin".into());
            }
        }
        if let Some(v) = req.max_body_bytes {
            if v != config.max_body_bytes {
                config.max_body_bytes = v;
                restart_fields.push("maxBodyBytes".into());
            }
        }

        // —— 主动 token 预刷新（批次4.4，TIER2 后台任务热更：改后 respawn 即时生效不重启）——
        if let Some(v) = req.proactive_token_refresh {
            if v != config.proactive_token_refresh {
                config.proactive_token_refresh = v;
                refresh_task_changed = true;
            }
        }
        if let Some(v) = req.token_refresh_lead_minutes {
            if v != config.token_refresh_lead_minutes {
                config.token_refresh_lead_minutes = v;
                refresh_task_changed = true;
            }
        }
        if let Some(v) = req.token_refresh_interval_secs {
            if v != config.token_refresh_interval_secs {
                config.token_refresh_interval_secs = v;
                refresh_task_changed = true;
            }
        }

        // —— 余额同步（A6，TIER2 后台任务热更：改后 respawn 即时生效不重启）——
        if let Some(v) = req.balance_refresh_interval_secs {
            if v != config.balance_refresh_interval_secs {
                config.balance_refresh_interval_secs = v;
                balance_task_changed = true;
            }
        }

        // —— 立即生效的字段：登录页背景开关 ——
        // 关闭时 random-bg 立即返回 null、后台预取轮次也会自我短路，不需重启。
        let mut login_bg_changed: Option<bool> = None;
        if let Some(v) = req.login_background_enabled {
            if v != config.login_background_enabled {
                config.login_background_enabled = v;
                login_bg_changed = Some(v);
            }
        }

        // —— 立即生效的字段：登录页背景 R18 开关 ——
        // 改后下一轮后台预取 / 池空实时兜底拉取即按新 r18 参数取图，不需重启。
        let mut login_bg_r18_changed: Option<bool> = None;
        if let Some(v) = req.login_background_r18 {
            if v != config.login_background_r18 {
                config.login_background_r18 = v;
                login_bg_r18_changed = Some(v);
            }
        }

        // —— 立即生效的字段：指纹采集开关（隐私）——
        // 关闭后热路径不再解析 device/ip/os/browser，用量记录留空；无需重启。
        let mut fingerprint_changed: Option<bool> = None;
        if let Some(v) = req.collect_client_fingerprint {
            if v != config.collect_client_fingerprint {
                config.collect_client_fingerprint = v;
                fingerprint_changed = Some(v);
            }
        }

        // —— 立即生效的字段：负载均衡模式（并入 TIER1 统一 reload 热应用）——
        if let Some(mode) = req.load_balancing_mode {
            if mode != "priority" && mode != "balanced" {
                return Err(AdminServiceError::InvalidCredential(
                    "loadBalancingMode 必须是 'priority' 或 'balanced'".to_string(),
                ));
            }
            config.load_balancing_mode = mode;
            hot_changed = true;
        }

        // 持久化（一次写盘）
        config
            .save()
            .map_err(|e| AdminServiceError::InternalError(format!("保存配置失败: {}", e)))?;

        // 配置快照(get_config_snapshot)读的是 token_manager.config()(ArcSwap 内存 config)。
        // 只要有**运行时/展示类**字段落盘,就 reload_config 把 ArcSwap 与磁盘对齐,否则快照会读到旧值——
        // ⭐这正是"关掉 R18/背景图保存后、刷新页面开关又变回开"的根因:那些字段过去只更运行时镜像
        //   (AtomicBool)+存盘,却没 reload ArcSwap,导致快照永远回读 ArcSwap 里的旧值。
        // reload_config 从盘重读整份 config 原子换入 ArcSwap(含 login_background/fingerprint/
        // extract_thinking 等所有热字段),幂等安全。
        //
        // ⚠️【proxy split-brain 修复】**绝不因 restart-only 字段(proxyUrl/tls/host/port/callback/
        // adminKey 等)触发 reload**。这些固化项在启动时已被固化到运行态(如 KiroProvider.self.proxy
        // 由 new() 一次性赋值,对话/刷新路径全程用它),而登录流(social/idc/external_idp)却是
        // **活读 config().proxy_url**。若改了 proxyUrl 就 reload 换进 ArcSwap:登录流立刻走新代理、
        // 对话/刷新流仍走启动固化的旧代理 = split-brain(功能性割裂,与"改这些需重启"的语义矛盾)。
        // 故这类字段只进 restart_fields 提示前端重启,ArcSwap 保持旧值 → 全局一致(全旧,重启才全新)。
        // 展示/热字段各有独立 *_changed 标志,不依赖 restart_fields,R18 stale 根治不受影响。
        let hot_or_display_changed = hot_changed
            || refresh_task_changed
            || balance_task_changed
            || login_bg_changed.is_some()
            || login_bg_r18_changed.is_some()
            || fingerprint_changed.is_some()
            || extract_thinking_changed.is_some()
            || cc_auto_buffer_changed.is_some()
            || strip_env_noise_changed.is_some()
            || tool_clean_leaked_tokens_changed.is_some()
            || tool_stream_align_failure_changed.is_some()
            || tool_expose_error_to_client_changed.is_some()
            || tool_repair_json_changed.is_some()
            || tool_truncation_recovery_changed.is_some()
            || tool_description_max_chars_changed.is_some();
        if hot_or_display_changed {
            if let Err(e) = self.token_manager.reload_config() {
                tracing::warn!("配置已存盘但热重载失败,下次重启生效: {}", e);
            }
        }

        // at-rest 加密开关变更:reload_config 后 config 已是新值,立即重写凭据+回收站文件(明文↔密文),
        // 让开/关即时落到磁盘,而非等下次偶发凭据变更。失败仅告警(下次 persist 会补上)。
        if encrypt_at_rest_changed {
            match self.token_manager.repersist_secrets() {
                Ok(true) => tracing::info!("at-rest 加密开关已改,已立即重写凭据/回收站文件"),
                Ok(false) => tracing::warn!(
                    "at-rest 加密开关已改,但当前为单凭据(Single)格式——persist 是 no-op,加密不生效。\
                     请先转为多凭据数组格式(如通过 UI 增删任一号触发格式升级)。"
                ),
                Err(e) => tracing::warn!("at-rest 加密开关已改,但立即重写凭据文件失败(下次变更会补上): {}", e),
            }
        }

        // TIER2 后台任务热重挂（读已 reload 的最新 config，abort 旧任务 + 按需 respawn）。
        if refresh_task_changed {
            self.token_manager.respawn_refresh_task();
        }
        if balance_task_changed {
            self.respawn_balance_task();
        }

        // 登录页背景开关立即应用到运行时镜像（下一次 random-bg / 预取轮次即生效）
        if let Some(v) = login_bg_changed {
            crate::admin_ui::set_login_background_enabled(v);
        }

        // 登录页背景 R18 开关立即应用到运行时镜像（下一轮预取 / 池空兜底拉取即按新参数）
        if let Some(v) = login_bg_r18_changed {
            crate::admin_ui::set_login_background_r18(v);
        }

        // ⭐修复"关闭 R18/背景后缓存不清、刷新还是旧图":开关一变就**立即清空背景图内存池**。
        // 否则池里已缓存的旧参数图(R18/全年龄)会一直服务到自然淘汰完(容量20、每12分钟才补6张),
        // 表现为"关了 R18 保存后刷新仍是旧图"。清池后下次 random-bg 按新参数即时重新拉取。
        if login_bg_r18_changed.is_some() || login_bg_changed.is_some() {
            let cleared = crate::admin_ui::clear_bg_pool();
            tracing::info!("登录背景开关变更,已清空背景图缓存池({} 张)", cleared);
            // ⭐清池后若背景图当前为开启态,立即补一批新参数图填池(不等常驻循环的下一轮 12min tick)。
            // 否则:开启背景图/切换 R18 后池是空的,登录页只能走单张实时兜底(慢/偶尔失败),
            // 表现为"第一次没图、关开偶尔显示一次、再刷新又没"——本次连同预取循环常驻一起根治。
            if config.login_background_enabled {
                crate::admin_ui::trigger_bg_refill();
                tracing::info!("背景图已开启,已触发即时补池(按新参数预取一批)");
            }
        }

        // 指纹采集开关立即应用到热路径运行时镜像（下一个请求即生效）
        if let Some(v) = fingerprint_changed {
            crate::anthropic::set_collect_client_fingerprint(v);
        }

        // TIER3：thinking 提取开关立即应用到热路径进程级镜像（下一个非流式请求即生效）
        if let Some(v) = extract_thinking_changed {
            crate::anthropic::set_extract_thinking(v);
        }

        // TIER3：CC 自动切缓冲开关立即应用到热路径进程级镜像（下一个流式请求即生效）
        if let Some(v) = cc_auto_buffer_changed {
            crate::anthropic::set_cc_auto_buffer(v);
        }

        // 环境噪音剥离开关立即应用到 converter 进程级镜像（下一个请求即生效）
        if let Some(v) = strip_env_noise_changed {
            crate::anthropic::set_strip_env_noise(v);
        }
        // 工具错误缓解三开关立即应用到 handlers 进程级镜像（下一个请求即生效，不重启）。
        if let Some(v) = tool_clean_leaked_tokens_changed {
            crate::anthropic::set_tool_clean_leaked_tokens(v);
        }
        if let Some(v) = tool_stream_align_failure_changed {
            crate::anthropic::set_tool_stream_align_failure(v);
        }
        if let Some(v) = tool_expose_error_to_client_changed {
            crate::anthropic::set_tool_expose_error_to_client(v);
        }
        if let Some(v) = tool_repair_json_changed {
            crate::anthropic::set_tool_repair_json(v);
        }
        if let Some(v) = tool_truncation_recovery_changed {
            crate::anthropic::set_tool_truncation_recovery(v);
        }
        // 工具描述上限立即应用到 converter 进程级镜像（下一个请求即生效，不重启）。
        if let Some(v) = tool_description_max_chars_changed {
            crate::anthropic::set_tool_description_max_chars(v);
        }

        let immediate_changed = hot_changed
            || refresh_task_changed
            || balance_task_changed
            || login_bg_changed.is_some()
            || login_bg_r18_changed.is_some()
            || fingerprint_changed.is_some()
            || extract_thinking_changed.is_some()
            || cc_auto_buffer_changed.is_some()
            || strip_env_noise_changed.is_some()
            || tool_clean_leaked_tokens_changed.is_some()
            || tool_stream_align_failure_changed.is_some()
            || tool_expose_error_to_client_changed.is_some()
            || tool_repair_json_changed.is_some()
            || tool_truncation_recovery_changed.is_some()
            || tool_description_max_chars_changed.is_some();
        let restart_required = !restart_fields.is_empty();
        let message = if restart_required {
            format!(
                "已保存。{} 个字段需重启服务后生效。",
                restart_fields.len()
            )
        } else if immediate_changed {
            "已保存并立即生效（无需重启）。".to_string()
        } else {
            "无改动。".to_string()
        };

        tracing::info!(
            "配置已更新（需重启字段: {:?}, TIER1热更: {}, TIER2重挂: 预刷新={} 余额={}, TIER3: thinking={:?} envNoise={:?}）",
            restart_fields,
            hot_changed,
            refresh_task_changed,
            balance_task_changed,
            extract_thinking_changed,
            strip_env_noise_changed
        );

        Ok(UpdateConfigResponse {
            success: true,
            message,
            restart_required,
            restart_fields,
        })
    }

    /// 强制刷新指定凭据的 Token
    pub async fn force_refresh_token(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .force_refresh_token_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))
    }

    /// 深度验活：通过实际 API 调用检测账号 suspend 状态
    pub async fn deep_verify_credential(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .deep_verify_credential(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))
    }

    /// 【F】列出指定 external_idp 号在候选 region 的全部 profile 及验活结果（供前端选 region）。
    /// 返回 `[(arn, region, account, usable, subscriptionTitle, reason)]`。
    pub async fn probe_regions(
        &self,
        id: u64,
    ) -> Result<Vec<crate::kiro::token_manager::ProfileCandidate>, AdminServiceError> {
        self.token_manager
            .probe_regions_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))
    }

    /// 【F】切换 external_idp 号到目标 region 的 profile（仅验活可用才写入）。
    /// 返回切换后拿到的订阅标题（若有）。
    pub async fn switch_profile_region(
        &self,
        id: u64,
        arn: &str,
    ) -> Result<Option<String>, AdminServiceError> {
        self.token_manager
            .switch_profile_region_for(id, arn)
            .await
            .map_err(|e| self.classify_balance_error(e, id))
    }

    /// 探测指定凭据当前可用的模型列表（选中令牌后手动触发）。
    /// 探测该凭据可用哪些模型。返回 (每模型明细[(model,status,credits)], 本次总花费 credits)。
    /// 认证/账号级失败时返回 Err（前端提示先刷新/检查号）。
    /// models 为空时用默认候选清单（真实 Kiro modelId，从便宜到贵）。
    pub async fn probe_models(
        &self,
        id: u64,
        models: Option<Vec<String>>,
    ) -> Result<(Vec<(String, String, f64)>, f64), AdminServiceError> {
        // 默认候选：覆盖 opus/sonnet 主力 + 一个最便宜的国产模型验证探测机制。
        // 真实 Kiro modelId（见 kiro-model-catalog）；探测直发不过 map_model。
        let list = models.filter(|v| !v.is_empty()).unwrap_or_else(|| {
            // 默认候选与 model_catalog::CATALOG 对齐(真实 Kiro modelId，探测直发不过 map_model)。
            // 补齐 opus-4.5/4.7，消除「/v1/models 广告了却无法探测」的清单漂移。
            [
                "qwen3-coder-next",
                "claude-haiku-4.5",
                "claude-sonnet-4.5",
                "claude-sonnet-4.6",
                "claude-opus-4.5",
                "claude-opus-4.6",
                "claude-opus-4.7",
                "claude-opus-4.8",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect()
        });
        self.token_manager
            .probe_models(id, &list)
            .await
            .map_err(|e| self.classify_balance_error(e, id))
    }

    /// 导出指定凭据的原始 JSON（用于 Admin 令牌下载）
    ///
    /// 返回可直接重新导入本系统的完整 KiroCredentials（camelCase）。
    /// 包含 refreshToken 等敏感字段，仅经 Admin 鉴权后可调用。
    pub fn export_credential(&self, id: u64) -> Result<KiroCredentials, AdminServiceError> {
        self.token_manager
            .export_credential(id)
            .ok_or(AdminServiceError::NotFound { id })
    }

    /// 一键重启本服务（spawn 后立即返回，实际退出延迟约 1 秒，systemd 3s 内自动拉起）。
    ///
    /// **实现方式：优雅自退，让 systemd 自动重启——不需要任何提权。**
    /// 根因（2026-07-08 定位）：systemd unit 设了 `NoNewPrivileges=true`，它会**永久禁止**
    /// 本进程及其子进程通过 setuid 提权，于是旧实现的 `sudo -n systemd-run ...` 静默失败
    /// （后台收到请求、打了日志，但 sudo 无法提权 → 什么都没发生 = "点了没反应"）。
    /// 由于 unit 配了 `Restart=always` + `RestartSec=3`，进程**只要退出**（任意退出码），
    /// systemd 就会在 3 秒内自动重新拉起。因此这里改为：延迟 1 秒（给 HTTP 200 flush 时间）
    /// 后 `std::process::exit(0)`，完全绕开 sudo/NoNewPrivileges，稳定可靠。
    /// 若将来 unit 去掉 Restart=always，此法失效——但当前部署（见 kirostudio.service）已配置。
    pub fn restart_service(&self) -> Result<(), AdminServiceError> {
        // Windows：用户普遍**裸跑双击 exe**，无 systemd/监督脚本会在 exit(0) 后重拉。
        // 若直接 exit(0),服务就此消失(H1)。故 Windows 下改为**进程自重启**:spawn 一个 detached
        // helper(cmd),让它等本进程退出+端口释放后,用**原 exe 路径**(OTA 已把新二进制放到原路径)
        // 加相同的 --config/--credentials 参数、相同 cwd 重新拉起,再由本进程 exit(0)。
        #[cfg(target_os = "windows")]
        {
            self.spawn_windows_relaunch();
            tokio::spawn(async {
                // 睡 1 秒让本次 HTTP 200 flush 给前端,再退出让出端口,helper 会拉起新进程。
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                tracing::warn!("一键重启(Windows 裸跑):进程退出,已交给 detached helper 拉起新二进制");
                std::process::exit(0);
            });
            return Ok(());
        }

        #[cfg(not(target_os = "windows"))]
        {
            tracing::warn!(
                "收到一键重启请求，约 1 秒后进程自退，由 systemd（Restart=always）在 3 秒内自动拉起"
            );
            // detached 异步任务：睡 1 秒让本次 HTTP 200 响应先 flush 给前端，再退出触发 systemd 重启。
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                tracing::warn!("一键重启：进程即将退出，交由 systemd 自动拉起");
                std::process::exit(0);
            });
            Ok(())
        }
    }

    /// Windows 专用：写一个临时 `.bat`，让它等本进程退出+端口释放后重新拉起新二进制。
    ///
    /// 为什么用 .bat 而不是 `cmd /C "start ... "`：Rust `Command::args(["/C", line])` 会对
    /// 整串再加一层引号转义传给 cmd，叠加 `start "" "path"` 的多重引号 + `&`，cmd 解析错乱
    /// 会去找 `\\`（实测 bug:`Windows cannot find '\\'` + `Access is denied`）。批处理**文件**的
    /// 解析规则可预测,把带空格路径的引号写进文件即可,彻底绕开 `/C` 引号地狱。
    ///
    /// 为什么要中间脚本而非当前进程直接 spawn 新 exe：新旧进程抢同一监听端口,当前进程还没退出、
    /// 端口没释放,新 exe 会 bind 失败。脚本先 sleep 等旧进程退出+端口释放,再启动新 exe。
    #[cfg(target_os = "windows")]
    fn spawn_windows_relaunch(&self) {
        // 复用模块级自由函数（托盘「重启服务」项亦共用同一逻辑），传入启动时的
        // config/credentials 路径，让新进程用同一套路径重启。
        let config_path = self
            .token_manager
            .config()
            .config_path()
            .map(|p| p.to_path_buf());
        let credentials_path = self.token_manager.credentials_path();
        spawn_windows_relaunch_process(config_path, credentials_path);
    }
}

/// Windows 专用自由函数：写一个临时 `.bat`，让它等本进程退出+端口释放后重新拉起新二进制。
///
/// 从 [`AdminService::spawn_windows_relaunch`] 抽出为模块级函数，供**面板一键重启**与
/// **系统托盘「重启服务」**共用同一套久经验证的自重启逻辑（不依赖 `AdminService` 实例，
/// 托盘线程也能调）。`config_path` / `credentials_path` 由调用方传入（启动参数），让新进程
/// 用同一套路径。为何用 .bat + 中间脚本 + `CREATE_BREAKAWAY_FROM_JOB` 的完整原因见函数体注释。
#[cfg(target_os = "windows")]
pub(crate) fn spawn_windows_relaunch_process(
    config_path: Option<PathBuf>,
    credentials_path: Option<PathBuf>,
) {
    {
        use std::io::Write;
        use std::os::windows::process::CommandExt;
        use std::process::Command;

        // OTA 已把新二进制放到原 exe 路径（rename 旧→.bak、new→原路径）。current_exe 即目标。
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("Windows 自重启:取 current_exe 失败,无法拉起新进程: {e}");
                return;
            }
        };
        // 新进程的工作目录：沿用当前 cwd（config/credentials 相对路径解析依赖它）。
        let cwd = std::env::current_dir().ok();

        // 组装批处理里的 exe 调用行:每个含空格/特殊字符的路径用双引号包裹(bat 内引号规则简单可靠)。
        let q = |s: &str| format!("\"{}\"", s);
        let mut launch = format!("start \"KiroStudio\" {}", q(&exe.to_string_lossy()));
        if let Some(cfg) = &config_path {
            launch.push_str(&format!(" --config {}", q(&cfg.to_string_lossy())));
        }
        if let Some(cred) = &credentials_path {
            launch.push_str(&format!(" --credentials {}", q(&cred.to_string_lossy())));
        }

        // 批处理内容:等 ~3 秒(ping 当 sleep,免 timeout 交互性)→ 起新 exe → 删自身。
        // `start "标题" "exe" args` 让新 exe 独立于本 .bat 存活;`chcp 65001` 防中文路径乱码。
        let cwd_line = cwd
            .as_ref()
            .map(|d| format!("cd /d \"{}\"\r\n", d.to_string_lossy()))
            .unwrap_or_default();
        let bat = format!(
            "@echo off\r\nchcp 65001 >nul\r\n{cwd_line}ping 127.0.0.1 -n 4 >nul\r\n{launch}\r\n(goto) 2>nul & del \"%~f0\"\r\n"
        );

        // 写进临时目录的唯一 .bat。
        let bat_path = std::env::temp_dir()
            .join(format!("kirostudio-relaunch-{}.bat", uuid::Uuid::new_v4()));
        {
            let mut f = match std::fs::File::create(&bat_path) {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!("Windows 自重启:创建重启脚本失败,请手动重启: {e}");
                    return;
                }
            };
            if let Err(e) = f.write_all(bat.as_bytes()) {
                tracing::error!("Windows 自重启:写重启脚本失败,请手动重启: {e}");
                return;
            }
        }

        // DETACHED_PROCESS(0x8) | CREATE_NEW_PROCESS_GROUP(0x200) | CREATE_NO_WINDOW(0x8000000)
        // + CREATE_BREAKAWAY_FROM_JOB(0x1000000):脱离父进程的 job object。
        // 【根因】若本进程被放进一个 job(如某些启动器/终端/服务包装把子进程装进 job,且 job 设了
        // KILL_ON_JOB_CLOSE),主进程 exit(0) 会**连带杀掉** detached 子进程 → 重启脚本还没 ping 完
        // 就被杀 → 新 exe 起不来(实测:Bash `&` 后台起的实例点重启即复现)。BREAKAWAY 让 cmd 脱离
        // 该 job,主进程退出不再牵连它。但 job 若禁止 breakaway,带此 flag 会 spawn 失败——故**先带
        // breakaway 尝试,失败再回退不带**(不在 job / 双击场景本就不需要,回退等价原行为)。
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        let base_flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;

        let bat_str = bat_path.to_string_lossy().to_string();
        let spawn_with = |flags: u32| {
            let mut c = Command::new("cmd");
            c.args(["/C", &bat_str]).creation_flags(flags);
            if let Some(dir) = &cwd {
                c.current_dir(dir);
            }
            c.spawn()
        };
        // 先带 breakaway;失败(job 禁止 breakaway / 其它)则回退到原 flags。
        let result = spawn_with(base_flags | CREATE_BREAKAWAY_FROM_JOB)
            .or_else(|_| spawn_with(base_flags));
        match result {
            Ok(_) => tracing::warn!(
                "Windows 自重启:已 spawn 重启脚本({:?}),将在本进程退出后拉起 {exe:?}",
                bat_path
            ),
            Err(e) => tracing::error!(
                "Windows 自重启:spawn 重启脚本失败,OTA 后服务可能不会自动恢复,请手动重启: {e}"
            ),
        }
    }
}

impl AdminService {
    // ============ 存储统计 / 清理（运维）============

    /// 用量数据目录（SQLite traces.db 与 usage-*.jsonl 所在目录）。
    fn usage_data_dir(&self) -> PathBuf {
        PathBuf::from(&self.token_manager.config().usage_data_dir)
    }

    /// 统计一个文件（含 SQLite 的 -wal/-shm 附属文件）的总字节数。
    fn file_size_bytes(path: &Path) -> u64 {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }

    /// 分区磁盘统计：trace.db / usage jsonl / trash.json / 背景图内存池。
    ///
    /// 路径全部从现有 config 派生，绝不接受请求传入路径（防目录穿越）。
    /// `trace_db` 由调用方（handler）从 AdminState 注入；未启用统计时为 None，
    /// 相应分区不出现在结果中。
    pub fn storage_stats(&self, trace_db: Option<&Arc<TraceDb>>) -> StorageStatsResponse {
        let mut partitions: Vec<StoragePartition> = Vec::new();
        let mut total_disk_bytes: u64 = 0;
        let usage_enabled = self.token_manager.config().usage_enabled;
        let data_dir = self.usage_data_dir();

        // 1) traces：SQLite 明细（含 WAL/SHM 附属文件）
        if let Some(db) = trace_db {
            let db_path = data_dir.join("traces.db");
            let mut bytes = Self::file_size_bytes(&db_path);
            for ext in ["-wal", "-shm"] {
                let mut side = db_path.clone().into_os_string();
                side.push(ext);
                bytes += Self::file_size_bytes(Path::new(&side));
            }
            let items = db.count().unwrap_or(0);
            total_disk_bytes += bytes;
            partitions.push(StoragePartition {
                key: "traces".to_string(),
                label: "请求明细 (SQLite)".to_string(),
                bytes,
                items,
                path: Some(db_path.display().to_string()),
                in_memory: false,
            });
        }

        // 2) usage_jsonl：按天分文件的 JSONL
        if usage_enabled {
            let (bytes, files) = Self::scan_usage_jsonl(&data_dir);
            total_disk_bytes += bytes;
            partitions.push(StoragePartition {
                key: "usage_jsonl".to_string(),
                label: "用量日志 (JSONL)".to_string(),
                bytes,
                items: files,
                path: Some(data_dir.display().to_string()),
                in_memory: false,
            });
        }

        // 3) trash：凭据回收站
        if let Some(trash_path) = self.token_manager.cache_dir().map(|d| d.join("trash.json")) {
            let bytes = Self::file_size_bytes(&trash_path);
            let items = self.token_manager.list_trash().len() as u64;
            total_disk_bytes += bytes;
            partitions.push(StoragePartition {
                key: "trash".to_string(),
                label: "凭据回收站".to_string(),
                bytes,
                items,
                path: Some(trash_path.display().to_string()),
                in_memory: false,
            });
        }

        // 4) bg_cache：登录页背景图内存池（无落盘，统计常驻内存）
        let (bg_count, bg_bytes) = crate::admin_ui::bg_pool_stats();
        partitions.push(StoragePartition {
            key: "bg_cache".to_string(),
            label: "登录背景图缓存 (内存)".to_string(),
            bytes: bg_bytes,
            items: bg_count as u64,
            path: None,
            in_memory: true,
        });

        StorageStatsResponse {
            partitions,
            total_disk_bytes,
            usage_enabled,
        }
    }

    /// 扫描目录下 `usage-*.jsonl`，返回 (总字节数, 文件数)。
    fn scan_usage_jsonl(dir: &Path) -> (u64, u64) {
        let mut bytes = 0u64;
        let mut files = 0u64;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let is_jsonl = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("usage-") && n.ends_with(".jsonl"))
                    .unwrap_or(false);
                if is_jsonl {
                    bytes += Self::file_size_bytes(&path);
                    files += 1;
                }
            }
        }
        (bytes, files)
    }

    /// 自定义清理：按 target 白名单 + 可选时间窗口清理数据。
    ///
    /// 安全：target 为固定枚举，路径全部从 config 派生，绝不接受任意路径（防穿越）。
    /// - traces：复用 [`TraceDb::retention_cleanup`]（keep_days）
    /// - usage_jsonl：按文件名日期删除早于 keep_days 的日文件
    /// - trash：复用 [`MultiTokenManager::purge_expired_trash`]
    /// - bg_cache：清空内存池
    /// - all：以上全部
    pub fn storage_cleanup(
        &self,
        target: &str,
        older_than_days: Option<i64>,
        trace_db: Option<&Arc<TraceDb>>,
    ) -> Result<StorageCleanupResponse, AdminServiceError> {
        // 白名单校验：非法 target 直接 400
        let valid = matches!(
            target,
            "traces" | "usage_jsonl" | "trash" | "bg_cache" | "all"
        );
        if !valid {
            return Err(AdminServiceError::InvalidCredential(format!(
                "非法清理目标 '{}'，允许: traces | usage_jsonl | trash | bg_cache | all",
                target
            )));
        }

        let do_all = target == "all";
        let mut results: Vec<StorageCleanupItem> = Vec::new();

        // traces
        if do_all || target == "traces" {
            results.push(self.cleanup_traces(older_than_days, trace_db));
        }
        // usage_jsonl
        if do_all || target == "usage_jsonl" {
            results.push(self.cleanup_usage_jsonl(older_than_days));
        }
        // trash
        if do_all || target == "trash" {
            results.push(self.cleanup_trash(older_than_days));
        }
        // bg_cache
        if do_all || target == "bg_cache" {
            let n = crate::admin_ui::clear_bg_pool() as u64;
            results.push(StorageCleanupItem {
                key: "bg_cache".to_string(),
                removed: n,
                freed_bytes: 0,
                note: Some("已清空背景图内存池".to_string()),
            });
        }

        let removed_total: u64 = results.iter().map(|r| r.removed).sum();
        Ok(StorageCleanupResponse {
            success: true,
            message: format!("清理完成，共移除 {} 项", removed_total),
            results,
        })
    }

    /// 清理 traces：keep_days 未指定时用 config.usage_retention_days。
    fn cleanup_traces(
        &self,
        older_than_days: Option<i64>,
        trace_db: Option<&Arc<TraceDb>>,
    ) -> StorageCleanupItem {
        let Some(db) = trace_db else {
            return StorageCleanupItem {
                key: "traces".to_string(),
                removed: 0,
                freed_bytes: 0,
                note: Some("用量统计未启用，跳过".to_string()),
            };
        };
        // older_than_days 为负会让 retention_cleanup 的 cutoff 落到未来 → 删光全部明细。
        // 与 usage_jsonl/trash 分支口径一致，下限钳到 0（0=删早于此刻的全部历史，非负安全）。
        let keep_days = older_than_days
            .unwrap_or(self.token_manager.config().usage_retention_days)
            .max(0);
        match db.retention_cleanup(keep_days) {
            Ok(n) => StorageCleanupItem {
                key: "traces".to_string(),
                removed: n as u64,
                freed_bytes: 0,
                note: Some(format!("删除 {} 天前的明细", keep_days)),
            },
            Err(e) => StorageCleanupItem {
                key: "traces".to_string(),
                removed: 0,
                freed_bytes: 0,
                note: Some(format!("清理失败: {}", e)),
            },
        }
    }

    /// 清理 usage_jsonl：删除文件名日期早于 keep_days 的日文件（keep_days<=0 删全部）。
    fn cleanup_usage_jsonl(&self, older_than_days: Option<i64>) -> StorageCleanupItem {
        let keep_days = older_than_days.unwrap_or(self.token_manager.config().usage_retention_days);
        let dir = self.usage_data_dir();
        // 保留窗口起点日期（UTC）：文件名日期早于此的被删
        let cutoff = chrono::Utc::now().date_naive() - chrono::Duration::days(keep_days.max(0));
        let mut removed = 0u64;
        let mut freed = 0u64;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n,
                    None => continue,
                };
                // 仅匹配 usage-YYYY-MM-DD.jsonl
                let is_jsonl = name.starts_with("usage-") && name.ends_with(".jsonl");
                if !is_jsonl {
                    continue;
                }
                let date_part = &name["usage-".len()..name.len() - ".jsonl".len()];
                let file_date = match chrono::NaiveDate::parse_from_str(date_part, "%Y-%m-%d") {
                    Ok(d) => d,
                    Err(_) => continue, // 文件名不含合法日期，保守跳过
                };
                if file_date < cutoff {
                    let size = Self::file_size_bytes(&path);
                    if std::fs::remove_file(&path).is_ok() {
                        removed += 1;
                        freed += size;
                    }
                }
            }
        }
        StorageCleanupItem {
            key: "usage_jsonl".to_string(),
            removed,
            freed_bytes: freed,
            note: Some(format!("删除 {} 天前的日文件", keep_days)),
        }
    }

    /// 清理 trash：keep_days 未指定时用 config.trash_retention_days。
    fn cleanup_trash(&self, older_than_days: Option<i64>) -> StorageCleanupItem {
        // trash 保留期为 u32 天；older_than_days 为负时按 0（全部清）处理
        let keep_days: u32 = match older_than_days {
            Some(d) => d.max(0) as u32,
            None => self.token_manager.config().trash_retention_days,
        };
        let n = self.token_manager.purge_expired_trash(keep_days) as u64;
        StorageCleanupItem {
            key: "trash".to_string(),
            removed: n,
            freed_bytes: 0,
            note: Some(format!("清理 {} 天前的回收站条目（0=永久保留不清）", keep_days)),
        }
    }

    // ============ 余额缓存持久化 ============

    fn load_balance_cache_from(cache_path: &Option<PathBuf>) -> HashMap<u64, CachedBalance> {
        let path = match cache_path {
            Some(p) => p,
            None => return HashMap::new(),
        };

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };

        // 文件中使用字符串 key 以兼容 JSON 格式
        let map: HashMap<String, CachedBalance> = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("解析余额缓存失败，将忽略: {}", e);
                return HashMap::new();
            }
        };

        let now = Utc::now().timestamp() as f64;
        map.into_iter()
            .filter_map(|(k, v)| {
                let id = k.parse::<u64>().ok()?;
                // 修复：启动恢复用【展示保留上限】(7 天)，而非 5 分钟新鲜度阈值。
                // 这样重启后仍能立刻显示上次的余额数字（前端据 cached_at 标注新鲜度），
                // 而不是因为磁盘缓存 >5 分钟就整批丢成“未知”。只有陈旧到 7 天才丢弃。
                if (now - v.cached_at) < BALANCE_CACHE_DISPLAY_MAX_AGE_SECS as f64 {
                    Some((id, v))
                } else {
                    None
                }
            })
            .collect()
    }

    fn save_balance_cache(&self) {
        let path = match &self.cache_path {
            Some(p) => p,
            None => return,
        };

        // 持有锁期间完成序列化和写入，防止并发损坏
        let cache = self.balance_cache.lock();
        let map: HashMap<String, &CachedBalance> =
            cache.iter().map(|(k, v)| (k.to_string(), v)).collect();

        match serde_json::to_string_pretty(&map) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!("保存余额缓存失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("序列化余额缓存失败: {}", e),
        }
    }

    // ============ 错误分类 ============

    /// 分类简单操作错误（set_disabled, set_priority, reset_and_enable）
    fn classify_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类余额查询错误（可能涉及上游 API 调用）
    fn classify_balance_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        // 0. 结构化诊断优先：若错误链携带 DiagnosedError，直接透传其诊断（归因+引导），
        //    绝不降级成字符串关键词匹配（那会丢结构 → 裸 502，正是本轮要根治的病）。
        if let Some(de) = e.downcast_ref::<crate::kiro::token_manager::DiagnosedError>() {
            return AdminServiceError::Diagnosed(de.diagnosis.clone());
        }

        let msg = e.to_string();

        // 1. 凭据不存在
        if msg.contains("不存在") {
            return AdminServiceError::NotFound { id };
        }

        // 2. API Key 凭据不支持刷新：客户端请求错误，映射为 400
        if msg.contains("API Key 凭据不支持刷新") {
            return AdminServiceError::InvalidCredential(msg);
        }

        // 2b. region profile 未开通（FEATURE_NOT_SUPPORTED）——可解释错误：
        //     该 region 的 external_idp profile 未开通，刷新路径会自动 reprobe 纠正到可用 region，
        //     或让用户手动切换。归为可解释的凭据错误（400），并给出中文提示。
        //     判据只认 FEATURE_NOT_SUPPORTED（上游真实的「该 region 未开通」信号）——**不再**用
        //     `msg.contains("region profile")` 模糊匹配：那会误伤 probe_regions_for 对非 external_idp
        //     号 bail 的「仅 External IdP 凭据支持列出 region profile」，把「号类型不对」误报成
        //     「region 未开通，将自动纠正」，误导用户以为是区域问题。号类型错走下面的默认分支原文透出。
        if msg.contains("FEATURE_NOT_SUPPORTED") {
            return AdminServiceError::InvalidCredential(format!(
                "该 region profile 未开通，将自动纠正到可用 region（或手动切换 region）: {}",
                msg
            ));
        }

        // 3. 上游服务错误特征：HTTP 响应错误或网络错误
        let is_upstream_error =
            // HTTP 响应错误（来自 refresh_*_token 的错误消息）
            msg.contains("凭证已过期或无效") ||
            msg.contains("权限不足") ||
            msg.contains("已被限流") ||
            msg.contains("服务器错误") ||
            msg.contains("Token 刷新失败") ||
            msg.contains("暂时不可用") ||
            // 网络错误（reqwest 错误）
            msg.contains("error trying to connect") ||
            msg.contains("connection") ||
            msg.contains("timeout") ||
            msg.contains("timed out");

        if is_upstream_error {
            AdminServiceError::UpstreamError(msg)
        } else {
            // 4. 默认归类为内部错误（本地验证失败、配置错误等）
            // 包括：缺少 refreshToken、refreshToken 已被截断、无法生成 machineId 等
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类添加凭据错误
    fn classify_add_error(&self, e: anyhow::Error) -> AdminServiceError {
        let msg = e.to_string();

        // 凭据验证失败（refreshToken 无效、格式错误等）
        let is_invalid_credential = msg.contains("缺少 refreshToken")
            || msg.contains("refreshToken 为空")
            || msg.contains("refreshToken 已被截断")
            || msg.contains("凭据已存在")
            || msg.contains("refreshToken 重复")
            || msg.contains("kiroApiKey 重复")
            || msg.contains("缺少 kiroApiKey")
            || msg.contains("kiroApiKey 为空")
            || msg.contains("凭证已过期或无效")
            || msg.contains("权限不足")
            || msg.contains("已被限流");

        if is_invalid_credential {
            AdminServiceError::InvalidCredential(msg)
        } else if msg.contains("error trying to connect")
            || msg.contains("connection")
            || msg.contains("timeout")
        {
            AdminServiceError::UpstreamError(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类删除凭据错误
    fn classify_delete_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else if msg.contains("只能删除已禁用的凭据") || msg.contains("请先禁用凭据") {
            AdminServiceError::InvalidCredential(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类回收站操作错误（restore / purge）
    fn classify_trash_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("回收站中不存在") {
            AdminServiceError::NotFound { id }
        } else if msg.contains("凭据已存在") || msg.contains("重复") {
            AdminServiceError::InvalidCredential(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }
}

#[cfg(test)]
mod insight_text_tests {
    use super::*;
    use crate::kiro::cooldown::{CooldownInfo, CooldownReason};

    /// 无冷却 + 未饱和 → "畅通"
    #[test]
    fn insight_clear() {
        assert_eq!(build_insight_text(1, 3, 50, false, false, None), "畅通");
    }

    /// 速率限制冷却中：含"冷却中（速率限制）剩Ns，已触发K次"，剩余毫秒向上取整到秒
    #[test]
    fn insight_rate_limit_cooldown() {
        let cd = CooldownInfo {
            credential_id: 54,
            reason: CooldownReason::RateLimitExceeded,
            started_at_ms: 0,
            remaining_ms: 21_500, // 向上取整应为 22s
            trigger_count: 3,
        };
        let text = build_insight_text(54, 40, 50, false, false, Some(&cd));
        assert_eq!(text, "#54 冷却中（速率限制）剩22s，已触发3次");
    }

    /// 非速率限制冷却：走通用分支（不带触发次数）
    #[test]
    fn insight_other_cooldown() {
        let cd = CooldownInfo {
            credential_id: 7,
            reason: CooldownReason::ServerError,
            started_at_ms: 0,
            remaining_ms: 5_000,
            trigger_count: 1,
        };
        let text = build_insight_text(7, 0, 50, false, false, Some(&cd));
        assert_eq!(text, "#7 冷却中（服务器错误）剩5s");
    }

    /// 已达软上限 → "已达软上限，建议分流"
    #[test]
    fn insight_saturated() {
        let text = build_insight_text(54, 50, 50, true, false, None);
        assert_eq!(text, "#54 近60s 50/50 已达软上限，建议分流");
    }

    /// 接近软上限（>=80%）但未饱和 → "接近软上限，建议分流"
    #[test]
    fn insight_near_saturation() {
        // 40/50 = 80%
        let text = build_insight_text(54, 40, 50, false, false, None);
        assert_eq!(text, "#54 近60s 40/50 接近软上限，建议分流");
    }

    /// rpm_limit=0（不限制）时永不判为接近上限，恒"畅通"
    #[test]
    fn insight_no_limit_always_clear() {
        assert_eq!(build_insight_text(9, 999, 0, false, false, None), "畅通");
    }

    /// 已禁用号:显示"已禁用"而非"畅通"(即便有 RPM/未冷却)
    #[test]
    fn insight_disabled() {
        assert_eq!(build_insight_text(54, 0, 50, false, true, None), "#54 已禁用（不参与调度）");
    }
}

#[cfg(test)]
mod balance_cache_tests {
    use super::*;

    fn make_cached(id: u64, cached_at: f64) -> (String, CachedBalance) {
        (
            id.to_string(),
            CachedBalance {
                cached_at,
                data: BalanceResponse {
                    id,
                    subscription_title: Some("Kiro Pro".to_string()),
                    current_usage: 10.0,
                    usage_limit: 100.0,
                    remaining: 90.0,
                    usage_percentage: 10.0,
                    next_reset_at: None,
                    overage_enabled: false,
                    overage_cap: 0.0,
                    effective_limit: 100.0,
                },
            },
        )
    }

    /// 回归测试：启动恢复必须保留“陈旧但仍在展示保留期内”的余额缓存，
    /// 而不是用 5 分钟新鲜度阈值把它整批丢成“未知”（这正是重启后余额消失的根因）。
    #[test]
    fn load_keeps_stale_but_within_display_window() {
        let dir = std::env::temp_dir().join(format!("ks_bal_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kiro_balance_cache.json");

        let now = Utc::now().timestamp() as f64;
        // 1 小时前写入：远超 5 分钟新鲜度阈值，但远在 7 天展示保留期内
        let stale = now - 3600.0;
        // 8 天前写入：超过展示保留期，应被丢弃
        let ancient = now - (8.0 * 24.0 * 3600.0);

        let mut map: HashMap<String, CachedBalance> = HashMap::new();
        let (k1, v1) = make_cached(1, stale);
        let (k2, v2) = make_cached(2, ancient);
        map.insert(k1, v1);
        map.insert(k2, v2);
        std::fs::write(&path, serde_json::to_string(&map).unwrap()).unwrap();

        let loaded = AdminService::load_balance_cache_from(&Some(path.clone()));

        // 陈旧但在展示窗口内 → 保留（重启后前端仍能显示上次数字）
        assert!(loaded.contains_key(&1), "陈旧但在 7 天内的缓存必须保留");
        // 超过展示窗口 → 丢弃（避免无界陈旧）
        assert!(!loaded.contains_key(&2), "超过 7 天的缓存应被丢弃");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
