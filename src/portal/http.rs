//! Portal 的 HTTP 层：路由、cookie 会话、明文回查。
//!
//! # 路由布局
//! 全部挂在 `/portal` 下，与 `/api/admin`、`/v1` 无任何共享中间件：
//! - `GET  /portal`             → 单页 HTML（登录/注册/列表三态都在这一页）
//! - `POST /portal/api/register` → 注册（需注册码）
//! - `POST /portal/api/login`    → 登录，下发会话 cookie
//! - `POST /portal/api/logout`   → 登出，删会话 + 清 cookie
//! - `GET  /portal/api/me`       → 当前登录态（前端决定渲染哪一态）
//! - `GET  /portal/api/keys`     → **明文凭据列表**（需登录）
//!
//! # 明文从哪来
//! 库里只有元数据。`/api/keys` 拿到 `credential_id` 后向**活的凭据池**回查
//! （[`MultiTokenManager::export_credential`]），凭据被删就自然查不到、自动停止外显。
//! 磁盘上不存在第二份 key 副本。
//!
//! # 未启用时的行为
//! `portalEnabled=false` 时所有路由回 404（而非 403）：不确认这个功能存在，
//! 扫描者拿不到「这里有个 portal，只是没开」的信息。

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::Mutex;

use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use super::auth::{AuthError, LoginOk, PortalAuth};
use crate::admin::{AdminService, types::CachedBalanceItem};
use crate::kiro::token_manager::MultiTokenManager;

/// 会话 cookie 名。带 `__Host-` 前缀会强制要求 Secure+无 Domain+Path=/，
/// 但那样在纯 HTTP 内网调试下浏览器直接不收，故用普通名字，安全属性显式声明。
const COOKIE_NAME: &str = "portal_session";

/// 单次请求体上限。登录/注册的 JSON 只有几百字节，给 8KB 足够宽松，
/// 同时挡住「用超大 body 打 argon2」这类放大攻击。
const MAX_BODY_BYTES: usize = 8 * 1024;

/// 同一凭据两次 Portal 手动刷新之间的最短间隔。
const BALANCE_REFRESH_COOLDOWN: Duration = Duration::from_secs(60);

#[derive(Default)]
struct BalanceRefreshGate {
    refreshed_at: Mutex<HashMap<u64, Instant>>,
}

impl BalanceRefreshGate {
    /// 原子地占用一次刷新名额。返回值是还需等待的整秒数。
    fn acquire(&self, credential_id: u64) -> Result<(), u64> {
        let now = Instant::now();
        let mut refreshed_at = self.refreshed_at.lock();
        if let Some(last) = refreshed_at.get(&credential_id) {
            let elapsed = now.saturating_duration_since(*last);
            if elapsed < BALANCE_REFRESH_COOLDOWN {
                let remaining = BALANCE_REFRESH_COOLDOWN - elapsed;
                let secs = remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0);
                return Err(secs.max(1));
            }
        }
        refreshed_at.insert(credential_id, now);
        Ok(())
    }
}

/// Portal HTTP 状态。
#[derive(Clone)]
pub struct PortalState {
    pub auth: Arc<PortalAuth>,
    /// 活凭据池——凭据信息与明文回查的唯一来源。
    pub manager: Arc<MultiTokenManager>,
    /// 用量聚合。`None` = 配置里关掉了用量统计（`usageEnabled=false`）。
    ///
    /// 用 Option 而不是必填：用量统计是可关的，关了之后 portal 仍应能列凭据，
    /// 只是用量列显示为「未启用」。做成必填会让 portal 的可用性绑死在一个可选功能上。
    pub usage: Option<Arc<crate::usage::UsageStats>>,
    /// 与 Admin UI 共用的上游额度缓存。`None` 表示 AdminService 未启用。
    pub balance_service: Option<Arc<AdminService>>,
    balance_refresh_gate: Arc<BalanceRefreshGate>,
}

impl PortalState {
    pub fn new(auth: Arc<PortalAuth>, manager: Arc<MultiTokenManager>) -> Self {
        Self {
            auth,
            manager,
            usage: None,
            balance_service: None,
            balance_refresh_gate: Arc::new(BalanceRefreshGate::default()),
        }
    }

    /// 注入用量句柄（与热路径 sink 共享同一实例，故读到的就是实时聚合）。
    pub fn with_usage(mut self, usage: Arc<crate::usage::UsageStats>) -> Self {
        self.usage = Some(usage);
        self
    }

    /// 注入管理端正在使用的同一个额度服务，确保 Portal 与管理端看到同一份缓存。
    pub fn with_balance_service(mut self, service: Arc<AdminService>) -> Self {
        self.balance_service = Some(service);
        self
    }
}

/// Portal 是否启用（运行时镜像，热更即时生效）。
///
/// 与 `auth_keys` 同样的理由：改配置不该需要重启（重启会掐断在途流式请求）。
static PORTAL_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// cookie 是否带 `Secure`（运行时镜像）。
static REQUIRE_HTTPS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_enabled(v: bool) {
    PORTAL_ENABLED.store(v, std::sync::atomic::Ordering::Relaxed);
}

pub fn enabled() -> bool {
    PORTAL_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_require_https(v: bool) {
    REQUIRE_HTTPS.store(v, std::sync::atomic::Ordering::Relaxed);
}

fn require_https() -> bool {
    REQUIRE_HTTPS.load(std::sync::atomic::Ordering::Relaxed)
}

/// 积分制是否启用（运行时镜像）。
///
/// 关闭时 portal 行为与积分功能上线前**完全一致**：明文照旧直接下发、
/// 不存在 `/portal/api/board`。这不只是为了灰度，也是回滚开关——
/// 万一计价出问题，改配置即可停用，不必回退版本。
static CREDITS_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 当前计价参数（运行时镜像）。
///
/// 用 `ArcSwap` 而非五个 `AtomicI64`：五个原子量各自更新时，中间态可能是
/// 「新的 base_count 配旧的 base_price」这种从未被配置过的组合，而一次上车
/// 恰好读到它就会按不存在的规则计价。整体换指针让读到的永远是某个完整配置。
static PRICING: std::sync::OnceLock<arc_swap::ArcSwap<super::credits::Pricing>> =
    std::sync::OnceLock::new();

fn pricing_cell() -> &'static arc_swap::ArcSwap<super::credits::Pricing> {
    PRICING.get_or_init(|| arc_swap::ArcSwap::from_pointee(super::credits::Pricing::default()))
}

pub fn set_credits_enabled(v: bool) {
    CREDITS_ENABLED.store(v, std::sync::atomic::Ordering::Relaxed);
}

pub fn credits_enabled() -> bool {
    CREDITS_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// 热更计价参数。入口处 `sanitized()`，下游读到的一定是合理值。
pub fn set_pricing(p: super::credits::Pricing) {
    pricing_cell().store(Arc::new(p.sanitized()));
}

/// 读当前计价参数（只用于**尚无快照**的 key；有快照的按快照）。
pub fn pricing() -> super::credits::Pricing {
    **pricing_cell().load()
}

/// 供 admin 面板读取当前 cookie 安全策略（展示用）。
///
/// 单独开一个公开访问器而不把 [`require_https`] 直接改成 pub：本模块内部
/// 用它决定 cookie 属性，属于实现细节；对外只暴露「只读查询」这一种用途。
pub fn require_https_public() -> bool {
    require_https()
}

// ============ 请求/响应体 ============

#[derive(Debug, Deserialize)]
struct RegisterRequest {
    username: String,
    password: String,
    #[serde(default, rename = "inviteCode")]
    invite_code: String,
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MeResponse {
    username: String,
    expires_at_ms: i64,
    /// 当前角色（`"admin"` / `"user"` / `"readonly"`）。
    ///
    /// 【为何页面需要知道】只读角色打上车接口会被服务端 403。若页面不知道自己
    /// 是只读的，它会照常渲染「上车 · 10分」按钮——用户点一次、失败一次，
    /// 还会怀疑是不是系统坏了。服务端闸门是**安全边界**，这个字段只是让界面
    /// 不去引导一个注定失败的操作，两者不可互相替代：删掉这个字段只是变难看，
    /// 删掉那个闸门是越权。
    role: super::role::RoleKind,
    /// 能否上车。由服务端算而非前端按 role 字符串判断——判断规则只该有一份，
    /// 而它已经在 [`super::role::RoleKind::can_board`] 里。
    can_board: bool,
    /// 能否看运营看板（管理员专属）。同 [`Self::can_board`]：规则在
    /// [`super::role::RoleKind::can_manage`] 里，前端只据此决定要不要画那个按钮。
    ///
    /// 【为何前端默认按 false 处理，与 canBoard 相反】canBoard 缺字段时按「没限制」
    /// 处理是因为老服务端本就人人可上车；而看板是本轮才有的东西，老服务端根本没有
    /// `/admin/dashboard` 路由。缺字段时把按钮画出来，点下去只会是 404。
    can_manage: bool,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

/// 一条外显给用户的**真实凭据**（来自活凭据池，不再是推送记录）。
///
/// # 为什么只有 API Key 类型给明文
/// 池里有三类号：`api_key`（推送来的 `ksk_…`）、`social`、`idc`。后两类没有可复制的
/// key，只有 `access_token` / `refresh_token`——那不是「粘到客户端里就能用」的东西，
/// 而是**账号本身的恢复凭据**：refresh_token 泄露等于整个 AWS 账号被接管，且能长期
/// 续命。用户要的「方便复制」指的是 `ksk_`，那个照原样明文给；OAuth 类型的号仍然
/// 完整显示身份、健康和用量，只是 [`Self::key`] 为 `None`、[`Self::key_kind`] 说明原因。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CredentialRow {
    // ---- 身份 ----
    id: u64,
    /// 展示名优先级：用户备注 → 邮箱 → `#id`。前端不必再自己拼。
    display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    /// `api_key` / `social` / `idc` / `custom_api`。
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subscription_title: Option<String>,

    // ---- 明文 key ----
    /// **明文 key**，仅 API Key 类型有值，供直接复制。
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    /// 打码串，任何类型都给——没有明文时前端至少能显示个可辨别的东西。
    #[serde(skip_serializing_if = "Option::is_none")]
    masked_key: Option<String>,
    /// key 指纹（SHA-256 前 8 位），与凭据管理页同源，肉眼对账用。
    #[serde(skip_serializing_if = "Option::is_none")]
    fingerprint: Option<String>,
    /// 明文的可得性：`plain`（可复制）/ `oauth`（OAuth 号，故意不外显）/ `none`。
    key_kind: &'static str,

    // ---- 路由 ----
    endpoint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<String>,

    // ---- 健康 ----
    disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    disabled_reason: Option<String>,
    cooling_down: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    cooldown_remaining_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cooldown_reason: Option<String>,
    failure_count: u32,
    /// 综合状态，前端直接上色：`disabled` / `cooling` / `active` / `idle`。
    /// 放在服务端算，避免每个前端各判一套、判得不一样。
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<String>,
    /// 这个号被加进车队的时间（Unix 毫秒）。
    ///
    /// 老凭据没有这个值（字段是本次新加的，旧 credentials.json 里不存在），
    /// 前端显示 `—`。不回填假时间：编一个「文件修改时间」之类的近似值，
    /// 会让用户以为那是真的添加时间，比诚实地显示「不知道」更糟。
    #[serde(skip_serializing_if = "Option::is_none")]
    added_at_ms: Option<i64>,

    // ---- 实时负载 ----
    /// 最近 60 秒请求数。
    rpm: u32,
    /// 当前在途请求数。
    inflight: u32,
    /// 凭据级 RPM 上限（None = 继承全局）。
    #[serde(skip_serializing_if = "Option::is_none")]
    rpm_limit: Option<u32>,

    // ---- 用量（生命周期，来自凭据池，只增不清）----
    success_count: u64,
    total_credits_used: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_used_at: Option<String>,

    // ---- 用量（统计窗口，来自 usage_stats；未启用统计时全 0）----
    /// 保留窗口内的请求数。与 [`Self::success_count`] 口径不同：那个是生命周期累计，
    /// 这个受用量保留期影响。两个都给，因为「这号一共用了多少」和「最近用了多少」
    /// 是两个不同的问题。
    requests: u64,
    success_rate: f64,
    input_tokens: i64,
    output_tokens: i64,
    avg_latency_ms: f64,
    /// 最近 10 分钟每 30 秒一个点（20 个点，从旧到新），供前端画迷你走势。
    #[serde(skip_serializing_if = "Option::is_none")]
    rate_series: Option<Vec<u32>>,

    // ---- 车队（积分未启用时全为默认值，前端据 creditsEnabled 决定是否显示）----
    /// 我是否已上车。**这是 [`Self::key`] 有值的唯一依据。**
    aboard: bool,
    /// 我这一单实付（未上车时为 0）。已上车者看到的是退款后的当前净支出。
    paid: i64,
    /// 我上车的时刻（毫秒）。未上车时为 None，前端显示占位符。
    ///
    /// 用毫秒整数而非格式化字符串：服务端不知道看的人在哪个时区，
    /// 交给浏览器用本地时区渲染才对得上用户的「我什么时候上的车」。
    #[serde(skip_serializing_if = "Option::is_none")]
    boarded_at_ms: Option<i64>,
    /// 现在上车要花多少分。已满时是最后一个座位的价，仅供展示。
    board_price: i64,
    /// 已上车人数。
    board_count: i64,
    /// 名额上限。
    max_boarders: i64,
    /// 车已满且我不在车上——此时上车按钮应禁用。
    ///
    /// 服务端算而不是让前端比 `count >= max`：满员判定要和 `board` 用同一份
    /// 快照参数（这把 key 可能冻结着与当前配置不同的 max），前端拿不到快照。
    full: bool,

    // ---- Kiro 上游额度（只读缓存；与车队积分不是同一概念）----
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_quota: Option<CachedBalanceItem>,
    /// 当前会话能否触发这张凭据的真实上游额度刷新。
    ///
    /// 权限规则只在服务端维护；前端只据此决定是否绘制按钮。
    can_refresh_upstream_quota: bool,
}

/// 池子的汇总数字，省得前端自己加。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PoolSummary {
    total: usize,
    /// 可用 = 未禁用且未冷却。
    available: usize,
    disabled: usize,
    cooling: usize,
    /// 有明文可复制的条数。
    copyable: usize,
    total_rpm: u32,
    total_inflight: u32,
    total_credits_used: f64,
    total_requests: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct KeysResponse {
    summary: PoolSummary,
    items: Vec<CredentialRow>,
    /// 用量统计是否启用。false 时前端应说明「用量列为空是因为统计未开」，
    /// 而不是让用户以为这些号真的零调用。
    usage_enabled: bool,
    /// 积分制是否启用。false 时明文按老规则直接下发，前端不显示任何车队 UI。
    credits_enabled: bool,
    /// 我的钱包。随列表一起返回，省掉前端一次额外请求——余额和上车价必须
    /// 同时呈现（「这辆车 3 分，你有 2 分」），分两次请求会出现短暂的不一致。
    wallet: super::store::Wallet,
    /// false 表示当前进程没有可共享的额度服务；true 但行内为空表示尚无缓存快照。
    upstream_quota_available: bool,
    /// 当前车费规则。积分未启用时为 `None`（字段整个消失）——那种部署没有车费
    /// 这回事，下发一份不生效的规则只会让人照着它算一遍然后发现对不上。
    #[serde(skip_serializing_if = "Option::is_none")]
    pricing: Option<PricingRules>,
}

/// 外显给用户的车费规则。
///
/// # 为何价格表由服务端算好再下发
/// 那个公式是两段式 + 整数 ceil + `min` 钳制（见 [`super::credits`]）。让前端照抄一遍
/// 意味着同一条规则有两份实现，而它们迟早会分叉——页面上写着 3 分、实际扣 4 分，
/// 用户只会相信自己看到的那个，然后来问为什么被多扣了。这里用的是 `unit_price`，
/// 与 `board` 真正扣费调用的**同一个函数**，显示与实收不可能不一致。
///
/// # 为何用配置快照而不是某一行的冻结快照
/// 每把 key 首次被上车时会冻结当时的参数，此后改配置对它无效。所以「这一页的规则」
/// 严格来说是**新车**的规则：老车按自己冻结的价走，可能与这里显示的不同。文案里
/// 必须说清这一点，否则一个按老规则计价的用户会觉得页面在骗他。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PricingRules {
    /// 前几个人享受固定价。
    base_count: u32,
    /// 前 `base_count` 人的单价，同时是价格上限。
    base_price: i64,
    /// 定价基数。**不是**「一把车总共只收这么多」——触到 `min_price` 下限后
    /// 实收会随人数线性增长（15 人 × 3 分 = 45 分 > 20）。字段名容易误解，
    /// 所以前端文案不直接展示它，只用它解释「均摊」这件事。
    total_price: i64,
    /// 单价下限。人再多也不会低于这个价。
    min_price: i64,
    /// 一把车最多几人。
    max_boarders: u32,
    /// `N=1..=max_boarders` 每个人数下的单价，前端直接画表，不自己算。
    price_table: Vec<i64>,
}

/// 上车成功的响应。
///
/// **明文在这里首次下发。** 上车成功后立刻给，用户不必再刷一次列表——
/// 「付了分还要自己去找 key 在哪」是很差的体验，而这一步的明文与
/// `/api/keys` 走的是同一条判定（都以 `portal_unlocks` 有行为准）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BoardResponse {
    ok: bool,
    /// 本次实付。`already` 为 true 时是历史实付（退款后的当前值）。
    price: i64,
    /// 上车后余额。
    balance: i64,
    /// 上车后人数。
    count: i64,
    /// 本次上车触发的退款总额（分给车上其他人的）。展示用：让用户看到
    /// 「你的加入让前面 3 个人各退了 2 分」，这是这套机制的卖点。
    refunded: i64,
    /// true = 本来就在车上，这次没扣分。前端据此不弹「扣费成功」提示。
    already: bool,
    /// 明文 key。取不到时（OAuth 号、号刚被删）为 None——此时 `ok` 仍是 true，
    /// 因为上车这个动作确实成功了，前端要提示「已上车但该号无明文可复制」。
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
}

/// 积分不足（402）。
///
/// 带上 `needed` 和 `balance` 而不是只给一句「余额不足」：用户下一步的动作
/// （充多少）完全取决于这两个数，不给就得自己去翻钱包页再算差额。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct NotEnoughBody {
    error: String,
    needed: i64,
    balance: i64,
}

/// 车已满（409）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FullBody {
    error: String,
    count: i64,
    max: i64,
}

/// 钱包 + 流水。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WalletResponse {
    #[serde(flatten)]
    wallet: super::store::Wallet,
    ledger: Vec<super::store::LedgerEntry>,
}

/// 钱包流水一次最多返回多少条。
///
/// 100 条足够看清最近的账，又不会让响应体随使用时长无限膨胀。差额模型下
/// 退款流水很密（每有新人上车，车上每人各一条），不设上限的话老用户拉一次
/// 钱包可能拖出几千条。
const LEDGER_PAGE: usize = 100;

// ============ 工具 ============

