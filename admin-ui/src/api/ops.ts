import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  SuccessResponse,
  StorageStatsResponse,
  StorageCleanupRequest,
  StorageCleanupResponse,
} from '@/types/api'

// 复用与 credentials/usage 相同的 baseURL 与鉴权拦截
const api = axios.create({
  baseURL: '/api/admin',
  // 超时兜底（与 credentials.ts 同值）：axios 默认 timeout=0 即"永远等"。
  // 请求挂在反代那一跳时会一直挂到上游超时，而 React Query 在上一次 fetch
  // 未 settle 前不发下一轮轮询 → 运维页整块静默冻结。
  timeout: 15000,
  headers: { 'Content-Type': 'application/json' },
})

api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) {
    config.headers['x-api-key'] = apiKey
  }
  return config
})

// 一键重启服务（detached，systemctl restart）。
// 注意：重启瞬间本服务断连是预期行为——本次请求可能因连接中断而抛错，调用方需容忍。
// 单独用更短的 5s 超时而非全局 15s：后端已 detached 起了重启，响应能不能回来取决于
// 连接被掐断的时机，等满 15s 只是让"重启中"的按钮多转 10s，对结果毫无影响。
// 调用方（settings-page ServiceManagementCard）的 onError 已把断连当作"已发起"，超时同理。
export async function restartService(): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>('/service/restart', undefined, {
    timeout: 5000,
  })
  return data
}

// 分区存储统计（trace.db / usage jsonl / trash / 背景图内存池）
export async function getStorageStats(): Promise<StorageStatsResponse> {
  const { data } = await api.get<StorageStatsResponse>('/storage/stats')
  return data
}

// 按 target + 可选保留天数清理存储（不可逆）。
// 后端请求体为 camelCase（target / olderThanDays）。
export async function cleanupStorage(
  req: StorageCleanupRequest
): Promise<StorageCleanupResponse> {
  const { data } = await api.post<StorageCleanupResponse>('/storage/cleanup', req)
  return data
}

// OTA 更新检查结果（后端 snake_case，见 admin/update.rs UpdateCheckResult）。
export interface CommitSnapshot {
  sha: string
  title: string
  date: string | null
}

export interface UpdateCheckResult {
  has_update: boolean
  local_version: string
  latest_version: string | null
  available_versions: string[]
  commits: CommitSnapshot[]
  error: string | null
}

export interface UpdatePerformResult {
  success: boolean
  message: string
  updated: boolean
  target_version: string | null
}

// 检查更新（只读，多镜像回退拉 GitHub tags）。
// 单独放宽超时：后端会串行试多个镜像，每个各有自己的超时，合计远超全局 15s；
// 用全局值会把"镜像还在回退中"误判成失败。
export async function checkUpdate(): Promise<UpdateCheckResult> {
  const { data } = await api.get<UpdateCheckResult>('/update/check', { timeout: 60000 })
  return data
}

// OTA 升级/回滚观测（后端 health_marker::HealthStatus，camelCase）。
export interface UpdateStatusResult {
  /** 本版是否已稳定确认（.health 已写）。 */
  healthConfirmed: boolean
  /** .health 原文（version=.. / confirmed_at=..）。 */
  healthDetail: string | null
  /** 回滚点 .bak 是否仍在（健康后应被清；仍在=尚未确认）。 */
  rollbackPointPresent: boolean
  /** 是否有 *.failed.* 残留（守卫脚本执行过回滚的证据）。 */
  rolledBackBinaryPresent: boolean
}

// 读 OTA 升级/回滚状态（只读，读 .health/.bak/*.failed 标记）。
export async function getUpdateStatus(): Promise<UpdateStatusResult> {
  const { data } = await api.get<UpdateStatusResult>('/update/status')
  return data
}

// 一键升级（下载→sha256→替换→重启）。成功后服务会自动重启，请求可能因断连抛错，调用方容忍。
// 单独放宽到 10 分钟：下载二进制（可达 200MiB 上限）+ 校验 + 替换是长操作，
// 套全局 15s 会在升级真正进行中就报超时，误导用户去重复点击。
export async function performUpdate(version?: string): Promise<UpdatePerformResult> {
  const { data } = await api.post<UpdatePerformResult>(
    '/update/perform',
    version ? { version } : {},
    { timeout: 600000 },
  )
  return data
}

