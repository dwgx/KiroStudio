//! JSON Schema 规范化（`$ref` 展开 + 白名单字段）。
//!
//! 对外入口由 `converter.rs` 再导出，保持 `converter::normalize_json_schema` 路径。

use super::{schema_description_max_chars, truncate_chars};

/// 规范化 JSON Schema，修复 MCP 工具定义中常见的类型问题
///
/// Claude Code / MCP 工具定义偶尔会出现 `required: null`、`properties: null` 等，
/// 导致上游返回 400 "Improperly formed request"。
/// 规范化工具的 JSON Schema，使其符合 Kiro 上游能接受的形式。
///
/// 关键改进（对齐参考实现 TsinHzl/kiro2cc-proxy，MIT）：**先递归展开 `$ref`**
/// 再规范化。Kiro 不认 `$ref`，未展开会让 MCP / pydantic / zod 生成的工具参数
/// 约束（属性用 `$ref` 指向 `$defs`）静默退化为无约束空对象，模型看不到真实参数
/// 结构。展开后再逐层规范化 type/properties/required/items/additionalProperties，
/// 丢弃 Kiro 兼容性差的 anyOf/oneOf/allOf，只保留白名单字段。
pub(crate) fn normalize_json_schema(schema: serde_json::Value) -> serde_json::Value {
    normalize_json_schema_with_node_budget(schema, MAX_SCHEMA_NODES)
}

/// `normalize_json_schema` 的可注入预算版本：只为让测试用**小预算 + 小 schema**
/// 验证预算机制本身（用真实的 5 万预算去测就得先展开 5 万节点，测试自身变成压力测试）。
/// 生产路径固定走 `MAX_SCHEMA_NODES`。
pub(super) fn normalize_json_schema_with_node_budget(
    schema: serde_json::Value,
    max_nodes: usize,
) -> serde_json::Value {
    // 先就地展开 $ref（依赖 $defs/definitions），再规范化。总是运行 resolve：即便没有
    // $defs，也需把无法展开的 $ref（OpenAPI/外部形式）显式降级为宽松 object，
    // 否则会被后续 retain 白名单清成空壳。
    let defs = extract_schema_defs(&schema);
    let mut budget = SchemaRefBudget::new(max_nodes);
    let resolved = resolve_schema_refs(schema, &defs, 0, &mut budget);
    // 降级必须留痕：否则线上被 $ref 炸弹打到（或某个 MCP server 发了异常巨大的 schema）时，
    // 现象只是"模型看到的参数约束莫名变宽松"，没有任何线索能定位到这里。
    if budget.truncated_nodes > 0 {
        tracing::warn!(
            nodes_visited = budget.visited,
            max_schema_nodes = budget.max_nodes,
            truncated_nodes = budget.truncated_nodes,
            "工具 JSON Schema 的 $ref 展开触达节点预算上限，超限子树已降级为宽松 object（疑似 $ref 放大攻击或异常巨大的 schema）"
        );
    }
    normalize_json_schema_inner(resolved, true)
}

/// 提取顶层 `$defs` / `definitions` 作为 `$ref` 解析表。
pub(super) fn extract_schema_defs(schema: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    let mut defs = serde_json::Map::new();
    if let Some(obj) = schema.as_object() {
        for key in ["$defs", "definitions"] {
            if let Some(serde_json::Value::Object(m)) = obj.get(key) {
                for (k, v) in m {
                    defs.insert(k.clone(), v.clone());
                }
            }
        }
    }
    defs
}

/// 单次 schema 展开允许访问的**节点总数**上限（整次展开共享一个预算，不是每层各自计数）。
///
/// 怎么定的（数都是实测的，别凭感觉调）：
/// - **合法侧**：一个 25KB / 120 个属性、每属性再嵌 object+array 的"大 schema"整棵树只
///   访问 **1803** 个节点。真实 MCP / pydantic / zod 工具 schema 都在 O(10^3) 量级。
///   5 万 ≈ 合法最坏情形的 **28 倍**冗余 ⇒ 正常请求不可能被截断。
/// - **攻击侧**：不设总量预算时，**159 字节**的自引用 `$ref` 输入就能展开出 **800 万+**
///   节点（b=2 时；b=3/b=4 更快），而这是跑在 tokio worker 上的同步 CPU 展开 ⇒ 单个请求
///   即可钉死一个 worker 数秒并把内存顶爆。
/// - **上界代价**：5 万个 `serde_json` 节点的克隆+插入是个位数毫秒级，同步跑在 async
///   runtime 上可接受。
pub(super) const MAX_SCHEMA_NODES: usize = 50_000;

