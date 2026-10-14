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
///
/// **根级 union 是剥除的例外**（2026-10-14）：Kiro 顶层直接拒绝 union
/// （"does not support oneOf, allOf, or anyOf at the top level"），而整体剥掉会把分支里的
/// `properties` 一起丢、工具退化成**零参工具**。所以根级 `oneOf`/`anyOf`/`allOf` 先由
/// `flatten_root_union` 扁平化合并进根对象；非根层维持原有剥除（Kiro 只拒顶层）。
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

/// 根级 union（`oneOf`/`anyOf`/`allOf`）扁平化合并成一个 object schema。
///
/// 为什么必须合并（2026-10-14，独立扫描 + 代码核对）：Kiro 上游**顶层**直接拒绝 union。
/// 此前的处置是逐层 `obj.remove(...)` 整体剥掉，但根级 union 的 `properties` 全在分支里
/// —— 剥掉后根 schema 只剩强制补出的 `properties: {}` / `required: []`，工具退化成
/// **零参工具**：模型完全不知道有哪些参数可传（比"约束变宽松"严重得多，参数直接消失）。
/// 参考实现（diegosouzapw/OmniRoute）的做法是把根级 union **扁平化合并**进根对象，本函数
/// 对齐该做法。非根层维持原有剥除（Kiro 只拒顶层，子层剥除是既有策略，不在本次范围）。
///
/// 合并语义（每一条都取保守方向：宁可约束偏松，不误杀合法调用、不产非法 schema）：
/// - `properties`：根自身 → oneOf/anyOf 分支 → allOf 分支依次取并集，属性**名字不丢**；
///   同名冲突保留**先出现**的定义并记 debug —— 分支间对同名属性的定义可能互斥
///   （string vs number），合并子 schema 需要再引一层 union，而那正是 Kiro 顶层拒绝的
///   东西；保留第一个是可预测、可解释的取舍。
/// - `required`：oneOf/anyOf 取各分支**交集**（不强制"只有部分分支才需要"的字段，否则
///   模型可能因无从填写的必填项而编造/放弃参数）；allOf 取**并集**（所有分支同时成立，
///   任一分支要求的字段对整体就是必填，取交集会漏掉真正的必填项）。根自身已有的
///   required 无条件保留（根层约束恒成立）。
/// - `additionalProperties`：仅当**所有**分支都显式 `false` 时才收紧为 `false`。理由：
///   合并后的 properties 是各分支属性的并集，只要有一个分支没声明 false，实例就可能
///   合法携带合并集之外的属性，此时设 false 会误杀合法输入；全 false 时收紧是安全的
///   （每个分支允许的属性都在合并集内，实例无论走哪个分支都满足合并 schema）。根自身
///   声明过则一律以根为准（不覆盖）。
/// - `type`：不动 —— 根 schema 随后会被强制成 object（既有行为）。
///
/// 只合并**一层**：分支自身再嵌套的 union 按非根层既有行为剥除（与改动前一致）。
fn flatten_root_union(obj: &mut serde_json::Map<String, serde_json::Value>) {
    // 摘除并收集分支。remove 无条件执行：值不是数组（畸形 schema）时也要把键摘掉 ——
    // 顶层残留 union 键会被 Kiro 拒绝。
    let mut one_of_any_of_branches: Vec<serde_json::Value> = Vec::new();
    let mut all_of_branches: Vec<serde_json::Value> = Vec::new();
    for key in ["oneOf", "anyOf", "allOf"] {
        let Some(serde_json::Value::Array(arr)) = obj.remove(key) else {
            continue;
        };
        if key == "allOf" {
            all_of_branches = arr;
        } else {
            one_of_any_of_branches.extend(arr);
        }
    }
    if one_of_any_of_branches.is_empty() && all_of_branches.is_empty() {
        // 无根级 union：什么都不动（无 union 的 schema 逐字节不变）。
        return;
    }

    // properties：并集，同名保留第一个（根自身 > oneOf/anyOf 分支 > allOf 分支）。
    let mut merged_properties = match obj.remove("properties") {
        Some(serde_json::Value::Object(props)) => props,
        _ => serde_json::Map::new(),
    };
    for branch in one_of_any_of_branches.iter().chain(all_of_branches.iter()) {
        let Some(serde_json::Value::Object(props)) = branch.get("properties") else {
            continue;
        };
        for (name, prop_schema) in props {
            if merged_properties.contains_key(name) {
                tracing::debug!(
                    property = %name,
                    "根级 union 扁平化：属性在多个分支重复声明，保留先出现的定义（丢弃后出现的）"
                );
                continue;
            }
            merged_properties.insert(name.clone(), prop_schema.clone());
        }
    }
    obj.insert(
        "properties".to_string(),
        serde_json::Value::Object(merged_properties),
    );

    // required：根自身先入；oneOf/anyOf 取交集、allOf 取并集（理由见函数文档）。
    let mut required = required_field_names(obj.get("required"));
    let mut common: Option<Vec<String>> = None;
    for branch in &one_of_any_of_branches {
        let names = required_field_names(branch.get("required"));
        common = Some(match common {
            None => names,
            Some(prev) => prev.into_iter().filter(|n| names.contains(n)).collect(),
        });
    }
    if let Some(common) = common {
        for name in common {
            if !required.contains(&name) {
                required.push(name);
            }
        }
    }
    for branch in &all_of_branches {
        for name in required_field_names(branch.get("required")) {
            if !required.contains(&name) {
                required.push(name);
            }
        }
    }
    obj.insert(
        "required".to_string(),
        serde_json::Value::Array(
            required
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ),
    );

    // additionalProperties：所有分支都显式 false 才收紧；根自身声明过则以根为准。
    if !obj.contains_key("additionalProperties")
        && one_of_any_of_branches
            .iter()
            .chain(all_of_branches.iter())
            .all(|branch| {
                branch.get("additionalProperties") == Some(&serde_json::Value::Bool(false))
            })
    {
        obj.insert(
            "additionalProperties".to_string(),
            serde_json::Value::Bool(false),
        );
    }
}

