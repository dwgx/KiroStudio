//! Token 计算模块
//!
//! 提供文本 token 数量计算功能。
//!
//! # 计算规则
//! - 非西文字符：每个计 4.5 个字符单位
//! - 西文字符：每个计 1 个字符单位
//! - 4 个字符单位 = 1 token（四舍五入）

use crate::anthropic::types::{
    CountTokensRequest, CountTokensResponse, Message, SystemMessage, Tool,
};
use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Count Tokens API 配置
#[derive(Clone, Default)]
pub struct CountTokensConfig {
    /// 外部 count_tokens API 地址
    pub api_url: Option<String>,
    /// count_tokens API 密钥
    pub api_key: Option<String>,
    /// count_tokens API 认证类型（"x-api-key" 或 "bearer"）
    pub auth_type: String,
    /// 代理配置
    pub proxy: Option<ProxyConfig>,

    pub tls_backend: TlsBackend,
}

/// 全局配置存储
static COUNT_TOKENS_CONFIG: OnceLock<CountTokensConfig> = OnceLock::new();

/// 初始化 count_tokens 配置
///
/// 应在应用启动时调用一次
pub fn init_config(config: CountTokensConfig) {
    let _ = COUNT_TOKENS_CONFIG.set(config);
}

/// 获取配置
fn get_config() -> Option<&'static CountTokensConfig> {
    COUNT_TOKENS_CONFIG.get()
}

/// 判断字符是否为非西文字符
///
/// 西文字符包括：
/// - ASCII 字符 (U+0000..U+007F)
/// - 拉丁字母扩展 (U+0080..U+024F)
/// - 拉丁字母扩展附加 (U+1E00..U+1EFF)
///
/// 返回 true 表示该字符是非西文字符（如中文、日文、韩文、阿拉伯文等）
fn is_non_western_char(c: char) -> bool {
    !matches!(c,
        // 基本 ASCII
        '\u{0000}'..='\u{007F}' |
        // 拉丁字母扩展-A (Latin Extended-A)
        '\u{0080}'..='\u{00FF}' |
        // 拉丁字母扩展-B (Latin Extended-B)
        '\u{0100}'..='\u{024F}' |
        // 拉丁字母扩展附加 (Latin Extended Additional)
        '\u{1E00}'..='\u{1EFF}' |
        // 拉丁字母扩展-C/D/E
        '\u{2C60}'..='\u{2C7F}' |
        '\u{A720}'..='\u{A7FF}' |
        '\u{AB30}'..='\u{AB6F}'
    )
}

/// 计算文本的 token 数量
///
/// # 计算规则
/// - 非西文字符：每个计 4.5 个字符单位
/// - 西文字符：每个计 1 个字符单位
/// - 4 个字符单位 = 1 token（四舍五入）
/// ```
pub fn count_tokens(text: &str) -> u64 {
    // println!("text: {}", text);

    let char_units: f64 = text
        .chars()
        .map(|c| if is_non_western_char(c) { 4.0 } else { 1.0 })
        .sum();

    let tokens = char_units / 4.0;

    let acc_token = if tokens < 100.0 {
        tokens * 1.5
    } else if tokens < 200.0 {
        tokens * 1.3
    } else if tokens < 300.0 {
        tokens * 1.25
    } else if tokens < 800.0 {
        tokens * 1.2
    } else {
        tokens * 1.0
    } as u64;

    // println!("tokens: {}, acc_tokens: {}", tokens, acc_token);
    acc_token
}

/// 估算请求的输入 tokens
///
/// 优先调用远程 API（结果带 60s 短 TTL 缓存，命中省掉整段 RTT），失败时回退到本地计算
pub(crate) fn count_all_tokens(
    model: &str,
    system: Option<&[SystemMessage]>,
    messages: &[Message],
    tools: Option<&[Tool]>,
) -> u64 {
    // 检查是否配置了远程 API
    if let Some(config) = get_config() {
        if config.api_url.is_some() {
            // 结果缓存：同一会话短时间内的重复请求（客户端重试 / 连续轮次）payload 基本
            // 不变，命中即跳过整个远程 RTT（含 block_in_place 等待）。key 用轻量哈希。
            let key = payload_hash(model, system, messages, tools);
            if let Some(tokens) = cached_remote_count(key) {
                return tokens;
            }
            // 尝试调用远程 API
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(call_remote_count_tokens(
                    config, model, system, messages, tools,
                ))
            });

            match result {
                Ok(tokens) => {
                    tracing::debug!("远程 count_tokens API 返回: {}", tokens);
                    store_remote_count(key, tokens);
                    return tokens;
                }
                Err(e) => {
                    tracing::warn!("远程 count_tokens API 调用失败，回退到本地计算: {}", e);
                }
            }
        }
    }

    // 本地计算
    count_all_tokens_local(system, messages, tools)
}

