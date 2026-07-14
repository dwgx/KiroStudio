//! OpenAI 协议类型(chat/completions)。
//!
//! 请求侧用 `serde_json::Value` 灵活解析(messages/tools/content 形状多变,移植参考项目的
//! 字段级改写思路);响应侧构造用轻量结构。只定义稳定核心,可选字段宽松处理。

use serde::Deserialize;

/// OpenAI chat/completions 请求(仅取我们需要的字段;其余靠 raw Value 读)。
/// 完整请求体以 `serde_json::Value` 形式在 convert 里按字段读,本结构只用于快速取 model/stream。
#[derive(Debug, Deserialize)]
pub struct ChatCompletionsPeek {
    pub model: String,
    #[serde(default)]
    pub stream: bool,
}

/// 生成一个 OpenAI 风格的响应 id(chatcmpl-<hex>)。
pub fn gen_chat_completion_id() -> String {
    format!("chatcmpl-{}", crate::openai::convert::random_hex(24))
}
