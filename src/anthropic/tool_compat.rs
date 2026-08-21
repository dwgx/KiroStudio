//! Claude Code ↔ Kiro 工具名/参数双向映射。
//!
//! 对外符号由 `converter.rs` 再导出，保持既有 `converter::…` 路径。

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use crate::kiro::model::requests::tool::{InputSchema, Tool, ToolSpecification};

use super::ConversionError;
use super::{
    BASH_TOOL_DESCRIPTION_SUFFIX, EDIT_TOOL_DESCRIPTION_SUFFIX, WRITE_TOOL_DESCRIPTION_SUFFIX,
    normalize_json_schema, tool_description_max_chars, truncate_chars,
};

/// Kiro API 工具名称最大长度限制（字节）
pub(super) const TOOL_NAME_MAX_LEN: usize = 63;
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

pub(super) fn tool_compat_mapping_enabled() -> bool {
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
pub(super) fn map_client_tool_name_to_kiro(
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
pub(super) fn map_tool_input_to_kiro(
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

pub(super) fn shorten_tool_name(name: &str) -> String {
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
pub(super) fn map_tool_name(name: &str, tool_name_map: &mut HashMap<String, String>) -> String {
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
pub(super) fn convert_tools(
    tools: &Option<Vec<crate::anthropic::types::Tool>>,
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
            // 函数工具。⚠️ 补名必须用 Claude Code 内置表的 key **"WebSearch"**（大写）：
            // `claude_code_tool_name_to_kiro` 是大小写敏感映射，补小写 `web_search`
            // 会错过 `is_builtin` 判定 → 即使 `tool_compat_mapping` 开关开着也取不到
            // 内置 `{query}` schema（2026-08-15 对抗审查发现，原实现补小写名有缺陷）。
            // 归一化后经通用转换：`map_client_tool_name_to_kiro("WebSearch")` →
            // Kiro 原生名 `web_search`（上游 tool_use 回传同名，回灌判定 name-only 命中）。
            let is_server_web_search = t
                .tool_type
                .as_deref()
                .is_some_and(|ty| ty.starts_with("web_search"));
            if is_server_web_search && t.name != "WebSearch" {
                let mut normalized = t.clone();
                normalized.name = "WebSearch".to_string();
                normalized.tool_type = None;
                normalized
            } else {
                t.clone()
            }
        })
        // Claude Code 2.1.215+ 新增的 fs_append 工具，Kiro 上游不支持（400/行为异常），
        // 兼容模式下隐藏不转发（参考仓 kiro-rs-admin converter.rs 同款处置）。
        // 开关关闭（raw 透传）时透传，与非 Claude Code 客户端的工具保持原样。
        .filter(|t| {
            if tool_compat_mapping_enabled() && t.name == "fs_append" {
                tracing::debug!("Claude Code 兼容模式隐藏 Kiro 不支持的 fs_append 工具");
                false
            } else {
                true
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
