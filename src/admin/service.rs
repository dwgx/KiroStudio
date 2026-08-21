//! Admin API 业务逻辑服务

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use parking_lot::Mutex;
use serde::Serialize;
use tokio::task::JoinHandle;

use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::model::socks_node::SocksNode;
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
    SetLoadBalancingModeRequest, StorageCleanupItem, StorageCleanupResponse,
    StoragePartition, StorageStatsResponse, TrashItemResponse, TrashListResponse,
    build_import_response, mask_import_key,
};
use crate::kiro::auth::social::OAuthCallbackData;
use crate::usage::TraceDb;

#[path = "insight.rs"]
mod insight;
use insight::build_insight;
pub use insight::InsightParams;
#[path = "ksk_import.rs"]
mod ksk_import;
use ksk_import::{apply_ksk_region_suffix, clean_ksk_api_key};
#[path = "balance_cache.rs"]
mod balance_cache;
use balance_cache::CachedBalance;
#[path = "socks_nodes.rs"]
mod socks_nodes;
pub use socks_nodes::{SocksNodeTest, SocksNodeUpsertRequest};
#[path = "config_update.rs"]
mod config_update;
#[path = "service_restart.rs"]
mod service_restart;
#[cfg(target_os = "windows")]
pub(crate) use service_restart::spawn_windows_relaunch_process;
#[allow(unused_imports)] // 测试 `use super::*` 仍点得到 bat/healthz 助手
pub(crate) use service_restart::{windows_healthz_probe_url, windows_relaunch_bat};

/// SSO Token 导入结果（`POST /api/admin/credentials/import-sso`）。
pub struct ImportSsoTokenResult {
    pub credential_id: u64,
    /// 解析到的账号 email（best-effort，可能为 None）。
    pub email: Option<String>,
}

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
/// 200 与 `MAX_BATCH_DELETE_IDS` 同值同理由：adminKey 在 sessionStorage（读取时清
/// localStorage 残留）且文档带 CSP，无上限的批量删除仍会放大 XSS 的破坏面。差别在于
/// 这个端点**不收 ids** —— 候选是服务端自己算的，所以上限是唯一的量级闸门，比批量
/// 删除那条更承重。超出部分留给下一次调用（`skipped` 里标 `over_limit`），不静默丢弃。
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
    /// 中文推断文案（旧 UI fallback；新 UI 优先 `insight_code` + `insight_params`）
    pub insight_text: String,
    /// 稳定 insight 码（clear / disabled / cooldown_rate / cooldown / saturated / ...）
    pub insight_code: String,
    /// 与 `insight_code` 配套的 i18n 插值参数（camelCase）
    pub insight_params: InsightParams,
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

/// 导出/快照用：剥掉 `proxyUrl` 内嵌 `user:pass@`，只留 scheme+host。
/// 写入路径会把账密拆进 `proxyUsername`/`proxyPassword`；读路径仍要打码，
/// 挡住磁盘旧数据或手改 config.json 把 userinfo 留在 URL 里的泄漏。
fn proxy_url_without_userinfo(url: &str) -> String {
    crate::http_client::split_proxy_credentials(url).0
}

/// 错误码/提示词表合法 status 白名单（设计 §二 1，对齐 `exhausted_status` 先例）。
///
/// 504：`upstream_timeout` 默认 504（api_error）——管理员显式写回默认值时不被拒。
const ERROR_STATUS_WHITELIST: [u16; 11] = [400, 401, 402, 403, 404, 413, 429, 500, 502, 503, 504];

