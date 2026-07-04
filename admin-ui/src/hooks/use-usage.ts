import { useQuery } from '@tanstack/react-query'
import {
  getUsageOverview,
  getUsageTimeseries,
  getUsageByModel,
  getUsageByCredential,
  getUsageRecent,
} from '@/api/usage'

// 统计页整体每 30 秒自动刷新
const REFETCH_MS = 30000

export function useUsageOverview() {
  return useQuery({
    queryKey: ['usage', 'overview'],
    queryFn: getUsageOverview,
    refetchInterval: REFETCH_MS,
  })
}

export function useUsageTimeseries(granularity: 'hourly' | 'daily') {
  return useQuery({
    queryKey: ['usage', 'timeseries', granularity],
    queryFn: () => getUsageTimeseries(granularity),
    refetchInterval: REFETCH_MS,
  })
}

export function useUsageByModel() {
  return useQuery({
    queryKey: ['usage', 'by-model'],
    queryFn: getUsageByModel,
    refetchInterval: REFETCH_MS,
  })
}

export function useUsageByCredential() {
  return useQuery({
    queryKey: ['usage', 'by-credential'],
    queryFn: getUsageByCredential,
    refetchInterval: REFETCH_MS,
  })
}

export function useUsageRecent(limit = 100) {
  return useQuery({
    queryKey: ['usage', 'recent', limit],
    queryFn: () => getUsageRecent(limit),
    refetchInterval: REFETCH_MS,
  })
}

// 概览健康热力图的“实时请求流动”专用：短轮询 /usage/recent（读本地统计，零上游，无封号风险）。
// 页面隐藏（切走标签页）时暂停轮询省资源；重新可见时 react-query 借由 focus 事件自动复轮。
const LIVE_RECENT_MS = 4000

export function useUsageRecentLive(limit = 60) {
  return useQuery({
    queryKey: ['usage', 'recent-live', limit],
    queryFn: () => getUsageRecent(limit),
    refetchInterval: () => (typeof document !== 'undefined' && document.hidden ? false : LIVE_RECENT_MS),
    refetchIntervalInBackground: false,
  })
}
