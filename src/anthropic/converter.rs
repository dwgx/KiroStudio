//! Anthropic → Kiro 协议转换器
//!
//! 负责将 Anthropic API 请求格式转换为 Kiro API 请求格式

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::kiro::model::requests::conversation::{
    AssistantMessage, ConversationState, CurrentMessage, HistoryAssistantMessage,
    HistoryUserMessage, KiroImage, Message, UserInputMessage, UserInputMessageContext, UserMessage,
};
use crate::kiro::model::requests::kiro::{
    AdditionalModelRequestFields, KiroOutputConfig,
};
use crate::kiro::model::requests::tool::{
    InputSchema, Tool, ToolResult, ToolSpecification, ToolUseEntry,
};

use super::image_resize::{ResizeConfig, ResizeError, maybe_shrink_image};
use super::types::{ContentBlock, ImageSource, MessagesRequest};

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
fn normalize_json_schema_with_node_budget(
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
fn extract_schema_defs(schema: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
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
const MAX_SCHEMA_NODES: usize = 50_000;

/// `$ref` 展开的全局节点预算 + 降级痕迹。
///
/// 存在的理由：`MAX_REF_DEPTH` 限的是**引用链有多长**，而 `depth` 只在 `$ref` 跳转时 +1、
/// 同级递归复用同一个 depth ⇒ 一个 `$defs` 条目里放 b 个指回自己的兄弟属性，展开量就是
/// b^MAX_REF_DEPTH，**链长限制对扇出爆炸完全无效**。所以必须再有一道按**节点总数**算的闸门。
///
/// ⚠️ 不要把这道闸门"简化"成同级递归也 `depth + 1`：那会把正常大 schema 的同级字段数
/// 算进链长，合法请求会被误杀。两道闸门是互补的，都要留着。
struct SchemaRefBudget {
    /// 本次展开的节点上限（生产恒为 `MAX_SCHEMA_NODES`，测试可注入小值）。
    max_nodes: usize,
    /// 已访问节点数（整棵树累计）。
    visited: usize,
    /// 因预算耗尽而被降级掉的节点数（>0 即本次发生了截断，供日志取证）。
    truncated_nodes: usize,
}

impl SchemaRefBudget {
    fn new(max_nodes: usize) -> Self {
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
fn resolve_schema_refs(
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

/// 从 base64（可能带 data: 前缀）的 PDF 提取纯文本。失败返回 None。
///
/// Kiro 上游不接受 Anthropic 的 `document`(application/pdf) 块。这里在网关侧做
/// 轻量文本抽取（无外部 PDF 库，直接扫描 PDF 内容流里的字面量字符串），把可读
/// 文本转成 `<document>` 文本块随消息下发，让模型至少能读到 PDF 内容。
fn extract_pdf_text_from_base64(data: &str) -> Option<String> {
    use base64::Engine;
    let data = data
        .rsplit_once(',')
        .map(|(_, tail)| tail)
        .unwrap_or(data)
        .trim();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    extract_pdf_text_from_bytes(&bytes)
}

/// 从 PDF 原始字节里抽取内容流字面量字符串（`(...)` 后接 Tj/TJ/' 操作符）。
fn extract_pdf_text_from_bytes(bytes: &[u8]) -> Option<String> {
    let pdf = String::from_utf8_lossy(bytes);
    let mut texts = Vec::new();
    let bytes = pdf.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'(' {
            i += 1;
            continue;
        }
        let Some((raw, next)) = parse_pdf_literal_string(&pdf, i) else {
            i += 1;
            continue;
        };
        i = next;
        // 只保留后面紧跟文本绘制操作符（Tj/TJ/'）的字面量，过滤掉非文本 payload
        let lookahead_end = (i + 32).min(bytes.len());
        let lookahead = &bytes[i..lookahead_end];
        if lookahead.windows(2).any(|w| w == b"Tj" || w == b"TJ") || lookahead.contains(&b'\'') {
            let text = raw.trim();
            if !text.is_empty() {
                texts.push(text.to_string());
            }
        }
    }

    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    }
}

/// 解析 PDF 字面量字符串 `(...)`，处理反斜杠转义（含八进制），返回 (内容, 结束位置)。
fn parse_pdf_literal_string(pdf: &str, start: usize) -> Option<(String, usize)> {
    let bytes = pdf.as_bytes();
    if bytes.get(start) != Some(&b'(') {
        return None;
    }
    let mut out = String::new();
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 1;
                if i >= bytes.len() {
                    break;
                }
                match bytes[i] {
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000c}'),
                    b'(' => out.push('('),
                    b')' => out.push(')'),
                    b'\\' => out.push('\\'),
                    b'\n' | b'\r' => {}
                    c if c.is_ascii_digit() && c < b'8' => {
                        let mut octal = vec![c];
                        for _ in 0..2 {
                            if i + 1 < bytes.len() && (b'0'..=b'7').contains(&bytes[i + 1]) {
                                i += 1;
                                octal.push(bytes[i]);
                            } else {
                                break;
                            }
                        }
                        if let Ok(value) =
                            u8::from_str_radix(std::str::from_utf8(&octal).unwrap_or_default(), 8)
                        {
                            out.push(value as char);
                        }
                    }
                    other => out.push(other as char),
                }
            }
            b')' => return Some((out, i + 1)),
            other => out.push(other as char),
        }
        i += 1;
    }
    None
}

/// 追加到 Write 工具 description 末尾的内容
const WRITE_TOOL_DESCRIPTION_SUFFIX: &str = "- IMPORTANT: If the content to write exceeds 150 lines, you MUST only write the first 50 lines using this tool, then use `Edit` tool to append the remaining content in chunks of no more than 50 lines each. If needed, leave a unique placeholder to help append content. Do NOT attempt to write all content at once.";

/// 追加到 Edit 工具 description 末尾的内容
const EDIT_TOOL_DESCRIPTION_SUFFIX: &str = "- IMPORTANT: If the `new_string` content exceeds 50 lines, you MUST split it into multiple Edit calls, each replacing no more than 50 lines at a time. If used to append content, leave a unique placeholder to help append content. On the final chunk, do NOT include the placeholder.";

/// 追加到 Bash 工具 description 末尾的内容（对齐 kiro 生态：防大命令触发上游参数截断）。
const BASH_TOOL_DESCRIPTION_SUFFIX: &str = "- IMPORTANT: Do not send very large commands, inline scripts, or heredocs through Bash. If a command would exceed 100 lines, 8,000 characters, or roughly 4,000 tokens, create or modify a script/file with chunked Write/Edit calls first, then run a short Bash command that executes it. If a Bash attempt fails due to argument size or truncation, do not retry the same large command; split it smaller.";

/// 追加到系统提示词的分块写入策略
const SYSTEM_CHUNKED_POLICY: &str = "\
When the Write or Edit tool has content size limits, always comply silently. \
Never suggest bypassing these limits via alternative tools. \
Never ask the user whether to switch approaches. \
Complete all chunked operations without commentary.";

/// Claude Code 归因头前缀。
///
/// CC 会把形如 `x-anthropic-billing-header: cc_version=...;cch=...` 的归因头放在
/// system 消息的**第一块**。其中 `cc_version`、`cch` 等字段每次请求都会漂移，
/// 而这一块位于整个 prompt 的最前端。上游 Bedrock prefix cache 要求前缀逐字节一致
/// 才命中（第 N 字节变化则其后全部失效），因此这一块的漂移会让上游缓存命中率≈0。
pub(super) const BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";

/// 归一化后的固定占位符，替换整行漂移的归因头，稳定住转发给上游的 prompt 前缀。
pub(super) const BILLING_HEADER_PLACEHOLDER: &str = "__anthropic_billing_header__";

/// 归一化 Claude Code 的归因头文本块。
///
/// 若 `text` 以归因头前缀开头（该块整行都是每请求漂移的归因字段），折叠成固定占位符；
/// 否则原样返回。此函数同时供两处调用：
/// 1. [`build_history`] 的 system 拼接路径——归一化**转发给上游**的字节，稳定缓存前缀；
/// 2. 影子缓存记账路径——本就归一化，保持两侧一致。
///    // 影子缓存记账已移至 StreamContext.cache_usage (prompt_cache_enabled 开启时)
///
/// 归一化保守：只折叠确定每请求漂移的归因头整块，不触碰其余稳定的 system 内容。
pub(super) fn canonicalize_billing_header(text: &str) -> &str {
    if text.starts_with(BILLING_HEADER_PREFIX) {
        BILLING_HEADER_PLACEHOLDER
    } else {
        text
    }
}

/// 环境噪音剥离开关的运行时镜像（`config.strip_env_noise`，TIER3 热更）。
///
/// 归一化路径（[`canonicalize_system_text`]）拿不到 config，故沿用
/// [`super::handlers`] 已验证的进程级原子镜像范式：main 启动按配置写入、admin 改开关
/// 立即改写、归一化热路径读镜像，无需重启、无锁近零成本。默认 true（与 config 默认一致）。
///
/// **关键**：转发字节路径（[`build_history`]）与影子缓存记账路径
/// 都经由 [`canonicalize_system_text`] 读同一镜像，
/// 保证两侧对同一 system 块施加**完全一致**的变换，记账与真实缓存不脱节。
/// // 影子缓存记账已移至 StreamContext.cache_usage (prompt_cache_enabled 开启时)
static STRIP_ENV_NOISE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// 设置环境噪音剥离开关（main 启动接线 / admin 热更调用，立即生效，下个请求即读到新值）。
pub fn set_strip_env_noise(enabled: bool) {
    STRIP_ENV_NOISE.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

fn strip_env_noise_enabled() -> bool {
    STRIP_ENV_NOISE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Kiro 原生 effort 开关镜像（`config.native_thinking_effort_enabled`，**默认 false**）。
///
/// 开启后，白名单模型 + thinking 启用时，请求改用顶层
/// `additionalModelRequestFields.output_config.effort` 触发上游原生 reasoning
/// （实测只有它能触发 `reasoningContentEvent`，XML 标签既不触发还污染历史上下文），
/// 并抑制 `<thinking_mode>` 标签注入；关闭（默认）时行为逐字节不变。
///
/// [`build_history`] / [`convert_request`] 是纯转换拿不到 config，沿用 [`STRIP_ENV_NOISE`]
/// 同款进程级原子镜像：main 启动按配置写入、admin 改开关立即改写、转换热路径读镜像。
static NATIVE_THINKING_EFFORT_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 设置 native effort 开关（main 启动接线 / admin 热更调用，立即生效，下个请求即读到新值）。
pub fn set_native_thinking_effort_enabled(enabled: bool) {
    NATIVE_THINKING_EFFORT_ENABLED.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

fn native_thinking_effort_enabled() -> bool {
    NATIVE_THINKING_EFFORT_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// 工具顶层 description 的字符上限镜像（`config.tool_description_max_chars`，TIER3 热更）。
///
/// [`convert_tools`] 是纯转换、拿不到 config，沿用 [`STRIP_ENV_NOISE`] 同款进程级原子镜像：
/// main 启动按配置写入、admin 改值立即改写、转换热路径读镜像，无锁近零成本。
/// schema 内嵌 description 上限取此值的 1/5（保持既有 10000/2000 比例），0 表示不截断。
static TOOL_DESC_MAX_CHARS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(10000);

/// 设置工具描述上限（main 启动接线 / admin 热更调用，立即生效，下个请求即读到新值）。
pub fn set_tool_description_max_chars(n: usize) {
    TOOL_DESC_MAX_CHARS.store(n, std::sync::atomic::Ordering::Relaxed);
}

/// 顶层 description 上限（0 = 不截断）。
fn tool_description_max_chars() -> usize {
    TOOL_DESC_MAX_CHARS.load(std::sync::atomic::Ordering::Relaxed)
}

/// schema 内嵌 description 上限：顶层的 1/5（保持既有 10000→2000 比例）。0（不截断）时同样不截断。
fn schema_description_max_chars() -> usize {
    let top = tool_description_max_chars();
    if top == 0 { 0 } else { (top / 5).max(1) }
}

/// 按字符边界安全截断（防多字节切断）。`max==0` 表示不截断，原样返回。
fn truncate_chars(s: &str, max: usize) -> String {
    if max == 0 {
        return s.to_string();
    }
    match s.char_indices().nth(max) {
        Some((idx, _)) => s[..idx].to_string(),
        None => s.to_string(),
    }
}

/// 归一化单个 system 文本块：折叠归因头 + （开关开启时）剥离环境噪音。
///
/// Claude Code 每次请求都会在 system 里携带「每次漂移」的环境上下文（工作目录/日期/
/// 平台的 `<env>` 块、`gitStatus:`、`Recent commits:`、模型名行等）。这些漂移行位于
/// prompt 前缀，只要变一个字节，上游 Bedrock prefix cache 其后全部失效（命中率≈0），
/// 且它们是关联「这是 Claude Code」的强指纹。剥离它们同时省 token、提命中率、降关联风险。
///
/// 返回 [`std::borrow::Cow`]：未发生任何改写时借用原串零分配；发生折叠/剥离时返回改写副本。
/// 该函数是转发字节与影子指纹两条路径的**唯一**归一化入口，确保两侧字节一致。
///
/// 保守原则：只剥离「确定每请求漂移」的整块 / 整行，绝不触碰稳定的 system 正文
/// （如工具说明、身份声明、任务指令）。
pub(super) fn canonicalize_system_text(text: &str) -> std::borrow::Cow<'_, str> {
    // 1. 归因头整块 → 固定占位符（无条件，历史行为）
    let folded = canonicalize_billing_header(text);
    if !std::ptr::eq(folded, text) {
        return std::borrow::Cow::Borrowed(folded);
    }
    // 2. 环境噪音剥离（受开关控制；默认开）
    if strip_env_noise_enabled()
        && let Some(stripped) = strip_env_noise_lines(text)
    {
        return std::borrow::Cow::Owned(stripped);
    }
    std::borrow::Cow::Borrowed(text)
}

/// 剥离 system 文本里每请求漂移的环境噪音行 / 整段。
///
/// 参照 kiro-account-manager `prompt_filter::filter_env_noise`（MIT），并补齐现代
/// Claude Code 的 `<env>...</env>` 标签块形式。仅当确有内容被剥离时返回 `Some(改写副本)`，
/// 否则返回 `None`（调用方据此零分配借用原串）。
///
/// 剥离目标全部是「确定每请求漂移」或「纯环境元数据」，不含稳定正文：
/// - `<env>...</env>` 整块（工作目录 / 平台 / 日期，每请求 / 每日漂移）；
/// - `# Environment` / `# auto memory` markdown 段（到下一个 `# ` 标题为止）；
/// - `gitStatus:` / `Recent commits:` 声明行、`.claude/projects/` 路径行；
/// - `Assistant knowledge cutoff` / `powered by the model named` 等模型/环境元数据行。
fn strip_env_noise_lines(text: &str) -> Option<String> {
    let mut changed = false;
    let mut out: Vec<&str> = Vec::new();
    let mut skip_section = false; // # Environment / # auto memory markdown 段
    let mut skip_env_tag = false; // <env>...</env> 标签块

    for line in text.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_lowercase();

        // <env>...</env> 块：整块剥离（含首尾标签行）。cwd/platform/date 每次漂移。
        if skip_env_tag {
            changed = true;
            if trimmed.contains("</env>") {
                skip_env_tag = false;
            }
            continue;
        }
        if trimmed.starts_with("<env>") {
            changed = true;
            // 兼容单行 <env>...</env>
            if !trimmed.contains("</env>") {
                skip_env_tag = true;
            }
            continue;
        }

        // # Environment / # auto memory 段：跳到下一个 `# ` 标题（保留该新标题）。
        if trimmed == "# Environment" || trimmed == "# auto memory" {
            skip_section = true;
            changed = true;
            continue;
        }
        if skip_section {
            if trimmed.starts_with("# ") {
                skip_section = false;
                // fall through：保留新标题行
            } else {
                changed = true;
                continue;
            }
        }

        // 单独漂移行 / 环境元数据行
        if trimmed.starts_with("gitStatus:")
            || trimmed.starts_with("Recent commits:")
            || trimmed.starts_with("Assistant knowledge cutoff")
            || lower.contains("powered by the model named")
            || trimmed.contains(".claude/projects/")
            || trimmed.contains("git status at the start of the conversation")
            || trimmed.contains("has been invoked in the following environment")
        {
            changed = true;
            continue;
        }

        out.push(line);
    }

    if !changed {
        return None;
    }
    Some(collapse_blank_lines(&out.join("\n")))
}

/// 连续空行合并为一行，并去除首尾空白（剥离整段后常留多余空行）。
fn collapse_blank_lines(s: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut blanks = 0;
    for line in s.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        out.push(line);
    }
    out.join("\n").trim().to_string()
}

/// 模型映射：将 Anthropic 模型名映射到 Kiro 模型 ID。
///
/// **已重构**:委托给声明式模型目录 [`super::model_catalog`](单一真相源)。
/// 旧实现是 `contains` 子串启发式，有三个真漏洞(Claude3 静默升贵档 / 高版本静默降级 /
/// 子串误命中),且模型清单散落四处易漂移。现改为「精确别名 → 结构化 family+版本 →
/// 老名近似(告警) → 未知拒绝」的分层解析，所有非精确命中都打 warn 日志(可观测)。
pub fn map_model(model: &str) -> Option<String> {
    super::model_catalog::resolve_kiro_id(model).map(|s| s.to_string())
}

/// 根据模型名称返回对应的上下文窗口大小。
///
/// 委托给模型目录 [`super::model_catalog::context_window`]:窗口值直接来自 `ModelSpec.context_window`
/// (单一真相源),不再复用 map_model 的映射结果拼判——避免继承 map_model 误判(如老名被当 1M)。
pub fn get_context_window_size(model: &str) -> i32 {
    super::model_catalog::context_window(model)
}

/// 转换结果
#[derive(Debug)]
pub struct ConversionResult {
    /// 转换后的 Kiro 请求
    pub conversation_state: ConversationState,
    /// 工具名称映射（短名称 → 原始名称），仅当存在超长工具名时非空
    pub tool_name_map: HashMap<String, String>,
    /// 本次请求声明的工具名集合（= 发给模型看到的名字，超长名已缩短）。
    /// 文本化 invoke 重组的"工具名硬护栏"用它：解析出的工具名必须在此集合里才允许捞回,
    /// 否则当普通文本吐出——宁可漏捞不可把正文里讨论的假命令误执行。
    pub known_tool_names: std::collections::HashSet<String>,
    /// 每个工具的**必需参数名**（`input_schema.required`），供流式层做 **Bug C** 校验：
    /// `tool_use` 参数 JSON 合法但缺必需字段（如 `Bash` 缺 `command`）。
    ///
    /// key 与 [`Self::known_tool_names`] 同口径（发给模型的名字，含缩短后的短名）。
    /// 无必需参数的工具不入表；空表 = 不校验。
    pub tool_required_fields: HashMap<String, Vec<String>>,
    /// Kiro 原生 effort 请求（`additionalModelRequestFields.output_config.effort`）。
    ///
    /// 仅当 `native_thinking_effort_enabled` 开启 **且** 模型在白名单 **且** thinking
    /// 启用时 Some；否则 None。开关默认关 ⇒ 恒 None，请求字节与旧版完全一致。
    /// 与 [`build_history`] 的 XML 抑制共用同一判定函数，保证「不发字段就不剥标签」。
    pub additional_model_request_fields: Option<AdditionalModelRequestFields>,
}

/// 转换错误
#[derive(Debug)]
pub enum ConversionError {
    UnsupportedModel(String),
    EmptyMessages,
    /// Claude Code 工具参数无法映射到 Kiro 上游。
    ///
    /// ⚠️ **2026-08-10 起全仓无产出点**（编译器会报 `never constructed`）。
    /// 曾经唯一的产出点是 `Read.pages`，现已改为**降级处理**
    /// （整读 + 把页范围意图写进 `explanation`，见 `map_tool_input_to_kiro` 的 `Read` 分支）
    /// —— 因为用一个可选的范围提示去否决整轮请求，代价（对话中断）远大于收益。
    ///
    /// **刻意保留而不删除**，两个理由：
    /// 1. 它有两处渲染分支（`handlers.rs:1875` 与 `:3119` → 400 `invalid_request_error`），
    ///    删变体会连带删掉那条对外契约；
    /// 2. 「客户端工具参数在 Kiro 侧确实无法表达」是**真实存在**的一类情形，
    ///    未来新增工具映射时很可能需要它。此时该判断的是「能否降级」而非直接沿用本变体
    ///    —— 只有当丢弃该参数会让工具**执行出错**（而非仅结果不精确）时才该用它。
    #[allow(dead_code)]
    ///
    /// 由 [`map_tool_input_to_kiro`] 返回，convert_request 侧把它转成客户端可读的 400，
    /// 而不是把无效参数透传给上游（那会得到更含糊的上游 400）。
    UnsupportedToolMapping { tool_name: String, reason: String },
}

impl std::fmt::Display for ConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConversionError::UnsupportedModel(model) => write!(f, "模型不支持: {}", model),
            ConversionError::EmptyMessages => write!(f, "消息列表为空"),
            ConversionError::UnsupportedToolMapping { tool_name, reason } => {
                write!(f, "工具参数无法映射: {tool_name} — {reason}")
            }
        }
    }
}

impl std::error::Error for ConversionError {}

/// 从 metadata.user_id 中提取 session UUID
///
/// 支持两种格式:
/// 1. 字符串格式: user_xxx_account__session_0b4445e1-f5be-49e1-87ce-62bbc28ad705
/// 2. JSON 格式: {"device_id":"...","account_uuid":"...","session_id":"UUID"}
///
/// 从 conversationId 确定性派生 agentContinuationId（UUID 形状的 SHA256 前 16 字节）。
///
/// 同一会话恒定、跨会话隔离。目的:稳住转发给上游的会话键,让同一会话连续请求能命中
/// 上游 prefix 缓存的 credit 折扣（见调用点说明）。加固定前缀避免与其它 SHA 用途碰撞。
fn derive_agent_continuation_id(conversation_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agent-continuation:");
    hasher.update(conversation_id.as_bytes());
    let r = hasher.finalize();
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        r[0],
        r[1],
        r[2],
        r[3],
        r[4],
        r[5],
        r[6],
        r[7],
        r[8],
        r[9],
        r[10],
        r[11],
        r[12],
        r[13],
        r[14],
        r[15]
    )
}

/// 提取 session UUID 作为 conversationId
fn extract_session_id(user_id: &str) -> Option<String> {
    // 先尝试 JSON 解析
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(user_id) {
        if let Some(session_id) = json.get("session_id").and_then(|v| v.as_str()) {
            if is_valid_uuid(session_id) {
                return Some(session_id.to_string());
            }
        }
    }

    // 回退到字符串格式: 查找 "session_" 后面的内容
    if let Some(pos) = user_id.find("session_") {
        let session_part = &user_id[pos + 8..]; // "session_" 长度为 8
        // 安全：用 get(..36) 而非 &session_part[..36]。后者按定长字节切片，
        // 若第 36 字节落在多字节 UTF-8 字符中间（客户端可控的 metadata.user_id
        // 如 "session_"+34字符+"中"），会 str range-index panic 打崩请求任务。
        // get(..36) 在非字符边界处返回 None，自然回退到随机 conversationId。
        if let Some(uuid_str) = session_part.get(..36) {
            if is_valid_uuid(uuid_str) {
                return Some(uuid_str.to_string());
            }
        }
    }
    None
}

/// 简单验证 UUID 格式（36 字符，包含 4 个连字符）
fn is_valid_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4
}

