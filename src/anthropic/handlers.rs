//! Anthropic API Handler 函数

use std::convert::Infallible;

use anyhow::Error;
use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::token;
use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use serde_json::json;
use std::time::Duration;
use tokio::time::interval;
use uuid::Uuid;

use super::converter::{ConversionError, convert_request};
use super::middleware::AppState;
use super::stream::{BufferedStreamContext, CompletionStatus, SseEvent, StreamContext};
use super::types::{CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse, OutputConfig, Thinking};
use super::websearch;

/// 从入站请求头提取客户端 IP（仅头部来源，不含连接层回退）。
///
/// 优先级：`x-forwarded-for` 首段（逗号分割，取最靠近客户端的第一跳）→
/// `x-real-ip` → 都没有则 `None`。反代场景下这两个头即客户端真实 IP；直连
/// 无反代时头缺失，由 [`ClientInfo::from_headers_with_peer`] 回退到 TCP 对端地址。
fn extract_client_ip(headers: &axum::http::HeaderMap) -> Option<String> {
    // x-forwarded-for: "client, proxy1, proxy2" —— 取第一段
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = xff.split(',').next() {
            let ip = first.trim();
            if !ip.is_empty() {
                return Some(ip.to_string());
            }
        }
    }
    // x-real-ip: 单个 IP
    if let Some(real) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        let ip = real.trim();
        if !ip.is_empty() {
            return Some(ip.to_string());
        }
    }
    None
}

/// 指纹采集开关的运行时镜像（`config.collect_client_fingerprint`）。
///
/// 热路径 [`ClientInfo::from_headers_with_peer`] 拿不到 config，故用一个进程级
/// AtomicBool 镜像：main 启动时按配置写入，admin 改开关时立即改写，无需重启。
/// 默认 true（与配置默认一致）。
static COLLECT_CLIENT_FINGERPRINT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// 设置指纹采集开关（供 main 启动接线 / admin 更新配置时立即生效调用）。
pub fn set_collect_client_fingerprint(enabled: bool) {
    COLLECT_CLIENT_FINGERPRINT.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

fn collect_client_fingerprint() -> bool {
    COLLECT_CLIENT_FINGERPRINT.load(std::sync::atomic::Ordering::Relaxed)
}

/// —— TIER3 配置热重载：AppState 曾固化的热路径开关改用进程级原子镜像 ——
///
/// `AppState` 是 `#[derive(Clone)]`、建路由时按值烘焙，一旦服务栈建成便不可变。
/// 沿用 [`COLLECT_CLIENT_FINGERPRINT`] 已验证的范式，把 admin 可热改的开关搬到
/// 进程级 static 原子镜像：main 启动写入、admin 改配置立即改写、handler 热路径读镜像，
/// 全程无需重启、无锁近零成本。initial 默认与 config 默认一致。
static EXTRACT_THINKING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 设置非流式 thinking 提取开关（main 启动接线 / admin 热更调用，立即生效）。
pub fn set_extract_thinking(enabled: bool) {
    EXTRACT_THINKING.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

fn extract_thinking_enabled() -> bool {
    EXTRACT_THINKING.load(std::sync::atomic::Ordering::Relaxed)
}

/// Claude Code 自动切缓冲协议开关（进程级镜像，admin 热更即时生效）。默认 true。
///
/// 开启时：`/v1/messages` 若识别到请求来自 Claude Code，流式响应自动改走 buffered 分发
/// （与 `/cc/v1` 同款），使 message_start 的 input_tokens 用上游 contextUsageEvent 的准确值——
/// CC 会校验该字段。这样 CC 直接打 `/v1` 也能拿到正确行为，无需用户手动改用 `/cc/v1` 端点。
static CC_AUTO_BUFFER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// 设置 CC 自动切缓冲开关（main 启动接线 / admin 热更调用，立即生效）。
pub fn set_cc_auto_buffer(enabled: bool) {
    CC_AUTO_BUFFER.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

fn cc_auto_buffer_enabled() -> bool {
    CC_AUTO_BUFFER.load(std::sync::atomic::Ordering::Relaxed)
}

/// 从入站请求头识别请求是否来自 Claude Code。
///
/// 两个信号（任一命中即判为 CC）：
/// - `x-anthropic-billing-header`：CC 专属归因头（converter.rs 已处理该前缀），最强信号。
/// - User-Agent 经 `usage::classify_device` 判为 `claude-code` 类（唯一真源，避免此处重复
///   维护 UA 关键字列表导致与设备分类逻辑静默漂移）。
fn is_claude_code_request(headers: &axum::http::HeaderMap) -> bool {
    if headers.contains_key("x-anthropic-billing-header") {
        return true;
    }
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
    crate::usage::classify_device(ua).as_deref() == Some("claude-code")
}

/// 输入压缩配置的进程级镜像（TIER3 热更）。
///
/// `CompressionConfig` 非标量（阈值 + 开关），用 `ArcSwap` 承载：admin 改配置时整份原子换、
/// handler 热路径 `load_full()` 拿 `Arc` 快照（无锁近零成本）。`OnceLock` 惰性初始化，
/// main 启动即 `set_compression` 写入真配置；未初始化时回退默认（与 config 默认一致）。
static COMPRESSION: std::sync::OnceLock<arc_swap::ArcSwap<crate::model::config::CompressionConfig>> =
    std::sync::OnceLock::new();

fn compression_cell() -> &'static arc_swap::ArcSwap<crate::model::config::CompressionConfig> {
    COMPRESSION.get_or_init(|| {
        arc_swap::ArcSwap::from_pointee(crate::model::config::CompressionConfig::default())
    })
}

/// 设置输入压缩配置（main 启动接线 / admin 热更调用，立即生效，下个请求即读到新值）。
pub fn set_compression(compression: crate::model::config::CompressionConfig) {
    compression_cell().store(std::sync::Arc::new(compression));
}

fn current_compression() -> std::sync::Arc<crate::model::config::CompressionConfig> {
    compression_cell().load_full()
}

/// 请求来源的客户端画像（设备类型 + IP + 细分 OS + 浏览器），
/// 一并沿用量埋点路径传递，避免多参数散落。
#[derive(Clone, Default)]
struct ClientInfo {
    device: Option<String>,
    ip: Option<String>,
    os: Option<String>,
    browser: Option<String>,
}

impl ClientInfo {
    /// 从入站请求头 + TCP 对端地址一次性解析设备/IP/OS/浏览器。
    ///
    /// IP 取值优先级：`x-forwarded-for` / `x-real-ip`（反代场景）→ TCP 连接对端
    /// 地址（直连 8990 无反代头时的回退）。peer 取自 axum 的 `ConnectInfo<SocketAddr>`，
    /// 仅取 IP 部分（丢弃端口），拿不到则为 None。
    ///
    /// 隐私开关：`collect_client_fingerprint` 关闭时直接返回全空画像，
    /// 热路径不解析任何指纹字段，用量记录不落这些信息。
    fn from_headers_with_peer(
        headers: &axum::http::HeaderMap,
        peer: Option<std::net::SocketAddr>,
    ) -> Self {
        if !collect_client_fingerprint() {
            return Self::default();
        }
        let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
        let ip = extract_client_ip(headers).or_else(|| peer.map(|a| a.ip().to_string()));
        Self {
            device: crate::usage::classify_device(ua),
            ip,
            os: crate::usage::parse_client_os(ua),
            browser: crate::usage::parse_client_browser(ua),
        }
    }

    /// 把画像字段写入一条用量记录
    fn apply(&self, record: &mut crate::usage::RequestRecord) {
        record.client_device = self.device.clone();
        record.client_ip = self.ip.clone();
        record.client_os = self.os.clone();
        record.client_browser = self.browser.clone();
    }
}

/// prompt 缓存记账所需的上下文（跟踪器 + 本次请求的缓存画像）
///
/// 构建发往上游的 Kiro 请求体（含输入压缩）。
///
/// 流程：先序列化测量大小；仅当启用压缩且体积超过 `trigger_bytes` 时，对
/// `ConversationState` 跑压缩管道（空白折叠 + tool_result 智能截断）再重新序列化。
///
/// 保守设计：默认阈值高（4MiB），正常小请求零处理；压缩后仍可能超上游硬限制，
/// 那种情况不再本地判死，交由上游返回 400，再由 [`map_provider_error`] 透传给客户端。
fn build_kiro_request_body(
    conversation_state: crate::kiro::model::requests::conversation::ConversationState,
    compression: &crate::model::config::CompressionConfig,
) -> Result<String, serde_json::Error> {
    let mut kiro_request = KiroRequest {
        conversation_state,
        profile_arn: None,
    };

    let body = serde_json::to_string(&kiro_request)?;

    if compression.enabled && body.len() > compression.trigger_bytes {
        let before = body.len();
        let stats = super::compressor::compress(&mut kiro_request.conversation_state, compression);
        let compressed = serde_json::to_string(&kiro_request)?;
        tracing::info!(
            before_bytes = before,
            after_bytes = compressed.len(),
            saved_bytes = stats.total_saved(),
            trigger_bytes = compression.trigger_bytes,
            "请求体超过压缩阈值，已执行输入压缩"
        );
        return Ok(compressed);
    }

    Ok(body)
}

/// 将 KiroProvider 错误映射为 HTTP 响应
fn map_provider_error(err: Error) -> Response {
    let err_str = err.to_string();

    // 全池冷却快速失败：token_manager 全池都在冷却时会带 retry_after_secs=N 快速 bail。
    // 这里透传成标准 429 + Retry-After 头，让客户端(Claude Code)按其自身退避策略重试——
    // 比网关内硬扛温和，也减少对被风控号的试探。
    if let Some(secs) = err_str
        .split("retry_after_secs=")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|d| d.parse::<u64>().ok())
    {
        let retry_after = secs.clamp(1, 300);
        tracing::warn!(retry_after_secs = retry_after, "全池冷却，返回 429 + Retry-After 让客户端退避");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, retry_after.to_string())],
            Json(ErrorResponse::new(
                "rate_limit_error",
                "All credentials are temporarily cooling down. Please retry after the indicated delay.",
            )),
        )
            .into_response();
    }

    // 上下文窗口满了（对话历史累积超出模型上下文窗口限制）
    if err_str.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        tracing::warn!(error = %err, "上游拒绝请求：上下文窗口已满（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Context window is full. Reduce conversation history, system prompt, or tools.",
            )),
        )
            .into_response();
    }

    // 单次输入太长（请求体本身超出上游限制）
    if err_str.contains("Input is too long") {
        tracing::warn!(error = %err, "上游拒绝请求：输入过长（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Input is too long. Reduce the size of your messages.",
            )),
        )
            .into_response();
    }
    tracing::error!("Kiro API 调用失败: {}", err);
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse::new(
            "api_error",
            format!("上游 API 调用失败: {}", err),
        )),
    )
        .into_response()
}

