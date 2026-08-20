//! Kiro API Key (`ksk_`) 粘贴清洗与 `|region` 后缀解析。
//!
//! 由 `service.rs` 以 `#[path]` 接入；`add_credential_with_intent` 仍是调用点。

use crate::admin::types::AddCredentialRequest;
use crate::kiro::model::credentials::KiroCredentials;

/// 清洗粘贴进来的 Kiro API Key（`ksk_`）：截取 `ksk_` 起、去首尾空白与包裹引号/逗号。
///
/// 移植自 k2cc-proxy（`admin/service.rs:346`）。实测用户会把 `"key: ksk_xxx"` 整段贴进
/// 表单，不清洗会同时破坏 region 探测（坏 key）与去重（同一 key 不同前缀可重复导入）。
/// 空串归一为 `None`（与 k2cc 的 `.filter(!is_empty)` 同语义，交给下游「必须提供」报错）。
///
/// Kiro-Go `ksk_…|region`：恰好一段 `|` 且后缀命中 region 白名单时，返回 key 本体；
/// 后缀由 [`apply_ksk_region_suffix`] 写入已有的 `api_region`（请求已带则不覆盖）。
pub(super) fn clean_ksk_api_key(raw: &str) -> Option<String> {
    peel_ksk_paste(raw).map(|(key, _region)| key)
}

/// 从粘贴噪声里取出 `ksk_` 本体，以及可选的 `|region` 后缀。
pub(super) fn peel_ksk_paste(raw: &str) -> Option<(String, Option<String>)> {
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
pub(super) fn split_ksk_region_suffix(key: &str) -> (&str, Option<&str>) {
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

pub(super) fn ksk_region_suffix(raw: &str) -> Option<String> {
    peel_ksk_paste(raw).and_then(|(_, region)| region)
}

/// `ksk_xxx|eu-central-1` 在请求未带 `api_region` 时写入该字段；已有非空值不覆盖。
pub(super) fn apply_ksk_region_suffix(req: &mut AddCredentialRequest) {
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