/// `$ref` 展开的全局节点预算 + 降级痕迹。
///
/// 存在的理由：`MAX_REF_DEPTH` 限的是**引用链有多长**，而 `depth` 只在 `$ref` 跳转时 +1、
/// 同级递归复用同一个 depth ⇒ 一个 `$defs` 条目里放 b 个指回自己的兄弟属性，展开量就是
/// b^MAX_REF_DEPTH，**链长限制对扇出爆炸完全无效**。所以必须再有一道按**节点总数**算的闸门。
///
/// ⚠️ 不要把这道闸门"简化"成同级递归也 `depth + 1`：那会把正常大 schema 的同级字段数
/// 算进链长，合法请求会被误杀。两道闸门是互补的，都要留着。
pub(super) struct SchemaRefBudget {
    /// 本次展开的节点上限（生产恒为 `MAX_SCHEMA_NODES`，测试可注入小值）。
    pub(super) max_nodes: usize,
    /// 已访问节点数（整棵树累计）。
    pub(super) visited: usize,
    /// 因预算耗尽而被降级掉的节点数（>0 即本次发生了截断，供日志取证）。
    pub(super) truncated_nodes: usize,
}

impl SchemaRefBudget {
    pub(super) fn new(max_nodes: usize) -> Self {
        Self {
            max_nodes,
            visited: 0,
            truncated_nodes: 0,
        }
    }
}

/// 无法展开 / 触达闸门时的降级占位 schema。
///
/// 语义选择：**宽松 object 而不是删掉该节点**。删节点会让父级的 `required` 指向不存在的
/// 属性，上游直接回 400 "Improperly formed request"（整个工具列表连坐失效）；宽松 object
/// 只是让模型在这一处看不到细粒度约束，工具仍可用。两处闸门（链长/总量）与"$ref 目标缺失"
/// 共用同一语义，避免降级形态各写一份再各自漂移。
fn degraded_object_schema() -> serde_json::Value {
    serde_json::json!({ "type": "object", "additionalProperties": true })
}

