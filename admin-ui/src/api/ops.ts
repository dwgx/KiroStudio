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
export async function restartService(): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>('/service/restart')
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
export async function checkUpdate(): Promise<UpdateCheckResult> {
  const { data } = await api.get<UpdateCheckResult>('/update/check')
  return data
}

// 一键升级（下载→sha256→替换→重启）。成功后服务会自动重启，请求可能因断连抛错，调用方容忍。
export async function performUpdate(version?: string): Promise<UpdatePerformResult> {
  const { data } = await api.post<UpdatePerformResult>('/update/perform', version ? { version } : {})
  return data
}
