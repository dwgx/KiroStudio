//! SQLite 追踪存储 sink（批次2.3）
//!
//! 把每条 [`RequestRecord`] 逐条落账到本地 SQLite 数据库，供 admin 侧做
//! 「最近请求」明细展示与历史留存清理。设计要点：
//! - 开启 WAL 模式 + `synchronous=NORMAL`：写吞吐更高，崩溃安全性对统计数据足够
//! - rusqlite 的 [`Connection`] 非 `Sync`，用 `parking_lot::Mutex` 包裹（与
//!   项目其它模块，如 `token_manager` 保持一致）
//! - 作为 [`UsageSink`]，`on_record` 失败只 `warn` 不 panic：统计侧故障绝不
//!   回传到请求路径
//!
//! 表结构 `traces` 的列与 `RequestRecord` 字段一一对应。u64/u32 字段按
//! SQLite 的整型能力统一以 i64 存取（凭据 ID / 延迟 / 重试数量级均安全）。

use std::path::Path;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, Row};

use super::pipeline::UsageSink;
use super::record::{RequestOutcome, RequestRecord};

/// SQLite 追踪存储
pub struct TraceDb {
    /// rusqlite Connection 非 Sync，用 Mutex 串行化访问
    conn: Mutex<Connection>,
}

impl TraceDb {
    /// 打开/创建数据库，配置 WAL 并建表。
    ///
    /// `path` 为数据库文件路径；父目录需已存在。
    pub fn open(path: &Path) -> Result<TraceDb> {
        let conn = Connection::open(path)
            .with_context(|| format!("打开 SQLite 数据库失败: {}", path.display()))?;

        // WAL 模式提升并发写性能；synchronous=NORMAL 在 WAL 下兼顾安全与吞吐
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;\
             PRAGMA synchronous=NORMAL;",
        )
        .context("配置 SQLite PRAGMA 失败")?;

        Self::init_schema(&conn)?;

        Ok(TraceDb {
            conn: Mutex::new(conn),
        })
    }

    /// 建表 + 建索引（幂等，IF NOT EXISTS）。
    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS traces (
                request_id     TEXT PRIMARY KEY,
                ts_ms          INTEGER NOT NULL,
                credential_id  INTEGER,
                model          TEXT NOT NULL,
                is_streaming   INTEGER NOT NULL,
                input_tokens   INTEGER NOT NULL,
                output_tokens  INTEGER NOT NULL,
                credits_used   REAL,
                latency_ms     INTEGER NOT NULL,
                first_token_ms INTEGER,
                outcome        TEXT NOT NULL,
                retries        INTEGER NOT NULL,
                error_message  TEXT,
                session_id     TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_traces_ts_ms ON traces(ts_ms);
            CREATE INDEX IF NOT EXISTS idx_traces_credential_id ON traces(credential_id);
            CREATE INDEX IF NOT EXISTS idx_traces_model ON traces(model);",
        )
        .context("初始化 traces 表结构失败")?;
        Ok(())
    }

    /// 插入一条记录（参数化，防注入）。
    ///
    /// 使用 `INSERT OR REPLACE`：request_id 主键冲突时覆盖（同一请求的重复落账
    /// 以最后一次为准，避免主键冲突报错）。
    fn insert(&self, record: &RequestRecord) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR REPLACE INTO traces (
                request_id, ts_ms, credential_id, model, is_streaming,
                input_tokens, output_tokens, credits_used, latency_ms, first_token_ms,
                outcome, retries, error_message, session_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                record.request_id,
                record.ts_ms,
                record.credential_id.map(|v| v as i64),
                record.model,
                record.is_streaming,
                record.input_tokens,
                record.output_tokens,
                record.credits_used,
                record.latency_ms as i64,
                record.first_token_ms.map(|v| v as i64),
                record.outcome.as_str(),
                record.retries as i64,
                record.error_message,
                record.session_id,
            ],
        )
        .context("INSERT traces 失败")?;
        Ok(())
    }

    /// 按 ts_ms 倒序取最近 N 条记录。
    pub fn recent(&self, limit: usize) -> Result<Vec<RequestRecord>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT request_id, ts_ms, credential_id, model, is_streaming,
                    input_tokens, output_tokens, credits_used, latency_ms, first_token_ms,
                    outcome, retries, error_message, session_id
             FROM traces
             ORDER BY ts_ms DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], row_to_record)?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.context("读取 traces 行失败")?);
        }
        Ok(out)
    }

    /// 删除 ts_ms 早于 `keep_days` 天前的记录，返回删除行数。
    ///
    /// `keep_days <= 0` 时删除全部记录（无有效保留窗口）。
    pub fn retention_cleanup(&self, keep_days: i64) -> Result<usize> {
        // 保留窗口起点（Unix 毫秒）：早于此时间戳的记录被清理
        let cutoff_ms = chrono::Utc::now().timestamp_millis() - keep_days * 86_400_000;
        let conn = self.conn.lock();
        let deleted = conn
            .execute("DELETE FROM traces WHERE ts_ms < ?1", params![cutoff_ms])
            .context("清理过期 traces 失败")?;
        Ok(deleted)
    }
}

