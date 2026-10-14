//! 隐藏上下文计量（`observe_hidden_context_sample` + 滚动窗口状态机）的回归测试。
//!
//! 钉住四类行为：
//! 1. 基线只在窗口**填满**时建立（提前建立 = 用半个窗口的噪声当基线；从不建立 = 漂移
//!    监控形同虚设）；
//! 2. 漂移告警只在中位数出带 / 带外进一步移动时发生（每样本都告警 = 日志刷屏、真信号
//!    被淹没；阈值改动不得让「带内小抖动」告警）；
//! 3. 回到带内后告警重新武装（不重新武装 = 同一漂移第二次出现时静默漏报）；
//! 4. 两条生产路径（流式 `StreamContext` / 非流式 handlers 聚合循环）都接上了观测，
//!    且共用同一个函数（与 `context_usage_predicate_must_be_shared` 同一教训：
//!    只接一侧或各写一份 = 样本分叉）。
//!
//! 另有三条源码级守卫（非流式路径没有可跑的单测，见文件末尾）：接线共用、
//! 「状态机事件 → 日志级别」映射、观测实现唯一性（不许并列第二份实现分叉样本）。
//!
//! 全局表（`HIDDEN_CTX_REGISTRY`）按模型名分桶；本模块用专属模型名
//! （`hidden-ctx-test-*`）与同二进制内其它测试隔离 —— 不依赖执行顺序。
use super::*;
use crate::kiro::model::events::ContextUsageEvent;

/// 填满一个窗口所需的样本数。
const FULL: usize = HIDDEN_CTX_WINDOW;

/// 连续推入 `times` 个相同样本，返回每个样本触发的状态机事件。
fn fill(window: &mut HiddenCtxWindow, delta: i64, times: usize) -> Vec<HiddenCtxEvent> {
    (0..times).map(|_| window.record(delta)).collect()
}

/// 全局表的 (样本数, 中位数) 快照。测试侧直接读私有字段实现 —— 生产段不留
/// `#[cfg(test)]` 的死代码，也避免破坏本仓源码守卫「生产段 = 首个 #[cfg(test)] 之前」
/// 的切法（守卫自身的裁剪逻辑依赖这条约定）。
fn global_snapshot(model: &str) -> Option<(usize, i64)> {
    let registry = HIDDEN_CTX_REGISTRY.lock();
    let window = registry.windows.get(model)?;
    Some((window.samples.len(), window.median()?))
}

/// 生产段归一化：切到首个 `#[cfg(test)]` 之前，剔注释行 + 空白折叠。
///
/// 同 `context_usage_predicate_must_be_shared` 的理由：注释里的函数名不算接线；
/// rustfmt 换行不得让守卫失效。本模块多条源码守卫共用（调用方按需再切片）。
fn production_normalized(src: &str) -> String {
    let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
    src[..cut]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 取生产段中某函数的正文切片：从签名起到下一个函数定义前。
///
/// 归一化后以 ` fn ` 作分界（本观测函数体内没有内嵌 `fn` 项；将来若在函数体内新增，
/// 切片会提前截断——守卫变红而非静默放行）。找不到签名 → panic（守卫前置失败即红）。
fn production_fn_body<'a>(src: &'a str, signature: &str) -> &'a str {
    let start = src
        .find(signature)
        .unwrap_or_else(|| panic!("生产段必须存在 `{signature}`"));
    let rest = &src[start..];
    match rest.find(" fn ") {
        Some(end) => &rest[..end],
        None => rest,
    }
}

/// 基线只在窗口填满时建立：第 14 个样本仍 Collecting，第 15 个才 BaselineEstablished。
///
/// 把守卫改坏的样子：`record` 里提前建基线（如首个样本即 `baseline = Some(delta)`）→
/// 前半段断言红；永远不建基线 → 后半段断言红。
#[test]
fn baseline_is_established_only_when_window_fills() {
    let mut w = HiddenCtxWindow::default();
    let events = fill(&mut w, 4000, FULL - 1);
    assert!(
        events.iter().all(|e| *e == HiddenCtxEvent::Collecting),
        "窗口未满前只能是 Collecting，实测 {events:?}"
    );
    assert_eq!(w.baseline, None, "窗口未满不得建立基线");
    assert_eq!(w.record(4000), HiddenCtxEvent::BaselineEstablished(4000));
    assert_eq!(w.baseline, Some(4000));
}

