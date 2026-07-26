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
//! Linux 下 `kirostudio-linux-{x86_64,aarch64}`，macOS 下 `kirostudio-macos-{x86_64,aarch64}`。
//! 下错平台/架构的二进制即便 sha256 自洽也无法运行（覆盖后服务当场死亡），故必须精确匹配；
//! 未适配的组合在编译期直接 `compile_error!`，绝不静默回退到某个默认资产名。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// 上游仓库（owner/repo）。发布产物见 .github/workflows/release.yml，每个平台一份二进制 + 同名 .sha256：
/// `kirostudio-linux-x86_64` / `kirostudio-linux-aarch64` /
/// `kirostudio-macos-x86_64` / `kirostudio-macos-aarch64` / `kirostudio-windows-x86_64.exe`。
/// ⚠️ 这份清单必须与 `ASSET_BIN` 的 cfg 分支、release.yml 的产出**三方保持一致**，
/// 少一个平台就意味着该平台的用户点 OTA 会 404（好）或下到错误资产（灾难）。
const GITHUB_REPO: &str = "dwgx/KiroStudio";

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
#[cfg(all(target_os = "windows", target_arch = "aarch64"))]
const ASSET_BIN: &str = "kirostudio-windows-aarch64.exe";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const ASSET_BIN: &str = "kirostudio-linux-x86_64";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const ASSET_BIN: &str = "kirostudio-linux-aarch64";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const ASSET_BIN: &str = "kirostudio-macos-x86_64";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const ASSET_BIN: &str = "kirostudio-macos-aarch64";

// 未覆盖的 OS×ARCH 组合：宁可编译失败，也绝不静默下载一个不匹配的二进制去覆盖自己。
#[cfg(not(any(
    all(target_os = "windows", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
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
    vec![
        ("gh-proxy.org", format!("https://gh-proxy.org/https://api.github.com/{path}")),
        ("hk.gh-proxy.org", format!("https://hk.gh-proxy.org/https://api.github.com/{path}")),
        ("cdn.gh-proxy.org", format!("https://cdn.gh-proxy.org/https://api.github.com/{path}")),
        ("edgeone.gh-proxy.org", format!("https://edgeone.gh-proxy.org/https://api.github.com/{path}")),
        ("github-direct", format!("https://api.github.com/{path}")),
    ]
}

/// GitHub API 镜像候选（拉 tags 用）。逐个试，首个成功即用。
fn github_api_candidates() -> Vec<(&'static str, String)> {
    github_api_candidates_for(&format!("repos/{GITHUB_REPO}/tags"))
}

/// Release 资产下载镜像候选（下载二进制 / sha256 用）。`{tag}`/`{asset}` 已插值。
fn asset_candidates(tag: &str, asset: &str) -> Vec<(&'static str, String)> {
    let gh = format!("github.com/{GITHUB_REPO}/releases/download/{tag}/{asset}");
    vec![
        ("gh-proxy.org", format!("https://gh-proxy.org/https://{gh}")),
        ("hk.gh-proxy.org", format!("https://hk.gh-proxy.org/https://{gh}")),
        ("cdn.gh-proxy.org", format!("https://cdn.gh-proxy.org/https://{gh}")),
        ("edgeone.gh-proxy.org", format!("https://edgeone.gh-proxy.org/https://{gh}")),
        ("github-direct", format!("https://{gh}")),
    ]
}

/// 校验 tag 格式：仅允许 `v?1.2.3` 形态，防路径注入 / 命令注入。
fn is_valid_version_tag(tag: &str) -> bool {
    let s = tag.strip_prefix('v').unwrap_or(tag);
    !s.is_empty()
        && s.split('.').all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        && s.split('.').count() <= 4
}

