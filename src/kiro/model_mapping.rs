//! 全局模型映射 —— 把客户端请求的模型名改写为上游实际下发的模型名。
//!
//! 背景：用户的代理链路里上游（如 sub2api 的中转站）可能只认一组固定的模型名，
//! 或者想"客户端写 claude-haiku-4-5、上游按 claude-sonnet-4-5 计费/调度"。
//! 这是**全局、显式、用户可配置**的映射规则，对 Kiro 主路径与 custom_api 透传
//! 路径都生效（deepseek 协议归一化已于 2026-08-16 移除，映射是透传路径唯一的
//! 模型改写）。
//!
//! 设计决定（用户 2026-08-09 拍板，勿改）：
//! - **全局映射 + 每凭据豁免**：`Config.model_mapping` 是全局规则；凭据设
//!   `model_mapping_exempt=true` 时完全跳过全局映射（安全阀，覆盖"该号上游不收
//!   映射后名"的场景）。
//! - **映射后不再判白名单**：`allowed_models` 只作为**选号门**（允许哪些**原始**
//!   模型走这条号）；映射发生在选中之后、发上游之前，改写成什么不再过白名单
//!   （生态主流：sub2api 真实映射的目标名往往就在白名单外）。
//!
//! ⚠️ 一个**残留不对称（设计明确接受、非 bug）**：`select_custom_api`
//! （token_manager.rs）选号时白名单按**原始模型名**判定（2026-08-16 起，归一化
//! 移除后判定键 = 发送键），映射**不进**这个预判（决定 3：选号门只看原始名）。
//! 因此「映射后名上游不认」在透传池仍可能选中该号 → 上游 400。这正是每凭据
//! 豁免要覆盖的场景。

use std::collections::HashMap;

/// 通配符匹配：仅支持**末尾 `*`** 前缀通配（sub2api `matchWildcard` 同款语义，
/// 不做中间/开头通配）。
///
/// - 以 `*` 结尾 → 前缀匹配（大小写不敏感）；`*` 单独 = 空前缀 = 匹配全部。
/// - 无 `*` → 精确匹配（`eq_ignore_ascii_case`）。
/// - 模型名与 pattern 前缀的字节切片用 `get` 拿，边界不对齐时返回 `false` 而非 panic。
pub(crate) fn wildcard_matches(pattern: &str, model: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => model
            .get(..prefix.len())
            .is_some_and(|head| prefix.eq_ignore_ascii_case(head)),
        None => pattern.eq_ignore_ascii_case(model),
    }
}

