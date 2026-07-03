import { useMemo, useState } from 'react'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
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

function pct(rate: number): string {
  return `${Math.round(rate * 100)}%`
}

function rateAccent(rate: number, hasReq: boolean): string {
  if (!hasReq) return ''
  if (rate >= 0.9) return 'text-green-600'
  if (rate < 0.7) return 'text-red-600'
  return 'text-amber-600'
}

// 单个窗口的汇总卡片
function WindowCard({ label, w }: { label: string; w: WindowSummary }) {
  const hasReq = w.requests > 0
  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-sm font-medium text-muted-foreground">{label}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-2">
        <div className="flex items-baseline justify-between">
          <span className="text-2xl font-bold">{compact(w.requests)}</span>
          <span className={`text-sm font-medium ${rateAccent(w.success_rate, hasReq)}`}>
            {hasReq ? `${pct(w.success_rate)} 成功` : '暂无请求'}
          </span>
        </div>
        <dl className="grid grid-cols-2 gap-x-3 gap-y-1 text-xs text-muted-foreground">
          <div className="flex justify-between">
            <dt>输入</dt>
            <dd className="font-medium text-foreground">{compact(w.input_tokens)}</dd>
          </div>
          <div className="flex justify-between">
            <dt>输出</dt>
            <dd className="font-medium text-foreground">{compact(w.output_tokens)}</dd>
          </div>
          <div className="flex justify-between">
            <dt>Credits</dt>
            <dd className="font-medium text-foreground">{w.credits_used.toFixed(2)}</dd>
          </div>
          <div className="flex justify-between">
            <dt>均延迟</dt>
            <dd className="font-medium text-foreground">{Math.round(w.avg_latency_ms)}ms</dd>
          </div>
        </dl>
      </CardContent>
    </Card>
  )
}

// 纯 CSS 柱状时间序列图
function TimeseriesChart({
  points,
  granularity,
}: {
  points: SeriesPoint[]
  granularity: 'hourly' | 'daily'
}) {
  const maxReq = useMemo(
    () => Math.max(1, ...points.map((p) => p.requests)),
    [points]
  )
  const nonEmpty = points.some((p) => p.requests > 0)

  const fmtLabel = (ts: number): string => {
    const d = new Date(ts)
    if (granularity === 'hourly') {
      return `${String(d.getMonth() + 1).padStart(2, '0')}/${String(d.getDate()).padStart(2, '0')} ${String(d.getHours()).padStart(2, '0')}:00`
    }
    return `${String(d.getMonth() + 1).padStart(2, '0')}/${String(d.getDate()).padStart(2, '0')}`
  }

  if (!nonEmpty) {
    return <p className="py-12 text-center text-sm text-muted-foreground">该区间暂无请求数据</p>
  }

  return (
    <div className="space-y-2">
      <div className="flex h-48 items-end gap-[2px]">
        {points.map((p) => {
          const total = Math.max(p.requests, 1)
          const h = (p.requests / maxReq) * 100
          const failH = p.requests > 0 ? (p.failure / total) * h : 0
          const okH = h - failH
          return (
            <div
              key={p.ts_ms}
              className="group relative flex flex-1 flex-col justify-end"
              style={{ minWidth: 0 }}
              title={`${fmtLabel(p.ts_ms)}\n请求 ${p.requests} · 成功 ${p.success} · 失败 ${p.failure}\n均延迟 ${Math.round(p.avg_latency_ms)}ms`}
            >
              {p.failure > 0 && (
                <div className="w-full bg-red-500/80" style={{ height: `${failH}%` }} />
              )}
              <div
                className="w-full rounded-t-sm bg-primary/80 group-hover:bg-primary"
                style={{ height: `${okH}%` }}
              />
            </div>
          )
        })}
      </div>
      <div className="flex justify-between text-[10px] text-muted-foreground">
        <span>{points.length > 0 ? fmtLabel(points[0].ts_ms) : ''}</span>
        <span>{points.length > 0 ? fmtLabel(points[points.length - 1].ts_ms) : ''}</span>
      </div>
      <div className="flex items-center gap-4 text-xs text-muted-foreground">
        <span className="flex items-center gap-1">
          <span className="inline-block h-2.5 w-2.5 rounded-sm bg-primary/80" /> 成功
        </span>
        <span className="flex items-center gap-1">
          <span className="inline-block h-2.5 w-2.5 rounded-sm bg-red-500/80" /> 失败
        </span>
      </div>
    </div>
  )
}

