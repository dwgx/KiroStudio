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
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

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

impl Pipeline {
    /// 非阻塞投递一条记录，返回 `true` = 进了通道，`false` = 通道满被丢弃（已计数）。
    ///
    /// 为什么单独抽出来而不是内联在 [`record`] 里：[`record`] 只能操作全局
    /// `PIPELINE`（`OnceLock` + 容量 [`CHANNEL_CAPACITY`] 写死 1 万），测试无法把它填满
    /// 也无法在同一测试二进制里换一个。抽成 `&self` 方法后，测试可以自造一个容量 1 的
    /// `Pipeline` 走**同一段**丢弃+计数+告警代码，避免"测了个复制品"。
    fn submit(&self, record: RequestRecord) -> bool {
        if self.tx.try_send(record).is_ok() {
            return true;
        }
        let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        // 同步给进程级可观测计数器（/api/admin/recovery-metrics 的出口）。
        crate::common::recovery_metrics::bump_usage_pipeline_dropped();
        // 降低日志噪音：仅在 2 的幂次时告警。
        // 用 fetch_add 的**返回值**判幂次（而非事后再 load）：丢弃风暴下多线程并发时
        // 事后 load 会让两个线程读到同一个值、或整个跨过 2 的幂次而一条都不打。
        if n.is_power_of_two() {
            tracing::warn!(
                dropped = n,
                "用量管道积压，已累计丢弃 {} 条记录（面板成功率/RPM 会偏乐观；\
                 累计值见 /api/admin/recovery-metrics 的 usagePipelineDropped）",
                n
            );
        }
        false
    }
}

static PIPELINE: OnceLock<Pipeline> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);
static WRITTEN: AtomicU64 = AtomicU64::new(0);

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
                // 分发完成才计入 written：它与 `dropped` 配对构成丢弃率的分母
                // （dropped/(dropped+written)），语义必须是"真进了下游"而不是"进了通道"。
                WRITTEN.fetch_add(1, Ordering::Relaxed);
                crate::common::recovery_metrics::bump_usage_pipeline_written();
            }
            tracing::info!("用量管道 worker 退出（通道关闭）");
        });

    if let Err(e) = spawned {
        tracing::error!("用量管道 worker 线程启动失败：{e}");
    }
}

/// 提交一条请求记录到管道（热路径调用，非阻塞）。
///
/// 未初始化或通道满时丢弃；丢弃**有出口**：[`dropped_count`]、
/// `/api/admin/recovery-metrics` 的 `usagePipelineDropped`、
/// `/api/admin/usage/overview` 的 `pipeline.dropped_records`，以及按 2 的幂次的 `warn!`。
pub fn record(record: RequestRecord) {
    #[cfg(test)]
    test_capture::push(&record);
    let Some(pipeline) = PIPELINE.get() else {
        return;
    };
    pipeline.submit(record);
}

/// 仅测试：把本线程随后 `record()` 的投递同步抄一份。
///
/// 未 `init` 时生产路径静默丢弃，单测仍能拿到记录。thread-local，不与并行测试串扰。
#[cfg(test)]
pub(crate) fn with_captured_records<T>(f: impl FnOnce() -> T) -> (T, Vec<RequestRecord>) {
    test_capture::with_captured_records(f)
}

#[cfg(test)]
mod test_capture {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        static BUF: RefCell<Option<Vec<RequestRecord>>> = const { RefCell::new(None) };
    }

    pub fn with_captured_records<T>(f: impl FnOnce() -> T) -> (T, Vec<RequestRecord>) {
        BUF.with(|slot| {
            *slot.borrow_mut() = Some(Vec::new());
        });
        let out = f();
        let recs = BUF.with(|slot| slot.borrow_mut().take().unwrap_or_default());
        (out, recs)
    }

    pub fn push(record: &RequestRecord) {
        BUF.with(|slot| {
            if let Some(buf) = slot.borrow_mut().as_mut() {
                buf.push(record.clone());
            }
        });
    }
}

/// 已丢弃的记录数（管道满导致）。
///
/// 非零即意味着面板上的成功率/RPM/token 统计**少算了这么多条**（热路径为了不阻塞请求
/// 主动放弃了它们）。出口见 [`record`] 的文档。
pub fn dropped_count() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// 已被 worker 分发给全部 sink 的记录数（即真进了聚合/SQLite 的那批）。
///
/// 单看 [`dropped_count`] 无法判读严重程度，丢弃率 = dropped/(dropped+written) 才可判读，
/// 故两个数必须成对暴露。
pub fn written_count() -> u64 {
    WRITTEN.load(Ordering::Relaxed)
}

