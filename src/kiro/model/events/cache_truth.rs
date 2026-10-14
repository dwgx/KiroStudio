//! 上游 cache 真值观测面（C-2）。
//!
//! # 背景：两条链路都建立在「上游不下发 cache 真值」之上，但这句话此前不可复验
//!
//! - `metadataEvent`（`metadata.rs`）：EXP-0 实测结论是 payload **只有** `stopReason`，
//!   从不带 `tokenUsage` / cache 真值；`tokenUsage` 因此只做防御性解析、解而不消费。
//! - `meteringEvent`（`metering.rs`）：`cacheReadInputTokens` / `cacheCreationInputTokens`
//!   是 `src/anthropic/cache.rs` 四层降级链的 **Layer 1** 真值来源，字段缺失即回落到
//!   本地估算（Layer 2 prefix → Layer 3 指纹 → Layer 4 ratio）。
//!
//! 但「上游某天开始下发」这件事此前**全仓不可观测**：真值出现只会被静默消费/静默忽略，
//! 非空率统计没有落点，C-2（真值消费口径）也就无法用线上日志定案。本模块只加观测，
//! **不改任何对外数字与降级链行为**：
//!
//! 1. **逐事件 debug**：payload 确实带 cache 相关字段时打一条 debug（字段值 + 来源事件名）；
//!    不带时**不打**（否则每请求一行，正常流量下纯噪声）。debug 级默认不落盘，需要时开
//!    `RUST_LOG=info,kirostudio::kiro::model::events=debug`（与 `base.rs` 的别名日志同一口径）。
//! 2. **周期汇总**（info 级，默认可见）：[`spawn_cache_truth_summary_task`] 每
//!    [`CACHE_TRUTH_SUMMARY_INTERVAL_SECS`] 秒打一条「近 N 秒 X 个 metadata 事件 /
//!    Y 个 metering 事件，其中带 cache 真值 M 个」+ 进程累计真值数。窗口内零事件时不打
//!    （空闲服务器不产生心跳噪声）。这是 C-2 的长期监控：M 长期为 0 ⇒ EXP-0 结论成立；
//!    某天 M > 0 ⇒ 上游开始下发，届时才谈得上消费口径。
//!
//! # 触发判据（计数与 debug 日志共用同一组谓词，杜绝「日志打了、汇总没算」漂移）
//!
//! 「带 cache 真值」= 至少一个 cache 相关字段非 `None`：
//! - metadata：`uncachedInputTokens` / `cacheReadInputTokens` / `cacheWriteInputTokens`。
//!   `outputTokens` **不算**：它不涉及缓存，单独出现不构成「上游下发 cache 真值」——
//!   算进去会让汇总里的 M 虚高，掩盖 C-2 真正关心的信号；
//! - metering：`cacheReadInputTokens` / `cacheCreationInputTokens`。
//!
//! # 观测点：解析层，而不是消费层
//!
//! 埋点在 `MetadataEvent::from_frame` / `MeteringEvent::from_frame`
//! （解析成功即观测）。`Event::from_frame` 是全仓唯一的事件解析入口（流式
//! `handlers_dispatch` / 非流式 / websearch / token_manager 用量探针都经它），
//! 所以这一处埋点覆盖全部消费方，不必在 stream.rs / handlers.rs 各改一遍（也不该
//! ——同一事实多处埋点必然漂移）。选解析层还有一个语义理由：C-2 问的是「上游 wire
//! 上到底有没有」，与下游拿它做什么无关；消费层埋点会漏掉「解析了但消费方没读」的事件。
//! 分母口径 = **解析成功的帧**（空白/坏 payload 的两个早退分支结构上不可能携带
//! cache 字段，不计数；坏帧另有 warn 日志，不会静默消失）。

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use super::metadata::MetadataEvent;
use super::metering::MeteringEvent;

