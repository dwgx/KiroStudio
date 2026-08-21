//! 文本化 invoke / tool_use XML 泄漏扫描（纯函数）。
//!
//! 由 `stream.rs` 以 `#[path]` 子模块接入。`StreamContext` 与
//! `drain_invoke_sniff_buffer` 仍留在 `stream.rs`。

use super::thinking_tags::is_quote_char;

/// 文本化工具调用诊断探针总开关(环境变量 `KIRO_INVOKE_TRACE` 非空即开)。平时零开销。
/// 开启时,assistantResponseEvent 文本流里出现工具调用标记(文本化 invoke)即记一条现场语料,
/// 用于坐实「模型把工具调用当纯文本吐出」现象(#70544 变体,致客户端断连)。
pub(crate) fn invoke_trace_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("KIRO_INVOKE_TRACE")
            .map(|v| !v.trim().is_empty() && v != "0")
            .unwrap_or(false)
    })
}

// ============================================================================
// 文本化 invoke 解析纯函数集（从 ZyphrZero/kiro.rs 移植，逐字保真逻辑）
//
// 这批函数全部是纯函数：不触碰 StreamContext / 任何可变状态，只对入参字符串做
// 结构解析。用于从「模型把工具调用当纯文本吐出」的退化输出（#70544 变体）里把
// `<invoke name="...">...<parameter ...>...</parameter>...</invoke>` 结构捞回。
// 复用 `thinking_tags::is_quote_char`（与 kiro.rs 完全一致）。
//
// 已接入状态机：`StreamContext::drain_invoke_sniff_buffer`（重组路径）逐块调用
// 本批函数，本批是它的解析层（先落地单测隔离验证，后接线——接线已完成）。
// ============================================================================

/// 检查 `name_pos`（指向标签名首字母）的前面是否构成合法的开标签起始，
/// 兼容裸写法 `<tag` 和带命名空间前缀的写法 `<prefix:tag`。
///
/// 返回 `Some(lt_pos)`（指向 `<` 的字节位置）表示合法；`None` 表示不是标签。
pub(crate) fn open_tag_lt_pos(buffer: &str, name_pos: usize) -> Option<usize> {
    let bytes = buffer.as_bytes();
    if name_pos == 0 {
        return None;
    }
    let prev = bytes[name_pos - 1];
    if prev == b'<' {
        return Some(name_pos - 1);
    }
    // 形如 `<prefix:tag`：name 前面是 ':'，再往前是一段标识符，再往前是 '<'
    if prev == b':' {
        let i = name_pos - 1; // 指向 ':'
        let mut j = i; // 标识符左边界扫描
        while j > 0 && {
            let c = bytes[j - 1];
            c.is_ascii_alphanumeric() || c == b'_'
        } {
            j -= 1;
        }
        // 标识符非空，且其左边是 '<'
        if j < i && j > 0 && bytes[j - 1] == b'<' {
            return Some(j - 1);
        }
    }
    None
}

/// 查找未被引用字符包裹的 invoke 开标签，返回指向 `<` 的字节位置
///
/// 兼容裸 `<invoke ...>` 与带命名空间前缀 `<prefix:invoke ...>` 两种写法。
/// 复用 `is_quote_char`：若 `<` 前紧贴反引号/引号等包裹字符，视为引用，跳过。
pub(crate) fn find_invoke_start(buffer: &str) -> Option<usize> {
    let mut search = 0;
    while let Some(rel) = buffer[search..].find("invoke") {
        let name_pos = search + rel;
        if let Some(lt) = open_tag_lt_pos(buffer, name_pos) {
            // 标签名后必须是边界字符（空白或 '>'），避免误匹配 invoked 之类
            let after = name_pos + "invoke".len();
            let next_ok = buffer.as_bytes().get(after).map_or(true, |c| {
                c.is_ascii_whitespace() || *c == b'>' || *c == b'/'
            });
            let has_quote_before = lt > 0 && is_quote_char(buffer, lt - 1);
            if next_ok && !has_quote_before {
                return Some(lt);
            }
        }
        search = name_pos + "invoke".len();
    }
    None
}

