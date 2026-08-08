//! 拼车管理页的独立二次认证。
//!
//! 外层 `adminApiKey` 只证明调用方能进入通用管理后台；本模块再用独立密码与短时
//! HttpOnly Cookie 保护 `/api/admin/portal/*` 的用户、余额和审计操作。密码仅以
//! Argon2id PHC 串写入 config.json，会话只在内存中保存令牌 SHA-256。

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

use super::{
    password,
    throttle::{LoginThrottle, ThrottleVerdict},
};

pub const COOKIE_NAME: &str = "portal_admin_session";
pub const IDLE_TTL: Duration = Duration::from_secs(30 * 60);
pub const ABSOLUTE_TTL: Duration = Duration::from_secs(8 * 60 * 60);
const MAX_SESSIONS: usize = 32;
const THROTTLE_BUCKET: &str = "<portal-admin>";

#[derive(Debug, Clone)]
struct Session {
    issued_at: Instant,
    last_seen: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginError {
    NotConfigured,
    BadPassword,
    Throttled { retry_after_secs: u64 },
    InsecureTransport,
    AlreadyConfigured,
    InvalidPassword,
    Internal,
}

pub struct LoginSuccess {
    pub token: String,
    pub expires_in_secs: u64,
}

impl std::fmt::Debug for LoginSuccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginSuccess")
            .field("token", &"<redacted>")
            .field("expires_in_secs", &self.expires_in_secs)
            .finish()
    }
}

/// 独立认证域。`setup_lock` 串行化首次设置与改密，杜绝两个并发 setup 都成功。
pub struct PortalAdminAuth {
    password_hash: Mutex<Option<String>>,
    sessions: Mutex<HashMap<String, Session>>,
    throttle: LoginThrottle,
    hash_slots: Arc<Semaphore>,
    setup_lock: AsyncMutex<()>,
    config_path: PathBuf,
    trust_forwarded_header: bool,
}