/// **本项目唯一决定明文 key 是否进响应体的地方。**
///
/// 返回 `(要下发的明文, key_kind)`。抽成纯函数不是为了复用（只有一个调用点），
/// 而是为了让这条安全红线**可以被穷举测试**：内联在 500 行的 `list_keys` 里，
/// 就只能靠通读和肉眼守住，而漏一个分支的代价是把可用的 key 白送出去。
/// [`tests::plaintext_gate_never_leaks_when_not_aboard`] 遍历全部 16 种输入组合。
///
/// 规则：
/// - **`can_reveal == false`（只读角色）→ 一律 `None`**，无视积分开关与上车状态；
/// - 积分未启用 → 保持老行为，有明文就给（这条路径下不存在「上车」概念）；
/// - 积分启用 + 已上车 → 给；
/// - 积分启用 + 未上车 → **`None`**。不能是「发下去但前端不渲染」，那样明文
///   已经到了浏览器，F12 的 Network 面板或一条 curl 就能拿到，门槛等于没有。
///
/// # 为何只读判定必须在这里，而不是只挡住上车端点
/// 「只读账号不能上车」若只写在 `board` handler 里，那么**积分未启用的部署**上
/// 这道门形同不存在：那条路径下明文本来就直接下发，只读账号打开页面即看到全部
/// `ksk_`。而只读角色的用途恰是演示与审计账号——给它看全部明文，这个角色就没有
/// 任何意义了。放在这个唯一收口里，两种部署形态下语义一致。
///
/// 判定只看服务端状态（会话里的角色 + `portal_unlocks` 表），不看任何请求参数——
/// 客户端说什么都不影响结果。
fn gate_plaintext(
    plain: Option<String>,
    is_api_key: bool,
    credits_on: bool,
    aboard: bool,
    can_reveal: bool,
) -> (Option<String>, &'static str) {
    // 第一道：非 API Key 号一律不外显，无视积分与上车状态。
    //
    // 【为何是最外层而不是某个分支里的判断】social/idc 号带的是 OAuth
    // `refresh_token`，泄一个等于把整个 AWS 账号交出去，危害远大于一把 ksk_。
    // 把它放在最外层意味着「无论后面的逻辑怎么改，OAuth 明文都出不去」；
    // 若只在「未上车」分支里判，那么任何让 `aboard` 为真的路径（上车成功、
    // 日后新增的管理员豁免、测试夹具）都会绕过它。第一版就是这么写的，
    // 被 `plaintext_leaves_only_when_credits_off_or_aboard` 抓住。
    if !is_api_key {
        return (None, "oauth");
    }

    // 第二道：角色不允许取明文（readonly）→ 一律不给。
    //
    // 【为何必须在积分判断之外、且在它之前】只把 readonly 挡在「上车」那条路上是不够的：
    // 积分**未启用**时根本没有上车这回事，明文是直接下发的，于是 readonly 账号会看到
    // 全池明文——而 readonly 的用途正是「给演示/审计账号看，但不能拿走 key」。
    // 放在这一层意味着无论积分开关如何、无论 `aboard` 因何为真，readonly 都拿不到明文。
    if !can_reveal {
        return (
            None,
            // 与 "locked" 区分：locked 是「花分可解」，这里是「这个角色永远不解」，
            // 前端据此显示「无权查看」而不是一个点了没用的上车按钮。
            if plain.is_some() { "forbidden" } else { "none" },
        );
    }

    // 第三道：积分启用时，必须已上车。
    if credits_on && !aboard {
        return (
            None,
            // 区分「花分能买到」和「买了也是空」：后者不该诱导付费。
            if plain.is_some() { "locked" } else { "none" },
        );
    }

    match plain {
        Some(k) => (Some(k), "plain"),
        // 是 API Key 号但取不到明文——号刚被删或字段缺失。
        None => (None, "none"),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 从 `Cookie` 头里取会话令牌。
///
/// 手写解析而非引 `axum-extra`：只需要读一个名字固定的 cookie，
/// 为此加一个依赖不值得。按 `;` 切分、只认精确的名字匹配。
fn session_token_from_cookies(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        if name.trim() == COOKIE_NAME {
            let v = value.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// 构造会话 cookie。
///
/// - `HttpOnly`：JS 读不到，XSS 偷不走会话。
/// - `SameSite=Strict`：跨站请求不带 cookie，CSRF 基本消解（这也是本页面不需要
///   额外 CSRF token 的原因——所有写操作都是同站 POST）。
/// - `Path=/portal`：不外泄给 `/v1`、`/admin` 等其它路径。
/// - `Secure`：跟随配置。默认开，纯 HTTP 内网调试才关。
/// - `Max-Age`：与服务端会话 TTL 对齐，浏览器侧同步过期。
fn build_session_cookie(token: &str, max_age_secs: i64) -> String {
    let secure = if require_https() { "; Secure" } else { "" };
    format!(
        "{COOKIE_NAME}={token}; HttpOnly; SameSite=Strict; Path=/portal; Max-Age={max_age_secs}{secure}"
    )
}

/// 清除 cookie（登出）。`Max-Age=0` 让浏览器立刻丢弃。
fn build_clear_cookie() -> String {
    let secure = if require_https() { "; Secure" } else { "" };
    format!("{COOKIE_NAME}=; HttpOnly; SameSite=Strict; Path=/portal; Max-Age=0{secure}")
}

fn err_response(e: &AuthError) -> Response {
    (
        e.status(),
        Json(ErrorBody {
            error: e.client_message(),
        }),
    )
        .into_response()
}

/// 登录/注册成功的统一出口：下发 cookie。
fn login_ok_response(ok: LoginOk) -> Response {
    let max_age = ((ok.expires_at_ms - now_ms()) / 1000).max(0);
    let cookie = build_session_cookie(&ok.token, max_age);
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(MeResponse {
            username: ok.username,
            expires_at_ms: ok.expires_at_ms,
            role: ok.role,
            can_board: ok.role.can_board(),
            can_manage: ok.role.can_manage(),
        }),
    )
        .into_response()
}

/// 取客户端 IP（字符串形式，供节流与审计）。
///
/// 【为何不直接调 `security::client_ip`】那个函数签名要 `&Request<Body>`，而本模块的
/// handler 都用 `Json<T>` 提取体——axum 不允许同时再拿整个 `Request`。故这里按
/// **同一套口径**从 HeaderMap + 对端地址算，并复用 `is_trusted_proxy_peer` 这个判定，
/// 保证两处语义不会漂移。
///
/// 取 XFF **最右**段：XFF 是各级代理依次追加的链，最左是客户端可任意伪造的值。
/// 取最左会让攻击者每次发一个新的伪造 IP 来绕过按 IP 的登录节流——那等于节流失效。
fn client_ip_string(headers: &HeaderMap, peer: Option<std::net::SocketAddr>) -> Option<String> {
    let trust = peer
        .map(|p| crate::common::security::is_trusted_proxy_peer(p.ip()))
        .unwrap_or(false);
    if trust {
        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(last) = xff.split(',').next_back() {
                let ip = last.trim();
                if !ip.is_empty() {
                    return Some(ip.to_string());
                }
            }
        }
        if let Some(real) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
            let ip = real.trim();
            if !ip.is_empty() {
                return Some(ip.to_string());
            }
        }
    }
    peer.map(|p| p.ip().to_string())
}

// ============ 中间件 ============

/// 未启用时一律 404。
///
/// 用 404 而非 403：不确认这个功能存在。扫描者拿到 403 就知道「这里有个 portal，
/// 只是没开」，值得继续盯；404 与「路径不存在」无法区分。
async fn feature_gate(request: Request<Body>, next: Next) -> Response {
    if !enabled() {
        return StatusCode::NOT_FOUND.into_response();
    }
    next.run(request).await
}

/// 登录态校验：解析 cookie → 校验会话。失败 401。
///
/// 校验通过的用户名塞进请求扩展，供 `/api/keys` 等 handler 取用，
/// 避免每个 handler 各自再解一遍 cookie（少一处出错的地方）。
async fn require_session(
    State(state): State<PortalState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let Some(token) = session_token_from_cookies(request.headers()) else {
        return unauthorized();
    };
    let Some(session) = state.auth.validate(&token, now_ms()) else {
        return unauthorized();
    };
    request.extensions_mut().insert(session);
    next.run(request).await
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrorBody {
            error: "未登录或会话已过期".to_string(),
        }),
    )
        .into_response()
}

/// 空 404。用于「积分未启用时访问上车接口」。
///
/// 与 [`feature_gate`] 同一个理由：不确认功能存在。返回 403「积分功能未开启」
/// 会告诉探测者这套机制在这台服务器上是有的，只是关着——而 404 与
/// 「这个版本没有这个接口」无法区分。
fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

/// 403：已登录，但这个角色不允许做这件事。
///
/// # 为何是 403 而不是 404
/// 与「功能未启用」相反，这里**应该**如实告诉用户原因。只读账号的持有者知道
/// 自己是只读的（那是管理员有意设的），回 404 会让他以为是页面坏了而去反复重试、
/// 或找管理员报「上车按钮点不动」这种查不出原因的故障。403 + 一句人话，
/// 他立刻知道该找谁要权限。
///
/// 不确认功能存在这条原则针对的是**未认证的探测者**；对已登录用户隐瞒自己的
/// 权限边界没有安全收益，只有支持成本。
fn forbidden(msg: impl Into<String>) -> Response {
    (StatusCode::FORBIDDEN, Json(ErrorBody { error: msg.into() })).into_response()
}

// ============ Handlers ============

async fn register(
    State(state): State<PortalState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<RegisterRequest>,
) -> Response {
    let ip = client_ip_string(&headers, Some(peer));
    match state.auth.register(
        &req.username,
        &req.password,
        &req.invite_code,
        ip.as_deref(),
        now_ms(),
    ) {
        Ok(ok) => login_ok_response(ok),
        Err(e) => err_response(&e),
    }
}

async fn login(
    State(state): State<PortalState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Response {
    let ip = client_ip_string(&headers, Some(peer));
    match state
        .auth
        .login(&req.username, &req.password, ip.as_deref(), now_ms())
    {
        Ok(ok) => login_ok_response(ok),
        Err(e) => err_response(&e),
    }
}

async fn logout(
    State(state): State<PortalState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Some(token) = session_token_from_cookies(&headers) {
        let ip = client_ip_string(&headers, Some(peer));
        // 先校验一次，只为拿到用户名写进审计。审计里留空 username 的登出记录
        // 事后没法和任何人对应，等于白记。校验不过（票已过期/伪造）就传 None——
        // 下面照样清 cookie，不因此报错。
        let username = state.auth.validate(&token, now_ms()).map(|s| s.username);
        state
            .auth
            .logout(&token, username.as_deref(), ip.as_deref(), now_ms());
    }
    // 无论有没有有效会话都回 200 + 清 cookie：登出是幂等的，
    // 「你本来就没登录」不值得报错，报错反而泄露会话状态。
    (
        StatusCode::OK,
        [(header::SET_COOKIE, build_clear_cookie())],
        Json(serde_json::json!({"ok": true})),
    )
        .into_response()
}

/// 当前登录态。前端据此决定渲染登录页还是列表页。
async fn me(request: Request<Body>) -> Response {
    // require_session 已校验并塞进 extensions
    match request.extensions().get::<super::store::PortalSession>() {
        Some(s) => (
            StatusCode::OK,
            Json(MeResponse {
                username: s.username.clone(),
                expires_at_ms: s.expires_at_ms,
                role: s.role,
                can_board: s.role.can_board(),
                can_manage: s.role.can_manage(),
            }),
        )
            .into_response(),
        None => unauthorized(),
    }
}

/// 一页最多返回多少条。防止一次拉出整池 + 拼接巨大 JSON。
/// 综合状态判定。放服务端算，前端直接按这个字符串上色。
///
/// 顺序即优先级：禁用 > 冷却 > 近期有流量 > 空闲。判定顺序反了会出现
/// 「已禁用的号显示成 active」这种误导——运维据此做决策，不能含糊。
fn derive_status(disabled: bool, cooling: bool, rpm: u32) -> &'static str {
    if disabled {
        "disabled"
    } else if cooling {
        "cooling"
    } else if rpm > 0 {
        "active"
    } else {
        "idle"
    }
}

/// **真实凭据列表**（来自活凭据池）。需登录。
///
/// 数据来自三处，合成一行：
/// 1. `manager.snapshot()` —— 身份、健康、实时负载、生命周期用量；
/// 2. `manager.cooldown_snapshot()` —— 冷却状态（与凭据管理页同源，口径一致）；
/// 3. `usage_stats.by_credential()` —— 保留窗口内的请求/tokens/延迟。
///
/// 明文只对 API Key 类型回查（见 [`CredentialRow`] 的说明）。每次调用写一条审计，
/// 计数只算**真正外显了明文**的条数——OAuth 号不算，否则审计数字会虚高，
/// 事后追查「到底泄了多少个可用 key」会被误导。
async fn list_keys(
    State(state): State<PortalState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    request: Request<Body>,
) -> Response {
    let session = match request.extensions().get::<super::store::PortalSession>() {
        Some(s) => s.clone(),
        None => return unauthorized(),
    };

    let snapshot = state.manager.snapshot();
    // 只读管理端的持久化快照，不触发任何 getUsageLimits 上游调用。
    let upstream_quota_available = state.balance_service.is_some();
    let mut cached_quotas = state
        .balance_service
        .as_ref()
        .map(|service| service.get_cached_balances().balances)
        .unwrap_or_default();
    // 整个 config 留着：region 解析要用它做兜底（见下面 effective_upstream_region），
    // 只 clone 一个 default_endpoint 就得再取一次锁。
    let config = state.manager.config();
    let default_endpoint = config.default_endpoint.clone();

    // 冷却快照转成 map，避免每条凭据都线性扫一遍（池子大了就是 O(n²)）。
    let cooldowns: std::collections::HashMap<u64, (u64, String)> = state
        .manager
        .cooldown_snapshot()
        .into_iter()
        .map(|c| {
            (
                c.credential_id,
                (c.remaining_ms, c.reason.description().to_string()),
            )
        })
        .collect();

    // 用量聚合：key 是凭据 ID 的字符串形式。统计未启用时是空 map，所有用量列为 0。
    let usage_enabled = state.usage.is_some();
    let usage: std::collections::HashMap<String, crate::usage::usage_stats::GroupStat> = state
        .usage
        .as_ref()
        .map(|s| {
            s.by_credential()
                .into_iter()
                .map(|g| (g.key.clone(), g))
                .collect()
        })
        .unwrap_or_default();

    // 积分态：一次性取回「我上了哪些车」和「每辆车几个人」，避免逐行查库
    // （列表有几十行，逐行查就是几十次加锁）。
    let credits_on = credits_enabled();
    // 只读角色永远拿不到明文（见 gate_plaintext 的第一道门）。从会话里取而不是
    // 再查一次库：会话已经带着角色，重查等于给「两次读到不同值」留出窗口。
    let can_reveal = session.role.can_reveal_plaintext();
    let db = state.auth.db();
    let (aboard, counts) = if credits_on {
        (
            db.aboard_map(session.user_id).unwrap_or_default(),
            db.unlocker_counts().unwrap_or_default(),
        )
    } else {
        Default::default()
    };
    // 价格快照也批量取。逐行 `pricing_of` 会让列表页的加锁次数随凭据数线性增长，
    // 而这三张表的全量都很小（上限是「被上过车的 key 数」）。
    let snaps = if credits_on {
        db.all_pricing().unwrap_or_default()
    } else {
        Default::default()
    };
    let cfg_pricing = pricing();

    let mut items = Vec::with_capacity(snapshot.entries.len());
    let mut summary = PoolSummary {
        total: snapshot.entries.len(),
        available: 0,
        disabled: 0,
        cooling: 0,
        copyable: 0,
        total_rpm: 0,
        total_inflight: 0,
        total_credits_used: 0.0,
        total_requests: 0,
    };
    let mut revealed = 0usize;

    for e in snapshot.entries {
        let cd = cooldowns.get(&e.id);
        let cooling = cd.is_some();

        // 回查完整凭据：明文 key 与 region 都从这里取（snapshot 不带 region）。
        // 每条一次，池子是个位到百位量级，成本可忽略；分两次查反而多拿一次锁。
        let cred = state.manager.export_credential(e.id);

        // region 必须与**网关实际请求上游时用的那个**完全一致，否则这一列就是错的：
        // 用户照着页面去配 client，配的却不是这把 key 真正生效的 region。
        //
        // 【为何不能自己拼 region || api_region || auth_region】那是本函数的旧写法，
        // 与运行时口径有三处偏差：①漏了 profileArn 第 4 段（它优先级最高，
        // Enterprise/IdC 号的 region 就藏在那里）②不过白名单，污染值会被原样显示给用户
        // ③没有 config 兜底，三字段皆空时显示「—」，而网关此刻其实在用 config.region。
        // 生产上 #252 三字段都是 eu-central-1 而全局 config 是 us-east-1，恰好掩盖了
        // 后两条——换一个只填了 profileArn 的号进来，显示的就会是错的。
        // 直接调运行时那一个函数，口径唯一，日后改优先级也不会漏掉这一处。
        let region = cred
            .as_ref()
            .map(|c| c.effective_upstream_region(&config).to_string());

        // 明文只对 API Key 类型外显。判定用 snapshot 的 auth_method
        // （它已把 builder-id/iam 归一成 idc，并对 api_key 做了识别）。
        let is_api_key = e.auth_method.as_deref() == Some("api_key");
        let plain = if is_api_key {
            cred.and_then(|c| c.kiro_api_key)
        } else {
            None
        };

        // 这把 key 的计价参数：有快照按快照，无快照按当前配置（与 `board` 一致，
        // 否则列表显示的价和真正扣的价会不一样）。
        let snap = snaps.get(&(e.id as i64)).copied().unwrap_or(cfg_pricing);
        let board_count = counts.get(&(e.id as i64)).copied().unwrap_or(0);
        // aboard_map 的值是 (实付, 上车时刻)——同一张表一次取回，不为了时间再查一遍。
        let my_row = aboard.get(&(e.id as i64)).copied();
        let onboard = my_row.is_some();
        let full = !onboard && snap.is_full(board_count as u32);
        // 未上车时展示的是「我上车要付多少」= count+1 时的单价。
        let board_price = snap.unit_price(board_count as u32 + 1);

        // 指纹在明文被裁掉**之前**算。
        //
        // 指纹是 SHA-256 前 8 位，不可逆、不足以复原 key，但足够让用户在付分之前
        // 确认「这把是不是我手上那把」——否则只能付了才知道买错。
        let fingerprint = plain
            .as_deref()
            .map(crate::common::key_mask::key_fingerprint);

        // 安全红线在 `gate_plaintext` 里，那是本文件唯一决定明文去留的地方。
        let (key, key_kind) = gate_plaintext(plain, is_api_key, credits_on, onboard, can_reveal);
        if key.is_some() {
            revealed += 1;
            summary.copyable += 1;
        }

        // 展示名：备注 → 邮箱 → #id。在服务端定一次，免得各前端拼得不一样。
        let display_name = e
            .name
            .clone()
            .or_else(|| e.email.clone())
            .unwrap_or_else(|| format!("#{}", e.id));

        let u = usage.get(&e.id.to_string());
        let rate_series = state.usage.as_ref().map(|s| s.recent_rate(e.id));

        if e.disabled {
            summary.disabled += 1;
        } else if cooling {
            summary.cooling += 1;
        } else {
            summary.available += 1;
        }
        summary.total_rpm += e.rpm;
        summary.total_inflight += e.inflight;
        summary.total_credits_used += e.total_credits_used;
        summary.total_requests += u.map(|g| g.requests).unwrap_or(0);

        items.push(CredentialRow {
            id: e.id,
            display_name,
            name: e.name,
            email: e.email,
            auth_method: e.auth_method,
            subscription_title: e.subscription_title,

            key,
            masked_key: e.masked_api_key,
            fingerprint,
            key_kind,

            endpoint: e.endpoint.unwrap_or_else(|| default_endpoint.clone()),
            region,

            disabled: e.disabled,
            disabled_reason: e.disabled_reason,
            cooling_down: cooling,
            cooldown_remaining_ms: cd.map(|(ms, _)| *ms),
            cooldown_reason: cd.map(|(_, r)| r.clone()),
            failure_count: e.failure_count,
            status: derive_status(e.disabled, cooling, e.rpm),
            expires_at: e.expires_at,
            added_at_ms: e.added_at_ms,

            rpm: e.rpm,
            inflight: e.inflight,
            rpm_limit: e.rpm_limit,

            success_count: e.success_count,
            total_credits_used: e.total_credits_used,
            last_used_at: e.last_used_at,

            requests: u.map(|g| g.requests).unwrap_or(0),
            success_rate: u.map(|g| g.success_rate).unwrap_or(0.0),
            input_tokens: u.map(|g| g.input_tokens).unwrap_or(0),
            output_tokens: u.map(|g| g.output_tokens).unwrap_or(0),
            avg_latency_ms: u.map(|g| g.avg_latency_ms).unwrap_or(0.0),
            rate_series,

            aboard: onboard,
            paid: my_row.map(|(p, _)| p).unwrap_or(0),
            boarded_at_ms: my_row.map(|(_, t)| t),
            board_price,
            board_count,
            max_boarders: snap.max_unlockers as i64,
            full,
            upstream_quota: cached_quotas.remove(&e.id),
            // 【为何不再要求「已上车」】额度数字本来就对所有人可见（fillQuota 无条件渲染），
            // 只把**刷新**挡住的结果是：未上车的人看到一个不知何时的旧数字，且没有任何办法
            // 更新它——而他正要靠这个数字决定值不值得花分上车。实测生产上没有任何人上过
            // 当前那把 key 的车，于是全员都没有按钮，这一列等于长期显示陈旧值。
            //
            // 放开的前提是上游压力与用户数无关：BALANCE_REFRESH_COOLDOWN 是**按凭据**
            // 计的 60 秒硬窗（见 BalanceRefreshGate），一百个人同时点，上游也只会被打一次。
            // 所以这里放开的是「谁能触发」，不是「能打多少次」。
            //
            // readonly 仍然挡住（can_reveal）：那个角色的定义就是只看不动，
            // 而刷新会真的向 AWS 发一次请求——那已经不是「看」了。
            can_refresh_upstream_quota: upstream_quota_available && can_reveal,
        });
    }

    // 排序：可用的在前，然后按 RPM 降序、ID 升序。用户最关心「现在能用哪个」。
    items.sort_by(|a, b| {
        let rank = |s: &str| match s {
            "active" => 0,
            "idle" => 1,
            "cooling" => 2,
            _ => 3,
        };
        rank(a.status)
            .cmp(&rank(b.status))
            .then(b.rpm.cmp(&a.rpm))
            .then(a.id.cmp(&b.id))
    });

    let ip = client_ip_string(&headers, Some(peer));
    state.auth.audit_reveal(
        now_ms(),
        session.user_id,
        &session.username,
        ip.as_deref(),
        revealed,
    );

    // 钱包随列表一起给：余额和上车价必须同时呈现（「这辆车 3 分，你有 2 分」），
    // 分两次请求会出现「余额是旧的、价格是新的」这种短暂不一致。
    // 积分未启用时不查库，返回全 0。
    let wallet = if credits_on {
        db.wallet_of(session.user_id).unwrap_or_default()
    } else {
        super::store::Wallet::default()
    };

    (
        StatusCode::OK,
        Json(KeysResponse {
            summary,
            items,
            usage_enabled,
            credits_enabled: credits_on,
            wallet,
            upstream_quota_available,
            // 规则从**运行时的同一份 pricing()** 算，而不是前端照着公式复算：
            // 两段式 + ceil + min 钳制一旦有两份实现，页面显示的价和真正扣的分
            // 就会在某个人数上对不上，而用户只相信自己看到的那个数。
            // 价格表也在服务端算好（同一个 unit_price），前端只负责画。
            pricing: if credits_on {
                Some(PricingRules {
                    base_count: cfg_pricing.base_count,
                    base_price: cfg_pricing.base_price,
                    total_price: cfg_pricing.total_price,
                    min_price: cfg_pricing.min_price,
                    max_boarders: cfg_pricing.max_unlockers,
                    price_table: (1..=cfg_pricing.max_unlockers)
                        .map(|n| cfg_pricing.unit_price(n))
                        .collect(),
                })
            } else {
                None
            },
        }),
    )
        .into_response()
}

