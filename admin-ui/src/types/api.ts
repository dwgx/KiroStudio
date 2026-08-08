// 凭据状态响应
export interface CredentialsStatusResponse {
  total: number
  available: number
  currentId: number
  credentials: CredentialStatusItem[]
}

// 单个凭据状态
export interface TestedModel {
  model: string
  status: 'supported' | 'unsupported' | 'unknown'
  testedAt: string
}
export interface CredentialStatusItem {
  id: number
  priority: number
  rpmLimit?: number
  /** 自定义 API 代挂:上游 base_url（api_key 不下发） */
  baseUrl?: string
  /** 自定义 API 代挂:请求上限 */
  requestLimit?: number
  /** 自定义 API 代挂:累计已发请求数 */
  requestCount?: number
  /** 自定义 API 代挂:deepseek 协议归一化开关 */
  deepseekNormalize?: boolean
  /** 「允许模型」白名单（成本安全硬门；空/缺省 = 不限制） */
  allowedModels?: string[]
  /** 「测试可用模型」历史结果（探测打的标签） */
  testedModels?: TestedModel[]
  disabled: boolean
  failureCount: number
  isCurrent: boolean
  expiresAt: string | null
  authMethod: string | null
  hasProfileArn: boolean
  email?: string
  /** 订阅等级标题（如 "Kiro Pro"）。随凭据持久化，重启后即可展示，无需等首次余额刷新。 */
  subscriptionTitle?: string | null
  refreshTokenHash?: string
  apiKeyHash?: string
  maskedApiKey?: string
  successCount: number
  /** 生命周期累计 credit 花费（真实计费累加，独立于用量保留期，只增不清）。 */
  totalCreditsUsed: number
  lastUsedAt: string | null
  hasProxy: boolean
  proxyUrl?: string
  refreshFailureCount: number
  disabledReason?: string
  /** 被禁用的时刻（RFC3339）。与 disabledReason 成对下发，用于展示"这号坏了多久"。 */
  disabledAt?: string
  /** **实际生效**的端点名（含自动路由与默认回退），如 "ide" / "cli"。 */
  endpoint: string
  /** 该端点是否被用户显式固定。false = 系统自动推断（ksk_ 号自动走 cli，其余回退全局默认）。 */
  endpointPinned?: boolean
  /**
   * **实际生效**的上游 region（真正拼进 host 的值，如 "us-east-1"）。
   *
   * ksk_ 是按区授权的 token，打错区上游恒 403。此前后端完全不下发 region，
   * 行视图只能恒显 `—`，探测探错了在面板上看不出来。
   */
  effectiveRegion?: string
  /** 该 region 是否被显式写死。false = 现值只是 config 全局回退，没人为这个号定过区。 */
  regionPinned?: boolean
  /** 当前在途（in-flight）请求数：实时负载，用于观测负载是否均衡分摊。 */
  inflight?: number
  /** 最近 60 秒滚动窗口内的请求数（RPM 观测）。 */
  rpm?: number
  /** Overage（超额）开关：KIRO Pro+ 开启后可突破 base 额度（付费）。
      后端 BE-overage 批次落地前为只读展示；字段缺省视为未知/关闭。 */
  overageEnabled?: boolean
  /** 用户自定义别名/备注（卡片展示优先于 email/#id）。 */
  name?: string
  /** 分身组标识：同一次多开的全部份共享（含主份）。单开凭据无此字段。
      前端按它分组呈现；后端用随机 UUID 而非父号 id/key 哈希，
      故 id 复用与 key 轮换都不会让分组错乱。 */
  cloneGroup?: string
  /** 组内序号（1-based，1 = 主份），展示为「分身 #2/5」。 */
  cloneSeq?: number
  /** 分身标签：这一份的用途标记（与 name 是账号别名不同）。 */
  tag?: string
  /** 是否正处于冷却中（429/限流/服务错误后短暂跳过调度）。 */
  coolingDown?: boolean
  /** 冷却剩余毫秒（coolingDown 为 true 时有效）。 */
  cooldownRemainingMs?: number
  /** 冷却原因（如「速率限制」「服务错误」）。 */
  cooldownReason?: string
}

// 回收站条目（不含敏感明文）
export interface TrashItem {
  id: number
  priority: number
  authMethod: string | null
  email: string | null
  maskedApiKey?: string | null
  refreshTokenHash?: string | null
  apiKeyHash?: string | null
  endpoint?: string
  /** 删除时间（RFC3339） */
  deletedAt: string
  /** 删除前累计成功次数 */
  successCount: number
  /** 删除前最后调用时间 */
  lastUsedAt: string | null
  /**
   * 删除前的禁用原因（枚举名，与 Credential.disabledReason 同口径，复用 disabledReasonLabel）。
   * 老回收站数据为 undefined —— 号被判死后往往紧接着被删，这里是唯一还能看到死因的地方。
   */
  disabledReason?: string
  /** 删除前被禁用的时刻（RFC3339）。区分「刚坏就删」与「坏了很久才删」。 */
  disabledAt?: string
}

/** 批量删除的逐条结果（部分失败仍返 200，须逐条检查 ok）。 */
export interface BatchDeleteItemResult {
  id: number
  ok: boolean
  error?: string
}

export interface BatchDeleteResponse {
  deleted: number
  failed: number
  results: BatchDeleteItemResult[]
}

export interface TrashListResponse {
  total: number
  trash: TrashItem[]
}

// 余额响应
export interface BalanceResponse {
  id: number
  subscriptionTitle: string | null
  currentUsage: number
  usageLimit: number
  remaining: number
  usagePercentage: number
  nextResetAt: number | null
  /** 是否开启超额（Online Overage）。缺省视为未知/关闭。 */
  overageEnabled?: boolean
  /** 超额上限（overage cap）。未开启时为 0。 */
  overageCap?: number
  /** 有效使用限额（base + overage cap）。 */
  effectiveLimit?: number
  /** 是否是过期的上次已知值（上游超时降级）。前端应提示"数据可能已过期"。 */
  stale?: boolean
  /** usage/remaining 是否含本地乐观推算（真值 + 缓存后新花掉的 credit）。可加"约"字样。 */
  optimistic?: boolean
}

