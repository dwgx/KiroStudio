//! Admin UI 路由配置

use axum::{
    Json, Router,
    body::Body,
    http::{HeaderMap, Response, StatusCode, Uri, header},
    response::IntoResponse,
    routing::get,
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

    // ⚠️ MIME 白名单：池里的字节会连同 content_type 一起，经**匿名可达**的
    // `/admin/api/bg-cached?idx=N` 原样吐给浏览器。图片 URL 来自第三方 JSON 源
    // （api.lolicon.app）的响应，属于外部可控数据；若该源被劫持返回一个 text/html 的
    // URL，就能把 HTML 灌进池子，再由 bg-cached 在 /admin 同源下吐出 → XSS →
    // localStorage 里的 adminKey 泄露。故非图片 MIME 一律拒绝入池。
    // 与代理端点共用同一判定（single source of truth，防两处漂移）；
    // 但入池侧更严格：直接**拒绝**而非覆盖，绝不让非图片字节进常驻内存池。
    if sanitize_image_content_type(&content_type, img_url) != content_type {
        tracing::warn!(
            "背景图预取拒绝非图片 MIME {:?}（防止污染内存池后经匿名 bg-cached 造成 XSS）: {}",
            content_type,
            img_url
        );
        return None;
    }

    // ⚠️ 体积上限：与 bg_img_proxy_handler 的 MAX_BG_BYTES 对齐。
    // resp.bytes() 会把整个响应体读进内存且无上限，而池容量 BG_POOL_CAP=20 ——
    // 恶意/超大图可直接把常驻内存顶上去。先按 Content-Length 预检，再流式累计兜底
    //（防伪造/缺失 Content-Length 的无限流）。
    const MAX_BG_BYTES: usize = 10 * 1024 * 1024; // 10 MiB，与代理端点同口径
    if let Some(len) = resp.content_length() {
        if len as usize > MAX_BG_BYTES {
            tracing::warn!(
                "背景图预取过大（Content-Length={}），跳过: {}",
                len,
                img_url
            );
            return None;
        }
    }
    let mut resp = resp;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if buf.len() + chunk.len() > MAX_BG_BYTES {
                    tracing::warn!(
                        "背景图预取流超过 {} 字节上限，丢弃: {}",
                        MAX_BG_BYTES,
                        img_url
                    );
                    return None;
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("背景图下载失败（读取）: {} - {}", img_url, e);
                return None;
            }
        }
    }
    if buf.is_empty() {
        tracing::warn!("背景图下载为空: {}", img_url);
        return None;
    }
    Some(CachedBg {
        bytes: buf,
        content_type,
    })
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
    BgSource {
        name: "alcy",
        kind: BgKind::Direct,
        url: "https://t.alcy.cc/pc",
    },
    BgSource {
        name: "loliapi",
        kind: BgKind::Direct,
        url: "https://www.loliapi.com/acg/",
    },
];

/// R18 图源组:同为二次元/pixiv 题材,仅内容分级不同。lolicon r18=1 可靠,anosu 作冗余备份。
const R18_SOURCES: &[BgSource] = &[
    BgSource {
        name: "lolicon-r18",
        kind: BgKind::Json,
        url: "https://api.lolicon.app/setu/v2?r18=1&size=regular&excludeAI=true&num={num}&aspectRatio=gt1.2",
    },
    BgSource {
        name: "anosu-r18",
        kind: BgKind::Direct,
        url: "https://image.anosu.top/pixiv/direct?r18=1",
    },
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
async fn index_handler(headers: HeaderMap) -> impl IntoResponse {
    serve_index_with(&headers)
}

/// 由资源内容算出的强 ETag。
///
/// 用 rust-embed 编译期就算好的 sha256（`metadata.sha256_hash()`），运行时零成本——
/// 不需要每次请求重新哈希字节。取前 16 个十六进制字符：64 位碰撞空间对「同一文件的
/// 不同版本」这个用途绰绰有余，而完整的 64 字符会让每个响应头白占 48 字节。
///
/// 是**强** ETag（不带 `W/` 前缀）：字节完全一致才给同一个值，所以可以安全地用于
/// Range 请求等需要精确匹配的场景。
fn etag_of(content: &rust_embed::EmbeddedFile) -> String {
    let h = content.metadata.sha256_hash();
    let mut s = String::with_capacity(18);
    s.push('"');
    for b in h.iter().take(8) {
        s.push_str(&format!("{b:02x}"));
    }
    s.push('"');
    s
}

/// 请求带的 `If-None-Match` 是否与当前 ETag 匹配。
///
/// 只做「逐个候选精确比对」，不实现 `If-None-Match: *`——那个语义是用于写请求的
/// 前置条件（"只要资源存在就失败"），对 GET 静态文件没有意义，实现它反而会引入
/// 一个「客户端发 `*` 就永远拿 304」的错法。
fn etag_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|inm| {
            inm.split(',').any(|candidate| {
                let c = candidate.trim();
                // 弱比较：`W/"abc"` 与 `"abc"` 视为同一个实体。浏览器一般原样回
                // 我们发出的强 ETag，但中间代理可能把它弱化，那时仍应命中 304。
                c == etag || c.strip_prefix("W/").map(str::trim) == Some(etag)
            })
        })
        .unwrap_or(false)
}