// ============ 自愈机器可观测（recovery-metrics）============
// 进程级计数器（自进程启动以来，重启归零）。后端 common/recovery_metrics.rs。
export interface RecoveryMetrics {
  uptimeMs: number
  /** at-rest 加密健康:false=开了加密但上次落盘回退了明文(密钥文件读写失败等,UI 应告警)。 */
  atRestHealthy: boolean
  refreshOk: number
  refreshFail: number
  failoverHops: number
  failoverExhausted: number
  deadTokensDisabled: number
  cooldownTriggered: number
  regionReprobeOk: number
  regionReprobeFail: number
  leakedCleanedRequests: number
  leakedSaturationRequests: number
  /** 文本化工具调用命中 chunk 数(Kiro 把工具调用当文本吐,court/Invalid-tool-params 根源信号)。 */
  textifiedInvokeHits: number
  /** 文本化 invoke 真重组成结构化 tool_use 的次数(捞回生效计数)。 */
  reclaimedInvokeCalls?: number
  /** stray token 复读熔断触发次数(≥32 连写刷屏被截断)。 */
  strayGuardTripped?: number
  /** 【观测】见过"独占 stray 行"的请求数(高置信泄漏)。 */
  strayStandaloneRequests?: number
  /** 【观测】见过"句中紧贴 CJK 的 stray 词"的请求数(clean 层够不到的句中黑洞取证)。 */
  strayInlineRequests?: number
}

export async function getRecoveryMetrics(): Promise<RecoveryMetrics> {
  const { data } = await api.get<RecoveryMetrics>('/recovery-metrics')
  return data
}

// ============ 运维日志（内存环形缓冲）============
export interface LogEntry {
  seq: number
  ts: string
  level: string
  target: string
  message: string
}

// 拉取最近日志（可选增量游标 since + 最低级别 level）。
export async function getLogs(params?: { since?: number; level?: string }): Promise<LogEntry[]> {
  const { data } = await api.get<{ logs: LogEntry[] }>('/logs', { params })
  return data.logs
}

// ============ 请求明细搜索（trace SQLite，服务端过滤 + 分页）============
// 单条请求明细（后端 RequestRecord，snake_case）。
export interface TraceRecord {
  request_id: string
  ts_ms: number
  credential_id: number | null
  model: string
  is_streaming: boolean
  /** gross 口径：**已包含** cache_read_tokens + cache_creation_tokens，算总输入时不得再加 */
  input_tokens: number
  output_tokens: number
  /** 命中缓存复用的输入 token（input_tokens 的子集；网关本地估算，上游不返回真值） */
  cache_read_tokens: number
  /** 写入缓存的输入 token（input_tokens 的子集；上游不提供，当前恒为 0） */
  cache_creation_tokens: number
  credits_used: number | null
  latency_ms: number
  first_token_ms: number | null
  outcome: string
  retries: number
  error_message: string | null
  session_id: string | null
  client_device: string | null
  client_ip: string | null
  client_os: string | null
  client_browser: string | null
}

// 搜索过滤条件（全部可选，空=不过滤）。camelCase 对齐后端 TracesSearchQuery。
export interface TraceSearchFilter {
  model?: string
  credentialId?: number
  clientIp?: string
  sessionId?: string
  outcome?: string
  tsFrom?: number
  tsTo?: number
  text?: string
  isStreaming?: boolean
  limit?: number
  offset?: number
}

export interface TraceSearchResponse {
  items: TraceRecord[]
  total: number
}

// GET /traces/search：服务端过滤 + 分页的请求明细。
export async function searchTraces(filter: TraceSearchFilter): Promise<TraceSearchResponse> {
  const { data } = await api.get<TraceSearchResponse>('/traces/search', { params: filter })
  return data
}

// ============ 代理测活 ============
export interface ProxyTestResult {
  ok: boolean
  latencyMs: number
  exitIp: string | null
  error: string | null
}

// POST /proxy/test：测试代理连通性（后端走该代理请求固定 ipify 探测出口 IP）。
// proxyUrl 传 "direct" 或空串=测直连。username/password 可选（覆盖 URL 内嵌账密）。
export async function testProxy(input: {
  proxyUrl: string
  proxyUsername?: string
  proxyPassword?: string
}): Promise<ProxyTestResult> {
  const { data } = await api.post<ProxyTestResult>('/proxy/test', input)
  return data
}

