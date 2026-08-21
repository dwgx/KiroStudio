//! Disk write for credentials.json and trash.json.
//! Child of `token_manager` (`#[path]`) so it can reach `MultiTokenManager` private fields.

use std::path::PathBuf;

use crate::common::fs_atomic::write_atomic;
use crate::kiro::model::credentials::{KiroCredentials, TrashEntry};

impl super::MultiTokenManager {
    pub(super) fn persist_credentials(&self) -> anyhow::Result<bool> {
        use anyhow::Context;

        // ⚠️ 2026-08-13 修复：Single 格式不再 no-op —— 内存里加号后若磁盘仍是单对象
        // 旧格式，persist 会静默跳过 ⇒ 重启即丢号（nbus 实测连丢两批：17:48/18:08/18:10
        // 与 22:21 加的 custom_api 号全部随重启消失）。现在**总是写数组格式**：
        // Single 加载的旧配置文件首次 persist 自动升级为数组（加载路径两种格式都认，
        // 旧版二进制也认数组，向后兼容）。is_multiple_format 参数保留仅为向后兼容
        // 调用点，不再参与持久化判定。

        let path = match &self.credentials_path {
            Some(p) => p,
            None => return Ok(false),
        };

        // 串行化写入：快照→序列化→落盘全程持 persist_lock。后到者排队后重取快照，
        // 消除「并发 persist、旧快照最后落盘」把刚禁用的号写回启用（SIGKILL 后死号复活）。
        // 锁序：persist_lock → entries；调用方必须先放 entries 再进本函数。
        let _persist = self.persist_lock.lock();

        // 收集所有凭据
        let credentials: Vec<KiroCredentials> = {
            let entries = self.entries.lock();
            entries
                .iter()
                .map(|e| {
                    let mut cred = e.credentials.clone();
                    cred.canonicalize_auth_method();
                    // ⭐ 同步 disabled 状态到凭据对象（#10 三处同步契约之「persist 全量写盘」）。
                    // 双份字段：entry 四件套（disabled/disabled_reason/disabled_at/quota_exhausted_at）
                    // 是**真源**，这里的 credentials 镜像只活到写盘；另两处同步是
                    // load 回填（`MultiTokenManager::new`）与 set_disabled 收口。
                    cred.disabled = e.disabled;
                    // ⭐ 同步禁用原因与时刻。不落盘这两项时，重启后加载路径会把所有禁用号
                    // 一律当成"手动禁用"，自动禁用原因（配额耗尽/被封/风控/连续失败）全部丢失，
                    // 且以 reason 为判据的自愈逻辑被一并击穿。
                    cred.disabled_reason = e.disabled_reason;
                    cred.disabled_at = e.disabled_at.clone();
                    cred.quota_exhausted_at = e.quota_exhausted_at.clone();
                    cred
                })
                .collect()
        };

        // 序列化为 pretty JSON
        let json = serde_json::to_string_pretty(&credentials).context("序列化凭据失败")?;

        // at-rest 加密(开关关=明文原样;开=持久化密钥文件加密。加密失败自动回退明文不丢数据,
        // 并置健康标志=false 让 UI 可观测"开了加密但这次实为明文")。
        let enc = self.config().encrypt_credentials_at_rest;
        let key_path = crate::common::secret_store::key_path_for(path);
        let (bytes, encrypted) =
            crate::common::secret_store::encode_for_disk(json.as_bytes(), enc, &key_path);
        // 可观测:开了加密但实际没加密成(密钥文件读写失败等)→ 记健康标志,消除安全预期偏差。
        crate::common::recovery_metrics::set_at_rest_healthy(!enc || encrypted);

        // 原子写入文件（在 Tokio runtime 内使用 block_in_place 避免阻塞 worker）
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| write_atomic(path, &bytes))
                .with_context(|| format!("回写凭据文件失败: {:?}", path))?;
        } else {
            write_atomic(path, &bytes).with_context(|| format!("回写凭据文件失败: {:?}", path))?;
        }

        tracing::debug!(
            "已回写凭据到文件: {:?}(加密开关={} 实际加密={})",
            path,
            enc,
            encrypted
        );
        Ok(true)
    }

    /// 回收站文件路径（cache_dir/trash.json）
    pub(super) fn trash_path(&self) -> Option<PathBuf> {
        self.cache_dir().map(|d| d.join("trash.json"))
    }

    /// 将回收站持久化到磁盘（仿 persist_credentials）
    ///
    /// # Returns
    /// - `Ok(true)` - 成功写入文件
    /// - `Ok(false)` - 跳过写入（非多凭据格式或无路径配置）
    /// - `Err(_)` - 写入失败
    pub(super) fn persist_trash(&self) -> anyhow::Result<bool> {
        use anyhow::Context;

        // 2026-08-13：与 load_trash 同口径，总是回写（不再按格式跳过）。
        let path = match self.trash_path() {
            Some(p) => p,
            None => return Ok(false),
        };

        // 串行化写入：快照→序列化→落盘全程持 persist_lock（与 persist_credentials 同锁）。
        // 后到者排队后重取快照，避免并发旧 trash 快照最后落盘。
        // 锁序：persist_lock → trash；调用方必须先放 trash/entries 再进本函数。
        let _persist = self.persist_lock.lock();

        let items: Vec<TrashEntry> = self.trash.lock().clone();

        // 序列化为 pretty JSON
        let json = serde_json::to_string_pretty(&items).context("序列化回收站失败")?;

        // at-rest 加密(trash 也含完整凭据敏感字段,与 credentials 同开关同口径,共用同一密钥文件)。
        let enc = self.config().encrypt_credentials_at_rest;
        let key_path = crate::common::secret_store::key_path_for(&path);
        let (bytes, encrypted) =
            crate::common::secret_store::encode_for_disk(json.as_bytes(), enc, &key_path);

        // 原子写入文件（在 Tokio runtime 内使用 block_in_place 避免阻塞 worker）
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| write_atomic(&path, &bytes))
                .with_context(|| format!("回写回收站文件失败: {:?}", path))?;
        } else {
            write_atomic(&path, &bytes)
                .with_context(|| format!("回写回收站文件失败: {:?}", path))?;
        }

        tracing::debug!(
            "已回写回收站到文件: {:?}(加密开关={} 实际加密={})",
            path,
            enc,
            encrypted
        );
        Ok(true)
    }
}
