//! Token 计算模块
//!
//! 提供文本 token 数量计算功能。
//!
//! # 计算规则
//! - 非西文字符：每个计 4.5 个字符单位（与 fuckopencode 4.5 口径对齐）
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
///
/// 返回 `CountTokensConfig` 的**拷贝**（非引用）：测试注入的配置存在 Mutex 里
/// （无 'static 生命周期可返回），统一按值返回让生产/测试两条路径同构。
/// CountTokensConfig 含 `Option<ProxyConfig>`（非 Copy），用 Clone。
fn get_config() -> Option<CountTokensConfig> {
    #[cfg(test)]
    {
        if let Some(config) = COUNT_TOKENS_CONFIG_FOR_TEST.lock().cloned() {
            return Some(config);
        }
    }
    COUNT_TOKENS_CONFIG.get().cloned()
}

/// 测试注入的配置（仅测试构建）：优先于生产 [`COUNT_TOKENS_CONFIG`]。
///
/// 生产 `OnceLock` 一次性不可复位 → 测试用独立 Mutex + Box::leak 覆盖注入；
/// 注入串行化由 `remote_count_tests::REMOTE_COUNT_TEST_LOCK` 保证
/// （对标 machine_id 双 HashMap 无锁污染的教训）。
#[cfg(test)]
static COUNT_TOKENS_CONFIG_FOR_TEST: parking_lot::Mutex<Option<&'static CountTokensConfig>> =
    parking_lot::Mutex::new(None);

/// 测试注入的远程 client（仅测试构建），语义同 [`COUNT_TOKENS_CONFIG_FOR_TEST`]。
#[cfg(test)]
static REMOTE_COUNT_CLIENT_FOR_TEST: parking_lot::Mutex<
    Option<Result<&'static reqwest::Client, String>>,
> = parking_lot::Mutex::new(None);