/// 中位数在阈值内（±100）不告警；出带即告警一次，且带外不重复告警。
///
/// 把守卫改坏的样子：阈值改小（如 50）→ 「+90 只应 Quiet」红；阈值改大（如 500）→
/// 「出带恰好 1 次 Drift」红。
#[test]
fn drift_alerts_only_after_median_leaves_band() {
    let mut w = HiddenCtxWindow::default();
    let _ = fill(&mut w, 4000, FULL); // 基线 4000

    // +90：阈值内。15 个样本换完后中位数 = 4090，仍不得告警。
    let in_band = fill(&mut w, 4090, FULL);
    assert!(
        in_band.iter().all(|e| *e == HiddenCtxEvent::Quiet),
        "+90 在阈值内（≤100），只应 Quiet 不得 Drift，实测 {in_band:?}"
    );

    // +300：第 8 个样本起中位数翻到 4300 → 恰好一次 Drift（带外未进一步移动不重复）。
    let out = fill(&mut w, 4300, FULL);
    let drifts: Vec<_> = out
        .iter()
        .filter(|e| matches!(e, HiddenCtxEvent::Drift { .. }))
        .collect();
    assert_eq!(drifts.len(), 1, "出带应恰好告警一次，实测 {out:?}");
    assert_eq!(
        *drifts[0],
        HiddenCtxEvent::Drift {
            baseline: 4000,
            median: 4300
        }
    );
}

/// 带外「进一步移动 > 阈值」才再次告警；中位数不动时不得重复 warn。
///
/// 把守卫改坏的样子：删掉 `last_alerted_median` 的 `further_moved` 判定（出带即告警）→
/// 15 个静止样本产出 15 条 warn → 「全 Quiet」红。
#[test]
fn drift_does_not_repeat_without_further_movement() {
    let mut w = HiddenCtxWindow::default();
    let _ = fill(&mut w, 4000, FULL);
    let _ = fill(&mut w, 4300, FULL); // 一次 Drift（median 4300）

    let still = fill(&mut w, 4300, FULL);
    assert!(
        still.iter().all(|e| *e == HiddenCtxEvent::Quiet),
        "中位数不动时不得重复告警，实测 {still:?}"
    );

    let moved = fill(&mut w, 4700, FULL);
    let drifts = moved
        .iter()
        .filter(|e| matches!(e, HiddenCtxEvent::Drift { .. }))
        .count();
    assert_eq!(
        drifts, 1,
        "带外进一步移动超过阈值应再告警一次，实测 {moved:?}"
    );
}

/// 回到带内 → 恰好一次 Recovered，并**重新武装**：同一漂移再次出现必须再次告警。
///
/// 把守卫改坏的样子：恢复时不 `take()` 清 `last_alerted_median` → 第二次漂到同一中位数
/// 时 `further_moved = |4300-4300| = 0` → 静默漏报 → 第二次断言红。
#[test]
fn recovery_rearms_the_alert() {
    let mut w = HiddenCtxWindow::default();
    let _ = fill(&mut w, 4000, FULL);
    let _ = fill(&mut w, 4300, FULL);

    let back = fill(&mut w, 4000, FULL);
    let recovered = back
        .iter()
        .filter(|e| matches!(e, HiddenCtxEvent::Recovered { .. }))
        .count();
    assert_eq!(
        recovered, 1,
        "回到带内应恰好记一次 Recovered，实测 {back:?}"
    );

    let again = fill(&mut w, 4300, FULL);
    let drifts = again
        .iter()
        .filter(|e| matches!(e, HiddenCtxEvent::Drift { .. }))
        .count();
    assert_eq!(
        drifts, 1,
        "恢复后同一漂移再次出现必须再次告警，实测 {again:?}"
    );
}

