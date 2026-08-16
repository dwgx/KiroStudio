import { useMemo } from 'react'
import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import i18n from '@/i18n'
import { Activity, CheckCircle2, Gauge, ShieldCheck, Timer, TrendingUp, Users, HeartPulse } from 'lucide-react'
import { Card } from '@/components/ui/card'
import { StatCard } from '@/components/ui/stat-card'
import { AnimatedNumber } from '@/components/ui/animated-number'
import { SegmentedBar } from '@/components/overview/SegmentedBar'
import { getEndpointHealth, getRecoveryMetrics, getDiagnosticsSnapshot } from '@/api/ops'
import type { CredentialStatusItem, RequestRecord, SeriesPoint, UsageOverview } from '@/types/api'

// 紧凑数字：1234 -> 1.2k（与 overview-page 同实现）
function compact(n: number): string {
  if (n < 1000) return n.toLocaleString()
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)}k`
  return `${(n / 1_000_000).toFixed(1)}M`
}

// 人性化字节数：1536 -> "1.5 KB"（1024 进制）
function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1)
  const v = bytes / Math.pow(1024, i)
  return `${i === 0 ? String(v) : v.toFixed(1)} ${units[i]}`
}

// 友好运行时长（与 ops-page 同实现：复用 opspage.uptime.* 键）
function formatUptime(ms: number): string {
  const s = Math.floor(ms / 1000)
  const d = Math.floor(s / 86400)
  const h = Math.floor((s % 86400) / 3600)
  const m = Math.floor((s % 3600) / 60)
  if (d > 0) return i18n.t('opspage.uptime.days', { d, h })
  if (h > 0) return i18n.t('opspage.uptime.hours', { h, m })
  return i18n.t('opspage.uptime.minutes', { m })
}

// 最近邻分位（nearest-rank）：样本排序后取 50%/90%/99% 位置的值。
function percentilesOf(vals: number[]): { p50: number; p90: number; p99: number } | null {
  if (vals.length === 0) return null
  const s = [...vals].sort((a, b) => a - b)
  const at = (q: number) => s[Math.min(s.length - 1, Math.floor(q * s.length))]
  return { p50: at(0.5), p90: at(0.9), p99: at(0.99) }
}

// 延迟分位条配置：分位越高越靠右、颜色越警（绿→黄→红）。
const PCT_ROWS: { key: 'p50' | 'p90' | 'p99'; pct: number; bar: string; text: string }[] = [
  { key: 'p50', pct: 50, bar: 'bg-emerald-500', text: 'text-emerald-400' },
  { key: 'p90', pct: 90, bar: 'bg-amber-500', text: 'text-amber-400' },
  { key: 'p99', pct: 99, bar: 'bg-red-500', text: 'text-red-400' },
]

/**
 * 性能仪表盘：概览页统计区下方的实时性能视图。
 *
 * 数据全部来自现有只读端点（零上游、零副作用），页面隐藏时随组件卸载停止轮询：
 * - usage/recent（由 overview-page 共享传入，4s 轮询）：平均延迟 / 延迟分位 / 错误分布 / 活跃凭据
 * - usage/overview + usage/timeseries（共享传入，30s）：24h 请求量 / 成功率 / 吞吐
 * - endpoint-health（本组件 10s）：池健康（每凭据×端点 EWMA 成功率）
 * - diagnostics/snapshot（本组件 30s）：进程 uptime / RSS
 * - recovery-metrics（本组件 30s）：自愈吸收层救回计数
 *
 * 显示/隐藏由设置页「外观」分区的开关控制（useUiLayoutPrefs，localStorage，默认显示）。
 */
export function PerfDashboard({
  recent,
  hourly,
  overview,
  creds,
}: {
  recent: RequestRecord[] | undefined
  hourly: SeriesPoint[] | undefined
  overview: UsageOverview | undefined
  creds: CredentialStatusItem[]
}) {
  const { t } = useTranslation()
  const naText = t('common.value.unavailable')

  // 本组件独占的三个查询：池健康 10s、诊断/自愈 30s（页面隐藏时随卸载自动停轮询）。
  const { data: health } = useQuery({
    queryKey: ['endpoint-health'],
    queryFn: getEndpointHealth,
    refetchInterval: 10000,
  })
  const { data: diag } = useQuery({
    queryKey: ['diagnostics-snapshot'],
    queryFn: getDiagnosticsSnapshot,
    refetchInterval: 30000,
  })
  const { data: recov } = useQuery({
    queryKey: ['recovery-metrics'],
    queryFn: getRecoveryMetrics,
    refetchInterval: 30000,
  })

  // ---- 指标卡数据 ----

  // 24h 请求量 / 成功率（与 KPI 行同源：usage/overview）
  const w24 = overview?.last_24h
  const hasReq = !!(w24 && w24.requests > 0)
  const successRate = w24 && w24.requests > 0 ? Math.round(w24.success_rate * 100) : null

  // 平均延迟 + 分位：实时窗口（usage/recent，≤100 条）。latency 缺失（0/undefined）不算样本。
  const latencies = useMemo(
    () => (recent ?? []).filter((r) => Number.isFinite(r.latency_ms) && r.latency_ms > 0).map((r) => r.latency_ms),
    [recent]
  )
  const avgLatency =
    latencies.length > 0 ? Math.round(latencies.reduce((s, v) => s + v, 0) / latencies.length) : null
  const pcts = useMemo(() => percentilesOf(latencies), [latencies])

  // 吞吐：hourly 序列最后一桶的请求数（近似"请求/小时"）。
  const throughput = useMemo(() => {
    const pts = hourly ?? []
    return pts.length > 0 ? pts[pts.length - 1].requests : null
  }, [hourly])

  // 活跃凭据：实时窗口内出现过请求的号（去重）。
  const activeCreds = useMemo(() => {
    const ids = new Set<number>()
    for (const r of recent ?? []) if (r.credential_id != null) ids.add(r.credential_id)
    return ids.size
  }, [recent])

  // 池健康：endpoint-health 里"有样本且成功率 ≥90%"的组合占比。
  // successRate=null 是"尚无样本"而非失败，不计入分母（新号看起来不能像坏号）。
  const poolHealth = useMemo(() => {
    const items = health?.items ?? []
    const sampled = items.filter((it) => it.successRate != null)
    const healthy = sampled.filter((it) => (it.successRate ?? 0) >= 0.9).length
    return { total: items.length, sampled: sampled.length, healthy }
  }, [health])
  const poolRatio = poolHealth.sampled > 0 ? poolHealth.healthy / poolHealth.sampled : null

  // 错误分布：success / rate_limited / 其余失败三类（复用 usage-page 的结果命名）。
  const outcome = useMemo(() => {
    const rows = recent ?? []
    let success = 0
    let rateLimited = 0
    for (const r of rows) {
      if (r.outcome === 'success') success++
      else if (r.outcome === 'rate_limited') rateLimited++
    }
    return { success, rateLimited, failed: rows.length - success - rateLimited, total: rows.length }
  }, [recent])

  // 头部右侧元信息：uptime · RSS（诊断快照 30s 刷新）。
  const meta = useMemo(() => {
    const parts: string[] = []
    if (diag) parts.push(t('perfdash.meta.uptime', { uptime: formatUptime(diag.uptimeMs) }))
    if (diag?.rssBytes != null) parts.push(t('perfdash.meta.rss', { rss: formatBytes(diag.rssBytes) }))
    return parts.join(' · ')
  }, [diag, t])

  // 延迟分位条：以 p99 为满刻度（各分位相对 p99 的比例），p50/p90/p99 三条。
  const pctMax = pcts ? Math.max(pcts.p50, pcts.p90, pcts.p99) : 0

  return (
    <section className="space-y-4" aria-label={t('perfdash.title')}>
      {/* 区块头：渐变标题 + 进程运行信息（uptime · RSS） */}
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-2">
          <Gauge className="h-4 w-4 text-primary" />
          <h3 className="text-sm font-semibold text-gradient-brand">{t('perfdash.title')}</h3>
        </div>
        {meta && <span className="text-xs text-muted-foreground">{meta}</span>}
      </div>

      {/* 指标卡组：6 张（metal-press 语言，与 KPI 行一致） */}
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
        <StatCard
          label={t('perfdash.card.requests.label')}
          value={hasReq ? <AnimatedNumber value={w24!.requests} format={compact} /> : naText}
          icon={Activity}
          accent="primary"
          hint={t('perfdash.card.requests.hint')}
        />
        <StatCard
          label={t('perfdash.card.successRate.label')}
          value={successRate != null ? `${successRate}%` : naText}
          icon={CheckCircle2}
          accent={!hasReq ? 'neutral' : successRate! >= 90 ? 'success' : successRate! < 70 ? 'destructive' : 'warning'}
          hint={
            hasReq
              ? t('overviewpage.kpi.successRate24h.detail', { success: compact(w24!.success), failure: compact(w24!.failure) })
              : undefined
          }
        />
        <StatCard
          label={t('perfdash.card.avgLatency.label')}
          value={avgLatency != null ? `${avgLatency}ms` : naText}
          icon={Timer}
          accent="neutral"
          hint={t('perfdash.card.avgLatency.hint', { n: latencies.length })}
        />
        <StatCard
          label={t('perfdash.card.throughput.label')}
          value={throughput != null ? <AnimatedNumber value={throughput} /> : naText}
          icon={TrendingUp}
          accent="neutral"
          hint={t('perfdash.card.throughput.hint')}
        />
        <StatCard
          label={t('perfdash.card.activeCreds.label')}
          value={<AnimatedNumber value={activeCreds} />}
          icon={Users}
          accent="primary"
          hint={t('perfdash.card.activeCreds.hint', { active: activeCreds, total: creds.length })}
        />
        <StatCard
          label={t('perfdash.card.poolHealth.label')}
          value={poolRatio != null ? `${Math.round(poolRatio * 100)}%` : naText}
          icon={ShieldCheck}
          accent={
            poolRatio == null ? 'neutral' : poolRatio >= 0.9 ? 'success' : poolRatio >= 0.7 ? 'warning' : 'destructive'
          }
          hint={
            poolRatio != null
              ? t('perfdash.card.poolHealth.hint', { healthy: poolHealth.healthy, total: poolHealth.sampled })
              : t('opspage.endpointHealth.emptyDesc')
          }
        />
      </div>

      {/* 下行：左延迟分布 / 右错误分布 */}
      <div className="grid gap-4 lg:grid-cols-2">
        <Card className="p-5">
          <div className="mb-4 flex items-center gap-2">
            <Timer className="h-4 w-4 text-muted-foreground" />
            <h3 className="text-sm font-medium text-foreground">{t('perfdash.latency.title')}</h3>
            <span className="ml-auto text-xs text-muted-foreground">{t('perfdash.card.avgLatency.hint', { n: latencies.length })}</span>
          </div>
          {!pcts ? (
            <p className="text-sm text-muted-foreground">{t('overviewpage.dashboard.summary.noData')}</p>
          ) : (
            <div className="flex flex-col gap-3">
              {PCT_ROWS.map(({ key, pct, bar, text }) => {
                const v = pcts[key]
                return (
                  <div
                    key={key}
                    className="flex items-center gap-3"
                    title={t('perfdash.latency.percentileTitle', { p: pct, pct })}
                  >
                    <span className="w-9 shrink-0 font-mono text-xs tabular-nums text-muted-foreground">p{key.slice(1)}</span>
                    <span className="relative h-1.5 flex-1 overflow-hidden rounded-full bg-secondary">
                      <span
                        className={`absolute inset-y-0 left-0 rounded-full transition-all ${bar}`}
                        style={{ width: `${Math.max(4, Math.round((v / pctMax) * 100))}%` }}
                      />
                    </span>
                    <span className={`w-16 shrink-0 text-right font-mono text-xs tabular-nums ${text}`}>{v}ms</span>
                  </div>
                )
              })}
            </div>
          )}
        </Card>

        <Card className="p-5">
          <div className="mb-4 flex items-center gap-2">
            <Activity className="h-4 w-4 text-muted-foreground" />
            <h3 className="text-sm font-medium text-foreground">{t('perfdash.error.title')}</h3>
          </div>
          {outcome.total === 0 ? (
            <p className="text-sm text-muted-foreground">{t('overviewpage.dashboard.summary.noData')}</p>
          ) : (
            <>
              <SegmentedBar
                segments={[
                  { label: t('usagepage.outcome.success'), value: outcome.success, color: 'hsl(160 84% 45%)' },
                  { label: t('usagepage.outcome.rateLimited'), value: outcome.rateLimited, color: 'hsl(38 92% 55%)' },
                  { label: t('perfdash.error.failed'), value: outcome.failed, color: 'hsl(0 84% 60%)' },
                ]}
              />
              {/* 自愈吸收层救回（recovery-metrics 计数器）：429 风暴时这里能看到吸收层扛了多少 */}
              {recov && (recov.absorbRecovered ?? 0) > 0 && (
                <div className="mt-4 flex items-center gap-1.5 border-t border-border/40 pt-3 text-xs text-muted-foreground">
                  <HeartPulse className="h-3.5 w-3.5 shrink-0 text-emerald-400/80" />
                  {t('perfdash.error.selfHeal', { n: recov.absorbRecovered ?? 0, rounds: recov.absorbRounds ?? 0 })}
                </div>
              )}
            </>
          )}
        </Card>
      </div>
    </section>
  )
}
