//! Portal 密码哈希（argon2id）与登录节流。
//!
//! # 为何 argon2id 而非 SHA-256
//! Portal **暴露到公网**，密码库一旦泄露，SHA-256 这类快哈希在 GPU 上是每秒百亿量级的
//! 爆破速度，弱密码几分钟就没了。argon2id 是内存硬（memory-hard）的，攻击者必须为每次
//! 尝试付出真实内存带宽，把爆破成本抬高几个数量级。它也是 OWASP 当前的首选推荐。
//! 选 `id` 变体（而非 `i`/`d`）：同时抵抗侧信道与 GPU/TMTO 攻击，是通用场景的默认答案。
//!
//! # 依赖形态
//! `argon2` 0.5 是**纯 Rust**（无 cc/bindgen），故 musl 静态链接安全——生产镜像是
//! alpine + musl，带 C 依赖的哈希库（bcrypt 的某些绑定）会在那里炸掉。这一点已实测确认。

use anyhow::{Result, anyhow};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};

/// 密码最短长度。
///
/// 公网暴露 + 明文凭据外显，弱密码的代价是整池凭据泄露，故不迁就「方便」。
/// 10 位是在「用户能记住」和「离线爆破不划算」之间的折中：配合 argon2id 的
/// 19MiB×2 迭代成本，10 位混合密码的爆破成本已远超凭据本身的价值。
pub const MIN_PASSWORD_LEN: usize = 10;

/// 密码最长长度。
///
/// argon2 的耗时随输入长度基本恒定，但不设上限等于允许「用 1MB 密码打 DoS」——
/// 每次登录都要为它做一次内存硬哈希。128 对真人足够宽松。
pub const MAX_PASSWORD_LEN: usize = 128;

/// 用户名最长长度（DoS 与展示两方面的约束）。
pub const MAX_USERNAME_LEN: usize = 64;

/// argon2id 参数：19 MiB 内存、2 次迭代、1 路并行。
///
/// 这是 OWASP 对 argon2id 的推荐档之一（m=19456, t=2, p=1）。选它而非更高档位的理由：
/// 单次约 30~50ms，登录体验可接受；而 19MiB 的内存墙已让 GPU 阵列的并行度受限于显存。
/// **注意**：并发登录时内存占用是 19MiB × 并发数，这也是必须有登录节流的原因之一。
const ARGON2_M_COST: u32 = 19 * 1024;
const ARGON2_T_COST: u32 = 2;
const ARGON2_P_COST: u32 = 1;

fn argon2() -> Result<Argon2<'static>> {
    let params = Params::new(ARGON2_M_COST, ARGON2_T_COST, ARGON2_P_COST, None)
        .map_err(|e| anyhow!("构造 argon2 参数失败: {e}"))?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// 校验密码强度。返回 `Err` 时带**用户可见**的中文原因。
///
/// 只做长度与「非纯数字/非纯字母」的基本检查，不搞复杂的字符类组合规则——
/// 那类规则被反复证明只是把用户逼向 `Password1!` 这种可预测形态，
/// 真正的强度来自长度。
pub fn validate_password_strength(password: &str) -> Result<()> {
    let len = password.chars().count();
    if len < MIN_PASSWORD_LEN {
        return Err(anyhow!("密码至少 {MIN_PASSWORD_LEN} 位"));
    }
    if len > MAX_PASSWORD_LEN {
        return Err(anyhow!("密码最长 {MAX_PASSWORD_LEN} 位"));
    }
    if password.chars().all(|c| c.is_ascii_digit()) {
        return Err(anyhow!("密码不能是纯数字"));
    }
    if password.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(anyhow!("密码不能是纯字母，请混入数字或符号"));
    }
    Ok(())
}

/// 校验用户名。允许字母、数字、`_`、`-`、`.`，长度 3~[`MAX_USERNAME_LEN`]。
///
/// 收紧字符集是为了避免同形字/前后空格造成的「看起来是同一个账号」混淆——
/// 表上的 `COLLATE NOCASE` 只解决大小写，解决不了 Unicode 同形。
pub fn validate_username(username: &str) -> Result<()> {
    let len = username.chars().count();
    if len < 3 {
        return Err(anyhow!("用户名至少 3 位"));
    }
    if len > MAX_USERNAME_LEN {
        return Err(anyhow!("用户名最长 {MAX_USERNAME_LEN} 位"));
    }
    if !username
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(anyhow!("用户名只能包含字母、数字、下划线、连字符和点"));
    }
    Ok(())
}

