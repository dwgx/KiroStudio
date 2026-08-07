//! Admin API 类型定义

use serde::{Deserialize, Serialize};

// ============ 凭据状态 ============

/// 所有凭据状态响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialsStatusResponse {
    /// 凭据总数
    pub total: usize,
    /// 可用凭据数量（未禁用）
    pub available: usize,
    /// 当前活跃凭据 ID
    pub current_id: u64,
    /// 各凭据状态列表
    pub credentials: Vec<CredentialStatusItem>,
}

/// 单个凭据的状态信息
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialStatusItem {
    /// 凭据唯一 ID
    pub id: u64,
    /// 优先级（数字越小优先级越高）
    pub priority: u32,
    /// 凭据级 RPM 容量上限（None=继承全局）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpm_limit: Option<u32>,
    /// 凭据级「允许模型」白名单（成本安全硬门；None/空=不限制）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_models: Option<Vec<String>>,
    /// 「测试可用模型」历史结果（探测打的标签，供前端展示该号测过什么）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tested_models: Option<Vec<crate::kiro::model::credentials::TestedModel>>,
    /// 是否被禁用
    pub disabled: bool,
    /// 连续失败次数
    pub failure_count: u32,
    /// 是否为当前活跃凭据
    pub is_current: bool,
    /// Token 过期时间（RFC3339 格式）
    pub expires_at: Option<String>,
    /// 认证方式
    pub auth_method: Option<String>,
    /// 自定义 API 代挂:上游 base_url(展示用；api_key 绝不下发)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// 自定义 API 代挂:请求上限
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_limit: Option<u64>,
    /// 自定义 API 代挂:累计已发请求数
    #[serde(default)]
    pub request_count: u64,
    /// 是否有 Profile ARN
    pub has_profile_arn: bool,
    /// refreshToken 的 SHA-256 哈希（仅 OAuth 凭据，用于前端去重）
    pub refresh_token_hash: Option<String>,
    /// kiroApiKey 的 SHA-256 哈希（仅 API Key 凭据，用于前端去重）
    pub api_key_hash: Option<String>,
    /// kiroApiKey 的脱敏展示（仅 API Key 凭据，用于前端显示）
    pub masked_api_key: Option<String>,
    /// 用户邮箱（用于前端显示）
    pub email: Option<String>,
    /// 订阅等级标题（如 "Kiro Pro"）。随凭据持久化，重启后即可展示，
    /// 无需等待首次余额刷新；后台温和刷新时会顺带更新。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_title: Option<String>,
    /// API 调用成功次数
    pub success_count: u64,
    /// 生命周期累计 credit 花费（上游 meteringEvent 真实计费累加，独立于用量保留期，只增不清）。
    /// 供前端凭据卡片展示"这个号从入池至今一共花了多少 credit"。
    pub total_credits_used: f64,
    /// 最后一次 API 调用时间（RFC3339 格式）
    pub last_used_at: Option<String>,
    /// 是否配置了凭据级代理
    pub has_proxy: bool,
    /// 代理 URL（用于前端展示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    /// Token 刷新连续失败次数
    pub refresh_failure_count: u32,
    /// 禁用原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// 被禁用的时刻（RFC3339）。与 `disabled_reason` 成对下发，供面板显示"坏了多久"。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<String>,
    /// 端点名称（决定该凭据走哪套 Kiro API）——**实际生效值**，含自动路由与默认回退。
    pub endpoint: String,
    /// 该端点是否由用户**显式固定**（凭据里写了 `endpoint` 字段）。
    ///
    /// `false` 表示 [`Self::endpoint`] 是系统推断的结果（`ksk_` 号自动路由到 `cli`，
    /// 或回退 `config.defaultEndpoint`）。面板据此区分「已固定」与「自动」两种状态，
    /// 并决定「恢复自动」按钮是否可用。
    pub endpoint_pinned: bool,
    /// 该号**实际生效**的上游 region（与 `endpoint` 同款「实际值」语义）。
    ///
    /// 取 `effective_upstream_region`，即真正拼进 host 的那个值
    /// （`q.{region}.amazonaws.com` / `runtime.{region}.kiro.dev`），而**不是**
    /// 裸的 `api_region` 字段 —— 后者可能为空并回退到别处，面板显示裸字段会
    /// 让运维看不出真实去向。
    ///
    /// # 为什么必须下发
    ///
    /// `ksk_` 是按区授权的 token，打错区恒 403。此前本结构体**完全不含 region**，
    /// 面板行视图只能恒显 `—`（见 credential-row.tsx 的注释：「写端点存在、读无出口」），
    /// 于是「这个号在打哪个区」在面板上不可见 —— 探测探错了也看不出来。
    pub effective_region: Option<String>,
    /// 该 region 是否由用户/探测**显式写死**（凭据里有 `api_region`/`region`/`auth_region`）。
    ///
    /// `false` = 当前值来自 `config` 全局默认回退，即「没人真的为这个号定过区」。
    /// 面板据此把这类号标成待确认：它们正是 region 探测缺口的受害者。
    pub region_pinned: bool,
    /// 当前在途（in-flight）请求数（实时负载，用于观测均衡是否生效）
    pub inflight: u32,
    /// 最近 60 秒滚动窗口内的请求数（RPM 观测）
    pub rpm: u32,
    /// 用户自定义别名/备注（卡片展示优先于 email/#id）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 分身组标识（同一次多开的全部份共享；单开为 None）。前端按它分组呈现。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone_group: Option<String>,
    /// 组内序号（1-based，1 = 主份），展示为「分身 #2/5」
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone_seq: Option<u32>,
    /// 分身标签（这一份的用途标记）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// 是否正处于冷却中（429/限流/服务错误后短暂跳过）
    pub cooling_down: bool,
    /// 冷却剩余毫秒（cooling_down 为 true 时有效）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_remaining_ms: Option<u64>,
    /// 冷却原因（如「速率限制」「服务错误」）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_reason: Option<String>,
}

// ============ 凭据回收站 ============

/// 回收站列表响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashListResponse {
    /// 回收站条目总数
    pub total: usize,
    /// 已删除凭据列表（按删除时间倒序）
    pub trash: Vec<TrashItemResponse>,
}

/// 单个回收站条目（不含敏感明文）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashItemResponse {
    /// 凭据唯一 ID（恢复时保持不变）
    pub id: u64,
    /// 优先级
    pub priority: u32,
    /// 认证方式
    pub auth_method: Option<String>,
    /// 用户邮箱
    pub email: Option<String>,
    /// kiroApiKey 的脱敏展示（仅 API Key 凭据）
    pub masked_api_key: Option<String>,
    /// refreshToken 的 SHA-256 哈希（仅 OAuth 凭据，用于前端去重展示）
    pub refresh_token_hash: Option<String>,
    /// kiroApiKey 的 SHA-256 哈希（仅 API Key 凭据）
    pub api_key_hash: Option<String>,
    /// 端点名称
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// 删除时间（RFC3339 格式）
    pub deleted_at: String,
    /// 删除前累计成功次数
    pub success_count: u64,
    /// 删除前最后一次调用时间（RFC3339 格式）
    pub last_used_at: Option<String>,
    /// 删除前的禁用原因（`None` = 老回收站数据或手动删除未记录）。
    ///
    /// 与 `CredentialResponse.disabled_reason` 同为字符串枚举名，前端复用同一份 i18n 映射。
    /// 用户要求「认定封号必须标明原因」，而号被判死后往往紧接着被删——回收站不带原因时，
    /// 恰在最需要它的时刻（判断换号还是申诉）信息就丢了。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// 删除前被禁用的时刻（RFC3339）。用于区分「刚坏就删」与「坏了很久才删」。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<String>,
}

/// 批量删除的逐条结果。
///
/// 部分失败仍返 200（HTTP 层成功），由本结构逐条标注 —— 与 `import/keys` 的既有模式一致。
/// 删除是逐号独立的软删，没有跨号事务语义；整体回滚会让"10 选 1 个 id 不存在"连带
/// 另外 9 个都删不掉。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchDeleteItemResult {
    /// 凭据 ID
    pub id: u64,
    /// 该条是否成功
    pub ok: bool,
    /// 失败原因（成功时为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 批量删除凭据请求。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchDeleteRequest {
    /// 待删除的凭据 ID 列表。上限见 `MAX_BATCH_DELETE_IDS`。
    pub ids: Vec<u64>,
    /// 是否**强制删除**：跳过「必须先禁用」这道门。仍进回收站，可恢复。
    ///
    /// 默认 false —— 旧客户端不发该字段时必须保持原有的保守语义，
    /// 不能因为新增字段就让所有删除都变成强删。
    #[serde(default)]
    pub force: bool,
}

/// 批量删除响应。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchDeleteResponse {
    /// 成功条数
    pub deleted: usize,
    /// 失败条数
    pub failed: usize,
    /// 逐条结果（顺序与请求的 ids 一致）
    pub results: Vec<BatchDeleteItemResult>,
}

/// 批量清理「已禁用」凭据的请求（`POST /credentials/cleanup-disabled`）。
///
/// 与 [`BatchDeleteRequest`] 的区别是**候选由服务端算**：调用方不传 ids，
/// 因为「哪些号该清」的判据（代挂排除）在后端，让前端各写一份必然漂移。
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CleanupDisabledRequest {
    /// 只预览不真删。默认 false（真删）。
    ///
    /// 存在的理由：这是个**批量破坏性**操作，而候选是服务端算的 ——
    /// 调用方若不能先看一眼将要删哪些号，就只能盲点。dry-run 与真删走
    /// **同一段筛选**（见 `AdminService::cleanup_disabled_credentials`），
    /// 故预览结果与真删结果同源，不存在"预览一套、实删另一套"。
    #[serde(default)]
    pub dry_run: bool,
}

/// 批量清理里被**跳过**的一条（连同原因）。
///
/// 只列「已禁用但被刻意排除」的号；未禁用的号根本不是候选，不出现在这里。
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CleanupSkippedItem {
    /// 凭据 ID
    pub id: u64,
    /// 跳过原因的稳定标识（前端按它做 i18n）：
    /// - `custom_api`：代挂号（有独立 passthrough 路径，不是死号）
    /// - `passthrough_disabled`：禁用原因是代挂专属（`PassthroughFailed`/`PassthroughOverloaded`）
    /// - `self_healable`：禁用原因**可自愈**（`TooManyFailures`/`SuspiciousActivityAuto`/
    ///   `TooManyRefreshFailures`），号会自己回池，删它等于拿走健康号
    /// - `not_in_pool`：候选算出来之后该号已被别处删掉（竞态）。与 `custom_api` 分开，
    ///   否则文案会说"这号是代挂所以没删"而真相是"它已经不在池里了"
    /// - `over_limit`：本次超出单次上限，留给下一次调用
    pub reason: &'static str,
}

/// 批量清理「已禁用」凭据的响应。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupDisabledResponse {
    /// 本次是否为预览（原样回显请求的 `dryRun`，便于前端断言自己没点错）
    pub dry_run: bool,
    /// 池中已禁用的凭据总数 —— 取快照那一刻 `disabled == true` 的条数，**筛选与截断之前**。
    ///
    /// 恒等式：`disabled_total == candidates.len() + skipped.len()`。每个禁用号必然落进
    /// 其中一个（超上限的那批是从 candidates **搬**进 skipped 的，不改总数）。
    ///
    /// 原注释写的是「= candidates + skipped 里**非** over_limit 的条数」，与实现不符：
    /// over_limit 那批同样是禁用号，本来就该计入。前端拿它当"池里有多少禁用号"的分母
    /// （配 `candidates.len()` 显示"本次能清几个"），按旧注释算会在触发上限时少算一批。
    pub disabled_total: usize,
    /// 本次判定「该清」的 id（dry-run 时这就是**将要**删的那批）
    pub candidates: Vec<u64>,
    /// 被跳过的条目及原因
    pub skipped: Vec<CleanupSkippedItem>,
    /// 实际删除成功条数（dry-run 恒 0）
    pub deleted: usize,
    /// 实际删除失败条数（dry-run 恒 0）
    pub failed: usize,
    /// 逐条删除结果（dry-run 为空数组）。复用批量删除的逐条形状。
    pub results: Vec<BatchDeleteItemResult>,
}

// ============ 操作请求 ============
/// 启用/禁用凭据请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetDisabledRequest {
    /// 是否禁用
    pub disabled: bool,
}

/// 修改优先级请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetPriorityRequest {
    /// 新优先级值
    pub priority: u32,
}

/// 设置凭据级 RPM 容量上限的请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetRpmLimitRequest {
    /// 新 RPM 容量（0/null = 继承全局）
    pub rpm_limit: Option<u32>,
}