/// 304 响应。**必须带上 ETag 与 Cache-Control**：RFC 9110 §15.4.5 要求 304 携带
/// 那些「若是 200 也会发」的缓存相关头，否则某些缓存会认为条目失效、下次仍整份重下，
/// 白白抵消掉 304 的收益。
fn not_modified(etag: &str, cache_control: &str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_MODIFIED)
        .header(header::ETAG, etag)
        .header(header::CACHE_CONTROL, cache_control)
        .body(Body::empty())
        .expect("Failed to build response")
}

/// 处理静态文件请求
async fn static_handler(uri: Uri, headers: HeaderMap) -> impl IntoResponse {
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
        let etag = etag_of(&content);

        // 命中 If-None-Match → 304，不回传字节。
        //
        // 对 `assets/` 下带内容哈希的文件，这条几乎永远走不到（immutable 让浏览器
        // 连请求都不发）；真正受益的是 index.html 那类 `no-cache` 的资源——它们
        // 每次导航都必须回源验证，有 ETag 才能用 304 代替整份重下。
        if etag_matches(&headers, &etag) {
            return not_modified(&etag, cache_control);
        }

        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .header(header::CACHE_CONTROL, cache_control)
            .header(header::ETAG, etag)
            .body(Body::from(content.data.into_owned()))
            .expect("Failed to build response");
    }

    // SPA fallback: 如果文件不存在且不是资源文件，返回 index.html
    if !is_asset_path(path) {
        return serve_index_with(&headers);
    }

    // 404
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::from("Not found"))
        .expect("Failed to build response")
}

/// 提供 index.html，支持 `If-None-Match` 条件请求。
///
/// index.html 走 `no-cache`（每次导航都必须回源验证），所以它是 ETag 收益最大的
/// 那个资源：验证通过时用一个空体 304 代替整份 HTML。
fn serve_index_with(headers: &HeaderMap) -> Response<Body> {
    match Asset::get("index.html") {
        Some(content) => {
            let etag = etag_of(&content);
            if etag_matches(headers, &etag) {
                return not_modified(&etag, "no-cache");
            }
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .header(header::CACHE_CONTROL, "no-cache")
                .header(header::ETAG, etag)
                .body(Body::from(content.data.into_owned()))
                .expect("Failed to build response")
        }
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
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("idx=")))
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
        // nosniff：本端点匿名可达，池内 MIME 虽已在 download_bg_bytes 入池时白名单过滤，
        // 这里再加一层防浏览器内容嗅探（纵深防御，避免任何遗漏路径变成 /admin 同源 XSS）。
        .header("x-content-type-options", "nosniff")
        // 命中的是内存字节，可让浏览器短期缓存。
        .header(header::CACHE_CONTROL, "public, max-age=3600")
        .body(Body::from(img.bytes.clone()))
        .expect("Failed to build response")
}

