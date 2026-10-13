//! 非流式完整文本的 DeepSeek DSML 标记剥离（纯函数）。
//!
//! 由 `stream.rs` 以 `#[path]` 子模块接入。跨 chunk 的
//! `StreamContext::strip_dsml_markers` 仍留在 `stream.rs`。

/// 判断 `<｜` / `</｜` 之后（跳过空白）的关键字是否为 dsml / tool / function 三者之一。
///
/// 只比较关键字本身的少数几个字符，**不**把剩余文本整段 collect 成 String：
/// 旧写法在每个 `<｜` 出现处都复制一遍剩余全文、再 `to_ascii_lowercase` 复制一次，
/// CJK 排版里 `<｜` 并不罕见，多次出现时就是 O(k·n) 的分配 + 扫描（本函数在
/// 非流式正文路径上，长文本只调用一次，但文本本身可能很大）。
fn looks_like_dsml_marker(chars: &[char], from: usize) -> bool {
    let mut k = from;
    while k < chars.len() && chars[k].is_whitespace() {
        k += 1;
    }
    const KEYS: [&str; 3] = ["dsml", "tool", "function"];
    KEYS.iter().any(|kw| {
        chars[k..]
            .iter()
            .take(kw.len())
            .map(|c| c.to_ascii_lowercase())
            .eq(kw.chars())
    })
}

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
            // 只比关键字，不整段 collect（见 looks_like_dsml_marker 的性能说明）。
            let looks_marker = looks_like_dsml_marker(&chars, kw_start);
            if looks_marker {
                // 闭合查找限行，同流式：正文里的 `>` 不能被当标记闭合导致跨行吞正文。
                // ⚠️ 只在**确认是标记**后才扫：非标记的 `<｜` 走下面原样放行，扫了也没人用
                // （旧写法无条件先扫，CJK 正文里多个 `<｜` 就是多次 O(n)）。
                let closed = chars[i..].iter().position(|&c| c == '>' || c == '\n');
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