/// 中位数对少数离群样本免疫：15 个样本里 4 个巨值不移动基线、不触发告警。
///
/// 把守卫改坏的样子：median 换成 mean → 基线变成 4000 + 4×(999999−4000)/15 ≈ 269k → 红。
#[test]
fn median_is_robust_to_minority_outliers() {
    let mut w = HiddenCtxWindow::default();
    let mut events = fill(&mut w, 4000, FULL - 4);
    events.extend(fill(&mut w, 999_999, 4));
    assert_eq!(
        events.last(),
        Some(&HiddenCtxEvent::BaselineEstablished(4000)),
        "4/15 离群不得移动中位数（均值口径会），实测 {events:?}"
    );
    assert_eq!(w.baseline, Some(4000));
}

/// 模型表容量守卫：第 17 个模型被丢弃（防客户端伪造唯一模型名灌爆内存），
/// 已跟踪模型不受影响。
///
/// 把守卫改坏的样子：删掉容量判定 → 溢出模型 observe 返回 Some → 第一条断言红。
#[test]
fn registry_drops_new_models_when_table_is_full() {
    let mut reg = HiddenCtxRegistry::default();
    for i in 0..HIDDEN_CTX_MAX_MODELS {
        let model = format!("hidden-ctx-cap-{i}");
        assert!(reg.observe(&model, 1).is_some(), "第 {i} 个模型应可入表");
    }
    assert!(
        reg.observe("hidden-ctx-cap-overflow", 1).is_none(),
        "表满后新模型必须被丢弃（否则是一条无界内存路径）"
    );
    assert!(
        reg.observe("hidden-ctx-cap-0", 1).is_some(),
        "已跟踪的模型不受表满影响"
    );
}

/// 全局入口的值契约：任一端 ≤0 拒绝（None）；合法样本返回 delta（含负差值，
/// 负差值 = 本地估算多算，同样是有效观测，不得 clamp 成 0）。
///
/// 把守卫改坏的样子：删掉非正值守卫 → `(0, 100)` 返回 Some(−100) → 红。
#[test]
fn global_entry_value_contract() {
    let model = "hidden-ctx-test-entry";
    assert_eq!(observe_hidden_context_sample(model, 0, 100, None), None);
    assert_eq!(observe_hidden_context_sample(model, 100, 0, None), None);
    assert_eq!(observe_hidden_context_sample(model, -5, 100, None), None);
    assert_eq!(
        observe_hidden_context_sample(model, 5000, 1000, None),
        Some(4000)
    );
    assert_eq!(
        observe_hidden_context_sample(model, 900, 1000, None),
        Some(-100),
        "负差值必须保留（本地估算多算的合法观测）"
    );
}

/// 流式路径端到端：喂一个有效 contextUsageEvent → 全局表记录 1 个样本；
/// 同请求再来第二个有效信号 → **不得**再记（首个有效信号规则，防重复样本灌窗）。
///
/// 把守卫改坏的样子：删掉 `first_valid_signal` 判定 → 第二次调用后 snapshot 变
/// `(2, 3000)` → 第二条断言红。
#[test]
fn stream_path_records_exactly_one_sample_per_request() {
    // 未知模型 → 默认窗口 200k；1.0% → 2000。
    let model = "hidden-ctx-test-stream-once";
    let mut ctx = StreamContext::new_with_thinking(model, 1000, false, HashMap::new());

    ctx.process_kiro_event(&Event::ContextUsage(ContextUsageEvent {
        context_usage_percentage: 1.0,
    }));
    assert_eq!(
        global_snapshot(model),
        Some((1, 1000)),
        "首个有效信号应记入 1 个样本（delta = 2000 − 1000）"
    );

    ctx.process_kiro_event(&Event::ContextUsage(ContextUsageEvent {
        context_usage_percentage: 2.0,
    }));
    assert_eq!(
        global_snapshot(model),
        Some((1, 1000)),
        "同一请求的后续 contextUsageEvent 不得再记样本"
    );
}