// 单条已缓存余额快照（后端 CachedBalanceItem：balance 字段被 serde flatten 到顶层，
// 再加一个 cachedAt）。用于概览/状态条按需展示，绝不触发上游调用（避免触发上游风控）。
export interface CachedBalanceItem extends BalanceResponse {
  /** 缓存写入时间（Unix 秒），前端据此显示“截至 X 分钟前”并判断新鲜度。 */
  cachedAt: number
}

// 批量已缓存余额响应（GET /api/admin/credentials/balances/cached）
export interface CachedBalancesResponse {
  /** 已缓存的凭据数量 */
  total: number
  /** id -> 缓存余额快照（key 为字符串化的凭据 id） */
  balances: Record<string, CachedBalanceItem>
}

// 成功响应
export interface SuccessResponse {
  success: boolean
  message: string
}

// 错误响应
export interface AdminErrorResponse {
  error: {
    type: string
    message: string
  }
}

// 请求类型
export interface SetDisabledRequest {
  disabled: boolean
}

export interface SetPriorityRequest {
  priority: number
}

// 添加凭据请求
export interface AddCredentialRequest {
  accessToken?: string
  refreshToken?: string
  authMethod?: 'social' | 'idc' | 'external_idp' | 'api_key' | 'custom_api'
  clientId?: string
  clientSecret?: string
  tokenEndpoint?: string
  issuerUrl?: string
  scopes?: string
  profileArn?: string
  expiresAt?: string
  priority?: number
  authRegion?: string
  apiRegion?: string
  machineId?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  kiroApiKey?: string
  endpoint?: string
  /**
   * 多开份数：同一账号导入 N 份，每份自动获得独立 machineId。
   * 省略 / 1 = 普通上号（后端行为完全不变，含"重复凭据"去重保护）。
   * >1 时第 1 份仍走去重，只有第 2..N 份绕过。上限 16。
   */
  copies?: number
  /**
   * 主份的出口**点名一个节点池 id**（「出口 IP → 从池中选」）。
   *
   * 必须传 id 而不是 URL：节点密码从不下发前端（只有 `hasPassword` 布尔），
   * 服务端按 id 自己取完整 `(url, username, password)` 写进凭据。
   *
   * 不与 `copies` 打架：本字段只钉主份，第 2..N 份仍从池里自动补
   * （若用 `nodeIds: [x]` 表达同一意图，那语义是"本次只用这些节点"，
   * 第 2/3 份就一个都拿不到）。
   *
   * id 不存在 / 已禁用 → 后端 400 且不建任何份（不会静默改成直连或别的节点）。
   */
  primaryNodeId?: number
  /**
   * 主份要不要也从节点池自动取一个出口。
   *
   * **省略 = 后端按份数定**：`copies=1` → true（只有一份，节点没有别的去处）；
   * `copies>1` → false（主份保持表单里选的出口，池节点全让给第 2..N 份，
   * 于是 N 份只需 N-1 个节点）。
   *
   * 「自动分配」按钮下发 `true`：那正是"让服务端替主份挑一个最合适的节点"。
   */
  assignPrimaryNode?: boolean
  /**
   * 要求每份都拿到独立节点，凑不齐就整个请求 400（**一份也不建**）。
   *
   * 省略 = 后端宽松模式（节点不够时多余的份直连并在文案里说明，行为不变）。
   * 前端在走池分配时下发 `true`：用户点的是"把这些份分散到不同出口"，
   * 凑不齐时给他一堆共用同一出口的份是假成功。
   */
  requireNodePerCopy?: boolean
  // 自定义 API 代挂透传
  baseUrl?: string
  apiKey?: string
  requestLimit?: number
  /** 自定义 API 代挂:deepseek 协议归一化开关（创建时设；改凭据走 deepseek-normalize 端点） */
  deepseekNormalize?: boolean
  /** 代挂模型白名单（创建表单探测勾选而来；空/不传 = 不限制） */
  allowedModels?: string[]
}

/**
 * 给**池中已有**凭据再加分身的请求（`POST /credentials/{id}/clone`）。
 *
 * 没有 key 字段是刻意的：key 由服务端按 id 自己读，一步都不经前端
 * （`CredentialStatusItem` 只有 `apiKeyHash` 与掩码形态，前端拿不到原文）。
 */
export interface CloneCredentialRequest {
  /** 本次再加几份。省略按 1 处理；上限与后端 `MAX_CREDENTIAL_COPIES` 同值（16）。 */
  copies?: number
  /**
   * 新建的分身是否直接启用。**省略 = 后端默认 false（不启用）**。
   *
   * 默认不启用是刻意的：新分身还没绑出口、没验活，直接入池参与调度等于
   * 把未经验证的号推上热路径；而分身共享同一个上游账号配额，多几份立刻
   * 参与调度只会更快撞 429。
   */
  enabled?: boolean
  /** 本次要用的节点池 id 列表。省略 = 从池里自动分配（按已绑数↑、延迟↑）。 */
  nodeIds?: number[]
  /**
   * 本次新建的**第 1 份**要不要也从池里取节点。
   *
   * ⚠️ **省略 = 后端默认 `true`**（与 `AddCredentialRequest.assignPrimaryNode` 的
   * 默认相反）：这条路父号一字节不动，建出来的 N 份全是新条目、彼此同质，
   * 让第 1 份独独裸连而池里空着一个节点，正是 2026-08-05 修掉的缺陷。
   */
  assignPrimaryNode?: boolean
  /** 凑不齐「每份一个独立节点」就整个请求 400。省略 = 宽松（不够的份直连）。 */
  requireNodePerCopy?: boolean
}

