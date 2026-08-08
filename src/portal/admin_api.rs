//! Portal 的 admin 侧管理接口（挂在 `/api/admin/portal/*`，走 admin 鉴权）。
//!
//! # 职责边界
//! 这里是**管理员**视角：看有哪些 portal 用户、建号、停用、重置密码、查审计。
//! 与 [`super::http`]（portal 用户自己的登录/查看入口）严格分开：
//! - 这一层要 `adminApiKey`，那一层要 portal 用户的会话 cookie；
//! - 这一层**不返回任何明文凭据**，管理员要看明文有现成的凭据管理页。
//!
//! # 为什么管理员也能建号
//! 注册码是自助通道，但内部场景常有「帮同事开个号」的需求，且注册码泄露后
//! 需要一个不依赖它的口子。两条路并存，都写审计。
//!
//! # 一条红线
//! **绝不返回 `password_hash`**。它是离线爆破的原料，泄露等于把所有用户的密码
//! 交给攻击者去慢慢算。响应结构里干脆没有这个字段，而不是靠调用方记得别填。

use std::sync::Arc;

use axum::{
    Json,
    body::Body,
    extract::{DefaultBodyLimit, Extension, FromRef, Path, Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use super::admin_auth::{COOKIE_NAME as ADMIN_COOKIE_NAME, LoginError, PortalAdminAuth};
use super::password;
use super::store::PortalDb;

/// 拼车管理子树的共享状态。业务 handler 仍只提取 `Arc<PortalDb>`，认证层提取完整状态。
#[derive(Clone)]
pub struct AdminPortalState {
    pub db: Option<Arc<PortalDb>>,
    pub auth: Arc<PortalAdminAuth>,
}

impl FromRef<AdminPortalState> for Arc<PortalDb> {
    fn from_ref(state: &AdminPortalState) -> Self {
        state
            .db
            .clone()
            .expect("正常拼车管理业务路由必须注入 PortalDb")
    }
}

impl FromRef<AdminPortalState> for Arc<PortalAdminAuth> {
    fn from_ref(state: &AdminPortalState) -> Self {
        state.auth.clone()
    }
}

#[derive(Debug, Clone)]
struct AdminRequestContext {
    ip: Option<String>,
    https: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdminPasswordRequest {
    password: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChangeAdminPasswordRequest {
    current_password: String,
    new_password: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdminAuthStatusResponse {
    configured: bool,
    authenticated: bool,
    secure_transport: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_in_secs: Option<u64>,
}

/// admin 侧看到的一个 portal 用户。**没有 password_hash 字段**（见模块级红线）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminUserRow {
    pub id: i64,
    pub username: String,
    pub disabled: bool,
    pub created_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_login_ms: Option<i64>,
    /// 当前积分余额（无余额记录时为 0）。
    pub balance: i64,
    /// 已上车的车队数。管理员判断「这个号在用吗」比看 last_login 更直接。
    pub aboard_count: i64,
    /// 角色（`"admin"` / `"user"` / `"readonly"`）。
    ///
    /// 【为何列表里就要给】改某人的角色之前得先看清现在谁是什么。若只在详情接口
    /// 返回，运营者得逐个点开才知道有几个管理员——而那恰恰是最该一眼看全的信息
    /// （尤其在「能不能降级这个人」取决于还剩几个 admin 的时候）。
    pub role: super::role::RoleKind,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortalStatusResponse {
    /// 总开关当前状态（运行时镜像，非磁盘配置）。
    pub enabled: bool,
    /// 注册码是否已配置。**只报有无，不回显值**——回显等于让任何拿到 admin 只读
    /// 权限的人直接获得注册能力。
    pub invite_code_configured: bool,
    pub require_https: bool,
    pub user_count: usize,
    pub key_count: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateUserRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetPasswordRequest {
    pub password: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetDisabledRequest {
    pub disabled: bool,
}

/// 设置角色请求。
///
/// # 为何字段是 `String` 而不是直接反序列化成 `RoleKind`
/// 直接反序列化成枚举时，非法值会在 serde 层被拒，返回的是「无法解析请求体」这类
/// 笼统信息——调用方看不出是哪个字段错了、合法值有哪些。收成字符串后由
/// [`super::role::RoleKind::parse_strict`] 校验，能给出「角色无效：xxx，合法值为
/// admin / user / readonly」这种可直接照着改的错误。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetRoleRequest {
    pub role: String,
}

/// 充值 / 扣减请求。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopupRequest {
    /// 正数充值，负数扣减。0 被拒（无意义操作，且会在流水里留下噪音行）。
    pub amount: i64,
    /// 备注，写进流水。管理员事后要能回答「这 500 分是为什么给的」。
    pub note: Option<String>,
}

/// 单个用户的钱包详情。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminWalletResponse {
    #[serde(flatten)]
    pub wallet: super::store::Wallet,
    pub ledger: Vec<super::store::LedgerEntry>,
}

/// 当前车费规则（面板展示用）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingResponse {
    pub enabled: bool,
    pub base_count: u32,
    pub base_price: i64,
    pub total_price: i64,
    pub min_price: i64,
    pub max_boarders: u32,
    /// 1..=max 每个人数下的单价，直接给前端画表。
    ///
    /// 服务端算而非前端复算：那个公式（两段式 + ceil + min 钳制）一旦有两份实现，
    /// 面板显示的价和真正扣的价就可能不一致，而用户只会相信自己看到的那个。
    pub price_table: Vec<i64>,
}

#[derive(Debug, Serialize)]
struct ErrBody {
    error: String,
}

fn bad_request(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, Json(ErrBody { error: msg.into() })).into_response()
}

fn internal(msg: impl Into<String>) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrBody { error: msg.into() }),
    )
        .into_response()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// GET /api/admin/portal/status
pub async fn status(State(db): State<Arc<PortalDb>>) -> Response {
    let user_count = db.list_users().map(|v| v.len()).unwrap_or(0);
    let key_count = db.count_import_keys().unwrap_or(0);
    Json(PortalStatusResponse {
        enabled: super::http::enabled(),
        invite_code_configured: crate::common::auth_keys::portal_invite_configured(),
        require_https: super::http::require_https_public(),
        user_count,
        key_count,
    })
    .into_response()
}

