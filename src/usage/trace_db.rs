//! SQLite 追踪存储 sink（批次2.3）
//!
//! 把每条 [`RequestRecord`] 落账到本地 SQLite 数据库，供 admin 侧做
//! 「最近请求」明细展示与历史留存清理。设计要点：
//! - 开启 WAL 模式 + `synchronous=NORMAL`：写吞吐更高，崩溃安全性对统计数据足够
//! - rusqlite 的 [`Connection`] 非 `Sync`，用 `parking_lot::Mutex` 包裹（与
//!   项目其它模块，如 `token_manager` 保持一致）
//! - 作为 [`UsageSink`]，`on_record` 失败只 `warn` 不 panic：统计侧故障绝不
//!   回传到请求路径
//! - **批量写（攒批）**：`on_record` 先把记录攒进内存队列（不锁 DB），
//!   满 [`BATCH_SIZE`] 条或距上次落库超 [`FLUSH_INTERVAL`] 时，一批一个事务
//!   批量 INSERT（fsync 从 N 次降到 1 次）。所有**读路径**（查询/清理）先
//!   flush 待写队列保证读写一致；`Drop` 兜底进程退出时残留的待写记录。
//!
//! 表结构 `traces` 的列与 `RequestRecord` 字段一一对应。u64/u32 字段按
//! SQLite 的整型能力统一以 i64 存取（凭据 ID / 延迟 / 重试数量级均安全）。
//!
//! # SQLite 运维三件套（防 db/WAL 无限膨胀）
//! 只靠 WAL 模式不够：WAL 文件不 checkpoint 会随写入无限增长、删除数据后
//! 空闲页不回收（db 文件不缩小）、大批量清理单条长 SQL 会长时间持锁。
//! 三件套全部内聚在本模块（main.rs 无需改动，接线走既有路径）：
//! 1. **WAL checkpoint 截断**：写路径（`TraceDb::flush_pending`）自驱动 + 清理路径
//!    （`TraceDb::retention_cleanup`）兜底，经 [`MAINTENANCE_INTERVAL`] 时间门控节流；
//!    PASSIVE checkpoint 不阻塞写，WAL 日志超 [`WAL_TRUNCATE_MIN_FRAMES`] 帧
//!    才 TRUNCATE 截断（WAL 峰值有界）。
//! 2. **空闲页回收**：新库在写下 page 1 之前设 `auto_vacuum=INCREMENTAL`；存量库
//!    （原本 NONE）启动期 `PRAGMA auto_vacuum=INCREMENTAL` + 一次 `VACUUM`
//!    做布局转换（把 header 写成 2；与 512MB 回收闸门无关，小库也要做）。
//!    大库空闲页回收仍走 [`VACUUM_MIN_FILE_BYTES`]：启动期
//!    [`TraceDb::maybe_convert_legacy_db`] 全量 `VACUUM`，运行期
//!    `incremental_vacuum` 渐进回收（每轮限 [`VACUUM_BATCH_PAGES`] 页防长锁）。
//! 3. **分批清理**：`retention_cleanup` 的 DELETE 改为每批
//!    [`DELETE_BATCH_SIZE`] 条循环（短事务，毫秒级持锁；也避免超大 DELETE
//!    在 WAL 里产生巨型日志帧）。

use std::path::Path;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, Row, ToSql, params, params_from_iter};

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
    /// 客户端请求的**原始**模型名精确匹配（requested_model = ?；映射双口径的 requested 维度）。
    pub requested_model: Option<String>,
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
        if let Some(rm) = &self.requested_model {
            clauses.push("requested_model = ?".to_string());
            binds.push(Box::new(rm.clone()));
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

/// WAL 日志帧数是否达到截断阈值（纯函数，可测：决定 TRUNCATE 是否执行）。
fn should_truncate_wal(log_frames: i64) -> bool {
    log_frames >= WAL_TRUNCATE_MIN_FRAMES
}

/// 空闲页回收是否该触发：db 文件 ≥ [`VACUUM_MIN_FILE_BYTES`] 且存在空闲页
/// （纯函数，可测：512MB 阈值保护 + 无空闲页不空转）。
fn should_vacuum(file_bytes: u64, freelist_pages: u64) -> bool {
    file_bytes >= VACUUM_MIN_FILE_BYTES && freelist_pages > 0
}

/// 维护间隔是否已到（纯函数，可测；saturating 防 `last` 在未来时 panic）。
fn maintenance_due(last: std::time::Instant, now: std::time::Instant, interval: std::time::Duration) -> bool {
    now.saturating_duration_since(last) >= interval
}

/// SQLite 追踪存储
pub struct TraceDb {
    /// rusqlite Connection 非 Sync，用 Mutex 串行化访问
    conn: Mutex<Connection>,
    /// 待批量落库的积攒队列（与 `conn` 分开锁：攒批不碰 DB，flush 才取锁）
    pending: Mutex<PendingBatch>,
}

/// 批量写攒批状态。
struct PendingBatch {
    records: Vec<RequestRecord>,
    /// 上次落库检查时刻（节流：低流量下保证 ≤ [`FLUSH_INTERVAL`] 落一次盘）
    last_flush: std::time::Instant,
    /// 上次维护（WAL checkpoint/空闲页回收）时刻：写路径自驱动的节流门控。
    /// 与 `conn` 分开锁（同 `pending` 整体），维护入口先读它再决定是否执行。
    last_maintenance: std::time::Instant,
}

/// 批量攒批上限：满 50 条触发一次落库（一个事务 50 条 INSERT，fsync 一次）。
const BATCH_SIZE: usize = 50;
/// 落库节流间隔：低流量请求下最迟这么久落一次盘。
const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
/// 维护间隔：距上次 WAL checkpoint/空闲页回收不足此间隔时跳过（节流，防高频
/// flush 反复触发）。高流量写路径下 ≈ 每 10 分钟一轮；低流量由
/// `TraceDb::retention_cleanup` 的 6h 周期任务兜底。
const MAINTENANCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10 * 60);
/// WAL 日志达到此帧数（4KB 页下 ≈ 8MB）才执行 TRUNCATE 截断：WAL 峰值有界，
/// 小流量下避免频繁截断。
const WAL_TRUNCATE_MIN_FRAMES: i64 = 2048;
/// 空闲页回收的库文件阈值：db 文件不足 512MB 不触发回收（避免小库反复
/// VACUUM/搬运，也避免启动期对小型存量库做无价值的全库重写）。
const VACUUM_MIN_FILE_BYTES: u64 = 512 * 1024 * 1024;
/// 单轮 incremental_vacuum 最多回收的页数（4KB 页下 ≈ 16MB）：渐进回收，防长锁。
const VACUUM_BATCH_PAGES: i64 = 4096;
/// retention 分批删除的单批行数：单条 DELETE 持锁时间从「全表删除」降到
/// 毫秒级（短事务），大批量清理不再长时间阻塞读路径。
const DELETE_BATCH_SIZE: i64 = 1000;