/// 深度优先展开所有 `$ref`（支持 `#/$defs/<name>` 与 `#/definitions/<name>`）。
///
/// 两道**互补**的闸门：
/// - `depth`（仅在 `$ref` 跳转时递增）限引用**链长**，超限视为循环引用。
/// - `budget` 限整次展开的**节点总数**，防同级扇出把 159 字节输入放大成百万节点。
///
/// 任一闸门触发都降级为宽松 object 兜底（见 `degraded_object_schema`）。
pub(super) fn resolve_schema_refs(
    value: serde_json::Value,
    defs: &serde_json::Map<String, serde_json::Value>,
    depth: usize,
    budget: &mut SchemaRefBudget,
) -> serde_json::Value {
    const MAX_REF_DEPTH: usize = 16;
    if depth > MAX_REF_DEPTH {
        return degraded_object_schema();
    }
    // 🔴 **数组分支必须排在预算闸门之前**：数组容器自身不消耗预算、也不被替换，
    // 只有它的对象元素消耗。
    //
    // 为什么承重：降级产物 `degraded_object_schema()` 是一个 **object**。若预算在一个
    // `Value::Array` 节点上耗尽，那个数组会被整体换成对象 —— 而 JSON Schema 里
    // `anyOf` / `oneOf` / `allOf` / 元组式 `items` **必须是数组**，换成对象即产出
    // 结构非法的 schema，上游会 400。而本预算存在的全部目的就是避免上游报错，
    // 那就自相矛盾了。
    //
    // 这样耗尽时数组仍是**合法数组**（元素各自退化为 object 占位），结构不变形。
    // 判据取自参考实现 `WindsurfAPI/src/handlers/tool-emulation.js`（MIT）的
    // `stripSchemaDocs`：`if (Array.isArray(schema)) return schema.map(...)` 排在
    // `if (budget.remaining <= 0)` 之前，注释原话是 "keeps `anyOf`/tuple `items`
    // a valid ARRAY under exhaustion instead of being replaced wholesale by an
    // object placeholder"。
    //
    // ⚠️ 全树共享语义不变：`&mut budget` 照样贯穿数组元素，元素仍逐个计数。
    // 改的只是「数组这个容器节点自己不计数、也不被替换」。
    if let serde_json::Value::Array(arr) = value {
        return serde_json::Value::Array(
            arr.into_iter()
                .map(|v| resolve_schema_refs(v, defs, depth, budget))
                .collect(),
        );
    }
    if budget.visited >= budget.max_nodes {
        budget.truncated_nodes += 1;
        return degraded_object_schema();
    }
    budget.visited += 1;
    match value {
        serde_json::Value::Object(mut obj) => {
            if let Some(serde_json::Value::String(ref_str)) = obj.get("$ref") {
                let ref_str = ref_str.clone();
                let name = ref_str
                    .strip_prefix("#/$defs/")
                    .or_else(|| ref_str.strip_prefix("#/definitions/"))
                    .map(str::to_string);
                obj.remove("$ref");
                match name.as_ref().and_then(|n| defs.get(n)) {
                    Some(target) => {
                        // 展开目标后并入同级字段（不覆盖 $ref 旁已有的 description 等）。
                        let resolved = resolve_schema_refs(target.clone(), defs, depth + 1, budget);
                        if let serde_json::Value::Object(robj) = resolved {
                            for (k, v) in robj {
                                obj.entry(k).or_insert(v);
                            }
                        }
                    }
                    None => {
                        // 未命中（OpenAPI #/components、外部 URL、目标缺失）：无法展开，
                        // 显式降级为宽松 object 而非留空壳，并记日志便于排查约束丢失。
                        tracing::debug!(
                            "$ref 无法展开（非 #/$defs 形式或目标缺失），降级为宽松 object: {}",
                            ref_str
                        );
                        obj.entry("type".to_string())
                            .or_insert(serde_json::Value::String("object".to_string()));
                    }
                }
            }
            let mut new_obj = serde_json::Map::new();
            for (k, v) in obj {
                new_obj.insert(k, resolve_schema_refs(v, defs, depth, budget));
            }
            serde_json::Value::Object(new_obj)
        }
        // 数组已在预算闸门**之前**提前返回（见函数开头那段），此处恒不可达。
        // 保留一条显式分支而不是让它落 `other => other`：若将来有人把前面那个提前返回
        // 删掉（那正是本文件修过的缺陷），数组会落到这里继续正确递归，而不是被
        // `other => other` 原样返回、内部 `$ref` 一个都不展开。即这是**降级兜底**，
        // 不是重复实现。
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.into_iter()
                .map(|v| resolve_schema_refs(v, defs, depth, budget))
                .collect(),
        ),
        other => other,
    }
}

