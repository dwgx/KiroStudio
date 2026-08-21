//! GitHub 版本检查 + 二进制 OTA 自更新
//!
//! 逻辑范式移植自 WindsurfAPI（AIClient-2-API/src/ui-modules/update-api.js），取其精华：
//! - **多镜像回退**：检查/下载都按 gh-proxy 系列镜像 + 直连逐个尝试，首个成功即用（国内拉 GitHub 关键）。
//! - **两段式**：`check_for_updates`（只检查，读本地版本 + 拉 tags + semver 比较）与
//!   `perform_update`（执行下载替换）分离，前端一个按钮检查、一个升级。
//!
//! KiroStudio 是**单静态二进制**，比 WindsurfAPI 拉源码简单：直接换 `kirostudio` 可执行文件。
//! 相较 WindsurfAPI 额外加了一道**它没有的安全线**：下载的二进制必须过 **sha256 校验**——
//! 换的是可执行文件，不校验 = 镜像/中间人替换二进制即 RCE。这是唯一不可省的安全点。
//!
//! 应用更新（平台差异，见 `perform_update`）：
//! - **Linux/macOS**：写 `<exe>.new` → 备份 `<exe>.bak` → 原子 rename 覆盖运行中的 exe →
//!   复用 `AdminService::restart_service`（exit(0) 交给 systemd `Restart=always` 拉起新二进制）。
//! - **Windows**：不能覆盖运行中的 exe，改用「rename 旧 exe→.bak（备份+腾路径）→ rename
//!   .new→原路径」，重启由 start.bat/run.bat 的监督循环按原路径拉起新二进制（exit(0) 即重拉）。
//!
//! OTA 资产按运行平台 **OS × 架构** 自动选择（`ASSET_BIN`）：Windows 下 `kirostudio-windows-x86_64.exe`，
//! Linux 下 `kirostudio-linux-x86_64`，macOS 下 `kirostudio-macos-{x86_64,aarch64}`。
//! 下错平台/架构的二进制即便 sha256 自洽也无法运行（覆盖后服务当场死亡），故必须精确匹配；
//! 未适配的组合在编译期直接 `compile_error!`，绝不静默回退到某个默认资产名。

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

/// 上游仓库（owner/repo）。发布产物见 .github/workflows/release.yml，每个平台一份二进制 + 同名 .sha256：
/// `kirostudio-linux-x86_64` / `kirostudio-macos-x86_64` /
/// `kirostudio-macos-aarch64` / `kirostudio-windows-x86_64.exe`。
/// ⚠️ 这份清单必须与 `ASSET_BIN` 的 cfg 分支、release.yml 的产出**三方保持一致**，
/// 少一个平台就意味着该平台的用户点 OTA 会 404（好）或下到错误资产（灾难）。
const DEFAULT_GITHUB_REPO: &str = "dwgx/KiroStudio";

/// OTA 目标仓库（owner/repo）。默认 [`DEFAULT_GITHUB_REPO`]，可用环境变量
/// `KIROSTUDIO_UPDATE_REPO` 覆盖，用于把 OTA 指向自建/私有仓库。
///
/// 为什么走环境变量而不是 config.json：OTA 是自更新路径，配置热重载与"正在替换自己的
/// 二进制"这两件事叠在一起风险高；环境变量在进程启动时固定，语义最简单。
/// 且 systemd 的 `Environment=` / `EnvironmentFile=` 天然适配（见部署单元）。
fn github_repo() -> String {
    std::env::var("KIROSTUDIO_UPDATE_REPO")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_GITHUB_REPO.to_string())
}

/// 私有仓库的 OTA 访问令牌，来自环境变量 `KIROSTUDIO_UPDATE_TOKEN`。
///
/// 公开仓库不需要它（GitHub 匿名可读），私有仓库则 API 与 asset 下载都必须带
/// `Authorization: Bearer`。**永远不要把它写进日志或 API 响应**——
/// 面板的更新接口只回显仓库名，不回显令牌。
///
/// 建议用 fine-grained PAT 且只授予目标仓库的 `Contents: read`：
/// 泄露只丢这一个仓库的读权限，拿不到账号其它仓库、也改不了任何东西。
fn update_token() -> Option<String> {
    std::env::var("KIROSTUDIO_UPDATE_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 给 OTA 请求附加认证头（有令牌时）。
///
/// 注意 asset 下载走的是 `github.com/.../releases/download/...`，私有仓库下该 URL
/// 会 302 到 objects.githubusercontent.com；reqwest 默认跟随重定向，而 GitHub
/// 对预签名的对象地址**不需要**再带 Authorization。带着也无害（对象存储忽略它），
/// 故此处统一加，不为两种 URL 分叉。
fn with_update_auth(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match update_token() {
        Some(token) => req.header("Authorization", format!("Bearer {token}")),
        None => req,
    }
}

/// 本平台对应的发布二进制资产名（按目标平台编译期选择）。
///
/// release.yml 产出 Linux(musl) / Windows(msvc) / macOS(darwin, arm64+x86_64) 四个资产。
/// OTA 必须下载**与当前运行平台匹配**的那一个——否则会下到别的平台/架构的二进制，
/// 即便 sha256 校验通过（下的和它自己的哈希对得上），替换后也无法执行。
///
/// ⚠️ 必须同时按 **OS × ARCH** 两个维度选择，只按 OS 分是不够的：
/// 历史 bug（两轮）——
///   1. 最初硬编码 Linux 资产名 → Windows 用户点 OTA 必然下错包；
///   2. 补了 Windows 分支后仍只有 `cfg(windows)` / `cfg(not(windows))` 二选一，**不看架构**：
///      于是 macOS（无论 Intel 还是 Apple Silicon）与 arm64 Linux 都会落到
///      `kirostudio-linux-x86_64` 分支 —— macOS 上会把 Mach-O 可执行文件替换成 Linux ELF，
///      随后 restart_service 让进程退出，新二进制根本无法执行 → **服务当场死亡且无法自愈**
///      （人工恢复：`mv kirostudio.bak kirostudio`）。arm64 Linux 同理（Exec format error）。
/// 因此这里穷举 OS×ARCH；未覆盖的组合让编译期直接失败（见最后的 compile_error!），
/// 避免"静默落到某个错误的默认值"这种最危险的形态再次发生。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const ASSET_BIN: &str = "kirostudio-windows-x86_64.exe";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const ASSET_BIN: &str = "kirostudio-linux-x86_64";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const ASSET_BIN: &str = "kirostudio-macos-x86_64";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const ASSET_BIN: &str = "kirostudio-macos-aarch64";

// 未覆盖的 OS×ARCH 组合：宁可编译失败，也绝不静默下载一个不匹配的二进制去覆盖自己。
#[cfg(not(any(
    all(target_os = "windows", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "x86_64"),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
)))]
compile_error!(
    "OTA 自更新未适配当前 OS/架构组合：请在 src/admin/update.rs 的 ASSET_BIN 增加对应分支，\
     并确保 .github/workflows/release.yml 会产出同名 release 资产。\
     （绝不能回退到某个默认资产名——那会导致 OTA 用不匹配的二进制覆盖自己，服务当场死亡。）"
);
/// 本地版本（编译期注入 Cargo.toml 的 version）。
const LOCAL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 构造某个 GitHub API 路径的镜像候选（逐个试，首个成功即用）。
/// `path` 如 `repos/{repo}/tags` 或 `repos/{repo}/commits?sha=...`。
fn github_api_candidates_for(path: &str) -> Vec<(&'static str, String)> {
    let direct = ("github-direct", format!("https://api.github.com/{path}"));
    // 🔒 配了令牌（私有仓库）时**只走直连**：镜像是第三方 HTTP 中间人，
    // 请求经它转发意味着把 Authorization 头（即 PAT）交给对方。
    // 私有仓库的可用性宁可依赖 GitHub 直连，也不能拿凭据换加速。
    if update_token().is_some() {
        return vec![direct];
    }
    vec![
        (
            "gh-proxy.org",
            format!("https://gh-proxy.org/https://api.github.com/{path}"),
        ),
        (
            "hk.gh-proxy.org",
            format!("https://hk.gh-proxy.org/https://api.github.com/{path}"),
        ),
        (
            "cdn.gh-proxy.org",
            format!("https://cdn.gh-proxy.org/https://api.github.com/{path}"),
        ),
        (
            "edgeone.gh-proxy.org",
            format!("https://edgeone.gh-proxy.org/https://api.github.com/{path}"),
        ),
        direct,
    ]
}

