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
                std::thread::sleep(std::time::Duration::from_millis(
                    BASE_DELAY_MS * attempt as u64,
                ));
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

/// rename 成功后 fsync 父目录，让 dirent 本身落盘。文件内容已在 tmp 上
/// `sync_all`；不 fsync 目录的话崩溃窗口里 rename 可能丢（旧文件还在，新内容只在 tmp）。
/// Unix: 打开目录 fd 再 `sync_all`。失败只告警，不回退裸写。
/// Windows: `std::fs::File` 打不开目录；同卷 NTFS rename 已是元数据原子。跳过。
fn fsync_parent_dir(dir: &Path) {
    #[cfg(unix)]
    {
        match std::fs::File::open(dir) {
            Ok(f) => {
                if let Err(e) = f.sync_all() {
                    tracing::warn!("原子写后 fsync 父目录失败 {:?}: {}", dir, e);
                }
            }
            Err(e) => tracing::warn!("原子写后打开父目录失败 {:?}: {}", dir, e),
        }
    }
    #[cfg(not(unix))]
    {
        // Windows: cannot open a directory with std::fs::File; NTFS same-volume
        // rename of a flushed file is already metadata-atomic. Skip.
        let _ = dir;
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
        // ⚠️ **绝不回退成「原地截断写」**：`std::fs::write` 先 `File::create`（truncate）再写，
        // 而临时文件写失败几乎总是磁盘级原因（ENOSPC/EIO/权限），同一条件下再写目标极可能
        // 再失败一次 —— 留下空文件或半截 JSON，正好推翻本模块的承诺。目标又往往是
        // `credentials.json`（空文件下游会被静默当空池 ⇒ 号池全丢）/ `config.json`
        // （半截 ⇒ 启动直接 exit(1)）。宁可这次不写、保留最后一份完好文件：调用方会记
        // warning，内存态领先磁盘一版，下次 persist 自然重试。
        // 2026-10-14 审计（持久化面）发现的形态，旧代码此处是 `std::fs::write(&target, bytes)?`。
        tracing::warn!(
            "原子写临时文件失败，保留原文件不写（拒绝原地截断）: {:?}: {}",
            tmp,
            e
        );
        return Err(e);
    }

    match rename_with_retry(&tmp, &target) {
        Ok(()) => {
            restrict_permissions(&target);
            fsync_parent_dir(dir);
            Ok(())
        }
        Err(e) => {
            tracing::warn!(
                "原子 rename 重试耗尽，改用「先备份后原地写」回退: {:?} -> {:?}: {}",
                tmp,
                target,
                e
            );
            // 为什么不能直接 `std::fs::write(&target, bytes)`：它先截断目标再写，一旦写失败
            // 或进程在窗口内被杀，目标就变成空/半截 JSON（本模块承诺消灭的形态）。改为：
            // 先把旧目标 rename 成同目录 `.bak`（旧内容仍在），再原地写；写失败则把 .bak
            // 放回。最坏情况（写失败 + 放回也失败）仍留一份完好的 `.bak`，绝不出现「旧内容
            // 被截掉且新内容没写进去」。2026-10-14 审计（持久化面）发现的形态。
            let bak = dir.join(format!(".{}.bak", file_name));
            let had_old = target.exists();
            if had_old {
                let _ = std::fs::remove_file(&bak);
                if let Err(be) = std::fs::rename(&target, &bak) {
                    // 备份都失败（多半与 rename 同因：句柄占用/只读）→ 不动目标，保留旧文件。
                    tracing::warn!("回退备份失败，保留原文件不写: {:?}: {}", bak, be);
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
            }
            let result = std::fs::copy(&tmp, &target).map(|_| ());
            match &result {
                Ok(()) => {
                    restrict_permissions(&target);
                    let _ = std::fs::remove_file(&bak);
                }
                Err(ce) => {
                    tracing::warn!("回退原地写失败: {:?}: {}", target, ce);
                    if had_old {
                        // 尽力放回备份；失败也只是留下 .bak（人工可恢复），不会更糟。
                        let _ = std::fs::rename(&bak, &target);
                    } else {
                        // 原本没有目标文件：`fs::copy` 是「先建/截断目标再拷」，中途失败
                        // （ENOSPC/EIO）会留下**半截**新文件，而这条路径没有 .bak 可恢复。
                        // 删掉它，把状态还原成「目标不存在」——与本次写入前一致，也避免
                        // 下次启动把半截 JSON 当损坏文件（首次引导/config 生成场景）。
                        let _ = std::fs::remove_file(&target);
                    }
                }
            }
            let _ = std::fs::remove_file(&tmp);
            result
        }
    }
}

/// `write_atomic` 临时名：`.{stem}.{pid}.{seq}.tmp`，`pid`/`seq` 均为十进制数字。
/// `stem` 可含点（如 `config.json`）。不匹配 `foo.tmp`、`.env`、`.foo.tmp`。
fn is_write_atomic_tmp_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('.') else {
        return false;
    };
    let Some(inner) = rest.strip_suffix(".tmp") else {
        return false;
    };
    let mut parts = inner.rsplitn(3, '.');
    let seq = parts.next().unwrap_or("");
    let pid = parts.next().unwrap_or("");
    let stem = parts.next().unwrap_or("");
    !stem.is_empty()
        && !pid.is_empty()
        && !seq.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit())
        && seq.bytes().all(|b| b.is_ascii_digit())
}

