import { useMemo } from 'react'
import { DatabaseZap } from 'lucide-react'
import { Card } from '@/components/ui/card'
import { Skeleton } from '@/components/ui/skeleton'
import { RadialGauge } from '@/components/overview/RadialGauge'
import { SegmentedBar } from '@/components/overview/SegmentedBar'
import type { CacheStatsSnapshot } from '@/types/api'

// 紧凑数字：1234 -> 1.2k，1234567 -> 1.2M。用于 token 大数展示。
function compact(n: number): string {
  if (n < 1000) return n.toLocaleString()
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)}k`
  if (n < 1_000_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  return `${(n / 1_000_000_000).toFixed(1)}B`
}

/**
 * 缓存命中卡片：展示影子 prompt 缓存记账「确实生效」。
 * 数据来自 useUsageCache（GET /usage/cache，读进程级 static 计数，零上游、无封号风险）。
 *
 * 左：命中率环形仪表（hits/requests）+ 请求/命中计数。
 * 右：读/写分布分段条——
 *   - 命中复用（cacheReadTokens）：从缓存复用、省下重复处理的输入 token；
 *   - 写入缓存（cacheCreationTokens）：为后续命中而写入缓存的输入 token。
 * 底部高亮「省下的输入 token」= cacheReadTokens（这些 token 直接命中缓存，
 * 无需按满价重复处理，是缓存节省的直接量）。
 *
 * 诚实边界：后端缓存记账只累计 token，不换算 credit（credit 单价由上游 meteringEvent
 * 决定、随模型浮动，网关侧无稳定换算率），故这里以「省下的输入 token」为节省口径，
 * 不编造 credit 数字。
 */
export function CacheStatsCard({
  data,
  loading,
}: {
  data: CacheStatsSnapshot | undefined
  loading: boolean
}) {
  // 命中率转百分比（后端 hitRate 为 0~1，requests 为 0 时为 0）。
  const hitPct = useMemo(() => {
    if (!data || data.requests === 0) return null
    return Math.round(data.hitRate * 100)
  }, [data])

  // 读/写 token 分布分段条：命中复用（绿）/ 写入缓存（蓝）。
  const segments = useMemo(() => {
    if (!data) return []
    return [
      { label: '命中复用', value: data.cacheReadTokens, color: 'hsl(160 84% 45%)' },
      { label: '写入缓存', value: data.cacheCreationTokens, color: 'hsl(210 90% 60%)' },
    ]
  }, [data])

  if (loading && !data) {
    return (
      <Card className="p-5">
        <Skeleton className="h-4 w-24" />
        <Skeleton className="mt-4 h-24 w-full rounded-lg" />
      </Card>
    )
  }

  const hasData = !!data && data.requests > 0

  return (
    <Card className="p-5">
      <div className="mb-4 flex items-center gap-2">
        <DatabaseZap className="h-4 w-4 text-emerald-400" />
        <h3 className="text-sm font-medium text-foreground">缓存命中</h3>
        <span className="text-xs text-muted-foreground">前缀缓存复用，省下重复处理的输入 token</span>
      </div>

      {!hasData ? (
        <div className="flex items-center gap-2 rounded-lg border border-border bg-secondary/40 px-3 py-2.5 text-sm text-muted-foreground">
          <DatabaseZap className="h-4 w-4 shrink-0" />
          暂无缓存记账数据（尚无命中缓存的请求）
        </div>
      ) : (
        <div className="grid gap-5 sm:grid-cols-[auto_1fr] sm:items-center">
          {/* 左：命中率环形仪表 + 请求/命中计数 */}
          <div className="flex items-center gap-4">
            <RadialGauge value={hitPct} size={72} stroke={7} />
            <div className="space-y-1">
              <div className="text-xs uppercase tracking-wide text-muted-foreground">命中率</div>
              <div className="font-mono text-xs tabular-nums text-muted-foreground">
                命中 <span className="font-medium text-emerald-400">{compact(data!.hits)}</span>
                {' / '}
                请求 <span className="font-medium text-foreground">{compact(data!.requests)}</span>
              </div>
            </div>
          </div>

          {/* 右：读/写 token 分布 + 省下输入 token 高亮 */}
          <div className="space-y-3">
            <SegmentedBar segments={segments} />
            <div className="flex items-center justify-between rounded-lg border border-emerald-500/20 bg-emerald-500/5 px-3 py-2">
              <span className="text-xs text-muted-foreground">省下的输入 token（缓存复用）</span>
              <span
                className="font-mono text-sm font-semibold tabular-nums text-emerald-400"
                title={`${data!.cacheReadTokens.toLocaleString()} cache_read_input_tokens`}
              >
                {compact(data!.cacheReadTokens)}
              </span>
            </div>
          </div>
        </div>
      )}
    </Card>
  )
}
