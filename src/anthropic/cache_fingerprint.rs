//! 账号级前缀指纹缓存计量（cache 链 Layer 3，2026-08-11 移植）。
//!
//! 移植自 GreyGunG/Kiro-RS-Tool 的 `cache_metering.rs`（@795b9ca），适配本仓结构。
//! 上游 Kiro 不下发 cache_read/cache_creation 真值（`docs/CACHE-EXP0-RESULT.md` 已确证），
//! 故在中转层模拟 Anthropic 滑动窗口缓存的「最长公共前缀命中」语义，给 cache 链
//! 的 Layer 3 提供比 Layer 2（只估 read、creation=0）更完整的估算：
//!
//! - 把 prompt 的稳定前缀按 message 边界切成递增前缀段链（tools+system → +msg0 → +msg1 …），
//!   每段 hash = 「从头累积到该边界」的指纹，token = 该前缀的累计估算；
//! - 查询取**最深命中段** = 最长已缓存前缀 = cache_read；其后到最深断点 = cache_creation；
//!   全部 miss → read = 0、creation = 覆盖前缀全量；
//! - 会话隔离：哈希链以 session_id 种子起头（不同会话的同前缀互不命中）；
//! - 结果经 `clamp_to_total` 收敛到真实 input 口径，与链上其它层同语义。
//!
//! # 与本仓既有四层链的关系
//!
//! `prompt_cache_enabled` 开启时，本模块在 handler 层取代 Layer 2 的
//! `estimate_cache_breakdown`（指纹含 creation，严格更完整；无指纹时回退 Layer 2）。
//! 输出仍带 `x-kirostudio-cache-estimated` 标注（估算，不冒充真值）。
//!
//! # 范围裁剪与已知偏差（对比参考仓，刻意声明）
//!
//! - **无持久化 / 无后台线程**：纯内存 HashMap + 惰性淘汰。参考仓每分钟落盘 JSON 并
//!   后台清理过期条目；本仓 8GB 约束下不值得为估算层引入文件 I/O 与常驻任务，进程重启
//!   丢缓存可接受（指纹命中率随会话时长回暖）。
//! - **计算时即记录**（参考仓是「成功路径才 commit」）：参考仓担心 429/5xx 失败请求污染
//!   本地缓存；但本仓是**估算层**，且失败请求的内容前缀同样已到达上游（上游内容缓存按
//!   内容命中，与响应成败无关），记录失败尝试反而更贴近上游真实缓存状态。语义差异写入
//!   测试与注释，将来若发现估算偏差可改回成功路径 commit。
//! - **数值口径近似**：图片块固定 1000 token/图（参考仓按尺寸估算）；工具 token 只计
//!   name+description 不计 schema。估算层的数值偏差经 `clamp_to_total` 与比例收敛，
//!   只影响估算精度不影响正确性。
//! - **指纹基于客户端原始 payload，非实际发送的转换/压缩后字节**（converter 会
//!   canonicalize system、截断工具描述、压缩重试改 body）：与参考仓同款局限，估算层
//!   可接受 —— 指纹模拟的是「客户端视角的缓存前缀」，与上游内容缓存判据同源。

use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::OnceLock;

use super::cache::PromptCacheUsage;
use super::types::{MessagesRequest, SystemMessage, Tool};

/// 默认条目上限（防内存无限增长，参考仓同值）。
const DEFAULT_CAPACITY: usize = 4096;
/// 默认 TTL（5 分钟，Anthropic ephemeral 默认值）。
const DEFAULT_TTL_SECS: i64 = 5 * 60;
/// TTL 上限（1 小时，Anthropic ttl="1h"）。
const MAX_TTL_SECS: i64 = 3600;
/// 图片块 token 近似（Anthropic 默认 ~1105 token/图，此处取整估算，见模块注释）。
const IMAGE_BLOCK_TOKENS: u32 = 1000;
/// LRU 淘汰的抽样间隔（2026-08-15）：每累计 [`EVICT_INTERVAL`] 条「超限插入」才
/// 全表排序一次。排序是 O(n log n)，改前每次超限插入都排序会拖慢热路径；抽样后
/// 表长上界 = cap + EVICT_INTERVAL − 1 + 单次 record 插入段数，内存可控。
const EVICT_INTERVAL: usize = 128;

/// 单个缓存条目。
#[derive(Debug, Clone, Copy)]
struct CacheEntry {
    tokens: u32,
    expires_at: i64,
    last_hit_at: i64,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<u64, CacheEntry>,
    /// 距上次 LRU 淘汰的「超限插入」条数（抽样淘汰计数，见 [`EVICT_INTERVAL`]）。
    inserts_since_evict: usize,
}

/// 进程内提示词前缀指纹缓存（纯内存、惰性淘汰）。
#[derive(Default)]
pub(crate) struct CacheFingerprintMeter {
    inner: Mutex<Inner>,
}

static METER: OnceLock<CacheFingerprintMeter> = OnceLock::new();

/// 全局唯一的指纹缓存（估算层，无配置项，开关由调用方按 `prompt_cache_enabled` 门控）。
pub(crate) fn meter() -> &'static CacheFingerprintMeter {
    METER.get_or_init(CacheFingerprintMeter::default)
}

