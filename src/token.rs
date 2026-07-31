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
/// 优先调用远程 API，失败时回退到本地计算
pub(crate) fn count_all_tokens(
    model: &str,
    system: Option<&[SystemMessage]>,
    messages: &[Message],
    tools: Option<&[Tool]>,
) -> u64 {
    // 检查是否配置了远程 API
    if let Some(config) = get_config() {
        if let Some(api_url) = &config.api_url {
            // 尝试调用远程 API
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(call_remote_count_tokens(
                    api_url, config, model, system, messages, tools,
                ))
            });

            match result {
                Ok(tokens) => {
                    tracing::debug!("远程 count_tokens API 返回: {}", tokens);
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

/// 调用远程 count_tokens API
async fn call_remote_count_tokens(
    api_url: &str,
    config: &CountTokensConfig,
    model: &str,
    system: Option<&[SystemMessage]>,
    messages: &[Message],
    tools: Option<&[Tool]>,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let client = build_client(config.proxy.as_ref(), 300, config.tls_backend)?;

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

    total.max(1)
}

/// 可缓存前缀占总输入的上限比例（百分之几）。
///
/// ⚠️ 这是防「客户端自动压缩永久失效」的关键闸门，不是美观调节。
///
/// 影子缓存估算出的 prefix 会经 `billed_input_tokens` 从 `input_tokens` 里扣掉。
/// 若不设上限，长会话的稳定前缀几乎等于全量输入（system + 除末条外的全部历史），
/// 扣完 `usage.input_tokens` 就是 **0**。客户端（Codex/Claude Code）据此认为本轮
/// 没消耗上下文 → 内部累计不增长 → 永远到不了自动压缩阈值 → 历史无限累积 →
/// 每轮都撞上游体积上限 → 每轮又回报 0，形成自我锁死的循环。
///
/// 取 85%（与 kiro-go `cache_tracker.go` 的 `maxCacheable` 同口径）：本轮最新内容
/// 本就不可能全部命中缓存，保留 >=15% 作为真实净输入，数学上保证 `input_tokens > 0`。
const MAX_CACHEABLE_PREFIX_PCT: i32 = 85;

/// 把可缓存前缀钳到总输入的 [`MAX_CACHEABLE_PREFIX_PCT`]%，保证净输入不为 0。
///
/// 抽成纯函数便于单测（调用点还要叠加 `.min(input_tokens)`，不便直接断言边界）。
pub(crate) fn cap_cacheable_prefix(prefix_tokens: i32, total_input_tokens: i32) -> i32 {
    if prefix_tokens <= 0 || total_input_tokens <= 0 {
        return 0;
    }
    // i64 中转：超大对话下 tokens * 85 可能溢出 i32（约 2530 万 token 起）。
    let max_cacheable =
        ((total_input_tokens as i64) * (MAX_CACHEABLE_PREFIX_PCT as i64) / 100) as i32;
    prefix_tokens.min(max_cacheable).max(0)
}

/// 估算稳定前缀（系统提示 + 历史轮次）占用的 token 数。
///
/// Bedrock prefix cache 缓存的是 [system_prompt] + [messages[0..len-1]]——即当前 user
/// 消息之前的所有内容。当 `agentContinuationId` 固定（同一会话），连续请求会命中该缓存。
///
/// 返回 0 表示第一轮（无历史，缓存尚未建立）；返回正值表示可估算的 cache_read 量。
///
/// `total_input_tokens` 为本次请求的总输入估算，用于施加
/// [`MAX_CACHEABLE_PREFIX_PCT`] 上限——见该常量注释里的锁死循环说明。
pub(crate) fn count_prefix_tokens(
    system: Option<&[crate::anthropic::types::SystemMessage]>,
    messages: &[crate::anthropic::types::Message],
    total_input_tokens: i32,
) -> i32 {
    // 第一轮：没有历史前缀，prefix cache 尚未建立，保守返回 0
    if messages.len() <= 1 {
        return 0;
    }
    let history_slice = &messages[..messages.len() - 1];
    let sys_tokens = count_all_tokens_local(system, &[], None);
    let hist_tokens = count_all_tokens_local(None, history_slice, None);
    cap_cacheable_prefix((sys_tokens + hist_tokens) as i32, total_input_tokens)
}

// 注：TOKENS_PER_TOOL / count_system_message_tokens / count_tool_definition_tokens /
// count_message_content_tokens / estimate_content_block_tokens 原仅供影子缓存记账
// （cache_tracker）按块累计使用。影子缓存已整体移除，这些辅助函数一并删除。
// 请求路径的输入 token 估算走 count_all_tokens_local（字符数/4），不受影响。

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

#[cfg(test)]
mod tests {
    use super::cap_cacheable_prefix;

    /// 核心不变式：净输入（总量 - 可缓存前缀）永远 > 0。
    ///
    /// 这是自动压缩能否工作的前提：`billed_input_tokens` 从总量里扣掉
    /// cache_read，若前缀等于全量则扣成 0，客户端据此认为本轮没消耗上下文、
    /// 内部计数不增长、压缩永不触发、历史无限累积（自我锁死）。
    #[test]
    fn test_prefix_never_consumes_all_input() {
        // 整段历史都是稳定前缀的极端情形（长会话常态）：前缀 == 总量。
        let total = 200_000;
        let capped = cap_cacheable_prefix(total, total);
        assert!(
            capped < total,
            "前缀不得等于总量，否则净输入为 0：capped={capped} total={total}"
        );
        assert!(total - capped > 0, "净输入必须 > 0");
        assert_eq!(capped, 170_000, "应钳到 85%");
    }

    /// 前缀本来就低于上限时不应被改动（不影响正常短会话的缓存展示）。
    #[test]
    fn test_prefix_below_cap_unchanged() {
        assert_eq!(cap_cacheable_prefix(1_000, 100_000), 1_000);
        assert_eq!(cap_cacheable_prefix(84_999, 100_000), 84_999);
    }

    /// 边界与退化输入：不得 panic，不得返回负数。
    #[test]
    fn test_prefix_cap_edge_cases() {
        assert_eq!(cap_cacheable_prefix(0, 100), 0, "无前缀");
        assert_eq!(cap_cacheable_prefix(100, 0), 0, "总量为 0（首轮）");
        assert_eq!(cap_cacheable_prefix(-5, 100), 0, "负前缀防御");
        assert_eq!(cap_cacheable_prefix(100, -5), 0, "负总量防御");
        // 前缀远超总量（估算口径不一致时可能出现）：仍钳到 85%，不返回负数。
        assert_eq!(cap_cacheable_prefix(999_999, 1_000), 850);
    }

    /// 超大对话不得因 `tokens * 85` 溢出 i32 而算出负上限。
    #[test]
    fn test_prefix_cap_no_overflow_on_huge_input() {
        let huge = 100_000_000; // 远超 i32 溢出阈值（约 2530 万）
        let capped = cap_cacheable_prefix(huge, huge);
        assert!(capped > 0, "溢出会得到负数/0：capped={capped}");
        assert_eq!(capped, 85_000_000);
        assert!(huge - capped > 0, "净输入仍须 > 0");
    }
}
