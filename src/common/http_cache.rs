//! 响应缓存指令：一处定义「什么东西可以被缓存多久」，各路由树共用。
//!
//! # 为什么需要这个模块
//! 「没有 `Cache-Control`」**不等于**「不缓存」，而等于「由缓存方自己拿主意」。
//! HTTP 允许缓存对没有显式指令的响应做启发式缓存（heuristic caching，
//! RFC 9111 §4.2.2）。对一个返回**用户私有数据**的 JSON 端点来说，这有两级危害：
//!
//! 1. 浏览器可能把响应体落到用户磁盘的缓存目录里；
//! 2. 更要紧的是，一旦前面挂了 nginx / CDN / 公司出口代理，这类响应就是
//!    **允许被共享缓存存下来的**。共享缓存里存着一份用户私有数据，
//!    下一个打同一个 URL 的人可能直接拿到它——跨用户串号。
//!
//! Portal 的 `/portal/api/keys` 在积分关闭或用户已上车时会下发**明文凭据**
//! （见 [`crate::portal::http`] 的 `gate_plaintext`）。这条路径上的响应被任何
//! 中间缓存留存都是不可接受的。
//!
//! # 为什么做成中间件，而不是逐个 handler 加头
//! 逐个加的写法有一个确定的失效模式：**日后新增端点必然漏**。漏掉不会有编译
//! 错误、不会有用例变红，表现是某条新端点悄悄变成可缓存的。装在路由树上意味着
//! 「新增路由自动继承」——安全属性的默认值应该是安全的那一侧。
//!
//! # 为什么是 `no-store` 而不是 `no-cache`
//! - `no-cache`：**可以存**，但每次用前必须回源验证。明文 key 落盘了。
//! - `no-store`：不得写入任何持久或非持久存储。这才是我们要的语义。
//!
//! 附带 `Pragma: no-cache` 与 `Expires: 0`：HTTP/1.0 时代的老代理不认
//! `Cache-Control`，而内网里"前面挂了个上古代理"并不罕见。两个头加起来几十
//! 字节，换掉一整类「中间某一跳不认新语义」的失败，划算。

use axum::{
    extract::Request,
    http::{HeaderValue, header},
    middleware::Next,
    response::Response,
};

/// 私有/敏感响应的缓存指令。
///
/// `private` 在有 `no-store` 时是冗余的，但保留它是给**只认一部分指令**的中间
/// 缓存留一道后手：万一某一跳忽略了 `no-store`，`private` 至少还能阻止它把响应
/// 放进跨用户共享的那份存储里。纵深防御，不是笔误。
const NO_STORE: &str = "no-store, no-cache, must-revalidate, private";

/// HTML 页面的缓存指令：可以存，但每次用前必须回源验证。
///
/// 页面本身不含用户数据（Portal 是一张静态单页，登录态全靠 JS 打 API 拿），
/// 所以不必 `no-store`；但**必须**回源验证，否则发新版后浏览器会拿着旧 HTML
/// 去打新 API——那种错法的表现是"页面看着正常，功能莫名其妙坏掉"，很难查。
const NO_CACHE: &str = "no-cache, must-revalidate";

/// 给响应打上「绝不缓存」，用于返回用户私有数据的 API。
///
/// **覆盖而非补充**：用 `insert` 而不是 `append`。若 handler 自己设过一个更宽松
/// 的值（比如有人从别处拷了 `max-age=3600` 过来），append 会留下两个互相矛盾的
/// 头，实际行为取决于缓存实现挑哪个——那是最坏的一种不确定性。
pub async fn no_store(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let h = response.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(NO_STORE));
    h.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    h.insert(header::EXPIRES, HeaderValue::from_static("0"));
    response
}

/// 给响应打上「可存但必须回源验证」，用于 HTML 页面。
pub async fn no_cache(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(NO_CACHE));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, http::StatusCode, routing::get};
    use tower::ServiceExt;

    /// 故意设一个宽松缓存头的 handler：用来验证中间件是**覆盖**而非叠加。
    async fn leaky() -> Response {
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CACHE_CONTROL, "public, max-age=31536000")
            .body(Body::from("secret"))
            .expect("build")
    }

    fn header_values(r: &Response, name: header::HeaderName) -> Vec<String> {
        r.headers()
            .get_all(name)
            .iter()
            .map(|v| v.to_str().unwrap_or("<binary>").to_string())
            .collect()
    }

    #[tokio::test]
    async fn no_store_sets_all_three_headers() {
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(no_store));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("res");

        assert_eq!(
            res.headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some(NO_STORE)
        );
        // HTTP/1.0 老代理的兜底，见模块文档。
        assert_eq!(
            res.headers()
                .get(header::PRAGMA)
                .and_then(|v| v.to_str().ok()),
            Some("no-cache")
        );
        assert_eq!(
            res.headers()
                .get(header::EXPIRES)
                .and_then(|v| v.to_str().ok()),
            Some("0")
        );
    }

    /// **关键用例**：handler 自带宽松缓存头时，必须被替换掉，不能并存。
    ///
    /// 若哪天有人把 `insert` 改成 `append`，响应里会同时出现
    /// `public, max-age=31536000` 和 `no-store`，行为取决于缓存实现挑哪个——
    /// 而挑错的那一侧是「明文 key 被 CDN 存一年」。
    #[tokio::test]
    async fn no_store_overrides_a_permissive_handler_header() {
        let app = Router::new()
            .route("/", get(leaky))
            .layer(axum::middleware::from_fn(no_store));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("res");

        let values = header_values(&res, header::CACHE_CONTROL);
        assert_eq!(
            values.len(),
            1,
            "cache-control 出现了 {} 个值：{values:?}。\
             两个互相矛盾的指令并存时，实际行为取决于缓存实现挑哪个——\
             必须用 insert 覆盖，不能 append。",
            values.len()
        );
        assert_eq!(values[0], NO_STORE);
        assert!(
            !values[0].contains("max-age=31536000"),
            "handler 的宽松指令残留了"
        );
    }

    #[tokio::test]
    async fn no_cache_allows_storage_but_requires_revalidation() {
        let app = Router::new()
            .route("/", get(|| async { "<html>" }))
            .layer(axum::middleware::from_fn(no_cache));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("res");

        let cc = res
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .expect("有 cache-control");
        assert!(cc.contains("no-cache"), "必须要求回源验证");
        assert!(cc.contains("must-revalidate"));
        // 页面不含用户数据，不该用 no-store：那会让每次导航都整份重下，
        // 白白放弃 304。这条断言把「两个策略别写混」钉住。
        assert!(
            !cc.contains("no-store"),
            "HTML 页面用 no-store 是过度收紧，放弃了 304 的好处"
        );
    }

    /// 两个策略不能是同一个字符串——否则上面那条"别写混"的断言会失去意义。
    #[test]
    fn the_two_policies_are_actually_different() {
        assert_ne!(NO_STORE, NO_CACHE);
    }
}