/// GET /api/admin/portal/users
pub async fn list_users(State(db): State<Arc<PortalDb>>) -> Response {
    // 用 `list_users_with_balance` 而非 `list_users`：后者返回的 `PortalUser`
    // 带 `password_hash` 字段，虽然这里不会去填它，但让哈希进到这个函数的作用域
    // 就是给「日后有人图省事直接 `Json(users)`」留了口子。取一个结构上就没有
    // 哈希的类型，那种失误连编译都过不了。
    match db.list_users_with_balance() {
        Ok(users) => Json(
            users
                .into_iter()
                .map(|u| AdminUserRow {
                    id: u.id,
                    username: u.username,
                    disabled: u.disabled,
                    created_at_ms: u.created_at_ms,
                    last_login_ms: u.last_login_ms,
                    balance: u.balance,
                    aboard_count: u.aboard_count,
                    role: u.role,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => {
            tracing::error!("Portal 列出用户失败: {:#}", e);
            internal("读取用户列表失败")
        }
    }
}

/// POST /api/admin/portal/users
///
/// 管理员建号。走与自助注册**同一套**强度校验：绕过校验建出的弱密码账号，
/// 在公网暴露下与自助注册的弱密码账号是同等风险，没有理由放宽。
pub async fn create_user(
    State(db): State<Arc<PortalDb>>,
    Json(req): Json<CreateUserRequest>,
) -> Response {
    let username = req.username.trim();
    if let Err(e) = password::validate_username(username) {
        return bad_request(e.to_string());
    }
    if let Err(e) = password::validate_password_strength(&req.password) {
        return bad_request(e.to_string());
    }
    let phc = match password::hash_password(&req.password) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("Portal 密码哈希失败: {:#}", e);
            return internal("创建失败");
        }
    };
    let now = now_ms();
    match db.create_user(username, &phc, now) {
        Ok(Some(id)) => {
            let _ = db.add_audit(
                now,
                Some(id),
                Some(username),
                "admin_create_user",
                None,
                None,
            );
            tracing::info!("管理员创建 Portal 用户 #{} {}", id, username);
            Json(serde_json::json!({"ok": true, "id": id})).into_response()
        }
        Ok(None) => bad_request("该用户名已被占用"),
        Err(e) => {
            tracing::error!("Portal 创建用户失败: {:#}", e);
            internal("创建失败")
        }
    }
}

/// POST /api/admin/portal/users/{id}/password
///
/// 重置密码。[`PortalDb::set_password`] 会在同一事务里清掉该用户所有会话——
/// 管理员重置密码的场景通常正是「怀疑这个号被盗了」，旧会话必须一起失效。
pub async fn reset_password(
    State(db): State<Arc<PortalDb>>,
    Path(id): Path<i64>,
    Json(req): Json<ResetPasswordRequest>,
) -> Response {
    if let Err(e) = password::validate_password_strength(&req.password) {
        return bad_request(e.to_string());
    }
    let phc = match password::hash_password(&req.password) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("Portal 密码哈希失败: {:#}", e);
            return internal("重置失败");
        }
    };
    match db.set_password(id, &phc) {
        Ok(true) => {
            let now = now_ms();
            let _ = db.add_audit(now, Some(id), None, "admin_reset_password", None, None);
            tracing::info!("管理员重置 Portal 用户 #{} 的密码（已清空其会话）", id);
            Json(serde_json::json!({"ok": true})).into_response()
        }
        Ok(false) => bad_request("用户不存在"),
        Err(e) => {
            tracing::error!("Portal 重置密码失败: {:#}", e);
            internal("重置失败")
        }
    }
}

/// POST /api/admin/portal/users/{id}/disabled
pub async fn set_disabled(
    State(db): State<Arc<PortalDb>>,
    Path(id): Path<i64>,
    Json(req): Json<SetDisabledRequest>,
) -> Response {
    match db.set_disabled(id, req.disabled) {
        Ok(true) => {
            let now = now_ms();
            let action = if req.disabled {
                "admin_disable_user"
            } else {
                "admin_enable_user"
            };
            let _ = db.add_audit(now, Some(id), None, action, None, None);
            tracing::info!(
                "管理员{} Portal 用户 #{}",
                if req.disabled { "停用" } else { "启用" },
                id
            );
            Json(serde_json::json!({"ok": true})).into_response()
        }
        Ok(false) => bad_request("用户不存在"),
        Err(e) => {
            tracing::error!("Portal 设置停用状态失败: {:#}", e);
            internal("操作失败")
        }
    }
}