impl PortalAdminAuth {
    pub fn new(
        password_hash: Option<String>,
        config_path: PathBuf,
        trust_forwarded_header: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            password_hash: Mutex::new(password_hash.filter(|v| !v.trim().is_empty())),
            sessions: Mutex::new(HashMap::new()),
            throttle: LoginThrottle::new(),
            // 每次 Argon2 约占 19 MiB；限制并发避免登录洪水放大成内存 DoS。
            hash_slots: Arc::new(Semaphore::new(2)),
            setup_lock: AsyncMutex::new(()),
            config_path,
            trust_forwarded_header,
        })
    }

    pub fn configured(&self) -> bool {
        self.password_hash.lock().is_some()
    }

    pub fn trust_forwarded_header(&self) -> bool {
        self.trust_forwarded_header
    }

    pub fn precheck(&self, ip: Option<&str>) -> Result<(), LoginError> {
        match self.throttle.check(ip, THROTTLE_BUCKET) {
            ThrottleVerdict::Allow => Ok(()),
            ThrottleVerdict::Locked { retry_after_secs } => {
                Err(LoginError::Throttled { retry_after_secs })
            }
        }
    }

    async fn hash_password_limited(&self, value: String) -> Result<String, LoginError> {
        let permit = self
            .hash_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| LoginError::Internal)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            password::hash_password(&value).map_err(|_| LoginError::Internal)
        })
        .await
        .map_err(|_| LoginError::Internal)?
    }

    async fn verify_limited(&self, value: String, phc: String) -> Result<bool, LoginError> {
        let permit = self
            .hash_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| LoginError::Internal)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            password::verify_password(&value, &phc)
        })
        .await
        .map_err(|_| LoginError::Internal)
    }

    fn issue_session(&self) -> Result<LoginSuccess, LoginError> {
        let token = password::generate_session_token().map_err(|_| LoginError::Internal)?;
        let token_hash = password::hash_session_token(&token);
        let now = Instant::now();
        let mut sessions = self.sessions.lock();
        Self::purge_sessions(&mut sessions, now);
        if sessions.len() >= MAX_SESSIONS {
            if let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, s)| s.issued_at)
                .map(|(k, _)| k.clone())
            {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(
            token_hash,
            Session {
                issued_at: now,
                last_seen: now,
            },
        );
        Ok(LoginSuccess {
            token,
            expires_in_secs: ABSOLUTE_TTL.as_secs(),
        })
    }

    pub async fn setup(
        &self,
        password_value: String,
        ip: Option<&str>,
    ) -> Result<LoginSuccess, LoginError> {
        let _guard = self.setup_lock.lock().await;
        if self.configured() {
            return Err(LoginError::AlreadyConfigured);
        }
        validate_admin_password_strength(&password_value)
            .map_err(|_| LoginError::InvalidPassword)?;
        self.precheck(ip)?;
        let phc = self.hash_password_limited(password_value).await?;

        // 重新从磁盘读取并原子写回，避免覆盖管理台在进程运行期间保存的其它配置。
        let mut config = crate::model::config::Config::load(&self.config_path)
            .map_err(|_| LoginError::Internal)?;
        if config
            .portal_admin_password_hash
            .as_deref()
            .is_some_and(|v| !v.trim().is_empty())
        {
            return Err(LoginError::AlreadyConfigured);
        }
        config.portal_admin_password_hash = Some(phc.clone());
        config.save().map_err(|_| LoginError::Internal)?;
        *self.password_hash.lock() = Some(phc);
        self.sessions.lock().clear();
        self.throttle.record_success(ip, THROTTLE_BUCKET);
        self.issue_session()
    }

    pub async fn login(
        &self,
        password_value: String,
        ip: Option<&str>,
    ) -> Result<LoginSuccess, LoginError> {
        self.precheck(ip)?;
        // 在进入 19 MiB Argon2 计算前拒绝异常长输入。认证路由另有 4 KiB body 上限，
        // 这里是领域层的第二道防线，避免绕过 HTTP 直接调用时形成 CPU/内存放大。
        if password_value.chars().count() > password::MAX_PASSWORD_LEN {
            self.throttle.record_failure(ip, THROTTLE_BUCKET);
            return Err(LoginError::BadPassword);
        }
        let Some(phc) = self.password_hash.lock().clone() else {
            // 即使未配置也跑 dummy verify，避免用响应耗时探测配置状态。
            let _ = self
                .verify_limited(password_value, dummy_phc().to_string())
                .await;
            return Err(LoginError::NotConfigured);
        };
        if !self.verify_limited(password_value, phc).await? {
            self.throttle.record_failure(ip, THROTTLE_BUCKET);
            return Err(LoginError::BadPassword);
        }
        self.throttle.record_success(ip, THROTTLE_BUCKET);
        self.issue_session()
    }

    pub async fn change_password(
        &self,
        token: &str,
        current: String,
        next: String,
        ip: Option<&str>,
    ) -> Result<LoginSuccess, LoginError> {
        if self.validate_and_touch(token).is_none() {
            return Err(LoginError::BadPassword);
        }
        validate_admin_password_strength(&next).map_err(|_| LoginError::InvalidPassword)?;
        if current.chars().count() > password::MAX_PASSWORD_LEN {
            self.throttle.record_failure(ip, THROTTLE_BUCKET);
            return Err(LoginError::BadPassword);
        }
        let _guard = self.setup_lock.lock().await;
        let Some(phc) = self.password_hash.lock().clone() else {
            return Err(LoginError::NotConfigured);
        };
        self.precheck(ip)?;
        if !self.verify_limited(current, phc).await? {
            self.throttle.record_failure(ip, THROTTLE_BUCKET);
            return Err(LoginError::BadPassword);
        }
        let next_phc = self.hash_password_limited(next).await?;
        let mut config = crate::model::config::Config::load(&self.config_path)
            .map_err(|_| LoginError::Internal)?;
        config.portal_admin_password_hash = Some(next_phc.clone());
        config.save().map_err(|_| LoginError::Internal)?;
        *self.password_hash.lock() = Some(next_phc);
        self.sessions.lock().clear();
        self.throttle.record_success(ip, THROTTLE_BUCKET);
        self.issue_session()
    }

    /// 校验并滑动闲置期限。返回绝对过期前剩余秒数。
    pub fn validate_and_touch(&self, token: &str) -> Option<u64> {
        if token.is_empty() || !self.configured() {
            return None;
        }
        let hash = password::hash_session_token(token);
        let now = Instant::now();
        let mut sessions = self.sessions.lock();
        Self::purge_sessions(&mut sessions, now);
        let session = sessions.get_mut(&hash)?;
        session.last_seen = now;
        Some(
            ABSOLUTE_TTL
                .saturating_sub(now.duration_since(session.issued_at))
                .as_secs(),
        )
    }

    pub fn logout(&self, token: &str) {
        if !token.is_empty() {
            self.sessions
                .lock()
                .remove(&password::hash_session_token(token));
        }
    }

    /// 路由级测试专用：直接签发会话，避免每条业务测试都重复跑 Argon2。
    #[cfg(test)]
    pub fn issue_test_session(&self) -> String {
        self.issue_session().expect("测试会话应可签发").token
    }

    fn purge_sessions(sessions: &mut HashMap<String, Session>, now: Instant) {
        sessions.retain(|_, s| {
            now.duration_since(s.issued_at) < ABSOLUTE_TTL
                && now.duration_since(s.last_seen) < IDLE_TTL
        });
    }
}