impl CacheFingerprintMeter {
    /// 查询一组前缀段哈希，返回每段是否命中（命中刷新 last_hit_at）。
    /// `now` 由调用方传入：生产走系统时钟，测试可注入时间。
    fn lookup_at(&self, hashes: &[u64], now: i64) -> Vec<bool> {
        let mut inner = self.inner.lock();
        hashes
            .iter()
            .map(|h| match inner.entries.get_mut(h) {
                Some(e) if e.expires_at > now => {
                    e.last_hit_at = now;
                    true
                }
                _ => false,
            })
            .collect()
    }

    /// 查询（生产路径，now = 系统时钟）。
    fn lookup(&self, hashes: &[u64]) -> Vec<bool> {
        self.lookup_at(hashes, now_secs())
    }

    /// 把一组前缀段写入缓存。`ttl_secs` clip 到 [60, MAX_TTL_SECS]；容量超限按
    /// last_hit_at（LRU）淘汰最旧条目（抽样淘汰，见 [`EVICT_INTERVAL`]）。
    fn record_at(&self, hashes: &[u64], tokens: &[u32], ttl_secs: i64, now: i64) {
        debug_assert_eq!(hashes.len(), tokens.len());
        let ttl = ttl_secs.clamp(60, MAX_TTL_SECS);
        let mut inner = self.inner.lock();
        for (h, t) in hashes.iter().zip(tokens.iter()) {
            inner.entries.insert(
                *h,
                CacheEntry {
                    tokens: *t,
                    expires_at: now + ttl,
                    last_hit_at: now,
                },
            );
        }
        // 🔴 抽样淘汰（2026-08-15）：改前每次超限插入都全表排序（O(n log n)）——
        // 容量打满后每次 record 都付一次全表排序成本，纯估算层不值得。改为每
        // EVICT_INTERVAL 条「超限插入」才淘汰一次：表长上界 = cap + EVICT_INTERVAL − 1
        // + 单次 record 段数（有界不泄漏），LRU 语义（最旧 last_hit_at 先淘汰）不变。
        if inner.entries.len() > DEFAULT_CAPACITY {
            inner.inserts_since_evict += 1;
            if inner.inserts_since_evict >= EVICT_INTERVAL {
                inner.inserts_since_evict = 0;
                let drop_n = inner.entries.len() - DEFAULT_CAPACITY;
                let mut victims: Vec<(u64, i64)> = inner
                    .entries
                    .iter()
                    .map(|(k, v)| (*k, v.last_hit_at))
                    .collect();
                victims.sort_by_key(|x| x.1);
                for (k, _) in victims.into_iter().take(drop_n) {
                    inner.entries.remove(&k);
                }
            }
        }
    }

