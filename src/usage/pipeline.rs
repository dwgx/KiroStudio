//! 异步用量管道（G-15）
//!
//! 热路径（请求处理）只调用 [`record`]，它对一个有界 mpsc 通道做非阻塞 `try_send`：
//! - 通道满时丢弃并计数（统计数据可容忍丢失，绝不阻塞请求）
//! - 后台单 worker 顺序消费，逐个分发给已注册的 [`UsageSink`]
//! - 每个 sink 的处理被 `catch_unwind` 隔离，某个 sink panic 不影响其它 sink 和请求路径
//!
//! 该模块在应用启动时通过 [`init`] 装配一次。未初始化时 [`record`] 静默丢弃，
//! 便于测试与「统计未启用」场景。
//!
//! **线程模型**：worker 跑在一个专用的 `std::thread` 上，而非 tokio 异步线程池。
//! sink 内部会做同步阻塞 IO（SQLite `execute`、文件 `writeln!`）——若跑在 tokio
//! worker 线程上，慢盘/fsync 抖动会阻塞该线程、侵蚀 tokio 线程池，把延迟传导回请求
//! 路径。用独立 OS 线程承载阻塞 IO，兑现「统计侧故障绝不回传到请求路径」的承诺。

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::OnceLock;

use super::record::RequestRecord;

/// 用量数据下游接收端
///
/// 实现者负责把记录写入自己的存储（SQLite / JSONL / 内存聚合等）。
/// 处理应尽量快且不 panic；即便 panic 也会被管道隔离。
pub trait UsageSink: Send + Sync {
    /// 消费一条请求记录
    fn on_record(&self, record: &RequestRecord);

    /// sink 名称（用于日志）
    fn name(&self) -> &'static str;
}

/// 有界通道容量：约 1 万条积压，超出则丢弃并计数
const CHANNEL_CAPACITY: usize = 10_000;

struct Pipeline {
    tx: mpsc::SyncSender<RequestRecord>,
    dropped: &'static AtomicU64,
}

static PIPELINE: OnceLock<Pipeline> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// 初始化用量管道并启动后台 worker。
///
/// `sinks` 为下游接收端集合，用 `Arc` 持有以便调用方（如 admin 查询）共享同一实例。
/// 应在应用启动时调用一次；重复调用被忽略。
pub fn init(sinks: Vec<Arc<dyn UsageSink>>) {
    // 有界同步通道：满时 try_send 立即失败（丢弃 + 计数），绝不阻塞热路径。
    let (tx, rx) = mpsc::sync_channel::<RequestRecord>(CHANNEL_CAPACITY);

    if PIPELINE
        .set(Pipeline {
            tx,
            dropped: &DROPPED,
        })
        .is_err()
    {
        tracing::warn!("用量管道已初始化，忽略重复初始化");
        return;
    }

    // 专用 OS 线程承载阻塞 IO，与 tokio 异步线程池隔离。
    let spawned = std::thread::Builder::new()
        .name("usage-pipeline".into())
        .spawn(move || {
            tracing::info!("用量管道 worker 启动，已注册 {} 个 sink", sinks.len());
            // rx.recv() 阻塞等待，通道所有发送端关闭后返回 Err，worker 退出。
            while let Ok(record) = rx.recv() {
                for sink in &sinks {
                    // 隔离每个 sink 的 panic，避免拖垮 worker 与其它 sink
                    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        sink.on_record(&record);
                    }));
                    if result.is_err() {
                        tracing::error!("用量 sink `{}` 处理记录时 panic（已隔离）", sink.name());
                    }
                }
            }
            tracing::info!("用量管道 worker 退出（通道关闭）");
        });

    if let Err(e) = spawned {
        tracing::error!("用量管道 worker 线程启动失败：{e}");
    }
}

/// 提交一条请求记录到管道（热路径调用，非阻塞）。
///
/// 未初始化或通道满时静默丢弃（丢弃计数可通过 [`dropped_count`] 观察）。
pub fn record(record: RequestRecord) {
    let Some(pipeline) = PIPELINE.get() else {
        return;
    };
    if pipeline.tx.try_send(record).is_err() {
        let n = pipeline.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        // 降低日志噪音：仅在 2 的幂次时告警
        if n.is_power_of_two() {
            tracing::warn!("用量管道积压，已累计丢弃 {} 条记录", n);
        }
    }
}

/// 已丢弃的记录数（管道满导致）
pub fn dropped_count() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    struct CountingSink {
        count: Arc<AtomicUsize>,
    }

    impl UsageSink for CountingSink {
        fn on_record(&self, _record: &RequestRecord) {
            self.count.fetch_add(1, Ordering::SeqCst);
        }
        fn name(&self) -> &'static str {
            "counting"
        }
    }

    struct PanicSink;
    impl UsageSink for PanicSink {
        fn on_record(&self, _record: &RequestRecord) {
            panic!("boom");
        }
        fn name(&self) -> &'static str {
            "panic"
        }
    }

    #[tokio::test]
    async fn test_pipeline_delivers_and_isolates_panic() {
        let count = Arc::new(AtomicUsize::new(0));
        // 注册一个 panic sink 和一个计数 sink，验证 panic 被隔离且不影响后续 sink
        init(vec![
            Arc::new(PanicSink),
            Arc::new(CountingSink {
                count: count.clone(),
            }),
        ]);

        for i in 0..5 {
            record(RequestRecord::new(format!("req-{i}"), "m"));
        }

        // 给 worker 一点时间消费
        for _ in 0..50 {
            if count.load(Ordering::SeqCst) >= 5 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(count.load(Ordering::SeqCst), 5, "计数 sink 应收到全部 5 条");
    }
}
