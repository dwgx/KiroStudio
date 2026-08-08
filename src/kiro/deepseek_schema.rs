//! 通用 JSON Schema 修复 —— 薄包装复用 `anthropic::converter::normalize_json_schema`。
//!
//! deepseek 类上游对标准 JSON Schema 之外的非标准形态（`$ref`、`anyOf/oneOf/allOf`、
//! 多余键）常报 400。`converter.rs` 的 `normalize_json_schema` 是成熟实现（$ref 展开 +
//! 节点预算 + 白名单清洗 + 空 schema 补 type），本模块只把它接到 deepseek 归一化路径。
//!
//! ⚠️ 不要在本模块另写一份 schema 清洗——`converter.rs` 已处理 $ref 展开（本仓唯一一份），
//! 重复实现会与它漂移（$ref 降级、白名单键、节点预算不一致）。

use serde_json::Value;

/// 清洗 JSON Schema（就地修改）：调用 `converter::normalize_json_schema` 后写回。
///
/// 对任意 JSON 安全（幂等），不会 panic。非对象/数组结构由 converter 原样返回。
pub fn fix_schema(value: &mut Value) {
    let cleaned = crate::anthropic::converter::normalize_json_schema(value.take());
    *value = cleaned;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix(v: serde_json::Value) -> serde_json::Value {
        let mut x = v;
        fix_schema(&mut x);
        x
    }

    /// 标准 schema 保留七键。
    #[test]
    fn keeps_whitelist_keys() {
        let v = serde_json::json!({
            "type": "object",
            "properties": { "a": { "type": "string" } },
            "required": ["a"],
            "format": "json",
            "pattern": ".*",
            "x-custom": 1
        });
        let out = fix(v);
        let o = out.as_object().unwrap();
        assert!(o.contains_key("type"));
        assert!(o.contains_key("properties"));
        assert!(o.contains_key("required"));
        assert!(!o.contains_key("format"), "format 不在白名单，须剥");
        assert!(!o.contains_key("pattern"), "pattern 不在白名单，须剥");
        assert!(!o.contains_key("x-custom"), "自定义键须剥");
    }

    /// anyOf/oneOf/allOf 丢弃。
    #[test]
    fn drops_anyof_oneof_allof() {
        let v = serde_json::json!({
            "anyOf": [{ "type": "string" }, { "type": "number" }],
            "oneOf": [{ "type": "string" }],
            "allOf": [{ "type": "object" }],
            "type": "object"
        });
        let out = fix(v);
        let o = out.as_object().unwrap();
        assert!(!o.contains_key("anyOf"));
        assert!(!o.contains_key("oneOf"));
        assert!(!o.contains_key("allOf"));
        assert!(o.contains_key("type"));
    }

    /// $ref 无定义时由 converter 降级为 object（不 panic、不把 $ref 原样透传）。
    #[test]
    fn ref_downgrades_to_object() {
        let v = serde_json::json!({ "$ref": "#/components/schemas/Foo" });
        let out = fix(v);
        assert_eq!(out["type"], "object", "无 $defs 可展开时降级 object，实际: {out}");
    }

    /// 递归 properties 清洗。
    #[test]
    fn recurses_into_properties() {
        let v = serde_json::json!({
            "type": "object",
            "properties": {
                "a": { "type": "string", "format": "date-time" },
                "b": { "anyOf": [{ "type": "number" }] }
            }
        });
        let out = fix(v);
        let a = &out["properties"]["a"];
        assert!(!a.as_object().unwrap().contains_key("format"));
        let b = &out["properties"]["b"];
        assert!(!b.as_object().unwrap().contains_key("anyOf"));
    }

    /// 深 schema 不 panic（超深降级 object）。
    #[test]
    fn deep_schema_no_panic() {
        let mut v = serde_json::json!({ "type": "object", "properties": {} });
        let mut cur = v.as_object_mut().unwrap().get_mut("properties").unwrap().as_object_mut().unwrap();
        for i in 0..20 {
            let child = serde_json::json!({ "type": "object", "properties": {} });
            cur.insert(format!("k{i}"), child);
            cur = cur.get_mut(&format!("k{i}")).unwrap().as_object_mut().unwrap();
        }
        fix_schema(&mut v);
        // 不 panic 即可；深处降级 object。
    }

    /// 非对象（字符串）由 converter 规范化为 object（不 panic）。
    #[test]
    fn non_object_is_safe() {
        assert_eq!(fix(serde_json::json!("hello"))["type"], "object");
        assert_eq!(fix(serde_json::json!([1, 2]))["type"], "object");
    }
}