/// 远程 count_tokens 结果的短 TTL 缓存条目（60s 过期）。
struct CachedCount {
    expires_at: Instant,
    tokens: u64,
}

/// 结果缓存上限：超过后先清过期条目，仍超则整表清空（缓存只是优化，丢命中不影响正确性）。
const REMOTE_COUNT_CACHE_CAP: usize = 256;
/// 结果缓存 TTL：token 估算允许秒级陈旧（同 payload 的 token 数几乎不变）。
const REMOTE_COUNT_CACHE_TTL_SECS: u64 = 60;

static REMOTE_COUNT_CACHE: OnceLock<
    parking_lot::Mutex<std::collections::HashMap<u64, CachedCount>>,
> = OnceLock::new();

/// 命中未过期的远程结果则返回；顺带清理过期条目防止长期堆积。
fn cached_remote_count(key: u64) -> Option<u64> {
    let cache = REMOTE_COUNT_CACHE
        .get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()));
    let mut guard = cache.lock();
    let now = Instant::now();
    let hit = guard
        .get(&key)
        .filter(|c| c.expires_at > now)
        .map(|c| c.tokens);
    if hit.is_none() && guard.len() >= REMOTE_COUNT_CACHE_CAP {
        guard.retain(|_, c| c.expires_at > now);
        if guard.len() >= REMOTE_COUNT_CACHE_CAP {
            guard.clear();
        }
    }
    hit
}

/// 写入远程成功结果（带过期时间）。失败结果不缓存——下次仍会重试远程。
fn store_remote_count(key: u64, tokens: u64) {
    let cache = REMOTE_COUNT_CACHE
        .get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()));
    let mut guard = cache.lock();
    let now = Instant::now();
    if guard.len() >= REMOTE_COUNT_CACHE_CAP {
        guard.retain(|_, c| c.expires_at > now);
        if guard.len() >= REMOTE_COUNT_CACHE_CAP {
            guard.clear();
        }
    }
    guard.insert(
        key,
        CachedCount {
            expires_at: now + Duration::from_secs(REMOTE_COUNT_CACHE_TTL_SECS),
            tokens,
        },
    );
}

/// 对 (model + system + messages + tools) 算轻量哈希（SipHash-1-3，DefaultHasher）。
///
/// 只哈希文本特征（role / 文本 / 工具名 / 描述 / input_schema JSON + 消息块整体序列化），
/// 遍历成本是 O(payload) 的纯内存操作（与本地估算同量级），远小于一次远程 RTT。
/// ⚠️ 块整体序列化（2026-08-13 对抗审查 M1）：只 hash text 会丢 image/tool_use 维度，
/// 同文本不同内容确定性撞 key（不是概率碰撞，是键构造缺维度）。
fn payload_hash(
    model: &str,
    system: Option<&[SystemMessage]>,
    messages: &[Message],
    tools: Option<&[Tool]>,
) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    model.hash(&mut h);
    if let Some(sys) = system {
        for m in sys {
            m.text.hash(&mut h);
        }
    }
    for msg in messages {
        msg.role.hash(&mut h);
        match &msg.content {
            serde_json::Value::String(s) => s.hash(&mut h),
            serde_json::Value::Array(arr) => {
                for item in arr {
                    // ⚠️ 2026-08-13 对抗审查 M1：必须把块**整体**纳入哈希——只 hash
                    // `text` 会丢 image（无 text 字段）与 tool_use（input 正文）维度，
                    // 同文本不同图/不同工具输入会撞同一缓存 key，返回错误 token 估算
                    // （图片每张 ~1000+ tokens，apply_patch 的 patch 可达数千）。
                    // 用序列化保证全维度（O(payload) 纯内存，远小于远程 RTT）。
                    let serialized = serde_json::to_string(item).unwrap_or_default();
                    serialized.hash(&mut h);
                }
            }
            _ => {}
        }
    }
    if let Some(ts) = tools {
        for t in ts {
            t.name.hash(&mut h);
            t.description.hash(&mut h);
            let schema = serde_json::to_string(&t.input_schema).unwrap_or_default();
            schema.hash(&mut h);
        }
    }
    h.finish()
}