/// GET /v1/models
///
/// 返回可用的模型列表
pub async fn get_models() -> impl IntoResponse {
    tracing::info!("Received GET /v1/models request");

    let models = vec![
        Model {
            id: "claude-opus-4-8".to_string(),
            object: "model".to_string(),
            created: 1779897600, // May 28, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.8".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128_000,
        },
        Model {
            id: "claude-opus-4-8-thinking".to_string(),
            object: "model".to_string(),
            created: 1779897600, // May 28, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.8 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128_000,
        },
        Model {
            id: "claude-opus-4-7".to_string(),
            object: "model".to_string(),
            created: 1776276000, // Apr 16, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-7-thinking".to_string(),
            object: "model".to_string(),
            created: 1776276000, // Apr 16, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-6".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-6".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-5-20251101".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-5-20251101-thinking".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-5-20250929".to_string(),
            object: "model".to_string(),
            created: 1759104000, // Sep 29, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-5-20250929-thinking".to_string(),
            object: "model".to_string(),
            created: 1759104000, // Sep 29, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-haiku-4-5-20251001".to_string(),
            object: "model".to_string(),
            created: 1760486400, // Oct 15, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-haiku-4-5-20251001-thinking".to_string(),
            object: "model".to_string(),
            created: 1760486400, // Oct 15, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        // 国产模型：Kiro 上游直收原生 modelId（倍率远低于 claude，见 kiro-model-catalog）。
        // id 直接用 Kiro 规范 modelId，客户端选它即原样透传上游。窗口 200k。
        Model {
            id: "deepseek-3.2".to_string(),
            object: "model".to_string(),
            created: 1759104000,
            owned_by: "deepseek".to_string(),
            display_name: "DeepSeek V3.2".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "glm-5".to_string(),
            object: "model".to_string(),
            created: 1759104000,
            owned_by: "zhipu".to_string(),
            display_name: "GLM-5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "qwen3-coder-next".to_string(),
            object: "model".to_string(),
            created: 1759104000,
            owned_by: "qwen".to_string(),
            display_name: "Qwen3 Coder Next".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "minimax-m2.5".to_string(),
            object: "model".to_string(),
            created: 1759104000,
            owned_by: "minimax".to_string(),
            display_name: "MiniMax M2.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "minimax-m2.1".to_string(),
            object: "model".to_string(),
            created: 1759104000,
            owned_by: "minimax".to_string(),
            display_name: "MiniMax M2.1".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
    ];

    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
}

/// POST /v1/messages
///
/// 创建消息（对话）
pub async fn post_messages(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    // 取**裸 body 字节**(而非 JsonExtractor):自定义 API 代挂需要原样透传原始请求体。
    // Kiro 路径行为不变——下面立即从同一份字节解析出 MessagesRequest,与旧 JsonExtractor 等价。
    raw_body: Bytes,
) -> Response {
    // 先按原逻辑解析请求体(解析失败=400,与旧 JsonExtractor 的行为对齐)。
    let mut payload: MessagesRequest = match serde_json::from_slice(&raw_body) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new("invalid_request_error", format!("请求体解析失败: {e}"))),
            )
                .into_response();
        }
    };
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages request"
    );

    // 从入站请求头 + TCP 对端地址识别来源画像（设备/IP/OS/浏览器，用于「最近请求」展示）
    let client = ClientInfo::from_headers_with_peer(&headers, Some(peer));
    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 混入池分流:选一次号,若命中自定义 API 凭据 → 原样透传原始请求体到其上游、直接返回。
    // 选到 Kiro 号(或池中无自定义号)→ 返回 None,继续走下方原 Kiro 路径(行为完全不变)。
    let user_id = payload.metadata.as_ref().and_then(|m| m.user_id.clone());
    if let Some(resp) = provider
        .try_custom_api_passthrough(raw_body.clone(), Some(&payload.model), user_id.as_deref())
        .await
    {
        return resp;
    }

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    // 检查是否应本地处理 WebSearch 请求（tool_choice 强制 / 纯 web_search 单工具 / Claude Code 前缀）
    if websearch::should_handle_websearch_request(&payload) {
        tracing::info!("检测到 WebSearch 请求，路由到本地 WebSearch 处理");

        // 估算输入 tokens（只读计数，传引用避免深拷贝整个对话历史）
        let input_tokens = token::count_all_tokens(
            &payload.model,
            payload.system.as_deref(),
            &payload.messages,
            payload.tools.as_deref(),
        ) as i32;

        return websearch::handle_websearch_request(provider, &payload, input_tokens).await;
    }

    // 混合工具场景：请求带 web_search 但未显式触发搜索，剔除 web_search 后走常规转发，
    // 避免把 web_search 原样下发给 Kiro 触发 400 Improperly formed request。
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到混合工具列表中的 web_search，剔除后转发上游");
        websearch::strip_web_search_tools(&mut payload);
    }

    // 转换请求
    let conversion_result = match convert_request(&payload) {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => {
                    ("invalid_request_error", format!("模型不支持: {}", model))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
            };
            tracing::warn!("请求转换失败: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // 构建 Kiro 请求体（发上游前，超阈值时执行输入压缩；profile_arn 由 provider 层注入）
    let request_body = match build_kiro_request_body(
        conversion_result.conversation_state,
        &current_compression(),
    ) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    // 估算输入 tokens（只读计数，传引用避免深拷贝整个对话历史）
    let input_tokens = token::count_all_tokens(
        &payload.model,
        payload.system.as_deref(),
        &payload.messages,
        payload.tools.as_deref(),
    ) as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;

    if payload.stream {
        // 流式响应。CC 自动切协议：识别到 Claude Code 且开关开启时，改走 buffered 分发
        // （等价 /cc/v1），让 message_start 的 input_tokens 用上游准确值——CC 会校验它。
        // 这样 CC 直接打 /v1 也能拿到正确行为，无需手动改用 /cc/v1 端点。
        if cc_auto_buffer_enabled() && is_claude_code_request(&headers) {
            tracing::debug!("识别到 Claude Code 请求，/v1 流式自动切换为 buffered 分发（准确 input_tokens）");
            handle_stream_request_buffered(
                provider,
                &request_body,
                &payload.model,
                input_tokens,
                thinking_enabled,
                tool_name_map,
                client,
            )
            .await
        } else {
            handle_stream_request(
                provider,
                &request_body,
                &payload.model,
                input_tokens,
                thinking_enabled,
                tool_name_map,
                client,
            )
            .await
        }
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = extract_thinking_enabled() && thinking_enabled;
        handle_non_stream_request(provider, &request_body, &payload.model, input_tokens, extract_thinking, tool_name_map, client).await
    }
}

/// 处理流式请求
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    client: ClientInfo,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let (response, meta) = match provider.call_api_stream(request_body).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 创建流处理上下文
    let mut ctx = StreamContext::new_with_thinking(model, input_tokens, thinking_enabled, tool_name_map);

    // 生成初始事件
    let initial_events = ctx.generate_initial_events();

    // 创建 SSE 流（流结束时用 meta + 最终 usage 埋点一条成功记录）
    let stream = create_sse_stream(provider, response, ctx, initial_events, meta, client);

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 流结束时，用 provider 元数据 + StreamContext 最终 usage 埋点一条成功记录
fn emit_stream_usage(
    provider: &crate::kiro::provider::KiroProvider,
    ctx: &StreamContext,
    meta: &crate::kiro::provider::CallMeta,
    client: &ClientInfo,
) {
    let usage = ctx.resolved_usage();
    let mut record = crate::usage::RequestRecord::new(
        Uuid::new_v4().to_string(),
        meta.model.clone().unwrap_or_else(|| ctx.model.clone()),
    );
    record.credential_id = Some(meta.credential_id);
    record.session_id = meta.session_id.clone();
    record.is_streaming = meta.is_streaming;
    record.input_tokens = usage.input_tokens;
    record.output_tokens = usage.output_tokens;
    record.cache_read_tokens = usage.cache_read_tokens;
    record.cache_creation_tokens = usage.cache_creation_tokens;
    record.credits_used = usage.credits_used;
    record.latency_ms = meta.latency_ms;
    record.retries = meta.retries;
    // 去硬编码 Success：按本次响应的真实完成状态记账，避免截断/上游错误被记成成功污染熔断信号。
    record.outcome = ctx.completion_outcome();
    // 生命周期累计花费：把本次真实 credit 消耗累加到该凭据（独立于用量保留期，只增不清）。
    if let Some(c) = record.credits_used {
        provider.report_credits(meta.credential_id, c);
    }
    client.apply(&mut record);
    crate::usage::emit_record(record);
}

/// Ping 事件间隔（25秒）
const PING_INTERVAL_SECS: u64 = 25;

/// 创建 ping 事件的 SSE 字符串
fn create_ping_sse() -> Bytes {
    Bytes::from("event: ping\ndata: {\"type\": \"ping\"}\n\n")
}

/// 创建 SSE 事件流
fn create_sse_stream(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    response: reqwest::Response,
    ctx: StreamContext,
    initial_events: Vec<SseEvent>,
    meta: crate::kiro::provider::CallMeta,
    client: ClientInfo,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    // 先发送初始事件
    let initial_stream = stream::iter(
        initial_events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string()))),
    );

    // 然后处理 Kiro 响应流，同时每25秒发送 ping 保活
    let body_stream = response.bytes_stream();

    let processing_stream = stream::unfold(
        (body_stream, ctx, EventStreamDecoder::new(), false, interval(Duration::from_secs(PING_INTERVAL_SECS)), meta, client, provider),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, meta, client, provider)| async move {
            if finished {
                return None;
            }

            // 使用 select! 同时等待数据和 ping 定时器
            tokio::select! {
                // 处理数据流
                chunk_result = body_stream.next() => {
                    match chunk_result {
                        Some(Ok(chunk)) => {
                            // 解码事件
                            if let Err(e) = decoder.feed(&chunk) {
                                tracing::warn!("缓冲区溢出: {}", e);
                            }

                            let mut events = Vec::new();
                            let mut last_decode_err: Option<String> = None;
                            for result in decoder.decode_iter() {
                                match result {
                                    Ok(frame) => {
                                        // from_frame 按值吞 frame，事件类型须在 move 前先拥有化捕获。
                                        let et = frame.event_type().map(|s| s.to_string());
                                        match Event::from_frame(frame) {
                                            Ok(event) => {
                                                // process_kiro_event 内部对 in-band Event::Error/Exception
                                                // 会置 completion 失败态并内联返回 SSE error 事件。
                                                let sse_events = ctx.process_kiro_event(&event);
                                                events.extend(sse_events);
                                            }
                                            Err(err) => {
                                                // 帧层解码成功、Frame→Event 反序列化失败：
                                                // toolUseEvent 失败意味着工具调用不可恢复丢失，置 DecoderStopped
                                                // 失败态（收尾靠 None 分支补发 SSE error），避免截断被当成功不重试；
                                                // 非 tool 帧解析失败历史上就允许被忽略，仅告警不置失败态，防误伤正常流。
                                                if et.as_deref() == Some("toolUseEvent") {
                                                    tracing::warn!("toolUseEvent 帧解析失败,按响应截断处理: {}", err);
                                                    ctx.mark_decoder_stopped(format!("toolUseEvent 帧解析失败: {}", err));
                                                } else {
                                                    tracing::warn!("事件帧解析失败(event_type={:?}),已忽略: {}", et.as_deref(), err);
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        last_decode_err = Some(e.to_string());
                                        tracing::warn!("解码事件失败: {}", e);
                                    }
                                }
                            }

                            // 解码器连续错误超限而永久停止：响应必然截断，置失败态供收尾记账，
                            // 并内联补发一个 SSE error 事件（若尚未发过），避免截断被当成功。
                            if decoder.is_stopped() {
                                ctx.mark_decoder_stopped(
                                    last_decode_err.unwrap_or_else(|| "解码器连续错误已停止".to_string()),
                                );
                                if !ctx.error_event_emitted() {
                                    events.push(SseEvent::error_event(
                                        ctx.completion().sse_error_type(),
                                        ctx.completion().client_message(),
                                    ));
                                    ctx.mark_error_event_emitted();
                                }
                            }

                            // 转换为 SSE 字节流
                            let bytes: Vec<Result<Bytes, Infallible>> = events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();

                            Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, meta, client, provider)))
                        }
                        Some(Err(e)) => {
                            tracing::error!("读取响应流失败: {}", e);
                            // 上游流中途失败：置传输失败态（供收尾按 NetworkError 记账），
                            // 先发一个 SSE error 事件显式告知客户端"本次未正常完成"，再补最终事件收尾。
                            // 否则 Claude Code 会把截断输出当作正常 message_stop=成功，不重试。
                            // 幂等：若 in-band 错误已置过失败态，mark_transport_error 会保留首因。
                            ctx.mark_transport_error(e.to_string());
                            let mut events = Vec::new();
                            if !ctx.error_event_emitted() {
                                events.push(SseEvent::error_event(
                                    ctx.completion().sse_error_type(),
                                    ctx.completion().client_message(),
                                ));
                                ctx.mark_error_event_emitted();
                            }
                            events.extend(ctx.generate_final_events());
                            let bytes: Vec<Result<Bytes, Infallible>> = events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            emit_stream_usage(&provider, &ctx, &meta, &client);
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, meta, client, provider)))
                        }
                        None => {
                            // 流结束，发送最终事件。
                            // 兜底：若本次完成状态为失败（in-band 错误/解码器停止）但尚未发过 error 事件，
                            // 在收尾处补发，确保客户端不把截断输出当成功。
                            let mut final_events = Vec::new();
                            if !ctx.completion().is_ok() && !ctx.error_event_emitted() {
                                final_events.push(SseEvent::error_event(
                                    ctx.completion().sse_error_type(),
                                    ctx.completion().client_message(),
                                ));
                                ctx.mark_error_event_emitted();
                            }
                            final_events.extend(ctx.generate_final_events());
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            emit_stream_usage(&provider, &ctx, &meta, &client);
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, meta, client, provider)))
                        }
                    }
                }
                // 发送 ping 保活
                _ = ping_interval.tick() => {
                    tracing::trace!("发送 ping 保活事件");
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, meta, client, provider)))
                }
            }
        },
    )
    .flatten();

    initial_stream.chain(processing_stream)
}