/// 周期汇总间隔（秒），固定 30 分钟。
///
/// 刻意不提供配置项（对齐 `socks_nodes.rs::SOCKS_HEALTH_CHECK_INTERVAL_SECS` 的理由：
/// `model/config.rs` 不在本波改动范围，且这是观测面不是行为面）。取 1800 有仓内先例：
/// `balance_refresh_interval_secs` 默认值同为 1800（受管周期任务的既有节奏）。
/// info 级一行/30 分钟 = 48 行/天，不构成刷屏（对比 `base.rs` 注释里那场 22939 条
/// 刷屏事故）；「上游某天开始下发」最迟 30 分钟内可见，对长期监控足够。
pub(crate) const CACHE_TRUTH_SUMMARY_INTERVAL_SECS: u64 = 1800;

/// cache 真值观测累计器（纯结构：不碰全局、不打日志 —— 计数逻辑在这里单测）。
///
/// 四个字段都是**进程启动以来的单调累计**：周期汇总任务自己保存上一轮快照、
/// 相减得到窗口增量（见 [`Self::delta_since`]），因此全局只需一份累计值 ——
/// 不存在「窗口计数与累计计数两份状态互相漂移」的可能。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CacheTruthWindow {
    /// 解析成功的 metadataEvent 帧数（含别名 messageMetadataEvent）。
    pub(crate) metadata_events: u64,
    /// 解析成功的 meteringEvent 帧数。
    pub(crate) metering_events: u64,
    /// 其中带 cache 相关字段的 metadataEvent 帧数（判据见 [`metadata_has_cache_truth`]）。
    pub(crate) metadata_with_cache_truth: u64,
    /// 其中带 cache 相关字段的 meteringEvent 帧数（判据见 [`metering_has_cache_truth`]）。
    pub(crate) metering_with_cache_truth: u64,
}

impl CacheTruthWindow {
    /// 全零初值（`Mutex::new` 需要 const 构造；`Default::default()` 不是 const fn）。
    const EMPTY: Self = Self {
        metadata_events: 0,
        metering_events: 0,
        metadata_with_cache_truth: 0,
        metering_with_cache_truth: 0,
    };

    /// 记一个 metadataEvent；返回它是否带 cache 真值（调用方据此决定是否打 debug 日志）。
    pub(crate) fn record_metadata(&mut self, event: &MetadataEvent) -> bool {
        self.metadata_events += 1;
        let with_cache_truth = metadata_has_cache_truth(event);
        if with_cache_truth {
            self.metadata_with_cache_truth += 1;
        }
        with_cache_truth
    }

    /// 记一个 meteringEvent；返回它是否带 cache 真值。
    pub(crate) fn record_metering(&mut self, event: &MeteringEvent) -> bool {
        self.metering_events += 1;
        let with_cache_truth = metering_has_cache_truth(event);
        if with_cache_truth {
            self.metering_with_cache_truth += 1;
        }
        with_cache_truth
    }

    /// 带 cache 真值的帧总数（两类事件合计，即周期汇总里的「M 个」）。
    pub(crate) fn cache_truth_events(&self) -> u64 {
        self.metadata_with_cache_truth + self.metering_with_cache_truth
    }

    /// 本快照相对上一份快照的增量（周期汇总的「近 N 秒」窗口）。
    ///
    /// 用 `saturating_sub`：累计器单调递增，正常路径不会回退；防御的是将来新增
    /// 「重置计数」入口时这里宁可报 0，也不 panic / 也不 wrap 成天文数字。
    pub(crate) fn delta_since(&self, previous: &Self) -> Self {
        Self {
            metadata_events: self
                .metadata_events
                .saturating_sub(previous.metadata_events),
            metering_events: self
                .metering_events
                .saturating_sub(previous.metering_events),
            metadata_with_cache_truth: self
                .metadata_with_cache_truth
                .saturating_sub(previous.metadata_with_cache_truth),
            metering_with_cache_truth: self
                .metering_with_cache_truth
                .saturating_sub(previous.metering_with_cache_truth),
        }
    }
}