/// 扫掉目录里 `write_atomic` 崩溃残留的隐藏 tmp（非递归，仅本层）。
///
/// 只删匹配 `.{stem}.{pid}.{seq}.tmp` 的普通文件。单文件失败 warn 后继续。
/// 目录不存在 → `Ok(0)`。返回成功删除的个数。
pub fn sweep_stale_tmp(dir: &Path) -> std::io::Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut deleted = 0usize;
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("扫残留 tmp 读目录项失败 {:?}: {}", dir, e);
                continue;
            }
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_write_atomic_tmp_name(name) {
            continue;
        }
        let path = entry.path();
        match entry.file_type() {
            Ok(ft) if ft.is_file() => {}
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!("扫残留 tmp 取类型失败 {:?}: {}", path, e);
                continue;
            }
        }
        match std::fs::remove_file(&path) {
            Ok(()) => deleted += 1,
            Err(e) => tracing::warn!("扫残留 tmp 删除失败 {:?}: {}", path, e),
        }
    }
    Ok(deleted)
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

    /// 临时文件写不出来时必须 **Err 且不动目标**（旧代码回退成原地截断写）。
    #[test]
    fn test_write_atomic_tmp_failure_returns_err_without_touching_target() {
        // 父目录不存在 ⇒ 建临时文件必然失败（ENOENT）。跨平台可复现。
        let dir = std::env::temp_dir().join(format!(
            "ks_fsatomic_missing_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let path = dir.join("data.json");
        let err = write_atomic(&path, b"new").expect_err("父目录缺失必须 Err");
        assert!(
            !path.exists(),
            "失败路径不得凭空造出目标文件: {:?}",
            err
        );
    }

    /// 临时文件写失败（只读目录）时，已存在的目标文件必须**逐字节保持原样**。
    /// Unix-only：Windows 的只读目录不拦文件创建，该场景在 Windows 上无法构造。
    #[cfg(unix)]
    #[test]
    fn test_write_atomic_tmp_failure_keeps_existing_content() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ks_fsatomic_ro_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        std::fs::write(&path, b"{\"old\":true}").unwrap();
        // 目录改只读 ⇒ 无法在其中创建 .tmp ⇒ 走失败分支。
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let r = write_atomic(&path, b"{\"new\":true}");
        // 恢复权限以便清理（即使断言失败也不留只读目录）。
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
        assert!(r.is_err(), "只读目录必须 Err");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"{\"old\":true}",
            "失败时旧内容必须逐字节保持（绝不原地截断）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_retryable_error_classification() {
        let perm = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(is_retryable_rename_error(&perm));
        let nf = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(!is_retryable_rename_error(&nf));
    }

    #[test]
    fn sweep_stale_tmp_removes_only_write_atomic_leftovers() {
        let dir = std::env::temp_dir().join(format!(
            "ks_fsatomic_sweep_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let leftover = dir.join(".data.json.123.0.tmp");
        std::fs::write(&leftover, b"stale").unwrap();
        let notes = dir.join("notes.tmp");
        std::fs::write(&notes, b"notes").unwrap();
        let env = dir.join(".env");
        std::fs::write(&env, b"SECRET=1").unwrap();
        let short = dir.join(".foo.tmp");
        std::fs::write(&short, b"nope").unwrap();
        let nested = dir.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let nested_tmp = nested.join(".data.json.123.0.tmp");
        std::fs::write(&nested_tmp, b"nested").unwrap();

        let n = sweep_stale_tmp(&dir).unwrap();
        assert_eq!(n, 1, "只删匹配 write_atomic 模式的本层文件");
        assert!(!leftover.exists());
        assert!(notes.exists(), "notes.tmp 不得删");
        assert!(env.exists(), ".env 不得删");
        assert!(short.exists(), ".foo.tmp 缺 pid.seq 不得删");
        assert!(nested_tmp.exists(), "不得递归进子目录");

        write_atomic(&dir.join("data.json"), b"hello-atomic").unwrap();
        assert_eq!(std::fs::read(dir.join("data.json")).unwrap(), b"hello-atomic");
        let leftover_now: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(is_write_atomic_tmp_name)
                    .unwrap_or(false)
            })
            .collect();
        assert!(leftover_now.is_empty(), "write_atomic 成功后不得残留自身 tmp");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_stale_tmp_missing_dir_returns_zero() {
        let dir = std::env::temp_dir().join(format!(
            "ks_fsatomic_sweep_missing_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!dir.exists());
        assert_eq!(sweep_stale_tmp(&dir).unwrap(), 0);
    }

    /// 父目录 fsync 是尽力而为：临时目录上不得 panic，write_atomic 仍成功。
    #[test]
    fn fsync_parent_dir_does_not_panic_on_temp_dir() {
        let dir = std::env::temp_dir().join(format!("ks_fsatomic_fsync_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        fsync_parent_dir(&dir);
        let path = dir.join("data.json");
        write_atomic(&path, b"after-dir-fsync").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"after-dir-fsync");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