/// GitHub API 镜像候选（拉 tags 用）。逐个试，首个成功即用。
fn github_api_candidates() -> Vec<(&'static str, String)> {
    github_api_candidates_for(&format!("repos/{}/tags", github_repo()))
}

/// Release 资产下载镜像候选（下载二进制 / sha256 用）。`{tag}`/`{asset}` 已插值。
fn asset_candidates(tag: &str, asset: &str) -> Vec<(&'static str, String)> {
    let gh = format!(
        "github.com/{}/releases/download/{tag}/{asset}",
        github_repo()
    );
    let direct = ("github-direct", format!("https://{gh}"));
    // 🔒 同 github_api_candidates_for：有令牌就只直连，绝不把 PAT 经第三方镜像转发。
    if update_token().is_some() {
        return vec![direct];
    }
    vec![
        ("gh-proxy.org", format!("https://gh-proxy.org/https://{gh}")),
        (
            "hk.gh-proxy.org",
            format!("https://hk.gh-proxy.org/https://{gh}"),
        ),
        (
            "cdn.gh-proxy.org",
            format!("https://cdn.gh-proxy.org/https://{gh}"),
        ),
        (
            "edgeone.gh-proxy.org",
            format!("https://edgeone.gh-proxy.org/https://{gh}"),
        ),
        direct,
    ]
}

/// 校验 tag 格式：仅允许 `v?1.2.3` 形态，防路径注入 / 命令注入。
fn is_valid_version_tag(tag: &str) -> bool {
    let s = tag.strip_prefix('v').unwrap_or(tag);
    !s.is_empty()
        && s.split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        && s.split('.').count() <= 4
}

/// semver 比较：v1 > v2 → 1，< → -1，== → 0（照 WindsurfAPI compareVersions，缺省段补 0）。
fn compare_versions(v1: &str, v2: &str) -> i32 {
    let clean = |v: &str| -> Vec<u64> {
        v.strip_prefix('v')
            .unwrap_or(v)
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let (a, b) = (clean(v1), clean(v2));
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x > y {
            return 1;
        }
        if x < y {
            return -1;
        }
    }
    0
}

/// GitHub tag 条目（只取 name）。
#[derive(Deserialize)]
struct GitHubTag {
    name: String,
}

/// GitHub commits API 返回的单条 commit（只取展示需要的字段）。
#[derive(Deserialize)]
struct GitHubCommitRaw {
    sha: String,
    commit: GitHubCommitMeta,
}
#[derive(Deserialize)]
struct GitHubCommitMeta {
    message: String,
    author: Option<GitHubCommitAuthor>,
}
#[derive(Deserialize)]
struct GitHubCommitAuthor {
    date: Option<String>,
}

/// 一条 commit 快照（回前端展示"这版改了啥"）。
#[derive(Serialize)]
pub struct CommitSnapshot {
    /// 短 sha（前 7 位）。
    pub sha: String,
    /// commit 首行标题（多行 message 只取第一行）。
    pub title: String,
    /// 作者提交时间（ISO8601，可能缺）。
    pub date: Option<String>,
}

/// 更新检查结果（回前端）。附带 commit 快照——展示"最新版相对当前版改了哪些 commit"。
#[derive(Serialize)]
pub struct UpdateCheckResult {
    pub has_update: bool,
    pub local_version: String,
    pub latest_version: Option<String>,
    pub available_versions: Vec<String>,
    /// 最新版相对本地版的 commit 快照（"这版改了啥"，最多 30 条）。拉不到则空。
    pub commits: Vec<CommitSnapshot>,
    pub error: Option<String>,
    /// 当前是否容器部署。容器里 OTA 替换二进制不生效（/app 下是镜像层文件，
    /// 重启/重建即还原），前端据此隐藏/禁用「升级」按钮，提示改用 deploy 流程重建镜像。
    /// 检查本身无害（只读展示），故**不拒绝**，仅下发状态供前端提示。
    #[serde(default)]
    pub container_deployment: bool,
}

/// 更新执行结果（回前端）。
#[derive(Serialize)]
pub struct UpdatePerformResult {
    pub success: bool,
    pub message: String,
    pub updated: bool,
    pub target_version: Option<String>,
}

