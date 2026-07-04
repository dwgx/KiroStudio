import { useMemo, useState } from 'react'
import {
  Activity,
  CalendarDays,
  CalendarRange,
  Zap,
  Clock,
  TrendingUp,
  Coins,
  type LucideIcon,
} from 'lucide-react'
import { Card } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { StatCard, type StatAccent } from '@/components/ui/stat-card'
import { RadialGauge } from '@/components/overview/RadialGauge'
import { AreaTrendChart } from '@/components/overview/AreaTrendChart'
import {
  useUsageOverview,
  useUsageTimeseries,
  useUsageByModel,
  useUsageByCredential,
  useUsageRecent,
} from '@/hooks/use-usage'
import type {
  WindowSummary,
  SeriesPoint,
  GroupStat,
  RequestRecord,
  RequestOutcome,
} from '@/types/api'

// 大数字紧凑显示：1234 -> 1.2k
function compact(n: number): string {
  if (n < 1000) return n.toLocaleString()
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)}k`
  return `${(n / 1_000_000).toFixed(1)}M`
}

// 成功率(0~100)映射到 StatCard 语义色
function accentForRate(rate: number | null): StatAccent {
  if (rate === null) return 'neutral'
  if (rate >= 90) return 'success'
  if (rate < 70) return 'destructive'
  return 'warning'
}

// 区块小标题（对齐概览页 SectionTitle）
function SectionTitle({ children, hint }: { children: React.ReactNode; hint?: string }) {
  return (
    <div className="mb-4 flex items-baseline justify-between">
      <h3 className="text-sm font-medium text-foreground">{children}</h3>
      {hint && <span className="text-xs text-muted-foreground">{hint}</span>}
    </div>
  )
}

// 趋势图上方的小指标（复刻概览页 MiniMetric）
function MiniMetric({
  icon: Icon,
  label,
  value,
  sub,
}: {
  icon: LucideIcon
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

// 单个时间窗口的 KPI 卡：StatCard 外壳 + 内嵌 RadialGauge(成功率环) + 迷你分项。
// 向概览 KPI 墙审美看齐，取代原朴素文字卡。
function WindowCard({
  label,
  icon,
  w,
}: {
  label: string
  icon: LucideIcon
  w: WindowSummary
}) {
  const hasReq = w.requests > 0
  const rate = hasReq ? Math.round(w.success_rate * 100) : null
  return (
    <StatCard
      label={label}
      icon={icon}
      accent={accentForRate(rate)}
      value={
        <div className="flex items-center gap-3">
          <span>{compact(w.requests)}</span>
          <RadialGauge value={rate} size={44} stroke={6} />
        </div>
      }
      hint={
        <div className="grid w-full grid-cols-2 gap-x-4 gap-y-1 pt-0.5 text-[11px] tabular-nums">
          <span className="flex justify-between">
            <span className="text-muted-foreground">Tokens</span>
            <span className="font-medium text-foreground">{compact(w.total_tokens)}</span>
          </span>
          <span className="flex justify-between">
            <span className="text-muted-foreground">Credits</span>
            <span className="font-medium text-foreground">{w.credits_used.toFixed(2)}</span>
          </span>
          <span className="flex justify-between">
            <span className="text-muted-foreground">In/Out</span>
            <span className="font-medium text-foreground">
              {compact(w.input_tokens)}/{compact(w.output_tokens)}
            </span>
          </span>
          <span className="flex justify-between">
            <span className="text-muted-foreground">均延迟</span>
            <span className="font-medium text-foreground">{Math.round(w.avg_latency_ms)}ms</span>
          </span>
        </div>
      }
    />
  )
}

// 分组榜单卡（按模型 / 按凭据）：沿用概览 RankBars 的设计语言（序号徽标 + 渐变横条 + 挂载宽度动画），
// 但在横条右侧额外并列「成功率」着色数字，一屏同时看清「谁吃了多少量」与「每个的质量」。
// 请求量决定横条长度（相对榜首归一化）；成功率按 ≥90 绿 / <70 红 / 中间黄 分级着色。
function GroupRankList({
  rows,
  labelMap,
}: {
  rows: GroupStat[]
  /** 可选的 key → 展示名映射（如凭据 #id -> 备注） */
  labelMap?: (key: string) => string
}) {
  const sorted = useMemo(
    () => [...rows].sort((a, b) => b.requests - a.requests).slice(0, 8),
    [rows]
  )
  if (sorted.length === 0) {
    return <p className="text-sm text-muted-foreground">暂无数据</p>
  }
  const max = Math.max(1, ...sorted.map((r) => r.requests))
  return (
    <ol className="space-y-3">
      {sorted.map((r, i) => {
        const pct = Math.round((r.requests / max) * 100)
        const rate = r.requests > 0 ? Math.round(r.success_rate * 100) : null
        const rateColor =
          rate === null
            ? 'text-muted-foreground'
            : rate >= 90
              ? 'text-emerald-400'
              : rate < 70
                ? 'text-red-400'
                : 'text-amber-400'
        return (
          <li key={r.key} className="space-y-1.5">
            <div className="flex items-center justify-between gap-2 text-xs">
              <span className="flex min-w-0 items-center gap-2">
                <span
                  className={`flex h-4 w-4 shrink-0 items-center justify-center rounded text-[10px] font-semibold tabular-nums ${
                    i === 0 ? 'bg-primary/15 text-primary' : 'bg-secondary text-muted-foreground'
                  }`}
                >
                  {i + 1}
                </span>
                <span className="truncate text-muted-foreground" title={r.key}>
                  {labelMap ? labelMap(r.key) : r.key}
                </span>
              </span>
              <span className="flex shrink-0 items-baseline gap-3 tabular-nums">
                <span className={`font-medium ${rateColor}`}>{rate === null ? '—' : `${rate}%`}</span>
                <span className="w-14 text-right font-medium text-foreground">
                  {compact(r.requests)}
                </span>
              </span>
            </div>
            <div className="h-2 w-full overflow-hidden rounded-full bg-muted">
              <div
                className="h-full rounded-full transition-[width] duration-700 ease-out-expo motion-reduce:transition-none"
                style={{
                  width: `${pct}%`,
                  background: 'linear-gradient(90deg, hsl(var(--primary)/0.55), hsl(var(--primary)))',
                }}
              />
            </div>
            <div className="flex justify-between text-[10px] text-muted-foreground tabular-nums">
              <span>{compact(r.input_tokens + r.output_tokens)} tokens</span>
              <span>
                {r.credits_used.toFixed(2)} credits · {Math.round(r.avg_latency_ms)}ms
              </span>
            </div>
          </li>
        )
      })}
    </ol>
  )
}

const OUTCOME_LABEL: Record<RequestOutcome, string> = {
  success: '成功',
  rate_limited: '限流',
  auth_failed: '认证失败',
  quota_exhausted: '额度用尽',
  account_suspended: '账户暂停',
  server_error: '服务端错误',
  bad_request: '请求错误',
  network_error: '网络错误',
  other_error: '其它错误',
}

// 结果分类 → tinted badge 语义色：成功绿 / 限流·额度黄 / 其余红
function outcomeVariant(o: RequestOutcome): 'success' | 'warning' | 'destructive' {
  if (o === 'success') return 'success'
  if (o === 'rate_limited' || o === 'quota_exhausted') return 'warning'
  return 'destructive'
}

function OutcomeBadge({ outcome }: { outcome: RequestOutcome }) {
  return (
    <Badge variant={outcomeVariant(outcome)} className="text-[10px]">
      {OUTCOME_LABEL[outcome] ?? outcome}
    </Badge>
  )
}

// 最近请求明细表：保留表格结构，行悬停高亮 + tinted OutcomeBadge，对齐概览行样式
function RecentTable({ rows }: { rows: RequestRecord[] }) {
  if (rows.length === 0) {
    return <p className="text-sm text-muted-foreground">暂无请求记录</p>
  }
  return (
    <div className="overflow-x-auto">
      <table className="w-full text-sm">
        <thead>
          <tr className="border-b border-border text-xs text-muted-foreground">
            <th className="py-2 text-left font-medium">时间</th>
            <th className="py-2 text-left font-medium">模型</th>
            <th className="py-2 text-right font-medium">凭据</th>
            <th className="py-2 text-center font-medium">结果</th>
            <th className="py-2 text-right font-medium">In/Out</th>
            <th className="py-2 text-right font-medium">延迟</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr
              key={r.request_id}
              className="border-b border-border/40 transition-colors last:border-0 hover:bg-secondary/40"
            >
              <td className="py-2 pr-2 tabular-nums text-muted-foreground">
                {new Date(r.ts_ms).toLocaleString()}
              </td>
              <td className="max-w-[160px] truncate py-2 pr-2" title={r.model}>
                {r.model}
              </td>
              <td className="py-2 text-right tabular-nums text-muted-foreground">
                {r.credential_id != null ? `#${r.credential_id}` : '—'}
              </td>
              <td className="py-2 text-center">
                <OutcomeBadge outcome={r.outcome} />
              </td>
              <td className="py-2 text-right tabular-nums">
                {r.input_tokens}/{r.output_tokens}
              </td>
              <td className="py-2 text-right tabular-nums text-muted-foreground">
                {r.latency_ms}ms
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

