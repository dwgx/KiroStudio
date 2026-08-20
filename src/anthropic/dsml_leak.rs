//! 非流式完整文本的 DeepSeek DSML 标记剥离（纯函数）。
//!
//! 由 `stream.rs` 以 `#[path]` 子模块接入。跨 chunk 的
//! `StreamContext::strip_dsml_markers` 仍留在 `stream.rs`。

/// 从**完整文本**里剥掉 DeepSeek DSML 工具协议标记（非流式路径用）。
///
/// 流式有 [`StreamContext::strip_dsml_markers`]（带跨 chunk 尾巴缓冲），非流式此前
/// **完全没有** DSML 处理 —— `handle_non_stream_request` 把 `text_content` 原样塞进
/// content 块，`<｜DSML｜function_calls>` 这类标记逐字泄漏给客户端。fuckopencode 的口径是
/// 非流式与流式**两处都接**，这里把非流式补齐。
///
/// 语义与流式一致（2026-08-09 修复后）：
/// - 完整标记 `<｜DSML｜…>` / `</｜DSML｜…>`（行内 `>` 闭合）→ 整段丢弃；
/// - 半截标记（无 `>` 收尾）→ 只剥**本行内**部分，换行及之后正文绝不吞
///   （正文里任意 `>` 如 `a > b` / `=>` 不能触发跨行吞整段）；
/// - 非 DSML 关键字的 `<｜…>`（CJK 排版）→ 白名单守住，绝不误删。
///
/// 完整文本无跨 chunk 问题，故残留（末尾半截标记/孤立 `<`）直接丢弃：补发会泄漏标记本体。
pub(crate) fn strip_dsml_from_complete_text(text: &str) -> String {
    if !text.contains('<') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let is_close_tag = chars[i] == '<'
            && i + 2 < chars.len()
            && chars[i + 1] == '/'
            && chars[i + 2] == '\u{FF5C}';
        if (chars[i] == '<' && i + 1 < chars.len() && chars[i + 1] == '\u{FF5C}') || is_close_tag {
            let kw_start = if is_close_tag { i + 3 } else { i + 2 };
            let rest: String = chars[kw_start..].iter().collect();
            let r = rest.trim_start().to_ascii_lowercase();
            let looks_marker =
                r.starts_with("dsml") || r.starts_with("tool") || r.starts_with("function");
            // 闭合查找限行，同流式：正文里的 `>` 不能被当标记闭合导致跨行吞正文。
            let closed = chars[i..].iter().position(|&c| c == '>' || c == '\n');
            if looks_marker {
                match closed {
                    Some(rel) if chars[i + rel] == '>' => {
                        i += rel + 1; // 完整标记整段丢弃（含 `>`）
                    }
                    Some(rel) => {
                        // 半截标记 + 换行：剥本行内标记，紧邻换行作分隔符一并跳过，正文保留。
                        i += rel;
                        if i < chars.len() && chars[i] == '\n' {
                            i += 1;
                        }
                    }
                    None => {
                        // 到文本末尾都没闭合：标记残留丢弃（不补发，补发即泄漏）。
                        break;
                    }
                }
                continue;
            }
            out.push(chars[i]);
            i += 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}