/// 从 schema 的 `required` 读字段名列表：只取数组里的字符串项，缺失/非数组 → 空。
/// 供 `flatten_root_union` 做交集/并集 —— 那里的输入都是**未归一化**的分支 schema，
/// 不能假设形状规范（非字符串项、非数组值都要能吞掉）。
fn required_field_names(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
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

    // 根级 union 先扁平化合并（Kiro 顶层拒绝 union；整体剥掉会让工具退化成零参工具，
    // 见 flatten_root_union 的注释）。非根层维持原有剥除行为，不动。
    if root {
        flatten_root_union(&mut obj);
    }

    // type（字符串；数组类型如 ["string","null"] 取第一个基础类型）
    let normalized_type = match obj.remove("type") {
        Some(serde_json::Value::String(s)) => normalize_schema_type(&s),
        Some(serde_json::Value::Array(arr)) => arr
            .into_iter()
            .filter_map(|v| v.as_str().and_then(normalize_schema_type))
            .next(),
        _ => None,
    };
    // items 合法性预判（下面的 items 块用）：`items` 只对 array 有意义。类型**缺失**时保留
    // （`{"items": …}` 本身合法，丢了会让模型看不到元素结构）；显式声明非 array 类型时丢弃，
    // 与上面 properties/required「非 object 就丢」同一判据。
    //
    // ⚠️ 必须在类型被写回 obj **之前**按**声明**类型算：根 schema 会被强制成 object
    // （见 is_object_schema），若按写回后的有效类型判断，根级 array schema 的元素结构
    // 会被一并丢掉。
    let items_type_ok = normalized_type.as_deref().is_none_or(|t| t == "array");
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
        // 先把归一化后的 properties 算出来（下面的 required 过滤要用它当判据）。
        let mut declared_properties = serde_json::Map::new();
        if let Some(serde_json::Value::Object(props)) = obj.remove("properties") {
            for (name, prop_schema) in props {
                declared_properties.insert(name, normalize_json_schema_inner(prop_schema, false));
            }
        }
        // required：只保留字符串、且**确实声明在 properties 里**的名字。
        //
        // 为什么过滤（对齐参考实现 diegosouzapw/OmniRoute）：悬空 required（指向不存在的
        // 属性）在本管线里只可能是加工残留（union 剥除后分支独有的字段、$ref 降级子树等），
        // 代价却不对称 —— 上游侧按 degraded_object_schema 注释的说法直接 400（整个工具
        // 列表连坐失效），本仓 Bug C 又会把「模型永远看不到的字段」判成缺参、整轮
        // INVALID_TOOL_INPUT；反过来，丢掉悬空名字只让 schema 变宽松，不会误杀合法调用。
        let required = match obj.remove("required") {
            Some(serde_json::Value::Array(arr)) => serde_json::Value::Array(
                arr.into_iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .filter(|name| declared_properties.contains_key(name))
                    .map(serde_json::Value::String)
                    .collect(),
            ),
            _ => serde_json::Value::Array(Vec::new()),
        };
        obj.insert(
            "properties".to_string(),
            serde_json::Value::Object(declared_properties),
        );
        obj.insert("required".to_string(), required);
    } else {
        obj.remove("properties");
        obj.remove("required");
    }

    // items（对象或数组形式取第一个 schema）
    if !items_type_ok {
        // 显式声明了非 array 类型：items 是无效关键字（JSON Schema 只允许 array 带 items），
        // 直接丢弃 —— 与上面 properties/required 对非 object 的处置同一判据，避免产出
        // 结构非法（上游可能判 malformed）的 schema。
        obj.remove("items");
    } else if let Some(items) = obj.remove("items") {
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
    // ⚠️ 能走到这里的 union 只剩**非根层**与畸形值：根级 union 在函数入口已被
    // `flatten_root_union` 合并（顶层直接剥掉会把分支里的 properties 一起丢、工具退化成
    // 零参工具）；这里的 remove 兼作兜底，不得删。
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
///
/// 大小写**敏感**（JSON Schema 规范即如此，`type` 的值域是小写字面量）：刻意不做
/// `to_lowercase()` 容错 —— `"Object"` / `"ARRAY"` 是非法 schema，静默纠正会把客户端
/// 生成器的 bug 藏起来，而且归一化后的值会进上游请求体、与客户端本地校验口径不一致。
fn normalize_schema_type(raw: &str) -> Option<String> {
    match raw.trim() {
        "object" | "array" | "string" | "number" | "integer" | "boolean" => {
            Some(raw.trim().to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `items` 只对 array 有意义：显式非 array 类型上的 items 必须丢弃（与 properties/required
    /// 对非 object 的处置同一判据，避免产出上游可能判 malformed 的 schema）；类型**缺失**时
    /// 保留（`{"items": …}` 本身合法，丢了会让模型看不到元素结构）。
    #[test]
    fn test_items_kept_only_for_array_or_typeless() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "list": { "type": "array", "items": { "type": "string" } },
                "bad": { "type": "string", "items": { "type": "string" } },
                "untyped": { "items": { "type": "string" } }
            }
        });
        let out = normalize_json_schema(schema);
        assert_eq!(
            out["properties"]["list"]["items"]["type"], "string",
            "array 类型的 items 必须保留"
        );
        assert!(
            out["properties"]["bad"].get("items").is_none(),
            "非 array 类型上的 items 是无效组合，必须丢弃: {}",
            out["properties"]["bad"]
        );
        assert_eq!(
            out["properties"]["untyped"]["items"]["type"], "string",
            "类型缺失时 items 合法（任意类型 + 元素约束），不得丢弃"
        );
    }

    /// 根级 array schema：类型会被强制成 object（既有行为），但 items 的合法性按**声明**
    /// 类型判定 —— 元素结构不得被连带丢掉（若按写回后的有效类型判断就会丢）。
    #[test]
    fn test_root_array_items_survives_object_forcing() {
        let out = normalize_json_schema(serde_json::json!({
            "type": "array",
            "items": { "type": "string" }
        }));
        assert_eq!(out["type"], "object", "根 schema 强制 object 是既有行为");
        assert_eq!(
            out["items"]["type"], "string",
            "items 不得因根 schema 被强制 object 而丢掉"
        );
    }

    /// 🔴 根级 `oneOf` 的工具 schema 曾被整体剥成**零参工具**（`properties: {}`）：
    /// Kiro 顶层拒绝 union，而旧处置是逐层 `obj.remove("oneOf")` ⇒ 分支里的 properties
    /// 一起丢、根上只剩强制补出的空 properties/required。本测试锁定扁平化后的合并语义：
    /// 属性逐字段保留、union 键消失、required 取交集。
    ///
    /// 回退即 FAILED：删掉 `flatten_root_union` 的调用（或改回只 remove 不合并），
    /// path/text/old_text 三个断言全部拿到 Null（改动前实际输出：
    /// `{"type":"object","properties":{},"required":[]}`）。
    #[test]
    fn test_root_oneof_flattens_to_merged_object_not_zero_param() {
        let out = normalize_json_schema(serde_json::json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "text": { "type": "string" }
                    },
                    "required": ["path", "text"]
                },
                {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "old_text": { "type": "string" }
                    },
                    "required": ["path", "old_text"]
                }
            ]
        }));
        assert!(
            out.get("oneOf").is_none(),
            "根级 oneOf 必须消失（Kiro 顶层拒绝）: {out}"
        );
        assert_eq!(out["type"], "object");
        // 逐字段断言：两个分支的属性都要在（修复前这里全是 Null —— 零参工具）。
        assert_eq!(out["properties"]["path"]["type"], "string");
        assert_eq!(out["properties"]["text"]["type"], "string");
        assert_eq!(out["properties"]["old_text"]["type"], "string");
        // required 取交集：path 两分支都要；text/old_text 各只属一个分支，不得强制。
        assert_eq!(
            out["required"],
            serde_json::json!(["path"]),
            "required 必须是交集（并集会把分支独有字段强加给所有调用）"
        );
    }

    /// 根级 `anyOf`：同样扁平化；只要有一个分支没声明 `required`，交集就是空 ——
    /// 不得把"部分分支才需要"的字段当必填（那是并集语义）。
    #[test]
    fn test_root_anyof_flattens_with_required_intersection() {
        let out = normalize_json_schema(serde_json::json!({
            "anyOf": [
                {
                    "type": "object",
                    "properties": { "a": { "type": "string" } },
                    "required": ["a"]
                },
                { "type": "object", "properties": { "b": { "type": "integer" } } }
            ]
        }));
        assert!(out.get("anyOf").is_none(), "根级 anyOf 必须消失: {out}");
        assert_eq!(out["properties"]["a"]["type"], "string");
        assert_eq!(out["properties"]["b"]["type"], "integer");
        assert_eq!(
            out["required"],
            serde_json::json!([]),
            "一个分支没声明 required ⇒ 交集为空，不得把 a 当必填"
        );
    }

    /// 根级 `allOf`：属性取并集；required 取**并集**（所有分支同时成立，任一分支要求的
    /// 字段对整体都是必填）—— 刻意与 oneOf/anyOf 的交集语义相反，锁住两者的差异。
    #[test]
    fn test_root_allof_flattens_with_required_union() {
        let out = normalize_json_schema(serde_json::json!({
            "allOf": [
                {
                    "type": "object",
                    "properties": { "name": { "type": "string" } },
                    "required": ["name"]
                },
                {
                    "type": "object",
                    "properties": { "count": { "type": "integer" } },
                    "required": ["count"]
                }
            ]
        }));
        assert!(out.get("allOf").is_none(), "根级 allOf 必须消失: {out}");
        assert_eq!(out["properties"]["name"]["type"], "string");
        assert_eq!(out["properties"]["count"]["type"], "integer");
        assert_eq!(
            out["required"],
            serde_json::json!(["name", "count"]),
            "allOf 各分支同时成立 ⇒ required 是并集；取交集会漏掉真正的必填项"
        );
    }

    /// 合并进来的分支属性仍要过常规递归归一化：子层 union 按非根行为剥除、子层 required
    /// 过滤照常 —— 不得把分支 schema 原样塞进去。
    #[test]
    fn test_root_union_merged_branch_properties_still_normalized() {
        let out = normalize_json_schema(serde_json::json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "filter": {
                            "type": "object",
                            "properties": {
                                "field": { "type": "string" },
                                "opts": { "anyOf": [{ "type": "object" }, { "type": "null" }] }
                            },
                            "required": ["field"]
                        }
                    }
                },
                { "type": "object", "properties": { "limit": { "type": "integer" } } }
            ]
        }));
        let filter = &out["properties"]["filter"];
        assert_eq!(filter["type"], "object", "分支里的子 object 必须保留");
        assert_eq!(
            filter["properties"]["field"]["type"], "string",
            "子层类型归一化照常"
        );
        assert_eq!(
            filter["properties"]["opts"],
            serde_json::json!({}),
            "子层 union 按非根行为剥除（剥完只剩空壳；Kiro 只拒顶层）: {filter}"
        );
        assert_eq!(
            filter["required"],
            serde_json::json!(["field"]),
            "子层 required 过滤（field 已声明）照常"
        );
        assert_eq!(out["properties"]["limit"]["type"], "integer");
    }

    /// 负例：本来就没有 union 的工具 schema 必须**逐字节不变**（防止扁平化误伤正常路径）。
    /// 期望值按改动前的既有行为逐字段手写；改动若在无 union 时多插/少插任何一个键，
    /// 整值相等断言即红。
    #[test]
    fn test_schema_without_root_union_is_unchanged() {
        let out = normalize_json_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "目标路径" },
                "nested": {
                    "type": "object",
                    "properties": { "flag": { "type": "boolean" } },
                    "required": ["flag"],
                    "additionalProperties": false
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }));
        assert_eq!(
            out,
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "目标路径" },
                    "nested": {
                        "type": "object",
                        "properties": { "flag": { "type": "boolean" } },
                        "required": ["flag"],
                        "additionalProperties": false
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            })
        );
    }

    /// 合并后 `additionalProperties` 的收紧判据：**所有**分支都显式 false 才收紧；
    /// 只要有一个分支没声明 false 就保持开放（不写该键）—— 否则会误杀"在宽松分支下
    /// 合法、但带合并集之外属性"的调用。
    #[test]
    fn test_root_union_additional_properties_tightens_only_when_all_branches_false() {
        let all_false = normalize_json_schema(serde_json::json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": { "a": { "type": "string" } },
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": { "b": { "type": "string" } },
                    "additionalProperties": false
                }
            ]
        }));
        assert_eq!(
            all_false["additionalProperties"],
            serde_json::json!(false),
            "所有分支都 false ⇒ 收紧是安全的（属性都在合并集内）"
        );

        let mixed = normalize_json_schema(serde_json::json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": { "a": { "type": "string" } },
                    "additionalProperties": false
                },
                { "type": "object", "properties": { "b": { "type": "string" } } }
            ]
        }));
        assert!(
            mixed.get("additionalProperties").is_none(),
            "有分支未声明 false ⇒ 必须保持开放，不得收紧: {mixed}"
        );
    }

    /// `required` 过滤为「已声明属性」：悬空名字（union 剥除/降级残留）必须去掉。
    /// 上游对悬空 required 直接 400（见 degraded_object_schema 注释），本仓 Bug C 又会把
    /// "模型永远看不到的字段"判成缺参；丢掉悬空名字只让 schema 变宽松。嵌套层同判据。
    #[test]
    fn test_required_filtered_to_declared_properties() {
        let out = normalize_json_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "a": { "type": "string" },
                "inner": {
                    "type": "object",
                    "properties": { "x": { "type": "string" } },
                    "required": ["x", "ghost"]
                }
            },
            "required": ["a", "ghost_root"]
        }));
        assert_eq!(
            out["required"],
            serde_json::json!(["a"]),
            "根层悬空 required 必须过滤"
        );
        assert_eq!(
            out["properties"]["inner"]["required"],
            serde_json::json!(["x"]),
            "嵌套层同判据"
        );
    }
}
