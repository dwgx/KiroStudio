//! 敏感文件 at-rest 加密(credentials.json / trash.json)。
//!
//! # 为什么需要
//! credentials.json 明文落盘,里面是 access_token/refresh_token/api_key/proxy_password 等**可直接
//! 复用的凭证**——文件被读到(服务器被登入/硬盘被拷/误传)即失守。尤其 Windows 上 fs_atomic 的
//! 0600 权限收紧是 no-op(裸奔),at-rest 加密价值更高。
//!
//! # 设计(务实、防呆优先)
//! - **对称 AEAD**:XChaCha20-Poly1305(纯 Rust 无 C 依赖,24 字节 nonce 随机不怕碰撞,带认证防篡改)。
//! - **机器绑定密钥**:key = SHA256("kirostudio-at-rest-v1" ‖ hostname ‖ os_user)。零配置、自动派生;
//!   同机器透明解密,文件拷到别的机器解不开(降低"整盘搬走"风险)。**换机器 token 本来也常失效,
//!   重新上号即可**——损失小。导出/导入接口走的是明文,与本加密**完全无关**,通用性不受影响。
//! - **可识别信封**:密文文件带 magic 前缀 `KSENC1\n`,`load` 侧据此区分明文/密文,实现**透明迁移**
//!   (读到明文照旧解析,下次 persist 才按开关决定是否写密文)。
//! - **只读侧永不因加密崩**:`maybe_decrypt` 拿到明文原样返回、拿到密文才解密;解密失败给清晰错误
//!   (而非当明文塞给 serde 报一堆看不懂的 JSON 错)。
//!
//! # 边界(诚实)
//! 机器绑定不是抗本机攻击者(能在本机跑本程序的人也能派生同一密钥)——它挡的是"文件离开本机"
//! (拷盘/误传/备份泄露)。要抗更强威胁需 HSM/OS keychain,超出自托管单机范围。

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use std::path::{Path, PathBuf};

/// 密文文件 magic 前缀(含版本);`load` 侧据此区分明文/密文。
const MAGIC: &[u8] = b"KSENC1\n";
/// 密钥文件名(与 credentials.json 同目录的隐藏文件)。
const KEY_FILE_NAME: &str = ".at_rest.key";

/// 给定 credentials.json 路径,返回同目录下的密钥文件路径。
/// 密钥与凭据文件同目录、但**独立成文件**:导出/备份 credentials.json 时通常不带走密钥文件,
/// 故"文件被单独拷走/误传"仍解不开(达到 at-rest 保护目的);而密钥**持久化不随机器属性漂移**,
/// 彻底消除"hostname/启动方式变化→解不开自己文件"的锁死风险。
pub fn key_path_for(credentials_path: &Path) -> PathBuf {
    let dir = credentials_path.parent().unwrap_or_else(|| Path::new("."));
    dir.join(KEY_FILE_NAME)
}

/// 用 OS CSPRNG 填满缓冲(密钥/nonce 材料)。**绝不用 fastrand**(那是 64-bit 非密码学 PRNG,
/// 会把 256-bit 密钥的有效熵坍到 ~64 位)。getrandom 直取操作系统熵源。
fn fill_random(buf: &mut [u8]) -> anyhow::Result<()> {
    getrandom::getrandom(buf).map_err(|e| anyhow::anyhow!("CSPRNG 取随机失败: {e}"))
}

/// 从 32 字节切片构造密钥数组(长度已在调用点校验)。
fn key_from_slice(bytes: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    key.copy_from_slice(bytes);
    key
}