/// semver 比较：v1 > v2 → 1，< → -1，== → 0（照 WindsurfAPI compareVersions，缺省段补 0）。
fn compare_versions(v1: &str, v2: &str) -> i32 {
    let clean = |v: &str| -> Vec<u64> {
        v.strip_prefix('v').unwrap_or(v).split('.').map(|p| p.parse().unwrap_or(0)).collect()
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
}

/// 更新执行结果（回前端）。
#[derive(Serialize)]
pub struct UpdatePerformResult {
    pub success: bool,
    pub message: String,
    pub updated: bool,
    pub target_version: Option<String>,
}

/// OTA 下载二进制的最大允许字节数（200 MiB）。
/// 防止恶意或被劫持的镜像推超大响应耗尽内存（OOM）。
const MAX_DOWNLOAD_BYTES: u64 = 200 * 1024 * 1024;

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

/// 从 GitHub 拉最近的 tag 列表（按 semver 降序），多镜像回退，全失败返回空。
async fn fetch_versions(limit: usize) -> Vec<String> {
    let client = http_client();
    for (name, url) in github_api_candidates() {
        match client
            .get(&url)
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<Vec<GitHubTag>>().await {
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
                        return versions;
                    }
                    Err(e) => tracing::warn!("[Update] {name} 解析 tags 失败: {e}"),
                }
            }
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
    let base_variants = [base.to_string(), format!("v{}", base.trim_start_matches('v'))];
    let client = http_client();
    for base_ref in base_variants.iter().collect::<std::collections::HashSet<_>>() {
        let path = format!("repos/{GITHUB_REPO}/compare/{base_ref}...{head}");
        for (name, url) in github_api_candidates_for(&path) {
            match client.get(&url).header("Accept", "application/vnd.github.v3+json").send().await {
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
            }
        }
    }
}

/// 多镜像回退下载一个资产，返回字节。全失败返回 Err。
async fn download_asset(tag: &str, asset: &str) -> anyhow::Result<Vec<u8>> {
    let client = http_client();
    let mut last_err = String::new();
    for (name, url) in asset_candidates(tag, asset) {
        tracing::info!("[Update] 经 {name} 下载 {asset}…");
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                // Content-Length 预检：防止恶意/被劫持镜像推超大响应 OOM
                if let Some(content_length) = resp.content_length() {
                    if content_length > MAX_DOWNLOAD_BYTES {
                        last_err = format!(
                            "{name} Content-Length {content_length} 超过 {MAX_DOWNLOAD_BYTES}，拒绝下载"
                        );
                        tracing::warn!("[Update] {last_err}");
                        continue;
                    }
                }
                match resp.bytes().await {
                    Ok(bytes) => {
                        tracing::info!("[Update] 经 {name} 下载 {asset} 成功（{} 字节）", bytes.len());
                        return Ok(bytes.to_vec());
                    }
                    Err(e) => last_err = format!("{name} 读取响应体失败: {e}"),
                }
            },
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
    let url = format!("https://github.com/{GITHUB_REPO}/releases/download/{tag}/{asset}");
    tracing::info!("[Update] 直连 github.com 下载 {asset}（独立可信信道,不走镜像）…");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("直连 GitHub 下载 {asset} 失败: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("直连 GitHub 下载 {asset} 返回 {}", resp.status());
    }
    // Content-Length 预检（sha256 文件应极小，> 200MiB 必然异常）
    if let Some(content_length) = resp.content_length() {
        if content_length > MAX_DOWNLOAD_BYTES {
            anyhow::bail!(
                "直连 GitHub 响应体 Content-Length {content_length} 超过 {MAX_DOWNLOAD_BYTES}，拒绝下载"
            );
        }
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| anyhow::anyhow!("直连 GitHub 读取 {asset} 响应体失败: {e}"))?;
    Ok(bytes.to_vec())
}

