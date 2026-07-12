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

/// 登录页背景是否走 R18 图源（`login_background_r18` 的运行时镜像）。
/// **默认 0=关闭（r18=0 全年龄）**；由 main 在启动时按配置写入，支持后续被 update_config 立即改写。
/// 下一轮后台预取 / 池空实时兜底拉取时读取此镜像决定 r18 参数。
static LOGIN_BG_R18: AtomicU64 = AtomicU64::new(0);

/// 背景池「代次」(generation/epoch)。clear_bg_pool 与 R18/开关变更时递增。
///
/// ⭐修复"关 R18 后仍可能服务到 R18 图"的在途竞态:fetch_bg_batch 是长时任务(多张×最长12s),
/// 起点捕获当时的 epoch+r18;若下载途中用户关了 R18/清了池(epoch 变),在途批次下载完的**旧参数
/// 图**若照旧 push 回池,就会把已清的 R18 图塞回刚清空的池 → random-bg 命中即返回本应清除的 R18 图。
/// push 前校验 epoch 未变 + r18 未变,不符则丢弃该图并中止本批,拦住陈旧写入。
static BG_EPOCH: AtomicU64 = AtomicU64::new(0);

/// 递增背景池代次,使所有在途 fetch 批次的后续 push 失效(它们捕获的是旧 epoch)。
fn bump_bg_epoch() {
    BG_EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// 读当前代次。
fn bg_epoch() -> u64 {
    BG_EPOCH.load(Ordering::Relaxed)
}

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

/// 设置登录页背景 R18 开关（供 main 启动接线 / update_config 立即生效调用）。
///
/// 值真正变化时递增 BG_EPOCH:使所有在途 fetch 批次(捕获旧 epoch/旧 r18)的后续 push 失效,
/// 防止"关 R18 瞬间在途批次把旧 R18 图 push 回池"。切换 R18 通常配合 clear_bg_pool 清池。
pub fn set_login_background_r18(r18: bool) {
    let new = if r18 { 1 } else { 0 };
    let old = LOGIN_BG_R18.swap(new, Ordering::Relaxed);
    if old != new {
        bump_bg_epoch();
    }
}

fn login_background_r18() -> bool {
    LOGIN_BG_R18.load(Ordering::Relaxed) != 0
}

/// 按当前 R18 开关取 lolicon 的 r18 参数值（开=1，关=0=全年龄）。
fn r18_param() -> u8 {
    if login_background_r18() { 1 } else { 0 }
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
    // 先递增代次:让此刻所有在途 fetch 批次的后续 push 失效,避免它们下载完的(可能是旧 R18)
    // 图在 clear 之后又塞回刚清空的池(在途竞态)。
    bump_bg_epoch();
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

/// 背景图源类型。
/// - `Json`：GET 返回 JSON，需按 lolicon 格式解析出图片 URL 列表再逐张下载(`{num}` 占位=本轮还需张数)。
/// - `Direct`：GET 直接返回图片字节(含 302 跳转到 CDN)，每 GET 一次得一张，要 N 张就 GET N 次。
#[derive(Clone, Copy)]
enum BgKind {
    Json,
    Direct,
}

/// 一个背景图源。题材统一为二次元/pixiv 插画横图,高质量。
#[derive(Clone, Copy)]
struct BgSource {
    name: &'static str,
    kind: BgKind,
    url: &'static str,
}

/// 非 R18(全年龄)图源组:都是二次元/pixiv 高质量横图。多源冗余——某源不可达/失败自动换下一个。
const NON_R18_SOURCES: &[BgSource] = &[
    BgSource {
        name: "lolicon",
        kind: BgKind::Json,
        url: "https://api.lolicon.app/setu/v2?r18=0&size=regular&excludeAI=true&num={num}&aspectRatio=gt1.2",
    },
    BgSource { name: "alcy", kind: BgKind::Direct, url: "https://t.alcy.cc/pc" },
    BgSource { name: "loliapi", kind: BgKind::Direct, url: "https://www.loliapi.com/acg/" },
];

/// R18 图源组:同为二次元/pixiv 题材,仅内容分级不同。lolicon r18=1 可靠,anosu 作冗余备份。
const R18_SOURCES: &[BgSource] = &[
    BgSource {
        name: "lolicon-r18",
        kind: BgKind::Json,
        url: "https://api.lolicon.app/setu/v2?r18=1&size=regular&excludeAI=true&num={num}&aspectRatio=gt1.2",
    },
    BgSource { name: "anosu-r18", kind: BgKind::Direct, url: "https://image.anosu.top/pixiv/direct?r18=1" },
];

/// 把一张下载好的图推进内存池,并按上限丢弃最老的(有界)。
///
/// `batch_epoch` = 本 fetch 批次起点捕获的代次。push 前校验它仍等于当前代次:若期间发生过
/// clear_bg_pool / R18 切换(代次已 bump),说明本批下载的是**陈旧参数图**,丢弃不入池并返回 false
/// (调用方据此中止本批),防止旧 R18 图被塞回已清空的池。
fn push_bg_to_pool(img: CachedBg, batch_epoch: u64) -> bool {
    if bg_epoch() != batch_epoch {
        tracing::debug!("背景图预取:代次已变(池被清/R18切换),丢弃在途陈旧图并中止本批");
        return false;
    }
    let pool = bg_pool();
    let mut guard = match pool.imgs.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(), // 锁中毒也继续用,背景图不涉及一致性风险
    };
    guard.push(img);
    while guard.len() > BG_POOL_CAP {
        guard.remove(0);
    }
    true
}

/// 从一个 Json 源(lolicon 格式)拿图片 URL 列表并逐张下载,返回成功存池的张数。
/// `batch_epoch` 透传给 push_bg_to_pool 做代次校验;某张 push 被拒(代次变)即停止本源下载。
async fn fetch_from_json_source(client: &reqwest::Client, url: &str, batch_epoch: u64) -> usize {
    let body: serde_json::Value = match client.get(url).send().await {
        Ok(r) => match r.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("背景图预取:解析 JSON 源响应失败: {}", e);
                return 0;
            }
        },
        Err(e) => {
            tracing::warn!("背景图预取:请求 JSON 源失败: {}", e);
            return 0;
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
    let mut fetched = 0usize;
    for u in urls {
        if let Some(img) = download_bg_bytes(client, &u).await {
            // 代次已变则 push 被拒,停止本源(在途陈旧图不入池)。
            if !push_bg_to_pool(img, batch_epoch) {
                break;
            }
            fetched += 1;
        }
    }
    fetched
}

/// 拉取一批背景图并存入内存池(多源 + R18 分流 + failover):
/// 按当前 R18 开关选源组,打乱顺序依次尝试,累计够 BG_FETCH_BATCH 张即停;某源不可达/失败自动换下一个。
async fn fetch_bg_batch() {
    // 关闭时不拉（即便被调度到也直接返回，池子保持原状不受影响）。
    if !login_background_enabled() {
        return;
    }

    // 单次下载超时 12s:够下大图(loliapi ~2.6MB),又不至于让死源(如本机不可达的 anosu)
    // 每次都干等 20s 拖垮整批 failover。
    let client = match bg_http_client(12) {
        Some(c) => c,
        None => {
            tracing::warn!("背景图预取：构造 HTTP 客户端失败，跳过本轮");
            return;
        }
    };

    // 捕获本批起点的代次:下载途中若 R18 切换/池被清(代次 bump),push 会被拒并中止本批,
    // 防止旧参数图被塞回池(关 R18 后仍服务到 R18 图的在途竞态修复)。
    let batch_epoch = bg_epoch();

    // 按 R18 开关选源组。多源冗余:打乱顺序依次尝试,累计够 BG_FETCH_BATCH 张即停;
    // 某源不可达/失败(超时/DNS/403)自动跳到下一个源——保证换环境(本机/服务器)都有可用源。
    let sources = if login_background_r18() {
        R18_SOURCES
    } else {
        NON_R18_SOURCES
    };
    // 打乱源顺序,避免总是死磕第一个源(也让不同源的图混着进池,更多样)。
    let mut order: Vec<usize> = (0..sources.len()).collect();
    for i in (1..order.len()).rev() {
        order.swap(i, fastrand::usize(..=i));
    }

    let mut fetched = 0usize;
    for idx in order {
        if fetched >= BG_FETCH_BATCH {
            break;
        }
        // 代次已变(R18 切换/清池)→ 中止本批,不再拉旧参数图。
        if bg_epoch() != batch_epoch {
            tracing::debug!("背景图预取:代次已变,中止本批 failover");
            return;
        }
        let need = BG_FETCH_BATCH - fetched;
        let src = &sources[idx];
        let got = match src.kind {
            BgKind::Json => {
                let url = src.url.replace("{num}", &need.to_string());
                fetch_from_json_source(&client, &url, batch_epoch).await
            }
            BgKind::Direct => {
                // Direct 源每 GET 一次得一张,要 need 张就 GET need 次(串行,不打爆图源)。
                // 连续失败 2 次即判定该源当前不可用,早停换下一个源——避免对死源(如不可达的
                // anosu)硬试满 need 次、每次超时拖垮整批 failover(R18 组池填不上的根因)。
                let mut n = 0usize;
                let mut consecutive_fail = 0usize;
                for _ in 0..need {
                    if let Some(img) = download_bg_bytes(&client, src.url).await {
                        // 代次已变则 push 被拒,停止本源(在途陈旧图不入池)。
                        if !push_bg_to_pool(img, batch_epoch) {
                            break;
                        }
                        n += 1;
                        consecutive_fail = 0;
                    } else {
                        consecutive_fail += 1;
                        if consecutive_fail >= 2 {
                            tracing::debug!("背景图预取:源 [{}] 连续失败,早停换下一个源", src.name);
                            break;
                        }
                    }
                }
                n
            }
        };
        if got > 0 {
            tracing::debug!("背景图预取:源 [{}] 贡献 {} 张", src.name, got);
        }
        fetched += got;
    }

    if fetched > 0 {
        let total = bg_pool().imgs.lock().map(|g| g.len()).unwrap_or(0);
        tracing::info!(
            "背景图预取:本轮新增 {} 张(R18={}),池内共 {} 张",
            fetched,
            login_background_r18(),
            total
        );
    } else {
        tracing::warn!("背景图预取:本轮所有图源均未拉到图(将靠下轮/实时兜底重试)");
    }
}

/// 启动登录页背景图预取后台任务。
///
/// 由 main 在启动时接线：
/// 启动一个**常驻**的背景图预取循环(main 启动调一次)。
///
/// - `enabled` 仅播种运行时镜像的初值;**循环无条件常驻**,不再"关闭时不 spawn"。
/// - 循环体 `fetch_bg_batch` 内已有 `if !login_background_enabled() return` 门:关闭时每轮空转
///   跳过(开销极小),开启时自动拉图填池。这样即便启动时 enabled=false、之后 admin 开启,
///   预取循环也一直在、下一轮(及开启时的即时 [`trigger_bg_refill`])就能把池填满。
/// - ⭐修复根因:旧实现"关闭时不 spawn、开启后不 respawn"→ 启动 false 再开启则预取循环永不启动、
///   池永远空、每次走单张实时兜底(慢/偶尔失败),表现为"第一次没图、关开偶尔显示一次、再刷新又没"。
pub fn spawn_bg_prefetch(enabled: bool) {
    set_login_background_enabled(enabled);
    tokio::spawn(async move {
        // 启动即先尝试拉一批(enabled=false 时 fetch 内部 gate 直接 return,不浪费)。
        fetch_bg_batch().await;
        let mut ticker = tokio::time::interval(Duration::from_secs(BG_REFILL_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await; // 跳过 interval 立即触发的首 tick(上面已主动拉过)
        loop {
            ticker.tick().await;
            fetch_bg_batch().await; // 内部按 login_background_enabled 门控:关则空转、开则补池
        }
    });
    tracing::info!(
        "登录页背景预取循环已常驻(初始 enabled={}):开则每 {} 秒补 {} 张(池上限 {}),关则空转",
        enabled,
        BG_REFILL_INTERVAL_SECS,
        BG_FETCH_BATCH,
        BG_POOL_CAP
    );
}

/// 立即触发一次背景图补池(供 admin 把 login_background_enabled 开启时调用,不用等常驻循环的下一轮
/// 12 分钟 tick)。`fetch_bg_batch` 内部有 enabled 门,关闭态调用会空转直接返回(幂等安全)。
pub fn trigger_bg_refill() {
    tokio::spawn(async move {
        fetch_bg_batch().await;
    });
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
    let api = format!(
        "https://api.lolicon.app/setu/v2?r18={}&size=regular&excludeAI=true&num=1&aspectRatio=gt1.2",
        r18_param()
    );
    let body: serde_json::Value = match client.get(&api).send().await {
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
