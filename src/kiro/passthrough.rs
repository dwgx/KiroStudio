//! 自定义 API「代挂透传」——Anthropic 兼容上游中转站的反向代理。
//!
//! 语义(dwgx 定):自定义 API 凭据(auth_method=custom_api)是一个 **Anthropic 兼容上游**
//! (base_url + api_key)。当选号命中这类凭据时,把客户端的 `/v1/messages` 请求**原样透传**
//! 到 `base_url`、换用该凭据的 api_key,响应流**原样回**给客户端。入口=出口=Anthropic,
//! 零协议转换——效果等同用户直接拿那个 key 打上游。
//!
//! ⚠️ 与 Kiro 主路径完全隔离:透传响应**绝不进** Kiro 的 event-stream 解码器 / StreamContext,
//! 而是把上游的字节流原样 [`Body::from_stream`] 回去。Kiro 转发路径一行不改。

use std::convert::Infallible;

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::StreamExt;

use crate::http_client::build_streaming_client;
use crate::kiro::model::credentials::KiroCredentials;
use crate::model::config::TlsBackend;

/// 把一次 Anthropic 请求原样透传到自定义 API 上游,响应流式原样返回。
///
/// - `cred`:命中的自定义 API 凭据(提供 base_url / api_key / 代理)。
/// - `raw_body`:客户端原始 `/v1/messages` 请求体(**未经 Kiro 转换**)。
/// - `global_proxy` / `tls_backend`:复用全局代理与 TLS 后端配置。
///
/// 成功返回上游响应的流式 [`Response`](原样透传 status/body);失败返回 502 错误响应。
pub async fn forward(
    cred: &KiroCredentials,
    raw_body: Bytes,
    global_proxy: Option<&crate::http_client::ProxyConfig>,
    tls_backend: TlsBackend,
) -> Response {
    let base = match cred.base_url.as_deref() {
        Some(b) if !b.trim().is_empty() => b.trim_end_matches('/').to_string(),
        _ => return err_response(StatusCode::BAD_GATEWAY, "自定义 API 凭据缺少 base_url"),
    };
    // Anthropic messages 端点:base 已含 /v1 则不重复拼;否则补 /v1/messages。
    let url = if base.ends_with("/v1") || base.contains("/v1/") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    };

    // 透传用流式 client:read_timeout(空闲间隔)而非总超时,防长回复被中途掐断
    // (与 Kiro 对话路径同款,根因见 build_streaming_client 注释)。
    let proxy = cred.effective_proxy(global_proxy);
    let client = match build_streaming_client(proxy.as_ref(), 720, tls_backend) {
        Ok(c) => c,
        Err(e) => return err_response(StatusCode::BAD_GATEWAY, &format!("构建透传 client 失败: {e}")),
    };

    // 组装转发请求:换上该凭据的 api_key(Anthropic 双头兼容:x-api-key + Authorization),
    // 带上 anthropic-version(上游中转站通常要求),content-type json。原样发送 raw_body。
    let mut req = client
        .post(&url)
        .header(header::CONTENT_TYPE, "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(raw_body);
    if let Some(key) = cred.api_key.as_deref().filter(|k| !k.is_empty()) {
        req = req
            .header("x-api-key", key)
            .header(header::AUTHORIZATION, format!("Bearer {key}"));
    }

    let upstream = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("[透传] 上游请求失败({}): {e}", url);
            return err_response(StatusCode::BAD_GATEWAY, &format!("透传上游请求失败: {e}"));
        }
    };

    let status = upstream.status();
    // 保留上游 content-type(流式为 text/event-stream,非流式为 application/json)。
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    // 原样把上游字节流转回客户端——不解析、不改写。上游怎么发,客户端怎么收。
    let byte_stream = upstream
        .bytes_stream()
        .map(|chunk| -> Result<Bytes, Infallible> {
            match chunk {
                Ok(b) => Ok(b),
                // 上游流中断:结束流(客户端会看到连接提前结束)。透传不臆造错误事件。
                Err(e) => {
                    tracing::warn!("[透传] 上游流读取中断: {e}");
                    Ok(Bytes::new())
                }
            }
        });

    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from_stream(byte_stream))
        .unwrap_or_else(|_| err_response(StatusCode::BAD_GATEWAY, "构建透传响应失败"))
}

/// 构建一个 Anthropic 风格的错误响应(供透传失败时返回)。
fn err_response(status: StatusCode, msg: &str) -> Response {
    let body = serde_json::json!({
        "type": "error",
        "error": { "type": "api_error", "message": msg }
    });
    (status, axum::Json(body)).into_response()
}