/// 仅测试：全局 `DROPPED` 的**差值断言**串行锁。
///
/// `DROPPED` 是进程级的，而 `cargo test` 默认多线程并发跑。任何
/// `after - before == N` 的精确断言都会被另一个同时在丢记录的测试打偏（本模块与
/// `usage_handlers` 各有若干条）。所有做差值断言的测试都必须先拿这把锁。
#[cfg(test)]
static DROP_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 仅测试：造一个容量 1 的管道，把它填满后继续投递，返回**真被丢弃**的条数。
///
/// 计数落在与生产同一个全局 `DROPPED` 上（`dropped_count()` 与
/// `recovery_metrics::usage_pipeline_dropped` 读的正是它），所以 admin 出口的回归测试
/// 可以据此断言"出口读到的是真数"，而不是断言一个测试专用的影子计数器。
///
/// rx 在函数内被持有到最后一次 `submit` 之后才 drop：一旦 rx 先 drop，`try_send` 会因
/// **通道断开**而失败，那样丢弃就不再是"满"引起的，测试也就测不到目标路径。
#[cfg(test)]
pub(crate) fn fill_and_drop_for_test(attempts: usize) -> u64 {
    let (tx, rx) = mpsc::sync_channel::<RequestRecord>(1);
    let pipeline = Pipeline {
        tx,
        dropped: &DROPPED,
    };
    let mut dropped = 0u64;
    for i in 0..attempts {
        if !pipeline.submit(RequestRecord::new(format!("fill-{i}"), "m")) {
            dropped += 1;
        }
    }
    drop(rx);
    dropped
}

/// 仅测试：在 [`DROP_TEST_LOCK`] 保护下制造 `attempts-1` 次丢弃，把
/// `(丢弃前的 dropped_count, 真实丢弃条数)` 交给 `f` 断言。
///
/// 为什么要把 `before` 的读取也放进临界区：调用方若自己先读 `before` 再调用本函数，
/// 两步之间别的测试可能已经 bump 过 → 差值偏大。读 `before` 与制造丢弃必须原子。
#[cfg(test)]
pub(crate) fn with_drop_burst<T>(attempts: usize, f: impl FnOnce(u64, u64) -> T) -> T {
    let _guard = DROP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let before = dropped_count();
    let dropped = fill_and_drop_for_test(attempts);
    f(before, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

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
        let before_written = written_count();
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
        // written 与 dropped 配对：成功分发的这 5 条必须计入 written，否则丢弃率的分母是错的。
        assert!(
            written_count() >= before_written + 5,
            "worker 分发完成后应计入 written：before={before_written} now={}",
            written_count()
        );
    }

    /// ⭐ 核心回归：通道满 → 记录被丢弃 → `dropped_count()` 必须递增。
    ///
    /// 用容量 1 的真管道（[`fill_and_drop_for_test`]）走生产的 `Pipeline::submit`，
    /// 计数落在生产同一个全局 `DROPPED` 上。差值断言在 [`DROP_TEST_LOCK`] 下做。
    #[test]
    fn dropped_count_increments_when_channel_is_full() {
        // 容量 1：第 1 条进通道，其余 9 条必然被丢。
        with_drop_burst(10, |before, dropped| {
            assert_eq!(dropped, 9, "容量 1 投 10 条应丢 9 条");
            assert_eq!(
                dropped_count(),
                before + 9,
                "dropped_count() 必须与真实丢弃数一致（差值口径，全局计数器不假设初值）"
            );
        });
    }

    /// 丢弃必须同时进 `recovery_metrics`，否则 `/api/admin/recovery-metrics` 出口读到的是 0。
    ///
    /// 两个计数器是**两个源**（pipeline 本地 + 进程级），这条按差值钉死它们不漂移：
    /// 删掉 `submit` 里的 `bump_usage_pipeline_dropped()` → 本测试 FAILED。
    #[test]
    fn dropped_also_bumps_recovery_metrics_counter() {
        with_drop_burst(5, |_before, dropped| {
            assert_eq!(dropped, 4);
            // 读必须在锁内：`submit` 先 fetch_add 本地计数、再 bump 进程级计数，两步之间
            // 若被别的 burst 抢到就会读到 local=N / metrics=N-1 的中间态（假失败）。
            // 锁内保证没有并发 submit ⇒ 两个数必然已配平。
            let snap = crate::common::recovery_metrics::snapshot();
            assert!(
                snap.usage_pipeline_dropped >= dropped_count(),
                "recovery_metrics 的丢弃计数不得少于 pipeline 本地计数（缺 bump 即失衡）：\
                 metrics={} local={}",
                snap.usage_pipeline_dropped,
                dropped_count()
            );
        });
    }

    /// 告警必须按 2 的幂次上报（高频丢弃时不刷爆日志），且必须在丢弃路径上。
    ///
    /// 源码级守卫：`tracing::warn!` 与 `is_power_of_two()` 都得在 `Pipeline::submit` 里。
    /// 删掉任一 → FAILED（日志出口是"丢弃不为零时能被发现"的唯一实时手段，
    /// 面板要人去看，日志会被告警系统抓）。
    #[test]
    fn drop_path_warns_on_power_of_two_only() {
        let src = include_str!("pipeline.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let submit = prod
            .split("fn submit(")
            .nth(1)
            .expect("Pipeline::submit 不应被改名");
        // needle 运行时拼接，避免 include_str! 自匹配（本仓踩过多次）。
        let pow = format!("is_power_of_two{}{}", "(", ")");
        assert!(
            submit.contains(&pow),
            "丢弃告警必须按 2 的幂次节流，否则积压时日志刷爆"
        );
        assert!(
            submit.contains("tracing::warn!"),
            "丢弃路径必须打 warn!，否则丢弃只能靠人主动去翻面板才能发现"
        );
        assert!(
            submit.contains("bump_usage_pipeline_dropped"),
            "丢弃必须同步进 recovery_metrics，否则 admin 出口恒为 0"
        );
    }
}