/// 远程 count_tokens 的 reqwest client：进程级复用一份连接池。
///
/// 对齐透传层（`passthrough_client`）的 client 缓存范式——原实现每请求新建 client，
/// 等于每请求重开 TCP + 重做 TLS 握手（1-2 RTT），并把 RTT 叠进 block_in_place 等待里。
/// count_tokens 配置是进程级一次性初始化（`init_config`），client 与 (proxy, tls) 绑定
/// 且永不变化，单例缓存足够。构建失败也缓存（配置级错误，重试无意义），调用方回退本地。
static REMOTE_COUNT_CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();

fn cached_remote_count_client() -> Result<&'static reqwest::Client, String> {
    let cached = REMOTE_COUNT_CLIENT.get_or_init(|| {
        let config = match get_config() {
            Some(c) => c,
            None => return Err("count_tokens 配置未初始化".to_string()),
        };
        build_client(config.proxy.as_ref(), 300, config.tls_backend).map_err(|e| e.to_string())
    });
    match cached {
        Ok(c) => Ok(c),
        Err(e) => Err(e.clone()),
    }
}

/// 调用远程 count_tokens API
async fn call_remote_count_tokens(
    config: &CountTokensConfig,
    model: &str,
    system: Option<&[SystemMessage]>,
    messages: &[Message],
    tools: Option<&[Tool]>,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let client = cached_remote_count_client()?;
    let api_url = config.api_url.as_deref().ok_or("count_tokens API 未配置")?;

    // 构建请求体（远程 API 需要 owned 值，clone 仅发生在这条真正走网络的分支）
    let request = CountTokensRequest {
        model: model.to_string(), // 模型名称用于 token 计算
        messages: messages.to_vec(),
        system: system.map(|s| s.to_vec()),
        tools: tools.map(|t| t.to_vec()),
    };

    // 构建请求
    let mut req_builder = client.post(api_url);

    // 设置认证头
    if let Some(api_key) = &config.api_key {
        if config.auth_type == "bearer" {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", api_key));
        } else {
            req_builder = req_builder.header("x-api-key", api_key);
        }
    }

    // 发送请求
    let response = req_builder
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!("API 返回错误状态: {}", response.status()).into());
    }

    let result: CountTokensResponse = response.json().await?;
    Ok(result.input_tokens as u64)
}

/// 本地计算请求的输入 tokens
///
/// `pub(crate)`:自定义 API 透传路径埋点专用——透传要低延迟原样中转,不能像 Kiro 路径那样
/// 走可能阻塞 TTFB 的远程 count_tokens API,故直接用本地估算(诚实边界:埋点 token 本就是估算)。
pub(crate) fn count_all_tokens_local(
    system: Option<&[SystemMessage]>,
    messages: &[Message],
    tools: Option<&[Tool]>,
) -> u64 {
    count_all_tokens_local_unfloored(system, messages, tools).max(1)
}

/// [`count_all_tokens_local`] 的无下限版本：真实为 0 时返回 0，不抬到 1。
///
/// 拆出来的理由：`.max(1)` 是给「请求输入 token」用的（一次请求不可能是 0 token，
/// 落库 0 会让面板出现除零/空值）。但前缀估算要把它**相加**，
/// 而 `.max(1)` 在每次调用上各抬 1 → 空 system + 空历史被算成 2 个「已缓存」token，
/// 于是 `estimate_cache_breakdown` 的 `prefix_tokens > 0` 闸门被这个幽灵值顶开，
/// 客户端收到 `cache_read_input_tokens: 2` 而实际什么都没缓存。
fn count_all_tokens_local_unfloored(
    system: Option<&[SystemMessage]>,
    messages: &[Message],
    tools: Option<&[Tool]>,
) -> u64 {
    let mut total = 0;

    // 系统消息
    if let Some(system) = system {
        for msg in system {
            total += count_tokens(&msg.text);
        }
    }

    // 用户消息
    for msg in messages {
        if let serde_json::Value::String(s) = &msg.content {
            total += count_tokens(s);
        } else if let serde_json::Value::Array(arr) = &msg.content {
            for item in arr {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    total += count_tokens(text);
                }
            }
        }
    }

    // 工具定义
    if let Some(tools) = tools {
        for tool in tools {
            total += count_tokens(&tool.name);
            total += count_tokens(&tool.description);
            let input_schema_json = serde_json::to_string(&tool.input_schema).unwrap_or_default();
            total += count_tokens(&input_schema_json);
        }
    }

    total
}