/// OTA 更新错误（分类错误码：handler 按 [`Self::status_code`] 映射 HTTP 状态，
/// 替代原先一律 500 —— 前端据此区分「输入错了(400)/环境冲突(409)/数据坏了(422)/
/// 上游挂了(502)/内部(500)」并给出对应提示）。
#[derive(Debug)]
pub enum UpdateError {
    /// 容器部署下执行 OTA：容器内替换二进制不生效（镜像层文件，重启即还原，
    /// 用户以为升级成功实际回滚）。必须走 deploy 流程重建镜像。HTTP 409。
    ContainerDeployment,
    /// 显式传入的版本 tag 格式非法（用户输入问题）。HTTP 400。
    BadTag(String),
    /// 显式指定的目标版本不高于当前版本（用户以为能降级/回滚）。HTTP 409。
    /// 与自动检查（target=None）场景区分：后者免更新是**正常结果**（Ok「已最新」），
    /// 只有**显式**指定了一个不可达的方向才叫冲突。
    VersionConflict(String),
    /// 下载内容校验失败：sha256 校验文件格式异常 / 校验不匹配（数据无效，重试无益）。
    /// HTTP 422。
    HashMismatch(String),
    /// 下载失败：全部镜像不可达 / 上游返回错误（上游问题，可稍后重试）。HTTP 502。
    DownloadFailed(String),
    /// 本进程侧内部错误：文件系统 IO、备份/替换失败等。HTTP 500。
    Internal(String),
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateError::ContainerDeployment => write!(
                f,
                "容器部署请用 deploy 流程重建镜像，OTA 自更新仅适用于裸进程部署"
            ),
            UpdateError::BadTag(tag) => write!(f, "版本 tag 格式非法: {tag}"),
            UpdateError::VersionConflict(tag) => {
                write!(f, "目标版本 {tag} 不高于当前版本 {LOCAL_VERSION}，拒绝降级")
            }
            UpdateError::HashMismatch(msg) | UpdateError::DownloadFailed(msg) => write!(f, "{msg}"),
            UpdateError::Internal(msg) => write!(f, "内部错误: {msg}"),
        }
    }
}

impl std::error::Error for UpdateError {}