/// 设置凭据级端点（走哪套 Kiro 协议）的请求。
///
/// `endpoint = None`（或空串）→ 清除显式配置，回到**自动路由**：`ksk_` API Key 号自动走
/// `cli`，其余凭据回退 `config.defaultEndpoint`。传具体名字则强制固定，用于上游协议变化时
/// 不改代码救急。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetEndpointRequest {
    /// 端点名（如 `"ide"` / `"cli"`）。null/空串 = 清除，回到自动路由。
    #[serde(default)]
    pub endpoint: Option<String>,
}

/// 修改凭据 `apiRegion` 的请求。
///
/// 存在的理由：`ksk_` token 按 region 授权，打错区恒 403 且永不自愈，而此前
/// 全仓没有任何修改它的入口（`/regions` / `/switch-region` 都是 ARN 门控）⇒
/// api_key 号 region 错了只能删号重建。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetApiRegionRequest {
    /// 目标 region（须在白名单内，实测只有 `us-east-1` / `eu-central-1` 真实存在）。
    /// null/空串 = 清除，回退全局 `config.region`。
    #[serde(default)]
    pub api_region: Option<String>,
}

/// 修改自定义 API(代挂透传)凭据配置的请求。字段均可选:None=不改。
/// 仅对 custom_api 凭据有效(后端 gate 拒绝非 custom_api 号)。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetCustomApiConfigRequest {
    /// 上游地址(base_url)。None=不改;非空=更新;空串=后端拒绝(必填)。
    #[serde(default)]
    pub base_url: Option<String>,
    /// 上游密钥(api_key)。None=不改;空串=清除;非空=更新。
    #[serde(default)]
    pub api_key: Option<String>,
    /// 请求上限。None=不改;0=不限;>0=更新。
    #[serde(default)]
    pub request_limit: Option<u64>,
    /// 是否归零调用次数(换上游/换 key 时前端可勾选,避免旧计数残留触顶)。
    #[serde(default)]
    pub reset_count: bool,
}

/// 设置凭据级「允许模型」白名单的请求（成本安全硬门；空/null = 不限制）
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetAllowedModelsRequest {
    /// 允许的 kiro modelId 列表（如 `["deepseek-3.2","glm-5"]`）。空/null = 不限制。
    #[serde(default)]
    pub allowed_models: Option<Vec<String>>,
}

/// 添加凭据请求
///
/// `Default` 供批量导入等内部构造路径用 `..Default::default()` 只填关心的字段
/// （注意：`Default` 的 `auth_method` 是空串，内部构造必须显式赋值）。
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddCredentialRequest {
    pub access_token: Option<String>,

    /// 刷新令牌（OAuth 凭据必填，API Key 凭据不需要）
    pub refresh_token: Option<String>,

    /// 认证方式（可选，默认 social）
    #[serde(default = "default_auth_method")]
    pub auth_method: String,

    /// OIDC Client ID（IdC 认证需要）
    pub client_id: Option<String>,

    /// OIDC Client Secret（IdC 认证需要）
    pub client_secret: Option<String>,

    pub token_endpoint: Option<String>,

    pub issuer_url: Option<String>,

    pub scopes: Option<String>,

    pub profile_arn: Option<String>,

    pub expires_at: Option<String>,

    /// 优先级（可选，默认 0）
    #[serde(default)]
    pub priority: u32,

    /// 凭据级 RPM 容量上限（可选，None/0=继承全局）
    #[serde(default)]
    pub rpm_limit: Option<u32>,

    // ==== 自定义 API 代挂透传（authMethod=custom_api 时前端填入）====
    /// 自定义 API 上游基址（Anthropic 兼容中转站，透传目标）
    #[serde(default)]
    pub base_url: Option<String>,
    /// 自定义 API 密钥（透传时替换成它）
    #[serde(default)]
    pub api_key: Option<String>,
    /// 请求上限（累计达到后自动禁用，None/0=不限）
    #[serde(default)]
    pub request_limit: Option<u64>,
    /// 该代挂号是否**无条件抢在所有 Kiro 号之前**。
    /// None = 跟随全局 `config.customApiFirst`（默认 false = 与 Kiro 号按 priority 公平比较）。
    #[serde(default)]
    pub custom_api_first: Option<bool>,

    /// 凭据级 Region 配置（用于 OIDC token 刷新）
    /// 未配置时回退到 config.json 的全局 region
    pub region: Option<String>,

    /// 凭据级 Auth Region（用于 Token 刷新）
    pub auth_region: Option<String>,

    /// 凭据级 API Region（用于 API 请求）
    pub api_region: Option<String>,

    /// 凭据级 Machine ID（可选，64 位字符串）
    /// 未配置时回退到 config.json 的 machineId
    pub machine_id: Option<String>,

    /// 用户邮箱（可选，用于前端显示）
    pub email: Option<String>,

    /// 用户自定义别名/备注（可选，卡片展示优先于 email/#id）
    #[serde(default)]
    pub name: Option<String>,

    /// 分身标签（可选）。多开时会复制到**每一份**上，之后可在分身管理页逐份改。
    #[serde(default)]
    pub tag: Option<String>,

    /// 入池时是否直接置为禁用态（默认 false = 启用，与旧行为一致）。
    ///
    /// 存在的理由：重新导入一个**已知被上游封禁**的号时，必须能让它以禁用态入池。
    /// 否则它会被立刻投入调度、换回一个 403 TEMPORARILY_SUSPENDED，反而加深上游
    /// 对该批号的风控判定。`credentials.json` 本就有 `disabled` 字段，这里是把它
    /// 在 API 添加路径上打通（此前被 `add_credential` 无条件丢弃）。
    #[serde(default)]
    pub disabled: bool,

    /// 「多开」份数：把同一个账号导入 N 份，每份自动获得**独立 machineId**。
    ///
    /// **字段缺失** = 普通上号，行为与此字段不存在时完全一致（含"凭据已存在"去重保护）。
    ///
    /// **显式给值** = 多开意图，**全部份都绕过去重**。这样「已导过的号再加 N 个分身」
    /// 才走得通 —— 若第 1 份仍去重，它会撞 `凭据已存在（kiroApiKey 重复）` 让整个请求失败，
    /// 一个分身也建不出来。绕过在此安全，因为误双击不会带上这个字段
    /// （前端只在份数 >1 时下发）。
    ///
    /// 值被 clamp 到 `[1, MAX_CREDENTIAL_COPIES]`（`0` 与超限都不报错）。
    ///
    /// # 用途与注意
    ///
    /// 每份独立 machineId + 各自配代理 → 上游看到的是「同一用户的多台设备」，
    /// 用来试探能否提高并发。但 `rpm_limit` 是**每凭据**的，N 份使网关侧放行量变 N 倍，
    /// 而这 N 份**共用同一个上游账号配额**。若上游按账号限流，多开只会更早撞惩罚窗口。
    /// 故导入后应按「账号实测上限 ÷ 份数」逐号调 `rpmLimit`（面板卡片里可设，0 = 继承全局）。
    #[serde(default)]
    pub copies: Option<u32>,

    /// 本次多开要用的**节点池 id 列表**（可选，按顺序分给各份）。
    ///
    /// 缺失 / 空数组 = 自动分配（从池里按插入顺序取全部启用节点）。
    /// 给了就**只用这些**：第 1 个给第 1 份、第 2 个给第 2 份，以此类推；
    /// 数量少于份数时剩下的份直连（**刻意不复用**已用过的节点——复用等于共用出口 IP）；
    /// 多于份数时忽略多余的。
    ///
    /// 不存在 / 已禁用的 id 被**跳过并在响应文案里点名**，绝不静默替换成别的节点：
    /// 静默替换会让用户以为分到了他选的那个出口，而实际出口是另一个。
    ///
    /// 与 `proxy_url` 的优先级：本份已显式给了 `proxy_url` 时**完全不介入**
    /// （那是明确意图），此时 `node_ids` 只用于把无效 id 报出来。
    #[serde(default)]
    pub node_ids: Option<Vec<u64>>,

    /// **主份**（本次新建的第 1 份）要不要也从节点池取一个出口。
    ///
    /// `None`（字段缺失）在本请求体上等价于 `false`：`POST /credentials` 的主份是
    /// 用户**亲手提交的那一条**，它的出口由同一个表单里的「出口 IP」决定
    /// （直连 / 从池中选 / 手填），池分配不该越过用户的选择去改它。
    /// 于是缺省时池节点全部让给第 2..N 份，`copies=N` 只需 **N-1** 个节点。
    ///
    /// `Some(true)` = 主份也参与池分配（此时需要 **N** 个节点）。前端在
    /// 「从池中选」+「自动分配」时下发它。
    ///
    /// ⚠️ 与 `proxy_url` 的优先级不变：本份已显式给了 `proxy_url` 时池完全不介入
    /// （`pool_may_assign` 那道门在前），这个开关只在"没给代理"时才有意义。
    ///
    /// ⚠️ `clone_credential`（`POST /credentials/{id}/clone`）走的是**另一个默认**：
    /// 它显式传 `Some(true)`。那条路上父号一个字节都不动，"主份"指的是本次新建的
    /// 第 1 个分身 —— 它和第 2..N 份完全同质，没有理由独独让它裸连。
    #[serde(default)]
    pub assign_primary_node: Option<bool>,

    /// 主份的出口**点名一个节点池 id**（上号对话框的「出口 IP → 从池中选」）。
    ///
    /// 为什么不复用 `node_ids[0]`：`node_ids` 的语义是「本次**只**用这些节点」，
    /// 于是 `copies=3 + nodeIds=[X]` 会让第 2/3 份一个节点都拿不到（计划只有 1 个）。
    /// 而对话框里那个下拉的意思只是"主份走 X"，第 2..N 份仍应从池里自动补 ——
    /// 两种意图挤在一个字段上必然有一方被静默牺牲。
    ///
    /// 行为：解析成该节点的 `(url, username, password)` 写进主份（**含密码** ——
    /// 这正是必须传 id 而不是让前端传 URL 的原因：节点密码从不下发前端），
    /// 并把该 id 从第 2..N 份的候选里排除（否则两份共用一个出口）。
    /// id 不存在 / 已禁用 → **400**（这是用户刚在下拉里点的东西，静默直连或静默换一个
    /// 节点都会让他以为出口是他选的那个）。
    ///
    /// 与 `proxy_url` 同时给 → `proxy_url` 优先（显式 URL 是更强的意图），
    /// 本字段被忽略。与 `assign_primary_node` 无关：给了本字段就是主份要出口。
    #[serde(default)]
    pub primary_node_id: Option<u64>,

    /// 要求「每一份都必须拿到一个独立节点」，凑不齐就**整个请求报错**（不建任何份）。
    ///
    /// `None` / `false`（缺省）= 既有行为：节点不够时多出来的份直连，
    /// 并在响应文案里如实说明。老 API 调用方与既有测试都落这一支，行为逐字不变。
    ///
    /// `Some(true)` = 严格模式：`需要的份数 > 计划里可用的节点数` 时返回 400，
    /// 一份也不建。前端在「从池中选 / 自动分配」这条路上下发它 —— 用户点的是
    /// "把这些份分散到不同出口"，凑不齐时给他一堆共用同一出口的份是**假成功**。
    ///
    /// 无论哪一支都**绝不复用节点**：复用等于共用出口，比直连更糟（直连至少看得出来）。
    #[serde(default)]
    pub require_node_per_copy: Option<bool>,

    /// 凭据级代理 URL（可选，特殊值 "direct" 表示不使用代理）
    pub proxy_url: Option<String>,

    /// 凭据级代理认证用户名（可选）
    pub proxy_username: Option<String>,

    /// 凭据级代理认证密码（可选）
    pub proxy_password: Option<String>,

    /// Kiro API Key（API Key 凭据必填，格式: ksk_xxxxxxxx）
    /// 设置后直接作为 Bearer Token 使用，无需 refreshToken
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kiro_api_key: Option<String>,

    /// 端点名称（可选，未配置时使用 config.defaultEndpoint）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

fn default_auth_method() -> String {
    "social".to_string()
}

/// 添加凭据成功响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddCredentialResponse {
    pub success: bool,
    pub message: String,
    /// 新添加的凭据 ID。
    ///
    /// 多开（`copies > 1`）时这里是**第 1 份**的 id，全部 id 见 `credential_ids`。
    /// 保持该字段语义不变是为了不破坏既有调用方（前端与 kiro-accounting 都读它）。
    pub credential_id: u64,
    /// 多开时全部新建凭据的 id（含第 1 份）。`copies == 1` 时不下发该字段。
    ///
    /// 部分失败不回滚：若第 2..N 份中某几份失败，这里只含成功的那些，
    /// `message` 会写明 `成功/请求` 份数。与 `import/keys` 的既有约定一致。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_ids: Option<Vec<u64>>,
    /// 用户邮箱（如果获取成功）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// 给**池中已有**凭据再加分身的请求（`POST /credentials/{id}/clone`）。
///
/// 只有份数一个字段：key 由服务端按 id 自己读，**绝不经前端**。
/// 分身管理页拿不到 `kiroApiKey` 原文（`CredentialStatusItem` 只有 `apiKeyHash`
/// 与掩码），所以「给已有组加分身」在没有本端点时只能让用户回加号对话框重新粘 key。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneCredentialRequest {
    /// 本次再加几份。缺失按 1 处理；上限与 `AddCredentialRequest::copies` 同一个
    /// 常量（`MAX_CREDENTIAL_COPIES`），超限 clamp 而不报错。
    ///
    /// 与 `copies` 的语义差别：这里 **1 也是有效的多开意图**（给已有号再加 1 份），
    /// 不像 `AddCredentialRequest::copies == 1` 表示"普通上号、要走去重"。
    #[serde(default)]
    pub copies: Option<u32>,
    /// 新建出来的分身要不要**立刻启用**。缺失 = `false`（建出来是**禁用**的）。
    ///
    /// # 为什么默认禁用（与 `AddCredentialRequest::disabled` 默认启用相反）
    ///
    /// 分身入池的瞬间就会被调度器选中，而它此刻**还没配好出口**：节点池不足时
    /// 多出来的份直连（与父号共用一个出口 IP，多开的意义正好被抵消），region 继承
    /// 也可能失手。实测事故（2026-08-05 02:42）：一次 `copies=5`，4 个分身
    /// `apiRegion=None` → 回退 `config.region` → `ksk_` 按区授权 → 恒 403 →
    /// **24 秒内三次失败全部被自动禁用、0% 成功**，而这 24 秒里真实用户流量正被
    /// 发往必然失败的号。默认禁用把"检查一遍再放行"变成默认动作。
    ///
    /// 这个默认值**只作用于本端点**（按 id 加分身，用户就在分身管理页上盯着），
    /// 普通上号路径（`POST /credentials`）的默认启用语义完全不动。
    #[serde(default)]
    pub enabled: Option<bool>,
    /// 本次要用的节点池 id 列表（可选）。语义与 [`AddCredentialRequest::node_ids`]
    /// 完全一致，本端点只是把它原样透给同一段共享实现。
    ///
    /// ⚠️ 必须是 `Option` + `#[serde(default)]`：省略时要能解析成 `None`，
    /// 否则老前端（只发 `{"copies":3}`）会 400。
    #[serde(default)]
    pub node_ids: Option<Vec<u64>>,

    /// 本次新建的**第 1 份**要不要也从池里取节点。语义见
    /// [`AddCredentialRequest::assign_primary_node`]，但**默认值相反**：
    /// 本端点缺省 = `true`（第 1 份也拿）。
    ///
    /// # ⚠️ 「第 1 份」= 本次**新建**的首份，**不是**已存在的父号
    ///
    /// 这个区别是一个真实缺陷的核心。本字段无论怎么设，都**不会**给父号分配节点 ——
    /// 它只决定「新建的 N 份里，第 1 份要不要也去消费一个池节点」。
    ///
    /// 于是会出现这样的形态（线上实测）：`#776` 是原始导入的号（无 `cloneGroup`、无代理），
    /// `#778–787` 是从它克隆的 10 份、各有独立 SOCKS ⇒ **11 份共用一个上游账号，
    /// 10 份走独立 IP、1 份走服务器裸 IP**。代理存在的意义（每凭据独立出口）在那一份上是空的，
    /// 而它在分身管理页上看起来和别的份一样。
    ///
    /// 处置是**只告警、不改配置**（用户拍板）：`add_credential_with_intent` 末尾按
    /// **key** 查同账号成员，发现无独立出口者就把 id 写进响应 `message`，
    /// 但绝不给它写 `proxy_url` —— 那是用户的显式配置，「直连」也可能是刻意留的对照。
    /// 判据必须按 key 而非按 `cloneGroup`：父号恰恰是那个没有组标识的。
    ///
    /// # 为什么这条路的默认是 true
    ///
    /// 本端点**从不碰父号的代理/启用状态**（只 `export_credential` 读它；组标识回填是
    /// 唯一的例外，理由见 service 层那段注释），建出来的 N 份全是新条目、
    /// `proxy_*` 全空、彼此完全同质。此时把第 1 份排除在池分配之外没有任何依据，
    /// 只会让它裸连而池里空着一个节点 —— 那正是 2026-08-05 修掉的缺陷
    /// （实测：池里 5 个全启用、`copies=4`，只有第 2/3/4 份拿到节点，主份裸连、两个节点闲置）。
    ///
    /// 于是「升级后行为不变」在这条路上=保持 true。缺省 `None` 在 service 层被解读成
    /// `true`，**不是**裸 `#[serde(default)]` 的 false —— 后者会让老前端
    /// （只发 `{"copies":3}`）静默退回那个已修掉的缺陷。
    #[serde(default)]
    pub assign_primary_node: Option<bool>,

    /// 严格模式：凑不齐「每份一个独立节点」就整个请求报错。语义与默认值均同
    /// [`AddCredentialRequest::require_node_per_copy`]（缺省 = 宽松，行为不变）。
    #[serde(default)]
    pub require_node_per_copy: Option<bool>,

    /// 建完分身后，把**被克隆的那个主份**（本请求路径上的 `{id}`）一并删掉。
    /// 缺省 / `false` = 保留主份，只追加分身（**行为不变**）。
    ///
    /// # 两种语义（用户 2026-08-05 拍板的那个勾选框）
    ///
    /// - 不勾（缺省）：主份留在池里，本次新建的 N 份是**追加**的分身。组内共 1+N 份，
    ///   其中主份没有独立出口（本端点从不碰它的 `proxy_*`）。
    /// - 勾上：先建 N 份（每份各自从节点池取出口），再把主份删掉。组内共 **N 份且彼此
    ///   完全同质** —— 没有"那一份裸连的主份"，也就没有它把整组账号关联度拉满的问题。
    ///   `cloneSeq === 1` 的那一份自然顶替主份角色（前端按 seq 升序取首个成员）。
    ///
    /// # 顺序是「先建后删」而不是用户原话里的「先删后建」
    ///
    /// 用户原话是「先给选择的凭据删掉 然后一下创建」，**实现刻意反过来**，两个理由：
    ///
    /// 1. 删了主份就读不到它了。共享实现按 key 从池中同 key 既有号继承
    ///    `apiRegion` / `authRegion` / `subscriptionTitle` / `cloneGroup`
    ///    （`find_credential_by_api_key`）—— 主份是**唯一**的继承源。先删 ⇒ 继承全空 ⇒
    ///    `apiRegion=None` ⇒ 回退 `config.region` ⇒ `ksk_` 按区授权 ⇒ 恒 403。
    ///    那正是 2026-08-05 02:42 那次「4 个分身 24 秒内全部被自动禁用、0% 成功」的成因。
    /// 2. 失败方向不同。先建后删：建失败 ⇒ 主份**一字节未动**（用户什么也没丢）；
    ///    删失败 ⇒ 分身在、主份也在，等价于"没勾"的形态，响应里点名让用户手工删。
    ///    先删后建则相反 —— 建失败就把用户唯一那份凭据弄没了（虽在回收站，但池里已空）。
    ///
    /// 终态与用户要的完全一致（N 份同质、主份不留），只有中间顺序不同。
    ///
    /// # 删除是软删
    ///
    /// 走 `delete_credential_forced(id, true)` ⇒ 进**回收站**可恢复。`force` 只跳过
    /// 「必须先禁用」那道误删护栏（主份通常正在服务中，不 force 会被挡住），
    /// **不**跳过回收站。
    #[serde(default)]
    pub replace_primary: Option<bool>,
}

