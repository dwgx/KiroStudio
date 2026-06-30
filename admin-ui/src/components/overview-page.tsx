import { useMemo } from 'react'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { useCredentials } from '@/hooks/use-credentials'
import type { CredentialStatusItem } from '@/types/api'

// 统计卡片
function StatCard({
  label,
  value,
  hint,
  accent,
}: {
  label: string
  value: React.ReactNode
  hint?: string
  accent?: string
}) {
  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-sm font-medium text-muted-foreground">{label}</CardTitle>
      </CardHeader>
      <CardContent>
        <div className={`text-2xl font-bold ${accent ?? ''}`}>{value}</div>
        {hint && <p className="mt-1 text-xs text-muted-foreground">{hint}</p>}
      </CardContent>
    </Card>
  )
}

// 横向条形分布（纯 CSS，无需图表库）
function DistributionBar({
  title,
  items,
}: {
  title: string
  items: { label: string; value: number; color: string }[]
}) {
  const total = items.reduce((sum, it) => sum + it.value, 0)
  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-sm font-medium">{title}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        {total === 0 ? (
          <p className="text-sm text-muted-foreground">暂无数据</p>
        ) : (
          items.map((it) => {
            const pct = total > 0 ? Math.round((it.value / total) * 100) : 0
            return (
              <div key={it.label} className="space-y-1">
                <div className="flex items-center justify-between text-xs">
                  <span className="text-muted-foreground">{it.label}</span>
                  <span className="font-medium">
                    {it.value} ({pct}%)
                  </span>
                </div>
                <div className="h-2 w-full overflow-hidden rounded-full bg-muted">
                  <div className={`h-full rounded-full ${it.color}`} style={{ width: `${pct}%` }} />
                </div>
              </div>
            )
          })
        )}
      </CardContent>
    </Card>
  )
}

// 鉴权方式中文名
function authLabel(method: string | null): string {
  switch (method) {
    case 'social':
      return 'Social（个人）'
    case 'idc':
      return 'IdC（企业 SSO）'
    case 'api_key':
      return 'API Key'
    default:
      return method || '未知'
  }
}

export function OverviewPage() {
  const { data, isLoading } = useCredentials()

  const stats = useMemo(() => {
    const creds: CredentialStatusItem[] = data?.credentials ?? []
    const total = data?.total ?? creds.length
    const available = data?.available ?? creds.filter((c) => !c.disabled).length
    const disabled = creds.filter((c) => c.disabled).length
    const totalSuccess = creds.reduce((s, c) => s + (c.successCount || 0), 0)
    const totalFailure = creds.reduce((s, c) => s + (c.failureCount || 0), 0)
    const withProxy = creds.filter((c) => c.hasProxy).length

    // 鉴权方式分布
    const authCounts = new Map<string, number>()
    creds.forEach((c) => {
      const key = authLabel(c.authMethod)
      authCounts.set(key, (authCounts.get(key) || 0) + 1)
    })
    const authColors = ['bg-blue-500', 'bg-violet-500', 'bg-amber-500', 'bg-slate-500']
    const authItems = Array.from(authCounts.entries()).map(([label, value], i) => ({
      label,
      value,
      color: authColors[i % authColors.length],
    }))

    // 健康分布
    const healthItems = [
      { label: '可用', value: available, color: 'bg-green-500' },
      { label: '已禁用', value: disabled, color: 'bg-red-500' },
    ]

    // 调用量 Top 5
    const topUsed = [...creds]
      .sort((a, b) => (b.successCount || 0) - (a.successCount || 0))
      .slice(0, 5)
      .filter((c) => (c.successCount || 0) > 0)

    const successRate =
      totalSuccess + totalFailure > 0
        ? Math.round((totalSuccess / (totalSuccess + totalFailure)) * 100)
        : null

    return {
      total,
      available,
      disabled,
      totalSuccess,
      totalFailure,
      withProxy,
      authItems,
      healthItems,
      topUsed,
      successRate,
    }
  }, [data])

  if (isLoading) {
    return (
      <div className="flex items-center justify-center py-24">
        <div className="animate-spin rounded-full h-10 w-10 border-b-2 border-primary" />
      </div>
    )
  }

  const maxUsed = stats.topUsed.length > 0 ? stats.topUsed[0].successCount || 1 : 1

  return (
    <div className="space-y-6">
      <h2 className="text-xl font-semibold">概览</h2>

      {/* 核心指标 */}
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <StatCard label="凭据总数" value={stats.total} />
        <StatCard label="可用凭据" value={stats.available} accent="text-green-600" />
        <StatCard
          label="累计成功调用"
          value={stats.totalSuccess.toLocaleString()}
          hint={`失败 ${stats.totalFailure.toLocaleString()} 次`}
        />
        <StatCard
          label="成功率"
          value={stats.successRate === null ? '—' : `${stats.successRate}%`}
          hint={stats.successRate === null ? '暂无调用记录' : undefined}
          accent={
            stats.successRate !== null && stats.successRate >= 90
              ? 'text-green-600'
              : stats.successRate !== null && stats.successRate < 70
              ? 'text-red-600'
              : ''
          }
        />
      </div>

      {/* 分布图 */}
      <div className="grid gap-4 md:grid-cols-2">
        <DistributionBar title="健康状态分布" items={stats.healthItems} />
        <DistributionBar title="鉴权方式分布" items={stats.authItems} />
      </div>

      {/* 调用量 Top 5 */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-sm font-medium">调用量 Top 5</CardTitle>
        </CardHeader>
        <CardContent>
          {stats.topUsed.length === 0 ? (
            <p className="text-sm text-muted-foreground">暂无调用记录</p>
          ) : (
            <div className="space-y-3">
              {stats.topUsed.map((c) => {
                const pct = Math.round(((c.successCount || 0) / maxUsed) * 100)
                return (
                  <div key={c.id} className="space-y-1">
                    <div className="flex items-center justify-between text-xs">
                      <span className="truncate text-muted-foreground">
                        #{c.id} {c.email || authLabel(c.authMethod)}
                      </span>
                      <span className="font-medium">
                        {(c.successCount || 0).toLocaleString()} 次
                      </span>
                    </div>
                    <div className="h-2 w-full overflow-hidden rounded-full bg-muted">
                      <div className="h-full rounded-full bg-primary" style={{ width: `${pct}%` }} />
                    </div>
                  </div>
                )
              })}
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  )
}
