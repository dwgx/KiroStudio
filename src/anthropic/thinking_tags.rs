//! thinking 标签扫描（纯函数）。由 `stream.rs` 以 `#[path]` 子模块接入。
//!
//! 流式状态机仍留在 `stream.rs`；本文件只负责标签形态判定与完整文本提取/剥离。

use std::borrow::Cow;

/// 需要跳过的包裹字符
///
/// 当 thinking 标签被这些字符包裹时，认为是在引用标签而非真正的标签：
/// - 反引号 (`)：行内代码
/// - 双引号 (")：字符串
/// - 单引号 (')：字符串
pub(crate) const QUOTE_CHARS: &[u8] = &[b'`', b'"', b'\''];

/// 检查指定位置的字符是否是引用字符
pub(crate) fn is_quote_char(buffer: &str, pos: usize) -> bool {
    buffer
        .as_bytes()
        .get(pos)
        .map(|c| QUOTE_CHARS.contains(c))
        .unwrap_or(false)
}

/// thinking 标签名（不含 `<` / `/` / `>`），大小写不敏感比对。
pub(crate) const THINKING_TAG_NAME: &[u8] = b"thinking";

/// 标签名之后到 `>` 之间允许的最大字节数（属性区上限）。
///
/// # 为什么必须有上限
///
/// 放宽为「容属性」后，「可能是半个标签」的尾巴**失去了 10 字节的天然上界**
/// （`<thinking foo="...">` 可任意长）。若无上限，一个永不闭合的 `<thinking xxxx...`
/// 会让扣留窗口无界增长 ⇒ 整条流的可见文本全被囤住不下发，复刻已知问题 #14
/// （`invoke_sniff_buffer` 无界持有 → 流停摆）。64 字节远超真实属性
/// （实测生产方 `converter.rs` 根本不发属性），超出即判定「这不是标签」。
pub(crate) const MAX_THINKING_TAG_INNER_BYTES: usize = 64;

/// 一个 thinking 标签的匹配结果：起始字节位置 + **实际**字节长度。
///
/// # 为什么必须带长度
///
/// 放宽为大小写不敏感 + 容属性后，标签长度**不再是常量**：
/// `<thinking>` 10 字节、`<thinking foo="1">` 18 字节、`</thinking >` 12 字节。
/// 此前全套查找函数只返回起点，调用方各自写死 `"<thinking>".len()` 跳过标签
/// （10 处，见 git 历史），任一处漏改就会把属性残片（`foo="1">`）留在缓冲里当正文。
/// 把长度和起点绑在同一个返回值里，调用方**无从假设**固定长度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ThinkingTagMatch {
    /// `<` 所在的字节下标
    pub(crate) start: usize,
    /// 从 `<` 到 `>`（含）的字节数
    pub(crate) len: usize,
}

impl ThinkingTagMatch {
    /// 标签之后第一个字节的下标
    pub(crate) fn end(&self) -> usize {
        self.start + self.len
    }
}

/// `buffer[pos..]` 是否恰好以一个 thinking 标签开头；是则返回该标签字节长度。
///
/// 语法（**大小写不敏感**）：
/// - 开标签：`<thinking` + 可选属性区 + `>`，属性区必须以空白起头
///   （否则 `<thinkingfoo>` 会被误认成 thinking 标签）
/// - 闭标签：`</thinking` + 可选空白 + `>`
///
/// 属性区/空白区内出现 `<` 或换行即判定「不是标签」——散文里的 `a < b`、
/// 跨行的 `<` 不该被吞成标签。
///
/// 返回 `None` 有两种含义（调用方须自行区分）：此处不是标签，或标签尚未到齐
/// （还没见到 `>`，可能跨 chunk）。
pub(crate) fn thinking_tag_len_at(buffer: &str, pos: usize, closing: bool) -> Option<usize> {
    let b = buffer.as_bytes();
    let mut i = pos;
    if b.get(i) != Some(&b'<') {
        return None;
    }
    i += 1;
    if closing {
        if b.get(i) != Some(&b'/') {
            return None;
        }
        i += 1;
    } else if b.get(i) == Some(&b'/') {
        // 闭标签不得被当成开标签
        return None;
    }
    let name_end = i + THINKING_TAG_NAME.len();
    if b.len() < name_end || !b[i..name_end].eq_ignore_ascii_case(THINKING_TAG_NAME) {
        return None;
    }
    i = name_end;
    match b.get(i) {
        Some(&b'>') => return Some(i + 1 - pos),
        // 名字后必须是 `>` 或空白，否则是别的标签名（`<thinkingfoo>`）
        Some(c) if c.is_ascii_whitespace() => {}
        _ => return None,
    }
    let inner_start = i;
    while let Some(&c) = b.get(i) {
        if i - inner_start >= MAX_THINKING_TAG_INNER_BYTES {
            return None;
        }
        match c {
            b'>' => return Some(i + 1 - pos),
            // 散文里的 `<` / 跨行内容 ⇒ 不是标签
            b'<' | b'\n' | b'\r' => return None,
            // 闭标签的 `>` 之前只允许空白（`</thinking >`）
            _ if closing && !c.is_ascii_whitespace() => return None,
            _ => i += 1,
        }
    }
    // 到缓冲末尾还没见到 `>` —— 可能是跨 chunk 的半标签，交由扣留逻辑处理
    None
}