impl UpdateError {
    /// 映射 HTTP 状态码：400 入参非法 / 409 状态冲突 / 422 数据无效 / 502 上游不可达 / 500 内部。
    pub fn status_code(&self) -> StatusCode {
        match self {
            UpdateError::ContainerDeployment | UpdateError::VersionConflict(_) => {
                StatusCode::CONFLICT
            }
            UpdateError::BadTag(_) => StatusCode::BAD_REQUEST,
            UpdateError::HashMismatch(_) => StatusCode::UNPROCESSABLE_ENTITY,
            UpdateError::DownloadFailed(_) => StatusCode::BAD_GATEWAY,
            UpdateError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// OTA 下载二进制的最大允许字节数（200 MiB）。
/// 防止恶意或被劫持的镜像推超大响应耗尽内存（OOM）。
const MAX_DOWNLOAD_BYTES: u64 = 200 * 1024 * 1024;

/// OTA 下载的带上限读取 —— 委托 [`crate::common::http_read::read_body_capped`]。
///
/// 曾在本文件内有一份独立实现。收口到 `common` 是因为同一模式已出现三处
/// （OTA 二进制、背景图图片、背景图 JSON），而**第三处当初被漏掉了** ——
/// 各写一份就是那次漏改的原因。此处保留一个薄包装只为固定 `MAX_DOWNLOAD_BYTES`。
async fn read_body_capped(
    resp: reqwest::Response,
    what: &str,
    cap: u64,
) -> anyhow::Result<Vec<u8>> {
    crate::common::http_read::read_body_capped(resp, what, cap).await
}

/// 构建一个带超时的 reqwest client（更新走独立 client，30s 超时；不复用 provider 的池）。
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("KiroStudio-Updater")
        .build()
        .unwrap_or_else(|e| {
            tracing::warn!("[Update] 构建 HTTP 客户端失败: {e}，使用无超时的默认客户端");
            reqwest::Client::default()
        })
}

/// 版本列表缓存 TTL（60 秒）：check/perform 连续点击在 TTL 内命中，不重复全量拉镜像。
///
/// 只缓存 `fetch_versions` 的原始结果（版本列表）；`fetch_commits` 的 commit 快照
/// **不缓存**（每次新鲜拉取——它是展示数据，且只在"有更新"时才拉，频率天然受限）。
const VERSIONS_CACHE_TTL: Duration = Duration::from_secs(60);

/// 进程级版本列表缓存（进程内全局；60s 过期后自然失效，无需清理）。
static VERSIONS_CACHE: std::sync::Mutex<Option<(Instant, Vec<String>)>> =
    std::sync::Mutex::new(None);

/// 读版本缓存：TTL 内命中返回副本，未命中/过期返回 None。
///
/// `now`/`ttl` 由调用方注入以便测试（生产用 `Instant::now()` + `VERSIONS_CACHE_TTL`）。
fn cached_versions_get(
    cache: &std::sync::Mutex<Option<(Instant, Vec<String>)>>,
    ttl: Duration,
    now: Instant,
) -> Option<Vec<String>> {
    let guard = cache.lock().ok()?;
    let (at, versions) = guard.as_ref()?;
    if now.duration_since(*at) > ttl {
        return None;
    }
    Some(versions.clone())
}

/// 写版本缓存（覆盖旧值）。
fn cached_versions_put(
    cache: &std::sync::Mutex<Option<(Instant, Vec<String>)>>,
    versions: Vec<String>,
    now: Instant,
) {
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((now, versions));
    }
}

/// 从 GitHub 拉最近的 tag 列表（按 semver 降序），多镜像回退，全失败返回空。
/// 结果带 60s TTL 缓存（见 [`VERSIONS_CACHE`]）；**只缓存成功结果**——
/// 镜像全挂时不缓存空列表，下次点击立即重试而不是干等一个 TTL 周期。
async fn fetch_versions(limit: usize) -> Vec<String> {
    if let Some(cached) = cached_versions_get(&VERSIONS_CACHE, VERSIONS_CACHE_TTL, Instant::now())
    {
        return cached.into_iter().take(limit).collect();
    }
    let client = http_client();
    for (name, url) in github_api_candidates() {
        match with_update_auth(
            client
                .get(&url)
                .header("Accept", "application/vnd.github.v3+json"),
        )
        .send()
        .await
        {
            Ok(resp) if resp.status().is_success() => match resp.json::<Vec<GitHubTag>>().await {
                Ok(tags) => {
                    let mut versions: Vec<String> = tags
                        .into_iter()
                        .map(|t| t.name)
                        .filter(|t| is_valid_version_tag(t))
                        .collect();
                    if versions.is_empty() {
                        tracing::warn!("[Update] {name} 返回无有效版本 tag");
                        continue;
                    }
                    versions.sort_by(|a, b| compare_versions(b, a).cmp(&0));
                    versions.truncate(limit);
                    tracing::info!("[Update] 经 {name} 取到 {} 个版本", versions.len());
                    cached_versions_put(&VERSIONS_CACHE, versions.clone(), Instant::now());
                    return versions;
                }
                Err(e) => tracing::warn!("[Update] {name} 解析 tags 失败: {e}"),
            },
            Ok(resp) => tracing::warn!("[Update] {name} 返回 {}", resp.status()),
            Err(e) => tracing::warn!("[Update] {name} 请求失败: {e}"),
        }
    }
    tracing::warn!("[Update] 所有 GitHub API 镜像均失败");
    Vec::new()
}

/// 拉两个 ref 之间的 commit 快照（展示"这版改了啥"）。用 GitHub compare API：
/// `repos/{repo}/compare/{base}...{head}` 返回 commits 数组。多镜像回退，全失败返回空。
/// base=当前本地版 tag，head=目标 tag。仅取标题+短sha+日期，不拉全 diff（省流量、够展示）。
async fn fetch_commits(base: &str, head: &str) -> Vec<CommitSnapshot> {
    // base 可能是不带 v 的本地版本；GitHub tag 习惯带 v，两种都试。
    let base_variants = [
        base.to_string(),
        format!("v{}", base.trim_start_matches('v')),
    ];
    let client = http_client();
    for base_ref in base_variants
        .iter()
        .collect::<std::collections::HashSet<_>>()
    {
        let path = format!("repos/{}/compare/{base_ref}...{head}", github_repo());
        for (name, url) in github_api_candidates_for(&path) {
            match with_update_auth(
                client
                    .get(&url)
                    .header("Accept", "application/vnd.github.v3+json"),
            )
            .send()
            .await
            {
                Ok(resp) if resp.status().is_success() => {
                    #[derive(Deserialize)]
                    struct Compare {
                        commits: Vec<GitHubCommitRaw>,
                    }
                    if let Ok(cmp) = resp.json::<Compare>().await {
                        let snaps: Vec<CommitSnapshot> = cmp
                            .commits
                            .into_iter()
                            .rev() // 最新的在前
                            .take(30)
                            .map(|c| CommitSnapshot {
                                sha: c.sha.chars().take(7).collect(),
                                title: c.commit.message.lines().next().unwrap_or("").to_string(),
                                date: c.commit.author.and_then(|a| a.date),
                            })
                            .collect();
                        tracing::info!("[Update] 经 {name} 取到 {} 条 commit 快照", snaps.len());
                        return snaps;
                    }
                }
                Ok(resp) => tracing::debug!("[Update] commit 快照 {name} 返回 {}", resp.status()),
                Err(e) => tracing::debug!("[Update] commit 快照 {name} 失败: {e}"),
            }
        }
    }
    tracing::warn!("[Update] commit 快照拉取失败（repo 私有或无 compare 权限）");
    Vec::new()
}

/// 检查更新：读本地版本 + 拉远端最新，semver 比较。
pub async fn check_for_updates() -> UpdateCheckResult {
    let available = fetch_versions(10).await;
    let latest = available.first().cloned();
    match &latest {
        None => UpdateCheckResult {
            has_update: false,
            local_version: LOCAL_VERSION.to_string(),
            latest_version: None,
            available_versions: vec![],
            commits: vec![],
            error: Some("无法获取远端版本信息（所有镜像失败）".into()),
            container_deployment: in_container(),
        },
        Some(latest_tag) => {
            let has_update = compare_versions(latest_tag, LOCAL_VERSION) > 0;
            tracing::info!(
                "[Update] 本地 {LOCAL_VERSION} / 远端 {latest_tag} / 有更新={has_update}"
            );
            // 有更新才拉 commit 快照（展示"这版改了啥"），无更新不浪费一次 compare 请求。
            let commits = if has_update {
                fetch_commits(LOCAL_VERSION, latest_tag).await
            } else {
                vec![]
            };
            UpdateCheckResult {
                has_update,
                local_version: LOCAL_VERSION.to_string(),
                latest_version: latest.clone(),
                available_versions: available,
                commits,
                error: None,
                container_deployment: in_container(),
            }
        }
    }
}

/// 后台定时自动检查新版本（由 main.rs 按 `ota_auto_check` 门控接线）。
///
/// 语义与面板「检查更新」按钮完全一致（同一个 `check_for_updates`），只是定时触发；
/// 发现新版**只打日志**（`[Update]` 前缀），**绝不自动下载/替换**——
/// OTA 是「下载二进制 + sha256 校验 + 原子覆盖运行中的自己」的高危路径，无人值守
/// 自动应用等于把"镜像投毒/中间人"升级成无人确认的 RCE；参考仓有「定时自动应用」
/// 的先例（对应其配置里的自动应用时间字段），本仓明确不立项：要做自动应用，先做
/// 独立可信信道的签名验签 + 人工确认窗口，另开会话设计。
///
/// 间隔 0 会被按 1 小时处理（tokio interval 不接受零时长，且零小时空转只会徒增
/// GitHub API 出站往返）。
pub(crate) fn spawn_auto_check(interval_hours: u32) {
    let interval = Duration::from_secs(u64::from(interval_hours.max(1)) * 3600);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let result = check_for_updates().await;
            match (&result.latest_version, result.has_update) {
                (Some(latest), true) => {
                    tracing::info!(
                        "[Update] 后台自动检查：有可用新版本 {latest}（本地 {local}）——仅通知不自动更新，可在面板手动升级",
                        latest = latest.as_str(),
                        local = result.local_version.as_str(),
                    );
                }
                (Some(latest), false) => {
                    tracing::debug!(
                        "[Update] 后台自动检查：已是最新（本地 {local} / 远端 {latest}）",
                        latest = latest.as_str(),
                        local = result.local_version.as_str(),
                    );
                }
                (None, _) => {
                    tracing::warn!(
                        "[Update] 后台自动检查失败：{}（{interval_hours} 小时后再试）",
                        result.error.as_deref().unwrap_or("未知错误"),
                    );
                }
            }
        }
    });
}