// ============ 余额查询 ============

/// 余额查询响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BalanceResponse {
    /// 凭据 ID
    pub id: u64,
    /// 订阅类型
    pub subscription_title: Option<String>,
    /// 当前使用量
    pub current_usage: f64,
    /// 使用限额（base，不含 overage）
    pub usage_limit: f64,
    /// 剩余额度（overage 感知：overage 开启时含 overage cap）
    pub remaining: f64,
    /// 使用百分比（基于 effective_limit 计算）
    pub usage_percentage: f64,
    /// 下次重置时间（Unix 时间戳）
    pub next_reset_at: Option<f64>,
    /// 是否开启超额（Online Overage）。serde default 兼容旧磁盘缓存。
    #[serde(default)]
    pub overage_enabled: bool,
    /// 超额上限（overage cap）。未开启时为 0。serde default 兼容旧磁盘缓存。
    #[serde(default)]
    pub overage_cap: f64,
    /// 有效使用限额（base + overage cap）。serde default 兼容旧磁盘缓存。
    #[serde(default)]
    pub effective_limit: f64,
    /// 本次返回的是否是**过期的**上次已知值（上游超时/失败时的降级）。
    ///
    /// `false`（默认）= 新鲜值。前端据此显示"数据可能已过期"提示而不是把它当当前额度。
    /// 用 `skip_serializing_if` 使新鲜响应不带该字段，旧前端不受影响。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
    /// 本次的 usage/remaining 是否**含本地乐观推算**（真值 + 缓存后新花掉的 credit）。
    ///
    /// 余额真值由后台每 30 分钟刷新一次，若不做推算，跑完一批请求后面板上的额度
    /// 最多 30 分钟不动（用户以为没生效）。置 true 时前端可加"约"字样。
    /// 绝不为此每请求打上游 —— 那是 web_portal 探测，会加重风控。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optimistic: bool,
}

// ============ 批量缓存余额（A10）============

/// 单条已缓存余额快照（含缓存时间戳，供前端判断新鲜度）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedBalanceItem {
    /// 缓存的余额数据
    #[serde(flatten)]
    pub balance: BalanceResponse,
    /// 缓存写入时间（Unix 秒），前端据此判断新鲜度
    pub cached_at: f64,
}

/// 批量已缓存余额响应
///
/// 仅返回【已缓存】凭据的快照，只读缓存，绝不触发任何上游调用。
/// 缓存未命中的凭据不出现在 balances 中（前端可按需单独拉取）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedBalancesResponse {
    /// 已缓存的凭据数量
    pub total: usize,
    /// id -> 缓存余额快照
    pub balances: std::collections::HashMap<u64, CachedBalanceItem>,
}

// ============ 负载均衡配置 ============

/// 负载均衡模式响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadBalancingModeResponse {
    /// 当前模式（"priority" 或 "balanced"）
    pub mode: String,
}

/// 设置负载均衡模式请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetLoadBalancingModeRequest {
    /// 模式（"priority" 或 "balanced"）
    pub mode: String,
}

// ============ 通用响应 ============

/// 操作成功响应
#[derive(Debug, Serialize)]
pub struct SuccessResponse {
    pub success: bool,
    pub message: String,
}

impl SuccessResponse {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            success: true,
            message: message.into(),
        }
    }
}

/// 错误响应
#[derive(Debug, Serialize)]
pub struct AdminErrorResponse {
    pub error: AdminError,
}

#[derive(Debug, Serialize)]
pub struct AdminError {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
    /// 结构化上号诊断（归因+引导），仅诊断类错误携带；前端据此渲染诊断卡片而非裸报错。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnosis: Option<crate::kiro::diagnosis::OnboardingDiagnosis>,
}

impl AdminErrorResponse {
    pub fn new(error_type: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: AdminError {
                error_type: error_type.into(),
                message: message.into(),
                diagnosis: None,
            },
        }
    }

    /// 携带结构化诊断的错误响应：error_type=diagnosis 的 fault，message=summary，diagnosis 全量。
    pub fn diagnosed(diagnosis: crate::kiro::diagnosis::OnboardingDiagnosis) -> Self {
        Self {
            error: AdminError {
                error_type: "onboarding_diagnosis".to_string(),
                message: diagnosis.summary.clone(),
                diagnosis: Some(diagnosis),
            },
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new("invalid_request", message)
    }

    pub fn authentication_error() -> Self {
        Self::new("authentication_error", "Invalid or missing admin API key")
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }

    pub fn api_error(message: impl Into<String>) -> Self {
        Self::new("api_error", message)
    }

    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new("internal_error", message)
    }
}

// ============ 网页上号（Social OAuth）============

