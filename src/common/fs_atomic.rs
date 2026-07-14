//! 原子文件写:temp → fsync → rename 的持久化保证(单一真相源)。
//!
//! # 为什么需要
//! 裸 `std::fs::write(path, bytes)` 先截断目标再写——若进程在截断与写完之间被杀
//! (SIGKILL/OOM/断电/panic),目标文件被留成空或半截。下次启动 `serde_json::from_str`
//! 失败,加载逻辑静默回退默认值 → 用户丢失全部持久化设置(凭据/config/adminApiKey)。
//!
//! # 保证(K7 durability)
//! 把新内容先写到同目录唯一临时文件 → `fsync` 强制落盘(bytes 真到盘面,不只在 page cache)
//! → `rename` 原子替换目标。崩溃在 fsync 与 rename 之间 → 目标完好(旧内容);崩溃在 rename
//! 之后 → 目标是已 fsync 的新内容。任一情况都不会出现截断 JSON。
//!
//! # 与旧散落实现的关系
//! 历史上 token_manager.rs 有一份内联 `write_atomic`。本模块把它提为共享单一真相源,
//! 供 config.rs(adminApiKey/proxyPassword 明文)、凭据、回收站等所有敏感持久化复用,
//! 并补上 Windows 句柄占用的 rename 重试(旧实现失败即降级裸写)。

use std::path::{Path, PathBuf};

/// 原子写失败时可重试的 rename 场景(Windows 上杀软/索引器/编辑器瞬时持有目标句柄):
/// `ERROR_ACCESS_DENIED`(5)/`ERROR_SHARING_VIOLATION`(32)。Unix 上一般不触发。
fn is_retryable_rename_error(e: &std::io::Error) -> bool {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        return true;
    }
    matches!(e.raw_os_error(), Some(5) | Some(32))
}

/// rename 带短退避重试:对可重试错误码(Windows 句柄占用)做几次指数退避,耗尽再返回错误。
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    const MAX_ATTEMPTS: u32 = 6;
    const BASE_DELAY_MS: u64 = 10;
    let mut attempt = 1u32;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if attempt >= MAX_ATTEMPTS || !is_retryable_rename_error(&e) {
                    return Err(e);
                }
                std::thread::sleep(std::time::Duration::from_millis(BASE_DELAY_MS * attempt as u64));
                attempt += 1;
            }
        }
    }
}

/// 将文件权限收紧为仅属主可读写(Unix 0600);非 Unix 无操作。
/// 敏感文件(含 token/adminApiKey)的纵深防护:即便走了裸写回退路径(默认受 umask 影响可能
/// 0644),也把最终文件权限拉回 0600,失败仅告警不致命。
pub fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!("收紧文件权限失败 {:?}: {}", path, e);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// 原子写:temp → fsync → rename,创建即 0600,rename 失败重试后回退裸写。
///
/// 关键点:
/// - 临时文件放目标**同目录**(保证同一文件系统,`rename` 才原子);
/// - 若 `path` 是软链,先 `canonicalize` 拿真身路径再 rename,避免把软链替换成普通文件;
/// - Windows 目标被占用:rename 先做可重试退避(见 [`rename_with_retry`]),耗尽才回退裸写并告警,
///   绝不让整体持久化失败。
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    // 解析真实目标路径:软链要写到它指向的真身;不存在(首次写入)则用原 path。
    let target: PathBuf = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let file_name = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("data");
    // 同目录隐藏临时文件:文件名带 pid + 进程内单调递增序号,既避免跨进程碰撞,
    // 也避免同进程内两个并发持久化争抢同一 tmp 互相截断。
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{}.{}.{}.tmp", file_name, std::process::id(), seq));

    // 写临时文件并尽力落盘。安全:创建即以 0600 打开,rename 后目标继承该权限,
    // 杜绝默认 umask 造成的 0644 本地泄露(无 rename 后再 set_permissions 的短窗口)。
    let write_tmp = || -> std::io::Result<()> {
        let mut f = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&tmp)?
            }
            #[cfg(not(unix))]
            {
                std::fs::File::create(&tmp)?
            }
        };
        f.write_all(bytes)?;
        f.flush()?;
        // 尽力 sync,失败不致命(部分平台/文件系统可能不支持)。
        let _ = f.sync_all();
        Ok(())
    };

    if let Err(e) = write_tmp() {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!("原子写临时文件失败,回退直接写: {:?}: {}", tmp, e);
        std::fs::write(&target, bytes)?;
        restrict_permissions(&target);
        return Ok(());
    }

    match rename_with_retry(&tmp, &target) {
        Ok(()) => {
            restrict_permissions(&target);
            Ok(())
        }
        Err(e) => {
            tracing::warn!("原子 rename 重试耗尽,回退直接写: {:?} -> {:?}: {}", tmp, target, e);
            let result = std::fs::write(&target, bytes);
            if result.is_ok() {
                restrict_permissions(&target);
            }
            let _ = std::fs::remove_file(&tmp);
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_atomic_writes_content_and_no_tmp_residue() {
        let dir = std::env::temp_dir().join(format!("ks_fsatomic_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.json");
        write_atomic(&path, b"hello-atomic").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello-atomic");
        // 无 .tmp 残留
        let residue: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(residue.is_empty(), "不应残留 tmp 文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_atomic_overwrites_existing_file() {
        let dir = std::env::temp_dir().join(format!("ks_fsatomic_ovr_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.json");
        std::fs::write(&path, b"old-content-longer").unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_retryable_error_classification() {
        let perm = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(is_retryable_rename_error(&perm));
        let nf = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(!is_retryable_rename_error(&nf));
    }
}