// 添加凭据响应
export interface AddCredentialResponse {
  success: boolean
  message: string
  /** 多开时这是第 1 份的 id；全部 id 见 credentialIds */
  credentialId: number
  /** 多开（copies>1）时全部新建凭据的 id（含第 1 份）。部分失败时只含成功的那些 */
  credentialIds?: number[]
  email?: string
}

// ============ 网页上号（Social OAuth）============

// 发起网页上号请求
export interface StartSocialLoginRequest {
  priority?: number
  proxyUrl?: string
}

// 发起网页上号响应
export interface StartSocialLoginResponse {
  sessionId: string
  portalUrl: string
}

// 轮询网页上号响应
export interface PollSocialLoginResponse {
  status: 'pending' | 'done' | 'error'
  credentialId?: number
  email?: string
  message?: string
}

// ============ IDC 上号（AWS SSO Device Code）============

export interface StartIdcLoginRequest {
  startUrl: string
  region?: string
  priority?: number
  proxyUrl?: string
}

export interface StartIdcLoginResponse {
  sessionId: string
  verificationUri: string
  verificationUriComplete?: string
  userCode: string
  expiresIn: number
}

export interface PollIdcLoginResponse {
  status: 'pending' | 'done' | 'expired' | 'error'
  credentialId?: number
  message?: string
}

// ============ 微软 SSO 上号（External IdP · 三步引导）============
// 全程零本机运行：用户只需在浏览器里复制地址栏 URL 粘回，本机不装/不跑任何程序。

// 第 1 步：发起外部 IdP 上号请求
export interface StartExternalIdpLoginRequest {
  priority?: number
  proxyUrl?: string
  // 优先探测区域（可选）：并入授权后的多 region profile 探测候选并排头，覆盖冷门 region。
  region?: string
}

// 第 1 步响应：拿到会话 id + Kiro 登录地址
export interface StartExternalIdpLoginResponse {
  sessionId: string
  signinUrl: string
}

// 第 2 / 3 步：把浏览器地址栏整串 URL 粘回
export interface ExternalIdpPasteRequest {
  sessionId: string
  url: string
}

// 第 2 步响应：拿到微软授权地址
export interface ExternalIdpLeg1Response {
  authorizeUrl: string
}

// 一个可选 profile：ARN + region + account（多 region 账号供用户选）
export interface ExternalIdpProfileOption {
  arn: string
  region: string
  account: string
  /** 该区域是否可用（订阅已开通）。缺省视为可用（旧后端未下发时不误置灰）。 */
  usable?: boolean
  /** 订阅等级标题（如 "Kiro Pro"），供选择列表展示。 */
  subscriptionTitle?: string | null
}

// 单个凭据在某 region 的 profile（切 Profile ARN 用）。
// 约定字段：arn / region / account / usable / subscriptionTitle（后端字段名有出入时以此为准）。
export interface CredentialRegionProfile {
  arn: string
  region: string
  account?: string | null
  /** 该区域是否可用（订阅已开通）。 */
  usable: boolean
  /** 订阅等级标题（如 "Kiro Pro"）。 */
  subscriptionTitle?: string | null
  /** 是否为该凭据当前正在使用的 profile。 */
  current?: boolean
}

// GET /credentials/{id}/regions 响应：该账号各 region 的 profile 列表。
export interface CredentialRegionsResponse {
  regions: CredentialRegionProfile[]
}

// 第 3 步响应：换 token + 探测多 region profile。
// - profiles 多个 → 弹窗选 region，随后调 leg2/select 建号（credentialId 为 null）。
// - profiles 恰 1 个 → 后端已自动建号，credentialId 有值，直接完成。
export interface ExternalIdpLeg2Response {
  credentialId: number | null
  profiles: ExternalIdpProfileOption[]
}

// 第 3 步选定响应：选定 profile 建号成功
export interface ExternalIdpSelectResponse {
  credentialId: number
}

// ============ 服务端配置快照 ============

