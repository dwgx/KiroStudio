use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum TlsBackend {
    Rustls,
    NativeTls,
}

impl Default for TlsBackend {
    fn default() -> Self {
        Self::Rustls
    }
}

/// 限流/重试的**档位预设**（2026-08-11 新增）。
///
/// # 它解决什么
/// 限流与重试相关的配置字段有 **34 个**，但 2026-08-11 逐个核对线上 `config.json`
/// 与代码路径后发现：真正决定行为的只有 7 个，其中 2 个还是代码常量而非配置；
/// 14 个是**死配置**（`absorb ×10` 不覆盖透传路径而线上 100% 流量走透传；
/// `rate_limit ×4` 有实测依据不能开）；另有 3 个是**语义陷阱** ——
/// 名字看起来是"关掉某功能"，实际会悄悄改变整条链的行为：
///
/// | 字段 | 线上值 | 真实语义 |
/// |---|---|---|
/// | `cooldown_enabled` | false | 429 后的号**不被跳过、立刻可重选** ⇒ 坏号不退避 |
/// | `inbound_queue_timeout_passthrough` | true | 排队超时**放行** ⇒ 整形层是"5 秒延迟器"不是限流器 |
/// | `inbound_rpm_auto` | false | 关着是对的（AIMD 单向棘轮会锁死在下限） |
///
/// 前两个方向相同（都放开），叠加后让外层重试外挂的放大能完整穿透进来。
///
/// ⚠️ **幅度用实测、别用配置值推算**：2026-08-11 曾按 `SWAP_MAX_ATTEMPTS=60` 推算
/// 「最坏 480×」，当天实测 shield 日志（84261 行）推翻 —— 19579 次判定全部落
/// `[passthrough]` 分支、零 `swap`，那个 60 从未被触及；真实每请求尝试**最大 7 次**
/// （成功均 4.27、放弃均 5.84）⇒ 总放大**约 5.6×**。
/// 复核命令见 `CLAUDE.md` 的「配置值上限 ≠ 实际放大」一节。
///
/// # 为什么默认是 `Manual`
/// **向前兼容的硬要求**：这 7 个字段都是「非 Option + serde default」，反序列化后
/// 分不清「用户显式写了 false」和「字段缺失走默认」。而线上 `config.json` 的 102 个键里
/// **这 7 个全部显式写了** —— 档位若无条件覆盖，会把现有生产配置全部冲掉。
/// 所以默认 `Manual`（完全不覆盖），老配置读进来行为**零变化**；
/// 只有用户主动选档才生效，且**只填空、不覆盖显式值**（见 `apply_to`）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum ThrottleProfile {
    /// 网关前面有**外部重试外挂**（线上真实链路：`Caddy → kiro_shield.py → KiroStudio`）。
    ///
    /// 目标是**不让外层的放大穿透进来**：
    /// - 整形层做真限流（排队超时返 429 而非放行）—— 对 shield 而言"放行"等于"重试成功"，
    ///   会让它立刻发下一个，整形反而成了放大器的润滑剂；返 429 才能让它走 cool 分支
    ///   听我们的 `Retry-After`。
    /// - 冷却开 —— 让 429 过的号真正退避，而不是被立刻重选（原地打转）。
    Shielded,
    /// 客户端**直连**网关，前面没有重试外挂。
    ///
    /// 目标是**单请求体验最好**：整形层超时放行（宁可慢也不要拒），
    /// 吸收层开（网关内部多承担，少让客户端看见错误）。
    Direct,
    /// 不做任何档位覆盖，全部读 `config.json` 原值 / 代码默认值。
    ///
    /// **这是默认值**，保证既有配置文件的行为零变化。
    Manual,
}

impl Default for ThrottleProfile {
    fn default() -> Self {
        Self::Manual
    }
}