use super::converter::get_context_window_size;

/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    client: ClientInfo,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let (response, meta) = match provider.call_api(request_body).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 读取响应体
    let body_bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!("读取响应体失败: {}", e);
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    format!("读取响应失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    // 解析事件流
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }

    let mut text_content = String::new();
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut has_tool_use = false;
    let mut stop_reason = "end_turn".to_string();
    // 从 contextUsageEvent 计算的实际输入 tokens
    let mut context_input_tokens: Option<i32> = None;
    // 从 meteringEvent 解析的真实 credit 消耗量
    let mut credits_used: Option<f64> = None;
    // 本次响应的完成状态：默认 Ok，遇 in-band 错误/异常/解码器停止置失败态。
    // 收尾据此决定 HTTP 码与用量记账 outcome，避免截断输出被当成 200 成功。
    let mut completion = CompletionStatus::Ok;

    // 收集工具调用的增量 JSON
    let mut tool_json_buffers: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    let mut last_decode_err: Option<String> = None;
    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => {
                // from_frame 按值吞 frame，事件类型须在 move 前先拥有化捕获。
                let et = frame.event_type().map(|s| s.to_string());
                match Event::from_frame(frame) {
                    Ok(event) => match event {
                        Event::AssistantResponse(resp) => {
                            text_content.push_str(&resp.content);
                        }
                        Event::ToolUse(tool_use) => {
                            has_tool_use = true;

                            // 累积工具的 JSON 输入（自适应累积快照 vs 纯增量，与流式路径同源修复）：
                            // Kiro 同一 tool_use_id 的 input 可能是"到目前为止的完整 JSON"（累积）
                            // 而非片段。若原样 push_str，累积模式会把 JSON 重复拼接 → 解析失败。
                            let buffer = tool_json_buffers
                                .entry(tool_use.tool_use_id.clone())
                                .or_insert_with(String::new);
                            // 与流式路径同源判据：累积（本帧含 buffer 为前缀且更长）→ 全量替换；
                            // 完全重复 → 不变；纯增量/前缀不成立 → 追加。
                            if tool_use.input.len() >= buffer.len()
                                && tool_use.input.starts_with(buffer.as_str())
                            {
                                *buffer = tool_use.input.clone();
                            } else if buffer.as_str() != tool_use.input {
                                buffer.push_str(&tool_use.input);
                            }

                            // 如果是完整的工具调用，添加到列表
                            if tool_use.stop {
                                let input: serde_json::Value = if buffer.is_empty() {
                                    serde_json::json!({})
                                } else {
                                    match serde_json::from_str(buffer) {
                                        Ok(v) => v,
                                        Err(e) => {
                                            // 拼装后仍非法：置失败态，收尾(下方 `if !completion.is_ok()`)
                                            // 返回非 200，绝不静默吞成空参数——空参会让客户端把失败的
                                            // 工具调用当成"无参数成功调用"执行，比报错更危险。
                                            tracing::warn!(
                                                "工具输入 JSON 解析失败: {}, tool_use_id: {}（返回错误,不静默空参）",
                                                e, tool_use.tool_use_id
                                            );
                                            if completion.is_ok() {
                                                completion = CompletionStatus::UpstreamError {
                                                    code: "INVALID_TOOL_INPUT".to_string(),
                                                    message: format!(
                                                        "工具参数 JSON 非法（tool_use_id={}）: {}",
                                                        tool_use.tool_use_id, e
                                                    ),
                                                };
                                            }
                                            serde_json::json!({})
                                        }
                                    }
                                };

                                let original_name = tool_name_map
                                    .get(&tool_use.name)
                                    .cloned()
                                    .unwrap_or_else(|| tool_use.name.clone());

                                tool_uses.push(json!({
                                    "type": "tool_use",
                                    "id": tool_use.tool_use_id,
                                    "name": original_name,
                                    "input": input
                                }));
                            }
                        }
                        Event::ContextUsage(context_usage) => {
                            // 从上下文使用百分比计算实际的 input_tokens
                            let window_size = get_context_window_size(model);
                            let actual_input_tokens = (context_usage.context_usage_percentage
                                * (window_size as f64)
                                / 100.0)
                                as i32;
                            context_input_tokens = Some(actual_input_tokens);
                            // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                            if context_usage.context_usage_percentage >= 100.0 {
                                stop_reason = "model_context_window_exceeded".to_string();
                            }
                            tracing::debug!(
                                "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                                context_usage.context_usage_percentage,
                                actual_input_tokens
                            );
                        }
                        Event::Metering(metering) => {
                            credits_used = Some(credits_used.unwrap_or(0.0) + metering.usage);
                        }
                        Event::Exception { exception_type, message } => {
                            // 铁律：ContentLengthExceededException = max_tokens 干净收尾，绝不算失败。
                            if exception_type == "ContentLengthExceededException" {
                                stop_reason = "max_tokens".to_string();
                            } else if completion.is_ok() {
                                // 其它异常是上游真实失败，置失败态（保留首因）。
                                tracing::error!("非流式收到 in-band 异常: {} - {}", exception_type, message);
                                completion = CompletionStatus::UpstreamError {
                                    code: exception_type,
                                    message,
                                };
                            }
                        }
                        Event::Error { error_code, error_message } => {
                            // in-band 错误事件：落入历史的 `_ => {}` 会被静默忽略、照样返回 200，
                            // 这里显式置失败态，收尾时返回非 200 并按真实 outcome 记账。
                            if completion.is_ok() {
                                tracing::error!("非流式收到 in-band 错误: {} - {}", error_code, error_message);
                                completion = CompletionStatus::UpstreamError {
                                    code: error_code,
                                    message: error_message,
                                };
                            }
                        }
                        _ => {}
                    },
                    Err(err) => {
                        // 帧层解码成功、Frame→Event 反序列化失败：
                        // toolUseEvent 失败=工具调用不可恢复丢失，置 DecoderStopped 失败态
                        // （收尾靠下方 `if !completion.is_ok()` 返回 502+记账），避免截断被当成功。
                        // 非 tool 帧解析失败历史上就允许被忽略，仅告警不置失败态，防误伤正常流。
                        if et.as_deref() == Some("toolUseEvent") {
                            tracing::warn!("非流式 toolUseEvent 帧解析失败,按响应截断处理: {}", err);
                            if completion.is_ok() {
                                completion = CompletionStatus::DecoderStopped {
                                    message: format!("toolUseEvent 帧解析失败: {}", err),
                                };
                            }
                        } else {
                            tracing::warn!("非流式事件帧解析失败(event_type={:?}),已忽略: {}", et.as_deref(), err);
                        }
                    }
                }
            }
            Err(e) => {
                last_decode_err = Some(e.to_string());
                tracing::warn!("解码事件失败: {}", e);
            }
        }
    }

    // 解码器永久停止：单 feed 中途连续错误超限，后续帧必然丢失、响应截断。
    if decoder.is_stopped() && completion.is_ok() {
        completion = CompletionStatus::DecoderStopped {
            message: last_decode_err.unwrap_or_else(|| "解码器连续错误已停止".to_string()),
        };
    }

    // 完成状态为失败：直接返回非 200 错误响应 + 埋点真实 outcome，绝不把截断输出当 200 成功。
    // （ContentLengthExceededException 走的是 max_tokens，completion 仍为 Ok，不进此分支。）
    if !completion.is_ok() {
        {
            let mut record = crate::usage::RequestRecord::new(
                Uuid::new_v4().to_string(),
                meta.model.clone().unwrap_or_else(|| model.to_string()),
            );
            record.credential_id = Some(meta.credential_id);
            record.session_id = meta.session_id.clone();
            record.is_streaming = meta.is_streaming;
            record.input_tokens = context_input_tokens.unwrap_or(input_tokens);
            record.credits_used = credits_used;
            record.latency_ms = meta.latency_ms;
            record.retries = meta.retries;
            record.outcome = completion.outcome();
            // 生命周期累计花费：本次真实 credit 消耗累加到该凭据（独立于用量保留期，只增不清）。
            if let Some(c) = record.credits_used {
                provider.report_credits(meta.credential_id, c);
            }
            client.apply(&mut record);
            crate::usage::emit_record(record);
        }
        let status = StatusCode::from_u16(completion.http_status_u16())
            .unwrap_or(StatusCode::BAD_GATEWAY);
        let sse_error_type = completion.sse_error_type();
        return (
            status,
            Json(ErrorResponse::new(sse_error_type, completion.client_message())),
        )
            .into_response();
    }

    // 确定 stop_reason
    if has_tool_use && stop_reason == "end_turn" {
        stop_reason = "tool_use".to_string();
    }

    // 构建响应内容
    let mut content: Vec<serde_json::Value> = Vec::new();

    if thinking_enabled {
        // 从完整文本中提取 thinking 块
        let (thinking, remaining_text) =
            super::stream::extract_thinking_from_complete_text(&text_content);

        if let Some(thinking_text) = thinking {
            // 补 signature 占位符：客户端 thinking 模式下本地校验 thinking 块必须带非空
            // signature，非流式组装时同样需要（回传时 converter 只读 thinking，占位符被
            // serde 静默丢弃，不会转发给 Kiro）。详见 stream::THINKING_SIGNATURE_PLACEHOLDER。
            content.push(json!({
                "type": "thinking",
                "thinking": thinking_text,
                "signature": super::stream::THINKING_SIGNATURE_PLACEHOLDER
            }));
        }

        if !remaining_text.is_empty() {
            content.push(json!({
                "type": "text",
                "text": remaining_text
            }));
        }
    } else if !text_content.is_empty() {
        content.push(json!({
            "type": "text",
            "text": text_content
        }));
    }

    content.extend(tool_uses);

    // 估算输出 tokens
    let output_tokens = token::estimate_output_tokens(&content);

    // 使用从 contextUsageEvent 计算的 input_tokens，如果没有则使用估算值
    let final_input_tokens = context_input_tokens.unwrap_or(input_tokens);

    // 用量埋点：非流式成功记录
    {
        let mut record = crate::usage::RequestRecord::new(
            Uuid::new_v4().to_string(),
            meta.model.clone().unwrap_or_else(|| model.to_string()),
        );
        record.credential_id = Some(meta.credential_id);
        record.session_id = meta.session_id.clone();
        record.is_streaming = meta.is_streaming;
        record.input_tokens = final_input_tokens;
        record.output_tokens = output_tokens;
        record.credits_used = credits_used;
        record.latency_ms = meta.latency_ms;
        record.retries = meta.retries;
        // 去硬编码：此处 completion 必为 Ok（失败已在上方 early-return），显式读取以统一口径。
        record.outcome = completion.outcome();
        // 生命周期累计花费：本次真实 credit 消耗累加到该凭据（独立于用量保留期，只增不清）。
        if let Some(c) = record.credits_used {
            provider.report_credits(meta.credential_id, c);
        }
        client.apply(&mut record);
        crate::usage::emit_record(record);
    }

    // 构建 usage（影子缓存记账已移除，不再注入 cache_read/cache_creation 字段）
    let usage = json!({
        "input_tokens": final_input_tokens,
        "output_tokens": output_tokens
    });

    // 构建 Anthropic 响应
    let response_body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage
    });

    (StatusCode::OK, Json(response_body)).into_response()
}