/// POST /api/admin/portal/users/{id}/role
///
/// 授予/收回角色。**这是 portal admin 权限的唯一来源**——`adminApiKey` 的持有者
/// （部署者）用它把运营权授予某个 portal 账号，此后那个账号能管用户与余额，
/// 但拿不到 `adminApiKey` 能做的事（改配置、增删凭据、看全部日志）。
///
/// # 为何不做「首个管理员」的特殊机制
/// 不需要。`adminApiKey` 本身就是最高权限，用它调这个端点即可造出第一个 admin。
/// 额外的引导机制（配置项指定用户名、首个注册者自动升级）都会引入一条
/// **绕过 adminApiKey 的提权路径**，而那正是不该存在的东西。
///
/// # 为何拦「降级最后一个管理员」
/// 允许的话，portal 内会再没有人能管用户，只能回去动 `adminApiKey`——而那正是
/// 引入 portal admin 想避免的。这里在入口就拒绝，而不是等自锁发生后才发现。
/// 注意判定用的是**未禁用**的 admin 数（见 [`PortalDb::admin_count`]）：
/// 若把禁用的 admin 也算进去，就会出现「数字上有 2 个管理员、实际没人能登录」。
pub async fn set_role(
    State(db): State<Arc<PortalDb>>,
    Path(id): Path<i64>,
    Json(req): Json<SetRoleRequest>,
) -> Response {
    // 严格解析：写入路径放过一个错别字，它就永久留在库里，此后每次读都要靠
    // 宽容解析兜底——而兜出来的未必是管理员当时想设的那一档。
    let next = match super::role::RoleKind::parse_strict(&req.role) {
        Some(r) => r,
        None => {
            return bad_request(format!(
                "角色无效：{:?}。合法值为 admin / user / readonly",
                req.role
            ));
        }
    };

    // 先读现状：既要判断是否在降级最后一个管理员，也要在角色没变时直接返回。
    let current = match db.role_of(id) {
        Ok(Some(r)) => r,
        Ok(None) => return bad_request("用户不存在"),
        Err(e) => {
            tracing::error!("Portal 读取用户角色失败: {:#}", e);
            return internal("操作失败");
        }
    };

    if current == next {
        // 幂等：重复设同一档不是错误，也不写审计（否则审计会被无意义的重复操作灌满）。
        return Json(serde_json::json!({"ok": true, "role": next.as_str(), "changed": false}))
            .into_response();
    }

    if current == super::role::RoleKind::Admin {
        match db.admin_count() {
            Ok(n) if n <= 1 => {
                return bad_request(
                    "这是最后一个启用中的运营管理员，不能降级。请先授予另一个账号 admin 角色。",
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::error!("Portal 统计管理员数量失败: {:#}", e);
                // 数不出来就不放行：这里放行的后果是可能造成自锁，而自锁只能靠
                // 改库或 adminApiKey 解开。宁可让这次操作失败。
                return internal("操作失败");
            }
        }
    }

    match db.set_role(id, next) {
        Ok(true) => {
            let now = now_ms();
            // 审计里记「从哪一档到哪一档」。只记新值的话，事后无法回答
            // 「这个人是被降级了还是本来就是 user」——而那正是排查越权时要问的。
            let _ = db.add_audit(
                now,
                Some(id),
                None,
                "admin_set_role",
                None,
                Some(&format!("{} -> {}", current.as_str(), next.as_str())),
            );
            tracing::info!(
                "管理员把 Portal 用户 #{} 的角色从 {} 改为 {}",
                id,
                current.as_str(),
                next.as_str()
            );
            Json(serde_json::json!({"ok": true, "role": next.as_str(), "changed": true}))
                .into_response()
        }
        Ok(false) => bad_request("用户不存在"),
        Err(e) => {
            tracing::error!("Portal 设置角色失败: {:#}", e);
            internal("操作失败")
        }
    }
}

/// DELETE /api/admin/portal/users/{id}
///
/// 删号。会话随外键 CASCADE 一起走；**审计故意保留**（不加外键），
/// 否则「查一下这个被删掉的号干过什么」就永远查不到了。
pub async fn delete_user(State(db): State<Arc<PortalDb>>, Path(id): Path<i64>) -> Response {
    match db.delete_user(id) {
        Ok(true) => {
            let now = now_ms();
            let _ = db.add_audit(now, Some(id), None, "admin_delete_user", None, None);
            tracing::info!("管理员删除 Portal 用户 #{}", id);
            Json(serde_json::json!({"ok": true})).into_response()
        }
        Ok(false) => bad_request("用户不存在"),
        Err(e) => {
            tracing::error!("Portal 删除用户失败: {:#}", e);
            internal("删除失败")
        }
    }
}

/// POST /api/admin/portal/users/{id}/topup
///
/// 手动充值或扣减。这是积分进入系统的**唯一**入口——没有自动发放、没有签到，
/// 所以每一分都对应一次管理员的显式操作，对账时不存在「这分哪来的」这种问题。
///
/// 扣减（`amount < 0`）在余额不足时返回 400 而非静默截断到 0：管理员想扣 50
/// 却只扣掉 30，却看到「成功」，是比失败更糟的结果。
pub async fn topup(
    State(db): State<Arc<PortalDb>>,
    Path(id): Path<i64>,
    Json(req): Json<TopupRequest>,
) -> Response {
    if req.amount == 0 {
        return bad_request("金额不能为 0");
    }

    // 先确认用户存在。
    //
    // 不查也不会写坏数据——`portal_balances.user_id` 有外键，给不存在的 id 充值
    // 会被 SQLite 拒绝（实测报 `FOREIGN KEY constraint failed`，code 787）。
    // 但那句话会经 `internal()` 变成 500「调整积分失败」，管理员看到的是服务器
    // 出错，而真相是「这个 id 没有对应用户」。查一次换来一句 400 人话，
    // 也把「数据没写坏」从依赖外键的巧合变成本函数的显式保证。
    match db.user_exists(id) {
        Ok(true) => {}
        Ok(false) => return bad_request("用户不存在"),
        Err(e) => {
            tracing::error!("Portal 查用户失败: {:#}", e);
            return internal("查询用户失败");
        }
    }

    let note = req.note.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let now = now_ms();

    match db.adjust_balance(
        id,
        req.amount,
        super::store::ADMIN_ADJUST_KIND,
        None,
        note,
        now,
    ) {
        Ok(Some(balance)) => {
            let _ = db.add_audit(
                now,
                Some(id),
                None,
                "admin_topup",
                None,
                Some(&format!(
                    "amount={} balance={} note={}",
                    req.amount,
                    balance,
                    note.unwrap_or("-")
                )),
            );
            tracing::info!(
                "管理员调整 Portal 用户 #{} 积分 {:+}，余额 {}",
                id,
                req.amount,
                balance
            );
            Json(serde_json::json!({"ok": true, "balance": balance})).into_response()
        }
        // adjust_balance 用 Ok(None) 表示「余额不足，什么都没做」——这是正常
        // 业务结果而非错误，所以在这里翻译成 400 + 具体数字。
        Ok(None) => {
            let cur = db.balance_of(id).unwrap_or(0);
            bad_request(format!(
                "余额不足：当前 {cur} 分，扣不动 {} 分",
                -req.amount
            ))
        }
        Err(e) => {
            tracing::error!("Portal 调整余额失败: {:#}", e);
            internal("调整积分失败")
        }
    }
}

/// GET /api/admin/portal/users/{id}/wallet
pub async fn user_wallet(State(db): State<Arc<PortalDb>>, Path(id): Path<i64>) -> Response {
    let wallet = match db.wallet_of(id) {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("Portal 读钱包失败: {:#}", e);
            return internal("读取钱包失败");
        }
    };
    // 流水读失败不致命：余额是权威数字，明细只是辅助。为一份明细读不到就让
    // 整个钱包查不了，是把观测故障升级成主功能故障。
    let ledger = db.ledger_of(id, LEDGER_LIMIT).unwrap_or_default();
    Json(AdminWalletResponse { wallet, ledger }).into_response()
}

/// GET /api/admin/portal/pricing
///
/// 报的是**运行时镜像**而非磁盘配置：热更后两者可能不一致，而管理员想知道的
/// 永远是「现在实际按什么收费」。
pub async fn pricing(State(_db): State<Arc<PortalDb>>) -> Response {
    let p = super::http::pricing();
    let max = p.max_unlockers;
    // 1..=max 每个人数下的单价。用同一个 unit_price 算，面板与实收必然一致。
    let price_table: Vec<i64> = (1..=max).map(|n| p.unit_price(n)).collect();

    Json(PricingResponse {
        enabled: super::http::credits_enabled(),
        base_count: p.base_count,
        base_price: p.base_price,
        total_price: p.total_price,
        min_price: p.min_price,
        max_boarders: max,
        price_table,
    })
    .into_response()
}

/// 单个用户钱包一次返回多少条流水。
const LEDGER_LIMIT: usize = 200;

/// 审计查询的条数上限。
const AUDIT_LIMIT: usize = 200;

/// GET /api/admin/portal/audit
pub async fn audit(State(db): State<Arc<PortalDb>>) -> Response {
    match db.recent_audit(AUDIT_LIMIT) {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => {
            tracing::error!("Portal 读取审计失败: {:#}", e);
            internal("读取审计失败")
        }
    }
}

