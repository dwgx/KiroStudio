//! 公共认证工具函数

use axum::{
    body::Body,
    http::{Request, header},
};
use subtle::ConstantTimeEq;

/// 从请求中提取 API Key
///
/// 支持两种认证方式：
/// - `x-api-key` header
/// - `Authorization: Bearer <token>` header
///
/// 两处**空值/大小写**的宽容是有意的（2026-10-14 审计）：
/// - `x-api-key:`（空值，反代/浏览器/误配客户端会发）视为**未提供**。原先直接 return，
///   于是同一请求里合法的 `Authorization: Bearer <真 key>` 永远看不到 → 带着真凭据拿 401；
///   `HeaderMap::get` 只取第一个值，空的首值也遮住后续同名头。空值判定用 `trim()`，
///   但**不改动**取出的值（存储侧 `auth_keys::set` 已 trim，比较仍按原字节）。
/// - Bearer 的 scheme 按 RFC 9110 §11.1 **大小写不敏感**（`bearer` / `BEARER` 合法），
///   凭据两侧的 OWS 也 trim。原先 `strip_prefix("Bearer ")` 精确匹配，合规客户端被 401。
///   空 token 仍返回 None（`Authorization: Bearer ` 不得变成 `Some("")`）。
pub fn extract_api_key(request: &Request<Body>) -> Option<String> {
    // 优先检查 x-api-key
    if let Some(key) = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .filter(|k| !k.trim().is_empty())
    {
        return Some(key.to_string());
    }

    // 其次检查 Authorization: Bearer
    request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, token) = v.split_once(' ')?;
            if !scheme.eq_ignore_ascii_case("bearer") {
                return None;
            }
            let token = token.trim();
            if token.is_empty() {
                return None;
            }
            Some(token.to_string())
        })
}

/// 常量时间字符串比较，防止时序攻击
///
/// 比较本身恒定时间：先各自 SHA-256 成定长 32 字节摘要，再 `subtle::ConstantTimeEq`。
///
/// review Finding 8 修复:`subtle` 对不等长 slice 会提前返回 → 泄漏长度侧信道(可测出 key 长度)。
/// 摘要化后**比较**与长度无关（任何长度输入都走同一条 32 字节比较路径）。
/// 摘要相等 ⟺ 原文相等(抗碰撞),安全等价。
///
/// **残留（诚实标注，别再当成"完全没有长度侧信道"）**：`Sha256::digest` 自身耗时正比于
/// 输入长度，而其中一侧是存储密钥（apiKey/adminApiKey）⇒ 高精度远端计时理论上仍能测出
/// **密钥长度**（不是内容，且需亚微秒级计时精度）。要彻底消除得把存储密钥的摘要**预计算**
/// 一次（改 `common::auth_keys` 的 set/matches 签名），不在本模块范围内——见 2026-10-14 审计。
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    use sha2::{Digest, Sha256};
    let ha = Sha256::digest(a.as_bytes());
    let hb = Sha256::digest(b.as_bytes());
    ha.ct_eq(&hb).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个带指定请求头的 `Request<Body>`。
    fn req(pairs: &[(&str, &str)]) -> Request<Body> {
        let mut builder = Request::builder();
        for (k, v) in pairs {
            builder = builder.header(*k, *v);
        }
        builder.body(Body::empty()).unwrap()
    }

    /// 空 `x-api-key` 视为**未提供**：不得遮住同一请求里合法的 Bearer。
    ///
    /// 2026-10-14 审计：原先空值直接 `return Some("")`，于是带着真 key（走 Authorization）的
    /// 客户端在反代/浏览器补了个空 `x-api-key` 头时恒拿 401；`HeaderMap::get` 只取首值，
    /// 空的首值还会遮住后续同名头。
    #[test]
    fn blank_x_api_key_falls_through_to_bearer() {
        let r = req(&[("x-api-key", ""), ("authorization", "Bearer real-key")]);
        assert_eq!(extract_api_key(&r).as_deref(), Some("real-key"));

        // 纯空白同样视为未提供；无 Bearer 时仍 None（绝不产出 Some("") 去和存储值比）
        assert_eq!(extract_api_key(&req(&[("x-api-key", "   ")])), None);

        // 非空 x-api-key 仍优先于 Bearer（既有语义不变）
        let r = req(&[("x-api-key", "k1"), ("authorization", "Bearer k2")]);
        assert_eq!(extract_api_key(&r).as_deref(), Some("k1"));
    }

    /// Bearer 的 scheme 大小写不敏感、凭据两侧 OWS 容忍（RFC 9110 §11.1）。
    #[test]
    fn bearer_scheme_is_case_insensitive() {
        for v in [
            "Bearer tok",
            "bearer tok",
            "BEARER tok",
            "Bearer  tok",
            "Bearer tok ",
        ] {
            let r = req(&[("authorization", v)]);
            assert_eq!(extract_api_key(&r).as_deref(), Some("tok"), "头值 {v:?}");
        }

        // 空 token / 非 Bearer scheme / 缺分隔符 → 一律 None
        for v in ["Bearer ", "Bearer", "Basic tok", "tok"] {
            let r = req(&[("authorization", v)]);
            assert_eq!(extract_api_key(&r), None, "头值 {v:?} 不该产出 key");
        }
    }

    /// 常量时间比较的基本正确性（等价即真、不等即假、空串不特殊）。
    #[test]
    fn constant_time_eq_basics() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        // 空串与空串相等 —— 正是 auth_keys 必须靠「空存储恒 false」兜住的那条性质
        assert!(constant_time_eq("", ""));
    }
}