/// 错误码/提示词表合法 type 白名单。
///
/// 402 + `billing_error` / `quota_exceeded_error` 已随 QUOTA→402 放行
/// （仅与 402 组合；429+billing_error 仍拒，避免配置成可退避限流）。
const ERROR_TYPE_WHITELIST: [&str; 10] = [
    "invalid_request_error",
    "authentication_error",
    "billing_error",
    "permission_error",
    "not_found_error",
    "request_too_large",
    "rate_limit_error",
    "api_error",
    "overloaded_error",
    "quota_exceeded_error",
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
        402 => ty == "billing_error" || ty == "quota_exceeded_error",
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
            (None, Some(t), None) => t == "billing_error" || t == "quota_exceeded_error",
            _ => false,
        };
        if combo_bad {
            return Err(format!(
                "errorMessages[{key}]: status 与 type 组合不合法（渲染值不满足 \
                 402→billing_error/quota_exceeded_error；\
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
    /// - 换号结果缺 refreshToken → `InvalidCredential`（拒绝落盘不可刷新号）。
    pub async fn import_sso_token(
        &self,
        token: String,
        region: Option<String>,
        priority: u32,
        proxy_url: Option<String>,
    ) -> Result<ImportSsoTokenResult, AdminServiceError> {
        use crate::kiro::auth::sso_token::{
            build_idc_credential_from_sso, exchange_sso_token, find_duplicate_idc_email,
            require_sso_refresh_token,
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
        // 落盘闸：exchange 成功但 refresh 仍空时不得 add_credential（不可刷新死号）。
        if let Err(e) = require_sso_refresh_token(&exchange.refresh_token) {
            return Err(AdminServiceError::InvalidCredential(e.to_string()));
        }

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
                let built = build_insight(
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
                    insight_text: built.text,
                    insight_code: built.code.to_string(),
                    insight_params: built.params,
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
        self.map_batch_item_results(ids, |id| self.delete_credential_forced(id, force))
    }

    fn map_batch_item_results<F>(&self, ids: &[u64], mut f: F) -> Vec<BatchDeleteItemResult>
    where
        F: FnMut(u64) -> Result<(), AdminServiceError>,
    {
        ids.iter()
            .map(|&id| match f(id) {
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

    /// 批量重置失败计数并重新启用。部分失败仍逐条标 `ok`/`error`。
    pub fn reset_credentials_batch(&self, ids: &[u64]) -> Vec<BatchDeleteItemResult> {
        self.map_batch_item_results(ids, |id| self.reset_and_enable(id))
    }

    /// 批量启用/禁用。部分失败仍逐条标 `ok`/`error`。
    pub fn set_disabled_batch(&self, ids: &[u64], disabled: bool) -> Vec<BatchDeleteItemResult> {
        self.map_batch_item_results(ids, |id| self.set_disabled(id, disabled))
    }

    /// 批量设置允许模型白名单。部分失败仍逐条标 `ok`/`error`。
    pub fn set_allowed_models_batch(
        &self,
        ids: &[u64],
        models: Option<Vec<String>>,
    ) -> Vec<BatchDeleteItemResult> {
        self.map_batch_item_results(ids, |id| self.set_allowed_models(id, models.clone()))
    }

    /// 批量强制刷新 Token。部分失败仍逐条标 `ok`/`error`；串行调用避免打爆上游。
    pub async fn force_refresh_tokens_batch(&self, ids: &[u64]) -> Vec<BatchDeleteItemResult> {
        let mut results = Vec::with_capacity(ids.len());
        for &id in ids {
            results.push(match self.force_refresh_token(id).await {
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
            });
        }
        results
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
    /// 与 `MAX_BATCH_DELETE_IDS` 同理由（adminKey 在 sessionStorage，读取时清
    /// localStorage 残留，且文档带 CSP；无上限的批量删除仍会放大 XSS 的破坏面）。
    /// 超出部分按 id 升序留给**下一次调用**，并在 `skipped` 里以 `over_limit` 显式告知，
    /// 让重复调用能收敛，而不是静默丢弃。
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
            proxy_url: config
                .proxy_url
                .as_deref()
                .map(proxy_url_without_userinfo),
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
    /// `proxyUrl` 仍导出（导入要能搬 host），但剥掉内嵌 userinfo；账密走省略键继承。
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
            if let Some(serde_json::Value::String(url)) = obj.get_mut("proxyUrl") {
                *url = proxy_url_without_userinfo(url);
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

        // serde skip 使反序列化后 config_path 恒 None；save() 依赖该字段。
        imported.set_config_path(config_path.clone());
        // ④ 校验全部通过且路径已回填 → 先轮换备份再原子写盘（此刻起才算生效）
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
#[path = "service_tests.rs"]
mod tests;
