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

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    // 文本化 invoke 真重组成结构化 tool_use 的次数(R4 捞回生效计数)。
    reclaimed_invoke_calls: bump_reclaimed_invoke,
    // stray token(call/count/card/court)复读熔断触发次数(退化刷屏被截断)。
    stray_guard_tripped: bump_stray_guard_tripped,
    // stray 泄漏形态观测(纯统计,点亮 clean 层够不到的句中黑洞):见过独占 stray 行的请求数 /
    // 见过句中紧贴 CJK 的 stray 词的请求数。用于取证真机泄漏形态,决定要不要开保守句中清洗。
    stray_standalone_requests: bump_stray_standalone_seen,
    stray_inline_requests: bump_stray_inline_seen,
    // 上游 429 吸收层(provider.rs 的 'absorb 循环):额外重试轮次总数 / 吸收成功(客户端未见 429)
    // / 预算不足一轮而放弃 / 403 风控(换号空窗)按策略跳过不吸收。
    //
    // 吸收比 = absorb_recovered / (absorb_recovered + 全部放弃结局)。⚠️ 分母现在是**四项之和**
    // (budget_exhausted + backoff_truncated + retry_quota_exhausted + 各 *_skipped),
    // 不再只有 budget_exhausted —— 见下面拆分那段的理由。用旧的两项算会得到偏乐观的比值。
    // 对应外置 kiro_shield 的 1.07:1 —— 内置版必须让这个数在面板可见(shield 的统计只在它
    // 自己进程内,面板完全看不见,这正是内置化的主要理由之一)。
    absorb_rounds: bump_absorb_round,
    absorb_recovered: bump_absorb_recovered,
    absorb_budget_exhausted: bump_absorb_budget_exhausted,
    absorb_suspend_skipped: bump_absorb_suspend_skipped,
    // ── 归因拆分（此前三条不同结局挤在上面两个桶里，运维会去抬错的旋钮）──────────────
    //
    // `absorb_backoff_truncated`：号池给出的**真实**恢复时刻 > 我们愿意睡的上限
    // (`upstreamRetryAbsorbMaxDelaySecs`) ⇒ 睡醒了池子还在冷却，这一轮结构上必然拿回同一
    // 个错误。此前与「总预算装不下一轮」共用 `absorb_budget_exhausted` ⇒ 面板看到「吸收比
    // 低」时无从判断该动哪个旋钮，而实测运维会去抬 budget，真正的瓶颈是 maxDelay。
    // 两者该调的旋钮**相反**，这正是必须拆开的理由。
    absorb_backoff_truncated: bump_absorb_backoff_truncated,
    //
    // `absorb_retry_quota_exhausted`：跨轮总额度 `ABSOLUTE_MAX_TOTAL_RETRIES=12` 用尽。
    // 此前这道闸门**不 bump 任何计数器** ⇒ 这类请求既不进吸收比的分子也不进分母 ⇒
    // 面板上的吸收比是**偏乐观的**（分母里少了这一批）。而它与上面两条的区别是：
    // 它是**每请求硬上限**，抬任何 `upstreamRetryAbsorb*` 旋钮都不会改变结局。
    absorb_retry_quota_exhausted: bump_absorb_retry_quota_exhausted,
    //
    // ── 每类 AbsorbClass 各一个可分辨计数器 ────────────────────────────────────────
    // 为什么必须按类别分：新合并进来的三类（5xx / 容量 400 / 换号空窗）各有独立开关与
    // 独立退避曲线，上线后「哪一类在起作用、哪一类只是在白等」只能靠这组数回答。
    // 若共用一个 `absorb_rounds`，开三个开关后面板上看到的仍是一个数 ⇒ 无法归因，
    // 也就无法决定该关掉哪个（外挂那 11.6:1 的重试比就是不分类别一律重试的账单）。
    //
    // 语义：`*_rounds` = 该类真的**睡完退避并重打**了一轮；`*_skipped` = 该类被分类出来了
    // 但因对应开关未开而**没有**吸收（这两个数一起看才知道「开了会救回多少」）。
    absorb_rounds_pool_cooldown: bump_absorb_round_pool_cooldown,
    absorb_rounds_rate_limit: bump_absorb_round_rate_limit,
    absorb_rounds_swap_window: bump_absorb_round_swap_window,
    absorb_rounds_server_error: bump_absorb_round_server_error,
    absorb_rounds_capacity_400: bump_absorb_round_capacity_400,
    absorb_server_error_skipped: bump_absorb_server_error_skipped,
    absorb_capacity_400_skipped: bump_absorb_capacity_400_skipped,
    // 入站整形准入闸门超时(provider.rs 的 acquire_admission bail):被**网关自己**的背压
    // 挡在门外、上游根本没被请求过的客户端请求数。
    //
    // 为什么必须单独有个计数器:这类请求此前**既不 emit_record 也不 bump 任何计数器** ⇒
    // 在面板上完全不存在 ⇒ 看到的成功率是**偏乐观的**(分母里少了被自己掐掉的那批)。
    // 而"面板成功率"是本项目后续一切限流调参判断的依据 —— 依据本身有偏,调参就是在算空气。
    //
    // 与 `rate_limited` 桶的区别:那是**上游**返的 429(等一会儿真的会好);这条是网关主动
    // 限流(重试只是把同一个请求塞回同一个已满的桶)。两者混在一个桶里会让人把网关的背压
    // 误判成上游风控,进而去调**错**的旋钮(调 credentialRpmLimit 而不是 inboundTargetRpm)。
    inbound_admission_timeouts: bump_inbound_admission_timeout,
    // MCP/WebSearch 工具调用路径的失败请求数(provider.rs call_mcp_with_retry 的失败出口)。
    // 此前 MCP 只有成功分支 emit_record,失败 bail 零埋点 ⇒ 失败在面板上不可见。
    // 与 failover_exhausted 分离:那是 Kiro 对话路径的池耗尽,混在一个桶里无法归因。
    mcp_failures: bump_mcp_failure,
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