/// 脏值（0/负数/NaN/inf）不产生样本 —— 与下界守卫同一判据：无效信号连观测都不该产生。
///
/// 把守卫改坏的样子：把观测挪到 `context_input_tokens_from_pct` 的 Some 分支之外
/// （或脏值也调观测）→ 该模型入表、snapshot 变 Some → 红。
#[test]
fn invalid_context_usage_signals_do_not_record_samples() {
    let model = "hidden-ctx-test-stream-dirty";
    let mut ctx = StreamContext::new_with_thinking(model, 1000, false, HashMap::new());
    for bad in [0.0_f64, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        ctx.process_kiro_event(&Event::ContextUsage(ContextUsageEvent {
            context_usage_percentage: bad,
        }));
    }
    assert_eq!(
        global_snapshot(model),
        None,
        "脏值不得产生任何样本（模型甚至不应入表）"
    );
}

/// 🔴 源码级守卫：隐藏上下文观测必须**同时接在两条主路径**上，且共用同一个函数。
///
/// 与 `context_usage_predicate_must_be_shared` 同一教训：同一观测只接一侧、或第二处
/// 自己写一份，都会分叉（漂移监控依赖两侧同源样本）。非流式路径没有可跑的单测
/// （需真实上游 + `CallMeta`/`InflightGuard`），故用源码断言钉住接线。
///
/// 把守卫改坏的样子：删掉任一调用点 → 对应断言红；调用点被换成自写实现
/// （不再出现共享函数名）→ 同样红。
#[test]
fn hidden_context_observation_must_be_wired_into_both_main_paths() {
    let handlers = production_normalized(include_str!("handlers.rs"));
    let stream = production_normalized(include_str!("stream.rs"));

    // 非流式：必须以**全限定名**调用共享函数（本文件没有它的定义）。
    // needle 运行时拼接，避免本测试自己的字面量污染匹配（本仓守卫惯例）。
    let qualified = format!("stream::observe_hidden_context_sample{}", "(");
    assert!(
        handlers.contains(&qualified),
        "非流式聚合循环必须调用共享的隐藏上下文观测函数 `{qualified}`（不得自写一份）"
    );

    // 流式：定义在本文件，调用点必须存在 —— 出现次数 ≥ 2（1 次定义 + ≥1 次调用）。
    // 只数出现次数而不是匹配整行调用：对 rustfmt 的换行布局免疫。
    let call = format!("observe_hidden_context_sample{}", "(");
    let occurrences = stream.matches(&call).count();
    assert!(
        occurrences >= 2,
        "stream.rs 生产段必须保留观测函数的定义与流式调用点（当前仅 {occurrences} 次出现）"
    );
}

