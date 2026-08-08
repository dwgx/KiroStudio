//! 外部凭据推送（`POST /api/import/keys`）的可观测记录（进程级，不持久化）。
//!
//! # 为什么需要
//! 导入接口是**对外**入口：推送方按自己的节奏调用，成功/失败只落容器日志。运维侧因此
//! 回答不了「今天推了几次 / 进了几个新号 / 哪个 key 失败了为什么」——只能 grep 日志。
//! 本模块把每次推送收敛成一条摘要 + 一组累计计数，让面板能直接看。
//!
//! # 设计（与 [`super::recovery_metrics`] 同源）
//! - **不持久化**：这是「自进程启动以来」的运营信号，重启归零；凭据本身已落 credentials.json。
//! - **有界内存**：只留最近 [`MAX_RECORDS`] 次推送，`VecDeque` 满则弹最旧，杜绝无界增长。
//! - **两条通道隔离**：既有批量通道保持 admin 面板可复制明文的历史行为；新 Relay
//!   通道只记录打码 key。两者使用独立队列与计数器，互不覆盖。

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

/// 保留的最近推送次数上限（有界内存，满则弹最旧）。
const MAX_RECORDS: usize = 20;

/// 每条推送记录里保留的明细条目上限——一次可推 1000 个 key，全存会让快照 JSON 过大。
/// 优先保留失败项（运维只关心失败原因），成功项只留计数。
const MAX_ITEMS_PER_RECORD: usize = 20;

/// 单个 key 的处置结果（明细，仅用于面板展示）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportItemRecord {
    /// 展示用 key。既有批量通道保持完整值；Relay 通道仅写入打码值。
    pub key: String,
    /// key 指纹（SHA-256 前 8 位）。用于和凭据管理页的指纹对照同一个号，
    /// 也便于在不整串比对的情况下快速判同。恒可计算，故非 Option。
    pub fingerprint: String,
    pub ok: bool,
    /// 该 key 本已存在（幂等命中）。
    pub duplicate: bool,
    /// 落库后的凭据 ID，供与推送方对账。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<u64>,
    /// 失败原因（成功时为 None）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// **推送方在请求里发来的 region 原值**。`None` = 对方没发这个字段。
    ///
    /// 与 [`Self::region`]（我们最终落库的值）分开记录：只看落库值分不清
    /// 「对方指定了 us-east-1」和「对方没发、我们探测出 us-east-1」，
    /// 而这两种情况在排查路由问题时含义完全不同。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent_region: Option<String>,
    /// **推送方发来的 endpoint 原值**。`None` = 对方没发（契约示例即 `null`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent_endpoint: Option<String>,
    /// **推送方发来的 groups 原值**，照实回显（契约称固定空数组，但不替它假设）。
    pub sent_groups: Vec<String>,
    /// Relay 单条协议的投递 ID；批量导入协议没有此字段。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_id: Option<String>,
    /// 最终落库的 region（探测或重用的结果）。面板显示用——运维要能看出号被路由到哪。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// 最终落库的 endpoint。`None` = 未指定，运行时回退 `config.defaultEndpoint`。
    ///
    /// 【为何必须可见】93 号那次落成 `cli` 导致上游 400 + 解码器崩溃，但面板上只显示
    /// key 和 #id，完全看不出路由是坏的。把它摆到明面上，同类问题一眼可辨。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

/// 一次推送（= 一个 HTTP 请求）的摘要。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportRecord {
    /// Unix 毫秒时间戳（前端按本地时区渲染）。
    pub at_ms: u64,
    pub total: usize,
    /// 新建的凭据数。
    pub imported: usize,
    /// 幂等命中、只更新元数据的数量。
    pub duplicates: usize,
    pub failed: usize,
    /// 本次耗时（含 region 探测），用于判断是否逼近推送方的 300s 超时。
    pub elapsed_ms: u64,
    /// 明细（失败项优先，最多 [`MAX_ITEMS_PER_RECORD`] 条）。
    pub items: Vec<ImportItemRecord>,
    /// 因超出上限而未收录的明细条数。
    pub omitted: usize,
}

static PUSHES: AtomicU64 = AtomicU64::new(0);
static KEYS_TOTAL: AtomicU64 = AtomicU64::new(0);
static KEYS_IMPORTED: AtomicU64 = AtomicU64::new(0);
static KEYS_DUPLICATE: AtomicU64 = AtomicU64::new(0);
static KEYS_FAILED: AtomicU64 = AtomicU64::new(0);
static LAST_AT_MS: AtomicU64 = AtomicU64::new(0);
static RELAY_PUSHES: AtomicU64 = AtomicU64::new(0);
static RELAY_KEYS_TOTAL: AtomicU64 = AtomicU64::new(0);
static RELAY_KEYS_IMPORTED: AtomicU64 = AtomicU64::new(0);
static RELAY_KEYS_DUPLICATE: AtomicU64 = AtomicU64::new(0);
static RELAY_KEYS_FAILED: AtomicU64 = AtomicU64::new(0);
static RELAY_LAST_AT_MS: AtomicU64 = AtomicU64::new(0);

