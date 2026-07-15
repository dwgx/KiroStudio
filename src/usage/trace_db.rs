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
use rusqlite::{params, params_from_iter, Connection, Row, ToSql};

use super::pipeline::UsageSink;
use super::record::{RequestOutcome, RequestRecord};

/// 追踪明细查询的最大单页条数（保护内存/带宽：单次 search 最多取这么多行）。
pub const MAX_SEARCH_LIMIT: usize = 500;

/// trace 明细多维过滤条件。
///
/// 所有字段均为 `Option`，`None` 表示该维度不过滤。组合时各维度按 AND 相连。
/// 全部经参数化查询下发，绝不字符串拼接用户值（SQL 注入安全）。
#[derive(Debug, Clone, Default)]
pub struct TraceFilter {
    /// 模型精确匹配（model = ?）
    pub model: Option<String>,
    /// 凭据 ID 精确匹配（credential_id = ?）
    pub credential_id: Option<u64>,
    /// 客户端 IP 子串匹配（client_ip LIKE %?%）
    pub client_ip: Option<String>,
    /// 会话 ID 精确匹配（session_id = ?）
    pub session_id: Option<String>,
    /// 结果精确匹配（outcome = ?，取 `RequestOutcome::as_str` 值）
    pub outcome: Option<String>,
    /// 时间范围起点（含，ts_ms >= ?）
    pub ts_from: Option<i64>,
    /// 时间范围终点（含，ts_ms <= ?）
    pub ts_to: Option<i64>,
    /// 全文子串匹配 error_message OR request_id OR model（任一 LIKE %?%）
    pub text: Option<String>,
    /// 是否流式（is_streaming = ?）
    pub is_streaming: Option<bool>,
}

impl TraceFilter {
    /// 依据当前过滤条件构建 `WHERE ...` 片段与对应的参数向量（顺序一致）。
    ///
    /// 返回的字符串以 " WHERE " 开头（无任何条件时为空串），参数向量按占位符顺序排列，
    /// 全部走 rusqlite 参数绑定——**绝不**把用户值拼进 SQL 文本，杜绝注入。
    fn build_where(&self) -> (String, Vec<Box<dyn ToSql>>) {
        let mut clauses: Vec<String> = Vec::new();
        let mut binds: Vec<Box<dyn ToSql>> = Vec::new();

        if let Some(m) = &self.model {
            clauses.push("model = ?".to_string());
            binds.push(Box::new(m.clone()));
        }
        if let Some(cid) = self.credential_id {
            clauses.push("credential_id = ?".to_string());
            binds.push(Box::new(cid as i64));
        }
        if let Some(ip) = &self.client_ip {
            // 子串匹配：转义 LIKE 元字符（% _ \），用 ESCAPE '\' 保证按字面量匹配。
            clauses.push("client_ip LIKE ? ESCAPE '\\'".to_string());
            binds.push(Box::new(format!("%{}%", escape_like(ip))));
        }
        if let Some(sid) = &self.session_id {
            clauses.push("session_id = ?".to_string());
            binds.push(Box::new(sid.clone()));
        }
        if let Some(oc) = &self.outcome {
            clauses.push("outcome = ?".to_string());
            binds.push(Box::new(oc.clone()));
        }
        if let Some(from) = self.ts_from {
            clauses.push("ts_ms >= ?".to_string());
            binds.push(Box::new(from));
        }
        if let Some(to) = self.ts_to {
            clauses.push("ts_ms <= ?".to_string());
            binds.push(Box::new(to));
        }
        if let Some(t) = &self.text {
            // 全文：error_message / request_id / model 任一子串命中。三个占位符共用同一模式串。
            clauses.push(
                "(error_message LIKE ? ESCAPE '\\' OR request_id LIKE ? ESCAPE '\\' OR model LIKE ? ESCAPE '\\')"
                    .to_string(),
            );
            let pat = format!("%{}%", escape_like(t));
            binds.push(Box::new(pat.clone()));
            binds.push(Box::new(pat.clone()));
            binds.push(Box::new(pat));
        }
        if let Some(s) = self.is_streaming {
            clauses.push("is_streaming = ?".to_string());
            binds.push(Box::new(s as i64));
        }

        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        (where_sql, binds)
    }
}