/// 手动刷新单个凭据的 Kiro 上游额度。
///
/// 列表页永远只读缓存；只有这个显式 POST 才真实调用上游。积分开启时，普通用户必须
/// 已经上过这辆车；Portal 管理员可为运营排障刷新。只读角色始终不能触发上游请求。
async fn refresh_upstream_quota(
    State(state): State<PortalState>,
    axum::extract::Path(cred_id): axum::extract::Path<u64>,
    request: Request<Body>,
) -> Response {
    let session = match request.extensions().get::<super::store::PortalSession>() {
        Some(s) => s.clone(),
        None => return unauthorized(),
    };

    if !session.role.can_reveal_plaintext() {
        return forbidden("当前账号是只读角色，不能刷新上游额度");
    }

    let exists = state
        .manager
        .snapshot()
        .entries
        .iter()
        .any(|entry| entry.id == cred_id);
    if !exists {
        return not_found();
    }

    // 【为何删掉了「必须已上车」这道门】理由与 can_refresh_upstream_quota 同一条：
    // 额度数字对所有登录用户可见，只挡刷新只会让未上车的人对着陈旧数字做决策。
    //
    // 这一处必须与上面那个字段的条件**保持一致**：字段决定画不画按钮，这里决定放不放行。
    // 两边不一致的表现是「按钮画出来了、点下去 403」，或者更糟——「按钮没画，
    // 但接口其实放行」。删这道门时若漏改另一处，就是前者。
    //
    // 仍然保留的两道：只读角色（上面 can_reveal_plaintext）与按凭据 60 秒硬窗（下面 gate）。
    // 前者管「谁」，后者管「多频繁」，而上游压力只由后者决定。

    let Some(service) = state.balance_service.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorBody {
                error: "上游额度服务未启用".to_string(),
            }),
        )
            .into_response();
    };

    if let Err(retry_after) = state.balance_refresh_gate.acquire(cred_id) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, retry_after.to_string())],
            Json(serde_json::json!({
                "error": format!("刷新过于频繁，请在 {retry_after} 秒后重试"),
                "retryAfterSecs": retry_after,
            })),
        )
            .into_response();
    }

    match service.refresh_balance(cred_id).await {
        Ok(quota) => (StatusCode::OK, Json(quota)).into_response(),
        Err(error) => {
            if error.status_code() == StatusCode::NOT_FOUND {
                return not_found();
            }
            tracing::warn!(
                "Portal 刷新上游额度失败 user={} cred={}: {}",
                session.user_id,
                cred_id,
                error
            );
            (
                StatusCode::BAD_GATEWAY,
                Json(ErrorBody {
                    error: "上游额度刷新失败，请稍后重试".to_string(),
                }),
            )
                .into_response()
        }
    }
}

/// 上车：扣分并首次下发明文。
///
/// # 为何是 POST 而不是 GET
/// 它扣钱、改库、且**不幂等地产生副作用**（第一次上车会扣分并触发全车退款）。
/// 做成 GET 会被浏览器预取、被爬虫触发、被 `<img src>` 跨站触发——每一次都是
/// 真金白银的扣分。POST + `SameSite=Strict` cookie 让这些都不成立。
///
/// # 状态码的选择
/// - `402 Payment Required` —— 积分不足。语义正好，且前端能据此弹充值提示而
///   不必解析错误文案（文案会改，状态码不会）。
/// - `409 Conflict` —— 车已满。不是客户端参数错（400），也不是没权限（403），
///   而是「当前资源状态不允许」，这正是 409 的定义。
/// - `404` —— 积分功能未启用。与 `portalEnabled=false` 同策略：不确认功能存在。
///   用 403 会告诉扫描者「这里有个上车接口，只是没开」。
async fn board(
    State(state): State<PortalState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    axum::extract::Path(cred_id): axum::extract::Path<u64>,
    request: Request<Body>,
) -> Response {
    let session = match request.extensions().get::<super::store::PortalSession>() {
        Some(s) => s.clone(),
        None => return unauthorized(),
    };

    // 只读角色不得上车。
    //
    // 【为何是 403 而不是 404】404 的语义是「这条路不存在」，用于功能未启用；
    // 这里路存在、身份也有效，只是这个角色不被允许——如实回 403，运营者看日志时
    // 能立刻分辨「功能没开」和「权限不够」，那是两种完全不同的处置。
    //
    // 【为何在扣分之前】这道门必须在 `db.board()` 之前，否则分已经扣了、
    // 座位已经占了，再拒绝就成了「付了钱不给货」，还要写补偿逻辑去退。
    if !session.role.can_board() {
        return forbidden("当前账号是只读角色，不能上车");
    }

    // 积分未启用 → 该功能不存在。明文本来就直接给，没有「上车」这回事。
    if !credits_enabled() {
        return not_found();
    }

    let ip = client_ip_string(&headers, Some(peer));
    let now = now_ms();
    let cid = cred_id as i64;

    // 只允许对**池子里真实存在**的凭据上车。
    //
    // 【为何必须先查】没有这一步，任意 id 都能上车：库里会长出指向不存在凭据的
    // 记录，用户付了分却永远拿不到明文（`list_keys` 回查会失败），而退款逻辑
    // 还会把后来的真实乘客算进这些幽灵座位里，把整把 key 的计价搅乱。
    let exists = state
        .manager
        .snapshot()
        .entries
        .iter()
        .any(|e| e.id == cred_id);
    if !exists {
        return not_found();
    }

    let outcome = match state.auth.db().board(session.user_id, cid, pricing(), now) {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("上车失败 user={} cred={}: {:#}", session.user_id, cid, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody {
                    error: "上车失败，请稍后重试".to_string(),
                }),
            )
                .into_response();
        }
    };

    use super::store::BoardOutcome;
    match outcome {
        BoardOutcome::Aboard {
            price,
            balance,
            count,
            refunded,
        } => {
            state.auth.audit_board(
                now,
                session.user_id,
                &session.username,
                ip.as_deref(),
                "board_ok",
                cid,
                &format!("price={price} count={count} refunded={refunded}"),
            );
            (
                StatusCode::OK,
                Json(BoardResponse {
                    ok: true,
                    already: false,
                    price,
                    balance,
                    count,
                    refunded,
                    key: lookup_plain(&state, cred_id, session.role.can_reveal_plaintext()),
                }),
            )
                .into_response()
        }

        // 重复上车不是错误：刷新页面、双击按钮都会走到这里。返回 200 + 明文，
        // 与首次上车的响应形状一致，前端不必分两条路处理。
        BoardOutcome::AlreadyAboard {
            paid,
            balance,
            count,
        } => (
            StatusCode::OK,
            Json(BoardResponse {
                ok: true,
                already: true,
                price: paid,
                balance,
                count,
                refunded: 0,
                key: lookup_plain(&state, cred_id, session.role.can_reveal_plaintext()),
            }),
        )
            .into_response(),

        BoardOutcome::NotEnough { needed, balance } => {
            state.auth.audit_board(
                now,
                session.user_id,
                &session.username,
                ip.as_deref(),
                "board_fail_insufficient",
                cid,
                &format!("needed={needed} balance={balance}"),
            );
            (
                StatusCode::PAYMENT_REQUIRED,
                Json(NotEnoughBody {
                    error: format!("积分不足：需要 {needed} 分，当前 {balance} 分"),
                    needed,
                    balance,
                }),
            )
                .into_response()
        }

        BoardOutcome::Full { count, max } => {
            state.auth.audit_board(
                now,
                session.user_id,
                &session.username,
                ip.as_deref(),
                "board_fail_full",
                cid,
                &format!("count={count} max={max}"),
            );
            (
                StatusCode::CONFLICT,
                Json(FullBody {
                    error: format!("这辆车已满（{count}/{max}）"),
                    count,
                    max,
                }),
            )
                .into_response()
        }
    }
}

/// 上车成功后回查明文。**调用方必须已确认该用户在车上。**
///
/// 【为何这里不自己判条件，而是转交 [`gate_plaintext`]】
/// 这是本项目第二条会下发明文的路径（第一条是列表页）。两条路各写一份判定，
/// 就等于同一条安全规则有两个实现——日后收紧一处、漏掉另一处，漏的那条就是
/// 现成的绕过口子，而且不会有任何编译错误提示。所以这里只负责取数据
/// （明文 + 是否 API Key 号），是否放行一律交给那个唯一的闸门，
/// `aboard: true` 是调用方已确认的事实。
///
/// 特别地，「非 API Key 号不外显」由闸门统一保证：不能因为「反正都上车了」
/// 就把 OAuth 号的 refresh_token 交出去——那等于交出整个 AWS 账号。
fn lookup_plain(state: &PortalState, cred_id: u64, can_reveal: bool) -> Option<String> {
    let is_api_key = state
        .manager
        .snapshot()
        .entries
        .iter()
        .find(|e| e.id == cred_id)
        .and_then(|e| e.auth_method.clone())
        .as_deref()
        == Some("api_key");

    let plain = state
        .manager
        .export_credential(cred_id)
        .and_then(|c| c.kiro_api_key);

    let (key, _) = gate_plaintext(plain, is_api_key, credits_enabled(), true, can_reveal);
    key
}

/// 钱包与流水。
async fn wallet(State(state): State<PortalState>, request: Request<Body>) -> Response {
    let session = match request.extensions().get::<super::store::PortalSession>() {
        Some(s) => s.clone(),
        None => return unauthorized(),
    };
    if !credits_enabled() {
        return not_found();
    }

    let db = state.auth.db();
    let w = db.wallet_of(session.user_id).unwrap_or_default();
    // 流水读失败不致命：余额是权威数字，流水只是明细。为了一份明细读不到就
    // 让整个钱包页打不开，是把观测功能的故障升级成主功能故障。
    let ledger = db
        .ledger_of(session.user_id, LEDGER_PAGE)
        .unwrap_or_default();

    (StatusCode::OK, Json(WalletResponse { wallet: w, ledger })).into_response()
}

// ============ 运营看板（portal 会话 + admin 角色）============

/// 车辆热度 / 失败 IP 各取前几条。
///
/// 不做分页：看板是「一眼看全局」，翻页到第 7 页的车队热度没有决策价值。真要逐条
/// 查就该去审计页（G4），那里才需要分页。
const DASHBOARD_TOP: usize = 20;

/// 审计列表不带 `limit` 时给多少条。
///
/// 【为何是 50 而不是上限 200】审计页第一屏要能快、要能扫完。一次 200 条要滚很久，
/// 而运营的典型动作是「看最近发生了什么」，看不完的那部分靠翻页而不是靠一次全给。
const DEFAULT_AUDIT_PAGE: i64 = 50;

/// 登录失败统计的回看窗口：24 小时。
const FAIL_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// 「今日」的起点：**服务器本地时区的今天零点**。
///
/// # 为何用本地零点而不是「最近 24 小时」
/// 运营看板上的「今日」要能和人对话——「今天发了 12 张票」得跟运营自己数的一致。
/// 滚动 24h 窗口里的数字会随时刻漂移（早上问和晚上问答案不同），而人心里的「今天」
/// 是自然日。代价是跨时区团队看到的是服务器所在时区的日界，所以响应里回传
/// `sinceMs`，界面显式写出「自 X 起」，不让人去猜这个数字的边界在哪。
///
/// # 为何用本地时区而不是 UTC
/// 部署在东八区的机器上，UTC 零点是当地早上 8 点——「今日」会把昨天晚上算进来，
/// 而把今天早上 8 点前的漏掉。那是最容易让人对不上账的一种偏差，因为它看起来
/// 只是「数字有点小」，不像故障。
fn today_start_ms(now: i64) -> i64 {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(now).single() {
        Some(dt) => dt
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .and_then(|naive| Local.from_local_datetime(&naive).single())
            .map(|midnight| midnight.timestamp_millis())
            // 拿不到本地零点（夏令时跳变那一小时确实可能无解/有歧义）就退回
            // 滚动 24h。宁可窗口口径退化，也不能让看板整个打不开。
            .unwrap_or(now - FAIL_WINDOW_MS),
        None => now - FAIL_WINDOW_MS,
    }
}

/// 运营看板数据。需 portal 会话 + `admin` 角色（由 [`require_admin`] 把门）。
///
/// # 为何这条接口与 adminApiKey 侧的 `/api/admin/portal/*` 并存
/// 那一侧是「部署者」的权限：它能改配置、增删凭据。把看板做在那边意味着想让谁看
/// 运营数据就得把 adminApiKey 交给他——而那把钥匙的权限严格大于看板所需。
/// 这一侧只认 portal 会话 + 角色，授权粒度对得上用途。
async fn admin_dashboard(State(state): State<PortalState>) -> Response {
    // 积分未启用时看板没有意义（所有聚合恒为 0），按「功能不存在」处理，
    // 与 wallet / board 同一口径。
    if !credits_enabled() {
        return not_found();
    }

    let now = now_ms();
    match state
        .auth
        .db()
        .dashboard(today_start_ms(now), now - FAIL_WINDOW_MS, DASHBOARD_TOP)
    {
        Ok(d) => (StatusCode::OK, Json(d)).into_response(),
        Err(e) => {
            // 看板是观测功能，故障只该影响它自己。如实回 500 + 一句话，
            // 不把 SQL 错误原文外发（那会暴露表结构）。
            tracing::error!("Portal 看板聚合失败: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody {
                    error: "看板数据读取失败".to_string(),
                }),
            )
                .into_response()
        }
    }
}

/// 审计查询串。全部可选，缺省即「不筛」。
///
/// 【为何 limit/offset 也是 Option】给了默认值就无法区分「用户没传」和「用户传了
/// 正好等于默认值的数」。这里区分它没意义，但 `Option` 让 `unwrap_or` 出现在
/// 一处、默认值只写一遍——`serde(default)` 配合裸 i64 会把 0 当成合法 limit，
/// 于是「不传 limit」变成「要 0 条」，页面显示一片空白而不报错。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuditParams {
    username: Option<String>,
    action: Option<String>,
    action_prefix: Option<String>,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    offset: Option<i64>,
    limit: Option<i64>,
}

impl AuditParams {
    /// 转成 store 层的查询条件。`max_limit` 由调用方给（分页与导出上限不同）。
    ///
    /// 空串一律当 `None`：表单里清空一个筛选框后浏览器仍会带上 `username=`，
    /// 若按「精确匹配空串」处理，结果永远是 0 条——用户会以为审计里没数据。
    fn to_query(&self, max_limit: i64, default_limit: i64) -> super::store::AuditQuery {
        let clean = |s: &Option<String>| -> Option<String> {
            s.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        super::store::AuditQuery {
            username: clean(&self.username),
            action: clean(&self.action),
            action_prefix: clean(&self.action_prefix),
            since_ms: self.since_ms,
            until_ms: self.until_ms,
            offset: self.offset.unwrap_or(0).max(0),
            limit: self.limit.unwrap_or(default_limit).clamp(1, max_limit),
        }
    }
}

/// 一页审计。需 portal 会话 + admin 角色。
async fn admin_audit(
    State(state): State<PortalState>,
    axum::extract::Query(p): axum::extract::Query<AuditParams>,
) -> Response {
    let q = p.to_query(super::store::AUDIT_PAGE_MAX, DEFAULT_AUDIT_PAGE);
    match state.auth.db().audit_page(&q) {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(e) => {
            tracing::error!("Portal 审计查询失败: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody {
                    error: "审计数据读取失败".to_string(),
                }),
            )
                .into_response()
        }
    }
}