// 分组统计表
function GroupTable({ title, rows }: { title: string; rows: GroupStat[] }) {
  const sorted = useMemo(() => [...rows].sort((a, b) => b.requests - a.requests), [rows])
  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-sm font-medium">{title}</CardTitle>
      </CardHeader>
      <CardContent>
        {sorted.length === 0 ? (
          <p className="text-sm text-muted-foreground">暂无数据</p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b text-xs text-muted-foreground">
                  <th className="py-2 text-left font-medium">键</th>
                  <th className="py-2 text-right font-medium">请求</th>
                  <th className="py-2 text-right font-medium">成功率</th>
                  <th className="py-2 text-right font-medium">Tokens</th>
                  <th className="py-2 text-right font-medium">Credits</th>
                  <th className="py-2 text-right font-medium">均延迟</th>
                </tr>
              </thead>
              <tbody>
                {sorted.map((r) => (
                  <tr key={r.key} className="border-b border-border/50 last:border-0">
                    <td className="max-w-[180px] truncate py-2 pr-2" title={r.key}>
                      {r.key}
                    </td>
                    <td className="py-2 text-right tabular-nums">{compact(r.requests)}</td>
                    <td
                      className={`py-2 text-right tabular-nums ${rateAccent(r.success_rate, r.requests > 0)}`}
                    >
                      {r.requests > 0 ? pct(r.success_rate) : '—'}
                    </td>
                    <td className="py-2 text-right tabular-nums">
                      {compact(r.input_tokens + r.output_tokens)}
                    </td>
                    <td className="py-2 text-right tabular-nums">{r.credits_used.toFixed(2)}</td>
                    <td className="py-2 text-right tabular-nums">
                      {Math.round(r.avg_latency_ms)}ms
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </CardContent>
    </Card>
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

function OutcomeBadge({ outcome }: { outcome: RequestOutcome }) {
  const variant = outcome === 'success' ? 'default' : 'destructive'
  return (
    <Badge variant={variant} className="text-[10px]">
      {OUTCOME_LABEL[outcome] ?? outcome}
    </Badge>
  )
}

// 最近请求明细表
function RecentTable({ rows }: { rows: RequestRecord[] }) {
  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-sm font-medium">最近请求</CardTitle>
      </CardHeader>
      <CardContent>
        {rows.length === 0 ? (
          <p className="text-sm text-muted-foreground">暂无请求记录</p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b text-xs text-muted-foreground">
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
                  <tr key={r.request_id} className="border-b border-border/50 last:border-0">
                    <td className="py-2 pr-2 tabular-nums text-muted-foreground">
                      {new Date(r.ts_ms).toLocaleString()}
                    </td>
                    <td className="max-w-[160px] truncate py-2 pr-2" title={r.model}>
                      {r.model}
                    </td>
                    <td className="py-2 text-right tabular-nums">
                      {r.credential_id != null ? `#${r.credential_id}` : '—'}
                    </td>
                    <td className="py-2 text-center">
                      <OutcomeBadge outcome={r.outcome} />
                    </td>
                    <td className="py-2 text-right tabular-nums">
                      {r.input_tokens}/{r.output_tokens}
                    </td>
                    <td className="py-2 text-right tabular-nums">{r.latency_ms}ms</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </CardContent>
    </Card>
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
    <Card>
      <CardContent className="py-12 text-center text-sm text-muted-foreground">
        用量统计未启用。请在配置中设置 <code className="font-mono">usageEnabled: true</code> 并重启服务。
      </CardContent>
    </Card>
  )
}

export function UsagePage() {
  const [granularity, setGranularity] = useState<'hourly' | 'daily'>('hourly')
  const overview = useUsageOverview()
  const timeseries = useUsageTimeseries(granularity)
  const byModel = useUsageByModel()
  const byCredential = useUsageByCredential()
  const recent = useUsageRecent(100)

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

      {/* 三窗口概览 */}
      {overview.data && (
        <div className="grid gap-4 md:grid-cols-3">
          <WindowCard label="最近 24 小时" w={overview.data.last_24h} />
          <WindowCard label="最近 7 天" w={overview.data.last_7d} />
          <WindowCard label="最近 30 天" w={overview.data.last_30d} />
        </div>
      )}

      {/* 时间序列 */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between pb-2">
          <CardTitle className="text-sm font-medium">请求趋势</CardTitle>
          <div className="flex gap-1">
            <Button
              size="sm"
              variant={granularity === 'hourly' ? 'default' : 'ghost'}
              className="h-7 px-3 text-xs"
              onClick={() => setGranularity('hourly')}
            >
              按小时
            </Button>
            <Button
              size="sm"
              variant={granularity === 'daily' ? 'default' : 'ghost'}
              className="h-7 px-3 text-xs"
              onClick={() => setGranularity('daily')}
            >
              按天
            </Button>
          </div>
        </CardHeader>
        <CardContent>
          {timeseries.isLoading ? (
            <Spinner />
          ) : (
            <TimeseriesChart points={timeseries.data ?? []} granularity={granularity} />
          )}
        </CardContent>
      </Card>

      {/* 分组统计 */}
      <div className="grid gap-4 lg:grid-cols-2">
        <GroupTable title="按模型" rows={byModel.data ?? []} />
        <GroupTable title="按凭据" rows={byCredential.data ?? []} />
      </div>

      {/* 最近请求 */}
      <RecentTable rows={recent.data ?? []} />
    </div>
  )
}