/// 转义 SQL LIKE 的元字符（`\` `%` `_`），配合 `ESCAPE '\'` 让用户输入按字面量子串匹配，
/// 避免用户传入的 `%`/`_` 被当通配符（既是正确性也是防注入的一部分）。
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

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
        Self::migrate_schema(&conn)?;

        Ok(TraceDb {
            conn: Mutex::new(conn),
        })
    }

    /// 增量迁移：为旧库补齐新增列（幂等）。
    ///
    /// `CREATE TABLE IF NOT EXISTS` 不会给已存在的表加列，因此历史库升级后
    /// 缺少新字段。这里用 `ALTER TABLE ... ADD COLUMN` 补列，并吞掉「列已存在」
    /// 错误（duplicate column），保证：新库/旧库都能得到完整表结构、不丢历史数据、
    /// 反复启动也安全。
    fn migrate_schema(conn: &Connection) -> Result<()> {
        // 逐条尝试新增列；已存在则忽略 "duplicate column name" 错误
        let add_columns = [
            "ALTER TABLE traces ADD COLUMN client_device TEXT",
            "ALTER TABLE traces ADD COLUMN client_ip TEXT",
            "ALTER TABLE traces ADD COLUMN client_os TEXT",
            "ALTER TABLE traces ADD COLUMN client_browser TEXT",
            // 缓存读写 tokens（历史库补列，默认 0，兼容旧数据）
            "ALTER TABLE traces ADD COLUMN cache_read_tokens INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE traces ADD COLUMN cache_creation_tokens INTEGER NOT NULL DEFAULT 0",
        ];
        for sql in add_columns {
            if let Err(e) = conn.execute(sql, []) {
                let msg = e.to_string().to_lowercase();
                // rusqlite/sqlite 对已存在列报 "duplicate column name: ..."
                if msg.contains("duplicate column") {
                    continue;
                }
                return Err(e).with_context(|| format!("迁移 traces 表失败: {sql}"));
            }
        }

        // 新增检索维度的索引：必须放在补列之后建——client_ip 由上面的 ALTER 才补上，
        // 若放进 init_schema 会在旧库上「no such column: client_ip」直接失败。
        // session_id/outcome 是初始列，但一并放这里保证顺序无依赖、幂等（IF NOT EXISTS）。
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_traces_client_ip ON traces(client_ip);
             CREATE INDEX IF NOT EXISTS idx_traces_session_id ON traces(session_id);
             CREATE INDEX IF NOT EXISTS idx_traces_outcome ON traces(outcome);",
        )
        .context("建立 traces 检索索引失败")?;
        Ok(())
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
                session_id     TEXT,
                client_device  TEXT,
                client_ip      TEXT,
                client_os      TEXT,
                client_browser TEXT,
                cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
                cache_creation_tokens INTEGER NOT NULL DEFAULT 0
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
                outcome, retries, error_message, session_id, client_device,
                client_ip, client_os, client_browser, cache_read_tokens, cache_creation_tokens
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
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
                record.client_device,
                record.client_ip,
                record.client_os,
                record.client_browser,
                record.cache_read_tokens,
                record.cache_creation_tokens,
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
                    outcome, retries, error_message, session_id, client_device,
                    client_ip, client_os, client_browser, cache_read_tokens, cache_creation_tokens
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

    /// 按 [`TraceFilter`] 多维过滤 + 分页查询明细（ts_ms 倒序）。
    ///
    /// `limit` 会裁剪到 `[1, MAX_SEARCH_LIMIT]`（保护内存）；`offset` 原样透传。
    /// WHERE 片段与全部用户值均走参数绑定，SQL 注入安全。
    pub fn search(
        &self,
        filter: &TraceFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RequestRecord>> {
        let capped = limit.clamp(1, MAX_SEARCH_LIMIT);
        let (where_sql, mut binds) = filter.build_where();

        let sql = format!(
            "SELECT request_id, ts_ms, credential_id, model, is_streaming,
                    input_tokens, output_tokens, credits_used, latency_ms, first_token_ms,
                    outcome, retries, error_message, session_id, client_device,
                    client_ip, client_os, client_browser, cache_read_tokens, cache_creation_tokens
             FROM traces{where_sql}
             ORDER BY ts_ms DESC
             LIMIT ? OFFSET ?"
        );

        // LIMIT / OFFSET 追加为最后两个参数（同样参数化，绝不拼进 SQL 文本）。
        binds.push(Box::new(capped as i64));
        binds.push(Box::new(offset as i64));

        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(binds.iter().map(|b| b.as_ref())), row_to_record)?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.context("读取 traces 行失败")?);
        }
        Ok(out)
    }

    /// 与 [`search`](Self::search) 同一 WHERE 条件下的匹配总行数（供分页展示总数）。
    pub fn count_filtered(&self, filter: &TraceFilter) -> Result<i64> {
        let (where_sql, binds) = filter.build_where();
        let sql = format!("SELECT COUNT(*) FROM traces{where_sql}");

        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let n: i64 = stmt.query_row(
            params_from_iter(binds.iter().map(|b| b.as_ref())),
            |row| row.get(0),
        )?;
        Ok(n.max(0))
    }

    /// 统计 traces 表当前总行数（供 admin 存储统计展示）。
    pub fn count(&self) -> Result<u64> {
        let conn = self.conn.lock();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM traces", [], |row| row.get(0))
            .context("统计 traces 行数失败")?;
        Ok(n.max(0) as u64)
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
        client_device: row.get(14)?,
        client_ip: row.get(15)?,
        client_os: row.get(16)?,
        client_browser: row.get(17)?,
        cache_read_tokens: row.get(18)?,
        cache_creation_tokens: row.get(19)?,
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
        rec.client_device = Some("claude-code".to_string());
        rec.client_ip = Some("203.0.113.7".to_string());
        rec.client_os = Some("Windows".to_string());
        rec.client_browser = Some("Chrome 120".to_string());
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
        assert_eq!(back.client_device, Some("claude-code".to_string()));
        assert_eq!(back.client_ip, Some("203.0.113.7".to_string()));
        assert_eq!(back.client_os, Some("Windows".to_string()));
        assert_eq!(back.client_browser, Some("Chrome 120".to_string()));
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
        assert_eq!(back.client_device, None);
        assert_eq!(back.client_ip, None);
        assert_eq!(back.client_os, None);
        assert_eq!(back.client_browser, None);
    }

    /// 构造一批 model/client_ip/outcome/ts 各异的记录，供 search 各维度断言。
    fn seed_varied(db: &TraceDb) {
        // rec-a: sonnet / 10.0.0.1 / success / ts=1000 / 流式 / cred 7
        let mut a = sample_record("rec-a", 1_000);
        a.model = "claude-sonnet-4".into();
        a.client_ip = Some("10.0.0.1".into());
        a.outcome = RequestOutcome::Success;
        a.is_streaming = true;
        a.credential_id = Some(7);
        a.error_message = None;
        a.session_id = Some("conv-A".into());
        db.on_record(&a);

        // rec-b: opus / 10.0.0.2 / rate_limited / ts=2000 / 非流式 / cred 8 / 带错误文案
        let mut b = sample_record("rec-b", 2_000);
        b.model = "claude-opus-4".into();
        b.client_ip = Some("10.0.0.2".into());
        b.outcome = RequestOutcome::RateLimited;
        b.is_streaming = false;
        b.credential_id = Some(8);
        b.error_message = Some("upstream 429 rate limited".into());
        b.session_id = Some("conv-B".into());
        db.on_record(&b);

        // rec-c: sonnet / 192.168.1.5 / server_error / ts=3000 / 非流式 / cred 7
        let mut c = sample_record("rec-c", 3_000);
        c.model = "claude-sonnet-4".into();
        c.client_ip = Some("192.168.1.5".into());
        c.outcome = RequestOutcome::ServerError;
        c.is_streaming = false;
        c.credential_id = Some(7);
        c.error_message = Some("internal server error".into());
        c.session_id = Some("conv-C".into());
        db.on_record(&c);

        // rec-d: haiku / 10.0.0.1 / success / ts=4000 / 流式 / cred 9
        let mut d = sample_record("rec-d", 4_000);
        d.model = "claude-haiku-4".into();
        d.client_ip = Some("10.0.0.1".into());
        d.outcome = RequestOutcome::Success;
        d.is_streaming = true;
        d.credential_id = Some(9);
        d.error_message = None;
        d.session_id = Some("conv-D".into());
        db.on_record(&d);
    }

    #[test]
    fn test_search_no_filter_returns_all_desc() {
        let tmp = TempDbPath::new("search_all");
        let db = TraceDb::open(tmp.path()).unwrap();
        seed_varied(&db);

        let got = db.search(&TraceFilter::default(), 100, 0).unwrap();
        assert_eq!(got.len(), 4);
        // ts_ms 倒序：d(4000) > c(3000) > b(2000) > a(1000)
        assert_eq!(got[0].request_id, "rec-d");
        assert_eq!(got[3].request_id, "rec-a");
        assert_eq!(db.count_filtered(&TraceFilter::default()).unwrap(), 4);
    }

    #[test]
    fn test_search_by_model_exact() {
        let tmp = TempDbPath::new("search_model");
        let db = TraceDb::open(tmp.path()).unwrap();
        seed_varied(&db);

        let f = TraceFilter {
            model: Some("claude-sonnet-4".into()),
            ..Default::default()
        };
        let got = db.search(&f, 100, 0).unwrap();
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|r| r.model == "claude-sonnet-4"));
        assert_eq!(db.count_filtered(&f).unwrap(), 2);
    }

    #[test]
    fn test_search_by_credential_and_outcome() {
        let tmp = TempDbPath::new("search_cred_outcome");
        let db = TraceDb::open(tmp.path()).unwrap();
        seed_varied(&db);

        // credential_id = 7
        let f_cred = TraceFilter {
            credential_id: Some(7),
            ..Default::default()
        };
        assert_eq!(db.count_filtered(&f_cred).unwrap(), 2);

        // outcome = success
        let f_oc = TraceFilter {
            outcome: Some("success".into()),
            ..Default::default()
        };
        let got = db.search(&f_oc, 100, 0).unwrap();
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|r| r.outcome == RequestOutcome::Success));

        // AND 组合：cred 7 且 success → 仅 rec-a
        let f_both = TraceFilter {
            credential_id: Some(7),
            outcome: Some("success".into()),
            ..Default::default()
        };
        let got = db.search(&f_both, 100, 0).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].request_id, "rec-a");
    }

    #[test]
    fn test_search_client_ip_substring() {
        let tmp = TempDbPath::new("search_ip");
        let db = TraceDb::open(tmp.path()).unwrap();
        seed_varied(&db);

        // 子串 "10.0.0" 命中 rec-a(10.0.0.1) / rec-b(10.0.0.2) / rec-d(10.0.0.1)（不含 192.168.x）
        let f = TraceFilter {
            client_ip: Some("10.0.0".into()),
            ..Default::default()
        };
        let got = db.search(&f, 100, 0).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(db.count_filtered(&f).unwrap(), 3);

        // 子串 "192.168" 只命中 rec-c
        let f_lan = TraceFilter {
            client_ip: Some("192.168".into()),
            ..Default::default()
        };
        assert_eq!(db.count_filtered(&f_lan).unwrap(), 1);

        // 精确到 .2 → 仅 rec-b
        let f2 = TraceFilter {
            client_ip: Some("10.0.0.2".into()),
            ..Default::default()
        };
        let got = db.search(&f2, 100, 0).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].request_id, "rec-b");
    }

    #[test]
    fn test_search_ts_range() {
        let tmp = TempDbPath::new("search_ts");
        let db = TraceDb::open(tmp.path()).unwrap();
        seed_varied(&db);

        // [2000, 3000] → rec-b / rec-c
        let f = TraceFilter {
            ts_from: Some(2_000),
            ts_to: Some(3_000),
            ..Default::default()
        };
        let got = db.search(&f, 100, 0).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].request_id, "rec-c"); // 倒序
        assert_eq!(got[1].request_id, "rec-b");
        assert_eq!(db.count_filtered(&f).unwrap(), 2);
    }

    #[test]
    fn test_search_text_matches_error_and_id_and_model() {
        let tmp = TempDbPath::new("search_text");
        let db = TraceDb::open(tmp.path()).unwrap();
        seed_varied(&db);

        // "rate limited" 只出现在 rec-b 的 error_message
        let f = TraceFilter {
            text: Some("rate limited".into()),
            ..Default::default()
        };
        let got = db.search(&f, 100, 0).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].request_id, "rec-b");

        // "rec-c" 命中 request_id
        let f2 = TraceFilter {
            text: Some("rec-c".into()),
            ..Default::default()
        };
        let got = db.search(&f2, 100, 0).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].request_id, "rec-c");

        // "opus" 命中 model（rec-b）
        let f3 = TraceFilter {
            text: Some("opus".into()),
            ..Default::default()
        };
        let got = db.search(&f3, 100, 0).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].request_id, "rec-b");
    }

    #[test]
    fn test_search_is_streaming_filter() {
        let tmp = TempDbPath::new("search_stream");
        let db = TraceDb::open(tmp.path()).unwrap();
        seed_varied(&db);

        let f_true = TraceFilter {
            is_streaming: Some(true),
            ..Default::default()
        };
        assert_eq!(db.count_filtered(&f_true).unwrap(), 2); // rec-a, rec-d

        let f_false = TraceFilter {
            is_streaming: Some(false),
            ..Default::default()
        };
        assert_eq!(db.count_filtered(&f_false).unwrap(), 2); // rec-b, rec-c
    }

    #[test]
    fn test_search_pagination_limit_offset() {
        let tmp = TempDbPath::new("search_page");
        let db = TraceDb::open(tmp.path()).unwrap();
        seed_varied(&db);

        // 倒序全序: d, c, b, a
        let page1 = db.search(&TraceFilter::default(), 2, 0).unwrap();
        assert_eq!(page1.len(), 2);
        assert_eq!(page1[0].request_id, "rec-d");
        assert_eq!(page1[1].request_id, "rec-c");

        let page2 = db.search(&TraceFilter::default(), 2, 2).unwrap();
        assert_eq!(page2.len(), 2);
        assert_eq!(page2[0].request_id, "rec-b");
        assert_eq!(page2[1].request_id, "rec-a");

        // offset 越界 → 空
        let page3 = db.search(&TraceFilter::default(), 2, 4).unwrap();
        assert!(page3.is_empty());

        // count_filtered 不受分页影响
        assert_eq!(db.count_filtered(&TraceFilter::default()).unwrap(), 4);
    }

    #[test]
    fn test_search_limit_capped() {
        let tmp = TempDbPath::new("search_cap");
        let db = TraceDb::open(tmp.path()).unwrap();
        seed_varied(&db);

        // 超大 limit 被裁剪到 MAX_SEARCH_LIMIT，仍返回全部现有行（4 < 500）
        let got = db.search(&TraceFilter::default(), usize::MAX, 0).unwrap();
        assert_eq!(got.len(), 4);
        assert!(MAX_SEARCH_LIMIT <= 500);
    }

    #[test]
    fn test_search_like_wildcards_are_literal() {
        let tmp = TempDbPath::new("search_wild");
        let db = TraceDb::open(tmp.path()).unwrap();
        seed_varied(&db);

        // 用户传入 "%" 不应被当通配符匹配所有 IP —— 无 IP 含字面 '%'，应 0 命中。
        let f = TraceFilter {
            client_ip: Some("%".into()),
            ..Default::default()
        };
        assert_eq!(db.count_filtered(&f).unwrap(), 0);
    }

    /// 模拟旧库（无 client_device 列 + 已有历史数据），验证迁移幂等且不丢数据。
    #[test]
    fn test_migration_adds_client_device_to_legacy_db() {
        let tmp = TempDbPath::new("migrate");

        // 1) 手工建一张「旧版」traces 表：故意不含 client_device 列，并塞一条历史记录
        {
            let conn = Connection::open(tmp.path()).unwrap();
            conn.execute_batch(
                "CREATE TABLE traces (
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
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO traces (
                    request_id, ts_ms, credential_id, model, is_streaming,
                    input_tokens, output_tokens, credits_used, latency_ms, first_token_ms,
                    outcome, retries, error_message, session_id
                ) VALUES ('legacy', 42, 7, 'm', 0, 1, 2, NULL, 10, NULL, 'success', 0, NULL, NULL)",
                [],
            )
            .unwrap();
        }

        // 2) 用 TraceDb::open 打开旧库 → 触发迁移
        let db = TraceDb::open(tmp.path()).unwrap();

        // 历史数据仍在，且新列读回为 None（旧行没有该值）
        let got = db.recent(10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].request_id, "legacy");
        assert_eq!(got[0].client_device, None);

        // 3) 迁移后可正常写入带 device 的新记录并读回
        db.on_record(&sample_record("new-with-device", 100));
        let got = db.recent(10).unwrap();
        let rec = got.iter().find(|r| r.request_id == "new-with-device").unwrap();
        assert_eq!(rec.client_device, Some("claude-code".to_string()));

        // 4) 再次 open 同一库，迁移应幂等（不因列已存在而报错）
        drop(db);
        let db2 = TraceDb::open(tmp.path()).unwrap();
        assert_eq!(db2.recent(10).unwrap().len(), 2);
    }
}
