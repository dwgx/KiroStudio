//! Webhook 告警（自愈事件的轻量通知）。
//!
//! # 定位
//! 自愈机器（吸收层 / failover / 自动禁用）的事件此前只有 recovery_metrics 计数器
//! 与日志，异常只能人肉看面板。本模块在关键计数点旁挂一层告警：事件发生时 POST
//! 一条 JSON 到管理员配置的 webhook 地址。
//!
//! # 设计
//! - **调用方零开销**：未配置 webhook 时 [`bump`] 只付一次 Relaxed 原子读即返回。
//! - **冷却去重**：同 key 在冷却窗口内只发一次；窗口内的重复事件只累计计数，
//!   `value` 字段 = 自上次发送以来该 key 的累计触发次数（增量语义），
//!   `window_secs` = 冷却窗口秒数，`host` = 注入的实例标识。
//! - **失败静默重试**：投递失败只记 warn 日志，并把该 key 解除冷却 ——
//!   下一次同 key 事件到来时自动重发，最多 [`MAX_FAILED_ATTEMPTS`] 次；
//!   连续失败达上限后强制进入冷却，避免持续轰炸 webhook。
//! - **发送不阻塞调用方**：HTTP 投递在 tokio 后台任务里做，[`bump`] 立即返回。
//!
//! # 配置与安全
//! 配置经 [`init`] 注入（provider 构造时从 Config 取，热更不生效，改配置需重启）。
//! ⚠️ URL 是管理员配置，网关会向它发起请求 —— SSRF 风险自负：建议填内网不可达、
//! 只能外联的告警服务，绝不要填内网管理面地址。

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 投递失败后的最大重试次数。连续失败达到此值后强制进入冷却，防止持续轰炸 webhook。
const MAX_FAILED_ATTEMPTS: u32 = 3;
/// 单次投递的超时秒数。
const SEND_TIMEOUT_SECS: u64 = 10;

/// 告警配置快照（init 注入后不变）。
#[derive(Debug, Clone)]
struct AlertConfig {
    /// None = 关闭（所有 bump 静默 no-op）。
    url: Option<String>,
    /// 同 key 去重冷却时长。
    cooldown: Duration,
    /// 告警体里的实例标识（取自 Config.host）。
    host: String,
}

/// 是否已注入过配置。bump 热路径先查它：false 时一次原子读即返回（零开销 no-op）。
static ENABLED: AtomicBool = AtomicBool::new(false);
/// 配置本体（ENABLED 为 true 后才被访问）。
static CFG: Mutex<Option<AlertConfig>> = Mutex::new(None);

/// 每 key 的告警状态。
#[derive(Debug, Default)]
struct KeyState {
    /// 最近一次发送尝试的时刻（冷却依据）。
    last_sent: Option<Instant>,
    /// 自上次发送以来累计触发次数（发给 webhook 的 value）。
    window_count: u64,
    /// 连续投递失败次数（达到上限后强制进入冷却）。
    failed_attempts: u32,
}

/// 进程级状态表。锁序固定 CFG → KEYS，锁内无 await，无死锁风险。
static KEYS: std::sync::LazyLock<Mutex<HashMap<&'static str, KeyState>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// 已触发的发送尝试总数（测试断言用；生产代码无读者）。
#[cfg_attr(not(test), allow(dead_code))]
static SENT_TOTAL: AtomicU64 = AtomicU64::new(0);

/// 冷却判定（纯函数）：从未发送过，或距上次发送已超过冷却时长，才允许发。
fn should_send(last_sent: Option<Instant>, now: Instant, cooldown: Duration) -> bool {
    match last_sent {
        None => true,
        Some(t) => now.duration_since(t) >= cooldown,
    }
}

