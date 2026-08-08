//! Portal 认证服务：注册、登录、登出、会话校验。
//!
//! # 分层位置
//! 这层把 [`super::store`]（持久化）、[`super::password`]（哈希）、[`super::throttle`]（节流）
//! 和 [`crate::common::auth_keys`]（注册码闸门）串起来，向 HTTP 层暴露四个动作。
//! HTTP 层只负责取 cookie/填响应，不碰任何安全判定——判定全在这里，避免「换个 handler
//! 就漏掉一道检查」。
//!
//! # 对外错误一律模糊
//! 所有失败路径都返回 [`AuthError`]，其 `client_message()` 对「用户不存在」「密码错」
//! 「账号停用」三种情况给**完全相同**的文案。理由：任何差异都是账号存在性的探测信道，
//! 公网暴露下等于免费送上撞库的目标列表。真实原因只进审计和日志。

use std::sync::Arc;

use super::password;
use super::store::PortalDb;
use super::throttle::{LoginThrottle, ThrottleVerdict};

/// 会话有效期：7 天。
///
/// 做常量不做配置项：这是「内部查看页」，7 天在「不用天天登录」和「设备丢了别一直有效」
/// 之间是常规折中。用户说了尽可能简单，多一个配置字段就多一个要解释和要测的东西。
const SESSION_TTL_MS: i64 = 7 * 24 * 3600 * 1000;

/// 单用户最多同时保留的会话数。
///
/// 不限制的话，反复登录会无界堆积会话行（每次登录一条），既是磁盘增长点，也让
/// 「改密清会话」要清的量不可控。5 个够覆盖「手机 + 电脑 + 几个浏览器」。
const MAX_SESSIONS_PER_USER: usize = 5;

/// 认证失败的原因。**注意 `client_message()` 故意不区分前三种。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// 用户名不存在。
    UnknownUser,
    /// 密码错误。
    BadPassword,
    /// 账号被停用。
    Disabled,
    /// 被登录节流拦下，附剩余秒数。
    Throttled { retry_after_secs: u64 },
    /// 注册码不对或未配置。
    BadInviteCode,
    /// 用户名已被占用（仅注册路径）。
    UsernameTaken,
    /// 输入不合规（用户名/密码格式），带**可以**直接展示的原因。
    Invalid(&'static str),
    /// Portal 未启用。
    FeatureDisabled,
    /// 内部错误（DB 写失败等）。
    Internal,
}

impl AuthError {
    /// 给下游看的文案。**前三种必须一致**——见模块级说明。
    pub fn client_message(&self) -> String {
        match self {
            // 用户不存在 / 密码错 / 账号停用 → 同一句话，不泄露账号是否存在。
            Self::UnknownUser | Self::BadPassword | Self::Disabled => {
                "用户名或密码错误".to_string()
            }
            Self::Throttled { retry_after_secs } => {
                format!("尝试过于频繁，请 {retry_after_secs} 秒后再试")
            }
            Self::BadInviteCode => "注册码不正确".to_string(),
            Self::UsernameTaken => "该用户名已被占用".to_string(),
            Self::Invalid(why) => (*why).to_string(),
            Self::FeatureDisabled => "该功能未启用".to_string(),
            Self::Internal => "服务内部错误，请稍后重试".to_string(),
        }
    }

    /// 对应的 HTTP 状态码。
    pub fn status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            Self::UnknownUser | Self::BadPassword | Self::Disabled => StatusCode::UNAUTHORIZED,
            Self::Throttled { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::BadInviteCode | Self::UsernameTaken | Self::Invalid(_) => StatusCode::BAD_REQUEST,
            Self::FeatureDisabled => StatusCode::NOT_FOUND,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// 写进审计的动作名（与 `client_message` 不同：这里要**精确**）。
    fn audit_action(&self) -> &'static str {
        match self {
            Self::UnknownUser => "login_fail_unknown_user",
            Self::BadPassword => "login_fail_bad_password",
            Self::Disabled => "login_fail_disabled",
            Self::Throttled { .. } => "login_fail_throttled",
            Self::BadInviteCode => "register_fail_bad_invite",
            Self::UsernameTaken => "register_fail_username_taken",
            Self::Invalid(_) => "register_fail_invalid_input",
            Self::FeatureDisabled => "request_fail_portal_disabled",
            Self::Internal => "request_fail_internal",
        }
    }
}