impl UsageSink for TraceDb {
    fn on_record(&self, record: &RequestRecord) {
        // sink 不应 panic：失败仅告警，丢弃该条统计
        if let Err(e) = self.insert(record) {
            tracing::warn!("trace_db 落账失败（已丢弃该条）: {e:#}");
        }
    }

    fn name(&self) -> &'static str {
        "trace_db"
    }
}

/// 把一行 SQLite 结果映射回 [`RequestRecord`]。
///
/// u64/u32 字段以 i64 读出后转回；outcome 文本经 [`parse_outcome`] 还原。
fn row_to_record(row: &Row<'_>) -> rusqlite::Result<RequestRecord> {
    let credential_id: Option<i64> = row.get(2)?;
    let latency_ms: i64 = row.get(8)?;
    let first_token_ms: Option<i64> = row.get(9)?;
    let outcome_str: String = row.get(10)?;
    let retries: i64 = row.get(11)?;

    Ok(RequestRecord {
        request_id: row.get(0)?,
        ts_ms: row.get(1)?,
        credential_id: credential_id.map(|v| v as u64),
        model: row.get(3)?,
        is_streaming: row.get(4)?,
        input_tokens: row.get(5)?,
        output_tokens: row.get(6)?,
        credits_used: row.get(7)?,
        latency_ms: latency_ms as u64,
        first_token_ms: first_token_ms.map(|v| v as u64),
        outcome: parse_outcome(&outcome_str),
        retries: retries as u32,
        error_message: row.get(12)?,
        session_id: row.get(13)?,
    })
}

