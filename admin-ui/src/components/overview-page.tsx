import { useMemo, useState, useEffect, useRef } from 'react'
import { Activity, CheckCircle2, Coins, Database, Zap, Clock, TrendingUp } from 'lucide-react'
import { Card } from '@/components/ui/card'
import { StatCard } from '@/components/ui/stat-card'
import { useCredentials } from '@/hooks/use-credentials'
import { useUsageOverview, useUsageTimeseries, useUsageRecentLive } from '@/hooks/use-usage'
import { Sparkline } from '@/components/overview/Sparkline'
import { RadialGauge } from '@/components/overview/RadialGauge'
import { AreaTrendChart } from '@/components/overview/AreaTrendChart'
import { SegmentedBar } from '@/components/overview/SegmentedBar'
import { StatusHeatmap, type CellActivity } from '@/components/overview/StatusHeatmap'
import { RankBars } from '@/components/overview/RankBars'
import { authLabel } from '@/lib/i18n-labels'
import type { CredentialStatusItem, SeriesPoint, WindowSummary } from '@/types/api'

// 紧凑数字：1234 -> 1.2k
function compact(n: number): string {
  if (n < 1000) return n.toLocaleString()
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)}k`
  return `${(n / 1_000_000).toFixed(1)}M`
}

type Win = '24h' | '7d' | '30d'

// 区块小标题
function SectionTitle({ children, hint }: { children: React.ReactNode; hint?: string }) {
  return (
    <div className="mb-4 flex items-baseline justify-between">
      <h3 className="text-sm font-medium text-foreground">{children}</h3>
      {hint && <span className="text-xs text-muted-foreground">{hint}</span>}
    </div>
  )
}

// 趋势图上方的小指标（峰值 / 活跃时段 / 窗口对比）
function MiniMetric({
  icon: Icon,
  label,
  value,
  sub,
}: {
  icon: typeof Zap
  label: string
  value: React.ReactNode
  sub?: string
}) {
  return (
    <div className="flex items-center gap-2.5">
      <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-md bg-secondary text-muted-foreground">
        <Icon className="h-4 w-4" />
      </div>
      <div className="min-w-0">
        <div className="text-[11px] leading-tight text-muted-foreground">{label}</div>
        <div className="flex items-baseline gap-1.5">
          <span className="text-sm font-semibold tabular-nums text-foreground">{value}</span>
          {sub && <span className="text-[11px] text-muted-foreground">{sub}</span>}
        </div>
      </div>
    </div>
  )
}

export function OverviewPage() {
  const { data, isLoading } = useCredentials()
  const overview = useUsageOverview()
  const hourly = useUsageTimeseries('hourly')
  const daily = useUsageTimeseries('daily')
  const recent = useUsageRecentLive(60)

  const [win, setWin] = useState<Win>('24h')

  // 后端未启用用量统计时返回 503：降级隐藏 usage 相关可视化，凭据侧照常
  const usageDisabled =
    (overview.error as { response?: { status?: number } } | undefined)?.response?.status === 503
  const usageReady = !usageDisabled && !overview.isLoading

  // PLACEHOLDER_STATS
  const stats = useMemo(() => {
    const creds: CredentialStatusItem[] = data?.credentials ?? []
    const total = data?.total ?? creds.length
    const available = data?.available ?? creds.filter((c) => !c.disabled).length
    const disabled = creds.filter((c) => c.disabled).length

    // 鉴权方式分布
    const authCounts = new Map<string, number>()
    creds.forEach((c) => {
      const key = authLabel(c.authMethod)
      authCounts.set(key, (authCounts.get(key) || 0) + 1)
    })
    const authColors = ['#0070f3', '#7928ca', '#f5a623', '#8b8b8b', '#50e3c2']
    const authSegments = Array.from(authCounts.entries()).map(([label, value], i) => ({
      label,
      value,
      color: authColors[i % authColors.length],
    }))

    // 健康分布（启用细分为健康 / 有失败）
    const withFailure = creds.filter((c) => !c.disabled && c.failureCount > 0).length
    const healthy = available - withFailure
    const healthSegments = [
      { label: '健康', value: healthy, color: 'hsl(160 84% 45%)' },
      { label: '有失败', value: withFailure, color: 'hsl(38 92% 55%)' },
      { label: '已禁用', value: disabled, color: 'hsl(0 84% 60%)' },
    ]

    // 调用量 Top 5（免费的 successCount，绝不拉 per-account balance）
    const topUsed = [...creds]
      .filter((c) => (c.successCount || 0) > 0)
      .sort((a, b) => (b.successCount || 0) - (a.successCount || 0))
      .slice(0, 5)
      .map((c) => ({
        id: c.id,
        label: `#${c.id} ${c.email || authLabel(c.authMethod)}`,
        value: c.successCount || 0,
      }))

    return { total, available, disabled, authSegments, healthSegments, topUsed, creds }
  }, [data])
  // PLACEHOLDER_SERIES
  const w24 = overview.data?.last_24h
  const w7 = overview.data?.last_7d
  const w30 = overview.data?.last_30d

  // 趋势图数据：24h 视图用 hourly 末尾 24 个桶（后端返回 48 桶跨 2 天，
  // 前导全 0 空桶会把有数据段横向挤扁 —— 取末 24 桶聚焦“最近 24 小时”真实数据段）；
  // 7d/30d 视图用 daily，末尾 7/30 桶。
  const chartSeries = useMemo<SeriesPoint[]>(() => {
    if (win === '24h') return (hourly.data ?? []).slice(-24)
    const d = daily.data ?? []
    return win === '7d' ? d.slice(-7) : d.slice(-30)
  }, [win, hourly.data, daily.data])

  const chartLoading = win === '24h' ? hourly.isLoading : daily.isLoading

  // KPI 卡固定展示 24h（不随趋势窗口切换），sparkline 用 24h 末 24 桶
  const reqSpark = useMemo(() => (hourly.data ?? []).slice(-24).map((p) => p.requests), [hourly.data])
  const successRate = w24 && w24.requests > 0 ? Math.round(w24.success_rate * 100) : null

  // 趋势区小指标：峰值请求/桶、活跃时段、当前窗口汇总
  const trendMetrics = useMemo(() => {
    const s = chartSeries
    if (s.length === 0) return null
    let peak = s[0]
    for (const p of s) if (p.requests > peak.requests) peak = p
    const totalReq = s.reduce((a, p) => a + p.requests, 0)
    const activeBuckets = s.filter((p) => p.requests > 0).length
    const peakLabel = win === '24h'
      ? `${String(new Date(peak.ts_ms).getHours()).padStart(2, '0')}:00`
      : `${new Date(peak.ts_ms).getMonth() + 1}/${new Date(peak.ts_ms).getDate()}`
    const unit = win === '24h' ? '小时' : '天'
    return { peak, peakLabel, totalReq, activeBuckets, unit }
  }, [chartSeries, win])

  // 当前窗口对应的后端汇总（对比用）
  const winSummary: WindowSummary | undefined = win === '24h' ? w24 : win === '7d' ? w7 : w30
  // PLACEHOLDER_ACTIVITY
  // 实时请求流动：短轮询 /usage/recent（零上游，无封号风险）。
  // 每次数据更新，找出 credential_id 上比上次记录更新的请求 → 该凭据 pulse+1 触发命中脉冲闪动。
  // activityRef 存跨轮询的 lastTs/pulse，activity state 供渲染。
  const activityRef = useRef<Map<number, CellActivity>>(new Map())
  const [activity, setActivity] = useState<Map<number, CellActivity>>(new Map())
  const seenMaxTsRef = useRef(0)

  useEffect(() => {
    const rows = recent.data
    if (!rows || rows.length === 0) return
    const map = activityRef.current
    let changed = false
    let batchMaxTs = seenMaxTsRef.current

    for (const r of rows) {
      if (r.credential_id == null) continue
      batchMaxTs = Math.max(batchMaxTs, r.ts_ms)
      const prev = map.get(r.credential_id)
      if (!prev) {
        // 首次见到该号：记录 lastTs，但仅当这条请求比“本组件已见过的全局最大 ts”更新才算“新命中”闪动，
        // 避免首帧把历史全量记录一次性全闪。
        const isNew = r.ts_ms > seenMaxTsRef.current
        map.set(r.credential_id, { lastTs: r.ts_ms, pulse: isNew ? 1 : 0 })
        if (isNew) changed = true
      } else if (r.ts_ms > prev.lastTs) {
        map.set(r.credential_id, { lastTs: r.ts_ms, pulse: prev.pulse + 1 })
        changed = true
      }
    }

    seenMaxTsRef.current = batchMaxTs
    if (changed) setActivity(new Map(map))
  }, [recent.data])

  if (isLoading) {
    return (
      <div className="flex items-center justify-center py-24">
        <div className="h-10 w-10 animate-spin rounded-full border-b-2 border-primary" />
      </div>
    )
  }

  // PLACEHOLDER_RENDER
  return (
    <div className="space-y-6">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl font-semibold">概览</h2>
        {usageDisabled && (
          <span className="text-xs text-muted-foreground">用量统计未启用</span>
        )}
      </div>

      {/* Row 1：四张 KPI 卡 */}
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        {/* KPI_CARDS */}
        {/* ① 凭据总数 + 可用/禁用胶囊 */}
        <StatCard
          label="凭据总数"
          value={stats.total}
          icon={Database}
          accent="neutral"
          hint={
            <div className="flex items-center gap-1.5">
              <span className="inline-flex items-center gap-1 rounded-full bg-emerald-500/10 px-2 py-0.5 text-emerald-400">
                <span className="h-1.5 w-1.5 rounded-full bg-emerald-400" />
                可用 {stats.available}
              </span>
              <span className="inline-flex items-center gap-1 rounded-full bg-red-500/10 px-2 py-0.5 text-red-400">
                <span className="h-1.5 w-1.5 rounded-full bg-red-400" />
                禁用 {stats.disabled}
              </span>
            </div>
          }
        />

        {/* ② 24h 请求量 + Sparkline */}
        <StatCard
          label="24h 请求量"
          value={usageReady && w24 ? compact(w24.requests) : '—'}
          icon={Activity}
          accent="primary"
          hint={
            usageReady && reqSpark.length > 0 ? (
              <div className="w-full pt-1">
                <Sparkline data={reqSpark} height={28} />
              </div>
            ) : usageDisabled ? (
              '用量统计未启用'
            ) : (
              '暂无请求数据'
            )
          }
        />

        {/* ③ 24h 成功率 + RadialGauge */}
        <StatCard
          label="24h 成功率"
          value={
            usageReady ? (
              <div className="flex items-center gap-3">
                <span>{successRate === null ? '—' : `${successRate}%`}</span>
                <RadialGauge value={successRate} size={44} stroke={6} />
              </div>
            ) : (
              '—'
            )
          }
          icon={CheckCircle2}
          accent={
            successRate === null
              ? 'neutral'
              : successRate >= 90
              ? 'success'
              : successRate < 70
              ? 'destructive'
              : 'warning'
          }
          hint={
            usageDisabled
              ? '用量统计未启用'
              : w24 && w24.requests > 0
              ? `成功 ${compact(w24.success)} · 失败 ${compact(w24.failure)}`
              : '暂无调用记录'
          }
        />

        {/* ④ 24h tokens / credits + 均延迟 */}
        <StatCard
          label="24h Tokens"
          value={usageReady && w24 ? compact(w24.total_tokens) : '—'}
          icon={Coins}
          accent="neutral"
          hint={
            usageDisabled
              ? '用量统计未启用'
              : w24
              ? `Credits ${w24.credits_used.toFixed(2)} · 均延迟 ${Math.round(w24.avg_latency_ms)}ms`
              : '暂无数据'
          }
        />
      </div>

      {/* Row 2：请求趋势（小指标条 + 窗口切换 + 双维曲线） */}
      {/* TREND_ROW */}
      {!usageDisabled && (
        <Card className="p-5">
          <div className="mb-4 flex flex-wrap items-center justify-between gap-3">
            <h3 className="text-sm font-medium text-foreground">请求趋势</h3>
            {/* 时间窗口切换（segmented） */}
            <div className="inline-flex rounded-md border border-border bg-secondary/40 p-0.5 text-xs">
              {(['24h', '7d', '30d'] as Win[]).map((k) => (
                <button
                  key={k}
                  onClick={() => setWin(k)}
                  className={`rounded px-2.5 py-1 font-medium transition-colors ${
                    win === k
                      ? 'bg-card text-foreground shadow-sm'
                      : 'text-muted-foreground hover:text-foreground'
                  }`}
                >
                  {k === '24h' ? '24 小时' : k === '7d' ? '7 天' : '30 天'}
                </button>
              ))}
            </div>
          </div>

          {/* 小指标条：用现成 overview / timeseries 数据，不加封号风险调用 */}
          {trendMetrics && winSummary && (
            <div className="mb-5 grid grid-cols-2 gap-4 border-b border-border pb-4 sm:grid-cols-4">
              <MiniMetric
                icon={Zap}
                label={`峰值请求 / ${trendMetrics.unit}`}
                value={compact(trendMetrics.peak.requests)}
                sub={trendMetrics.peakLabel}
              />
              <MiniMetric
                icon={Clock}
                label="活跃时段"
                value={`${trendMetrics.activeBuckets}`}
                sub={`/ ${chartSeries.length} ${trendMetrics.unit}`}
              />
              <MiniMetric
                icon={TrendingUp}
                label="窗口成功率"
                value={winSummary.requests > 0 ? `${Math.round(winSummary.success_rate * 100)}%` : '—'}
                sub={winSummary.requests > 0 ? `${compact(winSummary.success)} 成功` : undefined}
              />
              <MiniMetric
                icon={Coins}
                label="窗口 Credits"
                value={winSummary.credits_used.toFixed(1)}
                sub={`${compact(winSummary.requests)} 请求`}
              />
            </div>
          )}

          {chartLoading ? (
            <div className="flex h-[280px] items-center justify-center">
              <div className="h-8 w-8 animate-spin rounded-full border-b-2 border-primary" />
            </div>
          ) : (
            <AreaTrendChart
              points={chartSeries}
              height={280}
              showRate
              granularity={win === '24h' ? 'hourly' : 'daily'}
            />
          )}

          {/* 图例 */}
          <div className="mt-3 flex flex-wrap gap-x-4 gap-y-1.5 text-xs text-muted-foreground">
            <span className="flex items-center gap-1.5">
              <span className="h-0.5 w-4 rounded bg-primary" /> 请求量
            </span>
            <span className="flex items-center gap-1.5">
              <span className="h-0 w-4 border-t-2 border-dashed border-emerald-500" /> 成功率
            </span>
            <span className="flex items-center gap-1.5">
              <span className="h-1.5 w-1.5 rounded-full bg-red-500/60" /> 有失败
            </span>
          </div>
        </Card>
      )}

      {/* Row 3：左健康 / 右鉴权 + 榜单 */}
      <div className="grid gap-4 lg:grid-cols-2">
        <Card className="p-5">
          <SectionTitle hint={`共 ${stats.total} 个`}>健康状态</SectionTitle>
          <SegmentedBar segments={stats.healthSegments} className="mb-5" />
          <StatusHeatmap credentials={stats.creds} activity={activity} />
        </Card>

        <Card className="p-5">
          <SectionTitle>鉴权方式</SectionTitle>
          <SegmentedBar segments={stats.authSegments} className="mb-5" />
          <SectionTitle hint="按成功调用量">调用量 Top 5</SectionTitle>
          <RankBars items={stats.topUsed} unit="次" />
        </Card>
      </div>
    </div>
  )
}

