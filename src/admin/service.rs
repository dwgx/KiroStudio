//! Admin API 业务逻辑服务

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::model::socks_node::{
    MAX_SOCKS_NODES, SocksNode, SocksNodeFileCompat, SocksNodeTest,
};
use crate::kiro::token_manager::{
    DisabledReason, MultiTokenManager, sha256_hex, validate_refresh_token,
};

use super::error::AdminServiceError;
use super::external_idp_login::{
    ExternalIdpLeg1Result, ExternalIdpLeg2Result, ExternalIdpLoginManager, ExternalIdpSelectResult,
    ExternalIdpStartResult,
};
use super::idc_login::IdcLoginManager;
use super::idc_login::{IdcPollResult, IdcStartResult};
use super::social_login::SocialLoginManager;
pub use super::social_login::{PollResult, StartResult};
use super::types::{
    AddCredentialRequest, AddCredentialResponse, BalanceResponse, BatchDeleteItemResult,
    CleanupDisabledResponse, CleanupSkippedItem, ConfigSnapshotResponse, CredentialStatusItem,
    CredentialsStatusResponse, DiagnosticsConfigSummary, DiagnosticsCredentialEntry,
    DiagnosticsPoolHealth, DiagnosticsSnapshotResponse, DiagnosticsVersion,
    DisableQuotaExceededResponse, ImportConfigResponse, ImportKeyItem,
    ImportKeyResult, ImportKeysRequest, ImportKeysResponse, KamExportAccount,
    KamExportResponse, LoadBalancingModeResponse, ReprobeRegionResponse,
    SetLoadBalancingModeRequest, SocksNodeBulkImportItem, SocksNodeBulkImportOutcome,
    SocksNodeUpsertRequest, SocksNodeView, StorageCleanupItem, StorageCleanupResponse,
    StoragePartition, StorageStatsResponse, TrashItemResponse, TrashListResponse,
    UpdateConfigRequest, UpdateConfigResponse, build_import_response, mask_import_key,
};
use crate::kiro::auth::social::OAuthCallbackData;
use crate::usage::TraceDb;

/// SSO Token 导入结果（`POST /api/admin/credentials/import-sso`）。
pub struct ImportSsoTokenResult {
    pub credential_id: u64,
    /// 解析到的账号 email（best-effort，可能为 None）。
    pub email: Option<String>,
}

/// 余额缓存【新鲜度】阈值（秒），5 分钟。
/// 仅用于 `get_balance` 的按需（hover）路径：决定是否需要重新向上游拉取。
/// 注意：这【不是】展示缓存的丢弃阈值——展示用 `BALANCE_CACHE_DISPLAY_MAX_AGE_SECS`。
const BALANCE_CACHE_TTL_SECS: i64 = 300;

/// 余额查询等待上游的硬上限（秒）。
///
/// 取 6s 的理由：上游 `web_portal` 自己的 client 超时是 30s/60s，而前端 axios 是 15s ——
/// 若不在这一层设更短的闸门，用户必然先看到前端超时失败。6s 足够正常往返（实测上游
/// 健康时是百毫秒级），又远低于前端超时，使"慢"能被转成"显示上次已知值 + stale 标记"
/// 而不是转圈或报错。
const BALANCE_UPSTREAM_TIMEOUT_SECS: u64 = 6;

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

/// 批量导入 Key 时的最大在飞条数（[`AdminService::import_keys`]）。
///
/// 取 4 的依据：单条导入的耗时以 `add_credential` 内那次 `get_usage_limits_for`
/// 上游往返为主（非 CPU），所以并发度只需盖住 RTT，不需要按核数放大。
/// 上界则由上游侧决定——同一时刻对 Kiro 发起过多首访会招致风控，而这批号往往同族
/// （同租户/同 IP 出口），并发过高等于主动暴露批量特征。
///
/// 4 在实测量级下已把 100 个号的导入从 100×RTT 压到约 25×RTT，再往上收益递减而风险上升。
/// 刻意**不**采用请求体里的 `concurrencyLimit`：那是调用方（kiro-accounting）语义下的
/// 账号并发配额，与本地导入的上游压力无关；若直接拿来当并发度，调用方传 300 就会
/// 瞬间对上游发起 300 路首访。
const IMPORT_MAX_IN_FLIGHT: usize = 4;

/// 同一账号「多开」的最大份数（`AddCredentialRequest::copies` 的上限）。
///
/// 多开 = 同一个账号导入多份，每份独立 `machineId` + 独立代理，让上游把它们看成
/// 「同一用户的多台设备」，以试探能否提高并发。
///
/// 取 16 的依据是**风险侧**而非收益侧：
/// - 每份都是独立凭据，`rpm_limit` 是**每凭据**的 → N 份使网关侧放行量变为 N 倍。
///   若上游实际按**账号**限流（而非按设备），多开只是把同一份配额切成 N 刀并更早撞上
///   惩罚窗口。上游按什么维度限流**目前无证据**，故上限要保守。
/// - 每份在池中都是一个独立候选，会参与选号排序与健康统计；份数过大会让单账号主导
///   整个池子的排序，稀释真实多账号的分流。
///
/// 设上限而不是不限：面板传个 500 会瞬间生成 500 条同账号凭据，且它们共用一份配额 ——
/// 那不是"更高并发"而是把调度器塞满。真要超过 16，应先有实验数据支撑再调这个常量。
const MAX_CREDENTIAL_COPIES: u32 = 16;

/// 把请求里的 `copies` 归一为实际份数。
///
/// 抽成独立函数**只为可测**：份数直接决定"这一次请求会建多少条凭据"，是外部可控输入，
/// 必须有硬上限。若不 clamp，一个 `{"copies": 999}` 的请求就会真建 999 条同账号凭据，
/// 而它们共用一份上游配额 —— 那不是更高并发，是把调度器塞满。
///
/// 归一规则：`None`（字段缺失）与 `0`（无意义值）都当 1（普通上号，行为与该字段
/// 不存在时完全一致）；超出上限则 clamp 到 [`MAX_CREDENTIAL_COPIES`] 而**不报错** ——
/// 报错会让"想多开但填大了"的请求整个失败，而 clamp 后仍是可用结果。
fn effective_copies(requested: Option<u32>) -> u32 {
    requested.unwrap_or(1).clamp(1, MAX_CREDENTIAL_COPIES)
}

/// 该号是否需要在 region 探测期间被临时禁用（探测窗口保护，见
/// [`AdminService::add_credential_with_intent`] 的 `will_probe` 块）。
///
/// ⚠️ **判据镜像 `token_manager::needs_api_region_probe`（逐字一致）**——那个函数是
/// 私有的，此处是唯一镜像点，**改判据必须两边同步**。分叉的最坏后果是退化为
/// 「启用态入池」的现状（窗口保护失效，但不会更糟），代价是那个已修过两次的
/// 死亡竞态复发，故镜像义务由下方守卫测试的字面量断言背书。
///
/// 抽成独立函数只为可测：行为测试无法覆盖这条路径（真实探测走上游往返，
/// 本仓铁律禁止测试依赖网络），窗口保护只能靠「判据矩阵 + 源码守卫」锁。
fn needs_probe_window_guard(cred: &crate::kiro::model::credentials::KiroCredentials) -> bool {
    !cred.is_custom_api_credential()
        && cred.is_api_key_credential()
        && cred.region.is_none()
        && cred.auth_region.is_none()
        && cred.api_region.is_none()
}

/// 一次「批量清理已禁用凭据」最多删多少条（[`AdminService::cleanup_disabled_credentials`]）。
///
/// 200 与 `MAX_BATCH_DELETE_IDS` 同值同理由：adminKey 明文存 localStorage 且全仓无 CSP，
/// 无上限的批量删除会放大 XSS 的破坏面。差别在于这个端点**不收 ids** —— 候选是服务端
/// 自己算的，所以上限是唯一的量级闸门，比批量删除那条更承重。
/// 超出部分留给下一次调用（`skipped` 里标 `over_limit`），不静默丢弃。
const MAX_CLEANUP_DISABLED_IDS: usize = 200;

/// 跳过原因：代挂号（`is_custom_api_credential()`）。
const CLEANUP_SKIP_CUSTOM_API: &str = "custom_api";
/// 跳过原因：禁用原因是代挂专属（`PassthroughFailed` / `PassthroughOverloaded`）。
const CLEANUP_SKIP_PASSTHROUGH_REASON: &str = "passthrough_disabled";
/// 跳过原因：禁用原因**可自愈**，号会自己回池（见 [`CLEANUP_SELF_HEALABLE_REASONS`]）。
const CLEANUP_SKIP_SELF_HEALABLE: &str = "self_healable";
/// 跳过原因：本次超出 [`MAX_CLEANUP_DISABLED_IDS`]，留给下一次调用。
const CLEANUP_SKIP_OVER_LIMIT: &str = "over_limit";
/// 跳过原因：候选算出来之后凭据已不在池里（并发删除的竞态）。
const CLEANUP_SKIP_NOT_IN_POOL: &str = "not_in_pool";

/// 「自愈会把号重新启用」的禁用原因，清理时必须排除。
///
/// # 为什么不删这几个：它们不是死号，是**几分钟后会自己复活**的健康号
///
/// `token_manager.rs` 的 `is_self_healable_reason` 把这三个原因定义为可自愈，全池无可用号时
/// 会 `disabled=false` + `clear_transient_counters()` 把它们**原地复活**。也就是说被这三个
/// 原因禁用的号，禁用态本身是**瞬时**的：
///
/// - `TooManyFailures`：连续失败达阈值，失败源多半是上游抖动；
/// - `SuspiciousActivityAuto`：403 账户级风控，历史事故已确证是**临时态**；
/// - `TooManyRefreshFailures`：走到阈值的典型成因是 token 端点抖了几十秒，凭据本身完好。
///
/// 清理走的是回收站（可恢复），但用户点「清理禁用号」时的心智模型是「删死号」——
/// 把一个正在自愈途中的号删走，表现为**号池莫名变小**，而面板上看不出是自己删的。
/// 方向上这是数据损失，所以判据必须与自愈白名单对齐。
///
/// # 为什么在这里抄一份而不是调那个函数
///
/// `is_self_healable_reason` 是 `token_manager` 的私有函数且吃枚举，而清理判据吃的是
/// 快照下发的**字符串**（Admin API 契约）。这里用 `DisabledReason::as_str()` 取字面量，
/// 保证两侧同源；漂移由 `self_healable_set_matches_token_manager_whitelist` 那条测试兜。
const CLEANUP_SELF_HEALABLE_REASONS: [DisabledReason; 3] = [
    DisabledReason::TooManyFailures,
    DisabledReason::SuspiciousActivityAuto,
    DisabledReason::TooManyRefreshFailures,
];

/// 代理池自动健康探测间隔（秒），固定 5 分钟。
///
/// 刻意不提供配置项：`model/config.rs` 不在改动范围，且探测节奏属运维内部策略，
/// 与 `balance_refresh_interval_secs`（面板可调）定位不同。改这里 + 重启即生效。
const SOCKS_HEALTH_CHECK_INTERVAL_SECS: u64 = 300;

/// 连续失败多少次后自动禁用该节点（对齐「连续失败 N 次」的调度语义）。
///
/// 判定在 `run_socks_health_round` 内按**连续**失败计数（成功即清零），
/// 达阈值把 `enabled` 置 false 并落盘——面板节点卡片可看到最近失败与原因。
const SOCKS_HEALTH_FAIL_THRESHOLD: u32 = 3;

/// 一条**已禁用**凭据是否该被清理。返回 `Some(跳过原因)` = 不清；`None` = 清。
///
/// # 抽成纯函数只为可测
///
/// 这是「删」与「不删」的唯一判据，误判的代价不对称：漏清一个死号只是列表里多一行，
/// 而误清一个代挂号 = 删掉用户自己配的第三方中转（回收站能捞回来，但配置得重来）。
///
/// # 四道排除各自独立，缺一不可
///
/// - `is_custom_api == None`：候选是先从快照算的，随后才去池里问"是不是代挂"。取不到 =
///   这中间被别人删掉了。不清（下一次调用自然不会再列它），但原因必须与代挂**区分开**：
///   报 `custom_api` 会让面板说"这号是代挂所以没删"，而真相是"它已经不在池里了"。
/// - `is_custom_api == Some(true)`：代挂号有**独立的 passthrough 路径**，它被禁用不代表
///   "号死了"，多半是中转站的 key/额度/地址问题，修好配置就能继续用 → 不该被当死号清掉。
/// - 禁用原因是代挂专属：`PassthroughFailed` / `PassthroughOverloaded` 这两个原因
///   **只可能**由代挂路径写入（见 `DisabledReason` 的文档）。它是第二道网 ——
///   万一某条号的 `auth_method`/`base_url` 因历史数据缺失而认不出是代挂，
///   禁用原因仍能把它捞出来。
/// - 禁用原因可自愈：见 [`CLEANUP_SELF_HEALABLE_REASONS`]。这条拦的是**健康号**，
///   与前三条拦"配置问题"性质不同，但方向一致：都不是死号。
///
/// # 顺序：不在池里 > 代挂 > 代挂原因 > 可自愈
///
/// 顺序决定的只是 `skipped` 里报哪个原因（四者都是"不清"），但那个原因是用户唯一能看到的
/// 解释，所以按**信息量**排：先报"号没了"（其余判据此时全是猜的），再报最确定的号类型，
/// 最后才报瞬时状态。
///
/// `reason` 取快照下发的字符串（`DisabledReason::as_str()` 的产物），而不是枚举：
/// 那是 Admin API 的既有契约，两侧同源。
fn cleanup_verdict(is_custom_api: Option<bool>, reason: Option<&str>) -> Option<&'static str> {
    match is_custom_api {
        None => return Some(CLEANUP_SKIP_NOT_IN_POOL),
        Some(true) => return Some(CLEANUP_SKIP_CUSTOM_API),
        Some(false) => {}
    }
    let passthrough_only = [
        DisabledReason::PassthroughFailed.as_str(),
        DisabledReason::PassthroughOverloaded.as_str(),
    ];
    if reason.is_some_and(|r| passthrough_only.contains(&r)) {
        return Some(CLEANUP_SKIP_PASSTHROUGH_REASON);
    }
    if reason.is_some_and(|r| {
        CLEANUP_SELF_HEALABLE_REASONS
            .iter()
            .any(|s| s.as_str() == r)
    }) {
        return Some(CLEANUP_SKIP_SELF_HEALABLE);
    }
    None
}

/// 多开是否适用于该凭据。返回 `Some(拒绝理由)` 表示不适用。
///
/// # 为什么必须拦：OAuth 号的分身**注定是死号**
///
/// `refreshToken` 在每次刷新时会被上游**轮换**（`token_manager.rs` 的
/// `new_credentials.refresh_token = Some(new_refresh_token)`）。多开是把同一份凭据
/// 复制 N 条，于是第 2..N 份带的是**同一个** refreshToken：
///
/// 1. 任意一份先刷新成功 → 上游把那个 refreshToken 作废、发一个新的给它；
/// 2. 其余 N-1 份手里的那个已被消费 → 刷新拿 `invalid_grant`；
/// 3. `report_refresh_token_invalid` 把它们逐个禁用（且现在会持久化禁用态）。
///
/// 结果是用户建了 N 个分身、看着入池成功，随后它们一个个变灰，
/// 而面板上的原因是 `refresh_token_invalid` —— 极易被误读成「号被封了」。
///
/// api_key 号（`ksk_`）没有 refreshToken，压根不走刷新路径
/// （`is_token_expired` 对它直接返回 false），复制多份是安全的 ——
/// 这也是多开这个功能当初被设计出来时唯一验证过的形态。
///
/// 抽成独立函数**只为可测**：`add_credential` 会打真实上游
/// （`get_usage_limits_for`），穿它的成功路径测不了。
fn multi_open_rejection_reason(cred: &KiroCredentials) -> Option<String> {
    if cred.is_api_key_credential() {
        return None;
    }
    Some(format!(
        "多开（copies > 1）只支持 API Key 凭据（ksk_）。当前凭据的 authMethod 是 \"{}\"，\
         它靠 refreshToken 刷新，而 refreshToken 每次刷新都会被上游轮换：\
         N 份带的是同一个 refreshToken，任一份刷新成功后其余份立刻拿 invalid_grant 被禁用。\
         要给这类账号分散出口 IP，请逐个号在卡片上单独配代理，而不是多开。",
        cred.auth_method.as_deref().unwrap_or("social")
    ))
}

/// 一次多开的「节点 → 份」分配计划（由 [`AdminService::resolve_node_plan`] 算出）。
///
/// 拆成结构体而不是返回裸元组，是因为**被剔除的 id 必须能一路带到响应文案里**：
/// 用户最容易踩空的正是「我选了节点却仍然直连」，而只返回可用节点的话，
/// 无效 id 就在这一层被静默吃掉了。
#[derive(Debug, Default)]
struct NodePlan {
    /// 按份序排好的代理三元组 `(url, username, password)`：`[0]` 给第 1 份、
    /// `[1]` 给第 2 份，以此类推。长度可小于份数（不够的份直连，刻意不复用）。
    assignments: Vec<(String, Option<String>, Option<String>)>,
    /// 被剔除的 `(node_id, 原因)`。原因是静态串（`不存在` / `已禁用` / `重复`），
    /// 供响应文案逐条点名。
    rejected: Vec<(u64, &'static str)>,
}

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
    /// 冷却原因稳定枚举码（rate_limited/suspicious/...，`CooldownReason::code()`）。
    /// 前端判定与 i18n 走此码，不再依赖中文文案。
    pub code: String,
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
///
/// `gate_active`：RPM 硬门在当前配置下是否真的参与调度（balanced 模式 + 池号数 >1，
/// 见 `MultiTokenManager::rpm_saturation_gate_active`）。硬门不生效时即便 rpm 已经
/// 超过 `rpm_limit`，这个阈值对调度也没有拦截力——继续说"建议分流"会让人以为网关在
/// 限制自己，真实原因通常是上游账户级限流（如 USER_REQUEST_RATE_EXCEEDED），应改口
/// 引导去加号/降并发，而不是"分流"（priority 模式/单号池根本没有分流对象）。
fn build_insight_text(
    id: u64,
    rpm: u32,
    rpm_limit: u32,
    saturated: bool,
    gate_active: bool,
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
        // 调用方保证 saturated=true 时 gate_active 也为 true（saturated 已在上游
        // 与 gate_active 做过 &&），这里的 gate_active 分支只是让语义自文档化，
        // 不依赖调用方的隐式约束。
        return if gate_active {
            format!("#{id} 近60s {rpm}/{rpm_limit} 已达软上限，建议分流")
        } else {
            format!("#{id} 近60s {rpm}/{rpm_limit} 超过软上限，但当前调度模式下无分流对象，疑似上游账户级限流，建议加号或降低并发")
        };
    }
    // 接近软上限（>=80%）也提示，便于提前分流；硬门不生效时同理改口，不建议"分流"。
    if rpm_limit > 0 && (rpm as u64) * 5 >= (rpm_limit as u64) * 4 {
        return if gate_active {
            format!("#{id} 近60s {rpm}/{rpm_limit} 接近软上限，建议分流")
        } else {
            format!("#{id} 近60s {rpm}/{rpm_limit} 接近软上限，但当前调度模式下无分流对象，建议关注上游限流")
        };
    }
    "畅通".to_string()
}

/// Admin 服务
///
/// 封装所有 Admin API 的业务逻辑
pub struct AdminService {
    token_manager: Arc<MultiTokenManager>,
    /// 余额缓存。**键是「账号」而不是「凭据」** —— 见 [`AdminService::balance_cache_key`]。
    ///
    /// # 为什么不是 `id`
    ///
    /// 同一个 `ksk_` key 的多份分身是**同一个上游账号**，配额也是同一份。按 `id` 键会让
    /// N 份分身各存一份余额，于是面板上同组各份显示的数字**互不相同**（谁最近刷过谁新），
    /// 而它们描述的本来是同一个账号。线上实测缓存键是 `620/623/622/624` 四份分身各一条。
    ///
    /// 改成按账号键之后：任一份刷新即同组全部同步，且上游 `getUsageLimits` 探测从
    /// N 次降到 1 次（那是 `web_portal` 往返，调多了会加重风控）。
    balance_cache: Mutex<HashMap<String, CachedBalance>>,
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
    /// 可复用代理节点表（「分身管理」页维护）。**不进请求热路径** ——
    /// 凭据自己的 `proxy_*` 才是绑定结果，本表只是候选池，故无需三层热重载镜像。
    socks_nodes: Mutex<Vec<SocksNode>>,
    /// 节点表落盘路径（None = 单凭据格式，纯内存态，与 trash 同款约定）。
    socks_nodes_path: Option<PathBuf>,
    /// 节点表是否可安全回写。启动时文件**存在但读不出来**时为 false（只读降级）：
    /// 此时内存是空表，回写就等于抹平原文件。见 `load_socks_nodes_from`。
    socks_nodes_writable: bool,
    /// 下一个要发放的节点 id。**只增不减**且随表落盘，故 id 永不复用
    /// （`max(现有 id)+1` 会在删掉最大 id 后把它重新发出去）。
    socks_next_id: std::sync::atomic::AtomicU64,
    /// 配置写锁（2026-08-14 新增）：串行化「load → 逐字段改 → save → reload」整段，
    /// 根除并发 `update_config` 的 lost update（两请求同时 load、各自 save，后写覆盖先写）。
    /// 只包 `update_config` / `import_config` 两个写路径，读路径（快照/导出）不走它。
    /// 函数体全程同步无 await（sync fn），`parking_lot::Mutex` 即可。
    config_write_lock: parking_lot::Mutex<()>,
    /// 余额耗尽**自动**禁用开关（2026-08-14 新增，默认开）。
    ///
    /// 读取点在后台温和余额刷新循环：刷到「刚取到的上游真值 remaining<=0」时自动
    /// 调 `report_quota_exhausted` 禁用（对齐 402 路径与手动 `disable_quota_exceeded`
    /// 语义）。与手动端点唯一的差别：自动只对**本次刚取到的新鲜真值**生效
    /// （cached_at=now，无需 24h 新鲜度门）。
    ///
    /// ⚠️ 仅存活于本服务内存：`model/config.rs` 不在本次改动范围，开关不进 config.json，
    /// 重启回默认值 true。经 `UpdateConfigRequest` / 配置快照接线到面板。
    auto_disable_quota_exceeded: std::sync::atomic::AtomicBool,
    /// 代理池**自动**健康调度任务句柄（独立受管任务槽，对齐 `balance_task` 模式）。
    /// 后台每 `SOCKS_HEALTH_CHECK_INTERVAL_SECS` 秒对池内启用节点做一轮健康探测，
    /// 连续失败达 `SOCKS_HEALTH_FAIL_THRESHOLD` 次自动禁用该节点。
    /// None = 未运行（开关关闭或尚未启动）。
    socks_health_task: Mutex<Option<JoinHandle<()>>>,
    /// 自动健康调度开关（服务内存态，默认开；重启回默认值 true）。
    /// 与 `auto_disable_quota_exceeded` 同款：不进 config.json，经配置快照/更新请求接线。
    socks_auto_health: std::sync::atomic::AtomicBool,
    /// 节点 id → 连续失败次数（**仅内存，重启清零**）。
    ///
    /// 为什么计数放 service 内存而不是节点表：节点表模型（`kiro/model/socks_node.rs`）
    /// 不在本次改动范围；且「连续失败」是运行期健康语义，跨重启清零恰好是期望行为
    /// （重启后重新积累）。探测成功或手动启用时清零。
    socks_fail_counts: Mutex<HashMap<u64, u32>>,
    /// 健康探测轮次的 round-robin 起点轮转（每轮 +1，取模落到池内启用节点上，
    /// 避免每轮都从第一个节点开始——长时间运行后首个节点永远先被探测）。
    socks_health_round: std::sync::atomic::AtomicU64,
}

/// 清洗粘贴进来的 Kiro API Key（`ksk_`）：截取 `ksk_` 起、去首尾空白与包裹引号/逗号。
///
/// 移植自 k2cc-proxy（`admin/service.rs:346`）。实测用户会把 `"key: ksk_xxx"` 整段贴进
/// 表单，不清洗会同时破坏 region 探测（坏 key）与去重（同一 key 不同前缀可重复导入）。
/// 空串归一为 `None`（与 k2cc 的 `.filter(!is_empty)` 同语义，交给下游「必须提供」报错）。
///
/// Kiro-Go `ksk_…|region`：恰好一段 `|` 且后缀命中 region 白名单时，返回 key 本体；
/// 后缀由 [`apply_ksk_region_suffix`] 写入已有的 `api_region`（请求已带则不覆盖）。
fn clean_ksk_api_key(raw: &str) -> Option<String> {
    peel_ksk_paste(raw).map(|(key, _region)| key)
}

/// 从粘贴噪声里取出 `ksk_` 本体，以及可选的 `|region` 后缀。
fn peel_ksk_paste(raw: &str) -> Option<(String, Option<String>)> {
    let s = raw.trim().trim_matches(|c| c == '"' || c == '\'' || c == ',');
    let (out, had_ksk) = match s.find("ksk_") {
        Some(i) => {
            // ⚠️ `s[i..]` 之后要再剥一次包裹引号/逗号：`"key: 'ksk_abc123'"` 经外层
            // trim_matches 后 s = `key: 'ksk_abc123'`，直接 `s[i..]` 会留下尾引号
            // `ksk_abc123'` → key 污染 → region 探测恒 403。
            (
                s[i..]
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'' || c == ',')
                    .to_string(),
                true,
            )
        }
        None => (s.to_string(), false),
    };
    if out.is_empty() {
        return None;
    }
    if had_ksk {
        let (key, region) = split_ksk_region_suffix(&out);
        Some((key.to_string(), region.map(str::to_string)))
    } else {
        Some((out, None))
    }
}

/// 仅当恰好一段 `|` 且后缀是已知 region 时才拆；否则整段当 key。
fn split_ksk_region_suffix(key: &str) -> (&str, Option<&str>) {
    let Some((left, right)) = key.split_once('|') else {
        return (key, None);
    };
    if left.contains('|') || right.contains('|') {
        return (key, None);
    }
    let left = left.trim();
    let right = right.trim();
    if left.is_empty() || right.is_empty() {
        return (key, None);
    }
    if KiroCredentials::is_supported_region(right) {
        (left, Some(right))
    } else {
        (key, None)
    }
}

fn ksk_region_suffix(raw: &str) -> Option<String> {
    peel_ksk_paste(raw).and_then(|(_, region)| region)
}

/// `ksk_xxx|eu-central-1` 在请求未带 `api_region` 时写入该字段；已有非空值不覆盖。
fn apply_ksk_region_suffix(req: &mut AddCredentialRequest) {
    let already = req
        .api_region
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some();
    if already {
        return;
    }
    if let Some(region) = req.kiro_api_key.as_deref().and_then(ksk_region_suffix) {
        req.api_region = Some(region);
    }
}

/// 写盘前把当前 config.json 轮换为备份（保留 3 份：`config.json.bak`、
/// `config.json.bak.1`、`config.json.bak.2`）。
///
/// 轮换方向（旧→新覆盖）：`.bak.1` → `.bak.2`，`.bak` → `.bak.1`，当前文件复制为 `.bak`。
/// 复制而非 rename：写盘是原子的（`fs_atomic::write_atomic`），但复制保留 config.json
/// 原位直到 save 完成，任何时刻磁盘上都有一份完整配置。备份失败只告警不阻断保存
/// （备份是保险不是依赖）。
fn rotate_config_backup(config_path: &Path) {
    let bak = config_path.with_extension("json.bak");
    let bak1 = config_path.with_extension("json.bak.1");
    let bak2 = config_path.with_extension("json.bak.2");
    // 最旧一份先滚出（rename 在 Unix 上覆盖目标；失败仅忽略，下次轮换自然补齐）
    if bak1.exists() {
        let _ = std::fs::rename(&bak1, &bak2);
    }
    if bak.exists() {
        let _ = std::fs::rename(&bak, &bak1);
    }
    if let Err(e) = std::fs::copy(config_path, &bak) {
        tracing::warn!("配置备份轮换失败（继续保存，不影响写盘）: {}", e);
    }
}

/// 错误码/提示词表合法 status 白名单（设计 §二 1，对齐 `exhausted_status` 先例）。
///
/// 504：`upstream_timeout` 默认 504（api_error）——管理员显式写回默认值时不被拒。
const ERROR_STATUS_WHITELIST: [u16; 10] = [400, 401, 403, 404, 413, 429, 500, 502, 503, 504];

/// 错误码/提示词表合法 type 白名单（设计 §二 2：Anthropic 官方 9 类减 billing_error）。
///
/// ⚠️ `billing_error` 与 `quota_exceeded_error` **不在**白名单（B2 决策，
/// docs/error-codes-client-behavior.md §1.4/§6.1-H6）：
/// - `billing_error`：Claude Code CLI 层 D 判定 `type==="billing_error"` → 429/402
///   都会重试约 7 次（1 分钟）——配出来即触发重试风暴；
/// - `quota_exceeded_error`：k2cc 姿势需要 402（402 不在 status 白名单），配置即
///   组合必拒，留在白名单是死选项；
/// - 待 QUOTA→402 改造（docs/quota-402-design.md，约 45 行）落地时，把 402 加进
///   status 白名单并同时放行这两个 type（quota_exceeded_error 是 402 姿势的
///   non_retryable type，见 client-behavior §6.1-H6）。
///
/// ⚠️ 现状文案里 `service_unavailable` / `internal_error` / `upstream_error`
/// 不在白名单（非官方类）：默认表保留现状值，但配置侧只能选白名单内 type。
const ERROR_TYPE_WHITELIST: [&str; 8] = [
    "invalid_request_error",
    "authentication_error",
    "permission_error",
    "not_found_error",
    "request_too_large",
    "rate_limit_error",
    "api_error",
    "overloaded_error",
];

/// 错误码表条目数上限 / message 长度上限（机制文档 §4.3，防配置膨胀与日志毒化）。
const ERROR_TABLE_MAX_ENTRIES: usize = 200;
const ERROR_MESSAGE_MAX_CHARS: usize = 500;

/// status × type 组合约束（设计 §二 3）。调用前 status/type 已各自过白名单。
///
/// 504 与 500/502 同族（`upstream_timeout` 默认 504→api_error，H5：全客户端对
/// 5xx 必重试）；billing_error 已从白名单移除（重试风暴，见白名单注释）。
fn error_type_compatible_with_status(status: u16, ty: &str) -> bool {
    match status {
        401 => ty == "authentication_error",
        403 => ty == "permission_error",
        404 => ty == "not_found_error",
        429 => ty == "rate_limit_error" || ty == "overloaded_error",
        400 | 413 => ty == "invalid_request_error" || ty == "request_too_large",
        500 | 502 | 504 => ty == "api_error",
        503 => ty == "api_error" || ty == "overloaded_error",
        // 白名单外的 status 已被规则 1 拒绝，走不到这里。
        _ => true,
    }
}

/// message 决策词黑名单（设计 §二 5）：命中即拒——这些词会改变客户端决策
/// （Claude Code CLI 层重试 / 凭据处置 / type 分派），配置不允许出现。
///
/// 大小写不敏感（客户端判据是小写匹配，防绕过）。`quota`+`exhausted` 组合
/// **无条件拒绝**（B2：billing_error 已从白名单移除，旧豁免条件永远不可达，
/// 且 Claude Code D 判定/opencode 模式匹配都拿它当重试决策输入）。
fn error_message_decision_word_hit(message: &str) -> Option<&'static str> {
    let m = message.to_lowercase();
    if m.contains("credit balance is too low") {
        return Some("message 含决策词 `credit balance is too low`（触发 Claude Code CLI 层重试，禁止配置）");
    }
    if m.contains("organization has been disabled") {
        return Some("message 含决策词 `organization has been disabled`（触发客户端凭据处置，禁止配置）");
    }
    if m.contains("overloaded_error") {
        return Some("message 含 `overloaded_error` 字样（客户端按 type 分派行为的哨兵串，禁止混入文案）");
    }
    if m.contains("quota") && m.contains("exhausted") {
        return Some("message 同时含 `quota` 与 `exhausted`（客户端重试决策词，禁止配置）");
    }
    if m.contains("billing") {
        return Some("message 含 `billing` 字样（防 Claude Code CLI 层 7 次重试误触发，禁止配置）");
    }
    None
}

/// key 命名规范：小写字母开头，只允许小写字母/数字/下划线（防日志/前端毒化）。
fn is_valid_error_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// 错误码/提示词表校验：任一错误 → `Err(第一个错误)`，**整表不生效**（保持旧表）。
///
/// 规则（设计 §二 锁死项 + 机制文档 §4.3，逐条落实）：
/// 1. status 白名单；2. type 白名单；3. status×type 组合约束（取「配置 or 默认表」
///    的**最终渲染值**判定，防单字段绕过）；4. retryAfterSecs 0-3600；
/// 5. message 决策词黑名单（拒绝）；6. 承重字符串只告警不硬拒
///    （`check_load_bearing_message`）；7. 表条目数 ≤200、key 命名 snake_case、
///    message ≤500 字符。
///
/// `pub(crate)`：启动路径（main.rs 播种 set_error_messages 前）与
/// `update_config`（字段级 merge 前校验，失败 400 回显）、`import_config`
/// （整份导入校验，失败拒绝导入）共用同一校验。
pub(crate) fn validate_error_messages(
    entries: &HashMap<String, crate::model::error_messages::ErrorMessageOverride>,
) -> Result<(), String> {
    if entries.len() > ERROR_TABLE_MAX_ENTRIES {
        return Err(format!(
            "errorMessages 表条目数超过上限 {}",
            ERROR_TABLE_MAX_ENTRIES
        ));
    }
    for (key, entry) in entries {
        if !is_valid_error_key(key) {
            return Err(format!(
                "errorMessages[{key}]: key 命名不合法（只允许小写字母/数字/下划线）"
            ));
        }
        if let Some(ty) = entry.r#type.as_deref() {
            if !ERROR_TYPE_WHITELIST.contains(&ty) {
                return Err(format!(
                    "errorMessages[{key}].type 只允许 {:?}，收到 {ty}",
                    ERROR_TYPE_WHITELIST
                ));
            }
        }
        if let Some(status) = entry.status {
            if !ERROR_STATUS_WHITELIST.contains(&status) {
                return Err(format!(
                    "errorMessages[{key}].status 只允许 {:?}，收到 {status}",
                    ERROR_STATUS_WHITELIST
                ));
            }
        }
        // status×type 组合约束（设计 §二 3）：取「配置 or 默认表」的**最终渲染值**
        // 再查组合矩阵——只配 status（或只配 type）时另一半落默认，仍必须组合合法，
        // 堵住单字段绕过（如 {status:401} 单独配置 → 渲染 401+默认 rate_limit_error
        // → 拒；{type:authentication_error} 单独配置在默认 429 的 key 上 → 拒）。
        //
        // 默认表里保留的现状非官方值（service_unavailable / internal_error /
        // upstream_error、流式 in-band 200）不在白名单：渲染值任一不在白名单时
        // 跳过组合检查（组合约束只约束官方值域，不拦「只改 message」的合法姿势，
        // 设计 §一「只改 message 时 status/type 不填」）。
        // key 不在默认表（管理员自定义，无渲染基线）→ 退化为双方都显式才检查。
        let default_pair = default_status_type_for(key);
        let combo_bad = match (entry.status, entry.r#type.as_deref(), default_pair) {
            (Some(s), Some(t), _) => !error_type_compatible_with_status(s, t),
            (Some(s), None, Some((_, dt))) => {
                ERROR_STATUS_WHITELIST.contains(&s)
                    && ERROR_TYPE_WHITELIST.contains(&dt)
                    && !error_type_compatible_with_status(s, dt)
            }
            (None, Some(t), Some((ds, _))) => {
                ERROR_STATUS_WHITELIST.contains(&ds)
                    && ERROR_TYPE_WHITELIST.contains(&t)
                    && !error_type_compatible_with_status(ds, t)
            }
            _ => false,
        };
        if combo_bad {
            return Err(format!(
                "errorMessages[{key}]: status 与 type 组合不合法（渲染值不满足 \
                 429→rate_limit_error/overloaded_error；401→authentication_error；403→permission_error；\
                 404→not_found_error；400/413→invalid_request_error/request_too_large；\
                 500/502/504→api_error；503→api_error/overloaded_error）"
            ));
        }
        if let Some(ra) = entry.retry_after_secs {
            if ra > 3600 {
                return Err(format!(
                    "errorMessages[{key}].retryAfterSecs 必须在 0-3600 之间，收到 {ra}"
                ));
            }
        }
        if let Some(msg) = entry.message.as_deref() {
            if msg.chars().count() > ERROR_MESSAGE_MAX_CHARS {
                return Err(format!(
                    "errorMessages[{key}].message 超过 {ERROR_MESSAGE_MAX_CHARS} 字符上限"
                ));
            }
            if let Some(reason) = error_message_decision_word_hit(msg) {
                return Err(format!("errorMessages[{key}]: {reason}"));
            }
            // 承重字符串：提示不硬拒（外挂/客户端判据依赖，改了会静默失效）。
            if let Some(load_bearing) =
                crate::model::error_messages::check_load_bearing_message(msg)
            {
                tracing::warn!(
                    "errorMessages[{key}].message 含承重字符串: {load_bearing}（建议保留现状，见 docs/error-codes-inventory.md §3.1）"
                );
            }
        }
    }
    Ok(())
}

/// 默认表的 (status, type) 渲染基线：key 不在默认表（管理员自定义）→ `None`。
///
/// 默认表可能被并行任务重写（key 集变化）——始终以 `default_error_messages()`
/// 当前实现为准，不硬编码 key。
fn default_status_type_for(key: &str) -> Option<(u16, &'static str)> {
    crate::model::error_messages::default_error_messages()
        .iter()
        .find(|(k, ..)| *k == key)
        .map(|(_, s, t, ..)| (*s, *t))
}

/// 递归对比两份配置 JSON，返回「发生了变更的字段路径」列表。
///
/// 只记**字段名**（如 `proxyUrl` / `inboundRpmMin`），绝不记录字段值 ——
/// 敏感字段（apiKey/adminApiKey/proxyPassword 等）的值因此天然不会进审计日志。
fn diff_json_fields(old: &serde_json::Value, new: &serde_json::Value) -> Vec<String> {
    fn walk(path: &str, old: &serde_json::Value, new: &serde_json::Value, out: &mut Vec<String>) {
        match (old, new) {
            (serde_json::Value::Object(lo), serde_json::Value::Object(no)) => {
                let mut keys: Vec<&String> = lo.keys().collect();
                for k in no.keys() {
                    if !lo.contains_key(k) {
                        keys.push(k);
                    }
                }
                keys.sort();
                for k in keys {
                    let p = if path.is_empty() {
                        k.clone()
                    } else {
                        format!("{path}.{k}")
                    };
                    match (lo.get(k), no.get(k)) {
                        // 双方都有 → 递归到叶子；有一方缺失（新增/删除键）→ 记路径
                        (Some(a), Some(b)) => walk(&p, a, b, out),
                        _ => out.push(p),
                    }
                }
            }
            (a, b) if a == b => {}
            _ => out.push(path.to_string()),
        }
    }
    let mut out = Vec::new();
    walk("", old, new, &mut out);
    out
}

/// 将单个凭据映射为 KAM 1.8.3+ 平铺格式的账号结构
///
/// 移植自参考仓（kiro-rs-tool）`credential_to_kam_account`，按本仓数据结构适配：
/// - 无 refreshToken 的号（api_key / custom_api）KAM 无对应字段，整条跳过；
/// - 空字符串字段过滤为 None，保持导出 JSON 整洁；
/// - 本仓凭据结构没有 provider / start_url 字段 → 恒为 None（KAM 侧可再补）；
/// - idp 复用本仓 `KiroCredentials::effective_idp` 的既有推断（social → Google）。
fn credential_to_kam_account(cred: KiroCredentials) -> Option<KamExportAccount> {
    let refresh_token = cred
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)?;

    fn non_empty(value: Option<String>) -> Option<String> {
        value
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    // 派生值先算齐再移动字段（partial move 后不能再整结构借用 effective_idp）。
    let status = if cred.disabled {
        Some("disabled".to_string())
    } else {
        Some("active".to_string())
    };
    let idp = non_empty(Some(cred.effective_idp().to_string()));

    Some(KamExportAccount {
        email: non_empty(cred.email),
        nickname: None,
        idp,
        provider: None,
        status,
        auth_method: non_empty(cred.auth_method.clone()),
        // MINOR-2（2026-08-14 审查修正）：region 对齐调度真相源的口径 ——
        // profileArn 推导优先（SSO-OIDC 认证区与对话/余额区物理不同，导错区 =
        // KAM 导入后错位），再落 region → auth_region → api_region 回退链。
        region: non_empty(
            cred
                .profile_arn
                .as_deref()
                .and_then(KiroCredentials::region_from_profile_arn)
                .map(|s| s.to_string()),
        )
        .or_else(|| non_empty(cred.region.clone()))
        .or_else(|| non_empty(cred.auth_region.clone()))
        .or_else(|| non_empty(cred.api_region.clone())),
        start_url: None,
        client_id: non_empty(cred.client_id),
        client_secret: non_empty(cred.client_secret),
        refresh_token: Some(refresh_token),
        access_token: non_empty(cred.access_token),
        profile_arn: non_empty(cred.profile_arn),
        expires_at: non_empty(cred.expires_at),
        machine_id: non_empty(cred.machine_id),
    })
}

impl AdminService {
    pub fn new(
        token_manager: Arc<MultiTokenManager>,
        known_endpoints: impl IntoIterator<Item = String>,
    ) -> Self {
        let cache_path = token_manager
            .cache_dir()
            .map(|d| d.join("kiro_balance_cache.json"));

        // 传 token_manager 是为了把**旧格式**（按凭据 id 键）迁移成新格式（按账号键）。
        // 不迁移不会崩，但会让升级后 api_key 号的缓存全部失效、面板集体转圈打
        // getUsageLimits —— 那是上游探测。见 `load_balance_cache_from` 的迁移说明。
        let balance_cache = Self::load_balance_cache_from(&cache_path, &token_manager);

        let socks_nodes_path = token_manager
            .cache_dir()
            .map(|d| d.join("socks_nodes.json"));
        let (socks_nodes, socks_next_id, socks_nodes_writable) =
            Self::load_socks_nodes_from(&socks_nodes_path, &token_manager);

        Self {
            social_login: SocialLoginManager::new(token_manager.clone()),
            idc_login: IdcLoginManager::new(token_manager.clone()),
            external_idp_login: ExternalIdpLoginManager::new(token_manager.clone()),
            token_manager,
            balance_cache: Mutex::new(balance_cache),
            cache_path,
            known_endpoints: known_endpoints.into_iter().collect(),
            balance_task: Mutex::new(None),
            socks_nodes: Mutex::new(socks_nodes),
            socks_nodes_path,
            socks_nodes_writable,
            socks_next_id: std::sync::atomic::AtomicU64::new(socks_next_id),
            config_write_lock: parking_lot::Mutex::new(()),
            auto_disable_quota_exceeded: std::sync::atomic::AtomicBool::new(true),
            socks_health_task: Mutex::new(None),
            socks_auto_health: std::sync::atomic::AtomicBool::new(true),
            socks_fail_counts: Mutex::new(HashMap::new()),
            socks_health_round: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 模型单价表快照（成本估算用；空表 = 不估算成本）。
    ///
    /// 每次现读配置快照（`ArcSwap load_full`），改 config.json 立即生效。
    /// 供用量端点查询时传给 [`crate::usage::usage_stats::UsageStats::by_model`]。
    pub fn model_pricing(
        &self,
    ) -> std::collections::HashMap<String, crate::model::config::ModelPrice> {
        self.token_manager.config().model_pricing.clone()
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

    /// SSO Token 导入：粘贴 AWS portal 的 Bearer Token，服务端静默走完整设备授权
    /// 流程换取标准 IdC 凭据入池（免浏览器授权的人工步骤）。
    ///
    /// 流程与幂等语义见 [`crate::kiro::auth::sso_token`]。
    /// - `region` 缺省 us-east-1，且必须过 Kiro region 白名单（它直接拼进
    ///   `oidc.{region}.amazonaws.com` 出站 host，污染值会把 device session /
    ///   clientSecret 引到攻击者主机）。
    /// - 同一邮箱的 idc 号已在池中 → `DuplicateCredential`（SSO 每次导入都换出
    ///   不同的 refreshToken，哈希判重抓不住重复，email 是账号级稳定指纹）。
    pub async fn import_sso_token(
        &self,
        token: String,
        region: Option<String>,
        priority: u32,
        proxy_url: Option<String>,
    ) -> Result<ImportSsoTokenResult, AdminServiceError> {
        use crate::kiro::auth::sso_token::{
            build_idc_credential_from_sso, exchange_sso_token, find_duplicate_idc_email,
        };

        let region = region
            .map(|r| r.trim().to_lowercase())
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| "us-east-1".to_string());
        // 安全:region 拼进 oidc.{region}.amazonaws.com host(见 sso_token 模块文档)。
        if !KiroCredentials::is_supported_region(&region) {
            return Err(AdminServiceError::InvalidCredential(format!(
                "非法 region: {region}（不在支持的 AWS region 白名单内）"
            )));
        }

        let config = self.token_manager.config();
        // 组装代理:显式填的拆账密后仅它持久化到新凭据;global 回落不持久化(与上号同口径)。
        let (proxy, custom_proxy) = {
            let global = config.proxy_url.as_ref().map(|url| {
                let mut p = crate::http_client::ProxyConfig::new(url);
                if let (Some(u), Some(pw)) = (&config.proxy_username, &config.proxy_password) {
                    p = p.with_auth(u, pw);
                }
                p
            });
            let custom = proxy_url.filter(|u| !u.trim().is_empty()).map(|u| {
                let (clean, user, pass) = crate::http_client::split_proxy_credentials(&u);
                let mut p = crate::http_client::ProxyConfig::new(clean);
                if let (Some(user), Some(pass)) = (user, pass) {
                    p = p.with_auth(user, pass);
                }
                p
            });
            (custom.clone().or(global), custom)
        };

        // 7 步纯 HTTP 流程:验证 token → 模拟授权 → 换正式 IdC 凭据。
        let exchange = exchange_sso_token(&token, &region, &config, proxy.as_ref())
            .await
            .map_err(|e| AdminServiceError::InvalidCredential(e.to_string()))?;

        // 幂等判重:同邮箱 idc 号已在池 → 拒绝重复导入(email 大小写不敏感)。
        if let Some(email) = exchange.email.as_deref() {
            let pool: Vec<(Option<String>, Option<String>)> = {
                let snap = self.token_manager.snapshot();
                snap.entries
                    .iter()
                    .map(|e| (e.auth_method.clone(), e.email.clone()))
                    .collect()
            };
            if find_duplicate_idc_email(&pool, email) {
                return Err(AdminServiceError::DuplicateCredential(format!(
                    "该邮箱（{}）的 SSO 账号已在池中——如 Token 已过期请先删除旧号再导入",
                    email
                )));
            }
        }

        let new_cred = build_idc_credential_from_sso(&exchange, &region, priority, custom_proxy.as_ref());
        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| self.classify_add_error(e))?;

        // 顺带拉一次订阅等级（失败不阻断，仅告警——与上号路径同口径）。
        if let Err(e) = self.token_manager.get_usage_limits_for(credential_id).await {
            tracing::warn!("SSO Token 导入后获取订阅等级失败: {}", e);
        }

        // 新号自动初始化(异步):刷 token + 解析 profileArn(idc 号必需,同 IdC 上号)。
        self.token_manager.spawn_initial_refresh(credential_id);

        tracing::info!(
            "SSO Token 导入成功,新凭据 #{} (region={})",
            credential_id,
            region
        );
        Ok(ImportSsoTokenResult {
            credential_id,
            email: exchange.email,
        })
    }

    /// 获取所有凭据状态
    pub fn get_all_credentials(&self) -> CredentialsStatusResponse {
        // 端点的默认回退已在 snapshot 内解析完成（entry.effective_endpoint），此处不再重复。
        let snapshot = self.token_manager.snapshot();

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
                    model_mapping_exempt: entry.model_mapping_exempt,
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
                    // ⭐ 此前**漏映射**：`CredentialResponse` 与 `CredentialEntrySnapshot` 两侧都有
                    // `disabled_at` 字段，但这里没接上 → 面板拿到的恒为 null，
                    // 「这号什么时候坏的」这个信息在最后一跳丢失（smoke 实测发现）。
                    // 与上面的 disabled_reason 是一对，必须同源同步。
                    disabled_at: entry.disabled_at,
                    // 展示**实际生效**的端点（含 ksk_ 自动路由结果），并单独标出是否被人工固定。
                    // 此前这里只做 `endpoint.unwrap_or(default)`，ksk_ 号会显示成 "ide"
                    // 而请求其实走 cli —— 面板与真实行为不一致，排障时极易误判。
                    endpoint_pinned: entry.endpoint.is_some(),
                    endpoint: entry.effective_endpoint,
                    // 同款「实际生效值 + 是否被固定」二元组，理由见字段文档：
                    // ksk_ 按区授权，打错区恒 403，而面板此前完全看不到 region。
                    effective_region: Some(entry.effective_region),
                    region_pinned: entry.region_pinned,
                    inflight: entry.inflight,
                    rpm: entry.rpm,
                    name: entry.name,
                    clone_group: entry.clone_group,
                    clone_seq: entry.clone_seq,
                    tag: entry.tag,
                    cooling_down: cd.is_some(),
                    cooldown_remaining_ms: cd.map(|c| c.remaining_ms),
                    cooldown_reason: cd.map(|c| c.reason.description().to_string()),
                    cooldown_code: cd.map(|c| c.reason.code().to_string()),
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

    /// 导出凭据为 KAM 兼容 JSON（KAM 1.8.3+ 平铺格式）
    ///
    /// ⚠️ **敏感操作**：返回的 JSON 含明文 refreshToken / accessToken / clientSecret，
    /// 出站后即不可控，调用方（handler）必须保证只落到用户浏览器下载、不进日志/存储。
    ///
    /// 解密语义：at-rest 加密在启动加载期由 `CredentialsConfig::load` →
    /// `maybe_decrypt_to_string` 统一解密，内存中的凭据即明文；这里经
    /// `token_manager.export_credential` 直接复用解密结果，**不做二次加解密**。
    ///
    /// `id_filter` 为 None 时导出全部凭据；为 Some 时仅导出集合内的 ID。
    /// 结果按 priority 升序排序，与 UI 列表一致。
    pub fn export_kam_credentials(&self, id_filter: Option<&HashSet<u64>>) -> KamExportResponse {
        let snapshot = self.token_manager.snapshot();
        let mut credentials: Vec<KiroCredentials> = snapshot
            .entries
            .iter()
            .filter(|e| id_filter.map_or(true, |f| f.contains(&e.id)))
            .filter_map(|e| self.token_manager.export_credential(e.id))
            .collect();
        credentials.sort_by_key(|c| c.priority);

        let accounts = credentials
            .into_iter()
            .filter_map(credential_to_kam_account)
            .collect();

        KamExportResponse {
            version: "1.8.3".to_string(),
            exported_at: Utc::now().to_rfc3339(),
            accounts,
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
                let raw_saturated = e.rpm >= eff_limit;
                // 只在硬门真正生效时才把 raw_saturated 报告为 rpmSaturated：
                // effective_saturation_limit 恒返回一个数字（哪怕未配置也有 30×headroom 兜底），
                // 但这个数字只在 balanced + 池>1 时才真的影响选号（见 rpm_saturation_gate_active
                // 的推导）。priority 模式 / 单号池下报 true 会让前端火焰图标+文案指向一个从未
                // 拦过任何请求的门槛，误导排障（"网关把我限制在 25"实为上游账户级限流）。
                let gate_active = self.token_manager.rpm_saturation_gate_active();
                let saturated = raw_saturated && gate_active;
                // recent429：速率限制类冷却的连续触发计数近似"近期 429 次数"（零上游）；
                // 非速率限制冷却或无冷却则为 0。
                let recent429 = cd
                    .filter(|c| {
                        c.reason == crate::kiro::cooldown::CooldownReason::RateLimitExceeded
                    })
                    .map(|c| c.trigger_count)
                    .unwrap_or(0);
                let insight_text = build_insight_text(
                    e.id, e.rpm, eff_limit, saturated, gate_active, e.disabled, cd,
                );
                RateLimitInsight {
                    id: e.id,
                    rpm: e.rpm,
                    rpm_limit: eff_limit,
                    rpm_saturated: saturated && !e.disabled,
                    inflight: e.inflight,
                    disabled: e.disabled,
                    cooldown: cd.map(|c| CooldownDetail {
                        reason: c.reason.description().to_string(),
                        code: c.reason.code().to_string(),
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

    /// 设置凭据级端点（None/空 = 清除显式固定，回到自动路由）。
    ///
    /// 端点名先按 Admin 层已知端点列表校验，给出「可用: a, b」的友好提示；
    /// token_manager 侧还有一道 registry 校验兜底（直打 API 也进不去非法值）。
    /// 设置凭据的 `apiRegion`（空=清除，回退全局 `config.region`）。
    ///
    /// 补的是一个真实运维缺口：`ksk_` 按 region 授权、打错区恒 403 且永不自愈，
    /// 而此前全仓没有任何修改 `api_region` 的入口（`/regions` 与 `/switch-region`
    /// 都是 ARN 门控，只对 external_idp 有意义）⇒ api_key 号 region 错了只能删号重建。
    /// 实测 2026-08-05 02:42：4 个分身因缺 region 被打成 `TooManyFailures`，
    /// 运维手上没有"补 region 再启用"的手段。
    pub fn set_credential_api_region(
        &self,
        id: u64,
        api_region: Option<String>,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_credential_api_region(id, api_region)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 手动更新 OAuth 号的 refreshToken（token 轮换后的运维入口，2026-08-11）。
    ///
    /// ⚠️ 凭证纪律：refreshToken 是敏感值，本函数不记录、不回显、不进错误消息。
    /// 成功后下一次调用强制走刷新链路（access_token 缓存已清）。
    pub fn update_refresh_token(&self, id: u64, refresh_token: String) -> Result<(), AdminServiceError> {
        // 🔴 凭据类型闸（对抗审查 MINOR-6，2026-08-15）：refreshToken 是 OAuth 类
        // 凭据的专属字段，api_key 凭据没有该概念（直接用 kiro_api_key 作 Bearer），
        // 更新它是误操作，直接 400。判据问**真凭据**（export_credential）而非快照
        // 字段 —— 快照的 auth_method 对代挂/历史号不完整（与 cleanup 同款教训，
        // service.rs:1192）。
        let cred = self
            .token_manager
            .export_credential(id)
            .ok_or(AdminServiceError::NotFound { id })?;
        if cred.is_api_key_credential() {
            return Err(AdminServiceError::InvalidCredential(
                "仅 OAuth 凭据支持更新 refreshToken".to_string(),
            ));
        }
        // 🔴 服务端 trim（对抗审查 MINOR-7，2026-08-15）：从聊天工具粘贴的 token
        // 常带首尾换行/空白，不 trim 会把脏值写进 refresh_token_hash 与落库，下次
        // 刷新必然 invalid_grant。entry 处统一 trim 后再走 validate + 哈希 + 落库。
        let refresh_token = refresh_token.trim().to_string();
        if refresh_token.is_empty() {
            return Err(AdminServiceError::InvalidCredential(
                "refreshToken 不能为空".to_string(),
            ));
        }
        // 截断检测：与 add_credential 同款（长度 <100 或含 "..." 即拒）。从聊天工具
        // 粘贴时容易被截断，静默接受会让下一次刷新必然失败（invalid_grant）。
        let mut candidate = KiroCredentials::default();
        candidate.refresh_token = Some(refresh_token.clone());
        if let Err(e) = validate_refresh_token(&candidate) {
            return Err(AdminServiceError::InvalidCredential(e.to_string()));
        }
        // 跨凭据重复检测：对齐 add_credential 的 refreshToken 去重（sha256 哈希比较，
        // 见 snapshot 的 refresh_token_hash）。必须排除自身 —— 用当前值重提交是合法
        // no-op，不该被当成「与其他凭据重复」。
        let new_hash = sha256_hex(&refresh_token);
        let duplicate = self
            .token_manager
            .snapshot()
            .entries
            .iter()
            .any(|e| e.id != id && e.refresh_token_hash.as_deref() == Some(new_hash.as_str()));
        if duplicate {
            return Err(AdminServiceError::DuplicateCredential(
                "refreshToken 与其他凭据重复".to_string(),
            ));
        }
        self.token_manager
            .update_refresh_token(id, refresh_token)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 手动重探指定凭据的可用 region（命中则写死 `api_region` 并落盘）。
    ///
    /// 复用上号路径的 `probe_and_persist_api_region`（同一判据、同一落盘收口）。
    /// 与 `add_credential` 的处置差异在**失败侧**：
    ///
    /// - 上号时探不出结论要禁用（新号未接流量，启用即恒 403 被自动打死）；
    /// - 这里面对的是**已在服役**的存量号 —— 启动回填修过的误禁形态正是「服役号被
    ///   禁用会把一个靠 `config.region` 恰好对的好号打掉」，故失败一律**只报错、
    ///   绝不触碰禁用态**（禁用处置只适用于上号那一刻，见 `probe_and_persist_api_region`
    ///   与 `mark_region_probe_failed` 的文档）。
    ///
    /// 前端契约（admin-ui `reprobeRegion`）：成功 `{ region }`；失败标准
    /// `AdminErrorResponse`（`error.message` 带归因提示）。
    pub async fn reprobe_api_region(
        &self,
        id: u64,
    ) -> Result<ReprobeRegionResponse, AdminServiceError> {
        use crate::kiro::region_probe::ProbeOutcome;
        if self.token_manager.export_credential(id).is_none() {
            return Err(AdminServiceError::NotFound { id });
        }
        match self.token_manager.probe_and_persist_api_region(id).await {
            ProbeOutcome::Usable(region) => Ok(ReprobeRegionResponse {
                region: Some(region.clone()),
                message: format!("凭据 #{} 已探测并写死 region {}", id, region),
            }),
            // Skipped：号已带 region 字段 / 不是 api_key 号（OAuth、代挂）/ 取 token 瞬时失败。
            // 返回当前 api_region；号上没有任何 region 字段就明说「无需探测」——
            // 这不是失败（探测压根没发生或没有探测资格），不能走错误路径。
            ProbeOutcome::Skipped => {
                let region = self
                    .token_manager
                    .export_credential(id)
                    .and_then(|c| c.api_region);
                Ok(ReprobeRegionResponse {
                    region: region.clone(),
                    message: match region {
                        Some(r) => format!("凭据 #{} 无需探测（已带 region {}）", id, r),
                        None => format!("凭据 #{} 无需探测（已带 region 或非 api_key 号）", id),
                    },
                })
            }
            ProbeOutcome::NoUsableRegion => Err(AdminServiceError::InvalidCredential(format!(
                "凭据 #{} 候选 region 全部不可用（403/无结论），探不出可用区；\
                 号保持原状态未被禁用，请人工确认 region 授权范围",
                id
            ))),
            ProbeOutcome::TokenDead => Err(AdminServiceError::InvalidCredential(format!(
                "凭据 #{} token 已失效（401），探不出可用 region；\
                 号保持原状态未被禁用，需重新获取 token",
                id
            ))),
            ProbeOutcome::AccountThrottled => Err(AdminServiceError::UpstreamError(format!(
                "凭据 #{} 账户级风控挡住探测，与 region 无关；\
                 号保持原状态未被禁用，等风控过去后再重探",
                id
            ))),
        }
    }

    /// 一键禁用所有「余额已超额」的启用号（`remaining <= 0`）。
    ///
    /// # 数据源
    ///
    /// 余额缓存（`balance_cache`，按账号键共享）—— 与批量缓存余额端点同源，**零上游**。
    /// 因此前端点此按钮前应先触发余额刷新（或等后台 30 分钟温和刷新），否则候选可能为空。
    ///
    /// # 排除项
    ///
    /// - 已禁用的号：不是候选（幂等，重复点不重复禁）。
    /// - 代挂号（`custom_api`）：不适用 Kiro 配额体系，额度是中转站自己的，绝不代禁。
    /// - 缓存未命中 / 余额大于 0：不是候选。
    ///
    /// # 禁用机制（为什么用 `report_quota_exhausted` 而不是 `set_disabled`）
    ///
    /// `set_disabled` 在 token_manager 侧把原因写死成 `Manual`，而本端点要求
    /// `DisabledReason::QuotaExceeded`（面板要能看出「额度用尽」而不是「手动禁用」）。
    /// 全仓写该原因的既有收口只有 `report_quota_exhausted`（运行期 402 路径），
    /// 其附带动作对本端点同样成立：failure_count 拉到阈值（面板直观显示不可用）、
    /// 立即落盘（重启后不回池）、清亲和绑定、若禁的是当前号则切到下一个可用号。
    ///
    /// # 部分失败语义
    ///
    /// 与批量删除同款：单号失败不炸整批，逐条标 ok/error（`results`）。
    pub fn disable_quota_exceeded(&self) -> DisableQuotaExceededResponse {
        let snapshot = self.token_manager.snapshot();
        let mut candidates: Vec<u64> = Vec::new();
        let mut stale_results: Vec<BatchDeleteItemResult> = Vec::new();
        {
            let cache = self.balance_cache.lock();
            for entry in snapshot.entries.iter() {
                if entry.disabled {
                    continue;
                }
                // 代挂判据必须问**真凭据**而不是快照字段：快照的 auth_method 对
                // 「custom_api 且带 kiroApiKey」的号会显示成 `api_key`（见 snapshot 的
                // is_api_key_credential 分支）—— 与 cleanup 同款教训。
                let Some(cred) = self.token_manager.export_credential(entry.id) else {
                    continue; // 已被删的竞态：本轮跳过，不误判
                };
                if cred.is_custom_api_credential() {
                    continue;
                }
                // ⭐ 缓存新鲜度门（2026-08-11 对抗审查 MAJOR）：余额缓存最长可存活 7 天
                // （BALANCE_CACHE_DISPLAY_MAX_AGE_SECS），且后台温和刷新**跳过已禁用号**——
                // 「额度耗尽 → 被禁 → 月度重置 → 上游已恢复」的号，remaining=0 的缓存条目
                // 可能无限期留存。不检查 cached_at 会把**当前额度正常、正在服役**的号
                // 以 QuotaExceeded 禁掉。24h 窗口：月度重置后正常号至多一天内被旧缓存误判
                // 为超额——但被禁后会被跳过刷新，永久卡死。故窗口必须足够新（≤24h）
                // 且跳过时逐条可见（下方 error 标注），而不是静默漏掉。
                const BALANCE_CACHE_FRESH_SECS: f64 = 24.0 * 3600.0;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as f64)
                    .unwrap_or(0.0);
                let key = self.balance_cache_key(entry.id);
                match cache.get(&key) {
                    Some(c) if c.data.remaining <= 0.0 && now - c.cached_at <= BALANCE_CACHE_FRESH_SECS => {
                        candidates.push(entry.id);
                    }
                    Some(c) if c.data.remaining <= 0.0 => {
                        // 缓存过旧：不纳入候选，但记入结果让管理员可见「为什么没禁它」。
                        stale_results.push(BatchDeleteItemResult {
                            id: entry.id,
                            ok: false,
                            error: Some("余额缓存已过期（>24h），未禁用——请先刷新余额后重试".to_string()),
                        });
                    }
                    _ => {}
                }
            }
        }

        let mut results: Vec<BatchDeleteItemResult> = Vec::new();
        for id in candidates {
            // 快照之后、执行之前被删：逐条记失败而不是让整批炸掉。
            if self.token_manager.export_credential(id).is_none() {
                results.push(BatchDeleteItemResult {
                    id,
                    ok: false,
                    error: Some("凭据已被删除".to_string()),
                });
                continue;
            }
            self.token_manager.report_quota_exhausted(id);
            results.push(BatchDeleteItemResult {
                id,
                ok: true,
                error: None,
            });
        }

        let list: Vec<u64> = results
            .iter()
            .filter(|r| r.ok)
            .map(|r| r.id)
            .collect();
        let disabled = list.len();
        let failed = results.len() - disabled;
        // 过期缓存跳过项并入结果（管理员可见「为什么没禁它」），但不算 failed。
        // （BatchDeleteItemResult 无 Clone derive，直接 extend，先取 len。）
        results.extend(stale_results);
        DisableQuotaExceededResponse {
            disabled,
            failed,
            list,
            results,
        }
    }

    /// 自动禁用「余额已超额」的账号组（后台温和刷新循环用，2026-08-14 新增）。
    ///
    /// 入参是**本循环刚 commit 过真值**的账号缓存键 —— 新鲜度由调用点保证
    /// （cached_at=now），故这里不再查 24h 新鲜度门，与手动端点
    /// [`Self::disable_quota_exceeded`] 的唯一差别就在这一条。
    ///
    /// 禁用机制完全一致：逐 id 走 `report_quota_exhausted` 收口
    /// （`DisabledReason::QuotaExceeded` + 落盘 + 清亲和 + 若禁的是当前号则切号），
    /// 同 key 的 N 份分身全部禁用（账号配额是共享的，留一份仍会 402）。
    /// 幂等：已禁用的号 `report_quota_exhausted` 直接跳过，不产生副作用。
    fn auto_disable_exhausted_group(&self, key: &str) {
        let ids = match self.balance_key_to_ids().get(key) {
            Some(ids) => ids.clone(),
            None => return,
        };
        for id in &ids {
            self.token_manager.report_quota_exhausted(*id);
        }
        tracing::info!(
            "后台温和余额刷新：账号组 {} 余额已耗尽（remaining<=0），已自动禁用 {} 个凭据（开关 auto_disable_quota_exceeded=true）",
            key,
            ids.len()
        );
    }

    /// OAuth 类凭据（idc / social / external_idp）的「自助复活」。
    ///
    /// 对齐参考仓 `do_relogin_update` 的节奏（禁用 → 更新 → 重置启用），但本仓
    /// token_manager **没有写 refresh_token 的 setter**（token 只由内部刷新路径轮换），
    /// 故这里不做 token 替换，做的是清掉全部进程内惩罚状态并重新启用：
    /// `reset_and_enable` 内部已收口 `clear_transient_counters` + 冷却 + 限流器。
    ///
    /// 用途：号被 `SuspiciousActivityAuto` / 瞬时误禁 / 想强制解除冷却时的人工复活。
    /// 若 token 真的已废（`InvalidRefreshToken`），复活后首次刷新仍会失败并再次被禁 ——
    /// 那时需要的是「带新 token 的 relogin」，依赖 token_manager 补 setter（见交接报告）。
    ///
    /// 为什么先禁用再启用：与参考实现同形 —— 若中间将来插入 token 写入/验活，窗口内
    /// 号不会被调度选中；且 `reset_and_enable` 拒绝 `InvalidConfig` 号（bail）时
    /// 号会留在禁用态（fail-closed）而不是半复活。
    pub fn relogin_oauth(&self, id: u64) -> Result<(), AdminServiceError> {
        let cred = self
            .token_manager
            .export_credential(id)
            .ok_or(AdminServiceError::NotFound { id })?;
        if cred.is_api_key_credential() || cred.is_custom_api_credential() {
            return Err(AdminServiceError::InvalidCredential(format!(
                "凭据 #{} 不是 OAuth 类凭据（authMethod: {}），不支持自助复活；\
                 api_key 号应直接换 key，代挂号无此概念",
                id,
                cred.auth_method.as_deref().unwrap_or("?")
            )));
        }
        self.set_disabled(id, true)?;
        self.reset_and_enable(id)
    }

    /// 设置凭据的模型映射豁免开关（Kiro 号与 custom_api 号都可用）。
    pub fn set_credential_model_mapping_exempt(
        &self,
        id: u64,
        exempt: Option<bool>,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_credential_model_mapping_exempt(id, exempt)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 探测代挂凭据上游的可用模型列表（`GET {base_url}/v1/models`，OpenAI 兼容格式）。
    ///
    /// 现实约束：模型只能从上游获取，不硬编码。结果供前端展示 + 勾选写入 allowed_models。
    /// 仅 custom_api 代挂号有意义；SSRF 防护走 `build_streaming_client_no_redirect`（禁重定向）。
    pub async fn probe_upstream_models(&self, id: u64) -> Result<Vec<String>, AdminServiceError> {
        let cred = self
            .token_manager
            .export_credential(id)
            .ok_or(AdminServiceError::NotFound { id })?;
        if !cred.is_custom_api_credential() {
            return Err(AdminServiceError::InvalidCredential(
                "仅 custom_api 代挂凭据可探测上游模型".to_string(),
            ));
        }
        let cfg = self.token_manager.config();
        let proxy = cfg
            .proxy_url
            .as_deref()
            .map(|u| crate::http_client::ProxyConfig::new(u.to_string()));
        crate::kiro::passthrough::fetch_upstream_models(&cred, proxy.as_ref(), cfg.tls_backend)
            .await
            .map_err(|e| AdminServiceError::UpstreamError(e.to_string()))
    }

    /// 创建前探测代挂上游模型列表（`POST /credentials/probe-models`）。
    ///
    /// 凭据**还不存在**时的临时探测：构造一个仅含 base_url/api_key 的临时
    /// `KiroCredentials` 打 `GET {base}/v1/models`，**不持久化**。与
    /// [`Self::probe_upstream_models`]（需已有 id）共用同一个 `fetch_upstream_models`。
    pub async fn probe_models_standalone(
        &self,
        base_url: &str,
        api_key: Option<&str>,
    ) -> Result<Vec<String>, AdminServiceError> {
        let base_url = base_url.trim();
        if base_url.is_empty() {
            return Err(AdminServiceError::InvalidCredential(
                "自定义 API 凭据缺少 base_url".to_string(),
            ));
        }
        // 🔴 SSRF 防护必须与 create/set 路径一致：probe 会**直接**拿用户给的 base_url
        // 打上游，若不加这道 IP 层校验，可被用来打内网/169.254 元数据（响应虽只回模型
        // 列表，但错误消息可盲扫端口）。写入路径的主防线 `validate_custom_api_base_url`
        // 在这里同样要走，否则 probe 就成了唯一绕开它的口子。
        crate::kiro::token_manager::validate_custom_api_base_url(base_url)
            .await
            .map_err(|e| AdminServiceError::InvalidCredential(e.to_string()))?;
        let cred = crate::kiro::model::credentials::KiroCredentials {
            base_url: Some(base_url.to_string()),
            api_key: api_key.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            ..Default::default()
        };
        let cfg = self.token_manager.config();
        let proxy = cfg
            .proxy_url
            .as_deref()
            .map(|u| crate::http_client::ProxyConfig::new(u.to_string()));
        crate::kiro::passthrough::fetch_upstream_models(&cred, proxy.as_ref(), cfg.tls_backend)
            .await
            .map_err(|e| AdminServiceError::UpstreamError(e.to_string()))
    }

    pub fn set_credential_endpoint(
        &self,
        id: u64,
        endpoint: Option<String>,
    ) -> Result<(), AdminServiceError> {
        let cleaned = endpoint
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(ref name) = cleaned {
            if !self.known_endpoints.is_empty() && !self.known_endpoints.contains(name) {
                let mut names: Vec<_> = self.known_endpoints.iter().cloned().collect();
                names.sort();
                return Err(AdminServiceError::InvalidCredential(format!(
                    "未知 endpoint '{}'，可用: {}",
                    name,
                    names.join(", ")
                )));
            }
        }
        self.token_manager
            .set_credential_endpoint(id, cleaned)
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

    pub fn set_credential_tag(
        &self,
        id: u64,
        tag: Option<String>,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_credential_tag(id, tag)
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

    /// 获取凭据余额（带缓存 + 上游超时降级）
    ///
    /// # 为什么需要超时降级
    ///
    /// 上游链路是 `fetch_balance` → `token_manager.get_usage_limits_for` →
    /// `kiro::web_portal`（打 app.kiro.dev），而那里的 client 超时是 **30s / 60s**。
    /// 此前中间**没有任何降级**：缓存一过期，面板点余额就干等 30 秒；
    /// 前端 axios 是 15s 超时，所以用户先看到失败、而后端还在等（线上 Caddy 日志里
    /// 该端点有 5 次 502）。
    ///
    /// 现在：上游超过 [`BALANCE_UPSTREAM_TIMEOUT_SECS`] 就放弃，**有旧缓存就返旧缓存并标 stale**，
    /// 让面板显示"上次已知值 + 过期提示"而不是转圈或报错。只有连旧缓存都没有时才报错。
    ///
    /// # `force`：跳过 [`BALANCE_CACHE_TTL_SECS`] 这道新鲜度门
    ///
    /// 用户明确反馈「额度/积分刷新太慢」，而在 `force` 之前**没有任何路径**能让用户
    /// 主动取一次真值：面板列表读的是缓存（30 分钟才由后台刷一次），而本端点在
    /// 5 分钟 TTL 内直接返缓存 ⇒ 连点两次「查看余额」拿到的是同一个数字、零上游往返，
    /// 看起来就是"刷新没反应"。
    ///
    /// 风险边界（封号红线）：`force` **只作用于显式的单号请求**，不存在批量入口
    /// （`get_cached_balances` 恒零上游，后台刷新仍是 30 分钟 + 逐个 4 秒间隔）。
    /// 与既有的 `GET /credentials/{id}/overage`（每次调用都真打上游）同一量级。
    pub async fn get_balance(
        &self,
        id: u64,
        force: bool,
    ) -> Result<BalanceResponse, AdminServiceError> {
        let cache_key = self.balance_cache_key(id);
        // 先查缓存（新鲜即直接返；force 时只取降级值，不早返）
        let stale_fallback = {
            let cache = self.balance_cache.lock();
            match cache.get(&cache_key) {
                Some(cached) => {
                    let now = Utc::now().timestamp() as f64;
                    if !force && (now - cached.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                        tracing::debug!("凭据 #{} 余额命中缓存", id);
                        return Ok(cached.data.clone());
                    }
                    // 过期但可用：留作上游失败/超时时的降级值。
                    Some(cached.data.clone())
                }
                None => None,
            }
        };

        // 缓存未命中或已过期，从上游获取 —— 但**绝不为上游慢而无限等**。
        let balance = match tokio::time::timeout(
            std::time::Duration::from_secs(BALANCE_UPSTREAM_TIMEOUT_SECS),
            self.fetch_balance(id),
        )
        .await
        {
            Ok(r) => r?,
            Err(_) => {
                // 超时：有旧值就返旧值（标 stale），没有才报错。
                // 这是"面板可读性优先于数值新鲜度"的刻意取舍 —— 余额只用于展示，
                // 不参与调度决策（balanceWeightEnabled 走的是独立的 BalanceSnapshot 回推）。
                if let Some(mut stale) = stale_fallback {
                    stale.stale = true;
                    tracing::warn!(
                        credential_id = id,
                        timeout_secs = BALANCE_UPSTREAM_TIMEOUT_SECS,
                        "余额上游超时，返回上次已知值并标记 stale（面板显示过期提示而非报错）"
                    );
                    return Ok(stale);
                }
                tracing::warn!(
                    credential_id = id,
                    timeout_secs = BALANCE_UPSTREAM_TIMEOUT_SECS,
                    "余额上游超时且无历史缓存可降级"
                );
                return Err(AdminServiceError::UpstreamTimeout(id));
            }
        };

        // 落缓存 + **同步重置花费基线**（按账号键，于是同 key 的全部分身立刻共享这次结果）。
        // ⚠️ 绝不在这里内联 `cache.insert`：那会漏掉基线重置 → 面板把已含在真值里的花费
        // 再扣一次（见 `commit_fresh_balance` 的算例）。
        self.commit_fresh_balance(cache_key, balance.clone());

        Ok(balance)
    }

    /// 余额缓存的键：**同一个上游账号只有一个键**。
    ///
    /// - api_key 号（`ksk_`）→ `sha256(kiroApiKey)`。同 key 的全部分身共享一条缓存 ⇒
    ///   任一份刷新即全组同步，且上游 `getUsageLimits` 探测从 N 次降到 1 次。
    /// - 其余（OAuth：social / idc / external_idp）→ 十进制 `id`，保持原行为。
    ///
    /// # 为什么 OAuth 必须继续按 id
    ///
    /// 它们没有 `kiroApiKey`，无从算账号指纹。若为了"统一"给它们编一个共享键，
    /// 会把**互不相关的多个 OAuth 账号**的余额混成一条 —— 那是比不同步严重得多的错误
    /// （面板会显示别人的额度）。判据复用 `is_api_key_credential()`，与
    /// `api_key_hash` 字段的算法（`token_manager.rs:5484`：仅 api_key 号才算 sha256）同源。
    ///
    /// # 取不到凭据时
    ///
    /// 回落到 id。这只发生在凭据刚被删除的竞态里，此时缓存键正确与否都无意义。
    fn balance_cache_key(&self, id: u64) -> String {
        match self.token_manager.export_credential(id) {
            Some(c) if c.is_api_key_credential() => match c.kiro_api_key.as_deref() {
                Some(k) => crate::kiro::token_manager::sha256_hex(k),
                // api_key 号但 key 为空：配置无效（`InvalidConfig` 会禁用它），
                // 回落 id 而不是拿空串当共享键——空串会把所有这类号混成一条。
                None => id.to_string(),
            },
            _ => id.to_string(),
        }
    }

    /// 删凭据后清理它的余额缓存 —— **仅当没有别的凭据还共享同一个账号键**。
    ///
    /// # 为什么必须有条件
    ///
    /// 缓存按账号键存（`balance_cache_key`），一条被同 key 的 N 份分身共享。
    /// 无条件 `remove` 会让「删掉一份分身」把**整组**的余额缓存清掉 ⇒ 剩下的份
    /// 面板显示"暂无数据"，直到下次刷新（默认 30 分钟）或用户手点查余额。
    ///
    /// # 调用约定：`key` 必须在删除**之前**算好
    ///
    /// `balance_cache_key` 走 `export_credential`，凭据删掉后它返 `None` ⇒ 回落成 id
    /// 字符串 ⇒ 清的是一个不存在的键，真正那条泄漏在缓存里。所以键由调用方在删除前传入。
    fn prune_balance_cache_for_deleted(&self, key: &str) {
        // 还有别的凭据共享这个键吗？（此刻目标凭据已从池中移除）
        let still_shared = self
            .token_manager
            .snapshot()
            .entries
            .iter()
            .any(|e| self.balance_cache_key(e.id) == key);
        if still_shared {
            return;
        }
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(key);
        }
        self.save_balance_cache();
    }

    /// 账号缓存键 → 共享它的**全部**凭据 id。
    ///
    /// 缓存按账号键存（一条），而面板与调度器都按**凭据 id** 消费 —— 所以读回时必须把
    /// 一条展开成 N 条。这就是「同 key 的分身余额必然一致」在 UI 上真正生效的地方：
    /// 它们读的是同一条缓存，不存在各自一份、谁刷谁新的可能。
    ///
    /// 含禁用号：面板要显示禁用号的最后已知余额（判断是不是额度耗尽导致的禁用）。
    fn balance_key_to_ids(&self) -> HashMap<String, Vec<u64>> {
        let mut out: HashMap<String, Vec<u64>> = HashMap::new();
        for e in self.token_manager.snapshot().entries {
            out.entry(self.balance_cache_key(e.id))
                .or_default()
                .push(e.id);
        }
        out
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
            // 从上游新取的值 = 新鲜。降级路径在 get_balance 里显式置 true。
            stale: false,
            // 直接从上游取的是真值；乐观修正只发生在 get_cached_balances 的展示路径。
            optimistic: false,
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
        // 缓存按**账号**键存，而前端按**凭据 id** 展示 ⇒ 一条展开成共享它的全部 id。
        // 同 key 的分身因此读到**同一条**缓存，余额必然一致（这是同步生效的落点）。
        let key_to_ids = self.balance_key_to_ids();
        let cache = self.balance_cache.lock();
        let mut balances: HashMap<u64, CachedBalanceItem> = HashMap::new();
        for (key, c) in cache.iter() {
            if (now - c.cached_at) >= BALANCE_CACHE_DISPLAY_MAX_AGE_SECS as f64 {
                continue;
            }
            let item = CachedBalanceItem {
                balance: c.data.clone(),
                cached_at: c.cached_at,
            };
            match key_to_ids.get(key) {
                Some(ids) => {
                    for id in ids {
                        balances.insert(*id, item.clone());
                    }
                }
                // 键在缓存里但池中已无对应凭据（号被删）。若键本身是十进制 id（旧格式
                // 或 OAuth 号），仍按它展示，避免刚删号那一刻面板闪空。
                None => {
                    if let Ok(id) = key.parse::<u64>() {
                        balances.insert(id, item);
                    }
                }
            }
        }
        drop(cache);

        // ⭐ dwgx 需求「用了余额之后要刷新额度显示」：用**本地累计的 credit 花费**做乐观修正。
        //
        // 问题：余额真值由后台每 30 分钟温和刷新一次（`refresh_all_balances_gently`），
        // 所以刚跑完一批请求，面板上的额度**最多 30 分钟内都不动** —— 用户以为没生效。
        //
        // 为什么不每次请求都打上游：那是 `web_portal`（app.kiro.dev）探测，会**加重风控**。
        // 线上号池正被风控烧号（单号存活 25~60 分钟），多打探测只会更糟。
        //
        // 做法：`total_credits_used` 是每次请求完成后由 `meteringEvent` 真实计费量累加的
        // （`token_manager::add_credits`）。缓存里存了取值当时的 `credits_used_at_cache` 基线，
        // 两者之差 = **缓存之后新花掉的量**，据此乐观推进 current_usage / remaining / 百分比。
        // 后台刷新到来时用真值覆盖，所以误差不累积、只在两次真值之间起插值作用。
        // 复用**已有**的两套数据，不新造并行链路：
        // - `credits_used_snapshot()`：各号当前的 `total_credits_used`（由 meteringEvent 累加）
        // - `balance_baselines()`：`set_balance_snapshots` 回推时记下的 `credits_used_at_cache`
        //   （余额加权分流已经在用这个基线，见 token_manager 的 balance_factor）
        let used_now = self.token_manager.credits_used_snapshot();
        let baselines = self.token_manager.balance_baselines();
        let mut balances = balances;
        for (id, item) in balances.iter_mut() {
            let (Some(&now_used), Some(&base)) = (used_now.get(id), baselines.get(id)) else {
                continue;
            };
            // 只做**单向**推进：delta<=0 说明基线比当前还大（重启后计数从 0 起等），此时不动。
            let delta = now_used - base;
            if !(delta > 0.0) {
                continue;
            }
            let b = &mut item.balance;
            b.current_usage += delta;
            // remaining 不得为负：额度用超时上游会自己表达（overage/402），这里只保证展示不出负数。
            b.remaining = (b.remaining - delta).max(0.0);
            if b.effective_limit > 0.0 {
                b.usage_percentage = (b.current_usage / b.effective_limit * 100.0).min(100.0);
            }
            // 标记为"含本地推算"：与上游真值区分，前端可据此加"约"字样或提示。
            b.optimistic = true;
        }

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
        //
        // 🔴 必须排除 custom_api 代挂号（2026-08-10 修）：它们是用户自购的 Anthropic 兼容
        // 中转站，**没有 Kiro 账号**，`get_usage_limits` / `web_portal` 对它们必然失败
        // （`ensure_valid_token` 对代挂号返空 token 后仍会打上游，失败只被 warn 忽略）。
        // ⇒ 改前每轮后台刷新都对每个代挂号白打一次注定失败的上游请求。
        // 这与下面那条「绝不为展示类需求反复打 web_portal（加重风控）」的既定原则同向。
        let all_ids: Vec<u64> = self
            .token_manager
            .snapshot()
            .entries
            .into_iter()
            // 判据与 `KiroCredentials::is_custom_api_credential()` **逐条对齐**
            // （`auth_method == "custom_api"` 或 `base_url` 非空）—— 只判前者会漏掉
            // 「auth_method 未写全但配了 base_url」的历史号，那些同样没有 Kiro 账号。
            .filter(|e| {
                !e.disabled
                    && e.auth_method.as_deref() != Some("custom_api")
                    && e.base_url.is_none()
            })
            .map(|e| e.id)
            .collect();

        // ⭐ 按**账号**去重：同一个 `ksk_` key 的 N 份分身共享一个上游账号与一份配额，
        // 逐份打就是 N 次 `web_portal` 往返拿同一个数字。而 `web_portal` 是上游探测，
        // 调多了会加重风控（本仓调优结论：绝不为展示类需求反复打它）。
        //
        // 缓存现在按账号键（`balance_cache_key`），所以同组只需刷一份 —— 结果自动
        // 覆盖全组。实测线上一组 4 份分身，这一步把 4 次探测降到 1 次。
        //
        // 取组内**第一个**（id 升序，即主份优先）：与前端「查余额只打主份」同口径。
        let ids: Vec<u64> = {
            let mut seen: HashSet<String> = HashSet::new();
            all_ids
                .into_iter()
                .filter(|id| seen.insert(self.balance_cache_key(*id)))
                .collect()
        };

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
                    // usage_limit 先读（commit_fresh_balance 会移动 balance——M4 的门条件
                    // 必须在移动前取值，2026-08-13 编译期修正）。
                    let balance_usage_limit = balance.usage_limit;
                    let exhausted = balance.remaining <= 0.0;
                    let key = self.balance_cache_key(*id);
                    // 落缓存 + 重置该账号基线，走与「查看余额」**同一个**收口
                    // （两条路径各写一份 insert 正是基线漏更新的根源）。
                    // 逐个提交而不是攒到本轮末尾：一轮要走 N×4 秒，早提交的号能早点
                    // 在面板/调度器上生效，且中途进程重启不会白刷。
                    self.commit_fresh_balance(key.clone(), balance);
                    tracing::debug!("后台温和余额刷新：凭据 #{} 已更新缓存", id);
                    // ⭐ 超额自动禁用（2026-08-14 新增）：刚取到的上游真值必然新鲜
                    // （cached_at=now），无需 24h 新鲜度门；语义与手动端点
                    // disable_quota_exceeded 完全一致（report_quota_exhausted 收口）。
                    // 开关默认开，可在面板服务端配置里关闭。
                    // ⚠️ 2026-08-13 对抗审查 M4：空 breakdown 时 usage_limit()=0 → remaining=0，
                    // 会误杀「新号无 usage 记录 / 上游返回空 breakdown」的号（不可逆需人工
                    // 解禁）。必须加 limit>0 门：真额度用尽的号 limit 是正数（remaining=0
                    // 是已用尽），空 breakdown 的号 limit=0（拿不到额度信息）→ 跳过自动禁用。
                    if exhausted
                        && balance_usage_limit > 0.0
                        && self
                            .auto_disable_quota_exceeded
                            .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        self.auto_disable_exhausted_group(&key);
                    }
                }
                Err(e) => {
                    // 单个失败不影响整体节奏；仅更新缓存展示，不做任何禁用动作
                    tracing::warn!("后台温和余额刷新：凭据 #{} 刷新失败（忽略）: {}", id, e);
                }
            }
        }

        // 收尾回推：把**没能刷成功**但缓存里有值的号也补进表（否则调度侧缺表=中性因子 1.0，
        // 余额加权对它们完全失效）。`fresh_keys` 传空 ⇒ 它们保留原基线，不会被误当成
        // "刚取到真值"（见 `push_balance_snapshots_to_scheduler` 的 fresh_keys 文档）。
        // 刷成功的那些已在循环里逐个提交过，这里对它们是幂等的。
        self.push_balance_snapshots_to_scheduler(&HashSet::new());

        tracing::info!("后台温和余额刷新完成");
    }

    /// 把一次**新取到的上游真值**落进缓存，并**同步重置该账号的花费基线**。
    ///
    /// # 为什么必须是一个函数（G-2 修的就是这里）
    ///
    /// 面板列表（`get_cached_balances`）在两次真值之间做**乐观修正**：
    /// `delta = 当前 total_credits_used - credits_used_at_cache`，把 delta 从 remaining 里扣掉。
    /// 这要求「缓存里的真值」与「基线」**成对更新**。
    ///
    /// 而此前只有后台温和刷新那条路径会更新基线（`refresh_all_balances_gently` 末尾那次
    /// 回推），`get_balance`（面板「查看余额」）**只写缓存不动基线** ⇒ 新真值配着旧基线：
    ///
    /// - t0 后台刷新：remaining=100，基线=50 花费
    /// - 期间花掉 20（total=70）→ 面板显示 100-20=80 ✅
    /// - 用户点「查看余额」：上游真值 80（已含那 20），写进缓存，基线仍是 50
    /// - 面板下一次轮询：80-(70-50)=**60** ❌ 那 20 被扣了两次
    ///
    /// 于是「查看余额」拿到 80、而列表显示 60，同一个号两个数字，且**越刷越低**，
    /// 直到 30 分钟后的后台刷新才对上 —— 这正是"额度刷新不对/很慢"的一条实因。
    ///
    /// 收口成一个函数是刻意的：两条路径各写一份 `cache.insert` 正是漏改的根源
    /// （与 `update.rs` 抽 `read_body_capped` 同一理由）。
    fn commit_fresh_balance(&self, cache_key: String, balance: BalanceResponse) {
        {
            let mut cache = self.balance_cache.lock();
            cache.insert(
                cache_key.clone(),
                CachedBalance {
                    cached_at: Utc::now().timestamp() as f64,
                    data: balance,
                },
            );
        }
        self.save_balance_cache();
        // 只把**这一个账号**标记为"刚取到真值"。其余账号保留原基线 —— 见
        // `push_balance_snapshots_to_scheduler` 的 `fresh_keys` 文档。
        let mut fresh = HashSet::new();
        fresh.insert(cache_key);
        self.push_balance_snapshots_to_scheduler(&fresh);
    }

    /// 把当前余额缓存 + 各号 total_credits_used 基线打包成 BalanceSnapshot 表,回推给调度器。
    /// 供余额加权分流:remaining/effective_limit 归一成剩余比例,credits_used 作累加修正基线。
    ///
    /// # `fresh_keys`：哪些账号的基线该被重置
    ///
    /// 只有**本次真的取到上游真值**的账号键才重置基线（`credits_used_at_cache` = 当前花费）。
    /// 其余账号**保留原基线**。
    ///
    /// 为什么不能一律重置（原实现的缺陷）：基线与 `remaining_at_cache` 是一对，描述
    /// 「在花费为 X 的那一刻剩余是 R」。若某号本轮刷新**失败**（cache 里仍是旧的 R），
    /// 却把基线推到"现在"，那么 R 与新基线描述的不是同一时刻 ⇒ 期间已花掉的量被一次性
    /// 抹掉 ⇒ 面板与调度器都把它当成**比实际更有余额**的号。
    /// 表里原本没有该 id（新号 / 首次入表）时才回落到当前花费。
    fn push_balance_snapshots_to_scheduler(&self, fresh_keys: &HashSet<String>) {
        use crate::kiro::token_manager::BalanceSnapshot;
        // 上一轮的基线（保留用）。
        let prev_baselines = self.token_manager.balance_baselines();
        // 各号当前累计花费(本地实时,作累加修正基线)。
        let used_by_id: std::collections::HashMap<u64, f64> = self
            .token_manager
            .snapshot()
            .entries
            .into_iter()
            .map(|e| (e.id, e.total_credits_used))
            .collect();
        // 缓存按**账号**键 ⇒ 展开给共享它的每个凭据 id。
        //
        // 展开而非只给一个，是因为调度器按 id 查表：同组各份共享同一份上游配额，
        // 它们的 remaining 本来就该相同。只给主份会让其余份「缺表」→ 调度侧按中性因子
        // 1.0 处理 ⇒ 余额加权分流对分身完全失效（配额快耗尽时仍被当满额号选中）。
        //
        // `credits_used_at_cache` 仍**按各自 id 取**：那是本地累计花费的修正基线，
        // 每份各自累加，不共享。
        let key_to_ids = self.balance_key_to_ids();
        let snaps: std::collections::HashMap<u64, BalanceSnapshot> = {
            let cache = self.balance_cache.lock();
            let mut out: std::collections::HashMap<u64, BalanceSnapshot> =
                std::collections::HashMap::new();
            for (key, cb) in cache.iter() {
                let eff = if cb.data.effective_limit > 0.0 {
                    cb.data.effective_limit
                } else {
                    // 旧缓存可能无 effective_limit,回退 usage_limit(base)。<=0 则跳过(调度侧缺表=中性)。
                    cb.data.usage_limit
                };
                if eff <= 0.0 {
                    continue;
                }
                let ids: Vec<u64> = match key_to_ids.get(key) {
                    Some(v) => v.clone(),
                    None => key.parse::<u64>().map(|i| vec![i]).unwrap_or_default(),
                };
                let is_fresh = fresh_keys.contains(key);
                for id in ids {
                    let used_now = used_by_id.get(&id).copied().unwrap_or(0.0);
                    out.insert(
                        id,
                        BalanceSnapshot {
                            remaining_at_cache: cb.data.remaining,
                            effective_limit: eff,
                            // 刚取到真值 → 基线归零到"现在"；否则保留原基线（缺表才回落）。
                            credits_used_at_cache: if is_fresh {
                                used_now
                            } else {
                                prev_baselines.get(&id).copied().unwrap_or(used_now)
                            },
                        },
                    );
                }
            }
            out
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
        // 代理池自动健康调度搭车本函数作为**启动/热重挂入口**：main.rs 只调
        // `respawn_balance_task`（main.rs 不在改动范围），而本服务没有别的
        // `&Arc<Self>` 启动现场——故在这里顺带重挂 socks 健康任务。
        // 两个任务各自独立：自己的字段/循环/开关，此处只是共用调用时机。
        self.respawn_socks_health_task();
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

    /// 重挂代理池自动健康调度任务（受管任务槽，对齐 [`Self::respawn_balance_task`]）。
    ///
    /// - 启动入口：由 `respawn_balance_task` 顺带调用（见其开头注释）；
    /// - 开关 `socks_auto_health` 在任务循环内自检（改开关走 update_config，
    ///   不需要重挂——关着就整轮跳过，重开即恢复探测）；
    /// - 幂等：重复调用先 abort 旧句柄再重建，不会累积多个循环；
    /// - 任务体持 `Weak<Self>`：AdminService 被 drop 后下一轮 upgrade 失败即自我退出。
    ///
    /// 间隔固定 `SOCKS_HEALTH_CHECK_INTERVAL_SECS`（无配置项，见常量注释）。
    /// 首轮等满一个完整间隔才开始（对齐余额任务，避免启动即打一批探针）。
    pub fn respawn_socks_health_task(self: &Arc<Self>) {
        let mut slot = self.socks_health_task.lock();
        if let Some(old) = slot.take() {
            old.abort();
        }
        let weak = Arc::downgrade(self);
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
                SOCKS_HEALTH_CHECK_INTERVAL_SECS,
            ));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let Some(svc) = weak.upgrade() else {
                    tracing::debug!("AdminService 已释放，代理池健康调度任务退出");
                    break;
                };
                // 开关在任务内自检：关闭时整轮跳过（任务常驻但不做事，
                // 重开无需重挂）。池空时 `run_socks_health_round` 内部直接返回。
                if !svc.socks_auto_health.load(std::sync::atomic::Ordering::Relaxed) {
                    continue;
                }
                svc.run_socks_health_round().await;
            }
        });
        *slot = Some(handle);
        tracing::info!(
            "代理池自动健康调度已启用：间隔 {} 秒，连续失败 {} 次自动禁用",
            SOCKS_HEALTH_CHECK_INTERVAL_SECS,
            SOCKS_HEALTH_FAIL_THRESHOLD
        );
    }

    /// 跑一轮代理池健康探测：对池内**启用**节点逐个探测，按连续失败计数处置。
    ///
    /// - 池空直接返回（「只在池非空时跑」）；
    /// - round-robin：每轮从不同起点开始（`socks_health_round` 取模），
    ///   保证长时间运行下各节点被探测的时机公平，不固定偏袒队首；
    /// - 节点间**不**加 sleep：探针目标是固定公共服务（非上游 kiro，无风控节奏约束），
    ///   且单节点 10s 超时本身就是天然节奏；一轮慢不会丢下一轮
    ///   （`MissedTickBehavior::Skip`，探测本身串行不并发）。
    async fn run_socks_health_round(&self) {
        let enabled: Vec<(u64, String, Option<String>, Option<String>)> = {
            let nodes = self.socks_nodes.lock();
            nodes
                .iter()
                .filter(|n| n.enabled)
                .map(|n| (n.id, n.url.clone(), n.username.clone(), n.password.clone()))
                .collect()
        };
        if enabled.is_empty() {
            return;
        }
        let start = self
            .socks_health_round
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            as usize
            % enabled.len();
        for k in 0..enabled.len() {
            let (id, url, user, pass) = &enabled[(start + k) % enabled.len()];
            let test = self
                .probe_socks_node(url, user.clone(), pass.clone())
                .await;
            self.apply_socks_health_result(*id, test);
        }
    }

    /// 探测单个代理节点（复用 `/proxy/test` 与 `/socks/nodes/{id}/test` 的探针口径）。
    ///
    /// 探针 URL 与 `handlers.rs::run_proxy_probe` 共用同一常量（SSRF 防线：
    /// 目标硬编码固定，绝不接受请求方传入）。返回 `SocksNodeTest` 而非
    /// `ProxyTestResponse`：后台调度直接消费节点表同款结构，写回零转换。
    async fn probe_socks_node(
        &self,
        proxy_url: &str,
        username: Option<String>,
        password: Option<String>,
    ) -> SocksNodeTest {
        use crate::http_client::{ProxyConfig, build_client, split_proxy_credentials};
        use crate::admin::handlers::PROXY_TEST_PROBE_URL;

        let started = std::time::Instant::now();
        let tested_at = chrono::Utc::now().timestamp().max(0) as u64;
        let fail = |error: String| SocksNodeTest {
            ok: false,
            latency_ms: started.elapsed().as_millis() as u64,
            exit_ip: None,
            error: Some(error),
            tested_at,
        };

        // 拆出干净 URL 与内嵌账密；显式字段优先覆盖内嵌账密（与 run_proxy_probe 同款）。
        let (clean_url, embedded_user, embedded_pass) = split_proxy_credentials(proxy_url);
        // 池内节点按 SSRF 校验入库，直连形态理论不存在；真出现就按失败计（无意义探测）。
        if clean_url.is_empty() || clean_url.eq_ignore_ascii_case("direct") {
            return fail("节点地址无效（直连形态，后台调度拒绝探测）".into());
        }
        let username = username.filter(|s| !s.trim().is_empty()).or(embedded_user);
        let password = password.filter(|s| !s.is_empty()).or(embedded_pass);
        let mut cfg = ProxyConfig::new(clean_url);
        if let (Some(u), Some(p)) = (username, password) {
            cfg = cfg.with_auth(u, p);
        }
        // 与 run_proxy_probe 同款 10s 超时（连不上/超时都算失败）。
        let client = match build_client(Some(&cfg), 10, self.tls_backend()) {
            Ok(c) => c,
            Err(e) => return fail(format!("构建代理客户端失败: {e}")),
        };

        // 目标固定为硬编码探针 URL（与 /proxy/test 同一常量，见该常量注释）。
        match client.get(PROXY_TEST_PROBE_URL).send().await {
            Ok(resp) => {
                let status = resp.status();
                let latency_ms = started.elapsed().as_millis() as u64;
                if !status.is_success() {
                    return fail(format!("探针返回非 2xx 状态: {status}"));
                }
                // 解析 {"ip":"..."}；解析失败不影响连通性判定，仅 exit_ip 为 None。
                let exit_ip = resp.json::<serde_json::Value>().await.ok().and_then(|v| {
                    v.get("ip")
                        .and_then(|ip| ip.as_str().map(|s| s.to_string()))
                });
                SocksNodeTest {
                    ok: true,
                    latency_ms,
                    exit_ip,
                    error: None,
                    tested_at,
                }
            }
            Err(e) => fail(format!("代理连通失败: {e}")),
        }
    }

    /// 处置一次自动探测的结果：成功清零计数并写回；失败累计，达阈值自动禁用。
    ///
    /// 锁序注意：本方法**从不**同时持有 `socks_fail_counts` 与 `socks_nodes` 两把锁
    /// （计数在短临界区内算完即释放，再单独走节点写路径），
    /// 与 `upsert_socks_node` 的「nodes 锁内查计数」方向一致，无死锁交叉。
    fn apply_socks_health_result(&self, id: u64, test: SocksNodeTest) {
        if test.ok {
            self.socks_fail_counts.lock().remove(&id);
            if let Err(e) = self.record_socks_node_test(id, test) {
                tracing::warn!("代理池健康调度：写回节点 #{id} 成功结果失败: {e}");
            }
            return;
        }
        // 失败：计数在短临界区内 +1 后立即释放 counts 锁（见方法注释的锁序说明）。
        let fails = {
            let mut m = self.socks_fail_counts.lock();
            let c = m.entry(id).or_insert(0);
            *c += 1;
            *c
        };
        if fails < SOCKS_HEALTH_FAIL_THRESHOLD {
            // 未达阈值：只写回失败结果（面板可见「最近失败」，还不到动手的时机）。
            if let Err(e) = self.record_socks_node_test(id, test) {
                tracing::warn!("代理池健康调度：写回节点 #{id} 失败结果失败: {e}");
            }
            return;
        }
        // 达阈值：自动禁用（enabled=false + 失败结果 + 计数清零 + 落盘）。
        // 禁用只改节点表本身——已绑该节点的凭据保持绑定（与手动删除同语义，
        // 不主动切走既有出口），节点只从「新分配候选」里消失。
        self.socks_fail_counts.lock().remove(&id);
        let note = format!("连续 {fails} 次探测失败，已自动禁用");
        let mut disabled_test = test;
        disabled_test.error = Some(note);
        {
            // 只读降级与 record 路径同款先判后改：拒写时内存也不动（防内存/磁盘不一致）。
            if let Err(e) = self.ensure_socks_writable() {
                tracing::warn!("代理池健康调度：节点 #{id} 已连续失败 {fails} 次，但节点表只读降级，自动禁用被跳过: {e}");
                return;
            }
            let mut nodes = self.socks_nodes.lock();
            let Some(node) = nodes.iter_mut().find(|n| n.id == id) else {
                tracing::debug!("代理池健康调度：节点 #{id} 已被删除，跳过自动禁用");
                return;
            };
            node.enabled = false;
            node.last_test = Some(disabled_test);
        }
        // ⭐ 落盘必须在节点锁**之外**：persist 内部会重新锁节点表
        // （与 upsert_socks_node 的「先 drop(nodes) 再 persist」同款，持锁调用必死锁）。
        match self.persist_socks_nodes() {
            Ok(()) => tracing::info!("代理池健康调度：节点 #{id} 连续失败 {fails} 次，已自动禁用"),
            Err(e) => tracing::warn!("代理池健康调度：节点 #{id} 自动禁用后落盘失败: {e}"),
        }
    }

    /// 添加新凭据
    pub async fn add_credential(
        &self,
        req: AddCredentialRequest,
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        // 普通上号路径：多开意图**只能**由 `copies > 1` 推断（去重保护的关键，
        // 见下方 `is_multi_open` 处的长注释）。
        self.add_credential_with_intent(req, false).await
    }

    /// 给**池中已有**的凭据再加 N 份分身（`POST /credentials/{id}/clone`）。
    ///
    /// # 为什么需要一个按 id 的端点
    ///
    /// 分身管理页列出的是凭据状态（`CredentialStatusItem`），里面只有 `apiKeyHash` 与
    /// 掩码形式，**没有** `kiroApiKey` 原文 —— 这是刻意的（明文 key 不下发前端）。
    /// 于是前端无法自己拼 `POST /credentials` 来给已有组加分身，只能让用户回到加号
    /// 对话框重新粘一遍 key。按 id 走服务端读 key 是**严格更好**的方案：key 一步都不
    /// 离开服务端。
    ///
    /// # 不重复实现份数逻辑
    ///
    /// 本方法只做「按 id 取出凭据 → 拼出等价的 AddCredentialRequest」，随后原样走
    /// [`Self::add_credential_with_intent`]。去重绕过 / 组复用 / 序号原子预留 /
    /// 节点池分配 / OAuth 拒绝 / 份数 clamp 全部沿用同一段实现，不存在第二条校验路径。
    ///
    /// # `force_multi_open` 为何是必须的
    ///
    /// 共享实现按 `copies > 1` 推断多开意图，于是 `copies == 1` 会走去重 →
    /// 对一个**已在池中**的 key 必然撞 `凭据已存在`。而「再加 1 份」正是本端点最常见
    /// 的用法，所以这里必须显式声明意图，而不是把判据改成 `copies.is_some()`
    /// （后者会让普通上号路径永久丢掉误双击保护，那正是先前修掉的缺陷）。
    ///
    /// # `enabled` 缺省落到配置项 `cloneDefaultEnabled`（其默认 false ⇒ 建出来是**禁用**的）
    ///
    /// 见 [`CloneCredentialRequest::enabled`] 与
    /// [`crate::model::config::Config::clone_default_enabled`] 的完整理由。要点：分身入池
    /// 即被调度，而此刻出口/region 都还没核对过，实测出现过「4 个分身 24 秒内全被自动禁用、
    /// 0% 成功」而真实流量正打在上面。显式请求值恒压过配置项。
    ///
    /// # `replace_primary = Some(true)` 时**先建后删**
    ///
    /// 建完 N 份再软删主份 `id`。顺序与用户原话（"先删后建"）相反是刻意的：主份是按 key
    /// 继承 region 的唯一来源，先删会让每份分身 `apiRegion=None` → 恒 403。
    /// 删除失败**不**判整个请求失败（分身已真的建出来了），只在 `message` 里点名。
    /// 完整理由见 [`CloneCredentialRequest::replace_primary`]。
    ///
    /// ⚠️ 必须在**入池时**就是 disabled，不能"先建后批量禁用"——后者有中间窗口，
    /// 那段时间调度器已经在往分身上发流量了。所以这里把意图翻译成
    /// `AddCredentialRequest::disabled` 交给共享实现，由它写进每一份
    /// （第 1 份走 `new_cred`，第 2..N 份是 `new_cred.clone()`，故天然逐份生效）。
    /// **父号自身的启用状态不受影响**：本方法只读父号（`export_credential`），
    /// 建出来的全是新条目。
    pub async fn clone_credential(
        &self,
        id: u64,
        copies: u32,
        enabled: Option<bool>,
        node_ids: Option<Vec<u64>>,
        assign_primary_node: Option<bool>,
        require_node_per_copy: Option<bool>,
        replace_primary: Option<bool>,
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        let cred = self
            .token_manager
            .export_credential(id)
            .ok_or(AdminServiceError::NotFound { id })?;

        // 先在这里拦一次 OAuth 号：共享实现也会拦（`multi_open_rejection_reason`），
        // 但那要等构造完请求才判，而这里能给出带 id 的更直接的报错。
        if let Some(reason) = multi_open_rejection_reason(&cred) {
            return Err(AdminServiceError::InvalidCredential(format!(
                "凭据 #{id} 不支持加分身：{reason}"
            )));
        }

        let req = AddCredentialRequest {
            // 只带身份字段。region 三兄弟 / subscriptionTitle / clone_group 都由共享
            // 实现按 key 从池中既有号继承（那段逻辑本身就是为这个场景写的），
            // 这里刻意不重复一遍，避免两处继承规则分叉。
            auth_method: cred.auth_method.clone().unwrap_or_else(|| "api_key".into()),
            kiro_api_key: cred.kiro_api_key.clone(),
            // 与同组既有成员同调度档位：不带就会落 serde default 0，
            // 新分身反而排在父号之前，凭空改变整池的调度顺序。
            priority: cred.priority,
            rpm_limit: cred.rpm_limit,
            endpoint: cred.endpoint.clone(),
            tag: cred.tag.clone(),
            // ⭐ 分身默认**不启用**：`enabled` 缺省 → 落到配置项 `cloneDefaultEnabled`
            // （其默认值 false ⇒ disabled = true，与本行之前的硬编码逐字节等价）。
            //
            // 必须是 `unwrap_or_else` 而不是把配置项 `unwrap_or` 到前面去：显式请求值
            // （true **或** false）恒优先，配置项只在字段缺省时被查询 —— 否则服务端配成
            // true 时面板上那个开关就"关不掉"了。
            //
            // 走 `AddCredentialRequest::disabled` 这个既有字段而不是另开一条路：
            // 共享实现已经把它写进 `new_cred.disabled`，而第 2..N 份是 `new_cred.clone()`，
            // 于是"每一份都禁用"不需要任何额外循环（也就不会撞上那道
            // 「clone_credential 函数体里不得出现入池调用」的源码守卫）。
            disabled: !enabled.unwrap_or_else(|| self.clone_default_enabled()),
            // 调用方显式指定的节点 id 列表（可缺省）。原样透给共享实现 —— 解析 id、
            // 跳过无效 id、按顺序逐份分配全部只有一份实现（见 `resolve_node_plan`）。
            node_ids,
            // ⭐ 本端点缺省 = **true**（第 1 份也从池里取节点），与
            // `AddCredentialRequest` 的缺省相反。理由见 `CloneCredentialRequest`
            // 上的长注释：这条路父号一字节不动，"主份"是本次新建的第 1 个分身，
            // 与其余份完全同质；让它独独裸连而池里空着一个节点，正是 2026-08-05
            // 修掉的那个缺陷。`unwrap_or(true)` 在这里而不在共享实现里，是因为
            // 共享实现要为**两条入口的两个不同缺省**服务，把默认值写在入口侧才不打架。
            assign_primary_node: Some(assign_primary_node.unwrap_or(true)),
            require_node_per_copy,
            // machineId 必须留空：分身的核心就是各自独立指纹（共享实现会派生+撞车轮换）。
            // proxy_* 同样留空：本端点从不代用户决定出口，出口只来自节点池分配
            // （`node_ids` 或自动），而 proxy_* 有值会被判成"调用方已显式指定代理"而不介入。
            ..Default::default()
        };
        let mut created = self
            .add_credential_with_intent(AddCredentialRequest { copies: Some(copies), ..req }, true)
            .await?;

        // ⭐ 勾了「删除主份」才走这里。**必须在共享实现之后**：主份是按 key 继承
        // region/subscriptionTitle/cloneGroup 的唯一来源，先删就等于让每份分身丢 region
        // → 恒 403（完整理由见 `CloneCredentialRequest::replace_primary`）。
        //
        // 删除失败不把整个请求判失败：N 份分身已经真的建出来了，回 Err 会让前端提示
        // "生成失败"而池里凭空多了 N 份 —— 那比"主份没删掉"难排查得多。
        // 所以失败只在 message 里点名，让用户手工删那一份。
        if replace_primary.unwrap_or(false) {
            match self.delete_credential_forced(id, true) {
                Ok(()) => {
                    created.message.push_str(&format!(
                        "；已按「删除主份」删掉 #{id}（软删，可从「设置 → 回收站」恢复），\
                         本组现由新建的 {copies} 份同质分身组成"
                    ));
                }
                Err(e) => {
                    tracing::warn!(
                        credential_id = id,
                        error = %e,
                        "分身已建好但主份删除失败，需人工处理"
                    );
                    created.message.push_str(&format!(
                        "；⚠️ 分身已建好，但主份 #{id} **删除失败**（{e}）—— \
                         它仍在池中且没有独立出口，请手工删除或给它配一个节点"
                    ));
                }
            }
        }

        Ok(created)
    }

    /// `add_credential` / `clone_credential` 共用的实现。
    ///
    /// `force_multi_open = true` 表示调用方**已显式声明多开意图**，此时即使
    /// `copies == 1` 也按多开处理（绕去重 / 归组 / 预留序号）。
    async fn add_credential_with_intent(
        &self,
        req: AddCredentialRequest,
        force_multi_open: bool,
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        // 清洗粘贴噪声（移植自 k2cc-proxy）：截取 `ksk_` 起的部分，去掉首尾空白与包裹的引号/逗号。
        // 实测有用户把 `"key: ksk_xxx"` 整段贴进来，导致 region 探测失败并静默落到默认区，
        // 且去重失效（同号可重复导入）。在**最外层入口**规范化，保证去重/探测/落盘拿到同一值。
        // 批量导入（import_one_key → self.add_credential）也走本函数，故单加 + 批量两条路径都覆盖。
        let mut req = req;
        apply_ksk_region_suffix(&mut req);
        req.kiro_api_key = req.kiro_api_key.as_deref().and_then(clean_ksk_api_key);

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
                    req.proxy_username
                        .clone()
                        .filter(|s| !s.is_empty())
                        .or(inline_user),
                    req.proxy_password
                        .clone()
                        .filter(|s| !s.is_empty())
                        .or(inline_pass),
                )
            }
            None => (None, None, None),
        };

        // 份数（多开）：字段缺失 = 普通上号（默认，行为完全不变）。
        // 显式给值时同一账号导入多份，每份自动获得独立 machineId，之后可各自配代理。
        // 上限见 MAX_CREDENTIAL_COPIES 的说明。
        //
        // ⭐ 判据是**归一后的份数 > 1**（`is_multi_open`），不是「字段是否出现」。
        // 先前写成 `req.copies.is_some()`：一个总是下发 `"copies": 1` 的 API 客户端
        // （这是文档里那个"被 clamp 到 [1,16]"字段最自然的读法）就此**永久丢掉去重保护**，
        // 还会得到一个只有 1 个成员的分身组 —— 而 copies=1 的语义明确就是"普通上号"
        // （见 effective_copies：None 与 0 都归一为 1，三者同义）。
        // 下面三处（inherited / clone_group / allow_dup）共用这一个判据，避免再次分叉。
        //
        // `force_multi_open` 是**另一条**入口的显式声明（`clone_credential`：按 id 给
        // 已有号加分身）。它不改变本条推断规则 —— 普通上号路径恒传 false，
        // 故 `copies: 1` 仍然享有去重保护。
        let copies = effective_copies(req.copies);
        let is_multi_open = force_multi_open || copies > 1;

        // 真多开时，从**池中同 key 的既有号**继承请求未指定的关键字段。
        //
        // 🔴 修复的缺陷（线上实测复现）：分身只带 `authMethod` + `kiroApiKey` 时，
        // `apiRegion` 为 None → CLI 端点的 `host()` 是 `q.{api_region}.amazonaws.com`，
        // 拿不到就回退 config 默认（us-east-1）→ 而 `ksk_` token 是**按 region 授权**的
        // → 上游回 403 `AccessDeniedException: The bearer token included in the request is invalid.`
        //
        // 实测对照：父号 `apiRegion=eu-central-1` 成功率 95%，而分身（region 为 None）
        // **0% 成功、100% auth_failed**；补上 region 后同一批分身立刻变成 83/45/100/88%。
        // 此前据此误判成「这个 key 不支持分身」，实际是本层丢了字段。
        //
        // 只继承「与身份/路由相关且分身必须一致」的字段。**刻意不继承**：
        // - `machine_id` —— 分身的核心就是各自独立指纹（入池时自动轮换）
        // - `proxy_*` —— 每份要配不同出口 IP，由调用方逐个设置
        // - `disabled` / `disabled_reason` / `disabled_at` —— 父号被禁不该传染给分身
        let inherited = if is_multi_open {
            req.kiro_api_key
                .as_deref()
                .and_then(|k| self.token_manager.find_credential_by_api_key(k))
        } else {
            None
        };
        let inherit = |mine: Option<String>, pick: fn(&KiroCredentials) -> Option<String>| {
            mine.or_else(|| inherited.as_ref().and_then(pick))
        };

        // 分身组标识：只在真多开（归一后份数 > 1）时赋予，单开保持 None。
        // 判据用 `is_multi_open` 而非 `req.copies.is_some()`：后者会让 `copies: 1`
        // 造出一个只有 1 个成员的分身组，分身管理页上凭空多出一组「独苗」。
        //
        // **优先复用池中同 key 既有号的组** —— 最常见的场景是「这个号已经导过了，
        // 现在给它加 N 个分身」（正是 `allow_dup` 那段注释描述的场景）。若每次都生成新
        // UUID，同一账号会在管理页上裂成两组，而用户看到的是同一个 key。
        // 既有号没有组（多开功能之前导入的）时才新建一个。
        let clone_group = if is_multi_open {
            Some(
                inherited
                    .as_ref()
                    .and_then(|c| c.clone_group.clone())
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            )
        } else {
            None
        };

        // 构建凭据对象
        let email = req.email.clone();
        // `mut`：region 探测（见下方 probe_and_persist_api_region 后那段）需要把探到的
        // api_region 回写进来，供 `for seq in 2..=copies` 的分身继承。
        let mut new_cred = KiroCredentials {
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
            // 新增号白名单：创建表单已探测勾选时直接用；未给（None）则不限制，
            // 上号后仍可经 /credentials/{id}/allowed-models 单独设置。
            // 归一化对齐 set 路径（trim + 去空串 + 空表→None），防空白项 fail-closed。
            allowed_models: req.allowed_models.and_then(|v| {
                let cleaned: Vec<String> = v
                    .into_iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if cleaned.is_empty() {
                    None
                } else {
                    Some(cleaned)
                }
            }),
            tested_models: None,
            // 自定义 API 代挂透传字段（auth_method=custom_api 时由前端填入）。
            base_url: req.base_url,
            api_key: req.api_key,
            model_mapping_exempt: req.model_mapping_exempt,
            request_limit: req.request_limit,
            custom_api_first: req.custom_api_first,
            // ⭐ 三个 region 字段多开时必须继承（见上方 `inherited` 处的长注释）：
            // `api_region` 决定 CLI 端点的 host（`q.{region}.amazonaws.com`），
            // 而 ksk_ token 按 region 授权 —— 丢了它分身 100% 拿 403 bearer token invalid。
            region: inherit(req.region, |c| c.region.clone()),
            auth_region: inherit(req.auth_region, |c| c.auth_region.clone()),
            api_region: inherit(req.api_region, |c| c.api_region.clone()),
            // machine_id **刻意不继承**：分身的核心是各自独立指纹。
            // 传 None 让入池逻辑按 key 派生（确定性）→ 与父号撞车 → 自动轮换成独立随机值。
            machine_id: req.machine_id,
            email: req.email,
            name: req.name,
            clone_group: clone_group.clone(),
            // 本份的序号在 `copies` 已知后才能定（要接着组内既有的最大值编号），
            // 故此处留 None，由下方多开段统一回填。
            clone_seq: None,
            tag: req.tag.clone(),
            // 订阅档位多开时继承：它是 opus 过滤的门控，父号已探到就不必再打一次
            // web_portal（那是上游探测，会加重风控）。非多开时保持 None（首次用量查询自动填）。
            subscription_title: inherited
                .as_ref()
                .and_then(|c| c.subscription_title.clone()),
            proxy_url,
            proxy_username,
            proxy_password,
            // 透传调用方意图：未指定时 serde default = false（新号默认启用，旧行为不变）。
            // 指定 true 用于重新导入已知被封的号——见 AddCredentialRequest::disabled 的说明。
            disabled: req.disabled,
            disabled_reason: None,
            disabled_at: None,
            quota_exhausted_at: None,
            kiro_api_key: req.kiro_api_key,
            endpoint: req.endpoint,
            // 新号默认跟随全局 `config.cliOriginKiroCli`（None）；本条为单号 A/B 排查
            // 新增的旁路开关，尚未接入面板"新增凭据"表单，与既有面板行为零变化。
            cli_origin_kiro_cli: None,
        };

        // ⭐ OAuth 号不许多开：第 2..N 份带着同一个 refreshToken，而它每次刷新都被上游
        // 轮换 → 除先刷新的那一份外全部 invalid_grant 被禁用。理由详见
        // `multi_open_rejection_reason`。放在这里（构造完 new_cred、入池之前）：
        // 判据要看归一后的字段（authMethod / kiroApiKey），且必须在任何写入之前。
        if is_multi_open && let Some(reason) = multi_open_rejection_reason(&new_cred) {
            return Err(AdminServiceError::InvalidCredential(reason));
        }

        // ⭐ 节点分配计划必须在**第 1 份入池之前**算出来。
        //
        // 🔴 修复的缺陷：先前这段在 `copies > 1` 的块里（即第 1 份已经入池之后），
        // 于是第 1 份**永远拿不到节点**。原注释把这写成一条刻意取舍（"它可能是池里已有
        // 的号，覆盖会把在跑的号的出口换掉"），但那个理由只在「给已有号追加分身」时成立；
        // 「选凭据生成分身」这条路建的是**全新条目**、`proxy_url` 为空，覆盖不掉任何东西。
        // 实测：池里 5 个全启用、一次 copies=4，只有第 2/3/4 份拿到节点，主份裸连，
        // 两个节点闲置 —— 而用户以为 4 份都分散了。
        //
        // 判据因此从「是不是第 1 份」改成「**这一份当前有没有代理**」：
        // - 已有 `proxy_url` → 绝不覆盖（这才是那条注释真正要保护的东西）
        // - 为空 → 从计划里取
        // 主份本来就配了代理时行为与修复前完全一致（零回归）。
        //
        // 另两条取舍原样保留：**节点不足不轮询复用**（`assignments` 取完即止）、
        // **调用方显式给了 proxy_url 时完全不介入**（下面这个 `is_none()` 门）。
        //
        // 「这一份有没有代理」——全部份共享同一个基线（第 2..N 份是 `new_cred.clone()`），
        // 故这一个判断对所有份等价。**必须在主份点名节点之前算**：点名会写进
        // `new_cred.proxy_url`，之后再读就恒为 false，第 2..N 份继承来的代理就不会被清 →
        // 全部份共用主份那个出口（比直连更糟：直连至少看得出来没分散）。
        let pool_may_assign = new_cred.proxy_url.is_none();

        // 主份点名节点（对话框「出口 IP → 从池中选」）：解析成 (url, user, pass) 写进主份。
        // 必须服务端按 id 解析 —— 节点密码从不下发前端，前端只有 hasPassword 布尔。
        //
        // `proxy_url` 已给时忽略本字段（显式 URL 是更强的意图），与 `pool_may_assign`
        // 那道门同一原则。
        let mut primary_pinned_node: Option<u64> = None;
        if pool_may_assign && let Some(nid) = req.primary_node_id {
            let picked = {
                let nodes = self.socks_nodes.lock();
                match nodes.iter().find(|n| n.id == nid) {
                    // 已禁用也要拦：否则「禁用节点」这个开关在这条路上形同不存在。
                    Some(n) if !n.enabled => Err("已禁用"),
                    Some(n) => Ok((n.url.clone(), n.username.clone(), n.password.clone())),
                    None => Err("不存在"),
                }
            };
            match picked {
                Ok((url, user, pass)) => {
                    new_cred.proxy_url = Some(url);
                    new_cred.proxy_username = user;
                    new_cred.proxy_password = pass;
                    primary_pinned_node = Some(nid);
                }
                // ⭐ 400 而不是"静默直连"或"静默换一个"：这是用户刚在下拉里点的节点，
                // 两种静默都会让他以为出口是他选的那个（见 node_ids 的同款理由）。
                Err(why) => {
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "指定的出口节点 #{nid} {why}，已中止（**不会**静默改成直连或别的节点，\
                         否则出口就不是你选的那个）。请在「分身管理」里确认该节点后重试。"
                    )));
                }
            }
        }

        // 主份要不要参与池分配（4.1 的开关）。
        //
        // 缺省（`None`）按**份数**决定，而不是一律 false：
        // - `copies == 1` → true。只有一份，池节点没有别的去处；此时"关掉"等于让
        //   自动分配无处可去（下拉旁那个「自动分配」按钮就白点了）。
        // - `copies > 1` → false（用户拍板的默认）。主份是用户亲手提交的那一条，
        //   它的出口由表单里的「出口 IP」决定；池节点全部让给第 2..N 份，于是
        //   `copies=N` 只需 **N-1** 个节点。
        //
        // `clone_credential` 这条路显式传 `Some(true)`：那里父号一字节不动，
        // "主份"其实是本次新建的第 1 个分身，与其余份完全同质 —— 让它独独裸连而池里
        // 空着一个节点，正是 2026-08-05 修掉的那个缺陷（见 `CloneCredentialRequest`
        // 上的说明）。所以**这个默认值不改变既有那条路的行为**。
        //
        // 主份已点名节点时它已经有出口了，不再参与池分配（否则会被计划里的第 0 个顶掉）。
        let assign_primary =
            primary_pinned_node.is_none() && req.assign_primary_node.unwrap_or(copies == 1);

        // 计划要留几个节点 = 本次真会去消费节点的份数。主份不参与时是 `copies - 1`；
        // 传 `copies` 会多留一个永不被消费的节点，`rejected` 文案跟着不准。
        let node_cap = if assign_primary {
            copies as usize
        } else {
            copies.saturating_sub(1) as usize
        };
        // 非多开路径（普通上号）完全不进这里：计划恒空，否则每次上号都会被悄悄塞一个池节点。
        // 例外是**调用方显式表达了池分配意图**（`assignPrimaryNode=true` / `nodeIds`），
        // 否则「上号对话框选了池节点」这条路会变成一个静默无效的控件。
        // （`primaryNodeId` 不在此列：它已在上面直接写进主份，不需要计划。）
        let primary_intent = req.assign_primary_node == Some(true)
            || req.node_ids.as_ref().is_some_and(|v| !v.is_empty());
        let node_plan = if is_multi_open || primary_intent {
            self.resolve_node_plan(req.node_ids.as_deref(), node_cap, primary_pinned_node)
        } else {
            NodePlan::default()
        };

        // 4.4 严格模式：凑不齐「每份一个独立节点」就整个请求失败，**一份也不建**。
        //
        // 位置是承重的：必须在 `reserve_clone_seqs`（下一段）与第 1 份入池之前。
        // 放到入池之后就变成"建了一半再报错"，而放到号段预留之后会白烧掉一段组内序号
        // → 分身管理页上出现永久空洞（#1 #2 #3 #7 #8），且每次重试都再烧一段。
        //
        // 只在池真的要介入时才判：`pool_may_assign` 为假（调用方显式给了 proxy_url）
        // 时本次压根不走池分配，此时报"节点不够"是无中生有。
        if req.require_node_per_copy == Some(true) && pool_may_assign && node_cap > 0 {
            let have = node_plan.assignments.len();
            if have < node_cap {
                let rejected_note = if node_plan.rejected.is_empty() {
                    String::new()
                } else {
                    let list = node_plan
                        .rejected
                        .iter()
                        .map(|(id, why)| format!("#{id}（{why}）"))
                        .collect::<Vec<_>>()
                        .join("、");
                    format!("（其中被跳过的：{list}）")
                };
                // 主份点名了节点 / 开关关着时，主份不消耗计划里的节点，故"能建几份"
                // 要把它加回去（`have + 1`），否则建议值会少一份。
                let primary_desc = if assign_primary {
                    "参与分配"
                } else {
                    "不参与分配"
                };
                let suggest = if assign_primary { have } else { have + 1 };
                return Err(AdminServiceError::InvalidCredential(format!(
                    "节点不足：本次需要 {node_cap} 个可用节点（{copies} 份，主份{primary_desc}），\
                     实际只有 {have} 个{rejected_note}。已**不建任何份**并中止 —— \
                     绝不让多份共用同一个出口：那等于没分散，却让人以为分散了。\
                     请先在「分身管理」里加节点/启用节点/重测失败节点，或把份数降到 {suggest}。"
                )));
            }
        }

        // ⭐ 组内序号**在任何 await 之前一次性全额预留**（见 `reserve_clone_seqs` 文档）。
        //
        // 🔴 修复的并发缺陷：先前是「入池后读一次 max 给第 1 份、循环前再读一次当基准」，
        // 而这两次读之间、以及循环内每份入池都横跨 `.await`。两个并发的「给同一个 key
        // 加 N 份」请求（两个面板标签页 / 脚本重试）会各自读到同一个 max，于是同一组里
        // 出现两个 `#1`、两个 `#2` …… 管理页无法区分，删除时也无法指名。
        //
        // 预留放在这里的三个理由：
        // - 在 OAuth 拒绝判断**之后** —— 被拒的请求不该白占号段（那会在组内留下永久空洞）。
        // - 在**节点不足**判断之后 —— 同上，严格模式失败时不该烧号段。
        // - 在第 1 份入池（下方 `.await`）**之前** —— 号段一旦发出就与入池进度无关，
        //   这正是消除竞态的关键；放到入池之后就等于把竞态窗口原样留着。
        let clone_seq_start = clone_group
            .as_deref()
            .map(|g| self.token_manager.reserve_clone_seqs(g, copies))
            .unwrap_or(0);

        let mut assigned_nodes = 0usize;
        // 第 1 份：开关开着、有计划、且它本来没代理时才写。
        if assign_primary
            && pool_may_assign
            && let Some((url, user, pass)) = node_plan.assignments.first()
        {
            new_cred.proxy_url = Some(url.clone());
            new_cred.proxy_username = user.clone();
            new_cred.proxy_password = pass.clone();
            assigned_nodes += 1;
        }

        // **归一后份数 > 1** 决定第 1 份要不要走去重，这是刻意的语义：
        //
        // - 份数为 1（普通上号，含字段缺失 / `0` / `1`）→ 走正常去重，
        //   误双击与「总是下发 copies:1 的客户端」都仍被 `凭据已存在` 拦住。
        // - 份数 > 1（真多开）→ **全部份都绕过去重**。
        //
        // 为什么第 1 份也要绕：最常见的多开场景是「这个号已经导过了，现在给它加 N 个分身」。
        // 若第 1 份仍走去重，它会撞上 `凭据已存在（kiroApiKey 重复）` 并让整个请求失败，
        // 于是**给已有号加分身这条路根本走不通**（实测：#419/#420 已在池中，
        // 请求 copies=4 会在第 1 份就 bail，一个分身也建不出来）。
        //
        // 绕过在此处是安全的：份数 >1 本身就是调用方声明了多开意图，误双击上号绕不到
        // 这里（前端只在份数 >1 时才下发该字段，见 add-credential-dialog.tsx）。
        let allow_dup = is_multi_open;

        // ⭐ 探测窗口保护（M9）：要探测的号以**临时禁用态**入池，探测完成前调度器不碰它。
        //
        // 对齐同文件 `import_one_key`（disabled 随请求入池）与 `clone_credential`
        // （「必须在入池时就是 disabled，不能先建后批量禁用」）的**先禁后建**实践。
        // 下方 `probe_and_persist_api_region` 是 1-2s 的真实上游往返；若号以启用态入池，
        // 窗口期真实流量会打到错区（`api_region` 还没写死，回退 `config.region`）恒 403，
        // `MAX_FAILURES_PER_CREDENTIAL=3` 三次即自动禁用 —— 号在自己 region 被探出来
        // 之前就死了（线上事故 #536-550，见下方 P0 块的完整描述）。
        //
        // 判据镜像 `token_manager::needs_api_region_probe`（api_key 号 + region 三字段
        // 全空 + 非 custom_api）—— 该函数是私有的，此处是唯一镜像点，**改判据必须
        // 两边同步**（分叉的最坏后果是回到「启用态入池」的现状，不会更糟，但窗口保护
        // 就失效了）。带 region / OAuth / custom_api 号 will_probe=false，行为零变化。
        //
        // ⚠️ 写成布尔表达式而非 `if will_probe` 里的字面量赋值：
        // `account_throttled_must_not_disable_credential` 守卫用「函数体第一处
        // `new_cred.disabled = true` 字面量」定位失败处置分支；前置禁用若也用同一
        // 字面量，守卫的切片锚点会错位（详见那条守卫的文档）。
        let will_probe = needs_probe_window_guard(&new_cred);
        let orig_disabled = new_cred.disabled;
        new_cred.disabled = orig_disabled || will_probe;

        let credential_id = if allow_dup {
            self.token_manager
                .add_credential_allowing_duplicate(new_cred.clone())
                .await
        } else {
            self.token_manager.add_credential(new_cred.clone()).await
        }
        .map_err(|e| self.classify_add_error(e))?;

        // ⭐ region 自动探测：把该 `ksk_` 号真正可用的 region 写死进凭据。
        //
        // 必须在下面那次 `get_usage_limits_for` **之前** —— 那一次就是打
        // `management.{region}.kiro.dev`，region 错的话它自己也会 403，
        // 于是「订阅等级探测失败」只是错配的第一个受害者。
        //
        // 为什么必须有这一步（`region_probe.rs` 模块注释有完整实测表）：
        // `ksk_` token 是**按 region 授权**的，打错区上游恒回 403
        // `AccessDeniedException: The bearer token ... is invalid`。而不带任何 region
        // 字段的凭据会一路回退到 `config.region`，于是它对不对纯靠运气。
        // 实测无 region 号的「上号即废率」08-02=0% / 08-03=27% / 08-04=30%，正在恶化。
        //
        // 只探「api_key 且完全没有任何 region 字段」的号（判据在 probe_api_region 内），
        // 已显式带 region 的是调用方的明确意图，绝不覆盖。
        //
        // ⭐ 判决必须接住（P0）：探不出可用 region 的号**保持禁用**，见下方处置。
        let probe_outcome = self
            .token_manager
            .probe_and_persist_api_region(credential_id)
            .await;

        // 🔴 探测结果必须**回写进 `new_cred`**，否则下方 `for seq in 2..=copies` 里的
        // `new_cred.clone()` 克隆的是**探测前的过期副本**（`api_region` 仍为 None）。
        //
        // 实测事故（2026-08-05 02:42，本行加入前）：父号 #525 被探测写上
        // `eu-central-1` 并 95% 成功，而同批 4 个分身 #526–529 全部 `api_region=None`
        // ⇒ 回退 `config.region=us-east-1` ⇒ `ksk_` 按区授权 ⇒ 恒 403
        // `bearer token invalid` ⇒ 24 秒内三次失败全部被禁用、0% 成功。
        //
        // ⚠️ 这个缺陷是**接入探测才引入的**：探测之前父子都没有 region、一起废（症状一致，
        // 一眼能看出是 region 问题）；接入之后变成「父好子坏」，反而更容易被误判成
        // 「这个 key 不支持分身」。所以回写不是优化，是这条路径的正确性前提。
        if let Some(probed) = self
            .token_manager
            .export_credential(credential_id)
            .and_then(|c| c.api_region)
        {
            if new_cred.api_region.as_deref() != Some(probed.as_str()) {
                tracing::info!(
                    "分身继承：把父号 #{} 探测到的 api_region={} 回写进本次请求，供后续 {} 份分身继承",
                    credential_id,
                    probed,
                    copies.saturating_sub(1)
                );
                new_cred.api_region = Some(probed);
            }
        }

        // ⭐ P0：探不出可用 region 的号**保持禁用**，且让分身一并继承禁用态。
        //
        // 线上事故（2026-08-05 05:41–05:43）：#536–550 共 15 个号以**启用态**入池，
        // 探测要 1~2 秒，窗口里真实流量打到错区恒回 403，`MAX_FAILURES_PER_CREDENTIAL=3`
        // 三次即自动禁用 ⇒ 每个号只跑了 1~6 个请求、**0 成功**就死了。而 4 分钟后
        // 同一批 key 的 #551–556 探到 `eu-central-1`，其中一个跑到 881/881 全成功。
        // 差别只有几百毫秒的时序。
        //
        // 窗口期的死亡竞态由上方入池前的临时禁用（`will_probe` 块）消除：探测期间
        // 号不可被调度，窗口内不存在「真实流量打到错区」。这里只负责**探测之后的
        // 收尾**：失败（NoUsableRegion/TokenDead）保持禁用，其余结论恢复启用。
        //
        // ⚠️ **分身必须一并禁用**：分身继承父号的 `api_region`（上方回写块），父号探不到时
        // 它们继承到的是 `None` ⇒ 回退 `config.region` ⇒ 与父号同样恒 403。
        // 历史事故正是这个形态（父号 #525 探到 eu-central-1 而 4 个分身 api_region=None，
        // 24 秒内全部被禁用）。所以这里改 `new_cred.disabled`，让下方
        // `for seq in 2..=copies` 里的每一份 `new_cred.clone()` 都带上禁用态 ——
        // 而不是建完再批量禁用（那有中间窗口，分身会先接一波流量）。
        let region_probe_failed = matches!(
            probe_outcome,
            crate::kiro::region_probe::ProbeOutcome::NoUsableRegion
                | crate::kiro::region_probe::ProbeOutcome::TokenDead
        );
        // custom_api 不属于 Kiro region 体系：即使未来探测层误返了失败判决，
        // 这一层也必须保持管理员的 enabled 状态，不得让代挂站自动关闭。
        if region_probe_failed && !new_cred.is_custom_api_credential() {
            self.token_manager
                .mark_region_probe_failed(credential_id, &probe_outcome);
            new_cred.disabled = true;
        } else if will_probe && !orig_disabled {
            // 探测已得出结论（Usable / Skipped / AccountThrottled 三者都不判死）：
            // 把入池前临时禁用的第 1 份恢复为请求的原启用意图，分身（克隆
            // `new_cred`）一并恢复。恢复失败只告警不阻断 —— 号停留在临时禁用态
            // 是安全的一侧（绝不比「错误启用」更糟），但需要人工捞。
            if let Err(e) = self.token_manager.set_disabled(credential_id, false) {
                tracing::warn!(
                    credential_id,
                    "region 探测后恢复凭据启用失败（号停留在临时禁用态）: {e}"
                );
            }
            new_cred.disabled = false;
        }

        // ⭐ 账户级临时风控（403 `TEMPORARILY_SUSPENDED`）挡住了探测：**刻意不禁用**。
        //
        // 这一条与上面的 `region_probe_failed` 是两种完全不同的结论，必须分开处置：
        // - `NoUsableRegion` = 探过了、确定不行 → 禁用是对的
        // - `AccountThrottled` = **探不了**（风控挡在 region 授权校验之前，拿不到任何 region 信息）
        //
        // 为什么不禁用（这条是承重的，改成禁用会造成真实损失）：
        // ① `ids_needing_region_probe`（token_manager.rs 内）过滤 `!e.disabled` ——
        //    一旦禁用，**连重启时的存量回填都不再重探**它，风控过去了也永远不会自愈。
        // ② 不禁用的最坏态只是退回「探测接入前的基线」（api_region=None → 回退 config.region
        //    轮盘）；而若真打错区，会走 `report_failure` → `TooManyFailures`，
        //    **那个原因在 `is_self_healable_reason` 白名单里**，是可自愈的。
        //    即不禁用的最坏态严格优于禁用（后者是需人工的永久态）。
        //
        // 事故背景：这类 403 占近 2h 流量 22.3%（CLAUDE.md），是常态不是罕见；而
        // `MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE = 6` 存在的唯一理由就是
        // 「见过一次 403 不足以判死」—— 探测路径若用一次 403 就判死，等于绕过那道阈值。
        let region_probe_throttled = matches!(
            probe_outcome,
            crate::kiro::region_probe::ProbeOutcome::AccountThrottled
        );
        if region_probe_throttled {
            tracing::warn!(
                credential_id,
                "region 探测被账户级临时风控挡住，未能确定 region：该号以 config.region 回退入池\
                 （**未禁用**，见此处注释）。风控过去后重启会由存量回填自动重探；\
                 若急需确定 region，可在面板手动设置 apiRegion"
            );
        }

        // 主动获取订阅等级，避免首次请求时 Free 账号绕过 Opus 模型过滤。
        //
        // 探测失败时跳过：这一次打的正是 `management.{region}.kiro.dev`，region 都没探到
        // 就必然 403 —— 白付一次上游往返，而上号是用户交互路径（面板要多转一次圈）。
        // 订阅档位留 None 是安全的：`supports_opus()` 对 None 返 true（不误挡），
        // 人工确认 region 并启用后，首次真实使用会自动补齐。
        //
        // ⭐ `AccountThrottled` 同样跳过，理由同源且更强：号正被账户级风控挡着，
        // 打 `management.*` 查订阅等级**同样会 403** —— 白付一次上游往返，而这是用户交互路径。
        if region_probe_failed || region_probe_throttled {
            tracing::info!(
                credential_id,
                throttled = region_probe_throttled,
                "region 探测未得出结论，跳过订阅等级探测（同一 region host 必然 403）"
            );
        } else if let Err(e) = self.token_manager.get_usage_limits_for(credential_id).await {
            tracing::warn!("添加凭据后获取订阅等级失败（不影响凭据添加）: {}", e);
        }

        // 新号自动初始化(异步,不阻塞响应):刷 token + 解析 profileArn，根治上号初期查余额 403(#89)。
        // 门控在 spawn_initial_refresh 内部(custom_api/api_key 自动跳过)。
        self.token_manager.spawn_initial_refresh(credential_id);

        let mut credential_ids = vec![credential_id];

        // 回填第 1 份的组内序号。用**预留号段的首号**，绝不在这里重新扫 max ——
        // 重新扫就是那条并发缺陷本身（此刻其它请求的份可能还没落进 entries）。
        if let Some(ref group) = clone_group {
            if let Err(e) = self.token_manager.set_clone_identity(
                credential_id,
                Some(group.clone()),
                Some(clone_seq_start),
            ) {
                tracing::warn!("回填分身序号失败（不影响凭据可用性）: {}", e);
            }
        }

        if copies > 1 {
            // 订阅档位**只探一次**再透给其余份。
            //
            // 为什么不让每份各自调 `get_usage_limits_for`：那是一次真实的上游
            // `web_portal` 往返，而 web_portal 属上游探测、调多了会加重风控
            // （线上调优结论：绝不为展示类需求每请求打它）。N 份是**同一个账号**，
            // 档位必然相同，所以复制第 1 份解析出的值即可。
            //
            // 若第 1 份没探到（网络失败等），保持 None —— `supports_opus()` 对 None
            // 返回 true（不误挡），首次真实使用时会自动补齐，不会因此漏过 Free 档过滤。
            let resolved_title = self
                .token_manager
                .export_credential(credential_id)
                .and_then(|c| c.subscription_title);

            // 第 2..N 份的节点：计划里**去掉已给第 1 份的那个**之后的尾巴。
            //
            // 这是节点池唯一的消费方 —— 没有它，节点表就是一张没人读的表
            // （加了节点、建了分身，每份仍然直连、共用服务器同一个出口 IP，
            // 而用户以为已经分散了）。
            //
            // 切尾巴而不是直接按 `seq-1` 索引整份计划：第 1 份可能因为已有 `proxy_url`
            // 而没消费计划里的第 0 个（见上方 `pool_may_assign`），此时那个节点应当
            // 顺延给第 2 份，而不是空掉。故尾巴的起点取决于第 1 份实际消费了几个。
            //
            // 两条刻意的取舍原样保留（第三条"第 1 份不分配"已被修掉，见上方长注释）：
            // - **节点不足时不轮询复用**：复用同一节点的两份共用一个出口 IP，
            //   等于没分散，却让人以为分散了。宁可让多出来的份直连并在响应里明说。
            // - **调用方显式给了 proxy_url 时完全不介入**：那是明确意图，优先于池分配。
            let assignable: &[(String, Option<String>, Option<String>)] = if pool_may_assign {
                node_plan.assignments.get(assigned_nodes..).unwrap_or(&[])
            } else {
                &[]
            };
            for seq in 2..=copies {
                let mut copy = new_cred.clone();
                copy.subscription_title = resolved_title.clone();
                // 组内序号逐份递增，全部取自**本次预留的那一段**。`seq` 是本次请求内的
                // 份号（2..=copies），而落盘的是**组内**序号 —— 两者在「给已有组加分身」
                // 时不相等，不能混用。
                copy.clone_seq = Some(clone_seq_start + seq - 1);
                // ⚠️ 先清掉从 `new_cred` 继承来的代理，再按计划写本份的。
                // 不清就会**继承第 1 份的出口** —— 那正是"两份共用一个出口 IP"，
                // 比直连更糟（直连至少看得出来没分散）。
                // `pool_may_assign` 为假（调用方显式给了代理）时不清，那是明确意图。
                if pool_may_assign {
                    copy.proxy_url = None;
                    copy.proxy_username = None;
                    copy.proxy_password = None;
                }
                // 逐份取一个不同节点；取完即止（不复用，见上）。
                let picked = assignable.get(seq as usize - 2);
                if let Some((url, user, pass)) = picked {
                    copy.proxy_url = Some(url.clone());
                    copy.proxy_username = user.clone();
                    copy.proxy_password = pass.clone();
                }
                // machineId 置 None：让入池逻辑按 kiroApiKey/refreshToken 派生 —— 派生是
                // 确定性的，故与第 1 份撞车，随后被撞车检测轮换成独立随机指纹（防关联）。
                // 这正是"每份机器码不同"的来源，不需要调用方自己造。
                copy.machine_id = None;
                match self
                    .token_manager
                    .add_credential_allowing_duplicate(copy)
                    .await
                {
                    Ok(id) => {
                        self.token_manager.spawn_initial_refresh(id);
                        credential_ids.push(id);
                        // ⭐ 计数在**入池成功之后**才加。
                        //
                        // 修的是一处真实的文案偏差：先前 `assigned_nodes += 1` 紧跟在
                        // 赋值处（入池之前），第 2..N 份入池失败时 `credential_ids`
                        // 不增长而计数已增长 → 下方 `unassigned` 的两次
                        // `saturating_sub` 把差额吃成 0 → 响应声称「已为 N 份分配独立
                        // 出口 IP」，而那 N 份里有几份根本没建出来。
                        if picked.is_some() {
                            assigned_nodes += 1;
                        }
                    }
                    // 部分失败不回滚已建成的份：与 `import/keys` 的既有约定一致
                    // （部分失败仍返回成功并逐条标记）。回滚反而会把第 1 份也删掉，
                    // 而那一份是通过了去重校验的正常号。
                    Err(e) => {
                        tracing::warn!(
                            "多开第 {}/{} 份添加失败（已成功 {} 份，不回滚）: {}",
                            seq,
                            copies,
                            credential_ids.len(),
                            e
                        );
                    }
                }
            }
        }

        // ⭐ 同 key（= 同一个上游账号）的**其它**凭据。一次查、两处用：告警 + 组标识回填。
        //
        // 🔴 判据必须是 **key** 而不是 `clone_group`。线上实测的那组数据就是反例：
        // `#776` keyHash=029fdd8929、**无 clone_group、无代理**，`#778–787` 同 key 同组
        // 各有独立 SOCKS —— 11 份共用一个上游账号，其中 1 份从服务器裸 IP 出去。
        // 按组去找同账号成员，漏掉的恰好是最该被看见的那一份（组标识是后来加分身才有的，
        // 最先入池的那一份天然没有）。这就是本缺陷能长期存活的原因。
        //
        // ⚠️ 顺序是承重的：**必须在下面那段回填之前**取名单。回填会把父号补进组里，
        // 之后再查就无法区分「按 key 查」与「按组查」了 —— 于是有人把判据改坏也测不出来。
        let same_key_peers = new_cred
            .kiro_api_key
            .as_deref()
            .filter(|_| is_multi_open)
            .map(|k| self.token_manager.peers_sharing_api_key(k, &credential_ids))
            .unwrap_or_default();

        // ⛔ 只告警，**绝不**给这些号写 `proxy_url`。
        //
        // 用户已就此拍板：`proxy_url` 是**用户的显式配置**，「没有代理」也可能是一个刻意的
        // 状态（比如留一份直连做对照）。本仓一贯范式（`effective_endpoint` / `api_region`
        // 都是这样）：显式配置优先，绝不擅自覆盖。所以这里给的是判据 + 后果 + 处置建议，
        // 由人来决定。
        //
        // 为什么这值得占一句响应文案：同账号流量集中在少数 IP 上会被上游按账号关联。
        // 同一天的实测：克隆某号 10 份并全部启用，**15 分钟后**父号连同 10 份分身
        // （线上 `[749, 766-775]` 共 11 个）全部被 `suspiciousActivityAuto` 禁用。
        // 而这里的形态更隐蔽 —— 10 份有独立 IP、1 份没有，代理存在的意义在那一份上是空的，
        // 而面板上它长得和别的份一样。
        let bare_exit_peers: Vec<u64> = same_key_peers
            .iter()
            .filter(|p| !p.has_own_exit)
            .map(|p| p.id)
            .collect();

        // ⭐ 给缺 `clone_group` 的同 key 成员回填组标识（用户已同意）。
        //
        // 与上面「不改父号代理」不矛盾，两者性质不同，这个区别必须写下来，否则将来会有人
        // 照着本段的先例去改 `proxy_url`：
        // - `clone_group` 是**系统内部的分组标识**，没有语义选择余地 —— 那个号确实属于
        //   这个账号组，缺组只是「它入池时本字段还不存在」的历史债。
        // - `proxy_url` 是**用户的显式配置**，有语义选择余地（直连也可能是刻意的）。
        //
        // 只写 `clone_group`：`clone_seq` 原样带回（老成员多为 `None`，本次不给它编号 ——
        // 编号要走 `reserve_clone_seqs` 发号，在这里凭空塞一个会与组内既有号撞车）。
        //
        // ⚠️ 前端 `clone-management-card.tsx::groupClones` 那套 `apiKeyHash` 回落分组
        // **本轮刻意不动**：线上还有历史数据一个组标识都没有（该文件注释记载：回收站 349 条
        // 里 23 组 / 65 个凭据属于老数据，其中一组 9 份），删了它们的分组关系当场丢失。
        // 本段只让**新产生的**数据不再欠这笔债。回落逻辑可以退役的判据写在那个函数的注释里。
        for peer in same_key_peers.iter().filter(|p| p.clone_group.is_none()) {
            if let Some(ref group) = clone_group {
                if let Err(e) = self.token_manager.set_clone_identity(
                    peer.id,
                    Some(group.clone()),
                    peer.clone_seq,
                ) {
                    tracing::warn!(
                        "给同 key 的凭据 #{} 回填分身组失败（不影响本次新建的份）: {}",
                        peer.id,
                        e
                    );
                }
            }
        }

        let created = credential_ids.len();
        let message = if is_multi_open {
            // 如实报告节点分配结果：分了几份、还有几份直连、指定了哪些无效 id。
            // 「加了节点却仍然直连」是这条路最容易踩空又最难自查的地方，
            // 必须在响应里说清，而不是让用户逐个点开卡片才发现。
            //
            // ⚠️ 基数是「本次**该**去消费节点的份数」，不是 `created`，也不再是 `created - 1`：
            // - 主份参与分配（`assignPrimaryNode` 开 / clone 路径）→ 基数 = `created`。
            //   写死 `created - 1` 会让「2 份 2 节点」这种全额分配报成"有 1 份直连"。
            // - 主份不参与（`POST /credentials` 的缺省）→ 基数 = `created - 1`。
            //   写死 `created` 会把**按设置刻意直连**的主份算进"因启用节点不足而直连"，
            //   那是一句假归因：用户明明凑齐了 N-1 个节点，却被告知节点不够。
            let pool_targets = if assign_primary {
                created
            } else {
                created.saturating_sub(1)
            };
            let unassigned = pool_targets.saturating_sub(assigned_nodes);
            // 主份刻意不参与时必须单独说一句，否则用户看不出「为什么主份没有出口」
            // 到底是设置如此还是节点不够 —— 这两件事的处理方式完全不同。
            let primary_note = match primary_pinned_node {
                Some(nid) => format!(
                    "；主份走你点名的节点 #{nid}（未参与池分配，故第 2..N 份只需 {} 个节点）",
                    copies.saturating_sub(1)
                ),
                None if !assign_primary && pool_may_assign && copies > 1 => format!(
                    "；主份按「主份也从池取节点=关」保持自身出口（未参与池分配，\
                     故本次只需 {} 个节点）",
                    copies.saturating_sub(1)
                ),
                None => String::new(),
            };
            let proxy_note = if assigned_nodes == 0 {
                // 主份点名了节点时不能说"未从节点池分配代理"——那是假的（它就来自池）。
                // 这里说的是**第 2..N 份**一个都没分到。
                let subject = if primary_pinned_node.is_some() || !assign_primary {
                    "第 2..N 份未从节点池分配代理"
                } else {
                    "未从节点池分配代理"
                };
                format!(
                    "{subject}（池内无启用节点/最近测活失败，或已显式指定代理）——\
                     各份将共用服务器同一出口 IP，如需分散请在「分身管理」里添加节点后重建分身{primary_note}"
                )
            } else if unassigned == 0 {
                format!("已从节点池为 {assigned_nodes} 份分配独立出口 IP{primary_note}")
            } else {
                format!(
                    "已从节点池为 {assigned_nodes} 份分配独立出口 IP；\
                     另有 {unassigned} 份因启用节点不足而直连（刻意不复用节点：\
                     复用等于共用出口，反而掩盖问题）{primary_note}"
                )
            };
            // 指定了却用不上的 node id 必须逐条点名。静默吃掉它们的话，用户看到的是
            // 「我明明选了节点，怎么还是直连」——而这正是最容易踩空的那一步。
            let rejected_note = if node_plan.rejected.is_empty() {
                String::new()
            } else {
                let list = node_plan
                    .rejected
                    .iter()
                    .map(|(id, why)| format!("#{id}（{why}）"))
                    .collect::<Vec<_>>()
                    .join("、");
                format!(
                    "；⚠️ 指定的节点 {list} 未生效，已跳过（**不会**静默替换成别的节点，\
                     否则出口就不是你选的那个）"
                )
            };
            // 同账号里有份走服务器裸 IP —— 必须点名，且必须说清「本次没动它」。
            // 不点名的话，用户在面板上看到的是「N 份都有 socks」，那一份长得和别的一样
            // （它甚至可能被显示成组里的最后一份），而它把整组的账号关联度拉满了。
            let bare_exit_note = if bare_exit_peers.is_empty() {
                String::new()
            } else {
                let list = bare_exit_peers
                    .iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join("、");
                format!(
                    "；🔴 同一把 key 的凭据 {list} **没有独立出口**（proxyUrl 为空或 direct，\
                     即走服务器裸 IP），而它与本组共用同一个上游账号 —— \
                     同账号流量集中在一个 IP 上会被按账号关联风控（实测：一次克隆 10 份并全部启用，\
                     15 分钟后父号连同 10 份分身全部被 suspiciousActivity 自动禁用）。\
                     **本次未改动它的代理设置**（那是显式配置，不擅自覆盖）：\
                     请在「分身管理」里给它配一个节点，或确认它就是要走服务器出口\
                     （比如刻意留一份做对照）"
                )
            };
            format!(
                "凭据添加成功（多开 {}/{} 份），ID: {:?}。每份已分配独立 machineId；{}{}{}。\
                 注意 rpmLimit 是每凭据的，多份共用同一账号配额，\
                 建议按账号实测上限 ÷ 份数逐号调整。",
                created, copies, credential_ids, proxy_note, rejected_note, bare_exit_note
            )
        } else {
            format!("凭据添加成功，ID: {}", credential_id)
        };

        Ok(AddCredentialResponse {
            success: true,
            message,
            credential_id,
            credential_ids: if is_multi_open {
                Some(credential_ids)
            } else {
                None
            },
            email,
        })
    }

    /// 批量导入 Kiro API Key（`ksk_` 号）。
    ///
    /// **有界并发**：单条失败只记该条 `error`，不中断整批（HTTP 层统一 200，
    /// 前端逐条看 `ok`）。**响应与日志只出现脱敏 Key**。
    ///
    /// 为什么必须有界而不是串行、也不是全量并发：
    /// [`Self::add_credential`] 内部会调 `get_usage_limits_for`（一次上游网络往返，
    /// 见 service.rs:983），所以单条耗时以网络 RTT 为主而非 CPU。
    /// - 串行：N 条 = N × RTT，导入 100 个号要等到超时。
    /// - 全量并发：N 条同时打上游，既易触发上游风控，又让 `add_credential` 里的
    ///   写盘持久化产生 N 路争抢。
    /// 故以信号量把在飞数量压在 [`IMPORT_MAX_IN_FLIGHT`]，形成稳定的流水线队列：
    /// 任一条完成即放行下一条，总耗时 ≈ ceil(N / 并发) × RTT，且上游压力恒定可控。
    ///
    /// **结果顺序与输入严格一致**：并发完成顺序是乱的，故按下标回填而非 push，
    /// 让调用方能用 `results[i]` 直接对上 `items[i]`。
    ///
    /// `concurrency_limit`（请求里的 `concurrencyLimit`）语义说明：
    /// KiroStudio 的凭据结构（`KiroCredentials`）与 `config.json` 都没有"并发上限"
    /// 这个概念——调度侧的容量维度是 `rpm_limit`（每分钟请求数软上限）与 inflight
    /// 观测计数，二者语义都不是"最大并发数"。为不新造一套并发机制，该值**只在响应里
    /// 原样回显**，不写入任何凭据字段、不影响调度。它也**不是**本函数的导入并发度：
    /// 导入并发由 [`IMPORT_MAX_IN_FLIGHT`] 固定，避免调用方传个 300 就把上游打爆。
    pub async fn import_keys(
        self: &std::sync::Arc<Self>,
        req: ImportKeysRequest,
    ) -> ImportKeysResponse {
        let started = std::time::Instant::now();
        let total = req.items.len();

        // 空批直接返回，省掉一次 JoinSet 构造。
        if total == 0 {
            return build_import_response(Vec::new(), req.concurrency_limit, 0);
        }

        // 按下标回填，保证输出顺序 == 输入顺序（并发完成顺序不可靠）。
        let mut slots: Vec<Option<ImportKeyResult>> = vec![None; total];
        let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(IMPORT_MAX_IN_FLIGHT));
        let mut tasks = tokio::task::JoinSet::new();

        for (idx, item) in req.items.into_iter().enumerate() {
            let permits = std::sync::Arc::clone(&permits);
            let this = std::sync::Arc::clone(self);
            tasks.spawn(async move {
                // 信号量在整条导入（含上游往返）期间持有，出作用域自动归还。
                // Semaphore 永不关闭，acquire 只可能因关闭而失败，故此处不会 Err。
                let _permit = permits.acquire().await;
                (idx, this.import_one_key(item).await)
            });
        }

        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((idx, result)) => slots[idx] = Some(result),
                Err(e) => {
                    // 任务 panic：不能让整批 500，否则前面成功的号无从得知。
                    // 无法回指具体下标（下标随 panic 一起丢了），故记日志 + 由下方兜底填充。
                    tracing::error!("批量导入任务异常终止: {}", e);
                }
            }
        }

        // 兜底：panic 掉的槽位补成失败记录，保证 results 长度恒等于 total，
        // 使 total/imported/failed 三者始终自洽。
        let results = slots
            .into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| ImportKeyResult {
                    ok: false,
                    key: "(unknown)".to_string(),
                    error: Some("导入任务异常终止".to_string()),
                })
            })
            .collect();

        build_import_response(
            results,
            req.concurrency_limit,
            started.elapsed().as_millis() as u64,
        )
    }

    /// 导入单个 Key：[`Self::import_keys`] 的每任务体。
    ///
    /// 拆成独立方法是为了让并发任务体保持 `'static`（只捕获 `Arc<Self>` 与 item），
    /// 同时把「置禁用失败不算导入失败」这类分支收在一处。
    async fn import_one_key(&self, item: ImportKeyItem) -> ImportKeyResult {
        let masked = mask_import_key(&item.key);
        // disabled 直接随请求入池，而不是「先以启用态加进去、再调 set_disabled 置位」。
        // 后者有一个真实的窗口：两步之间号已在池中且可被调度，若此时正好来请求，
        // 一个本该禁用的号（通常是已知被封的号）会被真的拿去打上游。
        // 一步到位后该窗口不存在，也省掉了「已导入但置禁用失败」这个半成功状态。
        // `api_region` 必须透下去：`ksk_` 是按区授权的 token，打错区恒 403。
        // 推号方给了就用（比探测权威——它知道这把 key 注册在哪，且省一次上游往返），
        // 没给则留 None，由 `add_credential` 内的 `probe_and_persist_api_region` 去探。
        let add_req = AddCredentialRequest {
            auth_method: "api_key".to_string(),
            kiro_api_key: Some(item.key),
            endpoint: item.endpoint,
            disabled: item.disabled,
            api_region: item.api_region,
            ..Default::default()
        };
        match self.add_credential(add_req).await {
            Ok(_) => ImportKeyResult {
                ok: true,
                key: masked,
                error: None,
            },
            Err(e) => {
                // 只打脱敏 Key + 原因，绝不打明文。
                tracing::warn!("批量导入 Key {} 失败: {}", masked, e);
                ImportKeyResult {
                    ok: false,
                    key: masked,
                    error: Some(e.to_string()),
                }
            }
        }
    }

    /// 删除凭据
    pub fn delete_credential(&self, id: u64) -> Result<(), AdminServiceError> {
        self.delete_credential_forced(id, false)
    }

    /// 删除凭据，`force=true` 跳过「必须先禁用」这道门（仍进回收站，可恢复）。
    pub fn delete_credential_forced(&self, id: u64, force: bool) -> Result<(), AdminServiceError> {
        // ⚠️ 键必须在删除**之前**算：删掉后 export_credential 返 None，键会回落成 id
        // 字符串，清的就不是真正那条账号键了（见 prune_balance_cache_for_deleted）。
        let cache_key = self.balance_cache_key(id);

        self.token_manager
            .delete_credential_forced(id, force)
            .map_err(|e| self.classify_delete_error(e, id))?;

        // 清理已删除凭据的余额缓存 —— 但同 key 的分身还在时**不清**（那是共享的一条）。
        self.prune_balance_cache_for_deleted(&cache_key);

        Ok(())
    }

    /// 批量删除凭据。**部分失败仍返回 Ok**，逐条标记结果（与 `import/keys` 的既有模式一致）。
    ///
    /// # 为什么要批量端点
    ///
    /// 前端此前对每个选中项各发一次 `DELETE`，且因后端要求"先禁用"，实际是
    /// **2N 次往返**（禁用 + 删除）。批量 + force 把它降到 1 次。
    ///
    /// # 为什么部分失败不整体回滚
    ///
    /// 删除是逐号独立的软删（进回收站），没有跨号事务语义。整体回滚反而更糟：
    /// 用户选了 10 个号、其中 1 个 id 不存在，不该让另外 9 个都删不掉。
    /// 逐条返回 `ok`/`error` 让前端能精确提示"成功 9 个，失败 1 个（原因）"。
    pub fn delete_credentials_batch(&self, ids: &[u64], force: bool) -> Vec<BatchDeleteItemResult> {
        ids.iter()
            .map(|&id| match self.delete_credential_forced(id, force) {
                Ok(()) => BatchDeleteItemResult {
                    id,
                    ok: true,
                    error: None,
                },
                Err(e) => BatchDeleteItemResult {
                    id,
                    ok: false,
                    error: Some(e.to_string()),
                },
            })
            .collect()
    }

    /// 批量清理「已禁用」凭据：走 `delete_credential`（进**回收站**，可恢复），
    /// 排除代挂号与**可自愈**原因。
    ///
    /// # 为什么排除可自愈原因
    ///
    /// 见 [`CLEANUP_SELF_HEALABLE_REASONS`]：被那几个原因禁用的号，禁用态本身是瞬时的，
    /// 自愈会把它们原地复活。删掉它们不是"清死号"，是把健康号从池里拿走。
    ///
    /// # 为什么候选由服务端算
    ///
    /// 判据是「已禁用 且 不是代挂 且 原因不可自愈」，而"是不是代挂"的权威判据是
    /// [`KiroCredentials::is_custom_api_credential`]（`auth_method == custom_api`
    /// **或** `base_url.is_some()` 的旧数据兜底）。让前端自己按下发字段拼一份必然漂移
    /// —— 漂移的后果是**误删代挂号**，而代挂号是用户真金白银买的第三方中转，
    /// 删错了从回收站捞回来也得重配。所以判据只有服务端这一份。
    ///
    /// # 为什么不用 force
    ///
    /// 清理目标本来就是**已禁用**的号，`delete_credential`（force=false）那道
    /// 「必须先禁用」的门天然满足。刻意不传 force：万一筛选逻辑将来出 bug 把一个
    /// **在服务中**的号选进候选，那道门会挡住它 —— 这是最后一层护栏，白拿的。
    ///
    /// # 上限
    ///
    /// 与 `MAX_BATCH_DELETE_IDS` 同理由（adminKey 明文存 localStorage 且全仓无 CSP，
    /// 无上限的批量删除会放大 XSS 的破坏面）。超出部分按 id 升序留给**下一次调用**，
    /// 并在 `skipped` 里以 `over_limit` 显式告知，让重复调用能收敛，而不是静默丢弃。
    pub fn cleanup_disabled_credentials(&self, dry_run: bool) -> CleanupDisabledResponse {
        // 只看已禁用的号。未禁用的根本不是候选，连 skipped 都不进（那会把
        // 整池都列进响应，噪音掩埋真正被排除的那几条）。
        let disabled: Vec<(u64, Option<String>)> = self
            .token_manager
            .snapshot()
            .entries
            .into_iter()
            .filter(|e| e.disabled)
            .map(|e| (e.id, e.disabled_reason))
            .collect();
        let disabled_total = disabled.len();

        let mut candidates: Vec<u64> = Vec::new();
        let mut skipped: Vec<CleanupSkippedItem> = Vec::new();
        for (id, reason) in disabled {
            // 代挂判据必须问**真凭据**而不是快照字段：快照的 auth_method 对
            // 「custom_api 且带 kiroApiKey」的号会显示成 `api_key`（见 snapshot 里的
            // is_api_key_credential 分支），只看快照会把这类号误判成 Kiro 死号。
            // 取不到 = 刚被别人删掉的竞态。`None` 原样传下去，让判据报 `not_in_pool`
            // 而不是塞成 `true` 混进代挂 —— 见 `cleanup_verdict` 第一道排除。
            let is_custom_api = self
                .token_manager
                .export_credential(id)
                .map(|c| c.is_custom_api_credential());
            match cleanup_verdict(is_custom_api, reason.as_deref()) {
                Some(reason) => skipped.push(CleanupSkippedItem { id, reason }),
                None => candidates.push(id),
            }
        }

        // 升序 + 截断：保证"超出上限时留下哪些"是确定的，重复调用才能逐批收敛。
        candidates.sort_unstable();
        if candidates.len() > MAX_CLEANUP_DISABLED_IDS {
            for id in candidates.split_off(MAX_CLEANUP_DISABLED_IDS) {
                skipped.push(CleanupSkippedItem {
                    id,
                    reason: CLEANUP_SKIP_OVER_LIMIT,
                });
            }
        }

        if dry_run || candidates.is_empty() {
            return CleanupDisabledResponse {
                dry_run,
                disabled_total,
                candidates,
                skipped,
                deleted: 0,
                failed: 0,
                results: Vec::new(),
            };
        }

        tracing::info!(
            count = candidates.len(),
            skipped = skipped.len(),
            "批量清理已禁用凭据（进回收站，可恢复；代挂号与可自愈原因已排除）"
        );
        // force=false：见本方法文档「为什么不用 force」。
        let results = self.delete_credentials_batch(&candidates, false);
        let deleted = results.iter().filter(|r| r.ok).count();
        let failed = results.len() - deleted;
        CleanupDisabledResponse {
            dry_run,
            disabled_total,
            candidates,
            skipped,
            deleted,
            failed,
            results,
        }
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
                // 与凭据列表同口径的字符串枚举名，前端复用同一份 i18n 映射。
                disabled_reason: t.disabled_reason.map(|r| r.as_str().to_string()),
                disabled_at: t.disabled_at,
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
    ///
    /// `force`：跳过 key 重复校验，用于恢复**多开分身**（它与主凭据必然同 key）。
    /// 默认路径（false）保留误操作护栏。恢复后仍是禁用态，故强制恢复不会立刻投入调度。
    pub fn restore_credential(&self, id: u64, force: bool) -> Result<(), AdminServiceError> {
        self.token_manager
            .restore_credential(id, force)
            .map_err(|e| self.classify_trash_error(e, id))
    }

    /// 从回收站彻底删除凭据（不可恢复）
    pub fn purge_credential(&self, id: u64) -> Result<(), AdminServiceError> {
        // 同 delete_credential_forced：键在删除前算。
        let cache_key = self.balance_cache_key(id);

        self.token_manager
            .purge_credential(id)
            .map_err(|e| self.classify_trash_error(e, id))?;

        // 清理彻底删除凭据的余额缓存残留 —— 同 key 的分身还在时不清。
        self.prune_balance_cache_for_deleted(&cache_key);

        Ok(())
    }

    /// 获取负载均衡模式
    pub fn get_load_balancing_mode(&self) -> LoadBalancingModeResponse {
        LoadBalancingModeResponse {
            mode: self.token_manager.get_load_balancing_mode(),
        }
    }

    /// 当前 TLS 后端（供出站 HTTP client 构建复用配置，如代理测活）。
    pub fn tls_backend(&self) -> crate::model::config::TlsBackend {
        self.token_manager.config().tls_backend
    }

    /// 批量推号入口是否启用。每次直接读 ArcSwap（无 TIER3 镜像），故开关热更即时生效。
    pub fn import_keys_enabled(&self) -> bool {
        self.token_manager.config().import_keys_enabled
    }

    /// 分身在请求未显式指定 `enabled` 时是否默认启用（见
    /// [`crate::model::config::Config::clone_default_enabled`]）。
    ///
    /// 与 `import_keys_enabled` 同款：每次直接读 ArcSwap，无 TIER3 镜像 ⇒ 热更即时生效。
    /// ⚠️ 只在 `enabled` **缺省**时才被查询（`clone_credential` 里是 `unwrap_or_else`），
    /// 所以显式请求值永远压过配置项。
    /// 分身默认启用（`clone_default_enabled()` 每次直接读 config ArcSwap）
    pub fn clone_default_enabled(&self) -> bool {
        self.token_manager.config().clone_default_enabled
    }

    /// 是否强制信任转发头（供审计中间件取客户端 IP，与入口安全中间件同口径）。
    pub fn trust_forwarded_header(&self) -> bool {
        self.token_manager.config().trust_forwarded_header
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

        // AIMD 三个累计计数器一次取齐（同源快照 + 少两次原子读）。
        let aimd_counters = self.token_manager.inbound_aimd_counters();

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
            native_thinking_effort_enabled: config.native_thinking_effort_enabled,
            tool_compat_mapping: config.tool_compat_mapping,
            import_keys_enabled: config.import_keys_enabled,
            clone_default_enabled: config.clone_default_enabled,
            upstream_retry_absorb_enabled: config.upstream_retry_absorb_enabled,
            upstream_retry_absorb_budget_secs: config.upstream_retry_absorb_budget_secs,
            upstream_retry_absorb_max_rounds: config.upstream_retry_absorb_max_rounds,
            upstream_retry_absorb_min_delay_ms: config.upstream_retry_absorb_min_delay_ms,
            upstream_retry_absorb_max_delay_secs: config.upstream_retry_absorb_max_delay_secs,
            upstream_retry_absorb_suspended: config.upstream_retry_absorb_suspended,
            upstream_retry_absorb_server_error: config.upstream_retry_absorb_server_error,
            upstream_retry_absorb_capacity_400: config.upstream_retry_absorb_capacity_400,
            upstream_retry_absorb_swap_budget_secs: config.upstream_retry_absorb_swap_budget_secs,
            upstream_retry_absorb_exhausted_status: config.upstream_retry_absorb_exhausted_status,
            self_heal_base_backoff_secs: config.self_heal_base_backoff_secs,
            self_heal_max_backoff_secs: config.self_heal_max_backoff_secs,
            self_heal_max_shift: config.self_heal_max_shift,
            prompt_cache_enabled: config.prompt_cache_enabled,
            mock_cache_enabled: config.mock_cache_enabled,
            mock_cache_read_ratio: config.mock_cache_read_ratio,
            strip_env_noise: config.strip_env_noise,
            tool_clean_leaked_tokens: config.tool_clean_leaked_tokens,
            tool_reclaim_textified_invoke: config.tool_reclaim_textified_invoke,
            tool_stray_repeat_guard: config.tool_stray_repeat_guard,
            tool_stream_align_failure: config.tool_stream_align_failure,
            tool_expose_error_to_client: config.tool_expose_error_to_client,
            tool_repair_json: config.tool_repair_json,
            tool_truncation_recovery: config.tool_truncation_recovery,
            tool_description_max_chars: config.tool_description_max_chars,
            cli_origin_kiro_cli: config.cli_origin_kiro_cli,
            cli_codewhisperer_optout_false: config.cli_codewhisperer_optout_false,
            cli_ua_align_real_client: config.cli_ua_align_real_client,
            upstream_per_credential_limit: config.upstream_per_credential_limit,
            encrypt_credentials_at_rest: config.encrypt_credentials_at_rest,
            cooldown_enabled: config.cooldown_enabled,
            auto_disable_suspicious: config.auto_disable_suspicious,
            // 余额耗尽自动禁用开关（AdminService 内存态，见 update_config 对应分支注释）。
            auto_disable_quota_exceeded: self
                .auto_disable_quota_exceeded
                .load(std::sync::atomic::Ordering::Relaxed),
            // 代理池自动健康调度开关（AdminService 内存态，见 update_config 对应分支注释）。
            socks_auto_health: self
                .socks_auto_health
                .load(std::sync::atomic::Ordering::Relaxed),
            all_cooling_fast_fail: config.all_cooling_fast_fail,
            rate_limit_enabled: config.rate_limit_enabled,
            rate_limit_daily_max: config.rate_limit_daily_max,
            rate_limit_min_interval_ms: config.rate_limit_min_interval_ms,
            affinity_enabled: config.affinity_enabled,
            priority_in_balanced: config.priority_in_balanced,
            credential_rpm_limit: config.credential_rpm_limit,
            rpm_headroom_factor: config.rpm_headroom_factor,
            rpm_reserve_slots: config.rpm_reserve_slots,
            rpm_hard_gate_overload_wait: config.rpm_hard_gate_overload_wait,
            cooldown_scale_pct: config.cooldown_scale_pct,
            rate_limit_jitter_pct: config.rate_limit_jitter_pct,
            throttle_profile: config.throttle_profile,
            scheduling_mode: config.scheduling_mode,
            inbound_throttle_enabled: config.inbound_throttle_enabled,
            inbound_rpm_auto: config.inbound_rpm_auto,
            inbound_target_rpm: config.inbound_target_rpm,
            inbound_rpm_min: config.inbound_rpm_min,
            inbound_rpm_max: config.inbound_rpm_max,
            inbound_burst_secs: config.inbound_burst_secs,
            inbound_queue_max_wait_secs: config.inbound_queue_max_wait_secs,
            inbound_queue_timeout_passthrough: config.inbound_queue_timeout_passthrough,
            // ⚠️ 本字段是**目标**值，名字里的 "current" 指"当前生效的目标"。
            // 实测吞吐在下面两个字段。别把它改成实测 —— 面板/autotune 都按"目标"读它。
            inbound_current_rpm: self.token_manager.inbound_target_rpm(),
            // 🔴 实测三元组。此前只有上面那一个字段，且它返回 target ⇒ 面板"当前 RPM"
            // 恒等于"目标 RPM"（实测面板 500 而客户端真实 50~70，差一个数量级，
            // 运维据此做过两次限流分析）。三个数必须并排看：
            //   inbound_current_rpm          整形闸门的目标
            //   inbound_observed_rpm         客户端真实速率（不含重试）
            //   inbound_observed_upstream_rpm 上游承受的尝试速率（含 failover 重试）
            // 后两者之比即重试放大倍数（2026-08-06 实测 4.59×）。
            inbound_observed_rpm: self.token_manager.observed_inbound_rpm(),
            inbound_observed_upstream_rpm: self.token_manager.observed_upstream_rpm(),
            inbound_admitted_total: self.token_manager.inbound_admitted_total(),
            // AIMD 三元组：排队 / 降档 / 升档累计次数。此前是只写不读的死代码，
            // 「整形是否在起作用、是否卡在下限」无从判断（先修度量再谈调参）。
            // 一次取三个值：三个字段同源，分三次调会读到不一致的快照（也多两次原子读）。
            inbound_aimd_queued_total: aimd_counters.0,
            inbound_aimd_md_total: aimd_counters.1,
            inbound_aimd_ai_total: aimd_counters.2,
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
            ip_blocklist: config.ip_blocklist.clone(),
            machine_code_blocklist: config.machine_code_blocklist.clone(),
            trust_forwarded_header: config.trust_forwarded_header,
            ingress_rate_limit_per_min: config.ingress_rate_limit_per_min,
            max_body_bytes: config.max_body_bytes,
            proactive_token_refresh: config.proactive_token_refresh,
            token_refresh_lead_minutes: config.token_refresh_lead_minutes,
            token_refresh_interval_secs: config.token_refresh_interval_secs,
            login_background_enabled: config.login_background_enabled,
            login_background_r18: config.login_background_r18,
            balance_refresh_interval_secs: config.balance_refresh_interval_secs,
            ota_auto_check: config.ota_auto_check,
            collect_client_fingerprint: config.collect_client_fingerprint,
            config_path: config
                .config_path()
                .map(|p| p.display().to_string()),
            model_mapping: config.model_mapping.clone(),
            error_messages: config.error_messages.clone(),
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
        // ⚠️ 2026-08-13 对抗审查 M3：load→改→save 的写路径必须与 update_config 同锁，
        // 否则并发 update_config × set_load_balancing_mode 仍会 lost update
        // （两者写同一字段 loadBalancingMode，后写整体覆盖先写）。
        let _write_guard = self.config_write_lock.lock();

        self.token_manager
            .set_load_balancing_mode(req.mode.clone())
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        Ok(LoadBalancingModeResponse { mode: req.mode })
    }

    /// 更新服务端配置并持久化到 config.json
    ///
    /// # 并发写锁（2026-08-14）
    ///
    /// 本方法包住「load → 逐字段改 → save → reload_config」整段（见
    /// `update_config_locked`）。并发两个 PUT /config 时，若各自 load 后交错 save，
    /// 后完成者会把先完成者的改动整体覆盖（lost update）。持锁串行后互不覆盖。
    /// 锁内无任何 await（本函数与内部全部是同步调用），`parking_lot::Mutex` 足够。
    pub fn update_config(
        self: &Arc<Self>,
        req: UpdateConfigRequest,
    ) -> Result<UpdateConfigResponse, AdminServiceError> {
        let _guard = self.config_write_lock.lock();
        self.update_config_locked(req)
    }

    /// `update_config` 的锁内实现（原函数体）。**只有** `update_config` 包装函数
    /// 与 `import_config` 会调用它，调用方必须先持 `config_write_lock`。
    fn update_config_locked(
        self: &Arc<Self>,
        req: UpdateConfigRequest,
    ) -> Result<UpdateConfigResponse, AdminServiceError> {
        let config_path = self
            .token_manager
            .config()
            .config_path()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| {
                AdminServiceError::InternalError("配置文件路径未知，无法保存配置".to_string())
            })?;

        // 从磁盘重新加载，避免覆盖进程外的改动
        let mut config = crate::model::config::Config::load(&config_path)
            .map_err(|e| AdminServiceError::InternalError(format!("加载配置失败: {}", e)))?;

        // 审计（2026-08-14）：保存前对比新旧 JSON，记录「变了哪些字段」——只记字段名不记值。
        let old_json = serde_json::to_value(&config).unwrap_or_default();

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
        // 推号开关无 TIER3 镜像，只需让 reload_config 跑一次即可生效，故用 bool 而非 Option<bool>。
        let mut import_keys_enabled_changed = false;
        // 分身默认启用同款：`clone_default_enabled()` 每次直接读 config ArcSwap，
        // 所以只要 reload_config 被触发就即时生效。**必须**进 hot_or_display_changed 的
        // OR 链，漏掉就是"存了盘但 ArcSwap 仍是旧值"，面板开关静默无效。
        let mut clone_default_enabled_changed = false;
        // 上游 429 吸收层十项是否有变更。**无 TIER3 setter**：吸收层在 provider 内直接读
        // token_manager 的 config ArcSwap，所以只要下面的 reload_config 被触发就即时生效。
        // ⚠️ 正因如此，这个 flag **必须**进 hot_or_display_changed 的 OR 链 ——
        // 漏掉就会「存了盘但 ArcSwap 仍是旧值」，面板开关静默无效。
        let mut absorb_changed = false;
        // 全池自愈退避参数（2026-08-11 配置化）：token_manager 每周期从 config 读，
        // 必须进 hot_or_display_changed 的 OR 链，否则「存了盘但 ArcSwap 仍是旧值」。
        let mut self_heal_changed = false;
        let mut prompt_cache_enabled_changed: Option<bool> = None;
        // 透传模拟缓存（TIER3）：enabled/ratio 任一变更都调 handlers setter 即时生效。
        let mut mock_cache_changed = false;
        // 错误码/提示词覆盖表（TIER1）：无 TIER3 setter——消费点每请求读 config
        // ArcSwap 快照查表（model_mapping 同款范式），**只**靠下面 OR 链触发
        // reload_config。漏掉这行 → 存了盘但 ArcSwap 仍是旧值，面板改完当次不生效。
        let mut error_messages_changed = false;
        // 环境噪音剥离开关：改后调 converter setter 即时生效（进程级镜像，不重启）。
        let mut strip_env_noise_changed: Option<bool> = None;
        // Kiro 原生 effort 开关：改后调 converter setter 即时生效（进程级镜像，不重启）。
        let mut native_thinking_effort_enabled_changed: Option<bool> = None;
        // CC↔Kiro 工具名/参数映射开关：改后调 converter setter 即时生效（进程级镜像，不重启）。
        let mut tool_compat_mapping_changed: Option<bool> = None;
        // 工具错误缓解三开关：改后调 handlers setter 即时生效（进程级镜像，不重启）。
        let mut tool_clean_leaked_tokens_changed: Option<bool> = None;
        let mut tool_stream_align_failure_changed: Option<bool> = None;
        let mut tool_expose_error_to_client_changed: Option<bool> = None;
        let mut tool_repair_json_changed: Option<bool> = None;
        let mut tool_truncation_recovery_changed: Option<bool> = None;
        let mut tool_description_max_chars_changed: Option<usize> = None;
        // at-rest 加密开关变更:变更后立即重写凭据/回收站文件(明文↔密文),不等下次偶发变更。
        let mut encrypt_at_rest_changed = false;
        // 两把鉴权 key 的轮换：存盘后调 auth_keys setter 即时生效（不再进 restart_fields）。
        // 存 trim 后的新值而非 bool——setter 需要实际值，且 reload_config 会把 config 里的
        // 这两把 key 钉回启动值（restart-only 字段的 split-brain 防护），故热更单元是它们
        // 唯一的活真相源（详见下方 setter 调用处的顺序注释）。
        let mut user_key_changed: Option<String> = None;
        let mut admin_key_changed: Option<String> = None;

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
        // —— OTA 自动检查开关（需重启生效）——
        // main.rs 启动期按 config.ota_auto_check 门控 spawn 后台检查任务（无 respawn
        // 机制，TIER2 覆盖范围外），改后必须重启进程才生效 → 只进 restart_fields。
        if let Some(v) = req.ota_auto_check {
            if v != config.ota_auto_check {
                config.ota_auto_check = v;
                restart_fields.push("otaAutoCheck".into());
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
        // —— 批量推号入口开关（无 TIER3 setter：handler 每次直接读 config()，
        //    存盘 + reload_config 换入 ArcSwap 后下一个请求即生效）——
        if let Some(v) = req.import_keys_enabled {
            if v != config.import_keys_enabled {
                config.import_keys_enabled = v;
                import_keys_enabled_changed = true;
            }
        }
        // —— 分身默认启用（同上：无 TIER3 setter，靠 reload_config 换入 ArcSwap）——
        if let Some(v) = req.clone_default_enabled {
            if v != config.clone_default_enabled {
                config.clone_default_enabled = v;
                clone_default_enabled_changed = true;
            }
        }
        // —— 上游 429 吸收层十项（存盘 + reload_config 即时生效，无 TIER3 setter）——
        if let Some(v) = req.upstream_retry_absorb_enabled {
            if v != config.upstream_retry_absorb_enabled {
                config.upstream_retry_absorb_enabled = v;
                absorb_changed = true;
            }
        }
        if let Some(v) = req.upstream_retry_absorb_budget_secs {
            if v != config.upstream_retry_absorb_budget_secs {
                config.upstream_retry_absorb_budget_secs = v;
                absorb_changed = true;
            }
        }
        if let Some(v) = req.upstream_retry_absorb_max_rounds {
            if v != config.upstream_retry_absorb_max_rounds {
                config.upstream_retry_absorb_max_rounds = v;
                absorb_changed = true;
            }
        }
        if let Some(v) = req.upstream_retry_absorb_min_delay_ms {
            if v != config.upstream_retry_absorb_min_delay_ms {
                config.upstream_retry_absorb_min_delay_ms = v;
                absorb_changed = true;
            }
        }
        if let Some(v) = req.upstream_retry_absorb_max_delay_secs {
            if v != config.upstream_retry_absorb_max_delay_secs {
                config.upstream_retry_absorb_max_delay_secs = v;
                absorb_changed = true;
            }
        }
        if let Some(v) = req.upstream_retry_absorb_suspended {
            if v != config.upstream_retry_absorb_suspended {
                config.upstream_retry_absorb_suspended = v;
                absorb_changed = true;
            }
        }
        // 是否吸收上游 5xx（2026-08-10 补：此前该字段只能改 config.json + 重启）。
        // 线上代挂上游主要故障形态是 502，不吸收等于把最典型的瞬态故障直接甩给客户端。
        if let Some(v) = req.upstream_retry_absorb_server_error {
            if v != config.upstream_retry_absorb_server_error {
                config.upstream_retry_absorb_server_error = v;
                absorb_changed = true;
            }
        }
        // 吸收 400 容量类 / 换号空窗独立预算 / 耗尽状态码（2026-08-11 补：此前只能改 config.json）。
        if let Some(v) = req.upstream_retry_absorb_capacity_400 {
            if v != config.upstream_retry_absorb_capacity_400 {
                config.upstream_retry_absorb_capacity_400 = v;
                absorb_changed = true;
            }
        }
        if let Some(v) = req.upstream_retry_absorb_swap_budget_secs {
            if v != config.upstream_retry_absorb_swap_budget_secs {
                config.upstream_retry_absorb_swap_budget_secs = v;
                absorb_changed = true;
            }
        }
        if let Some(v) = req.upstream_retry_absorb_exhausted_status {
            if v != config.upstream_retry_absorb_exhausted_status {
                // 值域白名单（2026-08-11 审计）：config 文档明确「唯一另一个可选值 503」。
                // 消费端（provider.rs）只认精确 503、其余一律按 429 语义处理（有守卫钉死），
                // 但面板不该允许把 0/999 之类写进 config.json 长期驻留。
                if v != 429 && v != 503 {
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "upstreamRetryAbsorbExhaustedStatus 只允许 429 或 503，收到 {v}"
                    )));
                }
                config.upstream_retry_absorb_exhausted_status = v;
                absorb_changed = true;
            }
        }

        // —— prompt cache 记账下发开关（TIER3 热更：改后调 handlers setter 即时生效不重启）——
        // 此前该配置既无读取点也不在 admin 请求里，等于面板改不了、改了也没用。
        if let Some(v) = req.prompt_cache_enabled {
            if v != config.prompt_cache_enabled {
                config.prompt_cache_enabled = v;
                prompt_cache_enabled_changed = Some(v);
            }
        }
        // —— 透传模拟缓存（TIER3 热更：改后调 handlers setter 即时生效不重启）——
        // 两个字段共用一个 changed 标志：任一变更是同一个 setter 调用。
        if let Some(v) = req.mock_cache_enabled {
            if v != config.mock_cache_enabled {
                config.mock_cache_enabled = v;
                mock_cache_changed = true;
            }
        }
        if let Some(v) = req.mock_cache_read_ratio {
            // 先清洗再比较/写盘：setter（handlers）侧也会清洗，但 config 结构里存
            // 原始非法值（NaN/±inf/越界）会让面板快照（读 config 结构）与热路径
            // 生效值（经 setter clamp）不一致。
            let v = crate::anthropic::handlers::sanitize_mock_cache_ratio(v);
            if v != config.mock_cache_read_ratio {
                config.mock_cache_read_ratio = v;
                mock_cache_changed = true;
            }
        }
        // —— 环境噪音剥离开关（改后调 converter setter 即时生效不重启）——
        if let Some(v) = req.strip_env_noise {
            if v != config.strip_env_noise {
                config.strip_env_noise = v;
                strip_env_noise_changed = Some(v);
            }
        }
        // —— Kiro 原生 effort 开关（改后调 converter setter 即时生效不重启）——
        if let Some(v) = req.native_thinking_effort_enabled {
            if v != config.native_thinking_effort_enabled {
                config.native_thinking_effort_enabled = v;
                native_thinking_effort_enabled_changed = Some(v);
            }
        }
        // —— CC↔Kiro 工具名/参数映射开关（改后调 converter setter 即时生效不重启）——
        if let Some(v) = req.tool_compat_mapping {
            if v != config.tool_compat_mapping {
                config.tool_compat_mapping = v;
                tool_compat_mapping_changed = Some(v);
            }
        }
        if let Some(v) = req.tool_clean_leaked_tokens {
            if v != config.tool_clean_leaked_tokens {
                config.tool_clean_leaked_tokens = v;
                tool_clean_leaked_tokens_changed = Some(v);
            }
        }
        // 全池自愈退避参数（2026-08-11 配置化）：无 TIER3 setter（token_manager 每周期
        // 从 config 读），改后下一个自愈周期即生效（热更语义见 config.rs 字段注释）。
        if let Some(v) = req.self_heal_base_backoff_secs {
            if v != config.self_heal_base_backoff_secs {
                config.self_heal_base_backoff_secs = v;
                self_heal_changed = true;
            }
        }
        if let Some(v) = req.self_heal_max_backoff_secs {
            if v != config.self_heal_max_backoff_secs {
                config.self_heal_max_backoff_secs = v;
                self_heal_changed = true;
            }
        }
        if let Some(v) = req.self_heal_max_shift {
            if v != config.self_heal_max_shift {
                config.self_heal_max_shift = v;
                self_heal_changed = true;
            }
        }
        if let Some(v) = req.tool_reclaim_textified_invoke {
            if v != config.tool_reclaim_textified_invoke {
                config.tool_reclaim_textified_invoke = v;
                crate::anthropic::handlers::set_tool_reclaim_textified_invoke(v);
                hot_changed = true;
            }
        }
        if let Some(v) = req.tool_stray_repeat_guard {
            if v != config.tool_stray_repeat_guard {
                config.tool_stray_repeat_guard = v;
                crate::anthropic::handlers::set_tool_stray_repeat_guard(v);
                hot_changed = true;
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
        // ── CLI 端点协议/指纹三开关 ──
        // 都**不需要** TIER3 原子镜像：`decorate_api` / `transform_api_body` 从
        // `ctx.config` 读，而那份 Config 是 provider 每次调用时 `token_manager.config()`
        // （ArcSwap `load_full`）取的新快照 ⇒ 存盘 + reload_config 后下一个请求即生效。
        // 加镜像反而多一份要同步的真值（与吸收层同理，见 provider.rs 的 AbsorbPolicy 说明）。
        // 故这里只置 `hot_changed`，不进 restart_fields、不调任何 setter。
        if let Some(v) = req.cli_origin_kiro_cli {
            if v != config.cli_origin_kiro_cli {
                config.cli_origin_kiro_cli = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.cli_codewhisperer_optout_false {
            if v != config.cli_codewhisperer_optout_false {
                config.cli_codewhisperer_optout_false = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.cli_ua_align_real_client {
            if v != config.cli_ua_align_real_client {
                config.cli_ua_align_real_client = v;
                hot_changed = true;
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
        // `reload_config`（token_manager.rs:2163）已经在读这个字段并 store 进 AtomicBool，
        // 缺的只是「面板能把它写进 config」这一段 —— 所以补上这个分支即完成 TIER1 闭环，
        // 不需要动 token_manager。绝不 push 进 restart_fields。
        if let Some(v) = req.auto_disable_suspicious {
            if v != config.auto_disable_suspicious {
                config.auto_disable_suspicious = v;
                hot_changed = true;
            }
        }
        // —— 余额耗尽**自动**禁用开关（2026-08-14 新增，AdminService 内存态）——
        // 读取点在后台温和余额刷新循环：刷到「新鲜真值 remaining<=0」即自动禁用。
        // ⚠️ 该开关只存于本服务内存（model/config.rs 不在可改范围，无法落盘），
        // 重启回默认值 true。置 hot_changed 只为让响应如实回「已保存并立即生效」。
        if let Some(v) = req.auto_disable_quota_exceeded {
            let cur = self
                .auto_disable_quota_exceeded
                .load(std::sync::atomic::Ordering::Relaxed);
            if v != cur {
                self.auto_disable_quota_exceeded
                    .store(v, std::sync::atomic::Ordering::Relaxed);
                hot_changed = true;
            }
        }
        // —— 代理池自动健康调度开关（2026-08-14 新增，AdminService 内存态）——
        // 读取点在后台健康调度任务：每轮自检本开关，关闭时整轮跳过（任务常驻不做事，
        // 重开无需重挂）。⚠️ 只存于本服务内存（model/config.rs 不在可改范围，无法落盘），
        // 重启回默认值 true。置 hot_changed 只为让响应如实回「已保存并立即生效」。
        if let Some(v) = req.socks_auto_health {
            let cur = self
                .socks_auto_health
                .load(std::sync::atomic::Ordering::Relaxed);
            if v != cur {
                self.socks_auto_health
                    .store(v, std::sync::atomic::Ordering::Relaxed);
                hot_changed = true;
            }
        }
        if let Some(v) = req.all_cooling_fast_fail {
            if v != config.all_cooling_fast_fail {
                config.all_cooling_fast_fail = v;
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
        // ---- 智能调度(全部热更即时生效)。整百分比字段服务端 clamp,不信任前端。----
        if let Some(v) = req.credential_rpm_limit {
            // 全局每号 RPM 上界防 u32 极值污染(远超真实吞吐即无意义)。
            let v = v.min(100_000);
            if v != config.credential_rpm_limit {
                config.credential_rpm_limit = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.cooldown_scale_pct {
            let v = v.clamp(10, 500);
            if v != config.cooldown_scale_pct {
                config.cooldown_scale_pct = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.rate_limit_jitter_pct {
            let v = v.min(50);
            if v != config.rate_limit_jitter_pct {
                config.rate_limit_jitter_pct = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.rpm_headroom_factor {
            let v = v.min(100);
            if v != config.rpm_headroom_factor {
                config.rpm_headroom_factor = v;
                hot_changed = true;
            }
        }
        // ---- 限流档位（2026-08-11）----
        //
        // 🔴 与**文件加载**时的语义不同，这里必须真的把档位值写进配置。
        //
        // 文件加载走 `Config::apply_throttle_profile`，契约是「只填空、不覆盖显式值」——
        // 因为那时无法区分"用户想要 false"和"字段缺失默认 false"，而线上 config.json
        // 那 7 个字段全部显式写过，冲掉就是改写生产配置。
        //
        // 但从面板切档是**用户主动的意图表达**：他就是要这一档的行为。此时若还"只填空"，
        // 由于 config.json 里那些键都已存在，档位会**一个字段都改不动** —— 按钮点了没反应，
        // 这是比"冲掉配置"更糟的体验（静默无效）。
        // 所以这里用空 explicit 集合调用，让档位对所有受管字段生效，
        // 且改动会随 `save()` 落盘成显式值（之后重启加载时它们就是"显式"的，不会被再次覆盖 —— 自洽）。
        if let Some(m) = req.scheduling_mode {
            if m != config.scheduling_mode {
                config.scheduling_mode = m;
                // 调度模式映射到对应 ThrottleProfile 并写入预设矩阵
                //（smart→Direct / stable→Shielded / manual→Manual，见 `SchedulingMode`）。
                config.throttle_profile = m.to_throttle_profile();
                config.apply_throttle_profile_for_explicit_switch();
                hot_changed = true;
            }
        }
        if let Some(p) = req.throttle_profile {
            if p != config.throttle_profile {
                config.throttle_profile = p;
                // 反向同步：老客户端只发 throttleProfile 时，调度模式标记保持一致
                //（direct→smart / shielded→stable / manual→manual）。
                config.scheduling_mode = match p {
                    crate::model::config::ThrottleProfile::Direct => {
                        crate::model::config::SchedulingMode::Smart
                    }
                    crate::model::config::ThrottleProfile::Shielded => {
                        crate::model::config::SchedulingMode::Stable
                    }
                    crate::model::config::ThrottleProfile::Manual => {
                        crate::model::config::SchedulingMode::Manual
                    }
                };
                config.apply_throttle_profile_for_explicit_switch();
                hot_changed = true;
            }
        }
        // ---- 入站请求整形 + RPM 自动挡(全热更)----
        if let Some(v) = req.inbound_throttle_enabled {
            if v != config.inbound_throttle_enabled {
                config.inbound_throttle_enabled = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.inbound_rpm_auto {
            if v != config.inbound_rpm_auto {
                config.inbound_rpm_auto = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.inbound_target_rpm {
            let v = v.clamp(1, 100_000);
            if v != config.inbound_target_rpm {
                config.inbound_target_rpm = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.inbound_rpm_min {
            let v = v.clamp(1, 100_000);
            if v != config.inbound_rpm_min {
                config.inbound_rpm_min = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.inbound_rpm_max {
            let v = v.clamp(1, 100_000);
            if v != config.inbound_rpm_max {
                config.inbound_rpm_max = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.inbound_burst_secs {
            let v = v.clamp(1, 60);
            if v != config.inbound_burst_secs {
                config.inbound_burst_secs = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.inbound_queue_max_wait_secs {
            let v = v.clamp(1, 300);
            if v != config.inbound_queue_max_wait_secs {
                config.inbound_queue_max_wait_secs = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.inbound_queue_timeout_passthrough {
            if v != config.inbound_queue_timeout_passthrough {
                config.inbound_queue_timeout_passthrough = v;
                hot_changed = true;
            }
        }
        // ⭐ 三个 RPM 字段的**交叉**不变量：`min <= target <= max`。
        //
        // 必须在三者都处理完之后统一收口 —— 上面每个字段各自只 clamp 到 [1,100_000]，
        // 彼此不可见，于是能存出自相矛盾的组合。两个实测后果：
        //
        // ① **`min > max` 会 panic**：`throttle.rs` 的 `clamp(lo, hi)` 在 min>max 时
        //    panic（`u32::clamp` 的契约）。面板保存一次这样的配置就打死正在服务的进程。
        //    throttle 侧已加 `.max(lo)` 兜底，这里再拦一道，让**存下去的值**本身就自洽
        //    （否则面板显示的与实际生效的永远不一致，排查时会被带偏）。
        //
        // ② **`target > max` 让自动调节永久失效**（线上实测）：throttle 把 target
        //    clamp 到 max 后**只存在内存里**，config.json 仍留着未被 clamp 的原值。
        //    VPS 上的 `throttle-autotune` 读的是**存储值**，于是它拿一个从未生效过的
        //    数（614）跟自己的建议比 → 死区永远满足 → 永不调整，而实际生效的是 300。
        //    实测该差距在两天内从 307 扩大到 614，且仍在扩大。
        //    存储值与生效值统一后，autotune 读到的就是真值，死区判断才有意义。
        {
            let lo = config.inbound_rpm_min;
            if config.inbound_rpm_max < lo {
                tracing::warn!(
                    inbound_rpm_min = lo,
                    inbound_rpm_max = config.inbound_rpm_max,
                    "inboundRpmMax 小于 inboundRpmMin，已抬到与下限相等（否则整形层 clamp 会 panic）"
                );
                config.inbound_rpm_max = lo;
                hot_changed = true;
            }
            let clamped = config.inbound_target_rpm.clamp(lo, config.inbound_rpm_max);
            if clamped != config.inbound_target_rpm {
                tracing::warn!(
                    requested = config.inbound_target_rpm,
                    effective = clamped,
                    inbound_rpm_min = lo,
                    inbound_rpm_max = config.inbound_rpm_max,
                    "inboundTargetRpm 超出 [min,max]，已按生效值落盘（存储值与生效值必须一致，\
                     否则外部自动调节读到的是从未生效过的数）"
                );
                config.inbound_target_rpm = clamped;
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
        // 前端不回显现值，传空串=不改。
        // 【不再需要重启】鉴权已改为活读 `common::auth_keys` 的进程级单元，存盘后调 setter
        // 即时生效——轮换密钥是常规运维动作，重启整个网关会掐断所有在途流式请求。
        if let Some(v) = req.api_key {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                let new_val = Some(trimmed.to_string());
                if new_val != config.api_key {
                    config.api_key = new_val;
                    user_key_changed = Some(trimmed.to_string());
                }
            }
        }
        // adminApiKey：同 userKey，空串=不改（防把管理面锁死成 fail-closed 全 401）。
        // 【自锁风险】轮换后当前面板持有的旧 key 立即失效，前端须用新 key 重新鉴权——
        // 这是热更的正确语义（旧 key 必须马上作废），前端负责换 header 而非后端延迟生效。
        if let Some(v) = req.admin_api_key {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                let new_val = Some(trimmed.to_string());
                if new_val != config.admin_api_key {
                    config.admin_api_key = new_val;
                    admin_key_changed = Some(trimmed.to_string());
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
        if let Some(v) = req.ip_blocklist {
            let cleaned: Vec<String> =
                v.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            // 校验每条 CIDR 合法,非法直接拒绝。
            for entry in &cleaned {
                if let Err(e) = crate::common::security::validate_cidr(entry) {
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "ipBlocklist 条目 '{entry}' 非法: {e}"
                    )));
                }
            }
            if cleaned != config.ip_blocklist {
                config.ip_blocklist = cleaned.clone();
                // 业务层黑名单镜像热更(按真实客户端 IP 封禁,反代后也生效,立即生效无需重启)。
                // 注:security 中间件的黑名单仍是 restart-only(启动时建),但业务层这道已足够拦截。
                crate::anthropic::handlers::set_ip_blocklist(&cleaned);
                hot_changed = true;
            }
        }
        if let Some(v) = req.machine_code_blocklist {
            // 归一化:trim + 小写(判定端大小写不敏感);校验格式 MC- + 12 位十六进制,非法直接拒绝。
            let cleaned: Vec<String> = v
                .into_iter()
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            for entry in &cleaned {
                let ok = entry.len() == 15
                    && entry.starts_with("mc-")
                    && entry[3..].chars().all(|c| c.is_ascii_hexdigit());
                if !ok {
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "machineCodeBlocklist 条目 '{entry}' 非法(应为 MC- 加 12 位十六进制)"
                    )));
                }
            }
            if cleaned != config.machine_code_blocklist {
                config.machine_code_blocklist = cleaned.clone();
                // 业务层机器码黑名单镜像热更(立即生效无需重启)。
                crate::anthropic::handlers::set_machine_code_blocklist(&cleaned);
                hot_changed = true;
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

        // —— 立即生效的字段：全局模型映射（整表替换）——
        // provider 每次调用时 `token_manager.config()`（ArcSwap load_full）取新快照，
        // 所以只需保存 + reload_config 热应用即可，无需重启（TIER1 范式，同吸收层）。
        if let Some(mm) = req.model_mapping {
            if mm != config.model_mapping {
                config.model_mapping = mm;
                hot_changed = true;
            }
        }

        // —— 立即生效的字段：错误码/提示词覆盖表（per-key merge）——
        // 消费点（错误翻译处）读 handlers 进程镜像（reload_config 改写同一镜像），
        // 所以只需保存 + reload_config 热应用。⚠️ 语义：**per-key merge**——提交的
        // key 更新为提交值（字段 None = 用内置默认），空对象 `{}` = 清掉该 key 回默认，
        // **未提交的 key 保持不变**（前端按"有改动的 key"提交，整表替换会重置用户
        // 未改的 key）。⚠️ 先校验再写盘：任一 key 非法 → 整表拒绝（保持旧表），
        // 400 回显第一个错误（对齐 exhausted_status 白名单先例）。
        if let Some(em) = req.error_messages {
            let mut merged = config.error_messages.clone();
            for (k, v) in em {
                let is_empty = v.status.is_none()
                    && v.r#type.is_none()
                    && v.message.is_none()
                    && v.retry_after_secs.is_none();
                if is_empty {
                    merged.remove(&k);
                } else {
                    merged.insert(k, v);
                }
            }
            if merged != config.error_messages {
                validate_error_messages(&merged).map_err(AdminServiceError::InvalidCredential)?;
                config.error_messages = merged;
                error_messages_changed = true;
            }
        }

        // 持久化（一次写盘）
        //
        // 2026-08-14 新增两件事：
        // ① 写盘前轮换 .bak（保留 .bak / .bak.1 / .bak.2 三份，见 rotate_config_backup），
        //    手滑改错配置可回退；
        // ② 字段级 diff 审计：对比 load 时的旧值与改完的新值，只记字段名不记值
        //    （敏感字段的值绝不进日志）。
        rotate_config_backup(&config_path);
        {
            let new_json = serde_json::to_value(&config).unwrap_or_default();
            let changed = diff_json_fields(&old_json, &new_json);
            if !changed.is_empty() {
                tracing::info!(target: "audit", "配置更新，变更字段: {:?}", changed);
            }
        }
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
            || import_keys_enabled_changed
            // 分身默认启用同样没有 TIER3 setter，**只**靠这一行触发 reload_config。
            // 删掉它 → 面板改了、存了盘，但 clone_default_enabled() 读到的仍是旧值。
            || clone_default_enabled_changed
            || prompt_cache_enabled_changed.is_some()
            // 透传模拟缓存有 TIER3 setter（handlers 镜像），但要 `hot_changed` 之外仍进
            // OR 链才会调它：漏掉这行只改本项时面板会回「无改动」、镜像不刷新。
            || mock_cache_changed
            || strip_env_noise_changed.is_some()
            // Kiro 原生 effort 开关有 TIER3 setter（converter 镜像），但要 `hot_changed`
            // 之外仍进 OR 链才会调它：漏掉这行只改本项时面板会回「无改动」。
            || native_thinking_effort_enabled_changed.is_some()
            // CC↔Kiro 工具名/参数映射开关同款：TIER3 setter（converter 镜像），漏掉这行
            // 只改本项时面板会回「无改动」、镜像不刷新。
            || tool_compat_mapping_changed.is_some()
            || self_heal_changed
            || tool_clean_leaked_tokens_changed.is_some()
            || tool_stream_align_failure_changed.is_some()
            || tool_expose_error_to_client_changed.is_some()
            || tool_repair_json_changed.is_some()
            || tool_truncation_recovery_changed.is_some()
            || tool_description_max_chars_changed.is_some()
            // 🔴 吸收层没有 TIER3 setter，**只**靠这一行触发 reload_config 把新值换进 ArcSwap。
            // 删掉它 → 面板改了、存了盘、但 provider 读到的仍是旧值 → 开关静默无效。
            // 由 absorb_changed_is_in_hot_reload_or_chain 源码守卫钉死。
            || absorb_changed
            // 错误码/提示词覆盖表同款：消费点每请求读 config ArcSwap（无 TIER3 setter），
            // 只有这一行能触发 reload_config。漏掉 → 存盘但热路径仍读旧表。
            || error_messages_changed;
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
                    "at-rest 加密开关已改,但立即重写凭据文件被跳过（无凭据路径）"
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

        // TIER3：prompt cache 记账下发开关立即应用到热路径进程级镜像（下一个请求即生效）
        if let Some(v) = prompt_cache_enabled_changed {
            crate::anthropic::set_prompt_cache_enabled(v);
        }

        // TIER3：透传模拟缓存配置立即应用到热路径进程级镜像（下一个透传请求即生效）。
        // 用 `config`（已更新）而非 req 原值：两个字段可能只改一个，setter 要拿完整组。
        if mock_cache_changed {
            crate::anthropic::handlers::set_mock_cache_config(
                config.mock_cache_enabled,
                config.mock_cache_read_ratio,
            );
        }

        // 环境噪音剥离开关立即应用到 converter 进程级镜像（下一个请求即生效）
        if let Some(v) = strip_env_noise_changed {
            crate::anthropic::set_strip_env_noise(v);
        }
        // Kiro 原生 effort 开关立即应用到 converter 进程级镜像（下一个请求即生效）
        if let Some(v) = native_thinking_effort_enabled_changed {
            crate::anthropic::set_native_thinking_effort_enabled(v);
        }
        // CC↔Kiro 工具名/参数映射开关立即应用到 converter 进程级镜像（下一个请求即生效，不重启）。
        if let Some(v) = tool_compat_mapping_changed {
            crate::anthropic::set_tool_compat_mapping(v);
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

        // userKey 轮换立即生效：下一个 /v1 请求即按新 key 判定，旧 key 同时失效。
        // ⚠️必须放在 reload_config 之后——reload 会把 config 里的 userKey 钉回启动值
        // （restart-only 字段的 split-brain 防护，见 token_manager::reload_config 的
        // restore 表），但热更单元才是鉴权的活真相源，故此处后写、以新值为准。
        // setter 拒空，失败仅告警（旧 key 继续有效，不会裸奔）。
        if let Some(v) = &user_key_changed {
            match crate::common::auth_keys::set_user_key(v) {
                Ok(()) => tracing::info!("apiKey 已轮换并即时生效（无需重启）"),
                Err(e) => tracing::error!("apiKey 已存盘但热更失败，重启后生效: {}", e),
            }
        }
        // adminApiKey 轮换：同上。旧 key 立即失效，面板须用新 key 重新鉴权。
        if let Some(v) = &admin_key_changed {
            match crate::common::auth_keys::set_admin_key(v) {
                Ok(()) => tracing::info!("adminApiKey 已轮换并即时生效（无需重启）"),
                Err(e) => tracing::error!("adminApiKey 已存盘但热更失败，重启后生效: {}", e),
            }
        }

        let immediate_changed = hot_changed
            || refresh_task_changed
            || balance_task_changed
            || login_bg_changed.is_some()
            || login_bg_r18_changed.is_some()
            || fingerprint_changed.is_some()
            || extract_thinking_changed.is_some()
            || cc_auto_buffer_changed.is_some()
            || import_keys_enabled_changed
            // 立即生效项（reload_config 换 ArcSwap），漏掉这行只改本项时面板会回
            // 「无改动」，与实际不符。
            || clone_default_enabled_changed
            || prompt_cache_enabled_changed.is_some()
            || mock_cache_changed
            || strip_env_noise_changed.is_some()
            || native_thinking_effort_enabled_changed.is_some()
            || tool_compat_mapping_changed.is_some()
            || tool_clean_leaked_tokens_changed.is_some()
            || tool_stream_align_failure_changed.is_some()
            || tool_expose_error_to_client_changed.is_some()
            || tool_repair_json_changed.is_some()
            || tool_truncation_recovery_changed.is_some()
            || tool_description_max_chars_changed.is_some()
            // 吸收层是立即生效项（reload_config 换 ArcSwap），漏掉这行只改吸收层时面板会
            // 回「未检测到变更」，与实际不符。
            || absorb_changed
            // 错误码/提示词覆盖表同款（hot_or_display_changed 触发 reload_config 即生效）：
            // 漏掉这行只改错误码表时面板会回「未检测到变更」，与实际不符。
            || error_messages_changed
            // 两把 key 走 auth_keys setter 即时生效，故算「立即生效」而非「需重启」。
            // 不进 hot_or_display_changed：reload_config 会把它们钉回启动值，重载对它们无用。
            || user_key_changed.is_some()
            || admin_key_changed.is_some();
        let restart_required = !restart_fields.is_empty();
        let message = if restart_required {
            format!("已保存。{} 个字段需重启服务后生效。", restart_fields.len())
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

    /// 导出当前配置（脱敏）：返回整份 config.json 的 JSON。
    ///
    /// # 脱敏清单（**省略**而非掩码，保证「导出 → 导入」往返不破坏真实值）
    ///
    /// - `apiKey`（下游对话密钥）
    /// - `adminApiKey`（管理密钥）
    /// - `proxyPassword`（代理密码）
    /// - `proxyUsername`（代理登录名）
    /// - `countTokensApiKey`（count_tokens 密钥）
    ///
    /// 省略的键由导入端点「保留现值」逻辑承接：导入时这些键缺失（或写掩码）
    /// 即继承当前磁盘值。
    /// 其余字段（host/port/proxyUrl/限流/档位等）原样导出，与面板快照口径一致。
    pub fn export_config(&self) -> Result<serde_json::Value, AdminServiceError> {
        let config = self.token_manager.config();
        let mut value = serde_json::to_value(&*config)
            .map_err(|e| AdminServiceError::InternalError(format!("配置序列化失败: {}", e)))?;
        if let Some(obj) = value.as_object_mut() {
            for key in [
                "apiKey",
                "adminApiKey",
                "proxyPassword",
                "proxyUsername",
                "countTokensApiKey",
            ] {
                obj.remove(key);
            }
        }
        Ok(value)
    }

    /// 导入整份配置（**先校验后写盘**，校验或写盘失败均不破坏现有配置）。
    ///
    /// # 校验（全部在写盘前完成）
    ///
    /// - body 必须是合法 JSON 且能反序列化为 `Config`（缺字段走 serde 默认值）；
    /// - 必填字段：`host` 非空、`port` 1-65535（与 `update_config` 同口径）；
    /// - 敏感字段（apiKey / adminApiKey / proxyPassword / proxyUsername /
    ///   countTokensApiKey）三选一：**显式提供真实值** → 按提供值写入
    ///   （apiKey 显式提供时必须非空，防 fail-open）；
    ///   **省略 / `***` 掩码 / null** → 继承当前磁盘值
    ///   （导出端点省略这五个键，往返不破坏真实值；手改导出文件时
    ///   用 `***` 占位同样不破坏）。
    ///
    /// # 写盘与生效
    ///
    /// 校验全部通过后才写盘：先轮换 .bak 再原子写盘（同 `update_config`），随后
    /// `reload_config` 热应用 + 幂等重挂 TIER2 后台任务。host/port/adminKey 等
    /// 固化字段不热更，响应统一提示「需重启后生效」。
    pub fn import_config(
        self: &Arc<Self>,
        payload: serde_json::Value,
    ) -> Result<ImportConfigResponse, AdminServiceError> {
        // 与 update_config 共用写锁：导入期间的并发更新/导入同样串行化
        let _guard = self.config_write_lock.lock();

        // ① 反序列化即第一道校验（非法 JSON / 结构不符 → 拒绝，零写盘）
        let mut imported: crate::model::config::Config = serde_json::from_value(payload.clone())
            .map_err(|e| {
                AdminServiceError::InvalidCredential(format!("配置 JSON 解析失败: {}", e))
            })?;

        // ② 必填字段校验（与 update_config 同口径）
        if imported.host.trim().is_empty() {
            return Err(AdminServiceError::InvalidCredential(
                "host 不能为空".to_string(),
            ));
        }
        if imported.port == 0 {
            return Err(AdminServiceError::InvalidCredential(
                "port 必须是 1-65535".to_string(),
            ));
        }

        // ②.5 错误码/提示词表校验（与 update_config 同口径，失败整份拒绝零写盘）。
        // 导入是整份替换，错误码表直接进 Config，这里必须先过 validate_error_messages。
        validate_error_messages(&imported.error_messages)
            .map_err(AdminServiceError::InvalidCredential)?;

        // ③ 敏感字段：省略 / `***` 掩码 / null → 保留现值（读磁盘当前值，与
        //    update_config 的 load 同源）。只有**显式提供的真实新值**才覆盖。
        //    （`***` 是导出侧掩码的兜底写法：手改导出文件时不必记得删键。）
        let config_path = self
            .token_manager
            .config()
            .config_path()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| {
                AdminServiceError::InternalError("配置文件路径未知，无法保存配置".to_string())
            })?;
        let current = crate::model::config::Config::load(&config_path)
            .map_err(|e| AdminServiceError::InternalError(format!("加载配置失败: {}", e)))?;
        let obj = payload.as_object();
        let preserved: Vec<&str> = [
            "apiKey",
            "adminApiKey",
            "proxyPassword",
            "proxyUsername",
            "countTokensApiKey",
        ]
        .into_iter()
        .filter(|k| {
            // filter 闭包入参是 &Item（=&&str），先解引用成 &str 再查键
            let k: &str = *k;
            match obj.and_then(|o| o.get(k)) {
                // 键缺失 = 未提供 → 继承现值
                None => true,
                // 键存在但为掩码占位（`***`）或 null = 未提供真实值 → 继承现值
                Some(v) if v.is_null() || v.as_str() == Some("***") => true,
                // 显式真实值 → 覆盖
                _ => false,
            }
        })
        .collect();
        if preserved.contains(&"apiKey") {
            imported.api_key = current.api_key;
        }
        if preserved.contains(&"adminApiKey") {
            imported.admin_api_key = current.admin_api_key;
        }
        if preserved.contains(&"proxyPassword") {
            imported.proxy_password = current.proxy_password;
        }
        if preserved.contains(&"proxyUsername") {
            imported.proxy_username = current.proxy_username;
        }
        if preserved.contains(&"countTokensApiKey") {
            imported.count_tokens_api_key = current.count_tokens_api_key;
        }
        // apiKey 显式提供**真实值**时必须非空（防 fail-open：null/掩码/空串都不放行，
        // 与 update_config 的 userKey 分支同口径；想保留现值请省略该键或用 `***`）。
        if let Some(v) = obj.and_then(|o| o.get("apiKey")) {
            if !(v.is_null() || v.as_str() == Some("***")) {
                let provided = imported.api_key.as_deref().map(|k| k.trim()).unwrap_or("");
                if provided.is_empty() {
                    return Err(AdminServiceError::InvalidCredential(
                        "apiKey 不能为空（显式提供 null/空值会被拒绝，防 fail-open；想保留现值请省略该键或用 *** 掩码）"
                            .to_string(),
                    ));
                }
            }
        }
        if !preserved.is_empty() {
            tracing::info!(
                target: "audit",
                "配置导入，敏感字段省略/掩码已继承现值: {:?}",
                preserved
            );
        }

        // ④ 校验全部通过 → 先轮换备份再原子写盘（此刻起才算生效）
        rotate_config_backup(&config_path);
        imported
            .save()
            .map_err(|e| AdminServiceError::InternalError(format!("保存配置失败: {}", e)))?;

        // ⑤ 热应用：reload_config 换入 ArcSwap + 幂等重挂 TIER2 后台任务
        if let Err(e) = self.token_manager.reload_config() {
            tracing::warn!("配置已导入但热重载失败,下次重启生效: {}", e);
        }
        // ⑥ 鉴权密钥热更：导入显式提供了真实新值 → 播种进 auth_keys 即时生效。
        // 必须在 reload_config 之后（reload 会把 config 里的 key 钉回启动值，
        // auth_keys 才是鉴权活真相源）。setter 拒空，空值保持 fail-closed。
        if let Some(k) = imported.api_key.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            if let Err(e) = crate::common::auth_keys::set_user_key(k) {
                tracing::error!("apiKey 已导入但热更失败，重启后生效: {}", e);
            } else {
                tracing::info!("apiKey 已导入并即时生效（无需重启）");
            }
        }
        if let Some(k) = imported
            .admin_api_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if let Err(e) = crate::common::auth_keys::set_admin_key(k) {
                tracing::error!("adminApiKey 已导入但热更失败，重启后生效: {}", e);
            } else {
                tracing::info!("adminApiKey 已导入并即时生效（无需重启）");
            }
        }
        self.token_manager.respawn_refresh_task();
        self.respawn_balance_task();

        tracing::info!(
            target: "audit",
            "配置已导入（整份替换，敏感字段继承 {} 项）",
            preserved.len()
        );
        Ok(ImportConfigResponse {
            success: true,
            message: "配置已导入并保存；host/port/adminKey 等固化字段需重启服务后生效。"
                .to_string(),
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

    /// 一键重启本服务：Windows/macOS 下进程自重启（spawn detached 助手拉起新二进制）；
    /// 其余平台（Linux）优雅自退，由 systemd 自动重启。
    ///
    /// **Linux 实现方式：优雅自退，让 systemd 自动重启——不需要任何提权。**
    /// 根因（2026-07-08 定位）：systemd unit 设了 `NoNewPrivileges=true`，它会**永久禁止**
    /// 本进程及其子进程通过 setuid 提权，于是旧实现的 `sudo -n systemd-run ...` 静默失败
    /// （后台收到请求、打了日志，但 sudo 无法提权 → 什么都没发生 = "点了没反应"）。
    /// 由于 unit 配了 `Restart=always` + `RestartSec=3`，进程**只要退出**（任意退出码），
    /// systemd 就会在 3 秒内自动重新拉起。因此这里改为：延迟 1 秒（给 HTTP 200 flush 时间）
    /// 后 `std::process::exit(0)`，完全绕开 sudo/NoNewPrivileges，稳定可靠。
    /// 若将来 unit 去掉 Restart=always，此法失效——但当前部署（见 kirostudio.service）已配置。
    ///
    /// **macOS 没有 systemd**（2026-07-27 定位）：早期实现把"非 Windows"等同于"Linux+systemd"，
    /// macOS 下 `exit(0)` 后没有任何监督者会拉起新进程，一键重启/OTA 更新后服务直接消失、
    /// 端口不再监听。故 macOS 单独拆出一支，复用 Windows 同款思路自行 spawn 重启助手。
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

        // macOS：和 Windows 一样没有监督者会在 exit(0) 后自动拉起，且不像 Linux 有 systemd
        // 兜底——同样 spawn 一个 detached 助手，等端口释放后拉起新二进制，再自行退出。
        #[cfg(target_os = "macos")]
        {
            self.spawn_macos_relaunch();
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                tracing::warn!("一键重启(macOS):进程退出,已交给 detached 助手拉起新二进制");
                std::process::exit(0);
            });
            return Ok(());
        }

        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
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

    /// macOS 专用：spawn 一个 detached shell 助手，sleep 后 exec 拉起新二进制。
    ///
    /// 不落地临时脚本文件（Windows 因 cmd `/C` 的多重引号转义问题才需要写 .bat，见
    /// [`spawn_windows_relaunch_process`] 注释）：POSIX shell 用位置参数 `"$0" "$@"` 接收
    /// exe 路径与参数，不做任何字符串拼接/转义，天然规避引号/注入问题。
    /// `trap '' HUP`：若用户是在 Terminal 前台直接跑的（而非 launchd/nohup），关终端触发的
    /// SIGHUP 不该连累刚 spawn、还在 sleep 的助手（及它 exec 顶替出的新进程）。
    /// 不重定向 stdio：助手与 exec 出的新进程沿用当前的 stdout/stderr（终端或已重定向的日志
    /// 文件），保持和重启前一致的日志去向。
    #[cfg(target_os = "macos")]
    fn spawn_macos_relaunch(&self) {
        use std::process::Command;

        // OTA 已把新二进制放到原 exe 路径（rename 旧→.bak、new→原路径）。current_exe 即目标。
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("macOS 自重启:取 current_exe 失败,无法拉起新进程: {e}");
                return;
            }
        };
        // 新进程的工作目录：沿用当前 cwd（config/credentials 相对路径解析依赖它）。
        let cwd = std::env::current_dir().ok();
        let config_path = self
            .token_manager
            .config()
            .config_path()
            .map(|p| p.to_path_buf());
        let credentials_path = self.token_manager.credentials_path();

        // sh -c 'script' 之后的第一个参数是 $0，其余是 $1.. ($@ 不含 $0)——
        // 故把 exe 路径放第一位，"$0" 取到的正是它，"$@" 取到的正是后续的 --config/--credentials。
        let mut args: Vec<std::ffi::OsString> = vec![exe.clone().into_os_string()];
        if let Some(cfg) = &config_path {
            args.push("--config".into());
            args.push(cfg.clone().into_os_string());
        }
        if let Some(cred) = &credentials_path {
            args.push("--credentials".into());
            args.push(cred.clone().into_os_string());
        }

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(r#"trap '' HUP; sleep 3; exec "$0" "$@""#)
            .args(&args);
        if let Some(dir) = &cwd {
            cmd.current_dir(dir);
        }

        match cmd.spawn() {
            Ok(_) => tracing::warn!(
                "macOS 自重启:已 spawn 重启助手(sleep 3s 后拉起 {exe:?}),本进程退出后由它接管端口"
            ),
            Err(e) => tracing::error!(
                "macOS 自重启:spawn 重启助手失败,OTA/一键重启后服务可能不会自动恢复,请手动重启: {e}"
            ),
        }
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
    /// 存储统计：各分区占用 + 进程 RSS（观测项）。
    ///
    /// 磁盘可用空间（disk_free_bytes）暂缺：statvfs 不在 std 里，需要 libc 依赖
    /// （本项目刻意不引新依赖）。TODO：引入 libc 后，在响应里加 disk_free_bytes
    /// （`/` 的 statvfs.f_bavail × f_frsize，Linux/macOS 均可用 std 之外的系统调用）。
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

        // 5) rss：进程常驻内存（Linux 特有，读 /proc/self/status 的 VmRSS，单位 kB）。
        // 非 Linux（macOS/Windows）无 /proc，返回 None → 不展示该分区（前端按内存分区渲染）。
        if let Some(rss_kb) = Self::process_rss_kb() {
            partitions.push(StoragePartition {
                key: "rss".to_string(),
                label: "进程常驻内存 (RSS)".to_string(),
                bytes: rss_kb * 1024,
                items: 0,
                path: None,
                in_memory: true,
            });
        }

        StorageStatsResponse {
            partitions,
            total_disk_bytes,
            usage_enabled,
        }
    }

    /// 诊断快照聚合（`GET /api/admin/diagnostics/snapshot`，纯运维观测）。
    ///
    /// 一键聚合以下维度，全部**零上游**（版本检查除外——远端信息 5s 超时尽力而为）：
    /// - 版本：本地版本恒有，远端最新版本尽力获取（复用 `/update/check` 的检查器）；
    /// - 逐号：复用 `ratelimit_insights` 的 disabled/冷却/健康分/rpm，补余额缓存
    ///   （按账号键，与批量余额端点同源，见 `balance_cache_key`）；
    /// - 池健康：节点表内存统计（「可分配」口径与 `resolve_node_plan` 自动分配对齐）；
    /// - 配置摘要：throttleProfile / 吸收参数等，**刻意脱敏**（不含任何 key/密码/代理地址）；
    /// - 进程：uptime（与 /recovery-metrics 同源）+ RSS（Linux 特有，非 Linux 为 null）。
    pub async fn diagnostics_snapshot(&self) -> DiagnosticsSnapshotResponse {
        let generated_at = chrono::Utc::now().timestamp().max(0) as u64;

        // —— 版本：本地恒有；远端尽力（5s 超时，纯运维端点不阻塞、失败不 500）——
        let version = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            crate::admin::update::check_for_updates(),
        )
        .await
        {
            Ok(upd) => DiagnosticsVersion {
                local_version: upd.local_version,
                latest_version: upd.latest_version,
                has_update: Some(upd.has_update),
                error: upd.error,
            },
            Err(_) => DiagnosticsVersion {
                local_version: env!("CARGO_PKG_VERSION").to_string(),
                latest_version: None,
                has_update: None,
                error: Some("远端版本检查超时（>5s）".into()),
            },
        };

        // —— 逐号：复用限流 insights（disabled/冷却/健康分/rpm），补余额缓存 ——
        let insights = self.ratelimit_insights();
        let credentials = {
            // 锁序对齐 disable_quota_exceeded：balance_cache 锁内调 balance_cache_key
            // （该函数只读 token_manager，与既有调用点同款锁序，见 1032 行先例）。
            let cache = self.balance_cache.lock();
            insights
                .iter()
                .map(|ins| {
                    let key = self.balance_cache_key(ins.id);
                    let (balance_remaining, balance_cached_at) = match cache.get(&key) {
                        Some(c) => (Some(c.data.remaining), Some(c.cached_at)),
                        None => (None, None),
                    };
                    DiagnosticsCredentialEntry {
                        id: ins.id,
                        disabled: ins.disabled,
                        cooldown_remaining_ms: ins.cooldown.as_ref().map(|c| c.remaining_ms),
                        health_score: ins.health.as_ref().map(|h| h.health),
                        circuit_open: ins.health.as_ref().is_some_and(|h| h.circuit_open),
                        balance_remaining,
                        balance_cached_at,
                        rpm: ins.rpm,
                    }
                })
                .collect()
        };

        // —— 池健康：节点表内存统计 ——
        let (total, enabled, assignable, last_test_failed, latency_sum, latency_n) = {
            let nodes = self.socks_nodes.lock();
            let mut enabled = 0usize;
            let mut assignable = 0usize;
            let mut last_test_failed = 0usize;
            let mut latency_sum = 0u64;
            let mut latency_n = 0u64;
            for n in nodes.iter() {
                if n.enabled {
                    enabled += 1;
                    // 「可分配」口径与 resolve_node_plan 自动分配一致：
                    // enabled 且最近一次测活非失败（从未测过的也算，见该方法文档）。
                    if n.last_test.as_ref().is_none_or(|t| t.ok) {
                        assignable += 1;
                    }
                }
                match n.last_test.as_ref() {
                    // 最近一次测活失败（无论当前是否启用，运维都要看到）。
                    Some(t) if !t.ok => last_test_failed += 1,
                    // 最近一次测活成功 → 计入平均延迟样本。
                    Some(t) => {
                        latency_sum += t.latency_ms;
                        latency_n += 1;
                    }
                    None => {}
                }
            }
            (
                nodes.len(),
                enabled,
                assignable,
                last_test_failed,
                latency_sum,
                latency_n,
            )
        };
        let pool_health = DiagnosticsPoolHealth {
            total,
            enabled,
            assignable,
            last_test_failed,
            avg_latency_ms: if latency_n > 0 {
                Some(latency_sum / latency_n)
            } else {
                None
            },
            auto_health_enabled: self
                .socks_auto_health
                .load(std::sync::atomic::Ordering::Relaxed),
        };

        // —— 关键配置摘要（脱敏）：只挑运维排查要看的，不含任何敏感原文 ——
        let cfg = self.token_manager.config();
        let config = DiagnosticsConfigSummary {
            load_balancing_mode: self.token_manager.get_load_balancing_mode(),
            throttle_profile: cfg.throttle_profile,
            scheduling_mode: cfg.scheduling_mode,
            inbound_throttle_enabled: cfg.inbound_throttle_enabled,
            inbound_target_rpm: cfg.inbound_target_rpm,
            upstream_retry_absorb_enabled: cfg.upstream_retry_absorb_enabled,
            upstream_retry_absorb_capacity_400: cfg.upstream_retry_absorb_capacity_400,
            upstream_retry_absorb_budget_secs: cfg.upstream_retry_absorb_budget_secs,
            upstream_retry_absorb_max_rounds: cfg.upstream_retry_absorb_max_rounds,
            cooldown_enabled: cfg.cooldown_enabled,
            rate_limit_enabled: cfg.rate_limit_enabled,
            auto_disable_suspicious: cfg.auto_disable_suspicious,
            auto_disable_quota_exceeded: self
                .auto_disable_quota_exceeded
                .load(std::sync::atomic::Ordering::Relaxed),
            socks_auto_health: self
                .socks_auto_health
                .load(std::sync::atomic::Ordering::Relaxed),
        };

        // —— 进程：uptime（与 /recovery-metrics 同源）+ RSS（非 Linux 为 null）——
        let uptime_ms = crate::common::recovery_metrics::snapshot().uptime_ms;
        let rss_bytes = Self::process_rss_kb().map(|kb| kb * 1024);

        DiagnosticsSnapshotResponse {
            version,
            credentials,
            pool_health,
            config,
            uptime_ms,
            rss_bytes,
            generated_at,
        }
    }

    /// 读取进程常驻内存（RSS，单位 kB）。Linux：`/proc/self/status` 的 `VmRSS` 字段。
    ///
    /// 为什么选 VmRSS 而不是 /proc/self/statm：statm 的字段是「页数」，换算字节需要
    /// 页大小（sysconf 在 libc 里，本项目不引依赖），而 aarch64 可能是 16K/64K 页；
    /// VmRSS 直接给 kB，零换算、全架构正确。文件读不到/解析失败返回 None（不阻断统计）。
    #[cfg(target_os = "linux")]
    fn process_rss_kb() -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                // 形如 "VmRSS:\t  123456 kB"，取第一个空白分隔的数字
                return rest.split_whitespace().next()?.parse().ok();
            }
        }
        None
    }

    /// 非 Linux 平台无 /proc，不支持 RSS 观测，返回 None。
    #[cfg(not(target_os = "linux"))]
    fn process_rss_kb() -> Option<u64> {
        None
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
    /// - trash：`purge_all=false` 走 [`MultiTokenManager::purge_expired_trash`]（按天），
    ///   `true` 走 [`MultiTokenManager::purge_all_trash`]（全清，忽略天数）
    /// - bg_cache：清空内存池
    /// - all：以上全部
    ///
    /// `purge_all` 语义见 [`StorageCleanupRequest::purge_all`]：它存在的原因是回收站
    /// 保留天数的 `0` 已被后台任务占用为「永久保留」，按天数的入参无法表达「立即全清」。
    pub fn storage_cleanup(
        &self,
        target: &str,
        older_than_days: Option<i64>,
        purge_all: bool,
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
            results.push(self.cleanup_trash(older_than_days, purge_all));
        }
        // bg_cache
        if do_all || target == "bg_cache" {
            // 先读池占用再清：清完就无从得知释放了多少（历史缺陷是硬编码 freed_bytes: 0，
            // 面板永远显示「释放 0 字节」，而内存池的占用恰恰是唯一可精确统计的分区）。
            let (_, bytes_before) = crate::admin_ui::bg_pool_stats();
            let n = crate::admin_ui::clear_bg_pool() as u64;
            results.push(StorageCleanupItem {
                key: "bg_cache".to_string(),
                removed: n,
                freed_bytes: bytes_before,
                note: Some(format!("已清空背景图内存池（释放 {} 字节）", bytes_before)),
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

    /// 清理 trash：`purge_all=true` 全清（忽略天数），否则按天数清过期条目
    /// （`older_than_days` 未指定时用 `config.trash_retention_days`）。
    ///
    /// note 里刻意写明「还剩几条」：历史缺陷是按天数清时回 `removed=0`，面板只提示
    /// 「清理完成，共移除 0 项」，用户无法分辨是「没有过期条目」还是「按钮坏了」。
    fn cleanup_trash(&self, older_than_days: Option<i64>, purge_all: bool) -> StorageCleanupItem {
        if purge_all {
            let n = self.token_manager.purge_all_trash() as u64;
            return StorageCleanupItem {
                key: "trash".to_string(),
                removed: n,
                freed_bytes: 0,
                note: Some(format!("已清空回收站全部 {} 条条目（不可恢复）", n)),
            };
        }
        // trash 保留期为 u32 天；older_than_days 为负时按 0 处理（0=永久保留，不清）
        let keep_days: u32 = match older_than_days {
            Some(d) => d.max(0) as u32,
            None => self.token_manager.config().trash_retention_days,
        };
        let n = self.token_manager.purge_expired_trash(keep_days) as u64;
        let remaining = self.token_manager.list_trash().len();
        let note = if keep_days == 0 {
            format!("保留天数为 0（永久保留），未清理任何条目；回收站现有 {remaining} 条。要全部清空请用「全部清空」")
        } else if n == 0 && remaining > 0 {
            format!("回收站现有 {remaining} 条，均未超过 {keep_days} 天，故未清理。要立即清空请用「全部清空」")
        } else {
            format!("已清理 {keep_days} 天前的回收站条目 {n} 条，剩余 {remaining} 条")
        };
        StorageCleanupItem {
            key: "trash".to_string(),
            removed: n,
            freed_bytes: 0,
            note: Some(note),
        }
    }

    // ============ 余额缓存持久化 ============

    fn load_balance_cache_from(
        cache_path: &Option<PathBuf>,
        token_manager: &Arc<MultiTokenManager>,
    ) -> HashMap<String, CachedBalance> {
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

        // ⭐ 旧格式迁移：**按凭据 id 键 → 按账号键**。
        //
        // # 为什么必须迁移（而不是"接受失效"）
        //
        // 缓存键从 `id` 改成 `sha256(apiKey)` 之后，旧文件里的十进制 id 键**永远不会被
        // 命中** ⇒ 升级后 api_key 号的余额全部显示为空 ⇒ 面板集体转圈打
        // `getUsageLimits`。那是 `web_portal` 上游探测，本仓调优结论是绝不为展示类需求
        // 反复打它（线上号池正被风控烧号）。
        //
        // 实测规模：线上 5 条缓存 / 5 个 api_key 号 / **只有 1 个不同的 key** ⇒
        // 迁移后并成 1 条。量级小，但方向是"少打一次上游探测"，且迁移只需十几行。
        //
        // # 并组时取最新的那条
        //
        // N 个 id 映射到同一个账号键时，按 `cached_at` 取最新 —— 它们描述的是同一个账号
        // 同一份配额，旧的那些本来就是冗余副本（这正是本次改动要消除的东西）。
        //
        // # 无法映射的键原样保留
        //
        // OAuth 号的键本来就是 id（`balance_cache_key` 对非 api_key 号回落 id），
        // 以及"号已被删但缓存还在"的残留 —— 两者都原样留着，由展示层的 7 天上限自然淘汰。
        let mut migrated: HashMap<String, CachedBalance> = HashMap::new();
        for (key, v) in map {
            if (now - v.cached_at) >= BALANCE_CACHE_DISPLAY_MAX_AGE_SECS as f64 {
                // 修复：启动恢复用【展示保留上限】(7 天)，而非 5 分钟新鲜度阈值。
                // 这样重启后仍能立刻显示上次的余额数字（前端据 cached_at 标注新鲜度），
                // 而不是因为磁盘缓存 >5 分钟就整批丢成“未知”。只有陈旧到 7 天才丢弃。
                continue;
            }
            // 旧格式判定：键能 parse 成 u64 且该 id 是 api_key 号 ⇒ 需要迁移成账号键。
            // （新格式的账号键是 64 位 hex，parse::<u64> 必然失败，所以不会被误迁。）
            let target = match key.parse::<u64>() {
                Ok(id) => match token_manager.export_credential(id) {
                    Some(c) if c.is_api_key_credential() => match c.kiro_api_key.as_deref() {
                        Some(k) => crate::kiro::token_manager::sha256_hex(k),
                        None => key.clone(),
                    },
                    // 非 api_key 号（OAuth）或号已不在池里 ⇒ 键保持 id，与
                    // `balance_cache_key` 的回落一致。
                    _ => key.clone(),
                },
                Err(_) => key.clone(),
            };
            match migrated.get(&target) {
                // 已有更新的条目 ⇒ 丢弃这条旧副本
                Some(existing) if existing.cached_at >= v.cached_at => {}
                _ => {
                    migrated.insert(target, v);
                }
            }
        }
        migrated
    }

    /// 从磁盘加载代理节点表。
    ///
    /// **fail-soft**：解密/解析失败一律 `warn!` + 空表，绝不 bail。
    /// 理由：at-rest 密钥是机器绑定的，换机/重建 VPS 时 credentials 那条路径是
    /// `exit(1)`（凭据没了服务本来就没意义），但节点表只是候选池 ——
    /// 不该因为它解不开就让整个网关起不来。
    /// 从磁盘加载代理节点表。返回 `(节点表, 是否可安全回写)`。
    ///
    /// **「文件缺失」与「文件在但读不出来」必须分开处理**，这是本函数唯一的要点：
    ///
    /// - 缺失 → 首次启动，空表 + 允许回写。
    /// - 在但解不开/解析失败 → 空表 + **禁止回写**。
    ///
    /// 若两者都按「空表 + 允许回写」处理，就构成一条静默数据毁灭链：启动读不出来
    /// → 内存空表 → 用户加**一个**节点 → `persist_socks_nodes` 把这张只有一条的表
    /// 原子覆盖上去 → 原文件里那 20 个节点和它们的代理密码永久消失。
    /// credentials.json 那条路径是靠 `main.rs` 直接 `exit(1)` 避免同类事故的；
    /// 节点表不该让服务起不来（它只是候选池），所以改用「只读降级」而不是退出。
    fn load_socks_nodes_from(
        path: &Option<PathBuf>,
        token_manager: &Arc<MultiTokenManager>,
    ) -> (Vec<SocksNode>, u64, bool) {
        let path = match path {
            Some(p) => p,
            None => return (Vec::new(), 1, true),
        };
        let raw = match std::fs::read(path) {
            Ok(b) => b,
            // 文件不存在是首次启动的正常状态，不打日志，允许回写。
            Err(_) => return (Vec::new(), 1, true),
        };
        let key_path = crate::common::secret_store::key_path_for(path);
        let text = match crate::common::secret_store::maybe_decrypt_to_string(&raw, &key_path) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(
                    "代理节点表存在但解密失败，已进入**只读降级**（不会覆盖该文件）：{}。\
                     常见原因是 at-rest 密钥丢失或与数据不匹配；修好密钥后重启即恢复。",
                    e
                );
                return (Vec::new(), 1, false);
            }
        };
        match serde_json::from_str::<SocksNodeFileCompat>(&text) {
            Ok(compat) => {
                let (v, next_id) = compat.normalize();
                // 超限**不截断**：截断后第一次回写就把多出来的永久删掉。
                // 只拒绝新增（见 upsert 的上限判断），已有的照常可用。
                if v.len() > MAX_SOCKS_NODES {
                    tracing::warn!(
                        "代理节点表有 {} 条，超过上限 {}：全部保留可用，但不再允许新增",
                        v.len(),
                        MAX_SOCKS_NODES
                    );
                }
                let _ = token_manager; // 预留：将来按节点校验凭据绑定
                (v, next_id, true)
            }
            Err(e) => {
                tracing::error!(
                    "代理节点表存在但解析失败，已进入**只读降级**（不会覆盖该文件）：{}",
                    e
                );
                (Vec::new(), 1, false)
            }
        }
    }

    /// 只读降级检查，**必须在改内存之前调用**。
    ///
    /// 为什么不能只靠 `persist_socks_nodes` 那道判断：那道判断在**改完内存之后**才跑，
    /// 于是只读降级下的一次 upsert 会「内存里真的多出一个节点 + 调用方收到报错」——
    /// 面板列表从此显示一个磁盘上并不存在的节点，直到重启才消失，
    /// 而用户看到的是「保存失败但它出现了」，只会以为报错是假的、节点是真的。
    /// 三个写入方法（upsert / delete / record_test）都在顶部调它，先判后改。
    fn ensure_socks_writable(&self) -> Result<(), AdminServiceError> {
        // path 为 None 是纯内存态（单凭据格式），此时 writable 恒 true，与 persist 同口径。
        if self.socks_nodes_path.is_some() && !self.socks_nodes_writable {
            return Err(AdminServiceError::InternalError(
                "代理节点表处于只读降级（启动时该文件解密/解析失败）：\
                 为避免覆盖原文件，本次修改未落盘。请修复 at-rest 密钥后重启。"
                    .into(),
            ));
        }
        Ok(())
    }

    /// 回写代理节点表（含密码，故与 credentials/trash 同开关同密钥做 at-rest 加密）。
    ///
    /// 两条护栏：
    /// 1. **只读降级时拒绝写**（`socks_nodes_writable=false`）—— 启动时文件读不出来，
    ///    内存里是空表，写下去就等于把原文件抹平。
    /// 2. **序列化与写盘在同一把锁内**：先前把序列化放锁内、写盘放锁外，两个并发
    ///    修改会各自持有一份快照，后完成的那次写把先完成的改动覆盖掉（丢写）。
    fn persist_socks_nodes(&self) -> Result<(), AdminServiceError> {
        let path = match &self.socks_nodes_path {
            Some(p) => p,
            // 单凭据格式：纯内存态（与 trash 同款约定）。
            None => return Ok(()),
        };
        if !self.socks_nodes_writable {
            return Err(AdminServiceError::InternalError(
                "代理节点表处于只读降级（启动时该文件解密/解析失败）：\
                 为避免覆盖原文件，本次修改未落盘。请修复 at-rest 密钥后重启。"
                    .into(),
            ));
        }
        let enc = self.token_manager.config().encrypt_credentials_at_rest;
        let key_path = crate::common::secret_store::key_path_for(path);
        // ⭐ 整段在锁内：序列化 → 编码 → 原子写。放开锁再写会丢写（见上）。
        let nodes = self.socks_nodes.lock();
        let file = crate::kiro::model::socks_node::SocksNodeFile {
            nodes: nodes.clone(),
            next_id: self
                .socks_next_id
                .load(std::sync::atomic::Ordering::Relaxed),
        };
        let json = serde_json::to_string_pretty(&file)
            .map_err(|e| AdminServiceError::InternalError(format!("序列化节点表失败: {e}")))?;
        let (bytes, encrypted) =
            crate::common::secret_store::encode_for_disk(json.as_bytes(), enc, &key_path);
        // 与 token_manager 的 persist_credentials 同口径：开了加密却落成明文时
        // 必须把面板的 at-rest 健康灯打灭，否则密码明文落盘而界面显示一切正常。
        crate::common::recovery_metrics::set_at_rest_healthy(!enc || encrypted);
        crate::common::fs_atomic::write_atomic(path, &bytes)
            .map_err(|e| AdminServiceError::InternalError(format!("回写节点表失败: {e}")))?;
        Ok(())
    }

    /// 列出所有代理节点。**密码恒不外传**，只给 `hasPassword` 布尔。
    ///
    /// 同时带上「这个节点上已挂了几个凭据」（`boundCredentials`）：前端的节点下拉与
    /// 「自动分配」按钮按它排序，必须与 `resolve_node_plan` 的自动分配同一口径，
    /// 否则推荐顺序与实际分配结果不一致。计数表一次算好复用给全部节点（O(凭据数)），
    /// 且在 `socks_nodes` 锁**之外**取（避免与 token_manager.entries 构成新锁序）。
    pub fn list_socks_nodes(&self) -> Vec<SocksNodeView> {
        let usage = self.token_manager.proxy_url_usage();
        self.socks_nodes
            .lock()
            .iter()
            .map(|n| SocksNodeView::from_node(n, usage.get(&n.url).copied().unwrap_or(0)))
            .collect()
    }

    /// 批量导入代理节点（整段粘贴节点商文档）。
    ///
    /// 返回四个聚合计数 + **逐行结果**（见 [`SocksNodeBulkImportOutcome`]）。
    ///
    /// # 为什么要逐行结果
    ///
    /// 原先只返回四个数，其中「跳过数」= 非链接行 + SSRF 拒绝 —— 用户看到「跳过 10 行」
    /// 时无法区分「这行不是链接」「这行端口写错了」「这行地址是内网被拦了」，
    /// 三者需要的动作完全不同。逐行结果让每一行都带上行号、脱敏原文和原因码。
    ///
    /// # 设计取舍
    ///
    /// - **默认不启用**（`enabled` 由调用方给，前端默认 false）：新导入的节点还没测活，
    ///   直接参与分配会把未验证的出口塞给分身。与「生成分身时是否全部默认启用」同一原则。
    /// - **URL 去重**：同一节点在节点商文档里会出现两次（整段区 + 明细区）。
    ///   已在表里的 url 直接跳过，**不覆盖**已有节点的账密/启用状态 ——
    ///   覆盖会把一个已测活启用的节点重置成未启用。
    /// - **SSRF 校验逐条做**，任一条不过只跳过它，不让整批失败
    ///   （用户粘的是一大段，为一行内网地址废掉整批很难用）。
    pub async fn bulk_import_socks_nodes(
        &self,
        text: &str,
        enabled: bool,
    ) -> Result<SocksNodeBulkImportOutcome, AdminServiceError> {
        self.ensure_socks_writable()?;
        let report = crate::http_client::parse_proxy_lines_report(text);
        let has_parsable = report.items.iter().any(|i| i.link.is_some());
        if !has_parsable {
            // 一条都解析不出来时仍报错（保持既有行为：前端据此弹 error toast）。
            // 但把**失败原因**带上 —— 原先只说「跳过 N 行非链接文本」，
            // 而真实原因常常是端口写错或格式判不定。
            let why = report
                .items
                .iter()
                .filter_map(|i| i.issue.map(|e| format!("第 {} 行 {}", i.lineno, e.code())))
                .take(5)
                .collect::<Vec<_>>()
                .join("；");
            let tail = if why.is_empty() {
                String::new()
            } else {
                format!("。可疑行：{why}")
            };
            return Err(AdminServiceError::InvalidCredential(format!(
                "没有解析出任何节点（跳过 {} 行非链接文本）。\
                 期望形如 socks://<base64 或 user:pass>@host:port#名字，\
                 或 host:port:user:pass{tail}",
                report.skipped
            )));
        }

        let mut added = 0usize;
        let mut dup = 0usize;
        let mut over_cap = 0usize;
        let mut rejected = 0usize;
        let mut items: Vec<SocksNodeBulkImportItem> = Vec::with_capacity(report.items.len());

        for it in report.items {
            let lineno = it.lineno;
            let raw = it.raw;
            let p = match it.link {
                Some(p) => p,
                None => {
                    // 解析失败：原因码原样带回（前端做 i18n 映射）。
                    let code = it
                        .issue
                        .map(|e| e.code().to_string())
                        .unwrap_or_else(|| "invalid".to_string());
                    items.push(SocksNodeBulkImportItem {
                        lineno,
                        raw,
                        status: "invalid".into(),
                        reason: Some(code),
                        address: None,
                        username: None,
                    });
                    continue;
                }
            };
            let address = Some(p.url.clone());
            let username = p.username.clone();
            // 同一次粘贴内重复：与「已在池中」同样算 duplicate（对用户是同一件事）。
            if it.dup_in_paste {
                dup += 1;
                items.push(SocksNodeBulkImportItem {
                    lineno,
                    raw,
                    status: "duplicate".into(),
                    reason: Some("dup_in_paste".into()),
                    address,
                    username,
                });
                continue;
            }
            // 已存在（按 url）→ 跳过，绝不覆盖既有节点的账密/启用状态。
            if self.socks_nodes.lock().iter().any(|n| n.url == p.url) {
                dup += 1;
                items.push(SocksNodeBulkImportItem {
                    lineno,
                    raw,
                    status: "duplicate".into(),
                    reason: Some("already_in_pool".into()),
                    address,
                    username,
                });
                continue;
            }
            // SSRF：逐条校验，不过则只跳过这一条（await 必须在锁外）。
            if let Err(e) = crate::common::ssrf::validate_proxy_address(&p.url).await {
                // 只跳过这一条并告警：用户粘的是一大段，为一行内网地址废掉整批很难用。
                tracing::warn!("批量导入跳过节点 {}（地址校验未通过）: {}", p.url, e);
                rejected += 1;
                items.push(SocksNodeBulkImportItem {
                    lineno,
                    raw,
                    status: "invalid".into(),
                    // 与解析失败区分开：地址本身合法，是**策略**拦下的。
                    reason: Some("address_rejected".into()),
                    address,
                    username,
                });
                continue;
            }
            let mut nodes = self.socks_nodes.lock();
            if nodes.len() >= MAX_SOCKS_NODES {
                over_cap += 1;
                items.push(SocksNodeBulkImportItem {
                    lineno,
                    raw,
                    status: "over_capacity".into(),
                    reason: Some("over_capacity".into()),
                    address,
                    username,
                });
                continue;
            }
            let id = self
                .socks_next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            nodes.push(SocksNode {
                id,
                name: p.name.clone().unwrap_or_default(),
                url: p.url.clone(),
                username: p.username.clone(),
                password: p.password.clone(),
                enabled,
                last_test: None,
                created_at: Utc::now().timestamp().max(0) as u64,
            });
            added += 1;
            items.push(SocksNodeBulkImportItem {
                lineno,
                raw,
                status: "ok".into(),
                reason: None,
                address,
                username,
            });
        }

        if added > 0 {
            self.persist_socks_nodes()?;
        }
        Ok(SocksNodeBulkImportOutcome {
            added,
            // 保持旧口径：非链接行 + SSRF 拒绝。含义比字面宽，
            // 精确归因看 `items`（这正是加它的理由）。
            skipped: report.skipped + rejected,
            duplicate: dup,
            over_capacity: over_cap,
            items,
        })
    }

    /// 新建或更新一个代理节点。
    ///
    /// `id = None` → 新建；`Some(existing)` → 更新；`Some(不存在)` → NotFound
    /// （**不静默新建**：那会把一次误传的 id 变成一个用户没预期的新节点）。
    pub async fn upsert_socks_node(
        &self,
        req: SocksNodeUpsertRequest,
    ) -> Result<u64, AdminServiceError> {
        // ⭐ 先判只读降级再改内存：否则内存表会多出一个磁盘上不存在的节点（见 ensure_socks_writable）。
        self.ensure_socks_writable()?;
        // 账密从 URL 里拆出来，避免密码明文留在 url 字段里（与 set_credential_proxy 同口径）。
        let raw = req.url.trim();
        if raw.is_empty() {
            return Err(AdminServiceError::InvalidCredential("url 不能为空".into()));
        }
        // ⭐ 先试**分享链接**解析（`socks://base64(user:pass)@host:port#name`）——
        // 机场/节点商下发的就是这个形式，而 `split_proxy_credentials` 只做百分号解码，
        // 会把整个 base64 串当成用户名、密码为 None ⇒ 代理认证必然失败，
        // 而那个失败长得像「节点不通」，会把排障带偏。`#name` 还会残留在 URL 里污染 host。
        //
        // 解析不出（普通 `socks5://host:port` 或已拆好账密的表单提交）时回落原路径，
        // 行为逐字不变。
        let (clean_url, inline_user, inline_pass, link_name) =
            match crate::http_client::parse_proxy_link(raw) {
                Some(p) => (p.url, p.username, p.password, p.name),
                None => {
                    let (u, iu, ip) = crate::http_client::split_proxy_credentials(raw);
                    (u, iu, ip, None)
                }
            };

        // 拦内网/环回：节点地址会被写进凭据并在热路径上使用。
        // 策略是 SsrfPolicy::AdminConfigured（与 custom_api base_url 同口径）：管理员亲手填的
        // 目标，只放开 198.18.0.0/15 那一段 —— 那是 Clash/Mihomo 的 fake-IP 池默认段，
        // 用 Strict 会让开了 fake-IP 的机器一个域名形式的节点都加不进来。
        // ⚠️ 环回与 RFC1918 **仍然被拒**（本机 ssh -D 隧道 / 局域网旁车加不进来），
        // 这是当前的已知限制，不是 AdminConfigured 能解决的 —— 见 validate_proxy_address 文档。
        // ⚠️ 这**不是**安全边界（DNS 失败放行、不在使用时复验、且 set_credential_proxy
        // 与 /proxy/test 两条旁路完全不校验）—— 见 validate_proxy_address 的文档。
        crate::common::ssrf::validate_proxy_address(&clean_url)
            .await
            .map_err(AdminServiceError::InvalidCredential)?;

        let username = req
            .username
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| inline_user.clone());

        let mut nodes = self.socks_nodes.lock();

        let id = match req.id {
            Some(id) => {
                let node = nodes
                    .iter_mut()
                    .find(|n| n.id == id)
                    .ok_or(AdminServiceError::NotFound { id })?;
                // 名字优先级：显式 req.name > 分享链接的 #fragment > 保持原值。
                node.name = req
                    .name
                    .clone()
                    .or_else(|| link_name.clone())
                    .unwrap_or_else(|| node.name.clone());
                node.url = clean_url;
                // ⭐ 分享链接自带账密时，即使 req.username/password 都省略也要写入 ——
                // 编辑场景下用户粘一条新链接进来，期望的是"整条替换"，而三态语义
                // （省略=不改）会让新链接的账密被丢弃、继续用旧的 ⇒ 认证失败。
                if req.username.is_none() {
                    if let Some(u) = inline_user.clone() {
                        node.username = Some(u);
                    }
                }
                if req.password.is_none() {
                    if let Some(p) = inline_pass.clone() {
                        node.password = Some(p);
                    }
                }
                // 用户名与密码同款三态：**省略该键 = 不改**，`Some("") = 清空`。
                // 先前这里是无条件赋值，于是只发 {id,url,enabled} 的更新会把用户名
                // 抹成 None 而密码留着 → `build_client` 的 `if let (Some(u), Some(p))`
                // 不成立 → 认证被静默丢弃 → 该节点此后全部连不上。
                match req.username.as_ref() {
                    None => {}
                    Some(u) if u.is_empty() => node.username = None,
                    Some(_) => node.username = username,
                }
                // ⭐ 密码语义：**省略该键 = 不改**，`Some("") = 清空`。
                // 绝不能写成必填 —— 那样「改个节点名」就会把密码抹掉，
                // 已绑该节点的分身全部掉线（GET 抹密码 + 前端整体回填的经典坑）。
                match req.password.as_ref() {
                    None => {}
                    Some(p) if p.is_empty() => node.password = None,
                    Some(p) => node.password = Some(p.clone()),
                }
                if let Some(en) = req.enabled {
                    node.enabled = en;
                }
                // 手动启用即视为「已人工确认恢复」：清零自动健康调度的失败计数，
                // 否则刚手动拉起的节点会背着历史失败数、下轮探测失败一次就被禁。
                // 计数只存活于内存（见 `socks_fail_counts` 字段注释），此处仅做 remove。
                if req.enabled == Some(true) {
                    self.socks_fail_counts.lock().remove(&id);
                }
                id
            }
            None => {
                if nodes.len() >= MAX_SOCKS_NODES {
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "节点数已达上限 {MAX_SOCKS_NODES}"
                    )));
                }
                // id 从持久化高水位取，**不用** `max(现有 id)+1` —— 后者在删掉
                // 最大 id 的节点后会把该 id 重新发出去，让仍持有旧列表的面板标签页
                // 指向一个无关新节点。
                let id = self
                    .socks_next_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                nodes.push(SocksNode {
                    id,
                    // 名字优先级：显式 req.name > 分享链接的 #fragment > 空（前端回落 host:port）。
                    // 用 #fragment 当名字是刻意的：粘一条链接进来就能得到「US-1-SOCKS5」
                    // 这种可读标签，而不是一列长得一样的 IP。
                    name: req
                        .name
                        .clone()
                        .or_else(|| link_name.clone())
                        .unwrap_or_default(),
                    url: clean_url,
                    username,
                    password: req
                        .password
                        .clone()
                        .filter(|s| !s.is_empty())
                        .or(inline_pass),
                    enabled: req.enabled.unwrap_or(true),
                    last_test: None,
                    created_at: Utc::now().timestamp().max(0) as u64,
                });
                id
            }
        };
        drop(nodes);
        self.persist_socks_nodes()?;
        Ok(id)
    }

    /// 删除一个代理节点。**不动已绑该节点的凭据** —— 凭据的 `proxy_*` 是独立的绑定
    /// 结果，删节点只是把它从候选池移除；否则删一个节点会让一批分身当场掉线。
    pub fn delete_socks_node(&self, id: u64) -> Result<bool, AdminServiceError> {
        // ⭐ 先判后改：只读降级下删除若先动内存，节点会从面板消失但磁盘上还在。
        self.ensure_socks_writable()?;
        let removed = {
            let mut nodes = self.socks_nodes.lock();
            let before = nodes.len();
            nodes.retain(|n| n.id != id);
            before != nodes.len()
        };
        if removed {
            self.persist_socks_nodes()?;
        }
        Ok(removed)
    }

    /// 写回某节点的测速结果（由 `/socks/nodes/{id}/test` 调用）。
    pub fn record_socks_node_test(
        &self,
        id: u64,
        test: SocksNodeTest,
    ) -> Result<(), AdminServiceError> {
        // ⭐ 先判后改：只读降级下写测速结果若先动内存，面板会显示一个不会被持久化的结果。
        self.ensure_socks_writable()?;
        {
            let mut nodes = self.socks_nodes.lock();
            let node = nodes
                .iter_mut()
                .find(|n| n.id == id)
                .ok_or(AdminServiceError::NotFound { id })?;
            node.last_test = Some(test);
        }
        self.persist_socks_nodes()
    }

    /// 取某节点的完整代理配置（含密码），供测速与「一键生成分身」使用。
    pub fn socks_node_proxy(&self, id: u64) -> Option<(String, Option<String>, Option<String>)> {
        self.socks_nodes
            .lock()
            .iter()
            .find(|n| n.id == id)
            .map(|n| (n.url.clone(), n.username.clone(), n.password.clone()))
    }

    /// 算出本次多开的「节点 → 份」分配计划。**纯函数式**：只读节点表，不改任何状态。
    ///
    /// 两条来源二选一：
    /// - `node_ids` 非空 → **只用这些**，按给定顺序；无效的逐条剔除并记进
    ///   [`NodePlan::rejected`]（响应文案要点名，见下）。
    /// - 缺省 / 空数组 → 自动分配：池里全部 `enabled` 节点，按插入顺序。
    ///
    /// 两条都**截断到 `cap` 个**（多余的节点忽略），且都**不复用**：节点不够时多出来
    /// 的份直连。复用同一节点的两份共用一个出口 IP，等于没分散却让人以为分散了。
    ///
    /// `cap` 是「本次有几份会真的去消费节点」，由调用方算：主份参与时 = `copies`，
    /// 不参与时 = `copies - 1`。传 `copies` 会在主份不参与时多留一个永不被消费的节点在
    /// 计划里，`rejected` 文案就跟着不准（说得上「有效」的 id 其实没人用）。
    ///
    /// # 自动分配的顺序：先按「这个节点上已挂了几个凭据」升序，再按延迟升序
    ///
    /// 分身的意义就是分散出口，把多份塞进同一个节点等于没分。而池里的节点常常已经
    /// 被前几批分身占了 —— 按插入顺序取会一直命中最前面那几个（它们正是被占最多的），
    /// 于是"分散"只发生在本批内部，跨批看仍然挤在同几个出口上。
    ///
    /// 「已挂几个」是**启发式**（按 `proxy_url` 字符串比对，见
    /// [`crate::kiro::token_manager::MultiTokenManager::proxy_url_usage`]）：手工填过
    /// 代理的号可能因 scheme 未归一而漏算。漏算方向安全 —— 顶多是把一个已被占的节点
    /// 当空闲用，而那正是节点不足时的既有行为。
    ///
    /// 延迟取自 `last_test.latency_ms`；**从未测过**（`last_test = None`）的节点排在
    /// 所有测过的后面（当 `u64::MAX` 用）而不是被排除 —— 全新池子里所有节点都没测过，
    /// 排除等于池空、全部落直连。同延迟按 id 升序兜底，让顺序稳定可测。
    ///
    /// **最近测活失败**（`last_test.ok == false`）的节点被排除在自动分配之外：
    /// 已知不通的出口分出去只会让那一份必然失败。显式 `node_ids` 不受此限
    /// （用户点名要的节点不该被静默跳过 —— 那正是下面这段说的"静默替换"的另一面）。
    ///
    /// # 无效 id 为什么必须剔除而不是替换
    ///
    /// 「我选了节点却仍然直连」是这条路最容易踩空的地方，而**静默换一个节点**更糟：
    /// 用户以为出口是他挑的那个。故这里只剔除并记下原因，由调用方在响应文案里点名。
    ///
    /// 重复 id 同样记进 `rejected`（`重复`）：两份用同一个节点就是上面那条"复用"，
    /// 调用方显式写两遍也不例外——只是这次要说出来。
    fn resolve_node_plan(
        &self,
        node_ids: Option<&[u64]>,
        cap: usize,
        exclude_id: Option<u64>,
    ) -> NodePlan {
        let mut plan = NodePlan::default();
        match node_ids.filter(|ids| !ids.is_empty()) {
            Some(ids) => {
                let nodes = self.socks_nodes.lock();
                let mut seen: Vec<u64> = Vec::new();
                for id in ids {
                    // ⚠️ 先判重复再查表：`[5, 5]` 里第二个 5 是"重复"而不是"不存在"。
                    // `exclude_id`（已给主份的那个）也算重复：它已经是某一份的出口了。
                    if seen.contains(id) || exclude_id == Some(*id) {
                        plan.rejected.push((*id, "重复"));
                        continue;
                    }
                    seen.push(*id);
                    match nodes.iter().find(|n| n.id == *id) {
                        // 显式指定**也**要看 enabled：否则「禁用节点」这个开关在这条路上
                        // 形同不存在，用户关掉的出口还会被用上。
                        Some(n) if !n.enabled => plan.rejected.push((*id, "已禁用")),
                        Some(n) => plan.assignments.push((
                            n.url.clone(),
                            n.username.clone(),
                            n.password.clone(),
                        )),
                        None => plan.rejected.push((*id, "不存在")),
                    }
                }
            }
            None => {
                // 「每个出口 URL 上已挂几个凭据」——排序主键。在锁外先算，避免同时
                // 持有 socks_nodes 与 token_manager.entries 两把锁（全仓无此锁序）。
                let usage = self.token_manager.proxy_url_usage();
                // 排序键：(已挂数, 延迟, id)。`last_test = None` 的延迟当 MAX（排最后但**不排除**）。
                let mut ranked: Vec<(usize, u64, u64, (String, Option<String>, Option<String>))> =
                    self.socks_nodes
                        .lock()
                        .iter()
                        .filter(|n| n.enabled)
                        // 已知测活失败的不参与自动分配（从未测过的仍参与，见方法文档）。
                        .filter(|n| n.last_test.as_ref().is_none_or(|t| t.ok))
                        // 已经给主份的那个节点不再进计划：否则它会被再分一次，
                        // 两份共用一个出口 —— 那正是本函数刻意不做的"复用"。
                        .filter(|n| exclude_id != Some(n.id))
                        .map(|n| {
                            (
                                usage.get(&n.url).copied().unwrap_or(0),
                                n.last_test.as_ref().map_or(u64::MAX, |t| t.latency_ms),
                                n.id,
                                (n.url.clone(), n.username.clone(), n.password.clone()),
                            )
                        })
                        .collect();
                ranked.sort_by_key(|(used, latency, id, _)| (*used, *latency, *id));
                plan.assignments = ranked.into_iter().map(|(_, _, _, proxy)| proxy).collect();
            }
        }
        plan.assignments.truncate(cap);
        plan
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

        // 凭据重复（refreshToken/kiroApiKey 与池中已有冲突）——独立判别，与 restore
        // 路径（classify_trash_error）同口径，前端可据 error.type 处置。
        if msg.contains("凭据已存在") || msg.contains("refreshToken 重复") || msg.contains("kiroApiKey 重复")
        {
            return AdminServiceError::DuplicateCredential(msg);
        }

        // 凭据验证失败（refreshToken 无效、格式错误等）
        let is_invalid_credential = msg.contains("缺少 refreshToken")
            || msg.contains("refreshToken 为空")
            || msg.contains("refreshToken 已被截断")
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
            // 重复类独立判别：前端「自动强制恢复」按 error.type 触发（settings-page），
            // 不依赖「重复」字样的中文文案。
            AdminServiceError::DuplicateCredential(msg)
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
        assert_eq!(build_insight_text(1, 3, 50, false, true, false, None), "畅通");
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
        let text = build_insight_text(54, 40, 50, false, true, false, Some(&cd));
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
        let text = build_insight_text(7, 0, 50, false, true, false, Some(&cd));
        assert_eq!(text, "#7 冷却中（服务器错误）剩5s");
    }

    /// 已达软上限 + 硬门生效(balanced+池>1) → "已达软上限，建议分流"
    #[test]
    fn insight_saturated_gate_active() {
        let text = build_insight_text(54, 50, 50, true, true, false, None);
        assert_eq!(text, "#54 近60s 50/50 已达软上限，建议分流");
    }

    /// 接近软上限（>=80%）+ 硬门生效 → "接近软上限，建议分流"
    #[test]
    fn insight_near_saturation_gate_active() {
        // 40/50 = 80%
        let text = build_insight_text(54, 40, 50, false, true, false, None);
        assert_eq!(text, "#54 近60s 40/50 接近软上限，建议分流");
    }

    /// rpm_limit=0（不限制）时永不判为接近上限，恒"畅通"（与 gate_active 无关）
    #[test]
    fn insight_no_limit_always_clear() {
        assert_eq!(build_insight_text(9, 999, 0, false, true, false, None), "畅通");
        assert_eq!(build_insight_text(9, 999, 0, false, false, false, None), "畅通");
    }

    /// ⭐回归(#虚假饱和告警)：硬门不生效(priority 模式 / 单号池)时，即便 rpm 已达/超过
    /// 阈值，也不能再说"建议分流"——priority 模式下这个阈值对调度没有任何拦截力,
    /// "分流"这个词本身就是误导(根本没有第二个号可分)。改口引导去查上游账户级限流。
    /// 旧代码里 `saturated` 参数一旦为 true 就无条件走"已达软上限，建议分流"分支，
    /// 与 gate_active 完全无关——本测试对着新签名传 gate_active=false 会触发新分支，
    /// 证明新逻辑确实按 gate_active 分岔（旧函数体没有这个参数，编译都过不了，
    /// 这本身就是最强的"旧代码会失败"证据：旧调用点全是 6 个参数）。
    #[test]
    fn insight_saturated_but_gate_inactive_does_not_say_spillover() {
        let text = build_insight_text(54, 51, 25, true, false, false, None);
        assert_eq!(
            text,
            "#54 近60s 51/25 超过软上限，但当前调度模式下无分流对象，疑似上游账户级限流，建议加号或降低并发"
        );
        assert!(!text.contains("建议分流"), "硬门未生效时绝不能出现\"建议分流\"字样: {text}");
    }

    /// 同理:接近软上限但硬门未生效，也不该说"建议分流"。
    #[test]
    fn insight_near_saturation_but_gate_inactive_does_not_say_spillover() {
        let text = build_insight_text(54, 20, 25, false, false, false, None);
        assert!(!text.contains("建议分流"), "硬门未生效时接近上限也不该建议分流: {text}");
        assert!(text.contains("接近软上限"), "仍应保留接近上限的事实描述: {text}");
    }

    /// 已禁用号:显示"已禁用"而非"畅通"(即便有 RPM/未冷却)
    #[test]
    fn insight_disabled() {
        assert_eq!(
            build_insight_text(54, 0, 50, false, true, true, None),
            "#54 已禁用（不参与调度）"
        );
    }
}

#[cfg(test)]
mod multi_open_copies_tests {
    //! 多开份数归一。份数是**外部可控输入**且直接决定本次请求会建多少条凭据，
    //! 故硬上限必须有测试锁住 —— 去掉 clamp 后 `copies_above_cap_is_clamped` 必失败。
    use super::balance_cache_tests::mk_service_with_one_credential;
    use super::*;

    #[test]
    fn absent_or_one_means_normal_single_add() {
        // 字段缺失（老客户端 / 普通上号）必须等价于 1，行为与该字段不存在时完全一致。
        assert_eq!(effective_copies(None), 1);
        assert_eq!(effective_copies(Some(1)), 1);
    }

    #[test]
    fn zero_is_normalized_to_one_not_zero_copies() {
        // 0 若原样透传会让 `2..=0` 成为空区间——第 1 份仍建、循环不执行，
        // 结果"看起来对"但语义含糊。显式归一为 1，让 0 与缺失同义。
        assert_eq!(effective_copies(Some(0)), 1);
    }

    #[test]
    fn copies_above_cap_is_clamped() {
        // ⭐ 承重断言：无上限时 `{"copies": 999}` 会真建 999 条同账号凭据，
        // 而它们共用一份上游配额 —— 不是更高并发，是把调度器塞满。
        assert_eq!(effective_copies(Some(999)), MAX_CREDENTIAL_COPIES);
        assert_eq!(effective_copies(Some(u32::MAX)), MAX_CREDENTIAL_COPIES);
        // 边界：正好等于上限时不被改动。
        assert_eq!(
            effective_copies(Some(MAX_CREDENTIAL_COPIES)),
            MAX_CREDENTIAL_COPIES
        );
    }

    #[test]
    fn typical_multi_open_value_passes_through() {
        assert_eq!(effective_copies(Some(4)), 4);
    }

    /// ⭐ 源码级守卫：多开时 **`api_region` 必须继承父号**。
    ///
    /// 这是一条**线上真实发生过**的缺陷，而且我自己先误判成了「这个 key 不支持分身」：
    ///
    /// 分身请求通常只带 `authMethod` + `kiroApiKey` + `copies`，于是 `api_region` 为 None。
    /// 而 CLI 端点的 host 是 `q.{api_region}.amazonaws.com`（`endpoint/cli.rs`），
    /// 拿不到就回退 config 默认（us-east-1）—— 但 `ksk_` token 是**按 region 授权**的，
    /// 于是上游回 403 `AccessDeniedException: The bearer token included in the request is invalid.`
    ///
    /// 实测对照（同一个 key、同一批代理）：
    /// - 不继承 region → 4 个分身 **0% 成功、100% auth_failed**
    /// - 继承 region   → 同一批分身 **83% / 45% / 100% / 88%**
    ///
    /// 用源码守卫而非行为测试：`add_credential` 会打真实上游（`get_usage_limits_for`），
    /// 而本仓铁律禁止测试依赖网络。
    /// `POST /credentials/{id}/api-region` 必须存在且挂在鉴权路由树内。
    ///
    /// 补的是真实运维缺口：`ksk_` 按 region 授权、打错区恒 403 且**永不自愈**，
    /// 而此前全仓没有任何修改 `api_region` 的入口 —— `/regions` 与 `/switch-region`
    /// 都是 ARN 门控（只对有 `profileArn` 的 external_idp 号有意义）⇒
    /// api_key 号 region 错了**只能删号重建**。
    /// 实测 2026-08-05 02:42：4 个分身因缺 region 被打成 `TooManyFailures`，
    /// 运维手上没有「补 region 再启用」的手段。
    #[test]
    fn api_region_setter_endpoint_is_wired() {
        let router = include_str!("router.rs");
        // ⚠️ 判据必须**对空白不敏感**：原写法把路径与 handler 拼成一整行去 contains，
        // 而 rustfmt 会把这条 `.route(..)` 拆成三行（超过 fn_call_width）⇒ 一跑 fmt 就
        // 假红。这不是路由掉了，是守卫写脆了。折叠空白后再比，语义（路径→handler 的
        // 绑定关系）一个不少。同文件的 `clone_endpoint_is_registered_in_router`
        // 是分开断言的，同一意图两种写法，这里对齐成不脆的那种。
        let compact: String = router.chars().filter(|c| !c.is_whitespace()).collect();
        // needle 运行时拼接，避免 include_str! 自匹配。
        let route = format!(
            "\"/credentials/{{id}}/api-region\",post(set_credential_api_region{}",
            ")"
        );
        assert!(
            compact.contains(&route),
            "必须注册 POST /credentials/{{id}}/api-region，否则 api_key 号 region 错了只能删号重建"
        );
        // 校验必须存在：污染值会拼出 q.{垃圾}.amazonaws.com / runtime.{垃圾}.kiro.dev，
        // DNS 失败或 502 —— 而那个失败长得像「号坏了」，会把排查带偏。
        let tm = include_str!("../kiro/token_manager.rs");
        let cut = tm.find("#[cfg(test)]").unwrap_or(tm.len());
        let prod = &tm[..cut];
        let fname = format!("pub fn set_credential_api_region{}", "(");
        let fi = prod
            .find(&fname)
            .expect("token_manager 侧 setter 不该被改名");
        let body_end = prod[fi..]
            .find("\n    pub fn ")
            .map(|i| i + fi)
            .unwrap_or(prod.len());
        let body = &prod[fi..body_end];
        let guard = format!("is_supported_region{}", "(r)");
        assert!(
            body.contains(&guard),
            "setter 必须过 is_supported_region 白名单：污染 region 会拼出无法解析的 host，\
             而那个失败长得像「号坏了」会把排查带偏"
        );
    }

    /// 🔴 承重：`AccountThrottled` **绝不能**导致 `new_cred.disabled = true`。
    ///
    /// # 为什么这条是承重的（改成禁用会造成真实损失）
    ///
    /// `AccountThrottled` 的语义是「**探不了**」（403 账户级临时风控挡在 region 授权校验之前，
    /// 拿不到任何 region 信息），与 `NoUsableRegion`（探过了、确定不行）是两种不同结论。
    ///
    /// 一旦禁用：`ids_needing_region_probe` 过滤 `!e.disabled` ⇒ **连重启时的存量回填都不再
    /// 重探**它，风控过去了也永远不会自愈 ⇒ 临时态被固化成需人工的永久态。
    /// 而不禁用的最坏态只是退回「探测接入前的基线」（`api_region=None` → 回退 `config.region`），
    /// 且若真打错区会走 `report_failure` → `TooManyFailures` ——
    /// **那个原因在 `is_self_healable_reason` 白名单里**，是可自愈的。
    /// 即不禁用的最坏态**严格优于**禁用。
    ///
    /// 严重度：这类 403 占近 2h 流量 22.3%（CLAUDE.md），是常态不是罕见；而
    /// `MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE = 6` 存在的唯一理由就是
    /// 「见过一次 403 不足以判死」—— 探测路径若用一次 403 就判死，等于绕过那道阈值。
    ///
    /// 用源码守卫而非行为测试：`add_credential` 会打真实上游（`get_usage_limits_for`），
    /// 本仓铁律禁止测试依赖网络。
    #[test]
    fn account_throttled_must_not_disable_credential() {
        let src = include_str!("service.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];

        // needle 运行时拼接，避免 include_str! 把本测试自己的字面量算进匹配
        // （本文件已有两处守卫因此踩过坑，见它们的注释）。
        let throttled = format!("region_probe_throttled = {}", "matches!(");
        let ti = prod
            .find(&throttled)
            .expect("AccountThrottled 必须被单独识别，不能与 region_probe_failed 混为一谈");

        // 🔴 核心断言：禁用那句必须**只**在 region_probe_failed 的 if 里，
        // 且必须出现在 throttled 判定**之前** —— 若有人把 throttled 并进那个 matches!，
        // 禁用就会连带作用到它身上。
        let disable = format!("new_cred.disabled = {}", "true;");
        let di = prod.find(&disable).expect("禁用语句不该被改名");
        assert!(
            di < ti,
            "禁用必须发生在 AccountThrottled 判定之前（即只属于 region_probe_failed 那条）——\
             若顺序反了或两者被并进同一个 matches!，被风控的号会被永久禁用且不再重探"
        );

        // AccountThrottled 不得出现在决定禁用的那个 matches! 里。
        let failed_marker = format!("region_probe_failed = {}", "matches!(");
        let fi = prod
            .find(&failed_marker)
            .expect("region_probe_failed 不该被改名");
        let failed_block = &prod[fi..di];
        assert!(
            !failed_block.contains("AccountThrottled"),
            "AccountThrottled 绝不能进 region_probe_failed 的 matches! —— \
             那等于让「探不了」和「确定不行」同样被禁用（见本测试文档的损失论证）"
        );

        // 跳过订阅等级探测的那道门必须**同时**覆盖两者：被风控的号打 management.* 查订阅
        // 同样 403，白付一次上游往返，而上号是用户交互路径。
        let skip_gate = format!("if region_probe_failed || {}", "region_probe_throttled");
        assert!(
            prod.contains(skip_gate.as_str()),
            "跳过订阅等级探测的门必须同时覆盖 AccountThrottled（否则白付一次必然 403 的往返）"
        );
    }

    /// 🔴 region 探测的结果必须**回写进 `new_cred`**，且必须在分身循环**之前**。
    ///
    /// # 实测事故（2026-08-05 02:42）
    ///
    /// 父号 #525 被探测写上 `eu-central-1`（95% 成功），而同批 4 个分身 #526–529
    /// 全部 `api_region=None` ⇒ 回退 `config.region=us-east-1` ⇒ `ksk_` 按区授权
    /// ⇒ 恒 403 `bearer token invalid` ⇒ **24 秒内三次失败全部被禁用、0% 成功**。
    ///
    /// 根因：`for seq in 2..=copies` 里 `new_cred.clone()` 克隆的是**探测前**的
    /// 局部副本。探测只写了 entry，没写这个局部变量。
    ///
    /// ⚠️ 这个缺陷是**接入探测才引入的**：探测之前父子都没 region、一起废（症状一致）；
    /// 接入之后变成「父好子坏」，更容易被误判成「这个 key 不支持分身」。
    ///
    /// 位置断言是承重的：回写若放在分身循环**之后**，等于没回写。
    #[test]
    fn probed_region_must_be_written_back_before_clone_loop() {
        let src = include_str!("service.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        // needle 运行时拼接，避免 include_str! 把本测试自己的字面量算进匹配。
        let writeback = format!("new_cred.api_region = {}", "Some(probed)");
        let wi = prod.find(&writeback).expect(
            "探测结果必须回写进 new_cred，否则分身克隆的是探测前的副本 ⇒ \
             父号有 region、分身没有 ⇒ ksk_ 打错区恒 403 ⇒ 分身 0% 成功",
        );
        // ⚠️ 必须匹配**代码**而非注释：本文件里有两处注释在散文里提到这个循环
        // （`:1294` 与 `:1413`），裸用 "for seq in 2..=copies" 会先命中注释、
        // 让位置比较反向 → 守卫静默失效（我第一版就是这样，回退验证时才发现）。
        // 故带上循环体的左花括号。
        let loop_marker = format!("for seq in 2..=copies {}", "{");
        let li = prod.find(&loop_marker).expect("分身循环不该被改名");
        assert!(wi < li, "回写必须在分身循环之前（放之后等于没回写）");
        // 且必须在探测调用之后 —— 放之前读到的还是 None。
        let probe = format!("probe_and_persist_api_region{}", "(credential_id)");
        let pi = prod.find(&probe).expect("探测调用不该被改名");
        assert!(pi < wi, "回写必须在探测调用之后，否则读到的仍是探测前的值");
    }

    /// ⚠️ 本条此前**缺 `#[test]`、从未运行过** —— 属性被上一条测试的文档块吃掉了
    /// （2026-08-06 全仓扫出 2 处同型，另一处在 `provider.rs`）。补属性时它一次通过，
    /// 说明它守的东西一直是对的，只是守卫本身没生效。
    #[test]
    fn multi_open_must_inherit_api_region_from_parent() {
        let src = include_str!("service.rs");
        // needle 运行时拼接，避免字面量把自己也算进匹配（同 provider.rs 那个守卫的教训）。
        let needle = format!("{}{}", "api_region: ", "inherit(req.api_region");
        assert!(
            src.contains(needle.as_str()),
            "多开必须继承父号的 api_region：否则分身打到错误的 region host，\
             ksk_ token 按 region 授权 → 上游 403 bearer token invalid → 分身 0% 成功"
        );
        // 同族的另外两个 region 字段一并锁住（三者共同决定路由与认证 region）。
        for f in ["region", "auth_region"] {
            let n = format!("{}: {}", f, "inherit(req.");
            assert!(
                src.contains(n.as_str()),
                "多开也应继承 {f}（与 api_region 同族，共同决定路由/认证 region）"
            );
        }
    }

    /// ⭐ 源码级守卫：`copies` **显式给值时第 1 份也必须绕过去重**。
    ///
    /// 单测覆盖不到 `add_credential`（它会调 `get_usage_limits_for`，那是真实上游网络往返，
    /// 本仓铁律禁止测试依赖网络）。故用源码断言。
    ///
    /// 回归的是一个**实测走不通的场景**：号池里已有 #419/#420，想给它们各加 4 个分身
    /// （不同 machineId + 不同代理出口 IP）。若第 1 份走去重，它撞
    /// `凭据已存在（kiroApiKey 重复）` → 整个请求失败 → 一个分身也建不出来。
    ///
    /// 判据是**归一后份数 > 1**（`is_multi_open`），不是「字段是否出现」——
    /// 见 `copies_equal_one_must_not_bypass_dedup_or_create_a_group`。
    /// 但真多开时第 1 份仍必须绕，这条锁的就是这半边。
    #[test]
    fn explicit_copies_must_bypass_dedup_for_first_copy_too() {
        let src = include_str!("service.rs");
        // needle 运行时拼接：写成完整字面量时它会出现在 include_str! 读到的本测试自身里。
        let judgement = format!("{}{}", "let allow_dup = ", "is_multi_open;");
        let block = src
            .split(judgement.as_str())
            .nth(1)
            .expect("allow_dup 的判据必须是 is_multi_open（归一后份数 > 1）");
        let block = block
            .split("map_err(|e| self.classify_add_error(e))")
            .next()
            .expect("第 1 份的错误处理不应被改动");
        assert!(
            block.contains("add_credential_allowing_duplicate"),
            "真多开（份数 >1）时第 1 份必须走 add_credential_allowing_duplicate，\
             否则给已存在的号加分身会在第 1 份就 bail"
        );
    }

    /// ⭐ 源码级守卫：去重绕过与分身组都必须挂在 `is_multi_open` 上，
    /// 而**不是** `req.copies.is_some()`。
    ///
    /// 回退即 FAIL：把任一处判据改回 `req.copies.is_some()`，下面的否定断言失败。
    ///
    /// 修的是一条静默且不可逆的缺陷：一个总是下发 `"copies": 1` 的 API 客户端
    /// （文档说该字段被 clamp 到 [1,16]，"1 = 普通上号"，所以总是下发 1 是最自然的读法）
    /// 会**永久失去去重保护** —— 重复上号不再报 `凭据已存在`，同一个号在池里越积越多，
    /// 而它们共用一份上游配额；同时每次还造出一个只有 1 个成员的分身组，
    /// 分身管理页上凭空多出一堆「独苗组」。
    ///
    /// clone_group 那半边只能用源码守卫：走行为测试要让 `add_credential` **成功**，
    /// 而它内部会调 `get_usage_limits_for`（真实上游往返），本仓铁律禁止测试依赖网络。
    /// 去重那半边有对应的行为测试（见下一条，它在 bail 处就返回，不碰网络）。
    #[test]
    fn dedup_bypass_and_clone_group_must_hinge_on_effective_copies() {
        let src = include_str!("service.rs");
        // needle 全部运行时拼接：字面量会被 include_str! 读到自己，让断言失真。
        // 且只看**代码行**：注释里必须能写出这个错误判据（本条与上方那段长注释都要提它），
        // 否则这条否定断言会被自己的文档打成恒失败。
        let bug = format!("{}{}", "req.copies.", "is_some()");
        let offending: Vec<&str> = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| l.contains(bug.as_str()))
            .collect();
        assert!(
            offending.is_empty(),
            "判据不得是「字段是否出现」：copies=1 会因此绕过去重并造出 1 人分身组。\
             应改用归一后的份数（effective_copies → is_multi_open）。命中行: {offending:?}"
        );
        let group_judgement = format!("{}{}", "let clone_group = if ", "is_multi_open");
        assert!(
            src.contains(group_judgement.as_str()),
            "clone_group 必须只在归一后份数 >1 时赋值"
        );
        let inherit_judgement = format!("{}{}", "let inherited = if ", "is_multi_open");
        assert!(
            src.contains(inherit_judgement.as_str()),
            "字段继承也只在真多开时进行（与 clone_group 同一判据，避免两处再次分叉）"
        );
    }

    /// ⭐ 承重（行为测试）：`copies: 1` **不得绕过去重**。
    ///
    /// 池里已有 `ksk_test`，再用同一个 key + `copies: Some(1)` 上号必须撞
    /// `凭据已存在（kiroApiKey 重复）`。
    ///
    /// 回退即 FAIL：把 `allow_dup` 判据改回 `req.copies.is_some()` —— 去重被绕过，
    /// 这里变成"添加成功"，`expect_err` 失败。
    ///
    /// 不碰网络：去重在 `add_credential_inner` 的第 2 步就 bail，
    /// 早于第 3 步的刷新与之后的 `get_usage_limits_for`。
    #[tokio::test]
    async fn copies_equal_one_must_not_bypass_dedup() {
        let svc = mk_service_with_one_credential();
        let err = svc
            .add_credential(AddCredentialRequest {
                auth_method: "api_key".into(),
                kiro_api_key: Some("ksk_test".into()),
                copies: Some(1),
                ..Default::default()
            })
            .await
            .expect_err("copies=1 是普通上号，重复的 kiroApiKey 必须被去重拦住");
        let msg = err.to_string();
        assert!(msg.contains("已存在"), "应是去重报错，实际 {msg}");
        assert_eq!(
            svc.token_manager.total_count(),
            1,
            "池里不得多出一条同 key 的凭据"
        );
    }

    /// ⭐ 承重：OAuth 号（social/idc/external_idp）多开必须被拒。
    ///
    /// 回退即 FAIL：删掉 `add_credential` 里那段 `multi_open_rejection_reason` 判断 ——
    /// 本测试的 `expect_err` 失败（请求会继续走到入池与真实上游往返）。
    ///
    /// 为什么必须拒：refreshToken 每次刷新都被上游轮换，N 份带同一个 token →
    /// 先刷新的那份把它作废 → 其余份 invalid_grant 被禁用。用户看到的是
    /// 「分身建好了然后一个个变灰」，且原因写着 refresh_token_invalid，
    /// 极易误判成号被封。
    #[tokio::test]
    async fn multi_open_on_oauth_credential_is_rejected() {
        let svc = mk_service_with_one_credential();
        let err = svc
            .add_credential(AddCredentialRequest {
                auth_method: "social".into(),
                refresh_token: Some("rt_social_xyz".into()),
                copies: Some(3),
                ..Default::default()
            })
            .await
            .expect_err("OAuth 号多开必须被拒");
        assert!(
            matches!(err, AdminServiceError::InvalidCredential(_)),
            "应是 InvalidCredential，实际 {err:?}"
        );
        // ⭐ 承重断言是**报错内容**而不是错误种类：删掉这道门后请求会往下走到
        // `validate_refresh_token`，那里同样返回 InvalidCredential（「refreshToken 已被截断」），
        // 只看种类的话缺陷重现了测试照样过。必须断言这条错误确实是"多开不适用"那一条。
        let msg = err.to_string();
        assert!(
            msg.contains("refreshToken 每次刷新都会被上游轮换") && msg.contains("ksk_"),
            "错误必须说清原因（refreshToken 轮换）与适用范围（ksk_），实际: {msg}"
        );
        assert_eq!(
            svc.token_manager.total_count(),
            1,
            "被拒的请求不得留下任何新凭据"
        );
    }

    /// 拒绝判据本身的正反两面（纯函数，不碰网络）。
    #[test]
    fn multi_open_rejection_applies_only_to_non_api_key_credentials() {
        let mut api_key = KiroCredentials::default();
        api_key.auth_method = Some("api_key".into());
        api_key.kiro_api_key = Some("ksk_abc".into());
        assert!(
            multi_open_rejection_reason(&api_key).is_none(),
            "api_key 号没有 refreshToken，多开是安全的，不得被这道检查拦住"
        );

        for method in ["social", "idc", "external_idp"] {
            let mut oauth = KiroCredentials::default();
            oauth.auth_method = Some(method.into());
            oauth.refresh_token = Some("rt".into());
            let reason = multi_open_rejection_reason(&oauth)
                .unwrap_or_else(|| panic!("{method} 号多开必须被拒"));
            assert!(
                reason.contains(method),
                "拒绝理由应点明 authMethod，实际: {reason}"
            );
        }
    }

    // ---------------- M9：region 探测窗口保护 ----------------

    /// 探测窗口判据矩阵（纯函数，不碰网络）：只有「真的会被探测」的号才需要
    /// 临时禁用 —— api_key 号 + region 三字段全空 + 非 custom_api。
    ///
    /// 镜像 `token_manager::needs_api_region_probe` 的逐字判据；行为测试跑不到
    /// 真实探测（上游往返，本仓铁律），故矩阵锁住「哪些号进窗口保护」。
    #[test]
    fn probe_window_guard_judgement_matrix() {
        fn cred(region: Option<&str>, api_region: Option<&str>, auth_region: Option<&str>) -> KiroCredentials {
            KiroCredentials {
                auth_method: Some("api_key".into()),
                kiro_api_key: Some("ksk_m9".into()),
                region: region.map(String::from),
                auth_region: auth_region.map(String::from),
                api_region: api_region.map(String::from),
                ..Default::default()
            }
        }

        // 无任何 region 字段的 api_key 号 → 会探测 → 必须进窗口保护。
        assert!(needs_probe_window_guard(&cred(None, None, None)));
        // 任一 region 字段有值 → probe 直接 Skipped → 不进保护（行为零变化）。
        assert!(!needs_probe_window_guard(&cred(Some("eu-central-1"), None, None)));
        assert!(!needs_probe_window_guard(&cred(None, Some("us-east-1"), None)));
        assert!(!needs_probe_window_guard(&cred(None, None, Some("eu-central-1"))));
        // OAuth 号（无 kiro_api_key）→ probe Skipped → 不进保护。
        let mut oauth = cred(None, None, None);
        oauth.kiro_api_key = None;
        oauth.auth_method = Some("social".into());
        oauth.refresh_token = Some("rt".into());
        assert!(!needs_probe_window_guard(&oauth));
        // custom_api 号（即使旧数据带了 kiro_api_key）→ 不属于 Kiro region 体系 → 不进保护。
        let mut custom = cred(None, None, None);
        custom.auth_method = Some("custom_api".into());
        assert!(!needs_probe_window_guard(&custom));
        // 旧数据兜底：base_url 有值也算 custom_api（is_custom_api_credential 判据）。
        let mut legacy_custom = cred(None, None, None);
        legacy_custom.base_url = Some("https://relay.example.com".into());
        assert!(!needs_probe_window_guard(&legacy_custom));
    }

    /// ⭐ 源码级守卫（M9 承重）：探测窗口内凭据**不可被调度**。
    ///
    /// 线上事故（2026-08-05 05:41）：#536–550 以启用态入池，探测 1-2s 的窗口里
    /// 真实流量打到错区恒 403，3 次即自动禁用 —— 号在自己 region 被探出来之前就死了。
    /// 修复 = 探测前置临时禁用 + 探测后按结论恢复；守卫锁住这个结构：
    ///   1. `probe_and_persist_api_region(credential_id)` 调用**之前**必须存在
    ///      临时禁用赋值（`new_cred.disabled = orig_disabled || will_probe`）。
    ///   2. 探测调用**之后**必须存在恢复调用（`set_disabled(credential_id, false)`）。
    ///
    /// 回退即 FAIL：把临时禁用行删掉 / 把恢复调用删掉 / 把临时禁用挪到探测调用之后
    /// （那等于没保护）。行为测试测不到（真实探测是上游往返），故锁源码。
    #[test]
    fn probe_window_keeps_credential_unselectable() {
        let src = include_str!("service.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let fname = format!("async fn add_credential_with_intent{}", "(");
        let start = prod.find(&fname).expect("add_credential_with_intent 不应被改名");
        let body_end = prod[start..]
            .find("\n    pub async fn ")
            .map(|i| i + start)
            .unwrap_or(prod.len());
        let body = &prod[start..body_end];

        // 1) 探测调用位置
        let probe = format!("probe_and_persist_api_region{}", "(credential_id)");
        let pi = body
            .find(&probe)
            .expect("region 探测调用不应被删除或改名");

        // 2) 临时禁用必须出现在探测**之前**（needle 拼接防自匹配）。
        let guard_assign = format!(
            "new_cred.disabled = {} || will_probe;",
            "orig_disabled"
        );
        let gi = body.find(&guard_assign).unwrap_or_else(|| {
            panic!(
                "入池前必须存在临时禁用赋值（{guard_assign}）——否则探测窗口内\
                 号以启用态在池中，真实流量打错区 3 次即被自动禁用（事故 #536-550）"
            )
        });
        assert!(gi < pi, "临时禁用必须在探测调用之前（放之后等于没保护）");

        // 3) 恢复调用必须出现在探测**之后**。
        let restore = format!("set_disabled(credential_id, {})", "false");
        let ri = body.find(&restore).unwrap_or_else(|| {
            panic!(
                "探测后必须存在恢复启用调用（set_disabled(credential_id, false)）——\
                 否则临时禁用的号永远留在禁用态"
            )
        });
        assert!(pi < ri, "恢复必须在探测完成之后");
    }
}
    /// 🔴 档位切换必须真的落到消费侧（2026-08-11 新增）。
    ///
    /// 完整链路：面板切档 → `throttle_profile` 分支设 `hot_changed=true`
    /// → `hot_or_display_changed` → `reload_config()` → `token_manager` 把
    /// `inbound_queue_timeout_passthrough` 等值 `store` 进 `GlobalThrottle`。
    ///
    /// 断掉任一环的表现都是「面板显示已切档、config.json 里也写对了，但行为没变」——
    /// 而档位管的恰好是「整形层超时放行还是返 429」这种**只在真实压力下才看得出**的开关，
    /// 排障时极难定位。所以这里钉住两点：切档分支存在，且它设了 `hot_changed`。
    #[test]
    fn throttle_profile_switch_is_wired_to_hot_reload() {
        let src = include_str!("service.rs");
        // 显式截断测试段：否则本测试自身的字面量会让 split 命中测试代码
        // （本文件已因这类原因出过一次「守卫静默变绿」）。
        let needle_fn = format!("pub fn update{}", "_config");
        let update_fn = src
            .split(needle_fn.as_str())
            .nth(1)
            .and_then(|s| s.split_once("\n#[cfg(test)]").map(|(head, _)| head))
            .expect("找不到 update_config 的生产代码段");

        // ① 切档分支必须在 update_config 里
        let field = format!("req.throttle{}", "_profile");
        assert!(
            update_fn.contains(field.as_str()),
            "update_config 里找不到 {field} 的处理分支 —— 面板切档不会有任何效果"
        );

        // ② 该分支必须设 hot_changed（否则当次不触发 reload_config，改动要等重启才生效）
        let seg = update_fn
            .split(field.as_str())
            .nth(1)
            .expect("上面已断言存在");
        // ⚠️ 窗口必须截到**本分支结束**（下一个 `if let Some` 处），不能用固定字符数。
        // 初版取 600 字符 ⇒ 越过本分支、命中了下一个字段的 `hot_changed = true`
        // ⇒ 删掉切档分支自己那一行，守卫**仍然绿**（实测确认过）。
        // 这正是本守卫要防的失败模式，却先发生在守卫自己身上。
        let next_branch = format!("if let Some{}", "(");
        let window = seg
            .find(next_branch.as_str())
            .map(|end| &seg[..end])
            .unwrap_or(seg);
        let hot = format!("hot{}", "_changed = true");
        assert!(
            window.contains(hot.as_str()),
            "切档分支没有设 {hot} —— 后果：config.json 写对了、面板显示成功，\
             但当次进程内的整形层/冷却开关**不会更新**，要重启才生效。\
             这是本文件历史上出现过的同款隐蔽故障。"
        );
    }


#[cfg(test)]
mod absorb_hot_reload_tests {
    // ⚠️ 2026-08-15：error_messages 校验矩阵测试也挂在本模块（A1 实现）；
    // 子模块不自动继承父级项，必须显式引入（validate_error_messages /
    // ERROR_TABLE_MAX_ENTRIES / HashMap）。
    use super::*;
    use std::collections::HashMap;

    /// ⭐ 源码守卫：`absorb_changed` 必须出现在 `hot_or_display_changed` 的 OR 链里。
    ///
    /// 回退即 FAIL：删掉 `update_config` 里那行 `|| absorb_changed`，本测试失败。
    ///
    /// 为什么这条是本方案唯一新增的风险点：吸收层**没有** TIER3 setter（它在 provider 内
    /// 直接读 token_manager 的 config ArcSwap），所以「面板改动生效」这件事完全依赖
    /// `hot_or_display_changed` 触发 `reload_config` 把新配置从盘重读并原子换入 ArcSwap。
    /// 漏掉这一行的表现极其隐蔽：面板显示保存成功、config.json 里确实写进去了、
    /// 重启后也确实生效 —— 唯独**当次不生效**，排障时几乎不可能想到是这里。
    ///
    /// 单测无法真跑 `update_config`（需要真实 TokenManager + 磁盘 config），故用源码断言。
    #[test]
    fn absorb_changed_is_in_hot_reload_or_chain() {
        let src = include_str!("service.rs");
        // ⚠️ 显式截断测试段（2026-08-11 审计修复）：`split(...).nth(1)` 只取第二个片段，
        // 此前 update_fn 恰好在本测试自身的 `.split("pub fn update_config")` 字面量处
        // 截断 —— 绿是**巧合**（依赖测试段里存在该字面量），删掉那个字面量 update_fn
        // 会延伸到文件末尾、把本测试断言行的 `absorb_changed = true` 字面量数进去 →
        // 计数 11 ≠ 10 误红。显式截断 + needle 运行时拼接后语义与位置无关。
        let needle_fn = format!("pub fn update{}", "_config");
        let update_fn = src
            .split(needle_fn.as_str())
            .nth(1)
            .and_then(|s| s.split_once("\n#[cfg(test)]").map(|(head, _)| head))
            .expect("update_config 不应被改名");
        // 截到 reload_config 调用处为止，只看它之前的那条 OR 链。
        let or_chain = update_fn
            .split("self.token_manager.reload_config()")
            .next()
            .expect("reload_config 调用点不应被改名");
        let needle = format!("{}{}", "|| absorb_", "changed");
        assert!(
            or_chain.contains(needle.as_str()),
            "hot_or_display_changed 的 OR 链必须包含 absorb_changed，否则面板改了吸收层配置\
             会存盘但不触发 reload_config → ArcSwap 仍是旧值 → 开关当次静默无效"
        );
        // 七个字段都必须真的会把 absorb_changed 置位（防加了字段忘了置位）。
        // 2026-08-10 从六项扩到七项：补入 `upstream_retry_absorb_server_error`
        // —— 它在 `model/config.rs` 早已存在，但此前**没暴露到 Admin API**，
        // 只能改 config.json + 重启。线上代挂上游主要故障形态是 502，
        // 不吸收 5xx 等于把最典型的瞬态故障直接甩给客户端断会话。
        // 2026-08-11 扩到十项：capacity_400 / swap_budget_secs / exhausted_status
        // （同类问题：只存在于 config.json，面板与 API 都改不了）。
        let absorb_fields = [
            "upstream_retry_absorb_enabled",
            "upstream_retry_absorb_budget_secs",
            "upstream_retry_absorb_max_rounds",
            "upstream_retry_absorb_min_delay_ms",
            "upstream_retry_absorb_max_delay_secs",
            "upstream_retry_absorb_suspended",
            "upstream_retry_absorb_server_error",
            "upstream_retry_absorb_capacity_400",
            "upstream_retry_absorb_swap_budget_secs",
            "upstream_retry_absorb_exhausted_status",
        ];
        for field in absorb_fields {
            assert!(
                update_fn.contains(&format!("req.{field}")),
                "update_config 必须读取 req.{field}，否则该字段面板改不了"
            );
        }
        assert_eq!(
            update_fn.matches("absorb_changed = true").count(),
            absorb_fields.len(),
            "每个吸收层字段各自都必须置位 absorb_changed（漏一个 → 只改那个字段时不热更）。\
             新增字段时这里的计数会自动跟着 absorb_fields 走，不用再手改数字"
        );
    }

    /// ⭐ 源码守卫：配置快照的吸收层十项必须**逐字段从 config 读**，不得写死。
    ///
    /// 回退即 FAIL：把任一项改成字面量（如 `upstream_retry_absorb_enabled: false,`），断言失败。
    ///
    /// 为什么这条替代了规格里那条「第三处默认值镜像」的守卫：`ConfigSnapshotResponse`
    /// 其实**没有** `Default` impl（规格与我的设计文档都记错了，把 types.rs 里一个**测试夹具**
    /// 的结构体字面量当成了 Default）。真实的漂移面不是"默认值三处不一致"，而是
    /// "快照有没有真的把 config 的值读出来" —— 写死的话面板永远显示默认值、
    /// 用户改了也看不到变化，而任何只比对默认值的测试都发现不了（默认态下两者恰好相等）。
    #[test]
    fn absorb_snapshot_maps_every_field_from_config() {
        let src = include_str!("service.rs");
        for field in [
            "upstream_retry_absorb_enabled",
            "upstream_retry_absorb_budget_secs",
            "upstream_retry_absorb_max_rounds",
            "upstream_retry_absorb_min_delay_ms",
            "upstream_retry_absorb_max_delay_secs",
            "upstream_retry_absorb_suspended",
            // 2026-08-10 补：该字段此前完全没进 Admin API（面板看不到也改不了）
            "upstream_retry_absorb_server_error",
            // 2026-08-11 补：同类问题三个字段（只存在于 config.json）
            "upstream_retry_absorb_capacity_400",
            "upstream_retry_absorb_swap_budget_secs",
            "upstream_retry_absorb_exhausted_status",
        ] {
            let mapping = format!("{field}: config.{field},");
            assert!(
                src.contains(mapping.as_str()),
                "配置快照必须写 `{mapping}`（逐字段从 config 读）；\
                 写死字面量会让面板永远显示默认值、用户改了也看不到"
            );
        }
    }

    /// 🔴 回归：`auto_disable_suspicious` 必须**三处都接线**（快照 / 更新分支 / 不进重启集）。
    ///
    /// 这个字段此前只存在于 `Config` 与 `TokenManager`：`reload_config` 确实在读它，
    /// 但 `admin/types.rs` 既没有响应字段也没有请求字段，`service.rs` 也没有更新分支
    /// ⇒ **面板既看不到也改不了它**，只能手改 config.json + 重启。
    ///
    /// 实际造成的排查错误：线上有人「把三个自动禁用开关关掉」，而这一项其实改不到，
    /// 于是配置 API 读回 `None`，看起来像"没有这个开关"。
    ///
    /// 回退即 FAIL：删掉任一处接线 → 对应断言失败。
    #[test]
    fn auto_disable_suspicious_is_fully_wired() {
        let src = include_str!("service.rs");
        let types = include_str!("types.rs");
        // needle 运行时拼接：include_str! 会把本测试自己的字面量也读进来。
        let field = format!("auto_disable{}", "_suspicious");

        let snapshot = format!("{field}: config.{field},");
        assert!(
            src.contains(&snapshot),
            "配置快照必须逐字段从 config 读 `{snapshot}`，否则面板读不到真实值"
        );
        let update = format!("req.{field}");
        assert!(
            src.contains(&update),
            "必须有 `if let Some(v) = {update}` 的 TIER1 更新分支，否则面板改不动它"
        );
        // 响应结构与请求结构各一处。
        assert!(
            types.matches(&field).count() >= 2,
            "types.rs 里响应结构与请求结构都必须有该字段（当前 {} 处）",
            types.matches(&field).count()
        );
        // TIER1 语义守卫：它是热更字段，绝不能进 restart_fields。
        let restart = format!("restart_fields.push(\"{}\"", "autoDisableSuspicious");
        assert!(
            !src.contains(&restart),
            "该字段是 TIER1 热更（reload_config 已读它），不得要求重启"
        );
    }

    /// 🔴 回归（2026-08-15 补接线）：`ota_auto_check` 必须**全套接线**。
    ///
    /// 此前该字段只存在于 `Config` 与 main.rs 启动门控：前端 settings-page.tsx 提交
    /// `otaAutoCheck`，但 ConfigSnapshotResponse / UpdateConfigRequest 都没有它 →
    /// serde 静默丢弃 → 用户开了「自动检查」保存成功却不生效，且快照不下发 →
    /// 刷新后开关恒回弹为关。与已修的 prompt_cache_enabled 事故完全同型。
    ///
    /// 语义是 **restart-only**：main.rs 启动期按 config 门控 spawn 后台检查任务
    /// （无 TIER2 respawn 机制），改后必须重启进程才生效 → 必须进 restart_fields
    /// （前端据此 toast「需重启」），且绝不能进 hot_or_display_changed（restart-only
    /// 纪律，见 build_config_snapshot 的 proxy split-brain 注释）。
    ///
    /// 回退即 FAIL：删掉任一处接线 → 对应断言失败。
    #[test]
    fn ota_auto_check_is_fully_wired() {
        let src = include_str!("service.rs");
        let types = include_str!("types.rs");
        let field = format!("ota_auto{}", "_check");

        let snapshot = format!("{field}: config.{field},");
        assert!(
            src.contains(&snapshot),
            "配置快照必须逐字段从 config 读 `{snapshot}`，否则面板读不到真实值"
        );
        let update = format!("req.{field}");
        assert!(
            src.contains(&update),
            "必须有 `if let Some(v) = {update}` 的更新分支，否则面板改不动它"
        );
        assert!(
            types.matches(&field).count() >= 2,
            "types.rs 里响应结构与请求结构都必须有该字段（当前 {} 处）",
            types.matches(&field).count()
        );
        // restart-only 语义守卫：必须进 restart_fields（前端提示重启），
        // 且不得进 hot_or_display_changed 的 reload 触发链。
        let restart = format!("restart_fields.push(\"{}\"", "otaAutoCheck");
        assert!(
            src.contains(&restart),
            "OTA 自动检查是启动期 spawn 的后台任务，必须进 restart_fields 提示重启"
        );
        let hot_chain = format!("{field}_changed");
        assert!(
            !src.contains(&hot_chain),
            "restart-only 字段不得进 hot_or_display_changed 的 reload 触发链（proxy split-brain 纪律）"
        );
    }

    /// 🔴 回归（2026-08-16 新增）：`scheduling_mode` 必须**全套接线**。
    ///
    /// 三按钮方案（docs/scheduling-config-simplify.md §3.2）的前端入口。该字段此前
    /// 不存在，若只加 `Config` 字段而漏掉任一处接线，面板要么读不到（快照缺失）、
    /// 要么改不动（请求结构缺失/无更新分支）—— 与 `ota_auto_check` 事故同型。
    ///
    /// 语义是 TIER1 热更：切换调度模式即写矩阵 + 落盘（save），无需重启。
    ///
    /// 回退即 FAIL：删掉任一处接线 → 对应断言失败。
    #[test]
    fn scheduling_mode_is_fully_wired() {
        let src = include_str!("service.rs");
        let types = include_str!("types.rs");
        let field = format!("scheduling{}", "_mode");

        let snapshot = format!("{field}: config.{field},");
        assert!(
            src.contains(&snapshot),
            "配置快照必须逐字段从 config 读 `{snapshot}`，否则面板读不到真实值"
        );
        let update = format!("req.{field}");
        assert!(
            src.contains(&update),
            "必须有 `if let Some(m) = {update}` 的更新分支，否则面板改不动它"
        );
        assert!(
            types.matches(&field).count() >= 2,
            "types.rs 里响应结构与请求结构都必须有该字段（当前 {} 处）",
            types.matches(&field).count()
        );
        // TIER1 语义守卫：它是热更字段，绝不能进 restart_fields。
        let restart = format!("restart_fields.push(\"{}\"", "schedulingMode");
        assert!(
            !src.contains(&restart),
            "该字段是 TIER1 热更（切换即写矩阵 + save 落盘），不得要求重启"
        );
    }

    /// 🔴 回归（2026-08-14 新增）：`auto_disable_quota_exceeded` 必须**全套接线**。
    ///
    /// 该开关是 AdminService **内存态**（不进 config.json），漂移面有三处：
    /// ① 快照（面板读得到当前值）；② `req.{field}` 更新分支（面板改得动）；
    /// ③ 余额刷新循环的读取点（不接线 = 开关形同虚设）。types.rs 响应/请求结构各一处。
    ///
    /// 回退即 FAIL：删掉任一处接线 → 对应断言失败。
    #[test]
    fn auto_disable_quota_exceeded_is_fully_wired() {
        let src = include_str!("service.rs");
        let types = include_str!("types.rs");
        // 折叠空白再比：长链调用会被 rustfmt 拆成多行（同 router 守卫写法）。
        let compact: String = src.chars().filter(|c| !c.is_whitespace()).collect();
        let field = format!("auto_disable{}", "_quota_exceeded");

        let snapshot = format!("{field}:self.{field}");
        assert!(
            compact.contains(&snapshot),
            "配置快照必须输出 `{snapshot}`，否则面板读不到该开关当前值"
        );
        let update = format!("req.{field}");
        assert!(
            compact.contains(&update),
            "必须有 `if let Some(v) = {update}` 的更新分支，否则面板改不动它"
        );
        assert!(
            compact.contains(&format!("{field}.load")),
            "余额刷新循环必须有 `{field}.load(..)` 读取点，否则开关改了也不生效"
        );
        assert!(
            types.matches(&field).count() >= 2,
            "types.rs 里响应结构与请求结构都必须有该字段（当前 {} 处）",
            types.matches(&field).count()
        );
    }

    /// 🔴 回归：`native_thinking_effort_enabled` 必须**全套接线**（快照 / 更新分支 /
    /// TIER3 setter 应用 / 两条 OR 链），否则面板改了不生效且回「无改动」。
    ///
    /// 参考仓移植的新开关，必须一次性接通才会被面板看到、改到、热更到：
    /// - 快照：`build_config_snapshot` 逐字段从 config 读（否则面板永远显示默认值）；
    /// - 更新分支：`req.{field}` 置位（否则面板改不动）；
    /// - TIER3：改后调 `set_native_thinking_effort_enabled` 写 converter 进程镜像
    ///   （否则存了盘但热路径仍读旧值，开关静默无效）；
    /// - 两条 OR 链各一处（hot_or_display_changed 与 immediate_changed，漏一条 →
    ///   只改本项时面板回「无改动」）。
    ///
    /// 回退即 FAIL：删掉任一处接线 → 对应断言失败。
    #[test]
    fn native_thinking_effort_enabled_is_fully_wired() {
        let src = include_str!("service.rs");
        let types = include_str!("types.rs");
        // needle 运行时拼接：include_str! 会把本测试自己的字面量也读进来。
        let field = format!("native_thinking{}", "_effort_enabled");

        let snapshot = format!("{field}: config.{field},");
        assert!(
            src.contains(&snapshot),
            "配置快照必须逐字段从 config 读 `{snapshot}`，否则面板读不到真实值"
        );
        let update = format!("req.{field}");
        assert!(
            src.contains(&update),
            "必须有 `if let Some(v) = {update}` 的 TIER3 更新分支，否则面板改不动它"
        );
        let setter = format!("set_native{}", "_thinking_effort_enabled(v)");
        assert!(
            src.contains(&setter),
            "改后必须调 converter 的 `{setter}` 写进程镜像，否则热路径读旧值"
        );
        // 响应结构与请求结构各一处（快照 + 请求）。
        assert!(
            types.matches(&field).count() >= 2,
            "types.rs 里响应结构与请求结构都必须有该字段（当前 {} 处）",
            types.matches(&field).count()
        );
        // 两条 OR 链（hot_or_display_changed 与 immediate_changed）各必须含本 flag。
        assert!(
            src.matches(&format!("|| {field}_changed.is_some()")).count() >= 2,
            "本 flag 必须同时进 hot_or_display_changed 与 immediate_changed 两条 OR 链"
        );
    }

    /// 🔴 回归：`tool_compat_mapping` 必须**全套接线**（快照 / 更新分支 / TIER3 setter
    /// 应用 / 两条 OR 链），否则面板改了不生效且回「无改动」。
    ///
    /// CC↔Kiro 工具名/参数映射开关，此前只有 converter 原子默认 true 无配置入口，
    /// 必须一次性接通才会被面板看到、改到、热更到：
    /// - 快照：`build_config_snapshot` 逐字段从 config 读（否则面板永远显示默认值）；
    /// - 更新分支：`req.{field}` 置位（否则面板改不动）；
    /// - TIER3：改后调 `set_tool_compat_mapping` 写 converter 进程镜像
    ///   （否则存了盘但热路径仍读旧值，开关静默无效）；
    /// - 两条 OR 链各一处（hot_or_display_changed 与 immediate_changed，漏一条 →
    ///   只改本项时面板回「无改动」）。
    ///
    /// 回退即 FAIL：删掉任一处接线 → 对应断言失败。
    #[test]
    fn tool_compat_mapping_is_fully_wired() {
        let src = include_str!("service.rs");
        let types = include_str!("types.rs");
        // needle 运行时拼接：include_str! 会把本测试自己的字面量也读进来。
        let field = format!("tool_compat{}", "_mapping");

        let snapshot = format!("{field}: config.{field},");
        assert!(
            src.contains(&snapshot),
            "配置快照必须逐字段从 config 读 `{snapshot}`，否则面板读不到真实值"
        );
        let update = format!("req.{field}");
        assert!(
            src.contains(&update),
            "必须有 `if let Some(v) = {update}` 的 TIER3 更新分支，否则面板改不动它"
        );
        let setter = format!("set_tool{}", "_compat_mapping(v)");
        assert!(
            src.contains(&setter),
            "改后必须调 converter 的 `{setter}` 写进程镜像，否则热路径读旧值"
        );
        // 响应结构与请求结构各一处（快照 + 请求）。
        assert!(
            types.matches(&field).count() >= 2,
            "types.rs 里响应结构与请求结构都必须有该字段（当前 {} 处）",
            types.matches(&field).count()
        );
        // 两条 OR 链（hot_or_display_changed 与 immediate_changed）各必须含本 flag。
        assert!(
            src.matches(&format!("|| {field}_changed.is_some()")).count() >= 2,
            "本 flag 必须同时进 hot_or_display_changed 与 immediate_changed 两条 OR 链"
        );
    }

    /// 🔴 回归：透传模拟缓存必须**全套接线**（快照 / 更新分支 / TIER3 setter 应用 /
    /// 两条 OR 链），否则面板改了不生效且回「无改动」。
    ///
    /// 回退即 FAIL：删掉任一处接线 → 对应断言失败。
    #[test]
    fn mock_cache_config_is_fully_wired() {
        let src = include_str!("service.rs");
        // types.rs 的测试段（#[cfg(test)] 之后）含同名字段的访问/构造，会垫底 count ——
        // 先截断测试段再数，count 只反映生产结构（响应 + 请求各一处）。
        let types = include_str!("types.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap_or("");
        // needle 运行时拼接：include_str! 会把本测试自己的字面量也读进来。
        let field = format!("mock_cache{}", "_changed");

        // 快照：面板读到的配置快照必须逐字段来自 config（needle 拼接防自证）。
        let snapshot_enabled = format!("mock_cache_enabled: config.mock_cache_{}", "enabled,");
        let snapshot_ratio = format!("mock_cache_read_ratio: config.mock_cache_{}", "read_ratio,");
        assert!(
            src.contains(&snapshot_enabled) && src.contains(&snapshot_ratio),
            "配置快照必须逐字段从 config 读 mock 两字段，否则面板读不到真实值"
        );
        let update_enabled = format!("req.mock_cache_{}", "enabled");
        let update_ratio = format!("req.mock_cache_{}", "read_ratio");
        assert!(
            src.contains(&update_enabled) && src.contains(&update_ratio),
            "必须有更新分支读取 req 两字段，否则面板改不动它"
        );
        // setter 调用存在（needle 拼接，防测试段字面量自证；折叠空白防 rustfmt 拆行）。
        let compact: String = src.chars().filter(|c| !c.is_whitespace()).collect();
        let setter = format!(
            "set_mock_cache{}",
            "_config(config.mock_cache_enabled,config.mock_cache_read_ratio,)"
        );
        assert!(
            compact.contains(&setter),
            "改后必须调 handlers 的 set_mock_cache_config 写进程镜像，否则热路径读旧值"
        );
        // 响应结构与请求结构各一处（快照 + 请求）；测试段已截断，count 只数生产字段。
        assert!(
            types.matches("mock_cache_enabled").count() >= 2
                && types.matches("mock_cache_read_ratio").count() >= 2,
            "types.rs 里响应结构与请求结构都必须有该字段"
        );
        // 两条 OR 链（hot_or_display_changed 与 immediate_changed）各必须含本 flag。
        // 与 native_thinking 的 `_changed.is_some()` 不同，本 flag 是 bool：needle 为 `|| {field}`。
        assert!(
            src.matches(&format!("|| {field}")).count() >= 2,
            "本 flag 必须同时进 hot_or_display_changed 与 immediate_changed 两条 OR 链"
        );
    }

    /// 🔴 回归：错误码/提示词覆盖表必须**全套接线**（快照 / 更新分支 / 先校验再写盘 /
    /// OR 链 / import_config 同校验），否则面板改了不生效且回「无改动」。
    ///
    /// 回退即 FAIL：删掉任一处接线 → 对应断言失败。needle 全部运行时拼接
    /// （include_str! 会读到本测试自身，防自证绿，守卫纪律见 CURRENT.md）。
    #[test]
    fn error_messages_config_is_fully_wired() {
        let src = include_str!("service.rs");
        let types = include_str!("types.rs");

        let snapshot = format!("error_messages: config.error_messages{}", ".clone(),");
        assert!(
            src.contains(&snapshot),
            "配置快照必须逐字段从 config 读 error_messages，否则面板读不到真实值"
        );
        let update = format!("req.error{}", "_messages");
        assert!(
            src.contains(&update),
            "必须有更新分支读取 req.error_messages，否则面板改不动它"
        );
        // 函数定义 + 更新分支 + import_config 三处（needle 拼接，防测试段自证）。
        let define = format!("fn validate_error{}", "_messages(");
        assert!(
            src.contains(&define),
            "validate_error_messages 函数必须存在"
        );
        let update_call = format!("validate_error_messages(&merged{}", ")");
        assert!(
            src.contains(&update_call),
            "更新分支必须先调 validate_error_messages（merged）再写盘"
        );
        let import_call = format!("validate_error_messages(&imported.error{}", "_messages)");
        assert!(
            src.contains(&import_call),
            "import_config 必须校验导入的 error_messages（失败整份拒绝零写盘）"
        );
        // 整表拒绝语义：校验失败必须 Err 短路（保持旧表）。
        // ⚠️ 2026-08-15 per-key merge 改造后变量名 em → merged（merge 在赋值前完成），
        // needle 同步更新；语义不变（校验失败 Err 短路 = 旧表不被替换）。
        let err_short = format!(
            "validate_error_messages(&merged).map_err(AdminServiceError::{}",
            "InvalidCredential)?"
        );
        assert!(
            src.contains(&err_short),
            "校验失败必须整表拒绝（Err 短路，保持旧表）"
        );
        // 两条 OR 链（hot_or_display_changed 与 immediate_changed）各必须含本 flag
        // （bool flag：needle `|| {field}`，count>=2 防只进一条链）。
        let or_needle = format!("|| error_messages{}", "_changed");
        assert!(
            src.matches(&or_needle).count() >= 2,
            "error_messages_changed 必须同时进 hot_or_display_changed 与 immediate_changed \
             两条 OR 链：漏 hot 链 → 存盘但热路径读旧表（无 TIER3 setter，这是唯一生效通道）；\
             漏 immediate 链 → 面板只改本项时回「未检测到变更」，与实际不符"
        );
        // 响应结构与请求结构各一处（快照 + 请求）。
        assert!(
            types.matches("error_messages").count() >= 2,
            "types.rs 里响应结构与请求结构都必须有该字段（当前 {} 处）",
            types.matches("error_messages").count()
        );
    }

    // ---- validate_error_messages 校验矩阵（纯函数，不碰网络/磁盘）----

    fn error_entry(
        status: Option<u16>,
        ty: Option<&str>,
        message: Option<&str>,
        ra: Option<u64>,
    ) -> crate::model::error_messages::ErrorMessageOverride {
        crate::model::error_messages::ErrorMessageOverride {
            status,
            r#type: ty.map(str::to_string),
            message: message.map(str::to_string),
            retry_after_secs: ra,
        }
    }

    fn one_error_entry(
        entry: crate::model::error_messages::ErrorMessageOverride,
    ) -> HashMap<String, crate::model::error_messages::ErrorMessageOverride> {
        let mut m = HashMap::new();
        m.insert("test_key".to_string(), entry);
        m
    }

    #[test]
    fn validate_accepts_full_valid_entry() {
        let table = one_error_entry(error_entry(
            Some(429),
            Some("rate_limit_error"),
            Some("请按 Retry-After 退避后重试。"),
            Some(8),
        ));
        assert!(validate_error_messages(&table).is_ok(), "合法条目必须通过");
    }

    #[test]
    fn validate_rejects_status_out_of_whitelist() {
        for bad in [200u16, 418, 451, 529, 600] {
            let table = one_error_entry(error_entry(Some(bad), Some("api_error"), None, None));
            let err = validate_error_messages(&table).expect_err("白名单外的 status 必须整表拒绝");
            assert!(
                err.contains(".status"),
                "错误必须点名 status 字段，实际: {err}"
            );
        }
    }

    #[test]
    fn validate_rejects_type_out_of_whitelist() {
        for bad in [
            "service_unavailable",
            "internal_error",
            "upstream_error",
            "bogus_type",
        ] {
            let table = one_error_entry(error_entry(Some(502), Some(bad), None, None));
            let err = validate_error_messages(&table).expect_err("白名单外的 type 必须整表拒绝");
            assert!(err.contains(".type"), "错误必须点名 type 字段，实际: {err}");
        }
    }

    #[test]
    fn validate_rejects_status_type_combination_violation() {
        // 429 → 只允许 rate_limit_error / overloaded_error（billing_error 已移除，
        // 其拒绝在 type 白名单层，见 validate_rejects_billing_error_and_quota_exceeded_error）。
        for bad in [
            ("429", "invalid_request_error"),
            ("429", "api_error"),
            ("429", "not_found_error"),
            ("401", "rate_limit_error"),
            ("403", "authentication_error"),
            ("404", "permission_error"),
            ("400", "overloaded_error"),
            ("413", "rate_limit_error"),
            ("500", "overloaded_error"),
            ("502", "rate_limit_error"),
            ("503", "not_found_error"),
        ] {
            let table = one_error_entry(error_entry(
                Some(bad.0.parse().unwrap()),
                Some(bad.1),
                None,
                None,
            ));
            let err = validate_error_messages(&table).expect_err("组合违例必须整表拒绝");
            assert!(
                err.contains("组合不合法"),
                "错误必须说明组合约束，实际: {err}"
            );
        }
    }

    #[test]
    fn validate_rejects_decision_words() {
        // 决策词黑名单：任一命中 → 拒（设计 §二 5）。
        for bad in [
            "credit balance is too low",
            "organization has been disabled",
            "message says overloaded_error here",
            "Monthly quota exhausted",
            "this account has billing issues",
        ] {
            let table = one_error_entry(error_entry(
                Some(429),
                Some("rate_limit_error"),
                Some(bad),
                None,
            ));
            assert!(
                validate_error_messages(&table).is_err(),
                "决策词必须拒绝: {bad}"
            );
        }
        // quota+exhausted 无豁免（B2）：billing_error 已从白名单移除，旧豁免条件
        // 永远不可达——配什么 type 都拒（Claude Code CLI 层 D 判定/opencode 模式
        // 匹配都拿 quota+exhausted 当重试决策输入）。
        let rejected = one_error_entry(error_entry(
            Some(429),
            Some("billing_error"),
            Some("Monthly quota exhausted"),
            None,
        ));
        assert!(
            validate_error_messages(&rejected).is_err(),
            "quota+exhausted 必须无条件拒绝（billing_error 已不可配置，无豁免）"
        );
    }

    #[test]
    fn validate_rejects_retry_after_out_of_range() {
        let table = one_error_entry(error_entry(
            Some(429),
            Some("rate_limit_error"),
            None,
            Some(3601),
        ));
        let err = validate_error_messages(&table).expect_err("retryAfterSecs 超 3600 必须拒绝");
        assert!(
            err.contains("retryAfterSecs"),
            "错误必须点名 retryAfterSecs，实际: {err}"
        );
        let ok = one_error_entry(error_entry(
            Some(429),
            Some("rate_limit_error"),
            None,
            Some(3600),
        ));
        assert!(validate_error_messages(&ok).is_ok(), "3600 是边界合法值");
    }

    #[test]
    fn validate_accepts_load_bearing_message_with_warning() {
        // 承重字符串：提示不硬拒（shield COOLING_MARKERS 三哨兵 / prompt is too long / 背压哨兵）。
        // ⚠️ 2026-08-15 勘误：「等容量」不是 shield 判据（仅注释出现），已从词表移除——
        // 含它的普通文案照常放行（见 error_messages.rs 词表测试）。
        for keep in [
            "All credentials are temporarily cooling down. Please retry.",
            "Gateway inbound rate shaping is at capacity; retrying immediately will not help.",
            "prompt is too long: 上下文窗口已满",
            "This is gateway-side backpressure; retrying immediately will not help.",
        ] {
            let table = one_error_entry(error_entry(
                Some(400),
                Some("invalid_request_error"),
                Some(keep),
                None,
            ));
            assert!(
                validate_error_messages(&table).is_ok(),
                "承重字符串必须提示不硬拒: {keep}"
            );
        }
        // 非承重文案（含「等容量」）同样放行——它不承载任何 shield 判据。
        let plain = one_error_entry(error_entry(
            Some(400),
            Some("invalid_request_error"),
            Some("上游仍不可用（等容量）。请退避重试。"),
            None,
        ));
        assert!(
            validate_error_messages(&plain).is_ok(),
            "「等容量」不是承重词，含它的文案必须正常放行"
        );
    }

    #[test]
    fn validate_rejects_bad_key_name_and_oversize() {
        // key 命名规范。
        for bad_key in ["QuotaExhausted", "quota exhausted", "1quota", "quota!", ""] {
            let mut table = HashMap::new();
            table.insert(
                bad_key.to_string(),
                error_entry(Some(429), Some("rate_limit_error"), None, None),
            );
            assert!(
                validate_error_messages(&table).is_err(),
                "非法 key 名必须拒绝: {bad_key:?}"
            );
        }
        // message 超长。
        let long_msg = "x".repeat(501);
        let table = one_error_entry(error_entry(
            Some(429),
            Some("rate_limit_error"),
            Some(&long_msg),
            None,
        ));
        assert!(
            validate_error_messages(&table).is_err(),
            "message 超过 500 字符必须拒绝"
        );
        // 表条目数上限（200）。
        let mut big = HashMap::new();
        for i in 0..=ERROR_TABLE_MAX_ENTRIES {
            big.insert(format!("key_{i}"), error_entry(None, None, None, None));
        }
        assert!(
            validate_error_messages(&big).is_err(),
            "超过 {} 条必须拒绝",
            ERROR_TABLE_MAX_ENTRIES
        );
    }

    /// B1：组合校验必须用「配置 or 默认表」的**最终渲染值**——只配 status 或只配
    /// type 时另一半落默认，仍必须过组合矩阵（防单字段绕过）。
    ///
    /// key 不硬编码：默认表可能被并行任务重写（key 集变化），动态从
    /// `default_error_messages()` 取「默认 429+rate_limit_error」的 key——
    /// 改表场景测试自适应（表里没有该基线 key 时显式 panic 提示）。
    #[test]
    fn validate_rendered_combination_rejects_single_field_bypass() {
        let table = crate::model::error_messages::default_error_messages();
        let base = table
            .iter()
            .find(|(_, s, t, ..)| *s == 429 && *t == "rate_limit_error")
            .map(|(k, ..)| k.to_string())
            .expect("默认表必须保留至少一个 429+rate_limit_error 的 key（B1 渲染值组合校验基线）");

        // status-only 绕过：只配 status=401 → 渲染 401 + 默认 rate_limit_error → 拒。
        let mut status_only = HashMap::new();
        status_only.insert(base.clone(), error_entry(Some(401), None, None, None));
        let err = validate_error_messages(&status_only)
            .expect_err("只配 status 必须按渲染值过组合矩阵");
        assert!(err.contains("组合不合法"), "实际: {err}");

        // type-only 绕过：只配 type=authentication_error → 渲染 429 + 该 type → 拒。
        let mut type_only = HashMap::new();
        type_only.insert(base, error_entry(None, Some("authentication_error"), None, None));
        let err = validate_error_messages(&type_only)
            .expect_err("只配 type 必须按渲染值过组合矩阵");
        assert!(err.contains("组合不合法"), "实际: {err}");

        // 双显式合法 → 通过；双显式非法 → 拒。
        let ok = one_error_entry(error_entry(Some(429), Some("rate_limit_error"), None, None));
        assert!(validate_error_messages(&ok).is_ok(), "双显式合法组合必须通过");
        let bad = one_error_entry(error_entry(Some(429), Some("api_error"), None, None));
        assert!(validate_error_messages(&bad).is_err(), "双显式非法组合必须拒绝");
    }

    /// B1 改默认表场景：默认表所有「默认 status/type 都在官方白名单」的 key，其默认
    /// 渲染值必须自身组合合法——否则管理员对该 key 的任何配置（含只改 message 的
    /// 合法姿势）都会被渲染值组合检查误伤。并行任务重写默认表时本测试自动跟随。
    #[test]
    fn validate_default_table_combos_are_self_consistent() {
        let table = crate::model::error_messages::default_error_messages();
        let mut official = 0;
        for &(key, s, t, ..) in table {
            if ERROR_STATUS_WHITELIST.contains(&s) && ERROR_TYPE_WHITELIST.contains(&t) {
                official += 1;
                assert!(
                    error_type_compatible_with_status(s, t),
                    "默认表 {key}: 默认渲染 {s}+{t} 必须组合合法，\
                     否则管理员对该 key 的任何配置都会被渲染值检查拒绝"
                );
            }
        }
        assert!(official > 0, "默认表必须存在官方值域内的 key（否则渲染值检查无靶点）");
    }

    /// m2：504 必须在 status 白名单（`upstream_timeout` 默认 504——管理员显式写回
    /// 默认值时不被拒），组合上归 5xx→api_error 族（H5）。
    #[test]
    fn validate_accepts_504_upstream_timeout_default() {
        let ok = one_error_entry(error_entry(Some(504), Some("api_error"), None, None));
        assert!(validate_error_messages(&ok).is_ok(), "504+api_error 必须合法");
        let bad = one_error_entry(error_entry(Some(504), Some("rate_limit_error"), None, None));
        assert!(
            validate_error_messages(&bad).is_err(),
            "504 组合必须归 api_error 族"
        );
    }

    /// B2：billing_error / quota_exceeded_error 已从 type 白名单移除——任何 status
    /// 配置都拒绝（Claude Code CLI 层对 429/402+billing_error 重试约 7 次/1 分钟 =
    /// 重试风暴；quota_exceeded_error 需 402 支持，见白名单注释）；quota+exhausted
    /// 决策词无豁免（豁免条件随 billing_error 移除永远不可达）。
    #[test]
    fn validate_rejects_billing_error_and_quota_exceeded_error() {
        for status in [400u16, 401, 403, 404, 413, 429, 500, 502, 503, 504] {
            let table = one_error_entry(error_entry(Some(status), Some("billing_error"), None, None));
            let err = validate_error_messages(&table)
                .expect_err("billing_error 配置必须整表拒绝（重试风暴）");
            assert!(err.contains(".type"), "必须点名 type 字段，实际: {err}");
        }
        // 只配 type=billing_error（status 落默认）同样拒。
        let type_only = one_error_entry(error_entry(None, Some("billing_error"), None, None));
        assert!(
            validate_error_messages(&type_only).is_err(),
            "type-only billing_error 必须拒绝"
        );
        // quota_exceeded_error 同移除（非官方 type，402 未进 status 白名单）。
        let quota_ty = one_error_entry(error_entry(
            Some(429),
            Some("quota_exceeded_error"),
            None,
            None,
        ));
        assert!(
            validate_error_messages(&quota_ty).is_err(),
            "quota_exceeded_error 必须拒绝（待 402 改造后放行）"
        );
        // quota+exhausted 决策词：配任何 type（含无 type）都无条件拒。
        for ty in [Some("rate_limit_error"), Some("overloaded_error"), None] {
            let table = one_error_entry(error_entry(
                Some(429),
                ty,
                Some("Monthly quota exhausted"),
                None,
            ));
            assert!(
                validate_error_messages(&table).is_err(),
                "quota+exhausted 必须无条件拒绝 (ty={ty:?})"
            );
        }
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
                    stale: false,
                    optimistic: false,
                },
            },
        )
    }

    /// 造一个带单个凭据的 AdminService（余额展示 / 节点池 / 多开测试共用）。
    pub(super) fn mk_service_with_one_credential() -> AdminService {
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

    /// 造一条余额缓存条目（remaining=90 / limit=100 / used=10）。
    fn mk_cached_balance(id: u64, cached_at: f64) -> CachedBalance {
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
                stale: false,
                optimistic: false,
            },
        }
    }

    /// 造一个池：`n` 份**同 key** 的 api_key 号（模拟分身组）+ 一个**不同 key** 的对照号。
    fn mk_service_with_clone_group(n: u64) -> AdminService {
        let mut creds = Vec::new();
        for i in 1..=n {
            let mut c = crate::kiro::model::credentials::KiroCredentials::default();
            c.id = Some(i);
            c.auth_method = Some("api_key".to_string());
            // 同一个 key ⇒ 同一个上游账号 ⇒ 必须共享余额
            c.kiro_api_key = Some("ksk_shared_group".to_string());
            creds.push(c);
        }
        // 对照：不同 key，绝不能与上面那组混成一条
        let mut other = crate::kiro::model::credentials::KiroCredentials::default();
        other.id = Some(n + 1);
        other.auth_method = Some("api_key".to_string());
        other.kiro_api_key = Some("ksk_different".to_string());
        creds.push(other);

        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                false,
            )
            .expect("构造 token manager"),
        );
        AdminService::new(tm, Vec::<String>::new())
    }

    /// ⭐ 回归（dwgx 需求「同一个 key 的分身和凭据余额必须同步」）：
    /// 缓存按**账号**键，一次写入即全组可见。
    ///
    /// # 旧代码为何 FAIL
    ///
    /// `balance_cache` 原先是 `HashMap<u64, _>` 按**凭据 id** 键，于是同一个 `ksk_` key 的
    /// N 份分身各存一份余额 ⇒ 面板上同组各份显示的数字**互不相同**（谁最近刷过谁新），
    /// 而它们描述的本来是同一个上游账号、同一份配额。
    /// 线上实测缓存键是 `620/623/622/624` —— 四份分身四条独立记录。
    ///
    /// # 断言的是可观测状态
    ///
    /// 不断言内部键长什么样（那是实现细节），而是断言 `list_cached_balances` 这个
    /// **前端真正消费的端点**对同组各份返回同一个 `remaining`。
    ///
    /// 把 `balance_cache` 改回按 id 键 → 本测试必 FAILED。
    #[test]
    fn same_api_key_credentials_share_one_balance() {
        let svc = mk_service_with_clone_group(4);

        // 只给**其中一份**写缓存（模拟"任一份刷新过"）
        let now = Utc::now().timestamp() as f64;
        {
            let key = svc.balance_cache_key(2);
            let mut cache = svc.balance_cache.lock();
            cache.insert(key, mk_cached_balance(2, now));
        }

        let resp = svc.get_cached_balances();

        // 同组四份**全部**应拿到余额，且数字一致
        for id in 1..=4u64 {
            let item = resp.balances.get(&id).unwrap_or_else(|| {
                panic!("凭据 #{id} 应共享同组余额（旧代码按 id 键 ⇒ 只有 #2 有值）")
            });
            assert!(
                (item.balance.remaining - 90.0).abs() < 1e-6,
                "凭据 #{id} 的 remaining 应与同组一致，实际 {}",
                item.balance.remaining
            );
        }

        // ⭐ 承重反向断言：**不同 key** 的号绝不能被混进来。
        // 若为了"统一"给所有号一个共享键，面板会显示别人的额度 —— 那比不同步严重得多。
        assert!(
            resp.balances.get(&5).is_none(),
            "不同 key 的凭据 #5 不得共享这条余额（那会显示别的账号的额度）"
        );
    }

    /// ⭐ 回归：旧格式缓存（按凭据 id 键）必须被**迁移**成账号键，而不是静默失效。
    ///
    /// # 不迁移的代价
    ///
    /// 键从 `id` 改成 `sha256(apiKey)` 后，旧文件里的十进制 id 键永远不会被命中 ⇒
    /// 升级后 api_key 号余额全空 ⇒ 面板集体转圈打 `getUsageLimits`。那是 `web_portal`
    /// 上游探测，本仓调优结论是绝不为展示类需求反复打它。
    ///
    /// 实测规模：线上 5 条缓存 / 5 个 api_key 号 / **只有 1 个不同的 key** ⇒ 并成 1 条。
    ///
    /// # 并组取最新
    ///
    /// N 个 id 映射到同一账号键时按 `cached_at` 取最新 —— 旧的那些本来就是冗余副本。
    ///
    /// 把迁移改回"键原样保留" → 本测试必 FAILED。
    #[test]
    fn old_id_keyed_cache_migrates_to_account_key() {
        let dir = std::env::temp_dir().join(format!("kiro_bal_mig_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kiro_balance_cache.json");

        let now = Utc::now().timestamp() as f64;
        // 旧格式：三份同 key 分身各一条，cached_at 递增（#3 最新）
        let mut map: HashMap<String, CachedBalance> = HashMap::new();
        for (id, age) in [(1u64, 300.0), (2u64, 200.0), (3u64, 100.0)] {
            let mut cb = mk_cached_balance(id, now - age);
            // 用 remaining 标记是哪条，便于断言"取到的是最新那条"
            cb.data.remaining = 90.0 - age;
            map.insert(id.to_string(), cb);
        }
        std::fs::write(&path, serde_json::to_string(&map).unwrap()).unwrap();

        // 池里是三份同 key 的 api_key 号（mk_service_with_clone_group 的构造）
        let svc = mk_service_with_clone_group(3);
        let loaded = AdminService::load_balance_cache_from(&Some(path.clone()), &svc.token_manager);

        // 三条旧键并成一条账号键
        assert_eq!(
            loaded.len(),
            1,
            "三份同 key 分身的旧缓存应并成 1 条账号键，实际 {} 条：{:?}",
            loaded.len(),
            loaded.keys().collect::<Vec<_>>()
        );
        let account_key = svc.balance_cache_key(1);
        let kept = loaded
            .get(&account_key)
            .expect("并组后的键应等于 balance_cache_key 算出的账号键");
        // 取的是 cached_at 最新那条（age=100 ⇒ remaining = 90-100 = -10）
        assert!(
            (kept.data.remaining - (-10.0)).abs() < 1e-6,
            "并组应保留 cached_at 最新的那条，实际 remaining={}",
            kept.data.remaining
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⭐ 回归：删掉一份分身**不得**清掉整组共享的余额缓存。
    ///
    /// 无条件 `remove` 会让「删一份」把全组缓存清空 ⇒ 剩下的份面板显示"暂无数据"，
    /// 直到下次后台刷新（默认 30 分钟）。而键必须在删除**之前**算 —— 删掉后
    /// `export_credential` 返 `None`、键回落成 id 字符串，清的是不存在的键。
    ///
    /// 把 `prune_balance_cache_for_deleted` 改回无条件 remove → 本测试必 FAILED。
    #[test]
    fn deleting_one_clone_keeps_group_balance_cache() {
        let svc = mk_service_with_clone_group(3);
        let now = Utc::now().timestamp() as f64;
        let group_key = svc.balance_cache_key(1);
        {
            let mut cache = svc.balance_cache.lock();
            cache.insert(group_key.clone(), mk_cached_balance(1, now));
        }

        // 删掉组内一份（force 跳过"必须先禁用"那道门）
        svc.delete_credential_forced(2, true).expect("删除应成功");

        assert!(
            svc.balance_cache.lock().contains_key(&group_key),
            "删一份分身后，整组共享的余额缓存必须仍在（同 key 的其余份还要用它）"
        );
        // 剩下的份仍能读到
        let resp = svc.get_cached_balances();
        assert!(
            resp.balances.contains_key(&1) && resp.balances.contains_key(&3),
            "剩余份应仍有余额可显示"
        );
    }

    /// 回归（dwgx 需求「用了余额之后要刷新额度显示」）：展示路径必须用本地累计花费做乐观修正。
    ///
    /// **旧代码为何 FAIL**：余额真值由后台每 30 分钟刷新一次，展示端点原样吐缓存 →
    /// 跑完一批请求后额度**最多 30 分钟不动**，用户以为没生效。
    /// 本测试推进 `total_credits_used` 而不刷新缓存，断言展示值已跟着走。
    ///
    /// 关键约束：**绝不为此每请求打上游** —— 那是 web_portal 探测会加重风控
    /// （线上号池正被风控烧号）。所以修正只用已有的两份内存数据（累计花费 + 缓存基线）。
    #[test]
    fn cached_balances_apply_optimistic_credit_adjustment() {
        let svc = mk_service_with_one_credential();
        // 播种：缓存里有真值（remaining=90），基线 credits_used=0
        // 键走 balance_cache_key（缓存已改为按**账号**键，不再是凭据 id）。
        let k = svc.balance_cache_key(1);
        svc.balance_cache
            .lock()
            .insert(k, mk_cached_balance(1, Utc::now().timestamp() as f64));
        svc.token_manager.set_balance_snapshots(HashMap::from([(
            1u64,
            crate::kiro::token_manager::BalanceSnapshot {
                remaining_at_cache: 90.0,
                effective_limit: 100.0,
                credits_used_at_cache: 0.0,
            },
        )]));

        // 未花钱时：展示值 = 真值，且不标 optimistic
        let before = svc.get_cached_balances();
        let b0 = &before.balances.get(&1).expect("应有缓存条目").balance;
        assert_eq!(b0.remaining, 90.0);
        assert!(!b0.optimistic, "未花钱不应标记乐观修正");

        // 花掉 5 个 credit（模拟请求完成后 meteringEvent 累加），**不**刷新余额缓存
        svc.token_manager.add_credits(1, 5.0);

        let after = svc.get_cached_balances();
        let b1 = &after.balances.get(&1).expect("应有缓存条目").balance;
        assert_eq!(
            b1.remaining, 85.0,
            "remaining 未跟随本地花费推进（旧代码原样吐缓存，30 分钟内恒为 90）"
        );
        assert_eq!(b1.current_usage, 15.0, "current_usage 应同步推进");
        assert!(b1.optimistic, "含本地推算的值必须标记 optimistic，供前端区分真值");
    }

    /// 回归：乐观修正**只单向推进**，且 remaining 不得为负。
    ///
    /// 基线可能比当前累计值更大（重启后 total_credits_used 从 0 重新累计），
    /// 此时 delta<0，绝不能把额度往回加 —— 那会显示出"用了反而变多"。
    #[test]
    fn optimistic_adjustment_is_monotonic_and_clamped() {
        let svc = mk_service_with_one_credential();
        // 键走 balance_cache_key（缓存已改为按**账号**键）。
        let k = svc.balance_cache_key(1);
        svc.balance_cache
            .lock()
            .insert(k, mk_cached_balance(1, Utc::now().timestamp() as f64));
        // 基线 999：远大于当前累计（0），delta 为负
        svc.token_manager.set_balance_snapshots(HashMap::from([(
            1u64,
            crate::kiro::token_manager::BalanceSnapshot {
                remaining_at_cache: 90.0,
                effective_limit: 100.0,
                credits_used_at_cache: 999.0,
            },
        )]));
        let r = svc.get_cached_balances();
        let b = &r.balances.get(&1).unwrap().balance;
        assert_eq!(b.remaining, 90.0, "delta<=0 时不得改动展示值（不能出现'用了反而变多'）");
        assert!(!b.optimistic);

        // 花超额度：remaining 收敛到 0 而非负数
        svc.token_manager.set_balance_snapshots(HashMap::from([(
            1u64,
            crate::kiro::token_manager::BalanceSnapshot {
                remaining_at_cache: 90.0,
                effective_limit: 100.0,
                credits_used_at_cache: 0.0,
            },
        )]));
        svc.token_manager.add_credits(1, 500.0);
        let r2 = svc.get_cached_balances();
        let b2 = &r2.balances.get(&1).unwrap().balance;
        assert_eq!(b2.remaining, 0.0, "remaining 不得为负");
        assert!(b2.usage_percentage <= 100.0, "百分比不得超 100");
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

        // 传一个空池的 token_manager：本测试只验「7 天展示保留期」的淘汰，
        // 不验账号键迁移（那条由 migration 专用测试覆盖）。空池 ⇒ 键原样保留。
        let tm_empty = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![],
                None,
                None,
                false,
            )
            .expect("构造空池 token manager"),
        );
        let loaded = AdminService::load_balance_cache_from(&Some(path.clone()), &tm_empty);

        // 键现在**原样保留为字符串**（缓存改按账号键后不再 parse 成 u64）。
        // 磁盘格式不变（JSON 对象键本来就是字符串），所以旧文件仍能读回。
        // 陈旧但在展示窗口内 → 保留（重启后前端仍能显示上次数字）
        assert!(loaded.contains_key("1"), "陈旧但在 7 天内的缓存必须保留");
        // 超过展示窗口 → 丢弃（避免无界陈旧）
        assert!(!loaded.contains_key("2"), "超过 7 天的缓存应被丢弃");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn upsert_req(id: Option<u64>, url: &str, password: Option<&str>) -> SocksNodeUpsertRequest {
        SocksNodeUpsertRequest {
            id,
            name: Some("n".into()),
            url: url.into(),
            username: Some("u".into()),
            password: password.map(|s| s.to_string()),
            enabled: None,
        }
    }

    /// ⭐ 承重：**省略 `password` 键 = 不改密码**。
    ///
    /// 回退即 FAIL：把 upsert 里那个 `match req.password` 换成无条件
    /// `node.password = req.password` → 改个节点名就把密码抹成 None，
    /// 已绑该节点的分身在下次请求时全部因代理认证失败而掉线。
    #[tokio::test]
    async fn omitted_password_keeps_existing() {
        let svc = mk_service_with_one_credential();
        let id = svc
            .upsert_socks_node(upsert_req(
                None,
                "socks5://node.invalid:40002",
                Some("secret"),
            ))
            .await
            .expect("新建节点");
        assert_eq!(
            svc.socks_node_proxy(id).and_then(|(_, _, p)| p).as_deref(),
            Some("secret")
        );

        // 只改名，**不带** password 键。
        svc.upsert_socks_node(SocksNodeUpsertRequest {
            id: Some(id),
            name: Some("renamed".into()),
            url: "socks5://node.invalid:40002".into(),
            username: Some("u".into()),
            password: None,
            enabled: None,
        })
        .await
        .expect("更新节点");

        assert_eq!(
            svc.socks_node_proxy(id).and_then(|(_, _, p)| p).as_deref(),
            Some("secret"),
            "省略 password 键必须保留原密码"
        );
    }

    /// `password: ""` 才是清空。
    #[tokio::test]
    async fn empty_password_clears() {
        let svc = mk_service_with_one_credential();
        let id = svc
            .upsert_socks_node(upsert_req(
                None,
                "socks5://node.invalid:40002",
                Some("secret"),
            ))
            .await
            .unwrap();
        svc.upsert_socks_node(upsert_req(
            Some(id),
            "socks5://node.invalid:40002",
            Some(""),
        ))
        .await
        .unwrap();
        assert!(
            svc.socks_node_proxy(id).and_then(|(_, _, p)| p).is_none(),
            "显式空字符串必须清空密码"
        );
    }

    /// 列表视图**绝不外传密码**，只给 hasPassword。
    #[tokio::test]
    async fn list_never_leaks_password() {
        let svc = mk_service_with_one_credential();
        svc.upsert_socks_node(upsert_req(
            None,
            "socks5://node.invalid:40002",
            Some("secret"),
        ))
        .await
        .unwrap();
        let view = svc.list_socks_nodes();
        assert_eq!(view.len(), 1);
        assert!(view[0].has_password, "应报告设了密码");
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("secret"), "序列化后绝不能含密码明文: {json}");
    }

    /// 更新一个**不存在**的 id 必须 404，不得静默新建。
    #[tokio::test]
    async fn upsert_unknown_id_is_not_found() {
        let svc = mk_service_with_one_credential();
        let err = svc
            .upsert_socks_node(upsert_req(Some(999), "socks5://node.invalid:40002", None))
            .await
            .expect_err("不存在的 id 应报错");
        assert!(
            matches!(err, AdminServiceError::NotFound { id: 999 }),
            "应是 NotFound，实际 {err:?}"
        );
        assert!(svc.list_socks_nodes().is_empty(), "不得静默新建");
    }

    /// 内网 IP 字面量的节点地址必须被拒（只覆盖字面量，见 validate_proxy_address 文档）。
    ///
    /// 用**云元数据地址**（169.254.169.254）而不是 127.0.0.1 做样本：节点地址走
    /// `SsrfPolicy::AdminConfigured`，而链路本地段是它明确不豁免的（唯一豁免的是
    /// 198.18.0.0/15 fake-IP 池段，见下方第二条断言）。挑一个策略切换后语义仍然
    /// 明确的地址，测试才不会随策略调整而变成「碰巧还过」。
    #[tokio::test]
    async fn internal_node_address_is_rejected() {
        let svc = mk_service_with_one_credential();
        let err = svc
            .upsert_socks_node(upsert_req(None, "socks5://169.254.169.254:1080", None))
            .await
            .expect_err("云元数据链路本地地址应被拒");
        assert!(matches!(err, AdminServiceError::InvalidCredential(_)));
        assert!(svc.list_socks_nodes().is_empty());

        // ⭐ 承重：fake-IP 池段必须能加进来（这才是 AdminConfigured 的目的）。
        // 回退即 FAIL：把 validate_proxy_address 的策略改回 Strict —— 开了 Clash
        // fake-IP 的机器上任意域名都解析到该段，节点池对这些用户完全不可用。
        svc.upsert_socks_node(upsert_req(None, "socks5://198.18.0.46:40002", None))
            .await
            .expect("fake-IP 池段（198.18.0.0/15）在 AdminConfigured 下必须放行");
        assert_eq!(svc.list_socks_nodes().len(), 1);
    }

    /// 删节点**不动**已绑该节点的凭据（删一个节点不该让一批分身掉线）。
    #[tokio::test]
    async fn deleting_node_leaves_credential_proxy_untouched() {
        let svc = mk_service_with_one_credential();
        let id = svc
            .upsert_socks_node(upsert_req(None, "socks5://node.invalid:40002", Some("p")))
            .await
            .unwrap();
        // 把节点地址绑到凭据上（模拟「生成分身时写进凭据」）。
        svc.token_manager
            .set_credential_proxy(
                1,
                Some("socks5://node.invalid:40002".into()),
                Some("u".into()),
                Some("p".into()),
            )
            .expect("绑定代理");

        assert!(svc.delete_socks_node(id).unwrap());

        let cred = svc.token_manager.export_credential(1).expect("凭据仍在");
        assert_eq!(
            cred.proxy_url.as_deref(),
            Some("socks5://node.invalid:40002"),
            "删节点不得清掉凭据上已生效的代理绑定"
        );
    }

    /// ⭐ 最重要的一条：**文件在但读不出来时，绝不能把它覆盖掉**。
    ///
    /// 回退即 FAIL：把 `load_socks_nodes_from` 的解析失败分支改回
    /// `(Vec::new(), 1, true)`（即「空表 + 允许回写」），或删掉
    /// `persist_socks_nodes` 里那道 `socks_nodes_writable` 判断 —— 两者任一都会让
    /// 下面最后那条断言失败：原文件里的节点与代理密码被一张只有 1 条的表原子覆盖，
    /// 永久丢失。这是把 credentials.json 那条 `exit(1)` 换成只读降级的代价，
    /// 必须有测试兜住。
    #[test]
    fn unreadable_node_file_is_never_overwritten() {
        let dir = std::env::temp_dir().join(format!(
            "ks_socks_ro_{}_{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("socks_nodes.json");

        // 写一份**读不出来**的内容（既非合法 JSON 也非 KSENC1 密文）。
        let garbage = b"{ this is not valid json at all";
        std::fs::write(&path, garbage).unwrap();

        let (nodes, next_id, writable) =
            AdminService::load_socks_nodes_from(&Some(path.clone()), &{
                let mut c = crate::kiro::model::credentials::KiroCredentials::default();
                c.id = Some(1);
                c.auth_method = Some("api_key".into());
                c.kiro_api_key = Some("ksk_ro".into());
                Arc::new(
                    MultiTokenManager::new(
                        crate::model::config::Config::default(),
                        vec![c],
                        None,
                        None,
                        false,
                    )
                    .expect("token manager"),
                )
            });

        assert!(nodes.is_empty(), "读不出来时内存表应为空");
        assert_eq!(next_id, 1);
        assert!(
            !writable,
            "文件存在但解析失败必须进入只读降级，否则下一次修改会抹平它"
        );

        // ⭐ 承重：真的走一遍**写路径**，再核对磁盘。
        //
        // 只调 loader 是不够的（本测试第一版就只做了这一半）：那样删掉
        // `persist_socks_nodes` 里的 writable 判断，测试**照样通过** ——
        // 因为它从没写过。必须构造一个 socks_nodes_path 指向该文件、
        // socks_nodes_writable=false 的 service，然后调 persist 并断言两件事：
        // 调用被拒 + 文件逐字节未变。
        let svc = AdminService {
            socks_nodes: Mutex::new(vec![SocksNode {
                id: 1,
                name: "n".into(),
                url: "socks5://node.invalid:40002".into(),
                username: None,
                password: Some("would-be-written".into()),
                enabled: true,
                last_test: None,
                created_at: 0,
            }]),
            socks_nodes_path: Some(path.clone()),
            socks_nodes_writable: writable, // = false
            ..AdminService::new(
                {
                    let mut c = crate::kiro::model::credentials::KiroCredentials::default();
                    c.id = Some(1);
                    c.auth_method = Some("api_key".into());
                    c.kiro_api_key = Some("ksk_ro2".into());
                    Arc::new(
                        MultiTokenManager::new(
                            crate::model::config::Config::default(),
                            vec![c],
                            None,
                            None,
                            false,
                        )
                        .expect("token manager"),
                    )
                },
                Vec::<String>::new(),
            )
        };
        let err = svc
            .persist_socks_nodes()
            .expect_err("只读降级下回写必须被拒绝");
        assert!(
            matches!(err, AdminServiceError::InternalError(_)),
            "应是 InternalError，实际 {err:?}"
        );

        let after = std::fs::read(&path).unwrap();
        assert_eq!(
            after, garbage,
            "只读降级下原文件必须逐字节保持不变（这是防数据毁灭的唯一护栏）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⭐ 承重：只读降级下的 upsert **不得改内存**。
    ///
    /// 回退即 FAIL：把 `upsert_socks_node` 顶部那句 `self.ensure_socks_writable()?` 删掉
    /// （即回到「先 push 进内存、再由 persist 报错」的顺序）—— 下面第 2 条断言失败：
    /// 调用方收到报错、磁盘上什么都没有，但 `list_socks_nodes()` 里凭空多出一个节点，
    /// 面板会一直显示它直到重启。
    #[tokio::test]
    async fn readonly_degraded_upsert_leaves_memory_untouched() {
        let dir = std::env::temp_dir().join(format!(
            "ks_socks_ro_upsert_{}_{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("socks_nodes.json");
        let garbage = b"{ not json";
        std::fs::write(&path, garbage).unwrap();

        let svc = AdminService {
            socks_nodes: Mutex::new(Vec::new()),
            socks_nodes_path: Some(path.clone()),
            socks_nodes_writable: false,
            ..mk_service_with_one_credential()
        };

        let err = svc
            .upsert_socks_node(upsert_req(None, "socks5://node.invalid:40002", Some("p")))
            .await
            .expect_err("只读降级下新增节点必须报错");
        assert!(
            matches!(err, AdminServiceError::InternalError(_)),
            "应是 InternalError，实际 {err:?}"
        );
        assert!(
            svc.list_socks_nodes().is_empty(),
            "只读降级下报错后内存表必须仍为空，否则面板显示一个磁盘上不存在的节点"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            garbage,
            "原文件必须逐字节未变"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⭐ 承重：只读降级下的 delete / record_test 同样不得改内存。
    ///
    /// 回退即 FAIL：删掉这两个方法顶部的 `ensure_socks_writable()?` —— 删除会让节点
    /// 从面板消失（磁盘上还在），测速结果会写进一张永不落盘的表，两者都是「报错了但
    /// 界面显示已生效」。
    #[test]
    fn readonly_degraded_delete_and_test_leave_memory_untouched() {
        let dir = std::env::temp_dir().join(format!(
            "ks_socks_ro_del_{}_{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("socks_nodes.json");
        std::fs::write(&path, b"{ not json").unwrap();

        let svc = AdminService {
            socks_nodes: Mutex::new(vec![SocksNode {
                id: 7,
                name: "n".into(),
                url: "socks5://node.invalid:40002".into(),
                username: None,
                password: None,
                enabled: true,
                last_test: None,
                created_at: 0,
            }]),
            socks_nodes_path: Some(path.clone()),
            socks_nodes_writable: false,
            ..mk_service_with_one_credential()
        };

        assert!(svc.delete_socks_node(7).is_err(), "只读降级下删除必须报错");
        assert_eq!(
            svc.list_socks_nodes().len(),
            1,
            "报错后节点必须还在内存表里（否则面板上它消失了而磁盘上还在）"
        );

        assert!(
            svc.record_socks_node_test(
                7,
                SocksNodeTest {
                    ok: true,
                    latency_ms: 12,
                    error: None,
                    tested_at: 0,
                    exit_ip: None,
                }
            )
            .is_err(),
            "只读降级下写测速结果必须报错"
        );
        assert!(
            svc.list_socks_nodes()[0].last_test.is_none(),
            "报错后不得留下一个永不落盘的测速结果"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 缺失文件与不可读文件必须走不同分支：缺失是首次启动（可写），不可读是降级（只读）。
    #[test]
    fn missing_node_file_is_writable_unlike_unreadable_one() {
        let dir = std::env::temp_dir().join(format!(
            "ks_socks_missing_{}_{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tm = {
            let mut c = crate::kiro::model::credentials::KiroCredentials::default();
            c.id = Some(1);
            c.auth_method = Some("api_key".into());
            c.kiro_api_key = Some("ksk_missing".into());
            Arc::new(
                MultiTokenManager::new(
                    crate::model::config::Config::default(),
                    vec![c],
                    None,
                    None,
                    false,
                )
                .expect("token manager"),
            )
        };
        let (nodes, next_id, writable) =
            AdminService::load_socks_nodes_from(&Some(dir.join("socks_nodes.json")), &tm);
        assert!(nodes.is_empty());
        assert_eq!(next_id, 1, "首次启动的 next_id 应为 1");
        assert!(writable, "文件不存在是首次启动，必须允许回写");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⭐ 更新只发 `{id, url, enabled}` 时，用户名与密码都必须保留。
    ///
    /// 回退即 FAIL：把 username 改回无条件 `node.username = username` ——
    /// 用户名被抹成 None 而密码留着，`build_client` 的
    /// `if let (Some(u), Some(p))` 不成立 → 认证被静默丢弃 → 该节点全部连不上，
    /// 而面板上它看起来一切正常（仍显示「已设密码」）。
    #[tokio::test]
    async fn partial_update_preserves_both_username_and_password() {
        let svc = mk_service_with_one_credential();
        let id = svc
            .upsert_socks_node(SocksNodeUpsertRequest {
                id: None,
                name: Some("n".into()),
                url: "socks5://node.invalid:40002".into(),
                username: Some("alice".into()),
                password: Some("secret".into()),
                enabled: None,
            })
            .await
            .expect("新建");

        // 只改 enabled，username/password 两个键都不带。
        svc.upsert_socks_node(SocksNodeUpsertRequest {
            id: Some(id),
            name: None,
            url: "socks5://node.invalid:40002".into(),
            username: None,
            password: None,
            enabled: Some(false),
        })
        .await
        .expect("局部更新");

        let (_, user, pass) = svc.socks_node_proxy(id).expect("节点仍在");
        assert_eq!(
            user.as_deref(),
            Some("alice"),
            "省略 username 键必须保留原值"
        );
        assert_eq!(
            pass.as_deref(),
            Some("secret"),
            "省略 password 键必须保留原值"
        );

        // 显式空串仍必须清空（否则「清除用户名」这个操作不存在）。
        svc.upsert_socks_node(SocksNodeUpsertRequest {
            id: Some(id),
            name: None,
            url: "socks5://node.invalid:40002".into(),
            username: Some(String::new()),
            password: None,
            enabled: None,
        })
        .await
        .expect("清空用户名");
        let (_, user, pass) = svc.socks_node_proxy(id).unwrap();
        assert!(user.is_none(), "显式空串必须清空 username");
        assert_eq!(
            pass.as_deref(),
            Some("secret"),
            "清 username 不该动 password"
        );
    }

    /// ⭐ 源码级守卫：多开必须**消费**节点池，且不得复用节点。
    ///
    /// 用源码断言而非行为测试：`add_credential` 会调 `get_usage_limits_for`
    /// （真实上游往返），穿它的行为测试写不了 —— 本仓既有惯例，见
    /// `provider.rs` 的 `should_emit_usage_record_in_mcp_success_branch`。
    ///
    /// 回退即 FAIL：删掉 copies 循环里那段 `assignable.get(...)` 赋值 ——
    /// 节点池就再次变成一张没人读的表：用户加了节点、建了分身，每份仍然直连、
    /// 共用服务器同一个出口 IP，而面板上看起来一切正常。这正是本批第一版的状态。
    #[test]
    fn clone_creation_must_consume_the_node_pool_without_reuse() {
        let src = include_str!("service.rs");
        // needle 运行时拼接，避免被 include_str! 读到自己而多算一处。
        let consume = format!("{}{}", "assignable.get(seq as usize", " - 2)");
        assert!(
            src.contains(consume.as_str()),
            "多开循环必须按份从节点池取节点，否则节点池无任何消费方"
        );
        // 只取启用节点。
        let enabled_filter = format!("{}{}", ".filter(|n| n.", "enabled)");
        assert!(
            src.contains(enabled_filter.as_str()),
            "只能分配 enabled 的节点，否则「禁用节点」这个开关没有意义"
        );
        // ⭐ 承重：索引式取用（取完即止）而不是取模复用。
        // needle 必须运行时拼接 —— 写成完整字面量时它会出现在 include_str! 读到的
        // 本测试自身里，于是这条**否定**断言恒失败（本文件已两次踩到同一个坑）。
        let reuse = format!("{}{}", "assignable[seq as usize", " % ");
        assert!(
            !src.contains(reuse.as_str()),
            "不得对节点取模复用：两份共用一个出口 IP 等于没分散，却让人以为分散了"
        );
    }

    // ===================== 节点表落盘路径（round-trip）=====================
    //
    // 上面 11 条节点测试全部用 `mk_service_with_one_credential()`，它给
    // `MultiTokenManager::new` 传的 credentials_path 是 `None` → `cache_dir()` 为 None
    // → `socks_nodes_path` 为 None → `persist_socks_nodes` 在开头就 `return Ok(())`。
    // 也就是说**它们一次都没真的写过盘**，于是以下四件事此前零覆盖：
    // 密码的 at-rest 加解密往返、`next_id` 高水位跨存取存活、`SocksNodeFileCompat`
    // 的裸数组兼容分支（生产上唯一引用点是 `load_socks_nodes_from`，测试侧此前为零）、
    // 明文↔密文开关。
    //
    // 下面这组测试建一个 credentials_path 落在**独立临时目录**里的 service，
    // 从而让 `cache_dir()` 派生出真实的 socks_nodes_path（刻意走真实派生链，
    // 而不是直接塞 socks_nodes_path 字段 —— 后者测不到派生本身）。

    /// 造一个节点表真的落在 `dir` 里的 service。`encrypt` 控制 at-rest 开关。
    fn mk_service_rooted_at(dir: &std::path::Path, encrypt: bool) -> AdminService {
        let mut c = crate::kiro::model::credentials::KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("ksk_rt".to_string());
        let mut cfg = crate::model::config::Config::default();
        cfg.encrypt_credentials_at_rest = encrypt;
        let tm = Arc::new(
            MultiTokenManager::new(cfg, vec![c], None, Some(dir.join("credentials.json")), true)
                .expect("构造 token manager"),
        );
        AdminService::new(tm, Vec::<String>::new())
    }

    /// 每条测试独立临时目录（密钥文件 `.at_rest.key` 也落在里面，互不污染）。
    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ks_socks_rt_{tag}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// (a) 明文落盘往返：字段逐个还原，**含密码**。
    ///
    /// 回退即 FAIL：把 `persist_socks_nodes` 里那句 `write_atomic` 删掉（或让它写
    /// `nodes` 而不带 `next_id`，见下一条）—— 重启后节点表整张消失，
    /// 用户配好的一池代理与密码全部丢失，而面板只会显示「暂无节点」。
    ///
    /// ⚠️ 必须 `multi_thread`：`MultiTokenManager::new` 带真实 credentials_path 时会
    /// 回写凭据文件，而 `persist_credentials` 在 runtime 内走 `block_in_place`
    /// （current_thread runtime 上直接 panic）。本组其余落盘测试同理。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nodes_round_trip_plaintext_preserves_every_field() {
        let dir = tmp_dir("plain");
        let svc = mk_service_rooted_at(&dir, false);
        let id = svc
            .upsert_socks_node(SocksNodeUpsertRequest {
                id: None,
                name: Some("JP-1".into()),
                url: "socks5://node.invalid:40002".into(),
                username: Some("alice".into()),
                password: Some("p@ss-w0rd".into()),
                enabled: Some(true),
            })
            .await
            .expect("新建节点");

        let path = dir.join("socks_nodes.json");
        assert!(path.exists(), "cache_dir 派生的节点表必须真的落盘");
        // 关了加密 → 磁盘上是明文（这一条同时锁住「开关真的有效」）。
        let raw = std::fs::read(&path).unwrap();
        assert!(
            !crate::common::secret_store::is_encrypted(&raw),
            "encrypt_credentials_at_rest=false 时不得写成密文"
        );

        // 模拟重启：从同一路径重新加载。
        let svc2 = mk_service_rooted_at(&dir, false);
        let nodes = svc2.list_socks_nodes();
        assert_eq!(nodes.len(), 1, "重启后节点必须还在");
        assert_eq!(nodes[0].id, id);
        assert_eq!(nodes[0].label, "JP-1");
        assert_eq!(nodes[0].url, "socks5://node.invalid:40002");
        assert!(nodes[0].enabled);
        assert!(nodes[0].has_password);
        let (url, user, pass) = svc2.socks_node_proxy(id).expect("节点仍在");
        assert_eq!(url, "socks5://node.invalid:40002");
        assert_eq!(user.as_deref(), Some("alice"), "用户名必须随文件存活");
        assert_eq!(
            pass.as_deref(),
            Some("p@ss-w0rd"),
            "密码必须随文件存活，否则重启后该节点全部连不上"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (b) 加密落盘往返：磁盘字节**不得含密码明文**，但加载后密码完好。
    ///
    /// 回退即 FAIL：把 `persist_socks_nodes` 里的 `encode_for_disk(..., enc, ...)`
    /// 改成 `encode_for_disk(..., false, ...)`（即忽略 at-rest 开关）——
    /// 第 2 条断言失败：代理密码明文躺在磁盘上，而面板的 at-rest 健康灯仍然是绿的。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nodes_round_trip_encrypted_hides_password_on_disk() {
        let dir = tmp_dir("enc");
        let svc = mk_service_rooted_at(&dir, true);
        svc.upsert_socks_node(SocksNodeUpsertRequest {
            id: None,
            name: Some("enc".into()),
            url: "socks5://node.invalid:40002".into(),
            username: Some("bob".into()),
            password: Some("super-secret-pw".into()),
            enabled: None,
        })
        .await
        .expect("新建节点");

        let path = dir.join("socks_nodes.json");
        let raw = std::fs::read(&path).unwrap();
        assert!(
            crate::common::secret_store::is_encrypted(&raw),
            "开了 at-rest 时节点表必须带 KSENC1 magic 前缀"
        );
        let needle = b"super-secret-pw";
        assert!(
            !raw.windows(needle.len()).any(|w| w == needle),
            "磁盘字节里绝不能出现代理密码明文"
        );
        assert!(
            dir.join(".at_rest.key").exists(),
            "首次加密应在同目录创建密钥文件"
        );

        // 重启后必须能解开（同目录密钥在）。
        let svc2 = mk_service_rooted_at(&dir, true);
        let nodes = svc2.list_socks_nodes();
        assert_eq!(nodes.len(), 1, "密文必须能被解开并加载");
        let (_, user, pass) = svc2.socks_node_proxy(nodes[0].id).expect("节点仍在");
        assert_eq!(user.as_deref(), Some("bob"));
        assert_eq!(pass.as_deref(), Some("super-secret-pw"), "解密后密码应完好");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (c) `next_id` 高水位**跨重启**存活：删掉最大 id 的节点后重启，新号仍更大。
    ///
    /// 这是 `SocksNodeFile` 存在的全部理由（见其文档），而此前没有任何测试真的
    /// 存过一次盘 —— 于是"高水位被持久化"这件事从未被验证过。
    ///
    /// 回退即 FAIL：把 `persist_socks_nodes` 里的 `SocksNodeFile { nodes, next_id }`
    /// 换成直接序列化 `nodes` 裸数组（即回到"只存数组"）—— 重启后 next_id 只能按
    /// `max(id)+1` 现算，而最大那个刚被删掉，于是它的 id 被重新发出去：
    /// 面板另一个标签页仍持有删除前的列表，点它的「测活」会打到这个无关的新节点上。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_id_high_water_mark_survives_save_and_load() {
        let dir = tmp_dir("hwm");
        let svc = mk_service_rooted_at(&dir, false);
        let mk = |n: u16| SocksNodeUpsertRequest {
            id: None,
            name: Some(format!("n{n}")),
            url: format!("socks5://node{n}.invalid:40002"),
            username: None,
            password: None,
            enabled: None,
        };
        let a = svc.upsert_socks_node(mk(1)).await.unwrap();
        let b = svc.upsert_socks_node(mk(2)).await.unwrap();
        let c = svc.upsert_socks_node(mk(3)).await.unwrap();
        assert!(c > b && b > a);

        // 删掉**最大** id 那个（这正是"只存数组"会翻车的场景）。
        assert!(svc.delete_socks_node(c).unwrap());

        // 重启（从磁盘重新加载）后再建一个。
        let svc2 = mk_service_rooted_at(&dir, false);
        assert_eq!(svc2.list_socks_nodes().len(), 2, "剩下两个节点应被加载回来");
        let d = svc2.upsert_socks_node(mk(4)).await.unwrap();
        assert!(
            d > c,
            "重启后新节点 id（{d}）必须大于历史上发放过的任何 id（已发过 {c}）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (d) `SocksNodeFileCompat` 的**裸数组**（旧形态）分支：能加载且高水位补齐。
    ///
    /// 该枚举在生产上唯一的引用点是 `load_socks_nodes_from`，测试侧此前为零 ——
    /// 也就是说"旧文件还能不能读"这条兼容承诺从未被验证。
    ///
    /// 回退即 FAIL：删掉 `SocksNodeFileCompat` 的 `BareArray` 变体（只留结构体形态），
    /// 裸数组解析失败 → `load_socks_nodes_from` 走**只读降级**：用户升级后节点表在面板上
    /// 整张消失，且此后任何修改都被拒（"只读降级"），而文件其实是好的。
    #[test]
    fn legacy_bare_array_node_file_loads_and_backfills_next_id() {
        let dir = tmp_dir("compat");
        let path = dir.join("socks_nodes.json");
        // 旧形态：**裸数组**，没有 nextId 这一层。
        std::fs::write(
            &path,
            r#"[{"id":5,"name":"old","url":"socks5://legacy.invalid:1080","enabled":true}]"#,
        )
        .unwrap();

        let svc = mk_service_rooted_at(&dir, false);
        let nodes = svc.list_socks_nodes();
        assert_eq!(nodes.len(), 1, "裸数组旧文件必须能读出来（不得降级成空表）");
        assert_eq!(nodes[0].id, 5);
        assert_eq!(nodes[0].label, "old");
        assert!(nodes[0].enabled, "缺 enabled 字段时应默认 true");

        // 高水位按 max(id)+1 补齐 → 新节点 id 必须 > 5（而不是又发 1）。
        let new_id = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                svc.upsert_socks_node(SocksNodeUpsertRequest {
                    id: None,
                    name: Some("fresh".into()),
                    url: "socks5://fresh.invalid:1080".into(),
                    username: None,
                    password: None,
                    enabled: None,
                })
                .await
            })
            .expect("旧文件之上新建节点");
        assert!(
            new_id > 5,
            "裸数组归一化后 next_id 应至少是 max(id)+1，实得 {new_id}"
        );

        // 回写后应升级成新形态（带 nextId），且旧节点仍在。
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("\"nextId\""),
            "回写应升级为带高水位的新形态: {raw}"
        );
        let svc2 = mk_service_rooted_at(&dir, false);
        assert_eq!(svc2.list_socks_nodes().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 明文 ↔ 密文开关：同一份数据先明文落盘，改开关后回写即变密文（反之亦然）。
    ///
    /// 这是"透明迁移"承诺的两个方向。回退即 FAIL：`load_socks_nodes_from` 里若去掉
    /// `maybe_decrypt_to_string` 而直接当明文 parse，第二段（密文 → 加载）会解析失败
    /// 进只读降级 —— 开了加密的用户重启后节点表整张消失且无法修改。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn at_rest_toggle_migrates_both_directions() {
        let dir = tmp_dir("toggle");
        // 先明文写一份。
        let svc_plain = mk_service_rooted_at(&dir, false);
        svc_plain
            .upsert_socks_node(SocksNodeUpsertRequest {
                id: None,
                name: Some("mig".into()),
                url: "socks5://node.invalid:40002".into(),
                username: None,
                password: Some("pw-1".into()),
                enabled: None,
            })
            .await
            .unwrap();
        let path = dir.join("socks_nodes.json");
        assert!(!crate::common::secret_store::is_encrypted(
            &std::fs::read(&path).unwrap()
        ));

        // 打开加密后重启：明文照旧能读（透明迁移），下一次回写才变密文。
        let svc_enc = mk_service_rooted_at(&dir, true);
        assert_eq!(
            svc_enc.list_socks_nodes().len(),
            1,
            "明文文件在开了加密后仍必须能读"
        );
        svc_enc
            .upsert_socks_node(SocksNodeUpsertRequest {
                id: None,
                name: Some("mig2".into()),
                url: "socks5://node2.invalid:40002".into(),
                username: None,
                password: Some("pw-2".into()),
                enabled: None,
            })
            .await
            .unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert!(
            crate::common::secret_store::is_encrypted(&raw),
            "开了加密后的第一次回写应产出密文"
        );
        for needle in [b"pw-1".as_slice(), b"pw-2".as_slice()] {
            assert!(
                !raw.windows(needle.len()).any(|w| w == needle),
                "迁移后旧密码也不得残留明文"
            );
        }

        // 再关掉加密：密文仍能读（走解密），回写后落回明文。
        let svc_back = mk_service_rooted_at(&dir, false);
        assert_eq!(
            svc_back.list_socks_nodes().len(),
            2,
            "密文在关了加密后仍必须能读"
        );
        svc_back.delete_socks_node(1).ok();
        let raw2 = std::fs::read(&path).unwrap();
        assert!(
            !crate::common::secret_store::is_encrypted(&raw2),
            "关掉加密后的回写应落回明文"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⭐ 源码级守卫：组内序号必须在**任何 await 之前**一次性预留完，
    /// 且 `add_credential` 里不得再出现「扫 max 现算」。
    ///
    /// 用源码断言而非行为测试：`add_credential` 会调 `get_usage_limits_for`
    /// （真实上游往返），穿它的行为测试写不了 —— 与本文件既有的
    /// `clone_creation_must_consume_the_node_pool_without_reuse` 同款理由。
    /// 并发正确性本身由 `token_manager` 的
    /// `concurrent_clone_seq_reservations_never_overlap` 覆盖。
    ///
    /// 回退即 FAIL：把序号来源改回 `max_clone_seq_in_group`（无论放在入池前还是入池后）
    /// —— 第 1 条断言立刻失败。两个并发的「给同一 key 加 N 份」请求会各自读到同一个
    /// max，同一组里出现两个 `分身 #2`，管理页无法区分、删除时无法指名。
    #[test]
    fn clone_seq_must_be_reserved_before_any_await() {
        let src = include_str!("service.rs");
        // ⚠️ 两个 needle 都**运行时拼接**：写成完整字面量时它会出现在 include_str!
        // 读到的本测试自身里 —— 否定断言恒失败、肯定断言恒成立（即测试被静默作废）。
        // 本文件已两次踩到这个坑，见节点池那条守卫的注释。
        let scan = format!("{}{}", "max_clone_seq_in", "_group(");
        assert!(
            !src.contains(scan.as_str()),
            "add_credential 不得自行扫 max 现算组内序号：发号与入池之间横跨 await，\
             两个并发请求会读到同一个 max 而重号。序号必须走 token_manager 的原子预留。"
        );
        let reserve = format!("{}{}", "reserve_clone", "_seqs(g, copies)");
        assert!(
            src.contains(reserve.as_str()),
            "必须一次性预留本次全部份数的号段（copies 份），否则第 2..N 份仍会与并发请求撞号"
        );

        // ⭐ 承重的**顺序**断言：预留必须在第一个入池 await 之前。
        // 预留放到入池之后就等于把竞态窗口原样留着（旧代码正是那样）。
        let reserve_at = src.find(reserve.as_str()).expect("上一条断言已保证存在");
        let first_await = format!(
            "{}{}",
            "add_credential_allowing_", "duplicate(new_cred.clone())"
        );
        let await_at = src
            .find(first_await.as_str())
            .expect("第 1 份入池调用应存在");
        assert!(
            reserve_at < await_at,
            "号段预留（位置 {reserve_at}）必须早于第 1 份入池 await（位置 {await_at}）：\
             放在 await 之后等于竞态窗口原封不动"
        );
    }

    /// ⭐ 源码级守卫：`clone_credential` **不得重新实现份数逻辑**，必须复用共享实现。
    ///
    /// 用源码断言而非行为测试：这条路同样会调 `get_usage_limits_for`（真实上游往返）。
    ///
    /// 回退即 FAIL：在 `clone_credential` 里自己抄一遍 copies 循环（哪怕只抄
    /// `add_credential_allowing_duplicate` 那一句）—— 第 2 条断言失败。那会造出第二条
    /// 校验路径：去重绕过、组复用、**序号原子预留**、节点分配、OAuth 拒绝五件事
    /// 各有两份实现，其中任一份漏改就是一个只在某条入口上出现的缺陷。
    #[test]
    fn clone_endpoint_must_reuse_the_shared_copies_path() {
        // ⚠️ 本守卫读源码，必须做**两步归一**，否则它是纸面测试（CLAUDE.md 记载的必备两步）：
        //   ① 剔掉 `//` 开头的行 —— 否则匹配到被注释掉的实现或文档注释里的符号名，
        //      实现被删了守卫仍绿；
        //   ② **去掉全部空白** —— 否则 rustfmt 一次换行就让 needle 失配。
        //
        // 🔴 第 ② 步是 2026-08-06 实测补上的，代价是一次真实红灯：有人给
        // `AddCredentialRequest` 加了字段使调用行变长，rustfmt 于是把
        //     let mut created = self.add_credential_with_intent(
        // 折成
        //     let mut created = self
        //         .add_credential_with_intent(
        // 于是含 `self.` 的 needle 计数从 2 掉到 1、守卫报红，而**代码完全正确**。
        // 当时最省事的"修法"是把断言里的 2 改成 1 —— 那会把守卫彻底作废（它防的是
        // 去重绕过/组复用/序号原子预留/节点分配/OAuth 拒绝五件事各有两份实现）。
        // 归一化之后断言与排版无关，这类假红灯不会再来。
        let raw = include_str!("service.rs");
        let src = normalize_src_for_guard(raw);

        // needle 全部运行时拼接（见节点池守卫处的说明：字面量会匹配到本测试自身，
        // 从而把断言静默作废 —— 本文件已三次踩到这个坑）。
        // ⚠️ 这里的 count 断言尤其要小心：needle 若在本测试源码里出现，它会把自己算进
        // 计数，于是"两处调用都被删掉"仍然满足 `>= 2`。
        // 归一化后本测试自身的拼接式 `format!("{}{}", "self.add_credential_with", ...)`
        // 仍是分开的两段字符串字面量，**不会**自匹配 —— 这是拼接写法在归一化下依然承重的原因。
        let shared = format!("{}{}", "self.add_credential_with", "_intent(");
        assert_eq!(
            src.matches(shared.as_str()).count(),
            2,
            "add_credential 与 clone_credential 必须**都且只**走同一个共享实现\
             （断言已对空白归一，报红说明真的少了一处调用，不是排版问题）"
        );

        // ⭐ 承重：clone_credential 的函数体里不得出现入池调用。
        // ⚠️ needle 按**无空白形状**写（`pub async fn` → `pubasyncfn`），因为 src 已归一化。
        // 原来的带空格写法在归一化后恒不命中 ⇒ `expect` 直接 panic，守卫变成"总是报错"。
        let body_start = src
            .find(format!("{}{}", "pubasyncfnclone_", "credential(").as_str())
            .expect("clone_credential 应存在");
        let body_end = src[body_start..]
            .find(format!("{}{}", "asyncfnadd_credential_with_", "intent(").as_str())
            .map(|off| body_start + off)
            .expect("clone_credential 之后应紧跟共享实现");
        let body = &src[body_start..body_end];
        let insert = format!("{}{}", "add_credential_allowing_", "duplicate");
        assert!(
            !body.contains(insert.as_str()),
            "clone_credential 不得自己入池：份数/去重/序号/节点分配必须只有一份实现"
        );
        let reserve = format!("{}{}", "reserve_clone", "_seqs");
        assert!(
            !body.contains(reserve.as_str()),
            "clone_credential 不得自己预留序号（那会与共享实现各发一段号，重号回归）"
        );

        // 显式意图必须传 true，否则 `copies == 1` 会走去重 → 对已在池中的 key 必然
        // 撞 `凭据已存在`，而「再加 1 份」正是本端点最常见的用法。
        //
        // 🔴 本断言此前的 needle 是 `"            true,\n" + "        )\n        .await"`
        // —— 把**缩进宽度与换行位置**都写进了判据。实测它已经失配到 0 命中（rustfmt
        // 把这个调用收成了一行），只是 `assert_eq!` 在它之前先 panic，所以这条**一直没被
        // 执行过**，没人发现它坏了。这正是"守卫自己烂掉而无人知"的形态：
        // 它比没有守卫更糟，因为它让人以为这件事被钉住了。
        // 归一化后按「无空白形状」写：`...},true).await`。
        let forced = format!("{}{}", "..req},", "true).await");
        assert!(
            src.contains(forced.as_str()),
            "clone_credential 必须以 force_multi_open=true 调共享实现\
             （否则 copies==1 会走去重，对已在池中的 key 必然撞『凭据已存在』）"
        );
    }

    /// 源码级守卫专用的归一化：**剔注释行 + 去全部空白**。
    ///
    /// 这两步是 `CLAUDE.md` 记载的「写源码守卫的必备两步」，缺任一步守卫就是纸面测试：
    ///
    /// - **不剔注释** ⇒ `include_str!` 读到的是含注释的原始文本，把实现整段注释掉后
    ///   `contains` 仍匹配到注释里那行 ⇒ 实现没了守卫还绿。本文件已三次踩到。
    /// - **不去空白** ⇒ rustfmt 把一句调用折成多行就让 needle 失配 ⇒ 代码完全正确却报红。
    ///   2026-08-06 实测发生过：加了个字段使行变长 → rustfmt 换行 → 守卫假红，
    ///   而当时最省事的"修法"是改断言期望值，那等于把守卫作废。
    ///
    /// 去空白而非「归一成单空格」是刻意的：单空格仍然区分 `self .foo(` 与 `self.foo(`，
    /// 而这两者语义完全相同、只差 rustfmt 的一次决定。全去掉才真正与排版无关。
    ///
    /// ⚠️ 代价：needle 也必须写成无空白形状。跨 token 的 needle（如 `fn foo (`）会失配，
    /// 写 needle 时按「删掉所有空格后的样子」写。
    fn normalize_src_for_guard(raw: &str) -> String {
        raw.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .split_whitespace()
            .collect::<String>()
    }

    /// ⭐ 源码级守卫：新端点必须**真的注册进路由**。
    ///
    /// 回退即 FAIL：删掉 `router.rs` 里那行 `.route(...clone...)` —— service 层代码
    /// 还在、编译还过、测试还绿，但前端拿到 404。本仓已有多个"实现了却没挂路由"
    /// 的同类风险点，故把注册这件事钉死。
    #[test]
    fn clone_endpoint_is_registered_in_router() {
        let router = include_str!("router.rs");
        let path = format!("{}{}", "/credentials/{id}", "/clone");
        assert!(
            router.contains(path.as_str()),
            "clone 端点必须注册在 admin 路由树上"
        );
        let handler = format!("{}{}", "post(clone_", "credential)");
        assert!(
            router.contains(handler.as_str()),
            "clone 路由必须绑到 clone_credential 处理器（且是 POST）"
        );
    }

    /// 不存在的 id 必须 404，且**不得**建出任何凭据。
    ///
    /// 这条是 `clone_credential` 唯一不打网络就能穿到底的分支（NotFound 在
    /// `export_credential` 之后立即返回），故可以写真行为测试。
    #[tokio::test]
    async fn cloning_unknown_credential_is_not_found() {
        let svc = mk_service_with_one_credential();
        let before = svc.token_manager.total_count();
        let err = svc
            .clone_credential(9999, 2, None, None, None, None, None)
            .await
            .expect_err("不存在的 id 应报错");
        assert!(
            matches!(err, AdminServiceError::NotFound { id: 9999 }),
            "应是 NotFound，实际 {err:?}"
        );
        assert_eq!(svc.token_manager.total_count(), before, "不得建出任何凭据");
    }

    /// OAuth 号加分身必须被拒，且报错要点名是哪个 id。
    ///
    /// 回退即 FAIL：删掉 `clone_credential` 里那道 `multi_open_rejection_reason` ——
    /// 请求会继续走下去并真的建出 N 份带同一个 refreshToken 的分身，
    /// 它们随后被 `invalid_grant` 逐个自动禁用（面板上显示成"号被封了"）。
    #[tokio::test]
    async fn cloning_oauth_credential_is_rejected_with_id() {
        let mut c = crate::kiro::model::credentials::KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("social".to_string());
        c.refresh_token = Some("rt-oauth".to_string());
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![c],
                None,
                None,
                false,
            )
            .expect("token manager"),
        );
        let svc = AdminService::new(tm, Vec::<String>::new());

        let err = svc
            .clone_credential(1, 3, None, None, None, None, None)
            .await
            .expect_err("OAuth 号不该能加分身");
        let msg = match err {
            AdminServiceError::InvalidCredential(m) => m,
            other => panic!("应是 InvalidCredential，实际 {other:?}"),
        };
        assert!(msg.contains("#1"), "报错应点名 id，实际: {msg}");
        assert!(
            msg.contains("refreshToken"),
            "报错应说明 refreshToken 轮换这个根因，实际: {msg}"
        );
        assert_eq!(svc.token_manager.total_count(), 1, "被拒时不得建出任何份");
    }

    // ============ 分身默认不启用（clone_credential 的 enabled 语义）============
    //
    // 这三条是**真行为**测试，不是源码守卫：断言的是「分身入池后在面板上是 disabled」，
    // 也就是 `get_all_credentials()`（`/credentials/status` 的实现）看到的那个字段。
    //
    // 之所以能穿到底而不打真实上游：`mk_clone_service` 给 token manager 配了一个
    // **必然连不上的本地代理**（`127.0.0.1:1`），于是共享实现里那一次
    // `get_usage_limits_for` 立刻拿 connection refused 并被 `tracing::warn!` 吞掉
    // （它本就是"失败不影响上号"的路径）。同时父号预置了 `region`，共享实现按 key 继承给
    // 分身，于是 `probe_and_persist_api_region` 在廉价预判处就 return —— 全程零 DNS。

    /// 造一个「加分身能穿到底且不出网」的 service：父号是 api_key + 预置 region，
    /// 全局代理指向必然拒连的 127.0.0.1:1。
    fn mk_clone_service() -> AdminService {
        let mut c = crate::kiro::model::credentials::KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("ksk_clone_enabled_test".to_string());
        // 预置 region → 共享实现继承给每份 → region 探测在预判处返回，不出网。
        c.region = Some("us-east-1".to_string());
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![c],
                // 拒连代理：让唯一那次上游往返立刻失败，测试与网络环境无关。
                Some(crate::http_client::ProxyConfig::new("http://127.0.0.1:1")),
                None,
                false,
            )
            .expect("token manager"),
        );
        AdminService::new(tm, Vec::<String>::new())
    }

    /// 面板视角下「除父号 #1 之外的每一份」的 disabled 状态。
    fn clone_disabled_flags(svc: &AdminService) -> Vec<(u64, bool)> {
        let mut v: Vec<(u64, bool)> = svc
            .get_all_credentials()
            .credentials
            .into_iter()
            .filter(|c| c.id != 1)
            .map(|c| (c.id, c.disabled))
            .collect();
        v.sort_by_key(|(id, _)| *id);
        v
    }

    /// ⭐ `enabled` 省略 → **每一份**分身入池即禁用，父号状态不变。
    ///
    /// 回退即 FAIL：把 `clone_credential` 里那句 `disabled: !enabled.unwrap_or(false)`
    /// 改回旧行为（删掉该字段 / 写 `disabled: false`）—— 本条的 `all disabled` 断言变红。
    ///
    /// 为什么必须是"入池时就 disabled"而不是"建完再批量禁用"：后者有中间窗口，
    /// 分身在那段时间里是启用的，调度器立刻往它们发流量。实测事故
    /// （2026-08-05 02:42）一次 copies=5，4 个分身 region 错配 → 恒 403 →
    /// **24 秒内全部被自动禁用、0% 成功**，那 24 秒的真实用户请求全打在必废的号上。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cloned_credentials_are_disabled_by_default() {
        let svc = mk_clone_service();
        let resp = svc
            .clone_credential(1, 3, None, None, None, None, None)
            .await
            .expect("加分身应成功");

        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        assert_eq!(ids.len(), 3, "copies=3 应建出 3 份，实际 {ids:?}");
        assert_eq!(svc.token_manager.total_count(), 4, "父号 + 3 份分身");

        let flags = clone_disabled_flags(&svc);
        assert_eq!(flags.len(), 3, "父号之外应恰好 3 份，实际 {flags:?}");
        assert!(
            flags.iter().all(|(_, disabled)| *disabled),
            "省略 enabled 时每一份分身都必须是禁用态，实际 {flags:?}"
        );

        // 父号本身绝不能被顺手改状态。
        let parent = svc
            .get_all_credentials()
            .credentials
            .into_iter()
            .find(|c| c.id == 1)
            .expect("父号必须还在");
        assert!(!parent.disabled, "父号的启用状态不该被加分身影响");

        // available 只数未禁用的 → 仍然只有父号一个可用。
        assert_eq!(
            svc.get_all_credentials().available,
            1,
            "禁用的分身不得计入可用数（否则面板容量与调度池对不上）"
        );
    }

    /// `enabled: true` → 分身建出来就是启用的（这个开关必须真的双向可控）。
    ///
    /// 回退即 FAIL：把那句改成硬编码 `disabled: true` —— 本条变红。
    /// 有这一条，上一条才不可能靠"永远禁用"蒙过去。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cloned_credentials_can_be_created_enabled_on_request() {
        let svc = mk_clone_service();
        svc.clone_credential(1, 2, Some(true), None, None, None, None)
            .await
            .expect("加分身应成功");

        let flags = clone_disabled_flags(&svc);
        assert_eq!(flags.len(), 2, "copies=2 应建出 2 份，实际 {flags:?}");
        assert!(
            flags.iter().all(|(_, disabled)| !*disabled),
            "显式 enabled=true 时分身必须是启用态，实际 {flags:?}"
        );
        assert_eq!(svc.get_all_credentials().available, 3, "父号 + 2 份都可用");
    }

    /// `enabled: false` 显式给出时与省略同义（前端可能两种都发）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_enabled_false_matches_the_omitted_default() {
        let svc = mk_clone_service();
        svc.clone_credential(1, 2, Some(false), None, None, None, None)
            .await
            .expect("加分身应成功");
        let flags = clone_disabled_flags(&svc);
        assert_eq!(flags.len(), 2);
        assert!(
            flags.iter().all(|(_, disabled)| *disabled),
            "显式 false 必须与省略同义，实际 {flags:?}"
        );
    }

    /// `enabled` 的 JSON 契约：省略 → `None`（由 service 落到"禁用"），
    /// 显式 `true` / `false` 各自原样解出。
    ///
    /// 回退即 FAIL：给该字段加上 `#[serde(default = "...")]` 之类把 None 提前吃掉的
    /// 默认值 —— 第一条断言变红（service 层就再也分不清"没给"与"给了 false"）。
    #[test]
    fn clone_request_parses_enabled_as_optional_camel_case() {
        use super::super::types::CloneCredentialRequest;

        let omitted: CloneCredentialRequest =
            serde_json::from_str(r#"{"copies":3}"#).expect("省略 enabled 应能解析");
        assert_eq!(omitted.copies, Some(3));
        assert_eq!(omitted.enabled, None, "省略时必须是 None，不能被吃成 false");

        let on: CloneCredentialRequest =
            serde_json::from_str(r#"{"copies":2,"enabled":true}"#).expect("解析 enabled=true");
        assert_eq!(on.enabled, Some(true));

        let off: CloneCredentialRequest =
            serde_json::from_str(r#"{"copies":2,"enabled":false}"#).expect("解析 enabled=false");
        assert_eq!(off.enabled, Some(false));
    }

    // ============ 节点池 → 各份的分配（含主份）============
    //
    // 这一组是**真行为**测试而不是源码守卫：断言的是「入池后每一份的 proxyUrl 到底是什么」，
    // 也就是 `export_credential` 看到的那个字段。能穿到底不出网的理由与上面 `enabled`
    // 那三条相同（`mk_clone_service` 的拒连代理 + 预置 region）。
    //
    // ⚠️ 节点 URL 一律用 RFC 6761 保留的 `.invalid` TLD（与既有节点测试同款）：
    // `upsert_socks_node` 会对节点 URL 做 SSRF 校验，`127.0.0.1` 会被**正确地**拒绝
    // （`目标解析到非公网地址 127.0.0.1`），所以环回地址在这条路上根本进不了池。
    // `.invalid` 保证永不解析 → 走 DNS 失败的 fail-open 分支入池，而随后那一次
    // `get_usage_limits_for` 也在 DNS 处即失败，测试与本机 DNS/代理环境无关
    // （见 CLAUDE.md 已知问题 #19 的同款理由）。

    /// 节点 i（0-based）的 URL。逐个不同，断言才能区分是哪个节点。
    fn node_url(i: usize) -> String {
        format!("socks5://node{}.invalid:{}", i + 1, 40001 + i)
    }

    /// 往池里塞 n 个启用节点，返回它们的 id（顺序 = 插入顺序）。
    async fn seed_nodes(svc: &AdminService, n: usize) -> Vec<u64> {
        let mut ids = Vec::new();
        for i in 0..n {
            let id = svc
                .upsert_socks_node(SocksNodeUpsertRequest {
                    id: None,
                    name: Some(format!("n{i}")),
                    url: node_url(i),
                    username: None,
                    password: None,
                    enabled: Some(true),
                })
                .await
                .expect("加节点应成功");
            ids.push(id);
        }
        ids
    }

    /// 逐 id 取「这一份的 proxyUrl」。
    ///
    /// 走 `token_manager.export_credential`（原始值）而不是 `AdminService::export_credential`
    /// —— 后者是给导出用的、会做脱敏，断言出口 URL 必须看原始值。
    fn proxy_urls_by_id(svc: &AdminService, ids: &[u64]) -> Vec<Option<String>> {
        ids.iter()
            .map(|id| {
                svc.token_manager
                    .export_credential(*id)
                    .unwrap_or_else(|| panic!("凭据 #{id} 应存在"))
                    .proxy_url
            })
            .collect()
    }

    /// 🔴 承重（缺陷 A）：**主份也要拿节点**，只要它自己没有代理。
    ///
    /// 实测的旧行为：池里 5 个全启用、一次 `copies=4`，只有第 2/3/4 份拿到节点，
    /// **主份裸连**，两个节点闲置 —— 而用户以为 4 份都分散了。
    ///
    /// 回退即 FAILED：把节点计划挪回 `copies > 1` 块内（即第 1 份入池之后再算），
    /// 或把 `pool_may_assign` 的判据改回「是不是第 1 份」—— 第一条断言变红
    /// （`urls[0]` 是 None）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn first_copy_must_get_a_node_when_it_has_no_proxy_of_its_own() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 3).await;

        let resp = svc
            .clone_credential(1, 3, None, None, None, None, None)
            .await
            .expect("加分身应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        assert_eq!(ids.len(), 3, "copies=3 应建出 3 份，实际 {ids:?}");

        let urls = proxy_urls_by_id(&svc, &ids);
        // ⭐ 这一条是整个缺陷 A：修复前它恒为 None。
        assert!(
            urls[0].is_some(),
            "主份必须也从节点池拿到出口（它是全新条目、本来没代理），实际 {urls:?}"
        );
        // 三份三节点 → 每份都有，且**互不相同**（不复用）。
        assert!(
            urls.iter().all(|u| u.is_some()),
            "3 个启用节点 / 3 份应全部分到，实际 {urls:?}"
        );
        let mut distinct: Vec<&str> = urls.iter().map(|u| u.as_deref().unwrap()).collect();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            3,
            "各份出口必须互不相同（复用等于没分散），实际 {urls:?}"
        );

        // 文案要如实：3 份全额分配 → 不得出现"直连"字样。
        assert!(
            resp.message.contains("已从节点池为 3 份分配独立出口 IP"),
            "文案应如实报 3 份，实际: {}",
            resp.message
        );
        assert!(
            !resp.message.contains("直连"),
            "全额分配时不得声称有份直连，实际: {}",
            resp.message
        );
    }

    /// 🔴 承重（缺陷 A 的另一半 / 零回归）：主份**已有代理**时绝不覆盖。
    ///
    /// 这是原注释真正要保护的东西（"覆盖会把一个在跑的号的出口换掉"），
    /// 修复后必须仍然成立。走 `add_credential_with_intent` 而不是 `clone_credential`：
    /// 后者刻意把 proxy_* 留空，构造不出"调用方已显式指定代理"这个场景。
    ///
    /// 回退即 FAILED：把 `pool_may_assign` 恒设为 true（即不再看这一份有没有代理）
    /// —— 第一条断言变红（主份的出口被池节点顶掉）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_proxy_must_never_be_overwritten_by_the_pool() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 3).await;

        let resp = svc
            .add_credential_with_intent(
                AddCredentialRequest {
                    auth_method: "api_key".into(),
                    kiro_api_key: Some("ksk_clone_enabled_test".into()),
                    copies: Some(2),
                    // 调用方的明确意图：这一批就要走这个出口。
                    proxy_url: Some("socks5://127.0.0.1:9".into()),
                    disabled: true,
                    ..Default::default()
                },
                false,
            )
            .await
            .expect("多开应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        let urls = proxy_urls_by_id(&svc, &ids);
        assert!(
            urls.iter()
                .all(|u| u.as_deref() == Some("socks5://127.0.0.1:9")),
            "显式给了 proxy_url 时池分配必须完全不介入（每份都保持调用方给的那个），实际 {urls:?}"
        );
        assert!(
            resp.message.contains("未从节点池分配代理"),
            "文案应说明本次没走池分配，实际: {}",
            resp.message
        );
    }

    /// ⭐ 承重（缺陷 B）：`nodeIds` 给了就**按顺序**分给各份，池里其余节点一律不用。
    ///
    /// 回退即 FAILED：让 `resolve_node_plan` 忽略 `node_ids`（恒走"池里全部启用节点"
    /// 那一支）—— 各份会拿到 #1/#2/#3 也就是端口 1/2/3，而本条要求的是端口 3/1
    /// （用户挑的那两个，且顺序是他给的顺序）→ 断言变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_node_ids_are_assigned_in_the_given_order() {
        let svc = mk_clone_service();
        let nodes = seed_nodes(&svc, 3).await;

        let resp = svc
            // 刻意**倒序**且只挑两个：既验证"按给定顺序"，也验证"没挑的节点不会被顶上来"。
            .clone_credential(1, 2, None, Some(vec![nodes[2], nodes[0]]), None, None, None)
            .await
            .expect("加分身应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        let urls = proxy_urls_by_id(&svc, &ids);
        assert_eq!(
            urls,
            vec![Some(node_url(2)), Some(node_url(0))],
            "必须严格按 nodeIds 的顺序分（第 1 个给主份），实际 {urls:?}"
        );
        // 没被挑中的第 2 个节点绝不能出现。
        assert!(
            !urls
                .iter()
                .any(|u| u.as_deref() == Some(node_url(1).as_str())),
            "未被指定的节点不得被用上，实际 {urls:?}"
        );
        assert!(
            resp.message.contains("已从节点池为 2 份分配独立出口 IP"),
            "文案应如实报 2 份，实际: {}",
            resp.message
        );
    }

    /// ⭐ 承重（缺陷 B + C）：不存在 / 已禁用的 node id **跳过并点名**，绝不静默替换。
    ///
    /// 这是需求 C 的核心：「我选了节点却仍然直连」是最容易踩空的一步，
    /// 而**静默换一个节点**更糟 —— 用户以为出口是他挑的那个。
    ///
    /// 回退即 FAILED：
    /// - 让 `resolve_node_plan` 把无效 id 静默替换成池里下一个可用节点 →
    ///   第 2 份会拿到端口 2 而不是直连 → 第二条断言变红；
    /// - 或者把 `rejected` 从文案里删掉 → 后两条断言变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_node_ids_are_skipped_and_named_in_the_message() {
        let svc = mk_clone_service();
        let nodes = seed_nodes(&svc, 2).await;
        // 把第 2 个关掉：显式指定也不该用它（否则「禁用」这个开关在这条路上形同不存在）。
        svc.upsert_socks_node(SocksNodeUpsertRequest {
            id: Some(nodes[1]),
            name: None,
            url: node_url(1),
            username: None,
            password: None,
            enabled: Some(false),
        })
        .await
        .expect("禁用节点应成功");

        let missing = 9999u64;
        let resp = svc
            .clone_credential(
                1,
                2,
                None,
                Some(vec![nodes[0], nodes[1], missing]),
                None,
                None,
                None,
            )
            .await
            .expect("加分身应成功（无效 id 不该让整个请求失败）");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        let urls = proxy_urls_by_id(&svc, &ids);

        assert_eq!(
            urls[0].as_deref(),
            Some(node_url(0).as_str()),
            "有效的那个必须生效，实际 {urls:?}"
        );
        // ⭐ 承重：第 2 份**直连**，而不是被悄悄塞上别的节点。
        assert!(
            urls[1].is_none(),
            "无效 id 必须跳过、该份直连；静默替换会让用户以为出口是他选的那个。实际 {urls:?}"
        );

        // ⭐ 需求 C：两个无效 id 都要在文案里点名，且写清各自原因。
        let msg = &resp.message;
        assert!(
            msg.contains(&format!("#{}（已禁用）", nodes[1])),
            "被禁用的节点必须点名且注明原因，实际: {msg}"
        );
        assert!(
            msg.contains(&format!("#{missing}（不存在）")),
            "不存在的节点必须点名且注明原因，实际: {msg}"
        );
        assert!(
            msg.contains("已从节点池为 1 份分配独立出口 IP")
                && msg.contains("另有 1 份因启用节点不足而直连"),
            "文案必须同时报「分了几份」与「几份直连」，实际: {msg}"
        );
    }

    /// 重复的 node id 记作 `重复` 并只用一次（两份共用一个出口就是"复用"，
    /// 而复用等于没分散 —— 调用方显式写两遍也不例外，只是这次要说出来）。
    ///
    /// 回退即 FAILED：去掉 `resolve_node_plan` 里的查重 —— 两份都拿到端口 1，
    /// 第二条断言变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_node_ids_are_used_once_and_reported() {
        let svc = mk_clone_service();
        let nodes = seed_nodes(&svc, 2).await;

        let resp = svc
            .clone_credential(1, 2, None, Some(vec![nodes[0], nodes[0]]), None, None, None)
            .await
            .expect("加分身应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        let urls = proxy_urls_by_id(&svc, &ids);
        assert_eq!(
            urls[0].as_deref(),
            Some(node_url(0).as_str()),
            "第一次出现的应生效，实际 {urls:?}"
        );
        assert!(
            urls[1].is_none(),
            "同一个节点不得被两份共用（那等于没分散），实际 {urls:?}"
        );
        assert!(
            resp.message.contains(&format!("#{}（重复）", nodes[0])),
            "重复的 id 必须点名，实际: {}",
            resp.message
        );
    }

    /// 启用节点少于份数时：够的份分到，其余**直连**（刻意不轮询复用），文案如实。
    ///
    /// 回退即 FAILED：把取用改成取模复用（`% assignable.len()`）—— 第二条
    /// "互不相同"的断言变红。这条同时是那道源码守卫
    /// `clone_creation_must_consume_the_node_pool_without_reuse` 的行为侧对照。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fewer_nodes_than_copies_leaves_the_rest_direct_without_reuse() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 2).await;

        let resp = svc
            .clone_credential(1, 4, None, None, None, None, None)
            .await
            .expect("加分身应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        assert_eq!(ids.len(), 4, "copies=4 应建出 4 份，实际 {ids:?}");
        let urls = proxy_urls_by_id(&svc, &ids);

        let with_proxy: Vec<&str> = urls.iter().filter_map(|u| u.as_deref()).collect();
        assert_eq!(
            with_proxy.len(),
            2,
            "只有 2 个节点 → 只能有 2 份带出口，实际 {urls:?}"
        );
        let mut d = with_proxy.clone();
        d.sort_unstable();
        d.dedup();
        assert_eq!(
            d.len(),
            2,
            "带出口的两份必须用不同节点（不复用），实际 {urls:?}"
        );
        // 前两份拿到、后两份直连（顺序是承重的：份序与节点序一一对应）。
        assert!(
            urls[0].is_some() && urls[1].is_some() && urls[2].is_none() && urls[3].is_none(),
            "应按份序分配、不够的份直连，实际 {urls:?}"
        );
        assert!(
            resp.message.contains("已从节点池为 2 份分配独立出口 IP")
                && resp.message.contains("另有 2 份因启用节点不足而直连"),
            "文案必须如实报 2 分配 / 2 直连，实际: {}",
            resp.message
        );
    }

    // ============ 主份开关 / 自动分配排序 / 节点不足（4.1 · 4.3 · 4.4）============
    //
    // 全部穿 `add_credential_with_intent` 或 `clone_credential` 这两条**真实入口**，
    // 断言的是「入池后每一份的 proxyUrl 到底是什么」。
    // 刻意不直接测 `resolve_node_plan`：它是私有纯函数，而真实链路上排在它之前的
    // `pool_may_assign` / `primary_pinned_node` / `is_multi_open` 三道门都能把它的结果
    // 全部作废 —— 只测纯函数就是「测了分支内部，没测分支之间」那一类无效修复。

    /// 走普通上号入口（`POST /credentials` 等价路径）建 N 份。
    async fn add_copies(
        svc: &AdminService,
        copies: u32,
        mutate: impl FnOnce(&mut AddCredentialRequest),
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        let mut req = AddCredentialRequest {
            auth_method: "api_key".into(),
            kiro_api_key: Some("ksk_clone_enabled_test".into()),
            copies: Some(copies),
            disabled: true,
            ..Default::default()
        };
        mutate(&mut req);
        svc.add_credential_with_intent(req, false).await
    }

    /// 🔴 承重（4.1，开关**关**=缺省）：`POST /credentials` + `copies=3` 时
    /// **主份不从池取节点**，三个节点里只有 2 个被第 2/3 份消费。
    ///
    /// 回退即 FAILED：把 `assign_primary` 改回恒 true（即删掉
    /// `req.assign_primary_node.unwrap_or(copies == 1)` 这道门）—— 主份会拿到节点，
    /// 第一条断言变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn primary_does_not_take_a_pool_node_by_default_on_the_add_path() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 3).await;

        let resp = add_copies(&svc, 3, |_| {}).await.expect("多开应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        let urls = proxy_urls_by_id(&svc, &ids);

        assert!(
            urls[0].is_none(),
            "开关缺省（关）时主份必须保持自身出口（这里是无代理），实际 {urls:?}"
        );
        assert!(
            urls[1].is_some() && urls[2].is_some(),
            "第 2/3 份必须各拿到一个节点，实际 {urls:?}"
        );
        assert_ne!(urls[1], urls[2], "两份不得共用一个出口，实际 {urls:?}");
        // ⭐ 文案不得把「按设置刻意直连的主份」算进"因启用节点不足而直连"。
        assert!(
            resp.message.contains("已从节点池为 2 份分配独立出口 IP"),
            "应如实报 2 份，实际: {}",
            resp.message
        );
        assert!(
            !resp.message.contains("因启用节点不足而直连"),
            "主份是按设置直连，不是节点不够——这句是假归因。实际: {}",
            resp.message
        );
        assert!(
            resp.message.contains("主份按「主份也从池取节点=关」"),
            "必须说明主份为何没有出口，实际: {}",
            resp.message
        );
    }

    /// 🔴 承重（4.1，开关**开**）：显式 `assignPrimaryNode=true` 时主份也拿节点，
    /// 三份三节点全额分配。
    ///
    /// 回退即 FAILED：让 `assign_primary` 恒 false —— 主份不再拿节点，第一条断言变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn primary_takes_a_pool_node_when_the_switch_is_on() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 3).await;

        let resp = add_copies(&svc, 3, |r| r.assign_primary_node = Some(true))
            .await
            .expect("多开应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        let urls = proxy_urls_by_id(&svc, &ids);

        assert!(urls[0].is_some(), "开关开时主份必须拿到节点，实际 {urls:?}");
        let mut d: Vec<&str> = urls.iter().map(|u| u.as_deref().unwrap()).collect();
        d.sort_unstable();
        d.dedup();
        assert_eq!(d.len(), 3, "三份必须各自不同出口，实际 {urls:?}");
        assert!(
            resp.message.contains("已从节点池为 3 份分配独立出口 IP")
                && !resp.message.contains("主份按"),
            "开关开时不该出现「主份不参与」那句，实际: {}",
            resp.message
        );
    }

    /// 反序列化兼容（4.1 的硬要求）：两个请求体缺字段时都必须能解析成 `None`，
    /// 且 `None` 在各自入口上被解读成**各自的既有行为**。
    ///
    /// 回退即 FAILED：把字段写成非 `Option`（或去掉 `#[serde(default)]`）——
    /// 前两条 `expect` 直接 panic（老前端只发 `{"copies":3}` / 一堆身份字段）。
    #[test]
    fn new_node_switches_are_optional_and_default_to_existing_behavior() {
        use super::super::types::CloneCredentialRequest;

        // ① clone 入口：老前端的请求体必须照旧能解析。
        let old_clone: CloneCredentialRequest =
            serde_json::from_str(r#"{"copies":3}"#).expect("老 clone 请求体必须能解析");
        assert_eq!(old_clone.assign_primary_node, None);
        assert_eq!(old_clone.require_node_per_copy, None);

        // ② add 入口：老前端的请求体必须照旧能解析。
        let old_add: AddCredentialRequest =
            serde_json::from_str(r#"{"authMethod":"api_key","kiroApiKey":"ksk_x","copies":2}"#)
                .expect("老 add 请求体必须能解析");
        assert_eq!(old_add.assign_primary_node, None);
        assert_eq!(old_add.require_node_per_copy, None);
        assert_eq!(old_add.primary_node_id, None);

        // ③ camelCase 线上格式必须解得出（写成 snake_case 就永远收不到前端的值）。
        let given: AddCredentialRequest = serde_json::from_str(
            r#"{"authMethod":"api_key","assignPrimaryNode":true,"requireNodePerCopy":true,"primaryNodeId":7}"#,
        )
        .expect("camelCase 必须能解析");
        assert_eq!(given.assign_primary_node, Some(true));
        assert_eq!(given.require_node_per_copy, Some(true));
        assert_eq!(given.primary_node_id, Some(7));

        // ④ clone 入口的 `None` 必须被解读成 true —— 这是"升级后行为不变"的那一半：
        //    裸 `#[serde(default)]` 的 false 会让老前端静默退回 2026-08-05 修掉的缺陷
        //    （主份裸连、池里空着一个节点）。这里锁的是 service 层那句 `unwrap_or(true)`。
        let src = include_str!("service.rs");
        let needle = format!(
            "{}{}",
            "assign_primary_node: Some(assign_primary_node.", "unwrap_or(true))"
        );
        assert!(
            src.contains(needle.as_str()),
            "clone_credential 必须把缺省解读成 true，否则老前端退回主份裸连的旧缺陷"
        );
    }

    /// 🔴 承重（4.3）：自动分配按「已绑凭据数」升序、同数按延迟升序。
    ///
    /// 构造（3 个启用节点 + 1 个测活失败的）：
    /// | 节点 | 已绑 | 延迟 | 期望顺序 |
    /// |---|---|---|---|
    /// | n0 | 0 | 300ms | 第 2 |
    /// | n1 | **1**（父号绑着它）| 100ms | 第 3（已绑数是主键，延迟最低也排最后）|
    /// | n2 | 0 | 200ms | **第 1** |
    /// | n3 | 0 | 50ms + `ok=false` | 不参与（已知不通）|
    ///
    /// 一条断言同时钉住三件事：已绑数是主键（n1 最后）、延迟是次键（n2 在 n0 前）、
    /// 测活失败被排除（n3 不出现，尽管它延迟最低）。
    ///
    /// 回退即 FAILED：把排序键改回插入顺序（`sort_by_key` 那行删掉）→ 顺序变 n0/n1/n2；
    /// 或去掉 `last_test` 的 ok 过滤 → n3 会以 50ms 排到第一。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auto_assignment_orders_by_bound_count_then_latency() {
        let svc = mk_clone_service();
        let nodes = seed_nodes(&svc, 4).await;

        // 父号 #1 绑上 n1 → n1 的「已绑数」= 1（启发式按 proxy_url 字符串比对）。
        svc.token_manager
            .set_credential_proxy(1, Some(node_url(1)), None, None)
            .expect("给父号绑节点应成功");

        let mk_test = |ok: bool, latency: u64| crate::kiro::model::socks_node::SocksNodeTest {
            ok,
            latency_ms: latency,
            exit_ip: None,
            error: None,
            tested_at: 1,
        };
        svc.record_socks_node_test(nodes[0], mk_test(true, 300))
            .unwrap();
        svc.record_socks_node_test(nodes[1], mk_test(true, 100))
            .unwrap();
        svc.record_socks_node_test(nodes[2], mk_test(true, 200))
            .unwrap();
        // 已知不通：延迟最低但必须被排除。
        svc.record_socks_node_test(nodes[3], mk_test(false, 50))
            .unwrap();

        // clone 路径（主份也参与，缺省 true）建 3 份 → 按序应拿 n2 / n0 / n1。
        let resp = svc
            .clone_credential(1, 3, None, None, None, None, None)
            .await
            .expect("加分身应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        let urls = proxy_urls_by_id(&svc, &ids);

        assert_eq!(
            urls,
            vec![Some(node_url(2)), Some(node_url(0)), Some(node_url(1))],
            "顺序必须是 (已绑数↑, 延迟↑)：n2(0/200) → n0(0/300) → n1(1/100)。实际 {urls:?}"
        );
        assert!(
            !urls
                .iter()
                .any(|u| u.as_deref() == Some(node_url(3).as_str())),
            "最近测活失败的节点不得参与自动分配（它延迟最低，靠这条才能区分排序与过滤），实际 {urls:?}"
        );
    }

    /// 4.3 的另一半：`boundCredentials` 必须真的下发给前端。
    ///
    /// 前端的节点下拉与「自动分配」按钮按它排序，与后端 `resolve_node_plan` 同一口径。
    /// 回退即 FAILED：`list_socks_nodes` 改回 `map(SocksNodeView::from_node)`（恒 0）——
    /// 第二条断言变红，前端排序退化成插入顺序而后端仍按已绑数，两边推荐不一致。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_list_reports_bound_credential_count() {
        let svc = mk_clone_service();
        let nodes = seed_nodes(&svc, 2).await;
        svc.token_manager
            .set_credential_proxy(1, Some(node_url(1)), None, None)
            .expect("绑节点应成功");

        let listed = svc.list_socks_nodes();
        let by_id = |id: u64| {
            listed
                .iter()
                .find(|v| v.id == id)
                .unwrap_or_else(|| panic!("节点 #{id} 应在列表里"))
                .bound_credentials
        };
        assert_eq!(by_id(nodes[0]), 0, "没号绑它 → 0");
        assert_eq!(by_id(nodes[1]), 1, "父号绑着它 → 1");
    }

    /// 🔴 承重（4.4）：严格模式下节点不足 → **报错且一份也不建**，绝不复用。
    ///
    /// 回退即 FAILED：删掉那段 `require_node_per_copy == Some(true)` 的检查 ——
    /// 请求会成功建出 4 份（2 份带出口、2 份直连），前两条断言变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn strict_mode_errors_instead_of_creating_copies_without_nodes() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 2).await;
        let before = svc.token_manager.total_count();

        let err = add_copies(&svc, 4, |r| {
            r.assign_primary_node = Some(true);
            r.require_node_per_copy = Some(true);
        })
        .await
        .expect_err("节点不足时必须报错，而不是建出一堆共用出口的份");

        let msg = err.to_string();
        assert!(
            msg.contains("节点不足") && msg.contains("需要 4 个") && msg.contains("只有 2 个"),
            "报错必须说清需要几个/实际几个，实际: {msg}"
        );
        assert_eq!(
            svc.token_manager.total_count(),
            before,
            "严格模式失败时**一份都不该建出来**（否则是「建了一半再报错」）"
        );
    }

    /// 4.4 的宽松侧（零回归）：不开严格模式时行为逐字不变 —— 节点不够就直连，不报错。
    ///
    /// 这条是上一条的对照组：没有它，「严格模式」可能被写成"恒严格"而测不出来。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lenient_mode_still_falls_back_to_direct_without_error() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 2).await;

        let resp = add_copies(&svc, 4, |r| r.assign_primary_node = Some(true))
            .await
            .expect("缺省（宽松）时节点不够也必须成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        assert_eq!(ids.len(), 4, "宽松模式应照旧建出 4 份，实际 {ids:?}");
        let urls = proxy_urls_by_id(&svc, &ids);
        assert_eq!(
            urls.iter().filter(|u| u.is_some()).count(),
            2,
            "只有 2 个节点 → 只能有 2 份带出口（其余直连，不复用），实际 {urls:?}"
        );
    }

    /// 🔴 承重（4.4 的位置）：严格模式的检查必须排在 `reserve_clone_seqs` **之前**。
    ///
    /// 源码级顺序断言（同款范式见 `clone_seq_must_be_reserved_before_any_await`）：
    /// 放在号段预留之后时，每次"节点不够"的失败都会白烧掉一段组内序号 →
    /// 分身管理页上留下永久空洞（#1 #2 #3 #7 #8），而重试一次就再烧一段。
    /// 这一条测的是**分支之间的顺序**，行为测试测不出来（两种顺序都返回同一个错误）。
    #[test]
    fn strict_node_check_must_run_before_reserving_clone_seqs() {
        let src = include_str!("service.rs");
        // needle 运行时拼接：写成字面量会被 include_str! 读到本测试自身，
        // 于是两个 find 都命中这里、顺序恒成立 —— 断言静默作废。
        let check = format!(
            "{}{}",
            "req.require_node_per_copy == ", "Some(true) && pool_may_assign"
        );
        let reserve = format!(
            "{}{}",
            "self.token_manager.reserve_clone", "_seqs(g, copies)"
        );
        let check_at = src.find(check.as_str()).expect("严格模式检查应存在");
        let reserve_at = src.find(reserve.as_str()).expect("号段预留应存在");
        assert!(
            check_at < reserve_at,
            "节点不足检查（位置 {check_at}）必须早于号段预留（位置 {reserve_at}）：\
             放在之后会让每次失败都白烧一段组内序号，分身页上留永久空洞"
        );
    }

    /// 🔴 承重（4.2 的后端侧）：`primaryNodeId` 点名的节点写进主份，
    /// 且该节点**不会**再被第 2..N 份分到。
    ///
    /// 为什么不复用 `nodeIds[0]`：`nodeIds` 的语义是"本次只用这些"，于是
    /// `copies=3 + nodeIds=[X]` 会让第 2/3 份一个节点都拿不到。本字段只钉主份，
    /// 其余份仍从池里自动补。
    ///
    /// 回退即 FAILED：把 `primary_node_id` 的处理删掉 → 主份变直连（第一条断言红）；
    /// 或不把它从计划里排除（`exclude_id` 那两个 filter）→ 有一份会与主份共用出口
    /// （第三条断言红）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn primary_node_id_pins_the_primary_and_is_excluded_from_the_rest() {
        let svc = mk_clone_service();
        let nodes = seed_nodes(&svc, 3).await;

        let resp = add_copies(&svc, 3, |r| r.primary_node_id = Some(nodes[1]))
            .await
            .expect("多开应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        let urls = proxy_urls_by_id(&svc, &ids);

        assert_eq!(
            urls[0].as_deref(),
            Some(node_url(1).as_str()),
            "主份必须走点名的那个节点，实际 {urls:?}"
        );
        assert!(
            urls[1].is_some() && urls[2].is_some(),
            "第 2/3 份仍应从池里自动补（点名主份不该把池锁死），实际 {urls:?}"
        );
        let mut d: Vec<&str> = urls.iter().map(|u| u.as_deref().unwrap()).collect();
        d.sort_unstable();
        d.dedup();
        assert_eq!(
            d.len(),
            3,
            "点名的节点不得被第 2..N 份再分一次，实际 {urls:?}"
        );
    }

    /// `primaryNodeId` 指向不存在 / 已禁用的节点 → **400 且不建任何份**。
    ///
    /// 静默直连或静默换一个节点都会让用户以为出口是他刚点的那个（与 `nodeIds`
    /// 那条"不静默替换"同一原则，只是这里是他唯一的选择，故直接拒绝而不是跳过）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_primary_node_id_is_rejected_without_creating_anything() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 1).await;
        let before = svc.token_manager.total_count();

        let err = add_copies(&svc, 2, |r| r.primary_node_id = Some(9999))
            .await
            .expect_err("不存在的节点 id 必须报错");
        assert!(
            err.to_string().contains("#9999 不存在"),
            "必须点名那个 id 与原因，实际: {err}"
        );
        assert_eq!(
            svc.token_manager.total_count(),
            before,
            "报错时不得建出任何份"
        );
    }

    /// `nodeIds` 的 JSON 契约：省略 → `None`（走自动分配），给了则原样解出。
    ///
    /// 回退即 FAILED：把字段写成非 `Option` 或去掉 `#[serde(default)]` ——
    /// 第一条断言（老前端只发 `{"copies":3}`）直接解析失败。
    #[test]
    fn clone_request_parses_node_ids_as_optional_camel_case() {
        use super::super::types::CloneCredentialRequest;

        let omitted: CloneCredentialRequest =
            serde_json::from_str(r#"{"copies":3}"#).expect("省略 nodeIds 应能解析");
        assert_eq!(omitted.node_ids, None, "省略时必须是 None（走自动分配）");

        let given: CloneCredentialRequest =
            serde_json::from_str(r#"{"copies":4,"enabled":false,"nodeIds":[1,5,6,9]}"#)
                .expect("解析 nodeIds");
        assert_eq!(given.node_ids, Some(vec![1, 5, 6, 9]));
        assert_eq!(given.copies, Some(4));
        assert_eq!(given.enabled, Some(false));

        // 空数组与省略同义（前端可能两种都发）——语义在 service 层收口，
        // 这里只锁「能解析成空 Vec 而不是报错」。
        let empty: CloneCredentialRequest =
            serde_json::from_str(r#"{"copies":2,"nodeIds":[]}"#).expect("解析空 nodeIds");
        assert_eq!(empty.node_ids, Some(vec![]));
    }

    /// ⭐ 节点 id **永不复用**，包括「删掉最大 id 后再新建」。
    ///
    /// 回退即 FAIL：把 id 分配改回 `nodes.iter().map(|n| n.id).max().unwrap_or(0) + 1`
    /// —— 删掉 #2 后新建又得到 #2，而面板另一个标签页仍持有删除前的列表，
    /// 点它的「测活」会打到这个无关的新节点上。
    #[tokio::test]
    async fn node_ids_are_never_reused_after_deleting_the_highest() {
        let svc = mk_service_with_one_credential();
        let mk = |n: u16| SocksNodeUpsertRequest {
            id: None,
            name: Some(format!("n{n}")),
            url: format!("socks5://node{n}.invalid:40002"),
            username: None,
            password: None,
            enabled: None,
        };
        let a = svc.upsert_socks_node(mk(1)).await.unwrap();
        let b = svc.upsert_socks_node(mk(2)).await.unwrap();
        assert!(b > a);

        assert!(svc.delete_socks_node(b).await_ok());
        let c = svc.upsert_socks_node(mk(3)).await.unwrap();
        assert!(
            c > b,
            "删掉最大 id 后新建必须拿到更大的 id（实得 {c}，已发放过 {b}）"
        );
    }

    // ============ 同 key「无独立出口」告警 + 组标识回填 ============
    //
    // 线上实测的形态（本组测试的依据）：`#776` keyHash=029fdd8929、**无 cloneGroup、
    // 无代理**；`#778–787` 同 key 同组、各有独立 SOCKS ⇒ 11 份共用一个上游账号，
    // 其中 1 份走服务器裸 IP。`mk_clone_service` 的父号 `#1` 与 `#776` 完全同构
    // （api_key、无 proxy_url、无 clone_group），所以这组测试就是那个场景本身。

    /// 父号在池中的原始快照（用来断言「除了组标识，一个字段都没被动」）。
    fn parent_snapshot(svc: &AdminService) -> crate::kiro::model::credentials::KiroCredentials {
        svc.token_manager
            .export_credential(1)
            .expect("父号 #1 必须存在")
    }

    /// 🔴 承重（任务一）：同 key 有份**没有独立出口**时必须告警，
    /// 且**绝不**因此改动它的 `proxy_url`。
    ///
    /// 两条断言各自钉一件事，缺任何一条都会漏掉一类回归：
    /// - 告警出现 → 防「静默」（用户在面板上看到 N 份都有 socks，唯独那一份看不出来）
    /// - 父号 `proxy_url` 仍为 `None` → 防「好心自动分配」（用户已明确拍板不要，
    ///   `proxy_url` 是显式配置，直连也可能是刻意留的对照）
    ///
    /// 回退即 FAILED：删掉 `bare_exit_note` 那段 → 第一条变红；
    /// 把它改成「顺手给无出口的号 `set_credential_proxy`」→ 第二条变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cloning_warns_about_same_key_members_without_their_own_exit() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 2).await;
        // 前提：父号确实没有出口（与线上 #776 同构）。
        assert!(
            parent_snapshot(&svc).proxy_url.is_none(),
            "构造前提：父号必须无代理"
        );

        let resp = svc
            .clone_credential(1, 2, None, None, None, None, None)
            .await
            .expect("加分身应成功");

        assert!(
            resp.message.contains("没有独立出口") && resp.message.contains("#1"),
            "同 key 的 #1 无出口必须被点名告警，实际: {}",
            resp.message
        );
        // ⭐ 承重：告警不得升级成"自动改配置"。
        assert!(
            parent_snapshot(&svc).proxy_url.is_none(),
            "父号的 proxy_url 是显式配置，克隆路径只许告警、绝不许写它，实际 {:?}",
            parent_snapshot(&svc).proxy_url
        );
        // 新建的两份该拿到节点 —— 否则"父号无出口"这句可能只是因为整池都没分到。
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        let urls = proxy_urls_by_id(&svc, &ids);
        assert!(
            urls.iter().all(|u| u.is_some()),
            "两个节点两份，应各自拿到出口，实际 {urls:?}"
        );
    }

    /// 🔴 承重（任务一的判据）：查找必须按 **key**，不能按 `cloneGroup`。
    ///
    /// 这是缺陷能长期存活的原因：同账号里最先入池的那一份**天然没有组标识**
    /// （组是后来加分身才产生的），按组去找就恰好漏掉它 —— 而它正是那个裸 IP。
    ///
    /// 构造让两种判据结果不同：父号 `#1` 无 `cloneGroup`，新建的份拿到一个新组。
    /// 按 key 查 → 找到 `#1` → 告警；按组查 → `#1` 不在任何组里 → 静默。
    ///
    /// ⚠️ 这条能成立依赖**顺序**：名单必须在组标识回填**之前**取。回填之后父号也在组里了，
    /// 两种判据就再也分不出来（那正是本仓「测了分支内部、没测分支顺序」的老毛病）。
    ///
    /// 回退即 FAILED：把 `same_key_peers` 改成按 `clone_group` 过滤，
    /// 或把回填那段挪到取名单之前 —— 告警消失，本条变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_bare_exit_lookup_keys_on_the_api_key_not_the_clone_group() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 1).await;
        // 前提：父号一个组标识都没有（线上 #776 就是这样）。
        assert!(
            parent_snapshot(&svc).clone_group.is_none(),
            "构造前提：父号必须无 cloneGroup"
        );

        let resp = svc
            .clone_credential(1, 1, None, None, None, None, None)
            .await
            .expect("加 1 份应成功");

        assert!(
            resp.message.contains("没有独立出口") && resp.message.contains("#1"),
            "父号无组标识时仍必须被发现（按组查会漏掉它，那就是本缺陷），实际: {}",
            resp.message
        );
    }

    /// 🔴 源码级守卫：**取名单**必须早于**回填组标识**。
    ///
    /// 为什么必须额外有这一条（这是本仓「测了分支内部、没测分支顺序」那一类的正解）：
    /// 上面那条按-key 行为测试的判别力**依赖这个顺序**。实测过：只把判据改成按组 →
    /// 那条测试红；但**同时**把回填提到取名单之前 → 它又变绿了（回填先把父号补进组里，
    /// 按组查也能查到）。也就是说没有本条守卫时，两处一起改就能让缺陷重新隐形。
    ///
    /// 回退即 FAILED：把回填那段挪到 `same_key_peers` 之前 —— 位置比较翻转，本条变红。
    #[test]
    fn the_same_key_peer_snapshot_must_be_taken_before_the_group_backfill() {
        let src = include_str!("service.rs");
        // needle 运行时拼接：写成字面量会被 include_str! 读到本测试自身，
        // 两个 find 都命中这里 → 顺序恒成立 → 断言静默作废（同 strict_node_check 那条）。
        let snapshot = format!("{}{}", "let same_key_peers = ", "new_cred");
        let backfill = format!(
            "{}{}",
            "for peer in same_key_peers.iter()", ".filter(|p| p.clone_group.is_none())"
        );
        let snapshot_at = src.find(snapshot.as_str()).expect("同 key 名单快照应存在");
        let backfill_at = src.find(backfill.as_str()).expect("组标识回填应存在");
        assert!(
            snapshot_at < backfill_at,
            "取名单（位置 {snapshot_at}）必须早于回填（位置 {backfill_at}）：\
             反过来会让「按 key 查」与「按组查」再也无法区分，判据被改坏也测不出来"
        );
    }

    /// 对照组：同 key 的成员**都有**独立出口时不得告警。
    ///
    /// 没有这一条，上面两条可以靠"永远告警"蒙过去 —— 而永远告警等于没有告警
    /// （用户会学会忽略它），本仓已有多起同类文案失效。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_warning_when_every_same_key_member_already_has_an_exit() {
        let svc = mk_clone_service();
        let nodes = seed_nodes(&svc, 3).await;
        // 父号自己先绑一个节点（面板上"给这一份配个出口"的等价操作）。
        svc.token_manager
            .set_credential_proxy(1, Some(node_url(0)), None, None)
            .expect("给父号绑节点应成功");
        assert_eq!(nodes.len(), 3);

        let resp = svc
            .clone_credential(1, 2, None, None, None, None, None)
            .await
            .expect("加分身应成功");

        assert!(
            !resp.message.contains("没有独立出口"),
            "同 key 全员有出口时不得告警（狼来了会让告警失效），实际: {}",
            resp.message
        );
        // 顺带钉住：父号已有的出口不得被本次克隆改掉。
        assert_eq!(
            parent_snapshot(&svc).proxy_url.as_deref(),
            Some(node_url(0).as_str()),
            "父号已配的出口不得被克隆路径覆盖"
        );
    }

    /// 🔴 承重（任务二）：回填后父号的 `cloneGroup` 与新建的份一致，
    /// 且**除它之外一个字段都没被动**。
    ///
    /// 为什么要回填：前端 `groupClones` 为「父号早于 cloneGroup 字段入池」维护了一整套
    /// `apiKeyHash` 回落分组。回填让**新产生的**数据不再欠这笔债（老数据仍靠回落兜住，
    /// 本轮刻意不删回落逻辑）。
    ///
    /// 为什么这与「不改父号 proxy_url」不矛盾：`cloneGroup` 是系统内部的分组标识，
    /// 没有语义选择余地（父号确实属于那个组）；`proxy_url` 是用户的显式配置。
    ///
    /// 回退即 FAILED：删掉 service 里那段 `set_clone_identity` 回填循环 —— 第一条变红。
    /// 把回填改成连 `clone_seq` 一起写（或顺手写别的字段）—— 第三条变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cloning_backfills_the_clone_group_onto_the_same_key_parent() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 2).await;
        let before = parent_snapshot(&svc);
        assert!(before.clone_group.is_none(), "构造前提：父号无 cloneGroup");

        let resp = svc
            .clone_credential(1, 2, None, None, None, None, None)
            .await
            .expect("加分身应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");

        let after = parent_snapshot(&svc);
        let group = after
            .clone_group
            .clone()
            .expect("父号必须被回填 cloneGroup（否则前端只能靠 apiKeyHash 回落分组）");
        // 与新建的每一份同组 —— 回填一个**不同**的 UUID 比不回填更糟（面板上裂成两组）。
        for id in &ids {
            let child = svc
                .token_manager
                .export_credential(*id)
                .expect("分身应存在");
            assert_eq!(
                child.clone_group.as_deref(),
                Some(group.as_str()),
                "分身 #{id} 必须与回填后的父号同组"
            );
        }

        // ⭐ 承重：回填**只**动 clone_group。逐字段比对（比挑几个字段断言更难被绕过）。
        assert_eq!(
            after.clone_seq, before.clone_seq,
            "回填不得给父号凭空编号（编号必须走 reserve_clone_seqs，否则与组内既有号撞车）"
        );
        let mut expected = before.clone();
        expected.clone_group = after.clone_group.clone();
        assert_eq!(
            serde_json::to_value(&expected).expect("序列化父号快照"),
            serde_json::to_value(&after).expect("序列化父号现状"),
            "回填只许改 cloneGroup，其它字段一个都不能动"
        );
    }
}

/// 测试用小助手：把 `Result<bool, _>` 当成断言用，避免每处都 unwrap。
#[cfg(test)]
trait AwaitOk {
    fn await_ok(self) -> bool;
}

#[cfg(test)]
impl AwaitOk for Result<bool, AdminServiceError> {
    fn await_ok(self) -> bool {
        self.expect("操作应成功")
    }
}

#[cfg(test)]
mod balance_baseline_tests {
    //! G-2：新取到的余额真值与「花费基线」必须**成对更新**。
    //!
    //! 断言的全是 `get_cached_balances()` 的输出（前端真正消费的那个端点），
    //! 不断言内部表长什么样。
    use super::balance_cache_tests::mk_service_with_one_credential;
    use super::*;

    fn mk_balance(remaining: f64, used: f64) -> BalanceResponse {
        BalanceResponse {
            id: 1,
            subscription_title: Some("Kiro Pro".to_string()),
            current_usage: used,
            usage_limit: 100.0,
            remaining,
            usage_percentage: used,
            next_reset_at: None,
            overage_enabled: false,
            overage_cap: 0.0,
            effective_limit: 100.0,
            stale: false,
            optimistic: false,
        }
    }

    /// 面板上那个号当前显示的 remaining。
    fn shown_remaining(svc: &AdminService, id: u64) -> f64 {
        svc.get_cached_balances()
            .balances
            .get(&id)
            .unwrap_or_else(|| panic!("凭据 #{id} 应有缓存余额"))
            .balance
            .remaining
    }

    /// ⭐ 回归（用户反馈「额度/积分刷新太慢/不对」）：取到新真值后**不得再扣一次**已花掉的量。
    ///
    /// # 旧代码为何 FAIL
    ///
    /// `get_balance` 只 `cache.insert` 而不动基线 ⇒ 新真值（已含那 20）配着旧基线（50）
    /// ⇒ 面板再扣一次 delta=70-50=20 ⇒ 显示 60 而真值是 80。
    /// 把 `commit_fresh_balance` 里的 `push_balance_snapshots_to_scheduler` 那行删掉
    /// （= 回到旧行为），本测试最后一条断言必 FAILED（拿到 60）。
    #[test]
    fn fresh_truth_resets_the_spend_baseline_so_it_is_not_double_counted() {
        let svc = mk_service_with_one_credential();
        let key = svc.balance_cache_key(1);

        // t0：拿到真值 remaining=100，此刻本地累计花费 50
        svc.token_manager.add_credits(1, 50.0);
        svc.commit_fresh_balance(key.clone(), mk_balance(100.0, 0.0));
        assert_eq!(shown_remaining(&svc, 1), 100.0, "刚取到真值时不应有修正");

        // 期间花掉 20 → 乐观修正把它扣掉（这是既有的、正确的行为）
        svc.token_manager.add_credits(1, 20.0);
        assert_eq!(
            shown_remaining(&svc, 1),
            80.0,
            "两次真值之间应按本地花费乐观推进"
        );

        // 用户点「查看余额」，上游返回的真值 80 **已经包含**那 20。
        svc.commit_fresh_balance(key, mk_balance(80.0, 20.0));
        assert_eq!(
            shown_remaining(&svc, 1),
            80.0,
            "新真值已含那 20，绝不能再扣一次（旧代码在这里给出 60）"
        );
    }

    /// 只有**本次取到真值**的账号才重置基线；其余账号保留原基线。
    ///
    /// # 旧代码为何 FAIL
    ///
    /// 原 `push_balance_snapshots_to_scheduler` 无条件把所有账号的基线推到"现在"。
    /// 于是刷新失败（缓存仍是旧真值）的号，其"缓存之后已花掉的量"被一次性抹掉 ⇒
    /// 面板与调度器都把它当成比实际更有余额的号。
    /// 把 `fresh_keys` 判断改回无条件 `used_now`，第二条断言必 FAILED（拿到 100）。
    #[test]
    fn non_fresh_accounts_keep_their_baseline() {
        let svc = mk_service_with_one_credential();
        let key = svc.balance_cache_key(1);

        svc.token_manager.add_credits(1, 50.0);
        svc.commit_fresh_balance(key, mk_balance(100.0, 0.0));
        svc.token_manager.add_credits(1, 30.0);
        assert_eq!(shown_remaining(&svc, 1), 70.0);

        // 模拟「本轮该号刷新失败」的收尾回推：fresh_keys 为空。
        svc.push_balance_snapshots_to_scheduler(&HashSet::new());
        assert_eq!(
            shown_remaining(&svc, 1),
            70.0,
            "没取到新真值的号必须保留原基线，否则已花掉的 30 被抹掉、显示回 100"
        );
    }

    /// 源码守卫：`get_balance` 不得再内联 `cache.insert`（那会绕过基线重置）。
    ///
    /// 这条锁的是**接线**而非逻辑：上面两条测的是 `commit_fresh_balance` 的行为，
    /// 但真正的用户路径是 `get_balance`；若哪天有人在那里又写回一个裸 insert，
    /// 行为测试全绿而缺陷回归。单测无法真跑 `get_balance`（要打 app.kiro.dev），故用源码断言。
    #[test]
    fn get_balance_writes_through_the_single_commit_path() {
        let src = include_str!("service.rs");
        let body = src
            .split("pub async fn get_balance")
            .nth(1)
            .expect("get_balance 不应被改名")
            .split("fn balance_cache_key")
            .next()
            .expect("balance_cache_key 应紧随其后");
        // needle 运行时拼接：include_str! 会把本测试自己的字面量也读进来。
        let commit = format!("self.commit_fresh{}", "_balance(");
        assert!(
            body.contains(commit.as_str()),
            "get_balance 必须走 commit_fresh_balance 收口（它负责同步重置花费基线）"
        );
        let inline_insert = format!("cache.insert{}", "(");
        assert!(
            !body.contains(inline_insert.as_str()),
            "get_balance 里不得内联 cache.insert —— 那会漏掉基线重置，面板把已花掉的量扣两次"
        );
    }

    /// 后台温和刷新同样必须走那个收口（同一漏改面）。
    #[test]
    fn background_refresh_writes_through_the_single_commit_path() {
        let src = include_str!("service.rs");
        let body = src
            .split("pub async fn refresh_all_balances_gently")
            .nth(1)
            .expect("refresh_all_balances_gently 不应被改名")
            .split("fn commit_fresh_balance")
            .next()
            .expect("commit_fresh_balance 应紧随其后");
        let commit = format!("self.commit_fresh{}", "_balance(");
        assert!(
            body.contains(commit.as_str()),
            "后台刷新也必须走 commit_fresh_balance（两条路径各写一份 insert 正是漏改根源）"
        );
    }

    /// `force` 查询串契约：**省略必须是 false**（老前端不带该参数时保持走缓存的原语义）。
    ///
    /// 走真实的 axum `Query` 提取器而不是直接反序列化 —— 要锁的正是"没带这个参数的请求
    /// 不会 400、且不会变成强制打上游"。回退即 FAIL：去掉 `#[serde(default)]` →
    /// 第一条断言（无查询串）直接解析失败。
    #[test]
    fn balance_query_force_defaults_to_false() {
        use super::super::handlers::BalanceQuery;
        use axum::extract::Query;

        let bare: Query<BalanceQuery> = Query::try_from_uri(
            &"http://x/api/admin/credentials/1/balance"
                .parse::<axum::http::Uri>()
                .unwrap(),
        )
        .expect("不带查询串的请求必须能解析（老前端就是这么发的）");
        assert!(!bare.0.force, "省略 force 必须走缓存（不改既有行为）");

        let forced: Query<BalanceQuery> = Query::try_from_uri(
            &"http://x/api/admin/credentials/1/balance?force=true"
                .parse::<axum::http::Uri>()
                .unwrap(),
        )
        .expect("解析 force=true");
        assert!(forced.0.force);
    }
}

#[cfg(test)]
mod cleanup_disabled_tests {
    //! 批量清理已禁用凭据（G-1）。
    //!
    //! 承重点不是"能删"，而是**该不该删的判据**：误清一个代挂号 =
    //! 删掉用户自配的第三方中转。所以每条排除都有一条对照断言。
    use super::*;

    /// 造一条凭据。`base_url` 非 None 即为代挂号（`is_custom_api_credential` 的旧数据判据）。
    fn mk(
        id: u64,
        auth_method: &str,
        base_url: Option<&str>,
        disabled: bool,
        reason: Option<DisabledReason>,
    ) -> KiroCredentials {
        // QuotaExceeded 死号带**当月**判定时刻：启动跨月恢复只放过期月份（缺失时间戳
        // 也视为可恢复），不带时刻会让这些号在构造时被自动复活，测试前提被打破。
        let quota_exhausted_ts = (reason == Some(DisabledReason::QuotaExceeded))
            .then(|| Utc::now().to_rfc3339());
        KiroCredentials {
            id: Some(id),
            auth_method: Some(auth_method.to_string()),
            // `.invalid` 是 RFC 6761 保留 TLD，保证永不解析 —— 测试不依赖本机 DNS
            // （历史事故：fake-IP 模式代理把 example.com 解到 198.18/16，被 SSRF 正确拦掉）。
            base_url: base_url.map(|s| s.to_string()),
            kiro_api_key: match auth_method {
                "api_key" | "custom_api" => Some(format!("ksk_test_{id}")),
                _ => None,
            },
            disabled,
            disabled_reason: reason,
            quota_exhausted_at: quota_exhausted_ts,
            ..Default::default()
        }
    }

    fn mk_service(creds: Vec<KiroCredentials>) -> AdminService {
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                // 单凭据格式 ⇒ persist 是 no-op，删除只走内存 + 内存回收站。
                false,
            )
            .expect("构造 token manager"),
        );
        AdminService::new(tm, Vec::<String>::new())
    }

    /// 判据本身：四道排除各一条，加上"该清的真会被清"的对照。
    ///
    /// 回退即 FAIL：删掉 `cleanup_verdict` 里的 `is_custom_api` 分支 → 第 1 条失败；
    /// 删掉禁用原因那道 → 第 2/3 条失败；删掉可自愈那道 → 第 4 组失败。
    #[test]
    fn verdict_excludes_custom_api_and_passthrough_reasons() {
        // 代挂号：无论禁用原因是什么都不清
        assert_eq!(cleanup_verdict(Some(true), None), Some("custom_api"));
        assert_eq!(
            cleanup_verdict(Some(true), Some("QuotaExceeded")),
            Some("custom_api"),
            "代挂号即便原因看着像死号也不清（它的额度是中转站的，充值即可用）"
        );

        // 代挂专属原因：即便认不出是代挂（历史数据缺 auth_method/base_url）也要拦住
        assert_eq!(
            cleanup_verdict(Some(false), Some("PassthroughFailed")),
            Some("passthrough_disabled")
        );
        assert_eq!(
            cleanup_verdict(Some(false), Some("PassthroughOverloaded")),
            Some("passthrough_disabled")
        );

        // 可自愈原因：号会自己回池，删它等于拿走健康号
        for r in [
            "TooManyFailures",
            "SuspiciousActivityAuto",
            "TooManyRefreshFailures",
        ] {
            assert_eq!(
                cleanup_verdict(Some(false), Some(r)),
                Some("self_healable"),
                "{r} 在自愈白名单里，禁用态是瞬时的，不能当死号删"
            );
        }

        // 竞态：号已不在池里 → 不清，且原因不能报成代挂
        assert_eq!(cleanup_verdict(None, None), Some("not_in_pool"));
        assert_eq!(
            cleanup_verdict(None, Some("QuotaExceeded")),
            Some("not_in_pool"),
            "拿不到凭据时其余判据全是猜的，只能报'号没了'"
        );

        // 对照：真死号该清
        for r in [
            "Manual",
            "QuotaExceeded",
            "AccountSuspended",
            "InvalidRefreshToken",
            "InvalidConfig",
            "RequestLimitReached",
            "RegionProbeFailed",
            "RegionProbeTokenDead",
        ] {
            assert_eq!(
                cleanup_verdict(Some(false), Some(r)),
                None,
                "{r} 是 Kiro 号的死因（不在自愈白名单里），必须被清"
            );
        }
        // 禁用但无原因（老数据）也该清 —— 它已经是禁用态，本来就不参与调度。
        assert_eq!(cleanup_verdict(Some(false), None), None);
    }

    /// ⭐ 可自愈集合必须与 `token_manager::is_self_healable_reason` 的白名单**逐字符相同**。
    ///
    /// 那个函数是私有的、且吃枚举，这里没法直接调它，所以抄了一份。抄本会漂，而漂移的后果是
    /// 静默的：白名单加了新变体、这里没跟 → 那种号又会被当死号删走（正是本轮修的 bug）。
    ///
    /// 用**穷举 match** 而不是列表相等来锁：`DisabledReason` 新增变体时这条 match
    /// 会编译不过，逼作者当场判断"新原因可不可自愈"，而不是等线上删错号。
    ///
    /// 回退即 FAIL：从 `CLEANUP_SELF_HEALABLE_REASONS` 里删掉任一项 → 对应断言失败。
    #[test]
    fn self_healable_set_matches_token_manager_whitelist() {
        // 穷举全部变体，逐个声明期望值。expected 的取值依据是
        // `token_manager.rs::is_self_healable_reason` 的 matches! 白名单。
        let all: [(DisabledReason, bool); 14] = [
            (DisabledReason::Manual, false),
            (DisabledReason::TooManyFailures, true),
            (DisabledReason::TooManyRefreshFailures, true),
            (DisabledReason::QuotaExceeded, false),
            (DisabledReason::AccountSuspended, false),
            (DisabledReason::SuspiciousActivityAuto, true),
            (DisabledReason::InvalidRefreshToken, false),
            (DisabledReason::InvalidConfig, false),
            (DisabledReason::RequestLimitReached, false),
            (DisabledReason::PassthroughFailed, false),
            (DisabledReason::PassthroughOverloaded, false),
            (DisabledReason::RegionProbeFailed, false),
            (DisabledReason::RegionProbeTokenDead, false),
            (DisabledReason::Unknown, false),
        ];
        // 编译期门禁：新增变体后这个 match 缺分支即编译失败，届时必须回到上面的表里补一行。
        for (r, _) in &all {
            match r {
                DisabledReason::Manual
                | DisabledReason::TooManyFailures
                | DisabledReason::TooManyRefreshFailures
                | DisabledReason::QuotaExceeded
                | DisabledReason::AccountSuspended
                | DisabledReason::SuspiciousActivityAuto
                | DisabledReason::InvalidRefreshToken
                | DisabledReason::InvalidConfig
                | DisabledReason::RequestLimitReached
                | DisabledReason::PassthroughFailed
                | DisabledReason::PassthroughOverloaded
                | DisabledReason::RegionProbeFailed
                | DisabledReason::RegionProbeTokenDead
                | DisabledReason::Unknown => {}
            }
        }

        for (reason, healable) in all {
            assert_eq!(
                CLEANUP_SELF_HEALABLE_REASONS.contains(&reason),
                healable,
                "{} 的可自愈判定与 token_manager 白名单不一致",
                reason.as_str()
            );
        }
    }

    /// 判据里那两个字符串必须与 `DisabledReason::as_str()` 同源。
    ///
    /// 回退即 FAIL：把 `cleanup_verdict` 里的枚举调用换成手写字面量
    /// （例如 `"passthroughFailed"` 这种 camelCase 拼法）→ 本测试的 as_str 对不上。
    /// 这条锁的是**契约同源**：`as_str` 的字面量就是 Admin API 下发给前端的值，
    /// 而快照给我们的 `disabled_reason` 正是它的产物，两侧一旦分叉，排除会静默失效。
    #[test]
    fn passthrough_reason_strings_come_from_disabled_reason_as_str() {
        assert_eq!(
            cleanup_verdict(
                Some(false),
                Some(DisabledReason::PassthroughFailed.as_str())
            ),
            Some("passthrough_disabled")
        );
        assert_eq!(
            cleanup_verdict(
                Some(false),
                Some(DisabledReason::PassthroughOverloaded.as_str())
            ),
            Some("passthrough_disabled")
        );
    }

    /// ⭐ 端到端：真删一遍，代挂号必须**还在池里**、死号必须**进了回收站**。
    ///
    /// 这条测的是可观测状态（池 + 回收站），不是分支形状 —— 把
    /// `cleanup_disabled_credentials` 里的 `cleanup_verdict` 调用去掉（无条件收进候选），
    /// 第一组断言立刻 FAIL。
    #[test]
    fn cleanup_deletes_dead_kiro_credentials_and_keeps_passthrough() {
        let svc = mk_service(vec![
            // #1 未禁用 → 压根不是候选
            mk(1, "api_key", None, false, None),
            // #2 禁用的 Kiro 死号 → 清
            mk(
                2,
                "api_key",
                None,
                true,
                Some(DisabledReason::QuotaExceeded),
            ),
            // #3 管理员手动禁用的代挂号 → 留
            mk(
                3,
                "custom_api",
                Some("https://relay3.invalid/v1"),
                true,
                Some(DisabledReason::Manual),
            ),
            // #4 代挂号，但禁用原因是非代挂专属的未知值 → 仍靠 is_custom_api 拦住
            mk(
                4,
                "custom_api",
                Some("https://relay4.invalid/v1"),
                true,
                Some(DisabledReason::Unknown),
            ),
            // #5 认不出是代挂（api_key + 无 base_url），但原因是代挂专属 → 靠第二道网留
            mk(
                5,
                "api_key",
                None,
                true,
                Some(DisabledReason::PassthroughOverloaded),
            ),
            // #6 禁用无原因的老数据 → 清
            mk(6, "api_key", None, true, None),
            // #7 Kiro 号，但原因可自愈（自愈会把它复活）→ 留。删它 = 拿走健康号。
            mk(
                7,
                "api_key",
                None,
                true,
                Some(DisabledReason::TooManyFailures),
            ),
        ]);

        let resp = svc.cleanup_disabled_credentials(false);

        assert!(!resp.dry_run);
        assert_eq!(resp.disabled_total, 6, "#2..#7 共 6 个禁用号");
        assert_eq!(resp.candidates, vec![2, 6], "只有 #2/#6 是死号（且已升序）");
        assert_eq!(resp.deleted, 2);
        assert_eq!(resp.failed, 0);
        assert!(resp.results.iter().all(|r| r.ok));

        // 池里剩下：#1（未禁用）+ #3/#4/#5（代挂被排除）
        let remaining: Vec<u64> = {
            let mut v: Vec<u64> = svc
                .token_manager
                .snapshot()
                .entries
                .iter()
                .map(|e| e.id)
                .collect();
            v.sort_unstable();
            v
        };
        assert_eq!(
            remaining,
            vec![1, 3, 4, 5, 7],
            "代挂号 #3/#4/#5 必须还在池里（它们不是死号，修好配置就能用）；\
             #7 是自愈途中的健康号，更不能删"
        );

        // 回收站里只有那两个死号 —— 「进回收站可恢复」而不是 purge。
        let mut trashed: Vec<u64> = svc.list_trash().trash.iter().map(|t| t.id).collect();
        trashed.sort_unstable();
        assert_eq!(trashed, vec![2, 6], "删掉的号必须进回收站（可恢复）");

        // skipped 逐条带原因，供前端解释"为什么这几个没删"
        let mut skipped: Vec<(u64, &str)> = resp.skipped.iter().map(|s| (s.id, s.reason)).collect();
        skipped.sort_unstable();
        assert_eq!(
            skipped,
            vec![
                (3, "custom_api"),
                (4, "custom_api"),
                (5, "passthrough_disabled"),
                (7, "self_healable"),
            ]
        );
    }

    /// dry-run 必须**一个号都不动**，但候选与真删完全一致（同一段筛选）。
    ///
    /// 回退即 FAIL：把 `if dry_run` 那道早返回删掉 → 预览会真删，池子少两个号。
    #[test]
    fn dry_run_reports_candidates_without_deleting() {
        let creds = vec![
            mk(
                1,
                "api_key",
                None,
                true,
                Some(DisabledReason::QuotaExceeded),
            ),
            mk(
                2,
                "custom_api",
                Some("https://relay.invalid/v1"),
                true,
                Some(DisabledReason::Manual),
            ),
            mk(
                3,
                "api_key",
                None,
                true,
                Some(DisabledReason::AccountSuspended),
            ),
        ];
        let svc = mk_service(creds);

        let preview = svc.cleanup_disabled_credentials(true);
        assert!(preview.dry_run);
        assert_eq!(preview.candidates, vec![1, 3]);
        assert_eq!(preview.deleted, 0, "预览不得删任何号");
        assert!(preview.results.is_empty(), "预览没有逐条删除结果");
        assert_eq!(svc.token_manager.total_count(), 3, "预览后池子必须原样");
        assert!(svc.list_trash().trash.is_empty(), "预览不得往回收站放东西");

        // 同一段筛选 ⇒ 真删的候选与预览逐字相同
        let real = svc.cleanup_disabled_credentials(false);
        assert_eq!(
            real.candidates, preview.candidates,
            "预览与真删必须同源（否则用户看到的和实际删的不是一回事）"
        );
        assert_eq!(real.deleted, 2);
    }

    /// 上限：超出部分**留给下一次**且在 skipped 里标 `over_limit`，不静默丢弃。
    ///
    /// 回退即 FAIL：去掉 `split_off` 那段 → 一次就把 201 个全删了，
    /// 第一条断言（deleted == 200）失败。
    #[test]
    fn cleanup_caps_at_limit_and_reports_the_rest() {
        let n = MAX_CLEANUP_DISABLED_IDS as u64 + 1;
        // 原因必须是**不可自愈**的死因，否则整批都会被 self_healable 那道排除掉，
        // 这条测的上限逻辑就一个候选都碰不到（测了个空）。
        let creds: Vec<KiroCredentials> = (1..=n)
            .map(|i| {
                mk(
                    i,
                    "api_key",
                    None,
                    true,
                    Some(DisabledReason::QuotaExceeded),
                )
            })
            .collect();
        let svc = mk_service(creds);

        let resp = svc.cleanup_disabled_credentials(false);
        assert_eq!(resp.disabled_total, n as usize);
        assert_eq!(resp.candidates.len(), MAX_CLEANUP_DISABLED_IDS);
        assert_eq!(resp.deleted, MAX_CLEANUP_DISABLED_IDS);
        // 升序截断 ⇒ 留下的必然是最大的那个 id（确定性，重复调用可收敛）
        assert_eq!(
            resp.skipped
                .iter()
                .filter(|s| s.reason == "over_limit")
                .map(|s| s.id)
                .collect::<Vec<_>>(),
            vec![n]
        );
        assert_eq!(svc.token_manager.total_count(), 1, "只剩超出上限那一个");

        // 第二次调用把剩下那个清完 —— 这就是"留给下一次"的可收敛性。
        let again = svc.cleanup_disabled_credentials(false);
        assert_eq!(again.deleted, 1);
        assert_eq!(svc.token_manager.total_count(), 0);
    }

    /// ⭐ **顺序**断言：截断必须排在 `if dry_run` 早返**之前**。
    ///
    /// # 为什么单独测顺序，而不是各测一遍
    ///
    /// 「dry-run 会早返」和「超上限会截断」两个分支各自都是对的，现有测试也都覆盖了，
    /// 但它们**互相不知道对方存在**：把 `if dry_run` 那段整块挪到 `split_off` 之前，
    /// 两条旧测试仍全绿（一条不超限、一条不 dry-run），而预览会报 201 个候选、
    /// 真删只删 200 —— 用户看到的和实际删的不是一回事，正是 dry-run 唯一要防的事。
    ///
    /// 所以这条的断言不是"分支内容对不对"，而是**同一个池上预览与真删的候选逐字相等**，
    /// 且这个池刻意造在上限边界上（201），只有顺序错了才会分叉。
    ///
    /// 回退即 FAIL：把 `if dry_run || candidates.is_empty()` 那个 return 块移到
    /// `candidates.sort_unstable()` 之前 → 预览候选变 201 个，第一条断言失败。
    #[test]
    fn truncation_happens_before_dry_run_early_return() {
        let n = MAX_CLEANUP_DISABLED_IDS as u64 + 1;
        let creds: Vec<KiroCredentials> = (1..=n)
            .map(|i| {
                mk(
                    i,
                    "api_key",
                    None,
                    true,
                    Some(DisabledReason::QuotaExceeded),
                )
            })
            .collect();
        let svc = mk_service(creds);

        let preview = svc.cleanup_disabled_credentials(true);
        assert_eq!(
            preview.candidates.len(),
            MAX_CLEANUP_DISABLED_IDS,
            "预览也必须先截断：报 201 个而真删 200 个，预览就骗人了"
        );
        assert_eq!(
            preview
                .skipped
                .iter()
                .filter(|s| s.reason == CLEANUP_SKIP_OVER_LIMIT)
                .map(|s| s.id)
                .collect::<Vec<_>>(),
            vec![n],
            "预览必须把'留给下一次'的那条也标出来（否则用户不知道还得再点一次）"
        );
        assert_eq!(
            svc.token_manager.total_count(),
            n as usize,
            "预览不得动池子"
        );

        // 同一个池、同一段筛选 ⇒ 真删的候选与预览逐字相等。这才是顺序正确的可观测证据。
        let real = svc.cleanup_disabled_credentials(false);
        assert_eq!(
            real.candidates, preview.candidates,
            "预览与真删的候选必须逐字相同（上限边界上尤其如此）"
        );
        assert_eq!(real.deleted, MAX_CLEANUP_DISABLED_IDS);
    }

    /// `disabled_total` 的恒等式：`== candidates.len() + skipped.len()`，**含** over_limit 那批。
    ///
    /// 锁的是 `types.rs` 上那句文档（原注释写"非 over_limit 的条数"，与实现不符）。
    /// 前端拿它当"池里有多少禁用号"的分母，少算一批会显示错的数。
    ///
    /// 回退即 FAIL：把实现改成注释描述的样子（`disabled_total` 减去 over_limit 条数）
    /// → 第二条断言失败。
    #[test]
    fn disabled_total_counts_every_disabled_credential_including_over_limit() {
        let n = MAX_CLEANUP_DISABLED_IDS as u64 + 3;
        let creds: Vec<KiroCredentials> = (1..=n)
            .map(|i| {
                // 混入两条被排除的，保证恒等式不是"candidates 恰好等于全部"的巧合
                match i % 100 {
                    7 => mk(
                        i,
                        "custom_api",
                        Some("https://relay.invalid/v1"),
                        true,
                        Some(DisabledReason::Manual),
                    ),
                    _ => mk(
                        i,
                        "api_key",
                        None,
                        true,
                        Some(DisabledReason::QuotaExceeded),
                    ),
                }
            })
            .collect();
        let svc = mk_service(creds);

        let resp = svc.cleanup_disabled_credentials(true);
        assert_eq!(resp.disabled_total, n as usize, "池里所有禁用号都要计入");
        assert_eq!(
            resp.disabled_total,
            resp.candidates.len() + resp.skipped.len(),
            "恒等式：每个禁用号必然落进 candidates 或 skipped 之一"
        );
        assert!(
            resp.skipped
                .iter()
                .any(|s| s.reason == CLEANUP_SKIP_OVER_LIMIT),
            "这个池刻意超上限，必须真触发 over_limit（否则本条测了个空）"
        );
    }

    /// 空池 / 全是未禁用号：安静返回零，不报错也不删。
    #[test]
    fn nothing_to_clean_is_a_quiet_zero() {
        let svc = mk_service(vec![mk(1, "api_key", None, false, None)]);
        let resp = svc.cleanup_disabled_credentials(false);
        assert_eq!(resp.disabled_total, 0);
        assert!(resp.candidates.is_empty());
        assert!(resp.skipped.is_empty(), "未禁用号不进 skipped（否则噪音）");
        assert_eq!(resp.deleted, 0);
        assert_eq!(svc.token_manager.total_count(), 1);
    }

    /// 请求体契约：`{}` / 缺体 / `{"dryRun":true}` 三种都得能解。
    ///
    /// 回退即 FAIL：去掉 `#[serde(default)]` → 第一条（`{}`）解析失败，
    /// 而"不带任何参数直接清理"正是最常见用法。
    #[test]
    fn request_body_parses_camel_case_and_defaults_to_real_delete() {
        use super::super::types::CleanupDisabledRequest;
        let empty: CleanupDisabledRequest = serde_json::from_str("{}").expect("空体应能解析");
        assert!(
            !empty.dry_run,
            "缺字段必须是真删（与既有 force 同款保守语义）"
        );
        let preview: CleanupDisabledRequest =
            serde_json::from_str(r#"{"dryRun":true}"#).expect("camelCase 应能解析");
        assert!(preview.dry_run);
    }
}

#[cfg(test)]
mod ksk_clean_tests {
    use super::*;

    /// 清洗：引号/逗号/首尾空白/`ksk_` 前的噪声都要剥掉，干净的 key 原样保留。
    #[test]
    fn clean_ksk_api_key_strips_paste_noise() {
        // 干净 key 原样
        assert_eq!(clean_ksk_api_key("ksk_abc123"), Some("ksk_abc123".into()));
        // 首尾空白
        assert_eq!(clean_ksk_api_key("  ksk_abc123  "), Some("ksk_abc123".into()));
        // 整段 `"key: ksk_xxx"` 粘贴（k2cc 实测踩过的形态）
        assert_eq!(
            clean_ksk_api_key("\"key: ksk_abc123\""),
            Some("ksk_abc123".into())
        );
        // 单引号 + 逗号包裹
        assert_eq!(clean_ksk_api_key("'ksk_abc123',"), Some("ksk_abc123".into()));
        // 🔴 回归：`"key: 'ksk_abc123'"`（前缀 + 内层单引号）→ 之前尾引号残留成 `ksk_abc123'`
        assert_eq!(
            clean_ksk_api_key("\"key: 'ksk_abc123'\""),
            Some("ksk_abc123".into())
        );
        // `ksk_` 前有任意前缀 → 从 ksk_ 起截取（与 k2cc 逐字一致：`s[i..].trim()`，
        // 只去前缀噪声，`ksk_` 之后的内容原样保留）
        assert_eq!(
            clean_ksk_api_key("some noise here ksk_abc123 trailing"),
            Some("ksk_abc123 trailing".into())
        );
        // 非 ksk_ 值：原样（不透写，不改行为）
        assert_eq!(clean_ksk_api_key("refresh_token_value"), Some("refresh_token_value".into()));
        // 纯噪声/空白 → None（交给下游「必须提供 kiroApiKey」报错，与 k2cc 同语义）
        assert_eq!(clean_ksk_api_key("   "), None);
        assert_eq!(clean_ksk_api_key("\"\","), None);
    }

    /// Kiro-Go `ksk_key|region`：只拆恰好一段 `|` + 白名单 region。
    #[test]
    fn clean_ksk_api_key_splits_pipe_region() {
        assert_eq!(
            clean_ksk_api_key("ksk_abc123|eu-central-1"),
            Some("ksk_abc123".into())
        );
        assert_eq!(
            ksk_region_suffix("ksk_abc123|eu-central-1").as_deref(),
            Some("eu-central-1")
        );
        assert_eq!(
            ksk_region_suffix("\"key: ksk_abc123|eu-central-1\"").as_deref(),
            Some("eu-central-1")
        );
        // 未知 region / 多段 `|` / 非 ksk_：不拆
        assert_eq!(
            clean_ksk_api_key("ksk_abc123|not-a-region"),
            Some("ksk_abc123|not-a-region".into())
        );
        assert!(ksk_region_suffix("ksk_abc123|not-a-region").is_none());
        assert_eq!(
            clean_ksk_api_key("ksk_ab|c|eu-central-1"),
            Some("ksk_ab|c|eu-central-1".into())
        );
        assert!(ksk_region_suffix("refresh_token_value|eu-central-1").is_none());
        assert_eq!(
            clean_ksk_api_key("ksk_abc123| eu-central-1 "),
            Some("ksk_abc123".into())
        );
    }

    /// `|region` 只在 `api_region` 为空时写入；请求已带则保留。
    #[test]
    fn clean_ksk_apply_pipe_region_only_when_api_region_empty() {
        let mut req = AddCredentialRequest {
            kiro_api_key: Some("ksk_abc123|eu-central-1".into()),
            ..Default::default()
        };
        apply_ksk_region_suffix(&mut req);
        assert_eq!(req.api_region.as_deref(), Some("eu-central-1"));
        req.kiro_api_key = req.kiro_api_key.as_deref().and_then(clean_ksk_api_key);
        assert_eq!(req.kiro_api_key.as_deref(), Some("ksk_abc123"));

        let mut req = AddCredentialRequest {
            kiro_api_key: Some("ksk_abc123|eu-central-1".into()),
            api_region: Some("us-east-1".into()),
            ..Default::default()
        };
        apply_ksk_region_suffix(&mut req);
        assert_eq!(req.api_region.as_deref(), Some("us-east-1"));

        let mut req = AddCredentialRequest {
            kiro_api_key: Some("ksk_abc123|eu-central-1".into()),
            api_region: Some("  ".into()),
            ..Default::default()
        };
        apply_ksk_region_suffix(&mut req);
        assert_eq!(req.api_region.as_deref(), Some("eu-central-1"));
    }

    /// ⭐ 源码级守卫：`add_credential_with_intent` 入口必须对 `req.kiro_api_key` 应用清洗。
    /// 回退即 FAIL：去掉清洗调用 → 本测试红。
    /// 批量导入（import_one_key → add_credential）也走本函数，故一条守卫钉住两条路径。
    #[test]
    fn add_credential_entry_applies_ksk_cleaning() {
        let src = include_str!("service.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let needle = "req.kiro_api_key.as_deref().and_then(clean_ksk_api_key)";
        assert!(
            prod.contains(needle),
            "add_credential_with_intent 入口必须清洗 kiro_api_key（ksk_ 截取 + 去噪声），\
             否则粘贴 `\"key: ksk_xxx\"` 会破坏去重与 region 探测"
        );
        let region_needle = format!("{}{}", "apply_ksk_region_suffix(", "&mut req)");
        assert!(
            prod.contains(&region_needle),
            "add_credential_with_intent 必须在清洗前把 ksk_|region 写入 api_region"
        );
    }
}

#[cfg(test)]
mod reprobe_quota_relogin_tests {
    //! POST /credentials/{id}/reprobe-region、/credentials/disable-quota-exceeded、
    //! /credentials/{id}/relogin 三个新端点的行为测试与源码守卫。
    //!
    //! 能纯逻辑测的（筛选、Skipped 处置、OAuth 校验、复活）用真 service 行为测；
    //! 需要真实上游的（NoUsableRegion/TokenDead/AccountThrottled 探测判决）用源码守卫锁
    //! 「失败分支绝不触碰禁用态」—— 本仓铁律：测试不依赖网络。

    use super::*;
    use crate::admin::types::ReprobeRegionResponse;

    /// 造一条凭据（对齐 cleanup_disabled_tests::mk 的形状）。
    fn mk(id: u64, auth_method: &str, disabled: bool, reason: Option<DisabledReason>) -> KiroCredentials {
        KiroCredentials {
            id: Some(id),
            auth_method: Some(auth_method.to_string()),
            kiro_api_key: match auth_method {
                "api_key" | "custom_api" => Some(format!("ksk_test_{id}")),
                _ => None,
            },
            // OAuth 类必须带 refresh_token（validate 路径要求；测试里不触发刷新，仅占位）
            refresh_token: match auth_method {
                "api_key" | "custom_api" => None,
                _ => Some(format!("rt-test-{id}")),
            },
            disabled,
            disabled_reason: reason,
            ..Default::default()
        }
    }

    fn mk_service(creds: Vec<KiroCredentials>) -> AdminService {
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                // 单凭据格式 ⇒ persist 是 no-op，测试只改内存。
                false,
            )
            .expect("构造 token manager"),
        );
        AdminService::new(tm, Vec::<String>::new())
    }

    fn balance(id: u64, remaining: f64) -> BalanceResponse {
        BalanceResponse {
            id,
            subscription_title: None,
            current_usage: 0.0,
            usage_limit: 100.0,
            remaining,
            usage_percentage: 0.0,
            next_reset_at: None,
            overage_enabled: false,
            overage_cap: 0.0,
            effective_limit: 100.0,
            stale: false,
            optimistic: false,
        }
    }

    fn disabled_of(svc: &AdminService, id: u64) -> (bool, Option<String>) {
        let snap = svc.token_manager.snapshot();
        let e = snap
            .entries
            .iter()
            .find(|e| e.id == id)
            .expect("凭据应在池中");
        (e.disabled, e.disabled_reason.clone())
    }

    // ---------------- reprobe-region ----------------

    /// Skipped（已带 region）→ 原样返回当前 api_region，不算失败。
    /// 探测判据 `needs_api_region_probe` 对带 region 的 api_key 号直接 Skipped，零网络。
    #[tokio::test]
    async fn reprobe_skipped_with_region_returns_current_region() {
        let mut cred = mk(1, "api_key", false, None);
        cred.api_region = Some("eu-central-1".to_string());
        let svc = mk_service(vec![cred]);
        let resp: ReprobeRegionResponse = svc.reprobe_api_region(1).await.expect("Skipped 不是失败");
        assert_eq!(resp.region.as_deref(), Some("eu-central-1"));
        assert!(resp.message.contains("无需探测"));
    }

    /// Skipped（OAuth 号，无 region 概念）→ region=None + 说明文案，仍算成功。
    #[tokio::test]
    async fn reprobe_skipped_oauth_returns_no_region() {
        let svc = mk_service(vec![mk(1, "social", false, None)]);
        let resp: ReprobeRegionResponse = svc.reprobe_api_region(1).await.expect("Skipped 不是失败");
        assert_eq!(resp.region, None);
        assert!(resp.message.contains("无需探测"));
    }

    /// 号不存在 → NotFound（错误路径，不能假装探测成功）。
    #[tokio::test]
    async fn reprobe_missing_credential_is_not_found() {
        let svc = mk_service(vec![]);
        let err = svc.reprobe_api_region(1).await.expect_err("不存在必须报错");
        assert!(matches!(err, AdminServiceError::NotFound { id: 1 }));
    }

    /// ⭐ 源码级守卫（承重）：探测失败判决（NoUsableRegion / TokenDead / AccountThrottled）
    /// 只能返错误，**绝不能**调用禁用处置 —— 服役号被禁会把好号打掉
    /// （启动回填教训，见 `probe_and_persist_api_region` 文档）。
    /// 行为测试测不到（三个失败判决都要真上游探测），故锁源码。
    #[test]
    fn reprobe_failure_arms_must_not_disable_credential() {
        let src = include_str!("service.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let marker = format!("pub async fn reprobe_api_region{}", "(");
        let start = prod.find(&marker).expect("reprobe_api_region 不应被改名");
        let body_end = prod[start..]
            .find("\n    pub ")
            .map(|i| i + start)
            .unwrap_or(prod.len());
        let body = &prod[start..body_end];
        // 三个失败判决必须各自成臂（归因错误消息能区分「探不了」与「探过不行」）。
        for arm in ["NoUsableRegion =>", "TokenDead =>", "AccountThrottled =>"] {
            assert!(
                body.contains(arm),
                "失败判决 {arm} 必须显式处置（缺失会静默漏分支）"
            );
        }
        // 本函数体里**不允许**出现任何禁用收口调用（拼接 needle 防自匹配）。
        let disable_call = format!("mark_region_probe_failed{}", "(");
        assert!(
            !body.contains(&disable_call),
            "服役号重探失败不得走禁用处置（mark_region_probe_failed 只属于上号路径）"
        );
        let set_disabled_call = format!(".set_disabled{}", "(");
        assert!(
            !body.contains(&set_disabled_call),
            "服役号重探失败不得 set_disabled（会把好号打掉）"
        );
    }

    // ---------------- disable-quota-exceeded ----------------

    /// 核心筛选：remaining<=0 且启用 → 禁；healthy（remaining>0）→ 不动。
    #[test]
    fn quota_exceeded_disables_only_exhausted_enabled() {
        let svc = mk_service(vec![
            mk(1, "api_key", false, None),
            mk(2, "api_key", false, None),
            mk(3, "api_key", true, Some(DisabledReason::Manual)),
        ]);
        let key1 = svc.balance_cache_key(1);
        let key2 = svc.balance_cache_key(2);
        let key3 = svc.balance_cache_key(3);
        svc.commit_fresh_balance(key1, balance(1, 0.0));
        svc.commit_fresh_balance(key2, balance(2, 42.5));
        svc.commit_fresh_balance(key3, balance(3, 0.0));

        let resp = svc.disable_quota_exceeded();
        assert_eq!(resp.disabled, 1);
        assert_eq!(resp.failed, 0);
        assert_eq!(resp.list, vec![1]);
        // #1 被禁且原因是额度用尽（面板可读，不是 Manual）。
        assert_eq!(disabled_of(&svc, 1), (true, Some("QuotaExceeded".to_string())));
        // #2 余额充足：不碰。 #3 已禁用：不是候选（幂等）。
        assert_eq!(disabled_of(&svc, 2), (false, None));
        assert_eq!(disabled_of(&svc, 3), (true, Some("Manual".to_string())));
    }

    /// 代挂号（custom_api）即使缓存显示超额也**绝不**代禁 —— 它的额度是中转站自己的。
    #[test]
    fn quota_exceeded_never_disables_custom_api() {
        let svc = mk_service(vec![mk(10, "custom_api", false, None)]);
        svc.commit_fresh_balance(svc.balance_cache_key(10), balance(10, -5.0));

        let resp = svc.disable_quota_exceeded();
        assert_eq!(resp.disabled, 0);
        assert_eq!(resp.list, Vec::<u64>::new());
        assert_eq!(disabled_of(&svc, 10), (false, None));
    }

    /// 无缓存 / 缓存未命中 → 不是候选（零上游，绝不触发余额查询）。
    #[test]
    fn quota_exceeded_ignores_uncached() {
        let svc = mk_service(vec![mk(1, "api_key", false, None)]);
        let resp = svc.disable_quota_exceeded();
        assert_eq!(resp.disabled, 0);
        assert_eq!(resp.list, Vec::<u64>::new());
        assert_eq!(disabled_of(&svc, 1), (false, None));
    }

    // ---------------- relogin ----------------

    /// OAuth 号复活：禁用 + 惩罚态清零 + 重新启用（失败计数复位、原因清空）。
    #[test]
    fn relogin_revives_oauth_credential() {
        let svc = mk_service(vec![mk(5, "idc", false, None)]);
        // 先造一个「惩罚态深」的号：额度耗尽禁用会把 failure_count 拉到阈值。
        svc.token_manager.report_quota_exhausted(5);
        assert_eq!(disabled_of(&svc, 5), (true, Some("QuotaExceeded".to_string())));

        svc.relogin_oauth(5).expect("OAuth 号复活应成功");
        let (disabled, reason) = disabled_of(&svc, 5);
        assert!(!disabled, "复活后必须重新启用");
        assert_eq!(reason, None, "复活后禁用原因必须清空");
        let snap = svc.token_manager.snapshot();
        let entry = snap.entries.iter().find(|e| e.id == 5).expect("号应在池中");
        assert_eq!(entry.failure_count, 0, "复活必须重置失败计数");
    }

    /// api_key 号拒绝复活（它没有 refreshToken 生命周期概念），custom_api 同理。
    #[test]
    fn relogin_rejects_api_key_and_custom_api() {
        let svc = mk_service(vec![
            mk(1, "api_key", false, None),
            mk(2, "custom_api", false, None),
        ]);
        let err = svc.relogin_oauth(1).expect_err("api_key 号必须拒绝");
        assert!(matches!(err, AdminServiceError::InvalidCredential(_)));
        let err = svc.relogin_oauth(2).expect_err("代挂号必须拒绝");
        assert!(matches!(err, AdminServiceError::InvalidCredential(_)));
        // 拒绝时不得动状态。
        assert_eq!(disabled_of(&svc, 1), (false, None));
        assert_eq!(disabled_of(&svc, 2), (false, None));
    }

    /// 号不存在 → NotFound。
    #[test]
    fn relogin_missing_credential_is_not_found() {
        let svc = mk_service(vec![]);
        let err = svc.relogin_oauth(99).expect_err("不存在必须报错");
        assert!(matches!(err, AdminServiceError::NotFound { id: 99 }));
    }

    // ---------------- 路由存在性守卫 ----------------

    /// 三个新端点必须挂在鉴权路由树内（路径 → handler 绑定，空白不敏感）。
    /// 回退即 FAIL：删掉任一 `.route(..)` → 前端 404 且编译/测试都不报。
    #[test]
    fn new_endpoints_are_wired_in_router() {
        let router = include_str!("router.rs");
        // ⚠️ 判据必须对空白不敏感（rustfmt 会把长 .route(..) 拆成多行），
        // 折叠空白后再比 —— 与 `api_region_setter_endpoint_is_wired` 同款写法。
        let compact: String = router.chars().filter(|c| !c.is_whitespace()).collect();
        // needle 运行时拼接：写成完整字面量会被 include_str! 读到自己而多算一处。
        let routes = [
            format!(
                "\"/credentials/{{id}}/reprobe-region\",post(reprobe_credential_region{}",
                ")"
            ),
            format!(
                "\"/credentials/disable-quota-exceeded\",post(disable_quota_exceeded{}",
                ")"
            ),
            format!("\"/credentials/{{id}}/relogin\",post(relogin_oauth{}", ")"),
            // 2026-08-11 对抗审查 m4：refresh-token 路由此前无守卫（漏注册不红）。
            format!(
                "\"/credentials/{{id}}/refresh-token\",put(update_credential_refresh_token{}",
                ")"
            ),
        ];
        for route in routes {
            assert!(
                compact.contains(&route),
                "新端点必须注册进鉴权路由树：{}",
                route
            );
        }
    }
}

#[cfg(test)]
mod kam_export_tests {
    //! GET /credentials/export-kam 的导出行为测试。
    //!
    //! 解密语义：at-rest 加密在启动加载期由 `CredentialsConfig::load` →
    //! `maybe_decrypt_to_string` 统一解密，内存凭据即明文；导出直接复用内存明文，
    //! 本模块构造明文凭据进内存，断言「导出 = 明文直通」（不经任何加解密）。

    use super::*;

    /// 造一条 OAuth 类凭据（带 refresh_token 才可能进 KAM 导出）。
    fn mk(id: u64, auth_method: &str, has_rt: bool, disabled: bool) -> KiroCredentials {
        KiroCredentials {
            id: Some(id),
            auth_method: Some(auth_method.to_string()),
            email: Some(format!("user{id}@example.com")),
            access_token: Some(format!("at-test-{id}")),
            refresh_token: if has_rt {
                Some(format!("rt-test-{id}"))
            } else {
                None
            },
            region: Some("eu-central-1".to_string()),
            machine_id: Some(format!("machine-{id}")),
            client_id: Some(format!("client-{id}")),
            client_secret: Some(format!("secret-{id}")),
            profile_arn: Some(format!("arn:aws:iam::1:role/r{id}")),
            expires_at: Some("2030-01-01T00:00:00Z".to_string()),
            priority: id as u32,
            disabled,
            ..Default::default()
        }
    }

    fn mk_service(creds: Vec<KiroCredentials>) -> AdminService {
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                // 单凭据格式 ⇒ persist 是 no-op，测试只改内存（同 reprobe 测试先例）。
                false,
            )
            .expect("构造 token manager"),
        );
        AdminService::new(tm, Vec::<String>::new())
    }

    /// 导出即内存明文直通：refreshToken / accessToken 与构造值逐字一致。
    /// 这是「at-rest 密文 → 明文出站」语义的落点（解密发生在加载期，此处零加解密）。
    #[test]
    fn export_includes_plaintext_tokens() {
        let svc = mk_service(vec![mk(1, "social", true, false)]);
        let resp = svc.export_kam_credentials(None);
        assert_eq!(resp.accounts.len(), 1);
        let acc = &resp.accounts[0];
        assert_eq!(acc.refresh_token.as_deref(), Some("rt-test-1"));
        assert_eq!(acc.access_token.as_deref(), Some("at-test-1"));
        assert_eq!(acc.client_secret.as_deref(), Some("secret-1"));
    }

    /// 字段映射对齐 KAM 1.8.3+ 平铺格式；idp 复用本仓 social → Google 的既有推断。
    #[test]
    fn export_maps_kam_fields() {
        let mut cred = mk(1, "social", true, true);
        cred.region = None;
        cred.auth_region = None;
        cred.api_region = Some("ap-southeast-1".to_string());
        let svc = mk_service(vec![cred]);
        let acc = &svc.export_kam_credentials(None).accounts[0];
        assert_eq!(acc.email.as_deref(), Some("user1@example.com"));
        assert_eq!(acc.idp.as_deref(), Some("Google"));
        assert_eq!(acc.auth_method.as_deref(), Some("social"));
        assert_eq!(acc.status.as_deref(), Some("disabled"));
        // region 回退链（MINOR-3 修正）：本用例 region/auth_region 均缺 → 落第三级
        // api_region（effective_upstream_region 与导出同源，实测覆盖三级链末端）
        assert_eq!(acc.region.as_deref(), Some("ap-southeast-1"));
        assert_eq!(acc.machine_id.as_deref(), Some("machine-1"));
        assert_eq!(acc.client_id.as_deref(), Some("client-1"));
        assert_eq!(acc.profile_arn.as_deref(), Some("arn:aws:iam::1:role/r1"));
        assert_eq!(acc.expires_at.as_deref(), Some("2030-01-01T00:00:00Z"));
    }

    /// region 回退链：region 为空时依次落到 auth_region / api_region。
    #[test]
    fn export_region_falls_back_through_chain() {
        let mut cred = mk(2, "social", true, false);
        cred.region = None;
        cred.auth_region = Some("us-west-2".to_string());
        cred.api_region = Some("ap-northeast-1".to_string());
        let svc = mk_service(vec![cred]);
        let acc = &svc.export_kam_credentials(None).accounts[0];
        assert_eq!(acc.region.as_deref(), Some("us-west-2"));
    }

    /// 无 refreshToken 的号（api_key / custom_api）KAM 无对应字段 → 整条跳过。
    #[test]
    fn export_skips_credentials_without_refresh_token() {
        let mut api = mk(1, "api_key", false, false);
        api.kiro_api_key = Some("ksk_test_1".to_string());
        let mut passthrough = mk(2, "custom_api", false, false);
        passthrough.api_key = Some("sk-pt-2".to_string());
        let svc = mk_service(vec![api, passthrough, mk(3, "social", true, false)]);
        let resp = svc.export_kam_credentials(None);
        assert_eq!(resp.accounts.len(), 1);
        assert_eq!(resp.accounts[0].email.as_deref(), Some("user3@example.com"));
    }

    /// 空池 → accounts 空数组，不报错。
    #[test]
    fn export_empty_pool_returns_empty_accounts() {
        let svc = mk_service(vec![]);
        let resp = svc.export_kam_credentials(None);
        assert!(resp.accounts.is_empty());
    }

    /// ids 过滤：仅导出集合内的 ID。
    #[test]
    fn export_respects_id_filter() {
        let svc = mk_service(vec![
            mk(1, "social", true, false),
            mk(2, "social", true, false),
            mk(3, "social", true, false),
        ]);
        let filter: HashSet<u64> = [1u64, 3].into_iter().collect();
        let resp = svc.export_kam_credentials(Some(&filter));
        let emails: Vec<&str> = resp
            .accounts
            .iter()
            .filter_map(|a| a.email.as_deref())
            .collect();
        assert_eq!(emails, vec!["user1@example.com", "user3@example.com"]);
    }

    /// 按 priority 升序（与 UI 列表一致）。
    #[test]
    fn export_sorted_by_priority() {
        let mut low = mk(1, "social", true, false);
        low.priority = 10;
        let mut high = mk(2, "social", true, false);
        high.priority = 1;
        let svc = mk_service(vec![low, high]);
        let resp = svc.export_kam_credentials(None);
        let emails: Vec<&str> = resp
            .accounts
            .iter()
            .filter_map(|a| a.email.as_deref())
            .collect();
        assert_eq!(emails, vec!["user2@example.com", "user1@example.com"]);
    }

    /// 序列化契约：camelCase 键名 + 平铺 refreshToken + 无 null 字段（KAM 导入器判型要求）。
    #[test]
    fn export_serialization_contract() {
        let svc = mk_service(vec![mk(1, "social", true, false)]);
        let json = serde_json::to_value(svc.export_kam_credentials(None)).expect("序列化应成功");
        let obj = json.as_object().expect("顶层应为对象");
        assert_eq!(obj["version"], "1.8.3");
        assert!(obj["exportedAt"].as_str().is_some_and(|s| !s.is_empty()));
        let acc = obj["accounts"][0].as_object().expect("账号应为对象");
        assert!(acc.contains_key("refreshToken"), "KAM 平铺契约要求 refreshToken 直接在账号对象上");
        assert!(!acc.contains_key("refresh_token"), "键名必须是 camelCase");
        for (k, v) in acc {
            assert!(!v.is_null(), "字段 {k} 不应为 null（None 字段应省略）");
        }
    }

    /// 路由存在性守卫：export-kam 端点必须挂在鉴权路由树内。
    /// 回退即 FAIL：删掉 `.route(..)` → 前端 404 且编译/测试都不报。
    #[test]
    fn export_kam_endpoint_is_wired_in_router() {
        let router = include_str!("router.rs");
        // 判据对空白不敏感（rustfmt 会把长 .route(..) 拆多行），折叠空白后再比。
        let compact: String = router.chars().filter(|c| !c.is_whitespace()).collect();
        // needle 运行时拼接：写成完整字面量会被 include_str! 读到自己而多算一处。
        let route = format!(
            "\"/credentials/export-kam\",get(export_kam_credentials{}",
            ")"
        );
        assert!(
            compact.contains(&route),
            "export-kam 端点必须注册进鉴权路由树：{route}"
        );
    }
}

#[cfg(test)]
mod config_write_tests {
    //! 配置写路径（update_config / import_config）的健壮性测试：
    //! 写锁结构守卫、备份轮换行为、字段级 diff 审计行为。
    use super::*;

    /// 测试用临时目录（Drop 时自动清理；panic 时由 OS 留着，带 pid 不与其他进程撞）。
    struct TempDir(std::path::PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 🔴 承重：两个配置写路径必须持同一把写锁，且持锁先于临界区起点。
    ///
    /// 根除的是 lost update：并发两个 `update_config` 各自 load 后交错 save，
    /// 后完成者会把先完成者的改动整体覆盖（都改不同字段时静默吞掉先写字段）。
    /// 守卫锁死两件事：
    /// 1. `update_config` 包装函数必须先持锁、再委托 locked 实现（锁保护得住整个临界区）；
    /// 2. `import_config` 必须先持锁、再写盘（save 是临界区终点）。
    ///
    /// 回退即 FAIL：把持锁语句从函数里挪走 / 移到委托调用之后 / 换锁名。
    #[test]
    fn config_write_lock_covers_both_write_paths() {
        let src = include_str!("service.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        // needle 运行时拼接：写成完整字面量会被 include_str! 读到自己而多算一处。
        let lock = format!("config_write{}", "_lock.lock()");
        let count = prod.matches(&lock).count();
        assert!(
            count >= 2,
            "两个写路径必须各持一次锁（当前 {count} 处）"
        );

        // update_config 包装函数：持锁在委托调用之前
        let update_fn = format!("pub fn update_config{}", "(");
        let uf = prod
            .find(&update_fn)
            .expect("update_config 包装函数不该被改名");
        let body_end = prod[uf..]
            .find("\n    pub fn ")
            .map(|i| i + uf)
            .unwrap_or(prod.len());
        let body = &prod[uf..body_end];
        let li = body
            .find(&lock)
            .expect("update_config 必须先持写锁，否则并发 save 互相覆盖");
        let call = format!("self.update_config{}", "_locked(req)");
        let ci = body
            .find(&call)
            .expect("update_config 必须委托给锁内实现");
        assert!(li < ci, "持锁必须在委托调用之前，否则保护不到临界区");

        // import_config：持锁在写盘之前
        let import_fn = format!("pub fn import_config{}", "(");
        let ii = prod
            .find(&import_fn)
            .expect("import_config 不该被改名");
        let iend = prod[ii..]
            .find("\n    pub fn ")
            .map(|i| i + ii)
            .unwrap_or(prod.len());
        let ibody = &prod[ii..iend];
        // 写盘调用是跨行链式（`imported\n .save()`），折叠空白再比（同 router 守卫写法）。
        let icompact: String = ibody.chars().filter(|c| !c.is_whitespace()).collect();
        let il = icompact
            .find(&lock)
            .expect("import_config 必须先持写锁，否则与并发更新互相覆盖");
        let save = format!("imported{}", ".save()");
        let si = icompact
            .find(&save)
            .expect("import_config 必须写盘保存");
        assert!(il < si, "持锁必须在写盘之前，否则并发导入相互覆盖");
    }

    /// 备份轮换保留 3 代（.bak 最新 → .bak.1 → .bak.2 最旧），当前文件原位不动。
    #[test]
    fn rotate_config_backup_keeps_three_generations() {
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_cfg_bak_{}",
            std::process::id()
        )));
        let _ = std::fs::remove_dir_all(&dir.0);
        std::fs::create_dir_all(&dir.0).unwrap();
        let cfg = dir.0.join("config.json");

        std::fs::write(&cfg, b"v0").unwrap();
        rotate_config_backup(&cfg);
        assert_eq!(
            std::fs::read_to_string(cfg.with_extension("json.bak")).unwrap(),
            "v0"
        );

        std::fs::write(&cfg, b"v1").unwrap();
        rotate_config_backup(&cfg);
        assert_eq!(
            std::fs::read_to_string(cfg.with_extension("json.bak.1")).unwrap(),
            "v0"
        );
        assert_eq!(
            std::fs::read_to_string(cfg.with_extension("json.bak")).unwrap(),
            "v1"
        );

        std::fs::write(&cfg, b"v2").unwrap();
        rotate_config_backup(&cfg);
        assert_eq!(
            std::fs::read_to_string(cfg.with_extension("json.bak.2")).unwrap(),
            "v0"
        );
        assert_eq!(
            std::fs::read_to_string(cfg.with_extension("json.bak.1")).unwrap(),
            "v1"
        );
        assert_eq!(
            std::fs::read_to_string(cfg.with_extension("json.bak")).unwrap(),
            "v2"
        );
        // 当前文件保持最新内容未被轮换动过
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "v2");
    }

    /// 字段级 diff 审计：只记字段名/路径，绝不记字段值（敏感字段的值因此不会进日志）。
    #[test]
    fn diff_json_fields_reports_names_not_values() {
        // 值变了 → 记字段名；旧值/新值本身绝不出现
        let old = serde_json::json!({ "apiKey": "secret-old", "port": 8080 });
        let new = serde_json::json!({ "apiKey": "secret-new", "port": 8080 });
        let d = diff_json_fields(&old, &new);
        assert_eq!(d, vec!["apiKey".to_string()]);
        assert!(
            d.iter().all(|p| !p.contains("secret")),
            "diff 结果只能有字段名，不能夹带任何字段值"
        );

        // 完全相同 → 空
        let same = serde_json::json!({ "a": 1, "b": { "c": [1, 2] } });
        assert!(diff_json_fields(&same, &same).is_empty());

        // 新增/删除键 → 记路径
        let a = serde_json::json!({ "x": 1 });
        let b = serde_json::json!({ "x": 1, "y": 2 });
        assert_eq!(diff_json_fields(&a, &b), vec!["y".to_string()]);
        assert_eq!(diff_json_fields(&b, &a), vec!["y".to_string()]);

        // 嵌套对象 → 递归记完整路径
        let o1 = serde_json::json!({ "outer": { "inner": 1 } });
        let o2 = serde_json::json!({ "outer": { "inner": 2 } });
        assert_eq!(
            diff_json_fields(&o1, &o2),
            vec!["outer.inner".to_string()]
        );

        // 整块结构替换（数组 / 标量类型变）→ 记顶层路径
        let m1 = serde_json::json!({ "arr": [1, 2] });
        let m2 = serde_json::json!({ "arr": [3] });
        assert_eq!(diff_json_fields(&m1, &m2), vec!["arr".to_string()]);
    }

    // ============ update_config_locked 行为测试（2026-08-15 补）============
    //
    // 此前只有源码守卫（旧注释自称「单测无法真跑 update_config（需要真实 TokenManager +
    // 磁盘 config），故用源码断言」——前提其实不成立）：tmp 目录 + 写盘 config.json +
    // `Config::load` 带回 config_path 即可构造真实可跑的更新链路
    // （load → 逐字段改 → save → reload_config）。这批测试钉的是守卫钉不住的行为：
    // 字段 merge 不丢、restart_fields 累积、非法值整单拒绝且零写盘、
    // TIER1/TIER3 立即生效文案、error_messages per-key merge。

    /// 构造带磁盘 config.json 的 AdminService。
    ///
    /// seed 按测试意图写初始配置，整份写盘后经 `Config::load` 读回
    /// （与 update_config_locked 内部同一条加载路径），config_path 因此有值。
    fn svc_with_disk_config(
        dir: &TempDir,
        seed: impl FnOnce(&mut crate::model::config::Config),
    ) -> (Arc<AdminService>, std::path::PathBuf) {
        let path = dir.0.join("config.json");
        // 目录必须显式创建（TempDir 只登记路径不建目录）：缺了这行
        // fs::write 直接 NotFound panic，5 个 update_config 测试全红。
        std::fs::create_dir_all(&dir.0).unwrap();
        let mut cfg = crate::model::config::Config::default();
        seed(&mut cfg);
        std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
        let loaded = crate::model::config::Config::load(&path).expect("初始配置必须可加载");
        let tm = Arc::new(
            MultiTokenManager::new(loaded, vec![], None, None, false).expect("构造 token manager"),
        );
        (Arc::new(AdminService::new(tm, Vec::<String>::new())), path)
    }

    fn disk_config_json(path: &std::path::Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap())
            .expect("磁盘配置必须是合法 JSON")
    }

    /// 🔴 承重：改一个字段 → **只**改那一个，其余字段保持磁盘原值（merge 不丢）；
    /// 需重启字段按代码顺序累积进 restart_fields。
    ///
    /// 回退即 FAIL：把任一字段的写盘漏掉（「存了盘但读旧值」那类接线缺陷），
    /// 或 restart_fields 不按提交顺序 push（面板展示顺序错乱）。
    #[test]
    fn update_config_restart_fields_accumulate_and_unsubmitted_fields_preserved() {
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_upd_restart_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            c.host = "old.example.com".to_string();
            c.port = 8080;
            c.region = "us-east-1".to_string();
        });

        let resp = svc
            .update_config(UpdateConfigRequest {
                host: Some("new.example.com".to_string()),
                port: Some(9090),
                ..Default::default()
            })
            .expect("改 host+port 应成功");

        assert!(resp.restart_required, "host/port 都是重启字段");
        assert_eq!(
            resp.restart_fields,
            vec!["host".to_string(), "port".to_string()],
            "restart_fields 必须按代码顺序累积"
        );
        assert!(
            resp.message.contains("2 个字段"),
            "文案必须报 2 个字段需重启，实际: {}",
            resp.message
        );

        let disk = disk_config_json(&path);
        assert_eq!(disk["host"], "new.example.com", "提交的字段必须落盘");
        assert_eq!(disk["port"], 9090, "提交的字段必须落盘");
        assert_eq!(
            disk["region"], "us-east-1",
            "未提交的字段必须保持磁盘原值（merge 不丢）"
        );
    }

    /// 🔴 承重：非法值整单拒绝（Err），且**拒绝发生在写盘之前**——磁盘零改动。
    ///
    /// 覆盖四类校验：空串清洗后拒绝（host）、端口 0 拒绝（port）、
    /// 值域白名单拒绝（absorb_exhausted_status 只认 429/503）、枚举拒绝
    /// （load_balancing_mode 只认 priority/balanced）。
    ///
    /// 回退即 FAIL：把任一校验挪到 save 之后（拒绝但已落盘），或删掉任一校验，
    /// 对应断言失败。
    #[test]
    fn update_config_rejects_invalid_values_without_touching_disk() {
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_upd_reject_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            c.host = "h1".to_string();
            c.port = 8080;
        });
        let baseline = std::fs::read(&path).unwrap();

        let cases = [
            UpdateConfigRequest {
                host: Some("   ".to_string()),
                ..Default::default()
            },
            UpdateConfigRequest {
                port: Some(0),
                ..Default::default()
            },
            UpdateConfigRequest {
                upstream_retry_absorb_exhausted_status: Some(999),
                ..Default::default()
            },
            UpdateConfigRequest {
                load_balancing_mode: Some("bogus".to_string()),
                ..Default::default()
            },
        ];
        for req in cases {
            let err = svc
                .update_config(req)
                .expect_err("非法值必须整单拒绝");
            assert!(
                matches!(err, AdminServiceError::InvalidCredential(_)),
                "非法值拒绝必须用 InvalidCredential，实际: {err:?}"
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                baseline,
                "拒绝时必须零写盘（校验要先于 save）"
            );
        }
    }

    /// TIER1/TIER3 字段：保存后立即生效，不进 restart_fields、回「无需重启」。
    ///
    /// 用透传模拟缓存（TIER3 + setter 镜像）与吸收层开关（无 setter、只靠 reload_config
    /// 的 OR 链）各代表一类：两类都必须回「立即生效」——回「需重启」就是把热更字段
    /// 误分类的接线缺陷。
    #[test]
    fn update_config_hot_fields_report_immediate_effect_without_restart() {
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_upd_hot_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            c.mock_cache_enabled = false;
            c.upstream_retry_absorb_enabled = false;
        });

        let resp = svc
            .update_config(UpdateConfigRequest {
                mock_cache_enabled: Some(true),
                upstream_retry_absorb_enabled: Some(true),
                ..Default::default()
            })
            .expect("热更字段应成功");
        assert!(!resp.restart_required, "热更字段不得要求重启");
        assert!(resp.restart_fields.is_empty());
        assert!(
            resp.message.contains("立即生效"),
            "热更字段必须回「立即生效」，实际: {}",
            resp.message
        );

        let disk = disk_config_json(&path);
        assert_eq!(disk["mockCacheEnabled"], true, "TIER3 字段必须落盘");
        assert_eq!(disk["upstreamRetryAbsorbEnabled"], true, "吸收层开关必须落盘");
    }

    /// 🔴 承重：userKey（apiKey）轮换走 `auth_keys` setter **即时生效、无需重启**。
    ///
    /// 旧行为：apiKey 进 restart_fields、面板提示「需重启」——重启会掐断在途流式请求。
    /// 现在应回「立即生效」且 auth_keys 立刻按新 key 判定（旧 key 立即失效）。
    ///
    /// ⚠️ auth_keys 是进程级全局 cell：本用例必须持 `auth_keys::test_serial()` 全程，
    /// 否则并行的其他用例（构造 AppState/AdminState 或改 key）会覆写同一份全局状态。
    /// 先播旧 key 模拟 main.rs 启动播种，再经 update_config 轮换 → 断言旧失效/新生效。
    #[test]
    fn update_config_user_key_hot_swaps_without_restart() {
        let _g = crate::common::auth_keys::test_serial();
        crate::common::auth_keys::set_user_key("sk-old")
            .expect("启动播种（模拟 main.rs）不应失败");
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_upd_ukey_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            c.api_key = Some("sk-old".to_string());
        });
        assert!(
            crate::common::auth_keys::user_key_matches("sk-old"),
            "前置：播种后旧 key 应生效（模拟真实启动状态）"
        );

        let resp = svc
            .update_config(UpdateConfigRequest {
                api_key: Some("sk-new".to_string()),
                ..Default::default()
            })
            .expect("轮换 apiKey 应成功");
        assert!(!resp.restart_required, "apiKey 轮换不得要求重启");
        assert!(resp.restart_fields.is_empty(), "apiKey 不再进 restart_fields");
        assert!(
            resp.message.contains("立即生效"),
            "apiKey 轮换必须回「立即生效」，实际: {}",
            resp.message
        );

        // 鉴权活真相源立刻按新 key 判定：旧 key 失效、新 key 通过（热更定义）。
        assert!(
            crate::common::auth_keys::user_key_matches("sk-new"),
            "热更后新 apiKey 必须通过"
        );
        assert!(
            !crate::common::auth_keys::user_key_matches("sk-old"),
            "热更后旧 apiKey 必须立即失效"
        );

        let disk = disk_config_json(&path);
        assert_eq!(disk["apiKey"], "sk-new", "apiKey 必须落盘");
    }

    /// 承重：adminApiKey 轮换同样即时生效、无需重启。
    ///
    /// 语义上 admin key 是**新字段**（此前 UpdateConfigRequest 根本没有它，只能手改
    /// config.json + 重启）；现在走与 userKey 同款 setter 热更。自锁风险见字段注释。
    #[test]
    fn update_config_admin_key_hot_swaps_without_restart() {
        let _g = crate::common::auth_keys::test_serial();
        crate::common::auth_keys::set_admin_key("adm-old")
            .expect("启动播种（模拟 main.rs）不应失败");
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_upd_akey_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            c.admin_api_key = Some("adm-old".to_string());
        });
        assert!(
            crate::common::auth_keys::admin_key_matches("adm-old"),
            "前置：播种后旧 key 应生效"
        );

        let resp = svc
            .update_config(UpdateConfigRequest {
                admin_api_key: Some("adm-new".to_string()),
                ..Default::default()
            })
            .expect("轮换 adminApiKey 应成功");
        assert!(!resp.restart_required, "adminApiKey 轮换不得要求重启");
        assert!(resp.restart_fields.is_empty());
        assert!(
            resp.message.contains("立即生效"),
            "adminApiKey 轮换必须回「立即生效」，实际: {}",
            resp.message
        );
        assert!(
            crate::common::auth_keys::admin_key_matches("adm-new"),
            "热更后新 adminApiKey 必须通过"
        );
        assert!(
            !crate::common::auth_keys::admin_key_matches("adm-old"),
            "热更后旧 adminApiKey 必须立即失效"
        );
        assert_eq!(disk_config_json(&path)["adminApiKey"], "adm-new", "必须落盘");
    }

    /// 空/空白 key 传空串 = 不改（防把手动写入 fail-closed 的意图和「手滑存空」混为一谈）。
    ///
    /// 只提交空白 apiKey/adminApiKey 时：不报错、不落盘、鉴权仍走旧 key（绝不清成空串，
    /// 清空 = fail-open 敞口；真正关闭通道的意图在 auth_keys 层由 setter 拒空兜底）。
    #[test]
    fn update_config_blank_key_is_ignored_not_wiped() {
        let _g = crate::common::auth_keys::test_serial();
        crate::common::auth_keys::set_user_key("sk-keep")
            .expect("启动播种（模拟 main.rs）不应失败");
        crate::common::auth_keys::set_admin_key("adm-keep")
            .expect("启动播种（模拟 main.rs）不应失败");
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_upd_blank_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            c.api_key = Some("sk-keep".to_string());
            c.admin_api_key = Some("adm-keep".to_string());
        });

        let resp = svc
            .update_config(UpdateConfigRequest {
                api_key: Some("   ".to_string()),
                admin_api_key: Some("".to_string()),
                ..Default::default()
            })
            .expect("空白 key 应被忽略而非报错");
        assert!(
            resp.message.contains("无改动"),
            "空白 key 不算改动，实际: {}",
            resp.message
        );
        assert!(
            crate::common::auth_keys::user_key_matches("sk-keep"),
            "空白 key 不得清掉现有 apiKey"
        );
        assert!(
            crate::common::auth_keys::admin_key_matches("adm-keep"),
            "空白 key 不得清掉现有 adminApiKey"
        );
        let disk = disk_config_json(&path);
        assert_eq!(disk["apiKey"], "sk-keep");
        assert_eq!(disk["adminApiKey"], "adm-keep");
    }

    /// 🔴 承重：key 轮换与热字段**同批**提交时，reload_config 不得覆盖新 key。
    ///
    /// reload_config（token_manager）会把 apiKey/adminApiKey 这类 restart-only 字段
    /// 用 ArcSwap 旧值**钉回启动值**（split-brain 防护），鉴权却读 auth_keys 活单元——
    /// 所以 setter 必须放在 reload_config **之后**、以新值为准。本用例强制走
    /// mock_cache_enabled（TIER3 热字段 → 触发 reload_config）同批改 apiKey，
    /// 断言 reload 后 auth_keys 仍是新值：若有人把 setter 挪到 reload 之前或删了接线，
    /// 这里会当场红。
    #[test]
    fn update_config_key_survives_batched_hot_reload() {
        let _g = crate::common::auth_keys::test_serial();
        crate::common::auth_keys::set_user_key("sk-old")
            .expect("启动播种（模拟 main.rs）不应失败");
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_upd_seq_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            c.api_key = Some("sk-old".to_string());
            c.mock_cache_enabled = false;
        });

        let resp = svc
            .update_config(UpdateConfigRequest {
                api_key: Some("sk-new".to_string()),
                // 热字段：确保本次更新触发 reload_config（正踩「顺序坑」的场景）。
                mock_cache_enabled: Some(true),
                ..Default::default()
            })
            .expect("key + 热字段同批应成功");
        assert!(!resp.restart_required);
        assert!(
            crate::common::auth_keys::user_key_matches("sk-new"),
            "reload_config 之后 setter 必须以新值为准，旧 key 不得复活"
        );
        assert!(
            !crate::common::auth_keys::user_key_matches("sk-old"),
            "reload 不得把钉回的旧启动值当成鉴权真值"
        );
        assert_eq!(disk_config_json(&path)["apiKey"], "sk-new");
    }

    /// 源码守卫：key 热更接线 + 「setter 必须在 reload_config 之后」的顺序不变量。
    ///
    /// 回退即 FAIL：
    /// - 删掉 update_config_locked 里的 set_user_key/set_admin_key 调用（接线断了，
    ///   key 轮换又退回重启生效）；
    /// - 把 setter 挪到 reload_config **之前**（reload 会把 key 钉回启动旧值，
    ///   顺序反了热更静默失效）。
    #[test]
    fn guard_update_config_seeds_auth_keys_after_reload() {
        let src = include_str!("service.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let compact: String = prod.chars().filter(|c| !c.is_whitespace()).collect();
        for needle in [
            "crate::common::auth_keys::set_user_key",
            "crate::common::auth_keys::set_admin_key",
        ] {
            assert!(
                compact.contains(needle),
                "update_config 必须调 {needle} 热更（删接线 = key 轮换退回重启生效）"
            );
        }
        let reload = compact
            .find("self.token_manager.reload_config()")
            .expect("update_config 必须保留 reload_config 调用");
        let set_user = compact
            .find("crate::common::auth_keys::set_user_key")
            .expect("userKey setter 接线不该消失");
        let set_admin = compact
            .find("crate::common::auth_keys::set_admin_key")
            .expect("adminKey setter 接线不该消失");
        assert!(
            reload < set_user && reload < set_admin,
            "key setter 必须放在 reload_config 之后（reload 会把 key 钉回启动值，\
             顺序反了热更被 reload 覆盖而静默失效）"
        );
    }

    /// 提交与磁盘相同的值 → 「无改动。」（不误报立即生效/需重启）。
    #[test]
    fn update_config_no_change_reports_no_change() {
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_upd_none_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            c.host = "h1".to_string();
        });

        let resp = svc
            .update_config(UpdateConfigRequest {
                host: Some("h1".to_string()),
                ..Default::default()
            })
            .expect("同值提交应成功");
        assert_eq!(resp.message, "无改动。", "同值提交必须回「无改动。」");
        assert!(!resp.restart_required);
        assert_eq!(disk_config_json(&path)["host"], "h1", "同值提交磁盘不变");
    }

    /// 🔴 承重：error_messages 是 **per-key merge**——提交只更新提交的 key，
    /// 未提交的 key 保持磁盘原值；整表被校验拒绝时旧表保持（先校验再写盘）。
    ///
    /// 回退即 FAIL：把 merge 改成整表替换（`config.error_messages = em`），
    /// 未提交的 k2 会消失，断言失败。
    #[test]
    fn update_config_error_messages_merge_keeps_unsubmitted_keys() {
        use crate::model::error_messages::ErrorMessageOverride;
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_upd_errmsg_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            let mut table = HashMap::new();
            table.insert(
                "k1".to_string(),
                ErrorMessageOverride {
                    status: Some(429),
                    r#type: Some("rate_limit_error".to_string()),
                    message: Some("旧文案".to_string()),
                    retry_after_secs: Some(8),
                },
            );
            table.insert(
                "k2".to_string(),
                ErrorMessageOverride {
                    status: Some(500),
                    r#type: Some("api_error".to_string()),
                    message: Some("k2 保持".to_string()),
                    retry_after_secs: None,
                },
            );
            c.error_messages = table;
        });

        let mut submitted = HashMap::new();
        submitted.insert(
            "k1".to_string(),
            ErrorMessageOverride {
                status: Some(429),
                r#type: Some("rate_limit_error".to_string()),
                message: Some("新文案".to_string()),
                retry_after_secs: Some(8),
            },
        );
        svc.update_config(UpdateConfigRequest {
            error_messages: Some(submitted),
            ..Default::default()
        })
        .expect("合法 per-key 更新应成功");

        let disk = disk_config_json(&path);
        assert_eq!(
            disk["errorMessages"]["k1"]["message"], "新文案",
            "提交的 key 必须更新"
        );
        assert_eq!(
            disk["errorMessages"]["k2"]["message"], "k2 保持",
            "未提交的 key 必须保持（per-key merge，不是整表替换）"
        );

        // 整表被校验拒绝时旧表保持：提交一个非法 key → Err 且 k1 不被写坏。
        let mut bad = HashMap::new();
        bad.insert(
            "k1".to_string(),
            ErrorMessageOverride {
                status: Some(418),
                r#type: Some("api_error".to_string()),
                message: None,
                retry_after_secs: None,
            },
        );
        svc.update_config(UpdateConfigRequest {
            error_messages: Some(bad),
            ..Default::default()
        })
        .expect_err("非法错误码表必须整表拒绝");
        assert_eq!(
            disk_config_json(&path)["errorMessages"]["k1"]["message"], "新文案",
            "拒绝时必须保持旧表（先校验再写盘）"
        );
    }
}

#[cfg(test)]
mod update_refresh_token_tests {
    //! `update_refresh_token` 校验矩阵：截断拒 / 跨凭据重复拒 / 正常过（含自身原值重提交）。
    use super::*;

    fn mk_oauth_cred(id: u64, rt: &str) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.id = Some(id);
        c.auth_method = Some("oauth".to_string());
        c.refresh_token = Some(rt.to_string());
        c
    }

    fn mk_service(rt1: &str, rt2: &str) -> AdminService {
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![mk_oauth_cred(1, rt1), mk_oauth_cred(2, rt2)],
                None,
                None,
                false,
            )
            .expect("构造 token manager"),
        );
        AdminService::new(tm, Vec::<String>::new())
    }

    fn token_hash(svc: &AdminService, id: u64) -> Option<String> {
        svc.token_manager
            .snapshot()
            .entries
            .iter()
            .find(|e| e.id == id)
            .and_then(|e| e.refresh_token_hash.clone())
    }

    /// 截断 token（长度 <100 / 含 "..."）必须被拒：静默接受会让下一次刷新必然失败。
    #[test]
    fn truncated_token_is_rejected_with_400() {
        let svc = mk_service(&"a".repeat(150), &"b".repeat(150));
        for bad in ["a".repeat(99), "a".repeat(150) + "..."] {
            let err = svc
                .update_refresh_token(1, bad)
                .expect_err("截断 token 必须被拒");
            assert!(matches!(err, AdminServiceError::InvalidCredential(_)));
            assert_eq!(
                err.status_code(),
                axum::http::StatusCode::BAD_REQUEST,
                "校验失败必须返回 400"
            );
            assert!(err.to_string().contains("截断"), "文案应说明截断，实际 {err}");
        }
    }

    /// 与其他凭据的 refresh_token 重复必须被拒（对齐 add_credential 的哈希去重）。
    /// 跨凭据重复用 `DuplicateCredential`（非 `InvalidCredential`）：#13 语言耦合改造后
    /// 该变体是前端「duplicate_credential」判别的唯一依据，不能随文案改写而失配。
    #[test]
    fn duplicate_token_across_credentials_is_rejected() {
        let rt1 = "a".repeat(150);
        let rt2 = "b".repeat(150);
        let svc = mk_service(&rt1, &rt2);
        let err = svc
            .update_refresh_token(2, rt1.clone())
            .expect_err("与凭据 1 相同的 token 必须被拒");
        assert!(matches!(
            err,
            AdminServiceError::DuplicateCredential(_)
        ));
        assert_eq!(
            err.status_code(),
            axum::http::StatusCode::BAD_REQUEST,
            "校验失败必须返回 400"
        );
        assert!(err.to_string().contains("重复"), "文案应说明重复，实际 {err}");
        // 被拒后 2 号原值不得被改动。
        assert_eq!(
            token_hash(&svc, 2).as_deref(),
            Some(sha256_hex(&rt2).as_str())
        );
    }

    /// 正常 token 通过；用自身当前值重提交（no-op）也必须通过 —— 去重必须排除自己。
    #[test]
    fn valid_token_passes_and_self_resubmit_is_allowed() {
        let rt1 = "a".repeat(150);
        let rt2 = "b".repeat(150);
        let svc = mk_service(&rt1, &rt2);
        let new_rt = "c".repeat(150);
        svc.update_refresh_token(1, new_rt.clone())
            .expect("正常 token 必须通过");
        assert_eq!(
            token_hash(&svc, 1).as_deref(),
            Some(sha256_hex(&new_rt).as_str())
        );
        // 自身当前值重提交：不得被跨凭据重复检测误伤。
        svc.update_refresh_token(1, new_rt.clone())
            .expect("用自身当前值重提交必须通过（去重排除自身）");
        assert_eq!(
            token_hash(&svc, 1).as_deref(),
            Some(sha256_hex(&new_rt).as_str())
        );
    }

    fn mk_api_key_cred(id: u64) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.id = Some(id);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("ksk_test".to_string());
        c
    }

    /// 🔴 对抗审查 MINOR-6（2026-08-15）：api_key 凭据没有 refreshToken 概念
    /// （直接用 kiro_api_key 作 Bearer），更新它是误操作，必须 400 且不动原值。
    #[test]
    fn api_key_credential_update_refresh_token_is_rejected() {
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![mk_api_key_cred(1)],
                None,
                None,
                false,
            )
            .expect("构造 token manager"),
        );
        let svc = AdminService::new(tm, Vec::<String>::new());
        let err = svc
            .update_refresh_token(1, "a".repeat(150))
            .expect_err("api_key 凭据更新 refreshToken 必须被拒");
        assert!(matches!(err, AdminServiceError::InvalidCredential(_)));
        assert_eq!(
            err.status_code(),
            axum::http::StatusCode::BAD_REQUEST,
            "凭据类型闸必须返回 400"
        );
        assert!(
            err.to_string().contains("OAuth"),
            "文案应说明仅 OAuth 凭据支持，实际 {err}"
        );
        // 被拒后原凭据不得被改动（refresh_token 仍为 None，无新哈希）。
        assert_eq!(token_hash(&svc, 1), None, "api_key 号不得被写入 refresh_token_hash");
    }

    /// 🔴 对抗审查 MINOR-7（2026-08-15）：从聊天工具粘贴的 token 常带首尾换行/空白/
    /// 引号，entry 处 trim 后通过校验，落库（refresh_token_hash）必须是 trim 后的
    /// 规范值 —— 脏空白不得进入哈希，否则刷新链路对不上。
    #[test]
    fn whitespace_wrapped_token_is_trimmed_before_validate_and_store() {
        let rt1 = "a".repeat(150);
        let rt2 = "b".repeat(150);
        let svc = mk_service(&rt1, &rt2);
        let new_rt = "c".repeat(150);
        let wrapped = format!("\n\t\"{}\" \n", new_rt);
        let trimmed = wrapped.trim().to_string();
        svc.update_refresh_token(1, wrapped.clone())
            .expect("trim 后应通过校验（长度/截断检查作用于 trim 后值）");
        assert_eq!(
            token_hash(&svc, 1).as_deref(),
            Some(sha256_hex(&trimmed).as_str()),
            "落库哈希必须是 trim 后的值（首尾空白不得进入哈希）"
        );
        assert_ne!(
            token_hash(&svc, 1).as_deref(),
            Some(sha256_hex(&wrapped).as_str()),
            "未 trim 的原始串不得作为哈希（否则下次刷新 invalid_grant）"
        );
    }
}