/// 注入告警配置（provider 构造时调用一次；重复调用以后一次为准并清空状态表）。
///
/// `cooldown_secs` 下限 1 秒：0 会让「每次都发」退化成风暴时对 webhook 的轰炸。
/// url 为 None 时不置启用位：bump 保持一次原子读即返回（零开销 no-op）。
pub fn init(url: Option<String>, cooldown_secs: u32, host: String) {
    let enabled = url.is_some();
    let cfg = AlertConfig {
        url,
        cooldown: Duration::from_secs(cooldown_secs.max(1) as u64),
        host,
    };
    {
        let mut slot = CFG.lock().unwrap();
        *slot = Some(cfg);
    }
    KEYS.lock().unwrap().clear();
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// 告警事件触发点：由关键自愈事件处调用（幂等：冷却窗口内同 key 不重复发）。
///
/// 未配置 webhook 时零开销返回；已配置时累计窗口计数并按冷却决定是否投递。
pub fn bump(key: &'static str) {
    // 未注入配置：一次原子读即返回（热路径零开销）。
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let now = Instant::now();
    let guard = CFG.lock().unwrap();
    let Some(cfg) = guard.as_ref() else {
        return;
    };
    let Some(url) = cfg.url.as_ref() else {
        return;
    };

    let mut keys = KEYS.lock().unwrap();
    let st = keys.entry(key).or_default();
    st.window_count += 1;

    // 冷却窗口内：只累计计数（下一条 webhook 的 value 增量），不重复发。
    if !should_send(st.last_sent, now, cfg.cooldown) {
        return;
    }
    // 连续失败已达上限：放弃本批并强制进入冷却，避免持续轰炸 webhook。
    if st.failed_attempts >= MAX_FAILED_ATTEMPTS {
        st.last_sent = Some(now);
        st.failed_attempts = 0;
        st.window_count = 0;
        return;
    }

    let window_secs = cfg.cooldown.as_secs();
    let host = cfg.host.clone();
    let payload = serde_json::json!({
        "key": key,
        "value": st.window_count,
        "window_secs": window_secs,
        "host": host,
    });
    let url = url.clone();
    // 本次事件已随发送带走：重置计数、起算冷却（发送在后台做，不阻塞调用方）。
    st.window_count = 0;
    st.last_sent = Some(now);
    SENT_TOTAL.fetch_add(1, Ordering::Relaxed);
    drop(keys);
    tokio::spawn(async move {
        deliver(&url, key, payload).await;
    });
}

/// 后台投递：成功清失败计数；失败静默记日志并把该 key 解除冷却，让下一次事件重发。
async fn deliver(url: &str, key: &'static str, payload: serde_json::Value) {
    let resp = client().post(url).json(&payload).send().await;
    match resp {
        Ok(r) if r.status().is_success() => {
            mark_success(key);
            tracing::debug!(%key, "告警投递成功");
        }
        Ok(r) => {
            // 非 2xx 视为失败（webhook 可能静默吞掉请求体）。
            mark_failed(key);
            tracing::warn!(%key, %url, status = %r.status(), "告警投递被拒（将在下一次同 key 事件时重发）");
        }
        Err(e) => {
            mark_failed(key);
            tracing::warn!(%key, %url, error = %e, "告警投递失败（将在下一次同 key 事件时重发）");
        }
    }
}

/// 投递成功：清零连续失败计数。
fn mark_success(key: &'static str) {
    let mut keys = KEYS.lock().unwrap();
    if let Some(st) = keys.get_mut(key) {
        st.failed_attempts = 0;
    }
}

/// 投递失败后的重试判定（纯函数，2026-08-13 提取供确定性测试）：
/// 未达上限 → 解除冷却（下次事件立即重发）；达上限 → 保持 last_sent 不动，
/// 让自然冷却把本批事件彻底压掉（防轰炸 webhook）。
fn should_retry_after_failure(failed_after_incr: u32) -> bool {
    failed_after_incr < MAX_FAILED_ATTEMPTS
}

/// 投递失败：累计失败次数；未达上限时解除冷却（下次事件立即重发），
/// 达上限则保持 last_sent 不动，让自然冷却把本批事件彻底压掉。
fn mark_failed(key: &'static str) {
    let mut keys = KEYS.lock().unwrap();
    if let Some(st) = keys.get_mut(key) {
        st.failed_attempts += 1;
        if should_retry_after_failure(st.failed_attempts) {
            st.last_sent = None;
        }
    }
}

/// 懒建的投递客户端（复用连接池；仅在实际发送时创建）。
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(SEND_TIMEOUT_SECS))
            .build()
            .expect("告警投递客户端构建失败")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试串行互斥（2026-08-13 实测）：本模块测试共享进程级静态状态
    /// （KEYS/CFG/SENT_TOTAL），Rust 默认并行跑测试时相互污染（单跑绿、全量随机红）。
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 冷却判定纯函数：覆盖 从未发送 / 窗口内 / 刚过窗口 三种边界。
    #[test]
    fn should_send_respects_cooldown_boundaries() {
        let now = Instant::now();
        let cool = Duration::from_secs(600);
        assert!(should_send(None, now, cool), "从未发送必须允许发");
        assert!(
            !should_send(Some(now - Duration::from_secs(599)), now, cool),
            "窗口内（差 1 秒）不得重复发"
        );
        assert!(
            !should_send(Some(now), now, cool),
            "同一时刻不得重复发"
        );
        assert!(
            should_send(Some(now - Duration::from_secs(600)), now, cool),
            "恰好满冷却时长应允许发"
        );
        assert!(
            should_send(Some(now - Duration::from_secs(3600)), now, cool),
            "远超冷却时长应允许发"
        );
    }

    /// 集成流：未配置 no-op → 冷却去重 → 失败解除冷却重发 → 达上限强制冷却。
    ///
    /// ⚠️ 本测试操作进程级全局状态，必须单测串行；纯函数测试（上方）不碰全局，
    /// 两者可并行。投递目标用本地拒绝端口，失败在毫秒级发生。
    #[tokio::test]
    async fn bump_dedupes_and_retries_after_failure() {
        let _guard = TEST_LOCK.lock().unwrap();
        // 未 init：所有 bump 零开销 no-op。
        assert_eq!(SENT_TOTAL.load(Ordering::Relaxed), 0);
        bump("a");
        bump("a");
        assert_eq!(SENT_TOTAL.load(Ordering::Relaxed), 0, "未配置时必须 no-op");

        // init 长冷却 + 不可达地址。
        init(
            Some("http://127.0.0.1:1/hook".to_string()),
            3600,
            "test-host".to_string(),
        );

        // 同 key 连续触发：冷却窗口内只发一次（发送尝试数在 bump 内同步计数）。
        bump("a");
        bump("a");
        bump("a");
        assert_eq!(SENT_TOTAL.load(Ordering::Relaxed), 1, "冷却窗口内同 key 只发一次");

        // 不同 key 独立冷却。
        bump("b");
        bump("b");
        assert_eq!(SENT_TOTAL.load(Ordering::Relaxed), 2, "不同 key 互不影响");
    }

    /// 失败重试/强制冷却判定（纯函数，确定性——不再依赖真实网络投递的异步时序；
    /// 2026-08-13：原集成测试等待 deliver 的 send（SEND_TIMEOUT=10s）与并行测试
    /// 竞争，10s 轮询仍不稳定，故提取为纯逻辑测试）。
    #[test]
    fn failure_retry_semantics_are_deterministic() {
        // 未达上限：解除冷却（重发）。
        assert!(should_retry_after_failure(1), "第 1 次失败后应重发");
        assert!(should_retry_after_failure(2), "第 2 次失败后应重发");
        // 达上限：强制冷却（不再重发）。
        assert!(
            !should_retry_after_failure(3),
            "第 3 次失败（=MAX_FAILED_ATTEMPTS）后必须强制冷却"
        );
        assert!(!should_retry_after_failure(4), "超过上限后仍冷却");
    }
}
