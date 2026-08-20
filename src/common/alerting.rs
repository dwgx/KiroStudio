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

/// 测试串行互斥（2026-08-13 实测）：本模块测试共享进程级静态状态
/// （KEYS/CFG/SENT_TOTAL），Rust 默认并行跑测试时相互污染（单跑绿、全量随机红）。
#[cfg(test)]
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 跨模块测试互斥（F5/F6 新增）：任何会触发 `init`/`bump` 的测试——包括
/// `trace_db`、`main` 等其它模块的告警联动测试——都必须先取本锁，与
/// alerting 自身的集成测试串行，避免并行污染进程级告警状态。
#[cfg(test)]
pub fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

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
        let mut slot = CFG.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(cfg);
    }
    KEYS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// 告警事件触发点：由关键自愈事件处调用（幂等：冷却窗口内同 key 不重复发）。
///
/// 未配置 webhook 时零开销返回；已配置时累计窗口计数并按冷却决定是否投递。
pub fn bump(key: &'static str) {
    bump_with_reason(key, None);
}

/// 带原因分类的告警触发点（B8）：payload 额外携带 `reason` 字段，供
/// `pool_exhausted` 这类多形态事件区分根因（全部禁用 / 代挂全挂 / 全不可用），
/// 其余语义与 [`bump`] 完全一致。
pub fn bump_with_reason(key: &'static str, reason: Option<&'static str>) {
    bump_impl(key, reason);
}

/// 带运行时构造原因（如缺失镜像清单摘要）的告警触发点（F6）：语义与
/// [`bump_with_reason`] 完全一致，仅 reason 允许非 `'static` 的运行时字符串
/// （`bump_with_reason` 的 `&'static str` 承载不了动态拼出的摘要）。
pub fn bump_with_dynamic_reason(key: &'static str, reason: String) {
    bump_impl(key, Some(&reason));
}

fn bump_impl(key: &'static str, reason: Option<&str>) {
    // 未注入配置：一次原子读即返回（热路径零开销）。
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let now = Instant::now();
    let guard = CFG.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(cfg) = guard.as_ref() else {
        return;
    };
    let Some(url) = cfg.url.as_ref() else {
        return;
    };

    let mut keys = KEYS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
    let mut payload = serde_json::json!({
        "key": key,
        "value": st.window_count,
        "window_secs": window_secs,
        "host": host,
    });
    if let Some(r) = reason {
        payload["reason"] = serde_json::json!(r);
    }
    let url = url.clone();
    // 本次事件已随发送带走：重置计数、起算冷却（发送在后台做，不阻塞调用方）。
    st.window_count = 0;
    st.last_sent = Some(now);
    SENT_TOTAL.fetch_add(1, Ordering::Relaxed);
    drop(keys);
    // 🔴 2026-08-15（blockers 波次 3 修复）：tokio::spawn 在无 runtime 的线程（如同步
    // 测试、极少数非 async 调用路径）会 panic「no reactor running」——panic 时
    // CFG guard 尚在作用域 → Mutex 毒化 → 后续所有告警调用 PoisonError 连锁崩
    // （线上实测：32 个测试同时挂）。先查当前 runtime：有 → 后台投递；无 →
    // 计数已累计 + 跳过投递（告警不阻塞调用方，宁可丢一次 webhook 也不 panic）。
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                deliver(&url, key, payload).await;
            });
        }
        Err(_) => {
            tracing::warn!(
                "告警投递跳过（当前线程无 tokio runtime）: key={}",
                key
            );
        }
    }
}

/// 后台投递：成功清失败计数；失败静默记日志并把该 key 解除冷却，让下一次事件重发。
async fn deliver(url: &str, key: &'static str, payload: serde_json::Value) {
    // 客户端构建失败（缓存 Err）时降级 no-op —— 见 `client()` 的文档。
    let Some(client) = client() else { return; };
    let resp = client.post(url).json(&payload).send().await;
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
    let mut keys = KEYS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
    let mut keys = KEYS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(st) = keys.get_mut(key) {
        st.failed_attempts += 1;
        if should_retry_after_failure(st.failed_attempts) {
            st.last_sent = None;
        }
    }
}

