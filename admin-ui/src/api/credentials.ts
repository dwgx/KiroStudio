import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  CredentialsStatusResponse,
  BalanceResponse,
  CachedBalancesResponse,
  TrashListResponse,
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
  StartExternalIdpLoginRequest,
  StartExternalIdpLoginResponse,
  ExternalIdpLeg1Response,
  ExternalIdpLeg2Response,
  ConfigSnapshotResponse,
  UpdateConfigRequest,
  UpdateConfigResponse,
} from '@/types/api'

// 创建 axios 实例
const api = axios.create({
  baseURL: '/api/admin',
  // 超时兜底：避免网络/后端异常时请求无限挂起（登录卡顿的成因之一）。
  timeout: 15000,
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

// 登录页校验期间抑制"自动 reload 回登录"：登录校验自己 catch 并就地报错，不能被拦截器抢先 reload。
let suppressAuthReload = false
export function setSuppressAuthReload(v: boolean) {
  suppressAuthReload = v
}

// 响应拦截器：鉴权失败(401/403)=密钥失效，清掉本地 key 并回登录页，避免带着废 key 反复 401 死转圈。
// 已登录会话中途 key 失效(如管理员改了 adminkey)→ 干净地 reload 回登录页；
// 登录页的主动校验请求由调用方 setSuppressAuthReload(true) 抑制本处 reload，改为就地报错。
api.interceptors.response.use(
  (res) => res,
  (err) => {
    const status = err?.response?.status
    if ((status === 401 || status === 403) && !suppressAuthReload) {
      storage.removeApiKey()
      if (typeof window !== 'undefined') {
        window.location.reload()
      }
    }
    return Promise.reject(err)
  },
)

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

// 设置凭据别名/备注（传空字符串清除）
export async function setCredentialName(
  id: number,
  name: string | null
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/name`, { name })
  return data
}

// 设置单个凭据代理（立即生效、无需重启）。proxy_url 空清除(回退全局),"direct" 强制不走代理。
// username/password 传 undefined 不改,空串清除。字段名 snake_case 对齐后端 SetProxyRequest。
export async function setCredentialProxy(
  id: number,
  proxyUrl: string | null,
  proxyUsername?: string,
  proxyPassword?: string
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/proxy`, {
    proxy_url: proxyUrl,
    proxy_username: proxyUsername,
    proxy_password: proxyPassword,
  })
  return data
}

// 回收站列表
export async function listTrash(): Promise<TrashListResponse> {
  const { data } = await api.get<TrashListResponse>('/credentials/trash')
  return data
}

// 从回收站恢复单个凭据
export async function restoreCredential(id: number): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/trash/${id}/restore`)
  return data
}

// 永久清除单个回收站条目（不可恢复）
export async function purgeCredential(id: number): Promise<SuccessResponse> {
  const { data } = await api.delete<SuccessResponse>(`/credentials/trash/${id}`)
  return data
}

// 批量清空回收站（ids 为空则清空全部，不可恢复）
export async function purgeTrashBatch(ids?: number[]): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>('/credentials/trash/purge', { ids })
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

// 获取凭据余额（按需，hover 时触发；会向上游拉取，注意勿批量并发以免触发上游风控）
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

// 单号超额（Overage）状态快照（后端 OverageStatus，camelCase）。
export interface OverageStatus {
  id: number
  /** 上游当前的超额开关状态（缺省表示上游未上报该字段） */
  enabled?: boolean | null
  /** 是否具备 profileArn（开启超额的必要条件） */
  hasProfileArn: boolean
  /** 该凭据是否支持 Web Portal（仅网页登录凭据支持） */
  supported: boolean
  /** 状态是否已与目标一致（仅开关操作后返回；缺省为只读查询） */
  confirmed?: boolean
  /** 附加说明（如轮询超时提示），仅在需要时返回 */
  note?: string
}

// 开启单号超额（Overage）——超出 base 额度后按真实用量付费。幂等。
export async function enableOverage(id: number): Promise<OverageStatus> {
  const { data } = await api.post<OverageStatus>(`/credentials/${id}/overage/enable`)
  return data
}

// 关闭单号超额（Overage）。幂等。
export async function disableOverage(id: number): Promise<OverageStatus> {
  const { data } = await api.post<OverageStatus>(`/credentials/${id}/overage/disable`)
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

// ============ 微软 SSO 上号（External IdP · 三步引导）============
// 全程零本机运行：本机不装/不跑任何程序，用户只需在浏览器里复制地址栏 URL 粘回。

// 第 1 步：发起外部 IdP 上号 → 拿 sessionId + Kiro 登录地址
export async function startExternalIdpLogin(
  req: StartExternalIdpLoginRequest
): Promise<StartExternalIdpLoginResponse> {
  const { data } = await api.post<StartExternalIdpLoginResponse>('/auth/external-idp/start', {
    priority: req.priority,
    proxyUrl: req.proxyUrl,
  })
  return data
}

// 第 2 步：粘回登录后地址栏 URL → 拿微软授权地址
export async function submitExternalIdpLeg1(
  sessionId: string,
  url: string
): Promise<ExternalIdpLeg1Response> {
  const { data } = await api.post<ExternalIdpLeg1Response>('/auth/external-idp/leg1', {
    sessionId,
    url,
  })
  return data
}

// 第 3 步：粘回授权后地址栏 URL → 换 token 入池，返回新凭据 id
export async function submitExternalIdpLeg2(
  sessionId: string,
  url: string
): Promise<ExternalIdpLeg2Response> {
  const { data } = await api.post<ExternalIdpLeg2Response>('/auth/external-idp/leg2', {
    sessionId,
    url,
  })
  return data
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