/// 执行 OTA 更新：下载新二进制 + sha256 校验 + 备份 + 原子替换。
///
/// **不在此函数里重启**——替换成功后由 handler 调用 `restart_service` 触发 systemd 拉起新二进制，
/// 与"一键重启"复用同一条路径。返回结果供前端提示（success 后前端提示"数秒后自动升级完成"）。
pub async fn perform_update(target: Option<String>) -> anyhow::Result<UpdatePerformResult> {
    // 1) 定目标版本
    let check = check_for_updates().await;
    if let Some(err) = &check.error {
        anyhow::bail!("{err}");
    }
    let tag = match target {
        Some(t) => {
            if !is_valid_version_tag(&t) {
                anyhow::bail!("版本 tag 格式非法: {t}");
            }
            t
        }
        None => check.latest_version.clone().ok_or_else(|| anyhow::anyhow!("无最新版本"))?,
    };

    // 已是目标版本或目标版本更旧 → 免更新（防降级）
    if !target_is_newer(&tag) {
        return Ok(UpdatePerformResult {
            success: true,
            message: format!("当前版本 {LOCAL_VERSION} 已是 {tag} 或更新，无需更新"),
            updated: false,
            target_version: Some(tag),
        });
    }

    tracing::info!("[Update] 开始升级到 {tag}：下载二进制 + sha256 校验 + 替换（完成后自动重启）");

    // 2) 下载二进制（可走镜像加速）+ sha256 文件（强制 github.com 直连,独立可信信道）
    let bin = download_asset(&tag, ASSET_BIN).await?;
    tracing::info!("[Update] 二进制下载完成（{} 字节），开始取 sha256 校验文件", bin.len());
    // 安全(H1):sha256 只从 github 直连取,不走 asset_candidates 的第三方镜像——
    // 否则二进制与哈希同源,恶意镜像给"后门二进制+匹配哈希"即绕过校验=RCE。
    // 直连失败宁可中止升级,也不退回镜像取哈希(那等于没校验)。
    let sha_txt = download_from_github_direct(&tag, &format!("{ASSET_BIN}.sha256")).await?;

    // 3) ⭐sha256 校验（安全红线：哈希来自 github 直连、与二进制下载源解耦,镜像投毒改不了哈希）
    let expected = String::from_utf8_lossy(&sha_txt)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase();
    if expected.len() != 64 {
        anyhow::bail!("下载的 sha256 文件格式异常，拒绝更新");
    }
    let mut hasher = Sha256::new();
    hasher.update(&bin);
    let actual = hex::encode(hasher.finalize());
    if actual != expected {
        anyhow::bail!("sha256 校验失败（期望 {expected}，实得 {actual}），拒绝替换二进制");
    }
    tracing::info!("[Update] sha256 校验通过");

    // 4) 备份当前 exe + 替换。平台差异（运行中 exe 的替换方式不同）：
    let exe = std::env::current_exe()?;
    let bak = exe.with_extension("bak");
    let new = exe.with_extension("new");
    // 先写 .new（同目录，保证后续 rename 是同一文件系统的原子操作）
    tracing::info!("[Update] 写入新二进制到 {new:?} 并备份现役版本，准备原子替换");
    tokio::fs::write(&new, &bin).await?;
    // 赋可执行权限（Unix）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = tokio::fs::metadata(&new).await?.permissions();
        perm.set_mode(0o755);
        tokio::fs::set_permissions(&new, perm).await?;
    }

    #[cfg(not(target_os = "windows"))]
    {
        // Linux/macOS：允许 rename 覆盖运行中的 exe（inode 语义）。
        // 备份现役 exe（供启动自检失败时 systemd ExecStartPre 回滚兜底）。
        // ⚠️ 备份失败必须 abort：若无 .bak 就 rename 替换，回滚网彻底失效（崩了没得回滚）。
        // fail-safe 优于 fail-open——宁可本次不升级，也不留一个无回滚点的替换。
        tokio::fs::copy(&exe, &bak).await.map_err(|e| {
            anyhow::anyhow!("备份现役二进制到 {bak:?} 失败，已中止升级（不留无回滚点的替换）: {e}")
        })?;
        tokio::fs::rename(&new, &exe).await?;
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
        tokio::fs::rename(&exe, &bak).await.map_err(|e| {
            anyhow::anyhow!("移走现役二进制到 {bak:?} 失败，已中止升级（未改动运行中的 exe）: {e}")
        })?;
        if let Err(e) = tokio::fs::rename(&new, &exe).await {
            // 新二进制没顶上：尽力把旧的改名回来，避免原路径缺失导致重启后无 exe 可拉。
            let _ = tokio::fs::rename(&bak, &exe).await;
            let _ = tokio::fs::remove_file(&new).await;
            return Err(anyhow::anyhow!(
                "替换二进制失败，已回滚到原版本（未升级）: {e}"
            ));
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