/// 从 `from` 起扫描下一个 thinking 标签（不做任何引用/后缀判定）。
///
/// 全套 `find_real_*` 都建立在它之上，**标签形态的判据只有这一份**。本仓的教训是
/// 两套判据必然漂移，漂移的后果是「某形态在一条路径被剥、在另一条泄漏」。
pub(crate) fn scan_thinking_tag(buffer: &str, from: usize, closing: bool) -> Option<ThinkingTagMatch> {
    let b = buffer.as_bytes();
    let mut i = from.min(b.len());
    while i < b.len() {
        // `<` 是 ASCII，命中位置必在字符边界上，切片安全
        if b[i] == b'<' {
            if let Some(len) = thinking_tag_len_at(buffer, i, closing) {
                return Some(ThinkingTagMatch { start: i, len });
            }
        }
        i += 1;
    }
    None
}

/// 标签是否被引用字符包裹（正文里在**引用**标签，不是真标签）。
pub(crate) fn thinking_tag_is_quoted(buffer: &str, m: &ThinkingTagMatch) -> bool {
    let before = m.start > 0 && is_quote_char(buffer, m.start - 1);
    before || is_quote_char(buffer, m.end())
}

/// 查找真正的 thinking 结束标签（不被引用字符包裹，且后面有双换行符）
///
/// 当模型在思考过程中提到 `</thinking>` 时，通常会用反引号、引号等包裹，
/// 或者在同一行有其他内容（如"关于 </thinking> 标签"）。
/// 这个函数会跳过这些情况，只返回真正的结束标签位置。
///
/// 跳过的情况：
/// - 被引用字符包裹（反引号、引号等）
/// - 后面没有双换行符（真正的结束标签后面会有 `\n\n`）
/// - 标签在缓冲区末尾（流式处理时需要等待更多内容）
///
/// # 参数
/// - `buffer`: 要搜索的字符串
///
/// # 返回值
/// - `Some(pos)`: 真正的结束标签的起始位置
/// - `None`: 没有找到真正的结束标签
pub(crate) fn find_real_thinking_end_tag(buffer: &str) -> Option<ThinkingTagMatch> {
    let mut search_start = 0;

    while let Some(m) = scan_thinking_tag(buffer, search_start, true) {
        let absolute_pos = m.start;

        // 如果被引用字符包裹，跳过
        if thinking_tag_is_quoted(buffer, &m) {
            search_start = absolute_pos + 1;
            continue;
        }

        // 检查后面的内容
        let after_content = &buffer[m.end()..];

        // 标签后什么都还没到 → 等更多内容（可能是 `\n\n` 也可能是别的）
        if after_content.is_empty() {
            return None;
        }

        let next = after_content.chars().next().unwrap();

        // 紧跟下一个标签（`</thinking><invoke ...`）→ 立即判定结束，零字节可等。
        if next == '<' {
            return Some(m);
        }

        if next.is_whitespace() {
            let ws: &str = {
                let n: usize = after_content
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .map(char::len_utf8)
                    .sum();
                &after_content[..n]
            };
            // ⚠️ 跨 chunk 关键：空白串若**一直延伸到缓冲末尾且还没攒够 `\n\n`**，
            // 它仍可能长成完整段落分隔（`\n` 在本 chunk、第二个 `\n` 在下一个）。此时必须等，
            // 否则只消耗先到的那一个换行，剩下的漏进正文变成开头多一个空行。
            // 流真就此结束时由 `find_real_thinking_end_tag_at_buffer_end` 兜底收尾。
            if ws.len() == after_content.len() && ws.len() < PARAGRAPH_BREAK_LEN {
                return None;
            }
            // 空白串里**含换行**才算真结束（另起一行/空行）。纯行内空白（`</thinking> more`）
            // 更像正文里顺口提到标签，不认 —— 认了会把思考截断在半句话上。
            if ws.contains('\n') {
                return Some(m);
            }
            search_start = absolute_pos + 1;
            continue;
        }

        // 后面紧跟普通正文字符 → 更像是正文里提到标签，跳过继续搜索
        search_start = absolute_pos + 1;
    }

    None
}