/// 把上游 `Content-Type` 收敛成"安全的图片 MIME"。
///
/// ⚠️ 为什么必须做：`/admin/api/bg-img` 与 `/admin/api/bg-cached` 都是**匿名可达**的
/// （`create_admin_ui_router` 整棵树没有任何鉴权 layer），且会把响应体原样回给浏览器。
/// 若把上游 Content-Type 原样透传：
///   1. 攻击者构造一个返回 `text/html` 的 URL 作为 `url=` 参数；
///   2. 浏览器在 **`/admin` 同源**下把它当 HTML 执行 → XSS；
///   3. 面板的 adminKey 明文存在 localStorage（全仓无 CSP）→ 完整接管管理面。
/// SSRF 与 10MiB 上限都已经防了，唯独不限制 MIME 等于没闭合这条链。
///
/// 策略：只放行 `image/*`（以及某些 CDN 对图片用的 `application/octet-stream`），
/// 其余**覆盖**为 `image/jpeg` 而不是拒绝——避免上游偶发返回怪异 MIME 时背景图直接加载失败
/// （背景图是纯装饰，可用性优先；关键是绝不能让浏览器把它当可执行文档看待）。
/// 调用方还必须配合 `X-Content-Type-Options: nosniff`，否则浏览器仍可能内容嗅探绕过。
fn sanitize_image_content_type(content_type: &str, img_url: &str) -> String {
    // 只看 MIME 主类型，忽略参数（如 `image/jpeg; charset=binary`）。
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if mime.starts_with("image/") || mime == "application/octet-stream" {
        content_type.to_string()
    } else {
        tracing::warn!(
            "背景图代理：上游返回非图片 MIME {:?}，已覆盖为 image/jpeg 防止 /admin 同源 XSS（目标 URL: {}）",
            content_type,
            img_url
        );
        "image/jpeg".to_string()
    }
}