impl TraceDb {
    /// 打开/创建数据库，配置 WAL 并建表。
    ///
    /// `path` 为数据库文件路径；父目录需已存在。
    pub fn open(path: &Path) -> Result<TraceDb> {
        // 必须在 sqlite3_open 之前看文件：open 会创建 0 字节文件，但空文件仍算新库。
        // 新库一旦先 `PRAGMA auto_vacuum`（读），SQLite 会按 NONE 写下 page 1，之后
        // 同连接再设 INCREMENTAL 只改内存，别的连接读 header 仍是 0。
        let had_content = path.is_file()
            && std::fs::metadata(path)
                .map(|m| m.len() > 0)
                .unwrap_or(false);

        let conn = Connection::open(path)
            .with_context(|| format!("打开 SQLite 数据库失败: {}", path.display()))?;

        // ⚠️ 顺序敏感：
        // 1) 空库：INCREMENTAL 必须是第一条会写 header 的语句，且在 journal_mode/建表之前。
        // 2) 存量库：先读原始 auto_vacuum（此时读是安全的，page 1 已存在）。
        //    pragma 单独不够，表存在后必须 `INCREMENTAL` + `VACUUM` 才能把 header 写成 2。
        //    布局转换 ≠ 512MB 空闲页回收闸门——小库也要做一次。
        let legacy_auto_vacuum: i64 = if had_content {
            conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
                .context("读取 auto_vacuum 失败")?
        } else {
            0
        };
        if legacy_auto_vacuum != 2 {
            conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")
                .context("配置 auto_vacuum=INCREMENTAL 失败")?;
        }

        // WAL 模式提升并发写性能；synchronous=NORMAL 在 WAL 下兼顾安全与吞吐
        // busy_timeout=5000：多实例（SO_REUSEPORT）并发写同一库时，SQLITE_BUSY 等待
        // 而非立即失败——否则写锁竞争会静默丢记录（rusqlite 默认 busy 不等待）。
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;\
             PRAGMA synchronous=NORMAL;\
             PRAGMA busy_timeout=5000;",
        )
        .context("配置 SQLite PRAGMA 失败")?;

        Self::init_schema(&conn)?;
        Self::migrate_schema(&conn)?;

        if had_content && legacy_auto_vacuum != 2 {
            // 存量 NONE：同连接 `PRAGMA auto_vacuum` 在 VACUUM 前仍可能报 0（SQLite
            // 拒绝在已有页的库上改 header）。必须先设再 VACUUM，顺序反了 header 停在 0。
            conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL; VACUUM;")
                .context("存量库 VACUUM 以启用 incremental auto_vacuum 失败")?;
        } else if had_content {
            // 已是 INCREMENTAL 的大库：512MB 闸门下一次性收回空闲页。
            Self::maybe_convert_legacy_db(&conn)?;
        }