// 服务端配置（敏感字段已脱敏）
export interface ConfigSnapshotResponse {
  /** 服务端真实版本(编译期注入),侧边栏据此展示,不再硬编码 */
  serverVersion: string
  host: string
  port: number
  region: string
  kiroVersion: string
  systemVersion: string
  nodeVersion: string
  tlsBackend: string
  loadBalancingMode: string
  defaultEndpoint: string
  endpointNames: string[]
  extractThinking: boolean
  ccAutoBuffer: boolean
  /** 影子缓存记账：是否把估算的 prompt cache 命中下发给客户端（默认 true）。 */
  promptCacheEnabled: boolean
  /** 批量推号入口 POST /api/import/keys 是否启用（默认**开**：端点先于开关存在，
      外部 kiro-accounting 正在用；关掉后两个挂载点一起返 403）。 */
  importKeysEnabled: boolean
  /** 上游 429 吸收层：客户端未收到任何字节时，网关就地退避重试可恢复的 429（默认**开**）。 */
  upstreamRetryAbsorbEnabled: boolean
  upstreamRetryAbsorbBudgetSecs: number
  upstreamRetryAbsorbMaxRounds: number
  upstreamRetryAbsorbMinDelayMs: number
  upstreamRetryAbsorbMaxDelaySecs: number
  /** 是否把 403 临时风控也当可吸收错误（默认关，与自愈退避冲突）。 */
  upstreamRetryAbsorbSuspended: boolean
  stripEnvNoise: boolean
  toolCleanLeakedTokens: boolean
  toolReclaimTextifiedInvoke: boolean
  toolStrayRepeatGuard: boolean
  toolStreamAlignFailure: boolean
  toolExposeErrorToClient: boolean
  toolRepairJson: boolean
  toolTruncationRecovery: boolean
  toolDescriptionMaxChars: number
  /** credentials.json / trash.json at-rest 加密开关（机器绑定密钥，立即生效，默认关）。 */
  encryptCredentialsAtRest: boolean
  cooldownEnabled: boolean
  /** 账户级 403 风控连续 N 次零成功后自动禁用该号。默认开。 */
  autoDisableSuspicious: boolean
  /** 分身（clone）新建时默认是否启用（默认 false = 建出来是禁用的）。 */
  cloneDefaultEnabled: boolean
  allCoolingFastFail: boolean
  rateLimitEnabled: boolean
  rateLimitDailyMax: number
  rateLimitMinIntervalMs: number
  affinityEnabled: boolean
  priorityInBalanced: boolean
  /** 智能调度（0.7.23 headroom/背压 + 0.7.24 余额加权/429 感知，均热更即时生效） */
  /** 全局每号 RPM 软上限（单号 rpm_limit=0 时继承此值；此值也为 0 时用内置兜底 30） */
  credentialRpmLimit: number
  rpmHeadroomFactor: number
  rpmReserveSlots: number
  rpmHardGateOverloadWait: boolean
  /** 冷却时长缩放百分比（10..500，100=原时长；<100 更激进快重试，>100 更保守防封） */
  cooldownScalePct: number
  /** 拟人速率：请求间隔抖动百分比（0..50），让节奏更像人 */
  rateLimitJitterPct: number
  // 入站请求整形 + RPM 自动挡
  inboundThrottleEnabled: boolean
  inboundRpmAuto: boolean
  inboundTargetRpm: number
  inboundRpmMin: number
  inboundRpmMax: number
  inboundBurstSecs: number
  inboundQueueMaxWaitSecs: number
  inboundQueueTimeoutPassthrough: boolean
  /**
   * 当前实时**目标** RPM（自动挡动态，只读展示）。
   *
   * ⚠️ "current" 指「当前生效的目标值」，**不是实测吞吐**。实测在
   * `inboundObservedRpm`。这两个曾是同一个值（后端把它接成 target），实测面板显示
   * 500 而客户端真实只有 50~70 —— 差一个数量级。展示时务必标明它是目标而非实测。
   */
  inboundCurrentRpm: number
  /**
   * 最近 60 秒**实测**入站 RPM（客户端请求数，**不含** failover 重试）。
   *
   * 旧后端不下发此字段 ⇒ 可能为 undefined，渲染时要降级而不是显示 0
   * （显示 0 会被读成"没有流量"，而真相是"这个后端还没有这个指标"）。
   */
  inboundObservedRpm?: number
  /**
   * 逐号 RPM 之和 = 上游实际承受的尝试速率（**含** failover 重试）。
   *
   * 与 `inboundObservedRpm` 的比值即重试放大倍数（2026-08-06 实测 4.59×）。
   * 两者量纲不同，别相减也别当同一条曲线画。
   */
  inboundObservedUpstreamRpm?: number
  /** 累计放行的客户端请求数（滑窗恒 0 而它在涨 ⇒ 滑窗坏了）。 */
  inboundAdmittedTotal?: number
  balanceWeightEnabled: boolean
  balanceWeightFloor: number
  health429WeightEnabled: boolean
  hasProxy: boolean
  proxyUrl?: string
  hasAdminKey: boolean
  hasApiKey: boolean
  callbackMode: string
  callbackBaseUrl?: string
  // 反代安全（批次3）
  corsAllowedOrigins: string[]
  ipAllowlist: string[]
  ipBlocklist: string[]
  machineCodeBlocklist: string[]
  trustForwardedHeader: boolean
  ingressRateLimitPerMin: number
  maxBodyBytes: number
  // 主动 token 预刷新（批次4.4）
  proactiveTokenRefresh: boolean
  tokenRefreshLeadMinutes: number
  tokenRefreshIntervalSecs: number
  // 后台温和余额刷新间隔（秒；0 = 禁用）
  balanceRefreshIntervalSecs: number
  // Admin UI 登录页：是否显示随机背景图（立即生效，无需重启）。缺省视为开启。
  loginBackgroundEnabled?: boolean
  // Admin UI 登录页：背景图是否走 R18 图源（立即生效，无需重启）。缺省视为开启。
  loginBackgroundR18?: boolean
  // 隐私：是否采集下游客户端指纹（设备/IP/系统/浏览器）。立即生效，无需重启。缺省视为开启。
  collectClientFingerprint?: boolean
  configPath?: string
}