/// 读取或首次创建 32 字节随机密钥文件(0600)。
/// - 已存在且恰 32 字节 → 读用。
/// - 不存在 → 用 CSPRNG 生成 32 字节,**以 `create_new`(O_EXCL)原子创建**写入,返回。
/// - **并发首次竞态**:两线程同时进来各生成不同密钥,`create_new` 保证只有一个成功创建;
///   失败方(AlreadyExists)**回读已落盘的那把**——所有并发加密方最终都用磁盘上唯一的密钥,
///   杜绝"密钥文件与密文错配→下次启动解不开锁死"(review HIGH #1)。
/// - 存在但长度不对(损坏)→ Err(绝不用错长度的密钥,以免产出永远解不开的密文)。
fn get_or_create_key(key_path: &Path) -> anyhow::Result<[u8; 32]> {
    // 快路径:已存在直接读。
    if let Ok(bytes) = std::fs::read(key_path) {
        if bytes.len() != 32 {
            anyhow::bail!(
                "密钥文件 {:?} 长度异常({} 字节,应 32)。若被损坏,删除它会导致已加密的凭据永久\
                 解不开——请用明文备份重新导入。",
                key_path,
                bytes.len()
            );
        }
        return Ok(key_from_slice(&bytes));
    }

    // 慢路径:尝试原子创建。CSPRNG 生成候选密钥。
    let mut candidate = [0u8; 32];
    fill_random(&mut candidate)?;

    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true); // O_EXCL:文件已存在则失败,保证唯一创建者
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(key_path) {
        Ok(mut f) => {
            f.write_all(&candidate)
                .and_then(|_| f.sync_all())
                .map_err(|e| anyhow::anyhow!("写入密钥文件失败 {:?}: {e}", key_path))?;
            // Unix 已在 open 时 0600;非 Unix 尽力收紧(Windows 为 no-op)。
            crate::common::fs_atomic::restrict_permissions(key_path);
            tracing::info!("已生成 at-rest 加密密钥文件: {:?}(0600,勿删,勿随 credentials 导出)", key_path);
            Ok(candidate)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // 竞态输家:另一线程已创建密钥,回读那把(而非用自己刚生成、没落盘的 candidate)。
            let bytes = std::fs::read(key_path)
                .map_err(|e| anyhow::anyhow!("竞态后回读密钥文件失败 {:?}: {e}", key_path))?;
            if bytes.len() != 32 {
                anyhow::bail!("密钥文件 {:?} 长度异常({} 字节,应 32)", key_path, bytes.len());
            }
            Ok(key_from_slice(&bytes))
        }
        Err(e) => Err(anyhow::anyhow!("创建密钥文件失败 {:?}: {e}", key_path)),
    }
}

/// 仅读取已存在的密钥文件(解密路径用;不创建——解密时密钥必须已存在)。
fn read_existing_key(key_path: &Path) -> anyhow::Result<[u8; 32]> {
    let bytes = std::fs::read(key_path).map_err(|e| {
        anyhow::anyhow!(
            "凭据文件是密文,但密钥文件 {:?} 读取失败({e})。可能原因:密钥文件被删/移走,或 \
             credentials.json 来自另一台机器却没带上它的密钥文件。恢复:用明文备份重新导入。",
            key_path
        )
    })?;
    if bytes.len() != 32 {
        anyhow::bail!("密钥文件 {:?} 长度异常({} 字节,应 32),无法解密。", key_path, bytes.len());
    }
    Ok(key_from_slice(&bytes))
}