/// 估算稳定前缀（系统提示 + 历史轮次）占用的 token 数。
///
/// Bedrock prefix cache 缓存的是 [system_prompt] + [messages[0..len-1]]——即当前 user
/// 消息之前的所有内容。当 `agentContinuationId` 固定（同一会话），连续请求会命中该缓存。
///
/// 返回 0 表示第一轮（无历史，缓存尚未建立）；返回正值表示可估算的 cache_read 量。
///
/// # 边界必须与 converter 的转发切片对齐
///
/// `converter::convert_request` 发给上游的历史不是 `messages[..len-1]`：它先做 prefill
/// 预处理（末尾非 user 时截断到**最后一条 user**，`converter.rs:861-871`），
/// 再由 `build_history` 去掉末尾那条 user 作 currentMessage（`converter.rs:1583`）。
/// 即真实历史 = `messages[..last_user_idx]`。
///
/// 本函数原先直接砍末尾一条，对 prefill 载荷（末尾是 assistant）就会把**当前轮**的
/// user 消息算进「已缓存前缀」——那条消息此刻正第一次发给上游，不可能已被缓存。
/// 现改为按 role 定位 `last_user_idx`，与 converter 同口径。
pub(crate) fn count_prefix_tokens(
    system: Option<&[crate::anthropic::types::SystemMessage]>,
    messages: &[crate::anthropic::types::Message],
) -> i32 {
    // 与 converter 的 prefill 预处理同口径：末尾非 user 时，边界是最后一条 user 的下标。
    // 找不到任何 user 消息时 converter 直接报 EmptyMessages，这里保守返回 0。
    let Some(last_user_idx) = messages.iter().rposition(|m| m.role == "user") else {
        return 0;
    };
    let history_slice = &messages[..last_user_idx];

    // 第一轮：没有历史前缀，prefix cache 尚未建立，保守返回 0。
    // 注意这里判的是**历史切片**为空，而不是 `messages.len() <= 1`——
    // prefill 载荷 `[user, assistant]` 长度为 2 但真实历史仍是空的。
    if history_slice.is_empty() {
        return 0;
    }

    // 用无下限版本相加：两次 `.max(1)` 会凭空造出 2 个「已缓存」token（见
    // `count_all_tokens_local_unfloored` 的说明）。
    let sys_tokens = count_all_tokens_local_unfloored(system, &[], None);
    let hist_tokens = count_all_tokens_local_unfloored(None, history_slice, None);
    (sys_tokens + hist_tokens) as i32
}

// 注：TOKENS_PER_TOOL / count_system_message_tokens / count_tool_definition_tokens /
// count_message_content_tokens / estimate_content_block_tokens 原仅供影子缓存记账
// （cache_tracker）按块累计使用。影子缓存已整体移除，这些辅助函数一并删除。
// 请求路径的输入 token 估算走 count_all_tokens_local（字符数/4），不受影响。

#[cfg(test)]
mod prefix_tokens_tests {
    use super::*;
    use crate::anthropic::types::{Message, SystemMessage};

    fn msg(role: &str, text: &str) -> Message {
        Message {
            role: role.to_string(),
            content: serde_json::Value::String(text.to_string()),
        }
    }

    fn sys(text: &str) -> Vec<SystemMessage> {
        vec![SystemMessage {
            text: text.to_string(),
            block_type: None,
            cache_control: None,
        }]
    }

    /// 单轮请求没有历史前缀 → 0（保持既有契约）。
    #[test]
    fn should_return_zero_for_single_turn() {
        let m = vec![msg("user", "hello")];
        assert_eq!(count_prefix_tokens(Some(&sys("you are helpful")), &m), 0);
    }

    /// 🔴 核心回归：prefill 载荷（末尾 assistant）的历史仍是空的。
    ///
    /// 旧实现砍末尾一条得到 `[user]`，把**当前轮**的 user 消息算成已缓存前缀，
    /// 于是长 system + 长 user 的首轮请求会虚报一个很大的 cache_read。
    #[test]
    fn should_return_zero_for_prefill_payload_whose_history_is_empty() {
        let m = vec![msg("user", "hello"), msg("assistant", "partial prefill")];
        // 长 system，确保旧实现一定返回一个显著的正值（而非恰好为 0 的巧合）
        let system = sys(&"stable system instructions. ".repeat(200));
        assert_eq!(
            count_prefix_tokens(Some(&system), &m),
            0,
            "末尾 assistant 时真实历史为空，不应把当前轮 user 计入已缓存前缀"
        );
    }