// 更新服务端配置请求（所有字段可选，仅提交的字段被修改）
export interface UpdateConfigRequest {
  host?: string
  port?: number
  region?: string
  kiroVersion?: string
  systemVersion?: string
  nodeVersion?: string
  tlsBackend?: string
  loadBalancingMode?: string
  defaultEndpoint?: string
  extractThinking?: boolean
  ccAutoBuffer?: boolean
  importKeysEnabled?: boolean
  promptCacheEnabled?: boolean
  // 上游 429 吸收层
  upstreamRetryAbsorbEnabled?: boolean
  upstreamRetryAbsorbBudgetSecs?: number
  upstreamRetryAbsorbMaxRounds?: number
  upstreamRetryAbsorbMinDelayMs?: number
  upstreamRetryAbsorbMaxDelaySecs?: number
  upstreamRetryAbsorbSuspended?: boolean
  stripEnvNoise?: boolean
  toolCleanLeakedTokens?: boolean
  toolReclaimTextifiedInvoke?: boolean
  toolStrayRepeatGuard?: boolean
  toolStreamAlignFailure?: boolean
  toolExposeErrorToClient?: boolean
  toolRepairJson?: boolean
  toolTruncationRecovery?: boolean
  toolDescriptionMaxChars?: number
  encryptCredentialsAtRest?: boolean
  cooldownEnabled?: boolean
  autoDisableSuspicious?: boolean
  cloneDefaultEnabled?: boolean
  allCoolingFastFail?: boolean
  rateLimitEnabled?: boolean
  rateLimitDailyMax?: number
  rateLimitMinIntervalMs?: number
  affinityEnabled?: boolean
  priorityInBalanced?: boolean
  credentialRpmLimit?: number
  rpmHeadroomFactor?: number
  rpmReserveSlots?: number
  rpmHardGateOverloadWait?: boolean
  cooldownScalePct?: number
  rateLimitJitterPct?: number
  inboundThrottleEnabled?: boolean
  inboundRpmAuto?: boolean
  inboundTargetRpm?: number
  inboundRpmMin?: number
  inboundRpmMax?: number
  inboundBurstSecs?: number
  inboundQueueMaxWaitSecs?: number
  inboundQueueTimeoutPassthrough?: boolean
  balanceWeightEnabled?: boolean
  balanceWeightFloor?: number
  health429WeightEnabled?: boolean
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  /** userKey（对话 api_key）：留空不改，填了更新，需重启生效 */
  apiKey?: string
  callbackBaseUrl?: string
  // 反代安全（批次3，整表替换语义）
  corsAllowedOrigins?: string[]
  ipAllowlist?: string[]
  ipBlocklist?: string[]
  machineCodeBlocklist?: string[]
  trustForwardedHeader?: boolean
  ingressRateLimitPerMin?: number
  maxBodyBytes?: number
  // 主动 token 预刷新（批次4.4）
  proactiveTokenRefresh?: boolean
  tokenRefreshLeadMinutes?: number
  tokenRefreshIntervalSecs?: number
  // 后台温和余额刷新间隔（秒；0 = 禁用）
  balanceRefreshIntervalSecs?: number
  // Admin UI 登录页：显示背景图开关（立即生效，无需重启）
  loginBackgroundEnabled?: boolean
  // Admin UI 登录页：R18 图源开关（立即生效，无需重启）
  loginBackgroundR18?: boolean
  // 隐私：采集下游客户端指纹开关（立即生效，无需重启）
  collectClientFingerprint?: boolean
}

// 更新服务端配置响应
export interface UpdateConfigResponse {
  success: boolean
  message: string
  restartRequired: boolean
  restartFields: string[]
}

// ============ 用量统计（snake_case，与后端 usage 模块序列化一致）============

// 某个时间窗口的汇总
export interface WindowSummary {
  requests: number
  success: number
  failure: number
  success_rate: number
  /** 输入 token —— gross 口径，**已包含** cache_read_tokens + cache_creation_tokens */
  input_tokens: number
  output_tokens: number
  /** 仍为 input_tokens + output_tokens，不含额外缓存增量 */
  total_tokens: number
  /** 命中缓存复用的输入 token（input_tokens 的子集；网关本地估算） */
  cache_read_tokens: number
  /** 写入缓存的输入 token（input_tokens 的子集；上游不返回，当前恒为 0） */
  cache_creation_tokens: number
  credits_used: number
  avg_latency_ms: number
  /**
   * 换号次数累计（后端 `Aggregate.retries_sum` 直出）。
   *
   * **后端已下发**（`WindowSummary` DTO 已接通出口）。仍保持 optional 是为了兼容
   * 旧后端二进制 + 新前端的组合 —— 那种情况下字段真的不在，标成必填就是类型说谎。
   */
  retries_sum?: number
  /**
   * **发生过**重试的请求数（`retries > 0`）。同为后端已下发字段（optional 理由同上）。
   *
   * 两个字段必须成对出现、且**不要只取其一**：绝大多数请求 `retries=0`，
   * 用 requests 当分母算出的均值会被压到接近 0（1000 条里 10 条各重试 6 次 ⇒
   * 6000/1000=0.06，看着"几乎不重试"，真相是那 10 条平均重试 6 次）。
   * 两个分母各有用途：`retries_sum / requests` 是整池放大倍数（可与外置 shield 的
   * 实测 3.27x 同口径对比），`retries_sum / retried_requests` 是"真重试时重试几次"。
   */
  retried_requests?: number
  /** 每请求平均换号次数（= retries_sum / requests，整池放大倍数口径，后端已算好） */
  avg_retries_per_request?: number
  /**
   * 真发生重试时的平均次数（= retries_sum / retried_requests）。
   * 无重试样本时后端给 `null`（不是 0 —— 0 会被读成"重试过但只重试 0 次"）。
   */
  avg_retries_when_retried?: number | null
  /**
   * 平均 TTFB（毫秒）。无有效样本时为 `null` —— 0ms 物理上不可能，
   * 把"没数据"显示成 0 比显示 '—' 危险得多（看着像"快到测不出"）。
   */
  avg_first_token_ms?: number | null
}

// 概览：24h / 7d / 30d 三窗口
export interface UsageOverview {
  last_24h: WindowSummary
  last_7d: WindowSummary
  last_30d: WindowSummary
  all_time: WindowSummary
}

// 时间序列点（空桶各字段补 0，含两个 cache 字段）
export interface SeriesPoint {
  ts_ms: number
  requests: number
  success: number
  failure: number
  /** gross 口径，已含 cache 两项 */
  input_tokens: number
  output_tokens: number
  /** 命中缓存复用的输入 token（input_tokens 的子集） */
  cache_read_tokens: number
  /** 写入缓存的输入 token（input_tokens 的子集） */
  cache_creation_tokens: number
  credits_used: number
  avg_latency_ms: number
  /**
   * 该桶的换号次数累计（画重试趋势的数据源）。**后端已下发**；
   * optional 只为兼容旧后端二进制（见 WindowSummary 上同名字段的说明）。
   * 序列点上后端只给原始计数不给平均值 —— 两个分母都在同一个点里，口径由前端定。
   */
  retries_sum?: number
  /** 该桶内发生过重试的请求数（`retries > 0`）；与 retries_sum 成对使用，勿只取其一 */
  retried_requests?: number
}

