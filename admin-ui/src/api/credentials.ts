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
  ExternalIdpSelectResponse,
  ConfigSnapshotResponse,
  UpdateConfigRequest,
  UpdateConfigResponse,
  CredentialRegionsResponse,
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

// 设置凭据级 RPM 容量上限（0=继承全局）
export async function setCredentialRpmLimit(
  id: number,
  rpmLimit: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/rpm-limit`,
    { rpmLimit: rpmLimit > 0 ? rpmLimit : null }
  )
  return data
}

// 修改自定义 API(代挂透传)凭据的 base_url / api_key / 请求上限。仅 custom_api 号有效。
// 字段可选:undefined=不改;api_key 传空串=清除;requestLimit=0 视为不限。
// resetCount=true 时归零调用次数(换上游/换 key 时避免旧计数残留触顶)。
export interface SetCustomApiConfigInput {
  baseUrl?: string
  apiKey?: string
  requestLimit?: number
  resetCount?: boolean
}
export async function setCredentialCustomApi(
  id: number,
  input: SetCustomApiConfigInput
): Promise<SuccessResponse> {
  const body: Record<string, unknown> = {}
  if (input.baseUrl !== undefined) body.baseUrl = input.baseUrl
  if (input.apiKey !== undefined) body.apiKey = input.apiKey
  if (input.requestLimit !== undefined)
    body.requestLimit = input.requestLimit > 0 ? input.requestLimit : 0
  if (input.resetCount) body.resetCount = true
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/custom-api`, body)
  return data
}

// 设置凭据「允许模型」白名单（成本安全硬门；传空数组/null = 不限制）。
// 值为 kiro modelId（如 ['deepseek-3.2','glm-5']）。设了就是硬门：该号只接白名单内模型，
// 便宜模型的流量被锁在指定号上，绝不溢出到未列该模型的（更贵）号。
export async function setCredentialAllowedModels(
  id: number,
  allowedModels: string[] | null
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/allowed-models`,
    { allowedModels: allowedModels && allowedModels.length ? allowedModels : null }
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

// 探测该凭据各 region 的 profile（切 Profile ARN 用）：列出账号在各区域的 profile，
// 带 usable 标记 + subscription_title。切区域而非改 region（换的是对话走哪个上游 profile/端点）。
// 会向上游探测各区域，可能耗时，单独放宽超时。
export async function probeCredentialRegions(id: number): Promise<CredentialRegionsResponse> {
  const { data } = await api.get<CredentialRegionsResponse>(`/credentials/${id}/regions`, {
    timeout: 120000,
  })
  return data
}

// 切换该凭据当前使用的 Profile ARN（切区域，非改全局 region）。成功后下次请求生效。
export async function switchProfileRegion(id: number, arn: string): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/switch-region`, { arn })
  return data
}

// 探测该凭据可用哪些模型（逐模型发无提示词真实请求，⚠️消耗真实积分）
export interface ProbedModel {
  model: string
  /** supported=可用, unsupported=不支持(INVALID_MODEL_ID/400), unknown=上游5xx/网络无法判定 */
  status: 'supported' | 'unsupported' | 'unknown'
  /** 本模型探测真实消耗的 credits */
  credits: number
}
export interface ProbeModelsResponse {
  id: number
  models: ProbedModel[]
  /** 本次探测总花费 credits */
  totalCredits: number
}
/**
 * 全部可探测的候选模型（真实 Kiro modelId，从便宜到贵；供 UI 勾选）。
 * ⚠️ 须与后端声明式模型目录 src/anthropic/model_catalog.rs::CATALOG 保持一致
 * （id=kiro_id，mult=credit_mult）。补齐 opus-4.5/4.7 消除「广告了却无法探测/加白名单」漂移。
 */
export const PROBE_MODEL_CATALOG: { id: string; mult: string }[] = [
  { id: 'qwen3-coder-next', mult: '0.05x' },
  { id: 'minimax-m2.1', mult: '0.15x' },
  { id: 'deepseek-3.2', mult: '0.25x' },
  { id: 'minimax-m2.5', mult: '0.25x' },
  { id: 'claude-haiku-4.5', mult: '0.40x' },
  { id: 'glm-5', mult: '0.50x' },
  { id: 'auto', mult: '1.00x' },
  // GPT 系(Kiro 2026-07 新增,sol/luna/terra 三并列变体)。倍率暂用 1.00x 占位,待官方权威值校正。
  { id: 'gpt-5.6-sol', mult: '1.00x' },
  { id: 'gpt-5.6-luna', mult: '1.00x' },
  { id: 'gpt-5.6-terra', mult: '1.00x' },
  { id: 'claude-sonnet-4.0', mult: '1.30x' },
  { id: 'claude-sonnet-4.5', mult: '1.30x' },
  { id: 'claude-sonnet-4.6', mult: '1.30x' },
  { id: 'claude-sonnet-5', mult: '1.30x' },
  { id: 'claude-opus-4.5', mult: '2.20x' },
  { id: 'claude-opus-4.6', mult: '2.20x' },
  { id: 'claude-opus-4.7', mult: '2.20x' },
  { id: 'claude-opus-4.8', mult: '2.20x' },
]

export async function probeAvailableModels(id: number, models?: string[]): Promise<ProbeModelsResponse> {
  const q = models && models.length ? `?models=${encodeURIComponent(models.join(','))}` : ''
  // 探测会对每个模型发真实生成请求(可耗时数十秒~数分钟),远超全局 15s 超时。
  // 单独放宽到 5 分钟(后端每模型探测有自己的上游超时兜底,不会真无限挂)。
  const { data } = await api.get<ProbeModelsResponse>(`/credentials/${id}/models${q}`, {
    timeout: 300000,
  })
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
    region: req.region,
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

// 第 3 步：粘回授权后地址栏 URL → 换 token + 探测多 region profile。
// 返回 profiles（多个则弹窗选，1 个则 credentialId 已有值直接完成）。
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

// 第 3 步选定：从多 region profile 里选一个 arn → 用暂存 token 建号入池。
export async function submitExternalIdpLeg2Select(
  sessionId: string,
  arn: string
): Promise<ExternalIdpSelectResponse> {
  const { data } = await api.post<ExternalIdpSelectResponse>('/auth/external-idp/leg2/select', {
    sessionId,
    arn,
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