/// 发起网页上号请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartSocialLoginRequest {
    /// 新凭据优先级（默认 0：所有号平权，越小越优先）
    #[serde(default = "default_login_priority")]
    pub priority: u32,
    /// 可选自定义出站代理（不填继承全局）
    #[serde(default)]
    pub proxy_url: Option<String>,
}

fn default_login_priority() -> u32 {
    // 默认 0:所有号平权(见 handlers.rs default_priority 说明)。
    0
}

/// 发起网页上号响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartSocialLoginResponse {
    pub session_id: String,
    /// 供用户在浏览器打开的 Kiro 登录地址
    pub portal_url: String,
}

/// 轮询网页上号响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PollSocialLoginResponse {
    /// pending | done | error
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ============ 服务端配置快照（设置页只读展示 + 部分可改）============

/// 服务端配置快照（敏感字段脱敏后返回）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSnapshotResponse {
    /// 服务端版本（编译期注入 Cargo.toml version），供前端展示真实版本,不再硬编码。
    pub server_version: String,
    pub host: String,
    pub port: u16,
    pub region: String,
    pub kiro_version: String,
    pub system_version: String,
    pub node_version: String,
    pub tls_backend: String,
    pub load_balancing_mode: String,
    pub default_endpoint: String,
    pub endpoint_names: Vec<String>,
    pub extract_thinking: bool,
    /// Claude Code 自动切缓冲协议（识别到 CC 请求时 /v1 流式自动走 buffered，准确 input_tokens）
    pub cc_auto_buffer: bool,
    /// 批量推号入口 POST /api/import/keys 是否启用（默认开；关掉即对两个挂载点一起返 403）
    pub import_keys_enabled: bool,
    /// 分身凭据在请求未显式指定 `enabled` 时是否默认启用（默认 **关**）。
    /// 只影响 `POST /credentials/{id}/clone` 的缺省取值，显式请求值恒优先。
    pub clone_default_enabled: bool,
    /// 上游 429 吸收层：总开关 / 总预算秒 / 最大额外轮次 / 退避下限毫秒 / 退避上限秒 / 是否吸收 403 风控
    pub upstream_retry_absorb_enabled: bool,
    pub upstream_retry_absorb_budget_secs: u64,
    pub upstream_retry_absorb_max_rounds: u32,
    pub upstream_retry_absorb_min_delay_ms: u64,
    pub upstream_retry_absorb_max_delay_secs: u64,
    pub upstream_retry_absorb_suspended: bool,
    /// 是否把估算的 prompt cache 记账下发给客户端（估算值，上游不提供真值）
    pub prompt_cache_enabled: bool,
    /// 是否剥离转发给上游的 system 环境噪音（省 token / 提缓存命中 / 降关联，立即生效）
    pub strip_env_noise: bool,
    /// 工具错误缓解：泄漏控制 token 清洗 / 流式失败态对齐 / 如实暴露错误（均立即生效，默认关）
    pub tool_clean_leaked_tokens: bool,
    /// 文本化 invoke 重组(默认开):<invoke> 文本在四道门内重组成结构化 tool_use。
    pub tool_reclaim_textified_invoke: bool,
    /// stray token(call/count/card/court)复读熔断(默认开)。
    pub tool_stray_repeat_guard: bool,
    pub tool_stream_align_failure: bool,
    pub tool_expose_error_to_client: bool,
    /// JSON 修复层（根治向）：非法工具参数修成合法 JSON 再发客户端（立即生效，默认开）
    pub tool_repair_json: bool,
    /// 截断跨轮恢复：真截断且修复层补不回时置失败态让客户端重试整轮（立即生效，默认关）
    pub tool_truncation_recovery: bool,
    /// 入站工具顶层 description 字符上限（默认 10000，立即生效，0=不截断）
    pub tool_description_max_chars: usize,
    /// credentials.json / trash.json at-rest 加密开关（机器绑定密钥，立即生效，默认关）
    pub encrypt_credentials_at_rest: bool,
    pub cooldown_enabled: bool,
    /// 账户级 403 风控连续 N 次零成功后是否自动禁用该号。默认开。
    ///
    /// ⚠️ 此前该字段**只存在于 config.json**，既不在本响应里、也不在更新请求里、
    /// 也没有 TIER1 热更分支 —— 于是面板既看不到也改不了它，只能手改文件 + 重启。
    /// 线上曾出现「三个自动禁用开关被直连 API 关掉」，而这一项其实改不到，
    /// 排查时会得出错误结论。
    pub auto_disable_suspicious: bool,
    /// 全池冷却快速失败:全池都在冷却时立即返回 429+Retry-After 让客户端退避(而非网关内硬扛短等)。默认开。
    pub all_cooling_fast_fail: bool,
    pub rate_limit_enabled: bool,
    pub rate_limit_daily_max: u32,
    pub rate_limit_min_interval_ms: u64,
    pub affinity_enabled: bool,
    /// 均衡模式下是否叠加优先级分发
    pub priority_in_balanced: bool,
    // ---- 智能调度（0.7.23 headroom/背压 + 0.7.24 余额加权/429 感知，均立即生效）----
    /// 全局每号 RPM 软上限（单号 rpm_limit=0 时继承此值；此值也为 0 时用内置兜底 30）
    pub credential_rpm_limit: u32,
    /// RPM headroom 系数（整百分比 0..100，85=预留 15%）
    pub rpm_headroom_factor: u32,
    /// RPM 预留名额（headroom 折扣后再扣 N）
    pub rpm_reserve_slots: u32,
    /// 整池 RPM 饱和时是否走背压等待（默认关）
    pub rpm_hard_gate_overload_wait: bool,
    /// 冷却时长缩放百分比（10..500，100=原时长；只缩放可恢复的短冷却）
    pub cooldown_scale_pct: u32,
    /// 拟人速率：请求间隔抖动百分比（0..50）
    pub rate_limit_jitter_pct: u32,
    // ---- 入站请求整形 + RPM 自动挡 ----
    pub inbound_throttle_enabled: bool,
    pub inbound_rpm_auto: bool,
    pub inbound_target_rpm: u32,
    pub inbound_rpm_min: u32,
    pub inbound_rpm_max: u32,
    pub inbound_burst_secs: u32,
    pub inbound_queue_max_wait_secs: u32,
    /// 入站排队超时后是否放行(默认 true)而非返回 429。单号/高 RPM 不流通根治。
    pub inbound_queue_timeout_passthrough: bool,
    /// 当前实时**目标** RPM(自动挡下动态,只读展示)
    ///
    /// ⚠️ 名字里的 "current" 指「当前生效的目标值」，**不是实测吞吐**。
    /// 实测吞吐是 [`Self::inbound_observed_rpm`]。这两个字段曾经是同一个值 ——
    /// `service.rs` 把本字段接成 `inbound_target_rpm()`，于是面板"当前 RPM"恒等于
    /// "目标 RPM"，2026-08-06 实测面板显示 500 而客户端实际只有 50~70，差一个数量级，
    /// 运维据此做过两次限流分析。**要改本字段语义前先读 `throttle.rs` 的 `ObservedRate`。**
    pub inbound_current_rpm: u32,
    /// 最近 60 秒**实测**入站 RPM（客户端请求数，不含 failover 重试）。
    ///
    /// 与 [`Self::inbound_observed_upstream_rpm`] 的差值即重试放大倍数
    /// （2026-08-06 实测 4.59×）。三个数必须并排看才有意义：
    /// `target`（整形闸门）/ `observed`（客户端真实速率）/ `upstream`（逐号之和）。
    #[serde(default)]
    pub inbound_observed_rpm: u32,
    /// 逐号 RPM 之和 = 上游实际承受的尝试速率（**含 failover 重试**）。
    ///
    /// 量纲与 `inbound_observed_rpm` 不同：整形层在 failover 循环**之外**每客户端请求
    /// 取 1 个令牌，而逐号 `RpmTracker` 在**选号时**记账 ⇒ 每次 failover 尝试各记一次。
    /// 所以 `inboundTargetRpm=500` 实际允许约 500×放大倍数 的上游 RPM ——
    /// 这是"设了 500 却压不住 429"的原因，别把两者当同一个量比较。
    #[serde(default)]
    pub inbound_observed_upstream_rpm: u32,
    /// 累计放行的客户端请求数（用于对账滑窗是否在滚动；滑窗恒 0 而它在涨 = 滑窗坏了）。
    #[serde(default)]
    pub inbound_admitted_total: u64,
    /// 余额加权分流（默认开）：同档内按剩余额度微调选号，长期拉平号池余额
    pub balance_weight_enabled: bool,
    /// 余额加权 FLOOR（整百分比 0..100，50=因子下限 0.5，越小余额影响越强）
    pub balance_weight_floor: u32,
    /// 429/限速感知降权（默认开）：某号冒 429 经 EWMA 拉低健康分少被选
    pub health_429_weight_enabled: bool,
    /// 是否配置了全局代理（不回传明文）
    pub has_proxy: bool,
    pub proxy_url: Option<String>,
    /// 是否配置了 admin key（不回传明文）
    pub has_admin_key: bool,
    /// 是否配置了 userKey（下游对话 api_key，不回传明文）
    pub has_api_key: bool,
    /// 回调模式：local（本地端口）/ remote（公网回调）
    pub callback_mode: String,
    pub callback_base_url: Option<String>,
    // ---- 反代安全（批次3）----
    pub cors_allowed_origins: Vec<String>,
    pub ip_allowlist: Vec<String>,
    pub ip_blocklist: Vec<String>,
    pub machine_code_blocklist: Vec<String>,
    pub trust_forwarded_header: bool,
    pub ingress_rate_limit_per_min: u32,
    pub max_body_bytes: usize,
    // ---- 主动 token 预刷新（批次4.4）----
    pub proactive_token_refresh: bool,
    pub token_refresh_lead_minutes: i64,
    pub token_refresh_interval_secs: u64,
    // ---- Admin UI 登录页 ----
    pub login_background_enabled: bool,
    /// 登录页背景图是否走 R18 图源（立即生效）
    pub login_background_r18: bool,
    // ---- 余额同步（A6）----
    /// 后台温和余额刷新间隔（秒，0=禁用）
    pub balance_refresh_interval_secs: u64,
    // ---- 隐私 ----
    /// 是否采集下游客户端指纹（device/ip/os/browser，立即生效）
    pub collect_client_fingerprint: bool,
    /// 配置文件路径（运行时只读元数据）
    pub config_path: Option<String>,
}

