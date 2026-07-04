//! Admin UI 路由配置

use axum::{
    Router,
    body::Body,
    http::{Response, StatusCode, Uri, header},
    response::IntoResponse,
    routing::get,
    Json,
};
use rust_embed::Embed;

/// 嵌入前端构建产物
#[derive(Embed)]
#[folder = "admin-ui/dist"]
struct Asset;

/// 创建 Admin UI 路由
pub fn create_admin_ui_router() -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/api/random-bg", get(random_bg_handler))
        .route("/api/bg-img", get(bg_img_proxy_handler))
        .route("/{*file}", get(static_handler))
}

/// 处理首页请求
async fn index_handler() -> impl IntoResponse {
    serve_index()
}

/// 处理静态文件请求
async fn static_handler(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');

    // 安全检查：拒绝包含 .. 的路径
    if path.contains("..") {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("Invalid path"))
            .expect("Failed to build response");
    }

    // 尝试获取请求的文件
    if let Some(content) = Asset::get(path) {
        let mime = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();

        // 根据文件类型设置不同的缓存策略
        let cache_control = get_cache_control(path);

        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .header(header::CACHE_CONTROL, cache_control)
            .body(Body::from(content.data.into_owned()))
            .expect("Failed to build response");
    }

    // SPA fallback: 如果文件不存在且不是资源文件，返回 index.html
    if !is_asset_path(path) {
        return serve_index();
    }

    // 404
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::from("Not found"))
        .expect("Failed to build response")
}

/// 提供 index.html
fn serve_index() -> Response<Body> {
    match Asset::get("index.html") {
        Some(content) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(content.data.into_owned()))
            .expect("Failed to build response"),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from(
                "Admin UI not built. Run 'pnpm build' in admin-ui directory.",
            ))
            .expect("Failed to build response"),
    }
}

/// 根据文件类型返回合适的缓存策略
fn get_cache_control(path: &str) -> &'static str {
    if path.ends_with(".html") {
        // HTML 文件不缓存，确保用户获取最新版本
        "no-cache"
    } else if path.starts_with("assets/") {
        // assets/ 目录下的文件带有内容哈希，可以长期缓存
        "public, max-age=31536000, immutable"
    } else {
        // 其他文件（如 favicon）使用较短的缓存
        "public, max-age=3600"
    }
}

/// 判断是否为资源文件路径（有扩展名的文件）
fn is_asset_path(path: &str) -> bool {
    // 检查最后一个路径段是否包含扩展名
    path.rsplit('/')
        .next()
        .map(|filename| filename.contains('.'))
        .unwrap_or(false)
}

/// 随机背景图代理（服务端请求 + 图片代理，绕过 CORS 和防盗链）
async fn random_bg_handler() -> impl IntoResponse {
    // Lolicon API: 纯 R18 横图
    let url = "https://api.lolicon.app/setu/v2?r18=1&size=regular&excludeAI=true&num=1&aspectRatio=gt1.2";
    let client = match reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build() {
        Ok(c) => c,
        Err(_) => return Json(serde_json::json!({"url": null})).into_response(),
    };
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(_) => return Json(serde_json::json!({"url": null})).into_response(),
    };
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return Json(serde_json::json!({"url": null})).into_response(),
    };
    let img_url = body["data"][0]["urls"]["regular"].as_str().unwrap_or("");
    if img_url.is_empty() {
        Json(serde_json::json!({"url": null})).into_response()
    } else {
        let proxied = format!("/admin/api/bg-img?url={}", urlencoding::encode(img_url));
        Json(serde_json::json!({"url": proxied})).into_response()
    }
}

/// 图片代理（绕过 i.pixiv.re 防盗链，直接把图片 stream 给浏览器）
async fn bg_img_proxy_handler(uri: Uri) -> impl IntoResponse {
    let query = uri.query().unwrap_or("");
    let img_url = query.strip_prefix("url=").unwrap_or("");
    let img_url = match urlencoding::decode(img_url) {
        Ok(u) => u.into_owned(),
        Err(_) => return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("bad url"))
            .expect("Failed to build response"),
    };
    if !img_url.starts_with("https://") {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("invalid url"))
            .expect("Failed to build response");
    }
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build() {
        Ok(c) => c,
        Err(_) => return Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::from("client error"))
            .expect("Failed to build response"),
    };
    let resp = match client.get(&img_url)
        .header("referer", "https://www.pixiv.net/")
        .send().await {
        Ok(r) => r,
        Err(_) => return Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::from("fetch failed"))
            .expect("Failed to build response"),
    };
    let content_type = resp.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(_) => return Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::from("read failed"))
            .expect("Failed to build response"),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "public, max-age=3600")
        .body(Body::from(bytes.to_vec()))
        .expect("Failed to build response")
}