/// 🔴 源码级守卫：`observe_hidden_context_sample` 的「状态机事件 → 日志级别」映射。
///
/// 为什么需要：本文件的状态机测试钉的是 `HiddenCtxEvent` 事件本身，**不**钉事件到
/// 日志的映射 —— 把 `Collecting | Quiet => {}` 改成 `warn!` 不会让任何既有断言变红。
/// 而 Quiet 是窗口填满后**每个样本都经过**的稳态路径，一条 warn/info/error 就是刷屏
/// （与 `SEEN_UNKNOWN_EVENTS` 注释里那场 22939 条刷屏同类事故）；日志断言需要捕获
/// subscriber（不引新依赖做不到），故照本仓惯例用 `include_str!` + 切片断言钉映射。
///
/// 钉住的事实（改坏即红）：
/// - 静止期分支（Collecting/Quiet 到 BaselineEstablished 之间）不得出现
///   `warn!`/`error!`/`info!` —— 改成任一即红（debug 不受限：正常流量不落盘）；
/// - `Drift` 分支必须是 `tracing::warn!` —— 降级成 info/debug（告警静默化）即红；
/// - `BaselineEstablished` / `Recovered` 分支必须是 `tracing::info!` 且不得是 warn；
/// - 整个函数里 `tracing::warn!` 恰好一处（就是 Drift）—— 在映射之外另加 warn 即红。
#[test]
fn hidden_context_event_log_levels_are_pinned() {
    let stream = production_normalized(include_str!("stream.rs"));
    let body = production_fn_body(&stream, "fn observe_hidden_context_sample");
    assert!(
        body.contains("match event {"),
        "事件→日志映射的 match 必须仍在观测函数内（被搬走即红）"
    );

    // 切片工具：`from` 锚点到下一个 `to` 锚点（`to=None` 取到函数末尾）。
    // 锚点找不到直接 panic —— 分支被删/改名即红（守卫前置失败，不得静默通过）。
    fn region<'a>(hay: &'a str, from: &str, to: Option<&str>) -> &'a str {
        let start = hay
            .find(from)
            .unwrap_or_else(|| panic!("生产段必须含 `{from}`（分支被删/改名即红）"));
        let rest = &hay[start..];
        match to {
            Some(t) => {
                let end = rest
                    .find(t)
                    .unwrap_or_else(|| panic!("`{from}` 之后必须含 `{t}`（分支被删/重排即红）"));
                &rest[..end]
            }
            None => rest,
        }
    }

    // ① 静止期（Collecting/Quiet）：不得有任何非 debug 日志。
    let quiet = region(
        body,
        "HiddenCtxEvent::Collecting",
        Some("HiddenCtxEvent::BaselineEstablished"),
    );
    for level in ["warn!", "error!", "info!"] {
        assert!(
            !quiet.contains(level),
            "静止期分支不得出现 `{level}`（每样本都经过 → 刷屏），实际切片: {quiet}"
        );
    }

    // ② Drift 必须走 warn：漂移告警是这套观测唯一的对外信号，降级/删除即静默化。
    let drift = region(
        body,
        "HiddenCtxEvent::Drift",
        Some("HiddenCtxEvent::Recovered"),
    );
    assert!(
        drift.contains("tracing::warn!"),
        "Drift 分支必须是 tracing::warn!，实际切片: {drift}"
    );

    // ③ 基线建立 / 回到带内：info 级（正常路径里程碑，不是告警）。
    let baseline = region(
        body,
        "HiddenCtxEvent::BaselineEstablished",
        Some("HiddenCtxEvent::Drift"),
    );
    assert!(
        baseline.contains("tracing::info!"),
        "基线建立必须是 tracing::info!，实际切片: {baseline}"
    );
    assert!(
        !baseline.contains("warn!"),
        "基线建立不得是 warn（正常事件不得报成告警），实际切片: {baseline}"
    );
    let recovered = region(body, "HiddenCtxEvent::Recovered", None);
    assert!(
        recovered.contains("tracing::info!"),
        "回到带内必须是 tracing::info!，实际切片: {recovered}"
    );
    assert!(
        !recovered.contains("warn!"),
        "回到带内不得是 warn，实际切片: {recovered}"
    );

    // ④ 全函数范围：warn 恰好一处（= Drift）、error 零处 —— 在映射之外（match 前/
    //    后、分支内）另加告警级日志都会让计数变红。
    assert_eq!(
        body.matches("tracing::warn!").count(),
        1,
        "观测函数内 tracing::warn! 必须恰好一处（Drift 分支）"
    );
    assert_eq!(
        body.matches("tracing::error!").count(),
        0,
        "观测函数内不得出现 tracing::error!"
    );
}