/// 测试注入：整体替换远程 count_tokens 的 config 与 client（仅测试构建）。
///
/// 注入后 `get_config` / `cached_remote_count_client` 走测试分支，生产
/// `OnceLock` 不被触碰（REMOTE_COUNT_CLIENT 缓存不会被测试 client 污染）。
#[cfg(test)]
pub(crate) fn set_count_tokens_config_for_test(config: CountTokensConfig, client: reqwest::Client) {
    let leaked_config: &'static CountTokensConfig = Box::leak(Box::new(config));
    *COUNT_TOKENS_CONFIG_FOR_TEST.lock() = Some(leaked_config);
    let leaked_client: &'static reqwest::Client = Box::leak(Box::new(client));
    *REMOTE_COUNT_CLIENT_FOR_TEST.lock() = Some(Ok(leaked_client));
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
/// - 非西文字符：每个计 4.5 个字符单位（与 fuckopencode 对齐）
/// - 西文字符：每个计 1 个字符单位
/// - 4 个字符单位 = 1 token（四舍五入）
/// ```
pub fn count_tokens(text: &str) -> u64 {
    // println!("text: {}", text);

    let char_units: f64 = text
        .chars()
        .map(|c| if is_non_western_char(c) { 4.5 } else { 1.0 })
        .sum();

    let tokens = char_units / 4.0;

    // 🔴 2026-08-15：`as u64` 是**截断**，与注释「四舍五入」矛盾（如 0.75 → 0）。
    // 改 `.round()` 与注释一致 —— 估算层的小数舍入误差在长文本上无感知，
    // 但语义与文档口径统一。
    let acc_token = (if tokens < 100.0 {
        tokens * 1.5
    } else if tokens < 200.0 {
        tokens * 1.3
    } else if tokens < 300.0 {
        tokens * 1.25
    } else if tokens < 800.0 {
        tokens * 1.2
    } else {
        tokens * 1.0
    })
    .round() as u64;

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
                tokio::runtime::Handle::current().block_on(async {
                    let client = cached_remote_count_client()?;
                    call_remote_count_tokens(client, &config, model, system, messages, tools)
                        .await
                })
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
/// 远程 count_tokens 调用的总超时秒数（2026-08-15：300s 可拖死请求热路径）。
///
/// `count_all_tokens` 走 `block_in_place` 同步等待 —— 远程 API 故障/慢响应时
/// 300s 的总超时会让整条请求热路径挂 5 分钟。收到 10s：自建 count_tokens 服务
/// 通常同机房、秒级响应；慢网络下超时 → 调用方回退本地估算，语义不变（估算
/// 精度略降，正确性不受影响）。
const REMOTE_COUNT_TIMEOUT_SECS: u64 = 10;

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
                    let serialized = serde_json::to_string(&item).unwrap_or_default();
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
    #[cfg(test)]
    {
        if let Some(cached) = REMOTE_COUNT_CLIENT_FOR_TEST.lock().as_ref() {
            return match cached {
                Ok(client) => Ok(*client),
                Err(e) => Err(e.clone()),
            };
        }
    }
    let cached = REMOTE_COUNT_CLIENT.get_or_init(|| {
        let config = match get_config() {
            Some(c) => c,
            None => return Err("count_tokens 配置未初始化".to_string()),
        };
        // 🔴 2026-08-15：总超时 300s → REMOTE_COUNT_TIMEOUT_SECS(10s) —— 远程
        // count_tokens 故障时不再拖死请求热路径；超时失败由调用方回退本地估算。
        build_client(config.proxy.as_ref(), REMOTE_COUNT_TIMEOUT_SECS, config.tls_backend)
            .map_err(|e| e.to_string())
    });
    match cached {
        Ok(c) => Ok(c),
        Err(e) => Err(e.clone()),
    }
}

/// 调用远程 count_tokens API
///
/// client 由调用方传入：生产路径传进程级复用连接池的 client
/// （[`cached_remote_count_client`]），测试路径传指向本地 mock 的 client ——
/// 远程执行体与全局状态解耦，这是该路径可测试注入的关键。
async fn call_remote_count_tokens(
    client: &reqwest::Client,
    config: &CountTokensConfig,
    model: &str,
    system: Option<&[SystemMessage]>,
    messages: &[Message],
    tools: Option<&[Tool]>,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
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
            total += count_tokens(s.as_str());
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

    /// MINOR 8 守卫：token 折算必须**四舍五入**而非截断（与注释口径一致）。
    ///
    /// 2 个西文字符 = 2/4 = 0.5 字符单位 → ×1.5 = 0.75 → round = 1；
    /// 旧实现 `as u64` 截断得 0。回退即 FAIL：把 `.round()` 改回 `as u64`。
    #[test]
    fn token_count_rounds_instead_of_truncating() {
        assert_eq!(count_tokens("ab"), 1, "0.75 应四舍五入为 1（截断则为 0）");
        assert_eq!(count_tokens("a"), 0, "0.375 四舍五入为 0");
        // 大文本下舍入与截断的差异应 < 1（不放大误差）。
        let long = "a".repeat(4000); // 1000 token 基数
        let n = count_tokens(&long);
        assert!((1000..=1000 + 1).contains(&n), "长文本整数边界不受舍入影响: {n}");
    }

    /// 非西文 4.5 字符单位（fuckopencode 口径）。10 个汉字：
    /// 10×4.5/4 = 11.25，×1.5（<100）= 16.875 → round 17。
    /// 旧 4.0 口径会得到 15。
    #[test]
    fn cjk_uses_four_point_five_char_units() {
        let text = "一二三四五六七八九十";
        assert_eq!(text.chars().count(), 10);
        let n = count_tokens(text);
        assert_eq!(n, 17, "10 汉字 4.5 口径应为 17，实际 {n}");
    }

    /// MINOR 9 守卫：远程 count_tokens 总超时必须保持秒级（防被改回 300s 拖死热路径）。
    #[test]
    fn remote_count_timeout_stays_second_scale() {
        assert!(
            (1..=10).contains(&REMOTE_COUNT_TIMEOUT_SECS),
            "远程 count_tokens 总超时必须 ≤10s，当前 {}",
            REMOTE_COUNT_TIMEOUT_SECS
        );
    }
}

/// 远程 count_tokens 路径测试（2026-08-15 补：此前该路径零测试覆盖）。
///
/// 注入机制：`set_count_tokens_config_for_test` 替换 config + client，测试用
/// 本地 TCP mock 应答，**真实走网络路径**（reqwest → HTTP → mock server），
/// 不是跳过/打桩。`count_all_tokens` 走 `block_in_place`，故全部用
/// multi_thread runtime。
#[cfg(test)]
mod remote_count_tests {
    use super::*;
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 全局注入（config/client 注入 + 远程结果缓存）串行锁：所有触碰全局状态的
    /// 远程路径测试互斥执行，防止并行测试互相污染。
    static REMOTE_COUNT_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    fn msg(role: &str, text: &str) -> Message {
        Message {
            role: role.to_string(),
            content: serde_json::Value::String(text.to_string()),
        }
    }

    fn test_config(api_url: &str) -> CountTokensConfig {
        CountTokensConfig {
            api_url: Some(api_url.to_string()),
            api_key: None,
            auth_type: "x-api-key".to_string(),
            proxy: None,
            tls_backend: TlsBackend::default(),
        }
    }

    /// 测试 client：超时策略可注入 —— Hang mock 用短超时快速验证超时回退，
    /// 不必真等生产 REMOTE_COUNT_TIMEOUT_SECS(10s)。no_proxy() 确保打到本地
    /// mock（本机有全局代理时 reqwest 默认可能走系统代理）。
    fn test_client(timeout: Duration) -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(timeout)
            .no_proxy()
            .build()
            .unwrap()
    }

    #[derive(Clone)]
    enum MockResponse {
        Success(u64),
        Status(u16),
        /// 收到请求后挂起 30s 不响应（模拟远程无响应），由 client 超时触发回退。
        Hang,
    }

    /// 本地 TCP mock：极简 HTTP 服务，按 [`MockResponse`] 行为应答并计数请求。
    struct MockServer {
        addr: SocketAddr,
        request_count: Arc<AtomicUsize>,
    }

    impl MockServer {
        async fn start(resp: MockResponse) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let request_count = Arc::new(AtomicUsize::new(0));
            let count = request_count.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        break;
                    };
                    let count = count.clone();
                    let resp = resp.clone();
                    tokio::spawn(async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        let mut buf = vec![0u8; 4096];
                        let _ = sock.read(&mut buf).await;
                        let (status_line, body) = match resp {
                            MockResponse::Success(tokens) => (
                                "HTTP/1.1 200 OK".to_string(),
                                format!("{{\"input_tokens\":{}}}", tokens),
                            ),
                            MockResponse::Status(code) => (
                                format!("HTTP/1.1 {} Mock", code),
                                r#"{"input_tokens":0}"#.to_string(),
                            ),
                            MockResponse::Hang => {
                                tokio::time::sleep(Duration::from_secs(30)).await;
                                return;
                            }
                        };
                        let response = format!(
                            "{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            status_line,
                            body.len(),
                            body
                        );
                        let _ = sock.write_all(response.as_bytes()).await;
                    });
                }
            });
            MockServer {
                addr,
                request_count,
            }
        }

        fn requests(&self) -> usize {
            self.request_count.load(Ordering::SeqCst)
        }
    }

    /// 远程成功路径：mock 返回值原样透出，且真的打了一次远程。
    #[tokio::test(flavor = "multi_thread")]
    async fn remote_count_success_returns_mock_value() {
        let _guard = REMOTE_COUNT_TEST_LOCK.lock();
        let mock = MockServer::start(MockResponse::Success(1234)).await;
        set_count_tokens_config_for_test(
            test_config(&format!("http://{}", mock.addr)),
            test_client(Duration::from_secs(2)),
        );

        let messages = vec![msg("user", "hello world")];
        assert_eq!(count_all_tokens("test-model", None, &messages, None), 1234);
        assert_eq!(mock.requests(), 1, "成功路径必须真打远程一次");
    }

    /// 缓存命中路径：同 payload 两次调用，第二次命中 payload_hash 缓存跳过远程。
    ///
    /// 同时钉死 payload_hash 稳定性 —— 若哈希不稳定（同 payload 不同 key），
    /// 第二次调用会再打远程，本测试即红。
    #[tokio::test(flavor = "multi_thread")]
    async fn remote_count_cache_hit_skips_second_call() {
        let _guard = REMOTE_COUNT_TEST_LOCK.lock();
        let mock = MockServer::start(MockResponse::Success(4321)).await;
        set_count_tokens_config_for_test(
            test_config(&format!("http://{}", mock.addr)),
            test_client(Duration::from_secs(2)),
        );

        let messages = vec![msg("user", "cache me please")];
        assert_eq!(
            count_all_tokens("cache-model", None, &messages, None),
            4321
        );
        assert_eq!(mock.requests(), 1);
        assert_eq!(
            count_all_tokens("cache-model", None, &messages, None),
            4321
        );
        assert_eq!(mock.requests(), 1, "同 payload 第二次调用必须命中缓存，跳过远程");
    }

    /// payload_hash 差分：同 payload 稳定、不同 payload（文本/role/工具）不同。
    #[test]
    fn payload_hash_is_stable_and_differentiates_payloads() {
        let m1 = vec![msg("user", "stable payload")];
        let h1a = payload_hash("m", None, &m1, None);
        let h1b = payload_hash("m", None, &m1, None);
        assert_eq!(h1a, h1b, "同 payload 哈希必须稳定（缓存命中依赖此性质）");
        assert_ne!(
            payload_hash("m", None, &[msg("user", "stable payload!")], None),
            h1a,
            "文本不同哈希必须不同"
        );
        assert_ne!(
            payload_hash("m", None, &[msg("assistant", "stable payload")], None),
            h1a,
            "role 不同哈希必须不同"
        );
        let tool = Tool {
            tool_type: None,
            name: "t".to_string(),
            description: "d".to_string(),
            input_schema: HashMap::new(),
            max_uses: None,
            cache_control: None,
        };
        assert_ne!(
            payload_hash("m", None, &m1, Some(&[tool])),
            h1a,
            "工具不同哈希必须不同"
        );
    }

    /// 远程失败路径（5xx）：回退本地估算；失败结果不缓存，下次仍重试远程。
    #[tokio::test(flavor = "multi_thread")]
    async fn remote_count_error_falls_back_to_local_estimate() {
        let _guard = REMOTE_COUNT_TEST_LOCK.lock();
        let mock = MockServer::start(MockResponse::Status(500)).await;
        set_count_tokens_config_for_test(
            test_config(&format!("http://{}", mock.addr)),
            test_client(Duration::from_secs(2)),
        );

        let messages = vec![msg("user", "fallback me")];
        let expected = count_all_tokens_local(None, &messages, None);
        assert_eq!(
            count_all_tokens("fail-model", None, &messages, None),
            expected
        );
        assert_eq!(mock.requests(), 1);
        assert_eq!(
            count_all_tokens("fail-model", None, &messages, None),
            expected
        );
        assert_eq!(mock.requests(), 2, "失败结果不应缓存，下次调用应重试远程");
    }

    /// 远程超时路径：mock 挂起 + 注入短超时 client → 回退本地估算。
    #[tokio::test(flavor = "multi_thread")]
    async fn remote_count_timeout_falls_back_to_local_estimate() {
        let _guard = REMOTE_COUNT_TEST_LOCK.lock();
        let mock = MockServer::start(MockResponse::Hang).await;
        set_count_tokens_config_for_test(
            test_config(&format!("http://{}", mock.addr)),
            test_client(Duration::from_millis(200)),
        );

        let messages = vec![msg("user", "timeout me")];
        let expected = count_all_tokens_local(None, &messages, None);
        assert_eq!(
            count_all_tokens("timeout-model", None, &messages, None),
            expected
        );
        assert_eq!(mock.requests(), 1);
    }

    /// 并发场景：8 任务并发同 payload，结果一致（缓存读写无撕裂），并发结束后
    /// 缓存收敛 —— 单次调用不再打远程。
    ///
    /// 注：现状 check-then-act（cached_remote_count / store_remote_count 两把独立
    /// 锁）不保证并发 miss 恰好打一次远程，故请求数断言取 [1,8] 区间而非 ==1；
    /// 单飞去重不在本次注入化范围。
    #[tokio::test(flavor = "multi_thread")]
    async fn remote_count_concurrent_calls_share_cache_without_tearing() {
        let _guard = REMOTE_COUNT_TEST_LOCK.lock();
        let mock = MockServer::start(MockResponse::Success(777)).await;
        set_count_tokens_config_for_test(
            test_config(&format!("http://{}", mock.addr)),
            test_client(Duration::from_secs(2)),
        );

        let messages = vec![msg("user", "concurrent payload")];
        let mut handles = Vec::new();
        for _ in 0..8 {
            let messages = messages.clone();
            handles.push(tokio::spawn(async move {
                count_all_tokens("conc-model", None, &messages, None)
            }));
        }
        for handle in handles {
            assert_eq!(handle.await.unwrap(), 777, "并发结果必须一致且等于远程值");
        }
        let during = mock.requests();
        assert!(
            (1..=8).contains(&during),
            "并发 miss 请求数应在 [1,8] 内，实际 {}",
            during
        );
        assert_eq!(
            count_all_tokens("conc-model", None, &messages, None),
            777
        );
        assert_eq!(mock.requests(), during, "并发结束后缓存已填充，不应再打远程");
    }
}