/// 无 `metadata.user_id` 时，从「工作上下文」派生稳定 conversationId。
///
/// # 为什么需要
///
/// 只有 Claude Code 会发 `metadata.user_id`（内含 session UUID）。`python` / `curl` /
/// `opencode` 等客户端不发，旧实现回落到 `Uuid::new_v4()` —— **每个请求都是全新会话键，
/// 于是同一工作上下文的连续请求永远拿不到上游 prefix 缓存的 credit 折扣**。
///
/// 2026-08-04 实测（08-03 全天 40,634 请求）：15,776 个「单轮会话」占 38.8% 的请求，
/// 其中 `unknown`/`python`/`curl`/`opencode` 客户端 15,614 个。这批请求输入中位
/// 173,482 token、p90 657,907 —— **不是小请求**，而是永久零命中的大请求。
///
/// # 派生输入的选择
///
/// 用 `system` 文本 + 排序后的工具名集合，二者都经过与请求路径同一套归一化：
///
/// - **system 走 [`canonicalize_system_text`]** —— 它已剥掉每请求漂移的段（`<env>` 块、
///   `gitStatus:`、`# Environment` 等）。不复用它就会让工作目录或日期的变化把键打散，
///   等于没修。
/// - **工具名排序** —— 官方自认造过「工具排序非确定」的事故；不排序则同一上下文因工具
///   顺序抖动而分裂成多个键。
/// - **不含 messages** —— 历史每轮都在变，含进去等于每请求一个新键，回到原问题。
///
/// 加固定前缀 `derived-conversation:` 避免与 [`derive_agent_continuation_id`] 的哈希
/// 用途碰撞。返回 UUID 形状是因为下游 `derive_agent_continuation_id` 与上游都按 UUID
/// 形状消费该字段。
///
/// # 边界
///
/// system 与 tools 双双为空时返回 `None`，让调用方回落到随机 UUID —— 那种请求没有可
/// 稳定的前缀可言，强行归到同一个键只会让无关请求互相污染上游会话。
///
/// # 多租户：为何跨用户撞键是安全的
///
/// 不同用户若 system + tools 完全相同，会派生出同一个 conversationId。**这不会串话**：
/// [`ConversationState`] 每次请求都携带完整 `history`（由 [`build_history`] 现场构建），
/// 上游不靠 `continuationId` 重建历史。撞键的后果仅是两人共用一个上游会话键，而前缀
/// 字节不同 → 缓存未命中，退化到修复前的状态，不会读到对方的内容。
///
/// 因此没有按用户加盐。要加盐就得给 [`convert_request`] 传租户标识，那会改动全部调用点，
/// 而换来的只是「本来就不会发生的泄漏」不发生 —— 不值得。若将来上游改为按
/// `continuationId` 服务端保存历史，此处必须立刻改为加盐。
fn derive_conversation_id_from_context(req: &MessagesRequest) -> Option<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"derived-conversation:");

    let mut has_material = false;

    if let Some(system) = req.system.as_deref() {
        for msg in system {
            let canonical = canonicalize_system_text(&msg.text);
            if !canonical.trim().is_empty() {
                has_material = true;
                hasher.update(canonical.as_bytes());
                // 分隔符防止拼接歧义（["ab","c"] 与 ["a","bc"] 必须不同键）
                hasher.update(b"\x1f");
            }
        }
    }

    if let Some(tools) = req.tools.as_deref() {
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        for name in names {
            has_material = true;
            hasher.update(name.as_bytes());
            hasher.update(b"\x1f");
        }
    }

    if !has_material {
        return None;
    }

    let r = hasher.finalize();
    Some(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        r[0],
        r[1],
        r[2],
        r[3],
        r[4],
        r[5],
        r[6],
        r[7],
        r[8],
        r[9],
        r[10],
        r[11],
        r[12],
        r[13],
        r[14],
        r[15]
    ))
}

/// 收集历史消息中使用的所有工具名称
fn collect_history_tool_names(history: &[Message]) -> Vec<String> {
    let mut tool_names = Vec::new();

    for msg in history {
        if let Message::Assistant(assistant_msg) = msg {
            if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                for tool_use in tool_uses {
                    if !tool_names.contains(&tool_use.name) {
                        tool_names.push(tool_use.name.clone());
                    }
                }
            }
        }
    }

    tool_names
}

/// 为历史中使用但不在 tools 列表中的工具创建占位符定义
/// Kiro API 要求：历史消息中引用的工具必须在 currentMessage.tools 中有定义
fn create_placeholder_tool(name: &str) -> Tool {
    Tool {
        tool_specification: ToolSpecification {
            name: name.to_string(),
            description: "Tool used in conversation history".to_string(),
            input_schema: InputSchema::from_json(serde_json::json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": true
            })),
        },
    }
}

/// 将 Anthropic 请求转换为 Kiro 请求
pub fn convert_request(req: &MessagesRequest) -> Result<ConversionResult, ConversionError> {
    // 1. 映射模型
    let model_id = map_model(&req.model)
        .ok_or_else(|| ConversionError::UnsupportedModel(req.model.clone()))?;

    // 2. 检查消息列表
    if req.messages.is_empty() {
        return Err(ConversionError::EmptyMessages);
    }

    // 2.5. 预处理 prefill：如果末尾是 assistant，静默丢弃并截断到最后一条 user
    // Claude 4.x 已弃用 assistant prefill，Kiro API 也不支持
    let messages: &[_] = if req.messages.last().is_some_and(|m| m.role != "user") {
        tracing::info!("检测到末尾 assistant 消息（prefill），静默丢弃");
        let last_user_idx = req
            .messages
            .iter()
            .rposition(|m| m.role == "user")
            .ok_or(ConversionError::EmptyMessages)?;
        &req.messages[..=last_user_idx]
    } else {
        &req.messages
    };

    // 3. 生成会话 ID 和代理 ID
    // 优先从 metadata.user_id 中提取 session UUID 作为 conversationId
    // 三级回落：客户端显式 session_id → 工作上下文派生 → 随机。
    // 中间这级是 2026-08-04 新增（L0-5）：不发 metadata 的客户端（python/curl/opencode）
    // 此前每请求一个随机键 → 永久零命中，占全站 38.8% 的请求。见
    // `derive_conversation_id_from_context` 的实测数据。
    let conversation_id = req
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_ref())
        .and_then(|user_id| extract_session_id(user_id))
        .or_else(|| derive_conversation_id_from_context(req))
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    // agentContinuationId 从 conversationId 确定性派生（SHA256），而非每请求随机。
    //
    // 实测(2026-07-07 Phase0):同一大 prompt 前缀一致时上游 credit 折扣约 47%
    // （meteringEvent credits 0.141→0.075）。而每请求随机的 continuationId 若进入上游
    // 会话/前缀键,会让同一会话的连续请求无法命中上游 prefix 缓存,白白丢掉这份折扣。
    // 改为按 conversationId 确定性派生:同一会话稳定复用、跨会话仍相互隔离。
    // 参考 TsinHzl/kiro2cc-proxy derive_agent_continuation_id（MIT）。
    let agent_continuation_id = derive_agent_continuation_id(&conversation_id);

    // 4. 确定触发类型
    let chat_trigger_type = determine_chat_trigger_type(req);

    // 5. 处理最后一条消息作为 current_message（经过 prefill 预处理，末尾必为 user）
    let last_message = messages.last().unwrap();
    let (text_content, images, tool_results) = process_message_content(&last_message.content)?;

    // 6. 转换工具定义（超长名称自动缩短并记录映射）
    let mut tool_name_map = HashMap::new();
    let mut tools = convert_tools(&req.tools, &mut tool_name_map);

    // 7. 构建历史消息（需要先构建，以便收集历史中使用的工具）
    let mut history = build_history(req, messages, &model_id, &mut tool_name_map)?;

    // 8. 验证并过滤 tool_use/tool_result 配对
    // 移除孤立的 tool_result（没有对应的 tool_use）
    // 同时返回孤立的 tool_use_id 集合，用于后续清理
    let (validated_tool_results, orphaned_tool_use_ids) =
        validate_tool_pairing(&history, &tool_results);

    // 9. 从历史中移除孤立的 tool_use（Kiro API 要求 tool_use 必须有对应的 tool_result）
    remove_orphaned_tool_uses(&mut history, &orphaned_tool_use_ids);

    // 10. 收集历史中使用的工具名称，为缺失的工具生成占位符定义
    // Kiro API 要求：历史消息中引用的工具必须在 tools 列表中有定义
    // 注意：Kiro 匹配工具名称时忽略大小写，所以这里也需要忽略大小写比较
    let history_tool_names = collect_history_tool_names(&history);
    let existing_tool_names: std::collections::HashSet<_> = tools
        .iter()
        .map(|t| t.tool_specification.name.to_lowercase())
        .collect();

    for tool_name in history_tool_names {
        if !existing_tool_names.contains(&tool_name.to_lowercase()) {
            tools.push(create_placeholder_tool(&tool_name));
        }
    }

    // 工具名硬护栏集合 = 发给模型的工具名（含缩短后的短名 + 历史占位工具名）。模型文本化 invoke 时
    // 吐的就是它看到的这些名字,重组时据此校验,杜绝把正文里讨论的假命令误重组成 tool_use 执行。
    // 必须在 tools 被 move 进 context 之前收集。
    let known_tool_names: std::collections::HashSet<String> = tools
        .iter()
        .map(|t| t.tool_specification.name.clone())
        .collect();

    // 每个工具的**必需参数名**（Bug C 校验用）。与 `known_tool_names` 同处提取、
    // 同口径（key 是**发给模型的名字**，含 `map_tool_name` 缩短后的短名），
    // 这样流式层校验时不必再做名字还原。
    //
    // Bug C = `tool_use` 参数 **JSON 完全合法但缺必需字段**（如 `Bash` 只给了
    // `description` 没给 `command`）。它既不是 Bug A（JSON 语法坏，`tool_repair_json` 能修）
    // 也不是 Bug B（连 tool_use 块都没吐，网关碰不到），此前落在两者之间的盲区：
    // 客户端拿到合法 JSON 后按 schema 校验失败，报 `The required parameter 'X' is missing`。
    //
    // 只取 `required` 的**名字列表**，不做完整 JSON Schema 校验 —— 后者是过度设计，
    // 且类型不匹配的容错空间远大于"字段整个缺失"。
    // 无 `required` / 非数组 / 空数组的工具不入表（那类工具没有必需参数，无从校验）。
    let tool_required_fields: HashMap<String, Vec<String>> = tools
        .iter()
        .filter_map(|t| {
            let req = t.tool_specification.input_schema.json.get("required")?;
            let names: Vec<String> = req
                .as_array()?
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            if names.is_empty() {
                return None;
            }
            Some((t.tool_specification.name.clone(), names))
        })
        .collect();

    // 11. 构建 UserInputMessageContext
    let mut context = UserInputMessageContext::new();
    if !tools.is_empty() {
        context = context.with_tools(tools);
    }
    if !validated_tool_results.is_empty() {
        context = context.with_tool_results(validated_tool_results);
    }

    // 12. 构建当前消息
    // 保留文本内容，即使有工具结果也不丢弃用户文本
    let content = text_content;

    let mut user_input = UserInputMessage::new(content, &model_id)
        .with_context(context)
        .with_origin("AI_EDITOR");

    if !images.is_empty() {
        user_input = user_input.with_images(images);
    }

    let current_message = CurrentMessage::new(user_input);

    // 13. 构建 ConversationState
    let conversation_state = ConversationState::new(conversation_id)
        .with_agent_continuation_id(agent_continuation_id)
        .with_agent_task_type("vibe")
        .with_chat_trigger_type(chat_trigger_type)
        .with_current_message(current_message)
        .with_history(history);

    if !tool_name_map.is_empty() {
        tracing::info!("工具名称映射: {} 个超长名称已缩短", tool_name_map.len());
    }

    // 14. native effort：开关开 + 白名单模型 + thinking 启用时，把 effort 装进请求级
    // `additionalModelRequestFields.output_config.effort`（Kiro 原生 reasoning 通道）。
    // 与 build_history 里的 XML 抑制共用同一判定（build_additional_model_request_fields
    // 见 generate_thinking_prefix 上方的说明），两者不会分叉。
    let additional_model_request_fields = build_additional_model_request_fields(req, &model_id);

    Ok(ConversionResult {
        conversation_state,
        tool_name_map,
        known_tool_names,
        tool_required_fields,
        additional_model_request_fields,
    })
}

/// 确定聊天触发类型
/// "AUTO" 模式可能会导致 400 Bad Request 错误
fn determine_chat_trigger_type(_req: &MessagesRequest) -> String {
    "MANUAL".to_string()
}

/// 历史图片去重上限。
///
/// 历史中被"上浮"到顶层 images 的图片总数上限，防止多轮对话里累积的截图 base64
/// 把请求体撑爆（与"400 Input too long"防护对齐）。仅约束历史去重路径，当前轮图片
/// （dedup 为 None）不受此限、永远保留。
const MAX_TOTAL_IMAGES: usize = 20;

/// 处理消息内容，提取文本、图片和工具结果
fn process_message_content(
    content: &serde_json::Value,
) -> Result<(String, Vec<KiroImage>, Vec<ToolResult>), ConversionError> {
    // 当前轮消息不做去重（dedup 为 None），本轮所有图片永远保留
    process_message_content_dedup(content, None)
}