/// 把 outcome 文本还原为 [`RequestOutcome`]（与 `RequestOutcome::as_str` 互逆）。
///
/// record.rs 是只读契约，未提供反解析，故在此本地实现。未知值兜底为 `OtherError`。
fn parse_outcome(s: &str) -> RequestOutcome {
    match s {
        "success" => RequestOutcome::Success,
        "rate_limited" => RequestOutcome::RateLimited,
        "auth_failed" => RequestOutcome::AuthFailed,
        "quota_exhausted" => RequestOutcome::QuotaExhausted,
        "account_suspended" => RequestOutcome::AccountSuspended,
        "server_error" => RequestOutcome::ServerError,
        "bad_request" => RequestOutcome::BadRequest,
        "network_error" => RequestOutcome::NetworkError,
        _ => RequestOutcome::OtherError,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 生成唯一临时数据库路径（memory db 多连接不共享，测试用真实临时文件更稳）
    struct TempDbPath(PathBuf);

    impl TempDbPath {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            // 用进程内计数器 + 纳秒时间戳保证唯一，避免并发测试撞名
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            p.push(format!("kiro_trace_test_{tag}_{seq}_{nanos}.db"));
            TempDbPath(p)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDbPath {
        fn drop(&mut self) {
            // 清理数据库文件及 WAL/SHM 附属文件
            let _ = std::fs::remove_file(&self.0);
            for ext in ["-wal", "-shm"] {
                let mut side = self.0.clone().into_os_string();
                side.push(ext);
                let _ = std::fs::remove_file(side);
            }
        }
    }

    fn sample_record(id: &str, ts_ms: i64) -> RequestRecord {
        let mut rec = RequestRecord::new(id, "claude-sonnet-4");
        rec.ts_ms = ts_ms;
        rec.credential_id = Some(7);
        rec.is_streaming = true;
        rec.input_tokens = 120;
        rec.output_tokens = 45;
        rec.credits_used = Some(2.5);
        rec.latency_ms = 1500;
        rec.first_token_ms = Some(300);
        rec.outcome = RequestOutcome::Success;
        rec.retries = 1;
        rec.error_message = Some("none".to_string());
        rec.session_id = Some("conv-1".to_string());
        rec
    }

    #[test]
    fn test_create_insert_recent_roundtrip() {
        let tmp = TempDbPath::new("roundtrip");
        let db = TraceDb::open(tmp.path()).unwrap();

        let rec = sample_record("req-a", 1_000);
        db.on_record(&rec);

        let got = db.recent(10).unwrap();
        assert_eq!(got.len(), 1);
        let back = &got[0];
        assert_eq!(back.request_id, "req-a");
        assert_eq!(back.ts_ms, 1_000);
        assert_eq!(back.credential_id, Some(7));
        assert_eq!(back.model, "claude-sonnet-4");
        assert!(back.is_streaming);
        assert_eq!(back.input_tokens, 120);
        assert_eq!(back.output_tokens, 45);
        assert_eq!(back.credits_used, Some(2.5));
        assert_eq!(back.latency_ms, 1500);
        assert_eq!(back.first_token_ms, Some(300));
        assert_eq!(back.outcome, RequestOutcome::Success);
        assert_eq!(back.retries, 1);
        assert_eq!(back.error_message, Some("none".to_string()));
        assert_eq!(back.session_id, Some("conv-1".to_string()));
    }

    #[test]
    fn test_recent_orders_desc_by_ts() {
        let tmp = TempDbPath::new("order");
        let db = TraceDb::open(tmp.path()).unwrap();

        // 乱序插入，验证 recent 按 ts_ms 倒序、且 limit 生效
        db.on_record(&sample_record("old", 100));
        db.on_record(&sample_record("new", 300));
        db.on_record(&sample_record("mid", 200));

        let got = db.recent(2).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].request_id, "new");
        assert_eq!(got[1].request_id, "mid");
    }

    #[test]
    fn test_outcome_variants_roundtrip() {
        let tmp = TempDbPath::new("outcome");
        let db = TraceDb::open(tmp.path()).unwrap();

        let variants = [
            RequestOutcome::Success,
            RequestOutcome::RateLimited,
            RequestOutcome::AuthFailed,
            RequestOutcome::QuotaExhausted,
            RequestOutcome::AccountSuspended,
            RequestOutcome::ServerError,
            RequestOutcome::BadRequest,
            RequestOutcome::NetworkError,
            RequestOutcome::OtherError,
        ];

        // 每个变体用递增 ts_ms 插入，读回后按 request_id 建映射逐一校验
        for (i, oc) in variants.iter().enumerate() {
            let mut rec = sample_record(&format!("req-{i}"), 1_000 + i as i64);
            rec.outcome = *oc;
            db.on_record(&rec);
        }

        let got = db.recent(variants.len()).unwrap();
        assert_eq!(got.len(), variants.len());

        for (i, oc) in variants.iter().enumerate() {
            let id = format!("req-{i}");
            let rec = got.iter().find(|r| r.request_id == id).unwrap();
            assert_eq!(rec.outcome, *oc, "outcome 变体 {} 往返不一致", oc.as_str());
        }
    }

    #[test]
    fn test_retention_cleanup_deletes_old_keeps_new() {
        let tmp = TempDbPath::new("retention");
        let db = TraceDb::open(tmp.path()).unwrap();

        let now = chrono::Utc::now().timestamp_millis();
        let ten_days = 10 * 86_400_000i64;
        let one_day = 86_400_000i64;

        // 一条 10 天前的旧记录 + 一条 1 天前的新记录
        db.on_record(&sample_record("old", now - ten_days));
        db.on_record(&sample_record("fresh", now - one_day));

        // 保留 7 天：旧记录应被删，新记录保留
        let deleted = db.retention_cleanup(7).unwrap();
        assert_eq!(deleted, 1);

        let got = db.recent(10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].request_id, "fresh");
    }

    #[test]
    fn test_null_optional_fields_roundtrip() {
        let tmp = TempDbPath::new("nulls");
        let db = TraceDb::open(tmp.path()).unwrap();

        // 全部 Option 字段为 None，验证 NULL 往返
        let mut rec = RequestRecord::new("req-null", "m");
        rec.ts_ms = 500;
        rec.credential_id = None;
        rec.credits_used = None;
        rec.first_token_ms = None;
        rec.error_message = None;
        rec.session_id = None;
        db.on_record(&rec);

        let got = db.recent(1).unwrap();
        assert_eq!(got.len(), 1);
        let back = &got[0];
        assert_eq!(back.credential_id, None);
        assert_eq!(back.credits_used, None);
        assert_eq!(back.first_token_ms, None);
        assert_eq!(back.error_message, None);
        assert_eq!(back.session_id, None);
    }
}
