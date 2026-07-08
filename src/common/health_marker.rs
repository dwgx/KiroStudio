//! OTA 启动健康标记 / crashloop 回滚兜底（阶段A）
//!
//! 与 systemd `ExecStartPre` 回滚守卫脚本（`deploy/rollback-guard.sh`）配合，构成
//! 「新版启动即崩 → 自动回滚 `.bak` 旧版」的闭环。回滚决策放 systemd 层（绝不放
//! 可能已崩的进程自己，见 docs/RESEARCH-HOTRELOAD-ARCH-0708 §3.2 方案 A）。
//!
//! 进程侧只负责两件事，均基于 exe 同目录的标记文件（路径与 update.rs 的
//! `with_extension` 约定一致，`.bak` 即 OTA 备份点）：
//!
//! 1. **bind 成功即清零 `.boot_attempts`**：证明本次已越过 config/凭据/端口 bind 三道
//!    启动门，不是「启动阶段就崩」。守卫脚本靠这个计数区分 crashloop 与「健康后被正常
//!    一键重启」——只有连 bind 都到不了的崩溃才会让计数跨重启累积。
//! 2. **稳定运行 N 秒后写 `.health` 并删 `.bak`**：进程活过观测窗口即视为可信，删掉
//!    回滚点（无需再留），并落一个含版本/时间戳的健康标记供 `/update/status` 观测。
//!
//! 标记文件（均在 exe 同目录）：
//! - `<exe>.boot_attempts`：纯文本计数器，守卫脚本 +1、bind 成功清零。
//! - `<exe>.bak`：OTA 备份的旧版二进制，健康后删除。
//! - `<exe>.health`：健康确认标记（版本+unix 时间戳）。
//!
//! 平台：标记逻辑仅在 Unix 生产部署有意义（systemd + rename 运行中 exe）。非 Unix
//! （本机 Windows 开发）下 `current_exe` 仍可用，但守卫脚本不存在，写标记无副作用。

use std::path::PathBuf;
use std::time::Duration;

/// 健康观测窗口：进程活过这么久即视为本版可信，删除回滚点。
/// 取值权衡：足够长以覆盖启动后立即崩的场景（config 迟加载、首请求 panic 多在数秒内），
/// 又不至于让正常升级长时间保留 `.bak`。
const HEALTH_CONFIRM_DELAY_SECS: u64 = 30;

/// 由当前 exe 路径派生一个同目录的兄弟标记文件路径（`<exe>.<ext>`）。
/// 与 `src/admin/update.rs` 的 `exe.with_extension("bak"/"new")` 约定一致。
fn marker_path(ext: &str) -> Option<PathBuf> {
    std::env::current_exe().ok().map(|exe| exe.with_extension(ext))
}

/// bind 成功后立即调用：清零启动计数器，向守卫脚本表明「已越过启动门，非 crashloop」。
///
/// 计数器不存在（首次部署 / 无守卫脚本）时静默返回——缺失即视为 0，绝不因此报错。
pub fn clear_boot_attempts() {
    let Some(path) = marker_path("boot_attempts") else {
        return;
    };
    match clear_counter_at(&path) {
        Ok(existed) if existed => {
            tracing::debug!("已清零启动计数器 {:?}（bind 成功，非 crashloop）", path)
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("清零启动计数器 {:?} 失败（不影响运行）: {}", path, e),
    }
}

/// 删除计数器文件（等价清零）。返回文件是否曾存在。NotFound 视为成功（已是 0）。
/// 抽成独立函数便于单测（不依赖 current_exe）。
fn clear_counter_at(path: &std::path::Path) -> std::io::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// 稳定运行 N 秒后写 `.health` 标记并删除 `.bak` 回滚点。
///
/// spawn 一个后台任务：睡 [`HEALTH_CONFIRM_DELAY_SECS`] 秒——若进程在此期间崩溃，
/// systemd 会重启且守卫脚本靠 `.boot_attempts`/`.bak` 判定回滚；若活过窗口，说明本版
/// 可信，写健康标记 + 删 `.bak`（不再需要回滚点）。
///
/// `version` 落进 `.health` 供 `/update/status` 观测。
pub fn spawn_health_confirm(version: String) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(HEALTH_CONFIRM_DELAY_SECS)).await;

        // 写 .health（版本 + unix 秒），供前端/运维确认「已稳定升级到 vX」。
        if let Some(health) = marker_path("health") {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let body = format!("version={version}\nconfirmed_at={ts}\n");
            if let Err(e) = std::fs::write(&health, body) {
                tracing::warn!("写健康标记 {:?} 失败（不影响运行）: {}", health, e);
            } else {
                tracing::info!("已写健康标记 {:?}（本版稳定运行 {}s，可信）", health, HEALTH_CONFIRM_DELAY_SECS);
            }
        }

        // 删 .bak：本版已可信，回滚点无需再留（也避免下次 OTA 前陈旧 .bak 干扰判定）。
        if let Some(bak) = marker_path("bak") {
            match std::fs::remove_file(&bak) {
                Ok(()) => tracing::info!("已删除回滚点 {:?}（本版可信）", bak),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!("删除回滚点 {:?} 失败（不影响运行）: {}", bak, e),
            }
        }
    });
}

