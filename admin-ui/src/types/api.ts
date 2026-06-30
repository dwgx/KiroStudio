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
}

// 更新服务端配置响应
export interface UpdateConfigResponse {
  success: boolean
  message: string
  restartRequired: boolean
  restartFields: string[]
}
