//! OpenAI 协议类型(chat/completions)。
//!
//! 请求侧整体用 `serde_json::Value` 灵活解析(messages/tools/content 形状多变,移植参考项目的
//! 字段级改写思路),`model` / `stream` 也直接按字段读 —— **不为两个字段建反序列化结构**:
//! 那既要 `raw.clone()` 深拷贝整份请求体(带 base64 图片时峰值翻倍),又会让
//! `"stream": null`(客户端把可选字段序列化成 null 很常见)在 `bool` 反序列化下 400,
//! 而 `/v1/responses` 一直按 false 处理。可选字段一律宽松处理。
//!
//! 响应体由 `convert` 直接构造 JSON;本模块只剩响应 id 的生成。

/// 生成一个 OpenAI 风格的响应 id(chatcmpl-<hex>)。
pub fn gen_chat_completion_id() -> String {
    format!("chatcmpl-{}", crate::openai::convert::random_hex(24))
}