/// 判断字节流是否是本模块加密的密文(带 magic 前缀)。
pub fn is_encrypted(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

/// 用给定密钥加密明文 → 密文信封字节(magic ‖ nonce(24) ‖ ciphertext)。
///
/// nonce 每次随机(XChaCha20 的 24 字节 nonce 空间极大,随机不怕碰撞),故同一明文两次加密
/// 密文不同(不泄露"内容没变")。失败(极罕见)返回 Err,调用方回退明文写(绝不因加密丢数据)。
fn encrypt_with_key(plaintext: &[u8], key: &[u8; 32]) -> anyhow::Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    // nonce 用 OS CSPRNG(非 fastrand):XChaCha 24 字节 nonce 空间极大,CSPRNG 随机杜绝碰撞。
    let mut nonce_bytes = [0u8; 24];
    fill_random(&mut nonce_bytes)?;
    let nonce = XNonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("加密失败: {e}"))?;
    let mut out = Vec::with_capacity(MAGIC.len() + nonce_bytes.len() + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// 用给定密钥解密密文信封 → 明文字节。仅当 [`is_encrypted`] 为真时调用。
fn decrypt_with_key(bytes: &[u8], key: &[u8; 32]) -> anyhow::Result<Vec<u8>> {
    let body = &bytes[MAGIC.len()..];
    if body.len() < 24 {
        anyhow::bail!("密文损坏:长度不足(缺 nonce)");
    }
    let (nonce_bytes, ciphertext) = body.split_at(24);
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    let nonce = XNonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| anyhow::anyhow!("解密失败:密文被篡改/损坏,或密钥文件与该密文不匹配。"))
}

/// 透明读取:密文则用同目录密钥文件解密,明文则原样返回。供 credentials/trash 的 load 侧统一调用。
///
/// **迁移兼容核心**:老用户明文文件照旧直通(不碰密钥文件、不会因加密而崩);密文才需密钥。
/// 解密失败给**可恢复指引**(而非裸 serde 错),让"解不开"的原因一目了然。
pub fn maybe_decrypt_to_string(bytes: &[u8], key_path: &Path) -> anyhow::Result<String> {
    if is_encrypted(bytes) {
        let key = read_existing_key(key_path)?;
        let plain = decrypt_with_key(bytes, &key)?;
        String::from_utf8(plain).map_err(|e| anyhow::anyhow!("解密后非合法 UTF-8: {e}"))
    } else {
        // 明文直通(BOM 容错交给上层 serde)。
        String::from_utf8(bytes.to_vec()).map_err(|e| anyhow::anyhow!("文件非合法 UTF-8: {e}"))
    }
}

