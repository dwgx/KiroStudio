import { createAdminApi } from './client'
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

// 与 credentials/ops 同一个实例工厂：baseURL + x-api-key + 鉴权失败清会话。
// 走工厂而非各写一份 axios.create —— 拦截器是每个实例各自一份，漏抄响应拦截器会让
// 本页的 401 静默失败（既不清 sessionStorage 也不回登录页，旧数据看起来"还活着"）。
const api = createAdminApi()

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