/// 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
///
/// - Opus 4.6：覆写为 adaptive 类型
/// - 其他模型：覆写为 enabled 类型
/// - budget_tokens 固定为 20000
fn override_thinking_from_model_name(payload: &mut MessagesRequest) {
    let model_lower = payload.model.to_lowercase();
    if !model_lower.contains("thinking") {
        return;
    }

    let is_opus_4_6 =
        model_lower.contains("opus") && (model_lower.contains("4-6") || model_lower.contains("4.6"));

    let thinking_type = if is_opus_4_6 {
        "adaptive"
    } else {
        "enabled"
    };

    tracing::info!(
        model = %payload.model,
        thinking_type = thinking_type,
        "模型名包含 thinking 后缀，覆写 thinking 配置"
    );

    payload.thinking = Some(Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens: 20000,
    });
    
    if is_opus_4_6 {
        payload.output_config = Some(OutputConfig {
            effort: "high".to_string(),
        });
    }
}

/// POST /v1/messages/count_tokens
///
/// 计算消息的 token 数量
pub async fn count_tokens(
    JsonExtractor(payload): JsonExtractor<CountTokensRequest>,
) -> impl IntoResponse {
    tracing::info!(
        model = %payload.model,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages/count_tokens request"
    );

    let total_tokens = token::count_all_tokens(
        &payload.model,
        payload.system.as_deref(),
        &payload.messages,
        payload.tools.as_deref(),
    ) as i32;

    Json(CountTokensResponse {
        input_tokens: total_tokens.max(1) as i32,
    })
}

