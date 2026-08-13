import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  CredentialsStatusResponse,
  BalanceResponse,
  CachedBalancesResponse,
  TrashListResponse,
  SuccessResponse,
  BatchDeleteResponse,
  CleanupDisabledResponse,
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
  SocksNodeTest,
  SocksNodesResponse,
  SocksNodeUpsertRequest,
  SocksNodeBulkImportResponse,
  CloneCredentialRequest,
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

// 固定该凭据走的端点（'ide' / 'cli'）；传 null 清除 → 回到自动路由
// （ksk_ API Key 号自动走 cli，其余回退全局 defaultEndpoint）。
export async function setCredentialEndpoint(
  id: number,
  endpoint: string | null
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/endpoint`, {
    endpoint: endpoint && endpoint.trim() ? endpoint.trim() : null,
  })
  return data
}

/**
 * 手动指定该号的上游 region（传 null 清除 → 回退全局默认）。
 *
 * ksk_ API Key 是**按 region 授权**的：打错区上游恒 403（实测同一把 key 在
 * eu-central-1 98.9% 成功、在 us-east-1 100% 403）。自动探测可能探错，所以必须
 * 有手工兜底入口。
 *
 * ⚠️ 此前后端 `POST /credentials/{id}/api-region` 已存在，但前端**零调用** ——
 * 面板上没有任何能改 ksk_ 号 region 的入口（`switchProfileRegion` 对 api_key 号
 * 直接报「仅 External IdP / IdC 凭据支持」）。于是探错的号只能改 credentials.json
 * 手工救。
 */
export async function setCredentialApiRegion(
  id: number,
  apiRegion: string | null
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/api-region`, {
    apiRegion: apiRegion && apiRegion.trim() ? apiRegion.trim() : null,
  })
  return data
}

// 设置代挂凭据的 deepseek 协议归一化开关（仅 custom_api 有意义，后端 gate 拒绝其它类型）。
export async function setCredentialDeepseekNormalize(
  id: number,
  deepseekNormalize: boolean
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/deepseek-normalize`, {
    deepseekNormalize,
  })
  return data
}

// 设置凭据的模型映射豁免开关（跳过全局 model_mapping；Kiro 号与 custom_api 号都可用）。
export async function setCredentialModelMappingExempt(
  id: number,
  modelMappingExempt: boolean
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/model-mapping-exempt`, {
    modelMappingExempt,
  })
  return data
}

// 探测代挂上游的可用模型列表（GET /credentials/{id}/upstream-models，custom_api 专属）。
export async function probeUpstreamModels(id: number): Promise<string[]> {
  const { data } = await api.get<{ models: string[] }>(`/credentials/${id}/upstream-models`)
  return data.models ?? []
}

// 创建前探测代挂上游模型列表（POST /credentials/probe-models，凭据还不存在时的临时探测）。
export async function probeModelsStandalone(req: {
  baseUrl: string
  apiKey?: string
}): Promise<string[]> {
  const { data } = await api.post<{ models: string[] }>('/credentials/probe-models', req)
  return data.models ?? []
}

// 设置凭据别名/备注（传空字符串清除）
export async function setCredentialName(
  id: number,
  name: string | null
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/name`, { name })
  return data
}

// 设置分身标签（传空字符串清除）。与 name 分开：name 是账号别名，tag 描述这一份的用途。
export async function setCredentialTag(
  id: number,
  tag: string | null
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/tag`, { tag })
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

/**
 * 从回收站恢复单个凭据。
 *
 * `force`：跳过 key 重复校验。**多开分身与主凭据必然同 key**，不带这个参数时
 * 删掉的分身永远恢复不了（后端会回「凭据已存在（kiroApiKey 重复），无法恢复」）。
 * 默认 false 保留误操作护栏；恢复后仍是禁用态，故 force 不会让它立刻投入调度。
 */
