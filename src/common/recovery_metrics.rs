//! 自愈机器可观测计数器(进程级,不持久化)。
//!
//! # 为什么需要
//! 刷新 token / failover 换号 / 自动禁用死号 / 冷却 / 泄漏 token 清洗——这些"自愈机器"过去
//! 只打日志,故障排查时要 grep 日志才能回答"多少号刷新失败了 / failover 跳了几跳 / 自动禁用了
//! 几个号"。本模块把这些事件收敛成一组进程级原子计数器 + 一个查询端点,把黑箱变成可观测。
//!
//! # 设计
//! - **不持久化**:这是"自进程启动以来"的健康信号(重启即归零),不是业务数据。附 `uptime_ms`
//!   让抓取端自己算速率。
//! - **零成本**:全是 `AtomicU64::fetch_add(Relaxed)`,热路径可无脑调。
//! - **单一真相源**:各处自愈事件只调 `bump_*`,`snapshot()` 一次性导出给端点。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

/// at-rest 加密健康标志:true=最近一次凭据落盘符合加密开关预期(关→明文 / 开→真加密成功);
/// false=开了加密但上次落盘因密钥文件读写失败等回退了明文(安全预期与现实不符,UI 应告警)。
/// 初值 true(未落盘/未开加密时视为健康)。
static AT_REST_HEALTHY: AtomicBool = AtomicBool::new(true);

/// 设置 at-rest 加密健康标志(persist 时调:!enabled || 真加密成功 = true)。
pub fn set_at_rest_healthy(healthy: bool) {
    AT_REST_HEALTHY.store(healthy, Ordering::Relaxed);
}

/// 读 at-rest 加密健康标志(供 recovery-metrics 端点/UI)。
pub fn at_rest_healthy() -> bool {
    AT_REST_HEALTHY.load(Ordering::Relaxed)
}

/// 进程启动时刻(首次访问时锚定),用于 uptime_ms。
fn start_instant() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

macro_rules! counters {
    ($($field:ident : $bump:ident),* $(,)?) => {
        #[derive(Default)]
        struct Counters {
            $($field: AtomicU64,)*
        }
        static COUNTERS: OnceLock<Counters> = OnceLock::new();
        fn counters() -> &'static Counters {
            COUNTERS.get_or_init(Counters::default)
        }
        $(
            /// 自愈事件计数 +1(Relaxed,热路径零成本)。
            pub fn $bump() {
                counters().$field.fetch_add(1, Ordering::Relaxed);
            }
        )*

        /// 导出当前所有计数器 + uptime,供 /recovery-metrics 端点。
        pub fn snapshot() -> RecoveryMetricsSnapshot {
            // 首次 snapshot 也会锚定 start(若之前没 bump 过)。
            let uptime_ms = start_instant().elapsed().as_millis() as u64;
            let c = counters();
            RecoveryMetricsSnapshot {
                uptime_ms,
                at_rest_healthy: at_rest_healthy(),
                $($field: c.$field.load(Ordering::Relaxed),)*
            }
        }

        /// 计数器快照(序列化为 JSON 给前端)。字段与 `Counters` 一一对应 + uptime_ms + at_rest_healthy。
        #[derive(Debug, Clone, serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        pub struct RecoveryMetricsSnapshot {
            /// 自进程启动以来的毫秒数(抓取端据此算速率)。
            pub uptime_ms: u64,
            /// at-rest 加密健康:false=开了加密但上次落盘回退了明文(UI 告警用)。
            pub at_rest_healthy: bool,
            $(pub $field: u64,)*
        }
    };
}

// 字段名 → bump 函数名。所有自愈事件的可观测点集中在此声明。
counters! {
    // token 刷新
    refresh_ok: bump_refresh_ok,
    refresh_fail: bump_refresh_fail,
    // failover:换号跳数 + 重试预算耗尽(所有号都没成)
    failover_hops: bump_failover_hop,
    failover_exhausted: bump_failover_exhausted,
    // 自动禁用(订阅失效/可疑活动等判定为死号)
    dead_tokens_disabled: bump_dead_token_disabled,
    // 429 冷却触发次数
    cooldown_triggered: bump_cooldown_triggered,
    // 403 FEATURE_NOT_SUPPORTED 后的 region 重新探测:成功找到可用 / 全坏
    region_reprobe_ok: bump_region_reprobe_ok,
    region_reprobe_fail: bump_region_reprobe_fail,
    // 泄漏 token 清洗(#70544 幻觉 token):清洗过的请求数 + 命中 saturation 退化的请求数
    leaked_cleaned_requests: bump_leaked_cleaned_request,
    leaked_saturation_requests: bump_leaked_saturation_request,
    // 文本化工具调用(assistantResponseEvent 文本流出现 <invoke/antml:/<parameter):命中 chunk 数。
    // 取证用:量化 Kiro 把工具调用文本化的频率,决定是否值得做 R4 重组层。
    textified_invoke_hits: bump_textified_invoke,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bump_and_snapshot() {
        // 注意:全局计数器进程级共享,本测试只断言"单调不减 + bump 生效",不假设初值为 0。
        let before = snapshot();
        bump_refresh_ok();
        bump_refresh_ok();
        bump_failover_hop();
        let after = snapshot();
        assert!(after.refresh_ok >= before.refresh_ok + 2);
        assert!(after.failover_hops >= before.failover_hops + 1);
        assert!(after.uptime_ms >= before.uptime_ms);
    }

    #[test]
    fn test_snapshot_serializes_camelcase() {
        let snap = snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("uptimeMs"));
        assert!(json.contains("refreshOk"));
        assert!(json.contains("failoverHops"));
        assert!(json.contains("deadTokensDisabled"));
    }
}
