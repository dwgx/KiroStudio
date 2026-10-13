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

        // 原子写入文件。⚠️ 2026-10-14：**不用** `block_in_place` —— 判据不能是
        // `Handle::try_current().is_ok()`：**current_thread runtime 的 executor 线程**上
        // `block_in_place` 会 panic（「can call blocking only when running on the
        // multi-threaded runtime」）；`spawn_blocking` 线程上它是 **no-op**（`try_current()`
        // 返 Ok 但不 panic），普通 OS 线程上 `try_current()` 为 Err —— 即该判据既漏掉会 panic
        // 的 current_thread 情形，又在不 panic 的线程上做了无意义的判断（2026-10-14 用本仓
        // tokio rlib 实测澄清；此前本处注释把 spawn_blocking 也说成 panic，是错的）。
        // 本函数是凭据落盘路径，panic 发生在快照取完之后 ⇒ 内存里刚做的禁用/加号**没落盘**
        // 且调用方拿到的是 panic 而不是可处理的 Err。
        // 与 `cooldown.rs` 的 save（2026-08-13 对抗审查 M5）同一处置：文件小（KB 级）、
        // `write_atomic` 的 rename 重试上界 6 次 × 10~50ms（≈150ms 最坏），同步写阻塞 worker
        // 可接受，换来的是「任何 runtime 形态下都不会 panic」。
        write_atomic(path, &bytes).with_context(|| format!("回写凭据文件失败: {:?}", path))?;

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
        // 可观测：与 `persist_credentials` **同口径**置健康标志。trash 里同样是完整凭据
        // （refreshToken / apiKey / 代理口令），而 `encode_for_disk` 在密钥文件或 AEAD 失败时
        // 会**静默回退明文**、只返回 `encrypted=false` —— 少了这一句，trash.json 明文落盘
        // 时面板的 at-rest 健康标志仍显示正常（那一侧只被 credentials 的写入更新）。
        //
        // ⚠️ 语义是 **last-writer-wins**：本标志表达「**最近一次**落盘是否符合加密预期」，
        // 不是「历史上有没有回退过」。后一次成功写会把标志翻回健康 —— 这是**有意**的，
        // 不是漏掉的粘性：每次落盘都是整文件原子替换（`write_atomic`），一次成功写之后
        // 磁盘上那份文件确实已是密文，此时继续报红就是谎报；而粘性标志一旦置脏便永不
        // 自愈，运维修好密钥文件、后续写全部成功之后面板仍永远告警 —— 清不掉的告警会被
        // 无视，真正的新故障反而漏掉。「曾经回退过几次」要问的是独立的计数/时间戳 ——
        // 回退也不是无痕的（`encode_for_disk` 会 `tracing::error!` 打出失败原因），
        // 详见 `recovery_metrics::AT_REST_HEALTHY` 文档。
        //
        // 残留缺口（已知且接受）：trash 回退明文、随后 credentials 成功写，会把标志翻回
        // 健康，而 trash.json 那份明文要等它自己下一次写盘才被重新加密 —— 单一全局布尔量
        // 表达不了逐文件的健康，故这里只承诺「最近一次落盘」这个口径。
        crate::common::recovery_metrics::set_at_rest_healthy(!enc || encrypted);

        // 原子写入文件。⚠️ 理由与 `persist_credentials` 同处逐字一致：不用 `block_in_place`
        // （`Handle::try_current()` 不是安全判据，见该处注释）。
        write_atomic(&path, &bytes)
            .with_context(|| format!("回写回收站文件失败: {:?}", path))?;

        tracing::debug!(
            "已回写回收站到文件: {:?}(加密开关={} 实际加密={})",
            path,
            enc,
            encrypted
        );
        Ok(true)
    }
}
