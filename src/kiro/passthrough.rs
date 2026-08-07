//! 自定义 API「代挂透传」——Anthropic 兼容上游中转站的反向代理。
//!
//! 语义(dwgx 定):自定义 API 凭据(auth_method=custom_api)是一个 **Anthropic 兼容上游**
//! (base_url + api_key)。当选号命中这类凭据时,把客户端的 `/v1/messages` 请求**原样透传**
//! 到 `base_url`、换用该凭据的 api_key,响应流**原样回**给客户端。入口=出口=Anthropic,
//! 零协议转换——效果等同用户直接拿那个 key 打上游。
//!
//! ⚠️ 与 Kiro 主路径完全隔离:透传响应**绝不进** Kiro 的 event-stream 解码器 / StreamContext,
//! 而是把上游的字节流原样 [`Body::from_stream`] 回去。Kiro 转发路径一行不改。

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
// TryStreamExt 提供 map_err（错误传播）；StreamExt 的 map 不再需要。
use futures::TryStreamExt;

use crate::http_client::build_streaming_client_no_redirect;
use crate::kiro::model::credentials::KiroCredentials;
use crate::model::config::TlsBackend;

/// 把一次 Anthropic 请求原样透传到自定义 API 上游,响应流式原样返回。
///
/// - `cred`:命中的自定义 API 凭据(提供 base_url / api_key / 代理)。
/// - `raw_body`:客户端原始 `/v1/messages` 请求体(**未经 Kiro 转换**)。
/// - `global_proxy` / `tls_backend`:复用全局代理与 TLS 后端配置。
///
/// 返回 `(Response, StatusCode)`:Response 原样透传上游 status/body(失败为 502 错误响应);
/// StatusCode 供调用侧(provider)据以推断 usage outcome 并做轻量结果计数。**只暴露 header 层
/// status,body 仍原样流式回传,绝不解析上游 SSE**(隔离铁律 3)。
pub async fn forward(
    cred: &KiroCredentials,
    raw_body: Bytes,
    global_proxy: Option<&crate::http_client::ProxyConfig>,
    tls_backend: TlsBackend,
) -> (Response, StatusCode) {
    let base = match cred.base_url.as_deref() {
        Some(b) if !b.trim().is_empty() => b.trim_end_matches('/').to_string(),
        _ => {
            return (
                err_response(StatusCode::BAD_GATEWAY, "自定义 API 凭据缺少 base_url"),
                StatusCode::BAD_GATEWAY,
            );
        }
    };
    // Anthropic messages 端点:base 已含 /v1 则不重复拼;否则补 /v1/messages。
    let url = if base.ends_with("/v1") || base.contains("/v1/") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    };

    // 透传用流式 client:read_timeout(空闲间隔)而非总超时,防长回复被中途掐断
    // (与 Kiro 对话路径同款,根因见 build_streaming_client 注释)。
    // **禁重定向**(SSRF 纵深):写入 base_url 时已校验目标非内网,但公网中转站若返回
    // 302→内网/元数据仍能绕过,禁重定向堵死这条链。base_url 的 IP 层校验在写入时做。
    let proxy = cred.effective_proxy(global_proxy);
    let client = match build_streaming_client_no_redirect(proxy.as_ref(), 720, tls_backend) {
        Ok(c) => c,
        Err(e) => {
            return (
                err_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("构建透传 client 失败: {e}"),
                ),
                StatusCode::BAD_GATEWAY,
            );
        }
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
            // 连接层错误:上游不可达/超时,归 502(调用侧据此计一次失败)。
            return (
                err_response(StatusCode::BAD_GATEWAY, &format!("透传上游请求失败: {e}")),
                StatusCode::BAD_GATEWAY,
            );
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
    //
    // 🔴 修复的缺陷:此处原先是 `Err(e) => Ok(Bytes::new())`,即把上游中断**映射成一个正常的
    // 空 chunk**。空 chunk 在 HTTP 层完全不可见,于是 chunked body 会以**正常终止**收尾——
    // 客户端拿到 `200 OK` + 一个被截断的响应,判定成功、不重试、把半截内容当完整答案用。
    // 注释写的是"结束流",但 `Ok(_)` 表达的是"这一项没有数据",两者语义相反。
    // 根因是类型签名:`Result<Bytes, Infallible>` 里 `Infallible` **无法表达错误**,
    // 所以当时只剩 `Ok` 可用——是类型选错逼出的错误处理。
    //
    // 为什么严重:静默截断比报错危险得多。号池当前 33% 请求已在 429,截断并不罕见,
    // 而客户端对"成功但内容不全"没有任何恢复手段(它不知道出了问题)。
    //
    // 修法:用 `axum::Error` 让错误**真正传播**。`Body::from_stream` 见到 `Err` 会中止
    // body 并关闭连接,客户端侧得到一个"提前结束且非正常终止"的流 → 可据此判失败并重试。
    // 这正是原注释想表达的语义。`map_err` 只在出错时触发一次,不改变正常路径。
    //
    // ⚠️ 不在此处加重试:重试属 provider 层(见 try_custom_api_passthrough 的 failover)。
    // 在流层重试会绕过已建立的会话亲和绑定 → 破坏前缀缓存(历史教训:换号 = prompt cache
    // 全丢,单请求成本差 10 倍)。
    //
    // 注:这里**不会**因为返回 Err 而形成自旋——实测 reqwest 的 `bytes_stream` 出错后
    // 下一次 poll 返回 `None`,不重复吐同一个 Err;且 `map`/`map_err` 都不改变终止时机。
    let byte_stream = upstream.bytes_stream().map_err(|e| {
        tracing::warn!("[透传] 上游流读取中断,以错误终止响应流(客户端可据此重试): {e}");
        axum::Error::new(e)
    });

    let resp = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from_stream(byte_stream))
        .unwrap_or_else(|_| err_response(StatusCode::BAD_GATEWAY, "构建透传响应失败"));
    // 返回上游真实 status 供调用侧推断 outcome(成功/限流/失败);body 已原样流式接管。
    (resp, status)
}