/// 从 `start` 之后查找第一个 invoke 闭标签，返回结束位置（exclusive，含闭标签）
///
/// 兼容裸 `</invoke>` 与带前缀 `</prefix:invoke>`。找不到返回 `None`（块还没到齐）。
pub(crate) fn find_invoke_block_end(buffer: &str, start: usize) -> Option<usize> {
    // 块 A 的边界 = 下一个 `<invoke` 开标签（即下一个块 B 的起点），没有则到 buffer 结尾。
    // 这样连发 burst（A 紧跟 B）时，A 的搜索区间被 B 的开标签卡住，绝不会吃进 B。
    let boundary = match find_next_invoke_open(buffer, start) {
        Some(p) => p,
        None => buffer.len(),
    };
    // 在 [start, boundary) 区间里取【最后一个】 `</invoke>` 作为真闭合。
    // 贪婪取最后一个 → patch 正文里出现的字面 `</invoke>` 不会导致提前截断；
    // 区间被下一个块开标签卡住 → 不会跨块误合并。
    find_last_invoke_close(buffer, start, boundary)
}

/// 从 `start` 之后查找下一个真正的 `<invoke`（或 `<prefix:invoke`）开标签的字节位置。
/// 跳过 `start` 处当前块自身的开标签。
pub(crate) fn find_next_invoke_open(buffer: &str, start: usize) -> Option<usize> {
    // 先跳过当前块的开标签：从 start 之后第一个 '>' 之后开始找。
    let after_open = match buffer[start..].find('>') {
        Some(rel) => start + rel + 1,
        None => return None,
    };
    // 注意：不能复用 find_invoke_start——它对 `<` 前是 `>`（引用字符）的情况会拒绝，
    // 而连发 burst 里 B 的 `<invoke` 恰好紧跟在 A 的 `</invoke>` 的 `>` 后面。
    // 这里只认结构：`<invoke` 或 `<prefix:invoke`，开标签名后须是空白/`>`/`/` 边界。
    let region = &buffer[after_open..];
    let mut search = 0usize;
    while let Some(rel) = region[search..].find("invoke") {
        let name_pos = search + rel;
        if let Some(lt) = open_tag_lt_pos(region, name_pos) {
            let after = name_pos + "invoke".len();
            let next_ok = region.as_bytes().get(after).map_or(true, |c| {
                c.is_ascii_whitespace() || *c == b'>' || *c == b'/'
            });
            if next_ok {
                return Some(after_open + lt);
            }
        }
        search = name_pos + "invoke".len();
    }
    None
}

/// 在 `[from, boundary)` 区间内查找最后一个 `</invoke>` / `</prefix:invoke>` 的结束位置
/// （exclusive，含闭标签）。找不到返回 `None`（块还没到齐）。
pub(crate) fn find_last_invoke_close(buffer: &str, from: usize, boundary: usize) -> Option<usize> {
    let region_end = boundary.min(buffer.len());
    if from >= region_end {
        return None;
    }
    let region = &buffer[from..region_end];
    let bytes = region.as_bytes();
    let mut search = 0usize;
    let mut last: Option<usize> = None;
    while let Some(rel) = region[search..].find("invoke>") {
        let name_pos = search + rel;
        // '</invoke>' 形式
        if name_pos >= 2 && &region[name_pos - 2..name_pos] == "</" {
            last = Some(from + name_pos + "invoke>".len());
        } else if name_pos >= 1 && bytes[name_pos - 1] == b':' {
            // '</prefix:invoke>' 形式
            let mut j = name_pos - 1; // ':'
            while j > 0 && {
                let c = bytes[j - 1];
                c.is_ascii_alphanumeric() || c == b'_'
            } {
                j -= 1;
            }
            if j >= 2 && &region[j - 2..j] == "</" {
                last = Some(from + name_pos + "invoke>".len());
            }
        }
        search = name_pos + "invoke>".len();
    }
    last
}

/// 从标签字符串中抠出 `name="..."` 的值（取第一个匹配）
pub(crate) fn extract_name_attr(tag: &str) -> Option<String> {
    let needle = "name=\"";
    let rel = tag.find(needle)?;
    let start = rel + needle.len();
    let end_rel = tag[start..].find('"')?;
    Some(tag[start..start + end_rel].to_string())
}

