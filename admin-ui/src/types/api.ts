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
  refreshToken?: string
  authMethod?: 'social' | 'idc' | 'api_key'
  clientId?: string
  clientSecret?: string
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
}
