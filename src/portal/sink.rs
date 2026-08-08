//! 推送记录的持久化入口（进程级单例）。
//!
//! # 为什么需要这一层
//! `POST /api/import/keys` 的处理函数（[`crate::import_api`]）拿不到 `PortalDb`——它的
//! State 里只有 `MultiTokenManager`，而给它塞一个 portal 依赖会把两个本该无关的模块焊在一起
//! （导入通道不该因为 portal 没开就少写东西，portal 也不该反向依赖导入通道的内部结构）。
//!
//! 故用一个进程级 `OnceLock<Arc<PortalDb>>`：main 装配 portal 时注册，导入路径调
//! [`record_import`] 写一条。**未注册时是静默 no-op**——portal 开库失败或未编入时，
//! 导入通道照常工作，一行代码都不受影响。这与 [`crate::common::import_stats`]（同样是
//! 进程级、导入路径单向调用）是同一套思路，只是那个存内存、这个落盘。
//!
//! # 与 import_stats 的分工
//! - [`crate::common::import_stats`]：最近 20 次**推送批次**的运营摘要，进程内存，重启归零。
//! - 本模块：按 **key** 聚合的持久元数据，重启仍在，供 portal 用户查历史。
//!
//! 两者并存不是重复：一个回答「今天推了几批、失败率如何」，一个回答「这个号什么时候进来的、
//! 推过几次」。前者是运维视角的时间线，后者是用户视角的清单。
//!
//! # 绝不落明文
//! 这里只写 [`ImportKeyMeta`]（指纹、`credential_id`、region、endpoint、时间戳）。
//! 明文由 portal 在**请求时**按 `credential_id` 从活凭据池回查，磁盘上不出现第二份副本。

use std::sync::{Arc, OnceLock};

use super::store::{ImportKeyMeta, PortalDb};

static DB: OnceLock<Arc<PortalDb>> = OnceLock::new();

/// 注册持久化目标（main 在装配 portal 时调用一次）。
///
/// 重复注册被忽略并返回 `false`——`OnceLock` 的语义决定第一次赢。这不是错误路径，
/// 正常启动只会调一次；返回值仅供调用方记日志。
pub fn register(db: Arc<PortalDb>) -> bool {
    DB.set(db).is_ok()
}

/// 是否已注册（供诊断/测试）。
pub fn is_registered() -> bool {
    DB.get().is_some()
}

/// 记一个 key 的推送元数据。**未注册时静默返回**。
///
/// 参数刻意与 [`crate::import_api`] 已有的字段一一对应，避免调用方做转换：
/// - `plain_key`：只用来算指纹和全摘要，**不存**。
/// - `credential_id`：落库成功才有；失败时为 `None`，upsert 的 `COALESCE` 会保住旧值。
///
/// 写失败只记日志：导入通道的成败绝不能取决于 portal 的库能不能写。
pub fn record_import(
    plain_key: &str,
    credential_id: Option<u64>,
    region: Option<&str>,
    endpoint: Option<&str>,
    ok: bool,
    error: Option<&str>,
    now_ms: i64,
) {
    let Some(db) = DB.get() else {
        return;
    };

    let meta = ImportKeyMeta {
        key_hash: crate::common::key_mask::key_hash_full(plain_key),
        fingerprint: crate::common::key_mask::key_fingerprint(plain_key),
        credential_id: credential_id.and_then(|id| i64::try_from(id).ok()),
        region: region.map(str::to_string),
        endpoint: endpoint.map(str::to_string),
        first_seen_ms: now_ms,
        last_seen_ms: now_ms,
        push_count: 1,
        ok,
        error: error.map(str::to_string),
    };

    if let Err(e) = db.upsert_import_key(&meta) {
        tracing::warn!(
            fingerprint = %meta.fingerprint,
            "Portal 推送元数据落盘失败（不影响导入结果）: {:#}",
            e
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 未注册时必须是安全的 no-op——否则 portal 开库失败会连带打挂导入通道。
    ///
    /// 【为何不测「注册后写入」】`DB` 是 `OnceLock`，一旦在某个用例里 set 就无法复位，
    /// 会污染同进程内其它用例（且谁先跑不确定）。注册后的写入路径由
    /// [`super::store`] 的 `upsert_import_key` 系列用例直接覆盖，那里能用独立的内存库，
    /// 覆盖更彻底。这里只锁「没注册也不炸」这一条本模块独有的性质。
    #[test]
    fn unregistered_is_silent_noop() {
        if is_registered() {
            // 别的用例已注册过（`OnceLock` 不可复位），本用例的前提不再成立，直接跳过。
            // 不 panic：这不是产品缺陷，是同进程测试的固有限制。
            return;
        }
        record_import(
            "ksk_example",
            Some(1),
            Some("us-east-1"),
            None,
            true,
            None,
            1000,
        );
        record_import("ksk_example2", None, None, None, false, Some("boom"), 2000);
    }
}
