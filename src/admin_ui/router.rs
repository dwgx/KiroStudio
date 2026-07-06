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
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

/// 嵌入前端构建产物
#[derive(Embed)]
#[folder = "admin-ui/dist"]
struct Asset;

// ============ 登录页背景图预取池 ============
//
// 设计目标：登录页打开时秒回一张背景图，不再在请求热路径上实时打 lolicon。
// - 服务端在内存里预存一批图片「字节」（已绕好防盗链下载完毕）。
// - 后台定时任务周期性补一批，池子设上限、超限丢最老的，内存有界。
// - /api/random-bg 只从池子随机挑一张，返回指向 /api/bg-cached 的秒发 URL。

/// 内存池单张缓存图：图片原始字节 + Content-Type。
struct CachedBg {
    bytes: Vec<u8>,
    content_type: String,
}

/// 背景图内存池。用 `Mutex<Vec<..>>` 保护，读写都极短，无异步持锁。
struct BgPool {
    imgs: Mutex<Vec<CachedBg>>,
}

/// 池子容量上限：超过则从头丢弃最老的，避免内存无限增长。
const BG_POOL_CAP: usize = 20;
/// 每轮后台补充拉取的张数。
const BG_FETCH_BATCH: usize = 6;
/// 后台补充间隔（秒）：12 分钟。
const BG_REFILL_INTERVAL_SECS: u64 = 12 * 60;

/// 全局背景池（首次访问时惰性初始化）。
static BG_POOL: OnceLock<BgPool> = OnceLock::new();

/// 登录页背景是否启用（`login_background_enabled` 的运行时镜像）。
/// 默认 1=启用；由 main 在启动时按配置写入，支持后续被 update_config 立即改写。
static LOGIN_BG_ENABLED: AtomicU64 = AtomicU64::new(1);

fn bg_pool() -> &'static BgPool {
    BG_POOL.get_or_init(|| BgPool {
        imgs: Mutex::new(Vec::new()),
    })
}

/// 设置登录页背景开关（供 main 启动接线 / update_config 立即生效调用）。
pub fn set_login_background_enabled(enabled: bool) {
    LOGIN_BG_ENABLED.store(if enabled { 1 } else { 0 }, Ordering::Relaxed);
}

fn login_background_enabled() -> bool {
    LOGIN_BG_ENABLED.load(Ordering::Relaxed) != 0
}

/// 背景图内存池统计：返回 (张数, 总字节数)。供 admin 存储统计端点展示。
///
/// 背景图仅内存缓存（无落盘），故这里统计的是常驻内存占用；池上限 [`BG_POOL_CAP`]。
pub fn bg_pool_stats() -> (usize, u64) {
    let pool = bg_pool();
    let guard = match pool.imgs.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let count = guard.len();
    let bytes: u64 = guard.iter().map(|c| c.bytes.len() as u64).sum();
    (count, bytes)
}

/// 清空背景图内存池，返回清空前的张数。供 admin 存储清理端点调用。
///
/// 仅释放内存缓存；下一轮后台预取或实时请求会重新填充，无副作用。
pub fn clear_bg_pool() -> usize {
    let pool = bg_pool();
    let mut guard = match pool.imgs.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let n = guard.len();
    guard.clear();
    n
}

/// 构造一个下载用 reqwest 客户端（带超时，失败返回 None 由调用方容错）。
fn bg_http_client(timeout_secs: u64) -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .ok()
}

/// 下载单张图片字节（复用防盗链 referer 逻辑）。失败返回 None（warn 不 panic）。
async fn download_bg_bytes(client: &reqwest::Client, img_url: &str) -> Option<CachedBg> {
    if !img_url.starts_with("https://") {
        return None;
    }
    let resp = match client
        .get(img_url)
        .header("referer", "https://www.pixiv.net/")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("背景图下载失败（请求）: {} - {}", img_url, e);
            return None;
        }
    };
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();
    match resp.bytes().await {
        Ok(b) if !b.is_empty() => Some(CachedBg {
            bytes: b.to_vec(),
            content_type,
        }),
        Ok(_) => {
            tracing::warn!("背景图下载为空: {}", img_url);
            None
        }
        Err(e) => {
            tracing::warn!("背景图下载失败（读取）: {} - {}", img_url, e);
            None
        }
    }
}