    /// 记录（生产路径）。
    fn record(&self, hashes: &[u64], tokens: &[u32], ttl_secs: i64) {
        self.record_at(hashes, tokens, ttl_secs, now_secs());
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().entries.len()
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 解析 cache_control 的 ttl 字符串（"5m" / "1h"）→ 秒，未知值取默认 5 分钟。
fn parse_ttl(ttl: Option<&str>) -> i64 {
    match ttl {
        Some(s) if s.eq_ignore_ascii_case("1h") => 3600,
        Some(s) if s.eq_ignore_ascii_case("5m") => 300,
        _ => DEFAULT_TTL_SECS,
    }
}

/// 探测整个请求里出现过的最大 cache_control.ttl（无声明 → 默认 5 分钟）。
fn detect_max_ttl(req: &MessagesRequest) -> i64 {
    let mut max_ttl = DEFAULT_TTL_SECS;
    if let Some(systems) = req.system.as_ref() {
        for s in systems {
            if let Some(cc) = s.cache_control.as_ref() {
                max_ttl = max_ttl.max(parse_ttl(cc.ttl.as_deref()));
            }
        }
    }
    if let Some(tools) = req.tools.as_ref() {
        for t in tools {
            if let Some(cc) = t.cache_control.as_ref() {
                max_ttl = max_ttl.max(parse_ttl(cc.ttl.as_deref()));
            }
        }
    }
    for msg in &req.messages {
        if let serde_json::Value::Array(arr) = &msg.content {
            for v in arr {
                if let Some(cc) = v.get("cache_control") {
                    let ttl = cc
                        .get("ttl")
                        .and_then(|t| t.as_str())
                        .map(|s| parse_ttl(Some(s)))
                        .unwrap_or(DEFAULT_TTL_SECS);
                    max_ttl = max_ttl.max(ttl);
                }
            }
        }
    }
    max_ttl
}

/// 协议层提取出来的一个「段」：从请求开头累积到本断点的前缀。
#[derive(Debug, Clone, Copy)]
struct Segment {
    hash: u64,
    cumulative_tokens: u32,
}

/// 计算本次请求的影子缓存指纹。
///
/// 会话隔离种子从 `payload.metadata.user_id` 提取（Claude Code 格式
/// `user_xxx__session_<uuid>`，跨轮稳定、跨会话不同）；无 metadata → 无可缓存前缀。
///
/// 返回 `None` = 无可缓存前缀（无 system/tools/历史消息、或无会话隔离种子）→ 全入 input；
/// `Some` 时 cache_read = 最深命中段累计、cache_creation = 覆盖前缀 − read。
/// 结果**未 clamp**，由调用方走 `clamp_to_total`（与链上其它层同语义）。
pub(crate) fn compute_fingerprint_usage(req: &MessagesRequest) -> Option<PromptCacheUsage> {
    let cache_seed = isolation_seed(req)?;
    let (segments, ttl) = extract_segments(req, Some(cache_seed.as_str()));
    if segments.is_empty() {
        return None;
    }
    let hashes: Vec<u64> = segments.iter().map(|s| s.hash).collect();
    let cum: Vec<u32> = segments.iter().map(|s| s.cumulative_tokens).collect();
    let results = meter().lookup(&hashes);
    // 计算即记录（范围裁剪之一，见模块注释）：失败请求的前缀同样已到达上游内容缓存。
    meter().record(&hashes, &cum, ttl);

    let deepest_hit = results.iter().rposition(|h| *h);
    let covered = *cum.last().unwrap();
    let cache_read = deepest_hit.map(|i| cum[i]).unwrap_or(0);
    let cache_creation = covered.saturating_sub(cache_read);
    // 5m/1h 拆分：按探测到的最大 ttl 归边（全 1h 或全 5m，确定性）。
    let (creation_5m, creation_1h) = if ttl >= MAX_TTL_SECS {
        (0, cache_creation)
    } else {
        (cache_creation, 0)
    };
    Some(PromptCacheUsage {
        cache_creation_input_tokens: cache_creation as i32,
        cache_read_input_tokens: cache_read as i32,
        cache_creation_5m_input_tokens: creation_5m as i32,
        cache_creation_1h_input_tokens: creation_1h as i32,
    })
}

/// 生成会话隔离种子（哈希链最前置输入，不计 token）。
///
/// 直接用**完整 user_id**（Claude Code 格式 `user_xxx__session_<uuid>` —— 本身就含
/// 会话段，跨会话必然不同）作种子：同一会话多轮共享、跨会话/跨用户互不命中；
/// 无 metadata → None（无缓存资格）。不解析 `_session_` 子串（对抗审查 NIT：
/// 中间含 `_session_` 的 user_id 解析会误共享种子，整串哈希天然免疫）。
fn isolation_seed(req: &MessagesRequest) -> Option<String> {
    let uid = req.metadata.as_ref().and_then(|m| m.user_id.as_deref())?;
    if uid.trim().is_empty() {
        return None;
    }
    Some(format!("uid:{uid}"))
}

/// 从请求体按顺序提取断点段（tools → system → messages，与 Anthropic 拼接 prompt 的
/// 顺序一致）。每遇一个 cache_control 断点产出一个 Segment；最后一条 message 默认不切段
/// （当前轮新输入），除非其 content block 显式带 cache_control。
///
/// 返回 `(segments, ttl)`：segments 的 cumulative_tokens 是「从头到该断点」的累计估算；
/// ttl 是整个请求探测到的最大 cache_control.ttl。
fn extract_segments(req: &MessagesRequest, cache_seed: Option<&str>) -> (Vec<Segment>, i64) {
    let mut hasher = Sha256::new();
    let mut cache_tokens: u32 = 0;
    let mut segments: Vec<Segment> = Vec::new();
    let cache_enabled = cache_seed.is_some();
    hasher.update(cache_seed.unwrap_or("uncacheable:key:0").as_bytes());

    // feed 解耦哈希与 token 估算：hash_text 进哈希链（决定命中），token_text 进 token
    // 累计（决定数值口径）。两者分离让 token 贴近原文、不被签名前缀/分隔符污染，
    // 而哈希仍用结构化签名保持命中判定稳定。
    let feed = |hasher: &mut Sha256,
                hash_text: &str,
                token_text: &str,
                cache_tokens: &mut u32,
                participates: bool| {
        if participates {
            hasher.update(hash_text.as_bytes());
        }
        if !token_text.is_empty() {
            let tokens = crate::token::count_tokens(token_text).max(0) as u32;
            if participates {
                *cache_tokens = cache_tokens.saturating_add(tokens);
            }
        }
    };

    let commit = |hasher: &Sha256, cache_tokens: u32, segments: &mut Vec<Segment>| {
        if !cache_enabled || cache_tokens == 0 {
            return;
        }
        let digest = hasher.clone().finalize();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&digest[..8]);
        let hash = u64::from_be_bytes(buf);
        if let Some(last) = segments.last_mut()
            && last.hash == hash
            && last.cumulative_tokens == cache_tokens
        {
            return;
        }
        segments.push(Segment {
            hash,
            cumulative_tokens: cache_tokens,
        });
    };

    // 1. tools（全部喂入，作为前缀基础；工具定义跨轮稳定）。
    if let Some(tools) = req.tools.as_ref() {
        for t in tools {
            let sig = tool_signature(t);
            feed(&mut hasher, &sig, &tool_token_text(t), &mut cache_tokens, cache_enabled);
            if t.cache_control.is_some() {
                commit(&hasher, cache_tokens, &mut segments);
            }
        }
    }

    // 2. system —— 跳过「首个带 cache_control 的 block 之前」的动态头部（Claude Code
    //    在 system 最前面注入每轮变化的 block 且不打 cache_control；从它开始累积哈希
    //    会让整条前缀链被每轮污染、全部 miss —— 参考仓实测根因）。
    if let Some(systems) = req.system.as_ref() {
        let skip_until = systems
            .iter()
            .position(|s| s.cache_control.is_some())
            .unwrap_or(0);
        for sys in systems.iter().take(skip_until) {
            feed(&mut hasher, "", &sys.text, &mut cache_tokens, false);
        }
        for sys in systems.iter().skip(skip_until) {
            feed(&mut hasher, &system_signature(sys), &sys.text, &mut cache_tokens, cache_enabled);
            if sys.cache_control.is_some() {
                commit(&hasher, cache_tokens, &mut segments);
            }
        }
    }

    // tools+system 前缀作为链的第一段（仅当确实有内容时）。
    if cache_tokens > 0 {
        commit(&hasher, cache_tokens, &mut segments);
    }

    // 3. messages：除最后一条外，每条 message 边界切一个递增前缀段。
    let last_idx = req.messages.len().saturating_sub(1);
    for (idx, msg) in req.messages.iter().enumerate() {
        feed(&mut hasher, &msg.role, "", &mut cache_tokens, cache_enabled);
        match &msg.content {
            serde_json::Value::String(s) => {
                feed(&mut hasher, s, s, &mut cache_tokens, cache_enabled);
            }
            serde_json::Value::Array(arr) => {
                for v in arr {
                    if v.get("type").and_then(|t| t.as_str()) == Some("image") {
                        // 图片：哈希喂 media_type + 数据（同图稳定、异图不同），
                        // token 用固定近似（base64 不进文本估算）。
                        if cache_enabled {
                            hasher.update(image_signature_value(v).as_bytes());
                        }
                        cache_tokens = cache_tokens.saturating_add(IMAGE_BLOCK_TOKENS);
                    } else {
                        feed(
                            &mut hasher,
                            &block_signature_value(v),
                            &block_token_text(v),
                            &mut cache_tokens,
                            cache_enabled,
                        );
                    }
                    if v.get("cache_control").is_some() {
                        commit(&hasher, cache_tokens, &mut segments);
                    }
                }
            }
            _ => {}
        }
        if idx != last_idx {
            commit(&hasher, cache_tokens, &mut segments);
        }
    }

    let ttl = detect_max_ttl(req);
    (segments, ttl)
}

/// 工具的结构化签名（进哈希，决定命中）。
///
/// ⚠️ schema 走 `canonical_json`（2026-08-11 审计修复）：input_schema 是 HashMap，
/// serde 序列化顺序稳定但**依赖 hash 状态**——与 content 块的 `canonical_json`
/// 标准不一致时，客户端键序抖动会让整条前缀链连锁 miss。
fn tool_signature(t: &Tool) -> String {
    let schema = canonical_json(Some(&serde_json::to_value(&t.input_schema).unwrap_or_default()));
    format!("tool:{}|{}|{}", t.name, t.description, schema)
}

/// 工具的 token 文本（进 token 累计）。
fn tool_token_text(t: &Tool) -> String {
    format!("{} {}", t.name, t.description)
}

/// system 块的结构化签名。
fn system_signature(s: &SystemMessage) -> String {
    format!("sys:{}|{}", s.block_type.as_deref().unwrap_or("text"), s.text)
}

/// 从文本中剥除 `<system-reminder>...</system-reminder>` 标签对（语义移植自
/// k2cc `strip_system_reminders`）：Kiro 上游每轮在历史 user 消息里注入
/// system-reminder（内容含时间戳等逐轮漂移字段），把它编进指纹会让前缀链
/// 每轮全 miss、cache_read 恒 0。剥除只影响指纹签名与 token 估算，**转发字节不动**
/// （遵守 RFC「不做消息搬移」禁令）。未闭合的开始标签剥到文本末尾（k2cc 同语义）。
fn strip_system_reminders(text: &str) -> String {
    const OPEN_TAG: &str = "<system-reminder>";
    const CLOSE_TAG: &str = "</system-reminder>";

    let mut result = String::with_capacity(text.len());
    let mut search_from = 0;

    while let Some(start) = text[search_from..].find(OPEN_TAG) {
        let abs_start = search_from + start;
        result.push_str(&text[search_from..abs_start]);

        let after_open = abs_start + OPEN_TAG.len();
        if let Some(end) = text[after_open..].find(CLOSE_TAG) {
            search_from = after_open + end + CLOSE_TAG.len();
        } else {
            search_from = text.len();
        }
    }
    result.push_str(&text[search_from..]);

    result
}

/// 消息 content block 的结构化签名（进哈希）。
///
/// ⚠️ **刻意剔除每轮漂移的字段**（对抗审查 MAJOR 2，2026-08-11）：Claude Code 的工具
/// 对话里 `tool_use.id` / `tool_result.tool_use_id` 每轮回传时重新生成 —— 把它们编进
/// 哈希会让工具类多轮对话的前缀链每轮全 miss、read 恒 0。JSON 内容先 canonicalize
/// （递归排序键），防客户端键序抖动连锁 miss。语义对齐参考仓 cache_metering.rs。
/// text 块另剥 `<system-reminder>` 标签对（Kiro 注入、内容逐轮漂移，见
/// [`strip_system_reminders`]）。
fn block_signature_value(v: &serde_json::Value) -> String {
    let block_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("text");
    match block_type {
        "tool_use" => format!(
            "block:tool_use|{}|{}",
            v.get("name").and_then(|x| x.as_str()).unwrap_or(""),
            canonical_json(v.get("input"))
        ),
        "tool_result" => format!(
            "block:tool_result|{}|{}",
            v.get("is_error").map(|x| x.to_string()).unwrap_or_else(|| "false".into()),
            canonical_json(v.get("content"))
        ),
        "thinking" => format!(
            "block:thinking|{}",
            v.get("thinking").and_then(|x| x.as_str()).unwrap_or("")
        ),
        "redacted_thinking" => format!(
            "block:redacted_thinking|{}",
            v.get("data").and_then(|x| x.as_str()).unwrap_or("")
        ),
        _ => {
            let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("");
            format!("block:{block_type}|{}", strip_system_reminders(text))
        }
    }
}

/// JSON 值 → 稳定字符串：对象键递归排序（serde_json::Value 保留客户端键序，轮次间
/// 键序抖动会让同内容产出不同 hash → 连锁 miss）。标量/数组原样 to_string。
fn canonical_json(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::Object(map)) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", serde_json::Value::String(k.clone()), canonical_json(map.get(k))))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// 消息 content block 的 token 文本（text 块同样剥 `<system-reminder>`，与签名一致）。
fn block_token_text(v: &serde_json::Value) -> String {
    match v.get("type").and_then(|t| t.as_str()) {
        Some("tool_use") => v
            .get("input")
            .map(|x| x.to_string())
            .unwrap_or_default(),
        Some("tool_result") => v
            .get("content")
            .map(|x| match x {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default(),
        Some("thinking") => v
            .get("thinking")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        Some("redacted_thinking") => v
            .get("data")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        _ => strip_system_reminders(
            v.get("text").and_then(|x| x.as_str()).unwrap_or(""),
        ),
    }
}

/// 图片块的结构化签名（media_type + 数据，同图稳定、异图不同）。
fn image_signature_value(v: &serde_json::Value) -> String {
    let src = v.get("source").unwrap_or(&serde_json::Value::Null);
    format!(
        "image:{}|{}",
        src.get("media_type").and_then(|x| x.as_str()).unwrap_or(""),
        src.get("data").and_then(|x| x.as_str()).unwrap_or(""),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::types::{CacheControl, Message, SystemMessage, Tool};

    // ⚠️ 会话种子约定（2026-08-11 deep 审计）：
    // 走 compute_fingerprint_usage 的测试共享全局静态 METER（OnceLock）+ 真实时钟，
    // **新增测试必须用全仓唯一的 user_id 种子**（形如 `user_a__session-<测试名>`），
    // 且不要复用其它测试的消息内容 —— 否则并行执行时「首轮 read==0」类断言会被
    // 别的测试先写入的段污染而 flaky。

    fn msg(role: &str, content: &str) -> Message {
        Message {
            role: role.to_string(),
            content: serde_json::Value::String(content.to_string()),
        }
    }

    fn cc(ttl: Option<&str>) -> CacheControl {
        CacheControl {
            cache_type: "ephemeral".into(),
            ttl: ttl.map(|s| s.to_string()),
        }
    }

    fn req_with(
        user_id: Option<&str>,
        system: Option<Vec<SystemMessage>>,
        tools: Option<Vec<Tool>>,
        messages: Vec<Message>,
    ) -> MessagesRequest {
        MessagesRequest {
            model: "claude-opus-4-6".into(),
            max_tokens: 100,
            messages,
            stream: false,
            system,
            tools,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: user_id.map(|u| crate::anthropic::types::Metadata {
                user_id: Some(u.to_string()),
            }),
        }
    }

    /// 前提：同一 payload + 同一会话，第二次调用必须产生 cache_read（最长公共前缀命中）。
    #[test]
    fn same_session_second_call_gets_read() {
        let r = req_with(
            Some("user_a__session_0b4445e1-f5be-49e1-87ce-62bbc28ad705"),
            Some(vec![SystemMessage { text: "stable system".into(), block_type: None, cache_control: None }]),
            None,
            vec![msg("user", "turn one"), msg("assistant", "reply one"), msg("user", "turn two")],
        );
        let u1 = compute_fingerprint_usage(&r).expect("有可缓存前缀");
        assert_eq!(u1.cache_read_input_tokens, 0, "首轮无历史命中");
        assert!(u1.cache_creation_input_tokens > 0, "首轮覆盖前缀全部记为 creation");

        let u2 = compute_fingerprint_usage(&r).expect("有可缓存前缀");
        assert!(
            u2.cache_read_input_tokens > 0,
            "同会话同前缀第二轮必须命中（read>0），got {}",
            u2.cache_read_input_tokens
        );
        assert!(
            u2.cache_creation_input_tokens < u1.cache_creation_input_tokens,
            "第二轮已命中的前缀不再计 creation"
        );
    }

    /// 会话隔离：不同 session 的同前缀不得互相命中。
    #[test]
    fn different_session_never_hits() {
        let r_a = req_with(
            Some("user_a__session-isolate-a"),
            None,
            None,
            vec![msg("user", "hello world"), msg("assistant", "hi"), msg("user", "again")],
        );
        let _u1 = compute_fingerprint_usage(&r_a);
        let r_b = req_with(
            Some("user_a__session-isolate-b"),
            None,
            None,
            vec![msg("user", "hello world"), msg("assistant", "hi"), msg("user", "again")],
        );
        let u2 = compute_fingerprint_usage(&r_b);
        assert_eq!(
            u2.map(|u| u.cache_read_input_tokens).unwrap_or(0),
            0,
            "跨会话不得命中（隔离种子不同 → 哈希不同）"
        );
    }

    /// 无 metadata（拿不到会话种子）= 无可缓存前缀（连缓存资格都没有，不会误报命中）。
    #[test]
    fn no_session_seed_returns_none() {
        let r = req_with(None, None, None, vec![msg("user", "hello")]);
        assert!(compute_fingerprint_usage(&r).is_none());
    }

    /// 无 system/tools/历史消息（只有一条当前输入）→ 无可缓存前缀 → None。
    #[test]
    fn single_user_message_returns_none() {
        let r = req_with(Some("user_a__session-isolate-a"), None, None, vec![msg("user", "hello")]);
        assert!(compute_fingerprint_usage(&r).is_none());
    }

    /// 显式 cache_control 断点：最后一条消息的 block 带断点时也要切段（参考仓语义：
    /// 否则长当前输入永远只读不写）。
    #[test]
    fn explicit_cache_control_block_creates_segment() {
        let r = req_with(
            Some("user_a__session-isolate-a"),
            None,
            None,
            vec![
                msg("user", "history"),
                Message {
                    role: "user".into(),
                    content: serde_json::json!([
                        {"type": "text", "text": "current long input", "cache_control": {"type": "ephemeral"}}
                    ]),
                },
            ],
        );
        let u = compute_fingerprint_usage(&r).expect("显式断点必须有覆盖");
        assert!(u.cache_creation_input_tokens > 0, "显式断点产生可写段");
    }

    /// ttl=1h 时 creation 全部归 1h 档，5m 档为 0；默认 5m 时反之。
    #[test]
    fn ttl_splits_creation_between_ephemeral_tiers() {
        let r_1h = req_with(
            Some("user_a__session-ttl-1h"),
            Some(vec![SystemMessage { text: "sys".into(), block_type: None, cache_control: Some(cc(Some("1h"))) }]),
            None,
            vec![msg("user", "u1"), msg("assistant", "a1"), msg("user", "u2")],
        );
        let u_1h = compute_fingerprint_usage(&r_1h).unwrap();
        assert_eq!(u_1h.cache_creation_5m_input_tokens, 0, "1h 断点 → creation 全归 1h");
        assert_eq!(
            u_1h.cache_creation_1h_input_tokens,
            u_1h.cache_creation_input_tokens
        );

        let r_5m = req_with(
            Some("user_a__session-ttl-5m"),
            Some(vec![SystemMessage { text: "sys".into(), block_type: None, cache_control: Some(cc(None)) }]),
            None,
            vec![msg("user", "u1"), msg("assistant", "a1"), msg("user", "u2")],
        );
        let u_5m = compute_fingerprint_usage(&r_5m).unwrap();
        assert_eq!(u_5m.cache_creation_1h_input_tokens, 0, "默认 5m 断点 → creation 全归 5m");
    }

    /// 最长公共前缀语义：第三轮只多一条消息时，前两轮的段必须仍命中。
    /// ⚠️ 会话种子必须全仓唯一：全局 METER 跨测试共享，同种子同前缀会串扰（并行执行
    /// 顺序不定，会让本测试的首轮断言变成"已命中"）。
    #[test]
    fn prefix_chain_hits_after_appending_turn() {
        let seed = "user_a__session-prefix-chain";
        let base = req_with(
            Some(seed),
            None,
            None,
            vec![
                msg("user", "user message content one"),
                msg("assistant", "assistant reply content one"),
                msg("user", "user message content two"),
            ],
        );
        let u1 = compute_fingerprint_usage(&base).unwrap();
        assert_eq!(u1.cache_read_input_tokens, 0);

        let extended = req_with(
            Some(seed),
            None,
            None,
            vec![
                msg("user", "user message content one"),
                msg("assistant", "assistant reply content one"),
                msg("user", "user message content two"),
                msg("assistant", "assistant reply content two"),
                msg("user", "user message content three"),
            ],
        );
        let u2 = compute_fingerprint_usage(&extended).unwrap();
        assert!(
            u2.cache_read_input_tokens > 0,
            "追加轮次后历史前缀段必须命中（read>0）"
        );
        assert!(
            u2.cache_creation_input_tokens > 0,
            "追加的新消息不是缓存覆盖的一部分，仍计 creation"
        );
    }

    /// TTL 过期：记录后过 TTL 再查询，不得命中。
    #[test]
    fn expired_entries_do_not_hit() {
        let m = CacheFingerprintMeter::default();
        let now = 1_000_000;
        m.record_at(&[0xAAAA], &[100], DEFAULT_TTL_SECS, now);
        assert_eq!(m.lookup_at(&[0xAAAA], now + DEFAULT_TTL_SECS + 1), vec![false], "过期不命中");
        assert_eq!(m.lookup_at(&[0xAAAA], now + 10), vec![true], "未过期命中");
    }

    /// 容量上限语义（抽样淘汰）：表长必须有界（cap + 淘汰间隔余量），且 LRU
    /// （last_hit_at）语义不变 —— 最旧条目最终被淘汰、最新条目保留。
    ///
    /// ⚠️ 2026-08-15 语义微调：改前每次超限都淘汰、表长严格回到 cap；抽样后
    /// 每 EVICT_INTERVAL 条超限插入才淘汰一次，表长上界 = cap + EVICT_INTERVAL − 1。
    /// 断言改测上界 + LRU 方向，而非「严格等于 cap」。
    #[test]
    fn capacity_evicts_least_recently_hit() {
        let m = CacheFingerprintMeter::default();
        let now = 1_000_000;
        // 塞 DEFAULT_CAPACITY + EVICT_INTERVAL + 10 条（ttl 用上限，查询时刻所有
        // 条目都未过期）：至少触发一轮淘汰，最早写入的条目（last_hit_at 最小）
        // 应被淘汰。
        let total = DEFAULT_CAPACITY + EVICT_INTERVAL + 10;
        for i in 0..total {
            m.record_at(&[i as u64 + 1], &[10], MAX_TTL_SECS, now + i as i64);
        }
        assert!(
            m.len() <= DEFAULT_CAPACITY + EVICT_INTERVAL - 1,
            "抽样淘汰后表长必须保持有界（cap + 淘汰间隔余量），实际 {}",
            m.len()
        );
        // 查询时刻取 now + total：最早写入的 1、2 已被淘汰，最新的 total 仍在。
        let hits = m.lookup_at(&[1, 2, total as u64], now + total as i64);
        assert_eq!(hits, vec![false, false, true], "LRU 语义：最旧被淘汰，最新保留");
    }

    /// 工具对话的 id 漂移（对抗审查 MAJOR 2）：Claude Code 每轮回传工具块时重新生成
    /// `tool_use.id` / `tool_result.tool_use_id`。签名必须剔除这些漂移字段 ——
    /// 否则工具类多轮对话的前缀链每轮全 miss、read 恒 0。
    #[test]
    fn tool_call_history_still_hits_despite_id_drift() {
        let mk = |id: &str| {
            req_with(
                Some("user_a__session-tools"),
                None,
                None,
                vec![
                    msg("user", "please look at the files"),
                    Message {
                        role: "assistant".into(),
                        content: serde_json::json!([
                            {"type": "tool_use", "id": id, "name": "read_file",
                             "input": {"file_path": "src/main.rs", "limit": 10}}
                        ]),
                    },
                    Message {
                        role: "user".into(),
                        content: serde_json::json!([
                            {"type": "tool_result", "tool_use_id": id, "content": "fn main() {}"}
                        ]),
                    },
                    msg("user", "now summarize"),
                ],
            )
        };
        let u1 = compute_fingerprint_usage(&mk("toolu_round_1")).unwrap();
        assert_eq!(u1.cache_read_input_tokens, 0);
        let u2 = compute_fingerprint_usage(&mk("toolu_round_2")).unwrap();
        assert!(
            u2.cache_read_input_tokens > 0,
            "工具块 id 每轮漂移也不得破坏命中（签名剔除 id）。read={}",
            u2.cache_read_input_tokens
        );
    }

    /// 同语义内容 + 不同 tool_use_id：必须仍命中（参考仓同名测试的移植）。
    #[test]
    fn tool_use_id_drift_with_same_semantic_content_still_hits() {
        let base = req_with(
            Some("user_a__session-tools-2"),
            None,
            None,
            vec![
                msg("user", "q"),
                Message {
                    role: "assistant".into(),
                    content: serde_json::json!([
                        {"type": "tool_use", "id": "a", "name": "edit_file",
                         "input": {"file_path": "x.rs", "old_str": "a", "new_str": "b"}}
                    ]),
                },
                msg("user", "done"),
            ],
        );
        let u1 = compute_fingerprint_usage(&base).unwrap();
        assert_eq!(u1.cache_read_input_tokens, 0);

        let drifted = req_with(
            Some("user_a__session-tools-2"),
            None,
            None,
            vec![
                msg("user", "q"),
                Message {
                    role: "assistant".into(),
                    content: serde_json::json!([
                        {"type": "tool_use", "id": "completely-new-id", "name": "edit_file",
                         "input": {"file_path": "x.rs", "old_str": "a", "new_str": "b"}}
                    ]),
                },
                msg("user", "done"),
            ],
        );
        let u2 = compute_fingerprint_usage(&drifted).unwrap();
        assert!(
            u2.cache_read_input_tokens > 0,
            "同语义内容 + 不同 tool_use_id 必须命中（canonical 签名不含 id）"
        );
    }

    /// thinking 块按内容哈希与计 token（对抗审查 MINOR 1）：thinking 内容不同 →
    /// 前缀段哈希必须不同（否则文本相同但推理不同的两轮会误报整段命中）。
    #[test]
    fn thinking_block_content_participates_in_hash() {
        let mk = |thinking: &str| {
            req_with(
                Some("user_a__session-thinking"),
                None,
                None,
                vec![
                    msg("user", "solve this"),
                    Message {
                        role: "assistant".into(),
                        content: serde_json::json!([
                            {"type": "thinking", "thinking": thinking},
                            {"type": "text", "text": "answer here"}
                        ]),
                    },
                    msg("user", "more"),
                ],
            )
        };
        let u1 = compute_fingerprint_usage(&mk("plan step one")).unwrap();
        assert_eq!(u1.cache_read_input_tokens, 0);
        let u2 = compute_fingerprint_usage(&mk("plan step two")).unwrap();
        // msg0（"solve this"）是两轮公共前缀，仍可命中（read>0）；判别点是
        // thinking 之后的段：若 thinking 不参与哈希（旧 bug：签名恒为 "thinking:"），
        // 第二轮会一路命中到 assistant 段 → creation 被吞成 0。故断言 creation>0。
        assert!(
            u2.cache_read_input_tokens > 0,
            "msg0 公共前缀段仍应命中"
        );
        assert!(
            u2.cache_creation_input_tokens > 0,
            "thinking 内容不同不得命中其后段（thinking 必须参与哈希，creation 不得为 0）"
        );
    }

    /// 与链上 clamp 语义兼容：指纹输出经 clamp_to_total 后满足不变量。
    #[test]
    fn fingerprint_usage_clamps_into_total() {
        let r = req_with(
            Some("user_a__session-clamp"),
            None,
            None,
            vec![
                msg("user", "user message content one"),
                msg("assistant", "assistant reply content one"),
                msg("user", "user message content two"),
            ],
        );
        let u = compute_fingerprint_usage(&r).unwrap();
        let total = 500;
        let c = u.clamp_to_total(total);
        assert!(c.cache_read_input_tokens + c.cache_creation_input_tokens <= total);
        assert_eq!(
            c.cache_creation_5m_input_tokens + c.cache_creation_1h_input_tokens,
            c.cache_creation_input_tokens
        );
    }

    /// 剥除函数的本地语义（移植自 k2cc）：完整标签对被剥除、前后文保留、
    /// 多个标签逐对剥除、未闭合的开始标签剥到文本末尾。
    #[test]
    fn strip_system_reminders_removes_full_tag_pairs() {
        assert_eq!(strip_system_reminders("plain text"), "plain text");
        assert_eq!(
            strip_system_reminders(
                "<system-reminder>context walkthrough</system-reminder>real question"
            ),
            "real question"
        );
        assert_eq!(
            strip_system_reminders(
                "a<system-reminder>one</system-reminder>b<system-reminder>two</system-reminder>c"
            ),
            "abc"
        );
        assert_eq!(strip_system_reminders("<system-reminder>no close"), "");
    }

    /// Kiro 每轮在历史 user 消息注入 `<system-reminder>`（内容含时间戳等逐轮漂移
    /// 字段）：指纹签名/计数必须剥除该标签对，否则前缀链每轮全 miss、read 恒 0。
    /// 剥除后签名与漂移无关 → 命中不被破坏（转发字节不受影响，仅影响指纹估算）。
    #[test]
    fn system_reminder_drift_in_history_does_not_break_hits() {
        let mk = |reminder: &str| {
            req_with(
                Some("user_a__session-system-reminder"),
                None,
                None,
                vec![
                    Message {
                        role: "user".into(),
                        content: serde_json::json!([
                            {"type": "text", "text": format!(
                                "<system-reminder>{}</system-reminder>history question",
                                reminder
                            )}
                        ]),
                    },
                    msg("assistant", "history answer"),
                    msg("user", "current turn"),
                ],
            )
        };
        let u1 = compute_fingerprint_usage(&mk("2026-08-15 10:00:00 walkthrough")).unwrap();
        assert_eq!(u1.cache_read_input_tokens, 0, "首轮无历史命中");
        let u2 = compute_fingerprint_usage(&mk("2026-08-15 10:00:05 other walkthrough")).unwrap();
        assert!(
            u2.cache_read_input_tokens > 0,
            "system-reminder 内容漂移不得破坏命中（剥除后前缀一致）。read={}",
            u2.cache_read_input_tokens
        );
    }
}
