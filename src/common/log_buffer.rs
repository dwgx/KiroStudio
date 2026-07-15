//! 内存日志环形缓冲 + 实时广播(运维可观测)。
//!
//! # 为什么需要
//! 自托管网关出问题时,运维要么 SSH 上去 grep 日志,要么让用户"把日志贴到 issue"。本模块提供:
//! - 有界环形缓冲(默认 1000 条),保留最近日志,面板可拉取/导出;
//! - `tokio::broadcast` 实时推送,面板 SSE 直播新日志(无需 SSH/tail);
//! - 一个自定义 `tracing` Layer,把每条 `tracing` 事件抓成结构化 `LogEntry`。
//!
//! 纯内存、进程级、不落盘(文件日志仍由 fmt 层管);面板导出即把 ring 里的 JSONL 打包下载。

use std::collections::VecDeque;
use std::sync::OnceLock;

use parking_lot::Mutex;
use serde::Serialize;

/// 环形缓冲容量(条)。够覆盖一次故障排查的近期上下文,又不占大内存。
/// 5000 条:每条 message 通常几百字节,~数 MB 上限,覆盖更长排障窗口(搜索/回溯需要历史)。
const RING_CAPACITY: usize = 5000;
/// broadcast 通道容量。订阅者跟不上时丢最旧(lagged),不阻塞日志写入。
const BROADCAST_CAPACITY: usize = 256;

/// 一条结构化日志。字段面向"排障时想看什么"。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    /// 进程内单调递增序号(游标:客户端据此增量拉取 since=N)。
    pub seq: u64,
    /// RFC3339 毫秒时间戳。
    pub ts: String,
    /// 级别:ERROR/WARN/INFO/DEBUG/TRACE。
    pub level: String,
    /// 事件来源模块(target,如 kiro::provider)。
    pub target: String,
    /// 渲染后的消息文本(含字段)。
    pub message: String,
}

struct LogBuffer {
    ring: Mutex<VecDeque<LogEntry>>,
    seq: std::sync::atomic::AtomicU64,
    tx: tokio::sync::broadcast::Sender<LogEntry>,
}

fn buffer() -> &'static LogBuffer {
    static BUF: OnceLock<LogBuffer> = OnceLock::new();
    BUF.get_or_init(|| {
        let (tx, _rx) = tokio::sync::broadcast::channel(BROADCAST_CAPACITY);
        LogBuffer {
            ring: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
            seq: std::sync::atomic::AtomicU64::new(0),
            tx,
        }
    })
}

/// 追加一条日志:写环形缓冲(满则弹最旧)+ 尽力广播(无订阅者/满则忽略)。
fn push(level: String, target: String, message: String) {
    let buf = buffer();
    let seq = buf.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let entry = LogEntry {
        seq,
        ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        level,
        target,
        message,
    };
    {
        let mut ring = buf.ring.lock();
        if ring.len() >= RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(entry.clone());
    }
    // 广播失败(无订阅者)是正常态,忽略。
    let _ = buf.tx.send(entry);
}

/// 拉取环形缓冲快照,可选只取 seq > since 的、按 level 过滤(≥ 给定级别)。
/// `since=None` 取全部;`min_level=None` 不过滤级别。
pub fn snapshot(since: Option<u64>, min_level: Option<&str>) -> Vec<LogEntry> {
    let min_rank = min_level.map(level_rank);
    let ring = buffer().ring.lock();
    ring.iter()
        .filter(|e| since.map(|s| e.seq > s).unwrap_or(true))
        .filter(|e| min_rank.map(|m| level_rank(&e.level) >= m).unwrap_or(true))
        .cloned()
        .collect()
}

/// 订阅实时日志流(SSE 用)。返回 broadcast 接收端。
pub fn subscribe() -> tokio::sync::broadcast::Receiver<LogEntry> {
    buffer().tx.subscribe()
}

/// 级别排序:数字越大越严重。用于 min_level 过滤(≥)。
fn level_rank(level: &str) -> u8 {
    match level.to_ascii_uppercase().as_str() {
        "ERROR" => 4,
        "WARN" => 3,
        "INFO" => 2,
        "DEBUG" => 1,
        _ => 0, // TRACE / 未知
    }
}

/// 自定义 `tracing` Layer:把每条事件抓成 [`LogEntry`] 推进环形缓冲 + 广播。
/// 与 fmt 层并存(fmt 仍负责终端/文件输出),本层只额外喂内存 ring。
pub struct LogBufferLayer;

/// 访问器:把事件的所有字段拼成一条可读 message(message 字段优先,其余 key=value 追加)。
struct FieldCollector {
    message: String,
    extra: String,
}

impl tracing::field::Visit for FieldCollector {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
            // strip 掉 Debug 给字符串加的引号(message 通常是 &str)。
            if self.message.starts_with('"') && self.message.ends_with('"') && self.message.len() >= 2 {
                self.message = self.message[1..self.message.len() - 1].to_string();
            }
        } else {
            if !self.extra.is_empty() {
                self.extra.push(' ');
            }
            self.extra.push_str(&format!("{}={:?}", field.name(), value));
        }
    }
}

impl<S> tracing_subscriber::Layer<S> for LogBufferLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let meta = event.metadata();
        let mut collector = FieldCollector {
            message: String::new(),
            extra: String::new(),
        };
        event.record(&mut collector);
        let message = if collector.extra.is_empty() {
            collector.message
        } else if collector.message.is_empty() {
            collector.extra
        } else {
            format!("{} [{}]", collector.message, collector.extra)
        };
        push(meta.level().to_string(), meta.target().to_string(), message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_snapshot_and_level_filter() {
        push("INFO".into(), "test::a".into(), "hello info".into());
        push("ERROR".into(), "test::b".into(), "boom error".into());
        let all = snapshot(None, None);
        assert!(all.len() >= 2);
        // 只取 ERROR+ 级别
        let errs = snapshot(None, Some("ERROR"));
        assert!(errs.iter().all(|e| e.level == "ERROR"));
        assert!(errs.iter().any(|e| e.message.contains("boom error")));
    }

    #[test]
    fn test_since_cursor() {
        push("INFO".into(), "test::c".into(), "first".into());
        let after_first = snapshot(None, None);
        let cursor = after_first.last().unwrap().seq;
        push("INFO".into(), "test::c".into(), "second".into());
        let incremental = snapshot(Some(cursor), None);
        assert!(incremental.iter().all(|e| e.seq > cursor));
        assert!(incremental.iter().any(|e| e.message.contains("second")));
    }

    #[test]
    fn test_level_rank_ordering() {
        assert!(level_rank("ERROR") > level_rank("WARN"));
        assert!(level_rank("WARN") > level_rank("INFO"));
        assert!(level_rank("INFO") > level_rank("DEBUG"));
    }
}