/// 登录/注册成功后交给 HTTP 层的东西。
pub struct LoginOk {
    /// **原始**会话令牌，写进 cookie。库里只有它的 SHA-256。
    pub token: String,
    pub username: String,
    pub expires_at_ms: i64,
    /// 该用户的角色。
    ///
    /// 【为何登录响应里就要带上】前端在登录成功那一刻就要决定「渲染不渲染上车
    /// 按钮」。若这里不带，页面得再打一次 `/me` 才知道自己是只读的——那一个
    /// 往返之间按钮已经画出来了，用户可能已经点了。
    pub role: super::role::RoleKind,
}

/// 手写而非 `derive(Debug)`：[`LoginOk::token`] 是**等价于密码的活凭据**，
/// derive 会让任何 `{:?}`（错误链、`unwrap` 的 panic 消息、tracing 的 debug 字段）
/// 把它原样打进日志，日志再被收集/转发，等于会话令牌四处扩散。这里恒打码。
impl std::fmt::Debug for LoginOk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginOk")
            .field("token", &"<redacted>")
            .field("username", &self.username)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// Portal 认证服务。持有 DB 与节流表。
pub struct PortalAuth {
    db: Arc<PortalDb>,
    throttle: LoginThrottle,
}

impl PortalAuth {
    pub fn new(db: Arc<PortalDb>) -> Self {
        Self {
            db,
            throttle: LoginThrottle::new(),
        }
    }

    pub fn db(&self) -> &Arc<PortalDb> {
        &self.db
    }

    /// 审计写失败只记日志，绝不影响主流程返回值——审计是观测手段，
    /// 不能因为它坏了就把用户挡在门外（或反过来放进来）。
    fn audit(
        &self,
        now_ms: i64,
        user_id: Option<i64>,
        username: Option<&str>,
        action: &str,
        ip: Option<&str>,
        detail: Option<&str>,
    ) {
        if let Err(e) = self
            .db
            .add_audit(now_ms, user_id, username, action, ip, detail)
        {
            tracing::warn!("Portal 审计写入失败（不影响主流程）: {:#}", e);
        }
    }