function Spinner() {
  return (
    <div className="flex items-center justify-center py-24">
      <div className="h-10 w-10 animate-spin rounded-full border-b-2 border-primary" />
    </div>
  )
}

// 统计功能未启用（后端返回 503）时的提示
function StatsDisabled() {
  return (
    <Card className="p-12 text-center text-sm text-muted-foreground">
      用量统计未启用。请在配置中设置 <code className="font-mono">usageEnabled: true</code> 并重启服务。
    </Card>
  )
}

// 时间序列裁剪：后端 hourly 返回 48 桶跨 2 天、前导多为空桶，
// 空桶占去一半宽度会把有数据的段横向挤扁 → 看着“空”。
// 与概览页（slice(-24)）同一治法：先截最近区间，再裁掉前导全 0 桶，让有数据段铺满可视宽度。
function trimSeries(points: SeriesPoint[], granularity: 'hourly' | 'daily'): SeriesPoint[] {
  const windowed = granularity === 'hourly' ? points.slice(-24) : points.slice(-30)
  const firstIdx = windowed.findIndex((p) => p.requests > 0)
  return firstIdx > 0 ? windowed.slice(firstIdx) : windowed
}

export function UsagePage() {
  const [granularity, setGranularity] = useState<'hourly' | 'daily'>('hourly')
  const overview = useUsageOverview()
  const timeseries = useUsageTimeseries(granularity)
  const byModel = useUsageByModel()
  const byCredential = useUsageByCredential()
  const recent = useUsageRecent(100)

  // 裁剪后的趋势数据（去空桶后铺满宽度），据此算峰值/活跃桶/合计小指标
  const chartSeries = useMemo(
    () => trimSeries(timeseries.data ?? [], granularity),
    [timeseries.data, granularity]
  )
  const trendMetrics = useMemo(() => {
    if (chartSeries.length === 0) return null
    let peak = chartSeries[0]
    for (const p of chartSeries) if (p.requests > peak.requests) peak = p
    const totalReq = chartSeries.reduce((a, p) => a + p.requests, 0)
    const totalOk = chartSeries.reduce((a, p) => a + p.success, 0)
    const activeBuckets = chartSeries.filter((p) => p.requests > 0).length
    const unit = granularity === 'hourly' ? '小时' : '天'
    const d = new Date(peak.ts_ms)
    const peakLabel =
      granularity === 'hourly'
        ? `${String(d.getHours()).padStart(2, '0')}:00`
        : `${d.getMonth() + 1}/${d.getDate()}`
    const rate = totalReq > 0 ? Math.round((totalOk / totalReq) * 100) : null
    return { peak, peakLabel, totalReq, totalOk, activeBuckets, unit, rate }
  }, [chartSeries, granularity])

  // 后端未启用统计时返回 503
  const disabled =
    (overview.error as { response?: { status?: number } } | undefined)?.response?.status === 503

  if (overview.isLoading) return <Spinner />
  if (disabled) {
    return (
      <div className="space-y-6">
        <h2 className="text-xl font-semibold">用量统计</h2>
        <StatsDisabled />
      </div>
    )
  }

  return (
    <div className="space-y-6">
      <h2 className="text-xl font-semibold">用量统计</h2>

      {/* 三窗口 KPI 墙（向概览看齐：大数字 + 成功率环 + 分项） */}
      {overview.data && (
        <div className="grid gap-4 md:grid-cols-3">
          <WindowCard label="最近 24 小时" icon={Activity} w={overview.data.last_24h} />
          <WindowCard label="最近 7 天" icon={CalendarDays} w={overview.data.last_7d} />
          <WindowCard label="最近 30 天" icon={CalendarRange} w={overview.data.last_30d} />
        </div>
      )}

      {/* 全宽请求趋势：概览页同款 AreaTrendChart（面积渐变 + 平滑曲线 + 跟随鼠标 tooltip） */}
      <Card className="p-5">
        <div className="mb-4 flex flex-wrap items-center justify-between gap-3">
          <h3 className="text-sm font-medium text-foreground">请求趋势</h3>
          <div className="inline-flex rounded-md border border-border bg-secondary/40 p-0.5 text-xs">
            {(['hourly', 'daily'] as const).map((g) => (
              <button
                key={g}
                onClick={() => setGranularity(g)}
                className={`rounded px-2.5 py-1 font-medium transition-colors ${
                  granularity === g
                    ? 'bg-card text-foreground shadow-sm'
                    : 'text-muted-foreground hover:text-foreground'
                }`}
              >
                {g === 'hourly' ? '按小时' : '按天'}
              </button>
            ))}
          </div>
        </div>

        {timeseries.isLoading ? (
          <div className="flex h-[280px] items-center justify-center">
            <div className="h-8 w-8 animate-spin rounded-full border-b-2 border-primary" />
          </div>
        ) : (
          <>
            {trendMetrics && (
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
                  label="区间成功率"
                  value={trendMetrics.rate === null ? '—' : `${trendMetrics.rate}%`}
                  sub={trendMetrics.rate === null ? undefined : `${compact(trendMetrics.totalOk)} 成功`}
                />
                <MiniMetric
                  icon={Coins}
                  label="区间请求"
                  value={compact(trendMetrics.totalReq)}
                />
              </div>
            )}

            <AreaTrendChart
              points={chartSeries}
              height={280}
              showRate
              granularity={granularity}
            />

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
          </>
        )}
      </Card>

      {/* 分组榜单：按模型 / 按凭据（RankBars 风格横条 + 成功率并列） */}
      <div className="grid gap-4 lg:grid-cols-2">
        <Card className="p-5">
          <SectionTitle hint="按请求量排序">按模型</SectionTitle>
          <GroupRankList rows={byModel.data ?? []} />
        </Card>
        <Card className="p-5">
          <SectionTitle hint="每号请求量 / 成功率对比">按凭据</SectionTitle>
          <GroupRankList
            rows={byCredential.data ?? []}
            labelMap={(k) => (/^\d+$/.test(k) ? `#${k}` : k)}
          />
        </Card>
      </div>

      {/* 最近请求 */}
      <Card className="p-5">
        <SectionTitle hint={`最近 ${recent.data?.length ?? 0} 条`}>最近请求</SectionTitle>
        <RecentTable rows={recent.data ?? []} />
      </Card>
    </div>
  )
}