/// 审计里出现过的动作及次数。供筛选下拉。
async fn admin_audit_actions(State(state): State<PortalState>) -> Response {
    match state.auth.db().audit_actions() {
        Ok(rows) => (StatusCode::OK, Json(rows)).into_response(),
        Err(e) => {
            tracing::error!("Portal 审计动作聚合失败: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody {
                    error: "审计动作读取失败".to_string(),
                }),
            )
                .into_response()
        }
    }
}

/// 导出审计为 CSV。筛选条件与 [`admin_audit`] 完全一致（同一个 `AuditParams`），
/// 只是上限更大、返回 `text/csv` 附件。
///
/// # 为何导出与列表共用一套筛选解析
/// 两处各写一遍的话，「界面上看到 30 条、导出的 CSV 里 812 条」这种不一致会在
/// 加了新筛选维度而只改一处时出现——而运营会拿导出的表去核对，核对的对象却不是
/// 他刚看的那批数据。共用 `AuditParams::to_query` 让这件事从结构上不可能。
///
/// # 截断必须说出来（三处）
/// 库里满足条件的条数可能超过 [`AUDIT_EXPORT_MAX`]。服务端**知道**这件事
/// （`page.has_more`），早先的版本却把它丢了：只取 `page.rows` 转 CSV，于是
/// 运营下到 5000 行、以为那就是全部。列表页有分页条能看到「1–50 / 共 6000 条」，
/// 导出这条路径上一点提示都没有——而导出的表正是拿去核对账目的那一份。
///
/// 【为何要写进 CSV 正文而不只放响应头】导出的消费方式是「双击用 Excel 打开」，
/// 响应头在那个场景里根本不存在。文件里没有的信息等于没有。
/// 但同时也给响应头（`X-Audit-Truncated` / `X-Audit-Total`）：脚本化调用拿不到
/// 也不该去解析 CSV 尾注。两处都给，各自的消费者都能看见。
///
/// # 为何导出自身要写审计
/// 这个端点把**整张审计表（含每个用户的完整 IP）**变成一个可以带走的文件。
/// 明文外显会记 `reveal_keys`，而「把审计日志导出」——一次范围更大的数据出境——
/// 早先一条记录都不写。那意味着审计没有覆盖对审计自身的访问：管理员可以拿走
/// 全部 PII 而事后无痕。这是审计系统的原则性缺口，与「谁在什么时候拿走了什么」
/// 这个审计存在的理由直接冲突。
///
/// 【为何记筛选条件和条数而不只记「导出过」】事后要回答的是「他拿走了哪些数据」。
/// 只记动作名的话，导出一条和导出五千条在审计里长得一样。
async fn admin_audit_export(
    State(state): State<PortalState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    axum::extract::Query(p): axum::extract::Query<AuditParams>,
    request: Request<Body>,
) -> Response {
    let q = p.to_query(
        super::store::AUDIT_EXPORT_MAX,
        super::store::AUDIT_EXPORT_MAX,
    );
    let page = match state.auth.db().audit_page(&q) {
        Ok(page) => page,
        Err(e) => {
            tracing::error!("Portal 审计导出失败: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody {
                    error: "审计导出失败".to_string(),
                }),
            )
                .into_response();
        }
    };

    let exported = page.rows.len() as i64;
    // 截断 = 库里还有满足条件的行没被这一次导出带走。直接用服务端算好的
    // has_more，不在这里重算 `offset+len<total`（重算写错一次就是静默漏报）。
    let truncated = page.has_more;
    let csv = audit_to_csv(&page.rows, page.total, truncated);

    // 导出留痕。
    //
    // 【为何拿不到会话就拒绝导出，而不是「不留痕地照发」】这条路由挂在
    // `require_admin` 之下，所以 `None` 只可能是中间件顺序装错——一个编程错误。
    // 但这里的失败模式很特殊：`if let Some(..)` 那种写法下，装错顺序的表现是
    // **导出照常工作、只是不再留痕**。审计的缺失不会有任何迹象（没有报错、
    // 没有空文件、下载完全正常），而这段代码存在的唯一目的就是让「谁把含全部
    // 用户 IP 的审计表带走了」这件事必然留痕。所以宁可整条路由 401 —— 那是
    // 装错顺序时会立刻被发现的形态。
    let session = match request.extensions().get::<super::store::PortalSession>() {
        Some(s) => s.clone(),
        None => return unauthorized(),
    };
    let ip = client_ip_string(&headers, Some(peer));
    let detail = format!(
        "exported={exported} total={} truncated={} filter={}",
        page.total,
        truncated,
        q.describe_filter()
    );
    // 写审计失败不让导出失败（文件已经生成好了，此时拒绝反而更奇怪），
    // 但必须留下 error 日志——否则「审计缺一条」会无声发生。
    if let Err(e) = state.auth.db().add_audit(
        now_ms(),
        Some(session.user_id),
        Some(&session.username),
        "admin_audit_export",
        ip.as_deref(),
        Some(&detail),
    ) {
        tracing::error!("Portal 审计导出留痕失败（导出仍继续）: {:#}", e);
    }

    // 文件名带时间戳 + 是否截断。
    //
    // 【为何不再用固定名】连续导出两批不同筛选，浏览器给出 `portal-audit.csv`
    // 和 `portal-audit(1).csv`——事后分不清哪个是哪个，而这些文件是拿去核对账的。
    let stamp = file_stamp(now_ms());
    let suffix = if truncated { "-truncated" } else { "" };
    let disposition = format!("attachment; filename=\"portal-audit-{stamp}{suffix}.csv\"");

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (header::CONTENT_DISPOSITION, disposition),
            // 脚本化调用的信号：不必解析 CSV 尾注就能知道拿全了没有。
            ("x-audit-total".parse().unwrap(), page.total.to_string()),
            ("x-audit-truncated".parse().unwrap(), truncated.to_string()),
        ],
        csv,
    )
        .into_response()
}

/// 导出文件名里的时间戳：`20260806-223015`（本地时区）。
///
/// 【为何不用 ISO8601 原样】`:` 在 Windows 文件名里非法，会被浏览器改写成 `_`
/// 或干脆下载失败。这里先把它拿掉，而不是等浏览器各自处理。
fn file_stamp(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms).single() {
        Some(dt) => dt.format("%Y%m%d-%H%M%S").to_string(),
        None => ms.to_string(),
    }
}

/// 审计行转 CSV。
///
/// # 为何自己写而不引 csv crate
/// 需要转义的规则只有三条（引号翻倍、含分隔符/换行则整段加引号），而 CSV 注入
/// 防护那一条**任何 csv 库都不会替你做**——它不属于 CSV 规范，是 Excel 的行为。
/// 引一个依赖仍要自己加那一层，不如把三条规则和它写在一起、就地讲清为什么。
///
/// # UTF-8 BOM
/// 开头写 BOM。不写的话 Excel（Windows 简中版）按 GBK 解 UTF-8，中文全是乱码——
/// 而审计里的 detail 和用户名都可能是中文。BOM 是让「双击打开就是对的」的唯一办法。
/// # 截断尾注
/// `total` 超过导出的行数时，在**正文末尾**追加一行说明。Excel 里能直接看到，
/// 而响应头在「双击打开文件」这个场景里不存在。
///
/// 【为何写在末尾而不是开头】开头插一行非表头内容会让 Excel 把它当成表头、
/// 或让「首行是列名」的导入向导整列错位。末尾追加不影响前面每一行的解析。
fn audit_to_csv(rows: &[super::store::AuditEntry], total: i64, truncated: bool) -> String {
    let mut out = String::from("\u{feff}");
    out.push_str("id,time,username,action,clientIp,detail\n");
    for r in rows {
        out.push_str(&r.id.to_string());
        out.push(',');
        // 时间同时给毫秒和可读串没必要——运营拿去 Excel 排序用的是这一列，
        // 给 ISO8601 本地时间串（Excel 能直接识别成时间）。
        out.push_str(&csv_field(&iso_local(r.at_ms)));
        out.push(',');
        out.push_str(&csv_field(r.username.as_deref().unwrap_or("")));
        out.push(',');
        out.push_str(&csv_field(&r.action));
        out.push(',');
        out.push_str(&csv_field(r.client_ip.as_deref().unwrap_or("")));
        out.push(',');
        out.push_str(&csv_field(r.detail.as_deref().unwrap_or("")));
        out.push('\n');
    }

    // 被截断了就说出来。
    //
    // 【为何这一行至关重要】没有它，导出就是一个「静默失真」的出口：库里有 8000 条，
    // 用户下到 5000 行，以为那就是全部——而他拿这份文件去核对账、去回答「这个 IP
    // 撞了多少次」。列表页有分页条兜着（「1–50 / 共 8000 条」），导出这条路上
    // 在加这一行之前没有任何提示。
    //
    // 【为何用 `#` 开头】它不是数据行。`#` 让肉眼和大多数导入工具都能识别成注释，
    // 而不会被当成第 5001 条审计记录去解析（那会多出一行 id 为空的假数据）。
    if truncated {
        out.push_str(&format!(
            "#,,,,,\"本次导出被截断：共 {} 条匹配，仅导出最近 {} 条。请用时间段筛选后分批导出。\"\n",
            total,
            rows.len()
        ));
    }
    out
}

/// 单个 CSV 字段：转义 + **公式注入防护**。
///
/// # 公式注入（CSV injection）
/// Excel/Numbers/LibreOffice 会把以 `=`、`+`、`-`、`@`、Tab、CR 开头的单元格当**公式**
/// 执行。审计里的 `detail` 含管理员自填的备注（`admin_topup` 的 note），用户名也是
/// 用户自己起的——一个叫 `=cmd|'/c calc'!A0` 的用户名，导出后被运营双击打开就是
/// 在运营机器上执行命令。这不是理论风险：它是**存储型**的，攻击者只要注册一次。
///
/// 防法是在前面加一个单引号（Excel 视作「强制文本」，显示时不带这个引号）。
/// 【为何不改成删掉那些字符】审计的价值在于**如实记录**。把用户名改掉，
/// 事后对账时就对不上真实数据了。加前缀既保留原文又不被执行。
fn csv_field(s: &str) -> String {
    let dangerous = s
        .chars()
        .next()
        .is_some_and(|c| matches!(c, '=' | '+' | '-' | '@' | '\t' | '\r'));
    let body = if dangerous {
        format!("'{s}")
    } else {
        s.to_string()
    };
    if body.contains(',') || body.contains('"') || body.contains('\n') || body.contains('\r') {
        format!("\"{}\"", body.replace('"', "\"\""))
    } else {
        body
    }
}

/// 毫秒时间戳 → 本地时区的 ISO8601（`2026-08-06 19:30:00`）。
///
/// 【为何不带时区偏移】Excel 不认 `+08:00` 后缀，带上会让整列变成文本、没法排序。
/// 服务器时区在导出文件名和界面上都有交代。
fn iso_local(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        // 时间戳坏到无法解析时如实给原始毫秒，而不是编一个时间或留空。
        None => ms.to_string(),
    }
}

/// admin 角色校验。**必须叠在 [`require_session`] 之后**——它从 extensions 里取
/// 已校验的会话，自己不解 cookie。
///
/// # 为何单独一层而不是在每个 handler 里写一行判断
/// 「每个 handler 记得判一次」这种纪律会在新增第 4 个 handler 时失效，而失效的表现
/// 是**越权成功**，没有任何报错。G2 的教训正是如此：闸门写在纯函数里，没有任何
/// 测试能证明它被接上了。中间件让「哪些路由要 admin」在路由表上一眼看得见，
/// 新增路由若挂错了子树，`admin_endpoints_reject_non_admin_roles` 会红。
async fn require_admin(request: Request<Body>, next: Next) -> Response {
    match request.extensions().get::<super::store::PortalSession>() {
        Some(s) if s.role.can_manage() => next.run(request).await,
        // 有会话但角色不够 → 403，如实说明（理由见 `forbidden` 的文档）。
        Some(_) => forbidden("需要管理员角色"),
        // 没有会话说明中间件顺序装错了。回 401 而不是 panic：装错顺序是
        // 编程错误，但让它表现成「拒绝访问」比让服务崩掉安全。
        None => unauthorized(),
    }
}

/// 单页 HTML。
async fn page() -> Response {
    Html(super::page::PAGE_HTML).into_response()
}

// ============ 路由 ============