/// KNA 应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default = "default_region")]
    pub region: String,

    /// Auth Region（用于 Token 刷新），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// API Region（用于 API 请求），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    #[serde(default = "default_kiro_version")]
    pub kiro_version: String,

    #[serde(default)]
    pub machine_id: Option<String>,

    /// credentials.json / trash.json at-rest 加密开关(默认关,兼容现有明文文件)。
    /// 开启后:落盘用机器绑定密钥加密(XChaCha20-Poly1305),读时透明解密。首次开启后下次 persist
    /// 才把明文重写为密文(透明迁移)。导出/导入接口走明文,不受影响。见 common::secret_store。
    #[serde(default)]
    pub encrypt_credentials_at_rest: bool,

    #[serde(default)]
    pub api_key: Option<String>,

    #[serde(default = "default_system_version")]
    pub system_version: String,

    #[serde(default = "default_node_version")]
    pub node_version: String,

    #[serde(default = "default_tls_backend")]
    pub tls_backend: TlsBackend,

    /// 外部 count_tokens API 地址（可选）
    #[serde(default)]
    pub count_tokens_api_url: Option<String>,

    /// count_tokens API 密钥（可选）
    #[serde(default)]
    pub count_tokens_api_key: Option<String>,

    /// count_tokens API 认证类型（可选，"x-api-key" 或 "bearer"，默认 "x-api-key"）
    #[serde(default = "default_count_tokens_auth_type")]
    pub count_tokens_auth_type: String,

    /// HTTP 代理地址（可选）
    /// 支持格式: http://host:port, https://host:port, socks5://host:port
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// 代理认证用户名（可选）
    #[serde(default)]
    pub proxy_username: Option<String>,

    /// 代理认证密码（可选）
    #[serde(default)]
    pub proxy_password: Option<String>,

    /// Admin API 密钥（可选，启用 Admin API 功能）
    #[serde(default)]
    pub admin_api_key: Option<String>,

    /// 负载均衡模式（"priority" 或 "balanced"）
    #[serde(default = "default_load_balancing_mode")]
    pub load_balancing_mode: String,

    /// 是否开启非流式响应的 thinking 块提取（默认 true）
    ///
    /// 启用后，非流式响应中的 `<thinking>...</thinking>` 标签会被解析为
    /// 独立的 `{"type": "thinking", ...}` 内容块,与流式响应行为一致。
    #[serde(default = "default_extract_thinking")]
    pub extract_thinking: bool,

    /// Claude Code 自动切缓冲协议：识别到 CC 请求时，`/v1` 流式自动改走 buffered 分发
    /// （等价 `/cc/v1`，input_tokens 用上游准确值）。**默认 true**。CC 会校验 input_tokens，
    /// 开启后 CC 直接打 `/v1` 也能正确工作，无需手动改用 `/cc/v1`。
    ///
    /// ⚠️ 代价：buffered 会把整轮回答憋到上游流结束才一次性吐，期间**只发 ping**——
    /// 模型越慢越像卡死（可能触发客户端 `Stream idle timeout`），且 CC 的 steering 失效。
    /// 想要内容边到边的真流式请设为 false（热更即时生效）。详见 `default_cc_auto_buffer`。
    #[serde(default = "default_cc_auto_buffer")]
    pub cc_auto_buffer: bool,

    /// Kiro **原生** extended thinking：是否走请求级
    /// `additionalModelRequestFields.output_config.effort` 触发上游原生
    /// `reasoningContentEvent`（**默认 false**）。
    ///
    /// 关闭（默认）时行为逐字节不变：Opus/Sonnet 的 extended thinking 仍走既有的
    /// `<thinking_mode>` XML 标签注入。
    /// 开启后：命中白名单模型（claude-opus-4.8/4.7/4.6、claude-sonnet-4.6，实测过的
    /// 才放；opus-5/sonnet-5 等未实测的一律回退 XML 注入）+ thinking 启用时，改用
    /// `output_config.effort` 原生通道并抑制 XML 标签 —— 参考仓实测：只有原生字段
    /// 能触发 `reasoningContentEvent`，XML 标签既不触发还污染历史上下文。
    ///
    /// 热更即时生效：converter 持有进程级镜像（`set_native_thinking_effort_enabled`），
    /// 改后下个请求即读到新值，不重启。
    #[serde(default)]
    pub native_thinking_effort_enabled: bool,

    /// 是否启用**批量推号入口** `POST /api/import/keys`（及等价的
    /// `/api/admin/import/keys`）。默认 **true**。
    ///
    /// 【为何默认开】该端点在本开关之前就已存在，外部 kiro-accounting 正在用它推号。
    /// 默认关等于升级即切断对接方，属无声的破坏性变更。开关的用途是**需要时临时封口**
    /// （如怀疑对接方在灌坏号），而不是重新决定这个功能要不要有。
    ///
    /// 关闭时两个挂载点一起失效（同一个 handler），返回 403。
    /// 鉴权仍在开关之前生效，所以关掉它不会让未鉴权请求看到不同的错误。
    ///
    /// ⚠️ 闸门在**解析与入池之前**，但**不在读请求体之前**：handler 签名是
    /// `Json(payload): Json<Value>`，axum 的提取器先于函数体运行，所以请求体已被
    /// 读完并反序列化。两个后果：① 关闭时发**非法 JSON** 会拿到提取器的 400 而不是
    /// 本开关的 403（合法 JSON 才走到 403）；② 大 body 仍会被读进内存（有
    /// `MAX_BODY_BYTES` 上限兜底）。闸门保证的是「不解析成导入项、不碰号池」，
    /// 不是「不读字节」。
    #[serde(default = "default_import_keys_enabled")]
    pub import_keys_enabled: bool,

    /// 克隆/多开产生的**分身凭据**在请求未显式指定时是否默认启用。**默认 false（不启用）**。
    ///
    /// 作用范围只有一条路：`POST /credentials/{id}/clone` 省略 `enabled` 时的取值。
    /// 请求显式给了 `enabled`（true 或 false）时**恒以请求为准**，本项不参与 ——
    /// 否则面板上那个开关会在服务端配置为 true 时变成"关不掉"。
    ///
    /// # 为什么默认 false
    ///
    /// 两件事同时指向 false，所以这里没有"保持现状"与"按需求"的取舍：
    ///
    /// - 它就是本项之前那句硬编码 `enabled.unwrap_or(false)` 的值 ⇒ 升级零行为变化。
    /// - 分身入池的瞬间就会被调度器选中，而此刻出口/region 都还没核对过。实测事故
    ///   （2026-08-05 02:42）：一次 `copies=5`，4 个分身 `apiRegion=None` → 回退
    ///   `config.region` → `ksk_` 按区授权 → 恒 403 → **24 秒内三次失败全部被自动禁用、
    ///   0% 成功**，而那 24 秒的真实用户流量正打在必废的号上。
    ///
    /// 本项存在的意义是给"节点池充足、每次都手工再点一遍启用嫌烦"的部署一个开关，
    /// 而不是重新决定这个默认值该是什么。改成 true 前请先确认节点池份数够用
    /// （不够时多出来的份直连，与父号共用出口 IP，多开的意义正好被抵消）。
    ///
    /// ⚠️ `#[serde(default = ...)]` 是硬约束而非风格：线上 `config.json` 是既有文件、
    /// 不含本键，缺 default 会让整个配置反序列化失败 → **服务起不来**。
    #[serde(default = "default_clone_default_enabled")]
    pub clone_default_enabled: bool,

    /// 网关内置「上游 429 吸收层」总开关。**默认 false**（关闭时逐字节等价旧行为）。
    ///
    /// 开启后：`call_api_with_retry` 在**准入闸门之下**、failover 循环之外，对可恢复的
    /// 429（全池冷却 / 上游账户级速率限流）就地退避重打整条 failover 链，而不是把 429
    /// 直接吐给客户端。等价于把 VPS 上外置的 `kiro_shield.py` 收进网关，使统计与开关进面板。
    ///
    /// ⚠️ 吸收循环**必须**留在入站准入闸门之下（2026-08-10 起闸门位于 handler 层
    /// `post_messages` / `post_messages_cc` 入口，透传与 Kiro 两条路径统一过闸）：
    /// 入站令牌是「每客户端请求一个」，若在闸门之上重试，一条请求会吃 N 个令牌，
    /// 把令牌桶按 N 倍速率抽干（这是设计评审的 BLOCKER 1）。详见 `docs/absorb-layer-design.md`。
    ///
    /// **默认关。** 2026-08-04 曾短暂改为默认开，当天回退 —— 支撑「默认开」的那个
    /// 数字是错的，记录在此以免重复：
    ///
    /// 当时的依据是「24h 74000 请求里可吸收三类合计 38%」。实际只有**上游 429 那
    /// 18.2%** 真的可吸收：
    /// - **池空 16.5% 吸收不了**：唯一的自动复活是全池自愈，而它的退避下限是
    ///   `selfHealBaseBackoffSecs` 默认 60s（上限 900s），大于吸收层的**总**预算 45s。
    ///   号池在预算内**结构上**不可能恢复，吸收只是让客户端多等满预算再拿同一个 429。
    /// - **整池 RPM 饱和 3.3% 只部分可吸收**：`cooldown.rs` 给的恢复秒数常大于
    ///   `max_delay`（默认 15s），被 clamp 后会**提前**醒来重打一个仍在冷却的池。
    ///
    /// 另有两条与「开」直接冲突、尚未修的问题：
    /// 1. `upstream_retry_absorb_budget_secs` 会经 `round_budget` 反向支配**既有的**
    ///    45s failover 墙钟（面板允许填 1）—— 开着时把它调小会截断正常换号重试，
    ///    这是关着时不存在的行为。
    /// 2. `ABSOLUTE_MAX_TOTAL_RETRIES=4` 的语义从「每请求」变成「每轮」：
    ///    `max_rounds=3` ⇒ 一条客户端请求最坏 4×4 = **16 次**上游调用，
    ///    而当初把 64 砍到 4 就是为了压住这个放大。
    ///
    /// 结论：这四条修完、且有**真正驱动吸收循环**的运行时测试之后再谈默认开。
    /// 手动开启仍随时可用（面板开关热更即时生效），本字段只决定新装实例的初值。
    #[serde(default)]
    pub upstream_retry_absorb_enabled: bool,

    /// 吸收层**总预算秒数**（默认 45）。绝对 deadline 自进入 `call_api_with_retry` 起算
    /// （含入站准入排队），与 provider 内部的 45s 墙钟闸门**串联**记账。详见
    /// `default_absorb_budget_secs`。
    #[serde(default = "default_absorb_budget_secs")]
    pub upstream_retry_absorb_budget_secs: u64,

    /// 吸收层**最大额外轮次**（默认 3，0=只打一次即不吸收）。与预算取先到者。
    /// 名为 rounds 而非 attempts：后者在 provider 里已指 failover 换号跳数，同名两义必混。
    #[serde(default = "default_absorb_max_rounds")]
    pub upstream_retry_absorb_max_rounds: u32,

    /// 退避下限毫秒（默认 150）。号池冷却常在几十~几百毫秒即恢复，
    /// 外置 shield 的 `MIN_DELAY=1.0` 会把 50ms 的恢复睡成 1s，这里放开到亚秒级。
    #[serde(default = "default_absorb_min_delay_ms")]
    pub upstream_retry_absorb_min_delay_ms: u64,

    /// 退避上限秒（默认 15）。号池给出的恢复秒数再大也 clamp 到此值，防单请求长挂。
    #[serde(default = "default_absorb_max_delay_secs")]
    pub upstream_retry_absorb_max_delay_secs: u64,

    /// 是否也吸收 **403 账户级临时风控**（即外挂 `kiro_shield.py` 的「换号空窗」类，默认 false）。
    ///
    /// 默认关的理由：风控窗口约 10 分钟 ≫ 任何合理的单请求预算，窗口内重试成功率接近 0，
    /// 吸收只是把必然失败推迟再返回；且 `selfHealBaseBackoffSecs` 默认 60s 存在的意义就是
    /// **停止**向刚 403 的账号试探，15s 内重打同账号直接抵消它。
    ///
    /// 开启后额外轮次**硬钉为 1** —— 除非同时设了 `upstream_retry_absorb_swap_budget_secs`
    /// （见那里：那个旋钮换掉的正是「15s 内重打」这个前提，钉 1 的理由随之消失）。
    #[serde(default)]
    pub upstream_retry_absorb_suspended: bool,

    /// 是否吸收**上游 5xx**（默认 false）。
    ///
    /// 【为什么单独一个开关而不是跟着总开关】5xx 与 429 的失败机理不同：429 是「上游让我们慢
    /// 一点」，重试大概率换个号就过；5xx 是「上游/网关自己坏了」，可能是瞬时抖动（重试有效），
    /// 也可能是上游整片故障（重试只是在故障期间乘倍放大请求量）。外挂 `kiro_shield.py` 的
    /// `RETRYABLE={429,500,502,503,504}` 把两者一视同仁，而它的实测代价是
    /// **11.6 次重试才救回 1 个请求**（22448 请求 / 19226 次重试 / 1657 次吸收成功）——
    /// 那个比值就是「不分机理一律重试」的账单。所以这里默认关，要开是显式决定。
    ///
    /// 【判据】复用 `handlers::is_upstream_transient_5xx` 且**排除传输层**
    /// （`is_transport_error`）：连不上上游时 provider 内部的换号已经把每个号各试过一遍，
    /// 吸收层再套一层只是把同一个网络故障重打 N 遍。
    #[serde(default)]
    pub upstream_retry_absorb_server_error: bool,

    /// 是否吸收**带瞬态标记的 400**（模型容量不足，默认 false）。
    ///
    /// 【为什么值得单列】上游把一部分**瞬态**故障塞进 400，跟「请求写错了」同一个状态码。
    /// 外挂实测 6 小时样本里 400 共 165 次，其中容量类 101 次、真格式错 80 次
    /// （两数之和大于 165，说明它自己的分类也有重叠，取其量级即可）。不区分就会把真正的
    /// 格式错误重试满预算 —— 那种重试永远不会成功。
    ///
    /// 【判据】只认 `endpoint::default_is_model_temporarily_unavailable`
    /// （`MODEL_TEMPORARILY_UNAVAILABLE` / `INSUFFICIENT_MODEL_CAPACITY`）这一个既有谓词，
    /// **不认**外挂白名单里的裸 `ThrottlingException`：那个 `__type` 被真限流
    /// （`USER_REQUEST_RATE_EXCEEDED`）共用，认它等于把真限流也拖进容量路径
    /// （详见 `endpoint/mod.rs` 该谓词处的说明）。
    ///
    /// 【与 provider 内部容量重试的关系】provider 已有 `MAX_MODEL_UNAVAILABLE_RETRIES` 次
    /// 慢速重试。本开关是它耗尽之后的**第二层**：容量类恢复常在分钟级，而那几次慢速重试
    /// 加起来只有秒级。
    #[serde(default)]
    pub upstream_retry_absorb_capacity_400: bool,

    /// 换号空窗的**独立预算秒数**（默认 0 = 不启用，沿用总预算与短退避曲线）。
    ///
    /// 【为什么必须与总预算分开】外挂实测换号空窗（账号被封 → auto_disable → 切下一个号 →
    /// 推送补号）约 **10 分钟**，而总预算 `upstream_retry_absorb_budget_secs` 线上是 20s
    /// （代码默认 45s）。同一个预算装不下两种量级：抬总预算会让**所有**类别都能占着客户端
    /// 连接十分钟，而换号空窗恰恰是唯一等得起的一类（客户端在补号完成后自动恢复，
    /// 而不是当场断会话）。
    ///
    /// 【> 0 时改变三件事】① 该类退避从「min_delay 指数」换成 20/40/60s 长阶梯
    /// （外挂的 `SWAP_BACKOFF`）；② 该类的 deadline 从 `call_started + budget` 换成
    /// `call_started + 本值`；③ `upstream_retry_absorb_suspended` 的「额外轮次钉 1」解除
    /// （钉 1 的理由是「15s 内重打同一个刚被罚的账号会抵消 60s 自愈退避」，长阶梯的最短一档
    /// 就是 20s，那个前提不再成立）。
    ///
    /// 【默认 0 的理由】非零即意味着单条客户端请求最长可占用连接数分钟。这对 Cursor 那类
    /// 会因错误码掐会话的客户端是净收益，对普通客户端是可见的长挂 —— 属于部署侧决定，
    /// 不该由升级默默带来。0 时该类的退避与 deadline **逐字节等于**本字段引入前的行为。
    ///
    /// ⚠️ 仍受 `upstream_retry_absorb_max_rounds` 与 `ABSOLUTE_MAX_TOTAL_RETRIES=4` 约束：
    /// 本旋钮给的是**时间**预算，不是无限轮次。要覆盖完整 10 分钟空窗需要
    /// `max_rounds` 也够大（20+40+60+60… 至少 4 轮），否则只是把一次重试推迟到 20s 后。
    #[serde(default)]
    pub upstream_retry_absorb_swap_budget_secs: u64,

    /// 吸收层**预算耗尽时**回给客户端的状态码（默认 **503**；唯一另一个可选值 429）。
    ///
    /// 【为什么这是产品级差异而不是实现细节】外挂原注释：Cursor 见 429 会**掐会话**，
    /// 而对 503 不会。即同一个「网关已经尽力重试但还是没成」的事实，用 429 表达会让客户端
    /// 直接放弃（用户实测：全部暂停），用 503 表达会让它自己再退避重试。两者的差别不在
    /// 网关侧，在客户端的行为。
    ///
    /// 【为什么默认 503】（2026-08-11 改为 503）：503 + Retry-After 触发客户端自动退避，
    /// 频率受 Retry-After 控制——「网关已尽力、上游仍不可用」是瞬态终态，退避重试是正确
    /// 行为。只有确实需要 429 语义（如按状态码计费/监控的对接方）才显式填 429。
    ///
    /// 【只影响「吸收层真的跑过并放弃」的那些请求】判据是 provider 在放弃时打的
    /// `absorb_budget_exhausted=1` 标记，不是「所有 429」—— 没进过吸收层的 429 照旧。
    /// 填 429 或任何其它值时 provider 不打该标记 ⇒ 渲染路径逐字节不变。
    #[serde(default = "default_absorb_exhausted_status")]
    pub upstream_retry_absorb_exhausted_status: u16,

    /// 默认端点名称（凭据未显式指定 endpoint 时使用，默认 "ide"）
    #[serde(default = "default_endpoint")]
    pub default_endpoint: String,

    /// 端点特定的配置
    ///
    /// 键为端点名（如 "ide" / "cli"），值为该端点自由定义的参数对象。
    /// 未在此表出现的端点沿用实现内置默认值。
    #[serde(default)]
    pub endpoints: HashMap<String, serde_json::Value>,

    /// CLI 端点（`ksk_` 号）请求体是否按**真实 Kiro CLI 客户端**的形状发送。**默认 false**
    /// （关闭时逐字节等价旧行为）。
    ///
    /// 【为什么存在】`ksk_` 号本身就是 CLI 凭据，而 KiroStudio 至今用的是 IDE 形状的 body
    /// （`origin: "AI_EDITOR"`）—— 即**拿 CLI 密钥、对上游自报是 IDE**。用户实测同一把 key
    /// 在 kiro-rs（发 `origin: "KIRO_CLI"`）无 429、在 KiroStudio 429，`origin` 是头号嫌疑：
    /// 它极可能参与上游的配额/限流分档。
    ///
    /// 开启后 `CliEndpoint::transform_api_body` 走 kiro-rs 那套三步（详见
    /// `kiro::endpoint::cli::set_origin_kiro_cli`）：`origin` 改 `KIRO_CLI`、删
    /// `conversationState.agentContinuationId`、删 history 里每条 `userInputMessage.modelId`。
    ///
    /// 【为什么默认关】线上 17 个可用号正在服务，全池直切一个未验证的上游协议形状不可接受。
    /// 用法是**单号/小流量开着比 429 率**，而不是升级即换。开关只影响 CLI 端点，IDE 号（social/
    /// idc/external_idp）完全不受影响。
    ///
    /// 【热重载】不需要原子镜像：`transform_api_body` 从 `ctx.config` 读，而该 `Config` 是
    /// provider 每次调用时 `token_manager.config()`（ArcSwap `load_full`）取的新快照 ⇒ 改配置
    /// 后下一个请求即生效。加镜像反而多一份要同步的真值（与吸收层同理，见
    /// `provider.rs` 里 `AbsorbPolicy` 那段说明）。
    #[serde(default)]
    pub cli_origin_kiro_cli: bool,

    /// CLI 端点是否发 `x-amzn-codewhisperer-optout: false`（即**允许**上游用会话做训练）。
    ///
    /// 【为什么要有这个开关】四个参考实现（ZyphrZero / GreyGunG / Foxfishc / M-JYuan）
    /// 全部发 `false`，其中 Foxfishc/M-JYuan 带抓包出处（`kiro-cli 2.3.0` +
    /// `Q_LOG_LEVEL=trace`，2026-05-12 实测），所以 `false` 才是**真实官方客户端**的值。
    /// 本仓历史上发 `true`（隐私优先：拒绝被用于训练）。
    ///
    /// 两者是**语义冲突**，不是谁对谁错：
    /// - `true`（本字段 = false，默认）＝ 隐私优先。代价：该头与真实客户端不一致，
    ///   理论上可被上游用作「这不是官方客户端」的指纹信号。
    /// - `false`（本字段 = true）＝ 指纹对齐真实 CLI。代价：**等于同意上游用你的会话
    ///   内容做训练**。
    ///
    /// 因此不由代码替用户决定，做成开关、**默认保持隐私优先**（不改变既有行为）。
    /// 想最大化伪装成真实 CLI 时再显式打开。
    ///
    /// 【热重载】与 `cli_origin_kiro_cli` 同范式：`decorate_api` 从 `ctx.config` 读，
    /// 而那份 Config 是 provider 每次调用时从 ArcSwap `load_full()` 取的新快照
    /// ⇒ 改配置后下一个请求即生效，不需要原子镜像。
    #[serde(default)]
    pub cli_codewhisperer_optout_false: bool,

    /// CLI 端点 User-Agent 指纹形状：是否对齐真实 `kiro-cli` 抓包值。
    ///
    /// 关（默认）＝ 保持本仓历史形状：`user-agent` 与 `x-amz-user-agent` **同一个串**，
    /// 内含 `api/codewhispererstreaming#1.28.3`（`#` 分隔）与 `m/E`。
    ///
    /// 开 ＝ 对齐四个参考实现一致的真实 CLI 形状，三处同时变：
    /// 1. 两个头拆成**不同**的串 —— `user-agent` 带 `md/appVersion-{}`、
    ///    `x-amz-user-agent` 带 `m/F` 且不带 appVersion（四家都这么拆，本仓喂同一串）；
    /// 2. `api/codewhispererstreaming/{ver}` 用 `/` 分隔（本仓用 `#`）；
    /// 3. `m/E` → `m/F`。
    ///
    /// 【为什么做成开关而不是直接改】UA 是最直接的客户端指纹，但**没有任何一家有对照
    /// 实验数据**证明它影响 429 率或封号率 —— 四家的依据都只是「真实客户端这么发」。
    /// 在没有真号做 A/B 的前提下直接全池切换，是拿生产流量赌一个未验证的假设；
    /// 而做成开关就能单号开、比 429 率。默认关 ＝ 不改变既有线上行为。
    ///
    /// ⚠️ 已知局限：`amz-sdk-request` 头本仓与参考仓都写死 `attempt=1`，而真实 SDK 会
    /// 递增 attempt（kiro2cc 是 `attempt={n+1}; max=3`）。所以即便开了本开关，
    /// **attempt 恒为 1 本身仍是一个指纹**。要彻底对齐需把重试轮次透传进 decorate，
    /// 涉及 trait 签名变更，未做（见 `cli.rs` 的 TODO）。
    #[serde(default)]
    pub cli_ua_align_real_client: bool,

    /// 是否启用失败冷却（429/认证失败等后短暂跳过该凭据，默认 true）
    ///
    /// 纯本地反应式调度：仅在凭据已出错时跳过它一段时间，无副作用，建议常开。
    #[serde(default = "default_cooldown_enabled")]
    pub cooldown_enabled: bool,

    /// 冷却时长缩放百分比（10..500，默认 100=原时长）。统一缩放所有冷却基础时长：
    /// <100 更短（激进，快速重试，适合号多）、>100 更长（保守，慎防封号，适合号少）。
    /// 只缩放短时/瞬时冷却基数，不动认证失败/封号那类长冷却硬窗（防误配把死号放行）。
    #[serde(default = "default_cooldown_scale_pct")]
    pub cooldown_scale_pct: u32,

    /// 是否启用拟人速率限制（每凭据每日上限 + 请求间隔，默认 false）
    ///
    /// 防关联用：模拟人类节奏。注意默认间隔 1s/请求会拖慢单用户高频工具调用，
    /// 故默认关闭；多账号轮换或在意关联风险时再开。配合 `rate_limit_*` 微调。
    #[serde(default)]
    pub rate_limit_enabled: bool,

    /// 速率限制：每凭据每日最大请求数（仅 rate_limit_enabled 时生效，默认 500）
    #[serde(default = "default_rate_limit_daily")]
    pub rate_limit_daily_max: u32,

    /// 速率限制：最小请求间隔毫秒（仅 rate_limit_enabled 时生效，默认 1000）
    #[serde(default = "default_rate_limit_min_interval_ms")]
    pub rate_limit_min_interval_ms: u64,

    /// 速率限制：请求间隔抖动百分比（0..50，默认 20）。真实间隔 = min_interval ±jitter% 随机，
    /// 让节奏更像人（固定间隔太机械易被识别为脚本）。仅 rate_limit_enabled 时生效。
    #[serde(default = "default_rate_limit_jitter_pct")]
    pub rate_limit_jitter_pct: u32,

    // ---- 入站请求整形 + RPM 自动挡(治上游 429 雪崩;冷却是号挂后补救,整形在入口削平突发) ----
    /// 限流/重试的**档位预设**。默认 `Manual`（不覆盖任何字段，老配置行为零变化）。
    ///
    /// 选 `Shielded`/`Direct` 后，档位只会填**配置文件里没显式写**的字段 ——
    /// 完整语义与向前兼容论证见 [`ThrottleProfile`]。
    #[serde(default)]
    pub throttle_profile: ThrottleProfile,
    /// 入站整形总开关（默认 true）。开=请求进上游前先过全局令牌桶,突发被排队削平成受控 RPM。
    #[serde(default = "default_true")]
    pub inbound_throttle_enabled: bool,
    /// RPM 自动挡（默认 true）。开=AIMD 动态调 target(无 429 加性增/收 429 乘性减);关=固定 target(手动挡)。
    #[serde(default = "default_true")]
    pub inbound_rpm_auto: bool,
    /// 目标 RPM 初值/手动挡固定值（默认 100）。
    #[serde(default = "default_inbound_target_rpm")]
    pub inbound_target_rpm: u32,
    /// 自动挡 RPM 下限（默认 20）。乘性减不低于此。
    #[serde(default = "default_inbound_rpm_min")]
    pub inbound_rpm_min: u32,
    /// 自动挡 RPM 上限（默认 300）。加性增不超过此。
    #[serde(default = "default_inbound_rpm_max")]
    pub inbound_rpm_max: u32,
    /// 令牌桶突发容量（秒，默认 2）。允许短时小突发不排队。
    #[serde(default = "default_inbound_burst_secs")]
    pub inbound_burst_secs: u32,
    /// 排队最长等待（秒，默认 30）。超时后行为由 inbound_queue_timeout_passthrough 决定。
    #[serde(default = "default_inbound_queue_max_wait_secs")]
    pub inbound_queue_max_wait_secs: u32,

    /// 排队超时后是否**放行**（默认 true）而非返回 429。
    ///
    /// 【单号/高 RPM 不流通根治】入站整形是为"削峰保护号不被打爆"设计的。但单号被上游 429 砍到
    /// 下限 RPM 后，请求量远超该 RPM → 大量请求在网关内排队 → 超过 queue_max_wait 就被拒 429，
    /// 表现为"RPM 跑太多就不流通"（请求没到上游就被网关自己卡死超时）。
    /// - true（默认）：排队超时不拒绝，直接**放行**去打上游（上游能否处理交给上游 + failover/冷却）。
    ///   入站整形仍在平峰削峰（有令牌正常限速），只在真堆积超时时降级为放行，最坏 == 不限速，绝不"不流通"。
    /// - false：保持旧行为，超时返回带 Retry-After 的 429 让客户端退避（多号池/需严格保护上游时用）。
    #[serde(default = "default_inbound_queue_timeout_passthrough")]
    pub inbound_queue_timeout_passthrough: bool,

    /// 全局同时在飞的上游 HTTP 调用数上限（默认 16，重启生效）。
    ///
    /// 防「上游重试放大」的硬闸：无论外部请求率多高、每个请求重试几次，网关内部同时
    /// 进行中的上游调用恒 ≤ 本值 —— 内部 RPM 被钳制在 `容量 / 单次上游延迟` 量级，
    /// 不再随「号多 + 429 多 → 疯狂换号重试」线性放大。Semaphore 容量不可热更，
    /// 改配置需重启（并发闸是「上限」而非「目标值」，无需 autotune）。
    #[serde(default = "default_upstream_concurrency_limit")]
    pub upstream_concurrency_limit: usize,

    /// **单个凭据**同时在飞的上游调用数上限（默认 8，重启生效；0 = 不单独限制，
    /// 退化成全局闸容量）。
    ///
    /// 🔴 为什么全局闸不够：全局闸只保证「总在飞 ≤ N」，**不保证分布**。号池里一旦有号
    /// 响应慢（上游对它排队而不是立刻 429），慢号的请求会长时间占着全局许可 —— 极端情况
    /// 下 N 个许可全被同一个慢号吃掉，其余健康号一个都拿不到，**整池吞吐被一个号拖死**，
    /// 而症状显示为系统级的「并发闸已满」，排障时根本指不到是哪个号。
    ///
    /// 加这一级后，单号最多占 N_percred 个许可，剩余容量必然留给别的号；且拿不到许可时
    /// 走的是**换号**（`continue`）而不是放弃请求，池里其它空闲号会立刻接手。
    ///
    /// 与选号层 `inflight` 排序是**互补**关系，不是重复：inflight 影响「优先选谁」（软偏好），
    /// 本闸是「选中了也不许超」（硬上限）。选号可能因会话亲和、RPM 饱和门回退等原因仍旧
    /// 选中同一个号 —— 那时只有硬闸挡得住。
    ///
    /// 默认 8 = 全局默认 16 的一半，保证至少两个号能同时打满。对照 kiro2cc 的两级闸：
    /// 全局 50 + 每凭据 20（同为 40% 比例量级）。
    ///
    /// ⚠️ 与 RPM 无关：RPM 是**速率**（次/分钟，滚动窗口），本闸是**并发度**（同时在飞数）。
    /// 一个号可以低并发高 RPM（快速串行），也可以高并发低 RPM（慢响应）。本仓的 RPM 逻辑
    /// （`credential_rpm_limit` / headroom / 饱和门）完全不受本字段影响。
    #[serde(default = "default_upstream_per_credential_limit")]
    pub upstream_per_credential_limit: usize,

    /// deepseek 归一化的**全局默认**配置（custom_api 代挂 `deepseekNormalize=true` 时生效）。
    /// per-凭据 `deepseek_normalize_config` 可覆盖 fallback_model/min_max_tokens；
    /// bool 开关一律取这里（全局唯一）。TIER1 热重载，改 config.json 立即生效。
    #[serde(default)]
    pub deepseek_normalize: crate::kiro::deepseek_normalize::DeepseekNormalizeConfig,

    /// **全局模型映射**：`{"客户端请求的模型名": "实际发给上游的模型名"}`（默认空 = 不映射）。
    ///
    /// 语义（见 [`crate::kiro::model_mapping`] 的完整设计说明）：
    /// - 在**选号之后、发上游之前**改写模型名。选号门（`allowed_models` 白名单）只看
    ///   **原始**模型名，映射后**不再**判白名单。
    /// - 凭据可设 `model_mapping_exempt=true` **完全跳过**本表（安全阀，覆盖"该号上游
    ///   不认映射后名"的场景）。
    /// - key 大小写不敏感；**单跳**映射（不做链式，`A→B` 且 `B→C` 只改写 A→B）。
    /// - 用量统计双口径：`requested_model`（客户端原始名）与 `upstream_model`（映射后名）
    ///   分别聚合，面板可按两个维度看。
    ///
    /// TIER1 热重载，改 config.json 立即生效。一个请求的 failover 循环内只快照**一次**
    /// 规则表（见 `provider.rs` 的 `mapping_rules`），避免同一请求跨跳用不同规则。
    #[serde(default)]
    pub model_mapping: std::collections::HashMap<String, String>,

    /// 是否启用会话亲和性（同一会话尽量复用同一凭据，默认 true）
    ///
    /// 防关联用：让同一对话粘在同一账号上，避免单次会话散落到多个账号引发关联。
    /// key 取自请求 metadata.user_id 提取的 session UUID（无 session 时随机，不命中即正常轮换）。
    /// 主要在 balanced 模式下生效；priority 模式本就固定单凭据，影响甚微。
    #[serde(default = "default_affinity_enabled")]
    pub affinity_enabled: bool,

    /// **全局每号 RPM（每分钟请求数）软上限**（默认 0）。
    ///
    /// 这是号池的「全局默认 RPM」：单号未单独设置自己的 `rpm_limit`（=0/None）时**继承此值**。
    /// 解析顺序 = 单号 rpm_limit(>0) → 本全局值(>0) → 内置兜底 30（见 effective_saturation_limit）。
    /// 所以导入的新号 RPM=0 时用的就是这里；此值也为 0 时才落到内置 30。
    /// 调度用：balanced 选号时，滚动 60 秒窗口内请求数达到该上限的凭据会被**降权**
    /// （排到未饱和凭据之后），而非硬跳过。与 `rate_limit_*`（拟人节流，硬跳过）互补。
    #[serde(default)]
    pub credential_rpm_limit: u32,

    /// RPM headroom 系数(0..100 整百分比;默认 85=预留 15% 缓冲)。饱和阈值 = base_limit × factor/100。
    /// 让饱和判定在上游真硬限之下提前触发,削弱 60s 滑窗边界爆发 + 贴顶跑。0/100 = 不打折(=旧行为)。
    #[serde(default = "default_rpm_headroom_factor")]
    pub rpm_headroom_factor: u32,

    /// RPM 预留名额:在 headroom 折扣后再额外扣掉 N 个名额(默认 0)。给突发留固定缓冲,与 factor 叠加。
    #[serde(default)]
    pub rpm_reserve_slots: u32,

    /// 整池 RPM 饱和时是否走背压等待(默认 false=回退软门,选"最不坏"的号继续)。
    /// true 时选号返回 None → acquire_context 等待最短 RPM 恢复窗口(受 MAX_TRANSIENT_WAIT 上限)。
    /// 保守默认关:硬门只在非整池饱和时生效,整池饱和回退旧软门行为不阻塞。
    #[serde(default)]
    pub rpm_hard_gate_overload_wait: bool,

    /// 余额加权分流(默认 **true**):同优先级、同健康档、同在途时,按剩余额度比例微调选号评分——
    /// 余额多的号略多分、少的略少分,长期把号池剩余额度拉平,不让某个号先耗干。
    /// 软偏置非硬配额:只在 p_avail(末位兜底键)上乘一个 [floor,1] 因子,绝不掀翻 0.7.23 在途均分。
    /// 关闭 = 退回纯 0.7.23 行为(不看余额)。热更即时生效。
    #[serde(default = "default_balance_weight_enabled")]
    pub balance_weight_enabled: bool,

    /// 余额加权下限 FLOOR(0..100 整百分比,默认 50)。因子 = floor/100 + (1-floor/100) × 剩余额度比例。
    /// floor=50:满额号因子 1.0、半额号 0.75、耗尽号 0.5——差 10~20% 属微调不喧宾夺主。
    /// floor=100 = 因子恒 1.0(等于关闭加权)。越小余额影响越强。热更即时生效。
    #[serde(default = "default_balance_weight_floor")]
    pub balance_weight_floor: u32,

    /// 429/限速感知降权(默认 **true**):某号冒 429 时经 EWMA 拉低健康分→少被选(现有 health 机制)。
    /// 关闭 = p_avail 的 health 项跳过 429 惩罚(某些场景不想让偶发 429 影响分流)。热更即时生效。
    #[serde(default = "default_health_429_weight_enabled")]
    pub health_429_weight_enabled: bool,

    /// 全池冷却时是否"快速失败"：当所有可用凭据都在冷却/风控中，立即返回 429+Retry-After
    /// 让客户端(Claude Code)自己退避重试，而不是在网关内硬扛等待。默认 true。
    /// 客户端退避比网关反复选号温和，也减少对被风控号的零星试探（吸收其它 kiro.rs fork 做法）。
    #[serde(default = "default_all_cooling_fast_fail")]
    pub all_cooling_fast_fail: bool,

    /// 是否在凭据持续可疑活动风控(连续触发达阈值)时自动禁用它（移出调度，避免继续砸加重风控/触发封禁）。
    /// 默认 true。禁用后可人工或自愈重新启用。
    #[serde(default = "default_auto_disable_suspicious")]
    pub auto_disable_suspicious: bool,

    /// 均衡负载模式下是否叠加**优先级分发**（默认 false）。
    ///
    /// 关闭（默认）：balanced 纯按健康/负载分摊，priority 仅作末位兜底。
    /// 开启：balanced 先按 priority 分层（越小越优先），**层内**仍按健康/负载均衡，
    /// 且整层饱和/熔断才优雅溢出到下一优先级层——既尊重优先级又不死磕单个被打爆的高优先级号。
    /// 仅在 balanced 模式生效；priority 模式本就按优先级，不受影响。TIER1 热重载即时生效。
    #[serde(default)]
    pub priority_in_balanced: bool,

    /// custom_api 代挂号是否**无条件抢在所有 Kiro 号之前**（默认 **false**）。
    ///
    /// `false`（默认，推荐）：代挂号与 Kiro 号在**同一个 `priority` 维度**上公平比较 ——
    ///   谁的 priority 数字小谁先用，用完/失败再落另一池。这符合"priority 越小越优先"的直觉。
    /// `true`（历史行为）：只要有任一可用的 custom_api 号就先透传，Kiro 号完全不参与竞争，
    ///   用户设的 priority 在跨池维度上**完全无效**。
    ///
    /// 背景：历史实现把「custom_api 优先」写死在分派顺序里（handlers 一进来就先试透传，
    /// 见 `try_custom_api_passthrough` 的调用点），而 `select_custom_api` 只在代挂号**子集内**
    /// 比较 priority，于是跨池优先级从来没有被比较过 —— 表现为"我把 Kiro 号 priority 调到 0
    /// 了，它还是先走中转"。
    ///
    /// 单个凭据可用 `credentials.json` 的 `customApiFirst` 字段各自覆盖本全局值。
    /// TIER1 热重载即时生效。
    #[serde(default)]
    pub custom_api_first: bool,

    /// 是否启用 prompt 缓存记账（默认 false）
    ///
    /// 是否把**估算的** cache_read / cache_creation 记账字段下发给下游客户端。
    ///
    /// Kiro 上游不回传这两个字段——`docs/CACHE-EXP0-RESULT.md` 的 EXP-0 已实测确证：
    /// `metadataEvent` 的 payload 只有 `{"stopReason":...}`，全窗口 grep
    /// `tokenUsage|cacheReadInputTokens|cacheWriteInputTokens` 零命中。
    /// 所以网关下发的 `cache_read_input_tokens` 是**本地前缀 token 估算**
    /// （`token::count_prefix_tokens`，见 `handlers.rs`），不是上游真值、也不是真实计费依据。
    ///
    /// 开启（默认）：注入该字段，Claude Code 等客户端会显示"缓存命中 N tokens"。
    /// 关闭：**不注入该字段**（而非注入 0）。这个区别是有意的——对 Anthropic 客户端来说
    /// `cache_read_input_tokens: 0` 表示"确实一次都没命中"，字段缺失表示"本网关不做该记账"，
    /// 语义完全不同；注入 0 会让客户端把"未记账"误报成"缓存全未命中"。
    ///
    /// ⚠️ 该开关**只管下发**，不影响用量统计侧的 cache 字段：面板/SQLite 仍照常记录估算值，
    /// 否则关掉开关会让统计恒 0，把一个"要不要给客户端看"的选择变成"要不要留数据"。
    #[serde(default = "default_prompt_cache_enabled")]
    pub prompt_cache_enabled: bool,

    /// prompt 缓存估算的有效期上限（秒）。
    ///
    /// ⚠️ 当前**无实际读取点**：现行估算是无状态的（每请求按 `count_prefix_tokens`
    /// 重算前缀 token），没有需要按时间过期的缓存表，所以这个值改成什么都不影响行为。
    /// 保留该字段是为兼容既有 config.json 不因未知键报错。
    /// 若将来实现 `docs/CACHE-RFC.md` 的 L2 度量层（带过期的影子缓存表），
    /// 应对齐上游 5 分钟窗口（即 300），而不是沿用这里的 3600。
    #[serde(default = "default_prompt_cache_ttl_seconds")]
    pub prompt_cache_ttl_seconds: u64,

    /// 是否剥离转发给上游的 system 环境噪音（默认 true，立即生效 / 无需重启）
    ///
    /// Claude Code 每次请求都会在 system 携带每请求漂移的环境上下文
    /// （`<env>` 工作目录/平台/日期块、`gitStatus:`、`Recent commits:`、
    /// `# Environment` / `# auto memory` 段、模型名行等）。这些漂移行位于 prompt 前缀，
    /// 只要变一个字节，上游 Bedrock prefix cache 其后全部失效（命中率骤降），且它们是
    /// 关联「这是 Claude Code」的强指纹。开启后在归一化路径保守剥离这些整块 / 整行：
    /// 提升上游缓存命中率、省 token、降 CC 身份被关联风险。
    ///
    /// 剥离对**转发字节**与**影子缓存指纹**两条路径经同一归一化入口施加，保证记账与真实
    /// 缓存一致。保守：只剥确定漂移的环境块，绝不触碰稳定的 system 正文（工具/身份/任务指令）。
    #[serde(default = "default_strip_env_noise")]
    pub strip_env_noise: bool,

    /// 工具错误缓解 ①：清洗模型泄漏的控制 token（course/課/count/care 之类）。默认关，热更生效。
    ///
    /// 模型偶发把内部控制/规划 token 泄漏进输出文本、甚至混进 tool_use.input 导致 JSON 非法
    /// （客户端报 Invalid tool parameters）。开启后对文本流做**保守高信号**清洗：只剥离句首/块首、
    /// 且英文控制词直贴 CJK 无空格分隔的粘连（如 `course重读`），正常文本不会这样粘连，误删风险低。
    /// 这是**缓解非根治**（病根在模型侧生成参数，网关无法根治）。对所有模型可用（含 Claude 路径）。
    #[serde(default = "default_tool_clean_leaked_tokens")]
    pub tool_clean_leaked_tokens: bool,

    /// 工具错误缓解:文本化 invoke 重组(默认 **true**,热更)。模型把工具调用吐成 <invoke> 文本时,
    /// 在四道安全门内(行首+非代码围栏+工具名已声明+完整闭合)重组为结构化 tool_use;修不了的碎片/
    /// 截断当文本安全放过。移植 ZyphrZero__kiro.rs v0.6.5 生产方案。关=退回纯转发。
    #[serde(default = "default_tool_reclaim_textified_invoke")]
    pub tool_reclaim_textified_invoke: bool,

    /// 工具错误缓解:stray token 复读熔断(默认 **true**,热更)。call/count/card/court 连续独占行复读
    /// 超阈值(32)截断本轮文本,治 Opus 退化刷屏耗尽 max_tokens + 污染历史。
    #[serde(default = "default_tool_stray_repeat_guard")]
    pub tool_stray_repeat_guard: bool,

    /// 工具错误缓解 ②：流式路径工具拼装非法 JSON 时，对齐成失败态。默认开，热更生效。
    ///
    /// 修既有不对称：非流式工具拼装非法 → 502 失败态；流式却只告警+透传原文、网关记 Success。
    /// 开启后流式也置 UpstreamError{INVALID_TOOL_INPUT} 失败态（用量记 ServerError，不污染成功率），
    /// 与非流式对齐。**绝不静默吞成空参、绝不 report_failure 连坐号**（工具非法≠号坏）。
    #[serde(default = "default_tool_stream_align_failure")]
    pub tool_stream_align_failure: bool,

    /// 工具错误缓解 ③：工具拼装非法时，向客户端补发明确的 SSE error 事件。默认开，热更生效。
    ///
    /// 开启后拼装非法时收尾补发 in-band error（而非静默透传坏 JSON），让客户端收到明确失败信号、
    /// 自行退避重试，而不是把坏 JSON 当参数解析报 Invalid tool parameters。需配合 ② 使用效果最佳。
    #[serde(default = "default_tool_expose_error_to_client")]
    pub tool_expose_error_to_client: bool,

    /// 工具错误缓解 ④（**根治向**）：工具参数拼装后非法 JSON 时，尝试修成合法 JSON 再发客户端。默认**开**，热更生效。
    ///
    /// 依据 Claude Code 客户端源码坐实：客户端拿 `partial_json` 直接 `JSON.parse`、**不做任何修复**，
    /// 失败即报 "Invalid tool parameters"；官方对相关 issue（#69522/#20015/#29715）Open/not-planned
    /// **不修**。本网关在发给客户端前把坏 JSON 修好（转义非法反斜杠/裸控制符、补全截断），客户端即可
    /// parse 成功。安全：**只在 `from_str` 已失败时介入、修复后强制复验 `from_str` 通过才用**，修不好
    /// 退回原样透传——对正常合法 JSON 零影响，最坏情况等于不开。故默认开（纯增益，不改变正常流行为）。
    #[serde(default = "default_tool_repair_json")]
    pub tool_repair_json: bool,

    /// 工具错误缓解 ⑤：截断跨轮恢复。默认**关**（改变对话流程），热更生效。
    ///
    /// 只在**修复层④也补不回**（真截断，缺整段值）且归因为截断时触发：不发不完整的 tool_use 参数
    /// （半截参数会被客户端当完整调用执行），改置失败态 + 收尾补发 SSE error，让客户端退避后重试整个
    /// 请求（下轮模型可能生成更小的调用）。绝不连坐号（工具截断≠号坏）。默认关：它把截断从"发半截"
    /// 变成"整轮失败重试"，改变对话行为，需用户显式开启。
    #[serde(default = "default_tool_truncation_recovery")]
    pub tool_truncation_recovery: bool,

    /// 入站工具**顶层** description 的字符上限（默认 10000）。超出按字符边界安全截断（防多字节切断）。
    ///
    /// Claude Code 会给每个工具挂很长的说明，累积后既占 token 又逼近上游对单工具描述的隐性上限。
    /// 硬截断早已存在（等价 kiro2api `MAX_TOOL_DESCRIPTION_LENGTH`），此字段只把上限提为可配置；
    /// schema 内嵌 description 上限按同一比例（1/5）联动，无需单独字段。设 0 表示不截断。
    #[serde(default = "default_tool_description_max_chars")]
    pub tool_description_max_chars: usize,

    /// 网页上号回调基地址（可选）
    ///
    /// - 不配置：本地回调模式，后端在本机临时端口接收 OAuth 回调（仅本机浏览器可达）。
    /// - 配置为公网地址（如 `https://kiro.example.com`）：远程回调模式，
    ///   浏览器回调打到 `{callbackBaseUrl}/api/admin/auth/callback`，适合 Docker/服务器部署。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_base_url: Option<String>,

    /// 是否启用用量统计（请求埋点 + SQLite/JSONL 落盘 + 内存预聚合，默认 true）
    ///
    /// 关闭后热路径的埋点管道不初始化，`emit_record` 静默丢弃，零开销。
    #[serde(default = "default_usage_enabled")]
    pub usage_enabled: bool,

    /// 用量数据目录（SQLite 与 JSONL 落盘位置，默认 "data/usage"）
    #[serde(default = "default_usage_data_dir")]
    pub usage_data_dir: String,

    /// 用量明细（SQLite traces）保留天数，超期后台清理（默认 30）
    #[serde(default = "default_usage_retention_days")]
    pub usage_retention_days: i64,

    /// 是否采集下游客户端指纹（设备类型 / IP / OS / 浏览器，默认 true）
    ///
    /// 隐私开关：关闭后热路径不再从入站请求头/连接对端解析这些字段，
    /// 用量记录里的 client_device/client_ip/client_os/client_browser 全部留空，
    /// 落盘与前端展示都拿不到指纹信息（session_id 维度的 RPM 聚合不受影响）。
    /// 立即生效（运行时镜像），无需重启。
    #[serde(default = "default_collect_client_fingerprint")]
    pub collect_client_fingerprint: bool,

    // ============ 反代安全（批次3）============
    /// CORS 允许来源列表。空 = 允许任意来源（`Access-Control-Allow-Origin: *`，
    /// 保持向后兼容公开 API 场景）。非空时仅回显命中列表的 Origin，凭据请求也受控。
    /// 例：`["https://app.example.com", "http://localhost:5173"]`
    #[serde(default)]
    pub cors_allowed_origins: Vec<String>,

    /// 入口 IP 白名单（CIDR 或单 IP）。空 = 不限制。命中才放行，否则 403。
    /// 支持 IPv4/IPv6 CIDR，例：`["127.0.0.1/32", "10.0.0.0/8", "::1/128"]`。
    /// 客户端 IP 取 TCP 连接对端；若在反代后需按 `trust_forwarded_header` 取 XFF。
    #[serde(default)]
    pub ip_allowlist: Vec<String>,

    /// 入口 IP 黑名单（CIDR 或单 IP）。空 = 不启用。命中即拒（403），**优先于白名单判定**。
    /// 用于封禁特定滥用 IP。支持 IPv4/IPv6 CIDR，例：`["1.2.3.4/32", "5.6.0.0/16"]`。
    /// 客户端 IP 判定同白名单（TCP 对端 / 反代后按 trust_forwarded_header 取 XFF 最右段）。
    #[serde(default)]
    pub ip_blocklist: Vec<String>,

    /// 机器码黑名单（封禁）。空 = 不启用。命中即拒（403，返回消息 `sbsbsb！`）。
    /// 机器码 = `MC-` + SHA256(machine_key) 前 12 位，从运维台「按机器」视图复制。
    /// 判定时按当前请求的真实客户端 IP（同 IP 黑名单口径）重算机器码，精确匹配（大小写不敏感）。
    /// 与 IP 黑名单互补：机器码不暴露裸 IP，且与「按机器」分组一一对应，复制即可拉黑。
    #[serde(default)]
    pub machine_code_blocklist: Vec<String>,

    /// 是否**强制**信任 `X-Forwarded-For` / `X-Real-IP` 头来判定客户端 IP（默认 false）。
    /// 说明（A2 修复后）：即便为 false，当 TCP 对端是私网/环回地址（=本机可信反代）时也会
    /// 自动采信 XFF **最右**段（不可伪造）；置 true 则**无论对端**都信任转发头。
    /// **仅当本服务确实部署在可信反代（nginx/traefik）之后才可置 true**，否则公网直连客户端
    /// 可伪造该头绕过 IP 白名单与限流。取最右段防伪造，见 `security::client_ip`。
    #[serde(default)]
    pub trust_forwarded_header: bool,

    /// 入口每-IP 限流：每分钟最大请求数。0 = 不限流（默认 0）。
    /// 固定窗口计数，超限返回 429。与凭据级 `rate_limit_*` 相互独立。
    #[serde(default)]
    pub ingress_rate_limit_per_min: u32,

    /// 请求体最大字节数（默认 50MiB）。防止超大 body 打爆内存。
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,

    // ============ 主动 token 预刷新（批次4.4）============
    /// 是否启用后台主动预刷新：在 token 过期前后台刷新，削掉首个请求的刷新延迟与突发。
    /// 默认 true。关闭后退回原有「请求时按需刷新」行为。
    #[serde(default = "default_true")]
    pub proactive_token_refresh: bool,

    /// 预刷新提前量（分钟）：token 剩余有效期低于此值即后台刷新（默认 10）。
    #[serde(default = "default_refresh_lead_minutes")]
    pub token_refresh_lead_minutes: i64,

    /// 后台预刷新扫描间隔（秒，默认 60）。
    #[serde(default = "default_refresh_interval_secs")]
    pub token_refresh_interval_secs: u64,

    // ============ Admin UI 登录页 ============
    /// 登录页是否显示随机背景图（默认 true）。关闭后登录页用纯渐变背景，
    /// 不再请求外部图源。此项立即生效（登录页每次加载时读取）。
    #[serde(default = "default_true")]
    pub login_background_enabled: bool,

    /// 登录页背景图是否请求 R18 图源（**默认 false / 全年龄**）。开启走 r18=1，关闭走 r18=0。
    /// 此项立即生效（下一轮后台预取 / 池空实时兜底拉取时按此取 r18 参数）。
    /// 默认关闭：截图/演示/给别人看面板更安全，需要再手动开。
    #[serde(default)]
    pub login_background_r18: bool,

    // ============ 余额同步（A6：温和的周期性余额刷新）============
    /// 后台温和刷新余额缓存的间隔（秒）。`0` = 禁用（默认 1800 = 30 分钟）。
    ///
    /// 为避免触发上游风控：绝不在启动/挂载时批量拉；后台任务用长间隔、逐个刷新且每个之间
    /// 留有间隔（分散节奏），只刷未禁用的号，仅更新缓存供展示，绝不做主动禁用。
    /// 安全第一：可保守设为 0 禁用，由用户在设置里自行开启。
    ///
    /// 热重载批次(HR)会把它做成可热调，本批先作为需重启字段。
    #[serde(default = "default_balance_refresh_interval_secs")]
    pub balance_refresh_interval_secs: u64,

    // ============ 凭据回收站 ============
    /// 回收站保留天数：软删除的凭据超过此天数后由后台任务彻底清理（默认 30）。
    /// `0` 表示永久保留，不自动清理。
    #[serde(default = "default_trash_retention_days")]
    pub trash_retention_days: u32,

    // ============ 全池自愈退避（防自愈加深上游封禁，P0）============
    /// 全池自愈**基础退避**（秒）。第 n 次连续自愈需等 `base × 2^(n-1)`，
    /// 上限 `self_heal_max_backoff_secs`。任一号成功即清零 streak（见 token_manager
    /// 的 report_success），真恢复了立刻回到灵敏状态。
    ///
    /// 默认 60 的依据（历史教训）：自愈此前**没有任何退避**——选不出号就立刻复活全池，
    /// 实测 41 分钟触发 36 次（约每 68 秒一次）；403 `temporarily is suspended` 是上游刚下的
    /// 惩罚，每次复活都立刻再打一轮 = 持续撞同一面墙、加深封禁（用户直接反馈过
    /// 「已经 403 封号了，不知道为什么一直被自动开启」）。线上 403 突发窗口约 10 分钟，
    /// 60s 起、翻倍、上限 900s 让探测频率与真实窗口同量级，一个窗口内最多探两三次。
    ///
    /// 热重载即时生效：reload_config 换入 ArcSwap 后，下一个自愈周期即按新值退避，无需重启。
    #[serde(default = "default_self_heal_base_backoff_secs")]
    pub self_heal_base_backoff_secs: u64,

    /// 全池自愈退避**上限**（秒，默认 900 = 15 分钟）。见 `self_heal_base_backoff_secs`。
    #[serde(default = "default_self_heal_max_backoff_secs")]
    pub self_heal_max_backoff_secs: u64,

    /// 指数退避的**指数上限**（默认 4）。防 `2^n` 溢出：60 × 2^4 = 960 已超上限 900，故 4 足够。
    /// 注意此值只 clamp 指数增长上限，与 `self_heal_max_backoff_secs` 一起决定退避天花板；
    /// 消费点另有 31 的硬 clamp 兜底（u32 位移溢出 panic 防护）。
    #[serde(default = "default_self_heal_max_shift")]
    pub self_heal_max_shift: u32,

    // ============ 输入压缩管道（吸收自 Foxfishc__kiro.rs，MIT，致谢）============
    /// 转换后发上游前的输入压缩配置。
    ///
    /// 背景：Kiro 上游对请求体大小有硬限制（实测约 5MiB 会触发 400）。开启后，
    /// 网关在序列化 Kiro 请求体后测量大小，仅当超过 `trigger_bytes` 才跑压缩管道
    /// （空白折叠 + 大 tool_result 智能截断），压缩后再发上游，压缩后仍超限才透传 400。
    /// 保守设计：默认阈值高（只在快超限时才压），且可整体关闭。
    #[serde(default)]
    pub compression: CompressionConfig,

    /// MODEL_TEMPORARILY_UNAVAILABLE 重试耗尽时的自动回退模型（可选）。
    ///
    /// 设置后，当上游返回 MODEL_TEMPORARILY_UNAVAILABLE 且慢速重试失败时，
    /// 自动以此模型重试最后一次。留空（默认）则直接透传过载错误给客户端。
    /// 推荐备用："claude-sonnet-4-5-20251001"（容量池独立，响应更快）。
    /// 不建议填同族 opus 变体——容量压力通常跨变体共享，切过去大概率同样过载。
    #[serde(default)]
    pub overload_fallback_model: Option<String>,

    // ============ 上游 trace 埋点（P0-A，排障用）============
    /// 是否启用上游 trace 埋点（**默认 false**）。
    ///
    /// 开启后把「上游原始响应」与「网关内部判断」写进同一条 JSONL 记录，
    /// 用于回答日志答不了的四个问题（上游给没给 Retry-After / 两个 region 的响应差异 /
    /// 429 body 里有没有配额字段 / reasoningContentEvent 的原始形状）。
    ///
    /// **默认关是刻意的**：它每次失败响应写一行（含最多 2KiB body），是诊断期临时开关，
    /// 不是常态度量。关闭时热路径只付一次 `Relaxed` 原子读的代价
    /// （见 `kiro::upstream_trace::is_enabled`）。
    ///
    /// 脱敏由 `upstream_trace::sanitize_body` 保证：token / kiroApiKey / refreshToken /
    /// Authorization **不进 trace**，请求体（含用户 prompt）整体不落盘。
    #[serde(default)]
    pub upstream_trace_enabled: bool,

    /// 上游 trace JSONL 落盘路径（默认 `data/upstream_trace.jsonl`）。
    #[serde(default = "default_upstream_trace_path")]
    pub upstream_trace_path: String,

    /// 上游 trace 单文件大小上限（字节，默认 64MiB）。
    ///
    /// 超上限**轮转**（`.jsonl` → `.1` → `.2` → `.3`，最旧的删）而**不是覆盖** ——
    /// 覆盖写会让历史趋势永远拿不到（本仓 ops 侧刚踩过）。磁盘占用上界因此是
    /// `upstreamTraceMaxBytes × 4`，是个可算的有限数（本仓有过日志打满磁盘的事故）。
    #[serde(default = "default_upstream_trace_max_bytes")]
    pub upstream_trace_max_bytes: u64,

    /// 配置文件路径（运行时元数据，不写入 JSON）
    #[serde(skip)]
    config_path: Option<PathBuf>,
}