/// 解析一个完整 invoke 块，抠出 (tool_name, input_json_string)
///
/// - tool name 来自 invoke 开标签的 `name="..."`（兼容 antml: 前缀）
/// - 参数为零个或多个 `<parameter name="K">V</parameter>`（兼容前缀）
/// - 参数值取到下一个参数开标签前的**最后一个** `</parameter>` 为界（贪婪），
///   允许多行 / 含 `<` / 中文 / 含字面 `</parameter>`（P0-1 修复）
/// - 用 serde_json 拼成 object（值都是字符串，自动转义）
/// - 无合法 name 或拼不出合法 JSON 返回 `None`
pub(crate) fn parse_invoke_block(block: &str) -> Option<(String, String)> {
    // invoke 开标签 = 块开头到第一个 '>'
    let open_end = block.find('>')?;
    let open_tag = &block[..=open_end];
    let tool_name = extract_name_attr(open_tag)?;
    if tool_name.is_empty() {
        return None;
    }

    let mut map = serde_json::Map::new();
    let body = &block[open_end + 1..];
    let mut cursor = 0usize;
    while let Some(rel) = body[cursor..].find("parameter name=\"") {
        let name_kw = cursor + rel;
        // 确认是真正的 '<parameter' 或 '<prefix:parameter' 开标签
        // name_kw 指向 'parameter'，往前应是 '<' 或 '<prefix:'
        // 确认是真正的开标签（'<parameter' / '<prefix:parameter'）；仅用于校验，不需要位置值
        if open_tag_lt_pos(body, name_kw).is_none() {
            cursor = name_kw + "parameter".len();
            continue;
        }
        // 找该参数开标签的 '>'
        let tag_gt = match body[name_kw..].find('>') {
            Some(r) => name_kw + r,
            None => break, // 开标签未闭合，停止
        };
        let param_open_tag = &body[name_kw..tag_gt + 1];
        // 从 'parameter name="..."' 抠 key（剥掉前缀干扰：直接找 name="）
        let key = match extract_name_attr(param_open_tag) {
            Some(k) => k,
            None => {
                cursor = tag_gt + 1;
                continue;
            }
        };
        // 参数值取到 </parameter>（兼容前缀）为界。find_param_close 较贵，只调一次，
        // 同时复用 (闭标签起始, 闭标签结束) 两个值：起始用于切值，结束用于推进游标。
        let val_start = tag_gt + 1;
        let (close_start, close_end) = match find_param_close(body, val_start) {
            Some(pair) => pair,
            None => break, // 值未闭合，停止
        };
        let value = &body[val_start..close_start];
        map.insert(key, serde_json::Value::String(value.to_string()));
        // 推进到闭标签之后
        cursor = close_end;
    }

    let obj = serde_json::Value::Object(map);
    let s = serde_json::to_string(&obj).ok()?;
    Some((tool_name, s))
}

/// 从 `from` 开始查找第一个 parameter 闭标签，返回 (起始位置, 结束位置 exclusive)
///
/// 兼容裸 `</parameter>` 与带前缀 `</prefix:parameter>`。
pub(crate) fn find_param_close(body: &str, from: usize) -> Option<(usize, usize)> {
    // P0-1：参数值（尤其 apply_patch 的 patch 正文）可能含字面 `</parameter>`。
    // 朴素「取第一个 </parameter>」会把值截断。改成「贪婪取边界内最后一个 </parameter>」：
    // 边界 = 下一个 `<parameter name="` 开标签（多参数场景），没有则到 body 结尾。
    // 这样：① 单参数（含 apply_patch）取到真正的最后一个闭合，内容里的字面闭合不误伤；
    //      ② 多参数仍按下一个参数开标签正确切分。
    // 局限（已诚实标注）：若参数值里同时含字面 `<parameter name="`，边界判定会偏早；
    // 实测 apply_patch 正文极少出现该字面串，可接受。
    let boundary = match find_next_param_open(body, from) {
        Some(p) => p,
        None => body.len(),
    };
    let region = &body[from..boundary];
    let kw = "parameter>";
    let mut last: Option<(usize, usize)> = None;
    let mut search = 0usize;
    let bytes = region.as_bytes();
    while let Some(rel) = region[search..].find(kw) {
        let name_pos = search + rel;
        // '</parameter>' 形式
        if name_pos >= 2 && &region[name_pos - 2..name_pos] == "</" {
            last = Some((from + name_pos - 2, from + name_pos + kw.len()));
        } else if name_pos >= 1 && bytes[name_pos - 1] == b':' {
            // '</prefix:parameter>' 形式
            let mut j = name_pos - 1; // ':'
            while j > 0 && {
                let c = bytes[j - 1];
                c.is_ascii_alphanumeric() || c == b'_'
            } {
                j -= 1;
            }
            if j >= 2 && &region[j - 2..j] == "</" {
                last = Some((from + j - 2, from + name_pos + kw.len()));
            }
        }
        search = name_pos + kw.len();
    }
    last
}

