import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  UsageOverview,
  SeriesPoint,
  GroupStat,
  RequestRecord,
  ClientRpm,
  ThroughputSnapshot,
} from '@/types/api'

// 复用与 credentials 相同的 baseURL 与鉴权拦截
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

// 概览：24h / 7d / 30d
export async function getUsageOverview(): Promise<UsageOverview> {
  const { data } = await api.get<UsageOverview>('/usage/overview')
  return data
}

// 时间序列（小时 / 天）
export async function getUsageTimeseries(
  granularity: 'hourly' | 'daily'
): Promise<SeriesPoint[]> {
  const { data } = await api.get<SeriesPoint[]>('/usage/timeseries', {
    params: { granularity },
  })
  return data
}

// 按模型分组
export async function getUsageByModel(): Promise<GroupStat[]> {
  const { data } = await api.get<GroupStat[]>('/usage/by-model')
  return data
}

// 按凭据分组
export async function getUsageByCredential(): Promise<GroupStat[]> {
  const { data } = await api.get<GroupStat[]>('/usage/by-credential')
  return data
}

// 最近请求明细
export async function getUsageRecent(limit = 100): Promise<RequestRecord[]> {
  const { data } = await api.get<RequestRecord[]>('/usage/recent', {
    params: { limit },
  })
  return data
}

// per 客户端/窗口 RPM（发起方维度：谁开了几个窗口各打多少 RPM）
export async function getUsageClients(): Promise<ClientRpm[]> {
  const { data } = await api.get<ClientRpm[]>('/usage/clients')
  return data
}

// 全局实时吞吐快照（最近 60 秒速率 + 逐秒桶）：读本地内存环，零上游、无封号风险。
// 供趋势图渲染「沿曲线流动的发光粒子」。
export async function getUsageThroughput(): Promise<ThroughputSnapshot> {
  const { data } = await api.get<ThroughputSnapshot>('/usage/throughput')
  return data
}
