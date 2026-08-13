//! 全局模型映射 —— 把客户端请求的模型名改写为上游实际下发的模型名。
//!
//! 背景：用户的代理链路里上游（如 sub2api 的中转站）可能只认一组固定的模型名，
//! 或者想"客户端写 claude-haiku-4-5、上游按 claude-sonnet-4-5 计费/调度"。
//! 与 deepseek 归一化（`deepseek_normalize`，opencodezen 专用 fallback）不同，
//! 这里是**全局、显式、用户可配置**的映射规则，对 Kiro 主路径与 custom_api 透传
//! 路径都生效。
//!
//! 设计决定（用户 2026-08-09 拍板，勿改）：
//! - **全局映射 + 每凭据豁免**：`Config.model_mapping` 是全局规则；凭据设
//!   `model_mapping_exempt=true` 时完全跳过全局映射（安全阀，覆盖"该号上游不收
//!   映射后名"的场景）。
//! - **映射后不再判白名单**：`allowed_models` 只作为**选号门**（允许哪些**原始**
//!   模型走这条号）；映射发生在选中之后、发上游之前，改写成什么不再过白名单
//!   （生态主流：sub2api 真实映射的目标名往往就在白名单外）。
//! - **顺序：先映射 → 再 deepseek 归一化**。反序会让 deepseek 先把名字压成
//!   fallback，映射规则再也匹配不到原始名。
//!
//! ⚠️ 一个**残留不对称（设计明确接受、非 bug）**：`select_custom_api`
//!   （token_manager.rs:2939-2954）为门序对齐会预判 deepseek 改写后的名来判白名单，
//!   但映射**不进**这个预判（决定 3：选号门只看原始名）。因此「映射后名上游不认」
//!   在透传池仍可能选中该号 → 上游 400。这正是每凭据豁免要覆盖的场景。

use std::collections::HashMap;

/// 把原始模型名按全局规则映射成上游名。
///
/// - 规则命中且映射目标 **!= 原始名**（大小写不敏感比较）才返回 `Some(target)`，
///   否则返回 `None` —— 命中但同名（如 `claude-... → claude-...` 大小写不同）不触发改写，
///   避免无意义的 `rewrite_model_id` 全量解析开销。
/// - 大小写不敏感匹配 key（`eq_ignore_ascii_case`），与白名单判定
///   （`allows_model` 的 `eq_ignore_ascii_case`）同口径，避免 `claude-opus-4.8` /
///   `Claude-Opus-4.8` 命中不一致。
/// - **单跳映射**：只查一层，不做链式（`A→B` 且 `B→C` 时只把 A 改写为 B，
///   不再把 B 递归改写为 C）。理由：链式依赖 HashMap 迭代顺序产生不确定行为，
///   且与 deepseek 归一化、overload_fallback_model 叠加后行为不可预测。
///   `map_target` 只匹配一次，天然单跳。
/// - 空规则表返回 `None`（零开销，调用方可短路）。
pub fn map_target(model: &str, rules: &HashMap<String, String>) -> Option<String> {
    if rules.is_empty() {
        return None;
    }
    // 与 `allows_model` 同款大小写不敏感匹配：小写 key 一次查完，无迭代。
    let target = rules
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(model))
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
}
