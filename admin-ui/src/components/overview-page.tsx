import { useMemo, useState, useEffect, useRef } from 'react'
import { Activity, CheckCircle2, Coins, Database, LayoutGrid, List, Gauge, ShieldCheck, ShieldAlert, TriangleAlert } from 'lucide-react'
import { Card } from '@/components/ui/card'
import { StatCard } from '@/components/ui/stat-card'
import { Skeleton } from '@/components/ui/skeleton'
import { useCredentials, useCachedBalances } from '@/hooks/use-credentials'
import { useUsageOverview, useUsageTimeseries, useUsageRecentLive, useRatelimitInsights } from '@/hooks/use-usage'
import { Sparkline } from '@/components/overview/Sparkline'
import { RadialGauge } from '@/components/overview/RadialGauge'
import { SegmentedBar } from '@/components/overview/SegmentedBar'
import { type CellActivity } from '@/components/overview/StatusHeatmap'
import { RankBars } from '@/components/overview/RankBars'
import { GlowGrid } from '@/components/overview/GlowGrid'
import { StatusBars } from '@/components/overview/StatusBars'
import { AreaTrendChart } from '@/components/overview/AreaTrendChart'
import { authLabel } from '@/lib/i18n-labels'
import type { CredentialStatusItem, SeriesPoint, RateLimitInsight } from '@/types/api'

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

// ============================================================
// 限流健康仪表盘：把「哪些号在被限流 / 撑不住的风险」一眼看清。
// 数据来自 useRatelimitInsights（/ratelimit/insights，只读内存零上游）。
// 去掉了旧版雪花图标，改用中性的量表/盾牌语义图标。
// 核心新增：撑不住预测——按 RPM 占用率 + 近期 429 + 冷却状态推断限流可能性。
// ============================================================

// 单号限流风险等级（撑不住预测的核心判定）。
type RiskLevel = 'ok' | 'watch' | 'high' | 'limited' | 'disabled'

// 由 insight 推断该号的限流风险等级 + 一句中文预测。
function assessRisk(it: RateLimitInsight): { level: RiskLevel; label: string; pct: number } {
  // 已禁用:不参与调度,显示"已禁用"而非"畅通"(修 dwgx 反馈:禁用了还显示畅通/在用)。
  if (it.disabled) {
    return { level: 'disabled', label: '已禁用', pct: 0 }
  }
  // 冷却中（尤其可疑活动风控）= 已经被限流,最高危。
  if (it.cooldown) {
    const isSuspicious = it.cooldown.reason.includes('可疑')
    return {
      level: 'limited',
      label: isSuspicious ? '风控中' : '冷却中',
      pct: 100,
    }
  }
  // RPM 占用率：接近软上限 = 撑不住风险。rpmLimit=0（不限制）时按 0 处理。
  const pct = it.rpmLimit > 0 ? Math.min(100, Math.round((it.rpm / it.rpmLimit) * 100)) : 0
  // 近期 429 是强信号：有 429 说明已经在被上游限,即使当前没冷却。
  if (it.recent429 >= 3 || pct >= 90) return { level: 'high', label: '即将限流', pct: Math.max(pct, 90) }
  if (it.recent429 >= 1 || pct >= 70) return { level: 'watch', label: '压力偏高', pct: Math.max(pct, 70) }
  return { level: 'ok', label: '畅通', pct }
}

const RISK_TONE: Record<RiskLevel, { text: string; bar: string; chip: string }> = {
  ok: { text: 'text-emerald-400', bar: 'bg-emerald-500', chip: 'bg-emerald-500/10 text-emerald-400' },
  watch: { text: 'text-amber-400', bar: 'bg-amber-500', chip: 'bg-amber-500/10 text-amber-400' },
  high: { text: 'text-orange-400', bar: 'bg-orange-500', chip: 'bg-orange-500/10 text-orange-400' },
  limited: { text: 'text-red-400', bar: 'bg-red-500', chip: 'bg-red-500/10 text-red-400' },
  disabled: { text: 'text-muted-foreground/50', bar: 'bg-muted-foreground/30', chip: 'bg-secondary/60 text-muted-foreground/70' },
}