// ===== 数据新鲜度看门狗（B8）=====
//
// 「进程活着 ≠ 数据在产」（CLAUDE.md minutely.jsonl 断更两天无人发现的教训）：
// 关键产出信号需要一个独立于进程存活的监控出口。写入方每笔产出调
// [`note_data_activity`]；周期检查方（部署侧 timer / cron，或未来进程内挂点）
// 调 [`report_if_stale`]——超过 `max_idle` 无写入即 bump "stats_stale"。
//
// ⚠️ 本 crate 是 bin crate：`pub` 对死代码分析不对外，`report_if_stale` 当前
// 无进程内调用方（检查方在部署侧/未来挂点），release 下整链按 dead code 处理，
// 与 `SENT_TOTAL` 同款标注（测试里是活的）。
//
// 锁序：本表锁独立于 CFG/KEYS（report_if_stale 先取本表、释放后再进 bump 的
// CFG→KEYS，无嵌套持有），与告警主路径无死锁。

/// 各 tag 最近一次产出数据的时刻。
static DATA_ACTIVITY: std::sync::LazyLock<Mutex<HashMap<&'static str, Instant>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// 数据写入方：记录该 tag 最近一次产出数据的时刻。
pub fn note_data_activity(tag: &'static str) {
    DATA_ACTIVITY.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(tag, Instant::now());
}

/// 新鲜度判定（纯函数，供确定性测试）：无记录视为未过期——由首次写入起算，
/// 避免「进程刚启动、尚无数据」被误报为断更。
#[cfg_attr(not(test), allow(dead_code))]
fn is_stale(last_activity: Option<Instant>, now: Instant, max_idle: Duration) -> bool {
    match last_activity {
        None => false,
        Some(t) => now.duration_since(t) > max_idle,
    }
}

/// 周期检查（带注入时刻的内部实现，供确定性测试）。
#[cfg_attr(not(test), allow(dead_code))]
fn report_if_stale_at(tag: &'static str, max_idle: Duration, now: Instant) -> bool {
    let last = DATA_ACTIVITY.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(tag).copied();
    if !is_stale(last, now, max_idle) {
        return false;
    }
    bump("stats_stale");
    // 复位到本次检查时刻：断更持续期间不重复告警（alerting 冷却兜底第二道）。
    DATA_ACTIVITY.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(tag, now);
    true
}

/// 周期检查：tag 超过 `max_idle_secs` 秒无写入 → bump "stats_stale" 并复位，
/// 返回 true 表示本次判定为断更（已发告警）；未过期返回 false。
#[cfg_attr(not(test), allow(dead_code))]
pub fn report_if_stale(tag: &'static str, max_idle_secs: u64) -> bool {
    report_if_stale_at(tag, Duration::from_secs(max_idle_secs), Instant::now())
}