/// 多镜像回退下载一个资产，返回字节。全失败返回 Err。
async fn download_asset(tag: &str, asset: &str) -> anyhow::Result<Vec<u8>> {
    let client = http_client();
    let mut last_err = String::new();
    for (name, url) in asset_candidates(tag, asset) {
        tracing::info!("[Update] 经 {name} 下载 {asset}…");
        // 有令牌时 asset_candidates 已只返回直连，故此处不会把 PAT 发给任何第三方。
        match with_update_auth(client.get(&url)).send().await {
            Ok(resp) if resp.status().is_success() => {
                // 流式读 + 累计上限（Content-Length 预检也在内）：chunked 响应同样受限，
                // 见 read_body_capped 的说明。超限时换下一个镜像而非直接失败。
                match read_body_capped(resp, name, MAX_DOWNLOAD_BYTES).await {
                    Ok(bytes) => {
                        tracing::info!(
                            "[Update] 经 {name} 下载 {asset} 成功（{} 字节）",
                            bytes.len()
                        );
                        return Ok(bytes);
                    }
                    Err(e) => {
                        last_err = format!("{e}");
                        tracing::warn!("[Update] {last_err}");
                        continue;
                    }
                }
            }
            Ok(resp) => last_err = format!("{name} 返回 {}", resp.status()),
            Err(e) => last_err = format!("{name} 请求失败: {e}"),
        }
        tracing::warn!("[Update] {last_err}");
    }
    anyhow::bail!("所有镜像下载 {asset} 均失败: {last_err}")
}

/// 仅从 github.com 直连下载（不走任何第三方镜像/代理）。
///
/// 安全(H1):sha256 校验文件必须走**与二进制独立的可信信道**。原实现里二进制和 .sha256
/// 都走 asset_candidates()(gh-proxy.org 系列第三方代理优先),同源 → 恶意/被劫持的镜像可
/// 同时返回后门二进制 + 与之匹配的 .sha256,校验必过 = RCE。此函数强制 .sha256 只从
/// github.com 直连(TLS 证书校验由 reqwest 默认开启,无第三方 TLS 终止方能改写),
/// 使 sha256 的信任根与二进制的下载源解耦——镜像即便投毒二进制,也改不了直连 GitHub 的哈希。
/// (二进制仍可走镜像加速;完整方案是内置公钥验签,另立项。)
async fn download_from_github_direct(tag: &str, asset: &str) -> anyhow::Result<Vec<u8>> {
    let client = http_client();
    let url = format!(
        "https://github.com/{}/releases/download/{tag}/{asset}",
        github_repo()
    );
    tracing::info!("[Update] 直连 github.com 下载 {asset}（独立可信信道,不走镜像）…");
    let resp = with_update_auth(client.get(&url))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("直连 GitHub 下载 {asset} 失败: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("直连 GitHub 下载 {asset} 返回 {}", resp.status());
    }
    // 流式读 + 累计上限（含 Content-Length 预检）：与镜像路径同一道防线。
    // 即便这条是"直连 GitHub"的可信信道，也不假设对端行为——TLS 只保证没有中间人，
    // 不保证对端不会返回超大响应体（而 sha256 文件本应极小，超限必然异常）。
    read_body_capped(resp, &format!("直连 GitHub {asset}"), MAX_DOWNLOAD_BYTES).await
}

/// 容器判定的纯逻辑（探测结果注入，便于测试；[`in_container`] 是它的薄接线）。
///
/// 判据：`/.dockerenv` 存在（Docker/OrbStack 等标准标记），或 `/proc/1/cgroup`
/// 内容含 docker/kubepods（Kubernetes 的 cgroup 命名，含 K8s 场景）。
/// cgroup 探测读失败（无 /proc 的机器等）按非容器处理——探测失败不误伤裸进程部署。
fn container_detect(dockerenv_exists: bool, cgroup_content: Option<String>) -> bool {
    dockerenv_exists
        || cgroup_content
            .map(|c| c.contains("docker") || c.contains("kubepods"))
            .unwrap_or(false)
}

/// 检测是否运行在容器里（裸进程部署 vs 容器部署的语义分流）。
fn in_container() -> bool {
    container_detect(
        std::path::Path::new("/.dockerenv").exists(),
        std::fs::read_to_string("/proc/1/cgroup").ok(),
    )
}

/// 容器部署下的 OTA 拒绝理由。非容器返回 None。
///
/// 独立成纯函数而非在 `perform_update` 里内联：容器检测是环境探测，与版本判定解耦——
/// 检查接口（`check_for_updates`）**不拒绝**（只读展示，前端按
/// `container_deployment` 字段提示），只有执行路径（`perform_update`）拒绝。
fn container_rejection_reason() -> Option<String> {
    if in_container() {
        Some("容器部署请用 deploy 流程重建镜像，OTA 自更新仅适用于裸进程部署".to_string())
    } else {
        None
    }
}