/// metadata 事件是否带 cache 相关字段。
///
/// 判据刻意**不含** `outputTokens`（见模块注释）：它不涉及缓存，算进去会让汇总的
/// 「带真值」虚高，把 C-2 的信号淹没在正常输出量里。
fn metadata_has_cache_truth(event: &MetadataEvent) -> bool {
    event.token_usage.as_ref().is_some_and(|usage| {
        usage.uncached_input_tokens.is_some()
            || usage.cache_read_input_tokens.is_some()
            || usage.cache_write_input_tokens.is_some()
    })
}

/// metering 事件是否带 cache 相关字段（Layer 1 真值的两个字段任一非 None）。
fn metering_has_cache_truth(event: &MeteringEvent) -> bool {
    event.cache_read_input_tokens.is_some() || event.cache_creation_input_tokens.is_some()
}

/// 进程级累计器（观测专用）。
///
/// 锁被毒化（持锁线程 panic）时取 `into_inner` 继续：与 `base.rs::SEEN_UNKNOWN_EVENTS`
/// 同款 —— 观测面绝不能让事件解析这条热路径失败。临界区只有几次整数自增，
/// 单请求至多两次锁（一个 metadata + 一个 metering 帧），竞争可忽略。
static CACHE_TRUTH_COUNTERS: Mutex<CacheTruthWindow> = Mutex::new(CacheTruthWindow::EMPTY);

fn lock_counters() -> MutexGuard<'static, CacheTruthWindow> {
    match CACHE_TRUTH_COUNTERS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 当前累计快照（周期汇总按轮次读它做差分）。
pub(crate) fn totals() -> CacheTruthWindow {
    *lock_counters()
}

/// 观测一个解析成功的 `metadataEvent`（含别名 `messageMetadataEvent`）。
///
/// 计数无条件 +1（该帧进入观测分母）；带 cache 相关字段时另打一条 debug ——
/// 判据与计数共用 [`metadata_has_cache_truth`]，保证「日志打了」⇔「汇总算了」。
pub(crate) fn observe_metadata(source_event: &str, event: &MetadataEvent) {
    let with_cache_truth = {
        let mut counters = lock_counters();
        counters.record_metadata(event)
    };
    if !with_cache_truth {
        return;
    }
    // 值直接打：这是本机 tracing 日志，不是对外响应，无泄漏面。
    // 不带字段时提前返回 ⇒ 正常流量零日志（debug 级本就默认不落盘）。
    if let Some(usage) = event.token_usage.as_ref() {
        tracing::debug!(
            source_event = %source_event,
            uncached = ?usage.uncached_input_tokens,
            cache_read = ?usage.cache_read_input_tokens,
            cache_write = ?usage.cache_write_input_tokens,
            output = ?usage.output_tokens,
            "metadataEvent 携带 cache 相关字段（C-2 观测：EXP-0 结论为不下发，出现即反例）"
        );
    }
}

/// 观测一个解析成功的 `meteringEvent`。
///
/// 同 [`observe_metadata`]：无条件计事件数，带 cache 字段时另打 debug。
pub(crate) fn observe_metering(source_event: &str, event: &MeteringEvent) {
    let with_cache_truth = {
        let mut counters = lock_counters();
        counters.record_metering(event)
    };
    if !with_cache_truth {
        return;
    }
    tracing::debug!(
        source_event = %source_event,
        cache_read = ?event.cache_read_input_tokens,
        cache_creation = ?event.cache_creation_input_tokens,
        "meteringEvent 携带 cache 真值字段（四层降级链 Layer 1 来源；C-2 观测：出现即上游开始下发）"
    );
}

