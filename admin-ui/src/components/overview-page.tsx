import { useMemo, useState, useEffect, useRef } from 'react'
import { Activity, CheckCircle2, Coins, Database, LayoutGrid, List } from 'lucide-react'
import { Card } from '@/components/ui/card'
import { StatCard } from '@/components/ui/stat-card'
import { Skeleton } from '@/components/ui/skeleton'
import { useCredentials, useCachedBalances } from '@/hooks/use-credentials'
import { useUsageOverview, useUsageTimeseries, useUsageRecentLive } from '@/hooks/use-usage'
import { Sparkline } from '@/components/overview/Sparkline'
import { RadialGauge } from '@/components/overview/RadialGauge'
import { SegmentedBar } from '@/components/overview/SegmentedBar'
import { type CellActivity } from '@/components/overview/StatusHeatmap'
import { RankBars } from '@/components/overview/RankBars'
import { GlowGrid } from '@/components/overview/GlowGrid'
import { StatusBars } from '@/components/overview/StatusBars'
import { AreaTrendChart } from '@/components/overview/AreaTrendChart'
import { authLabel } from '@/lib/i18n-labels'
import type { CredentialStatusItem, SeriesPoint } from '@/types/api'

// 紧凑数字：1234 -> 1.2k
function compact(n: number): string {
  if (n < 1000) return n.toLocaleString()
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)}k`
  return `${(n / 1_000_000).toFixed(1)}M`
}

// 号池双视图标识；默认发光网格（用户最喜欢的绿点发光）。环形视图已下线。
type PoolView = 'grid' | 'bars'
const POOL_VIEW_KEY = 'kiro.overview.poolView'

// 从 localStorage 读回上次选择的视图（读不到或非法值——含已下线的 orbit——回退发光网格）。
function loadPoolView(): PoolView {
  if (typeof localStorage === 'undefined') return 'grid'
  const v = localStorage.getItem(POOL_VIEW_KEY)
  return v === 'bars' || v === 'grid' ? v : 'grid'
}

// 请求趋势区间：24h 走 hourly 序列，7d/30d 走 daily 序列。
type TrendRange = '24h' | '7d' | '30d'
const TREND_RANGE_KEY = 'kiro.overview.trendRange'

// 从 localStorage 读回上次选择的趋势区间（读不到或非法值回退 24h）。
function loadTrendRange(): TrendRange {
  if (typeof localStorage === 'undefined') return '24h'
  const v = localStorage.getItem(TREND_RANGE_KEY)
  return v === '24h' || v === '7d' || v === '30d' ? v : '24h'
}

// 各区间对应的桶数（hourly 每桶 1 小时，daily 每桶 1 天）。
const TREND_RANGE_META: { key: TrendRange; label: string; buckets: number; granularity: 'hourly' | 'daily' }[] = [
  { key: '24h', label: '24 小时', buckets: 24, granularity: 'hourly' },
  { key: '7d', label: '7 天', buckets: 7, granularity: 'daily' },
  { key: '30d', label: '30 天', buckets: 30, granularity: 'daily' },
]

// 截取最近 N 桶，再裁掉前导全 0 桶让有数据段铺满宽度（与用量页 trimSeries 同治法）。
function trimTrend(points: SeriesPoint[], buckets: number): SeriesPoint[] {
  const windowed = points.slice(-buckets)
  const firstIdx = windowed.findIndex((p) => p.requests > 0)
  return firstIdx > 0 ? windowed.slice(firstIdx) : windowed
}

// 区块小标题
function SectionTitle({ children, hint }: { children: React.ReactNode; hint?: string }) {
  return (
    <div className="mb-4 flex items-baseline justify-between">
      <h3 className="text-sm font-medium text-foreground">{children}</h3>
      {hint && <span className="text-xs text-muted-foreground">{hint}</span>}
    </div>
  )
}

// 号池视图切换（segmented）：发光网格 / 状态条。
const POOL_VIEWS: { key: PoolView; label: string; icon: typeof LayoutGrid }[] = [
  { key: 'grid', label: '发光网格', icon: LayoutGrid },
  { key: 'bars', label: '状态条', icon: List },
]

function PoolViewSwitch({
  value,
  onChange,
}: {
  value: PoolView
  onChange: (v: PoolView) => void
}) {
  return (
    <div className="inline-flex rounded-md border border-border bg-secondary/40 p-0.5 text-xs">
      {POOL_VIEWS.map(({ key, label, icon: Icon }) => (
        <button
          key={key}
          onClick={() => onChange(key)}
          aria-pressed={value === key}
          className={`inline-flex items-center gap-1.5 rounded px-2.5 py-1 font-medium transition-colors ${
            value === key
              ? 'bg-card text-foreground shadow-sm'
              : 'text-muted-foreground hover:text-foreground'
          }`}
        >
          <Icon className="h-3.5 w-3.5" />
          {label}
        </button>
      ))}
    </div>
  )
}

// 趋势区间切换（segmented）：24h / 7d / 30d。
function TrendRangeSwitch({
  value,
  onChange,
}: {
  value: TrendRange
  onChange: (v: TrendRange) => void
}) {
  return (
    <div className="inline-flex rounded-md border border-border bg-secondary/40 p-0.5 text-xs">
      {TREND_RANGE_META.map(({ key, label }) => (
        <button
          key={key}
          onClick={() => onChange(key)}
          aria-pressed={value === key}
          className={`rounded px-2.5 py-1 font-medium transition-colors ${
            value === key
              ? 'bg-card text-foreground shadow-sm'
              : 'text-muted-foreground hover:text-foreground'
          }`}
        >
          {label}
        </button>
      ))}
    </div>
  )
}

// KPI 卡加载态骨架：贴合 card-metal-press 的排版（标签条 / 大数值条 / 辅助条）。
function KpiSkeleton() {
  return (
    <div className="card-metal-press p-5">
      <div className="flex items-start justify-between">
        <div className="space-y-2">
          <Skeleton className="h-3 w-20" />
          <Skeleton className="h-8 w-24" />
        </div>
        <Skeleton className="h-9 w-9 rounded-lg" />
      </div>
      <Skeleton className="mt-3 h-3 w-28" />
    </div>
  )
}

export function OverviewPage() {
  const { data, isLoading: credLoading } = useCredentials()
  // 全页共享一份已缓存余额（只读、零上游），传给状态条视图展示剩余额度迷你条。
  const { data: cachedBalances } = useCachedBalances()
  const overview = useUsageOverview()
  // hourly 供 KPI sparkline + 24h 趋势；daily 供 7d/30d 趋势。两者都是本地统计，无上游封号风险。
  const hourly = useUsageTimeseries('hourly')
  const daily = useUsageTimeseries('daily')
  const recent = useUsageRecentLive(60)

  // 趋势区间切换（24h / 7d / 30d），记忆到 localStorage。
  const [trendRange, setTrendRange] = useState<TrendRange>(loadTrendRange)
  useEffect(() => {
    try {
      localStorage.setItem(TREND_RANGE_KEY, trendRange)
    } catch {
      /* 隐私模式等写入失败可忽略 */
    }
  }, [trendRange])

  // 号池视图选择（记忆到 localStorage）。
  const [poolView, setPoolView] = useState<PoolView>(loadPoolView)
  useEffect(() => {
    try {
      localStorage.setItem(POOL_VIEW_KEY, poolView)
    } catch {
      /* 隐私模式等写入失败可忽略 */
    }
  }, [poolView])

  // 后端未启用用量统计时返回 503：降级隐藏 usage 相关可视化，凭据侧照常。
  const usageDisabled =
    (overview.error as { response?: { status?: number } } | undefined)?.response?.status === 503
  // usage 数据就绪（未禁用且已加载完首帧）；加载中走各自骨架。
  const usageReady = !usageDisabled && !overview.isLoading
  const usageLoading = !usageDisabled && overview.isLoading

  // 请求趋势数据：24h 取 hourly 序列，7d/30d 取 daily 序列，各自截取最近 N 桶并裁掉前导全 0。
  const trend = useMemo(() => {
    const meta = TREND_RANGE_META.find((m) => m.key === trendRange) ?? TREND_RANGE_META[0]
    const src = meta.granularity === 'hourly' ? hourly.data : daily.data
    return { points: trimTrend(src ?? [], meta.buckets), granularity: meta.granularity, label: meta.label }
  }, [trendRange, hourly.data, daily.data])
  // 趋势加载态：跟随当前区间对应的序列查询首帧加载。
  const trendLoading =
    !usageDisabled &&
    ((TREND_RANGE_META.find((m) => m.key === trendRange) ?? TREND_RANGE_META[0]).granularity === 'hourly'
      ? hourly.isLoading
      : daily.isLoading)

  // 凭据派生统计（含空池标记）。
  const stats = useMemo(() => {
    const creds: CredentialStatusItem[] = data?.credentials ?? []
    const total = data?.total ?? creds.length
    const available = data?.available ?? creds.filter((c) => !c.disabled).length
    const disabled = creds.filter((c) => c.disabled).length
    const isEmpty = creds.length === 0

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

    return { total, available, disabled, isEmpty, authSegments, healthSegments, topUsed, creds }
  }, [data])

  // KPI 卡固定展示 24h（sparkline 用 24h 末 24 桶）。
  const w24 = overview.data?.last_24h
  const reqSpark = useMemo(() => (hourly.data ?? []).slice(-24).map((p) => p.requests), [hourly.data])
  const successRate = w24 && w24.requests > 0 ? Math.round(w24.success_rate * 100) : null
  const hasReq = !!(w24 && w24.requests > 0)

  // 实时请求流动：短轮询 /usage/recent（零上游，无封号风险）。
  // 每次数据更新，找出 credential_id 上比上次记录更新的请求 → 该凭据 pulse+1 触发命中脉冲闪动。
  // activityRef 存跨轮询的 lastTs/pulse，activity state 供渲染；三视图共用这份 map。
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

  // 号池主体：加载 / 空 / 三视图。
  const poolBody = credLoading ? (
    // 网格骨架，形状贴近发光网格
    <div
      className="grid gap-2.5"
      style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(34px, 1fr))' }}
    >
      {Array.from({ length: 24 }).map((_, i) => (
        <Skeleton key={i} className="aspect-square rounded-lg" />
      ))}
    </div>
  ) : poolView === 'bars' ? (
    <StatusBars credentials={stats.creds} activity={activity} balances={cachedBalances?.balances} />
  ) : (
    <GlowGrid credentials={stats.creds} activity={activity} />
  )

  return (
    <div className="space-y-6">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl font-semibold text-gradient-brand">概览</h2>
        {usageDisabled && <span className="text-xs text-muted-foreground">用量统计未启用</span>}
      </div>

      {/* Row 1：四张 KPI 卡（凭据侧与用量侧各自独立骨架，互不阻塞） */}
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        {/* ① 凭据总数 + 可用/禁用胶囊 —— 依赖 useCredentials */}
        {credLoading ? (
          <KpiSkeleton />
        ) : (
          <StatCard
            label="凭据总数"
            value={stats.total}
            icon={Database}
            accent="neutral"
            hint={
              stats.isEmpty ? (
                '暂无凭据'
              ) : (
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
              )
            }
          />
        )}

        {/* ② 24h 请求量 + Sparkline —— 依赖 useUsageOverview */}
        {usageLoading ? (
          <KpiSkeleton />
        ) : (
          <StatCard
            label="24h 请求量"
            value={usageReady && hasReq ? compact(w24!.requests) : usageReady ? '0' : '—'}
            icon={Activity}
            accent="primary"
            hint={
              usageDisabled ? (
                '用量统计未启用'
              ) : hasReq && reqSpark.length > 0 ? (
                <div className="w-full pt-1">
                  <Sparkline data={reqSpark} height={28} />
                </div>
              ) : (
                '近 24h 暂无调用'
              )
            }
          />
        )}

        {/* ③ 24h 成功率 + RadialGauge —— 依赖 useUsageOverview */}
        {usageLoading ? (
          <KpiSkeleton />
        ) : (
          <StatCard
            label="24h 成功率"
            value={
              usageReady && hasReq ? (
                <div className="flex items-center gap-3">
                  <span>{successRate}%</span>
                  <RadialGauge value={successRate} size={44} stroke={6} />
                </div>
              ) : (
                '—'
              )
            }
            icon={CheckCircle2}
            accent={
              !hasReq
                ? 'neutral'
                : successRate! >= 90
                ? 'success'
                : successRate! < 70
                ? 'destructive'
                : 'warning'
            }
            hint={
              usageDisabled
                ? '用量统计未启用'
                : hasReq
                ? `成功 ${compact(w24!.success)} · 失败 ${compact(w24!.failure)}`
                : '暂无调用记录'
            }
          />
        )}

        {/* ④ 24h Tokens + Credits/延迟 —— 依赖 useUsageOverview */}
        {usageLoading ? (
          <KpiSkeleton />
        ) : (
          <StatCard
            label="24h Tokens"
            value={usageReady && hasReq ? compact(w24!.total_tokens) : usageReady ? '0' : '—'}
            icon={Coins}
            accent="neutral"
            hint={
              usageDisabled
                ? '用量统计未启用'
                : hasReq
                ? `Credits ${w24!.credits_used.toFixed(2)} · 均延迟 ${Math.round(w24!.avg_latency_ms)}ms`
                : '近 24h 暂无调用'
            }
          />
        )}
      </div>

      {/* Row 2：号池状态（主体）—— 三视图切换，默认发光网格 */}
      <Card className="p-5">
        <div className="mb-4 flex flex-wrap items-center justify-between gap-3">
          <div className="flex items-baseline gap-2">
            <h3 className="text-sm font-medium text-foreground">号池状态</h3>
            {!credLoading && !stats.isEmpty && (
              <span className="text-xs text-muted-foreground">
                可用 {stats.available} / 共 {stats.total}
              </span>
            )}
          </div>
          <PoolViewSwitch value={poolView} onChange={setPoolView} />
        </div>
        {poolBody}
      </Card>

      {/* Row 2.5：请求趋势 —— 面积曲线 + 区间切换（24h/7d/30d），tooltip 跟随鼠标丝滑滑行 */}
      {!usageDisabled && (
        <Card className="p-5">
          <div className="mb-4 flex flex-wrap items-center justify-between gap-3">
            <div className="flex items-baseline gap-2">
              <h3 className="text-sm font-medium text-foreground">请求趋势</h3>
              <span className="text-xs text-muted-foreground">{trend.label}内的请求量与成功率</span>
            </div>
            <TrendRangeSwitch value={trendRange} onChange={setTrendRange} />
          </div>
          {trendLoading ? (
            <Skeleton className="h-[280px] w-full rounded-lg" />
          ) : (
            <AreaTrendChart points={trend.points} granularity={trend.granularity} showRate height={280} />
          )}
        </Card>
      )}

      {/* Row 3：左健康 / 右鉴权 + 榜单 */}
      <div className="grid gap-4 lg:grid-cols-2">
        <Card className="p-5">
          <SectionTitle hint={credLoading ? undefined : `共 ${stats.total} 个`}>
            健康状态
          </SectionTitle>
          {credLoading ? (
            <div className="space-y-3">
              <Skeleton className="h-2.5 w-full rounded-full" />
              <Skeleton className="h-3 w-40" />
            </div>
          ) : (
            <SegmentedBar segments={stats.healthSegments} />
          )}
        </Card>

        <Card className="p-5">
          <SectionTitle>鉴权方式</SectionTitle>
          {credLoading ? (
            <div className="space-y-3">
              <Skeleton className="h-2.5 w-full rounded-full" />
              <Skeleton className="h-3 w-40" />
            </div>
          ) : (
            <SegmentedBar segments={stats.authSegments} className="mb-5" />
          )}
          <SectionTitle hint="按成功调用量">调用量 Top 5</SectionTitle>
          {credLoading ? (
            <div className="space-y-2.5">
              {Array.from({ length: 3 }).map((_, i) => (
                <Skeleton key={i} className="h-2 w-full rounded-full" />
              ))}
            </div>
          ) : (
            <RankBars items={stats.topUsed} unit="次" className="text-muted-foreground" />
          )}
        </Card>
      </div>
    </div>
  )
}