/// 图片代理（绕过 i.pixiv.re 防盗链，直接把图片 stream 给浏览器）
async fn bg_img_proxy_handler(uri: Uri) -> impl IntoResponse {
    let query = uri.query().unwrap_or("");
    let img_url = query.strip_prefix("url=").unwrap_or("");
    let img_url = match urlencoding::decode(img_url) {
        Ok(u) => u.into_owned(),
        Err(_) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("bad url"))
                .expect("Failed to build response");
        }
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
    let resp = match client
        .get(&img_url)
        .header("referer", "https://www.pixiv.net/")
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("fetch failed"))
                .expect("Failed to build response");
        }
    };
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();

    let safe_content_type = sanitize_image_content_type(&content_type, &img_url);

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
            Err(_) => {
                return Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from("read failed"))
                    .expect("Failed to build response");
            }
        }
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, safe_content_type)
        // nosniff 是 MIME 白名单的必要补充：即使 Content-Type 已被收敛为 image/*，
        // 没有这个头时浏览器仍可能按内容嗅探（content sniffing）成 HTML 并执行。
        .header("x-content-type-options", "nosniff")
        .header(header::CACHE_CONTROL, "public, max-age=3600")
        .body(Body::from(buf))
        .expect("Failed to build response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_image_content_type_rejects_executable_mimes() {
        // ⭐XSS 回归(旧代码原样透传上游 Content-Type):/admin/api/bg-img 与 /admin/api/bg-cached
        // 都是**匿名可达**且原样回响应体。若把 text/html 透传出去,浏览器会在 /admin **同源**下
        // 执行它 → adminKey 存在 localStorage(全仓无 CSP) → 管理面完整接管。
        // 这里断言:一切可被浏览器当文档/脚本执行的 MIME 都必须被覆盖成 image/jpeg。
        for evil in [
            "text/html",
            "text/html; charset=utf-8",
            "TEXT/HTML", // 大小写不敏感
            "application/xhtml+xml",
            "image", // 缺斜杠,不算 image/*
            "text/javascript",
            "application/javascript",
            "application/pdf",
            "text/plain",
            "application/xml",
            "image_evil/html", // 前缀相似但不是 image/
            "",                // 缺失 Content-Type
        ] {
            assert_eq!(
                sanitize_image_content_type(evil, "https://example.invalid/x"),
                "image/jpeg",
                "非图片 MIME {evil:?} 必须被覆盖为 image/jpeg,否则匿名端点可在 /admin 同源 XSS"
            );
        }
    }

    #[test]
    fn test_sanitize_image_content_type_preserves_real_images() {
        // 真实图片 MIME 必须原样保留(含带参数的形态),否则浏览器可能不渲染或触发下载。
        for ok in [
            "image/jpeg",
            "image/png",
            "image/webp",
            "image/avif",
            "image/gif",
            "image/jpeg; charset=binary", // 带参数:只看主类型
            "IMAGE/PNG",                  // 大小写不敏感放行,且原样回传
            "application/octet-stream",   // 部分 CDN 对图片用它
        ] {
            assert_eq!(
                sanitize_image_content_type(ok, "https://example.invalid/x"),
                ok,
                "合法图片 MIME {ok:?} 应原样保留"
            );
        }
    }

    // ============ 缓存与条件请求 ============

    fn hm(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(k.clone(), v.parse().expect("合法头值"));
        }
        m
    }

    /// index.html 必须真的嵌进了二进制——否则下面所有 ETag 用例都在测一个
    /// 永远走 `None` 分支的空壳，全绿但毫无意义。
    #[test]
    fn index_html_is_actually_embedded() {
        assert!(
            Asset::get("index.html").is_some(),
            "index.html 未嵌入。ETag 相关用例会全部退化成对 404 分支的断言。"
        );
    }

    #[test]
    fn etag_is_stable_quoted_and_content_derived() {
        let index = Asset::get("index.html").expect("已由上一条用例保证存在");
        let a = etag_of(&index);
        let b = etag_of(&Asset::get("index.html").expect("同一文件"));

        assert_eq!(a, b, "同一内容必须给出同一个 ETag，否则 304 永远命中不了");
        assert!(
            a.starts_with('"') && a.ends_with('"'),
            "ETag 必须是带引号的 quoted-string（RFC 9110 §8.8.3），实际是 {a}"
        );
        assert!(
            !a.starts_with("W/"),
            "这里发的是强 ETag（字节完全一致才同值），不该带弱前缀"
        );
        // 16 个十六进制字符 + 两个引号。
        assert_eq!(a.len(), 18, "ETag 长度变了：{a}");
    }

    /// 不同文件必须给出不同 ETag。若哪天 `etag_of` 被改成返回常量或版本号，
    /// 表现就是「改了 JS 但浏览器一直拿 304」——线上最难查的那类问题。
    #[test]
    fn different_files_get_different_etags() {
        let mut seen = std::collections::HashMap::new();
        for f in Asset::iter() {
            let name = f.to_string();
            let content = Asset::get(&name).expect("iter 给出的名字必然存在");
            let tag = etag_of(&content);
            if let Some(other) = seen.insert(tag.clone(), name.clone()) {
                // 两个不同路径同 ETag 只有一种正当情形：字节完全相同。
                let a = Asset::get(&name).expect("存在").data.into_owned();
                let b = Asset::get(&other).expect("存在").data.into_owned();
                assert_eq!(
                    a, b,
                    "{name} 与 {other} 内容不同却共用 ETag {tag}——\
                     浏览器会对其中一个永久返回陈旧内容"
                );
            }
        }
        assert!(seen.len() > 1, "嵌入资源太少，本用例没有区分力");
    }

    #[test]
    fn if_none_match_matches_exactly_and_weakly() {
        let etag = "\"abc123\"";

        assert!(etag_matches(&hm(&[(header::IF_NONE_MATCH, etag)]), etag));
        // 中间代理可能把强 ETag 弱化，那时仍应命中 304。
        assert!(etag_matches(
            &hm(&[(header::IF_NONE_MATCH, "W/\"abc123\"")]),
            etag
        ));
        // 多候选：命中其中任意一个即可。
        assert!(etag_matches(
            &hm(&[(header::IF_NONE_MATCH, "\"zzz\", \"abc123\", \"yyy\"")]),
            etag
        ));

        assert!(!etag_matches(
            &hm(&[(header::IF_NONE_MATCH, "\"nope\"")]),
            etag
        ));
        assert!(!etag_matches(&HeaderMap::new(), etag), "没带头 → 不匹配");
        // `*` 是写请求的前置条件语义，对 GET 静态文件实现它会变成
        // 「客户端发 * 就永远拿 304」，那是错的。
        assert!(
            !etag_matches(&hm(&[(header::IF_NONE_MATCH, "*")]), etag),
            "`*` 不该被当成匹配"
        );
    }

    /// 304 必须携带 ETag 与 Cache-Control（RFC 9110 §15.4.5）。
    /// 少了它们，某些缓存会认为条目失效、下次仍整份重下，抵消掉 304 的收益。
    #[test]
    fn not_modified_carries_the_caching_headers() {
        let res = not_modified("\"abc\"", "no-cache");
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            res.headers()
                .get(header::ETAG)
                .and_then(|v| v.to_str().ok()),
            Some("\"abc\"")
        );
        assert_eq!(
            res.headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-cache")
        );
    }

    #[test]
    fn serve_index_returns_304_when_etag_matches_and_200_otherwise() {
        let index = Asset::get("index.html").expect("已嵌入");
        let etag = etag_of(&index);

        let fresh = serve_index_with(&HeaderMap::new());
        assert_eq!(fresh.status(), StatusCode::OK);
        assert_eq!(
            fresh
                .headers()
                .get(header::ETAG)
                .and_then(|v| v.to_str().ok()),
            Some(etag.as_str()),
            "200 必须带 ETag，否则客户端下次无从发起条件请求"
        );

        let revalidated = serve_index_with(&hm(&[(header::IF_NONE_MATCH, &etag)]));
        assert_eq!(
            revalidated.status(),
            StatusCode::NOT_MODIFIED,
            "带上刚拿到的 ETag 必须换来 304"
        );

        // 陈旧 ETag（发过新版）必须拿到完整的 200，不能误命中。
        let stale = serve_index_with(&hm(&[(header::IF_NONE_MATCH, "\"0000000000000000\"")]));
        assert_eq!(stale.status(), StatusCode::OK);
    }

    /// 缓存策略与资源命名方式必须配对：`immutable, max-age=1年` 只有在文件名
    /// 带内容哈希时才是安全的。若哪天 vite 配置改成不带哈希的固定名，这条会红——
    /// 否则表现是发新版后用户一年内都拿不到更新。
    #[test]
    fn year_long_immutable_is_only_used_for_content_hashed_assets() {
        let re_hash = |name: &str| {
            // vite 的形态是 `<base>-<hash>.<ext>`，hash 为 8 位 base64url 字符。
            //
            // ⚠️ 不能用 `rsplit_once('-')` 找哈希：base64url 的字符集**包含 `-`**，
            // 真实产物里就有 `overview-page-C-63v8Th.css` 这种——哈希本身是
            // `C-63v8Th`，从最后一个 `-` 切会切进哈希内部，得到 6 个字符然后误判
            // 成「没有哈希」。第一版就是这么写的，被本用例抓住。
            //
            // 正确判据是**定宽后缀**：stem 的最后 8 个字符都是 base64url 合法字符，
            // 且它们前面紧挨着一个 `-`。
            let stem = match std::path::Path::new(name)
                .file_stem()
                .and_then(|s| s.to_str())
            {
                Some(s) => s,
                None => return false,
            };
            let chars: Vec<char> = stem.chars().collect();
            // 至少 `-` + 8 位哈希。
            if chars.len() < 9 {
                return false;
            }
            let sep = chars[chars.len() - 9];
            let hash = &chars[chars.len() - 8..];
            sep == '-'
                && hash
                    .iter()
                    .all(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        };

        // 对照组：确认这个判据既能认出真哈希、也能拒掉无哈希的名字。否则一个
        // 恒返回 true 的判据会让下面的循环全绿而毫无意义。
        assert!(
            re_hash("assets/overview-page-C-63v8Th.css"),
            "哈希含 `-` 的真实产物必须被认出"
        );
        assert!(re_hash("assets/index-C9ZH2VGf.js"));
        assert!(!re_hash("vite.svg"), "无哈希的名字必须被拒");
        assert!(!re_hash("assets/style.css"));

        let mut checked = 0;
        for f in Asset::iter() {
            let name = f.to_string();
            let cc = get_cache_control(&name);
            if cc.contains("immutable") {
                assert!(
                    re_hash(&name),
                    "{name} 被标成 immutable 缓存一年，但文件名里没有内容哈希。\
                     改这个文件后用户最长一年拿不到新版本。"
                );
                checked += 1;
            }
        }
        assert!(checked > 0, "没有任何 immutable 资源，本用例没有区分力");
    }

    #[test]
    fn html_is_never_cached_immutably() {
        for f in Asset::iter() {
            let name = f.to_string();
            if name.ends_with(".html") {
                let cc = get_cache_control(&name);
                assert!(
                    !cc.contains("immutable") && !cc.contains("max-age=31536000"),
                    "{name} 是 HTML 却被长期缓存：发新版后浏览器会拿着旧壳打新 API"
                );
                assert!(cc.contains("no-cache"), "{name} 必须每次回源验证");
            }
        }
    }
}