/// 构造 Portal 路由（**绝对路径**，由调用方 `merge` 而非 `nest`）。
///
/// 分成三层，每层的门都不一样：
/// - `feature_gate`：未启用 → 全部 404（含页面本身）。
/// - 公开层：页面、注册、登录（自带节流，不需要会话）。
/// - 私有层：`/portal/api/me`、`/portal/api/keys`，过 `require_session`。
///
/// 登出放公开层：它只需要幂等地清掉 cookie，要求先有有效会话反而怪。
///
/// 【为何用绝对路径 + merge，而不是 `nest("/portal", …)` + 相对路径】
/// `nest` 下的 `"/"` 只匹配 `/portal`，**不匹配 `/portal/`**——后者会 404。
/// 而用户在浏览器地址栏里两种写法都会敲，实测正是如此翻的车。
/// 绝对路径能把 `/portal` 和 `/portal/` 显式注册成同一个 handler，
/// 两种写法都进得去，且「有哪些路径」在这一处看得全，不必再去调用方拼前缀。
pub fn create_router(state: PortalState) -> Router {
    // 页面单独一棵树：它的缓存策略与 API 不同（`no-cache` 可存但须回源验证，
    // 而 API 是 `no-store` 一律不得留存）。分开挂能让两种策略各自只写一处，
    // 不必在同一个 route_layer 里按路径分流——那种写法迟早有人加路由时判错。
    let pages = Router::new()
        // 两种写法都注册：带不带尾斜杠都能打开页面。
        .route("/portal", get(page))
        .route("/portal/", get(page))
        .route_layer(middleware::from_fn(crate::common::http_cache::no_cache));

    let public = Router::new()
        .route("/portal/api/register", post(register))
        .route("/portal/api/login", post(login))
        .route("/portal/api/logout", post(logout));

    let private = Router::new()
        .route("/portal/api/me", get(me))
        .route("/portal/api/keys", get(list_keys))
        .route(
            "/portal/api/keys/{credential_id}/balance/refresh",
            post(refresh_upstream_quota),
        )
        // 上车用 POST：它扣分、写库、改变服务端状态，不是幂等读取。
        // 若做成 GET，浏览器预取、爬虫、甚至聊天软件展开链接都可能凭 cookie
        // 悄悄替用户花掉积分。
        .route("/portal/api/board/{credential_id}", post(board))
        .route("/portal/api/wallet", get(wallet))
        // 运营看板：会话 + admin 角色。
        //
        // 【为何和普通私有路由同一棵树、只多挂一层 route_layer】另起一棵 Router 再
        // merge 的话，`require_session` 就得在两处各装一遍——而两处装配迟早漂移，
        // 漏装的表现是某条 admin 路由完全不校验会话。叠加而非并列，会话那道门
        // 只有一处，admin 那道门也只有一处。
        //
        // route_layer 的作用域是**本 Router 匹配到的路由**，所以 require_admin
        // 只管这一条，不会波及上面的 me/keys/board/wallet。
        .merge(
            Router::new()
                .route("/portal/api/admin/dashboard", get(admin_dashboard))
                .route("/portal/api/admin/audit", get(admin_audit))
                .route("/portal/api/admin/audit/actions", get(admin_audit_actions))
                .route("/portal/api/admin/audit.csv", get(admin_audit_export))
                .route_layer(middleware::from_fn(require_admin)),
        )
        // **必须是 route_layer，不能是 layer。**
        //
        // `layer` 会把中间件套在整个 Router 上，**包括 fallback**；而本路由被 merge 进
        // 主 app 之后，那个 fallback 承接的是全站所有未匹配路径。于是 `require_session`
        // 会去校验 `/foo`、`/definitely-not-real` 这类请求，把本该 404 的路径变成
        // `401 未登录或会话已过期`——等于向任何扫描者宣告"这里有个需要登录的系统"，
        // 而且 admin UI 的 SPA fallback 也一并被挡在门外。
        //
        // `route_layer` 只作用于**本 Router 匹配到的**路由，未匹配的请求原样落到
        // fallback。冒烟测试正是这样抓到的：v16 对 /foo 返回 404，装上 portal 后变成 401。
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_session,
        ));

    public
        .merge(private)
        // **所有 API 响应一律 no-store。**
        //
        // 装在这里（而非逐个 handler）是因为逐个加有个确定的失效模式：日后新增端点
        // 必然漏，且漏掉不会有编译错误、不会有用例变红，表现是新端点悄悄变成可缓存的。
        //
        // 为何 `no-store` 而不是 `no-cache`：`/portal/api/keys` 在积分关闭或用户已
        // 上车时会下发**明文凭据**（见 `gate_plaintext`）。`no-cache` 是"可以存，但
        // 用前须验证"——明文已经落盘了。而一旦前面挂了 nginx/CDN/公司代理，没有指令
        // 的响应是**允许进共享缓存**的，那就是跨用户串号。
        //
        // 注意顺序：这一层在 route_layer 里比 require_session 更靠外，所以中间件
        // 拒绝请求时生成的 401/403 响应也会被打上头——那些响应体虽不含明文，但同样
        // 不该被缓存（缓存住一个 401 会让用户登录后仍看到 401）。
        .route_layer(middleware::from_fn(crate::common::http_cache::no_store))
        // 页面在 API 的缓存层之外汇入：它要的是 no-cache，不是 no-store。
        // 但必须在 feature_gate 之内——未启用时页面也得 404。
        .merge(pages)
        // 同理用 route_layer：body 上限与 feature_gate 都只该管 portal 自己的路由。
        // feature_gate 用 layer 的危害更隐蔽——它对未匹配路径返回 404，看起来"正常"，
        // 但那会把 admin UI 的 SPA fallback 一起吃掉（portal 未启用时整个 /admin 前端白屏）。
        .route_layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .route_layer(middleware::from_fn(feature_gate))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: &str = "ksk_secret_value";

    /// 默认 `can_reveal = true`（可取明文的角色），让既有断言保持原本的语义：
    /// 它们测的是「积分/上车/号类型」这三个维度，角色维度由下面的专项用例覆盖。
    fn gate(is_api_key: bool, credits_on: bool, aboard: bool) -> (Option<String>, &'static str) {
        gate_plaintext(Some(K.to_string()), is_api_key, credits_on, aboard, true)
    }

    /// **只读角色在任何组合下都拿不到明文。**
    ///
    /// # 为何必须单独一条，而不能靠上面那些用例
    /// 上面的 `gate()` 辅助函数把 `can_reveal` 固定成 `true`——它测的是另外三个
    /// 维度。而 `key_kind_plain_iff_key_present` 虽然遍历了 `can_reveal`，但它只
    /// 断言「`key` 有值 ⟺ `kind == "plain"`」这条**一致性**：闸门被整段删掉后
    /// 返回的 `(Some(k), "plain")` 依然自洽，那条断言照样通过。
    ///
    /// 实测确认过这一点：删掉 `if !can_reveal` 整段后，138 个 portal 用例**全绿**。
    /// 所以只读角色这条红线在此之前是没有任何测试守着的。
    ///
    /// 这里穷举剩下三个维度的全部 8 种组合，逐一断言只读角色拿不到明文。
    #[test]
    fn readonly_role_never_gets_plaintext_in_any_combination() {
        for is_api_key in [false, true] {
            for credits_on in [false, true] {
                for aboard in [false, true] {
                    let (k, kind) = gate_plaintext(
                        Some(K.to_string()),
                        is_api_key,
                        credits_on,
                        aboard,
                        false, // 只读角色
                    );
                    assert_eq!(
                        k, None,
                        "只读角色拿到了明文 (api={is_api_key} credits={credits_on} aboard={aboard})"
                    );
                    assert_ne!(
                        kind, "plain",
                        "只读角色的 key_kind 不该是 plain (api={is_api_key} credits={credits_on} aboard={aboard})"
                    );
                }
            }
        }

        // 对照组：同样的输入换成可取明文的角色，**必须**能拿到。
        //
        // 没有这一组，上面全 None 也可能是因为闸门把所有人都挡了——那是另一个
        // 严重 bug（付了分也拿不到），却会让这个用例照样绿。
        let (k, kind) = gate_plaintext(Some(K.to_string()), true, false, false, true);
        assert_eq!(
            k.as_deref(),
            Some(K),
            "对照组失败：可取明文的角色也被挡住了"
        );
        assert_eq!(kind, "plain");
    }

    /// **本仓最重要的一条安全断言。**
    ///
    /// 穷举 `is_api_key × credits_on × aboard` 全部 8 种组合，逐一断言明文去留。
    /// 写成穷举而不是挑几个代表：这个门只有三个布尔输入，8 种全列出来的成本极低，
    /// 而漏测的那一种恰好就是明文泄漏的那一种——概率不站在我们这边。
    #[test]
    fn plaintext_leaves_only_when_credits_off_or_aboard() {
        // 积分未启用：行为与加积分之前完全一致，API Key 号照常给明文。
        assert_eq!(
            gate(true, false, false).0.as_deref(),
            Some(K),
            "积分关闭时应给明文"
        );
        assert_eq!(gate(true, false, true).0.as_deref(), Some(K));

        // 积分启用 + 已上车：这是唯一「花了分之后」该拿到明文的情形。
        assert_eq!(
            gate(true, true, true).0.as_deref(),
            Some(K),
            "已上车应给明文"
        );

        // 积分启用 + 未上车：**明文必须为 None**。
        assert_eq!(gate(true, true, false).0, None, "未上车绝不能下发明文");

        // OAuth 号：任何组合都不给明文（refresh_token 泄漏等于 AWS 账号失守）。
        for credits_on in [false, true] {
            for aboard in [false, true] {
                assert_eq!(
                    gate(false, credits_on, aboard).0,
                    None,
                    "OAuth 号不该外显明文 (credits_on={credits_on}, aboard={aboard})"
                );
            }
        }
    }

    /// 没有明文可给时，也要分清是「本来就没有」还是「要花分才有」。
    ///
    /// 这不是显示细节：把 OAuth 号标成 `locked` 会让用户为一把根本拿不到的 key
    /// 付积分，那是实质性的欺骗。
    #[test]
    fn key_kind_distinguishes_locked_from_unavailable() {
        assert_eq!(
            gate(true, true, false).1,
            "locked",
            "有明文但未上车 → locked"
        );
        assert_eq!(gate(true, true, true).1, "plain");
        assert_eq!(gate(false, true, false).1, "oauth", "OAuth 号不标 locked");

        // API Key 类型但明文缺失（号刚被删）：标 none，不诱导上车。
        let (k, kind) = gate_plaintext(None, true, true, false, true);
        assert_eq!(k, None);
        assert_eq!(kind, "none", "取不到明文时不该标 locked");

        // OAuth 且无明文。
        assert_eq!(gate_plaintext(None, false, true, false, true).1, "oauth");
    }

    /// `key_kind == "plain"` 与「`key` 有值」必须永远同真同假。
    ///
    /// 两者一旦脱钩，前端就会依据 `key_kind` 显示「可复制」却复制到空字符串，
    /// 或反过来：标着 locked 但 `key` 里躺着明文——后者是静默泄漏。
    #[test]
    fn key_kind_plain_iff_key_present() {
        for plain in [None, Some(K.to_string())] {
            for is_api_key in [false, true] {
                for credits_on in [false, true] {
                    for aboard in [false, true] {
                        for can_reveal in [false, true] {
                            let (k, kind) = gate_plaintext(
                                plain.clone(),
                                is_api_key,
                                credits_on,
                                aboard,
                                can_reveal,
                            );
                            assert_eq!(
                                k.is_some(),
                                kind == "plain",
                                "key 与 key_kind 脱钩: plain={:?} api={is_api_key} on={credits_on} aboard={aboard} reveal={can_reveal} → ({k:?}, {kind})",
                                plain.is_some()
                            );
                        }
                    }
                }
            }
        }
    }

    /// 计价参数的运行时镜像可热更，且非法值被纠正。
    #[test]
    fn pricing_mirror_hot_swaps_and_sanitizes() {
        let _g = crate::common::auth_keys::test_serial();

        set_pricing(crate::portal::credits::Pricing {
            base_count: 4,
            base_price: 5,
            total_price: 20,
            min_price: 1,
            max_unlockers: 10,
        });
        let p = pricing();
        assert_eq!(p.base_count, 4);
        assert_eq!(p.unit_price(1), 5, "前 4 人 5 分");
        assert_eq!(p.unit_price(10), 2, "第 10 人 ceil(20/10)=2");

        // 非法值：base_count=0 会让「前几人固定价」这段消失，必须被纠正成 1。
        set_pricing(crate::portal::credits::Pricing {
            base_count: 0,
            base_price: 10,
            total_price: 20,
            min_price: -5,
            max_unlockers: 0,
        });
        let p = pricing();
        assert!(p.base_count >= 1, "base_count 不能是 0");
        assert!(p.min_price >= 0, "min_price 为负等于「上车送分」");
        assert!(p.max_unlockers >= 1, "max=0 会让任何人都上不了车");

        set_pricing(crate::portal::credits::Pricing::default());
    }

    /// **结构性断言：明文只允许有一条判定路径。**
    ///
    /// 上面那些用例证明了 [`gate_plaintext`] 本身正确，但证明不了「所有下发明文的
    /// 地方都走了它」——真正的泄漏往往不是闸门算错，而是有人新写了一条绕过闸门的
    /// 路径。事实上本文件就有过两份判定（列表页一份、上车下发一份），逻辑当时凑巧
    /// 一致，但收紧其中一处、漏掉另一处不会有任何编译错误。
    ///
    /// 于是扫源码：`kiro_api_key`（明文字段）只允许在两处被读——列表页和
    /// `lookup_plain`——且两处都必须紧接着交给闸门。数字对不上时**先看是不是
    /// 新增了合法的第三处**，若是，那一处必须也走 `gate_plaintext`，然后改这里的期望值。
    ///
    /// 只扫 `#[cfg(test)]` 之前的部分，否则本用例自身的字面量会污染计数。
    #[test]
    fn plaintext_has_exactly_one_gate() {
        let src = include_str!("http.rs");
        let prod = src
            .split("\n#[cfg(test)]")
            .next()
            .expect("split 至少给一段");

        // 对照组：切分若失效（比如日后 cfg(test) 写法变了），prod 会变成整个文件或空串，
        // 下面的计数就毫无意义。先确认切出来的确实是生产代码那一段。
        assert!(
            prod.contains("async fn list_keys"),
            "切分失效：没在生产段里找到 list_keys"
        );
        assert!(
            !prod.contains("fn plaintext_has_exactly_one_gate"),
            "切分失效：测试代码漏进了生产段，计数会被自身污染"
        );

        let reads = prod.matches("kiro_api_key").count();
        assert_eq!(
            reads, 2,
            "读取明文字段的位置数变了（期望 2：list_keys 与 lookup_plain）。\
             新增的那一处必须把结果交给 gate_plaintext 再下发。"
        );

        // 一处定义 + 两处调用。调用数少于读取数，说明有人取了明文却没过闸门。
        let gate_uses = prod.matches("gate_plaintext(").count();
        assert_eq!(
            gate_uses, 3,
            "gate_plaintext 的定义+调用数变了（期望 3 = 1 定义 + 2 调用）"
        );
    }

    /// **结构性断言：用户页的凭据列表只能来自实时池，绝不能碰回收站。**
    ///
    /// 用户的要求是「显示的号 100% 跟随凭据管理页，删掉的不能出现」。这一条目前
    /// 成立的**唯一原因**是 `list_keys` 用 `manager.snapshot()`（只读 entries），
    /// 而软删除会把号移到 trash。但这是个隐式依赖：日后有人想「顺手把回收站的号
    /// 也列出来标成已删除」，加一行 `list_trash()` 就破了，且不会有任何编译错误、
    /// 任何现有用例变红——表现是被删的号在用户页重新出现，还带着可上车的按钮。
    ///
    /// 所以在源码层面钉死：本文件不得出现 `list_trash` / `trash`。
    #[test]
    fn portal_list_never_sources_from_trash() {
        let src = include_str!("http.rs");
        let prod = src
            .split("\n#[cfg(test)]")
            .next()
            .expect("split 至少给一段");

        // 对照组：切分失效时 prod 会变成整个文件或空串，下面的断言就毫无意义。
        assert!(
            prod.contains("async fn list_keys"),
            "切分失效：没在生产段里找到 list_keys"
        );
        assert!(
            !prod.contains("fn portal_list_never_sources_from_trash"),
            "切分失效：测试代码漏进了生产段，本用例会被自身的字面量污染"
        );

        // 正向：必须确实从实时池取数。若哪天改成读缓存或读文件，这条会红——
        // 那时「跟随管理页」就不再自动成立，必须重新论证。
        assert!(
            prod.contains("state.manager.snapshot()"),
            "list_keys 不再从 manager.snapshot()（实时 entries）取数，\
             「跟随凭据管理页」的前提已被打破，需重新验证删号后是否立刻消失"
        );

        // 反向：不得引入任何回收站来源。
        for needle in ["list_trash", "trash"] {
            assert!(
                !prod.contains(needle),
                "portal 用户页出现了 `{needle}`：回收站里的号是**已被管理员删掉**的号，\
                 在这一页显示它们等于让用户对着删掉的车票付分。"
            );
        }
    }

    /// 积分开关默认关闭。
    ///
    /// 默认关闭是有意的：升级到带积分的版本后，老部署的行为不该在管理员
    /// 没做任何配置的情况下改变（否则所有用户突然看不到 key 了）。
    #[test]
    fn credits_disabled_by_default() {
        let _g = crate::common::auth_keys::test_serial();
        // 其它用例可能开过，先复位再验默认语义。
        set_credits_enabled(false);
        assert!(!credits_enabled());
        set_credits_enabled(true);
        assert!(credits_enabled());
        set_credits_enabled(false);
    }

    /// **回归测试：portal 的中间件不得吃掉不属于它的请求。**
    ///
    /// 【这个 bug 长什么样】最初用 `.layer()` 挂 `require_session` 与 `feature_gate`。
    /// axum 的 `.layer()` 会连同 Router 的 **fallback** 一起包裹，于是 merge 进主 app 后，
    /// 任何未匹配的路径（`/foo`、`/api/nope`）都要先过 portal 的会话校验，
    /// 返回的是 `401 未登录或会话已过期` —— 而改动前是 404。
    ///
    /// 后果不只是状态码难看：它把「这台机器上跑着一个需要登录的 portal」这件事
    /// 泄露给了任何一次扫描，而 portal 的设计前提正是「未启用时连存在都不确认」。
    /// 修法是 `route_layer`，它只作用于本 Router 真正匹配到的路由。
    ///
    /// 断言方式是扫源码而非起服务：起 axum 服务要 tokio runtime + 端口，
    /// 而这条不变量的本质就是「别写 .layer()」，扫源码足够且更快。
    /// 冒烟测试里有对应的真实 HTTP 验证（未知路径必须 404）。
    #[test]
    fn portal_middleware_must_not_swallow_unmatched_requests() {
        let src = include_str!("http.rs");
        // 只截取生产 create_router 函数。文件前部可能存在局部 #[cfg(test)] 模块，
        // 不能再用 split("#[cfg(test)]")；那会在 create_router 之前提前截断。
        let start = src
            .find("pub fn create_router(state: PortalState)")
            .expect("缺少 create_router");
        let tail = &src[start..];
        let end = tail
            .find("\n#[cfg(test)]\nmod tests")
            .expect("缺少 create_router 后的测试模块锚点");
        let body = &tail[..end];

        let code_lines: Vec<&str> = body.lines().map(str::trim).collect();
        assert!(
            code_lines
                .iter()
                .any(|line| line.starts_with(".route_layer(")),
            "create_router 必须用 route_layer 挂中间件"
        );
        assert!(
            !code_lines.iter().any(|line| line.starts_with(".layer(")),
            "create_router 里出现了 .layer(——它会连 fallback 一起包裹，\n\
             导致主 app 上任何未匹配路径都返回 portal 的 401 而非 404。\n\
             改用 .route_layer()。"
        );
    }

    // ============ 上车端点的角色闸门（router 级） ============
    //
    // 【为何必须打真实 HTTP 而不是直接调 handler】`board` 是 async handler，
    // 它的只读闸门位于函数体开头。把闸门删掉后，任何只测 `gate_plaintext`
    // 纯函数的用例都照样通过——实测如此：第一版变异注入（删掉 board 的
    // `can_board` 检查）没有任何测试变红，因为那条路径根本没被测到。
    // 走 `oneshot` 发一次真请求，闸门才在覆盖范围内。

    /// 造一个只有一把 API Key 凭据的凭据池。
    fn one_key_manager() -> Arc<MultiTokenManager> {
        let mut c = crate::kiro::model::credentials::KiroCredentials::default();
        c.id = Some(9001);
        c.kiro_api_key = Some(K.to_string());
        c.auth_method = Some("api_key".to_string());
        Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![c],
                None,
                None,
                false,
            )
            .expect("建凭据池失败"),
        )
    }

    /// 注册一个用户并把它设成指定角色，返回 (state, 会话 cookie 值)。
    fn state_with_user(role: super::super::role::RoleKind) -> (PortalState, String) {
        let db = Arc::new(super::super::store::PortalDb::open_in_memory().unwrap());
        let auth = Arc::new(PortalAuth::new(db.clone()));

        crate::common::auth_keys::set_portal_invite_code("board-gate-code").unwrap();
        let ok = auth
            .register(
                "boarder",
                "correct horse battery",
                "board-gate-code",
                None,
                now_ms(),
            )
            .expect("注册失败");
        let uid = db.find_user_by_name("boarder").unwrap().unwrap().id;
        db.set_role(uid, role).unwrap();
        // 余额给足：这样"上不了车"只可能是角色闸门，不会是积分不足。
        db.adjust_balance(uid, 1000, "admin_topup", None, None, now_ms())
            .unwrap()
            .unwrap();

        (PortalState::new(auth, one_key_manager()), ok.token)
    }

    /// 发一次上车请求，返回 (状态码, 响应体文本)。
    async fn post_board(state: PortalState, token: &str, cred_id: u64) -> (StatusCode, String) {
        use tower::ServiceExt;

        let mut req = Request::builder()
            .method("POST")
            .uri(format!("/portal/api/board/{cred_id}"))
            .header(header::COOKIE, format!("{COOKIE_NAME}={token}"))
            .body(Body::empty())
            .unwrap();

        // `board` 取 `ConnectInfo` 用于审计里的来源 IP。真实服务由
        // `into_make_service_with_connect_info` 注入，而 `oneshot` 不会——
        // 少了它 handler 会在提取阶段就 500，看起来像业务错误。手动补上。
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                40000,
            ))));

        let res = create_router(state).oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// 发一次真实的额度刷新 HTTP 请求。测试状态不注入 AdminService，因此所有用例
    /// 都会在权限、凭据存在性或“服务未启用”处返回，绝不会访问 Kiro 上游。
    async fn post_quota_refresh(
        state: PortalState,
        token: Option<&str>,
        cred_id: u64,
    ) -> (StatusCode, String, Option<String>) {
        use tower::ServiceExt;

        let mut builder = Request::builder()
            .method("POST")
            .uri(format!("/portal/api/keys/{cred_id}/balance/refresh"));
        if let Some(token) = token {
            builder = builder.header(header::COOKIE, format!("{COOKIE_NAME}={token}"));
        }
        let request = builder.body(Body::empty()).unwrap();
        let response = create_router(state).oneshot(request).await.unwrap();
        let status = response.status();
        let retry_after = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        (
            status,
            String::from_utf8_lossy(&bytes).to_string(),
            retry_after,
        )
    }

    #[tokio::test]
    async fn quota_refresh_requires_a_valid_session() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(true);
        let (state, _) = state_with_user(super::super::role::RoleKind::User);

        let (status, body, _) = post_quota_refresh(state, None, 9001).await;

        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "未登录不应刷新额度: {body}"
        );
        set_credits_enabled(false);
        set_enabled(false);
    }

    #[tokio::test]
    async fn readonly_role_cannot_refresh_quota() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(true);
        let (state, token) = state_with_user(super::super::role::RoleKind::Readonly);

        let (status, body, _) = post_quota_refresh(state, Some(&token), 9001).await;

        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "只读角色不应刷新额度: {body}"
        );
        assert!(body.contains("只读"), "拒绝原因应明确: {body}");
        set_credits_enabled(false);
        set_enabled(false);
    }

    /// **未上车的用户也必须能刷新额度。**
    ///
    /// 这条替换了原先那条 `user_must_be_aboard_before_refresh_when_credits_are_enabled`
    /// ——那是旧需求。改的原因不是「门太严」而是它把功能变成了空壳：额度数字对所有
    /// 登录用户可见（`fillQuota` 无条件渲染），只把**刷新**挡住的结果是未上车的人
    /// 对着一个不知何时的旧数字，而他正要靠这个数字决定值不值得花分上车。
    /// 实测生产上没有任何人上过当前那把 key 的车，于是全员都没有刷新按钮。
    ///
    /// 【为何断言 503 而不是 200】测试状态不注入 `AdminService`，所以放行之后会在
    /// 「额度服务未启用」处停下。503 恰好证明请求**穿过了权限门**——若那道门还在，
    /// 这里会是 403。用 503 当通行证明比伪造一个上游更可靠，也绝不会真打 AWS。
    #[tokio::test]
    async fn user_not_aboard_can_still_refresh_quota() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(true);
        let (state, token) = state_with_user(super::super::role::RoleKind::User);

        let (status, body, _) = post_quota_refresh(state, Some(&token), 9001).await;

        assert_ne!(
            status,
            StatusCode::FORBIDDEN,
            "未上车用户被挡住了——额度这一列会长期显示陈旧值且无法更新: {body}"
        );
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "应穿过权限门后停在「额度服务未启用」: {body}"
        );
        set_credits_enabled(false);
        set_enabled(false);
    }

    /// **按钮的显示条件必须与端点的放行条件一致。**
    ///
    /// 这两处在源码里是分开的（一个算 `can_refresh_upstream_quota` 决定画不画按钮，
    /// 一个在 handler 里决定放不放行）。不一致的表现有两种，都很难从界面上看出来：
    /// 按钮画了但点下去 403，或者按钮没画而接口其实放行。
    /// 这里扫源码而非起服务——要锁的正是「两处别再长出一个不对称的条件」。
    #[test]
    fn quota_refresh_button_and_endpoint_agree_on_aboard() {
        let src = include_str!("http.rs");
        // 只扫描生产 list_keys 函数。文件前部允许有局部测试模块，不能按首个
        // #[cfg(test)] 截断，否则会只看到结构体字段声明而漏掉实际赋值。
        let start = src.find("async fn list_keys(").expect("缺少 list_keys");
        let tail = &src[start..];
        let end = tail
            .find("\nasync fn refresh_upstream_quota(")
            .expect("缺少 refresh_upstream_quota 锚点");
        let prod = &tail[..end];

        // 取出那一整行做**精确**比对，而不是 contains 一个前缀。
        //
        // 【为何必须整行】第一版这里写的是
        // `prod.contains("can_refresh_upstream_quota: upstream_quota_available && can_reveal")`，
        // 而它是「重新加上 && onboard」那个版本的前缀——变异注入实测**没被抓到**，
        // 测试照样绿。锁一个条件表达式时，前缀匹配等于没锁。
        // 同名前缀在文件里出现两次：结构体里的类型声明（`: bool,`）和 list_keys 里的
        // 赋值。声明在前，所以裸 find 抓到的是它——第二版实测就这么红了，报错写着
        // 「实际: can_refresh_upstream_quota: bool,」。排掉声明只留赋值。
        let lines: Vec<&str> = prod
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("can_refresh_upstream_quota:") && !l.ends_with("bool,"))
            .collect();
        assert_eq!(
            lines.len(),
            1,
            "期望恰好一处 can_refresh_upstream_quota 赋值，实得 {}：{lines:?}",
            lines.len()
        );
        let line = lines[0];
        assert_eq!(
            line, "can_refresh_upstream_quota: upstream_quota_available && can_reveal,",
            "按钮的显示条件变了（实际: {line}）。若重新加入 onboard/aboard 判断，\n\
             未上车用户会再次失去刷新按钮——额度列就又变成只能看陈旧值。"
        );
        assert!(
            !prod.contains("上车后才能刷新"),
            "handler 里又出现了「上车后才能刷新」这道门。\n\
             它与上面的按钮条件不一致：按钮会画出来，点下去 403。"
        );
    }

    #[tokio::test]
    async fn quota_refresh_rejects_missing_credentials_before_service_access() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(false);
        let (state, token) = state_with_user(super::super::role::RoleKind::User);

        let (status, body, _) = post_quota_refresh(state, Some(&token), 9999).await;

        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "不存在凭据应返回 404: {body}"
        );
        set_enabled(false);
    }

    #[tokio::test]
    async fn quota_refresh_reports_unavailable_when_shared_service_is_missing() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(false);
        let (state, token) = state_with_user(super::super::role::RoleKind::User);

        let (status, body, _) = post_quota_refresh(state, Some(&token), 9001).await;

        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "未注入共享额度服务时不能伪装成刷新成功: {body}"
        );
        assert!(body.contains("额度服务未启用"), "错误原因应明确: {body}");
        set_enabled(false);
    }

    #[test]
    fn quota_refresh_gate_is_per_credential_and_returns_retry_after() {
        let gate = BalanceRefreshGate::default();
        assert_eq!(gate.acquire(9001), Ok(()));
        let retry_after = gate.acquire(9001).expect_err("同一凭据连续刷新必须限流");
        assert!((1..=60).contains(&retry_after));
        assert_eq!(gate.acquire(9002), Ok(()), "不同凭据不应互相阻塞");
    }

    /// **只读角色打上车接口必须 403，且响应体里绝不能有明文。**
    ///
    /// 两条断言缺一不可：只断言状态码的话，一个「403 但顺手把 key 也发了」的
    /// 实现照样通过；只断言无明文的话，「200 但 key 字段为 null」也算过——
    /// 而那时分已经扣了。
    #[tokio::test]
    async fn readonly_role_cannot_board_over_http() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(true);

        let (state, token) = state_with_user(super::super::role::RoleKind::Readonly);
        let db = state.auth.db().clone();
        let before = db.balance_of(1).unwrap();

        let (status, body) = post_board(state, &token, 9001).await;

        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "只读角色上车必须 403，实际响应: {body}"
        );
        assert!(
            !body.contains(K),
            "403 响应里出现了明文 key —— 闸门拦了状态码却没拦数据: {body}"
        );
        assert_eq!(db.balance_of(1).unwrap(), before, "被拒的上车不得扣分");
        assert!(
            !db.is_aboard(1, 9001).unwrap(),
            "被拒的上车不得在库里留下上车记录"
        );

        set_credits_enabled(false);
        set_enabled(false);
    }

    /// 对照组：**普通用户走同一条路必须成功并拿到明文**。
    ///
    /// 没有这一条，上面那个 403 可能只是因为整条路由挂了（凭据不存在、
    /// 会话无效、积分没开），而不是角色闸门起了作用。
    #[tokio::test]
    async fn plain_user_can_board_over_http() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(true);

        let (state, token) = state_with_user(super::super::role::RoleKind::User);
        let db = state.auth.db().clone();

        let (status, body) = post_board(state, &token, 9001).await;

        assert_eq!(status, StatusCode::OK, "普通用户应能上车，实际响应: {body}");
        assert!(
            body.contains(K),
            "上车成功却没拿到明文（对照组失败，说明这条路本身不通）: {body}"
        );
        assert!(db.is_aboard(1, 9001).unwrap(), "上车成功应有库记录");

        set_credits_enabled(false);
        set_enabled(false);
    }

    /// admin 角色也能上车：运营者本身是用户，且需要能自测这条路是通的。
    #[tokio::test]
    async fn admin_role_can_also_board() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(true);

        let (state, token) = state_with_user(super::super::role::RoleKind::Admin);
        let (status, body) = post_board(state, &token, 9001).await;

        assert_eq!(status, StatusCode::OK, "admin 应能上车: {body}");
        assert!(body.contains(K), "admin 上车应拿到明文: {body}");

        set_credits_enabled(false);
        set_enabled(false);
    }

    /// `/portal/api/me` 必须下发 `role` 与 `canBoard`。
    ///
    /// 【为何要锁这两个字段名】页面用 `canBoard` 决定是否渲染上车按钮。改名或漏发
    /// 不会有任何编译错误——JS 读到 `undefined`，而 `CAN_BOARD` 的默认值是 `true`，
    /// 于是只读账号又看到了按钮。那是个纯显示回归（服务端仍会 403），但它会让
    /// 只读用户每次都点一下才知道不行。字段名是页面与服务端之间的契约，
    /// 契约得有测试。
    #[tokio::test]
    async fn me_endpoint_exposes_role_and_can_board() {
        use tower::ServiceExt;
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);

        for (role, expect_role, expect_board) in [
            (super::super::role::RoleKind::Readonly, "readonly", false),
            (super::super::role::RoleKind::User, "user", true),
            (super::super::role::RoleKind::Admin, "admin", true),
        ] {
            let (state, token) = state_with_user(role);
            let req = Request::builder()
                .method("GET")
                .uri("/portal/api/me")
                .header(header::COOKIE, format!("{COOKIE_NAME}={token}"))
                .body(Body::empty())
                .unwrap();
            let res = create_router(state).oneshot(req).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{role:?} 的 /me 应 200");
            let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

            assert_eq!(
                v.get("role").and_then(|x| x.as_str()),
                Some(expect_role),
                "{role:?} 的 role 字段不符: {v}"
            );
            assert_eq!(
                v.get("canBoard").and_then(|x| x.as_bool()),
                Some(expect_board),
                "{role:?} 的 canBoard 字段不符（页面据此渲染按钮）: {v}"
            );
        }

        set_enabled(false);
    }

    /// 积分**关闭**时，只读角色仍然不该在列表里看到明文。
    ///
    /// 【为何这条独立于上车】积分关闭时压根没有"上车"这回事，明文是直接下发的。
    /// 若只读闸门写在积分分支里，关掉积分就等于把只读账号提权成能看全部 key——
    /// 而"演示账号"最可能的部署方式恰恰是不开积分。
    #[test]
    fn readonly_sees_no_plaintext_even_with_credits_off() {
        for credits_on in [false, true] {
            for aboard in [false, true] {
                let (k, kind) = gate_plaintext(
                    Some(K.to_string()),
                    true,
                    credits_on,
                    aboard,
                    false, // 只读
                );
                assert_eq!(
                    k, None,
                    "只读角色拿到了明文 (credits_on={credits_on}, aboard={aboard})"
                );
                assert_eq!(kind, "forbidden");
            }
        }
        // 对照组：可取明文的角色在积分关闭时确实拿得到，否则上面全 None
        // 可能只是因为闸门恒返回 None。
        assert_eq!(
            gate_plaintext(Some(K.to_string()), true, false, false, true)
                .0
                .as_deref(),
            Some(K),
            "对照组失败：普通角色在积分关闭时也拿不到明文"
        );
    }

    // ============ 运营看板端点（router 级） ============
    //
    // 与上面上车闸门同一个理由：`require_admin` 是中间件，只有真发 HTTP 请求
    // 才能证明它被挂在了这条路由上。G2 的教训是「纯函数测试证明锁造得好，
    // 但没证明门上装了锁」——这里不再重犯。

    /// 看板路由。多处引用，写成常量免得改路径时漏改一处。
    const DASH: &str = "/portal/api/admin/dashboard";

    /// 从 `create_router` 的源码里抓出**全部** `/portal/api/admin/` 路由路径。
    ///
    /// # 为何不手写清单
    /// 手写会漏。而漏掉的那条路由的越权测试就此不存在——表现是「新加的 admin
    /// 接口对任何登录用户开放」，且所有测试全绿。G2 正是这么翻的车：闸门造好了，
    /// 但没有任何测试证明它被装在了新那条门上。从源码抓，新增路由自动进入矩阵。
    fn admin_routes() -> Vec<String> {
        let src = include_str!("http.rs");
        let start = src
            .find("pub fn create_router")
            .expect("必须有 create_router");
        let end = src[start..]
            .find("#[cfg(test)]")
            .map(|i| start + i)
            .unwrap_or(src.len());
        let body = &src[start..end];

        let pat = ".route(\"";
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(p) = body[from..].find(pat) {
            let s = from + p + pat.len();
            match body[s..].find('"') {
                Some(q) => {
                    let path = &body[s..s + q];
                    if path.starts_with("/portal/api/admin/") {
                        out.push(path.to_string());
                    }
                    from = s + q;
                }
                None => break,
            }
        }
        out
    }

    /// 发一次 GET 请求。`token = None` 表示不带 cookie（测未登录）。
    ///
    /// 【为何 uri 是参数】看板、审计、审计导出、动作聚合是四条路由，共用同一道
    /// `require_admin`。写死 `/dashboard` 的话，后三条的越权测试就只能靠「记得
    /// 各写一遍」——而漏写的表现是**越权成功且无人发现**，正是 G2 踩过的那个坑。
    async fn get_as(state: PortalState, uri: &str, token: Option<&str>) -> (StatusCode, String) {
        use tower::ServiceExt;

        let mut b = Request::builder().method("GET").uri(uri);
        if let Some(t) = token {
            b = b.header(header::COOKIE, format!("{COOKIE_NAME}={t}"));
        }
        let mut req = b.body(Body::empty()).unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                40001,
            ))));

        let res = create_router(state).oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 256 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// **非 admin 角色一律 403，admin 通行。**
    ///
    /// 【为何三档一起测而不只测被拒的两档】只断言「readonly/user 被拒」的话，
    /// 一个把整条路由挂错、对所有人都 403 的装配也能全绿——那时管理员自己也
    /// 进不去，功能等于不存在，而测试却是绿的。admin 那一档是对照组，
    /// 它证明这道门拒的是角色而不是所有人。
    #[tokio::test]
    async fn admin_endpoints_reject_non_admin_roles() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(true);

        // 逐条扫**源码里实际注册的** admin 路由，而不是手写一份清单。
        //
        // 【为何从源码抓】手写清单在新增第五条路由时会漏，而漏掉的那条恰恰是最
        // 新、最没被人看过的一条，它的越权就此无人知晓。这里让新增路由自动进入
        // 越权矩阵：忘了挂 require_admin，本测试当场红。
        let routes = admin_routes_from_source();
        assert!(
            routes.len() >= 4,
            "对照组失败：只从源码抓到 {} 条 admin 路由（{routes:?}），提取器大概失效了",
            routes.len()
        );

        for uri in &routes {
            for role in [
                super::super::role::RoleKind::Readonly,
                super::super::role::RoleKind::User,
            ] {
                let (state, token) = state_with_user(role);
                let (status, body) = get_as(state, uri, Some(&token)).await;
                assert_eq!(
                    status,
                    StatusCode::FORBIDDEN,
                    "角色 {} 竟然进得了 {uri}，响应: {body}",
                    role.as_str()
                );
                // 403 的响应体不能捎带任何数据。审计与看板各有自己的标志字段，
                // 一起查：只查看板字段的话，审计路由泄漏了也发现不了。
                for leak in ["balanceSum", "tiers", "\"rows\"", "hasMore"] {
                    assert!(
                        !body.contains(leak),
                        "{uri} 被拒后仍然泄漏了 {leak}: {body}"
                    );
                }
            }

            // 对照组：admin 必须真的进得去。这一档证明门拒的是角色而不是所有人。
            let (state, token) = state_with_user(super::super::role::RoleKind::Admin);
            let (status, body) = get_as(state, uri, Some(&token)).await;
            assert_eq!(status, StatusCode::OK, "admin 进不了 {uri}，响应: {body}");
            assert!(!body.is_empty(), "admin 拿到 200 但 {uri} 响应是空的");
        }
    }

    /// 从 `create_router` 源码里抓出注册的 admin 路由路径。
    ///
    /// 与 [`every_admin_route_lives_under_the_admin_guard`] 用同一个前缀常量思路：
    /// 路由表是唯一事实来源，测试不维护第二份清单。
    fn admin_routes_from_source() -> Vec<String> {
        let src = include_str!("http.rs");
        let start = src
            .find("pub fn create_router")
            .expect("必须有 create_router");
        let end = src[start..]
            .find("#[cfg(test)]")
            .map(|i| start + i)
            .unwrap_or(src.len());
        let body = &src[start..end];

        const PAT: &str = ".route(\"/portal/api/admin/";
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(p) = body[from..].find(PAT) {
            let s = from + p + ".route(\"".len();
            match body[s..].find('"') {
                Some(q) => {
                    out.push(body[s..s + q].to_string());
                    from = s + q;
                }
                None => break,
            }
        }
        out
    }

    /// 未登录必须 401（而不是 403 或 200）。
    ///
    /// 【为何单独一条】admin 那道门从 extensions 取会话。若两层中间件的顺序装反，
    /// `require_admin` 会先跑、取不到会话——此时它回 401 是对的，但**会话根本
    /// 没被校验过**，任何伪造 cookie 都会走到同样的分支。这条用例确认无 cookie
    /// 时被挡在最外层，配合下面那条结构测试锁住顺序。
    #[tokio::test]
    async fn admin_dashboard_requires_session() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(true);

        let (state, _token) = state_with_user(super::super::role::RoleKind::Admin);
        let (status, _) = get_as(state, DASH, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "无 cookie 必须 401");

        // 伪造的 token 同样是 401，不能因为「格式看起来对」就放行。
        let (state2, _) = state_with_user(super::super::role::RoleKind::Admin);
        let (status2, _) = get_as(state2, DASH, Some("totally-made-up-token")).await;
        assert_eq!(status2, StatusCode::UNAUTHORIZED, "伪造 token 必须 401");
    }

    /// 积分未启用时看板按「功能不存在」处理：404。
    ///
    /// 与 wallet / board 同一口径。回 200 + 全 0 会让人以为「今天真的没人上车」，
    /// 而实际是整套积分没开——那是两种完全不同的处置。
    #[tokio::test]
    async fn admin_dashboard_is_404_when_credits_disabled() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(false);

        let (state, token) = state_with_user(super::super::role::RoleKind::Admin);
        let (status, _) = get_as(state, DASH, Some(&token)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // 对照组：开了就不是 404，否则上面那条可能只是因为路由压根没注册。
        set_credits_enabled(true);
        let (state2, token2) = state_with_user(super::super::role::RoleKind::Admin);
        let (status2, _) = get_as(state2, DASH, Some(&token2)).await;
        assert_eq!(status2, StatusCode::OK, "对照组：开启积分后看板必须可访问");
    }

    /// **看板响应里绝不能出现任何明文 key。**
    ///
    /// 看板聚合的是钱和人数，本不该碰明文；但它按 `credential_id` 组织数据，
    /// 日后若有人「顺手」把凭据信息也 join 进来（为了显示账号名），明文就有了
    /// 一条新的外泄路径——而看板的受众是运营者，权限比凭据管理页低。
    /// 这条断言把那条路径钉死。
    #[tokio::test]
    async fn admin_dashboard_body_never_contains_plaintext_key() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(true);

        let (state, token) = state_with_user(super::super::role::RoleKind::Admin);
        // 先真上一次车，让看板有非零数据——空数据下「不含明文」是废话。
        let (bs, _) = post_board(state.clone(), &token, 9001).await;
        assert_eq!(bs, StatusCode::OK, "前置条件：admin 应能上车");

        let (status, body) = get_as(state, DASH, Some(&token)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("\"tickets\":1") || body.contains("\"tickets\": 1"),
            "前置条件失败：看板应看到刚才那张票，否则下面的断言是空的。body={body}"
        );
        assert!(!body.contains(K), "看板响应里出现了明文 key: {body}");
        assert!(
            !body.contains("ksk_"),
            "看板响应里出现了 key 前缀，可能是新加的字段带出了凭据明文: {body}"
        );
    }

    /// **结构性红线：`/portal/api/admin/` 下的每条路由都必须在 admin 子树里。**
    ///
    /// 【这条要拦的是什么】日后新增一条 admin 接口时，把 `.route(...)` 写在
    /// `private` 上而不是那个 `.merge(Router::new()...)` 里边，是最自然的手误——
    /// 代码照样编译、接口照样能用，只是**任何登录用户都能访问**。没有任何
    /// 运行时报错，而只读账号能看到全站营收。
    ///
    /// 逐条给新接口写角色测试当然更好，但那依赖「记得写」；这条测试不依赖记性：
    /// 路径前缀写对了就自动被覆盖。
    #[test]
    fn every_admin_route_lives_under_the_admin_guard() {
        let src = include_str!("http.rs");
        let start = src
            .find("pub fn create_router")
            .expect("必须有 create_router");
        // 只看到 `#[cfg(test)]` 为止，免得把本测试模块里的字符串算进去。
        let end = src[start..]
            .find("#[cfg(test)]")
            .map(|i| start + i)
            .unwrap_or(src.len());
        let body = &src[start..end];

        const PREFIX: &str = ".route(\"/portal/api/admin/";
        let total = body.matches(PREFIX).count();
        assert!(
            total > 0,
            "一条 admin 路由都没找到——要么前缀变了，要么这条测试已失效"
        );

        // 定位 admin 子树：从它的 `.merge(` 到 `require_admin` 那一行。
        let guard = body
            .find("from_fn(require_admin)")
            .expect("admin 子树必须挂 require_admin");
        let merge = body[..guard]
            .rfind(".merge(")
            .expect("require_admin 必须挂在一个 merge 进来的子 Router 上");
        let guarded = &body[merge..guard];

        let inside = guarded.matches(PREFIX).count();
        assert_eq!(
            inside,
            total,
            "有 {} 条 /portal/api/admin/ 路由不在 require_admin 子树里——\n\
             它们对任何登录用户开放。请把 .route(…) 移到那个 .merge(Router::new()…) 内部。",
            total - inside
        );
    }

    // ============ 审计导出（CSV） ============

    /// **红线：CSV 里绝不出现 `ksk_` 明文。**
    ///
    /// 【为何要测一个「本来就不该有」的东西】审计的 `detail` 目前只存计数、价格、
    /// 角色变迁和管理员备注，确实没有明文。但它是**自由文本字段**，日后有人为了
    /// 排障往里塞一句「key=ksk_…」是完全可能的，而那一刻这个导出接口就变成了
    /// 一键把全部明文下载下来。这条断言让那种改动在 CI 就停下。
    #[test]
    fn audit_csv_never_contains_plaintext_key() {
        let rows = vec![
            super::super::store::AuditEntry {
                id: 1,
                at_ms: 1_700_000_000_000,
                username: Some("alice".to_string()),
                action: "reveal_keys".to_string(),
                client_ip: Some("1.2.3.4".to_string()),
                detail: Some("count=3".to_string()),
            },
            super::super::store::AuditEntry {
                id: 2,
                at_ms: 1_700_000_001_000,
                username: Some("bob".to_string()),
                action: "board_ok".to_string(),
                client_ip: None,
                detail: Some("cred=9001 price=10".to_string()),
            },
        ];
        let csv = audit_to_csv(&rows, rows.len() as i64, false);

        // 对照组：CSV 必须真的有内容，否则下面的断言在空串上恒真。
        assert!(
            csv.contains("alice") && csv.contains("board_ok"),
            "CSV 是空的: {csv}"
        );
        assert!(!csv.contains("ksk_"), "导出的 CSV 含明文 key 前缀: {csv}");
    }

    /// **公式注入必须被中和。**
    ///
    /// 用户名是用户自己起的，`detail` 里有管理员自填备注。以 `=`/`+`/`-`/`@` 开头的
    /// 单元格会被 Excel 当公式执行——这是**存储型**攻击：注册一个叫
    /// `=cmd|'/c calc'!A0` 的账号，等运营导出后双击打开即可。
    #[test]
    fn audit_csv_neutralizes_formula_injection() {
        let nasty = "=cmd|'/c calc'!A0";
        let rows = vec![super::super::store::AuditEntry {
            id: 1,
            at_ms: 1_700_000_000_000,
            username: Some(nasty.to_string()),
            action: "login_ok".to_string(),
            client_ip: None,
            detail: Some("@SUM(1+1)".to_string()),
        }];
        let csv = audit_to_csv(&rows, rows.len() as i64, false);

        // 原文仍在（审计必须如实记录），但前面多了强制文本的单引号。
        assert!(csv.contains(nasty), "原始内容被改掉了，审计失真: {csv}");
        assert!(
            csv.contains(&format!("'{nasty}")),
            "以 = 开头的字段没有被加单引号前缀，Excel 会当公式执行: {csv}"
        );
        assert!(
            csv.contains("'@SUM(1+1)"),
            "以 @ 开头的 detail 没被中和: {csv}"
        );

        // 对照组：普通字段**不该**被加前缀，否则每个值都多一个引号。
        assert!(
            !csv.contains("'login_ok"),
            "普通字段被误加了单引号前缀: {csv}"
        );
    }

    /// **真正能走通注册的那个注入向量：以连字符开头的用户名。**
    ///
    /// # 为何单独一条
    /// 上面那条用 `=cmd|'/c calc'!A0` 作用户名，而 `validate_username` 只允许
    /// 字母/数字/下划线/连字符/点——`=` 和 `@` 进不了库，那条用例测的是
    /// 「万一别处写进来」的兜底。
    ///
    /// 但**连字符是合法用户名字符**，而它同时是 Excel 的公式起始字符。实测在
    /// 8991 上 `-2-2`、`-1e2`、`-minus-lead` 全部注册成功。于是：注册一个叫
    /// `-2-2` 的号 → 登录失败几次（无需任何权限）→ 管理员导出审计 → Excel 把
    /// 那一格算成 `-4`。这不是"万一"，是一条**任何匿名访客都能走通**的路径，
    /// 且用户名列比 detail 列更危险：detail 恒以 `amount=`/`cred=` 这类前缀开头，
    /// 公式字符落在中间（Excel 不解析），而用户名是整格的第一个字符。
    ///
    /// 【为何不改成禁止连字符开头】那会让「修 CSV 的问题」去动注册校验规则——
    /// 已注册的老用户可能正好叫这个名字，改规则会让他们登不进来。防护该放在
    /// 导出这一层（问题所在的那一层），而不是回头收紧一个无关的入口。
    #[test]
    fn audit_csv_neutralizes_the_hyphen_username_that_registration_accepts() {
        // 这三个名字都通过 validate_username（字母/数字/下划线/连字符/点）。
        for name in ["-2-2", "-1e2", "-minus-lead"] {
            assert!(
                super::super::password::validate_username(name).is_ok(),
                "对照组失败：{name:?} 本该是合法用户名，否则这条用例测的不是真实向量"
            );

            let rows = vec![super::super::store::AuditEntry {
                id: 1,
                at_ms: 1_700_000_000_000,
                username: Some(name.to_string()),
                action: "login_fail_bad_password".to_string(),
                client_ip: Some("1.2.3.4".to_string()),
                detail: None,
            }];
            let csv = audit_to_csv(&rows, rows.len() as i64, false);

            assert!(
                csv.contains(&format!("'{name}")),
                "以连字符开头的用户名 {name:?} 没被中和——Excel 会把它当公式算: {csv}"
            );
            // 原文必须还在：审计失真比 Excel 算错更严重。
            assert!(csv.contains(name), "用户名原文被改掉了，审计失真: {csv}");
        }
    }

    /// 逗号、引号、换行必须按 CSV 规范转义，否则一行会被拆成多列/多行。
    #[test]
    fn audit_csv_escapes_separators_and_quotes() {
        let rows = vec![super::super::store::AuditEntry {
            id: 1,
            at_ms: 1_700_000_000_000,
            username: Some("a,b".to_string()),
            action: "x".to_string(),
            client_ip: None,
            detail: Some("he said \"hi\"\nnext line".to_string()),
        }];
        let csv = audit_to_csv(&rows, rows.len() as i64, false);

        assert!(csv.contains("\"a,b\""), "含逗号的字段没加引号: {csv}");
        assert!(
            csv.contains("\"he said \"\"hi\"\"\nnext line\""),
            "引号没翻倍/含换行的字段没加引号: {csv}"
        );

        // 数据行数：BOM+表头一行 + 一条记录（记录内部那个换行在引号里，不算换行）。
        // 【为何要数】转义写错时症状正是「一条记录变成两行」，而肉眼看 CSV 看不出。
        let logical_rows = csv.matches('\n').count();
        assert_eq!(
            logical_rows, 3,
            "换行数不对（表头1 + 记录内嵌1 + 记录尾1 = 3）: {csv:?}"
        );
    }

    /// **导出被截断时必须说出来**，而且要在 Excel 里看得见。
    ///
    /// # 为何这条如此重要
    /// 保留上限与导出上限都是 5000，看着刚好对齐——但裁剪每 6 小时才跑一次，
    /// 两次之间行数可以远超 5000（撞库风暴一晚上就够）。此时导出静默给 5000 行，
    /// 而运营拿它去核对账，会把「这就是全部」当成事实。
    ///
    /// 服务端本来就知道被截断了（`has_more`），只是先前把这个信息丢掉了。
    /// 这条测试锁住三个出口：CSV 正文尾注（双击打开时唯一可见的地方）、
    /// 响应头（脚本用）、文件名后缀（存档后仍能分辨）。
    #[tokio::test]
    async fn audit_export_says_so_when_truncated() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(true);
        use tower::ServiceExt;

        let (state, token) = state_with_user(super::super::role::RoleKind::Admin);
        let db = state.auth.db();
        for i in 0..6 {
            db.add_audit(1000 + i, None, Some("alice"), "login_ok", None, None)
                .unwrap();
        }

        // 用 limit 制造截断，而不是真塞 5001 行：截断的判据是
        // 「库里还有满足条件的行没带走」，limit=2 与超上限走的是同一条 has_more 分支。
        let mut req = Request::builder()
            .method("GET")
            .uri("/portal/api/admin/audit.csv?limit=2")
            .header(header::COOKIE, format!("{COOKIE_NAME}={token}"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                40001,
            ))));
        let res = create_router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let hdr = |name: &str| -> String {
            res.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        let truncated_hdr = hdr("x-audit-truncated");
        let total_hdr = hdr("x-audit-total");
        let cd = hdr("content-disposition");
        let body = String::from_utf8_lossy(
            &axum::body::to_bytes(res.into_body(), 256 * 1024)
                .await
                .unwrap(),
        )
        .to_string();

        assert_eq!(truncated_hdr, "true", "截断了却没置 x-audit-truncated 头");
        assert!(
            total_hdr.parse::<i64>().unwrap() >= 6,
            "x-audit-total 应是筛选后的总数而非本次导出的行数: {total_hdr}"
        );
        assert!(
            cd.contains("-truncated.csv"),
            "文件名缺 -truncated 后缀，存档后无法分辨这份是否完整: {cd}"
        );
        // 正文尾注是「双击打开」时唯一能看到的信号。
        assert!(
            body.contains("本次导出被截断"),
            "CSV 正文里没有截断说明——在 Excel 里打开时用户看不到任何提示: {body}"
        );
        // 尾注必须是注释行（`#` 起头），不能被当成第 N+1 条审计记录解析。
        assert!(
            body.lines().last().is_some_and(|l| l.starts_with('#')),
            "尾注不是以 # 开头的注释行，导入工具会把它当成一条数据: {body}"
        );
        assert!(
            body.contains("共 6 条") || body.contains("共 7 条"),
            "尾注没说清库里到底有多少条: {body}"
        );

        // 对照组：不截断时**不该**出现这些信号，否则每次导出都在喊「不完整」。
        let (st, full) = get_as(state, "/portal/api/admin/audit.csv", Some(&token)).await;
        assert_eq!(st, StatusCode::OK);
        assert!(
            !full.contains("# 注意"),
            "没截断却加了截断尾注（会污染每一次正常导出）: {full}"
        );
    }

    /// **导出审计这件事本身必须被审计。**
    ///
    /// # 为何
    /// 明文外显会写 `reveal_keys`。而把整张审计表（含每个用户的完整来源 IP）
    /// 导成文件带走，先前一条记录都不写——审计系统没有覆盖对自身的访问，
    /// 事后无法回答「谁把日志拿走了」。
    #[tokio::test]
    async fn audit_export_is_itself_audited() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(true);

        let (state, token) = state_with_user(super::super::role::RoleKind::Admin);
        let db = state.auth.db();

        let before = db
            .audit_page(&super::super::store::AuditQuery {
                action: Some("admin_audit_export".to_string()),
                limit: 10,
                ..Default::default()
            })
            .unwrap()
            .total;

        let (st, _) = get_as(
            state.clone(),
            "/portal/api/admin/audit.csv?username=nobody-matches-this",
            Some(&token),
        )
        .await;
        assert_eq!(st, StatusCode::OK);

        let page = db
            .audit_page(&super::super::store::AuditQuery {
                action: Some("admin_audit_export".to_string()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            page.total,
            before + 1,
            "导出没有留下 admin_audit_export 审计记录"
        );

        let row = &page.rows[0];
        assert_eq!(row.username.as_deref(), Some("boarder"), "没记下是谁导的");
        let detail = row.detail.clone().unwrap_or_default();
        // 筛选条件必须记进去：只记「有人导出了」而不记导了哪批，
        // 事后仍然无法判断泄露范围。
        assert!(
            detail.contains("username=nobody-matches-this"),
            "detail 里没有本次导出的筛选条件，无法判断带走了哪批数据: {detail}"
        );
        assert!(
            detail.contains("exported=") && detail.contains("total="),
            "detail 里没有条数，无法判断带走了多少: {detail}"
        );
    }

    /// UTF-8 BOM 必须在最前面，否则 Excel 简中版把中文显示成乱码。
    #[test]
    fn audit_csv_starts_with_bom_for_excel() {
        let csv = audit_to_csv(&[], 0, false);
        assert!(csv.starts_with('\u{feff}'), "CSV 开头缺 UTF-8 BOM");
        // 空结果也要有表头，否则用户下到一个 0 字节文件会以为导出坏了。
        assert!(
            csv.contains("id,time,username,action"),
            "空导出缺表头: {csv}"
        );
    }

    /// 导出端点的响应头必须让浏览器**下载**而不是在标签页里显示。
    #[tokio::test]
    async fn audit_export_sets_download_headers() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(true);
        use tower::ServiceExt;

        let (state, token) = state_with_user(super::super::role::RoleKind::Admin);
        let mut req = Request::builder()
            .method("GET")
            .uri("/portal/api/admin/audit.csv")
            .header(header::COOKIE, format!("{COOKIE_NAME}={token}"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                40001,
            ))));

        let res = create_router(state).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let ct = res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let cd = res
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(ct.contains("text/csv"), "Content-Type 不是 csv: {ct}");
        assert!(
            ct.contains("charset=utf-8"),
            "Content-Type 缺 charset，浏览器可能按本地编码解: {ct}"
        );
        assert!(
            cd.contains("attachment") && cd.contains(".csv"),
            "缺 attachment 附件头，CSV 会直接显示在标签页里: {cd}"
        );
    }

    /// **导出必须应用与列表相同的筛选条件。**
    ///
    /// # 为何这条是变异逼出来的
    /// 上面那条只查响应头。把导出 handler 里的 `p.to_query(...)` 换成
    /// `AuditParams::default().to_query(...)`（等于「无视全部筛选条件，导出所有」）
    /// 之后，七条审计相关测试**全绿**——因为没有任何一条检查过 CSV 的**内容**
    /// 是否随筛选变化。
    ///
    /// 而这正是我在 `admin_audit_export` 的文档里写下的那个失败模式：运营在界面上
    /// 筛出 3 条、点导出、拿到一个 812 条的文件，然后拿它去核对——核对的对象根本
    /// 不是他刚看的那批数据。写下了理由却没写测试，等于只防住了「我记得」这一层。
    #[tokio::test]
    async fn audit_export_applies_the_same_filters_as_the_list() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(true);

        let (state, token) = state_with_user(super::super::role::RoleKind::Admin);
        let db = state.auth.db();
        db.add_audit(1000, None, Some("alice"), "login_ok", None, None)
            .unwrap();
        db.add_audit(2000, None, Some("bob"), "login_ok", None, None)
            .unwrap();

        // 不筛：两个人都在（对照组——证明数据确实进得去，下面的"消失"才有意义）。
        let (st, all) = get_as(state.clone(), "/portal/api/admin/audit.csv", Some(&token)).await;
        assert_eq!(st, StatusCode::OK);
        assert!(
            all.contains("alice") && all.contains("bob"),
            "不筛的导出应包含两人: {all}"
        );

        // 按用户名筛：bob 必须从 CSV 里消失。
        let (_, only_alice) = get_as(
            state.clone(),
            "/portal/api/admin/audit.csv?username=alice",
            Some(&token),
        )
        .await;
        assert!(
            only_alice.contains("alice"),
            "筛 alice 的导出里没有 alice: {only_alice}"
        );
        assert!(
            !only_alice.contains("bob"),
            "筛了 alice 但导出里仍有 bob——导出忽略了筛选条件。\n\
             后果：运营在界面上看 3 条、导出拿到全表，再用那个文件去核对，\n\
             对的却不是他刚看的数据。CSV: {only_alice}"
        );

        // 时间窗同样要生效（筛选维度不止用户名，只测一个维度会漏掉其余的）。
        let (_, early) = get_as(
            state.clone(),
            "/portal/api/admin/audit.csv?untilMs=1500",
            Some(&token),
        )
        .await;
        assert!(
            early.contains("alice") && !early.contains("bob"),
            "时间窗没作用到导出上: {early}"
        );
    }

    /// **筛选与分页必须真的生效**（而不是无论传什么都返回全部）。
    ///
    /// 【为何在 HTTP 层再测一遍】store 层已有 17 条筛选测试，但那些证明的是 SQL 对；
    /// 这条证明的是**查询串真的被解析并传下去了**。`AuditParams` 少一个
    /// `rename_all = "camelCase"` 就会让 `actionPrefix` 静默变成 None——筛选框
    /// 看起来在用，实际返回全部数据，而 store 层的测试全绿。
    #[tokio::test]
    async fn audit_endpoint_applies_filters_from_query_string() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(true);

        let (state, token) = state_with_user(super::super::role::RoleKind::Admin);
        let db = state.auth.db();
        for (at, name, action) in [
            (1000_i64, "alice", "login_ok"),
            (2000, "alice", "login_fail_bad_password"),
            (3000, "bob", "login_ok"),
        ] {
            db.add_audit(at, None, Some(name), action, Some("9.9.9.9"), None)
                .unwrap();
        }

        // 不筛：三条都在（外加注册用户时自己写的审计，故用 >=）。
        let (st, all) = get_as(state.clone(), "/portal/api/admin/audit", Some(&token)).await;
        assert_eq!(st, StatusCode::OK);
        assert!(
            all.contains("alice") && all.contains("bob"),
            "不筛时应有全部: {all}"
        );

        // 按用户名筛：bob 必须消失。**两个方向都断言**——只断言 alice 在的话，
        // 一个「筛选完全没生效」的实现也能通过。
        let (_, only_alice) = get_as(
            state.clone(),
            "/portal/api/admin/audit?username=alice",
            Some(&token),
        )
        .await;
        assert!(
            only_alice.contains("alice"),
            "筛 alice 却没有 alice: {only_alice}"
        );
        assert!(
            !only_alice.contains("bob"),
            "筛 alice 时 bob 仍在——查询串没被解析: {only_alice}"
        );

        // 前缀筛（驼峰参数名）：只剩失败那条。
        let (_, fails) = get_as(
            state.clone(),
            "/portal/api/admin/audit?actionPrefix=login_fail",
            Some(&token),
        )
        .await;
        assert!(
            fails.contains("login_fail_bad_password"),
            "前缀筛没筛出失败记录（actionPrefix 可能没被 serde 认出）: {fails}"
        );
        assert!(
            !fails.contains("\"login_ok\""),
            "前缀 login_fail 把 login_ok 也捞进来了: {fails}"
        );

        // 分页：limit=1 只能有一条 row。
        let (_, one) = get_as(
            state.clone(),
            "/portal/api/admin/audit?limit=1",
            Some(&token),
        )
        .await;
        assert_eq!(
            one.matches("\"action\"").count(),
            1,
            "limit=1 却返回了多条: {one}"
        );
        assert!(one.contains("\"hasMore\":true"), "还有后续却说没有: {one}");
    }

    /// limit 必须被钳住：一个 `limit=999999` 不该让服务端把整表拼进内存。
    #[tokio::test]
    async fn audit_endpoint_clamps_limit() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(true);

        let (state, token) = state_with_user(super::super::role::RoleKind::Admin);
        let (_, body) = get_as(
            state.clone(),
            "/portal/api/admin/audit?limit=999999",
            Some(&token),
        )
        .await;
        assert!(
            body.contains(&format!(
                "\"limit\":{}",
                super::super::store::AUDIT_PAGE_MAX
            )),
            "limit 没被钳到 {}: {body}",
            super::super::store::AUDIT_PAGE_MAX
        );

        // limit=0 会让页面显示一片空白而不报错，必须被抬到 1。
        let (_, zero) = get_as(state, "/portal/api/admin/audit?limit=0", Some(&token)).await;
        assert!(zero.contains("\"limit\":1"), "limit=0 没被抬到 1: {zero}");
    }

    // ============ 缓存指令 ============

    /// 发一次请求，只取响应头里的 `cache-control`（没有则 None）。
    async fn cache_control_of(
        state: PortalState,
        method: &str,
        uri: &str,
        token: Option<&str>,
    ) -> Option<String> {
        use tower::ServiceExt;

        let mut b = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            b = b.header(header::COOKIE, format!("{COOKIE_NAME}={t}"));
        }
        let mut req = b.body(Body::empty()).unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                40002,
            ))));

        let res = create_router(state).oneshot(req).await.unwrap();
        res.headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    /// **每一条 `/portal/api/` 路由的响应都必须带 `no-store`。**
    ///
    /// 这是本组用例里最重要的一条。`/portal/api/keys` 在积分关闭或用户已上车时
    /// 会下发**明文凭据**；没有缓存指令的响应，中间缓存（nginx / CDN / 公司代理）
    /// 是**允许**存下来的，那就是跨用户串号。
    ///
    /// 【为何从源码里抓路由表，而不是手写一张清单】手写清单的失效模式是确定的：
    /// 日后新增端点必然漏，且漏掉不会有编译错误、不会有用例变红，表现是新端点
    /// 悄悄变成可缓存的。从 `create_router` 源码里抓，新增路由自动纳入覆盖。
    #[test]
    fn every_portal_api_route_is_declared_no_store() {
        let src = include_str!("http.rs");
        let start = src
            .find("pub fn create_router")
            .expect("必须有 create_router");
        let body = &src[start..];
        let end = body.find("\n#[cfg(test)]").unwrap_or(body.len());
        let body = &body[..end];

        let mut routes: Vec<&str> = Vec::new();
        for seg in body.split(".route(\"").skip(1) {
            if let Some(path) = seg.split('"').next() {
                if path.starts_with("/portal/api/") {
                    routes.push(path);
                }
            }
        }

        // 对照组：抓不到路由的话下面的循环一次都不跑，用例会假绿。
        assert!(
            routes.len() >= 8,
            "只抓到 {} 条 /portal/api 路由，抓取逻辑可能失效了：{routes:?}",
            routes.len()
        );

        // 所有 API 路由都必须落在 no_store 那一层的作用域里。这里用结构性断言：
        // no_store 只挂了一处（在 public.merge(private) 之后），而页面树是在那之后
        // 才 merge 进来的。若有人把某条 API 路由挪进 `pages`，它就会变成 no-cache。
        let pages_start = body.find("let pages").expect("必须有 pages 子树");
        let pages_end = body[pages_start..]
            .find("let public")
            .map(|i| pages_start + i)
            .expect("pages 之后必须有 public");
        let pages_seg = &body[pages_start..pages_end];
        for r in &routes {
            assert!(
                !pages_seg.contains(r),
                "{r} 被挪进了 pages 子树。那棵树用的是 no-cache（可存、须验证），\
                 而 API 响应必须 no-store——`/portal/api/keys` 会下发明文凭据，\
                 no-cache 意味着明文已经落盘了。"
            );
        }

        assert!(
            body.contains("http_cache::no_store"),
            "create_router 里没有挂 no_store 中间件"
        );
    }

    /// 明文那条路由的实测：真发一次请求，头必须是 `no-store`。
    ///
    /// 上面那条是结构性断言（读源码），这条是行为断言（发真请求）。两条都要：
    /// 结构断言能覆盖到全部路由但证明不了中间件真的生效；行为断言证明生效但
    /// 只覆盖被点到的那几条。
    #[tokio::test]
    async fn keys_endpoint_forbids_all_caching() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        let (state, token) = state_with_user(super::super::role::RoleKind::User);

        let cc = cache_control_of(state, "GET", "/portal/api/keys", Some(&token))
            .await
            .expect("/portal/api/keys 必须带 cache-control");

        assert!(
            cc.contains("no-store"),
            "明文凭据接口的 cache-control 是 {cc:?}，缺 no-store。\
             没有 no-store 时中间缓存可以留存这份响应，等于跨用户串号。"
        );
        // private 是给「只认一部分指令」的缓存留的后手，见 http_cache 模块文档。
        assert!(cc.contains("private"), "缺 private: {cc}");
        set_enabled(false);
    }

    /// **被中间件拒绝的响应也要带 no-store。**
    ///
    /// 401/403 的响应体不含明文，但同样不该被缓存：缓存住一个 401 的表现是
    /// 用户登录成功后仍然看到「未登录」，而且清不掉。这条锁住 no_store 挂在
    /// 比 require_session 更靠外的位置。
    #[tokio::test]
    async fn rejected_requests_are_also_no_store() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        let (state, _) = state_with_user(super::super::role::RoleKind::User);

        // 不带 cookie → require_session 判 401。
        let cc = cache_control_of(state.clone(), "GET", "/portal/api/keys", None)
            .await
            .expect("401 响应也必须带 cache-control");
        assert!(
            cc.contains("no-store"),
            "401 响应的 cache-control 是 {cc:?}。缓存住一个 401 会让用户登录后仍看到未登录。"
        );

        // 角色不足 → require_admin 判 403，同样要有。
        let (ro_state, ro_token) = state_with_user(super::super::role::RoleKind::Readonly);
        let cc403 = cache_control_of(ro_state, "GET", "/portal/api/admin/audit", Some(&ro_token))
            .await
            .expect("403 响应也必须带 cache-control");
        assert!(cc403.contains("no-store"), "403 响应缺 no-store: {cc403}");
        set_enabled(false);
    }

    /// 页面用 `no-cache`（可存但须回源验证），**不是** `no-store`。
    ///
    /// 页面本身是一张静态单页，不含任何用户数据（登录态全靠 JS 打 API 拿），
    /// 所以不必禁止存储；但必须回源验证，否则发新版后浏览器会拿着旧 HTML 打新
    /// API——那种错法的表现是「页面看着正常，功能莫名其妙坏掉」。
    ///
    /// 带不带尾斜杠两种写法都测：它们是两条独立注册的路由，漏挂一条不会报错。
    #[tokio::test]
    async fn the_page_is_revalidated_not_forbidden_from_caching() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        let (state, _) = state_with_user(super::super::role::RoleKind::User);

        for uri in ["/portal", "/portal/"] {
            let cc = cache_control_of(state.clone(), "GET", uri, None)
                .await
                .unwrap_or_else(|| panic!("{uri} 必须带 cache-control"));
            assert!(cc.contains("no-cache"), "{uri} 缺 no-cache: {cc}");
            assert!(
                !cc.contains("no-store"),
                "{uri} 用了 no-store，这是过度收紧：页面不含用户数据，\
                 no-store 会让每次导航都整份重下，白白放弃 304。得到 {cc}"
            );
        }
        set_enabled(false);
    }

    /// 缓存中间件**不得**削弱 feature_gate：未启用时仍然是 404。
    ///
    /// 这条防的是一类装配事故：把缓存层挂在 feature_gate 外面、且用了 `layer`
    /// 而非 `route_layer`，就会给全站未匹配路径也打上头，等于向扫描者确认
    /// 「这里有个 portal」。
    #[tokio::test]
    async fn cache_layers_do_not_leak_when_portal_is_disabled() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(false);
        let (state, _) = state_with_user(super::super::role::RoleKind::User);

        use tower::ServiceExt;
        for uri in ["/portal", "/portal/api/keys"] {
            let mut req = Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            req.extensions_mut()
                .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    40003,
                ))));
            let res = create_router(state.clone()).oneshot(req).await.unwrap();
            assert_eq!(
                res.status(),
                StatusCode::NOT_FOUND,
                "portal 未启用时 {uri} 必须 404，实得 {}",
                res.status()
            );
        }
    }

    // ============ 车票的 region 必须等于网关实际用的那个 ============

    /// 用一把指定凭据造 state，config 用默认值（region = us-east-1）。
    fn state_with_cred(
        c: crate::kiro::model::credentials::KiroCredentials,
    ) -> (PortalState, String) {
        let db = Arc::new(super::super::store::PortalDb::open_in_memory().unwrap());
        let auth = Arc::new(PortalAuth::new(db.clone()));
        crate::common::auth_keys::set_portal_invite_code("region-code").unwrap();
        let ok = auth
            .register(
                "regionuser",
                "correct horse battery",
                "region-code",
                None,
                now_ms(),
            )
            .expect("注册失败");
        let mgr = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![c],
                None,
                None,
                false,
            )
            .expect("建凭据池失败"),
        );
        (PortalState::new(auth, mgr), ok.token)
    }

    /// 从 `/api/keys` 响应里取第一条的 `region`。
    fn first_region(body: &str) -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(body).expect("响应不是 JSON");
        v["items"][0]["region"].as_str().map(|s| s.to_string())
    }

    /// **region 只填在 profileArn 里时，车票也必须显示它。**
    ///
    /// 这是改动前真正会坑到用户的一档：旧写法只看 region/api_region/auth_region
    /// 三个字段，而 Enterprise/IdC 号的 region 藏在 profileArn 第 4 段里。三字段
    /// 皆空 → 旧代码给 None → 页面显示不出 region，用户按全局默认（us-east-1）
    /// 去配 client，而网关其实在打 ap-northeast-1，请求一路失败且错误里看不出原因。
    #[tokio::test]
    async fn ticket_region_comes_from_profile_arn_when_fields_empty() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(false); // 积分关 → 明文直接下发，专测 region

        let mut c = crate::kiro::model::credentials::KiroCredentials::default();
        c.id = Some(9101);
        c.kiro_api_key = Some(K.to_string());
        c.auth_method = Some("api_key".to_string());
        c.profile_arn =
            Some("arn:aws:codewhisperer:ap-northeast-1:123456789012:profile/ABCDEFGH".to_string());
        // region 三字段刻意全空——这正是旧写法失效的条件。
        c.region = None;
        c.auth_region = None;
        c.api_region = None;

        let (state, token) = state_with_cred(c);
        let (status, body) = get_as(state, "/portal/api/keys", Some(&token)).await;
        assert_eq!(status, StatusCode::OK, "取车票列表失败: {body}");

        assert_eq!(
            first_region(&body).as_deref(),
            Some("ap-northeast-1"),
            "车票的 region 必须来自 profileArn（网关就是这么解析的）。\
             实得 {:?}——用户会照着错的 region 配 client。\n响应: {body}",
            first_region(&body)
        );
        set_enabled(false);
    }

    /// **污染的 region 字段不得原样显示给用户，要回退到 config。**
    ///
    /// 凭据的 region 来自不可信来源（推送 API / 手工编辑）。旧写法把它直接
    /// clone 出来发给前端，于是页面上会出现一个网关根本不会用的值——网关那边
    /// 有白名单，不命中就回退 config.region。两边不一致时页面就是在撒谎。
    #[tokio::test]
    async fn ticket_region_rejects_polluted_value_and_falls_back() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(false);

        let mut c = crate::kiro::model::credentials::KiroCredentials::default();
        c.id = Some(9102);
        c.kiro_api_key = Some(K.to_string());
        c.auth_method = Some("api_key".to_string());
        c.region = Some("evil.com/".to_string()); // 白名单外
        c.auth_region = None;
        c.api_region = None;

        let (state, token) = state_with_cred(c);
        let (status, body) = get_as(state, "/portal/api/keys", Some(&token)).await;
        assert_eq!(status, StatusCode::OK, "取车票列表失败: {body}");

        let got = first_region(&body);
        assert_eq!(
            got.as_deref(),
            Some("us-east-1"),
            "污染 region 必须回退到 config（us-east-1），实得 {got:?}"
        );
        assert!(!body.contains("evil.com"), "污染值被原样发给了前端: {body}");
        set_enabled(false);
    }

    /// **对照组：三字段有值时照常显示，且优先级与网关一致。**
    ///
    /// 没有这一条，上面两条用一个「永远返回 config.region」的实现也能全绿——
    /// 那时所有号都显示 us-east-1，等于这一列没用。
    #[tokio::test]
    async fn ticket_region_uses_credential_field_when_present() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(false);

        let mut c = crate::kiro::model::credentials::KiroCredentials::default();
        c.id = Some(9103);
        c.kiro_api_key = Some(K.to_string());
        c.auth_method = Some("api_key".to_string());
        c.region = Some("eu-central-1".to_string()); // 生产 #252 就是这个形态
        c.auth_region = Some("eu-central-1".to_string());
        c.api_region = Some("eu-central-1".to_string());

        let (state, token) = state_with_cred(c);
        let (status, body) = get_as(state, "/portal/api/keys", Some(&token)).await;
        assert_eq!(status, StatusCode::OK, "取车票列表失败: {body}");
        assert_eq!(
            first_region(&body).as_deref(),
            Some("eu-central-1"),
            "凭据自带 region 时必须用它，不能被 config 覆盖。响应: {body}"
        );
        set_enabled(false);
    }

    /// **前端必须真的渲染 region。**
    ///
    /// 后端下发了、前端不画，用户依然拿不到——而这正是改动前的状态：字段在
    /// `CredentialRow` 里躺了很久，页面上一个字都没有。扫源码而非起浏览器：
    /// 这条不变量的本质是「fillTicket 里要用到 it.region」。
    #[test]
    fn page_must_render_region_next_to_ticket() {
        let page = include_str!("page.rs");
        let start = page
            .find("function fillTicket")
            .expect("page.rs 里必须有 fillTicket");
        // 截到下一个顶层 function 为止，确保断言落在 fillTicket 体内。
        let rest = &page[start + 10..];
        let end = rest
            .find("\nfunction ")
            .map(|i| start + 10 + i)
            .unwrap_or(page.len());
        let body = &page[start..end];

        assert!(
            body.contains("it.region"),
            "fillTicket 里没有用到 it.region——车票旁边不会显示 region，\n\
             用户只拿到 key 配不出能用的 client。"
        );
    }

    // ============ 车费规则外显（/api/keys 的 pricing 字段） ============

    /// **车费规则必须随列表下发，且价格表由服务端算。**
    ///
    /// 【为何断言整张表而不只断言几个字段】前端画表时若自己复算公式（两段式 +
    /// ceil + min 钳制），就会存在两份实现：页面上写着 2 分、实际扣 3 分，
    /// 而用户只相信自己看到的那个。把表钉在服务端响应里，前端只能照抄。
    #[tokio::test]
    async fn keys_response_carries_server_computed_pricing() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(true);
        // 用线上那套参数：min=3 / max=15。
        set_pricing(crate::portal::credits::Pricing {
            base_count: 2,
            base_price: 10,
            total_price: 20,
            min_price: 3,
            max_unlockers: 15,
        });

        let (state, token) = state_with_user(super::super::role::RoleKind::User);
        let (status, body) = get_as(state, "/portal/api/keys", Some(&token)).await;
        assert_eq!(status, StatusCode::OK, "取列表失败: {body}");

        let v: serde_json::Value = serde_json::from_str(&body).expect("响应不是 JSON");
        let p = &v["pricing"];
        assert!(
            !p.is_null(),
            "积分启用时 /api/keys 必须带 pricing——页面没有它就只能把规则写死在前端，\
             而写死的文案会在管理员改配置后变成谎言。\n响应: {body}"
        );

        assert_eq!(p["baseCount"], 2, "baseCount 不符: {body}");
        assert_eq!(p["basePrice"], 10, "basePrice 不符: {body}");
        assert_eq!(p["minPrice"], 3, "minPrice 不符: {body}");
        assert_eq!(p["maxBoarders"], 15, "maxBoarders 不符: {body}");

        // 与线上实测一致的那张表（副本容器 16 人上车验证过）。
        let table: Vec<i64> = p["priceTable"]
            .as_array()
            .expect("priceTable 必须是数组")
            .iter()
            .map(|x| x.as_i64().expect("表里必须是整数"))
            .collect();
        assert_eq!(
            table,
            vec![10, 10, 7, 5, 4, 4, 3, 3, 3, 3, 3, 3, 3, 3, 3],
            "价格表与实收口径不一致——这张表就是用户照着算钱的依据"
        );
        assert_eq!(
            table.len(),
            15,
            "表长必须等于 maxBoarders，否则页面画不出最后几个位子"
        );

        set_credits_enabled(false);
        set_pricing(crate::portal::credits::Pricing::default());
        set_enabled(false);
    }

    /// **积分未启用时 `pricing` 整个字段消失。**
    ///
    /// 【为何这一条值得单测】那种部署根本不存在车费，下发一份不生效的规则
    /// 会让用户照着它算一遍，然后发现扣的分跟写的不一样——比不显示更糟。
    /// 用 `skip_serializing_if` 实现，很容易在日后重构时被顺手删掉。
    #[tokio::test]
    async fn pricing_is_absent_when_credits_disabled() {
        let _g = crate::common::auth_keys::test_serial();
        set_enabled(true);
        set_credits_enabled(false);

        let (state, token) = state_with_user(super::super::role::RoleKind::User);
        let (status, body) = get_as(state, "/portal/api/keys", Some(&token)).await;
        assert_eq!(status, StatusCode::OK, "取列表失败: {body}");

        let v: serde_json::Value = serde_json::from_str(&body).expect("响应不是 JSON");
        assert!(
            v.get("pricing").is_none() || v["pricing"].is_null(),
            "积分未启用时不该下发 pricing，实得: {}",
            v["pricing"]
        );

        set_enabled(false);
    }

    /// **页面必须渲染服务端下发的规则，且不得自己复算公式。**
    ///
    /// 断言方式是扫源码：这条不变量的本质是「renderRules 只读 pricing 字段」。
    /// 一旦有人在前端补一句 `Math.ceil(total/n)`，页面与实收就可能分叉。
    #[test]
    fn page_renders_rules_from_server_without_recomputing() {
        let page = include_str!("page.rs");
        let start = page
            .find("function renderRules")
            .expect("page.rs 里必须有 renderRules——否则规则根本没显示在页面上");
        let rest = &page[start + 10..];
        let end = rest
            .find("\nfunction ")
            .map(|i| start + 10 + i)
            .unwrap_or(page.len());
        let body = &page[start..end];

        for field in [
            "baseCount",
            "basePrice",
            "minPrice",
            "maxBoarders",
            "priceTable",
        ] {
            assert!(
                body.contains(field),
                "renderRules 没用到 p.{field}——规则显示不全，\
                 用户还是得靠猜。"
            );
        }

        // 红线：前端不得复算价格公式。
        assert!(
            !body.contains("Math.ceil"),
            "renderRules 里出现了 Math.ceil——前端在自己算价。\n\
             公式有两份实现时，页面显示的价和真正扣的价就会分叉，\n\
             而用户只相信自己看到的那个。价格表必须整张来自服务端。"
        );

        // 规则块要挂进 render()，否则函数写了也不会被调用。
        assert!(
            page.contains("renderRules(LAST_PRICING"),
            "render() 里没有调用 renderRules——规则块永远不会出现"
        );
    }
}