/// 递归规范化（`$ref` 已展开后调用）。`root=true` 时强制视为 object schema。
fn normalize_json_schema_inner(schema: serde_json::Value, root: bool) -> serde_json::Value {
    let serde_json::Value::Object(mut obj) = schema else {
        return serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": true
        });
    };

    // 去掉 null 字段；Kiro 侧对 null 容忍度很低。
    obj.retain(|_, v| !v.is_null());

    // type（字符串；数组类型如 ["string","null"] 取第一个基础类型）
    let normalized_type = match obj.remove("type") {
        Some(serde_json::Value::String(s)) => normalize_schema_type(&s),
        Some(serde_json::Value::Array(arr)) => arr
            .into_iter()
            .filter_map(|v| v.as_str().and_then(normalize_schema_type))
            .next(),
        _ => None,
    };
    let is_object_schema = root
        || normalized_type.as_deref() == Some("object")
        || (normalized_type.is_none() && obj.contains_key("properties"));

    if is_object_schema {
        obj.insert(
            "type".to_string(),
            serde_json::Value::String("object".to_string()),
        );
    } else if let Some(t) = normalized_type {
        obj.insert("type".to_string(), serde_json::Value::String(t));
    }

    if is_object_schema {
        match obj.remove("properties") {
            Some(serde_json::Value::Object(props)) => {
                let mut normalized = serde_json::Map::new();
                for (name, prop_schema) in props {
                    normalized.insert(name, normalize_json_schema_inner(prop_schema, false));
                }
                obj.insert(
                    "properties".to_string(),
                    serde_json::Value::Object(normalized),
                );
            }
            _ => {
                obj.insert(
                    "properties".to_string(),
                    serde_json::Value::Object(serde_json::Map::new()),
                );
            }
        }
        let required = match obj.remove("required") {
            Some(serde_json::Value::Array(arr)) => serde_json::Value::Array(
                arr.into_iter()
                    .filter_map(|v| v.as_str().map(|s| serde_json::Value::String(s.to_string())))
                    .collect(),
            ),
            _ => serde_json::Value::Array(Vec::new()),
        };
        obj.insert("required".to_string(), required);
    } else {
        obj.remove("properties");
        obj.remove("required");
    }

    // items（对象或数组形式取第一个 schema）
    if let Some(items) = obj.remove("items") {
        let normalized_items = match items {
            serde_json::Value::Array(arr) => arr
                .into_iter()
                .find(|v| v.is_object())
                .map(|v| normalize_json_schema_inner(v, false)),
            serde_json::Value::Object(_) => Some(normalize_json_schema_inner(items, false)),
            _ => None,
        };
        if let Some(items) = normalized_items {
            obj.insert("items".to_string(), items);
        }
    }

    // Kiro 对组合 schema 兼容差：anyOf/oneOf/allOf 直接丢弃，避免整个工具列表被判 malformed。
    obj.remove("anyOf");
    obj.remove("oneOf");
    obj.remove("allOf");

    // additionalProperties（bool 或 object；其余按 true）
    match obj.remove("additionalProperties") {
        Some(serde_json::Value::Object(schema)) => {
            obj.insert(
                "additionalProperties".to_string(),
                normalize_json_schema_inner(serde_json::Value::Object(schema), false),
            );
        }
        Some(serde_json::Value::Bool(value)) => {
            obj.insert(
                "additionalProperties".to_string(),
                serde_json::Value::Bool(value),
            );
        }
        Some(_) => {
            obj.insert(
                "additionalProperties".to_string(),
                serde_json::Value::Bool(true),
            );
        }
        None => {}
    }

    // schema 内嵌 description 截断（默认 2000 字符 = 顶层上限的 1/5，可配置，按字符边界防多字节切断）
    if let Some(description) = obj.remove("description")
        && let Some(description) = description.as_str()
    {
        let description = truncate_chars(description, schema_description_max_chars());
        obj.insert(
            "description".to_string(),
            serde_json::Value::String(description),
        );
    }

    // enum 只保留 string/number/bool 值
    if let Some(enum_value) = obj.remove("enum")
        && let serde_json::Value::Array(values) = enum_value
    {
        let values: Vec<_> = values
            .into_iter()
            .filter(|v| v.is_string() || v.is_number() || v.is_boolean())
            .collect();
        if !values.is_empty() {
            obj.insert("enum".to_string(), serde_json::Value::Array(values));
        }
    }

    // 白名单：只保留 Kiro 认识的字段
    obj.retain(|key, _| {
        matches!(
            key.as_str(),
            "type"
                | "properties"
                | "required"
                | "items"
                | "additionalProperties"
                | "description"
                | "enum"
        )
    });

    serde_json::Value::Object(obj)
}

/// 规范化 type 字符串：只认 JSON Schema 的 6 种基础类型，其余返回 None。
fn normalize_schema_type(raw: &str) -> Option<String> {
    match raw.trim() {
        "object" | "array" | "string" | "number" | "integer" | "boolean" => {
            Some(raw.trim().to_string())
        }
        _ => None,
    }
}
