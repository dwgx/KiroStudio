// 凭据状态响应
export interface CredentialsStatusResponse {
  total: number
  available: number
  currentId: number
  credentials: CredentialStatusItem[]
}

// 单个凭据状态
export interface CredentialStatusItem {
  id: number
  priority: number
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
  lastUsedAt: string | null
  hasProxy: boolean
  proxyUrl?: string
  refreshFailureCount: number
  disabledReason?: string
  endpoint: string
  /** 当前在途（in-flight）请求数：实时负载，用于观测负载是否均衡分摊。 */
  inflight?: number
  /** 最近 60 秒滚动窗口内的请求数（RPM 观测）。 */
  rpm?: number
  /** Overage（超额）开关：KIRO Pro+ 开启后可突破 base 额度（付费）。
      后端 BE-overage 批次落地前为只读展示；字段缺省视为未知/关闭。 */
  overageEnabled?: boolean
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
}

// 单条已缓存余额快照（后端 CachedBalanceItem：balance 字段被 serde flatten 到顶层，
// 再加一个 cachedAt）。用于概览/状态条按需展示，绝不触发上游调用（封号红线）。
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
  authMethod?: 'social' | 'idc' | 'external_idp' | 'api_key'
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
}

// 添加凭据响应
export interface AddCredentialResponse {
  success: boolean
  message: string
  credentialId: number
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

// ============ 服务端配置快照 ============

// 服务端配置（敏感字段已脱敏）
export interface ConfigSnapshotResponse {
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
  cooldownEnabled: boolean
  rateLimitEnabled: boolean
  rateLimitDailyMax: number
  rateLimitMinIntervalMs: number
  affinityEnabled: boolean
  hasProxy: boolean
  proxyUrl?: string
  hasAdminKey: boolean
  callbackMode: string
  callbackBaseUrl?: string
  // 反代安全（批次3）
  corsAllowedOrigins: string[]
  ipAllowlist: string[]
  trustForwardedHeader: boolean
  ingressRateLimitPerMin: number
  maxBodyBytes: number
  // 主动 token 预刷新（批次4.4）
  proactiveTokenRefresh: boolean
  tokenRefreshLeadMinutes: number
  tokenRefreshIntervalSecs: number
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
  cooldownEnabled?: boolean
  rateLimitEnabled?: boolean
  rateLimitDailyMax?: number
  rateLimitMinIntervalMs?: number
  affinityEnabled?: boolean
  proxyUrl?: string
  callbackBaseUrl?: string
  // 反代安全（批次3，整表替换语义）
  corsAllowedOrigins?: string[]
  ipAllowlist?: string[]
  trustForwardedHeader?: boolean
  ingressRateLimitPerMin?: number
  maxBodyBytes?: number
  // 主动 token 预刷新（批次4.4）
  proactiveTokenRefresh?: boolean
  tokenRefreshLeadMinutes?: number
  tokenRefreshIntervalSecs?: number
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
  input_tokens: number
  output_tokens: number
  total_tokens: number
  credits_used: number
  avg_latency_ms: number
}

// 概览：24h / 7d / 30d 三窗口
export interface UsageOverview {
  last_24h: WindowSummary
  last_7d: WindowSummary
  last_30d: WindowSummary
}

// 时间序列点
export interface SeriesPoint {
  ts_ms: number
  requests: number
  success: number
  failure: number
  input_tokens: number
  output_tokens: number
  credits_used: number
  avg_latency_ms: number
}

// 按模型/凭据分组统计
export interface GroupStat {
  key: string
  requests: number
  success_rate: number
  input_tokens: number
  output_tokens: number
  credits_used: number
  avg_latency_ms: number
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