/// 把原始模型名按全局规则映射成上游名。
///
/// - 规则命中且映射目标 **!= 原始名**（大小写不敏感比较）才返回 `Some(target)`，
///   否则返回 `None` —— 命中但同名（如 `claude-... → claude-...` 大小写不同）不触发改写，
///   避免无意义的 `rewrite_model_id` 全量解析开销。
/// - 匹配 key 支持**末尾 `*` 通配**（`claude-*` 匹配所有 claude- 开头模型，大小写不敏感），
///   与白名单判定共用 [`wildcard_matches`]，避免 `claude-opus-4.8` / `Claude-Opus-4.8`
///   命中不一致。
/// - **多 pattern 命中取最长**（sub2api `matchWildcardMappingResult` 语义）：按 pattern
///   长度降序选最长者；长度打平时精确（无 `*`）优先于通配，再按 key 字典序兜底保证
///   确定性（HashMap 迭代序不稳定，同长通配双命中虽实际不可能，防御起见仍要稳定）。
/// - **单跳映射**：只查一层，不做链式（`A→B` 且 `B→C` 时只把 A 改写为 B，
///   不再把 B 递归改写为 C）。理由：链式依赖 HashMap 迭代顺序产生不确定行为，
///   且与 deepseek 归一化、overload_fallback_model 叠加后行为不可预测。
///   `map_target` 只匹配一次，天然单跳。
/// - 空规则表返回 `None`（零开销，调用方可短路）。
pub fn map_target(model: &str, rules: &HashMap<String, String>) -> Option<String> {
    if rules.is_empty() {
        return None;
    }
    // 收集所有命中（精确 + 通配），按上述排序规则取最优者。
    let target = rules
        .iter()
        .filter(|(k, _)| wildcard_matches(k, model))
        .max_by(|(ka, _), (kb, _)| {
            ka.len()
                .cmp(&kb.len())
                .then_with(|| ka.contains('*').cmp(&kb.contains('*')).reverse())
                .then_with(|| kb.cmp(ka))
        })
        .map(|(_, v)| v.clone())?;
    // 命中但目标与原模型名相同（大小写不敏感）→ 视为未命中，避免无意义改写。
    if target.eq_ignore_ascii_case(model) {
        return None;
    }
    Some(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn test_map_target_basic_hit() {
        let r = rules(&[("claude-haiku-4-5", "claude-sonnet-4-5")]);
        assert_eq!(
            map_target("claude-haiku-4-5", &r),
            Some("claude-sonnet-4-5".to_string())
        );
    }

    #[test]
    fn test_map_target_case_insensitive_key() {
        let r = rules(&[("claude-haiku-4-5", "claude-sonnet-4-5")]);
        // 客户端传的大小写不同也应命中（与白名单 `allows_model` 同口径）。
        assert_eq!(
            map_target("Claude-Haiku-4-5", &r),
            Some("claude-sonnet-4-5".to_string())
        );
    }

    #[test]
    fn test_map_target_identity_returns_none() {
        // 命中但目标与原始名相同（大小写不敏感）→ None，不触发无意义改写。
        let r = rules(&[("claude-haiku-4-5", "CLAUDE-HAIKU-4-5")]);
        assert_eq!(map_target("claude-haiku-4-5", &r), None);
    }

    #[test]
    fn test_map_target_no_hit_returns_none() {
        let r = rules(&[("claude-haiku-4-5", "claude-sonnet-4-5")]);
        assert_eq!(map_target("claude-opus-4-8", &r), None);
    }

    #[test]
    fn test_map_target_empty_rules_returns_none() {
        let r = rules(&[]);
        assert_eq!(map_target("claude-opus-4-8", &r), None);
    }

    #[test]
    fn test_map_target_is_single_hop_not_chain() {
        // A→B 且 B→C：只改写为 B（单跳），不会递归改成 C。
        let r = rules(&[("a", "b"), ("b", "c")]);
        assert_eq!(map_target("a", &r), Some("b".to_string()));
    }

    #[test]
    fn test_map_target_multiple_keys_distinct_targets() {
        let r = rules(&[
            ("claude-haiku-4-5", "claude-sonnet-4-5"),
            ("claude-opus-4-8", "claude-sonnet-4-5"),
        ]);
        assert_eq!(
            map_target("claude-opus-4-8", &r),
            Some("claude-sonnet-4-5".to_string())
        );
    }

    #[test]
    fn test_map_target_wildcard_prefix_match() {
        // 末尾 * 前缀通配：claude-* 匹配所有 claude- 开头模型。
        let r = rules(&[("claude-*", "claude-sonnet-4.5")]);
        assert_eq!(
            map_target("claude-opus-5", &r),
            Some("claude-sonnet-4.5".to_string())
        );
        assert_eq!(
            map_target("claude-haiku-4-5", &r),
            Some("claude-sonnet-4.5".to_string())
        );
    }

    #[test]
    fn test_map_target_exact_beats_wildcard() {
        // 精确规则与通配同时命中时，精确优先（其 pattern 天然更长）。
        let r = rules(&[
            ("claude-*", "claude-sonnet-4.5"),
            ("claude-opus-5", "claude-opus-4.8"),
        ]);
        assert_eq!(
            map_target("claude-opus-5", &r),
            Some("claude-opus-4.8".to_string())
        );
    }

    #[test]
    fn test_map_target_wildcard_longest_pattern_wins() {
        // 多个通配命中 → 取 pattern 最长者（sub2api 语义）。
        let r = rules(&[
            ("claude-*", "claude-sonnet-4.5"),
            ("claude-opus-*", "claude-opus-4.8"),
        ]);
        assert_eq!(
            map_target("claude-opus-5", &r),
            Some("claude-opus-4.8".to_string())
        );
        // 边界：带尾 * 的 pattern 比精确名还长时，最长优先（与 sub2api 一致）。
        let r2 = rules(&[
            ("claude-opus-5", "claude-opus-4.8"),
            ("claude-opus-5*", "claude-sonnet-4.5"),
        ]);
        assert_eq!(
            map_target("claude-opus-5", &r2),
            Some("claude-sonnet-4.5".to_string())
        );
    }

    #[test]
    fn test_map_target_wildcard_case_insensitive() {
        // 通配匹配与精确匹配同口径：大小写不敏感。
        let r = rules(&[("Claude-*", "claude-sonnet-4.5")]);
        assert_eq!(
            map_target("claude-opus-5", &r),
            Some("claude-sonnet-4.5".to_string())
        );
        assert_eq!(
            map_target("CLAUDE-OPUS-5", &r),
            Some("claude-sonnet-4.5".to_string())
        );
    }

    #[test]
    fn test_map_target_bare_star_matches_all() {
        // * 单独 = 匹配全部（空前缀）。
        let r = rules(&[("*", "claude-sonnet-4.5")]);
        assert_eq!(
            map_target("deepseek-v4-flash", &r),
            Some("claude-sonnet-4.5".to_string())
        );
    }

    #[test]
    fn test_map_target_bare_star_tie_break_prefers_exact() {
        // *（长 1）与单字符精确 key 同长命中时，精确优先于通配。
        let r = rules(&[("*", "fallback"), ("x", "x-model")]);
        assert_eq!(map_target("x", &r), Some("x-model".to_string()));
    }

    #[test]
    fn test_map_target_wildcard_no_hit_returns_none() {
        let r = rules(&[("deepseek-*", "claude-sonnet-4.5")]);
        assert_eq!(map_target("claude-opus-5", &r), None);
        // 通配只做前缀：非 claude- 开头的不被 claude-* 命中。
        let r2 = rules(&[("claude-*", "claude-sonnet-4.5")]);
        assert_eq!(map_target("my-claude-opus-5", &r2), None);
    }

    #[test]
    fn test_map_target_wildcard_identity_returns_none() {
        // 通配命中但目标与原模型名相同（大小写不敏感）→ None，不触发无意义改写。
        let r = rules(&[("claude-*", "CLAUDE-OPUS-5")]);
        assert_eq!(map_target("claude-opus-5", &r), None);
    }

    #[test]
    fn test_map_target_bare_star_maps_empty_model() {
        // F5（对抗审查 2026-08-15）：`*` 全通配规则会命中空模型名并改写为 target。
        // 与 forward/predict 两侧一致（无漂移），显式钉住行为防未来空名短路改动。
        let r = rules(&[("*", "deepseek-v4-flash")]);
        assert_eq!(map_target("", &r), Some("deepseek-v4-flash".to_string()));
        // 无 `*` 规则的精确匹配对空名行为不变（空 key 精确命中空名——与原有语义一致）。
        let r2 = rules(&[("", "x")]);
        assert_eq!(map_target("", &r2), Some("x".to_string()));
    }

    #[test]
    fn test_wildcard_matches_semantics() {
        // 共享匹配函数的语义边界（allows_model 与 map_target 共用）。
        assert!(wildcard_matches("claude-*", "claude-opus-5"));
        assert!(wildcard_matches("claude-*", "Claude-Opus-5"));
        assert!(!wildcard_matches("claude-*", "x-claude-opus-5"));
        assert!(wildcard_matches("*", "anything"));
        assert!(wildcard_matches("deepseek-v4-flash", "deepseek-v4-flash"));
        assert!(wildcard_matches("DeepSeek-V4-Flash", "deepseek-v4-flash"));
        assert!(!wildcard_matches("deepseek-*", "claude-opus-5"));
        // 非末尾 *（如 claude-*-5）不是通配 → 精确匹配恒不命中。
        assert!(!wildcard_matches("claude-*-5", "claude-opus-5"));
        assert!(!wildcard_matches("**", "anything"));
        // 字节边界安全：prefix 超长返回 false 不 panic。
        assert!(!wildcard_matches("claude-opus-5-longer*", "claude-opus-5"));
    }
}