/// 从 `from` 开始查找下一个 `<parameter name="`（或 `<prefix:parameter name="`）开标签的字节位置。
/// 用于 `find_param_close` 的贪婪边界：当前参数值最多吃到下一个参数开标签之前。
pub(crate) fn find_next_param_open(body: &str, from: usize) -> Option<usize> {
    let mut search = from;
    while let Some(rel) = body[search..].find("parameter name=\"") {
        let kw_pos = search + rel;
        // 必须是真正的开标签：'parameter' 前面是 '<' 或 '<prefix:'
        if let Some(lt) = open_tag_lt_pos(body, kw_pos) {
            return Some(lt);
        }
        search = kw_pos + "parameter".len();
    }
    None
}

/// 剥掉块前文本尾部的独立 stray token 行（单独一行的 `call` / `count` / `card` / `court`）
///
/// 实测里 `<invoke>` 前常出现一行裸 `call`/`count`，需要从块前叙述文本里剥掉，
/// 避免泄漏给客户端。只剥“尾部、且独占一行”的 stray token，前面的正常叙述保留。
/// 已实测到的 stray token 集合：Opus 长上下文退化时，泄漏的 `<invoke>` 前常有一行裸的
/// `call` / `count` / `card`。集合形式便于以后扩充。
///
/// 生产语料（KiroStudio #70544 变体）里 `court` 是最主要的 stray token，故并入集合。
/// 中文变体 `課`/`课` 也是我们实测到的高置信泄漏词（见 LEAKED_CONTROL_TOKENS），一并纳入熔断计数，
/// 否则中文退化刷屏时逐字清洗能剥、但复读熔断（32 次截断止血）抓不到 → 仍会耗尽 max_tokens。
pub(crate) const STRAY_INVOKE_TOKENS: &[&str] = &["call", "count", "card", "court", "課", "课"];

/// 复读熔断阈值：同一个 stray token（call/count/card/court）连续作为独占一行重复出现
/// 超过这么多次，判定为「Opus 长上下文退化复读死循环」，立即熔断本轮文本输出。
///
/// 取值权衡：正常工具调用前最多出现 1 个引导词行（偶有 2~3），绝不会连续几十次。
/// 设为 32 远高于正常上限、又远低于退化时的数万次，既不误伤正常引导词，又能尽早止血。
pub(crate) const REPEAT_GUARD_TRIP_THRESHOLD: u32 = 32;

/// stray 泄漏观测词表(与 clean 层 LEAKED_CONTROL_TOKENS 对齐,纯观测用)。
pub(crate) const STRAY_OBSERVE_TOKENS: &[&str] = &[
    "court", "course", "count", "care", "card", "call", "課", "课",
];

/// 判断字符是否 CJK 表意文字(观测"stray 词紧贴 CJK"的判据,与 clean 层 is_leak_glue_char 同族)。
pub(crate) fn is_cjk_ideograph(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}')
}

