import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  restartService,
  getStorageStats,
  cleanupStorage,
  checkUpdate,
  performUpdate,
  getUpdateStatus,
} from '@/api/ops'
import type { StorageCleanupRequest } from '@/types/api'

// 一键重启服务。
// 注意：重启会中断本次连接，请求可能抛错——调用方在 onError 里也当作"已发起"处理更稳妥，
// 但这里保持透明，把成功/失败如实抛给调用方决定文案。
export function useRestartService() {
  return useMutation({
    mutationFn: restartService,
  })
}

// 分区存储统计。手动进入设置页后按需拉取，不做高频轮询（避免频繁 stat 磁盘）。
export function useStorageStats() {
  return useQuery({
    queryKey: ['storage-stats'],
    queryFn: getStorageStats,
  })
}

// 存储清理（不可逆）。成功后刷新统计。
export function useCleanupStorage() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: StorageCleanupRequest) => cleanupStorage(req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['storage-stats'] })
    },
  })
}

// 检查更新（手动触发，不自动轮询——拉 GitHub 有网络代价）。
export function useCheckUpdate() {
  return useMutation({
    mutationFn: checkUpdate,
  })
}

// 一键升级。成功后服务自动重启，请求可能因断连抛错，调用方当"已发起"处理。
export function usePerformUpdate() {
  return useMutation({
    mutationFn: (version?: string) => performUpdate(version),
  })
}

// OTA 升级/回滚状态（只读）。进设置页按需拉取，展示"本版是否稳定确认 / 是否发生过回滚"。
export function useUpdateStatus() {
  return useQuery({
    queryKey: ['update-status'],
    queryFn: getUpdateStatus,
  })
}