/// 读健康标记状态（供 `/api/admin/update/status` 观测端点）。
///
/// 返回 `(health_present, health_body, has_failed_binary)`：
/// - `.health` 是否存在 + 其内容（版本/时间戳）
/// - 同目录是否有 `kirostudio.failed.*`（守卫脚本回滚时把坏版重命名留证）
pub fn read_status() -> HealthStatus {
    let health_body = marker_path("health").and_then(|p| std::fs::read_to_string(p).ok());
    let has_bak = marker_path("bak").map(|p| p.exists()).unwrap_or(false);

    // 扫描 exe 同目录有无 *.failed.* 残留（回滚证据）。
    let has_failed_binary = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.to_path_buf()))
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains(".failed."))
        })
        .unwrap_or(false);

    HealthStatus {
        health_confirmed: health_body.is_some(),
        health_detail: health_body,
        rollback_point_present: has_bak,
        rolled_back_binary_present: has_failed_binary,
    }
}

/// `/update/status` 观测端点的健康快照。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthStatus {
    /// `.health` 是否已写（本版已稳定确认）
    pub health_confirmed: bool,
    /// `.health` 原文（version=.. / confirmed_at=..）
    pub health_detail: Option<String>,
    /// `.bak` 回滚点是否还在（健康后应被删除；仍在=尚未确认或备份未清）
    pub rollback_point_present: bool,
    /// 是否有 `*.failed.*` 残留（守卫脚本执行过回滚的证据）
    pub rolled_back_binary_present: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clear_counter_removes_existing() {
        let dir = std::env::temp_dir().join(format!("ks_hm_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let counter = dir.join("kirostudio.boot_attempts");
        std::fs::write(&counter, "3").unwrap();
        assert!(counter.exists());

        let existed = clear_counter_at(&counter).unwrap();
        assert!(existed, "清零应报告文件曾存在");
        assert!(!counter.exists(), "计数器文件应被删除（等价清零）");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_clear_counter_missing_is_ok() {
        // 首次部署 / 无守卫脚本：计数器不存在，清零应静默成功（视为已是 0）
        let dir = std::env::temp_dir().join(format!("ks_hm_missing_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let counter = dir.join("kirostudio.boot_attempts");
        assert!(!counter.exists());

        let existed = clear_counter_at(&counter).unwrap();
        assert!(!existed, "缺失时应报告文件不曾存在，且不报错");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_health_status_serializes_camel_case() {
        // 观测端点 DTO 应输出 camelCase 供前端消费
        let s = HealthStatus {
            health_confirmed: true,
            health_detail: Some("version=0.2.2\nconfirmed_at=1\n".to_string()),
            rollback_point_present: false,
            rolled_back_binary_present: false,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"healthConfirmed\":true"), "应为 camelCase: {json}");
        assert!(json.contains("\"rollbackPointPresent\":false"), "应为 camelCase: {json}");
    }
}