        Ok(TraceDb {
            conn: Mutex::new(conn),
            pending: Mutex::new(PendingBatch {
                records: Vec::new(),
                last_flush: std::time::Instant::now(),
                last_maintenance: std::time::Instant::now(),
            }),
        })
    }

    /// 已是 INCREMENTAL 的存量库：db 文件 ≥ [`VACUUM_MIN_FILE_BYTES`] 且存在
    /// 空闲页时，启动期一次性全库 `VACUUM` 收回空闲页（真正缩文件；WAL 下
    /// `incremental_vacuum` 做不到）。NONE→INCREMENTAL 的布局转换不走这里，
    /// 见 [`TraceDb::open`]。此步只发生在启动期（连接唯一持有者，无并发）。
    fn maybe_convert_legacy_db(conn: &Connection) -> Result<()> {
        let page_count: i64 = conn
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .context("读取 page_count 失败")?;
        let page_size: i64 = conn
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .context("读取 page_size 失败")?;
        let freelist: i64 = conn
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .context("读取 freelist_count 失败")?;
        if !should_vacuum(page_count as u64 * page_size as u64, freelist as u64) {
            return Ok(());
        }
        conn.execute_batch("VACUUM;")
            .context("存量库 VACUUM 转换失败")?;
        tracing::info!("trace_db 存量库 VACUUM 完成（空闲页已回收）");
        Ok(())
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
            // 映射双口径（历史库补列，默认 NULL 兼容旧数据）
            "ALTER TABLE traces ADD COLUMN requested_model TEXT",
            "ALTER TABLE traces ADD COLUMN upstream_model TEXT",
            // 中断字节（历史库补列，默认 NULL = 未中断，兼容旧数据）
            "ALTER TABLE traces ADD COLUMN interrupted_bytes INTEGER",
            // 链内首选号（历史库补列，默认 NULL = 无 failover 信息，兼容旧数据）
            "ALTER TABLE traces ADD COLUMN first_attempted_credential_id INTEGER",
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
                cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                requested_model       TEXT,
                upstream_model        TEXT,
                interrupted_bytes     INTEGER,
                first_attempted_credential_id INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_traces_ts_ms ON traces(ts_ms);
            CREATE INDEX IF NOT EXISTS idx_traces_credential_id ON traces(credential_id);
            CREATE INDEX IF NOT EXISTS idx_traces_model ON traces(model);",
        )
        .context("初始化 traces 表结构失败")?;
        Ok(())
    }

    /// 批量插入一组记录（单事务 + 预编译语句复用；参数化，防注入）。
    ///
    /// 使用 `INSERT OR REPLACE`：request_id 主键冲突时覆盖（同一请求的重复落账
    /// 以最后一次为准，避免主键冲突报错）。一个事务批量提交把 fsync 从 N 次降到 1 次。
    fn insert_batch(&self, records: &[RequestRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction().context("开启 traces 写事务失败")?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO traces (
                        request_id, ts_ms, credential_id, model, is_streaming,
                        input_tokens, output_tokens, credits_used, latency_ms, first_token_ms,
                        outcome, retries, error_message, session_id, client_device,
                        client_ip, client_os, client_browser, cache_read_tokens, cache_creation_tokens,
                        requested_model, upstream_model, interrupted_bytes, first_attempted_credential_id
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)",
                )
                .context("预编译 traces 插入语句失败")?;
            for record in records {
                stmt.execute(params![
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
                    record.requested_model,
                    record.upstream_model,
                    record.interrupted_bytes.map(|v| v as i64),
                    record.first_attempted_credential_id.map(|v| v as i64),
                ])
                .context("INSERT traces 失败")?;
            }
        }
        tx.commit().context("提交 traces 写事务失败")?;
        Ok(())
    }

    /// 把积攒的待写记录批量落库（幂等；空队列直接返回）。
    ///
    /// 失败只告警、丢弃该批（与旧逐条落账同语义：统计侧故障不回传请求路径）。
    fn flush_pending(&self) {
        let batch = {
            let mut pend = self.pending.lock();
            pend.last_flush = std::time::Instant::now();
            std::mem::take(&mut pend.records)
        };
        if batch.is_empty() {
            return;
        }
        if let Err(e) = self.insert_batch(&batch) {
            tracing::warn!(
                "trace_db 批量落账失败（已丢弃该批 {} 条）: {e:#}",
                batch.len()
            );
            // F5/D3-1（scheduling-audit-research）：SQLite 断写只 warn 的话——面板
            // 「最近请求」的唯一数据源断写无人知（stats_stale 只盯 JSONL）。直报：
            // webhook 冷却窗口内幂等（同 key 只发一次），未配置时零开销 no-op。
            crate::common::alerting::bump("trace_db_write_failed");
        } else {
            // 落库成功且确有大宗写入：顺带检查是否该做维护（WAL checkpoint +
            // 空闲页回收，时间门控节流）。低流量（无写入）时由
            // retention_cleanup 的 6h 周期任务兜底——WAL 只在写入时增长。
            self.maybe_maintenance();
        }
    }

    /// 按 ts_ms 倒序取最近 N 条记录。
    pub fn recent(&self, limit: usize) -> Result<Vec<RequestRecord>> {
        // 先 flush 待写队列：攒批落库的延迟不得让读路径读到陈旧数据
        self.flush_pending();
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT request_id, ts_ms, credential_id, model, is_streaming,
                    input_tokens, output_tokens, credits_used, latency_ms, first_token_ms,
                    outcome, retries, error_message, session_id, client_device,
                    client_ip, client_os, client_browser, cache_read_tokens, cache_creation_tokens,
                    requested_model, upstream_model, interrupted_bytes, first_attempted_credential_id
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
        // 先 flush 待写队列（同 `recent`，读路径必须看到已落账的积攒记录）
        self.flush_pending();
        let capped = limit.clamp(1, MAX_SEARCH_LIMIT);
        let (where_sql, mut binds) = filter.build_where();

        let sql = format!(
            "SELECT request_id, ts_ms, credential_id, model, is_streaming,
                    input_tokens, output_tokens, credits_used, latency_ms, first_token_ms,
                    outcome, retries, error_message, session_id, client_device,
                    client_ip, client_os, client_browser, cache_read_tokens, cache_creation_tokens,
                    requested_model, upstream_model, interrupted_bytes, first_attempted_credential_id
             FROM traces{where_sql}
             ORDER BY ts_ms DESC
             LIMIT ? OFFSET ?"
        );

        // LIMIT / OFFSET 追加为最后两个参数（同样参数化，绝不拼进 SQL 文本）。
        binds.push(Box::new(capped as i64));
        binds.push(Box::new(offset as i64));

        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params_from_iter(binds.iter().map(|b| b.as_ref())),
            row_to_record,
        )?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.context("读取 traces 行失败")?);
        }
        Ok(out)
    }

    /// 与 [`search`](Self::search) 同一 WHERE 条件下的匹配总行数（供分页展示总数）。
    pub fn count_filtered(&self, filter: &TraceFilter) -> Result<i64> {
        self.flush_pending();
        let (where_sql, binds) = filter.build_where();
        let sql = format!("SELECT COUNT(*) FROM traces{where_sql}");

        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let n: i64 = stmt.query_row(params_from_iter(binds.iter().map(|b| b.as_ref())), |row| {
            row.get(0)
        })?;
        Ok(n.max(0))
    }

    /// 统计 traces 表当前总行数（供 admin 存储统计展示）。
    pub fn count(&self) -> Result<u64> {
        self.flush_pending();
        let conn = self.conn.lock();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM traces", [], |row| row.get(0))
            .context("统计 traces 行数失败")?;
        Ok(n.max(0) as u64)
    }

    /// 删除 ts_ms 早于 `keep_days` 天前的记录，返回删除行数。
    ///
    /// `keep_days <= 0` 时删除全部记录（无有效保留窗口）。
    ///
    /// **分批**：每批 [`DELETE_BATCH_SIZE`] 条短事务循环，直到删完——单条
    /// DELETE 持锁时间从「全表删除」降到毫秒级，大批量清理不长时间阻塞读路径；
    /// 也避免超大 DELETE 在 WAL 里产生巨型日志帧（WAL 暴涨的另一来源）。
    pub fn retention_cleanup(&self, keep_days: i64) -> Result<usize> {
        // 保留窗口起点（Unix 毫秒）：早于此时间戳的记录被清理
        // ⚠️ 用 saturating_mul：keep_days 来自 admin API（older_than_days），
        // 传 i64::MAX 时 `keep_days * 86_400_000` 在 release 下回绕成负数
        // → cutoff 落到未来 → 一次清理把全部明细静默删光。饱和后 cutoff 落在
        // 极遥远的过去 → 什么都不删（超大保留期=全保留，语义正确）。
        let cutoff_ms = chrono::Utc::now().timestamp_millis()
            - keep_days.saturating_mul(86_400_000);
        // 先落库再清理：积攒在队列里的过期记录也应被本次清理覆盖
        self.flush_pending();
        let mut total = 0usize;
        loop {
            let conn = self.conn.lock();
            // IN 子查询取「最旧的一批 request_id」再按主键删：走 idx_traces_ts_ms
            // 索引定位 + 主键删除，每批行数恒定，锁粒度可控。
            let deleted = conn
                .execute(
                    "DELETE FROM traces WHERE request_id IN (
                        SELECT request_id FROM traces WHERE ts_ms < ?1 ORDER BY ts_ms LIMIT ?2
                     )",
                    params![cutoff_ms, DELETE_BATCH_SIZE],
                )
                .context("清理过期 traces 失败")?;
            total += deleted;
            if deleted == 0 {
                break;
            }
        }
        // 大批量删除后空闲页/WAL 暴增：顺带跑一轮维护（时间门控内部节流，
        // 与写路径共用 MAINTENANCE_INTERVAL，不会连跑）。
        if total > 0 {
            self.maybe_maintenance();
        }
        Ok(total)
    }

    /// 维护入口（写路径自驱动 + 清理路径兜底）：时间门控节流，防止高频 flush
    /// 反复触发。只做两件事，均幂等、不阻塞写：
    /// 1. WAL checkpoint：日志超 [`WAL_TRUNCATE_MIN_FRAMES`] 帧时 TRUNCATE 截断
    ///    （WAL 文件有界，不再随写入无限增长）；
    /// 2. 空闲页回收：db 文件 ≥ [`VACUUM_MIN_FILE_BYTES`] 且有空闲页时
    ///    incremental_vacuum 渐进回收（每轮限 [`VACUUM_BATCH_PAGES`] 页防长锁）。
    /// 失败只告警（下轮/下次清理重试），绝不影响写路径。
    fn maybe_maintenance(&self) {
        // 时间门控：先读 pending 里的 last_maintenance；到期才执行，且就地
        // 置位当前时刻防重入（执行耗时期间其它 flush 不会重复触发）。
        let due = {
            let mut pend = self.pending.lock();
            let now = std::time::Instant::now();
            if !maintenance_due(pend.last_maintenance, now, MAINTENANCE_INTERVAL) {
                return;
            }
            pend.last_maintenance = now;
            true
        };
        if due {
            if let Err(e) = self.run_maintenance() {
                tracing::warn!("trace_db 维护（WAL checkpoint/空闲页回收）失败: {e:#}");
            }
        }
    }

    /// 执行一轮维护（不做时间门控；供 [`maybe_maintenance`](Self::maybe_maintenance)
    /// 调用）。`conn` 锁串行化保证与读写互斥；多进程共享同一 db 文件时，
    /// PASSIVE checkpoint / incremental_vacuum 遇并发写会等 busy_timeout 或
    /// 幂等失败下轮重试，均安全。
    fn run_maintenance(&self) -> Result<()> {
        let conn = self.conn.lock();
        Self::checkpoint_if_large(&conn)?;
        Self::incremental_vacuum_if_due(&conn)?;
        Ok(())
    }

    /// WAL checkpoint：PASSIVE 尽力搬回主库（遇并发读不等待、不阻塞写），
    /// 日志帧数超阈值再 TRUNCATE 截断文件。
    fn checkpoint_if_large(conn: &Connection) -> Result<()> {
        let (_busy, log_frames, _ckpt): (i64, i64, i64) = conn
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .context("WAL checkpoint(PASSIVE) 失败")?;
        if should_truncate_wal(log_frames) {
            Self::checkpoint_truncate(conn)?;
        }
        Ok(())
    }

    /// TRUNCATE 截断：把 WAL 文件截到 0 字节。并发读时返回 busy（幂等失败，
    /// 本轮跳过、下轮重试）——绝不因此阻塞。
    fn checkpoint_truncate(conn: &Connection) -> Result<()> {
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })
        .context("WAL checkpoint(TRUNCATE) 失败")?;
        Ok(())
    }

    /// 空闲页回收入口：db 文件 ≥ [`VACUUM_MIN_FILE_BYTES`] 且 `freelist_count > 0`
    /// 才执行（阈值保护：小库不触发，避免反复搬运）。
    /// WAL 下 SQLite 的 `incremental_vacuum` 不缩主库文件；真正缩文件的是启动期
    /// `VACUUM`（[`maybe_convert_legacy_db`]）。这里仍调用，便于 journal_mode 若
    /// 将来不是 WAL 时同一路径生效，失败只告警。
    fn incremental_vacuum_if_due(conn: &Connection) -> Result<()> {
        let page_count: i64 = conn
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .context("读取 page_count 失败")?;
        let page_size: i64 = conn
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .context("读取 page_size 失败")?;
        let freelist: i64 = conn
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .context("读取 freelist_count 失败")?;
        if !should_vacuum(page_count as u64 * page_size as u64, freelist as u64) {
            return Ok(());
        }
        Self::incremental_vacuum(conn, VACUUM_BATCH_PAGES)
    }

    /// 执行一轮 `incremental_vacuum`，最多回收 `max_pages` 页（渐进式防长锁；
    /// 超量空闲页由后续维护轮次继续回收）。
    fn incremental_vacuum(conn: &Connection, max_pages: i64) -> Result<()> {
        // PRAGMA 函数式语法（`incremental_vacuum(N)`），不能用 rusqlite 的
        // pragma_update（它会生成 `name = value` 形式，SQLite 不识别）。
        // 无参 = 回收全部空闲页。该 pragma 可能返回 0 列，不能 query_row(col 0)，
        // 也不能 execute（"did you mean to call query"）。execute_batch 可吃掉结果。
        let sql = if max_pages >= i64::MAX / 2 {
            "PRAGMA incremental_vacuum;".to_string()
        } else {
            format!(
                "PRAGMA incremental_vacuum({});",
                max_pages.clamp(1, 1_000_000)
            )
        };
        conn.execute_batch(&sql)
            .context("incremental_vacuum 失败")?;
        Ok(())
    }
}