/// 与 [`process_message_content`] 相同，但当 `dedup` 为 `Some` 时按 SHA256 对图片去重：
/// 同一张图（base64 完全一致）在历史多轮中反复出现时只在首次保留，之后替换为占位符文本，
/// 避免同一截图跨多轮反复以 base64 重发而烧 token。
fn process_message_content_dedup(
    content: &serde_json::Value,
    mut dedup: Option<&mut std::collections::HashSet<String>>,
) -> Result<(String, Vec<KiroImage>, Vec<ToolResult>), ConversionError> {
    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    let mut tool_results = Vec::new();

    match content {
        serde_json::Value::String(s) => {
            text_parts.push(s.clone());
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                if let Ok(block) = serde_json::from_value::<ContentBlock>(item.clone()) {
                    match block.block_type.as_str() {
                        "text" => {
                            if let Some(text) = block.text {
                                text_parts.push(text);
                            }
                        }
                        "image" => {
                            if let Some(source) = block.source
                                && let Some(placeholder) =
                                    extract_kiro_image(&source, &mut dedup, &mut images)
                            {
                                // 去重/超额命中时补占位符文本，保证图片语义不至于完全消失
                                text_parts.push(placeholder);
                            }
                        }
                        "document" => {
                            // Kiro 不接受 document 块；对 PDF 做轻量文本抽取转成文本块下发，
                            // 抽取失败则留占位说明，避免文档语义完全丢失。
                            if let Some(source) = block.source
                                && source.media_type == "application/pdf"
                            {
                                match extract_pdf_text_from_base64(&source.data) {
                                    Some(text) => text_parts.push(format!(
                                        "<document media_type=\"application/pdf\">\n{}\n</document>",
                                        text
                                    )),
                                    None => text_parts.push(
                                        "[PDF 文档已附加，但文本提取失败]".to_string(),
                                    ),
                                }
                            }
                        }
                        "tool_result" => {
                            if let Some(tool_use_id) = block.tool_use_id {
                                let result_content = extract_tool_result_content(
                                    &block.content,
                                    &mut dedup,
                                    &mut images,
                                );
                                let is_error = block.is_error.unwrap_or(false);

                                let mut result = if is_error {
                                    ToolResult::error(&tool_use_id, result_content)
                                } else {
                                    ToolResult::success(&tool_use_id, result_content)
                                };
                                result.status =
                                    Some(if is_error { "error" } else { "success" }.to_string());

                                tool_results.push(result);
                            }
                        }
                        "tool_use" => {
                            // tool_use 在 assistant 消息中处理，这里忽略
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    Ok((text_parts.join("\n"), images, tool_results))
}

/// 从 media_type 获取图片格式
fn get_image_format(media_type: &str) -> Option<String> {
    match media_type {
        "image/jpeg" => Some("jpeg".to_string()),
        "image/png" => Some("png".to_string()),
        "image/gif" => Some("gif".to_string()),
        "image/webp" => Some("webp".to_string()),
        _ => None,
    }
}

/// 嗅探图片 magic bytes 所需的字节数。
///
/// webp 判据最长：`RIFF`(4) + 文件长度(4) + `WEBP`(4) = 12 字节，其余格式都更短。
const IMAGE_MAGIC_PROBE_BYTES: usize = 12;

/// 只解码 base64 头部若干字节，够判类型即止。
///
/// 图片动辄几百 KB，为判类型解整张图纯属浪费。base64 每 4 字符对应 3 字节，故取
/// 前 16 个有效字符（=12 字节，正好覆盖 webp 判据）单独解码——切在 4 的整数倍上，
/// 无需补 padding。客户端偶尔发 `data:image/png;base64,` 前缀或带换行的 base64，
/// 这里一并剥掉，否则解码直接失败、退化成"认不出"。
fn decode_base64_head(data: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let payload = data.rsplit_once(',').map(|(_, tail)| tail).unwrap_or(data);
    // 过滤空白（换行/缩进）后再截取，避免把有效字符数算少
    let head: Vec<u8> = payload
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .take(IMAGE_MAGIC_PROBE_BYTES.div_ceil(3) * 4)
        .collect();
    // 不足 4 字符无法解出任何完整字节；非 4 倍数说明整张图本就极短，交给下游按原样处理
    if head.len() < 4 || head.len() % 4 != 0 {
        return None;
    }
    base64::engine::general_purpose::STANDARD.decode(&head).ok()
}

/// 按 magic bytes 判断图片真实格式，认不出返回 None（不猜）。
fn sniff_image_format(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8]) {
        return Some("jpeg");
    }
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        return Some("png");
    }
    if bytes.starts_with(b"GIF8") {
        return Some("gif");
    }
    // RIFF 容器还装 wav/avi，必须连偏移 8 处的 `WEBP` 一起验
    if bytes.starts_with(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    None
}

/// 定出下发给上游的图片格式：**magic bytes 优先于客户端声明的 media_type**。
///
/// 客户端（尤其截图工具链）经常声明 `image/png` 而实际字节是 jpeg，上游据此回
/// `ValidationException` 400。实测这类 400 有 49 次，纯本地可修。
///
/// 顺序是本函数的全部意义：先嗅探、认出就用嗅探结果覆盖声明值；只有**认不出**
/// （不匹配任何 magic）才回退到声明值——不猜，否则会把上游本来能接受的格式改坏。
fn resolve_image_format(source: &ImageSource) -> Option<String> {
    let declared = get_image_format(&source.media_type);
    let sniffed = decode_base64_head(&source.data)
        .as_deref()
        .and_then(sniff_image_format);

    match sniffed {
        Some(actual) => {
            if declared.as_deref() != Some(actual) {
                tracing::debug!(
                    declared = %source.media_type,
                    actual = %actual,
                    "图片 media_type 与 magic bytes 不符，按实际字节纠正"
                );
            }
            Some(actual.to_string())
        }
        None => declared,
    }
}

/// 把一个 image 块的 source 转成 `KiroImage` 并上浮到顶层 `images`。
///
/// tool_result 内的图片与顶层 image 块走同一条转换链（mime 校验 + SHA256 去重 + 上浮），
/// 因为上游 Kiro 的 `ToolResult` 没有 image 字段，图片只能走 `userInputMessage.images[]`
/// 顶层通道。
///
/// 返回值：
/// - `Some(placeholder)`：历史去重命中、历史图片数超过 [`MAX_TOTAL_IMAGES`] 上限、或图片超过安全上限，图片被省略；
/// - `None`：图片已上浮到 `images`，或格式不支持（无法转换）。
fn extract_kiro_image(
    source: &ImageSource,
    dedup: &mut Option<&mut std::collections::HashSet<String>>,
    images: &mut Vec<KiroImage>,
) -> Option<String> {
    // 单图 base64 大小上限（硬安全网，非降采样目标）：AWS Q / Kiro 上游有 per-field 大小硬限制，
    // 8MiB 是远超大图的阈值；超限省略 + 占位符（不 fail 请求，与 MAX_TOTAL_IMAGES 同风格）。
    // 8MiB 内的超大图（如 4K 截图常超 1MiB）不再省略，走下方 maybe_shrink_image 降采样后不丢图。
    const MAX_SINGLE_IMAGE_BASE64_BYTES: usize = 8 * 1024 * 1024;
    if source.data.len() > MAX_SINGLE_IMAGE_BASE64_BYTES {
        tracing::warn!(
            bytes = source.data.len(),
            "单图 base64 超过 {} 字节，省略该图（防上游 per-field 大小限制）",
            MAX_SINGLE_IMAGE_BASE64_BYTES
        );
        return Some("[image omitted: exceeds single-image size limit]".to_string());
    }

    // 格式以 magic bytes 为准、声明值兜底；都定不出时无声跳过（与旧行为一致，不补占位符）
    let format = resolve_image_format(source)?;

    // 历史去重：只在 dedup 为 Some（历史路径）时生效，当前轮图片永远保留
    if let Some(seen) = dedup.as_deref_mut() {
        let mut hasher = Sha256::new();
        hasher.update(source.data.as_bytes());
        let digest = format!("{:x}", hasher.finalize());

        if !seen.insert(digest) {
            // 已见过同一张图：省略 base64，返回去重占位符
            return Some("[image omitted: identical to an earlier screenshot]".to_string());
        }
        // 首次见到但已超过历史图片配额：撤销刚插入的指纹，返回超额占位符
        if seen.len() > MAX_TOTAL_IMAGES {
            return Some("[image omitted: too many images in history]".to_string());
        }
    }

    // 智能降采样（GreyGunG image_resize 移植）：8MiB 内的图先尝试缩小到上游可接受的长边
    // /字节预算（PNG/WebP/JPEG 一律重编码 JPEG，GIF 保留原格式防丢动画）。小图零开销直通。
    let cfg = ResizeConfig::from_env();
    match maybe_shrink_image(cfg, &format, &source.data) {
        Ok(processed) => {
            images.push(KiroImage::from_base64(processed.format, processed.data_base64));
        }
        Err(ResizeError::LimitExceeded(_)) => {
            // 安全上限（base64/解码字节/像素/GIF 超预算）触发：维持旧行为"省略 + 占位符"，
            // 不 fail 请求——把超限 payload 直接透传上游比省略更危险。
            tracing::warn!(
                bytes = source.data.len(),
                format = %format,
                "单图超过安全上限，省略该图（防上游 per-field 大小限制）"
            );
            return Some("[image omitted: exceeds image safety limits]".to_string());
        }
        Err(e) => {
            // 解码/编码失败：保留原图透传（坏图不该让整请求失败），警告留痕。
            tracing::warn!(
                error = %e,
                bytes = source.data.len(),
                format = %format,
                "图片降采样失败，保留原图透传"
            );
            images.push(KiroImage::from_base64(format, source.data.clone()));
        }
    }
    None
}

/// 提取工具结果内容
///
/// 文本元素保留为 tool_result 占位符文本；`type=="image"` 的块被提取成 `KiroImage`
/// 并上浮到顶层 `images`（上游 `ToolResult` 无 image 字段，图片只能走顶层通道）。
/// 若某个 tool_result 只有图片没有文本，则用占位符文本 "[image attached]"。
fn extract_tool_result_content(
    content: &Option<serde_json::Value>,
    dedup: &mut Option<&mut std::collections::HashSet<String>>,
    images: &mut Vec<KiroImage>,
) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => {
            let mut parts = Vec::new();
            let mut had_image = false;
            for item in arr {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    parts.push(text.to_string());
                } else if item.get("type").and_then(|v| v.as_str()) == Some("image")
                    && let Ok(block) = serde_json::from_value::<ContentBlock>(item.clone())
                    && let Some(source) = block.source
                {
                    had_image = true;
                    if let Some(placeholder) = extract_kiro_image(&source, dedup, images) {
                        parts.push(placeholder);
                    }
                }
            }
            if parts.is_empty() && had_image {
                "[image attached]".to_string()
            } else {
                parts.join("\n")
            }
        }
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

/// 验证并过滤 tool_use/tool_result 配对
///
/// 收集所有 tool_use_id，验证 tool_result 是否匹配
/// 静默跳过孤立的 tool_use 和 tool_result，输出警告日志
///
/// # Arguments
/// * `history` - 历史消息引用
/// * `tool_results` - 当前消息中的 tool_result 列表
///
/// # Returns
/// 元组：(经过验证和过滤后的 tool_result 列表, 孤立的 tool_use_id 集合)
fn validate_tool_pairing(
    history: &[Message],
    tool_results: &[ToolResult],
) -> (Vec<ToolResult>, std::collections::HashSet<String>) {
    use std::collections::HashSet;

    // 1. 收集所有历史中的 tool_use_id
    let mut all_tool_use_ids: HashSet<String> = HashSet::new();
    // 2. 收集历史中已经有 tool_result 的 tool_use_id
    let mut history_tool_result_ids: HashSet<String> = HashSet::new();

    for msg in history {
        match msg {
            Message::Assistant(assistant_msg) => {
                if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                    for tool_use in tool_uses {
                        all_tool_use_ids.insert(tool_use.tool_use_id.clone());
                    }
                }
            }
            Message::User(user_msg) => {
                // 收集历史 user 消息中的 tool_results
                for result in &user_msg
                    .user_input_message
                    .user_input_message_context
                    .tool_results
                {
                    history_tool_result_ids.insert(result.tool_use_id.clone());
                }
            }
        }
    }

    // 3. 计算真正未配对的 tool_use_ids（排除历史中已配对的）
    let mut unpaired_tool_use_ids: HashSet<String> = all_tool_use_ids
        .difference(&history_tool_result_ids)
        .cloned()
        .collect();

    // 4. 过滤并验证当前消息的 tool_results
    let mut filtered_results = Vec::new();

    for result in tool_results {
        if unpaired_tool_use_ids.contains(&result.tool_use_id) {
            // 配对成功
            filtered_results.push(result.clone());
            unpaired_tool_use_ids.remove(&result.tool_use_id);
        } else if all_tool_use_ids.contains(&result.tool_use_id) {
            // tool_use 存在但已经在历史中配对过了，这是重复的 tool_result
            tracing::warn!(
                "跳过重复的 tool_result：该 tool_use 已在历史中配对，tool_use_id={}",
                result.tool_use_id
            );
        } else {
            // 孤立 tool_result - 找不到对应的 tool_use
            tracing::warn!(
                "跳过孤立的 tool_result：找不到对应的 tool_use，tool_use_id={}",
                result.tool_use_id
            );
        }
    }

    // 5. 检测真正孤立的 tool_use（有 tool_use 但在历史和当前消息中都没有 tool_result）
    for orphaned_id in &unpaired_tool_use_ids {
        tracing::warn!(
            "检测到孤立的 tool_use：找不到对应的 tool_result，将从历史中移除，tool_use_id={}",
            orphaned_id
        );
    }

    (filtered_results, unpaired_tool_use_ids)
}

/// 从历史消息中移除孤立的 tool_use
///
/// Kiro API 要求每个 tool_use 必须有对应的 tool_result，否则返回 400 Bad Request。
/// 此函数遍历历史中的 assistant 消息，移除没有对应 tool_result 的 tool_use。
///
/// # Arguments
/// * `history` - 可变的历史消息列表
/// * `orphaned_ids` - 需要移除的孤立 tool_use_id 集合
fn remove_orphaned_tool_uses(
    history: &mut [Message],
    orphaned_ids: &std::collections::HashSet<String>,
) {
    if orphaned_ids.is_empty() {
        return;
    }

    for msg in history.iter_mut() {
        if let Message::Assistant(assistant_msg) = msg {
            if let Some(ref mut tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                let original_len = tool_uses.len();
                tool_uses.retain(|tu| !orphaned_ids.contains(&tu.tool_use_id));

                // 如果移除后为空，设置为 None
                if tool_uses.is_empty() {
                    assistant_msg.assistant_response_message.tool_uses = None;
                } else if tool_uses.len() != original_len {
                    tracing::debug!(
                        "从 assistant 消息中移除了 {} 个孤立的 tool_use",
                        original_len - tool_uses.len()
                    );
                }
            }
        }
    }
}

/// Kiro API 工具名称最大长度限制（字节）
const TOOL_NAME_MAX_LEN: usize = 63;
/// 生成确定性短名称：截断前缀 + "_" + 8 位 SHA256 hex
// ============ Claude Code ↔ Kiro 工具名/参数双向映射（对齐生态 kiro.rs） ============
// 开关：默认开启（KiroStudio 的目标客户端是 Claude Code，8 个内置工具映射成 Kiro 原生名后
// 与上游协议匹配）。若接入非 Claude Code 客户端且恰好使用同名自定义工具，可经
// set_tool_compat_mapping(false) 关闭回退为透传（main.rs 可从 config/env 接入）。
static TOOL_COMPAT_MAPPING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

pub(crate) fn set_tool_compat_mapping(enabled: bool) {
    TOOL_COMPAT_MAPPING.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

fn tool_compat_mapping_enabled() -> bool {
    TOOL_COMPAT_MAPPING.load(std::sync::atomic::Ordering::Relaxed)
}

// 背景：Claude Code 的 8 个内置工具（Write/Edit/Bash/Read/Glob/Grep/LS/WebSearch）与 Kiro
// 上游的原生工具名不同（fs_write/str_replace/execute_bash/read_file/...），参数也不同
// （file_path→path、content→text、old_string→oldStr、new_string→newStr、offset/limit→start_line/end_line）。
// 本层在入站时把 Claude Code 工具名+参数映射成 Kiro 原生格式，出站时还原回 Claude Code 格式，
// 根治 `Invalid tool parameters`（Claude Code 发 file_path 而上游只认 path 那类参数错配）。

fn optional_number(value: &serde_json::Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_u64().map(|v| v as i64))
}

fn take_first(
    obj: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<serde_json::Value> {
    keys.iter().find_map(|key| obj.get(*key).cloned())
}

fn maybe_insert(
    out: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: Option<serde_json::Value>,
) {
    if let Some(value) = value {
        if !value.is_null() {
            out.insert(key.to_string(), value);
        }
    }
}

fn default_explanation(tool_name: &str) -> serde_json::Value {
    serde_json::Value::String(format!("Mapped from Claude Code {} tool.", tool_name))
}

/// Claude Code 内置工具名 → Kiro 原生工具名（8 个固定映射）。
/// 不在表内的工具（MCP/自定义）原样透传，不做映射。
fn claude_code_tool_name_to_kiro(name: &str) -> Option<&'static str> {
    match name {
        "Write" => Some("fs_write"),
        "Edit" => Some("str_replace"),
        "Bash" => Some("execute_bash"),
        "Read" => Some("read_file"),
        "Glob" => Some("file_search"),
        "Grep" => Some("grep_search"),
        "LS" => Some("list_directory"),
        "WebSearch" => Some("web_search"),
        _ => None,
    }
}

/// 客户端工具名 → Kiro 工具名：命中映射表用 Kiro 名并记录反向映射（Kiro名→客户端名）；
/// 否则走 [`map_tool_name`]（超长缩短）。返回 Kiro 名。
fn map_client_tool_name_to_kiro(
    name: &str,
    tool_name_map: &mut HashMap<String, String>,
) -> String {
    if let Some(kiro_name) = claude_code_tool_name_to_kiro(name) {
        tool_name_map
            .entry(kiro_name.to_string())
            .or_insert_with(|| name.to_string());
        return kiro_name.to_string();
    }
    map_tool_name(name, tool_name_map)
}

/// 入站参数转换：Claude Code 工具参数 → Kiro 原生参数。非内置工具或非对象输入原样返回。
fn map_tool_input_to_kiro(
    client_name: &str,
    input: serde_json::Value,
) -> Result<serde_json::Value, ConversionError> {
    let Some(kiro_name) = claude_code_tool_name_to_kiro(client_name) else {
        return Ok(input);
    };
    let serde_json::Value::Object(obj) = input else {
        return Ok(input);
    };

    let mut out = serde_json::Map::new();
    match (client_name, kiro_name) {
        ("Write", "fs_write") => {
            maybe_insert(&mut out, "path", take_first(&obj, &["file_path", "path"]));
            maybe_insert(&mut out, "text", take_first(&obj, &["content", "text"]));
            // ⚠️ 透传 write_mode（"create"|"append"）：不映射会被静默丢弃，
            // 上游 fs_write 退化成覆盖写，多轮后客户端按默认执行 → 数据丢失风险。
            maybe_insert(&mut out, "write_mode", take_first(&obj, &["write_mode"]));
        }
        ("Edit", "str_replace") => {
            maybe_insert(&mut out, "path", take_first(&obj, &["file_path", "path"]));
            maybe_insert(&mut out, "oldStr", take_first(&obj, &["old_string", "oldStr"]));
            maybe_insert(&mut out, "newStr", take_first(&obj, &["new_string", "newStr"]));
        }
        ("Bash", "execute_bash") => {
            maybe_insert(&mut out, "command", take_first(&obj, &["command"]));
            maybe_insert(&mut out, "timeout", take_first(&obj, &["timeout"]));
        }
        ("Read", "read_file") => {
            // 🔴 `Read.pages`（读 PDF 页范围）在 Kiro `read_file` 侧无等价参数。
            //
            // 改前：直接 `return Err(UnsupportedToolMapping)` ⇒ `handlers.rs:1875` 把它渲染成
            // **400 `工具参数无法映射: Read — ...`** 并终结整个请求（无重试、无降级）
            // ⇒ 客户端（Claude Code）看到硬错误，**整轮对话失败**。
            //
            // 为什么这个处置过重：`pages` 只是「读哪几页」的**范围提示**，丢掉它的后果是
            // 「读了整个文件」——信息更多而非更少，模型完全能自己在结果里找目标页。
            // 拿它去否决整轮请求，代价（对话中断）远大于收益（避免一次范围不精确的读取）。
            // 本仓既有原则也是这个方向：`toolTruncationRecovery` 宁可整轮重试也不下发半截参数，
            // 但那是因为半截参数会让**工具执行出错**；这里丢掉可选提示不会出错。
            //
            // 现在：把 `pages` 的意图折进 explanation，折不了就**忽略并继续**。
            // ⚠️ 这里只**记下** hint，真正写入放在下面 `maybe_insert(explanation)` **之后**
            // —— 那个 helper 无条件覆盖，先写会被原始 explanation 冲掉。
            let mut pages_hint: Option<String> = None;
            if let Some(pages) = obj.get("pages").filter(|v| !v.is_null()) {
                // Claude Code 的 pages 形如 "1-5" / "3" / [1,2,3]（视客户端版本）。
                // Kiro 只有 start_line/end_line（行号语义）。两者量纲不同（页 vs 行），
                // 无法精确换算 ⇒ 只在**单页**且能解析出数字时给一个保守提示，其余一律忽略。
                // 不猜「每页多少行」——猜错会截掉用户要的内容，比不截更糟。
                let hint = match pages {
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Number(n) => Some(n.to_string()),
                    serde_json::Value::Array(a) => Some(
                        a.iter()
                            .filter_map(|v| v.as_i64())
                            .map(|n| n.to_string())
                            .collect::<Vec<_>>()
                            .join(","),
                    ),
                    _ => None,
                };
                pages_hint = hint.filter(|h| !h.is_empty());
                tracing::debug!(
                    tool = %client_name,
                    "Read.pages 无 Kiro 等价参数，已降级为整读 + explanation 提示（不再 400 终结请求）"
                );
            }
            maybe_insert(&mut out, "path", take_first(&obj, &["file_path", "path"]));
            let offset = obj.get("offset").and_then(optional_number);
            let limit = obj.get("limit").and_then(optional_number);
            if let Some(start) = offset {
                out.insert("start_line".to_string(), serde_json::json!(start));
            }
            if let Some(limit) = limit {
                let end = offset.map(|start| start + limit - 1).unwrap_or(limit);
                out.insert("end_line".to_string(), serde_json::json!(end));
            }
            maybe_insert(&mut out, "explanation", take_first(&obj, &["explanation"]));
            out.entry("explanation".to_string())
                .or_insert_with(|| default_explanation(client_name));
            // pages 的意图在最后追加：此刻 explanation 已定稿（原值或默认值），
            // 追加不会被覆盖。这是「降级但不丢意图」的落点。
            if let Some(h) = pages_hint {
                let prev = out
                    .get("explanation")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                out.insert(
                    "explanation".to_string(),
                    serde_json::json!(format!(
                        "{prev}（用户只关心第 {h} 页；Kiro read_file 无页范围参数，已整读，请自行定位）"
                    )),
                );
            }
        }
        ("Glob", "file_search") => {
            maybe_insert(&mut out, "query", take_first(&obj, &["pattern", "query"]));
            maybe_insert(
                &mut out,
                "excludePattern",
                take_first(&obj, &["excludePattern", "exclude"]),
            );
            if let Some(v) = take_first(&obj, &["includeIgnoredFiles", "include_ignored"]) {
                let mapped = match v {
                    serde_json::Value::Bool(true) => serde_json::json!("yes"),
                    serde_json::Value::Bool(false) => serde_json::json!("no"),
                    other => other,
                };
                out.insert("includeIgnoredFiles".to_string(), mapped);
            }
            maybe_insert(&mut out, "explanation", take_first(&obj, &["explanation"]));
            out.entry("explanation".to_string())
                .or_insert_with(|| default_explanation(client_name));
        }
        ("Grep", "grep_search") => {
            maybe_insert(&mut out, "query", take_first(&obj, &["pattern", "query"]));
            maybe_insert(
                &mut out,
                "includePattern",
                take_first(&obj, &["glob", "includePattern"]),
            );
            maybe_insert(
                &mut out,
                "excludePattern",
                take_first(&obj, &["excludePattern", "exclude"]),
            );
            maybe_insert(
                &mut out,
                "caseSensitive",
                take_first(&obj, &["caseSensitive", "case_sensitive"]),
            );
            maybe_insert(&mut out, "explanation", take_first(&obj, &["explanation"]));
        }
        ("LS", "list_directory") => {
            maybe_insert(&mut out, "path", take_first(&obj, &["path"]));
            maybe_insert(&mut out, "depth", take_first(&obj, &["depth"]));
            maybe_insert(&mut out, "explanation", take_first(&obj, &["explanation"]));
            out.entry("explanation".to_string())
                .or_insert_with(|| default_explanation(client_name));
        }
        ("WebSearch", "web_search") => {
            maybe_insert(&mut out, "query", take_first(&obj, &["query"]));
        }
        _ => return Ok(serde_json::Value::Object(obj)),
    }

    Ok(serde_json::Value::Object(out))
}

/// 出站参数还原：Kiro 原生参数 → Claude Code 工具参数。非内置工具原样返回。
///
/// `pub(crate)`：stream.rs / handlers.rs 在 tool_use 下发前用它把 Kiro 参数（path/oldStr/start_line…）
/// 还原成 Claude Code 参数（file_path/old_string/offset…），保证客户端看到的工具调用与它发出的
/// 参数形态一致（否则多轮 tool_result 上下文与当前轮 schema 错配）。
pub(crate) fn map_tool_input_from_kiro(client_name: &str, input: serde_json::Value) -> serde_json::Value {
    if claude_code_tool_name_to_kiro(client_name).is_none() {
        return input;
    }
    let serde_json::Value::Object(obj) = input else {
        return input;
    };
    // ⚠️ 从 obj 克隆而非空对象重建：旧实现把内置工具输入里所有未映射键静默丢弃
    // （Write 带额外字段 → 还原后只剩 file_path/content，其余消失），既丢真实数据，
    // 也破坏「tool input 原样透传」的既有契约（stream.rs 的 test_tool_input_not_stripped_by_dsml_*
    // 靠它钉住）。改为保留未映射键、只重命名已知键。
    let mut out = obj.clone();
    match client_name {
        "Write" => {
            remap_tool_keys(&mut out, &["path", "file_path"], "file_path");
            remap_tool_keys(&mut out, &["text", "content"], "content");
        }
        "Edit" => {
            remap_tool_keys(&mut out, &["path", "file_path"], "file_path");
            remap_tool_keys(&mut out, &["oldStr", "old_string"], "old_string");
            remap_tool_keys(&mut out, &["newStr", "new_string"], "new_string");
        }
        "Bash" => {
            remap_tool_keys(&mut out, &["command"], "command");
            remap_tool_keys(&mut out, &["timeout"], "timeout");
        }
        "Read" => {
            remap_tool_keys(&mut out, &["path", "file_path"], "file_path");
            let start = obj.get("start_line").and_then(optional_number);
            let end = obj.get("end_line").and_then(optional_number);
            out.remove("start_line");
            out.remove("end_line");
            if let Some(start) = start {
                out.insert("offset".to_string(), serde_json::json!(start));
            }
            if let Some(end) = end {
                let limit = start.map(|s| end - s + 1).unwrap_or(end);
                if limit > 0 {
                    out.insert("limit".to_string(), serde_json::json!(limit));
                }
            }
            // ⚠️ 入站注入的 explanation 是幻影参数（客户端 Read schema 无此键），
            // 严格校验会报 invalid tool input 并污染多轮上下文，出站必须剥离。
            out.remove("explanation");
        }
        "Glob" => {
            remap_tool_keys(&mut out, &["query", "pattern"], "pattern");
            // ⚠️ 入站把 includeIgnoredFiles 的 bool 转成了 "yes"/"no" 字符串（Kiro 接受），
            // 出站必须还原回 bool，否则客户端收到字符串与其 boolean schema 类型不符。
            if let Some(v) = out.get("includeIgnoredFiles").cloned() {
                let mapped = match v.as_str() {
                    Some("yes") => serde_json::json!(true),
                    Some("no") => serde_json::json!(false),
                    _ => v,
                };
                out.insert("includeIgnoredFiles".to_string(), mapped);
            }
            out.remove("explanation");
        }
        "Grep" => {
            remap_tool_keys(&mut out, &["query", "pattern"], "pattern");
            remap_tool_keys(&mut out, &["includePattern", "glob"], "glob");
            // ⚠️ 入站把 exclude 映射到 excludePattern，出站必须还原回 exclude，
            // 否则客户端 Grep 用错参数名、exclude 丢失。
            remap_tool_keys(&mut out, &["excludePattern", "exclude"], "exclude");
            remap_tool_keys(
                &mut out,
                &["caseSensitive", "case_sensitive"],
                "case_sensitive",
            );
            // ⚠️ Grep 入站只 `maybe_insert`（客户端发才保留）**不注入默认**，
            // 模型真实生成的 explanation 应保留（Claude Code Grep schema 的 explanation
            // 是 required），不能像 Read/Glob/LS 那样无条件剥除。
        }
        "LS" => {
            remap_tool_keys(&mut out, &["path"], "path");
            out.remove("explanation");
        }
        "WebSearch" => {
            remap_tool_keys(&mut out, &["query"], "query");
        }
        _ => return serde_json::Value::Object(obj),
    }
    serde_json::Value::Object(out)
}

/// 出站还原辅助：把 `out` 里第一个命中的候选键移除并改插到目标键下，其余键原样保留。
/// 未命中任何候选键时 no-op（保留原键，不产生新键）。
fn remap_tool_keys(
    out: &mut serde_json::Map<String, serde_json::Value>,
    candidates: &[&str],
    target: &str,
) {
    let value = candidates.iter().find_map(|k| out.remove(*k));
    if let Some(v) = value {
        if !v.is_null() {
            out.insert(target.to_string(), v);
        }
    }
}

/// 出站工具名 + 参数还原：Kiro 名 → Claude Code 名（查 tool_name_map），参数同步还原。
pub fn restore_tool_use_for_client(
    kiro_name: &str,
    input: serde_json::Value,
    tool_name_map: &HashMap<String, String>,
) -> (String, serde_json::Value) {
    let client_name = tool_name_map
        .get(kiro_name)
        .cloned()
        .unwrap_or_else(|| kiro_name.to_string());
    let client_input = map_tool_input_from_kiro(&client_name, input);
    (client_name, client_input)
}

/// 可选字段 schema：Kiro 的 read_file/file_search 等要求可选参数用 anyOf[type, null]。
fn optional_schema(schema: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "anyOf": [schema, {"type": "null"}] })
}

/// Kiro 内置工具的描述：命中内置名用固定描述（拼接大文件分块提示），否则回退调用方描述。
fn kiro_builtin_tool_description(name: &str, fallback: &str) -> String {
    match name {
        "fs_write" => format!(
            "Write text content to a file.\n{}",
            WRITE_TOOL_DESCRIPTION_SUFFIX
        ),
        "str_replace" => format!(
            "Replace an exact string in a file.\n{}",
            EDIT_TOOL_DESCRIPTION_SUFFIX
        ),
        "execute_bash" => format!(
            "Execute the specified bash command.\n{}",
            BASH_TOOL_DESCRIPTION_SUFFIX
        ),
        "read_file" => "Read a single file with optional line range specification.".to_string(),
        "file_search" => "Search for files by fuzzy file path query.".to_string(),
        "grep_search" => "Search file contents using a regex pattern.".to_string(),
        "list_directory" => "List directory contents.".to_string(),
        "web_search" => "Search the web for up-to-date information.".to_string(),
        _ if fallback.trim().is_empty() => name.to_string(),
        _ => fallback.to_string(),
    }
}

/// Kiro 内置工具的 JSON schema 合成：命中内置名返回合成 schema（参数名已是 Kiro 原生形态，
/// 与 [`map_tool_input_to_kiro`] 的输出一致），否则返回 None。
fn kiro_builtin_tool_schema(name: &str) -> Option<serde_json::Value> {
    Some(match name {
        "fs_write" => serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Absolute path to file."},
                "text": {"type": "string", "description": "Contents to write into the file."}
            },
            "required": ["path", "text"],
            "additionalProperties": false
        }),
        "str_replace" => serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Absolute path to file."},
                "oldStr": {"type": "string", "description": "Exact string to replace."},
                "newStr": {"type": "string", "description": "Replacement string."}
            },
            "required": ["path", "oldStr", "newStr"],
            "additionalProperties": false
        }),
        "execute_bash" => serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Bash command to execute."},
                "timeout": optional_schema(serde_json::json!({"type": "number", "description": "Optional timeout in milliseconds."}))
            },
            "required": ["command"],
            "additionalProperties": false
        }),
        "read_file" => serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to file to read."},
                "start_line": optional_schema(serde_json::json!({"type": "number", "description": "Starting line number."})),
                "end_line": optional_schema(serde_json::json!({"type": "number", "description": "Ending line number."})),
                "explanation": {"type": "string", "description": "Why this file is being read."}
            },
            "required": ["path", "explanation"],
            "additionalProperties": false
        }),
        "file_search" => serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Fuzzy filename query."},
                "explanation": {"type": "string", "description": "Why this search is being performed."},
                "excludePattern": optional_schema(serde_json::json!({"type": "string", "description": "Glob pattern for files to exclude."})),
                "includeIgnoredFiles": optional_schema(serde_json::json!({"type": "string", "description": "Whether to include ignored files, yes or no."}))
            },
            "required": ["query", "explanation"],
            "additionalProperties": false
        }),
        "grep_search" => serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "minLength": 1, "description": "Regex pattern to search for."},
                "caseSensitive": optional_schema(serde_json::json!({"type": "boolean", "description": "Whether the search should be case sensitive."})),
                "includePattern": optional_schema(serde_json::json!({"type": "string", "description": "Glob pattern for files to include."})),
                "excludePattern": optional_schema(serde_json::json!({"type": "string", "description": "Glob pattern for files to exclude."})),
                "explanation": optional_schema(serde_json::json!({"type": "string", "description": "Why this search is being performed."}))
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        "list_directory" => serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to directory."},
                "depth": optional_schema(serde_json::json!({"type": "number", "description": "Depth of recursive listing."})),
                "explanation": {"type": "string", "description": "Why this directory is being listed."}
            },
            "required": ["path", "explanation"],
            "additionalProperties": false
        }),
        "web_search" => serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query."}
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        _ => return None,
    })
}