/// POST /cc/v1/messages
///
/// Claude Code 兼容端点，与 /v1/messages 的区别在于：
/// - 流式响应会等待 kiro 端返回 contextUsageEvent 后再发送 message_start
/// - message_start 中的 input_tokens 是从 contextUsageEvent 计算的准确值
pub async fn post_messages_cc(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /cc/v1/messages request"
    );

    // 从入站请求头 + TCP 对端地址识别来源画像（设备/IP/OS/浏览器，用于「最近请求」展示）
    let client = ClientInfo::from_headers_with_peer(&headers, Some(peer));

    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    // 检查是否应本地处理 WebSearch 请求（tool_choice 强制 / 纯 web_search 单工具 / Claude Code 前缀）
    if websearch::should_handle_websearch_request(&payload) {
        tracing::info!("检测到 WebSearch 请求，路由到本地 WebSearch 处理");

        // 估算输入 tokens（只读计数，传引用避免深拷贝整个对话历史）
        let input_tokens = token::count_all_tokens(
            &payload.model,
            payload.system.as_deref(),
            &payload.messages,
            payload.tools.as_deref(),
        ) as i32;

        return websearch::handle_websearch_request(provider, &payload, input_tokens).await;
    }

    // 混合工具场景：请求带 web_search 但未显式触发搜索，剔除 web_search 后走常规转发，
    // 避免把 web_search 原样下发给 Kiro 触发 400 Improperly formed request。
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到混合工具列表中的 web_search，剔除后转发上游");
        websearch::strip_web_search_tools(&mut payload);
    }

    // 转换请求
    let conversion_result = match convert_request(&payload) {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => {
                    ("invalid_request_error", format!("模型不支持: {}", model))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
            };
            tracing::warn!("请求转换失败: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // 构建 Kiro 请求体（发上游前，超阈值时执行输入压缩；profile_arn 由 provider 层注入）
    let request_body = match build_kiro_request_body(
        conversion_result.conversation_state,
        &current_compression(),
    ) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    // 估算输入 tokens（只读计数，传引用避免深拷贝整个对话历史）
    let input_tokens = token::count_all_tokens(
        &payload.model,
        payload.system.as_deref(),
        &payload.messages,
        payload.tools.as_deref(),
    ) as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;

    if payload.stream {
        // 流式响应（缓冲模式）
        handle_stream_request_buffered(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
            client,
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = extract_thinking_enabled() && thinking_enabled;
        handle_non_stream_request(provider, &request_body, &payload.model, input_tokens, extract_thinking, tool_name_map, client).await
    }
}

/// 处理流式请求（缓冲版本）
///
/// 与 `handle_stream_request` 不同，此函数会缓冲所有事件直到流结束，
/// 然后用从 contextUsageEvent 计算的正确 input_tokens 生成 message_start 事件。
async fn handle_stream_request_buffered(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    estimated_input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    client: ClientInfo,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let (response, meta) = match provider.call_api_stream(request_body).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 创建缓冲流处理上下文
    let ctx = BufferedStreamContext::new(model, estimated_input_tokens, thinking_enabled, tool_name_map);

    // 创建缓冲 SSE 流（流结束时用 meta + 最终 usage 埋点）
    let stream = create_buffered_sse_stream(provider, response, ctx, meta, client);

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 创建缓冲 SSE 事件流
///
/// 工作流程：
/// 1. 等待上游流完成，期间只发送 ping 保活信号
/// 2. 使用 StreamContext 的事件处理逻辑处理所有 Kiro 事件，结果缓存
/// 3. 流结束后，用正确的 input_tokens 更正 message_start 事件
/// 4. 一次性发送所有事件
fn create_buffered_sse_stream(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    response: reqwest::Response,
    ctx: BufferedStreamContext,
    meta: crate::kiro::provider::CallMeta,
    client: ClientInfo,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let body_stream = response.bytes_stream();

    stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
            meta,
            client,
            provider,
        ),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, meta, client, provider)| async move {
            if finished {
                return None;
            }

            loop {
                tokio::select! {
                    // 使用 biased 模式，优先检查 ping 定时器
                    // 避免在上游 chunk 密集时 ping 被"饿死"
                    biased;

                    // 优先检查 ping 保活（等待期间唯一发送的数据）
                    _ = ping_interval.tick() => {
                        tracing::trace!("发送 ping 保活事件（缓冲模式）");
                        let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                        return Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, meta, client, provider)));
                    }

                    // 然后处理数据流
                    chunk_result = body_stream.next() => {
                        match chunk_result {
                            Some(Ok(chunk)) => {
                                // 解码事件
                                if let Err(e) = decoder.feed(&chunk) {
                                    tracing::warn!("缓冲区溢出: {}", e);
                                }

                                let mut last_decode_err: Option<String> = None;
                                for result in decoder.decode_iter() {
                                    match result {
                                        Ok(frame) => {
                                            // from_frame 按值吞 frame，事件类型须在 move 前先拥有化捕获。
                                            let et = frame.event_type().map(|s| s.to_string());
                                            match Event::from_frame(frame) {
                                                Ok(event) => {
                                                    // 缓冲事件（复用 StreamContext 的处理逻辑）。
                                                    // in-band Event::Error/Exception 会在此置 completion 失败态。
                                                    ctx.process_and_buffer(&event);
                                                }
                                                Err(err) => {
                                                    // 帧层解码成功、Frame→Event 反序列化失败：
                                                    // toolUseEvent 失败=工具调用不可恢复丢失，置 DecoderStopped
                                                    // 失败态（收尾靠 None 分支补发 SSE error），避免截断被当成功不重试；
                                                    // 非 tool 帧解析失败历史上就允许被忽略，仅告警不置失败态，防误伤正常流。
                                                    if et.as_deref() == Some("toolUseEvent") {
                                                        tracing::warn!("buffered toolUseEvent 帧解析失败,按响应截断处理: {}", err);
                                                        ctx.mark_decoder_stopped(format!("toolUseEvent 帧解析失败: {}", err));
                                                    } else {
                                                        tracing::warn!("buffered 事件帧解析失败(event_type={:?}),已忽略: {}", et.as_deref(), err);
                                                    }
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            last_decode_err = Some(e.to_string());
                                            tracing::warn!("解码事件失败: {}", e);
                                        }
                                    }
                                }
                                // 解码器永久停止：响应必然截断，置失败态供收尾记账。
                                if decoder.is_stopped() {
                                    ctx.mark_decoder_stopped(
                                        last_decode_err.unwrap_or_else(|| "解码器连续错误已停止".to_string()),
                                    );
                                }
                                // 继续读取下一个 chunk，不发送任何数据
                            }
                            Some(Err(e)) => {
                                tracing::error!("读取响应流失败: {}", e);
                                // 上游流中途失败：置传输失败态（供收尾按 NetworkError 记账），
                                // 先发 SSE error 事件显式告知"本次未正常完成"，再补齐已缓冲事件收尾。
                                // 否则 Claude Code 把截断输出当成功、不重试。幂等保留首因。
                                ctx.mark_transport_error(e.to_string());
                                let mut all_events = Vec::new();
                                if !ctx.error_event_emitted() {
                                    all_events.push(SseEvent::error_event(
                                        ctx.completion().sse_error_type(),
                                        ctx.completion().client_message(),
                                    ));
                                    ctx.mark_error_event_emitted();
                                }
                                all_events.extend(ctx.finish_and_get_all_events());
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                emit_buffered_usage(&provider, &ctx, &meta, &client);
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, meta, client, provider)));
                            }
                            None => {
                                // 流结束，完成处理并返回所有事件（已更正 input_tokens）。
                                // 兜底：完成状态为失败（in-band 错误/解码器停止）但尚未发过 error 事件时，
                                // 在收尾处补发一个 error 事件，确保客户端不把截断输出当成功。
                                let mut all_events = Vec::new();
                                if !ctx.completion().is_ok() && !ctx.error_event_emitted() {
                                    all_events.push(SseEvent::error_event(
                                        ctx.completion().sse_error_type(),
                                        ctx.completion().client_message(),
                                    ));
                                    ctx.mark_error_event_emitted();
                                }
                                all_events.extend(ctx.finish_and_get_all_events());
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                emit_buffered_usage(&provider, &ctx, &meta, &client);
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, meta, client, provider)));
                            }
                        }
                    }
                }
            }
        },
    )
    .flatten()
}