/// 更新服务端配置请求
///
/// 所有字段可选：仅提交的字段被修改并持久化到 config.json。
/// 敏感字段（admin key / api key / 代理密码）不在此开放。
/// 除 `load_balancing_mode` 立即生效外，其余字段需重启进程后生效。
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateConfigRequest {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub region: Option<String>,
    pub kiro_version: Option<String>,
    pub system_version: Option<String>,
    pub node_version: Option<String>,
    /// "rustls" | "native-tls"
    pub tls_backend: Option<String>,
    /// "priority" | "balanced"（立即生效）
    pub load_balancing_mode: Option<String>,
    pub default_endpoint: Option<String>,
    pub extract_thinking: Option<bool>,
    pub cc_auto_buffer: Option<bool>,
    pub import_keys_enabled: Option<bool>,
    /// 分身默认启用（立即生效：存盘后 reload_config 换入 ArcSwap，下一次 clone 即读到）。
    pub clone_default_enabled: Option<bool>,
    /// 上游 429 吸收层六项（立即生效：存盘后 reload_config 换入 ArcSwap，下一个请求即读到）
    pub upstream_retry_absorb_enabled: Option<bool>,
    pub upstream_retry_absorb_budget_secs: Option<u64>,
    pub upstream_retry_absorb_max_rounds: Option<u32>,
    pub upstream_retry_absorb_min_delay_ms: Option<u64>,
    pub upstream_retry_absorb_max_delay_secs: Option<u64>,
    pub upstream_retry_absorb_suspended: Option<bool>,
    /// 是否把**估算的** cache_read/cache_creation 下发给客户端（详见 config 同名字段）。
    /// 关闭时字段整体缺失而非置 0——两者对客户端语义不同。
    pub prompt_cache_enabled: Option<bool>,
    pub strip_env_noise: Option<bool>,
    pub tool_clean_leaked_tokens: Option<bool>,
    pub tool_reclaim_textified_invoke: Option<bool>,
    pub tool_stray_repeat_guard: Option<bool>,
    pub tool_stream_align_failure: Option<bool>,
    pub tool_expose_error_to_client: Option<bool>,
    pub tool_repair_json: Option<bool>,
    pub tool_truncation_recovery: Option<bool>,
    pub tool_description_max_chars: Option<usize>,
    /// credentials.json / trash.json at-rest 加密开关。开启后下次 persist 把明文重写为密文(透明迁移)。
    pub encrypt_credentials_at_rest: Option<bool>,
    pub cooldown_enabled: Option<bool>,
    /// 账户级 403 风控自动禁用开关（见响应结构注释）。TIER1 热更。
    pub auto_disable_suspicious: Option<bool>,
    /// 全池冷却快速失败开关(见响应结构注释)。
    pub all_cooling_fast_fail: Option<bool>,
    pub rate_limit_enabled: Option<bool>,
    pub rate_limit_daily_max: Option<u32>,
    pub rate_limit_min_interval_ms: Option<u64>,
    pub affinity_enabled: Option<bool>,
    pub priority_in_balanced: Option<bool>,
    // ---- 智能调度（立即生效热更）----
    pub credential_rpm_limit: Option<u32>,
    pub rpm_headroom_factor: Option<u32>,
    pub rpm_reserve_slots: Option<u32>,
    pub rpm_hard_gate_overload_wait: Option<bool>,
    pub cooldown_scale_pct: Option<u32>,
    pub rate_limit_jitter_pct: Option<u32>,
    pub inbound_throttle_enabled: Option<bool>,
    pub inbound_rpm_auto: Option<bool>,
    pub inbound_target_rpm: Option<u32>,
    pub inbound_rpm_min: Option<u32>,
    pub inbound_rpm_max: Option<u32>,
    pub inbound_burst_secs: Option<u32>,
    pub inbound_queue_max_wait_secs: Option<u32>,
    pub inbound_queue_timeout_passthrough: Option<bool>,
    pub balance_weight_enabled: Option<bool>,
    pub balance_weight_floor: Option<u32>,
    pub health_429_weight_enabled: Option<bool>,
    /// 全局代理地址；传空字符串表示清除
    pub proxy_url: Option<String>,
    /// 全局代理认证用户名；出于安全前端不回显已存值，仅在非空时更新
    #[serde(default)]
    pub proxy_username: Option<String>,
    /// 全局代理认证密码；出于安全前端不回显已存值，仅在非空时更新
    #[serde(default)]
    pub proxy_password: Option<String>,
    /// 网页上号回调基地址；传空字符串表示清除（回退本地模式）
    pub callback_base_url: Option<String>,
    /// 下游客户端对话 API Key（userKey，x-api-key）。出于安全前端不回显已存值，仅在非空时更新；
    /// ⚠️需重启生效（认证中间件在启动时固化 key）。空白值会被后端拒绝（防 fail-open）。
    #[serde(default)]
    pub api_key: Option<String>,
    // ---- 反代安全（批次3，均需重启生效）----
    /// CORS 允许来源列表（整表替换）
    pub cors_allowed_origins: Option<Vec<String>>,
    /// 入口 IP 白名单（CIDR/单 IP，整表替换）
    pub ip_allowlist: Option<Vec<String>>,
    /// 入口 IP 黑名单（CIDR/单 IP，整表替换；命中即 403，优先于白名单）
    pub ip_blocklist: Option<Vec<String>>,
    /// 机器码黑名单（整表替换；命中即 403，消息 sbsbsb！）
    pub machine_code_blocklist: Option<Vec<String>>,
    /// 是否信任 X-Forwarded-For
    pub trust_forwarded_header: Option<bool>,
    /// 入口每-IP 每分钟限流（0=关闭）
    pub ingress_rate_limit_per_min: Option<u32>,
    /// 请求体最大字节数
    pub max_body_bytes: Option<usize>,
    // ---- 主动 token 预刷新（批次4.4，需重启生效）----
    pub proactive_token_refresh: Option<bool>,
    pub token_refresh_lead_minutes: Option<i64>,
    pub token_refresh_interval_secs: Option<u64>,
    // ---- Admin UI 登录页（立即生效）----
    pub login_background_enabled: Option<bool>,
    /// 登录页背景图是否走 R18 图源（立即生效）
    pub login_background_r18: Option<bool>,
    // ---- 余额同步（A6，需重启生效）----
    /// 后台温和余额刷新间隔（秒，0=禁用）
    pub balance_refresh_interval_secs: Option<u64>,
    // ---- 隐私（立即生效）----
    /// 是否采集下游客户端指纹（device/ip/os/browser）
    pub collect_client_fingerprint: Option<bool>,
}

/// 更新服务端配置响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateConfigResponse {
    pub success: bool,
    pub message: String,
    /// 是否有字段需要重启才能生效
    pub restart_required: bool,
    /// 需要重启才生效的已改字段名（前端用于提示）
    pub restart_fields: Vec<String>,
}

// ============ 存储统计 / 清理（运维）============

/// 单个数据分区的占用统计
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoragePartition {
    /// 分区键（与清理 target 一致）：traces | usage_jsonl | trash | bg_cache
    pub key: String,
    /// 展示名（中文）
    pub label: String,
    /// 占用字节数（内存分区为常驻内存字节）
    pub bytes: u64,
    /// 条目/文件数（trace 为行数，usage_jsonl 为文件数，trash 为条目数，bg_cache 为张数）
    pub items: u64,
    /// 落盘路径（内存分区为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 是否为纯内存分区（无落盘，清理即释放内存）
    pub in_memory: bool,
}

/// 存储统计响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageStatsResponse {
    /// 各分区占用明细
    pub partitions: Vec<StoragePartition>,
    /// 落盘分区字节合计（不含纯内存分区）
    pub total_disk_bytes: u64,
    /// 统计是否可用（用量统计未启用时 trace/jsonl 分区缺失）
    pub usage_enabled: bool,
}

/// 存储清理请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageCleanupRequest {
    /// 清理目标（白名单枚举）：traces | usage_jsonl | trash | bg_cache | all
    pub target: String,
    /// 保留天数：删除早于 N 天前的数据。省略时按各分区的配置默认保留期。
    #[serde(default)]
    pub older_than_days: Option<i64>,
    /// 全清标记：忽略时间维度，清空该分区的**全部**条目。
    ///
    /// 为什么需要独立于 `older_than_days`：回收站的保留天数里 `0` 已被后台任务占用为
    /// 「永久保留、永不自动清」，因此按天数的入参**无法表达「现在就全清」**——
    /// 传 0 会被当成永久保留而清 0 条，传 N 又清不掉 N 天内新删的条目。
    /// 该标记为 true 时 `older_than_days` 被忽略。不可逆，前端须二次确认。
    #[serde(default)]
    pub purge_all: bool,
}

/// 单个分区的清理结果
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageCleanupItem {
    /// 分区键
    pub key: String,
    /// 清理的条目/文件数
    pub removed: u64,
    /// 释放的字节数（不可精确统计时为 0）
    pub freed_bytes: u64,
    /// 说明（如跳过原因）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 存储清理响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageCleanupResponse {
    pub success: bool,
    pub message: String,
    /// 各分区清理明细
    pub results: Vec<StorageCleanupItem>,
}

// ============ 批量导入 Kiro API Key ============

/// `concurrencyLimit` 合法区间上界（含）。超出即 400，不做静默截断。
pub const IMPORT_CONCURRENCY_LIMIT_MAX: i64 = 999;

/// 单条待导入 Key（四种请求体归一化后的内部表示）。
#[derive(Debug, Clone)]
pub struct ImportKeyItem {
    /// Kiro API Key 明文（仅进程内传递，响应/日志一律走 [`mask_import_key`]）
    pub key: String,
    /// 固定端点名（可选）；None = 走自动路由（`ksk_` 号自动 cli）
    pub endpoint: Option<String>,
    /// 是否导入后立即置禁用态
    pub disabled: bool,
    /// 推号方声明的授权 region（`apiRegion` / `region` 任一键，可选）。
    ///
    /// # 为什么必须收下它
    ///
    /// `ksk_` token 是**按 region 授权**的：打错区上游恒 403（实测同一把 key
    /// 在 `eu-central-1` 98.9% 成功、在 `us-east-1` **100% 403**，见
    /// `kiro::region_probe` 模块文档的对照表）。
    ///
    /// 此前本结构体只有 `key`/`endpoint`/`disabled` 三个字段，推号方**明明带了**
    /// `apiRegion` 也会被静默丢弃，于是每个号都必须靠上号时的自动探测去猜——
    /// 那是一次真实上游往返，且探测本身还可能探错。收下已知值 = 直接跳过探测、
    /// 且比探测更权威（推号方知道这把 key 注册在哪）。
    ///
    /// `None` = 推号方没说 → 保持既有行为，交给 `probe_and_persist_api_region` 探。
    pub api_region: Option<String>,
}

/// 代理节点的**对外视图**：`password` 恒不外传，只给 `has_password`。
///
/// 单列一个类型而不是复用 `SocksNode` + `skip_serializing`：后者一旦有人给
/// `SocksNode` 加 `Serialize` 的新字段就可能把密码带出去，而这里是白名单式的。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SocksNodeView {
    pub id: u64,
    pub name: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// 是否设了密码（**密码本身绝不外传**）
    pub has_password: bool,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_test: Option<crate::kiro::model::socks_node::SocksNodeTest>,
    pub created_at: u64,
    /// 前端展示标签（name 优先，空则回落 url）
    pub label: String,
    /// 已绑在这个节点上的凭据数（**启发式**：按 `proxy_url` 字符串比对，
    /// 见 `MultiTokenManager::proxy_url_usage`）。
    ///
    /// 前端用它做两件事：节点下拉里显示「已挂 N 个」、以及「自动分配」按钮的排序主键
    /// （少的优先）—— 这两处必须与后端 `resolve_node_plan` 的自动分配同一个排序口径，
    /// 否则用户在下拉里看到的推荐顺序与实际分到的节点不一致。
    ///
    /// 手工填过代理的号可能因 scheme 未归一而漏算（偏低）。漏算方向是安全的：
    /// 顶多把一个已被占的节点当空闲，而那正是节点不足时的既有行为。
    #[serde(default)]
    pub bound_credentials: usize,
}

impl SocksNodeView {
    /// `bound` = 该节点 url 上已挂的凭据数（调用方从
    /// `MultiTokenManager::proxy_url_usage` 里查，那张表一次算好复用给全部节点）。
    pub fn from_node(n: &crate::kiro::model::socks_node::SocksNode, bound: usize) -> Self {
        Self {
            id: n.id,
            name: n.name.clone(),
            url: n.url.clone(),
            username: n.username.clone(),
            has_password: n.password.as_ref().is_some_and(|p| !p.is_empty()),
            enabled: n.enabled,
            last_test: n.last_test.clone(),
            created_at: n.created_at,
            label: n.display_label().to_string(),
            bound_credentials: bound,
        }
    }
}

/// 新建/更新代理节点请求。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocksNodeUpsertRequest {
    /// None = 新建；Some = 更新（id 不存在时 404，**不静默新建**）
    #[serde(default)]
    pub id: Option<u64>,
    #[serde(default)]
    pub name: Option<String>,
    /// `socks5://host:port` 等。内嵌账密会被拆出，不留在 url 里。
    pub url: String,
    #[serde(default)]
    pub username: Option<String>,
    /// ⚠️ **省略该键 = 不改密码**；`""` = 清空。
    /// 绝不能改成必填 —— 那样改个节点名就会把密码抹掉，已绑该节点的分身全部掉线。
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// 批量导入代理节点：整段粘贴节点商发的文档。
///
/// 存在的理由：节点商下发的是 `socks://base64(user:pass)@host:port#name` 分享链接，
/// 且通常混在一份含标题/分隔线/`端口: 40002`/curl 示例的文档里，同一节点还会出现两次
/// （「整段复制」区 + 「逐台明细」区）。逐条手填 5 台机 = 25 个字段，且极易把
/// base64 串当成用户名填进去（那会让认证失败长得像"节点不通"）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocksNodeBulkImportRequest {
    /// 整段文本。逐行解析，非链接行安静跳过，按 url 去重。
    pub text: String,
    /// 导入后是否直接启用。**默认 false** —— 与「生成分身时是否全部默认启用」同一原则：
    /// 新导入的节点还没测活，直接参与分配会把没验证过的出口塞给分身。
    #[serde(default)]
    pub enabled: bool,
}