fn records() -> &'static Mutex<VecDeque<ImportRecord>> {
    static RECORDS: OnceLock<Mutex<VecDeque<ImportRecord>>> = OnceLock::new();
    RECORDS.get_or_init(|| Mutex::new(VecDeque::with_capacity(MAX_RECORDS)))
}

fn relay_records() -> &'static Mutex<VecDeque<ImportRecord>> {
    static RECORDS: OnceLock<Mutex<VecDeque<ImportRecord>>> = OnceLock::new();
    RECORDS.get_or_init(|| Mutex::new(VecDeque::with_capacity(MAX_RECORDS)))
}

/// 当前 Unix 毫秒（系统时间倒退时回 0，不 panic）。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 记一次推送。`items` 为全部条目，内部会按「失败优先」裁剪到上限后入队。
pub fn record_push(items: Vec<ImportItemRecord>, elapsed_ms: u64) {
    record_into(
        items,
        elapsed_ms,
        &PUSHES,
        &KEYS_TOTAL,
        &KEYS_IMPORTED,
        &KEYS_DUPLICATE,
        &KEYS_FAILED,
        &LAST_AT_MS,
        records(),
    );
}

/// Record one Relay `/push` request separately from the legacy batch-import channel.
pub fn record_relay_push(items: Vec<ImportItemRecord>, elapsed_ms: u64) {
    record_into(
        items,
        elapsed_ms,
        &RELAY_PUSHES,
        &RELAY_KEYS_TOTAL,
        &RELAY_KEYS_IMPORTED,
        &RELAY_KEYS_DUPLICATE,
        &RELAY_KEYS_FAILED,
        &RELAY_LAST_AT_MS,
        relay_records(),
    );
}

#[allow(clippy::too_many_arguments)]
fn record_into(
    items: Vec<ImportItemRecord>,
    elapsed_ms: u64,
    pushes: &AtomicU64,
    keys_total: &AtomicU64,
    keys_imported: &AtomicU64,
    keys_duplicate: &AtomicU64,
    keys_failed: &AtomicU64,
    last_at_ms: &AtomicU64,
    queue: &Mutex<VecDeque<ImportRecord>>,
) {
    let total = items.len();
    let imported = items.iter().filter(|i| i.ok && !i.duplicate).count();
    let duplicates = items.iter().filter(|i| i.ok && i.duplicate).count();
    let failed = items.iter().filter(|i| !i.ok).count();

    pushes.fetch_add(1, Ordering::Relaxed);
    keys_total.fetch_add(total as u64, Ordering::Relaxed);
    keys_imported.fetch_add(imported as u64, Ordering::Relaxed);
    keys_duplicate.fetch_add(duplicates as u64, Ordering::Relaxed);
    keys_failed.fetch_add(failed as u64, Ordering::Relaxed);
    let at_ms = now_ms();
    last_at_ms.store(at_ms, Ordering::Relaxed);

    let mut kept: Vec<ImportItemRecord> = items.iter().filter(|i| !i.ok).cloned().collect();
    if kept.len() < MAX_ITEMS_PER_RECORD {
        kept.extend(
            items
                .iter()
                .filter(|i| i.ok)
                .take(MAX_ITEMS_PER_RECORD - kept.len())
                .cloned(),
        );
    }
    kept.truncate(MAX_ITEMS_PER_RECORD);
    let omitted = total.saturating_sub(kept.len());

    let mut queue = queue.lock();
    if queue.len() == MAX_RECORDS {
        queue.pop_front();
    }
    queue.push_back(ImportRecord {
        at_ms,
        total,
        imported,
        duplicates,
        failed,
        elapsed_ms,
        items: kept,
        omitted,
    });
}

/// 导出快照供 `/api/admin/import-stats` 端点。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportStatsSnapshot {
    /// 导入接口是否已配置 `importApiKey` 并启用。
    pub enabled: bool,
    /// Relay 单条推送频道是否已配置专用密钥。
    pub relay_enabled: bool,
    pub relay_pushes: u64,
    pub relay_keys_total: u64,
    pub relay_keys_imported: u64,
    pub relay_keys_duplicate: u64,
    pub relay_keys_failed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay_last_at_ms: Option<u64>,
    pub relay_records: Vec<ImportRecord>,
    pub pushes: u64,
    pub keys_total: u64,
    pub keys_imported: u64,
    pub keys_duplicate: u64,
    pub keys_failed: u64,
    /// 最近一次推送的 Unix 毫秒；从未推送过为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_at_ms: Option<u64>,
    /// 最近若干次推送，新的在前。
    pub records: Vec<ImportRecord>,
}