fn default_upstream_trace_path() -> String {
    "data/upstream_trace.jsonl".to_string()
}

fn default_upstream_trace_max_bytes() -> u64 {
    64 * 1024 * 1024
}

/// 输入压缩配置
///
/// 控制请求体在协议转换完成后、发送到上游前的多层压缩策略。
/// 所有阈值均可通过配置文件调整。
///
/// 当前实现两层（收益最大、风险最小）：
/// 1. 空白压缩：折叠连续空行、移除行尾空格（近乎无损）。
/// 3. tool_result 智能截断：超长工具结果保留头 N 行 + 尾 M 行，中间以占位符省略。
///
/// TODO(后续批次)：thinking 块丢弃/截断、tool_use input 截断、历史轮次截断，
/// 以及截断后 tool_use/tool_result 跨消息配对修复（参考 Fox compressor.rs 的
/// compress_thinking_pass / compress_tool_use_inputs_pass / compress_history_pass /
/// repair_tool_pairing_pass）。这些层风险更高（可能破坏配对/丢历史），暂缓。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompressionConfig {
    /// 总开关，默认 true（但受 `trigger_bytes` 高阈值保护，平时不触发）
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// 触发阈值（字节）：序列化后的 Kiro 请求体超过此大小才启动压缩，默认 4MiB。
    ///
    /// 保守：上游硬限制约 5MiB，这里留足安全余量，只在请求快超限时才压，
    /// 避免对正常小请求做任何有损处理，把对模型输出质量的影响降到最低。
    #[serde(default = "default_compression_trigger_bytes")]
    pub trigger_bytes: usize,

    /// 空白压缩开关（连续空行折叠、行尾空格移除），默认 true
    #[serde(default = "default_true")]
    pub whitespace_compression: bool,

    /// tool_result 截断阈值（字符数），默认 8000；`0` = 关闭该层
    #[serde(default = "default_tool_result_max_chars")]
    pub tool_result_max_chars: usize,

    /// tool_result 智能截断保留头部行数，默认 80
    #[serde(default = "default_tool_result_head_lines")]
    pub tool_result_head_lines: usize,

    /// tool_result 智能截断保留尾部行数，默认 40
    #[serde(default = "default_tool_result_tail_lines")]
    pub tool_result_tail_lines: usize,
}