/// 段落分隔（`\n\n`）的字节数 —— 结束标签后最多消耗这么多换行。
pub(crate) const PARAGRAPH_BREAK_LEN: usize = 2;

/// `</thinking>` 结束标签**实际消耗的字节数**（标签本身 + 其后最多一个 `\n\n` 段落分隔）。
///
/// # 为什么不能写死 `"</thinking>\n\n".len()`
///
/// [`find_real_thinking_end_tag`] 的后缀判据已放宽为「任意空白或紧跟 `<`」——
/// 只有 `\n\n` 这一种形态才恰好是 13 字节。写死 13 会在其余形态下**多切 2 字节**，
/// 把正文首两个字符吃掉（`</thinking>\nAnswer` → 切掉 `\nA` → 客户端看到 `nswer`）；
/// 而紧跟 `<invoke` 时更会切掉 `<i`，让文本化工具调用**永远无法重组**。
///
/// # 为什么参数是 [`ThinkingTagMatch`] 而不是位置
///
/// 标签本身也不是定长（`</thinking>` 11 / `</thinking >` 12 字节）。只给位置的话
/// 本函数只能写死 11，带属性/带空白的闭标签就会切错。长度必须由**匹配方**给出。
///
/// 语义：跳过标签，再跳过最多两个换行（保持既有 `\n\n` 段落分隔的剥离行为），
/// 其余字符一律保留。
pub(crate) fn thinking_end_tag_consumed_len(buffer: &str, m: &ThinkingTagMatch) -> usize {
    let rest = &buffer[m.end().min(buffer.len())..];
    let nl = rest
        .bytes()
        .take(PARAGRAPH_BREAK_LEN)
        .take_while(|b| *b == b'\n')
        .count();
    m.len + nl
}

/// 查找缓冲区末尾的 thinking 结束标签（允许末尾只有空白字符）
///
/// 用于“边界事件”场景：例如 thinking 结束后立刻进入 tool_use，或流结束，
/// 此时 `</thinking>` 后面可能没有 `\n\n`，但结束标签依然应被识别并过滤。
///
/// 约束：只有当 `</thinking>` 之后全部都是空白字符时才认为是结束标签，
/// 以避免在 thinking 内容中提到 `</thinking>`（非结束标签）时误判。
pub(crate) fn find_real_thinking_end_tag_at_buffer_end(buffer: &str) -> Option<ThinkingTagMatch> {
    let mut search_start = 0;

    while let Some(m) = scan_thinking_tag(buffer, search_start, true) {
        if thinking_tag_is_quoted(buffer, &m) {
            search_start = m.start + 1;
            continue;
        }

        // 只有当标签后面全部是空白字符时才认定为结束标签
        if buffer[m.end()..].trim().is_empty() {
            return Some(m);
        }

        search_start = m.start + 1;
    }

    None
}