export async function restoreCredential(
  id: number,
  force = false
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/trash/${id}/restore`,
    { force }
  )
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
  { id: 'claude-opus-5', mult: '2.20x' },
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

// 给**已在池中**的凭据再加 N 份分身。
//
// 为什么不是前端自己拼 addCredential({ kiroApiKey, copies })：凭据列表里只有
// apiKeyHash 与掩码形态，**没有 key 原文**（刻意的，明文 key 不下发前端）。
// 走这个端点让服务端按 id 自己读 key，key 一步都不离开服务端。
//
// 份数语义与 addCredential 的 copies 不同：这里 1 也是有效意图（再加 1 份），
// 服务端会绕过"凭据已存在"去重（那是显式多开意图，不是误双击）。
//
// `enabled`：新分身是否直接启用。**不传 = 后端默认 false（不启用）**。
// 这里刻意用可选参数而不是默认 `false` 常量：省略该键让「默认值」只有服务端一份，
// 前端写死 false 就会在后端改默认时静默分叉。
// `replacePrimary`：建完 N 份后把主份软删进回收站（组内 N 份彼此同质）。省略 = 保留主份。
// 保持可选参数追加而非对象入参：老调用点（如 credential-row 的行视图扩容）不传即行为不变。
export async function cloneCredential(
  id: number,
  copies: number,
  enabled?: boolean,
  replacePrimary?: boolean
): Promise<AddCredentialResponse> {
  const body: CloneCredentialRequest = { copies }
  if (enabled !== undefined) body.enabled = enabled
  if (replacePrimary !== undefined) body.replacePrimary = replacePrimary
  const { data } = await api.post<AddCredentialResponse>(`/credentials/${id}/clone`, body)
  return data
}

/**
 * 重新探测该号上游实际生效的 region 并**写回凭据**（救「自动探测探错」的最后一招）。
 *
 * ⚠️ 后端端点与前端在**并行开发**，按约定契约对接：
 * - 方法/路径：`POST /credentials/{id}/reprobe-region`
 * - 成功：`{ region: string }`（探测到的 region code）
 * - 失败：HTTP 错误 + 标准 `AdminErrorResponse` 错误体（`error.message`）
 *
 * 响应形状已对齐后端（2026-08-11）：
 * - `region: string | null`——null 是**合法成功**（Skipped：已带 region / 非 api_key 号 /
 *   取 token 瞬时失败），不代表失败
 * - `message`——展示文案（region 为空时 toast 用它，用户才知道「探测根本没发生」）
 */
export async function reprobeRegion(id: number): Promise<{ region: string | null; message: string }> {
  const { data } = await api.post<{ region: string | null; message: string }>(`/credentials/${id}/reprobe-region`)
  return data
}

/**
 * 清除全部「该清」的已禁用号（判据在**服务端唯一收口**：排除代挂 / 透传原因 /
 * 自愈中的号 / 超上限，见后端 cleanup_disabled_credentials）。
 *
 * 前端不要再手写判据（2026-08-11 对抗审查 M2：dashboard 曾绕开本端点逐号 DELETE，
 * 会把「自愈中」的健康号软删进回收站）。dryRun=true 只预览候选。
 */
export async function cleanupDisabled(dryRun = false): Promise<CleanupDisabledResponse> {
  const { data } = await api.post<CleanupDisabledResponse>('/credentials/cleanup-disabled', { dryRun })
  return data
}

// 删除凭据
export async function deleteCredential(id: number): Promise<SuccessResponse> {
  const { data } = await api.delete<SuccessResponse>(`/credentials/${id}`)
  return data
}

/**
 * 批量删除凭据。`force=true` 跳过后端「必须先禁用」这道门（仍进回收站，可恢复）。
 *
 * 为什么要它：此前批量删是对每个选中项各发一次 DELETE，且因后端要求先禁用，
 * 实际是 2N 次往返。批量 + force 降到 1 次。
 * 部分失败仍返 200，逐条看 results[].ok —— 调用方须据此提示，不能只看 HTTP 状态。
 */
export async function deleteCredentialsBatch(
  ids: number[],
  force = false,
): Promise<BatchDeleteResponse> {
  const { data } = await api.post<BatchDeleteResponse>('/credentials/batch-delete', { ids, force })
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

// ——— 可复用代理节点（「分身管理」页的候选池）———

export async function listSocksNodes(): Promise<SocksNodesResponse> {
  const { data } = await api.get<SocksNodesResponse>('/socks/nodes')
  return data
}

/** 新建（省略 id）或更新（给 id）一个代理节点。
 *
 * ⚠️ **密码语义**：`req.password` 省略 = 不改；空串 = 清空。
 * 调用方在用户未触碰密码框时必须**不设该键**（不是设 undefined —— 虽然 axios 会
 * 丢掉 undefined 从而恰好正确，但那是依赖序列化细节；这里明确不放该键）。
 * 若图省事回填空串，改个节点名就会把密码抹掉，已绑该节点的分身全部掉线。 */
export async function upsertSocksNode(
  req: SocksNodeUpsertRequest
): Promise<{ id: number; message: string }> {
  const { data } = await api.post<{ id: number; message: string }>('/socks/nodes', req)
  return data
}

/** 整段粘贴批量导入节点。
 *
 * 后端逐行解析：非链接行（标题/分隔线/`端口: 40002`/curl 示例）安静跳过，
 * 按 url 去重且**已存在的跳过而不覆盖**（否则重复导入会把已配好的账密抹掉），
 * `enabled` 省略时默认 false —— 未测活的出口不该直接参与分身分配。
 *
 * 超时单独放宽：一段文档可能带几十个节点，且写盘（persist_socks_nodes）在请求内完成。 */
export async function bulkImportSocksNodes(
  text: string,
  enabled?: boolean
): Promise<SocksNodeBulkImportResponse> {
  const body: Record<string, unknown> = { text }
  if (enabled !== undefined) body.enabled = enabled
  const { data } = await api.post<SocksNodeBulkImportResponse>(
    '/socks/nodes/bulk-import',
    body,
    { timeout: 60000 },
  )
  return data
}

export async function deleteSocksNode(id: number): Promise<{ deleted: boolean }> {
  const { data } = await api.delete<{ deleted: boolean }>(`/socks/nodes/${id}`)
  return data
}

/** 测活并把结果写回节点（复用与 /proxy/test 完全相同的探针路径）。 */
export async function testSocksNode(id: number): Promise<SocksNodeTest> {
  const { data } = await api.post<SocksNodeTest>(`/socks/nodes/${id}/test`)
  return data
}
