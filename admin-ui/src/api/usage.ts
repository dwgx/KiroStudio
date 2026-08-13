import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  UsageOverview,
  SeriesPoint,
  GroupStat,
  RequestRecord,
  ClientRpm,
  MachineRpm,
  ThroughputSnapshot,
  RateLimitInsight,
} from '@/types/api'

// 复用与 credentials 相同的 baseURL 与鉴权拦截
const api = axios.create({
  baseURL: '/api/admin',
  // 超时兜底（与 credentials.ts 同值）：axios 默认 timeout=0 即"永远等"。
  // 请求挂在反代那一跳时会一直挂到上游超时（实测 p90 71s / max 1077s），
  // 而 React Query 在上一次 fetch 未 settle 前不会发下一轮轮询 → 整块面板静默冻结。
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

// 按「上游实际服务模型」分组（映射双口径的 upstream 维度：upstream_model 映射后名，None 回落 model）
export async function getUsageByModel(): Promise<GroupStat[]> {
  const { data } = await api.get<GroupStat[]>('/usage/by-model')
  return data
}

// 按「客户端请求的原始模型名」分组（映射双口径的 requested 维度）
export async function getUsageByRequestedModel(): Promise<GroupStat[]> {
  const { data } = await api.get<GroupStat[]>('/usage/by-requested-model')
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

// 机器维度 RPM（按设备指纹分组，IP 变化不拆分；IP 仅作见过列表）
export async function getUsageMachines(): Promise<MachineRpm[]> {
  const { data } = await api.get<MachineRpm[]>('/usage/machines')
  return data
}

// 限流健康：每号 RPM/软上限/冷却/近期429/中文推断（只读内存零上游）
export async function getRatelimitInsights(): Promise<RateLimitInsight[]> {
  const { data } = await api.get<RateLimitInsight[]>('/ratelimit/insights')
  return data
}

// 全局实时吞吐快照（最近 60 秒速率 + 逐秒桶）：读本地内存环，零上游、无封号风险。
// 供趋势图渲染「沿曲线流动的发光粒子」。
export async function getUsageThroughput(): Promise<ThroughputSnapshot> {
  const { data } = await api.get<ThroughputSnapshot>('/usage/throughput')
  return data
}