impl UsageSink for TraceDb {
    fn on_record(&self, record: &RequestRecord) {
        // 攒批：先入队（不碰 DB），满 BATCH_SIZE 或距上次落库超 FLUSH_INTERVAL 时批量落库。
        // sink 不应 panic：落库失败仅告警，丢弃该批统计。
        let due = {
            let mut pend = self.pending.lock();
            pend.records.push(record.clone());
            let now = std::time::Instant::now();
            pend.records.len() >= BATCH_SIZE
                || now.duration_since(pend.last_flush) >= FLUSH_INTERVAL
        };
        if due {
            self.flush_pending();
        }
    }

    fn name(&self) -> &'static str {
        "trace_db"
    }
}

/// 停机兜底：进程正常退出时把残留的待写记录落库（优雅停机路径 pipeline worker
/// 退出后会 drop 本 sink）。失败只告警——统计明细可容忍丢失，绝不在析构路径上抛错。
impl Drop for TraceDb {
    fn drop(&mut self) {
        self.flush_pending();
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
        // 映射双口径（请求原始名 / 上游实际名）。历史库缺列时 row.get 失败 → 下面
        // 用 `get_or` 兜底 None，与 JSONL 的 serde default 同一语义。
        requested_model: row.get(20).unwrap_or(None),
        upstream_model: row.get(21).unwrap_or(None),
        // 中断字节（历史库缺列时兜底 None = 未中断，同 requested_model 模式）
        interrupted_bytes: row.get::<_, Option<i64>>(22).unwrap_or(None).map(|v| v as u64),
        // 链内首选号（历史库缺列时兜底 None = 无 failover 信息，同 interrupted_bytes 模式）
        first_attempted_credential_id: row
            .get::<_, Option<i64>>(23)
            .unwrap_or(None)
            .map(|v| v as u64),
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
        "model_unavailable" => RequestOutcome::ModelUnavailable,
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
        // 中断字节的往返验证（本样本 outcome 为 Success，仅为纯序列化往返用例）
        rec.interrupted_bytes = Some(2048);
        // 链内首选号（N4 往返验证：与 credential_id=7 构成「首选号 → 最终号」换号链形态）
        rec.first_attempted_credential_id = Some(3);
        rec.outcome = RequestOutcome::Success;
        rec.retries = 1;
        rec.error_message = Some("none".to_string());
        // 映射双口径两列（与 by_model / by_requested_model 审计口径同构：requested =
        // 客户端原始名、upstream = 改写后名）——insert→recent 往返在 roundtrip 测试与
        // trace_db_roundtrip_mapping_dimensions 中显式断言。
        rec.requested_model = Some("claude-sonnet-4".to_string());
        rec.upstream_model = Some("deepseek-v4-flash".to_string());
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
        assert_eq!(
            back.first_attempted_credential_id, Some(3),
            "链内首选号（SQLite 列）往返必须保留"
        );
        assert_eq!(back.input_tokens, 120);
        assert_eq!(back.output_tokens, 45);
        assert_eq!(back.credits_used, Some(2.5));
        assert_eq!(back.latency_ms, 1500);
        assert_eq!(back.first_token_ms, Some(300));
        assert_eq!(back.interrupted_bytes, Some(2048));
        assert_eq!(back.outcome, RequestOutcome::Success);
        assert_eq!(back.retries, 1);
        assert_eq!(back.error_message, Some("none".to_string()));
        assert_eq!(
            back.requested_model.as_deref(),
            Some("claude-sonnet-4"),
            "requested_model（客户端原始名）往返必须保留"
        );
        assert_eq!(
            back.upstream_model.as_deref(),
            Some("deepseek-v4-flash"),
            "upstream_model（改写后名）往返必须保留"
        );
        assert_eq!(back.session_id, Some("conv-1".to_string()));
        assert_eq!(back.client_device, Some("claude-code".to_string()));
        assert_eq!(back.client_ip, Some("203.0.113.7".to_string()));
        assert_eq!(back.client_os, Some("Windows".to_string()));
        assert_eq!(back.client_browser, Some("Chrome 120".to_string()));
    }