/// 盐长度：16 字节（128 bit），argon2/PHC 的通行推荐值。
const SALT_LEN: usize = 16;

/// 生成随机盐。
///
/// 【为何不用 `SaltString::generate(&mut OsRng)`】那条路要求 `rand_core` 的
/// `getrandom` feature 开着，而本项目里它**只是被 `chacha20poly1305` 的默认特性
/// 顺带打开的**——没有任何一处声明「密码哈希需要它」。哪天有人给那个无关的 crate
/// 加上 `default-features = false`，这里就会编译不过；更糟的是同样的写法在
/// 别的 feature 组合下会静默换掉熵源。改为直接调项目自己已在用的
/// [`getrandom`]（与 [`crate::common::secret_store`] 同一条 CSPRNG 路径），
/// 依赖关系变成显式的。**绝不用 fastrand**：非密码学 PRNG 产出的盐可预测，
/// 彩虹表重新可行。
fn generate_salt() -> Result<SaltString> {
    let mut buf = [0u8; SALT_LEN];
    getrandom::getrandom(&mut buf).map_err(|e| anyhow!("生成密码盐失败: {e}"))?;
    SaltString::encode_b64(&buf).map_err(|e| anyhow!("编码密码盐失败: {e}"))
}

/// 哈希密码，返回 PHC 串（`$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>`）。
///
/// 盐由 [`generate_salt`] 用 OS 熵源生成（非 fastrand——那是非密码学 PRNG，
/// 同盐会让彩虹表重新可行）。
pub fn hash_password(password: &str) -> Result<String> {
    let salt = generate_salt()?;
    let hash = argon2()?
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow!("密码哈希失败: {e}"))?;
    Ok(hash.to_string())
}