/// 从 Cookie 头取拼车管理会话。令牌只进入栈上临时字符串，不写日志。
fn admin_session_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(name, value)| (name == ADMIN_COOKIE_NAME).then(|| value.to_string()))
}

fn build_admin_cookie(token: &str, max_age_secs: u64, https: bool) -> String {
    let secure = if https { "; Secure" } else { "" };
    format!(
        "{ADMIN_COOKIE_NAME}={token}; HttpOnly; SameSite=Strict; Path=/api/admin/portal; Max-Age={max_age_secs}{secure}"
    )
}

fn clear_admin_cookie(https: bool) -> String {
    build_admin_cookie("", 0, https)
}

fn auth_error(error: LoginError) -> Response {
    let (status, message, retry_after) = match error {
        LoginError::NotConfigured => (
            StatusCode::PRECONDITION_REQUIRED,
            "尚未初始化拼车管理密码",
            None,
        ),
        LoginError::AlreadyConfigured => (
            StatusCode::CONFLICT,
            "拼车管理密码已初始化，不能重复设置",
            None,
        ),
        LoginError::InvalidPassword => (
            StatusCode::BAD_REQUEST,
            "管理密码至少 16 位，并包含大写字母、小写字母、数字、符号中的至少三类",
            None,
        ),
        LoginError::BadPassword => (StatusCode::UNAUTHORIZED, "密码错误或会话已失效", None),
        LoginError::Throttled { retry_after_secs } => (
            StatusCode::TOO_MANY_REQUESTS,
            "尝试过于频繁，请稍后再试",
            Some(retry_after_secs),
        ),
        LoginError::InsecureTransport => (
            StatusCode::UPGRADE_REQUIRED,
            "远程访问拼车管理必须使用 HTTPS",
            None,
        ),
        LoginError::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "认证服务内部错误", None),
    };
    let mut response = (
        status,
        Json(ErrBody {
            error: message.into(),
        }),
    )
        .into_response();
    if let Some(seconds) = retry_after {
        if let Ok(value) = seconds.to_string().parse() {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
    }
    response
}

async fn admin_auth_status(
    State(state): State<AdminPortalState>,
    Extension(context): Extension<AdminRequestContext>,
    headers: HeaderMap,
) -> Response {
    let expires = admin_session_cookie(&headers)
        .as_deref()
        .and_then(|token| state.auth.validate_and_touch(token));
    Json(AdminAuthStatusResponse {
        configured: state.auth.configured(),
        authenticated: expires.is_some(),
        secure_transport: context.https,
        expires_in_secs: expires,
    })
    .into_response()
}

async fn admin_auth_setup(
    State(state): State<AdminPortalState>,
    Extension(context): Extension<AdminRequestContext>,
    Json(req): Json<AdminPasswordRequest>,
) -> Response {
    match state.auth.setup(req.password, context.ip.as_deref()).await {
        Ok(ok) => {
            let mut response = Json(serde_json::json!({"ok": true})).into_response();
            if let Ok(cookie) =
                build_admin_cookie(&ok.token, ok.expires_in_secs, context.https).parse()
            {
                response.headers_mut().insert(header::SET_COOKIE, cookie);
            }
            response
        }
        Err(error) => auth_error(error),
    }
}

async fn admin_auth_login(
    State(state): State<AdminPortalState>,
    Extension(context): Extension<AdminRequestContext>,
    Json(req): Json<AdminPasswordRequest>,
) -> Response {
    match state.auth.login(req.password, context.ip.as_deref()).await {
        Ok(ok) => {
            let mut response = Json(serde_json::json!({"ok": true})).into_response();
            if let Ok(cookie) =
                build_admin_cookie(&ok.token, ok.expires_in_secs, context.https).parse()
            {
                response.headers_mut().insert(header::SET_COOKIE, cookie);
            }
            response
        }
        Err(error) => auth_error(error),
    }
}

async fn admin_auth_logout(
    State(state): State<AdminPortalState>,
    Extension(context): Extension<AdminRequestContext>,
    headers: HeaderMap,
) -> Response {
    if let Some(token) = admin_session_cookie(&headers) {
        state.auth.logout(&token);
    }
    let mut response = Json(serde_json::json!({"ok": true})).into_response();
    if let Ok(cookie) = clear_admin_cookie(context.https).parse() {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    response
}

async fn admin_auth_change_password(
    State(state): State<AdminPortalState>,
    Extension(context): Extension<AdminRequestContext>,
    headers: HeaderMap,
    Json(req): Json<ChangeAdminPasswordRequest>,
) -> Response {
    let Some(token) = admin_session_cookie(&headers) else {
        return auth_error(LoginError::BadPassword);
    };
    match state
        .auth
        .change_password(
            &token,
            req.current_password,
            req.new_password,
            context.ip.as_deref(),
        )
        .await
    {
        Ok(ok) => {
            let mut response = Json(serde_json::json!({"ok": true})).into_response();
            if let Ok(cookie) =
                build_admin_cookie(&ok.token, ok.expires_in_secs, context.https).parse()
            {
                response.headers_mut().insert(header::SET_COOKIE, cookie);
            }
            response
        }
        Err(error) => auth_error(error),
    }
}

/// 为所有拼车管理请求计算可信客户端 IP / HTTPS 状态，并对写请求做 CSRF 防护。
async fn admin_request_context(
    State(state): State<AdminPortalState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|value| value.0);
    let ip =
        crate::common::security::client_ip(&request, peer, state.auth.trust_forwarded_header())
            .map(|value| value.to_string());

    let trusted_forwarder = state.auth.trust_forwarded_header()
        || peer
            .map(|value| crate::common::security::is_trusted_proxy_peer(value.ip()))
            .unwrap_or(false);
    let forwarded_https = trusted_forwarder
        && request
            .headers()
            .get("x-forwarded-proto")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next_back())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("https"));
    let peer_loopback = peer.is_some_and(|value| value.ip().is_loopback());
    let local_host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(':')
                .next()
                .unwrap_or(value)
                .trim_matches(['[', ']'])
        })
        .is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
        });
    let https = forwarded_https;
    let transport_allowed = https || peer_loopback || (local_host && trusted_forwarder);

    if request.method() != Method::GET {
        if request
            .headers()
            .get("x-portal-admin-csrf")
            .and_then(|value| value.to_str().ok())
            != Some("1")
        {
            return (
                StatusCode::FORBIDDEN,
                Json(ErrBody {
                    error: "缺少 CSRF 防护头".into(),
                }),
            )
                .into_response();
        }
        if !transport_allowed {
            return auth_error(LoginError::InsecureTransport);
        }
    }

    request
        .extensions_mut()
        .insert(AdminRequestContext { ip, https });
    next.run(request).await
}