/// 落盘编码:enabled 时用密钥文件加密;否则原样明文。返回 (字节, 是否真加密)。
///
/// **防呆**:加密失败(密钥文件读写失败/加密报错)不吞数据——回退明文 + 告警,并把 `false` 回给调用方,
/// 让上层可观测"开了加密但这次其实是明文"(消除安全预期与现实的偏差,见 recovery_metrics)。
pub fn encode_for_disk(plaintext: &[u8], enabled: bool, key_path: &Path) -> (Vec<u8>, bool) {
    if !enabled {
        return (plaintext.to_vec(), false);
    }
    match get_or_create_key(key_path).and_then(|key| encrypt_with_key(plaintext, &key)) {
        Ok(ct) => (ct, true),
        Err(e) => {
            tracing::error!("at-rest 加密失败,本次回退明文落盘(数据不丢,但未加密): {e}");
            (plaintext.to_vec(), false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 每个测试用独立临时目录,密钥文件落在其中(隔离,不互相污染)。
    fn tmp_key_path(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ks_secret_{}_{}", tag, uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cred = dir.join("credentials.json");
        let key = key_path_for(&cred);
        (dir, key)
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let (dir, kp) = tmp_key_path("rt");
        let plain = br#"[{"id":1,"refreshToken":"secret-token-xyz"}]"#;
        let (ct, encd) = encode_for_disk(plain, true, &kp);
        assert!(encd, "应真加密");
        assert!(is_encrypted(&ct), "密文应带 magic 前缀");
        assert!(!ct.windows(plain.len()).any(|w| w == plain), "密文不应含明文");
        assert!(kp.exists(), "首次加密应创建密钥文件");
        let back = maybe_decrypt_to_string(&ct, &kp).unwrap();
        assert_eq!(back.as_bytes(), plain, "解密应还原明文");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_plaintext_passthrough() {
        // 明文(无 magic)直通,不当密文解密、不碰密钥文件(迁移兼容:老明文文件照旧能读)。
        let (dir, kp) = tmp_key_path("pt");
        let plain = br#"[{"id":1}]"#;
        assert!(!is_encrypted(plain));
        let s = maybe_decrypt_to_string(plain, &kp).unwrap();
        assert_eq!(s, r#"[{"id":1}]"#);
        assert!(!kp.exists(), "明文直通不应创建密钥文件");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_encode_for_disk_toggle() {
        let (dir, kp) = tmp_key_path("tog");
        let plain = br#"{"a":1}"#;
        // 关:原样明文,不建密钥。
        let (off, encd_off) = encode_for_disk(plain, false, &kp);
        assert_eq!(off, plain);
        assert!(!encd_off);
        assert!(!is_encrypted(&off));
        assert!(!kp.exists());
        // 开:密文信封。
        let (on, encd_on) = encode_for_disk(plain, true, &kp);
        assert!(encd_on);
        assert!(is_encrypted(&on));
        assert_eq!(maybe_decrypt_to_string(&on, &kp).unwrap().as_bytes(), plain);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_key_persists_across_calls() {
        // 关键:同一密钥文件跨多次加密复用(不像旧机器派生每次重算)——保证重启后仍能解开。
        let (dir, kp) = tmp_key_path("persist");
        let (ct1, _) = encode_for_disk(b"first", true, &kp);
        // 第二次加密复用同一密钥文件;两段密文应能互相用同密钥解开。
        let (ct2, _) = encode_for_disk(b"second", true, &kp);
        assert_eq!(maybe_decrypt_to_string(&ct1, &kp).unwrap(), "first");
        assert_eq!(maybe_decrypt_to_string(&ct2, &kp).unwrap(), "second");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_concurrent_first_key_creation_converges() {
        // 回归(review HIGH #1):多线程同时首次建密钥,create_new(O_EXCL) 保证只有一个赢,
        // 输家回读赢家密钥——所有线程最终拿到同一把密钥,杜绝密钥/密文错配锁死。
        let (dir, kp) = tmp_key_path("race");
        let mut handles = Vec::new();
        for _ in 0..8 {
            let kp = kp.clone();
            handles.push(std::thread::spawn(move || {
                // 每个线程独立首次加密同一明文,各自触发 get_or_create_key。
                encode_for_disk(b"race-data", true, &kp).0
            }));
        }
        let ciphertexts: Vec<Vec<u8>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        // 关键:磁盘上最终那把密钥能解开**每一个**线程产出的密文(否则说明有线程用了没落盘的异密钥)。
        for ct in &ciphertexts {
            assert_eq!(
                maybe_decrypt_to_string(ct, &kp).unwrap(),
                "race-data",
                "并发建密钥后,磁盘密钥应能解开所有线程的密文"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_decrypt_fails_when_key_missing() {
        // 密文存在但密钥文件被删/没带 → 解密失败给可恢复指引(不 panic)。
        let (dir, kp) = tmp_key_path("nokey");
        let (ct, _) = encode_for_disk(b"data", true, &kp);
        std::fs::remove_file(&kp).unwrap();
        let err = maybe_decrypt_to_string(&ct, &kp).unwrap_err();
        assert!(err.to_string().contains("密钥文件"), "错误应指明密钥文件问题");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_tamper_detection() {
        let (dir, kp) = tmp_key_path("tamper");
        let (mut ct, _) = encode_for_disk(b"important", true, &kp);
        let last = ct.len() - 1;
        ct[last] ^= 0xff;
        assert!(maybe_decrypt_to_string(&ct, &kp).is_err(), "篡改的密文应解密失败");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_corrupt_ciphertext_too_short() {
        let (dir, kp) = tmp_key_path("short");
        // 先建密钥,再喂长度不足的密文 → 报错不 panic。
        let _ = encode_for_disk(b"x", true, &kp);
        let mut bad = MAGIC.to_vec();
        bad.extend_from_slice(b"xy");
        assert!(maybe_decrypt_to_string(&bad, &kp).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