/// 🔴 源码级守卫（反向）：隐藏上下文观测只能有**一份实现**，且状态只能由它写。
///
/// 为什么需要：上一条守卫只钉「共享函数名在场」（handlers 有全限定调用、stream 出现
/// ≥2 次），并列自写第二份实现（超集）也能通过 —— 样本会分叉：一侧喂共享状态机、
/// 另一侧喂自己的实现，漂移中位数失真且无任何信号。本守卫补反向判据：
/// 1. 定义唯一性：生产面只有 stream.rs 一个 `observe_hidden_context*` 函数 ——
///    前缀计数把 `_v2`/`_inner`/`_raw` 这类并列实现一起数进来；
/// 2. 非流式侧每一处调用形态出现都必须是全限定共享调用 —— 自写一份、本地别名、
///    `_v2` 变体调用都会让「裸调用数」≠「全限定调用数」；
/// 3. 非流式侧不得引用任何观测内部状态（registry/window/event 都是 stream 模块私有，
///    并列实现第一步就得提权引用 —— 计数非 0 即红）；
/// 4. 注册表在生产段恰好 2 处引用（1 次定义 + 1 次使用），且使用点必须落在观测函数
///    体内 —— 「除定义处外，状态只被该函数写」。
///
/// 改坏即红（已逐条用等价切片逻辑演练过）：handlers 加 `fn observe_hidden_context_*`
/// 并改调它 → 判据 1/2 红；保留共享调用再加自写同名实现与调用 → 判据 1/2 红；
/// handlers 引用 `HIDDEN_CTX_REGISTRY`（须先提权）→ 判据 3 红；stream.rs 别处再写一次
/// registry → 判据 4 红；把加锁搬出观测函数 → 判据 4 的函数体内计数红。
#[test]
fn hidden_context_observation_has_exactly_one_implementation() {
    let stream = production_normalized(include_str!("stream.rs"));
    let handlers = production_normalized(include_str!("handlers.rs"));

    // 判据 1：定义唯一性。
    let def_prefix = "fn observe_hidden_context";
    assert_eq!(
        stream.matches(def_prefix).count(),
        1,
        "stream.rs 生产段必须恰好一个 `{def_prefix}*` 函数定义（并列 `_v2`/`_inner` 实现会加一）"
    );
    assert_eq!(
        handlers.matches(def_prefix).count(),
        0,
        "handlers.rs 不得定义任何 `{def_prefix}*` 函数（第二份实现）"
    );

    // 判据 2：非流式侧每一处调用形态出现都必须是全限定共享调用。
    // needle 运行时拼接（本仓守卫惯例）：`(` 不进字面量。
    let call = format!("observe_hidden_context_sample{}", "(");
    let qualified_call = format!("stream::observe_hidden_context_sample{}", "(");
    let calls = handlers.matches(&call).count();
    assert!(calls > 0, "非流式路径必须调用共享观测函数（接线缺失即红）");
    assert_eq!(
        calls,
        handlers.matches(&qualified_call).count(),
        "handlers.rs 里每一处 `{call}` 都必须是全限定共享调用（自写实现/别名/`_v2` 变体会让计数不等）"
    );

    // 判据 3：非流式侧不得触碰观测内部状态（引用即需先提权，= 并列实现的第一步）。
    for needle in [
        "HIDDEN_CTX_REGISTRY",
        "HiddenCtxWindow",
        "HiddenCtxRegistry",
        "HiddenCtxEvent",
    ] {
        assert_eq!(
            handlers.matches(needle).count(),
            0,
            "handlers.rs 不得引用观测内部状态 `{needle}`（引用即并列实现/私自接线）"
        );
    }

    // 判据 4：注册表（唯一状态写入点）恰好 2 处引用：static 定义 + 观测函数内的使用。
    assert_eq!(
        stream.matches("HIDDEN_CTX_REGISTRY").count(),
        2,
        "生产段 HIDDEN_CTX_REGISTRY 必须恰好 2 处（static 定义 + 观测函数内的 .lock()）"
    );
    let body = production_fn_body(&stream, "fn observe_hidden_context_sample");
    assert_eq!(
        body.matches("HIDDEN_CTX_REGISTRY").count(),
        1,
        "注册表的使用点必须在观测函数体内（把加锁搬出该函数 = 出现第二个写入口）"
    );
}