/// 缓冲流结束时埋点一条成功记录
fn emit_buffered_usage(
    provider: &crate::kiro::provider::KiroProvider,
    ctx: &BufferedStreamContext,
    meta: &crate::kiro::provider::CallMeta,
    client: &ClientInfo,
) {
    let usage = ctx.resolved_usage();
    let mut record = crate::usage::RequestRecord::new(
        Uuid::new_v4().to_string(),
        meta.model.clone().unwrap_or_default(),
    );
    record.credential_id = Some(meta.credential_id);
    record.session_id = meta.session_id.clone();
    record.is_streaming = meta.is_streaming;
    record.input_tokens = usage.input_tokens;
    record.output_tokens = usage.output_tokens;
    record.cache_read_tokens = usage.cache_read_tokens;
    record.cache_creation_tokens = usage.cache_creation_tokens;
    record.credits_used = usage.credits_used;
    record.latency_ms = meta.latency_ms;
    record.retries = meta.retries;
    // 去硬编码 Success：按真实完成状态记账（截断/上游错误不再被记成成功）。
    record.outcome = ctx.completion_outcome();
    // 生命周期累计花费：把本次真实 credit 消耗累加到该凭据（独立于用量保留期，只增不清）。
    if let Some(c) = record.credits_used {
        provider.report_credits(meta.credential_id, c);
    }
    client.apply(&mut record);
    crate::usage::emit_record(record);
}

