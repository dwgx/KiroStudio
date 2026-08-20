//! 两把鉴权密钥的热更单元（单一真相源）。
//!
//! # 为什么需要
//! `apiKey`（下游 /v1 对话）/`adminApiKey`（管理面）原先都在启动时 clone 进各自的
//! State（`AppState.api_key` / `AdminState.admin_api_key`），改配置必须重启才生效。
//! 轮换密钥是常规运维动作，重启整个网关代价过高（断开在途流式请求）。
//! 本模块把两把 key 收进进程级 `ArcSwap`，admin 存盘后调 setter 即时生效。
//!
//! # 安全命门：空值 = 网关裸奔
//! [`crate::common::auth::constant_time_eq`] 对 `("", "")` 返回 **true**。若空串进了热更单元，
//! 任意 `x-api-key:`（空值请求头）都会通过鉴权，`/v1/*` 变成匿名可用、直接白刷上游凭据。
//! 原先靠两道防线兜住（启动 `exit(1)` + `update_config` 拒空），但**热更绕过了启动检查**。
//! 故本模块把判定收进 `*_matches()`：存储值为空时**恒 false**（fail-closed），
//! 且 [`set_user_key`] / [`set_admin_key`] 拒绝写入空值。这是第三道、也是最后一道防线。
//!
//! # 移植说明
//! 移植自参考仓 `common/auth_keys.rs`（四 key → 本项目只有两把，取 userKey/adminApiKey）。
//! import/relay 两把不在此仓使用，未移植。

use std::sync::OnceLock;

use arc_swap::ArcSwap;

use super::auth::constant_time_eq;

/// 空串表示「未配置」。每把 key 各一个独立单元（互不影响，避免一次 store 覆盖另一把）。
fn cell(which: Which) -> &'static ArcSwap<String> {
    static USER: OnceLock<ArcSwap<String>> = OnceLock::new();
    static ADMIN: OnceLock<ArcSwap<String>> = OnceLock::new();
    match which {
        Which::User => USER.get_or_init(|| ArcSwap::from_pointee(String::new())),
        Which::Admin => ADMIN.get_or_init(|| ArcSwap::from_pointee(String::new())),
    }
}

#[derive(Copy, Clone)]
enum Which {
    User,
    Admin,
}

/// 比对候选值与当前存储值。**存储值为空 → 恒 false**（fail-closed，见模块级安全说明）。
fn matches(which: Which, candidate: &str) -> bool {
    let stored = cell(which).load();
    if stored.is_empty() {
        return false;
    }
    constant_time_eq(candidate, &stored)
}

/// 写入一把 key。空白值直接拒绝（防 fail-open）；成功返回 `Ok(())`。
fn set(which: Which, value: &str, label: &str) -> Result<(), String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("{label} 不能为空（空值会导致鉴权 fail-open）"));
    }
    cell(which).store(std::sync::Arc::new(trimmed.to_string()));
    Ok(())
}

/// 设置下游对话 key（`x-api-key`）。拒绝空值。
pub fn set_user_key(value: &str) -> Result<(), String> {
    set(Which::User, value, "apiKey")
}

/// 设置管理面 key。拒绝空值。
pub fn set_admin_key(value: &str) -> Result<(), String> {
    set(Which::Admin, value, "adminApiKey")
}

pub fn user_key_matches(candidate: &str) -> bool {
    matches(Which::User, candidate)
}

pub fn admin_key_matches(candidate: &str) -> bool {
    matches(Which::Admin, candidate)
}

/// 测试用的全局串行锁。守护这两把 key 的**唯一**一把锁，不要在别处再建一把。
///
/// 【为什么必须放在模块作用域而非各自的 `mod tests` 里】本模块的被测状态是**进程级**的
/// （几个 `OnceLock<ArcSwap>`），而 cargo test 默认多线程并行跑同一进程内的用例：
/// A 刚 `set_user_key("sk-new")`，并行的 B 立刻覆写成 "user-1"，A 的断言就会看到别人的值。
///
/// 日后若别处的用例也要动这些 key（如 service.rs 的 update_config 热更测试），
/// 必须从这里取锁：各建一个私有 `static SERIAL` 就是**两把不同的锁守同一份状态**——
/// 等于没有互斥，测试会随机失败且极难复现。
#[cfg(test)]
pub static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 取全局串行锁。
///
/// 用 `unwrap_or_else(|e| e.into_inner())` 容忍投毒：某个用例断言失败会 panic 并投毒此锁，
/// 若直接 unwrap 会让后续用例全部连带失败、掩盖真正的那一个。
#[cfg(test)]
pub fn test_serial() -> std::sync::MutexGuard<'static, ()> {
    TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        test_serial()
    }

    /// 安全红线：存储值为空时任何候选都不得通过——包括空候选。
    /// 若此用例失败，说明 `/v1` 对 `x-api-key:`（空头）fail-open，整个网关裸奔。
    #[test]
    fn empty_stored_rejects_everything_including_empty_candidate() {
        let _g = serial();
        set_user_key("").err().expect("setter 必须拒绝空值");
        assert!(!user_key_matches(""), "空存储 + 空候选必须拒绝");
        assert!(!user_key_matches("anything"));
        assert!(!admin_key_matches(""));
    }

    /// setter 拒绝空白值（含纯空格），防止「保存空字符串」把网关打开。
    #[test]
    fn setters_reject_blank_values() {
        let _g = serial();
        assert!(set_user_key("").is_err());
        assert!(set_user_key("   ").is_err());
        assert!(set_admin_key("\t\n").is_err());
        assert!(set_admin_key(" ").is_err());
    }

    /// 热更后立刻按新值判定：旧值失效、新值通过（这正是「不重启生效」的定义）。
    #[test]
    fn hot_swap_takes_effect_immediately() {
        let _g = serial();
        set_user_key("sk-old").unwrap();
        assert!(user_key_matches("sk-old"));
        set_user_key("sk-new").unwrap();
        assert!(!user_key_matches("sk-old"), "旧 key 必须立即失效");
        assert!(user_key_matches("sk-new"));
    }

    /// 两把 key 相互独立：写一把不得影响另一把（曾用同一 cell 会串味）。
    ///
    /// 【为何专门锁这条】两把 key 共用同一套 `matches`/`set` 代码，若 `cell()` 的
    /// match 分支写错（复制粘贴时漏改一处），会出现「用 adminApiKey 也能打开 /v1」
    /// 这类无声的权限穿透——编译器不会报错，只有断言能抓住。
    #[test]
    fn keys_are_independent() {
        let _g = serial();
        set_user_key("user-1").unwrap();
        set_admin_key("admin-1").unwrap();
        assert!(user_key_matches("user-1"));
        assert!(admin_key_matches("admin-1"));
        assert!(!user_key_matches("admin-1"), "不同 key 不得互相通过");
        assert!(!admin_key_matches("user-1"), "穿透是双向的，只查一个方向会漏");
    }

    /// setter 存 trim 后的值：配置里带尾随换行也能匹配用户实际发送的 key。
    #[test]
    fn stored_value_is_trimmed() {
        let _g = serial();
        set_admin_key("  sk-padded  ").unwrap();
        assert!(admin_key_matches("sk-padded"));
    }
}