/// 拼车管理密码比普通 Portal 用户密码更严格：至少 16 位，并覆盖至少三类字符。
pub fn validate_admin_password_strength(value: &str) -> anyhow::Result<()> {
    let len = value.chars().count();
    if len < 16 {
        anyhow::bail!("管理密码至少 16 位");
    }
    if len > password::MAX_PASSWORD_LEN {
        anyhow::bail!("管理密码最长 {} 位", password::MAX_PASSWORD_LEN);
    }
    let classes = [
        value.chars().any(|c| c.is_ascii_lowercase()),
        value.chars().any(|c| c.is_ascii_uppercase()),
        value.chars().any(|c| c.is_ascii_digit()),
        value.chars().any(|c| !c.is_ascii_alphanumeric()),
    ]
    .into_iter()
    .filter(|v| *v)
    .count();
    if classes < 3 {
        anyhow::bail!("管理密码需包含大写字母、小写字母、数字、符号中的至少三类");
    }
    Ok(())
}

// 与 password.rs 的真实参数一致，仅用于未配置路径的恒定成本验证。
fn dummy_phc() -> &'static str {
    "$argon2id$v=19$m=19456,t=2,p=1$jMjIQUNKZ58hjoXjN33OPw$yXUMKH/BLENNZ+gVdEfIunzeiKDS+rAjIksdHo3D910"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_password_policy_is_stricter() {
        assert!(validate_admin_password_strength("Long-Random-Password-2026!").is_ok());
        assert!(validate_admin_password_strength("short-A1!").is_err());
        assert!(validate_admin_password_strength("alllowercaseandlong1").is_err());
        assert!(validate_admin_password_strength("ALLUPPERCASE123456").is_err());
    }

    #[test]
    fn sessions_expire_on_idle_or_absolute_deadline() {
        let now = Instant::now();
        let mut sessions = HashMap::new();
        sessions.insert(
            "idle".into(),
            Session {
                issued_at: now,
                last_seen: now - IDLE_TTL - Duration::from_secs(1),
            },
        );
        sessions.insert(
            "absolute".into(),
            Session {
                issued_at: now - ABSOLUTE_TTL - Duration::from_secs(1),
                last_seen: now,
            },
        );
        sessions.insert(
            "valid".into(),
            Session {
                issued_at: now,
                last_seen: now,
            },
        );
        PortalAdminAuth::purge_sessions(&mut sessions, now);
        assert_eq!(sessions.len(), 1);
        assert!(sessions.contains_key("valid"));
    }

    #[tokio::test]
    async fn overlong_login_is_rejected_before_hashing() {
        let phc = password::hash_password("Portal-Admin-Test-2026!").unwrap();
        let auth = PortalAdminAuth::new(
            Some(phc),
            std::env::temp_dir().join("unused-portal-admin-overlong.json"),
            false,
        );
        let too_long = "A1!".repeat(password::MAX_PASSWORD_LEN + 1);
        assert!(matches!(
            auth.login(too_long, Some("127.0.0.1")).await,
            Err(LoginError::BadPassword)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn password_change_revokes_every_old_session_and_persists_hash() {
        let old = "Portal-Admin-Old-2026!";
        let new = "Portal-Admin-New-2026!";
        let phc = password::hash_password(old).unwrap();
        let path = std::env::temp_dir().join(format!(
            "kirostudio-portal-admin-{}.json",
            uuid::Uuid::new_v4()
        ));
        let mut config = crate::model::config::Config::default();
        config.portal_admin_password_hash = Some(phc.clone());
        std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();

        let auth = PortalAdminAuth::new(Some(phc), path.clone(), false);
        let first = auth.login(old.into(), Some("127.0.0.1")).await.unwrap();
        let second = auth.login(old.into(), Some("127.0.0.2")).await.unwrap();
        let replacement = auth
            .change_password(&first.token, old.into(), new.into(), Some("127.0.0.1"))
            .await
            .unwrap();

        assert!(auth.validate_and_touch(&first.token).is_none());
        assert!(auth.validate_and_touch(&second.token).is_none());
        assert!(auth.validate_and_touch(&replacement.token).is_some());
        assert!(matches!(
            auth.login(old.into(), Some("127.0.0.3")).await,
            Err(LoginError::BadPassword)
        ));
        assert!(auth.login(new.into(), Some("127.0.0.4")).await.is_ok());

        let persisted = crate::model::config::Config::load(&path).unwrap();
        let persisted_phc = persisted.portal_admin_password_hash.unwrap();
        assert!(password::verify_password(new, &persisted_phc));
        assert!(!password::verify_password(old, &persisted_phc));
        std::fs::remove_file(path).unwrap();
    }
}