/// 估算输出 tokens
pub(crate) fn estimate_output_tokens(content: &[serde_json::Value]) -> i32 {
    let mut total = 0;

    for block in content {
        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
            total += count_tokens(text) as i32;
        }
        if block.get("type").and_then(|v| v.as_str()) == Some("thinking") {
            // thinking 块计入 output_tokens（与流式路径口径对齐：Anthropic 语义中
            // output_tokens 含思考内容；签名是元数据不算）。
            if let Some(text) = block.get("thinking").and_then(|v| v.as_str()) {
                total += count_tokens(text) as i32;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_output_tokens_counts_thinking_blocks() {
        let with_thinking = serde_json::from_str::<Vec<serde_json::Value>>(
            r#"[
                {"type":"text","text":"hello"},
                {"type":"thinking","thinking":"deep thought","signature":"sig"}
            ]"#,
        )
        .unwrap();
        let without_thinking = serde_json::from_str::<Vec<serde_json::Value>>(
            r#"[{"type":"text","text":"hello"}]"#,
        )
        .unwrap();
        let with_t = estimate_output_tokens(&with_thinking);
        let without_t = estimate_output_tokens(&without_thinking);
        // thinking 块必须计入 output_tokens
        assert!(with_t > without_t, "thinking 块必须计入 output_tokens");
        // 差分 = thinking 文本 token 数（"deep thought" 12 字符 ≈ 3 token）
        assert!(
            with_t - without_t >= 2,
            "差分应反映 thinking 文本（got {} - {} = {}）",
            with_t,
            without_t,
            with_t - without_t
        );
    }
}

