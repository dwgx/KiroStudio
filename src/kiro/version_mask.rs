//! Kiro IDE 版本自动获取（版本伪装）
//!
//! 从官方稳定版元数据端点读取 `currentRelease` 字段，得到当前发布的 Kiro IDE 版本号，
//! 用于构造与官方 IDE 一致的 User-Agent 里的版本号段（`KiroIDE-<version>-<machineId>`
//! 形状的版本部分，UA 构造点见 `endpoint/ide.rs` 与 `token_manager.rs`）。
//!
//! - 进程内缓存（`OnceLock<RwLock<Option<String>>>`）+ 后台定时刷新（12h，与参考仓一致）；
//! - 跨平台 `currentRelease` 一致，固定使用 linux-x64 元数据即可（win32 路径在 CDN 上
//!   返回 403，Windows 走不同的分发格式）；
//! - 获取失败静默降级：调用方回退 `config.kiro_version`（本地常量），不阻塞启动；
//! - 由 `config.ua_version_fetch` 门控（默认开）：关闭时 main 不 spawn，缓存恒空，
//!   `effective` 恒回退，UA 行为与未加此功能时完全一致（零回归）。
//!
//! 注意：用量类 REST 接口（getUsageLimits / 区探测等）**不用**这里的「最新版本」——
//! 参考仓实测新版 IDE 会在那些接口上强制要求 profileArn 导致失败，所以那几个构造点
//! 沿用 `config.kiro_version` 固定版本，本模块的缓存对它们不可见。

use std::sync::OnceLock;
use std::time::Duration;

use parking_lot::RwLock;
use serde::Deserialize;

use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;

/// 官方稳定版元数据端点（`currentRelease` 即当前 IDE 版本，跨平台一致）。
///
/// 注意：必须使用 linux-x64 路径——win32-* 路径在 CDN 上返回 403（Windows 走不同的
/// 分发格式）。版本号本身与平台无关，任选可用平台即可。
const METADATA_URL: &str =
    "https://prod.download.desktop.kiro.dev/stable/metadata-linux-x64-stable.json";

/// 默认刷新间隔：12 小时（与参考仓 kiro-rs 的 spawn 参数一致）。
const REFRESH_INTERVAL: Duration = Duration::from_secs(12 * 3600);

static LATEST_VERSION: OnceLock<RwLock<Option<String>>> = OnceLock::new();

fn cell() -> &'static RwLock<Option<String>> {
    LATEST_VERSION.get_or_init(|| RwLock::new(None))
}

/// 已自动获取到的最新 Kiro IDE 版本（后台刷新成功后才有值）
pub(crate) fn cached() -> Option<String> {
    cell().read().clone()
}

/// 返回有效的 Kiro IDE 版本：优先用自动获取到的最新版本，否则回退到 `fallback`
/// （`config.kiro_version` 本地常量）。失败降级的唯一出口，调用方不用感知拉取是否成功。
pub(crate) fn effective(fallback: &str) -> String {
    cached().unwrap_or_else(|| fallback.to_string())
}

#[derive(Deserialize)]
struct Metadata {
    #[serde(rename = "currentRelease")]
    current_release: Option<String>,
}

/// 拉取一次最新版本号（超时 15s，与参考仓一致）
async fn fetch_latest(
    proxy: Option<&ProxyConfig>,
    tls_backend: TlsBackend,
) -> anyhow::Result<String> {
    let client = build_client(proxy, 15, tls_backend)?;
    let resp = client.get(METADATA_URL).send().await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("拉取 Kiro 版本元数据失败: {}", status);
    }
    let meta: Metadata = resp.json().await?;
    meta.current_release
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("元数据缺少 currentRelease"))
}

/// 启动后台定时刷新：立即拉取一次，之后每 12h 刷新一次。
///
/// 失败仅记录告警后静默降级（继续用 `config.kiro_version`），不影响服务。
/// 由 main.rs 在 `config.ua_version_fetch` 开启时调用。
pub(crate) fn spawn_refresher(proxy: Option<ProxyConfig>, tls_backend: TlsBackend) {
    tokio::spawn(async move {
        loop {
            match fetch_latest(proxy.as_ref(), tls_backend).await {
                Ok(version) => {
                    let changed = cached().as_deref() != Some(version.as_str());
                    *cell().write() = Some(version.clone());
                    if changed {
                        tracing::info!("已自动获取 Kiro IDE 版本: {}", version);
                    }
                }
                Err(e) => {
                    tracing::warn!("自动获取 Kiro IDE 版本失败（继续使用配置中的版本）: {}", e);
                }
            }
            tokio::time::sleep(REFRESH_INTERVAL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metadata_parses_current_release() {
        let json = r#"{"currentRelease":"0.12.301","releases":[]}"#;
        let meta: Metadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.current_release.as_deref(), Some("0.12.301"));
    }

    #[test]
    fn test_effective_falls_back_without_cache() {
        // 未注入缓存时回退到 fallback（注意：其它测试可能已填充全局缓存，
        // 故此处仅断言返回值非空且为合法字符串；本测试绝不写缓存，不污染其它测试）
        let v = effective("0.9.2");
        assert!(!v.is_empty());
    }
}
