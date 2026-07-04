import type { CredentialStatusItem } from '@/types/api'
import { Tooltip, TooltipTrigger, TooltipContent, TooltipProvider } from '@/components/ui/tooltip'
import { authLabel, disabledReasonLabel } from '@/lib/i18n-labels'

/** 单个凭据的实时活动信息（由概览页短轮询 /usage/recent 派生）。 */
export interface CellActivity {
  /** 该凭据最近一次请求的 ts_ms（用于 tooltip “最近命中”展示） */
  lastTs: number
  /** 命中脉冲计数：每检测到一次新请求 +1，作为 React key 触发闪动重放 */
  pulse: number
}

export interface StatusHeatmapProps {
  credentials: CredentialStatusItem[]
  /** credential_id -> 实时活动；发现新命中时对应方块闪一下（体现请求流向 / 并发） */
  activity?: Map<number, CellActivity>
  className?: string
}

function fmtAgo(ts: number): string {
  const diff = Date.now() - ts
  if (diff < 0) return '刚刚'
  const s = Math.floor(diff / 1000)
  if (s < 5) return '刚刚'
  if (s < 60) return `${s} 秒前`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m} 分钟前`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h} 小时前`
  return `${Math.floor(h / 24)} 天前`
}

/**
 * 凭据健康热力图（GitHub 贡献图式网格）：每个凭据一个小方块，
 * 绿=健康 / 红=已禁用 / 琥珀=有失败计数但仍启用。
 * 实时：短轮询 /usage/recent，发现请求打到某凭据 → 该方块一次性“命中脉冲”（克制的高光快速衰减），
 * 多个方块近乎同时脉冲即体现并发。isCurrent 用安静的常驻边缘高光标记（去掉张扬的呼吸+扫光）。
 * hover 弹出 tooltip 展示账户免费字段（#id / email / 鉴权 / 成功·失败次数 / 状态 / 最近命中）。
 * 纯 CSS + Radix Tooltip，无图表库；motion-reduce 降级。
 */
export function StatusHeatmap({ credentials, activity, className }: StatusHeatmapProps) {
  if (credentials.length === 0) {
    return <p className={className}>暂无凭据</p>
  }

  const cellClass = (c: CredentialStatusItem): string => {
    if (c.disabled) return 'bg-red-500/80'
    if (c.failureCount > 0) return 'bg-amber-500/80'
    return 'bg-emerald-500/80'
  }

  const statusText = (c: CredentialStatusItem): string => {
    if (c.disabled) return disabledReasonLabel(c.disabledReason) || '已禁用'
    if (c.failureCount > 0) return `启用（失败 ${c.failureCount}）`
    return '健康'
  }

  return (
    <TooltipProvider delayDuration={80}>
      <div className={className}>
        <div
          className="grid gap-1"
          style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(14px, 1fr))' }}
        >
          {credentials.map((c) => {
            const act = activity?.get(c.id)
            return (
              <Tooltip key={c.id}>
                <TooltipTrigger asChild>
                  <div
                    className={`relative aspect-square cursor-pointer overflow-hidden rounded-[3px] transition-transform duration-200 ease-out-expo hover:scale-[1.35] hover:z-10 ${cellClass(c)} ${
                      c.isCurrent
                        ? 'z-10 ring-1 ring-primary/70 ring-offset-1 ring-offset-card animate-idle-glow motion-reduce:animate-none'
                        : ''
                    }`}
                  >
                    {/* 命中脉冲：pulse 计数变化 → key 变化 → 重挂载重放一次快速高光（体现请求流入） */}
                    {act && act.pulse > 0 && (
                      <span
                        key={act.pulse}
                        className="pointer-events-none absolute inset-0 rounded-[3px] bg-white/85 animate-hit-flash motion-reduce:hidden"
                      />
                    )}
                  </div>
                </TooltipTrigger>
                <TooltipContent>
                  <div className="space-y-1">
                    <div className="flex items-center gap-2 font-medium text-foreground">
                      <span>#{c.id}</span>
                      {c.isCurrent && <span className="text-emerald-400">当前活跃</span>}
                    </div>
                    {c.email && <div className="text-muted-foreground">{c.email}</div>}
                    <div className="text-muted-foreground">鉴权：{authLabel(c.authMethod)}</div>
                    <div className="flex gap-3 tabular-nums text-muted-foreground">
                      <span className="text-emerald-400">成功 {c.successCount}</span>
                      <span className={c.failureCount > 0 ? 'text-red-400' : ''}>失败 {c.failureCount}</span>
                    </div>
                    <div className="text-muted-foreground">状态：{statusText(c)}</div>
                    <div className="text-muted-foreground">
                      最近命中：{act ? fmtAgo(act.lastTs) : '近 24h 无'}
                    </div>
                    {/* TODO(BE-balance): 额度/积分待接后端批量缓存端点 cached-balances，
                        本批 hover 仅展示 credentials 免费字段，不在此拉 per-account balance（封号红线）。 */}
                  </div>
                </TooltipContent>
              </Tooltip>
            )
          })}
        </div>
        {/* 图例 */}
        <div className="mt-3 flex flex-wrap gap-x-4 gap-y-1.5 text-xs text-muted-foreground">
          <span className="flex items-center gap-1.5">
            <span className="h-2.5 w-2.5 rounded-[3px] bg-emerald-500/80" /> 健康
          </span>
          <span className="flex items-center gap-1.5">
            <span className="h-2.5 w-2.5 rounded-[3px] bg-amber-500/80" /> 有失败
          </span>
          <span className="flex items-center gap-1.5">
            <span className="h-2.5 w-2.5 rounded-[3px] bg-red-500/80" /> 已禁用
          </span>
          <span className="flex items-center gap-1.5">
            <span className="h-2.5 w-2.5 rounded-[3px] bg-transparent ring-1 ring-primary/70" /> 当前活跃
          </span>
        </div>
      </div>
    </TooltipProvider>
  )
}