/// 找到一个「[`find_real_thinking_end_tag`] 的严格判据**永远不可能再满足**」的 `</thinking>`。
///
/// # 为什么需要它（否则答案会被永久丢弃）
///
/// 严格判据要求结束标签后跟「含换行的空白」或 `<`。它对**跨 chunk**是必要的：标签后
/// 还什么都没到时必须等，否则会把段落分隔的后半个换行漏进正文。
///
/// 但有一类形态**等也没用**：`</thinking>Answer` —— 标签后紧跟的普通字符**已经到了**，
/// 后续 chunk 再来多少内容都改不了它，严格判据对这个位置永久为假。
/// 而 [`StreamContext::strip_inline_thinking_when_disabled`] 在判据返回 `None` 时会
/// **丢弃整段**（客户端没要 thinking ⇒ 未闭合就全是思考内容）⇒ `Answer` 连同后面
/// 所有正文一起消失，客户端收到**空回答**，而这在面板上是一次「成功」——完全无痕。
///
/// 本函数把「该等」与「等也没用」区分开，只对后者放行。判据是**可证的**而非启发式：
///
/// | 标签后 | 结论 |
/// |---|---|
/// | 空 | **等** —— 可能长成 `\n\n` 或 `<` |
/// | 全空白且无换行、且顶到缓冲末尾 | **等** —— 下一个 chunk 可能补上换行 |
/// | 含换行的空白 / `<` | 严格判据本就会命中，不该走到这里 |
/// | 普通字符（非空白非 `<`） | **永久不可满足 ⇒ 就地判定结束** |
///
/// 只在 `!thinking_enabled` 的剥离路径用。thinking 开启时残留会进 thinking 面板、
/// 不算泄漏也不算吞字，无需放宽（放宽反而可能把正文里顺口提到的标签当成真结束）。
pub(crate) fn find_permanently_unsatisfiable_end_tag(buffer: &str) -> Option<ThinkingTagMatch> {
    let mut search_start = 0;
    while let Some(m) = scan_thinking_tag(buffer, search_start, true) {
        let absolute_pos = m.start;
        let after = &buffer[m.end()..];
        match after.chars().next() {
            // 标签后还什么都没到 → 等
            None => return None,
            Some(c) if c == '<' => {
                // 严格判据会命中，交给它
                search_start = absolute_pos + 1;
            }
            Some(c) if c.is_whitespace() => {
                let ws_len: usize = after
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .map(char::len_utf8)
                    .sum();
                if ws_len == after.len() {
                    // 空白顶到缓冲末尾 → 还可能长成 `\n\n`，等
                    return None;
                }
                // 空白后还有别的内容：含换行则严格判据已命中；纯行内空白（`</thinking> more`）
                // 严格判据判为"正文顺口提到标签"。两种都交给严格判据，这里不抢。
                search_start = absolute_pos + 1;
            }
            // 普通字符已就位 ⇒ 该位置的严格判据永久为假
            Some(_) => return Some(m),
        }
    }
    None
}

/// 找**孤立的** thinking 闭标签：没有配对开标签的 `</thinking>`。
///
/// # 为什么它需要一套独立（最宽松）的判据
///
/// [`find_real_thinking_end_tag`] 的后缀判据（要求标签后跟含换行的空白或 `<`）是为
/// **闭合一个已开启的思考块**服务的：判早了会把思考截断在半句话上，所以宁可等。
/// 但「没有开标签」时那套判据反而有害 —— `答案开始</thinking>答案继续` 的两侧都是
/// **真正文**，没有任何理由等，也没有任何理由丢；而不认它的直接后果就是标签字面量
/// 原样进 `text_delta`（实测泄漏形态①②）。
///
/// 处置：把它当纯标记剥掉，两侧正文都保留。唯一保留的过滤是引用包裹
/// （`用 \`</thinking>\` 结束` 是正文在引用标签，不是标签）。
///
/// 只在「不在思考块内」时调用 —— 块内必须走严格判据，否则正文里顺口提到的标签
/// 会把思考块提前掐断。
pub(crate) fn find_stray_thinking_end_tag(buffer: &str) -> Option<ThinkingTagMatch> {
    let mut search_start = 0;
    while let Some(m) = scan_thinking_tag(buffer, search_start, true) {
        if thinking_tag_is_quoted(buffer, &m) {
            search_start = m.start + 1;
            continue;
        }
        return Some(m);
    }
    None
}