fn default_compression_trigger_bytes() -> usize {
    4 * 1024 * 1024
}

fn default_tool_result_max_chars() -> usize {
    8000
}

fn default_tool_result_head_lines() -> usize {
    80
}

fn default_tool_result_tail_lines() -> usize {
    40
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            trigger_bytes: default_compression_trigger_bytes(),
            whitespace_compression: default_true(),
            tool_result_max_chars: default_tool_result_max_chars(),
            tool_result_head_lines: default_tool_result_head_lines(),
            tool_result_tail_lines: default_tool_result_tail_lines(),
        }
    }
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_kiro_version() -> String {
    "0.11.107".to_string()
}

fn default_system_version() -> String {
    const SYSTEM_VERSIONS: &[&str] = &["darwin#24.6.0", "win32#10.0.22631"];
    SYSTEM_VERSIONS[fastrand::usize(..SYSTEM_VERSIONS.len())].to_string()
}

fn default_node_version() -> String {
    "22.22.0".to_string()
}

fn default_count_tokens_auth_type() -> String {
    "x-api-key".to_string()
}

fn default_tls_backend() -> TlsBackend {
    TlsBackend::Rustls
}

fn default_load_balancing_mode() -> String {
    "priority".to_string()
}

fn default_extract_thinking() -> bool {
    true
}