// 按模型/凭据分组统计（无 total_tokens 字段）
export interface GroupStat {
  key: string
  requests: number
  success_rate: number
  /** gross 口径，已含 cache 两项 */
  input_tokens: number
  output_tokens: number
  /** 命中缓存复用的输入 token（input_tokens 的子集） */
  cache_read_tokens: number
  /** 写入缓存的输入 token（input_tokens 的子集） */
  cache_creation_tokens: number
  credits_used: number
  avg_latency_ms: number
  /**
   * 换号次数累计（按模型/凭据看哪个维度在烧重试预算）。**后端已下发**；
   * optional 只为兼容旧后端二进制（见 WindowSummary 上同名字段的说明）。
   */
  retries_sum?: number
  /** 发生过重试的请求数（`retries > 0`）；与 retries_sum 成对使用，勿只取其一 */
  retried_requests?: number
  /** 该分组每请求平均换号次数（= retries_sum / requests，后端已算好） */
  avg_retries_per_request?: number
}

// 请求结果分类
export type RequestOutcome =
  | 'success'
  | 'rate_limited'
  | 'auth_failed'
  | 'quota_exhausted'
  | 'account_suspended'
  | 'server_error'
  | 'bad_request'
  | 'network_error'
  | 'model_unavailable'
  | 'other_error'

// ============ 运维：存储统计 / 清理（对接 GET /storage/stats, POST /storage/cleanup） ============

// 单个数据分区的占用统计（后端 serde camelCase）
export interface StoragePartition {
  /** 分区键（与清理 target 一致）：traces | usage_jsonl | trash | bg_cache */
  key: string
  /** 展示名（中文） */
  label: string
  /** 占用字节数（内存分区为常驻内存字节） */
  bytes: number
  /** 条目/文件数（trace 为行数，usage_jsonl 为文件数，trash 为条目数，bg_cache 为张数） */
  items: number
  /** 落盘路径（内存分区省略） */
  path?: string
  /** 是否为纯内存分区（无落盘，清理即释放内存） */
  inMemory: boolean
}

// 存储统计响应
export interface StorageStatsResponse {
  partitions: StoragePartition[]
  /** 落盘分区字节合计（不含纯内存分区） */
  totalDiskBytes: number
  /** 统计是否可用（用量统计未启用时 trace/jsonl 分区缺失） */
  usageEnabled: boolean
}

// 清理目标白名单
export type StorageCleanupTarget = 'traces' | 'usage_jsonl' | 'trash' | 'bg_cache' | 'all'

// 存储清理请求
export interface StorageCleanupRequest {
  /** 清理目标（白名单枚举） */
  target: StorageCleanupTarget
  /** 保留天数：删除早于 N 天前的数据。省略时按各分区的配置默认保留期。 */
  olderThanDays?: number
  /**
   * 全清标记：忽略 olderThanDays，清空该分区**全部**条目。
   *
   * 回收站必须靠它才能清空：保留天数的 0 在后端已被后台任务占用为「永久保留」，
   * 所以按天数的入参无法表达「立即全清」——传 0 清 0 条，传 N 又清不掉 N 天内新删的。
   * 不可逆，调用前须二次确认。
   */
  purgeAll?: boolean
}

// 单个分区的清理结果
export interface StorageCleanupItem {
  key: string
  /** 清理的条目/文件数 */
  removed: number
  /** 释放的字节数（不可精确统计时为 0） */
  freedBytes: number
  /** 说明（如跳过原因） */
  note?: string
}

// 存储清理响应
export interface StorageCleanupResponse {
  success: boolean
  message: string
  results: StorageCleanupItem[]
}

// ============ per 客户端/窗口 RPM（对接 GET /usage/clients） ============

// 单个窗口（session）的 RPM
export interface SessionRpm {
  /** 窗口标识（session_id / conversationId） */
  sessionId: string
  /** 该窗口最近 60 秒请求数（RPM） */
  rpm: number
}

// 单个下游客户端的 RPM 视图（按 client_ip 优先，回退 device 分组）
export interface ClientRpm {
  /** 客户端分组键（client_ip 优先，回退 device） */
  clientKey: string
  /** 客户端 IP（可能缺省） */
  clientIp?: string
  /** 设备类型（如 claude-code） */
  device?: string
  /** 该客户端最近 60 秒请求数（RPM，聚合其所有窗口） */
  rpm: number
  /** 活跃窗口数（distinct session_id，近 10 分钟内有请求） */
  activeSessions: number
  /** 各活跃窗口的 RPM（后端已按 RPM 降序） */
  sessions: SessionRpm[]
}

// 注:CooldownDetail / RateLimitInsight 已在本文件后半定义(限流健康用),此处不重复。

// 单条请求明细
export interface RequestRecord {
  request_id: string
  ts_ms: number
  credential_id: number | null
  model: string
  is_streaming: boolean
  input_tokens: number
  output_tokens: number
  credits_used: number | null
  latency_ms: number
  first_token_ms: number | null
  outcome: RequestOutcome
  retries: number
  error_message: string | null
  session_id: string | null
  /** 请求来源设备（后端从入站 User-Agent 分类）：
      claude-code / curl / windows / macos / linux / python / node / vscode / browser / unknown */
  client_device?: string | null
  /** 客户端 IP（后端从 x-forwarded-for 首段 / x-real-ip 提取，可能缺省） */
  client_ip?: string | null
  /** 客户端操作系统（后端解析 UA）：如 "Windows"/"macOS"/"iOS"/"Android"/"Linux"，无法识别为 null */
  client_os?: string | null
  /** 客户端浏览器（后端解析 UA）：如 "Chrome 120"/"Edge 120"/"Safari"，非浏览器客户端为 null */
  client_browser?: string | null
  /** 从缓存复用、省下的输入 token（cache_read_input_tokens）。
      是 input_tokens 的**子集**（后者为 gross 口径），算总输入时不得再加。历史记录缺省 0。 */
  cache_read_tokens?: number
  /** 写入缓存的输入 token（cache_creation_input_tokens）。同为 input_tokens 的子集。
      上游 Kiro 不提供该数字，当前恒为 0。历史记录缺省 0。 */
  cache_creation_tokens?: number
}