fn shorten_tool_name(name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    let hash_hex = format!("{:x}", hasher.finalize());
    let hash_suffix = &hash_hex[..8];
    // 预算：54 字节前缀 + 1 字节下划线 + 8 字节 hash = 63 字节 = Kiro 上限。
    //
    // ⚠️ 前缀必须按**字节**截断（Kiro 的 63 是字节限制），同时**不能切裂 UTF-8 多字节字符**。
    // 两个历史错误都要避免：
    //   - 按字节裸切 `&name[..54]`：会 panic（切在 CJK 字符中间）；
    //   - 按字符数截断 `chars().take(54)`：54 个汉字 = 162 字节，加后缀 171 字节，照样远超上限。
    // 正确做法：逐字符累加字节数，在不超过预算的前提下尽可能多取（UTF-8 安全的字节界截断）。
    const PREFIX_MAX_BYTES: usize = TOOL_NAME_MAX_LEN - 1 - 8; // 54 字节
    let mut prefix = String::with_capacity(PREFIX_MAX_BYTES);
    for ch in name.chars() {
        if prefix.len() + ch.len_utf8() > PREFIX_MAX_BYTES {
            break;
        }
        prefix.push(ch);
    }
    format!("{}_{}", prefix, hash_suffix)
}

/// 如果名称超长则缩短，并记录映射（short → original）
///
/// ⚠️ 超限判断与前缀截断必须用**同一单位（字节）**，因为 Kiro 上游的 63 是字节限制。
///
/// 历史 bug：两者单位不一致 ——
///   - `map_tool_name` 用 `name.len()`（字节）判是否超过 63 ← 正确
///   - `shorten_tool_name` 用 `char_indices().nth(54)`（字符）截前缀 ← 对纯 ASCII 恰好等价，
///     但对 CJK 就错了：30 个汉字 = 90 字节 > 63 触发缩短，而 `nth(54)` 在只有 30 个字符时
///     返回 `None` → prefix 取**整个名字**（90 字节）→ short = 90+1+8 = 99 字节，
///     **比原名更长且仍然超限** → 上游回 400 Improperly formed request。
/// 修复后：前缀按字节预算（54）逐字符累加截断，UTF-8 安全且结果恒 ≤63 字节。
fn map_tool_name(name: &str, tool_name_map: &mut HashMap<String, String>) -> String {
    if name.len() <= TOOL_NAME_MAX_LEN {
        return name.to_string();
    }
    let short = shorten_tool_name(name);
    debug_assert!(
        short.len() <= TOOL_NAME_MAX_LEN,
        "shorten_tool_name 生成的短名 {:?} ({} 字节) 仍超过 Kiro 上限 {} 字节",
        short,
        short.len(),
        TOOL_NAME_MAX_LEN
    );
    tool_name_map.insert(short.clone(), name.to_string());
    short
}

/// 转换工具定义
fn convert_tools(
    tools: &Option<Vec<super::types::Tool>>,
    tool_name_map: &mut HashMap<String, String>,
) -> Vec<Tool> {
    let Some(tools) = tools else {
        return Vec::new();
    };

    tools
        .iter()
        // 🔴 2026-08-09：web_search **不再整条剥离**，改为归一化成 Kiro 可接受的形态。
        //
        // 改前的问题：这里把 `type: web_search_*` / `name: web_search` 一律跳过，
        // 于是"常规工具 + web_search"混合请求（Claude Code 的常态）下发给上游时
        // **完全没有搜索工具** ⇒ 模型压根不知道自己能搜 ⇒ CC 的 WebSearch 静默失效。
        //
        // 为什么现在能不剥：本文件 `kiro_builtin_tool_schema` 早就定义了合法的
        // `"web_search" => {query: string}` schema（converter.rs:1946），且参考实现
        // ref-grey 用**同一份 schema**、不做剥离 ⇒ 上游认这个形态。改前是"定义了却
        // 从不下发"的自相矛盾：原注释担心的 400 来自**原样透传 Anthropic 的
        // `type: web_search_20250305` 服务端工具形态**（那个上游确实不认），
        // 而不是来自这个归一化后的函数形态。
        //
        // 所以处置是：把 Anthropic 服务端工具形态**改写**成 Kiro 函数工具形态
        // （下面 map 里 `t.name == "web_search"` 会命中 `kiro_builtin_tool_schema`），
        // 保留搜索能力。纯 web_search 与显式触发仍走本地 MCP 快路径（handlers 前置判定），
        // 不受影响。
        .map(|t| {
            // Anthropic 服务端工具形态（`type: web_search_*`、可能没有 name）→ 归一化成
            // 函数工具 `name: web_search`，让下面的通用转换命中内置 schema。
            let is_server_web_search = t
                .tool_type
                .as_deref()
                .is_some_and(|ty| ty.starts_with("web_search"));
            if is_server_web_search && t.name != "web_search" {
                let mut normalized = t.clone();
                normalized.name = "web_search".to_string();
                normalized.tool_type = None;
                normalized
            } else {
                t.clone()
            }
        })
        .map(|t| {
            let map_enabled = tool_compat_mapping_enabled();
            // Claude Code 内置工具名 → Kiro 原生名（Write→fs_write 等），记录反向映射；
            // 非内置工具走 map_tool_name（超长缩短）。开关关闭时全部透传（仅超长缩短）。
            let mapped_name = if map_enabled {
                map_client_tool_name_to_kiro(&t.name, tool_name_map)
            } else {
                map_tool_name(&t.name, tool_name_map)
            };
            let is_builtin = map_enabled && claude_code_tool_name_to_kiro(&t.name).is_some();

            // 描述：内置工具用 Kiro 固定描述（已拼大文件分块提示，见 kiro_builtin_tool_description）；
            // 非内置工具保留原描述 + Write/Edit/Bash 分块后缀。
            let description = if is_builtin {
                kiro_builtin_tool_description(&mapped_name, &t.description)
            } else {
                let mut description = t.description.clone();
                let suffix = match t.name.as_str() {
                    "Write" => WRITE_TOOL_DESCRIPTION_SUFFIX,
                    "Edit" => EDIT_TOOL_DESCRIPTION_SUFFIX,
                    "Bash" => BASH_TOOL_DESCRIPTION_SUFFIX,
                    _ => "",
                };
                if !suffix.is_empty() {
                    description.push('\n');
                    description.push_str(suffix);
                }
                description
            };
            // 限制顶层描述长度（默认 10000 字符，可配置；安全截断 UTF-8，单次遍历）
            let description = truncate_chars(&description, tool_description_max_chars());

            // schema：内置工具用合成 schema（参数名已是 Kiro 原生形态，与 map_tool_input_to_kiro
            // 的输出一致）；非内置工具用客户端 schema 规范化（剥 $ref/null/anyOf 等）。
            let schema = if is_builtin {
                kiro_builtin_tool_schema(&mapped_name).unwrap_or_else(|| {
                    normalize_json_schema(serde_json::json!(t.input_schema))
                })
            } else {
                normalize_json_schema(serde_json::json!(t.input_schema))
            };

            Tool {
                tool_specification: ToolSpecification {
                    name: mapped_name,
                    description,
                    input_schema: InputSchema::from_json(schema),
                },
            }
        })
        .collect()
}

/// 生成thinking标签前缀
fn generate_thinking_prefix(req: &MessagesRequest) -> Option<String> {
    if let Some(t) = &req.thinking {
        if t.thinking_type == "enabled" {
            return Some(format!(
                "<thinking_mode>enabled</thinking_mode><max_thinking_length>{}</max_thinking_length>",
                t.budget_tokens
            ));
        } else if t.thinking_type == "adaptive" {
            let effort = req
                .output_config
                .as_ref()
                .map(|c| c.effort.as_str())
                .unwrap_or("high");
            return Some(format!(
                "<thinking_mode>adaptive</thinking_mode><thinking_effort>{}</thinking_effort>",
                effort
            ));
        }
    }
    None
}

/// —— native effort 路径 ——
///
/// 背景：本仓此前 Opus/Sonnet 的 extended thinking 只靠 `<thinking_mode>` XML 标签注入。
/// 参考仓（GreyGunG/Kiro-RS-Tool @795b9ca，2026-06-07 对 Kiro CLI 2.6.0 + Opus 4.8/xHigh
/// 的黑盒实测）结论：**只有请求级 `additionalModelRequestFields.output_config.effort`
/// 能触发上游 `reasoningContentEvent`**；XML 标签既不触发，还会把 `thinking_mode` /
/// `max_thinking_length` 塞进历史上下文（污染 + 烧 token）。本段把该机制移植过来。
///
/// 保守边界：白名单是参考仓**单次实测的硬编码推测值**，按本仓 `model_catalog` 校准——
/// 只放 catalog 里存在、且参考仓实测过的 4 个 kiro_id；未实测的模型（opus-5 / sonnet-5 /
/// 4.5 / 4.0 等）一律不进白名单，宁可回退 XML 注入。白名单与 catalog 的一致性由守卫测试
/// `native_effort_whitelist_models_exist_in_catalog` 钉死。
///
/// 开关 `native_thinking_effort_enabled` 默认 false ⇒ 本路径整体不生效，请求字节与旧版
/// 完全一致；开启后仅命中白名单模型 + thinking 启用时改写。
const EFFORTS_WITH_XHIGH: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const EFFORTS_WITHOUT_XHIGH: &[&str] = &["low", "medium", "high", "max"];

/// 模型 → 白名单允许的 effort 档位表（key 是 `map_model` 映射后的 Kiro modelId）。
///
/// - `claude-opus-4.8` / `claude-opus-4.7`：实测认 `output_config` + 五档（含 xhigh）；
/// - `claude-opus-4.6` / `claude-sonnet-4.6`：同一 schema 路径，但**无 xhigh** 档。
fn native_reasoning_efforts(model_id: &str) -> Option<&'static [&'static str]> {
    match model_id {
        "claude-opus-4.8" | "claude-opus-4.7" => Some(EFFORTS_WITH_XHIGH),
        "claude-opus-4.6" | "claude-sonnet-4.6" => Some(EFFORTS_WITHOUT_XHIGH),
        _ => None,
    }
}

/// 请求是否真的想要 reasoning（thinking 启用，或显式给了非空 effort）。
fn requested_native_reasoning(req: &MessagesRequest) -> bool {
    req.thinking.as_ref().is_some_and(|t| t.is_enabled())
        || req
            .output_config
            .as_ref()
            .is_some_and(|oc| !oc.effort.trim().is_empty())
}

/// budget_tokens → effort 档位（参考仓同款映射表）。
fn effort_from_budget_tokens(tokens: i32) -> &'static str {
    match tokens {
        i32::MIN..=4_000 => "low",
        4_001..=16_000 => "medium",
        16_001..=64_000 => "high",
        _ => "xhigh",
    }
}

/// 归一化 effort：trim + 小写，只认 low/medium/high/xhigh/max，否则回退 "high"。
fn normalize_thinking_effort(effort: &str) -> &'static str {
    match effort.trim().to_ascii_lowercase().as_str() {
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" => "xhigh",
        "max" => "max",
        _ => "high",
    }
}

/// 选档：显式 `output_config.effort` 优先（归一化后），否则 thinking 启用时按
/// budget_tokens 映射，都没给默认 "high"。白名单不允许的档位 → 回退档位表最后一个
/// （五档表最后是 "max"，四档表最后也是 "max"）。
///
/// ⚠️ 空 effort 视同「未给」：与 [`requested_native_reasoning`] 的判空口径一致
/// （那边 `effort.trim().is_empty()` = 没要求），否则 enabled thinking + 大 budget
/// 时客户端空 effort 会被归一化成 high，覆盖掉 budget 映射出的更高档。
fn select_native_reasoning_effort(
    req: &MessagesRequest,
    efforts: &'static [&'static str],
) -> &'static str {
    let requested = req
        .output_config
        .as_ref()
        .filter(|oc| !oc.effort.trim().is_empty())
        .map(|oc| normalize_thinking_effort(&oc.effort))
        .or_else(|| {
            req.thinking.as_ref().map(|t| {
                if t.thinking_type == "enabled" {
                    effort_from_budget_tokens(t.budget_tokens)
                } else {
                    normalize_thinking_effort("")
                }
            })
        })
        .unwrap_or_else(|| normalize_thinking_effort(""));
    if efforts.contains(&requested) {
        requested
    } else {
        efforts.last().copied().unwrap_or("high")
    }
}

/// native effort 总判定：开关 → thinking 未显式禁用 → 白名单 → 选档。
///
/// `build_additional_model_request_fields`（产出请求字段）与 build_history 的 XML 抑制
/// **共用本函数**，同一请求上两处判定恒一致，不会出现「发了字段还塞标签」或
/// 「剥了标签又没发字段」的分叉。
fn native_thinking_effort(req: &MessagesRequest, model_id: &str) -> Option<&'static str> {
    if !native_thinking_effort_enabled() {
        return None;
    }
    if req
        .thinking
        .as_ref()
        .is_some_and(|t| t.thinking_type == "disabled")
    {
        return None;
    }
    let efforts = native_reasoning_efforts(model_id)?;
    if !requested_native_reasoning(req) {
        return None;
    }
    Some(select_native_reasoning_effort(req, efforts))
}

/// 构建请求级 `additionalModelRequestFields`（native effort 通道）。
fn build_additional_model_request_fields(
    req: &MessagesRequest,
    model_id: &str,
) -> Option<AdditionalModelRequestFields> {
    let effort = native_thinking_effort(req, model_id)?;
    Some(AdditionalModelRequestFields {
        output_config: Some(KiroOutputConfig {
            effort: effort.to_string(),
        }),
    })
}

/// native 路径下抑制 XML 标签注入；否则原样走 [`generate_thinking_prefix`]。
///
/// 为什么必须抑制：实测塞 `<thinking_mode>` 标签既不触发上游 reasoningContentEvent，
/// 还会把标签文字污染进历史上下文（参考仓 converter.rs 1451-1455 的实测结论）。
/// 非 native 路径（开关关 / 非白名单）行为逐字节不变。
fn generate_thinking_prefix_for_model(req: &MessagesRequest, model_id: &str) -> Option<String> {
    if native_thinking_effort(req, model_id).is_some() {
        return None;
    }
    generate_thinking_prefix(req)
}

/// 检查内容是否已包含thinking标签
fn has_thinking_tags(content: &str) -> bool {
    content.contains("<thinking_mode>") || content.contains("<max_thinking_length>")
}

/// 构建历史消息
///
/// # Arguments
/// * `req` - 原始请求，用于读取 `system`、`thinking` 等配置字段
/// * `messages` - 经过 prefill 预处理的消息切片，末尾必定是 user 消息。
///   注意：该切片与 `req.messages` 可能不同（prefill 时会截断末尾的 assistant 消息），
///   调用方应始终使用此参数而非 `req.messages`。
/// * `model_id` - 已映射的 Kiro 模型 ID
fn build_history(
    req: &MessagesRequest,
    messages: &[super::types::Message],
    model_id: &str,
    tool_name_map: &mut HashMap<String, String>,
) -> Result<Vec<Message>, ConversionError> {
    let mut history = Vec::new();

    // 生成thinking前缀（如果需要）
    // native effort 路径（开关开 + 白名单 + thinking 启用，见
    // build_additional_model_request_fields 上方的说明）下不注入 XML 标签：
    // 实测只有 `additionalModelRequestFields.output_config.effort` 能触发上游
    // reasoningContentEvent，塞标签既不触发还污染历史上下文。
    let thinking_prefix = generate_thinking_prefix_for_model(req, model_id);

    // 1. 处理系统消息
    //
    // 先把 system 归一化成"有效文本或无"，再统一决策注入内容。
    // 这里刻意不用 `if let Some(system) = .. {} else if let Some(prefix) = ..`：
    // `system` 存在但归一化后为空（如 `"system": ""` 被 types.rs 的 visit_str 变成
    // `Some(vec![{text:""}])`，或整块被环境噪音剥空）时，外层分支已匹配，控制流永远到不了
    // else 分支 → thinking 前缀被静默丢弃，扩展思考不生效且无任何日志。
    let system_content: Option<String> = req.system.as_ref().and_then(|system| {
        // 归一化每一块 system 文本：折叠 CC 归因头（第一块，每请求漂移）+ 剥离环境噪音
        // （<env> 块 / gitStatus / Recent commits / 模型名行等，每请求漂移）。
        // 稳定住转发给上游的 prompt 前缀，避免 Bedrock prefix cache 因这些漂移而 0 命中，
        // 同时省 token、降 CC 身份被关联风险。空块（整块被剥空）直接丢弃不参与拼接。
        let joined: String = system
            .iter()
            .map(|s| canonicalize_system_text(&s.text).into_owned())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if joined.is_empty() {
            None
        } else {
            Some(joined)
        }
    });

    // 最终要写入首条 user 消息的内容：三种情形共用一个出口
    //   ① 有 system 有效文本 → [thinking 前缀 +] system + 分块策略
    //   ② 无 system 有效文本但有 thinking → 仅 thinking 前缀
    //   ③ 两者都无 → 不插入 system 配对
    let system_injection: Option<String> = match (system_content, thinking_prefix.as_ref()) {
        (Some(content), prefix) => {
            // 追加分块写入策略到系统消息
            let content = format!("{}\n{}", content, SYSTEM_CHUNKED_POLICY);
            // 注入thinking标签到系统消息最前面（如果需要且不存在）
            Some(match prefix {
                Some(p) if !has_thinking_tags(&content) => format!("{}\n{}", p, content),
                _ => content,
            })
        }
        (None, Some(prefix)) => {
            // 没有可用系统文本但有 thinking 配置，仅以 thinking 前缀插入系统消息
            Some(prefix.clone())
        }
        (None, None) => None,
    };

    if let Some(final_content) = system_injection {
        // 系统消息作为 user + assistant 配对
        let user_msg = HistoryUserMessage::new(final_content, model_id);
        history.push(Message::User(user_msg));

        let assistant_msg = HistoryAssistantMessage::new("I will follow these instructions.");
        history.push(Message::Assistant(assistant_msg));
    }

    // 2. 处理常规消息历史
    // 最后一条消息作为 currentMessage，不加入历史
    // 经过 prefill 预处理后，messages 末尾必定是 user，故直接截掉最后一条即可
    let history_end_index = messages.len().saturating_sub(1);

    // 收集并配对消息
    let mut user_buffer: Vec<&super::types::Message> = Vec::new();
    let mut assistant_buffer: Vec<&super::types::Message> = Vec::new();
    // 跨整个历史的图片 SHA256 去重集合：同一张图只在首次出现时保留 base64
    let mut image_dedup: std::collections::HashSet<String> = std::collections::HashSet::new();

    for i in 0..history_end_index {
        let msg = &messages[i];

        if msg.role == "user" {
            // 先处理累积的 assistant 消息
            if !assistant_buffer.is_empty() {
                let merged = merge_assistant_messages(&assistant_buffer, tool_name_map)?;
                history.push(Message::Assistant(merged));
                assistant_buffer.clear();
            }
            user_buffer.push(msg);
        } else if msg.role == "assistant" {
            // 先处理累积的 user 消息
            if !user_buffer.is_empty() {
                let merged_user = merge_user_messages(&user_buffer, model_id, &mut image_dedup)?;
                history.push(Message::User(merged_user));
                user_buffer.clear();
            }
            // 累积 assistant 消息（支持连续多条）
            assistant_buffer.push(msg);
        }
    }

    // 处理末尾累积的 assistant 消息
    if !assistant_buffer.is_empty() {
        let merged = merge_assistant_messages(&assistant_buffer, tool_name_map)?;
        history.push(Message::Assistant(merged));
    }

    // 处理结尾的孤立 user 消息
    if !user_buffer.is_empty() {
        let merged_user = merge_user_messages(&user_buffer, model_id, &mut image_dedup)?;
        history.push(Message::User(merged_user));

        // 自动配对一个 "OK" 的 assistant 响应
        let auto_assistant = HistoryAssistantMessage::new("OK");
        history.push(Message::Assistant(auto_assistant));
    }

    Ok(history)
}

/// 合并多个 user 消息
fn merge_user_messages(
    messages: &[&super::types::Message],
    model_id: &str,
    dedup: &mut std::collections::HashSet<String>,
) -> Result<HistoryUserMessage, ConversionError> {
    let mut content_parts = Vec::new();
    let mut all_images = Vec::new();
    let mut all_tool_results = Vec::new();

    for msg in messages {
        let (text, images, tool_results) =
            process_message_content_dedup(&msg.content, Some(dedup))?;
        if !text.is_empty() {
            content_parts.push(text);
        }
        all_images.extend(images);
        all_tool_results.extend(tool_results);
    }

    let content = content_parts.join("\n");
    // 保留文本内容，即使有工具结果也不丢弃用户文本
    let mut user_msg = UserMessage::new(&content, model_id);

    if !all_images.is_empty() {
        user_msg = user_msg.with_images(all_images);
    }

    if !all_tool_results.is_empty() {
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(all_tool_results);
        user_msg = user_msg.with_context(ctx);
    }

    Ok(HistoryUserMessage {
        user_input_message: user_msg,
    })
}