/// 批量导入代理节点的结果：四个聚合计数 + 逐行明细。
///
/// # 🔴 兼容性
///
/// 四个聚合字段是**旧客户端唯一读的东西**（`added` / `skipped` / `duplicate` /
/// `overCapacity`），必须逐字保留。`items` 是新增的，旧前端会忽略它。
/// 新前端读 `items` 做逐行展示，但也要容忍它缺失（`#[serde(default)]` 在 TS 侧
/// 对应 `items?`），因为面板与二进制可能不同版本（OTA 只换二进制不换浏览器缓存）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SocksNodeBulkImportOutcome {
    /// 真正落库的条数。
    pub added: usize,
    /// 被跳过的行数 = 非链接行 + 地址被策略拒绝的行。
    /// ⚠️ 含义比字面宽（两类混在一起），精确归因看 `items`。
    pub skipped: usize,
    /// 因重复而未导入（已在池中 **或** 同一次粘贴内重复）。
    pub duplicate: usize,
    /// 因超出节点数上限而未导入。
    pub over_capacity: usize,
    /// 逐行明细。安静跳过的行（标题/分隔线/说明文字）**不在内** ——
    /// 一份节点商文档里那类行有几十条，全列出来会把真正要看的几行埋掉。
    #[serde(default)]
    pub items: Vec<SocksNodeBulkImportItem>,
}

/// 批量导入的逐行结果。
///
/// `status` / `reason` 都是**稳定字符串码**，译文由前端查 i18n ——
/// 后端返回中文会让面板的语言切换对这段文案无效（其余 Admin API 同口径）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SocksNodeBulkImportItem {
    /// 原始行号（1 起，与用户粘的文本对齐）。
    pub lineno: usize,
    /// 原文，**密码已脱敏**（这份响应会进浏览器 devtools 与反代 access log）。
    pub raw: String,
    /// `ok` | `duplicate` | `invalid` | `over_capacity`
    pub status: String,
    /// 原因码：`dup_in_paste` / `already_in_pool` / `address_rejected` /
    /// `over_capacity` / 或解析层的 `bad_port` `bad_host` `no_host_port` `ambiguous`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 解析出的 `scheme://host:port`（解析失败为 None）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// 解析出的用户名（**密码恒不外传**）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

/// 批量导入请求（归一化后的内部表示）。
#[derive(Debug, Clone)]
pub struct ImportKeysRequest {
    /// 待导入 Key 列表（至少一条，否则解析阶段即 400）
    pub items: Vec<ImportKeyItem>,
    /// 客户端声明的并发上限（0~999）。当前仅回显，见 `AdminService::import_keys` 注释。
    pub concurrency_limit: Option<u32>,
}

/// 单条导入结果（`key` 恒为脱敏形态）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportKeyResult {
    /// 该条是否导入成功
    pub ok: bool,
    /// 脱敏后的 Key（`ksk_xxxx…xxxx`），绝不含明文
    pub key: String,
    /// 失败原因（成功为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 批量导入响应（部分失败也是 HTTP 200，逐条看 `results[].ok`）。
///
/// 字段有意冗余，为的是同时满足两类调用方而无需任何一方改代码：
/// - 本仓前端与既有脚本读 `results` / `elapsedMs` / `concurrencyLimit`
/// - 外部对接方（kiro-accounting 一类）读 `success` / `items`
///
/// `items` 与 `results` 是**同一份数据的两个名字**（见 [`ImportKeysResponse::new`]），
/// 不是两次导入结果；`success` 表示「请求被成功处理」而非「每条都成功」——
/// 逐条成败一律看 `ok`，这与「部分失败仍返 200」的既有语义一致。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportKeysResponse {
    /// 请求是否被成功处理（≠ 每条都成功）。恒 true——真正的失败走 4xx/5xx，
    /// 不会走到这个结构体。为兼容以 `success` 判定的调用方而存在。
    pub success: bool,
    /// 提交条目总数
    pub total: usize,
    /// 成功导入数
    pub imported: usize,
    /// 失败数
    pub failed: usize,
    /// 原样回显请求里的 concurrencyLimit（未提交为 null；当前不生效）
    pub concurrency_limit: Option<u32>,
    /// 端到端耗时（毫秒）
    pub elapsed_ms: u64,
    /// 逐条结果（顺序与请求一致）
    pub results: Vec<ImportKeyResult>,
    /// `results` 的别名，内容完全相同。外部对接方按 `items` 读取。
    pub items: Vec<ImportKeyResult>,
}

impl ImportKeysResponse {
    /// 由逐条结果装配响应，自动算出 total/imported/failed 并同步 `items` 别名。
    ///
    /// 走这个构造器而不是手写字面量，是为了让 `results` 与 `items` 不可能不一致——
    /// 若将来有人只更新一处，编译器不会报错但调用方会看到矛盾数据。
    pub fn new(
        results: Vec<ImportKeyResult>,
        concurrency_limit: Option<u32>,
        elapsed_ms: u64,
    ) -> Self {
        let total = results.len();
        let imported = results.iter().filter(|r| r.ok).count();
        Self {
            success: true,
            total,
            imported,
            failed: total - imported,
            concurrency_limit,
            elapsed_ms,
            items: results.clone(),
            results,
        }
    }
}

/// Key 脱敏：前 8 字符 + `…` + 后 4 字符；长度不足 13 一律 `***`。
///
/// 响应体与日志只允许出现这个形态，杜绝完整 Key 落盘/回显。按 char 切分，非 ASCII 也安全。
pub fn mask_import_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 12 {
        // 太短，前后拼起来就等于原文，直接整体打码
        return "***".to_string();
    }
    let head: String = chars[..8].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// 解析批量导入请求体，兼容 4 种历史/新格式（任一命中即可，互斥优先级见实现）。