#[cfg(test)]
mod tier3_hotreload_tests {
    //! TIER3 配置热重载回归：AppState 曾固化的热路径开关改用进程级镜像后，
    //! setter 写入应被对应 getter（handler 热路径读点）立即读到，证明改配置即时生效。
    //!
    //! 注意：镜像是进程级 static，测试间共享同一份。这些测试各自操作**不同的**镜像，
    //! 且末尾恢复默认，避免串扰；不并发断言同一镜像的中间态。
    use super::*;

    #[test]
    fn test_extract_thinking_mirror_roundtrip() {
        set_extract_thinking(true);
        assert!(extract_thinking_enabled(), "set true 后热路径应读到 true");
        set_extract_thinking(false);
        assert!(!extract_thinking_enabled(), "set false 后热路径应读到 false");
    }

    #[test]
    fn test_compression_mirror_roundtrip() {
        use crate::model::config::CompressionConfig;
        let mut c = CompressionConfig::default();
        // 翻转 enabled 以可观测地区分（不依赖具体默认值，只验证 setter→getter 传递）
        c.enabled = !c.enabled;
        let flipped = c.enabled;
        set_compression(c);
        assert_eq!(
            current_compression().enabled,
            flipped,
            "set_compression 后热路径应读到新的 compression 快照"
        );
        // 复位默认，避免影响其它测试
        set_compression(CompressionConfig::default());
    }
}

#[cfg(test)]
mod truncation_completion_tests {
    //! 「截断即成功」修复回归：验证非流式收尾逻辑依赖的
    //! 解码 → CompletionStatus → HTTP 状态码 链路。
    //!
    //! 非流式 handler 与实盘 provider 强耦合，无法在单测里跑完整请求；
    //! 这里用**真实构造的 event-stream 帧**驱动 handler 内部同一套解码 + 事件分类逻辑，
    //! 断言 in-band error 帧会被识别为失败态，且映射到非 200。
    use super::*;
    use crate::kiro::parser::crc::crc32;