/// 启动周期汇总任务（受管后台任务，句柄由 `AdminService` 的任务槽持有、respawn 先
/// abort 旧的；对齐 `refresh_loop.rs::spawn` 的「模块产出句柄、槽在服务侧」范式）。
///
/// 每 [`CACHE_TRUTH_SUMMARY_INTERVAL_SECS`] 秒读一次全局累计，与上一轮快照相减得窗口
/// 增量，info 级打一条；窗口内零事件时不打。先吃掉 interval 的立即首 tick：窗口从
/// 任务启动时刻起算，且不会启动即打一条（对齐 `respawn_balance_task` /
/// `respawn_socks_health_task` 的「首轮等满一个完整间隔」节奏）。
///
/// 不持 `Weak<AdminService>`：计数器是进程级全局，任务不需要访问服务实例；
/// 生命周期由槽里的句柄控制（respawn 时 abort / 进程退出即随 runtime 停）。
pub(crate) fn spawn_cache_truth_summary_task() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker =
            tokio::time::interval(Duration::from_secs(CACHE_TRUTH_SUMMARY_INTERVAL_SECS));
        // 错过的 tick 直接跳过而非补偿，防止唤醒后连打
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // 吃掉立即触发的首 tick（见函数注释）
        ticker.tick().await;
        let mut previous = totals();
        loop {
            ticker.tick().await;
            let current = totals();
            let window = current.delta_since(&previous);
            previous = current;
            // 零事件窗口不打：空闲服务器每 30 分钟一条「0 / 0」是纯噪声。
            if window.metadata_events + window.metering_events == 0 {
                continue;
            }
            tracing::info!(
                "cache 真值观测（C-2）：近 {} 秒 {} 个 metadata 事件 / {} 个 metering 事件，\
                 其中带 cache 真值 {} 个（进程累计 {} 个）",
                CACHE_TRUTH_SUMMARY_INTERVAL_SECS,
                window.metadata_events,
                window.metering_events,
                window.cache_truth_events(),
                current.cache_truth_events(),
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::events::base::EventPayload;
    use crate::kiro::parser::frame::Frame;
    use crate::kiro::parser::header::{HeaderValue, Headers};

    /// 最小事件帧：`from_frame` 只读 `:event-type` 头与 payload（不走解码器/CRC）。
    fn event_frame(event_type: &str, payload: &str) -> Frame {
        let mut headers = Headers::new();
        headers.insert(
            ":event-type".to_string(),
            HeaderValue::String(event_type.to_string()),
        );
        Frame {
            headers,
            payload: payload.as_bytes().to_vec(),
        }
    }

    fn metadata_from(json: &str) -> MetadataEvent {
        serde_json::from_str(json).expect("测试 JSON 必须可解析")
    }

    /// 累加器：带真值 → 事件数 +1 且真值数 +1；不带 → 只加事件数。两类事件各一组。
    ///
    /// 回退即 FAIL：把 `record_metadata` 改成「无条件加真值数」后，第 2 次 record 的
    /// 断言红；把 `record_metering` 与 `record_metadata` 的字段写串，后三条红。
    #[test]
    fn record_counts_truth_only_when_fields_present() {
        let mut window = CacheTruthWindow::EMPTY;

        let meta_with = metadata_from(
            r#"{"stopReason":"end_turn","tokenUsage":{"cacheReadInputTokens":900,"uncachedInputTokens":100}}"#,
        );
        let meta_without = metadata_from(r#"{"stopReason":"end_turn"}"#);
        assert!(
            window.record_metadata(&meta_with),
            "带 cache 字段必须返回 true（调用方据此打 debug）"
        );
        assert!(
            !window.record_metadata(&meta_without),
            "不带字段必须返回 false（不打 debug）"
        );
        assert_eq!(window.metadata_events, 2, "两帧都进事件分母");
        assert_eq!(
            window.metadata_with_cache_truth, 1,
            "只有带字段的那帧进真值分子"
        );

        let metering_with = serde_json::from_str::<MeteringEvent>(
            r#"{"usage":1.5,"cacheCreationInputTokens":200}"#,
        )
        .expect("metering 测试 JSON");
        let metering_without =
            serde_json::from_str::<MeteringEvent>(r#"{"usage":1.5}"#).expect("metering 测试 JSON");
        assert!(window.record_metering(&metering_with));
        assert!(!window.record_metering(&metering_without));
        assert_eq!(window.metering_events, 2);
        assert_eq!(window.metering_with_cache_truth, 1);
        assert_eq!(
            window.cache_truth_events(),
            2,
            "合计 = metadata 真值 + metering 真值"
        );
    }

    /// 负例（metadata）：字段全缺 / payload 是空对象 / 字段是 null / 只有 outputTokens
    /// —— 一律**不得**计入「带真值」；三个 cache 字段任一单独出现都算。
    ///
    /// 回退即 FAIL：把判据放宽成「tokenUsage 存在即算」后，第 2/3/4 行样例红；
    /// 漏掉任一 cache 字段（如只认 cacheRead）后，后三行样例红。
    #[test]
    fn metadata_truth_requires_a_real_cache_field() {
        for (json, expect_truth) in [
            // 字段全缺：整个 tokenUsage 不存在
            (r#"{"stopReason":"end_turn"}"#, false),
            // payload 是空对象：连 stopReason 都没有
            (r#"{}"#, false),
            // 空对象 tokenUsage：存在但没有任何字段
            (r#"{"stopReason":"end_turn","tokenUsage":{}}"#, false),
            // 字段是 null：反序列化成 None，等于「没给」
            (
                r#"{"stopReason":"end_turn","tokenUsage":{"cacheReadInputTokens":null,"cacheWriteInputTokens":null,"uncachedInputTokens":null}}"#,
                false,
            ),
            // 只有 outputTokens：不涉及缓存，不算 cache 真值
            (
                r#"{"stopReason":"end_turn","tokenUsage":{"outputTokens":7}}"#,
                false,
            ),
            // 只有 cacheRead：算
            (
                r#"{"stopReason":"end_turn","tokenUsage":{"cacheReadInputTokens":1}}"#,
                true,
            ),
            // 只有 uncached：算（它同样是上游对缓存语义的显式表态）
            (
                r#"{"stopReason":"end_turn","tokenUsage":{"uncachedInputTokens":1}}"#,
                true,
            ),
            // 只有 cacheWrite：算
            (
                r#"{"stopReason":"end_turn","tokenUsage":{"cacheWriteInputTokens":1}}"#,
                true,
            ),
        ] {
            let event = metadata_from(json);
            assert_eq!(
                metadata_has_cache_truth(&event),
                expect_truth,
                "谓词判据与样例不符：{json}"
            );
            // 累加器必须同判据（不允许「谓词说 false、record 却 +1」的漂移）。
            let mut window = CacheTruthWindow::EMPTY;
            assert_eq!(
                window.record_metadata(&event),
                expect_truth,
                "record 返回值漂移：{json}"
            );
            assert_eq!(
                window.metadata_with_cache_truth,
                u64::from(expect_truth),
                "record 计数漂移：{json}"
            );
        }
    }

    /// 负例（metering）：无 cache 字段 / 字段是 null 不算；任一 cache 字段非 None 才算。
    ///
    /// 回退即 FAIL：判据放宽成「字段存在（哪怕 null）即算」后，第 2 行样例红。
    #[test]
    fn metering_truth_requires_a_real_cache_field() {
        for (json, expect_truth) in [
            (r#"{"usage":1.5}"#, false),
            (
                r#"{"usage":1.5,"cacheReadInputTokens":null,"cacheCreationInputTokens":null}"#,
                false,
            ),
            (r#"{"usage":1.5,"cacheReadInputTokens":600}"#, true),
            (r#"{"usage":1.5,"cacheCreationInputTokens":200}"#, true),
        ] {
            let event: MeteringEvent = serde_json::from_str(json).expect("测试 JSON 必须可解析");
            assert_eq!(metering_has_cache_truth(&event), expect_truth, "{json}");
            let mut window = CacheTruthWindow::EMPTY;
            assert_eq!(window.record_metering(&event), expect_truth, "{json}");
            assert_eq!(
                window.metering_with_cache_truth,
                u64::from(expect_truth),
                "{json}"
            );
        }
    }

    /// 窗口增量：周期汇总用「当前累计 − 上一轮快照」得到「近 N 秒」的数字。
    ///
    /// 回退即 FAIL：字段错配（如把 metadata 的增量算到 metering 头上）即红；
    /// `saturating_sub` 被换成裸减法后，回退方向会 panic/wrap，最后一条红。
    #[test]
    fn delta_since_yields_window_delta() {
        let previous = CacheTruthWindow {
            metadata_events: 10,
            metering_events: 20,
            metadata_with_cache_truth: 1,
            metering_with_cache_truth: 2,
        };
        let current = CacheTruthWindow {
            metadata_events: 13,
            metering_events: 20,
            metadata_with_cache_truth: 2,
            metering_with_cache_truth: 2,
        };
        assert_eq!(
            current.delta_since(&previous),
            CacheTruthWindow {
                metadata_events: 3,
                metering_events: 0,
                metadata_with_cache_truth: 1,
                metering_with_cache_truth: 0,
            }
        );
        // 无新事件 → 全零（周期任务据此跳过日志）。
        assert_eq!(current.delta_since(&current), CacheTruthWindow::EMPTY);
        // 理论上不会回退（累计单调），但防御性 saturate：不 panic、不 wrap。
        assert_eq!(previous.delta_since(&current), CacheTruthWindow::EMPTY);
    }

    /// 解析层埋点：`from_frame` 解析成功即计入全局（流式/非流式/websearch/用量探针
    /// 全部经 `Event::from_frame` → 这里；埋点被删即所有消费方失明）。
    ///
    /// 用「>= before + 1」下界而非等值断言：计数器是进程级全局，同二进制内其它测试
    /// （如 `base.rs` 的别名用例）可能并行加计数 —— 单调下界既非 flaky，又能在
    /// 「埋点被删/被短路」时变红（极小概率被并行增量假绿，故另有源码守卫钉死）。
    #[test]
    fn from_frame_observes_metadata_and_metering() {
        let before = totals();

        let metadata = MetadataEvent::from_frame(&event_frame(
            "metadataEvent",
            r#"{"stopReason":"end_turn","tokenUsage":{"cacheReadInputTokens":1234}}"#,
        ))
        .expect("合法 metadata 帧必须解析成功");
        assert!(metadata.token_usage.is_some(), "前置：观测不得改动解析结果");

        let metering = MeteringEvent::from_frame(&event_frame(
            "meteringEvent",
            r#"{"usage":1.5,"cacheReadInputTokens":600}"#,
        ))
        .expect("合法 metering 帧必须解析成功");

        let after = totals();
        assert!(
            after.metadata_events >= before.metadata_events + 1,
            "metadataEvent::from_frame 未计入事件数（解析层埋点被删/短路？）"
        );
        assert!(
            after.metadata_with_cache_truth >= before.metadata_with_cache_truth + 1,
            "带 cacheRead 的 metadata 帧未计入真值数"
        );
        assert!(
            after.metering_events >= before.metering_events + 1,
            "meteringEvent::from_frame 未计入事件数"
        );
        assert!(
            after.metering_with_cache_truth >= before.metering_with_cache_truth + 1,
            "带 cacheRead 的 metering 帧未计入真值数"
        );
        assert_eq!(metering.usage, 1.5, "前置：观测不得改动解析结果");
    }

    /// 源码级守卫：两个 `from_frame` 里的观测埋点必须还在。
    ///
    /// 运行期下界断言在「并行增量假绿」的极小窗口下可能漏红（见上一条注释），
    /// 源码守卫把「埋点被删」这一事实钉死 —— 两条一起才无死角。
    /// needle 运行时拼接：写成完整字面量会让 include_str! 读到本测试自己。
    #[test]
    fn from_frame_source_keeps_observe_calls() {
        let metadata_src = include_str!("metadata.rs");
        let metering_src = include_str!("metering.rs");
        let metadata_needle = format!("observe_{}", "metadata(");
        let metering_needle = format!("observe_{}", "metering(");
        assert!(
            metadata_src.contains(&metadata_needle),
            "metadata.rs::from_frame 的 C-2 观测埋点被删（线上 cache 真值观测失明）"
        );
        assert!(
            metering_src.contains(&metering_needle),
            "metering.rs::from_frame 的 C-2 观测埋点被删（线上 cache 真值观测失明）"
        );
    }
}
