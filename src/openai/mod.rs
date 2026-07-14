//! OpenAI 兼容入站层。
//!
//! 让 OpenAI 协议的客户端(Codex / Cline / Roo / OpenAI SDK 等)通过 KiroStudio 使用上游模型。
//! 端点:`/v1/chat/completions`(第一批)。`/v1/responses`(第二批,待补)。
//!
//! # 架构(薄适配层,不重复管线)
//! OpenAI 请求 → [`convert::openai_chat_to_anthropic`] 翻译成内部 Anthropic MessagesRequest JSON
//! → **直接调用现有 `anthropic::handlers::post_messages`**(复用整条管线:custom_api 透传 / failover /
//! 工具修复 / 泄漏清洗 / 用量埋点)→ 拿回 Anthropic SSE(流式)或 Messages JSON(非流式)
//! → [`convert`] 的响应翻译器翻回 OpenAI chat.completion(.chunk) 格式。
//!
//! 翻译层只处理 JSON 字节,不碰网络/号池,与调度彻底解耦。转换规则移植自参考项目
//! sub2api / CLIProxyAPI(见 memory openai-inbound-endpoints-sub2api-cliproxy)。

pub mod convert;
pub mod handlers;
pub mod types;