/// 转换 assistant 消息
fn convert_assistant_message(
    msg: &super::types::Message,
    tool_name_map: &mut HashMap<String, String>,
) -> Result<HistoryAssistantMessage, ConversionError> {
    let mut thinking_content = String::new();
    let mut text_content = String::new();
    let mut tool_uses = Vec::new();

    match &msg.content {
        serde_json::Value::String(s) => {
            text_content = s.clone();
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                if let Ok(block) = serde_json::from_value::<ContentBlock>(item.clone()) {
                    match block.block_type.as_str() {
                        "thinking" => {
                            if let Some(thinking) = block.thinking {
                                thinking_content.push_str(&thinking);
                            }
                        }
                        "text" => {
                            if let Some(text) = block.text {
                                text_content.push_str(&text);
                            }
                        }
                        "tool_use" => {
                            if let (Some(id), Some(name)) = (block.id, block.name) {
                                let input = block.input.unwrap_or(serde_json::json!({}));
                                // 历史 tool_use 也走 CC↔Kiro 映射：名（Write→fs_write）+ 参数
                                // （file_path→path、old_string→oldStr 等），否则多轮工具上下文
                                // 与当前轮的工具协议不一致。Read.pages 无 Kiro 等价 → 传播错误。
                                // 开关关闭时回退透传（仅超长缩短）。
                                let map_enabled = tool_compat_mapping_enabled();
                                let mapped_name = if map_enabled {
                                    map_client_tool_name_to_kiro(&name, tool_name_map)
                                } else {
                                    map_tool_name(&name, tool_name_map)
                                };
                                let input = if map_enabled {
                                    map_tool_input_to_kiro(&name, input)?
                                } else {
                                    input
                                };
                                tool_uses
                                    .push(ToolUseEntry::new(id, mapped_name).with_input(input));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    // 组合 thinking 和 text 内容
    // 格式: <thinking>思考内容</thinking>\n\ntext内容
    // 注意: Kiro API 要求 content 字段不能为空，当只有 tool_use 时需要占位符
    let final_content = if !thinking_content.is_empty() {
        if !text_content.is_empty() {
            format!(
                "<thinking>{}</thinking>\n\n{}",
                thinking_content, text_content
            )
        } else {
            format!("<thinking>{}</thinking>", thinking_content)
        }
    } else if text_content.is_empty() && !tool_uses.is_empty() {
        " ".to_string()
    } else {
        text_content
    };

    let mut assistant = AssistantMessage::new(final_content);
    if !tool_uses.is_empty() {
        assistant = assistant.with_tool_uses(tool_uses);
    }

    Ok(HistoryAssistantMessage {
        assistant_response_message: assistant,
    })
}

/// 合并多个连续的 assistant 消息为一条
/// 用于处理网络不稳定时产生的连续 assistant 消息（Issue #79）
fn merge_assistant_messages(
    messages: &[&super::types::Message],
    tool_name_map: &mut HashMap<String, String>,
) -> Result<HistoryAssistantMessage, ConversionError> {
    assert!(!messages.is_empty());
    if messages.len() == 1 {
        return convert_assistant_message(messages[0], tool_name_map);
    }

    let mut all_tool_uses: Vec<ToolUseEntry> = Vec::new();
    let mut content_parts: Vec<String> = Vec::new();

    for msg in messages {
        let converted = convert_assistant_message(msg, tool_name_map)?;
        let am = converted.assistant_response_message;
        if !am.content.trim().is_empty() {
            content_parts.push(am.content);
        }
        if let Some(tus) = am.tool_uses {
            all_tool_uses.extend(tus);
        }
    }

    let content = if content_parts.is_empty() && !all_tool_uses.is_empty() {
        " ".to_string()
    } else {
        content_parts.join("\n\n")
    };

    let mut assistant = AssistantMessage::new(content);
    if !all_tool_uses.is_empty() {
        assistant = assistant.with_tool_uses(all_tool_uses);
    }
    Ok(HistoryAssistantMessage {
        assistant_response_message: assistant,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_model_sonnet() {
        assert!(
            map_model("claude-sonnet-4-20250514")
                .unwrap()
                .contains("sonnet")
        );
        assert!(
            map_model("claude-3-5-sonnet-20241022")
                .unwrap()
                .contains("sonnet")
        );
    }

    /// 短板3：按字符边界截断，多字节不被切坏；max==0 = 不截断。
    #[test]
    fn test_truncate_chars_boundary_safe() {
        // 5 个中文（各 3 字节）→ 截到 3 字符应得前 3 个字，且是合法 UTF-8（未切多字节）。
        let s = "你好世界啊";
        let out = truncate_chars(s, 3);
        assert_eq!(out, "你好世");
        assert_eq!(out.chars().count(), 3);
        // 短于上限 → 原样。
        assert_eq!(truncate_chars("abc", 10), "abc");
        // max==0 → 不截断（原样返回）。
        assert_eq!(truncate_chars(s, 0), s);
    }

    /// 短板3：schema 内嵌上限恒为顶层的 1/5（保持既有 10000→2000 比例），0 时同样为 0。
    #[test]
    fn test_schema_desc_ratio_derives_from_top() {
        // 不改全局镜像（并行测试污染风险），只验证默认镜像值下的派生比例。
        let top = tool_description_max_chars();
        let schema = schema_description_max_chars();
        if top == 0 {
            assert_eq!(schema, 0);
        } else {
            assert_eq!(schema, (top / 5).max(1));
        }
        // 默认镜像应为 10000（与 config 默认一致）→ schema 2000。
        assert_eq!(top, 10000);
        assert_eq!(schema, 2000);
    }

    /// T1 回归：带每请求漂移 cc_version/cch 的归因头，归一化后 system 转发字节应相同。
    #[test]
    fn test_billing_header_canonicalized_in_forwarded_system() {
        use super::super::types::{Message as AnthropicMessage, SystemMessage};

        let mk_req = |header: &str| MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("hello"),
            }],
            stream: false,
            system: Some(vec![
                SystemMessage {
                    text: header.to_string(),
                    block_type: Some("text".to_string()),
                    cache_control: None,
                },
                SystemMessage {
                    text: "You are a helpful assistant.".to_string(),
                    block_type: Some("text".to_string()),
                    cache_control: None,
                },
            ]),
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        // 两个请求的归因头 cc_version / cch 不同（每请求漂移）
        let req_a = mk_req("x-anthropic-billing-header: cc_version=1.0.0;cch=aaaaaaaa");
        let req_b = mk_req("x-anthropic-billing-header: cc_version=2.5.9;cch=zzzzzzzz");

        // build_history 的第一条 user 消息即拼接后的 system 内容
        let extract_system = |req: &MessagesRequest| -> String {
            let history =
                build_history(req, &req.messages, "claude-sonnet-4.5", &mut HashMap::new())
                    .unwrap();
            match &history[0] {
                Message::User(u) => u.user_input_message.content.clone(),
                _ => panic!("首条历史应为 system 对应的 user 消息"),
            }
        };

        let sys_a = extract_system(&req_a);
        let sys_b = extract_system(&req_b);

        // 归一化后前缀稳定：两个请求转发给上游的 system 字节完全相同
        assert_eq!(sys_a, sys_b, "归因头归一化后 system 转发字节应一致");
        // 占位符出现在最前端，漂移字段不再泄漏到转发字节里
        assert!(sys_a.starts_with(BILLING_HEADER_PLACEHOLDER));
        assert!(!sys_a.contains("cc_version"));
        assert!(sys_a.contains("You are a helpful assistant."));
    }

    #[test]
    fn test_billing_header_non_matching_untouched() {
        // 保守性：非归因头开头的 system 内容不应被改动
        assert_eq!(
            canonicalize_billing_header("You are a helpful assistant."),
            "You are a helpful assistant."
        );
        assert_eq!(
            canonicalize_billing_header("x-anthropic-billing-header: cc_version=1;cch=x"),
            BILLING_HEADER_PLACEHOLDER
        );
    }

    // ===== 环境噪音剥离 prompt_filter =====

    /// 环境噪音开关是进程级全局，测试并行会相互污染。用一把静态锁串行所有触碰该开关的
    /// 用例，并在守卫里恢复原值，既消除竞态又不影响其它测试。
    static ENV_NOISE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvNoiseGuard {
        prev: bool,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl EnvNoiseGuard {
        fn with(enabled: bool) -> Self {
            let lock = ENV_NOISE_TEST_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let prev = strip_env_noise_enabled();
            set_strip_env_noise(enabled);
            EnvNoiseGuard { prev, _lock: lock }
        }
        fn enable() -> Self {
            Self::with(true)
        }
    }
    impl Drop for EnvNoiseGuard {
        fn drop(&mut self) {
            set_strip_env_noise(self.prev);
        }
    }

    #[test]
    fn test_strip_env_noise_removes_env_block() {
        let _g = EnvNoiseGuard::enable();
        // <env> 块整块剥离，稳定正文保留
        let text = "You are a helpful assistant.\n<env>\nWorking directory: /home/a\nPlatform: linux\nToday's date: 2026-07-09\n</env>\nFollow the task.";
        let out = canonicalize_system_text(text);
        assert!(!out.contains("<env>"), "env 起始标签应被剥离");
        assert!(!out.contains("Working directory"), "cwd 行应被剥离");
        assert!(!out.contains("Today's date"), "日期行应被剥离");
        assert!(
            out.contains("You are a helpful assistant."),
            "稳定正文应保留"
        );
        assert!(out.contains("Follow the task."), "env 后正文应保留");
    }

    #[test]
    fn test_strip_env_noise_removes_git_and_model_lines() {
        let _g = EnvNoiseGuard::enable();
        let text = "System prompt body.\ngitStatus: main clean\nRecent commits: abc123 fix\nYou are powered by the model named Claude.\nKeep going.";
        let out = canonicalize_system_text(text);
        assert!(!out.contains("gitStatus:"));
        assert!(!out.contains("Recent commits:"));
        assert!(!out.contains("powered by the model named"));
        assert!(out.contains("System prompt body."));
        assert!(out.contains("Keep going."));
    }

    #[test]
    fn test_strip_env_noise_removes_environment_section() {
        let _g = EnvNoiseGuard::enable();
        // # Environment 段剥到下一个 # 标题为止，后续标题及正文保留
        let text =
            "# Task\nDo the work.\n# Environment\nfoo\nbar\ngitStatus: x\n# Rules\nBe concise.";
        let out = canonicalize_system_text(text);
        assert!(out.contains("# Task"));
        assert!(out.contains("Do the work."));
        assert!(!out.contains("# Environment"));
        assert!(!out.contains("foo"));
        assert!(!out.contains("bar"));
        assert!(out.contains("# Rules"), "环境段后的新标题应保留");
        assert!(out.contains("Be concise."));
    }

    #[test]
    fn test_strip_env_noise_stable_content_untouched() {
        let _g = EnvNoiseGuard::enable();
        // 纯稳定正文：无任何噪音标记 → 原样借用不改写
        let text =
            "You are an expert engineer.\nWrite clean, tested code.\nExplain your reasoning.";
        let out = canonicalize_system_text(text);
        assert_eq!(out.as_ref(), text, "稳定正文一字节不改");
        assert!(
            matches!(out, std::borrow::Cow::Borrowed(_)),
            "未改写应零分配借用"
        );
    }

    #[test]
    fn test_strip_env_noise_disabled_keeps_noise() {
        // 开关关闭时不剥离环境噪音（但归因头折叠仍无条件生效）
        let _g = EnvNoiseGuard::with(false);
        let text = "Body.\ngitStatus: main\n<env>\ncwd\n</env>";
        let out = canonicalize_system_text(text);
        assert_eq!(out.as_ref(), text, "关闭时环境噪音应原样保留");
    }

    /// 转发字节路径（canonicalize_system_text）正确剥离环境噪音。
    /// （原先还与影子指纹路径 cache_tracker 做一致性比对；影子缓存记账已整体移除，
    ///   此处只保留转发路径本身的归一化回归。）
    #[test]
    fn test_forward_canonicalization_strips_env_noise() {
        let _g = EnvNoiseGuard::enable();
        let raw = "You are a helpful assistant.\n<env>\nWorking directory: /x\nPlatform: linux\n</env>\ngitStatus: clean\nDo the task.";

        let forwarded = canonicalize_system_text(raw).into_owned();

        assert!(!forwarded.contains("Working directory"));
        assert!(!forwarded.contains("gitStatus:"));
        assert!(forwarded.contains("You are a helpful assistant."));
        assert!(forwarded.contains("Do the task."));
    }

    #[test]
    fn test_env_noise_drift_produces_identical_forwarded_system() {
        use super::super::types::{Message as AnthropicMessage, SystemMessage};
        let _g = EnvNoiseGuard::enable();

        // 两次请求：env 块里的 cwd/日期漂移，稳定正文相同
        let mk = |env_line: &str| MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("hi"),
            }],
            stream: false,
            system: Some(vec![SystemMessage {
                text: format!(
                    "You are Claude Code.\n<env>\n{}\nPlatform: win32\n</env>\nHelp the user.",
                    env_line
                ),
                block_type: Some("text".to_string()),
                cache_control: None,
            }]),
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };
        let req_a = mk("Working directory: /home/a  (2026-07-08)");
        let req_b = mk("Working directory: /home/b  (2026-07-09)");

        let extract = |req: &MessagesRequest| -> String {
            let history =
                build_history(req, &req.messages, "claude-sonnet-4.5", &mut HashMap::new())
                    .unwrap();
            match &history[0] {
                Message::User(u) => u.user_input_message.content.clone(),
                _ => panic!("首条历史应为 system 对应的 user 消息"),
            }
        };
        let sys_a = extract(&req_a);
        let sys_b = extract(&req_b);

        assert_eq!(sys_a, sys_b, "env 漂移剥离后转发字节应一致");
        assert!(
            !sys_a.contains("Working directory"),
            "漂移的 cwd 不应泄漏到转发字节"
        );
        assert!(sys_a.contains("Help the user."), "稳定正文应保留");
    }

    /// 构造只有一条 user 消息的最小请求，system/thinking 可控。
    fn mk_thinking_req(
        system: Option<Vec<super::super::types::SystemMessage>>,
        thinking: Option<super::super::types::Thinking>,
    ) -> MessagesRequest {
        use super::super::types::Message as AnthropicMessage;
        MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("hi"),
            }],
            stream: false,
            system,
            tools: None,
            tool_choice: None,
            thinking,
            output_config: None,
            metadata: None,
        }
    }

    /// 构造只控制「工作上下文」（system 文本 + 工具名）的最小请求，供 L0-5 派生用例使用。
    fn req_with_context(system: Option<&str>, tool_names: &[&str]) -> MessagesRequest {
        use super::super::types::Message as AnthropicMessage;
        use super::super::types::{SystemMessage, Tool};
        MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("hi"),
            }],
            stream: false,
            system: system.map(|t| {
                vec![SystemMessage {
                    text: t.to_string(),
                    block_type: Some("text".to_string()),
                    cache_control: None,
                }]
            }),
            tools: if tool_names.is_empty() {
                None
            } else {
                Some(
                    tool_names
                        .iter()
                        .map(|n| Tool {
                            tool_type: None,
                            name: n.to_string(),
                            description: String::new(),
                            input_schema: HashMap::new(),
                            cache_control: None,
                            max_uses: None,
                        })
                        .collect(),
                )
            },
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        }
    }

    fn first_history_user_content(req: &MessagesRequest) -> Option<String> {
        let history =
            build_history(req, &req.messages, "claude-sonnet-4.5", &mut HashMap::new()).unwrap();
        match history.first() {
            Some(Message::User(u)) => Some(u.user_input_message.content.clone()),
            _ => None,
        }
    }

    /// 回归：`"system": ""` 经 types.rs 的 visit_str 变成 `Some(vec![{text:""}])`，
    /// 归一化后为空。旧代码外层 `if let Some(system)` 已匹配、内层 is_empty 跳过，
    /// 控制流到不了 else 分支 → thinking 前缀被静默丢弃。修复后必须仍注入。
    #[test]
    fn should_inject_thinking_prefix_when_system_is_empty_string() {
        use super::super::types::SystemMessage;

        let req = mk_thinking_req(
            Some(vec![SystemMessage {
                text: String::new(),
                block_type: Some("text".to_string()),
                cache_control: None,
            }]),
            Some(super::super::types::Thinking {
                thinking_type: "enabled".to_string(),
                budget_tokens: 8192,
            }),
        );

        let content = first_history_user_content(&req)
            .expect("system 空但 thinking 开启时，首条历史应为注入 thinking 前缀的 user 消息");
        assert!(
            has_thinking_tags(&content),
            "thinking 前缀必须注入，实际内容：{content}"
        );
        assert!(content.contains("<max_thinking_length>8192</max_thinking_length>"));
        // 无有效 system 文本时不应附带分块策略（保持与 system=None 路径一致）
        assert!(!content.contains(SYSTEM_CHUNKED_POLICY));
    }

    /// 回归：system 整块是环境噪音，剥离后为空 —— 同一条控制流缺陷的第二条触发路径。
    #[test]
    fn should_inject_thinking_prefix_when_system_stripped_to_empty_by_env_noise() {
        use super::super::types::SystemMessage;
        let _g = EnvNoiseGuard::enable();

        let req = mk_thinking_req(
            Some(vec![SystemMessage {
                text: "<env>\nWorking directory: /home/a\nPlatform: linux\n</env>".to_string(),
                block_type: Some("text".to_string()),
                cache_control: None,
            }]),
            Some(super::super::types::Thinking {
                thinking_type: "adaptive".to_string(),
                budget_tokens: 0,
            }),
        );

        let content = first_history_user_content(&req)
            .expect("system 被剥空但 thinking 开启时，首条历史应为 thinking 前缀");
        assert!(has_thinking_tags(&content), "实际内容：{content}");
        assert!(content.contains("<thinking_effort>high</thinking_effort>"));
    }

    /// 正常路径不变：有有效 system + thinking → 前缀在最前，system 正文与分块策略都在。
    #[test]
    fn derived_conversation_id_is_stable_across_requests() {
        // 同一工作上下文（system + tools 不变）必须派生出同一个键 —— 这正是 L0-5 的目的。
        let a = req_with_context(Some("you are a helpful agent"), &["read", "write"]);
        let b = req_with_context(Some("you are a helpful agent"), &["read", "write"]);
        let ka = derive_conversation_id_from_context(&a).expect("应能派生");
        let kb = derive_conversation_id_from_context(&b).expect("应能派生");
        assert_eq!(ka, kb, "同上下文必须稳定派生同一键，否则等于没修");
        assert!(is_valid_uuid(&ka), "必须是 UUID 形状：下游与上游都按此消费");
    }

    #[test]
    fn derived_conversation_id_ignores_tool_order() {
        // 官方自认造过「工具排序非确定」的事故；不排序会让同上下文分裂成多个键。
        let a = req_with_context(Some("sys"), &["alpha", "beta", "gamma"]);
        let b = req_with_context(Some("sys"), &["gamma", "alpha", "beta"]);
        assert_eq!(
            derive_conversation_id_from_context(&a),
            derive_conversation_id_from_context(&b),
            "工具名顺序抖动不得改变派生键"
        );
    }

    #[test]
    fn derived_conversation_id_separates_distinct_contexts() {
        let a = req_with_context(Some("agent A"), &["read"]);
        let b = req_with_context(Some("agent B"), &["read"]);
        assert_ne!(
            derive_conversation_id_from_context(&a),
            derive_conversation_id_from_context(&b),
            "不同工作上下文必须隔离，否则无关请求会互相污染上游会话"
        );
    }

    #[test]
    fn derived_conversation_id_resists_concat_ambiguity() {
        // 无分隔符时 ["ab","c"] 与 ["a","bc"] 会哈希成同一串。
        let a = req_with_context(None, &["ab", "c"]);
        let b = req_with_context(None, &["a", "bc"]);
        assert_ne!(
            derive_conversation_id_from_context(&a),
            derive_conversation_id_from_context(&b),
            "拼接歧义必须由分隔符消除"
        );
    }

    #[test]
    fn derived_conversation_id_is_none_without_material() {
        // system 与 tools 双空：没有可稳定的前缀，应回落随机而非归到同一个键。
        let empty = req_with_context(None, &[]);
        assert!(
            derive_conversation_id_from_context(&empty).is_none(),
            "无材料时必须返回 None，让调用方回落随机 UUID"
        );
        let blank = req_with_context(Some("   "), &[]);
        assert!(
            derive_conversation_id_from_context(&blank).is_none(),
            "纯空白 system 不算材料"
        );
    }

    #[test]
    fn derived_conversation_id_survives_env_noise_drift() {
        // 关键回归：工作目录/日期漂移不得打散键。不复用 canonicalize_system_text
        // 就会在这里失败，而那等于 L0-5 没修。
        let a = req_with_context(
            Some("stable instructions\n<env>cwd: /home/a\ntoday: 2026-08-04</env>"),
            &["read"],
        );
        let b = req_with_context(
            Some("stable instructions\n<env>cwd: /home/b\ntoday: 2026-08-05</env>"),
            &["read"],
        );
        assert_eq!(
            derive_conversation_id_from_context(&a),
            derive_conversation_id_from_context(&b),
            "环境噪音漂移必须被归一化吸收"
        );
    }

    #[test]
    fn explicit_session_id_wins_over_derivation() {
        // 回落顺序不能反：Claude Code 给了 session_id 就必须用它。
        let mut req = req_with_context(Some("sys"), &["read"]);
        let sid = "11111111-2222-3333-4444-555555555555";
        req.metadata = Some(super::super::types::Metadata {
            user_id: Some(format!("user_x_session_{sid}")),
        });
        let result = convert_request(&req).expect("转换应成功");
        let derived = derive_conversation_id_from_context(&req).expect("应能派生");
        assert_ne!(
            result.conversation_state.conversation_id, derived,
            "显式 session_id 优先于上下文派生"
        );
        assert_eq!(result.conversation_state.conversation_id, sid);
    }

    #[test]
    fn should_keep_thinking_prefix_ahead_of_non_empty_system() {
        use super::super::types::SystemMessage;

        let req = mk_thinking_req(
            Some(vec![SystemMessage {
                text: "You are a helpful assistant.".to_string(),
                block_type: Some("text".to_string()),
                cache_control: None,
            }]),
            Some(super::super::types::Thinking {
                thinking_type: "enabled".to_string(),
                budget_tokens: 1024,
            }),
        );

        let content = first_history_user_content(&req).expect("首条历史应为 system 对应的 user");
        assert!(content.starts_with("<thinking_mode>enabled</thinking_mode>"));
        assert!(content.contains("You are a helpful assistant."));
        assert!(content.contains(SYSTEM_CHUNKED_POLICY));
    }

    /// 两者都无时不插入 system 配对（首条历史不再是 system 伪装的 user）。
    #[test]
    fn should_not_inject_system_pair_when_system_empty_and_thinking_off() {
        use super::super::types::SystemMessage;

        let req = mk_thinking_req(
            Some(vec![SystemMessage {
                text: String::new(),
                block_type: Some("text".to_string()),
                cache_control: None,
            }]),
            None,
        );

        let history = build_history(
            &req,
            &req.messages,
            "claude-sonnet-4.5",
            &mut HashMap::new(),
        )
        .unwrap();
        // 只有一条 user 消息 → 作为 currentMessage 不入历史，历史应为空
        assert!(
            history.is_empty(),
            "无 system 无 thinking 时不应插入任何历史"
        );
    }

    #[test]
    fn test_map_model_sonnet_variants() {
        assert!(
            map_model("claude-3-5-sonnet-20241022")
                .unwrap()
                .contains("sonnet")
        );
    }

    #[test]
    fn test_map_model_opus() {
        assert!(
            map_model("claude-opus-4-20250514")
                .unwrap()
                .contains("opus")
        );
    }

    #[test]
    fn test_map_model_haiku() {
        assert!(
            map_model("claude-haiku-4-20250514")
                .unwrap()
                .contains("haiku")
        );
    }

    #[test]
    fn test_map_model_unsupported() {
        assert!(map_model("gpt-4").is_none());
        // 仍不支持的：gemini / 未知
        assert!(map_model("gemini-2.0").is_none());
    }

    #[test]
    fn test_map_model_national() {
        // 模糊名 → 规范 kiro modelId
        assert_eq!(map_model("deepseek"), Some("deepseek-3.2".to_string()));
        assert_eq!(map_model("glm"), Some("glm-5".to_string()));
        assert_eq!(map_model("qwen"), Some("qwen3-coder-next".to_string()));
        assert_eq!(map_model("minimax"), Some("minimax-m2.5".to_string()));
        // 完整原生 id 直透（含子串，映射回自身）
        assert_eq!(map_model("deepseek-3.2"), Some("deepseek-3.2".to_string()));
        assert_eq!(map_model("glm-5"), Some("glm-5".to_string()));
        assert_eq!(
            map_model("qwen3-coder-next"),
            Some("qwen3-coder-next".to_string())
        );
        assert_eq!(map_model("minimax-m2.5"), Some("minimax-m2.5".to_string()));
        // minimax 版本细分
        assert_eq!(map_model("minimax-m2.1"), Some("minimax-m2.1".to_string()));
        // 大小写不敏感
        assert_eq!(map_model("DeepSeek"), Some("deepseek-3.2".to_string()));
        // 国产模型窗口 = 200k（非 1M）
        assert_eq!(get_context_window_size("deepseek-3.2"), 128_000); // 官方 128K
        assert_eq!(get_context_window_size("glm-5"), 200_000);
    }

    #[test]
    fn test_map_model_thinking_suffix_sonnet() {
        // thinking 后缀不应影响 sonnet 模型映射
        let result = map_model("claude-sonnet-4-5-20250929-thinking");
        assert_eq!(result, Some("claude-sonnet-4.5".to_string()));
    }

    #[test]
    fn test_map_model_thinking_suffix_opus_4_5() {
        // thinking 后缀不应影响 opus 4.5 模型映射
        let result = map_model("claude-opus-4-5-20251101-thinking");
        assert_eq!(result, Some("claude-opus-4.5".to_string()));
    }

    #[test]
    fn test_map_model_thinking_suffix_opus_4_6() {
        // thinking 后缀不应影响 opus 4.6 模型映射
        let result = map_model("claude-opus-4-6-thinking");
        assert_eq!(result, Some("claude-opus-4.6".to_string()));
    }

    #[test]
    fn test_map_model_opus_4_8() {
        assert_eq!(
            map_model("claude-opus-4-8"),
            Some("claude-opus-4.8".to_string())
        );
        assert_eq!(
            map_model("claude-opus-4-8-thinking"),
            Some("claude-opus-4.8".to_string())
        );
        assert_eq!(get_context_window_size("claude-opus-4-8"), 1_000_000);
    }

    #[test]
    fn test_map_model_thinking_suffix_haiku() {
        // thinking 后缀不应影响 haiku 模型映射
        let result = map_model("claude-haiku-4-5-20251001-thinking");
        assert_eq!(result, Some("claude-haiku-4.5".to_string()));
    }

    #[test]
    fn test_determine_chat_trigger_type() {
        // 无工具时返回 MANUAL
        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };
        assert_eq!(determine_chat_trigger_type(&req), "MANUAL");
    }

    #[test]
    fn test_collect_history_tool_names() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 创建包含工具使用的历史消息
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
            ToolUseEntry::new("tool-2", "write")
                .with_input(serde_json::json!({"path": "/out.txt"})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        let tool_names = collect_history_tool_names(&history);
        assert_eq!(tool_names.len(), 2);
        assert!(tool_names.contains(&"read".to_string()));
        assert!(tool_names.contains(&"write".to_string()));
    }

    #[test]
    fn test_create_placeholder_tool() {
        let tool = create_placeholder_tool("my_custom_tool");

        assert_eq!(tool.tool_specification.name, "my_custom_tool");
        assert!(!tool.tool_specification.description.is_empty());

        // 验证 JSON 序列化正确
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("\"name\":\"my_custom_tool\""));
    }

    #[test]
    fn test_shorten_tool_name_deterministic() {
        let long_name =
            "mcp__some_very_long_server_name__some_very_long_tool_name_that_exceeds_limit";
        assert!(long_name.len() > TOOL_NAME_MAX_LEN);

        let short1 = shorten_tool_name(long_name);
        let short2 = shorten_tool_name(long_name);
        assert_eq!(short1, short2, "相同输入应产生相同的短名称");
        assert!(
            short1.len() <= TOOL_NAME_MAX_LEN,
            "短名称长度应 <= 63，实际 {}",
            short1.len()
        );
    }

    #[test]
    fn test_map_tool_name_cjk_never_exceeds_limit() {
        // ⭐回归(旧代码必失败):超限判断用字节数、前缀截取用字符数,两者单位不一致。
        // 30 个汉字 = 90 字节 > 63 → 触发缩短;但 char_indices().nth(54) 在只有 30 字符时
        // 返回 None → prefix 取整个名字 → 结果 90+1+8 = 99 字节,**比原名更长且仍超上限**,
        // 上游 Kiro 会回 400 Improperly formed request。
        // 修复后前缀按 chars().take(54) 截取,短名恒为 ASCII 且 ≤63 字节。
        let mut map = HashMap::new();
        for n in [20usize, 22, 30, 40, 60, 100, 200] {
            let cjk_name: String = "工".repeat(n);
            let short = map_tool_name(&cjk_name, &mut map);
            assert!(
                short.len() <= TOOL_NAME_MAX_LEN,
                "{n} 个汉字({} 字节)的工具名缩短后为 {} 字节(>{}上限): {:?}",
                cjk_name.len(),
                short.len(),
                TOOL_NAME_MAX_LEN,
                short
            );
            if cjk_name.len() > TOOL_NAME_MAX_LEN {
                assert!(
                    short.len() < cjk_name.len(),
                    "缩短后必须比原名更短,否则毫无意义(原 {} 字节 → 短 {} 字节)",
                    cjk_name.len(),
                    short.len()
                );
                assert_eq!(
                    map.get(&short).map(String::as_str),
                    Some(cjk_name.as_str()),
                    "必须登记 short→original 映射,否则 stream 层无法还原成客户端原名"
                );
            }
        }
    }

    #[test]
    fn test_map_tool_name_mixed_width_boundary() {
        // 混合宽度(ASCII + CJK)在 63 字节边界附近:凡触发缩短的,结果都必须 ≤63 字节。
        let mut map = HashMap::new();
        for ascii_len in 0..8usize {
            for cjk_len in 18..26usize {
                let name = format!("{}{}", "a".repeat(ascii_len), "文".repeat(cjk_len));
                let short = map_tool_name(&name, &mut map);
                assert!(
                    short.len() <= TOOL_NAME_MAX_LEN,
                    "name({} 字节) → short({} 字节) 超限",
                    name.len(),
                    short.len()
                );
                // 未超限的名字必须原样返回(不该被无谓改写)。
                if name.len() <= TOOL_NAME_MAX_LEN {
                    assert_eq!(short, name, "未超限的工具名不应被改写");
                }
            }
        }
    }

    #[test]
    fn test_shorten_tool_name_uniqueness() {
        let name_a = "mcp__server_alpha__tool_name_that_is_very_long_and_exceeds_the_limit_a";
        let name_b = "mcp__server_alpha__tool_name_that_is_very_long_and_exceeds_the_limit_b";
        let short_a = shorten_tool_name(name_a);
        let short_b = shorten_tool_name(name_b);
        assert_ne!(short_a, short_b, "不同输入应产生不同的短名称");
    }

    #[test]
    fn test_map_tool_name_short_passthrough() {
        let mut map = HashMap::new();
        let result = map_tool_name("short_name", &mut map);
        assert_eq!(result, "short_name");
        assert!(map.is_empty(), "短名称不应产生映射");
    }

    #[test]
    fn test_map_tool_name_long_creates_mapping() {
        let mut map = HashMap::new();
        let long_name = "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";
        let result = map_tool_name(long_name, &mut map);
        assert!(result.len() <= TOOL_NAME_MAX_LEN);
        assert_eq!(map.get(&result), Some(&long_name.to_string()));
    }

    #[test]
    fn test_tool_name_mapping_in_convert_request() {
        use super::super::types::{Message as AnthropicMessage, Tool as AnthropicTool};

        let long_tool_name =
            "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";
        assert!(long_tool_name.len() > TOOL_NAME_MAX_LEN);

        let mut schema = std::collections::HashMap::new();
        schema.insert("type".to_string(), serde_json::json!("object"));
        schema.insert("properties".to_string(), serde_json::json!({}));

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            system: None,
            stream: false,
            tools: Some(vec![AnthropicTool {
                name: long_tool_name.to_string(),
                description: "A test tool".to_string(),
                input_schema: schema,
                tool_type: None,
                max_uses: None,
                cache_control: None,
            }]),
            thinking: None,
            tool_choice: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req).unwrap();

        // 应该有映射
        assert_eq!(result.tool_name_map.len(), 1);

        // 映射中的值应该是原始名称
        let (short, original) = result.tool_name_map.iter().next().unwrap();
        assert_eq!(original, long_tool_name);
        assert!(short.len() <= TOOL_NAME_MAX_LEN);

        // Kiro 请求中的工具名应该是短名称
        let tools = &result
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .tools;
        assert_eq!(tools[0].tool_specification.name, *short);
    }

    #[test]
    fn test_convert_tools_strips_web_search_in_mixed_list() {
        use super::super::types::Tool as AnthropicTool;

        let mk = |name: &str, ty: Option<&str>| AnthropicTool {
            name: name.to_string(),
            description: String::new(),
            input_schema: std::collections::HashMap::new(),
            tool_type: ty.map(|s| s.to_string()),
            max_uses: None,
            cache_control: None,
        };

        // 🔴 2026-08-09 行为变更：web_search（带 type）在混合列表里**不再剥离**，
        // 而是归一化成 Kiro 认的函数工具形态（`name: web_search` + 内置 schema）。
        // 改前 assert 它被剥离 —— 那是导致 CC WebSearch 静默失效的行为。
        let tools = Some(vec![
            mk("web_search", Some("web_search_20250305")),
            mk("Read", None),
            mk("Write", None),
        ]);
        let mut map = HashMap::new();
        let converted = convert_tools(&tools, &mut map);

        let names: Vec<&str> = converted
            .iter()
            .map(|t| t.tool_specification.name.as_str())
            .collect();
        // 现在应是 3 个：web_search（归一化后）+ read_file + fs_write。
        assert_eq!(names.len(), 3, "web_search 不应被剥离，应归一化保留: {names:?}");
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"fs_write"));
        assert!(
            names.contains(&"web_search"),
            "web_search 必须归一化为 name=web_search 的函数工具（模型需要能看到搜索能力）"
        );
    }

    #[test]
    fn test_convert_tools_regular_tool_unaffected() {
        use super::super::types::Tool as AnthropicTool;

        let tools = Some(vec![AnthropicTool {
            name: "Read".to_string(),
            description: String::new(),
            input_schema: std::collections::HashMap::new(),
            tool_type: None,
            max_uses: None,
            cache_control: None,
        }]);
        let mut map = HashMap::new();
        let converted = convert_tools(&tools, &mut map);
        assert_eq!(converted.len(), 1);
        // Read → read_file（CC↔Kiro 映射层）；tool_name_map 记录反向映射供出站还原。
        assert_eq!(converted[0].tool_specification.name, "read_file");
        assert_eq!(
            map.get("read_file").map(|s| s.as_str()),
            Some("Read"),
            "应记录 Kiro名→Claude Code 名的反向映射"
        );
    }

    /// CC↔Kiro 映射：Write → fs_write，且 schema 换成 Kiro 原生参数形态（path/text）。
    #[test]
    fn test_convert_tools_maps_builtin_to_kiro_schema() {
        use super::super::types::Tool as AnthropicTool;

        let tools = Some(vec![AnthropicTool {
            name: "Write".to_string(),
            description: String::new(),
            input_schema: std::collections::HashMap::new(),
            tool_type: None,
            max_uses: None,
            cache_control: None,
        }]);
        let mut map = HashMap::new();
        let converted = convert_tools(&tools, &mut map);
        assert_eq!(converted.len(), 1);
        let spec = &converted[0].tool_specification;
        assert_eq!(spec.name, "fs_write");
        // schema 应为合成 schema：参数名已是 Kiro 形态（path/text），不是客户端 file_path/content。
        let schema: serde_json::Value = spec.input_schema.json.clone();
        let props = &schema["properties"];
        assert!(props.get("path").is_some(), "合成 schema 应有 path");
        assert!(props.get("text").is_some(), "合成 schema 应有 text");
        assert!(
            props.get("file_path").is_none(),
            "合成 schema 不应残留客户端 file_path"
        );
        // 反向映射已记录
        assert_eq!(map.get("fs_write").map(|s| s.as_str()), Some("Write"));
    }

    /// 入站参数转换：Claude Code 参数 → Kiro 参数（file_path→path、content→text、
    /// old_string→oldStr、offset/limit→start_line/end_line）。
    #[test]
    /// 🔴 `Read.pages` 必须**降级而非报错**（2026-08-10 修，线上实测缺陷）。
    ///
    /// 改前：带 `pages` 的 Read 直接 `Err(UnsupportedToolMapping)` ⇒ handlers 渲染成
    /// **400 `工具参数无法映射: Read — ...`** 并终结整个请求 ⇒ Claude Code 整轮对话失败。
    ///
    /// 为什么处置过重：`pages` 只是「读哪几页」的范围提示，丢掉它的后果是「整读」——
    /// 信息更多而非更少，模型能自己定位。拿它否决整轮请求，代价远大于收益。
    #[test]
    fn read_pages_degrades_instead_of_failing() {
        // ① 字符串页范围：不再 Err，且意图进了 explanation
        let out = map_tool_input_to_kiro(
            "Read",
            serde_json::json!({"file_path": "/a.pdf", "pages": "1-5"}),
        )
        .expect("带 pages 的 Read 不该再报错（旧代码在此 panic）");
        assert_eq!(out["path"], "/a.pdf", "路径必须照常映射");
        let expl = out["explanation"].as_str().unwrap_or_default();
        assert!(
            expl.contains("1-5"),
            "页范围意图必须落进 explanation，否则降级就是静默丢信息: {expl}"
        );
        assert!(
            !out.as_object().unwrap().contains_key("pages"),
            "pages 不该原样透传给 Kiro（它不认这个参数）"
        );

        // ② 数组形式（部分客户端版本）
        let out = map_tool_input_to_kiro(
            "Read",
            serde_json::json!({"file_path": "/b.pdf", "pages": [2, 3]}),
        )
        .expect("数组 pages 同样不该报错");
        assert!(out["explanation"].as_str().unwrap_or_default().contains("2,3"));

        // ③ pages 为 null / 缺失：行为与改前完全一致（不追加任何提示）
        for v in [
            serde_json::json!({"file_path": "/c.txt", "pages": null}),
            serde_json::json!({"file_path": "/c.txt"}),
        ] {
            let out = map_tool_input_to_kiro("Read", v).expect("无 pages 必须正常");
            assert!(
                !out["explanation"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("只关心第"),
                "没有 pages 时不该凭空追加页提示"
            );
        }

        // ④ 与既有 offset/limit 映射共存（pages 提示不能挤掉行范围）
        let out = map_tool_input_to_kiro(
            "Read",
            serde_json::json!({"file_path": "/d.txt", "pages": "7", "offset": 10, "limit": 5}),
        )
        .expect("共存不该报错");
        assert_eq!(out["start_line"], 10);
        assert_eq!(out["end_line"], 14);
        assert!(out["explanation"].as_str().unwrap_or_default().contains("7"));
    }

    #[test]
    fn test_map_tool_input_to_kiro_converts_params() {
        // Write：file_path→path, content→text
        let write_in = serde_json::json!({"file_path": "/a.txt", "content": "hi"});
        let write_out = map_tool_input_to_kiro("Write", write_in).unwrap();
        assert_eq!(
            write_out,
            serde_json::json!({"path": "/a.txt", "text": "hi"})
        );

        // Edit：old_string→oldStr, new_string→newStr
        let edit_in = serde_json::json!({"file_path": "/a.txt", "old_string": "x", "new_string": "y"});
        let edit_out = map_tool_input_to_kiro("Edit", edit_in).unwrap();
        assert_eq!(
            edit_out,
            serde_json::json!({"path": "/a.txt", "oldStr": "x", "newStr": "y"})
        );

        // Read：offset/limit→start_line/end_line
        let read_in = serde_json::json!({"file_path": "/a.txt", "offset": 10, "limit": 5});
        let read_out = map_tool_input_to_kiro("Read", read_in).unwrap();
        assert_eq!(
            read_out,
            serde_json::json!({"path": "/a.txt", "start_line": 10, "end_line": 14, "explanation": "Mapped from Claude Code Read tool."})
        );

        // 非内置工具原样
        let custom = serde_json::json!({"x": 1});
        assert_eq!(map_tool_input_to_kiro("my_tool", custom.clone()).unwrap(), custom);
    }

    /// 出站参数还原：Kiro 参数 → Claude Code 参数（path→file_path、oldStr→old_string、
    /// start_line/end_line→offset/limit）。
    #[test]
    fn test_map_tool_input_from_kiro_restores_params() {
        let kiro_in = serde_json::json!({"path": "/a.txt", "text": "hi"});
        let restored = map_tool_input_from_kiro("Write", kiro_in);
        assert_eq!(
            restored,
            serde_json::json!({"file_path": "/a.txt", "content": "hi"})
        );

        let kiro_edit = serde_json::json!({"path": "/a.txt", "oldStr": "x", "newStr": "y"});
        assert_eq!(
            map_tool_input_from_kiro("Edit", kiro_edit),
            serde_json::json!({"file_path": "/a.txt", "old_string": "x", "new_string": "y"})
        );

        let kiro_read = serde_json::json!({"path": "/a.txt", "start_line": 10, "end_line": 14});
        assert_eq!(
            map_tool_input_from_kiro("Read", kiro_read),
            serde_json::json!({"file_path": "/a.txt", "offset": 10, "limit": 5})
        );
    }

    /// 🔴 回归：Write write_mode 透传、Glob includeIgnoredFiles 出站还原 bool、
    /// Grep excludePattern→exclude 出站还原、注入的 explanation 出站剥离。
    #[test]
    fn test_tool_mapping_write_mode_and_glob_grep_roundtrip() {
        // Write write_mode 入站透传 + 出站保留（之前被静默丢弃 → 覆盖写数据丢失）
        let write_in = serde_json::json!({"file_path": "/a.txt", "content": "hi", "write_mode": "append"});
        let write_out = map_tool_input_to_kiro("Write", write_in).unwrap();
        assert_eq!(write_out["write_mode"], "append", "write_mode 必须透传（防退化成覆盖写）");
        let restored = map_tool_input_from_kiro("Write", write_out);
        assert_eq!(restored["write_mode"], "append", "write_mode 出站保留");
        assert_eq!(restored["file_path"], "/a.txt");

        // Glob includeIgnoredFiles：入站 bool→"yes"/"no"，出站必须还原回 bool
        let glob_in = serde_json::json!({"pattern": "*.ts", "includeIgnoredFiles": true});
        let glob_out = map_tool_input_to_kiro("Glob", glob_in).unwrap();
        assert_eq!(glob_out["includeIgnoredFiles"], "yes");
        let glob_restored = map_tool_input_from_kiro("Glob", glob_out);
        assert_eq!(glob_restored["includeIgnoredFiles"], true, "includeIgnoredFiles 必须还原回 bool");
        assert!(
            !glob_restored.as_object().unwrap().contains_key("explanation"),
            "入站注入的 explanation 出站必须剥离（幻影参数）"
        );

        // Grep excludePattern→exclude 出站还原
        let grep_in = serde_json::json!({"pattern": "foo", "exclude": "vendor"});
        let grep_out = map_tool_input_to_kiro("Grep", grep_in).unwrap();
        assert_eq!(grep_out["excludePattern"], "vendor");
        let grep_restored = map_tool_input_from_kiro("Grep", grep_out);
        assert_eq!(grep_restored["exclude"], "vendor", "excludePattern 必须还原成 exclude");
        assert!(
            !grep_restored.as_object().unwrap().contains_key("excludePattern"),
            "还原后不应残留 excludePattern"
        );

        // Read 出站剥离注入的 explanation
        let read_restored = map_tool_input_from_kiro(
            "Read",
            serde_json::json!({"path": "/a.txt", "start_line": 1, "end_line": 2}),
        );
        assert!(
            !read_restored.as_object().unwrap().contains_key("explanation"),
            "Read 出站剥离 explanation"
        );
    }

    /// 出站完整还原：Kiro 名 + 参数 → Claude Code 名 + 参数（fs_write + path → Write + file_path）。
    #[test]
    fn test_restore_tool_use_for_client_roundtrip() {
        let mut map = HashMap::new();
        map.insert("fs_write".to_string(), "Write".to_string());
        let (name, input) = restore_tool_use_for_client(
            "fs_write",
            serde_json::json!({"path": "/a.txt", "text": "hi"}),
            &map,
        );
        assert_eq!(name, "Write");
        assert_eq!(
            input,
            serde_json::json!({"file_path": "/a.txt", "content": "hi"})
        );
    }

    #[test]
    fn test_tool_name_mapping_in_history() {
        use super::super::types::{Message as AnthropicMessage, Tool as AnthropicTool};

        let long_tool_name =
            "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";

        let mut schema = std::collections::HashMap::new();
        schema.insert("type".to_string(), serde_json::json!("object"));
        schema.insert("properties".to_string(), serde_json::json!({}));

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("use the tool"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "calling tool"},
                        {"type": "tool_use", "id": "toolu_01", "name": long_tool_name, "input": {}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "toolu_01", "content": "done"}
                    ]),
                },
            ],
            system: None,
            stream: false,
            tools: Some(vec![AnthropicTool {
                name: long_tool_name.to_string(),
                description: "A test tool".to_string(),
                input_schema: schema,
                tool_type: None,
                max_uses: None,
                cache_control: None,
            }]),
            thinking: None,
            tool_choice: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req).unwrap();
        let short_name = result.tool_name_map.iter().next().unwrap().0.clone();

        // 历史中 assistant 消息的 tool_use name 也应该被映射
        let history = &result.conversation_state.history;
        let mut found = false;
        for msg in history {
            if let Message::Assistant(a) = msg {
                if let Some(ref tool_uses) = a.assistant_response_message.tool_uses {
                    for tu in tool_uses {
                        if tu.tool_use_id == "toolu_01" {
                            assert_eq!(tu.name, short_name, "历史中的 tool_use name 应该是短名称");
                            found = true;
                        }
                    }
                }
            }
        }
        assert!(found, "应该在历史中找到 tool_use");
    }

    // ===== JSON Schema $ref 展开 + 规范化 =====

    #[test]
    fn test_normalize_schema_expands_ref_from_defs() {
        // MCP/pydantic 风格：属性用 $ref 指向 $defs 的子 schema
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "filter": { "$ref": "#/$defs/Filter" } },
            "$defs": {
                "Filter": {
                    "type": "object",
                    "properties": { "field": { "type": "string" } },
                    "required": ["field"]
                }
            }
        });
        let out = normalize_json_schema(schema);
        let filter = &out["properties"]["filter"];
        // $ref 应展开为真实子 schema，而非退化为空对象
        assert_eq!(filter["type"], "object");
        assert_eq!(filter["properties"]["field"]["type"], "string");
        // $defs / $ref 不应残留（Kiro 不认）
        assert!(out.get("$defs").is_none(), "$defs 不应残留");
        assert!(filter.get("$ref").is_none(), "$ref 不应残留");
    }

    #[test]
    fn test_normalize_schema_ref_cycle_safe() {
        // 自引用循环：node 指向自身，必须靠深度上限兜底不栈溢出
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "node": { "$ref": "#/$defs/Node" } },
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": { "child": { "$ref": "#/$defs/Node" } }
                }
            }
        });
        let out = normalize_json_schema(schema);
        // 不 panic 即通过；顶层结构正常
        assert_eq!(out["type"], "object");
        assert!(out["properties"].get("node").is_some());
    }

    /// 构造一个自引用扇出（fan-out）schema：`$defs.T` 的 `properties` 里放 `b` 个
    /// 都指回 `T` 自身的属性（`c0..c{b-1}`），根节点的 `node` 属性再引用 `T`。
    ///
    /// 这是节点预算文档注释里描述的攻击最小形态：链长闸门（`MAX_REF_DEPTH=16`）只限
    /// "跳了多少次 `$ref`"，同级的 b 个属性复用同一个 `depth`，于是不设总量预算时
    /// 节点数是 b^16 量级（实测 b=2 时六百万+，`resolve_schema_refs` 内联文档写的
    /// "800 万+" 与之量级一致）。b=1（唯一安全的分叉因子）不会触发这条路径，因为
    /// 链长闸门本身就先拦住了它——这正是本测试要补的缺口。
    fn build_fanout_ref_schema(b: usize) -> serde_json::Value {
        let mut props = serde_json::Map::new();
        for i in 0..b {
            props.insert(format!("c{i}"), serde_json::json!({ "$ref": "#/$defs/T" }));
        }
        serde_json::json!({
            "type": "object",
            "properties": { "node": { "$ref": "#/$defs/T" } },
            "$defs": {
                "T": { "type": "object", "properties": props }
            }
        })
    }

    /// 构造一条**有限**的分叉引用链：`T0 -> T1 -> ... -> T{depth-1} -> leaf`，每层都
    /// fan-out `b` way（`c0..c{b-1}` 都指向下一层，而不是指回自己）。
    ///
    /// 与 `build_fanout_ref_schema` 的关键区别：这不是自引用循环，链长本身就有限
    /// （`depth` 层后落到叶子 `"type": "string"`），无论有没有节点预算 / 深度闸门都会
    /// 自然终止 —— 这正是"预算充足时应正常展开"这条对照要验证的场景：一个真实存在
    /// （只是层数多、扇出大）的合法 schema，不该被节点预算误杀成宽松 object。
    fn build_bounded_fanout_chain_schema(b: usize, depth: usize) -> serde_json::Value {
        let mut defs = serde_json::Map::new();
        defs.insert(format!("T{depth}"), serde_json::json!({ "type": "string" }));
        for lvl in (0..depth).rev() {
            let mut props = serde_json::Map::new();
            for i in 0..b {
                props.insert(
                    format!("c{i}"),
                    serde_json::json!({ "$ref": format!("#/$defs/T{}", lvl + 1) }),
                );
            }
            defs.insert(
                format!("T{lvl}"),
                serde_json::json!({ "type": "object", "properties": props }),
            );
        }
        serde_json::json!({
            "type": "object",
            "properties": { "node": { "$ref": "#/$defs/T0" } },
            "$defs": serde_json::Value::Object(defs)
        })
    }

    /// 递归统计 JSON 节点总数（object/array 记 1 再加子节点，标量记 1），用来断言
    /// 展开结果确实被节点预算钉住了上界，而不是靠"没崩就算过"这种弱断言。
    fn count_json_nodes(v: &serde_json::Value) -> usize {
        match v {
            serde_json::Value::Object(obj) => 1 + obj.values().map(count_json_nodes).sum::<usize>(),
            serde_json::Value::Array(arr) => 1 + arr.iter().map(count_json_nodes).sum::<usize>(),
            _ => 1,
        }
    }

    #[test]
    fn test_normalize_schema_fanout_b2_bounded_by_small_budget() {
        // b=2：最小的真实扇出因子。小预算下必须返回（不挂死）且被截断（预算生效）。
        let schema = build_fanout_ref_schema(2);
        let out = normalize_json_schema_with_node_budget(schema, 100);
        // 上界：展开结果的节点数不能超过注入的预算（+ 少量白名单/顶层字段的常数开销）。
        // 100 节点的预算下，实测输出稳定在个位数到十几个节点，远小于无预算时的
        // 6,488,067（b=2, 深度 16）——这就是本测试要守住的差异。
        assert!(
            count_json_nodes(&out) <= 100,
            "b=2 小预算展开结果应被节点预算钉住上界，实际 {} 个节点",
            count_json_nodes(&out)
        );
        // 结构仍是合法 schema（顶层未被整体判 malformed）。
        assert_eq!(out["type"], "object");
    }

    #[test]
    fn test_normalize_schema_fanout_b3_bounded_by_small_budget() {
        // b=3：分叉因子更大，无预算时展开量比 b=2 大得多（指数级），链长闸门
        // （depth 只在 $ref 跳转时 +1，不算同级扇出）对此完全无效，必须靠节点预算拦。
        let schema = build_fanout_ref_schema(3);
        let out = normalize_json_schema_with_node_budget(schema, 100);
        assert!(
            count_json_nodes(&out) <= 100,
            "b=3 小预算展开结果应被节点预算钉住上界，实际 {} 个节点",
            count_json_nodes(&out)
        );
        assert_eq!(out["type"], "object");
    }

    #[test]
    fn test_normalize_schema_fanout_b3_expands_fully_when_budget_sufficient() {
        // 对照组：证明节点预算**不是无脑截断** —— 一个合法的大 schema 在**生产预算**下
        // 必须完整展开、零截断。对应生产注释里"合法请求不可能被截断"的担忧。
        //
        // ⚠️ 参数是**实测**定的，不是估的（本测试上一版就是估错才长期为红）：
        //   b=3 深度 3 → visited   825 / 输出  110
        //   b=3 深度 4 → visited 3,168 / 输出  326
        //   b=3 深度 5 → visited 11,613 / 输出  974   ← 本测试用这档
        //   b=3 深度 6 → visited 41,199 / 输出 2,918   （旧版注释写"18,774"，错了一倍多）
        // 深度 6 在 20,000 预算下**必然**被截断（41,199 > 20,000），而旧版断言它不该截断，
        // 于是实现正确、测试为红。深度 5 的 11,613 在生产预算 50,000 下余量约 4 倍。
        //
        // 刻意用生产常量 `MAX_SCHEMA_NODES` 而非注入一个更大的数：注入 60,000 也能让深度 6
        // 通过，但那证明的是"给足够大的预算就不截断"（同义反复），而这里要证明的是
        // **真实生产配置下合法 schema 不受影响**。
        let schema = build_bounded_fanout_chain_schema(3, 5);
        let defs = extract_schema_defs(&schema);
        let mut budget = SchemaRefBudget::new(MAX_SCHEMA_NODES);
        let resolved = resolve_schema_refs(schema.clone(), &defs, 0, &mut budget);

        // 🔴 承重断言：**零截断**。这比"输出节点数 > N"强得多 ——
        // 后者在部分截断时仍可能成立（截断只降级末梢子树，总数照样很大）。
        assert_eq!(
            budget.truncated_nodes, 0,
            "生产预算下合法 schema 不得被截断，实际截断 {} 次（visited={}）",
            budget.truncated_nodes, budget.visited
        );
        // 预算确实被消耗了（防"夹具没触发展开"这种恒真断言：若 $defs 拼错导致
        // 一个 $ref 都没展开，visited 会是个位数而零截断照样成立）。
        assert!(
            budget.visited > 10_000,
            "夹具自检：深度 5 应访问约 11,613 个节点，实际 {} —— 太少说明 $ref 没被展开",
            budget.visited
        );
        assert!(
            resolved.get("properties").is_some(),
            "展开结果应保留 properties"
        );

        let out = normalize_json_schema_with_node_budget(schema, MAX_SCHEMA_NODES);
        let node = &out["properties"]["node"];
        // 未被截断：$ref 已展开为真实子 schema，属性里应能看到 c0/c1/c2，
        // 而不是降级后的 { "type": "object", "additionalProperties": true } 空壳。
        assert_eq!(node["type"], "object");
        assert!(
            node["properties"].get("c0").is_some(),
            "预算充足时应展开出真实子属性 c0，而非降级空壳: {node:?}"
        );
        // 再深一层同样展开（证明是整棵树展开，不只是第一层）。
        assert_eq!(node["properties"]["c0"]["type"], "object");
        assert!(
            node["properties"]["c0"]["properties"].get("c0").is_some(),
            "第二层也应展开出 c0（整棵树而非仅首层）"
        );
    }

    /// 🔴 预算耗尽时**数组必须仍是数组**，不得被换成 object 占位。
    ///
    /// 缺陷形态：`degraded_object_schema()` 是个 object，若预算在一个 `Value::Array`
    /// 节点上耗尽，整个数组被替换成对象。而 `anyOf` / `oneOf` / `allOf` / 元组式
    /// `items` **必须是数组** ⇒ 产出结构非法的 JSON Schema ⇒ 上游 400。
    /// 而节点预算存在的全部目的就是避免上游报错，那就自相矛盾了。
    ///
    /// 回退即 FAILED：把函数开头那个数组提前返回删掉（让数组重新落到预算闸门之后），
    /// 本测试必红。
    #[test]
    fn test_budget_exhaustion_keeps_arrays_as_arrays() {
        // ⚠️ 数组必须放在**根的直接键**上。实测过一版把它们埋在 `properties.pick` /
        // `properties.tuple` 之下：预算在那两个**对象**上就耗尽 ⇒ 对象被换成 object 占位
        // ⇒ `anyOf`/`items` 这两个键**整个消失**，断言拿到 `Null` 而不是「数组变对象」。
        // 那测的是父节点降级，不是本缺陷。放根上才让断言只关心数组本身。
        let big = serde_json::json!({ "type": "object", "properties": {
            "a": {"type":"string"}, "b": {"type":"string"}, "c": {"type":"string"},
            "d": {"type":"string"}, "e": {"type":"string"}, "f": {"type":"string"}
        }});

        // 每个 case 一份独立夹具：`anyOf` 与元组式 `items` 各自在根上。
        for (label, schema) in [
            (
                "anyOf",
                serde_json::json!({
                    "anyOf": [
                        { "$ref": "#/$defs/Big" },
                        { "$ref": "#/$defs/Big" },
                        { "$ref": "#/$defs/Big" }
                    ],
                    "$defs": { "Big": big.clone() }
                }),
            ),
            (
                "items",
                serde_json::json!({
                    "items": [ { "$ref": "#/$defs/Big" }, { "$ref": "#/$defs/Big" } ],
                    "$defs": { "Big": big.clone() }
                }),
            ),
        ] {
            let defs = extract_schema_defs(&schema);
            // 预算 3：足够访问根对象，但展开第一个 $ref 就耗尽。
            let mut budget = SchemaRefBudget::new(3);
            let resolved = resolve_schema_refs(schema.clone(), &defs, 0, &mut budget);

            // 夹具自检：预算必须真的被耗尽，否则本断言恒真（本仓「纸面测试」形态之一）。
            assert!(
                budget.truncated_nodes > 0,
                "[{label}] 夹具自检失败：预算未被耗尽（truncated=0, visited={}）⇒ 本测试恒真",
                budget.visited
            );

            // 🔴 承重断言：耗尽后该位置仍是数组。
            let arr = &resolved[label];
            assert!(
                arr.is_array(),
                "[{label}] 预算耗尽后必须仍是数组（否则 schema 结构非法、上游 400），实际: {arr:?}"
            );
            // 元素数不变（元素可退化成 object 占位，但不能少 —— 少了元组语义就变了）。
            let expected = if label == "anyOf" { 3 } else { 2 };
            assert_eq!(
                arr.as_array().map(|a| a.len()),
                Some(expected),
                "[{label}] 元素数不得改变"
            );
        }
    }

    #[test]
    fn test_normalize_schema_unresolvable_ref_degrades() {
        // OpenAPI 风格 #/components 无法展开 → 降级为宽松 object 而非空壳
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "x": { "$ref": "#/components/schemas/Foo" } }
        });
        let out = normalize_json_schema(schema);
        assert_eq!(out["properties"]["x"]["type"], "object");
        assert!(out["properties"]["x"].get("$ref").is_none());
    }

    #[test]
    fn test_normalize_schema_drops_combinators_and_nonwhitelist() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "opts": { "anyOf": [{"type": "object"}, {"type": "null"}] }
            },
            "title": "should be stripped",
            "$schema": "http://json-schema.org/draft-07/schema#"
        });
        let out = normalize_json_schema(schema);
        // anyOf 被丢弃；非白名单顶层字段被清
        assert!(out["properties"]["opts"].get("anyOf").is_none());
        assert!(out.get("title").is_none());
        assert!(out.get("$schema").is_none());
    }

    #[test]
    fn test_derive_agent_continuation_id_deterministic_and_isolated() {
        let a1 = derive_agent_continuation_id("conv-abc");
        let a2 = derive_agent_continuation_id("conv-abc");
        let b = derive_agent_continuation_id("conv-xyz");
        // 同会话恒定
        assert_eq!(a1, a2, "同一 conversationId 必须派生相同 continuationId");
        // 跨会话隔离
        assert_ne!(a1, b, "不同 conversationId 必须不同");
        // UUID 形状（36 字符,含 4 个连字符）
        assert_eq!(a1.len(), 36);
        assert_eq!(a1.matches('-').count(), 4);
    }

    #[test]
    fn test_extract_pdf_text_from_literal_streams() {
        // 构造一个最小 PDF 内容流片段：两个 (文本) 后接 Tj
        let fake_pdf = b"%PDF-1.4\nBT /F1 12 Tf (Hello World) Tj 0 -14 Td (Second line) Tj ET\n";
        let out = extract_pdf_text_from_bytes(fake_pdf);
        assert!(out.is_some(), "应能抽取到文本");
        let text = out.unwrap();
        assert!(text.contains("Hello World"), "应含第一段: {text}");
        assert!(text.contains("Second line"), "应含第二段: {text}");
    }

    #[test]
    fn test_extract_pdf_text_none_when_no_text() {
        // 没有文本绘制操作符的字面量不应被当作文本
        let no_text = b"%PDF-1.4\n(random data without Tj)\n";
        // 后面无 Tj/TJ/' → 不算文本
        let out = extract_pdf_text_from_bytes(no_text);
        assert!(out.is_none(), "无 Tj 操作符不应抽出文本");
    }

    #[test]
    fn test_history_tools_added_to_tools_list() {
        use super::super::types::Message as AnthropicMessage;

        // 创建一个请求，历史中有工具使用，但 tools 列表为空
        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("Read the file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "I'll read the file."},
                        {"type": "tool_use", "id": "tool-1", "name": "read", "input": {"path": "/test.txt"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "tool-1", "content": "file content"}
                    ]),
                },
            ],
            stream: false,
            system: None,
            tools: None, // 没有提供工具定义
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req).unwrap();

        // 验证 tools 列表中包含了历史中使用的工具的占位符定义
        let tools = &result
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .tools;

        assert!(!tools.is_empty(), "tools 列表不应为空");
        assert!(
            tools.iter().any(|t| t.tool_specification.name == "read"),
            "tools 列表应包含 'read' 工具的占位符定义"
        );
    }

    #[test]
    fn test_extract_session_id_valid() {
        // 测试有效的 user_id 格式
        let user_id = "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd_account__session_8bb5523b-ec7c-4540-a9ca-beb6d79f1552";
        let session_id = extract_session_id(user_id);
        assert_eq!(
            session_id,
            Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552".to_string())
        );
    }

    #[test]
    fn test_extract_session_id_json_format() {
        // 测试 JSON 格式的 user_id
        let user_id = r#"{"device_id":"0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd","account_uuid":"","session_id":"8bb5523b-ec7c-4540-a9ca-beb6d79f1552"}"#;
        let session_id = extract_session_id(user_id);
        assert_eq!(
            session_id,
            Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552".to_string())
        );
    }

    #[test]
    fn test_extract_session_id_json_invalid_session() {
        // 测试 JSON 格式但 session_id 不是有效 UUID
        let user_id = r#"{"device_id":"abc","session_id":"not-a-uuid"}"#;
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_extract_session_id_no_session() {
        // 测试没有 session 的 user_id
        let user_id = "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd";
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_extract_session_id_invalid_uuid() {
        // 测试无效的 UUID 格式
        let user_id = "user_xxx_session_invalid-uuid";
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_convert_request_with_session_metadata() {
        use super::super::types::{Message as AnthropicMessage, Metadata};

        // 测试带有 metadata 的请求，应该使用 session UUID 作为 conversationId
        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Hello"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: Some(Metadata {
                user_id: Some(
                    "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd_account__session_a0662283-7fd3-4399-a7eb-52b9a717ae88".to_string(),
                ),
            }),
        };

        let result = convert_request(&req).unwrap();
        assert_eq!(
            result.conversation_state.conversation_id,
            "a0662283-7fd3-4399-a7eb-52b9a717ae88"
        );
    }

    #[test]
    fn test_convert_request_without_metadata() {
        use super::super::types::Message as AnthropicMessage;

        // 无 metadata **且** system/tools 双空 —— 三级回落链的最后一级（随机 UUID）。
        // 有 system 或 tools 时走上下文派生，见 `derived_conversation_id_*` 系列用例。
        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Hello"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req).unwrap();
        // 验证生成的是有效的 UUID 格式
        assert_eq!(result.conversation_state.conversation_id.len(), 36);
        assert_eq!(
            result
                .conversation_state
                .conversation_id
                .chars()
                .filter(|c| *c == '-')
                .count(),
            4
        );
    }

    #[test]
    fn test_validate_tool_pairing_orphaned_result() {
        // 测试孤立的 tool_result 被过滤
        // 历史中没有 tool_use，但 tool_results 中有 tool_result
        let history = vec![
            Message::User(HistoryUserMessage::new("Hello", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage::new("Hi there!")),
        ];

        let tool_results = vec![ToolResult::success("orphan-123", "some result")];

        let (filtered, _) = validate_tool_pairing(&history, &tool_results);

        // 孤立的 tool_result 应该被过滤掉
        assert!(filtered.is_empty(), "孤立的 tool_result 应该被过滤");
    }

    #[test]
    fn test_validate_tool_pairing_orphaned_use() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试孤立的 tool_use（有 tool_use 但没有对应的 tool_result）
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-orphan", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        // 没有 tool_result
        let tool_results: Vec<ToolResult> = vec![];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 结果应该为空（因为没有 tool_result）
        // 同时应该返回孤立的 tool_use_id
        assert!(filtered.is_empty());
        assert!(orphaned.contains("tool-orphan"));
    }

    #[test]
    fn test_validate_tool_pairing_valid() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试正常配对的情况
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        let tool_results = vec![ToolResult::success("tool-1", "file content")];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 配对成功，应该保留，无孤立
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].tool_use_id, "tool-1");
        assert!(orphaned.is_empty());
    }

    #[test]
    fn test_validate_tool_pairing_mixed() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试混合情况：部分配对成功，部分孤立
        let mut assistant_msg = AssistantMessage::new("I'll use two tools.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
            ToolUseEntry::new("tool-2", "write").with_input(serde_json::json!({})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new("Do something", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        // tool_results: tool-1 配对，tool-3 孤立
        let tool_results = vec![
            ToolResult::success("tool-1", "result 1"),
            ToolResult::success("tool-3", "orphan result"), // 孤立
        ];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 只有 tool-1 应该保留
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].tool_use_id, "tool-1");
        // tool-2 是孤立的 tool_use（无 result），tool-3 是孤立的 tool_result
        assert!(orphaned.contains("tool-2"));
    }

    #[test]
    fn test_validate_tool_pairing_history_already_paired() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试历史中已配对的 tool_use 不应该被报告为孤立
        // 场景：多轮对话中，之前的 tool_use 已经在历史中有对应的 tool_result
        let mut assistant_msg1 = AssistantMessage::new("I'll read the file.");
        assistant_msg1 = assistant_msg1.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        // 构建历史中的 user 消息，包含 tool_result
        let mut user_msg_with_result = UserMessage::new("", "claude-sonnet-4.5");
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(vec![ToolResult::success("tool-1", "file content")]);
        user_msg_with_result = user_msg_with_result.with_context(ctx);

        let history = vec![
            // 第一轮：用户请求
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            // 第一轮：assistant 使用工具
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg1,
            }),
            // 第二轮：用户返回工具结果（历史中已配对）
            Message::User(HistoryUserMessage {
                user_input_message: user_msg_with_result,
            }),
            // 第二轮：assistant 响应
            Message::Assistant(HistoryAssistantMessage::new("The file contains...")),
        ];

        // 当前消息没有 tool_results（用户只是继续对话）
        let tool_results: Vec<ToolResult> = vec![];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 结果应该为空，且不应该有孤立 tool_use
        // 因为 tool-1 已经在历史中配对了
        assert!(filtered.is_empty());
        assert!(orphaned.is_empty());
    }

    #[test]
    fn test_validate_tool_pairing_duplicate_result() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试重复的 tool_result（历史中已配对，当前消息又发送了相同的 tool_result）
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        // 历史中已有 tool_result
        let mut user_msg_with_result = UserMessage::new("", "claude-sonnet-4.5");
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(vec![ToolResult::success("tool-1", "file content")]);
        user_msg_with_result = user_msg_with_result.with_context(ctx);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
            Message::User(HistoryUserMessage {
                user_input_message: user_msg_with_result,
            }),
            Message::Assistant(HistoryAssistantMessage::new("Done")),
        ];

        // 当前消息又发送了相同的 tool_result（重复）
        let tool_results = vec![ToolResult::success("tool-1", "file content again")];

        let (filtered, _) = validate_tool_pairing(&history, &tool_results);

        // 重复的 tool_result 应该被过滤掉
        assert!(filtered.is_empty(), "重复的 tool_result 应该被过滤");
    }

    #[test]
    fn test_convert_assistant_message_tool_use_only() {
        use super::super::types::Message as AnthropicMessage;

        // 测试仅包含 tool_use 的 assistant 消息（无 text 块）
        // Kiro API 要求 content 字段不能为空
        let msg = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "tool_use", "id": "toolu_01ABC", "name": "read_file", "input": {"path": "/test.txt"}}
            ]),
        };

        let result = convert_assistant_message(&msg, &mut HashMap::new()).expect("应该成功转换");

        // 验证 content 不为空（使用占位符）
        assert!(
            !result.assistant_response_message.content.is_empty(),
            "content 不应为空"
        );
        assert_eq!(
            result.assistant_response_message.content, " ",
            "仅 tool_use 时应使用 ' ' 占位符"
        );

        // 验证 tool_uses 被正确保留
        let tool_uses = result
            .assistant_response_message
            .tool_uses
            .expect("应该有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_01ABC");
        assert_eq!(tool_uses[0].name, "read_file");
    }

    #[test]
    fn test_convert_assistant_message_with_text_and_tool_use() {
        use super::super::types::Message as AnthropicMessage;

        // 测试同时包含 text 和 tool_use 的 assistant 消息
        let msg = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "text", "text": "Let me read that file for you."},
                {"type": "tool_use", "id": "toolu_02XYZ", "name": "read_file", "input": {"path": "/data.json"}}
            ]),
        };

        let result = convert_assistant_message(&msg, &mut HashMap::new()).expect("应该成功转换");

        // 验证 content 使用原始文本（不是占位符）
        assert_eq!(
            result.assistant_response_message.content,
            "Let me read that file for you."
        );

        // 验证 tool_uses 被正确保留
        let tool_uses = result
            .assistant_response_message
            .tool_uses
            .expect("应该有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_02XYZ");
    }

    #[test]
    fn test_remove_orphaned_tool_uses() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试从历史中移除孤立的 tool_use
        let mut assistant_msg = AssistantMessage::new("I'll use multiple tools.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
            ToolUseEntry::new("tool-2", "write").with_input(serde_json::json!({})),
            ToolUseEntry::new("tool-3", "delete").with_input(serde_json::json!({})),
        ]);

        let mut history = vec![
            Message::User(HistoryUserMessage::new("Do something", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        // 移除 tool-1 和 tool-3
        let mut orphaned = std::collections::HashSet::new();
        orphaned.insert("tool-1".to_string());
        orphaned.insert("tool-3".to_string());

        remove_orphaned_tool_uses(&mut history, &orphaned);

        // 验证只剩下 tool-2
        if let Message::Assistant(ref assistant_msg) = history[1] {
            let tool_uses = assistant_msg
                .assistant_response_message
                .tool_uses
                .as_ref()
                .expect("应该还有 tool_uses");
            assert_eq!(tool_uses.len(), 1);
            assert_eq!(tool_uses[0].tool_use_id, "tool-2");
        } else {
            panic!("应该是 Assistant 消息");
        }
    }

    #[test]
    fn test_remove_orphaned_tool_uses_all_removed() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试移除所有 tool_use 后，tool_uses 变为 None
        let mut assistant_msg = AssistantMessage::new("I'll use a tool.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
        ]);

        let mut history = vec![
            Message::User(HistoryUserMessage::new("Do something", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        let mut orphaned = std::collections::HashSet::new();
        orphaned.insert("tool-1".to_string());

        remove_orphaned_tool_uses(&mut history, &orphaned);

        // 验证 tool_uses 变为 None
        if let Message::Assistant(ref assistant_msg) = history[1] {
            assert!(
                assistant_msg.assistant_response_message.tool_uses.is_none(),
                "移除所有 tool_use 后应为 None"
            );
        } else {
            panic!("应该是 Assistant 消息");
        }
    }

    #[test]
    fn test_merge_consecutive_assistant_messages() {
        // 测试连续 assistant 消息被正确合并（Issue #79）
        use super::super::types::Message as AnthropicMessage;

        let msg1 = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "thinking", "thinking": "Let me think about this..."},
                {"type": "text", "text": " "}
            ]),
        };

        let msg2 = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "thinking", "thinking": "I should read the file."},
                {"type": "text", "text": "Let me read that file."},
                {"type": "tool_use", "id": "toolu_01ABC", "name": "read_file", "input": {"path": "/test.txt"}}
            ]),
        };

        let messages: Vec<&AnthropicMessage> = vec![&msg1, &msg2];
        let result = merge_assistant_messages(&messages, &mut HashMap::new()).expect("合并应成功");

        let content = &result.assistant_response_message.content;
        assert!(content.contains("<thinking>"), "应包含 thinking 标签");
        assert!(
            content.contains("Let me read that file"),
            "应包含第二条消息的 text 内容"
        );

        let tool_uses = result
            .assistant_response_message
            .tool_uses
            .expect("应有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_01ABC");
    }

    #[test]
    fn test_consecutive_assistant_with_tool_use_result_pairing() {
        // 测试 Issue #79 的完整场景
        use super::super::types::Message as AnthropicMessage;

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("Read the config file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "thinking", "thinking": "I need to read the file..."},
                        {"type": "text", "text": " "}
                    ]),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "thinking", "thinking": "Let me read the config."},
                        {"type": "text", "text": "I'll read the config file for you."},
                        {"type": "tool_use", "id": "toolu_01XYZ", "name": "read_file", "input": {"path": "/config.json"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "toolu_01XYZ", "content": "{\"key\": \"value\"}"}
                    ]),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req);
        assert!(
            result.is_ok(),
            "连续 assistant 消息场景不应报错: {:?}",
            result.err()
        );

        let state = result.unwrap().conversation_state;
        let mut found_tool_use = false;
        for msg in &state.history {
            if let Message::Assistant(assistant_msg) = msg {
                if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                    if tool_uses.iter().any(|t| t.tool_use_id == "toolu_01XYZ") {
                        found_tool_use = true;
                        break;
                    }
                }
            }
        }
        assert!(found_tool_use, "合并后的 assistant 消息应包含 tool_use");
    }

    // === B1 回归：tool_result 内的图片上浮到顶层 images ===

    /// 1x1 PNG 的 base64（测试用）
    const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

    #[test]
    fn test_tool_result_image_lifts_to_top_level() {
        use super::super::types::Message as AnthropicMessage;

        // user 提问 -> assistant tool_use -> user tool_result（含 image + text）
        let req = MessagesRequest {
            model: "claude-sonnet-4.5".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("take a screenshot"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_use", "id": "tool-1", "name": "screenshot", "input": {}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "tool-1", "content": [
                            {"type": "text", "text": "here is the screen"},
                            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": TINY_PNG_B64}}
                        ]}
                    ]),
                },
                // 追加一轮当前 user 消息，让上一轮 tool_result 进入历史（走去重/上浮路径）
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("what do you see?"),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req).unwrap();

        // 图片应从历史 tool_result 上浮到某条历史 user 消息的顶层 images
        let mut found_image = false;
        let mut tool_result_text_ok = false;
        for msg in &result.conversation_state.history {
            if let Message::User(u) = msg {
                for img in &u.user_input_message.images {
                    if img.format == "png" && img.source.bytes == TINY_PNG_B64 {
                        found_image = true;
                    }
                }
                // tool_result 只保留文本，base64 不应出现在 tool_result content 里
                for tr in &u.user_input_message.user_input_message_context.tool_results {
                    if tr.tool_use_id == "tool-1" {
                        let text = tr.content[0]
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        assert_eq!(text, "here is the screen");
                        assert!(!text.contains(TINY_PNG_B64), "tool_result 不应含 base64");
                        tool_result_text_ok = true;
                    }
                }
            }
        }
        assert!(found_image, "tool_result 内的图片应上浮到顶层 images");
        assert!(tool_result_text_ok, "应找到保留文本的 tool_result");
    }

    #[test]
    fn test_tool_result_text_only_unchanged() {
        use super::super::types::Message as AnthropicMessage;

        // 纯文本 tool_result：回归不变，不应产生任何顶层图片
        let req = MessagesRequest {
            model: "claude-sonnet-4.5".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("read the file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_use", "id": "tool-1", "name": "read", "input": {"path": "/a.txt"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "tool-1", "content": "file content"}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("thanks"),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req).unwrap();
        for msg in &result.conversation_state.history {
            if let Message::User(u) = msg {
                assert!(
                    u.user_input_message.images.is_empty(),
                    "纯文本 tool_result 不应产生顶层图片"
                );
            }
        }
    }

    #[test]
    fn test_current_message_image_always_kept() {
        // 当前轮消息（非历史）图片永远保留，不去重
        let content = serde_json::json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": TINY_PNG_B64}},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": TINY_PNG_B64}}
        ]);
        let (_text, images, _tr) = process_message_content(&content).unwrap();
        // 当前轮 dedup 为 None，两张相同图片都保留
        assert_eq!(images.len(), 2, "当前轮相同图片应全部保留");
    }

    #[test]
    fn test_history_image_dedup() {
        // 历史路径：同一张图跨消息重复出现，只保留首次
        let mut dedup = std::collections::HashSet::new();
        let content = serde_json::json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": TINY_PNG_B64}}
        ]);

        let (_t1, imgs1, _) = process_message_content_dedup(&content, Some(&mut dedup)).unwrap();
        assert_eq!(imgs1.len(), 1, "首次出现应保留图片");

        let (text2, imgs2, _) = process_message_content_dedup(&content, Some(&mut dedup)).unwrap();
        assert!(imgs2.is_empty(), "重复图片不应再次上浮");
        assert!(
            text2.contains("identical to an earlier screenshot"),
            "重复图片应替换为去重占位符"
        );
    }

    // === H3 回归：图片格式按 magic bytes 校正（客户端声明值不可信）===

    /// 造一张以 `magic` 开头、填充到 24 字节的假图，返回其 base64。
    ///
    /// 只需头部字节能被嗅探到，后续内容与判类型无关，故不必用真图。
    fn fake_image_b64(magic: &[u8]) -> String {
        use base64::Engine;
        let mut bytes = magic.to_vec();
        bytes.resize(24, 0x00);
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    }

    /// 走真实调用点（`process_message_content` → `extract_kiro_image`）取下发格式。
    ///
    /// 直接测 `resolve_image_format` 会变成纸面测试：函数本身对了但调用点没接上一样是 400。
    fn format_via_real_path(media_type: &str, data: &str) -> Option<String> {
        let content = serde_json::json!([
            {"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}}
        ]);
        let (_text, images, _tr) = process_message_content(&content).unwrap();
        images.first().map(|img| img.format.clone())
    }

    #[test]
    fn test_image_format_corrected_to_jpeg_by_magic_bytes() {
        let data = fake_image_b64(&[0xFF, 0xD8, 0xFF, 0xE0]);
        assert_eq!(
            format_via_real_path("image/png", &data).as_deref(),
            Some("jpeg"),
            "声明 png 而字节是 jpeg，应按 magic bytes 纠正为 jpeg"
        );
    }

    #[test]
    fn test_image_format_corrected_to_png_by_magic_bytes() {
        let data = fake_image_b64(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(
            format_via_real_path("image/jpeg", &data).as_deref(),
            Some("png"),
            "声明 jpeg 而字节是 png，应按 magic bytes 纠正为 png"
        );
    }

    #[test]
    fn test_image_format_corrected_to_gif_by_magic_bytes() {
        let data = fake_image_b64(b"GIF89a");
        assert_eq!(
            format_via_real_path("image/webp", &data).as_deref(),
            Some("gif"),
            "声明 webp 而字节是 gif，应按 magic bytes 纠正为 gif"
        );
    }

    #[test]
    fn test_image_format_corrected_to_webp_by_magic_bytes() {
        // RIFF + 4 字节长度占位 + WEBP：偏移 8 处的 WEBP 必须一起验，否则 wav/avi 也会命中
        let mut magic = b"RIFF".to_vec();
        magic.extend_from_slice(&[0x10, 0x00, 0x00, 0x00]);
        magic.extend_from_slice(b"WEBP");
        let data = fake_image_b64(&magic);
        assert_eq!(
            format_via_real_path("image/png", &data).as_deref(),
            Some("webp"),
            "声明 png 而字节是 webp，应按 magic bytes 纠正为 webp"
        );
    }

    #[test]
    fn test_image_format_keeps_declared_when_magic_unknown() {
        // 不匹配任何 magic：保留声明值，不猜——瞎猜会把上游本来能接受的格式改坏
        let data = fake_image_b64(&[0x00, 0x01, 0x02, 0x03]);
        assert_eq!(
            format_via_real_path("image/png", &data).as_deref(),
            Some("png"),
            "magic 认不出时应保留客户端声明的 png"
        );
        // RIFF 但偏移 8 不是 WEBP（wav 容器）同样算认不出
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&[0x10, 0x00, 0x00, 0x00]);
        wav.extend_from_slice(b"WAVE");
        assert_eq!(
            format_via_real_path("image/gif", &fake_image_b64(&wav)).as_deref(),
            Some("gif"),
            "RIFF/WAVE 不是 webp，应保留声明的 gif"
        );
    }

    #[test]
    fn test_image_format_unchanged_when_declaration_matches_magic() {
        // 真 1x1 PNG：声明与 magic 一致，格式不变
        assert_eq!(
            format_via_real_path("image/png", TINY_PNG_B64).as_deref(),
            Some("png"),
            "声明与 magic 一致时格式应保持 png"
        );
    }

    #[test]
    fn test_image_format_unsupported_declaration_rescued_by_magic() {
        // 声明是不支持的 media_type 但字节认得出：旧行为整张图无声丢弃，现在按 magic 下发
        let data = fake_image_b64(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(
            format_via_real_path("image/bmp", &data).as_deref(),
            Some("png"),
            "声明 image/bmp 而字节是 png，应按 magic 下发 png 而非丢图"
        );
        // 真 BMP（magic `BM` 不在判据内）仍认不出 → 声明值也不支持 → 维持旧的丢弃行为
        assert!(
            format_via_real_path("image/bmp", &fake_image_b64(b"BM")).is_none(),
            "magic 与声明都定不出格式时应维持旧的无声跳过"
        );
    }

    #[test]
    fn test_image_format_sniff_tolerates_data_url_prefix_and_newlines() {
        // 客户端偶发 data: 前缀 / 带换行的 base64，剥不掉就退化成"认不出"、纠正失效
        let jpeg = fake_image_b64(&[0xFF, 0xD8, 0xFF, 0xDB]);
        let with_prefix = format!("data:image/png;base64,{}", jpeg);
        assert_eq!(
            format_via_real_path("image/png", &with_prefix).as_deref(),
            Some("jpeg"),
            "带 data: 前缀时仍应按 magic bytes 纠正"
        );

        let wrapped = format!("{}\n{}", &jpeg[..8], &jpeg[8..]);
        assert_eq!(
            format_via_real_path("image/png", &wrapped).as_deref(),
            Some("jpeg"),
            "base64 带换行时仍应按 magic bytes 纠正"
        );
    }
}

/// native effort 路径测试。
///
/// 镜像开关是进程级全局，测试并行会相互污染。用一把静态锁串行所有触碰该开关的
/// 用例，并在守卫里恢复原值（与 ENV_NOISE_TEST_LOCK 同款）。
#[cfg(test)]
mod native_effort_tests {
    use super::*;
    use super::super::types::Message as AnthropicMessage;

    static NATIVE_EFFORT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct NativeEffortGuard {
        prev: bool,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl NativeEffortGuard {
        fn with(enabled: bool) -> Self {
            let lock = NATIVE_EFFORT_TEST_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let prev = native_thinking_effort_enabled();
            set_native_thinking_effort_enabled(enabled);
            NativeEffortGuard { prev, _lock: lock }
        }
    }
    impl Drop for NativeEffortGuard {
        fn drop(&mut self) {
            set_native_thinking_effort_enabled(self.prev);
        }
    }

    /// 构造只有一条 user 消息的最小请求，thinking/output_config 可控。
    fn mk_req(
        thinking: Option<super::super::types::Thinking>,
        output_config: Option<super::super::types::OutputConfig>,
    ) -> MessagesRequest {
        MessagesRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("hi"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking,
            output_config,
            metadata: None,
        }
    }

    fn enabled_thinking(budget: i32) -> super::super::types::Thinking {
        super::super::types::Thinking {
            thinking_type: "enabled".to_string(),
            budget_tokens: budget,
        }
    }

    /// budget_tokens → effort 档位表边界（参考仓同款映射）。
    #[test]
    fn budget_tokens_map_to_effort_tiers() {
        assert_eq!(effort_from_budget_tokens(0), "low");
        assert_eq!(effort_from_budget_tokens(4_000), "low");
        assert_eq!(effort_from_budget_tokens(4_001), "medium");
        assert_eq!(effort_from_budget_tokens(16_000), "medium");
        assert_eq!(effort_from_budget_tokens(16_001), "high");
        assert_eq!(effort_from_budget_tokens(64_000), "high");
        assert_eq!(effort_from_budget_tokens(64_001), "xhigh");
        assert_eq!(effort_from_budget_tokens(i32::MAX), "xhigh");
        assert_eq!(effort_from_budget_tokens(i32::MIN), "low");
    }

    /// 归一化：trim + 小写；未知值回退 "high"。
    #[test]
    fn normalize_effort_is_case_insensitive_with_fallback() {
        assert_eq!(normalize_thinking_effort("low"), "low");
        assert_eq!(normalize_thinking_effort("  HIGH "), "high");
        assert_eq!(normalize_thinking_effort("XHigh"), "xhigh");
        assert_eq!(normalize_thinking_effort("max"), "max");
        assert_eq!(normalize_thinking_effort(""), "high");
        assert_eq!(normalize_thinking_effort("ultra"), "high");
        assert_eq!(normalize_thinking_effort("enabled"), "high");
    }

    /// 白名单：实测过的 4 个模型命中，其余一律不命中（保守）。
    #[test]
    fn whitelist_hits_verified_models_only() {
        assert_eq!(native_reasoning_efforts("claude-opus-4.8"), Some(EFFORTS_WITH_XHIGH));
        assert_eq!(native_reasoning_efforts("claude-opus-4.7"), Some(EFFORTS_WITH_XHIGH));
        assert_eq!(
            native_reasoning_efforts("claude-opus-4.6"),
            Some(EFFORTS_WITHOUT_XHIGH)
        );
        assert_eq!(
            native_reasoning_efforts("claude-sonnet-4.6"),
            Some(EFFORTS_WITHOUT_XHIGH)
        );
        for miss in [
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-opus-4.5",
            "claude-sonnet-4.5",
            "claude-sonnet-4.0",
            "claude-haiku-4.5",
            "deepseek-v4-flash",
            "claude-3-5-sonnet",
            "",
        ] {
            assert_eq!(
                native_reasoning_efforts(miss),
                None,
                "未实测的模型 {miss} 不得进白名单（保守回退 XML 注入）"
            );
        }
    }

    /// ⭐ 白名单与 model_catalog 校准守卫：白名单每个 kiro_id 必须真实存在于目录，
    /// 目录里删模型 → 本测试红，提示同步白名单（防止硬编码白名单脱离 catalog 漂移）。
    #[test]
    fn native_effort_whitelist_models_exist_in_catalog() {
        let catalog = super::super::model_catalog::CATALOG;
        let ids: Vec<&str> = catalog.iter().map(|s| s.kiro_id).collect();
        for model in ["claude-opus-4.8", "claude-opus-4.7", "claude-opus-4.6", "claude-sonnet-4.6"] {
            assert!(
                ids.contains(&model),
                "白名单模型 {model} 不在 model_catalog.CATALOG 中 —— 白名单与目录已漂移"
            );
        }
    }

    /// ⭐ 镜像初值与 config 默认一致（都 false）：改任一处默认都必须同步另一处，
    /// 否则绕过 main 播种的测试/旁路会读到与 config 矛盾的默认值。
    ///
    /// ⚠️ 必须持锁：同模块 12 个测试会把镜像临时置 true（NativeEffortGuard），
    /// 本测试不持锁就会在那些窗口内随机读到 true 而误红。
    #[test]
    fn native_effort_mirror_matches_config_default() {
        let _lock = NATIVE_EFFORT_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            native_thinking_effort_enabled(),
            crate::model::config::Config::default().native_thinking_effort_enabled,
            "NATIVE_THINKING_EFFORT_ENABLED static 初值必须与 config 默认一致（默认关）"
        );
    }

    /// 开关关闭（默认）：白名单模型 + thinking 启用也不走 native（行为逐字节不变）。
    #[test]
    fn toggle_off_keeps_legacy_behavior() {
        let _g = NativeEffortGuard::with(false);
        let req = mk_req(Some(enabled_thinking(32_000)), None);
        assert_eq!(native_thinking_effort(&req, "claude-opus-4.8"), None);
        assert_eq!(build_additional_model_request_fields(&req, "claude-opus-4.8"), None);
        // XML 前缀照旧注入。
        assert_eq!(
            generate_thinking_prefix_for_model(&req, "claude-opus-4.8"),
            generate_thinking_prefix(&req)
        );
    }

    /// 开关开启 + 白名单 + thinking 启用：budget_tokens 映射选档。
    #[test]
    fn native_effort_selected_from_budget_tokens() {
        let _g = NativeEffortGuard::with(true);
        // 32000 → high
        let req = mk_req(Some(enabled_thinking(32_000)), None);
        assert_eq!(native_thinking_effort(&req, "claude-opus-4.8"), Some("high"));
        // 1000 → low
        let req = mk_req(Some(enabled_thinking(1_000)), None);
        assert_eq!(native_thinking_effort(&req, "claude-opus-4.8"), Some("low"));
        // 100000 → xhigh（5 档表允许）
        let req = mk_req(Some(enabled_thinking(100_000)), None);
        assert_eq!(native_thinking_effort(&req, "claude-opus-4.8"), Some("xhigh"));
    }

    /// 显式 output_config.effort 优先于 budget_tokens 映射。
    #[test]
    fn explicit_output_config_effort_wins() {
        let _g = NativeEffortGuard::with(true);
        // budget 会映射成 high，但显式 effort=low 优先。
        let req = mk_req(
            Some(enabled_thinking(32_000)),
            Some(super::super::types::OutputConfig {
                effort: "low".to_string(),
            }),
        );
        assert_eq!(native_thinking_effort(&req, "claude-opus-4.8"), Some("low"));
    }

    /// adaptive 分支：无 output_config 时默认 high；显式 effort 优先；
    /// 无 xhigh 档模型收到 xhigh 回退 max。
    #[test]
    fn adaptive_thinking_defaults_to_high() {
        let _g = NativeEffortGuard::with(true);
        let adaptive = super::super::types::Thinking {
            thinking_type: "adaptive".to_string(),
            budget_tokens: 0,
        };
        // 无显式 effort → high。
        let req = mk_req(Some(adaptive.clone()), None);
        assert_eq!(native_thinking_effort(&req, "claude-opus-4.8"), Some("high"));
        // 显式 xhigh + 5 档表 → xhigh。
        let req = mk_req(
            Some(adaptive.clone()),
            Some(super::super::types::OutputConfig {
                effort: "xhigh".to_string(),
            }),
        );
        assert_eq!(native_thinking_effort(&req, "claude-opus-4.8"), Some("xhigh"));
        // 显式 xhigh + 无 xhigh 档的 sonnet-4.6 → 回退 max。
        let req = mk_req(
            Some(adaptive.clone()),
            Some(super::super::types::OutputConfig {
                effort: "xhigh".to_string(),
            }),
        );
        assert_eq!(
            native_thinking_effort(&req, "claude-sonnet-4.6"),
            Some("max"),
            "adaptive + xhigh 超出白名单档位应回退 max"
        );
    }

    /// 空 effort 视同未给：enabled thinking + 大 budget 应按 budget 映射（xhigh），
    /// 不被空串归一化出的 high 覆盖（与 requested_native_reasoning 判空口径一致）。
    #[test]
    fn empty_effort_falls_through_to_budget_mapping() {
        let _g = NativeEffortGuard::with(true);
        let req = mk_req(
            Some(enabled_thinking(100_000)),
            Some(super::super::types::OutputConfig {
                effort: "".to_string(),
            }),
        );
        assert_eq!(
            native_thinking_effort(&req, "claude-opus-4.8"),
            Some("xhigh"),
            "空 effort 不应覆盖 budget 映射出的档位"
        );
    }

    /// 白名单档位外 → 回退档位表最后一项（max）。
    #[test]
    fn effort_outside_whitelist_falls_back() {
        let _g = NativeEffortGuard::with(true);
        // sonnet-4.6 无 xhigh 档：budget 映射出 xhigh → 回退 max。
        let req = mk_req(Some(enabled_thinking(100_000)), None);
        assert_eq!(
            native_thinking_effort(&req, "claude-sonnet-4.6"),
            Some("max"),
            "无 xhigh 档的模型收到 xhigh 请求应回退到允许表最后档"
        );
        // 未知 effort 字符串 → normalize 成 high（在表内，直接用）。
        let req = mk_req(
            Some(enabled_thinking(32_000)),
            Some(super::super::types::OutputConfig {
                effort: "ultra".to_string(),
            }),
        );
        assert_eq!(native_thinking_effort(&req, "claude-sonnet-4.6"), Some("high"));
    }

    /// 开关开启但模型不在白名单 → 无 native 字段，XML 照旧（非 native 路径逐字节不变）。
    #[test]
    fn non_whitelist_model_keeps_xml_injection() {
        let _g = NativeEffortGuard::with(true);
        let req = mk_req(Some(enabled_thinking(32_000)), None);
        assert_eq!(native_thinking_effort(&req, "claude-opus-5"), None);
        assert_eq!(native_thinking_effort(&req, "claude-sonnet-4.5"), None);
        assert_eq!(
            generate_thinking_prefix_for_model(&req, "claude-opus-5"),
            generate_thinking_prefix(&req),
            "非白名单模型的 XML 注入必须保持原样"
        );
    }

    /// thinking 显式 disabled → 不出 native 字段（即使给了 output_config.effort）。
    #[test]
    fn disabled_thinking_suppresses_native_path() {
        let _g = NativeEffortGuard::with(true);
        let req = mk_req(
            Some(super::super::types::Thinking {
                thinking_type: "disabled".to_string(),
                budget_tokens: 0,
            }),
            Some(super::super::types::OutputConfig {
                effort: "high".to_string(),
            }),
        );
        assert_eq!(native_thinking_effort(&req, "claude-opus-4.8"), None);
    }

    /// 无 thinking 也无 output_config → 无 native 字段（也没有 XML，两端一致 None）。
    #[test]
    fn no_reasoning_request_yields_nothing() {
        let _g = NativeEffortGuard::with(true);
        let req = mk_req(None, None);
        assert_eq!(native_thinking_effort(&req, "claude-opus-4.8"), None);
        assert_eq!(build_additional_model_request_fields(&req, "claude-opus-4.8"), None);
        assert_eq!(generate_thinking_prefix_for_model(&req, "claude-opus-4.8"), None);
    }

    /// 只给 output_config.effort（无 thinking 块）也走 native：实测的
    /// `/effort xhigh` 最小形态就是 `{output_config:{effort:xhigh}}`。
    #[test]
    fn bare_output_config_effort_triggers_native() {
        let _g = NativeEffortGuard::with(true);
        let req = mk_req(
            None,
            Some(super::super::types::OutputConfig {
                effort: "XHIGH".to_string(),
            }),
        );
        let fields = build_additional_model_request_fields(&req, "claude-opus-4.8")
            .expect("白名单 + 显式 effort 应产出 native 字段");
        assert_eq!(
            fields.output_config.expect("effort 字段应存在").effort,
            "xhigh",
            "显式 effort 应归一化后写入"
        );
    }

    /// 端到端：convert_request 产出的字段能序列化进 KiroRequest 顶层 JSON（wire 形状）。
    #[test]
    fn convert_request_carries_native_fields_into_wire_json() {
        let _g = NativeEffortGuard::with(true);
        let req = mk_req(Some(enabled_thinking(100_000)), None);
        let conversion = convert_request(&req).expect("转换应成功");
        let fields = conversion
            .additional_model_request_fields
            .expect("白名单 + thinking 应产出 native 字段");
        let kiro_request = crate::kiro::model::requests::kiro::KiroRequest {
            conversation_state: conversion.conversation_state,
            profile_arn: None,
            additional_model_request_fields: Some(fields),
        };
        let v = serde_json::to_value(&kiro_request).unwrap();
        assert_eq!(
            v["additionalModelRequestFields"]["output_config"]["effort"],
            "xhigh",
            "wire 形状必须为顶层 additionalModelRequestFields.output_config.effort"
        );
    }

    /// 开关关（默认）：同一请求零 native 字段（与旧版逐字节一致）。
    #[test]
    fn toggle_off_produces_no_native_fields_in_wire() {
        let _g = NativeEffortGuard::with(false);
        let req = mk_req(Some(enabled_thinking(100_000)), None);
        let conversion = convert_request(&req).expect("转换应成功");
        assert!(
            conversion.additional_model_request_fields.is_none(),
            "开关关时不得产出 native 字段"
        );
    }
}
