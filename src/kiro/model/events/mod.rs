//! 事件模型
//!
//! 定义 generateAssistantResponse 流式响应的事件类型

mod assistant;
mod base;
mod cache_truth;
mod context_usage;
mod metadata;
mod metering;
mod reasoning;
mod tool_use;

pub use assistant::AssistantResponseEvent;
// tool_use XML 泄漏过滤的两条防线共用的判据：流层（anthropic/stream.rs 的跨 chunk 状态机）
// 与帧层（assistant.rs::from_frame 就地剥离）必须共用同一组字面量与同一个收尾剥离函数，
// 否则两层判据漂移（一层剥、一层不剥 = 泄漏仍会穿透）。`pub(crate)` 而非 `pub`：
// 这是内部实现细节，不属于 events 模块对外的事件类型 API。
pub(crate) use assistant::{
    strip_tool_use_xml_leaks, TOOL_USE_XML_CLOSE, TOOL_USE_XML_PREFIX,
};
pub use base::Event;
// C-2 上游 cache 真值观测面（详见 cache_truth.rs）：逐事件埋点由 metadata/metering
// 的 from_frame 调用（模块内直取 `cache_truth::observe_*`，不经这里）；周期汇总任务的
// 启动入口由 admin 服务槽（`respawn_cache_truth_summary_task`）消费，故只导出这两个。
pub(crate) use cache_truth::{CACHE_TRUTH_SUMMARY_INTERVAL_SECS, spawn_cache_truth_summary_task};
pub use context_usage::ContextUsageEvent;
pub use metadata::MetadataEvent;
pub(crate) use metadata::map_metadata_stop_reason;
pub use metering::MeteringEvent;
pub use reasoning::ReasoningContentEvent;
pub use tool_use::ToolUseEvent;