/// 拉取一批背景图并存入内存池：
/// 1) 向 lolicon 请求一批图片 URL；2) 逐张下载字节；3) 存入池并按上限丢老。
async fn fetch_bg_batch() {
    // 关闭时不拉（即便被调度到也直接返回，池子保持原状不受影响）。
    if !login_background_enabled() {
        return;
    }

    let client = match bg_http_client(20) {
        Some(c) => c,
        None => {
            tracing::warn!("背景图预取：构造 HTTP 客户端失败，跳过本轮");
            return;
        }
    };

    // 一次向 lolicon 要 BG_FETCH_BATCH 张（纯 R18 横图，用户自用）。
    let api = format!(
        "https://api.lolicon.app/setu/v2?r18=1&size=regular&excludeAI=true&num={}&aspectRatio=gt1.2",
        BG_FETCH_BATCH
    );
    let body: serde_json::Value = match client.get(&api).send().await {
        Ok(r) => match r.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("背景图预取：解析 lolicon 响应失败: {}", e);
                return;
            }
        },
        Err(e) => {
            tracing::warn!("背景图预取：请求 lolicon 失败: {}", e);
            return;
        }
    };

    let urls: Vec<String> = match body["data"].as_array() {
        Some(arr) => arr
            .iter()
            .filter_map(|it| it["urls"]["regular"].as_str())
            .map(|s| s.to_string())
            .collect(),
        None => Vec::new(),
    };
    if urls.is_empty() {
        tracing::warn!("背景图预取：lolicon 未返回可用 URL");
        return;
    }

    // 逐张下载（串行，避免瞬时并发打爆图源），只把成功的存进池。
    let mut fetched = 0usize;
    for u in urls {
        if let Some(img) = download_bg_bytes(&client, &u).await {
            let pool = bg_pool();
            let mut guard = match pool.imgs.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(), // 锁中毒也继续用，背景图不涉及一致性风险
            };
            guard.push(img);
            // 超过上限：从头丢最老的，保持有界。
            while guard.len() > BG_POOL_CAP {
                guard.remove(0);
            }
            fetched += 1;
        }
    }
    if fetched > 0 {
        let total = bg_pool()
            .imgs
            .lock()
            .map(|g| g.len())
            .unwrap_or(0);
        tracing::info!("背景图预取：本轮新增 {} 张，池内共 {} 张", fetched, total);
    }
}