/// 吸收层总预算：默认 **45s**，与 `provider.rs` 的 `MAX_REQUEST_RETRY_BUDGET_SECS` 同值。
///
/// 为什么同值而不是更大：每轮的墙钟上限是 `min(45s, 剩余预算)`，两者同源 `min()` 才有意义。
/// 外置 shield 用 600s 预算 / 60 次重试换来 1.07:1 的吸收比，但 **p50 达 73.2s** ——
/// 长预算买到的是延迟而非成功率。45s 下最坏客户端可见延迟 ≈ 45s，约为 shield p50 的六成。
///
/// 号池 429 是**毫秒级 fast-fail**（`acquire_context` 全池冷却时直接带 `retry_after_secs=N`
/// 返回，不消耗 45s），所以 45s 内实际能跑满 `max_rounds`；45s 闸门只在真打上游时才吃满。
fn default_absorb_budget_secs() -> u64 {
    45
}

/// 吸收层最大额外轮次：默认 **3**。
///
/// 放大上限 = `max_rounds × compute_max_retries` ≤ 3×12 = 36 次上游调用，对比外置 shield 的
/// 60×12=720。单号池下 `compute_max_retries(1,1)=1`，故实际最坏只有 4 次。
fn default_absorb_max_rounds() -> u32 {
    3
}

/// 退避下限：默认 **150ms**（号池亚秒级恢复不该被睡满 1s，这是 shield p50 偏高的病根之一）。
fn default_absorb_min_delay_ms() -> u64 {
    150
}

/// 退避上限：默认 **15s**（与外置 shield 的 clamp 上界一致）。
fn default_absorb_max_delay_secs() -> u64 {
    15
}

/// 预算耗尽时回给客户端的状态码：默认 **503**（2026-08-11 改为 503）。
///
/// 为什么默认 503 而非 429：`429` 语义正确但 **Cursor 一类客户端见 429 会掐会话
/// （用户观测：全部暂停）**；`503` 会触发客户端自己退避重试，重试频率受
/// `Retry-After` 控制。吸收层耗尽是「网关已尽力重试、上游仍不可用」的瞬态终态，
/// 不该让客户端把会话掐死——503 + Retry-After 是安全侧。
///
/// 写成 `default_*()` 函数而不是裸 `#[serde(default)]`：后者对 `u16` 给的是 **0**，
/// 那是个非法状态码，会让「缺字段的存量 config.json」拿到一个 provider 侧判不出来的值。
/// 同款陷阱在 `import_keys_enabled` 上已经吃过一次（bool 裸默认是 false）。
fn default_absorb_exhausted_status() -> u16 {
    503
}

/// 推号入口默认**开**：该端点在本开关之前就存在且有外部对接方在用，
/// 默认关会让升级变成一次无声的破坏性变更。
fn default_import_keys_enabled() -> bool {
    true
}

/// 分身默认启用：默认 **false**（= 与本项之前的硬编码值一致，升级零行为变化）。
/// 完整理由见 `Config::clone_default_enabled`。
fn default_clone_default_enabled() -> bool {
    false
}

fn default_cc_auto_buffer() -> bool {
    // 默认 **true** = 识别到 Claude Code 的请求走 buffered 分发。
    //
    // 【为何默认开】buffered 让 message_start 的 input_tokens 用上游 contextUsageEvent 的
    // **准确值**（CC 会校验该字段），这样 CC 直接打 `/v1` 也能拿到正确行为，无需手动改用
    // `/cc/v1`。同时与两处进程级镜像的初值保持一致（`anthropic/handlers.rs` 的
    // `CC_AUTO_BUFFER` static、`admin/types.rs` 的 `ConfigSnapshotResponse::default`
    // 都是 true）——历史上此处返回 false 与那两处相反，是长期的默认值不一致来源。
    //
    // 【代价，务必知情】buffered = 整轮回答对客户端**全程只发 ping、憋到上游流结束才一次性吐**：
    //   ① `contextUsageEvent` **结尾才到**，所以 buffered 等于把整条流憋到最后 →
    //      客户端整轮看不到进度（慢/看不到工具调用），**模型越慢越像卡死**；
    //      客户端侧可能表现为 `Stream idle timeout - no chunks received`。
    //   ② CC 的 steering（执行途中插入消息引导方向）依赖观察流式增量判断当前 turn 状态，
    //      buffered 把整轮变成**不可打断的黑盒** → 途中发消息要等整轮憋完才被处理。
    //
    // 【想要真流式怎么做】把 ccAutoBuffer 设为 false（热更即时生效，无需重启）：
    // 内容边到边逐块转发，`message_start` 发估算 input_tokens、结尾 `message_delta` 携带
    // 上游真实 usage 修正 —— CC 以最终 usage 记账，估算值不影响功能。旁挂实测真流式下
    // CC 能正常干活（工具任务成功、无 input_tokens 报错）且流式增量恢复。
    //
    // 【作用范围】本开关**同时**决定两个端点的分发方式(2026-07-27 统一):
    //   - `/v1/messages`     : 识别到 CC 请求且开关为 true 时走 buffered
    //   - `/cc/v1/messages`  : 开关为 true 时走 buffered
    // 历史缺陷:`/cc/v1` 曾**无条件** buffered,导致把 CC 指向该端点的用户即便把本开关设成
    // false 也关不掉。现两端语义统一,一个开关控制到底。
    true
}

fn default_all_cooling_fast_fail() -> bool {
    true
}

fn default_auto_disable_suspicious() -> bool {
    true
}

/// 全池自愈基础退避：默认 **60s**（= 原 token_manager.rs 硬编码 `SELF_HEAL_BASE_BACKOFF`，
/// 升级零行为变化）。语义与依据见 `Config::self_heal_base_backoff_secs`。
fn default_self_heal_base_backoff_secs() -> u64 {
    60
}

/// 全池自愈退避上限：默认 **900s**（15 分钟，= 原硬编码 `SELF_HEAL_MAX_BACKOFF`）。
fn default_self_heal_max_backoff_secs() -> u64 {
    900
}