/// 导入接口是否已启用（启动时按 `importApiKey` 是否配置写入一次）。
static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static RELAY_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 启动接线时调用：把「导入接口是否启用」告知本模块，供面板区分
/// 「未启用」与「已启用但还没人推过」——两者都表现为零计数，但处置完全不同。
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn set_relay_enabled(enabled: bool) {
    RELAY_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn snapshot() -> ImportStatsSnapshot {
    let last = LAST_AT_MS.load(Ordering::Relaxed);
    let mut list: Vec<ImportRecord> = records().lock().iter().cloned().collect();
    list.reverse(); // 新的在前，前端直接渲染
    let mut relay_list: Vec<ImportRecord> = relay_records().lock().iter().cloned().collect();
    relay_list.reverse();
    let relay_last = RELAY_LAST_AT_MS.load(Ordering::Relaxed);
    ImportStatsSnapshot {
        enabled: ENABLED.load(Ordering::Relaxed),
        relay_enabled: RELAY_ENABLED.load(Ordering::Relaxed),
        relay_pushes: RELAY_PUSHES.load(Ordering::Relaxed),
        relay_keys_total: RELAY_KEYS_TOTAL.load(Ordering::Relaxed),
        relay_keys_imported: RELAY_KEYS_IMPORTED.load(Ordering::Relaxed),
        relay_keys_duplicate: RELAY_KEYS_DUPLICATE.load(Ordering::Relaxed),
        relay_keys_failed: RELAY_KEYS_FAILED.load(Ordering::Relaxed),
        relay_last_at_ms: (relay_last > 0).then_some(relay_last),
        relay_records: relay_list,
        pushes: PUSHES.load(Ordering::Relaxed),
        keys_total: KEYS_TOTAL.load(Ordering::Relaxed),
        keys_imported: KEYS_IMPORTED.load(Ordering::Relaxed),
        keys_duplicate: KEYS_DUPLICATE.load(Ordering::Relaxed),
        keys_failed: KEYS_FAILED.load(Ordering::Relaxed),
        last_at_ms: (last > 0).then_some(last),
        records: list,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(ok: bool, duplicate: bool) -> ImportItemRecord {
        ImportItemRecord {
            key: "ksk_abcd...wxyz".to_string(),
            fingerprint: "a3f7c9d1".to_string(),
            ok,
            duplicate,
            credential_id: ok.then_some(1),
            error: (!ok).then(|| "boom".to_string()),
            region: Some("us-east-1".to_string()),
            endpoint: None,
            sent_region: None,
            sent_endpoint: None,
            sent_groups: Vec::new(),
            delivery_id: None,
        }
    }

    /// 统计是进程级静态量，用例并行会互相覆盖/挤出记录，故本模块用例串行执行。
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 明细超上限时优先留失败项——运维只关心失败原因，成功项有计数就够。
    #[test]
    fn keeps_failed_items_first_when_over_limit() {
        let _guard = test_lock();
        let mut items: Vec<ImportItemRecord> = (0..30).map(|_| item(true, false)).collect();
        items.push(item(false, false));
        record_push(items, 10);

        // 静态队列在测试间共享且用例并行跑，故按特征定位本用例写入的那条，不依赖 first()。
        let snap = snapshot();
        let latest = snap
            .records
            .iter()
            .find(|r| r.total == 31)
            .expect("应能找到本用例写入的记录");
        assert_eq!(latest.failed, 1);
        assert_eq!(
            latest.items.len(),
            MAX_ITEMS_PER_RECORD,
            "明细应被裁剪到上限"
        );
        assert!(!latest.items[0].ok, "失败项必须排在最前且不被裁掉");
        assert_eq!(latest.omitted, 31 - MAX_ITEMS_PER_RECORD);
    }

    /// 记录队列有界：超过 MAX_RECORDS 后弹最旧，杜绝无界内存增长。
    #[test]
    fn record_queue_is_bounded() {
        let _guard = test_lock();
        for _ in 0..(MAX_RECORDS + 5) {
            record_push(vec![item(true, false)], 1);
        }
        assert!(
            records().lock().len() <= MAX_RECORDS,
            "记录数不得超过 {MAX_RECORDS}"
        );
    }

    /// 新 Relay 的记录和计数不得污染生产批量通道；旧面板仍只读取原字段。
    #[test]
    fn relay_and_batch_statistics_are_independent() {
        let _guard = test_lock();
        let before = snapshot();

        let mut batch = item(true, false);
        batch.key = "ksk_batch_plaintext_marker".into();
        record_push(vec![batch], 11);

        let mut relay = item(false, false);
        relay.key = "ksk_rela...sker".into();
        relay.delivery_id = Some("relay-isolation-marker".into());
        record_relay_push(vec![relay], 12);

        let after = snapshot();
        assert_eq!(after.pushes, before.pushes + 1, "Relay 不得增加旧 pushes");
        assert_eq!(after.relay_pushes, before.relay_pushes + 1);
        assert!(
            after.records.iter().any(|r| r
                .items
                .iter()
                .any(|i| i.key == "ksk_batch_plaintext_marker")),
            "旧批量队列必须继续保留管理台明文 key"
        );
        assert!(
            after.relay_records.iter().any(|r| r
                .items
                .iter()
                .any(|i| i.delivery_id.as_deref() == Some("relay-isolation-marker"))),
            "Relay 记录必须只进入 relayRecords"
        );
        assert!(
            !after.records.iter().any(|r| r
                .items
                .iter()
                .any(|i| i.delivery_id.as_deref() == Some("relay-isolation-marker"))),
            "Relay 记录污染了生产批量队列"
        );
    }
}
