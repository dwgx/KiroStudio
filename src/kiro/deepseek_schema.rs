//! 通用 JSON Schema 修复（移植 kiro2cc-proxy 的 `normalize_json_schema_inner`）。
//!
//! deepseek 类上游对标准 JSON Schema 之外的非标准形态（`$ref`、`anyOf/oneOf/allOf`、
//! 多余键）常报 400。本模块把 `tools[].input_schema` 清洗成 deepseek 能接受的白名单形态：
//!
//! - `$ref`：单 schema 上下文无法解析引用 → 降级为 `{"type":"object"}`（防循环引用死循环）。
//! - 只保留 `type/properties/required/items/additionalProperties/description/enum` 七键。
//! - `anyOf/oneOf/allOf` 直接丢弃（不在白名单）。

use serde_json::Value;

/// 允许保留的 JSON Schema 顶层键（白名单）。
const SCHEMA_KEEP_KEYS: &[&str] = &[
    "type",
    "properties",
    "required",
    "items",
    "additionalProperties",
    "description",
    "enum",
];

/// 递归清洗 JSON Schema：$ref 降级 + 白名单剥离（就地修改）。
///
/// 对任意 JSON 安全（幂等），不会 panic。非对象/数组结构原样保留。
pub fn fix_schema(value: &mut Value) {
    clean_schema(value, 0);
}

/// 递归清洗，`depth` 防深层/循环引用撑爆栈。
fn clean_schema(value: &mut Value, depth: usize) {
    // 深度上限：超过即降级为宽松 object（宁可少约束，不 panic）。
    if depth > 8 {
        *value = serde_json::json!({ "type": "object" });
        return;
    }
    match value {
        Value::Object(map) => {
            // $ref 无法在单 schema 上下文解析 → 降级 object，避免把引用原样透传给上游被拒。
            if map.contains_key("$ref") {
                *value = serde_json::json!({ "type": "object" });
                return;
            }
            // 白名单剥离：只留七键（anyOf/oneOf/allOf/format/pattern 等全部丢弃）。
            map.retain(|k, _| SCHEMA_KEEP_KEYS.contains(&k.as_str()));
            // 递归 properties 的各子 schema。
            if let Some(props) = map.get_mut("properties").and_then(|v| v.as_object_mut()) {
                for sub in props.values_mut() {
                    clean_schema(sub, depth + 1);
                }
            }
            // 递归 items（数组元素 schema）。
            if let Some(items) = map.get_mut("items") {
                clean_schema(items, depth + 1);
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                clean_schema(item, depth + 1);
            }
        }
        _ => {}
    }
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

    /// $ref 降级为 object。
    #[test]
    fn ref_downgrades_to_object() {
        let v = serde_json::json!({ "$ref": "#/components/schemas/Foo" });
        assert_eq!(fix(v), serde_json::json!({ "type": "object" }));
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

    /// 非对象（字符串/数组）安全。
    #[test]
    fn non_object_is_noop() {
        assert_eq!(fix(serde_json::json!("hello")), serde_json::json!("hello"));
        assert_eq!(fix(serde_json::json!([1, 2])), serde_json::json!([1, 2]));
    }
}
