import type { CredentialStatusItem } from '@/types/api'
import type { CellActivity } from '@/components/overview/StatusHeatmap'
import { authLabel, disabledReasonLabel } from '@/lib/i18n-labels'

// ============================================================
// 号池可视化三视图（GlowGrid / OrbitRing / StatusBars）共用的
// 纯逻辑与展示碎片，集中一处避免各视图重复造轮子。
// 只读 credentials 免费字段 + activity，绝不在此拉 balance（避免触发上游风控）。
// ============================================================

/** 凭据健康三态：健康 / 有失败但仍启用 / 已禁用。 */
export type Health = 'healthy' | 'warn' | 'disabled'

/** 由凭据免费字段派生健康态（与 StatusHeatmap 口径一致）。 */
export function healthOf(c: CredentialStatusItem): Health {
  if (c.disabled) return 'disabled'
  if (c.failureCount > 0) return 'warn'
  return 'healthy'
}

/** 健康态对应的基色（rgb 三元组，供 box-shadow / rgba 拼装发光用）。 */
export const HEALTH_RGB: Record<Health, string> = {
  healthy: '16 185 129', // emerald-500
  warn: '245 158 11', // amber-500
  disabled: '127 29 29', // 暗红（red-900，禁用号发暗不发光）
}

/** 健康态中文短标签（图例用）。 */
export const HEALTH_LABEL: Record<Health, string> = {
  healthy: '健康',
  warn: '有失败',
  disabled: '已禁用',
}

/** 相对时间：与 StatusHeatmap 一致的“x 秒/分钟/小时前”。 */
export function fmtAgo(ts: number): string {
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

/** 状态文案：禁用给原因，启用有失败给次数，否则健康。 */
export function statusText(c: CredentialStatusItem): string {
  if (c.disabled) return disabledReasonLabel(c.disabledReason) || '已禁用'
  if (c.failureCount > 0) return `启用（失败 ${c.failureCount}）`
  return '健康'
}

/**
 * 三视图共用的 tooltip 正文（放进 <TooltipContent> 内）。
 * 展示号免费字段：#id / email / 鉴权 / 成功·失败 / 状态 / 最近命中 / 在途。
 * 不含任何 balance / 额度字段（待后端批量缓存端点，避免触发上游风控）。
 */
export function CredTooltipBody({
  c,
  act,
}: {
  c: CredentialStatusItem
  act?: CellActivity
}) {
  return (
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
      <div className="text-muted-foreground">最近命中：{act ? fmtAgo(act.lastTs) : '近 24h 无'}</div>
      {typeof c.inflight === 'number' && c.inflight > 0 && (
        <div className="text-primary">在途：{c.inflight}</div>
      )}
      {/* TODO(BE-balance): 额度/积分待后端批量缓存端点 cached-balances；
          本视图 hover 仅展示免费字段，绝不在此拉 per-account balance（避免触发上游风控）。 */}
    </div>
  )
}

/** 空池优雅占位：三视图共用。 */
export function EmptyPool({ className }: { className?: string }) {
  return (
    <div
      className={`flex min-h-[140px] flex-col items-center justify-center gap-2 rounded-lg border border-dashed border-border/60 text-center ${className ?? ''}`}
    >
      <div className="h-2 w-2 rounded-full bg-muted-foreground/40" />
      <p className="text-sm text-muted-foreground">暂无凭据</p>
      <p className="text-xs text-muted-foreground/60">添加凭据后这里会亮起</p>
    </div>
  )
}
