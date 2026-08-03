use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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

    /// 默认端点名称（凭据未显式指定 endpoint 时使用，默认 "ide"）
    #[serde(default = "default_endpoint")]
    pub default_endpoint: String,

    /// 端点特定的配置
    ///
    /// 键为端点名（如 "ide" / "cli"），值为该端点自由定义的参数对象。
    /// 未在此表出现的端点沿用实现内置默认值。
    #[serde(default)]
    pub endpoints: HashMap<String, serde_json::Value>,

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

    /// 配置文件路径（运行时元数据，不写入 JSON）
    #[serde(skip)]
    config_path: Option<PathBuf>,
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
            default_endpoint: default_endpoint(),
            endpoints: HashMap::new(),
            cooldown_enabled: default_cooldown_enabled(),
            cooldown_scale_pct: default_cooldown_scale_pct(),
            rate_limit_jitter_pct: default_rate_limit_jitter_pct(),
            inbound_throttle_enabled: default_true(),
            inbound_rpm_auto: default_true(),
            inbound_target_rpm: default_inbound_target_rpm(),
            inbound_rpm_min: default_inbound_rpm_min(),
            inbound_rpm_max: default_inbound_rpm_max(),
            inbound_burst_secs: default_inbound_burst_secs(),
            inbound_queue_max_wait_secs: default_inbound_queue_max_wait_secs(),
            inbound_queue_timeout_passthrough: default_inbound_queue_timeout_passthrough(),
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
            compression: CompressionConfig::default(),
            overload_fallback_model: None,
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
        Ok(config)
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