/// 执行 OTA 更新：下载新二进制 + sha256 校验 + 备份 + 原子替换。
///
/// **不在此函数里重启**——替换成功后由 handler 调用 `restart_service` 触发 systemd 拉起新二进制，
/// 与"一键重启"复用同一条路径。返回结果供前端提示（success 后前端提示"数秒后自动升级完成"）。
pub async fn perform_update(target: Option<String>) -> Result<UpdatePerformResult, UpdateError> {
    // 0) 容器部署拒绝（目标版本判定**之前**）：容器内 /app 下的可执行文件是镜像层文件，
    //    rename 后容器重启/镜像重建即还原——用户以为升级成功实际回滚。绝不静默换二进制。
    if container_rejection_reason().is_some() {
        return Err(UpdateError::ContainerDeployment);
    }
    // 1) 定目标版本
    let explicit = target.is_some();
    let tag = match target {
        Some(t) => {
            if !is_valid_version_tag(&t) {
                return Err(UpdateError::BadTag(t));
            }
            t
        }
        // 显式指定 tag 时**不拉 GitHub tags**（MINOR-8）：直接走下载+校验。
        // 原来无条件先拉一次 tags —— 显式 tag 场景下那次
        // 往返纯属浪费（还可能因 GitHub 抖动误报「下载失败」而阻断本来可用的
        // 指定版本安装）。
        None => {
            let check = check_for_updates().await;
            if let Some(err) = &check.error {
                return Err(UpdateError::DownloadFailed(err.clone()));
            }
            check
                .latest_version
                .clone()
                .ok_or_else(|| UpdateError::DownloadFailed("无最新版本".into()))?
        }
    };

    // 已是目标版本或目标版本更旧 → 免更新（防降级）。
    // 语义分流：**显式**指定了旧/等版本 → 409（用户以为能降级/回滚，必须明确拒绝）；
    // 自动（target=None=升级到最新）→ 正常 Ok「已是最新」，不是错误，前端照常展示。
    if !target_is_newer(&tag) {
        if explicit {
            return Err(UpdateError::VersionConflict(tag));
        }
        return Ok(UpdatePerformResult {
            success: true,
            message: format!("当前版本 {LOCAL_VERSION} 已是 {tag} 或更新，无需更新"),
            updated: false,
            target_version: Some(tag),
        });
    }

    tracing::info!("[Update] 开始升级到 {tag}：下载二进制 + sha256 校验 + 替换（完成后自动重启）");

    // 2) 下载二进制（可走镜像加速）+ sha256 文件（强制 github.com 直连,独立可信信道）
    let bin = download_asset(&tag, ASSET_BIN)
        .await
        .map_err(|e| UpdateError::DownloadFailed(e.to_string()))?;
    tracing::info!(
        "[Update] 二进制下载完成（{} 字节），开始取 sha256 校验文件",
        bin.len()
    );
    // 安全(H1):sha256 只从 github 直连取,不走 asset_candidates 的第三方镜像——
    // 否则二进制与哈希同源,恶意镜像给"后门二进制+匹配哈希"即绕过校验=RCE。
    // 直连失败宁可中止升级,也不退回镜像取哈希(那等于没校验)。
    let sha_txt = download_from_github_direct(&tag, &format!("{ASSET_BIN}.sha256"))
        .await
        .map_err(|e| UpdateError::DownloadFailed(e.to_string()))?;

    // 3) ⭐sha256 校验（安全红线：哈希来自 github 直连、与二进制下载源解耦,镜像投毒改不了哈希）
    let expected = String::from_utf8_lossy(&sha_txt)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase();
    if expected.len() != 64 {
        return Err(UpdateError::HashMismatch(
            "下载的 sha256 文件格式异常，拒绝更新".into(),
        ));
    }
    let mut hasher = Sha256::new();
    hasher.update(&bin);
    let actual = hex::encode(hasher.finalize());
    if actual != expected {
        return Err(UpdateError::HashMismatch(format!(
            "sha256 校验失败（期望 {expected}，实得 {actual}），拒绝替换二进制"
        )));
    }
    tracing::info!("[Update] sha256 校验通过");

    // 4) 备份当前 exe + 替换。平台差异（运行中 exe 的替换方式不同）：
    let exe = std::env::current_exe().map_err(|e| {
        UpdateError::Internal(format!("获取当前可执行文件路径失败: {e}"))
    })?;
    let bak = exe.with_extension("bak");
    let new = exe.with_extension("new");
    // 先写 .new（同目录，保证后续 rename 是同一文件系统的原子操作）
    tracing::info!("[Update] 写入新二进制到 {new:?} 并备份现役版本，准备原子替换");
    tokio::fs::write(&new, &bin)
        .await
        .map_err(|e| UpdateError::Internal(format!("写入新二进制 {new:?} 失败: {e}")))?;
    // 赋可执行权限（Unix）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = tokio::fs::metadata(&new)
            .await
            .map_err(|e| UpdateError::Internal(format!("读取新二进制元数据失败: {e}")))?
            .permissions();
        perm.set_mode(0o755);
        tokio::fs::set_permissions(&new, perm)
            .await
            .map_err(|e| UpdateError::Internal(format!("设置可执行权限失败: {e}")))?;
    }

    #[cfg(not(target_os = "windows"))]
    {
        // Linux/macOS：允许 rename 覆盖运行中的 exe（inode 语义）。
        // 备份现役 exe（供启动自检失败时 systemd ExecStartPre 回滚兜底）。
        // ⚠️ 备份失败必须 abort：若无 .bak 就 rename 替换，回滚网彻底失效（崩了没得回滚）。
        // fail-safe 优于 fail-open——宁可本次不升级，也不留一个无回滚点的替换。
        tokio::fs::copy(&exe, &bak)
            .await
            .map_err(|e| {
                UpdateError::Internal(format!(
                    "备份现役二进制到 {bak:?} 失败，已中止升级（不留无回滚点的替换）: {e}"
                ))
            })?;
        tokio::fs::rename(&new, &exe)
            .await
            .map_err(|e| UpdateError::Internal(format!("替换二进制失败: {e}")))?;
    }

    #[cfg(target_os = "windows")]
    {
        // Windows：**不能覆盖**运行中的 .exe（文件被独占锁定，rename over 会 os error 5），
        // 但**可以把运行中的 .exe 改名移走**。故用「移走旧的 → 新的顶上」的 Windows 惯用法：
        //   1) rename(exe → .bak)：把正在运行的 exe 改名为 .bak（既是备份、又腾出原路径）；
        //   2) rename(.new → exe)：把新二进制放到原路径。
        // 重启由 start.bat / run.bat 的监督循环负责：进程 exit(0) 后脚本按**原路径**重新拉起，
        // 拉起的就是新二进制。若第 2 步失败，尽力回滚（把 .bak 改回来），不留下缺失的 exe。
        // 先清理可能残留的旧 .bak（上次升级留下的），否则 rename 到已存在路径在 Windows 会失败。
        let _ = tokio::fs::remove_file(&bak).await;
        tokio::fs::rename(&exe, &bak)
            .await
            .map_err(|e| {
                UpdateError::Internal(format!(
                    "移走现役二进制到 {bak:?} 失败，已中止升级（未改动运行中的 exe）: {e}"
                ))
            })?;
        if let Err(e) = tokio::fs::rename(&new, &exe).await {
            // 新二进制没顶上：尽力把旧的改名回来，避免原路径缺失导致重启后无 exe 可拉。
            let _ = tokio::fs::rename(&bak, &exe).await;
            let _ = tokio::fs::remove_file(&new).await;
            return Err(UpdateError::Internal(format!(
                "替换二进制失败，已回滚到原版本（未升级）: {e}"
            )));
        }
    }
    tracing::warn!("[Update] 二进制已替换为 {tag}（备份在 {bak:?}），待重启生效");

    Ok(UpdatePerformResult {
        success: true,
        message: format!("已升级到 {tag}，即将重启生效（数秒后自动恢复）"),
        updated: true,
        target_version: Some(tag),
    })
}