    /// 🔴 分支顺序守卫：prefill 截断必须发生在「历史是否为空」判定**之前**。
    ///
    /// `[u, a, u, a]` 的真实转发历史是 `[u, a]`（截断到最后一条 user 再去掉它），
    /// 而旧实现给出 `[u, a, u]` —— 多算了一整轮。两者都非 0，所以只有比较**数值**
    /// 才能抓住；只断言 `> 0` 的测试会双双通过而放过缺陷。
    #[test]
    fn should_cut_history_at_last_user_not_at_last_message() {
        let long = "z".repeat(4000);
        let m = vec![
            msg("user", "turn1-user"),
            msg("assistant", "turn1-assistant"),
            msg("user", &long), // 当前轮 user：绝不该计入前缀
            msg("assistant", "prefill tail"),
        ];
        let got = count_prefix_tokens(None, &m);

        // 期望值 = 只含前两条的历史
        let expect = count_prefix_tokens(
            None,
            &[
                msg("user", "turn1-user"),
                msg("assistant", "turn1-assistant"),
                msg("user", "current"),
            ],
        );
        assert_eq!(got, expect, "边界应落在最后一条 user 之前");

        // 并且必须显著小于「把当前轮 user 也算进去」的旧口径
        let old_style = count_all_tokens_local_unfloored(None, &m[..m.len() - 1], None) as i32;
        assert!(
            got < old_style,
            "旧口径 {} 含了 4000 字符的当前轮 user，新口径 {} 应显著更小",
            old_style,
            got
        );
    }

    /// 幽灵 token：空 system + 内容为空的历史不应产出正的 cache_read。
    ///
    /// 旧实现两次 `.max(1)` 各抬 1 → 返回 2，而 `estimate_cache_breakdown` 只要
    /// `prefix_tokens > 0` 就下发 `cache_read_input_tokens`，于是客户端看到
    /// 「命中 2 tokens」而实际什么都没有。
    #[test]
    fn should_not_invent_phantom_tokens_when_prefix_is_empty() {
        let m = vec![
            msg("user", ""),
            msg("assistant", ""),
            msg("user", "current question"),
        ];
        assert_eq!(
            count_prefix_tokens(None, &m),
            0,
            "空前缀不应产出正值（两次 .max(1) 的幽灵值）"
        );
    }

    /// 正常多轮：有真实历史时仍返回正值（防止修复过度收敛成恒 0）。
    #[test]
    fn should_count_real_history_and_system() {
        let m = vec![
            msg("user", &"question one ".repeat(50)),
            msg("assistant", &"answer one ".repeat(50)),
            msg("user", "question two"),
        ];
        let with_sys = count_prefix_tokens(Some(&sys(&"rules ".repeat(50))), &m);
        let without_sys = count_prefix_tokens(None, &m);
        assert!(without_sys > 0, "有历史应返回正值，实际 {}", without_sys);
        assert!(
            with_sys > without_sys,
            "system 应计入前缀：{} 应大于 {}",
            with_sys,
            without_sys
        );
    }

    /// 无 user 消息（converter 会报 EmptyMessages）时保守返回 0，不 panic。
    #[test]
    fn should_return_zero_when_no_user_message_exists() {
        let m = vec![msg("assistant", "orphan"), msg("assistant", "another")];
        assert_eq!(count_prefix_tokens(Some(&sys("sys")), &m), 0);
    }

    /// `count_all_tokens_local` 的对外下限契约不变（拆函数不能改公开行为）。
    #[test]
    fn should_keep_floor_of_one_on_public_local_counter() {
        assert_eq!(count_all_tokens_local(None, &[], None), 1);
        assert_eq!(count_all_tokens_local_unfloored(None, &[], None), 0);
    }
}

/// 估算输出 tokens
pub(crate) fn estimate_output_tokens(content: &[serde_json::Value]) -> i32 {
    let mut total = 0;

    for block in content {
        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
            total += count_tokens(text) as i32;
        }
        if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
            // 工具调用开销
            if let Some(input) = block.get("input") {
                let input_str = serde_json::to_string(input).unwrap_or_default();
                total += count_tokens(&input_str) as i32;
            }
        }
    }

    total.max(1)
}