///
/// 1. 新格式：`{"items":[{"key":"ksk_x","groups":[],"endpoint":null,"disabled":false}],"concurrencyLimit":300}`
/// 2. 旧格式：`{"keys":["ksk_x", ...]}`
/// 3. 旧格式：`{"apiKey":"ksk_x"}`
/// 4. 旧格式：`{"kiroApiKey":"ksk_x"}`
///
/// 返回 `Err(msg)` 时上层一律回 400（格式错误由调用方转 `invalid_request`）。
pub fn parse_import_keys_request(body: &serde_json::Value) -> Result<ImportKeysRequest, String> {
    let obj = body
        .as_object()
        .ok_or_else(|| "请求体必须是 JSON 对象".to_string())?;

    // concurrencyLimit：可选整数，0..=999；类型不对或越界一律 400（不静默夹取，避免用户以为生效）。
    let concurrency_limit = match obj.get("concurrencyLimit") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => {
            let n = v
                .as_i64()
                .ok_or_else(|| "concurrencyLimit 必须是整数".to_string())?;
            if !(0..=IMPORT_CONCURRENCY_LIMIT_MAX).contains(&n) {
                return Err(format!(
                    "concurrencyLimit 越界：{n}（合法范围 0~{IMPORT_CONCURRENCY_LIMIT_MAX}）"
                ));
            }
            Some(n as u32)
        }
    };

    let items = if let Some(raw_items) = obj.get("items") {
        // 格式 1：items 必须是数组（对象/字符串一律 400，不做宽容猜测）。
        let arr = raw_items
            .as_array()
            .ok_or_else(|| "items 必须是数组".to_string())?;
        let mut items = Vec::with_capacity(arr.len());
        for (idx, raw) in arr.iter().enumerate() {
            let item = raw
                .as_object()
                .ok_or_else(|| format!("items[{idx}] 必须是对象"))?;
            let key = item
                .get("key")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| format!("items[{idx}].key 缺失或为空"))?;
            // groups：为兼容 kiro-accounting 的导出格式保留的占位字段。KiroStudio 没有
            // 分组概念，这里**接受但忽略**（不报错），避免旧客户端整批导入失败。
            let endpoint = item
                .get("endpoint")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let disabled = item
                .get("disabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // `apiRegion` 优先，回落 `region`：两个键在生态里都有人用
            // （kiro-accounting 导出用 `region`，本仓面板用 `apiRegion`）。
            // 不做 region 白名单校验：那是 `add_credential` 的职责，在这里报 400
            // 会让一条坏 region 把整批导入打回。非法值到下游会被忽略并回落探测。
            let api_region = item
                .get("apiRegion")
                .or_else(|| item.get("region"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            items.push(ImportKeyItem {
                key: key.to_string(),
                endpoint,
                disabled,
                api_region,
            });
        }
        items
    } else if let Some(raw_keys) = obj.get("keys") {
        // 格式 2：字符串数组
        let arr = raw_keys
            .as_array()
            .ok_or_else(|| "keys 必须是数组".to_string())?;
        let mut items = Vec::with_capacity(arr.len());
        for (idx, raw) in arr.iter().enumerate() {
            let key = raw
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| format!("keys[{idx}] 必须是非空字符串"))?;
            items.push(ImportKeyItem {
                key: key.to_string(),
                endpoint: None,
                disabled: false,
                api_region: None,
            });
        }
        items
    } else if let Some(key) = obj
        .get("apiKey")
        .or_else(|| obj.get("kiroApiKey"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        // 格式 3/4：单个 Key。顶层 `apiRegion`/`region` 同样收下（与格式 1 同口径）。
        let api_region = obj
            .get("apiRegion")
            .or_else(|| obj.get("region"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        vec![ImportKeyItem {
            key: key.to_string(),
            endpoint: None,
            disabled: false,
            api_region,
        }]
    } else {
        return Err(
            "缺少可识别的 Key 字段（支持 items[].key / keys[] / apiKey / kiroApiKey）".to_string(),
        );
    };

    if items.is_empty() {
        return Err("没有待导入的 Key".to_string());
    }

    Ok(ImportKeysRequest {
        items,
        concurrency_limit,
    })
}

/// 汇总逐条结果 → 响应体（imported/failed 计数唯一收口）。
pub fn build_import_response(
    results: Vec<ImportKeyResult>,
    concurrency_limit: Option<u32>,
    elapsed_ms: u64,
) -> ImportKeysResponse {
    // 委托给 ImportKeysResponse::new，保证 results/items 别名与计数只有一处真相源。
    ImportKeysResponse::new(results, concurrency_limit, elapsed_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_config_deserializes_login_background_r18() {
        // 前端以 camelCase 提交 loginBackgroundR18，应正确落到 snake_case 字段。
        let json = r#"{"loginBackgroundR18": false}"#;
        let req: UpdateConfigRequest = serde_json::from_str(json).expect("反序列化应成功");
        assert_eq!(req.login_background_r18, Some(false));
        // 未提交的字段应为 None（仅改动字段才更新）。
        assert_eq!(req.login_background_enabled, None);
    }

    #[test]
    fn update_config_omits_login_background_r18_when_absent() {
        // 请求体不含该字段时应为 None，不会误改。
        let req: UpdateConfigRequest = serde_json::from_str("{}").expect("空对象应成功");
        assert_eq!(req.login_background_r18, None);
    }

    // ---- 存储清理请求 ----

    /// 旧客户端不发 `purgeAll` 时必须默认 false（按天数清），不能变成"静默全清"。
    #[test]
    fn storage_cleanup_defaults_purge_all_to_false() {
        let req: StorageCleanupRequest =
            serde_json::from_str(r#"{"target":"trash"}"#).expect("最小请求体应可解析");
        assert_eq!(req.target, "trash");
        assert_eq!(req.older_than_days, None);
        assert!(
            !req.purge_all,
            "未提交 purgeAll 时必须是 false —— 默认全清会让旧客户端误删整个回收站"
        );
    }

    /// 面板「全部清空」提交 camelCase 的 purgeAll，必须落到 snake_case 字段。
    #[test]
    fn storage_cleanup_parses_purge_all_camel_case() {
        let req: StorageCleanupRequest =
            serde_json::from_str(r#"{"target":"trash","purgeAll":true}"#).expect("应可解析");
        assert!(req.purge_all, "purgeAll=true 应落到 purge_all");
    }

    // ---- 批量导入 Kiro API Key ----

    fn parse(json: &str) -> Result<ImportKeysRequest, String> {
        let v: serde_json::Value = serde_json::from_str(json).expect("测试 JSON 必须合法");
        parse_import_keys_request(&v)
    }

    /// 格式 1（新）：items[] + concurrencyLimit；groups 被接受但忽略。
    #[test]
    fn import_parses_items_format() {
        let req = parse(
            r#"{"items":[{"key":"ksk_abcdefgh1234","groups":["a"],"endpoint":null,"disabled":false}],
                "concurrencyLimit":300}"#,
        )
        .expect("items 格式应解析成功");
        assert_eq!(req.items.len(), 1);
        assert_eq!(req.items[0].key, "ksk_abcdefgh1234");
        assert_eq!(req.items[0].endpoint, None);
        assert!(!req.items[0].disabled);
        assert_eq!(req.concurrency_limit, Some(300));
    }

    /// items[] 的可选字段生效：endpoint 固定 + disabled=true。
    #[test]
    fn import_items_optional_fields_apply() {
        let req =
            parse(r#"{"items":[{"key":"ksk_abcdefgh1234","endpoint":"cli","disabled":true}]}"#)
                .expect("可选字段应解析成功");
        assert_eq!(req.items[0].endpoint.as_deref(), Some("cli"));
        assert!(req.items[0].disabled);
        // 未提交 concurrencyLimit → None（响应回显 null）
        assert_eq!(req.concurrency_limit, None);
    }

    /// ⭐ 承重：推号方带的 `apiRegion` / `region` 必须被收下，不能静默丢弃。
    ///
    /// 回退即 FAIL。`ksk_` 是按区授权的 token（打错区恒 403，实测同一把 key
    /// eu-central-1 98.9% 成功 / us-east-1 100% 403）。此前 `ImportKeyItem` 只有
    /// `key`/`endpoint`/`disabled`，推号方**明明给了** region 也会被丢 → 每个号都得
    /// 靠上号时的自动探测去猜（一次真实上游往返，且可能探错）。
    #[test]
    fn import_carries_api_region_from_pusher() {
        // apiRegion 键
        let req = parse(r#"{"items":[{"key":"ksk_abcdefgh1234","apiRegion":"us-east-1"}]}"#)
            .expect("apiRegion 应被解析");
        assert_eq!(
            req.items[0].api_region.as_deref(),
            Some("us-east-1"),
            "推号方给的 apiRegion 必须收下，否则只能靠探测猜"
        );

        // region 键（kiro-accounting 导出用这个名字）→ 同样接受
        let req = parse(r#"{"items":[{"key":"ksk_abcdefgh1234","region":"eu-central-1"}]}"#)
            .expect("region 应被解析");
        assert_eq!(req.items[0].api_region.as_deref(), Some("eu-central-1"));

        // apiRegion 优先于 region（两个都给时）
        let req = parse(
            r#"{"items":[{"key":"ksk_abcdefgh1234","apiRegion":"us-east-1","region":"eu-central-1"}]}"#,
        )
        .expect("两键同时存在应解析");
        assert_eq!(req.items[0].api_region.as_deref(), Some("us-east-1"));

        // 单 Key 格式（格式 3/4）的顶层 apiRegion 同样收下
        let req = parse(r#"{"apiKey":"ksk_abcdefgh1234","apiRegion":"us-east-1"}"#)
            .expect("单 Key 格式应解析 apiRegion");
        assert_eq!(req.items[0].api_region.as_deref(), Some("us-east-1"));

        // 没给 → None（保持既有行为，交给自动探测）
        let req = parse(r#"{"items":[{"key":"ksk_abcdefgh1234"}]}"#).expect("无 region 应解析");
        assert!(req.items[0].api_region.is_none());

        // 空串视为没给（不写进凭据，避免空 region 拼坏 host）
        let req = parse(r#"{"items":[{"key":"ksk_abcdefgh1234","apiRegion":"  "}]}"#)
            .expect("空 region 应解析");
        assert!(req.items[0].api_region.is_none());
    }

    /// 格式 2（旧）：keys 字符串数组。
    #[test]
    fn import_parses_keys_array_format() {
        let req = parse(r#"{"keys":["ksk_abcdefgh1234","ksk_zyxwvuts9876"]}"#)
            .expect("keys 格式应解析成功");
        assert_eq!(req.items.len(), 2);
        assert_eq!(req.items[1].key, "ksk_zyxwvuts9876");
        assert!(
            req.items
                .iter()
                .all(|i| i.endpoint.is_none() && !i.disabled)
        );
    }

    /// 格式 3（旧）：单个 apiKey。
    #[test]
    fn import_parses_single_api_key_format() {
        let req = parse(r#"{"apiKey":"ksk_abcdefgh1234"}"#).expect("apiKey 格式应解析成功");
        assert_eq!(req.items.len(), 1);
        assert_eq!(req.items[0].key, "ksk_abcdefgh1234");
    }

    /// 格式 4（旧）：单个 kiroApiKey。
    #[test]
    fn import_parses_single_kiro_api_key_format() {
        let req = parse(r#"{"kiroApiKey":"ksk_abcdefgh1234"}"#).expect("kiroApiKey 应解析成功");
        assert_eq!(req.items.len(), 1);
        assert_eq!(req.items[0].key, "ksk_abcdefgh1234");
    }

    /// concurrencyLimit 越界（>999 / 负数 / 非整数）一律报错 → 上层 400。
    #[test]
    fn import_rejects_out_of_range_concurrency_limit() {
        let err = parse(r#"{"keys":["ksk_abcdefgh1234"],"concurrencyLimit":1000}"#)
            .expect_err("1000 应越界");
        assert!(err.contains("越界"), "错误应说明越界: {err}");
        assert!(parse(r#"{"keys":["ksk_abcdefgh1234"],"concurrencyLimit":-1}"#).is_err());
        assert!(parse(r#"{"keys":["ksk_abcdefgh1234"],"concurrencyLimit":"300"}"#).is_err());
        // 边界内合法：0 与 999
        assert_eq!(
            parse(r#"{"keys":["ksk_abcdefgh1234"],"concurrencyLimit":0}"#)
                .unwrap()
                .concurrency_limit,
            Some(0)
        );
        assert_eq!(
            parse(r#"{"keys":["ksk_abcdefgh1234"],"concurrencyLimit":999}"#)
                .unwrap()
                .concurrency_limit,
            Some(999)
        );
    }

    /// 结构非法：items 非数组 / 缺 key / 无任何可识别字段 / 空列表 → 全部 Err（400）。
    #[test]
    fn import_rejects_malformed_bodies() {
        assert!(parse(r#"{"items":"ksk_x"}"#).is_err(), "items 非数组应拒绝");
        assert!(
            parse(r#"{"items":[{"groups":[]}]}"#).is_err(),
            "缺 key 应拒绝"
        );
        assert!(
            parse(r#"{"items":[{"key":"  "}]}"#).is_err(),
            "空白 key 应拒绝"
        );
        assert!(parse(r#"{"items":[]}"#).is_err(), "空 items 应拒绝");
        assert!(parse(r#"{"foo":1}"#).is_err(), "无可识别字段应拒绝");
        assert!(parse(r#"[]"#).is_err(), "非对象体应拒绝");
    }

    /// 脱敏：只保留前 8 + 后 4，中间必须被省略号吃掉，绝不含完整 Key。
    #[test]
    fn import_masks_key_without_leaking_plaintext() {
        let key = "ksk_1234567890abcdefFEDCBA";
        let masked = mask_import_key(key);
        assert_eq!(masked, "ksk_1234…DCBA");
        assert!(!masked.contains(key), "脱敏结果不得含完整 Key");
        assert!(!key.contains(&masked), "脱敏结果不应是原文的连续子串");
        // 短 Key（<=12 字符）整体打码，避免前后拼接等于原文
        assert_eq!(mask_import_key("ksk_12345678"), "***");
        assert_eq!(mask_import_key(""), "***");
        // 非 ASCII 也按 char 切，不会 panic
        assert_eq!(
            mask_import_key("密钥密钥密钥密钥密钥密钥密"),
            "密钥密钥密钥密钥…钥密钥密"
        );
    }

    /// 部分失败：total/imported/failed 由逐条结果汇总，失败条目带 error 且 key 脱敏。
    #[test]
    /// 外部对接方（kiro-accounting 一类）的响应契约：`success` / `items` 必须存在，
    /// 且 `items` 与 `results` 是同一份数据。
    ///
    /// 这条测试锁的是**兼容承诺**：对接方路径与字段名都固定改不了，任何一方漂移
    /// 都会让线上导入静默失败（他们按 `items` 读，读到 undefined 就当零条）。
    #[test]
    fn import_response_exposes_external_compat_fields() {
        let results = vec![
            ImportKeyResult {
                ok: true,
                key: mask_import_key("ksk_1234567890abcdef"),
                error: None,
            },
            ImportKeyResult {
                ok: false,
                key: mask_import_key("ksk_zyxwvutsrq987654"),
                error: Some("凭据已存在（kiroApiKey 重复）".to_string()),
            },
        ];
        let resp = build_import_response(results, Some(100), 42);

        assert!(
            resp.success,
            "success 恒 true——真正的失败走 4xx/5xx 不会到这里"
        );
        assert_eq!(
            resp.items.len(),
            resp.results.len(),
            "items 是 results 的别名"
        );
        for (a, b) in resp.items.iter().zip(resp.results.iter()) {
            assert_eq!(a.ok, b.ok);
            assert_eq!(a.key, b.key);
            assert_eq!(a.error, b.error);
        }

        let s = serde_json::to_string(&resp).expect("序列化应成功");
        assert!(s.contains("\"success\":true"), "缺 success 字段");
        assert!(s.contains("\"items\":["), "缺 items 字段");
        assert!(
            s.contains("\"results\":["),
            "results 必须保留，本仓前端在读它"
        );
        assert!(s.contains("\"total\":2"));
        assert!(s.contains("\"imported\":1"));
        assert!(s.contains("\"failed\":1"));
        // 无论走哪个字段名，key 都必须是脱敏的
        assert!(!s.contains("ksk_1234567890abcdef"), "响应体不得含明文 Key");
        assert!(!s.contains("ksk_zyxwvutsrq987654"), "响应体不得含明文 Key");
    }

    /// 对接方给的示例请求体必须原样可解析（含 `groups: []` 与 `endpoint: null`）。
    #[test]
    fn import_parses_external_partner_exact_payload() {
        let body = serde_json::json!({
            "items": [{
                "key": "ksk_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
                "groups": [],
                "endpoint": null
            }],
            "concurrencyLimit": 100
        });
        let req = parse_import_keys_request(&body).expect("对接方的固定请求体必须可解析");
        assert_eq!(req.items.len(), 1);
        assert_eq!(req.items[0].key, "ksk_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx");
        assert_eq!(req.items[0].endpoint, None, "endpoint: null 应视作未指定");
        assert!(!req.items[0].disabled, "未给 disabled 时默认启用");
        assert_eq!(req.concurrency_limit, Some(100));
    }

    #[test]
    fn import_response_counts_partial_failure() {
        let results = vec![
            ImportKeyResult {
                ok: true,
                key: mask_import_key("ksk_1234567890abcdef"),
                error: None,
            },
            ImportKeyResult {
                ok: false,
                key: mask_import_key("ksk_zyxwvutsrq987654"),
                error: Some("凭据已存在（kiroApiKey 重复）".to_string()),
            },
        ];
        let resp = build_import_response(results, Some(300), 123);
        assert_eq!(resp.total, 2);
        assert_eq!(resp.imported, 1);
        assert_eq!(resp.failed, 1);
        assert_eq!(resp.concurrency_limit, Some(300));
        assert_eq!(resp.elapsed_ms, 123);

        let s = serde_json::to_string(&resp).expect("序列化应成功");
        // 契约字段以 camelCase 下发
        assert!(s.contains("\"concurrencyLimit\":300"));
        assert!(s.contains("\"elapsedMs\":123"));
        // 明文 Key 绝不出现在响应里
        assert!(!s.contains("ksk_1234567890abcdef"));
        assert!(!s.contains("ksk_zyxwvutsrq987654"));
        // 成功条目省略 error 字段
        assert!(s.contains("{\"ok\":true,\"key\":\"ksk_1234…cdef\"}"));
    }

    /// 全字段占位夹具：`ConfigSnapshotResponse` 无 `Default` impl（每次都由
    /// `build_config_snapshot` 从 config 逐字段构造），而线协议契约测试只关心键名，
    /// 故用本夹具填满其余字段，被测的六项由调用方覆盖。
    fn absorb_snapshot_fixture() -> ConfigSnapshotResponse {
        ConfigSnapshotResponse {
            server_version: "x".into(),
            host: "x".into(),
            port: 0,
            region: "x".into(),
            kiro_version: "x".into(),
            system_version: "x".into(),
            node_version: "x".into(),
            tls_backend: "x".into(),
            load_balancing_mode: "x".into(),
            default_endpoint: "x".into(),
            endpoint_names: vec![],
            extract_thinking: false,
            cc_auto_buffer: false,
            import_keys_enabled: true,
            clone_default_enabled: false,
            upstream_retry_absorb_enabled: false,
            upstream_retry_absorb_budget_secs: 0,
            upstream_retry_absorb_max_rounds: 0,
            upstream_retry_absorb_min_delay_ms: 0,
            upstream_retry_absorb_max_delay_secs: 0,
            upstream_retry_absorb_suspended: false,
            prompt_cache_enabled: false,
            strip_env_noise: false,
            tool_clean_leaked_tokens: false,
            tool_reclaim_textified_invoke: false,
            tool_stray_repeat_guard: false,
            tool_stream_align_failure: false,
            tool_expose_error_to_client: false,
            tool_repair_json: false,
            tool_truncation_recovery: false,
            tool_description_max_chars: 0,
            encrypt_credentials_at_rest: false,
            cooldown_enabled: false,
            auto_disable_suspicious: false,
            all_cooling_fast_fail: false,
            rate_limit_enabled: false,
            rate_limit_daily_max: 0,
            rate_limit_min_interval_ms: 0,
            affinity_enabled: false,
            priority_in_balanced: false,
            credential_rpm_limit: 0,
            rpm_headroom_factor: 0,
            rpm_reserve_slots: 0,
            rpm_hard_gate_overload_wait: false,
            cooldown_scale_pct: 0,
            rate_limit_jitter_pct: 0,
            inbound_throttle_enabled: false,
            inbound_rpm_auto: false,
            inbound_target_rpm: 0,
            inbound_rpm_min: 0,
            inbound_rpm_max: 0,
            inbound_burst_secs: 0,
            inbound_queue_max_wait_secs: 0,
            inbound_queue_timeout_passthrough: false,
            inbound_current_rpm: 0,
            inbound_observed_rpm: 0,
            inbound_observed_upstream_rpm: 0,
            inbound_admitted_total: 0,
            balance_weight_enabled: false,
            balance_weight_floor: 0,
            health_429_weight_enabled: false,
            has_proxy: false,
            proxy_url: None,
            has_admin_key: false,
            has_api_key: false,
            callback_mode: "x".into(),
            callback_base_url: None,
            cors_allowed_origins: vec![],
            ip_allowlist: vec![],
            ip_blocklist: vec![],
            machine_code_blocklist: vec![],
            trust_forwarded_header: false,
            ingress_rate_limit_per_min: 0,
            max_body_bytes: 0,
            proactive_token_refresh: false,
            token_refresh_lead_minutes: 0,
            token_refresh_interval_secs: 0,
            login_background_enabled: false,
            login_background_r18: false,
            balance_refresh_interval_secs: 0,
            collect_client_fingerprint: false,
            config_path: None,
        }
    }

    /// ⭐ 线协议契约：吸收层六项必须以**精确的 camelCase 名**上下行。
    ///
    /// 回退即 FAIL：改任一字段名（或去掉 `rename_all = "camelCase"`），断言失败。
    ///
    /// 为什么值得单列一条：前端按这些字符串读写（`admin-ui/src/types/api.ts` /
    /// `settings-page.tsx`）。名字对不上时**没有任何编译错误、也没有运行时报错** ——
    /// 快照里那个键就是 `undefined`，开关渲染成"关"、用户点开保存后后端收到的是 `None`
    /// 从而什么也不改。表现为"面板上这个开关点了没反应"，是最难排的一类问题。
    #[test]
    fn absorb_fields_use_exact_camel_case_on_the_wire() {
        let snap = ConfigSnapshotResponse {
            server_version: "0".into(),
            host: "127.0.0.1".into(),
            port: 8080,
            region: "us-east-1".into(),
            kiro_version: "0".into(),
            system_version: "s".into(),
            node_version: "n".into(),
            tls_backend: "rustls".into(),
            load_balancing_mode: "priority".into(),
            default_endpoint: "ide".into(),
            endpoint_names: vec![],
            upstream_retry_absorb_enabled: true,
            upstream_retry_absorb_budget_secs: 45,
            upstream_retry_absorb_max_rounds: 3,
            upstream_retry_absorb_min_delay_ms: 150,
            upstream_retry_absorb_max_delay_secs: 15,
            upstream_retry_absorb_suspended: false,
            ..absorb_snapshot_fixture()
        };
        let s = serde_json::to_string(&snap).expect("序列化应成功");
        for (key, val) in [
            ("upstreamRetryAbsorbEnabled", "true"),
            ("upstreamRetryAbsorbBudgetSecs", "45"),
            ("upstreamRetryAbsorbMaxRounds", "3"),
            ("upstreamRetryAbsorbMinDelayMs", "150"),
            ("upstreamRetryAbsorbMaxDelaySecs", "15"),
            ("upstreamRetryAbsorbSuspended", "false"),
        ] {
            let expect = format!("\"{key}\":{val}");
            assert!(
                s.contains(expect.as_str()),
                "快照必须含 {expect}；前端按这个字符串读，名字不符时那个键是 undefined、\
                 开关永远显示关且改不动，且无任何编译/运行时报错。实际: {s}"
            );
        }

        // 入向同理：前端提交的 camelCase 必须能反序列化进对应字段。
        let req: UpdateConfigRequest = serde_json::from_str(
            r#"{"upstreamRetryAbsorbEnabled":true,"upstreamRetryAbsorbBudgetSecs":30,
                "upstreamRetryAbsorbMaxRounds":2,"upstreamRetryAbsorbMinDelayMs":200,
                "upstreamRetryAbsorbMaxDelaySecs":9,"upstreamRetryAbsorbSuspended":true}"#,
        )
        .expect("前端 camelCase 请求体必须能反序列化");
        assert_eq!(req.upstream_retry_absorb_enabled, Some(true));
        assert_eq!(req.upstream_retry_absorb_budget_secs, Some(30));
        assert_eq!(req.upstream_retry_absorb_max_rounds, Some(2));
        assert_eq!(req.upstream_retry_absorb_min_delay_ms, Some(200));
        assert_eq!(req.upstream_retry_absorb_max_delay_secs, Some(9));
        assert_eq!(req.upstream_retry_absorb_suspended, Some(true));

        // 缺字段（旧前端 / 只改别的设置）必须全 None，绝不能被当成"要改成 false/0"。
        let empty: UpdateConfigRequest =
            serde_json::from_str("{}").expect("空请求体必须能反序列化");
        assert_eq!(empty.upstream_retry_absorb_enabled, None);
        assert_eq!(empty.upstream_retry_absorb_budget_secs, None);
        assert_eq!(empty.upstream_retry_absorb_suspended, None);
    }

    /// ⭐ 分身三字段的线上契约：`CredentialStatusItem` 的
    /// `clone_group` / `clone_seq` / `tag` 必须带 `skip_serializing_if`。
    ///
    /// 用源码断言而非构造实例：该结构体有 39 个字段且不派生 `Default`，
    /// 手搓 fixture 会在别的会话加字段时立刻编译失败（与被测契约无关的脆性）。
    ///
    /// 回退即 FAIL：删掉任一 `skip_serializing_if` → **每个**单开号的响应里
    /// 都会多出 `"cloneGroup":null,"cloneSeq":null,"tag":null`。
    /// 前端 `groupClones` 用 `if (!it.cloneGroup) continue` 过滤，功能上恰好仍对，
    /// 但凭据列表是面板轮询最频的端点（实测 24h 2747 次），白涨 3 字段 × 凭据数。
    ///
    /// camelCase 本身由结构体级 `rename_all` 保证，已被同结构体的其它测试覆盖。
    #[test]
    fn clone_identity_fields_are_omitted_when_absent() {
        let src = include_str!("types.rs");
        let decl = src
            .split_once("pub struct CredentialStatusItem {")
            .expect("CredentialStatusItem 不应被改名")
            .1;
        let body = decl.split_once("\n}").expect("结构体应有结尾").0;

        // ⚠️ 必须**逐字段按行**判定，不能取「字段前 N 字节窗口」：三个字段是背靠背
        // 声明的，200 字节窗口会跨进**上一个**字段的属性行，于是最后一个字段（tag）
        // 少了属性也照样通过 —— 本测试第一版正是这个形态（对 clone_group/clone_seq
        // 有效、对 tag 无效的半失效守卫）。
        let lines: Vec<&str> = body.lines().collect();
        for field in ["clone_group", "clone_seq", "tag"] {
            let needle = format!("pub {field}:");
            let idx = lines
                .iter()
                .position(|l| l.trim_start().starts_with(needle.as_str()))
                .unwrap_or_else(|| panic!("{field} 应在 CredentialStatusItem 内"));
            // 只看**紧邻上一行**：serde 属性必须直接挂在字段上。
            let prev = lines[idx.saturating_sub(1)].trim();
            assert!(
                prev.contains("skip_serializing_if"),
                "{field} 的紧邻上一行必须是 skip_serializing_if 属性，实际是: {prev}。\
                 否则每个单开号的响应里都白发一个 null 字段，而凭据列表是面板轮询最频的端点"
            );
        }

        // 结构体级 camelCase 必须在（前端读 cloneGroup，snake_case 会让分组恒空）。
        let head = src
            .split_once("pub struct CredentialStatusItem {")
            .unwrap()
            .0;
        assert!(
            head.rsplit("/// 单个凭据的状态信息")
                .next()
                .unwrap()
                .contains("rename_all"),
            "CredentialStatusItem 必须带 rename_all=camelCase"
        );
    }

    #[test]
    fn config_snapshot_serializes_login_background_r18() {
        // 快照以 camelCase 下发，前端据此渲染开关初值。
        let snap = ConfigSnapshotResponse {
            server_version: "0.0.0".into(),
            host: "127.0.0.1".into(),
            port: 8080,
            region: "us-east-1".into(),
            kiro_version: "0.0.0".into(),
            system_version: "sys".into(),
            node_version: "node".into(),
            tls_backend: "rustls".into(),
            load_balancing_mode: "priority".into(),
            default_endpoint: "ide".into(),
            endpoint_names: vec![],
            extract_thinking: true,
            cc_auto_buffer: true,
            import_keys_enabled: true,
            clone_default_enabled: false,
            // 吸收层六项：本处是**测试夹具**（不是 Default impl，本类型没有 Default），
            // 取值与 config 默认一致只为可读性。真正防漂移的是 service.rs 里
            // build_config_snapshot 必须逐字段从 config 读，由
            // `absorb_snapshot_maps_every_field_from_config` 钉死。
            upstream_retry_absorb_enabled: false,
            upstream_retry_absorb_budget_secs: 45,
            upstream_retry_absorb_max_rounds: 3,
            upstream_retry_absorb_min_delay_ms: 150,
            upstream_retry_absorb_max_delay_secs: 15,
            upstream_retry_absorb_suspended: false,
            prompt_cache_enabled: true,
            cooldown_enabled: true,
            auto_disable_suspicious: true,
            all_cooling_fast_fail: true,
            rate_limit_enabled: false,
            rate_limit_daily_max: 500,
            rate_limit_min_interval_ms: 1000,
            affinity_enabled: true,
            priority_in_balanced: false,
            credential_rpm_limit: 0,
            rpm_headroom_factor: 85,
            rpm_reserve_slots: 0,
            rpm_hard_gate_overload_wait: false,
            cooldown_scale_pct: 100,
            rate_limit_jitter_pct: 20,
            inbound_throttle_enabled: true,
            inbound_rpm_auto: true,
            inbound_target_rpm: 100,
            inbound_rpm_min: 20,
            inbound_rpm_max: 300,
            inbound_burst_secs: 2,
            inbound_queue_max_wait_secs: 30,
            inbound_queue_timeout_passthrough: true,
            inbound_current_rpm: 100,
            // 实测类字段的"默认值"刻意为 0：它们只能来自真实观测，任何非零默认都是
            // 凭空造数 —— 那正是本次要修的缺陷（把配置值当实测值展示）。
            inbound_observed_rpm: 0,
            inbound_observed_upstream_rpm: 0,
            inbound_admitted_total: 0,
            balance_weight_enabled: true,
            balance_weight_floor: 50,
            health_429_weight_enabled: true,
            has_proxy: false,
            proxy_url: None,
            has_admin_key: false,
            has_api_key: false,
            callback_mode: "local".into(),
            callback_base_url: None,
            cors_allowed_origins: vec![],
            ip_allowlist: vec![],
            ip_blocklist: vec![],
            machine_code_blocklist: vec![],
            trust_forwarded_header: false,
            ingress_rate_limit_per_min: 0,
            max_body_bytes: 0,
            proactive_token_refresh: true,
            token_refresh_lead_minutes: 10,
            token_refresh_interval_secs: 60,
            login_background_enabled: true,
            login_background_r18: false,
            balance_refresh_interval_secs: 1800,
            collect_client_fingerprint: true,
            strip_env_noise: true,
            tool_clean_leaked_tokens: true,
            tool_reclaim_textified_invoke: true,
            tool_stray_repeat_guard: true,
            tool_stream_align_failure: true,
            tool_expose_error_to_client: true,
            tool_repair_json: true,
            tool_truncation_recovery: false,
            tool_description_max_chars: 10000,
            encrypt_credentials_at_rest: false,
            config_path: None,
        };
        let s = serde_json::to_string(&snap).expect("序列化应成功");
        assert!(s.contains("\"loginBackgroundR18\":false"));
        assert!(s.contains("\"loginBackgroundEnabled\":true"));
    }
}