    /// 映射双口径两列（requested_model / upstream_model）的 insert→recent 往返：
    /// trace 详情页的模型双口径不被 SQLite 列级序列化/读取丢失（与 record.rs 的
    /// JSONL 序列化测试互补，本测试验证的是 DB 列往返）。
    #[test]
    fn trace_db_roundtrip_mapping_dimensions() {
        let tmp = TempDbPath::new("mapping-dims");
        let db = TraceDb::open(tmp.path()).unwrap();

        let rec = sample_record("req-map", 1_000);
        db.on_record(&rec);

        let got = db.recent(10).unwrap();
        assert_eq!(got.len(), 1);
        let back = &got[0];
        assert_eq!(back.request_id, "req-map");
        assert_eq!(
            back.requested_model.as_deref(),
            Some("claude-sonnet-4"),
            "requested_model（客户端原始名）必须保留"
        );
        assert_eq!(
            back.upstream_model.as_deref(),
            Some("deepseek-v4-flash"),
            "upstream_model（改写后名）必须保留"
        );
        assert_eq!(back.model, "claude-sonnet-4", "model 与 requested_model 同源");
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
            RequestOutcome::ModelUnavailable,
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
    fn test_retention_cleanup_huge_keep_days_does_not_wipe() {
        // 回归：keep_days 来自 admin API（older_than_days），传 i64::MAX 时
        // `keep_days * 86_400_000` 在 release 下溢出回绕成负数 → cutoff 落到未来
        // → 一次清理把全部明细静默删光。saturating_mul 后 cutoff 落在极遥远的过去
        // → 什么都不删（超大保留期 = 全保留）。旧代码在 debug 下直接 panic。
        let tmp = TempDbPath::new("retention_huge");
        let db = TraceDb::open(tmp.path()).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        db.on_record(&sample_record("keep", now));
        let deleted = db.retention_cleanup(i64::MAX).unwrap();
        assert_eq!(deleted, 0, "超大 keep_days 不得把全部明细删光");
        let got = db.recent(10).unwrap();
        assert_eq!(got.len(), 1, "明细应完整保留");
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
        rec.interrupted_bytes = None;
        rec.error_message = None;
        rec.session_id = None;
        db.on_record(&rec);

        let got = db.recent(1).unwrap();
        assert_eq!(got.len(), 1);
        let back = &got[0];
        assert_eq!(back.credential_id, None);
        assert_eq!(back.credits_used, None);
        assert_eq!(back.first_token_ms, None);
        assert_eq!(back.interrupted_bytes, None);
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

    /// 批量写：读路径必须先 flush 待写队列，保证读写一致（攒批不引入可见性延迟）。
    #[test]
    fn test_batched_inserts_are_visible_to_reads() {
        let tmp = TempDbPath::new("batch_read");
        let db = TraceDb::open(tmp.path()).unwrap();
        for i in 0..5 {
            db.on_record(&sample_record(&format!("batch-{i}"), 1000 + i));
        }
        let got = db.recent(10).unwrap();
        assert_eq!(got.len(), 5, "读路径应先 flush 待写队列");
    }

    /// 批量写：超过 BATCH_SIZE 时第一批自动落库（事务批量），余下由读路径 flush 补齐；
    /// 整批往返不丢不重。
    #[test]
    fn test_batch_size_flush_roundtrip() {
        let tmp = TempDbPath::new("batch_size");
        let db = TraceDb::open(tmp.path()).unwrap();
        for i in 0..(BATCH_SIZE + 3) {
            db.on_record(&sample_record(&format!("req-{i}"), 1000 + i as i64));
        }
        let got = db.recent(BATCH_SIZE + 3).unwrap();
        assert_eq!(
            got.len(),
            BATCH_SIZE + 3,
            "全部记录都应可读（自动批量落库 + 读路径 flush 补齐）"
        );
        let mut ids: std::collections::HashSet<_> =
            got.iter().map(|r| r.request_id.clone()).collect();
        assert_eq!(ids.len(), BATCH_SIZE + 3, "批量往返不得丢重");
    }

    /// 停机兜底：Drop 时把残留待写记录落库，重开连接仍可读到。
    #[test]
    fn test_drop_flushes_pending() {
        let tmp = TempDbPath::new("drop_flush");
        let path = tmp.path().to_path_buf();
        {
            let db = TraceDb::open(&path).unwrap();
            for i in 0..3 {
                db.on_record(&sample_record(&format!("keep-{i}"), 1000 + i));
            }
        } // 作用域结束 drop：残留记录应被落库
        let db2 = TraceDb::open(&path).unwrap();
        let got = db2.recent(10).unwrap();
        assert_eq!(got.len(), 3, "Drop 应 flush 残留待写记录");
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
        let rec = got
            .iter()
            .find(|r| r.request_id == "new-with-device")
            .unwrap();
        assert_eq!(rec.client_device, Some("claude-code".to_string()));

        // 4) 再次 open 同一库，迁移应幂等（不因列已存在而报错）
        drop(db);
        let db2 = TraceDb::open(tmp.path()).unwrap();
        assert_eq!(db2.recent(10).unwrap().len(), 2);
    }

    /// F5/D3-1：flush 落库失败必须 bump `trace_db_write_failed`（SQLite 断写直报，
    /// 不依赖 stats_stale 的间接覆盖——stats_stale 只盯 JSONL）。
    ///
    /// 构造失败方式：drop 掉 traces 表后写记录触发 flush → `insert_batch` 在
    /// prepare 阶段报 no such table。本地 HTTP server 收 payload 断言（同
    /// alerting::tests 的 bump_with_reason_payload_carries_reason 模式）。
    ///
    /// 防自弱化：删掉 flush 失败分支的 bump 行 → 本测试收不到投递 → 超时红。
    #[tokio::test]
    async fn flush_failure_bumps_trace_db_write_failed() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let _guard = crate::common::alerting::test_lock();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (body_tx, mut body_rx) = tokio::sync::mpsc::channel::<String>(4);
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if let Some(pos) = text.find("\r\n\r\n") {
                    let _ = body_tx.send(text[pos + 4..].to_string()).await;
                }
                let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
                let _ = socket.write_all(resp.as_bytes()).await;
            }
        });

        crate::common::alerting::init(
            Some(format!("http://{addr}/hook")),
            3600,
            "test".to_string(),
        );

        let tmp = TempDbPath::new("flush_fail");
        let db = TraceDb::open(tmp.path()).unwrap();
        // 破坏表结构：flush 时 insert_batch 的 prepare 必然失败（no such table）
        db.conn.lock().execute_batch("DROP TABLE traces").unwrap();
        db.on_record(&sample_record("req-fail", 1_000));
        let _ = db.recent(10); // 读路径先 flush → 失败 → warn + bump

        let body = tokio::time::timeout(std::time::Duration::from_secs(5), body_rx.recv())
            .await
            .expect("应收到告警投递")
            .expect("channel 不应关闭");
        let v: serde_json::Value = serde_json::from_str(&body).expect("payload 应为 JSON");
        assert_eq!(
            v["key"], "trace_db_write_failed",
            "flush 失败必须 bump trace_db_write_failed"
        );
    }

    // ---------- SQLite 运维三件套 ----------

    /// 三件套/回收：新库必须开启 auto_vacuum=INCREMENTAL(2)（删除页进 freelist，
    /// incremental_vacuum 的前提）。回退即 FAIL：删掉 open 里的
    /// `PRAGMA auto_vacuum=INCREMENTAL` 或建表后才设置。
    #[test]
    fn test_open_new_db_sets_incremental_auto_vacuum() {
        let tmp = TempDbPath::new("auto_vacuum");
        TraceDb::open(tmp.path()).unwrap();
        let conn = Connection::open(tmp.path()).unwrap();
        let av: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0)).unwrap();
        assert_eq!(av, 2, "新库必须开启 auto_vacuum=INCREMENTAL(2)");
    }

    /// 三件套/存量库迁移：旧库（auto_vacuum=NONE + 历史数据）open 后 header
    /// 被设为 INCREMENTAL 且数据不丢。SQLite 不能在已有页的库上只改 header，
    /// 小库也会走一次布局转换 VACUUM（≠ 512MB 空闲页回收闸门；闸门由
    /// `should_vacuum` 纯函数测试覆盖）。
    #[test]
    fn test_open_legacy_db_sets_auto_vacuum_header_without_wiping() {
        let tmp = TempDbPath::new("legacy_av");
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
            let av: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0)).unwrap();
            assert_eq!(av, 0, "旧库默认 auto_vacuum=NONE(0)");
        }
        let db = TraceDb::open(tmp.path()).unwrap();
        // 存量数据不得被转换逻辑清空；header 必须已切换 INCREMENTAL
        assert_eq!(db.count().unwrap(), 1, "存量数据不得被转换逻辑清空");
        let conn = Connection::open(tmp.path()).unwrap();
        let av: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0)).unwrap();
        assert_eq!(av, 2, "存量库 header 必须被设为 auto_vacuum=INCREMENTAL(2)");
    }

    /// 三件套/触发条件（纯函数）：WAL 帧数阈值决定 TRUNCATE 与否。
    #[test]
    fn test_should_truncate_wal_threshold() {
        assert!(!should_truncate_wal(WAL_TRUNCATE_MIN_FRAMES - 1), "未达阈值不截断");
        assert!(should_truncate_wal(WAL_TRUNCATE_MIN_FRAMES), "达到阈值截断");
        assert!(should_truncate_wal(WAL_TRUNCATE_MIN_FRAMES + 1000));
    }

    /// 三件套/触发条件（纯函数）：512MB 文件阈值 + 空闲页 > 0 双条件才回收。
    #[test]
    fn test_should_vacuum_threshold_gating() {
        assert!(
            !should_vacuum(VACUUM_MIN_FILE_BYTES - 1, 100),
            "文件不足 512MB 阈值不回收"
        );
        assert!(!should_vacuum(VACUUM_MIN_FILE_BYTES, 0), "无空闲页不回收");
        assert!(
            should_vacuum(VACUUM_MIN_FILE_BYTES, 1),
            "超阈值且有空闲页才回收"
        );
    }

    /// 三件套/触发条件（纯函数）：维护间隔门控（含 last 在未来时不 panic）。
    #[test]
    fn test_maintenance_due_gating() {
        let now = std::time::Instant::now();
        let interval = std::time::Duration::from_secs(60);
        assert!(maintenance_due(now - interval, now, interval), "恰好到期应触发");
        assert!(
            maintenance_due(now - interval - std::time::Duration::from_secs(1), now, interval),
            "超期应触发"
        );
        assert!(
            !maintenance_due(now - std::time::Duration::from_secs(59), now, interval),
            "未到期不触发"
        );
        assert!(
            !maintenance_due(now + interval, now, interval),
            "last 在未来（时钟回拨）不触发且不 panic"
        );
    }

    /// 三件套/checkpoint：TRUNCATE 后 WAL 文件必须截断为 0 字节（有界，不再
    /// 随写入无限增长）。回退即 FAIL：把 checkpoint_truncate 里的 TRUNCATE
    /// 换成 PASSIVE（或删掉）→ WAL 文件保持非零 → 红。
    #[test]
    fn test_checkpoint_truncate_empties_wal_file() {
        let tmp = TempDbPath::new("ckpt");
        let db = TraceDb::open(tmp.path()).unwrap();
        for i in 0..(BATCH_SIZE * 4) {
            db.on_record(&sample_record(&format!("ckpt-{i}"), 1000 + i as i64));
        }
        let _ = db.recent(10); // 收尾 flush：全部落库，WAL 里留有日志帧
        let wal_path = {
            let mut p = tmp.path().to_path_buf().into_os_string();
            p.push("-wal");
            std::path::PathBuf::from(p)
        };
        assert!(wal_path.exists(), "WAL 文件应存在");
        {
            let conn = db.conn.lock();
            TraceDb::checkpoint_truncate(&conn).unwrap();
        }
        let meta = std::fs::metadata(&wal_path).expect("WAL 文件应存在");
        assert_eq!(meta.len(), 0, "TRUNCATE 后 WAL 文件必须截断为 0 字节");
    }

    /// 三件套/空闲页：新库 INCREMENTAL 下删除后 freelist 增长；`incremental_vacuum`
    /// 在 WAL 模式是 SQLite 的 no-op（文件收缩靠 maybe_convert 的全量 VACUUM 闸门）。
    /// 本测试钉：header=INCREMENTAL、删除进 freelist、pragma 调用不报错。
    #[test]
    fn test_incremental_vacuum_reclaims_freelist() {
        let tmp = TempDbPath::new("vac");
        let db = TraceDb::open(tmp.path()).unwrap();
        let stats = || -> (i64, i64) {
            let conn = db.conn.lock();
            let fl = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0)).unwrap();
            let av = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0)).unwrap();
            (fl, av)
        };
        let (fl0, av) = stats();
        assert_eq!(av, 2, "新库必须是 auto_vacuum=INCREMENTAL");
        assert_eq!(fl0, 0, "新库初始无空闲页");
        for i in 0..(BATCH_SIZE * 3) {
            db.on_record(&sample_record(&format!("v-{i}"), 1000 + i as i64));
        }
        db.retention_cleanup(0).unwrap(); // 全删 → 删除页进 freelist
        let (after_delete, _) = stats();
        assert!(after_delete > 0, "删除后应产生空闲页（auto_vacuum=INCREMENTAL 生效）");
        {
            let conn = db.conn.lock();
            TraceDb::checkpoint_truncate(&conn).unwrap();
            TraceDb::incremental_vacuum(&conn, i64::MAX).unwrap();
        }
        let (_after, av2) = stats();
        assert_eq!(av2, 2, "vacuum 后 header 仍是 INCREMENTAL");
    }

    /// 三件套/分批清理：大批量删除（远超 DELETE_BATCH_SIZE）必须逐批删完，
    /// 总数正确、无残留。回退即 FAIL：把 retention_cleanup 改回单条全量
    /// DELETE（或加大批次跳过循环）——语义等价但长锁回归，本测试用
    /// 3007 条（3 整批 + 余数）钉住分批循环本身。
    #[test]
    fn test_retention_cleanup_batches_large_delete() {
        let tmp = TempDbPath::new("batch_del");
        let db = TraceDb::open(tmp.path()).unwrap();
        let n = DELETE_BATCH_SIZE as usize * 3 + 7;
        for i in 0..n {
            db.on_record(&sample_record(&format!("old-{i}"), 1_000 + i as i64));
        }
        let deleted = db.retention_cleanup(0).unwrap();
        assert_eq!(deleted, n, "分批清理必须删完所有过期记录");
        assert_eq!(db.count().unwrap(), 0, "删除后应无残留");
    }
}