// 单台机器（按设备指纹分组，IP 变化不拆分）的 RPM 视图（对接 GET /usage/machines）。
// 与 ClientRpm（按 IP 分组）的关键区别：分组主键是设备画像派生的 machineKey（不含 IP），
// 同一机器换 IP（DHCP/VPN/NAT）仍合并为一组，IP 只作 ips 列表展示。
// 单个 IP → 机器码：漫游机器（多 IP）逐 IP 展示可复制的封禁码。
export interface MachineIpCode {
  ip: string
  code: string
}

export interface MachineRpm {
  /** 机器分组键（设备画像派生：device|os|browser 拼接，稳定标识一台机器） */
  machineKey: string
  /** 机器码（MC- + SHA256 前 12 位，对应 machineKey/粘滞 IP；漫游多 IP 请用 ipCodes 逐个封） */
  machineCode: string
  /** 设备类型（如 claude-code） */
  device?: string | null
  /** 操作系统细分（如 Windows） */
  os?: string | null
  /** 浏览器 + 版本（非浏览器为 null） */
  browser?: string | null
  /** 这台机器见过的所有 IP（升序去重） */
  ips: string[]
  /** 每个见过的 IP 各自的机器码（与 ips 对应）：复制哪个 IP 的码就精准封哪个 IP */
  ipCodes: MachineIpCode[]
  /** 该机器最近 60 秒请求数（RPM，聚合其所有窗口） */
  rpm: number
  /** 活跃窗口数（distinct session_id，近 10 分钟内有请求） */
  activeSessions: number
  /** 各活跃窗口的 RPM（后端已按 RPM 降序） */
  sessions: SessionRpm[]
}

/** 逐秒吞吐桶（GET /usage/throughput 的 recentBuckets 元素，后端 camelCase） */
export interface ThroughputBucket {
  /** 桶起始时间（Unix 毫秒，对齐到秒） */
  tsMs: number
  /** 该秒的请求数 */
  requests: number
  /** 该秒的 tokens（input+output）吞吐 */
  tokens: number
}

/** 全局实时吞吐快照：当前速率 + 最近 60 秒逐秒桶。
    前端据此驱动趋势图上「沿曲线流动的发光粒子」：
    - currentRps → 粒子密度（数量）
    - currentTokensPerSec → 粒子速度（流速）
    - 活跃度越高粒子越多越快越亮，空闲则稀疏慢速。
    纯读内存、零上游、无封号风险。 */
export interface ThroughputSnapshot {
  /** 最近 60 秒总请求数（RPM 近似） */
  currentRpm: number
  /** 最近 60 秒平均每秒请求数（粒子密度） */
  currentRps: number
  /** 最近 60 秒平均每秒 tokens 吞吐（粒子速度） */
  currentTokensPerSec: number
  /** 窗口时长（秒），前端据此换算速率 */
  windowSecs: number
  /** 最近 60 秒逐秒桶（从旧到新，空秒补 0） */
  recentBuckets: ThroughputBucket[]
}

// ============ 限流健康 insights（对接 GET /api/admin/ratelimit/insights） ============
// 后端 service.rs 的 RateLimitInsight / CooldownDetail（serde camelCase）。
// 全部取自内存快照（token_manager + cooldown + config 软上限），零上游、无封号风险。

// 单个凭据的冷却明细（未冷却时整体为 null）
export interface CooldownDetail {
  /** 冷却原因（中文描述，如「速率限制」「服务器错误」） */
  reason: string
  /** 剩余冷却时间（毫秒） */
  remainingMs: number
  /** 连续触发次数 */
  triggerCount: number
}

// 每号一条限流健康快照（后端按 rpm 降序、id 升序）
export interface RateLimitInsight {
  /** 凭据 ID */
  id: number
  /** 最近 60 秒滚动窗口内的选号次数（RPM） */
  rpm: number
  /** 每凭据 RPM 软上限（0 = 不限制） */
  rpmLimit: number
  /** 是否已达软上限（rpmLimit>0 且 rpm>=rpmLimit） */
  rpmSaturated: boolean
  /** 当前在途请求数 */
  inflight: number
  /** 是否已禁用（禁用号显示"已禁用"而非"畅通"） */
  disabled?: boolean
  /** 冷却明细；未冷却时为 null */
  cooldown: CooldownDetail | null
  /** 近期 429 次数（取自速率限制冷却的连续触发计数，零上游） */
  recent429: number
  /** 中文推断文案（如「#54 冷却中（速率限制）剩22s，已触发3次」「畅通」） */
  insightText: string
  /** 真实熔断/健康快照（后端 HealthTracker）。无健康记录（从未被选过）时为 null，前端按缺省=满血处理。 */
  health?: HealthSnapshot | null
}

/** 熔断器/健康只读快照（后端 HealthTracker，族级 family_key 共享）。 */
export interface HealthSnapshot {
  /** 熔断器是否 Open（完全拒流，等退避到期转半开） */
  circuitOpen: boolean
  /** 是否半开（试探性放行 admitProb 比例的流量） */
  halfOpen: boolean
  /** 半开期试探放行概率 [0,1]（Closed=1，Open=0） */
  admitProb: number
  /** 健康分 [0,1]（EWMA 成功率 × 429 惩罚） */
  health: number
  /** EWMA 成功率 [0,1] */
  ewmaSuccess: number
  /** EWMA 429 率 [0,1] */
  ewma429: number
  /** 连续 429 次数 */
  consecutive429: number
  /** 熔断剩余秒（Open 时 >0，其余为 0） */
  openRemainingSecs: number
}