    /// 构造一个带指定 message-type / 头部 / payload 的 event-stream 帧。
    ///
    /// 头部编码：name_len(1) + name + type(7=String) + value_len(2) + value。
    fn build_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        let mut header_bytes = Vec::new();
        for (name, value) in headers {
            header_bytes.push(name.len() as u8);
            header_bytes.extend_from_slice(name.as_bytes());
            header_bytes.push(7u8); // String
            header_bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
            header_bytes.extend_from_slice(value.as_bytes());
        }
        let header_length = header_bytes.len() as u32;
        let total_length = (PRELUDE_SIZE + header_bytes.len() + payload.len() + 4) as u32;

        let mut buf = Vec::new();
        buf.extend_from_slice(&total_length.to_be_bytes());
        buf.extend_from_slice(&header_length.to_be_bytes());
        let prelude_crc = crc32(&buf[..8]);
        buf.extend_from_slice(&prelude_crc.to_be_bytes());
        buf.extend_from_slice(&header_bytes);
        buf.extend_from_slice(payload);
        let msg_crc = crc32(&buf);
        buf.extend_from_slice(&msg_crc.to_be_bytes());
        buf
    }

    // 引入 PRELUDE_SIZE
    use crate::kiro::parser::frame::PRELUDE_SIZE;

    /// 复刻非流式 handler 的解码收尾判定：drain 全部帧，遇 in-band error/非 CL 异常/
    /// 解码器停止置失败态，返回最终 CompletionStatus。
    fn decode_to_completion(data: &[u8]) -> CompletionStatus {
        let mut decoder = EventStreamDecoder::new();
        decoder.feed(data).unwrap();

        let mut completion = CompletionStatus::Ok;
        let mut last_err: Option<String> = None;
        for result in decoder.decode_iter() {
            match result {
                Ok(frame) => {
                    // 忠实镜像非流式收尾：move 前先拥有化事件类型，供 Err 分支判据用。
                    let et = frame.event_type().map(|s| s.to_string());
                    match Event::from_frame(frame) {
                        Ok(event) => match event {
                            Event::Error { error_code, error_message } => {
                                if completion.is_ok() {
                                    completion = CompletionStatus::UpstreamError {
                                        code: error_code,
                                        message: error_message,
                                    };
                                }
                            }
                            Event::Exception { exception_type, message } => {
                                if exception_type != "ContentLengthExceededException"
                                    && completion.is_ok()
                                {
                                    completion = CompletionStatus::UpstreamError {
                                        code: exception_type,
                                        message,
                                    };
                                }
                            }
                            _ => {}
                        },
                        Err(_) => {
                            // 镜像非流式：toolUseEvent 帧解析失败 → DecoderStopped 失败态。
                            if et.as_deref() == Some("toolUseEvent") && completion.is_ok() {
                                completion = CompletionStatus::DecoderStopped {
                                    message: "toolUseEvent 帧解析失败".to_string(),
                                };
                            }
                        }
                    }
                }
                Err(e) => last_err = Some(e.to_string()),
            }
        }
        if decoder.is_stopped() && completion.is_ok() {
            completion = CompletionStatus::DecoderStopped {
                message: last_err.unwrap_or_default(),
            };
        }
        completion
    }

    #[test]
    fn test_inband_error_frame_maps_to_non_200() {
        // 回归 BUG①：in-band error 帧过去落入 `_ => {}` 被忽略、照返 200。
        // 现在应被识别为 UpstreamError，映射非 200。
        let frame = build_frame(
            &[(":message-type", "error"), (":error-code", "InternalServerException")],
            b"upstream exploded",
        );
        let completion = decode_to_completion(&frame);

        assert!(!completion.is_ok(), "in-band error 帧应被识别为失败");
        assert_ne!(completion.http_status_u16(), 200, "失败必须返回非 200");
        assert_eq!(completion.http_status_u16(), 502);
        assert_eq!(completion.outcome(), crate::usage::RequestOutcome::ServerError);
    }

    #[test]
    fn test_inband_throttling_error_frame_maps_to_429() {
        let frame = build_frame(
            &[(":message-type", "error"), (":error-code", "ThrottlingException")],
            b"slow down",
        );
        let completion = decode_to_completion(&frame);
        assert_eq!(completion.http_status_u16(), 429);
        assert_eq!(completion.outcome(), crate::usage::RequestOutcome::RateLimited);
    }

    #[test]
    fn test_content_length_exception_frame_stays_ok() {
        // 铁律：ContentLengthExceededException 干净收尾，不算失败，仍走 200。
        let frame = build_frame(
            &[
                (":message-type", "exception"),
                (":exception-type", "ContentLengthExceededException"),
            ],
            b"max tokens reached",
        );
        let completion = decode_to_completion(&frame);
        assert!(completion.is_ok(), "CL 异常不应被判为失败");
        assert_eq!(completion.outcome(), crate::usage::RequestOutcome::Success);
    }

    #[test]
    fn test_toolusevent_parse_failure_maps_to_502() {
        // 回归：toolUseEvent 帧解析失败过去被静默丢弃 → 客户端按 end_turn 当成功不重试。
        // 现在应置 DecoderStopped 失败态，映射 502/ServerError，供收尾补发 error 触发重试。
        // 帧 CRC/framing 合法（decoder 不 is_stopped），仅 ToolUseEvent::from_frame 因非法 JSON 返 Err。
        let frame = build_frame(
            &[(":message-type", "event"), (":event-type", "toolUseEvent")],
            b"not valid json",
        );
        let completion = decode_to_completion(&frame);
        assert!(!completion.is_ok(), "toolUseEvent 解析失败应判失败态");
        assert_eq!(completion.http_status_u16(), 502);
        assert_eq!(completion.outcome(), crate::usage::RequestOutcome::ServerError);
    }

    #[test]
    fn test_non_tool_parse_failure_stays_ok() {
        // 零倒退承诺：非 tool 帧解析失败只应告警、不置失败态。
        // 注意 AssistantResponseEvent.content 有 serde(default)，故须用非法 JSON 而非 `{}` 才能触发反序列化失败。
        let frame = build_frame(
            &[(":message-type", "event"), (":event-type", "assistantResponseEvent")],
            b"not valid json",
        );
        let completion = decode_to_completion(&frame);
        assert!(completion.is_ok(), "非 tool 帧解析失败只应告警,不置失败态");
        assert_eq!(completion.outcome(), crate::usage::RequestOutcome::Success);
    }

    #[test]
    fn test_from_frame_toolusevent_malformed_errs() {
        // 防呆：锁死「frame 层成功、Event 层失败、event_type 在 move 前可取」三条前提，
        // 防未来 payload 结构变动悄悄使该帧变成 Ok。
        let raw = build_frame(
            &[(":message-type", "event"), (":event-type", "toolUseEvent")],
            b"not valid json",
        );
        let mut d = EventStreamDecoder::new();
        d.feed(&raw).unwrap();
        let frame = d.decode_iter().next().unwrap().unwrap();
        assert_eq!(frame.event_type(), Some("toolUseEvent"));
        assert!(Event::from_frame(frame).is_err());
    }
}
