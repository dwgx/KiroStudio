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