// 限流健康仪表盘组件。creds 用于 id→别名/邮箱 映射;insights 提供限流真数据。
function RateLimitDashboard({
  creds,
  insights,
  loading,
}: {
  creds: CredentialStatusItem[]
  insights: RateLimitInsight[]
  loading: boolean
}) {
  const labelOf = (id: number): string => {
    const c = creds.find((x) => x.id === id)
    return c?.name || c?.email || `#${id}`
  }

  // 号池整体撑不住预测:取各号最高风险 + 被限流号数。
  const summary = useMemo(() => {
    if (insights.length === 0) return { level: 'ok' as RiskLevel, text: '暂无数据', limited: 0 }
    let worst: RiskLevel = 'ok'
    const order: RiskLevel[] = ['ok', 'watch', 'high', 'limited']
    let limited = 0
    let disabledCount = 0
    for (const it of insights) {
      const r = assessRisk(it)
      if (r.level === 'disabled') { disabledCount++; continue } // 禁用号不算进健康度严重性
      if (r.level === 'limited') limited++
      if (order.indexOf(r.level) > order.indexOf(worst)) worst = r.level
    }
    // 健康号 = 总数 - 被限流 - 已禁用(禁用号不算"仍可用")
    const healthy = insights.length - limited - disabledCount
    const text =
      worst === 'limited' && healthy === 0
        ? '号池全部被限流,请加号或降低并发'
        : worst === 'limited'
        ? `${limited} 个号被限流,${healthy} 个仍可用`
        : worst === 'high'
        ? '部分号即将限流,建议关注'
        : worst === 'watch'
        ? '号池压力偏高'
        : '号池畅通'
    return { level: worst, text, limited }
  }, [insights])

  const tone = RISK_TONE[summary.level]
  // 图标语义化(去雪花):畅通=盾牌勾,偏高=量表,即将/被限=警告盾牌/三角。
  const SummaryIcon =
    summary.level === 'ok' ? ShieldCheck : summary.level === 'watch' ? Gauge : summary.level === 'high' ? ShieldAlert : TriangleAlert

  if (loading && insights.length === 0) {
    return (
      <Card className="p-5">
        <Skeleton className="h-4 w-24" />
        <Skeleton className="mt-4 h-8 w-full rounded-lg" />
      </Card>
    )
  }

  // 排序:风险高的排前面(被限流→即将→偏高→畅通),让最该关注的一眼看到。
  const order: RiskLevel[] = ['limited', 'high', 'watch', 'ok', 'disabled']
  const rows = [...insights].sort((a, b) => order.indexOf(assessRisk(a).level) - order.indexOf(assessRisk(b).level))

  return (
    <Card className="p-5">
      <div className="mb-4 flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-2">
          <SummaryIcon className={`h-4 w-4 ${tone.text}`} />
          <h3 className="text-sm font-medium text-foreground">限流健康</h3>
        </div>
        {/* 撑不住预测:整体结论胶囊 */}
        <span className={`inline-flex items-center gap-1.5 rounded-full px-3 py-1 text-xs font-medium ${tone.chip}`}>
          <span className={`h-1.5 w-1.5 rounded-full ${tone.bar}`} />
          {summary.text}
        </span>
      </div>
      {rows.length === 0 ? (
        <div className="flex items-center gap-2 rounded-lg border border-emerald-500/20 bg-emerald-500/5 px-3 py-2.5 text-sm text-emerald-400">
          <ShieldCheck className="h-4 w-4 shrink-0" />
          号池畅通,无限流
        </div>
      ) : (
        <div className="flex flex-col gap-1.5">
          {rows.map((it) => (
            <RateLimitRow key={it.id} it={it} label={labelOf(it.id)} />
          ))}
        </div>
      )}
    </Card>
  )
}

// 单号一行:名称 + 风险胶囊 + RPM占用率迷你条 + 推断浮层(hover)。
function RateLimitRow({ it, label }: { it: RateLimitInsight; label: string }) {
  const risk = assessRisk(it)
  const tone = RISK_TONE[risk.level]
  // 冷却剩余本地按秒显示(粗粒度,数据每 10s 刷)。
  const cdSecs = it.cooldown ? Math.ceil(it.cooldown.remainingMs / 1000) : 0
  return (
    <div
      className="group relative flex items-center gap-3 rounded-lg border border-border bg-secondary/40 px-3 py-2"
      title={it.insightText}
    >
      <span className={`h-2 w-2 shrink-0 rounded-full ${tone.bar}`} />
      <span className="shrink-0 font-mono text-xs tabular-nums text-foreground">#{it.id}</span>
      <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground">{label}</span>
      {/* RPM 占用率迷你条(撑不住可视化):越满越危险 */}
      {it.rpmLimit > 0 && (
        <div className="hidden w-24 shrink-0 items-center gap-1.5 sm:flex" title={`RPM ${it.rpm}/${it.rpmLimit}`}>
          <span className={`w-10 shrink-0 text-right font-mono text-[10px] tabular-nums ${tone.text}`}>
            {it.rpm}/{it.rpmLimit}
          </span>
          <span className="relative h-1.5 flex-1 overflow-hidden rounded-full bg-secondary">
            <span className={`absolute inset-y-0 left-0 rounded-full transition-all ${tone.bar}`} style={{ width: `${risk.pct}%` }} />
          </span>
        </div>
      )}
      {it.recent429 > 0 && (
        <span className="shrink-0 rounded bg-red-500/10 px-1.5 py-0.5 font-mono text-[10px] tabular-nums text-red-400/90" title="近期 429 次数">
          429×{it.recent429}
        </span>
      )}
      {it.inflight > 0 && (
        <span className="shrink-0 rounded bg-primary/15 px-1.5 py-0.5 font-mono text-[10px] tabular-nums text-primary">
          在途 {it.inflight}
        </span>
      )}
      <span className={`inline-flex shrink-0 items-center gap-1 rounded-full px-2 py-0.5 text-[10px] font-medium ${tone.chip}`}>
        {risk.label}
        {cdSecs > 0 && <span className="tabular-nums">{cdSecs}s</span>}
      </span>
    </div>
  )
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
  // 限流健康 insights（10s 轮询，只读内存零上游）。
  const insights = useRatelimitInsights()
  // 火力全开号集合（RPM 饱和）：状态条据此点燃 WebGL 火焰。同时通常仅 1-2 个。
  const saturatedIds = useMemo(
    () => new Set((insights.data ?? []).filter((it) => it.rpmSaturated).map((it) => it.id)),
    [insights.data],
  )

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
    <StatusBars credentials={stats.creds} activity={activity} balances={cachedBalances?.balances} saturatedIds={saturatedIds} />
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

      {/* Row 2.2：限流健康 —— 撑不住预测 + 各号限流风险（去雪花，中性图标） */}
      <RateLimitDashboard creds={stats.creds} insights={insights.data ?? []} loading={insights.isLoading} />

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