/// 把一段**即将作为可见正文下发**的文本里的孤立闭标签剥掉（保留两侧正文）。
///
/// 用于几条「缓冲原样倒给客户端」的收尾路径（`thinking_extracted` 之后的剩余内容、
/// EOF 残留）。这些路径此前是 [`find_stray_thinking_end_tag`] 的旁路：主循环剥了，
/// 收尾分支照样把标签倒出去。判据复用同一个函数，不新写匹配。
pub(crate) fn strip_stray_thinking_end_tags(text: &str) -> Cow<'_, str> {
    if find_stray_thinking_end_tag(text).is_none() {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(m) = find_stray_thinking_end_tag(rest) {
        out.push_str(&rest[..m.start]);
        let cut = m.start + thinking_end_tag_consumed_len(rest, &m);
        rest = &rest[cut..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// 流已结束（EOF）时，把 thinking 剥离器的残留缓冲拆成「丢弃的思考」与「必须下发的可见尾巴」。
///
/// # 为什么 EOF 需要一套单独的判据
///
/// 流式期间三个查找函数都刻意保守：[`find_real_thinking_end_tag`] 要求标签后跟空白或 `<`，
/// [`find_real_thinking_end_tag_at_buffer_end`] 要求标签后**全是**空白。保守是对的 ——
/// 半个标签跨 chunk 到达时宁可多等一个 chunk，也不能把它当正文吐出去。
///
/// 但 EOF 时「再等一个 chunk」已不存在，保守就变成了**静默吞字**：
/// `</thinking>Answer`（零空白、紧跟普通字符）这一形态三个函数全不认 ⇒
/// `in_thinking_block` 永远回不到 false ⇒ 整个 `Answer` 连同标签一起蒸发，
/// 客户端收到空回答，而面板上这是一次「成功」——**完全无痕**。
///
/// EOF 时缓冲里就是全部剩余内容，不再有歧义，因此可以放宽到「字面量 `</thinking>`」。
///
/// # 返回值
///
/// 标签后的内容（`trim_start` 后）作为可见文本返回；找不到标签时返回空串
/// （整段都还在未闭合的思考块里 ⇒ 全部丢弃，与流式期间「客户端没要就不给」的口径一致，
/// 也与 [`StreamContext::process_reasoning_content`] 在 `!thinking_enabled` 时直接丢帧一致）。
pub(crate) fn split_unclosed_thinking_residue_at_eof(buffer: &str) -> &str {
    // 不做引用包裹判定（保持「EOF 最宽松」的既有语义），但标签形态仍走统一匹配，
    // 否则大写/带属性的闭标签在这里认不出来 ⇒ 标签后的正文被整段丢弃。
    match scan_thinking_tag(buffer, 0, true) {
        Some(m) => buffer[m.end()..].trim_start(),
        None => "",
    }
}

/// `tail`（以 `<` 开头、且**尚未**见到 `>`）是否还可能长成一个 thinking 标签。
pub(crate) fn could_grow_into_thinking_tag(tail: &str) -> bool {
    let b = tail.as_bytes();
    debug_assert_eq!(b.first(), Some(&b'<'));
    let mut i = 1;
    if b.get(i) == Some(&b'/') {
        i += 1;
    }
    let rest = &b[i.min(b.len())..];
    let n = THINKING_TAG_NAME.len();
    if rest.len() < n {
        // 名字还没打完：必须是名字的真前缀（`<thi` 可以，`<div` 不行）
        return THINKING_TAG_NAME[..rest.len()].eq_ignore_ascii_case(rest);
    }
    if !rest[..n].eq_ignore_ascii_case(THINKING_TAG_NAME) {
        return false;
    }
    // 名字已完整、`>` 还没到 ⇒ 处在属性区（开标签）或空白区（闭标签）。
    // 上限与 `thinking_tag_len_at` 一致，否则扣留窗口会无界增长（见
    // `MAX_THINKING_TAG_INNER_BYTES` 的说明）。
    let inner = &rest[n..];
    if inner.len() > MAX_THINKING_TAG_INNER_BYTES {
        return false;
    }
    // 属性区起头必须是空白；区内不得出现 `<` 或换行
    if let Some(&first) = inner.first() {
        if !first.is_ascii_whitespace() {
            return false;
        }
    }
    !inner.iter().any(|c| matches!(c, b'<' | b'\n' | b'\r'))
}

/// 缓冲区末尾**真的可能是 thinking 标签**的字节数（0 = 尾巴不可能是标签，可立即放行）。
///
/// # 为什么不能无条件扣一个固定长度
///
/// 扣留尾巴是为了防"标签跨 chunk 断开时把半个标签当正文吐出去"。但无条件扣
/// `"<thinking>".len()` = **10 字节**会连带扣住别的东西 —— `</invoke>` 恰好只有
/// **9 字节**，于是文本化 invoke 的闭合标签被扣在缓冲里，重组层永远看到未闭合的
/// `<invoke`，把它当纯文本吐出去 → **工具不执行**。
/// （这正是 `generate_final_events` 那条 reclaim 旁路的成因，同型缺陷。）
///
/// 反过来，扣得太少同样致命：固定扣 10 字节**盖不住 11 字节的 `</thinking>`**，
/// 于是孤立闭标签（实测泄漏形态①②）整条穿透进可见正文。
///
/// 所以判据不能是「某个字面量的真前缀」，只能是**按标签语法判定**：
///
/// | 尾巴 | 结论 |
/// |---|---|
/// | 不含 `<` | 0（散文尾巴立刻放行，首字节少等一个 chunk） |
/// | `<` 之后不可能长成 thinking 标签（`</invoke>`、`a < b`） | 0 |
/// | 半个标签（`<thin` / `</thinki` / `<thinking fo`） | 扣住整条尾巴 |
/// | 已是完整标签、其后**只剩空白** | 扣住 —— `\n\n` 段落分隔可能跨 chunk 未到齐 |
/// | 已是完整标签、其后已有实质内容 | 0 —— 该由 finder 判定，不是"等更多"的形态 |
///
/// 上界由 [`MAX_THINKING_TAG_INNER_BYTES`] 保证（带属性后标签不再定长，无上限会
/// 复刻已知问题 #14 的流停摆）。
pub(crate) fn partial_thinking_tag_suffix_len(buffer: &str) -> usize {
    // 标签必以 `<` 开头；只有最后一个 `<` 之后的部分才可能是"还没到齐的标签"。
    let Some(p) = buffer.rfind('<') else {
        return 0;
    };
    let tail_len = buffer.len() - p;
    for closing in [true, false] {
        if let Some(len) = thinking_tag_len_at(buffer, p, closing) {
            let rest = &buffer[p + len..];
            return if rest.trim().is_empty() { tail_len } else { 0 };
        }
    }
    if could_grow_into_thinking_tag(&buffer[p..]) {
        tail_len
    } else {
        0
    }
}

/// 查找真正的 thinking 开始标签（不被引用字符包裹）
///
/// 与 `find_real_thinking_end_tag` 类似，跳过被引用字符包裹的开始标签。
pub(crate) fn find_real_thinking_start_tag(buffer: &str) -> Option<ThinkingTagMatch> {
    let mut search_start = 0;

    while let Some(m) = scan_thinking_tag(buffer, search_start, false) {
        // 如果不被引用字符包裹，则是真正的开始标签
        if !thinking_tag_is_quoted(buffer, &m) {
            return Some(m);
        }

        // 继续搜索下一个匹配
        search_start = m.start + 1;
    }

    None
}

/// 从完整文本中提取 thinking 块（用于非流式响应）
///
/// 使用与流式处理相同的标签检测逻辑（引用字符过滤），确保一致性。
/// 非流式场景下文本已完整，无需处理跨 chunk 分割问题。
///
/// # 返回值
/// - `(Some(thinking_content), remaining_text)` — 检测到有效 thinking 块
/// - `(None, original_text)` — 未检测到，原样返回
pub(crate) fn extract_thinking_from_complete_text(text: &str) -> (Option<String>, String) {
    let open = match find_real_thinking_start_tag(text) {
        Some(m) => m,
        // 没有开标签，但可能有**孤立闭标签**（形态①）。原样返回就是把标签字面量
        // 交给客户端，故仍需剥一遍；判据与流式路径同一函数。
        None => return (None, strip_stray_thinking_end_tags(text).into_owned()),
    };

    let before = &text[..open.start];
    let after_open = &text[open.end()..];

    // 查找结束标签：优先匹配带 \n\n 后缀的，退而使用末尾匹配
    let (thinking_raw, text_after) = if let Some(m) = find_real_thinking_end_tag(after_open) {
        (
            &after_open[..m.start],
            &after_open[m.start + thinking_end_tag_consumed_len(after_open, &m)..],
        )
    } else if let Some(m) = find_real_thinking_end_tag_at_buffer_end(after_open) {
        (&after_open[..m.start], after_open[m.end()..].trim_start())
    } else {
        // 找不到有效的结束标签，不做提取
        return (None, text.to_string());
    };

    // 剥离开头的换行符（与流式处理一致：模型输出 <thinking>\n）
    let thinking_content = thinking_raw.strip_prefix('\n').unwrap_or(thinking_raw);

    // 组装剩余文本：跳过纯空白的 before 部分
    let mut remaining = String::new();
    if !before.trim().is_empty() {
        remaining.push_str(before);
    }
    remaining.push_str(text_after);

    if thinking_content.is_empty() {
        (None, remaining)
    } else {
        (Some(thinking_content.to_string()), remaining)
    }
}

/// 客户端**没有**声明 thinking 时，从**完整文本**（非流式响应）里剥掉内联 `<thinking>` 块。
///
/// # 为什么非流式也必须剥
///
/// 剥离逻辑此前只存在于流式路径（[`StreamContext::strip_inline_thinking_when_disabled`]），
/// 而非流式 `handlers.rs` 的 `!thinking_enabled` 分支把上游文本**原样**塞进响应 ⇒
/// 内联 `<thinking>` 标签连同模型的内部推理**逐字泄漏**给客户端。
///
/// 同一种内容在本仓已有明确口径：`process_reasoning_content` 在 `!thinking_enabled` 时
/// **直接丢弃整帧**。流式剥、非流式漏，是同一内容两套处置。
///
/// # 判据完全复用，不新写一套
///
/// 起止标签走 [`find_real_thinking_start_tag`] / [`find_real_thinking_end_tag`] /
/// [`find_real_thinking_end_tag_at_buffer_end`]，EOF 兜底走
/// [`split_unclosed_thinking_residue_at_eof`] —— 与流式路径**同一批函数**。
/// 本仓的教训是两套判据必然漂移，而漂移的后果是「某形态在一条路径被剥、在另一条泄漏」。
///
/// # 与 thinking 开启时的差异
///
/// [`extract_thinking_from_complete_text`] 在找不到有效结束标签时**原样返回**
/// （thinking 开启时那是对的：内容会进 thinking 面板，不算泄漏）。
/// 这里不行 —— 原样返回就是泄漏本体。故未闭合时按 EOF 兜底处理：
/// 丢思考本体，只留标签之后的正文。
pub(crate) fn strip_thinking_from_complete_text(text: &str) -> String {
    let Some(open) = find_real_thinking_start_tag(text) else {
        // 无开标签时仍可能有孤立闭标签（形态①）——原样返回即泄漏，剥一遍。
        return strip_stray_thinking_end_tags(text).into_owned();
    };

    let before = &text[..open.start];
    let after_open = &text[open.end()..];

    let text_after = if let Some(m) = find_real_thinking_end_tag(after_open) {
        &after_open[m.start + thinking_end_tag_consumed_len(after_open, &m)..]
    } else if let Some(m) = find_real_thinking_end_tag_at_buffer_end(after_open) {
        after_open[m.end()..].trim_start()
    } else {
        // 未闭合（或闭合形态是流式判据不认的 `</thinking>Answer`）：EOF 兜底。
        split_unclosed_thinking_residue_at_eof(after_open)
    };

    // 与 `extract_thinking_from_complete_text` 一致：纯空白的 before 不保留
    // （模型常输出 `\n<thinking>`，留着会让正文凭空多一个前导空行）。
    let mut out = String::new();
    if !before.trim().is_empty() {
        out.push_str(before);
    }
    out.push_str(text_after);
    out
}