/// 业务接口二次认证。仅持有 adminApiKey 仍会在这里被拒绝。
async fn require_portal_admin_session(
    State(state): State<AdminPortalState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !state.auth.configured() {
        return auth_error(LoginError::NotConfigured);
    }
    let Some(token) = admin_session_cookie(request.headers()) else {
        return auth_error(LoginError::BadPassword);
    };
    if state.auth.validate_and_touch(&token).is_none() {
        return auth_error(LoginError::BadPassword);
    }
    next.run(request).await
}

/// 构造 `/api/admin/portal/*` 子路由。认证接口和业务接口是两棵独立子树；业务子树
/// 整体挂二次会话中间件，避免新增接口时漏锁。
pub fn create_router(db: Arc<PortalDb>, auth: Arc<PortalAdminAuth>) -> axum::Router {
    use axum::routing::{delete, get, post};

    let state = AdminPortalState { db: Some(db), auth };
    let protected = axum::Router::new()
        .route("/status", get(status))
        .route("/users", get(list_users).post(create_user))
        .route("/users/{id}", delete(delete_user))
        .route("/users/{id}/password", post(reset_password))
        .route("/users/{id}/disabled", post(set_disabled))
        .route("/users/{id}/role", post(set_role))
        .route("/users/{id}/topup", post(topup))
        .route("/users/{id}/wallet", get(user_wallet))
        .route("/pricing", get(pricing))
        .route("/audit", get(audit))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_portal_admin_session,
        ));

    let auth_routes = axum::Router::new()
        .route("/auth/status", get(admin_auth_status))
        .route("/auth/setup", post(admin_auth_setup))
        .route("/auth/login", post(admin_auth_login))
        .route("/auth/logout", post(admin_auth_logout))
        .route("/auth/change-password", post(admin_auth_change_password))
        .route_layer(DefaultBodyLimit::max(4 * 1024));

    protected
        .merge(auth_routes)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            admin_request_context,
        ))
        .layer(axum::middleware::from_fn(admin_auth))
        .layer(axum::middleware::from_fn(
            crate::common::http_cache::no_store,
        ))
        .with_state(state)
}

/// 库不可用时仍保留认证接口；诊断详情只有通过两层认证后才能读取。
pub fn unavailable_router(reason: String, auth: Arc<PortalAdminAuth>) -> axum::Router {
    let state = AdminPortalState { db: None, auth };
    let reason = Arc::new(reason);
    // 不能在只有 fallback 的空 Router 上挂 layer/route_layer：axum 0.8 会把它
    // 判为“在任何路由之前加层”并直接 panic。显式注册根路径与 catch-all 后，
    // 二次会话层只包诊断业务子树；下面的 /auth/* 仍可用于登录与初始化。
    let diagnostic = {
        let reason = reason.clone();
        move || {
            let reason = reason.clone();
            async move {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(ErrBody {
                        error: format!("Portal 数据库不可用：{reason}"),
                    }),
                )
                    .into_response()
            }
        }
    };
    let protected = axum::Router::new()
        .route("/", axum::routing::any(diagnostic.clone()))
        .route("/{*path}", axum::routing::any(diagnostic))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_portal_admin_session,
        ));
    let auth_routes = axum::Router::new()
        .route("/auth/status", axum::routing::get(admin_auth_status))
        .route("/auth/setup", axum::routing::post(admin_auth_setup))
        .route("/auth/login", axum::routing::post(admin_auth_login))
        .route("/auth/logout", axum::routing::post(admin_auth_logout))
        .route(
            "/auth/change-password",
            axum::routing::post(admin_auth_change_password),
        )
        .route_layer(DefaultBodyLimit::max(4 * 1024));
    protected
        .merge(auth_routes)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            admin_request_context,
        ))
        .layer(axum::middleware::from_fn(admin_auth))
        .layer(axum::middleware::from_fn(
            crate::common::http_cache::no_store,
        ))
        .with_state(state)
}