    /// 注册。需要正确的注册码；用户名/密码需过格式校验。
    ///
    /// 【注册也要节流】否则可以用注册接口暴力猜注册码——它是个共享静态值，
    /// 不限速就是纯粹的在线爆破靶子。用同一张节流表，维度是 IP + 固定伪用户名
    /// `"<register>"`（真实用户名此刻还不存在，按它计数没有意义）。
    pub fn register(
        &self,
        username: &str,
        password: &str,
        invite_code: &str,
        ip: Option<&str>,
        now_ms: i64,
    ) -> Result<LoginOk, AuthError> {
        const REGISTER_BUCKET: &str = "<register>";

        if let ThrottleVerdict::Locked { retry_after_secs } =
            self.throttle.check(ip, REGISTER_BUCKET)
        {
            let e = AuthError::Throttled { retry_after_secs };
            self.audit(now_ms, None, Some(username), e.audit_action(), ip, None);
            return Err(e);
        }

        // 注册码：空存储恒 false（fail-closed），即「没配注册码 = 注册通道关闭」。
        if !crate::common::auth_keys::portal_invite_matches(invite_code) {
            self.throttle.record_failure(ip, REGISTER_BUCKET);
            let e = AuthError::BadInviteCode;
            self.audit(now_ms, None, Some(username), e.audit_action(), ip, None);
            return Err(e);
        }

        let username = username.trim();
        if let Err(why) = password::validate_username(username) {
            // 校验函数返回的是动态 String，但 AuthError::Invalid 要 &'static str。
            // 这里不做泄露性拼接：直接给固定文案，精确原因进审计。
            let e = AuthError::Invalid("用户名格式不合规（3-64 位，字母/数字/下划线/连字符/点）");
            self.audit(
                now_ms,
                None,
                Some(username),
                e.audit_action(),
                ip,
                Some(&why.to_string()),
            );
            return Err(e);
        }
        if let Err(why) = password::validate_password_strength(password) {
            let e = AuthError::Invalid("密码不合规（至少 10 位，且不能是纯数字或纯字母）");
            self.audit(
                now_ms,
                None,
                Some(username),
                e.audit_action(),
                ip,
                Some(&why.to_string()),
            );
            return Err(e);
        }

        let phc = match password::hash_password(password) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("Portal 密码哈希失败: {:#}", e);
                return Err(AuthError::Internal);
            }
        };

        let user_id = match self.db.create_user(username, &phc, now_ms) {
            Ok(Some(id)) => id,
            Ok(None) => {
                // 用户名已占用。这**确实**泄露了「这个名字有人用了」——但注册接口无法避免
                // （否则用户永远不知道该换个名字），且已被注册码挡在门外，暴露面可接受。
                let e = AuthError::UsernameTaken;
                self.audit(now_ms, None, Some(username), e.audit_action(), ip, None);
                return Err(e);
            }
            Err(e) => {
                tracing::error!("Portal 创建用户失败: {:#}", e);
                return Err(AuthError::Internal);
            }
        };

        self.throttle.record_success(ip, REGISTER_BUCKET);
        self.audit(
            now_ms,
            Some(user_id),
            Some(username),
            "register_ok",
            ip,
            None,
        );
        tracing::info!("Portal 新用户注册: {} (#{ifmt})", username, ifmt = user_id);

        // 注册后直接给会话，省掉一次「注册成功请登录」的往返。
        // 新注册用户的角色 = 迁移默认值（普通用户）。这里写 `default()` 而不是
        // 硬编码 `User`：默认档若日后改动，注册路径会跟着变，不会留下一处漏改。
        self.issue_session(
            user_id,
            username,
            super::role::RoleKind::default(),
            ip,
            now_ms,
        )
    }

    /// 登录。
    pub fn login(
        &self,
        username: &str,
        password_input: &str,
        ip: Option<&str>,
        now_ms: i64,
    ) -> Result<LoginOk, AuthError> {
        let username = username.trim();

        if let ThrottleVerdict::Locked { retry_after_secs } = self.throttle.check(ip, username) {
            let e = AuthError::Throttled { retry_after_secs };
            self.audit(now_ms, None, Some(username), e.audit_action(), ip, None);
            return Err(e);
        }

        let user = match self.db.find_user_by_name(username) {
            Ok(u) => u,
            Err(e) => {
                tracing::error!("Portal 查询用户失败: {:#}", e);
                return Err(AuthError::Internal);
            }
        };

        let Some(user) = user else {
            // 用户不存在也要跑一次同等成本的哈希，否则「快速失败」本身就暴露了
            // 这个用户名不存在——文案一致也拦不住，因为泄露的是时间。
            password::dummy_verify(password_input);
            self.throttle.record_failure(ip, username);
            let e = AuthError::UnknownUser;
            self.audit(now_ms, None, Some(username), e.audit_action(), ip, None);
            return Err(e);
        };

        if !password::verify_password(password_input, &user.password_hash) {
            self.throttle.record_failure(ip, username);
            let e = AuthError::BadPassword;
            self.audit(
                now_ms,
                Some(user.id),
                Some(&user.username),
                e.audit_action(),
                ip,
                None,
            );
            return Err(e);
        }

        // 密码对了才检查停用。顺序有意：先验密码，使停用账号与正常账号的
        // 响应时间一致，不让攻击者靠「秒回」判断出某个账号存在但被停用。
        if user.disabled {
            self.throttle.record_failure(ip, username);
            let e = AuthError::Disabled;
            self.audit(
                now_ms,
                Some(user.id),
                Some(&user.username),
                e.audit_action(),
                ip,
                None,
            );
            return Err(e);
        }

        // 密码正确但哈希是旧参数 → 趁手上有明文，静默升级。
        // 这是日后调高 argon2 成本时唯一不打扰用户的迁移路径。
        if password::needs_rehash(&user.password_hash) {
            match password::hash_password(password_input) {
                Ok(new_phc) => {
                    // set_password 会清掉该用户所有会话；此刻用户正在登录，
                    // 清掉旧会话正是我们想要的（下面立即发新会话）。
                    if let Err(e) = self.db.set_password(user.id, &new_phc) {
                        tracing::warn!("Portal 密码哈希升级失败（不影响登录）: {:#}", e);
                    } else {
                        tracing::info!("Portal 用户 {} 的密码哈希已升级到当前参数", user.username);
                    }
                }
                Err(e) => tracing::warn!("Portal 密码重算失败（不影响登录）: {:#}", e),
            }
        }

        self.throttle.record_success(ip, username);
        if let Err(e) = self.db.touch_last_login(user.id, now_ms) {
            tracing::warn!("Portal 更新最后登录时间失败: {:#}", e);
        }
        self.audit(
            now_ms,
            Some(user.id),
            Some(&user.username),
            "login_ok",
            ip,
            None,
        );

        self.issue_session(user.id, &user.username, user.role, ip, now_ms)
    }

    /// 发一张会话票。
    fn issue_session(
        &self,
        user_id: i64,
        username: &str,
        role: super::role::RoleKind,
        ip: Option<&str>,
        now_ms: i64,
    ) -> Result<LoginOk, AuthError> {
        let token = match password::generate_session_token() {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Portal 生成会话令牌失败: {:#}", e);
                return Err(AuthError::Internal);
            }
        };
        let token_hash = password::hash_session_token(&token);
        let expires_at_ms = now_ms.saturating_add(SESSION_TTL_MS);

        if let Err(e) = self
            .db
            .create_session(&token_hash, user_id, now_ms, expires_at_ms, ip)
        {
            tracing::error!("Portal 创建会话失败: {:#}", e);
            return Err(AuthError::Internal);
        }

        // 顺手做两件清理：裁掉该用户超量的旧会话、清掉全表过期行。
        // 放在登录路径而非只靠定时任务：登录是低频操作，成本可忽略，
        // 而这样即使定时任务因故没跑，会话表也不会无界增长。
        if let Err(e) = self.db.trim_user_sessions(user_id, MAX_SESSIONS_PER_USER) {
            tracing::warn!("Portal 裁剪用户会话失败: {:#}", e);
        }
        if let Err(e) = self.db.purge_expired_sessions(now_ms) {
            tracing::warn!("Portal 清理过期会话失败: {:#}", e);
        }

        Ok(LoginOk {
            token,
            username: username.to_string(),
            expires_at_ms,
            role,
        })
    }

    /// 校验 cookie 里的令牌，返回身份。
    ///
    /// 失败一律 `None`，不区分「没这张票」「票过期了」「用户被停用了」——
    /// 调用方只需知道「这个请求没有身份」。
    pub fn validate(&self, token: &str, now_ms: i64) -> Option<super::store::PortalSession> {
        if token.is_empty() {
            return None;
        }
        let token_hash = password::hash_session_token(token);
        match self.db.validate_session(&token_hash, now_ms) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Portal 会话校验查询失败: {:#}", e);
                None
            }
        }
    }

    /// 登出：删掉这张票。
    pub fn logout(&self, token: &str, username: Option<&str>, ip: Option<&str>, now_ms: i64) {
        if token.is_empty() {
            return;
        }
        let token_hash = password::hash_session_token(token);
        if let Err(e) = self.db.delete_session(&token_hash) {
            tracing::warn!("Portal 删除会话失败: {:#}", e);
        }
        self.audit(now_ms, None, username, "logout", ip, None);
    }

    /// 记一次**明文外显**。
    ///
    /// 【为何单独开一个公开方法】这是整个系统里唯一把可用凭据明文交出去的出口。
    /// 不留痕就永远回答不了「谁在什么时候拿走了哪些号」——公网暴露 + 不做租户隔离的
    /// 前提下，事后追溯是仅存的约束手段。`count` 记条数而非 key 本身：审计表若也存明文，
    /// 就等于在磁盘上又抄了一份凭据，与「明文不落盘」的设计直接冲突。
    pub fn audit_reveal(
        &self,
        now_ms: i64,
        user_id: i64,
        username: &str,
        ip: Option<&str>,
        count: usize,
    ) {
        self.audit(
            now_ms,
            Some(user_id),
            Some(username),
            "reveal_keys",
            ip,
            Some(&format!("count={count}")),
        );
    }

    /// 记一次上车尝试（成功与失败都记）。
    ///
    /// 【为何失败也要记】只记成功的话，暴力遍历 `credential_id` 试探「哪些车还有空位」
    /// 这类行为在审计里完全不可见——而它不花一分积分，是免费的信息收集。
    /// `action` 用 `board_ok` / `board_already` / `board_insufficient` / `board_full`
    /// 四个值分开，便于直接按 action 聚合，不必解析 detail 文本。
    ///
    /// `extra` 由调用方按情形拼（成功记 price/count/refunded，失败记 needed/balance
    /// 或 count/max）：三种结局关心的数字不同，硬塞进一个固定格式会让一半字段常年为空。
    /// `cred=` 前缀统一在这里加——它是三种情形都必须有的字段，也是事后按 key 聚合的依据。
    pub fn audit_board(
        &self,
        now_ms: i64,
        user_id: i64,
        username: &str,
        ip: Option<&str>,
        action: &str,
        credential_id: i64,
        extra: &str,
    ) {
        self.audit(
            now_ms,
            Some(user_id),
            Some(username),
            action,
            ip,
            Some(&format!("cred={credential_id} {extra}")),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::auth_keys;

    const CODE: &str = "invite-code-for-test";
    const PW: &str = "correct horse 42";

    /// 注册码是**进程级** ArcSwap，与 [`auth_keys`] 自己的用例操作同一个 cell。
    ///
    /// 【为何复用那边的锁而不在这里新建】两把不同的锁守同一份状态等于没有互斥：
    /// 本模块的用例 `set_portal_invite_code(CODE)` 之后，`auth_keys` 里并行跑的
    /// `portal_invite_rotation_and_clear` 可能立刻把它清空，本模块的注册就会
    /// 突然报「注册码不正确」而随机失败。故全进程只有一把锁。
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        crate::common::auth_keys::test_serial()
    }

    fn auth() -> PortalAuth {
        auth_keys::set_portal_invite_code(CODE).unwrap();
        PortalAuth::new(Arc::new(PortalDb::open_in_memory().unwrap()))
    }

    #[test]
    fn register_then_login_roundtrip() {
        let _g = serial();
        let a = auth();
        let ok = a
            .register("alice", PW, CODE, Some("1.1.1.1"), 1000)
            .unwrap();
        assert_eq!(ok.username, "alice");
        assert!(!ok.token.is_empty());

        // 注册直接给会话，不需要再登录一次
        let s = a.validate(&ok.token, 2000).expect("注册后应已有会话");
        assert_eq!(s.username, "alice");

        let login = a.login("alice", PW, Some("1.1.1.1"), 3000).unwrap();
        assert_ne!(login.token, ok.token, "每次登录应发新票");
        assert!(a.validate(&login.token, 4000).is_some());
    }

    /// 注册码不对必须拒绝——这是「内部使用」的唯一技术保障。
    #[test]
    fn register_requires_correct_invite_code() {
        let _g = serial();
        let a = auth();
        assert_eq!(
            a.register("alice", PW, "wrong-code", None, 1000)
                .unwrap_err(),
            AuthError::BadInviteCode
        );
        assert_eq!(
            a.register("alice", PW, "", None, 1000).unwrap_err(),
            AuthError::BadInviteCode
        );
    }

    /// 未配置注册码 = 注册通道关闭（fail-closed）。
    /// 若此用例失败，说明升级版本后凭空多出一个任何人可自助开号的公网入口。
    #[test]
    fn no_invite_code_configured_closes_registration() {
        let _g = serial();
        auth_keys::clear_portal_invite_code();
        let a = PortalAuth::new(Arc::new(PortalDb::open_in_memory().unwrap()));
        assert_eq!(
            a.register("alice", PW, "", None, 1000).unwrap_err(),
            AuthError::BadInviteCode,
            "未配置注册码时空码不得放行"
        );
        assert_eq!(
            a.register("alice", PW, "anything", None, 1000).unwrap_err(),
            AuthError::BadInviteCode
        );
    }

    /// 三种登录失败的对外文案必须**逐字相同**，否则就是账号存在性探测信道。
    #[test]
    fn login_failures_are_indistinguishable() {
        let _g = serial();
        let a = auth();
        a.register("alice", PW, CODE, None, 1000).unwrap();

        let unknown = a.login("nobody", PW, None, 2000).unwrap_err();
        let bad_pw = a
            .login("alice", "wrong password 1", None, 2000)
            .unwrap_err();

        let uid = a.db.find_user_by_name("alice").unwrap().unwrap().id;
        a.db.set_disabled(uid, true).unwrap();
        let disabled = a.login("alice", PW, None, 2000).unwrap_err();

        assert_eq!(unknown, AuthError::UnknownUser);
        assert_eq!(bad_pw, AuthError::BadPassword);
        assert_eq!(disabled, AuthError::Disabled);

        // 内部枚举不同，对外文案必须一致
        assert_eq!(unknown.client_message(), bad_pw.client_message());
        assert_eq!(bad_pw.client_message(), disabled.client_message());
        assert_eq!(unknown.status(), disabled.status());
    }

    /// 停用用户即刻失去访问能力，已发出的票也失效。
    #[test]
    fn disabling_user_kills_existing_session() {
        let _g = serial();
        let a = auth();
        let ok = a.register("alice", PW, CODE, None, 1000).unwrap();
        assert!(a.validate(&ok.token, 2000).is_some());

        let uid = a.db.find_user_by_name("alice").unwrap().unwrap().id;
        a.db.set_disabled(uid, true).unwrap();
        assert!(
            a.validate(&ok.token, 2000).is_none(),
            "停用后旧票必须立即失效"
        );
    }

    #[test]
    fn logout_invalidates_token() {
        let _g = serial();
        let a = auth();
        let ok = a.register("alice", PW, CODE, None, 1000).unwrap();
        a.logout(&ok.token, Some("alice"), None, 2000);
        assert!(a.validate(&ok.token, 3000).is_none());
    }

    /// 会话到期后失效（TTL 边界）。
    #[test]
    fn session_expires_after_ttl() {
        let _g = serial();
        let a = auth();
        let ok = a.register("alice", PW, CODE, None, 1000).unwrap();
        assert!(a.validate(&ok.token, ok.expires_at_ms - 1).is_some());
        assert!(
            a.validate(&ok.token, ok.expires_at_ms).is_none(),
            "到期时刻必须已失效"
        );
    }

    /// 库里绝不能出现原始令牌——否则库泄露等于所有在线会话被接管。
    #[test]
    fn raw_token_never_stored() {
        let _g = serial();
        let a = auth();
        let ok = a.register("alice", PW, CODE, None, 1000).unwrap();
        let hash = password::hash_session_token(&ok.token);
        // 用哈希查得到，说明存的就是哈希
        assert!(a.db.validate_session(&hash, 2000).unwrap().is_some());
        // 用原始令牌当哈希查不到
        assert!(a.db.validate_session(&ok.token, 2000).unwrap().is_none());
    }

    /// 密码明文绝不能进库。
    #[test]
    fn password_stored_as_argon2_not_plaintext() {
        let _g = serial();
        let a = auth();
        a.register("alice", PW, CODE, None, 1000).unwrap();
        let u = a.db.find_user_by_name("alice").unwrap().unwrap();
        assert!(u.password_hash.starts_with("$argon2id$"));
        assert!(!u.password_hash.contains(PW), "库里不得出现密码明文");
    }

    #[test]
    fn duplicate_username_rejected() {
        let _g = serial();
        let a = auth();
        a.register("alice", PW, CODE, None, 1000).unwrap();
        assert_eq!(
            a.register("ALICE", PW, CODE, None, 2000).unwrap_err(),
            AuthError::UsernameTaken,
            "大小写不同也应视为同一用户名"
        );
    }

    #[test]
    fn weak_password_and_bad_username_rejected() {
        let _g = serial();
        let a = auth();
        assert!(matches!(
            a.register("alice", "short", CODE, None, 1000).unwrap_err(),
            AuthError::Invalid(_)
        ));
        assert!(matches!(
            a.register("ab", PW, CODE, None, 1000).unwrap_err(),
            AuthError::Invalid(_)
        ));
    }

    /// 连续失败要被节流拦下，且返回 429。
    #[test]
    fn repeated_failures_get_throttled() {
        let _g = serial();
        let a = auth();
        a.register("alice", PW, CODE, None, 1000).unwrap();

        let ip = Some("9.9.9.9");
        let mut throttled = None;
        for i in 0..12 {
            let now = 2000 + i * 10;
            match a.login("alice", "wrong password 1", ip, now) {
                Err(AuthError::Throttled { retry_after_secs }) => {
                    throttled = Some(retry_after_secs);
                    break;
                }
                Err(_) => continue,
                Ok(_) => panic!("错误密码不该成功"),
            }
        }
        let secs = throttled.expect("连续失败必须触发节流");
        assert!(secs > 0);
        assert_eq!(
            AuthError::Throttled {
                retry_after_secs: secs
            }
            .status(),
            axum::http::StatusCode::TOO_MANY_REQUESTS
        );
    }

    /// 节流期间**即使密码正确**也拒绝——否则爆破者猜中的那一次照样能进。
    #[test]
    fn throttle_blocks_even_correct_password() {
        let _g = serial();
        let a = auth();
        a.register("alice", PW, CODE, None, 1000).unwrap();

        let ip = Some("9.9.9.8");
        for i in 0..12 {
            if let Err(AuthError::Throttled { .. }) =
                a.login("alice", "wrong password 1", ip, 2000 + i * 10)
            {
                break;
            }
        }
        assert!(
            matches!(
                a.login("alice", PW, ip, 2200),
                Err(AuthError::Throttled { .. })
            ),
            "锁定期内正确密码也必须拒绝"
        );
    }

    /// 注册接口本身也要节流，否则注册码成了在线爆破的靶子。
    #[test]
    fn register_is_throttled_to_protect_invite_code() {
        let _g = serial();
        let a = auth();
        let ip = Some("9.9.9.7");
        let mut hit = false;
        for i in 0..12 {
            match a.register("someone", PW, "guess-code", ip, 1000 + i * 10) {
                Err(AuthError::Throttled { .. }) => {
                    hit = true;
                    break;
                }
                Err(AuthError::BadInviteCode) => continue,
                other => panic!("预期注册码错误或节流，实得 {other:?}"),
            }
        }
        assert!(hit, "猜注册码必须会被节流");
    }

    /// 登录成功后审计里应有 login_ok，失败应有对应精确动作。
    #[test]
    fn audit_records_precise_reason() {
        let _g = serial();
        let a = auth();
        a.register("alice", PW, CODE, None, 1000).unwrap();
        let _ = a.login("alice", "wrong password 1", None, 2000);
        let _ = a.login("alice", PW, None, 3000);

        let actions: Vec<String> =
            a.db.recent_audit(20)
                .unwrap()
                .into_iter()
                .map(|e| e.action)
                .collect();
        assert!(actions.contains(&"register_ok".to_string()));
        assert!(actions.contains(&"login_fail_bad_password".to_string()));
        assert!(actions.contains(&"login_ok".to_string()));
    }

    /// 超量会话被裁掉，只留最近的。
    #[test]
    fn old_sessions_trimmed_on_login() {
        let _g = serial();
        let a = auth();
        a.register("alice", PW, CODE, None, 1000).unwrap();

        let mut tokens = Vec::new();
        for i in 0..(MAX_SESSIONS_PER_USER + 3) {
            let ok = a
                .login("alice", PW, None, 2000 + (i as i64) * 1000)
                .unwrap();
            tokens.push(ok.token);
        }
        let alive = tokens
            .iter()
            .filter(|t| a.validate(t, 99_000).is_some())
            .count();
        assert!(
            alive <= MAX_SESSIONS_PER_USER,
            "存活会话 {alive} 超过上限 {MAX_SESSIONS_PER_USER}"
        );
        assert!(
            a.validate(tokens.last().unwrap(), 99_000).is_some(),
            "最近一次登录的票必须存活"
        );
    }

    #[test]
    fn empty_token_rejected() {
        let _g = serial();
        let a = auth();
        assert!(a.validate("", 1000).is_none());
    }

    /// 【安全红线】注册码检查必须在「用户名已占用」之前。
    ///
    /// `UsernameTaken` 天然泄露「这个名字有人用了」——注册接口无法避免。使这个泄露可接受的
    /// 唯一前提，就是拿不到注册码的人根本走不到那一步。若日后有人为了「更友好的报错」
    /// 把用户名检查提前，公网上任何人就能免注册码枚举出全部已注册用户名，
    /// 撞库的搜索空间当场坍缩。本用例锁死这个顺序。
    #[test]
    fn invite_code_is_checked_before_username_taken() {
        let _g = serial();
        let a = auth();
        a.register("alice", PW, CODE, None, 1000).unwrap();

        // 已存在的用户名 + 错的注册码 → 必须报注册码错，绝不能报「已占用」
        assert_eq!(
            a.register("alice", PW, "wrong-code", None, 2000)
                .unwrap_err(),
            AuthError::BadInviteCode,
            "错注册码下暴露了用户名占用状态 = 可免码枚举用户名"
        );

        // 注册码未配置时同理（fail-closed），也不得泄露
        auth_keys::clear_portal_invite_code();
        assert_eq!(
            a.register("alice", PW, "anything", None, 3000).unwrap_err(),
            AuthError::BadInviteCode,
            "未配注册码时暴露了用户名占用状态"
        );
        auth_keys::set_portal_invite_code(CODE).unwrap();
    }

    /// 未配置注册码 = 注册通道关闭（fail-closed），但**登录不受影响**。
    ///
    /// 这两件事必须解耦：运维清掉注册码是为了「不再收新人」，不该把已有用户一起锁在门外。
    #[test]
    fn clearing_invite_code_closes_registration_but_not_login() {
        let _g = serial();
        let a = auth();
        let ok = a.register("alice", PW, CODE, None, 1000).unwrap();
        assert!(a.validate(&ok.token, 2000).is_some());

        auth_keys::clear_portal_invite_code();
        assert_eq!(
            a.register("bob", PW, CODE, None, 3000).unwrap_err(),
            AuthError::BadInviteCode,
            "清空注册码后必须拒绝新注册"
        );
        // 老用户仍能登录
        assert!(
            a.login("alice", PW, None, 4000).is_ok(),
            "清空注册码不该影响已有用户登录"
        );
        auth_keys::set_portal_invite_code(CODE).unwrap();
    }

    /// 停用的账号即使密码正确也登不进，且文案与「密码错」完全一致。
    #[test]
    fn disabled_user_cannot_login_and_message_is_indistinguishable() {
        let _g = serial();
        let a = auth();
        a.register("alice", PW, CODE, None, 1000).unwrap();
        let uid = a.db().find_user_by_name("alice").unwrap().unwrap().id;
        a.db().set_disabled(uid, true).unwrap();

        let disabled_err = a.login("alice", PW, None, 2000).unwrap_err();
        assert_eq!(disabled_err, AuthError::Disabled);

        let bad_pw_err = a
            .login("alice", "wrong password 9", None, 2000)
            .unwrap_err();
        let unknown_err = a.login("nosuchuser", PW, None, 2000).unwrap_err();

        // 三种内部原因不同，但对外文案与状态码必须一致
        assert_eq!(disabled_err.client_message(), bad_pw_err.client_message());
        assert_eq!(disabled_err.client_message(), unknown_err.client_message());
        assert_eq!(disabled_err.status(), unknown_err.status());
        // 而审计里必须能区分（否则运维查不出到底发生了什么）
        assert_ne!(disabled_err.audit_action(), unknown_err.audit_action());
    }

    /// `LoginOk` 的 Debug 输出**绝不能**包含原始令牌——它等价于密码。
    ///
    /// 日志里 `{:?}` 一个结构体是极常见的动作，若 derive(Debug) 就会把活会话票写进日志文件。
    #[test]
    fn debug_output_never_leaks_session_token() {
        let _g = serial();
        let a = auth();
        let ok = a.register("alice", PW, CODE, None, 1000).unwrap();
        let dbg = format!("{ok:?}");
        assert!(!dbg.contains(&ok.token), "Debug 输出泄露了会话令牌: {dbg}");
        assert!(dbg.contains("alice"), "用户名应可见，便于排查: {dbg}");
    }
}