/// 【纯观测】扫 content 里的 stray 泄漏形态,累加两类计数(**不修改 content**):
/// - standalone:某 stray 词**独占一整行**(trim 后整行 == 词)——高置信泄漏(court 实测全独占行)。
/// - inline:某 stray 词出现在句中且**紧贴 CJK 表意字**(如 `重读course课`/`值是count的`)——
///   正常中英混排会有空格分隔,紧贴 CJK 是泄漏特征。用于点亮 clean 层够不到的句中黑洞。
/// 快路径:先 contains 任一词才细扫,正常文本零开销。
pub(crate) fn observe_stray_leak_forms(content: &str, standalone: &mut u32, inline: &mut u32) {
    // 快路径:一个都不含直接返回。
    if !STRAY_OBSERVE_TOKENS.iter().any(|t| content.contains(*t)) {
        return;
    }
    // 独占行:逐行 trim 后整行等于某 stray 词。
    for line in content.split('\n') {
        let t = line.trim();
        if STRAY_OBSERVE_TOKENS.contains(&t) {
            *standalone = standalone.saturating_add(1);
        }
    }
    // 句中紧贴 CJK:词出现处,其紧邻(前或后)是 CJK 表意字。
    for tok in STRAY_OBSERVE_TOKENS {
        let tb = tok.as_bytes();
        let mut from = 0usize;
        while let Some(rel) = content[from..].find(*tok) {
            let start = from + rel;
            let end = start + tb.len();
            let before_cjk = content[..start]
                .chars()
                .next_back()
                .is_some_and(is_cjk_ideograph);
            let after_cjk = content[end..].chars().next().is_some_and(is_cjk_ideograph);
            if before_cjk || after_cjk {
                *inline = inline.saturating_add(1);
            }
            from = end;
        }
    }
}

/// 判断一个 trim 后的行是否"看起来像退化刷屏 token":短(≤6 字符)、且全为字母或全为 CJK 表意文字,
/// 无空格/标点/数字。用于逐行检测里放宽词表(不止已知的 call/count/card/court/課/课),
/// 但仍保守(要求整行就是这么个短纯词),正常句子/代码不会整行是这种。
pub(crate) fn is_short_flood_token(line: &str) -> bool {
    let n = line.chars().count();
    if n == 0 || n > 6 {
        return false;
    }
    let all_ascii_alpha = line.chars().all(|c| c.is_ascii_alphabetic());
    // CJK 统一表意文字区(含扩展 A):课/課 等中文单字刷屏。
    let all_cjk = line
        .chars()
        .all(|c| matches!(c, '\u{3400}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}'));
    all_ascii_alpha || all_cjk
}

