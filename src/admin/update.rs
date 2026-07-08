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
//! 应用更新：写入 `<exe>.new` → 原子 rename 覆盖运行中的 exe（Linux 允许）→ 复用
//! `AdminService::restart_service`（exit(0) 交给 systemd `Restart=always` 拉起新二进制）。
//! 替换前备份 `<exe>.bak`，供启动自检失败时回滚兜底。
//!
//! ⚠️ 平台差异：rename 运行中 exe 仅 Linux 可行；本机 Windows 开发跑不了（文件占用），
//! 只能在 CI/Linux 部署验证（同"本机 npm build 假绿"规矩）。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// 上游仓库（owner/repo）。发布产物见 .github/workflows/release.yml：
/// `kirostudio-linux-x86_64` + `kirostudio-linux-x86_64.sha256`。
const GITHUB_REPO: &str = "dwgx/KiroStudio";
/// 发布的二进制资产名（musl 静态链接，见 release.yml）。
const ASSET_BIN: &str = "kirostudio-linux-x86_64";
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

/// 构建一个带超时的 reqwest client（更新走独立 client，30s 超时；不复用 provider 的池）。
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("KiroStudio-Updater")
        .build()
        .unwrap_or_default()
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
            Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                Ok(bytes) => {
                    tracing::info!("[Update] 经 {name} 下载 {asset} 成功（{} 字节）", bytes.len());
                    return Ok(bytes.to_vec());
                }
                Err(e) => last_err = format!("{name} 读取响应体失败: {e}"),
            },
            Ok(resp) => last_err = format!("{name} 返回 {}", resp.status()),
            Err(e) => last_err = format!("{name} 请求失败: {e}"),
        }
        tracing::warn!("[Update] {last_err}");
    }
    anyhow::bail!("所有镜像下载 {asset} 均失败: {last_err}")
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

    // 已是目标版本 → 免更新
    if !target_differs(&tag) {
        return Ok(UpdatePerformResult {
            success: true,
            message: format!("已是版本 {tag}，无需更新"),
            updated: false,
            target_version: Some(tag),
        });
    }

    // 2) 下载二进制 + sha256 文件
    let bin = download_asset(&tag, ASSET_BIN).await?;
    let sha_txt = download_asset(&tag, &format!("{ASSET_BIN}.sha256")).await?;

    // 3) ⭐sha256 校验（安全红线，不可省——防镜像/中间人替换二进制 = RCE）
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

    // 4) 备份当前 exe + 原子替换（Linux 允许 rename 运行中的 exe）
    let exe = std::env::current_exe()?;
    let bak = exe.with_extension("bak");
    let new = exe.with_extension("new");
    // 先写 .new（同目录，保证 rename 是同一文件系统的原子操作）
    tokio::fs::write(&new, &bin).await?;
    // 赋可执行权限（Unix）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = tokio::fs::metadata(&new).await?.permissions();
        perm.set_mode(0o755);
        tokio::fs::set_permissions(&new, perm).await?;
    }
    // 备份现役 exe（供启动自检失败时 systemd ExecStartPre 回滚兜底）。
    // ⚠️ 备份失败必须 abort：若无 .bak 就 rename 替换，回滚网彻底失效（崩了没得回滚）。
    // fail-safe 优于 fail-open——宁可本次不升级，也不留一个无回滚点的替换。
    tokio::fs::copy(&exe, &bak).await.map_err(|e| {
        anyhow::anyhow!("备份现役二进制到 {bak:?} 失败，已中止升级（不留无回滚点的替换）: {e}")
    })?;
    tokio::fs::rename(&new, &exe).await?;
    tracing::warn!("[Update] 二进制已替换为 {tag}（备份在 {bak:?}），待重启生效");

    Ok(UpdatePerformResult {
        success: true,
        message: format!("已升级到 {tag}，即将重启生效（数秒后自动恢复）"),
        updated: true,
        target_version: Some(tag),
    })
}

/// 目标版本是否与本地不同（相同则免更新）。
fn target_differs(tag: &str) -> bool {
    compare_versions(tag, LOCAL_VERSION) != 0
}


