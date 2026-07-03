//! 用量统计与请求追踪
//!
//! 批次2 可观测性核心。围绕单次 API 请求的完整生命周期采集数据：
//! - `RequestRecord`：一次请求的最终结算（凭据/模型/token/credits/延迟/结果）
//! - 异步管道（G-15）：热路径只做 `try_send`，后台 worker 消费并分发到各 sink，
//!   sink 的 panic 被隔离，绝不影响请求处理
//! - sink：`trace_db`（SQLite 逐条落账）、`usage_stats`（JSONL + 内存预聚合）
//!
//! 设计目标：埋点对请求延迟零可感知影响；统计侧故障不回传到请求路径。

pub mod pipeline;
pub mod record;
pub mod trace_db;
pub mod usage_stats;

pub use pipeline::{init as init_pipeline, record as emit_record, UsageSink};
pub use record::{RequestOutcome, RequestRecord};
pub use trace_db::TraceDb;
pub use usage_stats::UsageStats;
