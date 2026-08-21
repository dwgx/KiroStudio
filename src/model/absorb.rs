//! 内置「上游 429 吸收层」的类别枚举。
//!
//! 下沉到 `model/`：`kiro` 策略层消费该类型，不能依赖 `anthropic` 协议模块。
//! 分类函数 [`crate::anthropic::absorb_class_of`] 仍留在 handlers —— 判据复用
//! 那边的既有谓词，且其中一条调用 `kiro::endpoint`（`model` 不能看见那些串）。

/// 内置「上游 429 吸收层」的可吸收类别。`None` = 不可吸收（详见 `anthropic::absorb_class_of`）。
///
/// # 与外挂 `kiro_shield.py` 的类别对照（合并的依据）
///
/// | 外挂那侧 | 本枚举 | 退避节奏 |
/// |---|---|---|
/// | `RETRYABLE` 里的 429 | [`Self::UpstreamRateLimit`] | 指数（min_delay 起） |
/// | `Retry-After` 头（它只看 HTTP 头） | [`Self::PoolCooldown`] | **号池进程内真值**，不需 HTTP 头往返 |
/// | `SWAP_WINDOW_MARKERS` | [`Self::SwapWindow`] | 长阶梯 20/40/60s（需显式开预算） |
/// | `RETRYABLE` 里的 5xx | [`Self::TransientServerError`] | 1s 起指数 |
/// | 瞬态 400 标记白名单 | [`Self::TransientCapacity400`] | 中等（2s 起） |
///
/// 外挂白名单里未被本枚举覆盖的：`ServiceUnavailable` / `InternalFailure` / `SlowDown` /
/// 裸 `ThrottlingException`。前三个本仓没有既有谓词、也没有实测样本证明它们真出现过
/// （`InternalServerException` 已被 `is_upstream_transient_5xx` 覆盖）；裸
/// `ThrottlingException` 是**刻意不认**——那个 `__type` 被真限流共用，详见
/// `endpoint::default_is_model_temporarily_unavailable` 处的说明。新写一套字符串匹配
/// 去覆盖它们，必然与渲染侧漂移，而那正是本仓反复出现的缺陷成因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AbsorbClass {
    /// 全池冷却 / 整池 RPM 饱和 / 池真耗尽：`retry_after_secs=N` 带号池算出的**真实**恢复秒数。
    PoolCooldown(u64),
    /// 上游账户级速率限流（`USER_REQUEST_RATE_EXCEEDED` 一类）。可重试。
    UpstreamRateLimit,
    /// 403 账户级**临时风控** = 外挂所称的「换号空窗」：账号被风控 → 网关 auto_disable →
    /// 切下一个凭据 → 推送补号，实测空窗约 **10 分钟**，期间该账号的请求全是 403。
    ///
    /// 默认**不吸收**（`upstream_retry_absorb_suspended`）；开启后若还想要长阶梯节奏，
    /// 需另设 `upstream_retry_absorb_swap_budget_secs`。
    ///
    /// ⚠️ 与 [`Self::PoolCooldown`] **必须严格分开**，且那条判据排在前面（见
    /// `absorb_class_of` 的顺序说明）：外挂 2026-08-04 踩过的坑正是把
    /// `"All credentials"` 挂进 `SWAP_WINDOW_MARKERS` → 全池冷却被套上长阶梯 →
    /// 本该等 10 秒（网关明确给了 `Retry-After: 10`）的等了几十秒。
    /// 号池冷却**必须听网关真值**，换号空窗才用长阶梯。
    SwapWindow,
    /// 上游 **5xx**（网关/上游抖动）。默认**不吸收**，见 `upstream_retry_absorb_server_error`。
    ///
    /// 刻意**不含传输层**失败：连不上上游时 provider 内部换号已把每个号各试一遍，
    /// 吸收层再套一层只是把同一个网络故障重打 N 遍。
    TransientServerError,
    /// 带**瞬态标记的 400**（模型容量不足）。默认**不吸收**，见 `upstream_retry_absorb_capacity_400`。
    ///
    /// 上游把这类瞬态故障塞进 400，与「请求写错了」同一个状态码，故判据必须窄到只认
    /// 既有谓词认的那两个 reason 字面量，其余 400 一律透传。
    TransientCapacity400,
}