/// 指数退避指数上限：默认 **4**（= 原硬编码 `SELF_HEAL_MAX_SHIFT`）。
fn default_self_heal_max_shift() -> u32 {
    4
}

fn default_endpoint() -> String {
    crate::kiro::endpoint::ide::IDE_ENDPOINT_NAME.to_string()
}

/// RPM headroom 系数默认 85(预留 15% 缓冲)。
fn default_rate_limit_jitter_pct() -> u32 {
    20
}
fn default_inbound_target_rpm() -> u32 {
    100
}
fn default_inbound_rpm_min() -> u32 {
    20
}
fn default_inbound_rpm_max() -> u32 {
    300
}
fn default_inbound_burst_secs() -> u32 {
    2
}
fn default_inbound_queue_max_wait_secs() -> u32 {
    30
}
fn default_inbound_queue_timeout_passthrough() -> bool {
    true
}
fn default_upstream_concurrency_limit() -> usize {
    16
}
/// 每凭据并发上限默认 8 = 全局默认 16 的一半，保证至少两个号能同时打满
/// （对照 kiro2cc：全局 50 + 每凭据 20，同为 40% 量级）。
fn default_upstream_per_credential_limit() -> usize {
    8
}
fn default_cooldown_scale_pct() -> u32 {
    100
}
fn default_rpm_headroom_factor() -> u32 {
    85
}

/// 余额加权默认开(动态化出厂即用,dwgx 真机观察拉平)。
fn default_balance_weight_enabled() -> bool {
    true
}

/// 余额加权 FLOOR 默认 50(因子 [0.5,1.0],差 10~20% 微调)。
fn default_balance_weight_floor() -> u32 {
    50
}

/// 429 降权默认开(现有 health/EWMA 机制)。
fn default_health_429_weight_enabled() -> bool {
    true
}

fn default_cooldown_enabled() -> bool {
    true
}

fn default_affinity_enabled() -> bool {
    true
}

fn default_rate_limit_daily() -> u32 {
    500
}

fn default_rate_limit_min_interval_ms() -> u64 {
    1000
}

fn default_prompt_cache_enabled() -> bool {
    // 默认**开启**下发。
    //
    // 为什么由 false 改为 true：这个开关此前是死配置（全仓零读取点），而注入行为一直在
    // 无条件发生——即用户显式写 "promptCacheEnabled": false 也照样注入，配置在说谎。
    // 现在把它接上真实读取点时，必须在两个方向里选一个作为默认，而"保持既有可观测行为
    // 不变"比"忠于一个从未生效过的默认值"更重要：沿用 false 会让所有现网客户端的缓存
    // 显示在升级后**突然消失**，那是一次没人要求的行为回退。
    //
    // 旧注释声称的 build_profile（JSON 规范化 + SHA256 指纹）热路径开销**已不存在**：
    // 现行实现是 token::count_prefix_tokens 的一次线性估算，开销与原本就要做的
    // count_all_tokens 同量级，不再是需要靠默认关来规避的成本。
    true
}

fn default_prompt_cache_ttl_seconds() -> u64 {
    3600
}

fn default_strip_env_noise() -> bool {
    true
}

/// 泄漏控制 token 清洗默认**开启**：治 #70544 模型泄漏（course/課/count 粘连），保守只剥行首
/// 高信号粘连、正常文本零误删。纯缓解、对干净输出零影响，故默认开。
fn default_tool_reclaim_textified_invoke() -> bool {
    true
}
fn default_tool_stray_repeat_guard() -> bool {
    true
}
fn default_tool_clean_leaked_tokens() -> bool {
    true
}

/// 流式失败态对齐默认**开启**：工具拼装非法时置失败态（与非流式一致，不再静默记成功），
/// 配合 ③ 才让「修复层也修不好的残留」有干净的失败信号。绝不连坐号。
fn default_tool_stream_align_failure() -> bool {
    true
}

/// 工具错误如实暴露客户端默认**开启**：与 ② 配对——修复层④修不好时不发坏 JSON，改发明确 SSE
/// error 让客户端退避重试，客户端不再拿坏参数报 Invalid tool parameters。
fn default_tool_expose_error_to_client() -> bool {
    true
}

/// JSON 修复层默认**开启**：只在 JSON 已非法时介入 + 修复后强制复验，对正常流零影响，纯增益。
fn default_tool_repair_json() -> bool {
    true
}

/// 截断跨轮恢复默认关：它改变对话流程（截断→整轮失败重试），需用户显式开启。
fn default_tool_truncation_recovery() -> bool {
    false
}

/// 工具顶层描述上限默认 10000 字符（保持既有硬编码行为，只是提为可配置）。
fn default_tool_description_max_chars() -> usize {
    10000
}

fn default_usage_enabled() -> bool {
    true
}

fn default_usage_data_dir() -> String {
    "data/usage".to_string()
}

fn default_usage_retention_days() -> i64 {
    30
}

fn default_collect_client_fingerprint() -> bool {
    true
}

fn default_max_body_bytes() -> usize {
    // 256MiB 大软上限：远超正常请求（上游 compression 4MiB 触发、~5MiB 就 400），
    // 又挡住恶意超大 body 打死进程。想彻底放开可显式设 0（= 不限制，见 anthropic/router.rs）。
    256 * 1024 * 1024
}

fn default_true() -> bool {
    true
}

fn default_refresh_lead_minutes() -> i64 {
    10
}

fn default_refresh_interval_secs() -> u64 {
    60
}

fn default_trash_retention_days() -> u32 {
    30
}

fn default_balance_refresh_interval_secs() -> u64 {
    1800
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            region: default_region(),
            auth_region: None,
            api_region: None,
            kiro_version: default_kiro_version(),
            machine_id: None,
            encrypt_credentials_at_rest: false,
            api_key: None,
            system_version: default_system_version(),
            node_version: default_node_version(),
            tls_backend: default_tls_backend(),
            count_tokens_api_url: None,
            count_tokens_api_key: None,
            count_tokens_auth_type: default_count_tokens_auth_type(),
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            admin_api_key: None,
            load_balancing_mode: default_load_balancing_mode(),
            extract_thinking: default_extract_thinking(),
            cc_auto_buffer: default_cc_auto_buffer(),
            // native effort 默认关：开关开=白名单模型走 output_config.effort 原生通道
            // （未验证的协议形状不全池直切，与 cli_origin_kiro_cli 同款保守理由）。
            native_thinking_effort_enabled: false,
            import_keys_enabled: default_import_keys_enabled(),
            clone_default_enabled: default_clone_default_enabled(),
            // 吸收层十项：**必须调 default_*()，不得另写字面量** —— 默认值散落多处的
            // 历史不一致正是 `cc_auto_buffer_default_is_on_and_consistent_across_mirrors`
            // 那条测试要防的形态。suspended 无 default_*() 函数（裸 serde default）。
            upstream_retry_absorb_enabled: false,
            upstream_retry_absorb_budget_secs: default_absorb_budget_secs(),
            upstream_retry_absorb_max_rounds: default_absorb_max_rounds(),
            upstream_retry_absorb_min_delay_ms: default_absorb_min_delay_ms(),
            upstream_retry_absorb_max_delay_secs: default_absorb_max_delay_secs(),
            upstream_retry_absorb_suspended: false,
            upstream_retry_absorb_server_error: false,
            upstream_retry_absorb_capacity_400: false,
            upstream_retry_absorb_swap_budget_secs: 0,
            upstream_retry_absorb_exhausted_status: default_absorb_exhausted_status(),
            default_endpoint: default_endpoint(),
            endpoints: HashMap::new(),
            // CLI body 对齐 kiro-rs 默认关（线上号池正在服务，未验证的协议形状不全池直切）。
            cli_origin_kiro_cli: false,
            // 隐私优先：默认仍发 optout: true（拒绝会话被用于训练），需显式开启才对齐真实 CLI。
            cli_codewhisperer_optout_false: false,
            // UA 指纹对齐默认关：无对照实验数据前不拿生产流量赌未验证假设。
            cli_ua_align_real_client: false,
            cooldown_enabled: default_cooldown_enabled(),
            cooldown_scale_pct: default_cooldown_scale_pct(),
            rate_limit_jitter_pct: default_rate_limit_jitter_pct(),
            // 默认 Manual：不覆盖任何字段，保证既有配置行为零变化
            // （守卫 `throttle_profile_defaults_to_manual_and_changes_nothing` 钉住这点）。
            throttle_profile: ThrottleProfile::default(),
            inbound_throttle_enabled: default_true(),
            inbound_rpm_auto: default_true(),
            inbound_target_rpm: default_inbound_target_rpm(),
            inbound_rpm_min: default_inbound_rpm_min(),
            inbound_rpm_max: default_inbound_rpm_max(),
            inbound_burst_secs: default_inbound_burst_secs(),
            inbound_queue_max_wait_secs: default_inbound_queue_max_wait_secs(),
            inbound_queue_timeout_passthrough: default_inbound_queue_timeout_passthrough(),
            upstream_concurrency_limit: default_upstream_concurrency_limit(),
            upstream_per_credential_limit: default_upstream_per_credential_limit(),
            deepseek_normalize: Default::default(),
            model_mapping: Default::default(),
            rate_limit_enabled: false,
            rate_limit_daily_max: default_rate_limit_daily(),
            rate_limit_min_interval_ms: default_rate_limit_min_interval_ms(),
            affinity_enabled: default_affinity_enabled(),
            credential_rpm_limit: 0,
            rpm_headroom_factor: default_rpm_headroom_factor(),
            rpm_reserve_slots: 0,
            rpm_hard_gate_overload_wait: false,
            balance_weight_enabled: default_balance_weight_enabled(),
            balance_weight_floor: default_balance_weight_floor(),
            health_429_weight_enabled: default_health_429_weight_enabled(),
            all_cooling_fast_fail: default_all_cooling_fast_fail(),
            auto_disable_suspicious: default_auto_disable_suspicious(),
            priority_in_balanced: false,
            // 默认 false = priority 全局统一比较（修正历史上"代挂号隐含绝对优先"的反直觉行为）。
            custom_api_first: false,
            prompt_cache_enabled: default_prompt_cache_enabled(),
            prompt_cache_ttl_seconds: default_prompt_cache_ttl_seconds(),
            strip_env_noise: default_strip_env_noise(),
            tool_clean_leaked_tokens: default_tool_clean_leaked_tokens(),
            tool_reclaim_textified_invoke: default_tool_reclaim_textified_invoke(),
            tool_stray_repeat_guard: default_tool_stray_repeat_guard(),
            tool_stream_align_failure: default_tool_stream_align_failure(),
            tool_expose_error_to_client: default_tool_expose_error_to_client(),
            tool_repair_json: default_tool_repair_json(),
            tool_truncation_recovery: default_tool_truncation_recovery(),
            tool_description_max_chars: default_tool_description_max_chars(),
            callback_base_url: None,
            usage_enabled: default_usage_enabled(),
            usage_data_dir: default_usage_data_dir(),
            usage_retention_days: default_usage_retention_days(),
            collect_client_fingerprint: default_collect_client_fingerprint(),
            cors_allowed_origins: Vec::new(),
            ip_allowlist: Vec::new(),
            ip_blocklist: Vec::new(),
            machine_code_blocklist: Vec::new(),
            trust_forwarded_header: false,
            ingress_rate_limit_per_min: 0,
            max_body_bytes: default_max_body_bytes(),
            proactive_token_refresh: default_true(),
            token_refresh_lead_minutes: default_refresh_lead_minutes(),
            token_refresh_interval_secs: default_refresh_interval_secs(),
            login_background_enabled: default_true(),
            login_background_r18: false,
            trash_retention_days: default_trash_retention_days(),
            balance_refresh_interval_secs: default_balance_refresh_interval_secs(),
            // 自愈退避三项：**必须调 default_*()，不得另写字面量**（默认值守卫兜底）。
            self_heal_base_backoff_secs: default_self_heal_base_backoff_secs(),
            self_heal_max_backoff_secs: default_self_heal_max_backoff_secs(),
            self_heal_max_shift: default_self_heal_max_shift(),
            compression: CompressionConfig::default(),
            overload_fallback_model: None,
            upstream_trace_enabled: false,
            upstream_trace_path: default_upstream_trace_path(),
            upstream_trace_max_bytes: default_upstream_trace_max_bytes(),
            config_path: None,
        }
    }
}