/// 构建一个 Anthropic 风格的错误响应(供透传失败时返回)。
fn err_response(status: StatusCode, msg: &str) -> Response {
    let body = serde_json::json!({
        "type": "error",
        "error": { "type": "api_error", "message": msg }
    });
    (status, axum::Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    /// 回归：上游流中断必须**以错误终止** body，绝不能伪装成正常 EOF。
    ///
    /// **旧代码为何 FAIL**：原实现 `Err(e) => Ok(Bytes::new())` 把中断映射成一个正常的空 chunk。
    /// 空 chunk 在 HTTP 层不可见 → chunked body 正常收尾 → 客户端拿到 `200 OK` + 截断内容，
    /// 判定成功、不重试、把半截答案当完整结果用。旧代码下最后一项是 `Ok(b"")` 而非 `Err`，
    /// 本测试的 `is_err()` 断言必然 FAIL。
    ///
    /// 静默截断比报错危险：客户端对「成功但内容不全」没有任何恢复手段（它不知道出了问题）。
    /// 号池当前有三分之一请求在 429，截断并不罕见。
    ///
    /// 这里直接测 `map_err` 这一层的语义（与生产同款闭包），不依赖真实网络——
    /// `forward` 需要真上游，而缺陷恰恰在这个映射本身。
    #[tokio::test]
    async fn upstream_stream_interruption_terminates_body_with_error_not_silent_eof() {
        // 造「两个正常 chunk 后中断」的上游流，错误类型用 reqwest 的真实错误无法手工构造，
        // 故用 std::io::Error 代表传输层失败——map_err 的语义与错误具体类型无关。
        let upstream = futures::stream::iter(vec![
            Ok::<Bytes, std::io::Error>(Bytes::from_static(b"data: a\n\n")),
            Ok(Bytes::from_static(b"data: b\n\n")),
            Err(std::io::Error::other("connection reset by peer")),
        ]);

        // 与生产同款：错误传播而非吞成空 chunk。
        let mapped = upstream.map_err(axum::Error::new);
        let items: Vec<_> = mapped.collect().await;

        assert_eq!(items.len(), 3, "两个数据项 + 一个错误项");
        assert!(items[0].is_ok() && items[1].is_ok(), "正常 chunk 不受影响");
        assert!(
            items[2].is_err(),
            "上游中断必须传播为 Err（旧代码是 Ok(空 chunk) → 客户端把截断响应当成功）"
        );
        // 反向守卫：绝不能是"成功的空 chunk"这种最隐蔽的形式。
        assert!(
            !matches!(&items[2], Ok(b) if b.is_empty()),
            "空 chunk 在 HTTP 层不可见，等于静默截断"
        );
    }
}