/// adminApiKey 外层鉴权：与通用 admin 树同一把热更 key，空值恒拒绝。
async fn admin_auth(request: Request<Body>, next: Next) -> Response {
    let key = crate::common::auth::extract_api_key(&request);
    match key {
        Some(k) if crate::common::auth_keys::admin_key_matches(&k) => next.run(request).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(ErrBody {
                error: "认证失败".into(),
            }),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHC: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aaaa";

    fn db() -> Arc<PortalDb> {
        Arc::new(PortalDb::open_in_memory().unwrap())
    }

    /// **红线：admin 侧任何响应体都不得含 `password_hash`。**
    ///
    /// 断言的是**类型**而非某次调用的输出：只要 `AdminUserRow` 里没有这个字段，
    /// 就没有任何 handler 能填上它——比「记得别填」可靠，因为后者靠人。
    /// 这里序列化一个真实用户，确认哈希不出现在 JSON 里。
    #[test]
    fn admin_user_row_never_serializes_password_hash() {
        let db = db();
        db.create_user("alice", PHC, 1000).unwrap().unwrap();

        let rows = db.list_users_with_balance().unwrap();
        let json = serde_json::to_string(
            &rows
                .into_iter()
                .map(|u| AdminUserRow {
                    id: u.id,
                    username: u.username,
                    disabled: u.disabled,
                    created_at_ms: u.created_at_ms,
                    last_login_ms: u.last_login_ms,
                    balance: u.balance,
                    aboard_count: u.aboard_count,
                    role: u.role,
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();

        // 对照组：先确认序列化确实产出了内容，否则下面两条断言在空串上也会通过。
        assert!(json.contains("alice"), "对照组失败：用户名都没序列化出来");

        assert!(
            !json.contains("password"),
            "响应体出现了 password 字样: {json}"
        );
        assert!(!json.contains(PHC), "响应体泄露了密码哈希: {json}");
        assert!(
            !json.contains("argon2"),
            "响应体出现了哈希算法标识，说明 PHC 串漏出去了: {json}"
        );
    }

    /// 余额联表：没有 `portal_balances` 行的新用户余额是 0，不是查询失败。
    ///
    /// 【为何值得单测】用 INNER JOIN 写这条查询，新用户会**整行消失**——
    /// 管理员会以为号没建成功。COALESCE + LEFT JOIN 两者缺一都出错，
    /// 而错法是「少一行」，不报任何异常。
    #[test]
    fn new_user_shows_zero_balance_not_missing_row() {
        let db = db();
        db.create_user("alice", PHC, 1000).unwrap().unwrap();
        let uid = db.create_user("bob", PHC, 2000).unwrap().unwrap();
        db.adjust_balance(uid, 50, super::super::store::TOPUP_KIND, None, None, 3000)
            .unwrap()
            .unwrap();

        let rows = db.list_users_with_balance().unwrap();
        assert_eq!(rows.len(), 2, "从未充值的用户不能从列表里消失");

        let alice = rows.iter().find(|r| r.username == "alice").unwrap();
        assert_eq!(alice.balance, 0);
        assert_eq!(alice.aboard_count, 0);

        let bob = rows.iter().find(|r| r.username == "bob").unwrap();
        assert_eq!(bob.balance, 50);
    }

    /// 上车数随实际上车变化，且按用户隔离。
    #[test]
    fn aboard_count_reflects_actual_boarding() {
        let db = db();
        let a = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        let b = db.create_user("bob", PHC, 1000).unwrap().unwrap();
        for uid in [a, b] {
            db.adjust_balance(uid, 100, super::super::store::TOPUP_KIND, None, None, 1000)
                .unwrap()
                .unwrap();
        }

        let p = super::super::credits::Pricing::default();
        db.board(a, 7, p, 2000).unwrap();
        db.board(a, 8, p, 2100).unwrap();
        db.board(b, 7, p, 2200).unwrap();

        let rows = db.list_users_with_balance().unwrap();
        let alice = rows.iter().find(|r| r.username == "alice").unwrap();
        let bob = rows.iter().find(|r| r.username == "bob").unwrap();
        assert_eq!(alice.aboard_count, 2, "alice 上了两辆车");
        assert_eq!(bob.aboard_count, 1, "bob 只上了一辆，不该数到 alice 的");
    }

    /// `user_exists` 不把密码哈希读进内存。
    ///
    /// 存在性判断用不着哈希。用 `find_user_by_name` 代替会让 PHC 串进到
    /// 调用方作用域，那里一个 `{:?}` 或 error 上下文就可能把它写进日志。
    #[test]
    fn user_exists_answers_without_loading_hash() {
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        assert!(db.user_exists(uid).unwrap());
        assert!(
            !db.user_exists(uid + 999).unwrap(),
            "不存在的 id 返回 false 而非报错"
        );
    }

    /// 扣减到负余额被拒，且**什么都不改**。
    #[test]
    fn overdraw_is_refused_and_leaves_state_untouched() {
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        db.adjust_balance(uid, 30, super::super::store::TOPUP_KIND, None, None, 2000)
            .unwrap()
            .unwrap();

        let res = db
            .adjust_balance(
                uid,
                -50,
                super::super::store::ADMIN_ADJUST_KIND,
                None,
                Some("扣太多"),
                3000,
            )
            .unwrap();
        assert_eq!(res, None, "扣超余额返回 None（handler 据此回 400）");
        assert_eq!(db.balance_of(uid).unwrap(), 30, "失败的扣减不得改余额");
        assert_eq!(
            db.ledger_of(uid, 10).unwrap().len(),
            1,
            "失败的扣减不得留流水"
        );
    }

    /// 价格表与真实计价用同一个函数。
    ///
    /// 【为何这条重要】面板显示的价和实收的价若来自两份实现，用户只会相信
    /// 自己看到的那个，而系统按另一个扣分——这类投诉无法自证清白。
    #[test]
    fn price_table_matches_unit_price() {
        let p = super::super::credits::Pricing::default();
        let table: Vec<i64> = (1..=p.max_unlockers).map(|n| p.unit_price(n)).collect();

        assert_eq!(table.len(), 10, "默认 10 个座位");
        assert_eq!(table[0], 10, "第 1 人 base_price");
        assert_eq!(table[1], 10, "第 2 人 base_price");
        assert_eq!(table[2], 7, "第 3 人 ceil(20/3)");
        assert_eq!(table[9], 2, "第 10 人 ceil(20/10)");

        // 单调不增：多一个人绝不该让单价变贵。
        for w in table.windows(2) {
            assert!(w[1] <= w[0], "价格表出现上涨: {table:?}");
        }
    }

    // ============ router 级测试 ============
    //
    // 【为何必须打真实 HTTP 而不是直接调 handler 函数】
    // handler 里的校验（角色合法性、最后一个管理员保护）只在**请求真正流过
    // 中间件与提取器**时才会执行。直接调函数会绕过 `admin_auth`，也绕过
    // `Json<T>` 的反序列化——于是「非法角色被拒绝」这类断言测的是一条
    // 生产中不存在的代码路径。本轮的变异注入证实了这一点：去掉 set_role 里
    // 的自锁保护，所有单测依然全绿，因为没有任何测试能走到那一行。

    use tower::ServiceExt;

    const TEST_ADMIN_PASSWORD: &str = "Portal-Admin-Test-2026!";

    /// 构造已配置的二次认证域，并通过真实 login 路径签发会话 Cookie。
    async fn authenticated_admin() -> (Arc<PortalAdminAuth>, String) {
        let phc = password::hash_password(TEST_ADMIN_PASSWORD).expect("生成测试管理密码哈希");
        let auth = PortalAdminAuth::new(
            phc.into(),
            std::path::PathBuf::from("unused-test-config.json"),
            false,
        );
        let login = auth
            .login(TEST_ADMIN_PASSWORD.to_string(), Some("127.0.0.1"))
            .await
            .expect("测试管理密码登录失败");
        (auth, format!("{ADMIN_COOKIE_NAME}={}", login.token))
    }

    /// 只构造已配置认证域，不签发会话。用于验证外层 adminApiKey 会先拒绝请求。
    fn configured_admin() -> Arc<PortalAdminAuth> {
        let phc = password::hash_password(TEST_ADMIN_PASSWORD).expect("生成测试管理密码哈希");
        PortalAdminAuth::new(
            phc.into(),
            std::path::PathBuf::from("unused-test-config.json"),
            false,
        )
    }

    /// 发一个带 admin key 的请求，返回 (状态码, 响应体文本)。
    async fn call(
        db: Arc<PortalDb>,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, String) {
        let (auth, cookie) = authenticated_admin().await;
        let req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("x-api-key", "test-admin-key")
            .header("content-type", "application/json")
            .header(header::HOST, "localhost")
            .header(header::COOKIE, cookie)
            .header("x-portal-admin-csrf", "1")
            // 生产服务由 into_make_service_with_connect_info 注入；测试显式模拟本机连接。
            .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                43210,
            ))));
        let req = match body {
            Some(v) => req.body(axum::body::Body::from(v.to_string())).unwrap(),
            None => req.body(axum::body::Body::empty()).unwrap(),
        };
        let res = create_router(db, auth).oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// 播种 admin key，让 `admin_auth` 能放行。
    fn seed_admin_key() -> std::sync::MutexGuard<'static, ()> {
        let g = crate::common::auth_keys::test_serial();
        crate::common::auth_keys::set_admin_key("test-admin-key").unwrap();
        g
    }

    /// 合法角色都能设置成功，且真的落库。
    #[tokio::test]
    async fn set_role_endpoint_accepts_all_valid_roles() {
        let _g = seed_admin_key();
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        // 先造第二个 admin，免得后面把 alice 从 admin 降级时撞上「最后一个管理员」保护。
        db.create_user_with_role("keeper", PHC, super::super::role::RoleKind::Admin, 1000)
            .unwrap()
            .unwrap();

        for role in super::super::role::RoleKind::ALL {
            let (st, body) = call(
                db.clone(),
                "POST",
                &format!("/users/{uid}/role"),
                Some(serde_json::json!({"role": role.as_str()})),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "设 {role:?} 失败: {body}");
            assert_eq!(
                db.role_of(uid).unwrap(),
                Some(role),
                "设 {role:?} 返回 200 但没落库"
            );
        }
    }

    /// 非法角色必须 400，且**不得改动现有角色**。
    ///
    /// 后半句是重点：若 handler 先写库再校验，一个拼错的角色名会把用户
    /// 打成未知档，而未知档读回来是 user——静默降权。
    #[tokio::test]
    async fn set_role_endpoint_rejects_invalid_and_keeps_current() {
        let _g = seed_admin_key();
        let db = db();
        let uid = db
            .create_user_with_role("boss", PHC, super::super::role::RoleKind::Admin, 1000)
            .unwrap()
            .unwrap();

        for bad in ["administrator", "root", "", "owner", "ADMIN_", "user;--"] {
            let (st, body) = call(
                db.clone(),
                "POST",
                &format!("/users/{uid}/role"),
                Some(serde_json::json!({"role": bad})),
            )
            .await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "非法角色 {bad:?} 竟被接受");
            assert!(
                body.contains("admin / user / readonly"),
                "错误信息该给出合法值，实际: {body}"
            );
            assert_eq!(
                db.role_of(uid).unwrap(),
                Some(super::super::role::RoleKind::Admin),
                "非法请求 {bad:?} 改动了现有角色"
            );
        }
    }

    /// **不能降级最后一个启用中的管理员**——否则 portal 内再没人能管用户，
    /// 只能回去动 adminApiKey，而那正是引入 portal admin 想避免的。
    #[tokio::test]
    async fn set_role_endpoint_refuses_to_demote_last_admin() {
        let _g = seed_admin_key();
        let db = db();
        let only = db
            .create_user_with_role("solo", PHC, super::super::role::RoleKind::Admin, 1000)
            .unwrap()
            .unwrap();

        for target in ["user", "readonly"] {
            let (st, body) = call(
                db.clone(),
                "POST",
                &format!("/users/{only}/role"),
                Some(serde_json::json!({"role": target})),
            )
            .await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "降级最后一个管理员竟成功了");
            assert!(body.contains("最后一个"), "错误信息没解释原因: {body}");
            assert_eq!(
                db.role_of(only).unwrap(),
                Some(super::super::role::RoleKind::Admin),
                "被拒的请求仍然改了库"
            );
        }

        // 对照组：有第二个 admin 之后，降级就该放行——否则上面的拒绝
        // 可能只是「这个端点永远拒绝」，而不是自锁保护在起作用。
        db.create_user_with_role("backup", PHC, super::super::role::RoleKind::Admin, 1000)
            .unwrap()
            .unwrap();
        let (st, body) = call(
            db.clone(),
            "POST",
            &format!("/users/{only}/role"),
            Some(serde_json::json!({"role": "user"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "有备份管理员后仍拒绝降级: {body}");
        assert_eq!(
            db.role_of(only).unwrap(),
            Some(super::super::role::RoleKind::User)
        );
    }

    /// 已禁用的 admin 不算「启用中的管理员」。
    ///
    /// 若把禁用的 admin 也计入，会出现「数字上有 2 个管理员、实际没人能登录」
    /// 的自锁——而那正是这条保护要防的情形。
    #[tokio::test]
    async fn disabled_admin_does_not_count_as_backup() {
        let _g = seed_admin_key();
        let db = db();
        let active = db
            .create_user_with_role("active", PHC, super::super::role::RoleKind::Admin, 1000)
            .unwrap()
            .unwrap();
        let sleeping = db
            .create_user_with_role("sleeping", PHC, super::super::role::RoleKind::Admin, 1000)
            .unwrap()
            .unwrap();
        db.set_disabled(sleeping, true).unwrap();

        let (st, body) = call(
            db.clone(),
            "POST",
            &format!("/users/{active}/role"),
            Some(serde_json::json!({"role": "user"})),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "被禁用的 admin 被当成了有效备份: {body}"
        );
    }

    /// 重复设同一档是幂等的：返回 200 且 `changed: false`。
    #[tokio::test]
    async fn set_role_is_idempotent() {
        let _g = seed_admin_key();
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();

        let (st, body) = call(
            db.clone(),
            "POST",
            &format!("/users/{uid}/role"),
            Some(serde_json::json!({"role": "user"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert!(
            body.contains("\"changed\":false"),
            "重复设同一档该报 changed:false，实际: {body}"
        );
    }

    /// 不存在的用户返回 400，而不是静默成功。
    #[tokio::test]
    async fn set_role_on_missing_user_is_rejected() {
        let _g = seed_admin_key();
        let db = db();
        let (st, _) = call(
            db,
            "POST",
            "/users/99999/role",
            Some(serde_json::json!({"role": "admin"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    /// **没有 admin key 的请求一律 401**，包括改角色这种高危操作。
    ///
    /// 【为何要专门测】新增路由时容易挂在 `.layer(admin_auth)` 之后，
    /// 那样这条路由就完全不设防，而其余路由照常受保护——单看代码很难发现。
    #[tokio::test]
    async fn role_endpoint_requires_admin_key() {
        let _g = seed_admin_key();
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();

        let req = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/users/{uid}/role"))
            .header("content-type", "application/json")
            // 故意不带 x-api-key
            .body(axum::body::Body::from(
                serde_json::json!({"role": "admin"}).to_string(),
            ))
            .unwrap();
        let res = create_router(db.clone(), configured_admin())
            .oneshot(req)
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "改角色端点没有被 admin 鉴权保护"
        );
        assert_eq!(
            db.role_of(uid).unwrap(),
            Some(super::super::role::RoleKind::User),
            "未鉴权的请求改动了角色"
        );
    }

    /// 只有外层 adminApiKey 仍不能读取拼车业务数据；必须有独立会话。
    #[tokio::test]
    async fn admin_key_alone_cannot_access_portal_business_routes() {
        let _g = seed_admin_key();
        let db = db();
        let req = axum::http::Request::builder()
            .uri("/users")
            .header("x-api-key", "test-admin-key")
            .header(header::HOST, "localhost")
            .body(axum::body::Body::empty())
            .unwrap();
        let res = create_router(db, configured_admin())
            .oneshot(req)
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// Cookie 认证的写请求必须再带自定义 CSRF 头；SameSite 不是唯一防线。
    #[tokio::test]
    async fn authenticated_write_without_csrf_header_is_rejected() {
        let _g = seed_admin_key();
        let db = db();
        let (auth, cookie) = authenticated_admin().await;
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/users")
            .header("x-api-key", "test-admin-key")
            .header(header::HOST, "localhost")
            .header(header::COOKIE, cookie)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({"username":"alice","password":"Strong-User-2026!"}).to_string(),
            ))
            .unwrap();
        let res = create_router(db.clone(), auth).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(!db.user_exists(1).unwrap(), "CSRF 被拒后不得创建用户");
    }

    /// 未初始化时业务接口必须 fail-closed，不能因「还没设密码」临时裸奔。
    #[tokio::test]
    async fn unconfigured_second_factor_closes_business_routes() {
        let _g = seed_admin_key();
        let auth = PortalAdminAuth::new(
            None,
            std::path::PathBuf::from("unused-test-config.json"),
            false,
        );
        let req = axum::http::Request::builder()
            .uri("/status")
            .header("x-api-key", "test-admin-key")
            .header(header::HOST, "localhost")
            .body(axum::body::Body::empty())
            .unwrap();
        let res = create_router(db(), auth).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::PRECONDITION_REQUIRED);
    }

    /// 改角色必须写审计，且记下「从哪一档到哪一档」。
    ///
    /// 只记新值的话，事后无法回答「这个人是被降级了还是本来就是 user」——
    /// 而那正是排查越权时要问的第一个问题。
    #[tokio::test]
    async fn set_role_writes_audit_with_transition() {
        let _g = seed_admin_key();
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();

        let (st, _) = call(
            db.clone(),
            "POST",
            &format!("/users/{uid}/role"),
            Some(serde_json::json!({"role": "admin"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);

        let rows = db.recent_audit(50).unwrap();
        let entry = rows
            .iter()
            .find(|a| a.action == "admin_set_role")
            .expect("没有写 admin_set_role 审计");
        let detail = entry.detail.as_deref().unwrap_or("");
        assert!(
            detail.contains("user") && detail.contains("admin"),
            "审计详情没记录角色变迁: {detail:?}"
        );
    }
    /// **库不可用时，管理接口必须回 503 而不是 404。**
    ///
    /// 【为何这条值得单独钉住】404 的语义是「没这个功能」。生产实证 2026-08-07：
    /// portal 库因目录权限打不开，挂载点写在 `Ok` 分支里，于是整棵
    /// `/api/admin/portal/*` 变成 404——排查时第一反应是「前端没编进去 / 路由没挂」，
    /// 而真正的原因是一行 `Permission denied`。503 + 真实原因能把人直接带到现场。
    ///
    /// 变异验证：把 `unavailable_router` 的状态码改回 `NOT_FOUND`，本用例变红。
    #[tokio::test]
    async fn unavailable_router_answers_503_with_the_real_reason() {
        use tower::ServiceExt;

        let _g = crate::common::auth_keys::test_serial();
        crate::common::auth_keys::set_admin_key("k-unavail").unwrap();

        let reason = "创建数据目录 /app/data/usage 失败: Permission denied (os error 13)";
        let (auth, cookie) = authenticated_admin().await;
        let app = unavailable_router(reason.to_string(), auth);

        // 每条真实存在的管理路径都应给出同一个 503——用 fallback 实现，所以
        // 这里顺带证明「不管打哪条子路径都不会漏成 404」。
        for path in ["/status", "/users", "/audit", "/pricing", "/users/1/topup"] {
            let req = axum::http::Request::builder()
                .uri(path)
                .header("x-api-key", "k-unavail")
                .header(header::COOKIE, &cookie)
                .body(axum::body::Body::empty())
                .unwrap();
            let res = app.clone().oneshot(req).await.unwrap();

            assert_eq!(
                res.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "{path} 应回 503（库不可用），回 404 会把排查引向「功能不存在」"
            );

            let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
                .await
                .unwrap();
            let text = String::from_utf8_lossy(&body);
            assert!(
                text.contains("Permission denied"),
                "{path} 的响应体没带出真实原因，等于把唯一的线索丢了: {text}"
            );
        }
    }

    /// 库不可用的诊断信息**只给管理员**：无 key / 错 key 一律 401。
    ///
    /// 那段文本里有服务端绝对路径与 errno，属于不该对匿名者外泄的实现细节。
    /// 若哪天有人为了「方便排查」把 `admin_auth` 从 `unavailable_router` 上摘掉，
    /// 这条会红。
    #[tokio::test]
    async fn unavailable_router_still_requires_admin_auth() {
        use tower::ServiceExt;

        let _g = crate::common::auth_keys::test_serial();
        crate::common::auth_keys::set_admin_key("k-real").unwrap();

        let app = unavailable_router("内部路径不该外泄".to_string(), configured_admin());

        for key in [None, Some("k-wrong")] {
            let mut b = axum::http::Request::builder().uri("/status");
            if let Some(k) = key {
                b = b.header("x-api-key", k);
            }
            let res = app
                .clone()
                .oneshot(b.body(axum::body::Body::empty()).unwrap())
                .await
                .unwrap();

            assert_eq!(
                res.status(),
                StatusCode::UNAUTHORIZED,
                "key={key:?} 时必须 401：503 的详情含服务端路径，不能对匿名者外泄"
            );

            let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
                .await
                .unwrap();
            assert!(
                !String::from_utf8_lossy(&body).contains("不该外泄"),
                "401 响应里带出了诊断详情"
            );
        }
    }

    /// 诊断响应同样不得被任何中间缓存留存。
    #[tokio::test]
    async fn unavailable_router_is_also_no_store() {
        use tower::ServiceExt;

        let _g = crate::common::auth_keys::test_serial();
        crate::common::auth_keys::set_admin_key("k-cc").unwrap();

        let (auth, cookie) = authenticated_admin().await;
        let res = unavailable_router("x".to_string(), auth)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/status")
                    .header("x-api-key", "k-cc")
                    .header(header::COOKIE, cookie)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let cc = res
            .headers()
            .get(axum::http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(cc.contains("no-store"), "诊断响应缺少 no-store: {cc:?}");
    }
}