impl Config {
    /// 获取默认配置文件路径
    pub fn default_config_path() -> &'static str {
        "config.json"
    }

    /// 获取有效的 Auth Region（用于 Token 刷新）
    /// 优先使用 auth_region，未配置时回退到 region
    pub fn effective_auth_region(&self) -> &str {
        self.auth_region.as_deref().unwrap_or(&self.region)
    }

    /// 获取有效的 API Region（用于 API 请求）
    /// 优先使用 api_region，未配置时回退到 region
    pub fn effective_api_region(&self) -> &str {
        self.api_region.as_deref().unwrap_or(&self.region)
    }

    /// 从文件加载配置
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            // 配置文件不存在，返回默认配置
            let mut config = Self::default();
            config.config_path = Some(path.to_path_buf());
            return Ok(config);
        }

        let content = fs::read_to_string(path)?;
        let mut config: Config = serde_json::from_str(&content)?;
        config.config_path = Some(path.to_path_buf());

        // 🔴 档位只对**文件里没显式写**的字段生效（2026-08-11）。
        //
        // 为什么必须先看原始 JSON：那 7 个受档位管的字段都是「非 Option + serde default」，
        // 反序列化**之后**已经分不清「用户显式写了 false」和「字段缺失走了默认 false」。
        // 而线上 config.json 的 102 个键里这 7 个全部显式写了 —— 无条件覆盖会把生产配置冲掉。
        // 所以在这里、用解析前的原始 JSON 判定"哪些键真的存在"，档位只填空。
        let explicit: std::collections::HashSet<String> =
            match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(serde_json::Value::Object(m)) => m.keys().cloned().collect(),
                // 解析成非对象（理论上不可能，上面 from_str::<Config> 已经成功）：
                // 保守当作"全部显式"，即档位不改任何东西。
                _ => std::collections::HashSet::new(),
            };
        config.apply_throttle_profile(&explicit);

        Ok(config)
    }

    /// **用户从面板主动切档**时用这个：让档位对所有受管字段生效（不做"只填空"保护）。
    ///
    /// 与文件加载路径（[`Self::apply_throttle_profile`]，只填空不覆盖）刻意分开：
    /// - 文件加载时分不清「显式 false」与「缺失默认 false」，而线上那 7 个字段全部显式写过，
    ///   无条件覆盖会改写生产配置 ⇒ 必须只填空。
    /// - 面板切档是明确的意图表达；若还只填空，由于键都已存在，会**一个字段都改不动**
    ///   （按钮点了没反应，比冲掉配置更糟）⇒ 必须真的写进去。
    ///
    /// 切档后的值会随 `save()` 落盘成显式键，下次启动时它们就是"显式"的、
    /// 不会被加载路径再次覆盖 —— 两条路径自洽。
    pub fn apply_throttle_profile_for_explicit_switch(&mut self) {
        self.apply_throttle_profile(&std::collections::HashSet::new());
    }

    /// 把档位预设应用到**未显式配置**的字段上。
    ///
    /// `explicit` 是配置文件里真实出现过的 camelCase 键名集合。
    /// 契约：**只填空、不覆盖**。任何出现在 `explicit` 里的键，档位一律不碰 ——
    /// 这是向前兼容的核心保证（详见 [`ThrottleProfile`] 的文档）。
    ///
    /// `Manual`（默认）直接返回，一个字段都不动。
    fn apply_throttle_profile(&mut self, explicit: &std::collections::HashSet<String>) {
        use ThrottleProfile as P;
        let profile = self.throttle_profile;
        if profile == P::Manual {
            return;
        }

        // 小工具：键未显式出现时才写入。
        //
        // ⚠️ `explicit` 必须**显式传进宏**，不能靠宏体直接引用外层局部变量：
        // `macro_rules!` 的卫生性（hygiene）会让宏体里的标识符在**定义处**的语境解析，
        // 而不是展开处的语境。靠捕获写出来的版本在本轮实测中会静默失效
        // （`explicit` 解析不到同一个绑定 ⇒ 检查形同不存在 ⇒ 显式值被覆盖），
        // 而这恰好是本函数唯一必须守住的契约。
        macro_rules! fill {
            ($ex:expr, $key:literal, $field:ident, $val:expr) => {
                if !$ex.contains($key) {
                    self.$field = $val;
                }
            };
        }

        match profile {
            P::Shielded => {
                // 整形层做真限流：排队超时返 429，而不是放行。
                // 对 shield 而言"放行"等于"重试成功"（它会立刻发下一个）；返 429 才能让它
                // 走 cool 分支听我们的 Retry-After，把 60 次重打压成按真值退避。
                fill!(explicit, 
                    "inboundQueueTimeoutPassthrough",
                    inbound_queue_timeout_passthrough,
                    false
                );
                // 冷却开：让 429 过的号真正退避。关掉时坏号会被立刻重选 = 原地打转，
                // 这是放大链里最便宜的一刀。
                fill!(explicit, "cooldownEnabled", cooldown_enabled, true);
                fill!(explicit, "inboundThrottleEnabled", inbound_throttle_enabled, true);
                // 吸收层：Shielded 档刻意**不开** —— 外层 shield 已经在吸收，
                // 网关内再吸收会叠乘（shield 60 次 × 网关吸收轮数）。
                fill!(explicit, 
                    "upstreamRetryAbsorbEnabled",
                    upstream_retry_absorb_enabled,
                    false
                );
            }
            P::Direct => {
                // 无外挂：宁可慢也不要拒，排队超时放行。
                fill!(explicit, 
                    "inboundQueueTimeoutPassthrough",
                    inbound_queue_timeout_passthrough,
                    true
                );
                fill!(explicit, "cooldownEnabled", cooldown_enabled, true);
                fill!(explicit, "inboundThrottleEnabled", inbound_throttle_enabled, true);
                // 吸收层开：网关内部多承担，少让客户端看见错误。
                // ⚠️ 已知边界：吸收层**不覆盖透传路径**，纯代挂号池下开了也不生效。
                fill!(explicit, 
                    "upstreamRetryAbsorbEnabled",
                    upstream_retry_absorb_enabled,
                    true
                );
            }
            P::Manual => unreachable!("上面已提前返回"),
        }
    }

    /// 获取配置文件路径（如果有）
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// 将当前配置写回原始配置文件
    pub fn save(&self) -> anyhow::Result<()> {
        let path = self
            .config_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，无法保存配置"))?;

        let content = serde_json::to_string_pretty(self).context("序列化配置失败")?;
        // 原子写:config.json 明文含 adminApiKey / proxyPassword,裸 fs::write 崩溃会截断
        // → 面板密钥丢失锁死管理入口。走 temp→fsync→rename(创建即 0600,无 rename 后设权的短
        // world-readable 窗口)+ Windows 句柄占用 rename 重试。见 common::fs_atomic。
        // 在 Tokio runtime 内(save 从 update_config 异步 handler 调)用 block_in_place,
        // 避免 rename 重试的同步 sleep 阻塞 worker(与 persist_credentials 同一惯例)。
        let bytes = content.as_bytes();
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| crate::common::fs_atomic::write_atomic(path, bytes))
                .with_context(|| format!("写入配置文件失败: {}", path.display()))?;
        } else {
            crate::common::fs_atomic::write_atomic(path, bytes)
                .with_context(|| format!("写入配置文件失败: {}", path.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 🔴 **语义陷阱备忘守卫**（2026-08-11）：这三个开关的名字与真实后果不对应。
    ///
    /// 本测试不校验"值应该是什么"（那由部署环境决定），只钉住**默认值**和
    /// **它们必须成组理解**这件事。它存在的理由是：2026-08-11 排查
    /// 「外部 30 RPM 打成上游 1000 RPM」时发现，线上这三个的组合
    /// （`queueTimeoutPassthrough=true` + `cooldownEnabled=false`）**不属于任何一档**——
    /// 两个方向都放开，于是外层重试外挂的放大能完整穿透到上游
    /// （幅度以实测为准，见 `ThrottleProfile` 文档；曾推算的 480× 已被实测推翻）。
    ///
    /// | 字段 | 名字看起来 | 真实后果 |
    /// |---|---|---|
    /// | `cooldown_enabled=false` | 「不用冷却功能」 | 429 过的号**不被跳过、立刻可重选** ⇒ 换号=原地打转 |
    /// | `inbound_queue_timeout_passthrough=true` | 「排队超时别拒绝」 | 整形层退化成**延迟器**；前面有重试外挂时"放行"=鼓励它立刻重发 |
    /// | `inbound_rpm_auto`（默认 **true**，线上刻意 **false**） | 「自动调 RPM 挺好」 | 内置 AIMD 是**单向棘轮**：429 乘性减半、回升要 20s 静默 ×N，而实测每 6.4s 一次 429 ⇒ 锁死在下限（曾卡 30 RPM 而池能跑 216）。线上关掉是对的，代价是目标 RPM 完全交给外部脚本 |
    ///
    /// 改这三个默认值前请读 [`ThrottleProfile`] 的档位定义，并想清楚
    /// 「网关前面有没有会自动重试的组件」——这是决定取值的唯一关键问题。
    #[test]
    fn throttle_semantic_traps_defaults_are_documented() {
        let c: Config = serde_json::from_str("{}").expect("缺字段的 config 必须能反序列化");

        // 代码默认值（**不是**线上值）。断言它们是为了：改动时必须来读上面那张表。
        assert!(
            c.cooldown_enabled,
            "cooldown_enabled 的代码默认必须是 true。\
             改成 false 意味着 429 过的号不再被跳过、会被立刻重选（换号变成原地打转）—— \
             那是放大链的一环，不是'关掉一个功能'"
        );
        assert!(
            c.inbound_queue_timeout_passthrough,
            "inbound_queue_timeout_passthrough 的代码默认必须是 true（直连场景：宁可慢不要拒）。\
             ⚠️ 但网关前面有重试外挂时这个值应当是 false —— 选 ThrottleProfile::Shielded 档，\
             而不是改这里的默认值（默认值要服务于最常见的直连场景）"
        );
        // ⚠️ `inbound_rpm_auto` 的代码默认是 **true**，而线上刻意设成 **false** ——
        // 这是已知且有依据的分歧，不是配置漂移：
        //   内置 AIMD 是**单向棘轮**（429 就乘性减半，回升要 20s 静默 ×N），
        //   而线上实测每 6.4s 就有一次 429 ⇒ 单调下滑锁死在下限
        //   （曾卡在 30 RPM 而号池能跑 216）。
        // 所以这里断言的是"默认仍是 true"，用途是：若有人把默认改成 false，
        // 必须同时来更新这段说明（否则线上那个 false 就失去了"刻意偏离默认"的语境，
        // 下一个人会以为它只是没设）。
        assert!(
            c.inbound_rpm_auto,
            "inbound_rpm_auto 的代码默认变了（原为 true）。\
             若这是有意为之，请更新本测试上方关于「线上刻意设 false」的说明 —— \
             那条记录依赖'代码默认是 true'这个语境才成立"
        );
    }

    /// 🔴 **向前兼容硬保证**：档位默认 `Manual`，且 `Manual` 一个字段都不改。
    ///
    /// 线上 `config.json` 有 102 个键，受档位管的 7 个**全部显式写了**。
    /// 若档位默认值不是 `Manual`、或 `Manual` 会改字段，升级二进制的瞬间就会
    /// 把生产配置整体改写（`inboundQueueTimeoutPassthrough` / `cooldownEnabled`
    /// 这类开关一翻，限流行为立刻变）。
    #[test]
    fn throttle_profile_defaults_to_manual_and_changes_nothing() {
        // ① 默认必须是 Manual
        assert_eq!(
            ThrottleProfile::default(),
            ThrottleProfile::Manual,
            "档位默认值必须是 Manual —— 任何别的默认值都会在升级瞬间改写既有生产配置"
        );
        let bare: Config =
            serde_json::from_str("{}").expect("缺字段的 config 必须能反序列化");
        assert_eq!(
            bare.throttle_profile,
            ThrottleProfile::Manual,
            "配置文件不含 throttleProfile 时必须落到 Manual"
        );

        // ② Manual 下 apply 不改任何字段（用空 explicit 集合 = 最激进的填空条件）
        let mut c = bare.clone();
        let before = (
            c.inbound_queue_timeout_passthrough,
            c.cooldown_enabled,
            c.inbound_throttle_enabled,
            c.upstream_retry_absorb_enabled,
        );
        c.apply_throttle_profile(&std::collections::HashSet::new());
        assert_eq!(
            (
                c.inbound_queue_timeout_passthrough,
                c.cooldown_enabled,
                c.inbound_throttle_enabled,
                c.upstream_retry_absorb_enabled,
            ),
            before,
            "Manual 档必须一个字段都不改"
        );
    }

    /// 档位的**只填空不覆盖**契约：显式写过的键，档位一律不碰。
    #[test]
    fn throttle_profile_never_overrides_explicit_keys() {
        // 构造「用户显式把两个开关设成与 Shielded 档相反」的配置
        let mut c: Config = serde_json::from_str(
            r#"{"inboundQueueTimeoutPassthrough": true, "cooldownEnabled": false}"#,
        )
        .expect("应能反序列化");
        c.throttle_profile = ThrottleProfile::Shielded;

        let explicit: std::collections::HashSet<String> =
            ["inboundQueueTimeoutPassthrough", "cooldownEnabled"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        c.apply_throttle_profile(&explicit);

        assert!(
            c.inbound_queue_timeout_passthrough,
            "用户显式写的 true 被 Shielded 档冲成了 false —— 违反「只填空不覆盖」契约。\
             线上 config.json 这 7 个字段全部显式写过，冲掉就是改写生产配置。"
        );
        assert!(
            !c.cooldown_enabled,
            "用户显式写的 false 被 Shielded 档冲成了 true —— 同上"
        );

        // 反面：没显式写的字段，档位应当填上
        let mut c2: Config = serde_json::from_str("{}").expect("应能反序列化");
        c2.throttle_profile = ThrottleProfile::Shielded;
        c2.inbound_queue_timeout_passthrough = true; // 代码默认就是 true
        c2.cooldown_enabled = false;
        c2.apply_throttle_profile(&std::collections::HashSet::new());
        assert!(
            !c2.inbound_queue_timeout_passthrough,
            "Shielded 档应把未显式配置的 queueTimeoutPassthrough 填成 false（整形做真限流）"
        );
        assert!(
            c2.cooldown_enabled,
            "Shielded 档应把未显式配置的 cooldownEnabled 填成 true（让 429 过的号真退避）"
        );
    }

    #[test]
    fn cc_auto_buffer_default_is_on_and_consistent_across_mirrors() {
        // ccAutoBuffer 的默认值散落在**三处**，历史上曾长期不一致
        //（config 默认 false，而 handlers 的 static 初值与 admin 快照 Default 都是 true）：
        //   ① 本函数 default_cc_auto_buffer()
        //   ② src/anthropic/handlers.rs 的 CC_AUTO_BUFFER static 初值
        //   ③ src/admin/types.rs 的 ConfigSnapshotResponse::default
        // 运行时 ② 会被 main 启动播种覆盖，所以不一致不会立刻出错——但会让单元测试、
        // 以及任何绕过 create_router_with_provider 的代码路径读到错的默认值，排障时极易误判。
        // 此处把 ①②的一致性钉死；改任一处默认值都必须同步另一处，否则本测试失败。
        assert!(
            default_cc_auto_buffer(),
            "ccAutoBuffer 默认应为 true（CC 请求走 buffered，message_start 用上游精确 input_tokens）"
        );
        assert_eq!(
            Config::default().cc_auto_buffer,
            default_cc_auto_buffer(),
            "Config::default() 必须走 default_cc_auto_buffer()，不得另写字面量"
        );
        // ② 的一致性断言放在 handlers 自己的测试里
        //（cc_auto_buffer_enabled 是模块私有函数，不为测试放宽生产可见性）：
        //   见 anthropic::handlers::tier3_hotreload_tests::cc_auto_buffer_static_matches_config_default
    }

    /// `Config::default()` 的**带默认函数**吸收层项必须走 `default_absorb_*()`，不得另写
    /// 字面量（实测守卫覆盖 5 项：budget/max_rounds/min_delay/max_delay/exhausted_status；
    /// capacity_400 与 swap_budget_secs 无 default 函数，Default 里写与 `serde(default)`
    /// 一致的字面量，不在此守卫范围）。
    ///
    /// 回退即 FAIL：把 `Config::default()` 里任一项改成硬编码数字（例如 `budget_secs: 60`），
    /// 本测试立刻失败。这是 `cc_auto_buffer` 那条镜像一致性测试的同款守卫 —— 该字段的历史
    /// 教训正是「默认值散落多处、长期互相矛盾」，而 `Config::default()` 是最容易被随手写
    /// 字面量的一处。
    ///
    /// 注：`ConfigSnapshotResponse` **没有** `Default` impl（它每次都由
    /// `build_config_snapshot` 从 config 逐字段构造），所以不存在第三处默认值镜像；
    /// 真正的漂移面在「快照是否真的逐字段读 config」，由 admin/service.rs 的
    /// `absorb_snapshot_maps_every_field_from_config` 守卫。
    #[test]
    fn absorb_config_default_goes_through_default_fns() {
        let cfg = Config::default();
        assert_eq!(
            cfg.upstream_retry_absorb_budget_secs,
            default_absorb_budget_secs(),
            "Config::default() 必须走 default_absorb_budget_secs()，不得另写字面量"
        );
        assert_eq!(
            cfg.upstream_retry_absorb_max_rounds,
            default_absorb_max_rounds()
        );
        assert_eq!(
            cfg.upstream_retry_absorb_min_delay_ms,
            default_absorb_min_delay_ms()
        );
        assert_eq!(
            cfg.upstream_retry_absorb_max_delay_secs,
            default_absorb_max_delay_secs()
        );
        // 预算与 provider 的单轮墙钟闸门同值是刻意的（两者同源 min() 才有意义）。
        assert_eq!(
            default_absorb_budget_secs(),
            45,
            "吸收总预算应与 provider.rs 的 MAX_REQUEST_RETRY_BUDGET_SECS 同值"
        );
        assert_eq!(
            cfg.upstream_retry_absorb_exhausted_status,
            default_absorb_exhausted_status()
        );
    }

    /// ⭐ 默认值守卫：自愈退避三项必须走 default_*()，且等于原 token_manager.rs 硬编码值
    /// （60s / 900s / 4）。常量已从 token_manager.rs 移除，这里是防默认漂移的唯一锚点
    /// —— 改默认值 = 升级行为变化，必须有意识为之。
    ///
    /// 与 `absorb_config_default_goes_through_default_fns` 同款范式：
    /// - `Config::default()` 必须走 default_*()，不得另写字面量；
    /// - serde 缺字段路径（存量 config.json 没有这三个键）必须与 Config::default() 同源；
    /// - 显式配置必须被尊重，否则配置化等于不存在。
    #[test]
    fn self_heal_backoff_defaults_go_through_default_fns() {
        let cfg = Config::default();
        assert_eq!(
            cfg.self_heal_base_backoff_secs,
            default_self_heal_base_backoff_secs(),
            "Config::default() 必须走 default_self_heal_base_backoff_secs()"
        );
        assert_eq!(
            cfg.self_heal_max_backoff_secs,
            default_self_heal_max_backoff_secs()
        );
        assert_eq!(
            cfg.self_heal_max_shift,
            default_self_heal_max_shift()
        );

        // 钉死与原硬编码常量同值（token_manager.rs 的 SELF_HEAL_* 已删除）。
        assert_eq!(default_self_heal_base_backoff_secs(), 60, "原 SELF_HEAL_BASE_BACKOFF=60s");
        assert_eq!(default_self_heal_max_backoff_secs(), 900, "原 SELF_HEAL_MAX_BACKOFF=900s");
        assert_eq!(default_self_heal_max_shift(), 4, "原 SELF_HEAL_MAX_SHIFT=4");

        // serde 缺字段路径必须与 Config::default() 同源：存量 config.json 没有这三个键。
        let from_json: Config =
            serde_json::from_str("{}").expect("缺字段的 config 必须能反序列化");
        assert_eq!(from_json.self_heal_base_backoff_secs, 60);
        assert_eq!(from_json.self_heal_max_backoff_secs, 900);
        assert_eq!(from_json.self_heal_max_shift, 4);

        // 显式配置必须被尊重（否则配置化等于不存在）。
        let on: Config = serde_json::from_str(
            r#"{"selfHealBaseBackoffSecs":5,"selfHealMaxBackoffSecs":60,"selfHealMaxShift":3}"#,
        )
        .expect("合法 JSON 必须能反序列化");
        assert_eq!(on.self_heal_base_backoff_secs, 5);
        assert_eq!(on.self_heal_max_backoff_secs, 60);
        assert_eq!(on.self_heal_max_shift, 3);
    }

    /// ⭐ 硬约束守卫：**合并外挂能力的四个新旋钮全部默认「保持现状」**。
    ///
    /// 线上正在服务且号池只剩个位数，任何「顺手改默认值」都是事故。这四项各自的默认值
    /// 都必须是「行为与本批改动之前逐字节相同」的那一侧：
    /// - 两个 bool 默认 false ⇒ 新增的两类（5xx / 容量 400）分类出来也不吸收；
    /// - swap 预算默认 0 ⇒ 换号空窗仍走旧的 min_delay 指数曲线与总预算；
    /// - 耗尽状态码默认 503（2026-08-11 改）⇒ provider 打 `absorb_budget_exhausted=1` 标记、
    ///   map_provider_error 首分支回 503 + Retry-After（见下方断言与 handlers 分支守卫）。
    ///
    /// 回退即 FAIL：把任一项默认值改到「新行为」那一侧。
    #[test]
    fn newly_merged_absorb_knobs_all_default_to_current_behavior() {
        let cfg = Config::default();
        assert!(
            !cfg.upstream_retry_absorb_server_error,
            "5xx 吸收默认必须关：外挂实测 11.6 次重试才救回 1 个请求，\
             那个比值就是「不分失败机理一律重试」的账单"
        );
        assert!(
            !cfg.upstream_retry_absorb_capacity_400,
            "容量 400 吸收默认必须关"
        );
        assert_eq!(
            cfg.upstream_retry_absorb_swap_budget_secs, 0,
            "换号空窗独立预算默认必须 0 —— 非零意味着单条请求可占用连接数分钟"
        );
        assert_eq!(
            cfg.upstream_retry_absorb_exhausted_status, 503,
            "耗尽状态码默认必须 503（2026-08-11 改）：429 会让 Cursor 一类客户端掐会话\
             （用户实测全部暂停）；503 触发客户端退避重试，频率受 Retry-After 控制"
        );

        // ⭐ serde 缺字段路径必须与 Config::default() 同源：存量 config.json 没有这四个键。
        let from_json: Config = serde_json::from_str("{}").expect("缺字段的 config 必须能反序列化");
        assert!(!from_json.upstream_retry_absorb_server_error);
        assert!(!from_json.upstream_retry_absorb_capacity_400);
        assert_eq!(from_json.upstream_retry_absorb_swap_budget_secs, 0);
        assert_eq!(
            from_json.upstream_retry_absorb_exhausted_status, 503,
            "裸 #[serde(default)] 对 u16 给的是 0（非法状态码），必须走 default_* 函数；\
             缺字段路径默认 503（2026-08-11 改，与 Config::default() 同源）"
        );

        // 显式开启仍必须被尊重，否则这些开关等于不存在。
        let on: Config = serde_json::from_str(
            r#"{"upstreamRetryAbsorbServerError":true,"upstreamRetryAbsorbCapacity400":true,
                "upstreamRetryAbsorbSwapBudgetSecs":600,"upstreamRetryAbsorbExhaustedStatus":503}"#,
        )
        .expect("显式值必须能反序列化");
        assert!(on.upstream_retry_absorb_server_error);
        assert!(on.upstream_retry_absorb_capacity_400);
        assert_eq!(on.upstream_retry_absorb_swap_budget_secs, 600);
        assert_eq!(on.upstream_retry_absorb_exhausted_status, 503);
    }

    /// ⭐ 硬约束守卫：`native_thinking_effort_enabled` **默认必须关**（= 行为逐字节不变）。
    ///
    /// 开=白名单模型 + thinking 走 `output_config.effort` 原生通道并抑制 XML 标签注入，
    /// 而那条通道只有参考仓单次实测支撑（2026-06-07，Kiro CLI 2.6.0 + Opus 4.8），
    /// 未验证的协议形状不全池直切（与 `cli_origin_kiro_cli` 同款保守理由）。
    ///
    /// 回退即 FAIL：把默认改成 true，或 serde 缺字段路径与 Config::default() 分叉。
    #[test]
    fn native_thinking_effort_defaults_to_off() {
        assert!(
            !Config::default().native_thinking_effort_enabled,
            "native effort 默认必须关：未验证的协议形状不得随升级全池直切"
        );
        // serde 缺字段路径与 Config::default() 同源：存量 config.json 没有这个键。
        let from_json: Config = serde_json::from_str("{}").expect("缺字段的 config 必须能反序列化");
        assert_eq!(
            from_json.native_thinking_effort_enabled,
            Config::default().native_thinking_effort_enabled,
            "serde default 与 Config::default() 必须一致"
        );
        // 显式开启必须仍被尊重，否则开关等于不存在。
        let on: Config = serde_json::from_str(r#"{"nativeThinkingEffortEnabled":true}"#)
            .expect("显式值必须能反序列化");
        assert!(on.native_thinking_effort_enabled);
    }

    /// 吸收层与 403 风控吸收**都默认关**。
    ///
    /// 「默认开」在 2026-08-04 试过并当天回退：支撑它的「38% 可吸收」是错的
    /// （池空那 16.5% 的自愈退避下限 60s > 吸收总预算 45s，结构上吸收不了），
    /// 且开着会让 `budget_secs` 反向支配既有的 45s failover 墙钟、把
    /// `ABSOLUTE_MAX_TOTAL_RETRIES` 从每请求变成每轮（最坏 16 次上游调用）。
    /// 完整依据见 `upstream_retry_absorb_enabled` 的字段文档。
    ///
    /// 回退即 FAIL：把任一默认改成 true。
    #[test]
    fn absorb_and_suspended_absorption_are_both_off_by_default() {
        assert!(
            !Config::default().upstream_retry_absorb_enabled,
            "吸收层默认必须关：可吸收类别实际只有上游 429 的 18.2%，\
             且预算/放大两条问题未修完前不该默认开"
        );
        assert!(
            !Config::default().upstream_retry_absorb_suspended,
            "403 临时风控吸收默认必须关闭（与自愈退避冲突）"
        );
        // ⭐ 三处默认值必须同源：serde 缺字段路径与 Config::default() 不得分叉。
        let from_json: Config = serde_json::from_str("{}").expect("缺字段的 config 必须能反序列化");
        assert_eq!(
            from_json.upstream_retry_absorb_enabled,
            Config::default().upstream_retry_absorb_enabled,
            "serde default 与 Config::default() 必须一致，否则加载路径不同行为不同"
        );
    }

    /// ⭐ 推号入口**默认必须开**，且缺字段时也是开。
    ///
    /// 回退即 FAIL：把 `default_import_keys_enabled()` 改成 false，或把
    /// `#[serde(default = ...)]` 换成裸 `#[serde(default)]`（bool 裸默认是 false）。
    /// 两者任一都会让**升级即切断外部 kiro-accounting 的推号**（那个端点在本开关
    /// 之前就存在且正在被使用），属无声的破坏性变更。
    #[test]
    fn import_keys_enabled_by_default_including_absent_field() {
        assert!(
            Config::default().import_keys_enabled,
            "推号入口默认必须开：端点先于开关存在，默认关等于升级即断对接方"
        );
        let from_json: Config = serde_json::from_str("{}").expect("缺字段必须能反序列化");
        assert!(
            from_json.import_keys_enabled,
            "缺字段时必须开（裸 #[serde(default)] 对 bool 是 false，是这里的陷阱）"
        );
        // 显式 false 仍必须被尊重，否则这个开关等于不存在。
        let off: Config = serde_json::from_str(r#"{"importKeysEnabled":false}"#).unwrap();
        assert!(!off.import_keys_enabled);
    }

    /// 缺字段的旧 config.json 必须能加载，且各项落在与 `Config::default()` 相同的一侧。
    ///
    /// 回退即 FAIL：把任一 `#[serde(default...)]` 摘掉 → 反序列化直接报 missing field，
    /// 线上既有 config.json 加载失败（`CredentialsConfig::load` 那条路径是 exit(1)）。
    ///
    /// ⚠️ 注意这条**不**保证存量部署会开启吸收层：既有 config.json 里显式写了
    /// `upstreamRetryAbsorbEnabled: false` 时，serde 读到的是那个 false 而非默认值。
    /// 存量实例必须改配置值（或删键）才生效 —— 见 `upstream_retry_absorb_enabled` 的文档。
    #[test]
    fn absorb_fields_absent_from_json_fall_back_to_defaults() {
        // 只给最小必需字段，其余全靠 serde default 兜。
        let cfg: Config = serde_json::from_str("{}").expect("缺字段的 config 必须能反序列化");
        assert!(
            !cfg.upstream_retry_absorb_enabled,
            "缺字段时吸收层应按默认关"
        );
        assert!(!cfg.upstream_retry_absorb_suspended);
        assert_eq!(
            cfg.upstream_retry_absorb_budget_secs,
            default_absorb_budget_secs()
        );
        assert_eq!(
            cfg.upstream_retry_absorb_max_rounds,
            default_absorb_max_rounds()
        );

        // ⭐ 显式 true 必须压过默认值（手动开启的那条路必须仍然管用）。
        let explicit_on: Config = serde_json::from_str(r#"{"upstreamRetryAbsorbEnabled":true}"#)
            .expect("显式 true 必须能反序列化");
        assert!(
            explicit_on.upstream_retry_absorb_enabled,
            "面板/配置显式开启必须生效，否则这个功能没有任何启用途径"
        );
    }

    #[test]
    fn login_background_defaults_on() {
        // 登录页背景图开关默认开启（显示背景图）。
        let cfg = Config::default();
        assert!(cfg.login_background_enabled);
    }

    #[test]
    fn login_background_r18_defaults_off() {
        // R18 开关**默认关闭**（走 r18=0 全年龄图源，截图/演示更安全，需要再手动开）。
        let cfg = Config::default();
        assert!(!cfg.login_background_r18);
    }

    #[test]
    fn login_background_r18_missing_field_defaults_off() {
        // 旧配置文件缺 loginBackgroundR18 字段时，serde 默认回退为 false（全年龄）。
        let json = r#"{"host":"127.0.0.1","port":8080}"#;
        let cfg: Config = serde_json::from_str(json).expect("解析最小配置应成功");
        assert!(!cfg.login_background_r18);
        assert!(cfg.login_background_enabled);
    }

    #[test]
    fn login_background_r18_roundtrip() {
        // camelCase 序列化 + 反序列化保真：关闭 R18 应被正确保留。
        let mut cfg = Config::default();
        cfg.login_background_r18 = false;
        let s = serde_json::to_string(&cfg).expect("序列化应成功");
        assert!(s.contains("\"loginBackgroundR18\":false"));
        let back: Config = serde_json::from_str(&s).expect("反序列化应成功");
        assert!(!back.login_background_r18);
    }
}