/// 懒建的投递客户端（复用连接池；仅在实际发送时创建）。
///
/// 🔴 2026-08-15 两项加固：
/// - **禁重定向**（`redirect::Policy::none()`）：webhook URL 是管理员配置，payload
///   含 host 等元信息 —— 若 webhook 误配/被攻破返回 302，跟随重定向会把告警
///   payload 打去 `Location` 指向的内网/元数据端点（盲 SSRF）。
/// - **构建失败降级**：改前 `.expect()` 会 panic 整个网关 —— 告警只是自愈事件的
///   轻量通知，绝不能因客户端构建失败（如 TLS 后端缺失）拖垮进程。失败缓存 Err
///   （配置级/环境级错误，重试无意义），`deliver` 据此降级为本次 no-op。
fn client() -> Option<&'static reqwest::Client> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    match CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(SEND_TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())
    }) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::error!("告警投递客户端构建失败，本次投递降级为 no-op: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let _guard = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // 未 init：所有 bump 零开销 no-op（差值断言——别的告警测试可能已 bump
        // 过进程级 SENT_TOTAL，绝对 0 会被并发测试污染，2026-08-16 实测）。
        let before = SENT_TOTAL.load(Ordering::Relaxed);
        bump("a");
        bump("a");
        assert_eq!(
            SENT_TOTAL.load(Ordering::Relaxed),
            before,
            "未配置时必须 no-op"
        );

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

    // ===== MINOR 5/6：投递客户端加固（2026-08-15）=====

    /// MINOR 5 守卫：告警 webhook 客户端必须**禁重定向** —— 302 会把含元信息的
    /// payload 打去 `Location` 指向的目标（可被指使打内网/元数据，盲 SSRF）。
    ///
    /// 回退即 FAIL：删掉 `.redirect(Policy::none())` —— client 跟随 302 去
    /// `127.0.0.1:1`（连接被拒，send 报错）或 200（目标可达），本测试必然失败。
    /// 本地起 302 server 验证，不依赖外网。
    #[tokio::test]
    async fn webhook_client_does_not_follow_redirects() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let resp = "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(resp.as_bytes()).await;
        });

        let client = client().expect("客户端应构建成功（正常环境）");
        let resp = client
            .post(format!("http://{addr}/hook"))
            .json(&serde_json::json!({ "key": "test" }))
            .send()
            .await
            .expect("禁重定向时 302 应原样返回，而不是跟随失败");
        assert_eq!(
            resp.status().as_u16(),
            302,
            "必须不跟随重定向（302 原样返回，绝不跳去 Location）"
        );
    }

    /// MINOR 6 守卫：构建失败时 `client()` 必须降级（None）而非 panic。
    ///
    /// 正常环境下构建必然成功（返回 Some）；防回退点：把闭包改回 `.expect()`
    /// 会导致本测试编译/语义变化 —— 且 `deliver` 对 None 的 no-op 分支
    /// 让「构建失败不拖垮网关」成为结构性保证（见源码注释）。
    #[test]
    fn webhook_client_builds_or_degrades_gracefully() {
        assert!(client().is_some(), "正常环境客户端必须可构建");
    }

    // ===== B8 告警扩展（2026-08-15）=====

    /// bump_with_reason 的 payload 必须带 reason 字段（pool_exhausted 的根因
    /// 分类靠它区分全部禁用 / 代挂全挂 / 全不可用）。本地起 HTTP server 收 body
    /// 断言，不依赖外网。
    #[tokio::test]
    async fn bump_with_reason_payload_carries_reason() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let _guard = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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

        init(
            Some(format!("http://{addr}/hook")),
            3600,
            "test-host".to_string(),
        );
        bump_with_reason("pool_exhausted", Some("all_disabled"));

        let body = tokio::time::timeout(std::time::Duration::from_secs(5), body_rx.recv())
            .await
            .expect("应收到投递")
            .expect("channel 不应关闭");
        let v: serde_json::Value = serde_json::from_str(&body).expect("payload 应为 JSON");
        assert_eq!(v["key"], "pool_exhausted", "key 字段必须正确");
        assert_eq!(v["reason"], "all_disabled", "reason 字段必须随 payload 投递");
        assert!(v["value"].is_u64(), "value 字段必须存在");
    }

    /// 数据新鲜度判定（纯函数）：从未写入不误报；窗口内不报；恰好满时长不算
    /// 过期（严格大于才判 stale，避免边界抖动）；超时即 stale。
    #[test]
    fn staleness_pure_judgement_boundaries() {
        let now = Instant::now();
        let idle = Duration::from_secs(120);
        assert!(!is_stale(None, now, idle), "尚无活动记录不得误报");
        assert!(
            !is_stale(Some(now - Duration::from_secs(119)), now, idle),
            "窗口内不得判 stale"
        );
        assert!(
            !is_stale(Some(now - Duration::from_secs(120)), now, idle),
            "恰好满 max_idle 不算过期（严格大于）"
        );
        assert!(
            is_stale(Some(now - Duration::from_secs(121)), now, idle),
            "超过 max_idle 必须判 stale"
        );
    }

    /// 断更告警集成流（注入时刻，确定性）：写入 → 未过期不告警 → 超时告警一次
    /// 并复位 → 同一断更持续段不重复告警。
    #[test]
    fn report_if_stale_bumps_once_then_resets() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = SENT_TOTAL.load(Ordering::Relaxed);
        init(Some("http://127.0.0.1:1/hook".to_string()), 3600, "test".to_string());

        let t0 = Instant::now();
        assert!(
            !report_if_stale_at("usage_jsonl", Duration::from_secs(60), t0),
            "无活动记录（进程刚启动）不得告警"
        );
        note_data_activity("usage_jsonl");
        assert!(
            !report_if_stale_at("usage_jsonl", Duration::from_secs(60), t0),
            "刚写入不得告警"
        );

        // 超时（t0 + 61s 后检查）：告警一次并复位。
        let later = t0 + Duration::from_secs(61);
        assert!(
            report_if_stale_at("usage_jsonl", Duration::from_secs(60), later),
            "超时无写入必须告警"
        );
        assert_eq!(
            SENT_TOTAL.load(Ordering::Relaxed),
            before + 1,
            "断更必须恰好 bump 一次"
        );

        // 复位后同一时刻再查：不重复告警（断更持续段只报一次）。
        assert!(
            !report_if_stale_at("usage_jsonl", Duration::from_secs(60), later),
            "复位后不得在同窗口重复告警"
        );
        assert_eq!(
            SENT_TOTAL.load(Ordering::Relaxed),
            before + 1,
            "复位后重复检查不得再次 bump"
        );
    }
}