/// 验证密码。哈希串损坏或参数不识别时返回 `Ok(false)`，不返回 `Err`——
/// 调用方对「验不过」和「库里的串坏了」应当有同一种对外表现（统一的登录失败），
/// 否则错误文案本身就成了账号存在性与库状态的探测信道。
pub fn verify_password(password: &str, phc: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        tracing::warn!("Portal 用户的密码哈希串无法解析，已按验证失败处理");
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// 对不存在的用户也跑一次同等成本的哈希，抹平时序差异。
///
/// 【为什么必须有】不这么做时，「用户不存在」会立刻返回，而「用户存在、密码错」要等
/// 30~50ms 的 argon2。攻击者据此可以枚举出哪些用户名真实存在，把撞库的搜索空间大幅收窄
/// ——即使响应文案完全一致也拦不住，因为泄露的是**时间**。
///
/// 这里对一个固定的假 PHC 串做验证，成本与真实路径同阶。
pub fn dummy_verify(password: &str) {
    let _ = verify_password(password, DUMMY_PHC);
}

/// 用于 [`dummy_verify`] 的固定 PHC 串，参数与 [`ARGON2_M_COST`] 等一致。
///
/// **不要手写这个串**：它必须能被 [`PasswordHash::new`] 解析成功，否则 [`verify_password`]
/// 会在解析处提前返回，`dummy_verify` 瞬间完成，时序防护静默失效（编译和测试都不会报错，
/// 除了 `dummy_verify_costs_comparable_time` 那一条）。本串由 [`hash_password`] 真实生成。
/// 它对应哪个密码无关紧要——只有**计算成本**有意义，明文永远不参与比较结果。
const DUMMY_PHC: &str = "$argon2id$v=19$m=19456,t=2,p=1$jMjIQUNKZ58hjoXjN33OPw$yXUMKH/BLENNZ+gVdEfIunzeiKDS+rAjIksdHo3D910";

/// 会话令牌：32 字节 OS 熵，base64url 无填充。
///
/// 用 [`getrandom`] 而非 `fastrand`：令牌是**认证凭据**，非密码学 PRNG 的内部状态
/// 可从少量输出反推，等于让攻击者预测他人会话。
pub fn generate_session_token() -> Result<String> {
    use base64::Engine;
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).map_err(|e| anyhow!("生成会话令牌失败: {e}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf))
}

/// 令牌 → SHA-256 hex。**库里只存这个**。
///
/// 数据库泄露时，攻击者拿到 hash 无法反推令牌，因此劫持不了在线会话；
/// 明文存令牌则等于把所有活跃会话一起交出去。
pub fn hash_session_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    format!("{:x}", h.finalize())
}

/// 从 PHC 串里读出 argon2 的 m/t/p，用于「参数升级」判断。
///
/// 日后调高 [`ARGON2_M_COST`] 时，老用户的哈希仍是旧参数。有了这个函数，可以在用户
/// 下次登录成功、手上正握着明文密码的那一刻静默重算，无需强制所有人改密。
pub fn needs_rehash(phc: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        return true; // 串坏了，下次登录成功时重建
    };
    let Ok(params) = Params::try_from(&parsed) else {
        return true;
    };
    params.m_cost() < ARGON2_M_COST || params.t_cost() < ARGON2_T_COST
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_then_verify_roundtrip() {
        let phc = hash_password("correct horse battery").unwrap();
        assert!(phc.starts_with("$argon2id$"), "必须是 argon2id: {phc}");
        assert!(verify_password("correct horse battery", &phc));
        assert!(!verify_password("wrong horse battery", &phc));
    }

    /// 同一密码两次哈希必须不同——相同则说明盐没起作用，彩虹表重新可行。
    #[test]
    fn same_password_different_salt() {
        let a = hash_password("correct horse battery").unwrap();
        let b = hash_password("correct horse battery").unwrap();
        assert_ne!(a, b, "两次哈希相同 = 盐失效");
        assert!(verify_password("correct horse battery", &a));
        assert!(verify_password("correct horse battery", &b));
    }

    /// 参数必须真的写进了 PHC 串，否则「以为调了参数其实没调」不会有任何报错。
    #[test]
    fn phc_carries_expected_params() {
        let phc = hash_password("correct horse battery").unwrap();
        assert!(
            phc.contains(&format!(
                "m={ARGON2_M_COST},t={ARGON2_T_COST},p={ARGON2_P_COST}"
            )),
            "PHC 未携带预期参数: {phc}"
        );
    }

    /// 损坏的哈希串必须是 false，不能 panic——库里若有脏数据，
    /// panic 会把一次登录变成整个进程的可用性问题。
    #[test]
    fn corrupt_phc_returns_false_not_panic() {
        for bad in ["", "not-a-phc", "$argon2id$broken", "$2y$10$abcdefg"] {
            assert!(!verify_password("whatever", bad), "应为 false: {bad}");
        }
    }

    /// 假验证必须真的花掉与真实验证同阶的时间，否则时序信道依然存在。
    #[test]
    fn dummy_verify_costs_comparable_time() {
        let phc = hash_password("correct horse battery").unwrap();

        let t0 = std::time::Instant::now();
        assert!(!verify_password("nope nope nope", &phc));
        let real = t0.elapsed();

        let t1 = std::time::Instant::now();
        dummy_verify("nope nope nope");
        let dummy = t1.elapsed();

        // 只断言「同一数量级」：CI 机器抖动大，卡太死会变成 flaky 测试。
        // 真正要防的是「假验证快 100 倍」这种量级差。
        assert!(
            dummy * 5 > real,
            "假验证过快({dummy:?} vs {real:?})，时序信道未被抹平"
        );
    }

    #[test]
    fn password_strength_rules() {
        assert!(validate_password_strength("abc123!@#xyz").is_ok());
        assert!(validate_password_strength("short1!").is_err(), "太短应拒绝");
        assert!(
            validate_password_strength("1234567890123").is_err(),
            "纯数字应拒绝"
        );
        assert!(
            validate_password_strength("abcdefghijkl").is_err(),
            "纯字母应拒绝"
        );
        assert!(
            validate_password_strength(&"a1".repeat(200)).is_err(),
            "超长应拒绝（DoS）"
        );
    }

    #[test]
    fn username_rules() {
        assert!(validate_username("alice_01").is_ok());
        assert!(validate_username("a.b-c").is_ok());
        assert!(validate_username("ab").is_err(), "太短应拒绝");
        assert!(validate_username("alice bob").is_err(), "空格应拒绝");
        assert!(
            validate_username("张三").is_err(),
            "非 ASCII 应拒绝（同形混淆）"
        );
        assert!(validate_username(&"a".repeat(100)).is_err(), "超长应拒绝");
    }

    /// 令牌必须够长、够随机、且哈希稳定。
    #[test]
    fn session_token_shape_and_hash() {
        let a = generate_session_token().unwrap();
        let b = generate_session_token().unwrap();
        assert_ne!(a, b, "两次令牌相同 = CSPRNG 失效");
        assert!(
            a.len() >= 43,
            "32 字节 base64url 应为 43 字符，实得 {}",
            a.len()
        );
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "令牌含非 URL 安全字符: {a}"
        );

        let h = hash_session_token(&a);
        assert_eq!(h.len(), 64, "SHA-256 hex 应为 64 字符");
        assert_eq!(h, hash_session_token(&a), "同一令牌哈希必须稳定");
        assert_ne!(h, hash_session_token(&b));
        assert!(!h.contains(&a[..8]), "哈希里不该出现令牌原文片段");
    }

    /// 当前参数产出的哈希不需要重算；旧的弱参数需要。
    #[test]
    fn rehash_detection() {
        let current = hash_password("correct horse battery").unwrap();
        assert!(!needs_rehash(&current), "当前参数不应判定需要重算");

        // m=8, t=1 是明显低于当前档位的旧参数
        let weak = Argon2::new(
            Algorithm::Argon2id,
            Version::V0x13,
            Params::new(8, 1, 1, None).unwrap(),
        )
        .hash_password(b"correct horse battery", &generate_salt().unwrap())
        .unwrap()
        .to_string();
        assert!(needs_rehash(&weak), "弱参数应判定需要重算");
        assert!(needs_rehash("garbage"), "坏串应判定需要重算");
    }

    /// `DUMMY_PHC` 必须可解析。
    ///
    /// 【为什么单独测】它不可解析时 [`verify_password`] 会**立刻**返回 false，
    /// [`dummy_verify`] 就退化成空操作，用户名枚举的时序信道悄悄恢复——
    /// 而所有功能测试依然全绿，没有任何报错。这是个会静默失效的防护，
    /// 必须有一条测试直接钉住它。
    #[test]
    fn dummy_phc_is_parseable() {
        let parsed = PasswordHash::new(DUMMY_PHC).expect("DUMMY_PHC 必须可解析");
        let params = Params::try_from(&parsed).expect("DUMMY_PHC 参数必须可读");
        assert_eq!(params.m_cost(), ARGON2_M_COST, "假串成本须与真实路径同阶");
        assert_eq!(params.t_cost(), ARGON2_T_COST);
    }

    /// 验证必须走 PHC 串里**自带**的参数，而不是验证方当前的默认参数。
    ///
    /// 【为什么重要】日后调高 [`ARGON2_M_COST`] 时，库里老用户的哈希还是旧参数。
    /// 若验证时用的是新参数，所有老用户会在升级瞬间全部无法登录，
    /// 而 [`needs_rehash`] 的渐进升级方案也就无从生效（得先能登录成功才能重算）。
    #[test]
    fn verify_honors_params_embedded_in_phc() {
        let weak = Argon2::new(
            Algorithm::Argon2id,
            Version::V0x13,
            Params::new(8, 1, 1, None).unwrap(),
        )
        .hash_password(b"correct horse battery", &generate_salt().unwrap())
        .unwrap()
        .to_string();

        assert!(
            verify_password("correct horse battery", &weak),
            "旧参数哈希必须仍能验证通过，否则升参数会锁死所有老用户"
        );
        assert!(!verify_password("wrong password", &weak));
    }
}