/** 上号智能诊断：后端错误响应 error.diagnosis 携带（归因+引导），前端渲染诊断卡片。 */
export interface OnboardingDiagnosis {
  /** 阶段：register/device_auth/poll/resolve_profile/refresh/probe_region/verify */
  stage: string
  /** 归因方：user_input(用户填错)/account_state(账号问题)/upstream(AWS侧)/gateway(网关未覆盖)/transient(瞬时) */
  fault: 'user_input' | 'account_state' | 'upstream' | 'gateway' | 'transient'
  /** 机器可读错误码，如 REGION_MISMATCH / CLIENT_OR_TOKEN_MISMATCH / NO_PROFILE_IN_REGION */
  code: string
  /** 一句话中文诊断（主行） */
  summary: string
  /** 用户该做什么——有序可操作步骤 */
  guidance: string[]
  /** 原始上游 status+body（折叠详情，供排障） */
  raw?: string
  /** 能否重试 */
  retriable: boolean
}

/** 一次代理测活的结果快照（与 /proxy/test 同语义）。 */
export interface SocksNodeTest {
  ok: boolean
  latencyMs: number
  exitIp?: string
  error?: string
  /** 测试时刻（Unix 秒） */
  testedAt: number
}

/** 可复用代理节点（「分身管理」页维护的候选池）。
 *
 * ⚠️ `password` **不在此类型里**：后端恒不外传密码，只给 `hasPassword`。
 * 编辑表单必须在用户未触碰密码框时**不发送** password 键（省略 = 不改），
 * 发空串才是清空 —— 否则改个节点名就会把密码抹掉，已绑该节点的分身全部掉线。 */
export interface SocksNode {
  id: number
  name: string
  url: string
  username?: string
  hasPassword: boolean
  enabled: boolean
  lastTest?: SocksNodeTest
  createdAt: number
  /** 展示标签（name 优先，空则回落 url） */
  label: string
  /**
   * 已绑在这个节点上的凭据数。**启发式**：后端按「凭据的 proxyUrl == 节点的 url」
   * 字符串比对（凭据与节点之间没有 id 级绑定）。手工填过代理的号可能因 scheme
   * 未归一而漏算，方向偏低 —— 顶多把一个已被占的节点当空闲，而那正是节点不足时
   * 的既有行为。
   *
   * 排序用它做主键（少的优先），与后端自动分配同一口径；口径不一致会让下拉里
   * 的推荐顺序与实际分到的节点对不上。
   *
   * `#[serde(default)]` 在后端侧，故老后端不下发该字段时这里是 `undefined`。
   */
  boundCredentials?: number
}

export interface SocksNodesResponse {
  total: number
  nodes: SocksNode[]
}

/** 新建/更新代理节点请求。`id` 省略 = 新建。
 *
 * `url` 可以直接是节点商下发的**分享链接**（`socks://base64(user:pass)@host:port#name`）：
 * 后端 `parse_proxy_link` 会拆出账密与 `#name` 并只把干净地址写进 url，
 * 所以前端一个输入框就够，不必让用户自己把 base64 串拆成用户名/密码。 */
export interface SocksNodeUpsertRequest {
  id?: number
  name?: string
  url: string
  username?: string
  /** ⚠️ 省略 = 不改密码；空串 = 清空。绝不要为了「补齐字段」而回填空串。 */
  password?: string
  enabled?: boolean
}

/** 整段粘贴批量导入代理节点（`POST /socks/nodes/bulk-import`）。 */
export interface SocksNodeBulkImportRequest {
  /** 整段文本。后端逐行解析，非链接行安静跳过，按 url 去重。 */
  text: string
  /** 导入后是否直接启用。**省略 = 后端默认 false**（未测活的出口不该直接参与分配）。 */
  enabled?: boolean
}

/** 批量导入的逐行结果（后端 `SocksNodeBulkImportItem`）。
 *
 * 🔴 **后端是权威。** 前端粘贴后会先本地预览一遍（`lib/proxy-line-parse.ts`），
 * 但那只是为了在打后端之前把话说清楚；这里的 `status` / `reason` 才是真实发生的事，
 * 且带着前端无从得知的两类结论：`address_rejected`（SSRF 策略拦下）与
 * `over_capacity`（节点数上限）。两者不一致时以本结构为准。 */
export interface SocksNodeBulkImportItem {
  /** 原始行号（1 起，与用户粘的文本对齐） */
  lineno: number
  /** 该行原文，**密码已由后端脱敏** */
  raw: string
  status: 'ok' | 'duplicate' | 'invalid' | 'over_capacity'
  /** 稳定原因码（前端查 i18n）：`dup_in_paste` / `already_in_pool` /
   *  `address_rejected` / `over_capacity` / `bad_port` / `bad_host` /
   *  `no_host_port` / `ambiguous` / `not_proxy` */
  reason?: string
  /** 解析出的 `scheme://host:port` */
  address?: string
  /** 解析出的用户名（**密码恒不外传**） */
  username?: string
}

/** 批量导入结果。四个计数互不重叠，加起来才是「这段文本发生了什么」。 */
export interface SocksNodeBulkImportResponse {
  /** 真正新建的节点数 */
  added: number
  /** 跳过的行数 = 非链接行 + 地址被策略拒绝的行。
   *  ⚠️ 含义比字面宽（两类混在一起），精确归因看 `items`。 */
  skipped: number
  /** 因重复而未导入（已在池中 **或** 同一次粘贴内重复）。**不覆盖**原有账密 */
  duplicate: number
  /** 因超出节点数上限而未导入的条数 */
  overCapacity: number
  /** 服务端汇总文案（含「默认未启用」等提示，优先直接展示它） */
  message: string
  /** 逐行明细。安静跳过的行（标题/分隔线/说明文字）不在内。
   *
   * ⚠️ **可选**：面板与二进制可能不同版本（OTA 只换二进制不换浏览器缓存，反之亦然），
   * 旧后端不返回它。读之前必须判空，不要假定它存在。 */
  items?: SocksNodeBulkImportItem[]
}