/// 启动登录页背景图预取后台任务。
///
/// 由 main 在启动时接线：
/// - `enabled` 为当前 `login_background_enabled` 配置，写入运行时镜像。
/// - 启用时立即先拉一批（不等第一个 interval），随后每 12 分钟补一批。
/// - 关闭时不 spawn 任务；即使后续被开启，也可由 update_config 改写镜像后靠下次
///   请求回退到实时拉逻辑兜底（保持最小侵入，不做热重启任务）。
pub fn spawn_bg_prefetch(enabled: bool) {
    set_login_background_enabled(enabled);
    if !enabled {
        tracing::info!("登录页背景预取未启用（login_background_enabled=false）");
        return;
    }
    tokio::spawn(async move {
        // 启动即先拉一批，避免开机后头一批用户拿到空池。
        fetch_bg_batch().await;
        let mut ticker = tokio::time::interval(Duration::from_secs(BG_REFILL_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // 跳过 interval 立即触发的第一次 tick（上面已经主动拉过一批）。
        ticker.tick().await;
        loop {
            ticker.tick().await;
            fetch_bg_batch().await;
        }
    });
    tracing::info!(
        "登录页背景预取已启用：启动即拉一批，之后每 {} 秒补 {} 张（池上限 {}）",
        BG_REFILL_INTERVAL_SECS,
        BG_FETCH_BATCH,
        BG_POOL_CAP
    );
}

/// 创建 Admin UI 路由
pub fn create_admin_ui_router() -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/api/random-bg", get(random_bg_handler))
        .route("/api/bg-cached", get(bg_cached_handler))
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

/// 随机背景图：从服务端内存池随机挑一张已缓存的图，返回秒发 URL。
///
/// 正常路径完全不打外网：直接给前端一个 `/admin/api/bg-cached?idx=N` 链接，
/// 前端 fetch 到的是内存里的字节，秒回。
/// - 背景关闭：直接返回 `{"url": null}`（前端有渐变兜底）。
/// - 池为空（启动瞬间还没拉到）：回退到实时拉一张的旧逻辑兜底。
async fn random_bg_handler() -> impl IntoResponse {
    // 开关关闭：不返回任何图，前端用纯渐变。
    if !login_background_enabled() {
        return Json(serde_json::json!({"url": null})).into_response();
    }

    // 优先走内存池：随机挑一个索引，指向秒发端点。
    let len = bg_pool().imgs.lock().map(|g| g.len()).unwrap_or(0);
    if len > 0 {
        let idx = fastrand::usize(..len);
        let url = format!("/admin/api/bg-cached?idx={}", idx);
        return Json(serde_json::json!({"url": url})).into_response();
    }

    // 池空兜底：实时拉一张（旧逻辑），下载交给现有 bg-img 代理。
    // 这条路径只在服务刚启动、后台首批还没到位时短暂出现。
    let client = match bg_http_client(10) {
        Some(c) => c,
        None => return Json(serde_json::json!({"url": null})).into_response(),
    };
    let api = "https://api.lolicon.app/setu/v2?r18=1&size=regular&excludeAI=true&num=1&aspectRatio=gt1.2";
    let body: serde_json::Value = match client.get(api).send().await {
        Ok(r) => match r.json().await {
            Ok(v) => v,
            Err(_) => return Json(serde_json::json!({"url": null})).into_response(),
        },
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

/// 秒发端点：从内存池按索引取一张缓存图的字节直接返回。
///
/// 索引可能因池子补充/丢老而漂移，这里对当前长度取模兜底——所有图等价，
/// 取到相邻的另一张同样可用，不会 404。池空则 404（前端回退渐变）。
async fn bg_cached_handler(uri: Uri) -> impl IntoResponse {
    // 解析 idx（缺省 0）。
    let idx: usize = uri
        .query()
        .and_then(|q| {
            q.split('&')
                .find_map(|kv| kv.strip_prefix("idx="))
        })
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let pool = bg_pool();
    let guard = match pool.imgs.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if guard.is_empty() {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("no cached background"))
            .expect("Failed to build response");
    }
    let real = idx % guard.len();
    let img = &guard[real];
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, img.content_type.clone())
        // 命中的是内存字节，可让浏览器短期缓存。
        .header(header::CACHE_CONTROL, "public, max-age=3600")
        .body(Body::from(img.bytes.clone()))
        .expect("Failed to build response")
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
    // SSRF 防护：本端点匿名可达且原样回响应体，必须校验目标不落内网/保留段，
    // 并把 DNS 固定到已校验 IP、禁重定向，防止 rebinding / 302 跳内网绕过。
    let client = match crate::common::ssrf::build_guarded_client(
        &img_url,
        std::time::Duration::from_secs(15),
        &["https"],
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("背景图代理拒绝目标 URL: {} - {}", img_url, e);
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("invalid url"))
                .expect("Failed to build response");
        }
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

    // DoS 防护：本端点匿名可达且把响应体读进内存，必须限制最大字节数，
    // 否则攻击者可把 url 指向超大文件/无限流一次撑爆内存。
    const MAX_BG_BYTES: usize = 10 * 1024 * 1024; // 10 MiB
    // 先按 Content-Length 预检（有则据此快速拒绝）
    if let Some(len) = resp.content_length() {
        if len as usize > MAX_BG_BYTES {
            tracing::warn!("背景图过大（Content-Length={}），拒绝", len);
            return Response::builder()
                .status(StatusCode::PAYLOAD_TOO_LARGE)
                .body(Body::from("image too large"))
                .expect("Failed to build response");
        }
    }
    // 流式累计读取，超限即中断（防伪造/缺失 Content-Length 的无限流）
    let mut resp = resp;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if buf.len() + chunk.len() > MAX_BG_BYTES {
                    tracing::warn!("背景图流超过 {} 字节上限，中断", MAX_BG_BYTES);
                    return Response::builder()
                        .status(StatusCode::PAYLOAD_TOO_LARGE)
                        .body(Body::from("image too large"))
                        .expect("Failed to build response");
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("read failed"))
                .expect("Failed to build response"),
        }
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "public, max-age=3600")
        .body(Body::from(buf))
        .expect("Failed to build response")
}