/// 目标版本是否**比本地新**（只升不降；等于或低于当前版本→免更新，防降级攻击）。
///
/// 安全：`perform_update` 接受 admin 传入的任意 tag，若不做版本方向检查，
/// 持 admin key 的攻击者可把服务降到含已知漏洞的旧版本。
fn target_is_newer(tag: &str) -> bool {
    compare_versions(tag, LOCAL_VERSION) > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归（已知缺陷 #2 的未修那一半）：**chunked 响应**（无 Content-Length）也必须受上限约束。
    ///
    /// **旧代码为何 FAIL**：上限检查只有 `if let Some(content_length) = resp.content_length()`
    /// 这一道预检，chunked 响应不带该头 → 预检整个被跳过 → 随后 `resp.bytes()` 把无上限的
    /// 响应体全读进内存。旧代码在这个测试里会一路读完 `MAX_DOWNLOAD_BYTES` 以上的数据并返回
    /// `Ok`，而本测试断言 `is_err()`。
    ///
    /// 生产后果：OOM 的对象是**正在服务的网关进程**——点一次「检查更新」就能打死线上。
    /// 注意加大上限治不了它（绕过的是"有没有检查"，不是阈值大小）。
    ///
    /// 用真实 TCP 服务端而非 mock：缺陷恰恰在 reqwest 的分块读取语义上，
    /// 用假 Response 测不出"没有 Content-Length 时会发生什么"。
    // multi_thread：服务端任务要与客户端读取**真正并发**推 1MiB。
    // current_thread 下写端会在 socket 缓冲填满后卡住而客户端还没开始读 body，
    // 表现为 hyper 侧 IncompleteMessage（看起来像连接坏了，实则是调度饿死）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn chunked_response_without_content_length_is_still_capped() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // 服务端：chunked 编码、**不发 Content-Length**、持续吐数据直到被对端关闭。
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // 必须把请求头**读到空行为止**再回响应：只 read 一次可能截断，
            // 导致 hyper 侧报 IncompleteMessage 而非进入我们要测的 body 读取路径。
            let mut req = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                match tokio::io::AsyncReadExt::read(&mut sock, &mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        req.extend_from_slice(&chunk[..n]);
                        // GET 请求无 body，收到头部结束的空行即可回响应。
                        if req.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            if sock
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }
            // 发 16 块 × 64KiB = 1MiB，远超测试用的 TEST_CAP（64KiB）。
            // 用**有限**块数而非无限循环：客户端一旦因超限中断读取，无限写会拿到 EPIPE，
            // 那本身没问题，但有限块数让测试行为完全确定、不依赖断开时序。
            let payload = vec![b'A'; 64 * 1024];
            let head = format!("{:x}\r\n", payload.len());
            for _ in 0..16 {
                if sock.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                if sock.write_all(&payload).await.is_err() {
                    return;
                }
                if sock.write_all(b"\r\n").await.is_err() {
                    return;
                }
            }
            // 正常收尾（0 长度块），使响应是**合法完整**的 chunked 响应——
            // 这样测的就是"上限生效"，而不是"连接坏了"。
            let _ = sock.write_all(b"0\r\n\r\n").await;
            let _ = sock.flush().await;
        });

        // ⚠️ 必须 no_proxy()：本 crate 的 reqwest 开了 `system-proxy` feature，
        // 默认会读系统代理设置。开发机上装了 Clash/Surge 之类时，客户端会尝试把
        // 到 127.0.0.1 的请求也塞进代理隧道 → 握手失败，报 hyper IncompleteMessage
        // （看起来像"连接坏了"，与被测逻辑毫无关系）。这与已知问题 #19 同型：
        // 测试必须与本机网络环境无关。
        let resp = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("构建测试 client")
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .expect("请求应成功建立");
        assert!(
            resp.content_length().is_none(),
            "前提：chunked 响应不应带 Content-Length，否则测不到该绕过路径"
        );

        // 小上限：64KiB。服务端会推 1MiB，故必然触发累计截断。
        const TEST_CAP: u64 = 64 * 1024;
        let err = read_body_capped(resp, "test-chunked", TEST_CAP)
            .await
            .expect_err("超限的 chunked 响应必须返回 Err（旧代码会无上限读完 → Ok）");
        let msg = err.to_string();
        assert!(
            msg.contains("超过上限"),
            "错误应明确指出超过上限，便于运维定位: {msg}"
        );
    }

    /// 环境变量是进程级全局状态，多个测试并发读写会互相打断，故串行化。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard(&'static str);
    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            unsafe { std::env::set_var(key, val) };
            Self(key)
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var(self.0) };
        }
    }

    #[test]
    fn repo_defaults_and_env_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("KIROSTUDIO_UPDATE_REPO") };
        assert_eq!(github_repo(), DEFAULT_GITHUB_REPO);

        let _g = EnvGuard::set("KIROSTUDIO_UPDATE_REPO", "example/override-repo");
        assert_eq!(github_repo(), "example/override-repo");
    }

    #[test]
    fn blank_env_falls_back_to_default() {
        let _lock = ENV_LOCK.lock().unwrap();
        // 空串/纯空白必须视作"未配置"，否则 systemd 里写了空的 Environment=
        // 会让 OTA 去请求 repos//tags 这种畸形路径。
        let _g = EnvGuard::set("KIROSTUDIO_UPDATE_REPO", "   ");
        assert_eq!(github_repo(), DEFAULT_GITHUB_REPO);

        let _t = EnvGuard::set("KIROSTUDIO_UPDATE_TOKEN", "  ");
        assert!(update_token().is_none());
    }

    /// 核心安全属性：配了令牌就绝不把请求发给第三方镜像。
    /// 镜像是 HTTP 中间人，经它转发等于把 PAT 交给对方。
    #[test]
    fn token_present_restricts_to_direct_only() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _t = EnvGuard::set("KIROSTUDIO_UPDATE_TOKEN", "ghp_dummy_for_test");

        let api = github_api_candidates_for("repos/x/y/tags");
        assert_eq!(api.len(), 1, "有令牌时 API 候选必须只剩直连");
        assert_eq!(api[0].0, "github-direct");
        assert!(api.iter().all(|(_, u)| !u.contains("gh-proxy")));

        let assets = asset_candidates("v1.2.3", "kirostudio-linux-x86_64");
        assert_eq!(assets.len(), 1, "有令牌时资产候选必须只剩直连");
        assert_eq!(assets[0].0, "github-direct");
        assert!(assets.iter().all(|(_, u)| !u.contains("gh-proxy")));
    }

    #[test]
    fn no_token_keeps_mirror_fallbacks() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("KIROSTUDIO_UPDATE_TOKEN") };

        // 公开仓库无凭据可泄露，镜像加速照旧，且直连必须是最后兜底。
        let api = github_api_candidates_for("repos/x/y/tags");
        assert!(api.len() > 1);
        assert_eq!(api.last().unwrap().0, "github-direct");

        let assets = asset_candidates("v1.2.3", "kirostudio-linux-x86_64");
        assert!(assets.len() > 1);
        assert_eq!(assets.last().unwrap().0, "github-direct");
    }

    /// UpdateError → HTTP 状态码映射（前端按状态码分类展示/重试，逐变体钉死）。
    #[test]
    fn update_error_status_codes() {
        assert_eq!(
            UpdateError::ContainerDeployment.status_code(),
            StatusCode::CONFLICT,
            "容器部署拒绝 = 409（环境状态冲突）"
        );
        assert_eq!(
            UpdateError::BadTag("1.2".into()).status_code(),
            StatusCode::BAD_REQUEST,
            "tag 格式非法 = 400（输入问题）"
        );
        assert_eq!(
            UpdateError::VersionConflict("v0.9.0".into()).status_code(),
            StatusCode::CONFLICT,
            "显式降级 = 409（版本状态冲突）"
        );
        assert_eq!(
            UpdateError::HashMismatch("x".into()).status_code(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "sha256 校验失败 = 422（数据无效）"
        );
        assert_eq!(
            UpdateError::DownloadFailed("x".into()).status_code(),
            StatusCode::BAD_GATEWAY,
            "下载失败 = 502（上游不可达）"
        );
        assert_eq!(
            UpdateError::Internal("x".into()).status_code(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "内部错误 = 500"
        );
    }

    /// 容器判定的纯逻辑：四个输入组合全覆盖。
    ///
    /// 刻意不写 `in_container()`/`container_rejection_reason()` 的环境断言——
    /// 本仓库验证循环本身跑在容器里，那种断言在 CI 必红。
    #[test]
    fn container_detect_logic() {
        assert!(
            container_detect(true, None),
            ".dockerenv 存在即容器（cgroup 探测失败也不影响）"
        );
        assert!(
            container_detect(false, Some("1:name=systemd:/docker/abc123".into())),
            "cgroup 含 docker 标记"
        );
        assert!(
            container_detect(false, Some("0::/kubepods/besteffort/pod123".into())),
            "cgroup 含 kubepods 标记（K8s）"
        );
        assert!(
            !container_detect(false, None),
            "cgroup 读失败 + 无 .dockerenv = 非容器（探测失败不误伤裸进程部署）"
        );
        assert!(
            !container_detect(false, Some("1:name=systemd:/".into())),
            "cgroup 无容器标记 = 非容器"
        );
    }

    /// 版本缓存命中/过期/覆盖：用局部 Mutex + 注入时间，不碰全局缓存、不碰网络。
    #[test]
    fn versions_cache_hit_and_expiry() {
        let cache = std::sync::Mutex::new(None);
        let t0 = Instant::now();

        assert_eq!(
            cached_versions_get(&cache, VERSIONS_CACHE_TTL, t0),
            None,
            "空缓存必不命中"
        );

        let versions = vec!["v1.2.3".to_string(), "v1.2.2".to_string()];
        cached_versions_put(&cache, versions.clone(), t0);
        assert_eq!(
            cached_versions_get(&cache, VERSIONS_CACHE_TTL, t0 + Duration::from_secs(30)),
            Some(versions.clone()),
            "TTL 内命中"
        );
        assert_eq!(
            cached_versions_get(&cache, VERSIONS_CACHE_TTL, t0 + Duration::from_secs(61)),
            None,
            "超过 TTL 视为过期"
        );

        // 过期后可重新写入并立即命中（下次 fetch 的落盘路径）。
        cached_versions_put(&cache, vec!["v2.0.0".into()], t0 + Duration::from_secs(61));
        assert_eq!(
            cached_versions_get(&cache, VERSIONS_CACHE_TTL, t0 + Duration::from_secs(61)),
            Some(vec!["v2.0.0".to_string()])
        );
    }

    /// ⭐ 源码守卫（MINOR-8）：显式指定 tag 的升级**不得**拉 GitHub tags。
    ///
    /// 原实现无条件先 `check_for_updates()` 拉一次 tags —— 显式 tag 场景下那次
    /// 往返纯属浪费，且 GitHub 抖动会误报「下载失败」阻断本来可用的指定版本安装。
    /// 修复后 `check_for_updates` 只存在于自动升级（`None`）分支内。
    ///
    /// 回退即 FAIL：把 tags 拉取挪回 match 之外（无条件执行）——`None => {`
    /// 出现在 `check_for_updates` **之前**，位置断言红。
    #[test]
    fn explicit_tag_skips_tags_fetch() {
        let src = include_str!("update.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let fname = format!("pub async fn perform_update{}", "(");
        let start = prod
            .find(&fname)
            .expect("perform_update 不应被改名");
        let body_end = prod[start..]
            .find("\nfn ")
            .map(|i| i + start)
            .unwrap_or(prod.len());
        let body = &prod[start..body_end];

        let check_at = body
            .find("check_for_updates")
            .expect("自动升级分支必须保留 tags 拉取");
        let none_at = body
            .find("None => {")
            .expect("目标版本判定（None 分支）不应被改名");
        assert!(
            none_at < check_at,
            "check_for_updates 必须只在 None（自动升级）分支内调用——\
             显式 tag 场景不该拉 GitHub tags（浪费一次往返 + 可能误报下载失败）"
        );
    }
}