/// ② 结构性洪水检测:**不依赖换行、不依赖词表**。扫描文本里"同一个短 token 连续紧邻重复"的最长游程,
/// 覆盖单行连写 "课课课…课" / "coursecoursecourse…" / 逐字符重复,任意退化词都抓。
/// 命中(游程 ≥ 阈值)返回该游程起点的字节偏移(从那里截断)。
///
/// 算法:对每个可能的 token 长度(1..=6 字符),检测是否有从某位置起、同一 token 连续重复 ≥阈值次。
/// 优先抓最靠前的命中点。中文单字(len=1 char)刷屏是最常见形态,单独快速扫一遍。
pub(crate) fn detect_structural_flood(text: &str) -> Option<usize> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let n = chars.len();
    if n < REPEAT_GUARD_TRIP_THRESHOLD as usize {
        return None;
    }
    let thresh = REPEAT_GUARD_TRIP_THRESHOLD as usize;
    // 单字符游程(最常见:中文"课"连写、单字母连写)。只对"字母或 CJK"的字符计游程,
    // 避免把正常重复(如 "----" 分隔线、"...")误判——那些是标点不在此列。
    let is_floodable = |c: char| {
        c.is_ascii_alphabetic() || matches!(c, '\u{3400}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}')
    };
    let mut i = 0usize;
    while i < n {
        let (byte_start, ch) = chars[i];
        if is_floodable(ch) {
            let mut j = i + 1;
            while j < n && chars[j].1 == ch {
                j += 1;
            }
            if j - i >= thresh {
                return Some(byte_start);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    // 多字符 token 连写(如 "coursecourse…"):对 token 长度 2..=6 char 滑窗检测连续相等块。
    for tok_len in 2..=6usize {
        if n < tok_len * thresh {
            continue;
        }
        let mut i = 0usize;
        while i + tok_len <= n {
            // 当前 token = chars[i..i+tok_len],要求全 floodable(纯词,不含空格标点)。
            if !chars[i..i + tok_len].iter().all(|(_, c)| is_floodable(*c)) {
                i += 1;
                continue;
            }
            let tok: Vec<char> = chars[i..i + tok_len].iter().map(|(_, c)| *c).collect();
            let mut reps = 1usize;
            let mut k = i + tok_len;
            while k + tok_len <= n
                && chars[k..k + tok_len]
                    .iter()
                    .map(|(_, c)| *c)
                    .eq(tok.iter().copied())
            {
                reps += 1;
                k += tok_len;
            }
            if reps >= thresh {
                return Some(chars[i].0);
            }
            i = if reps > 1 { k } else { i + 1 };
        }
    }
    None
}

/// 块级复读折叠：对「已完整的整段文本」做一次性复读熔断。
///
/// 注释里曾规划用于非流式 / web_search loop 路径（当时的 `extract_invoke_content_blocks`
/// 入口），该入口至今未落地——流式路径已由 `stray_guard_filter` 逐 chunk 熔断覆盖。
/// 本函数保留为纯函数 + 单测（供未来非流式路径接线），故标记 `allow(dead_code)`。
#[allow(dead_code)]
pub(crate) fn collapse_stray_token_floods(text: &str) -> std::borrow::Cow<'_, str> {
    let mut last_line = "";
    let mut run: u32 = 0;
    let mut cut_at: Option<usize> = None;
    let mut offset = 0usize;
    for segment in text.split_inclusive('\n') {
        let line = segment.trim();
        if STRAY_INVOKE_TOKENS.contains(&line) {
            if line == last_line {
                run += 1;
            } else {
                last_line = line;
                run = 1;
            }
            if run >= REPEAT_GUARD_TRIP_THRESHOLD {
                // 从「本段（这一行）开头」截断：保留阈值内已累计的内容。
                cut_at = Some(offset);
                break;
            }
        } else if !line.is_empty() {
            last_line = line;
            run = 0;
        }
        offset += segment.len();
    }
    match cut_at {
        Some(pos) => std::borrow::Cow::Owned(text[..pos].to_string()),
        None => std::borrow::Cow::Borrowed(text),
    }
}

/// 剥掉块前文本尾部独占一行的 stray token（保留其前一行的换行）
pub(crate) fn strip_trailing_stray_tokens(before: &str) -> &str {
    let mut end = before.len();
    loop {
        let bytes = before.as_bytes();
        // 先跳过尾部的换行符，定位“最后一行”的真实结束位置
        let mut e = end;
        while e > 0 && (bytes[e - 1] == b'\n' || bytes[e - 1] == b'\r') {
            e -= 1;
        }
        let line_start = before[..e].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let last_line = before[line_start..e].trim();
        // Opus 长上下文退化时，泄漏的 <invoke> 前常有一个孤立的 stray token 行。
        // 实测样本里出现过 call / count / card / court；用集合便于以后扩充。
        if STRAY_INVOKE_TOKENS.contains(&last_line) {
            // 只剥 stray token 行本身，【保留】前一行末尾的换行符。
            // 旧实现用 line_start - 1 把前一行的换行也吞掉，会把前面的叙述正文和
            // 后续 <invoke> 挤到同一行，导致 invoke_looks_like_real_leak 的“行首”判定
            // 失败、漏捞真泄漏（narrative\ncall\n<invoke>）。改成 end = line_start：
            //   "some text\ncall" -> "some text\n"（行首信号保留）
            //   "call"（无前导正文）-> ""（line_start==0）
            end = line_start;
            if end == 0 {
                return "";
            }
        } else {
            break;
        }
    }
    &before[..end]
}

/// 判定一个 `<invoke>` 块到底像“真泄漏的工具调用”还是“正文里讨论的文本”
///
/// 实测真泄漏的 `<invoke>` 都出现在**行首**（前面是流的开头、或上一行已经换行结束），
/// 而正文讨论里的 `<invoke>` 一般**嵌在一句话中间**——前面同一行还有普通文字。
///
/// 判定规则（输入 `before` 是 `<invoke>` 之前、已剥过 stray token 的文本）：
/// - `before` 为空（`<invoke>` 在流开头）→ 像真泄漏，抓。
/// - `before` 去掉尾部空格/制表符后以换行结尾（`<invoke>` 独占新行）→ 抓。
/// - 否则（同一行前面还有非空白正文）→ 像讨论文本，不抓。
///
/// 注意：这里的“尾部空白”只剥行内空白（空格 / 制表符），不剥换行；
/// 换行结尾才是“另起一行”的信号。
pub(crate) fn invoke_looks_like_real_leak(before: &str) -> bool {
    // 剥掉尾部的行内空白（空格 / 制表符），但保留换行
    let trimmed = before.trim_end_matches([' ', '\t']);
    // 行首：要么前面什么都没有，要么上一行已经以换行结束
    trimmed.is_empty() || trimmed.ends_with('\n') || trimmed.ends_with('\r')
}

/// 推进「代码围栏」奇偶状态，对切分到多个 chunk 的 ``` 分隔符鲁棒。
///
/// 只在遇到换行符时才对「已重组的完整行」判定是否为围栏行（行首去空白后以 ``` 开头）。
/// 未遇换行的尾部留在 `partial` 里，等后续 chunk 拼齐——所以即使 ``` 被切成
/// `` `` `` + `` ` `` 两个 chunk，重组成完整行后仍能正确翻转 `open`。
///
/// 返回值仅在内部使用；主要副作用是更新 `open` 与 `partial`。
pub(crate) fn advance_code_fence_state(open: &mut bool, partial: &mut String, text: &str) {
    // review Finding 6 修复:围栏判定只需"行首若干字节是否 ```",无换行的超长行会让 partial 无界增长。
    // 一旦当前行已超过判定所需长度(远大于 "```" + 缩进),就不再累积字符(围栏与否已定),防无界 String。
    const FENCE_SCAN_LINE_CAP: usize = 256;
    for ch in text.chars() {
        if ch == '\n' {
            if partial.trim_start().starts_with("```") {
                *open = !*open;
            }
            partial.clear();
        } else if partial.len() < FENCE_SCAN_LINE_CAP {
            partial.push(ch);
        }
        // 超过 cap 的同一行剩余字符丢弃(围栏判定不需要;遇换行才重置)。
    }
}

/// 纯函数：在不改动真实状态的前提下，试算「把 `text` 走完之后围栏是否打开」。
/// 用于 drain 决策处判断某个 `<invoke>` 是否落在围栏内。
pub(crate) fn fence_open_after(open: bool, partial: &str, text: &str) -> bool {
    let mut o = open;
    let mut p = partial.to_string();
    advance_code_fence_state(&mut o, &mut p, text);
    // 还要考虑：partial 里残留的「未换行行」如果本身已经是 ``` 开头，
    // 它在遇到换行前不算翻转（保守：只有完整行才翻转）。这里返回已翻转的 o。
    o
}

/// 计算缓冲区末尾”可能是部分 `<invoke` 开标签前缀”的字节数，需要保留等待更多内容
///
/// 例如缓冲区以 `<inv` / `<` / `<i` 结尾时，可能是被切碎的 invoke 开标签，
/// 保留这段尾巴等下一个 chunk 拼齐，避免把半个标签当文本吐出去。
///
/// ⚠️ **安全上界**：真正的部分开标签（`<invoke` / `<invoke` 等）最多只有几十字节。
/// 若从末尾最后一个 `<` 到缓冲区结尾的字节数超过此阈值，说明这个 `<` 只是正文里的普通
/// `<`（中文散文的”a < b”、代码里的比较运算符等），**不是**未闭合的 invoke 开标签。
/// 此时应把整段缓冲（含 `<`）当普通文本吐出去，而不是无限持有导致流停摆：
///   1. `invoke_sniff_buffer` 一旦积压，下一轮 chunk 追加进来，lt=0，emit_len=0，
///      没有任何输出，请求看起来挂死（客户端无增量输出 + 无界内存增长）；
///   2. 根本触发路径：reclaim 开（默认）+ 请求带工具 + 模型输出含一个孤立 `<`，
///      比如”条件 a < b 时触发”这样在中文段落里极为常见的表达式。
/// 64 字节远超最长合法部分标签（`<parameter name=”` ≈ 18 字节含引号，
/// 加最长的 antml: 前缀也不超过 32 字节），同时对真正被切碎的标签有充足余量。
pub(crate) fn partial_invoke_tag_suffix_len(buf: &str) -> usize {
    /// 最长合法开标签前缀的安全上界（字节）。
    /// `<parameter name=”` ≈ 23 字节，`<invoke` = 7 字节；64 字节极为保守。
    /// 超过这个长度的”尾巴”一定不是被切碎的开标签，不应该再持有。
    const MAX_PARTIAL_TAG_BYTES: usize = 64;
    // 任何形如 `<...`（最后一个 '<' 之后没有 '>'）的尾巴都可能是部分开标签
    if let Some(lt) = buf.rfind('<') {
        if !buf[lt..].contains('>') {
            let tail_len = buf.len() - lt;
            // 安全上界：真正的部分开标签只有几十字节，超过则是正文中的普通 '<'，
            // 不应持有（否则导致缓冲区无界增长 + 整条响应停摆）。
            if tail_len <= MAX_PARTIAL_TAG_BYTES {
                return tail_len;
            }
        }
    }
    0
}

/// 计算缓冲区末尾”可能是半个 `<tool_use` 开标签前缀”的字节数，需要保留等下一 chunk 拼上。
///
/// 与 `partial_invoke_tag_suffix_len` 同型：跨 chunk 的标签可能被上游切在任意字节边界，
/// 直接按整帧吐会把半个标签泄漏给客户端。只保留与 `<tool_use` 前缀匹配的尾巴，
/// 其余（包括正文里普通 `<`）整段吐出。
pub(crate) fn partial_tool_use_xml_prefix_suffix(s: &str) -> usize {
    let max = s.len().min(crate::kiro::model::events::TOOL_USE_XML_PREFIX
        .len()
        .saturating_sub(1));
    for len in (1..=max).rev() {
        if s.is_char_boundary(s.len() - len)
            && crate::kiro::model::events::TOOL_USE_XML_PREFIX
                .starts_with(&s[s.len() - len..])
        {
            return len;
        }
    }
    0
}

/// 计算缓冲区末尾”可能是半个 `</tool_use>` **闭合**标签前缀”的字节数，需要保留等下一 chunk。
///
/// 🔴 这个函数是本层与参考仓 ref-grey 的**关键差异**，不是可选优化：
/// ref-grey 在剥离态下每个 chunk 都 `self.buffer.clear()`（ref-grey stream.rs:51），
/// 于是闭合标签一旦被上游分帧切开（`</to` + `ol_use>`），拼不齐 ⇒ `stripping` 永远退不出
/// ⇒ **响应余下的全部正文被静默吞掉**（我们用逐字节切分的 chunk 序列实测复现）。
/// 保留这段尾巴即修掉它：最多 hold `</tool_use` 的 10 字节，被误判的普通正文
/// 下一 chunk 立刻判定放行，不吞字。
pub(crate) fn partial_tool_use_xml_close_suffix(s: &str) -> usize {
    let close = crate::kiro::model::events::TOOL_USE_XML_CLOSE;
    let max = s.len().min(close.len().saturating_sub(1));
    for len in (1..=max).rev() {
        if s.is_char_boundary(s.len() - len) && close.starts_with(&s[s.len() - len..]) {
            return len;
        }
    }
    0
}

/// 半截 `<tool_use …` 是否像 tool_use 开标签的前缀（决定进入剥离态还是当正文放行）。
///
/// 覆盖两种形态：`<tool_u` 这类**比前缀短的**半截（后续可能是 `se` 拼成完整前缀），
/// 以及 `<tool_use` 已齐但属性名还没到 `>`（后接空白即合法开标签）的形态。
/// `<tool_user>` 拼到一半的 `<tool_use` 恰好也命中"前缀已齐"，但那一段在上层
/// `filter_tool_use_xml_leaks` 里已被按 `>` 后判定为非开标签吐出；这里只兜底"还没 `>`"。
pub(crate) fn is_potential_tool_use_xml_tag_start(s: &str) -> bool {
    let prefix = crate::kiro::model::events::TOOL_USE_XML_PREFIX;
    prefix.starts_with(s)
        || s.get(prefix.len()..)
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(char::is_whitespace))
}

/// 检测文本片段里是否出现「文本化的工具调用标记」。
/// 覆盖:Anthropic 工具调用语法 `<invoke`/`</invoke>`/`<parameter name=`(不论是否带 antml: 前缀),
/// 及 `<function_calls>` 包裹。仅诊断用(探针),不改控制流。
pub(crate) fn contains_textified_tool_call(text: &str) -> bool {
    text.contains("<invoke")
        || text.contains("</invoke")
        || text.contains("<parameter name=")
        || text.contains("function_calls>")
        || text.contains("antml:")
}
