//! API Key 的脱敏展示与指纹（面板辨别用，单一真相源）。
//!
//! # 为什么要集中
//! 历史上有两处各自实现的脱敏：`token_manager::mask_api_key`（前 4 + 后 4）与
//! `import_api::mask_key`（前 8 + 后 4）。同一个 key 在凭据管理页显示 `ksk_...gggg`、
//! 在导入卡片显示 `ksk_EXAM...gggg`，运维无法对照确认是不是同一个号。
//!
//! # 格式选择
//! [`mask_api_key`] 取 **前 8 + 后 4**，与对方推送契约的示例（`ksk_Xwbz...SwBh`）同格式，
//! 便于双方对账时肉眼比对。`ksk_` 是所有 key 的公共前缀、无区分度，只留 4 位等于只有
//! 公共前缀可见（实测生产 key `ksk_...gggg` 仅 4 个可辨别字符），故前缀取 8 位让紧跟
//! `ksk_` 之后的 4 位随机字符也可见。
//!
//! # 指纹
//! [`key_fingerprint`] 取 SHA-256 前 8 个 hex 字符。脱敏串在极端情况下可能撞车
//! （同前 8 同后 4），且过短/非 ASCII 的 key 会一律塌成 `***` 完全无法区分；指纹不可逆、
//! 与完整 key 一一对应，是面板上真正可靠的辨别依据。

use sha2::{Digest, Sha256};

/// API Key 脱敏展示：前 8 + `...` + 后 4。
///
/// 与外部推送契约示例同格式（`ksk_Xwbz...SwBh`），便于双方对账。长度不足 16 或非 ASCII
/// 时回退 `***`——此时不暴露任何片段，改由 [`key_fingerprint`] 提供辨别能力。
pub fn mask_api_key(key: &str) -> String {
    if key.is_ascii() && key.len() > 16 {
        format!("{}...{}", &key[..8], &key[key.len() - 4..])
    } else {
        "***".to_string()
    }
}

/// key 指纹：SHA-256 前 8 个 hex 字符。
///
/// 不可逆、与完整 key 一一对应。脱敏串撞车或回退 `***` 时，指纹仍能唯一辨别一个号。
/// 与前端去重用的完整 `api_key_hash` 同源（同一 SHA-256 的前缀），故两者天然一致。
pub fn key_fingerprint(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    format!("{:x}", hasher.finalize())[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与对方契约示例同格式：前 8 + 后 4，肉眼可对账。
    #[test]
    fn mask_matches_contract_format() {
        let key = "ksk_EXAMPLEaaaabbbbccccddddeeeeffffgggg";
        assert_eq!(mask_api_key(key), "ksk_EXAM...gggg");
    }

    /// 只留 4 位前缀等于只显示公共前缀 `ksk_`——本用例锁住「紧跟 ksk_ 的随机位必须可见」。
    #[test]
    fn mask_exposes_chars_beyond_common_prefix() {
        let a = mask_api_key("ksk_aaaa1111222233334444555566667777");
        let b = mask_api_key("ksk_bbbb1111222233334444555566667777");
        assert_ne!(a, b, "同后缀不同前缀的 key 必须能从脱敏串区分开");
        assert!(a.starts_with("ksk_aaaa"));
    }

    /// 过短/非 ASCII 不暴露任何片段（回退 ***），辨别交给指纹。
    #[test]
    fn mask_falls_back_for_short_or_non_ascii() {
        assert_eq!(mask_api_key("ksk_test_1"), "***");
        assert_eq!(mask_api_key("ksk_中文中文中文中文中文中文"), "***");
    }

    /// 指纹稳定、不同 key 不同——脱敏串塌成 *** 时的唯一辨别手段。
    #[test]
    fn fingerprint_distinguishes_keys_that_mask_identically() {
        assert_eq!(mask_api_key("ksk_test_1"), mask_api_key("ksk_test_2"));
        assert_ne!(
            key_fingerprint("ksk_test_1"),
            key_fingerprint("ksk_test_2"),
            "脱敏串相同时，指纹必须能区分"
        );
        assert_eq!(key_fingerprint("ksk_test_1").len(), 8);
    }

    /// 指纹是完整 SHA-256 的前缀，与前端去重用的 hash 同源。
    #[test]
    fn fingerprint_is_prefix_of_full_sha256() {
        let key = "ksk_abcdefghijklmnopqrstuvwxyz";
        let mut hasher = Sha256::new();
        hasher.update(key.as_bytes());
        let full = format!("{:x}", hasher.finalize());
        assert!(full.starts_with(&key_fingerprint(key)));
    }
}
