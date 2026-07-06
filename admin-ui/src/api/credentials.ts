import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  CredentialsStatusResponse,
  BalanceResponse,
  CachedBalancesResponse,
  SuccessResponse,
  SetDisabledRequest,
  SetPriorityRequest,
  AddCredentialRequest,
  AddCredentialResponse,
  StartSocialLoginRequest,
  StartSocialLoginResponse,
  PollSocialLoginResponse,
  StartIdcLoginRequest,
  StartIdcLoginResponse,
  PollIdcLoginResponse,
  ConfigSnapshotResponse,
  UpdateConfigRequest,
  UpdateConfigResponse,
} from '@/types/api'

// 创建 axios 实例
const api = axios.create({
  baseURL: '/api/admin',
  headers: {
    'Content-Type': 'application/json',
  },
})

// 请求拦截器添加 API Key
api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) {
    config.headers['x-api-key'] = apiKey
  }
  return config
})

// 获取所有凭据状态
export async function getCredentials(): Promise<CredentialsStatusResponse> {
  const { data } = await api.get<CredentialsStatusResponse>('/credentials')
  return data
}

// 设置凭据禁用状态
export async function setCredentialDisabled(
  id: number,
  disabled: boolean
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/disabled`,
    { disabled } as SetDisabledRequest
  )
  return data
}

// 设置凭据优先级
export async function setCredentialPriority(
  id: number,
  priority: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/priority`,
    { priority } as SetPriorityRequest
  )
  return data
}

// 重置失败计数
export async function resetCredentialFailure(
  id: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/reset`)
  return data
}

// 强制刷新 Token
export async function forceRefreshToken(
  id: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/refresh`)
  return data
}

// 获取凭据余额（按需，hover 时触发；会向上游拉取，注意封号红线勿批量并发）
export async function getCredentialBalance(id: number): Promise<BalanceResponse> {
  const { data } = await api.get<BalanceResponse>(`/credentials/${id}/balance`)
  return data
}

// 批量读取【已缓存】的余额快照（只读缓存，绝不触发上游调用，安全用于概览/状态条）。
// 后端后台每 30 分钟温和刷新一次缓存，这里返回最近已知值 + cachedAt 新鲜度。
export async function getCachedBalances(): Promise<CachedBalancesResponse> {
  const { data } = await api.get<CachedBalancesResponse>('/credentials/balances/cached')
  return data
}

// 深度验活（真实 API 调用检测 suspend）
export async function deepVerifyCredential(id: number): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/verify`)
  return data
}

// 添加新凭据
export async function addCredential(
  req: AddCredentialRequest
): Promise<AddCredentialResponse> {
  const { data } = await api.post<AddCredentialResponse>('/credentials', req)
  return data
}

// 删除凭据
export async function deleteCredential(id: number): Promise<SuccessResponse> {
  const { data } = await api.delete<SuccessResponse>(`/credentials/${id}`)
  return data
}

// 导出凭据完整对象（原始 KiroCredentials，camelCase，含 refreshToken/kiroApiKey 等）
// 字段随认证方式不同而不同，前端按拿到的对象处理，不假设某字段一定存在。
export async function exportCredential(id: number): Promise<Record<string, unknown>> {
  const { data } = await api.get<Record<string, unknown>>(`/credentials/${id}/export`)
  return data
}

// 获取负载均衡模式
export async function getLoadBalancingMode(): Promise<{ mode: 'priority' | 'balanced' }> {
  const { data } = await api.get<{ mode: 'priority' | 'balanced' }>('/config/load-balancing')
  return data
}

// 设置负载均衡模式
export async function setLoadBalancingMode(mode: 'priority' | 'balanced'): Promise<{ mode: 'priority' | 'balanced' }> {
  const { data } = await api.put<{ mode: 'priority' | 'balanced' }>('/config/load-balancing', { mode })
  return data
}

// 发起网页上号（返回浏览器登录地址）
export async function startSocialLogin(
  req: StartSocialLoginRequest
): Promise<StartSocialLoginResponse> {
  const { data } = await api.post<StartSocialLoginResponse>('/auth/social/start', req)
  return data
}

// 轮询网页上号状态
export async function pollSocialLogin(
  sessionId: string
): Promise<PollSocialLoginResponse> {
  const { data } = await api.post<PollSocialLoginResponse>(`/auth/social/poll/${sessionId}`)
  return data
}

// 发起 IDC 上号（AWS SSO device code flow）
export async function startIdcLogin(
  req: StartIdcLoginRequest
): Promise<StartIdcLoginResponse> {
  const { data } = await api.post<StartIdcLoginResponse>('/auth/idc/start', {
    start_url: req.startUrl,
    region: req.region,
    priority: req.priority,
    proxy_url: req.proxyUrl,
  })
  // 后端返回 snake_case，前端用 camelCase
  return {
    sessionId: (data as any).session_id ?? data.sessionId,
    verificationUri: (data as any).verification_uri ?? data.verificationUri,
    verificationUriComplete: (data as any).verification_uri_complete ?? data.verificationUriComplete,
    userCode: (data as any).user_code ?? data.userCode,
    expiresIn: (data as any).expires_in ?? data.expiresIn,
  }
}

// 轮询 IDC 上号状态
export async function pollIdcLogin(
  sessionId: string
): Promise<PollIdcLoginResponse> {
  const { data } = await api.post<PollIdcLoginResponse>(`/auth/idc/poll/${sessionId}`)
  // 后端返回 snake_case
  return {
    status: data.status,
    credentialId: (data as any).credential_id ?? data.credentialId,
    message: data.message,
  }
}

// 获取服务端配置快照（敏感字段脱敏）
export async function getConfigSnapshot(): Promise<ConfigSnapshotResponse> {
  const { data } = await api.get<ConfigSnapshotResponse>('/config')
  return data
}

// 更新服务端配置（仅提交的字段被修改）
export async function updateConfig(
  req: UpdateConfigRequest
): Promise<UpdateConfigResponse> {
  const { data } = await api.put<UpdateConfigResponse>('/config', req)
  return data
}
